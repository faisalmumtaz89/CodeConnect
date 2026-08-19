//! The upstream app-server connection, behind a factory seam.
//!
//! A4: **one upstream per client connection, N per leg** (the TUI's `/resume` picker
//! opens a *second* concurrent app-server connection, so a one-per-leg model kills the
//! live TUI). Every accepted client connection therefore asks the [`UpstreamFactory`]
//! for a fresh upstream.
//!
//! The data path is plain channels of whole [`Message`]s — no async-fn-in-trait, no
//! `Send` gymnastics. The security core never touches the network; the factory is the
//! single seam where a real WS-over-UDS connection (production) or a scripted fake
//! (integration tests, driven by captured Phase-0 frames) is supplied. This is how the
//! broker is "standalone testable against the captured frames without live infra."

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

/// The duplex to one upstream app-server connection. Whole messages only.
///
/// * `to_upstream` — client→server messages the broker has **admitted** (forwarded).
///   Refused messages never reach here; that is the whole point.
/// * `from_upstream` — server→client messages, relayed to the client byte-exact.
///
/// Dropping either half tears the connection down.
pub struct UpstreamChannels {
    pub to_upstream: mpsc::Sender<Message>,
    pub from_upstream: mpsc::Receiver<Message>,
}

/// A boxed, `Send` connect future — keeps the factory trait object-safe.
pub type ConnectFuture = Pin<Box<dyn Future<Output = anyhow::Result<UpstreamChannels>> + Send>>;

/// Establishes a fresh upstream connection per client connection.
pub trait UpstreamFactory: Send + Sync + 'static {
    fn connect(&self) -> ConnectFuture;
}

/// Byte-based frame/message bounds. D8: single notifications reach multi-MB
/// (`plugin/list` at 5.76 MB is the A4 buffer worst case), so the bounds are byte-based,
/// not message-count-based, and generous enough that a legitimate large frame is never
/// rejected while still capping a malicious body.
pub fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(64 << 20), // 64 MiB
        max_frame_size: Some(64 << 20),   // 64 MiB
        ..Default::default()
    }
}

/// Production factory: WebSocket over a Unix domain socket to the Codex app-server.
///
/// SEAM: the broker owns an `initialize` the client never sees when it must re-attach a
/// rotated connection (D4). That belongs to the switch sub-chunk; here each upstream is
/// a plain relay of one client connection's traffic, and the client's own `initialize`
/// flows through the allowlist.
#[derive(Clone)]
pub struct WsUdsUpstreamFactory {
    sock_path: PathBuf,
    buffer: usize,
}

impl WsUdsUpstreamFactory {
    pub fn new(sock_path: impl Into<PathBuf>) -> Self {
        Self {
            sock_path: sock_path.into(),
            buffer: 64,
        }
    }
}

impl UpstreamFactory for WsUdsUpstreamFactory {
    fn connect(&self) -> ConnectFuture {
        let sock = self.sock_path.clone();
        let buffer = self.buffer;
        Box::pin(async move {
            let stream = UnixStream::connect(&sock).await?;
            // The broker is the WS *client* to the app-server. `ws://localhost/` mirrors
            // the Phase-0 wsuds handshake (Host: localhost, Upgrade: websocket).
            let (ws, _resp) = tokio_tungstenite::client_async_with_config(
                "ws://localhost/",
                stream,
                Some(ws_config()),
            )
            .await?;
            let (to_tx, to_rx) = mpsc::channel::<Message>(buffer);
            let (from_tx, from_rx) = mpsc::channel::<Message>(buffer);
            tokio::spawn(pump(ws, to_rx, from_tx));
            Ok(UpstreamChannels {
                to_upstream: to_tx,
                from_upstream: from_rx,
            })
        })
    }
}

/// Bridge one upstream WebSocket to the two channels. Control frames (ping/pong) are
/// terminated per hop by tungstenite and never relayed; a close/EOF tears down both
/// directions.
async fn pump<S>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    mut to_rx: mpsc::Receiver<Message>,
    from_tx: mpsc::Sender<Message>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    loop {
        tokio::select! {
            biased;
            outbound = to_rx.recv() => match outbound {
                Some(msg) => {
                    if ws.send(msg).await.is_err() {
                        break;
                    }
                }
                None => break, // client connection dropped
            },
            inbound = ws.next() => match inbound {
                Some(Ok(msg)) => {
                    if matches!(msg, Message::Ping(_) | Message::Pong(_)) {
                        continue;
                    }
                    let closing = msg.is_close();
                    if from_tx.send(msg).await.is_err() {
                        break;
                    }
                    if closing {
                        break;
                    }
                }
                Some(Err(_)) | None => break, // app-server error/EOF
            },
        }
    }
}
