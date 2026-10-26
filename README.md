<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/berthd-mark.svg">
    <img src="assets/berthd-mark-light.svg" width="160" alt="berthd: a pier, a bollard, and a mooring line looped around it">
  </picture>
</p>

# berthd — Shell sessions that survive closures

[日本語](README.ja.md)

**Close the window. The shell keeps running.**

Under the hood it is a small daemon that owns the PTY, so the shell's lifetime belongs to the daemon rather than to the terminal app. Like `tmux` it keeps sessions alive, but it is **headless**: it stays small by leaving panes and tabs, and all the drawing, to your terminal. It works in any terminal (Windows Terminal, Ghostty, Alacritty, …), and inside tmux panes, tabs and splits, and over SSH. Two binaries get installed: `berth` and `berthd`. It is tested on Windows, Linux and macOS.

`berth` is the CLI that manages sessions.

```bash
$ berth ls
- claude  pwsh  120x40  C:\src\app
  build   pwsh  120x40  C:\src\lib
```

---

## Installation

The installer downloads the latest release, verifies it against `SHA256SUMS`, and puts `berthd` and `berth` in `~/.local/bin` (override with `BERTHD_INSTALL_DIR`).
When updating, stop the daemon first with `berth kill-server`; the installer aborts if sessions are still running.

### Windows

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1 | iex
```

* The distribution is a `.zip` archive, so you can also open it from Explorer by double-clicking.
* **⚠️ No Windows ARM64 build yet.** To install the x64 build under emulation, pass `-Target windows-x86_64`:
  ```powershell
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1))) -Target windows-x86_64
  ```
* To keep the daemon running across logons, use a Task Scheduler logon trigger (no administrator rights needed). See [dist/README.md](dist/README.md).
* When updating from inside `berth attach`, the in-use `.exe` is moved aside first. The `*.exe.old` cannot be deleted until that process exits; the next install cleans it up.

Manual install (built-in tools only):

```powershell
curl.exe -fsSL -o berthd.zip https://github.com/takakix2/berthd/releases/latest/download/berthd-windows-x86_64.zip
tar.exe -xf berthd.zip
Move-Item berthd-*\berth*.exe $env:USERPROFILE\.local\bin\ -Force
```

### Linux

```bash
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

* The build is x86_64.
* To keep the daemon running, use the systemd user service. See [dist/README.md](dist/README.md).

Manual install:

```bash
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-linux-x86_64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

### macOS

```bash
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

* The build is Apple Silicon (arm64).
* To keep the daemon running, use the launchd LaunchAgent. See [dist/README.md](dist/README.md).

Manual install:

```bash
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-macos-arm64.tar.gz | tar xz
cd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

### On every OS

Mismatched versions of `berth` and `berthd` may misbehave. If something is off, check both versions with `berth info`.

---

## Usage

```bash
berth attach work     # Enter session 'work' (creates it if missing, spawns berthd if not running)
# … work happens …
berth d               # Detach from the session (the shell keeps running under berthd)
berth attach work     # Reconnect to the same shell
```

---

## Commands

| Command | Description |
| :--- | :--- |
| `berth attach [<name>]` (or `a`) | Enter the session in this terminal (creates one if it doesn't exist) |
| `berth attach -s <shell> [<name>]` | Create a session with a specific shell (`-s` / `--shell`) |
| `berth attach -r <name>` | View-only attach (does not send keystrokes or resize the screen; press `q` or `Ctrl-C` to exit) |
| `berth attach --no-scrollback [<name>]` | Restore only the last screen (by default the session's past output is replayed into this terminal's own scrollback first; see below) |
| `berth detach [<name>]` (or `d`) | Detach from the session (defaults to current session; disconnects all viewers including the terminal screen) |
| `berth ls` | List sessions (`*` indicates your current session) |
| `berth rename [<session>] <name>` (or `r`) | Rename a session (defaults to current session) |
| `berth kill <session>` | Close a session (along with its inner shell) |
| `berth info` | Display connection info for `berthd` (host & version) and the shell that would be spawned for a new session |
| `berth kill-server [-f]` | Stop `berthd` (requires `-f` if active sessions remain) |
| `berth --version` / `berthd --version` | Display version (does not connect to the daemon) |

`<name>` accepts either a session name (e.g., `main`) or an ID. Inside a session, `BERTHD_SESSION` holds your session ID, making shortcuts like `berth ls` (* marker) and single-argument `berth rename <name>` work smoothly.

### Reattaching Brings Back the Session's Output in Your Terminal's Own Scrollback (Default)

When you `berth attach` to an existing session, its past output (the most recent 512 KB) is replayed into **your terminal's own scrollback buffer** before the current screen is redrawn. Nothing is confined to a separate screen the way tmux does it, so mouse-wheel scrolling, search, and copy work exactly as they normally do. If the session was left inside a full-screen app (such as `vim`), the history is not replayed and only the current screen is drawn.

* **Only the last screen** (the previous behavior): `berth attach --no-scrollback <name>`
* **Always off**: `BERTH_SCROLLBACK=off`
* **Reattaching the same pane resumes only the tail:** on terminals that give each pane a stable id (Windows Terminal, WezTerm, kitty, iTerm2, Apple Terminal, Alacritty), `berth` remembers how far it already showed and replays only what is new, so the history is not appended twice. On terminals that expose no pane id, it falls back to replaying the whole 512 KB each time.

### Specifying a Shell for a New Session (`-s` / `--shell`)
You can ad-hoc specify which shell to launch when creating a new session using `-s <shell>`, `--shell <shell>`, or `--shell=<shell>`:

```bash
berth attach -s /bin/bash debug      # Launch only this session with bash
berth attach -s hsh work             # Create a new session with hsh
berth attach -s powershell.exe win   # Specify a shell on Windows
```

* Only applies when creating a new session (when it does not already exist). If attaching to an already running session, you connect to the shell currently active inside it (shells are not swapped on-the-fly).
* Works across all platforms (Linux / macOS / Windows). Takes the absolute highest priority over environment variables (`BERTHD_SHELL`) or automatic parent process detection (`client:cli_flag`).
* Can be combined with `-r` (read-only), e.g., `berth attach -s /bin/bash -r monitor`.

### Multiple Terminals Accessing the Same Session
* Only one terminal can send keystrokes at a time. If another terminal attaches to the same session via `berth attach`, the previous terminal displays `[detached from …]` and disconnects (the shell keeps running). Keystrokes from multiple terminals will never mix.
* **Any number of viewers** (`berth attach -r`). The typing terminal decides the screen size, so a smaller viewer will see full-screen apps garbled.
* If a viewer falls behind on output, only that viewer gets dropped (the typing side is never blocked).

---

## Opening Your Usual Panes All at Once

Since `berth attach <name>` creates a session if it doesn't exist and connects if it does, the layout-launching script remains identical on both first-time and subsequent runs. Run it in the morning to spin up sessions, or run it again after closing your terminal to jump right back into the same shell.

**Windows Terminal (from PowerShell):**
```powershell
wt new-tab --title dev berth attach dev `; split-pane -V berth attach build `; split-pane -H berth attach logs
```
*(⚠️ PowerShell consumes `;` as its own delimiter, so you must escape it as ``;` when passing to `wt`. Plain `;` works fine from `cmd`).*

**WezTerm (from a shell inside WezTerm):**
```bash
wezterm cli spawn -- berth attach dev
wezterm cli split-pane --right -- berth attach build
```

The exact same session can also be accessed via `berth attach build` from another terminal or over SSH.

---

## Environment Variables

| Variable | Effect |
| :--- | :--- |
| `BERTHD_SOCKET` | Path to the socket (default: `$XDG_RUNTIME_DIR/berthd.sock`, falling back to `~/.berth/berthd.sock`). Setting this prevents `berth attach` from auto-spawning a daemon (as it might point to a forwarded socket or external endpoint). |
| `BERTHD_NO_SPAWN` | When set, prevents `berth attach` from auto-spawning a daemon. Useful to prevent duplicate instances in environments where the daemon is already persistently running. |
| `BERTHD_SHELL` | Shell used for sessions (see "How the Session Shell is Decided" below). Effective in both the environment where `berth` is invoked and inside `berthd` itself (the former takes precedence). |
| `BERTHD_JOURNAL_PATH` | Path to the journal file (default: `~/.berth/journal.jsonl`). |
| `BERTHD_SESSION` | Session ID passed by `berthd` to its child processes. |
| `BERTH_SCROLLBACK` | Set to `off` (`0` / `no` / `false` also work) to stop `berth attach` from replaying output into the terminal's scrollback (same as always passing `--no-scrollback`). |
| `AUDIT_ACTOR` | Name claimed by `berth` upon attach (recorded in the journal's `actor` field). Intended for administrative labeling. |

---

## How the Session Shell is Decided

When creating a new session via `berth attach <name>`, the shell is determined in the following priority order:

| Priority | Source | Designation |
| :---: | :--- | :--- |
| 1 | `--shell <path>` / `-s <path>` (CLI argument — highest priority, cross-platform) | `client:cli_flag` |
| 2 | `BERTHD_SHELL` (environment where `berth` is invoked, cross-platform) | `client:berthd_shell` |
| 3 | `default-shell` in `~/.berth/config.toml` (see [Configuration File](#configuration-file), cross-platform) | `client:config` |
| 4 | The currently running shell (parent process of `berth`, restricted to shells listed in `/etc/shells`, Unix only) | `client:parent_shell` |
| 5 | `$SHELL` (in the environment where `berth` is invoked, Unix only) | `client:env_shell` |
| 6 | Fallback defaults on the `berthd` side (`BERTHD_SHELL` → `$SHELL` → the login shell in `/etc/passwd` → `/bin/bash` or `/bin/sh`, whichever exists / `powershell.exe`) | `daemon:…` |

`berth info` displays the resolved answer for your current environment. The same designation is preserved in the journal under `shell_src`.

**Why priority #4 is necessary:** If you run `fish` from `bash` and find yourself inside `fish`, `$SHELL` remains `/bin/bash` (because `$SHELL` declares your chosen login shell, not necessarily what's running right now). Consequently, the currently active shell is inherited for the session.

On Windows, priorities #4 and #5 do not apply (there is no `/etc/shells`, and git-bash's `$SHELL` uses the MSYS notation `/bin/bash.exe`, which cannot be resolved by Windows process spawning). Use the `-s` / `--shell` option, `BERTHD_SHELL`, or `default-shell` in the configuration file instead.

---

## Configuration File

`berth` reads `~/.berth/config.toml` (the same directory as the journal and the pane files, on every OS). Set `BERTH_CONFIG=<path>` to read a different file. The file is optional.

```toml
# The shell for new sessions (same as typing `berth attach -s hsh` every time).
default-shell = "hsh"

# Environment variables added to new sessions only.
[env]
CLAUDE_CODE_SHELL = "~/.local/libexec/hsh-as-bash/bash"
```

* `default-shell` ranks below `-s` and `BERTHD_SHELL` and above the detected shell (see [How the Session Shell is Decided](#how-the-session-shell-is-decided)). The journal records it as `client:config`.
* `[env]` applies **only when a session is created**. Reattaching an existing session does not add anything. A leading `~` or `~/` in a value expands to your home directory; `$VAR` is not expanded.
* A file that cannot be read — a TOML error, an unknown key, a wrong type — is **not silently ignored**: `berth` prints one line saying so and continues as if there were no file. `berth info` shows the path and whether it was read.

**Why `[env]`:** an AI agent such as Claude Code runs each command in its own shell, chosen by an environment variable (`CLAUDE_CODE_SHELL`). With the example above, the commands of a `claude` started inside the session run under `hsh` too, and land in its journal.

---

## Journal

`berthd` records session creations and attach starts and ends in `~/.berth/journal.jsonl` as NDJSON ([Rheon Protocol](https://github.com/rheon-protocol/spec) v1 records). No configuration is required.

| Record | When | What it tells you |
| :--- | :--- | :--- |
| `spawn:started` | A session was spawned | Which shell (`shell`), **who decided it** (`shell_src`), and `cwd` |
| `spawn:failed` | A shell could not be spawned | What was attempted (`shell` / `shell_src`) and **why it failed** (`reason`) |
| `attach:start` / `attach:end` | Someone entered / left | Who, for how long, and how they left |

Each line carries both **kernel-observed values** (`peer_uid` / `peer_pid` / `peer_auid` / `peer_ancestry`) and the **client's self-report** (`actor` / `actor_src`, taken from `AUDIT_ACTOR` or `AI_AGENT`, unverified). Humans and AI agents run under the same uid, so observation alone is not enough, and neither is a claim; side by side, a mismatch is readable in one line.

<details>
<summary>Field list</summary>

| Field | What | Source |
| :--- | :--- | :--- |
| `peer_uid` / `peer_pid` | The connecting process | Kernel |
| `peer_auid` | Audit uid of the logged-in user (survives `su` / `sudo`; Linux / macOS) | Kernel |
| `peer_ancestry` / `peer_ancestry_src` | Parent chain of the connecting process at the moment of attach (up to 8), and how far it could be read | Kernel |
| `actor` / `actor_src` | The name the client claimed and where it came from (`audit_actor` / `agent`) | Self-report (unverified) |
| `shell` / `shell_src` | The binary actually spawned and who decided it | berthd |
| `read_only` | Whether it was a view-only connection | berthd |
| `how` | `exited` (shell finished) / `detached` (taken over) / `closed` (left) / `too_slow` (viewer dropped) | berthd |

</details>

**To find out which tool attached, read `peer_ancestry`** (`actor` is a name somebody chose, and blank if nobody did). The immediate parent is usually a shell or a terminal (`bash`, `WindowsTerminal.exe`); the tool sits a little deeper. Strip a trailing `.exe`, ignoring case, before comparing. The chain does not stop at `sshd`, so "what launched this" survives SSH.

`berthd` does not record the commands typed inside the shell. If the shell records `BERTHD_SESSION` in its own log (for example `hsh`'s `session:start`), the two can be joined on the session id.

---

## Permissions
Sockets are created with `0600`, `~/.berth` with `0700`, and journals with `0600` directly by `berthd` (independent of the launching umask). macOS lacks `XDG_RUNTIME_DIR`, so sockets are placed in `~/.berth`, making these tight restrictions function as built-in security.

On Windows the named pipe (`\\.\pipe\berthd-<user>`) carries an owner-only DACL, and `berth` checks that the process on the other end of the pipe is owned by your account before it says anything; a pipe taken by another user is refused and named. It does not guard against other processes running under your own account.

---

## License
ISC (`LICENSE`). Third-party notices and attributions are listed in `THIRD-PARTY-NOTICES.md`.