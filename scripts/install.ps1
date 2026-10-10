# berthd installer (Windows) -- berthd (daemon) と berth (CLI) を **対で** 入れる。
#
# ⭐ **素の Windows で前提ゼロ**（📏 2026-09-17 実測）——
#    `curl.exe`（8.21.0）も `tar.exe`（bsdtar 3.8.8）も System32 に在るので、
#    git-bash も 7-Zip も要らない。∴ 配る形は unix と同じ `.tar.gz` のままでよい。
#
#   iex (irm https://raw.githubusercontent.com/takakix2/berthd/main/scripts/install.ps1).TrimStart([char]0xFEFF)
#
# 🚨 **このファイルは UTF-8 with BOM で保存すること。**
#    📏 実測: BOM 無しを `powershell -File` で撃つと、日本語の行の後で**構文エラー**になる
#    （5.1 はファイルをコードページで読む）。⭐ BOM が在れば読み違えない。
#    🚨 **その BOM が `irm | iex` を壊す**（📏 2026-10-10 実測・0.5.0 の公開後に発見）——
#       `irm` の文字列には U+FEFF が残り、`param` より前の「文」になって構文エラー。
#       ∴ 入口の 1 行は **BOM を外してから流す**（上の綴り）。素の `irm | iex` に戻さないこと。
#       ⏭ ASCII だけにして BOM を捨てる案は issue（README の 1 行を定番に戻せる）。
#
# 🚨 **要件は install.sh と同じ 4 本**（片方だけ直すと方針が割れる）:
#   ① 入れる前に「いま在る物」と「これから入れる物」を名乗る
#   ② 旧名（argo / argod）を消す
#   ③ berthd と berth を**対で**置く
#   ④ **走っているデーモンを先に止める** —— セッションが在れば**こちらから止めない**
#
# ⚠️ **Windows PowerShell 5.1 で動く綴りに限る**（素の Windows に在るのはそれ）——
#    🚫 三項演算子 / `??` / `-ErrorAction` の新しい既定に依らない。

[CmdletBinding()]
param(
    # ⭐ 試験用の逃げ道。**普段は触らない** —— 既定は GitHub の最新 release。
    [string] $Dir     = $env:BERTHD_INSTALL_DIR,
    [string] $Archive = "",   # ローカルの .tar.gz を入れる（検算は飛ばす）
    [string] $Repo    = "takakix2/berthd",
    # ⭐ 面を手で名指す逃げ道。**断られた人が押し切るため**に在る。
    [string] $Target  = ""
)

$ErrorActionPreference = "Stop"
$MinTag = "v0.2.0"   # 🚨 これより前の release は中身が argod / argo（改名前）

# ── 面を判定する ──────────────────────────────
#
# ⚠️ **配っている対象だけを名乗る。** 判定できない環境に「たぶんこれ」を当てない
#    （`install.sh` と同じ規律）。🚨 ここは一度 `windows-x86_64` を**直書きしていた**ので、
#    ARM64 の機械で撃つと**黙って x64 の物を入れていた**。
#
# 🚨 **`$env:PROCESSOR_ARCHITECTURE` で見ない。** あれは**このプロセスの姿**で、
#    ARM64 上でエミュレートされた x64 PowerShell は **`AMD64` と答える** ——
#    ⭐⭐ 計器が被検体ではなく自分を見ている形。
#    ▶ `RuntimeInformation::OSArchitecture` は **OS の姿**を返す。
# 📏 5.1 で使えることを実測（2026-09-18・PS 5.1.26100）。綴りは X86 / X64 / Arm / Arm64。
if (-not $Target) {
    $osArch = "$([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
    if ($osArch -eq "X64") {
        $Target = "windows-x86_64"
    } elseif ($osArch -eq "Arm64") {
        # 🧑 **ARM 版は後日**（2026-09-18）—— 今は作らないと決めた。
        # ⚠️ x64 版はエミュレーションで**動いてしまう**ので、黙って入れると
        #    「入ったけど遅い」になり、**原因が installer に見えない**。∴ 名乗って断る。
        Die "ARM64 向けはまだ配っていません。`n   ⚠️ x64 版はエミュレーションで動きますが、**その分遅くなります**。`n   ▶ それでよければ: -Target windows-x86_64"
    } else {
        Die "この環境向けはまだ配っていません: $osArch"
    }
}

function Say  { param($m) Write-Host $m }
function Die  { param($m) Write-Host ""; Write-Host "🚨 $m"; exit 1 }

# ── 置き場 ──────────────────────────────────────────────────────
# ⭐ `~/.local/bin` に**実体**を置く（install.sh と同じ）。
if (-not $Dir) { $Dir = Join-Path $env:USERPROFILE ".local\bin" }

# ── ① いま入っている物を名乗る ──────────────────────────────────
function Report-Current {
    param($d)
    $found = $false
    foreach ($b in @("berth", "berthd", "argo", "argod")) {
        $p = Join-Path $d "$b.exe"
        if (Test-Path $p) {
            $v = "(名乗らない)"
            try { $v = (& $p --version 2>$null) } catch { }
            if (-not $v) { try { $v = (& $p version 2>$null) } catch { } }
            Say ("   いま: {0,-7} {1}" -f $b, $v)
            $found = $true
        }
    }
    if (-not $found) { Say "   いま: (この置き場には何も入っていません)" }
}

Say "berthd installer (Windows)"
Say "   対象:   $Target"
Say "   置き場: $Dir"
Report-Current $Dir
Say ""

# ── ④ 走っているデーモンを先に止める ────────────────────────────
# 🚨 **消してから気づいても遅い。** 旧 CLI を消した後だと、生きている PTY に触る道具が無くなる。
# ⭐ 止める前に**セッション数を見せる**。0 でなければ**こちらから止めない**。
foreach ($old in @("berth", "argo")) {
    $cli = Join-Path $Dir "$old.exe"
    if (-not (Test-Path $cli)) { continue }
    $sessions = $null
    try { $sessions = & $cli ls 2>$null } catch { continue }
    if ($null -eq $sessions) { continue }
    $lines = @($sessions | Where-Object { $_ -notmatch "^no .* sessions$" -and $_.Trim() -ne "" })
    if ($lines.Count -gt 0) {
        Write-Host ""
        Write-Host "⚠️ 走っているデーモンに **$($lines.Count) 個のセッション**が在ります:"
        $lines | ForEach-Object { Write-Host "     $_" }
        Die "先に閉じてから入れ直してください（``$old kill-server`` は中のシェルごと停止します）。`n   ⭐ 入れてしまうと、旧 CLI が消えて**触れないまま生きている PTY**が残ります。"
    }
    Say "   ④ 走っている旧デーモンを停止します（セッション 0）"
    try { & $cli kill-server 2>$null | Out-Null } catch { }
}

# ── 落としてくる ────────────────────────────────────────────────
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("berthd-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {

$asset = "berthd-$Target.zip"
$local = Join-Path $tmp $asset

if ($Archive) {
    # ⭐ 試験用の経路。🚨 **検算しない**ので、そう名乗る。
    Say "   元:     $Archive（ローカル・**検算しません**）"
    Copy-Item $Archive $local
} else {
    $rel = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest"
    $tag = $rel.tag_name
    if (-not $tag) { Die "最新の release を引けませんでした（$Repo）" }
    Say "   版:     $tag"
    # 🚨 **改名前の release を黙って入れない。** v0.1.x の中身は argod / argo。
    if ($tag -like "v0.1.*" -or $tag -like "v0.0.*") {
        Die "$tag は改名前の release です（中身は ``argod`` / ``argo``）。`n   ⭐ ``berthd`` / ``berth`` は **$MinTag から**です。"
    }
    $base = "https://github.com/$Repo/releases/download/$tag"
    Say "   落とす: $asset"
    try { Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile $local } catch { Die "$asset を落とせませんでした（$base）" }
    $sumsPath = Join-Path $tmp "SHA256SUMS"
    try { Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile $sumsPath } catch { Die "SHA256SUMS を落とせませんでした" }

    # ── 検算 ────────────────────────────────────────────────────
    # ⚠️ **検算できないなら止める** —— 「検算しなかった」と「検算に通った」を同じ見た目にしない。
    $want = $null
    foreach ($line in (Get-Content $sumsPath)) {
        if ($line -match ("^([0-9a-fA-F]{64})\s+[*]?" + [regex]::Escape($asset) + "$")) { $want = $Matches[1].ToLower() }
    }
    if (-not $want) { Die "SHA256SUMS に $asset の行が在りません。`n   🚫 検算せずには入れません。" }
    $got = (Get-FileHash -Algorithm SHA256 $local).Hash.ToLower()
    if ($got -ne $want) { Die "$asset の sha256 が SHA256SUMS と一致しません。`n     期待 $want`n     実際 $got" }
    Say "   検算:   ✅ sha256 一致"
}

# ── 展開（🚨 中身は 1 段深い）────────────────────────────────────
# ⭐ Windows 同梱の bsdtar で展開する（📏 System32\tar.exe・bsdtar 3.8.8）。
# ⭐ zip も bsdtar が読む（📏 実測）—— `Expand-Archive` より早く、道具を 1 つに保てる。
& "$env:SystemRoot\System32\tar.exe" -xf $local -C $tmp
if ($LASTEXITCODE -ne 0) { Die "tar が展開に失敗しました（rc=$LASTEXITCODE）" }
$srcExe = Get-ChildItem -Path $tmp -Recurse -Filter "berthd.exe" | Select-Object -First 1
if (-not $srcExe) { Die "tarball の中に berthd.exe が見つかりません（中身が変わった？）" }
$src = $srcExe.DirectoryName
if (-not (Test-Path (Join-Path $src "berth.exe"))) {
    Die "tarball に berth.exe が入っていません。`n   🚫 **対でしか入れません**（片方だけだとプロトコルが食い違ったまま動きます）。"
}

# ── ③ 対で置く ──────────────────────────────────────────────────
if (-not (Test-Path $Dir)) { New-Item -ItemType Directory -Path $Dir | Out-Null }

# ⭐ 前回退かした古い実体を掃く（あのときは走っていて消せなかった）。
foreach ($b in @("berthd", "berth")) {
    $o = Join-Path $Dir "$b.exe.old"
    if (Test-Path $o) { try { Remove-Item $o -Force } catch { } }
}

# 🚨 **native の API では、走っている実行ファイルに被せられない**（📏 2026-09-17 実測）——
#    `DeleteFileW` / `MoveFileExW`（.NET と PowerShell はこちら）は **アクセス拒否**。
#    ⭐ だが **退かす（別名へ rename）は通る**し、その後に置くのも通る。
#    ⚠️ 退かした古い方は **走っている間は消せない** ので、次の回に掃く。
# 📌 MSYS の `mv` は Windows 10 の POSIX 削除を使うので通る ＝ **install.sh には要らない細工**。
$stale = @()
foreach ($b in @("berthd", "berth")) {
    $dst = Join-Path $Dir "$b.exe"
    $new = "$dst.new"
    Copy-Item (Join-Path $src "$b.exe") $new -Force
    $placed = $false
    # 🚨 **`[System.IO.File]::Move` を使わない** —— 5.1 の .NET Framework には
    #    上書き overload が無く、**宛先が在るだけで失敗する**。
    #    ∴ それを使うと、使っていないときでも毎回 `.old` のごみが残る
    #    （📏 2026-09-17: そうなっているのを対照で見つけた）。
    # ⭐ `Move-Item -Force` は `MOVEFILE_REPLACE_EXISTING` なので、
    #    **使用中のときだけ**失敗する —— 退かすのはそのときだけでよい。
    try { Move-Item -LiteralPath $new -Destination $dst -Force -ErrorAction Stop; $placed = $true } catch { }
    if (-not $placed) {
        if (Test-Path $dst) {
            try { [System.IO.File]::Move($dst, "$dst.old") } catch { Die "$b.exe を退かせません（使っているプロセスが在るかもしれません）" }
            $stale += "$dst.old"
        }
        try { [System.IO.File]::Move($new, $dst) } catch { Die "$b.exe を置けません" }
    }
}

# ── ② 旧名を消す ────────────────────────────────────────────────
# 🚨 残すと**旧名が動き続ける**。旧 argo は新 berthd と端点もプロトコル版も食い違うので、
#    「動く」ではなく「**中途半端に動く**」。
$removed = @()
foreach ($old in @("argo", "argod")) {
    Get-ChildItem -Path $Dir -Filter "$old.exe*" -ErrorAction SilentlyContinue | ForEach-Object {
        try { Remove-Item $_.FullName -Force; $removed += $_.Name } catch { }
    }
}
if ($removed.Count -gt 0) { Say ("   ② 旧名を削除: " + ($removed -join " ")) }

# 🚨 **「消した」ではなく「消えている」を確かめる**。
# ⭐ rm が走らなかった / 権限で失敗した / 別の置き場にもう 1 本在った、は**同じ静けさ**になる。
$left = @()
foreach ($old in @("argo", "argod")) {
    $p = Join-Path $Dir "$old.exe"
    if (Test-Path $p) { $left += $p }
    $cmd = Get-Command $old -ErrorAction SilentlyContinue
    if ($cmd) { $left += $cmd.Source }
}
if ($left.Count -gt 0) {
    Write-Host ""
    Write-Host ("🚨 旧名がまだ引けます: " + ($left -join " "))
    Write-Host "   ⚠️ 残っていると、旧 CLI は**別の端点**を見に行くので、新しいデーモンに"
    Write-Host "      繋がらないまま「セッションが無い」と答えます。"
    Write-Host "   ▶ 手で消してから、もう一度実行してください。"
    exit 1
}

# 🚨 **黙って残骸を置かない**（退かした古い実体は、使っているプロセスが終わるまで消せない）。
if ($stale.Count -gt 0) {
    Say ""
    Say ("⚠️ 古い実体を退かしました（使用中で消せないため）: " + ($stale -join " "))
    Say "   ⭐ 次に install するときに掃きます。手で消しても構いません。"
}

# ── ① 入った物を名乗り、対であることを見せる ────────────────────
Say ""
$v1 = "?"; $v2 = "?"
try { $v1 = (& (Join-Path $Dir "berthd.exe") --version) } catch { }
try { $v2 = (& (Join-Path $Dir "berth.exe")  version) }   catch { }
Say "   ✅ $v1"
Say "   ✅ $v2"
# ⭐ 版が同じでも別の build が同居しうる —— **見分けが付くのは括弧の中だけ**。∴ hash で見る。
$h1 = ""; $h2 = ""
if ("$v1" -match "[(]([^)]*)[)]") { $h1 = $Matches[1] }
if ("$v2" -match "[(]([^)]*)[)]") { $h2 = $Matches[1] }
if ($h1 -and $h1 -ne $h2) { Write-Host "🚨 対になっていません（$h1 ≠ $h2）—— 入れ直してください。"; exit 1 }

# ⚠️ PATH の警告は**最後に**（入れる前に断らない）。
$onPath = $false
foreach ($d in ($env:PATH -split ";")) { if ($d -and (($d.TrimEnd("\\")) -ieq $Dir.TrimEnd("\\"))) { $onPath = $true } }
if (-not $onPath) {
    Say ""
    Say "⚠️ $Dir が `$env:PATH に在りません。足してください:"
    Say "      [Environment]::SetEnvironmentVariable('PATH', '$Dir;' + [Environment]::GetEnvironmentVariable('PATH','User'), 'User')"
    Say "   ⚠️ 新しい PATH は**開き直した端末から**効きます。"
}
Say ""
Say "   ▶ berth attach work    （berthd は要るときに自分で起きます）"

} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
