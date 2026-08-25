//! The WS-over-UDS relay skeleton — the async transport edge.
//!
//! Two Unix-domain listeners (`tui.sock`, `ccd.sock`); each accepted connection gets its
//! own upstream app-server connection (A4: N upstreams per leg — the `/resume` picker
//! opens a second one). The client→server direction is whole-message classified by the
//! pure security core before **any** byte is forwarded; the server→client direction is a
//! byte-exact passthrough.
//!
//! Every accepted connection is stamped with a monotonic [`ConnId`] (round-2 P1). It is
//! threaded into BOTH directions — the c2s classifier reads it through
//! [`crate::refusal::Env::conn`], and the s2c thread-binding observer takes it as a
//! parameter — so thread-creation correlation is CONNECTION-scoped rather than role-scoped
//! and two connections of the same role cannot answer each other's pending creation. The
//! id appears in the `leg opened` / `leg ended` / error log lines so an operator can follow
//! one connection through `broker.log`.
//!
//! Deliberately NOT here (clean seams for the switch/fanout sub-chunk): the D2
//! per-leg/session latch and vector barrier, quiesce/seal, generation/epoch stamping,
//! the one-use response-capability fanout, per-leg failure containment beyond a plain
//! close, and the byte-fidelity comparison harness. The role is anchored to the socket;
//! the live reinitialization guard (a second `initialize` fails closed) is enforced
//! here, and [`crate::allowlist::narrow_role`] holds the identity-narrowing rule.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::allowlist::Role;
use crate::fingerprint::LaunchFingerprint;
use crate::message::{RequestId, Shape, WsPayload};
use crate::refusal::{decide, Env, RelayAction};
use crate::response_capability::{LegCapabilities, ResponseArbiter};
use crate::session::{ConnId, SessionThreads, ThreadBinding};
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
    /// The shared one-use response-capability arbiter (fanout winner across all legs).
    /// Each connection wraps it in its own [`LegCapabilities`] view; the arbiter itself
    /// is session-wide so the first response to a fanned-out approval wins across legs.
    arbiter: Arc<ResponseArbiter>,
    /// Session-scoped thread binding, shared across every connection: the c2s classifier
    /// claims a creation slot when it admits a `thread/start`, and the s2c stream of the
    /// SAME leg installs the binding when the correlated creation response verifies. The
    /// resume and turn paths read it.
    threads: SessionThreads,
    /// Mints the per-connection instance id ([`ConnId`]) handed to each accepted
    /// connection's task (round-2 P1). A monotonic counter is enough: it never wraps in any
    /// realistic session (2^64 accepts), and it only has to distinguish connections that
    /// are alive at the same time from one another and from every earlier one.
    next_conn: AtomicU64,
    log: EventSink,
}

impl<F: UpstreamFactory> Broker<F> {
    /// Build a broker. The one-use response-capability fanout arbiter is installed and
    /// fail-closed by construction: until a leg observes the soliciting `serverRequest`,
    /// every method-less response forwards zero upstream bytes.
    pub fn new(
        tui_sock: impl Into<PathBuf>,
        ccd_sock: impl Into<PathBuf>,
        fingerprint: LaunchFingerprint,
        factory: F,
    ) -> Self {
        // The session thread store is anchored to the launch cwd carried in the fingerprint
        // (round-2 P4): a creation response naming any other workspace binds nothing.
        let threads = SessionThreads::new(fingerprint.launch_cwd.clone());
        Self {
            tui_sock: tui_sock.into(),
            ccd_sock: ccd_sock.into(),
            ctx: Arc::new(Ctx {
                fingerprint,
                factory,
                arbiter: Arc::new(ResponseArbiter::new()),
                threads,
                next_conn: AtomicU64::new(1),
                log: Arc::new(|_| {}),
            }),
        }
    }

    /// Replace the audit-log sink.
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        Arc::get_mut(&mut self.ctx).expect("no clones yet").log = sink;
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
            // Mint this connection's instance id (round-2 P1) and announce it. The OPEN
            // marker is what lets a log reader attribute every later `(conn N)` line — and
            // the close/error lines below — to one connection.
            let conn = ConnId(ctx.next_conn.fetch_add(1, Ordering::Relaxed));
            (ctx.log)(&format!("{role:?}: leg opened (conn {conn})"));
            tokio::spawn(async move {
                let outcome = handle_connection(role, conn, stream, ctx.clone()).await;
                // The owning connection is gone. A creation still pending on it DID reach
                // the server, so it lands in the indeterminate closed state rather than
                // being stranded in flight for ever (round-2 P3); the connection's
                // reservation/tombstone sets are released at the same time.
                ctx.threads.close_connection(conn);
                match outcome {
                    // NOTE: the substrings `Tui leg ended` / `Ccd leg ended` are asserted by
                    // live gates — the connection id is APPENDED, never spliced into them.
                    Ok(()) => (ctx.log)(&format!("{role:?} leg ended (conn {conn}): closed")),
                    Err(e) => (ctx.log)(&format!("{role:?} leg ended (conn {conn}): {e}")),
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
    conn: ConnId,
    stream: UnixStream,
    ctx: Arc<Ctx<F>>,
) -> anyhow::Result<()> {
    let mut ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config())).await?;
    let mut up = ctx.factory.connect().await?;
    let mut seen_initialize = false;
    // This leg's own view over the shared fanout arbiter: the s2c stream registers the
    // one-use capabilities solicited on THIS connection, disambiguating a bare response
    // id into its thread (upstream ids are per-thread integers, reused across threads).
    let mut caps = LegCapabilities::new(Arc::clone(&ctx.arbiter), Arc::clone(&ctx.log));

    loop {
        tokio::select! {
            biased;
            // server -> client: byte-exact passthrough (classification is c2s-only), but
            // observed to verify this leg's pending thread creation (thread binding), to
            // release the outstanding request ids this leg's responses answer (round-3 P1),
            // and to register one-use response capabilities (approval fanout). Both
            // observers are PER CONNECTION — each correlates a bare response id against what
            // THIS connection solicited, keyed by the relay-minted `ConnId`. (O12: the
            // thread-binding key was `(Role, RequestId)` in round 1; it is
            // `(ConnId, RequestId)` now, so two connections of the same role cannot answer
            // each other's requests.)
            outbound = up.from_upstream.recv() => match outbound {
                Some(msg) => {
                    if let Message::Text(t) = &msg {
                        ctx.threads.observe_server_frame(conn, t);
                        caps.observe_server_frame(t);
                    }
                    ws.send(msg).await?
                }
                None => {
                    // Upstream (app-server) closed — deliberate close of the client leg.
                    (ctx.log)(&format!("{role:?}: upstream closed (conn {conn}); closing leg"));
                    break;
                }
            },
            // client -> server: whole-message classify before forwarding any byte.
            inbound = ws.next() => {
                let msg = match inbound {
                    // NOTE: `Ccd: read error` is asserted by a live gate — the connection id
                    // is APPENDED after it, never spliced into the marker.
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        (ctx.log)(&format!("{role:?}: read error (conn {conn}): {e}"));
                        break;
                    }
                    None => break, // client closed
                };
                match msg {
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        // No client→server binary form (refusal matrix): zero bytes, close.
                        (ctx.log)(&format!("{role:?}: binary frame (conn {conn}); closing leg"));
                        break;
                    }
                    Message::Text(text) => {
                        if handle_text(
                            role, conn, &ctx, &caps, &mut ws, &up, &mut seen_initialize, text,
                        )
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
#[allow(clippy::too_many_arguments)]
async fn handle_text<F, S>(
    role: Role,
    conn: ConnId,
    ctx: &Arc<Ctx<F>>,
    caps: &LegCapabilities,
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
                (ctx.log)(&format!(
                    "{role:?}: reinitialization (conn {conn}); closing leg"
                ));
                return Ok(true);
            }
            *seen_initialize = true;
        }
    }

    // Round-2 P3 — the creation slot is claimed inside `decide` (it must be atomic with the
    // decision), but the claim is only sound if the bytes actually GO OUT. The relay is the
    // only place that knows both the parsed shape and whether the upstream write succeeded,
    // so it remembers which id a `thread/start` would have claimed and rolls the claim back
    // if the write fails. A refused `thread/start` never reaches the `Forward` arm, so the
    // rollback below can only ever un-claim a claim this very message made.
    let creation_id: Option<RequestId> = match &shape {
        Shape::Request {
            method,
            id: Some(id),
            ..
        } if method == "thread/start" => Some(id.clone()),
        _ => None,
    };

    // Scope `env` so it is dropped before any await (its `&dyn` trait objects are not
    // `Send`, and the connection task must be `Send`).
    let action = {
        let env = Env {
            fingerprint: &ctx.fingerprint,
            capabilities: caps,
            threads: &ctx.threads,
            conn,
        };
        decide(role, &env, shape)
    };
    match action {
        RelayAction::Forward { note } => {
            // M8 — the connection id is APPENDED AFTER the existing parenthesised note, never
            // spliced into it. Live gates assert the exact substrings
            // `Tui: forward (ownership request: fingerprint asserted)`,
            // `Ccd: forward (request allowlisted)`, `Ccd: forward (notification allowlisted)`,
            // `Tui: forward (turn/start: head-checked; …)`, so nothing may be inserted between
            // `forward (` and the note text. The trailing `(conn N)` is what lets a gate pair
            // a forward with the connection that made it, by real connection identity rather
            // than by role.
            (ctx.log)(&format!("{role:?}: forward ({note}) (conn {conn})"));
            // Recover the original bytes (no injection this sub-chunk) and forward.
            let text = match payload {
                WsPayload::Text(t) => t,
                WsPayload::Binary => unreachable!("text branch"),
            };
            if up.to_upstream.send(Message::Text(text)).await.is_err() {
                if let Some(id) = &creation_id {
                    // ZERO bytes reached the server, so the creation provably did not
                    // happen: un-claim the slot so a retry (on a fresh connection) can
                    // create the session's thread. Never a tombstone — nothing is ambiguous.
                    ctx.threads.rollback_creation(conn, id);
                    (ctx.log)(&format!(
                        "{role:?}: upstream send failed (conn {conn}); creation claim rolled \
                         back"
                    ));
                }
                (ctx.log)(&format!(
                    "{role:?}: upstream gone (conn {conn}); closing leg"
                ));
                return Ok(true);
            }
        }
        // The same M8 rule for the three refusal dispositions: note first, `(conn N)` after.
        RelayAction::SyntheticError { frame, note } => {
            (ctx.log)(&format!(
                "{role:?}: refuse->synthetic error ({note}) (conn {conn})"
            ));
            ws.send(Message::Text(frame)).await?;
        }
        RelayAction::DropLogKeepOpen { note } => {
            (ctx.log)(&format!("{role:?}: drop, keep open ({note}) (conn {conn})"));
        }
        RelayAction::DropCloseLeg { note } => {
            (ctx.log)(&format!("{role:?}: drop, close leg ({note}) (conn {conn})"));
            return Ok(true);
        }
    }
    Ok(false)
}
