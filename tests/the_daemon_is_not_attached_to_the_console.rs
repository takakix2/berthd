//! **`berth` が起こす berthd は、起こした端末のコンソールに繋がっていない。**
//!
//! # 何を測るか: 窓を閉じなくても「閉じたら死ぬ集合」は読める
//!
//! Windows Terminal の窓を × で閉じると、conhost は **そのコンソールに繋がる全プロセス**へ
//! `CTRL_CLOSE_EVENT` を配って畳む。その集合は `GetConsoleProcessList` がそのまま返す。
//! ∴ 「窓を閉じても berthd が生きる」とは「berthd がこの一覧に居ない」ことである。窓も job も要らない。
//!
//! 旗（[`berthd::daemon_creation_flags`]）が無いと、`berth attach` が起こした berthd は窓を閉じると死ぬ。
//! 自分のコンソールを持たせて起こすと生きる。
//!
//! 対照つき: 素の `Command` の子は一覧に**居る**（居なければ、一覧ではなく計器を疑う）。
//! 注意: Windows 専用。コンソールの無い環境（CI のサービス）では「測れない」と名乗って通す。
#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use windows_sys::Win32::System::Console::GetConsoleProcessList;

/// このプロセスのコンソールに繋がる pid の一覧（コンソールが無ければ空）。
fn console_pids() -> Vec<u32> {
    let mut buf = vec![0u32; 512];
    // SAFETY: `buf` は書き込み可能な u32 の配列で、長さを一緒に渡している（API の契約どおり）。
    let n = unsafe { GetConsoleProcessList(buf.as_mut_ptr(), buf.len() as u32) };
    buf.truncate(n as usize);
    buf
}

/// 30 秒だけ生きる子（`ping` は stdio が null でも走る）。
fn sleeper() -> Command {
    let mut c = Command::new("ping.exe");
    c.args(["-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

#[test]
fn the_daemon_is_outside_the_set_that_a_window_close_would_kill() {
    if console_pids().is_empty() {
        eprintln!("⏸ コンソールが無い —— 測れない（緑ではない）");
        return;
    }
    // 対照: 素で起こした子はコンソールに繋がる（stdio を null にしても）。
    let mut plain = sleeper().spawn().expect("spawn control");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let listed = console_pids();
    assert!(
        listed.contains(&plain.id()),
        "対照が一覧に居ない ＝ 計器が壊れている: {listed:?}"
    );

    // 本体: berth と同じ旗（job の軸は外し、コンソールの軸だけ）。
    let mut cmd = sleeper();
    cmd.creation_flags(berthd::daemon_creation_flags(false));
    let mut detached = cmd.spawn().expect("spawn detached");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let listed = console_pids();
    assert!(
        !listed.contains(&detached.id()),
        "起こした子がこのコンソールに繋がっている ＝ 窓を閉じると一緒に死ぬ: {listed:?}"
    );

    let _ = plain.kill();
    let _ = detached.kill();
    let _ = plain.wait();
    let _ = detached.wait();
}
