//! berth-screen — 入り直す瞬間に 1 回だけ、セッションの録画（replay）を端末エミュレータに通し、
//! 端末自身のスクロールへ積める綺麗な行に描き直す。
//!
//! # なぜ別 crate か
//!
//! berthd（デーモン）は画面の模型を持たない。画面を持たないから、どの端末のどのペインの
//! 中にでも入れる。ここはその決めを変えない: エミュレータを回すのはクライアント（`berth attach`）が
//! 入り直す瞬間の 1 回だけで、デーモンは生バイトの録画のまま。`--no-scrollback` の人は
//! この crate を一度も通らない。
//!
//! # 何を返すか
//!
//! 録画を `cols × rows` の端末として最後まで再生したあとの、
//! - **スクロールに積む分**（`scrollback`）= 画面に入らない行（履歴）
//! - **今の画面**（`screen`）= 表示中の `rows` 行ぜんぶ（空行も。端末の画面とエミュレータの viewport の
//!   行番号を一致させるため。全画面アプリと行エディタの絶対位置がそのまま効く）
//! - カーソルの位置と、中のアプリが alt screen に居るか
//!
//! 重要: 同じ行を両方に出さない（二重描きになる）。履歴は `Line(0)` の手前まで、画面は `Line(0)` から。
//!
//! 重要: alt screen の中なら履歴は出さない。vim の格子がスクロールに崩れて積まれるのが
//! 「画面が壊れて見える」のいちばん目立つ形。そのときは今の画面（alt 側の格子）だけ。
//!
//! 注意: 色・属性は「変わったときだけ `ESC[0m` ＋ 全部」の単純形。行末で必ず戻す。

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, LineLength};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

/// 再生の結果。バイト列は端末へそのまま書ける（`scrollback` → `screen` の順）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// 画面に入らない行（履歴）。各行は `\r\n` で終わる。ただし端末が折った行（soft wrap）は
    /// 改行を付けない（同じ幅の端末が自分で折るため、元の 1 行として選択・コピーできる）。
    pub scrollback: Vec<u8>,
    /// 表示中の `rows` 行。最後の行だけ改行無し。
    pub screen: Vec<u8>,
    /// カーソル（viewport 内の 0 始まりの行, 0 始まりの桁）。
    pub cursor: (u16, u16),
    /// カーソルの居る行が文字で占めている桁数（末尾の空白は数えない）。
    /// `berth attach` が合図（`[attached to …]`）をその行の右端に置けるかを、これとカーソルの桁で決める。
    pub cursor_row_len: u16,
    /// 中のアプリが alt screen に居る（`scrollback` は空・`screen` は alt 側の格子）。
    pub in_alt_screen: bool,
    /// 中のアプリが最後に名乗っていた題名（OSC 0 / 2）。`None` は「名乗っていない」か「戻した」。
    /// `berth attach` が端末の題名に `berth: <名前> · <これ>` と出す（録画の中の題名は再生では消えるので）。
    pub title: Option<String>,
}

/// 題名だけ拾う聞き手（他の出来事は捨てる・`VoidListener` と同じ）。`Term` は聞き手を返してくれないので、
/// 外と共有する箱（`Rc<RefCell>`）を持つ。
struct TitleListener(std::rc::Rc<std::cell::RefCell<Option<String>>>);

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        match event {
            // 空の題名は「戻した」と同じ扱い（alacritty は空でも `Title("")` を上げる）。
            Event::Title(t) => {
                *self.0.borrow_mut() = if t.trim().is_empty() { None } else { Some(t) }
            }
            Event::ResetTitle => *self.0.borrow_mut() = None,
            _ => {}
        }
    }
}

/// 履歴の上限（行）。録画は最大 512KB（berthd の `REPLAY_CAP`）なので、1 行 5 バイトでも 10 万行に届かない。
const HISTORY_LINES: usize = 100_000;

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// `replay` を `cols × rows` の端末として再生し、描き直した行を返す。
///
/// 空の録画（新しいセッション）は空を返す。呼ぶ側は何も書かない（バナーだけ、すなわち今までの見え方）。
pub fn render(replay: &[u8], cols: u16, rows: u16) -> Rendered {
    if replay.is_empty() {
        return Rendered::default();
    }
    // alacritty の下限（幅 2 は全角 1 文字のため・高さ 1）。
    let size = Size {
        cols: (cols as usize).max(2),
        rows: (rows as usize).max(1),
    };
    let config = Config {
        scrolling_history: HISTORY_LINES,
        ..Config::default()
    };
    let seen_title = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mut term = Term::new(config, &size, TitleListener(seen_title.clone()));
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, replay);

    let in_alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
    let title = seen_title.borrow().clone();
    let grid = term.grid();
    let mut sgr = Sgr::default();

    let mut scrollback = Vec::new();
    if !in_alt_screen {
        let history = grid.history_size();
        for i in 0..history {
            let line = Line(i as i32 - history as i32);
            emit_row(&grid[line], &mut sgr, &mut scrollback, true);
        }
    }

    let mut screen = Vec::new();
    for i in 0..size.rows {
        let last = i + 1 == size.rows;
        emit_row(&grid[Line(i as i32)], &mut sgr, &mut screen, !last);
    }

    let cursor = cursor_of(grid, &size);
    let cursor_row_len = occupied_len(&grid[Line(i32::from(cursor.0))]) as u16;
    Rendered {
        scrollback,
        screen,
        cursor,
        in_alt_screen,
        cursor_row_len,
        title,
    }
}

fn cursor_of(grid: &Grid<Cell>, size: &Size) -> (u16, u16) {
    let p = grid.cursor.point;
    let row = p.line.0.clamp(0, size.rows as i32 - 1) as u16;
    let col = p.column.0.min(size.cols - 1) as u16;
    (row, col)
}

/// 行の中で見た目のある最後の桁まで（その次の桁の番号）。
///
/// alacritty の `line_length` は「空白でない最後の文字」で測るので、背景色の付いた空白や
/// 下線・反転・取り消し線の付いた空白を数えない。そのまま使うと `ESC[41m` で塗った行末が
/// 戻した画面から消える。
fn occupied_len(row: &Row<Cell>) -> usize {
    let len = row.line_length().0;
    let visible_blank = |cell: &Cell| {
        cell.bg != Color::Named(NamedColor::Background)
            || cell
                .flags
                .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
    };
    (len..row.len())
        .rev()
        .find(|&col| visible_blank(&row[Column(col)]))
        .map_or(len, |col| col + 1)
}

/// 1 行を書く。占有している桁まで（末尾の空白は切る）、属性は変わったときだけ、行末で戻す。
/// `newline` が偽か、端末が折った行（`WRAPLINE`）なら改行を付けない。
fn emit_row(row: &Row<Cell>, sgr: &mut Sgr, out: &mut Vec<u8>, newline: bool) {
    let len = occupied_len(row);
    let mut buf = [0u8; 4];
    for col in 0..len {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        sgr.apply(cell, out);
        out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
        if let Some(zw) = cell.zerowidth() {
            for &c in zw {
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    sgr.reset(out);
    let wrapped = len > 0 && row[Column(len - 1)].flags.contains(Flags::WRAPLINE);
    if newline && !wrapped {
        out.extend_from_slice(b"\r\n");
    }
}

/// いま端末に効いている属性（こちらが書いた分）。
#[derive(Clone, PartialEq, Eq)]
struct Sgr {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Default for Sgr {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl Sgr {
    /// 端末へ出す属性だけ（`WRAPLINE` や spacer は見た目ではない）。
    const MASK: Flags = Flags::BOLD
        .union(Flags::DIM)
        .union(Flags::ITALIC)
        .union(Flags::ALL_UNDERLINES)
        .union(Flags::INVERSE)
        .union(Flags::HIDDEN)
        .union(Flags::STRIKEOUT);

    fn apply(&mut self, cell: &Cell, out: &mut Vec<u8>) {
        let want = Sgr {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & Self::MASK,
        };
        if want == *self {
            return;
        }
        *self = want;
        // いつも `0`（全部戻す）から組み直す（差分を考えない。属性の組が少ないので十分短い）。
        let mut seq = String::from("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::ALL_UNDERLINES, "4"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if self.flags.intersects(flag) {
                seq.push(';');
                seq.push_str(code);
            }
        }
        if let Some(p) = color_params(self.fg, true) {
            seq.push(';');
            seq.push_str(&p);
        }
        if let Some(p) = color_params(self.bg, false) {
            seq.push(';');
            seq.push_str(&p);
        }
        seq.push('m');
        out.extend_from_slice(seq.as_bytes());
    }

    fn reset(&mut self, out: &mut Vec<u8>) {
        if *self != Sgr::default() {
            *self = Sgr::default();
            out.extend_from_slice(b"\x1b[0m");
        }
    }
}

/// SGR の色の引数。既定色は `None`（`0` で戻した直後なので何も要らない）。
fn color_params(c: Color, fg: bool) -> Option<String> {
    let (normal, bright) = if fg { (30, 90) } else { (40, 100) };
    match c {
        Color::Named(n) => {
            let idx = n as usize;
            match idx {
                0..=7 => Some((normal + idx).to_string()),
                8..=15 => Some((bright + idx - 8).to_string()),
                // DimBlack..DimWhite: 暗さは `DIM` の旗が運ぶので、色は素の 8 色。
                259..=266 => Some((normal + idx - 259).to_string()),
                // Foreground / Background / Cursor / BrightForeground / DimForeground: 既定色。
                _ => None,
            }
        }
        Color::Indexed(i) => Some(format!("{};5;{i}", if fg { 38 } else { 48 })),
        Color::Spec(rgb) => Some(format!(
            "{};2;{};{};{}",
            if fg { 38 } else { 48 },
            rgb.r,
            rgb.g,
            rgb.b
        )),
    }
}

/// 端末の制御列を落として文字だけにする（検体と物差し用・色は見ない）。
pub fn plain_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('[') => {
                for d in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(d) = it.next() {
                    if d == '\u{7}' {
                        break;
                    }
                    if d == '\u{1b}' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(bytes: &[u8]) -> Vec<String> {
        plain_text(bytes)
            .split("\r\n")
            .map(str::to_string)
            .collect()
    }

    fn numbered(n: usize) -> Vec<u8> {
        (1..=n)
            .map(|i| format!("{i}\r\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn an_empty_replay_renders_nothing() {
        assert_eq!(render(b"", 80, 24), Rendered::default());
    }

    /// 300 行を 24 行の端末に通す: 画面に入らない分が履歴、残りが画面（同じ行は 1 回だけ）。
    #[test]
    fn history_and_screen_split_the_lines_without_overlap() {
        let r = render(&numbered(300), 80, 24);
        assert!(!r.in_alt_screen);
        // 300 行の後にカーソルの空行が 1 つ（計 301 行）。画面 24 行 → 履歴 277 行。
        let sb = lines(&r.scrollback);
        assert_eq!(
            sb.last().map(String::as_str),
            Some(""),
            "履歴は \\r\\n で終わる"
        );
        let sb: Vec<_> = sb[..sb.len() - 1].to_vec();
        assert_eq!(sb.len(), 277, "履歴の行数: {sb:?}");
        assert_eq!(sb.first().map(String::as_str), Some("1"));
        assert_eq!(sb.last().map(String::as_str), Some("277"));
        let sc = lines(&r.screen);
        assert_eq!(sc.len(), 24, "画面は rows 行ぜんぶ: {sc:?}");
        assert_eq!(sc[0], "278");
        assert_eq!(sc[22], "300");
        assert_eq!(sc[23], "", "最後の行はカーソルの空行");
        assert_eq!(r.cursor, (23, 0));
        assert_eq!(r.cursor_row_len, 0, "カーソル行は空行");
        // 1..=300 が履歴と画面を合わせてちょうど 1 回ずつ。
        let mut all: Vec<usize> = sb
            .iter()
            .chain(sc.iter())
            .filter_map(|s| s.parse().ok())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (1..=300).collect::<Vec<_>>());
    }

    /// `\r` で書き換える進捗バーは最後の 1 行だけが残る。
    #[test]
    fn a_progress_bar_leaves_only_its_last_state() {
        let r = render(b"step 1\rstep 2\rstep 3\r\ndone\r\n", 80, 24);
        let sc = lines(&r.screen);
        assert_eq!(sc[0], "step 3");
        assert_eq!(sc[1], "done");
        // `done\r\n` の後のカーソルは 3 行目の空行。
        assert_eq!(r.cursor_row_len, 0);
        assert_eq!(r.cursor, (2, 0));
        assert!(!plain_text(&r.screen).contains("step 1"));
    }

    /// 色は行末で戻し、次の行で付け直す（行ごとに独立に貼れる）。
    #[test]
    fn colour_is_reset_at_each_line_end_and_reapplied() {
        let r = render(b"\x1b[31mred1\r\nred2\x1b[0m\r\nplain\r\n", 80, 24);
        let s = String::from_utf8_lossy(&r.screen);
        assert!(s.contains("\x1b[0;31mred1\x1b[0m\r\n"), "{s:?}");
        assert!(s.contains("\x1b[0;31mred2\x1b[0m\r\n"), "{s:?}");
        assert!(s.contains("\r\nplain\r\n"), "{s:?}");
    }

    /// 全角と絵文字: spacer のセルは出さない（文字がそのまま 1 回ずつ）。
    #[test]
    fn wide_characters_are_emitted_once_without_their_spacers() {
        let r = render("日本語😀 ok\r\n".as_bytes(), 80, 24);
        let sc = lines(&r.screen);
        assert_eq!(sc[0], "日本語😀 ok");
    }

    /// 幅を超える行は端末が折る（折れ目に改行を入れない、元の 1 行として戻る）。
    #[test]
    fn a_soft_wrapped_line_gets_no_newline_at_the_fold() {
        let r = render(b"abcdefghijklmnop\r\nq\r\n", 10, 24);
        let s = plain_text(&r.screen);
        assert!(s.starts_with("abcdefghijklmnop\r\nq\r\n"), "{s:?}");
    }

    /// 背景色の付いた行末の空白は「見えている」ので切らない（色の付かない空白は今までどおり切る）。
    #[test]
    fn a_coloured_background_at_the_line_end_is_kept() {
        let r = render(b"\x1b[41mred   \x1b[0m\r\nplain   \r\n", 80, 24);
        let s = String::from_utf8_lossy(&r.screen);
        assert!(s.contains("red   \x1b[0m"), "塗った空白が消えた: {s:?}");
        assert!(s.contains("plain\r\n"), "色の無い空白まで残った: {s:?}");
    }

    /// 全画面アプリに入って出た分は、履歴にも画面にも残らない。
    #[test]
    fn what_happened_inside_the_alt_screen_leaves_no_trace() {
        let r = render(b"before\r\n\x1b[?1049hINSIDE\x1b[?1049l", 80, 24);
        assert!(!r.in_alt_screen);
        let all = plain_text(&r.scrollback) + &plain_text(&r.screen);
        assert!(all.contains("before"), "{all:?}");
        assert!(!all.contains("INSIDE"), "{all:?}");
    }

    /// 中に居たままなら: 履歴は出さず、画面は alt 側の格子。
    #[test]
    fn inside_the_alt_screen_only_the_screen_is_rendered() {
        let r = render(b"a\r\nb\r\nc\r\nd\r\n\x1b[?1049h\x1b[HINSIDE", 80, 2);
        assert!(r.in_alt_screen);
        assert!(r.scrollback.is_empty(), "{:?}", plain_text(&r.scrollback));
        let s = plain_text(&r.screen);
        assert!(s.contains("INSIDE"), "{s:?}");
        assert!(!s.contains('a'), "{s:?}");
    }

    /// 画面消去の後、消した中身は画面に無い。
    #[test]
    fn a_cleared_screen_does_not_show_what_was_cleared() {
        let r = render(b"old\r\n\x1b[2J\x1b[Hnew", 80, 24);
        let s = plain_text(&r.screen);
        assert!(s.contains("new"), "{s:?}");
        assert!(!s.contains("old"), "{s:?}");
        assert_eq!(r.cursor, (0, 3));
        assert_eq!(r.cursor_row_len, 3, "カーソル行の文字は `new` の 3 桁");
    }

    /// 中のアプリが名乗った題名（OSC 0 / 2）は最後の物を拾う。戻せば `None`。
    #[test]
    fn the_last_title_the_app_set_is_reported() {
        let r = render(b"\x1b]2;first\x07x\x1b]0;Claude Code\x1b\\y\r\n", 80, 24);
        assert_eq!(r.title.as_deref(), Some("Claude Code"));
        assert_eq!(lines(&r.screen)[0], "xy");
        let r = render(b"\x1b]2;first\x07\x1b]2;\x07", 80, 24);
        assert_eq!(r.title, None, "空の題名は戻したの意味");
        assert_eq!(render(b"plain\r\n", 80, 24).title, None);
    }

    /// 端末が答えを返す問い（DSR）は録画に残っていても何も起こさない（聞き手が捨てる）。
    #[test]
    fn a_query_in_the_replay_is_inert() {
        let r = render(b"x\x1b[6n\x1b[cy\r\n", 80, 24);
        assert_eq!(lines(&r.screen)[0], "xy");
    }
}
