//! **Each session's shell ends up in a systemd scope of its own** (Linux).
//!
//! Without it every session shares berthd's cgroup, and systemd-oomd stops all of them (and berthd)
//! for one session's heavy build. `.cargo/config.toml` turns the move off for every other test so
//! they leave no scopes behind; this one turns it back on.
//!
//! Note: Needs a systemd user manager and `busctl`. Without them it says so and returns — **not
//! measured is not green**.
#![cfg(target_os = "linux")]

use berthd::Control;
use std::time::{Duration, Instant};

mod common;
use common::{read_ctrl, send_ctrl, wait_for_daemon};

fn cgroup_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default()
}

#[tokio::test]
async fn a_session_gets_its_own_scope() {
    let manager = std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|d| std::path::Path::new(&d).join("systemd/private").exists());
    let busctl = std::process::Command::new("busctl")
        .arg("--version")
        .output()
        .is_ok();
    if !manager || !busctl {
        eprintln!("⏸ not measured: no systemd user manager ({manager}) or busctl ({busctl})");
        return;
    }
    // The only test in this binary, so nothing else reads the environment meanwhile.
    std::env::remove_var("BERTHD_NO_SESSION_SCOPE");

    let f = common::Fixture::start("scope");
    let daemon = f.daemon_pid().expect("daemon pid");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("scope-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    let pid = match read_ctrl(&mut c).await {
        Control::Spawned { pid, .. } => pid.expect("spawned pid"),
        other => panic!("expected Spawned, got {other:?}"),
    };

    // Control: the daemon itself stays where it was started.
    let daemon_cgroup = cgroup_of(daemon);
    assert!(
        !daemon_cgroup.contains("berthd-session-"),
        "the daemon moved too: {daemon_cgroup}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = cgroup_of(pid);
    while !seen.contains("berthd-session-") && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen = cgroup_of(pid);
    }
    assert!(
        seen.contains(&format!("berthd-session-{pid}-")),
        "the shell (pid {pid}) stayed in {seen:?}; the daemon is in {daemon_cgroup:?}"
    );
}
