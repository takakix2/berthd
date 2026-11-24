//! `~/.berth/config.toml`（ここでは `BERTH_CONFIG` で指す）の `default-shell` と `[env]` が、
//! **新しく作る**セッションに効き、台帳の `spawn:started` が `shell_src: client:config` でそのシェルを名乗り、
//! セッションの中で `[env]` の値（先頭の `~` は berth の home に展開）が見える。
//!
//! 読むのは client なので、実物の `berth attach` を pty から撃つ（プロトコル直では通らない経路）。

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{wait_for_daemon, PtyChildGuard};

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

async fn wait_for_bytes(buf: &Arc<Mutex<Vec<u8>>>, needle: &[u8], within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if contains(&buf.lock().unwrap(), needle) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_config_file_names_the_shell_and_adds_env_to_a_new_session() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write as _;

    let scratch = std::env::temp_dir().join(format!("berth-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let config = scratch.join("config.toml");
    // `$SHELL` は bash、設定は sh とし、設定が `$SHELL` に勝つことまで測る。
    std::fs::write(
        &config,
        "default-shell = \"/bin/sh\"\n\n[env]\nBERTH_CFG_PROBE = \"~/probe-dir\"\n",
    )
    .unwrap();
    let journal = scratch.join("journal.jsonl");
    let endpoint = berthd::scratch_endpoint("cfg");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .env("BERTHD_JOURNAL_PATH", &journal)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    drop(wait_for_daemon(&endpoint, Duration::from_secs(5)).await);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.arg("attach");
    cmd.arg("cfg-1");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" || k == "BERTHD_SHELL" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("BERTH_CONFIG", &config);
    // `~` の展開先（berth の home）。
    cmd.env("HOME", &scratch);
    cmd.env("SHELL", "/bin/bash");
    cmd.env("TERM", "xterm-256color");
    let mut keys = pair.master.take_writer().expect("pty writer");
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));
    assert!(
        wait_for_bytes(&buf, b"[attached to cfg-1]", Duration::from_secs(10)).await,
        "陽性の対照: attach のバナーが出ない: {:?}",
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    // 打つ側では割る（echo が自分の目印に当たらないように）。
    keys.write_all(b"echo PROBE=$BERTH_CFG_PROBE; echo CFG_''DONE\n")
        .ok();
    keys.flush().ok();
    assert!(
        wait_for_bytes(&buf, b"CFG_DONE", Duration::from_secs(10)).await,
        "陽性の対照: 打った行が実行されない: {:?}",
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    let text = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    let want = format!("PROBE={}/probe-dir", scratch.display());
    assert!(
        text.contains(&want),
        "🚨 `[env]` の値がセッションに届いていない（~ の展開込み）: want {want:?} in {text:?}"
    );
    drop(terminal);

    // 台帳: spawn:started が設定のシェルを `client:config` で名乗る。
    let lines = std::fs::read_to_string(&journal).unwrap();
    let spawn = lines
        .lines()
        .find(|l| l.contains("\"type\":\"spawn\"") && l.contains("\"event\":\"started\""))
        .unwrap_or_else(|| panic!("spawn:started が台帳に無い: {lines}"));
    assert!(
        spawn.contains("\"shell\":\"/bin/sh\"")
            && spawn.contains("\"shell_src\":\"client:config\""),
        "🚨 台帳のシェルと出所が設定の物ではない: {spawn}"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// `default-shell` が名前だけで、その名前が**デーモンの PATH に無く `berth` の PATH に在る**とき、
/// セッションが起き、台帳の `shell` が `berth` の PATH で引いた絶対パスになる（#72）。
///
/// 再現の形: linger でログイン前に起きた berthd の PATH には `~/.local/bin` が無い。
/// 直す前はデーモンが自分の PATH で探して `cannot start \`hsh\`: not found in PATH` になった。
#[tokio::test]
async fn a_bare_shell_name_is_found_on_the_path_of_the_seat_that_typed_berth() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write as _;

    let scratch = std::env::temp_dir().join(format!("berth-config-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let bin = scratch.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // デーモンの PATH には無い名前（`/bin/sh` を別の名前で置く）。
    let shell = bin.join("berth-bare-sh");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let config = scratch.join("config.toml");
    std::fs::write(&config, "default-shell = \"berth-bare-sh\"\n").unwrap();
    let journal = scratch.join("journal.jsonl");
    let endpoint = berthd::scratch_endpoint("cfgbare");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .env("BERTHD_JOURNAL_PATH", &journal)
        // ログイン前の user manager の PATH を真似る（`bin` が無い）。
        .env("PATH", "/usr/bin:/bin")
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    drop(wait_for_daemon(&endpoint, Duration::from_secs(5)).await);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.arg("attach");
    cmd.arg("cfgbare-1");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" || k == "BERTHD_SHELL" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("BERTH_CONFIG", &config);
    cmd.env("HOME", &scratch);
    let client_path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("PATH", &client_path);
    cmd.env("TERM", "xterm-256color");
    let mut keys = pair.master.take_writer().expect("pty writer");
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));
    assert!(
        wait_for_bytes(&buf, b"[attached to cfgbare-1]", Duration::from_secs(10)).await,
        "🚨 名前だけの default-shell でセッションが起きない（デーモンの PATH で探している？）: {:?}",
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    keys.write_all(b"echo BARE_''DONE\n").ok();
    keys.flush().ok();
    assert!(
        wait_for_bytes(&buf, b"BARE_DONE", Duration::from_secs(10)).await,
        "陽性の対照: 打った行が実行されない: {:?}",
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    drop(terminal);

    let lines = std::fs::read_to_string(&journal).unwrap();
    let spawn = lines
        .lines()
        .find(|l| l.contains("\"type\":\"spawn\"") && l.contains("\"event\":\"started\""))
        .unwrap_or_else(|| panic!("spawn:started が台帳に無い: {lines}"));
    let want = format!("\"shell\":\"{}\"", shell.display());
    assert!(
        spawn.contains(&want) && spawn.contains("\"shell_src\":\"client:config\""),
        "🚨 台帳のシェルが berth の PATH で引いた絶対パスではない（want {want}）: {spawn}"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}
