# berthd installer (Windows) -- installs berthd (daemon) and berth (CLI) as a pair.
#
#   irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1 | iex
#
# Nothing to install first on a stock Windows 10+: curl.exe and tar.exe (bsdtar)
# are in System32 (measured 2026-09-17), and bsdtar reads the .zip we ship.
#
# THIS FILE MUST STAY ASCII-ONLY AND WITHOUT A BOM (berthd #50, measured 2026-10-10
# on Windows PowerShell 5.1.26100):
#   - With a BOM, `irm | iex` breaks: irm keeps U+FEFF at the start of the string,
#     the parser takes it as a statement before `param(...)`, and the whole script
#     fails to parse -- nothing is installed. ([scriptblock]::Create fails the same way.)
#   - Without a BOM, 5.1 reads a saved .ps1 in the ANSI code page, so any non-ASCII
#     text (Japanese, emoji) breaks parsing when run as a file (13 errors measured).
#   - ASCII is the only encoding both paths read the same. scripts/check-install-ps1.sh
#     checks this, and publish-snapshot runs it.
#
# Same four requirements as install.sh (change one, change both):
#   1. Say what is installed now and what is about to be installed.
#   2. Remove the old names (argo / argod).
#   3. Place berthd and berth as a pair.
#   4. Stop the running daemon first -- but never if it still has sessions.
#
# Windows PowerShell 5.1 only syntax (that is what a stock Windows has):
# no ternary, no `??`, no newer -ErrorAction defaults.
#
# NEVER `exit` on failure: under `irm | iex` the script runs in the caller's session,
# and `exit` closes the user's PowerShell window before they can read the message
# (measured). Die prints the message and throws: iex returns to the prompt, and
# `powershell -File` still exits non-zero.
#
# For the same reason the body below runs inside `& { ... }`: evaluated by iex, a
# plain script body assigns straight into the caller's session, and the user is
# left with $ErrorActionPreference = "Stop" (every non-terminating error in that
# window becomes fatal from then on) plus $d, $b, $tmp, $tag and the rest.
# A child scope keeps all of it here;
# the param() values are still readable inside, and a throw still gets out.

[CmdletBinding()]
param(
    # Test hatches -- normally untouched; the default is the latest GitHub release.
    [string] $Dir     = $env:BERTHD_INSTALL_DIR,
    [string] $Archive = "",   # install a local archive (no checksum, and it says so)
    [string] $Repo    = "takakix2/berthd",
    # Name the target by hand -- exists so someone who was refused can insist.
    [string] $Target  = ""
)

& {

$ErrorActionPreference = "Stop"
$MinTag = "v0.2.0"   # releases before this ship argod / argo (pre-rename)

# Defined before anything can fail: PowerShell runs top to bottom, and a function
# called before its definition is "not recognized" (measured -- the ARM64 refusal
# below used to hit exactly that).
function Say { param($m) Write-Host $m }
function Die {
    param($m)
    Write-Host ""
    Write-Host "[!] $m"
    throw "berthd install stopped."
}

# -- Detect the target --------------------------------------------------------
#
# Only name what we ship. Never guess "probably this" (same rule as install.sh).
# This once hard-coded windows-x86_64, which silently installed x64 on ARM64.
#
# Do NOT read $env:PROCESSOR_ARCHITECTURE: it describes this *process*, and an
# emulated x64 PowerShell on ARM64 answers AMD64. RuntimeInformation::OSArchitecture
# describes the OS (works on 5.1, measured 2026-09-18; values X86 / X64 / Arm / Arm64).
if (-not $Target) {
    $osArch = "$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
    if ($osArch -eq "X64") {
        $Target = "windows-x86_64"
    } elseif ($osArch -eq "Arm64") {
        # ARM builds are for later (decided 2026-09-18). The x64 build *runs* under
        # emulation, so installing it silently gives "installed but slow" with no
        # visible cause. Say so and refuse.
        Die ("No ARM64 build is published yet.`n" +
             "    The x64 build runs under emulation, but slower.`n" +
             "    To install it anyway:`n" +
             "      & ([scriptblock]::Create((irm https://raw.githubusercontent.com/$Repo/main/scripts/install.ps1))) -Target windows-x86_64")
    } else {
        Die "No build is published for this architecture: $osArch"
    }
}

# -- Where -------------------------------------------------------------------
# The real files go in ~/.local/bin (same as install.sh).
if (-not $Dir) { $Dir = Join-Path $env:USERPROFILE ".local\bin" }

# -- 1. Say what is installed now ---------------------------------------------
function Report-Current {
    param($d)
    $found = $false
    foreach ($b in @("berth", "berthd", "argo", "argod")) {
        $p = Join-Path $d "$b.exe"
        if (Test-Path $p) {
            $v = "(does not say)"
            try { $v = (& $p --version 2>$null) } catch { }
            if (-not $v) { try { $v = (& $p version 2>$null) } catch { } }
            Say ("   now:     {0,-7} {1}" -f $b, $v)
            $found = $true
        }
    }
    if (-not $found) { Say "   now:     (nothing installed here)" }
}

Say "berthd installer (Windows)"
Say "   target:  $Target"
Say "   dir:     $Dir"
Report-Current $Dir
Say ""

# -- 4. Stop the running daemon first ------------------------------------------
# Too late once the old CLI is gone: nothing would be left to reach its live PTYs.
# Show the session count first; if it is not zero, do NOT stop it.
foreach ($old in @("berth", "argo")) {
    $cli = Join-Path $Dir "$old.exe"
    if (-not (Test-Path $cli)) { continue }
    $sessions = $null
    try { $sessions = & $cli ls 2>$null } catch { continue }
    if ($null -eq $sessions) { continue }
    $lines = @($sessions | Where-Object { $_ -notmatch "^no .* sessions$" -and $_.Trim() -ne "" })
    if ($lines.Count -gt 0) {
        Write-Host ""
        Write-Host "[!] The running daemon has $($lines.Count) session(s):"
        $lines | ForEach-Object { Write-Host "      $_" }
        Die ("Close them first, then run this again (``$old kill-server`` stops them, shells included).`n" +
             "    Installing now would remove the old CLI and leave PTYs alive that nothing can reach.")
    }
    Say "   stop:    the old daemon (0 sessions)"
    try { & $cli kill-server 2>$null | Out-Null } catch { }
}

# -- Download ----------------------------------------------------------------
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("berthd-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {

$asset = "berthd-$Target.zip"
$local = Join-Path $tmp $asset

if ($Archive) {
    # Test path. Not checksummed, and it says so.
    Say "   from:    $Archive (local file -- NOT checksummed)"
    Copy-Item $Archive $local
} else {
    $rel = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest"
    $tag = $rel.tag_name
    if (-not $tag) { Die "Could not read the latest release of $Repo." }
    Say "   version: $tag"
    # Never install a pre-rename release silently: v0.1.x ships argod / argo.
    if ($tag -like "v0.1.*" -or $tag -like "v0.0.*") {
        Die ("$tag is a pre-rename release (it ships ``argod`` / ``argo``).`n" +
             "    ``berthd`` / ``berth`` start at $MinTag.")
    }
    $base = "https://github.com/$Repo/releases/download/$tag"
    Say "   fetch:   $asset"
    try { Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile $local } catch { Die "Could not download $asset ($base)." }
    $sumsPath = Join-Path $tmp "SHA256SUMS"
    try { Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile $sumsPath } catch { Die "Could not download SHA256SUMS." }

    # -- Checksum ------------------------------------------------------------
    # If it cannot be checked, stop: "not checked" must never look like "checked".
    $want = $null
    foreach ($line in (Get-Content $sumsPath)) {
        if ($line -match ("^([0-9a-fA-F]{64})\s+[*]?" + [regex]::Escape($asset) + "$")) { $want = $Matches[1].ToLower() }
    }
    if (-not $want) { Die "SHA256SUMS has no line for $asset. Refusing to install without a checksum." }
    $got = (Get-FileHash -Algorithm SHA256 $local).Hash.ToLower()
    if ($got -ne $want) { Die "sha256 of $asset does not match SHA256SUMS.`n      expected $want`n      actual   $got" }
    Say "   sha256:  OK"
}

# -- Extract (the contents are one directory deeper) -----------------------------
# Windows' own bsdtar (System32\tar.exe) reads the zip too (measured) -- faster
# than Expand-Archive, and one tool for both.
& "$env:SystemRoot\System32\tar.exe" -xf $local -C $tmp
if ($LASTEXITCODE -ne 0) { Die "tar failed to extract the archive (rc=$LASTEXITCODE)." }
$srcExe = Get-ChildItem -Path $tmp -Recurse -Filter "berthd.exe" | Select-Object -First 1
if (-not $srcExe) { Die "berthd.exe is not in the archive (did its layout change?)." }
$src = $srcExe.DirectoryName
if (-not (Test-Path (Join-Path $src "berth.exe"))) {
    Die "berth.exe is not in the archive. Installing only as a pair (one alone would run against a mismatched protocol)."
}

# -- 3. Place the pair --------------------------------------------------------
if (-not (Test-Path $Dir)) { New-Item -ItemType Directory -Path $Dir | Out-Null }

# Sweep what the previous run moved aside (it was running then and could not be deleted).
foreach ($b in @("berthd", "berth")) {
    $o = Join-Path $Dir "$b.exe.old"
    if (Test-Path $o) { try { Remove-Item $o -Force } catch { } }
}

# A running executable cannot be overwritten through the native API (measured
# 2026-09-17): DeleteFileW / MoveFileExW -- what .NET and PowerShell use -- get
# "access denied". Renaming it aside DOES work, and placing the new one after that
# works too. The renamed one cannot be deleted while it runs, so the next run sweeps it.
# (MSYS `mv` uses Windows 10 POSIX delete, so install.sh needs none of this.)
$stale = @()
foreach ($b in @("berthd", "berth")) {
    $dst = Join-Path $Dir "$b.exe"
    $new = "$dst.new"
    Copy-Item (Join-Path $src "$b.exe") $new -Force
    $placed = $false
    # Not [System.IO.File]::Move: 5.1's .NET Framework has no overwrite overload,
    # so it fails whenever the target exists and would leave a .old every time
    # (found by a control run, 2026-09-17). Move-Item -Force uses
    # MOVEFILE_REPLACE_EXISTING, which fails only while the file is in use --
    # the only case that needs moving aside.
    try { Move-Item -LiteralPath $new -Destination $dst -Force -ErrorAction Stop; $placed = $true } catch { }
    if (-not $placed) {
        if (Test-Path $dst) {
            try { [System.IO.File]::Move($dst, "$dst.old") } catch { Die "Cannot move $b.exe aside (a process may be using it)." }
            $stale += "$dst.old"
        }
        try { [System.IO.File]::Move($new, $dst) } catch { Die "Cannot place $b.exe." }
    }
}

# -- 2. Remove the old names --------------------------------------------------
# Left behind, the old names keep running. The old argo talks to a different
# endpoint and protocol version than the new berthd, so it does not "work" --
# it half-works.
$removed = @()
foreach ($old in @("argo", "argod")) {
    Get-ChildItem -Path $Dir -Filter "$old.exe*" -ErrorAction SilentlyContinue | ForEach-Object {
        try { Remove-Item $_.FullName -Force; $removed += $_.Name } catch { }
    }
}
if ($removed.Count -gt 0) { Say ("   removed: " + ($removed -join " ") + " (old names)") }

# Check "gone", not "deleted": a delete that never ran, failed on permissions,
# or a second copy elsewhere on PATH all look equally quiet.
$left = @()
foreach ($old in @("argo", "argod")) {
    $p = Join-Path $Dir "$old.exe"
    if (Test-Path $p) { $left += $p }
    $cmd = Get-Command $old -ErrorAction SilentlyContinue
    if ($cmd) { $left += $cmd.Source }
}
if ($left.Count -gt 0) {
    Die ("The old names can still be found: " + ($left -join " ") + "`n" +
         "    The old CLI looks at a different endpoint, so it cannot reach the new daemon`n" +
         "    and answers `"no sessions`". Delete them by hand, then run this again.")
}

# Never leave leftovers silently (a moved-aside file cannot be deleted while in use).
if ($stale.Count -gt 0) {
    Say ""
    Say ("[i] Moved the old files aside (in use, so not deleted): " + ($stale -join " "))
    Say "    The next install sweeps them. Deleting them by hand is fine too."
}

# -- 1. Say what got installed, and show that it is a pair --------------------------
Say ""
$v1 = "?"; $v2 = "?"
try { $v1 = (& (Join-Path $Dir "berthd.exe") --version) } catch { }
try { $v2 = (& (Join-Path $Dir "berth.exe")  version) }   catch { }
Say "   OK  $v1"
Say "   OK  $v2"
# The same version can still be two builds -- only the hash in parentheses tells them apart.
$h1 = ""; $h2 = ""
if ("$v1" -match "[(]([^)]*)[)]") { $h1 = $Matches[1] }
if ("$v2" -match "[(]([^)]*)[)]") { $h2 = $Matches[1] }
if ($h1 -and $h1 -ne $h2) { Die "Not a pair ($h1 vs $h2). Run the installer again." }

# The PATH warning comes last (never refuse before installing).
$onPath = $false
foreach ($d in ($env:PATH -split ";")) { if ($d -and (($d.TrimEnd("\\")) -ieq $Dir.TrimEnd("\\"))) { $onPath = $true } }
if (-not $onPath) {
    Say ""
    Say "[i] $Dir is not on `$env:PATH. Add it with:"
    Say "      [Environment]::SetEnvironmentVariable('PATH', '$Dir;' + [Environment]::GetEnvironmentVariable('PATH','User'), 'User')"
    Say "    The new PATH takes effect in a newly opened terminal."
}
Say ""
Say "   next:    berth attach work    (berthd starts itself when needed)"

} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

}   # end of the child scope (see the header)
