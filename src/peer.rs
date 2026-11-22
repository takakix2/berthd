//! 接続の向こうに居るプロセス: 身元（peer credential / 監査セッション id）と親の並び。

use super::*;

/// unix の口の向こうに居るプロセスを**カーネルに訊く**（`SO_PEERCRED` ＋ 監査セッション id）。
///
/// `peer_from_pipe`（Windows）の対で、同じ 4 欄を返す。ただし `session` の意味は面で違う。
/// Windows は Terminal Services の区画、unix は**監査セッション id**。
/// どちらの番号かは `src` が名乗る（`proc_sessionid` / `local_peertoken` / `named_pipe`）。
///
/// # `sock` が要る理由
///
/// 監査セッション id が何に付いているかが、面で違う。
///
/// | 面 | 何に付いているか | 何を渡せば引けるか |
/// |---|---|---|
/// | Linux | **プロセス**（`/proc/<pid>/sessionid`）| `pid` |
/// | macOS | **接続**（`LOCAL_PEERTOKEN`: カーネルが connect 時に控えた `audit_token_t`）| socket |
///
/// macOS には pid から辿る道が無い（`getaudit_addr()` は自プロセス専用）ので、口そのものを渡す。
/// `sock` が `Option` なのは検体のため（呼び手は必ず渡せる）。
///
/// # なぜこの番号か
///
/// 監査セッション id はログイン時に張られ、子孫すべてに継承される（ssh のログインごとに別の番号）。
/// ∴ ssh 配下で走った `berth` も同じ値を持ち、シェルの側が自分の記録に同じ番号を書いていれば
/// （hsh の `session:start` の `os_sessionid`）、2 つの記録を join できる。
///
/// # これは「相手を見た」証拠ではない
///
/// 同じログインセッションの中では berthd 自身も同じ値を持つ。「相手を見た」を言うのは `peer_pid` で、
/// この番号が効くのは ssh の一跳びを跨いだとき。値の一致を「別プロセスだ」の根拠に使わないこと。
///
/// # 取れない理由は `src` が名乗る
///
/// | `src` | 面 | 意味 |
/// |---|---|---|
/// | `so_peercred+proc_sessionid` | Linux | 両方取れた |
/// | `so_peercred+proc_sessionid:unset` | Linux | 機構は在るが PAM を通っていない（`u32::MAX` の番兵・cron / コンテナ）|
/// | `so_peercred+unavailable:proc_sessionid` | Linux | 読めなかった（`/proc` が無い、または相手が既に消えた）|
/// | `so_peercred+local_peertoken` | macOS | 両方取れた |
/// | `so_peercred+local_peertoken:unset` | macOS | 機構は在るが監査セッションに属していない（`AU_DEFAUDITSID` の番兵）|
/// | `so_peercred+unavailable:local_peertoken` | macOS | 訊けなかった（相手が既に消えていると `ENOTCONN`）|
/// | `so_peercred+unimplemented:peer_audit_session` | その他 unix | この面には実装が無い（BSD / iOS / Android）|
/// | `unavailable:so_peercred` | 全 unix | pid すら取れないため引く先が無い |
///
/// 注意: `unavailable:so_peercred` は macOS では厳密には早すぎる（番号は接続に付いているので、pid が
/// 取れなくても `LOCAL_PEERTOKEN` は答えうる）。その形を作って確かめられていないので、道を足していない。
#[cfg(unix)]
#[must_use]
pub fn peer_from_cred(
    uid: Option<u32>,
    pid: Option<i64>,
    sock: Option<std::os::fd::BorrowedFd<'_>>,
) -> Peer {
    let Some(pid) = pid else {
        // pid が無ければ引く先が無い。uid だけ取れる形は `SO_PEERCRED` では起きないが、
        // 取れたなら落とさない（観測は捨てない）。
        return Peer {
            uid,
            pid: None,
            session: None,
            auid: None,
            src: Some("unavailable:so_peercred"),
        };
    };
    let (session, auid, session_src) = peer_audit_session(pid, sock);
    Peer {
        uid,
        pid: Some(pid),
        session,
        auid,
        // 面ごとの綴りを、ここ 1 箇所で `so_peercred+` に前置する。注意: ここに無い綴りは
        // `unimplemented:` に化けるが、面ごとの陽性対照の検体がそれを赤にする。
        src: Some(match session_src {
            "proc_sessionid" => "so_peercred+proc_sessionid",
            "proc_sessionid:unset" => "so_peercred+proc_sessionid:unset",
            "unavailable:proc_sessionid" => "so_peercred+unavailable:proc_sessionid",
            "local_peertoken" => "so_peercred+local_peertoken",
            "local_peertoken:unset" => "so_peercred+local_peertoken:unset",
            "unavailable:local_peertoken" => "so_peercred+unavailable:local_peertoken",
            _ => "so_peercred+unimplemented:peer_audit_session",
        }),
    }
}

/// Linux: 監査セッション id は `/proc/<pid>/sessionid` に在る（世界読み取り可）。
///
/// 同じ場所の `loginuid` から auid も取る。返り値は `(監査セッション id, auid, 出所)`。
///
/// 重要: **`u32::MAX` は値ではなく「未設定」の番兵**（`init(1)` がこれを返す）。そのまま台帳に書くと
/// 42 億番のセッションが在ったことになる。auid の番兵も同じ。
///
/// 注意: 「読めなかった」と「読めたが未設定」を分ける。前者は機構が無い（`/proc` 無し・古いカーネル）か
/// 相手が既に消えた、後者は PAM を通っていない。
#[cfg(target_os = "linux")]
pub(crate) fn peer_audit_session(
    pid: i64,
    _sock: Option<std::os::fd::BorrowedFd<'_>>,
) -> (Option<u32>, Option<u32>, &'static str) {
    let auid = std::fs::read_to_string(format!("/proc/{pid}/loginuid"))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|&v| v != u32::MAX);
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/sessionid")) else {
        return (None, auid, "unavailable:proc_sessionid");
    };
    match text.trim().parse::<u32>() {
        Ok(u32::MAX) => (None, auid, "proc_sessionid:unset"),
        Ok(v) => (Some(v), auid, "proc_sessionid"),
        // 数を答えなかったため、機構が答えていない（「未設定」とは別）。
        Err(_) => (None, auid, "unavailable:proc_sessionid"),
    }
}

/// macOS: 監査セッション id は **接続**に付いている（`LOCAL_PEERTOKEN`）。
///
/// pid からは辿れない（`getaudit_addr()` は自プロセス専用）。代わりに、カーネルが connect の時点で
/// 控えた `audit_token_t` を口から貰う（`SO_PEERCRED` と同じ「カーネルに訊く」層で、client の自称ではない）。
///
/// この口の性質（確かめてある）:
///
/// - 返るのは**相手の**番号で、自分の値は漏れない。相手の側で `getaudit_addr()` が返す値と一致する。
/// - `socketpair` にも答える（相手が自分のため自分の `asid` になる）ので、ssh 無しで検体が書ける。
/// - 相手が既に消えた後は `ENOTCONN` となり「読めなかった」と名乗れる（Linux の `/proc` 消失と同じ形）。
///
/// 注意: `AU_DEFAUDITSID`（`0`）は値ではなく「監査セッションに属していない」の番兵。Linux の
/// `u32::MAX` と同じ扱いにする。`AU_ASSIGN_ASID`（`-1`）を含め、`0` 以下はすべて未設定
/// （`au_asid_t` は `pid_t` で符号付き）。これはヘッダの宣言に従った扱いで、`0` を返す相手を
/// 作って確かめてはいない。
#[cfg(target_os = "macos")]
pub(crate) fn peer_audit_session(
    _pid: i64,
    sock: Option<std::os::fd::BorrowedFd<'_>>,
) -> (Option<u32>, Option<u32>, &'static str) {
    use std::os::fd::AsRawFd;

    // BSM の `audit_token_t` は `u_int32_t val[8]`。並びを自分で仮定しない。
    // 取り出しは文書化された `audit_token_to_asid()` に任せる（libbsm は macOS 標準）。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AuditToken {
        val: [u32; 8],
    }
    #[link(name = "bsm")]
    extern "C" {
        fn audit_token_to_asid(atoken: AuditToken) -> libc::pid_t;
        // 同じトークンから auid も取る（`audit_token_to_asid` の隣人）。
        fn audit_token_to_auid(atoken: AuditToken) -> libc::uid_t;
    }

    let Some(sock) = sock else {
        // 呼び手が口を渡さなかったため訊く相手が無い。「取れなかった」と同じ綴りで返すが、
        // 実運用では起きない（接続を受ける側は必ず渡す）。
        return (None, None, "unavailable:local_peertoken");
    };
    let mut tok = AuditToken { val: [0; 8] };
    let mut len = std::mem::size_of::<AuditToken>() as libc::socklen_t;
    // SAFETY: `tok` は `LOCAL_PEERTOKEN` が求める大きさの `audit_token_t` そのもので、
    // `len` にその大きさを渡している。fd は `BorrowedFd` で、呼び出し中は生きている。
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            std::ptr::addr_of_mut!(tok).cast(),
            &raw mut len,
        )
    };
    // 短く答えた場合も「取れなかった」。半分だけ埋まった token から番号を作らない。
    if rc != 0 || len as usize != std::mem::size_of::<AuditToken>() {
        return (None, None, "unavailable:local_peertoken");
    }
    // SAFETY: 上で全欄が埋まったことを確かめた `audit_token_t` を値渡しするだけ。
    let asid = unsafe { audit_token_to_asid(tok) };
    // SAFETY: 同上（同じトークンを値渡しするだけ）。
    // 注意: `AU_DEFAUDITID`（`(uid_t)-1` すなわち `u32::MAX`）は「未設定」の番兵。値として書かない。
    let auid = Some(unsafe { audit_token_to_auid(tok) }).filter(|&v| v != u32::MAX);
    if asid <= 0 {
        // `0` = `AU_DEFAUDITSID` / `-1` = `AU_ASSIGN_ASID` / それ以外の負値はセッションたりえない。
        (None, auid, "local_peertoken:unset")
    } else {
        (Some(asid.unsigned_abs()), auid, "local_peertoken")
    }
}

/// Linux / macOS 以外の unix（BSD / iOS / Android）: まだ書いていない。
/// `None` を黙って返さず、`src` で「未実装」と名乗る（[`peer_from_cred`] の表）。
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn peer_audit_session(
    _pid: i64,
    _sock: Option<std::os::fd::BorrowedFd<'_>>,
) -> (Option<u32>, Option<u32>, &'static str) {
    (None, None, "unimplemented:peer_audit_session")
}

// ── 親プロセスの並び（Rheon Protocol `spec/ACTOR_v1.md` §2.4）──────────────
//
// 設定ゼロで「どの道具が繋いできたか」が読める。名前はプロセスの側に在る（Claude Code は
// `comm=claude`、codex は `codex`）。berthd は分類しない（道具の一覧を持たないため腐らない）。
// 生の名前を並べ、読む側が分ける。
// ssh 越しなら並びに `[priv]` の sshd の pid が載るため、sshd が既定で書くログ
// （`Accepted publickey … sshd[<pid>]`）と結べる。

/// 並びの深さの上限（当のプロセスを含む）。
pub const ANCESTRY_DEPTH: usize = 8;

/// 親プロセスの並びの 1 段。名前だけ（引数は秘密が混ざるので持たない）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: i64,
    /// comm（Linux は `/proc/<pid>/stat` の括弧の中・macOS は `pbi_comm`）。
    /// 注意: exe のファイル名は使わない。版番号がファイル名の道具が在る（Claude Code は `…/versions/<版>`）。
    pub name: String,
}

/// attach してきたプロセスから上へ辿った並びと、その出所。
#[derive(Clone, Debug)]
pub struct Ancestry {
    /// 0 番目が当のプロセス、以降が親。
    pub chain: Vec<Ancestor>,
    /// どこで止まったか / なぜ読めなかったか（[`Peer::src`] と同じ規律。
    /// 空欄だと「この面では取れない」と「古いバイナリ」が同じ顔になる）。
    pub src: &'static str,
}

/// 親を 1 段**読めなかった理由**。
///
/// 重要: `ESRCH` と `EPERM` を混ぜないこと。居ない pid は `ESRCH`、root の持ち物（macOS の
/// `login(1)` など）は `EPERM`。カーネルは区別して答えているので、1 つに畳まない。
/// 語彙の正本は Rheon Protocol `spec/ACTOR_v1.md` §2.4。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unread {
    /// そのプロセスが**居ない**（`ESRCH` / `NotFound`）。本当に消えた。
    Gone,
    /// 居るが、読むことを**断られた**（`EPERM` / `EACCES`）。
    Denied,
    /// それ以外（機構が失敗した、または読めたが解せない（`/proc` の行が壊れている 等））。
    Unreadable,
}

/// 辿るのをやめた理由。止まり方で意味が違うので `src` に残す。
///
/// 注意: [`Unread`] と 1 対 1 ではない。同じ「読めなかった」が、0 段目（当のプロセス）なら
/// [`Self::SelfUnreadable`]、途中なら理由ごとに割れる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AncestryEnd {
    /// pid 1（または 0）の手前まで辿り切った。
    Root,
    /// [`ANCESTRY_DEPTH`] で打ち切った（その上にまだ親が居る）。
    DepthLimit,
    /// 途中の親が**居なかった**。辿っている間に消えた（`ESRCH`）。
    Vanished,
    /// 途中の親を読むことを**断られた**（`EPERM`）。居るのに読めない。
    Denied,
    /// 途中の親が**それ以外の理由**で読めなかった（理由を名乗れない、を名乗る）。
    Unreadable,
    /// 当のプロセスすら読めなかった（理由は問わず、並びが空で辿る先が無い）。
    SelfUnreadable,
}

/// `/proc/<pid>/stat` の 1 行から `(comm, ppid)` を取る（純粋・I/O なし）。
///
/// 重要: **最後の `)` の後ろから欄を数える**。comm には空白も括弧も入りうる
/// （`tmux: server` を素朴に `split_whitespace` すると ppid を取り違える）。
#[must_use]
pub fn parse_proc_stat(line: &str) -> Option<(String, i64)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }
    let name = line[open + 1..close].to_string();
    let mut rest = line[close + 1..].split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((name, ppid))
}

/// `/proc/<pid>/stat` の 1 行から **starttime**（22 欄目・起動からの clock tick）を取る（純粋）。
///
/// 比べるだけなので秒に直さない（`CLK_TCK` を引く理由が無い）。欄の数え方は [`parse_proc_stat`] と同じで、
/// 最後の `)` の後ろから数えるため、comm に空白や括弧が入っても欄がずれない。`)` の後ろは state が 3 欄目なので、
/// starttime は 0 始まりで 19 番目。
#[must_use]
pub fn parse_proc_stat_start(line: &str) -> Option<i64> {
    let close = line.rfind(')')?;
    line[close + 1..].split_whitespace().nth(19)?.parse().ok()
}

/// Linux: そのプロセスの **starttime**（pid 再利用の検め・[`rejecting_recycled_pids`] の `created`）。
/// 読めなければ `None`（言えない: 検めない・嘘の証拠にしない）。
#[cfg(target_os = "linux")]
pub(crate) fn created_at(pid: i64) -> Option<i64> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|line| parse_proc_stat_start(&line))
}

/// macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` の `pbi_start_tvsec` / `tvusec`。
///
/// 注意: この長い版は root の持ち物を `EPERM` で断るが、それでよい。読めなければ `None` で
/// 検めないだけで、並びは [`read_parent`]（短い版）が今までどおり辿る。短い版には生成時刻が無い。
#[cfg(target_os = "macos")]
pub(crate) fn created_at(pid: i64) -> Option<i64> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `proc_bsdinfo` は整数と文字配列だけの C 構造体で、全 0 は有効な値。
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` は `size` バイトの書き込み先として有効。
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    let us = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
    i64::try_from(us).ok()
}

/// `start` から親へ辿る（純粋関数。「pid → (名前, 親の pid)」を引数で貰うので `/proc` 無しで検体が書ける）。
///
/// pid 1 は含めない（全プロセスの祖先で情報が無い）。重要: **`sshd` では止めない**。
/// ログに出る pid はセッションのすぐ上ではなく、その 1 つ上の `[priv]`（どちらも comm は `sshd`）。
pub fn walk_ancestry(
    start: i64,
    mut read: impl FnMut(i64) -> Result<(String, i64), Unread>,
) -> (Vec<Ancestor>, AncestryEnd) {
    let mut chain = Vec::new();
    let mut pid = start;
    loop {
        if chain.len() == ANCESTRY_DEPTH {
            return (chain, AncestryEnd::DepthLimit);
        }
        let (name, ppid) = match read(pid) {
            Ok(v) => v,
            Err(why) => {
                // 0 段目は理由で割らない。並びが空なら辿る先が無いので、
                // 「なぜ読めなかったか」は次に読む物の選択を変えない。
                let end = if chain.is_empty() {
                    AncestryEnd::SelfUnreadable
                } else {
                    match why {
                        Unread::Gone => AncestryEnd::Vanished,
                        Unread::Denied => AncestryEnd::Denied,
                        Unread::Unreadable => AncestryEnd::Unreadable,
                    }
                };
                return (chain, end);
            }
        };
        chain.push(Ancestor { pid, name });
        if ppid <= 1 {
            return (chain, AncestryEnd::Root);
        }
        pid = ppid;
    }
}

/// attach してきたプロセス（`peer_pid`）の親プロセスの並び。
///
/// 注意: attach のその瞬間に呼ぶこと。相手が終われば pid は再利用される。
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[must_use]
pub fn peer_ancestry(pid: Option<i64>) -> Ancestry {
    let Some(pid) = pid else {
        return Ancestry {
            chain: Vec::new(),
            src: "unavailable:peer_pid",
        };
    };
    // Windows と同じ検め（`rejecting_recycled_pids`）。辿っている最中に親が終わって pid が
    // 回ってくると、別のプロセスの名前が祖先に入る。子より新しい親は親ではないため `:vanished` で切る。
    let (chain, end) = walk_ancestry(pid, rejecting_recycled_pids(read_parent, created_at));
    Ancestry {
        chain,
        src: ancestry_src(end),
    }
}

/// その他の unix（iOS / Android 等）: まだ書いていない。
/// 空の並びを黙って返さず、`src` で「未実装」と名乗る。
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[must_use]
pub fn peer_ancestry(_pid: Option<i64>) -> Ancestry {
    Ancestry {
        chain: Vec::new(),
        src: "unimplemented:peer_ancestry",
    }
}

/// Windows: `CreateToolhelp32Snapshot` の**一枚絵を 1 回だけ撮って**、そこから辿る。
///
/// # 1 枚で撮る理由: 並びが途中でちぎれない
///
/// unix の 2 つは pid ごとにカーネルへ訊くので、辿っている最中に親が終わるとそこで切れる
/// （`:vanished`）。Toolhelp は全プロセスを 1 枚で返すので、撮ってから辿れば並び全体が同じ瞬間の絵になる。
///
/// # 重要: この面では `:denied` が産まれない
///
/// 一枚絵は、名前と親 pid だけなら他人のプロセスでも返す（プロセスごとに開かない）。
/// ∴ 語彙の `:denied` はこの面に存在しない。出ないことを「断られなかった」と読まないこと。
/// 訊いてすらいない。
///
/// # 重要: この面では `:vanished` が普通の終わり方
///
/// Windows は親が終わっても子を pid 1 へ付け替えないので、上まで辿るとたいてい
/// 「親 pid が一枚絵に居ない」で終わる（`… <- WindowsTerminal.exe <- explorer.exe` の先では
/// `explorer.exe` の親はもう居ない）。∴ `:vanished` をこの面で「異常が起きた印」と読まないこと。
/// unix では珍しいが、ここでは既定。検体も「`toolhelp` で終わること」を縛らない。
///
/// # 重要: pid の再利用は時刻で検める
///
/// `th32ParentProcessID` は親が既に終わっていても残る。∴ その pid が別のプロセスに再利用されていると、
/// 並びは**嘘の親**へ伸びる。一枚絵に居なければ `:vanished` で正しく切れる。居る場合だけが危うい。
///
/// これは理屈の上の話ではなく、秒の単位で起きる（300 回の spawn・約 7 秒の間に pid が 8 回巡り、
/// 最初の再利用は約 2 秒後だった）。そして捏造された並びは `toolhelp` か `toolhelp:vanished` で
/// 普通に終わるため信号が出ない。
///
/// ∴ [`created_at`] で生成時刻を引き、**子より新しい親は親ではない**としてそこで切る
/// （`Unread::Gone` → `:vanished`）。語彙は増やさない。本当の親は消えている（だから pid が
/// 回った）ので、`:vanished` が正確な名乗り。
///
/// 重要: 時刻が読めなかったときは何もしない（そのまま辿る）。嘘の証拠が無いから。
/// 注意: ∴ 検められなかった段が並びの中に混ざりうる。
///
/// 一枚絵と `OpenProcess` の間に再利用される場合も在るが、そのとき読むのは新しい方の時刻であり、
/// 新しすぎるので `:vanished` へ倒れる（安全側）。
///
/// # 名前は `.exe` を落とさない
///
/// 一枚絵が返すのは image のファイル名で、それがこの面で観測した物。producer は加工しない
/// （Rheon Protocol `spec/ACTOR_v1.md` §2.4）。読む側は、末尾の `.exe` を大小を無視して落としてから
/// 突き合わせる。`System32` には大文字の `.EXE` が数百本在る（`PING.EXE` 等）。
#[cfg(windows)]
pub(crate) fn process_snapshot() -> Result<std::collections::HashMap<i64, (String, i64)>, Unread> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: 旗と 0 を渡すだけ。返った handle は下で必ず閉じる。
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(Unread::Unreadable);
    }
    // 重要: `dwSize` を埋めないと `Process32FirstW` は黙って失敗する（この API の作法）。
    let mut e = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..Default::default()
    };
    let mut map = std::collections::HashMap::new();
    // SAFETY: `snap` は有効な handle、`e` は上で `dwSize` を埋めた PROCESSENTRY32W。
    let mut more = unsafe { Process32FirstW(snap, &raw mut e) } != 0;
    while more {
        let n = e
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(e.szExeFile.len());
        map.insert(
            i64::from(e.th32ProcessID),
            (
                String::from_utf16_lossy(&e.szExeFile[..n]),
                i64::from(e.th32ParentProcessID),
            ),
        );
        // SAFETY: 同上。
        more = unsafe { Process32NextW(snap, &raw mut e) } != 0;
    }
    // SAFETY: 上で得た handle を 1 度だけ閉じる。
    unsafe { CloseHandle(snap) };
    // 自分すら居ない絵は「撮れた」とは言わない（0 件は答えではなく失敗）。
    if map.is_empty() {
        Err(Unread::Unreadable)
    } else {
        Ok(map)
    }
}

/// プロセスの**生成時刻**を引く（Windows・pid 再利用の検め）。
///
/// 一枚絵（[`process_snapshot`]）は名前と親 pid しか返さないので、時刻はここで別に訊く。
/// 戻り値は FILETIME（100ns 単位・UTC）をそのまま数として扱う。比べるだけなので暦に直さない
/// （直すと地域と閏秒の話が入る）。
///
/// 重要: **`None` は「新しい」でも「古い」でもなく、「言えない」**。呼び手は `None` を
/// 検めないとして扱うこと（嘘の証拠にしない）。
/// 注意: `PROCESS_QUERY_LIMITED_INFORMATION` は他人のプロセスにも広く許されるが、
/// 保護されたプロセスや別 session では断られる。
#[cfg(windows)]
pub(crate) fn created_at(pid: i64) -> Option<i64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let pid = u32::try_from(pid).ok()?;
    // SAFETY: 旗と pid を渡すだけ。返った handle は下で必ず閉じる。
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None; // 断られた / もう居ない（どちらも「言えない」）
    }
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = created;
    let mut kernel = created;
    let mut user = created;
    // SAFETY: `h` は有効な handle、4 つとも初期化済みの FILETIME。
    let ok = unsafe {
        GetProcessTimes(
            h,
            &raw mut created,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    // SAFETY: 上で得た handle を 1 度だけ閉じる。
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let t = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    i64::try_from(t).ok()
}

/// 読み取りに **pid 再利用の検め**を巻く（3 つの面とも通る）。
///
/// 巻く形にしてあるのは、検体が実コードと同じ経路を通るため。時刻を注入できれば、pid が
/// 回ってくるのを待たずに陰陽両方の対照が撮れる（本物の再利用を待つ検体は非決定的になる）。
///
/// # 仕組み
///
/// 辿るのは下から上へ順番なので、直前に受け入れた段の生成時刻を覚えておけば、次の段をそれと
/// 比べられる。**先祖は子孫より新しくなり得ない。** 途中の段が読めなくても、最後に分かった時刻と
/// 比べれば推移的に正しい。
///
/// 重要: 時刻が読めなければ何もしない。嘘の証拠が無いから。「検められなかった」を「嘘だった」にしない。
/// 面を限らないのは、規則が純粋だから（どの面の検体でも走る）。
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
pub(crate) fn rejecting_recycled_pids(
    mut read: impl FnMut(i64) -> Result<(String, i64), Unread>,
    mut created: impl FnMut(i64) -> Option<i64>,
) -> impl FnMut(i64) -> Result<(String, i64), Unread> {
    let mut newest = None::<i64>;
    move |p| {
        let entry = read(p)?;
        let t = created(p);
        if let (Some(child), Some(t)) = (newest, t) {
            if t > child {
                // 子より新しいため pid が回ってきている。
                // 本当の親は消えているのだから、`Gone`（`:vanished`）が正確な名乗り。
                return Err(Unread::Gone);
            }
        }
        // 重要: `None`（読めなかった）で上書きしない。境界を捨てるとその先の検めが止まる。
        newest = t.or(newest);
        Ok(entry)
    }
}

/// attach してきたプロセス（`peer_pid`）の親プロセスの並び（Windows）。
///
/// 注意: attach のその瞬間に呼ぶこと。相手が終われば pid は再利用される。
/// 機構と限界は [`process_snapshot`] の doc。
#[cfg(windows)]
#[must_use]
pub fn peer_ancestry(pid: Option<i64>) -> Ancestry {
    let Some(pid) = pid else {
        return Ancestry {
            chain: Vec::new(),
            src: "unavailable:peer_pid",
        };
    };
    let map = match process_snapshot() {
        Ok(m) => m,
        Err(_) => {
            return Ancestry {
                chain: Vec::new(),
                src: "unavailable:toolhelp",
            }
        }
    };
    // 一枚絵に居ないため、その瞬間には居なかった（`:vanished`）。
    // `Denied` は返さない。この機構では訊いていないので（`process_snapshot` の doc）。
    // pid 再利用の検めもこの窓口で巻く（`rejecting_recycled_pids`）。
    let (chain, end) = walk_ancestry(
        pid,
        rejecting_recycled_pids(|p| map.get(&p).cloned().ok_or(Unread::Gone), created_at),
    );
    Ancestry {
        chain,
        src: ancestry_src(end),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) fn ancestry_src(end: AncestryEnd) -> &'static str {
    #[cfg(target_os = "linux")]
    let [root, depth, vanished, denied, unreadable, self_unreadable] = [
        "proc",
        "proc:depth_limit",
        "proc:vanished",
        "proc:denied",
        "proc:unreadable",
        "unavailable:proc",
    ];
    #[cfg(target_os = "macos")]
    let [root, depth, vanished, denied, unreadable, self_unreadable] = [
        "proc_pidinfo",
        "proc_pidinfo:depth_limit",
        "proc_pidinfo:vanished",
        "proc_pidinfo:denied",
        "proc_pidinfo:unreadable",
        "unavailable:proc_pidinfo",
    ];
    // 注意: `:denied` はこの面では産まれない（一枚絵は他人のプロセスの名前と親 pid を、
    // 開かずに返す・`process_snapshot` の doc）。
    #[cfg(windows)]
    let [root, depth, vanished, denied, unreadable, self_unreadable] = [
        "toolhelp",
        "toolhelp:depth_limit",
        "toolhelp:vanished",
        "toolhelp:denied",
        "toolhelp:unreadable",
        "unavailable:toolhelp",
    ];
    match end {
        AncestryEnd::Root => root,
        AncestryEnd::DepthLimit => depth,
        AncestryEnd::Vanished => vanished,
        AncestryEnd::Denied => denied,
        AncestryEnd::Unreadable => unreadable,
        AncestryEnd::SelfUnreadable => self_unreadable,
    }
}

/// Linux: `/proc/<pid>/stat`（世界読み取り可）。
///
/// 読めなかった理由を [`Unread`] に写す。素の `/proc` では `EACCES` は出ないが、
/// `hidepid` を付けた `/proc` では出る。読めたのに解せない行は [`Unread::Unreadable`] にする。
/// 「消えた」でも「断られた」でもないので、どちらにも寄せない。
#[cfg(target_os = "linux")]
pub(crate) fn read_parent(pid: i64) -> Result<(String, i64), Unread> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(line) => parse_proc_stat(&line).ok_or(Unread::Unreadable),
        Err(e) => Err(match e.kind() {
            std::io::ErrorKind::NotFound => Unread::Gone,
            std::io::ErrorKind::PermissionDenied => Unread::Denied,
            _ => Unread::Unreadable,
        }),
    }
}

/// macOS: `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`（`pbsi_ppid` と `pbsi_comm`）。
///
/// # 重要: 短い版で訊く
///
/// `PROC_PIDTBSDINFO`（長い版）は root の持ち物を `EPERM` で断る。macOS の端末は `login(1)`（root）
/// 経由でシェルを起こすので、長い版だとそこで並びが切れ、端末アプリの名前まで届かない。
/// 短い版は一般ユーザーでも読める（`login` → 端末アプリ → `launchd` まで辿り切る）。
/// 欲しいのは `ppid` と `comm` の 2 つだけなので、長い版の残りは要らない。
/// `sysctl(KERN_PROC_PID)` も読めるが採らなかった。`kinfo_proc` が libc に無く、居ない pid でも
/// `rc=0`（`len=0` で名乗る）ため失敗が返り値に出ない。
///
/// # 注意: 失敗は `-1` ではなく `0` で返る
///
/// ∴ `n != size` を失敗の判定に使い、errno は撃つ直前に 0 へ戻してから読む。さもないと、
/// 短く答えた（errno を触らない）場合に、前の呼び出しの errno を理由として拾う。
#[cfg(target_os = "macos")]
pub(crate) fn read_parent(pid: i64) -> Result<(String, i64), Unread> {
    let pid = libc::c_int::try_from(pid).map_err(|_| Unread::Unreadable)?;
    // SAFETY: `proc_bsdshortinfo` は整数と文字配列だけの C 構造体で、全 0 は有効な値。
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdshortinfo>())
        .map_err(|_| Unread::Unreadable)?;
    // SAFETY: `__error()` は当スレッドの errno を指す（macOS の `errno` マクロの実体）。
    unsafe { *libc::__error() = 0 };
    // SAFETY: `info` は `size` バイトの書き込み先として有効。
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    // 短く答えた場合も「読めなかった」。半分だけ埋まった構造体から名前を作らない。
    if n != size {
        return Err(match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Unread::Gone,
            Some(libc::EPERM | libc::EACCES) => Unread::Denied,
            _ => Unread::Unreadable,
        });
    }
    // 注意: `pbsi_comm` は NUL で終わらないことがある（ちょうど MAXCOMLEN の名前）。配列の中だけ見る。
    #[allow(clippy::cast_sign_loss)]
    let bytes: Vec<u8> = info
        .pbsi_comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        i64::from(info.pbsi_ppid),
    ))
}

/// **berthd 自身**が居る区画（台帳の `daemon_session`）。
///
/// # なぜ要るか
///
/// 監査セッション id で、ssh の外側のシェルの記録と berthd の attach の記録を結べる
/// （[`peer_from_cred`]）:
///
/// ```text
/// ① 外側のシェル（ssh の中）     os_sessionid = N
/// ② berthd の attach 記録         peer_session = N        ← join A
/// ③ 内側のシェル（berthd の子）   os_sessionid = ?
/// ```
///
/// 重要: **③ は「attach した client の区画」ではなく「berthd が生まれた区画」を継ぐ。**
/// berthd が ssh のログインの中で生まれると、後からローカルで attach した client の下のシェルも、
/// その（既に終了しているかもしれない）ログインの番号を名乗る。
///
/// 誰も嘘はついていない。子プロセスは本当にその監査セッションに居る。偽になるのは読み手の推論
/// 「③ の記録は ① のログインの中で起きた」の方で、join A が正しいのは berthd が client と同じ
/// ログインから起きたときだけ。
///
/// ∴ 値を 1 つ足して、2 つの場合を読む側が見分けられるようにする:
///
/// ```text
/// ③.os_sessionid == daemon_session  → ③ の番号は berthd の区画。join A を当てはめない
/// ①.os_sessionid == ②.peer_session  → join A が成立している
/// ```
///
/// 誰の報告も変えない（`peer_session` もシェルの `os_sessionid` もそのまま）。黙っていた曖昧さを、
/// 検出できる形にするだけ。
///
/// # 機構は peer と同じ物を使う
///
/// 別の機構で取ると「同じ番号空間か」をその場で確かめられなくなる。∴
///
/// - **Linux**: `peer_audit_session` に自分の pid を渡す（`/proc/self/sessionid` と同じ物）
/// - **macOS**: `socketpair` を作って `LOCAL_PEERTOKEN` で訊く（相手が自分のため自分の `asid` が
///   返る）。`getaudit_addr()` を新たに呼ばない。peer と同じ口で取った値でなければ、
///   比べてよいかを doc でしか保証できない。
/// - **Windows**: `ProcessIdToSessionId(GetCurrentProcessId())`（peer 側と同じ関数）
///
/// 注意: `src` は必ず返す。「無い」が *この面では取れない* なのか *取り損ねた* なのかを分ける。
#[must_use]
pub fn own_audit_session() -> (Option<u32>, &'static str) {
    #[cfg(target_os = "linux")]
    {
        // auid は使わない（`daemon_session` は区画の番号だけを載せる欄）。
        let (session, _auid, src) = peer_audit_session(i64::from(std::process::id()), None);
        (session, src)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsFd;
        let Ok((sock, _peer)) = std::os::unix::net::UnixStream::pair() else {
            return (None, "unavailable:socketpair");
        };
        // 重要: **`_peer` を落とさないこと。** 相手側を先に閉じると `LOCAL_PEERTOKEN` は `ENOTCONN` を
        // 返し、`unavailable:local_peertoken` になる（`peer_audit_session` の「相手が既に消えた後」）。
        // 注意: 「daemon と検体が同じ綴りを返す」だけを見る検体は、両方が同じように壊れると通る。
        let (session, _auid, src) = peer_audit_session(0, Some(sock.as_fd()));
        (session, src)
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        (None, "unimplemented:own_audit_session")
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        let mut sid: u32 = 0;
        // SAFETY: どちらも引数の無い / 出力 1 つの Win32 呼び出しで、`sid` は有効な書き先。
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut sid) } != 0 {
            (Some(sid), "process_id_to_session_id")
        } else {
            (None, "unavailable:process_id_to_session_id")
        }
    }
}

/// 名前付きパイプの向こうに居るプロセスを**カーネルに訊く**（Windows）。
///
/// unix の `SO_PEERCRED` に対応する口だが、**答えるのは pid だけ**（Windows に uid は無い）。
/// その pid から **session id** を引く: `0` ならサービスセッション（ssh 経由がここに載る）、
/// `1` 以上なら対話ログイン。
///
/// 注意: 失敗しても発明しない。取れなければ `None` のまま、`src` に理由を残す。
#[cfg(windows)]
#[must_use]
pub fn peer_from_pipe(handle: std::os::windows::io::RawHandle) -> Peer {
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;

    let mut pid: u32 = 0;
    if unsafe { GetNamedPipeClientProcessId(handle.cast(), &raw mut pid) } == 0 {
        return Peer {
            uid: None,
            pid: None,
            session: None,
            auid: None,
            src: Some("unavailable:GetNamedPipeClientProcessId"),
        };
    }
    let mut sid: u32 = 0;
    let session = (unsafe { ProcessIdToSessionId(pid, &raw mut sid) } != 0).then_some(sid);
    Peer {
        // Windows に uid は無い。空のままにする（無い物を発明しない）。
        uid: None,
        pid: Some(i64::from(pid)),
        // Windows に auid は無い（uid と同じく存在しない）。
        auid: None,
        session,
        // session だけ落ちた場合を分けて名乗る（pid は取れているので「取れなかった」ではない）。
        src: Some(if session.is_some() {
            "named_pipe"
        } else {
            "named_pipe:no_session"
        }),
    }
}
