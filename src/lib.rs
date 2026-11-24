//! berthd protocol — client と berthd(daemon) が共有する層。
//!
//! 要点:
//! - フレーム: `| len:u32 LE | kind:u8 | serial:u32 LE | payload[len] |`
//! - `kind=0`(control): payload は **Rheon Protocol v1 のレコード**（1 行 JSON・`{v,source,type,event,ts,..}`）
//! - `kind=1`(data): payload は **生バイト**（PTY 入出力・無加工・UTF-8 保証なし）
//! - `serial`: req/resp 対応（push は 0）。レコードは req/resp の概念を持たないのでフレーム側が補う。

// ── ワイヤの契約は `berth-protocol` が持つ ──
//
// ここに在るのは写しではなく re-export。正本は `berth-protocol` の 1 箇所で、ここは
// 「berthd もその契約を話す」という宣言だけ。berthd は別プロセスのサービスなので、
// client は berthd を link せず、契約のクレートだけを引けば足りる（境界はソケットと 9 バイトのヘッダ）。
//
// 新しく書くコードは `berth_protocol::` を直に引くこと。ここは既存の呼び手を動かさないための口で、
// 増やす物ではない。
#[cfg(windows)]
pub use berth_protocol::pipe_name;
pub use berth_protocol::{
    connect_daemon, connect_daemon_at, control_frame, decode_control, default_socket_path,
    encode_control, endpoint_display, endpoint_os, ensure_private_dir, read_frame,
    scratch_endpoint, socket_path, write_frame, Claim, Control, DaemonStream, Frame, SessionInfo,
    KIND_CONTROL, KIND_DATA, PROTOCOL_VERSION, RHEON_V, SESSION_TYPE,
};

/// 名前付きパイプの口に付ける DACL。Windows だけの機構。
#[cfg(windows)]
mod win_pipe_acl;

// 録画の頭より前に捨てた出力が残した状態（berthd が食い・berth が作り直す）。
// 注意: `///` にしない。外の doc があると rustdoc は中の `//!` と繋いで lib の側からリンクを引き、
// `replay_head.rs` の `HeadTracker` へのリンクが切れる。説明は `replay_head.rs` の `//!` が持つ。
pub mod replay_head;

/// `connect_daemon()` が返す `io::Error` を割る。
///
/// 「居ない」「開かせてもらえない」「口は在るが全インスタンス使用中」は、利用者がやることが
/// 全部違う。1 つに丸めると、走っている berthd が在るのに「セッション 0 件」と刷ることになる。
///
/// 例: Windows で ssh 越しに起きた berthd は session 0 に載るので、デスクトップからは
/// 口が見えているのに `ERROR_ACCESS_DENIED` で開けない。「繋がらない」は「居ない」ではない。
pub enum Unreachable {
    /// 口そのものが無い（berthd は走っていない）。これだけが「未起動」。
    NotRunning,
    /// 口は在るが開かせてもらえない（別 session / 別の資格で走っている）。
    Denied,
    /// 口は在るが全インスタンスが使用中（一時的・待てば通りうる）。
    /// Windows だけが作る（名前付きパイプの `ERROR_PIPE_BUSY`）。
    /// unix の socket に対応する状態が無いので、あちらでは構築されない。
    #[cfg_attr(not(windows), allow(dead_code))]
    Busy,
    /// 口は在って開いたが、向こうは自分の berthd ではない（Windows）。パイプ名を
    /// 別のユーザーのプロセスが先に取っている。`owner` はそのプロセスの所有者の SID（読めなければ理由）。
    /// Windows だけが作る（unix の socket はファイルの所有者と mode が先に守る）。
    #[cfg_attr(not(windows), allow(dead_code))]
    Impostor { owner: String },
    /// それ以外。丸めずに綴りをそのまま見せる。
    Other(std::io::Error),
}

/// 繋いだ先のパイプの持ち主が自分ではなかった、を `io::Error` に載せて運ぶ器
/// （[`Unreachable::of`] が取り出して [`Unreachable::Impostor`] にする）。
#[derive(Debug)]
pub struct Impostor {
    /// 向こうのプロセスの所有者（SID の文字列）。読めなかったときはその理由。
    pub owner: String,
}

impl std::fmt::Display for Impostor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the server on this pipe is owned by {}, not you",
            self.owner
        )
    }
}

impl std::error::Error for Impostor {}

/// 規則（純粋・面に依らない）: 繋いだ先の所有者 `theirs` が自分 `mine` と同じときだけ通す。
/// 違えば `Err(theirs)`。同一ユーザーの別プロセスは通る（既に owner 権限を持つ物は脅威モデルの外）。
///
/// # Errors
/// 所有者が違う。中身は向こうの SID。
pub fn server_belongs_to(mine: &str, theirs: &str) -> Result<(), String> {
    if mine == theirs {
        Ok(())
    } else {
        Err(theirs.to_string())
    }
}

/// Windows: 繋いだ直後・握手の前に、**パイプの向こうのプロセスの所有者**を自分と照合する。
/// 違えば `PermissionDenied` に [`Impostor`] を載せて返す（呼び手は `Unreachable::of` で割る）。
///
/// 名前付きパイプの名前空間はグローバルで、作るのに特権が要らないため別のユーザーが同じ名前を
/// 先に取れる。照合しないと、偽のサーバに握手を渡すことになる。
///
/// 注意: 読めなかったときも通さない。「検められなかった」を「自分の物だった」にしない。
/// 同一ユーザーの berthd なら `PROCESS_QUERY_LIMITED_INFORMATION` は常に通るので、読めないのは
/// 別の資格で走っている何かか、保護されたプロセスであり、どちらも自分の berthd ではない。
///
/// # Errors
/// 所有者が違う／読めない（`PermissionDenied` ＋ [`Impostor`]）。自分の SID が引けない（そのまま）。
#[cfg(windows)]
pub fn refuse_a_server_that_is_not_mine(client: &DaemonStream) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    let mine = win_pipe_acl::current_user_sid_string()?;
    let theirs = match win_pipe_acl::pipe_server_owner_sid(client.as_raw_handle()) {
        Ok(sid) => sid,
        Err(e) => format!("<unreadable: {e}>"),
    };
    match server_belongs_to(&mine, &theirs) {
        Ok(()) => Ok(()),
        Err(owner) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            Impostor { owner },
        )),
    }
}

impl Unreachable {
    pub fn of(e: std::io::Error) -> Self {
        // `ERROR_PIPE_BUSY` (231) は `ErrorKind` に写像が無いので生の番号で見る。
        #[cfg(windows)]
        {
            if e.raw_os_error() == Some(231) {
                return Unreachable::Busy;
            }
        }
        // 繋がったが持ち主が違った（[`refuse_a_server_that_is_not_mine`]）。
        if let Some(i) = e.get_ref().and_then(|i| i.downcast_ref::<Impostor>()) {
            return Unreachable::Impostor {
                owner: i.owner.clone(),
            };
        }
        match e.kind() {
            // unix は端点が消えていれば NotFound、残骸が残っていれば ConnectionRefused。
            // どちらも「走っていない」で、利用者の次の一手は同じ。
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                Unreachable::NotRunning
            }
            std::io::ErrorKind::PermissionDenied => Unreachable::Denied,
            _ => Unreachable::Other(e),
        }
    }

    /// berthd は走っていない、と断言してよいか。
    /// 注意: これが false のときに「セッション 0 件」や「停止した」と刷ってはいけない。
    pub fn means_not_running(&self) -> bool {
        matches!(self, Unreachable::NotRunning)
    }

    /// 何が起きているかだけを言う（助言は含めない）。
    ///
    /// 出口の案内は呼び手ごとに違う（CLI は「ssh で入り直せ」、デーモンは「先に畳め」）ので、
    /// 診断はここ・助言は呼び手に分けてある。
    pub fn describe(&self, endpoint: &str) -> String {
        match self {
            Unreachable::NotRunning => format!("nothing is listening at {endpoint}"),
            Unreachable::Denied => format!(
                "berthd at {endpoint} refused this session (access denied).\n\
                 Something is listening there that this session may not open,\n\
                 and on Windows a berthd started over ssh lives in session 0\n\
                 where the desktop session cannot open it."
            ),
            Unreachable::Busy => {
                format!("berthd at {endpoint} is busy (all pipe instances in use); try again")
            }
            Unreachable::Impostor { owner } => format!(
                "the server at {endpoint} is owned by {owner}, not you.\n\
                 Refusing to talk to it: someone else holds this pipe name."
            ),
            Unreachable::Other(e) => format!("cannot reach berthd at {endpoint}: {e}"),
        }
    }
}

/// 出荷版。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// このバイナリが作られたソース = git の短縮ハッシュ（未コミットなら `-dirty`・
/// git が答えられなければ `unknown`）。焼き込みは `build.rs`。
pub const BUILD_ID: &str = env!("BERTHD_BUILD_ID");

/// 自己申告に使う 1 行（`0.1.0 (abc123def)`）。
///
/// 版だけでは配備物を同定できない。client と daemon は別々に配備されるので、ずれは日常的に起きる。
/// `berth info` が「今どの berthd に繋がっているか」に答えられるよう、ビルドの出所も一緒に名乗る。
///
/// 注意: `/` を含めないこと。`berth info` は `Welcome.server`（`berthd/<ver>`）を `rsplit('/')` で
/// 割って版を取り出す。
pub const VERSION_STRING: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("BERTHD_BUILD_ID"),
    ")"
);

/// レコードの `source` の既定値。
pub const SOURCE: &str = "berthd";

mod config;
mod journal;
mod listener;
mod peer;
mod shell;
/// 中身は Windows だけ。
#[cfg(windows)]
mod winproc;
pub use config::*;
pub use journal::*;
pub use listener::*;
pub use peer::*;
pub use shell::*;
#[cfg(windows)]
pub use winproc::*;

#[cfg(test)]
mod tests {

    /// unix の peer に監査セッション id が載る。
    ///
    /// 見るのは値そのものではなく `src` が機構を名乗ること。値は環境で違う
    /// （ログインごとの番号・PAM を通っていなければ `u32::MAX`（未設定））ので、
    /// 番号を決め打ちすると別の環境で赤になる。
    ///
    /// 陽性対照: `src` が必ずその面の機構を名乗る（Linux = `proc_sessionid` /
    /// macOS = `local_peertoken`）。これが無いと、実装をすべて外しても
    /// 「`session` が `None`」なだけで緑のまま通る。
    ///
    /// 口を渡すのは macOS のため（あちらの番号は接続に付いている）。`socketpair` の相手は
    /// 自分自身なので、ssh を張らずに機構だけ確かめられる。Linux はこの口を見ない。
    #[cfg(unix)]
    #[test]
    fn a_unix_peer_carries_its_audit_session_or_says_why_not() {
        use std::os::fd::AsFd;

        let me = i64::from(std::process::id());
        let (sock, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let p = peer_from_cred(Some(1000), Some(me), Some(sock.as_fd()));

        assert_eq!(p.pid, Some(me), "pid を落としている: {p:?}");
        assert_eq!(p.uid, Some(1000), "uid を落としている: {p:?}");
        let src = p.src.expect("src は面に依らず必ず在る");
        assert!(
            src.starts_with("so_peercred+"),
            "観測の機構を名乗っていない: {src}"
        );

        // 面ごとの機構名。これを持たない面は `unimplemented:` と名乗ること。
        // *この面では取れない* と *まだ書いていない* を同じ顔にしないための陽性対照。
        #[cfg(target_os = "linux")]
        let mechanism = "proc_sessionid";
        #[cfg(target_os = "macos")]
        let mechanism = "local_peertoken";

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // 陽性対照: 実装を外すと `unimplemented:` に落ちてここで赤になる。
            assert!(
                src.contains(mechanism),
                "この面の機構（{mechanism}）を名乗っていない（実装が外れても気づけない）: {src}"
            );
            // 値と名乗りが一致していること（片方だけ動く形を作らない）。
            let has_value = p.session.is_some();
            assert_eq!(
                has_value,
                src == format!("so_peercred+{mechanism}"),
                "値と `src` が食い違っている: session={:?} src={src}",
                p.session
            );
            // 番兵をそのまま値にしていないこと（Linux は `u32::MAX`／macOS は `0` の
            // `AU_DEFAUDITSID`。どちらも値ではない）。
            assert_ne!(
                p.session,
                Some(u32::MAX),
                "`u32::MAX` は値ではなく未設定の番兵"
            );
            assert_ne!(
                p.session,
                Some(0),
                "`0`（`AU_DEFAUDITSID`）は値ではなく未設定の番兵"
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(
            src, "so_peercred+unimplemented:peer_audit_session",
            "未実装は**黙らず名乗る**（*取れない* と *書いていない* を分ける）: {src}"
        );
    }

    /// 陰性対照: pid が取れなければ、引く先が無いのでセッションも無い。
    /// ここで `src` が `unavailable:so_peercred` のままであること（沈黙を主張に変えていないこと、
    /// `peer_from_cred` が uid を発明していないこと）。
    ///
    /// 注意: 口は生きているのに `None` を返すことを釘付けしている。macOS の番号は接続に付いているので、
    /// 理屈の上ではここでも訊けるはずだが、`getpeereid` だけが落ちて token だけ通る形を作って
    /// 確かめられていないので、その道は書いていない。実装するとここが赤くなる。
    /// この但し書きを読んでから変えること。
    #[cfg(unix)]
    #[test]
    fn no_pid_means_no_session_and_it_says_so() {
        use std::os::fd::AsFd;

        let (sock, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let p = peer_from_cred(None, None, Some(sock.as_fd()));
        assert_eq!(p.pid, None);
        assert_eq!(
            p.session, None,
            "pid が無いのにセッションが載っている（発明している）"
        );
        assert_eq!(p.src, Some("unavailable:so_peercred"), "{p:?}");
    }

    /// 陰性対照（macOS）: 相手の端が閉じた口を訊いたら「訊けなかった」と名乗る。
    /// *訊けなかった* と *訊けたが未設定* は別の事実なので、同じ顔にしない。
    ///
    /// 実運用で起きる。attach の直後に peer が消えると、カーネルは `ENOTCONN` を返す。
    /// Linux の「`/proc/<pid>` が無い」に対応する形で、同じ `unavailable:` に落ちる。
    ///
    /// 注意: `shutdown` では起きない（token は答え続ける）。相手の端が閉じることが条件。
    /// ∴ 検体は `drop` する（`shutdown` に書き換えると緑のまま無意味になる）。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_peer_whose_end_is_closed_says_unavailable_not_unset() {
        use std::os::fd::AsFd;

        let (sock, peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        // 前提を先に釘付けする（閉じる前は答えていたこと）。
        // これが無いと、そもそも訊けない口で「取れなかった」を見て緑にしうる。
        let before = peer_from_cred(Some(1000), Some(1), Some(sock.as_fd()));
        assert_eq!(
            before.src,
            Some("so_peercred+local_peertoken"),
            "検体の前提が崩れている（閉じる前から訊けていない）: {before:?}"
        );

        drop(peer);
        let p = peer_from_cred(Some(1000), Some(1), Some(sock.as_fd()));
        assert_eq!(p.session, None);
        assert_eq!(
            p.src,
            Some("so_peercred+unavailable:local_peertoken"),
            "消えた相手を「未設定」と読ませている: {p:?}"
        );
    }

    /// 陰性対照（Linux）: 居ないプロセスを訊いたら「読めなかった」と名乗る。
    /// *読めなかった* と *読めたが未設定* は別の事実なので、同じ顔にしない。
    /// 実運用で起きる。attach の直後に peer が消えると `/proc/<pid>` が無い。
    #[cfg(target_os = "linux")]
    #[test]
    fn a_peer_that_is_already_gone_says_unavailable_not_unset() {
        // pid を推測しない。`pid_max` より大きい値は必ず存在しない。
        let pid_max: i64 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(4_194_304);
        let gone = pid_max + 1;
        assert!(
            !std::path::Path::new(&format!("/proc/{gone}")).exists(),
            "検体の前提が崩れている"
        );

        let p = peer_from_cred(Some(1000), Some(gone), None);
        assert_eq!(p.session, None);
        assert_eq!(
            p.src,
            Some("so_peercred+unavailable:proc_sessionid"),
            "消えた相手を「未設定」と読ませている: {p:?}"
        );
    }
    /// 検体が開発者の実台帳へ書かないこと。
    ///
    /// 守っているのは `.cargo/config.toml` の `[env]` 1 行で、設定が黙って効かなくなるのが
    /// この種の仕掛けの壊れ方。∴ 設定ではなく、設定が効いていることを検査する
    /// （効いていないと、`cargo test` のたびに実台帳へ検体の行が入る）。
    #[test]
    fn tests_never_write_to_the_developers_real_journal() {
        let p = journal_path();
        let Some(home) = dirs::home_dir() else { return };
        assert!(
            !p.starts_with(home.join(".berth")),
            "検体の台帳が実の家を指している: {} —— .cargo/config.toml の \
             BERTHD_JOURNAL_PATH が効いていない",
            p.display()
        );
    }

    use super::*;

    /// 名乗りの解決順（環境を触らない純粋テスト）。明示は `AUDIT_ACTOR` だけ。
    #[test]
    fn an_explicit_name_wins_over_a_trace_left_in_the_environment() {
        let c = Claim::from_parts(Some("deploy-bot".into()), Some("claude-code".into()));
        assert_eq!(c.actor.as_deref(), Some("deploy-bot"));
        assert_eq!(c.src.as_deref(), Some("audit_actor"));
    }

    /// 痕跡から拾うときは集計できる形に正規化する。シェルの側（hsh）と同じ規則で
    /// ないと、2 つの記録を突き合わせたときに同じエージェントが別名で並ぶ。
    #[test]
    fn an_agents_name_is_normalised_the_same_way_the_shell_normalises_it() {
        let c = Claim::from_parts(None, Some("claude-code_2-1-208_agent".into()));
        assert_eq!(c.actor.as_deref(), Some("claude-code"));
        assert_eq!(c.src.as_deref(), Some("agent"));
    }

    /// 誰も名乗らなければ空。ここで OS ユーザー名に落ちてはいけない。
    /// それは隣の `peer_uid` の焼き直しで、沈黙を主張に変えてしまう。
    #[test]
    fn silence_stays_silence() {
        let c = Claim::from_parts(None, None);
        assert!(c.actor.is_none() && c.src.is_none(), "{c:?}");
    }

    /// `stat` は最後の `)` の後ろから欄を数える。
    /// 名前に空白や括弧が入る（`tmux: server`）ので、素朴に空白で割ると ppid を取り違える。
    #[test]
    fn a_name_with_spaces_or_parens_does_not_shift_the_parent_pid() {
        assert_eq!(
            parse_proc_stat("5798 (tmux: server) S 1 5798 5798 0 -1"),
            Some(("tmux: server".into(), 1))
        );
        assert_eq!(
            parse_proc_stat("42 (a) b) R 7 42 42 0 -1"),
            Some(("a) b".into(), 7))
        );
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("1 (x) S notanumber"), None);
    }

    /// 持ち主が同じときだけ通す（両腕）。
    #[test]
    fn a_server_is_mine_only_when_the_owner_sid_is_mine() {
        assert_eq!(
            server_belongs_to("S-1-5-21-1-2-3-1001", "S-1-5-21-1-2-3-1001"),
            Ok(())
        );
        assert_eq!(
            server_belongs_to("S-1-5-21-1-2-3-1001", "S-1-5-21-1-2-3-1002"),
            Err("S-1-5-21-1-2-3-1002".to_string())
        );
        // 読めなかった印も「違う」側に倒れる（通さない）。
        assert!(server_belongs_to("S-1-5-21-1-2-3-1001", "<unreadable: denied>").is_err());
        // `of` が運ぶ: PermissionDenied ＋ Impostor は Denied ではなく Impostor。
        let e = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            Impostor {
                owner: "S-1-5-21-9".into(),
            },
        );
        assert!(
            matches!(Unreachable::of(e), Unreachable::Impostor { owner } if owner == "S-1-5-21-9")
        );
        let plain = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(Unreachable::of(plain), Unreachable::Denied));
    }

    /// starttime は 22 欄目（`)` の後ろから数えて 0 始まりの 19）。名前の空白で欄がずれない。
    #[test]
    fn the_start_time_is_the_22nd_field_counted_after_the_last_paren() {
        // 実際の `/proc/<pid>/stat` の形（22 欄目 = 123456789）。
        let line = "5798 (tmux: server) S 1 5798 5798 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 123456789 2000 300 18446744073709551615";
        assert_eq!(parse_proc_stat_start(line), Some(123_456_789));
        assert_eq!(parse_proc_stat_start("5798 (tmux: server) S 1 5798"), None);
        assert_eq!(parse_proc_stat_start("garbage"), None);
    }

    /// 陽性対照: 自分の生成時刻は読めて、親より新しい（親は先に居た）。
    /// 読めることだけを主張する。値の意味（tick / µs）は面で違う。
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn my_own_start_time_is_readable_and_not_older_than_my_parents() {
        let me = i64::from(std::process::id());
        let mine = created_at(me).expect("own start time is always readable");
        let (_, ppid) = read_parent(me).expect("own stat is readable");
        if let Some(parent) = created_at(ppid) {
            assert!(parent <= mine, "parent {parent} started after child {mine}");
        }
    }

    // ── どのシェルを起こすか ──

    use super::{
        looks_like_a_login_shell, parse_login_shells, resolve_client_shell, resolve_default_shell,
        Face, ShellSrc,
    };

    fn shells() -> Vec<String> {
        // 実際の `/etc/shells` の抜粋（シェルでない `tmux` が載っていることもある）。
        parse_login_shells(
            "# /etc/shells: valid login shells\n\
             /bin/sh\n\
             /bin/bash\n\
             /usr/bin/dash\n\
             /usr/bin/tmux\n\
             /tmp/t/.local/bin/hsh\n\
             \n\
             /usr/bin/fish\n",
        )
    }

    #[test]
    fn the_login_shell_list_drops_comments_and_blank_lines() {
        let list = shells();
        assert!(!list.iter().any(|l| l.starts_with('#')), "{list:?}");
        assert!(!list.iter().any(|l| l.is_empty()), "{list:?}");
        assert_eq!(list.len(), 6, "{list:?}");
    }

    /// 照合は basename。`/etc/shells` に載る綴り（`~/.local/bin/hsh`）と PATH が引く綴り
    /// （`~/.cargo/bin/hsh`）は、同じ道具でも違いうる。パス全体の一致だと
    /// 「シェルではない」と判定してしまう。
    #[test]
    fn a_shell_installed_somewhere_else_is_still_a_shell() {
        assert!(looks_like_a_login_shell("/tmp/t/.cargo/bin/hsh", &shells()));
        assert!(looks_like_a_login_shell("/usr/bin/dash", &shells()));
        // 陰性側: 親がシェルでない道具（エージェントなど）のとき。
        assert!(!looks_like_a_login_shell(
            "/tmp/t/.local/share/claude/versions/2.1.273",
            &shells()
        ));
        assert!(!looks_like_a_login_shell("", &shells()));
        // 名簿が空なら何も通さない（Windows がこの形であり、あちらに面の分岐が要らない理由）。
        assert!(!looks_like_a_login_shell("/bin/bash", &[]));
    }

    /// 設定ファイルの `default-shell` は `-s` と `BERTHD_SHELL` に負け、親の観測と `$SHELL` に勝つ（面に依らない）。
    #[test]
    fn the_config_shell_sits_between_the_explicit_and_the_observed() {
        let list = shells();
        for face in [Face::Unix, Face::Windows] {
            assert_eq!(
                resolve_client_shell(
                    face,
                    Some("rsh"),
                    Some("hsh"),
                    Some("zsh"),
                    Some("/bin/bash"),
                    &list,
                    Some("/bin/zsh")
                ),
                Some(("rsh".to_string(), ShellSrc::CliFlag))
            );
            assert_eq!(
                resolve_client_shell(
                    face,
                    None,
                    Some("hsh"),
                    Some("zsh"),
                    Some("/bin/bash"),
                    &list,
                    Some("/bin/zsh")
                ),
                Some(("hsh".to_string(), ShellSrc::BerthdShell))
            );
            assert_eq!(
                resolve_client_shell(
                    face,
                    None,
                    None,
                    Some("hsh"),
                    Some("/bin/bash"),
                    &list,
                    Some("/bin/zsh")
                ),
                Some(("hsh".to_string(), ShellSrc::Config)),
                "設定は親の観測より強い"
            );
            assert_eq!(
                resolve_client_shell(face, None, None, Some("  "), None, &list, None),
                None,
                "空白だけの default-shell は無いのと同じ"
            );
        }
        assert_eq!(ShellSrc::Config.as_str(), "config");
    }

    /// 設定ファイルの読み方: 読める・`~` の展開・知らないキーと型違いは**読めない**（黙って無視しない）。
    #[test]
    fn the_config_file_is_read_strictly_and_expands_a_leading_tilde() {
        let home = std::path::Path::new("/tmp/t");
        let cfg = parse_config(
            "default-shell = \"~/.local/bin/hsh\"\n\n[env]\nCLAUDE_CODE_SHELL = \"~/.local/libexec/hsh-as-bash/bash\"\nPLAIN = \"x~y\"\nHOME_ONLY = \"~\"\n",
            Some(home),
        )
        .unwrap();
        assert_eq!(cfg.default_shell.as_deref(), Some("/tmp/t/.local/bin/hsh"));
        assert_eq!(
            cfg.env_pairs(),
            vec![
                (
                    "CLAUDE_CODE_SHELL".to_string(),
                    "/tmp/t/.local/libexec/hsh-as-bash/bash".to_string()
                ),
                ("HOME_ONLY".to_string(), "/tmp/t".to_string()),
                ("PLAIN".to_string(), "x~y".to_string()),
            ],
            "先頭の ~ だけ展開・キー順"
        );
        // 空のファイルは既定（何も言わない）。
        assert_eq!(
            parse_config("", Some(home)).unwrap(),
            BerthConfig::default()
        );
        // home が無ければ展開しない。
        assert_eq!(
            parse_config("default-shell = \"~/x\"", None)
                .unwrap()
                .default_shell
                .as_deref(),
            Some("~/x")
        );
        // 知らないキー・型違い・壊れた TOML は Err（呼び手が 1 度名乗る）。
        let err = parse_config("default_shell = \"hsh\"", Some(home)).unwrap_err();
        assert!(
            err.starts_with("unknown field `default_shell`") && err.contains("line 1"),
            "綴り違い（_）は知らないキーで、1 行で理由と場所が分かる: {err}"
        );
        assert!(parse_config("default-shell = 3", Some(home)).is_err());
        assert!(
            parse_config("[env]\nX = 1", Some(home)).is_err(),
            "env の値は文字列だけ"
        );
        assert!(parse_config("default-shell = \"hsh", Some(home)).is_err());
    }

    /// 場所は `BERTH_CONFIG` > `<home>/.berth/config.toml`。空の `BERTH_CONFIG` は無いのと同じ。home も無ければ `None`。
    #[test]
    fn the_config_path_is_the_override_or_the_berth_home() {
        use std::ffi::OsStr;
        let home = std::path::Path::new("/tmp/t");
        assert_eq!(
            config_path_from(None, Some(home)),
            Some(std::path::PathBuf::from("/tmp/t/.berth/config.toml"))
        );
        assert_eq!(
            config_path_from(Some(OsStr::new("/tmp/c.toml")), Some(home)),
            Some(std::path::PathBuf::from("/tmp/c.toml"))
        );
        assert_eq!(
            config_path_from(Some(OsStr::new("")), Some(home)),
            Some(std::path::PathBuf::from("/tmp/t/.berth/config.toml"))
        );
        assert_eq!(config_path_from(None, None), None);
    }

    /// client 側の解決を、両方の面から撃つ（`cfg!` に埋めていないので、どの面の上でも両方を撃てる）。
    #[test]
    fn the_client_resolves_the_shell_of_the_seat_that_typed_it() {
        let list = shells();
        // ⓪ CLI オプションが最優先（面に依らない）。
        assert_eq!(
            resolve_client_shell(
                Face::Unix,
                Some("rsh"),
                Some("hsh"),
                None,
                Some("/bin/bash"),
                &list,
                Some("/bin/zsh")
            ),
            Some(("rsh".to_string(), ShellSrc::CliFlag))
        );
        assert_eq!(
            resolve_client_shell(
                Face::Windows,
                Some("hsh.exe"),
                Some("powershell.exe"),
                None,
                None,
                &list,
                None
            ),
            Some(("hsh.exe".to_string(), ShellSrc::CliFlag))
        );
        // ① BERTHD_SHELL 明示が次点（面に依らない）。
        assert_eq!(
            resolve_client_shell(
                Face::Unix,
                None,
                Some("hsh"),
                None,
                Some("/bin/bash"),
                &list,
                Some("/bin/zsh")
            ),
            Some(("hsh".to_string(), ShellSrc::BerthdShell))
        );
        // ② 観測が `$SHELL` に勝つ（bash から `hsh` と打った端末がこれ。
        //    （`$SHELL` は `/bin/bash` のままなのに、走っているのは hsh）。
        assert_eq!(
            resolve_client_shell(
                Face::Unix,
                None,
                None,
                None,
                Some("/tmp/t/.cargo/bin/hsh"),
                &list,
                Some("/bin/bash")
            ),
            Some(("/tmp/t/.cargo/bin/hsh".to_string(), ShellSrc::ParentShell))
        );
        // ③ 親がシェルでないとき（エージェントの下・cron）は `$SHELL` へ落ちる。
        assert_eq!(
            resolve_client_shell(
                Face::Unix,
                None,
                None,
                None,
                Some("/usr/bin/cron"),
                &list,
                Some("/bin/zsh")
            ),
            Some(("/bin/zsh".to_string(), ShellSrc::EnvShell))
        );
        // ④ 何も無ければ何も言わない（berthd の既定へ）。
        assert_eq!(
            resolve_client_shell(Face::Unix, None, None, None, None, &list, None),
            None
        );
        // ⑤ 注意: Windows は `$SHELL` を見ない。git-bash の `/bin/bash.exe` は
        //    `CreateProcessW` から解決しない（os error 3）。
        assert_eq!(
            resolve_client_shell(
                Face::Windows,
                None,
                None,
                None,
                None,
                &list,
                Some("/bin/bash.exe")
            ),
            None,
            "Windows で $SHELL を載せると、git-bash から打った人が全員落ちる"
        );
        // ⑥ Windows でも明示は効く。
        assert_eq!(
            resolve_client_shell(
                Face::Windows,
                None,
                Some("hsh.exe"),
                None,
                None,
                &list,
                None
            ),
            Some(("hsh.exe".to_string(), ShellSrc::BerthdShell))
        );
        // ⑦ 空・空白だけは「言わなかった」と同じ。
        assert_eq!(
            resolve_client_shell(
                Face::Unix,
                Some("  "),
                Some("  "),
                None,
                None,
                &list,
                Some("")
            ),
            None
        );
    }

    /// 名前だけのシェルは client の `PATH` で絶対パスに直す（#72）。
    ///
    /// 注意: 検体のパスは [`abs`] で組む。`/usr/bin` は Windows では絶対パスではない
    /// （`is_absolute` は drive を要る）ので、直書きすると Windows でだけ落ちる。
    #[test]
    fn a_bare_shell_name_is_resolved_on_the_clients_path() {
        use super::resolve_bare_name;
        use std::path::Path;
        let none: Vec<String> = Vec::new();
        let local = abs("home/u/.local/bin");
        let usr = abs("usr/bin");
        let path = vec![local.clone(), usr.clone()];
        let at = |list: Vec<std::path::PathBuf>| move |p: &Path| list.iter().any(|x| x == p);
        let s = |p: std::path::PathBuf| p.to_str().unwrap().to_string();

        // ① PATH の先にある方で当たる。
        assert_eq!(
            resolve_bare_name(
                Face::Unix,
                "hsh",
                &path,
                &none,
                &at(vec![local.join("hsh"), usr.join("hsh")])
            ),
            s(local.join("hsh"))
        );
        // ② 起こせない物（無い・実行できない）は飛ばして次のディレクトリへ。
        assert_eq!(
            resolve_bare_name(Face::Unix, "hsh", &path, &none, &at(vec![usr.join("hsh")])),
            s(usr.join("hsh"))
        );
        // ③ 見つからなければ名前のまま（デーモンの PATH に任せる・直す前と同じ）。
        assert_eq!(
            resolve_bare_name(Face::Unix, "hsh", &path, &none, &at(vec![])),
            "hsh"
        );
        // ④ 区切りを含む値は、書いた人が場所を言っている。触らない。
        let everything = |_: &Path| true;
        for v in ["/bin/sh", "./hsh", "bin/hsh"] {
            assert_eq!(
                resolve_bare_name(Face::Unix, v, &path, &none, &everything),
                v
            );
        }
        // ⑤ 絶対でない PATH の項目は飛ばす（client の cwd 基準の綴りは、デーモンでは別の場所）。
        let relative = vec![
            std::path::PathBuf::from("."),
            std::path::PathBuf::from("bin"),
        ];
        assert_eq!(
            resolve_bare_name(Face::Unix, "hsh", &relative, &none, &everything),
            "hsh"
        );
        // ⑥ 空は空のまま。
        assert_eq!(
            resolve_bare_name(Face::Unix, "", &path, &none, &everything),
            ""
        );
    }

    /// Windows の面: 名前そのまま → `PATHEXT` の順（portable-pty の `search_path` と同じ）。
    /// Linux の上でも検体にできる（見ているのは探す順番）。
    #[test]
    fn a_bare_shell_name_on_windows_tries_pathext() {
        use super::resolve_bare_name;
        use std::path::Path;
        let ps7 = abs("w/ps7");
        let sys32 = abs("w/sys32");
        let path = vec![ps7.clone(), sys32.clone()];
        let ext = vec![".COM".to_string(), ".EXE".to_string()];
        let at = |list: Vec<std::path::PathBuf>| move |p: &Path| list.iter().any(|x| x == p);
        let s = |p: std::path::PathBuf| p.to_str().unwrap().to_string();
        // 拡張子を補って当たる。
        assert_eq!(
            resolve_bare_name(
                Face::Windows,
                "pwsh",
                &path,
                &ext,
                &at(vec![ps7.join("pwsh.EXE")])
            ),
            s(ps7.join("pwsh.EXE"))
        );
        // 名前そのままが在ればそれ（拡張子を補うより先）。
        assert_eq!(
            resolve_bare_name(
                Face::Windows,
                "powershell.exe",
                &path,
                &ext,
                &at(vec![sys32.join("powershell.exe")])
            ),
            s(sys32.join("powershell.exe"))
        );
        // `\` も区切り。触らない。
        let everything = |_: &Path| true;
        assert_eq!(
            resolve_bare_name(Face::Windows, r"C:\hsh\hsh.exe", &path, &ext, &everything),
            r"C:\hsh\hsh.exe"
        );
        // unix の面では `\` は区切りではない（名前の一部）ので、探しに行く。
        assert_ne!(
            resolve_bare_name(Face::Unix, r"a\b", &path, &ext, &everything),
            r"a\b"
        );
    }

    /// その面で絶対になるパス（unix は `/` の下・Windows は `C:\` の下）。
    fn abs(rel: &str) -> std::path::PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        rel.split('/')
            .fold(std::path::PathBuf::from(root), |p, c| p.join(c))
    }

    /// daemon 側の解決。ここに親の観測は入らない（berthd の親は
    /// 「最初に起こした人」であって、いま頼んできた人ではない。
    #[test]
    fn the_daemon_default_still_answers_when_the_client_says_nothing() {
        // 環境を閉包で作る（在る綴りだけが最後の手になる）。
        let has = |list: &'static [&'static str]| move |p: &str| list.contains(&p);
        let usual = has(&["/bin/bash", "/bin/sh"]);
        assert_eq!(
            resolve_default_shell(Face::Unix, None, Some("/bin/zsh"), None, &usual),
            ("/bin/zsh".to_string(), ShellSrc::EnvShell)
        );
        assert_eq!(
            resolve_default_shell(Face::Unix, None, None, None, &usual),
            ("/bin/bash".to_string(), ShellSrc::Fallback)
        );
        // `$SHELL` が無ければ passwd のログインシェル（在るときだけ）。
        assert_eq!(
            resolve_default_shell(
                Face::Unix,
                None,
                None,
                Some("/usr/bin/fish"),
                &has(&["/usr/bin/fish", "/bin/bash", "/bin/sh"])
            ),
            ("/usr/bin/fish".to_string(), ShellSrc::Passwd)
        );
        assert_eq!(
            resolve_default_shell(Face::Unix, None, None, Some("/usr/bin/fish"), &usual),
            ("/bin/bash".to_string(), ShellSrc::Fallback),
            "passwd の綴りが無ければ飛ばす"
        );
        assert_eq!(
            resolve_default_shell(
                Face::Unix,
                None,
                Some("/bin/zsh"),
                Some("/usr/bin/fish"),
                &usual
            ),
            ("/bin/zsh".to_string(), ShellSrc::EnvShell),
            "`$SHELL` の申告は passwd に勝つ（在るかは見ない ＝ 失敗は spawn が言う）"
        );
        // bash の無い環境は `/bin/sh`。どちらも無ければ `/bin/sh` を名指しして spawn に言わせる。
        assert_eq!(
            resolve_default_shell(Face::Unix, None, None, None, &has(&["/bin/sh"])),
            ("/bin/sh".to_string(), ShellSrc::Fallback)
        );
        assert_eq!(
            resolve_default_shell(Face::Unix, None, None, None, &has(&[])),
            ("/bin/sh".to_string(), ShellSrc::Fallback)
        );
        // Windows は passwd も在るかも見ない。
        assert_eq!(
            resolve_default_shell(
                Face::Windows,
                None,
                Some("/bin/bash.exe"),
                Some("/bin/zsh"),
                &has(&[])
            ),
            ("powershell.exe".to_string(), ShellSrc::Fallback)
        );
        assert_eq!(
            resolve_default_shell(Face::Windows, Some("hsh.exe"), None, None, &has(&[])),
            ("hsh.exe".to_string(), ShellSrc::BerthdShell)
        );
    }

    /// 表に無い pid は **居ない**（[`Unread::Gone`]）。理由を割る検体は [`table_with`] を使う。
    fn table<'a>(
        rows: &'a [(i64, &'a str, i64)],
    ) -> impl FnMut(i64) -> Result<(String, i64), Unread> + 'a {
        table_with(rows, Unread::Gone)
    }

    /// 表に無い pid が**どの理由で**読めないかを選べる版。
    fn table_with<'a>(
        rows: &'a [(i64, &'a str, i64)],
        why: Unread,
    ) -> impl FnMut(i64) -> Result<(String, i64), Unread> + 'a {
        move |pid| {
            rows.iter()
                .find(|r| r.0 == pid)
                .map(|r| (r.1.to_string(), r.2))
                .ok_or(why)
        }
    }

    /// ssh で入ったセッションの実際の並び。
    /// 注意: ログに出るのは `[priv]` の sshd の pid。最初の sshd で止めたら結べない。
    #[test]
    fn the_walk_stops_below_pid_1_and_keeps_every_sshd_on_the_way() {
        let rows = [
            (749_352, "sh", 749_351),
            (749_351, "sshd", 749_256),
            (749_256, "sshd", 40_910),
            (40_910, "sshd", 1),
        ];
        let (chain, end) = walk_ancestry(749_352, table(&rows));
        let pids: Vec<i64> = chain.iter().map(|a| a.pid).collect();
        assert_eq!(pids, [749_352, 749_351, 749_256, 40_910]);
        assert_eq!(chain[0].name, "sh");
        assert_eq!(end, AncestryEnd::Root);
    }

    /// 止まり方で意味が違うので、名乗り分けること。
    #[test]
    fn the_walk_says_why_it_stopped() {
        let deep: Vec<(i64, &str, i64)> = (2..40).map(|p| (p, "p", p + 1)).collect();
        let (chain, end) = walk_ancestry(2, table(&deep));
        assert_eq!(
            (chain.len(), end),
            (ANCESTRY_DEPTH, AncestryEnd::DepthLimit)
        );

        let (chain, end) = walk_ancestry(100, table(&[(100, "berth", 90)]));
        assert_eq!((chain.len(), end), (1, AncestryEnd::Vanished));

        let empty: [(i64, &str, i64); 0] = [];
        let (chain, end) = walk_ancestry(5, table(&empty));
        assert_eq!((chain.len(), end), (0, AncestryEnd::SelfUnreadable));
    }

    /// 「読めなかった」を「消えた」と呼ばない。
    ///
    /// macOS の `proc_pidinfo` は、居ない pid に `ESRCH`、root の `login(1)` に `EPERM` を返す。
    /// 同じ並び・同じ深さで止まっても、`src` は別の語を名乗る。
    #[test]
    fn a_parent_we_were_refused_is_not_a_parent_that_vanished() {
        let seat = [(100_i64, "berth", 90_i64)];

        let (gone, end) = walk_ancestry(100, table_with(&seat, Unread::Gone));
        assert_eq!((gone.len(), end), (1, AncestryEnd::Vanished));

        let (denied, end) = walk_ancestry(100, table_with(&seat, Unread::Denied));
        assert_eq!((denied.len(), end), (1, AncestryEnd::Denied));

        let (other, end) = walk_ancestry(100, table_with(&seat, Unread::Unreadable));
        assert_eq!((other.len(), end), (1, AncestryEnd::Unreadable));

        // 並びは 3 つとも同一。違いは `src` にしか出ない（だから `src` が要る）。
        let pids = |c: &[Ancestor]| c.iter().map(|a| a.pid).collect::<Vec<_>>();
        assert_eq!(pids(&gone), pids(&denied));
        assert_eq!(pids(&gone), pids(&other));
    }

    /// 陽性対照: `pid 1` は必ず居て、必ず root の持ち物。
    ///
    /// ∴ 長い版（`PROC_PIDTBSDINFO`）へ戻すと `EPERM` でこの検体が赤くなる。
    /// 名前は主張しない（`launchd` は macOS の実装詳細）。読めたことだけを主張する。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_process_owned_by_root_can_still_be_read() {
        let (name, ppid) = read_parent(1).expect("pid 1 is always there, and always root's");
        assert!(!name.is_empty(), "comm が空 ＝ 構造体を読み違えている");
        assert_eq!(ppid, 0, "pid 1 の親は 0");
    }

    /// 居ない pid は **`Gone`**（`ESRCH`）。上の対照の相方。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_pid_that_is_not_there_is_gone_not_denied() {
        assert_eq!(read_parent(i64::from(i32::MAX)), Err(Unread::Gone));
    }

    /// Windows: 自分から辿れて、`toolhelp` の語彙で名乗る。
    ///
    /// 終わり方は縛らない。この面の普通の終わりは `toolhelp:vanished`
    /// （Windows は親が終わっても pid 1 へ付け替えないので、`explorer.exe` の親は
    /// たいてい一覧に居ない）。∴ `:vanished` は異常の印ではなく、「`toolhelp` で終わること」を
    /// assert すると正しい振る舞いが赤になる。
    #[cfg(windows)]
    #[test]
    fn windows_walks_from_this_very_process() {
        let me = i64::from(std::process::id());
        let a = peer_ancestry(Some(me));
        assert!(
            !a.chain.is_empty(),
            "🚨 自分自身すら並びに出ない ＝ 一枚絵が撮れていない（src={})",
            a.src
        );
        assert_eq!(a.chain[0].pid, me, "0 番目は当のプロセスのはず");
        assert!(
            a.src.starts_with("toolhelp"),
            "🚨 この面の出所は toolhelp のはず（src={})",
            a.src
        );
    }

    /// pid が回ってきたとき、並びは他人へ登ってはいけない（Windows）。
    ///
    /// 稀な事故ではない。300 回の spawn・約 7 秒で pid が 8 回巡る。しかも取り違えた並びは
    /// `toolhelp` / `toolhelp:vanished` で普通に終わるので、見ただけでは分からない。だから時刻で検める。
    ///
    /// 本物の再利用を待たない。待つ検体は非決定的になる。
    /// 時刻を注入できる形（[`rejecting_recycled_pids`]）にしてあるのがそのため。
    #[test]
    fn a_parent_younger_than_its_child_is_a_recycled_pid_not_a_parent() {
        // berth(100) <- 親として記録されている 90 <- その親 80
        let seat = [
            (100_i64, "berth.exe", 90_i64),
            (90, "stranger.exe", 80),
            (80, "root.exe", 0),
        ];
        let at = |times: &'static [(i64, i64)]| {
            move |p: i64| times.iter().find(|(k, _)| *k == p).map(|(_, t)| *t)
        };

        // 陽性: 90 は 100 より新しいため pid が回ってきている。
        //    ▶ 100 で切れ、他人の `stranger.exe` は並びに乗らない。
        let (chain, end) = walk_ancestry(
            100,
            rejecting_recycled_pids(table(&seat), at(&[(100, 500), (90, 900), (80, 100)])),
        );
        assert_eq!(
            (chain.len(), end),
            (1, AncestryEnd::Vanished),
            "子より新しい親は親ではない——本当の親は消えている（{chain:?}）"
        );

        // 陰性①: 時刻が整合していれば上まで辿る。
        let (chain, end) = walk_ancestry(
            100,
            rejecting_recycled_pids(table(&seat), at(&[(100, 500), (90, 300), (80, 100)])),
        );
        assert_eq!((chain.len(), end), (3, AncestryEnd::Root));

        // 陰性②: 時刻が読めないときは切らない。
        //    「検められなかった」を「嘘だった」にしない（`sshd` を越える道を残す）。
        let (chain, end) = walk_ancestry(100, rejecting_recycled_pids(table(&seat), |_| None));
        assert_eq!((chain.len(), end), (3, AncestryEnd::Root));

        // 陰性③: 途中が読めなくても、境界は捨てない。
        //    90 の時刻は不明だが、80 は 100 より新しいのでここで切れる。
        let (chain, end) = walk_ancestry(
            100,
            rejecting_recycled_pids(table(&seat), at(&[(100, 500), (80, 900)])),
        );
        assert_eq!(
            (chain.len(), end),
            (2, AncestryEnd::Vanished),
            "最後に分かった時刻と比べれば推移的に正しい（{chain:?}）"
        );
    }

    /// 居ない pid は空の並びで、しかも理由を名乗る（`unimplemented` ではなく）。
    #[cfg(windows)]
    #[test]
    fn windows_says_why_when_the_pid_is_not_there() {
        let a = peer_ancestry(Some(i64::from(i32::MAX)));
        assert!(a.chain.is_empty());
        // 0 段目が読めないため辿る先が無い（`SelfUnreadable`）。
        assert_eq!(a.src, "unavailable:toolhelp");
    }

    /// pid が取れていなければ辿る先が無い。黙って空にせずそう名乗る。
    #[test]
    fn no_pid_means_no_walk_and_says_so() {
        let a = peer_ancestry(None);
        assert!(a.chain.is_empty());
        // pid が無いのは面の話ではないので、3 つの面とも同じ名乗り。
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert_eq!(a.src, "unavailable:peer_pid");
    }

    #[test]
    fn control_roundtrips_through_rheon_envelope() {
        let c = Control::Spawn {
            session_id: None,
            name: None,
            shell: Some("zsh".into()),
            shell_src: Some("parent_shell".into()),
            cwd: Some("/tmp/t".into()),
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        };
        let bytes = encode_control(SOURCE, &c);
        // エンベロープが Rheon Protocol v1 の形であること。
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["source"], "berthd");
        assert_eq!(v["type"], "session");
        assert_eq!(v["event"], "spawn");
        assert_eq!(v["cols"], 80);
        // 時刻は `ts`。旧綴りが同時に出ていないことも見る。
        // 「両方書く」で移行したつもりになると、消費者はどちらを信じるか決められない。
        assert!(v.get("ts").is_some(), "ts が無い");
        assert!(v.get("modified").is_none(), "旧綴り modified が残っている");
        // decode で Control に戻る（envelope フィールドは無視される）。
        match decode_control(&bytes).unwrap() {
            Control::Spawn {
                shell, cols, rows, ..
            } => {
                assert_eq!(shell.as_deref(), Some("zsh"));
                assert_eq!((cols, rows), (80, 24));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn frame_roundtrips_over_a_pipe() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = Frame::data(0, b"hello\x1b[0m world".to_vec());
        write_frame(&mut a, &sent).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got.kind, KIND_DATA);
        assert_eq!(got.serial, 0);
        assert_eq!(got.payload, sent.payload);
    }
}
