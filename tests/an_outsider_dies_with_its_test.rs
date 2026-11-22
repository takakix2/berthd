//! 検体の陰性対照（`common::Outsider`・`ping -n 300`）は、**検体が途中で落ちても**残らない。
//!
//! 残ると、検体が assert で落ちた回に ping が 300 秒生き残る。stderr を継いでいるので
//! `cargo test 2>&1 | tail` のようなパイプが閉じず、呼んだ側がその間黙る。
//! ここは guard の性質だけを測る（panic の unwind で Drop が走り、pid が消えること）。
//! 注意: `ping` を起こすので Windows でも unix でも走るが、`-n` は Windows の綴り（unix の ping は `-c`）。
#![cfg(windows)]

mod common;

#[test]
fn an_outsider_dies_when_the_test_panics() {
    let pid = std::panic::catch_unwind(|| {
        let outsider = common::Outsider::spawn();
        let pid = outsider.pid();
        assert!(
            common::alive(pid),
            "🚨 陰性対照が立っていない ＝ この検体は何も測れていない"
        );
        // 検体が途中で落ちる形。guard はこの unwind で drop される。
        panic!("simulated assertion failure ({pid})");
    })
    .expect_err("the closure must panic");
    let pid: u32 = pid
        .downcast_ref::<String>()
        .and_then(|s| s.rsplit('(').next())
        .and_then(|s| s.trim_end_matches(')').parse().ok())
        .expect("pid in the panic message");
    assert!(
        common::wait_gone(pid, 5),
        "🔴 検体が落ちた後も陰性対照 (pid {pid}) が生き残っている"
    );
}
