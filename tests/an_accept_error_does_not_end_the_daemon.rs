//! **accept が 1 回失敗しても、デーモンは待ち続ける。**
//!
//! 機構: fd が尽きる（EMFILE）と `accept` はエラーを返す。それを致命扱いしてループを抜けると、
//! 生きている全 session が巻き添えで終わる。fd が戻れば、また受けられなければならない。
//!
//! 注意: unix だけ（デーモンの fd 上限を `setrlimit` で下げて起こす）。
#![cfg(unix)]

use berthd::Control;
use std::os::unix::process::CommandExt;
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl, ChildGuard};

/// デーモンに許す fd の数。起動に要る分（std 3 本 ＋ socket ＋ tokio の内部）より少し多い。
const NOFILE: libc::rlim_t = 32;

#[tokio::test]
async fn an_accept_error_does_not_end_the_daemon() {
    let dir = std::env::temp_dir().join(format!("berthd-emfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let endpoint = dir.join("berthd.sock");
    std::fs::remove_file(&endpoint).ok();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"));
    command
        .env("BERTHD_SOCKET", &endpoint)
        .env("BERTHD_JOURNAL_PATH", dir.join("journal.jsonl"));
    // SAFETY: fork と exec の間で呼ぶのは async-signal-safe な setrlimit だけ。
    unsafe {
        command.pre_exec(|| {
            let lim = libc::rlimit {
                rlim_cur: NOFILE,
                rlim_max: NOFILE,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _daemon = ChildGuard::new(command.spawn().expect("spawn berthd"));
    let endpoint = endpoint.into_os_string();

    let first = common::wait_for_daemon(&endpoint, Duration::from_secs(5)).await;

    // 上限を超える数だけ繋いで握ったままにする（どこかで accept が EMFILE を返す）。
    let mut hogs = vec![first];
    for _ in 0..(NOFILE as usize * 2) {
        match berthd::connect_daemon_at(&endpoint).await {
            Ok(s) => hogs.push(s),
            Err(_) => break,
        }
    }
    // デーモンが EMFILE を踏むまでの猶予。
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(hogs);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // fd が戻った後も受けられること。
    let mut c = tokio::time::timeout(
        Duration::from_secs(5),
        common::wait_for_daemon(&endpoint, Duration::from_secs(5)),
    )
    .await
    .expect("🚨 accept の失敗でデーモンが終わった");
    send_ctrl(&mut c, 1, &Control::List).await;
    let listed = tokio::time::timeout(Duration::from_secs(5), read_ctrl(&mut c)).await;
    assert!(
        matches!(listed, Ok(Control::Sessions { .. })),
        "🚨 accept の失敗の後、List に答えなかった"
    );
    std::fs::remove_dir_all(&dir).ok();
}
