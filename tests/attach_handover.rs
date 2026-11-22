//! **attach の受け渡し**: 前の attach が残らない・止まった持ち主に新しい持ち主が待たされない。
//!
//! 注意: unix のみ（子に `sh` の小さなスクリプトと `yes` を使う）。
#![cfg(unix)]

use berthd::{read_frame, Control, DaemonStream, KIND_DATA};
use std::time::Duration;
use tokio::sync::mpsc;

mod common;
use common::{read_ctrl, send_ctrl};

async fn spawn(c: &mut DaemonStream, serial: u32, id: &str, shell: &str) {
    send_ctrl(
        c,
        serial,
        &Control::Spawn {
            session_id: Some(id.into()),
            name: None,
            shell: Some(shell.into()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    let reply = read_ctrl(c).await;
    assert!(
        matches!(reply, Control::Spawned { .. }),
        "spawn {id}: {reply:?}"
    );
}

fn attach(id: &str) -> Control {
    Control::Attach {
        session_id: id.into(),
        cols: 80,
        rows: 24,
        actor: None,
        actor_src: None,
        read_only: false,
        seen_total: None,
    }
}

/// data フレームを読み続けるタスクを立てる。
///
/// 注意: `read_frame` は取り消しに強くない。`timeout` で途中で落とすと、読みかけのフレームの残りが
/// 次のフレームの頭に見えて、読む位置がずれる。∴ 読むのは 1 本のタスクに任せ、こちらは受け口を待つ。
fn read_data_in_background(mut c: DaemonStream) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(f) = read_frame(&mut c).await {
            if f.kind == KIND_DATA && tx.send(f.payload).is_err() {
                return;
            }
        }
    });
    rx
}

/// `within` の間に届いた data を全部集める。
async fn collect_data(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>, within: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, async {
        while let Some(bytes) = rx.recv().await {
            out.extend_from_slice(&bytes);
        }
    })
    .await;
    out
}

/// 同じ接続で別の session へ attach し直したら、前の session の出力はもう届かない。
#[tokio::test]
async fn reattaching_on_the_same_connection_lets_go_of_the_previous_session() {
    let f = common::Fixture::start("handover-reattach");
    let ticker = f.dir.join("ticker.sh");
    std::fs::write(
        &ticker,
        "#!/bin/sh\nwhile :; do echo TICK_A; sleep 0.1; done\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ticker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut c = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    spawn(&mut c, 1, "a", ticker.to_str().unwrap()).await;
    spawn(&mut c, 2, "b", "/bin/cat").await;

    send_ctrl(&mut c, 3, &attach("a")).await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Attached { .. }));
    send_ctrl(&mut c, 4, &attach("b")).await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Attached { .. }));

    // 切り替えの直前に送られていた分を捨ててから測る。
    let mut data = read_data_in_background(c);
    let _ = collect_data(&mut data, Duration::from_millis(300)).await;
    let after = collect_data(&mut data, Duration::from_secs(1)).await;
    assert!(
        !String::from_utf8_lossy(&after).contains("TICK_A"),
        "🚨 b に attach し直した後も a の出力が届いている: {:?}",
        String::from_utf8_lossy(&after)
    );
}

/// 前の持ち主が読むのをやめていても、新しい持ち主の attach は待たされず、出力も届く。
#[tokio::test]
async fn a_stalled_owner_does_not_hold_up_the_next_attach() {
    let f = common::Fixture::start("handover-stalled");
    let mut stalled = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    spawn(&mut stalled, 1, "flood", "/usr/bin/yes").await;
    send_ctrl(&mut stalled, 2, &attach("flood")).await;
    assert!(matches!(
        read_ctrl(&mut stalled).await,
        Control::Attached { .. }
    ));
    // 注意: ここから `stalled` は一切読まない（socket も sink も満杯になる）。
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut next = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(&mut next, 1, &attach("flood")).await;
    let attached = tokio::time::timeout(Duration::from_secs(5), read_ctrl(&mut next)).await;
    assert!(
        matches!(attached, Ok(Control::Attached { .. })),
        "🚨 止まった持ち主のせいで、次の attach が 5 秒返らなかった: {attached:?}"
    );
    // 録画の後に live が流れ続けること（ポンプが古い持ち主を待ったままになっていない）。
    let mut data = read_data_in_background(next);
    let _ = collect_data(&mut data, Duration::from_millis(500)).await;
    let live = collect_data(&mut data, Duration::from_secs(1)).await;
    assert!(
        !live.is_empty(),
        "🚨 新しい持ち主に live の出力が届かない（ポンプが前の持ち主を待っている）"
    );

    // 外部 detach も待たされない。
    let mut other = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut other,
        1,
        &Control::Detach {
            session_id: "flood".into(),
        },
    )
    .await;
    send_ctrl(&mut other, 2, &Control::List).await;
    let listed = tokio::time::timeout(Duration::from_secs(5), read_ctrl(&mut other)).await;
    assert!(
        matches!(listed, Ok(Control::Sessions { .. })),
        "🚨 外部 detach が返らない"
    );
    drop(stalled);
}
