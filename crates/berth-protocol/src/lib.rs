//! berthd のワイヤ契約（client と berthd(daemon) が共有する層）。
//!
//! - フレーム: `| len:u32 LE | kind:u8 | serial:u32 LE | payload[len] |`
//! - `kind=0`(control): payload は Rheon Protocol v1 のレコード（1 行 JSON・`{v,source,type,event,ts,..}`）
//! - `kind=1`(data): payload は生バイト（PTY 入出力・無加工・UTF-8 保証なし）
//! - `serial`: req/resp 対応（push は 0）。レコードは req/resp の概念を持たないのでフレーム側が補う。

// README の例をコンパイルさせる（doctest）。使い方の記述が古くなったら機械が止める。
#![doc = include_str!("../README.md")]
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const KIND_CONTROL: u8 = 0;
pub const KIND_DATA: u8 = 1;

/// 制御プロトコルの版。客が振る舞いを変えなければならない変更を入れたら上げる。
///
/// berthd は client より長生きする（それが永続化の本体）ので、client を更新しても古い berthd が
/// 走り続ける。berthd は未知の制御メッセージを黙って捨てる（前方互換）ため、放っておくと
/// 新しい verb が沈黙のうちに効かない。client は `Welcome.protocol` を見て skew を検知する。
///
/// - 1: `KillServer` / `Spawn.env` / `Spawn.label` / `Welcome.protocol` を含む版
/// - 2: `Rename` / `SessionInfo.name`（人間用の名前）を含む版
/// - 3: `Ping` / `Pong`（heartbeat・継続 liveness）＋ `Welcome.host` を含む版。
///   client は heartbeat をこの版以上でだけ起動する（古い berthd は Ping を黙って捨て Pong を
///   返さないため、「無応答」を「死」と誤判定するため）。
/// - 4: `Detached`（server→client）を含む版。外部からの `Detach`（別接続の `berth detach <name>`）で
///   いま attach しているクライアントを外す通知。`berth attach` で入り `berth detach` で抜ける、の
///   「抜ける」を実現する。
/// - 5: `Attach.actor`: attach してきた者の自称する名前。台帳で peer credential（観測）の隣に
///   並べるためだけに在る。版ゲートは要らない（往復しない欄で、古い berthd は黙って捨て、古い client は
///   送ってこないので `None`、すなわち「名乗らなかった」になる。どちらも誤検知に化けない）。上げるのは
///   「この berthd は自称を記録できるか」を `berth info` の skew 表示で言えるようにするため。
/// - 6: `Dropped`（server→client）: 溢れて落とした分の通知。
/// - 7: `Attach.read_only`（見るだけの attach・tmux の `attach -r`）＋ `Detached.reason`。
///   重要: 版ゲートが要る。古い berthd は知らない欄を黙って捨てる（`deny_unknown_fields` 無し）ので、
///   `read_only: true` を送っても操作する側として attach され、打鍵も通る。
///   ∴ client は `Welcome.protocol` が 7 未満なら、見るだけの attach を送らずに断ること
///   （`berth attach -r` がそうする）。`reason` の方は古い client が読み飛ばすだけで害が無い。
/// - 8: `Attached.resized`: その attach が pty の大きさを実際に変えたか。
///   判定は berthd にしか出来ない: 大きさは tty 自身の属性で、セッションの中の `stty` でも
///   動くので、client が「自分の端末の大きさ」と `berth ls` を見比べても当たらない。
///   版ゲートは要らない。古い berthd は欄を送らず `default` の `false` に落ち、
///   client は今までと同じバナーの出し方をする（`read_only` と違い、黙って別物になる向きが無い）。
/// - 9: `Spawn.shell_src`: その `shell` を誰が決めたか。
///   シェルの解決は client 側でやる（`berth` は自分の親プロセスを観測して決める）ので、
///   出所はワイヤを越えないと berthd に届かない。
///   版ゲートは要らない。古い client は送らず、berthd は「client が名指しした」（`explicit`）として
///   記録する。それは実際に起きたことなので、既定が嘘にならない。
/// - 10: `SessionInfo.cwd_src`: その `cwd` をどこから知ったか。
///   版ゲートは要らない。古い berthd は欄を送らず、空（すなわち「名乗っていない ∴ 信じるな」）に落ちる。
/// - 11: `Spawn.name`: 作る時点で名前を渡す。
///   これが無いと「`Spawn` → 返事 → `Rename`」の 2 手になり、berthd は spawn の瞬間には既定名
///   （`session-N`）しか知らないので、spawn の記録の `name` が既定名になる。
///   版ゲートは要らない。古い berthd は欄を黙って捨てて既定名で起こすが、client は今までどおり
///   `Rename` も送るので最終的な名前は同じ。変わるのは spawn の記録の `name` だけ。
///   注意: 使えない名前（空・空白・`;`・重複）は berthd が既定名に落として起こす。断るのは
///   後続の `Rename` の仕事で、そこで理由が返る。
/// - 12: `Attached.replay_head`: 録画の頭より前に捨てた出力が残した状態。
///   録画（replay）は有界で古いバイトから捨てるので、頭に来る物は切れ目の位置次第（命令の途中・文字の途中・
///   前提（alt screen に入った・色を付けた・題名を変えた）がすべて消えた後、の 3 つ）。berthd は捨てるバイトを
///   小さな状態機械に通し（格子は持たない）、捨てた分が残した状態だけをここで渡す。client は再生の頭で
///   その状態になる命令を作り直してからエミュレータに通す。
///   版ゲートは要らない。古い berthd は欄を送らず `None`（「捨てていない」と同じ）で今までどおり描く。
///   古い client は欄を読み飛ばすだけ。一度も捨てていなければ berthd も `None`（欄ごと出ない）。
/// - 13: `Attach.seen_total` と `Attached.replay_total`: 同じペインで入り直したとき続きだけ流す。
///   `replay_total`: セッション開始から produce した累計バイト（単調増加）。
///   client は前回このペインで見せた位置（`seen_total`）を送り、berthd はそれが録画の中なら
///   `[seen_total, replay_total)` だけ返す（頭の状態は 12 の `ReplayHead` で作り直す）。
///   版ゲートは要らない。古い berthd は `seen_total` を捨てて録画をそのまま全量返し、`replay_total` を
///   送らない（`None`）。client は `replay_total` が `None` なら続きだけ流すのをやめる（今までどおり）。
/// - 14: `SendKeys` / `SendKeysAck` / `Peek` / `PeekResult`: attach せずに外からキー入力を差し込み、
///   セッションの画面状態と無出力時間（`idle_ms`）を覗く口。
///   版ゲートが要る。古い berthd は未知の制御メッセージを黙って捨てるため、client は応答を待って
///   ハングする。∴ client は `Welcome.protocol >= 14` を確認してから送ること。
pub const PROTOCOL_VERSION: u32 = 14;

/// Rheon Protocol v1 のエンベロープの既定値。
pub const RHEON_V: u32 = 1;
pub const SESSION_TYPE: &str = "session";

/// 巨大フレームの防御（録画の上限 512KB を大きく上回る安全弁）。
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// 1 フレーム。
#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: u8,
    pub serial: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn control(serial: u32, payload: Vec<u8>) -> Self {
        Self {
            kind: KIND_CONTROL,
            serial,
            payload,
        }
    }
    pub fn data(serial: u32, payload: Vec<u8>) -> Self {
        Self {
            kind: KIND_DATA,
            serial,
            payload,
        }
    }
}

/// フレームを 1 つ読む（EOF/切断は Err で返る）。
pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Frame> {
    let len = r.read_u32_le().await?;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "berthd: frame too large",
        ));
    }
    let kind = r.read_u8().await?;
    let serial = r.read_u32_le().await?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    Ok(Frame {
        kind,
        serial,
        payload,
    })
}

/// フレームを 1 つ書く（flush まで）。
pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, f: &Frame) -> std::io::Result<()> {
    w.write_u32_le(f.payload.len() as u32).await?;
    w.write_u8(f.kind).await?;
    w.write_u32_le(f.serial).await?;
    w.write_all(&f.payload).await?;
    w.flush().await?;
    Ok(())
}

/// 制御メッセージ（レコードの `event` ＋ payload）。
/// エンコード時に `v` / `source` / `type` / `ts` を付けて Rheon Protocol v1 のエンベロープに包む。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Control {
    // ── client → server ──
    Hello {
        client: String,
    },
    List,
    Spawn {
        /// client 指定の session id（省略時は berthd が採番）。client を再起動した後の reattach のため、
        /// client が安定した id を渡せる（attach-or-spawn を id で回せる）。
        #[serde(default)]
        session_id: Option<String>,
        /// 作る時点の表示名（protocol 11）。省けば berthd が `session-N` を振る。
        /// `session_id` とは別物（あちらは reattach のための安定した鍵、こちらは人が読む名前）。
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        shell: Option<String>,
        /// `shell` を誰が決めたか（protocol 9）。`shell` が `None` のときは意味を持たない。
        ///
        /// 綴りは client 側の解決の段（`berthd::ShellSrc`）: `berthd_shell` / `parent_shell` / `env_shell` など。
        /// 省いてよい。berthd は「client が名指しした」（`explicit`）として台帳に書く。
        /// 注意: ここに `explicit` を送らないこと（自分で決めておいて「誰かが決めた」と言う形になる）。
        #[serde(default)]
        shell_src: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// 子プロセスに足す環境変数。
        #[serde(default)]
        env: Option<Vec<(String, String)>>,
        /// 一覧に出す表示名（省略時は shell のパス）。実行ファイルの名前がシェルの名前と違うとき
        /// （自分自身を再 exec してシェルとして動く client など）に、`berth ls` へ読める名前を出すため。
        #[serde(default)]
        label: Option<String>,
        cols: u16,
        rows: u16,
    },
    Attach {
        session_id: String,
        cols: u16,
        rows: u16,
        /// この attach をしている者が自称する名前（protocol 5+・`claude-code` / `deploy-bot` …）。
        ///
        /// これは名前だけで、種別（agent / human / …）は載せない。「その名前が何の仲間か」は
        /// 名前についての事実であって、この attach で起きたことではない（同じ名前を持つ全レコードで
        /// 同じ値になるものは、レコードの持ち物ではない）。
        /// 分類が要る読み手は名前で引く。
        ///
        /// 注意: 名乗らなかったときは `None`。OS ユーザー名で埋めない。それは client の主張ではなく
        /// カーネルが既に `peer_uid` で答えていることで、複製すると「名乗った」ように見える。
        /// 空欄は「誰も名乗らなかった」という事実として意味を持つ。
        #[serde(default)]
        actor: Option<String>,
        /// その名前が client のどこから来たか（`audit_actor` = `AUDIT_ACTOR` を明示 /
        /// `agent` = 環境の自己申告痕跡）。名前からは引けない情報なので記録する
        /// （「打った人が名乗った」のと「起動器が置いた痕跡を拾った」のでは、同じ文字列でも重みが違う）。
        #[serde(default)]
        actor_src: Option<String>,
        /// 見るだけの attach（protocol 7+・tmux の `attach -r`）。
        ///
        /// 出力は受け取るが、打鍵と `Resize` は berthd が捨てる。attach の時に大きさも変えない。
        /// 操作する側（`false`）を置き換えず、何本でも並べられる。
        /// 注意: 詰まって出力に追いつけない見る側は外される（`Detached { reason: "too_slow" }`）。
        /// 操作する側は背圧で待つが、見る側 1 本のために操作する側を止めないため。
        /// 重要: 古い berthd はこの欄を読まずに操作する側として扱うため、送る前に `Welcome.protocol >= 7` を確かめる。
        #[serde(default)]
        read_only: bool,
        /// 前回このペインで見せた録画の位置（protocol 13+）。
        ///
        /// client がペイン（端末が付ける ID）ごとに覚えている累計バイト数。これが録画の中にあれば
        /// berthd は `[seen_total, replay_total)` だけ返し、同じペインで入り直したときの積み増しを防ぐ。
        /// 録画から落ちている・ペインが未知なら `None`（録画をそのまま全量返す）。
        /// 古い berthd はこの欄を読まずに全量を返す。送っても黙って別物にならない（版ゲート不要）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seen_total: Option<u64>,
    },
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    Detach {
        session_id: String,
    },
    Close {
        session_id: String,
    },
    /// デーモンごと終了する（全 session の子を kill → socket を消して exit）。`berth kill-server`。
    /// berthd は自分からは落ちない（idle timeout 無し）ので、畳む手段はこれ。
    KillServer,
    /// session に人間用の名前を付け直す（`berth rename`）。名前は berthd が持つため client が死んでも残る
    /// （名前は「後で戻ってくるときの手がかり」なので、client と運命を共にしては意味がない）。
    /// 空白を含む名前・重複名は拒否（名前で attach できることが名前の存在理由）。
    Rename {
        session_id: String,
        name: String,
    },
    /// heartbeat（継続 liveness・protocol 3+）。client が attach 中の接続に周期的に流し、`Pong` が
    /// 返るかで berthd の生死を測る。`ssh -L` のトンネル越しだと transport の切断が隠れる（ローカルの
    /// socket が残る）ので、生死は「応答フレームが返るか」でしか測れない。
    /// 出力ストリーム（data フレーム）に割り込んで送られるので、berthd は attach 有無に依らず即返す。
    Ping,
    /// attach せずにセッションの PTY へバイト列を差し込む（protocol 14+）。
    ///
    /// 既に操作側クライアントが attach していても追い出さずに書き込む。
    /// `force: false` のとき、前回の送信からセッション出力（`produced`）が進んでいない連続送信が
    /// 上限に達すると `Error { code: "send_loop_limit", .. }` で拒否する。
    SendKeys {
        session_id: String,
        data: Vec<u8>,
        #[serde(default)]
        force: bool,
    },
    /// セッションの現在の状態（`idle_ms` / `attached`）と直近の録画を覗く（protocol 14+）。
    ///
    /// `PeekResult` の直後に `KIND_DATA` フレーム 1 本で生バイト列（`data_len` バイト）を返す。
    Peek {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<usize>,
    },

    // ── server → client ──
    Welcome {
        server: String,
        /// [`PROTOCOL_VERSION`]。古い berthd は送ってこないので `default = 0`（skew の検知に使う）。
        #[serde(default)]
        protocol: u32,
        /// berthd が走っているマシンの hostname（`berth info` が「今どのマシンの berthd か」を名乗るため）。
        /// 古い berthd は送ってこないので `default`（空）となり、client は "unknown" 扱い。
        /// remote（`ssh -L` のトンネル越し）だと socket パスは不透明なので、host は berthd の自己申告で得る。
        #[serde(default)]
        host: String,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Spawned {
        session_id: String,
        #[serde(default)]
        pid: Option<u32>,
    },
    Attached {
        session_id: String,
        replay_len: usize,
        /// この attach が pty の大きさを実際に変えたか（protocol 8）。
        ///
        /// client の画面の作り方が変わる: 大きさが変わると readline は
        /// カーソルの居る行を `\r` から描き直すので、そこにバナーが在ると消える。
        /// ∴ 変わるときはバナーを自分の行に置き、プロンプトを出させるための改行を送らない
        /// （SIGWINCH の描き直しがその役をする）。変わらないときは SIGWINCH が鳴らないので改行を送る。
        ///
        /// 注意: 見るだけの attach（`read_only`）では常に `false`（見る側は大きさに口を出さない）。
        /// 古い berthd は送ってこないので `default` の `false`。
        #[serde(default)]
        resized: bool,
        /// 録画の頭より前に捨てた出力が残した状態（protocol 12）。
        /// `None` は「捨てていない」、または古い berthd（欄を送ってこない）。どちらも「頭に前提は無い」として描く。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay_head: Option<ReplayHead>,
        /// セッション開始から produce した累計バイト数（protocol 13+）。
        ///
        /// 単調増加。録画は常にその末尾（バイト `[replay_total − replay_len, replay_total)`）。
        /// client はこれを「どこまで見せたか」の基準にし、抜けるときペインに覚える。次に同じペインで
        /// 入り直すとき `Attach.seen_total` で送り返す。
        /// `None` は古い berthd（欄を送ってこない）。client はペインに覚えず、録画をそのまま全量受け取る。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay_total: Option<u64>,
    },
    Exited {
        session_id: String,
        status: i32,
    },
    Renamed {
        session_id: String,
        name: String,
    },
    /// `Ping` への応答（protocol 3+）。これが timeout 内に返らなければ client は berthd の死亡（または
    /// トンネルの切断）と判定する。payload の無い軽い往復。
    Pong,
    /// 外部からの `Detach` で attach を切られた通知（protocol 4+）。別接続の `berth detach <name>` が
    /// berthd に届くと、いま session に attach しているクライアント（対話の `berth attach` など）へこれを送る。
    /// 受け取った client は raw モードを戻して離脱する（子 PTY は生きたまま・後で再 attach できる）。
    Detached {
        session_id: String,
        /// なぜ外されたか（protocol 7+）。`None` は外部の `Detach`（`berth detach`）か、
        /// 別の接続の attach による置き換え。`Some("too_slow")` は見るだけの接続が出力に
        /// 追いつけず外された。無いときは欄ごと出さない（古い client の読みを変えない）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 出力を落としたことの通知（protocol 6+）。
    ///
    /// 重要: これは直しではない。「N byte 落とした」を出しても、その N byte は戻らない。
    /// client はこれを「直った」と読ませてはいけない。
    ///
    /// 通知は、溢れているその同じ口を通る。∴ 落とした瞬間には送れず、口が空いた最初の機会に
    /// まとめて出る。畳みは制約から出ているので、間引きの規則は別に無い。
    Dropped {
        session_id: String,
        /// 落としたバイト数。この通知が畳んでいる区間の合計（1 回の失敗ではない）。
        bytes: u64,
        /// 落とした回数。`bytes` だけだと「1 回で大きく」と「何度も少しずつ」が同じ顔になるので、
        /// 分布を読めるように回数も運ぶ。
        events: u32,
        /// この通知が覆う区間（ms）。0 は「測れなかった」ではなく「同じ tick 内」。
        window_ms: u32,
    },
    /// `SendKeys` の受理応答（protocol 14+）。
    SendKeysAck {
        session_id: String,
        bytes: usize,
    },
    /// `Peek` への応答（protocol 14+）。`data_len > 0` なら直後に `KIND_DATA` フレームが 1 本続く。
    PeekResult {
        session_id: String,
        data_len: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay_head: Option<ReplayHead>,
        cols: u16,
        rows: u16,
        idle_ms: u64,
        produced: u64,
        attached: bool,
    },
    Error {
        code: String,
        message: String,
    },
}

/// `Attached.replay_head`（protocol 12）: 録画の頭より前に捨てた出力が残した状態。
///
/// 命令の列ではなく状態。命令は捨てられても、命令が残した状態はセッションに続いている。持つのは
/// スクロールへの描き直しに効く 3 つだけ。画面の中身（格子）は持たない。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayHead {
    /// 捨てた時点で alt screen（`?1049` / `?1047` / `?47`）の中に居たか。
    #[serde(default)]
    pub alt_screen: bool,
    /// 捨てた時点で効いていた文字の色と装飾を、そのまま端末へ書ける 1 本の `ESC[…m` にした物。
    /// 何も効いていなければ空。
    #[serde(default)]
    pub sgr: String,
    /// 捨てた時点の題名（最後の OSC 0 / 2）。無ければ `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `List` 応答の 1 session 分。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// 内部の鍵（client が渡した session id と同一）。再利用しない（寝ている session を新しい client が
    /// 掴まないための不変条件）。人間に見せる handle は `name` の方。
    pub id: String,
    /// 人間用の名前（既定 `session-N`・`Rename` で変更可）。id と違って読める / 変えられる。
    /// 古い berthd は送ってこないので `default`（空）→ client は id で代用する。
    #[serde(default)]
    pub name: String,
    pub shell: String,
    pub cwd: String,
    /// `cwd` がどこから来たか（protocol 10）。
    ///
    /// 「今の値」と「起動時の値」を同じ顔にしないために在る欄。berthd は `List` のたびに子シェルの
    /// 今の cwd を引き直すが、引けなければ spawn 時に記録した値へ落ちる。
    ///
    /// | 値 | 意味 |
    /// |---|---|
    /// | `proc` | 今の値（Linux・`/proc/<pid>/cwd`）|
    /// | `proc_pidinfo` | 今の値（macOS・`PROC_PIDVNODEPATHINFO`）|
    /// | `spawn:unavailable:<機構>` | 起動時の値（機構は在るが引けなかった） |
    /// | `spawn:unimplemented:live_cwd` | 起動時の値（その面に道が無い、Windows は PEB 読みが要る） |
    /// | `spawn:unavailable:pid` | 起動時の値（子の pid が無い、もう居ない 等） |
    ///
    /// 注意: `spawn:` で始まるなら、それは今の値ではない。空欄は「古い berthd」の意味
    /// （下の `default`）であって「今の値」ではない（名乗らない物を信じないこと）。
    /// 綴りの作法は Rheon Protocol の `spec/ACTOR_v1.md` §2.4（`identity_src` / `ancestry_src`）と同じ。
    #[serde(default)]
    pub cwd_src: String,
    pub alive: bool,
    pub cols: u16,
    pub rows: u16,
}

/// `Control` を Rheon Protocol v1 のエンベロープ付き JSON バイト列にする。
/// serde の内部タグ enum が `{"event":"spawn",..}` を出すので、そこへ `v` / `source` / `type` / `ts` を足す。
///
/// 時刻の綴りは `ts`（レコードの時刻）。`source` は呼び手が渡す（そのレコードを出したプロセスの名前）。
/// [`decode_control`] は未知フィールドを無視するので、版が混ざってもどちらの向きでも読める。
pub fn encode_control(source: &str, c: &Control) -> Vec<u8> {
    let mut val = serde_json::to_value(c).expect("Control serialize");
    if let serde_json::Value::Object(ref mut m) = val {
        m.insert("v".into(), serde_json::json!(RHEON_V));
        m.insert("source".into(), serde_json::json!(source));
        m.insert("type".into(), serde_json::json!(SESSION_TYPE));
        m.insert("ts".into(), serde_json::json!(now_rfc3339()));
    }
    serde_json::to_vec(&val).expect("Control to_vec")
}

/// エンベロープ付き JSON を `Control` に戻す（`v` / `source` / `type` / `ts` は未知フィールドとして無視される）。
pub fn decode_control(payload: &[u8]) -> serde_json::Result<Control> {
    serde_json::from_slice(payload)
}

/// 制御フレームを作る（`encode_control` + フレーム化）。
pub fn control_frame(source: &str, serial: u32, c: &Control) -> Frame {
    Frame::control(serial, encode_control(source, c))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 実際に使う socket path。`BERTHD_SOCKET` env が最優先、無ければ [`default_socket_path`]。
/// daemon と client の両方がこれを使ってパスを一致させる。
pub fn socket_path() -> std::path::PathBuf {
    std::env::var_os("BERTHD_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

/// 既定の unix socket path。`$XDG_RUNTIME_DIR/berthd.sock`、無ければ `~/.berth/berthd.sock`
/// （macOS は `XDG_RUNTIME_DIR` が無いのでこちらに乗る）。親ディレクトリが無いと bind に
/// 失敗するので、ここで作る。
pub fn default_socket_path() -> std::path::PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = std::path::Path::new(&rt);
        if p.is_dir() {
            return p.join("berthd.sock");
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let dir = home.join(".berth");
    ensure_private_dir(&dir);
    dir.join("berthd.sock")
}

/// `~/.berth` を本人だけが入れるディレクトリとして用意する（無ければ作り、有れば締める）。
///
/// 注意: ここが macOS 側の唯一の壁。Linux は socket が `$XDG_RUNTIME_DIR`（`drwx------`）に居るので
/// 構造的に守られるが、macOS には `XDG_RUNTIME_DIR` が無く `~/.berth` に落ちる。そこが既定の
/// `0755` のままだと誰でも中まで到達でき、残る壁は socket 自身の mode だけになる。
/// しかもその mode は起動元から継いだ umask 次第で berthd の決定ではない。`umask 002` で
/// 起こされれば socket は `0775` になり、macOS ではローカルユーザー全員が `staff` グループなので
/// 「同じ Mac の誰でも attach できる」（すなわち本人として任意コード実行）に化ける。
/// ∴ 環境が良いことに賭けず、ここで決める。
///
/// ディレクトリの実行ビットは「中に入れる」の意味なので `0700`（`0600` だと自分すら入れない）。
/// 既存のディレクトリにも当てる（過去に緩く作られた物を締め直すため。所有者の権限は減らない）。
/// 失敗しても続行する（権限を触れない環境で berthd ごと止める理由が無い）。
pub fn ensure_private_dir(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

// ── 監査台帳（attach / detach）の語彙 ────────────────────────────────────────
//
// なぜ berthd が台帳を持つのか: berthd を挟むと、シェルの側の記録は「誰が打ったか」を見失う。
// セッションのシェルは berthd の env を継ぐだけで、ssh で入ってきた相手の身元は境界で消える。
//
// env を運んでも解けない: berthd は永続でシェルが attach より長生きし、後から別の誰かが同じ
// セッションに入れる。「今打っているのは誰か」は spawn 時の env では原理的に表せない。
// 身元はプロセスではなく attach に属する。だから attach 自体を記録する。
//
// 自称ではなく観測: unix socket の peer credential（uid / pid）はカーネルが答えるので、
// client が名乗る必要も、名乗りを信じる必要もない。
//
// 台帳は berthd 自身の物（`~/.berth/` の下）で、どのシェルの記録にも書かない。berthd は
// シェルに依らない。シェル側の記録とは `berthd_session` を join 鍵にして突き合わせる
// （子は `BERTHD_SESSION` で同じ値を見ている）。

/// attach してきた者の自称（`Attach` フレームで運ばれてくる・protocol 5+）。
///
/// peer credential がカーネルの観測なのに対し、こちらは client の主張。両方を 1 行に並べるのが要点で、
/// 一致していれば退屈・食い違っていれば調べる価値がある、という読み方ができる。観測だけでは
/// 「この uid で誰かが打っている」までしか言えず、人もエージェントも同じ uid で動く環境では
/// そこで止まる。
///
/// 注意: 検証しない。自称は誰でもできる。それは欠陥ではなく、この構造体が申告を申告として
/// 記録する場所だから。信じるかどうかは、隣の観測と突き合わせる読み手が決める。
#[derive(Clone, Debug, Default)]
pub struct Claim {
    /// 自称する名前（`claude-code` / `deploy-bot` …）。名乗らなければ `None`。
    pub actor: Option<String>,
    /// その名前の出所（`audit_actor` / `agent`）。名前からは引けないのでレコードが持つ。
    /// 注意: 古い台帳の行には `hsh_actor` も在る（いまは書かない・台帳は追記のみ）。
    pub src: Option<String>,
}

impl Claim {
    /// 環境から名乗りを拾う（client 側で呼ぶ）。
    ///
    /// 注意: client ごとに二重実装しない。優先順位や正規化が片方だけ変わると、同じ台帳の中で
    /// 同じエージェントが別名で並んだり、`claimed_actor_src` が実際の解決とずれて嘘になる。
    #[must_use]
    pub fn from_env() -> Self {
        let pick = |k: &str| std::env::var(k).ok();
        Self::from_parts(pick("AUDIT_ACTOR"), pick("AI_AGENT"))
    }

    /// [`Claim::from_env`] の純粋なコア（env を引数にしてあるので、環境に依らないテストが書ける）。
    ///
    /// 順序: `AUDIT_ACTOR`（管理者が付ける名前・中立な綴り。Rheon Protocol の `spec/ACTOR_v1.md` §2.6）→
    /// エージェントの自己申告（汎用の `AI_AGENT`）。管理者が触る変数は `AUDIT_ACTOR` の 1 つだけ。
    ///
    /// 読まない物:
    /// - シェル固有・ベンダ固有の変数（`HSH_ACTOR` / `CLAUDE_CODE_ENTRYPOINT` など）。berthd は
    ///   シェルに依らないので、特定の製品の変数を読まない。名乗りを分けて数えたい人は `AUDIT_ACTOR` を設定する。
    /// - 種別の変数。種別は名前についての事実で、読む側が名前で分ける。
    /// - `USER` / `LOGNAME`。ここで OS ユーザー名を入れると、誰も名乗っていない attach が
    ///   「名乗った」ように見える。しかもその値はカーネルが既に `peer_uid` として同じ行に書いており、
    ///   自称欄への複製は情報を増やさず信頼度だけ水増しする。
    ///
    /// 注意: 空文字は、どの段でも「無い」と同じに扱う。ここで畳むので、`from_env` を通らない
    /// 呼び手でも `AUDIT_ACTOR=` が「名乗った上で空」にならず、下の段へ落ちる。
    ///
    /// 注意: エージェントの値は集計できる形に正規化する（`claude-code_2-1-208_agent` → `claude-code`）。
    /// シェルの側（hsh）と同じ規則にしておかないと、2 つの記録を突き合わせたときに
    /// 同じエージェントが別名で並ぶ。
    #[must_use]
    pub fn from_parts(audit_actor: Option<String>, ai_agent: Option<String>) -> Self {
        let given = |v: Option<String>| v.filter(|s| !s.is_empty());
        if let Some(a) = given(audit_actor) {
            return Self {
                actor: Some(a),
                src: Some("audit_actor".into()),
            };
        }
        if let Some(raw) = given(ai_agent) {
            let head = raw.split('_').next().unwrap_or(&raw).to_string();
            return Self {
                actor: Some(head),
                src: Some("agent".into()),
            };
        }
        Self::default()
    }
}

// ── デーモンへの道（transport）: 面が違えば、道の種類そのものが違う ──
//
// `socket_path()` は「パス」を返すが、Windows の待ち口はパスではない。名前付きパイプは
// file system に存在せず、`\\.\pipe\berthd-<user>` という名前。同じ関数の戻り値を面で
// 分けることはできず、型ごと割れる。
//
// ∴ 「どこで待つか」ではなく「どう繋ぐか」を公開する。呼ぶ側は端点の綴りを知らなくてよく、
// `connect_daemon()` を呼ぶだけになる。

/// デーモンとの接続。面ごとに実体が違う（unix socket / 名前付きパイプ）。
///
/// どちらも `AsyncRead + AsyncWrite + Unpin + Send` を満たすので、
/// 上位（フレームの読み書き）はこの型を知らずに書ける。
#[cfg(unix)]
pub type DaemonStream = tokio::net::UnixStream;
/// デーモンとの接続（Windows は名前付きパイプの client 側）。
#[cfg(windows)]
pub type DaemonStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// 待ち口の表示名（ログとエラーに出す 1 行）。
///
/// 注意: これを「パス」として扱わないこと（Windows では file system に存在しない）。
#[must_use]
pub fn endpoint_display() -> String {
    #[cfg(unix)]
    {
        socket_path().display().to_string()
    }
    #[cfg(windows)]
    {
        pipe_name()
    }
}

/// Windows の名前付きパイプの名前。`BERTHD_SOCKET` env が最優先（unix の socket path と同じ役）。
///
/// 利用者ごとに分ける。名前付きパイプは machine 全域の名前空間に居るので、
/// 名前が固定だと同じ機械の別の利用者と衝突する（unix 側が `$XDG_RUNTIME_DIR` や
/// `~/.berth`（`0700`）で構造的に分けているのと同じことを、名前でやる）。
///
/// 重要: これは権限の壁ではない。名前を分けることは、権限を分けることではない。
/// 壁は口を作る側が付ける（`ServerOptions::create_with_security_attributes_raw` に
/// DACL を渡す）。作成時にしか決められない（名前付きパイプを後から締める道は実質無い）ので、
/// インスタンスを作る場所すべてが渡さないと、2 本目以降の口が素のままになる。
///
/// berthd は自分の口に DACL を付けている。ここは名前を返すだけなので、この名前を使う
/// 別の server は、自分で DACL を渡さないと既定の DACL のままになる。
///
/// 既定の DACL は `Everyone` と `Anonymous` に読み取り（`FR`）を与える。書き込みは与えないので、
/// 既定のままでも他の利用者が attach して打鍵することは出来ないが、口を開くことは出来る。
/// tokio の `reject_remote_clients` は既定で `true` なので、`\\<host>\pipe\…` の遠隔からは届かない
/// （射程は同じ機械のローカル利用者に閉じる）。
#[cfg(windows)]
#[must_use]
pub fn pipe_name() -> String {
    if let Some(v) = std::env::var_os("BERTHD_SOCKET") {
        return v.to_string_lossy().into_owned();
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
    format!(r"\\.\pipe\berthd-{user}")
}

/// デーモンへ繋ぐ。呼ぶ側は端点の綴りを知らなくてよい。
///
/// # Errors
/// デーモンが待っていない / 権限が無い等、OS の接続失敗をそのまま返す。
pub async fn connect_daemon() -> std::io::Result<DaemonStream> {
    connect_daemon_at(endpoint_os()).await
}

/// 端点を名指して繋ぐ。`connect_daemon()` はこれに既定の端点を渡すだけ。
///
/// # なぜ「名指す口」が要るか
///
/// `connect_daemon()` は `BERTHD_SOCKET` から端点を解く（プロセス全域の状態）。
/// テストは同じプロセスで並列に走り、1 本ずつ別の berthd を起こす（`BERTHD_SOCKET` は子にだけ渡す）
/// ので、`connect_daemon()` を呼ぶと全部が既定の端点へ行き、隣のテストと衝突する。
///
/// ∴ 「どう繋ぐか」（面の知識）と「どこへ繋ぐか」（呼び手の知識）を分けてある。
///
/// # Errors
/// デーモンが待っていない / 権限が無い等、OS の接続失敗をそのまま返す。
pub async fn connect_daemon_at<E: AsRef<std::ffi::OsStr>>(
    endpoint: E,
) -> std::io::Result<DaemonStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(std::path::Path::new(endpoint.as_ref())).await
    }
    #[cfg(windows)]
    {
        // 重要: 名前付きパイプは「1 インスタンス 1 接続」。server は客が付いた瞬間に
        //    次のインスタンスを作るが、その隙に来た客は `ERROR_PIPE_BUSY`(231) で弾かれる。
        //    これは「居ない」ではなく「順番待ち」なので、短く待って撃ち直すのが Win32 の作法。
        // 注意: 待ちは短く保つ。長くすると「居ない」が「遅い」に化け、
        //    呼び手が「繋がらない」と「まだ来ていない」を区別できなくなる。
        const BUSY: i32 = 231;
        for _ in 0..20 {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint.as_ref()) {
                Err(e) if e.raw_os_error() == Some(BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                other => return other,
            }
        }
        // 最後の 1 回はそのまま返す（諦めた理由（busy）を呼び手に見せる）。
        tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint.as_ref())
    }
}

/// この面の既定の端点を、そのまま繋ぎ先として渡せる形で返す。
///
/// [`endpoint_display`] は人に見せるための `String`。こちらは機械に渡すための
/// `OsString`（unix ではパスに非 UTF-8 が入りうるので、表示用で往復させない）。
#[must_use]
pub fn endpoint_os() -> std::ffi::OsString {
    #[cfg(unix)]
    {
        socket_path().into_os_string()
    }
    #[cfg(windows)]
    {
        std::ffi::OsString::from(pipe_name())
    }
}

/// この走行だけの端点を組む（テスト用）。面の形を知っているのはここ 1 箇所。
///
/// テストは「他のテストと衝突しない端点」を要る。その綴りは面で違う
/// （unix は一時ディレクトリのパス / Windows は名前空間の名前）ので、
/// テストごとに書くと面の知識が何箇所にも写る。∴ ライブラリに置いてある。
///
/// `tag` は呼び手が付ける印。pid を混ぜるので、同じ tag を別のプロセスで使ってもぶつからない。
/// 注意: 後始末はしない。unix 側はファイルが残るので、呼び手が消すこと
/// （Windows の名前付きパイプはプロセスが死ねば消える。面で後始末の要否が違う）。
#[must_use]
pub fn scratch_endpoint(tag: &str) -> std::ffi::OsString {
    // pid だけでは足りない。テストは同じプロセスで並列に走るので、
    // 同じ tag を 2 回使うと衝突する。∴ プロセス内の連番も混ぜる。
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = format!("{tag}-{}-{n}", std::process::id());
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("berthd-{unique}.sock"))
            .into_os_string()
    }
    #[cfg(windows)]
    {
        std::ffi::OsString::from(format!(r"\\.\pipe\berthd-{unique}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名乗りの解決順（環境を触らない純粋テスト）。
    #[test]
    fn an_explicit_name_wins_over_a_trace_left_in_the_environment() {
        let c = Claim::from_parts(Some("deploy-bot".into()), Some("claude-code".into()));
        assert_eq!(c.actor.as_deref(), Some("deploy-bot"));
        assert_eq!(c.src.as_deref(), Some("audit_actor"));
    }

    /// `AUDIT_ACTOR` が一番強い（管理者が付ける中立な名前）。
    /// 痕跡が 2 通りとも同時に在る状況で勝つこと（順序そのものを釘付けする。
    /// （1 段ずつ見るテストだと、順序を入れ替えても緑のままになる）。
    #[test]
    fn the_administrators_name_wins_over_every_trace() {
        let c = Claim::from_parts(
            Some("deploy-bot".into()),
            Some("claude-code_2-1-268_agent".into()),
        );
        assert_eq!(c.actor.as_deref(), Some("deploy-bot"), "{c:?}");
        assert_eq!(c.src.as_deref(), Some("audit_actor"), "{c:?}");
    }

    /// シェル固有・ベンダ固有の変数を読まないことを釘付けする。
    /// 読む段を消しただけだと、誰かが「互換のため」と足し直してもテストは緑のまま。
    /// 読まないという決定そのものをテストが持つ。
    #[test]
    fn the_shell_specific_variable_is_not_read_any_more() {
        // 注意: env はプロセス全域。env を触るテストはこの 1 本に閉じてある。
        // SAFETY: 他のスレッドが env を読み書きしていない（この検体は単独で完結する）。
        unsafe {
            std::env::set_var("HSH_ACTOR", "someone-else");
            std::env::remove_var("AUDIT_ACTOR");
            std::env::remove_var("AI_AGENT");
            // ベンダ固有の綴りも読まない（env を触るテストを増やさないよう、同じ 1 本で見る）。
            std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
        }
        let c = Claim::from_env();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("HSH_ACTOR");
            std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        }
        assert!(
            c.actor.is_none() && c.src.is_none(),
            "HSH_ACTOR / CLAUDE_CODE_ENTRYPOINT だけが在る席では、誰も名乗っていないこと: {c:?}"
        );
    }

    /// 注意: 空の `AUDIT_ACTOR=` は名乗りではない。下の段へ落ちること。
    /// どの段も空なら沈黙のまま（「名乗った上で空」を作らない）。
    #[test]
    fn an_empty_name_is_no_name_at_every_step() {
        let c = Claim::from_parts(Some(String::new()), Some("deploy-bot".into()));
        assert_eq!(c.src.as_deref(), Some("agent"), "{c:?}");
        let c = Claim::from_parts(Some(String::new()), Some(String::new()));
        assert!(c.actor.is_none() && c.src.is_none(), "{c:?}");
    }

    /// 痕跡から拾うときは集計できる形に正規化する。シェルの側（hsh）と同じ規則で
    /// ないと、2 つの記録を突き合わせたときに同じエージェントが別名で並ぶ。
    #[test]
    fn an_agents_name_is_normalised_the_same_way_the_shell_normalises_it() {
        let c = Claim::from_parts(None, Some("claude-code_2-1-208_agent".into()));
        assert_eq!(c.actor.as_deref(), Some("claude-code"));
        assert_eq!(c.src.as_deref(), Some("agent"));
    }

    /// 注意: 誰も名乗らなければ空。ここで OS ユーザー名に落ちてはいけない。
    /// それは隣の `peer_uid` の焼き直しで、沈黙を主張に変えてしまう。
    #[test]
    fn silence_stays_silence() {
        let c = Claim::from_parts(None, None);
        assert!(c.actor.is_none() && c.src.is_none(), "{c:?}");
    }

    /// protocol 7: 古い client の `Attach`（`read_only` を持たない）は操作する側として読める。
    /// そして `Detached` は `reason` が無ければ欄ごと出ない（古い client の読みを変えない）。
    #[test]
    fn an_attach_without_read_only_is_an_operator_and_a_plain_detached_carries_no_reason() {
        let new = encode_control(
            "berth",
            &Control::Attach {
                session_id: "s".into(),
                cols: 80,
                rows: 24,
                actor: None,
                actor_src: None,
                read_only: true,
                seen_total: None,
            },
        );
        let mut v: serde_json::Value = serde_json::from_slice(&new).unwrap();
        assert_eq!(v["read_only"], serde_json::json!(true), "{v}");
        // 古い client の形（欄が無い）。
        v.as_object_mut().unwrap().remove("read_only");
        let old = serde_json::to_vec(&v).unwrap();
        match decode_control(&old).unwrap() {
            Control::Attach { read_only, .. } => assert!(!read_only, "{v}"),
            other => panic!("expected Attach, got {other:?}"),
        }

        let plain = encode_control(
            "berthd",
            &Control::Detached {
                session_id: "s".into(),
                reason: None,
            },
        );
        let v: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert!(v.get("reason").is_none(), "{v}");
    }

    /// protocol 12: `Attached.replay_head` は在るときだけ欄に出て、無い（古い berthd）なら `None` に落ちる。
    #[test]
    fn the_replay_head_rides_on_attached_only_when_something_was_dropped() {
        let head = ReplayHead {
            alt_screen: true,
            sgr: "\x1b[1;31m".into(),
            title: Some("vim".into()),
        };
        let with = encode_control(
            "berthd",
            &Control::Attached {
                session_id: "s".into(),
                replay_len: 7,
                resized: false,
                replay_head: Some(head.clone()),
                replay_total: None,
            },
        );
        let v: serde_json::Value = serde_json::from_slice(&with).unwrap();
        assert_eq!(
            v["replay_head"]["alt_screen"],
            serde_json::json!(true),
            "{v}"
        );
        assert_eq!(
            v["replay_head"]["sgr"],
            serde_json::json!("\x1b[1;31m"),
            "{v}"
        );
        match decode_control(&with).unwrap() {
            Control::Attached { replay_head, .. } => assert_eq!(replay_head, Some(head)),
            other => panic!("expected Attached, got {other:?}"),
        }

        // 捨てていない → 欄ごと出ない。
        let without = encode_control(
            "berthd",
            &Control::Attached {
                session_id: "s".into(),
                replay_len: 7,
                resized: false,
                replay_head: None,
                replay_total: None,
            },
        );
        let mut v: serde_json::Value = serde_json::from_slice(&without).unwrap();
        assert!(v.get("replay_head").is_none(), "{v}");
        // 古い berthd の形（欄が無い、`resized` も無い）→ どちらも既定に落ちる。
        v.as_object_mut().unwrap().remove("resized");
        let old = serde_json::to_vec(&v).unwrap();
        match decode_control(&old).unwrap() {
            Control::Attached {
                replay_head,
                resized,
                ..
            } => {
                assert!(replay_head.is_none());
                assert!(!resized);
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    /// protocol 13: `Attach.seen_total` と `Attached.replay_total` は在るときだけ欄に出て、
    /// 無い（古い相手）なら `None` に落ち、今までどおりの全量再生になる。
    #[test]
    fn seen_total_and_replay_total_ride_only_when_present() {
        // client → server: seen_total を送る形と、送らない（ペイン未知/古い client）形。
        let with_seen = encode_control(
            "berth",
            &Control::Attach {
                session_id: "s".into(),
                cols: 80,
                rows: 24,
                actor: None,
                actor_src: None,
                read_only: false,
                seen_total: Some(4096),
            },
        );
        let v: serde_json::Value = serde_json::from_slice(&with_seen).unwrap();
        assert_eq!(v["seen_total"], serde_json::json!(4096), "{v}");
        match decode_control(&with_seen).unwrap() {
            Control::Attach { seen_total, .. } => assert_eq!(seen_total, Some(4096)),
            other => panic!("expected Attach, got {other:?}"),
        }

        let no_seen = encode_control(
            "berth",
            &Control::Attach {
                session_id: "s".into(),
                cols: 80,
                rows: 24,
                actor: None,
                actor_src: None,
                read_only: false,
                seen_total: None,
            },
        );
        let mut v: serde_json::Value = serde_json::from_slice(&no_seen).unwrap();
        assert!(v.get("seen_total").is_none(), "{v}");
        // 古い client の形（欄ごと無い）→ None に落ちる。
        v.as_object_mut().unwrap().remove("seen_total");
        let old = serde_json::to_vec(&v).unwrap();
        match decode_control(&old).unwrap() {
            Control::Attach { seen_total, .. } => assert!(seen_total.is_none()),
            other => panic!("expected Attach, got {other:?}"),
        }

        // server → client: replay_total を載せる形と、載せない（古い berthd）形。
        let with_total = encode_control(
            "berthd",
            &Control::Attached {
                session_id: "s".into(),
                replay_len: 7,
                resized: false,
                replay_head: None,
                replay_total: Some(1_048_576),
            },
        );
        let v: serde_json::Value = serde_json::from_slice(&with_total).unwrap();
        assert_eq!(v["replay_total"], serde_json::json!(1_048_576), "{v}");
        match decode_control(&with_total).unwrap() {
            Control::Attached { replay_total, .. } => assert_eq!(replay_total, Some(1_048_576)),
            other => panic!("expected Attached, got {other:?}"),
        }

        // 古い berthd の形（replay_total の欄が無い）→ None に落ちる。
        let mut v: serde_json::Value = serde_json::from_slice(&with_total).unwrap();
        v.as_object_mut().unwrap().remove("replay_total");
        let old = serde_json::to_vec(&v).unwrap();
        match decode_control(&old).unwrap() {
            Control::Attached { replay_total, .. } => assert!(replay_total.is_none()),
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    #[test]
    fn control_roundtrips_through_rheon_envelope() {
        let c = Control::Spawn {
            session_id: None,
            name: None,
            shell: Some("zsh".into()),
            shell_src: Some("parent_shell".into()),
            cwd: Some("/tmp/t".into()),
            env: None,
            label: None,
            cols: 80,
            rows: 24,
        };
        // `source` は呼び手が渡す（そのレコードを出したプロセスの名前）。
        let bytes = encode_control("argotty", &c);
        // エンベロープが Rheon Protocol v1 の形であること。
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["v"], 1);
        // 呼び手が名乗った物がそのまま出ること。ここが固定の値に戻ったら、
        // 送り手の身元が消えて台帳が「全部デーモンが出した」に化ける。
        assert_eq!(
            v["source"], "argotty",
            "呼び手の名乗りが `source` に出ていない"
        );
        assert_eq!(v["type"], "session");
        assert_eq!(v["event"], "spawn");
        assert_eq!(v["cols"], 80);
        // 時刻は `ts`。旧綴り（`modified`）が同時に出ていないことも見る。
        // 「両方書く」で移行したつもりになると、消費者はどちらを信じるか決められない。
        assert!(v.get("ts").is_some(), "ts が無い");
        assert!(v.get("modified").is_none(), "旧綴り modified が残っている");
        // decode で Control に戻る（envelope フィールドは無視される）。
        match decode_control(&bytes).unwrap() {
            Control::Spawn {
                shell, cols, rows, ..
            } => {
                assert_eq!(shell.as_deref(), Some("zsh"));
                assert_eq!((cols, rows), (80, 24));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn frame_roundtrips_over_a_pipe() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = Frame::data(0, b"hello\x1b[0m world".to_vec());
        write_frame(&mut a, &sent).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got.kind, KIND_DATA);
        assert_eq!(got.serial, 0);
        assert_eq!(got.payload, sent.payload);
    }

    #[test]
    fn send_keys_and_peek_roundtrip_through_rheon_envelope() {
        let send = Control::SendKeys {
            session_id: "s1".into(),
            data: b"echo ok\r".to_vec(),
            force: false,
        };
        let decoded = decode_control(&encode_control("berth", &send)).unwrap();
        match decoded {
            Control::SendKeys {
                session_id,
                data,
                force,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(data, b"echo ok\r");
                assert!(!force);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let peek = Control::PeekResult {
            session_id: "s1".into(),
            data_len: 42,
            replay_head: None,
            cols: 80,
            rows: 24,
            idle_ms: 1200,
            produced: 4096,
            attached: true,
        };
        let decoded = decode_control(&encode_control("berthd", &peek)).unwrap();
        match decoded {
            Control::PeekResult {
                session_id,
                data_len,
                cols,
                rows,
                idle_ms,
                produced,
                attached,
                ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(data_len, 42);
                assert_eq!((cols, rows), (80, 24));
                assert_eq!(idle_ms, 1200);
                assert_eq!(produced, 4096);
                assert!(attached);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    /// 綴りをバイトで釘付けする。
    ///
    /// 文字列の中のバックスラッシュが経路の途中で食われると、`\a` が BEL、`\b` が BS のような
    /// 制御文字に化ける。文字列としては正しいのでコンパイラでは捕まらず、バイトを見るテストが要る。
    ///
    /// 注意: 製品名を変えたらこのテストは赤くなる。それが正しい。綴りは人が目で確かめる物なので、
    /// テストが自分で追随してはいけない（追随させると「変わったこと」が誰にも見えなくなる）。
    #[test]
    fn the_scratch_endpoint_is_spelled_for_this_face() {
        let e = scratch_endpoint("probe");
        let s = e.to_string_lossy();
        // 一覧にしない、族で塞ぐ。特定の 1 文字（BEL など）だけを見ていると、綴りが変わった日に
        // 別の文字（`\b`: BS など）が素通しする。
        //
        // `is_control()` は C0（U+0000〜U+001F）だけでなく DEL(0x7F) と C1（U+0080〜U+009F）
        // も見る。経路が壊す文字を先に予想しないための選び方。
        if let Some(c) = s.chars().find(|c| c.is_control()) {
            panic!(
                "制御文字 U+{:04X} が紛れている（綴りが経路で壊れた）: {s:?}",
                c as u32
            );
        }
        #[cfg(windows)]
        {
            assert!(
                s.starts_with(r"\\.\pipe\berthd-probe-"),
                "名前付きパイプの綴り: {s:?}"
            );
        }
        #[cfg(unix)]
        {
            assert!(
                s.ends_with(".sock") && s.contains("berthd-probe-"),
                "socket path: {s:?}"
            );
        }
    }

    /// 注意: 同じ tag を並列に使ってもぶつからないこと（pid だけでは足りない）。
    #[test]
    fn the_same_tag_twice_gives_two_endpoints() {
        assert_ne!(scratch_endpoint("dup"), scratch_endpoint("dup"));
    }

    /// 既定の端点は、人に見せる綴りと機械に渡す綴りが一致すること。
    #[test]
    fn the_default_endpoint_reads_the_same_both_ways() {
        assert_eq!(endpoint_os().to_string_lossy(), endpoint_display());
    }
}
