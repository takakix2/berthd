use crate::conn::sink_is;
use crate::{HEAD_HARD_CAP, HEAD_SLACK, REPLAY_CAP};
use berthd::replay_head::HeadTracker;
use std::collections::VecDeque;
use tokio::sync::mpsc;

/// フロントエンド → io ループへの命令。
///
/// 打鍵（データ）はここを通らない（書き込み専用スレッド `spawn_writer_thread` へ直接行く）。
/// 注意: PTY への書き込みは、子が出力で止まっていると返らない。io ループの中で書くと、その間
/// 出力を捌けず（子はさらに止まる）、`Close` も届かなくなる。
pub(crate) enum PtyInput {
    Resize { cols: u16, rows: u16 },
    Close,
}

/// PTY → attached client へのイベント（forward task が socket フレームに変換）。
// `Debug` は検体のため。「何が来たか」を名乗れないと `panic!` が
// 「期待と違う」しか言えない。
#[derive(Debug)]
pub(crate) enum SessionEvent {
    Output(Vec<u8>),
    Exited(i32),
    /// 落としたことの内部通知。`Control::Dropped` へ変換されて出る。
    Dropped {
        bytes: u64,
        events: u32,
        window_ms: u32,
    },
}

/// 見るだけの attach 1 本（`berth attach -r`）。
pub(crate) struct Viewer {
    pub(crate) tx: mpsc::Sender<SessionEvent>,
    /// 外す知らせ（理由つき・`"too_slow"` / `"detached"`）。forward task が受けて `Detached` を送り、
    /// 台帳の `how` に理由を書く。
    /// 出力の口（`tx`）とは別の口にしてある。外す理由の筆頭が「その口が詰まった」なので、
    /// 同じ口で知らせると届かない（`dropped_bytes` の doc と同じ形）。
    pub(crate) kick: Option<tokio::sync::oneshot::Sender<&'static str>>,
}

/// `replay_buf` + 現在 attach 中の client への出力口を 1 つの mutex で束ねる。
/// これで「attach 時の snapshot」と「PTY 出力の push」が原子的になり、境目でのロスト/重複を防ぐ。
pub(crate) struct SessionOutput {
    pub(crate) replay_buf: VecDeque<u8>,
    /// `replay_buf` から捨てたバイトが残した状態（alt screen・SGR・題名）。捨てる順に全部食う。
    pub(crate) head: HeadTracker,
    /// セッション開始から produce した累計バイト数（単調増加）。`replay_buf` は常にその
    /// 末尾、バイト `[produced − replay_buf.len(), produced)` に対応する。attach で `Attached.replay_total` に載せ、
    /// client が「どこまで見せたか」の基準にする（同じペインで入り直したとき続きだけ流すため）。
    pub(crate) produced: u64,
    /// 操作する側の口（1 本だけ）。溢れたら pump が待つ（背圧）。
    pub(crate) sink: Option<mpsc::Sender<SessionEvent>>,
    /// 操作する側の外す知らせ（[`Viewer::kick`] と同じ形）。置き換え・外部 detach はこちらで知らせる。
    /// 出力の口（`sink`）は、前の持ち主が読むのをやめていると満杯で、待つと新しい持ち主まで止まる。
    pub(crate) sink_kick: Option<tokio::sync::oneshot::Sender<&'static str>>,
    /// 見る側の口（何本でも）。pump は待たず、溢れた見る側は外す。
    /// 見る側 1 本（遅い回線）のために操作する側を止めないため。
    pub(crate) viewers: Vec<Viewer>,
    /// 落とした分は「送る物」ではなく「持つ物」。
    ///
    /// 通知は、溢れているその同じ口を通る。`try_send` が満杯で失敗した瞬間に
    /// 「落とした」を送ろうとしても、それも失敗する。∴ ここに溜めて、
    /// 口が空いた最初の機会に吐く（[`SessionOutput::stage`]）。間引きの規則を別に書く必要は無い。
    pub(crate) dropped_bytes: u64,
    pub(crate) dropped_events: u32,
    pub(crate) dropped_since: Option<std::time::Instant>,
    /// セッション作成時刻（一度も出力が無いときの `idle_ms` 基準）。
    pub(crate) created_at: std::time::Instant,
    /// 最後に PTY から出力があった時刻（`stage` が更新する）。
    pub(crate) last_output: Option<std::time::Instant>,
    /// 直近の `SendKeys` 送信時点での `produced`。無反応のままの連打検知に使う。
    pub(crate) last_send_produced: Option<u64>,
    /// `produced` が進まないまま連続で `SendKeys` された回数。
    pub(crate) consecutive_sends: u32,
    /// 中のプログラムが OSC 7501 で名乗った最後の状態と、名乗った時刻（`osc7501.rs`）。
    program_status: Option<(berth_protocol::ProgramStatus, std::time::Instant)>,
}

/// 出力が進まないまま連続で `SendKeys` できる上限回数（無限ループ防止）。
pub(crate) const SEND_LOOP_LIMIT: u32 = 5;

impl SessionOutput {
    pub(crate) fn new() -> Self {
        Self {
            replay_buf: VecDeque::new(),
            head: HeadTracker::default(),
            produced: 0,
            sink: None,
            sink_kick: None,
            viewers: Vec::new(),
            dropped_bytes: 0,
            dropped_events: 0,
            dropped_since: None,
            created_at: std::time::Instant::now(),
            last_output: None,
            last_send_produced: None,
            consecutive_sends: 0,
            program_status: None,
        }
    }

    /// 名乗りを覚える（`None` は `state=clear` ＝ 忘れる）。古い順に渡すこと（最後の 1 件が残る）。
    ///
    /// 注意: 同じ名乗りの繰り返しでは時刻を進めない。`age_ms` は「その状態になってから」で、
    /// 描き直しのたびに同じ状態を名乗るプログラムが居ても、確認待ちの放置の長さが 0 に戻らないように。
    pub(crate) fn record_program_status(&mut self, report: Option<berth_protocol::ProgramStatus>) {
        self.program_status = match (report, self.program_status.take()) {
            (Some(new), Some((old, at))) if new == old => Some((old, at)),
            (Some(new), _) => Some((new, std::time::Instant::now())),
            (None, _) => None,
        };
    }

    /// 最後の名乗り（`age_ms` は今の時点で数える）。
    pub(crate) fn program_status(&self) -> Option<berth_protocol::ProgramStatus> {
        self.program_status
            .as_ref()
            .map(|(s, at)| berth_protocol::ProgramStatus {
                age_ms: u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..s.clone()
            })
    }
    /// replay_buf へ追記し、送るべき物を返す（注意: このロックの中では送らない）。
    ///
    /// `sink` は `tokio::mpsc` なので待つには `.await` が要るが、ここは `std::sync::Mutex` の中で、
    /// `StdMutex` を握ったまま await はできない。∴ 「追記して、送る物を返す」だけにする。
    ///
    /// 報告が先、データが後。逆にすると「落ちた後に来たデータ」の手前に報告が来ないので、
    /// どこで落ちたかが読めなくなる（報告の値打ちはその位置に在る）。
    pub(crate) fn stage(
        &mut self,
        bytes: &[u8],
    ) -> Option<(mpsc::Sender<SessionEvent>, Option<SessionEvent>)> {
        self.replay_buf.extend(bytes.iter().copied());
        self.produced += bytes.len() as u64;
        if !bytes.is_empty() {
            self.last_output = Some(std::time::Instant::now());
        }
        self.trim_head();
        let s = self.sink.clone()?;
        // 溜まっていた報告はここで取り切る。溢れでは増えない（背圧で待つ）が、
        // client が消えた瞬間の取りこぼしだけは `record_drop` で溜まる。
        let pending = (self.dropped_events > 0).then(|| {
            let ev = SessionEvent::Dropped {
                bytes: self.dropped_bytes,
                events: self.dropped_events,
                window_ms: self.dropped_since.map_or(0, |t| {
                    u32::try_from(t.elapsed().as_millis()).unwrap_or(u32::MAX)
                }),
            };
            self.dropped_bytes = 0;
            self.dropped_events = 0;
            self.dropped_since = None;
            ev
        });
        Some((s, pending))
    }

    /// ここへ来るのは「client が送信中に消えた」時だけ。
    /// 溢れでは来ない（溢れは待って捌く、`run_pty_io_loop`）。
    pub(crate) fn record_drop(&mut self, n: usize) {
        self.dropped_bytes += n as u64;
        self.dropped_events += 1;
        self.dropped_since
            .get_or_insert_with(std::time::Instant::now);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.replay_buf.iter().copied().collect()
    }

    /// 最後に PTY 出力があってからの経過ミリ秒（未出力なら作成からの経過）。
    pub(crate) fn idle_ms(&self) -> u64 {
        let base = self.last_output.unwrap_or(self.created_at);
        u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// 覗き見（`Peek`）用に末尾最大 `max_bytes` を切り出す。
    ///
    /// 切り口が命令・文字の途中にならないよう [`HeadTracker::at_boundary`] まで進める。
    /// 返り: `(bytes, replay_head, idle_ms, produced, attached)`。
    pub(crate) fn peek_tail(
        &self,
        max_bytes: Option<usize>,
    ) -> (Vec<u8>, Option<berth_protocol::ReplayHead>, u64, u64, bool) {
        let buf = self.snapshot();
        let (slice, head) = match max_bytes {
            Some(max) if buf.len() > max => {
                let mut skip = buf.len() - max;
                let mut head = self.head.clone();
                for &b in &buf[..skip] {
                    head.feed(b);
                }
                while skip < buf.len() && !head.at_boundary() {
                    head.feed(buf[skip]);
                    skip += 1;
                }
                (buf[skip..].to_vec(), head.head())
            }
            _ => (buf, self.head.head()),
        };
        (
            slice,
            head,
            self.idle_ms(),
            self.produced,
            self.sink.is_some(),
        )
    }

    /// `SendKeys` の連続無応答送信を数えてループを防ぐ。
    ///
    /// 前回の送信から `produced` が 1 バイトも増えていない送信が [`SEND_LOOP_LIMIT`] 回を超えると
    /// `Err(SEND_LOOP_LIMIT)` を返す（`force: true` のときは上限を無視して通過させる）。
    pub(crate) fn check_and_record_send(&mut self, force: bool) -> Result<(), u32> {
        let next_count = match self.last_send_produced {
            Some(prev) if prev == self.produced => self.consecutive_sends + 1,
            _ => 1,
        };
        if !force && next_count > SEND_LOOP_LIMIT {
            return Err(SEND_LOOP_LIMIT);
        }
        self.consecutive_sends = next_count;
        self.last_send_produced = Some(self.produced);
        Ok(())
    }

    /// attach の録画を切り出す。`seen_total`（前回このペインで見せた累計位置）が録画の中にあれば
    /// 続きだけ返し、頭の状態はそこまでを [`HeadTracker`] に通して作り直す。
    /// 録画から落ちている・`None`（ペイン未知 / 古い client）なら全体を返す（既存の `head`）。
    ///
    /// 注意: `seen` は client が数えたバイト位置なので、命令や文字の途中を指しうる。そのまま切ると頭に
    /// ゴミ（命令の尻尾・壊れた文字）が出るので、切り口を [`HeadTracker::at_boundary`] まで前へ進める。
    /// 進めた分のバイトも head が食うので状態は失わない（失うのは境目の 1 命令 / 1 文字の表示だけ）。
    /// 返り: (送る録画バイト, 頭の状態)。
    pub(crate) fn resume_from(
        &self,
        seen_total: Option<u64>,
    ) -> (Vec<u8>, Option<berth_protocol::ReplayHead>) {
        let buf = self.snapshot();
        let floor = self.produced - buf.len() as u64;
        if let Some(seen) = seen_total {
            if seen >= floor && seen <= self.produced {
                let mut skip = (seen - floor) as usize;
                let mut head = self.head.clone();
                for &b in &buf[..skip] {
                    head.feed(b);
                }
                while skip < buf.len() && !head.at_boundary() {
                    head.feed(buf[skip]);
                    skip += 1;
                }
                return (buf[skip..].to_vec(), head.head());
            }
        }
        (buf, self.head.head())
    }

    /// 容量を越えた分を頭から捨てる。捨てるバイトは全部 [`HeadTracker`] が食い、越えた分の
    /// あとも行頭（[`HEAD_SLACK`] の中に無ければ命令の外・文字の外）まで捨て続ける。
    fn trim_head(&mut self) {
        if self.replay_buf.len() <= REPLAY_CAP {
            return;
        }
        let over = self.replay_buf.len() - REPLAY_CAP;
        for b in self.replay_buf.drain(..over) {
            self.head.feed(b);
        }
        let mut extra = 0usize;
        while let Some(&b) = self.replay_buf.front() {
            if self.head.at_line_start()
                || (extra >= HEAD_SLACK && self.head.at_boundary())
                || extra >= HEAD_HARD_CAP
            {
                break;
            }
            self.replay_buf.pop_front();
            self.head.feed(b);
            extra += 1;
        }
    }

    /// 見る側を外す。外した見る側の forward task へ理由を渡す。
    /// 知らせは oneshot の同期送信なので、このロックの中で撃ってよい（待たない）。
    pub(crate) fn kick_viewers(
        &mut self,
        pick: impl Fn(&mpsc::Sender<SessionEvent>) -> bool,
        why: &'static str,
    ) {
        self.viewers.retain_mut(|v| {
            if pick(&v.tx) {
                if let Some(k) = v.kick.take() {
                    let _ = k.send(why);
                }
                false
            } else {
                true
            }
        });
    }

    /// 操作する側を外して知らせる（待たない）。forward task が `Detached` を送って抜ける。
    pub(crate) fn kick_owner(&mut self, why: &'static str) {
        self.sink = None;
        if let Some(k) = self.sink_kick.take() {
            let _ = k.send(why);
        }
    }

    /// この接続が張った口（操作する側でも見る側でも）を、知らせずに外す（自分で離れた場合）。
    pub(crate) fn forget(&mut self, mine: &mpsc::Sender<SessionEvent>) {
        if sink_is(self, mine) {
            self.sink = None;
            self.sink_kick = None;
        }
        self.viewers.retain(|v| !v.tx.same_channel(mine));
    }
}

#[cfg(test)]
mod head_tests {
    //! 録画の頭は行頭（か、せめて命令の外）で切れ、捨てた分の状態が残る。
    use super::*;

    fn out() -> SessionOutput {
        SessionOutput::new()
    }

    fn lines(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("\x1b[31mline {i} あいう\x1b[0m\n").into_bytes())
            .collect()
    }

    #[test]
    fn below_the_cap_nothing_is_dropped_and_there_is_no_head() {
        let mut o = out();
        o.stage(&lines(100));
        assert_eq!(o.head.head(), None);
        assert_eq!(o.snapshot(), lines(100));
    }

    #[test]
    fn over_the_cap_the_head_starts_at_a_line_and_remembers_the_state() {
        let mut o = out();
        // 色を付けたまま・alt の中・題名を変えたまま、大量に流す。
        o.stage(b"\x1b]2;vim\x07\x1b[?1049h\x1b[1m");
        let body = lines(40_000);
        for chunk in body.chunks(4096) {
            o.stage(chunk);
        }
        let snap = o.snapshot();
        assert!(snap.len() <= REPLAY_CAP, "{}", snap.len());
        assert!(snap.len() > REPLAY_CAP - HEAD_SLACK - 64, "{}", snap.len());
        assert!(
            snap.starts_with(b"\x1b[31mline "),
            "head: {:?}",
            String::from_utf8_lossy(&snap[..40])
        );
        let head = o.head.head().unwrap();
        assert!(head.alt_screen);
        assert_eq!(head.title.as_deref(), Some("vim"));
        // 各行の末尾の `ESC[0m` で色は戻っている・太字も戻っている。
        assert_eq!(head.sgr, "");
    }

    #[test]
    fn the_state_at_the_cut_is_the_state_after_the_last_dropped_byte() {
        let mut o = out();
        o.stage(b"\x1b[1m");
        let body = lines(40_000);
        for chunk in body.chunks(4096) {
            o.stage(chunk);
        }
        // 最後に捨てた行の `ESC[0m` が効いて、太字も消えている。
        assert_eq!(o.head.head().unwrap().sgr, "");
        // 行の中で切れる場合は、その行の色が効いたまま。
        let mut o = out();
        let mut row = b"\x1b[31m".to_vec();
        row.extend(std::iter::repeat_n(b'x', REPLAY_CAP + HEAD_SLACK * 2));
        o.stage(&row);
        let snap = o.snapshot();
        assert!(snap.len() <= REPLAY_CAP);
        assert!(
            snap.iter().all(|&b| b == b'x'),
            "a line without a newline is cut at a boundary"
        );
        assert_eq!(o.head.head().unwrap().sgr, "\x1b[31m");
    }

    #[test]
    fn a_cut_inside_a_sequence_does_not_leave_its_tail_in_the_head() {
        // 容量をちょうど `ESC[` の直後で越えさせる（越えた分 = 頭の 12 バイト）。残りの `31m…` は行頭まで一緒に捨てられる。
        let mut o = out();
        let mut body = b"aaaaaaaaaa\x1b[31mrest of line\nnext line\n".to_vec();
        let filler = REPLAY_CAP + 12 - body.len();
        body.extend(std::iter::repeat_n(b'z', filler));
        o.stage(&body);
        let snap = o.snapshot();
        assert!(
            snap.starts_with(b"next line\n"),
            "{:?}",
            String::from_utf8_lossy(&snap[..20])
        );
        assert_eq!(o.head.head().unwrap().sgr, "\x1b[31m");
    }

    #[test]
    fn a_character_is_never_cut_in_half() {
        // 越えた分（3 バイト）が `あ` の 1 バイト目で終わるため、残り 2 バイトは一緒に捨てられる。
        let mut o = out();
        let mut body = b"aa".to_vec();
        body.extend_from_slice("あ".as_bytes());
        body.extend(std::iter::repeat_n(b'b', REPLAY_CAP - 2));
        o.stage(&body);
        let snap = o.snapshot();
        assert!(snap.iter().all(|&b| b == b'b'), "{:?}", &snap[..4]);
        assert!(snap.len() <= REPLAY_CAP);
    }
}

#[cfg(test)]
mod resume_tests {
    //! `seen` が録画の中なら続きだけ・頭の状態を作り直す。外 / `None` なら全体。
    use super::*;

    fn out() -> SessionOutput {
        SessionOutput::new()
    }

    #[test]
    fn produced_counts_every_staged_byte() {
        let mut o = out();
        o.stage(b"hello");
        o.stage(b" world\n");
        assert_eq!(o.produced, 12);
    }

    #[test]
    fn seen_in_the_ring_returns_only_the_tail() {
        let mut o = out();
        o.stage(b"hello world\n"); // 12 バイト・容量以下なので floor = 0。
        let (bytes, head) = o.resume_from(Some(6));
        assert_eq!(bytes, b"world\n");
        // 何か食ったので頭は Some（既定状態）。
        assert!(head.is_some());
    }

    #[test]
    fn seen_equal_to_produced_returns_nothing() {
        let mut o = out();
        o.stage(b"hello world\n");
        let (bytes, _head) = o.resume_from(Some(o.produced));
        assert!(bytes.is_empty(), "全部見ていたら録画は空: {bytes:?}");
    }

    #[test]
    fn seen_out_of_the_ring_returns_the_whole_thing() {
        let mut o = out();
        o.stage(b"\x1b[1m");
        let body = head_tests_lines(40_000);
        for chunk in body.chunks(4096) {
            o.stage(chunk);
        }
        // 録画の頭はもう捨てられている（floor > 0）。seen = 0 は録画から落ちている → 全体。
        let floor = o.produced - o.snapshot().len() as u64;
        assert!(floor > 0);
        let (bytes, head) = o.resume_from(Some(0));
        assert_eq!(bytes, o.snapshot());
        assert_eq!(head, o.head.head());
        // None も同じ（ペイン未知・古い client）。
        let (bytes, head) = o.resume_from(None);
        assert_eq!(bytes, o.snapshot());
        assert_eq!(head, o.head.head());
    }

    #[test]
    fn a_seen_inside_a_sequence_advances_to_a_boundary_and_keeps_its_state() {
        let mut o = out();
        o.stage(b"ab\x1b[31mCD\n"); // ESC[ は index 2..=3・seen=4 は CSI の途中。
        let (bytes, head) = o.resume_from(Some(4));
        assert_eq!(bytes, b"CD\n", "命令の途中で切らず境目まで進める");
        assert_eq!(head.unwrap().sgr, "\x1b[31m", "進めた分の色は頭に残る");
    }

    #[test]
    fn a_seen_inside_a_character_does_not_split_it() {
        let mut o = out();
        let mut body = b"x ".to_vec();
        body.extend_from_slice("あ".as_bytes()); // index 2..=4（3 バイト）。
        body.push(b'\n');
        o.stage(&body);
        // seen = 3 は `あ` の 1 バイト目の途中 → 残り 2 バイトも一緒に進める。
        let (bytes, _head) = o.resume_from(Some(3));
        assert_eq!(bytes, b"\n", "文字を半分で切らない");
    }

    // head_tests の `lines` を使い回す（同じ fixture）。
    fn head_tests_lines(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("\x1b[31mline {i} あいう\x1b[0m\n").into_bytes())
            .collect()
    }
}

#[cfg(test)]
mod drop_report_tests {
    use super::*;
    use crate::pty_loop::notify_exit;
    use std::sync::{Arc, Mutex as StdMutex};

    /// 操作する側の口が満杯の瞬間に子が終わっても、`Exited` は落ちずに届く。
    #[tokio::test]
    async fn the_exit_reaches_an_owner_whose_queue_is_full() {
        let (o, mut rx) = out_with(1);
        o.sink
            .as_ref()
            .unwrap()
            .try_send(SessionEvent::Output(b"last".to_vec()))
            .unwrap();
        let output = Arc::new(StdMutex::new(o));
        let reader = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let first = rx.recv().await;
            let second = rx.recv().await;
            (first, second)
        });
        notify_exit(&output, 7).await;
        drop(output);
        let (first, second) = reader.await.unwrap();
        assert!(matches!(first, Some(SessionEvent::Output(_))), "{first:?}");
        assert!(
            matches!(second, Some(SessionEvent::Exited(7))),
            "🚨 満杯の口に Exited が落ちた: {second:?}"
        );
    }

    fn out_with(cap: usize) -> (SessionOutput, mpsc::Receiver<SessionEvent>) {
        let (tx, rx) = mpsc::channel::<SessionEvent>(cap);
        let mut o = SessionOutput::new();
        o.sink = Some(tx);
        (o, rx)
    }

    #[test]
    fn send_loop_limit_blocks_unanswered_repeats_until_output_advances() {
        let mut o = SessionOutput::new();
        for _ in 0..SEND_LOOP_LIMIT {
            assert!(o.check_and_record_send(false).is_ok());
        }
        assert_eq!(o.check_and_record_send(false), Err(SEND_LOOP_LIMIT));
        // force なら通る。
        assert!(o.check_and_record_send(true).is_ok());
        // 出力が 1 バイトでも進めばカウンタがリセットされる。
        o.stage(b"x");
        assert!(o.check_and_record_send(false).is_ok());
    }

    // この一群が見るのは `record_drop` ＋ `stage()`。
    //
    // 溢れでは落ちない（`run_pty_io_loop` が待つ）ので、「口を溢れさせて Dropped が出ること」を
    // 測る書き方は、起きえないことを測る緑になる。残っている唯一の落下は
    // 「client が送信中に消えた」で、そこは `record_drop` が受ける。
    // 「溢れても 1 バイトも落ちない」の方は、この層では測れない（`run_pty_io_loop` と実 PTY が要る）。

    /// 陽性対照。下の検査は「Dropped が出ること」を見るが、
    /// 溜めを作れていなければ何も起きず、それでも緑になる書き方が在りうる。
    /// ∴ まず「溜めを作れる」ことを示す。
    #[tokio::test]
    async fn the_probe_can_actually_record_a_drop() {
        let (mut o, _rx) = out_with(1);
        o.record_drop(1);
        assert_eq!(o.dropped_events, 1, "溜めを作れていない（検体が空振り）");
        assert_eq!(o.dropped_bytes, 1);
    }

    /// 落としても黙らない。次に送る物を組むとき、報告が先に出る。
    #[tokio::test]
    async fn a_drop_is_reported_with_the_next_batch() {
        let (mut o, _rx) = out_with(2);
        o.record_drop(3);
        o.record_drop(4);
        assert_eq!((o.dropped_events, o.dropped_bytes), (2, 7));

        let (_sink, pending) = o.stage(b"d").expect("sink が在る");
        // 報告が先、データが後（この順でないと「どこで落ちたか」が読めない）。
        match pending.expect("報告が組まれていない") {
            SessionEvent::Dropped { bytes, events, .. } => {
                assert_eq!((bytes, events), (7, 2), "畳んだ合計と回数が合わない");
            }
            other => panic!("報告ではない: {other:?}"),
        }
        assert_eq!(o.dropped_events, 0, "報告を組んだのに溜めが残っている");
    }

    /// 報告を組むまでは溜めたまま（報告自体を落とさない）。
    #[tokio::test]
    async fn a_report_is_kept_until_it_is_staged() {
        let (mut o, _rx) = out_with(1);
        o.record_drop(1);
        o.record_drop(1);
        assert_eq!((o.dropped_events, o.dropped_bytes), (2, 2), "溜めが消えた");
        let (_s, pending) = o.stage(b"d").expect("sink が在る");
        assert!(matches!(
            pending,
            Some(SessionEvent::Dropped { events: 2, .. })
        ));
    }

    /// sink が無ければ何も返さない（detach 中は replay_buf だけが伸びる）。
    /// それでも replay_buf には積む（reattach の replay がそこから作られるため）。
    #[tokio::test]
    async fn a_detached_session_still_fills_the_ring() {
        let (mut o, _rx) = out_with(4);
        o.sink = None;
        assert!(o.stage(b"abc").is_none(), "sink が無いのに送る物を返した");
        assert_eq!(
            o.snapshot(),
            b"abc",
            "detach 中に replay_buf が伸びていない"
        );
    }

    /// 溜めが無ければ報告は組まれない（陰性対照）。
    #[tokio::test]
    async fn a_healthy_sink_never_reports() {
        let (mut o, _rx) = out_with(8);
        for _ in 0..4 {
            let (_s, pending) = o.stage(b"x").expect("sink が在る");
            assert!(pending.is_none(), "落ちていないのに報告が組まれた");
        }
        assert_eq!(o.dropped_events, 0);
    }
}

#[cfg(test)]
mod program_status_tests {
    //! 名乗りは最後の 1 件。同じ名乗りの繰り返しでは「その状態になってから」の時計を戻さない。
    use super::*;
    use berth_protocol::ProgramStatus;

    fn st(state: &str) -> ProgramStatus {
        ProgramStatus {
            state: state.into(),
            ..ProgramStatus::default()
        }
    }

    #[test]
    fn the_same_report_again_keeps_the_age_and_a_new_one_resets_it() {
        let mut o = SessionOutput::new();
        assert_eq!(o.program_status(), None);
        o.record_program_status(Some(st("blocked")));
        std::thread::sleep(std::time::Duration::from_millis(30));
        o.record_program_status(Some(st("blocked")));
        let age = o.program_status().unwrap().age_ms;
        assert!(age >= 30, "the repeat reset the clock: {age}");
        o.record_program_status(Some(st("working")));
        let now = o.program_status().unwrap();
        assert_eq!(now.state, "working");
        assert!(now.age_ms < 30, "a new state starts its own clock");
        o.record_program_status(None);
        assert_eq!(o.program_status(), None, "clear forgets");
    }
}
