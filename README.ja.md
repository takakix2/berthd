<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/berthd-mark.svg">
    <img src="assets/berthd-mark-light.svg" width="160" alt="berthd: a pier, a bollard, and a mooring line looped around it">
  </picture>
</p>

# berthd — 閉じても消えないシェルセッション

[English](README.md)

**ウィンドウを閉じても、シェルが動き続けます。**

中身は **PTY を所有する小さなデーモン**です。シェルの寿命が端末アプリではなくデーモンの物になるので、
アプリを閉じてもシェルは動き続けます。セッションを生かし続けるという意味では tmux と同じですが、**ヘッドレス**です。
ペインやタブの描画を端末の仕事とすることで、小ささを保っています。どのターミナルでも動きます（Windows Terminal、Ghostty、Alacritty など）。
tmux のペイン、タブや分割、ssh の中でも動きます。`berth` と `berthd` の 2 本のバイナリがインストールされます。Windows・Linux・macOS で動作確認をしています。

**`berth`** がセッションを操作する CLI です。

```
$ berth ls
* claude  pwsh  120x40  C:\src\app
  build   pwsh  120x40  C:\src\lib
```

## インストール

インストーラーは最新のリリースをダウンロードして `SHA256SUMS` で検算し、`berthd` と `berth` を `~/.local/bin` に置きます（`BERTHD_INSTALL_DIR` で変更できます）。
入れ直すときは先に `berth kill-server` でデーモンを止めてください。セッションが残っていると中断します。

### Windows

PowerShell で:

```powershell
irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1 | iex
```

- 配布物は `.zip` なので、エクスプローラーからダブルクリックで開くこともできます。
- ⚠️ **Windows ARM64 向けはまだ配っていません。** x64 ビルドをエミュレーションで入れるなら、`-Target windows-x86_64` を付けてください:
  ```powershell
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1))) -Target windows-x86_64
  ```
- 常駐させるなら、タスクスケジューラのログオントリガを使います（管理者権限不要）。手順は [dist/README.md](dist/README.ja.md)。
- `berth attach` の中から入れ直すときは、使用中の `.exe` を退かしてから置きます。退かした `*.exe.old` はそのプロセスが終わるまで消せませんが、次に install したときに自動で掃きます。

手で入れる場合（標準の道具だけで済みます）:

```powershell
curl.exe -fsSL -o berthd.zip https://github.com/takakix2/berthd/releases/latest/download/berthd-windows-x86_64.zip
tar.exe -xf berthd.zip
Move-Item berthd-*\berth*.exe $env:USERPROFILE\.local\bin\ -Force
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

- 配布しているのは x86_64 です。
- 常駐させるなら systemd の user service を使います。手順は [dist/README.md](dist/README.ja.md)。

手で入れる場合:

```sh
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-linux-x86_64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

- 配布しているのは Apple Silicon（arm64）です。
- 常駐させるなら launchd の LaunchAgent を使います。手順は [dist/README.md](dist/README.ja.md)。

手で入れる場合:

```sh
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-macos-arm64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

### どの OS でも

`berth` と `berthd` のバージョンが異なると動かない場合があります。うまく動かないときは `berth info` で両方のバージョンを確認してください。

## 使い方

```sh
berth attach work     # セッション work に入る（無ければ作る。berthd も居なければ起こす）
# … 作業 …
berth d               # セッションから抜ける（シェルは berthd の下で動き続ける）
berth attach work     # 同じシェルに戻る
```

| コマンド                                   |                                                                                             |
| ------------------------------------------ | ------------------------------------------------------------------------------------------- |
| `berth attach [<name>]`（`a`）             | このターミナルでセッションに入る（無ければ作る）                                            |
| `berth attach -s <shell> [<name>]`         | **指定したシェル**でセッションを作る（無ければ作る。`-s` / `--shell`）                      |
| `berth attach -r <name>`                   | **見るだけ**で入る（打鍵も画面の大きさも送らない。`q` か Ctrl-C で抜ける）                  |
| `berth attach --no-scrollback [<name>]`    | **最後の画面だけ**戻す（既定は、これまでの出力をこの端末のスクロールに流してから戻る。下記） |
| `berth detach [<name>]`（`d`）             | セッションから抜ける（省略時は今いるセッション。見ている端末も含めて全員が抜ける）          |
| `berth ls`                                 | 一覧（`*` が今いるセッション）                                                              |
| `berth rename [<session>] <name>`（`r`）   | 名前を付け直す（省略時は今いるセッション）                                                  |
| `berth kill <session>`                     | セッションを閉じる（中のシェルごと）                                                        |
| `berth info`                               | 接続先の berthd（ホストとバージョン）と、**この端末で新しいセッションを作ったら起きるシェル**を表示 |
| `berth kill-server [-f]`                   | berthd を停止する（セッションが残っていれば `-f` が要る）                                   |
| `berth --version` / `berthd --version`     | バージョンを表示（デーモンには接続しません）                                                |

`<name>` には名前（`main` など）か id を渡せます。セッションの中では `BERTHD_SESSION` に自分の
session id が入っているので、`berth ls` の `*` や、引数 1 つの `berth rename <name>` が効きます。

### 入り直すと、これまでの出力が端末のスクロールで辿れる（既定）

`berth attach` で戻ると、そのセッションがこれまでに出した出力（直近 512KB ぶん）を**この端末自身の
スクロールバック**に流してから、今の画面を描き直します。tmux のように別の画面に閉じ込めないので、
ホイールや端末の検索・コピーがいつもどおり効きます。全画面アプリ（vim など）の中に居たまま戻った
ときは履歴を流さず、今の画面だけを描きます。

- **最後の画面だけでよい**（従来の動作）: `berth attach --no-scrollback <name>`
- **ずっと外す**: `BERTH_SCROLLBACK=off`
- **同じペインで入り直すと続きだけ流します**: ペインごとに安定した ID を出す端末（Windows Terminal・WezTerm・kitty・iTerm2・Apple Terminal・Alacritty）では、`berth` がどこまで見せたかを覚えていて新しい分だけ流すので、同じ履歴が二重に積まれません。ID を出さない端末では、毎回 512 KB をまるごと流す従来の動作になります。

### シェルを指定してセッションを作る（`-s` / `--shell`）

新しいセッションを作るとき、起動するシェルをアドホックに指定できます。
`-s <shell>`、`--shell <shell>`、`--shell=<shell>` が使えます。

```sh
berth attach -s /bin/bash debug      # このセッションだけ bash で起動する
berth attach -s hsh work             # hsh で新しいセッションを作る
berth attach -s powershell.exe win   # Windows でシェルを指定する
```

- **新規作成時（セッションが存在しないとき）のみ有効です。** すでに動いている既存セッションに attach する場合は、そのセッションで動いているシェルにそのまま接続します（後からシェルをすり替えることはしません）。
- **すべての OS（Linux / macOS / Windows）で効きます。** 環境変数（`BERTHD_SHELL`）や親プロセスの自動観測よりも最優先（出所: `client:cli_flag`）で解決されます。
- `-r`（read-only）と組み合わせることもできます（例: `berth attach -s /bin/bash -r monitor`）。

### 複数の端末から同じセッションに入る

- **キーを打てるのは常に 1 台です。** 別の端末から `berth attach` で同じセッションに入ると、前の端末は
  `[detached from …]` と表示して抜けます（シェルはそのまま動き続けます）。前の端末の打鍵が
  混ざることはありません。
- **見るだけなら何台でも**（`berth attach -r`）。画面の大きさは打っている端末が決めるので、見る側がそれより小さいと全画面アプリの表示は崩れます。
- 見る側が出力に追いつけないときは、**その見る側だけ**が外れます（打っている側は止まりません）。

### いつものペインを一度に開く

`berth attach <name>` は**無ければ作り、あれば繋ぐ**ので、レイアウトを開くスクリプトは
**初回も 2 回目も同じ 1 本**で済みます。朝に叩けばセッションが生まれ、端末を閉じたあとにもう一度
叩けば同じシェルへ戻ります。

**Windows Terminal**（PowerShell から）:

```powershell
wt new-tab --title dev berth attach dev `; split-pane -V berth attach build `; split-pane -H berth attach logs
```

⚠️ PowerShell では `;` が自分の区切りとして食われるので、`wt` へ渡すには `` `; `` と書きます
（cmd からなら素の `;` で通ります）。

**WezTerm**（WezTerm の中のシェルから）:

```sh
wezterm cli spawn -- berth attach dev
wezterm cli split-pane --right -- berth attach build
```

同じセッションは、別の端末や ssh の向こうからでも `berth attach build` で入れます。

### 環境変数

| 変数                  | 効果                                                                                                                                                                                                             |
| --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BERTHD_SOCKET`       | ソケットの場所（既定は `$XDG_RUNTIME_DIR/berthd.sock`、無ければ `~/.berth/berthd.sock`）。**明示すると `berth attach` は berthd を起こしません**（転送したソケットなど、別の接続先を指している可能性があるため） |
| `BERTHD_NO_SPAWN`     | 立てると `berth attach` が berthd を起こさなくなる。常駐させている環境で二重起動を防ぐ                                                                                                                           |
| `BERTHD_SHELL`        | セッションで使うシェル（下記の「セッションのシェルはどう決まるか」）。`berth` を実行した環境と berthd 自身の環境の**どちらでも**効きます（前者が優先）                                                                               |
| `BERTHD_JOURNAL_PATH` | journal の場所（既定 `~/.berth/journal.jsonl`）                                                                                                                                                                  |
| `BERTHD_SESSION`      | **berthd が子プロセスに渡す**自分の session id（上記）                                                                                                                                                           |
| `BERTH_SCROLLBACK`    | `off`（`0` / `no` / `false` も可）で、`berth attach` が出力を端末のスクロールに流すのをやめる（`--no-scrollback` を常に指定した状態）                                                                             |
| `AUDIT_ACTOR`         | `berth` が attach のときに名乗る名前（journal の `actor`）。管理者が付ける想定です                                                                                                                       |

## セッションのシェルはどう決まるか

`berth attach <名前>` で新しいセッションを作るとき、シェルは次の順で決まります。

| 順  | 何を見るか                                                                                       | 出所の呼び名          |
| --- | ------------------------------------------------------------------------------------------------ | --------------------- |
| 1   | `--shell <path>` / `-s <path>`（CLI 引数・最優先・**全 OS 共通**）                               | `client:cli_flag`     |
| 2   | `BERTHD_SHELL`（`berth` を実行した環境・**全 OS 共通**）                                          | `client:berthd_shell` |
| 3   | `~/.berth/config.toml` の `default-shell`（[設定ファイル](#設定ファイル)・**全 OS 共通**）        | `client:config`       |
| 4   | **そのとき走っていたシェル**（`berth` の親プロセス。`/etc/shells` に載っているシェルだけ・unix のみ）| `client:parent_shell` |
| 5   | `$SHELL`（同じく `berth` を実行した環境・unix のみ）                                               | `client:env_shell`    |
| 6   | 上のどれも無ければ berthd 側の既定（`BERTHD_SHELL` → `$SHELL` → `/etc/passwd` のログインシェル → `/bin/bash` か `/bin/sh` の在る方 / `powershell.exe`） | `daemon:…`            |

`berth info` がこの端末の答えを表示します。同じ呼び名が journal にも `shell_src` として残ります。

**4 番目が要る理由**: `bash` から `fish` と入力して fish に居るとき、`$SHELL` は `/bin/bash` のままです
（`$SHELL` は「ログインシェルに何を選んだか」の申告値で、いま走っているシェルではありません）。
そのため、**いま使っているシェルがそのままセッションのシェルになります**。

**Windows では 4 と 5 は効きません**（`/etc/shells` が無く、git-bash の `$SHELL` は
`/bin/bash.exe` という MSYS の綴りで、Windows のプロセス起動からは解決できないため）。
切り替えは `-s / --shell` オプション・`BERTHD_SHELL`・設定ファイルの `default-shell` を使ってください。

## 設定ファイル

`berth` は `~/.berth/config.toml` を読みます（journal・ペインファイルと同じ場所。どの OS でも同じ）。別のファイルを読ませるには `BERTH_CONFIG=<path>`。無くても動きます。

```toml
# 新しいセッションのシェル（毎回 `berth attach -s hsh` と打つのと同じ）
default-shell = "hsh"

# 新しく作るセッションにだけ足す環境変数
[env]
CLAUDE_CODE_SHELL = "~/.local/libexec/hsh-as-bash/bash"
```

- `default-shell` は `-s` と `BERTHD_SHELL` に負け、観測したシェルに勝ちます（[セッションのシェルはどう決まるか](#セッションのシェルはどう決まるか)）。journal には `client:config` と残ります。
- `[env]` は**セッションを作るときだけ**効きます。既存のセッションに入り直しても足しません。値の先頭の `~` / `~/` はホームに展開します。`$VAR` は展開しません。
- 読めないファイル（TOML の誤り・知らないキー・型違い）は**黙って無視しません**。`berth` が 1 行名乗って、設定が無いものとして続けます。`berth info` に場所と読めたかが出ます。

**`[env]` が要る理由**: Claude Code のようなエージェントは命令ごとに自分のシェルを起こし、どのシェルかは環境変数（`CLAUDE_CODE_SHELL`）で決めます。上の例を書いておけば、セッションの中で起こした `claude` の命令も `hsh` の上で走り、その journal に残ります。

---

## Journal

berthd は**セッションを起こしたこと**と **attach の開始・終了**を `~/.berth/journal.jsonl` に
NDJSON（[Rheon Protocol](https://github.com/rheon-protocol/spec) v1 レコード）で残します。設定は要りません。

| 行                            | いつ                     | 何が読めるか                                                                      |
| ----------------------------- | ------------------------ | --------------------------------------------------------------------------------- |
| `spawn:started`               | セッションを起こしたとき | どのシェルで起こしたか（`shell`）と、**それを誰が決めたか**（`shell_src`）、`cwd` |
| `spawn:failed`                | シェルを起こせなかったとき | 何を起こそうとしたか（`shell` / `shell_src`）と、**なぜ駄目だったか**（`reason`） |
| `attach:start` / `attach:end` | 誰かが入った / 出たとき  | 誰が入ったか、どのくらい居たか、どう抜けたか                                      |

1 行に**カーネルが観測した値**（`peer_uid` / `peer_pid` / `peer_auid` / `peer_ancestry`）と、**クライアントの自己申告**（`actor` / `actor_src`・`AUDIT_ACTOR` か `AI_AGENT` から拾う・検証しません）の両方が載ります。
人も AI エージェントも同じ uid で動くので観測だけでは足りず、名乗りだけでも足りません。並べて置けば、食い違いが 1 行で読めます。

<details>
<summary>欄の一覧</summary>

| 欄                                    | 何か                                                                                                   | 出所                   |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------ | ---------------------- |
| `peer_uid` / `peer_pid`               | 接続してきたプロセス                                                                                   | カーネル               |
| `peer_auid`                           | ログインしたときのユーザーの監査 uid（`su` や `sudo` をまたいでも変わらない・Linux / macOS）            | カーネル               |
| `peer_ancestry` / `peer_ancestry_src` | attach した瞬間の、接続してきたプロセスの親の並び（最大 8 段）と、どこまで読めたか                     | カーネル               |
| `actor` / `actor_src`                 | クライアントが名乗った名前と、その出所（`audit_actor` / `agent`）                                      | 自己申告（検証しない） |
| `shell` / `shell_src`                 | 実際に起こした実行ファイルと、それを誰が決めたか                                                       | berthd                 |
| `read_only`                           | 見るだけの接続か                                                                                       | berthd                 |
| `how`                                 | `exited`（シェルが終了）／ `detached`（外された）／ `closed`（自分で抜けた）／ `too_slow`（追いつけず） | berthd                 |

</details>

**どの道具が attach したのかを知りたいときは `peer_ancestry` を読んでください**（`actor` は誰かが選んだ名前で、選ばなければ空です）。
直の親はたいていシェルや端末（`bash`・`WindowsTerminal.exe`）で、道具はそのもう少し奥に出ます。末尾の `.exe` は大小を無視して落としてから比べてください。
親の並びは `sshd` で止めずに辿るので、ssh 越しでも「何から起動されたか」が残ります。

berthd はシェルの中で打たれたコマンドを記録しません。シェルが `BERTHD_SESSION` を自分の記録に残せば（例: hsh の `session:start`）、berthd の journal と session id で突き合わせられます。

### 権限

ソケットは `0600`、`~/.berth` は `0700`、journal は `0600` で berthd 自身が作ります（起動元の umask に
依存しません）。macOS には `XDG_RUNTIME_DIR` が無く、ソケットは `~/.berth` に置かれるため、
この締め付けがそのまま防御になります。

Windows の名前付きパイプ（`\\.\pipe\berthd-<user>`）には所有者だけの DACL が付き、`berth` は繋いだ先の
プロセスが自分のアカウントの物かを握手の前に確かめます（他のユーザーに取られていれば、相手を名指しして断ります）。
同じアカウントで走る別のプロセスからは守りません。

## ライセンス

ISC（[LICENSE](LICENSE)）。同梱しているサードパーティの表示は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にあります。
