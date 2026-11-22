//! 検体が berthd と話すための最小の口。
//!
//! `send_ctrl` / `read_ctrl` のような補助は、検体のファイルごとに写しを持つと片方だけ直って
//! もう片方が腐る。検体をまたいで使う物はここに 1 つだけ置く。

// `dead_code` を許すのはここだけ。`tests/common/mod.rs` は各テストバイナリへ別々に
// コンパイルされるので、どのヘルパにも「使わない側」が必ず在る。
// 個々に `#[allow]` を貼ると、本当に誰も使わなくなった物が見分けられなくなるので
// ファイル単位で許す（消すかどうかは grep で数えて決める）。
#![allow(dead_code)]

use berthd::{connect_daemon_at, KIND_CONTROL};
use berthd::{
    control_frame, decode_control, read_frame, write_frame, Control, DaemonStream, Frame,
};
use std::time::Duration;

pub async fn send_ctrl(s: &mut DaemonStream, serial: u32, c: &Control) {
    write_frame(s, &control_frame(berthd::SOURCE, serial, c))
        .await
        .unwrap();
}

/// **制御フレームが来るまで読む**（data フレームは読み飛ばす）。
///
/// 最初の 1 フレームが制御だとは限らない（シェルや PTY が先に何か出していれば data フレームが
/// 先に着く）。製品の client（`src/bin/berth/wire.rs::next_control` / `commands.rs::cmd_info`）も
/// 同じ読み方をしている。
///
/// Windows では必ず起きる: ConPTY は PTY を開いた瞬間に 20 バイトの前口上
/// （`ESC[?9001h` / `ESC[?1004h` / `ESC[6n`）を出し、これが制御応答より先に着くことがある。
///
/// 注意: 「最初の 1 フレームは制御」と決め打ちに戻さないこと（何か出た回だけ落ちる検体になる）。
pub async fn read_ctrl(s: &mut DaemonStream) -> Control {
    loop {
        let f = read_frame(s).await.unwrap();
        if f.kind != KIND_CONTROL {
            continue;
        }
        let n = f.payload.len().min(80);
        // 黙って落ちないよう、何が来たかを名乗る（`decode` の失敗は契約違反なので、
        // 読み飛ばさずここで止める）。
        return decode_control(&f.payload).unwrap_or_else(|e| {
            panic!(
                "制御フレームを待っていたが kind={} serial={} len={} 先頭={:?} ({e})",
                f.kind,
                f.serial,
                f.payload.len(),
                String::from_utf8_lossy(&f.payload[..n])
            )
        });
    }
}

/// **子プロセスの番人**（`Drop` で `kill()` してから `wait()` する）。
///
/// 後始末を関数の末尾に書くと、`assert` が panic した瞬間にそこへ届かず、子が残る。
/// panic を越えて走るのは `Drop` だけなので、殺すのはここに置く。
///
/// 残ったときの症状は OS で違う。Windows は走っているバイナリを消せないので、落ちた検体が
/// 残した berthd が次のビルドを折る（`failed to remove file …berthd.exe`）。unix は `unlink`
/// できるので次のビルドは通るが、残っていること自体は同じ。
///
/// `wait()` までやるのは、`TerminateProcess` が非同期に返るため（`kill()` だけでは、まだ
/// イメージが握られている瞬間が残る）。
pub struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    #[must_use]
    pub fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    /// 番人が持っている子の pid。取り出さない（`Drop` の後始末を壊さない）。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.0.as_ref().map(std::process::Child::id)
    }

    /// 正常経路で待つ。子を**取り出す**ので、この後 `Drop` は何もしない。
    ///
    /// # Errors
    /// 子の待ち受けに失敗したとき。
    pub fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        self.0
            .take()
            .expect("ChildGuard から子を 2 度取り出した")
            .wait_with_output()
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("ChildGuard は既に空")
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("ChildGuard は既に空")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // 既に終わっていれば `kill` は失敗する（unix: ESRCH）。それは正常。
            let _ = child.kill();
            // `wait` まで来て初めてイメージが解放される。
            let _ = child.wait();
        }
    }
}

/// デーモンが待ち始めるまで待って繋ぐ。
///
/// 端点の綴りは知らない（unix なら socket path、Windows なら名前付きパイプの名前）。
/// 面の知識は `berth_protocol::connect_daemon_at` の中だけに在る。
pub async fn wait_for_daemon(endpoint: &std::ffi::OsStr, within: Duration) -> DaemonStream {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Ok(s) = connect_daemon_at(endpoint).await {
            return s;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("berthd never came up at {}", endpoint.to_string_lossy());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
/// 一時の家と端点で起こした berthd 1 本。
///
/// 注意: daemon の後始末は必ず `Drop` で行うこと（テスト本体で `kill` するだけでは足りない）。
/// assert が落ちると残りの行は走らないので daemon が生き残り、継承した stdout/stderr を掴んだままに
/// なる。すると `cargo test` の出力読み取りが EOF を見ず、失敗ではなくハングになる。
pub struct Fixture {
    pub dir: std::path::PathBuf,
    pub endpoint: std::ffi::OsString,
    pub journal: std::path::PathBuf,
    daemon: Option<ChildGuard>,
}

impl Fixture {
    /// 起こした daemon の pid（その子を数える検体が使う）。
    /// `ChildGuard` から覗くだけで、所有権は動かさない。
    #[must_use]
    pub fn daemon_pid(&self) -> Option<u32> {
        self.daemon.as_ref().and_then(ChildGuard::pid)
    }

    pub fn start(tag: &str) -> Self {
        Self::start_with_env(tag, &[])
    }

    /// **daemon の環境を指定して**起こす。
    ///
    /// 「berthd の環境で決まっていないこと」を測る検体は、デーモンに意図的に別の値（`SHELL` など）を
    /// 持たせる必要がある。持たせないと `cargo test` を走らせた環境の値がそのまま入り、
    /// 何と何を比べたのか言えなくなる。
    pub fn start_with_env(tag: &str, env: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("berthd-journal-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 意図的に緩い家から始める（`~/.berth` が既に 0755 で存在する実環境の再現）。
        // berthd が締め直すことまで込みで見たいので、テストの初期条件を良くしない。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // 端点の形は面で違う: unix はこの `dir` の下の socket（権限を測る検体が
        // その位置に依存しているので動かさない）、Windows は名前空間の名前。
        #[cfg(unix)]
        let endpoint: std::ffi::OsString = {
            let sock = dir.join("berthd.sock");
            std::fs::remove_file(&sock).ok();
            sock.into_os_string()
        };
        #[cfg(not(unix))]
        let endpoint: std::ffi::OsString = berthd::scratch_endpoint(tag);
        let journal = dir.join("journal.jsonl");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"));
        command
            .env("BERTHD_SOCKET", &endpoint)
            .env("BERTHD_JOURNAL_PATH", &journal);
        for (k, v) in env {
            command.env(k, v);
        }
        let daemon = command.spawn().map(ChildGuard::new).expect("spawn berthd");
        Self {
            dir,
            endpoint,
            journal,
            daemon: Some(daemon),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 番人を**先に**落とす（unix では socket がこの `dir` の下に在るので、
        // 生きている berthd の足元を消さない）。
        // ここで `kill()` を書き直さないこと（殺すのは番人の仕事）。
        drop(self.daemon.take());
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// pid が生きているか。
///
/// 生死は pid で見る。job の会員かでは見ない（`IsProcessInJob(child, NULL)` は
/// breakaway した子としない子を区別しないので、「job に入っているか」は答えにならない）。
pub fn alive(pid: u32) -> bool {
    // pid 0 を「生きている」と読まない（0 は Idle で、必ず引けるため、
    // spawn に失敗して pid が 0 のままの腕が「生きている」に化ける）。
    if pid == 0 {
        return false;
    }
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// **その pid の子のうち、コンソールのホスト**。返すのは `(pid, 名前)`。
///
/// 名前だけで探さない（`conhost.exe` はほかの端末やセッションの分も常に何本も居るので、
/// 親で絞る）。
///
/// 見る名前は 2 つ: `conhost.exe`（OS 標準）と `OpenConsole.exe`（sideload された ConPTY が
/// 立てる方）。どちらが立っても数える。
#[cfg(windows)]
#[must_use]
pub fn console_hosts_of(parent: u32) -> Vec<(u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: 旗と 0 を渡すだけ。返った handle は下で必ず閉じる。
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut e = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..Default::default()
    };
    let mut found = Vec::new();
    // SAFETY: `snap` は有効な handle、`e` は `dwSize` を埋めた PROCESSENTRY32W。
    let mut more = unsafe { Process32FirstW(snap, &raw mut e) } != 0;
    while more {
        if e.th32ParentProcessID == parent {
            let n = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..n]);
            if name.eq_ignore_ascii_case("conhost.exe")
                || name.eq_ignore_ascii_case("OpenConsole.exe")
            {
                found.push((e.th32ProcessID, name));
            }
        }
        // SAFETY: 同上。
        more = unsafe { Process32NextW(snap, &raw mut e) } != 0;
    }
    // SAFETY: 上で得た handle を 1 度だけ閉じる。
    unsafe { CloseHandle(snap) };
    found
}

/// **端末になって、シェルが応えることを確かめてから返る**。
///
/// # なぜ「静かになった」では足りないのか
///
/// ConPTY は DSR に答えるまで進まないので、まずそれに答える。問題はその後:
/// powershell は banner を出したあと間を置いてプロンプトを出すので、「静かになった」を「落ち着いた」とは読めない。
/// そこで抜けると banner までで抜け、打った行がプロンプトの前に消える。
/// 無音の回数で待つのは時間の代理でしかなく、スイート全体を並列で回すと、その間が待ちを超える
/// 回が出る。待ち時間を伸ばしても同じ代理を大きくしただけで、負荷が増えればまた越える。
///
/// # 応えたことを見てから返る
///
/// 無音で粗く待ったあと、合図を打って、それが返るまで打ち直す。
/// 合図は `echo` なので何度打っても同じ（打ち直しが副作用を持たない）。
///
/// 注意: 合図の出力はペインにも replay にも残る。呼び手はどれも「含むか」しか見ていないので
/// 害は無いが、厳密なバイト数を数える検体を書くならここを思い出すこと。
pub async fn become_a_terminal(s: &mut DaemonStream, within: Duration) -> Vec<u8> {
    const READY: &[u8] = b"BERTHD_READY";
    let deadline = tokio::time::Instant::now() + within;
    let mut seen = Vec::new();
    let mut answered = false;
    let mut quiet = 0u32;

    // ① DSR に答え、ひとまず静かになるまで待つ（粗い待ち）。
    while tokio::time::Instant::now() < deadline {
        let got = drain_data(s, Duration::from_millis(400)).await;
        seen.extend_from_slice(&got);
        if !answered && contains(&seen, DSR_REQUEST) {
            answered = true;
            write_frame(s, &Frame::data(0, DSR_REPLY.to_vec()))
                .await
                .unwrap();
            continue;
        }
        if got.is_empty() && !seen.is_empty() {
            quiet += 1;
            if quiet >= 4 {
                break;
            }
        } else {
            quiet = 0;
        }
    }

    // ② 応えるまで打ち直す（ここが代理ではなく観測）。
    let mut line = READY.to_vec();
    #[cfg(unix)]
    line.push(b'\n');
    #[cfg(not(unix))]
    line.extend_from_slice(b"\r\n");
    let mut probe = b"echo ".to_vec();
    probe.extend_from_slice(&line);
    while tokio::time::Instant::now() < deadline {
        write_frame(s, &Frame::data(0, probe.clone()))
            .await
            .unwrap();
        let mut waited = Duration::ZERO;
        while waited < Duration::from_secs(3) && tokio::time::Instant::now() < deadline {
            let got = drain_data(s, Duration::from_millis(200)).await;
            waited += Duration::from_millis(200);
            seen.extend_from_slice(&got);
            // 打った行そのものの echo でも足りる（2 つ目の出現を待たない）。ConPTY の echo も
            // 「シェルが読んだ」証拠で、読まずに echo は返らない。
            if contains(&seen, READY) {
                return seen;
            }
        }
    }
    seen
}

/// `within` の間に次のフレームが届き始めたら、そのフレームを最後まで読んで返す。
/// 届き始めなければ `None`。
///
/// 注意: `timeout(…, read_frame(s))` と書かないこと。`read_frame` は長さ・種類・通し番号・中身を
/// 順に読むので、途中で時間切れが来ると読んだ分が捨てられ、その接続は以後ずっとフレームの
/// 境目がずれる（長さの 4 バイトだけ失うと、次の読みはデータのフレームを「長さ 1 の制御
/// フレーム」として返す）。書く側も 1 フレームを数回に分けて書くので、忙しいマシンほど
/// 分かれ目に時間切れが落ちる。
///
/// ∴ 時間切れを掛けるのは最初の 1 バイトだけにする（1 回の `read` は、取り消されたら
/// 何も読んでいない）。読み始めたフレームは時間切れなしで読み切る（書く側は 1 フレームを
/// 続けて書くので、残りはすぐ届く）。
pub async fn read_frame_within<R>(s: &mut R, within: Duration) -> Option<std::io::Result<Frame>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut first = [0u8; 1];
    match tokio::time::timeout(within, s.read(&mut first)).await {
        Err(_) => None,
        Ok(Err(e)) => Some(Err(e)),
        Ok(Ok(0)) => Some(Err(std::io::ErrorKind::UnexpectedEof.into())),
        Ok(Ok(_)) => {
            let mut whole = (&first[..]).chain(s);
            Some(read_frame(&mut whole).await)
        }
    }
}

/// `within` の間に来たデータ payload を集める（制御フレームは読み飛ばす）。
async fn drain_data(s: &mut DaemonStream, within: Duration) -> Vec<u8> {
    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        let left = deadline - tokio::time::Instant::now();
        match read_frame_within(s, left).await {
            Some(Ok(f)) if f.kind == berthd::KIND_DATA => acc.extend_from_slice(&f.payload),
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    acc
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// pid が `secs` 秒以内に消えるか。消えたら `true`。
pub fn wait_gone(pid: u32, secs: u64) -> bool {
    for _ in 0..(secs * 4) {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// ConPTY が端末へ出すカーソル位置の問い合わせ（DSR）。
pub const DSR_REQUEST: &[u8] = b"[6n";
/// その答え（1 行 1 桁）。値は何でもよく、返すこと自体が要件。
pub const DSR_REPLY: &[u8] = b"[1;1R";

/// **PTY の master を読み続け、ConPTY の問いに答える番人**（検体が「端末のふり」をする）。
///
/// # なぜ「読むだけ」では足りないか
///
/// ConPTY は PTY を開いた瞬間に 20 バイトの前口上を出し、その 3 本目（`ESC[6n`: DSR）に
/// 答えるまで 1 バイトも先へ進まない。∴ 読んで捨てるだけの pump では、子は起動しても何も出さず、
/// 何もしない。答える半分を落とした pump は、Windows でだけ「何も来ない」で落ち、しかも
/// 外から切るまで終わらない。
///
/// # 注意: 答えるのは Windows だけ
///
/// unix の PTY はこの問いを出さないので、無条件に答えると子の stdin へ余計なバイトを
/// 流し込むことになる。∴ 面で囲う。
///
/// `sink` を渡すと読んだバイトをそこへ溜める（検体が中身を見たいとき）。要らなければ `None`。
pub fn pump_pty_as_terminal(
    master: &(dyn portable_pty::MasterPty + Send),
    sink: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
) -> PtyTerminal {
    spawn_pump(master, sink, true)
}

/// **答えない番人**（PTY を閉じる検体専用）。
///
/// 走っている間 DSR に答えないので、ConPTY のホストが前口上で止まったままになる。
/// ∴ [`PtyTerminal::close`] がその状態からでも返ることを測れる。ほかの検体では使わないこと。
pub fn pump_pty_without_answering(
    master: &(dyn portable_pty::MasterPty + Send),
    sink: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
) -> PtyTerminal {
    spawn_pump(master, sink, false)
}

fn spawn_pump(
    master: &(dyn portable_pty::MasterPty + Send),
    sink: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    answer: bool,
) -> PtyTerminal {
    let mut reader = master.try_clone_reader().expect("pty reader");
    // unix では DSR が来ないので `answer` を読む所が無い。そのままだと未使用警告が出る。
    #[cfg(unix)]
    let _ = answer;
    // `take_writer()` は 1 回きり（2 度目は `writer already taken` で panic）。
    // ∴ 後始末で使う口も、ここで取った 1 本を共有する。
    #[cfg(not(unix))]
    let writer = std::sync::Arc::new(std::sync::Mutex::new(
        master.take_writer().expect("pty writer"),
    ));
    #[cfg(not(unix))]
    let writer_for_pump = std::sync::Arc::clone(&writer);
    let pump = std::thread::spawn(move || {
        // 利用者と同じ `#[cfg]` を付ける（unix 側だけ `unused_imports` で鳴るため）。
        #[cfg(not(unix))]
        use std::io::Write as _;
        #[cfg(not(unix))]
        let mut answered = false;
        #[cfg(not(unix))]
        let mut seen: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut reader, &mut tmp) {
                // `Err` も抜ける（slave 全閉で EIO / EOF になる面が在る）。
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(s) = sink.as_ref() {
                        s.lock().unwrap().extend_from_slice(&tmp[..n]);
                    }
                    #[cfg(not(unix))]
                    {
                        seen.extend_from_slice(&tmp[..n]);
                        if answer
                            && !answered
                            && seen.windows(DSR_REQUEST.len()).any(|w| w == DSR_REQUEST)
                        {
                            // 位置は何でもよい（1;1 と答える）。答えたことだけが要る。
                            if let Ok(mut w) = writer_for_pump.lock() {
                                let _ = w.write_all(DSR_REPLY);
                                let _ = w.flush();
                            }
                            answered = true;
                        }
                    }
                }
            }
        }
    });
    PtyTerminal {
        pump: Some(pump),
        #[cfg(not(unix))]
        writer,
    }
}

/// **何もせずすぐ終わる子**（面で綴りが違う）。
///
/// 検体が「pty の中に子が要る」だけで、子が何をするかは問わないときに使う
/// （PTY を閉じる検体: 止めているのは子ではなく ConPTY のホスト）。
pub fn trivial_child() -> portable_pty::CommandBuilder {
    #[cfg(windows)]
    {
        let mut c = portable_pty::CommandBuilder::new("cmd.exe");
        c.arg("/c");
        c.arg("echo berthd-trivial-child");
        c
    }
    #[cfg(unix)]
    {
        let mut c = portable_pty::CommandBuilder::new("/bin/echo");
        c.arg("berthd-trivial-child");
        c
    }
}

/// 番人の取っ手。PTY を閉じるのもこの人の仕事（[`PtyTerminal::close`]）。
/// `drop(master)` をどれだけ待ってやるか。
///
/// この数に「正しい値」は無い。閉じられるときは数ミリ秒で返り、閉じられないときは上限が無い
/// （掛けた timeout に張り付く）。∴ 境目をどこに置いても結果は同じで、この数が決めているのは
/// 「壊れている走をどれだけ引き延ばすか」だけ。
///
/// 判定は [`PtyClose`] が持つので、この数は検出には使われていない。
/// 遅い環境（負荷の高い CI・wine）で偽陽性が出るかは測っていない（出たら `GaveUp` が名乗る）。
const CLOSE_DEADLINE: Duration = Duration::from_secs(3);

/// [`PtyTerminal::close`] の結末。**時間ではなく事実**。
///
/// # なぜ返り値が要るか
///
/// 経過時間で判定する形（`took < 5s` なら成功）は、諦める期限が assert の閾値より長いときしか
/// 成立しない。期限を閾値より短くすると、諦めた走も `took` が閾値を下回って assert が通り、
/// `OpenConsole.exe` を漏らしたまま緑になる（数を下げるだけの変更が検出器を黙らせる）。
///
/// `close()` は自分が期限へ落ちたかを知っているので、それを返す。期限の値は
/// 「どれだけ待ってやるか」だけになる。
#[must_use = "閉じられたかどうかを捨てると、OpenConsole.exe が漏れたことに誰も気づかない"]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PtyClose {
    /// `drop(master)` が期限内に返った。
    Returned,
    /// 期限へ落ちて**諦めた**。注意: Windows では `OpenConsole.exe` が 1 本残る（親が消えても掃除されない）。
    GaveUp,
}

pub struct PtyTerminal {
    pump: Option<std::thread::JoinHandle<()>>,
    #[cfg(not(unix))]
    writer: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
}

impl PtyTerminal {
    /// **PTY を閉じる**。順番に意味がある。
    ///
    /// # 素の `drop(master)` は返らないことが在る
    ///
    /// 子が ConPTY の前口上で止まったまま検体が落ちると、`drop(master)` は返らない（上限が無い）。
    /// 読み手の clone が受け口を握っているせいでも、子が死んでいないせいでも、unwind の途中だから
    /// でもない（どれも外して確かめた）。生き残っているのは `OpenConsole.exe`（ConPTY のホスト）で、
    /// `ClosePseudoConsole` が待っているのはそのホストの終了、ホストは DSR の答えを待ったまま
    /// 動けない。
    ///
    /// # 閉じる前に答える
    ///
    /// 答えを書いてから閉じると数ミリ秒で返り、何も漏れない。
    /// 「期限つきで諦める」だけだと、走るたびに `OpenConsole.exe` を 1 本ずつ漏らす（親が消えても
    /// 掃除されない）。∴ 諦めるのは最後の手で、そこへ落ちたことは必ず名乗る。
    ///
    /// # 注意: 呼ぶ順番
    ///
    /// **子を先に落としてから**呼ぶこと。答えは子の stdin へ入るバイトでもあるので、
    /// 生きている子に余計な `CPR` を渡さない（[`pump_pty_as_terminal`] と同じ理由）。
    ///
    /// `pump` は join しない。止まったホストでは join も返らない。スレッドはプロセス終了で
    /// 回収される。
    pub fn close(mut self, master: Box<dyn portable_pty::MasterPty + Send>) -> PtyClose {
        // ① ホストを動けるようにする（Windows だけ・unix はこの問いを出さない）。
        #[cfg(not(unix))]
        {
            use std::io::Write as _;
            if let Ok(mut w) = self.writer.lock() {
                let _ = w.write_all(DSR_REPLY);
                let _ = w.flush();
            }
        }
        // ② 期限つきで閉じる。①が効いていれば数ミリ秒で返る。
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            drop(master);
            let _ = tx.send(());
        });
        let outcome = if rx.recv_timeout(CLOSE_DEADLINE).is_ok() {
            PtyClose::Returned
        } else {
            // 黙って諦めない。漏れる物を名指しで刷る。
            // 刷るのは人が走を見ているときのため。判定は返り値が持つ（stderr は
            // 検体の assert に届かないので、これだけでは検出器にならない）。
            eprintln!(
                "🚨 PTY を {CLOSE_DEADLINE:?} で閉じられなかった —— **諦めた**。
                    ⚠️ `OpenConsole.exe` が 1 本残る（親が消えても掃除されない・実測）。
                    ⭐ ①の DSR 応答が効かなかった ＝ **別の理由でホストが止まっている**。"
            );
            PtyClose::GaveUp
        };
        // pump は join しない（上の doc）。
        let _ = self.pump.take();
        outcome
    }
}

/// **portable-pty の子の番人**。
///
/// [`ChildGuard`] はここでは使えない（あちらは `std::process::Child` 専用で、pty の子は
/// `Box<dyn portable_pty::Child>` という別の型）。メソッドの綴り（`kill` / `wait`）は同じでも、
/// 共通の trait を通していないので 1 つの番人では包めない。
///
/// 注意: 子を起こす所を `.spawn()` で探すと、これを取りこぼす（綴りが `spawn_command` だから）。
pub struct PtyChildGuard(Box<dyn portable_pty::Child + Send + Sync>);

/// 陰性対照の 1 本（session と無関係な `ping -n 300 127.0.0.1`）。**drop で必ず殺す**。
///
/// `std::process::Child` は drop では殺さない。検体が途中の assert で落ちると `kill()` の行に
/// 届かず、ping が 300 秒生き残る。しかも stderr を親から継いでいると、`cargo test 2>&1 | tail` の
/// 形で受けたときパイプの EOF が ping の寿命まで来ない。
/// ∴ stdio は 3 つとも null、終わり方は Drop に任せる（panic の unwind でも走る）。
pub struct Outsider(std::process::Child);

impl Outsider {
    pub fn spawn() -> Self {
        let child = std::process::Command::new("ping")
            .args(["-n", "300", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("outsider");
        Self(child)
    }

    pub fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Outsider {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl PtyChildGuard {
    pub fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self(child)
    }

    /// pty の中の子の pid（外から signal を撃つために要る）。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.0.process_id()
    }

    /// 期限つきで子の終了を待つ。`None` は期限内に終わらなかったことを示す（殺すのは Drop）。
    pub fn wait_within(&mut self, within: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if let Ok(Some(s)) = self.0.try_wait() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

/// `buf` に `needle` が現れるまで待つ（pty の番人が溜めた列を見る検体用）。
pub async fn wait_for_bytes(
    buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    needle: &[u8],
    within: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if buf
            .lock()
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

impl Drop for PtyChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
