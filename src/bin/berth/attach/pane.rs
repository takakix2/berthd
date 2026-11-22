/// 端末がペインごとに付ける ID。最初に当たった env を使う（順序固定）。
/// 出さない端末（Ghostty など）は `None` を返し、dedupe を行わない（録画全体をそのまま流す）。
/// 一度に 1 つの端末にしか居ないので、ID 自身の衝突より env 変数間の衝突を避けるため `<var>-<値>` にする。
/// 注意: 親の端末から引き継がれた変数が先に一致する場合がある（Alacritty を WezTerm から起動すると `WEZTERM_PANE` が残る）。
/// それは順番では塞がず、席（[`pane_seat`]）の裏取りで別のペインと見分ける。
fn pane_id() -> Option<String> {
    pane_id_from(|var| std::env::var_os(var).map(|v| v.to_string_lossy().into_owned()))
}

/// ペイン ID を出す端末と、その変数（見る順）。
/// Windows Terminal / WezTerm / kitty / iTerm2 / Apple Terminal / Alacritty（窓ごとに 1 つ）。
const PANE_ID_VARS: &[&str] = &[
    "WT_SESSION",
    "WEZTERM_PANE",
    "KITTY_WINDOW_ID",
    "ITERM_SESSION_ID",
    "TERM_SESSION_ID",
    "ALACRITTY_WINDOW_ID",
];

/// [`pane_id`] の内部実装。env の参照処理を差し替え可能にし、テストが実際の env に依存しないようにしている。
fn pane_id_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    for var in PANE_ID_VARS {
        if let Some(raw) = lookup(var) {
            let val = raw.trim();
            if !val.is_empty() {
                return Some(sanitize_pane_id(var, val));
            }
        }
    }
    None
}

/// ファイル名に使える形へ：`<var>-<値>` の英数と `. _ -` 以外を `_` にし、値は 128 文字で切る。
fn sanitize_pane_id(var: &str, value: &str) -> String {
    let mut s = String::with_capacity(var.len() + 1 + value.len().min(128));
    s.push_str(var);
    s.push('-');
    for c in value.chars().take(128) {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    s
}

/// ペインの「席」。ペイン ID の裏付けに用いる、子プロセスへ引き継がれない識別情報。
///
/// 重要: ペイン ID は環境変数であるため、その端末から起動したプロセスへ引き継がれる（例: Terminal.app から `open` で起動した
/// Alacritty の窓 2 つが、どちらも親の `TERM_SESSION_ID` を持つ。tmux の全ペインも同じ値を継ぐ）。
/// ID だけで引くと別の窓を同じペインと読み、2 つ目には続きしか流れず履歴が欠ける。
/// ∴ unix は stdin の tty の名前とそのセッション（`getsid(0)`）、Windows はコンソールの窓（`GetConsoleWindow`）を
/// 一緒に記録し、食い違えば読み込まない（安全側に倒して全体を流す）。同じペインで再接続するときはシェルが同じなので一致する。
/// 席が取れない（`None`）ときは dedupe しない。
#[cfg(unix)]
fn pane_seat() -> Option<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: 長さを渡した自前の buffer に書かせるだけ。成功すれば NUL 終端の C 文字列。
    let tty = unsafe {
        if libc::ttyname_r(0, buf.as_mut_ptr(), buf.len()) != 0 {
            return None;
        }
        std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    // SAFETY: 引数 0 は自プロセスを指す。失敗時は -1 を返す。
    let sid = unsafe { libc::getsid(0) };
    (sid > 0).then(|| format!("tty={tty} sid={sid}"))
}

/// 同上（Windows）。コンソールごとに異なるウィンドウハンドルを持つ（ConPTY のペインは独自の擬似ウィンドウを持つ）。
#[cfg(windows)]
fn pane_seat() -> Option<String> {
    // SAFETY: 引数なし。コンソールに繋がっていなければ null。
    let hwnd = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
    (!hwnd.is_null()).then(|| format!("console={:#x}", hwnd as usize))
}

#[cfg(not(any(unix, windows)))]
fn pane_seat() -> Option<String> {
    None
}

/// ペインごとに「前回どこまで見せたか」を覚えるファイル（`~/.berth/panes/<pane>`）の中身。
#[derive(serde::Serialize, serde::Deserialize)]
struct PaneSeen {
    session_id: String,
    seen_total: u64,
    /// [`pane_seat`]。注意: これを持たない古いファイルは一致しないため、1 回全体を流して書き直す。
    #[serde(default)]
    seat: Option<String>,
}

/// このペインの鍵と席。両方そろったときのみ dedupe を行う。
pub(super) struct Pane {
    id: String,
    seat: String,
}

pub(super) fn current_pane() -> Option<Pane> {
    Some(Pane {
        id: pane_id()?,
        seat: pane_seat()?,
    })
}

/// `~/.berth/panes`。台帳と同じ家の下。panes は client の手元の物なので home 基準でよい
/// （ssh -L 越しのリモート接続であっても、記録するのはローカルのペインに紐づく情報である）。
fn panes_dir() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join(".berth").join("panes"))
}

/// 前回このペインで表示した位置を読み込む。注意: session_id と席が今回と一致するときのみ返す
/// （別セッションを同じペインで開いたら「続き」にはならない・ID が漏れた別の窓も同じ）。
pub(super) fn read_pane_seen(pane: &Pane, session_id: &str) -> Option<u64> {
    read_pane_seen_at(&panes_dir()?.join(&pane.id), session_id, &pane.seat)
}

fn read_pane_seen_at(path: &std::path::Path, session_id: &str, seat: &str) -> Option<u64> {
    let data = std::fs::read(path).ok()?;
    let rec: PaneSeen = serde_json::from_slice(&data).ok()?;
    (rec.session_id == session_id && rec.seat.as_deref() == Some(seat)).then_some(rec.seen_total)
}

/// 離脱時に位置を記録する（tmp 経由の rename で原子的更新）。失敗は無視する。dedupe は最適化であり、
/// 必須の整合性要件ではない（書き込めなくても次回全体を流すだけで済む）。
pub(super) fn write_pane_seen(pane: &Pane, session_id: &str, seen_total: u64) {
    if let Some(dir) = panes_dir() {
        write_pane_seen_at(&dir, &pane.id, session_id, &pane.seat, seen_total);
    }
}

/// tmp 名に pid を入れて別プロセスと衝突させない。
fn write_pane_seen_at(
    dir: &std::path::Path,
    pane: &str,
    session_id: &str,
    seat: &str,
    seen_total: u64,
) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let rec = PaneSeen {
        session_id: session_id.to_string(),
        seen_total,
        seat: Some(seat.to_string()),
    };
    let Ok(buf) = serde_json::to_vec(&rec) else {
        return;
    };
    let tmp = dir.join(format!("{pane}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &buf).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, dir.join(pane));
    prune_pane_files(dir, pane, PANE_FILES_CAP);
}

/// ペインファイルを何件まで持つか。越えたら更新が古い物から消す（LRU）。
/// 使い続けているペインの物は `berth d` のたびに書き直されて新しいので、消えるのは「閉じて二度と戻らなかった
/// ペイン」のファイルのみ。万一消えても、次の attach が 1 回全体を流して書き直すだけである（dedupe は最適化であり必須ではない）。
const PANE_FILES_CAP: usize = 256;

/// `dir` のペインファイルが `cap` を越えていたら、更新の古い物から越えた分だけ消す。
/// `keep`（今書いた物）と、書きかけの `*.tmp` は数えもしないし消しもしない。失敗は黙って無視。
fn prune_pane_files(dir: &std::path::Path, keep: &str, cap: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut others: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name == keep || name.ends_with(".tmp") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        others.push((modified, e.path()));
    }
    // `keep` の 1 件ぶんを足して数える。
    let total = others.len() + 1;
    if total <= cap {
        return;
    }
    others.sort();
    for (_, path) in others.into_iter().take(total - cap) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    /// ペイン ID は `<var>-<値>`・英数と `. _ -` 以外は `_`・値は 128 文字で切る。
    #[test]
    fn a_pane_id_is_made_safe_for_a_filename() {
        use super::sanitize_pane_id;
        assert_eq!(
            sanitize_pane_id("WT_SESSION", "d3adbe3f-0000-4aaa-bbbb-ccccdddd"),
            "WT_SESSION-d3adbe3f-0000-4aaa-bbbb-ccccdddd"
        );
        assert_eq!(sanitize_pane_id("WEZTERM_PANE", "0"), "WEZTERM_PANE-0");
        // パス区切り・空白・コロンは `_` に倒す（iTerm2 の ID は `:` や `/` を含む）。
        assert_eq!(
            sanitize_pane_id("ITERM_SESSION_ID", "w0t1p2:ABCD/EF GH"),
            "ITERM_SESSION_ID-w0t1p2_ABCD_EF_GH"
        );
        // 長い値は 128 文字で切る（var の分は別）。
        let long = "a".repeat(200);
        let out = sanitize_pane_id("WT_SESSION", &long);
        assert_eq!(out.len(), "WT_SESSION-".len() + 128);
    }

    /// ペインファイルは原子的に書いて読み戻せる・session_id が違えば読まない。
    #[test]
    fn a_pane_seen_round_trips_only_for_the_same_session() {
        use super::{read_pane_seen_at, write_pane_seen_at};
        let dir = std::env::temp_dir().join(format!("berth-panes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seat = "tty=/dev/ttys003 sid=4242";
        write_pane_seen_at(&dir, "WT_SESSION-abc", "tab-1", seat, 1_048_576);
        let path = dir.join("WT_SESSION-abc");
        // 同じ session_id なら位置が返る。
        assert_eq!(read_pane_seen_at(&path, "tab-1", seat), Some(1_048_576));
        // 別の session_id（同じペインに別セッションを開いた）なら返さない。
        assert_eq!(read_pane_seen_at(&path, "tab-2", seat), None);
        // 無いファイルは None。
        assert_eq!(read_pane_seen_at(&dir.join("nope"), "tab-1", seat), None);
        // 上書きできる（最新の位置が勝つ）。
        write_pane_seen_at(&dir, "WT_SESSION-abc", "tab-1", seat, 2_000_000);
        assert_eq!(read_pane_seen_at(&path, "tab-1", seat), Some(2_000_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ペイン ID は決まった順で最初に当たった env・Alacritty（`ALACRITTY_WINDOW_ID`）も読む・空や空白だけは無いのと同じ。
    #[test]
    fn the_pane_id_comes_from_the_first_terminal_variable_that_is_set() {
        use super::{pane_id_from, PANE_ID_VARS};
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |var: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == var)
                    .map(|(_, v)| (*v).to_string())
            }
        };
        assert_eq!(pane_id_from(env(&[])), None, "何も出さない端末（Ghostty）");
        assert_eq!(
            pane_id_from(env(&[("ALACRITTY_WINDOW_ID", "3")])),
            Some("ALACRITTY_WINDOW_ID-3".into())
        );
        assert_eq!(
            pane_id_from(env(&[("WEZTERM_PANE", "7"), ("ALACRITTY_WINDOW_ID", "3")])),
            Some("WEZTERM_PANE-7".into()),
            "親から漏れた WezTerm の値が先に当たる —— 見分けるのは席"
        );
        assert_eq!(
            pane_id_from(env(&[("WT_SESSION", "  "), ("KITTY_WINDOW_ID", "12")])),
            Some("KITTY_WINDOW_ID-12".into()),
            "空白だけの値は無いのと同じ"
        );
        assert_eq!(PANE_ID_VARS.last(), Some(&"ALACRITTY_WINDOW_ID"));
    }

    /// ペインファイルは上限（256）を越えたら更新の古い物から消す。今書いた物と書きかけの `.tmp` は数えない・消さない。
    #[test]
    fn pane_files_beyond_the_cap_are_pruned_oldest_first() {
        use super::{prune_pane_files, write_pane_seen_at};
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("berth-panes-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 10 件の上限に対し 12 件を、古い順に並ぶ更新日時で置く（P-00 が一番古い）。
        let now = SystemTime::now();
        for i in 0..12u32 {
            let path = dir.join(format!("P-{i:02}"));
            std::fs::write(&path, b"{}").unwrap();
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(now - Duration::from_secs(1000 - u64::from(i)))
                .unwrap();
        }
        // 書きかけの tmp は無視される（数えも消しもしない）。
        std::fs::write(dir.join("P-99.123.tmp"), b"{").unwrap();
        // 13 件目を書くと上限 10 により古い 3 件（P-00〜P-02）が削除され、新しいファイルと P-03〜P-11 が残る。
        write_pane_seen_at(&dir, "NEW", "t1", "tty=/dev/pts/1 sid=1", 1);
        prune_pane_files(&dir, "NEW", 10);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "NEW",
                "P-03",
                "P-04",
                "P-05",
                "P-06",
                "P-07",
                "P-08",
                "P-09",
                "P-10",
                "P-11",
                "P-99.123.tmp"
            ]
        );
        // 上限の中なら何も消さない。
        prune_pane_files(&dir, "NEW", 10);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 11);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ペイン ID が引き継がれて同じでも、席（tty とセッション）が異なれば別のペインとみなし読み込まない（全体を流す）。
    /// 例: Terminal.app から `open` で起こした Alacritty の窓 2 つが同じ `TERM_SESSION_ID` を持つ。
    #[test]
    fn a_leaked_pane_id_on_another_seat_is_not_the_same_pane() {
        use super::{read_pane_seen_at, write_pane_seen_at};
        let dir = std::env::temp_dir().join(format!("berth-panes-seat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = "TERM_SESSION_ID-5B774C27-5DDA-4826-B2D8-5C045573D99F";
        write_pane_seen_at(&dir, id, "t1", "tty=/dev/ttys003 sid=100", 2213);
        let path = dir.join(id);
        assert_eq!(
            read_pane_seen_at(&path, "t1", "tty=/dev/ttys003 sid=100"),
            Some(2213)
        );
        assert_eq!(
            read_pane_seen_at(&path, "t1", "tty=/dev/ttys007 sid=200"),
            None,
            "別の窓"
        );
        assert_eq!(
            read_pane_seen_at(&path, "t1", "tty=/dev/ttys003 sid=300"),
            None,
            "tty が使い回された"
        );
        // 席を持たない古いファイルは一致させない。
        std::fs::write(&path, br#"{"session_id":"t1","seen_total":2213}"#).unwrap();
        assert_eq!(
            read_pane_seen_at(&path, "t1", "tty=/dev/ttys003 sid=100"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 席は取れる形をしている（tty に繋がっていない検体の実行では `None` もありうる）。
    #[cfg(unix)]
    #[test]
    fn the_seat_names_the_tty_and_the_session_when_there_is_one() {
        if let Some(seat) = super::pane_seat() {
            assert!(seat.starts_with("tty=/dev/"), "{seat}");
            assert!(seat.contains(" sid="), "{seat}");
        }
    }
}
