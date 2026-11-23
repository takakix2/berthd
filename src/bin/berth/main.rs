//! `berth` — berthd のセッションをどのシェルからでも操作する CLI（tmux の `tmux ls` に当たる）。
//!
//! シェルの builtin ではなく独立したバイナリにしてあるのは、berthd の口（unix socket / 名前付きパイプ）を
//! 直接叩けば、hsh / zsh / fish / bash のどれから打っても同じように動くから（tmux がそうであるように）。

/// berthd に接続する。この CLI のすべての接続はここを通る。
///
/// Windows では繋いだ直後・握手の前に、パイプの向こうのプロセスの所有者を自分と照合する
/// （`berthd::refuse_a_server_that_is_not_mine` は Windows 専用の関数のためリンクにしない）。
/// 名前付きパイプの名前空間はグローバルで `CreateNamedPipe` に特権が要らないので、
/// 別のユーザーが `\\.\pipe\berthd-<user>` を先に取れる。
/// 照合しないと、偽のサーバに握手を渡し、偽の名乗りを rc=0 で刷ることになる。
/// 同一ユーザーの別プロセスは通す（既に owner 権限を持つ物は脅威モデルの外）。
/// unix は socket ファイルの所有者と mode が先に守るので、素通し。
async fn connect_daemon() -> std::io::Result<berthd::DaemonStream> {
    let stream = berthd::connect_daemon().await?;
    #[cfg(windows)]
    berthd::refuse_a_server_that_is_not_mine(&stream)?;
    Ok(stream)
}
use std::process::ExitCode;

/// 短い命令（`--version` / `--help` / `info` / `ls` / `rename` / `kill` / `kill-server` / `detach`）の出力は
/// すべてここを通す。読み込み側が先に閉じた（`berth info | head -3` の `head` が抜けた後）ときは黙って rc 0 で終了する。
/// Rust は起動時に SIGPIPE を無視するので、素の `println!` は EPIPE で panic（rc 101）になる。
/// SIGPIPE を既定に戻す案は採らない。attach は unix socket へ書き込むため、berthd が閉じた瞬間に berth が
/// 黙って殺され、`[disconnected …]` が出なくなる。他の書き込み失敗は無視して続ける（出力は結果であって約束ではない）。
fn say(args: std::fmt::Arguments<'_>, newline: bool) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let wrote = out
        .write_fmt(args)
        .and_then(|()| {
            if newline {
                out.write_all(b"\n")
            } else {
                Ok(())
            }
        })
        .and_then(|()| out.flush());
    if let Err(e) = wrote {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
    }
}

/// `println!` の代わり（[`say`]）。
macro_rules! say {
    ($($t:tt)*) => { say(format_args!($($t)*), true) };
}

/// `print!` の代わり（[`say`]）。
macro_rules! say_raw {
    ($($t:tt)*) => { say(format_args!($($t)*), false) };
}

mod attach;
mod commands;
mod daemon_start;
mod wire;

use crate::attach::cmd_attach;
use crate::commands::{
    cmd_detach, cmd_info, cmd_kill, cmd_kill_server, cmd_ls, cmd_peek, cmd_rename, cmd_send,
};
use crate::wire::warn_if_stale;

/// 設定ファイル（`~/.berth/config.toml`）を読む。壊れていれば 1 度名乗って、設定なしとして続ける
/// （attach は止めない。設定は利便性のためのものであり、必須の要件ではない）。
fn config_for_this_seat() -> Option<berthd::BerthConfig> {
    match berthd::load_config() {
        berthd::ConfigOutcome::Loaded { config, .. } => Some(config),
        berthd::ConfigOutcome::Missing { .. } => None,
        berthd::ConfigOutcome::Broken { path, error } => {
            eprintln!(
                "berth: ignoring {}: {}",
                path.display(),
                error.lines().next().unwrap_or("unreadable")
            );
            None
        }
    }
}

/// この CLI が台帳（journal）で名乗る名前。`source` はそのイベントを発行したプロセスの名前。
const SOURCE_CLI: &str = "berth";

/// ユーザーに見える文字列は英語（端末に出るメッセージの規約）。doc コメントは日本語のまま。
const USAGE: &str = "\
berth — drive berthd sessions (persistent shells that own a PTY)

USAGE:
    berth attach [-r] [-s <shell>] [<name>] (a)  attach to a session in this terminal (creates it if new)
                                    -r: watch only — see the output, type nothing (q to leave)
                                    -s, --shell <shell>: shell to run if creating a new session
                                    --no-scrollback: show only the last screen (default: replay the
                                    session's output into this terminal's own scrollback)
                                    --scrollback: replay even inside tmux/zellij/screen
    berth detach [<name>]   (d)     detach the session (defaults to the one you're in)
    berth info                      show which berthd you're talking to (host / version)
    berth ls [--json]               list sessions (* = this shell; --json for scripts, never starts berthd)
    berth rename [<session>] <name> (r)  rename a session (defaults to this one)
    berth send <session> [<text>...]     send input to a session without attaching (appends CR; --no-enter, --ctrl-c, --stdin, --force)
    berth peek <session> [--lines N] [--json]  inspect a session's recent screen and idle time
    berth kill <session>            close a session (kills its child)
    berth kill-server [-f]          shut berthd down (-f if sessions are alive)
    berth --version                 print this client's version (never contacts berthd)

<name> takes a session name (`main` / `iris`) or an id.

Persistence, tmux-style: `berth attach main` to enter, work, then `berth d` to
leave — the shell keeps running under berthd, and `berth attach main` returns to it.
";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("ls");

    // help / info 以外は berthd を触るので、先に版のズレを見る（古い berthd は新しい verb を黙って捨てる）。
    // info は自分で host / version / ズレを出すので、二重に警告しないよう外す。
    // `--version` もここで除外する。バージョンの差異を警告するために berthd へ接続してしまうと、
    // 「daemon に訊かずに自分の版を答える」という目的が消える。
    if !matches!(
        cmd,
        "-h" | "--help" | "help" | "info" | "status" | "-V" | "--version" | "version"
    ) {
        warn_if_stale().await;
    }

    let result = match cmd {
        "info" | "status" => cmd_info().await,
        "ls" | "list" => cmd_ls(args.get(1..).unwrap_or(&[])).await,
        "rename" | "r" => cmd_rename(&args[1..]).await,
        "kill" | "close" => cmd_kill(&args[1..]).await,
        "kill-server" => cmd_kill_server(args[1..].iter().any(|a| a == "-f")).await,
        "attach" | "a" => cmd_attach(&args[1..]).await,
        "detach" | "d" => cmd_detach(&args[1..]).await,
        "send" => cmd_send(&args[1..]).await,
        "peek" => cmd_peek(&args[1..]).await,
        // berthd に訊かずに答える。`berth info` は daemon の自己申告なので、daemon を止めている間
        // （配備の最中など）は版が分からない。これは client 自身の版なので、誰にも訊かない。
        "-V" | "--version" | "version" => {
            say!("berth {}", berthd::VERSION_STRING);
            return ExitCode::SUCCESS;
        }
        "-h" | "--help" | "help" => {
            say_raw!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("berth: unknown command `{other}`\n\n{USAGE}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
