use crate::conn::sink_is;
#[cfg(windows)]
use crate::launch_job;
use crate::output::{PtyInput, SessionEvent, SessionOutput};
use crate::session::{SharedMaster, SharedWriter, Table};
use crate::COALESCE_CAP;
use portable_pty::{Child, PtySize};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::mpsc;

/// 入力 1 件を PTY へ渡した結果。2 箇所から呼ばれるので関数に出してある。
/// 出力の背圧で待っている間も打鍵を捌く必要が在る（[`run_pty_io_loop`] の内側の `select!`）。
enum Step {
    Go,
    Stop,
}

fn apply_input(
    cmd: Option<PtyInput>,
    master: &SharedMaster,
    child: &mut Box<dyn Child + Send + Sync>,
) -> Step {
    match cmd {
        Some(PtyInput::Resize { cols, rows }) => {
            // ロックの中で ioctl 1 回（await は跨がない）。取り出された後（子が畳まれた後）は
            // 何もしない（「もう大きさを変える先が無い」を黙って受ける）。
            if let Ok(guard) = master.lock() {
                if let Some(m) = guard.as_ref() {
                    let _ = m.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            Step::Go
        }
        Some(PtyInput::Close) | None => {
            let _ = child.kill();
            Step::Stop
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_pty_io_loop(
    master: SharedMaster,
    writer: SharedWriter,
    mut child: Box<dyn Child + Send + Sync>,
    mut input_rx: mpsc::Receiver<PtyInput>,
    mut output_rx: mpsc::Receiver<Vec<u8>>,
    output: Arc<StdMutex<SessionOutput>>,
    table: Table,
    session_id: String,
    // この起動 1 本分の job（`isize` なのは `Send` のため）。
    #[cfg(windows)] job: Option<isize>,
    // PTY への書き口（writer スレッドへの送り口）。OSC 7501 の問い合わせと、Windows の
    // 起動直後の `ESC[6n` に答えるため。
    write_tx: mpsc::Sender<Vec<u8>>,
) {
    // OSC 7501 の問い合わせを抜いて答え、名乗りを拾う（`osc7501.rs`・berthd が答える唯一の問い合わせ）。
    let mut osc7501 = crate::osc7501::Osc7501::default();
    // 子の死を覗きに行く間隔。短くすると起床が増え、長くすると「終わったのに一覧に残る」窓が
    // 伸びる。unix は EOF ですぐ気づくので、これは Windows での遅れの天井。
    const REAP_POLL: std::time::Duration = std::time::Duration::from_millis(500);
    // 背圧で待っている間に、持ち主が替わっていないか覗く間隔。
    const SINK_RECHECK: std::time::Duration = std::time::Duration::from_millis(100);
    // ConPTY（`--inheritcursor`・portable-pty が固定）は起動時にまず `ESC[6n`（カーソル位置の
    // 問い合わせ）を出し、答えが来るまでシェルを止める。その 4 バイトがクライアントの attach より先に
    // 届くと録画に入り、attach 時の `sanitize_replay`（問い合わせは再生しない）が捨てるため、誰も答えず
    // シェルは二度と起動しない。
    // ∴ 最初の出力に在る `ESC[6n` は berthd が自分で答え、録画にも live にも流さない。
    // 答えは常に「1 行目 1 桁目」。セッションは端末より長生きするので、どの端末のカーソル行も
    // 意味を持たない（`berth` の側も開始行を 1 に揃えている）。
    // 注意: 見るのは最初の出力 1 回だけ。以後の `ESC[6n` はシェルの物（PSReadLine 等）で、端末が答えるべき物。
    #[cfg(windows)]
    let mut startup_dsr_pending = true;

    'io: loop {
        tokio::select! {
            cmd = input_rx.recv() => {
                if matches!(apply_input(cmd, &master, &mut child), Step::Stop) {
                    break 'io;
                }
            }
            // 重要: 子の死を「出力の EOF」に頼らない。
            //
            // 下の `output_rx.recv() => None` は「reader EOF = 子プロセス終了」と読むが、Windows では
            // その等号が成り立たない。`exit` を打つと子は確かに終わる（pid が消える）のに、
            // reader は EOF に到達せず、`'io` を抜けない。ConPTY のホストがパイプを持ったままなので、
            // 閉じる者が居ない。∴ `child.wait()` にも表からの削除にも辿り着かず、
            // session は `alive: true` のまま一覧に残り続ける。
            // （`drop(master)` が返らない件とは別。あちらは畳む側で、こちらは畳み始めることすら無い。）
            //
            // unix は EOF が来るので、この腕が先に鳴ることはほぼ無い。それでも面で切らない。
            // 「EOF が来ること」は前提にしてよい性質ではなく、ここはその前提を外す腕。
            // 代償は session 1 本あたり `REAP_POLL` ごとの起床 1 回。
            _ = tokio::time::sleep(REAP_POLL) => {
                // `wait()` は使わない（返らないと `'io` ごと止まる）。`try_wait` は覗くだけ。
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break 'io;
                }
            }
            out = output_rx.recv() => {
                match out {
                    Some(first) => {
                        // 束ねてから 1 回で押す。
                        //
                        // PTY は細かく刻んで返す（1 回の read が数十バイトのことがある）ので、読んだ塊をそのまま
                        // 1 通ずつ送ると、数万バイトの出力が数百通になり、sink（`mpsc::channel(256)`）を軽く超える。
                        //
                        // ここで待たないのが肝。取るのは既にキューに在る分だけ（`try_recv`）で、空なら即座に押す。
                        // ∴ 対話の打鍵に遅延は乗らない（1 打鍵は 1 通のまま）。
                        // タイマーで貯める設計にすると、そこが体感の遅さになる。
                        //
                        // 重要: 溢れても捨てない（背圧で待つ）。捨てる側へ戻さないこと。
                        // 「速いが失う」と「正しいが遅い」のうち後者を選んである。捨てると、数千行の出力の途中が
                        // 黙って消え、末尾とプロンプトは着くので完走に見える（失敗が失敗の顔をしない）。
                        // 待ちが刺さっても出口は在る: 暴走した 1 本は木ごと終わらせられる（Windows は起動 1 本ごとの
                        // job ＋ `TerminateJobObject`）。速さが要るなら、捨てる側ではなく束ねる側（`COALESCE_CAP`）を動かす。
                        //
                        // 待てるようにするために、追記と送信を分けてある。`SessionOutput::stage` は
                        // `StdMutex` の中で追記だけして送る物を返し、送るのはロックの外。
                        //
                        // 重要: 素直に await すると別の欠陥が出る。打鍵と出力は同じ `select!` に居るので、
                        // 送信を待つ間 `input_rx` が捌けなくなる（出力の洪水中に `Ctrl+C` も `Close` も効かない）。
                        // ∴ 待つ間も入力を捌く（下の内側の `select!`）。
                        // `Sender::reserve()` は cancel safe。落としても permit を食わない。
                        let mut batch = first;
                        while batch.len() < COALESCE_CAP {
                            match output_rx.try_recv() {
                                Ok(more) => batch.extend_from_slice(&more),
                                Err(_) => break, // 空 or 切断。切断は次の recv() が None で拾う
                            }
                        }
                        // 起動直後の `ESC[6n` は berthd が答えて消す（上の `startup_dsr_pending` の説明）。
                        #[cfg(windows)]
                        if startup_dsr_pending {
                            startup_dsr_pending = false;
                            const DSR_REQUEST: &[u8] = b"\x1b[6n";
                            const DSR_REPLY_TOP_LEFT: &[u8] = b"\x1b[1;1R";
                            if let Some(p) = batch.windows(DSR_REQUEST.len()).position(|w| w == DSR_REQUEST) {
                                batch.drain(p..p + DSR_REQUEST.len());
                                if write_tx.send(DSR_REPLY_TOP_LEFT.to_vec()).await.is_err() {
                                    log_line!("[berthd] session {session_id}: could not answer the startup DSR");
                                }
                                if batch.is_empty() {
                                    continue 'io;
                                }
                            }
                        }
                        // OSC 7501 の問い合わせは録画にも端末にも流さず、berthd が答える。
                        // 注意: 書き口は待たない（`try_send`）。子が入力を読まずに塞がっていると、待てば出力まで止まる。
                        let filtered = osc7501.filter(&batch);
                        for _ in 0..filtered.probes {
                            if write_tx.try_send(crate::osc7501::REPLY.to_vec()).is_err() {
                                log_line!("[berthd] session {session_id}: could not answer the OSC 7501 probe");
                            }
                        }
                        let batch = filtered.out;
                        // 名乗りは流すバイトの中から拾うので、空なら名乗りも無い。
                        if batch.is_empty() {
                            continue 'io;
                        }
                        // replay_buf への追記はロックの中、送るのはロックの外。
                        let staged = output.lock().ok().map(|mut o| {
                            for r in filtered.reports {
                                o.record_program_status(r);
                            }
                            let op = o.stage(&batch);
                            let viewers: Vec<_> = o.viewers.iter().map(|v| v.tx.clone()).collect();
                            (op, viewers)
                        });
                        let (op, viewers) = staged.unwrap_or((None, Vec::new()));
                        // 見る側へは待たずに渡す（遅い見る側は外す）。操作する側（下）は背圧で待つ。
                        // 注意: 見る側まで待つと、遅い回線の見る側 1 本が操作する側の画面を止める。
                        // 溢れた見る側は外すので、黙って欠けた画面は見せない（繋ぎ直せば今の画面から）。
                        // 見る側を先に回すのは、操作する側の待ちに見る側を巻き込まないため。
                        if !viewers.is_empty() {
                            let mut lagging: Vec<mpsc::Sender<SessionEvent>> = Vec::new();
                            for v in &viewers {
                                if let Err(mpsc::error::TrySendError::Full(_)) =
                                    v.try_send(SessionEvent::Output(batch.clone()))
                                {
                                    lagging.push(v.clone());
                                }
                            }
                            if !lagging.is_empty() {
                                if let Ok(mut o) = output.lock() {
                                    o.kick_viewers(
                                        |tx| lagging.iter().any(|l| l.same_channel(tx)),
                                        "too_slow",
                                    );
                                }
                            }
                        }
                        if let Some((sink, pending)) = op {
                            let lost = batch.len();
                            let mut queue = Vec::with_capacity(2);
                            if let Some(ev) = pending { queue.push(ev); }
                            queue.push(SessionEvent::Output(batch));
                            'deliver: for ev in queue {
                                let mut slot = Some(ev);
                                loop {
                                    tokio::select! {
                                        permit = sink.reserve() => {
                                            match permit {
                                                Ok(p) => { if let Some(e) = slot.take() { p.send(e); } }
                                                // client が送信中に消えた。ここだけが「落ちる」口。
                                                Err(_) => {
                                                    if let Ok(mut o) = output.lock() { o.record_drop(lost); }
                                                }
                                            }
                                            break;
                                        }
                                        cmd = input_rx.recv() => {
                                            if matches!(
                                                apply_input(cmd, &master, &mut child),
                                                Step::Stop
                                            ) { break 'io; }
                                        }
                                        // 待っている間に持ち主が替わった（置き換え・外部 detach）なら、前の口は諦める。
                                        // 前の持ち主が読むのをやめていると、ここで永久に待って新しい持ち主へ何も流れない。
                                        // この分は `stage` で録画に積んであるので、新しい attach の録画に入っている。
                                        _ = tokio::time::sleep(SINK_RECHECK) => {
                                            if !output.lock().is_ok_and(|o| sink_is(&o, &sink)) {
                                                break 'deliver;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => break, // reader EOF = 子プロセス終了
                }
            }
        }
    }
    // 子を reap して status を確定し、attach 中の client へ Exited を通知。
    let status = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);

    // 重要: PTY は明示的に、順序を決めて落とす。スコープ末尾の暗黙 drop に任せない。
    // 暗黙だと `table.lock().await` の後ろに来るし、順序も読む人に見えない。
    //
    // Windows では、これが CPU を 1 コア返すかどうかを決める。ConPTY は別プロセスのホスト
    // （`OpenConsole.exe`）を立てるので、master を落とさないと擬似コンソールが閉じず、
    // ホストが 100% で回り続ける。berthd も検体もそれを報告しない。
    // unix では無害（プロセス終了で fd が閉じる）であり、unix では再現しない欠陥。
    //
    // 順序は 書き口 → job の terminate → master（理由は下）。
    // 注意: 書き込みスレッドが書いている最中（子が読まずに塞がっている）なら書き口は取れない。
    // 待たずに先へ進み、そのスレッドが書けなくなった時点で自分で落とす。
    if let Ok(mut w) = writer.try_lock() {
        drop(w.take());
    }

    // 木ごと止める。`child.kill()` は直接の子しか殺さない。
    //
    // 重要: この 1 本は 2 つの行に挟まれていて、どちらへ動かしても壊れる。
    //
    // `child.wait()` より後でなければならない。先に木を消すと、上の `status` が
    //    子の本当の終了状態ではなく、こちらが撃った 1 に化けて、台帳が嘘をつく。
    //
    // `drop(master)` より前でなければならない。後ろに置くと、client が読むのをやめている間に
    //    session を閉じたとき、片付けごと永久に固まる。機構は循環待ち:
    //      ① client が読まない → sink が埋まり、pump は `sink.reserve()` で待つ
    //      ② `Close` が届き pump は `'io` を抜ける → 誰も `output_rx` を捌かなくなる
    //      ③ reader スレッドは満杯の `output_tx` に押せず止まる
    //      ④ `drop(master)`（`ClosePseudoConsole`）は ConPTY の出力が捌けるまで返らない
    //         → ③ が止まっているので永久に返らない
    //    ∴ 木を先に消せば ConPTY が畳めるようになり、④ が返る。
    //    後ろに在ると、木ごと終わらせる手（`terminate`）に到達しない。
    //
    // この 1 本を外すと `tests/a_session_takes_its_tree_with_it.rs` が「孫が生き残った」で赤くなる。
    //    順序の方は `tests/a_flooded_session_can_still_be_killed.rs` が押さえる。
    //    `drop(master)` の後ろに戻すと、その検体が赤くなる。
    #[cfg(windows)]
    if let Some(j) = job {
        launch_job::terminate(j);
    }

    // master は session 表とも共有している（`List` が大きさを訊く）。
    // 重要: ここで `take()` して、ここで落とすこと。参照を持ったまま表の掃除に落とさせると、
    // 上で決めた順序が別の関数へ移る。取り出した後の `List` は `None` を見て
    // 記録値へ落ちる（session はこの数行後に表から消えるので、見えるのは一瞬）。
    let master = master.lock().ok().and_then(|mut g| g.take());
    drop(master);

    log_line!("[berthd] session {session_id}: pty closed (status {status})");
    notify_exit(&output, status).await;
    // 注意: 自分の session のときだけ消す。id だけで消すと、同じ id で後から立った別の session を消しうる。
    let mut tbl = table.lock().await;
    if tbl
        .get(&session_id)
        .is_some_and(|s| Arc::ptr_eq(&s.output, &output))
    {
        tbl.remove(&session_id);
    }
}

/// attach している client へ終わり（`Exited` と終了状態）を知らせる。
///
/// 注意: 操作する側へは待って送る（有界）。大量に出して終わる子では、最後の出力で口がちょうど
/// 満杯になる。待たずに送ると `Exited` が落ち、client は終わり方も状態も知らずに切られる。
/// 見る側は待たない（遅い見る側は外す）。
pub(crate) async fn notify_exit(output: &Arc<StdMutex<SessionOutput>>, status: i32) {
    const EXIT_NOTICE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    let sink = output.lock().ok().and_then(|o| {
        // 見る側にも終わりを知らせる。
        for v in &o.viewers {
            let _ = v.tx.try_send(SessionEvent::Exited(status));
        }
        o.sink.clone()
    });
    if let Some(s) = sink {
        let _ = tokio::time::timeout(EXIT_NOTICE_WAIT, s.send(SessionEvent::Exited(status))).await;
    }
}

/// 全 session に `Close` を送り、pump が畳み終えるのを有界で待つ。
///
/// なぜ待つのか: `Close` は非同期で、pump が `child.kill()` → `child.wait()` →
/// master の drop まで進んで初めて ConPTY が閉じる。
/// 送っただけで `exit` すると、送る前と何も変わらない（デストラクタは走らない）。
///
/// 有界にする理由: 畳めない子（`kill` が届かない / `wait` が返らない）に付き合うと、
/// 利用者は「畳めない」ではなく「固まった」を見る。
/// ∴ 待ちきれなかったら数を名乗って先へ進む（黙って諦めない）。
pub(crate) async fn shutdown_sessions(table: &Table) {
    let ids: Vec<String> = {
        let tbl = table.lock().await;
        tbl.keys().cloned().collect()
    };
    if ids.is_empty() {
        return;
    }
    log_line!("[berthd] kill-server: closing {} session(s)", ids.len());
    {
        let tbl = table.lock().await;
        for id in &ids {
            if let Some(s) = tbl.get(id) {
                // 満杯でも待たない（`send` だと畳みが入力の詰まりに引きずられる）。
                let _ = s.input_tx.try_send(PtyInput::Close);
            }
        }
    }
    // 2 秒。シェルが SIGKILL されて reap されるには十分で、人が待てる長さ。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let left = table.lock().await.len();
        if left == 0 {
            log_line!("[berthd] kill-server: all sessions closed");
            return;
        }
        if std::time::Instant::now() >= deadline {
            // 名乗る。Windows ではこの残りが `OpenConsole.exe` の孤児になる。
            // 「何本残したか」を言えないと、後から誰も数え直せない。
            log_line!(
                "[berthd] kill-server: {left} session(s) did not close in time \
                 — their pty hosts may outlive this process"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
