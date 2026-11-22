use crate::conn::{journal, send};
use crate::output::{PtyInput, SessionEvent, Viewer};
use crate::sanitize::sanitize_replay;
use crate::session::{hostname, live_size, Table};
use crate::{ConnWriter, VIEWER_QUEUE};
use berthd::{control_frame, Control, Frame};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as TokioMutex};

/// session に attach: 出力 sink を張り、replay を流してから live にする。
#[allow(clippy::too_many_arguments)] // 接続ローカルの attach 状態（id + sink）を両方引き回すため
pub(crate) async fn attach_session(
    table: &Table,
    wr: &Arc<TokioMutex<ConnWriter>>,
    attached: &mut Option<String>,
    attached_sink: &mut Option<mpsc::Sender<SessionEvent>>,
    session_id: String,
    serial: u32,
    cols: u16,
    rows: u16,
    // 見るだけの attach（誰も置き換えない・大きさを変えない・遅れたら外される）。
    read_only: bool,
    // 前回このペインで見せた録画の位置。録画の中なら続きだけ流す（`None` は全体）。
    seen_total: Option<u64>,
    peer: berthd::Peer,
    client: Option<String>,
    claim: berthd::Claim,
) {
    let (sink_tx, mut sink_rx) =
        mpsc::channel::<SessionEvent>(if read_only { VIEWER_QUEUE } else { 256 });
    // 外す知らせの口（見る側だけが使う）。注意: 操作する側では送り手がこの関数の終わりで落ちるため、
    // forward task はそれを「外された」ではなく「知らせの口が無い」と解釈する。
    let (kick_tx, mut kick_rx) = tokio::sync::oneshot::channel::<&'static str>();
    let snapshot: Vec<u8>;
    let replay_head: Option<berth_protocol::ReplayHead>;
    // セッションの累計バイト。client はこれを基準に「どこまで見せたか」を覚える。
    let replay_total: u64;
    let name: String;
    // この attach が pty の大きさを実際に変えるか。決められるのはここだけ。
    // client は「自分の端末の大きさ」しか知らず、セッションの側は `stty` でも動く。
    let resized: bool;
    // 大きさを合わせる送り口（送るのは表のロックを外してから）。
    let mut resize_tx: Option<mpsc::Sender<PtyInput>> = None;
    {
        let tbl = table.lock().await;
        let Some(sess) = tbl.get(&session_id) else {
            drop(tbl);
            let _ = send(
                wr,
                &control_frame(
                    berthd::SOURCE,
                    serial,
                    &Control::Error {
                        code: "no_such_session".into(),
                        message: format!("{session_id} not found"),
                    },
                ),
            )
            .await;
            return;
        };
        // この接続が前に張った口（別の session でも、同じ session の別の形でも）を先に外す。
        // 残すと前の session の出力がこの接続へ流れ続け、口も表に残り続ける。
        if let (Some(prev), Some(mine)) = (attached.as_deref(), attached_sink.as_ref()) {
            if let Some(p) = tbl.get(prev) {
                if let Ok(mut o) = p.output.lock() {
                    o.forget(mine);
                }
            }
        }
        // snapshot と sink 設定を 1 mutex で原子的に（境目のロスト防止）。
        // 既存 sink（前の client）は黙って置き換えず、下で外す知らせを送る。
        // 黙って置き換えると、前の client は何も知らされず、前の接続が送り手の控え（`attached_sink`）を
        // 持ち続けるので forward task が終わらず、台帳に end が書かれない。
        {
            let mut o = sess.output.lock().unwrap();
            // seen が録画の中なら続きだけ・頭の状態を作り直す（外 / `None` は全体）。
            let (resume_bytes, resume_head) = o.resume_from(seen_total);
            snapshot = resume_bytes;
            replay_head = resume_head;
            replay_total = o.produced;
            if read_only {
                // 見る側は誰も置き換えない。
                o.viewers.push(Viewer {
                    tx: sink_tx.clone(),
                    kick: Some(kick_tx),
                });
            } else {
                // 前の持ち主へ、外部 `berth detach` と同じ口で知らせる。前の forward task が
                // `Detached` を送って抜け（`berth` は `[detached from …]` を出す）、台帳も `detached` で終わる。
                // 形は tmux の `attach -d`（後から来た側が全部を持っていく）。
                // 注意: 待たない。前の client が読むのをやめていても、新しい持ち主を止めない。
                if o.sink.is_some() {
                    o.kick_owner("detached");
                }
                o.sink = Some(sink_tx.clone());
                o.sink_kick = Some(kick_tx);
            }
        }
        // 台帳に人が読む名前も残す（id は再利用しない内部鍵で、人が見るのは名前の方）。
        name = sess.info.name.clone();
        // 現サイズへ resize（再アタッチで端末サイズを合わせる）。
        // 見る側は大きさに口を出さない（attach の時も）。
        if read_only {
            resized = false;
        } else {
            // 注意: 送る前に読む。送った後では、自分が変えた結果を読んでしまう。
            // 引けない（master が畳まれた後）ときは `false`: 知らないことを
            // 「変えた」と言うと、client はプロンプトの出ない画面を出す。
            let now = live_size(&sess.master);
            resized = now.is_some_and(|now| now != (cols, rows));
            // 同じ大きさなら送らない。unix のカーネルは同じ大きさの resize で SIGWINCH を出さないが、
            // ConPTY は同じ大きさでも画面を描き直す（`ESC[K` で各行を消して書き直す）ため、
            // client が描き直した画面の上に置いた合図（`[attached to …]`）が消える。
            // 読めなかったとき（`None`）は送る。大きさが合っていない可能性を残さないため。
            if now != Some((cols, rows)) {
                resize_tx = Some(sess.input_tx.clone());
            }
        }
    }
    if let Some(tx) = resize_tx {
        let _ = tx.send(PtyInput::Resize { cols, rows }).await;
    }

    // Attached 応答 → replay バイト → 以後 live。replay は過去録画なので裸 BEL を落として渡す
    // （当時のベルを reattach で鳴らさない）。live push は sanitize しないため本物のベルは鳴る。
    let replay = sanitize_replay(&snapshot);
    let _ = send(
        wr,
        &control_frame(
            berthd::SOURCE,
            serial,
            &Control::Attached {
                session_id: session_id.clone(),
                replay_len: replay.len(),
                resized,
                replay_head,
                // 必ず載せる（新しい client はこれで dedupe を有効にする）。
                replay_total: Some(replay_total),
            },
        ),
    )
    .await;
    if !replay.is_empty() {
        let _ = send(wr, &Frame::data(0, replay)).await;
    }

    *attached = Some(session_id.clone());
    *attached_sink = Some(sink_tx);

    // attach した瞬間の親プロセスの並び（Rheon Protocol `spec/ACTOR_v1.md` §2.4）。
    // 注意: この瞬間に読むこと。相手が終われば pid は再利用される。
    // end の行にも同じ値を載せる（end の瞬間には相手がもう居ないことが多く、
    // 読み直すと「消えた」になり、1 行で完結しなくなる）。
    let ancestry = berthd::peer_ancestry(peer.pid);

    // attach が成立した瞬間を台帳へ（成立前に書くと、存在しない session への attach 失敗まで
    // 「入った」ことになる）。
    journal(&berthd::attach_record(
        berthd::AttachEvent::Start,
        &session_id,
        &name,
        peer,
        &ancestry,
        client.as_deref(),
        &claim,
        &hostname(),
        read_only,
    ));
    let attached_at = std::time::Instant::now();

    // forward task: sink → socket フレーム。sink が drop されるまで回る。
    //
    // 注意: attach の終わりはこの task の終わりと同値。self-detach（handle_conn が sink を外す）も、
    // 別接続からの外部 detach も、client 切断も、別接続の attach による置き換えも、最後は必ず
    // ここを抜ける（送信者が全部 drop されれば `recv` が `None` を返す）。終わり方が
    // 複数あるので、記録は分岐ごとでなく合流点に置く。
    let wr_c = wr.clone();
    tokio::spawn(async move {
        // 抜けた理由。`closed` = 送信側が全部消えた（self-detach か、同じ接続の attach し直し）。
        // 別接続による置き換えは外す知らせが来るので `detached` になる（外部 detach と同じ語）。
        let mut how = "closed";
        let mut kick_open = true;
        loop {
            let ev = tokio::select! {
                // 外す知らせ（置き換え・外部 detach・遅い見る側）。出力の口とは別なので、口が詰まっていても届く。
                why = &mut kick_rx, if kick_open => {
                    kick_open = false;
                    match why {
                        Ok(why) => {
                            // 外部 detach は `reason` を持たない。
                            let reason = (why != "detached").then(|| why.to_string());
                            let _ = send(
                                &wr_c,
                                &control_frame(
                                    berthd::SOURCE,
                                    0,
                                    &Control::Detached {
                                        session_id: session_id.clone(),
                                        reason,
                                    },
                                ),
                            )
                            .await;
                            how = why;
                            break;
                        }
                        // 知らせの口が落ちただけ（自分で離れた）。外されたのではない。
                        Err(_) => continue,
                    }
                }
                ev = sink_rx.recv() => ev,
            };
            let Some(ev) = ev else { break };
            match ev {
                SessionEvent::Output(bytes) => {
                    if send(&wr_c, &Frame::data(0, bytes)).await.is_err() {
                        how = "disconnected";
                        break;
                    }
                }
                // 内部の溜め → プロトコルの型へ。
                // 古い client は未知の control を静かに落とす（`decode_control` の
                // パターン一致が `Err` になる）ため、繋いでも壊れず届かないだけになる。
                SessionEvent::Dropped {
                    bytes,
                    events,
                    window_ms,
                } => {
                    let _ = send(
                        &wr_c,
                        &control_frame(
                            berthd::SOURCE,
                            0,
                            &Control::Dropped {
                                session_id: session_id.clone(),
                                bytes,
                                events,
                                window_ms,
                            },
                        ),
                    )
                    .await;
                }
                SessionEvent::Exited(status) => {
                    let _ = send(
                        &wr_c,
                        &control_frame(
                            berthd::SOURCE,
                            0,
                            &Control::Exited {
                                session_id: session_id.clone(),
                                status,
                            },
                        ),
                    )
                    .await;
                    how = "exited";
                    break;
                }
            }
        }
        // end にも同じ自称を載せる。end だけを読む集計（滞在時間・終わり方）が
        // 「誰の attach だったか」を start まで戻らずに言えるようにし、1 行で完結させる。
        journal(&berthd::attach_record(
            berthd::AttachEvent::End {
                how,
                ms: u64::try_from(attached_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
            &session_id,
            &name,
            peer,
            &ancestry,
            client.as_deref(),
            &claim,
            &hostname(),
            read_only,
        ));
    });
}
