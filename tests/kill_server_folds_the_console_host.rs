//! **`kill-server` が ConPTY のホストを本当に畳むか。**
//!
//! # なぜ別の検体が要るか: unix 側の検体は恒真
//!
//! `smoke.rs::kill_server_closes_sessions_before_it_exits` は `#[cfg(unix)]` で、unix では畳まなくても
//! 子は死ぬ（`shutdown_sessions` を外しても緑のまま）。本物の性質（ConPTY が閉じる）を測れるのは Windows だけ。
//! ∴ あちらが見ているのは工程が在ることで、畳めたことではない。
//!
//! ここが見るのは**ホストのプロセスが残らないこと**（台帳でも log でもなく、プロセス）。
//! 注意: `kill-server` が成功を返すことを「畳めた」と読まない（子は死んでいるのに session が残る形が在る）。
//!
//! 注意: Windows のみ。ConPTY のホストはこの面にしか無い。
#![cfg(windows)]

use berthd::Control;
use std::time::Duration;

mod common;
use common::{alive, console_hosts_of, read_ctrl, send_ctrl, wait_for_daemon};

const ID: &str = "fold-tab";

#[tokio::test]
async fn kill_server_leaves_no_console_host_behind() {
    let f = common::Fixture::start("fold");
    let daemon_pid = f.daemon_pid().expect("daemon pid");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

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
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
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
    // 注意: 意図的に DSR に答えない。畳みが刺さるのは答えていない session のときで
    // （ホストが自分の DSR の答えを待っている形）、答えてしまうとこの検体は病理を通らない。
    // ホストは `openpty` の時点で立つので、数える相手はこれで足りる。
    // `become_a_terminal` を呼ばない（呼ぶと恒真になる）。
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 陽性対照: 畳めたことを測る前に、畳む相手が居ることを見る。
    // これが無いと「ホストが 0 本」は一度も立たなかったのと同じ顔になる。
    let hosts = console_hosts_of(daemon_pid);
    assert!(
        !hosts.is_empty(),
        "🚨 ConPTY のホストが 1 本も立っていない ＝ この検体は何も測れていない\n\
         （berthd pid {daemon_pid} の子にコンソールのホストが居ない）"
    );

    send_ctrl(&mut c, 3, &Control::KillServer).await;
    // daemon が終わるのを待つ（`kill-server` の成功ではなく、**終わったこと**を見る）。
    for _ in 0..100 {
        if !alive(daemon_pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !alive(daemon_pid),
        "🚨 berthd が終わっていない（pid {daemon_pid}）"
    );

    // 注意: 親が死んだ後に数える（残っていれば孤児）。
    // 少し待つ: 畳みは親の終了と同時とは限らない。
    let t0 = std::time::Instant::now();
    let mut left: Vec<(u32, String)> = Vec::new();
    for _ in 0..50 {
        left = hosts
            .iter()
            .filter(|(pid, _)| alive(*pid))
            .cloned()
            .collect();
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "[fold] ホスト {} 本が消えるまで {:.2}s",
        hosts.len(),
        t0.elapsed().as_secs_f64()
    );
    assert!(
        left.is_empty(),
        "🚨 ConPTY のホストが孤児として残った: {left:?}\n\
         ⭐ berthd は終わっているのに、立てたホストが生きている。\n\
         当て所は shutdown_sessions → terminate(job) → drop(master) の順（src/bin/berthd/pty_loop.rs）。"
    );
}
