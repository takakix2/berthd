//! 監査台帳: attach / spawn の記録と、接続の相手（`Peer`）。

use super::*;

// ── 監査台帳（attach/detach）──
//
// なぜ berthd が台帳を持つのか: berthd を挟むと、シェルの記録は「誰が打ったか」を見失う。
// セッションのシェルは berthd 自身の env を継ぐだけなので、繋いできた相手の身元は境界で消える。
// 消えた先が「不明」ではなく「対話している人」に見える分、黙って間違う。
//
// env を運んでも解けない: berthd は永続でシェルが attach より長生きし、後から別の誰かが
// 同じセッションに入れる。「今打っているのは誰か」は spawn 時の env では原理的に表せない。
// 身元はプロセスでなく **attach に属する**。だから attach 自体を記録する。
//
// 自称でなく観測: unix socket の peer credential（uid/pid）はカーネルが答えるので、
// client が名乗る必要も、名乗りを信じる必要もない。
//
// 注意: シェルの側の記録には書かない。berthd はシェル非依存（PTY と unix socket だけ）で、
// fish / zsh のセッションも持つ。台帳は `~/.berth/journal.jsonl` に持ち、`berthd_session` を
// join 鍵にしてシェル側の記録と突き合わせる（子は `BERTHD_SESSION` で同じ値を見ている）。

/// 台帳のパス。`BERTHD_JOURNAL_PATH`（フルパス）が最優先、無ければ `~/.berth/journal.jsonl`。
pub fn journal_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("BERTHD_JOURNAL_PATH") {
        return std::path::PathBuf::from(p);
    }
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    home.join(".berth").join("journal.jsonl")
}

/// Rheon Protocol v1 のレコードを 1 行 append する。書けなくても呼び出し側は続行する
/// （戻り値で失敗を返すのは、黙って捨てずに 1 回だけ言うため。台帳の失敗は daemon を止めない）。
///
/// 注意: 追記は 1 回の `write_all` に畳む（NDJSON の行が割れると読み手が壊れる）。
pub fn journal_append(record: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = journal_path();
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir);
    }
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    // 台帳の mode を umask に決めさせない。既定（umask 022）だと `0644` となり、
    // 「誰がいつどのセッションに入ったか」を同じホストの全ユーザーが読める。
    // 注意: `mode` が効くのは新規作成時だけ（既存ファイルの mode は変えない）。
    // 親を `0700` にしてあるのは、古い台帳が緩いまま残る場合の二枚目。
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(line.as_bytes())
}

/// 接続の相手（**カーネルが答えた観測値**であり、client の自称ではない）。
///
/// uid と pid は必ず一緒に扱う（片方だけでは「誰が」にも「どのプロセスが」にも答えられない）ので
/// 1 つの値にする。OS によっては pid を答えないので `None` を許す
/// （知らないと言える欄の方が、当てずっぽうより価値がある）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Peer {
    pub uid: Option<u32>,
    pub pid: Option<i64>,
    /// 相手のログイン／サービスの区画。注意: **面で意味が違う欄**なので、読むときは
    /// 必ず [`Peer::src`] を一緒に見ること（どちらの番号かはそこにしか書いていない）:
    ///
    /// | 面 | 何の番号か | `src` |
    /// |---|---|---|
    /// | Windows | Terminal Services session id（`0` = サービス: ssh 経由 / `1` 以上 = 対話ログイン）| `named_pipe…` |
    /// | Linux | 監査セッション id（`/proc/<pid>/sessionid`: pam が張り子孫に継承）| `so_peercred+proc_sessionid…` |
    /// | macOS | 監査セッション id（BSM の `asid`: `LOCAL_PEERTOKEN` で接続に訊く）| `so_peercred+local_peertoken…` |
    /// | その他 unix | 未実装（BSD / iOS / Android・`peer_from_cred` の doc）| `…unimplemented:peer_audit_session` |
    ///
    /// 1 欄に畳んであるのは、join する相手（hsh の `os_sessionid`）が同じ畳み方をしているから。
    /// あちらも Linux と macOS の監査セッション id を 1 欄に載せ、機構は別の欄で名乗る。
    /// macOS では、相手が自分で訊いた `asid` と、こちらが `LOCAL_PEERTOKEN` で見た `asid` は同じ番号になる。
    ///
    /// これは到達性の手掛かりでもある。Windows の session 0 の berthd はデスクトップからは
    /// 開けないので、台帳にこの番号が載っていれば「なぜ繋がらなかったか」が後から読める。
    pub session: Option<u32>,
    /// 相手の auid（最初にログインした人・sudo しても変わらない）。
    ///
    /// | 面 | どこから | 未設定の番兵 |
    /// |---|---|---|
    /// | Linux | `/proc/<pid>/loginuid` | `4294967295` |
    /// | macOS | `LOCAL_PEERTOKEN` の監査トークン（`audit_token_to_auid()`）| `AU_DEFAUDITID`（同じく `u32::MAX`）|
    /// | Windows / その他 | 無い（uid と同じく存在しない）| — |
    ///
    /// 出所の欄は増やさない。機構は [`Peer::src`] が既に名乗っている（`proc_sessionid` / `local_peertoken`）。
    pub auid: Option<u32>,
    /// どうやって知ったか / なぜ知れなかったか。
    ///
    /// これだけは「無い事実は書かない」の例外。「`peer_uid` が無い」が *この面では取れない* なのか
    /// *記録し忘れ・古いバイナリ* なのかを分けるために在る。空欄のままだと、その 2 つが同じ顔になる。
    pub src: Option<&'static str>,
}

/// attach の始まりと終わり。**終わり方と滞在時間は「終わり」にしか無い**ので、
/// `Option` を 2 つ並べるのではなく型で持つ（`start` なのに `how` が在る、が作れない）。
#[derive(Clone, Copy, Debug)]
pub enum AttachEvent<'a> {
    Start,
    End {
        /// 終わり方（`exited` / `detached` / `disconnected` / `closed`）。
        how: &'a str,
        /// attach が続いた時間。
        ms: u64,
    },
}

/// 台帳の 1 行に載せる**接続の観測**（＋裏の取れない自称 1 つ）。
///
/// attach と spawn で 2 通りに書かない。同じ欄が別々に育つと、片方だけ新しい観測を持つ行が
/// 生まれ、読む側は「取れなかった」と「書いていない」を区別できなくなる。
pub(crate) fn put_observation(
    m: &mut serde_json::Map<String, serde_json::Value>,
    peer: Peer,
    ancestry: &Ancestry,
    client: Option<&str>,
) {
    // 自称でなくカーネルの観測。
    if let Some(uid) = peer.uid {
        m.insert("peer_uid".into(), serde_json::json!(uid));
    }
    if let Some(pid) = peer.pid {
        m.insert("peer_pid".into(), serde_json::json!(pid));
    }
    // 相手の区画。注意: 面で意味が違う（Windows = TS session / unix = 監査セッション id）。
    // どちらかは `peer_identity_src` にしか書いていない（`Peer::session` の表）。
    if let Some(session) = peer.session {
        m.insert("peer_session".into(), serde_json::json!(session));
    }
    // 最初にログインした人（sudo しても変わらない）。未設定ならキーごと無い。
    if let Some(auid) = peer.auid {
        m.insert("peer_auid".into(), serde_json::json!(auid));
    }
    // どの道具が繋いできたか（親プロセスの並び・名前と pid だけ）。
    // 並びが空でも `src` は必ず書く（`peer_identity_src` と同じ規律）。
    if !ancestry.chain.is_empty() {
        let chain: Vec<serde_json::Value> = ancestry
            .chain
            .iter()
            .map(|a| serde_json::json!({ "pid": a.pid, "name": a.name }))
            .collect();
        m.insert("peer_ancestry".into(), serde_json::Value::Array(chain));
    }
    m.insert("peer_ancestry_src".into(), serde_json::json!(ancestry.src));
    // ここだけは「無い事実は書かない」の例外であり、上の欄が空のとき、それが *この面では取れない* なのか
    // *記録し忘れ・古いバイナリ* なのかを分けるために書く。空欄のままだと 2 つが同じ顔になる。
    if let Some(src) = peer.src {
        m.insert("peer_identity_src".into(), serde_json::json!(src));
    }
    // berthd 自身の区画（[`own_audit_session`] に理由）。読む側が「子の監査セッション id は
    // ログインの物か、berthd の生まれの物か」を見分けるための 1 値。相手の番号と一致していたら、
    // その番号での join は当てはまらない。
    // 引くのは 1 回（値はプロセスの寿命の間ずっと同じ）だが、記録には毎回載せる。
    // 台帳は 1 行で読めることに値打ちが在るので、別の行を探しに行かせない。
    {
        static OWN: std::sync::OnceLock<(Option<u32>, &'static str)> = std::sync::OnceLock::new();
        let (session, src) = *OWN.get_or_init(own_audit_session);
        if let Some(session) = session {
            m.insert("daemon_session".into(), serde_json::json!(session));
        }
        // `peer_identity_src` と同じ規律。空欄のままだと *この面では取れない* と
        // *古いバイナリで書いていない* が同じ顔になる。
        m.insert("daemon_session_src".into(), serde_json::json!(src));
    }
    // 観測の隣に自称（`Hello { client }`）。名乗らなかった接続はキーごと無い（「誰も名乗らなかった」が読める）。
    // 綴りは客が名乗ったまま入れる（例: `berth/<版> (<build>)`）。こちらで正規化しない。
    // 名乗りを畳むと、名乗り方が変わった日に気づけなくなる。
    // 注意: 古い版が書いた行ではこの欄は `claimed_client`（台帳は追記のみなので残っている）。
    if let Some(c) = client.filter(|s| !s.is_empty()) {
        m.insert("client".into(), serde_json::json!(c));
    }
}

/// 台帳の 1 行に共通する封筒。
///
/// 2 箇所に書かない。綴りが 1 つでもずれると、シェルの側の記録との join が書けなくなる。
/// レコードの種類が増える日は、写しが生まれる日でもある。
pub(crate) fn rheon_envelope(
    kind: &str,
    event: &str,
    berthd_session: &str,
    name: &str,
    host: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("v".into(), serde_json::json!(RHEON_V));
    m.insert("source".into(), serde_json::json!(SOURCE));
    m.insert("type".into(), serde_json::json!(kind));
    m.insert("event".into(), serde_json::json!(event));
    // 綴りは `ts`（Rheon Protocol v1）。シェルの側の記録と時刻で突き合わせるとき、ここが違うと join が書けない。
    m.insert(
        "ts".into(),
        serde_json::json!(chrono::Utc::now().to_rfc3339()),
    );
    // シェル側の台帳と突き合わせる鍵。子は同じ値を `BERTHD_SESSION` で見ている。
    m.insert("berthd_session".into(), serde_json::json!(berthd_session));
    m.insert("name".into(), serde_json::json!(name));
    m.insert("host".into(), serde_json::json!(host));
    // どの berthd が書いたか（版だけでは配備物を同定できない・`VERSION_STRING` 参照）。
    m.insert("ver".into(), serde_json::json!(env!("CARGO_PKG_VERSION")));
    m.insert("build".into(), serde_json::json!(env!("BERTHD_BUILD_ID")));
    m
}

/// セッションを起こした 1 レコード（純粋・I/O なし）。
///
/// # なぜ一覧では足りないか
///
/// `berth ls` は今のシェルを見せるが、セッションが消えたら何も残らない。一覧は「今」の計器、
/// 台帳は「あった事」の計器。この行が無いと、「なぜこのシェルなのか」は走っているデーモンの
/// 環境を読むしかなく、デーモンが入れ替わるとその答えは失われる。
///
/// # `shell_src`
///
/// 値だけでは「誰が決めたか」が再現できない（`$SHELL` か `BERTHD_SHELL` か、
/// あるいは `berth` を打った側で走っていたシェルか）。綴りは `<誰の側>:<何を見て>`:
///
/// | 例 | 意味 |
/// |---|---|
/// | `client:parent_shell` | `berth` を打った側で**走っていた**シェル |
/// | `client:berthd_shell` | 打った側の `BERTHD_SHELL` |
/// | `client:env_shell` | 打った側の `$SHELL`（親がシェルでなかったとき）|
/// | `client:explicit` | 名指ししたが出所を言わなかった client（protocol 9 より前）|
/// | `daemon:*` | client が何も言わなかったので berthd が決めた（[`default_shell_with_src`]）|
///
/// 注意: `name` は spawn の瞬間の名前（`berth attach <name>` はこの後で改名する）。
/// 指し続けられるのは `berthd_session`（id）の方で、そちらが join 鍵。
// 引数が 9 つで clippy の既定（7）を超えるが、束ねて減らさない。理由は [`attach_record`] と同じで、
// `peer`（接続の時点の観測）と `ancestry`（この瞬間の観測）は取る時点が違う。
// 1 つの型に畳むと、同じ値を使い回してよいように見える。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn spawn_record(
    berthd_session: &str,
    name: &str,
    shell: &str,
    shell_src: &str,
    cwd: &str,
    peer: Peer,
    ancestry: &Ancestry,
    client: Option<&str>,
    host: &str,
) -> serde_json::Value {
    // 綴りは `spawn:started`。近すぎる名前を 2 つ避けている:
    //
    // ① `event` を `start` にしない。台帳には既に `attach` の `start` が居て、同じ
    //    `berthd_session` を持つ。`event` だけで引く読み手は、attach の行のつもりでこの行を掴む。
    // ② `type` を `session` にしない。制御フレームが既に `session:spawn` を名乗っている
    //    （`encode_control` が `SESSION_TYPE` を押すので、`Control::Spawn` はその綴りで飛ぶ）。
    //    同じ `source` の 1 文字違いが「線の上の要求」と「台帳の事実」に分かれると、
    //    読む側に区別する手掛かりが無い（どちらも Rheon Protocol v1 の 1 行）。
    //
    // ∴ 台帳の流儀（1 行の種類につき 1 つの `type`・`attach` がそう）に従う。
    let mut m = rheon_envelope("spawn", "started", berthd_session, name, host);
    m.insert("shell".into(), serde_json::json!(shell));
    m.insert("shell_src".into(), serde_json::json!(shell_src));
    // spawn した時の cwd（一覧の `cwd` と違って引き直さない）。セッションは動くが、
    // 「どこで起こされたか」は動かない事実。
    m.insert("cwd".into(), serde_json::json!(cwd));
    put_observation(&mut m, peer, ancestry, client);
    serde_json::Value::Object(m)
}

/// シェルを起こせなかったときの 1 レコード（`spawn:failed`・純粋・I/O なし）。
///
/// 要るのは監査より診断: 人が `berth` を打ったなら端末のエラーで足りるが、stderr を誰も読まない
/// client（GUI やエージェント）が裏で起こして失敗したとき、後から「何を起こそうとして、
/// なぜ駄目だったか」を読む場所がここになる。`berth attach -s` で起こす物を client が名指しできるので、
/// その出所（`shell_src`）は失敗したときにも残す。
///
/// 他の断り（`no_such_session` / `bad_name`）は書かない。`berth attach` は先に attach を
/// 試して外れたときだけ spawn するので、`no_such_session` は新しいセッションのたびに必ず出る。
/// 「プログラムを起こせと言われて、起きなかった」のは spawn だけ。
///
/// - `berthd_session`: 頼まれた id（セッションは生まれていないので、どの記録とも join しない）。
/// - `name`: client が名前を渡したときだけ（無い事実は書かない）。
/// - `reason`: 人へ返した物と同じ、OS の理由だけの 1 行。
#[allow(clippy::too_many_arguments)] // `spawn_record` と同じ理由で束ねない
#[must_use]
pub fn spawn_failed_record(
    berthd_session: &str,
    name: Option<&str>,
    shell: &str,
    shell_src: &str,
    cwd: &str,
    reason: &str,
    peer: Peer,
    ancestry: &Ancestry,
    client: Option<&str>,
    host: &str,
) -> serde_json::Value {
    let mut m = rheon_envelope(
        "spawn",
        "failed",
        berthd_session,
        name.unwrap_or_default(),
        host,
    );
    if name.is_none() {
        m.remove("name");
    }
    m.insert("shell".into(), serde_json::json!(shell));
    m.insert("shell_src".into(), serde_json::json!(shell_src));
    m.insert("cwd".into(), serde_json::json!(cwd));
    m.insert("reason".into(), serde_json::json!(reason));
    put_observation(&mut m, peer, ancestry, client);
    serde_json::Value::Object(m)
}

/// attach / detach の 1 レコードを組み立てる（純粋・I/O なし）。
/// 注意: 無い事実は書かない: 取れなかった peer の欄も、されなかった自称もキーごと省く
/// （`"actor": null` は「名乗った上で null」と読めてしまう）。
///
/// 1 行の中に「言ったこと」と「見えたこと」が同居するので、どちらがどちらかは行を見ただけで
/// 分からないといけない。印を持つのは**観測の側**（`peer_` 接頭辞）で、自称は素の名前
/// （`actor` / `actor_src` / `client`）。
// 引数が 8 つで clippy の既定（7）を超えるが、束ねて減らさない。`peer`（接続の時点の観測）と
// `ancestry`（attach の瞬間の観測）は取る時点が違うので同じ値にしない（`Peer` は `Copy` で接続に 1 つ）。
#[allow(clippy::too_many_arguments)]
pub fn attach_record(
    event: AttachEvent<'_>,
    berthd_session: &str,
    name: &str,
    peer: Peer,
    // attach の瞬間の親プロセスの並び（[`peer_ancestry`]）。
    ancestry: &Ancestry,
    // 何が繋いできたか（`Hello` の自称）。`peer` と違って裏が取れない（∴ `peer_` の印を付けない）。
    // Windows では `peer_cred` が取れず uid も pid も空になるので、そこでは身元の唯一の手掛かりになる。
    client: Option<&str>,
    claim: &Claim,
    host: &str,
    // 見るだけの attach か。観測した形であって自称ではないので毎回書く。
    read_only: bool,
) -> serde_json::Value {
    let mut m = rheon_envelope(
        "attach",
        match event {
            AttachEvent::Start => "start",
            AttachEvent::End { .. } => "end",
        },
        berthd_session,
        name,
        host,
    );
    // `false` も書く。この欄が無い古い記録（全部操作する側）と、操作する側だと分かっている記録を
    // 見分けられるように。
    m.insert("read_only".into(), serde_json::json!(read_only));
    // 観測（カーネルが答えた事実）と「何が繋いできたか」。spawn の行と同じ形
    // （[`spawn_record`] も同じ関数を通るため、2 種類のレコードで欄がずれない）。
    put_observation(&mut m, peer, ancestry, client);
    // 自称の綴りは素の `actor` / `actor_src`（ワイヤの `Control::Attach` と同じ・hsh の記録とも同じ）。
    // 「自称か観測か」は観測の側の `peer_` が示す。
    // 注意: 古い版が書いた行ではこの欄は `claimed_actor`（台帳は追記のみ）。読む側は両方を引くこと。
    if let Some(actor) = claim.actor.as_deref().filter(|s| !s.is_empty()) {
        m.insert("actor".into(), serde_json::json!(actor));
        // src は名前があるときだけ意味を持つ（名前無しの出所は語義矛盾）。
        if let Some(src) = claim.src.as_deref().filter(|s| !s.is_empty()) {
            m.insert("actor_src".into(), serde_json::json!(src));
        }
    }
    if let AttachEvent::End { how, ms } = event {
        m.insert("how".into(), serde_json::json!(how));
        m.insert("ms".into(), serde_json::json!(ms));
    }
    serde_json::Value::Object(m)
}

/// `SendKeys` の記録。入力内容そのものは機密を含みうるため記録せず、送信バイト数だけ残す。
pub fn send_keys_record(
    berthd_session: &str,
    name: &str,
    peer: Peer,
    ancestry: &Ancestry,
    client: Option<&str>,
    host: &str,
    bytes: usize,
) -> serde_json::Value {
    let mut m = rheon_envelope("session", "send_keys", berthd_session, name, host);
    m.insert("bytes".into(), serde_json::json!(bytes));
    put_observation(&mut m, peer, ancestry, client);
    serde_json::Value::Object(m)
}
