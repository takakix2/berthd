//! どのシェルを起こすか（client が決める分と、berthd の既定）。

// ══════════════════════════════════════════════════════════════════════
// どのシェルを起こすか: 値と、その値を誰が決めたか
// ══════════════════════════════════════════════════════════════════════
//
// 解決するのは `berth`（client）で、決めた値と出所を `Spawn` に載せる。berthd が自分の環境を
// 読んで決める形だと、最初に berthd を起こした端末の環境がデーモンに残り、以後どの端末から
// attach してもそのシェルが出る。
//
// `$SHELL` では足りない。bash から別のシェルを起こした端末でも `$SHELL` は `/bin/bash` のまま
// （`$SHELL` は「ログインシェルに何を選んだか」の申告値で、走っている物ではない）。
// ∴ 親プロセスを観測する（身元はまず観測し、自称は隣に置く）。
//
// 重要: 親がシェルとは限らない（エディタや AI エージェントの下で撃てば親はそのプロセス、
// `tmux new berth …` なら `tmux`）。∴ `/etc/shells` に載っている物だけ採る。名簿を手で書かない。
// 照合は basename で行う。同じシェルが `/etc/shells` に載る場所と PATH が引く場所で違うことがあり、
// パス全体の一致では弾かれる。

/// そのシェルを誰が決めたか。台帳の `shell_src` になる。
///
/// 値だけでは再現できない。`$SHELL` だったのか `BERTHD_SHELL` だったのかが分からないと、
/// 「なぜこのシェルなのか」を調べるのにデーモンの `environ` を読む羽目になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellSrc {
    /// `--shell` / `-s`: CLI 引数の明示。すべてに勝つ最優先。
    CliFlag,
    /// `BERTHD_SHELL`: 明示。面に依らず、CLI 引数の次に強い。
    BerthdShell,
    /// `~/.berth/config.toml` の `default-shell`: 書いた既定。環境より弱く、観測より強い。
    Config,
    /// 観測: `berth` を打った端末で走っていたシェル（親プロセス）。
    ParentShell,
    /// `$SHELL`: ログインシェルの申告値（unix のみ）。
    EnvShell,
    /// `/etc/passwd` のログインシェル（`getpwuid`・unix のみ）。`$SHELL` も無いときの最後の観測。
    Passwd,
    /// 何も無かったので面ごとの既定（unix: `/bin/bash` か `/bin/sh` の在る方 / `powershell.exe`）。
    Fallback,
}

impl ShellSrc {
    /// 台帳とワイヤに載る綴り。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CliFlag => "cli_flag",
            Self::BerthdShell => "berthd_shell",
            Self::Config => "config",
            Self::ParentShell => "parent_shell",
            Self::EnvShell => "env_shell",
            Self::Passwd => "passwd",
            Self::Fallback => "fallback",
        }
    }
}

/// どの面の規則で解くか。
///
/// 重要: `cfg!` で分岐させない。埋めてしまうと、片方の面は検体から永久に見えなくなる。
/// 引数に取れば、Linux の上で Windows の規則も検体にできる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Unix,
    Windows,
}

impl Face {
    /// いま走っている面。呼ぶのは端（`default_shell` / `client_shell`）だけにする。
    #[must_use]
    pub const fn here() -> Self {
        if cfg!(unix) {
            Self::Unix
        } else {
            Self::Windows
        }
    }
}

/// `/etc/shells` の中身を綴りの一覧にする（コメントと空行を落とす）。
///
/// ここは純粋関数であり、本物の `/etc/shells` は環境ごとに違うので、文字列を渡して検体にできるようにしてある。
#[must_use]
pub fn parse_login_shells(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// システムが「ログインシェル」と認めている物の一覧。
///
/// Windows には無い（`/etc/shells` という機構自体が無い）ため空。空なら
/// [`looks_like_a_login_shell`] は必ず偽になるので、あちらに面の分岐が要らない。
#[must_use]
pub fn login_shells() -> Vec<String> {
    std::fs::read_to_string("/etc/shells").map_or_else(|_| Vec::new(), |t| parse_login_shells(&t))
}

/// `path` は、システムがログインシェルと認めている物か。
///
/// 注意: 照合は basename で行う。同じシェルが別の場所に置かれていることが普通に在る
/// （`/etc/shells` は `~/.local/bin/` の下・PATH が引くのは `~/.cargo/bin/` の下、など）。
/// それでも名前だけの名簿は作らない。名簿は `/etc/shells` が持ち、こちらは照合の規則だけ。
#[must_use]
pub fn looks_like_a_login_shell(path: &str, login_shells: &[String]) -> bool {
    let base = |s: &str| {
        s.rsplit(['/', '\\'])
            .next()
            .unwrap_or(s)
            .to_ascii_lowercase()
    };
    let wanted = base(path);
    !wanted.is_empty() && login_shells.iter().any(|s| base(s) == wanted)
}

/// いま走っているシェル（自分の親プロセスの実行ファイル）。
///
/// 重要: 綴りは「名前」ではなく「パス」でなければならない。起こす側は `CommandBuilder::new()` に
/// 渡すので、名前だけだと berthd の `PATH` で解決され直す（別の場所の物を掴みうる）。
/// 注意: Linux の `/proc/<pid>/exe` は、消されたバイナリに ` (deleted)` を付けて答える。
/// その綴りは起動に使えないので捨てる。
#[cfg(target_os = "linux")]
#[must_use]
pub fn running_shell_path() -> Option<String> {
    // SAFETY: `getppid` は引数を取らず、失敗しない（POSIX）。
    let ppid = unsafe { libc::getppid() };
    let path = std::fs::read_link(format!("/proc/{ppid}/exe")).ok()?;
    let path = path.to_str()?;
    if path.ends_with(" (deleted)") {
        return None;
    }
    Some(path.to_string())
}

/// macOS: `proc_pidpath`（`peer_ancestry` の `proc_pidinfo` と同じ棚の API）。
///
/// `pbi_comm` では足りない。あちらは名前で、しかも 16 バイトで切られる
/// （台帳には十分だが、起こすには使えない）。
#[cfg(target_os = "macos")]
#[must_use]
pub fn running_shell_path() -> Option<String> {
    // SAFETY: `getppid` は引数を取らず、失敗しない（POSIX）。
    let ppid = unsafe { libc::getppid() };
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` は `len` バイトの書き込み先として有効。
    let n = unsafe {
        libc::proc_pidpath(
            ppid,
            buf.as_mut_ptr().cast(),
            u32::try_from(buf.len()).ok()?,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(usize::try_from(n).ok()?);
    String::from_utf8(buf).ok()
}

/// Windows とその他の unix: まだ書いていない。
/// 当てずっぽうを返さない（`peer_ancestry` の未実装と同じ規律）。
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn running_shell_path() -> Option<String> {
    None
}

/// 空・空白だけの値は「言わなかった」と同じ（環境変数の常識に合わせる）。
pub(crate) fn named(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// client 側の解決（`berth` が使う・純粋）。
///
/// 返すのは「berthd に名指しする値」で、`None` は何も言わない（berthd の既定に任せる）の意。
///
/// | 順 | 何を見るか | 出所 |
/// |---|---|---|
/// | 1 | `--shell` / `-s` | `cli_flag`（CLI オプション・最優先・面に依らない）|
/// | 2 | `BERTHD_SHELL` | `berthd_shell`（明示が慣習に勝つ・面に依らない）|
/// | 3 | `~/.berth/config.toml` の `default-shell`（面に依らない）| `config` |
/// | 4 | 親プロセスの実行ファイルが `/etc/shells` に載っていれば | `parent_shell` |
/// | 5 | `$SHELL`（unix のみ）| `env_shell` |
/// | 6 | 何も無い | `None`（berthd の `default_shell()` へ）|
///
/// 重要: Windows で `$SHELL` を見てはいけない。git-bash は `SHELL=/bin/bash.exe` を立てるが、
/// あれは MSYS の綴りで `CreateProcessW` から解決しない（os error 3）。
/// 名簿（`login_shells`）は Windows では空なので、4 も自然に外れる。つまり
/// あの面で効くのは `--shell`・`BERTHD_SHELL`・設定ファイルだけ。
#[must_use]
pub fn resolve_client_shell(
    face: Face,
    cli_shell: Option<&str>,
    berthd_shell: Option<&str>,
    config_shell: Option<&str>,
    parent: Option<&str>,
    login_shells: &[String],
    env_shell: Option<&str>,
) -> Option<(String, ShellSrc)> {
    if let Some(v) = named(cli_shell) {
        return Some((v.to_string(), ShellSrc::CliFlag));
    }
    if let Some(v) = named(berthd_shell) {
        return Some((v.to_string(), ShellSrc::BerthdShell));
    }
    if let Some(v) = named(config_shell) {
        return Some((v.to_string(), ShellSrc::Config));
    }
    if let Some(v) = named(parent).filter(|p| looks_like_a_login_shell(p, login_shells)) {
        return Some((v.to_string(), ShellSrc::ParentShell));
    }
    if face == Face::Unix {
        if let Some(v) = named(env_shell) {
            return Some((v.to_string(), ShellSrc::EnvShell));
        }
    }
    None
}

/// daemon 側の解決（`default_shell` の中身・純粋）。
///
/// 注意: ここに親の観測は入らない。berthd の親は「最初に起こした端末」であって、
/// いま頼んできた人ではない。
///
/// unix の最後の手を `/bin/bash` 決め打ちにしない。bash の無い環境（Alpine・NixOS・最小コンテナ）では
/// spawn が ENOENT で落ちる。`$SHELL` の次に `/etc/passwd` のログインシェル（`passwd_shell`）を見て、
/// それも無ければ `/bin/bash` → `/bin/sh` のうち在る物。どれも無ければ `/bin/sh` を名指しして、
/// spawn の失敗（`spawn:failed`・ENOENT）にそう言わせる。黙って別の物を起こさない。
/// `exists` は「その綴りが在るか」（純粋に保つため呼び手が渡す・検体は閉包で環境を作る）。
#[must_use]
pub fn resolve_default_shell(
    face: Face,
    berthd_shell: Option<&str>,
    env_shell: Option<&str>,
    passwd_shell: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
) -> (String, ShellSrc) {
    if let Some(v) = named(berthd_shell) {
        return (v.to_string(), ShellSrc::BerthdShell);
    }
    if face == Face::Unix {
        if let Some(v) = named(env_shell) {
            return (v.to_string(), ShellSrc::EnvShell);
        }
        if let Some(v) = named(passwd_shell).filter(|p| exists(p)) {
            return (v.to_string(), ShellSrc::Passwd);
        }
        let last = ["/bin/bash", "/bin/sh"]
            .into_iter()
            .find(|p| exists(p))
            .unwrap_or("/bin/sh");
        return (last.to_string(), ShellSrc::Fallback);
    }
    // `ComSpec` は見ない（`cmd.exe` に落ちる）。`$SHELL` も見ない（`resolve_client_shell` の doc）。
    ("powershell.exe".to_string(), ShellSrc::Fallback)
}

/// いまの uid の `/etc/passwd` のログインシェル（`getpwuid_r`）。無い／読めない／空なら `None`。
///
/// Windows には無い（`None`）。`$SHELL` より後に見る。あちらは「選んだ」の申告で、
/// こちらは「システムが持っている」値。申告が在るならそれを尊重する。
#[must_use]
pub fn passwd_shell() -> Option<String> {
    #[cfg(unix)]
    {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 4096];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: バッファは自分の物で、`getpwuid_r` はその中へ書く。`out` が null なら読まない。
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &raw mut pw,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &raw mut out,
            )
        };
        if rc != 0 || out.is_null() || pw.pw_shell.is_null() {
            return None;
        }
        // SAFETY: `pw_shell` は `buf` の中の NUL 終端文字列（`getpwuid_r` の契約）。
        let shell = unsafe { std::ffi::CStr::from_ptr(pw.pw_shell) };
        let s = shell.to_str().ok()?.trim();
        (!s.is_empty()).then(|| s.to_string())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// CLI オプション付きで client 側のシェルを解決する。
///
/// `cli_shell` は `--shell` / `-s` で渡された値。`None` なら CLI 指定なしとして
/// 次の優先順位（`BERTHD_SHELL` → 設定ファイル → 親プロセス → `$SHELL`）へ落ちる。
/// `config_shell` は `~/.berth/config.toml` の `default-shell`（読むのは呼び手）。
#[must_use]
pub fn client_shell_with(
    cli_shell: Option<&str>,
    config_shell: Option<&str>,
) -> Option<(String, ShellSrc)> {
    resolve_client_shell(
        Face::here(),
        cli_shell,
        std::env::var("BERTHD_SHELL").ok().as_deref(),
        config_shell,
        running_shell_path().as_deref(),
        &login_shells(),
        std::env::var("SHELL").ok().as_deref(),
    )
}

/// `berth` が `Control::Spawn` に載せる値と出所。`None` は何も言わないことを意味する。
#[must_use]
pub fn client_shell() -> Option<(String, ShellSrc)> {
    client_shell_with(None, None)
}

/// client が shell を指定しなかったときに起こす物。
///
/// # ここが正本
///
/// 重要: 検体もこの関数を通すこと。`/bin/sh` を検体に直書きすると、製品の規則と検体の前提が
/// 別々に古くなる。
///
/// # `$SHELL` は Windows で使えない
///
/// git-bash は必ず `SHELL=/bin/bash.exe` を立てるが、あれは MSYS の綴りで `CreateProcessW` からは
/// 解決しない。`portable_pty::CommandBuilder` に渡すと `/bin/bash.exe` も `/bin/bash` も os error 3
/// （指定されたパスが見つかりません）で落ち、`powershell.exe` / `pwsh.exe` / `cmd.exe` は起きる
/// （存在しない名前は os error 2 で区別できる）。
/// ∴ Windows で `$SHELL` を見ると、git-bash から berthd を起こした人が全員落ちる。
///
/// # 切り替えは `BERTHD_SHELL`（面に依らない）
///
/// `$SHELL` より先に見る。あちらは「その人がログインシェルに何を選んだか」の慣習で、
/// こちらは「berthd に何を起こさせたいか」の明示。明示が慣習に勝つ。
///
/// # ここが効く場面は狭い
///
/// 注意: 読むのはデーモンの環境で、`berth` を打った端末のではない。`berth` はデーモンが居なければ
/// 自分で起こすので、ここだけで決めると最初に起こした端末の環境が以後のすべての attach に効く。
/// ∴ `berth` は自分の側で解決して名指しする（[`client_shell`]）。ここが効くのは client が何も
/// 言わなかったときだけである:
/// 1. `BERTHD_SHELL` も `$SHELL` も無く、親もシェルでない所から打たれた（例: cron）
/// 2. 名指ししない client が繋いできた（古い `berth` / 自作の client）。
///
/// それでもここが既定の正本であることは変わらない。
#[must_use]
pub fn default_shell() -> String {
    default_shell_with_src().0
}

/// [`default_shell`] と同じ決め方で、値と一緒に出所も返す（台帳が書く）。
///
/// 重要: 2 通りに解かないこと。値と出所が別々の規則で決まると、台帳が
/// 「起きた物」と「起きた理由」で別のことを言う。
#[must_use]
pub fn default_shell_with_src() -> (String, ShellSrc) {
    resolve_default_shell(
        Face::here(),
        std::env::var("BERTHD_SHELL").ok().as_deref(),
        std::env::var("SHELL").ok().as_deref(),
        passwd_shell().as_deref(),
        &|p| std::path::Path::new(p).exists(),
    )
}
