//! **読むのが遅い client にも、終わったこと（`Exited` と終了状態）が届く。**
//!
//! 機構: 大量に出力して終わる子では、最後の出力で操作する側の口がちょうど満杯になる。
//! そこへ `Exited` を待たずに送ると落ち、client は終わり方も状態も知らずに切られる。
//!
//! 注意: unix だけ（子に `sh` のスクリプトを使う）。
#![cfg(unix)]

use berthd::{decode_control, read_frame, Control, KIND_CONTROL};
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl};

#[tokio::test]
async fn a_slow_reader_still_hears_the_exit() {
    let f = common::Fixture::start("slowexit");
    let script = f.dir.join("flood-and-exit.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nsleep 0.5\nhead -c 3000000 /dev/zero | tr '\\0' 'y'\nexit 7\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut c = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("slow".into()),
            name: None,
            shell: Some(script.to_str().unwrap().into()),
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
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "slow".into(),
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

    // 注意: しばらく読まない（口が満杯のまま子が出し切る）。
    tokio::time::sleep(Duration::from_secs(3)).await;

    let exited = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let Ok(f) = read_frame(&mut c).await else {
                return None;
            };
            // ずっと遅く読む（口は満杯の近くに留まり続ける）。
            tokio::time::sleep(Duration::from_millis(2)).await;
            if f.kind == KIND_CONTROL {
                if let Ok(Control::Exited { status, .. }) = decode_control(&f.payload) {
                    return Some(status);
                }
            }
        }
    })
    .await;
    assert_eq!(
        exited,
        Ok(Some(7)),
        "🚨 読むのが遅い client に Exited が届かなかった（None ＝ 黙って切られた）"
    );
}
