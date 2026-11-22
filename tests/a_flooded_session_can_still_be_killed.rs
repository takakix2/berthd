//! **出力の洪水で塞がっている最中でも、session を閉じれば木ごと止まる。**
//!
//! # なぜ要るか
//!
//! `src/bin/berthd/pty_loop.rs` は、出力の送信を待つ間も入力を捌く（内側の `select!`）。打鍵と出力は
//! 同じ `select!` に居るので、素直に await すると送信を待つ間 `input_rx` が捌けなくなり、
//! 出力の洪水中に `Close` が届かない。内側の `select!` を消してもコンパイルは通り、単体テストも
//! 緑のままで、壊れるのは洪水のときだけ。この検体が無いと気づけない。
//!
//! ## 何を測るか（`Ctrl+C` ではない）
//!
//! Windows の ConPTY には line discipline が無いので、走行中の `Ctrl+C` は効かない
//! （0x03 は PTY に届くが割り込みにならない）。代わりに在るのは「その木ごと終わらせる」手で、
//! 中断ではなく終了なので意味は違う。この検体が主張するのは「止められる」ことだけ。
//!
//! 注意: Windows だけ（job object はこの面にしか無い）。`cfg(not(unix))` ではなく
//! `cfg(windows)` で切る（iOS/Android を巻き込まない）。
#![cfg(windows)]

use berthd::{write_frame, Control, Frame};
use std::time::Duration;

mod common;
use common::{alive, read_ctrl, read_frame_within, send_ctrl, wait_gone, DSR_REPLY, DSR_REQUEST};

/// 洪水が終わったら刷る印。これを Close の前に見たら、検体は「洪水中」を測っていない。
///
/// 注意: 打った行の中にこの綴りを現れさせないこと。シェルは打鍵をエコーするので、
/// 素直に `'FLOOD-DONE'` と打つと印が自分の echo に当たり、すぐ「洪水が終わった」と
/// 報告する。∴ 下のコマンドは `-join` で組み立てて刷る。
const FLOOD_DONE: &[u8] = b"FLOOD-DONE";

/// Close を撃つ前に、ペインから受け取っておきたい最小のバイト数。
/// 「洪水が本当に始まった」の陽性対照。少なすぎると前口上だけで満たされる。
const FLOOD_SEEN_MIN: usize = 64 * 1024;

// 陰性対照: `src/bin/berthd/pty_loop.rs` の `launch_job::terminate(job)` を `drop(master)` の
// **後ろ**へ動かすと、この検体だけが赤くなる（隣の `a_session_takes_its_tree_with_it` は緑のまま）。
// ∴ この 2 本は「木ごと殺す」と「その順序」を別々に押さえている。
#[tokio::test]
async fn a_flood_does_not_stop_close_from_taking_the_tree() {
    let f = common::Fixture::start("floodkill");
    let mut pane = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // 陰性対照を先に立てる（session と無関係な 1 本）。
    // これが道連れになったら「木ごと」ではなく「全部」を殺している。
    // 検体が落ちた回にも残さない（guard が drop で殺す。stdio は 3 つとも null）。
    let outsider = common::Outsider::spawn();
    let outsider_pid = outsider.pid();

    send_ctrl(
        &mut pane,
        1,
        &Control::Spawn {
            session_id: Some("flood".into()),
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
    assert!(matches!(
        read_ctrl(&mut pane).await,
        Control::Spawned { .. }
    ));

    send_ctrl(
        &mut pane,
        2,
        &Control::Attach {
            session_id: "flood".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut pane).await,
        Control::Attached { .. }
    ));

    // 孫を起こして pid をファイルへ、**その後で**洪水を始める。
    // 順序が要る（洪水が先だと pid ファイルが書かれる前に読み手が塞がる）。
    // pid は絵ではなくファイルで受け取る（PTY の出力から読むのは脆い）。
    let pid_file = f.dir.join("flood-grandchild.pid");
    let line = format!(
        // 注意: `-WindowStyle Hidden` は必須。理由は
        // `a_session_takes_its_tree_with_it.rs` の同じ行のコメントに在る（窓が積み上がる）。
        "(Start-Process ping -ArgumentList '-n','300','127.0.0.1' -WindowStyle Hidden -PassThru).Id | \
         Set-Content '{}'; 1..2000000; ('FLOOD','DONE') -join '-'\r\n",
        pid_file.display()
    );
    write_frame(&mut pane, &Frame::data(3, line.into_bytes()))
        .await
        .unwrap();

    // 孫が立ち、かつ洪水が本当に流れ始めるまで読む。
    // 出力も溜める（立たなかったときにシェルが何を言ったかを出すため）。
    let mut grandchild = 0u32;
    let mut seen: Vec<u8> = Vec::new();
    let mut flood_bytes = 0usize;
    let mut answered = false;
    // 注意: 回数ではなく締切で回す。`read_frame` はデータが在れば即返るので、
    // 回数の上限は時間の上限にならない（`for _ in 0..240` は 1 秒足らずで使い切る）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        if grandchild == 0 {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                if let Ok(p) = s.trim().parse::<u32>() {
                    grandchild = p;
                }
            }
        }
        if grandchild != 0 && flood_bytes >= FLOOD_SEEN_MIN {
            break;
        }
        if let Some(Ok(fr)) = read_frame_within(&mut pane, Duration::from_millis(250)).await {
            if fr.kind == berthd::KIND_DATA {
                flood_bytes += fr.payload.len();
                // 全部は溜めない（2M 行を抱えると検体自身が重くなる）。
                // 先頭だけ残せば「シェルが何を言ったか」には足りる。
                if seen.len() < 8192 {
                    seen.extend_from_slice(&fr.payload);
                }
                if fr
                    .payload
                    .windows(FLOOD_DONE.len())
                    .any(|w| w == FLOOD_DONE)
                {
                    panic!(
                        "🚨 Close を撃つ前に洪水が終わった ＝ この検体は「洪水中」を測っていない。\n\
                         ▶ `1..2000000` を増やすか、読み取りを遅くすること。"
                    );
                }
            }
            // 注意: DSR に答えないとシェルは起動しない（20 バイトの前口上で止まる）。
            // 詳しくは `tests/smoke.rs` の doc。
            if !answered && seen.windows(DSR_REQUEST.len()).any(|w| w == DSR_REQUEST) {
                answered = true;
                write_frame(&mut pane, &Frame::data(9, DSR_REPLY.to_vec()))
                    .await
                    .unwrap();
            }
        }
    }

    assert_ne!(
        grandchild,
        0,
        "🚨 孫が立たなかった ＝ 検体が何も測れていない。ペインの出力:\n{}",
        String::from_utf8_lossy(&seen)
    );
    assert!(
        alive(grandchild),
        "🚨 孫が既に死んでいる ＝ 検体が空回りしている"
    );
    assert!(
        flood_bytes >= FLOOD_SEEN_MIN,
        "🚨 洪水が {flood_bytes} バイトしか流れていない（要 {FLOOD_SEEN_MIN}）\
         ＝ 塞がった状態を作れていない。ペインの出力:\n{}",
        String::from_utf8_lossy(&seen)
    );

    // ここからが本題: ペインの読み取りを止める。
    // これで client の sink が埋まり、pump は `sink.reserve()` で待つ。
    // 内側の `select!` が無ければ `input_rx` は二度と捌かれない状態。
    // 注意: `pane` は drop しない（切ると berthd 側が「client が消えた」経路に入り、
    //    測りたい「塞がっている」状態ではなくなる）。
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 別の接続から閉じる（実際に `berth kill` を撃つのは別 client なので、
    // 攻め口も本番と同じにする（`Close` は表を引くだけで attach を要求しない）。
    let mut killer = common::wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut killer,
        1,
        &Control::Close {
            session_id: "flood".into(),
        },
    )
    .await;

    assert!(
        wait_gone(grandchild, 20),
        "🔴 洪水で塞がっている間は Close が効かない —— 孫 (pid {grandchild}) が生き残った。\n\
         ⭐ `src/bin/berthd/pty_loop.rs` の内側の `select!`（`sink.reserve()` と `input_rx.recv()` を\n\
         同じ `select!` に置く形）が効いていない可能性。"
    );
    // 陰性対照: 無関係な 1 本は無傷。
    assert!(
        alive(outsider_pid),
        "🔴 session と無関係な pid {outsider_pid} まで死んだ ＝ 殺し過ぎ"
    );

    drop(outsider);
}
