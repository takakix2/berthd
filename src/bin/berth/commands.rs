use crate::wire::{explain, handshake, request, send, Welcome};
use crate::{connect_daemon, say, USAGE};
use berthd::{endpoint_display, Control, SessionInfo, Unreachable, PROTOCOL_VERSION};

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

pub(crate) async fn cmd_ls() -> Result<(), String> {
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
            "{mark} {:<name_w$}  {:<shell_w$}  {}x{}  {}{}{}",
            s.name,
            s.shell,
            s.cols,
            s.rows,
            s.cwd,
            cwd_mark(&s.cwd_src),
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

#[cfg(test)]
mod tests {
    use super::cwd_mark;

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
}
