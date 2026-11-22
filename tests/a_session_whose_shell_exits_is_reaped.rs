//! **子が終わったら session が表から消えること。**
//!
//! # なぜ検体が要るか
//!
//! Windows では、`vim` に `:q`、`powershell` に `exit` を打つと、子は死ぬのに `berth ls` に残り、
//! 待っても消えない形が在った。unix では同じ操作ですぐ消える。
//! ∴ 割れているのは「速い/遅い」ではなく「刈る経路が在る/無い」。
//!
//! # 注意: この検体が測るのは (a) だけ
//!
//! 症状は 2 つに割れる: **(a) session が一覧に残る** と **(b) `berth attach` が終わらない**。
//! ここが押さえるのは (a)（protocol の `List`）。
//! (b) は CLI の話なので別（端末が要る）。どちらが原因かは、この検体からは言えない。

use berthd::{connect_daemon_at, write_frame, Control, DaemonStream, Frame};
use std::time::Duration;

mod common;
use common::{become_a_terminal, read_ctrl, send_ctrl, wait_for_daemon};

const ID: &str = "reap-tab";

fn line(cmd: &str) -> Vec<u8> {
    let mut v = cmd.as_bytes().to_vec();
    #[cfg(unix)]
    v.extend_from_slice(b"\n");
    #[cfg(not(unix))]
    v.extend_from_slice(b"\r\n");
    v
}

/// 一覧に居るか（`alive` も返す）。pty とは別の接続で訊く（
/// 同じ接続だと、出力の流れの中で問うことになる）。
async fn listed(l: &mut DaemonStream, serial: u32) -> Option<bool> {
    send_ctrl(l, serial, &Control::List).await;
    match read_ctrl(l).await {
        Control::Sessions { sessions } => sessions.iter().find(|s| s.id == ID).map(|s| s.alive),
        other => panic!("expected Sessions, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_whose_shell_exits_leaves_the_list() {
    let f = common::Fixture::start("reap");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    let mut l = connect_daemon_at(&f.endpoint)
        .await
        .expect("second connection");

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some(ID.into()),
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
    // pid を控える（「一覧に残る」を測る前に、子が本当に終わったかを見るため）。
    let pid = match read_ctrl(&mut c).await {
        Control::Spawned { pid, .. } => pid.expect("spawned pid"),
        other => panic!("expected Spawned, got {other:?}"),
    };

    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: ID.into(),
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

    become_a_terminal(&mut c, Duration::from_secs(20)).await;

    // 陽性対照（消えたことを測る前に、在ることを見る）。
    // これが無いと「一度も出来ていなかった」と「消えた」が同じ顔になる。
    assert_eq!(
        listed(&mut l, 100).await,
        Some(true),
        "🚨 打つ前に session が一覧に居ない ＝ この検体は何も測れていない"
    );

    write_frame(&mut c, &Frame::data(0, line("exit")))
        .await
        .unwrap();

    // 注意: 計器の対照（子が死んでいなければ、この検体は「刈られない」を測っていない
    //    （打った `exit` が届かなかっただけ）。
    let child_gone = common::wait_gone(pid, 10);
    assert!(
        child_gone,
        "🚨 `exit` を打っても子（pid {pid}）が終わらない ＝ **この検体は何も測れていない**。
         ⭐ 先に見るべきは「刈られるか」ではなく「打鍵が子まで届くか」。
         ⚠️ まだ alive = {:?}",
        listed(&mut l, 150).await
    );

    // 長くは待たない（unix はすぐ消えるので、
    // 10 秒は「刈る経路が在る/無い」を分けるのに充分で、赤いときも速い。
    let start = std::time::Instant::now();
    let deadline = start + Duration::from_secs(10);
    let mut serial = 200;
    let mut last = Some(true);
    while std::time::Instant::now() < deadline {
        last = listed(&mut l, serial).await;
        serial += 1;
        if last.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let took = start.elapsed();
    assert!(
        last.is_none(),
        "🚨 子が終わっても session が一覧に残っている —— {:.1}s 待った。\n\
         ⭐ 最後に見えた alive = {:?}（`false` なら「死んだと知っているのに消していない」）。\n\
         当て所は child.wait() → drop(master) → table.remove() の順（src/bin/berthd/pty_loop.rs）。
         ⭐ 子（pid {pid}）は**確かに終わっている**（上の対照が通っている）。",
        took.as_secs_f64(),
        last
    );
    eprintln!("[reap] 消えるまで {:.2}s", took.as_secs_f64());
}
