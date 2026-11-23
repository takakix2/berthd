//! `SendKeys`（attach なしのキー注入）と `Peek`（状態・画面の覗き見）の結合テスト。

use berthd::{read_frame, Control, KIND_DATA};
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl, wait_for_daemon};

fn test_shell() -> String {
    #[cfg(windows)]
    {
        "powershell".to_string()
    }
    #[cfg(not(windows))]
    {
        "sh".to_string()
    }
}

#[tokio::test]
async fn send_keys_and_peek_work_without_detaching_owner() {
    let f = common::Fixture::start("sendpeek");
    let mut owner = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut owner,
        1,
        &Control::Spawn {
            session_id: Some("sp1".into()),
            name: Some("worker".into()),
            shell: Some(test_shell()),
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
        read_ctrl(&mut owner).await,
        Control::Spawned { .. }
    ));

    send_ctrl(
        &mut owner,
        2,
        &Control::Attach {
            session_id: "sp1".into(),
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
        read_ctrl(&mut owner).await,
        Control::Attached { .. }
    ));

    // 別接続から attach せずに SendKeys を送る。
    let mut injector = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut injector,
        1,
        &Control::SendKeys {
            session_id: "sp1".into(),
            data: b"echo BERTH_SEND_PEEK_OK\r".to_vec(),
            force: false,
        },
    )
    .await;
    match read_ctrl(&mut injector).await {
        Control::SendKeysAck { session_id, bytes } => {
            assert_eq!(session_id, "sp1");
            assert_eq!(bytes, b"echo BERTH_SEND_PEEK_OK\r".len());
        }
        other => panic!("expected SendKeysAck, got {other:?}"),
    }

    // Peek で出力が届くまで確認する。
    let mut saw_marker = false;
    for _ in 0..40 {
        send_ctrl(
            &mut injector,
            2,
            &Control::Peek {
                session_id: "sp1".into(),
                max_bytes: Some(64 * 1024),
            },
        )
        .await;
        let (data_len, cols, rows, attached) = match read_ctrl(&mut injector).await {
            Control::PeekResult {
                data_len,
                cols,
                rows,
                attached,
                ..
            } => (data_len, cols, rows, attached),
            other => panic!("expected PeekResult, got {other:?}"),
        };
        assert!(attached, "owner connection should still be attached");
        let bytes = if data_len > 0 {
            loop {
                let frame = read_frame(&mut injector).await.unwrap();
                if frame.kind == KIND_DATA {
                    break frame.payload;
                }
            }
        } else {
            Vec::new()
        };
        let plain = berth_screen::render_plain(&bytes, cols, rows);
        if plain.lines.iter().any(|l| l.contains("BERTH_SEND_PEEK_OK")) {
            saw_marker = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(saw_marker, "Peek did not observe injected command output");

    // 既存オーナーは Detached されず Ping に応答できるはず（別の操作も継続可能）。
    send_ctrl(&mut owner, 3, &Control::Ping).await;
    assert!(matches!(read_ctrl(&mut owner).await, Control::Pong));
}

#[cfg(unix)]
#[tokio::test]
async fn send_loop_limit_rejects_repeated_unanswered_sends() {
    let f = common::Fixture::start("sendloop");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("silent".into()),
            name: None,
            shell: Some("sh".into()),
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

    // エコーを切って無出力のまま待機させる。
    send_ctrl(
        &mut c,
        2,
        &Control::SendKeys {
            session_id: "silent".into(),
            data: b"stty -echo; sleep 10\r".to_vec(),
            force: false,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut c).await,
        Control::SendKeysAck { .. }
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 1 回目で現在の produced を固定し、続けて 4 回（計 5 回）まで通ることを確認する。
    for seq in 3..8u32 {
        send_ctrl(
            &mut c,
            seq,
            &Control::SendKeys {
                session_id: "silent".into(),
                data: b"x".to_vec(),
                force: false,
            },
        )
        .await;
        match read_ctrl(&mut c).await {
            Control::SendKeysAck { .. } => {}
            other => panic!("send #{seq} should succeed, got {other:?}"),
        }
    }

    // 6 回目は send_loop_limit で拒否される。
    send_ctrl(
        &mut c,
        8,
        &Control::SendKeys {
            session_id: "silent".into(),
            data: b"x".to_vec(),
            force: false,
        },
    )
    .await;
    match read_ctrl(&mut c).await {
        Control::Error { code, .. } => assert_eq!(code, "send_loop_limit"),
        other => panic!("expected send_loop_limit error, got {other:?}"),
    }

    // force: true なら通過する。
    send_ctrl(
        &mut c,
        9,
        &Control::SendKeys {
            session_id: "silent".into(),
            data: b"x".to_vec(),
            force: true,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut c).await,
        Control::SendKeysAck { .. }
    ));
}

/// berthd は `ESC ] 7501 ; ?` に自分で答え（端末が居なくても）、問い合わせは録画に残さず、
/// 返事を受けたプログラムが名乗った状態を `Peek` / `List` で返す（protocol 15）。
#[cfg(unix)]
#[tokio::test]
async fn berthd_answers_the_osc_7501_probe_and_remembers_the_status() {
    let f = common::Fixture::start("osc7501");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("agent".into()),
            name: None,
            shell: Some("sh".into()),
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

    // エージェントの代役: 問い合わせを出し、返事（10 バイト）が届いたときだけ名乗る。
    // 誰も attach していないので、答えられるのは berthd だけ。
    let script = concat!(
        "stty raw -echo; printf '\\033]7501;?\\007'; dd bs=1 count=10 >/dev/null 2>&1; stty sane; ",
        "printf '\\033]7501;state=blocked:kind=permission:app=stand-in\\007'; echo DONE_7501\r"
    );
    send_ctrl(
        &mut c,
        2,
        &Control::SendKeys {
            session_id: "agent".into(),
            data: script.as_bytes().to_vec(),
            force: false,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut c).await,
        Control::SendKeysAck { .. }
    ));

    let mut seq = 3;
    let mut seen = None;
    for _ in 0..50 {
        send_ctrl(
            &mut c,
            seq,
            &Control::Peek {
                session_id: "agent".into(),
                max_bytes: None,
            },
        )
        .await;
        seq += 1;
        let (data_len, status) = match read_ctrl(&mut c).await {
            Control::PeekResult {
                data_len,
                program_status,
                ..
            } => (data_len, program_status),
            other => panic!("expected PeekResult, got {other:?}"),
        };
        let bytes = if data_len > 0 {
            loop {
                let frame = read_frame(&mut c).await.unwrap();
                if frame.kind == KIND_DATA {
                    break frame.payload;
                }
            }
        } else {
            Vec::new()
        };
        if let Some(st) = status {
            seen = Some((st, bytes));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (st, bytes) = seen.expect("the stand-in never got a reply (no program_status)");
    assert_eq!(st.state, "blocked");
    assert_eq!(st.kind.as_deref(), Some("permission"));
    assert_eq!(st.app.as_deref(), Some("stand-in"));
    // 問い合わせは録画に残らない（名乗りは残る・端末へも流れる）。
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(!has(b"\x1b]7501;?\x07"), "the probe leaked into the replay");
    assert!(
        has(b"\x1b]7501;state=blocked"),
        "the report should pass through"
    );

    // `List` も同じ状態を返す。
    send_ctrl(&mut c, seq, &Control::List).await;
    match read_ctrl(&mut c).await {
        Control::Sessions { sessions } => {
            let s = sessions.iter().find(|s| s.id == "agent").unwrap();
            assert_eq!(
                s.program_status.as_ref().map(|p| p.state.as_str()),
                Some("blocked")
            );
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    // `berth ls --json`（スクリプトが読む口）も同じ状態を返す。
    let doc = berth_ls_json(&f.endpoint);
    assert_eq!(doc["berthd_running"], true);
    let row = doc["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "agent")
        .expect("the session is in ls --json");
    assert_eq!(row["program_status"]["state"], "blocked");
    assert_eq!(row["program_status"]["kind"], "permission");
}

/// `berth ls --json` を本物の CLI で撃つ。`BERTHD_NO_SPAWN` を立てる（居なくても起こさない）。
#[cfg(unix)]
fn berth_ls_json(endpoint: &std::ffi::OsStr) -> serde_json::Value {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["ls", "--json"])
        .env("BERTHD_SOCKET", endpoint)
        .env("BERTHD_NO_SPAWN", "1")
        .output()
        .expect("spawn berth");
    assert!(
        o.status.success(),
        "berth ls --json failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("berth ls --json prints one JSON document")
}

/// berthd が居なければ、`berth ls --json` は起こさずに「居ない」と言い、rc 0 で終わる。
#[cfg(unix)]
#[test]
fn ls_json_says_when_no_berthd_is_running() {
    let doc = berth_ls_json(berthd::scratch_endpoint("ls-json-absent").as_os_str());
    assert_eq!(doc["berthd_running"], false);
    assert_eq!(doc["sessions"].as_array().map(Vec::len), Some(0));
}
