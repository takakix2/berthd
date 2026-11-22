# berth-protocol

`berthd` と話すためのワイヤ契約です。フレーム、制御メッセージ、端点の解決だけを持ちます。

[`berthd`](https://github.com/takakix2/berthd) は PTY を所有する headless デーモンで、
シェルの寿命を端末アプリの寿命から切り離します（tmux / wezterm-mux-server と同じ棚）。
このクレートは、そこへ繋ぐ側と繋がれる側が共有する語彙です。berthd のリポジトリの中に在り、
デーモン（`berthd`）と CLI（`berth`）の両方がこれを引きます。

## できること

- **フレームの読み書き**: `read_frame` / `write_frame`。9 バイトのヘッダと本体。
- **制御メッセージ**: `Control` の 20 variant（spawn / attach / detach / resize / exit など）。
  ワイヤ上は Rheon Protocol v1 のレコード（`{v, source, type, event, ts}`）です。
- **端点を面から隠す**: `connect_daemon()` を呼べば、呼び手は unix socket と
  Windows の名前付きパイプの綴りを知らなくて済みます。
- **版のずれの検知**: `Hello` の応答が `protocol` を運ぶので、古いデーモンに繋いだ
  クライアントはそれを利用者に伝えられます。

## やらないこと

- **端末エミュレーション**: スクリーンの模型を持ちません。描画は客の仕事です。
- **デーモンそのもの**: PTY を所有するのは `berthd` です。ここに実装は在りません。
- **待ち受け**: 繋ぐ口（`connect_daemon`）だけを公開します。
- **台帳**: 誰がいつ繋いだかを書くのは `berthd` 自身です。
- **認証**: 端点の権限（`0700` のディレクトリ / 利用者ごとのパイプ名と DACL）に委ねます。

## 使い方

このクレートは berthd のワークスペースの一員です。リポジトリの中からは path で引きます。

```toml
[dependencies]
berth-protocol = { path = "crates/berth-protocol" }
```

```rust
use berth_protocol::{connect_daemon, control_frame, read_frame, write_frame, Control};

# async fn example() -> std::io::Result<()> {
let mut s = connect_daemon().await?;

// 名乗ってから、セッションの一覧を訊く
write_frame(&mut s, &control_frame("my-client", 0, &Control::Hello {
    client: "my-client/1.0".into(),
})).await?;
write_frame(&mut s, &control_frame("my-client", 1, &Control::List)).await?;

let frame = read_frame(&mut s).await?;
# Ok(())
# }
```

`source` は呼び手が名乗ります（このクレートは値を決めません）。台帳を読む側が
「どのプロセスが出したイベントか」を見分けるための欄です。

## ワイヤの形

```text
u32le length | u8 kind | u32le serial | payload[length]
                 │
                 ├─ 0 = control : Rheon Protocol v1 の JSON レコード 1 行
                 └─ 1 = data    : 生バイト（PTY の入出力）
```

`serial` は制御メッセージの往復を綴じる番号です。`data` フレームは順序だけが意味を持ちます。

端点は面で違います。

| 面      | 端点                   | 既定                                                             |
| ------- | ---------------------- | ---------------------------------------------------------------- |
| unix    | socket path            | `$XDG_RUNTIME_DIR/berthd.sock` → 無ければ `~/.berth/berthd.sock` |
| Windows | 名前付きパイプの名前   | `\\.\pipe\berthd-<user>`                                         |

どちらも `BERTHD_SOCKET` で上書きできます。

## 往復

順序は強制されません。`Hello` を送らずに `List` も `Spawn` も通ります。
この図が答えるのは「何を先に送るか」ではなく、省くと何を失うかです。

線は 2 種類あります（上の「ワイヤの形」の `kind`）。`---` が control（`kind 0`・JSON を 1 行）、
`===` が data（`kind 1`・PTY の生バイト）です。出力と打鍵を運んでいるのは `===` の 2 本だけで、
残りは「誰が・どのセッションを・どう扱うか」の取り決めです。`serial` で往復を綴じられるのも
control だけです。

```text
client                                            berthd
  |                                                 |
  |-- Hello{client} ------------------------------->|   省ける。省くと台帳に何が繋いだかが載らない
  |<------------------ Welcome{server,protocol,host}|   版のずれが分かるのはここだけ
  |                                                 |
  |-- List ---------------------------------------->|
  |<----------------------------- Sessions{sessions}|
  |                                                 |
  |-- Spawn{shell,cwd,cols,rows} ------------------>|
  |<------------------------ Spawned{session_id,pid}|
  |                                                 |
  |-- Attach{session_id,cols,rows,actor,read_only} >|   actor を省くと「誰が入ったか」の自称が消える
  |<------------------------ Attached{session_id}   |
  |<==== data（replay …そのまま live へ）===========|   境目は無い（同じ排他の下で継がれる）
  |===== data（キー入力）==========================>|
  |                                                 |
  |-- Detach{session_id} -------------------------->|   子は死なない
  |                                                 |
  |（誰かが別の端末で `berth detach <id>` を撃つと）|
  |<-------------------- Detached{session_id,reason}|   頼んでいないのに届く（外から外された / 置き換えられた / 遅くて外された）
  |<--------------- Dropped{bytes,events,window_ms} |   出力が溢れて捨てた、の申告
  |<---------------------------- Exited{session_id} |   子が終わった
```

省いたときに失う物:

| 省く物                        | 失う物 |
| ----------------------------- | ------ |
| `Hello{client}`               | 台帳の `claimed_client`。注意: Windows では相手の uid を観測できない（取れるのは pid とセッションだけ）ので、身元の手掛かりとして重みが大きい |
| `Welcome` を読まない          | 版のずれに気づけない。注意: 古い berthd は知らないメッセージを黙って捨てるので、新しい機能は「効かない」ではなく「無音」になる |
| `Attach.actor`                | 「誰が入ったか」の自称。観測（`peer_uid`）は berthd が自分で取るので消えないが、エージェントと人の区別が付かなくなる |
| `Attach.read_only`            | 見るだけの attach（protocol 7+）。省くと操作する側として入り、今の操作する側を置き換える。注意: 古い berthd はこの欄を読まずに操作する側にするので、`Welcome.protocol` が 7 未満なら送らないこと |
| `Attached.resized` を読まない | その attach が pty の大きさを変えたか（protocol 8+）。読まないと、大きさが変わる attach で最初の 1 行が消える（readline がカーソルの居る行を描き直すため）。版ゲートは不要で、古い berthd では `false` に落ちて今までの挙動になる |
| `Spawn.shell_src`             | その `shell` を誰が決めたか（protocol 9+）。省いても起きるシェルは変わらないが、台帳に `client:explicit`（客が名指しした）としか残らず、なぜそのシェルなのかが後から引けない。版ゲートは不要で、古い berthd は欄を捨てるだけ |

`Detached` / `Dropped` / `Exited` は頼まずに届きます。要求と応答だけを待つ client は、
セッションが下から消えたことに気づけません。

## 版

`PROTOCOL_VERSION` が上がるのは、客が振る舞いを変えなければならないときだけです。
デーモンは知らない制御メッセージを黙って捨てる（前方互換）ので、新しい客が古い
デーモンに繋ぐと黙って何も起きません。それを避けるため、`Hello` の応答が運ぶ
`protocol` を見て、客の側が機能を出し分けます。各版で足された物の一覧は
`PROTOCOL_VERSION` の doc に在ります。

欄を足しただけの版でも、黙って効かないでは済まない場合があります。protocol 7 の
`Attach.read_only` は、古いデーモンが欄を捨てると、見るだけのつもりの客が打鍵できる側になります。
そういう欄は「送ってから様子を見る」ではなく、版を確かめてから送ります（`berth attach -r` がそうします）。

逆に、版ゲートが要らない欄もあります。protocol 8 の `Attached.resized` は server→client の
知らせで、古いデーモンでは欄ごと来ず `false` に落ちます。`false` は「変わらなかった」、すなわち
8 より前の客が前提にしていた場合そのものなので、黙って今までの挙動になります。

見分け方は「欄が落ちたとき、既定が嘘になるか」です。`read_only` は落ちると `false`（すなわち
「操作する側」）に化けて頼んだのと反対の物になりますが、`resized` はなりません。

## ライセンス

ISC（リポジトリと同じ）。
