# dist — berthd を常駐させる

他のマシンやモバイルの端末アプリから `ssh` で入って `berth attach` する使い方では、そのマシンで
berthd が**常駐**している必要があります。ここにはそのための service 定義を置いています。

先に `berthd` と `berth` を `~/.local/bin` に入れておきます（[README](../README.md) の「入れる」）。

## Linux（systemd user service）

```sh
mkdir -p ~/.config/systemd/user
cp dist/berthd.service ~/.config/systemd/user/
loginctl enable-linger "$USER"        # ログアウト後も動かし続ける（常駐させるなら必須）
systemctl --user daemon-reload
systemctl --user enable --now berthd
berth info                             # 動作確認
```

停止は `systemctl --user stop berthd` です（service 管理下では `berth kill-server` を使いません）。

## macOS（launchd LaunchAgent）

```sh
# plist の ProgramArguments のパス（/Users/CHANGEME）を自分の $HOME に直してから:
cp dist/com.berth.berthd.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist
berth info
```

停止は `launchctl unload ~/Library/LaunchAgents/com.berth.berthd.plist` です。

## Windows（タスクスケジューラのログオントリガ）

🚨 **`systemd` も `launchd` も無い面**なので、上の 2 つの unit は使えません
（だから Windows の配布物には `dist/` を入れていません）。代わりに**タスクスケジューラ**を使います。

PowerShell で（⭐ **管理者権限は要りません**）:

```powershell
$exe = "$env:USERPROFILE\.local\bin\berthd.exe"
$a = New-ScheduledTaskAction -Execute $exe
$t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
# ⚠️ ExecutionTimeLimit を 0 にしないと、既定の 3 日で**勝手に止められます**（daemon なので）。
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
         -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName "berthd" -Action $a -Trigger $t -Settings $s -Force
berth info                              # 動作確認
```

止めるのは `Unregister-ScheduledTask -TaskName berthd -Confirm:$false` です
（🚫 service 管理下では `berth kill-server` を使いません。下の節を見てください）。

⚠️ **Windows サービス（`sc.exe`）にはしないでください。** サービスは Session 0 で走るので、
デスクトップの `berth` から berthd に届かないことがあります。ログオントリガなら、ログオンしている
ユーザーの session で動きます。

⚠️ **ログオンしていない間は走りません**（unix の `loginctl enable-linger` に当たる設定はありません）。

## service 管理下では自動起動を切る（推奨）

berthd を service に任せるなら、その環境で **`BERTHD_NO_SPAWN=1`** を立てておくと、service が
一瞬落ちた隙に `berth attach` が**別の berthd を立てて二重になる**のを防げます。`~/.profile` などに:

```sh
export BERTHD_NO_SPAWN=1
```

service を使わず「最初の `berth attach` で立てる」運用なら、これは設定しません。

## 旧 `argod` / `argo` から乗り換える

バイナリだけ入れ替えても、unit が古い名前を起こしに行きます。unit も入れ替えてください。

```sh
# Linux (systemd --user)
systemctl --user disable --now argod          # 旧 unit を止めてから
rm -f ~/.config/systemd/user/argod.service
cp dist/berthd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now berthd

# macOS (launchd)
launchctl unload ~/Library/LaunchAgents/com.argo.argod.plist   # 旧 Label を外してから
rm -f ~/Library/LaunchAgents/com.argo.argod.plist
cp dist/com.berth.berthd.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist

# 旧バイナリを消す（installer を使った場合は消えています）
rm -f ~/.local/bin/argod ~/.local/bin/argo
```

旧バイナリは必ず消してください。`argo` / `argod` は新しい berthd と**別のソケット**を見るので、
繋がらないか、古いデーモンに繋がって「セッションが無い」と答えます。
