<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/berthd-mark.svg">
    <img src="assets/berthd-mark-light.svg" width="160" alt="berthd: a pier, a bollard, and a mooring line looped around it">
  </picture>
</p>

# berthd — 閉じても消えないシェルセッション

[English](README.md)

**ウィンドウを閉じても、シェルが動き続けます。**

`berthd` は、疑似端末（PTY）を保持・管理する軽量なヘッドレスデーモンです。シェルのプロセスライフサイクルをターミナルエミュレータから切り離し、デーモン側で永続化します。tmux のようにセッションを維持しながらも、ペイン分割やタブ描画などの UI レンダリングを行わないヘッドレス設計を採用しています。Windows Terminal、Ghostty、Alacritty など任意のターミナルエミュレータ、tmux のペイン内、あるいは SSH セッション越しでもシームレスに動作します。

パッケージには、デーモン本体の `berthd` と、セッション操作用 CLI ツール `berth` の 2 つのバイナリが含まれます（Windows / Linux / macOS クロスプラットフォーム対応）。

**`berth`** がセッションを操作する CLI です。

```
$ berth ls
* claude  pwsh  120x40  C:\src\app
  build   pwsh  120x40  C:\src\lib
```

## インストール

インストーラーは最新のリリースバイナリをダウンロードして `SHA256SUMS` でチェックサムを検証し、`berthd` と `berth` を `~/.local/bin` に配置します（配置先は `BERTHD_INSTALL_DIR` で変更可能です）。

再インストール時は、事前に `berth kill-server` でデーモンを停止してください（実行中のセッションが存在する場合は停止処理が中断されます）。

### Windows

PowerShell で実行:

```powershell
irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1 | iex
```

- 配布物は `.zip` 形式です。エクスプローラー等で展開して配置することも可能です。
- **Windows ARM64 向けバイナリは未提供です。** x64 エミュレーション環境でインストールする場合は、`-Target windows-x86_64` を指定してください:
  ```powershell
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1))) -Target windows-x86_64
  ```
- 常駐化（サービス登録）の手順については [dist/README.ja.md](dist/README.ja.md) を参照してください（タスク スケジューラのログオントリガーを使用、管理者権限不要）。
- `berth attach` セッション内から再インストールを行う場合、インストーラーは実行中のバイナリを一時的にリネームして配置します（退避された `*.exe.old` はプロセス終了後の次回インストール時に自動削除されます）。

手動インストールの手順:

```powershell
curl.exe -fsSL -o berthd.zip https://github.com/takakix2/berthd/releases/latest/download/berthd-windows-x86_64.zip
tar.exe -xf berthd.zip
Move-Item berthd-*\berth*.exe $env:USERPROFILE\.local\bin\ -Force
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

- 配布アーキテクチャ: x86_64
- 常駐化（systemd user service）の手順については [dist/README.ja.md](dist/README.ja.md) を参照してください。

手動インストールの手順:

```sh
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-linux-x86_64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

- 配布アーキテクチャ: Apple Silicon（arm64）
- 常駐化（launchd LaunchAgent）の手順については [dist/README.ja.md](dist/README.ja.md) を参照してください。

手動インストールの手順:

```sh
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-macos-arm64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

### バージョンの整合性

クライアント（`berth`）とデーモン（`berthd`）のバージョンが異なるとプロトコル不一致等により動作しない場合があります。動作に問題がある場合は `berth info` を実行し、双方のバージョンを確認してください。

## 使い方

```sh
berth attach work     # セッション work にアタッチ（未作成時は新規作成、デーモン未起動時は自動起動）
# … 作業 …
berth d               # セッションからデタッチ（シェルはバックグラウンドで稼働を継続）
berth attach work     # 同一セッションに再アタッチ
```

| コマンド                                   | 説明                                                                                        |
| ------------------------------------------ | ------------------------------------------------------------------------------------------- |
| `berth attach [<name>]`（`a`）             | カレント端末でセッションにアタッチ（存在しない場合は新規作成）                              |
| `berth attach -s <shell> [<name>]`         | 指定したシェルでセッションを新規作成・アタッチ（`-s` / `--shell`）                          |
| `berth attach -r <name>`                   | 読み取り専用（read-only）でアタッチ（キー入力・サイズ同期無効、`q` または `Ctrl-C` でデタッチ） |
| `berth attach --no-scrollback [<name>]`    | スクロールバック再生をスキップし、カレント画面のみを復元（後述）                            |
| `berth detach [<name>]`（`d`）             | セッションからデタッチ（省略時はカレントセッション。閲覧中の全クライアントがデタッチ）      |
| `berth ls`                                 | セッション一覧の表示（`*` はカレントセッション）                                             |
| `berth rename [<session>] <name>`（`r`）   | セッション名のリネーム（省略時はカレントセッション）                                        |
| `berth kill <session>`                     | セッションの終了（子プロセスを含む強制終了）                                                |
| `berth info`                               | 接続先デーモン情報（ホスト名・バージョン）および新規セッション生成時に起動されるシェルの表示 |
| `berth kill-server [-f]`                   | berthd デーモンを停止（アクティブセッションが存在する場合は `-f` が必要）                   |
| `berth --version` / `berthd --version`     | バージョンを表示（デーモンには接続しません）                                                |

`<name>` にはセッション名（`main` 等）またはセッション ID を指定できます。セッション内部では環境変数 `BERTHD_SESSION` に自身のセッション ID が格納されているため、引数を省略した `berth rename <name>` や `berth detach` はカレントセッションを対象として実行されます。

### スクロールバック再生による履歴復元（デフォルト動作）

`berth attach` で再接続すると、対象セッションの直近の出力履歴（最大 512 KB）を接続先端末自身のスクロールバックバッファへ再生した上でカレント画面を描画します。tmux などの仮想画面内に閉じ込められないため、ターミナルエミュレータ本来のマウスホイールスクロール、テキスト選択・コピー、検索機能がそのまま利用可能です。なお、vim などのフルスクリーン（オルタネートスクリーン）アプリケーションの実行中にアタッチした場合は、履歴再生をスキップしてカレント画面のみを描画します。

- **カレント画面のみ復元**: `berth attach --no-scrollback <name>`
- **常時無効化**: `BERTH_SCROLLBACK=off`
- **差分スクロールバック再生**: 安定したペイン識別子を出力するターミナル（Windows Terminal、WezTerm、kitty、iTerm2、Apple Terminal、Alacritty）では、同一ペインでの再アタッチ時に未表示の差分出力のみを再生し、履歴の重複を防ぎます（識別子非対応のターミナルでは毎回直近 512 KB を再生します）。

### 起動シェルの明示指定（`-s` / `--shell`）

新規セッション作成時、起動するシェルを明示的に指定できます。`-s <shell>`、`--shell <shell>`、`--shell=<shell>` 形式に対応しています。

```sh
berth attach -s /bin/bash debug      # このセッションのみ bash で起動
berth attach -s hsh work             # hsh で新しいセッションを作成
berth attach -s powershell.exe win   # Windows でシェルを指定して作成
```

- **新規作成時のみ有効**: 既に稼働中のセッションへアタッチする場合は、そのセッションで実行中のシェルに接続します（シェルの動的差し替えは行われません）。
- **全プラットフォーム共通**: 環境変数 `BERTHD_SHELL` や親プロセスの自動検出よりも優先して適用されます（出所識別子: `client:cli_flag`）。
- `-r`（read-only）オプションと組み合わせて作成することも可能です（例: `berth attach -s /bin/bash -r monitor`）。

### マルチクライアント接続

- **排他制御（単一操作クライアント）**: セッションに対してキー入力（書き込み）を行えるクライアントは常に 1 台に制限されます。別端末から `berth attach` が行われると、先行クライアントは自動的にデタッチされ（画面に `[detached from …]` を表示）、入力の競合や混濁を防止します（セッション内のシェルはそのまま動作を継続します）。
- **読み取り専用クライアントの多重接続**: `-r` オプションによる読み取り専用アタッチは複数台から同時に接続可能です。ターミナルサイズ（PTY ウィンドウサイズ）は操作中クライアントのサイズに追従するため、閲覧側の端末サイズが小さい場合はフルスクリーンアプリケーションの表示が乱れる場合があります。
- **遅延クライアントの自動切断**: 読み取り専用クライアントのバッファ溢れ（描画遅延）が発生した場合、操作側クライアントをブロックせず、該当の読み取りクライアントのみが自動的に切断されます。

### ターミナル分割レイアウトの自動復元

`berth attach <name>` は対象セッションの有無に応じて新規作成と再接続を透過的に切り替えるため、起動スクリプトを初回作成時・再開時で共通化できます。

**Windows Terminal**（PowerShell から）:

```powershell
wt new-tab --title dev berth attach dev `; split-pane -V berth attach build `; split-pane -H berth attach logs
```

※ PowerShell ではセミコロンが構文区切り文字となるため、`wt` コマンドへ渡す際はバッククォートによるエスケープ（``` `; ```）が必要です（cmd.exe からはエスケープ不要）。

**WezTerm**（WezTerm 内のシェルから）:

```sh
wezterm cli spawn -- berth attach dev
wezterm cli split-pane --right -- berth attach build
```

作成されたセッションには、別端末や SSH 経由からでも `berth attach build` 等で接続できます。

### 環境変数

| 変数                  | 説明                                                                                                                                                                             |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BERTHD_SOCKET`       | UNIX ドメインソケットのパス（デフォルト: `$XDG_RUNTIME_DIR/berthd.sock`、未設定時は `~/.berth/berthd.sock`）。**明示的に指定した場合、`berth attach` はデーモンの自動起動を行いません**（ポート転送されたソケット等、外部接続先を指す可能性があるため）。 |
| `BERTHD_NO_SPAWN`     | `1` に設定すると `berth attach` によるデーモンの自動起動を抑止します。OS サービス管理下での常駐運用時に二重起動を防止するために使用します。                                   |
| `BERTHD_SHELL`        | 新規セッションで起動するシェル（後述の「起動シェルの決定優先順位」を参照）。クライアント実行環境とデーモン環境の双方で認識され、クライアント側が優先されます。                   |
| `BERTHD_JOURNAL_PATH` | ジャーナルファイルの出力先パス（デフォルト: `~/.berth/journal.jsonl`）。                                                                                                        |
| `BERTHD_SESSION`      | **デーモンが子プロセス（シェル）へ注入する**セッション ID。                                                                                                                      |
| `BERTH_SCROLLBACK`    | `off`（`0` / `no` / `false` も可）に設定すると、アタッチ時のスクロールバック再生を無効化します（`--no-scrollback` を常時指定した状態と同等）。                                   |
| `AUDIT_ACTOR`         | アタッチ時にクライアントが名乗る識別名（ジャーナルの `actor` 欄）。運用管理者や監査ツール等による設定を想定しています。                                                         |

## 起動シェルの決定優先順位

`berth attach <name>` で新しいセッションを作成する際、起動されるシェルは以下の優先順位に従って決定されます。

| 順位 | 判定対象                                                                                         | 出所識別子            |
| :--- | :----------------------------------------------------------------------------------------------- | :-------------------- |
| 1    | `--shell <path>` / `-s <path>`（CLI オプション・最優先・**全 OS 共通**）                         | `client:cli_flag`     |
| 2    | `BERTHD_SHELL`（`berth` 実行環境の環境変数・**全 OS 共通**）                                      | `client:berthd_shell` |
| 3    | `~/.berth/config.toml` 内の `default-shell`（[設定ファイル](#設定ファイル)・**全 OS 共通**）     | `client:config`       |
| 4    | **親プロセスのシェル**（`berth` 実行元プロセス。`/etc/shells` に登録されているシェルのみ・UNIX 系） | `client:parent_shell` |
| 5    | `$SHELL`（`berth` 実行環境の環境変数・UNIX 系）                                                  | `client:env_shell`    |
| 6    | 上記がいずれも未指定の場合のデーモン側フォールバック（`BERTHD_SHELL` → `$SHELL` → `/etc/passwd` のログインシェル → `/bin/bash` または `/bin/sh` / Windows では `powershell.exe`） | `daemon:…`            |

`berth info` を実行することで、カレント環境における判定結果を確認できます（出所識別子はジャーナルの `shell_src` にも記録されます）。

**親プロセス自動検出の背景**: `bash` からインタラクティブに `fish` や `zsh` を起動している場合でも、環境変数 `$SHELL` はログインシェル（`/bin/bash`）の値を保持し続けます。`berth` は親プロセスの実行ファイルを調べることで、現在操作中のシェルを正確に引き継いでセッションを初期化します。

**Windows 環境における制約**: Windows では `/etc/shells` が存在せず、Git Bash 等の `$SHELL` は MSYS 仮想パス（`/bin/bash.exe`）を指すため、親プロセス検出（4）および `$SHELL` 参照（5）は無効化されています。シェルの指定には `-s / --shell`、環境変数 `BERTHD_SHELL`、または設定ファイルの `default-shell` を使用してください。

## 設定ファイル

`berth` は `~/.berth/config.toml` を読み込みます（ジャーナルやペイン管理ファイルと同階層、全 OS 共通）。別のパスを指定する場合は `BERTH_CONFIG=<path>` を設定します。設定ファイルが存在しない場合でもデフォルト設定で動作します。

```toml
# 新規セッションで起動するデフォルトシェル（berth attach -s hsh を毎回指定するのと同等）
default-shell = "hsh"

# 新規セッション生成時に注入する環境変数
[env]
CLAUDE_CODE_SHELL = "~/.local/libexec/hsh-as-bash/bash"
```

- `default-shell`: CLI オプション（`-s`）および環境変数 `BERTHD_SHELL` に次ぐ優先度を持ち、親プロセスの自動検出値に優先します（ジャーナルには `client:config` と記録されます）。
- `[env]`: セッション新規作成時のみ環境変数を注入します（既存セッションへの再アタッチ時には注入されません）。値のプレフィックス `~` / `~/` はホームディレクトリパスへ展開されます（`$VAR` 形式の環境変数展開は非対応）。
- **設定エラー時の挙動**: TOML 構文エラーや未知のキーが存在する場合、警告メッセージを出力した上で設定ファイルが存在しないものとして処理を継続します（読み込み成否およびパスは `berth info` で確認可能です）。

**ユースケース（AI コーディングエージェント連携）**: Claude Code 等のエージェントツールは環境変数（`CLAUDE_CODE_SHELL` 等）で指定されたシェルをサブプロセスとして起動します。`[env]` に設定を記述しておくことで、セッション内で実行されるエージェントのシェルを監査対象シェル（hsh 等）へ統一できます。

---

## ジャーナル（監査ログ）

`berthd` は、セッションの生成・終了およびアタッチ／デタッチのイベントを `~/.berth/journal.jsonl` に NDJSON（[Rheon Protocol](https://github.com/rheon-protocol/spec) v1 レコード）で永続記録します（追加設定不要）。

| イベント種別                  | 記録タイミング             | 主な記録内容                                                                       |
| ----------------------------- | -------------------------- | ---------------------------------------------------------------------------------- |
| `spawn:started`               | セッション生成時           | 起動シェル（`shell`）、シェルの決定元（`shell_src`）、作業ディレクトリ（`cwd`）    |
| `spawn:failed`                | シェル起動失敗時           | 起動試行シェル（`shell` / `shell_src`）、失敗理由（`reason`）                      |
| `attach:start` / `attach:end` | クライアント接続／切断時   | 接続プロセス情報、滞在時間、切断要因（`how`）                                      |

各レコードには、**カーネルが観測したプロセス属性**（`peer_uid`, `peer_pid`, `peer_auid`, `peer_ancestry`）と、**クライアントの自己申告値**（`actor`, `actor_src`・`AUDIT_ACTOR` または `AI_AGENT` から取得、未検証）の双方が記録されます。同一 UID 上で人間と AI エージェントが並行動作する環境において、OS レベルの客観的証跡とアプリケーション申告値を照合・検証できます。

<details>
<summary>フィールド一覧</summary>

| フィールド                            | 説明                                                                                                   | 出所                   |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------ | ---------------------- |
| `peer_uid` / `peer_pid`               | 接続クライアントの UID / PID                                                                           | カーネル観測           |
| `peer_auid`                           | ログイン時の監査 UID（`su` や `sudo` 実行後も不変・Linux / macOS）                                     | カーネル観測           |
| `peer_ancestry` / `peer_ancestry_src` | アタッチ時点における接続元プロセスの祖先プロセスツリー（最大 8 階層）および取得ステータス              | カーネル観測           |
| `actor` / `actor_src`                 | クライアント申告名およびその出所（`audit_actor` / `agent`）                                            | 自己申告（未検証）     |
| `shell` / `shell_src`                 | 実際に起動した実行ファイルパスおよび決定元                                                             | berthd                 |
| `read_only`                           | 読み取り専用接続フラグ                                                                                 | berthd                 |
| `how`                                 | 切断要因: `exited`（シェル終了）/ `detached`（他クライアントによる追い出し）/ `closed`（正常デタッチ）/ `too_slow`（描画遅延による切断） | berthd                 |

</details>

**アタッチ元ツールの追跡**: アタッチ元のツールやプロセスを特定する場合は `peer_ancestry` を参照します（`actor` は明示的に指定しない限り空となります）。直接の親プロセスは通常シェルやターミナル（`bash`、`WindowsTerminal.exe` 等）となり、呼び出し元のツールはその上位階層に現れます（Windows の `.exe` 拡張子は大小文字を無視して比較してください）。プロセスツリーは `sshd` を越えて追跡されるため、SSH 経由の接続でも起動元プロセスを特定できます。

なお、`berthd` はシェル内部で実行された個別コマンド自体は記録しません。シェル側で環境変数 `BERTHD_SESSION` を自身のログに記録することで（例: hsh の `session:start`）、`berthd` のジャーナルとシェル監査ログをセッション ID をキーとして相互照合できます。

### パーミッションとセキュリティモデル

UNIX ドメインソケットはパーミッション `0600`、ディレクトリ `~/.berth` は `0700`、ジャーナルファイルは `0600` で `berthd` 自身によって作成されます（プロセスの umask に依存しません）。macOS では `XDG_RUNTIME_DIR` が存在せずソケットが `~/.berth` 配下に配置されますが、ディレクトリパーミッションによって保護されます。

Windows の名前付きパイプ（`\\.\pipe\berthd-<user>`）には所有者アカウントのみを許可する DACL が設定され、`berth` は接続先プロセスの SID が自身のアカウントと一致するかをハンドシェイク前に検証します（別ユーザーによるパイプの横取りを防止します）。同一アカウント権限で動作する悪意ある別プロセスからの保護は対象外です。

## ライセンス

ISC ライセンス（[LICENSE](LICENSE)）。サードパーティ製ライブラリのライセンス表記は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) を参照してください。
