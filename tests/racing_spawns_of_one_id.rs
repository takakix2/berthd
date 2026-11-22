//! **同じ id の spawn が並んで来ても、1 本だけが立ち、その 1 本は生き残る。**
//!
//! 機構: 「その id は空いているか」を確かめてから表に入れるまでの間に pty と子を起こしている。
//! その間に同じ id の spawn が割り込むと、両方が確認を通り、後の登録が前を上書きする。
//! 前の session の後始末が id だけで表から消すと、**生きている後の session まで消える**。
//!
//! 注意: unix のみ（子に `/bin/cat` を使う）。
#![cfg(unix)]

use berthd::Control;
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl};

const ID: &str = "race";
const RACERS: usize = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_spawns_of_one_id_leave_exactly_one_live_session() {
    let f = common::Fixture::start("race");
    let mut conns = Vec::new();
    for _ in 0..RACERS {
        conns.push(common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await);
    }
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut racers = Vec::new();
    for mut c in conns {
        let barrier = barrier.clone();
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            send_ctrl(
                &mut c,
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
            let reply = read_ctrl(&mut c).await;
            (matches!(reply, Control::Spawned { .. }), c)
        }));
    }
    let mut won = 0;
    let mut keep = Vec::new();
    for r in racers {
        let (ok, c) = r.await.unwrap();
        won += usize::from(ok);
        keep.push(c);
    }
    assert_eq!(won, 1, "🚨 同じ id の spawn が {won} 本とも成功した");

    // 立った 1 本が、負けた側の後始末に巻き込まれず生きていること。
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut l = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(&mut l, 1, &Control::List).await;
    let Control::Sessions { sessions } = read_ctrl(&mut l).await else {
        panic!("List に Sessions が返らない");
    };
    assert!(
        sessions.iter().any(|s| s.id == ID && s.alive),
        "🚨 勝った session が消えている: {:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
}
