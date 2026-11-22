use crate::{connect_daemon, SOURCE_CLI};
use berthd::{
    control_frame, decode_control, endpoint_display, read_frame, Control, Unreachable,
    KIND_CONTROL, PROTOCOL_VERSION,
};

// ─── berthd との 1 往復 ─────────────────────────────────────
// 接続 → 制御フレームを 1 本書く → 応答を 1 本読む。ここの関数は berthd を起こさない
// （一覧を見るためだけにデーモンを起動しない）。

/// `Unreachable` をこの CLI の言葉にする。
///
/// 分類は `berthd::Unreachable`（lib）に在る（berthd 本体も同じ判定を使うので、写しを持たない）。
/// ここに在るのは助言のみで、`berth` を実行するユーザー向けの案内である。
pub(crate) fn explain(why: &Unreachable, endpoint: &str) -> String {
    match why {
        Unreachable::NotRunning => {
            format!("berth: no berthd at {endpoint} (nothing to connect to)")
        }
        Unreachable::Denied => format!(
            "berth: {}\nReach it from where it was started (ssh in and run berth there).",
            why.describe(endpoint)
        ),
        // 名前を他人に取られている。起こしても bind で落ちる（`already running`）ので、そう言う。
        Unreachable::Impostor { .. } => format!(
            "berth: {}\nberthd cannot take this name while that process holds it; I will not start one.",
            why.describe(endpoint)
        ),
        // 接頭辞は 1 箇所で付ける（この CLI のメッセージは全部 `berth: ` で始まる）。
        _ => format!("berth: {}", why.describe(endpoint)),
    }
}

/// 応答を待たない制御メッセージ（`Close` / `KillServer`）。返り値 = berthd が居たか。
///
/// berthd はこの 2 つに応答を返さない。[`request`] で送ると、`Close` は来ない返事を待ってハングし、
/// `KillServer` は切断を「応答が読めない」という偽のエラーとして報告する。送って落とすのが正しい。
pub(crate) async fn send(c: &Control) -> Result<bool, String> {
    // 「未起動」の場合のみ false（送る相手が存在しない）。
    let Some(stream) = connect_unless_absent().await? else {
        return Ok(false);
    };
    let (_rd, mut wr) = tokio::io::split(stream);
    send_ctrl(&mut wr, 1, c)
        .await
        .map_err(|e| format!("berth: send failed: {e}"))?;
    Ok(true)
}

/// berthd に繋ぐ。走っていないときだけ `None`。
///
/// 拒否や話中まで `None` にすると、呼び手が「berthd は走っていない」と刷る
/// （`berth ls` が「no berthd sessions」を rc=0 で出す）。∴ それ以外は理由を名乗って `Err`。
async fn connect_unless_absent() -> Result<Option<berthd::DaemonStream>, String> {
    match connect_daemon().await {
        Ok(s) => Ok(Some(s)),
        Err(e) => {
            let why = Unreachable::of(e);
            if why.means_not_running() {
                Ok(None)
            } else {
                Err(explain(&why, &endpoint_display()))
            }
        }
    }
}

/// 制御フレームを 1 つ送る。
pub(crate) async fn send_ctrl<W: tokio::io::AsyncWrite + Unpin>(
    wr: &mut W,
    serial: u32,
    c: &Control,
) -> std::io::Result<()> {
    berthd::write_frame(wr, &control_frame(SOURCE_CLI, serial, c)).await
}

/// berthd が名乗り返した中身（`Welcome`）。
pub(crate) struct Welcome {
    pub(crate) server: String,
    pub(crate) protocol: u32,
    pub(crate) host: String,
}

/// 握手: 名乗って（`Hello`）、`Welcome` を受け取る。berthd は `Hello` に必ず `Welcome` を返す。
///
/// 注意: 応答は**ここで読み切る**こと。読み捨てないと、次の制御待ちが `Welcome` を取得してしまう。
/// 注意: 別の応答が来たらそのまま続行しないこと。ストリームが 1 つずれたまま進み、症状は
/// 「次の返事がおかしい」に化ける。∴ 起きた所で名乗る。
pub(crate) async fn handshake<R, W>(rd: &mut R, wr: &mut W) -> Result<Welcome, String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let hello = Control::Hello {
        client: format!("berth/{}", berthd::VERSION_STRING),
    };
    send_ctrl(wr, 1, &hello)
        .await
        .map_err(|e| format!("berth: send failed: {e}"))?;
    match next_control(rd).await? {
        Control::Welcome {
            server,
            protocol,
            host,
        } => Ok(Welcome {
            server,
            protocol,
            host,
        }),
        other => Err(format!("berth: unexpected reply to hello: {other:?}")),
    }
}

/// 制御メッセージを送り、最初の制御応答を返す。berthd 未起動なら `None`。
pub(crate) async fn request(c: &Control) -> Result<Option<Control>, String> {
    // 「未起動」の場合のみ None（セッションが 1 つも存在しない状態）。
    let Some(stream) = connect_unless_absent().await? else {
        return Ok(None);
    };
    let (mut rd, mut wr) = tokio::io::split(stream);
    send_ctrl(&mut wr, 1, c)
        .await
        .map_err(|e| format!("berth: send failed: {e}"))?;
    // data フレーム（他 session の出力 push）が混ざりうるので、制御が来るまで読み飛ばす。
    next_control(&mut rd).await.map(Some)
}

/// 走っている berthd が古くないかを見て、古ければ警告する（止めはしない）。
///
/// berthd はクライアントより長生きする（それが永続化の本体）ので、更新しても古い berthd が
/// 走り続ける。berthd は未知の制御メッセージを黙って捨てるので、新しい verb は沈黙のうちに
/// 効かない（例: 古い berthd は `BERTHD_SESSION` を子に渡さず、`berth ls` の `*` が付かない）。
/// 「効かないのに何も言わない」を避けるため、ここで一度だけ声を出す。
pub(crate) async fn warn_if_stale() {
    let Ok(stream) = connect_daemon().await else {
        // 未起動ならバージョンの差異もない。拒否・話中でもここは黙る。これは警告器であり、
        // 続けて走る本命のコマンドが理由を名乗る（ここでも刷ると同じ事実が 2 行出る）。
        return;
    };
    let (mut rd, mut wr) = tokio::io::split(stream);
    if let Ok(Welcome { protocol, .. }) = handshake(&mut rd, &mut wr).await {
        if protocol < PROTOCOL_VERSION {
            eprintln!(
                "berth: warning — the running berthd is old (protocol {protocol} < {PROTOCOL_VERSION}).\n\
                 Newer features fail silently. Replace it with `berth kill-server` (add -f to take\n\
                 live sessions down with it), then reopen the app."
            );
        }
    }
}

/// 制御フレームが来るまで読む（data フレームは読み飛ばす）。attach 前の応答待ち用。
pub(crate) async fn next_control<R: tokio::io::AsyncRead + Unpin>(
    rd: &mut R,
) -> Result<Control, String> {
    loop {
        let f = read_frame(rd)
            .await
            .map_err(|e| format!("berth: read: {e}"))?;
        if f.kind == KIND_CONTROL {
            return decode_control(&f.payload).map_err(|e| format!("berth: parse: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::explain;
    use berthd::Unreachable;

    // ─── 到達不能の分類 ───────────────────────────────
    //
    // 本物の「拒否」は cargo test で作れない（別のログインセッションで berthd を起こす必要が在る）。
    // ∴ ここで守るのは分類そのもの。「拒否のときに拒否と言うか」までしか言えず、
    // 「本当に拒否が起きるか」は言えない。

    #[test]
    fn a_missing_endpoint_is_the_only_thing_that_means_not_running() {
        use std::io::ErrorKind::*;
        for kind in [NotFound, ConnectionRefused] {
            let u = Unreachable::of(std::io::Error::new(kind, "x"));
            assert!(u.means_not_running(), "{kind:?} は未起動として読むべき");
        }
        // 陰性対照: これらを未起動と解釈してはならない。
        for kind in [PermissionDenied, InvalidData] {
            let u = Unreachable::of(std::io::Error::new(kind, "x"));
            assert!(
                !u.means_not_running(),
                "{kind:?} を未起動と読んではいけない"
            );
        }
    }

    #[test]
    fn denied_never_says_nothing_to_connect_to() {
        let denied = Unreachable::of(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "x",
        ));
        let msg = explain(&denied, "ENDPOINT");
        // これが戻ると、「開かせてもらえない」を「居ない」と刷ることになる。
        assert!(
            !msg.contains("nothing to connect to"),
            "拒否が不在の文言に戻っている: {msg}"
        );
        assert!(msg.contains("access denied"), "理由を名乗っていない: {msg}");
        assert!(msg.contains("ENDPOINT"), "どこの話か言っていない: {msg}");

        // 陽性対照: 不在のときはその文言でよい。
        let gone = Unreachable::of(std::io::Error::new(std::io::ErrorKind::NotFound, "x"));
        assert!(explain(&gone, "ENDPOINT").contains("nothing to connect to"));
    }

    #[test]
    fn an_unknown_error_is_shown_as_written_not_rounded() {
        let u = Unreachable::of(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "WEIRD-DETAIL",
        ));
        // 注意: 丸めると、次に別の原因で接続できなくなった際の手掛かりが失われる。
        assert!(
            explain(&u, "E").contains("WEIRD-DETAIL"),
            "綴りを捨てている: {}",
            explain(&u, "E")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_busy_pipe_is_not_an_absent_one() {
        // 注意: `ERROR_PIPE_BUSY` (231) は `ErrorKind` へのマッピングがないため、生のエラー番号で判定している。
        let u = Unreachable::of(std::io::Error::from_raw_os_error(231));
        assert!(!u.means_not_running(), "話中を未起動と読んではいけない");
        assert!(explain(&u, "E").contains("busy"), "{}", explain(&u, "E"));
    }
}
// unreachable-classifier tests
