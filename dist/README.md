# dist — keep berthd running

[日本語](README.ja.md)

To `ssh` into a machine (from another computer or from a mobile terminal app) and `berth attach`,
berthd has to be running on that machine. This page shows how to keep it running on each OS.

Install `berthd` and `berth` into `~/.local/bin` first (see "Install" in the [README](../README.md)).

## Linux (systemd user service)

```sh
mkdir -p ~/.config/systemd/user
cp dist/berthd.service ~/.config/systemd/user/
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable --now berthd
```

`loginctl enable-linger` is required. Without it, berthd stops when your last login session ends.

To stop it: `systemctl --user stop berthd`.

## macOS (launchd LaunchAgent)

Copy the plist, replacing the path placeholder (`/Users/CHANGEME`) with your home directory:

```sh
mkdir -p ~/Library/LaunchAgents
sed "s|/Users/CHANGEME|$HOME|" dist/com.berth.berthd.plist > ~/Library/LaunchAgents/com.berth.berthd.plist
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist
```

To stop it: `launchctl unload ~/Library/LaunchAgents/com.berth.berthd.plist`.

## Windows (Task Scheduler logon trigger)

Windows has neither systemd nor launchd, so use Task Scheduler. No administrator rights are needed.

```powershell
$exe = "$env:USERPROFILE\.local\bin\berthd.exe"
$a = New-ScheduledTaskAction -Execute $exe
$t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
         -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName "berthd" -Action $a -Trigger $t -Settings $s -Force
```

Set `-ExecutionTimeLimit` to zero (no limit). With the default, Task Scheduler stops the task after three days.

To stop it: `Unregister-ScheduledTask -TaskName berthd -Confirm:$false`.

- Do not make it a Windows service (`sc.exe`). Services run in Session 0, where `berth` on the desktop
  may not reach them. A logon trigger runs berthd in the session of the user who is logged on.
- It does not run while you are logged off (there is no equivalent of Linux's `enable-linger`).

## On every OS

### Check that it is running

```sh
berth info
```

`berth info` never starts berthd, so if it prints a berthd version, the berthd you set up is running.

### Do not use `berth kill-server`

While berthd is kept running this way, stop it with your OS's command above. If you stop it with
`berth kill-server`, the OS sees a clean exit and does not restart it.

### Turn off auto-start (recommended)

`berth attach` starts a berthd if none is running. When berthd is kept running by the OS, set
`BERTHD_NO_SPAWN` so that a brief restart cannot leave you with a second berthd.

```sh
# Linux / macOS (e.g. in ~/.profile)
export BERTHD_NO_SPAWN=1
```

```powershell
# Windows (user environment variable; takes effect in newly opened shells)
[Environment]::SetEnvironmentVariable("BERTHD_NO_SPAWN", "1", "User")
```

If you do not keep berthd running and let the first `berth attach` start it, leave this unset.
