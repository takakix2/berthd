//! 名前付きパイプの口に**鍵を付ける**。
//!
//! # 何が無かったか
//!
//! `ServerOptions::new().create(..)` は `lpSecurityAttributes = NULL` で `CreateNamedPipeW` を呼ぶ。
//! ∴ 口には作成者トークンの既定 DACL が付き、そこには `Everyone`（`WD`）と `Anonymous`（`AN`）の
//! 読み取りが入っている。つまり `NT AUTHORITY\ANONYMOUS LOGON` が読み取りで口を開けられる
//! （読み書きは `ACCESS_DENIED`）。
//!
//! 接続した時点で降ってくるバイトは 0。`Welcome` は `Control::Hello` への応答でしか送られないので、
//! 書けない client は何も引けない。∴ 塞ぐ理由は機密性ではなく「口に鍵が無い」ことそのもの。
//!
//! # 誰を通すか: owner + SYSTEM + Administrators
//!
//! Administrators を残してある。理由は unix 側の `0600` が `root` を締め出していないのと同じで、
//! Administrators は所有権を奪えるので、拒否しても何も買えない。「拒否した」と書いてある DACL は、
//! 守っていないのに守ったつもりにさせる方が害が大きい。
//!
//! # 作成時にしか決められない
//!
//! 名前付きパイプは後から締める道が実質無い。∴ **`accept()` が作る 2 本目以降も同じ関数を通すこと**。
//! 片方だけ締めると、2 人目以降の客の口が素のままになる。
//!
//! # 失敗したら既定に落ちない
//!
//! DACL を組めなかったら `bind` ごと失敗させる。黙って既定（`WD` / `AN` に開いた口）へ落ちる作りは、
//! 「鍵を掛けたつもり」を作るだけで、掛かっていないことに誰も気づけない。

use std::io;

use windows_sys::Win32::Foundation::{LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 組み上がった security descriptor。`LocalFree` するために所有する。
pub(crate) struct PipeAcl {
    sd: PSECURITY_DESCRIPTOR,
}

impl PipeAcl {
    /// owner ＋ SYSTEM ＋ Administrators だけを通す DACL を組む。
    ///
    /// # Errors
    /// トークンから SID を引けない / SDDL を descriptor に変換できないときに返す。
    pub(crate) fn owner_only() -> io::Result<Self> {
        let sid = current_user_sid_string()?;
        // `D:` = DACL / `P` = protected（継承を受け取らない）/ `GA` = GENERIC_ALL。
        // `WD`（Everyone）と `AN`（Anonymous）を「書かない」ことが鍵であり、
        // 明示的な拒否 ACE は足さない（拒否は評価順で事故りやすく、ここでは不要）。
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})");
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` は NUL 終端の UTF-16。`sd` は out パラメータで、成功時のみ
        // `LocalAlloc` された領域を指す（`Drop` で `LocalFree` する）。
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { sd })
    }

    /// `CreateNamedPipeW` に渡す `SECURITY_ATTRIBUTES`。
    ///
    /// 注意: 返り値は借用（`self` が生きている間だけ有効）。呼び手はこれをスタックに置き、
    /// そのポインタを tokio へ渡すこと。
    pub(crate) fn security_attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.sd,
            bInheritHandle: 0,
        }
    }
}

impl Drop for PipeAcl {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            // SAFETY: `sd` は `ConvertStringSecurityDescriptorToSecurityDescriptorW` が
            // `LocalAlloc` した領域で、ここでしか解放しない。
            unsafe { LocalFree(self.sd.cast()) };
        }
    }
}

/// 自分の SID を SDDL に埋められる文字列（`S-1-5-21-…`）で返す。
pub(crate) fn current_user_sid_string() -> io::Result<String> {
    // SAFETY: 自プロセスの擬似 handle（閉じない）。
    user_sid_string_of_process(unsafe { GetCurrentProcess() })
}

/// 名前付きパイプの**向こう側**（サーバ）のプロセスの所有者の SID。
///
/// `GetNamedPipeServerProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → トークンの `TokenUser`。
/// 注意: 読めなければ `Err`。呼び手は「自分の物だった」とは読まない（[`crate::refuse_a_server_that_is_not_mine`]）。
pub(crate) fn pipe_server_owner_sid(pipe: std::os::windows::io::RawHandle) -> io::Result<String> {
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let mut pid: u32 = 0;
    // SAFETY: `pipe` は開いているパイプの handle。`pid` は out パラメータ。
    if unsafe { GetNamedPipeServerProcessId(pipe, &raw mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 旗と pid を渡すだけ。返った handle は `ProcessHandle` が閉じる。
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let guard = ProcessHandle(h);
    user_sid_string_of_process(guard.0)
}

/// そのプロセス handle のトークンの `TokenUser` を SID 文字列で。
fn user_sid_string_of_process(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: トークンを読み取り専用で開くだけ。`token` は out パラメータ。
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let guard = TokenHandle(token);
    let mut needed: u32 = 0;
    // 1 回目は必ず失敗する（必要な大きさを訊くだけ）ので戻り値を見ない。
    // SAFETY: バッファ長 0 の問い合わせ。`needed` に必要量が入る。
    unsafe { GetTokenInformation(guard.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` は上で訊いた必要量ちょうど。成功時、先頭に `TOKEN_USER` が置かれる。
    if unsafe {
        GetTokenInformation(
            guard.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 成功した `GetTokenInformation(TokenUser)` の契約。
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: `user.User.Sid` は上のバッファの中の有効な SID。`raw` は out パラメータ。
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut raw) } == 0 || raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    // SAFETY: `raw` は NUL 終端の UTF-16（`ConvertSidToStringSidW` の契約）。
    while unsafe { *raw.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: 上で数えた長さちょうど。
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
    // SAFETY: `raw` は `LocalAlloc` された領域。ここでしか解放しない。
    unsafe { LocalFree(raw.cast()) };
    Ok(s)
}

/// `OpenProcess` で開いた handle を閉じるためだけの器。
struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 自分が開いた handle。
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

/// `OpenProcessToken` で開いた handle を閉じるためだけの器。
struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 自分が開いた handle。
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 陽性対照: 自分が立てたパイプの向こうは自分。同一ユーザーは通る（同じユーザーの別プロセスは疑わない）。
    #[tokio::test]
    async fn a_pipe_served_by_this_process_is_owned_by_me() {
        use std::os::windows::io::AsRawHandle as _;
        let name = probe_name("owner");
        let server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .expect("create");
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .expect("open");
        server.connect().await.expect("connect");
        let theirs = pipe_server_owner_sid(client.as_raw_handle()).expect("server owner sid");
        let mine = current_user_sid_string().expect("own sid");
        assert_eq!(theirs, mine);
        assert!(crate::refuse_a_server_that_is_not_mine(&client).is_ok());
    }

    /// 陽性対照: SID が引けて、SDDL の形をしている。
    #[test]
    fn the_current_user_sid_is_readable_and_looks_like_a_sid() {
        let sid = current_user_sid_string().expect("SID を引けない");
        assert!(sid.starts_with("S-1-"), "SID の形をしていない: {sid}");
    }

    /// 組んだ descriptor が null でない（`security_attributes` が渡せる形）。
    #[test]
    fn the_descriptor_is_built_and_carries_a_non_null_pointer() {
        let acl = PipeAcl::owner_only().expect("DACL を組めない");
        let sa = acl.security_attributes();
        assert!(!sa.lpSecurityDescriptor.is_null());
        assert_eq!(sa.bInheritHandle, 0, "口を子へ継がせない");
    }

    // ───────────────────────────────────────────────────────────────────────
    // 本体: 匿名が口を開けられないこと。
    //
    // 3 本で 1 組。真ん中だけでは足りない。「開けなかった」は
    // 「鍵が効いた」とも「名前を間違えた」とも読めるので、同じ計器で
    // 素の口が開くこと（1 本目）を並べて初めて、差が鍵に帰せられる。
    // ───────────────────────────────────────────────────────────────────────

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{ImpersonateAnonymousToken, RevertToSelf};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
    use windows_sys::Win32::System::Threading::GetCurrentThread;

    /// `GENERIC_READ`。定数の置き場が `windows-sys` の版で動くので、数で書く。
    const GENERIC_READ: u32 = 0x8000_0000;

    /// 口を開けるか試す。`anon` なら `ANONYMOUS LOGON` に化けてから撃つ。
    fn can_open(name: &str, anon: bool) -> bool {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut impersonated = false;
        if anon {
            // SAFETY: 自スレッドを匿名トークンに切り替えるだけ。下で必ず戻す。
            impersonated = unsafe { ImpersonateAnonymousToken(GetCurrentThread()) } != 0;
            assert!(
                impersonated,
                "匿名に化けられなかった ＝ 計器が成立していない"
            );
        }
        // SAFETY: `wide` は NUL 終端。他の引数は定数と null。
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        let opened = h != INVALID_HANDLE_VALUE;
        if opened {
            // SAFETY: 自分が開いた handle。
            unsafe { CloseHandle(h) };
        }
        if impersonated {
            // SAFETY: 直前に化けた分を戻すだけ。
            unsafe { RevertToSelf() };
        }
        opened
    }

    fn probe_name(tag: &str) -> String {
        format!(r"\\.\pipe\berthd-acl-{}-{}", std::process::id(), tag)
    }

    /// 陽性対照: 鍵の無い口は、匿名でも開く。
    ///
    /// 重要: これが緑でなければ下の 2 本は何も言っていない
    /// （名前が違う / 化けが効いていない / そもそも口が無い、と区別できない）。
    #[tokio::test]
    async fn a_door_without_a_lock_opens_for_anonymous() {
        let name = probe_name("open");
        let _server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .expect("素の口を作れない");
        assert!(
            can_open(&name, true),
            "既定 DACL の口が匿名を弾いた ＝ 計器か前提が変わっている"
        );
    }

    /// 本体: 鍵を付けた口は、匿名を弾く。
    #[tokio::test]
    async fn a_locked_door_is_shut_to_anonymous() {
        let name = probe_name("locked");
        let _server = crate::create_pipe_instance_at(&name, true).expect("鍵付きの口を作れない");
        assert!(
            !can_open(&name, true),
            "鍵を付けた口を匿名が開けた ＝ DACL が効いていない"
        );
    }

    /// 陰性対照: 鍵を付けても、自分は締め出されない。
    ///
    /// 注意: ここを落とすと「全部拒否」でも上の 2 本が緑になる。
    #[tokio::test]
    async fn a_locked_door_still_opens_for_its_owner() {
        let name = probe_name("owner");
        let _server = crate::create_pipe_instance_at(&name, true).expect("鍵付きの口を作れない");
        assert!(can_open(&name, false), "鍵が自分まで締め出している");
    }
}
