#!/bin/sh
# berthd installer —— berthd（デーモン）と berth（CLI）を **対で** 入れる。
#
# ⭐ **POSIX sh で書く。** 配り方が `curl … | sh` なので、相手のシェルを選べない
#    （macOS の `/bin/sh` / dash / busybox）。⚠️ bash 前提の書き方は使わない
#    （配列・`local`・`[[ ]]`・`${var,,}`）。
#
# 📌 **要件は 4 本**。④ は実際に配ったときに見つかった —— 走っている旧デーモンを
#    検出しながら、その出力を見ずに旧バイナリを消していた。セッションが在れば
#    「触れないまま生きている PTY」を作っていた。∴ **④を先頭に置く。**
#
#   ① 入れる前に「いま在る物」と「これから入れる物」を名乗る（`--version` の括弧＝ build hash）
#   ② 旧 `argo` / `argod` を消す（残すと旧名が動き続ける）
#   ③ 対で置く（片方だけは通さない）
#   ④ 走っているデーモンを先に畳む
#
# 🚨 **版の文字列では対を判定できない。** `argo 0.1.0 (c15f9b812)` と
#    `argod 0.1.0 (2ec5ef8d0)` のように、版が同じでも別の build が同居しうる ——
#    **見分けが付くのは括弧の中だけ**。∴ 対の判定は hash でやる。

set -eu

REPO="takakix2/berthd"
MIN_TAG="v0.2.0"   # 🚨 これより前の release は中身が `argod`/`argo`（改名前）

say()  { printf '%s\n' "$*"; }
err()  { printf '%s\n' "$*" >&2; }
die()  { err ""; err "🚨 $*"; exit 1; }

# ── 置き場 ──────────────────────────────────────────────────────
# ⭐ `~/.local/bin` に**実体**を置く（symlink ではない）。
# ⚠️ `$PATH` に無ければ**入れた後に言う**（入れる前に断ると、直してから入れ直す羽目になる）。
pick_dir() {
    if [ -n "${BERTHD_INSTALL_DIR:-}" ]; then printf '%s' "$BERTHD_INSTALL_DIR"; return; fi
    printf '%s' "${HOME}/.local/bin"
}

# ── 対象を判定 ──────────────────────────────────────────────────
# ⚠️ **配っている対象だけを名乗る。** 判定できない環境に「たぶんこれ」を当てない ——
#    ⭐ 違う環境のバイナリを入れると `Exec format error` で、原因が installer に見えない。
detect_target() {
    os=$(uname -s); arch=$(uname -m)
    case "$os" in
        Linux)  case "$arch" in x86_64|amd64) printf 'linux-x86_64' ;; *) die "この環境向けはまだ配っていません: $os/$arch" ;; esac ;;
        Darwin) case "$arch" in arm64|aarch64) printf 'macos-arm64' ;; *) die "この環境向けはまだ配っていません: $os/${arch}（Intel Mac は未配布）" ;; esac ;;
        *) die "この環境向けはまだ配っていません: ${os}（Windows は別手順）" ;;
    esac
}

# ── いま入っている物を名乗る（要件①）─────────────────────────────
# ⭐ 綴りは `<name> <semver> (<hash>)`。読む側は括弧を見るだけ。
report_current() {
    _dir=$1; _found=0
    for b in berth berthd argo argod; do
        if [ -x "$_dir/$b" ]; then
            _v=$("$_dir/$b" --version 2>/dev/null || "$_dir/$b" version 2>/dev/null || echo '(名乗らない)')
            printf '   いま: %-7s %s\n' "$b" "$_v"
            _found=1
        fi
    done
    [ "$_found" = 1 ] || say "   いま: (この置き場には何も入っていません)"
}

main() {
    dir=$(pick_dir)
    target=$(detect_target)

    say "berthd installer"
    say "   対象:   $target"
    say "   置き場: $dir"
    report_current "$dir"
    say ""

    # ── ④ 走っているデーモンを先に畳む ──────────────────────────
    # 🚨 **消してから気づいても遅い。** 旧 CLI を消した後だと、生きている PTY に
    #    触る道具が無くなる（新しい CLI は別の端点を見るので気づけもしない）。
    # ⭐ 畳む前に**セッション数を見せる**。0 でなければ**こちらから畳まない** ——
    #    人のシェルを落とす判断は installer がして良いものではない。
    for old in berth argo; do
        [ -x "$dir/$old" ] || continue
        # 🚨 **`2>/dev/null` は飾りではなく、この数え方の前提。** 版が食い違うとき
        #    CLI は skew の警告を **stderr** に 3 行出す ——
        #    ⭐ それを数えると「セッション 3 つ」に見えて、**まさにその入れ替えを拒否する**。
        if sessions=$("$dir/$old" ls 2>/dev/null); then
            n=$(printf '%s\n' "$sessions" | grep -cv '^no .* sessions$' || true)
            if [ "${n:-0}" -gt 0 ] 2>/dev/null; then
                err ""
                err "⚠️ 走っているデーモンに **$n 個のセッション**が在ります:"
                printf '%s\n' "$sessions" | sed 's/^/     /' >&2
                die "先に閉じてから入れ直してください（\`$old kill-server\` は中のシェルごと畳みます）。
   ⭐ 入れてしまうと、旧 CLI が消えて**触れないまま生きている PTY**が残ります。"
            fi
            say "   ④ 走っている旧デーモンを畳みます（セッション 0）"
            "$dir/$old" kill-server >/dev/null 2>&1 || true
        fi
    done

    # ── 最新 tag を引き、改名前なら断る ─────────────────────────
    tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
          | sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' | head -1)
    [ -n "$tag" ] || die "最新の release を引けませんでした（${REPO}）"
    say "   版:     $tag"

    # 🚨 **改名前の release を黙って入れない。** v0.1.x の tarball の中身は `argod`/`argo` で、
    #    入れると**この installer が消した名前がそのまま戻る**。⭐ 断る方が親切。
    case "$tag" in
        v0.1.*|v0.0.*) die "$tag は改名前の release です（中身は \`argod\` / \`argo\`）。
   ⭐ \`berthd\` / \`berth\` は **$MIN_TAG から**です。" ;;
    esac

    asset="berthd-${target}.tar.gz"
    base="https://github.com/$REPO/releases/download/$tag"
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT INT TERM

    say "   落とす: $asset"
    curl -fsSL "$base/$asset"      -o "$tmp/$asset"    || die "$asset を落とせませんでした（${base}）"
    curl -fsSL "$base/SHA256SUMS"  -o "$tmp/SHA256SUMS" || die "SHA256SUMS を落とせませんでした"

    # ── 検算 ────────────────────────────────────────────────────
    # ⚠️ **検算できない環境では止める**（`sha256sum` も `shasum` も無い）——
    #    ⭐ 「検算しなかった」と「検算に通った」を同じ見た目にしない。
    ( cd "$tmp" || exit 1
      if command -v sha256sum >/dev/null 2>&1; then
          grep " $asset\$" SHA256SUMS | sha256sum -c - >/dev/null
      elif command -v shasum >/dev/null 2>&1; then
          grep " $asset\$" SHA256SUMS | shasum -a 256 -c - >/dev/null
      else
          exit 42
      fi ) || {
        [ $? = 42 ] && die "sha256 を検算する道具がありません（sha256sum / shasum）。
   🚫 検算せずには入れません。"
        die "$asset の sha256 が SHA256SUMS と一致しません。"
    }
    say "   検算:   ✅ sha256 一致"

    # ── 展開（🚨 中身は 1 段深い）──────────────────────────────
    # tarball は `berthd-<版>-<対象>/` に展開される（カレントに `berthd` は出来ない）。
    # ⭐ ∴ 展開先を**当てずに探す**。
    tar xzf "$tmp/$asset" -C "$tmp"
    src=$(find "$tmp" -maxdepth 2 -type f -name berthd | head -1)
    [ -n "$src" ] || die "tarball の中に berthd が見つかりません（中身が変わった？）"
    src=$(dirname "$src")
    [ -f "$src/berth" ] || die "tarball に berth が入っていません。
   🚫 **対でしか入れません**（片方だけだとプロトコルが食い違ったまま動きます）。"

    # ── ③ 対で置く ──────────────────────────────────────────────
    mkdir -p "$dir"
    for b in berthd berth; do
        cp "$src/$b" "$dir/$b.new" && chmod 755 "$dir/$b.new" && mv -f "$dir/$b.new" "$dir/$b"
    done

    # ── ② 旧名を消す ────────────────────────────────────────────
    # 🚨 残すと**旧名が動き続ける**。しかも旧 `argo` は新 `berthd` と端点も
    #    プロトコル版も食い違うので、「動く」ではなく「**中途半端に動く**」。
    removed=""
    for old in argo argod; do
        # ⚠️ **`rm -f "$dir/$old".bak*` を直に書かない。** 🚨 zsh は未マッチの glob で
        #    **コマンド行ごと中止する**（bash の既定と違う）—— `.bak` が無いと `rm` が 1 度も走らない。
        #    ⭐ ここは `sh` で走るが、**書き方でシェル方言に依らせない**。
        for f in "$dir/$old" "$dir/$old".bak*; do
            [ -e "$f" ] || continue
            rm -f "$f"; removed="$removed $(basename "$f")"
        done
    done
    [ -z "$removed" ] || say "   ② 旧名を削除:$removed"

    # 🚨 **「消した」ではなく「消えている」を確かめる**。
    # ⭐ `rm` が走らなかった / 権限で失敗した / 別の置き場にもう 1 本在った、は
    #    どれも**同じ静けさ**になる。∴ 結果を見る。
    left=""
    for old in argo argod; do
        [ -e "$dir/$old" ] && left="$left $dir/$old"
        # ⚠️ **PATH の別の場所**に旧名が残っていないか（この置き場だけ掃除しても、
        #    別の bin ディレクトリの古い argo が勝つ環境が在りうる）。
        p=$(command -v "$old" 2>/dev/null || true)
        [ -n "$p" ] && left="$left $p"
    done
    if [ -n "$left" ]; then
        err ""
        err "🚨 旧名がまだ引けます:$left"
        err "   ⚠️ 残っていると、旧 CLI は**別の端点**（argod.sock / \\.\pipe\argod-*）を"
        err "      見に行くので、新しいデーモンに繋がらないまま「セッションが無い」と答えます。"
        err "   ▶ 手で消してから、もう一度実行してください。"
        exit 1
    fi

    # ── ① 入った物を名乗り、対であることを見せる ────────────────
    say ""
    v1=$("$dir/berthd" --version 2>/dev/null || echo '?')
    v2=$("$dir/berth"  version   2>/dev/null || echo '?')
    say "   ✅ $v1"
    say "   ✅ $v2"
    h1=$(printf '%s' "$v1" | sed -n 's/.*(\(.*\)).*/\1/p')
    h2=$(printf '%s' "$v2" | sed -n 's/.*(\(.*\)).*/\1/p')
    if [ -n "$h1" ] && [ "$h1" != "$h2" ]; then
        err "🚨 対になっていません（$h1 ≠ ${h2}）—— 入れ直してください。"
        exit 1
    fi

    # ⚠️ PATH の警告は**最後に**（入れる前に断らない）。
    case ":${PATH}:" in
        *":$dir:"*) : ;;
        *) say ""
           say "⚠️ $dir が \$PATH に在りません。シェルの設定に足してください:"
           say "      export PATH=\"$dir:\$PATH\"" ;;
    esac
    say ""
    say "   ▶ berth attach work    （berthd は要るときに自分で起きます）"
    # 📌 **`berth ls` / `berth info` は確認にならない** ——
    #    どちらも**デーモンを起こさない設計**なので、「入れ替えが済んで静かな状態」と
    #    「壊れている状態」が同じ文言を返す。⭐ 往復を見たいなら**起こす操作**が要る。
    say "      ⚠️ ls / info はデーモンを起こさないので、動作確認にはなりません。"
    say "         往復を確かめるなら attach を実行してください。"
}

main "$@"
