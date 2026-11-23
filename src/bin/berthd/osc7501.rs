//! OSC 7501（プログラムの状態の名乗り）—— berthd が端末の代わりに受ける、ただ 1 種の問い合わせ。
//!
//! # 線の例外
//!
//! berthd は端末エミュレーションを持たない（問い合わせに答えるのは端末の仕事）。例外はこれ 1 つだけ:
//! 中のプログラムが出す `ESC ] 7501 ; ? ST` には berthd が `ESC ] 7501 ; ? ESC \` と答え、
//! 問い合わせ自体は録画にも端末にも流さない。
//!
//! 理由: コーディングエージェント（Claude Code・Antigravity 等）は起動時にこの問い合わせを出し、
//! 返事が来たときだけ状態（`ESC ] 7501 ; state=idle:… ST`）を流す。ほとんどの端末はこの OSC を
//! 知らずに黙って捨てるので、端末に任せるとエージェントは黙る。detach 中は答える端末すら居ない。
//! berthd が答えれば、どの端末の下でも・誰も見ていなくても名乗りが届き、berthd はその最後の 1 件を
//! `peek` / `ls` で返せる。
//! 問い合わせを端末へ渡さないのは、端末も答えると返事が 2 つになり、余った方がプログラムの入力に入るから。
//!
//! 注意: 他の問い合わせ（DA・カーソル位置・色 …）には今まで通り答えない（Windows の起動直後の
//! `ESC[6n` は別件・`pty_loop.rs`）。
//!
//! # 切れ目
//!
//! PTY の読み取りは任意の位置で切れるので、問い合わせが 2 回の読み取りに割れうる。∴ 出力の末尾が
//! 問い合わせの頭（`ESC` / `ESC ]` / … / `ESC ] 7501 ; ? ESC`）に一致している間は、その分だけ手元に留めて
//! 次の読み取りを待つ。留めるのは書きかけのエスケープ列だけで、端末もその続きを待っている所なので、
//! 留めても画面は変わらない（∴ 時間で吐き出す仕掛けは要らない）。

use berth_protocol::ProgramStatus;

/// 問い合わせの本体（終端の手前まで）。
const PROBE: &[u8] = b"\x1b]7501;?";
/// berthd が書き戻す返事。Claude Code は「OSC 7501 で中身が `?` で始まる物」を返事として受ける。
pub(crate) const REPLY: &[u8] = b"\x1b]7501;?\x1b\\";
/// 名乗りの本体として持つ上限（`msg` の base64 を含む）。越えた名乗りは読まない。
const BODY_CAP: usize = 4096;
/// 添え書きとして持つ上限（文字数）。
const MSG_CHARS: usize = 512;

/// 1 回の読み取りを通した結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Filtered {
    /// 録画と端末へ流すバイト（問い合わせを抜いた残り）。
    pub(crate) out: Vec<u8>,
    /// この読み取りで抜いた問い合わせの数（その数だけ返事を書く）。
    pub(crate) probes: usize,
    /// この読み取りで終わった名乗り（古い順）。`None` は `state=clear`（忘れる）。
    pub(crate) reports: Vec<Option<ProgramStatus>>,
}

/// 名乗りを拾う側の状態（流すバイトだけを見る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Scan {
    #[default]
    Normal,
    Esc,
    Osc,
    OscEsc,
}

/// セッション 1 本ぶんの濾過器。読み取りをまたいで状態を持つ。
#[derive(Debug, Default)]
pub(crate) struct Osc7501 {
    /// 問い合わせの頭に一致していて、まだ流していないバイト。
    held: Vec<u8>,
    scan: Scan,
    /// OSC の本体（`ESC ]` の後・終端の手前）。
    body: Vec<u8>,
    overflow: bool,
}

impl Osc7501 {
    pub(crate) fn filter(&mut self, batch: &[u8]) -> Filtered {
        let mut f = Filtered {
            out: Vec::with_capacity(batch.len() + self.held.len()),
            ..Filtered::default()
        };
        for &b in batch {
            self.push(b, &mut f);
        }
        f
    }

    fn push(&mut self, b: u8, f: &mut Filtered) {
        if self.held.is_empty() {
            if b == 0x1b {
                self.held.push(b);
            } else {
                self.emit(b, f);
            }
            return;
        }
        self.held.push(b);
        match probe_match(&self.held) {
            Match::Whole => {
                self.held.clear();
                f.probes += 1;
            }
            Match::Prefix => {}
            Match::No => {
                // 最後の 1 バイトは新しい頭かもしれないので、それだけ読み直す。
                let last = self.held.pop().expect("held is not empty");
                let flushed = std::mem::take(&mut self.held);
                for c in flushed {
                    self.emit(c, f);
                }
                self.push(last, f);
            }
        }
    }

    /// 流すバイト。名乗りの OSC だけを拾う（流れは変えない）。
    fn emit(&mut self, b: u8, f: &mut Filtered) {
        f.out.push(b);
        self.scan = match (self.scan, b) {
            (Scan::Normal, 0x1b) => Scan::Esc,
            (Scan::Normal, _) => Scan::Normal,
            (Scan::Esc, b']') => {
                self.body.clear();
                self.overflow = false;
                Scan::Osc
            }
            (Scan::Esc, 0x1b) => Scan::Esc,
            (Scan::Esc, _) => Scan::Normal,
            (Scan::Osc, 0x07) => {
                self.finish(f);
                Scan::Normal
            }
            (Scan::Osc, 0x1b) => Scan::OscEsc,
            (Scan::Osc, 0x18 | 0x1a) => Scan::Normal,
            (Scan::Osc, _) => {
                if self.body.len() < BODY_CAP {
                    self.body.push(b);
                } else {
                    self.overflow = true;
                }
                Scan::Osc
            }
            // ST（`ESC \`）。他の ESC も終端として扱い、そのバイトを ESC の続きとして読む。
            (Scan::OscEsc, _) => {
                self.finish(f);
                match b {
                    b'\\' => Scan::Normal,
                    b']' => {
                        self.body.clear();
                        self.overflow = false;
                        Scan::Osc
                    }
                    0x1b => Scan::Esc,
                    _ => Scan::Normal,
                }
            }
        };
    }

    fn finish(&mut self, f: &mut Filtered) {
        if self.overflow {
            return;
        }
        if let Some(rest) = self.body.strip_prefix(b"7501;") {
            if let Some(report) = parse_report(rest) {
                f.reports.push(report);
            }
        }
    }
}

enum Match {
    Whole,
    Prefix,
    No,
}

/// `held` が問い合わせ（`PROBE` ＋ BEL か `ESC \`）と比べてどこまで一致しているか。
fn probe_match(held: &[u8]) -> Match {
    for term in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
        let whole = [PROBE, term].concat();
        if held == whole.as_slice() {
            return Match::Whole;
        }
        if whole.starts_with(held) {
            return Match::Prefix;
        }
    }
    Match::No
}

/// `state=…:kind=…:app=…:msg=…` を読む。`?` で始まる物（問い合わせ・返事）と `state` の無い物は名乗りではない。
/// 返り: `Some(None)` は `state=clear`。
fn parse_report(body: &[u8]) -> Option<Option<ProgramStatus>> {
    if body.first() == Some(&b'?') {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut st = ProgramStatus::default();
    for field in text.split(':') {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "state" => st.state = v.to_string(),
            "kind" => st.kind = Some(v.to_string()),
            "app" => st.app = Some(v.to_string()),
            "msg" => {
                st.msg = decode_base64(v).map(|b| {
                    String::from_utf8_lossy(&b)
                        .chars()
                        .take(MSG_CHARS)
                        .collect()
                })
            }
            _ => {}
        }
    }
    if st.state.is_empty() {
        return None;
    }
    Some((st.state != "clear").then_some(st))
}

/// 標準の base64（`+` `/`・`-` `_` も受ける・`=` の有無を問わない）。壊れていれば `None`。
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Filtered {
        let mut o = Osc7501::default();
        let mut all = Filtered::default();
        for c in chunks {
            let f = o.filter(c);
            all.out.extend(f.out);
            all.probes += f.probes;
            all.reports.extend(f.reports);
        }
        all
    }

    #[test]
    fn a_probe_is_taken_out_and_counted() {
        for probe in [&b"\x1b]7501;?\x07"[..], b"\x1b]7501;?\x1b\\"] {
            let mut input = b"before".to_vec();
            input.extend_from_slice(probe);
            input.extend_from_slice(b"after");
            let f = run(&[&input]);
            assert_eq!(f.out, b"beforeafter");
            assert_eq!(f.probes, 1);
        }
    }

    /// 読み取りの切れ目がどこにあっても、抜けるのは同じ。
    #[test]
    fn a_probe_split_anywhere_is_still_taken_out() {
        let input = b"ab\x1b]7501;?\x1b\\cd";
        for cut in 1..input.len() {
            let f = run(&[&input[..cut], &input[cut..]]);
            assert_eq!(f.out, b"abcd", "cut at {cut}");
            assert_eq!(f.probes, 1, "cut at {cut}");
        }
    }

    #[test]
    fn things_that_only_look_like_a_probe_pass_through() {
        for input in [
            &b"\x1b]7501;?x\x07"[..],
            b"\x1b]75\x07",
            b"\x1b]7502;?\x07",
            b"\x1b[31mred\x1b[0m",
            b"\x1b]0;title\x07",
            b"\x1b\x1b]7501;",
        ] {
            let f = run(&[input]);
            assert_eq!(f.probes, 0, "{input:?}");
            // 末尾が問い合わせの頭なら留まる。それ以外は全部流れる。
            let held = input.ends_with(b"\x1b]7501;");
            if held {
                assert_eq!(f.out, b"\x1b");
            } else {
                assert_eq!(f.out, input, "{input:?}");
            }
        }
    }

    /// 頭に一致して留めた分は、続きが違えば欠けずに出る（2 つ目の ESC は新しい頭として読み直す）。
    #[test]
    fn held_bytes_come_out_once_the_next_byte_disagrees() {
        let f = run(&[b"x\x1b]75", b"01;", b"?", b"\x1b", b"[0m"]);
        assert_eq!(f.out, b"x\x1b]7501;?\x1b[0m");
        assert_eq!(f.probes, 0);
    }

    #[test]
    fn two_probes_in_a_row_are_two_replies() {
        let f = run(&[b"\x1b]7501;?\x07\x1b]7501;?\x07"]);
        assert!(f.out.is_empty());
        assert_eq!(f.probes, 2);
    }

    #[test]
    fn a_report_passes_through_and_is_read() {
        // `msg` は base64（"Run sleep 12?"）。
        let input =
            b"\x1b]7501;state=blocked:kind=permission:app=agy:msg=UnVuIHNsZWVwIDEyPw==\x1b\\";
        let f = run(&[&input[..20], &input[20..]]);
        assert_eq!(f.out, input);
        assert_eq!(f.probes, 0);
        let st = f.reports[0].clone().expect("a state");
        assert_eq!(st.state, "blocked");
        assert_eq!(st.kind.as_deref(), Some("permission"));
        assert_eq!(st.app.as_deref(), Some("agy"));
        assert_eq!(st.msg.as_deref(), Some("Run sleep 12?"));
    }

    #[test]
    fn clear_forgets_and_noise_is_not_a_report() {
        let f = run(&[b"\x1b]7501;state=idle:app=claude-code\x07\x1b]7501;state=clear\x07"]);
        assert_eq!(f.reports.len(), 2);
        assert_eq!(f.reports[0].as_ref().unwrap().state, "idle");
        assert_eq!(f.reports[1], None);
        // state の無い物・問い合わせの返事の形は名乗りではない。
        let f = run(&[b"\x1b]7501;app=x\x07\x1b]7501;?foo\x07\x1b]2;t\x07"]);
        assert!(f.reports.is_empty());
    }

    #[test]
    fn base64_without_padding_and_broken_base64() {
        assert_eq!(decode_base64("aGk").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64("44GC").as_deref(), Some("あ".as_bytes()));
        assert_eq!(decode_base64("a"), None);
        assert_eq!(decode_base64("a*b="), None);
    }
}
