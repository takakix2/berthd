//! berthd — PTY を所有する headless デーモン。client は unix socket（Windows は名前付きパイプ）でつなぐ。
//!
//! 扱うのは spawn / attach / data / resize / detach / close / output / exited。
//! セッションの表は接続を跨いで生存する（client が死んでも子プロセスは berthd の下で走り続ける）。
//! PTY の読み書きは専用スレッドで行い、tokio の mpsc で io ループへ渡す。

use std::collections::HashMap;
use std::sync::Arc;
// unix 固有の型（`OwnedWriteHalf` / `UnixListener` / `UnixStream`）は使わない。
// 待ち口と接続は `berthd::{bind_daemon, connect_daemon, DaemonConn}` が面ごとに持つ。
use tokio::io::WriteHalf;

/// 1 接続の書き口。transport 非依存で、unix socket でも名前付きパイプでも同じ型に落ちる。
type ConnWriter = WriteHalf<berthd::DaemonConn>;
use tokio::sync::Mutex as TokioMutex;

/// 直近の出力をどれだけ持つか（1 session あたり）。既定 512KB。
///
/// 注意: 環ではない。上書きで巻き戻るのではなく、押し込むと古い側から出ていく
/// （`VecDeque::pop_front`）ため、常に直近 512KB だけ残る。
/// `ring` とも `sliding window` とも呼ばない（前者は環に、後者は TCP のフロー制御に読めるため）。
const REPLAY_CAP: usize = 512 * 1024;

/// 容量を越えた分を捨てたあと、行頭までさらに捨ててよい上限。
///
/// 切れ目をバイトの位置で決めると、録画の頭に命令の途中（`31m`）や文字の途中（`�`）が残る。∴ 越えた分を捨てた
/// あとも最初の `\n` まで捨て続ける（捨てた分は `HeadTracker` が食うので状態は失わない）。`\n` がこの中に無い
/// 行（進捗バーの `\r` の連打など）は、命令の外・文字の外の最初の位置で切る（頭の 1 行が途中から始まるだけ）。
const HEAD_SLACK: usize = 4 * 1024;
/// 行頭も境界も来ないまま（終端の無い OSC 52 など）捨て続けてよい上限。ここで諦めて切る。
/// トラッカーは残りをあとで食うので、その状態は狂わない（client の見る頭がその回だけ命令の途中になる）。
const HEAD_HARD_CAP: usize = 64 * 1024;

/// 1 回の `push` で束ねる上限。
///
/// PTY のバッファ 1 杯ぶんを目安にした。これ以上ためても、受け手が 1 通で飲める量では
/// なくなるだけで、溢れにくさは増えない。
/// 注意: 下限ではなく上限。キューが空なら 1 通でも即座に押す（待たない）。
const COALESCE_CAP: usize = 64 * 1024;

/// 見る側 1 本が溜められる通数。
///
/// 操作する側（256 通・背圧で待つ）より意図的に小さくしている。見る側は待たせずに外すので、
/// ここは「どれだけ遅れたら外すか」の目盛りそのもの。1 通は最大 `COALESCE_CAP`（64KB）なので
/// 最悪 4MB 遅れたら外れる。注意: 大きくすると、外すまでに berthd が抱えるメモリが見る側の本数だけ増える。
const VIEWER_QUEUE: usize = 64;

/// ログ 1 行。書けなくても死なない（`eprintln!` は書き込み失敗で panic する）。
///
/// 注意: berthd は別のプロセスの子として起こされると、stderr を親から継承する。親が死ぬと
/// そのパイプは壊れ、以後 `eprintln!` は EPIPE で panic する。`kill-server` の途中でそれが起きると、
/// 子は殺したのにそのタスクだけが死に、デーモンと socket が残る（ログの失敗がシャットダウンを止める）。
/// ログは補助であって、制御フローを左右してはならない。
macro_rules! log_line {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod attach;
mod conn;
mod output;
mod pty_loop;
mod sanitize;
mod session;
mod spawn;

use crate::conn::handle_conn;
use crate::session::{hostname, Table};

/// Linux: 起動 1 本ごとの systemd scope。Windows の job（`launch_job`）と同じ場所・近い理由。
#[cfg(target_os = "linux")]
mod session_scope;

/// Windows: 起動 1 本ごとの job object。
///
/// 要る理由: `child.kill()` は直接の子（シェル）しか殺さない。シェルが背景に置いた孫
/// （`sleep 900 &`）は残る。`TerminateJobObject` は孫まで届く。
///
/// 買うのは「木ごと止める」で、「親と一緒に死ぬ」は買わない。∴ `KILL_ON_JOB_CLOSE` は立てない。
/// 後者はもう 1 段上で買ってある（`berthd::join_kill_on_close_job()`（`main()` の入口）が
/// berthd 自身を `KILL_ON_JOB_CLOSE` の job に入れており、子は job の会員資格を継承する）。
/// ここで重ねると、`BREAKAWAY_OK` で開けてある「1 本だけ切り離す」道と噛み合わなくなる。
/// 注意: これは機構を読んだ結論で、berthd を強制終了して session が死ぬところは確かめていない。
///
/// 重要: `BREAKAWAY_OK` は必須。祖先の job がこれを立てていないと、`CREATE_BREAKAWAY_FROM_JOB` を
/// 付けた `CreateProcess` 自体が `ACCESS_DENIED` で落ち、後から切り離す道が 1 本も無くなる。
/// job のフラグは作成時にしか決められないので、立て忘れは後で直せない。
#[cfg(windows)]
mod launch_job;

/// 起動時に決まること。daemon を起こすかどうかは、引数を読み終えてから決める。
enum Cli {
    Run,
    Version,
    Help,
    Unknown(String),
}

/// argv を読む。副作用ゼロ（ここで socket も journal も触らない）。
fn parse_args() -> Cli {
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => Cli::Run,
        Some(a) => match a.as_str() {
            "-V" | "--version" | "version" => Cli::Version,
            "-h" | "--help" | "help" => Cli::Help,
            _ => Cli::Unknown(a),
        },
    }
}

const USAGE: &str = "\
berthd — the daemon that owns the PTYs (your shells outlive the app that opened them)

USAGE:
    berthd                 run in the foreground (this is what the service unit does)
    berthd --version       print the version and exit — this never starts a daemon
    berthd --help          this text

berthd takes no other arguments; sessions are driven by the `berth` client
(`berth ls` / `berth attach <name>` / `berth info`).

ENVIRONMENT:
    BERTHD_SOCKET          socket path (default: $XDG_RUNTIME_DIR/berthd.sock, else ~/.berth/berthd.sock)
    BERTHD_JOURNAL_PATH    audit ledger path (default: ~/.berth/journal.jsonl)

Starting a second berthd on a socket that already answers is a no-op: it logs that
one is running and exits, leaving the live daemon alone.
";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 強制終了でも pty のホストを道連れにする。
    // Rust の `Drop` は `TerminateProcess` では走らないので、OS 側の仕掛けに預ける。
    #[cfg(windows)]
    berthd::pin_dll_search();
    #[cfg(windows)]
    berthd::join_kill_on_close_job();
    // 注意: 引数を見てから socket を触る。argv を見ずに daemon を起こすと、`berthd --version` が
    // 版を答えずにデーモンを増やしてしまう（問いに答えるはずの操作が状態を変えてしまう。
    // 配備中など版を訊きたい場面ほど踏む）。
    // 知らない引数は黙って無視しない（無視は「起動していい」と読み替えることになる）。
    match parse_args() {
        Cli::Run => {}
        Cli::Version => {
            println!("berthd {}", berthd::VERSION_STRING);
            return Ok(());
        }
        Cli::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Cli::Unknown(arg) => {
            eprintln!("berthd: unknown argument: {arg}");
            eprintln!("berthd takes no arguments — it is driven by the `berth` client.");
            eprintln!("Try: berthd --help");
            std::process::exit(2);
        }
    }

    // 待ち口は client と共有（`BERTHD_SOCKET` env 優先 → 既定）。
    // 「パス」ではない（Windows では名前付きパイプの名前で、file system に居ない）。
    let endpoint = berthd::endpoint_display();

    // 既存の待ち口の掃除: 生きた berthd がいれば譲る、いなければ stale を消して bind。
    //
    // 掃除が要るのは unix だけ（unix socket はファイルなので死体が残るが、
    // 名前付きパイプはプロセスが消えれば名前ごと消えるため Windows には stale が無い）。
    #[cfg(unix)]
    {
        let sock = berthd::socket_path();
        if let Some(dir) = sock.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if sock.exists() {
            if berthd::connect_daemon().await.is_ok() {
                log_line!("[berthd] already running at {endpoint}");
                return Ok(());
            }
            std::fs::remove_file(&sock).ok();
        }
    }
    // Windows は `first_pipe_instance(true)` が排他を宣言するので、bind の失敗そのものが
    // 「もう誰か居る」の合図になる（unix の探り接続に相当する）。
    #[cfg(windows)]
    match berthd::connect_daemon().await {
        Ok(_) => {
            log_line!("[berthd] already running at {endpoint}");
            return Ok(());
        }
        Err(e) => {
            let why = berthd::Unreachable::of(e);
            // 重要: 「繋がらない」を「誰も居ない」と読まないこと。口を別のログオンセッションの berthd が
            // 握っていると connect は拒否で返る。それを「誰も居ない」と読んで先へ進むと、
            //    `bind_daemon` が生の os error 5（アクセス拒否）で落ちる。
            //    名前は取られていて、こちらからは畳むことも話すこともできない。断るのが正しい。
            if !why.means_not_running() {
                log_line!("[berthd] not starting: {}", why.describe(&endpoint));
                log_line!(
                    "[berthd] shut that one down from where it was started, or set BERTHD_SOCKET elsewhere"
                );
                // ここで `std::process::exit` してよいのは、まだ何も bind しておらず、
                //    セッションも PTY も 1 つも無い（`Drop` を飛ばして困る物が無い）から。
                //    `Err` で返すと `main` が Debug 形式で刷るので、そうしない。
                std::process::exit(1);
            }
        }
    }

    let mut listener = berthd::bind_daemon().await?;
    // socket の mode を umask に決めさせない。connect に要るのは書き込みビットなので、
    // 所有者だけの `0600` で閉じる（socket に実行ビットの意味は無く、ディレクトリの `0700` とは別）。
    // 注意: bind と chmod の間には一瞬の窓がある。その窓を塞いでいるのは親ディレクトリで、
    // Linux は `$XDG_RUNTIME_DIR`（`drwx------`）、macOS は `ensure_private_dir` が `~/.berth` を
    // `0700` にしている。∴ ここは二枚目の壁であって一枚目ではない。
    // 失敗しても起動は続ける（親が閉じている限り実害は無く、ここで daemon を殺す理由が無い）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let sock = berthd::socket_path();
        if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
            log_line!(
                "[berthd] warning: could not tighten {}: {e}",
                sock.display()
            );
        }
    }
    // Windows の口も締めてあるが、機構は unix と違う。unix は作ってから `chmod` で締められるが、
    // 名前付きパイプの DACL は作成時にしか決められない。∴ 締める場所は `bind_daemon` ではなく
    // `berthd::create_pipe_instance`（`accept()` が作る 2 本目以降も同じ関数を通る）。
    // 名前を分けることは、権限を分けることではない（名前は衝突避けでしかない）。
    log_line!("[berthd] listening on {endpoint}");

    // 重要: pty が 1 つも無いうちに hostname を引いておく。
    // 台帳の行はどれもこれを載せるが、その最初の 1 回が `openpty` の直後に落ちることがあり、
    // Windows ではそこで子を起こすと、その session のシェルが黙る。
    // ここで `OnceLock` を埋めれば、以後の呼び出しは子を起こさない（[`hostname`] の doc）。
    let _ = hostname();

    let table: Table = Arc::new(TokioMutex::new(HashMap::new()));
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let table = table.clone();
                tokio::spawn(handle_conn(stream, table));
            }
            Err(e) => {
                // 重要: 抜けないこと。抜けると生きている全 session が巻き添えになる。accept の失敗は
                // 大半が一時的（fd の枯渇・繋いですぐ閉じた客）なので、少し待って受け直す。
                log_line!("[berthd] accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}
