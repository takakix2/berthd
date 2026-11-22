//! berthd 統合スモーク — 永続性コアを実バイナリで実証する。
//!
//! spawn → attach → 入力 → 出力 → **切断（GUI 死を模擬）→ 再接続 → attach → replay** で
//! 「セッションが GUI の寿命を跨いで生き、再アタッチでバッファから画面が戻る」ことを確認する。
//! 実 PTY + 実シェルを使うので、出力待ちは固定 sleep でなく marker が出るまでの poll で吸収する。

use berthd::{decode_control, write_frame, Control, Frame, KIND_DATA};
// 繋ぐ口。端点の綴りは面ごとに違うので、検体は `connect_daemon_at` に任せる。
use berthd::{connect_daemon_at, DaemonStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MARKER: &[u8] = b"BERTHD_OK";
// DSR の綴り（`DSR_REQUEST`）は `common` に在る。

mod common;
use common::{
    become_a_terminal, read_ctrl, read_frame_within, send_ctrl, PtyChildGuard, DSR_REQUEST,
};

/// 端末の制御シーケンスを取り除く（CSI: `ESC [ … 終端`、OSC: `ESC ] … BEL` か `ESC \\`、
/// その他の 2 文字の `ESC x`）。
///
/// 注意: 出力を行に割る前に必ず通す。シェルは出力の手前に自分の印を挟み、その後に区切りを
/// 置くとは限らない: bash は bracketed paste を切る `ESC[?2004l`、hsh は OSC 133;C
/// （「ここからコマンドの出力」）を `\r` 無しで 1 行目に直に付ける。どちらも 1 行目が数字でない片に埋まる。
/// 区切りを 1 つずつ足すと次のシェルの印でまた外れるので、印そのものを落とす。
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.next() {
            // CSI: 引数と中間の文字の後、0x40..=0x7e の 1 文字で終わる。
            Some('[') => {
                for d in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
            // OSC: BEL か ST（ESC \\）で終わる。
            Some(']') => {
                while let Some(d) = it.next() {
                    if d == '\u{7}' {
                        break;
                    }
                    if d == '\u{1b}' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            // その他の `ESC x`（`ESC 7` / `ESC 8` など）は 2 文字で終わる。
            _ => {}
        }
    }
    out
}

/// 実物の 1 行目の手前（bash・hsh・題名を出すプロンプト）で釘付けする。
#[test]
fn the_first_line_survives_the_shells_own_marks() {
    let bash = "\u{1b}[?2004l\r1\r\n2\r\n";
    let hsh = "\u{1b}[J\r\n\u{1b}[?2004l\u{1b}]133;C\u{1b}\\1\r\n2\r\n";
    let title = "\u{1b}]0;user@host: ~\u{7}\u{1b}[01;32muser\u{1b}[00m$ \u{1b}7\u{1b}81\r\n";
    for sample in [bash, hsh, title] {
        let lines: Vec<String> = strip_escapes(sample)
            .split(['\n', '\r'])
            .map(str::to_owned)
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with('1') && l.trim_start_matches("user$ ").trim() == "1"),
            "1 行目が埋まった: {sample:?} → {lines:?}"
        );
    }
    // 対照: 落とさなければ hsh の 1 行目は数字でない片に埋まる。
    assert!(!hsh.split(['\n', '\r']).any(|l| l == "1"));
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// marker が出るまで（or timeout）フレームを読み、データ payload を集めて返す。
async fn drain_data_until(s: &mut DaemonStream, needle: &[u8], within: Duration) -> Vec<u8> {
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
            _ => break, // timeout または EOF
        }
    }
    acc
}

/// 端点が消えたことを、面ごとの言い方で確かめる。
///
/// | 面 | 端点の正体 | 消えたとは |
/// |---|---|---|
/// | unix | ファイルシステム上の socket | ファイルが無くなる（消し忘れると次の bind が詰まる）|
/// | Windows | 名前空間の名前付きパイプ | プロセスが死ねば消える（ファイルは元から無い） |
///
/// ∴ unix は後始末の検査、Windows は到達不能の検査になる。
/// `!path.exists()` を両面で使わないこと（Windows では常に真で、何も測らない）。
async fn endpoint_is_gone(endpoint: &std::ffi::OsStr) -> bool {
    if connect_daemon_at(endpoint).await.is_ok() {
        return false;
    }
    #[cfg(unix)]
    {
        // 繋がらないだけでは足りない（消し忘れた socket ファイルが残る形が在る）。
        !std::path::Path::new(endpoint).exists()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// シェルに「1 行打つ」ときのバイト列（面ごとの行末）。
///
/// Windows のシェルは `\n` でも `\r` でも実行しない。`\r\n` で初めて確定する。
/// 注意: `\n` だけでも打った字は画面に返るので、「入力が届いていない」ようには見えない。
/// 届いていて確定だけされず、「シェルが応答しない」と同じ顔で失敗する。
///
/// 行末を検体ごとに書かないこと（面の規則はここ 1 箇所）。
fn line(cmd: &str) -> Vec<u8> {
    let mut v = cmd.as_bytes().to_vec();
    #[cfg(unix)]
    v.extend_from_slice(b"\n");
    #[cfg(not(unix))]
    v.extend_from_slice(b"\r\n");
    v
}

/// デーモンが待ち始めるまで待って繋ぐ。端点の綴りは知らない（面の知識は `connect_daemon_at` の中だけ）。
async fn wait_for_daemon(endpoint: &std::ffi::OsStr, within: Duration) -> DaemonStream {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Ok(s) = connect_daemon_at(endpoint).await {
            return s;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("berthd never came up at {}", endpoint.to_string_lossy());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Windows では、`List` の cwd が「今の値ではない」ことを `cwd_src` が名乗ること。
///
/// `live_cwd_with_src()`（`src/bin/berthd/session.rs`）は linux / macos の腕しか持たず、それ以外は
/// `None` を返す。∴ Windows では cd しても `List` の cwd は起動時の値のままになる（`/tmp` も無い）。
/// 値は測れないが、その不在を `cwd_src` が `spawn:unimplemented:live_cwd` と名乗ることは測れる。
///
/// Windows の腕は書かないと決めてある。この検体はその決めを釘付けしている。
/// assert を緩めないこと。腕が入ればここが赤くなるので、そのときは unix 側と同じ形
/// （cd して追う）へ直す。
#[cfg(not(unix))]
#[tokio::test]
async fn list_names_the_missing_cwd_mechanism_instead_of_faking_a_live_value() {
    let endpoint = berthd::scratch_endpoint("cwdsrc");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("cwdsrc-tab".into()),
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

    send_ctrl(&mut c, 2, &Control::List).await;
    let Control::Sessions { sessions } = read_ctrl(&mut c).await else {
        panic!("List must answer Sessions");
    };
    let s = sessions
        .iter()
        .find(|s| s.id == "cwdsrc-tab")
        .expect("the session we just spawned must be listed");

    assert_eq!(
        s.cwd_src, "spawn:unimplemented:live_cwd",
        "この面には live_cwd の実装が無いのだから、一覧はそう名乗ること（cwd = {}）",
        s.cwd
    );
    // 陽性対照の相方: 名乗りが空だと「古い berthd」と見分けが付かない。
    assert!(
        !s.cwd_src.is_empty(),
        "空の cwd_src は「古い berthd」の意味なので、今の berthd が出してはいけない"
    );
}

/// `List` の cwd が、子シェルの今いる場所を返すこと（spawn 時の記録ではなく）。
/// これが無いと `berth ls` の CWD 列が起動場所のまま固まり、detached session を見分けられない。
#[cfg(unix)]
#[tokio::test]
async fn list_reports_the_shells_current_cwd_not_the_spawn_cwd() {
    let endpoint = berthd::scratch_endpoint("cwd");

    // `let _ =` にしないこと。`ChildGuard` は `Drop` で子を殺すので、その場で落ちて
    // デーモンを即座に殺す。名前を付けて生かす。
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("cwd-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None, // = berthd の cwd（テストランナーの cwd）で起動する
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
            session_id: "cwd-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    // シェルを別ディレクトリへ移動させる。/tmp は macOS では /private/tmp への symlink なので、
    // 期待値はカーネルが返す実パス（canonicalize）で持つ。
    let want = std::fs::canonicalize("/tmp").unwrap().display().to_string();
    write_frame(&mut c, &Frame::data(0, b"cd /tmp\n".to_vec()))
        .await
        .unwrap();

    // `cd` は無出力なので marker では待てない。cwd が変わるまで List を poll する（固定 sleep 回避）。
    let mut list = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got = String::new();
    let mut got_src = String::new();
    let mut serial = 10;
    while tokio::time::Instant::now() < deadline {
        send_ctrl(&mut list, serial, &Control::List).await;
        serial += 1;
        if let Control::Sessions { sessions } = read_ctrl(&mut list).await {
            if let Some(s) = sessions.iter().find(|s| s.id == "cwd-tab") {
                got = s.cwd.clone();
                got_src = s.cwd_src.clone();
                if got == want {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        got, want,
        "List must report the shell's live cwd after it cd'd"
    );
    // 値が合っているだけでは足りない。どうやって得たかを名乗ること。
    // 面ごとの機構名を直書きする（`live_cwd_with_src` の値表と対）。
    let want_src = if cfg!(target_os = "linux") {
        "proc"
    } else {
        "proc_pidinfo"
    };
    assert_eq!(
        got_src, want_src,
        "live な cwd を返したのなら、cwd_src はその機構を名乗ること"
    );
}

/// 名前の層: 既定名が付き、`Rename` で変えられ、**重複名は拒否**される。
/// 名前の存在理由は「名前で attach できること」なので、重複を許すと handle として壊れる。
#[tokio::test]
async fn sessions_get_default_names_and_rename_rejects_duplicates() {
    let endpoint = berthd::scratch_endpoint("name");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    for (serial, id) in [(1u32, "tab-a"), (2, "tab-b")] {
        send_ctrl(
            &mut c,
            serial,
            &Control::Spawn {
                session_id: Some(id.into()),
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
    }

    // 既定名 = session-N（id とは別系統の採番）。
    send_ctrl(&mut c, 10, &Control::List).await;
    let names: Vec<String> = match read_ctrl(&mut c).await {
        Control::Sessions { sessions } => sessions.iter().map(|s| s.name.clone()).collect(),
        other => panic!("expected Sessions, got {other:?}"),
    };
    assert!(
        names.iter().all(|n| n.starts_with("session-")),
        "default names: {names:?}"
    );

    // rename できる。
    send_ctrl(
        &mut c,
        11,
        &Control::Rename {
            session_id: "tab-a".into(),
            name: "iris".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Renamed { .. }));

    // 同じ名前を別 session に付けるのは拒否（名前で引けなくなるため）。
    send_ctrl(
        &mut c,
        12,
        &Control::Rename {
            session_id: "tab-b".into(),
            name: "iris".into(),
        },
    )
    .await;
    match read_ctrl(&mut c).await {
        Control::Error { code, .. } => assert_eq!(code, "bad_name"),
        other => panic!("duplicate name must be rejected, got {other:?}"),
    }

    // 空白入りも拒否（handle として打てないため）。
    send_ctrl(
        &mut c,
        13,
        &Control::Rename {
            session_id: "tab-b".into(),
            name: "my session".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Error { .. }));

    // 自分自身の名前で rename し直すのは重複ではない（no-op として通る）。
    send_ctrl(
        &mut c,
        14,
        &Control::Rename {
            session_id: "tab-a".into(),
            name: "iris".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Renamed { .. }));
}

/// `KillServer` でデーモンが本当に死ぬこと（＋ socket を残さないこと）。
/// berthd は自分からは落ちない設計なので、これが唯一の畳む手段。socket が残ると次の berthd が
/// 「生きた berthd がいる」と誤認して譲ってしまう（stale socket は起動時に掃除されるが、
/// 掃除は接続失敗を確かめてからなので、消しておく方が素直）。
#[tokio::test]
async fn kill_server_stops_the_daemon_and_removes_the_socket() {
    let endpoint = berthd::scratch_endpoint("killsrv");

    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("doomed".into()),
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

    send_ctrl(&mut c, 2, &Control::KillServer).await;

    // デーモンが終了すること（応答は待たない: EOF/exit が合図）。
    let status = tokio::task::spawn_blocking(move || daemon.wait())
        .await
        .unwrap()
        .expect("wait berthd");
    assert!(
        status.success(),
        "berthd should exit cleanly on kill-server"
    );
    assert!(
        endpoint_is_gone(&endpoint).await,
        "kill-server must leave nothing behind at its endpoint"
    );
}

/// kill-server は、出る前に session を畳むこと。
///
/// 畳まずに `std::process::exit` するとデストラクタが走らず、各 session の `master`（Windows では
/// ConPTY）が一度も drop されない。Windows ではそのとき、セッションごとに ConPTY のホスト
/// （`OpenConsole.exe`）が孤児になって CPU を回し続ける。
///
/// 注意: この検体が測るのは「畳む工程が走ったこと」だけで、漏れないことではない。
/// unix では畳まなくても子は死ぬ（プロセス終了で OS が片付ける）ので、「子が生きているか」を
/// 見る検体は恒真になる。本物の性質（ConPTY が閉じる）は Windows でしか測れない
/// （`tests/kill_server_folds_the_console_host.rs`）。ここの緑は「Windows で漏れていない」ではない。
#[cfg(unix)]
#[tokio::test]
async fn kill_server_closes_sessions_before_it_exits() {
    use std::io::Read as _;
    let endpoint = berthd::scratch_endpoint("teardown");

    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    let mut err = daemon.stderr.take().expect("stderr");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("teardown-check".into()),
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

    send_ctrl(&mut c, 2, &Control::KillServer).await;
    let status = tokio::task::spawn_blocking(move || daemon.wait())
        .await
        .unwrap()
        .expect("wait berthd");
    assert!(status.success());

    let mut log = String::new();
    err.read_to_string(&mut log).ok();
    assert!(
        log.contains("kill-server: closing 1 session"),
        "🚨 畳む工程が走っていません。`std::process::exit` の手前で session を閉じないと、\n\
         Windows では ConPTY のホストが 100% CPU で孤児になります。\n\
         --- berthd の stderr ---\n{log}"
    );
    assert!(
        log.contains("kill-server: all sessions closed"),
        "🚨 畳み終える前に出ています（待たずに exit している）。\n--- stderr ---\n{log}"
    );
}

/// 台帳の 1 行を書くために、pty を起こした直後に子プロセスを起こしていないか。
///
/// `hostname()` は env が空だと最後に外部コマンドを起こす。その最初の 1 回が `openpty` の直後に
/// 落ちると、Windows ではその session のシェルが 1 バイトも出さなくなることがある。
///
/// 陽性対照は環境で作る。`HOSTNAME` を持つ環境でも同じ道を通すために、daemon の `HOSTNAME` と
/// `COMPUTERNAME` を意図的に空にする（`hostname()` は空を弾くので、最後の手段まで落ちる）。
/// 空にしないと、`HOSTNAME` を持つ環境ではこの検体は何も測らずに緑になる。
///
/// これは「子を起こすな」の検体ではなく「pty が黙るな」の検体である（直し方は縛らない）。
#[tokio::test]
async fn the_ledger_does_not_silence_the_pty_when_the_host_name_must_be_looked_up() {
    let f = common::Fixture::start_with_env("nohost", &[("HOSTNAME", ""), ("COMPUTERNAME", "")]);
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("nohost-tab".into()),
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
            session_id: "nohost-tab".into(),
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

    // 打つ前に端末になる（ConPTY は DSR に答えるまで進まない・`become_a_terminal` の doc）。
    let preamble = become_a_terminal(&mut c, Duration::from_secs(20)).await;
    write_frame(&mut c, &Frame::data(0, line("echo BERTHD_OK")))
        .await
        .unwrap();
    let out = drain_data_until(&mut c, MARKER, Duration::from_secs(5)).await;
    assert!(
        contains(&out, MARKER),
        "🚨 台帳の行を書く道が pty を黙らせている。
         ⭐ 直す前は、この形で**前口上すら 20s 来なかった**。
         attach 直後に見えた分: {}
         打ったあとに見えた分: {}",
        String::from_utf8_lossy(&preamble),
        String::from_utf8_lossy(&out)
    );
}

/// GUI の warm リロード（プロセス生存のまま WebView だけ再読込）を模す:
/// 新しい接続が同じ session へ attach した **後** に、古い接続が閉じる。
/// 古い接続の後始末が新しい client の sink を巻き添えにすると、replay は出るのに
/// 以後の live 出力が無音になる。ここではその回帰を止める。
#[tokio::test]
async fn stale_client_close_does_not_kill_the_new_attachment() {
    let endpoint = berthd::scratch_endpoint("warm");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut old = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut old,
        1,
        &Control::Spawn {
            session_id: Some("warm-tab".into()),
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
    assert!(matches!(read_ctrl(&mut old).await, Control::Spawned { .. }));
    send_ctrl(
        &mut old,
        2,
        &Control::Attach {
            session_id: "warm-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    // 打つ前に端末になる（ConPTY は DSR に答えるまで進まない・`become_a_terminal` の doc）。
    become_a_terminal(&mut old, Duration::from_secs(20)).await;
    write_frame(&mut old, &Frame::data(0, line("echo BERTHD_OK")))
        .await
        .unwrap();
    assert!(contains(
        &drain_data_until(&mut old, MARKER, Duration::from_secs(5)).await,
        MARKER
    ));

    // 新しい client が（古い接続が生きているうちに）同じ session へ attach する。
    let mut fresh = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut fresh,
        1,
        &Control::Attach {
            session_id: "warm-tab".into(),
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
        read_ctrl(&mut fresh).await,
        Control::Attached { .. }
    ));

    // ここで古い接続が閉じる（frontend が古い ClientSession を捨てるのと同じ順序）。
    old.shutdown().await.ok();
    drop(old);
    tokio::time::sleep(Duration::from_millis(300)).await; // berthd が EOF を処理し切るのを待つ

    // 新しい client の live 出力が生きていること（sink を奪われていない）。
    // 打つ前に端末になる（ConPTY は DSR に答えるまで進まない・`become_a_terminal` の doc）。
    become_a_terminal(&mut fresh, Duration::from_secs(20)).await;
    write_frame(&mut fresh, &Frame::data(0, line("echo BERTHD_LIVE")))
        .await
        .unwrap();
    let live = drain_data_until(&mut fresh, b"BERTHD_LIVE", Duration::from_secs(5)).await;
    assert!(
        contains(&live, b"BERTHD_LIVE"),
        "the surviving client must keep receiving live output after the stale one closes; got {} bytes",
        live.len()
    );
}

// ── 同じ session に 2 本目の attach が来たら、1 本目は全部を手放す ──────────────
//
// 形は tmux の `attach -d`: 後から来た側が出力・打鍵・`Resize` の全部を持っていく。
// PTY は 1 本のバイト列なので、2 つの端末から打鍵が届くと同じ行に混ざったまま Enter で実行されうる。
// 前の client には `Detached`（外部 `berth detach` と同じ口）が届き、`berth` は
// `[detached from …]` を出して抜ける。見るだけの接続（`read_only`）は別の話。

/// A が attach している session へ B が attach した状態を作る（どちらも `Attached` を読み終えてある）。
async fn a_replaced_by_b(
    tag: &str,
    b_cols: u16,
    b_rows: u16,
) -> (
    common::ChildGuard,
    std::ffi::OsString,
    DaemonStream,
    DaemonStream,
    String,
) {
    let endpoint = berthd::scratch_endpoint(tag);
    let daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    let session_id = format!("{tag}-tab");

    let mut a = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut a,
        1,
        &Control::Spawn {
            session_id: Some(session_id.clone()),
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
    assert!(matches!(read_ctrl(&mut a).await, Control::Spawned { .. }));
    send_ctrl(
        &mut a,
        2,
        &Control::Attach {
            session_id: session_id.clone(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut a).await, Control::Attached { .. }));
    become_a_terminal(&mut a, Duration::from_secs(20)).await;

    let mut b = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut b,
        1,
        &Control::Attach {
            session_id: session_id.clone(),
            cols: b_cols,
            rows: b_rows,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut b).await, Control::Attached { .. }));
    become_a_terminal(&mut b, Duration::from_secs(20)).await;

    (daemon, endpoint, a, b, session_id)
}

#[tokio::test]
async fn a_replaced_attachment_is_told_it_was_detached() {
    let (_daemon, _endpoint, mut a, _b, session_id) = a_replaced_by_b("told", 80, 24).await;
    let told = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Control::Detached { session_id, .. } = read_ctrl(&mut a).await {
                return session_id;
            }
        }
    })
    .await;
    assert_eq!(
        told.ok(),
        Some(session_id),
        "🚨 置き換えられた 1 本目に何も知らされていない —— 画面が止まるだけで `berth` は抜けない"
    );
}

/// unix 専用。打つのは `printf`。Windows の既定シェル（powershell）の綴りでは書いていない。
#[cfg(unix)]
#[tokio::test]
async fn a_replaced_attachment_can_no_longer_type() {
    let (_daemon, _endpoint, mut a, mut b, _session_id) = a_replaced_by_b("typing", 80, 24).await;
    // 目印は実行した結果にだけ出る形で打つ（`A_%s` ＋ `LEAK` → `A_LEAK`）。
    // 打った字の echo に目印が出ると、「届いた」と「実行された」が区別できない。
    write_frame(&mut a, &Frame::data(0, line("printf 'A_%s\\n' LEAK")))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(500)).await;
    write_frame(&mut b, &Frame::data(0, line("printf 'B_%s\\n' LIVE")))
        .await
        .unwrap();
    let out = drain_data_until(&mut b, b"B_LIVE", Duration::from_secs(5)).await;
    let text = String::from_utf8_lossy(&out);
    assert!(
        contains(&out, b"B_LIVE"),
        "陽性の対照: 今の持ち主（B）の打鍵は実行されるはず: {text:?}"
    );
    assert!(
        !contains(&out, b"A_LEAK"),
        "🚨 置き換えられた A の打鍵が PTY に届いて実行された: {text:?}"
    );
}

/// unix 専用。大きさは `stty size` で読む。
#[cfg(unix)]
#[tokio::test]
async fn only_the_current_attachment_resizes_the_session() {
    let (_daemon, endpoint, mut a, mut b, session_id) = a_replaced_by_b("size", 100, 30).await;
    // 置き換えられた A と、attach していない C の両方から撃つ（赤のときどちらが効いたか読めるよう大きさを変える）。
    send_ctrl(
        &mut a,
        9,
        &Control::Resize {
            session_id: session_id.clone(),
            cols: 40,
            rows: 10,
        },
    )
    .await;
    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Resize {
            session_id: session_id.clone(),
            cols: 41,
            rows: 11,
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    write_frame(
        &mut b,
        &Frame::data(0, line("printf 'SZ[%s]SZ_%s\\n' \"$(stty size)\" END")),
    )
    .await
    .unwrap();
    let out = drain_data_until(&mut b, b"SZ_END", Duration::from_secs(5)).await;
    let text = String::from_utf8_lossy(&out);
    assert!(
        contains(&out, b"SZ_END"),
        "陽性の対照: `stty size` が返らない: {text:?}"
    );
    assert!(
        contains(&out, b"SZ[30 100]"),
        "🚨 今の持ち主（B・100x30）以外の Resize が効いた（A は 10 40 / C は 11 41）: {text:?}"
    );
}

#[tokio::test]
async fn session_survives_disconnect_and_replays_on_reattach() {
    // ユニークな socket path（他の berthd と衝突しない）。
    let endpoint = berthd::scratch_endpoint("smoke");

    // 実 berthd バイナリを起動（BERTHD_SOCKET でこのテスト用 socket を指定）。
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    // ── client 1: spawn → attach → 入力 → 出力確認 ──
    let session_id = {
        let mut c1 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;

        send_ctrl(
            &mut c1,
            1,
            &Control::Hello {
                client: "smoke/1".into(),
            },
        )
        .await;
        assert!(matches!(read_ctrl(&mut c1).await, Control::Welcome { .. }));

        // client 指定 id で spawn（frontend タブ id を berthd key にする reattach 経路）。
        send_ctrl(
            &mut c1,
            2,
            &Control::Spawn {
                session_id: Some("smoke-tab".into()),
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
        assert_eq!(
            id, "smoke-tab",
            "berthd must honor the client-specified session id"
        );

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

        // MARKER をシェルに打つ（PTY エコー + 実行結果に "BERTHD_OK" が出る）。
        // 打つ前に端末になる（ConPTY は DSR に答えるまで進まない・`become_a_terminal` の doc）。
        become_a_terminal(&mut c1, Duration::from_secs(20)).await;
        write_frame(&mut c1, &Frame::data(0, line("echo BERTHD_OK")))
            .await
            .unwrap();

        let out = drain_data_until(&mut c1, MARKER, Duration::from_secs(5)).await;
        assert!(
            contains(&out, MARKER),
            "live output should contain the marker; got {} bytes",
            out.len()
        );

        // client 1 を落とす（= GUI 死）。session は berthd の下で生き続けるはず。
        c1.shutdown().await.ok();
        drop(c1);
        id
    };

    // 少し待って detach が反映されるのを許容。
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── client 2: 再接続 → 同じ session に attach → replay に MARKER が戻る ──
    {
        let mut c2 = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;

        // list に session が生存していること。
        send_ctrl(&mut c2, 10, &Control::List).await;
        match read_ctrl(&mut c2).await {
            Control::Sessions { sessions } => {
                assert!(
                    sessions.iter().any(|s| s.id == session_id && s.alive),
                    "session {session_id} must still be alive after client 1 died"
                );
            }
            other => panic!("expected Sessions, got {other:?}"),
        }

        send_ctrl(
            &mut c2,
            11,
            &Control::Attach {
                session_id: session_id.clone(),
                cols: 80,
                rows: 24,
                actor: None,
                actor_src: None,
                read_only: false,
                seen_total: None,
            },
        )
        .await;

        // 追加入力なしで、replay バッファだけから MARKER が戻ることを確認（= 永続性の核）。
        let replay = drain_data_until(&mut c2, MARKER, Duration::from_secs(5)).await;
        assert!(
            contains(&replay, MARKER),
            "reattach replay must reconstruct earlier output (persistence); got {} bytes",
            replay.len()
        );

        // replay は録画なので、過去の「問い」まで再生される。`sanitize_replay` の規則は
        // 「live 出力は素通し。過去の再生だけ黙らせる」。
        // 注意: DSR（`ESC[6n`）が再生されると端末が答えてしまい、その応答（`ESC[1;1R`）はシェルの
        // stdin へ行く。余計な行が増えるだけでなく、打鍵と競合すると入力が黙って書き換わる
        // （`echo PROBE_OK` が `Recho PROBE_OK` になる）。
        //
        // 注意: unix ではこの assert は空振りする（緑でも何も証明しない）。`ESC[6n` を録画の先頭へ
        // 入れるのは ConPTY の前口上（`tests/common/mod.rs` の doc）で、unix の PTY は出さない。
        // Windows で走らせて初めて意味を持つ。
        //
        // `become_a_terminal` が送った応答（`ESC[1;1R`）は入力なので録画には入らない。
        // ここが見ているのは問いの側だけ。
        assert!(
            !contains(&replay, DSR_REQUEST),
            "replay must not re-ask the terminal for its cursor: the answer lands in the \
             shell's stdin and corrupts whatever the user is typing; got {} bytes",
            replay.len()
        );
    }

    // 後始末。
}

/// `berth`（CLI）が **PTY の中から自分を名乗れる**こと。
///
/// これが `berth` を独立バイナリにした理由そのもの: PTY ペイン（hsh でも zsh でも）から
/// セッションを触れないと、一番見たい時（作業中）に見られない。berthd が子へ渡す
/// `BERTHD_SESSION` を CLI が読み、`ls` の `*` と 引数なし `rename`（自分自身）が成立する。
///
/// 注意: 子シェルの中で実際に `berth` を打つ代わりに、**子と同じ env を与えて CLI を直接実行**する
/// （PTY 越しにプロンプトを待って文字を流し込むのは脆く、ここで守りたい性質はそこではない）。
#[tokio::test]
async fn argo_cli_marks_and_renames_its_own_session_from_inside_the_pty() {
    let endpoint = berthd::scratch_endpoint("cli");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("cli-tab".into()),
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

    // 子プロセスと同じ env（BERTHD_SOCKET + BERTHD_SESSION）で `berth ls` を実行する。
    let berth = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_berth"))
            .args(args)
            .env("BERTHD_SOCKET", &endpoint)
            .env("BERTHD_SESSION", "cli-tab")
            .output()
            .expect("run berth")
    };

    let out = berth(&["ls"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "berth ls failed: {stdout}");
    assert!(
        stdout.starts_with('*'),
        "自分のセッションには * が付くこと（BERTHD_SESSION 由来）; got: {stdout}"
    );

    // 引数 1 つの rename は「自分」を指す（PTY の中から打つときの自然形）。
    let out = berth(&["rename", "mywork"]);
    assert!(
        out.status.success(),
        "berth rename failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&berth(&["ls"]).stdout).to_string();
    assert!(
        stdout.contains("mywork"),
        "改名が ls に出ること; got: {stdout}"
    );

    // 自分自身の kill は拒否する（自分の足元を撃たない）。
    let out = berth(&["kill", "mywork"]);
    assert!(!out.status.success(), "自分自身の kill は拒否されること");
}

/// 親（GUI アプリ）が死んで stderr が壊れた後でも `kill-server` で畳めること。
///
/// berthd はアプリの子として spawn されると stderr を継承する。アプリが死ぬとそのパイプは壊れ、
/// `eprintln!` は EPIPE で panic する。KillServer の処理が「子を殺す → ログ → socket 削除 → exit」の
/// 順だと、ログで panic してそのタスクだけ死に、daemon が生き残る（子は死ぬのに socket は残り、
/// 以後 `berth kill-server` が効かない）。すなわち、ログの失敗がシャットダウンを止める。
///
/// ここでは stderr を「読み手のいないパイプ」にして親を消し、壊れたパイプを再現する。
#[tokio::test]
async fn kill_server_works_even_when_stderr_is_a_broken_pipe() {
    use std::process::Stdio;

    let endpoint = berthd::scratch_endpoint("brokenpipe");

    // stderr = パイプ。読み手（このテスト側の handle）を即 drop して壊す。
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    drop(daemon.stderr.take()); // 読み手が消える → 以後 berthd の stderr 書き込みは EPIPE
    drop(daemon.stdout.take());

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    // セッションを 1 つ持たせる（kill-server が子を殺す経路を通す）。
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("bp-tab".into()),
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

    send_ctrl(&mut c, 2, &Control::KillServer).await;

    // socket が消える = daemon が本当に畳めた（壊れた stderr に阻まれていない）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if endpoint_is_gone(&endpoint).await {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "berthd は stderr が壊れていると kill-server で死ななかった（ログの失敗が制御フローを止めている）"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **外部 detach**: 別接続の `berth detach <name>` が、今 attach している client を蹴ること。
///
/// 対話 `berth attach`（tmux 型）で「抜ける」の土台。`berth d` は attach とは別の接続で `Detach` を送り、
/// berthd は attach 中の接続へ `Detached` フレームを流す → その client が raw モードを戻して離脱する。
/// 子（PTY）は殺さず detached で生存し、後で再 attach できる。
#[tokio::test]
async fn external_detach_kicks_the_attached_client_but_keeps_the_session() {
    let endpoint = berthd::scratch_endpoint("detach");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    // conn A = 「今 attach している client」（対話 `berth attach` 相当）。
    let mut a = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut a,
        1,
        &Control::Spawn {
            session_id: Some("det-tab".into()),
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
    assert!(matches!(read_ctrl(&mut a).await, Control::Spawned { .. }));
    send_ctrl(
        &mut a,
        2,
        &Control::Attach {
            session_id: "det-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut a).await, Control::Attached { .. }));

    // conn B = 外から `berth detach det-tab` を撃つ（応答は返らない撃ちっぱなし）。
    let mut b = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut b,
        1,
        &Control::Detach {
            session_id: "det-tab".into(),
        },
    )
    .await;

    // conn A は `Detached` を受け取る（replay 等の data フレームが先に混ざりうるので読み飛ばす）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let got_detached = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break false;
        }
        match read_frame_within(&mut a, remaining).await {
            Some(Ok(f)) if f.kind == berthd::KIND_CONTROL => {
                if matches!(decode_control(&f.payload), Ok(Control::Detached { .. })) {
                    break true;
                }
            }
            Some(Ok(_)) => continue, // data（replay）は無視
            _ => break false,        // timeout / EOF
        }
    };
    assert!(
        got_detached,
        "attached client must receive a Detached frame on external detach"
    );

    // detach は子を殺さない（session は生きたまま、後で再 attach できる）。
    send_ctrl(&mut b, 2, &Control::List).await;
    match read_ctrl(&mut b).await {
        Control::Sessions { sessions } => assert!(
            sessions.iter().any(|s| s.id == "det-tab" && s.alive),
            "detach must not kill the session"
        ),
        other => panic!("expected Sessions, got {other:?}"),
    }
}

/// OSC 2746 の attach / detach 信号を、実 `berth attach` が生バイトで吐くこと。
///
/// 発行元は `berth` の対話 attach、消費側は端末アプリの OSC ハンドラ。この信号が ssh 断時の
/// 「素の再接続 vs 元のセッションへ reattach」の出し分けを支える。信号は対話 attach（tty 必須）で
/// しか出ないので、protocol 直叩きでは踏めない。実 PTY の中で `berth attach` を起こして
/// その stdout を生バイトで捕まえる。
///
/// - attach 成立 → `ESC ] 2746 ; attach ; <handle> ESC \`
/// - 外部 detach → `ESC ] 2746 ; detach ESC \`
///
/// 終端は ST（`ESC \`）固定（BEL でない）。berthd の `sanitize_replay` が裸 BEL を落とすのと、
/// bell と紛れないため。

#[tokio::test]
async fn attach_and_detach_emit_the_osc_2746_signal() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let endpoint = berthd::scratch_endpoint("osc");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    // berthd が socket を bind し終える前に `berth attach` が繋ぐと "no berthd at …" で弾かれる
    // （BERTHD_SOCKET 明示により autostart しない）。他テストと同じく socket が上がるまで待つ。
    drop(wait_for_daemon(&endpoint, Duration::from_secs(5)).await);

    // 実 PTY を張って、その中で `berth attach osc-demo` を起こす（stdin が tty となり対話パスに入る）。
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // 親 env をそのまま継承させる（`berth attach` は SHELL 等を子セッションの spawn に使う）。
    // BERTHD_SESSION だけは除外する（自己 attach ガード（自分のセッションへ再 attach）に触れないため）。
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.arg("attach");
    // この検体は部屋の形（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるので、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("osc-demo");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );

    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave); // 子だけが slave を持つ（子 exit で master が EOF になる）

    // master を別スレッドで読み続け、共有バッファへ溜める（read は blocking なので runtime を塞がない）。
    // 注意: 読むだけでは足りない。ConPTY は PTY を開いた瞬間に DSR（`ESC[6n`）を問い返し、
    //    答えるまで先へ進まない。理由・面の囲い・綴りは `common::pump_pty_as_terminal` の doc に在る
    //    （ここへ写さないこと）。
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    // ① attach 信号（handle 付き）。marker と別れて画面に出ないのが本来だが、ここでは生 stdout を見る。
    let attach = b"\x1b]2746;attach;osc-demo\x1b\\";
    let got_attach = wait_for_bytes(&buf, attach, Duration::from_secs(5)).await;

    // 別接続から外部 detach（= berth detach）を撃つ。attach client は Detached を受けて信号を吐き exit。
    let mut ctl = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut ctl,
        1,
        &Control::Detach {
            session_id: "osc-demo".into(),
        },
    )
    .await;

    // ② detach 信号（引数なし: attach 状態を落とす）。
    let detach = b"\x1b]2746;detach\x1b\\";
    let got_detach = wait_for_bytes(&buf, detach, Duration::from_secs(5)).await;

    // 後始末は assert より前・kill ベース（join/wait は hang しうるので使わない）。
    // 素の `drop(master)` は、子が ConPTY の前口上で止まっていると返らないので、
    // 子を先に落としてから（`_child` をここで落とす）、番人に閉じさせる。
    drop(_child);
    // 結末を捨てない。閉じられずに期限へ落ちたなら、それを赤にする。
    assert_eq!(
        terminal.close(pair.master),
        common::PtyClose::Returned,
        "pty を閉じられず期限へ落ちた"
    );

    let nbytes = buf.lock().unwrap().len();
    let dump = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert!(
        got_attach,
        "attach must emit OSC 2746 attach; captured {nbytes} bytes: {dump:?}"
    );
    assert!(
        got_detach,
        "external detach must emit OSC 2746 detach; captured {nbytes} bytes: {dump:?}"
    );
}

/// 端点は利用者ごとに 1 本。先に取った方が勝ち、後から来た berthd は名乗って降りる。
///
/// 結末は 2 つ在り、rc が違うのは事情が違うから:
///
/// | 先客と | 何が起きるか | rc |
/// |---|---|---|
/// | 話せる（同じ session） | 「already running」と言って降りる（目的は既に果たされている） | `0` |
/// | 話せない（Windows の別 session） | 「not starting」（目的は果たされず、自分にも果たせない） | `≠0` |
///
/// 下の行はここでは測れない（別 session を作るには ssh が要る）。
/// 注意: 「降りたこと」だけを見ないこと（それだと両方死んでも緑になる）。
/// ∴ 先客が端点を持ち続けていることまで見る。
#[tokio::test]
async fn a_second_daemon_yields_to_the_first_instead_of_fighting_for_the_endpoint() {
    let endpoint = berthd::scratch_endpoint("yield");
    let _first = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    // 先客が待ち始めたことを確かめてから 2 人目を出す（順序が主題なので）。
    drop(wait_for_daemon(&endpoint, Duration::from_secs(5)).await);

    let second = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the second berthd");

    assert!(
        second.status.success(),
        "先客と話せるのだから、2 人目は成功として降りるべき: {second:?}"
    );
    let said = String::from_utf8_lossy(&second.stderr);
    // 黙って消えない（降りる理由を名乗る）。
    assert!(
        said.contains("already running"),
        "2 人目が降りた理由を名乗っていない: {said:?}"
    );

    // 先客が生きて端点を持ち続けていること。ここを見ないと、
    // 「2 人とも死んだ」も上の 2 つの assert を通ってしまう。
    let mut again = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(&mut again, 1, &Control::List).await;
    assert!(
        matches!(read_ctrl(&mut again).await, Control::Sessions { .. }),
        "先客が端点を持ち続けていない"
    );
}

/// 速い出力が 1 行も欠けないこと（出力の背圧）。
///
/// 「溢れても 1 バイトも落ちない」はユニットテストの層では測れない（`run_pty_io_loop` と実 PTY が
/// 要る）。このファイルには両方在るので、ここで測る。
///
/// # 指標は「行数」ではなく「番号の連続性」
///
/// 末尾は必ず着くので、行数や末尾マーカーでは完走に見えてしまう。抜けは途中で起きる。
/// ∴ `1..=N` が全部ちょうど 1 回ずつ出ることを見る。
///
/// # 注意: この検体は unix にしか無い
///
/// 生成器に `seq` を使っており、Windows の既定シェルには無い。Windows にはこの性質の検体が無い。
#[cfg(unix)]
#[tokio::test]
async fn a_fast_burst_loses_no_lines() {
    const N: usize = 20_000;
    const MARKER: &[u8] = b"TAIL-MARKER-OK";

    let endpoint = berthd::scratch_endpoint("burst");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("burst-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 200,
            rows: 50,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "burst-tab".into(),
            cols: 200,
            rows: 50,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    // 注意: 打った行の echo が自分のマーカーに当たる。シェルは打鍵をエコーするので、目印をそのまま
    // 打つと `drain_data_until` が出力の 1 行目より前に止まり、「全部抜けた」と読んでしまう。
    // 打つ側では割り（`TAIL-''MARKER-OK`）、出る側で繋がるようにする。
    let line = format!("seq 1 {N}; echo TAIL-''MARKER-OK\n");
    write_frame(&mut c, &Frame::data(0, line.into_bytes()))
        .await
        .unwrap();

    // 背圧は「速いが失う」を「正しいが遅い」に替える仕組みなので、待ち時間は寛容に取る。
    let out = drain_data_until(&mut c, MARKER, Duration::from_secs(120)).await;
    assert!(
        contains(&out, MARKER),
        "末尾マーカーが着いていない（{} bytes）",
        out.len()
    );

    // 数字だけの行を拾う。打った行の echo（`seq 1 20000; echo …`）は混ざらない。
    let mut seen = vec![false; N + 1];
    let mut dupes = 0usize;
    // `\n` だけで割らないこと。シェルは bracketed paste を切る escape の後に `\r` を出すので、
    // 最初の行が `ESC[?2004l\r1` という 1 片になり、`1` だけが数字でない片に埋まる。
    // `\r` でも割り、その前に端末の印を落とす（`strip_escapes`）。
    for raw in strip_escapes(&String::from_utf8_lossy(&out)).split(['\n', '\r']) {
        let t = raw.trim_matches(' ');
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(v) = t.parse::<usize>() {
            if (1..=N).contains(&v) {
                if seen[v] {
                    dupes += 1;
                }
                seen[v] = true;
            }
        }
    }
    let missing: Vec<usize> = (1..=N).filter(|&v| !seen[v]).collect();
    assert!(
        missing.is_empty(),
        "🚨 {} 行が黙って抜けた（最初の 10 本: {:?}）—— 背圧が効いていない",
        missing.len(),
        &missing[..missing.len().min(10)]
    );
    assert_eq!(dupes, 0, "🚨 重複が出ている（{dupes} 件）");
}

// ── 見るだけの attach（`berth attach -r`・tmux の `attach -r`）──────────────────
//
// 決めてある形: 見るのは何本でも・打つのは 1 本 ／ 遅い見る側は外す ／
// 見る側は `q` か Ctrl-C で抜ける ／ 外部 detach は全員を外す。
// 誰が打つかは仕組みではなく人が決める（tmux のペアプロと同じ割り切り）。
// 注意: `read_only` を読まない berthd では、見るだけのつもりの attach が操作する側を置き換える。

/// 共有バッファに needle が現れるまで（or timeout）待つ。PTY master は別スレッドが読み続ける。
async fn wait_for_bytes(buf: &Arc<Mutex<Vec<u8>>>, needle: &[u8], within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if contains(&buf.lock().unwrap(), needle) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 末尾だけを見て needle を待つ。MB 単位の出力で `drain_data_until`（毎回全体を走査する）を使うと二乗になる。
/// 返すのは（見つかったか, 受け取った data のバイト数）。
/// 呼び手（遅い見る側の検体）が unix だけなので、ここも unix 専用（付けないと Windows で未使用の警告になる）。
#[cfg(unix)]
async fn drain_until_tail(s: &mut DaemonStream, needle: &[u8], within: Duration) -> (bool, usize) {
    let deadline = tokio::time::Instant::now() + within;
    let mut tail: Vec<u8> = Vec::new();
    let mut total = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (false, total);
        }
        match read_frame_within(s, remaining).await {
            Some(Ok(f)) => {
                if f.kind != KIND_DATA {
                    continue;
                }
                total += f.payload.len();
                tail.extend_from_slice(&f.payload);
                if contains(&tail, needle) {
                    return (true, total);
                }
                let keep = needle.len().saturating_sub(1);
                if tail.len() > keep {
                    tail.drain(..tail.len() - keep);
                }
            }
            _ => return (false, total),
        }
    }
}

/// `Detached` が来るまで読む（data は読み飛ばす）。来たら `Some(reason)`、来なければ `None`。
async fn wait_detached(s: &mut DaemonStream, within: Duration) -> Option<Option<String>> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match read_frame_within(s, remaining).await {
            Some(Ok(f)) if f.kind == berthd::KIND_CONTROL => {
                if let Ok(Control::Detached { reason, .. }) = decode_control(&f.payload) {
                    return Some(reason);
                }
            }
            Some(Ok(_)) => continue,
            _ => return None,
        }
    }
}

/// 操作する側（`op`）が attach している session へ、見る側（`viewer`・`read_only: true`）が attach した状態を作る。
/// どちらも `Attached` を読み終えてある。注意: 見る側はまだ読んでいない（遅い見る側の検体がそれを使う）。
async fn an_operator_and_a_viewer(
    tag: &str,
    op_size: (u16, u16),
    viewer_size: (u16, u16),
) -> (
    common::ChildGuard,
    std::ffi::OsString,
    DaemonStream,
    DaemonStream,
    String,
) {
    let endpoint = berthd::scratch_endpoint(tag);
    let daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    let session_id = format!("{tag}-tab");

    let mut op = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut op,
        1,
        &Control::Spawn {
            session_id: Some(session_id.clone()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: op_size.0,
            rows: op_size.1,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Spawned { .. }));
    send_ctrl(
        &mut op,
        2,
        &Control::Attach {
            session_id: session_id.clone(),
            cols: op_size.0,
            rows: op_size.1,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Attached { .. }));
    become_a_terminal(&mut op, Duration::from_secs(20)).await;

    let mut viewer = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut viewer,
        1,
        &Control::Attach {
            session_id: session_id.clone(),
            cols: viewer_size.0,
            rows: viewer_size.1,
            actor: None,
            actor_src: None,
            read_only: true,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut viewer).await,
        Control::Attached { .. }
    ));

    (daemon, endpoint, op, viewer, session_id)
}

#[tokio::test]
async fn a_viewer_does_not_take_the_session_from_the_operator() {
    let (_daemon, _endpoint, mut op, _viewer, _sid) =
        an_operator_and_a_viewer("keep", (80, 24), (80, 24)).await;
    assert_eq!(
        wait_detached(&mut op, Duration::from_millis(1500)).await,
        None,
        "🚨 見るだけの attach が操作する側を外した（`read_only` を読んでいない）"
    );
}

/// unix 専用。打つのは `printf`（目印が実行結果にだけ出る形）。Windows の綴りでは書いていない。
#[cfg(unix)]
#[tokio::test]
async fn a_viewer_sees_the_output_but_its_keys_are_dropped() {
    let (_daemon, _endpoint, mut op, mut viewer, _sid) =
        an_operator_and_a_viewer("keys", (80, 24), (80, 24)).await;

    write_frame(&mut op, &Frame::data(0, line("printf 'OP_%s\\n' SEEN")))
        .await
        .unwrap();
    let seen = drain_data_until(&mut viewer, b"OP_SEEN", Duration::from_secs(5)).await;
    assert!(
        contains(&seen, b"OP_SEEN"),
        "🚨 見る側に操作する側の出力が届かない: {:?}",
        String::from_utf8_lossy(&seen)
    );

    write_frame(&mut viewer, &Frame::data(0, line("printf 'V_%s\\n' LEAK")))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(500)).await;
    write_frame(&mut op, &Frame::data(0, line("printf 'OP_%s\\n' AFTER")))
        .await
        .unwrap();
    let out = drain_data_until(&mut op, b"OP_AFTER", Duration::from_secs(5)).await;
    let text = String::from_utf8_lossy(&out);
    assert!(
        contains(&out, b"OP_AFTER"),
        "陽性の対照: 操作する側の打鍵は実行されるはず: {text:?}"
    );
    assert!(
        !contains(&out, b"V_LEAK"),
        "🚨 見る側の打鍵が PTY に届いて実行された: {text:?}"
    );
}

/// unix 専用。大きさは `stty size` で読む。
#[cfg(unix)]
#[tokio::test]
async fn a_viewer_does_not_resize_the_session() {
    // 見る側は attach の時点で 40x10 を名乗り、さらに Resize も撃つ。
    let (_daemon, _endpoint, mut op, mut viewer, session_id) =
        an_operator_and_a_viewer("vsize", (100, 30), (40, 10)).await;
    send_ctrl(
        &mut viewer,
        9,
        &Control::Resize {
            session_id: session_id.clone(),
            cols: 41,
            rows: 11,
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    write_frame(
        &mut op,
        &Frame::data(0, line("printf 'SZ[%s]SZ_%s\\n' \"$(stty size)\" END")),
    )
    .await
    .unwrap();
    let out = drain_data_until(&mut op, b"SZ_END", Duration::from_secs(5)).await;
    let text = String::from_utf8_lossy(&out);
    assert!(
        contains(&out, b"SZ_END"),
        "陽性の対照: 操作する側の `stty size` が返らない: {text:?}"
    );
    assert!(
        contains(&out, b"SZ[30 100]"),
        "🚨 見る側が大きさを変えた（attach で 10 40 / Resize で 11 41）: {text:?}"
    );
}

/// 遅い見る側は外す。操作する側は背圧で待つが、見る側 1 本のために止めない。
/// unix 専用。出力は `head -c … /dev/zero | tr`。
#[cfg(unix)]
#[tokio::test]
async fn a_slow_viewer_is_detached_without_stalling_the_operator() {
    let (_daemon, _endpoint, mut op, mut viewer, _sid) =
        an_operator_and_a_viewer("slow", (80, 24), (80, 24)).await;

    // 見る側は読まない。操作する側から大きな出力を出し、末尾の目印まで届くかを見る。
    write_frame(
        &mut op,
        &Frame::data(
            0,
            line("head -c 12000000 /dev/zero | tr '\\0' x; printf 'END_%s\\n' MARK"),
        ),
    )
    .await
    .unwrap();
    let (reached, bytes) = drain_until_tail(&mut op, b"END_MARK", Duration::from_secs(90)).await;
    assert!(
        reached,
        "🚨 操作する側が出力の末尾まで届かなかった（{bytes} byte で止まった）—— 読まない見る側が止めている"
    );

    let reason = wait_detached(&mut viewer, Duration::from_secs(30)).await;
    assert_eq!(
        reason,
        Some(Some("too_slow".to_string())),
        "🚨 追いつけない見る側が `too_slow` で外されていない"
    );
}

/// 外部 detach（`berth detach` / 中の `berth d`）は全員を外す（tmux の `detach-client -s`）。
#[tokio::test]
async fn an_external_detach_takes_the_operator_and_every_viewer_off() {
    let (_daemon, endpoint, mut op, mut viewer, session_id) =
        an_operator_and_a_viewer("everyone", (80, 24), (80, 24)).await;
    // 前提: 見る側が入っても操作する側は外れていない（崩れると下の主張が意味を失う）。
    assert_eq!(
        wait_detached(&mut op, Duration::from_secs(1)).await,
        None,
        "🚨 前提が崩れている —— 見るだけの attach が操作する側を外した"
    );

    let mut ctl = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut ctl,
        1,
        &Control::Detach {
            session_id: session_id.clone(),
        },
    )
    .await;
    assert!(
        wait_detached(&mut op, Duration::from_secs(5))
            .await
            .is_some(),
        "🚨 外部 detach で操作する側に `Detached` が来ない"
    );
    assert!(
        wait_detached(&mut viewer, Duration::from_secs(5))
            .await
            .is_some(),
        "🚨 外部 detach で見る側に `Detached` が来ない"
    );
}

/// 実物の `berth attach -r`: 見る印を出し、操作する側を外さず、`q` で抜ける。
///
/// 注意: attach の OSC 2746 を出さないこと。その信号を覚える端末アプリは、ssh が切れると覚えた名前で
/// `berth attach <name>` を打ち直すので、見る側が出すと操作する側として入り直してしまう。
/// unix 専用。Windows の raw モードで `q` がどう届くかは確かめていない。
#[cfg(unix)]
#[tokio::test]
async fn berth_attach_r_watches_without_claiming_and_leaves_on_q() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write as _;

    let endpoint = berthd::scratch_endpoint("watch");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    // 操作する側を protocol で先に入れておく（見る側が「引き取らない」ことを見るため）。
    let mut op = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut op,
        1,
        &Control::Spawn {
            session_id: Some("watch-demo".into()),
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
    assert!(matches!(read_ctrl(&mut op).await, Control::Spawned { .. }));
    // `berth` が画面に出すのは id ではなく人向けの名前（`session_handle`）。protocol で作った session は
    // 名前が `session-N` になるので、`berth attach <name>` が作るときと同じく名前を揃えておく
    // （揃えないと `[watching session-1 — …]` と出て、検体の方が外れる）。
    send_ctrl(
        &mut op,
        3,
        &Control::Rename {
            session_id: "watch-demo".into(),
            name: "watch-demo".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Renamed { .. }));
    send_ctrl(
        &mut op,
        2,
        &Control::Attach {
            session_id: "watch-demo".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Attached { .. }));

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
    // この検体は部屋の形（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるので、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("-r");
    cmd.arg("watch-demo");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );

    let mut keys = pair.master.take_writer().expect("pty writer");
    let _child = PtyChildGuard::new(
        pair.slave
            .spawn_command(cmd)
            .expect("spawn berth attach -r"),
    );
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    let watching = wait_for_bytes(&buf, b"[watching watch-demo", Duration::from_secs(5)).await;
    let op_kept = wait_detached(&mut op, Duration::from_secs(1))
        .await
        .is_none();
    keys.write_all(b"q").ok();
    keys.flush().ok();
    let left = wait_for_bytes(
        &buf,
        b"[stopped watching watch-demo]",
        Duration::from_secs(5),
    )
    .await;

    drop(_child);
    assert_eq!(
        terminal.close(pair.master),
        common::PtyClose::Returned,
        "pty を閉じられず期限へ落ちた"
    );

    let dump = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    assert!(
        watching,
        "🚨 `berth attach -r` が見る印を出さない（`-r` を名前として読んでいないか）: {dump:?}"
    );
    assert!(
        op_kept,
        "🚨 `berth attach -r` が操作する側を外した: {dump:?}"
    );
    assert!(left, "🚨 `q` で抜けない: {dump:?}");
    assert!(
        !dump.contains("\u{1b}]2746;attach"),
        "🚨 見る側が attach の OSC 2746 を出した —— argotty が ssh 断で打ち直し、操作する側として入り直す: {dump:?}"
    );
}

// ── 一覧の大きさは「作った時」ではなく「今」 ────────────────────────────
//
// `SessionInfo` の `cols`/`rows` は spawn 時の記録ではなく、`List` のたびに pty へ訊いた値
// （cwd の今の値と同じ形）。winsize は tty 自身の属性なので、誰が変えたかに依らず master から
// 読める（attach の resize でも、セッションの中の `stty` でも）。
// 注意: 面で意味が違う: unix の `get_size()` は master への本物の `TIOCGWINSZ`（測定）だが、
// Windows（ConPTY）は最後に設定した値を返すだけ（控え）。Windows に `stty` の経路は無いので、
// そこで落ちる物は無い。

/// `List` を撃って、その session の今の大きさを返す。
async fn listed_size(s: &mut DaemonStream, serial: u32, id: &str) -> Option<(u16, u16)> {
    send_ctrl(s, serial, &Control::List).await;
    match read_ctrl(s).await {
        Control::Sessions { sessions } => sessions
            .iter()
            .find(|x| x.id == id)
            .map(|x| (x.cols, x.rows)),
        other => panic!("expected Sessions, got {other:?}"),
    }
}

/// `want` になるまで `List` を撃ち直す（resize は channel 越しなので即時ではない）。
/// 最後に見えた値を返す（赤のとき「何が出ていたか」が読めないと直せない）。
async fn listed_size_settles(
    s: &mut DaemonStream,
    id: &str,
    want: (u16, u16),
    within: Duration,
) -> Option<(u16, u16)> {
    let deadline = tokio::time::Instant::now() + within;
    let mut serial = 900;
    let mut last = None;
    while tokio::time::Instant::now() < deadline {
        last = listed_size(s, serial, id).await;
        serial += 1;
        if last == Some(want) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

#[tokio::test]
async fn the_list_reports_the_size_the_pty_has_now_not_the_one_it_was_made_with() {
    let (_daemon, _endpoint, _a, mut b, session_id) = a_replaced_by_b("lssize", 100, 30).await;
    // 陽性対照は作った側で取ってある。`a_replaced_by_b` は 80x24 で spawn し、A も 80x24 で
    // attach するので、B が来る前の一覧は 80x24（欄が配線されていること自体は別に成り立つ）。
    let got = listed_size_settles(&mut b, &session_id, (100, 30), Duration::from_secs(5)).await;
    assert_eq!(
        got,
        Some((100, 30)),
        "🚨 一覧の大きさが attach の resize を追っていない —— spawn 時の 80x24 のまま"
    );
}

/// unix 専用。`stty` で大きさを変えるのは unix の経路。
/// 「控えを書き直す」ではなく「pty に訊く」実装でないと通らない側の検体。
#[cfg(unix)]
#[tokio::test]
async fn a_stty_inside_the_session_shows_up_in_the_list() {
    let (_daemon, _endpoint, _a, mut b, session_id) = a_replaced_by_b("lsstty", 100, 30).await;
    assert_eq!(
        listed_size_settles(&mut b, &session_id, (100, 30), Duration::from_secs(5)).await,
        Some((100, 30)),
        "陽性の対照: attach の大きさが先に届いていること"
    );
    write_frame(&mut b, &Frame::data(0, line("stty rows 11 cols 33")))
        .await
        .unwrap();
    let got = listed_size_settles(&mut b, &session_id, (33, 11), Duration::from_secs(5)).await;
    assert_eq!(
        got,
        Some((33, 11)),
        "🚨 セッションの中の `stty` が一覧に出ない（2 つ目の経路）"
    );
}

// ── 大きさが変わる attach では、バナーを自分の行に置く ──────────────────
//
// 大きさが変わると readline はカーソルの居る行を `\r` から描き直す。バナー（`[attached to …]`）の
// 後に改行が無いと、その描き直しがバナーを上書きする。さらに、プロンプトを出させるために
// シェルへ `\n` を送る形だと、プロンプトが 2 つ並ぶ。
//
// ∴ 変わるかどうかを berthd に言わせる（`Attached.resized`）。client には判定できない。
// 大きさは tty 自身の属性で、セッションの中の `stty` でも動く。

/// `Attach` を撃って `Attached.resized` を読む。
async fn attach_resized(s: &mut DaemonStream, serial: u32, id: &str, cols: u16, rows: u16) -> bool {
    send_ctrl(
        s,
        serial,
        &Control::Attach {
            session_id: id.to_string(),
            cols,
            rows,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    match read_ctrl(s).await {
        Control::Attached { resized, .. } => resized,
        other => panic!("expected Attached, got {other:?}"),
    }
}

#[tokio::test]
async fn attached_says_whether_this_attach_changed_the_size() {
    let (_daemon, endpoint, _a, _b, session_id) = a_replaced_by_b("resized", 100, 30).await;
    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    // 陰性対照が先。今と同じ 100x30 で入り直しても「変えた」と言わないこと。
    // 常に true を返す実装でも次の assert は通るので、順番に意味がある。
    assert!(
        !attach_resized(&mut c, 1, &session_id, 100, 30).await,
        "🚨 大きさが変わらない attach で `resized` が立った —— 立つとバナーの下が空になる"
    );
    let mut d = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    assert!(
        attach_resized(&mut d, 1, &session_id, 80, 24).await,
        "🚨 大きさが変わる attach なのに `resized` が立たない —— バナーが上書きされる"
    );
}

/// unix 専用。見るのは実物の `berth` が pty に書いたバイト。
///
/// 実物で測る理由: この欠陥は画面のバイトの並びで、protocol の欄だけでは再現しない。
#[cfg(unix)]
#[tokio::test]
async fn a_resizing_attach_keeps_the_banner_on_its_own_line() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let endpoint = berthd::scratch_endpoint("banner");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    // 24 行のシェルを立てて落ち着かせ、誰も attach していない状態にする。
    //
    // 注意: シェルを `/bin/bash` に決め打つ（この検体だけ）。測っているのは readline が SIGWINCH で
    // 行を描き直すことで、`dash` のように行編集を持たないシェルでは何も起きず、`$SHELL` 次第で
    // 神託が意味を失う。macOS の `/bin/bash`（3.2）も readline を持つ。
    let mut op = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut op,
        1,
        &Control::Spawn {
            session_id: Some("banner-demo".into()),
            name: None,
            shell: Some("/bin/bash".into()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Spawned { .. }));
    send_ctrl(
        &mut op,
        3,
        &Control::Rename {
            session_id: "banner-demo".into(),
            name: "banner-demo".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Renamed { .. }));
    send_ctrl(
        &mut op,
        2,
        &Control::Attach {
            session_id: "banner-demo".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Attached { .. }));
    become_a_terminal(&mut op, Duration::from_secs(20)).await;
    // プロンプトを数えられる綴りにする。env の `PS1` は効かない（対話 bash は `/etc/bash.bashrc` と
    // `~/.bashrc` で上書きし、HOME を空にしても `/etc` 側が残る。∴ セッションの中で立てる。
    // ここで出る echo と新しいプロンプトは op の側にしか流れない（CLI は replay を捨てる）。
    write_frame(&mut op, &Frame::data(0, line("PS1='PROMPT$ '")))
        .await
        .unwrap();
    drain_data_until(&mut op, b"PROMPT$ ", Duration::from_secs(5)).await;
    // この接続を畳んで「誰も attach していない」に戻す（次の `berth` が置き換えでなく普通に入る）。
    drop(op);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 40 行の pty から実物の `berth attach` を撃つ（セッションは 24 → 40 に変わる）。
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.arg("attach");
    // この検体は部屋の形（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるので、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("banner-demo");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    let banner = b"[attached to banner-demo]";
    assert!(
        wait_for_bytes(&buf, banner, Duration::from_secs(5)).await,
        "陽性の対照: バナーが出ない"
    );
    // 描き直しが届くまで待つ（SIGWINCH → readline）。
    tokio::time::sleep(Duration::from_secs(2)).await;
    let seen = buf.lock().unwrap().clone();
    let at = seen
        .windows(banner.len())
        .position(|w| w == banner)
        .expect("banner");
    let mut after = &seen[at + banner.len()..];
    // 注意: バナーの直後には OSC が挟まる（2746 の attach と題名の OSC 2 など、2 つ以上並びうる）。
    // 端末には何も描かない列なので、終端（ST）まで読んで OSC が続く限り飛ばしてから、
    // 「次に来るのは改行か」を見る。
    while after.starts_with(b"\x1b]") {
        match after.windows(2).position(|w| w == b"\x1b\\") {
            Some(end) => after = &after[end + 2..],
            None => break,
        }
    }
    let text = String::from_utf8_lossy(&seen);

    // 改行でなく `\r`（同じ行の描き直し）が来ると、バナーが消える。
    assert!(
        after.starts_with(b"\r\n"),
        "🚨 バナーの後が改行で終わっていない ＝ 描き直しが同じ行に来て上書きされる: {:?}",
        String::from_utf8_lossy(&after[..after.len().min(40)])
    );
    // プロンプトは 1 つだけ（注入した改行による 2 つ目が出ていないこと）。
    let prompts = text.matches("PROMPT$").count();
    assert_eq!(
        prompts, 1,
        "🚨 プロンプトが {prompts} 個 —— 大きさが変わる attach では改行を送らないこと: {text:?}"
    );
    drop(terminal);
}

/// 同じ大きさの再接続でも、シェルに SIGWINCH が届く（改行は送らない）。
///
/// 改行を 1 つ送って空コマンドを走らせる形だと、覚えている行へ絶対位置で描き直す行エディタ
/// （reedline など）では、berth が消した画面の最下段にプロンプトが出て、改行のスクロールで
/// バナーが押し出される。SIGWINCH を受けた行エディタは位置を測り直すので、同じ大きさでも
/// 一度ずらして戻し、その道に乗せる。
///
/// シェルは偽物（`trap … WINCH` と `read` だけ）。測りたいのは「何がシェルに届くか」で、
/// 行エディタの描き方ではない。
#[cfg(unix)]
#[tokio::test]
async fn a_same_size_reattach_tells_the_shell_by_sigwinch_not_by_a_newline() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::os::unix::fs::PermissionsExt as _;

    let shell = std::env::temp_dir().join(format!("berthd-fake-shell-{}.sh", std::process::id()));
    std::fs::write(
        &shell,
        // Note: No fractional `read -t`: macOS /bin/bash is 3.2 and rejects `-t 0.2`, so read dies
        // at once and the trap never gets its turn. The trap waits in
        // `wait`, which a trapped signal interrupts in every bash; lines are read in the
        // background, on fd 3 because a background job's stdin is /dev/null otherwise.
        "#!/bin/bash\n\
         trap 'printf \"WINCHED %s\\n\" \"$(stty size)\"' WINCH\n\
         printf 'READY\\n'\n\
         exec 3<&0\n\
         ( while IFS= read -r l; do printf 'GOT-LINE[%s]\\n' \"$l\"; done ) <&3 &\n\
         while :; do sleep 1 & wait $!; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    struct Rm(std::path::PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _rm = Rm(shell.clone());

    let endpoint = berthd::scratch_endpoint("samesize");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    // 24x80 で作り、一度 attach して READY まで見届けてから手放す（次の berth は再接続）。
    let mut op = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut op,
        1,
        &Control::Spawn {
            session_id: Some("samesize".into()),
            name: Some("samesize".into()),
            shell: Some(shell.to_string_lossy().into_owned()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Spawned { .. }));
    send_ctrl(
        &mut op,
        2,
        &Control::Attach {
            session_id: "samesize".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Attached { .. }));
    drain_data_until(&mut op, b"READY", Duration::from_secs(5)).await;
    drop(op);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 同じ 24x80 の pty から実物の `berth attach`。
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
    // この検体は部屋の形（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるので、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("samesize");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    let banner = b"[attached to samesize]";
    assert!(
        wait_for_bytes(&buf, banner, Duration::from_secs(5)).await,
        "陽性の対照: バナーが出ない"
    );
    // 最後の大きさで描き直したことを見る（途中の大きさの WINCHED が先に来うる）。
    assert!(
        wait_for_bytes(&buf, b"WINCHED 24 80", Duration::from_secs(5)).await,
        "🚨 同じ大きさの再接続で、シェルが 24x80 の SIGWINCH を受けていない: {:?}",
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let text = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    // 注意: ずらした大きさを、シェルが一度は見ていること。2 回の resize を続けて送ると合図が 1 回に
    // まとまり、シェルは元の大きさしか見ない（readline は「変わっていない」と描き直さず、
    // プロンプトが出ない。最後の大きさだけを見る神託はそれを見逃す。
    let ajar = text.find("WINCHED 23 80");
    let back = text.rfind("WINCHED 24 80");
    assert!(
        matches!((ajar, back), (Some(a), Some(b)) if a < b),
        "🚨 シェルがずらした大きさ（23 行）を見てから元の大きさ（24 行）に戻っていない: {text:?}"
    );
    assert!(
        !text.contains("GOT-LINE"),
        "🚨 berth がシェルへ行を送った（空コマンドを走らせる回り道）: {text:?}"
    );
    // 終わったときの大きさは手元の端末と同じ（ずらしたまま残っていない）。
    let mut q = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(&mut q, 9, &Control::List).await;
    match read_ctrl(&mut q).await {
        Control::Sessions { sessions, .. } => {
            let s = sessions
                .iter()
                .find(|s| s.name == "samesize")
                .expect("session listed");
            assert_eq!((s.cols, s.rows), (80, 24), "🚨 ずらした大きさが残っている");
        }
        other => panic!("unexpected reply to List: {other:?}"),
    }
    drop(terminal);
}

/// unix 専用。実物の `berth` が pty に書いたバイトを見る。
///
/// 単体（`bin/berth/attach/alt_screen.rs` の `AltScreenShim`）は翻訳しか守らない。「その翻訳が
/// セッションの出力に本当に掛かっているか」はここでしか言えない。
#[cfg(unix)]
#[tokio::test]
async fn a_full_screen_app_inside_the_session_leaves_no_residue() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write as _;

    let endpoint = berthd::scratch_endpoint("altshim");
    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    // 端点が立つまで待つ（`berth` は BERTHD_SOCKET 付きでは自分で起こさない）。
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
    // この検体は部屋の形（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるので、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("alt-demo");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    let mut keys = pair.master.take_writer().expect("pty writer");
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    assert!(
        wait_for_bytes(&buf, b"[attached to alt-demo]", Duration::from_secs(10)).await,
        "陽性の対照: attach のバナーが出ない"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = buf.lock().unwrap().len();

    // 全画面アプリの出入りだけを真似る（本物の vim を要求しない）。
    // 注意: 打った行の echo が自分のマーカーに当たる（`a_fast_burst_loses_no_lines` と同じ罠）。
    // `BACK_OUT` を echo に見つけて待つのをやめると、出力が届く前に数えて「クリア 0 回」と読む
    // （速いシェルでは間に合って緑になる）。打つ側では割り、出る側でだけ繋がるようにする。
    keys.write_all(b"printf '\\033[?1049hINSIDE_ALT\\033[?1049l'; echo BACK_''OUT\n")
        .ok();
    keys.flush().ok();
    assert!(
        wait_for_bytes(&buf, b"BACK_OUT", Duration::from_secs(10)).await,
        "陽性の対照: 打った行が実行されない"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = buf.lock().unwrap()[before..].to_vec();
    let text = String::from_utf8_lossy(&seen);

    // 切替そのものは端末へ出さない（出すと `berth` 自身の部屋が畳まれる）。
    assert!(
        !contains(&seen, b"\x1b[?1049"),
        "🚨 alt-screen の切替が素通しになった ＝ `berth` の部屋が畳まれる: {text:?}"
    );
    // 入口と出口の両方がクリアに翻訳されること。翻訳が掛かっていないと 0 回で、`INSIDE_ALT` が画面に残る。
    let clears = seen.windows(7).filter(|w| *w == b"\x1b[2J\x1b[H").count();
    assert_eq!(
        clears, 2,
        "🚨 全画面アプリの出入りがクリアに翻訳されていない —— 入口と出口で 2 回のはず: {text:?}"
    );
    drop(terminal);
}

// ── 入り直す瞬間に、録画を端末自身のスクロールへ描き直す（`berth attach --scrollback`）────
//
// 既定は ON。外し方は `--no-scrollback` / `BERTH_SCROLLBACK=off` / tmux 等の中では自動。
// berthd は生バイトのリングのまま（格子を持たない）。エミュレータは `crates/berth-screen` が
// クライアントで 1 回だけ回す。単体（`berth-screen`）は描き直しの形しか守らない。
// 「その描き直しが実物の attach で端末に届くか」はここでしか言えない。
// unix 専用。Windows の ConPTY は berth の出力を描き直してから届けるので、行の数を数えられない。
// Windows では端末（Windows Terminal）で目で確かめるしかない。

/// 300 行を出したセッションを作り、手放す（次の `berth attach` は再接続）。
#[cfg(unix)]
async fn a_session_that_printed_300_lines(tag: &str) -> (common::ChildGuard, std::ffi::OsString) {
    a_session_that_ran(tag, b"seq 1 300; echo END_''MARK\n", b"END_MARK").await
}

/// 実デーモンにセッションを作り、`cmd` を打って `marker` が出るまで待ってから抜ける（録画を仕込む）。
#[cfg(unix)]
async fn a_session_that_ran(
    tag: &str,
    cmd: &[u8],
    marker: &[u8],
) -> (common::ChildGuard, std::ffi::OsString) {
    let endpoint = berthd::scratch_endpoint(tag);
    let daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");
    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some(tag.into()),
            name: Some(tag.into()),
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
            session_id: tag.into(),
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
    become_a_terminal(&mut c, Duration::from_secs(20)).await;
    // 打つ側では割る（echo が自分のマーカーに当たらないように）。
    write_frame(&mut c, &Frame::data(0, cmd.to_vec()))
        .await
        .unwrap();
    let out = drain_data_until(&mut c, marker, Duration::from_secs(20)).await;
    assert!(
        contains(&out, marker),
        "陽性の対照: {:?} が出ない",
        String::from_utf8_lossy(marker)
    );
    drop(c);
    tokio::time::sleep(Duration::from_millis(300)).await;
    (daemon, endpoint)
}

/// 実物の `berth attach <flag> <tag>` を 24x80 の pty から撃ち、`needle` が見えてから少し置いて、
/// 端末が受け取った生のバイトを返す。
#[cfg(unix)]
async fn attach_through_a_pty(
    endpoint: &std::ffi::OsStr,
    tag: &str,
    flag: &str,
    needle: &[u8],
) -> Vec<u8> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
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
    cmd.arg(flag);
    cmd.arg(tag);
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));
    assert!(
        wait_for_bytes(
            &buf,
            format!("[attached to {tag}]").as_bytes(),
            Duration::from_secs(10)
        )
        .await,
        "陽性の対照: バナーが出ない"
    );
    assert!(
        wait_for_bytes(&buf, needle, Duration::from_secs(10)).await,
        "陽性の対照: {:?} が出ない: {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let bytes = buf.lock().unwrap().clone();
    drop(terminal);
    bytes
}

/// 数字だけの行を拾って、1..=300 の各値が何回出たかを数える（打った行の echo `seq 1 300` は混ざらない）。
#[cfg(unix)]
fn count_numbered_lines(bytes: &[u8]) -> Vec<usize> {
    let mut seen = vec![0usize; 301];
    for raw in strip_escapes(&String::from_utf8_lossy(bytes)).split(['\n', '\r']) {
        let t = raw.trim_matches(' ');
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(v) = t.parse::<usize>() {
            if (1..=300).contains(&v) {
                seen[v] += 1;
            }
        }
    }
    seen
}

/// 再接続で 1..=300 がそれぞれちょうど 1 回端末へ届き（履歴 → 画面・二重描き無し）、部屋（alt screen）を作らない。
#[cfg(unix)]
#[tokio::test]
async fn attach_replays_history_into_the_terminals_own_scrollback() {
    let (_daemon, endpoint) = a_session_that_printed_300_lines("sb-on").await;
    // 画面の最後の方に来る行が見えるまで待つ（履歴はその前に流れている）。
    let bytes = attach_through_a_pty(&endpoint, "sb-on", "--scrollback", b"300").await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !contains(&bytes, b"\x1b[?1049h"),
        "🚨 スクロールへ流す形なのに部屋（alt screen）を作っている: {text:?}"
    );
    let seen = count_numbered_lines(&bytes);
    let missing: Vec<usize> = (1..=300).filter(|&v| seen[v] == 0).collect();
    let dupes: Vec<usize> = (1..=300).filter(|&v| seen[v] > 1).collect();
    assert!(
        missing.is_empty(),
        "🚨 履歴から {} 行が抜けた（最初の 10 本: {:?}）: {text:?}",
        missing.len(),
        &missing[..missing.len().min(10)]
    );
    // 二重描き（最後の画面分が履歴と画面の両方に並ぶ形）を止める。
    assert!(
        dupes.is_empty(),
        "🚨 同じ行が 2 回届いている（最初の 10 本: {:?}）＝ 履歴と画面が重なっている: {text:?}",
        &dupes[..dupes.len().min(10)]
    );
    // 順序: 履歴の末尾（277 あたり）は画面の頭（278）より前に来る。
    let t = strip_escapes(&text);
    let (a, b) = (t.find("\n277"), t.find("\n278"));
    assert!(
        matches!((a, b), (Some(a), Some(b)) if a < b),
        "履歴と画面の順が逆: {t:?}"
    );
}

/// 画面の頭が空の録画で入り直すとき、berth は履歴を書いた後に端末へ今の行を訊き（`ESC[6n`）、
/// 答えの行から最下行まで足りない分だけ改行する（1 画面ぶんの空白を割り込ませない）。
/// 偽の端末は DSR に「6 行目」と答える。
///
/// 録画: 空行 23 本 → `MARK` → プロンプト（80x24 の画面は上 22 行が空、23 行目 MARK、最下行プロンプト）。
/// 期待: 履歴を書いた直後に問い（`ESC[6n`）、その後の改行は 22 - 5 = 17 本で MARK に着く。訊かなければ 22 本。
#[cfg(unix)]
#[tokio::test]
async fn the_replay_asks_the_terminal_for_its_row_and_skips_the_blank_top() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write as _;

    let (_daemon, endpoint) =
        a_session_that_ran("sb-dsr", b"yes '' | head -23; echo MA''RK\n", b"MARK").await;
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
    cmd.arg("--scrollback");
    cmd.arg("sb-dsr");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    // 偽の端末: 読みながら、`ESC[6n` を見たら「6 行目 1 桁目」と答える（カーソルは 0 始まりで 5 行目）。
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let asked = Arc::new(Mutex::new(0usize));
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut writer = pair.master.take_writer().expect("pty writer");
    let (sink, asked_in) = (buf.clone(), asked.clone());
    let pump = std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        let mut seen = 0usize;
        loop {
            match std::io::Read::read(&mut reader, &mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut s = sink.lock().unwrap();
                    s.extend_from_slice(&tmp[..n]);
                    while let Some(p) = s[seen..].windows(4).position(|w| w == b"\x1b[6n") {
                        seen += p + 4;
                        *asked_in.lock().unwrap() += 1;
                        let _ = writer.write_all(b"\x1b[6;1R");
                        let _ = writer.flush();
                    }
                }
            }
        }
    });
    assert!(
        wait_for_bytes(&buf, b"[attached to sb-dsr]", Duration::from_secs(10)).await,
        "陽性の対照: バナーが出ない: {:?}",
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bytes = buf.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert_eq!(
        *asked.lock().unwrap(),
        1,
        "端末に行を 1 回訊くはず: {text:?}"
    );
    let mark = text.find("MARK\r\n").expect("MARK が画面に出る");
    // MARK の直前に続く `\r\n` の本数と、その塊の直前に問いが在ること（履歴を書いてから訊いている）。
    let before = &text[..mark];
    let mut run = 0;
    let mut t = before;
    while let Some(rest) = t.strip_suffix("\r\n") {
        run += 1;
        t = rest;
    }
    assert!(
        t.ends_with("\x1b[6n"),
        "🚨 問いは履歴を書いた直後・画面の直前に在るはず: {text:?}"
    );
    assert_eq!(
        run, 17,
        "🚨 MARK の前の改行が 17 本（22 − 5）ではない ＝ 画面の頭の空白をそのまま書いている: {text:?}"
    );
    drop(_child);
    pump.join().ok();
}

/// `--no-scrollback` は今までの形（部屋に入り、録画は流さない。画面はシェルの描き直しで戻る）。
#[cfg(unix)]
#[tokio::test]
async fn no_scrollback_keeps_the_old_room_and_replays_nothing() {
    let (_daemon, endpoint) = a_session_that_printed_300_lines("sb-off").await;
    // 待つのは描き直し（SIGWINCH）が済む目安としてのバナーだけ。
    let bytes = attach_through_a_pty(
        &endpoint,
        "sb-off",
        "--no-scrollback",
        b"[attached to sb-off]",
    )
    .await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        contains(&bytes, b"\x1b[?1049h"),
        "🚨 `--no-scrollback` なのに部屋（alt screen）に入っていない: {text:?}"
    );
    let seen = count_numbered_lines(&bytes);
    let replayed = (1..=300).filter(|&v| seen[v] > 0).count();
    assert_eq!(
        replayed, 0,
        "🚨 `--no-scrollback` なのに録画が流れている（{replayed} 行）: {text:?}"
    );
}

/// 部屋が無いとき、中の全画面アプリの alt 切替はそのまま端末へ通る（翻訳しない）。
/// 上の `a_full_screen_app_inside_the_session_leaves_no_residue` の対照。あちらは部屋の中で
/// 翻訳されることを、こちらは部屋が無ければ本物の端末に任せることを釘付けする。
#[cfg(unix)]
#[tokio::test]
async fn a_full_screen_app_passes_through_when_replaying_into_scrollback() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write as _;

    let endpoint = berthd::scratch_endpoint("altpass");
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
    cmd.arg("--scrollback");
    cmd.arg("alt-pass");
    for (k, v) in std::env::vars() {
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &endpoint);
    cmd.env("TERM", "xterm-256color");
    // 本物の `~/.berth/config.toml` は読ませない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    let mut keys = pair.master.take_writer().expect("pty writer");
    let _child = PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth attach"));
    drop(pair.slave);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(buf.clone()));

    assert!(
        wait_for_bytes(&buf, b"[attached to alt-pass]", Duration::from_secs(10)).await,
        "陽性の対照: attach のバナーが出ない"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = buf.lock().unwrap().len();
    keys.write_all(b"printf '\\033[?1049hINSIDE_ALT\\033[?1049l'; echo BACK_''OUT\n")
        .ok();
    keys.flush().ok();
    assert!(
        wait_for_bytes(&buf, b"BACK_OUT", Duration::from_secs(10)).await,
        "陽性の対照: 打った行が実行されない"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = buf.lock().unwrap()[before..].to_vec();
    let text = String::from_utf8_lossy(&seen);
    assert!(
        contains(&seen, b"\x1b[?1049h") && contains(&seen, b"\x1b[?1049l"),
        "🚨 部屋が無いのに alt 切替が端末へ届いていない（翻訳か握り潰し）: {text:?}"
    );
    let clears = seen.windows(7).filter(|w| *w == b"\x1b[2J\x1b[H").count();
    assert_eq!(
        clears, 0,
        "🚨 部屋が無いのにクリアへ翻訳している（端末の履歴を消す）: {text:?}"
    );
    drop(terminal);
}

/// セッション内の子プロセスへ `COLORTERM=truecolor` が自動設定されること。
/// Windows（ConPTY）配下の CLI ツール（Node / Bun / Rust 製など）が ANSI カラー非対応と誤認して
/// 白黒になるのを防ぐ。
#[tokio::test]
async fn the_session_receives_colorterm_env() {
    let f = common::Fixture::start("colorterm");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("ct-tab".into()),
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
            session_id: "ct-tab".into(),
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

    let _preamble = become_a_terminal(&mut c, Duration::from_secs(20)).await;

    #[cfg(windows)]
    let probe = line("echo CT_''VAL=$env:COLORTERM");
    #[cfg(not(windows))]
    let probe = line("echo CT_''VAL=$COLORTERM");

    write_frame(&mut c, &Frame::data(0, probe)).await.unwrap();
    let out = drain_data_until(&mut c, b"CT_VAL=truecolor", Duration::from_secs(5)).await;
    assert!(
        contains(&out, b"CT_VAL=truecolor"),
        "🚨 セッションに COLORTERM=truecolor が渡されていない: {:?}",
        String::from_utf8_lossy(&out)
    );
}
