//! **裸の名前の `LoadLibrary` が PATH を見ないこと。**
//!
//! # なぜ検体が要るか（外しても多くの環境では赤くならない）
//!
//! この対処の効き目は「PATH に `conpty.dll` を置いているアプリが入っている環境」でしか現れない
//! （そこでは対処が無いと `berth kill-server -f` が返らない）。入っていない環境では、
//! 対処をすべて消しても全部緑のまま。
//! ∴ 機構そのものを撃つ検体を置く（`conpty.dll` は使わず、**PATH だけを問う**）。
//!
//! 注意: Windows 専用。`SetDefaultDllDirectories` はこの面にしか無い。
#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 裸の名前で読めるか。絶対パスでは訊かない（問いは「探索順が PATH まで落ちるか」）。
fn loadable(name: &str) -> bool {
    let h = unsafe { LoadLibraryW(wide(name).as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe {
        FreeLibrary(h);
    }
    true
}

/// 注意: この検体は 1 プロセスに 1 回しか成立しない。`SetDefaultDllDirectories` は
/// 取り消せないので、陽性対照（設定する前）を**同じ関数の中で先に**取る。
/// だから独立したファイルに置いてある（他の検体と同居させない）。
///
/// 注意: 2 回とも同じ名前で訊かない。一度読み込んだ DLL は名前で再要求すると
/// 探索せずに手元の物を返すので、2 回目は常に成功して偽の緑になる。
/// ∴ 写しを 2 つ作って、別々の名前で訊く。
#[test]
fn a_bare_name_stops_reaching_path_once_the_search_is_pinned() {
    let dir = std::env::temp_dir().join(format!("berthd-dllsearch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    // 中身は何でもよい（読めさえすれば）。System32 の物を写して名前だけ変える。
    let src = std::path::Path::new(r"C:\Windows\System32\version.dll");
    assert!(src.exists(), "🚨 写し元が無い: {}", src.display());
    let before_name = "zz_berthd_probe_before.dll";
    let after_name = "zz_berthd_probe_after.dll";
    std::fs::copy(src, dir.join(before_name)).expect("copy before");
    std::fs::copy(src, dir.join(after_name)).expect("copy after");

    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{};{old_path}", dir.display()));

    // 陽性対照（設定する前は PATH まで落ちる）。
    // これが偽なら、この検体は何も測っていない（PATH が効いていないだけ）。
    assert!(
        loadable(before_name),
        "🚨 陽性対照が立たない —— 裸の名前が PATH の写しに当たらなかった。\n\
         この検体は「PATH を外せた」を測れていない（外す前から当たっていない）。\n\
         置いた場所: {}",
        dir.display()
    );

    berthd::pin_dll_search();

    assert!(
        !loadable(after_name),
        "🚨 探索順から PATH が外れていない。\n\
         ⭐ 意味: 利用者の PATH に `conpty.dll` を置いているアプリが在ると、\n\
            berthd はその ConPTY で動く —— `kill-server` が返らなくなる。\n\
         置いた場所: {}",
        dir.display()
    );

    // 隣（自分の exe のディレクトリ）が今も効くことは、ここでは測っていない。
    // 上流の意図（*deployed alongside the application*）はそちら。
    std::fs::remove_dir_all(&dir).ok();
}
