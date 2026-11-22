//! Windows で、クライアントの attach が ConPTY の最初の出力より**後**になっても、シェルが起動すること。
//!
//! ConPTY（`--inheritcursor`）は起動時にまず `ESC[6n`（カーソル位置の問い合わせ）を出し、答えが来るまで
//! シェルを止める。その 4 バイトが attach より先に届くと録画バッファに入り、attach 時の `sanitize_replay`
//! （問い合わせは再生しない）が捨てる。クライアントは問いを見ないので答えず、シェルは二度と起動しない。
//! 先に届くか後に届くかは競合なので、対処が無いと同じテストがときどき失敗する形になる。
//!
//! ここでは attach を**意図的に遅らせて**、問いが必ず録画側に入る状態を作る（対処が無ければ毎回失敗する）。
//! 対処: berthd が起動直後の `ESC[6n` を自分で答え（`ESC[1;1R`）、録画にも live にも流さない。
//! ∴ 期待は 2 つ: (1) 遅れて attach してもシェルが打鍵に応える、(2) クライアントに `ESC[6n` は届かない
//! （届くと berth が答えてしまい、その答えがシェルの stdin に入る）。
#![cfg(windows)]

use berthd::{write_frame, Control, Frame};
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl, wait_for_daemon, DSR_REQUEST};

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn a_late_attach_still_gets_a_shell() {
    let f = common::Fixture::start("lateattach");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("late".into()),
            name: None,
            shell: Some("powershell".into()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));

    // ここが本題: ConPTY の最初の出力（`ESC[6n`）が録画に入るまで待ってから attach する。
    // 問いは spawn から数 ms で届くので、700ms は十分に遅い。
    tokio::time::sleep(Duration::from_millis(700)).await;

    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "late".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Attached { .. }));

    // 打鍵に応えるまで読む（DSR が来れば答えるが、来ないのが正しい姿）。
    let seen = common::become_a_terminal(&mut c, Duration::from_secs(20)).await;
    assert!(
        contains(&seen, b"BERTHD_READY"),
        "🔴 attach が遅れるとシェルが起動しない。届いた {} バイト: {:?}",
        seen.len(),
        String::from_utf8_lossy(&seen)
    );
    assert!(
        !contains(&seen, DSR_REQUEST),
        "🔴 起動時の `ESC[6n` がクライアントまで届いている ＝ berthd が答えていない"
    );

    send_ctrl(
        &mut c,
        3,
        &Control::Close {
            session_id: "late".into(),
        },
    )
    .await;
    let _ = write_frame(&mut c, &Frame::data(0, Vec::new())).await;
}
