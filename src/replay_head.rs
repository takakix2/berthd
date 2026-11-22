//! 録画（replay）の頭より前に**捨てた**出力が残した状態（protocol 12 から）。
//!
//! 録画は有界（`REPLAY_CAP`）で古いバイトから捨てるので、切れ目はバイトの位置で決まり、意味の区切りでは
//! 切れない。切れ目に来る物は 3 つ: 命令の途中（`ESC[` が消えて `31m` が文字として残る）・UTF-8 の文字の途中・
//! **前提がすべて消える**（`ESC[?1049h` が捨てられ、入り直したときの描き直しが vim の格子を普通の行として
//! スクロールへ積む。色を付けたまま、題名を変えたまま、も同じ）。
//!
//! # 命令ではなく状態を持つ
//!
//! 命令の列は捨ててよい。命令が残した**状態**はセッションに続いている。ここは捨てるバイトだけを
//! 1 バイトずつ食い、スクロールへの描き直しに効く 3 つ（alt screen に居るか・文字の色と装飾・題名）を
//! 畳んで持つ。画面の中身（格子）は持たない（berthd は画面の模型を持たない）という決めの
//! 範囲に収まる。費用は録画が一杯になってから・捨てる分だけ。
//!
//! # 切る位置も選ぶ
//!
//! [`HeadTracker::at_line_start`] / [`HeadTracker::at_boundary`] で、呼ぶ側（`SessionOutput::stage`）は容量を
//! 越えた分を捨てた**あとも**行頭まで捨て続ける。∴ 録画の頭は常にトークン境界、ほぼ常に行頭となり、
//! 最初の 2 つは根本から起きず、3 つ目だけがここで渡す状態の仕事になる。
//!
//! 注意: 食うのは**捨てる順**に全部であり、途中を抜くと状態が嘘になる。切れ目が命令の途中でも、残りはあとで
//! 捨てられるときに食うので、状態は常にストリームに対して正しい。

use berth_protocol::ReplayHead;

/// 色（SGR の fg / bg）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Color {
    #[default]
    Default,
    /// 0–7 が通常色・8–15 が明るい色・以後が 256 色。
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// 文字の色と装飾（SGR）の畳んだ形。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Sgr {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strike: bool,
    fg: Color,
    bg: Color,
}

impl Sgr {
    /// `ESC[` と `m` の間のパラメータ（`1;31` / `38;5;196` / 空）を畳む。
    ///
    /// コロンの下位パラメータも読む: `4:0`（下線オフ）/ `4:3`（波下線: 下線オン）/
    /// `38:5:n` / `38:2::r:g:b`（ITU T.416・色空間の欄が空）/ `38:2:r:g:b`（欄を省く端末）。
    /// 重要: `:` の後を捨てると、`4:0` が**下線オン**に化ける（kitty / neovim が出す綴り）。
    fn apply(&mut self, params: &[u8]) {
        fn num(p: &[u8]) -> u16 {
            std::str::from_utf8(p)
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0)
        }
        // 各パラメータは (番号, コロンの下位パラメータ)。`38:2::1:2:3` の空欄は 0 で持つ。
        let items: Vec<(u16, Vec<u16>)> = params
            .split(|&b| b == b';')
            .map(|p| {
                let mut it = p.split(|&b| b == b':');
                let n = num(it.next().unwrap_or(&[]));
                (n, it.map(num).collect())
            })
            .collect();
        let mut i = 0;
        while i < items.len() {
            let (n, sub) = items[i].clone();
            match n {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                // `4:0` は下線オフ、`4` / `4:1..` は（種類を問わず）下線オン。21 は二重下線。
                4 => self.underline = sub.first().is_none_or(|&k| k != 0),
                21 => self.underline = true,
                5 | 6 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strike = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strike = false,
                30..=37 => self.fg = Color::Indexed((n - 30) as u8),
                39 => self.fg = Color::Default,
                40..=47 => self.bg = Color::Indexed((n - 40) as u8),
                49 => self.bg = Color::Default,
                90..=97 => self.fg = Color::Indexed((n - 90 + 8) as u8),
                100..=107 => self.bg = Color::Indexed((n - 100 + 8) as u8),
                38 | 48 | 58 => {
                    // 拡張色。コロン形式なら引数は同じパラメータの中、`;` 形式なら続くパラメータ。
                    // 58（下線の色）はどちらの形でも読み飛ばすだけ。
                    let (color, used) = if !sub.is_empty() {
                        (Self::extended_color(&sub, true), 0)
                    } else {
                        let rest: Vec<u16> = items[i + 1..].iter().map(|(k, _)| *k).collect();
                        let used = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 0,
                        };
                        (Self::extended_color(&rest, false), used)
                    };
                    if let Some(c) = color {
                        match n {
                            38 => self.fg = c,
                            48 => self.bg = c,
                            _ => {}
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// `5;n` / `2;r;g;b` の引数を色にする。`colon` はコロン形式（`2::r:g:b` の色空間の欄を許す）。
    fn extended_color(args: &[u16], colon: bool) -> Option<Color> {
        let clamp = |v: u16| v.min(255) as u8;
        match args.first() {
            Some(5) => args.get(1).map(|&v| Color::Indexed(clamp(v))),
            Some(2) => {
                // T.416 は `2:<色空間>:r:g:b`（5 欄）。欄を省く端末は `2:r:g:b`（4 欄）。`;` 形式は常に 4。
                let rgb = if colon && args.len() >= 5 {
                    &args[2..5]
                } else {
                    args.get(1..4)?
                };
                match rgb {
                    [r, g, b] => Some(Color::Rgb(clamp(*r), clamp(*g), clamp(*b))),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// 端末へそのまま書ける 1 本の `ESC[…m`。何も効いていなければ空。
    fn sequence(&self) -> String {
        let mut p: Vec<String> = Vec::new();
        let flags = [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverse, "7"),
            (self.hidden, "8"),
            (self.strike, "9"),
        ];
        for (on, code) in flags {
            if on {
                p.push(code.to_string());
            }
        }
        for (color, base, ext) in [(self.fg, 30u8, 38u8), (self.bg, 40, 48)] {
            match color {
                Color::Default => {}
                Color::Indexed(n @ 0..=7) => p.push((base + n).to_string()),
                Color::Indexed(n @ 8..=15) => p.push((base + 60 + n - 8).to_string()),
                Color::Indexed(n) => p.push(format!("{ext};5;{n}")),
                Color::Rgb(r, g, b) => p.push(format!("{ext};2;{r};{g};{b}")),
            }
        }
        if p.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", p.join(";"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Normal,
    /// UTF-8 の多バイト文字の途中（残りの継続バイト数）。
    Utf8(u8),
    Esc,
    /// `ESC x`（`x` は 0x20–0x2F の中間バイト）の最終バイト待ち（`ESC ( 0` など）。
    EscIntermediate,
    Csi,
    Osc,
    OscEsc,
    /// DCS / SOS / PM / APC: ST（`ESC \`）まで読み飛ばす。
    Str,
    StrEsc,
}

/// 捨てるバイトを順に食って、残った状態を持つ。
#[derive(Debug, Clone, Default)]
pub struct HeadTracker {
    state: State,
    /// CSI のパラメータと中間バイト（`ESC[` の後・最終バイトの手前）。
    params: Vec<u8>,
    /// OSC の本体（`ESC]` の後・終端の手前）。
    osc: Vec<u8>,
    /// 溜める上限を越えた（読むが解釈しない）。
    overflow: bool,
    /// alt 画面に居るか。注意: 入れ子にしない。実端末（xterm）の 1049/1047/47 はオンかオフの 2 値で、
    /// 落ちたアプリが残した `h` の上で次のアプリが出入りすれば通常の画面に戻る。
    alt_screen: bool,
    sgr: Sgr,
    title: Option<String>,
    /// 1 バイトでも食ったか（食っていなければ渡す状態は無い）。
    dropped: bool,
    /// 最後に食ったバイトが行の終わり（`\n`・命令の外）だったか。
    line_start: bool,
}

impl HeadTracker {
    /// CSI のパラメータの上限。
    const PARAMS_CAP: usize = 64;
    /// OSC の本体の上限（OSC 52 の貼り付けは何 KB にもなる）。
    const OSC_CAP: usize = 8 * 1024;
    /// 題名として持つ上限（文字数）。
    const TITLE_CHARS: usize = 256;

    /// 捨てるバイトを 1 つ食う。
    pub fn feed(&mut self, b: u8) {
        self.dropped = true;
        self.line_start = false;
        match self.state {
            State::Normal => self.normal(b),
            State::Utf8(n) => {
                if (0x80..=0xBF).contains(&b) {
                    self.state = if n <= 1 {
                        State::Normal
                    } else {
                        State::Utf8(n - 1)
                    };
                } else {
                    // 壊れた列。このバイトを頭から読み直す。
                    self.state = State::Normal;
                    self.normal(b);
                }
            }
            State::Esc => match b {
                b'[' => {
                    self.state = State::Csi;
                    self.params.clear();
                    self.overflow = false;
                }
                b']' => {
                    self.state = State::Osc;
                    self.osc.clear();
                    self.overflow = false;
                }
                b'P' | b'X' | b'^' | b'_' => self.state = State::Str,
                // RIS（`reset` が出す）。端末の状態を全部初期化する。
                b'c' => {
                    self.alt_screen = false;
                    self.sgr = Sgr::default();
                    self.title = None;
                    self.state = State::Normal;
                }
                0x1b => {}
                0x20..=0x2F => self.state = State::EscIntermediate,
                _ => self.state = State::Normal,
            },
            State::EscIntermediate => match b {
                0x20..=0x2F => {}
                _ => self.state = State::Normal,
            },
            State::Csi => match b {
                0x20..=0x3F => {
                    if self.params.len() < Self::PARAMS_CAP {
                        self.params.push(b);
                    } else {
                        self.overflow = true;
                    }
                }
                0x40..=0x7E => {
                    self.state = State::Normal;
                    if !self.overflow {
                        self.finish_csi(b);
                    }
                }
                0x1b => self.state = State::Esc,
                0x18 | 0x1a => self.state = State::Normal,
                _ => {} // 実行される C0（BEL・BS …）。CSI は続く
            },
            State::Osc => match b {
                0x07 => self.finish_osc(),
                0x1b => self.state = State::OscEsc,
                0x18 | 0x1a => self.state = State::Normal,
                _ => {
                    if self.osc.len() < Self::OSC_CAP {
                        self.osc.push(b);
                    } else {
                        self.overflow = true;
                    }
                }
            },
            State::OscEsc => {
                // ST（`ESC \`）で終わる。他の ESC も終端として扱い、そのバイトを ESC の続きとして読む。
                self.finish_osc();
                if b != b'\\' {
                    self.state = State::Esc;
                    self.feed_after_esc(b);
                }
            }
            State::Str => match b {
                0x1b => self.state = State::StrEsc,
                0x18 | 0x1a => self.state = State::Normal,
                _ => {}
            },
            State::StrEsc => {
                self.state = State::Normal;
                if b != b'\\' {
                    self.state = State::Esc;
                    self.feed_after_esc(b);
                }
            }
        }
    }

    /// `ESC` の直後のバイトとして読む（終端として ESC を見たあとの続き）。`dropped` / `line_start` は触らない。
    fn feed_after_esc(&mut self, b: u8) {
        let was_dropped = self.dropped;
        self.feed(b);
        self.dropped = was_dropped;
    }

    fn normal(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Esc,
            b'\n' => self.line_start = true,
            0xC0..=0xDF => self.state = State::Utf8(1),
            0xE0..=0xEF => self.state = State::Utf8(2),
            0xF0..=0xF7 => self.state = State::Utf8(3),
            _ => {}
        }
    }

    fn finish_csi(&mut self, final_byte: u8) {
        match final_byte {
            b'm' => {
                // 中間バイトや `?` / `>` の付いた物は SGR ではない。
                if self
                    .params
                    .iter()
                    .all(|&c| c.is_ascii_digit() || c == b';' || c == b':')
                {
                    let params = std::mem::take(&mut self.params);
                    self.sgr.apply(&params);
                    self.params = params;
                }
            }
            b'h' | b'l' => {
                if let Some(rest) = self.params.strip_prefix(b"?") {
                    let alt = rest
                        .split(|&c| c == b';')
                        .any(|p| matches!(p, b"1049" | b"1047" | b"47"));
                    if alt {
                        self.alt_screen = final_byte == b'h';
                    }
                }
            }
            // DECSTR（`CSI ! p`・ソフトリセット）。色は戻る。alt 画面と題名は触らない。
            b'p' if self.params == b"!" => self.sgr = Sgr::default(),
            _ => {}
        }
    }

    fn finish_osc(&mut self) {
        self.state = State::Normal;
        if self.overflow {
            return;
        }
        let Some(sep) = self.osc.iter().position(|&c| c == b';') else {
            return;
        };
        if !matches!(&self.osc[..sep], b"0" | b"2") {
            return;
        }
        let text = String::from_utf8_lossy(&self.osc[sep + 1..]);
        let text: String = text.trim().chars().take(Self::TITLE_CHARS).collect();
        self.title = if text.is_empty() { None } else { Some(text) };
    }

    /// 最後に食ったバイトが行の終わりだった（次のバイトは行頭）。
    pub fn at_line_start(&self) -> bool {
        self.line_start
    }

    /// 命令の外・文字の外（次のバイトから切っても頭にゴミが出ない）。
    pub fn at_boundary(&self) -> bool {
        self.state == State::Normal
    }

    /// 捨てた分が残した状態。1 バイトも捨てていなければ `None`。
    pub fn head(&self) -> Option<ReplayHead> {
        self.dropped.then(|| ReplayHead {
            alt_screen: self.alt_screen,
            sgr: self.sgr.sequence(),
            title: self.title.clone(),
        })
    }
}

/// client 側: 捨てた分が残した状態になる命令を**作り直す**（再生の頭に付ける）。
///
/// alt screen の中なら `?1049h`（描き直しのエミュレータが alt に入るため履歴を出さない）、色と装飾、題名、
/// そして**カーソルを最下行へ**（改行 × `rows − 1`）。
///
/// 最後の 1 つは、頭が切れていること自体が失わせる前提。生きていた端末では出力は**最下行でスクロール**していた。
/// ConPTY は長い行を折り返すとき `ESC[29;119H` のような**絶対位置**（最下行の 1 つ上）を挟むので、エミュレータが
/// 1 行目から始めると、頭の行の直後に画面の高さ近い空白が開く（30 行の端末で 27 行）。最下行から
/// 始めれば ConPTY の絶対位置と噛み合う。unix の出力は `\n` だけなので、どちらから始めても同じ。
/// 注意: 改行で押し出された空の行が履歴の頭に積まれる。呼ぶ側（`berth attach`）が描く前に剥がす。
pub fn head_prefix(head: &ReplayHead, rows: u16) -> Vec<u8> {
    let mut out = Vec::new();
    if head.alt_screen {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(head.sgr.as_bytes());
    if let Some(t) = &head.title {
        out.extend_from_slice(b"\x1b]2;");
        out.extend_from_slice(t.as_bytes());
        out.push(0x07);
    }
    for _ in 1..rows {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// `head_prefix` の改行で履歴の頭に積まれた**空の行**を剥がす（描き直しの `scrollback` の先頭だけ）。
/// 頭が切れているときしか呼ばない。切れた頭の手前の空行に意味は無い。
pub fn strip_leading_blank_rows(scrollback: &mut Vec<u8>) {
    let mut n = 0;
    while scrollback[n..].starts_with(b"\r\n") {
        n += 2;
    }
    scrollback.drain(..n);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(bytes: &[u8]) -> HeadTracker {
        let mut t = HeadTracker::default();
        for &b in bytes {
            t.feed(b);
        }
        t
    }

    #[test]
    fn nothing_dropped_means_no_head() {
        assert_eq!(HeadTracker::default().head(), None);
        assert_eq!(
            fed(b"plain\n").head(),
            Some(ReplayHead::default()),
            "plain text leaves no state but the head exists"
        );
    }

    #[test]
    fn the_colour_in_force_is_folded_into_one_sequence() {
        assert_eq!(fed(b"\x1b[31mred").head().unwrap().sgr, "\x1b[31m");
        assert_eq!(fed(b"\x1b[1m\x1b[31m").head().unwrap().sgr, "\x1b[1;31m");
        assert_eq!(fed(b"\x1b[1;31m\x1b[39m").head().unwrap().sgr, "\x1b[1m");
        assert_eq!(fed(b"\x1b[1;31m\x1b[0m").head().unwrap().sgr, "");
        assert_eq!(fed(b"\x1b[1;31m\x1b[m").head().unwrap().sgr, "");
        assert_eq!(fed(b"\x1b[38;5;196m").head().unwrap().sgr, "\x1b[38;5;196m");
        assert_eq!(
            fed(b"\x1b[48;2;10;20;30m\x1b[93m").head().unwrap().sgr,
            "\x1b[93;48;2;10;20;30m"
        );
        assert_eq!(fed(b"\x1b[1;2m\x1b[22m").head().unwrap().sgr, "");
        assert_eq!(fed(b"\x1b[4:3m").head().unwrap().sgr, "\x1b[4m");
        // コロンの下位パラメータを読む。`4:0` は下線**オフ**、拡張色はコロン形式でも色。
        assert_eq!(fed(b"\x1b[4m\x1b[4:0m").head().unwrap().sgr, "");
        assert_eq!(fed(b"\x1b[4:0m").head().unwrap().sgr, "");
        assert_eq!(fed(b"\x1b[38:5:196m").head().unwrap().sgr, "\x1b[38;5;196m");
        assert_eq!(
            fed(b"\x1b[38:2::10:20:30m").head().unwrap().sgr,
            "\x1b[38;2;10;20;30m"
        );
        assert_eq!(
            fed(b"\x1b[48:2:10:20:30m").head().unwrap().sgr,
            "\x1b[48;2;10;20;30m"
        );
        // 下線の色（58）はコロン形式でも読み飛ばすだけ（続く 1 は太字として効く）。
        assert_eq!(fed(b"\x1b[58:5:1;1m").head().unwrap().sgr, "\x1b[1m");
        assert_eq!(fed(b"\x1b[58;2;1;2;3;1m").head().unwrap().sgr, "\x1b[1m");
        // SGR ではない `m`（中間バイト付き）は読まない。
        assert_eq!(fed(b"\x1b[?31m").head().unwrap().sgr, "");
    }

    #[test]
    fn the_alt_screen_is_on_or_off_not_nested() {
        assert!(fed(b"\x1b[?1049h").head().unwrap().alt_screen);
        assert!(!fed(b"\x1b[?1049h\x1b[?1049l").head().unwrap().alt_screen);
        // 実端末は入れ子にしない。落ちたアプリが残した `h` の上で次のアプリが入って出たら、通常の画面。
        assert!(
            !fed(b"\x1b[?1049h\x1b[?1049h\x1b[?1049l")
                .head()
                .unwrap()
                .alt_screen
        );
        assert!(
            !fed(b"\x1b[?1049l").head().unwrap().alt_screen,
            "a lone leave is swallowed"
        );
        assert!(fed(b"\x1b[?1h\x1b[?47h").head().unwrap().alt_screen);
        assert!(!fed(b"\x1b[?25l").head().unwrap().alt_screen);
    }

    #[test]
    fn a_full_reset_forgets_what_came_before() {
        // `reset`（RIS: `ESC c`）は alt 画面・色・題名を全部初期化する。
        let h = fed(b"\x1b[?1049h\x1b[1;31m\x1b]2;vim\x07\x1bc")
            .head()
            .unwrap();
        assert!(!h.alt_screen);
        assert_eq!(h.sgr, "");
        assert_eq!(h.title, None);
        // DECSTR（`CSI ! p`）は色だけ戻す（alt 画面と題名は触らない）。
        let h = fed(b"\x1b[?1049h\x1b[1;31m\x1b[!p").head().unwrap();
        assert!(h.alt_screen);
        assert_eq!(h.sgr, "");
    }

    #[test]
    fn the_title_is_the_last_osc_0_or_2() {
        assert_eq!(
            fed(b"\x1b]0;first\x07\x1b]2;second\x1b\\")
                .head()
                .unwrap()
                .title
                .as_deref(),
            Some("second")
        );
        assert_eq!(
            fed(b"\x1b]2;vim\x07\x1b]2;\x07").head().unwrap().title,
            None
        );
        assert_eq!(fed(b"\x1b]52;c;aGVsbG8=\x07").head().unwrap().title, None);
        // 終端が ESC の別の命令: 題名は閉じ、続く命令も読める。
        let t = fed(b"\x1b]2;vim\x1b[31m");
        assert_eq!(t.head().unwrap().title.as_deref(), Some("vim"));
        assert_eq!(t.head().unwrap().sgr, "\x1b[31m");
    }

    #[test]
    fn a_sequence_split_anywhere_still_lands_in_the_state() {
        let bytes = b"text\x1b[1;3\x1b]2;vi";
        let mut t = fed(bytes);
        assert!(!t.at_boundary(), "inside an OSC");
        for &b in b"m\x07" {
            t.feed(b);
        }
        // `ESC[1;3` は `ESC]` で捨てられた（終端なし）。OSC は閉じた。
        assert_eq!(t.head().unwrap().title.as_deref(), Some("vim"));
        assert_eq!(t.head().unwrap().sgr, "");
    }

    #[test]
    fn boundaries_fall_outside_sequences_and_characters() {
        assert!(fed(b"abc").at_boundary());
        assert!(!fed(b"abc\x1b[3").at_boundary());
        assert!(!fed(b"abc\x1b").at_boundary());
        assert!(fed(b"abc\x1b[31m").at_boundary());
        let mut t = fed("あ".as_bytes());
        assert!(t.at_boundary());
        t = fed(&"あ".as_bytes()[..2]);
        assert!(!t.at_boundary(), "inside a 3-byte character");
        assert!(fed(b"line\n").at_line_start());
        assert!(!fed(b"line\n ").at_line_start());
        assert!(
            !fed(b"\x1b]2;a\nb").at_line_start(),
            "a newline inside an OSC is not a line end"
        );
    }

    #[test]
    fn an_oversized_osc_is_skipped_without_losing_the_rest() {
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', HeadTracker::OSC_CAP + 10));
        big.extend_from_slice(b"\x07\x1b[31m");
        let t = fed(&big);
        assert!(t.at_boundary());
        assert_eq!(t.head().unwrap().sgr, "\x1b[31m");
    }

    #[test]
    fn the_prefix_rebuilds_the_state_in_the_order_the_emulator_needs() {
        let h = ReplayHead {
            alt_screen: true,
            sgr: "\x1b[1;31m".into(),
            title: Some("vim".into()),
        };
        assert_eq!(
            head_prefix(&h, 3),
            b"\x1b[?1049h\x1b[1;31m\x1b]2;vim\x07\r\n\r\n"
        );
        assert!(head_prefix(&ReplayHead::default(), 1).is_empty());
        assert_eq!(head_prefix(&ReplayHead::default(), 2), b"\r\n");
    }

    #[test]
    fn the_blank_rows_the_prefix_pushed_into_history_are_stripped() {
        let mut s = b"\r\n\r\nline 1\r\n\r\nline 2\r\n".to_vec();
        strip_leading_blank_rows(&mut s);
        assert_eq!(s, b"line 1\r\n\r\nline 2\r\n");
        let mut empty = Vec::new();
        strip_leading_blank_rows(&mut empty);
        assert!(empty.is_empty());
    }
}
