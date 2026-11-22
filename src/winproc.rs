//! Windows のプロセスの扱い（DLL の探し方・job object・作成フラグ）。

/// 裸の名前の `LoadLibrary` から **PATH を外す**。
///
/// # なぜ要るか
///
/// `portable-pty` は ConPTY を `LoadLibrary("conpty.dll")` の裸の名前で探す。意図は
/// 「アプリと一緒に配った `conpty.dll` と `OpenConsole.exe` を使う」だが、裸の名前は探索順が
/// PATH まで落ちるので、まったく別のアプリが PATH に置いた `conpty.dll` を掴みうる。
/// 重要: 掴んだ ConPTY が古いと、`berth kill-server -f` が返らず、ホストが 1 コアを使い切って
/// 回り続ける形になる（PATH からその 1 ディレクトリを外すだけで直る）。
///
/// # 直し方
///
/// `LOAD_LIBRARY_SEARCH_DEFAULT_DIRS` は探索をアプリのディレクトリ ＋ System32 ＋ 明示的に足した
/// 場所に限り、PATH を外す。∴ 一緒に配った `conpty.dll` は今までどおり効き、他人が PATH に置いた物
/// だけが効かなくなる。
///
/// - `SetDllDirectoryW("")` では足りない（あれが外すのはカレントディレクトリで、PATH は残る）。
/// - 注意: プロセス全体に効くので、以後の `LoadLibrary` も PATH を見ない。berthd が読む DLL は
///   System32 と自分の隣だけ。
/// - **最初の pty より前**に呼ぶこと（`portable-pty` は初回の pty 生成で遅延ロードする）。
/// - 失敗しても止めない（「今までどおり PATH も見る」に戻るだけ）。ただし黙らない。
#[cfg(windows)]
pub fn pin_dll_search() {
    use windows_sys::Win32::System::LibraryLoader::{
        SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    };
    // SAFETY: 引数はフラグ 1 つ。プロセス全体の DLL 探索順を狭めるだけで、解放するものは無い。
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) } == 0 {
        eprintln!(
            "[berthd] DLL の探索順から PATH を外せませんでした —— \
             PATH に conpty.dll を置いているアプリが在ると、その ConPTY で動きます"
        );
    }
}

/// このプロセスと、その子孫を、1 つの job に入れる（kill-on-close）。
///
/// Windows の ConPTY は別プロセスのホスト（`OpenConsole.exe`）を立てる。berthd が正規に畳めば
/// `master` の `Drop` が擬似コンソールを閉じるが、強制終了（`TerminateProcess` / タスクマネージャ /
/// OS のシャットダウン）では `Drop` は走らず、ホストは親を失って 1 セッションにつき 1 本残り、
/// CPU を使い切って回り続ける。∴ `Drop` に依らない道が要る。job object は OS が畳むので、
/// どう死んでも（クラッシュ・強制終了・シャットダウン）子孫ごと消える。
///
/// 重要: **handle を閉じない**（`CloseHandle` を呼ばない）。job は最後の handle が閉じたときに
/// 中身を殺すので、ここで閉じると自分の子を今すぐ殺すことになる。プロセスの死が handle を閉じ、
/// それが引き金になる形にする。`mem::forget` は要らない（`HANDLE` は生ポインタで `Copy`・
/// 落とす物が無い）。
///
/// 失敗しても止めない（job に入れないだけで berthd は動く）。ただし黙らない。
/// 「入れなかった」は、後で残ったホストを数える人に効く事実なので。
#[cfg(windows)]
pub fn join_kill_on_close_job() {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            eprintln!(
                "[berthd] job object を作れませんでした —— 強制終了時に pty のホストが残りえます"
            );
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            eprintln!("[berthd] job object に kill-on-close を設定できませんでした");
            return;
        }
        // 既に別の job に入っていると失敗しうる（Windows 8 以降は入れ子を許すが、
        // breakaway を禁じた job の下では失敗する）。そのときは名乗って続ける。
        if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            eprintln!("[berthd] job object に入れませんでした（既に別の job の下かもしれません）");
            return;
        }
        // 閉じない。閉じた瞬間に中身（自分と子）が死ぬ。
        // `CloseHandle(job)` を書かないことが、この関数の要点。
        let _ = job;
    }
}

/// `berth` が berthd を起こすときの Windows の作成フラグ。**起こした端末のコンソールに繋がない**。
///
/// # なぜ `DETACHED_PROCESS` が要るか
///
/// stdio を null にしても、子は親の**コンソール**には繋がったまま起きる（stdio のハンドルと
/// コンソールへの所属は別の軸）。端末の窓（Windows Terminal など）を閉じると、conhost がその
/// コンソールに繋がる全プロセスへ `CTRL_CLOSE_EVENT` を配って畳む。これにより `berth attach` が起こした
/// berthd は窓と一緒に死に、セッションも消える。job から出す `CREATE_BREAKAWAY_FROM_JOB` が
/// 通っていても死ぬ（job とコンソールは別の軸）。
///
/// `DETACHED_PROCESS` の子はコンソールを持たないので、そのイベントの配達先に入らない。
/// berthd はコンソールの API を使わない（ConPTY は自分のコンソールを要らない）ので、無くて困らない。
///
/// `breakaway` は job の軸。祖先が許さなければ `ERROR_ACCESS_DENIED` で断られるので、
/// 呼び手は `false` で撃ち直す。注意: コンソールの旗は撃ち直しでも降ろさない。
///
/// 検体: `tests/the_daemon_is_not_attached_to_the_console.rs`（`GetConsoleProcessList`:
/// その close イベントが届く集合に、起こした子が居ないこと。対照つき）。
#[cfg(windows)]
#[must_use]
pub fn daemon_creation_flags(breakaway: bool) -> u32 {
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS};
    if breakaway {
        CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS
    } else {
        DETACHED_PROCESS
    }
}
