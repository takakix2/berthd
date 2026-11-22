/// CSI（`ESC [` … 終端）が **端末に答えを要求する**物か。
///
/// 「設定」と「問い」を混ぜないこと。落としてよいのは後者だけ。前者を落とすと
/// 画面の再構成そのものが壊れる（`sanitize_replay` の doc）。
fn csi_is_a_question(seq: &[u8]) -> bool {
    if seq.len() < 3 {
        return false;
    }
    let body = &seq[2..seq.len() - 1];
    match seq[seq.len() - 1] {
        // DSR（`CSI 6 n` / `CSI 5 n` / `CSI ? 6 n`）。`n` は ECMA-48 で DSR 専用。
        // ConPTY の前口上に居る `ESC[6n` がこれ。
        b'n' => true,
        // DA1 / DA2 / DA3（`CSI c` / `CSI > c` / `CSI = c`）。`c` も問い専用。
        b'c' => true,
        // DECRQM（`CSI ? Ps $ p`）。`$` の無い `p` は DECSTR（`CSI ! p`）等の設定。
        b'p' => body.contains(&b'$'),
        // XTVERSION（`CSI > q`）。`>` の無い `q` は DECSCUSR（カーソル形状 = 設定）。
        b'q' => body.first() == Some(&b'>'),
        // XTWINOPS の報告系だけ（窓・画面の寸法や題名を答えさせる）。
        // 1〜10 は窓の操作（移動・リサイズ・アイコン化）で、答えは返らない。
        b't' => matches!(
            first_param(body),
            Some(11 | 13 | 14 | 15 | 16 | 18 | 19 | 20 | 21)
        ),
        _ => false,
    }
}

/// 先頭の数値引数（`CSI 18 t` の `18`）。数字で始まらなければ `None`。
fn first_param(body: &[u8]) -> Option<u32> {
    let end = body
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(body.len());
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&body[..end]).ok()?.parse().ok()
}

/// OSC（`ESC ]` … BEL / ST）が問い合わせか（値の位置が `?` かどうか）。
///
/// 例: `ESC ] 11 ; ? ST`（背景色）/ `ESC ] 4 ; 1 ; ? ST`（パレット）/
/// `ESC ] 52 ; c ; ? ST`（クリップボードの読み出し）。
/// 重要: 最後のものは、過去の録画が**いまのクリップボードを送らせる**という形になる。
fn osc_is_a_question(seq: &[u8]) -> bool {
    // 前後の飾りを外す: 先頭 `ESC ]`、終端は BEL か `ESC \`。
    let body = &seq[2..];
    let body = if body.ends_with(b"\x1b\\") {
        &body[..body.len() - 2]
    } else if body.ends_with(b"\x07") {
        &body[..body.len() - 1]
    } else {
        body
    };
    body.rsplit(|&b| b == b';').next() == Some(b"?".as_slice())
}

/// DCS が DECRQSS（`ESC P $ q … ST`、設定の問い合わせ）か。
fn dcs_is_a_question(seq: &[u8]) -> bool {
    seq.len() >= 4 && seq[2] == b'$' && seq[3] == b'q'
}

/// replay 用に、端末が答えを返す物と、OSC の外の裸 BEL(0x07) を落とす。
///
/// # 規則は 1 つ（live 出力は素通し。過去の再生だけ黙らせる）
///
/// replay_buf は PTY 出力の生録画なので、当時のバイトがそのまま入っている。それを reattach で
/// 端末へ流すと、過去の出来事が現在に対して起きる。害は 2 段あり、後の方が重い:
///
/// | | 起きること |
/// |---|---|
/// | 裸 BEL | 当時のベルが鳴り直す（短い録画にも数十個入る）|
/// | 問い合わせ | 端末が答えてしまい、その応答がシェルの stdin へ行く |
///
/// 重要: 後者は「余計な行が 1 本増える」では済まない。応答（例: `ESC[1;1R`）が打鍵と競合すると、
/// 利用者のコマンドに混ざる（`echo PROBE_OK` と打った行が `Recho PROBE_OK` として実行される）。
///
/// # 落とすのは「問い」だけ。「設定」は残す
///
/// `\e]0;title\a` の OSC 終端 BEL は残す。落とすと OSC が終端されず、続くプロンプト
/// 文字列がタイトル送りに飲まれて画面が壊れる。まともな端末は OSC 終端の BEL を鳴らさない
/// （残しても無音）。同じ理由で `CSI ! p`（DECSTR）や `CSI Ps q`（カーソル形状）は残す。
/// 判定は [`csi_is_a_question`] / [`osc_is_a_question`] / [`dcs_is_a_question`] が持つ。
///
/// # 終端が来なかった分は落とさない
///
/// replay_buf は先頭が切り捨てられる（`REPLAY_CAP`）ので、断片から始まることが在る。
/// 判定できないまま溜まった分はそのまま出す（「読めなかったから消す」は
/// 画面を壊す側に倒れる）。
pub(crate) fn sanitize_replay(raw: &[u8]) -> Vec<u8> {
    enum S {
        Normal,
        Esc,
        Csi,
        Osc,
        OscEsc,
        Dcs,
        DcsEsc,
    }
    let mut out = Vec::with_capacity(raw.len());
    // 判定のために溜める（CSI / OSC / DCS の本体。終端を見るまで出せない）。
    let mut buf: Vec<u8> = Vec::new();
    let mut st = S::Normal;
    for &b in raw {
        match st {
            S::Normal => match b {
                0x1b => {
                    st = S::Esc;
                    buf.push(b);
                }
                0x07 => {} // 裸のベル → 落とす
                // ENQ: 端末が answerback 文字列を返す。印字もされないので落として安全。
                0x05 => {}
                _ => out.push(b),
            },
            S::Esc => {
                buf.push(b);
                match b {
                    b'[' => st = S::Csi,
                    b']' => st = S::Osc,
                    b'P' => st = S::Dcs,
                    // DECID（`ESC Z`）: DA と同じ答えを返させる古い綴り。
                    b'Z' => {
                        buf.clear();
                        st = S::Normal;
                    }
                    _ => {
                        out.append(&mut buf);
                        st = S::Normal;
                    }
                }
            }
            S::Csi => {
                buf.push(b);
                // CSI の終端は 0x40..=0x7E。それ以外（引数 `0x30..=0x3F` / 中間 `0x20..=0x2F`）は溜める。
                if (0x40..=0x7e).contains(&b) {
                    if !csi_is_a_question(&buf) {
                        out.extend_from_slice(&buf);
                    }
                    buf.clear();
                    st = S::Normal;
                }
            }
            S::Osc => {
                buf.push(b);
                match b {
                    0x07 => {
                        // BEL は OSC 終端（残す・無音）。
                        if !osc_is_a_question(&buf) {
                            out.extend_from_slice(&buf);
                        }
                        buf.clear();
                        st = S::Normal;
                    }
                    0x1b => st = S::OscEsc, // ST(`\e\\`) の可能性
                    _ => {}
                }
            }
            S::OscEsc => {
                buf.push(b);
                if b == b'\\' {
                    if !osc_is_a_question(&buf) {
                        out.extend_from_slice(&buf);
                    }
                    buf.clear();
                    st = S::Normal;
                } else {
                    st = S::Osc;
                }
            }
            S::Dcs => {
                buf.push(b);
                if b == 0x1b {
                    st = S::DcsEsc;
                }
            }
            S::DcsEsc => {
                buf.push(b);
                if b == b'\\' {
                    if !dcs_is_a_question(&buf) {
                        out.extend_from_slice(&buf);
                    }
                    buf.clear();
                    st = S::Normal;
                } else {
                    st = S::Dcs;
                }
            }
        }
    }
    // 終端が来なかった断片はそのまま出す（上の doc の理由）。
    out.append(&mut buf);
    out
}

#[cfg(test)]
mod tests {
    use super::sanitize_replay;

    #[test]
    fn strips_bare_bells_but_keeps_osc_terminator() {
        // 行頭 backspace 連打で録画された「裸のベル」は落とす。
        assert_eq!(sanitize_replay(b"ab\x07\x07cd"), b"abcd");
        // OSC タイトル（`\e]0;title\a`）の終端 BEL は残す（落とすと OSC が終端されず画面が壊れる）。
        let osc = b"\x1b]0;title\x07next";
        assert_eq!(sanitize_replay(osc), osc);
        // OSC を ST(`\e\\`) で閉じた後の裸 BEL は落とす。
        assert_eq!(
            sanitize_replay(b"\x1b]0;t\x1b\\x\x07y"),
            b"\x1b]0;t\x1b\\xy"
        );
        // CSI(`\e[…m`)は素通し（BEL を含まない）。
        assert_eq!(
            sanitize_replay(b"\x1b[01;32mok\x1b[0m"),
            b"\x1b[01;32mok\x1b[0m"
        );
    }

    /// 端末が答えを返す物は落とす（応答はシェルの stdin へ行くため）。
    ///
    /// ConPTY の前口上に居るのは `ESC[6n` 1 つだが、同じ穴は他にも開いているので族ごと塞ぐ。
    #[test]
    fn strips_everything_the_terminal_would_answer() {
        // ① DSR: ConPTY の前口上に居る。
        assert_eq!(sanitize_replay(b"a\x1b[6nb"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b[5nb"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b[?6nb"), b"ab");
        // ② DA1 / DA2 / DA3。
        assert_eq!(sanitize_replay(b"a\x1b[cb"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b[>cb"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b[=cb"), b"ab");
        // ③ DECRQM（`$` 付きの `p`）。
        assert_eq!(sanitize_replay(b"a\x1b[?2026$pb"), b"ab");
        // ④ XTVERSION（`>` 付きの `q`）。
        assert_eq!(sanitize_replay(b"a\x1b[>0qb"), b"ab");
        // ⑤ XTWINOPS の報告系（14 = 画素での窓の寸法 / 18 = 文字での画面の寸法）。
        assert_eq!(sanitize_replay(b"a\x1b[14tb"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b[18tb"), b"ab");
        // ⑥ OSC の問い合わせ。52 はクリップボードを送らせる。
        assert_eq!(sanitize_replay(b"a\x1b]11;?\x1b\\b"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b]4;1;?\x07b"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1b]52;c;?\x1b\\b"), b"ab");
        // ⑦ DECRQSS（`ESC P $ q … ST`）と DECID（`ESC Z`）と ENQ。
        assert_eq!(sanitize_replay(b"a\x1bP$qm\x1b\\b"), b"ab");
        assert_eq!(sanitize_replay(b"a\x1bZb"), b"ab");
        assert_eq!(sanitize_replay(b"a\x05b"), b"ab");
    }

    /// 陰性対照: 設定は残す。ここが緩むと画面の再構成が壊れる。
    ///
    /// 上の検体だけだと「CSI を全部落とす」でも緑になるので、対で主張する。
    #[test]
    fn keeps_everything_that_only_sets() {
        // ConPTY の前口上（`ESC[?9001h ESC[?1004h ESC[6n`）のうち、問いでない 2 つは残る
        // （win32-input-mode / focus 通知）。
        let preamble = b"\x1b[?9001h\x1b[?1004h\x1b[6n";
        assert_eq!(sanitize_replay(preamble), b"\x1b[?9001h\x1b[?1004h");
        // `p` は `$` が無ければ設定（DECSTR = soft reset）。
        assert_eq!(sanitize_replay(b"\x1b[!p"), b"\x1b[!p");
        // `q` は `>` が無ければ設定（DECSCUSR = カーソル形状）。
        assert_eq!(sanitize_replay(b"\x1b[2 q"), b"\x1b[2 q");
        // XTWINOPS の 1〜10 は窓の操作で、答えは返らない。
        assert_eq!(sanitize_replay(b"\x1b[4;600;800t"), b"\x1b[4;600;800t");
        // 値が `?` でない OSC はただのタイトル送り。
        assert_eq!(sanitize_replay(b"\x1b]0;title\x07"), b"\x1b]0;title\x07");
        assert_eq!(
            sanitize_replay(b"\x1b]11;#000000\x1b\\"),
            b"\x1b]11;#000000\x1b\\"
        );
        // 素の色付けと改行は 1 バイトも変えない。
        assert_eq!(
            sanitize_replay(b"\x1b[01;32mok\x1b[0m\r\n"),
            b"\x1b[01;32mok\x1b[0m\r\n"
        );
    }

    /// 終端が来なかった断片は落とさない（replay_buf は先頭が切り捨てられるため）。
    ///
    /// 「読めなかったから消す」は画面を壊す側に倒れる。
    #[test]
    fn an_unterminated_tail_is_kept_as_is() {
        assert_eq!(sanitize_replay(b"ok\x1b[38;2;1"), b"ok\x1b[38;2;1");
        assert_eq!(sanitize_replay(b"ok\x1b]0;half"), b"ok\x1b]0;half");
        assert_eq!(sanitize_replay(b"ok\x1b"), b"ok\x1b");
    }
}
