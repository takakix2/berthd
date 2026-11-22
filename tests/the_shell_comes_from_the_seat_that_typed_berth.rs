//! **セッションのシェルは、`berth` を打った端末で走っているシェル**であることと、
//! **台帳がそれを出所つきで書く**こと。
//!
//! # ここでしか測れないこと
//!
//! 単体（`lib.rs` の解決）は「値の選び方」しか守らない。その選択が本当に
//! `Control::Spawn` に載って、berthd が起こす物を変えているかは、実物の `berth` を
//! **実物のシェルの子として**起こさないと言えない（「berthd の環境が勝つ」のは配線の欠陥）。
//!
//! # 注意: 検体の作り方（ここを間違えると測りたい物が測れない）
//!
//! - **daemon の `SHELL` は `/bin/bash`** にして起こす（配線が切れていればこれが出る）。
//!   ∴ 陽性の対照が要らない: 結果が bash なら「効いていない」と一目で分かる。
//! - **`berth` の親は `zsh`**（pty の中で対話的に起こし、そこへ打ち込む）。
//!   `zsh -c 'berth …'` では駄目。シェルは `-c` の 1 コマンドを `exec` することがあり、
//!   そうなると `berth` の親がシェルでなくなる（測りたい機構が消える）。
//! - `berth` にも `SHELL=/bin/bash` を渡す（観測が `$SHELL` に勝つことまで見る）。
//!
//! # 重要: 親を `/bin/sh` にしてはいけない
//!
//! macOS の `/bin/sh` は bash であり、`/bin/sh` として起こしたプロセスに実行ファイルを訊くと
//! `/bin/bash` と答える。∴ 「観測が勝った」と「daemon の環境が勝った」のどちらも `/bin/bash` になり、
//! 区別できなくなる。打った綴りと、走っている実行ファイルは別物。∴ 神託は綴りの一致ではなく
//! 「`zsh` で終わること」＋「出所が `client:parent_shell` であること」で書く。
#![cfg(unix)]

mod common;

use berthd::{Control, DaemonStream, SessionInfo};
use common::{read_ctrl, send_ctrl, wait_for_daemon, Fixture, PtyChildGuard};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::time::Duration;

/// 親に据えられる `zsh` を探す（`/etc/shells` に basename が居ることまで確かめる）。
///
/// 注意: 満たさない環境で黙って緑にしない。何も測っていないのと、測って通ったのは別の事実。
/// 判定そのものの検体は `lib.rs` の単体が持つので、ここで測れなくても網に穴は開かない。
fn a_parent_shell_that_is_not_bash() -> Option<String> {
    let shells = berthd::login_shells();
    for candidate in ["/bin/zsh", "/usr/bin/zsh"] {
        if std::path::Path::new(candidate).exists()
            && berthd::looks_like_a_login_shell(candidate, &shells)
        {
            return Some(candidate.to_string());
        }
    }
    eprintln!(
        "⚠️ 測れなかった: /etc/shells に載った zsh が無い \
         （bash 以外の親が要る —— macOS の /bin/sh は bash と答えるため）"
    );
    None
}

/// `berth` を打った端末。全部持ったまま生かしておく（どれか 1 つでも落とすと `berth` が畳まれる）。
struct Seat {
    _berth: PtyChildGuard,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    _terminal: common::PtyTerminal,
    /// 打ち込む口。**落とさない**（落とすと `^D` が中のシェルへ届き、
    /// 起きたばかりのセッションが即座に終わる（`[attached to seat]` の直後に `exit`）。
    _writer: Box<dyn std::io::Write + Send>,
    /// pty に届いたバイト（**失敗したとき、シェルが何と言ったかを出すため**）。
    seen: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

/// pty の中で走っているシェルへ任意のコマンド行を打ち込む。
fn type_command_into_a_shell(
    endpoint: &std::ffi::OsStr,
    journal: &std::path::Path,
    parent_shell: &str,
    cmd_line: &str,
) -> Seat {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(parent_shell);
    // 注意: env は継ぎ足しでなく組み立てる。`cargo test` を走らせている環境の `SHELL` が
    // 紛れ込むと、「$SHELL に勝った」のか「たまたま同じ」のかが見分けられなくなる。
    cmd.env("BERTHD_SOCKET", endpoint);
    cmd.env("BERTHD_JOURNAL_PATH", journal);
    cmd.env("SHELL", "/bin/bash");
    cmd.env("TERM", "xterm-256color");
    // 利用者の本物の `~/.berth/config.toml` は読まない（`default-shell` が親の観測に勝ち、検体が利用者の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
    // 注意: この検体の親シェルは自分の起動ファイルを読む。`HOME` を消さないのは、
    // 読むこと自体は測りたい物に影響しない（見るのは `berth` の親が誰かだけ）ため。
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    let child = PtyChildGuard::new(
        pair.slave
            .spawn_command(cmd)
            .unwrap_or_else(|e| panic!("spawn {parent_shell}: {e}")),
    );
    drop(pair.slave);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_as_terminal(&*pair.master, Some(seen.clone()));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while seen.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "pty の中のシェルが 10 秒間 1 バイトも出さない（打ち込む相手が居ない）"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut writer = pair.master.take_writer().expect("pty writer");
    std::io::Write::write_all(&mut writer, cmd_line.as_bytes()).expect("打ち込む");
    std::io::Write::flush(&mut writer).expect("flush");
    Seat {
        _berth: child,
        _master: pair.master,
        _terminal: terminal,
        _writer: writer,
        seen,
    }
}

/// `berth attach <name>` を、**pty の中で走っているシェルに打ち込む**。
/// 返すのは pty の番人と、読み手（どちらも落とすまで生かしておくこと）。
fn type_berth_attach_into_a_shell(
    endpoint: &std::ffi::OsStr,
    journal: &std::path::Path,
    parent_shell: &str,
    name: &str,
) -> Seat {
    type_command_into_a_shell(
        endpoint,
        journal,
        parent_shell,
        &format!("{} attach {name}\n", env!("CARGO_BIN_EXE_berth")),
    )
}

/// pty に届いたバイトを読める形で返す（失敗したとき、シェルが何と言ったかを出すため）。
///
/// これが無いと、検体は「セッションが起きなかった」としか言えず、シェルが出していた説明を捨てることになる。
fn what_the_shell_said(seen: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&seen.lock().unwrap()).into_owned()
}

/// 台帳から唯一の `spawn:started` レコードを取り出す（複数の検体が同じ抽出をする）。
fn the_single_spawn_record(journal: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(journal).expect("台帳が書かれていること");
    let spawns: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            v.get("type").and_then(serde_json::Value::as_str) == Some("spawn")
                && v.get("event").and_then(serde_json::Value::as_str) == Some("started")
        })
        .collect();
    assert_eq!(
        spawns.len(),
        1,
        "🚨 spawn:started の記録が 1 件でない: {text}"
    );
    spawns.into_iter().next().unwrap()
}

/// そのセッションが一覧に出るまで待って、行ごと返す。
async fn session_named(c: &mut DaemonStream, name: &str, within: Duration) -> Option<SessionInfo> {
    let deadline = std::time::Instant::now() + within;
    let mut serial = 100;
    loop {
        serial += 1;
        send_ctrl(c, serial, &Control::List).await;
        if let Control::Sessions { sessions } = read_ctrl(c).await {
            if let Some(s) = sessions.into_iter().find(|s| s.name == name) {
                return Some(s);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_session_takes_the_shell_that_was_running_when_berth_was_typed() {
    let Some(parent) = a_parent_shell_that_is_not_bash() else {
        return;
    };
    // daemon の環境は **bash**。配線が切れていれば、これがセッションのシェルになる。
    let fx = Fixture::start_with_env("shell-from-seat", &[("SHELL", "/bin/bash")]);
    let mut c = wait_for_daemon(&fx.endpoint, Duration::from_secs(5)).await;

    let seat = type_berth_attach_into_a_shell(&fx.endpoint, &fx.journal, &parent, "seat");

    let session = session_named(&mut c, "seat", Duration::from_secs(10))
        .await
        .unwrap_or_else(|| {
            panic!(
                "`berth attach` がセッションを起こさない。pty に届いた物: {:?}",
                what_the_shell_said(&seat.seen)
            )
        });
    assert!(
        session.shell.ends_with("zsh"),
        "🚨 セッションのシェルが、`berth` を打った席で走っているシェルでない: {} \
         （/bin/bash なら berthd の環境か $SHELL が勝っている）",
        session.shell
    );
}

#[tokio::test]
async fn the_ledger_records_the_shell_and_who_decided_it() {
    let Some(parent) = a_parent_shell_that_is_not_bash() else {
        return;
    };
    let fx = Fixture::start_with_env("shell-in-ledger", &[("SHELL", "/bin/bash")]);
    let mut c = wait_for_daemon(&fx.endpoint, Duration::from_secs(5)).await;

    let seat = type_berth_attach_into_a_shell(&fx.endpoint, &fx.journal, &parent, "ledger");
    let session = session_named(&mut c, "ledger", Duration::from_secs(10))
        .await
        .unwrap_or_else(|| {
            panic!(
                "`berth attach` がセッションを起こさない。pty に届いた物: {:?}",
                what_the_shell_said(&seat.seen)
            )
        });

    // 台帳は attach の行より先に spawn の行を持つ（起こしてから入るので）。
    let rec = the_single_spawn_record(&fx.journal);
    assert_eq!(
        rec.get("shell").and_then(serde_json::Value::as_str),
        Some(session.shell.as_str()),
        "台帳のシェルが一覧と食い違う: {rec}"
    );
    assert!(
        session.shell.ends_with("zsh"),
        "起こしたのは打った席のシェルであること: {rec}"
    );
    assert_eq!(
        rec.get("shell_src").and_then(serde_json::Value::as_str),
        Some("client:parent_shell"),
        "🚨 出所が書かれていない ＝ 「なぜこのシェルか」をまた調査することになる: {rec}"
    );
    // 出所の欄が在るだけでは足りない。**観測の隣に居る**ことまで見る
    //   （台帳は 1 行で読めることに値打ちが在る）。
    assert!(
        rec.get("peer_pid").is_some(),
        "spawn の行にも接続の観測が載ること: {rec}"
    );
    // 注意: 名前ではなく id で指す。`berth attach <name>` は起こしてから改名するので、
    // spawn の瞬間の名前は既定名（`session-1`）。台帳の join 鍵は最初から id の方。
    assert_eq!(
        rec.get("berthd_session")
            .and_then(serde_json::Value::as_str),
        Some(session.id.as_str()),
        "どのセッションの spawn かが 1 行で読めること: {rec}"
    );
}

#[tokio::test]
async fn the_cli_shell_flag_overrides_parent_shell_and_records_cli_flag() {
    let Some(parent) = a_parent_shell_that_is_not_bash() else {
        return;
    };
    let fx = Fixture::start_with_env("shell-cli-flag", &[("SHELL", "/bin/bash")]);
    let mut c = wait_for_daemon(&fx.endpoint, Duration::from_secs(5)).await;

    let seat = type_command_into_a_shell(
        &fx.endpoint,
        &fx.journal,
        &parent,
        &format!(
            "{} attach -s /bin/sh flag-override\n",
            env!("CARGO_BIN_EXE_berth")
        ),
    );
    let session = session_named(&mut c, "flag-override", Duration::from_secs(10))
        .await
        .unwrap_or_else(|| {
            panic!(
                "`berth attach -s` がセッションを起こさない。pty に届いた物: {:?}",
                what_the_shell_said(&seat.seen)
            )
        });

    assert_eq!(
        session.shell, "/bin/sh",
        "セッションのシェルが -s で指定した /bin/sh になっていない: {}",
        session.shell
    );

    let rec = the_single_spawn_record(&fx.journal);
    assert_eq!(
        rec.get("shell").and_then(serde_json::Value::as_str),
        Some("/bin/sh"),
        "台帳のシェルが指定したシェルと異なる: {rec}"
    );
    assert_eq!(
        rec.get("shell_src").and_then(serde_json::Value::as_str),
        Some("client:cli_flag"),
        "🚨 出所が client:cli_flag になっていない: {rec}"
    );
}
