# berthd — Shell sessions that survive closures

[日本語](README.ja.md)

`berthd` is a lightweight daemon that owns a PTY. By decoupling the shell's lifespan from your terminal emulator app, your shell keeps running even if you close the app, allowing you to reconnect to the exact same session—complete with its scrollback buffer—later. 

Like `tmux`, it provides session persistence, but it is **headless**. It does no rendering; terminal emulation is entirely handled by your client, so you can use whatever terminal you prefer.

The lack of a UI is intentional. Window splitting and tabs (multiplexing) are the job of your terminal app, not `berthd`. Instead, you can create as many sessions as you like—open two panes, run `berth attach a` and `berth attach b` in each, and both will keep running even after you close their respective terminals. Because it has no UI, it can live inside any pane of any terminal (tmux panes, terminal app tabs/splits, or over SSH).

The companion CLI `berth` is used to manage sessions and works from any shell.

```bash
$ berth ls
- mywork  zsh   106x29  ~/src/app
  build   zsh   120x40  ~/src/lib
```

---

## Installation

### Linux / macOS
```bash
curl -fsSL https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.sh | sh
```

### Windows (PowerShell)
```powershell
irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1 | iex
```

* **No prerequisites required:** Windows 10+ includes `curl` and `tar` out of the box, so no extraction software is needed. Since the distribution is a `.zip` archive, you can even open it directly from Explorer by double-clicking.
* **⚠️ Do not run `install.sh` from `git-bash`:** There is only one proper entry point per environment. On Windows, use the PowerShell entry point above.
* **⚠️ ARM64 builds are not yet distributed.** While x64 builds will run via emulation, they are slower, so the installer will refuse to proceed automatically. If you want to force it anyway, run the installer with the target argument:
  ```powershell
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1))) -Target windows-x86_64
  ```
* **For daemon persistence on Windows:** Since Windows lacks `systemd` or `launchd`, use Task Scheduler's logon trigger (no administrator privileges required). Instructions can be found in `dist/README.md`.
* 🚨 **When updating an in-use binary via `berth attach`:** Make sure to move the active `.exe` aside before replacing it. The moved `*.exe.old` cannot be deleted until that process terminates, but the next installation will automatically clean it up.

The installer performs the following steps:
1. Downloads the latest release and verifies it with `SHA256SUMS`.
2. Installs `berthd` and `berth` to `~/.local/bin` (can be overridden with `BERTHD_INSTALL_DIR`).
3. **Stop the daemon** using `berth kill-server` before installing. The installation will abort if active sessions remain.

### Manual Installation

**Linux / macOS:**
```bash
curl -fsSL https://github.com/takakix2/berthd/releases/latest/download/berthd-linux-x86_64.tar.gz | tar xzcd berthd-*/ && install -m755 berthd berth ~/.local/bin/
```

**Windows (using built-in tools):**
```powershell
curl.exe -fsSL -o berthd.zip https://github.com/takakix2/berthd/releases/latest/download/berthd-windows-x86_64.zip
tar.exe -xf berthd.zip
Move-Item berthd-*\berth*.exe $env:USERPROFILE\.local\bin\ -Force
```

* Distributed binaries include Linux x86_64 and macOS arm64 (`berthd-macos-arm64.tar.gz`).
* The extraction target is a directory (it won't drop `berthd` directly into the current directory).
* Mismatched versions of `berth` and `berthd` may cause issues. If things aren't working as expected, verify both versions using `berth info`.

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
| `berth detach [<name>]` (or `d`) | Detach from the session (defaults to current session; disconnects all viewers including the terminal screen) |
| `berth ls` | List sessions (`*` indicates your current session) |
| `berth rename [<session>] <name>` (or `r`) | Rename a session (defaults to current session) |
| `berth kill <session>` | Close a session (along with its inner shell) |
| `berth info` | Display connection info for `berthd` (host & version) and the shell that would be spawned for a new session |
| `berth kill-server [-f]` | Stop `berthd` (requires `-f` if active sessions remain) |
| `berth --version` / `berthd --version` | Display version (does not connect to the daemon) |

`<name>` accepts either a session name (e.g., `main`) or an ID. Inside a session, `BERTHD_SESSION` holds your session ID, making shortcuts like `berth ls` (* marker) and single-argument `berth rename <name>` work smoothly.

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
* A read-only option is available (`berth attach -r`). Any number of terminals can attach in read-only mode. Read-only clients do not alter screen dimensions; since screen size is determined by the active typing terminal, smaller read-only screens may experience visual glitches with full-screen apps like `vim` or `htop`.
* If a viewer falls behind on output, only that viewer gets dropped (the typing side is never blocked).
* `-r` requires `berthd` 0.3.0 or later. `berth` will refuse to connect to older daemons (use `berth kill-server` to restart/upgrade).

---

## Opening Your Usual Panes All at Once

`berthd` was originally built as a landing zone for the iOS/desktop terminal app `argotty`. Because it lacks a UI, it could embrace any incoming terminal—leaving layout arrangements to the terminal while `berthd` solely retains session lifespans.

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

## Daemon Lifespan
* `berthd` never terminates on its own (there is no idle timeout). It must be explicitly stopped via `berth kill-server`.
* If you want `berthd` to run as a persistent background service, `systemd` / `launchd` configurations are available in `dist/README.md`.

---

## Environment Variables

| Variable | Effect |
| :--- | :--- |
| `BERTHD_SOCKET` | Path to the socket (default: `$XDG_RUNTIME_DIR/berthd.sock`, falling back to `~/.berth/berthd.sock`). Setting this prevents `berth attach` from auto-spawning a daemon (as it might point to a forwarded socket or external endpoint). |
| `BERTHD_NO_SPAWN` | When set, prevents `berth attach` from auto-spawning a daemon. Useful to prevent duplicate instances in environments where the daemon is already persistently running. |
| `BERTHD_BIN` | Path to the `berthd` executable invoked by `berth attach` (default: next to `berth`, then falling back to `PATH`). |
| `BERTHD_SHELL` | Shell used for sessions (see "How the Session Shell is Decided" below). Effective in both the environment where `berth` is invoked and inside `berthd` itself (the former takes precedence). |
| `BERTHD_JOURNAL_PATH` | Path to the journal file (default: `~/.berth/journal.jsonl`). |
| `BERTHD_SESSION` | Session ID passed by `berthd` to its child processes. |
| `AUDIT_ACTOR` | Name claimed by `berth` upon attach (recorded in the journal's `actor` field). Intended for administrative labeling. |

---

## How the Session Shell is Decided

When creating a new session via `berth attach <name>`, the shell is determined in the following priority order:

| Priority | Source | Designation |
| :---: | :--- | :--- |
| 1 | `--shell <path>` / `-s <path>` (CLI argument — highest priority, cross-platform) | `client:cli_flag` |
| 2 | `BERTHD_SHELL` (environment where `berth` is invoked, cross-platform) | `client:berthd_shell` |
| 3 | The currently running shell (parent process of `berth`, restricted to shells listed in `/etc/shells`, Unix only) | `client:parent_shell` |
| 4 | `$SHELL` (in the environment where `berth` is invoked, Unix only) | `client:env_shell` |
| 5 | Fallback defaults on the `berthd` side (`BERTHD_SHELL` → `$SHELL` → `/bin/bash` / `powershell.exe`) | `daemon:…` |

`berth info` displays the resolved answer for your current environment. The same designation is preserved in the journal under `shell_src`.

**Why priority #3 is necessary:** If you run `fish` from `bash` and find yourself inside `fish`, `$SHELL` remains `/bin/bash` (because `$SHELL` declares your chosen login shell, not necessarily what's running right now). Consequently, the currently active shell is inherited for the session.

On Windows, priorities #3 and #4 do not apply (there is no `/etc/shells`, and git-bash's `$SHELL` uses the MSYS notation `/bin/bash.exe`, which cannot be resolved by Windows process spawning). Use the `-s` / `--shell` option or `BERTHD_SHELL` instead.

*(Note: Up to version 0.3.x, the shell was decided solely by the daemon's environment. Because `berth` auto-spawns the daemon if missing, the environment of the terminal that first started it got baked into the daemon, meaning any subsequent `berth attach` from any terminal inherited that exact shell — similar to how launching tmux in a pane forces that daemon's `default-shell` onto everything until it exits).*

---

## Journal

`berthd` records session creations, attach starts, and exits into `~/.berth/journal.jsonl` using NDJSON ([Rheon Protocol](https://github.com/rheon-protocol/spec) v1 records). No configuration is required. Each line combines observed facts with client self-reporting.

### Event Types (`spawn` / `attach`)
* `spawn:started`: Logged when a session is spawned, including which shell was used (`shell`), who decided it (`shell_src`), and `cwd`.
* `spawn:failed`: Logged when a shell fails to spawn, including what was attempted (`shell` / `shell_src`) and why it failed (`reason`, single-line).
* `attach:start` / `attach:end`: Logged when someone enters or leaves, recording who entered, duration, and how they exited.

`spawn:started` is logged separately because listings only answer "the present" — once a session is destroyed, its original shell would otherwise be lost entirely.

### Record Fields

| Field | Source | Description |
| :--- | :--- | :--- |
| `peer_uid` / `peer_pid` | Kernel-observed | UID and PID of the connecting process |
| `peer_auid` | Kernel-observed | Audit UID of the logged-in user (persists across `su` or `sudo`, Linux / macOS) |
| `peer_ancestry` / `peer_ancestry_src` | Kernel-observed | Ancestry chain (up to 8 levels deep) of the connecting process's parent hierarchy at the moment of attach |
| `actor` / `actor_src` | Client self-reported | Name claimed by the client and its source (unverified) |
| `shell` / `shell_src` (spawn events) | Daemon-resolved | Actual executed binary and how it was decided |
| `read_only` | Daemon-resolved | Whether it was a view-only connection |
| `how` (exit events) | Daemon-resolved | `exited` (shell finished) / `detached` (disconnected because another terminal took over) / `closed` (manually left / connection closed) / `too_slow` (viewer dropped for failing to keep up with output) |

Kernel observations are necessary because both humans and AI agents operate under the same user UID, but self-reporting alone isn't enough (anyone can claim anything). Having both side-by-side lets you instantly verify consistency at a glance. Unnamed attaches leave the `actor` field blank (omission is also a fact; it is not backfilled with the OS username).

`berth` picks up its actor identity from the environment variables of the process invoking `berth attach` (empty values are ignored):
1. `AUDIT_ACTOR` — Explicit override if you want to assign a specific actor name (`actor_src` becomes `audit_actor`).
2. AI agent environment markers — Automatically populated if running under agents like `AI_AGENT` or `CLAUDE_CODE_ENTRYPOINT` (`agent`).

This setting is shell-agnostic (`export AUDIT_ACTOR=…` for bash/zsh, `set -gx AUDIT_ACTOR …` for fish; use `Environment=` in service units if running persistently). It only respects the environment where `berth attach` is run—setting it *inside* the session won't affect that attach record.

Only `actor` / `actor_src` values cannot be independently verified (they remain as reported by the client). Fields prefixed with `peer_` are kernel-audited and independent of claims.

### Tracing Which Tool Attached
🚫 **Do not rely on `actor`** — That is merely "a name chosen by someone for aggregation," and remains blank if left unspecified.
▶ **Read `peer_ancestry` instead.**

`berthd` does not inject parent process names into `actor`. Immediate parents are usually shells or terminals (`bash`, `WindowsTerminal.exe`, `pwsh.exe`, etc.), so the actual tool used for attaching sits slightly deeper. Inspect the entire ancestry chain.

⚠️ **Normalization note:** When comparing, ignore case and strip any trailing `.exe` (since the same tool might be recorded as both `claude` and `claude.exe`).

Because ancestry tracing does not stop at `sshd` or similar boundaries, "what launched this" remains fully traceable even across SSH connections.

`berthd` does not record commands typed inside the shell (following a design where byte streams are never intercepted or parsed). Command auditing is the shell's responsibility; if your shell records `BERTHD_SESSION` into its own history logs (e.g., `hsh`'s `session:start`), you can correlate `berthd`'s journal with your shell's logs via the session ID. Since the session ID indicates "descendant of that session," narrow down timestamps using the attach record's start and end times.

---

## Permissions
Sockets are created with `0600`, `~/.berth` with `0700`, and journals with `0600` directly by `berthd` (independent of the launching umask). macOS lacks `XDG_RUNTIME_DIR`, so sockets are placed in `~/.berth`, making these tight restrictions function as built-in security.

---

## Migrating from Legacy `argod` / `argo`
As of v0.2.0, the daemon is named `berthd` and the CLI is `berth`. The name `argo` conflicted with the Argo Workflows CLI, causing both to return empty lists without error when installed together. To avoid this ambiguity, no legacy compatibility alias is provided.

```bash
argo attach build      # Old
berth attach build     # New
```

* The installer automatically removes old-named binaries. If you ran it as a persistent service, update your service unit / plist files accordingly (`dist/README.md`).
* Environment variables have been renamed from `ARGOD_*` to `BERTHD_*`.
* If you have existing journal logs in `~/.argo/journal.jsonl`, manually move them over to `~/.berth/journal.jsonl`.
* `~/.argo/config.toml` is not used by `berthd` and can be left as-is.

---

## License
ISC (`LICENSE`). Third-party notices and attributions are listed in `THIRD-PARTY-NOTICES.md`.