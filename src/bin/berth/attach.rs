mod alt_screen;
mod args;
mod pane;
mod replay;
mod terminal;

use crate::commands::{
    current_session, farewell_text, list, resolve, self_attach_refusal, session_handle,
};
use crate::daemon_start::ensure_berthd;
use crate::wire::{handshake, next_control, send_ctrl};
use crate::{config_for_this_seat, connect_daemon};
use alt_screen::{AltScreenShim, AttachDump};
use args::{
    inside_multiplexer, parse_attach_args, read_only_supported, scrollback_wanted, AttachArgs,
    SCROLLBACK_ENV,
};
use berthd::{decode_control, read_frame, Control, Frame, SessionInfo, KIND_DATA};
use pane::{current_pane, read_pane_seen, write_pane_seen};
use replay::{
    farewell_bytes, screen_bytes, seen_to_remember, signal_beside_cursor, signal_on_empty_row,
    take_dsr_reply, take_the_window, title_of, title_sequence, wants_term_row, DsrWait,
    DETACH_RESET, DETACH_RESET_NO_ROOM, DSR_LATE, DSR_REQUEST, DSR_WAIT, FRESH_GIVE_UP,
    FRESH_QUIET, FRESH_WATCH_CAP, NUDGE_RESTORE_AFTER, TITLE_CLEAR,
};
use terminal::{send_resize, stdin_is_tty, stdout_is_tty, term_size, Quit, RawMode, WindowChange};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ─── 対話 attach（tmux 型）─────────────────────────────────────
// `ssh <host>` で入った先で `berth attach <name>` を打つと、この端末で berthd の永続セッションが動く
// （tmux attach 相当）。クライアントは **純粋な dumb pipe**: stdin を生バイトで berthd へ、berthd の出力を
// stdout へ流すだけ。エスケープキーの解析も prefix も無い（Ctrl-b 等も素通し）。抜けるのは別コマンド
// `berth detach`（berthd が現在 attach している client を切断する、`Detached` フレーム）。

/// `berth attach` が入る先。
struct Target {
    /// プロトコル用の id（新規作成・無名の場合は空文字列とし、berthd が採番する）。
    session_id: String,
    /// まだ存在しないため、作成してから接続する。
    fresh: bool,
    /// 人に見せる名前（下の doc）。
    handle: String,
    /// 名前を指定された（id をその名前として作成する）。
    named: bool,
}

/// 名前（無ければ唯一の生きているセッション）から、入る先を決める。
async fn choose_target(name: Option<&str>, read_only: bool) -> Result<Target, String> {
    // name → 既存 session の id に解決。無ければ id=name で新規 spawn（+ rename で ls に名前を出す）。
    // `handle` = **人に見せる名前**（`[attached to …]` / `[detached from …]` に出す）。内部の `session_id`
    // （id）とは別物: 既存 session を id で attach した場合や GUI が作ったセッション（id=`s3`・名前=`iris`）でも、
    // ユーザーが知っている名前を一貫して見せる。id はプロトコル用、handle は表示用。
    let sessions = list().await?;
    let (session_id, fresh, handle, named) = match name {
        Some(n) => match resolve(&sessions, n) {
            Ok(s) => (s.id.clone(), false, session_handle(s), true),
            Err(_) => (n.to_string(), true, n.to_string(), true),
        },
        None => {
            let alive: Vec<&SessionInfo> = sessions.iter().filter(|s| s.alive).collect();
            match alive.as_slice() {
                // 引数なし・セッション無し = 新規作成。**id も名前も berthd に採番させる**（session-N）。
                // CLI が "main" を差し込むのは全体で見ると特別扱いなので、既定命名に従う。
                // id/handle は spawn 後に確定する（`spawn_fresh`）。
                [] => (String::new(), true, String::new(), false),
                [only] => (only.id.clone(), false, session_handle(only), true),
                _ => {
                    return Err(
                        "berth attach: multiple sessions — name one (berth attach <name>)"
                            .to_string(),
                    )
                }
            }
        }
    };

    // 見るだけの attach は session を作らない（見る物が無い）。
    if read_only && fresh {
        return Err(match name {
            Some(n) => format!(
                "berth attach -r: no session named `{n}` to watch (watching never creates one)"
            ),
            None => "berth attach -r: no session to watch".to_string(),
        });
    }

    // tmux 型の入れ子ガード: **自分が動いているそのセッションへ attach し直す**と、この client の
    // stdout は自分の PTY へ書き戻り → berthd がそれを読み直して → また sink へ流す…という自己
    // フィードバックになり、端末が出力で溢れる。detach したつもりで中に居たまま `berth attach <同名>` を
    // 実行すると発生する。tmux が `$TMUX` を参照して入れ子を禁止するのと同じ発想で、`BERTHD_SESSION`（現在自分が居る
    // session）と一致したら弾く。別 session への attach は入れ子でも安全。
    if let Some(refusal) = self_attach_refusal(current_session().as_deref(), &session_id, &handle) {
        return Err(refusal);
    }
    Ok(Target {
        session_id,
        fresh,
        handle,
        named,
    })
}

/// 入る先がまだ無いので作る（`Spawn`、名前があれば `Rename`）。確定した (id, 見せる名前) を返す。
async fn spawn_fresh<R, W>(
    rd: &mut R,
    wr: &mut W,
    t: &Target,
    cli_shell: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<(String, String), String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // `berth` を打ったシェルを解決する（`berthd::client_shell_with`）。
    // `None` の場合は何も指定せず、berthd の既定値に任せる（手掛かりが無いとき）。
    // 設定ファイル（`default-shell` と `[env]`）は**新しく作るときだけ**効く。
    let config = config_for_this_seat();
    let (shell, shell_src) = match berthd::client_shell_with(
        cli_shell,
        config.as_ref().and_then(|c| c.default_shell.as_deref()),
    ) {
        Some((shell, src)) => (Some(shell), Some(src.as_str().to_string())),
        None => (None, None),
    };
    let config_env = config
        .as_ref()
        .map(berthd::BerthConfig::env_pairs)
        .filter(|v| !v.is_empty());
    // 名前指定があれば id=名前で spawn、無ければ **berthd に採番させる**（session_id: None → 既定名 session-N）。
    let spawn_id = if t.named {
        Some(t.session_id.clone())
    } else {
        None
    };
    send_ctrl(
        wr,
        2,
        &Control::Spawn {
            session_id: spawn_id,
            // 名前は**作成時点で**渡す（protocol 11）。下の `Rename` だけだと、berthd は spawn の瞬間に
            // 既定名しか知らず、台帳の `spawn:started` が `name: session-1` になる。
            // 古い berthd はこの欄を捨てるので `Rename` は残す。
            name: t.named.then(|| t.handle.clone()),
            // シェルは client が決めて渡す。`None` にすると決めるのは **berthd の環境**で、`berth` は
            // デーモンが居なければ自分で起こすので、最初に起こした端末の環境が焼き付き、以後どの端末から
            // attach してもそのシェルが出る。
            //
            // `$SHELL` だけでは不十分である。bash から別のシェルを起動して使っている場合、`$SHELL` は `/bin/bash` のまま
            // （あれは「ログインシェルに何を選んだか」の申告値）。∴ 親プロセスを観測する。
            //
            // 下の `env` に名乗りを載せないのとは事情が違う: シェルは名乗りではなく**この spawn の引数**で、
            // 後から古くなる余地が無い（起こした物は起こした物のまま）。
            shell,
            shell_src,
            cwd: None,
            // 重要: ここに操作者（actor）の環境変数を載せないこと。載せると子プロセスの台帳記録が**不正確になる**。
            // 子は attach より長生きし、actor はプロセスに 1 回しか解決されないので、同じセッションへ
            // 別の人が後から attach して打った分まで、spawn した人の名前で、しかも「名乗り」として残る。
            // 渡さなければ子の記録は OS 由来の既定値（ログインユーザー）で、誰も「名乗った」とは読まない。
            //
            // 「今打っているのは誰か」は berthd の attach 記録の窓（`berthd_session` ＋ start / end の時刻）で引く。
            // この非対称は env では消せない。
            //
            // 入るのは設定ファイルの `[env]` だけ（無ければ `None`）。
            env: config_env,
            label: None,
            cols,
            rows,
        },
    )
    .await
    .map_err(|e| format!("berth: {e}"))?;
    // 採番の場合はここで id が確定する（Spawned が返す）。
    let session_id = match next_control(rd).await? {
        Control::Spawned { session_id, .. } => session_id,
        Control::Error { message, .. } => return Err(format!("berth: {message}")),
        other => return Err(format!("berth: unexpected reply to spawn: {other:?}")),
    };
    let handle = if t.named {
        // 名前指定 → その名前に rename。これで表示名（`berth ls` の `*` が付く名前）・marker・id が一致する。
        let _ = send_ctrl(
            wr,
            3,
            &Control::Rename {
                session_id: session_id.clone(),
                name: t.handle.clone(),
            },
        )
        .await;
        let _ = next_control(rd).await; // Renamed/Error は無視（attach は続行）
        t.handle.clone()
    } else {
        // 無名 → berthd の既定名（session-N）をそのまま使い、handle をその名前に合わせる（別接続で引く）。
        // これで marker `[attached to session-1]` と `berth ls` の `* session-1` が一致する。
        list()
            .await?
            .iter()
            .find(|s| s.id == session_id)
            .map(session_handle)
            .unwrap_or_else(|| session_id.clone())
    };
    Ok((session_id, handle))
}

/// berthd の `Attached` の返事のうち、client が使う分。
struct Attached {
    replay_len: usize,
    resized: bool,
    replay_head: Option<berth_protocol::ReplayHead>,
    replay_total: Option<u64>,
}

/// `Attach` を送り、`Attached` を受け取る。操作者情報は環境から取得する（無ければ両方 None となり、「指定なし」として台帳に記録される）。
async fn send_attach<R, W>(
    rd: &mut R,
    wr: &mut W,
    session_id: &str,
    cols: u16,
    rows: u16,
    read_only: bool,
    seen_total: Option<u64>,
) -> Result<Attached, String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let claim = berthd::Claim::from_env();
    send_ctrl(
        wr,
        4,
        &Control::Attach {
            session_id: session_id.to_owned(),
            cols,
            rows,
            actor: claim.actor,
            actor_src: claim.src,
            read_only,
            seen_total,
        },
    )
    .await
    .map_err(|e| format!("berth: {e}"))?;
    match next_control(rd).await? {
        Control::Attached {
            replay_len,
            resized,
            replay_head,
            replay_total,
            ..
        } => Ok(Attached {
            replay_len,
            resized,
            replay_head,
            replay_total,
        }),
        Control::Error { message, .. } => Err(format!("berth: {message}")),
        other => Err(format!("berth: unexpected reply to attach: {other:?}")),
    }
}

/// attach 中に端末へ書く口。書いたバイトは計器（[`AttachDump`]）にも同じものを残す。
struct Term {
    stdout: tokio::io::Stdout,
    dump: AttachDump,
}

impl Term {
    /// 書く（flush はしない・書けなくても黙る）。入口でまとめて書いてから 1 回 flush する用。
    async fn put(&mut self, bytes: &[u8]) {
        self.dump.put(bytes);
        let _ = self.stdout.write_all(bytes).await;
    }

    /// 書き込みと flush を行う。書き込めなければ `Err`（端末が閉じた状態）。
    async fn show(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.dump.put(bytes);
        self.stdout.write_all(bytes).await?;
        let _ = self.stdout.flush().await;
        Ok(())
    }
}

/// 打鍵をセッションへ送る。`Err` は抜ける理由。
///
/// 閲覧モードではキー入力を**1 つもサーバーへ送信しない**ため、どのキーもローカルで処理できる。
/// `q` か Ctrl-C（raw モードなので 0x03 のバイトで届く）で抜ける。
async fn send_keys<W: tokio::io::AsyncWrite + Unpin>(
    wr: &mut W,
    read_only: bool,
    keys: Vec<u8>,
) -> Result<(), &'static str> {
    if read_only {
        if keys.iter().any(|&b| b == b'q' || b == 0x03) {
            return Err("left");
        }
    } else if berthd::write_frame(wr, &Frame::data(0, keys))
        .await
        .is_err()
    {
        return Err("closed");
    }
    Ok(())
}

/// 抜ける: 端末を戻し（題名・部屋・既知の状態へ倒す）、termios を戻し、別れの合図を置いて終わる。
/// `inner_alt`: 中のアプリが alt screen に居る状態で離脱するかどうか。
async fn leave(
    mut term: Term,
    raw: RawMode,
    scrollback: bool,
    inner_alt: bool,
    reason: &str,
    handle: &str,
) -> ! {
    // alt-screen 形式: **alt-screen から抜ける**（`\e[?1049l`）ことで、attach 前の外側の画面をそのまま復元する。この後に出す
    // `[detached from …]` は復元された画面の上に乗る（「部屋から出てきた」形）。
    // 続けて端末を既知の状態へ倒す（screen / tmux と同じ後始末）: bracketed paste off / カーソル可視 /
    // cursor keys 通常 / 行頭へ。中のシェルが点けた物（bash 4.4+ は `\e[?2004h` が既定）だけでなく、
    // ConPTY の前口上が端末へ素通しで届ける `?9001h`（win32-input-mode）と `?1004h`（フォーカス通知）も倒す。
    //
    // この列は「点けた物を戻す」ではなく「既知の状態へ倒す」（点いていない mode も消す）。
    // 注意: 副作用として、外側が元から有効にしていた場合も無効化する。それが問題になるなら、列全体を「前口上で検出したものだけ戻す」
    //    作りへ変える話になる。
    // 面では切らない: 未知の mode は端末が黙って読み飛ばすので、unix で出しても害が無い。
    //
    // 末尾に OSC 2746 の detach 信号を付与し、端末アプリが保持する attach 状態を解除する。
    // reason に依らず無条件に出す: "detached" / "exited"（transport 生存）はアプリに届いて状態が消え、
    // "disconnected"（transport 切断）は切断されたパイプに消えて届かないため、アプリは attach 状態を保持し
    // reattach できる。この非対称がちょうど欲しい挙動になる。
    //
    // 代替画面を使わなかったときは `?1049l` を出さない（出すと ConPTY がカーソルを home へ戻すため。
    // `DETACH_RESET_NO_ROOM` の doc）。中のアプリが alt に居るまま抜けるときだけ、先に出して端末を戻す。
    // 題名は空へ戻す（入口で設定した分）。注意: 元の題名は把握していないため「復元」ではなく「空にする」。
    let _ = term.stdout.write_all(TITLE_CLEAR).await;
    if scrollback {
        if inner_alt {
            let _ = term.stdout.write_all(b"\x1b[?1049l").await;
        }
        let _ = term.stdout.write_all(DETACH_RESET_NO_ROOM).await;
    } else {
        let _ = term.stdout.write_all(DETACH_RESET).await;
    }
    let _ = term.stdout.flush().await;
    drop(raw); // termios 復元（明示）— この後 process::exit すると Drop は走らないため
    let farewell = farewell_text(reason, handle);
    // スクロールへ流す形では、抜ける合図も右端に薄く置く（入るときと同じ形・`farewell_bytes`）。
    // 部屋の形は自分の行（部屋から出た外側の画面の上に乗る）。
    {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&farewell_bytes(scrollback, term_size().0, &farewell));
        let _ = out.flush();
    }
    // tokio の stdin リーダは spawn_blocking の中で `read(0)` に入りっぱなしになり、ランタイムを畳んでも
    // この OS スレッドが起きるまで**プロセスが exit しない**。すると外側シェルが子（この `berth attach`）を
    // reap できず、detach 後にプロンプトが Return を押すまで戻らない。端末は既に戻して
    // あるので、std stdout を flush して即 exit する（tmux/screen 系の detach も同様に即抜ける）。
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

pub(crate) async fn cmd_attach(args: &[String]) -> Result<(), String> {
    // 引数は**端末かどうかより先に**読む（打ち間違いは tty の外でも言える）。
    let AttachArgs {
        name,
        read_only,
        shell: cli_shell,
        scrollback: cli_scrollback,
    } = parse_attach_args(args)?;
    if !stdin_is_tty() {
        return Err(
            "berth attach: run it in an interactive terminal (it takes over the screen)."
                .to_string(),
        );
    }
    // 録画を**端末のスクロールへ流す**か。既定 ON・外し方は `scrollback_wanted` の doc。
    // 注意: 理由は使用しない（利用者へ表示するUIがまだ存在しないため）。テストでは理由で判別する。
    let (scrollback, _why) = scrollback_wanted(
        cli_scrollback,
        std::env::var(SCROLLBACK_ENV).ok().as_deref(),
        inside_multiplexer(),
        stdout_is_tty(),
    );
    // berthd が未起動のことがある。attach は「セッションを作って入る」操作なので、
    // ここで起動する（tmux 方式）。ls/info は起動しない（未起動時はセッション無しとして扱うのが正しい）。
    ensure_berthd().await?;

    let target = choose_target(name.as_deref(), read_only).await?;

    let stream = connect_daemon()
        .await
        .map_err(|e| format!("berth: connect berthd: {e}"))?;
    let (mut rd, mut wr) = tokio::io::split(stream);

    // 握手の頭で名乗る（`Hello`）。berthd は `client` を**接続に紐づけて**憶えるので、別の接続で
    // 名乗っても attach の記録には届かない（台帳の attach 行から `claimed_client` がキーごと抜ける）。
    // `serial: 0` は使えない（push が 0）ので、握手が 1、以降を 1 つずつ使う。
    let welcome = handshake(&mut rd, &mut wr).await?;
    // 重要: 古い berthd に `read_only` を送ると**操作側**として接続されるため、送信前にここで停止する。
    if read_only {
        read_only_supported(welcome.protocol)?;
    }

    // 注意: `mut` である理由: ウィンドウサイズが変化したら（下の `winch`）再描画と合図の位置はそのサイズで決める。
    let (mut cols, mut rows) = term_size();

    let fresh = target.fresh;
    let (session_id, handle) = if fresh {
        spawn_fresh(&mut rd, &mut wr, &target, cli_shell.as_deref(), cols, rows).await?
    } else {
        (target.session_id, target.handle)
    };

    // スクロールへ流す形のときだけ、このペインで前回どこまで見せたかを引く。
    // 一致すれば `seen_total` を送って**続きだけ**もらう（同じ履歴を積み増さない）。
    // ペインを名乗らない端末・別セッション・alt-screen 形式では `None`（全体を流す）。
    let pane = if scrollback { current_pane() } else { None };
    let seen_total = pane.as_ref().and_then(|p| read_pane_seen(p, &session_id));

    // `replay_len` = 先頭に来る録画（過去の出力）のバイト数。以後が live。
    // 部屋の形（CLI の対話 attach の元の形）では録画を**捨てる**: attach した瞬間、今見ていた画面へ過去の
    // 描画が流れ込むと「画面が飛んだ」ように見える。状態（cwd / env / 実行中のプロセス）はシェル自身が
    // 生きているので保たれ、attach 時の resize が SIGWINCH を起こしてシェルが今のプロンプトを描き直すので、
    // それだけ見えれば足りる。ペインの中身を復元したい GUI クライアントは録画を使うので、berthd は常に送る。
    // スクロールへ流す形では溜めて描き直す（下の loop）。
    //
    // `resized` = **この attach が pty の大きさを変えたか**（protocol 8）。下のバナーの出し方がこれで割れる。
    // 古い berthd は欄を送らず `false`。
    // `replay_head` = **録画の頭より前に捨てた出力が残した状態**（protocol 12）。スクロールへ流す形では
    // その状態になる命令を作り直して録画の頭に付けてからエミュレータに通す（`head_prefix`）。古い berthd は欄を
    // 送らず `None`（先頭に前提状態はないものとして描画する）。
    // `replay_total` = **セッション開始からの累計バイト**（protocol 13）。抜けるとき、ここに流した live の
    // バイトを足した位置をペインに覚え、次に同じペインで入り直したとき続きだけもらう。
    // 古い berthd はフィールドを送らず `None`（dedupe を無効化し、ペインに記録しない）。
    let Attached {
        replay_len: mut replay_remaining,
        resized: resized_by_attach,
        replay_head,
        replay_total,
    } = send_attach(
        &mut rd,
        &mut wr,
        &session_id,
        cols,
        rows,
        read_only,
        seen_total,
    )
    .await?;
    // seen の勘定。基準は `replay_total`（daemon が出した累計）、そこへこの接続で流した
    // **raw の live バイト**を足す。`replay_for_seen` は録画フレーム分を差し引くための控除値であり、fresh で
    // `replay_remaining` を 0 にしてもここは別に数えるので、録画の前口上を live と二重に数えない。
    // `None`（古い daemon）の場合は dedupe を無効化し、ペインに記録しない。
    let mut seen_pos: Option<u64> = replay_total;
    let mut replay_for_seen: usize = replay_remaining;
    // スクロールへ流す形で新しく作ったセッションの出力は**全部 live として流す**（録画に居るのは pty の
    // 前口上だけで、画面を描き直す物ではない）。合図は最初のプロンプトの隣に置く（下の `fresh_watch`）。
    if scrollback && fresh && !read_only {
        replay_remaining = 0;
    }

    // ここから raw モード（Drop で復元）。以後 socket には replay data + live が流れる。
    let raw = RawMode::enter()?;

    // 部屋の形: **alt-screen へ入る**（`\e[?1049h`）→ クリア → home → `[attached to <name>]`
    // （`[detached from <name>]` と対）。attach は「代替画面」に入り、detach で外側の画面をそのまま復元する
    // （tmux / vim の作法）。中のセッション出力は [`AltScreenShim`] を通して、vim 等の alt-screen 切替が
    // こちらの部屋ごと抜けて外側の画面に落ちるのを防ぐ（落とすのではなく画面クリアへ翻訳する）。
    // 順序が肝心: `\e[2J`（消去）でカーソルを動かす端末があるので、home（`\e[H`）を消去の**後**に置いて
    // marker を必ず最上段へ固定する（逆の順だと、そういう端末では marker の上に隙間ができる）。
    // marker 後の改行は fresh / reattach で出し方が違う（下記）ので付けない。
    //
    // 計器: `BERTH_ATTACH_DUMP=<path>` が在れば、端末へ書いたバイトをそのまま追記する
    // （外側の ConPTY は描き直して届けるので、生のバイトはここでしか見えない）。
    let mut term = Term {
        stdout: tokio::io::stdout(),
        dump: AttachDump::from_env(),
    };
    // 末尾に OSC 2746 の attach 信号を付ける。対応する端末アプリが「この端末は <handle> に attach 中」を
    // 記録し、ssh が切れたときに reattach を出し分ける。素の端末には無害（未知の OSC は ST まで読み飛ばされ、
    // 表示されない）。
    // **スクロールへ流すときは代替画面（alt screen）を作らない**。履歴も画面も端末の通常バッファに書き込み、
    // 抜けたあとも残す（端末本来のスクロールで辿れる）。部屋が無いので、中のアプリの alt 切替は
    // 本物の端末に任せる（下の `AltScreenShim` が素通し）。
    let room = if scrollback {
        ""
    } else {
        "\x1b[?1049h\x1b[2J\x1b[H"
    };
    let banner = if read_only {
        format!("[watching {handle} — press q to leave]")
    } else {
        format!("[attached to {handle}]")
    };
    let signal = if read_only {
        String::new()
    } else {
        format!("\x1b]2746;attach;{handle}\x1b\\")
    };
    // スクロールへ流す再接続では、**見える案内は描き直した画面の後**に置く（`replay_entrance`）。
    // 復元した画面が端末を埋めるので、先に出した行は 1 スクロール上へ消えて「繋がったか分からない」。
    // 信号（OSC 2746・見えない）だけはここで出す。
    let deferred_banner = scrollback && !fresh;
    // スクロールへ流す形で**新しく作った**ときも、合図は自分の行を取らず、入り直したときと同じ置き方に
    // する。窓を上端から使い（`take_the_window`）、新しいシェルが**最初のプロンプトを描き終わってから**、
    // カーソルの居る行の右端に薄く置く。描き終わりは下の loop が見る（`fresh_watch`）。ここでは窓を取るだけ。
    let beside_command = scrollback && fresh && !read_only;
    // 繋がっている間は**端末の題名**でも名乗る。復元した画面が埋まっていると画面の中に案内を置けない
    // （ConPTY は絶対座標で描き直す・最下行を空けるにはスクロール領域が要り、それはスクロールバックを
    // 殺す）。題名なら画面を触らない。抜けるときは空にする（Windows Terminal は空ならプロファイル名へ戻る）。
    // 注意: 中のシェルが自分で題名を変更すれば上書きされる（live は素通し）。
    let mine = title_of(&handle, read_only);
    let entrance = if deferred_banner {
        format!("{signal}{}", title_sequence(&mine, None))
    } else if beside_command {
        let top = String::from_utf8(take_the_window(rows)).expect("built from str");
        format!("{top}{signal}{}", title_sequence(&mine, None))
    } else {
        format!("{room}{banner}{signal}{}", title_sequence(&mine, None))
    };
    term.put(entrance.as_bytes()).await;
    // バナーを自分の行で終わらせる（改行する）のは、この後プロンプトが自分で出てくる場合:
    //   ① fresh spawn: 新しいシェルが marker の下にプロンプトを出す。
    //   ② この attach がサイズを変えた: SIGWINCH で readline が**カーソルの居る行を `\r` から
    //      描き直す**ので、そこにバナーが在ると消える。
    //   ③ サイズが同じ再接続: 下で一度ずらして戻し、② と同じ処理に乗せる（`nudge`）。
    // ∴ 部屋の形ではいつも改行で終える（見る側も）。
    // スクロールへ流す形式では ③ を行わない。画面は `render` が再描画済みであり、シェルに
    // 描き直させる理由が無い。ずらすと ConPTY が画面を描き直し、置いた案内が消える。
    let nudge = !fresh && !read_only && !resized_by_attach && !scrollback;
    if (read_only || fresh || resized_by_attach || nudge) && !deferred_banner && !beside_command {
        let _ = term.stdout.write_all(b"\r\n").await;
    }
    // スクロールへ流す形で**新しく作った**ときは、中のシェルを窓の上端から始める（`take_the_window`
    // の doc）。中の pty が最初に出す問い（`ESC[6n`）は berthd が「1 行目」と答えるので、窓の上端が
    // その 1 行目になるように、live を流し始める前に書く。合図を置く形（`beside_command`）は入口で済ませている。
    if scrollback && fresh && !beside_command {
        let top = take_the_window(rows);
        term.put(&top).await;
    }
    // 続きが 0 バイトの入り直し: 録画を描き終える瞬間が来ないので、合図はここで置く（`signal_on_empty_row`）。
    if deferred_banner && replay_remaining == 0 {
        let bytes = signal_on_empty_row(cols, &banner);
        term.put(&bytes).await;
    }
    let _ = term.stdout.flush().await;
    // 中から来る題名は `berth: <名前> · <中の題名>` に書き換えて通す（両方の形で）。
    let mut alt_shim = if scrollback {
        AltScreenShim::passing_through()
    } else {
        AltScreenShim::translating()
    }
    .renaming_titles(mine.clone());
    // スクロールへ流す形では、録画は読み飛ばさず**溜めて**、揃った瞬間に 1 回だけ描き直す
    // （`berth_screen::render`）。頭には、捨てられた分が残した状態を作り直した命令を先に置く（`replay_head`）。
    let mut replay_buf: Vec<u8> = replay_head
        .as_ref()
        .map(|h| berthd::replay_head::head_prefix(h, rows))
        .unwrap_or_default();

    // **同じ大きさの再接続は、一度 1 行ずらして戻す**（SIGWINCH を 2 回起こす）。
    // reattach したシェルは画面が消されたことを知らない。同じ大きさの SIGWINCH はカーネルが送らず
    // （大きさが変わったときだけ）、readline も描き直さないので、合図を作るには**本当に変える**しかない。
    // サイズが異なる再接続（②）は元から正しい。合図を受けた行エディタは自分で再描画し、位置を再計算する。
    // ここはその道に同じ大きさのときも乗せるだけ。
    // 改行を 1 つ送って空コマンドを走らせる手は採らない: 覚えている行へ絶対位置で描き直す行エディタ
    // （reedline）では、消した画面の最下段にプロンプトが出て、改行のスクロールでバナーが押し出される。
    // 大きさの変更は、前面が全画面アプリでも安全（外部 detach や接続断で vim の中から戻っても、
    // vim は SIGWINCH で描き直すだけで、行は刺さらない）。
    // 注意: 途中のサイズ（1 行少ない状態）で 1 回描画される場合があるが、最終サイズで再描画されるため残らない。
    // 注意: 閲覧モードでは送信しない（サイズ変更に関与しない）。
    // 重要: **戻す側のリサイズは、シェルがずらしたサイズで再描画したのを確認してから送信する**（下の loop）。
    // 2 回を続けて送ると、シェルが合図を受けた時にはもう元の大きさで、readline は「覚えている大きさと
    // 同じ」と見て描き直さない（プロンプトが出ない）。
    // 待ちは「次のセッション出力」か [`NUDGE_RESTORE_AFTER`] の早い方（黙るシェルでも必ず戻す）。
    let mut restore: Option<(u16, u16)> = None;
    let restore_by = tokio::time::Instant::now() + NUDGE_RESTORE_AFTER;
    // 新しく作ったときの合図は、新しいシェルが**最初のプロンプトを描き終わってから**、入り直しと
    // 同じ置き方（カーソル行の右端）で置く。描き終わりは誰も教えてくれないので、出力が [`FRESH_QUIET`]
    // 黙り、カーソルが文字の在る行の途中に居る（プロンプトの形）瞬間とする。出力は窓の上端（1 行目）から
    // 始まるので、同じバイトをエミュレータに通せば端末のカーソルの場所が分かる。[`FRESH_GIVE_UP`] を
    // 過ぎても形にならなければ諦める（題名だけが名乗る）。
    let mut fresh_watch: Option<Vec<u8>> = beside_command.then(Vec::new);
    let fresh_give_up = tokio::time::Instant::now() + FRESH_GIVE_UP;
    let mut fresh_quiet_at = fresh_give_up;
    if nudge {
        let (cols, rows) = term_size();
        let ajar = if rows > 1 { rows - 1 } else { rows + 1 };
        send_resize(&mut wr, &session_id, cols, ajar).await;
        restore = Some((cols, rows));
    }

    // socket 読みと stdin 読みは **別タスク→channel**（select! のキャンセルで frame/byte を壊さない）。
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Frame>(256);
    tokio::spawn(async move {
        while let Ok(f) = read_frame(&mut rd).await {
            if frame_tx.send(f).await.is_err() {
                break;
            }
        }
    });
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut winch = WindowChange::new()?;
    let mut quit = Quit::new()?;
    // 端末に行を訊いている間（`DsrWait`）は、打鍵を溜めて答えを抜き取り、残りを後で流す。
    let mut dsr: Option<DsrWait> = None;
    let mut dsr_deadline = tokio::time::Instant::now();
    // 諦めた DSR の答えが遅れて来るかもしれない間（`DSR_LATE`）。
    let mut dsr_late_until: Option<tokio::time::Instant> = None;
    let reason = loop {
        tokio::select! {
            data = in_rx.recv() => match data {
                None => break "eof",
                Some(bytes) => {
                    let bytes = if let Some(d) = dsr.as_mut() {
                        d.keys.extend_from_slice(&bytes);
                        match take_dsr_reply(&mut d.keys) {
                            Some(row) => {
                                let d = dsr.take().expect("checked just above");
                                let mut out = screen_bytes(&d.r, cols, rows, &banner, resized_by_attach, Some(row), &mine);
                                out.extend_from_slice(&d.live);
                                if term.show(&out).await.is_err() { break "output_closed"; }
                                d.keys
                            }
                            None => continue,
                        }
                    } else if dsr_late_until.is_some_and(|t| tokio::time::Instant::now() < t) {
                        let mut bytes = bytes;
                        if take_dsr_reply(&mut bytes).is_some() {
                            dsr_late_until = None;
                        }
                        bytes
                    } else {
                        bytes
                    };
                    if bytes.is_empty() { continue; }
                    // 閲覧モードではキー入力を**1 つもサーバーへ送信しない**ため、どのキーもローカルで処理できる。
                    // `q` か Ctrl-C（raw モードなので 0x03 のバイトで届く）で抜ける。
                    if let Err(why) = send_keys(&mut wr, read_only, bytes).await { break why; }
                }
            },
            frame = frame_rx.recv() => match frame {
                None => break "closed",
                Some(f) if f.kind == KIND_DATA => {
                    // seen の勘定（raw・録画スナップショットの後の live バイトだけ数える）。
                    // 注意: `replay_remaining` とは別にカウントする。fresh で 0 にしても録画の前口上を live と二重に
                    // 数えないため。録画フレーム分（`replay_for_seen`）を差し引いた残りが live の raw バイト。
                    if let Some(p) = seen_pos.as_mut() {
                        let skip = replay_for_seen.min(f.payload.len());
                        replay_for_seen -= skip;
                        *p += (f.payload.len() - skip) as u64;
                    }
                    // 先頭 replay_len バイト（過去の録画）。以後が live。
                    // - 部屋の形: **読み飛ばす**（上の `replay_len` の説明）。
                    // - スクロールへ流す形: **溜めて**、揃ったら端末エミュレータに 1 回通し、
                    //   履歴 → 画面 → カーソルの順に書く（同じ行は 1 回だけ・alt screen の中なら画面だけ）。
                    let live = if replay_remaining > 0 {
                        let take = replay_remaining.min(f.payload.len());
                        replay_remaining -= take;
                        if scrollback {
                            replay_buf.extend_from_slice(&f.payload[..take]);
                            if replay_remaining == 0 {
                                term.dump.put_replay(&replay_buf);
                                let mut r = berth_screen::render(&replay_buf, cols, rows);
                                replay_buf = Vec::new();
                                // 先頭が欠けているときは最下行から開始した（`head_prefix`）ため、押し出された空行を除去する。
                                if replay_head.is_some() {
                                    berthd::replay_head::strip_leading_blank_rows(&mut r.scrollback);
                                }
                                // 画面の頭に空の行が在るなら、**履歴だけ先に書いて**端末に今の行を訊き
                                // （DSR）、答えを待ってから画面を書く（`replay_entrance` の `term_row`）。
                                // 普段の入り直し（頭に空の行が無い）は訊かずにそのまま書く。
                                if wants_term_row(&r, resized_by_attach) {
                                    let mut r = r;
                                    let mut bytes = std::mem::take(&mut r.scrollback);
                                    bytes.extend_from_slice(DSR_REQUEST);
                                    if term.show(&bytes).await.is_err() { break "output_closed"; }
                                    dsr_deadline = tokio::time::Instant::now() + DSR_WAIT;
                                    dsr = Some(DsrWait { r, keys: Vec::new(), live: Vec::new() });
                                } else {
                                    let bytes = screen_bytes(&r, cols, rows, &banner, resized_by_attach, None, &mine);
                                    if term.show(&bytes).await.is_err() { break "output_closed"; }
                                }
                            }
                        }
                        &f.payload[take..]
                    } else {
                        &f.payload[..]
                    };
                    // ずらしたサイズでの再描画が届いたため、元のサイズへ戻す。
                    if !live.is_empty() {
                        if let Some((c, r)) = restore.take() {
                            send_resize(&mut wr, &session_id, c, r).await;
                        }
                    }
                    if !live.is_empty() {
                        if let Some(buf) = fresh_watch.as_mut() {
                            buf.extend_from_slice(live);
                            if buf.len() > FRESH_WATCH_CAP {
                                fresh_watch = None;
                            } else {
                                fresh_quiet_at = (tokio::time::Instant::now() + FRESH_QUIET).min(fresh_give_up);
                            }
                        }
                    }
                    if !live.is_empty() {
                        // alt-screen 切替（vim 等）をクリアへ翻訳してから端末へ。
                        // フレーム境界で割れても shim が持ち越すので安全。
                        let mut filtered = Vec::with_capacity(live.len());
                        alt_shim.feed(live, &mut filtered);
                        if let Some(d) = dsr.as_mut() {
                            d.live.extend_from_slice(&filtered);
                        } else if !filtered.is_empty() && term.show(&filtered).await.is_err() {
                            break "output_closed";
                        }
                    }
                }
                Some(f) => match decode_control(&f.payload) {
                    Ok(Control::Exited { .. }) => break "exited",
                    Ok(Control::Detached { reason: why, .. }) => {
                        break if why.as_deref() == Some("too_slow") { "too_slow" } else { "detached" };
                    }
                    _ => {}
                },
            },
            () = tokio::time::sleep_until(dsr_deadline), if dsr.is_some() => {
                // 端末が応答しなかったため、画面 rows 行をそのまま出力し、バッファした入力を流す。
                let d = dsr.take().expect("checked by the precondition");
                dsr_late_until = Some(tokio::time::Instant::now() + DSR_LATE);
                let mut out = screen_bytes(&d.r, cols, rows, &banner, resized_by_attach, None, &mine);
                out.extend_from_slice(&d.live);
                if term.show(&out).await.is_err() { break "output_closed"; }
                if !d.keys.is_empty() {
                    if let Err(why) = send_keys(&mut wr, read_only, d.keys).await { break why; }
                }
            }
            () = tokio::time::sleep_until(restore_by), if restore.is_some() => {
                // シェルが黙っていても、ずらした大きさのままにはしない。
                if let Some((c, r)) = restore.take() {
                    send_resize(&mut wr, &session_id, c, r).await;
                }
            }
            () = tokio::time::sleep_until(fresh_quiet_at), if fresh_watch.is_some() => {
                // 最初の出力が黙った。プロンプトの形（文字の在る行の途中にカーソル）なら合図を隣に置く。
                let now = tokio::time::Instant::now();
                let r = berth_screen::render(fresh_watch.as_deref().unwrap_or_default(), cols, rows);
                let prompt_like = r.cursor.1 > 0 && r.cursor_row_len > 0 && !r.in_alt_screen;
                if prompt_like {
                    fresh_watch = None;
                    let bytes = signal_beside_cursor(&r, cols, &banner);
                    if !bytes.is_empty() && term.show(&bytes).await.is_err() {
                        break "output_closed";
                    }
                } else if now >= fresh_give_up {
                    fresh_watch = None;
                } else {
                    fresh_quiet_at = (now + FRESH_QUIET).min(fresh_give_up);
                }
            }
            // 外部から停止シグナルを受けたため、既定の動作で終了する前に下の `leave` で端末を復元する。
            why = quit.recv() => break why,
            (c, r) = winch.changed() => {
                // 録画を受け終わる前に窓が動いても、画面と合図は今の大きさの格子で描く。
                cols = c;
                rows = r;
                // 手元の端末が動いたら、戻す予定は要らない（今の大きさを送るので）。
                restore = None;
                // サイズが変わると監視していたバイトのグリッドが端末と合わなくなるため、合図は断念する。
                fresh_watch = None;
                // 見る側は大きさに口を出さない（berthd も捨てるが、送らない）。
                if !read_only {
                    send_resize(&mut wr, &session_id, c, r).await;
                }
            }
        }
    };

    // 同じペインで次に入り直したとき**続きだけ**流せるよう、見せた位置を覚える。
    // スクロールへ流す形式で、ペインが特定でき、かつ新しい daemon（`replay_total` を送る、すなわち `seen_pos` が `Some`）の
    // ときだけ。書けなくても黙って続ける（dedupe は最適化であって正しさではない）。
    let seen = seen_to_remember(seen_pos, replay_remaining, dsr.is_some());
    if let (true, Some(pane), Some(seen)) = (scrollback, pane.as_ref(), seen) {
        write_pane_seen(pane, &session_id, seen);
    }

    leave(term, raw, scrollback, alt_shim.depth > 0, reason, &handle).await
}
