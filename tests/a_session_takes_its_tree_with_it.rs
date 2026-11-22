//! **起動 1 本ごとの job で、session を閉じると木ごと止まる。**
//!
//! なぜ要るか: `child.kill()` は直接の子（シェル）しか殺さない。シェルが背景に起こした孫
//! （`sleep 900 &`）は、session を閉じても残る。`TerminateJobObject` は孫まで届く。
//!
//! 注意: Windows だけの検体（job object はこの面にしか無い）。
//! unix の「木ごと」は `setsid` + `killpg` の別の道で、ここでは測らない。
//! `cfg(not(unix))` ではなく `cfg(windows)` で切る（iOS/Android を巻き込まない）。
#![cfg(windows)]

use berthd::{write_frame, Control, Frame};
use std::time::Duration;

mod common;
// `alive` / `wait_gone` は `common` に在る。判定の根拠（会員かで見ない理由・pid 0 の罠）はあちらの doc。
use common::{alive, read_ctrl, read_frame_within, send_ctrl, wait_gone, DSR_REPLY, DSR_REQUEST};

#[tokio::test]
async fn closing_a_session_kills_the_grandchild_too() {
    let f = common::Fixture::start("jobtree");
    let mut c = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // 陰性対照を先に立てる（session と無関係な 1 本）。
    // これが道連れになったら「木ごと」ではなく「全部」を殺している。
    // 検体が落ちた回にも残さない（guard が drop で殺す。stdio は 3 つとも null）。
    let outsider = common::Outsider::spawn();
    let outsider_pid = outsider.pid();

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("tree".into()),
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

    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "tree".into(),
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

    // 孫を起こして、その pid をファイルに書かせる。
    // 絵ではなくファイルで受け取る（PTY の出力から pid を読むのは脆い）。
    let pid_file = f.dir.join("grandchild.pid");
    let line = format!(
        // 注意: `-WindowStyle Hidden` を外さないこと。`Start-Process` は既定で
        // 新しいコンソールの窓を作るので、検体を回すたびにデスクトップに窓が積み上がる。
        // しかも中身は `0x800700e8`（`ERROR_NO_DATA`）、すなわち job を落とした瞬間に
        // 起動しかけて失敗した ping で、プロセスは 0 本なのに窓だけ残る
        // （「孫が生き残った」と読めてしまうが、job は正しく働いている）。
        // `-NoNewWindow` は使わない（ペインの中へ ping の出力が混ざり、
        // DSR や印の走査を汚す）。
        "(Start-Process ping -ArgumentList '-n','300','127.0.0.1' -WindowStyle Hidden -PassThru).Id | Set-Content '{}'\r\n",
        pid_file.display()
    );
    write_frame(&mut c, &Frame::data(3, line.into_bytes()))
        .await
        .unwrap();

    // 孫が立つまで待つ（powershell の起動込み）。
    // 待つ間にペインの出力も溜める（立たなかったときにシェルが何を言ったかを出すため）。
    // これが無いと「立たなかった」しか分からず、原因を推測することになる。
    let mut grandchild = 0u32;
    let mut seen = Vec::new();
    let mut answered = false;
    for _ in 0..80 {
        if let Ok(s) = std::fs::read_to_string(&pid_file) {
            if let Ok(p) = s.trim().parse::<u32>() {
                grandchild = p;
                break;
            }
        }
        if let Some(Ok(fr)) = read_frame_within(&mut c, Duration::from_millis(250)).await {
            if fr.kind == berthd::KIND_DATA {
                seen.extend_from_slice(&fr.payload);
            }
        }
        // 注意: DSR に答えないとシェルは起動しない（20 バイトの前口上で止まる。
        // ペインの出力が `ESC[?9001h ESC[?1004h ESC[6n` だけになる）。
        // 詳しくは `tests/smoke.rs` の doc。
        if !answered && seen.windows(DSR_REQUEST.len()).any(|w| w == DSR_REQUEST) {
            answered = true;
            write_frame(&mut c, &Frame::data(9, DSR_REPLY.to_vec()))
                .await
                .unwrap();
        }
    }
    assert_ne!(
        grandchild,
        0,
        "🚨 孫が立たなかった ＝ 検体が何も測れていない。ペインの出力:
{}",
        String::from_utf8_lossy(&seen)
    );
    assert!(
        alive(grandchild),
        "🚨 孫が既に死んでいる ＝ 検体が空回りしている"
    );

    // session を閉じる: `child.kill()` → teardown → `TerminateJobObject`。
    send_ctrl(
        &mut c,
        4,
        &Control::Close {
            session_id: "tree".into(),
        },
    )
    .await;

    assert!(
        wait_gone(grandchild, 10),
        "🔴 孫 (pid {grandchild}) が生き残った —— job が木を掴めていない"
    );
    // 陰性対照: 無関係な 1 本は無傷。
    assert!(
        alive(outsider_pid),
        "🔴 session と無関係な pid {outsider_pid} まで死んだ ＝ 殺し過ぎ"
    );

    drop(outsider);
}
