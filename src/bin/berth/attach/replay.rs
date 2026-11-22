/// 同じ大きさの再接続で、ずらした大きさから戻すまでの**最長の**待ち。
/// ふつうはシェルの描き直し（次のセッション出力）を見た時点で戻す。
pub(super) const NUDGE_RESTORE_AFTER: std::time::Duration = std::time::Duration::from_millis(300);
/// 新しく作成したセッションの合図を置く「最初のプロンプトが描画完了した」目安。出力がこれだけ停止し、
/// カーソルが文字の在る行の途中に居る（プロンプトの形）。
pub(super) const FRESH_QUIET: std::time::Duration = std::time::Duration::from_millis(250);
/// それでもプロンプトの形にならなければ合図を諦める（題名だけが名乗る）。
pub(super) const FRESH_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(5);
/// 入り直しで画面の頭に空の行が在るとき、端末に今のカーソル行を訊く（DSR・`ESC[6n`）。
/// 答えが来なければこの時間で諦め、画面 rows 行をそのまま書く。
pub(super) const DSR_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
/// カーソル位置の問い（DSR）。答えは `ESC[<行>;<桁>R`（1 始まり）で stdin に来る。
pub(super) const DSR_REQUEST: &[u8] = b"\x1b[6n";
/// タイムアウト後に遅延して届いた DSR の応答を、入力から抜き取る期間。重要: 抜き取らないと `ESC[12;1R` がそのまま
/// 中のシェルへ入力として渡ってしまう（RTT が `DSR_WAIT` を超える ssh で発生する）。注意: 応答と同じ形式のキー
/// （Shift+F3、`ESC[1;2R`）を 1 回だけ消費する可能性があるため、期間は短く・抜き取りは 1 回のみ行う。
pub(super) const DSR_LATE: std::time::Duration = std::time::Duration::from_secs(2);
/// 合図のために見る最初の出力の上限。超えたら諦める（プロンプトを出さずに流し続けるシェル）。
pub(super) const FRESH_WATCH_CAP: usize = 256 * 1024;

/// 繋がっている間の、こちらの名乗り（題名の頭）。見る側はそう名乗る。
pub(super) fn title_of(handle: &str, read_only: bool) -> String {
    if read_only {
        format!("berth: {handle} (watching)")
    } else {
        format!("berth: {handle}")
    }
}

/// 端末の題名を設定する列（OSC 2・ST 終端）。`inner` は中のアプリが名乗っていた題名
/// （録画から取得したもの）。存在すれば `berth: <名前> · <中の題名>` となる。
pub(super) fn title_sequence(mine: &str, inner: Option<&str>) -> String {
    match inner.and_then(inner_title) {
        Some(t) => format!("\x1b]2;{mine} · {t}\x1b\\"),
        None => format!("\x1b]2;{mine}\x1b\\"),
    }
}

/// 中のアプリの題名として**採る**物だけ返す。
///
/// 実行ファイルのパスは名乗りではない: ConPTY は起動時に conhost の既定の題名（最初のプロセスの
/// exe のパス（`C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe`）を OSC 0 で出す。
/// PowerShell 5.1 は自分では題名を設定しないので、そのまま繋ぐとタブが
/// `berth: work · C:\…\powershell.exe` になる。自分で名乗るアプリはパスを出さないので、
/// `.exe` で終わる題名は捨てる。
pub(super) fn inner_title(raw: &str) -> Option<&str> {
    let t = raw.trim();
    if t.is_empty() || t.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }
    Some(t)
}

/// 抜けるときに題名を空にする（Windows Terminal はプロファイル名へ戻る・他の端末は空の題名）。
pub(super) const TITLE_CLEAR: &[u8] = b"\x1b]2;\x1b\\";

/// 描き直した録画を端末へ書き込むバイト列（履歴 → 画面 → **合図** → カーソル）。
///
/// 画面は `rows` 行ぜんぶ書く（最後の行は改行無し）ので、書き終えた瞬間のカーソルは**画面の最下行**に
/// 居る。そこから相対移動で戻す（`CSI n A` で上へ、`CSI n G` で桁指定）。絶対位置は使わない
/// （端末の画面のどこに viewport が載ったかを、こちらは知らなくてよい）。
/// 重要: ここは `render` の `screen` が rows 行ちょうどであることに依存している（空行も出力する理由）。
///
/// 合図（`[attached to …]`）は、右プロンプトのように**カーソル行の右端**へ右寄せで薄く（`SGR 2`）置く。
/// 復元した画面が端末を埋めるので、行を 1 本足すと ConPTY の絶対座標が 1 行ずれて壊れる。
/// カーソル行の右端なら縦に 1 行も増えず、シェルはその行の右側を空だと思っているので衝突せず、
/// シェルがその行を描き直した瞬間に自然に消える（消えることを前提とした合図であり、恒久的な案内ではない）。
/// 置けないとき（右側に文字が在る・入らない・カーソルに近すぎる）は出さない。
/// 録画が空なら案内を自分の行に出すだけ（新しいセッションと同じ見え方）。
fn replay_entrance(
    r: &berth_screen::Rendered,
    cols: u16,
    rows: u16,
    banner: &str,
    resized: bool,
    term_row: Option<u16>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(r.scrollback.len() + r.screen.len() + banner.len() + 32);
    if r.screen.is_empty() {
        out.extend_from_slice(banner.as_bytes());
        out.extend_from_slice(b"\r\n");
        return out;
    }
    out.extend_from_slice(&r.scrollback);
    // この attach が大きさを変えたときは、画面を描き直さない。
    // 大きさが変わると中のシェル / ConPTY が自分で画面を描き直す。ConPTY は viewport を下寄せで組み直し
    // 絶対座標で書くので、こちらが描いた古い大きさの画面の上に重なり、上に古い行・下に新しい行という
    // 壊れた見え方になる。
    // ▶ 履歴を流し、画面は**空の rows 行**にして、描き直しに任せる（空の上に描けば崩れない）。
    //   案内は履歴の末尾に残す（合図は描き直しで消えるので置かない）。
    // 注意: 画面の中身も行として流す（落とすと、最後の 1 画面分が履歴にも画面にも残らない）。末尾の空行は切り捨てる。
    if resized {
        let mut screen = r.screen.as_slice();
        while let Some(s) = screen.strip_suffix(b"\r\n") {
            screen = s;
        }
        if !screen.is_empty() {
            out.extend_from_slice(screen);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(banner.as_bytes());
        out.extend_from_slice(b"\r\n");
        for _ in 1..rows {
            out.extend_from_slice(b"\r\n");
        }
        return out;
    }
    let (row, col) = r.cursor;
    let signal_at = signal_column(cols, col, r.cursor_row_len, banner);
    // `signal_at` が `None`（右端に余白が無い、すなわち全幅を使う CLI）のときは合図を置かない。行を足すと画面を
    // 押し上げて ConPTY の絶対座標を壊す。
    //
    // 画面の頭の空の行は**書かない**。続きだけの入り直しは頭の状態（`replay_head`）で最下行から始めるので、
    // 続きがプロンプト 1 行でも画面は「上 rows−1 行が空・最下行がプロンプト」になり、そのまま書くと
    // `berth attach` を打った行の下に 1 画面ぶんの空白が割り込む。
    // ▶ 端末に今の行（`term_row`・履歴を書いた後に DSR で訊いた答え）を聞き、中身の行が最下行で終わるのに
    //   **足りない分だけ**改行する。上の行には端末に元から在った物が残る。
    //   最下行と pty の最下行は揃ったまま（ConPTY の絶対座標の前提は崩さない）。
    // 答えが無い（`None`）・alt screen の中（格子の全部が意味を持つ）は rows 行をそのまま書く。
    let blank = match term_row {
        Some(_) if !r.in_alt_screen => leading_blank_rows(&r.screen, row),
        _ => 0,
    };
    if blank > 0 {
        let k = term_row.unwrap_or(0).min(rows.saturating_sub(1));
        for _ in k..blank {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(&r.screen[usize::from(blank) * 2..]);
    } else {
        out.extend_from_slice(&r.screen);
    }
    let up = rows.saturating_sub(1).saturating_sub(row);
    if up > 0 {
        out.extend_from_slice(format!("\x1b[{up}A").as_bytes());
    }
    if let Some(start) = signal_at {
        out.extend_from_slice(format!("\x1b[{}G\x1b[2m{banner}\x1b[0m", start + 1).as_bytes());
    }
    out.extend_from_slice(format!("\x1b[{}G", col + 1).as_bytes());
    out
}

/// 描き直した画面（`render` の `screen`・行は `\r\n` 区切り）の**頭の空の行**の数。カーソルの行より上だけ数える
/// （カーソルの行は空でも書き込む。そこへ戻るため）。`blank > 0` のときのみ端末に行番号を問い合わせる。
fn leading_blank_rows(screen: &[u8], cursor_row: u16) -> u16 {
    let mut n: u16 = 0;
    while n < cursor_row
        && screen
            .get(usize::from(n) * 2..)
            .is_some_and(|rest| rest.starts_with(b"\r\n"))
    {
        n += 1;
    }
    n
}

/// 打鍵に混ざって来た DSR の答え（`ESC[<行>;<桁>R`）を**抜き取って**、0 始まりの行を返す。
/// 答えが途中までしか来ていなければ `None`（次の読みで続く）。答え以外の打鍵は `buf` に残す（後で流す）。
pub(super) fn take_dsr_reply(buf: &mut Vec<u8>) -> Option<u16> {
    let start = buf.windows(2).position(|w| w == b"\x1b[")?;
    let mut i = start + 2;
    let mut row: u32 = 0;
    let mut digits = 0;
    while i < buf.len() && buf[i].is_ascii_digit() {
        row = row
            .saturating_mul(10)
            .saturating_add(u32::from(buf[i] - b'0'));
        i += 1;
        digits += 1;
    }
    if i >= buf.len() {
        return None;
    }
    if digits == 0 || buf[i] != b';' {
        // `ESC[` で始まる別のシーケンス（矢印キーなど）であり応答ではない。その先にもう 1 つ `ESC[` があれば解析を続ける。
        let rest = buf[start + 2..].windows(2).position(|w| w == b"\x1b[")?;
        let mut tail = buf.split_off(start + 2 + rest);
        let got = take_dsr_reply(&mut tail);
        buf.extend_from_slice(&tail);
        return got;
    }
    i += 1;
    let mut cols = 0;
    while i < buf.len() && buf[i].is_ascii_digit() {
        i += 1;
        cols += 1;
    }
    if i >= buf.len() {
        return None;
    }
    if cols == 0 || buf[i] != b'R' {
        let rest = buf[start + 2..].windows(2).position(|w| w == b"\x1b[")?;
        let mut tail = buf.split_off(start + 2 + rest);
        let got = take_dsr_reply(&mut tail);
        buf.extend_from_slice(&tail);
        return got;
    }
    buf.drain(start..=i);
    Some(u16::try_from(row.saturating_sub(1)).unwrap_or(u16::MAX))
}

/// 再接続時に**端末へ現在の行を問い合わせる必要があるか**を判定する。画面の先頭に空行がある場合のみ true（通常の再接続は
/// 履歴が画面を埋めるため `false` となり、問い合わせも待機も行わない）。
pub(super) fn wants_term_row(r: &berth_screen::Rendered, resized: bool) -> bool {
    !resized
        && !r.in_alt_screen
        && !r.screen.is_empty()
        && leading_blank_rows(&r.screen, r.cursor.0) > 0
}

/// 描き直した画面（履歴は除く）＋ 中で名乗っていた題名を、端末へ書くバイトにする。
pub(super) fn screen_bytes(
    r: &berth_screen::Rendered,
    cols: u16,
    rows: u16,
    banner: &str,
    resized: bool,
    term_row: Option<u16>,
    mine: &str,
) -> Vec<u8> {
    let mut bytes = replay_entrance(r, cols, rows, banner, resized, term_row);
    // 録画の中で中のアプリが名乗っていた題名も、同じ形で戻す。
    if r.title.is_some() {
        bytes.extend_from_slice(title_sequence(mine, r.title.as_deref()).as_bytes());
    }
    bytes
}

/// 端末に行番号を問い合わせている間のバッファ。描画する画面と、応答待ち中に混ざったキー入力を保持する。
pub(super) struct DsrWait {
    pub(super) r: berth_screen::Rendered,
    pub(super) keys: Vec<u8>,
    /// 待っている間に届いた live 出力（端末向けに整形済み）。重要: 先に書き込むと、履歴の直後・画面の前に挟まり、
    /// その後に書く画面（相対移動で描く）が崩れる。画面を書いてから流す。
    pub(super) live: Vec<u8>,
}

/// 抜けるときの合図（スクロールへ流す形で新しく作ったときの `[attached to …]` も同じ置き方）。
/// スクロールへ流す形では**中のシェルの最後のプロンプト行**（今の行の 1 つ上・`IN$ berth d` を打った行）の
/// 右端に薄く置き、今の行へ戻って**そこから下を消す**。外側のシェルは今の行にプロンプトを描く:
///
/// ```text
/// IN$ berth d                                   [detached from t1]
/// OUT$ ▌
/// ```
///
/// その行は抜けた後に誰も描き直さないので、外側のシェルがプロンプトを描く前に行から下を消しても残る。
/// 入らなければ、また部屋の形なら、自分の行に素で出す。
/// 注意: 1 つ上の行の中身は把握していない（live は素通し）ため、右端の桁に文字があれば上書きする。中で `berth d` を打った
/// ときはプロンプト行（Enter で今の行へ下りている）。外から detach されたときは出力の途中の行でありうる。
/// 呼ぶ前提: カーソルは今の行の**頭**に居る（`DETACH_RESET_NO_ROOM` の `\r`）。
pub(super) fn farewell_bytes(scrollback: bool, cols: u16, msg: &str) -> Vec<u8> {
    let width = u16::try_from(unicode_width::UnicodeWidthStr::width(msg)).unwrap_or(u16::MAX);
    match cols.checked_sub(width + 1) {
        // 1 つ上の行の右端 → 今の行の頭へ戻る（`\r\n`・上端なら `1A` は no-op で今の行に置かれ、改行で 1 行下がる）
        // → 今の行から下を消す。
        Some(start) if scrollback => {
            format!("\x1b[1A\x1b[{}G\x1b[2m{msg}\x1b[0m\r\n\x1b[J", start + 1).into_bytes()
        }
        _ if scrollback => format!("{msg}\r\n\x1b[J").into_bytes(),
        _ => format!("{msg}\r\n").into_bytes(),
    }
}

/// 新しいセッションを作るとき、**窓を上端から使う**ために端末へ書くバイト列。
///
/// 今の行が窓の上端に来るまでスクロールし（改行を `rows - 1` 回・押し出された行は端末が自分の
/// スクロールバックへ積む）、カーソルを上端へ戻す。`clear` と同じ見え方で、消える行は無い。
///
/// なぜ要るか: 中の pty（ConPTY）は起動時に `ESC[6n` でカーソル行を訊き、その行を自分の開始行にして、
/// 以後の描き直しを絶対座標で置く。その問いには berthd が「1 行目」と答えるので、開始行は端末に依らず
/// 常に 1 となるため、ここではウィンドウ側をそれに合わせる。再接続時の再描画（`berth_screen::render`）も
/// 1 行目から始まるので、生きているときと入り直したときの画面が同じ形になる。
/// unix の pty は訊かないが、見え方を揃えるために同じにする（tmux が画面を取るのと同じ動き）。
/// 呼ぶ前提: 今の行は空で、カーソルはその頭に居る（合図を置いた直後）。
pub(super) fn take_the_window(rows: u16) -> Vec<u8> {
    let mut out = vec![b'\n'; usize::from(rows.saturating_sub(1))];
    out.extend_from_slice(b"\x1b[H");
    out
}

/// カーソルの居る行の右端に合図を薄く置いて、カーソルの桁へ戻る（`replay_entrance` の合図と同じ置き方）。
/// 入らなければ何も書かない（題名だけが名乗る）。呼ぶ前提: 端末のカーソルが `r.cursor` の場所に居る。
pub(super) fn signal_beside_cursor(r: &berth_screen::Rendered, cols: u16, banner: &str) -> Vec<u8> {
    let (_, col) = r.cursor;
    match signal_column(cols, col, r.cursor_row_len, banner) {
        Some(start) => {
            format!("\x1b[{}G\x1b[2m{banner}\x1b[0m\x1b[{}G", start + 1, col + 1).into_bytes()
        }
        None => Vec::new(),
    }
}

/// **続きが 0 バイトの入り直し**の合図。カーソルの居る空の行（`berth attach` を打った次の行の頭）の右端に薄く置き、
/// 行頭へ戻る。入らなければ何も書かない（題名だけが名乗る）。
///
/// 同じペインで前回見せた所から何も出ていないと、daemon は空の録画を返し、録画を描き終えた瞬間に置く
/// 合図（`screen_bytes`）に一度も到達しないため、これが無いと画面に何も表示されず固まったように見える。
/// 画面は再描画しない（端末には同じペインの画面がそのまま残っている）。
pub(super) fn signal_on_empty_row(cols: u16, banner: &str) -> Vec<u8> {
    match signal_column(cols, 0, 0, banner) {
        Some(start) => format!("\r\x1b[{}G\x1b[2m{banner}\x1b[0m\r", start + 1).into_bytes(),
        None => Vec::new(),
    }
}

/// 合図を置く桁（0 始まり）。右端を 1 桁空けて右寄せ。**行の文字ともカーソルとも 1 桁以上離れる**ときだけ。
/// 幅は表示幅で測る（名前に全角が在りうる）。
fn signal_column(cols: u16, cursor_col: u16, row_len: u16, banner: &str) -> Option<u16> {
    let width = u16::try_from(unicode_width::UnicodeWidthStr::width(banner)).ok()?;
    let start = cols.checked_sub(width + 1)?;
    (row_len < start && cursor_col + 1 < start).then_some(start)
}

/// detach 直前に端末へ書き込むリセット用バイト列。定数にしているのはテストで参照するため。
/// pty 越しの検体（`tests/attach_turns_off_what_it_turned_on.rs`）は ConPTY のホストが自分の mode
/// （`?9001` win32-input / `?1004` フォーカス）を飲むことがあるので、berth が**書いた**かどうかを
/// そこでは測れない（飲むかどうかはホストで違う）。
///
/// 中の全画面アプリが点けたまま落ちた分も倒す: マウス報告（`?1000` / `?1002` / `?1003` / `?1006`）・
/// kitty のキーボード（`CSI < u` で 1 段戻す）・色（`CSI 0 m`）。スクロール領域（`CSI r`）は戻さない。
/// カーソルを home へ動かすので、抜ける合図の置き場所が崩れる。
pub(super) const DETACH_RESET: &[u8] = b"\x1b[?1049l\x1b[?2004l\x1b[?9001l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[0m\x1b[?25h\x1b[?1l\r\x1b]2746;detach\x1b\\";

/// **代替画面を使わなかった**場合のリセット列。上の列から `?1049l` を除いたもの。
///
/// 重要: `?1049l` は「alt に居なければ no-op」ではない。ConPTY は alt を抜ける合図で、保存していない
/// カーソルを home へ戻すので、スクロールへ流した形でこれを出すと `[detached from …]` と次のプロンプトが
/// 画面の先頭から描かれ、セッションの古い画面に重なる。中のアプリが alt に居るまま抜けるときだけ
/// （`AltScreenShim` の深さ > 0）、呼び手が `?1049l` を**先に**足す。
///
/// ここは mode を倒して `\r` で行頭へ戻し、detach の信号を出すだけ。カーソルより下に残った
/// セッションの画面を消すのと、抜ける合図を置くのは [`farewell_bytes`] の仕事。
pub(super) const DETACH_RESET_NO_ROOM: &[u8] = b"\x1b[?2004l\x1b[?9001l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[0m\x1b[?25h\x1b[?1l\r\x1b]2746;detach\x1b\\";

/// 抜けるときにペインへ覚える「見せた位置」。
///
/// 重要: **録画を描き終えていなければ記録しない**。録画の途中で離脱した（遅い回線で切断された、外部から
/// detach された）のに録画の終わりまで見せたと覚えると、次に同じペインで入り直したとき daemon は
/// 続きしか返さず、描いていない分がそのペインでは二度と出ない。覚えなければ前回の位置が残り、
/// 多めに流れるだけで済む。注意: DSR の応答を待っている間も、画面はまだ書き込んでいない。
pub(super) fn seen_to_remember(
    seen_pos: Option<u64>,
    replay_remaining: usize,
    awaiting_term_row: bool,
) -> Option<u64> {
    if replay_remaining > 0 || awaiting_term_row {
        return None;
    }
    seen_pos
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_resume_with_nothing_new_still_signals_at_the_right_edge() {
        let b = "[attached to x]";
        let got = String::from_utf8(super::signal_on_empty_row(80, b)).unwrap();
        // 幅 15・右端を 1 桁空けるため、0 始まりで 64 桁目（1 始まりで 65）になる。行頭から始めて行頭へ戻る。
        assert_eq!(got, format!("\r\x1b[65G\x1b[2m{b}\x1b[0m\r"));
        // 入らない窓では何も書かない。
        assert!(super::signal_on_empty_row(10, b).is_empty());
    }

    #[test]
    fn a_pane_remembers_only_what_it_actually_drew() {
        use super::seen_to_remember;
        assert_eq!(seen_to_remember(Some(900), 0, false), Some(900));
        // 録画の途中で抜けた。
        assert_eq!(seen_to_remember(Some(900), 120, false), None);
        // 履歴は書いたが、画面は DSR の答え待ちで書いていない。
        assert_eq!(seen_to_remember(Some(900), 0, true), None);
        // 古い daemon（基準が無い）。
        assert_eq!(seen_to_remember(None, 0, false), None);
    }

    /// **berth が書く後始末の列そのもの**（ConPTY を挟まない）: ConPTY の前口上が点ける
    /// `?9001` / `?1004` を消し、alt screen と bracketed paste も倒し、カーソルを戻してから detach を名乗る。
    /// pty 越しの検体はホストが飲む分を見られないので、「書いているか」はここで釘付けする。
    #[test]
    fn the_detach_reset_turns_off_what_conpty_turns_on() {
        let s = String::from_utf8_lossy(super::DETACH_RESET);
        for off in ["\x1b[?9001l", "\x1b[?1004l", "\x1b[?1049l", "\x1b[?2004l"] {
            assert!(s.contains(off), "{off:?} が無い: {s:?}");
        }
        assert!(s.contains("\x1b[?25h"), "カーソルを戻していない: {s:?}");
        assert!(
            s.ends_with("\x1b]2746;detach\x1b\\"),
            "detach の信号が末尾に無い: {s:?}"
        );
    }

    /// 中のアプリ（`vim` の `mouse=a`・htop・neovim の CSI-u）が点けたまま抜けても、外側のシェルに
    /// クリックが `^[[<0;12;5M` として入らない・キーが化けない・色が残らない。両方の形で。
    #[test]
    fn the_detach_resets_turn_off_what_full_screen_apps_turn_on() {
        for reset in [super::DETACH_RESET, super::DETACH_RESET_NO_ROOM] {
            let s = String::from_utf8_lossy(reset);
            for off in [
                "\x1b[?1000l",
                "\x1b[?1002l",
                "\x1b[?1003l",
                "\x1b[?1006l",
                "\x1b[<u",
                "\x1b[0m",
            ] {
                assert!(s.contains(off), "{off:?} が無い: {s:?}");
            }
            // スクロール領域は戻さない（`CSI r` はカーソルを home へ動かし、合図の配置位置が崩れるため）。
            assert!(!s.contains("\x1b[r"), "{s:?}");
        }
    }

    /// 部屋を作らなかったときの後始末は **`?1049l` を含まない**（ConPTY がカーソルを home へ戻す）。
    /// 同じ mode は倒し、行頭へ戻し、detach の信号は末尾。
    #[test]
    fn the_no_room_detach_reset_leaves_the_alt_screen_alone_and_clears_below() {
        let s = String::from_utf8_lossy(super::DETACH_RESET_NO_ROOM);
        assert!(
            !s.contains("\x1b[?1049"),
            "部屋が無いのに alt を触っている: {s:?}"
        );
        for off in ["\x1b[?9001l", "\x1b[?1004l", "\x1b[?2004l", "\x1b[?1l"] {
            assert!(s.contains(off), "{off:?} が無い: {s:?}");
        }
        assert!(s.contains("\x1b[?25h"), "カーソルを戻していない: {s:?}");
        // 消去は `farewell_bytes` の役割であり、ここでは行頭へ戻すのみ（合図は 1 つ上の行の右端に配置される）。
        assert!(
            !s.contains("\x1b[J"),
            "ここでは消さない（合図を置いてから消す）: {s:?}"
        );
        assert!(s.contains("\r\x1b]2746"), "行頭へ戻していない: {s:?}");
        assert!(
            s.ends_with("\x1b]2746;detach\x1b\\"),
            "detach の信号が末尾に無い: {s:?}"
        );
    }

    /// 描き直した録画は 履歴 → 画面 → 合図（カーソル行の右端・置けるときだけ）→ カーソル。
    #[test]
    fn the_signal_sits_beside_the_cursor_and_returns_to_it() {
        use super::signal_beside_cursor;
        let r = berth_screen::Rendered {
            cursor: (3, 5),
            cursor_row_len: 5,
            ..Default::default()
        };
        // cols=20・`[B]` は幅 3 → 右端 1 桁空けて `17G`。置いたらカーソルの桁（`6G`）へ戻る。
        assert_eq!(
            signal_beside_cursor(&r, 20, "[B]"),
            b"\x1b[17G\x1b[2m[B]\x1b[0m\x1b[6G"
        );
        // 行の文字が右端まで届く・入らない → 置かない。
        let r = berth_screen::Rendered {
            cursor_row_len: 16,
            ..r
        };
        assert!(signal_beside_cursor(&r, 20, "[B]").is_empty());
        assert!(signal_beside_cursor(&r, 3, "[B]").is_empty());
    }

    #[test]
    fn taking_the_window_scrolls_the_current_line_to_the_top() {
        use super::take_the_window;
        assert_eq!(take_the_window(4), b"\n\n\n\x1b[H");
        assert_eq!(take_the_window(1), b"\x1b[H");
        assert_eq!(take_the_window(0), b"\x1b[H");
    }

    #[test]
    fn the_replay_entrance_puts_the_signal_at_the_right_of_the_cursor_row() {
        use super::replay_entrance;
        let r = berth_screen::Rendered {
            scrollback: b"old\r\n".to_vec(),
            screen: b"a\r\n> \r\n\r\n".to_vec(),
            cursor: (1, 2),
            cursor_row_len: 1,
            in_alt_screen: false,
            title: None,
        };
        // cols=20・`[B]` は幅 3 → 右端 1 桁空けて 0 始まりの 16 桁目（`17G`）。最下行（3）から 2 つ上がって置き、
        // カーソルの桁（`3G`）へ戻す。縦には 1 行も増えない。
        assert_eq!(
            replay_entrance(&r, 20, 4, "[B]", false, None),
            b"old\r\na\r\n> \r\n\r\n\x1b[2A\x1b[17G\x1b[2m[B]\x1b[0m\x1b[3G"
        );
        // 入らない（cols=6）: 画面を押し上げて ConPTY の座標を壊さないよう、合図は置かない（全幅を使う CLI）。
        assert_eq!(
            replay_entrance(&r, 6, 4, "[B]", false, None),
            b"old\r\na\r\n> \r\n\r\n\x1b[2A\x1b[3G"
        );
        // 右側に文字が在る（行が 16 桁まで埋まっている）: 重ねず、合図はスキップ。
        let busy = berth_screen::Rendered {
            cursor_row_len: 16,
            ..r.clone()
        };
        assert_eq!(
            replay_entrance(&busy, 20, 4, "[B]", false, None),
            b"old\r\na\r\n> \r\n\r\n\x1b[2A\x1b[3G"
        );
        // カーソルが最下行なら上へは動かない。
        let bottom = berth_screen::Rendered {
            cursor: (3, 0),
            cursor_row_len: 0,
            ..r.clone()
        };
        assert_eq!(
            replay_entrance(&bottom, 20, 4, "[B]", false, None),
            b"old\r\na\r\n> \r\n\r\n\x1b[17G\x1b[2m[B]\x1b[0m\x1b[1G"
        );
        // 録画が空（新しいセッション）なら案内のみを表示する（従来の表示と同じ）。
        assert_eq!(
            replay_entrance(
                &berth_screen::Rendered::default(),
                20,
                24,
                "[B]",
                false,
                None
            ),
            b"[B]\r\n"
        );
    }

    /// サイズが変更された attach では画面を再描画しない（履歴 → 案内 → 空の rows 行を出力し、再描画に任せる）。
    #[test]
    fn a_resized_attach_leaves_the_screen_blank_for_the_shell_to_repaint() {
        use super::replay_entrance;
        let r = berth_screen::Rendered {
            scrollback: b"old\r\n".to_vec(),
            screen: b"a\r\n> \r\n\r\n".to_vec(),
            cursor: (1, 2),
            cursor_row_len: 1,
            in_alt_screen: false,
            title: None,
        };
        // 履歴 → 画面の中身（末尾の空行は切る）→ 案内 → 空の rows 行。画面の行も履歴として残る。
        assert_eq!(
            replay_entrance(&r, 20, 4, "[B]", true, None),
            b"old\r\na\r\n> \r\n[B]\r\n\r\n\r\n\r\n"
        );
    }

    /// 画面の頭の空の行は書かず、端末の今の行から最下行まで足りない分だけ改行する。
    /// 答えが無い・alt screen の中・カーソルより上でないなら rows 行をそのまま書く。
    #[test]
    fn the_screen_is_written_from_the_terminals_row_not_padded_with_blank_rows() {
        use super::{leading_blank_rows, replay_entrance, wants_term_row};
        // rows=5: 上 3 行が空・4 行目 MARK・最下行がプロンプト（カーソルは最下行の 3 桁目）。
        let r = berth_screen::Rendered {
            scrollback: Vec::new(),
            screen: b"\r\n\r\n\r\nMARK\r\n> ".to_vec(),
            cursor: (4, 2),
            cursor_row_len: 1,
            in_alt_screen: false,
            title: None,
        };
        assert_eq!(leading_blank_rows(&r.screen, 4), 3);
        assert!(wants_term_row(&r, false));
        assert!(
            !wants_term_row(&r, true),
            "大きさが変わった attach は画面を描かないので訊かない"
        );
        // 端末のカーソルが 1 行目（0）に居る: 空 3 行分のうち 3 - 1 = 2 行だけ改行してから中身を書く。
        let signal = b"\x1b[17G\x1b[2m[B]\x1b[0m\x1b[3G";
        let mut want = b"\r\n\r\nMARK\r\n> ".to_vec();
        want.extend_from_slice(signal);
        assert_eq!(replay_entrance(&r, 20, 5, "[B]", false, Some(1)), want);
        // 端末のカーソルが空の行数より下（3）に居る: 改行は 0 本（書けば端末が自分でスクロールする）。
        let mut want = b"MARK\r\n> ".to_vec();
        want.extend_from_slice(signal);
        assert_eq!(replay_entrance(&r, 20, 5, "[B]", false, Some(3)), want);
        assert_eq!(replay_entrance(&r, 20, 5, "[B]", false, Some(4)), want);
        // 答えが無ければ rows 行をそのまま（空の 3 行も書く）。
        let mut want = b"\r\n\r\n\r\nMARK\r\n> ".to_vec();
        want.extend_from_slice(signal);
        assert_eq!(replay_entrance(&r, 20, 5, "[B]", false, None), want);
        // 合図が入らない（cols=6）: 案内の行を書かないので画面行を押し上げず、改行の数も変わらない。
        assert_eq!(
            replay_entrance(&r, 6, 5, "[B]", false, Some(1)),
            b"\r\n\r\nMARK\r\n> \x1b[3G"
        );
        // alt screen の中はセル全体が意味を持つため、問い合わせず、指定されても削らない。
        let alt = berth_screen::Rendered {
            in_alt_screen: true,
            ..r.clone()
        };
        assert!(!wants_term_row(&alt, false));
        assert!(
            replay_entrance(&alt, 20, 5, "[B]", false, Some(1)).starts_with(b"\r\n\r\n\r\nMARK")
        );
        // カーソルが空の行の中に居る（2 行目）: その行より上の 1 行しか削らない（戻る先を消さない）。
        let above = berth_screen::Rendered {
            cursor: (1, 0),
            cursor_row_len: 0,
            ..r.clone()
        };
        assert_eq!(leading_blank_rows(&above.screen, 1), 1);
        let out = replay_entrance(&above, 20, 5, "[B]", false, Some(0));
        assert!(
            out.starts_with(b"\r\n\r\n\r\nMARK\r\n> \x1b[3A"),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// DSR の応答はキー入力に混ざって届くため、抜き取って残りを流す。途中までなら次の入力を待つ。
    #[test]
    fn a_dsr_reply_is_taken_out_of_the_typed_bytes() {
        use super::take_dsr_reply;
        let mut b = b"\x1b[12;1R".to_vec();
        assert_eq!(take_dsr_reply(&mut b), Some(11));
        assert!(b.is_empty());
        let mut b = b"ab\x1b[3;9Rcd".to_vec();
        assert_eq!(take_dsr_reply(&mut b), Some(2));
        assert_eq!(b, b"abcd");
        // 途中まで: 消費せず None。続きが足されれば取れる。
        let mut b = b"\x1b[3".to_vec();
        assert_eq!(take_dsr_reply(&mut b), None);
        b.extend_from_slice(b";9R");
        assert_eq!(take_dsr_reply(&mut b), Some(2));
        // 矢印キー（`ESC[A`）が先に居ても、その後の答えを取る。矢印は残す。
        let mut b = b"\x1b[A\x1b[7;1R".to_vec();
        assert_eq!(take_dsr_reply(&mut b), Some(6));
        assert_eq!(b, b"\x1b[A");
        let mut b = b"xyz".to_vec();
        assert_eq!(take_dsr_reply(&mut b), None);
        assert_eq!(b, b"xyz");
    }

    /// 離脱時の合図: スクロールへ流す形式では右端に薄く表示し、収まらない場合や alt-screen 形式では通常の行として出力する。
    #[test]
    fn the_farewell_is_right_aligned_in_the_scrollback_form_and_plain_otherwise() {
        use super::farewell_bytes;
        // cols=40・幅 25 → 右端 1 桁空けて 0 始まり 14 桁目（`15G`）。
        assert_eq!(
            farewell_bytes(true, 40, "[detached from session-1]"),
            b"\x1b[1A\x1b[15G\x1b[2m[detached from session-1]\x1b[0m\r\n\x1b[J"
        );
        assert_eq!(
            farewell_bytes(false, 40, "[detached from session-1]"),
            b"[detached from session-1]\r\n"
        );
        assert_eq!(
            farewell_bytes(true, 10, "[detached from session-1]"),
            b"[detached from session-1]\r\n\x1b[J",
            "入らなければ素の行（スクロールへ流す形では下も消す）"
        );
    }

    /// 合図の配置桁: 右端 1 桁を空け、行の文字とカーソルの両方から 1 桁以上離れる場合のみ配置する（幅は表示幅で計算）。
    #[test]
    fn the_signal_column_keeps_a_gap_from_text_and_cursor_and_measures_width() {
        use super::signal_column;
        assert_eq!(signal_column(20, 2, 1, "[B]"), Some(16));
        assert_eq!(
            signal_column(20, 14, 1, "[B]"),
            Some(16),
            "カーソル 14・合図 16 ＝ 1 桁空く"
        );
        assert_eq!(
            signal_column(20, 15, 1, "[B]"),
            None,
            "カーソルの隣には置かない"
        );
        assert_eq!(signal_column(20, 2, 15, "[B]"), Some(16));
        assert_eq!(signal_column(20, 2, 16, "[B]"), None, "行の文字と重なる");
        assert_eq!(signal_column(4, 0, 0, "[B]"), None, "入らない");
        // 全角は 2 桁: `[日本]` は幅 6 → 20 - 7 = 13。
        assert_eq!(signal_column(20, 0, 0, "[日本]"), Some(13));
    }

    /// 繋がっている間は題名で名乗り、抜けるとき空にする（画面を触らない案内）。
    #[test]
    fn the_title_names_the_session_while_attached_and_is_cleared_on_the_way_out() {
        use super::{title_of, title_sequence, TITLE_CLEAR};
        assert_eq!(title_of("work", false), "berth: work");
        assert_eq!(title_of("work", true), "berth: work (watching)");
        assert_eq!(
            title_sequence("berth: work", None),
            "\x1b]2;berth: work\x1b\\"
        );
        // 中のアプリの題名が在れば `· ` で続ける。空白だけなら無いのと同じ。
        assert_eq!(
            title_sequence("berth: work", Some(" Claude Code ")),
            "\x1b]2;berth: work · Claude Code\x1b\\"
        );
        assert_eq!(
            title_sequence("berth: work", Some("  ")),
            "\x1b]2;berth: work\x1b\\"
        );
        // conhost の既定の題名（exe のパス）はアプリ名ではないため破棄する。
        assert_eq!(
            title_sequence(
                "berth: work",
                Some("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
            ),
            "\x1b]2;berth: work\x1b\\"
        );
        assert_eq!(
            title_sequence("berth: work", Some("cmd.EXE")),
            "\x1b]2;berth: work\x1b\\"
        );
        assert_eq!(TITLE_CLEAR, b"\x1b]2;\x1b\\");
    }
}
