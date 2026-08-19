//! The WS-over-UDS relay skeleton — the async transport edge.
//!
//! Two Unix-domain listeners (`tui.sock`, `ccd.sock`); each accepted connection gets its
//! own upstream app-server connection (A4: N upstreams per leg — the `/resume` picker
//! opens a second one). The client→server direction is whole-message classified by the
//! pure security core before **any** byte is forwarded; the server→client direction is a
//! byte-exact passthrough.
//!
//! Deliberately NOT here (clean seams for the switch/fanout sub-chunk): the D2
//! per-leg/session latch and vector barrier, quiesce/seal, generation/epoch stamping,
//! the one-use response-capability fanout, per-leg failure containment beyond a plain
//! close, and the byte-fidelity comparison harness. The role is anchored to the socket;
//! the live reinitialization guard (a second `initialize` fails closed) is enforced
//! here, and [`crate::allowlist::narrow_role`] holds the identity-narrowing rule.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::allowlist::Role;
use crate::fingerprint::LaunchFingerprint;
use crate::message::{Shape, WsPayload};
use crate::refusal::{decide, Env, RelayAction};
use crate::response_capability::{NoCapabilities, ResponseCapabilityRegistry};
use crate::session::SessionThreads;
use crate::upstream::{ws_config, UpstreamFactory};

/// An audit-log sink. Every classification outcome and lifecycle event is reported here;
/// production wires this to `tracing`, tests to a recording buffer.
pub type EventSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The broker: two listeners, a launch fingerprint, and a per-connection upstream
/// factory. Generic over the factory so integration tests drive captured frames through
/// a fake upstream with no live app-server.
pub struct Broker<F: UpstreamFactory> {
    tui_sock: PathBuf,
    ccd_sock: PathBuf,
    ctx: Arc<Ctx<F>>,
}

struct Ctx<F: UpstreamFactory> {
    fingerprint: LaunchFingerprint,
    factory: F,
    caps: Arc<dyn ResponseCapabilityRegistry + Send + Sync>,
    /// Session-scoped thread binding, shared across every connection: the s2c stream
    /// populates it (`thread/started`), the c2s resume path reads it.
    threads: SessionThreads,
    log: EventSink,
}

impl<F: UpstreamFactory> Broker<F> {
    /// Build a broker. The capability registry defaults to the fail-closed
    /// [`NoCapabilities`] (the fanout registry is the switch sub-chunk's job).
    pub fn new(
        tui_sock: impl Into<PathBuf>,
        ccd_sock: impl Into<PathBuf>,
        fingerprint: LaunchFingerprint,
        factory: F,
    ) -> Self {
        Self {
            tui_sock: tui_sock.into(),
            ccd_sock: ccd_sock.into(),
            ctx: Arc::new(Ctx {
                fingerprint,
                factory,
                caps: Arc::new(NoCapabilities),
                threads: SessionThreads::new(),
                log: Arc::new(|_| {}),
            }),
        }
    }

    /// Replace the audit-log sink.
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        Arc::get_mut(&mut self.ctx).expect("no clones yet").log = sink;
        self
    }

    /// SEAM: install the real one-use response-capability registry (switch/fanout
    /// sub-chunk). Until then, method-less responses forward zero bytes.
    pub fn with_capabilities(
        mut self,
        caps: Arc<dyn ResponseCapabilityRegistry + Send + Sync>,
    ) -> Self {
        Arc::get_mut(&mut self.ctx).expect("no clones yet").caps = caps;
        self
    }

    /// Bind both listeners and accept forever. Each accepted connection is handled in its
    /// own task with its own upstream connection.
    pub async fn serve(self) -> io::Result<()> {
        let tui = UnixListener::bind(&self.tui_sock)?;
        let ccd = UnixListener::bind(&self.ccd_sock)?;
        (self.ctx.log)("broker: listening on tui.sock and ccd.sock");
        loop {
            tokio::select! {
                accepted = tui.accept() => spawn_leg(Role::Tui, accepted, &self.ctx),
                accepted = ccd.accept() => spawn_leg(Role::Ccd, accepted, &self.ctx),
            }
        }
    }
}

fn spawn_leg<F: UpstreamFactory>(
    role: Role,
    accepted: io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
    ctx: &Arc<Ctx<F>>,
) {
    match accepted {
        Ok((stream, _)) => {
            let ctx = Arc::clone(ctx);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(role, stream, ctx.clone()).await {
                    (ctx.log)(&format!("{role:?} leg ended: {e}"));
                }
            });
        }
        Err(e) => (ctx.log)(&format!("{role:?} accept error: {e}")),
    }
}

/// Handle one client connection end-to-end: WS handshake, a fresh upstream, then the
/// classify/forward loop.
async fn handle_connection<F: UpstreamFactory>(
    role: Role,
    stream: UnixStream,
    ctx: Arc<Ctx<F>>,
) -> anyhow::Result<()> {
    let mut ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config())).await?;
    let mut up = ctx.factory.connect().await?;
    let mut seen_initialize = false;

    loop {
        tokio::select! {
            biased;
            // server -> client: byte-exact passthrough (classification is c2s-only), but
            // observed to learn session-owned thread ids for resume binding.
            outbound = up.from_upstream.recv() => match outbound {
                Some(msg) => {
                    if let Message::Text(t) = &msg {
                        ctx.threads.observe_server_frame(t);
                    }
                    ws.send(msg).await?
                }
                None => {
                    // Upstream (app-server) closed — deliberate close of the client leg.
                    (ctx.log)(&format!("{role:?}: upstream closed; closing leg"));
                    break;
                }
            },
            // client -> server: whole-message classify before forwarding any byte.
            inbound = ws.next() => {
                let msg = match inbound {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => { (ctx.log)(&format!("{role:?}: read error: {e}")); break; }
                    None => break, // client closed
                };
                match msg {
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        // No client→server binary form (refusal matrix): zero bytes, close.
                        (ctx.log)(&format!("{role:?}: binary frame; closing leg"));
                        break;
                    }
                    Message::Text(text) => {
                        if handle_text(role, &ctx, &mut ws, &up, &mut seen_initialize, text)
                            .await?
                        {
                            break; // hostile/close disposition
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Classify one whole text message and act. Returns `Ok(true)` when the leg must close.
async fn handle_text<F, S>(
    role: Role,
    ctx: &Arc<Ctx<F>>,
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    up: &crate::upstream::UpstreamChannels,
    seen_initialize: &mut bool,
    text: String,
) -> anyhow::Result<bool>
where
    F: UpstreamFactory,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Parse the whole message exactly once (a 5.76 MB plugin/list is not reparsed).
    let payload = WsPayload::Text(text);
    let shape = crate::message::classify_shape(&payload);

    // Live reinitialization guard: role is anchored to the socket; a second `initialize`
    // on the same connection is an identity change and fails closed.
    if let Shape::Request { method, .. } = &shape {
        if method == "initialize" {
            if *seen_initialize {
                (ctx.log)(&format!("{role:?}: reinitialization; closing leg"));
                return Ok(true);
            }
            *seen_initialize = true;
        }
    }

    // Scope `env` so it is dropped before any await (its `&dyn` trait objects are not
    // `Send`, and the connection task must be `Send`).
    let action = {
        let env = Env {
            fingerprint: &ctx.fingerprint,
            capabilities: ctx.caps.as_ref(),
            threads: &ctx.threads,
        };
        decide(role, &env, shape)
    };
    match action {
        RelayAction::Forward { note } => {
            (ctx.log)(&format!("{role:?}: forward ({note})"));
            // Recover the original bytes (no injection this sub-chunk) and forward.
            let text = match payload {
                WsPayload::Text(t) => t,
                WsPayload::Binary => unreachable!("text branch"),
            };
            if up.to_upstream.send(Message::Text(text)).await.is_err() {
                (ctx.log)(&format!("{role:?}: upstream gone; closing leg"));
                return Ok(true);
            }
        }
        RelayAction::SyntheticError { frame, note } => {
            (ctx.log)(&format!("{role:?}: refuse->synthetic error ({note})"));
            ws.send(Message::Text(frame)).await?;
        }
        RelayAction::DropLogKeepOpen { note } => {
            (ctx.log)(&format!("{role:?}: drop, keep open ({note})"));
        }
        RelayAction::DropCloseLeg { note } => {
            (ctx.log)(&format!("{role:?}: drop, close leg ({note})"));
            return Ok(true);
        }
    }
    Ok(false)
}
