use super::replay::inner_title;

/// セッション出力の alt-screen 切替（`\e[?1049h/l`・`\e[?1047h/l`・`\e[?47h/l`）を
/// 画面クリアへ翻訳する代役。他の `\e[?…h/l`（bracketed paste 2004・カーソル可視 25・
/// cursor keys 1 等）は素通し。
///
/// # なぜ素通しに出来ないか
///
/// 対話 attach は端末を alt-screen（代替画面バッファ）に入れて「中の部屋」を作り、detach で元の画面へ
/// 戻す（tmux / vim / less の作法）。だがセッションの中で vim 等の全画面アプリを動かすと、
/// そちらも同じフラグを叩く。重要: alt buffer は 1 枚しかなく入れ子にできないため、素通しにすると
/// vim の alt-leave がこちらの部屋ごと抜けて元の画面に落ちる（attach 中なのに「外」に出てしまう）。
///
/// # なぜ「落とす」だけでは駄目か
///
/// 切替を落とすと、中のアプリはこちらの画面に直接描き、抜けるときの「元に戻せ」も落ちるので、
/// 描画がそのまま残り、抜けた後のプロンプトが残骸の上に重なる。
///
/// ∴ 落とす代わりに `\e[2J\e[H`（消去 → home）へ翻訳する。
/// 注意: これは「元に戻す」操作ではないため、全画面アプリに入る前の画面は失われる（素の端末なら戻る）。
/// 戻せないのは `berth` が画面の模型を持たないからで、直すなら tmux と同じ道になる。
///
/// # 数えるのは 0⇄1 の境目だけ
///
/// 入っていないのに `l` だけ来ることが在る（起動時に身構えて出すアプリ・`reset` の類）。
/// そこで消すと利用者の画面を勝手に消すので、対応する `h` を見た分だけ消す。
/// 実端末でも、alt に居るときの 2 回目の `h` は何もしないため、深さを数えるのが素直である。
///
/// # カーソルの保存/復元は真似しない
///
/// 実端末の `1049` は入口で DECSC・出口で DECRC も行うが、こちらは出口で画面を消すので、
/// 元の位置へ戻すと空の画面の途中にプロンプトが出る。∴ home に置く。
/// 巻き戻せない物（スクロール領域・SGR・カーソル可視）は実端末でも画面切替で戻らない
/// （xterm が保存・復元するのはカーソルと画面の中身のみ）であり、アプリ自身の後始末の領分である。
///
/// フレーム境界でシーケンスが割れても壊れないよう、途中一致は状態として持ち越す（held バイトはまだ出さない）。
enum AltState {
    Normal,
    Esc,     // `\e`
    Bracket, // `\e[`
    Priv,    // `\e[?` — params 収集中
    Osc,     // `\e]` — 本体収集中（題名の書き換え）
    OscEsc,  // `\e]…\e` — ST（`\e\\`）の途中か
    OscPass, // 長すぎる OSC（OSC 52 の貼り付け等）は溜めずに素通し・終端まで
    OscPassEsc,
}
pub(super) struct AltScreenShim {
    state: AltState,
    params: Vec<u8>,
    /// 中のアプリが alt-screen に何段入っているか。消すのは 0→1 と 1→0 のときだけ。
    pub(super) depth: u32,
    /// 翻訳するか（部屋の中）、素通しするか（スクロールへ流す形・部屋が無い）。
    /// 素通しでも深さは数える（抜けるときに alt の中に居れば `DETACH_RESET` の `?1049l` が外に出す）。
    translate: bool,
    /// 中のアプリが出す題名（OSC 0 / 2）を `berth: <名前> · <中の題名>` に書き換えるための、
    /// こちらの名乗り。`None` なら題名は素通し。
    /// 中のアプリが自分の名前をタブに出しても、セッション名が消えない。
    title: Option<String>,
    /// OSC の本体（`\e]` の後・終端の手前まで）。
    osc: Vec<u8>,
}
impl AltScreenShim {
    /// 溜める OSC の上限。超えたら素通しに切り替える（OSC 52 の貼り付けは何 KB にもなる）。
    const OSC_CAP: usize = 8 * 1024;

    /// 翻訳先: 消去 → home。順序は attach の入口と同じ理由による。
    /// `\e[2J` でカーソルが動く端末が在るので、home を消去の後に置く。
    const CLEAR: &'static [u8] = b"\x1b[2J\x1b[H";

    /// 部屋の中（alt screen に入って attach している）: 中の alt 切替を消去へ翻訳する。
    pub(super) fn translating() -> Self {
        Self {
            state: AltState::Normal,
            params: Vec::new(),
            depth: 0,
            translate: true,
            title: None,
            osc: Vec::new(),
        }
    }
    /// 部屋が無い（スクロールへ流す形）: 中の alt 切替をそのまま端末へ通す。
    /// 端末が 1 枚しか持たない alt buffer をこちらが使用していないため、中のアプリが使用してよい。
    /// vim を抜ければ端末が元の画面（履歴ごと）を自分で戻す。
    pub(super) fn passing_through() -> Self {
        Self {
            translate: false,
            ..Self::translating()
        }
    }
    /// 中から来る題名を `berth: <title> · <中の題名>` に書き換える（`title` はこちらの名乗り）。
    pub(super) fn renaming_titles(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }
    /// OSC が終端まで揃った: 題名（`0;` / `2;`）ならこちらの名乗りを頭に付け、他はそのまま出す。
    /// `term` は検出した終端そのもの（BEL か ST）であり、端末が受け取る表記を変えない。
    fn finish_osc(&mut self, term: &[u8], out: &mut Vec<u8>) {
        let rewritten = self.title.as_deref().and_then(|mine| {
            let sep = self.osc.iter().position(|&b| b == b';')?;
            let n = &self.osc[..sep];
            if n != b"0" && n != b"2" {
                return None;
            }
            let inner = String::from_utf8_lossy(&self.osc[sep + 1..]);
            Some(match inner_title(&inner) {
                None => format!("\x1b]{};{mine}", String::from_utf8_lossy(n)),
                Some(t) => format!("\x1b]{};{mine} · {t}", String::from_utf8_lossy(n)),
            })
        });
        match rewritten {
            Some(s) => out.extend_from_slice(s.as_bytes()),
            None => {
                out.extend_from_slice(b"\x1b]");
                out.extend_from_slice(&self.osc);
            }
        }
        out.extend_from_slice(term);
        self.osc.clear();
        self.state = AltState::Normal;
    }
    /// alt-screen 切替の DEC private mode 番号（他の `?…` モードは巻き込まない）。
    fn is_alt_screen(params: &[u8]) -> bool {
        matches!(params, b"1049" | b"1047" | b"47")
    }
    /// `?…h` / `?…l` が揃った: alt 切替が他の mode と並んで来ても見る。
    ///
    /// `?1049;1h` のように `;` で複数の mode を 1 本に乗せる綴りが在る（vim・less・ConPTY の前口上）。
    /// 重要: 完全一致で判定していると素通しになり、端末の本物の alt buffer が切り替わって状態が壊れる。
    /// ∴ alt の番号が 1 つでも混じっていれば、alt の分だけ `toggle`（翻訳 or 深さ）し、
    /// 残りの mode は同じ `h` / `l` で別に出す。alt の番号が無ければそのまま出す。
    fn switch(&mut self, enter: bool, out: &mut Vec<u8>) {
        let has_alt = self.params.split(|&b| b == b';').any(Self::is_alt_screen);
        if !has_alt {
            out.extend_from_slice(b"\x1b[?");
            out.extend_from_slice(&self.params);
            out.push(if enter { b'h' } else { b'l' });
            return;
        }
        if !self.translate {
            // 素通し: 綴りは変えず、深さだけ数える（`toggle` が params をそのまま出す）。
            self.toggle(enter, out);
            return;
        }
        let rest: Vec<&[u8]> = self
            .params
            .split(|&b| b == b';')
            .filter(|p| !p.is_empty() && !Self::is_alt_screen(p))
            .collect();
        if !rest.is_empty() {
            out.extend_from_slice(b"\x1b[?");
            out.extend_from_slice(&rest.join(&b';'));
            out.push(if enter { b'h' } else { b'l' });
        }
        self.toggle(enter, out);
    }
    /// alt-screen の出入りを深さで畳んで、画面へ出すバイトを決める。
    /// 翻訳する形では `h` も `l` もそのままは出さない（`berth` の部屋を壊すため）。
    fn toggle(&mut self, enter: bool, out: &mut Vec<u8>) {
        if !self.translate {
            // 素通し: 受信した列をそのまま出し、深さだけ数える。
            out.extend_from_slice(b"\x1b[?");
            out.extend_from_slice(&self.params);
            out.push(if enter { b'h' } else { b'l' });
            self.depth = if enter {
                self.depth + 1
            } else {
                self.depth.saturating_sub(1)
            };
            return;
        }
        if enter {
            if self.depth == 0 {
                out.extend_from_slice(Self::CLEAR);
            }
            self.depth += 1;
        } else if self.depth == 1 {
            out.extend_from_slice(Self::CLEAR);
            self.depth = 0;
        } else {
            // 深さ 0 の `l`（対応する `h` を見ていない）は無視して破棄する（型の doc を参照）。
            self.depth = self.depth.saturating_sub(1);
        }
    }
    /// `input` を通して、出力すべきバイトを `out` に積む（alt-screen 切替はクリアに翻訳）。
    pub(super) fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                AltState::Normal => {
                    if b == 0x1b {
                        self.state = AltState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                AltState::Esc => {
                    if b == b'[' {
                        self.state = AltState::Bracket;
                    } else if b == b']' && self.title.is_some() {
                        // 題名を書き換えるときだけ OSC を溜める（それ以外は素通し）。
                        self.state = AltState::Osc;
                        self.osc.clear();
                    } else {
                        // `\e` の後が `[` 以外 = ここで扱う対象外。held の ESC を出してから b を処理。
                        out.push(0x1b);
                        if b == 0x1b {
                            // ESC の連続。新しい ESC を held に。
                        } else {
                            out.push(b);
                            self.state = AltState::Normal;
                        }
                    }
                }
                AltState::Bracket => {
                    if b == b'?' {
                        self.state = AltState::Priv;
                        self.params.clear();
                    } else {
                        // private でない CSI（SGR 等）。held `\e[` と b を出す。残りは Normal で素通り。
                        out.extend_from_slice(b"\x1b[");
                        out.push(b);
                        self.state = AltState::Normal;
                    }
                }
                AltState::Priv => {
                    if b.is_ascii_digit() || b == b';' {
                        self.params.push(b);
                    } else {
                        if b == b'h' || b == b'l' {
                            self.switch(b == b'h', out);
                        } else {
                            out.extend_from_slice(b"\x1b[?");
                            out.extend_from_slice(&self.params);
                            out.push(b);
                        }
                        self.state = AltState::Normal;
                    }
                }
                AltState::Osc => match b {
                    0x07 => self.finish_osc(b"\x07", out),
                    0x1b => self.state = AltState::OscEsc,
                    _ => {
                        self.osc.push(b);
                        if self.osc.len() > Self::OSC_CAP {
                            // 長すぎる: 溜めた分を出して、終端まで素通しに切り替える。
                            out.extend_from_slice(b"\x1b]");
                            out.extend_from_slice(&self.osc);
                            self.osc.clear();
                            self.state = AltState::OscPass;
                        }
                    }
                },
                AltState::OscEsc => {
                    if b == b'\\' {
                        self.finish_osc(b"\x1b\\", out);
                    } else {
                        // ST ではなかった: 本体の一部として ESC を戻し、b を本体として読み直す。
                        self.osc.push(0x1b);
                        self.state = AltState::Osc;
                        self.feed(&[b], out);
                    }
                }
                AltState::OscPass => {
                    out.push(b);
                    match b {
                        0x07 => self.state = AltState::Normal,
                        0x1b => self.state = AltState::OscPassEsc,
                        _ => {}
                    }
                }
                AltState::OscPassEsc => {
                    out.push(b);
                    self.state = match b {
                        b'\\' => AltState::Normal,
                        0x1b => AltState::OscPassEsc,
                        _ => AltState::OscPass,
                    };
                }
            }
        }
    }
}

/// 計器: `BERTH_ATTACH_DUMP=<path>` が在れば、`berth attach` が端末へ書いたバイトをそのまま追記する。
/// 外側が ConPTY（Windows Terminal / pty 越しの検体）だと、届くのは描き直した後のバイトで、berth が何を
/// 書いたかはそこからは読めない。∴ 書く側で写す。無ければ何もしない（本番の経路に費用を足さない）。
pub(super) struct AttachDump(Option<std::fs::File>);

impl AttachDump {
    pub(super) fn from_env() -> Self {
        let f = std::env::var_os("BERTH_ATTACH_DUMP").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        Self(f)
    }
    pub(super) fn put(&mut self, bytes: &[u8]) {
        if let Some(f) = self.0.as_mut() {
            use std::io::Write as _;
            let _ = f.write_all(bytes);
        }
    }
    /// 生の録画（描き直す前）も `<path>.replay` に残す（描き直しの誤りと録画の中身を分離して確認するため）。
    pub(super) fn put_replay(&self, bytes: &[u8]) {
        if self.0.is_some() {
            if let Some(p) = std::env::var_os("BERTH_ATTACH_DUMP") {
                let mut p = std::path::PathBuf::from(p);
                p.as_mut_os_string().push(".replay");
                let _ = std::fs::write(p, bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AltScreenShim;

    fn filter(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = AltScreenShim::translating();
        let mut out = Vec::new();
        for c in chunks {
            f.feed(c, &mut out);
        }
        out
    }

    /// `?1049;1h` のように他の mode と並べて来た alt 切替も判定し、alt の分だけ翻訳して残りは通す。
    #[test]
    fn an_alt_switch_bundled_with_other_modes_is_still_translated() {
        let mut f = AltScreenShim::translating();
        let mut out = Vec::new();
        f.feed(b"\x1b[?1049;1h", &mut out);
        assert_eq!(f.depth, 1, "alt に入ったと数えていない");
        assert!(
            out.starts_with(AltScreenShim::CLEAR) || out.ends_with(AltScreenShim::CLEAR),
            "消去へ翻訳していない: {out:?}"
        );
        assert!(
            out.windows(5).any(|w| w == b"\x1b[?1h"),
            "一緒に来た `?1h` を落としている: {out:?}"
        );
        assert!(
            !out.windows(4).any(|w| w == b"1049"),
            "`1049` がそのまま端末へ出ている ＝ 部屋が壊れる: {out:?}"
        );
        let mut out = Vec::new();
        f.feed(b"\x1b[?25;1049l", &mut out);
        assert_eq!(f.depth, 0, "alt から出たと数えていない");
        assert!(out
            .windows(AltScreenShim::CLEAR.len())
            .any(|w| w == AltScreenShim::CLEAR));
        assert!(
            out.windows(6).any(|w| w == b"\x1b[?25l"),
            "一緒に来た `?25l` を落としている: {out:?}"
        );
        assert!(!out.windows(4).any(|w| w == b"1049"), "{out:?}");
    }

    /// 素通しの形でも、並んだ中の alt 切替を深さに数える（抜けるとき `?1049l` を出す判断に要る）。
    #[test]
    fn passing_through_counts_an_alt_switch_bundled_with_other_modes() {
        let mut f = AltScreenShim::passing_through();
        let mut out = Vec::new();
        f.feed(b"\x1b[?1049;1h", &mut out);
        assert_eq!(out, b"\x1b[?1049;1h", "素通しは綴りを変えない");
        assert_eq!(f.depth, 1);
        f.feed(b"\x1b[?1;1049l", &mut out);
        assert_eq!(f.depth, 0);
    }

    /// 部屋が無いときは alt 切替をそのまま通す（翻訳しない）が、深さは数える。
    #[test]
    fn passing_through_keeps_the_alt_switch_as_written_and_still_counts() {
        let mut f = AltScreenShim::passing_through();
        let mut out = Vec::new();
        f.feed(b"a\x1b[?1049hb\x1b[?2004h", &mut out);
        assert_eq!(out, b"a\x1b[?1049hb\x1b[?2004h");
        assert_eq!(f.depth, 1);
        f.feed(b"\x1b[?1049l", &mut out);
        assert_eq!(&out[out.len() - 8..], b"\x1b[?1049l");
        assert_eq!(f.depth, 0);
        // 深さ 0 の `l` もそのまま通す（端末が自分で no-op にする）。
        f.feed(b"\x1b[?1049l", &mut out);
        assert_eq!(&out[out.len() - 8..], b"\x1b[?1049l");
        assert_eq!(f.depth, 0);
    }

    /// 中のアプリが出す題名（OSC 0 / 2）は、こちらの名乗りを頭に付けて通す。
    /// 他の OSC はそのまま・終端の綴り（BEL / ST）も変えない・フレームの境目で割れても壊れない。
    #[test]
    fn inner_titles_are_prefixed_with_the_session_and_other_oscs_pass_untouched() {
        let mut f = AltScreenShim::passing_through().renaming_titles("berth: work".into());
        let mut out = Vec::new();
        f.feed(b"a\x1b]2;Claude Code\x07b", &mut out);
        assert_eq!(out, "a\x1b]2;berth: work · Claude Code\x07b".as_bytes());
        // ST 終端・OSC 0（icon も）・フレームの境目。
        out.clear();
        f.feed(b"\x1b]0;vi", &mut out);
        assert_eq!(out, b"", "終端を見るまで出さない");
        f.feed(b"m\x1b\\z", &mut out);
        assert_eq!(out, "\x1b]0;berth: work · vim\x1b\\z".as_bytes());
        // 空の題名（戻す）はこちらの名乗りだけ。
        out.clear();
        f.feed(b"\x1b]2;\x07", &mut out);
        assert_eq!(out, b"\x1b]2;berth: work\x07");
        // exe のパス（ConPTY の起動時の既定）も同じ扱い。
        out.clear();
        f.feed(b"\x1b]0;C:\\WINDOWS\\System32\\cmd.exe\x07", &mut out);
        assert_eq!(out, b"\x1b]0;berth: work\x07");
        // 題名でない OSC（ハイパーリンク・OSC 133）は 1 バイトも変えない。
        out.clear();
        f.feed(
            b"\x1b]8;;http://x\x1b\\t\x1b]8;;\x1b\\\x1b]133;A\x07",
            &mut out,
        );
        assert_eq!(out, b"\x1b]8;;http://x\x1b\\t\x1b]8;;\x1b\\\x1b]133;A\x07");
        // 本体の途中の ESC（ST でない）は本体として残る。
        out.clear();
        f.feed(b"\x1b]9;x\x1by\x07", &mut out);
        assert_eq!(out, b"\x1b]9;x\x1by\x07");
        // 名乗りが無ければ OSC は溜めない（素通し）。
        let mut g = AltScreenShim::passing_through();
        out.clear();
        g.feed(b"\x1b]2;Claude Code\x07", &mut out);
        assert_eq!(out, b"\x1b]2;Claude Code\x07");
    }

    /// 長すぎる OSC（OSC 52 の貼り付け等）は溜め込まず、終端まで素通しする。
    #[test]
    fn an_oversized_osc_is_passed_through_instead_of_buffered() {
        let mut f = AltScreenShim::passing_through().renaming_titles("berth: work".into());
        let mut out = Vec::new();
        let big = vec![b'A'; AltScreenShim::OSC_CAP + 100];
        let mut input = b"\x1b]52;c;".to_vec();
        input.extend_from_slice(&big);
        input.extend_from_slice(b"\x1b\\tail");
        f.feed(&input, &mut out);
        assert_eq!(out, input, "素通しなら 1 バイトも変わらない");
        // 素通しの後は普通に戻っている（題名の書き換えが効く）。
        out.clear();
        f.feed(b"\x1b]2;x\x07", &mut out);
        assert_eq!(out, b"\x1b]2;berth: work \xc2\xb7 x\x07");
    }

    /// 翻訳先。実装と同じ定数を見る（写すと、片方を直したときに黙って食い違う）。
    const CLEAR: &[u8] = AltScreenShim::CLEAR;

    #[test]
    fn entering_the_alt_screen_becomes_a_clear() {
        // 落とさず、画面クリアへ翻訳する。落とすと、中の全画面アプリがこちらの画面に直接描き、
        // 抜けるときの「元に戻せ」も落ちるので描画が残る。
        assert_eq!(filter(&[b"\x1b[?1049h"]), CLEAR);
        assert_eq!(filter(&[b"\x1b[?1047h"]), CLEAR);
        assert_eq!(filter(&[b"\x1b[?47h"]), CLEAR);
    }

    #[test]
    fn leaving_the_alt_screen_becomes_a_clear_when_it_was_entered() {
        let mut want = CLEAR.to_vec();
        want.extend_from_slice(b"a");
        want.extend_from_slice(CLEAR);
        want.extend_from_slice(b"b");
        assert_eq!(filter(&[b"\x1b[?1049h", b"a\x1b[?1049lb"]), want);
    }

    #[test]
    fn an_unmatched_leave_is_swallowed_without_clearing() {
        // 陰性対照（これが存在するため深さを数えている）: 入っていないのに `l` だけ来ることがある
        // （起動時に身構えて出すアプリ・`reset` の類）。そこで消すと利用者の画面を勝手に消す。
        // 素通しも出来ない（`berth` 自身の部屋が畳まれる）。∴ 黙って飲む。
        assert_eq!(filter(&[b"\x1b[?1049l"]), b"");
        assert_eq!(filter(&[b"a\x1b[?47lb"]), b"ab");
    }

    #[test]
    fn nested_enters_only_clear_at_the_boundary() {
        // 実端末でも、alt に居るときの 2 回目の `h` は何もしない。∴ 数えるのは 0⇄1 の境目だけ。
        let mut want = CLEAR.to_vec(); // 1 回目の h
        want.extend_from_slice(b"x"); // 2 回目の h は無音
        want.extend_from_slice(b"y"); // 内側の l も無音（深さ 2→1）
        want.extend_from_slice(CLEAR); // 外側の l で初めて消す（1→0）
        assert_eq!(
            filter(&[
                b"\x1b[?1049h",
                b"\x1b[?1049h",
                b"x",
                b"\x1b[?1049l",
                b"y",
                b"\x1b[?1049l",
            ]),
            want
        );
    }

    #[test]
    fn keeps_other_private_modes_and_normal_csi() {
        // bracketed paste / カーソル可視 / cursor keys は通す（alt-screen ではない）。
        assert_eq!(filter(&[b"\x1b[?2004h"]), b"\x1b[?2004h");
        assert_eq!(filter(&[b"\x1b[?25l\x1b[?1h"]), b"\x1b[?25l\x1b[?1h");
        // 普通の CSI（SGR）や本文はそのまま。
        assert_eq!(filter(&[b"\x1b[01;32mhi\x1b[0m"]), b"\x1b[01;32mhi\x1b[0m");
        assert_eq!(filter(&[b"plain text\r\n"]), b"plain text\r\n");
    }

    #[test]
    fn survives_sequences_split_across_frames() {
        // フレーム境界でシーケンスが割れても正しく翻訳する／通す。
        assert_eq!(filter(&[b"\x1b[?10", b"49h"]), CLEAR);
        assert_eq!(filter(&[b"\x1b", b"[?", b"47", b"l"]), b"");
        assert_eq!(filter(&[b"x\x1b[?20", b"04hy"]), b"x\x1b[?2004hy");
    }

    #[test]
    fn lone_esc_and_non_bracket_escapes_pass_through() {
        // `\e` の後が `[` 以外（`\eM` 等）は対象外のためそのまま通す。
        assert_eq!(filter(&[b"a\x1bMb"]), b"a\x1bMb");
    }
}
