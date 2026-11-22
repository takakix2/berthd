//! **外から止められても、`berth` は端末を元に戻してから死ぬか。**
//!
//! # なぜ検体が要るか
//!
//! `berth attach` は端末を raw にし、部屋（alt screen）に入り、中のアプリがマウス報告や kitty キーボードを
//! 点ける。普段の抜け方（`q` / detach / exited）は全部 `leave` を通って `DETACH_RESET` を書き、termios を戻す。
//! 注意: だが SIGTERM / SIGHUP は既定の動作でその場で死ぬ（`leave` は走らず、`RawMode` の `Drop` も走らない）。
//! つまり、何もしなければ `kill <berth>` や ssh の切断の後、端末が raw・alt・マウス報告のまま残る。
//!
//! ここは**自分が端末になって**（pty を張り、その中で `berth attach` を起こす）、attach が成立した後に
//! 外から SIGTERM を撃ち、後始末の列（末尾が detach の信号）が書かれたかを見る。画面は見ない（見るのはバイト）。
//! `DETACH_RESET` を書くのは `leave` だけで、`leave` は termios も戻す（信号が出た ⇔ 後始末が走った）。
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{wait_for_bytes, wait_for_daemon, PtyChildGuard};

async fn attach_in_a_pty_and_signal(sig: libc::c_int, tag: &str) -> (bool, bool) {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let endpoint = berthd::scratch_endpoint(tag);
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
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
    // 部屋の形を名指しする（既定はスクロールへ流す形で、tmux の中では自動で外れるため環境で結果が割れる）。
    cmd.arg("--no-scrollback");
    cmd.arg(tag);
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 利用者の本物の `~/.berth/config.toml` は読まない。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );

    let child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    // 陽性対照（撃つ前に、attach が成立したことを見る）。
    let attached = wait_for_bytes(
        &buf,
        format!("\x1b]2746;attach;{tag}\x1b\\").as_bytes(),
        Duration::from_secs(10),
    )
    .await;

    let pid = child.pid().expect("berth pid") as libc::pid_t;
    // SAFETY: 自分が起こした子へ signal を送るだけ。
    unsafe {
        libc::kill(pid, sig);
    }
    // 後始末の列は detach の信号で**終わる**。それが出れば `leave` が最後まで走った。
    let cleaned = wait_for_bytes(&buf, b"\x1b]2746;detach\x1b\\", Duration::from_secs(10)).await;

    drop(child);
    assert_eq!(
        terminal.close(pair.master),
        common::PtyClose::Returned,
        "pty を閉じられず期限へ落ちた"
    );
    (attached, cleaned)
}

#[tokio::test]
async fn sigterm_still_puts_the_terminal_back() {
    let (attached, cleaned) = attach_in_a_pty_and_signal(libc::SIGTERM, "sigterm-demo").await;
    assert!(attached, "attach が成立していない ＝ 何も測れていない");
    assert!(
        cleaned,
        "🚨 SIGTERM で死んだ後に後始末の列（detach の信号）が出ていない ＝ 端末が raw / alt / マウス報告のまま残る"
    );
}

#[tokio::test]
async fn sighup_still_puts_the_terminal_back() {
    let (attached, cleaned) = attach_in_a_pty_and_signal(libc::SIGHUP, "sighup-demo").await;
    assert!(attached, "attach が成立していない ＝ 何も測れていない");
    assert!(
        cleaned,
        "🚨 SIGHUP で死んだ後に後始末の列（detach の信号）が出ていない"
    );
}
