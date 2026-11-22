//! 録画（replay）が容量を越えたとき、頭は**行頭**で切れ、捨てた分が残した状態が
//! `Attached.replay_head` で渡る（protocol 12）。
//!
//! 行頭で切らないと、頭が `31mline 22527 あいう…` のようになる（`ESC[` が捨てられ、`31m` が文字として残る形）。
//! この検体はその形が出ないことを、実デーモンとプロトコルで測る。
//!
//! pty 越しの `berth` ではなくプロトコルを直に話すので両面で走る（ConPTY は描き直して届けるので
//! 行を数えられないが、ここで見るのは録画のバイトそのもので、ConPTY が描いた後の形でよい）。

use berthd::{write_frame, Control, Frame, KIND_DATA};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

mod common;
use common::{
    become_a_terminal, read_ctrl, read_frame_within, send_ctrl, wait_for_daemon, Fixture,
};

/// 打つ側の echo に同じ綴りが出ないよう、シェルの中で連結して作らせる。
const MARKER: &[u8] = b"BERTHD_HEAD_DONE";
const REPLAY_CAP: usize = 512 * 1024;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn line(cmd: &str) -> Vec<u8> {
    let mut v = cmd.as_bytes().to_vec();
    #[cfg(unix)]
    v.extend_from_slice(b"\n");
    #[cfg(not(unix))]
    v.extend_from_slice(b"\r\n");
    v
}

async fn drain_data_until(
    s: &mut berthd::DaemonStream,
    needle: &[u8],
    within: Duration,
) -> Vec<u8> {
    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match read_frame_within(s, remaining).await {
            Some(Ok(f)) => {
                if f.kind == KIND_DATA {
                    acc.extend_from_slice(&f.payload);
                }
                if contains(&acc, needle) {
                    break;
                }
            }
            _ => break,
        }
    }
    acc
}

#[tokio::test]
async fn the_replay_head_starts_at_a_line_and_carries_the_state_the_dropped_bytes_left() {
    let fx = Fixture::start("replayhead");
    let endpoint = fx.endpoint.clone();
    let id = {
        let mut c1 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
        send_ctrl(
            &mut c1,
            1,
            &Control::Hello {
                client: "head/1".into(),
            },
        )
        .await;
        assert!(matches!(read_ctrl(&mut c1).await, Control::Welcome { .. }));
        send_ctrl(
            &mut c1,
            2,
            &Control::Spawn {
                session_id: Some("replay-head".into()),
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
        let id = match read_ctrl(&mut c1).await {
            Control::Spawned { session_id, .. } => session_id,
            other => panic!("expected Spawned, got {other:?}"),
        };
        send_ctrl(
            &mut c1,
            3,
            &Control::Attach {
                session_id: id.clone(),
                cols: 80,
                rows: 24,
                actor: None,
                actor_src: None,
                read_only: false,
                seen_total: None,
            },
        )
        .await;
        become_a_terminal(&mut c1, Duration::from_secs(20)).await;
        // 1 行 ≈ 25 バイト × 40,000 行 ≈ 1 MB（容量 512 KB を 2 倍近く越える）。
        // 色は行ごとに付けて戻す（ConPTY は描き直して届けるので、Windows ではどのみち行ごとの形になる）。
        #[cfg(windows)]
        let cmd = "$e=[char]27; $sb=New-Object System.Text.StringBuilder; foreach($i in 1..40000){ [void]$sb.Append(\"$e[31mline $i abc$e[0m`n\") }; [Console]::Out.Write($sb.ToString()); echo (\"BERTHD_HEAD_\" + \"DONE\")";
        #[cfg(unix)]
        let cmd = "i=0; while [ $i -lt 40000 ]; do i=$((i+1)); printf '\\033[31mline %d abc\\033[0m\\n' $i; done; echo BERTHD_HEAD_\"\"DONE";
        write_frame(&mut c1, &Frame::data(0, line(cmd)))
            .await
            .unwrap();
        let out = drain_data_until(&mut c1, MARKER, Duration::from_secs(120)).await;
        assert!(
            contains(&out, MARKER),
            "the shell must finish the loop; got {} bytes",
            out.len()
        );
        assert!(
            out.len() > REPLAY_CAP,
            "the loop must overflow the recording to mean anything; got {} bytes",
            out.len()
        );
        c1.shutdown().await.ok();
        drop(c1);
        id
    };
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut c2 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c2,
        10,
        &Control::Hello {
            client: "head/2".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c2).await, Control::Welcome { .. }));
    send_ctrl(
        &mut c2,
        11,
        &Control::Attach {
            session_id: id,
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    let (replay_len, head) = match read_ctrl(&mut c2).await {
        Control::Attached {
            replay_len,
            replay_head,
            ..
        } => (replay_len, replay_head),
        other => panic!("expected Attached, got {other:?}"),
    };
    let head = head.expect("something was dropped, so the head state must be there (protocol 12)");
    assert!(!head.alt_screen, "{head:?}");
    assert_eq!(
        head.sgr, "",
        "each line reset its colour before the newline: {head:?}"
    );

    let mut replay = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while replay.len() < replay_len {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match read_frame_within(&mut c2, remaining).await {
            Some(Ok(f)) if f.kind == KIND_DATA => replay.extend_from_slice(&f.payload),
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(replay.len(), replay_len);
    assert!(replay.len() <= REPLAY_CAP, "{}", replay.len());
    // 行頭とは、その行の最初の命令（色）からを指す。`31mline` のような命令の尻尾でも、`\n` の手前の切れ端でもない。
    let shown = String::from_utf8_lossy(&replay[..replay.len().min(48)]).to_string();
    assert!(
        replay.starts_with(b"\x1b[31mline "),
        "the head must start at a line: {shown:?}"
    );
    drop(fx);
}
