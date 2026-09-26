//! The WS-over-UDS relay skeleton — the async transport edge.
//!
//! Two Unix-domain listeners (`tui.sock`, `ccd.sock`); each accepted connection gets its
//! own upstream app-server connection (N upstreams per leg — the `/resume` picker
//! opens a second one).
//!
//! * **The keyboard leg (`tui.sock`) is a passthrough.** Every frame the TUI sends —
//!   request, notification, response, malformed text, binary — reaches the app-server as
//!   it was written. The broker only watches it: the session learns where the head is and
//!   what is running ([`SessionThreads::observe_tui_request`]), and a keyboard answer to a
//!   phone-family approval takes the arbitration slot. The one frame it keeps back is a
//!   keyboard answer to an approval whose slot is already taken
//!   ([`LegCapabilities::arbitrate_tui`]).
//! * **The phone leg (`ccd.sock`) is classified** whole-message by the pure security core
//!   ([`crate::refusal`]) before any byte is forwarded.
//! * **Server→client** is a byte-exact passthrough to the keyboard. The phone is handed
//!   only the server requests it may answer ([`S2cDisposition`]).
//!
//! Every accepted connection is stamped with a monotonic [`ConnId`]. It is threaded into
//! BOTH directions — the c2s path reads it, and the s2c observer takes it as a parameter —
//! so correlation of a response to its request is CONNECTION-scoped and two connections of
//! the same role cannot answer each other's requests. The id appears in the `leg opened` /
//! `leg ended` / error log lines so an operator can follow one connection through
//! `broker.log`. The phone's role is anchored to its socket: a second `initialize` on
//! that leg fails closed.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::allowlist::Role;
use crate::frame_tee::FrameTee;
use crate::message::{RequestId, Shape, WsPayload};
use crate::refusal::{decide, Env, RelayAction};
use crate::response_capability::S2cDisposition;
use crate::response_capability::{LegCapabilities, ResponseArbiter};
use crate::session::{ConnId, SessionThreads, ThreadBinding};
use crate::upstream::{ws_config, UpstreamFactory, UpstreamWrite};

/// An audit-log sink. Every classification outcome and lifecycle event is reported here;
/// production wires this to `tracing`, tests to a recording buffer.
pub type EventSink = Arc<dyn Fn(&str) + Send + Sync>;

/// "Has this session bound a thread yet?", asked of a broker that
/// [`Broker::serve`] has already consumed.
///
/// The answer is the broker's own record ([`SessionThreads::thread_ever_bound`]) — a
/// keyboard `thread/start`, `thread/resume` or `thread/fork` answered on the connection
/// that asked with a thread — never a guess read back out of the log's text. It is the one
/// fact that separates "the pane came up and a session started" from "the pane came up
/// and the TUI exited without starting one", and the host reads it to decide which of
/// those to record.
pub type BoundThreadProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The broker: two listeners and a per-connection upstream
/// factory. Generic over the factory so integration tests drive captured frames through
/// a fake upstream with no live app-server.
pub struct Broker<F: UpstreamFactory> {
    tui_sock: PathBuf,
    ccd_sock: PathBuf,
    ctx: Arc<Ctx<F>>,
}

struct Ctx<F: UpstreamFactory> {
    factory: F,
    /// The shared one-use response-capability arbiter (fanout winner across all legs).
    /// Each connection wraps it in its own [`LegCapabilities`] view; the arbiter itself
    /// is session-wide so the first response to a fanned-out approval wins across legs.
    arbiter: Arc<ResponseArbiter>,
    /// Session-scoped thread state, shared across every connection: the keyboard's
    /// requests start a head move, the answer on the SAME leg settles it, and the phone's
    /// classifier reads the head and what is running.
    threads: SessionThreads,
    /// Mints the per-connection instance id ([`ConnId`]) handed to each accepted
    /// connection's task. A monotonic counter is enough: it never wraps in any realistic
    /// session (2^64 accepts), and it only has to distinguish connections that are alive
    /// at the same time from one another and from every earlier one.
    next_conn: AtomicU64,
    /// **The head fan-out**: the `thread/started` frames this broker forwarded, kept
    /// verbatim, and every `ccd` leg with the head it was last told.
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
    /// So the fan-out is repaired at the only place that still holds the fact — and
    /// extended to the one head move the app-server never announces at all, the
    /// keyboard's `/resume`. See [`owed_head`] for what is delivered, to whom, on what
    /// authority, and why the announcements, the head and the subscriber table are one
    /// lock rather than three.
    ///
    /// A `std::sync::Mutex` and not tokio's: every access is a field read or write
    /// with no await between lock and unlock.
    head_fanout: std::sync::Mutex<HeadFanout>,
    /// **How long a leg waits for the proof that its answer was written.**
    /// [`UPSTREAM_WRITE_BUDGET`], except where an integration test shortens it.
    write_budget: std::time::Duration,
    log: EventSink,
    /// The measurement instrument. Off in production and unaskable from the shipping
    /// launcher; see [`crate::frame_tee`]. When off, every `record` below is an
    /// `Option` discriminant test.
    tee: FrameTee,
}

/// The announcements, the legs told the head, and nothing else — **deliberately one
/// mutex**.
///
/// The decision "this leg is owed *this* frame" is a joint statement about three
/// facts: the broker's head, the announcements it forwarded, and what
/// the leg has already been told. Reading them separately admits a stale pair — a
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
    /// **Why not one slot.** A single last-one-wins slot follows task scheduling,
    /// not the head's progression. The same broadcast reaches every leg on its own
    /// upstream socket, so two legs can process announcements A and B in opposite
    /// orders: leg 1 records B, leg 2 is descheduled and records A *after* it, and
    /// the slot is left naming a thread the session has already left. That is
    /// not merely a stale read — the late subscriber is then handed a thread the session
    /// has left rather than the head's own announcement.
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
    /// [`owed_head`] selects by the head rather than by arrival. **First bytes per
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
/// **Two, because two is what matters at once**: the head's own announcement and at most
/// one successor's still waiting for the keyboard's move to settle. Announcements the
/// keyboard did not cause can evict the head's own; nothing is lost when they do, because
/// a head with no announcement held is told by the broker's own [`HEAD_NOTICE`] instead
/// of the app-server's bytes.
const MAX_ANNOUNCEMENTS: usize = 2;

/// **What the broker tells a `ccd` leg about the answer it just sent.**
///
/// A method-less `{id, result}` response is the one frame a leg sends whose fate it
/// cannot observe: the arbiter forwards the winner's bytes and drops every sibling's
/// with no reply and no close ([`crate::refusal::classify_response`]), so a leg that
/// lost a race with the keyboard and a leg whose answer actuated the command see the
/// identical nothing. Measured on real 0.153: the losing leg received zero frames and
/// stayed open. A daemon that has to record who resolved an approval cannot derive
/// that from silence, so the broker — the only party that knows — says it.
///
/// **`codeconnect/` and not a bare word.** The s2c direction is otherwise a byte-exact
/// passthrough of the app-server, and this is the broker speaking about itself. The
/// namespace is what keeps the two tellable apart for ever: no app-server method can
/// collide with it, and a reader that does not know this method treats it as the
/// unknown notification it is.
///
/// **`ccd` only.** The TUI leg is a real Codex client and gets the app-server's bytes
/// and nothing else; a frame this broker composed has no business in that stream. The
/// `ccd` leg is CodeConnect's own daemon, which is what makes it addressable.
pub const RESPONSE_DISPOSITION: &str = "codeconnect/responseDisposition";

/// **The method namespace this broker reserves for itself, both directions.**
///
/// Everything under it is composed here and nowhere else, which is what lets a `ccd`
/// reader tell this broker's word from the app-server's for ever. That guarantee is
/// only worth what the origin boundary enforces: the s2c direction is otherwise an
/// unconditional passthrough, so without a gate an app-server could simply *say*
/// [`RESPONSE_DISPOSITION`] about a pending request and be believed. A frame wearing
/// this prefix arriving FROM upstream is therefore never traffic to filter and pass on
/// — it is evidence about the far end, and it fails the leg closed. See
/// [`crate::response_capability::LegCapabilities::observe_server_frame`].
pub(crate) const CODECONNECT_NAMESPACE: &str = "codeconnect/";

/// **The head bound to a thread no announcement this broker holds names.**
///
/// `{"method":"codeconnect/head","params":{"threadId":"<id>"}}`, to `ccd` legs only.
///
/// The app-server broadcasts `thread/started` for a thread it creates and nothing for one
/// the keyboard resumes (`fixtures/codex/thread-switch.jsonl`: the `thread/resume`, its
/// answer, an unsubscribe of the thread left, and no announcement). The head follows the
/// resume ([`crate::session`]), so without a word from somewhere the daemon's link stays
/// on the thread the keyboard left, and the phone's turns are refused for naming it.
///
/// **Composed, because there are no bytes to repeat.** Where the app-server did announce
/// the head, its own frame is replayed instead (`owed_head`); this is the fallback for
/// the head nothing announced, and it carries the one fact the broker itself established:
/// a keyboard move naming this thread was answered with it. `threadId` is the spelling the
/// `ccd` reader already takes a frame's thread from.
///
/// Under the `codeconnect/` namespace, so it can never be an app-server method, and an
/// upstream frame claiming it closes the leg.
pub const HEAD_NOTICE: &str = "codeconnect/head";

/// The disposition frame for one answered `serverRequest`.
///
/// `delivered` is a statement about **bytes**, not about correctness: `true` means the
/// original response was written to the app-server socket, `false` means no complete
/// message was. Nothing weaker would be usable — an answer that provably never left is
/// one the daemon may report as not-taken, while an answer with no disposition at all
/// is one it must report as unknown. It is read from the upstream pump's write receipt
/// ([`crate::upstream::UpstreamWrite`]) and never from the in-process hand-off, which
/// happens before the socket is touched at all.
///
/// `winner` is **omitted unless this broker can name the leg that answered first.**
/// `delivered:false` on its own does not say who did: the answer may have lost to the
/// keyboard, lost to another `ccd` leg, been refused a capability it never held, or been
/// written into a socket that died. A reader that maps every `false` onto "answered at
/// the Mac" is wrong in three of those four. So the field appears only carrying a role
/// the arbiter has **confirmed** — a reservation whose write is still outstanding names
/// nobody — and its absence keeps the meaning the reader can safely act on: *something
/// else settled this, and this broker cannot say what.* It is never present alongside
/// `delivered:true` — a leg told its own bytes went out is not being told about somebody
/// else's.
///
/// `cause` is the other half of that, and it exists for one reason. The two ways a leg's
/// OWN write can fail to land are not the same fact and must not read as a lost race:
///
/// * `"write_failed"` — the pump answered `false`: the socket refused the write. The
///   arbiter reservation was released, so the approval is answerable again **at this
///   broker**. Who can actually take it is the daemon's business and the answer is "the
///   keyboard": `ccd` makes the claim terminal and retires the card, because the write
///   was admitted before it failed and tungstenite's `send` is a feed plus a flush, so
///   zero bytes is not provable. The release is what stops the keyboard being locked out
///   of an approval nobody answered — it is not a second chance for the phone.
/// * `"unconfirmed"` — the receipt was dropped, or the broker's own bound on the write
///   expired. Nobody can say whether the bytes went out, so the reservation stands and
///   the approval stays consumed.
///
/// `cause` and `winner` are mutually exclusive by construction: `winner` is read only on
/// a leg that did not forward, `cause` only on a leg that did. `delivered:false` with
/// NEITHER is the pre-existing shape and keeps its pre-existing meaning — this leg did
/// not forward and no confirmed winner can be named (a losing sibling whose winner is
/// still writing, a capability never held, a poisoned leg, a saturated arbiter).
fn response_disposition(
    thread_id: &str,
    id: &RequestId,
    delivered: bool,
    winner: Option<Role>,
    cause: Option<&str>,
) -> String {
    let mut params = serde_json::json!({
        "threadId": thread_id,
        "requestId": id.to_value(),
        "delivered": delivered,
    });
    if let Some(role) = winner {
        params["winner"] = winner_role(role).into();
    }
    if let Some(cause) = cause {
        params["cause"] = cause.into();
    }
    serde_json::json!({ "method": RESPONSE_DISPOSITION, "params": params }).to_string()
}

/// **How long the broker waits for the proof that an answer's bytes went out.**
///
/// The receipt is awaited inline in [`handle_text`], which `handle_connection` awaits
/// inline in turn — so an unbounded wait here parks the whole relay task: the leg cannot
/// notice its own client closing, cannot read another frame, and the daemon on the other
/// end is left to time out on its own with a card standing. That is the stall this bound closes.
///
/// **Ten seconds, and the number is a floor plus a ceiling, not a taste.**
///
/// The ceiling is `ccd`'s `DISPOSITION_BUDGET` (15 s in production): this bound must
/// expire FIRST, or the daemon gives up on a broker that was still going to answer and
/// files an unknown for a write the broker was about to confirm. Ten leaves five seconds
/// for the disposition frame itself to be composed and read, which is orders of magnitude
/// more than it needs.
///
/// The floor is what a real acknowledged write actually costs, measured on codex 0.153.2
/// through the live gate `measure_a_ccd_leg_answering_a_command_approval`: it times the
/// interval from writing the answer to reading the disposition — which contains this
/// whole wait plus the frame's own trip back — and prints it as
/// `MEASURED disposition round-trip`. The figure is **103.6 ms**, and it is an UPPER
/// bound rather than the cost: that gate polls at 100 ms, so all the measurement
/// establishes is that the round-trip finished inside the first tick. Ten seconds is
/// roughly a hundred of those ticks.
///
/// The margin is deliberate: `sink.send` is a feed plus a flush on a socket the
/// app-server may legitimately be slow to drain while it is doing something else, and
/// this bound is for a peer that has STOPPED, not one that is busy. The broker really
/// does wait on the app-server, so a missing disposition can be a slow peer — which is
/// exactly why the expiry reports `unconfirmed` rather than a proven failure.
const UPSTREAM_WRITE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// **The write was refused — at the socket, or one step before it.**
///
/// Two refusals wear this one word, and they do not prove the same thing.
///
/// A hand-off the pump's channel would not take is provably zero bytes: the envelope was
/// never dequeued and never reached `sink.send` at all.
///
/// A `sink.send` that came back `Err` is not. Tungstenite's send is a feed followed by a
/// flush, so a refused flush can follow a feed that already put bytes on the socket.
/// "Refused" there means the write did not complete, not that it left nothing behind.
///
/// What the two share is the verdict, and it is chosen for the party that can still act.
/// **The reservation is released either way**, so the keyboard in front of the same
/// prompt is not locked out of an approval that may otherwise never be answered by
/// anyone — that is a cost paid in the only direction where a wrong guess is recoverable.
/// The daemon, which cannot recall bytes that may have gone out, does the opposite with
/// the same frame: it retires the card as unknown rather than as not applied, and never
/// sends the answer again. Neither side claims to know more than it does.
pub const CAUSE_WRITE_FAILED: &str = "write_failed";

/// **Nobody can say whether the write landed**: the receipt was dropped (the pump died or
/// was cancelled mid-write) or [`UPSTREAM_WRITE_BUDGET`] expired. The reservation stands.
pub const CAUSE_UNCONFIRMED: &str = "unconfirmed";

/// The wire spelling of a recorded winner. Two words, and only ever these two: the
/// daemon switches on them to decide whether it tells the phone the Mac answered or
/// another phone did, and a third spelling would be a case it has no branch for.
fn winner_role(role: Role) -> &'static str {
    match role {
        Role::Tui => "tui",
        Role::Ccd => "ccd",
    }
}

/// One `thread/started`, kept as it arrived.
struct Announcement {
    /// The thread the frame names, extracted the same way the `ccd` link extracts
    /// it (`params.threadId` or `params.thread.id`) so the head comparison in
    /// [`owed_head`] is asking about the thread the reader will bind to.
    thread_id: String,
    /// **The original bytes.** Not a frame this broker composes: the s2c direction
    /// is a byte-exact passthrough, and a replay that reconstructed the
    /// announcement from the head would be this broker asserting a
    /// shape rather than repeating one. Anything the app-server puts in that frame
    /// and the reader has not been told about survives the replay unchanged.
    raw: String,
}

/// A `ccd` leg, and what it has been told about the head.
struct CcdSubscriber {
    /// Wakes the leg's own task to ask [`owed_head`]. A wake and not a queue of
    /// frames, because the socket belongs to that leg's task while the head moves
    /// under whichever task saw the keyboard's answer — usually the TUI's. **The frame
    /// is chosen when it is sent, not when the head moved**: a frame chosen earlier could
    /// be overtaken on this leg by a whole `/new` before it left, and the `ccd` reader
    /// takes an announcement naming neither its visit nor its candidate as a person
    /// moving — it would walk the link back onto a thread the session has left. Wakes
    /// coalesce, so a head that moves twice before the leg runs costs one delivery, of
    /// the head as it then is.
    wake: Arc<tokio::sync::Notify>,
    /// The thread this leg was last told to follow: a `thread/started` it carried live,
    /// or a head this broker sent it. The one record of what the leg was told. The leg is
    /// owed the head exactly when the head is bound and this names another thread.
    told: Option<String>,
}

impl<F: UpstreamFactory> Broker<F> {
    /// Build a broker. The one-use response-capability fanout arbiter is installed and
    /// fail-closed by construction: until a leg observes the soliciting `serverRequest`,
    /// every method-less response forwards zero upstream bytes.
    pub fn new(tui_sock: impl Into<PathBuf>, ccd_sock: impl Into<PathBuf>, factory: F) -> Self {
        let threads = SessionThreads::new();
        Self {
            tui_sock: tui_sock.into(),
            ccd_sock: ccd_sock.into(),
            ctx: Arc::new(Ctx {
                factory,
                arbiter: Arc::new(ResponseArbiter::new()),
                threads,
                next_conn: AtomicU64::new(1),
                head_fanout: std::sync::Mutex::new(HeadFanout::default()),
                write_budget: UPSTREAM_WRITE_BUDGET,
                log: Arc::new(|_| {}),
                tee: FrameTee::off(),
            }),
        }
    }

    /// Shorten [`UPSTREAM_WRITE_BUDGET`] for a test that has to watch it expire.
    ///
    /// A configuration seam, not a behaviour one: the code under test is identical and
    /// only the duration differs, the same way the `ccd` link's own
    /// `DISPOSITION_BUDGET` is shortened under `cfg(test)`. The broker's integration
    /// tests are a separate crate, so `cfg(test)` cannot reach them and a builder is
    /// what is left. Production never calls it and takes the constant.
    pub fn with_upstream_write_budget(mut self, budget: std::time::Duration) -> Self {
        Arc::get_mut(&mut self.ctx)
            .expect("no clones yet")
            .write_budget = budget;
        self
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

    /// Take a [`BoundThreadProbe`] on this broker's thread store.
    ///
    /// **Call it last, after every `with_*` builder method.** Those replace fields
    /// through `Arc::get_mut` and so require the context to be un-cloned; this
    /// clones it, which is the whole point — the handle has to outlive `serve`,
    /// which consumes the broker. Taking the probe first turns the next builder
    /// call into the "no clones yet" panic.
    ///
    /// A closure rather than a handle on the store itself, so the private context
    /// type stays private and no caller can reach past this one question.
    pub fn bound_thread_probe(&self) -> BoundThreadProbe {
        let ctx = Arc::clone(&self.ctx);
        Arc::new(move || ctx.threads.thread_ever_bound())
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
            // Mint this connection's instance id and announce it. The OPEN marker is what
            // lets a log reader attribute every later `(conn N)` line — and the close/error
            // lines below — to one connection.
            let conn = ConnId(ctx.next_conn.fetch_add(1, Ordering::Relaxed));
            (ctx.log)(&format!("{role:?}: leg opened (conn {conn})"));
            tokio::spawn(async move {
                let outcome = handle_connection(role, conn, stream, ctx.clone()).await;
                // The owning connection is gone. A keyboard move still in flight on it DID
                // reach the server, so its outcome is unknown and no head stands until the
                // keyboard binds one again; the connection's id ledger goes with it.
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
    // id into its thread (a response names only the bare id, never the thread).
    let mut caps = LegCapabilities::new(Arc::clone(&ctx.arbiter), Arc::clone(&ctx.log));
    // This leg's head-fan-out wake, minted for EVERY leg and published for none.
    // Minting it here rather than under the role test is what leaves the role test
    // load-bearing: [`handle_text`] is the only place it is handed to the subscriber
    // table, and dropping its `Role::Ccd` conjunct would put a TUI leg in that table
    // with a wake that is already wired to its socket. See
    // `a_tui_leg_is_neither_replayed_a_head_nor_able_to_trigger_one`.
    let wake = Arc::new(tokio::sync::Notify::new());
    let mut head_wake = Some(Arc::clone(&wake));

    loop {
        tokio::select! {
            biased;
            // server -> client: observed to settle this leg's keyboard move, to follow what
            // is running, to release the outstanding request ids this leg's responses
            // answer, and to register one-use response capabilities (approval fanout). Both
            // observers are PER CONNECTION — each correlates a bare response id against what
            // THIS connection solicited, keyed by the relay-minted `ConnId`, so two
            // connections of the same role cannot answer each other's requests.
            outbound = up.from_upstream.recv() => match outbound {
                Some(msg) => {
                    let mut deliver = true;
                    if let Message::Text(t) = &msg {
                        ctx.tee.record(conn.0, "s2c", t);
                        ctx.threads.observe_server_frame(conn, t);
                        // The keyboard is handed every frame. The phone is handed only the
                        // server requests it may answer; the rest are the keyboard's, and
                        // this leg neither delivers nor answers them. See `S2cDisposition`.
                        match caps.observe_server_frame(role, &ctx.threads, t) {
                            S2cDisposition::Deliver => {}
                            S2cDisposition::Withhold => {
                                (ctx.log)(&format!(
                                    "{role:?}: withhold (server request the phone cannot \
                                     answer; the keyboard answers it) (conn {conn})"
                                ));
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
                    // **Delivery on bind, not only on subscribe.** The observer above
                    // settles the head from the move's answer, so any server frame
                    // can be the moment it binds. Asking after every one is what
                    // makes the head reach a leg whichever leg carried the answer.
                    deliver_head(&ctx);
                }
                None => {
                    // Upstream (app-server) closed — deliberate close of the client leg.
                    (ctx.log)(&format!("{role:?}: upstream closed (conn {conn}); closing leg"));
                    break;
                }
            },
            // client -> server: the keyboard's frames pass through; the phone's are
            // classified whole before any byte is forwarded.
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
                    // The keyboard's bytes pass through, whatever they are.
                    Message::Binary(bytes) if role == Role::Tui => {
                        (ctx.log)(&format!("{role:?}: forward (keyboard passthrough) (conn {conn})"));
                        let write = UpstreamWrite::unacked(Message::Binary(bytes));
                        if !matches!(
                            tokio::time::timeout(ctx.write_budget, up.to_upstream.send(write)).await,
                            Ok(Ok(()))
                        ) {
                            (ctx.log)(&format!("{role:?}: upstream gone (conn {conn}); closing leg"));
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        // No phone-leg binary form (refusal matrix): zero bytes, close.
                        (ctx.log)(&format!("{role:?}: binary frame (conn {conn}); closing leg"));
                        break;
                    }
                    Message::Text(text) => {
                        // Recorded BEFORE anything is decided, so a frame the broker refuses
                        // is captured too: the refusal log cannot name the parameter that
                        // caused it.
                        ctx.tee.record(conn.0, "c2s", &text);
                        if handle_text(
                            role, conn, &ctx, &caps, &mut ws, &up, &mut seen_initialize,
                            &mut head_wake, text,
                        )
                        .await?
                        {
                            break; // hostile/close disposition
                        }
                    }
                }
            }
            // The head owed to THIS leg, chosen and sent by its own task. Last of the
            // three arms deliberately: a live server frame is never stale, so when both
            // are ready the passthrough goes first and the head follows it.
            () = wake.notified() => {
                if let Some(owed) = owed_head(&ctx, conn) {
                    ws.send(Message::Text(owed.frame)).await?;
                    // NOTE: `replayed thread/started` is asserted by live gates.
                    (ctx.log)(&if owed.replayed {
                        format!(
                            "{role:?}: replayed thread/started for {} to a late subscriber \
                             (conn {conn})",
                            owed.thread
                        )
                    } else {
                        format!("{role:?}: told the head {} (conn {conn})", owed.thread)
                    });
                }
            }
        }
    }
    Ok(())
}

/// Watch (keyboard) or classify (phone) one whole text message and act. Returns `Ok(true)`
/// when the leg must close.
#[allow(clippy::too_many_arguments)]
async fn handle_text<F, S>(
    role: Role,
    conn: ConnId,
    ctx: &Arc<Ctx<F>>,
    caps: &LegCapabilities,
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    up: &crate::upstream::UpstreamChannels,
    seen_initialize: &mut bool,
    head_wake: &mut Option<Arc<tokio::sync::Notify>>,
    text: String,
) -> anyhow::Result<bool>
where
    F: UpstreamFactory,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Parse the whole message exactly once.
    let payload = WsPayload::Text(text);
    let shape = crate::message::classify_shape(&payload);

    // Live reinitialization guard on the phone leg: role is anchored to the socket; a
    // second `initialize` on the same connection is an identity change and fails closed.
    if role == Role::Ccd {
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
    }

    // Noted before `decide` consumes the shape; acted on below, only if the
    // `initialize` was actually forwarded. See [`deliver_head`].
    let ccd_subscribing = matches!(role, Role::Ccd)
        && matches!(&shape, Shape::Request { method, .. } if method == "initialize");

    // Noted for the same reason, and read after the action has been carried out:
    // whether the answer went upstream is not known until the `Forward` arm has
    // actually written it. See [`RESPONSE_DISPOSITION`].
    //
    // Two ids, and they are deliberately not the same one. `responded` is EVERY leg's
    // answer — the arbiter reservation it may have taken belongs to whichever role sent
    // it, and a reservation is confirmed or released by the leg that took it, TUI
    // included. `answered` is the subset the broker speaks to: only a `ccd` leg is told
    // a disposition, because only CodeConnect's own daemon is addressable by a frame this
    // broker composed. With only the `ccd` id, a TUI winner's write
    // was never confirmed and could never be named.
    let responded: Option<RequestId> = match &shape {
        Shape::Response { id, .. } => Some(id.clone()),
        _ => None,
    };
    let answered: Option<RequestId> = match &responded {
        Some(id) if matches!(role, Role::Ccd) => Some(id.clone()),
        _ => None,
    };

    // The keyboard's frame is watched, not judged: the session learns from it before its
    // bytes go out, and it forwards. A keyboard move is remembered so a hand-off that
    // provably fails can put the head back.
    //
    // The phone's frame is classified. `env` is scoped so it is dropped before any await
    // (its `&dyn` trait objects are not `Send`, and the connection task must be `Send`).
    let mut move_id: Option<RequestId> = None;
    // A keyboard request or notification is logged under its method, rendered through the
    // audit-log grammar, so `broker.log` says what the keyboard did.
    let mut keyboard_method: Option<String> = None;
    let action = match role {
        Role::Tui => {
            if let Shape::Response { id, .. } = &shape {
                // The one keyboard frame the broker may keep: an answer to an approval
                // whose slot is already taken. See `LegCapabilities::arbitrate_tui`.
                if caps.arbitrate_tui(id) {
                    RelayAction::Forward {
                        note: "keyboard passthrough",
                    }
                } else {
                    RelayAction::DropLogKeepOpen {
                        note: format!(
                            "keyboard answer id={} to an approval already answered",
                            crate::redact::request_id(id)
                        ),
                    }
                }
            } else {
                if let Shape::Request { method, .. } | Shape::Notification { method, .. } = &shape {
                    keyboard_method = Some(crate::redact::method(method).into_owned());
                }
                if ctx.threads.observe_tui_request(conn, &shape) {
                    if let Shape::Request { id, .. } = &shape {
                        move_id = id.clone();
                    }
                }
                RelayAction::Forward {
                    note: "keyboard passthrough",
                }
            }
        }
        Role::Ccd => {
            let env = Env {
                capabilities: caps,
                threads: &ctx.threads,
                conn,
            };
            decide(&env, shape)
        }
    };
    // The receipt for the one message whose fate the arbiter and a leg are told about.
    // Minted only for a RESPONSE that is actually forwarded — `Forward` is the only
    // action that puts bytes on the wire at all, so for a response its absence is
    // exactly the proof that some other answer settled the request, and the only
    // condition under which this leg may be told who that was
    // ([`WriteOutcome::NotForwarded`]). Every other forward is handed over
    // unacknowledged and waits for nothing.
    let mut wrote_upstream: Option<tokio::sync::oneshot::Receiver<bool>> = None;
    // Set when this message's budget expires, or when the hand-off to the pump is
    // refused: the disposition (if this leg is owed one) is written, then the leg closes.
    let mut close_after_disposition = false;
    // **One deadline for one message, and it starts before the hand-off.** The journey
    // to the app-server's socket has two halves and both of them wait: the bounded
    // channel in front of the pump, and then the `sink.send` the pump performs. A
    // deadline that began at the receipt measured only the second, so a pump parked
    // inside `sink.send` with a full queue behind it parked the answer — and, because
    // `handle_connection` awaits this function inline, the whole leg — before any timer
    // was running. Taken here, the same budget covers both halves, which is what makes
    // it a bound on the answer rather than on one step of it.
    let write_deadline = tokio::time::Instant::now() + ctx.write_budget;
    // **The pump's channel would not take the envelope.** A separate flag and not a
    // receipt outcome, because there IS no receipt to read: the envelope carrying the
    // acknowledgement sender was dropped with the failed send, so the receiver would
    // resolve to a dropped-sender error — "nobody can say" — for a write that nobody
    // ever attempted. See the arm that sets it for why that is provably zero bytes.
    let mut handoff_refused = false;
    // **The queue would not take the envelope in time.** Distinct from the refusal above
    // and settled the other way: a hand-off that was cancelled leaves the envelope
    // unqueued, but from here that is indistinguishable from a receipt this leg gave up
    // waiting for, and the reading that is safe under both is the one that keeps the
    // reservation. A slot released for an answer that may have been written is how one
    // command gets actuated twice.
    let mut handoff_expired = false;
    match action {
        RelayAction::Forward { note } => {
            // The connection id is APPENDED AFTER the existing parenthesised note, never
            // spliced into it. Live gates assert the exact substrings
            // `Tui: forward (turn/start)`, `Ccd: forward (request allowlisted)`,
            // `Ccd: forward (notification allowlisted)`, so nothing may be inserted between
            // `forward (` and the note text. The trailing `(conn N)` is what lets a gate pair
            // a forward with the connection that made it, by real connection identity rather
            // than by role.
            let note = keyboard_method.as_deref().unwrap_or(note);
            (ctx.log)(&format!("{role:?}: forward ({note}) (conn {conn})"));
            // Recover the original bytes (nothing is injected) and forward.
            let text = match payload {
                WsPayload::Text(t) => t,
                WsPayload::Binary => unreachable!("text branch"),
            };
            let write = if responded.is_some() {
                let (write, receipt) = UpstreamWrite::acked(Message::Text(text));
                wrote_upstream = Some(receipt);
                write
            } else {
                UpstreamWrite::unacked(Message::Text(text))
            };
            // **Bounded, and bounded for every forward.** The hand-off is this leg's own
            // await: a leg parked on it cannot read its client, cannot notice it leaving,
            // and cannot serve the head it owes. The three outcomes are three different
            // facts about the bytes and are settled three different ways.
            match tokio::time::timeout_at(write_deadline, up.to_upstream.send(write)).await {
                // Handed over. Nothing is proven about the socket yet; that is what the
                // receipt below is for.
                Ok(Ok(())) => {
                    if ccd_subscribing {
                        // Subscribe, then ask once. The registration is what makes every
                        // LATER bind reach this leg; the ask covers the head that is
                        // already bound when it arrives. `take` is what makes this
                        // once-per-leg without a second flag — the reinitialization guard
                        // above has already closed a leg that tried twice.
                        if let Some(wake) = head_wake.take() {
                            ctx.head_fanout
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .subscribers
                                .insert(conn, CcdSubscriber { wake, told: None });
                            deliver_head(ctx);
                        }
                    }
                }
                // Refused: the pump is gone, so the envelope was never taken off the
                // queue and never fed to `sink.send`. Zero bytes, proven.
                Ok(Err(_)) => {
                    if let Some(id) = &move_id {
                        // ZERO bytes reached the server, so the keyboard's move provably
                        // did not happen: the head goes back to where it was.
                        ctx.threads.rollback_move(conn, id);
                        (ctx.log)(&format!(
                            "{role:?}: upstream send failed (conn {conn}); head move rolled \
                             back"
                        ));
                        // **A restored head is a head that BOUND.** A `ccd` leg that
                        // initialized while the move was in flight saw no head at
                        // subscribe time, and this rollback is the last thing that will
                        // ever make one true for it. Without the ask it waits for a server
                        // frame that the failed upstream is not going to send.
                        deliver_head(ctx);
                    }
                    (ctx.log)(&format!(
                        "{role:?}: upstream gone (conn {conn}); closing leg"
                    ));
                    // **A reservation this answer took is released here, for the same
                    // reason the head move above is rolled back.** Zero bytes proven
                    // is exactly what a socket that refused the write proves, arrived at
                    // one step earlier, so it earns the same verdict — the slot goes back
                    // and the approval is answerable again, by the keyboard in front of the
                    // same prompt. Left standing it would spend the request for the whole
                    // session: nobody answered it, so no `serverRequest/resolved` is coming
                    // to retire the card either.
                    if let Some(id) = &responded {
                        caps.release_write(role, id);
                    }
                    // The leg still closes — its upstream is gone — but it is told first.
                    handoff_refused = true;
                    close_after_disposition = true;
                }
                // The queue would not take it inside the budget. The upstream has stopped
                // behaving like the app-server, so the leg closes and takes it down — but a
                // `ccd` leg is told first, and told `unconfirmed`. Nothing is rolled back
                // here, and that is the fail-closed direction: a head move whose hand-off
                // expired is settled by the connection's own teardown, which leaves no head
                // rather than putting back one the keyboard may be leaving.
                Err(_) => {
                    (ctx.log)(&format!(
                        "{role:?}: upstream did not take the message within {:?} (conn \
                         {conn}); closing leg",
                        ctx.write_budget
                    ));
                    handoff_expired = true;
                    close_after_disposition = true;
                }
            }
        }
        // The same rule for the three refusal dispositions: note first, `(conn N)` after.
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
    // **Waited for here, after the action, before the frame — and BOUNDED.** The
    // receipt is the pump's statement that this answer's `ws.send` completed. Without a
    // deadline on this await, a `sink.send` parked on a
    // backpressured app-server socket parked the whole relay task with it: this
    // function is awaited inline from `handle_connection`, so the leg could not even
    // notice its client closing while the receipt was stuck.
    //
    // Every response is waited on, not only a `ccd` one, because the wait is what
    // decides the ARBITER's verdict on the reservation this leg took — and a TUI
    // reservation that is never confirmed is a winner no losing phone can be told
    // about. Only the `ccd` leg is then sent a frame about it.
    //
    // A hand-off that never completed is settled before any of that, because there is no
    // receipt to read: a refused send dropped the acknowledgement sender along with the
    // envelope, and an expired one left the envelope unqueued. Reading the receiver in
    // either case would report "nobody can say" about a write nobody attempted. Both
    // verdicts have been reached above; these are the words the daemon reads them in.
    //
    // The receipt continues on the SAME deadline the hand-off started, not a fresh one.
    // Two budgets in series would be two chances to wait the whole of it, and the number
    // is chosen against `ccd`'s own — the party that can say something has to give up
    // first, and it cannot do that twice.
    let outcome = if handoff_refused {
        WriteOutcome::Failed
    } else if handoff_expired {
        WriteOutcome::Unconfirmed
    } else {
        match wrote_upstream {
            None => WriteOutcome::NotForwarded,
            Some(receipt) => match tokio::time::timeout_at(write_deadline, receipt).await {
                // Proven written: the reservation becomes a confirmed win.
                Ok(Ok(true)) => {
                    if let Some(id) = &responded {
                        caps.confirm_write(role, id);
                    }
                    WriteOutcome::Delivered
                }
                // Refused at the socket. The write did not complete, which is not the
                // same as having left nothing behind — see [`CAUSE_WRITE_FAILED`] — but
                // the reservation goes back regardless, so the keyboard in front of the
                // same prompt can still answer. Holding a slot for a write that failed
                // is how an approval ends up answerable by nobody.
                Ok(Ok(false)) => {
                    if let Some(id) = &responded {
                        caps.release_write(role, id);
                    }
                    WriteOutcome::Failed
                }
                // The pump died or was cancelled mid-write. Nothing is proven either way,
                // so the reservation STAYS — see [`crate::response_capability`].
                Ok(Err(_)) => WriteOutcome::Unconfirmed,
                // The budget expired. Also unproven, and additionally session-fatal: a
                // write this broker cannot account for means the upstream is no longer
                // behaving like the app-server, so the leg closes and takes its upstream
                // down with it (dropping `up` ends the pump's outbound half). The
                // disposition below is still written first, so the daemon is told
                // `unconfirmed` rather than being left to time out on its own.
                Err(_) => {
                    (ctx.log)(&format!(
                        "{role:?}: upstream write unacknowledged after {:?} (conn {conn}); \
                     closing leg",
                        ctx.write_budget
                    ));
                    close_after_disposition = true;
                    WriteOutcome::Unconfirmed
                }
            },
        }
    };

    // **Told last, and told even by a leg that is closing.** Tearing the leg down is a
    // statement about the upstream, not about this answer: the arbiter has just reached a
    // verdict on the reservation, and the daemon holding the card is the one party that
    // has to act on it. Saying nothing would leave it to run out its own deadline and
    // file the weakest terminal there is, for an answer whose fate this broker knows
    // exactly. `bound_thread` names the request the way its reader
    // files it, and returns `None` for exactly the ids this leg cannot truthfully speak
    // about — in which case nothing is said.
    if let Some(id) = answered {
        if let Some(thread_id) = caps.bound_thread(&id) {
            // **`winner` is named only when somebody ELSE answered, and `cause` only
            // when this leg's own write did not land.** A leg that reached `Forward`
            // consumed the slot itself, so the winner the arbiter holds for it is this
            // very leg — naming it would tell the daemon the answer was overtaken when
            // what actually happened is that this leg's own write failed. That
            // misreading is closed by construction here: the two
            // fields come from disjoint arms.
            //
            // A leg that did NOT forward can be told who did, and only when the arbiter
            // has a CONFIRMED winner to name; a winner still writing, saturation and a
            // capability never held all record none, and all three are honestly
            // reported by saying nothing.
            let (delivered, winner, cause) = match outcome {
                WriteOutcome::Delivered => (true, None, None),
                WriteOutcome::Failed => (false, None, Some(CAUSE_WRITE_FAILED)),
                WriteOutcome::Unconfirmed => (false, None, Some(CAUSE_UNCONFIRMED)),
                // **A missing `winner` here says "unattributed", never "overtaken".**
                // This leg forwarded nothing, so its own answer is zero bytes either
                // way — but the arbiter names only a CONFIRMED winner, and a slot that
                // is merely reserved has none to name. That reservation can still be
                // released or expire, in which case nothing answered the request at
                // all. Saturation and a capability never held record none for their own
                // reasons. The daemon must read the silence as "this broker will not
                // say what settled it", which is the truth of all three, and not as a
                // race it lost — see `ccd`'s `note_response_disposition`.
                WriteOutcome::NotForwarded => (false, caps.recorded_winner(&id), None),
            };
            let frame = response_disposition(thread_id, &id, delivered, winner, cause);
            (ctx.log)(&format!(
                "{role:?}: response disposition delivered={delivered} winner={winner:?} \
                 cause={cause:?} id={id:?} (conn {conn})"
            ));
            // **Bounded, like the write it reports on.** This send is downstream, to the
            // daemon's own socket, and it is the last thing this leg does about the
            // answer — so a daemon that has stopped draining could park the leg here just
            // as surely as a stalled app-server parked it on the hand-off, and on a leg
            // that is already closing there would be nothing left to end the wait. The
            // budget is the same one, because the reader on the other end is timing this
            // whole exchange against a deadline of its own: past it the frame has no
            // reader left to inform, and the daemon's own budget files the terminal.
            match tokio::time::timeout(ctx.write_budget, ws.send(Message::Text(frame))).await {
                Ok(sent) => sent?,
                Err(_) => {
                    (ctx.log)(&format!(
                        "{role:?}: response disposition undeliverable after {:?} (conn \
                         {conn}); closing leg",
                        ctx.write_budget
                    ));
                    return Ok(true);
                }
            }
        }
    }
    Ok(close_after_disposition)
}

/// **What became of the bytes of one forwarded response.**
///
/// The four are not degrees of the same thing; they are four different states the
/// arbiter and the daemon must act on differently, and collapsing any two of them
/// strands an approval. See [`response_disposition`] for how each is spoken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    /// The pump acknowledged the write. The reservation is confirmed.
    Delivered,
    /// The write was refused: the pump answered `false`, or its channel would not take
    /// the envelope at all. The reservation was released. See [`CAUSE_WRITE_FAILED`] for
    /// which of the two proves zero bytes and why the release is right for both.
    Failed,
    /// Receipt dropped, or the write budget expired. Nothing is proven; the reservation
    /// stands.
    Unconfirmed,
    /// This leg forwarded nothing — it lost the race, or never held the capability. It
    /// took no reservation, so it has none to confirm or release.
    NotForwarded,
}

/// Keep a `thread/started` seen on the way out, so a later subscriber can be told
/// about it — and record that the leg carrying it has been told that thread.
///
/// The second half is not bookkeeping. `conn` is about to be sent these bytes by
/// its own passthrough, so the thread they name is the one its reader follows next.
/// Recording it is what stops the ordinary launch telling a leg a head it heard
/// announced — and what makes a late copy, one naming a thread the keyboard has since
/// moved away from, owe the leg the head again: [`deliver_head`], asked after this very
/// frame, sends it behind the announcement.
///
/// **A keyboard move's announcement cannot owe the leg its predecessor**, and that is
/// causality rather than a rule kept here. The keyboard's request is observed, and the
/// head set moving, before its bytes go upstream ([`crate::session`]); the app-server
/// announces the new thread only after reading them (`fixtures/codex/thread-switch.jsonl`:
/// the `thread/start` at line 30, the broadcasts at 31 and 33). So while a leg carries
/// that announcement no head is bound, and nothing is owed until the new thread itself
/// binds — which the leg was already told. An announcement no keyboard move caused names
/// a thread that is not the head, so the head follows it.
///
/// Notifications only. A `thread/started` arriving as anything else is not the
/// announcement, and this stores nothing rather than guessing.
///
/// An `ephemeral` thread's announcement is ignored whole: that thread never becomes the
/// head ([`crate::session`]) and the `ccd` reader does not follow it, so holding it
/// would only evict the head's own announcement from the two slots, and a leg that
/// heard it live has still not heard the head's.
fn note_announcement<F: UpstreamFactory>(ctx: &Arc<Ctx<F>>, conn: ConnId, text: &str) {
    let Shape::Notification { method, obj } =
        crate::message::classify_shape(&WsPayload::Text(text.to_string()))
    else {
        return;
    };
    if method != "thread/started"
        || obj.pointer("/params/thread/ephemeral") == Some(&serde_json::Value::Bool(true))
    {
        return;
    }
    let Some(thread_id) = announced_thread_id(&obj) else {
        return;
    };
    let mut fanout = ctx
        .head_fanout
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // **Keyed by thread, first bytes win.** A leg descheduled across a `/new`
    // re-presents an announcement another leg recorded already; recording it again
    // would be this task's arrival order overwriting the head's progression. Having
    // it change nothing is what makes a delayed leg inert rather than destructive.
    // See [`HeadFanout::announced`].
    if !fanout.announced.iter().any(|a| a.thread_id == thread_id) {
        fanout.announced.push(Announcement {
            thread_id: thread_id.clone(),
            raw: text.to_string(),
        });
        // Oldest out. `remove(0)` on a two-element vector, once per `/new`.
        while fanout.announced.len() > MAX_ANNOUNCEMENTS {
            fanout.announced.remove(0);
        }
    }
    if let Some(sub) = fanout.subscribers.get_mut(&conn) {
        sub.told = Some(thread_id);
    }
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

/// **Wake every `ccd` leg whose head is out of date.**
///
/// Called after every server frame, on subscribe, and when a failed hand-off puts a head
/// back — every moment the head can bind. It decides nothing about frames: each woken leg
/// asks [`owed_head`] itself, from its own task, at the moment it can send. Nothing is
/// woken when there is no head, which is the ordinary launch's opening: the real broadcast
/// is still to come and every `ccd` leg initialized by then will hear it.
fn deliver_head<F: UpstreamFactory>(ctx: &Arc<Ctx<F>>) {
    let fanout = ctx
        .head_fanout
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(head) = ctx.threads.bound_thread() else {
        return;
    };
    for sub in fanout.subscribers.values() {
        if sub.told.as_deref() != Some(head.as_str()) {
            sub.wake.notify_one();
        }
    }
}

/// A head one `ccd` leg is owed, ready to send.
struct OwedHead {
    thread: String,
    frame: String,
    /// `frame` is the app-server's own announcement repeated, not a [`HEAD_NOTICE`].
    replayed: bool,
}

/// **What the head is, told to one `ccd` leg that has not heard it.**
///
/// Two gaps, one repair.
///
/// *The late leg.* The app-server broadcasts `thread/started` once, to the connections it
/// has when the thread is created. A `ccd` connection that did not exist then hears
/// nothing, ever — and it is structurally the late one: the daemon's control link is built
/// by a *registration*, the registration is sent once the launch is `ready`, and `ready`
/// already requires the TUI to be past `execve` and alive. It can also be late inside a
/// `/new`: the app-server adds a connection to the broadcast set only once it has answered
/// its `initialize`, and the measured interleave puts the broadcast BEFORE the creation
/// response (`fixtures/codex/thread-switch.jsonl:31`), so a leg reconnecting into that
/// window misses the live frame and has no bound head to be told either — until the head
/// binds, which is why [`deliver_head`] asks at every bind and not only on subscribe.
///
/// *The unannounced move.* A keyboard `/resume` moves the head and the app-server
/// announces nothing, to anybody. Every `ccd` leg misses it, early or late.
///
/// **The authority** is the broker's own head ([`ThreadBinding::bound_thread`]): a thread
/// is the head only if a keyboard `thread/start`, `thread/resume` or `thread/fork` was
/// answered with it on the connection that asked. An ephemeral one never is. When the
/// broker also forwarded an announcement naming that thread, the app-server's own bytes
/// are what the leg is sent — exactly the frame it would have received had it been there.
/// Otherwise it is sent [`HEAD_NOTICE`], the broker's word for the one fact it holds.
///
/// **Once per move, and never backwards.** A leg is owed the head only when it was last
/// told another thread, live or by this broker ([`CcdSubscriber::told`]): a `/new` or a
/// fork the leg heard announced is followed from the announcement, a head that moved away
/// and back while the leg was not looking is not news, and a head is read at the moment it
/// is sent, so a thread the session passed through is never said. No head is bound while
/// a keyboard move is in flight, so a successor the leg heard announced is never
/// overtaken by its predecessor ([`note_announcement`]).
///
/// **They are read as one snapshot.** The head is read *inside* the [`HeadFanout`] guard,
/// which is the guard the announcements' only writer also takes, and the leg's record is
/// updated under it. The lock order is `head_fanout` → `threads` and never the reverse —
/// `note_announcement` takes both in that order, `observe_server_frame` only the latter.
///
/// **Sent only to the `ccd` role**, because the subscriber table only ever holds `ccd` legs
/// (see [`handle_text`]). The TUI is the client that *creates* and *resumes* threads; it
/// knows where it is, and a frame this broker composed has no business in its stream.
fn owed_head<F: UpstreamFactory>(ctx: &Arc<Ctx<F>>, conn: ConnId) -> Option<OwedHead> {
    let mut guard = ctx
        .head_fanout
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fanout = &mut *guard;
    let head = ctx.threads.bound_thread()?;
    let sub = fanout.subscribers.get_mut(&conn)?;
    if sub.told.as_deref() == Some(head.as_str()) {
        return None;
    }
    sub.told = Some(head.clone());
    let announced = fanout
        .announced
        .iter()
        .find(|a| a.thread_id == head)
        .map(|a| a.raw.clone());
    Some(OwedHead {
        replayed: announced.is_some(),
        frame: announced.unwrap_or_else(|| head_notice(&head)),
        thread: head,
    })
}

/// The [`HEAD_NOTICE`] frame for `thread`.
fn head_notice(thread: &str) -> String {
    serde_json::json!({ "method": HEAD_NOTICE, "params": { "threadId": thread } }).to_string()
}
