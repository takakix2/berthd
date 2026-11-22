use crate::wire::send_ctrl;
use berthd::Control;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

/// stdin が tty か（パイプ実行では対話 attach しない）。
///
/// `std::io::IsTerminal` が面の違いを吸収する（cfg を増やさずに Windows でも同じ答えが出る）。
pub(super) fn stdin_is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

/// セッションの pty の大きさを変えてもらう（`Control::Resize`）。
pub(super) async fn send_resize<W: tokio::io::AsyncWrite + Unpin>(
    wr: &mut W,
    session_id: &str,
    cols: u16,
    rows: u16,
) {
    let resize = Control::Resize {
        session_id: session_id.to_owned(),
        cols,
        rows,
    };
    let _ = send_ctrl(wr, 0, &resize).await;
}

/// 端末サイズ（cols, rows）。取れなければ 80x24。
#[cfg(unix)]
pub(super) fn term_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

/// 端末サイズ（Windows）。`GetConsoleScreenBufferInfo` の window 矩形を使用する。
///
/// 注意: buffer の大きさではなく window の矩形を参照すること。Windows のコンソールは見えている窓より広い buffer を
/// 持てるので、buffer を返すと見えていない列までシェルに教えることになる（行が折り返さず消える）。
///
/// 取れなければ既定へ落ちる（コンソールに接続されていないパイプ実行時など）。
/// この値の唯一の読み手は `Attach` の初期サイズで、繋いだ後は `Resize` が正す。
#[cfg(not(unix))]
pub(super) fn term_size() -> (u16, u16) {
    use windows_sys::Win32::System::Console::{
        GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &raw mut info) == 0 {
            return (80, 24);
        }
        let w = info.srWindow.Right - info.srWindow.Left + 1;
        let h_ = info.srWindow.Bottom - info.srWindow.Top + 1;
        if w > 0 && h_ > 0 {
            #[allow(clippy::cast_sign_loss)]
            (w as u16, h_ as u16)
        } else {
            (80, 24)
        }
    }
}

/// termios raw モードを張り、Drop で必ず戻す（panic 経路でも端末を壊さない）。
#[cfg(unix)]
pub(super) struct RawMode(libc::termios);
#[cfg(unix)]
impl RawMode {
    pub(super) fn enter() -> Result<Self, String> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return Err("berth: tcgetattr failed".to_string());
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) != 0 {
                return Err("berth: tcsetattr failed".to_string());
            }
            Ok(RawMode(saved))
        }
    }
}
#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

/// Windows の raw mode。`SetConsoleMode` で有効化し、`Drop` で必ず元に戻す。
///
/// unix と対称ではない。termios は fd 1 つ・構造体 1 つ・`cfmakeraw` 1 発だが、
/// Windows は入力と出力が別のハンドル・別のフラグ組で、両方要る:
///
/// | ハンドル | 何をするか | 忘れるとどうなるか |
/// |---|---|---|
/// | stdin | `LINE_INPUT`/`ECHO_INPUT`/`PROCESSED_INPUT` を倒し、`VIRTUAL_TERMINAL_INPUT` を立てる | 行単位でしか届かず、打鍵が二重に見える |
/// | stdout | `VIRTUAL_TERMINAL_PROCESSING` を立てる | 重要: 接続されているのに読めない（berthd が返す ANSI がそのまま文字として出力される） |
///
/// ハンドルは保持せず、`Drop` で `GetStdHandle` を再取得する。
/// 生ポインタを構造体に持つと future が `Send` でなくなり、置き場所が縛られる。
#[cfg(not(unix))]
pub(super) struct RawMode {
    saved_in: u32,
    saved_out: u32,
}

#[cfg(not(unix))]
impl RawMode {
    pub(super) fn enter() -> Result<Self, String> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        unsafe {
            let h_in = GetStdHandle(STD_INPUT_HANDLE);
            let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut saved_in: u32 = 0;
            let mut saved_out: u32 = 0;
            // 片方だけ変更して元に戻せない状態を作らないため、先に両方読めることを確認する。
            if GetConsoleMode(h_in, &raw mut saved_in) == 0 {
                return Err("berth: stdin is not a console (cannot take it raw)".to_string());
            }
            if GetConsoleMode(h_out, &raw mut saved_out) == 0 {
                return Err("berth: stdout is not a console (cannot take it raw)".to_string());
            }
            let want_in = (saved_in
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            if SetConsoleMode(h_in, want_in) == 0 {
                return Err("berth: SetConsoleMode(stdin) failed".to_string());
            }
            if SetConsoleMode(h_out, saved_out | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                // 入った分は戻してから断る（半端な状態で降りない）。
                SetConsoleMode(h_in, saved_in);
                return Err("berth: SetConsoleMode(stdout) failed".to_string());
            }
            Ok(RawMode {
                saved_in,
                saved_out,
            })
        }
    }
}

#[cfg(not(unix))]
impl Drop for RawMode {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.saved_in);
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), self.saved_out);
        }
    }
}

/// 外部からの終了シグナル（SIGTERM / SIGHUP / SIGINT）を select の 1 本として待機する。
///
/// 重要: これが無いと、signal は既定の動作でその場でプロセスを終了させるため、`leave` も `RawMode` の `Drop` も
/// 走らず、`kill <berth>` や ssh の切断の後に端末が raw・alt screen・マウス報告・kitty キーボードのまま残る。
///
/// - SIGTERM / SIGINT → `terminated`（raw モードでは Ctrl-C は打鍵 0x03 なので、ここへ来る SIGINT は `kill` から）
/// - SIGHUP → `hangup`（端末が消えた。後始末の列は届かないことが多いが、termios は戻す）
///
/// Windows は対象外（コンソールの `CTRL_CLOSE_EVENT` は別経路）であるため、永続的に待機する。
/// 注意: `select!` の中で使用するためキャンセル安全であること（`Signal::recv` はキャンセル安全）。
pub(super) struct Quit {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
}

impl Quit {
    pub(super) fn new() -> Result<Self, String> {
        #[cfg(unix)]
        {
            let hook = |k: SignalKind| signal(k).map_err(|e| format!("berth: signal: {e}"));
            Ok(Quit {
                term: hook(SignalKind::terminate())?,
                int: hook(SignalKind::interrupt())?,
                hup: hook(SignalKind::hangup())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Quit {})
        }
    }

    /// 来るまで待ち、抜ける理由（`farewell_text` の綴り）を返す。
    pub(super) async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => "terminated",
                _ = self.int.recv() => "terminated",
                _ = self.hup.recv() => "hangup",
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<&'static str>().await
        }
    }
}

/// ウィンドウサイズの変更を検知するインタフェース（プラットフォームごとに異なる機構を同じ形で扱う）。
///
/// | 面 | 機構 |
/// |---|---|
/// | unix | `SIGWINCH`（カーネルが教えに来る）|
/// | Windows | ポーリング（`GetConsoleScreenBufferInfo` を定期的に見て差分を取る）|
///
/// Windows に `SIGWINCH` は無い。`ReadConsoleInput` の `WINDOW_BUFFER_SIZE_EVENT` を読む道も在るが、
/// stdin の読み経路ごと差し替わる（いまは `tokio::io::stdin`）うえ、VT input mode でその event が
/// 来るかを確かめていない。∴ 読み経路に触らないポーリングを採った。
/// 費用は遅れと、静かな時も回ること。250ms なら窓を掴んでいる間に追随する。
/// event が来ると分かったら、ここだけ差し替えれば済む（呼び手は形しか見ていない）。
pub(super) struct WindowChange {
    #[cfg(unix)]
    sig: tokio::signal::unix::Signal,
    #[cfg(not(unix))]
    last: (u16, u16),
    #[cfg(not(unix))]
    tick: tokio::time::Interval,
}

impl WindowChange {
    pub(super) fn new() -> Result<Self, String> {
        #[cfg(unix)]
        {
            Ok(WindowChange {
                sig: signal(SignalKind::window_change())
                    .map_err(|e| format!("berth: signal: {e}"))?,
            })
        }
        #[cfg(not(unix))]
        {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            // 取りこぼした tick をまとめて撃たない（窓の大きさは今の値だけが要る）。
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Ok(WindowChange {
                last: term_size(),
                tick,
            })
        }
    }

    /// 変わるまで待ち、新しい大きさを返す。
    ///
    /// サイズを返すのは、`SIGWINCH` がサイズ変化なしでも発火するためである。
    /// 呼び手が毎回 `term_size()` を引き直す形だと、面ごとに意味がずれる。
    /// 注意: `select!` の中で使用するためキャンセル安全であること（unix は `recv`、
    /// Windows は `tick` で、どちらも途中で落としても状態を壊さない。
    pub(super) async fn changed(&mut self) -> (u16, u16) {
        #[cfg(unix)]
        {
            self.sig.recv().await;
            term_size()
        }
        #[cfg(not(unix))]
        {
            loop {
                self.tick.tick().await;
                let now = term_size();
                if now != self.last {
                    self.last = now;
                    return now;
                }
            }
        }
    }
}

/// stdout が tty か（パイプへ流しているなら、スクロールへ書き出す意味が無い）。
pub(super) fn stdout_is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}
