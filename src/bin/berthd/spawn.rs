#[cfg(windows)]
use crate::launch_job;
use crate::output::{PtyInput, SessionOutput};
use crate::pty_loop::run_pty_io_loop;
use crate::session::{
    initial_session_name, Session, SharedMaster, SharedWriter, SpawnReservation, Table, COUNTER,
    SPAWNING,
};
#[cfg(target_os = "linux")]
use crate::session_scope;
use berthd::SessionInfo;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc;

/// シェルを起こせなかったときに、人に見せる 1 行。
///
/// portable-pty の失敗をそのまま出すと、内部の姿が見える（例:
/// ``CreateProcessW `"no_such.exe\0"` in cwd `Some("C:\\Users\\me\0")` failed: …``）。原因は
/// 伝わるが、`\0` も `Some(…)` も利用者に見せる物ではない。シェルの名前は打ち間違えやすいので、
/// これは日常の失敗になる。
///
/// 出すのは**シェルの名前と OS の理由だけ**。
/// - unix: portable-pty は `io::Error` をそのまま運ぶ → 鎖から取る。
/// - Windows: 文字列に畳んでから返す（`bail!("{}", msg)`）ので型では取れない →
///   最後の `failed: ` の後ろを理由として取る。
///
/// 重要: 形が変わったら元の全文に落ちる。依存の文言を読む以上、壊れるなら
/// 「冗長に戻る」向きにだけ壊れること（理由を失う向きには壊さない）。
/// 注意: OS の理由はロケールの言語で出る。ここでは揃えない。
fn spawn_failure_message(shell: &str, e: &(dyn std::error::Error + 'static)) -> String {
    let mut link: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = link {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return format!("cannot start `{shell}`: {io}");
        }
        link = err.source();
    }
    let text = e.to_string();
    format!("cannot start `{shell}`: {}", spawn_failure_reason(&text))
}

/// 鎖に `io::Error` が居ればその文言、居なければ畳まれた文字列から理由だけ（台帳の `reason` 用、
/// （`spawn_failure_message` から「cannot start `X`: 」を除いた部分と同じ物）。
fn spawn_failure_reason_of(e: &(dyn std::error::Error + 'static)) -> String {
    let mut link: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = link {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return io.to_string();
        }
        link = err.source();
    }
    spawn_failure_reason(&e.to_string()).to_string()
}

/// シェルを起こせなかったという失敗（`spawn_session` の中でしか分からない文脈、すなわち
/// 実際に起こそうとした物・その出所・cwd）を、台帳を書く呼び手まで運ぶ。
/// 注意: `Display` は人へ返す 1 行そのもの。client に返す文言を変えないこと。
#[derive(Debug)]
pub(crate) struct ShellNotStarted {
    message: String,
    pub(crate) reason: String,
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) shell: String,
    pub(crate) shell_src: String,
    pub(crate) cwd: String,
}

impl std::fmt::Display for ShellNotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ShellNotStarted {}

/// portable-pty が文字列に畳んだ失敗から、理由の部分だけを取る。
///
/// | 面 | portable-pty 0.9 の形 | 取る物 |
/// |---|---|---|
/// | Windows | `CreateProcessW … failed: <OS の理由>` | 最後の `failed: ` の後ろ |
/// | unix | `Unable to spawn X because it does not exist`（`is a directory` / `is not executable`）| `because` の後ろ |
/// | unix | `Unable to spawn X because:` ＋ 改行 ＋ `No viable candidates found in PATH "<PATH 全体>"` | **`not found in PATH`** |
///
/// 重要: unix の最後の形に注意。起動の前に portable-pty が自分で PATH を探し、失敗すると
/// デーモンの PATH 全体を含む複数行を返す。「知らない形は全文」に落とすと、それがそのまま
/// client に出る。
/// 注意: どの形にも当たらなければ全文（理由を失う向きには壊さない）。
fn spawn_failure_reason(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("Unable to spawn ") {
        if let Some((_, why)) = rest.split_once(" because") {
            if why.contains("No viable candidates found in PATH") {
                return "not found in PATH";
            }
            let why = why.trim_start_matches(':').trim();
            return why.lines().next().unwrap_or(why);
        }
    }
    text.rsplit_once(" failed: ").map_or(text, |(_, r)| r)
}

/// ブロッキング reader を 1 スレッドで読み、バイト列を tokio チャネルに流す。
fn spawn_reader_thread(mut reader: Box<dyn Read + Send>, output_tx: mpsc::Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 打鍵を 1 スレッドで PTY へ書く（reader と対）。
///
/// 書き込みは子が読まないと返らないので、tokio のワーカーでも io ループでもなく専用スレッドで待つ。
/// 書けなくなったら（子が終わった）黙って抜ける。子の終わりは io ループが EOF / `try_wait` で拾う。
fn spawn_writer_thread(writer: SharedWriter, mut write_rx: mpsc::Receiver<Vec<u8>>) {
    std::thread::spawn(move || {
        while let Some(bytes) = write_rx.blocking_recv() {
            let Ok(mut guard) = writer.lock() else { break };
            let Some(w) = guard.as_mut() else { break };
            if w.write_all(&bytes).is_err() {
                break;
            }
            let _ = w.flush();
        }
    });
}

/// 起こしたセッションのうち、台帳が要る分。
///
/// 注意: `SessionInfo` を返さないのは、あちらの `shell` 欄が表示名（label）に上書きされるため。
/// 台帳が要るのは「実際に起こした実行ファイル」の方（両者が違う client が在る）。
pub(crate) struct Spawned {
    pub(crate) id: String,
    pub(crate) pid: Option<u32>,
    /// spawn した瞬間の名前（注意: `berth attach <name>` はこの後に改名しうる）。
    pub(crate) name: String,
    /// 実際に起こした物。
    pub(crate) shell: String,
    /// 誰が決めたか（`client:*` / `daemon:*`）。
    pub(crate) shell_src: String,
    pub(crate) cwd: String,
}

/// 新しい PTY session を起こして table に登録し、台帳が要る分を返す。
#[allow(clippy::too_many_arguments)] // Spawn の各フィールドをそのまま受ける
pub(crate) async fn spawn_session(
    table: &Table,
    desired_id: Option<String>,
    desired_name: Option<String>,
    shell: Option<String>,
    shell_src: Option<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    label: Option<String>,
    cols: u16,
    rows: u16,
) -> std::io::Result<Spawned> {
    // client 指定 id（frontend タブ id）があれば使う（安定 id で reattach 可能にする）。無ければ採番。
    let id = desired_id.unwrap_or_else(|| format!("s{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
    // 表のロックを握ったまま予約する（確認と予約が 1 つの出来事になる）。
    let _reservation = {
        let tbl = table.lock().await;
        let mut spawning = SPAWNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tbl.contains_key(&id) || !spawning.insert(id.clone()) {
            // 既存 id への spawn は誤り（client は先に attach を試し、miss 時だけ spawn する）。
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session {id} already exists"),
            ));
        }
        SpawnReservation(id.clone())
    };
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|e| std::io::Error::other(format!("openpty: {e}")))?;

    // 誰が決めたかを、決めるのと同じ場所で決める。
    // client が名指ししたなら、出所はあちらの事実（`client:*`）。出所を言わずに
    // 名指しした古い client は `client:explicit`（「客が名指しした」は実際に起きたこと）。
    // `berth` は自分の側で解決して名指しするので、普段ここを通るのは client が何も言わなかった
    // ときだけ。∴ `daemon:*` が並ぶ台帳は「名指ししない client が居る」か
    // 「打った側に手掛かりが無かった」のどちらか。
    let (shell, shell_src) = match shell.filter(|s| !s.is_empty()) {
        Some(s) => {
            let src = shell_src
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("explicit");
            (s, format!("client:{src}"))
        }
        None => {
            let (s, src) = berthd::default_shell_with_src();
            (s, format!("daemon:{}", src.as_str()))
        }
    };
    let mut cmd = CommandBuilder::new(&shell);
    let cwd_str = cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    if let Some(dir) = &cwd {
        cmd.cwd(dir);
    }
    cmd.env("TERM", "xterm-256color");
    // ConPTY / PTY 配下の子プロセス（Node, Bun, Rust 製の CLI 等）が ANSI Truecolor 対応端末と判定できるよう渡す。
    // Windows では TERM=xterm-256color だけではレガシー扱いされ白黒化する CLI が多いため必須。
    cmd.env("COLORTERM", "truecolor");
    // 子に「自分がどの session か」を渡す。`berth`（CLI）がこれを読んで一覧に自分の印を付け、
    // 引数なしの `berth rename <name>` のような「自分に対する操作」を成立させる。
    // tmux が $TMUX / $TMUX_PANE を渡すのと同じ役割（シェルに依らず、zsh でも fish でも読める）。
    cmd.env("BERTHD_SESSION", &id);
    // client 指定の env。TERM の後に当てるので上書きも効く。
    for (k, v) in env.into_iter().flatten() {
        cmd.env(k, v);
    }

    let child = pair.slave.spawn_command(cmd).map_err(|e| {
        // 文脈（何を・誰の指定で・どこで）ごと上へ運ぶ。台帳に `spawn:failed` を書くのは
        // 接続の観測を持っている呼び手の側。人へ返す文言は `Display` がそのまま持つ。
        // portable-pty の失敗は `anyhow::Error`。berthd は anyhow に直に依存していないので、
        // `std::error::Error` の鎖として受ける（`AsRef` がその口）。
        let err: &(dyn std::error::Error + Send + Sync + 'static) = e.as_ref();
        std::io::Error::other(ShellNotStarted {
            message: spawn_failure_message(&shell, err),
            reason: spawn_failure_reason_of(err),
            id: id.clone(),
            name: desired_name.clone(),
            shell: shell.clone(),
            shell_src: shell_src.clone(),
            cwd: cwd_str.clone(),
        })
    })?;
    drop(pair.slave); // slave を閉じる → 子終了時に master reader が EOF

    let pid = child.process_id();

    // 起動 1 本ごとの systemd scope（Linux）。berthd の cgroup から出す。
    // そうしないと、1 本のセッションの重いビルドで systemd-oomd が berthd ごと全セッションを止める。
    // 注意: 失敗しても続ける（session は berthd の cgroup のまま動く）。撃つのは別スレッドなので待たない。
    #[cfg(target_os = "linux")]
    if let Some(p) = pid {
        session_scope::place(p, desired_name.as_deref().unwrap_or(&id));
    }

    // 起動 1 本ごとの job（Windows）。ここで入れておくと、シェルが後から起こす孫も
    // 会員として継承される（`TerminateJobObject` が木ごと届く）。
    // 注意: 失敗しても続ける。job が取れないことは session が使えない理由にならない。
    #[cfg(windows)]
    let job = pid.and_then(|p| {
        let j = launch_job::create()?;
        if launch_job::add(j, p) {
            Some(j)
        } else {
            // 入れられなかった job を持ち歩かない（あとで木を殺したつもりになる）。
            launch_job::terminate(j);
            log_line!("[berthd] session {id}: could not put pid {p} in its own job");
            None
        }
    });

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(std::io::Error::other)?;
    let writer = pair.master.take_writer().map_err(std::io::Error::other)?;

    // master を箱に入れて io ループと session 表で共有する（`List` が大きさを訊く）。
    let master: SharedMaster = Arc::new(StdMutex::new(Some(pair.master)));

    let (input_tx, input_rx) = mpsc::channel::<PtyInput>(64);
    let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(64);
    // io ループも PTY へ書く口を 1 つ持つ（OSC 7501 の問い合わせに答えるため・Windows では
    // 起動直後の `ESC[6n` にも）。
    let write_tx_for_io = write_tx.clone();
    let write_tx_for_table = write_tx;
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);
    spawn_reader_thread(reader, output_tx);
    let writer: SharedWriter = Arc::new(StdMutex::new(Some(writer)));
    spawn_writer_thread(writer.clone(), write_rx);

    let output = Arc::new(StdMutex::new(SessionOutput::new()));

    // 採番→insert を同じロックで行う（空き番号の二重採番レースを防ぐ）。表示名は空きの最小 N。
    // shell 列は client が渡した label 優先（client が自分の実行ファイルをシェルとして起こす形では、
    // そのままだと `berth ls` に無関係な名前が出る）。
    let mut guard = table.lock().await;
    let info = SessionInfo {
        id: id.clone(),
        name: initial_session_name(desired_name.as_deref(), &guard),
        shell: label.unwrap_or_else(|| shell.clone()),
        cwd: cwd_str.clone(),
        // 重要: この控えは外へ出ない。`SessionInfo` を送るのは `Control::List` だけで、
        // あそこは毎回 `cwd_src` を上書きする。空のままにしてあるのは、万一漏れたときに
        // プロトコルが既に持っている意味（「名乗っていない ∴ 信じるな」）へ落ちるため。
        // ここで `proc` などと名乗らせない（それは「今引いた」の意味になる）。
        cwd_src: String::new(),
        alive: true,
        cols,
        rows,
        // 今の値は `List` のたびに `SessionOutput` から引く（ここは控え）。
        program_status: None,
        // `List` のたびに `Session.pid` から入れる（ここは控え）。
        pid: None,
    };
    let spawned = Spawned {
        id: id.clone(),
        pid,
        name: info.name.clone(),
        shell,
        shell_src,
        cwd: cwd_str,
    };
    guard.insert(
        id.clone(),
        Session {
            input_tx,
            write_tx: write_tx_for_table,
            output: output.clone(),
            info,
            pid,
            master: master.clone(),
        },
    );
    drop(guard);

    // I/O ループ。
    let table_c = table.clone();
    let id_c = id.clone();
    #[cfg(windows)]
    tokio::spawn(run_pty_io_loop(
        master,
        writer,
        child,
        input_rx,
        output_rx,
        output,
        table_c,
        id_c,
        job,
        write_tx_for_io,
    ));
    #[cfg(not(windows))]
    tokio::spawn(run_pty_io_loop(
        master,
        writer,
        child,
        input_rx,
        output_rx,
        output,
        table_c,
        id_c,
        write_tx_for_io,
    ));

    Ok(spawned)
}

#[cfg(test)]
mod tests {
    /// 起こせなかったときに人へ見せるのは、シェルの名前と OS の理由だけ。
    #[test]
    fn a_spawn_failure_is_said_in_one_plain_line() {
        use super::spawn_failure_message;
        // ① unix の形: `io::Error` が鎖に居る。
        let io = std::io::Error::from_raw_os_error(2);
        let expected = format!("cannot start `nosuch`: {io}");
        assert_eq!(spawn_failure_message("nosuch", &io), expected);
        // ② Windows の形: portable-pty が文字列に畳んだ物（実物から採った形）。
        let flattened = std::io::Error::other(
            "CreateProcessW `\"nosuch.exe\\0\"` in cwd `Some(\"C:\\\\Users\\0\")` failed: not found (os error 2)",
        );
        // 注意: `io::Error::other` は `io::Error` なので①の枝に入る。②は鎖に `io::Error` が居ない形で撃つ。
        let text_only: Box<dyn std::error::Error> = flattened.to_string().into();
        assert_eq!(
            spawn_failure_message("nosuch.exe", text_only.as_ref()),
            "cannot start `nosuch.exe`: not found (os error 2)"
        );
        // ②' unix の形（実物から採った形）。PATH をそのまま人へ出さない。
        let path_dump: Box<dyn std::error::Error> =
            "Unable to spawn nosuch because:\nNo viable candidates found in PATH \"/a/bin:/b/bin\""
                .into();
        assert_eq!(
            spawn_failure_message("nosuch", path_dump.as_ref()),
            "cannot start `nosuch`: not found in PATH"
        );
        let missing: Box<dyn std::error::Error> =
            "Unable to spawn /opt/x/sh because it does not exist".into();
        assert_eq!(
            spawn_failure_message("/opt/x/sh", missing.as_ref()),
            "cannot start `/opt/x/sh`: it does not exist"
        );
        // ③ 形が変わったら全文に落ちる（理由を失う向きには壊れない）。
        let unknown: Box<dyn std::error::Error> = "something new went wrong".into();
        assert_eq!(
            spawn_failure_message("sh", unknown.as_ref()),
            "cannot start `sh`: something new went wrong"
        );
    }

    /// `$SHELL` を Windows で見ないことを釘付けする。
    ///
    /// git-bash は必ず `SHELL=/bin/bash.exe` を立てるが、あれは MSYS の綴りで
    /// `CreateProcessW` から解決できない（os error 3）。
    /// ∴ 見た瞬間に、git-bash から berthd を起こした人が全員落ちる。
    ///
    /// 切り替えは `BERTHD_SHELL`（面に依らない・`$SHELL` より先）。
    /// 重要: env はプロセス全域なので、この検体は 1 本に畳んである（分けると並列で壊れる）。
    #[test]
    fn the_default_shell_is_chosen_per_face() {
        let saved_berthd = std::env::var("BERTHD_SHELL").ok();
        let saved_shell = std::env::var("SHELL").ok();

        // ① 明示（`BERTHD_SHELL`）は、どちらの面でも勝つ。
        std::env::set_var("BERTHD_SHELL", "hsh.exe");
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(berthd::default_shell(), "hsh.exe", "明示が慣習に勝つこと");

        // ② 明示が無いとき。
        std::env::remove_var("BERTHD_SHELL");
        #[cfg(windows)]
        {
            // `$SHELL` が在っても見ない（これがこの検体の芯）。
            assert_eq!(
                berthd::default_shell(),
                "powershell.exe",
                "Windows は $SHELL を見ない"
            );
            std::env::set_var("SHELL", "/bin/bash.exe"); // git-bash が立てる綴り
            assert_eq!(
                berthd::default_shell(),
                "powershell.exe",
                "MSYS の綴りに引きずられない"
            );
        }
        #[cfg(unix)]
        {
            assert_eq!(berthd::default_shell(), "/bin/zsh", "unix は $SHELL に従う");
            std::env::remove_var("SHELL");
            // 無ければ passwd のログインシェル（在れば）、無ければ `/bin/bash` / `/bin/sh` の在る方。
            let exists = |p: &str| std::path::Path::new(p).exists();
            let expected = berthd::passwd_shell()
                .filter(|p| exists(p))
                .unwrap_or_else(|| {
                    if exists("/bin/bash") {
                        "/bin/bash"
                    } else {
                        "/bin/sh"
                    }
                    .to_string()
                });
            assert_eq!(berthd::default_shell(), expected, "無ければ台の既定");
        }

        match saved_berthd {
            Some(v) => std::env::set_var("BERTHD_SHELL", v),
            None => std::env::remove_var("BERTHD_SHELL"),
        }
        match saved_shell {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }
}
