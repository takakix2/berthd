//! デーモンとして待つ口（unix socket / 名前付きパイプ）。

use super::*;

// ── デーモンへの道（transport）──
//
// `socket_path()` は「パス」を返すが、Windows の待ち口はパスではない。名前付きパイプは
// file system に存在せず、`\\.\pipe\berthd-<user>` という名前。∴ 同じ関数の戻り値を面で
// 分けることはできず、型ごと割れる。
//
// ∴ 公開するのは「どこで待つか」ではなく「どう繋ぐか」。呼ぶ側（berthd 本体・berth・
// ほかの client）は端点の綴りを知らなくてよく、`connect_daemon()` を呼ぶだけになる。

/// **デーモンが accept した側**の接続。
///
/// unix socket は両端が同じ型（`UnixStream`）だが、名前付きパイプは異なる。
/// 繋ぐ側が `NamedPipeClient`、待って受けた側が `NamedPipeServer` で、別の型。
/// ∴ 別名を 2 つに分ける（1 つで済ませようとすると unix でしか通らない）。
///
/// 上位は `AsyncRead + AsyncWrite + Unpin + Send` しか要求しないので、
/// 2 つに分かれていること自体は誰にも見えない。
#[cfg(unix)]
pub type DaemonConn = tokio::net::UnixStream;
/// デーモンが accept した側の接続（Windows）。
#[cfg(windows)]
pub type DaemonConn = tokio::net::windows::named_pipe::NamedPipeServer;

/// デーモンとして待つ口。
///
/// # accept の形が面で違う
///
/// unix の `UnixListener` は 1 つの listener が何度でも `accept()` を返すが、
/// **名前付きパイプは「1 インスタンス = 1 接続」**。接続が付いたら、その
/// インスタンスはその相手のものになるので、次の客のためにもう 1 つ作る必要が在る。
/// 注意: 作り直しを忘れると、2 人目が永久に繋がらない（1 人目は正常に動くので気づけない）。
pub struct DaemonListener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    next: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
}

/// デーモンとして待ち口を開く。
///
/// # Errors
/// bind / パイプ作成に失敗したら返す（既に別の berthd が居る場合を含む）。
pub async fn bind_daemon() -> std::io::Result<DaemonListener> {
    #[cfg(unix)]
    {
        Ok(DaemonListener {
            inner: tokio::net::UnixListener::bind(socket_path())?,
        })
    }
    #[cfg(windows)]
    {
        // `first_pipe_instance(true)` は排他の宣言。同じ名前で既に誰かが
        // 待っていれば、ここが失敗する（unix 側の「生きた berthd が居れば譲る」に相当する）。
        let server = create_pipe_instance(true)?;
        Ok(DaemonListener { next: Some(server) })
    }
}

/// パイプのインスタンスを 1 本作る（鍵付き）。
///
/// 重要: **`bind_daemon` と `accept` の両方がここを通ること。** 名前付きパイプの DACL は
/// 作成時にしか決められないので、片方だけ締めると 2 人目以降の客の口が素のままになる
/// （`win_pipe_acl` の doc）。∴ 作る場所を 1 箇所に畳んである。
///
/// 鍵を組めなかったら既定に落ちない。既定 DACL は `Everyone` / `Anonymous` に
/// 読み取りを与える。黙って落ちると「鍵を掛けたつもり」だけが残る。
#[cfg(windows)]
pub(crate) fn create_pipe_instance(
    first: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    create_pipe_instance_at(pipe_name(), first)
}

/// 端点を名指して鍵付きの口を作る。`create_pipe_instance` はこれに既定の名前を渡すだけ。
///
/// 名指す口が要る理由は [`connect_daemon_at`] と同じ。検体は端点を共有できない。
/// `pipe_name()` の口は生きている berthd の物なので、検体がそこへ作ると
/// `first_pipe_instance` の排他とぶつかるか、最悪、本物の客を横取りする。
#[cfg(windows)]
pub(crate) fn create_pipe_instance_at(
    name: impl AsRef<std::ffi::OsStr>,
    first: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let acl = crate::win_pipe_acl::PipeAcl::owner_only()?;
    let mut sa = acl.security_attributes();
    let mut opts = tokio::net::windows::named_pipe::ServerOptions::new();
    if first {
        opts.first_pipe_instance(true);
    }
    // SAFETY: `sa` は有効な `SECURITY_ATTRIBUTES` で、`acl` がこの関数の終わりまで
    // descriptor を所有している。`CreateNamedPipeW` は呼び出しの間しか触らない。
    unsafe { opts.create_with_security_attributes_raw(name, std::ptr::addr_of_mut!(sa).cast()) }
}

impl DaemonListener {
    /// 次の客を待つ。
    ///
    /// # Errors
    /// accept / パイプ作成の失敗をそのまま返す。
    pub async fn accept(&mut self) -> std::io::Result<DaemonConn> {
        #[cfg(unix)]
        {
            let (stream, _addr) = self.inner.accept().await?;
            Ok(stream)
        }
        #[cfg(windows)]
        {
            // 前回作れなかったときは、ここで作り直す（失敗を「待ち口が無い」のまま固定しない）。
            let server = match self.next.take() {
                Some(server) => server,
                None => create_pipe_instance(false)?,
            };
            let connected = server.connect().await;
            // 繋がった瞬間に次のインスタンスを作る（上の doc の理由）。注意: connect が失敗しても
            // 作る。失敗したインスタンスは使えないので、次を持たないと以後の accept が全部落ちる。
            // 2 本目以降も同じ鍵で作る（`create_pipe_instance` の doc）。
            // 作れなければ `None` のまま返し、次の accept の頭で作り直す。
            match create_pipe_instance(false) {
                Ok(next) => self.next = Some(next),
                Err(e) => {
                    connected?;
                    return Err(e);
                }
            }
            connected?;
            Ok(server)
        }
    }
}
