//! 読む側が先に閉じた stdout（`berth info | head -3` の `head` が抜けた後）へ書いても、
//! `berth` は panic せず**黙って終わる**。Rust は起動時に SIGPIPE を無視するので、素の `println!` は
//! EPIPE で panic（rc 101・stderr に `failed printing to stdout: Broken pipe`）になる。
//!
//! 読む側を**先に**閉じたパイプを渡すので、書く側は必ず EPIPE を踏む（競合しない陽性対照）。

#![cfg(unix)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

fn a_pipe_whose_reader_is_already_closed() -> OwnedFd {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: 長さ 2 の配列を渡す素の pipe(2)。
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe(2)");
    // SAFETY: 今作った読む側を閉じる（以後、書く側は EPIPE）。
    unsafe { libc::close(fds[0]) };
    // SAFETY: 今作った書く側の fd を 1 回だけ所有させる。
    unsafe { OwnedFd::from_raw_fd(fds[1]) }
}

fn run(args: &[&str], out: &OwnedFd) -> (Option<i32>, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(args)
        .env("BERTHD_SOCKET", berthd::scratch_endpoint("closed-stdout"))
        .env("BERTHD_NO_SPAWN", "1")
        .stdout(Stdio::from(out.try_clone().expect("dup the write end")))
        .stderr(Stdio::piped())
        .output()
        .expect("spawn berth");
    (
        o.status.code(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn a_closed_stdout_does_not_panic() {
    let w = a_pipe_whose_reader_is_already_closed();
    // 誰にも訊かずに答える 2 つ: 読む側が居なければ**黙って成功**で終わる。
    for args in [&["--version"][..], &["--help"][..]] {
        let (code, err) = run(args, &w);
        assert!(
            !err.contains("panicked") && !err.contains("Broken pipe"),
            "🚨 {args:?}: stdout が閉じているだけで panic した: {err}"
        );
        assert_eq!(code, Some(0), "{args:?}: rc が 0 ではない（stderr: {err})");
    }
    // berthd に訊く 2 つ（居ないので「居ない」と言う経路）: 落ちないことだけを見る（rc は経路の約束に従う）。
    for args in [&["ls"][..], &["info"][..]] {
        let (code, err) = run(args, &w);
        assert!(
            !err.contains("panicked") && !err.contains("Broken pipe"),
            "🚨 {args:?}: stdout が閉じているだけで panic した: {err}"
        );
        assert_ne!(code, Some(101), "{args:?}: panic の rc（stderr: {err})");
    }
}
