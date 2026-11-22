//! **1 本の session が入力で詰まっても、デーモン全体は止まらない。**
//!
//! 機構: 出力を読まない client が大量に貼り付けると、子（`cat`）は出力で止まり、
//! PTY への書き込みが返らなくなる。書き込みが io ループの中で同期に走り、
//! 打鍵の転送が session 表のロックを握ったまま `send().await` していると、
//! **別の client の `List` まで止まる**。
//!
//! 注意: unix だけ（Windows の ConPTY は DSR に答えないと進まないので、別の形が要る）。
#![cfg(unix)]

use berthd::{write_frame, Control, Frame};
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl};

const ID: &str = "jam";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jammed_session_does_not_stop_the_daemon() {
    let f = common::Fixture::start("jam");
    let mut paster = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut paster,
        1,
        &Control::Spawn {
            session_id: Some(ID.into()),
            name: None,
            shell: Some("/bin/cat".into()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(
        matches!(read_ctrl(&mut paster).await, Control::Spawned { .. }),
        "spawn"
    );
    send_ctrl(
        &mut paster,
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
    assert!(
        matches!(read_ctrl(&mut paster).await, Control::Attached { .. }),
        "attach"
    );

    // 注意: **出力を一切読まない**貼り付け手。行ごとに改行を入れて `cat` に読ませ、echo を溢れさせる。
    let line = {
        let mut l = vec![b'x'; 1023];
        l.push(b'\n');
        l
    };
    let pasting = tokio::spawn(async move {
        for _ in 0..4096 {
            if write_frame(&mut paster, &Frame::data(0, line.clone()))
                .await
                .is_err()
            {
                break;
            }
        }
        paster
    });

    // 詰まるまでの猶予（4MB を流し込む間に、書き込み・出力の両方が塞がる）。
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut other = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(&mut other, 1, &Control::List).await;
    let listed = tokio::time::timeout(Duration::from_secs(5), read_ctrl(&mut other)).await;
    assert!(
        matches!(listed, Ok(Control::Sessions { .. })),
        "🚨 1 本の session の詰まりで、別の client の List が 5 秒返らなかった"
    );

    // 詰まった session も閉じられること（`Close` が入力の詰まりに巻き込まれない）。
    send_ctrl(
        &mut other,
        2,
        &Control::Close {
            session_id: ID.into(),
        },
    )
    .await;
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        let mut serial = 3;
        loop {
            send_ctrl(&mut other, serial, &Control::List).await;
            serial += 1;
            if let Control::Sessions { sessions } = read_ctrl(&mut other).await {
                if sessions.iter().all(|s| s.id != ID) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "🚨 詰まった session を Close しても消えなかった"
    );
    pasting.abort();
}
