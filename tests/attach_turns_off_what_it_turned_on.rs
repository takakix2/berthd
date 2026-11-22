//! **attach で点いた端末の mode が、detach で消えているか。**
//!
//! # なぜ検体が要るか: 点ける出所が 2 つ在る
//!
//! `berth` の後始末（`src/bin/berth/attach/replay.rs`）の一覧は「中のシェルが点けた物」（bracketed paste など）
//! から作られやすい。ところが端末へ届く `?…h` にはもう 1 つの出所（**ConPTY の前口上**:
//! `?9001h` win32-input-mode / `?1004h` フォーカス通知）が在る。`filter` はそれを素通しする。
//!
//! ここは**自分が端末になって**（pty を張り、その中で `berth attach` を起こす）、
//! 書かれたバイトを全部溜め、点いた物と消えた物を棚卸しする。
//! 画面は見ない（見るのはバイト）。
#![cfg(windows)]

use berthd::Control;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{send_ctrl, wait_for_bytes as wait_for, wait_for_daemon, PtyChildGuard};

/// `ESC[?<params><h|l>` を全部拾って `(params, on)` にする。
/// `?1049;1h` のように `;` で複数が来る綴りが在るので割る。
fn modes(bytes: &[u8]) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' && bytes[i + 2] == b'?' {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'h' || bytes[j] == b'l') {
                let on = bytes[j] == b'h';
                for p in String::from_utf8_lossy(&bytes[i + 3..j]).split(';') {
                    if !p.is_empty() {
                        out.push((p.to_string(), on));
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[tokio::test]
async fn every_mode_attach_turns_on_is_turned_off_again() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let endpoint = berthd::scratch_endpoint("modes");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    drop(wait_for_daemon(&endpoint, Duration::from_secs(5)).await);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.arg("attach");
    // この検体は**部屋の形**（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるため環境で結果が割れる。そこで、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("modes-demo");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 利用者の本物の `~/.berth/config.toml` は読まない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );

    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);

    // 注意: 番人が DSR に答える（答えないと ConPTY は 20 バイトで止まる）。詳しくは `common` の doc。
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    // 陽性対照: 棚卸しの前に、attach が成立したことを見る。
    let attached = wait_for(
        &buf,
        b"\x1b]2746;attach;modes-demo\x1b\\",
        Duration::from_secs(10),
    )
    .await;

    let mut ctl = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut ctl,
        1,
        &Control::Detach {
            session_id: "modes-demo".into(),
        },
    )
    .await;
    // 後始末まで見たいので、detach 信号（列の**末尾**）が出るまで待つ。
    let detached = wait_for(&buf, b"\x1b]2746;detach\x1b\\", Duration::from_secs(10)).await;

    drop(_child);
    assert_eq!(
        terminal.close(pair.master),
        common::PtyClose::Returned,
        "pty を閉じられず期限へ落ちた"
    );

    let bytes = buf.lock().unwrap().clone();
    assert!(
        attached,
        "attach が成立していない ＝ 何も測れていない（{} bytes）",
        bytes.len()
    );
    assert!(
        detached,
        "detach の信号が出ていない ＝ 後始末まで見えていない（{} bytes）",
        bytes.len()
    );

    let seen = modes(&bytes);
    let on: Vec<&str> = seen
        .iter()
        .filter(|(_, o)| *o)
        .map(|(p, _)| p.as_str())
        .collect();
    let off: Vec<&str> = seen
        .iter()
        .filter(|(_, o)| !*o)
        .map(|(p, _)| p.as_str())
        .collect();
    let leaked: Vec<&&str> = on.iter().filter(|p| !off.contains(p)).collect();
    eprintln!("[modes] 点いた: {on:?}");
    eprintln!("[modes] 消えた: {off:?}");
    eprintln!("[modes] 点いたまま: {leaked:?}");

    // 注意: この計器は素通しではない。
    //
    // Windows では `portable-pty` が張るのも ConPTY なので、`berth` の書いたバイトは
    // conhost に一度解釈され、描き直されてからここへ届く（`berth` が 1 度も書かない `ESC[K` が
    // 捕まえた列に大量に居る）。∴ ここで見ているのは berth の生の出力ではない。
    //
    // 実害: `?9001l`（win32-input-mode）と `?1004l`（フォーカス通知 off）は、`berth` が同じ 1 回の
    // `write_all` で `?2004l` `?25h` `?1l` と一緒に書いているのに、届かないことが在る。どちらも
    // ConPTY のホスト自身の mode で、ホストが自分で持っていて飲む。飲む mode の集合はホストで違う
    // （素の conhost は両方飲む。`?9001` を素通しするホストも在る）。
    //
    // ∴ 主張を `9001` / `1004` へは広げない。これは「消してよい」ではなく、
    //    この面のこの計器では見えないという意味であり、後始末から外せば黙って戻る。
    //    berth が**書いているか**は `src/bin/berth/attach/replay.rs` の `DETACH_RESET` の単体検体が釘付けする
    //    （ConPTY を挟まない）。ホストの外で**消えたか**は実端末でしか測れない。
    let blind: [&str; 2] = ["1004", "9001"];
    let leaked: Vec<&&str> = leaked.into_iter().filter(|p| !blind.contains(p)).collect();
    assert!(
        leaked.is_empty(),
        "🚨 attach が点けた端末 mode が detach で消えていない: {leaked:?}\n\
         点いた: {on:?}\n消えた: {off:?}\n\
         ⭐ `?9001` = win32-input-mode / `?1004` = フォーカス通知（ConPTY の前口上）。\n\
         後始末の列は src/bin/berth/attach.rs の detach 直前。"
    );
}
