# berthd — 閉じても消えないシェルセッション

`berthd` は **PTY を所有する小さなデーモン**です。シェルの寿命を端末アプリから切り離すので、
**アプリを閉じてもシェルは動き続け、あとで同じセッションへ scrollback ごと戻れます**。
tmux と同じ種類の道具ですが、**画面の描画はしません**（端末エミュレーションはクライアントの仕事です）。

同梱の **`berth`** がセッションを操作する CLI です。どのシェルからでも使えます。

```
$ berth ls
* mywork  zsh   106x29  ~/src/app
  build   zsh   120x40  ~/src/lib
```

## 入れる

```sh
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

installer は次のことをします。

- 最新の release を落とし、`SHA256SUMS` で検算する（検算できない環境では入れません）
- `berthd` と `berth` を**必ず 2 本一緒に** `~/.local/bin` へ置く（`BERTHD_INSTALL_DIR` で変更できます）
- 入れる前に、動いている古いデーモンを畳む（**セッションが残っていれば止まって知らせます**）
- 旧名の `argod` / `argo` を消す

手で入れる場合:

```sh
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-linux-x86_64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

- 配布しているのは **Linux x86_64** と **macOS arm64**（`berthd-macos-arm64.tar.gz`）です。
- 展開先はディレクトリです（カレントに `berthd` は出来ません）。
- **片方だけ入れないでください** —— プロトコルが食い違ったまま動きます。

## 使う

```sh
berth attach work     # セッション work に入る（無ければ作る。berthd も必要なら自動で起動）
# … 作業 …
berth d               # セッションから抜ける（シェルは berthd の下で動き続ける）
berth attach work     # 同じシェルに戻る
```

| コマンド | |
|---|---|
| `berth attach [<name>]`（`a`） | このターミナルでセッションに入る（無ければ作る） |
| `berth detach [<name>]`（`d`） | セッションから抜ける（省略時は今いるセッション） |
| `berth ls` | 一覧（`*` が今いるセッション） |
| `berth rename [<session>] <name>`（`r`） | 名前を付け直す（省略時は今いるセッション） |
| `berth kill <session>` | セッションを閉じる（中のシェルごと） |
| `berth info` | 接続先の berthd（ホストと版）を表示 |
| `berth kill-server [-f]` | berthd を畳む（セッションが残っていれば `-f` が要る） |
| `berth --version` / `berthd --version` | 版を表示（デーモンには接続しません） |

`<name>` には名前（`main` など）か id を渡せます。セッションの中では `BERTHD_SESSION` に自分の
session id が入っているので、`berth ls` の `*` や、引数 1 つの `berth rename <name>` が効きます。

### デーモンの寿命

- berthd が居なければ `berth attach` が起動します。`ls` / `info` は起動しません（見るためだけに立てない）。
- berthd は**自分からは終了しません**（アイドルタイムアウトはありません）。畳むのは `berth kill-server` です。
- 他のマシンから ssh で入って attach する使い方では、berthd を常駐させておきます。
  systemd / launchd の設定は [dist/README.md](dist/README.md) にあります。

### 環境変数

| 変数 | 効果 |
|---|---|
| `BERTHD_SOCKET` | ソケットの場所（既定は `$XDG_RUNTIME_DIR/berthd.sock`、無ければ `~/.berth/berthd.sock`）。**明示すると `berth attach` は自動起動しません**（転送したソケットなど、別の接続先を指している可能性があるため） |
| `BERTHD_NO_SPAWN` | 立てると `berth attach` の自動起動を止める。常駐させている環境で二重起動を防ぐ |
| `BERTHD_BIN` | 自動起動する berthd の場所（既定は `berth` の隣、次に `PATH`） |
| `BERTHD_JOURNAL_PATH` | 監査台帳の場所（既定 `~/.berth/journal.jsonl`） |
| `BERTHD_SESSION` | **berthd が子プロセスに渡す**自分の session id（上記） |

## 監査台帳

berthd は attach の**開始と終了**を `~/.berth/journal.jsonl` に NDJSON（Flux v1 レコード）で残します。
1 行に**観測した事実と、クライアントの自己申告**の両方が載ります。

| 欄 | 何か | 出所 |
|---|---|---|
| `peer_uid` / `peer_pid` | 接続してきたプロセス | カーネルが観測した値 |
| `claimed_actor` / `claimed_actor_src` | クライアントが名乗った名前と、その出所 | クライアントの自己申告（検証しません） |

観測だけでは足りないのは、**人も AI エージェントも同じユーザーの uid で動く**からです。
自己申告だけでも足りません（誰でも名乗れます）。並べておけば、一致していれば問題なし、
食い違っていれば調べる、が 1 行で読めます。名乗らなかった attach の欄は空のままにします
（「誰も名乗らなかった」も事実なので、OS のユーザー名で埋めません）。

berthd はシェルの中で打たれたコマンドを記録しません（流れるバイト列を解釈しない設計です）。
コマンドの記録はシェル側の仕事で、シェルが `BERTHD_SESSION` を自分の記録に残せば
（例: hsh の `session:start`）、2 つの台帳を session id で突き合わせられます。
session id が示すのは「そのセッションの子孫である」ことなので、時刻は attach レコードの
開始・終了で絞ってください。

### 権限

ソケットは `0600`、`~/.berth` は `0700`、台帳は `0600` で berthd 自身が作ります（起動元の umask に
依存しません）。macOS には `XDG_RUNTIME_DIR` が無く、ソケットは `~/.berth` に置かれるため、
この締め付けがそのまま防御になります。

## 旧 `argod` / `argo` から乗り換える

v0.2.0 でデーモンは **`berthd`**、CLI は **`berth`** になりました。`argo` という名前は
[Argo Workflows](https://argoproj.io) の CLI と重なり、どちらにも `list` があるため、両方入っている
環境ではエラーにならずに**違う方の空の一覧**が返ります。この衝突を残さないため、`argo` の互換名は
用意していません。

```sh
argo attach build      # 旧
berth attach build     # 新
```

- installer は旧名のバイナリを消します。常駐させていた場合は unit / plist も入れ替えてください
  （[dist/README.md](dist/README.md)）。
- 環境変数は `ARGOD_*` → `BERTHD_*` です。
- 監査台帳が `~/.argo/journal.jsonl` に残っていれば、手で `~/.berth/journal.jsonl` へ移してください。
- `~/.argo/config.toml` は berthd の物ではないので、そのままで構いません。

## ライセンス

ISC（[LICENSE](LICENSE)）。同梱しているサードパーティの表示は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にあります。
