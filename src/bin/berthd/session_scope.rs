//! Each session's shell gets its own systemd scope (Linux).
//!
//! berthd itself is started outside the terminal's cgroup, but every session it starts still lands in
//! berthd's own (`berthd-<pid>.scope` or `berthd.service`). systemd-oomd stops the cgroup under the
//! most pressure as a whole, so one session running a heavy build takes berthd and every other session
//! with it — the one thing berthd exists to keep alive.
//!
//! So right after a session's shell starts, the user manager is asked to move it into a transient
//! scope of its own. Done with `busctl` as a child process — the same way berthd itself is started
//! through `systemd-run` — so berthd takes on no D-Bus dependency.
//!
//! Best effort by design: no user manager, no `busctl`, or a refusal leaves the session in berthd's
//! cgroup, as before, after one log line. Nothing here can make a session fail to start.
//! `BERTHD_NO_SESSION_SCOPE` turns it off.
//!
//! Note: No memory limit on the scope: a limit makes the scope reclaim inside itself, and that stall is
//! counted as pressure on the parent — it gets things stopped sooner, not later.
//!
//! Note: Whatever the shell forks in the moment before the move stays in berthd's cgroup, and a shell
//! that exits at once can be gone before the call lands (the call then fails, harmlessly).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Unique per daemon: pids are reused.
static SEQ: AtomicU64 = AtomicU64::new(1);
/// Say it once per daemon, not once per session.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Which `busctl` to run, or `None` when sessions stay where they are.
fn enabled(opted_out: bool, user_manager_up: bool, busctl: Option<PathBuf>) -> Option<PathBuf> {
    if opted_out || !user_manager_up {
        return None;
    }
    busctl
}

/// `berthd-session-<pid>-<seq>.scope`. Not the session id: a unit name cannot carry every character
/// an id may hold.
fn unit_name(pid: u32, seq: u64) -> String {
    format!("berthd-session-{pid}-{seq}.scope")
}

/// The `busctl` call that starts the scope with `pid` in it.
fn busctl_args(unit: &str, pid: u32, name: &str) -> Vec<OsString> {
    [
        "--user",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        unit,
        "fail",
        // properties: 4 of them
        "4",
        "PIDs",
        "au",
        "1",
        &pid.to_string(),
        "Description",
        "s",
        &format!("berthd session ({name})"),
        // An empty scope and a failed one (oomd stopped it) are both collected.
        "CollectMode",
        "s",
        "inactive-or-failed",
        "Slice",
        "s",
        "app.slice",
        // aux units: none
        "0",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| {
                std::fs::metadata(p)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

fn warn_once(what: &str) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        log_line!("[berthd] session scope: {what} (sessions stay in berthd's cgroup)");
    }
}

/// Moves the session's shell `pid` into a scope of its own, on a thread of its own. Never fails the
/// caller.
pub fn place(pid: u32, name: &str) {
    let Some(busctl) = enabled(
        std::env::var_os("BERTHD_NO_SESSION_SCOPE").is_some(),
        std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|d| std::path::Path::new(&d).join("systemd/private").exists()),
        find_in_path("busctl"),
    ) else {
        return;
    };
    let unit = unit_name(pid, SEQ.fetch_add(1, Ordering::Relaxed));
    let args = busctl_args(&unit, pid, name);
    std::thread::spawn(move || {
        match std::process::Command::new(&busctl)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => warn_once(&format!(
                "{unit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => warn_once(&format!("could not run {}: {e}", busctl.display())),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_move_only_with_a_user_manager_and_busctl() {
        let b = || Some(PathBuf::from("/usr/bin/busctl"));
        assert_eq!(enabled(false, true, b()), b());
        assert_eq!(enabled(true, true, b()), None, "BERTHD_NO_SESSION_SCOPE");
        assert_eq!(enabled(false, false, b()), None, "no user manager");
        assert_eq!(enabled(false, true, None), None, "no busctl");
    }

    #[test]
    fn the_unit_name_is_unique_per_spawn() {
        assert_eq!(unit_name(4242, 7), "berthd-session-4242-7.scope");
        assert_ne!(unit_name(4242, 7), unit_name(4242, 8));
    }

    #[test]
    fn the_call_moves_the_pid_and_collects_failed_scopes() {
        let args: Vec<String> = busctl_args("u.scope", 4242, "main")
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let after = |k: &str| {
            let i = args
                .iter()
                .position(|a| a == k)
                .unwrap_or_else(|| panic!("{k} missing"));
            args[i + 1..].iter().take(3).cloned().collect::<Vec<_>>()
        };
        assert_eq!(after("PIDs")[..3], ["au", "1", "4242"]);
        assert_eq!(after("CollectMode")[..2], ["s", "inactive-or-failed"]);
        assert_eq!(after("Slice")[..2], ["s", "app.slice"]);
        assert_eq!(after("Description")[..2], ["s", "berthd session (main)"]);
        // The count before the properties has to match how many there are.
        let i = args.iter().position(|a| a == "fail").unwrap();
        let named = ["PIDs", "Description", "CollectMode", "Slice"];
        assert_eq!(args[i + 1], named.len().to_string());
        assert_eq!(args.last().map(String::as_str), Some("0"), "no aux units");
    }
}
