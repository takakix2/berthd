//! **版を訊くことが、状態を変えてはならない。**
//!
//! berthd が argv を見ずに daemon を起動すると、`berthd --version` は版を答えず
//! **デーモンを 1 つ増やして居座る**。問い合わせのつもりの操作が副作用を持つ形で、しかも
//! 配備中など版を確かめたい場面ほど踏む。
//!
//! `berth` 側は逆向きの同じ話。版が `berth info`（daemon の自己申告）でしか見えないと、
//! daemon を止めている最中は client の版が分からない。自分の版は誰にも訊く必要がないので、自分で答える。

use std::process::{Command, Output, Stdio};

// この検体は `send_ctrl` / `read_ctrl` を使わない。`dead_code` は `common/mod.rs` がファイル単位で
// 許しているので、ここには書かない（外側にも在ると clippy が `duplicated attribute` で赤くなる）。
mod common;
use std::time::{Duration, Instant};

/// 期限付きで走らせる。**返らなければ殺して落とす。**
///
/// 注意: これが要るのは、守っているバグの失敗の仕方が**ハング**だから。素の
/// `output()` で書くと、退行したときテストは落ちずに固まる（daemon が継いだ pipe を
/// 掴んだまま終わらない）。
fn run_bounded(bin: &str, arg: &str, sock: &std::path::Path) -> Output {
    let mut child = Command::new(bin)
        .arg(arg)
        .env("BERTHD_SOCKET", sock)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(common::ChildGuard::new)
        .unwrap_or_else(|e| panic!("spawn {bin} {arg}: {e}"));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => return child.wait_with_output().expect("collect output"),
            None if Instant::now() >= deadline => {
                panic!(
                    "{bin} {arg} was still running after 10s — it started a daemon instead of \
                     answering the question (that is the bug this test exists for)"
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// テスト用の socket path（**既定の socket を絶対に触らせない**）。
///
/// 注意: このテストの主題は「daemon が生えないこと」なので、万一生えたときに
/// **本番の socket を掴ませない**ことが前提条件になる。生えた瞬間に他のテストや
/// 実運用の berthd と喧嘩しては、何を測ったのか分からなくなる。
fn isolated_socket(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("berthd-who-{tag}-{}.sock", std::process::id()))
}

/// `berthd --version` は**答えて、終わる**（daemon を残さない）。
#[test]
fn berthd_answers_its_version_without_becoming_a_daemon() {
    let sock = isolated_socket("ver");
    std::fs::remove_file(&sock).ok();

    let out = run_bounded(env!("CARGO_BIN_EXE_berthd"), "--version", &sock);

    assert!(
        out.status.success(),
        "berthd --version must exit 0: {out:?}"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("berthd "),
        "expected a version line, got: {text:?}"
    );
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "the version line must carry the version: {text:?}"
    );
    // **副作用が無いことの証拠**: socket を作っていない（listen していない）。
    assert!(
        !sock.exists(),
        "asking for the version must not leave a listening daemon behind"
    );
}

/// `berthd --help` も同じ（**使い方を読むためにデーモンを増やさない**）。
#[test]
fn berthd_prints_usage_without_becoming_a_daemon() {
    let sock = isolated_socket("help");
    std::fs::remove_file(&sock).ok();

    let out = run_bounded(env!("CARGO_BIN_EXE_berthd"), "--help", &sock);

    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("USAGE"));
    assert!(!sock.exists(), "help must not start a daemon");
}

/// **知らない引数は黙って無視しない。**
///
/// 無視するということは「起動していい」と読み替えるということ。
/// タイプミス（`berthd --verison`）で**デーモンが増える**より、断って終わる方がよい。
#[test]
fn an_unknown_argument_is_refused_rather_than_ignored() {
    let sock = isolated_socket("unknown");
    std::fs::remove_file(&sock).ok();

    let out = run_bounded(env!("CARGO_BIN_EXE_berthd"), "--verison", &sock); // 意図的な綴り間違い

    assert_eq!(
        out.status.code(),
        Some(2),
        "expected a usage error: {out:?}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown argument"),
        "it must say what it did not understand: {out:?}"
    );
    assert!(
        !sock.exists(),
        "a typo must not leave a daemon listening — that is the whole bug"
    );
}

/// `berth --version` は **berthd に訊かずに**答える（daemon が居なくても答えられる）。
///
/// 注意: `BERTHD_SOCKET` を**存在しない path**に向けて走らせるのが要点。もし内部で
/// berthd に問い合わせていれば、ここで失敗するか autostart してしまう。
#[test]
fn the_client_knows_its_own_version_with_no_daemon_around() {
    let sock = isolated_socket("client");
    std::fs::remove_file(&sock).ok();

    let out = run_bounded(env!("CARGO_BIN_EXE_berth"), "--version", &sock);

    assert!(out.status.success(), "berth --version must exit 0: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("berth "),
        "expected a version line: {text:?}"
    );
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text:?}");
    // client が「訊かずに答えた」ことの証拠。autostart もしていない。
    assert!(!sock.exists(), "the client must not have started anything");
}

/// 重要: **skew の警告は stdout に出てはいけない。**
///
/// `scripts/install.sh` は入れる前に `berth ls` を撃ち、stdout の行数で
/// 「走っているデーモンにセッションが在るか」を数える（在れば入れずに断る。
/// 人のシェルを落とす判断は installer の仕事ではないので）。
///
/// 版が食い違うとき CLI は警告を 3 行出す。それが stdout に出ると
/// 「セッションが 3 つ在る」と読まれ、installer はまさにその入れ替えを拒否する。
/// 古いデーモンを置き換えるための道具が、古いデーモンが居ることを理由に止まる。
/// しかもエラーには見えない（親切な拒否の顔をする）。
///
/// ∴ 出口の分かれ方を釘で留める: 警告は stderr、一覧は stdout。
/// この検体は同じリポの `install.sh` の数え方を守っている（片方だけ動くと気づけない）。
#[test]
fn the_skew_warning_stays_off_stdout() {
    let src = include_str!("../src/bin/berth/wire.rs");
    // 警告の本文を持つ行の**手前**が `eprintln!` であること。
    let i = src
        .find("the running berthd is old")
        .expect("skew の警告文が見つからない（文言を変えたなら、この検体も一緒に直すこと）");
    let before = &src[..i];
    let macro_start = before
        .rfind("println!")
        .expect("警告が println 系のマクロで出されていない");
    assert!(
        before[..macro_start].ends_with("e"),
        "🚨 skew の警告が **stdout** に出ています（`eprintln!` ではない）。\n\
         scripts/install.sh は `berth ls` の stdout の行数でセッションを数えるので、\n\
         この 3 行が「セッション 3 つ」に化けて、入れ替えが拒否されます。"
    );
}
