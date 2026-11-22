use crate::USAGE;

/// 見るだけの attach を送れる berthd の版（`berth-protocol` の `Attach.read_only`）。
const READ_ONLY_SINCE: u32 = 7;

/// `berth attach` の引数（読んだまま・まだ環境や端末は見ていない）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct AttachArgs {
    pub(super) name: Option<String>,
    pub(super) read_only: bool,
    pub(super) shell: Option<String>,
    /// `--scrollback` は `Some(true)`、`--no-scrollback` は `Some(false)`、指定がなければ `None`（環境と端末で決める）。
    pub(super) scrollback: Option<bool>,
}

/// `berth attach` の引数を読む（純粋）。
///
/// 知らないフラグは名前にせず拒否する。余計な引数も黙って捨てずに拒否する。最初の引数をそのまま名前にすると、
/// `berth attach -r main` が `-r` という名前のシェルを新しく作って attach する。
pub(super) fn parse_attach_args(args: &[String]) -> Result<AttachArgs, String> {
    let mut name: Option<String> = None;
    let mut read_only = false;
    let mut shell: Option<String> = None;
    let mut scrollback: Option<bool> = None;
    let mut iter = args.iter();

    while let Some(a) = iter.next() {
        match a.as_str() {
            "-r" | "--read-only" => read_only = true,
            // 後に書いた方が勝つ（alias で `--no-scrollback` を焼いた人が 1 回だけ戻せる）。
            "--scrollback" => scrollback = Some(true),
            "--no-scrollback" => scrollback = Some(false),
            "-s" | "--shell" => {
                let val = iter.next().ok_or_else(|| {
                    format!("berth attach: `{a}` requires a shell path\n\n{USAGE}")
                })?;
                if val.starts_with('-') {
                    return Err(format!(
                        "berth attach: `{a}` requires a shell path, got option `{val}`\n\n{USAGE}"
                    ));
                }
                shell = Some(val.to_string());
            }
            s if s.starts_with("--shell=") => {
                let val = s.trim_start_matches("--shell=");
                if val.is_empty() {
                    return Err(format!(
                        "berth attach: `--shell=` requires a shell path\n\n{USAGE}"
                    ));
                }
                shell = Some(val.to_string());
            }
            s if s.len() > 1 && s.starts_with('-') => {
                return Err(format!("berth attach: unknown option `{s}`\n\n{USAGE}"));
            }
            s => {
                if let Some(first) = &name {
                    return Err(format!(
                        "berth attach: unexpected argument `{s}` (already attaching to `{first}`)"
                    ));
                }
                name = Some(s.to_string());
            }
        }
    }
    Ok(AttachArgs {
        name,
        read_only,
        shell,
        scrollback,
    })
}

/// `BERTH_SCROLLBACK` の綴り。値が `off` / `0` / `no` / `false`（大小無視）なら外す。
pub(super) const SCROLLBACK_ENV: &str = "BERTH_SCROLLBACK";

/// 外側が既にスクロールを持っている多重化ツールの印（この中では自動で外す）。
/// tmux は `TMUX`、zellij は `ZELLIJ`、GNU screen は `STY`。
const MULTIPLEXER_MARKS: [&str; 3] = ["TMUX", "ZELLIJ", "STY"];

/// 再接続の瞬間に録画を端末のスクロールへ流すかを判定する（純粋関数）。結果と理由を返す。
///
/// 既定は ON。外し方は 3 段:
/// 1. `--no-scrollback` / `--scrollback`（1 回限り。tmux の中で検証できるよう自動判定も上書きする）
/// 2. `BERTH_SCROLLBACK=off`（ずっと・画面 1 枚の形が好きな人）
/// 3. 自動: 外側が既にスクロールを持つ多重化ツールの中（流すと二重になる）、または stdout が端末でない場合
pub(super) fn scrollback_wanted(
    cli: Option<bool>,
    env: Option<&str>,
    inside_multiplexer: bool,
    stdout_is_tty: bool,
) -> (bool, &'static str) {
    if let Some(on) = cli {
        return (
            on,
            if on {
                "--scrollback"
            } else {
                "--no-scrollback"
            },
        );
    }
    if let Some(v) = env {
        if matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "no" | "false"
        ) {
            return (false, "BERTH_SCROLLBACK=off");
        }
    }
    if inside_multiplexer {
        return (false, "inside a multiplexer");
    }
    if !stdout_is_tty {
        return (false, "stdout is not a terminal");
    }
    (true, "default")
}

/// 多重化ツールの中か（環境の印で見る・`MULTIPLEXER_MARKS`）。
pub(super) fn inside_multiplexer() -> bool {
    MULTIPLEXER_MARKS
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// 注意: 古い berthd は `read_only` を読まずに操作側として接続する（未知のフィールドを黙って捨てるため）。
/// 見るだけのつもりの端末から打鍵が通る。∴ 送る前に断る。
pub(super) fn read_only_supported(protocol: u32) -> Result<(), String> {
    if protocol >= READ_ONLY_SINCE {
        return Ok(());
    }
    Err(format!(
        "berth attach -r: the running berthd is too old to watch (protocol {protocol} < {READ_ONLY_SINCE}) — \
         it would attach you as the operator, keys and all.\n\
         Replace it with `berth kill-server` (add -f to take live sessions down with it)."
    ))
}

#[cfg(test)]
mod tests {
    /// 既定は ON。外す順は 旗 → 環境変数 → 多重化ツールの中 → stdout が端末でない。
    #[test]
    fn scrollback_is_on_by_default_and_turned_off_in_three_ways() {
        use super::scrollback_wanted;
        assert_eq!(
            scrollback_wanted(None, None, false, true),
            (true, "default")
        );
        assert_eq!(
            scrollback_wanted(Some(false), None, false, true),
            (false, "--no-scrollback")
        );
        for v in ["off", "OFF", " 0 ", "no", "false"] {
            assert_eq!(
                scrollback_wanted(None, Some(v), false, true),
                (false, "BERTH_SCROLLBACK=off"),
                "{v:?}"
            );
        }
        // 知らない値は ON のまま（`on` / `1` / 空 …）。
        for v in ["on", "1", "", "yes"] {
            assert!(scrollback_wanted(None, Some(v), false, true).0, "{v:?}");
        }
        assert_eq!(
            scrollback_wanted(None, None, true, true),
            (false, "inside a multiplexer")
        );
        assert_eq!(
            scrollback_wanted(None, None, false, false),
            (false, "stdout is not a terminal")
        );
        // `--scrollback` は自動の判断も環境変数も押し切る（tmux の中で測るため）。
        assert_eq!(
            scrollback_wanted(Some(true), Some("off"), true, false),
            (true, "--scrollback")
        );
    }

    /// `berth attach` の引数。最初の引数をそのまま名前にすると、`berth attach -r main` が
    /// `-r` という名前のシェルを新しく作って attach する（エラーも出ない）。
    /// `-s` / `--shell` で起動するシェルを指定できる。
    #[test]
    fn attach_reads_r_as_a_flag_not_as_a_session_name() {
        use super::parse_attach_args;
        // ここの検体は 3 つ組で読む（名前・見るだけ・シェル）。
        let a = |v: &[&str]| {
            parse_attach_args(&v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
                .map(|p| (p.name, p.read_only, p.shell))
        };
        assert_eq!(
            a(&["-r", "main"]),
            Ok((Some("main".to_string()), true, None))
        );
        assert_eq!(
            a(&["main", "--read-only"]),
            Ok((Some("main".to_string()), true, None))
        );
        assert_eq!(a(&["main"]), Ok((Some("main".to_string()), false, None)));
        assert_eq!(a(&[]), Ok((None, false, None)));
        assert_eq!(a(&["-r"]), Ok((None, true, None)));
        // -s / --shell でシェルを指定できる。
        assert_eq!(
            a(&["-s", "hsh", "main"]),
            Ok((Some("main".to_string()), false, Some("hsh".to_string())))
        );
        assert_eq!(
            a(&["--shell", "/bin/zsh"]),
            Ok((None, false, Some("/bin/zsh".to_string())))
        );
        assert_eq!(
            a(&["--shell=/bin/bash", "-r", "main"]),
            Ok((
                Some("main".to_string()),
                true,
                Some("/bin/bash".to_string())
            ))
        );
        let e = a(&["-s"]).unwrap_err();
        assert!(e.contains("requires a shell path"), "{e}");
        let e = a(&["--shell"]).unwrap_err();
        assert!(e.contains("requires a shell path"), "{e}");
        let e = a(&["-s", "-r"]).unwrap_err();
        assert!(e.contains("got option `-r`"), "{e}");
        let e = a(&["--shell="]).unwrap_err();
        assert!(e.contains("requires a shell path"), "{e}");
        // 知らない旗は名前にせず断る（`-x` という名前のシェルを作らない）。
        let e = a(&["-x", "main"]).unwrap_err();
        assert!(e.contains("-x"), "{e}");
        // 余計な名前も断る（黙って 2 つ目を捨てない）。
        let e = a(&["one", "two"]).unwrap_err();
        assert!(e.contains("two"), "{e}");
    }

    /// `--scrollback` / `--no-scrollback` は旗であって名前ではない。無ければ `None`（環境で決める）。
    #[test]
    fn attach_reads_the_scrollback_flags_and_leaves_the_default_to_the_environment() {
        use super::parse_attach_args;
        let a = |v: &[&str]| {
            parse_attach_args(&v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
                .map(|p| (p.name, p.scrollback))
        };
        assert_eq!(a(&["main"]), Ok((Some("main".to_string()), None)));
        assert_eq!(
            a(&["--no-scrollback", "main"]),
            Ok((Some("main".to_string()), Some(false)))
        );
        assert_eq!(
            a(&["main", "--scrollback"]),
            Ok((Some("main".to_string()), Some(true)))
        );
        // 後に書いた方が勝つ。
        assert_eq!(
            a(&["--no-scrollback", "--scrollback"]),
            Ok((None, Some(true)))
        );
    }

    /// 古い berthd は `read_only` を読まずに操作側として扱う（キー入力も通ってしまう）ため、送信前に拒否する。
    #[test]
    fn a_read_only_attach_is_refused_before_protocol_7() {
        use super::read_only_supported;
        assert!(read_only_supported(7).is_ok());
        let e = read_only_supported(6).unwrap_err();
        assert!(e.contains("protocol 6"), "{e}");
    }
}
