use crate::output::{PtyInput, SessionOutput};
use berthd::SessionInfo;
use portable_pty::MasterPty;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{mpsc, Mutex as TokioMutex};

/// pty の master を io ループと `List` で共有する箱。
///
/// `Option` なのは畳む順番を守るため。io ループの末尾は `drop(master)` の位置と
/// 前後関係が 3 者（`child.wait()` / job の terminate / ConPTY の close）で決まっている。
/// 重要: Arc に持たせたまま最後の参照者に落とさせると、その順序が session 表の掃除に移る。
/// ∴ io ループの末尾では `take()` して同じ場所で落とす。取り出した後は `None`（すなわち
/// 「もう訊けない」状態であり、session はその直後に表から消える）。
pub(crate) type SharedMaster = Arc<StdMutex<Option<Box<dyn MasterPty + Send>>>>;

/// pty の書き口。書き込みスレッドが握り、io ループの末尾が畳む順序の中で取り出す。
pub(crate) type SharedWriter = Arc<StdMutex<Option<Box<dyn Write + Send>>>>;

pub(crate) struct Session {
    pub(crate) input_tx: mpsc::Sender<PtyInput>,
    /// 打鍵の送り口（書き込みスレッド行き）。注意: 送るときは表のロックの外で待つこと。
    pub(crate) write_tx: mpsc::Sender<Vec<u8>>,
    pub(crate) output: Arc<StdMutex<SessionOutput>>,
    pub(crate) info: SessionInfo,
    /// 子シェルの pid。`List` のたびに現在 cwd を引くのに使う（[`live_cwd_with_src`]）。
    pub(crate) pid: Option<u32>,
    /// pty の master。`List` と attach が今の大きさを引くのに使う（[`live_size`]）。
    pub(crate) master: SharedMaster,
}

/// pty の**今の**大きさを引く（`(cols, rows)`）。
///
/// [`SessionInfo`] の `cols`/`rows` は spawn 時に記録した値で、その後の resize を追わない。
/// `cwd` と同じ形（[`live_cwd_with_src`] の doc を見よ）。一覧が「作った時の大きさ」のままでは
/// 「今どの大きさで動いているか」に答えられず、attach する前に知りたい場面で嘘をつく
/// （別の大きさで attach し直しても、中で `stty rows 11 cols 33` を撃っても、一覧は作った時のまま）。
///
/// # 面で「今」の意味が違う
///
/// | 面 | `get_size()` の中身 | 何が見えるか |
/// |---|---|---|
/// | unix | master fd への `TIOCGWINSZ`（測定） | resize もセッションの中の `stty` も見える |
/// | Windows | ConPTY が最後に設定した値を返す（控え） | resize は見えるが、控えの外は見えない |
///
/// Windows に `stty` の経路（`ioctl`）は無いので、控えで落ちる物は無い。
pub(crate) fn live_size(master: &SharedMaster) -> Option<(u16, u16)> {
    let guard = master.lock().ok()?;
    let sz = guard.as_ref()?.get_size().ok()?;
    Some((sz.cols, sz.rows))
}

/// 子シェルの**現在の** cwd を、出所と一緒に引く。
///
/// `SessionInfo.cwd` は spawn 時に記録した値で、その後の `cd` を追わない。一覧の cwd が
/// 「起動した場所」のままでは detached session を見分けられず `berth ls` が使い物にならないので、
/// List のたびに実プロセスから取り直す。取れなければ呼び出し側が spawn 時の値へ落ちる。
///
/// # 落ちたことを名乗らせるのがこの関数の仕事
///
/// 出所を返さないと、読む側には「今さっき引いた値」と「起動時の値」が同じ顔で見える
/// （実装の無い面でも、一覧はもっともらしい cwd を返してしまう）。
///
/// 重要: 値と出所を 2 通りに解かないこと（[`berthd::default_shell_with_src`] と同じ作法）。
/// 呼び手が `None` を見て自分で理由を名乗ると、機構が増えた日に呼び手側だけが腐る。
///
/// # 値表（正本は berth-protocol の [`SessionInfo::cwd_src`]）
///
/// | 面 | 引けた | 引けなかった |
/// |---|---|---|
/// | Linux | `proc` | `spawn:unavailable:proc` |
/// | macOS | `proc_pidinfo` | `spawn:unavailable:proc_pidinfo` |
/// | その他（Windows 含む）| — | `spawn:unimplemented:live_cwd` |
///
/// pid が無い場合はここへ来ない。それは `spawn:unavailable:pid` で、呼び手が名乗る。
pub(crate) fn live_cwd_with_src(pid: u32) -> (Option<String>, &'static str) {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Ok(p) => (Some(p.to_string_lossy().into_owned()), "proc"),
            Err(_) => (None, "spawn:unavailable:proc"),
        }
    }
    // macOS に /proc は無い。proc_pidinfo(PROC_PIDVNODEPATHINFO) が current dir (pvi_cdir) を返す。
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if n != size {
            return (None, "spawn:unavailable:proc_pidinfo"); // 子が死んだ / 権限が無い
        }
        // vip_path は `c_char` の 32×32 の二次元配列（libc が古い rustc を支えるための表現）。
        // 実体は連続した MAXPATHLEN バイトなので、平らに均して NUL 終端まで読む。
        let bytes: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|&c| c as u8)
            .take_while(|&b| b != 0)
            .collect();
        if bytes.is_empty() {
            return (None, "spawn:unavailable:proc_pidinfo");
        }
        match String::from_utf8(bytes) {
            Ok(v) => (Some(v), "proc_pidinfo"),
            Err(_) => (None, "spawn:unavailable:proc_pidinfo"),
        }
    }
    // Windows の実装は書かないと決めてある（「まだ」ではなく「やらない」）。
    //
    // Windows には `/proc` も `proc_pidinfo` も無く、公開 API に代案が無い
    // （`Win32_Process` にも .NET の `Process` にも作業ディレクトリの欄は無い）。
    // 書くなら `NtQueryInformationProcess` で PEB の番地を取り、`ReadProcessMemory` で
    // 他プロセスのメモリを読むことになる（観測者というよりデバッガの振る舞い）。
    // 注意: しかも ntdll の準公開 API（将来変わりうる）で、WOW64 では PEB の形も変わる。
    //
    // 買えるのは `berth ls` の 1 列だけなので、代金に見合わない。
    // `unimplemented` と名乗る限り、実装が無くても嘘はつかない。
    //
    // 開け直す引き金は 1 つ。Windows で複数セッションを `berth ls` で選ぶ人が、
    // `(spawn)` では選べないと実際に困ったとき。推測で開け直さないこと。
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        (None, "spawn:unimplemented:live_cwd")
    }
}

pub(crate) type Table = Arc<TokioMutex<HashMap<String, Session>>>;

pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(1);

/// 起こしている途中の session id（まだ表に居ない）。
///
/// 重要: 空きの確認と表への登録の間で pty と子を起こすので、その間に同じ id の spawn が来ると
/// 両方が確認を通る。∴ 確認と同じロックの中で、ここに予約しておく。
pub(crate) static SPAWNING: StdMutex<std::collections::BTreeSet<String>> =
    StdMutex::new(std::collections::BTreeSet::new());

/// [`SPAWNING`] の予約。登録に成功しても失敗しても、落ちた時点で外す。
pub(crate) struct SpawnReservation(pub(crate) String);

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if let Ok(mut set) = SPAWNING.lock() {
            set.remove(&self.0);
        }
    }
}

/// 既定名 `session-N` を、空いている最小の N（tmux の window 採番式）で作る。
///
/// 番号は人間が見る飾りで、reattach/kill の identity は id（`shell-7` 等・client 発番）が持つため、
/// 番号の再利用は安全。死んだセッションは exit で table から消える（`run_pty_io_loop`）ので、
/// 「使用中の名前集合」は今生きているセッションだけを指す。単調増加だと長命 daemon で `session-4712` の
/// ように青天井になるのを避け、「今いくつ開いているか」を映す小さな番号に保つ。
/// **table のロックを握ったまま**呼ぶこと（採番→insert の間に別 spawn が同番号を採るのを防ぐ）。
fn lowest_free_session_name(table: &HashMap<String, Session>) -> String {
    lowest_free_name(table.values().map(|s| s.info.name.as_str()))
}

/// 作る時点の名前: client が渡した名前が使えるならそれ、駄目なら既定名。
///
/// 名前を spawn の後の `Rename` で付けると、spawn の瞬間に berthd が知っているのは既定名だけになり、
/// 記録の `spawn:started` の `name` が全部 `session-1` になり、同時に生きている 2 本が同名になる
/// （名前付きのセッションしか居ないと、空いている最小はいつも 1）。∴ 作る時点で受け取る。
/// 注意: 使えない名前で断らない。ここは起こす所で、断るのは後続の `Rename` の仕事
/// （そこで理由が返る）。既定名に落として起こす。
pub(crate) fn initial_session_name(
    desired: Option<&str>,
    table: &HashMap<String, Session>,
) -> String {
    if let Some(name) = desired {
        let taken = table.values().map(|s| s.info.name.clone());
        if validate_name(name, taken).is_ok() {
            return name.trim().to_string();
        }
    }
    lowest_free_session_name(table)
}

/// 既存名のイテレータから空いている最小の `session-N` を作る純粋関数（`lowest_free_session_name`
/// の核・テスト可能に分離）。`session-` 接頭でない名前（rename 済み等）は無視する。
fn lowest_free_name<'a>(existing: impl Iterator<Item = &'a str>) -> String {
    let used: HashSet<u64> = existing
        .filter_map(|n| n.strip_prefix("session-"))
        .filter_map(|n| n.parse::<u64>().ok())
        .collect();
    let mut n = 1u64;
    while used.contains(&n) {
        n += 1;
    }
    format!("session-{n}")
}

/// 名前として使えるか（`Rename` の検証）。名前で attach できることが名前の存在理由なので、
/// 空・空白入り・重複は拒否する。id と紛れる形（`shell-*`）も避ける。
pub(crate) fn validate_name(name: &str, taken: impl Iterator<Item = String>) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("name must not be empty".into());
    }
    if n.chars().any(char::is_whitespace) {
        return Err("name must not contain whitespace".into());
    }
    // `;` は OSC 2746（attach 信号）のフィールド区切り。名前に混ざると信号が壊れ、消費側で handle が
    // 切り詰められて別セッション名に化ける（`attach;my;work` → arg=`my`）。
    if n.contains(';') {
        return Err("name must not contain ';'".into());
    }
    if taken.into_iter().any(|t| t == n) {
        return Err(format!("name already in use: {n}"));
    }
    Ok(())
}

/// このマシンの hostname（`Welcome.host`、すなわち `berth info` が「どの箱の berthd か」を名乗るため）。
/// env `HOSTNAME`（daemon では未設定のことが多い）→ Windows は env `COMPUTERNAME` →
/// `hostname` コマンド → "unknown"。プロセス寿命で不変なので一度だけ引いて使い回す。
/// 新規 dep を足さないため libc でなく外部コマンド。
///
/// # 重要: pty が生きている最中に、ここで子を起こしてはいけない
///
/// Windows では、`openpty` の直後に `hostname` を起こすと、その session のシェルが
/// 1 バイトも出さなくなる（記録を Spawn の返事の前に書く経路が、その場所でここを通る）。
/// 引き金はここまで確かめてある: 引数を定数に替えると起きず、コードを触らずに `HOSTNAME` を
/// export するだけでも起きない。注意: 起きる検体と起きない検体が在り、その差の理由は調べていない。
///
/// 塞ぎ方は 2 枚で、どちらか片方にしない:
/// 1. 起動時に一度引いておく（`main` の accept ループ直前）。pty が 1 つも無いうちに
///    `OnceLock` を埋めるので、以後どこから呼ばれても子は起きない。こちらが本体で、
///    呼び出し元が増えても効く。
/// 2. Windows では `COMPUTERNAME` を見る。あの面では常に在るので、そもそも子を起こさない。
///    注意: これだけだと、環境を剥がされた起動（`HOSTNAME` も `COMPUTERNAME` も空）で外れる。
pub(crate) fn hostname() -> String {
    use std::sync::OnceLock;
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                // 重要: Windows でここに落ちると子が起きる（上の doc）。あの面は常に持っている。
                if cfg!(windows) {
                    std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty())
                } else {
                    None
                }
            })
            .or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "unknown".to_string())
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::{lowest_free_name, validate_name};

    #[test]
    fn name_must_not_contain_the_osc_field_separator() {
        // `;` は OSC 2746 のフィールド区切り。名前に許すと attach 信号が壊れる（consumer で切り詰め）。
        assert!(validate_name("my;work", std::iter::empty()).is_err());
        // 空白（既存ルール）と空も拒否。
        assert!(validate_name("my work", std::iter::empty()).is_err());
        assert!(validate_name("", std::iter::empty()).is_err());
        // 素直な handle は通る。
        assert!(validate_name("iris", std::iter::empty()).is_ok());
    }

    #[test]
    fn first_name_is_one() {
        assert_eq!(lowest_free_name(std::iter::empty()), "session-1");
    }

    #[test]
    fn fills_the_lowest_gap_not_the_max_plus_one() {
        // session-1 と session-3 が生きている → 空きの最小は 2（単調増加なら 4 になっていた）。
        let names = ["session-1", "session-3"];
        assert_eq!(lowest_free_name(names.into_iter()), "session-2");
    }

    #[test]
    fn appends_after_a_contiguous_run() {
        let names = ["session-1", "session-2", "session-3"];
        assert_eq!(lowest_free_name(names.into_iter()), "session-4");
    }

    #[test]
    fn ignores_renamed_and_malformed_names() {
        // rename 済み（`iris`）や壊れた名前は session-N の空き計算に混ぜない。
        let names = ["iris", "session-2", "session-abc", "session-"];
        assert_eq!(lowest_free_name(names.into_iter()), "session-1");
    }

    #[test]
    fn reuses_a_freed_low_number() {
        // session-2 が死んで抜けた（table から消えている）→ 次はその 2 を埋める。
        let names = ["session-1", "session-3", "session-4"];
        assert_eq!(lowest_free_name(names.into_iter()), "session-2");
    }
}
