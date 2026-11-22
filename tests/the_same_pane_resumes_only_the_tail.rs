//! 同じペインで入り直したら**続きだけ**流す（protocol 13）。
//!
//! berthd は `Attached.replay_total`（セッション開始からの累計バイト）を名乗る。client はそれを基準に
//! 「どこまで見せたか」を覚え、次の `Attach.seen_total` で送り返す。seen がリング内なら daemon は
//! `[seen, produced)` だけ返す。この検体は、ある attach が名乗った `replay_total` をそのまま次の
//! `seen_total` にすると、**その間に出た分だけ**が録画で返り、古い洪水は二度と流れないことを実デーモンで測る。
//!
//! pty 越しの `berth` ではなくプロトコルを直に話す（録画のバイトそのものを数えるので ConPTY の描き直しに依らない）。

use berthd::{write_frame, Control, Frame, KIND_DATA};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

mod common;
use common::{
    become_a_terminal, read_ctrl, read_frame_within, send_ctrl, wait_for_daemon, Fixture,
};

const FLOOD_MARKER: &[u8] = b"BERTHD_FLOOD_DONE";
const TAIL_MARKER: &[u8] = b"BERTHD_TAIL_HERE";
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
async fn the_same_pane_resumes_only_the_tail_not_the_whole_flood() {
    let fx = Fixture::start("resumepane");
    let endpoint = fx.endpoint.clone();

    // ── 1) セッションを作り、容量を越える洪水を流す ──
    let mut c1 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c1,
        1,
        &Control::Hello {
            client: "pane/1".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c1).await, Control::Welcome { .. }));
    send_ctrl(
        &mut c1,
        2,
        &Control::Spawn {
            session_id: Some("resume-pane".into()),
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
    // 最初の attach も replay_total を名乗る（protocol 13）。
    assert!(
        matches!(
            read_ctrl(&mut c1).await,
            Control::Attached {
                replay_total: Some(_),
                ..
            }
        ),
        "even the first attach names replay_total",
    );
    become_a_terminal(&mut c1, Duration::from_secs(20)).await;
    #[cfg(windows)]
    let flood = "$e=[char]27; $sb=New-Object System.Text.StringBuilder; foreach($i in 1..40000){ [void]$sb.Append(\"$e[31mline $i abc$e[0m`n\") }; [Console]::Out.Write($sb.ToString()); echo (\"BERTHD_FLOOD_\" + \"DONE\")";
    #[cfg(unix)]
    let flood = "i=0; while [ $i -lt 40000 ]; do i=$((i+1)); printf '\\033[31mline %d abc\\033[0m\\n' $i; done; echo BERTHD_FLOOD_\"\"DONE";
    write_frame(&mut c1, &Frame::data(0, line(flood)))
        .await
        .unwrap();
    let out = drain_data_until(&mut c1, FLOOD_MARKER, Duration::from_secs(120)).await;
    assert!(contains(&out, FLOOD_MARKER), "the flood must finish");
    assert!(
        out.len() > REPLAY_CAP,
        "the flood must overflow the recording; got {} bytes",
        out.len()
    );
    c1.shutdown().await.ok();
    drop(c1);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ── 2) 入り直す（全体）。ここで名乗られる replay_total が「ここまで見せた」位置。
    //        続けて小さな目印を出す（これが次に返るべき「続き」）。 ──
    let mut c2 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c2,
        10,
        &Control::Hello {
            client: "pane/2".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c2).await, Control::Welcome { .. }));
    send_ctrl(
        &mut c2,
        11,
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
    let (whole_len, seen) = match read_ctrl(&mut c2).await {
        Control::Attached {
            replay_len,
            replay_total,
            ..
        } => (replay_len, replay_total.expect("protocol 13")),
        other => panic!("expected Attached, got {other:?}"),
    };
    // 全体はリングいっぱい（洪水が入っている）。
    assert!(
        whole_len > REPLAY_CAP / 2,
        "the whole replay should be large: {whole_len}"
    );
    // replay を読み切ってから目印を出す（live と混ざらないように）。
    let _ = drain_data_until(&mut c2, FLOOD_MARKER, Duration::from_secs(20)).await;
    #[cfg(windows)]
    let tail = "echo (\"BERTHD_TAIL_\" + \"HERE\")";
    #[cfg(unix)]
    let tail = "echo BERTHD_TAIL_\"\"HERE";
    write_frame(&mut c2, &Frame::data(0, line(tail)))
        .await
        .unwrap();
    let _ = drain_data_until(&mut c2, TAIL_MARKER, Duration::from_secs(20)).await;
    c2.shutdown().await.ok();
    drop(c2);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ── 3) 同じペインの位置（seen）で入り直す → 返るのは目印を含む「続き」だけ・洪水は出ない ──
    let mut c3 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c3,
        20,
        &Control::Hello {
            client: "pane/3".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c3).await, Control::Welcome { .. }));
    send_ctrl(
        &mut c3,
        21,
        &Control::Attach {
            session_id: id,
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: Some(seen),
        },
    )
    .await;
    let tail_len = match read_ctrl(&mut c3).await {
        Control::Attached { replay_len, .. } => replay_len,
        other => panic!("expected Attached, got {other:?}"),
    };
    // 続きは小さい（洪水の 512KB ではなく、目印とプロンプト数行ぶん）。
    assert!(
        tail_len < 64 * 1024,
        "the resume must be the tail, not the whole flood: {tail_len}"
    );
    let mut replay = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while replay.len() < tail_len {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match read_frame_within(&mut c3, remaining).await {
            Some(Ok(f)) if f.kind == KIND_DATA => replay.extend_from_slice(&f.payload),
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(replay.len(), tail_len);
    assert!(
        contains(&replay, TAIL_MARKER),
        "the resume must carry the new output that happened after seen"
    );
    assert!(
        !contains(&replay, b"line 1 abc"),
        "the resume must NOT re-show the old flood"
    );
    drop(fx);
}
