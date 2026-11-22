//! attach/detach の監査台帳 — 実バイナリが**観測した身元**を書くことを確認する。
//!
//! berthd を挟むと、シェルの journal は「誰が打ったか」を見失う。`berth attach` は身元を運ばずに
//! spawn するので、名乗って ssh してきた相手の身元は境界で消える。env を運んでも解けない。
//! berthd は永続でシェルが attach より長生きし、後から別の誰かが同じセッションに入れるので、
//! 「今打っているのは誰か」は spawn 時の env では表せない。
//! **身元は attach に属する**。だから attach 自体を、client の自称でなく peer credential で記録する。

use berthd::Control;
use std::time::Duration;

mod common;
use common::{read_ctrl, send_ctrl, wait_for_daemon, Fixture};

/// **カーネルが答えた身元**が、面ごとに正しい在り方をしていること。
///
/// 同じ性質を、面ごとに別の言い方で主張する:
///
/// | 面 | 主張 |
/// |---|---|
/// | unix | uid と pid の観測が在り、自称を足しても押し流されていない（走らせた自分と一致）|
/// | Windows | pid は在る。uid は無いままである（Windows に uid は無いため発明していない）|
///
/// 「取れない面では測らない」にしない（取れないことを主張にしておかないと、後で埋めた日に
/// 誰も気づかない。自称で `peer_uid` を焼き直すのは沈黙を主張に変えることになる）。
fn assert_the_kernels_answer(rec: &serde_json::Value, ctx: &str) {
    #[cfg(unix)]
    {
        assert_eq!(
            rec["peer_uid"],
            serde_json::json!(unsafe { libc::getuid() }),
            "{ctx}: 観測が押し出されている: {rec}"
        );
        // uid だけでは足りない。「誰が」の観測は uid ＋ pid の対で、daemon 自身の pid が
        // 入っていないことまで見て初めて「相手を見た」と言える。
        assert_eq!(
            rec["peer_pid"],
            serde_json::json!(std::process::id()),
            "{ctx}: peer は**この検体のプロセス**であって daemon ではない: {rec}"
        );
    }
    // unix の peer には区画も載る。Linux では監査セッション id
    // （`/proc/<pid>/sessionid`: ログイン時に張られ子孫に継承される）。
    // 番号を決め打ちしない（環境で違うし、PAM を通っていない箱では未設定になる）。
    // ここで見るのは「実バイナリが実際にその機構を引いたか」だけ。
    // これが無いと、lib の単体の検体が緑でも、デーモンが繋いでいない形が通る。
    #[cfg(target_os = "linux")]
    {
        let src = rec["peer_identity_src"].as_str().unwrap_or_default();
        // 綴りは全体で照合する（`contains` にしない）。`unavailable:proc_sessionid` も
        // 「proc_sessionid」を含むので、「訊けた」と「訊けなかった」が同じ顔で通る。
        // ここでは peer はこの検体自身（生きている）ので `unavailable:` は起きえない。
        assert!(
            matches!(
                src,
                "so_peercred+proc_sessionid" | "so_peercred+proc_sessionid:unset"
            ),
            "{ctx}: 実バイナリが監査セッション id を引いていない: {rec}"
        );
        // 値と名乗りが一致していること。PAM 配下で走らせれば値が載り、そうでなければ
        // `:unset` と名乗る（どちらでも良いが、片方だけは駄目）。
        assert_eq!(
            rec.get("peer_session").is_some(),
            src == "so_peercred+proc_sessionid",
            "{ctx}: 値と `peer_identity_src` が食い違っている: {rec}"
        );
        // この検体は daemon と同じログインセッションで走るので、値の一致は「相手を見た」証拠に
        // ならない（daemon も同じ番号を持つ）。∴ 突き合わせるのはこの検体自身の /proc。
        if let Some(got) = rec.get("peer_session").and_then(serde_json::Value::as_u64) {
            let mine: u64 = std::fs::read_to_string("/proc/self/sessionid")
                .expect("Linux なら在る")
                .trim()
                .parse()
                .expect("数値");
            assert_eq!(got, mine, "{ctx}: 別のプロセスの区画を読んでいる: {rec}");
        }
    }

    // macOS でも同じ性質を、別の機構で主張する。あちらの番号は接続に付いている
    // （`LOCAL_PEERTOKEN`）ので、実バイナリが口を渡しているかはここでしか測れない。
    // 注意: これが無いと、デーモンが口を渡すのをやめても 1 本も赤くならない
    // （Linux は pid の配線を外すと赤くなるが、macOS の「配線」は口そのもの）。
    #[cfg(target_os = "macos")]
    {
        let src = rec["peer_identity_src"].as_str().unwrap_or_default();
        // 綴りは全体で照合する。`unavailable:local_peertoken` も「local_peertoken」を含むので、
        // `contains` では口を渡していなくても緑になる。
        // `:unset` は許す（監査セッションに属していなくても、機構は引いている）。
        assert!(
            matches!(
                src,
                "so_peercred+local_peertoken" | "so_peercred+local_peertoken:unset"
            ),
            "{ctx}: 実バイナリが監査セッション id を引いていない（口を渡していない）: {rec}"
        );
        // 値と名乗りが一致していること（Linux 側と同じ）。
        assert_eq!(
            rec.get("peer_session").is_some(),
            src == "so_peercred+local_peertoken",
            "{ctx}: 値と `peer_identity_src` が食い違っている: {rec}"
        );
        // この検体は daemon と同じ監査セッションで走る（子として起こしている）ので、
        // 値の一致は「相手を見た」証拠にならない。∴ 突き合わせるのはこの検体自身の番号。
        // 自分の番号は socketpair の相手（自分自身）に訊けば取れる。
        // 注意: 完全に独立な経路ではない（同じ `peer_from_cred` を通る）。Linux 側が
        // `/proc/self/sessionid` を直に読むのとはそこが違う。捕まえられるのは
        // 「別の口の番号を書いている」「daemon が自分の値を漏らしている」までで、
        // 機構そのものが揃って壊れた場合は捕まらない（その層は lib の陽性対照が持つ）。
        if let Some(got) = rec.get("peer_session").and_then(serde_json::Value::as_u64) {
            use std::os::fd::AsFd;
            let (sock, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
            let mine = berthd::peer_from_cred(None, Some(1), Some(sock.as_fd()))
                .session
                .expect("値が載っている以上、自分の番号も取れるはず");
            assert_eq!(
                got,
                u64::from(mine),
                "{ctx}: 別の口の区画を読んでいる: {rec}"
            );
        }
    }

    #[cfg(not(unix))]
    {
        // pid は `GetNamedPipeClientProcessId` で埋まる。
        // uid は空のまま（Windows に uid は無く、取れないのではなく存在しない）。
        assert!(
            rec.get("peer_uid").is_none(),
            "{ctx}: Windows に uid は無い —— **発明しない**こと: {rec}"
        );
        assert_eq!(
            rec["peer_pid"],
            serde_json::json!(std::process::id()),
            "{ctx}: peer は**この検体のプロセス**であって daemon ではない: {rec}"
        );
        // 相手の区画（Terminal Services session id）。
        // 番号は決め打ちしない（入り方で違う: ssh は 0・対話ログインは 1 以上）。
        // ここで見たいのは「載っていること」だけ。
        assert!(
            rec.get("peer_session").is_some(),
            "{ctx}: session id が載っていない（後から読めない）: {rec}"
        );
    }

    // 「どの道具が繋いできたか」が読める欄（親プロセスの並び）と、`peer_auid`
    // （Rheon Protocol `spec/ACTOR_v1.md` §2.4）。
    // 並びの 0 番目は attach してきたプロセス（この検体）、1 番目はその親。
    // 名前を決め打ちしない（検体のバイナリ名は環境で違うため、自分の /proc と突き合わせる）。
    #[cfg(target_os = "linux")]
    {
        let src = rec["peer_ancestry_src"].as_str().unwrap_or_default();
        assert!(
            matches!(src, "proc" | "proc:depth_limit"),
            "{ctx}: 親プロセスの並びを引いていない: {rec}"
        );
        let chain = rec["peer_ancestry"]
            .as_array()
            .unwrap_or_else(|| panic!("{ctx}: `peer_ancestry` が無い: {rec}"));
        assert_eq!(
            chain[0]["pid"],
            serde_json::json!(std::process::id()),
            "{ctx}: 並びの 0 番目は**この検体**であるはず: {rec}"
        );
        let comm = std::fs::read_to_string("/proc/self/comm").expect("Linux なら在る");
        assert_eq!(
            chain[0]["name"],
            serde_json::json!(comm.trim_end()),
            "{ctx}: 名前が自分の comm と違う: {rec}"
        );
        // `stat` は最後の `)` の後ろから数える（comm に空白や括弧が入りうる）。
        let stat = std::fs::read_to_string("/proc/self/stat").expect("Linux なら在る");
        let ppid: u64 = stat[stat.rfind(')').expect("stat に `)` が在る") + 1..]
            .split_whitespace()
            .nth(1)
            .expect("ppid の欄")
            .parse()
            .expect("数値");
        if ppid > 1 {
            assert_eq!(
                chain[1]["pid"],
                serde_json::json!(ppid),
                "{ctx}: 並びの 1 番目は**この検体の親**であるはず: {rec}"
            );
        }
        // auid はこの検体自身の `loginuid` と一致すること（daemon の値を漏らしていない）。
        // `4294967295` は値ではなく「未設定」の番兵であるため、そのときは欄ごと無いこと。
        let mine: u64 = std::fs::read_to_string("/proc/self/loginuid")
            .expect("Linux なら在る")
            .trim()
            .parse()
            .expect("数値");
        if mine == u64::from(u32::MAX) {
            assert!(
                rec.get("peer_auid").is_none(),
                "{ctx}: 未設定の auid を値として書いている: {rec}"
            );
        } else {
            assert_eq!(
                rec["peer_auid"],
                serde_json::json!(mine),
                "{ctx}: `peer_auid` がこの検体の loginuid と違う: {rec}"
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        let src = rec["peer_ancestry_src"].as_str().unwrap_or_default();
        // 終わり方まで縛る: 端末が `login(1)`（root）経由でシェルを起こしていても、
        // 並びは `login` の向こうまで辿れる（長い版の `proc_pidinfo` は `EPERM` で断られるので、
        // 実装は短い版を使っている）。
        // ∴ `:denied` / `:vanished` が来たらそれは本当の赤である（「読めなかった」を許す形に緩めないこと）。
        assert!(
            matches!(src, "proc_pidinfo" | "proc_pidinfo:depth_limit"),
            "{ctx}: 親プロセスの並びを引いていない: {rec}"
        );
        let chain = rec["peer_ancestry"]
            .as_array()
            .unwrap_or_else(|| panic!("{ctx}: `peer_ancestry` が無い: {rec}"));
        assert_eq!(
            chain[0]["pid"],
            serde_json::json!(std::process::id()),
            "{ctx}: 並びの 0 番目は**この検体**であるはず: {rec}"
        );
        // auid は同じ監査トークンから取る（socketpair の相手である自分自身に訊いて突き合わせる。
        // （上の `peer_session` と同じ作法・同じ限界）。
        if let Some(got) = rec.get("peer_auid").and_then(serde_json::Value::as_u64) {
            use std::os::fd::AsFd;
            let (sock, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
            let mine = berthd::peer_from_cred(None, Some(1), Some(sock.as_fd()))
                .auid
                .expect("値が載っている以上、自分の auid も取れるはず");
            assert_eq!(
                got,
                u64::from(mine),
                "{ctx}: 別の口の auid を読んでいる: {rec}"
            );
        }
    }
    #[cfg(windows)]
    {
        let src = rec["peer_ancestry_src"].as_str().unwrap_or_default();
        // 終わり方は縛らない。この面の普通の終わりは `toolhelp:vanished` である。
        // Windows は親が終わっても pid 1 へ付け替えないので、上まで辿ると
        // 「親 pid が一枚絵に居ない」で終わる（`explorer.exe` の親）。
        // ∴ macOS のように根まで辿れることを要求すると、正しい振る舞いが赤になる。
        // 代わりに縛るのは「この機構で引いたこと」と「0 番目が自分」の 2 つ。
        assert!(
            src.starts_with("toolhelp"),
            "{ctx}: 親プロセスの並びを引いていない: {rec}"
        );
        let chain = rec["peer_ancestry"]
            .as_array()
            .unwrap_or_else(|| panic!("{ctx}: `peer_ancestry` が無い: {rec}"));
        assert_eq!(
            chain[0]["pid"],
            serde_json::json!(std::process::id()),
            "{ctx}: 並びの 0 番目は**この検体**であるはず: {rec}"
        );
        // auid はこの面に無い（発明していないことを縛る）。
        assert!(
            rec.get("peer_auid").is_none(),
            "{ctx}: Windows で auid を**発明**している: {rec}"
        );
    }
    #[cfg(not(any(unix, windows)))]
    {
        // この面にはまだ実装が無い（黙って空にせず、`src` で名乗ること）。
        assert_eq!(
            rec["peer_ancestry_src"],
            serde_json::json!("unimplemented:peer_ancestry"),
            "{ctx}: 未実装を名乗っていない: {rec}"
        );
        assert!(
            rec.get("peer_ancestry").is_none() && rec.get("peer_auid").is_none(),
            "{ctx}: 並びや auid を**発明**している: {rec}"
        );
    }

    // 面に依らず「どう知ったか」は必ず在る。
    // これが無いと、空欄が「この面では取れない」なのか「記録し忘れ」なのか読めない。
    assert!(
        rec.get("peer_identity_src").is_some(),
        "{ctx}: 観測の出所が書かれていない（空欄と『取れない』が同じ顔になる）: {rec}"
    );
}

/// 台帳の行を読む。まだ書かれていない間は空で返る。
fn records(path: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("journal line must be valid JSON"))
        .collect()
}

/// **attach の行**が `want` 本書かれるまで台帳を poll する（書き込みは別タスクなので固定 sleep で待たない）。
/// 返すのは**台帳の全行**（spawn の行も含む）。
///
/// 数えるのは attach の行だけ。全行を数えると、`berth` が起こした回は `spawn` の行が先に着くので、
/// 「1 件書かれた」がまだ attach が来ていないことを意味しうる。
async fn wait_for_records(
    path: &std::path::Path,
    want: usize,
    within: Duration,
) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let rs = records(path);
        let attaches = rs.iter().filter(|r| r["type"] == "attach").count();
        if attaches >= want || tokio::time::Instant::now() >= deadline {
            return rs;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// attach が成立したら、**カーネルが答えた相手の uid/pid** が台帳に載る。
/// client は身元を一切名乗っていない（`Attach` は session id しか運ばない）ことが要点。
#[tokio::test]
async fn an_attach_records_the_peer_the_kernel_reports() {
    let f = Fixture::start("peer");

    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("audit-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));

    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "audit-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;
    let start = rs
        .iter()
        .find(|r| r["type"] == "attach" && r["event"] == "start")
        .unwrap_or_else(|| panic!("no attach:start in {rs:?}"));

    // Rheon Protocol v1 の必須 4 フィールド。
    assert_eq!(start["v"], 1);
    assert_eq!(start["source"], "berthd");
    // シェル側の台帳と突き合わせる鍵（子は同じ値を BERTHD_SESSION で見ている）。
    assert_eq!(start["berthd_session"], "audit-tab");
    // **観測された身元**。テストランナー自身が peer なので、自分の uid と一致するはず。
    assert_the_kernels_answer(start, "attach:start の観測");
    // 書いた berthd を同定できること（版だけでは配備物を指せない）。
    assert!(start["build"].as_str().is_some_and(|b| !b.is_empty()));
}

/// **自称は観測の隣に載り、名乗らなければキーごと無い**（protocol 5）。
///
/// 人も AI エージェントも同じ uid で走る環境では、`peer_uid` だけでは「この uid で誰かが
/// 打っている」までしか言えない。そこで client の**自称**を同じ行に置く（検証はしない
/// ため誰でも名乗れるが、一致していれば退屈、食い違っていれば面白いという読み方が
/// 1 行でできるようになる）。
///
/// 注意: 名乗らなかった attach を OS ユーザー名で埋めないことをここで固定する。埋めると
/// 「誰も名乗らなかった」という事実が消え、しかもその値は隣の `peer_uid` の焼き直しでしかない。
#[tokio::test]
async fn a_claim_rides_next_to_the_observation_and_absence_stays_absent() {
    let f = Fixture::start("claim");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // ① 名乗って attach する。
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("claimed-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "claimed-tab".into(),
            cols: 80,
            rows: 24,
            actor: Some("claude-code".into()),
            actor_src: Some("audit_actor".into()),
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;
    let claimed = rs
        .iter()
        .find(|r| r["event"] == "start" && r["berthd_session"] == "claimed-tab")
        .unwrap_or_else(|| panic!("no attach:start in {rs:?}"));
    assert_eq!(claimed["actor"], "claude-code");
    assert_eq!(claimed["actor_src"], "audit_actor");
    // **観測が消えていないこと**: 自称を足したせいでカーネルの答えが押し出されては本末転倒。
    assert_the_kernels_answer(claimed, "観測が押し出されていないこと");

    // ② 名乗らずに別 session へ attach する（同じ経路・同じ接続）。
    send_ctrl(
        &mut c,
        3,
        &Control::Spawn {
            session_id: Some("silent-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    send_ctrl(
        &mut c,
        4,
        &Control::Attach {
            session_id: "silent-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 3, Duration::from_secs(5)).await;
    let silent = rs
        .iter()
        .find(|r| r["event"] == "start" && r["berthd_session"] == "silent-tab")
        .unwrap_or_else(|| panic!("no attach:start for silent-tab in {rs:?}"));
    assert!(
        silent.get("actor").is_none(),
        "an attachment that claimed nothing must not look like it claimed something: {silent}"
    );
    // 観測の方は名乗りの有無に関わらず在る（そちらは client の協力を必要としない）。
    assert_the_kernels_answer(silent, "観測が押し出されていないこと");
}

/// **何が繋いできたか**が台帳に残る（`Hello` の自称）。
///
/// `Control::Hello { client }` は握手の 1 通目で、客が自分を名乗る
/// （CLI は `berth/<版> (<build>)`）。`peer_uid` / `peer_pid` は「誰が」（カーネルの観測）で、
/// こちらは「何が」。
///
/// Windows には uid が無いので、そこでは `client` が身元の手掛かりとして重い。
///
/// 注意: 裏は取れない（客の自称）。∴ `peer_*` とは別の欄に置いてある。
#[tokio::test]
async fn the_ledger_says_what_attached_not_only_who() {
    let f = Fixture::start("client");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // 握手で名乗る（本番の CLI がやっていること）。
    send_ctrl(
        &mut c,
        0,
        &Control::Hello {
            client: "berth/test (deadbeef)".into(),
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Welcome { .. }));

    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("client-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "client-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;
    let rec = rs
        .iter()
        .find(|r| r["event"] == "start" && r["berthd_session"] == "client-tab")
        .unwrap_or_else(|| panic!("no attach:start in {rs:?}"));

    // 綴りは名乗ったまま記録する（こちらで正規化しない。畳むと名乗り方が変わった日に気づけない）。
    assert_eq!(
        rec["client"], "berth/test (deadbeef)",
        "握手で名乗った client が台帳に残っていない: {rec}"
    );
    // 観測が押し出されていないこと（自称を足した副作用で消えては本末転倒）。
    assert_the_kernels_answer(rec, "観測が押し出されていないこと");
}

/// **陰性対照**: 名乗らなかった接続は、名乗ったように見えてはいけない。
///
/// `actor` と同じ作法（キーごと存在しないため、「誰も名乗らなかった」が読める）。
/// ここが空文字や `"unknown"` で埋まると、「名乗らなかった」と「名乗れなかった」が同じ顔になる。
#[tokio::test]
async fn an_attachment_that_never_said_hello_does_not_look_like_it_did() {
    let f = Fixture::start("noclient");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // `Hello` を送らない（本番の客は必ず送るが、契約は強制していない）。
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("quiet-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "quiet-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;
    let rec = rs
        .iter()
        .find(|r| r["event"] == "start" && r["berthd_session"] == "quiet-tab")
        .unwrap_or_else(|| panic!("no attach:start in {rs:?}"));
    assert!(
        rec.get("client").is_none(),
        "名乗らなかった接続に client が付いている: {rec}"
    );
}

/// **実バイナリの `berth attach` が、attach の接続で名乗ること。**
///
/// # これが無いと、上の 2 本が緑でも本番では載らない
///
/// 上の `the_ledger_says_what_attached_not_only_who` は検体が客のふりをして `Hello` を
/// 送っている。すなわち測っているのは berthd の記録側だけで、客が実際に名乗るかは見ていない。
/// `client` は接続に紐づくので、`berth` が別の接続（版の確認など）でだけ名乗っていても、
/// attach の記録には届かない。
///
/// # pty が要る
///
/// `berth attach` は `stdin_is_tty()` で対話パスに入るので、protocol 直叩きでは踏めない。
/// pty の番人は `common::pump_pty_as_terminal`（ConPTY の DSR に答える半分まで要るため、
/// 読むだけの pump を書かないこと）。
///
/// # 綴りを決め打ちしない
///
/// 版も build id も環境で変わる。見るのは `berth/` で始まることだけ（
/// 「名乗った」と「名乗らなかった」を分けるのに、それ以上は要らない）。
#[tokio::test]
async fn the_real_cli_says_what_it_is_on_the_attach_connection() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let f = Fixture::start("cli-hello");
    drop(wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_berth"));
    cmd.arg("attach");
    // この検体は部屋の形（alt screen の中）を見ている。既定はスクロールへ流す形で、
    // しかも tmux の中では自動で外れるため環境で割れる。そこで、形を名指しする。
    cmd.arg("--no-scrollback");
    cmd.arg("cli-hello-tab");
    for (k, v) in std::env::vars() {
        // 自己 attach ガードに触れないよう、自分が入っているセッションの印だけ外す。
        if k == "BERTHD_SESSION" {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("BERTHD_SOCKET", &f.endpoint);
    cmd.env("BERTHD_JOURNAL_PATH", &f.journal);
    cmd.env("TERM", "xterm-256color");
    // 利用者の本物の `~/.berth/config.toml` は読まない（`default-shell` が親の観測に勝ち、検体が手元の設定を測ってしまう）。
    cmd.env(
        "BERTH_CONFIG",
        "/nonexistent/berth-tests-have-no-config.toml",
    );

    let _child = common::PtyChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn berth"));
    drop(pair.slave);

    // master を読み続けないと、子が書いた分でパイプが詰まって attach が進まない。
    // 読むだけでも足りない（ConPTY は DSR に答えるまで 1 バイトも先へ進まない。詳細は番人の doc を参照）。
    let terminal = common::pump_pty_as_terminal(&*pair.master, None);

    let rs = wait_for_records(&f.journal, 1, Duration::from_secs(15)).await;

    // 後始末は assert より前。台帳は既に読み終えているので、ここで閉じても見る物は残る。
    // 閉じるのは番人の仕事（素の `drop(master)` は返らないことが在る。子が ConPTY の前口上で
    // 止まっていると `ClosePseudoConsole` がホストの終了を待ち続ける）。
    // 子を先に落としてから閉じること（答えは子の stdin へ入るバイトでもあるので）。
    drop(_child);
    // 結末を捨てない（この検体は台帳を見に来ているが、閉じられなかったこと自体は
    // 無関係ではない。Windows なら `OpenConsole.exe` が 1 本残る）。`let _ =` で黙らせないこと。
    assert_eq!(
        terminal.close(pair.master),
        common::PtyClose::Returned,
        "pty を閉じられず期限へ落ちた"
    );

    let rec = rs
        .iter()
        .find(|r| r["event"] == "start")
        .unwrap_or_else(|| panic!("no attach:start in {rs:?}"));

    let client = rec
        .get("client")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("実バイナリの attach 記録に `client` が無い: {rec}"));
    assert!(
        client.starts_with("berth/"),
        "CLI が `berth/` で名乗っていない（綴りは客のまま入るはず）: {client:?}"
    );

    // 観測が押し出されていないこと（自称を足した副作用で消えては本末転倒）。
    // `assert_the_kernels_answer` はここでは使えない（あちらは「peer がこの検体のプロセスである」ことを
    // 前提にしている（protocol を直叩きするので真）。この検体だけは peer が起こした `berth` の
    // 子プロセスなので前提が違い、「daemon でも検体でもない、第三のプロセスを見た」まで言える。
    #[cfg(unix)]
    {
        assert_eq!(
            rec["peer_uid"],
            serde_json::json!(unsafe { libc::getuid() }),
            "観測が押し出されている: {rec}"
        );
        let pid = rec["peer_pid"].as_i64().expect("peer_pid が無い");
        assert_ne!(
            pid,
            i64::from(std::process::id()),
            "peer が**検体自身**になっている ＝ 起こした `berth` を見ていない: {rec}"
        );
    }
    assert!(
        rec.get("peer_identity_src").is_some(),
        "観測の出所が書かれていない: {rec}"
    );
}

/// 名乗りは **attach ごと**に取り直される（プロセスに 1 回ではない）。
///
/// berthd は永続でシェルは attach より長生きし、後から別の誰かが同じ session に入れる。
/// `actor` をプロセススコープで持つ限り「今打っているのは誰か」は原理的に表せないので、
/// 名乗りを attach の属性として持つことでそこを表す。
#[tokio::test]
async fn a_second_attachment_carries_its_own_claim() {
    let f = Fixture::start("reclaim");

    let mut first = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut first,
        1,
        &Control::Spawn {
            session_id: Some("shared-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut first).await,
        Control::Spawned { .. }
    ));
    send_ctrl(
        &mut first,
        2,
        &Control::Attach {
            session_id: "shared-tab".into(),
            cols: 80,
            rows: 24,
            actor: Some("claude-code".into()),
            actor_src: Some("agent".into()),
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;

    // 同じ session に**別の接続**が入り直す（人が代わって座った形）。
    let mut second = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut second,
        1,
        &Control::Attach {
            session_id: "shared-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 3, Duration::from_secs(5)).await;
    let starts: Vec<_> = rs
        .iter()
        .filter(|r| r["event"] == "start" && r["berthd_session"] == "shared-tab")
        .collect();
    assert_eq!(starts.len(), 2, "expected two attachments in {rs:?}");
    assert_eq!(starts[0]["actor"], "claude-code");
    assert!(
        starts[1].get("actor").is_none(),
        "the second attachment claimed nothing — it must not inherit the first one's name: {}",
        starts[1]
    );
}

/// attach の**終わり**も記録される。終わり方が複数あるので `how` で言い分ける。
/// ここでは client が黙って消える（GUI が死ぬ / ssh が切れる）形を撃つ。
#[tokio::test]
async fn a_vanishing_client_closes_the_attachment_in_the_ledger() {
    let f = Fixture::start("end");

    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("gone-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "gone-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;

    // client 消滅（GUI 死 / ssh 断）。session は生き続けるが attach は終わる。
    drop(c);

    let rs = wait_for_records(&f.journal, 2, Duration::from_secs(5)).await;
    let end = rs
        .iter()
        .find(|r| r["type"] == "attach" && r["event"] == "end")
        .unwrap_or_else(|| panic!("no attach:end in {rs:?}"));

    assert_eq!(end["berthd_session"], "gone-tab");
    assert!(end["ms"].is_number(), "an attachment has a duration: {end}");
    // 終わり方を言えること。注意: 値までは固定しない。切断は「送信側が全部消えた(closed)」
    // としても「書き込みが失敗した(disconnected)」としても観測されうる競合で、
    // どちらも正しい観測。ここで一方に固定すると、実装ではなくタイミングを固定してしまう。
    let how = end["how"].as_str().unwrap_or_default();
    assert!(
        matches!(how, "closed" | "disconnected"),
        "unexpected how={how:?} in {end}"
    );
}

/// **置き換えられた attach は `detached` で終わる**。
///
/// `closed` と書くと、自分で離れたのと同じ顔になる。2 本目が入った時点で 1 本目には `Detached` が
/// 届く（外部の `berth detach` と同じ口）ので、台帳も同じ語で書く（registry の `how` の説明
/// *another connection detached it* のまま）。
#[tokio::test]
async fn a_replaced_attachment_ends_as_detached_in_the_ledger() {
    let f = Fixture::start("replaced");

    let mut first = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut first,
        1,
        &Control::Spawn {
            session_id: Some("replaced-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(
        read_ctrl(&mut first).await,
        Control::Spawned { .. }
    ));
    send_ctrl(
        &mut first,
        2,
        &Control::Attach {
            session_id: "replaced-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;

    let mut second = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut second,
        1,
        &Control::Attach {
            session_id: "replaced-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    // どちらの接続もまだ閉じない（閉じると `closed` / `disconnected` の競合が混ざる。
    // （上の検体の注意）。ここで見たいのは「置き換え」だけ。
    let rs = wait_for_records(&f.journal, 3, Duration::from_secs(5)).await;
    let end = rs
        .iter()
        .find(|r| r["type"] == "attach" && r["event"] == "end")
        .unwrap_or_else(|| panic!("no attach:end in {rs:?}"));
    assert_eq!(
        end["how"], "detached",
        "🚨 置き換えを、自分で離れたのと同じ顔で書いている: {end}"
    );
    drop((first, second));
}

/// **見るだけの attach は台帳で `read_only: true`**、操作する側は `false`。
/// 見る側が入っても操作する側の `end` は書かれない（置き換えではない）。
#[tokio::test]
async fn a_viewer_attachment_is_marked_read_only_and_does_not_end_the_operator() {
    let f = Fixture::start("viewer");

    let mut op = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut op,
        1,
        &Control::Spawn {
            session_id: Some("viewer-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut op).await, Control::Spawned { .. }));
    send_ctrl(
        &mut op,
        2,
        &Control::Attach {
            session_id: "viewer-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;

    let mut viewer = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut viewer,
        1,
        &Control::Attach {
            session_id: "viewer-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: true,
            seen_total: None,
        },
    )
    .await;
    wait_for_records(&f.journal, 2, Duration::from_secs(5)).await;
    // 置き換えになっていれば end が遅れて 1 行増えるので、少し待ってから数え直す。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rs = records(&f.journal);

    let starts: Vec<_> = rs.iter().filter(|r| r["event"] == "start").collect();
    assert_eq!(starts.len(), 2, "expected two attachments in {rs:?}");
    assert_eq!(
        starts[0]["read_only"],
        serde_json::json!(false),
        "🚨 操作する側の start に `read_only: false` が無い: {}",
        starts[0]
    );
    assert_eq!(
        starts[1]["read_only"],
        serde_json::json!(true),
        "🚨 見る側の start に `read_only: true` が無い: {}",
        starts[1]
    );
    assert!(
        rs.iter().all(|r| r["event"] != "end"),
        "🚨 見る側が入ったら操作する側が終わった（置き換えになっている）: {rs:?}"
    );
    drop((op, viewer));
}

/// **誰が socket と台帳を守っているか**を固定する。
///
/// Linux では socket が `$XDG_RUNTIME_DIR`（`drwx------`）に居るので構造的に守られるが、
/// macOS には XDG_RUNTIME_DIR が無く `~/.berth` に落ちる。そこが `0755` のままだと誰でも中まで
/// 到達でき、残る壁は socket の mode 1 ビットである。しかもそれは起動元から継いだ umask 次第で
/// berthd の決定ではない。`umask 002` で起こされれば `0775` になり、macOS はローカルユーザー全員が
/// `staff` なので「同じ Mac の誰でも attach できる」に化ける。だから berthd が自分で締める。
///
/// 注意: このテストは umask 002 の下で走らせて意味がある（緩い環境を再現して、それでも
/// 締まっていることを見る）。umask を継がない実装なら結果は変わらない。
#[cfg(unix)]
#[tokio::test]
async fn the_socket_and_the_ledger_are_closed_to_everyone_else() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::start("perm");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(std::path::Path::new(&f.endpoint)),
        0o600,
        "connect needs the write bit, so the socket must not grant it to anyone else"
    );

    // 台帳を 1 行書かせて、その mode も見る。
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("perm-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "perm-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;

    assert_eq!(
        mode(&f.journal),
        0o600,
        "who attached and when is not for every account on the box to read"
    );
    // 親ディレクトリが一枚目の壁（bind と chmod の間の窓を塞いでいるのはこちら）。
    assert_eq!(
        mode(&f.dir),
        0o700,
        "an already-loose ~/.berth must be tightened, not left as found"
    );
}

/// 台帳を書けない場所に向けても **daemon は動き続ける**。
/// 監査台帳は補助であって、制御フローを左右してはならない（ログと同じ扱い）。
#[tokio::test]
async fn an_unwritable_ledger_does_not_stop_the_daemon() {
    let dir = std::env::temp_dir().join(format!("berthd-journal-ro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    let endpoint: std::ffi::OsString = {
        let sock = dir.join("berthd.sock");
        std::fs::remove_file(&sock).ok();
        sock.into_os_string()
    };
    #[cfg(not(unix))]
    let endpoint: std::ffi::OsString = berthd::scratch_endpoint("journal-ro");

    // 書けない置き場を、面に依らずに作る。自分でファイルを 1 つ作り、それを親ディレクトリに
    // 見立てる（`create_dir_all` が必ず失敗する）。`/etc/hosts/…` のような unix にしか無い
    // ファイルを当てにしない。
    let not_a_dir = dir.join("not-a-dir");
    std::fs::write(
        &not_a_dir,
        b"i am a file
",
    )
    .unwrap();
    let unwritable = not_a_dir.join("nope").join("journal.jsonl");

    let _daemon = std::process::Command::new(env!("CARGO_BIN_EXE_berthd"))
        .env("BERTHD_SOCKET", &endpoint)
        .env("BERTHD_JOURNAL_PATH", &unwritable)
        .spawn()
        .map(common::ChildGuard::new)
        .expect("spawn berthd");

    let mut c = wait_for_daemon(&endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("ro-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "ro-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Attached { .. }));

    // 台帳が書けなくても、以後の要求に答え続ける。
    send_ctrl(&mut c, 3, &Control::List).await;
    let sessions = loop {
        if let Control::Sessions { sessions } = read_ctrl(&mut c).await {
            break sessions;
        }
    };
    assert!(sessions.iter().any(|s| s.id == "ro-tab"));

    std::fs::remove_dir_all(&dir).ok();
}

/// **berthd 自身の区画が記録に載り、子の番号がログインのものか見分けられる**。
///
/// セッションの中のシェルが継ぐのは、attach した client の区画ではなく、berthd が生まれた区画。
/// ∴ シェルの記録の番号は、既に終了しているログインのものでありうる。誰も嘘をついていない
/// （シェルは本当にそこに居る）のであり、偽になりうるのは「シェルの番号と今 attach しているログインが一致する」
/// という読み手の推論の方。∴ 記録に berthd 自身の区画を載せ、読む側が見分けられるようにする。
///
/// 注意: この検体で作れるのは「同じログイン」の側だけ（テストランナーが berthd を起こすので、
/// 両者は必ず同じ区画に居る）。食い違う側は in-process では作れない（別のログインが要る）。
/// ∴ ここが押さえるのは「見分けるための値が、毎回、名乗り付きで載ること」まで。
#[tokio::test]
async fn the_ledger_says_which_session_the_daemon_itself_is_in() {
    // ① まず機構がこの面で答えることを確かめる（陽性対照）。
    //    これが無いと、下の assert は「載っていない」を「この面では取れない」と
    //    読み違える余地を残す。
    let (own, src) = berthd::own_audit_session();
    assert!(
        !src.is_empty(),
        "src は必ず名乗る（無言で None を返さない）"
    );
    // `!src.is_empty()` では足りない（`unavailable:…` も空ではないので、機構が 1 度も
    // 答えていなくても通る）。しかも下の assert は「daemon と検体が同じ綴り」しか見ないので、
    // 両方が同じように壊れると通る。
    // ∴ この面で機構が実際に答えることを、ここで名指しで要求する。
    // `:unset` は許す（PAM を通っていない箱でも引いてはいる。peer の検体と同じ）。
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(
        !src.starts_with("unavailable:"),
        "この面では自分の区画を引けるはず —— 引けていない ＝ 配線が壊れている: {src}"
    );

    // ② 区画はプロセスの性質であって接続の性質ではないため、何度訊いても同じ。
    //    ここが崩れるなら、peer の値を拾ってしまっている。
    assert_eq!(
        berthd::own_audit_session(),
        (own, src),
        "自分の区画は呼ぶたびに変わらない"
    );

    let f = Fixture::start("ownsess");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("own-tab".into()),
            name: None,
            shell: Some(berthd::default_shell()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Spawned { .. }));
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "own-tab".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;

    let rs = wait_for_records(&f.journal, 1, Duration::from_secs(5)).await;
    let start = rs
        .iter()
        .find(|r| r["type"] == "attach" && r["event"] == "start")
        .unwrap_or_else(|| panic!("no attach:start in {rs:?}"));

    // ③ 名乗りは必ず載る（`peer_identity_src` と同じ）。
    //    値が無いときに欄ごと消すと、「この面では取れない」と「古いバイナリ」が同じ顔になる。
    assert_eq!(
        start["daemon_session_src"], src,
        "daemon_session_src は毎回載り、機構を名乗る: {start:?}"
    );

    // ④ 値が取れる面なら、記録の値は自分に訊いた値そのもの。
    if let Some(own) = own {
        assert_eq!(
            start["daemon_session"], own,
            "記録の daemon_session は own_audit_session() と同じ値: {start:?}"
        );
        // この検体では「同じログイン」の側になる（ランナーが berthd を起こしたので）。
        // 食い違う側を作れないので、押さえているのは読む側が比較できることまで:
        // 一致していれば、子の番号は berthd の区画。
        assert_eq!(
            start["daemon_session"], start["peer_session"],
            "この harness では client と berthd が同じ区画に居るはず（違うなら前提が変わった）: {start:?}"
        );
    } else {
        // 取れない面でも黙らない。「取れなかった」と名乗っていることを固定する。
        assert!(
            start.get("daemon_session").is_none(),
            "値が無いのに欄が在る ＝ 発明している: {start:?}"
        );
        assert!(
            src.starts_with("unavailable:") || src.starts_with("unimplemented:"),
            "値を返さないなら、理由を名乗る綴りであること: {src}"
        );
    }
}

/// **名前を付けて作ったセッションの `spawn:started` は、その名前を持つ。**
///
/// `Spawn` → `Rename` の 2 手で名前を付けると、spawn の瞬間に berthd が知っているのは既定名だけで、
/// 記録は `name: session-1` になる（同時に生きている 2 本が台帳の上で同名になりうる）。
/// ∴ `Spawn.name` で最初から渡す（protocol 11）。
#[tokio::test]
async fn a_session_created_with_a_name_is_spawned_under_that_name() {
    let f = Fixture::start("spawnname");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // (session_id, 渡す名前, 台帳に期待する名前)
    let cases: [(&str, Option<&str>, &str); 5] = [
        ("id-one", Some("n-one"), "n-one"),
        // 2 本目も自分の名前（同時に生きている 2 本が同名にならない）。
        ("id-two", Some("n-two"), "n-two"),
        // 陰性対照: 名前を渡さなければ既定名。
        ("id-three", None, "session-1"),
        // 使えない名前では断らずに既定名で起こす（断るのは後続の `Rename` の仕事）。
        ("id-four", Some("has space"), "session-2"),
        // 重複も同じ。
        ("id-five", Some("n-one"), "session-3"),
    ];
    for (i, (id, name, _)) in cases.iter().enumerate() {
        send_ctrl(
            &mut c,
            u32::try_from(i).unwrap() + 1,
            &Control::Spawn {
                session_id: Some((*id).into()),
                name: name.map(Into::into),
                shell: Some(berthd::default_shell()),
                shell_src: None,
                cwd: None,
                env: None,
                label: None,
                cols: 80,
                rows: 24,
            },
        )
        .await;
        assert!(
            matches!(read_ctrl(&mut c).await, Control::Spawned { .. }),
            "spawn of {id} must succeed even when the name is unusable"
        );
    }

    let rs = wait_for_records(&f.journal, cases.len(), Duration::from_secs(5)).await;
    for (id, _, expected) in cases {
        let spawn = rs
            .iter()
            .find(|r| r["type"] == "spawn" && r["berthd_session"] == id)
            .unwrap_or_else(|| panic!("no spawn record for {id} in {rs:?}"));
        assert_eq!(spawn["name"], expected, "spawn record of {id}: {spawn}");
    }
}

/// 存在しないシェルを名指しされたとき、返す文言は**人が読める 1 行**。
///
/// portable-pty の内部の姿（`CreateProcessW` … NUL 終端 … `Some(…)`）をそのまま client に
/// 出さない。この検体は本物の依存を通す（単体の検体は採ってあった文字列を見ているだけなので、
/// portable-pty が文言を変えたらこちらが先に赤くなる。
#[tokio::test]
async fn an_unknown_shell_is_refused_in_one_plain_line() {
    let f = Fixture::start("badshell");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("bad".into()),
            name: None,
            shell: Some("no_such_shell_berthd_46".into()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    let Control::Error { message, .. } = read_ctrl(&mut c).await else {
        panic!("a shell that does not exist must be refused");
    };
    assert!(
        message.starts_with("cannot start `no_such_shell_berthd_46`: "),
        "{message}"
    );
    // 1 行であること（unix の portable-pty は PATH 全体を含む複数行を返す）。
    assert!(!message.contains('\n'), "more than one line: {message:?}");
    for leaked in [
        "CreateProcessW",
        "Some(",
        "in cwd",
        "execvp",
        "Unable to spawn",
        "viable candidates",
    ] {
        assert!(
            !message.contains(leaked),
            "internal wording leaked ({leaked}): {message}"
        );
    }
    assert!(
        !message.contains(char::from(0)),
        "a NUL reached the person: {message:?}"
    );
    // 理由は残っていること（名前だけ言って理由を落とすのは、直しすぎ）。
    assert!(
        message.len() > "cannot start `no_such_shell_berthd_46`: ".len(),
        "{message}"
    );
}

/// **シェルを起こせなかったことも台帳に残る**（`spawn:failed`）。
///
/// 失敗した spawn が台帳にもデーモンのログにも残らないと、stderr を誰も読まない client
/// （GUI・エージェント）が裏で起こして失敗したとき、後から読む場所が無い。
#[tokio::test]
async fn a_shell_that_could_not_be_started_is_in_the_ledger() {
    let f = Fixture::start("spawnfailed");
    let mut c = wait_for_daemon(&f.endpoint, Duration::from_secs(5)).await;

    // ① 名前つきで、存在しないシェルを名指しする。
    send_ctrl(
        &mut c,
        1,
        &Control::Spawn {
            session_id: Some("want-this".into()),
            name: Some("wanted".into()),
            shell: Some("no_such_shell_berthd_47".into()),
            shell_src: Some("cli_flag".into()),
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    let Control::Error { code, message } = read_ctrl(&mut c).await else {
        panic!("a shell that does not exist must be refused");
    };
    assert_eq!(code, "spawn_failed");

    // ② 陰性対照: 他の断りは書かない。`berth attach` は先に attach を試して外れたときだけ
    //    spawn するので、`no_such_session` は新しいセッションのたびに必ず出る（書いたら雑音で埋まる）。
    send_ctrl(
        &mut c,
        2,
        &Control::Attach {
            session_id: "never-existed".into(),
            cols: 80,
            rows: 24,
            actor: None,
            actor_src: None,
            read_only: false,
            seen_total: None,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Error { .. }));

    // ③ 名前を渡さない失敗。
    send_ctrl(
        &mut c,
        3,
        &Control::Spawn {
            session_id: Some("nameless".into()),
            name: None,
            shell: Some("no_such_shell_berthd_47".into()),
            shell_src: None,
            cwd: None,
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        },
    )
    .await;
    assert!(matches!(read_ctrl(&mut c).await, Control::Error { .. }));

    let rs = wait_for_records(&f.journal, 2, Duration::from_secs(5)).await;
    assert_eq!(
        rs.len(),
        2,
        "exactly the two failed spawns, nothing else: {rs:?}"
    );
    assert!(
        rs.iter()
            .all(|r| r["type"] == "spawn" && r["event"] == "failed"),
        "{rs:?}"
    );

    let named = &rs[0];
    assert_eq!(named["berthd_session"], "want-this");
    assert_eq!(named["name"], "wanted");
    assert_eq!(named["shell"], "no_such_shell_berthd_47");
    assert_eq!(named["shell_src"], "client:cli_flag");
    assert!(
        named["cwd"].as_str().is_some_and(|c| !c.is_empty()),
        "{named}"
    );
    // 人へ返した 1 行と同じ理由（名前の前置きを除いた部分）。
    let reason = named["reason"].as_str().expect("reason");
    assert!(
        !reason.is_empty() && !reason.contains(char::from(10)),
        "{reason:?}"
    );
    assert!(
        message.ends_with(reason),
        "message {message:?} / reason {reason:?}"
    );
    // 誰が頼んだか（成功した spawn と同じ観測が隣に載る）。
    assert_the_kernels_answer(named, "spawn:failed の観測");

    // 無い事実は書かない: 名前を渡さなかった失敗に `name` の欄は無い。出所は `explicit`。
    let nameless = &rs[1];
    assert_eq!(nameless["berthd_session"], "nameless");
    assert!(nameless.get("name").is_none(), "{nameless}");
    assert_eq!(nameless["shell_src"], "client:explicit");
}
