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
use crate::frame_tee::FrameTee;
use crate::message::{RequestId, Shape, WsPayload};
use crate::refusal::{decide, Env, RelayAction};
use crate::response_capability::S2cDisposition;
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
    /// **The head fan-out repair**: the last `thread/started` this broker forwarded,
    /// kept verbatim, and every `ccd` leg that is owed one.
    ///
    /// The app-server broadcasts `thread/started` once, to the connections it has
    /// at that instant, and never replays it. That is the whole announcement: the
    /// id exists on the wire for exactly as long as it takes to read the frame.
    /// The `ccd` control link is created by a *registration*, and a registration
    /// cannot happen before the launch is `ready`, which cannot happen before the
    /// TUI is running — so the connection that most needs the announcement is
    /// structurally the one most likely to arrive after it. Nothing repairs that:
    /// the link's own recovery has only the predecessor's carry and the
    /// registration's hint to chase, and neither can name a thread that appeared
    /// while nothing was watching.
    ///
    /// So the fan-out is repaired at the only place that still holds the fact.
    /// See [`deliver_head`] for what is delivered, to whom, on what authority, and
    /// why the announcement, the head and the subscriber table are one lock rather
    /// than three.
    ///
    /// A `std::sync::Mutex` and not tokio's: every access is a field read or write
    /// with no await between lock and unlock.
    head_fanout: std::sync::Mutex<HeadFanout>,
    log: EventSink,
    /// The measurement instrument. Off in production and unaskable from the shipping
    /// launcher; see [`crate::frame_tee`]. When off, every `record` below is an
    /// `Option` discriminant test.
    tee: FrameTee,
}

/// The announcement, the legs owed it, and nothing else — **deliberately one
/// mutex** (round-2 F1).
///
/// The decision "this leg is owed *this* frame" is a joint statement about three
/// facts: the broker's verified head, the announcement it last forwarded, and what
/// the leg has already been sent. Reading them separately admits a stale pair — a
/// head read as A, then B binds and replaces the announcement, and A is what goes
/// out. Holding this one guard across the whole decision (and reading the head
/// *inside* it, which is the only lock this one is ever taken above) makes the
/// snapshot simultaneous, and makes the enqueues themselves ordered: two threads
/// cannot interleave their sends, so a leg's deliveries follow the head's own
/// progression and never run backwards.
#[derive(Default)]
struct HeadFanout {
    /// The `thread/started` frames this broker forwarded, **keyed by the thread each
    /// one names**, newest last.
    ///
    /// **Why not one slot** (round-3 F2). A single last-one-wins slot follows task
    /// scheduling, not the head's progression. The same broadcast reaches every leg
    /// on its own upstream socket, so two legs can process announcements A and B in
    /// opposite orders: leg 1 records B, leg 2 is descheduled and records A *after*
    /// it, and the slot is left naming a thread the session has already left. That is
    /// not merely a stale read — [`deliver_head`] then refuses to say anything at all
    /// (the announcement no longer names the head), and since no further announcement
    /// of B is coming, B is denied to every later `ccd` subscriber for the rest of the
    /// session.
    ///
    /// **The rule that was tried and measured false.** "Accept an announcement only
    /// when it names the current head" would make the slot monotonic — and would
    /// refuse every real announcement: the measured `/new` interleave puts the
    /// broadcast BEFORE the creation response (`fixtures/codex/thread-switch.jsonl:31`),
    /// so at write time the announced thread is never yet the head. Narrowing it to
    /// "refuse a RETIRED thread" survives that but leaves the window open — A is still
    /// the head when the delayed write lands, and B is retired-and-bound a moment
    /// later.
    ///
    /// Keying by thread removes the ordering question instead of arbitrating it. A
    /// late write of A cannot displace B because it does not share a slot with it, and
    /// [`deliver_head`] selects by the head rather than by arrival. **First bytes per
    /// thread win**: a second copy of a frame already recorded changes nothing, so a
    /// delayed leg's duplicate is inert by construction.
    ///
    /// Bounded by [`MAX_ANNOUNCEMENTS`], oldest evicted first.
    announced: Vec<Announcement>,
    /// Every leg that has forwarded its `initialize` as `ccd`, by connection.
    /// Registered in [`handle_text`] and removed when the leg's task ends.
    subscribers: std::collections::HashMap<ConnId, CcdSubscriber>,
}

/// How many `thread/started` frames [`HeadFanout::announced`] keeps.
///
/// **Two, because two is what can be live at once.** A creation is admitted one at a
/// time (`Creation::Pending` is a single slot and a competing `thread/start` is
/// refused), so at any instant there is the head's own announcement and at most one
/// successor's still waiting for its creation response. A third arrival means the
/// session moved on again, and the frame evicted to make room is one whose thread is
/// at or behind a head that has already been superseded twice — nothing
/// [`deliver_head`] would ever select. If that eviction is ever wrong the failure is
/// the pre-existing fail-closed one: nothing is said, and the next bind asks again.
const MAX_ANNOUNCEMENTS: usize = 2;

/// One `thread/started`, kept as it arrived.
struct Announcement {
    /// The thread the frame names, extracted the same way the `ccd` link extracts
    /// it (`params.threadId` or `params.thread.id`) so the head comparison in
    /// [`deliver_head`] is asking about the thread the reader will bind to.
    thread_id: String,
    /// **The original bytes.** Not a frame this broker composes: the s2c direction
    /// is a byte-exact passthrough, and a replay that reconstructed the
    /// announcement from the verified binding would be this broker asserting a
    /// shape rather than repeating one. Anything the app-server puts in that frame
    /// and the reader has not been told about survives the replay unchanged.
    raw: String,
}

/// A `ccd` leg waiting to be told the head.
struct CcdSubscriber {
    /// The leg's own queue into its `ws`. A channel and not the socket, because the
    /// socket belongs to that leg's task and this decision is made under a
    /// `std::sync::Mutex` by whichever task saw the head move — usually the TUI's.
    /// Unbounded: it carries at most one frame per head, and a head moves only when
    /// a person presses `/new`.
    ///
    /// **Every entry carries the thread it was queued for**, so the leg can revalidate
    /// it at the moment it sends rather than trusting a decision made when it was
    /// enqueued. See [`QueuedHead`].
    tx: tokio::sync::mpsc::UnboundedSender<QueuedHead>,
    /// **The proof this leg no longer needs repairing**: it has forwarded a
    /// `thread/started` of its own, so the app-server has it in the broadcast set
    /// and every later announcement arrives live. Nothing is delivered to a leg
    /// after this — which is also what keeps a *current* head from overtaking a
    /// successor the leg has already seen announced but that has not bound yet
    /// (the `ccd` link would take that for a `/new` back to the older thread).
    live_seen: bool,
    /// The head last delivered to this leg, so a redelivery of the same head is not
    /// sent twice. It is not a stop condition: the window between forwarding
    /// `initialize` and the server adding this connection to the broadcast set can
    /// span a whole `/new`, so a leg already given A can still be owed B.
    replayed: Option<String>,
}

/// A replay waiting in a leg's queue, **tagged with the head it was queued for**
/// (round-3 F1).
///
/// Enqueueing is not sending. The replay arm is the last of the three in
/// [`handle_connection`]'s `select!`, so between the enqueue and the send this leg
/// can pass a whole live exchange — including `/new`'s own announcement of B and the
/// resume response that adopts it. Sending the queued A afterwards is not a stale
/// no-op: the `ccd` reader takes an announcement naming neither its visit nor its
/// candidate as a person pressing `/new`, so it reads it as a switch BACK to A and
/// walks the link onto a thread the session has left.
///
/// `live_seen` does not cover this. It stops *future* enqueues to a leg that has seen
/// a broadcast of its own; it cannot reach into a queue that was written before.
///
/// So the decision is re-made where it is acted on: the leg drops the entry unless
/// `thread` is still the broker's verified head, read under the same guard the
/// enqueue was made under. See [`head_is`].
struct QueuedHead {
    /// The head this frame was queued to announce.
    thread: String,
    /// The original bytes ([`Announcement::raw`]).
    raw: String,
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
                head_fanout: std::sync::Mutex::new(HeadFanout::default()),
                log: Arc::new(|_| {}),
                tee: FrameTee::off(),
            }),
        }
    }

    /// Replace the audit-log sink.
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        Arc::get_mut(&mut self.ctx).expect("no clones yet").log = sink;
        self
    }

    /// Attach the verbatim frame recorder — the measurement instrument, off unless a
    /// live harness asked for it.
    ///
    /// Production never calls this with an enabled tee: the host builds one from
    /// [`crate::frame_tee::FrameTee::from_env`], and `codeconnect` never sets that
    /// variable (`the_shipping_launcher_cannot_enable_the_frame_tee`).
    pub fn with_frame_tee(mut self, tee: FrameTee) -> Self {
        Arc::get_mut(&mut self.ctx).expect("no clones yet").tee = tee;
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
                // The head fan-out's own release: a leg that is gone is owed nothing,
                // and its queue must not keep a slot in the table for the life of the
                // session. Unconditional — `remove` on a leg that never subscribed
                // (every TUI leg, and any `ccd` leg that closed before its
                // `initialize`) is a no-op.
                ctx.head_fanout
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .subscribers
                    .remove(&conn);
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
    // This leg's head-fan-out queue, minted for EVERY leg and published for none.
    // Minting it here rather than under the role test is what leaves the role test
    // load-bearing: [`handle_text`] is the only place the sender is handed to the
    // subscriber table, and dropping its `Role::Ccd` conjunct would put a TUI leg in
    // that table with a queue that is already wired to its socket. See
    // `a_tui_leg_is_neither_replayed_a_head_nor_able_to_trigger_one`.
    let (head_tx, mut head_rx) = tokio::sync::mpsc::unbounded_channel::<QueuedHead>();
    let mut head_tx = Some(head_tx);

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
                    let mut deliver = true;
                    if let Message::Text(t) = &msg {
                        ctx.tee.record(conn.0, "s2c", t);
                        ctx.threads.observe_server_frame(conn, t);
                        // **An unanswerable server request is answered here, not sent on.**
                        // Delivering a request the client will never be allowed to reply to
                        // strands the exchange the SERVER opened — the hang C7 was, and the
                        // same shape for every id-bearing non-approval s2c request. See
                        // `S2cDisposition`.
                        match caps.observe_server_frame(&ctx.threads, t) {
                            S2cDisposition::Deliver => {}
                            S2cDisposition::AnswerUpstream(frame) => {
                                (ctx.log)(&format!(
                                    "{role:?}: answer upstream (server request not serviceable \
                                     through this broker; not delivered) (conn {})",
                                    conn.0
                                ));
                                if up.to_upstream.send(Message::Text(frame)).await.is_err() {
                                    break;
                                }
                                deliver = false;
                            }
                            // Nothing further may cross a leg whose capability view can no
                            // longer be trusted — see `S2cDisposition::CloseLeg`.
                            S2cDisposition::CloseLeg(why) => {
                                (ctx.log)(&format!(
                                    "{role:?}: drop, close leg ({why}; s2c frame not delivered) \
                                     (conn {})",
                                    conn.0
                                ));
                                break;
                            }
                        }
                        note_announcement(&ctx, conn, t);
                    }
                    if !deliver {
                        continue;
                    }
                    ws.send(msg).await?;
                    // **Delivery on bind, not only on subscribe.** Either observer
                    // above can be the moment the (head, announcement) pair completes
                    // — `observe_server_frame` installs the binding from the creation
                    // RESPONSE while `note_announcement` records the broadcast, and
                    // the measured `/new` interleave puts the broadcast FIRST
                    // (`fixtures/codex/thread-switch.jsonl:31`). Asking after every
                    // server frame is what makes the completion order irrelevant.
                    deliver_head(&ctx);
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
                        // Recorded BEFORE classification, so a frame the broker refuses
                        // is captured too. Those are the ones a re-grounding needs most:
                        // the refusal log cannot name the parameter that caused it.
                        ctx.tee.record(conn.0, "c2s", &text);
                        if handle_text(
                            role, conn, &ctx, &caps, &mut ws, &up, &mut seen_initialize,
                            &mut head_tx, text,
                        )
                        .await?
                        {
                            break; // hostile/close disposition
                        }
                    }
                }
            }
            // The head owed to THIS leg, written by its own task. Last of the three
            // arms deliberately: a live server frame is never stale, so when both are
            // ready the passthrough goes first and the repair follows it.
            replay = head_rx.recv() => match replay {
                // **Revalidated HERE, not where it was queued** (round-3 F1). Between
                // the enqueue and this moment the two arms above can have carried a
                // whole `/new` past this leg; a queued predecessor sent now would read
                // as a switch BACK to it. Only the head still verified at send time
                // goes out. See [`QueuedHead`].
                //
                // The `ccd` reader binds on an announcement and the bind is idempotent
                // under the adapter's thread-namespaced identity keys, so a head it
                // already holds costs it nothing.
                Some(queued) => {
                    if head_is(&ctx, &queued.thread) {
                        ws.send(Message::Text(queued.raw)).await?;
                    } else {
                        (ctx.log)(&format!(
                            "{role:?}: dropped a stale head replay for {} (conn {conn})",
                            queued.thread
                        ));
                    }
                }
                // Unreachable while the leg lives: either this task still holds the
                // sender (never subscribed) or the subscriber table does, and the table
                // is only cleared after this loop has ended.
                None => break,
            },
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
    head_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<QueuedHead>>,
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

    // Noted before `decide` consumes the shape; acted on below, only if the
    // `initialize` was actually forwarded. See [`deliver_head`].
    let ccd_subscribing = matches!(role, Role::Ccd)
        && matches!(&shape, Shape::Request { method, .. } if method == "initialize");

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
                    // **A restored head is a head that BOUND** (round-3 F3). A switch
                    // that provably failed puts the predecessor back as the session's
                    // active thread, and that is exactly the event the fan-out repair
                    // exists to follow: a `ccd` leg that initialized while the creation
                    // was pending saw no bound head at subscribe time, and this rollback
                    // is the last thing that will ever make one true for it. Without the
                    // ask it waits for a server frame that the failed upstream is not
                    // going to send, and stays unbound for the life of the session.
                    deliver_head(ctx);
                }
                (ctx.log)(&format!(
                    "{role:?}: upstream gone (conn {conn}); closing leg"
                ));
                return Ok(true);
            }
            if ccd_subscribing {
                // Subscribe, then ask once. The registration is what makes every
                // LATER bind reach this leg; the ask covers the head that is already
                // bound when it arrives. `take` is what makes this once-per-leg
                // without a second flag — the reinitialization guard above has
                // already closed a leg that tried twice.
                if let Some(tx) = head_tx.take() {
                    ctx.head_fanout
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .subscribers
                        .insert(
                            conn,
                            CcdSubscriber {
                                tx,
                                live_seen: false,
                                replayed: None,
                            },
                        );
                    deliver_head(ctx);
                }
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

/// Keep a `thread/started` seen on the way out, so a later subscriber can be told
/// about it — and record that the leg carrying it needs no repairing.
///
/// The second half is not bookkeeping. `conn` is about to be sent these bytes by
/// its own passthrough, which means the app-server has this connection in the
/// broadcast set and every later announcement will reach it live. Marking it here
/// is what stops the ordinary launch replaying a frame the leg is already
/// receiving, and — the sharper case — what stops a *current* head being delivered
/// to a leg that has already seen its successor announced but not yet bound. The
/// `ccd` link reads an announcement naming neither its visit nor its candidate as
/// a person pressing `/new`, so delivering the older thread there would walk the
/// link backwards. See [`CcdSubscriber::live_seen`].
///
/// Notifications only. A `thread/started` arriving as anything else is not the
/// announcement, and this stores nothing rather than guessing.
fn note_announcement<F: UpstreamFactory>(ctx: &Arc<Ctx<F>>, conn: ConnId, text: &str) {
    let Shape::Notification { method, obj } =
        crate::message::classify_shape(&WsPayload::Text(text.to_string()))
    else {
        return;
    };
    if method != "thread/started" {
        return;
    }
    let Some(thread_id) = announced_thread_id(&obj) else {
        return;
    };
    let mut fanout = ctx
        .head_fanout
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // **Keyed by thread, first bytes win** (round-3 F2). A leg descheduled across a
    // `/new` re-presents an announcement another leg recorded already; recording it
    // again would be this task's arrival order overwriting the head's progression.
    // Having it change nothing is what makes a delayed leg inert rather than
    // destructive. See [`HeadFanout::announced`].
    if !fanout.announced.iter().any(|a| a.thread_id == thread_id) {
        fanout.announced.push(Announcement {
            thread_id,
            raw: text.to_string(),
        });
        // Oldest out. `remove(0)` on a two-element vector, once per `/new`.
        while fanout.announced.len() > MAX_ANNOUNCEMENTS {
            fanout.announced.remove(0);
        }
    }
    if let Some(sub) = fanout.subscribers.get_mut(&conn) {
        sub.live_seen = true;
    }
}

/// Whether `thread` is **still** the broker's verified head.
///
/// Read under the [`HeadFanout`] guard — the same lock, in the same order
/// (`head_fanout` → `threads`), that [`deliver_head`] takes to make the enqueue
/// decision. That is what makes the send-time revalidation in [`handle_connection`]
/// a statement about the head at the moment of sending rather than a second stale
/// snapshot.
fn head_is<F: UpstreamFactory>(ctx: &Arc<Ctx<F>>, thread: &str) -> bool {
    let _fanout = ctx
        .head_fanout
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ctx.threads
        .bound_thread()
        .is_some_and(|head| head.id == thread)
}

/// The thread a `thread/started` names.
///
/// Both spellings, and a disagreement between them names nothing: this is the
/// same extraction `ccd::codex_link::frame_thread_id` performs on the reading
/// side, so a frame the reader would refuse to bind from is one this refuses to
/// replay.
fn announced_thread_id(obj: &serde_json::Value) -> Option<String> {
    let params = obj.get("params")?;
    let flat = params
        .get("threadId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty());
    let nested = params
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty());
    match (flat, nested) {
        (Some(a), Some(b)) if a != b => None,
        (Some(id), _) | (None, Some(id)) => Some(id.to_string()),
        (None, None) => None,
    }
}

/// **Repair the fan-out for a `ccd` leg that subscribed after the announcement.**
///
/// The app-server broadcasts `thread/started` to the connections it has when the
/// thread is created. A `ccd` connection that did not exist then hears nothing,
/// ever — and it is structurally the late one: the daemon's control link is built
/// by a *registration*, the registration is sent by the supervisor the coordinator
/// becomes on `ready`, and `ready` already requires the TUI to be past `execve` and
/// alive. So the ordering the live gates observe is favourable scheduling, not a
/// happens-before edge, and unfavourable scheduling leaves the link permanently
/// unbound to the thread the session is actually on.
///
/// Rather than order the launch around it — which cannot be done without inverting
/// what `ready` means — the missed frame is delivered late, to the one connection
/// that missed it, by the one process that still holds it.
///
/// **The authority, stated exactly.** Two independent facts have to agree:
///
///   * the broker's own verified binding ([`ThreadBinding::bound_thread`]) — a
///     thread is the head only if this broker admitted its creation, correlated
///     the response on the connection that asked, and proved the server-resolved
///     `cwd` equal to the coordinator's launch cwd; and
///   * an announcement this broker actually forwarded, naming that same thread.
///
/// Neither alone would do. The binding has no announcement bytes, and composing
/// some would make this broker the author of a frame the reader treats as the
/// app-server's. The announcement alone is unverified broadcast content. Together
/// they are "the frame the app-server sent about the thread this launch owns",
/// which is precisely what the reader would have received had it been connected.
///
/// **Sent only to the `ccd` role.** The TUI is the client that *creates* threads;
/// it was there for the announcement by construction, and a second copy would be a
/// frame it never asked for. `ccd` is the observer, and re-announcing a thread it
/// already carries is the one case its reader is explicitly built for (the
/// reconnect path re-announces, and the bind is idempotent under the adapter's
/// thread-namespaced identity keys).
///
/// **Delivered when the head BINDS, not only when a leg subscribes** (round-2 F1).
///
/// Replaying once, at subscribe time, closed only half the hole. A leg forwards
/// `initialize` and is registered here, but the app-server adds it to the broadcast
/// set only when it has *answered* that request — and `ccd` sends `initialized`
/// after the answer. During `/new` the announcement can precede the creation
/// response (measured: `fixtures/codex/thread-switch.jsonl:31`), so a leg that
/// reconnects while a creation is pending sees no bound head to be replayed, misses
/// the live broadcast for the same reason, and under replay-on-subscribe would
/// never be told again. Asking after every server frame as well is what turns the
/// repair from "whatever was true at subscribe time" into "whatever becomes true
/// while this leg is here".
///
/// **The authority, stated exactly.** Two independent facts have to agree:
///
///   * the broker's own verified binding ([`ThreadBinding::bound_thread`]) — a
///     thread is the head only if this broker admitted its creation, correlated
///     the response on the connection that asked, and proved the server-resolved
///     `cwd` equal to the coordinator's launch cwd; and
///   * an announcement this broker actually forwarded, naming that same thread.
///
/// Neither alone would do. The binding has no announcement bytes, and composing
/// some would make this broker the author of a frame the reader treats as the
/// app-server's. The announcement alone is unverified broadcast content. Together
/// they are "the frame the app-server sent about the thread this launch owns",
/// which is precisely what the reader would have received had it been connected.
///
/// **They are read as one snapshot.** The head is read *inside* the [`HeadFanout`]
/// guard, which is the guard the announcement's only writer also takes. Reading
/// them apart admits the stale pair the round-2 review names: head observed as A,
/// then B binds and replaces the announcement, and the leg is handed A. The lock
/// order is `head_fanout` → `threads` and never the reverse — `note_announcement`
/// touches only the former, `observe_server_frame` only the latter.
///
/// **Sent only to the `ccd` role**, because the subscriber table only ever holds
/// `ccd` legs (see [`handle_text`]). The TUI is the client that *creates* threads;
/// it was there for the announcement by construction, and a second copy would be a
/// frame it never asked for.
///
/// Nothing is sent when there is no head, which is the ordinary launch's opening:
/// the real broadcast is still to come and this connection will be one of its
/// recipients — and when it is, `live_seen` retires it from this repair entirely.
fn deliver_head<F: UpstreamFactory>(ctx: &Arc<Ctx<F>>) {
    let mut delivered: Vec<ConnId> = Vec::new();
    let head_id = {
        let mut fanout = ctx
            .head_fanout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Inside the guard: this and the announcement below are ONE snapshot.
        let Some(head) = ctx.threads.bound_thread() else {
            return;
        };
        // Selected BY THE HEAD, not by arrival order (round-3 F2). Nothing is said
        // when no announcement names it: either none has arrived yet, or the ones
        // held name threads this broker has not bound — a `/new` whose creation
        // response has not landed. Not evidence about where the session is; the bind
        // that follows will ask again.
        let Some(raw) = fanout
            .announced
            .iter()
            .find(|a| a.thread_id == head.id)
            .map(|a| a.raw.clone())
        else {
            return;
        };
        for (conn, sub) in fanout.subscribers.iter_mut() {
            if sub.live_seen || sub.replayed.as_deref() == Some(head.id.as_str()) {
                continue;
            }
            // A closed queue means the leg's task has ended; its entry is removed by
            // that task, so there is nothing to do here but skip it.
            if sub
                .tx
                .send(QueuedHead {
                    thread: head.id.clone(),
                    raw: raw.clone(),
                })
                .is_err()
            {
                continue;
            }
            sub.replayed = Some(head.id.clone());
            delivered.push(*conn);
        }
        head.id
    };
    // Logged after the guard: the sink is caller-supplied and must never run under
    // this lock. The `(conn N)` pairing a live gate reads is preserved per leg.
    for conn in delivered {
        (ctx.log)(&format!(
            "Ccd: replayed thread/started for {head_id} to a late subscriber (conn {conn})"
        ));
    }
}
