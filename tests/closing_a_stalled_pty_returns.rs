//! **止まった PTY を閉じても返ってくること。**
//!
//! # 何が起きるか
//!
//! Windows で、子が ConPTY の前口上で止まったまま検体が落ちると、素の `drop(master)` が返らない。
//! 「遅い」ではなく、外から切るまで終わらない（掛けた `timeout` にそのまま張り付き、上限が無い）。
//!
//! # 待っているのはホスト
//!
//! | 疑い | 確かめた結果 |
//! |---|---|
//! | 読み手の clone が受け口を握っている | 誰も持たない形でも返らない |
//! | 子が死んでいない | `drop(child)` はすぐ返り、子は死んでいる |
//! | unwind の途中で詰まる | assert の前へ出しても同じ所で止まる |
//!
//! 生き残っているのは `OpenConsole.exe`（ConPTY のホスト）。`ClosePseudoConsole` が待っているのは
//! ホストの終了で、そのホストは DSR の答えを待ったまま動けない。閉じる前に答えれば解ける（数 ms で返る）。
//!
//! # なぜ「期限つきで諦める」を第一の手にしないか
//!
//! 諦めると、そのたびに `OpenConsole.exe` が 1 本ずつ残り、親が消えても掃除されない（孤児として残る）。
//! 答える方は漏れない。∴ 期限は最後の手で、そこへ落ちたら [`common::PtyTerminal::close`] が必ず名乗る。
//!
//! # 注意: この検体の意味は面で違う
//!
//! unix にはこの問い（DSR）が無いので、あちらでは「閉じるのが速い」を測るだけの
//! 緑になる。「両面で同じ物を測った」と読まないこと。

mod common;

use portable_pty::{native_pty_system, PtySize};
use std::time::{Duration, Instant};

#[test]
fn closing_a_pty_whose_host_never_got_its_answer_still_returns() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // 子は何でもよい（止めているのは子ではなくホストなので）。
    let child = common::PtyChildGuard::new(
        pair.slave
            .spawn_command(common::trivial_child())
            .expect("spawn"),
    );
    drop(pair.slave);

    // 注意: **意図的に答えない番人**を据える（これによりホストが前口上で止まったままになる）。
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let terminal = common::pump_pty_without_answering(&*pair.master, Some(seen.clone()));

    // ホストが前口上を出し切るだけの間を置く。
    std::thread::sleep(Duration::from_millis(1500));

    // 注意: 子を先に落とす（答えは子の stdin へ入るバイトでもある）。
    drop(child);

    let t = Instant::now();
    let outcome = terminal.close(pair.master);
    let took = t.elapsed();

    // 時間ではなく結末で判定する（[`common::PtyClose`]）。
    //
    // 注意: `took < 5s` のような時間の判定にしないこと。それが検出器として働くのは
    // 期限が assert の閾より長い間だけで、期限を閾より短くすると「諦めて `OpenConsole.exe` を
    // 漏らした」回も assert を通り、緑になる（すなわち、数を下げるだけの変更が検出器を黙らせる）。
    // 結末で判定していれば、期限の値を縮めても検体は嘘をつかない。
    //
    // `took` は捨てない（落ちたときに「どれだけ張り付いたか」が読み筋になる）。
    assert_eq!(
        outcome,
        common::PtyClose::Returned,
        "止まった PTY を閉じられず期限へ落ちた（{took:?}）—— \
         答えが効いていない（`OpenConsole.exe` が 1 本漏れている）: \
         pty から読めたのは {} バイト",
        seen.lock().unwrap().len()
    );
}
