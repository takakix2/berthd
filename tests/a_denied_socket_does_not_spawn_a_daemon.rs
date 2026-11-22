//! **口は在るのに開かせてもらえないとき、`berth` はもう 1 本 berthd を起こさないか。**
//!
//! # なぜ検体が要るか
//!
//! 起こす側（`ensure_berthd`）が「繋げたか」だけで分岐すると、繋げない理由（走っていない／拒否された／話中）を
//! 見ずに、全部「走っていない」として berthd を起こしに行く。別の資格で走っている berthd の socket が
//! 在ると、起こした 2 本目が bind で落ちるか、二重に走る。
//!
//! ここは**既定の端点**（`$XDG_RUNTIME_DIR/berthd.sock`）を一時ディレクトリに向け、そこに
//! 自分では開けない socket（mode 000、すなわち connect が EACCES）を置き、`BERTHD_BIN` に印を残すだけの偽 berthd
//! を指して、pty の中で `berth attach` を撃つ（tty が無いと `ensure_berthd` の手前で降りる）。
//! 見るのは「印が残ったか」と「拒否を説明して降りたか」。
//! `BERTHD_SOCKET` では測れない。あれが立っていると `ensure_berthd` は最初から起こさない。
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{wait_for_bytes, PtyChildGuard};

#[tokio::test]
async fn a_denied_socket_is_explained_not_replaced() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let dir = std::env::temp_dir().join(format!("berthd-denied-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("berthd.sock");
    // 口は在る（listen している）が、自分には開かせない。
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();

    // 偽の berthd: 起こされたら印を残して終わる。
    let marker = dir.join("berthd-was-started");
    let fake = dir.join("fake-berthd");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.args(["attach", "--no-scrollback", "denied-demo"]);
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" || k == "BERTHD_SOCKET" || k == "BERTHD_NO_SPAWN" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("XDG_RUNTIME_DIR", &dir);
    cmd.env("BERTHD_BIN", &fake);
    cmd.env("BERTHD_NO_SCOPE", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );

    let mut child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    // 降りた（拒否の説明）か、起こしに行った（印）か、どちらかが先に見える。
    let explained = wait_for_bytes(
        &buf,
        b"Reach it from where it was started",
        Duration::from_secs(10),
    )
    .await;
    let status = child.wait_within(Duration::from_secs(5));
    let started = marker.exists();
    drop(child);
    assert_eq!(terminal.close(pair.master), common::PtyClose::Returned);
    let bytes = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !started,
        "🚨 開かせてもらえない socket が在るのに、berthd をもう 1 本起こしに行った\n出力: {bytes}"
    );
    assert!(
        explained,
        "拒否を説明していない（`explain(Denied)` の文言が無い）: {bytes}"
    );
    assert!(
        matches!(status, Some(ref s) if !s.success()),
        "rc≠0 で終わっていない（{status:?}）: {bytes}"
    );
}
