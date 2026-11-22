use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// 起動 1 本分の job を作る。失敗しても `None`（session は今までどおり動く）。
///
/// `isize` で返す。`HANDLE` は生ポインタで `Send` ではないため、
/// `tokio::spawn` へ渡すタスクに持たせられない。境界でだけ生の型に戻す。
pub fn create() -> Option<isize> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return None;
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
        )
    };
    if ok == 0 {
        unsafe { CloseHandle(job) };
        return None;
    }
    Some(job as isize)
}

/// 子を job に入れる。
///
/// 注意: 入れる前に子が孫を起こしたら、その孫は入らない（会員は spawn の瞬間に継承される）。
/// その窓は塞げていない（`portable-pty` は suspended 起動の口を出していないため、
/// spawn の直後に入れるのが今できる最短。「無い」ではなく「狭い」）。
pub fn add(job: isize, pid: u32) -> bool {
    let job = job as HANDLE;
    let h = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    if h.is_null() {
        return false;
    }
    let ok = unsafe { AssignProcessToJobObject(job, h) };
    unsafe { CloseHandle(h) };
    ok != 0
}

/// 木ごと止めて、ハンドルを閉じる。
pub fn terminate(job: isize) {
    let job = job as HANDLE;
    unsafe {
        TerminateJobObject(job, 1);
        CloseHandle(job);
    }
}
