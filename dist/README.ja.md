# dist — berthd を常駐させる

[English](README.md)

他のマシンやモバイルの端末アプリから `ssh` で入って `berth attach` する使い方では、そのマシンで
berthd が常駐している必要があります。ここでは OS ごとの常駐のさせ方を説明します。

先に `berthd` と `berth` を `~/.local/bin` に入れておきます（[README](../README.ja.md) の「入れる」）。

## Linux（systemd user service）

```sh
mkdir -p ~/.config/systemd/user
cp dist/berthd.service ~/.config/systemd/user/
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable --now berthd
```

`loginctl enable-linger` は必須です。これが無いと、最後のログインセッションが切れたときに berthd も止まります。

止めるときは `systemctl --user stop berthd` です。

## macOS（launchd LaunchAgent）

plist のパス（`/Users/CHANGEME`）を自分のホームに置き換えながら写します。

```sh
mkdir -p ~/Library/LaunchAgents
sed "s|/Users/CHANGEME|$HOME|" dist/com.berth.berthd.plist > ~/Library/LaunchAgents/com.berth.berthd.plist
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist
```

止めるときは `launchctl unload ~/Library/LaunchAgents/com.berth.berthd.plist` です。

## Windows（タスクスケジューラのログオントリガ）

Windows には systemd も launchd も無いので、タスクスケジューラを使います。管理者権限は要りません。

```powershell
$exe = "$env:USERPROFILE\.local\bin\berthd.exe"
$a = New-ScheduledTaskAction -Execute $exe
$t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
         -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName "berthd" -Action $a -Trigger $t -Settings $s -Force
```

`-ExecutionTimeLimit` は 0（無制限）にします。既定のままだと、3 日でタスクとして止められます。

止めるときは `Unregister-ScheduledTask -TaskName berthd -Confirm:$false` です。

- Windows サービス（`sc.exe`）にはしないでください。サービスは Session 0 で走るので、デスクトップの
  `berth` から届かないことがあります。ログオントリガなら、ログオンしているユーザーの session で動きます。
- ログオンしていない間は走りません（Linux の `enable-linger` に当たる設定はありません）。

## どの OS でも

### 動いているか確かめる

```sh
berth info
```

`berth info` は berthd を起こさないので、ここで berthd の版が出れば、常駐させた berthd が動いています。

### `berth kill-server` を使わない

常駐させている間は、上の各 OS の止め方で止めてください。`berth kill-server` で止めると、
OS の側は「正常に終わった」と受け取り、立て直しません。

### 自動起動を切る（推奨）

`berth attach` は、berthd が居なければその場で起こします。常駐させているときは、常駐側が一瞬落ちた
隙に別の berthd が立って二重になるのを防ぐため、`BERTHD_NO_SPAWN` を立てておきます。

```sh
# Linux / macOS（~/.profile など）
export BERTHD_NO_SPAWN=1
```

```powershell
# Windows（ユーザーの環境変数・新しく開いたシェルから効く）
[Environment]::SetEnvironmentVariable("BERTHD_NO_SPAWN", "1", "User")
```

常駐させず「最初の `berth attach` で立てる」使い方なら、これは設定しません。
