use crate::connect_daemon;
use crate::wire::explain;
use berthd::{endpoint_display, Unreachable};

/// `berth attach` を叩いたとき、berthd が未起動ならその場で起こす（tmux が最初の `tmux` で server を
/// 立てるのと同じ）。attach でのみ呼び出す（`ls` / `info` は参照目的のみでデーモンを起動しない
/// — 未起動なら「セッション無し」）。
///
/// 起こさない場合:
/// - `BERTHD_SOCKET` を明示している場合。接続先を明示的に指定しているため（remote から forward した socket 等を
///   掴んでいる可能性があり、ローカルの berthd を勝手に起こすのは事故（遠くのセッションに
///   繋ぎたいのに手元で空の berthd が上がる）。
/// - `BERTHD_NO_SPAWN` が立っている時（明示の opt-out）。
pub(crate) async fn ensure_berthd() -> Result<(), String> {
    let sock = endpoint_display();
    // 起こすのは「走っていない」と言えるときだけ。拒否（別の資格で走っている）・話中・その他は
    // エンドポイントが存在するため、もう 1 本起動しても bind で失敗するか二重起動になるだけなので、理由を出力して終了する。
    match connect_daemon().await {
        Ok(_) => return Ok(()), // 既に居る
        Err(e) => {
            let why = Unreachable::of(e);
            if !why.means_not_running() {
                return Err(explain(&why, &sock));
            }
        }
    }
    if std::env::var_os("BERTHD_SOCKET").is_some() {
        return Err(format!(
            "berth: no berthd at {} — BERTHD_SOCKET is set, so I won't start a local one.\n\
             Start berthd at that socket yourself, or unset BERTHD_SOCKET to use the default.",
            sock
        ));
    }
    if std::env::var_os("BERTHD_NO_SPAWN").is_some() {
        return Err(format!(
            "berth: no berthd at {} (BERTHD_NO_SPAWN is set — not starting one).",
            sock
        ));
    }

    // バイナリの解決: `BERTHD_BIN` → berth の隣（存在時のみ）→ PATH の berthd。
    // 隣の候補を存在確認せず握ると、開発時の配置（berthd は別の target の下）で ENOENT になり PATH に落ちられない。
    let bin = std::env::var_os("BERTHD_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("berthd")))
                .filter(|p| p.exists())
        })
        .unwrap_or_else(|| std::path::PathBuf::from("berthd"));

    let mut command = std::process::Command::new(&bin);
    // stdio を切り離す: デーモンに端末の stdout/stderr を握らせない。握ったまま親が死ぬとパイプが壊れ、
    // berthd 内のログ出力が EPIPE になる。
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Linux: 起動元端末の cgroup から分離する（下の Windows の job object と同じ理由）。
    //
    // 素で起こすと berthd は起こした端末と同じ cgroup に入る ∴ 端末のスコープごと止められると
    // （例: systemd-oomd が端末アプリの scope を止める）、生かしていたシェルも消える。
    // tmux はシェルを `tmux-spawn-*.scope` に入れるため生き残る。これと同様に、systemd のユーザーマネージャが
    // 居れば `systemd-run --user --scope` を通して起こす（systemd-run がスコープを作って自分を移し、berthd を exec する）。
    // 居なければ（systemd の無い環境・コンテナ）直に起こす。`BERTHD_NO_SCOPE` で明示的に外せる。
    #[cfg(target_os = "linux")]
    let mut scoped: Option<std::process::Child> = None;
    #[cfg(target_os = "linux")]
    if let Some(systemd_run) = scope_runner(
        std::env::var_os("BERTHD_NO_SCOPE").is_some(),
        std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|d| std::path::Path::new(&d).join("systemd/private").exists()),
        find_in_path("systemd-run"),
    ) {
        use std::os::unix::process::CommandExt;
        let mut wrapped = std::process::Command::new(&systemd_run);
        wrapped
            .args(scope_args(bin.as_os_str(), std::process::id()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        scoped = wrapped.spawn().ok();
    }
    // berthd を自分のプロセスグループへ分離。これが無いと、この `berth attach` を Ctrl+C した時の
    // SIGINT が foreground group 宛に飛び、起動したばかりの berthd ごと終了してセッションが即死する。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        #[cfg(target_os = "linux")]
        let already = scoped.is_some();
        #[cfg(not(target_os = "linux"))]
        let already = false;
        if !already {
            command
                .spawn()
                .map_err(|e| format!("berth: could not start berthd ({}): {e}", bin.display()))?;
        }
    }
    // Windows: 呼び手の job object から出し、呼び手のコンソールにも繋がない。
    //
    // unix の `process_group(0)` と同じ箇所・同じ理由で、起動したばかりの berthd を
    // 呼び手の死に道連れにされないようにする。ただし機構が違い、軸が 2 本在る:
    //   - job の軸: sshd は ssh のセッションを `KILL_ON_JOB_CLOSE` の job に入れるので、素で起こすと
    //     ssh を閉じた瞬間に berthd ごと畳まれる。
    //   - コンソールの軸: Windows Terminal は窓を × で閉じると、そのコンソールに繋がる全プロセスに
    //     `CTRL_CLOSE_EVENT` を配信するため、breakaway が通っていてもコンソールを継承した berthd はウィンドウと一緒に終了する。
    // 旗は [`berthd::daemon_creation_flags`] に 1 つにまとめてある（検体と同じ物）。
    //
    // 注意: job の失敗パターンは 2 種類あり、片方は静かに失敗する:
    //   - 呼び手自身の job が許さない → `CreateProcess` が `WinError 5` で即失敗するため捕捉できる
    //   - 祖先の job が許さない       → spawn は成功し、あとで黙って終了するため捕捉できない
    // ∴ 「永続に成功した」とは刷らない。言えるのは「breakaway を撃った」まで。
    // 拾える方だけ名乗って、job の旗だけ降ろして撃ち直す（起こせない方が悪い）。コンソールの旗は残す。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        command.creation_flags(berthd::daemon_creation_flags(true));
        #[allow(clippy::cast_possible_wrap)]
        let denied = ERROR_ACCESS_DENIED as i32;
        if let Err(e) = command.spawn() {
            if e.raw_os_error() == Some(denied) {
                // 黙って失敗させず、何を諦めたかを出力する。
                eprintln!("berth: this terminal will not let berthd out of its job; berthd stays in it and goes down with it.");
                command.creation_flags(berthd::daemon_creation_flags(false));
                command.spawn().map_err(|e2| {
                    format!("berth: could not start berthd ({}): {e2}", bin.display())
                })?;
            } else {
                return Err(format!(
                    "berth: could not start berthd ({}): {e}",
                    bin.display()
                ));
            }
        }
    }

    // socket が上がるまで待つ（最大 ~2s）。
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if connect_daemon().await.is_ok() {
            return Ok(());
        }
        // Linux: スコープ越しの起動が失敗した（systemd-run がユーザーマネージャに届かない等）なら、
        // 黙って失敗させず、何を諦めたかを出力して直接起動し直す（Windows の breakaway と同じ作法）。
        #[cfg(target_os = "linux")]
        let failed = scoped
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .filter(|status| !status.success());
        #[cfg(target_os = "linux")]
        if let Some(status) = failed {
            scoped = None;
            eprintln!(
                "berth: could not start berthd in its own scope (systemd-run: {status}); starting it in this terminal's — it will go down if this terminal is killed."
            );
            command
                .spawn()
                .map_err(|e| format!("berth: could not start berthd ({}): {e}", bin.display()))?;
        }
    }
    Err("berth: started berthd but its socket never came up".to_string())
}

/// `systemd-run` を経由して起動するかを判定し、使用する `systemd-run` のパスを返す。
/// 判断は引数で受ける（環境を直に読むと、どの組み合わせも検体にできない）。
#[cfg(target_os = "linux")]
fn scope_runner(
    opted_out: bool,
    user_manager_up: bool,
    systemd_run: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if opted_out || !user_manager_up {
        return None;
    }
    systemd_run
}

/// `systemd-run` に渡す引数。berthd を `berthd-<berth の pid>.scope` という専用のスコープで動作させる。
/// `--collect` は、異常終了したスコープを残骸として残さないため。
#[cfg(target_os = "linux")]
fn scope_args(bin: &std::ffi::OsStr, berth_pid: u32) -> Vec<std::ffi::OsString> {
    vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        format!("--unit=berthd-{berth_pid}").into(),
        "--description=berthd (started by berth)".into(),
        "--".into(),
        bin.to_os_string(),
    ]
}

/// PATH から実行ファイルを探す（無ければ `None`）。
#[cfg(target_os = "linux")]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| {
                std::fs::metadata(p)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

#[cfg(all(test, target_os = "linux"))]
mod scope_tests {
    use super::{scope_args, scope_runner};
    use std::path::PathBuf;

    #[test]
    fn a_scope_is_used_only_with_a_user_manager_and_systemd_run() {
        let sr = Some(PathBuf::from("/usr/bin/systemd-run"));
        assert_eq!(scope_runner(false, true, sr.clone()), sr);
        assert_eq!(
            scope_runner(false, false, sr.clone()),
            None,
            "ユーザーマネージャが居ない"
        );
        assert_eq!(scope_runner(false, true, None), None, "systemd-run が無い");
        assert_eq!(scope_runner(true, true, sr), None, "BERTHD_NO_SCOPE");
    }

    #[test]
    fn the_scope_is_named_after_berth_and_runs_berthd() {
        let args = scope_args(std::ffi::OsStr::new("/tmp/x/.local/bin/berthd"), 4242);
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[..2], ["--user", "--scope"]);
        assert!(args.contains(&"--unit=berthd-4242".to_string()), "{args:?}");
        assert_eq!(args[args.len() - 2..], ["--", "/tmp/x/.local/bin/berthd"]);
    }
}
