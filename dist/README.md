# dist — Running berthd as a Service

[日本語](README.ja.md)

When connecting via SSH or using terminal apps from mobile devices, `berthd` must remain running in the background even when no client is currently attached. This guide explains how to register and manage `berthd` as an OS-level service on Linux, macOS, and Windows.

Make sure `berthd` and `berth` are installed in `~/.local/bin` (or another directory in your `PATH`) beforehand (see [README](../README.md)).

## Linux (systemd user service)

```sh
mkdir -p ~/.config/systemd/user
cp dist/berthd.service ~/.config/systemd/user/
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user enable --now berthd
```

- **Enable lingering**: Running `loginctl enable-linger "$USER"` is required to keep the daemon running after your SSH or login session disconnects. Without lingering, systemd terminates user processes upon logout.
- **Stopping the service**: `systemctl --user stop berthd`

## macOS (launchd LaunchAgent)

Deploy the template plist (`dist/com.berth.berthd.plist`) to your LaunchAgents directory, substituting the placeholder (`/Users/CHANGEME`) with your actual home directory, and load it:

```sh
mkdir -p ~/Library/LaunchAgents
sed "s|/Users/CHANGEME|$HOME|" dist/com.berth.berthd.plist > ~/Library/LaunchAgents/com.berth.berthd.plist
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist
```

- **Stopping the agent**: `launchctl unload ~/Library/LaunchAgents/com.berth.berthd.plist`

## Windows (Task Scheduler logon trigger)

On Windows, use Task Scheduler with a logon trigger to run `berthd` inside your user session (administrator privileges are not required).

```powershell
$exe = "$env:USERPROFILE\.local\bin\berthd.exe"
$a = New-ScheduledTaskAction -Execute $exe
$t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
         -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName "berthd" -Action $a -Trigger $t -Settings $s -Force
```

- **Execution time limit**: Set `-ExecutionTimeLimit` to `[TimeSpan]::Zero` (unlimited). By default, Task Scheduler terminates tasks after 72 hours.
- **Stopping and unregistering**: `Unregister-ScheduledTask -TaskName berthd -Confirm:$false`
- **Do not register as a Windows Service (`sc.exe`)**: Windows services run in Session 0 (session isolation), which may prevent desktop `berth` clients from accessing the named pipe. A logon trigger ensures the daemon runs inside the interactive user session.
- **Logoff behavior**: The daemon will not run while logged off (Windows has no direct equivalent to systemd lingering).

## General Operations

### Health Check

```sh
berth info
```

`berth info` never starts `berthd` on demand. If it successfully outputs version and status information, the daemon registered with your OS service manager is running.

### Stopping the Daemon (Do not use `berth kill-server`)

When running under a service manager, always stop `berthd` using your OS service command (`systemctl stop`, `launchctl unload`, etc.). Using `berth kill-server` exits with status 0, which service managers treat as a clean exit and will not restart.

### Suppressing On-Demand Spawning (Recommended)

By default, `berth attach` automatically launches `berthd` if it is not currently running. When using a service manager, set `BERTHD_NO_SPAWN=1` to prevent accidental dual instances during transient restarts.

```sh
# Linux / macOS (e.g. in ~/.profile or ~/.bashrc)
export BERTHD_NO_SPAWN=1
```

```powershell
# Windows (user environment variable; effective for new shells)
[Environment]::SetEnvironmentVariable("BERTHD_NO_SPAWN", "1", "User")
```

(If you prefer on-demand startup on the first `berth attach` rather than running a persistent service, leave this variable unset.)
