//! The upstream app-server connection, behind a factory seam.
//!
//! A4: **one upstream per client connection, N per leg** (the TUI's `/resume` picker
//! opens a *second* concurrent app-server connection, so a one-per-leg model kills the
//! live TUI). Every accepted client connection therefore asks the [`UpstreamFactory`]
//! for a fresh upstream.
//!
//! The data path is plain channels of whole messages — server→client as [`Message`],
//! client→server as [`UpstreamWrite`] (the message plus the optional receipt for its
//! write) — no async-fn-in-trait, no `Send` gymnastics. The security core never touches the network; the factory is the
//! single seam where a real WS-over-UDS connection (production) or a scripted fake
//! (integration tests, driven by captured Phase-0 frames) is supplied. This is how the
//! broker is "standalone testable against the captured frames without live infra."

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

/// One admitted client→server message, plus an optional receipt for the **write**.
///
/// Handing a message to `to_upstream` proves nothing about the app-server: it is an
/// in-process channel, and the socket write happens later in [`pump`]. Anything the
/// broker says to a leg about the fate of its message on the strength of the hand-off
/// alone is a claim about a write that has not been attempted yet, and it is wrong
/// whenever the pump dies in between. The only frame that has to be spoken about is a
/// `ccd` leg's response to a `serverRequest` (see [`crate::relay`]'s response
/// disposition), so that one carries `ack` and every other forward carries `None` —
/// no channel, no allocation, no wait.
///
/// `ack` receives `true` only after [`pump`] has completed the `ws.send` for this
/// message. A `false`, or a dropped sender (the pump is gone, or was cancelled
/// mid-write), both mean the same thing to the awaiting side: **this broker cannot
/// prove a complete message reached the app-server.**
pub struct UpstreamWrite {
    pub msg: Message,
    pub ack: Option<oneshot::Sender<bool>>,
}

impl UpstreamWrite {
    /// A forward whose fate nobody is waiting on — every message except a `ccd` leg's
    /// answer to a `serverRequest`.
    pub fn unacked(msg: Message) -> UpstreamWrite {
        UpstreamWrite { msg, ack: None }
    }

    /// A forward and the receipt for its write. The receiver resolves to `true` once the
    /// `ws.send` has completed, to `false` if it failed — and to a `RecvError` (which
    /// the caller MUST read as `false`) if the pump died before it could answer.
    pub fn acked(msg: Message) -> (UpstreamWrite, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        (UpstreamWrite { msg, ack: Some(tx) }, rx)
    }
}

/// The duplex to one upstream app-server connection. Whole messages only.
///
/// * `to_upstream` — client→server messages the broker has **admitted** (forwarded),
///   each in an [`UpstreamWrite`] envelope. Refused messages never reach here; that is
///   the whole point.
/// * `from_upstream` — server→client messages, relayed to the client byte-exact.
/// * `pump` — the task moving bytes between those channels and the socket, kept so that
///   dropping this really does end the connection. See [`PumpHandle`].
///
/// Dropping this tears the connection down.
pub struct UpstreamChannels {
    pub to_upstream: mpsc::Sender<UpstreamWrite>,
    pub from_upstream: mpsc::Receiver<Message>,
    /// `None` for a scripted upstream that has no separate task to end — the fake in a
    /// test drains the channel itself, so closing the channel is the whole teardown.
    pub pump: Option<PumpHandle>,
}

/// **The pump's task, ended when its upstream is dropped.**
///
/// Closing the channels is not a teardown. It ends a pump sitting at `to_rx.recv()`, and
/// that is the ordinary case — but a pump parked inside `sink.send`, against an
/// app-server socket that has stopped draining, is not at the receive and will never
/// return to it. Detached, that task, its socket and its half-written frame outlive the
/// leg that created them, so a close the broker performs precisely BECAUSE the upstream
/// stopped behaving like the app-server could not promise the old upstream was gone.
///
/// An abort rather than a deadline on the write, because there is nothing left to wait
/// for: the decision to tear the connection down has already been taken by the time this
/// is dropped, and the frame in flight is one the broker has already reported as
/// unproven. See [`crate::relay`]'s write budget for the side that makes that report.
pub struct PumpHandle(tokio::task::JoinHandle<()>);

impl PumpHandle {
    pub fn new(task: tokio::task::JoinHandle<()>) -> PumpHandle {
        PumpHandle(task)
    }
}

impl Drop for PumpHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn [`pump`] and keep the handle that ends it. The one way a real upstream's pump
/// is started, so no caller can accidentally detach one.
pub fn spawn_pump<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    to_rx: mpsc::Receiver<UpstreamWrite>,
    from_tx: mpsc::Sender<Message>,
) -> PumpHandle
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    PumpHandle(tokio::spawn(pump(ws, to_rx, from_tx)))
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
            let (to_tx, to_rx) = mpsc::channel::<UpstreamWrite>(buffer);
            let (from_tx, from_rx) = mpsc::channel::<Message>(buffer);
            let pump = spawn_pump(ws, to_rx, from_tx);
            Ok(UpstreamChannels {
                to_upstream: to_tx,
                from_upstream: from_rx,
                pump: Some(pump),
            })
        })
    }
}

/// Bridge one upstream WebSocket to the two channels. Control frames (ping/pong) are
/// terminated per hop by tungstenite and never relayed; a close/EOF tears down both
/// directions.
///
/// **The two directions are split, and that is what keeps the write receipt safe to
/// wait on.** A single `select!` loop parks inside whichever arm it took, so a writer
/// blocked on `from_tx.send` (the relay's s2c queue is bounded, and the relay stops
/// draining it while it is classifying) would stop serving `to_rx` entirely — and a
/// leg awaiting the acknowledgement for a message sitting unread in `to_rx` would
/// wait for ever. Split into two halves polled concurrently by one `select!`, neither
/// direction can withhold progress from the other; the first half to finish still
/// tears the whole connection down, exactly as the single loop did.
pub async fn pump<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    mut to_rx: mpsc::Receiver<UpstreamWrite>,
    from_tx: mpsc::Sender<Message>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = ws.split();
    // Acknowledged AFTER the write, never before: `ack` is the statement "these bytes
    // went out", and a receipt issued on receipt of the message would say nothing the
    // channel hand-off had not already said. A failed send answers `false` and ends
    // the half; a cancelled one (the read side finished first) drops the sender, which
    // the awaiting side reads as `false` for the same reason.
    let outbound = async move {
        // Ends when the channel closes: the client connection is gone.
        while let Some(write) = to_rx.recv().await {
            let wrote = sink.send(write.msg).await.is_ok();
            if let Some(ack) = write.ack {
                let _ = ack.send(wrote);
            }
            if !wrote {
                break;
            }
        }
    };
    let inbound = async move {
        // Ends on app-server EOF, or on the error/close handled inside.
        while let Some(inbound) = stream.next().await {
            let Ok(msg) = inbound else {
                break; // app-server error
            };
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
    };
    tokio::select! {
        _ = outbound => {}
        _ = inbound => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::WebSocketStream;

    /// A frame big enough that a one-byte pipe cannot swallow it in a single write.
    fn frame() -> Message {
        Message::Text(format!(
            r#"{{"id":1,"result":{{"pad":"{}"}}}}"#,
            "x".repeat(64)
        ))
    }

    /// **The receipt follows the write; it does not precede it.**
    ///
    /// The pipe holds one byte at a time, so the frame is stuck in the socket until the
    /// peer reads it. Anything acknowledged during that window is an acknowledgement of
    /// the channel hand-off, not of the write — which is the whole distinction the
    /// receipt exists to draw.
    ///
    /// **Mutation:** acknowledge before `sink.send` (or acknowledge `true`
    /// unconditionally) and the receipt resolves while the bytes are still in the pipe.
    #[tokio::test]
    async fn the_write_receipt_is_sent_after_the_socket_took_the_bytes() {
        let (near, far) = tokio::io::duplex(1);
        let ws = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let mut peer = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        let (to_tx, to_rx) = mpsc::channel::<UpstreamWrite>(4);
        let (from_tx, _from_rx) = mpsc::channel::<Message>(4);
        tokio::spawn(pump(ws, to_rx, from_tx));

        let sent = frame();
        let (write, mut receipt) = UpstreamWrite::acked(sent.clone());
        to_tx.send(write).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            matches!(receipt.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "nothing may be acknowledged while the frame is still stuck in the socket"
        );

        let got = peer.next().await.expect("frame").expect("ws");
        assert_eq!(got, sent, "the peer receives the frame byte-exact");
        assert!(
            receipt.await.expect("the pump answers a completed write"),
            "a completed write is acknowledged true"
        );
    }

    /// **A backed-up s2c queue may not withhold the write receipt.**
    ///
    /// The two directions are polled by one `select!`, and a `select!` parks inside
    /// whichever arm it took. If the inbound half's `from_tx.send` and the outbound
    /// half's `sink.send` shared an arm, an s2c queue the relay had stopped draining
    /// would stop the pump serving `to_rx` at all — and a leg awaiting the receipt for a
    /// message sitting unread in `to_rx` would wait for the whole write budget and then
    /// tear down a perfectly healthy upstream.
    ///
    /// So: fill `from_tx` past capacity with nobody draining it, and require an
    /// acknowledged write to still complete. This is the liveness guard for the split
    /// halves, and it is what makes the bound in `relay.rs` a bound on a STUCK peer
    /// rather than on a busy one.
    ///
    /// **Mutation:** merge the two halves back into a single `select!` loop body (or
    /// `await` the inbound send inside the outbound arm) and the receipt below never
    /// resolves.
    #[tokio::test]
    async fn a_full_undrained_s2c_queue_does_not_withhold_the_write_receipt() {
        let (near, far) = tokio::io::duplex(4096);
        let ws = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let mut peer = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        let (to_tx, to_rx) = mpsc::channel::<UpstreamWrite>(4);
        // Capacity ONE, and never drained: `_from_rx` is held so the channel stays open.
        let (from_tx, _from_rx) = mpsc::channel::<Message>(1);
        tokio::spawn(pump(ws, to_rx, from_tx));

        // Push the app-server's chatter in until the inbound half is parked on a
        // `from_tx.send` that nothing will ever accept.
        use futures_util::SinkExt as _;
        for i in 0..4 {
            peer.send(Message::Text(format!(r#"{{"n":{i}}}"#)))
                .await
                .expect("the peer can write");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The outbound half must still be serving.
        let (write, receipt) = UpstreamWrite::acked(frame());
        to_tx
            .send(write)
            .await
            .expect("the pump still takes writes");
        let acked = tokio::time::timeout(std::time::Duration::from_secs(2), receipt)
            .await
            .expect("the receipt must resolve while the s2c queue is stuck")
            .expect("the pump answers");
        assert!(acked, "the write completed, so it is acknowledged true");
    }

    /// **Tearing the upstream down has to end the pump, and dropping the channels does
    /// not.**
    ///
    /// A leg that closes drops its [`UpstreamChannels`], and the contract that used to
    /// rest on is "the sender closes, so `to_rx.recv()` returns `None` and the outbound
    /// half ends". That is only true of a pump sitting AT the receive. A pump parked
    /// inside `sink.send`, against a socket that stopped draining, notices nothing: it is
    /// not at the receive, and no amount of closing the channel behind it will bring it
    /// back there. The task, its socket and its half-written frame outlive the leg that
    /// created them — and the moment a session-fatal close most needs the old upstream
    /// gone is exactly the moment it is stuck like this.
    ///
    /// So the spawn's handle travels with the channels, and the teardown aborts it.
    ///
    /// **The proof is a sentinel the task owns**, not the socket. Reading the far end to
    /// see it finish would hand the parked write the bytes of progress it was waiting
    /// for — the observation would be what ended the pump. Nothing here reads the pipe;
    /// what is watched is whether the task's own future was dropped.
    ///
    /// **Mutation:** spawn the pump detached (drop the handle rather than keeping it, or
    /// remove the abort) and the count below stays at two.
    #[tokio::test]
    async fn tearing_the_upstream_down_ends_a_pump_parked_in_a_write() {
        let (near, far) = tokio::io::duplex(1);
        let ws = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let (to_tx, to_rx) = mpsc::channel::<UpstreamWrite>(64);
        let (from_tx, from_rx) = mpsc::channel::<Message>(64);
        // The far end is held open and NEVER read: the pipe takes one byte and the rest
        // of the frame stays in the write for ever, which is what an app-server that
        // stopped draining does to a real upstream.
        let _far = far;

        // Owned by the pump's future, so it lives exactly as long as that future does.
        let alive = std::sync::Arc::new(());
        let held = std::sync::Arc::clone(&alive);
        let task = tokio::spawn(async move {
            let _held = held;
            pump(ws, to_rx, from_tx).await;
        });
        let up = UpstreamChannels {
            to_upstream: to_tx,
            from_upstream: from_rx,
            pump: Some(PumpHandle::new(task)),
        };

        up.to_upstream
            .send(UpstreamWrite::unacked(frame()))
            .await
            .expect("the pump takes the message");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            std::sync::Arc::strong_count(&alive),
            2,
            "the staging is only real if the pump is still parked in the write"
        );

        // The leg ended.
        drop(up);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            std::sync::Arc::strong_count(&alive),
            1,
            "an upstream that was torn down may not leave its pump holding the socket"
        );
    }

    /// **A write the socket never took is never acknowledged true.**
    ///
    /// The app-server end is gone before the message is handed over, so the `ws.send`
    /// cannot succeed. Whether the pump answers `false` or dies before it can answer,
    /// the awaiting side must read the same thing — no complete message reached the
    /// app-server — which is what makes `unwrap_or(false)` the honest read.
    ///
    /// **Mutation:** acknowledge `true` regardless of what `sink.send` returned and a
    /// write into a dead socket reports itself delivered.
    #[tokio::test]
    async fn a_write_the_socket_refused_is_never_acknowledged_true() {
        let (near, far) = tokio::io::duplex(64);
        let ws = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        drop(far); // the app-server is gone
        let (to_tx, to_rx) = mpsc::channel::<UpstreamWrite>(4);
        let (from_tx, _from_rx) = mpsc::channel::<Message>(4);
        tokio::spawn(pump(ws, to_rx, from_tx));

        let (write, receipt) = UpstreamWrite::acked(frame());
        let _ = to_tx.send(write).await;
        assert!(
            !receipt.await.unwrap_or(false),
            "a write into a dead socket is never reported as written"
        );
    }
}
