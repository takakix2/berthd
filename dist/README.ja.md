# dist — berthd の常駐化

[English](README.md)

SSH 経由での接続やモバイル端末からのリモート利用など、クライアントが接続していない間もセッションを維持したい環境では、OS のサービス管理機構を用いて `berthd` をバックグラウンドで常駐（デーモン化）させます。本ドキュメントでは、各 OS におけるサービス登録手順を解説します。

事前に `berthd` および `berth` を `~/.local/bin`（または PATH の通ったディレクトリ）へインストールしてください（詳細は [README](../README.ja.md) の「インストール」を参照）。

## Linux（systemd user service）

```sh
mkdir -p ~/.config/systemd/user
cp dist/berthd.service ~/.config/systemd/user/
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable --now berthd
```

- **linger の有効化**: セッション切断後もデーモンを稼働し続けるため、`loginctl enable-linger "$USER"` が必須です（未設定の場合、最後のログインセッション終了時にプロセスが停止します）。
- **停止手順**: `systemctl --user stop berthd`

## macOS（launchd LaunchAgent）

テンプレート（`dist/com.berth.berthd.plist`）内のプレースホルダー（`/Users/CHANGEME`）を自身のホームディレクトリへ展開して配置し、ロードします。

```sh
mkdir -p ~/Library/LaunchAgents
sed "s|/Users/CHANGEME|$HOME|" dist/com.berth.berthd.plist > ~/Library/LaunchAgents/com.berth.berthd.plist
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist
```

- **停止手順**: `launchctl unload ~/Library/LaunchAgents/com.berth.berthd.plist`

## Windows（タスク スケジューラのログオントリガー）

Windows 環境では、タスク スケジューラのログオントリガーを利用してユーザーセッション内でバックグラウンド起動します（管理者権限は不要です）。

```powershell
$exe = "$env:USERPROFILE\.local\bin\berthd.exe"
$a = New-ScheduledTaskAction -Execute $exe
$t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
         -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName "berthd" -Action $a -Trigger $t -Settings $s -Force
```

- **実行時間制限の解除**: タイムアウトによるプロセス停止を防ぐため、`-ExecutionTimeLimit` には `[TimeSpan]::Zero`（無制限）を指定します（デフォルト設定のままだと 72 時間でタスクが停止されます）。
- **停止・登録解除**: `Unregister-ScheduledTask -TaskName berthd -Confirm:$false`
- **Windows サービス（`sc.exe` 等）として登録しないこと**: サービスは Session 0（分離セッション）で実行されるため、ユーザーセッション（デスクトップ）の `berth` から名前付きパイプへ接続できない場合があります。ログオントリガーであれば、ログオン中ユーザーのセッション内で実行されます。
- **ログオフ時の挙動**: ユーザーがログオンしていない状態では動作しません（Linux の `enable-linger` に相当する機能はありません）。

## 共通事項

### 稼働確認

```sh
berth info
```

`berth info` はデーモンを自動起動（オンデマンド起動）しません。そのため、バージョン情報等の応答が返れば、サービスとして登録した `berthd` が正常に常駐稼働していることを確認できます。

### プロセスの停止方法（`berth kill-server` を使用しない）

サービスとして常駐させている場合は、`berth kill-server` ではなく、各 OS のサービス管理コマンド（`systemctl stop` / `launchctl unload` 等）を用いて停止してください。`berth kill-server` で終了させると、OS 側は「正常終了」と判断して再起動を行いません。

### クライアントによる自動起動の抑止（推奨）

デフォルトでは、`berth attach` は `berthd` が未起動の場合に自動でデーモンを起動します。OS のサービス機構で常駐運用する場合は、再起動などの過渡状態でプロセスが二重起動することを防ぐため、環境変数 `BERTHD_NO_SPAWN=1` の設定を推奨します。

```sh
# Linux / macOS（~/.profile や ~/.bashrc など）
export BERTHD_NO_SPAWN=1
```

```powershell
# Windows（ユーザー環境変数・新規シェルから反映）
[Environment]::SetEnvironmentVariable("BERTHD_NO_SPAWN", "1", "User")
```

（常駐運用を行わず、最初の `berth attach` 時にオンデマンド起動させる運用の場合は、この設定は不要です。）
