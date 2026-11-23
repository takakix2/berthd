use crate::wire::{explain, handshake, next_control, request, send, send_ctrl, Welcome};
use crate::{connect_daemon, say, USAGE};
use berthd::{
    endpoint_display, read_frame, Control, SessionInfo, Unreachable, KIND_DATA, PROTOCOL_VERSION,
};

pub(crate) async fn list() -> Result<Vec<SessionInfo>, String> {
    match request(&Control::List).await? {
        None => Ok(Vec::new()),
        Some(Control::Sessions { sessions }) => Ok(sessions),
        Some(other) => Err(format!("berth: unexpected reply to list: {other:?}")),
    }
}

/// 名前または id からセッションを引く。人間が扱う handle は名前（id は再利用しない鍵）なので
/// 名前を先に見る。どちらでも引けるのは tmux と同じ割り切り。
pub(crate) fn resolve<'a>(
    sessions: &'a [SessionInfo],
    handle: &str,
) -> Result<&'a SessionInfo, String> {
    sessions
        .iter()
        .find(|s| s.name == handle)
        .or_else(|| sessions.iter().find(|s| s.id == handle))
        .ok_or_else(|| format!("berth: no such session: {handle}"))
}

/// このシェル自身の session id（berthd が `BERTHD_SESSION` で子に渡す）。PTY の外なら `None`。
pub(crate) fn current_session() -> Option<String> {
    std::env::var("BERTHD_SESSION").ok()
}

// 名乗りの解決は `berthd::Claim::from_env`（規則を二重に持たない）。

/// 人に見せる handle（messages 用）: 名前があればそれ、無ければ id。id は再利用しない内部鍵なので、
/// 人が打つ/読むのは名前の方（`berth attach room` なら "room"）。
pub(crate) fn session_handle(s: &SessionInfo) -> String {
    if s.name.is_empty() {
        s.id.clone()
    } else {
        s.name.clone()
    }
}

/// 入れ子ガードの判定（`cmd_attach`）: 自分が入っている session（`current`、すなわち `BERTHD_SESSION` の id）へ
/// attach し直そうとしていれば断りの文言を返す。突き合わせは id で行い、表示には `handle`（名前）を用いる。
/// 人が知っているのは `berth ls` に出る名前で、`s4` のような id ではない。
pub(crate) fn self_attach_refusal(
    current: Option<&str>,
    session_id: &str,
    handle: &str,
) -> Option<String> {
    (current == Some(session_id)).then(|| {
        format!(
            "berth attach: you are already inside session `{handle}` — attaching to it from \
             within loops the terminal.\nDetach first with `berth detach` (or `berth d`), then attach \
             from a shell outside berthd."
        )
    })
}

/// 抜けるときの合図の文言（`cmd_attach` のループが `break` した理由ごと）。
///
/// 経路ごとに言い分ける。`disconnected` は berthd との接続が切れたときのみ使用する。すべてを
/// `[disconnected from X]` にまとめると、どの経路で抜けたかが画面から判別できない。
/// 注意: ASCII 文字のみを使用すること（Windows のコードページ 932 でダッシュが文字化けするため）。
pub(crate) fn farewell_text(reason: &str, handle: &str) -> String {
    match reason {
        "detached" => format!("[detached from {handle}]"),
        "left" => format!("[stopped watching {handle}]"),
        "too_slow" => {
            format!("[detached from {handle}: too slow to keep up - attach -r again to catch up]")
        }
        "exited" => format!("[{handle} exited]"),
        // 手元の入力（stdin）が閉じた。セッションは berthd に残っている。
        "eof" => format!("[input closed - left {handle}]"),
        // 手元の端末へ書けなくなった（同上。この行自体も届かないことが多い）。
        "output_closed" => format!("[terminal closed - left {handle}]"),
        // 外から止められた（SIGTERM / SIGINT）または端末が消えた（SIGHUP）。セッションは berthd に残っている。
        "terminated" => format!("[terminated - left {handle}]"),
        "hangup" => format!("[hung up - left {handle}]"),
        // `closed`（berthd への書き込みか読み取りが切れた）と、名前の無い理由。
        _ => format!("[disconnected from {handle}]"),
    }
}

// ─── サブコマンド ────────────────────────────────────────────

/// `berth info` — 今どの berthd に繋がっているかを berthd 自身に名乗らせる。
/// remote（ssh -L トンネル越し）だと socket パスは不透明なので、host は Welcome の自己申告で得る。
/// Welcome 応答が返ること自体が「生きている」証明（`ls` の「no sessions」に丸める曖昧さを解消）。
pub(crate) async fn cmd_info() -> Result<(), String> {
    let sock = endpoint_display();
    let stream = match connect_daemon().await {
        Ok(s) => s,
        Err(e) => return Err(explain(&Unreachable::of(e), &sock)),
    };
    let (mut rd, mut wr) = tokio::io::split(stream);
    let Welcome {
        server,
        protocol,
        host,
    } = handshake(&mut rd, &mut wr).await?;
    let host = if host.is_empty() {
        "unknown".to_string()
    } else {
        host
    };

    // session 数は別接続で List（info は頻度が低いので 2 往復で十分）。
    let n = list().await?.len();
    let sessions = if n == 0 {
        "none".to_string()
    } else {
        n.to_string()
    };

    // server は "berthd/<ver>" の自己申告。ラベル列は "berthd" 固定なので値は版だけにする。
    let ver = server.rsplit('/').next().unwrap_or(server.as_str());
    // protocol 番号は食い違いの検知専用。互換を気にしなくてよい平常時は隠して version だけ出し
    // （数字を減らす）、古い berthd に当たったときだけ理由付きで顔を出す。
    let berthd = if protocol < PROTOCOL_VERSION {
        format!("v{ver} · outdated (protocol {protocol}, expected {PROTOCOL_VERSION})")
    } else {
        format!("v{ver}")
    };

    // ラベル揃えの表式（値は 8 幅ラベル + 2 空白で桁を合わせる）。
    say!("{:<8}  {host}", "host");
    say!("{:<8}  {berthd}", "berthd");
    // client（この `berth`）と daemon は別々に配備される。remote の箱に置いた berthd だけ
    // 古い、あるいはその逆は日常的に起きるので、両方名乗らせて突き合わせられるようにする。
    // バージョンが同じでも build が異なれば別物であり、それを判別できるのがこの行の主目的である。
    say!("{:<8}  v{}", "client", berthd::VERSION_STRING);
    say!("{:<8}  {sessions}", "sessions");
    say!("{:<8}  {}", "socket", sock);
    // ここで新しいセッションを作ったら何が起きるか。シェルは打った側の観測で決まるので、
    // 実行場所ごとに異なる結果になるため、問い合わせ先が必要になる。
    // berthd には問い合わせない。答えるのは実行側の事実であり、デーモンは関知しない。
    // 設定ファイルの場所と、読めたか（無い・壊れている・読めた）もここで出す。
    let outcome = berthd::load_config();
    match &outcome {
        berthd::ConfigOutcome::Missing { path } => {
            say!("{:<8}  {} (none)", "config", path.display())
        }
        berthd::ConfigOutcome::Loaded { path, config } => say!(
            "{:<8}  {} (default-shell: {}, env: {})",
            "config",
            path.display(),
            config.default_shell.as_deref().unwrap_or("-"),
            config.env.len()
        ),
        berthd::ConfigOutcome::Broken { path, error } => say!(
            "{:<8}  {} (ignored: {})",
            "config",
            path.display(),
            error.lines().next().unwrap_or("unreadable")
        ),
    }
    match berthd::client_shell_with(
        None,
        outcome.config().and_then(|c| c.default_shell.as_deref()),
    ) {
        Some((shell, src)) => say!("{:<8}  {shell} ({})", "shell", src.as_str()),
        // ASCII 文字のみで記述する（Windows の既定のコードページ（932）では `—` が文字化けして出力される）。
        None => say!("{:<8}  (none from this seat - berthd decides)", "shell"),
    }
    Ok(())
}

/// `berth ls` の引数。`--json` だけを受け付ける（知らない語は黙って捨てずに断る）。
fn parse_ls_args(args: &[String]) -> Result<bool, String> {
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other => return Err(format!("berth: ls: unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    Ok(json)
}

/// `berth ls --json` の 1 行。機械が読む形なので、表の見た目（幅・印）とは切り離す。
///
/// `berthd_running` は「berthd が居ない」と「居るがセッションが 0」を分けるための欄。
/// `program_status` は名乗っていなければ `null`（欄を落とさない ＝ `peek --json` と同じ）。
fn ls_json(running: bool, sessions: &[SessionInfo]) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "shell": s.shell,
                "cwd": s.cwd,
                "cwd_src": s.cwd_src,
                "alive": s.alive,
                "cols": s.cols,
                "rows": s.rows,
                "program_status": s.program_status,
            })
        })
        .collect();
    serde_json::json!({ "berthd_running": running, "sessions": rows })
}

pub(crate) async fn cmd_ls(args: &[String]) -> Result<(), String> {
    let json = parse_ls_args(args)?;
    if json {
        // `list()` は berthd が居ないと空を返すので、居るかどうかはここで直に見る。
        let (running, sessions) = match request(&Control::List).await? {
            None => (false, Vec::new()),
            Some(Control::Sessions { sessions }) => (true, sessions),
            Some(other) => return Err(format!("berth: unexpected reply to list: {other:?}")),
        };
        say!("{}", ls_json(running, &sessions));
        return Ok(());
    }
    let sessions = list().await?;
    if sessions.is_empty() {
        say!("no berthd sessions");
        return Ok(());
    }
    let me = current_session();
    // 名前は可変長なので幅を実測して揃える（固定幅だと長い名前で崩れる）。
    let name_w = sessions
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let shell_w = sessions
        .iter()
        .map(|s| s.shell.len())
        .max()
        .unwrap_or(5)
        .max(5);
    for s in &sessions {
        let mark = if me.as_deref() == Some(s.id.as_str()) {
            "*"
        } else {
            " "
        };
        let state = if s.alive { "" } else { "  [exited]" };
        say!(
            "{mark} {:<name_w$}  {:<shell_w$}  {}x{}  {}{}{}{}",
            s.name,
            s.shell,
            s.cols,
            s.rows,
            s.cwd,
            cwd_mark(&s.cwd_src),
            status_mark(s.program_status.as_ref()),
            state,
            name_w = name_w,
            shell_w = shell_w,
        );
    }
    Ok(())
}

/// `cwd` の後ろに付ける印。
///
/// berthd は List のたびに子シェルの今の cwd を引き直すが、引けなければ spawn 時の値へ黙って落ちる
/// （Windows には引く実装が無いので、全行が起動時の値）。印が無いと、この 2 つが同じ顔で並ぶ。
///
/// 列を増やさず、「今ではないものにだけ印を付ける」（黙っている行は live）。
/// 注意: 空文字列と `spawn:` を同じ印にしないこと。空文字列は「古い berthd が通知しなかった」状態であり、
/// 「起動時の値だと分かっている」とは別の状態。
fn cwd_mark(cwd_src: &str) -> &'static str {
    if cwd_src.is_empty() {
        " (?)"
    } else if cwd_src.starts_with("spawn:") {
        " (spawn)"
    } else {
        ""
    }
}

/// 中のプログラムが OSC 7501 で名乗った状態の印（`  [blocked:permission 3m]`）。名乗っていなければ空。
///
/// 経過時間は「その状態になってから」。`blocked` が長いのは確認待ちの放置（Ctrl+C で止めないこと）。
fn status_mark(st: Option<&berth_protocol::ProgramStatus>) -> String {
    let Some(st) = st else {
        return String::new();
    };
    let secs = st.age_ms / 1000;
    let age = if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    };
    match &st.kind {
        Some(kind) => format!("  [{}:{kind} {age}]", st.state),
        None => format!("  [{} {age}]", st.state),
    }
}

pub(crate) async fn cmd_rename(args: &[String]) -> Result<(), String> {
    // `berth rename <name>`（自分を改名）と `berth rename <session> <name>` の両方を受ける。
    // PTY の中から打つときは自分を指すのが自然なので、1 引数形を用意する。
    let (handle, name) = match args {
        [name] => {
            let me = current_session().ok_or_else(|| {
                "berth: this shell is not an berthd session (name the target: berth rename <session> <name>)"
                    .to_string()
            })?;
            (me, name.clone())
        }
        [handle, name] => (handle.clone(), name.clone()),
        _ => return Err(format!("berth: wrong arguments\n\n{USAGE}")),
    };

    let sessions = list().await?;
    let target = resolve(&sessions, &handle)?;
    match request(&Control::Rename {
        session_id: target.id.clone(),
        name: name.clone(),
    })
    .await?
    {
        None => Err("berth: berthd is not running".to_string()),
        Some(Control::Renamed { .. }) => {
            // ASCII だけ（Windows のコードページ 932 で矢印が化ける・`berth info` の行と同じ理由）。
            say!("{} -> {name}", target.name);
            Ok(())
        }
        // 重複名・空白入りは berthd が拒否する（名前で attach できることが名前の存在理由）。
        Some(Control::Error { message, .. }) => Err(format!("berth: {message}")),
        Some(other) => Err(format!("berth: unexpected reply to rename: {other:?}")),
    }
}

pub(crate) async fn cmd_kill(args: &[String]) -> Result<(), String> {
    let handle = args
        .first()
        .ok_or_else(|| format!("berth: name a target\n\n{USAGE}"))?;
    let sessions = list().await?;
    let target = resolve(&sessions, handle)?;

    // 自分自身を kill するのは筋が悪い（自分の足元を撃つ）。exit を案内する。
    if current_session().as_deref() == Some(target.id.as_str()) {
        return Err("berth: refusing to kill this shell (use `exit` to close it)".to_string());
    }

    // Close は応答を返さない（撃ちっぱなし）。効いたかどうかは再 list で確かめる。
    if !send(&Control::Close {
        session_id: target.id.clone(),
    })
    .await?
    {
        return Err("berth: berthd is not running".to_string());
    }
    // 子の後始末（SIGHUP → exit → table から消える）を少しだけ待って結果を見せる。
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let gone = list().await?.iter().all(|s| s.id != target.id || !s.alive);
    if gone {
        say!("killed {}", target.name);
        Ok(())
    } else {
        Err(format!("berth: {} is still alive", target.name))
    }
}

pub(crate) async fn cmd_kill_server(force: bool) -> Result<(), String> {
    let sessions = list().await?;
    let alive: Vec<&SessionInfo> = sessions.iter().filter(|s| s.alive).collect();

    // 生きたセッションがあるのに黙って全部殺すのは事故。巻き添えを見せて止まる（-f で押し切る）。
    if !alive.is_empty() && !force {
        eprintln!("berth: sessions are still running:");
        for s in &alive {
            eprintln!(
                "  {}  {}  {}{}",
                s.name,
                s.shell,
                s.cwd,
                cwd_mark(&s.cwd_src)
            );
        }
        return Err("berth: kill them all with `berth kill-server -f`".to_string());
    }

    // KillServer にも応答は無い（berthd は子を kill → socket を消して exit する）。
    if !send(&Control::KillServer).await? {
        say!("berthd is not running");
        return Ok(());
    }
    // 畳めたことは端点が消えることで確かめる。黙って成功と言わない。
    // 注意: 「接続できない」ではなく「未起動」で判定する。拒否（`ERROR_ACCESS_DENIED`）を
    //    停止の証拠に使うと、止まっていない berthd を「停止した」と刷る。
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        match connect_daemon().await {
            Ok(_) => continue,
            Err(e) => {
                let why = Unreachable::of(e);
                if why.means_not_running() {
                    say!("berthd stopped");
                    return Ok(());
                }
                if matches!(why, Unreachable::Denied) {
                    // 拒否は待っても変わらない。停止の証拠にならないので、そう言って降りる。
                    return Err(explain(&why, &endpoint_display()));
                }
                // 話中・その他は一時的かもしれないので次の周回へ。
            }
        }
    }
    Err("berth: sent kill-server but berthd still answers".to_string())
}

/// `berth detach [<name>]` — 別接続で berthd に `Detach` を送り、その session に今 attach している
/// client（対話 `berth attach`）を蹴る。引数無しは自分が入っている session（`BERTHD_SESSION`）。
pub(crate) async fn cmd_detach(args: &[String]) -> Result<(), String> {
    let handle = match args.first() {
        Some(h) => h.clone(),
        None => current_session().ok_or_else(|| {
            "berth detach: this shell is not an berthd session (name one: berth detach <name>)"
                .to_string()
        })?,
    };
    let sessions = list().await?;
    let target = resolve(&sessions, &handle)?;
    // Detach は応答を返さない（berthd が attached client を蹴る）。送れれば OK。
    if !send(&Control::Detach {
        session_id: target.id.clone(),
    })
    .await?
    {
        return Err("berth: berthd is not running".to_string());
    }
    // 自分が入っている session を畳むとき（`berth d` を中で打つ通常ケース）は黙る:
    // その session を今 attach している client が `[detached from …]` を出すし、この行は
    // session の出力の録画に残って、次の attach で再生されるだけになる。
    if current_session().as_deref() != Some(target.id.as_str()) {
        say!("detached {}", target.name);
    }
    Ok(())
}

/// `berth send` / `berth peek` で要求する最小プロトコル版（14）。
const MIN_PROTO_SEND_PEEK: u32 = 14;

fn protocol_too_old_msg(protocol: u32, need: u32) -> String {
    format!(
        "berth: running berthd has protocol {protocol}, need >= {need}. \
         Replace it with `berth kill-server` (add -f to take live sessions down with it)."
    )
}

async fn connect_v14() -> Result<
    (
        tokio::io::ReadHalf<berthd::DaemonStream>,
        tokio::io::WriteHalf<berthd::DaemonStream>,
    ),
    String,
> {
    let sock = endpoint_display();
    let stream = match connect_daemon().await {
        Ok(s) => s,
        Err(e) => return Err(explain(&Unreachable::of(e), &sock)),
    };
    let (mut rd, mut wr) = tokio::io::split(stream);
    let Welcome { protocol, .. } = handshake(&mut rd, &mut wr).await?;
    if protocol < MIN_PROTO_SEND_PEEK {
        return Err(protocol_too_old_msg(protocol, MIN_PROTO_SEND_PEEK));
    }
    Ok((rd, wr))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SendArgs {
    handle: String,
    data: Vec<u8>,
    force: bool,
}

fn parse_send_args(args: &[String], stdin_bytes: Option<&[u8]>) -> Result<SendArgs, String> {
    let mut handle: Option<String> = None;
    let mut no_enter = false;
    let mut ctrl_c = false;
    let mut force = false;
    let mut use_stdin = false;
    let mut parts: Vec<&str> = Vec::new();
    let mut rest_literal = false;

    for a in args {
        if !rest_literal {
            match a.as_str() {
                "--" => {
                    rest_literal = true;
                    continue;
                }
                "--no-enter" => {
                    no_enter = true;
                    continue;
                }
                "--ctrl-c" => {
                    ctrl_c = true;
                    continue;
                }
                "--force" => {
                    force = true;
                    continue;
                }
                "--stdin" => {
                    use_stdin = true;
                    continue;
                }
                s if s.starts_with('-') && handle.is_some() => {
                    return Err(format!("berth send: unknown option `{s}`\n\n{USAGE}"));
                }
                _ => {}
            }
        }
        if handle.is_none() {
            handle = Some(a.clone());
        } else {
            parts.push(a.as_str());
        }
    }

    let handle = handle.ok_or_else(|| format!("berth send: name a target\n\n{USAGE}"))?;
    let mut data = Vec::new();
    if ctrl_c {
        if !parts.is_empty() || use_stdin {
            return Err("berth send: --ctrl-c cannot be combined with text or --stdin".into());
        }
        data.push(0x03);
        return Ok(SendArgs {
            handle,
            data,
            force,
        });
    }
    if use_stdin {
        if !parts.is_empty() {
            return Err("berth send: --stdin cannot be combined with text arguments".into());
        }
        if let Some(bytes) = stdin_bytes {
            // 末尾の改行 1 つは、このあと付ける Enter と重なって 2 回実行になるので落とす。
            let bytes = match bytes {
                [rest @ .., b'\r', b'\n'] | [rest @ .., b'\n'] if !no_enter => rest,
                all => all,
            };
            data.extend_from_slice(bytes);
        }
    } else {
        if parts.is_empty() && no_enter {
            return Err("berth send: nothing to send".into());
        }
        data.extend_from_slice(parts.join(" ").as_bytes());
    }
    if !no_enter {
        data.push(b'\r');
    }
    Ok(SendArgs {
        handle,
        data,
        force,
    })
}

pub(crate) async fn cmd_send(args: &[String]) -> Result<(), String> {
    let want_stdin = args.iter().any(|a| a == "--stdin") && !args.iter().any(|a| a == "--ctrl-c");
    let stdin_buf = if want_stdin {
        use std::io::Read as _;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| format!("berth send: stdin: {e}"))?;
        Some(buf)
    } else {
        None
    };
    let parsed = parse_send_args(args, stdin_buf.as_deref())?;
    let sessions = list().await?;
    let target = resolve(&sessions, &parsed.handle)?;

    let (mut rd, mut wr) = connect_v14().await?;
    send_ctrl(
        &mut wr,
        2,
        &Control::SendKeys {
            session_id: target.id.clone(),
            data: parsed.data,
            force: parsed.force,
        },
    )
    .await
    .map_err(|e| format!("berth: send failed: {e}"))?;

    match next_control(&mut rd).await? {
        Control::SendKeysAck { .. } => Ok(()),
        Control::Error { message, .. } => Err(format!("berth: {message}")),
        other => Err(format!("berth: unexpected reply to send: {other:?}")),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PeekArgs {
    handle: String,
    lines: usize,
    json: bool,
}

fn parse_peek_args(args: &[String]) -> Result<PeekArgs, String> {
    let mut handle: Option<String> = None;
    let mut lines: usize = 30;
    let mut json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--lines" | "-n" => {
                let v = it
                    .next()
                    .ok_or_else(|| "berth peek: --lines requires a number".to_string())?;
                lines = v
                    .parse::<usize>()
                    .map_err(|_| format!("berth peek: invalid --lines value `{v}`"))?;
            }
            s if s.starts_with('-') => {
                return Err(format!("berth peek: unknown option `{s}`\n\n{USAGE}"));
            }
            _ if handle.is_none() => handle = Some(a.clone()),
            other => {
                return Err(format!(
                    "berth peek: unexpected argument `{other}`\n\n{USAGE}"
                ))
            }
        }
    }
    let handle = handle.ok_or_else(|| format!("berth peek: name a target\n\n{USAGE}"))?;
    Ok(PeekArgs {
        handle,
        lines,
        json,
    })
}

/// `Peek` の応答を画面にする。
///
/// 録画は末尾だけが届くので、切られた分が残した状態（alt screen・装飾）は `replay_head` から作り直して
/// 頭に付ける。付けないと、alt screen の中に居るセッションを通常画面として読んでしまう。
/// 作り直しの改行が頭に積む空行は、切れた頭の手前なので落とす。
fn peek_screen(
    head: Option<&berth_protocol::ReplayHead>,
    bytes: &[u8],
    cols: u16,
    rows: u16,
) -> berth_screen::PlainScreen {
    let Some(head) = head else {
        return berth_screen::render_plain(bytes, cols, rows);
    };
    let mut replay = berthd::replay_head::head_prefix(head, rows);
    replay.extend_from_slice(bytes);
    let mut screen = berth_screen::render_plain(&replay, cols, rows);
    let blank = screen.lines.iter().take_while(|l| l.is_empty()).count();
    screen.lines.drain(..blank);
    screen
}

pub(crate) async fn cmd_peek(args: &[String]) -> Result<(), String> {
    const PEEK_MAX_BYTES: usize = 128 * 1024;
    let parsed = parse_peek_args(args)?;
    let sessions = list().await?;
    let target = resolve(&sessions, &parsed.handle)?;

    let (mut rd, mut wr) = connect_v14().await?;
    send_ctrl(
        &mut wr,
        2,
        &Control::Peek {
            session_id: target.id.clone(),
            max_bytes: Some(PEEK_MAX_BYTES),
        },
    )
    .await
    .map_err(|e| format!("berth: send failed: {e}"))?;

    let (data_len, replay_head, cols, rows, idle_ms, attached, program_status) =
        match next_control(&mut rd).await? {
            Control::PeekResult {
                data_len,
                replay_head,
                cols,
                rows,
                idle_ms,
                attached,
                program_status,
                ..
            } => (
                data_len,
                replay_head,
                cols,
                rows,
                idle_ms,
                attached,
                program_status,
            ),
            Control::Error { message, .. } => return Err(format!("berth: {message}")),
            other => return Err(format!("berth: unexpected reply to peek: {other:?}")),
        };

    let bytes = if data_len > 0 {
        loop {
            let f = read_frame(&mut rd)
                .await
                .map_err(|e| format!("berth: read: {e}"))?;
            if f.kind == KIND_DATA {
                break f.payload;
            }
        }
    } else {
        Vec::new()
    };

    let screen = peek_screen(replay_head.as_ref(), &bytes, cols, rows);
    let tail: Vec<String> = if parsed.lines == 0 {
        Vec::new()
    } else {
        let start = screen.lines.len().saturating_sub(parsed.lines);
        screen.lines[start..].to_vec()
    };

    if parsed.json {
        let doc = serde_json::json!({
            "session_id": target.id,
            "name": target.name,
            "cols": cols,
            "rows": rows,
            "idle_ms": idle_ms,
            "attached": attached,
            "in_alt_screen": screen.in_alt_screen,
            // 中のプログラムが OSC 7501 で名乗った最後の状態（名乗っていなければ null）。
            "program_status": program_status,
            "lines": tail,
        });
        say!("{doc}");
    } else {
        for line in &tail {
            say!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::cwd_mark;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn send_appends_enter_unless_told_not_to() {
        use super::parse_send_args as p;
        let got = p(&argv(&["w1", "echo", "hi"]), None).unwrap();
        assert_eq!(got.handle, "w1");
        assert_eq!(got.data, b"echo hi\r");
        assert_eq!(
            p(&argv(&["w1", "--no-enter", "y"]), None).unwrap().data,
            b"y"
        );
        // 文字なしは Enter だけ。
        assert_eq!(p(&argv(&["w1"]), None).unwrap().data, b"\r");
        assert_eq!(p(&argv(&["w1", "--ctrl-c"]), None).unwrap().data, b"\x03");
        // `--` の後ろはオプションに見えても文字。
        assert_eq!(
            p(&argv(&["w1", "--", "--force"]), None).unwrap().data,
            b"--force\r"
        );
        assert!(p(&argv(&["w1", "--bogus"]), None).is_err());
        assert!(p(&argv(&["w1", "--ctrl-c", "x"]), None).is_err());
        assert!(p(&argv(&["w1", "--no-enter"]), None).is_err());
        assert!(p(&argv(&[]), None).is_err());
    }

    #[test]
    fn ls_takes_only_json() {
        use super::parse_ls_args;
        assert_eq!(parse_ls_args(&argv(&[])), Ok(false));
        assert_eq!(parse_ls_args(&argv(&["--json"])), Ok(true));
        assert!(parse_ls_args(&argv(&["--jsno"])).is_err());
        assert!(parse_ls_args(&argv(&["main"])).is_err());
    }

    #[test]
    fn ls_json_keeps_the_fields_a_script_reads() {
        use super::ls_json;
        use berth_protocol::{ProgramStatus, SessionInfo};
        let s = |id: &str, st: Option<ProgramStatus>| SessionInfo {
            id: id.into(),
            name: format!("n-{id}"),
            shell: "bash".into(),
            cwd: "/w".into(),
            cwd_src: "proc".into(),
            alive: true,
            cols: 80,
            rows: 24,
            program_status: st,
        };
        let blocked = ProgramStatus {
            state: "blocked".into(),
            kind: Some("permission".into()),
            app: Some("codex".into()),
            age_ms: 7_000,
            ..ProgramStatus::default()
        };
        let v = ls_json(true, &[s("a", Some(blocked)), s("b", None)]);
        assert_eq!(v["berthd_running"], true);
        let rows = v["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "n-a");
        assert_eq!(rows[0]["alive"], true);
        assert_eq!(rows[0]["program_status"]["state"], "blocked");
        assert_eq!(rows[0]["program_status"]["kind"], "permission");
        assert_eq!(rows[0]["program_status"]["age_ms"], 7_000);
        // 名乗っていない物は欄ごと落とさず null（「欄が無い」と「名乗っていない」を混ぜない）。
        assert!(rows[1].as_object().unwrap().contains_key("program_status"));
        assert!(rows[1]["program_status"].is_null());
        // berthd が居なければ、空の一覧と「居ない」を分けて言う。
        let none = ls_json(false, &[]);
        assert_eq!(none["berthd_running"], false);
        assert_eq!(none["sessions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_mark_shows_the_state_kind_and_age() {
        use super::status_mark;
        use berth_protocol::ProgramStatus;
        assert_eq!(status_mark(None), "");
        let st = |state: &str, kind: Option<&str>, age_ms| ProgramStatus {
            state: state.into(),
            kind: kind.map(Into::into),
            age_ms,
            ..ProgramStatus::default()
        };
        assert_eq!(status_mark(Some(&st("idle", None, 4_000))), "  [idle 4s]");
        assert_eq!(
            status_mark(Some(&st("blocked", Some("permission"), 185_000))),
            "  [blocked:permission 3m]"
        );
        assert_eq!(
            status_mark(Some(&st("working", None, 7_300_000))),
            "  [working 2h]"
        );
    }

    /// `echo cmd | berth send --stdin` の末尾の改行は Enter と重ねない（2 回実行になる）。
    #[test]
    fn stdin_trailing_newline_is_not_a_second_enter() {
        use super::parse_send_args as p;
        let a = argv(&["w1", "--stdin"]);
        assert_eq!(p(&a, Some(b"ls\n")).unwrap().data, b"ls\r");
        assert_eq!(p(&a, Some(b"ls\r\n")).unwrap().data, b"ls\r");
        assert_eq!(p(&a, Some(b"a\nb\n")).unwrap().data, b"a\nb\r");
        // `--no-enter` は渡された物をそのまま送る。
        let raw = argv(&["w1", "--stdin", "--no-enter"]);
        assert_eq!(p(&raw, Some(b"ls\n")).unwrap().data, b"ls\n");
    }

    /// 切られた録画の頭が alt screen の中だったなら、覗いた画面も alt screen と答える。
    #[test]
    fn a_cut_peek_keeps_the_alt_screen_it_was_in() {
        use super::peek_screen;
        let head = berth_protocol::ReplayHead {
            alt_screen: true,
            ..Default::default()
        };
        let tail = b"status line\r\n> ";
        assert!(peek_screen(Some(&head), tail, 80, 24).in_alt_screen);
        assert!(!peek_screen(None, tail, 80, 24).in_alt_screen);
        // 通常画面で頭が切れているとき、作り直しの改行は空行として残さない。
        let plain = berth_protocol::ReplayHead::default();
        assert_eq!(
            peek_screen(Some(&plain), tail, 80, 24).lines,
            vec!["status line", ">"]
        );
    }

    /// `disconnected` は berthd との接続が切れたときだけ。手元の入力・端末が閉じたときは別の文言。
    #[test]
    fn each_way_out_says_which_end_closed() {
        use super::farewell_text as f;
        assert_eq!(f("closed", "t1"), "[disconnected from t1]");
        assert_eq!(f("eof", "t1"), "[input closed - left t1]");
        assert_eq!(f("output_closed", "t1"), "[terminal closed - left t1]");
        assert_eq!(f("detached", "t1"), "[detached from t1]");
        for r in [
            "closed",
            "eof",
            "output_closed",
            "detached",
            "left",
            "too_slow",
            "exited",
        ] {
            assert!(f(r, "t1").is_ascii(), "{r}: ASCII でない（932 で化ける）");
        }
    }

    /// 入れ子ガードは id で突き合わせ、名前で言う（id しか持たない session は id のまま）。
    #[test]
    fn the_self_attach_refusal_names_the_session_not_its_id() {
        let msg = super::self_attach_refusal(Some("s4"), "s4", "iris").expect("断るはず");
        assert!(msg.contains("session `iris`"), "名前で言っていない: {msg}");
        assert!(!msg.contains("`s4`"), "id を見せている: {msg}");
        // 名前が同じでも id が違えば別の session（入れ子でも安全）。
        assert!(super::self_attach_refusal(Some("s5"), "s4", "iris").is_none());
        assert!(super::self_attach_refusal(None, "s4", "iris").is_none());
        // 名前の無い session は handle が id そのもの。
        let msg = super::self_attach_refusal(Some("s4"), "s4", "s4").expect("断るはず");
        assert!(msg.contains("session `s4`"), "{msg}");
    }

    /// 黙っている行だけが live。
    ///
    /// ここを緩める（例えば 3 つとも空にする）と、`berth ls` は
    /// 「今の値」と「起動時の値」を同じ顔で並べる。
    #[test]
    fn only_a_live_cwd_goes_unmarked() {
        assert_eq!(cwd_mark("proc"), "", "Linux で引けた値に印は付けない");
        assert_eq!(cwd_mark("proc_pidinfo"), "", "macOS で引けた値も同じ");
        assert_eq!(cwd_mark("spawn:unimplemented:live_cwd"), " (spawn)");
        assert_eq!(cwd_mark("spawn:unavailable:proc"), " (spawn)");
        assert_eq!(cwd_mark("spawn:unavailable:pid"), " (spawn)");
        // 空文字列は `spawn:` と別の印にする。古い berthd は何も通知しないため、
        // 「起動時の値だと分かっている」と混ぜると嘘になる。
        assert_eq!(cwd_mark(""), " (?)");
    }

    #[test]
    fn protocol_too_old_message_explains_kill_server_force_flag() {
        let msg = super::protocol_too_old_msg(13, 14);
        assert!(msg.contains("13") && msg.contains("14"), "{msg}");
        assert!(msg.contains("berth kill-server"), "{msg}");
        assert!(msg.contains("-f"), "{msg}");
    }
}
