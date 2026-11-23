use crate::attach::attach_session;
use crate::output::{PtyInput, SessionEvent, SessionOutput};
use crate::pty_loop::shutdown_sessions;
use crate::session::{hostname, live_cwd_with_src, live_size, validate_name, Session, Table};
use crate::spawn::{spawn_session, ShellNotStarted, Spawned};
use crate::ConnWriter;
use berthd::{
    control_frame, decode_control, write_frame, Control, Frame, SessionInfo, KIND_CONTROL,
    KIND_DATA,
};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex};

/// socket writer へフレームを送る（control 応答と output forward が共有）。
pub(crate) async fn send(wr: &Arc<TokioMutex<ConnWriter>>, f: &Frame) -> std::io::Result<()> {
    let mut guard = wr.lock().await;
    write_frame(&mut *guard, f).await
}

/// 1 client 接続を処理する。MVP: この接続が attach できる session は同時に 1 つ。
///
/// `attached_sink` は「この接続が張った sink」の控え。detach / 切断で sink を外すときは、
/// session の現在の sink が **自分のもの**であるときだけ外す（[`clear_own_sink`]）。
/// reattach（同じ session に新しい接続が attach）した直後に古い接続が閉じると、素朴に
/// clear すると新しい client の live 出力が止まるため。
pub(crate) async fn handle_conn(stream: berthd::DaemonConn, table: Table) {
    // 接続の相手をカーネルに訊く（監査台帳に載せる観測）。client の自称でなく peer credential
    // なので、名乗りを信じる必要がない。注意: split する前に取ること（分割後は生の fd / ハンドルを
    // 持つ側が片方だけになる）。
    // 取れる物は面で違うので、`berthd::Peer` の欄は `Option`。Windows は pid を訊ける
    // （`GetNamedPipeClientProcessId`）が、uid は無い（取れないのではなく存在しない）。代わりに
    // session id が載る（`0` = サービス（ssh 経由） / `1` 以上 = 対話ログイン）。
    // どちらの面でも `src` に「どう知ったか / なぜ知れなかったか」を必ず残す。
    #[cfg(unix)]
    let peer = {
        use std::os::fd::AsFd;
        // 監査セッション id も引く。シェルの側が同じ番号を自分の記録に書いていれば
        // （hsh の `session:start` の `os_sessionid`）、「ssh で入った誰かがこのセッションに
        // attach した」を台帳だけで言える。
        // 同じ番号だが、面によって別の物に付いている: Linux はプロセス
        // （`/proc/<pid>/sessionid`）、macOS は接続（`LOCAL_PEERTOKEN`）。
        // ∴ 口そのものを渡す（pid だけでは macOS を引けない）。これも split する前でなければ
        // ならない理由（生 fd が要る）。
        let cred = stream.peer_cred().ok();
        berthd::peer_from_cred(
            cred.as_ref().map(tokio::net::unix::UCred::uid),
            cred.as_ref()
                .and_then(tokio::net::unix::UCred::pid)
                .map(i64::from),
            Some(stream.as_fd()),
        )
    };
    #[cfg(windows)]
    let peer = {
        use std::os::windows::io::AsRawHandle;
        berthd::peer_from_pipe(stream.as_raw_handle())
    };

    // `tokio::io::split`（汎用）で分ける。上位が要るのは `AsyncRead` / `AsyncWrite` だけなので、
    // unix socket でも名前付きパイプでも同じ形になる。
    let (mut rd, wr) = tokio::io::split(stream);
    let mut conn = Conn {
        table,
        wr: Arc::new(TokioMutex::new(wr)),
        peer,
        attached: None,
        attached_sink: None,
        client: None,
    };

    loop {
        let frame = match berthd::read_frame(&mut rd).await {
            Ok(f) => f,
            Err(_) => break, // client 切断
        };

        // データフレーム = attach 中 session への入力。
        if frame.kind == KIND_DATA {
            conn.on_data(frame.payload).await;
            continue;
        }
        if frame.kind != KIND_CONTROL {
            continue;
        }

        let ctrl = match decode_control(&frame.payload) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let serial = frame.serial;

        match ctrl {
            Control::Hello { client: name } => conn.on_hello(serial, name).await,
            // heartbeat: 即 Pong を返す。attach 中でも forward task と writer を共有するので、
            // 出力ストリームに割り込んで応答できる（この read ループと出力書き込みは別タスク）。
            Control::Ping => conn.reply(serial, &Control::Pong).await,
            Control::List => conn.on_list(serial).await,
            Control::Spawn {
                session_id,
                name,
                shell,
                shell_src,
                cwd,
                env,
                label,
                cols,
                rows,
            } => {
                let result = spawn_session(
                    &conn.table,
                    session_id,
                    name,
                    shell,
                    shell_src,
                    cwd,
                    env,
                    label,
                    cols,
                    rows,
                )
                .await;
                conn.on_spawned(serial, result).await;
            }
            Control::Attach {
                session_id,
                cols,
                rows,
                actor,
                actor_src,
                read_only,
                seen_total,
            } => {
                attach_session(
                    &conn.table,
                    &conn.wr,
                    &mut conn.attached,
                    &mut conn.attached_sink,
                    session_id,
                    serial,
                    cols,
                    rows,
                    read_only,
                    seen_total,
                    conn.peer,
                    conn.client.clone(),
                    // 自称はこの attach 要求が運んできた値をそのまま持つ（接続に紐づけて
                    // 憶えない）。同じ接続で attach し直せば名乗りも取り直されるため、身元が
                    // attach スコープであることと、保持の仕方が一致する。
                    berthd::Claim {
                        actor,
                        src: actor_src,
                    },
                )
                .await;
            }
            Control::Resize {
                session_id,
                cols,
                rows,
            } => conn.on_resize(&session_id, cols, rows).await,
            Control::Detach { session_id } => conn.on_detach(&session_id).await,
            Control::Close { session_id } => conn.on_close(&session_id).await,
            Control::Rename { session_id, name } => {
                conn.on_rename(serial, session_id, name).await;
            }
            Control::KillServer => conn.kill_server().await,
            Control::SendKeys {
                session_id,
                data,
                force,
            } => conn.on_send_keys(serial, session_id, data, force).await,
            Control::Peek {
                session_id,
                max_bytes,
            } => conn.on_peek(serial, session_id, max_bytes).await,
            // server→client メッセージが来たら無視。
            _ => {}
        }
    }

    // 切断時: attach 中 session を detach（sink を外す）。子は殺さず、detached で生存させる。
    // 自分が張った sink のときだけ外す（別 client が既に reattach 済みならその sink を尊重）。
    if let Some(sid) = conn.attached {
        clear_own_sink(&conn.table, &sid, conn.attached_sink.as_ref()).await;
    }
}

/// 1 本の接続が持つ状態（[`handle_conn`] の 1 回ぶん）。
struct Conn {
    table: Table,
    wr: Arc<TokioMutex<ConnWriter>>,
    peer: berthd::Peer,
    attached: Option<String>,
    attached_sink: Option<mpsc::Sender<SessionEvent>>,
    /// 何が繋いできたか（`Hello` の自称）。注意: `peer` と違って接続に紐づけて憶える。
    /// 握手は接続の頭で 1 回だけなので、attach ごとに取り直せない（`Claim` とはスコープが違う）。
    ///
    /// 台帳の `peer_uid` / `peer_pid` は「誰が」（カーネルの観測）で、こちらは「何が」。
    /// Windows では uid が無いので、`claimed_client` が身元の手掛かりとして重くなる。
    client: Option<String>,
}

impl Conn {
    async fn reply(&self, serial: u32, ctrl: &Control) {
        let _ = send(&self.wr, &control_frame(berthd::SOURCE, serial, ctrl)).await;
    }

    /// この接続が `sid` の今の持ち主のときだけ、session から `pick` で 1 つ取り出す。
    ///
    /// 注意: 送り口だけ取って、表のロックを外してから待つこと。子が読まずに詰まると
    /// 送りは返らないので、握ったままだと他の client の操作まで止まる。
    async fn if_owner<T>(&self, sid: &str, pick: impl FnOnce(&Session) -> T) -> Option<T> {
        self.table
            .lock()
            .await
            .get(sid)
            .filter(|sess| holds(sess, self.attached_sink.as_ref()))
            .map(pick)
    }

    async fn on_data(&self, payload: Vec<u8>) {
        // 打てるのは今の持ち主だけ。置き換えられた接続の打鍵は捨てる。
        // PTY は 1 本のバイト列なので、2 台の打鍵は同じ行に混ざったまま Enter で実行される。
        let Some(sid) = &self.attached else { return };
        if let Some(tx) = self.if_owner(sid, |sess| sess.write_tx.clone()).await {
            let _ = tx.send(payload).await;
        }
    }

    async fn on_hello(&mut self, serial: u32, name: String) {
        // 自称なので裏は取れない（`claimed_*` の棚）。空は名乗らなかったのと同じ。
        if !name.is_empty() {
            self.client = Some(name);
        }
        self.reply(
            serial,
            &Control::Welcome {
                // 版だけでは配備物を同定できない（`berthd::VERSION_STRING` 参照）。build まで名乗る。
                server: format!("berthd/{}", berthd::VERSION_STRING),
                protocol: berthd::PROTOCOL_VERSION,
                host: hostname(),
            },
        )
        .await;
    }

    async fn on_list(&self, serial: u32) {
        // cwd は spawn 時の記録でなく、子シェルの今の cwd を引き直す（追えなければ記録値）。
        // 落ちたときは `cwd_src` がそう名乗る（黙って記録値を返さない）。
        let sessions: Vec<SessionInfo> = self
            .table
            .lock()
            .await
            .values()
            .map(|s| {
                let mut info = s.info.clone();
                match s.pid {
                    // pid が無いのは面の問題ではないので、ここで名乗る。
                    None => info.cwd_src = "spawn:unavailable:pid".into(),
                    Some(pid) => {
                        let (cwd, src) = live_cwd_with_src(pid);
                        if let Some(cwd) = cwd {
                            info.cwd = cwd;
                        }
                        info.cwd_src = src.into();
                    }
                }
                // 大きさも spawn 時の記録でなく、pty の今を引き直す。
                if let Some((cols, rows)) = live_size(&s.master) {
                    info.cols = cols;
                    info.rows = rows;
                }
                info.program_status = s.output.lock().ok().and_then(|o| o.program_status());
                info
            })
            .collect();
        self.reply(serial, &Control::Sessions { sessions }).await;
    }

    async fn on_spawned(&self, serial: u32, result: std::io::Result<Spawned>) {
        let peer = self.peer;
        match result {
            Ok(spawned) => {
                // 起こした瞬間を台帳へ。一覧は「今」の計器で、セッションが消えれば何も残らない。
                // 接続の観測をここでも取る。attach の行と同じ棚に並べないと、
                // 「誰が起こしたか」を別の行から推理させることになる。
                journal(&berthd::spawn_record(
                    &spawned.id,
                    &spawned.name,
                    &spawned.shell,
                    &spawned.shell_src,
                    &spawned.cwd,
                    peer,
                    &berthd::peer_ancestry(peer.pid),
                    self.client.as_deref(),
                    &hostname(),
                ));
                self.reply(
                    serial,
                    &Control::Spawned {
                        session_id: spawned.id,
                        pid: spawned.pid,
                    },
                )
                .await;
            }
            Err(e) => {
                // 起こせなかったことも台帳へ。注意: 書くのはシェルの起動の失敗だけ。
                // id の重複や pty の確保の失敗はこの型を持たず、返事だけ。
                if let Some(f) = e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<ShellNotStarted>())
                {
                    journal(&berthd::spawn_failed_record(
                        &f.id,
                        f.name.as_deref(),
                        &f.shell,
                        &f.shell_src,
                        &f.cwd,
                        &f.reason,
                        peer,
                        &berthd::peer_ancestry(peer.pid),
                        self.client.as_deref(),
                        &hostname(),
                    ));
                }
                self.reply(
                    serial,
                    &Control::Error {
                        code: "spawn_failed".into(),
                        message: e.to_string(),
                    },
                )
                .await;
            }
        }
    }

    async fn on_resize(&self, session_id: &str, cols: u16, rows: u16) {
        // 大きさを決めるのも今の持ち主だけ。attach していない接続から任意の session の大きさを
        // 変えられると、別の端末で覗いただけで持ち主の画面が崩れる。
        if self.attached.as_deref() != Some(session_id) {
            return;
        }
        if let Some(tx) = self
            .if_owner(session_id, |sess| sess.input_tx.clone())
            .await
        {
            let _ = tx.send(PtyInput::Resize { cols, rows }).await;
        }
    }

    async fn on_detach(&mut self, session_id: &str) {
        if self.attached.as_deref() == Some(session_id) {
            // 自己 detach（GUI ペイン等がこの接続で attach 中）: 自分の sink を外すだけ。
            clear_own_sink(&self.table, session_id, self.attached_sink.as_ref()).await;
            self.attached = None;
            self.attached_sink = None;
        } else {
            // 外部 detach（別接続の `berth detach <name>`）: session に今 attach している client を蹴る。
            // 外す知らせ（oneshot）で伝える → 相手の forward task が `Detached` フレームを送って離脱する。
            // 出力の口では送らない。相手が読むのをやめていると満杯で、ここが返らなくなる。
            // 見る側も含め、全員を外す（tmux の `detach-client -s`）。
            let tbl = self.table.lock().await;
            if let Some(sess) = tbl.get(session_id) {
                if let Ok(mut o) = sess.output.lock() {
                    o.kick_viewers(|_| true, "detached");
                    o.kick_owner("detached");
                }
            }
        }
    }

    async fn on_close(&self, session_id: &str) {
        let tx = self
            .table
            .lock()
            .await
            .get(session_id)
            .map(|sess| sess.input_tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(PtyInput::Close).await;
        }
    }

    async fn on_rename(&self, serial: u32, session_id: String, name: String) {
        let mut tbl = self.table.lock().await;
        let taken: Vec<String> = tbl
            .iter()
            .filter(|(id, _)| **id != session_id) // 自分自身の現在名は「重複」ではない
            .map(|(_, s)| s.info.name.clone())
            .collect();
        let reply = match validate_name(&name, taken.into_iter()) {
            Err(message) => Control::Error {
                code: "bad_name".into(),
                message,
            },
            Ok(()) => match tbl.get_mut(&session_id) {
                None => Control::Error {
                    code: "no_such_session".into(),
                    message: format!("{session_id} not found"),
                },
                Some(sess) => {
                    let name = name.trim().to_string();
                    sess.info.name = name.clone();
                    Control::Renamed { session_id, name }
                }
            },
        };
        drop(tbl);
        self.reply(serial, &reply).await;
    }

    async fn on_send_keys(&self, serial: u32, session_id: String, data: Vec<u8>, force: bool) {
        let bytes = data.len();
        let (tx, name, outcome) = {
            let tbl = self.table.lock().await;
            let Some(sess) = tbl.get(&session_id) else {
                drop(tbl);
                self.reply(
                    serial,
                    &Control::Error {
                        code: "no_such_session".into(),
                        message: format!("{session_id} not found"),
                    },
                )
                .await;
                return;
            };
            let check = sess
                .output
                .lock()
                .map(|mut o| o.check_and_record_send(force))
                .unwrap_or(Ok(()));
            (sess.write_tx.clone(), sess.info.name.clone(), check)
        };
        if let Err(limit) = outcome {
            self.reply(
                serial,
                &Control::Error {
                    code: "send_loop_limit".into(),
                    message: format!(
                        "refusing send after {limit} consecutive sends without session output (pass --force to override)"
                    ),
                },
            )
            .await;
            return;
        }
        if tx.send(data).await.is_err() {
            self.reply(
                serial,
                &Control::Error {
                    code: "session_closed".into(),
                    message: format!("{session_id} is closing"),
                },
            )
            .await;
            return;
        }
        let ancestry = berthd::peer_ancestry(self.peer.pid);
        journal(&berthd::send_keys_record(
            &session_id,
            &name,
            self.peer,
            &ancestry,
            self.client.as_deref(),
            &hostname(),
            bytes,
        ));
        self.reply(serial, &Control::SendKeysAck { session_id, bytes })
            .await;
    }

    async fn on_peek(&self, serial: u32, session_id: String, max_bytes: Option<usize>) {
        let (bytes, replay_head, cols, rows, idle_ms, produced, attached, program_status) = {
            let tbl = self.table.lock().await;
            let Some(sess) = tbl.get(&session_id) else {
                drop(tbl);
                self.reply(
                    serial,
                    &Control::Error {
                        code: "no_such_session".into(),
                        message: format!("{session_id} not found"),
                    },
                )
                .await;
                return;
            };
            let (cols, rows) = live_size(&sess.master).unwrap_or((sess.info.cols, sess.info.rows));
            let ((bytes, replay_head, idle_ms, produced, attached), program_status) = sess
                .output
                .lock()
                .map(|o| (o.peek_tail(max_bytes), o.program_status()))
                .unwrap_or_else(|_| ((Vec::new(), None, 0, 0, false), None));
            (
                bytes,
                replay_head,
                cols,
                rows,
                idle_ms,
                produced,
                attached,
                program_status,
            )
        };
        let data_len = bytes.len();
        self.reply(
            serial,
            &Control::PeekResult {
                session_id,
                data_len,
                replay_head,
                cols,
                rows,
                idle_ms,
                produced,
                attached,
                program_status,
            },
        )
        .await;
        if data_len > 0 {
            let _ = send(&self.wr, &Frame::data(0, bytes)).await;
        }
    }

    async fn kill_server(&self) {
        // 全 session の子を kill → socket を消して自分も終わる。
        // 応答は返さない（返す前に落ちる可能性がある）。client は EOF で成功を知る。
        let txs: Vec<_> = self
            .table
            .lock()
            .await
            .values()
            .map(|sess| sess.input_tx.clone())
            .collect();
        for tx in txs {
            let _ = tx.send(PtyInput::Close).await;
        }
        // 注意: 後始末を先に、ログは最後。ログの書き込みは失敗しうる（親が死んだ後の stderr は
        // 壊れたパイプ）。`log_line!` は書けなくても死なないが、順序でも守ることでログが落ちても畳める。
        // socket を消すのは unix だけ。名前付きパイプはプロセスが消えれば名前ごと消えるので、
        // 消す物が無い（`bind_daemon` の doc と対）。
        #[cfg(unix)]
        std::fs::remove_file(berthd::socket_path()).ok();
        // 重要: `std::process::exit` はデストラクタを走らせない。ここでそのまま抜けると、各 session の
        // `master`（すなわち ConPTY）が一度も drop されず、子シェルも残る。Windows ではそれが
        // `OpenConsole.exe` を孤児にし、100% CPU で回り続ける。
        //
        // ∴ 畳んでから出る。待つのは有界（畳めない子に付き合わない）。
        shutdown_sessions(&self.table).await;
        log_line!("[berthd] kill-server: shutting down");
        std::process::exit(0);
    }
}

/// 台帳へ 1 レコード。書けなくても daemon は動き続けるが、黙って捨てない
/// （プロセスにつき 1 回だけ言う。コマンドごとに騒ぐのは邪魔で、一度も言わないのは事故）。
pub(crate) fn journal(record: &serde_json::Value) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if let Err(e) = berthd::journal_append(record) {
        if !WARNED.swap(true, Ordering::Relaxed) {
            log_line!(
                "berthd: journal: 書き込めません: {}: {e} (以降は抑制)",
                berthd::journal_path().display()
            );
        }
    }
}

/// 自分が張った sink のときだけ外す。session の現在の sink が別 client のもの（その間に
/// reattach された）なら何もしない。これが無いと、reattach 直後に古い接続が閉じたときに
/// 新しい client の live 出力が止まる（replay だけ出て以後無音になる）。
async fn clear_own_sink(
    table: &Table,
    session_id: &str,
    mine: Option<&mpsc::Sender<SessionEvent>>,
) {
    let Some(mine) = mine else { return };
    if let Some(sess) = table.lock().await.get(session_id) {
        if let Ok(mut o) = sess.output.lock() {
            // 見る側として張っていたなら、その 1 本も外す（自分で離れたため知らせは撃たない）。
            o.forget(mine);
        }
    }
}

/// この接続が張った sink が、いま session の出力口か（この接続が session の今の持ち主か）。
///
/// sink の同一性だけで決める。置き換えられた接続も `attached` に session id を持ったままなので、
/// id では決められない。
fn holds(sess: &Session, mine: Option<&mpsc::Sender<SessionEvent>>) -> bool {
    let Some(mine) = mine else { return false };
    sess.output.lock().is_ok_and(|o| sink_is(&o, mine))
}

pub(crate) fn sink_is(o: &SessionOutput, mine: &mpsc::Sender<SessionEvent>) -> bool {
    o.sink.as_ref().is_some_and(|cur| cur.same_channel(mine))
}
