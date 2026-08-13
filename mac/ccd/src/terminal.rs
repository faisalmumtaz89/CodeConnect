//! The live-terminal carrier: a disposable tmux control-mode client.
//!
//! A [`TerminalHandle`] is the daemon's half of the Terminal tab. It owns a
//! `tmux -C attach` child speaking tmux's control-mode text protocol over plain
//! pipes: pane bytes arrive as `%output` lines and are forwarded to the phone;
//! the phone's keystrokes are delivered with `send-keys -H`, which types them
//! into the active pane as data — no byte a phone sends is ever parsed as a
//! tmux control-mode command, so the carrier itself cannot be steered into
//! another session or made to touch the server. (The shell in the pane is a
//! real shell and can of course run `tmux`; that is the terminal's whole
//! point.) The client owns nothing durable: the tmux server, the agent, and the
//! supervisor all outlive it, and closing it (or the daemon dying) detaches a
//! viewer without changing the running session.
//!
//! Control mode streams only what a pane prints *next*, so an attach would
//! otherwise show a blank screen until the pane happens to speak. The carrier
//! therefore paints: on the initial bind, and again whenever it follows the
//! focus to another pane, it asks tmux for that pane's current screen and
//! cursor and synthesises one chunk that clears, repaints and re-homes. From
//! the moment a target is published its `%output` is *held* rather than
//! forwarded, because those same bytes are in the grid the capture reads and
//! feeding both would paint the same text twice.
//!
//! The hold is not a discard. The capture's `%begin` marks how much had piled
//! up by the time tmux ran the command: that prefix is what became the captured
//! screen and is dropped, and everything held after it is flushed to the phone,
//! in order, the instant the paint lands. A snapshot that cannot be painted —
//! the pane vanished, the reply was not a screen, or the pane outran the
//! buffer's bound — costs the paint and never the bytes: the whole buffer goes
//! to the phone and the stream carries on live. And a screen that arrives after
//! the carrier has followed the focus on again is the wrong pane's: every bind
//! takes a new generation, the writer stamps the commands it issues with the
//! one it read, and only a reply for the bind still in force is painted.
//!
//! What an attach does *not* carry is history. The contract is the pane's
//! current screen and then its live bytes, so anything that had already
//! scrolled off the visible grid before the capture ran — including a burst
//! taller than the pane arriving between the publish and the capture — is
//! superseded by the snapshot exactly as pre-attach output is.
//!
//! Everything that could attach to the *wrong* session is settled before a byte
//! flows. The `session_uid` is resolved to one live session pinned to its
//! server epoch ([`protocol::tmux::resolve_owned_session`]); the client is
//! spawned with `-N -C -E -f ignore-size` against that session's internal id;
//! the client's own `%session-changed` must name that same id; and the attach
//! is re-verified by the spawned client's pid
//! ([`protocol::tmux::reverify_owned_client`]) before the handle is returned.
//!
//! The connection never blocks on the carrier. Input and resize are handed off
//! without waiting; teardown is one synchronous [`TerminalHandle::close`] (also
//! fired by `Drop`). Three tasks own the blocking work — a reader on the
//! child's stdout, a writer on its stdin, and a reaper that holds the child and
//! the leases. Every await that could block on the child (a pipe read or write,
//! a credit wait, a forward send) is raced against a shared close signal, so a
//! wedged pane, a credit-starved phone, or a full pipe stalls only its own task
//! and is released the instant the terminal closes. The two the *peer* can hold
//! open — the credit wait and the hand-over — spend the attachment's
//! [`PEER_STALL_BUDGET_DEADLINES`] as well, because a close signal only helps
//! once something decides to send one. Whichever task ends first
//! broadcasts the close, including the reaper when the client exits on its own,
//! so the tasks tear down together rather than one lingering. The reaper waits
//! on the client after signalling a kill; if that wait times out it drops the
//! child, which carries `kill_on_drop`, so the process is signalled on every
//! path and the runtime reaps the orphan.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use protocol::tmux::{self, ControlLine, OwnedSession, ResolveError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};

/// The most terminals that may be open at once, across every connection.
///
/// Each attachment costs a child process and its two pipes. The daemon's
/// descriptor budget already earmarks 64 WebSocket + 128 IPC sockets under
/// macOS's 256 soft limit, so this is deliberately small; the bound is
/// confirmed against a measured descriptor delta in the tests.
pub const MAX_TERMINAL_ATTACHMENTS: usize = 8;

/// The most pane bytes one `send-keys` command may carry. Each byte is one
/// hex argument on tmux's parser stack, which a maximal 16 KiB chunk
/// overflows; this size is measured safe with an order of magnitude to spare.
const SEND_KEYS_BYTES_PER_LINE: usize = 1024;

/// The most reply-block body the reader will hold while a snapshot is
/// assembled, charged as each row's bytes *plus the row entry holding them*.
/// The body that matters is a `capture-pane`, which is one line per visible
/// pane row, so the body to size against is the largest pane the wire admits:
/// 512x256 is 128 KiB of cells, which this clears eightfold — sevenfold with an
/// entry charged for each of those 256 rows too. What it does not
/// promise to hold is that pane fully attributed — `-e` can spend tens of bytes
/// of SGR on a cell, and a screen contrived to change attributes everywhere can
/// exceed this. That costs the paint and never the bytes: a body over the bound
/// is dropped and the held output is flushed exactly as for an `%error`, so the
/// bound's real job is refusing to grow without end if some future tmux
/// answered a command with something unending. Charging the entry is what makes
/// that refusal hold against an unending run of *empty* lines, which spend no
/// bytes and against a bytes-only count would push rows for ever. What it
/// bounds is the body line by line; the line itself is bounded by
/// [`CONTROL_LINE_BYTES`], which is where a line that never ends is refused.
const REPLY_BODY_BYTES: usize = 1024 * 1024;

/// What holding one body line costs beyond its bytes: the row entry in the
/// body. Every line charges this, so no line is free and the bound is reached
/// in a bounded number of lines whatever they contain.
const BODY_ROW_OVERHEAD: usize = std::mem::size_of::<Vec<u8>>();

/// The most one control-mode line may grow to before the reader stops adding to
/// it and starts discarding.
///
/// A line is what every other bound here is charged in, and until its newline
/// arrives there is no line to charge: `read_until` would size its buffer to
/// whatever the stream sends, so a client that never emitted a newline again
/// would grow the daemon's memory without end — no forgery needed, only a wedge.
///
/// The bound is [`REPLY_BODY_BYTES`] itself, because that is where the two
/// agree. A body line longer than the whole body bound is one
/// [`hold_body_line`] would refuse anyway, and refuses into the same overflow
/// latch, so capping the read here costs the body nothing it would have kept;
/// a tighter cap would be a second, different limit refusing lines the body
/// bound accepts. Legitimate lines sit orders of magnitude below it, measured
/// on tmux 3.7b: `%output` is chunked by the server — 2089 bytes was the
/// longest line seen across a 512x256 pane flooded with 500-byte rows, and
/// again with rows whose every byte octal-escapes to four — and the widest
/// capture row the wire's largest pane can give, 512 columns each changing
/// truecolour foreground, background and six attributes under `-e`, measured
/// 18662 bytes. A megabyte clears the worse of those fiftyfold.
const CONTROL_LINE_BYTES: usize = REPLY_BODY_BYTES;

/// The most pane output the reader will hold while a snapshot is assembled.
///
/// The bytes held are those printed between a target being published and its
/// screen being painted, which is milliseconds of one pane — the same order as
/// the screen itself. A pane that outruns this is one whose snapshot is no
/// longer worth waiting for, so the bound is spent on the bytes rather than the
/// paint: the buffer is flushed to the phone in order and the stream falls back
/// to live output. Generous enough that nothing but a runaway pane reaches it.
const HELD_OUTPUT_BYTES: usize = 1024 * 1024;

/// How long a closed client gets to detach on its own before it is killed,
/// and how long a kill gets to be reaped. Bounds only the carrier's background
/// teardown — the connection never waits on either.
const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the reader will wait for the phone to return output credit — with
/// nothing of its own left to hand over — before closing the attachment as a
/// slow consumer. A phone that has genuinely stopped rendering loses its
/// terminal rather than pinning one open and holding tmux's output buffer
/// indefinitely. Generous, because a foreground phone returns credit in
/// milliseconds.
///
/// It bounds the phone and only the phone. While the reader is blocked it cannot
/// see the session's control notifications (a pane change, an exit), and this is
/// not a bound on that blindness in general: waiting on the connection to
/// deliver what it was already given is not charged here (see [`forward`]), so
/// blindness caused by the daemon's own backpressure is not bounded here at all,
/// and blindness the *peer* manufactures is charged against
/// [`Delivery::peer_stall`] instead — which bounds the charge and not the
/// blindness itself, a gap [`forward`] states and puts a number on. Naming
/// either time a stalled consumer on this clock would be the daemon blaming the
/// phone for output the phone was never given, which is the one thing this close
/// code may never mean.
const OUTPUT_STALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The peer's share of one attachment, as a multiple of [`OUTPUT_STALL_DEADLINE`]:
/// **eight deadlines, which is four minutes of *charged* time at the production
/// value**. Only a wait with a terminal chunk's write in flight is charged, so
/// the wall clock a peer can stay blind for is longer — measured at about twenty
/// minutes against this loop's four ready arms. See [`Delivery::peer_stall`] for
/// what spends it and [`forward`] for that gap and its price.
///
/// Kept as a multiple rather than written out as a `Duration` so that shortening
/// the deadline for a test shortens this with it, and a test can say
/// `stall * PEER_STALL_BUDGET_DEADLINES` and mean the whole budget.
///
/// Named for the budget and not, as it once was, for forgiven deadlines. Nothing
/// counts deadlines any more: this is a quantity of *time*, and a deadline is
/// only the unit it is quoted in so that a shortened one carries the tests along
/// with it.
///
/// Chosen rather than derived, and chosen against the peer's side of the bargain
/// rather than the daemon's. Four minutes of charge is far past anything a
/// connection that is still moving bytes accrues, and long enough that a peer
/// which spends it has stopped being a phone that is merely slow. What makes the
/// mechanism sound is that the budget is finite, spans the attachment, and is
/// never given back — not that the number is eight, and not that the wall clock
/// it buys is four minutes, which it is not.
pub(crate) const PEER_STALL_BUDGET_DEADLINES: u32 = 8;

/// How long a peer may hold one *maximal* chunk's write open before the time
/// starts coming out of the attachment's budget. Smaller chunks get a
/// proportional share, so what this really sets is a floor drain rate:
/// [`protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT`] (256 KiB) in five seconds,
/// which is 51.2 KiB/s.
///
/// There is a reason it has to be a *rate* and not a flat grace. Elapsed time
/// alone cannot tell a hostile peer apart from a slow link: both park the reader
/// for the whole of every write, and that is the only thing the daemon can see.
/// Throughput can. A peer beating the floor is charged nothing whatever its chunk
/// sizes, and a phone that cannot beat 51.2 KiB/s cannot render a terminal
/// anyway; a peer that stalls while moving almost nothing is charged almost all
/// of it. Evading the charge therefore costs an attacker real bandwidth in
/// proportion to the blindness it buys, which is the honest place to stop —
/// **a peer that pays that price is a slow link, and nothing at this layer can
/// tell the two apart.**
///
/// 51.2 KiB/s counts *pane* bytes, which is what this is handed. The wire carries
/// them base64'd inside a JSON envelope, so the link underneath has to be doing
/// nearer 68 KiB/s — around 560 kbit/s — for a peer to sit at zero. That is still
/// far below any link a terminal is watchable over, but it is the number a phone
/// actually has to make, and quoting only the pane-byte figure would understate
/// what is being demanded of it by a third.
///
/// A quarter of [`crate::ws_server::WRITE_DEADLINE`] rather than a number of its
/// own, and strictly below it, which is the relationship the assertion under this
/// pins. That ordering is the whole defect this constant answers: the terminal's
/// old tolerance sat *above* the connection's write deadline, so a peer finishing
/// every write just inside the write deadline stayed inside the terminal's clock
/// too and was never charged at all. Now even a maximal chunk held for the whole
/// write deadline is charged three quarters of it.
/// Divided in nanoseconds, not seconds. `as_secs() / 4` is integer division, so a
/// write deadline of anything under four seconds would round this to *zero* —
/// every nanosecond a peer held a write charged with no allowance at all, which
/// closes every phone on the first busy pane. The assertions under this refuse
/// both ends of that.
pub(crate) const PEER_DRAIN_WINDOW: std::time::Duration =
    std::time::Duration::from_nanos((crate::ws_server::WRITE_DEADLINE.as_nanos() / 4) as u64);

/// A maximal chunk's allowance must stay strictly under the write deadline, or a
/// peer sending maximal chunks just inside that deadline would be charged
/// nothing and the bound would hold only for small chunks.
const _: () = assert!(PEER_DRAIN_WINDOW.as_nanos() < crate::ws_server::WRITE_DEADLINE.as_nanos());

/// And must not be zero, which would charge every peer for every instant it held
/// a write and close the honest ones first.
const _: () = assert!(PEER_DRAIN_WINDOW.as_nanos() > 0);

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64};

/// The stall deadline in force, so a test can shorten it without waiting the
/// production 30s. Zero (the default) means "use the constant".
#[cfg(test)]
static TEST_STALL_MS: AtomicU64 = AtomicU64::new(0);

/// Shortens the stall deadline for the duration of one test and restores the
/// production value on drop, so a test that panics part-way cannot leave every
/// test behind it running against a two-second deadline — which several of them
/// would fail, and for a reason that names the wrong test.
///
/// The restore alone is not enough, because the value is process-wide: a holder
/// also takes [`fixture_test_guard`], so no test runs beside a shortened
/// deadline it did not ask for. `ws_server`'s replay test reaches this, which is
/// why it is `pub(crate)`.
#[cfg(test)]
pub(crate) struct StallDeadline;

#[cfg(test)]
impl StallDeadline {
    pub(crate) fn shortened_to(deadline: std::time::Duration) -> StallDeadline {
        TEST_STALL_MS.store(deadline.as_millis() as u64, Relaxed);
        StallDeadline
    }
}

#[cfg(test)]
impl Drop for StallDeadline {
    fn drop(&mut self) {
        TEST_STALL_MS.store(0, Relaxed);
    }
}

/// The supersede wait in force, so a test can drive the give-up path without
/// waiting the production five seconds. Zero (the default) means "use the
/// constant".
#[cfg(test)]
static TEST_SUPERSEDE_MS: AtomicU64 = AtomicU64::new(0);

/// Shortens [`SUPERSEDE_WAIT`] for the duration of one test, restoring it on
/// drop for the same reason [`StallDeadline`] does: the value is process-wide.
#[cfg(test)]
pub(crate) struct SupersedeWait;

#[cfg(test)]
impl SupersedeWait {
    pub(crate) fn shortened_to(wait: std::time::Duration) -> SupersedeWait {
        TEST_SUPERSEDE_MS.store(wait.as_millis() as u64, Relaxed);
        SupersedeWait
    }
}

#[cfg(test)]
impl Drop for SupersedeWait {
    fn drop(&mut self) {
        TEST_SUPERSEDE_MS.store(0, Relaxed);
    }
}

/// Lets a test suppress the seed query so the pane never binds, to prove the
/// attach gate fails rather than returning a terminal that shows nothing.
#[cfg(test)]
static TEST_SKIP_SEED: AtomicBool = AtomicBool::new(false);

/// Suppresses the seed query for the duration of one test and restores it on
/// drop, for the same reason: a leaked suppression would fail every attach
/// after it.
#[cfg(test)]
struct SeedSuppressed;

#[cfg(test)]
impl SeedSuppressed {
    fn hold() -> SeedSuppressed {
        TEST_SKIP_SEED.store(true, Relaxed);
        SeedSuppressed
    }
}

#[cfg(test)]
impl Drop for SeedSuppressed {
    fn drop(&mut self) {
        TEST_SKIP_SEED.store(false, Relaxed);
    }
}

/// How many `%output` chunks the reader did not forward as they arrived —
/// dropped because nothing is bound yet or they belong to another pane, or held
/// because the bound pane's snapshot is still being assembled. A test waits on
/// this to act *causally*, once the pane has genuinely produced output the
/// carrier withheld, rather than on a race-prone timer.
#[cfg(test)]
static TEST_WITHHELD: AtomicUsize = AtomicUsize::new(0);

/// How many `%output` notifications the reader has seen at all, counted before
/// it decides what to do with them.
///
/// Deliberately upstream of the pane filter, unlike [`TEST_WITHHELD`]. A gate
/// released by "output the carrier withheld" is released by the very branch the
/// tests that use it are about, so removing that branch strands them at the
/// gate's own timeout and they fail on the barrier rather than on the claim.
/// This counts the arrival, which no routing decision can suppress.
#[cfg(test)]
static TEST_OUTPUT_SEEN: AtomicUsize = AtomicUsize::new(0);

/// How many snapshot replies — a capture or a cursor query — the reader has
/// answered. This is the barrier that says tmux *ran* a snapshot command, which
/// is a stronger fact than the writer having written one.
#[cfg(test)]
static TEST_SNAPSHOT_REPLIES: AtomicUsize = AtomicUsize::new(0);

/// The generation of the target the reader published most recently, so a test
/// can wait for a pane switch to have been bound before acting on it.
#[cfg(test)]
static TEST_PUBLISHED: AtomicU64 = AtomicU64::new(0);

/// Points the *next* snapshot pair at a pane id tmux cannot resolve, so
/// `capture-pane` answers `%error`. That is the wire condition of a pane that
/// vanished between the publish and the capture, reproduced without depending
/// on how tmux orders its own death notification against the reply. The pair it
/// applies to consumes it; [`VanishedTarget`] is what clears it on the paths
/// where no pair is ever issued.
#[cfg(test)]
static TEST_VANISHED_TARGET: AtomicBool = AtomicBool::new(false);

/// Arms [`TEST_VANISHED_TARGET`] for one test and disarms it on drop. The flag
/// is consumed by the next snapshot pair, but a test that panics before one is
/// issued — an attach that fails, a fixture that does not come up — would
/// otherwise leave it armed, and the next test's attach would paint nothing for
/// a reason that names the wrong test.
#[cfg(test)]
struct VanishedTarget;

#[cfg(test)]
impl VanishedTarget {
    fn arm() -> VanishedTarget {
        TEST_VANISHED_TARGET.store(true, Relaxed);
        VanishedTarget
    }
}

#[cfg(test)]
impl Drop for VanishedTarget {
    fn drop(&mut self) {
        TEST_VANISHED_TARGET.store(false, Relaxed);
    }
}

/// Points the *next cursor query alone* somewhere other than the pane its
/// capture named, leaving that capture to answer with a real screen. Both ways
/// a cursor reply can fail to be this pane's position are synthetic conditions
/// the wire will not produce on demand, and this is the only thing that drives
/// them:
///
/// - [`CursorTarget::unresolvable`] names a bare pane id tmux cannot resolve.
///   Measured on 3.7b, that makes `display-message` answer empty fields and no
///   error — the branch that paints rows with no cursor to place on them.
/// - [`CursorTarget::aimed_at`] names a different live pane, whose reply is
///   well-formed and names that pane. That is the shape of the reply a
///   *vanished* pane really produces: measured on 3.7b, a composite whose pane
///   stopped resolving is answered for the window's active pane, rc 0 and no
///   `%error`, so the position is another pane's and only the reported id says
///   so. Reproducing it by killing the bound pane would race tmux's own
///   ordering of the death notification against the reply; naming the other
///   pane outright puts the same bytes on the wire every run.
///
/// Consumed by the query it applies to; [`CursorTarget`] clears it on the paths
/// where none is ever issued.
#[cfg(test)]
static TEST_CURSOR_TARGET: Mutex<Option<String>> = Mutex::new(None);

/// Arms [`TEST_CURSOR_TARGET`] for one test and disarms it on drop, for the
/// same reason [`VanishedTarget`] does: a test that panics before the pair is
/// issued would otherwise leave the next test's snapshot without a cursor.
#[cfg(test)]
struct CursorTarget;

#[cfg(test)]
impl CursorTarget {
    /// Aim the next cursor query at `dest` — a real pane that is not the one
    /// being captured.
    fn aimed_at(dest: String) -> CursorTarget {
        *TEST_CURSOR_TARGET.lock().unwrap_or_else(|p| p.into_inner()) = Some(dest);
        CursorTarget
    }

    /// Aim the next cursor query at a pane id no tmux server hands out.
    fn unresolvable() -> CursorTarget {
        Self::aimed_at(NO_SUCH_PANE.to_string())
    }
}

#[cfg(test)]
impl Drop for CursorTarget {
    fn drop(&mut self) {
        *TEST_CURSOR_TARGET.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// The line cap in force, so a test can drive the overflow paths against a real
/// tmux, which will not produce a line anywhere near the production bound.
/// Zero (the default) means "use the constant".
#[cfg(test)]
static TEST_LINE_CAP: AtomicUsize = AtomicUsize::new(0);

/// Shortens the line cap for the duration of one test and restores the
/// production value on drop, so a test that panics part-way cannot leave every
/// test behind it reading against a cap their screens overflow.
#[cfg(test)]
struct LineCap;

#[cfg(test)]
impl LineCap {
    fn shortened_to(bytes: usize) -> LineCap {
        TEST_LINE_CAP.store(bytes, Relaxed);
        LineCap
    }
}

#[cfg(test)]
impl Drop for LineCap {
    fn drop(&mut self) {
        TEST_LINE_CAP.store(0, Relaxed);
    }
}

/// When a stall deadline was last armed — the credit wait's, or the hand-over's,
/// whichever went last. The deadline that closes a stalled carrier belongs to the
/// chunk that stalled, and the first chunk — the attach's paint — can be under
/// way while `open()` is still re-verifying the attach. A test measuring that
/// deadline therefore anchors here rather than to when its own `open()`
/// returned, which is a different instant by however long the re-verify probes
/// took.
#[cfg(test)]
static TEST_DEADLINE_ARMED: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// How many stall deadlines have expired since a test last zeroed this.
///
/// A test that means to drive the peer *without* ever expiring a deadline has to
/// be able to say so as an assertion rather than as a claim in its own comment:
/// the whole point of such a test is the premise, and a premise that is only
/// asserted in prose is one a later change can quietly falsify while the test
/// goes on passing for the wrong reason.
#[cfg(test)]
static TEST_DEADLINE_EXPIRIES: AtomicU64 = AtomicU64::new(0);

/// A pane id no tmux server hands out, for [`TEST_VANISHED_TARGET`] and
/// [`CursorTarget::unresolvable`].
#[cfg(test)]
const NO_SUCH_PANE: &str = "%4294967295";

/// How long a held command waits before it runs anyway, and how often it looks.
/// Generous, so a loaded machine cannot release a hold a test is still setting
/// up behind; bounded, so a test that forgets to open a gate fails on its own
/// assertion rather than hanging.
#[cfg(test)]
const GATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(test)]
const GATE_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// A place the writer stops until a test lets it past.
///
/// `arrived` counts the commands that have reached the gate, which is how a
/// test knows the writer has committed to a target before it changes that
/// target underneath. A command passes when `allowance` is [`usize::MAX`] — the
/// default, meaning no hold at all — when it can spend one of a finite
/// allowance, or once `open_at_output` `%output` notifications have reached the
/// reader, which is how a hold is released causally by the pane having actually
/// produced output rather than on a timer.
#[cfg(test)]
struct Gate {
    arrived: AtomicUsize,
    allowance: AtomicUsize,
    open_at_output: AtomicUsize,
}

#[cfg(test)]
impl Gate {
    const fn open() -> Gate {
        Gate {
            arrived: AtomicUsize::new(0),
            allowance: AtomicUsize::new(usize::MAX),
            open_at_output: AtomicUsize::new(usize::MAX),
        }
    }

    /// Reach the gate, and wait there until it lets this command past.
    async fn pass(&self) {
        self.arrived.fetch_add(1, Relaxed);
        let deadline = tokio::time::Instant::now() + GATE_DEADLINE;
        loop {
            let allowance = self.allowance.load(Relaxed);
            if allowance == usize::MAX
                || TEST_OUTPUT_SEEN.load(Relaxed) >= self.open_at_output.load(Relaxed)
            {
                return;
            }
            if allowance > 0
                && self
                    .allowance
                    .compare_exchange(allowance, allowance - 1, Relaxed, Relaxed)
                    .is_ok()
            {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(GATE_POLL).await;
        }
    }
}

/// The three commands a test can hold: the seed query that binds the pane, and
/// the capture and the cursor query that make up one snapshot.
#[cfg(test)]
static SEED_GATE: Gate = Gate::open();
#[cfg(test)]
static CAPTURE_GATE: Gate = Gate::open();
#[cfg(test)]
static CURSOR_GATE: Gate = Gate::open();

/// And the attach itself, held at its first instruction. [`TerminalHandle::open`]
/// resolves, spawns and waits for a client to bind, and what is worth proving
/// about that window is what the *connection* driving it does meanwhile — so
/// this gate is reached through [`OpenHold`], which `ws_server`'s tests hold.
#[cfg(test)]
static OPEN_GATE: Gate = Gate::open();

/// Holds one gate closed for the duration of a test and opens it on drop — so a
/// panicking test cannot leave the next one held.
#[cfg(test)]
struct GateHold(&'static Gate);

#[cfg(test)]
impl GateHold {
    /// Close `gate` until this hold lets commands through.
    fn close(gate: &'static Gate) -> GateHold {
        Self::arm(gate, usize::MAX)
    }

    /// Close `gate` until `chunks` output chunks have been withheld, so the
    /// held command runs only once the pane has produced output the carrier
    /// kept back — proving that window is real rather than assuming it.
    fn until_output(gate: &'static Gate, chunks: usize) -> GateHold {
        Self::arm(gate, chunks)
    }

    fn arm(gate: &'static Gate, open_at_output: usize) -> GateHold {
        TEST_WITHHELD.store(0, Relaxed);
        TEST_OUTPUT_SEEN.store(0, Relaxed);
        gate.arrived.store(0, Relaxed);
        gate.allowance.store(0, Relaxed);
        gate.open_at_output.store(open_at_output, Relaxed);
        GateHold(gate)
    }

    /// Let `commands` more past, and no more than that.
    fn allow(&self, commands: usize) {
        self.0.allowance.fetch_add(commands, Relaxed);
    }

    /// Open the gate for good, whatever was armed.
    fn release(&self) {
        self.0.allowance.store(usize::MAX, Relaxed);
    }

    /// How many commands have reached the gate since it was closed.
    fn arrived(&self) -> usize {
        self.0.arrived.load(Relaxed)
    }
}

#[cfg(test)]
impl Drop for GateHold {
    fn drop(&mut self) {
        self.0.allowance.store(usize::MAX, Relaxed);
        self.0.open_at_output.store(usize::MAX, Relaxed);
        self.0.arrived.store(0, Relaxed);
        TEST_WITHHELD.store(0, Relaxed);
        TEST_OUTPUT_SEEN.store(0, Relaxed);
    }
}

/// Holds every [`TerminalHandle::open`] at its first instruction until this
/// guard is dropped, so a caller can act *inside* an attach rather than race
/// tmux for the window. Every attach in the process is held, so a holder takes
/// [`fixture_test_guard`] like any other terminal test.
#[cfg(test)]
pub(crate) struct OpenHold(GateHold);

#[cfg(test)]
impl OpenHold {
    pub(crate) fn close() -> OpenHold {
        OpenHold(GateHold::close(&OPEN_GATE))
    }

    /// Wait until an attach has actually reached the hold, so what a caller does
    /// next is inside the window and not before it. Bounded like the gate
    /// itself, so a caller that never attaches fails its own assertion rather
    /// than hanging here.
    pub(crate) async fn reached(&self) {
        let deadline = tokio::time::Instant::now() + GATE_DEADLINE;
        while self.0.arrived() == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(GATE_POLL).await;
        }
    }
}

#[cfg(test)]
fn withheld() -> usize {
    TEST_WITHHELD.load(Relaxed)
}

#[cfg(test)]
fn snapshot_replies() -> usize {
    TEST_SNAPSHOT_REPLIES.load(Relaxed)
}

#[cfg(test)]
fn published_generation() -> u64 {
    TEST_PUBLISHED.load(Relaxed)
}

#[cfg(test)]
fn note_withheld() {
    TEST_WITHHELD.fetch_add(1, Relaxed);
}

#[cfg(test)]
fn output_seen() -> usize {
    TEST_OUTPUT_SEEN.load(Relaxed)
}

#[cfg(test)]
fn note_output_seen() {
    TEST_OUTPUT_SEEN.fetch_add(1, Relaxed);
}

#[cfg(not(test))]
fn note_output_seen() {}

#[cfg(not(test))]
fn note_withheld() {}

#[cfg(test)]
fn note_snapshot_reply() {
    TEST_SNAPSHOT_REPLIES.fetch_add(1, Relaxed);
}

#[cfg(not(test))]
fn note_snapshot_reply() {}

#[cfg(test)]
fn note_published(generation: u64) {
    TEST_PUBLISHED.store(generation, Relaxed);
}

#[cfg(not(test))]
fn note_published(_generation: u64) {}

#[cfg(test)]
fn note_deadline_armed() {
    *TEST_DEADLINE_ARMED
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(std::time::Instant::now());
}

#[cfg(not(test))]
fn note_deadline_armed() {}

/// When the stall deadline now in force was armed, or `None` if this carrier
/// has forwarded nothing.
#[cfg(test)]
fn deadline_armed() -> Option<std::time::Instant> {
    *TEST_DEADLINE_ARMED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
fn note_deadline_expired() {
    TEST_DEADLINE_EXPIRIES.fetch_add(1, Relaxed);
}

#[cfg(not(test))]
fn note_deadline_expired() {}

/// Zero the expiry count and hand back a reader for it. Taken by a test that
/// asserts on the count, so the number it reads is its own and not one left
/// behind by whatever ran before it.
#[cfg(test)]
fn count_deadline_expiries() -> impl Fn() -> u64 {
    TEST_DEADLINE_EXPIRIES.store(0, Relaxed);
    || TEST_DEADLINE_EXPIRIES.load(Relaxed)
}

#[cfg(test)]
async fn hold_seed_for_test() {
    SEED_GATE.pass().await
}

#[cfg(not(test))]
async fn hold_seed_for_test() {}

#[cfg(test)]
async fn hold_capture_for_test() {
    CAPTURE_GATE.pass().await
}

#[cfg(not(test))]
async fn hold_capture_for_test() {}

#[cfg(test)]
async fn hold_cursor_for_test() {
    CURSOR_GATE.pass().await
}

#[cfg(not(test))]
async fn hold_cursor_for_test() {}

#[cfg(test)]
async fn hold_open_for_test() {
    OPEN_GATE.pass().await
}

#[cfg(not(test))]
async fn hold_open_for_test() {}

/// The target one snapshot pair is asked for: the bound target, unless a test
/// has pointed it at a pane that cannot be resolved to exercise the `%error`
/// path. Takes the target by value so production pays nothing for the hook.
#[cfg(test)]
fn snapshot_target_for_test(dest: String) -> String {
    if TEST_VANISHED_TARGET.swap(false, Relaxed) {
        NO_SUCH_PANE.to_string()
    } else {
        dest
    }
}

#[cfg(not(test))]
fn snapshot_target_for_test(dest: String) -> String {
    dest
}

/// The target the cursor query of one snapshot pair is asked for: the same
/// target the capture used, unless a test has pointed this half alone
/// elsewhere. Takes the target by value so production pays nothing for the
/// hook.
#[cfg(test)]
fn cursor_target_for_test(dest: String) -> String {
    TEST_CURSOR_TARGET
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .unwrap_or(dest)
}

#[cfg(not(test))]
fn cursor_target_for_test(dest: String) -> String {
    dest
}

#[cfg(test)]
fn skip_seed_for_test() -> bool {
    TEST_SKIP_SEED.load(Relaxed)
}

#[cfg(not(test))]
fn skip_seed_for_test() -> bool {
    false
}

fn output_stall_deadline() -> std::time::Duration {
    #[cfg(test)]
    {
        let ms = TEST_STALL_MS.load(Relaxed);
        if ms > 0 {
            return std::time::Duration::from_millis(ms);
        }
    }
    OUTPUT_STALL_DEADLINE
}

fn supersede_wait() -> std::time::Duration {
    #[cfg(test)]
    {
        let ms = TEST_SUPERSEDE_MS.load(Relaxed);
        if ms > 0 {
            return std::time::Duration::from_millis(ms);
        }
    }
    SUPERSEDE_WAIT
}

/// The whole of one attachment's peer budget: four minutes of charged time in
/// production, and whatever a shortened deadline makes of it under test. Charged
/// and elapsed are not the same quantity — see [`forward`].
fn peer_stall_budget() -> std::time::Duration {
    output_stall_deadline() * PEER_STALL_BUDGET_DEADLINES
}

/// How much of a write's time its own bytes account for, at the floor rate
/// [`PEER_DRAIN_WINDOW`] sets. Time beyond this is what the peer is charged.
///
/// Widened in `u128` because a maximal chunk against the production window
/// overflows neither, but a shortened test window against a large chunk is not
/// worth having to think about twice.
fn drain_allowance(bytes: u64) -> std::time::Duration {
    let nanos = PEER_DRAIN_WINDOW.as_nanos() * u128::from(bytes)
        / u128::from(protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT);
    std::time::Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn control_line_bytes() -> usize {
    #[cfg(test)]
    {
        let bytes = TEST_LINE_CAP.load(Relaxed);
        if bytes > 0 {
            return bytes;
        }
    }
    CONTROL_LINE_BYTES
}

/// Why a carrier ended: the code the phone reasons about, and the sentence it
/// may show verbatim.
///
/// Paired because two different failures can share a code and still deserve
/// different words.
///
/// They share `slow_consumer` honestly: both of them *are* the consumer failing
/// to consume, one by not crediting and one by not reading. The ending that had
/// no business carrying that code was the daemon's own backpressure, and that one
/// no longer closes anything at all — the misclassification is removed by
/// removing the close, not by renaming it.
///
/// A second code would still be the better end state, and what stands in its way
/// is ownership rather than merit: `protocol::ws::terminal_close` is where wire
/// codes live, and minting one here would put a wire value outside the crate that
/// defines the wire.
///
/// The distinction reaches the user anyway, which is why the sentence is where it
/// is drawn: the app shows the daemon's `reason` verbatim whenever there is one
/// and falls back to a per-code sentence only when there is not — see
/// `TerminalCarrier.describe(code:reason:)`, which is where that is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseCause {
    pub code: &'static str,
    pub reason: &'static str,
}

/// Every ending this carrier records, code and sentence together.
///
/// Constants rather than a function from code to sentence, because a function
/// would have to answer for codes this carrier never produces — and would let a
/// new ending be recorded with no sentence written for it, falling through to
/// whatever the default said.
pub mod cause {
    use super::CloseCause;
    use protocol::ws::terminal_close as tc;

    /// The ordinary end: the reader saw the stream stop, so the pane or the
    /// client is gone. Also the answer for a carrier that has not closed at all,
    /// because that is what the connection is about to be told anyway.
    pub const SESSION_EXITED: CloseCause = CloseCause {
        code: tc::SESSION_EXITED,
        reason: "the session ended",
    };

    /// The session's active window moved; this single-window carrier does not
    /// follow it.
    pub const WINDOW_CHANGED: CloseCause = CloseCause {
        code: tc::WINDOW_CHANGED,
        reason: "the session's window changed",
    };

    /// A later attach took this session's terminal over. Noted on the *holder*
    /// before its close is fired, so the connection that owns it reads this
    /// rather than the generic ending when its stream stops.
    pub const SUPERSEDED: CloseCause = CloseCause {
        code: tc::SUPERSEDED,
        reason: "another terminal took this session over",
    };

    /// The client announced a session other than the one the attach verified.
    pub const IDENTITY_MISMATCH: CloseCause = CloseCause {
        code: tc::IDENTITY_MISMATCH,
        reason: "the session changed under the terminal",
    };

    /// A control-mode line the reader could not frame, so what is on the pipe is
    /// no longer the protocol.
    pub const PROTOCOL_ERROR: CloseCause = CloseCause {
        code: tc::PROTOCOL_ERROR,
        reason: "the tmux client stopped speaking the control protocol",
    };

    /// Everything handed over has been written out and the phone has stopped
    /// granting credit for it. See [`super::ForwardEnd::CreditStarved`].
    pub const CREDIT_STARVED: CloseCause = CloseCause {
        code: tc::SLOW_CONSUMER,
        reason: "the phone stopped acknowledging terminal output",
    };

    /// The peer held the connection's writes across the whole of the
    /// attachment's forgiveness. See [`super::ForwardEnd::SocketNotDraining`].
    pub const SOCKET_NOT_DRAINING: CloseCause = CloseCause {
        code: tc::SLOW_CONSUMER,
        reason: "the phone stopped taking terminal output from its socket",
    };
}

/// Why a terminal could not be opened. Each maps to a `terminal_close` code.
#[derive(Debug)]
pub enum OpenError {
    /// No live CodeConnect session carries the uid.
    NotHosted,
    /// The uid resolved ambiguously, or the session changed during attach.
    IdentityMismatch(String),
    /// tmux could not be run, or answered indeterminately.
    Unavailable(String),
    /// The Mac's global cap on open terminals, and only that. A session that
    /// already has one is [`OpenError::SessionBusy`] or a takeover.
    AttachmentLimit(String),
    /// The terminal this session already had was asked to close so this attach
    /// could take it over, and it had not let go of the lease inside
    /// [`SUPERSEDE_WAIT`]. Retrying is the answer.
    SessionBusy(String),
}

impl OpenError {
    /// The stable wire code for a [`protocol::ws::terminal_close`].
    pub fn close_code(&self) -> &'static str {
        use protocol::ws::terminal_close::*;
        match self {
            OpenError::NotHosted => SESSION_NOT_HOSTED,
            OpenError::IdentityMismatch(_) => IDENTITY_MISMATCH,
            OpenError::Unavailable(_) => TMUX_UNAVAILABLE,
            OpenError::AttachmentLimit(_) => ATTACHMENT_LIMIT,
            OpenError::SessionBusy(_) => SESSION_BUSY,
        }
    }

    pub fn reason(&self) -> String {
        match self {
            OpenError::NotHosted => "no live session carries that id".to_string(),
            OpenError::IdentityMismatch(why)
            | OpenError::Unavailable(why)
            | OpenError::AttachmentLimit(why)
            | OpenError::SessionBusy(why) => why.clone(),
        }
    }
}

impl From<ResolveError> for OpenError {
    fn from(err: ResolveError) -> Self {
        match err {
            ResolveError::NotHosted => OpenError::NotHosted,
            ResolveError::IdentityMismatch(why) => OpenError::IdentityMismatch(why),
            ResolveError::Unavailable(why) => OpenError::Unavailable(why),
        }
    }
}

/// How long a taking-over attach waits for the terminal it superseded to let go
/// of the session's lease.
///
/// The wait is not the close — the close is fired synchronously and the holder's
/// tasks wake at once. It is the *reap*: the lease belongs to the reaper, which
/// releases it only after the disposable tmux client has been killed and waited
/// on, and that is the whole point of holding it there (two clients on one
/// session must never overlap). Five seconds is an order of magnitude above what
/// killing a `tmux -C` costs and well under the phone's own attach patience, so
/// a takeover that hits it is a genuinely stuck child rather than a slow one.
pub const SUPERSEDE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Hands out the two leases every attachment needs: a global slot, and the
/// one-terminal-per-session guarantee. Cheap to clone (it is all `Arc`).
#[derive(Clone)]
pub struct TerminalLeases {
    /// The global cap, as counting permits.
    slots: Arc<Semaphore>,
    /// The session uids that currently have a terminal open, and how to close
    /// the terminal that has each. A `std` mutex, not tokio's, because the
    /// guard releases in `Drop`, which is not async.
    active: Arc<Mutex<HashMap<String, LeaseHolder>>>,
    /// Fired on every lease release, so a supersede waits on the reap rather
    /// than polling for it.
    freed: Arc<tokio::sync::Notify>,
}

/// What a supersede needs to reach whichever carrier holds a session's lease:
/// the very close signal, credit semaphore and cause slot that carrier's own
/// [`TerminalHandle::close`] uses.
///
/// Registered *with* the lease rather than after it, which is why
/// [`TerminalHandle::open`] builds all three before it acquires: a holder
/// installed a moment later leaves a window in which the lease is held by
/// something a supersede cannot close, and that window is exactly the
/// reconnect race this whole mechanism exists for.
#[derive(Clone)]
struct LeaseHolder {
    close: watch::Sender<bool>,
    credit: Arc<Semaphore>,
    cause: Arc<Mutex<Option<CloseCause>>>,
}

impl LeaseHolder {
    /// Tell this holder it has been superseded and tear it down — the same two
    /// steps [`TerminalHandle::close`] takes, plus the cause, so the connection
    /// that owns it puts `superseded` on the wire when its stream ends rather
    /// than the generic "the session ended".
    fn supersede(&self) {
        note_cause(&self.cause, cause::SUPERSEDED);
        let _ = self.close.send(true);
        self.credit.close();
    }

    /// A holder with nothing behind it, for tests that exercise the lease
    /// arithmetic without a carrier. Superseding one is a no-op, which is what
    /// makes it useful: the lease is then provably released by the test's own
    /// `drop` and never by a carrier reacting to the close.
    #[cfg(test)]
    fn inert() -> LeaseHolder {
        LeaseHolder {
            close: watch::channel(false).0,
            credit: Arc::new(Semaphore::new(0)),
            cause: Arc::new(Mutex::new(None)),
        }
    }
}

impl TerminalLeases {
    pub fn new() -> Self {
        TerminalLeases {
            slots: Arc::new(Semaphore::new(MAX_TERMINAL_ATTACHMENTS)),
            active: Arc::new(Mutex::new(HashMap::new())),
            freed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Acquire both leases for `session_uid`, or say why not. The returned
    /// guard releases both when dropped. The reaper task holds it for the
    /// child's whole life, so a session's slot frees only once its previous
    /// client is genuinely dead — never while two clients could overlap.
    ///
    /// **The session's incumbent is decided before the global cap is touched,
    /// and that order is load-bearing.** Taking the permit first meant a
    /// saturated Mac answered [`OpenError::AttachmentLimit`] without ever
    /// looking at the session — including when one of the eight slots was held
    /// by this very session's stale terminal. [`Self::acquire_superseding`]
    /// only supersedes on [`OpenError::SessionBusy`], so the takeover never
    /// ran and the reconnecting phone was locked out of its own session for
    /// exactly the window supersede exists to close. Checking the map first
    /// also means no caller ever waits for an incumbent's release while
    /// holding a permit that release needs back.
    fn acquire(&self, session_uid: &str, holder: LeaseHolder) -> Result<Lease, OpenError> {
        let permit = {
            // One critical section for both, so nothing can slip between the
            // "is it free" and the claim. `try_acquire_owned` never blocks, so
            // holding the map's guard across it is a handful of instructions.
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            if active.contains_key(session_uid) {
                return Err(OpenError::SessionBusy(
                    "the previous terminal on this session is still closing".to_string(),
                ));
            }
            let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
                OpenError::AttachmentLimit(format!(
                    "the Mac already has {MAX_TERMINAL_ATTACHMENTS} terminals open"
                ))
            })?;
            active.insert(session_uid.to_string(), holder);
            permit
        };
        Ok(Lease {
            permit: Some(permit),
            active: Arc::clone(&self.active),
            freed: Arc::clone(&self.freed),
            session_uid: session_uid.to_string(),
        })
    }

    /// Acquire for `session_uid`, taking the session's terminal over from
    /// whoever already has one.
    ///
    /// A lease held by a *dead* connection is the case this exists for. iOS
    /// backgrounds the app and the socket dies without a FIN; the daemon has no
    /// read deadline and a quiet pane arms no stall, so the holder can outlive
    /// its phone by minutes. Refusing the reconnecting phone its own session's
    /// terminal for that window — which is what the lease used to do — is a
    /// lockout with no way through but waiting for TCP to notice.
    ///
    /// So the newcomer wins, and the incumbent is told why: the holder is closed
    /// as `superseded`, and this waits `within` for the reaper to hand the lease
    /// back. Losing the terminal is the right way round because the attach that
    /// is *asking* is the one a human is looking at, and a holder that is still
    /// alive gets a close it can act on rather than a stream that stops.
    ///
    /// The wait can still lose — to a stuck child, or to a third attach taking
    /// the freed lease first — and then this refuses with
    /// [`OpenError::SessionBusy`], which is a different wire code from the
    /// global cap precisely so the phone can retry the one and not the other.
    ///
    /// A full Mac does not stop any of this: [`Self::acquire`] answers
    /// `SessionBusy` for a session that has an incumbent whether or not a
    /// global permit is free, and the incumbent's release hands back *both*
    /// leases, so the newcomer finds a slot waiting for it. `AttachmentLimit`
    /// is left meaning only what it says — the Mac is full of terminals on
    /// *other* sessions — which is the one case a retry cannot fix.
    async fn acquire_superseding(
        &self,
        session_uid: &str,
        holder: LeaseHolder,
        within: std::time::Duration,
    ) -> Result<Lease, OpenError> {
        match self.acquire(session_uid, holder.clone()) {
            Err(OpenError::SessionBusy(_)) => {}
            settled => return settled,
        }
        let incumbent = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session_uid)
            .cloned();
        if let Some(incumbent) = incumbent {
            incumbent.supersede();
        }
        // Registered before the lease is looked at, and that order is the whole
        // of the correctness here: a `notified()` future subscribes when it is
        // first polled, so checking first and awaiting after would miss a
        // release that landed in between and wait out the whole bound for a
        // lease that was already free.
        let _ = tokio::time::timeout(within, async {
            loop {
                let freed = self.freed.notified();
                tokio::pin!(freed);
                freed.as_mut().enable();
                if !self.held(session_uid) {
                    return;
                }
                freed.await;
            }
        })
        .await;
        self.acquire(session_uid, holder)
    }

    /// Does some terminal hold `session_uid`'s lease right now?
    pub(crate) fn held(&self, session_uid: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(session_uid)
    }

    /// Hold a session's lease from outside any carrier, for a test that needs an
    /// incumbent nothing will ever release.
    ///
    /// The stuck-child case, which has no other stimulus: a real carrier hands
    /// its lease back the moment it is closed, and a test that could not hold
    /// one could only reach the give-up path by waiting out the production
    /// bound.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self, session_uid: &str) -> LeaseHold {
        LeaseHold(
            self.acquire(session_uid, LeaseHolder::inert())
                .expect("the lease is free"),
        )
    }
}

/// A lease held by a test; releases on drop like any other.
#[cfg(test)]
pub(crate) struct LeaseHold(#[allow(dead_code)] Lease);

impl Default for TerminalLeases {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds both leases; releases them on drop.
struct Lease {
    /// An `Option` only so [`Drop`] can choose *when* the global permit goes
    /// back: inside the same critical section that removes the session from
    /// `active`, and before anyone is woken. A supersede re-acquires both
    /// leases, and one that saw the session free while the permit was still out
    /// would answer `attachment_limit` on a Mac that was a microsecond away from
    /// having room — the very refusal the acquire order exists to keep honest.
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    active: Arc<Mutex<HashMap<String, LeaseHolder>>>,
    freed: Arc<tokio::sync::Notify>,
    session_uid: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        {
            // Both leases go back inside *one* critical section, the same way
            // [`TerminalLeases::acquire`] takes them both inside one. The map
            // guard is what makes the handback atomic to anyone who can see it:
            // an acquirer reaches the "is this session free" question only by
            // taking this lock, so there is no instant at which it can find the
            // session gone from the map and the global permit not yet returned.
            // Releasing the map first — which is what this used to do — left
            // exactly that window, and a phone taking its own session back on a
            // full Mac fell into it and was answered `attachment_limit` a
            // microsecond before there was room.
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            active.remove(&self.session_uid);
            #[cfg(test)]
            release_seam::park(&self.session_uid);
            drop(self.permit.take());
        }
        // Woken only once both leases are back and the map is legible again. A
        // supersede woken by this re-reads the map and then acquires: waking it
        // while the entry was still there would send it straight back to sleep
        // for the rest of its bound, and waking it while the permit was still
        // held would answer it `attachment_limit` for want of a slot this very
        // drop is returning.
        self.freed.notify_waiters();
    }
}

/// A seam inside [`Lease::drop`], at the point between the map removal and the
/// permit's return — the point that used to be observable to an acquirer and
/// now is not.
///
/// It exists because that ordering cannot be witnessed any other way. The two
/// steps are microseconds apart and both are synchronous, so a test can only
/// prove which side of the map lock the permit goes back on by *stopping* the
/// release between them and asking a concurrent acquirer what it sees: with the
/// handback inside the critical section the acquirer blocks on the map lock and
/// is answered once both leases are back, and with the old ordering it walks
/// straight into an empty map and a permit that is still out.
///
/// Armed per session uid, so a lease released by some other test running in the
/// same binary can never trip it, and disarmed by the guard's `Drop`, so a test
/// that panics between arming and releasing cannot leave it armed for the rest
/// of the run.
#[cfg(test)]
mod release_seam {
    use std::sync::mpsc::{Receiver, SyncSender};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    struct Park {
        session_uid: String,
        entered: SyncSender<()>,
        hold: Duration,
    }

    fn armed() -> &'static Mutex<Option<Park>> {
        static ARMED: OnceLock<Mutex<Option<Park>>> = OnceLock::new();
        ARMED.get_or_init(|| Mutex::new(None))
    }

    /// Disarms the seam when dropped, however the test ends.
    pub(super) struct Armed;

    impl Drop for Armed {
        fn drop(&mut self) {
            *armed().lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }

    /// Park the next release of `session_uid`'s lease for `hold`, between the
    /// map removal and the permit's return. The receiver fires as the release
    /// enters the seam, so the caller can act while it is parked there.
    pub(super) fn arm(session_uid: &str, hold: Duration) -> (Armed, Receiver<()>) {
        let (entered, arrival) = std::sync::mpsc::sync_channel(1);
        *armed().lock().unwrap_or_else(|p| p.into_inner()) = Some(Park {
            session_uid: session_uid.to_string(),
            entered,
            hold,
        });
        (Armed, arrival)
    }

    /// Called from [`super::Lease::drop`]. A no-op unless a test has armed this
    /// exact session; fires once and disarms itself, so one arming parks one
    /// release.
    pub(super) fn park(session_uid: &str) {
        // Taken by value, so the seam's own lock is not held across the sleep.
        let mut slot = armed().lock().unwrap_or_else(|p| p.into_inner());
        let Some(park) = slot.take_if(|park| park.session_uid == session_uid) else {
            return;
        };
        drop(slot);
        let _ = park.entered.try_send(());
        std::thread::sleep(park.hold);
    }
}

/// The connection's grip on one live terminal. Everything on it is
/// non-blocking; dropping it (or calling [`TerminalHandle::close`]) tears the
/// whole carrier down without waiting for it.
pub struct TerminalHandle {
    session_id: String,
    client_pid: i32,
    /// Input bytes for the writer. Unbounded, because the connection's
    /// input-credit ledger — not this queue — is the bound: the phone may have
    /// at most one credit window of input in flight, empty frames are dropped
    /// before they arrive, and an over-credit send is refused up the stack.
    input: mpsc::UnboundedSender<Vec<u8>>,
    /// The phone's desired viewport. A `watch`, not a queue, so a resize storm
    /// coalesces to the latest value the writer will apply — it can never grow
    /// unbounded the way a per-message queue could.
    resize: watch::Sender<(u16, u16)>,
    /// Output credit, one permit per byte. The reader spends it; the connection
    /// replenishes it as the phone acknowledges. Closed on teardown so a reader
    /// parked waiting for credit wakes immediately.
    output_credit: Arc<Semaphore>,
    /// Why the carrier ended, set by whichever task closed it. Read by the
    /// connection when the output stream ends so it can name the reason on the
    /// wire — a session exit, an identity change, a stalled consumer — instead
    /// of guessing. `None` until closed.
    close_cause: Arc<std::sync::Mutex<Option<CloseCause>>>,
    /// The close signal every task races its awaits against.
    close: watch::Sender<bool>,
}

impl TerminalHandle {
    /// Lease, resolve, spawn, verify — in that order — and hand back a live
    /// carrier, or the reason there is none. The lease comes first on purpose;
    /// the reason is at the call itself. Decoded pane bytes flow into
    /// `chunks` (closing it is the "session ended" signal); the count of input
    /// bytes actually written to the pane flows into `input_written`, which the
    /// connection turns into replenished input credit; `output_credit` must
    /// arrive holding the attach's initial grant.
    #[allow(clippy::too_many_arguments)]
    pub async fn open(
        leases: &TerminalLeases,
        socket: &str,
        session_uid: &str,
        cols: u16,
        rows: u16,
        output_credit: Arc<Semaphore>,
        chunks: OutputSender,
        input_written: mpsc::UnboundedSender<u32>,
    ) -> Result<TerminalHandle, OpenError> {
        // (A test may hold every attach here, to act inside the window this
        // spends resolving, spawning and waiting for a client to bind.)
        hold_open_for_test().await;
        // The close signal and the cause slot are built here rather than beside
        // the other channels below, because the lease registers them: a
        // supersede must be able to close whatever holds a session's lease from
        // the instant the lease is held. See [`LeaseHolder`].
        let (close, closed) = watch::channel(false);
        let close_cause: Arc<std::sync::Mutex<Option<CloseCause>>> =
            Arc::new(std::sync::Mutex::new(None));
        let holder = LeaseHolder {
            close: close.clone(),
            credit: Arc::clone(&output_credit),
            cause: Arc::clone(&close_cause),
        };
        // The lease first, so a storm of attaches is bounded before any of
        // them does the more expensive resolve/spawn.
        let lease = leases
            .acquire_superseding(session_uid, holder, supersede_wait())
            .await?;

        let uid = session_uid.to_string();
        let resolve_socket = socket.to_string();
        let owned: OwnedSession =
            tokio::task::spawn_blocking(move || tmux::resolve_owned_session(&resolve_socket, &uid))
                .await
                .map_err(|join| OpenError::Unavailable(format!("resolve task failed: {join}")))??;

        let bin = tmux::tmux_bin()
            .ok_or_else(|| OpenError::Unavailable("tmux is not installed".to_string()))?;
        let argv = tmux::daemon_attach_argv(socket, &owned.session_id)
            .ok_or_else(|| OpenError::Unavailable("session is not addressable".to_string()))?;

        // Plain pipes: control mode is a text protocol, not a screen. The
        // client finds the server from the argv, so the environment carries
        // nothing; `TMUX`/`TMUX_PANE` are removed so it never believes it is
        // nested. `kill_on_drop` is the last-resort backstop under the reaper.
        let mut child = tokio::process::Command::new(&bin)
            .args(&argv)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| OpenError::Unavailable(format!("could not spawn tmux: {err}")))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let client_pid = child
            .id()
            .ok_or_else(|| OpenError::Unavailable("the tmux client has no pid".to_string()))?
            as i32;

        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (resize_tx, resize_rx) = watch::channel((cols, rows));
        let (attached_tx, attached) = oneshot::channel();
        // The scoped input target the reader has bound and follows, shared with
        // the writer so keystrokes reach the *same* pane the phone is shown —
        // never whatever `$session` happens to have active, which a local pane
        // switch during an output stall could otherwise redirect input to — and
        // scoped to the session/window so a migrated pane fails closed. `None`
        // until the seed reply binds it; the writer holds input until then.
        let (target_tx, target_rx) = watch::channel(None::<Target>);
        // What the writer has asked tmux for, in the order it asked. The reader
        // drains it one tag per reply block, which is the only way a
        // `capture-pane` body — arbitrary screen text — can be told from any
        // other command's reply.
        let replies: ReplyQueue = Arc::new(Mutex::new(VecDeque::new()));

        // The reader owns stdout from the first byte, so nothing the client
        // says can be lost. It binds the window and active pane from the seed
        // query's reply, scopes notifications to its own window and session,
        // reports the attach id once, and treats a session or window change as
        // end-of-carrier.
        tokio::spawn(read_client(ReadCtx {
            stdout,
            expected_session: owned.session_id.clone(),
            credit: Arc::clone(&output_credit),
            chunks,
            attached: attached_tx,
            target: target_tx,
            replies: Arc::clone(&replies),
            close_cause: Arc::clone(&close_cause),
            close: close.clone(),
            closed: closed.clone(),
        }));
        // Input targets the bound, scoped target, so it always reaches exactly
        // the pane the phone is looking at and never another session.
        tokio::spawn(write_client(
            stdin,
            owned.session_id.clone(),
            input_rx,
            resize_rx,
            target_rx.clone(),
            replies,
            input_written,
            close.clone(),
            closed.clone(),
        ));
        // The reaper owns the child and the leases: whatever ends the carrier,
        // the client is waited on and the slot frees only after; and a client
        // that exits on its own broadcasts the close so the rest tears down.
        tokio::spawn(reap_client(child, lease, close.clone(), closed));

        let handle = TerminalHandle {
            session_id: owned.session_id.clone(),
            client_pid,
            input: input_tx,
            resize: resize_tx,
            output_credit,
            close_cause,
            close,
        };

        // The client must say it attached to the exact session that was
        // resolved: its own `%session-changed` is one binding fact, straight
        // from the horse's mouth, and cheap to insist on.
        let announced = tokio::time::timeout(TEARDOWN_GRACE, attached)
            .await
            .map_err(|_| {
                handle.close();
                OpenError::Unavailable("the tmux client did not attach in time".to_string())
            })?
            .map_err(|_| {
                handle.close();
                OpenError::Unavailable("the tmux client ended during attach".to_string())
            })?;
        if announced != owned.session_id {
            handle.close();
            return Err(OpenError::IdentityMismatch(format!(
                "attached to {announced}, resolved {}",
                owned.session_id
            )));
        }

        // And the server must independently agree, for this client's own pid,
        // on the same session and epoch — a fact the child cannot fabricate.
        let expected = owned.clone();
        let verified =
            tokio::task::spawn_blocking(move || tmux::reverify_owned_client(client_pid, &expected))
                .await
                .map_err(|join| {
                    handle.close();
                    OpenError::Unavailable(format!("verify task failed: {join}"))
                })?;
        if let Err(err) = verified {
            handle.close();
            return Err(err.into());
        }

        // The seed must bind before the terminal is handed back, so a client
        // that never answers the seed query (or answers with an error) fails
        // the attach rather than presenting a terminal that shows nothing and
        // whose input has no pane to target. The reader publishes the target
        // when the reply lands; wait for that, bounded.
        let mut seed = target_rx;
        let seeded = tokio::time::timeout(TEARDOWN_GRACE, async {
            while seed.borrow().is_none() {
                if seed.changed().await.is_err() {
                    return false;
                }
            }
            true
        })
        .await;
        if !matches!(seeded, Ok(true)) {
            handle.close();
            return Err(OpenError::Unavailable(
                "the tmux client did not report its pane".to_string(),
            ));
        }
        Ok(handle)
    }

    /// A handle whose commands are accepted and discarded (the receiver is kept
    /// alive but never drained), for exercising the connection's own ledgers
    /// without a child process. `input` succeeds, so it is the connection's
    /// credit accounting — not a dead carrier — that any refusal comes from.
    #[cfg(test)]
    pub(crate) fn inert_stub() -> TerminalHandle {
        let (input, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // Keep the receiver alive for the handle's lifetime so sends succeed.
        std::mem::forget(rx);
        let (resize, _) = watch::channel((80, 24));
        let (close, _) = watch::channel(false);
        TerminalHandle {
            session_id: "$0".to_string(),
            client_pid: 0,
            input,
            resize,
            output_credit: Arc::new(Semaphore::new(0)),
            close_cause: Arc::new(std::sync::Mutex::new(None)),
            close,
        }
    }

    /// The wire code alone, for the carrier's own tests.
    ///
    /// Production reads [`Self::close_cause`], because the connection needs both
    /// halves and wants them from one look at the slot. This survives for the
    /// tests that assert on the code and mean the code: it is what the phone
    /// branches on, where the sentence is prose it only displays, and a test
    /// comparing whole causes would fail for a wording change that costs the
    /// phone nothing.
    #[cfg(test)]
    pub(crate) fn close_reason(&self) -> &'static str {
        self.close_cause().code
    }

    /// Why the carrier ended, as the pair the connection puts on the wire.
    ///
    /// The sentence is minted here rather than by the connection because two
    /// endings share one code and only the carrier knows which of them happened:
    /// a phone that stopped crediting and a peer that stopped reading its socket
    /// are both `slow_consumer`, and a lookup from the code could only ever
    /// answer with one of the two sentences. See [`CloseCause`].
    pub fn close_cause(&self) -> CloseCause {
        self.close_cause
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unwrap_or(cause::SESSION_EXITED)
    }

    /// The resolved internal session id (`$N`), for logs — never a wire value.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The disposable client's pid, for logs.
    pub fn client_pid(&self) -> i32 {
        self.client_pid
    }

    /// Hand exact bytes to the writer. Never waits and never drops data: the
    /// queue is unbounded and the connection's input-credit ledger is what
    /// bounds how much may be in flight. Fails only when the carrier is gone.
    pub fn input(&self, bytes: Vec<u8>) -> Result<(), ()> {
        self.input.send(bytes).map_err(|_| ())
    }

    /// Set the desired viewport. Latest-wins: a resize storm collapses to the
    /// most recent value rather than queuing. Ignored if the carrier is gone;
    /// the current size holds for as long as there is a terminal to hold it.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize.send((cols, rows));
    }

    /// The viewport the carrier has been asked for.
    ///
    /// The boundary the connection's own resize filters actually govern, and the
    /// only place they are observable: measured on tmux 3.7b, a `refresh-client`
    /// past the wire's range is clamped by the server, so the pane's size cannot
    /// tell a resize that was refused here from one that was refused there.
    #[cfg(test)]
    pub(crate) fn desired_size(&self) -> (u16, u16) {
        *self.resize.borrow()
    }

    /// Tear the carrier down, without waiting for it. Synchronous and
    /// idempotent: the close signal wakes every task's awaits at once (the
    /// writer detaches, the reader stops), the credit close wakes a reader
    /// parked on credit, and the reaper kills and reaps the child. `Drop` does
    /// the same, so no path can leak the child.
    pub fn close(&self) {
        let _ = self.close.send(true);
        self.output_credit.close();
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// The pane the carrier is bound to, and which bind bound it.
///
/// The generation is what ties a snapshot reply to the target it was asked
/// for. Order alone cannot: the writer may still be issuing the screen for pane
/// B when the reader has already followed the focus to C, and a reply that
/// arrives after that switch is a picture of a pane the phone has left. The
/// reader bumps the generation on every publish, the writer stamps the tags it
/// queues with the value it read, and the two are compared before anything is
/// painted.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    generation: u64,
    /// The scoped `session:window.pane` every command names.
    dest: String,
}

/// What one completed `%begin`…`%end` block means, queued by the writer in the
/// order it writes the commands that produce them.
///
/// Order, not shape, is what correlates a reply with its command: a
/// `capture-pane` body is arbitrary screen text, so nothing in it identifies
/// it. tmux's own guarantee — "each command will produce one block of output" —
/// is what makes the queue exact, and an `%error` consumes its tag too, so a
/// capture of a pane that vanished cannot shift every reply behind it by one.
/// Order says which *command* answered; the generation on the snapshot tags
/// says which *target* it answered for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExpectedReply {
    /// The seed query: one `@<win> %<pane>` line, which binds and publishes the
    /// target the whole carrier is scoped to.
    Seed,
    /// A `capture-pane` for the target of that generation: its screen, one body
    /// line per row.
    Capture(u64),
    /// A cursor query for the target of that generation: one `%<pane> <x> <y>`
    /// line, which completes the paint if the pane it names is the one whose
    /// screen was captured.
    Cursor(u64),
    /// A command whose reply carries nothing — `send-keys`, `refresh-client`,
    /// `detach-client`. Its block is consumed and dropped.
    Empty,
}

/// The writer's queue of what it has asked for, drained by the reader in the
/// same order. A `std` mutex: every critical section is a push or a pop with no
/// await inside it.
type ReplyQueue = Arc<Mutex<VecDeque<ExpectedReply>>>;

fn queue_reply(queue: &ReplyQueue, expect: ExpectedReply) {
    queue
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push_back(expect);
}

fn next_reply(queue: &ReplyQueue) -> Option<ExpectedReply> {
    queue
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .front()
        .copied()
}

fn take_reply(queue: &ReplyQueue) {
    queue.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
}

/// One snapshot in flight, and the pane bytes held back while it is assembled.
///
/// The reader holds `Some` of these from a target being published until its
/// screen is painted (or given up on); `None` is ordinary streaming.
struct Snapshot {
    /// The publish this snapshot and its held bytes belong to. A reply carrying
    /// any other generation is a screen for a pane the carrier has left.
    generation: u64,
    /// The pane this snapshot is of, as tmux named it. The generation says the
    /// reply answered the command the carrier is still on; this says the *pane*
    /// answered it, which for the cursor query is a separate question — see
    /// [`cursor_from_reply`].
    pane: String,
    /// The target's `%output` since it was published, in order.
    held: Vec<u8>,
    /// How much of `held` had arrived when tmux began the capture. Those bytes
    /// had already been processed into the grid the capture read, so the
    /// captured screen *is* their visible effect and they are dropped with it;
    /// everything after them is still owed to the phone. Set when the capture's
    /// own `%begin` is seen.
    ///
    /// Dropping a prefix is safe only while the grid is never *behind* this
    /// client's own `%output`: a stream that ran ahead of the screen would have
    /// those bytes dropped here and never painted anywhere. Measured on tmux
    /// 3.7b rather than reasoned about — a pane emitting strictly increasing
    /// numbered lines, twenty runs across two regimes (printing flat out, and a
    /// hundred lines a second), comparing the highest line written to this
    /// client before the capture's `%begin` against the highest line the
    /// capture returned. The two were equal on every run: the grid is neither
    /// behind the stream, which would lose those bytes, nor ahead of it, which
    /// would paint bytes the replay then repeats. The boundary falls exactly
    /// here.
    mark: Option<usize>,
    painting: Painting,
}

/// How far along one snapshot is.
enum Painting {
    /// The capture has been asked for and not yet answered.
    Awaiting,
    /// The screen is in hand; the cursor query paired with it finishes the
    /// paint. Output arriving across this window — tmux is free to send a
    /// notification between two reply blocks — is held like any other and
    /// flushed after the paint, so it is neither doubled nor lost.
    Captured(Vec<Vec<u8>>),
}

/// Synthesise the bytes that paint one captured screen: reset the attributes a
/// previous pane may have left set, clear, home, the rows exactly as tmux
/// reported them, then the cursor where the pane has it. Rows are joined with
/// CRLF because a bare LF only moves down a line, and the cursor address is
/// 1-based because that is how CUP counts — tmux reports it from zero. A
/// cursor tmux would not name ends the paint after the last row instead: the
/// screen is worth painting without it.
fn paint_snapshot(rows: &[Vec<u8>], cursor: Option<(u16, u16)>) -> Vec<u8> {
    let mut painted = Vec::with_capacity(rows.iter().map(|row| row.len() + 2).sum::<usize>() + 16);
    painted.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
    for (n, row) in rows.iter().enumerate() {
        if n > 0 {
            painted.extend_from_slice(b"\r\n");
        }
        painted.extend_from_slice(row);
    }
    if let Some((x, y)) = cursor {
        let (row, col) = (y.saturating_add(1), x.saturating_add(1));
        painted.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
    }
    painted
}

/// Everything the reader task owns. A struct because there are enough fields
/// that positional arguments would be a footgun.
struct ReadCtx {
    stdout: ChildStdout,
    /// The session the attach resolved to. A `%session-changed` naming any
    /// other session means the client was moved out from under us.
    expected_session: String,
    credit: Arc<Semaphore>,
    chunks: OutputSender,
    attached: oneshot::Sender<String>,
    /// The bound input target, published for the writer and the attach gate: a
    /// `session:window.pane` composite so `send-keys` is scoped — an ordinary
    /// focus move inside our window updates the pane, but a pane migrated out of
    /// the window (`break-pane`/`join-pane`) no longer matches and the write
    /// fails closed rather than typing into whatever took its place. The seed
    /// reply sets it; each in-window focus move updates it, under a new
    /// generation.
    target: watch::Sender<Option<Target>>,
    /// What the writer has asked tmux for, in order, so each completed reply
    /// block can be read as the answer to a known command.
    replies: ReplyQueue,
    close_cause: Arc<std::sync::Mutex<Option<CloseCause>>>,
    close: watch::Sender<bool>,
    closed: watch::Receiver<bool>,
}

/// A `session:window.pane` `send-keys` target. Naming all three scopes input to
/// the verified session and window: an ordinary focus move within the window
/// updates only the pane, while a pane that leaves the window — for another
/// window of the same session or for another session entirely — no longer
/// matches the composite, and tmux answers `can't find pane` rather than
/// delivering the keys somewhere else (measured on tmux 3.7b).
fn scoped_target(session_id: &str, window: &str, pane: &str) -> String {
    format!("{session_id}:{window}.{pane}")
}

/// Bind the carrier to a pane and tell the writer.
///
/// Every bind — the seed's and every focus move followed after it — takes a new
/// generation, which is what makes a snapshot reply for the pane just left
/// recognisable as one. The bytes held for the previous target are dropped with
/// it: they belong to a pane the carrier no longer follows, exactly like that
/// pane's live output.
fn publish(
    generation: &mut u64,
    snapshot: &mut Option<Snapshot>,
    target: &watch::Sender<Option<Target>>,
    session: &str,
    window: &str,
    pane: &str,
) {
    *generation += 1;
    *snapshot = Some(Snapshot {
        generation: *generation,
        pane: pane.to_string(),
        held: Vec::new(),
        mark: None,
        painting: Painting::Awaiting,
    });
    let _ = target.send(Some(Target {
        generation: *generation,
        dest: scoped_target(session, window, pane),
    }));
    note_published(*generation);
}

/// Record why the carrier ended, for the connection to read. First writer wins,
/// so the specific cause (an identity change, a stall) is never overwritten by
/// the generic teardown that follows it.
fn note_cause(slot: &std::sync::Mutex<Option<CloseCause>>, cause: CloseCause) {
    let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
    slot.get_or_insert(cause);
}

/// The cursor `captured` should paint with, read from the block that answered
/// its cursor query: the position tmux reported *for that pane*, or `None` if
/// the query errored, outgrew the body bound, did not answer with exactly one
/// parseable position, or answered for some other pane. `None` costs the cursor
/// and never the screen — the rows are painted without a cursor move behind
/// them, which leaves the cursor at the end of the last row until the pane
/// moves it itself.
///
/// The pane test is the one that is not defensive. A pane can vanish between
/// its capture being answered and its cursor query running, and measured on
/// 3.7b `display-message` does not report that: a composite whose pane stopped
/// resolving is answered for the window's *active* pane, rc 0, no `%error`,
/// where `capture-pane` against the same target errors. Only the pane id in the
/// reply distinguishes the two, so a carrier that trusted the position alone
/// would paint one pane's screen and then place another pane's cursor on it.
/// The generation on the reply tag cannot catch this: it says the carrier has
/// not moved on, not that the pane is still there.
fn cursor_from_reply(
    errored: bool,
    overflowed: bool,
    body: &[Vec<u8>],
    captured: &str,
) -> Option<(u16, u16)> {
    let (pane, x, y) = (!errored && !overflowed && body.len() == 1)
        .then(|| tmux::parse_cursor_reply(&body[0]))
        .flatten()?;
    (pane == captured).then_some((x, y))
}

/// Hold one line of an open reply block against [`REPLY_BODY_BYTES`], charging
/// what the line really costs — its bytes and the row entry that holds them —
/// to `held`. `false` means the line does not fit and the block has overflowed;
/// the caller latches that and gives the whole body up, because a body missing
/// a line in the middle is not a screen.
fn hold_body_line(body: &mut Vec<Vec<u8>>, held: &mut usize, line: &[u8]) -> bool {
    let cost = line.len().saturating_add(BODY_ROW_OVERHEAD);
    if held.saturating_add(cost) > REPLY_BODY_BYTES {
        return false;
    }
    *held += cost;
    body.push(line.to_vec());
    true
}

/// How one read of a control-mode line ended.
enum LineRead {
    /// The buffer holds one whole line, without its newline.
    Line,
    /// The stream is over — clean EOF, or a read error, which ends the carrier
    /// either way.
    Ended,
    /// The line ran past the cap before its newline arrived. The buffer holds
    /// the first `cap` bytes, the rest is discarded, and the stream is left at
    /// the start of the next line so everything behind it still reads.
    TooLong,
}

/// Read one newline-terminated line into `line`, never letting it grow past
/// `cap`.
///
/// `read_until` would size the buffer to whatever the stream sends; scanning
/// `fill_buf` chunks instead holds the allocation at `cap` while still draining
/// the over-long line to its newline, so the caller decides what an unending
/// line means rather than the allocator deciding for it.
///
/// Not cancel-safe: bytes consumed from the stream are lost if the future is
/// dropped part-way through a line. Its one caller drops it only when it is
/// ending the carrier anyway, which is the same contract `read_until` was used
/// under here.
async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    src: &mut R,
    line: &mut Vec<u8>,
    cap: usize,
) -> LineRead {
    line.clear();
    let mut too_long = false;
    loop {
        let (used, complete) = {
            let available = match src.fill_buf().await {
                Ok(available) => available,
                Err(_) => return LineRead::Ended,
            };
            if available.is_empty() {
                // EOF. A trailing line with no newline is still a line, and one
                // that outgrew the cap is still over-long.
                return if too_long {
                    LineRead::TooLong
                } else if line.is_empty() {
                    LineRead::Ended
                } else {
                    LineRead::Line
                };
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(at) => {
                    too_long |= !keep_capped(line, &available[..at], cap);
                    (at + 1, true)
                }
                None => {
                    too_long |= !keep_capped(line, available, cap);
                    (available.len(), false)
                }
            }
        };
        src.consume(used);
        if complete {
            return if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            };
        }
    }
}

/// Append what of `bytes` still fits under `cap`, and say whether all of it
/// did. `false` is what makes the cap a cap: everything past it is dropped on
/// the floor rather than allocated.
fn keep_capped(line: &mut Vec<u8>, bytes: &[u8], cap: usize) -> bool {
    let room = cap.saturating_sub(line.len());
    line.extend_from_slice(&bytes[..room.min(bytes.len())]);
    bytes.len() <= room
}

/// The reader task: owns the client's stdout for its whole life. Forwards the
/// active pane's bytes under credit, paints the pane's existing screen whenever
/// a target is published, learns and follows the active pane from the stream,
/// reports the attach id once, and ends the carrier on `%exit`, a session or
/// window change, a stalled consumer, or any read error.
async fn read_client(ctx: ReadCtx) {
    let ReadCtx {
        stdout,
        expected_session,
        credit,
        chunks,
        attached,
        target: target_tx,
        replies,
        close_cause,
        close,
        mut closed,
    } = ctx;
    // Taken by value and reached mutably below: the forgiveness budget it holds
    // is spent across every chunk of the attachment, so it cannot be rebuilt per
    // forward without becoming the per-chunk ceiling it replaces.
    let mut delivery = Delivery::new(credit, chunks, Arc::clone(&close_cause));
    let mut lines = BufReader::new(stdout);
    let mut line = Vec::with_capacity(4096);
    let mut attached = Some(attached);
    // The window and active pane are bound authoritatively from the client's
    // own answer to the seed query (issued first by the writer), not guessed
    // from a probe or the first output — control mode streams every pane of
    // every window in the session, so neither guess is reliable. Nothing is
    // forwarded until the reply lands; the snapshot that follows the bind is
    // what the phone sees first. `%window-pane-changed` is scoped to our window
    // (tmux broadcasts it across sessions), and only our window's focus moves
    // are followed.
    let mut window: Option<String> = None;
    let mut active_pane: Option<String> = None;
    // Which bind the carrier is on, and the snapshot being assembled for it —
    // `None` once the screen has been painted, which is ordinary streaming.
    let mut generation: u64 = 0;
    let mut snapshot: Option<Snapshot> = None;
    // The reply block currently open, held as the `%begin` arguments that must
    // be matched to close it, and the body lines gathered inside it.
    let mut open_block: Option<Vec<u8>> = None;
    let mut body: Vec<Vec<u8>> = Vec::new();
    // What the body held so far costs against its bound — bytes and row
    // entries both, so an unending reply reaches the bound however cheap its
    // individual lines are.
    let mut body_held: usize = 0;
    let mut body_overflowed = false;
    'read: loop {
        let read = tokio::select! {
            read = read_capped_line(&mut lines, &mut line, control_line_bytes()) => read,
            _ = closed.changed() => break,
        };
        match read {
            LineRead::Line => {}
            LineRead::Ended => break,
            // A line past the cap inside a reply block is one the body bound
            // would have refused anyway, so it fails the same way: the block is
            // given up whole and the held output flushed, because a body
            // missing a line in the middle is not a screen.
            LineRead::TooLong if open_block.is_some() => {
                body_overflowed = true;
                continue;
            }
            // Outside a block there is nothing to give up but the stream. A
            // notification line with no end is not something tmux emits — the
            // server chunks `%output` itself — so what is on the wire is no
            // longer the protocol, and the carrier refuses to keep reading it
            // for the same reason it refuses a session it did not verify.
            LineRead::TooLong => {
                note_cause(&close_cause, cause::PROTOCOL_ERROR);
                break;
            }
        }
        // Inside a reply block, every line is body until the *matching* close:
        // tmux does not escape command output, so a captured screen showing the
        // text `%end 1 2 3` would otherwise end the block it appears in.
        //
        // Taking every line as body rests on tmux emitting a command's block
        // contiguously — notifications and pane output are ordered before the
        // `%begin` or after the close, never interleaved inside. A `%output`
        // that did land inside would be swallowed into the screen instead of
        // held, which no amount of parsing here could undo. Measured on 3.7b
        // against a pane printing flat out: across 246 complete blocks and
        // 5760 body lines, not one body line was a notification.
        if open_block.is_some() {
            let closed_by = match tmux::reply_block_close(&line) {
                Some((args, errored)) if open_block.as_deref() == Some(args) => Some(errored),
                _ => None,
            };
            let Some(errored) = closed_by else {
                if !hold_body_line(&mut body, &mut body_held, &line) {
                    body_overflowed = true;
                }
                continue;
            };
            open_block = None;
            body_held = 0;
            let done = std::mem::take(&mut body);
            let overflowed = std::mem::replace(&mut body_overflowed, false);
            // One tag per completed block, in the order the commands were
            // written — including for an `%error`, so a failed command cannot
            // shift every reply behind it onto the wrong meaning.
            match next_reply(&replies) {
                // The seed anchors the queue by shape, because the block ahead
                // of it is not ours: tmux answers its own `attach-session` with
                // an empty block before the session is even announced (measured
                // on 3.7b). Only a reply of the seed's own shape pops the tag —
                // an unowned block is skipped, and so is an `%error`, because a
                // failed block that consumed the tag would hand the seed's real
                // reply to the tag behind it. A seed that never answers in that
                // shape never binds, and the attach gate then refuses the
                // terminal rather than showing one whose input has no pane to
                // reach. From the seed on, order is exact.
                Some(ExpectedReply::Seed) => {
                    let seed = (!errored && !overflowed && done.len() == 1)
                        .then(|| tmux::parse_seed_reply(&done[0]))
                        .flatten();
                    if let Some((w, p)) = seed {
                        take_reply(&replies);
                        window = Some(w.clone());
                        active_pane = Some(p.clone());
                        // Publish before anything can block: `open()`'s attach
                        // gate waits on this, and the paint that follows it
                        // waits on output credit.
                        publish(
                            &mut generation,
                            &mut snapshot,
                            &target_tx,
                            &expected_session,
                            &w,
                            &p,
                        );
                    }
                }
                Some(ExpectedReply::Capture(asked_for)) => {
                    take_reply(&replies);
                    note_snapshot_reply();
                    let Some(mut pending) = snapshot.take() else {
                        continue;
                    };
                    // A screen captured for a target the carrier has already
                    // left is the wrong pane's. It is dropped; the snapshot for
                    // the target now followed is still awaited, and its own
                    // capture is on the way.
                    if pending.generation != asked_for {
                        snapshot = Some(pending);
                        continue;
                    }
                    if errored || overflowed {
                        // The pane vanished, or answered with more than a
                        // screen. There is no snapshot to paint, so everything
                        // held goes to the phone in order and the stream
                        // carries on live rather than losing those bytes.
                        if !deliver(&pending.held, &mut delivery, &mut closed).await {
                            break 'read;
                        }
                    } else {
                        pending.painting = Painting::Captured(done);
                        snapshot = Some(pending);
                    }
                }
                Some(ExpectedReply::Cursor(asked_for)) => {
                    take_reply(&replies);
                    note_snapshot_reply();
                    let Some(pending) = snapshot.take() else {
                        continue;
                    };
                    // Same rule as the capture: only the target the carrier is
                    // on now is painted, so a focus move that landed either
                    // side of the pair cannot flash a view the phone has left.
                    if pending.generation != asked_for {
                        snapshot = Some(pending);
                        continue;
                    }
                    let Snapshot {
                        pane,
                        mut held,
                        mark,
                        painting,
                        ..
                    } = pending;
                    let (painted, replay) = match (painting, mark) {
                        (Painting::Captured(rows), Some(mark)) => {
                            // Everything tmux had already processed when the
                            // capture ran is the screen in hand; everything
                            // after it is still owed, and follows the paint in
                            // the order it arrived.
                            let replay = held.split_off(mark);
                            // A cursor that could not be read, or that came
                            // back for some other pane, costs the cursor and
                            // not the screen: the rows are painted anyway and
                            // no cursor move follows them, which leaves the
                            // cursor where painting the rows left it — at the
                            // end of the last row — rather than where the pane
                            // actually has it. A screen whose cursor is
                            // misplaced until the pane next moves it beats no
                            // screen at all, and beats another pane's cursor
                            // placed on it as if it were this one's.
                            (
                                paint_snapshot(
                                    &rows,
                                    cursor_from_reply(errored, overflowed, &done, &pane),
                                ),
                                replay,
                            )
                        }
                        // No screen was taken for this generation, or its own
                        // `%begin` never marked where the held bytes stood when
                        // tmux read the grid. Either way nothing here is known
                        // to be doubled by a paint, and the rule is that a
                        // snapshot may be lost and bytes may not: all of them
                        // are owed, and none of them is painted over.
                        _ => (Vec::new(), held),
                    };
                    // The same credit, stall and close machinery as any pane
                    // bytes: a snapshot larger than one grant is sent as the
                    // grant allows, never as an exception to it.
                    for bytes in [painted, replay] {
                        if bytes.is_empty() {
                            continue;
                        }
                        if !deliver(&bytes, &mut delivery, &mut closed).await {
                            break 'read;
                        }
                    }
                }
                Some(ExpectedReply::Empty) => take_reply(&replies),
                None => {}
            }
            continue;
        }
        if let Some(args) = tmux::reply_block_open(&line) {
            open_block = Some(args.to_vec());
            // Where the held bytes stood when tmux began this capture. What is
            // behind the mark had already been processed into the grid the
            // capture reads, so the screen about to arrive is its visible
            // effect; what comes after it is not in that screen and is still
            // owed to the phone.
            if let (Some(ExpectedReply::Capture(asked_for)), Some(pending)) =
                (next_reply(&replies), snapshot.as_mut())
            {
                if pending.generation == asked_for {
                    pending.mark = Some(pending.held.len());
                }
            }
            continue;
        }
        match tmux::parse_control_line(&line) {
            ControlLine::Output { pane, bytes } => {
                // Counted before anything is decided about it, so a test barrier
                // built on it cannot be silenced by the routing below.
                note_output_seen();
                // Only the bound pane's bytes are this viewer's screen: every
                // other pane of the session is dropped where it arrives, as is
                // anything that speaks before the seed binds one.
                if active_pane.as_deref() != Some(pane.as_str()) {
                    note_withheld();
                    continue;
                }
                // While a snapshot is being assembled the bytes are held rather
                // than sent — the capture will carry whatever tmux has already
                // processed, and forwarding both would paint it twice — and
                // replayed the instant the paint lands.
                let held_now = match snapshot.as_mut() {
                    Some(pending) => {
                        note_withheld();
                        pending.held.extend_from_slice(&bytes);
                        true
                    }
                    None => false,
                };
                // Either the live bytes, or a buffer that outgrew its bound: a
                // pane that outruns its own snapshot loses the snapshot, never
                // the bytes.
                let send = if !held_now {
                    Some(bytes)
                } else if snapshot
                    .as_ref()
                    .is_some_and(|pending| pending.held.len() > HELD_OUTPUT_BYTES)
                {
                    snapshot.take().map(|pending| pending.held)
                } else {
                    None
                };
                if let Some(send) = send {
                    if !deliver(&send, &mut delivery, &mut closed).await {
                        break;
                    }
                }
            }
            // A pane focus change. tmux broadcasts these across sessions, so
            // act only on our own window; a change in any other window (another
            // session's, or a background window of ours) is not our view.
            ControlLine::PaneChanged { window: w, pane } => {
                // A change naming the pane already bound is not a move, and
                // must not cost a repaint of the screen the phone is looking at.
                if window.as_deref() == Some(&w) && active_pane.as_deref() != Some(pane.as_str()) {
                    // Move the writer's target with the reader's — still scoped
                    // to our session and window — so input keeps reaching the
                    // pane the phone is shown, and so the writer asks for the
                    // new pane's screen. Until that lands the new pane's output
                    // is held, because the capture will carry the part of it
                    // that is on screen by then.
                    publish(
                        &mut generation,
                        &mut snapshot,
                        &target_tx,
                        &expected_session,
                        &w,
                        &pane,
                    );
                    active_pane = Some(pane);
                }
            }
            // The active window changed. Broadcast across sessions too, so act
            // only on our own; then close, as this single-window carrier does
            // not follow a window switch.
            ControlLine::WindowChanged(session) if session == expected_session => {
                note_cause(&close_cause, cause::WINDOW_CHANGED);
                break;
            }
            ControlLine::WindowChanged(_) => {}
            ControlLine::SessionChanged(id) => match attached.take() {
                // The attach announcement, reported to `open` for the bind.
                Some(tx) => {
                    let _ = tx.send(id);
                }
                // A later switch to a different session: the client is no
                // longer on what we verified, so we refuse to keep streaming.
                None if id != expected_session => {
                    note_cause(&close_cause, cause::IDENTITY_MISMATCH);
                    break;
                }
                None => {}
            },
            ControlLine::Exit => break,
            ControlLine::Ignored => {}
        }
    }
    // However the stream ended, the whole carrier ends: the close wakes the
    // other tasks, and the dropped `chunks` sender tells the connection.
    let _ = close.send(true);
}

/// The carrier's output stream: the carrier hands pane bytes in at one end, the
/// connection writes them to the phone at the other.
///
/// A plain channel would say only what is *queued*, and queue occupancy is not
/// delivery accounting. The connection takes a chunk off the queue and only then
/// awaits the socket write, so between those two moments the queue reads empty
/// while the phone has not been given the bytes — and a carrier that read the
/// queue would call a credit wait across that window a stalled consumer, on a
/// phone with nothing to answer for. So the two halves share one count of what
/// has been handed over and not yet written: charged before a chunk is queued,
/// settled when the value carrying it is dropped, which is after the write.
///
/// Two counts, not one, because "on its way" and "being written" are different
/// questions with different answers: the first is the daemon's own backpressure
/// and is forgiven without limit, the second is the peer choosing when a write
/// completes and has to be budgeted. [`Delivery::stall`] is where they are asked.
///
/// Both are created here with the channel and reachable only through these two
/// halves, so there is no second place for either to drift out of step with.
pub fn output_channel(capacity: usize) -> (OutputSender, OutputDrain) {
    let (chunks, queued) = mpsc::channel(capacity);
    let undelivered = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(PeerHold::new());
    (
        OutputSender {
            chunks,
            undelivered: Arc::clone(&undelivered),
            hold: Arc::clone(&hold),
        },
        OutputDrain {
            chunks: queued,
            undelivered,
            hold,
        },
    )
}

/// The clock the peer's share of an attachment is measured on: how long some
/// chunk's write has been in flight, and how many bytes those writes carried.
///
/// Where `undelivered` says *whether* output is still on its way, this says *for
/// how long* the connection has been holding some of it against the socket, which
/// is the difference between a bound on consecutive deadline expiries and a bound
/// on elapsed time — and the whole of the fix. Both quantities are cumulative and
/// monotone, so the difference between two readings is what accrued between them:
/// a reader can charge its own wait without either side polling, and across a
/// write spanning the whole window at that, since with a write in flight since
/// `s` the reading is `total + (t - s)` and the difference over `[m, n]` is
/// `n - m`.
///
/// Everything in here is read and written **under one lock**, and that is not
/// caution but the cheapest correct thing available. Time and bytes are a pair: a
/// charge is elapsed time *less what its bytes allow*, so a reading that caught
/// one of them updated and the other not would charge a write's time with no
/// allowance against it. Every lock-free arrangement leaves such a window — order
/// the writes either way and the mark or the charge is the one that tears — and a
/// window that narrow is exactly the kind that survives review and then closes
/// somebody's terminal on a Tuesday. There is no contention to lose: one task
/// writes, one reads, neither holds it across an await, and it is taken twice per
/// chunk against a socket write.
///
/// The span timed is dequeue to drop — the whole time the chunk is out of the
/// queue — and not the
/// socket send alone, so it includes the connection framing the chunk before
/// sending it. That is deliberate twice over: it is the span for which the queue
/// slot is held and the reader is therefore blocked, which is the harm being
/// bounded rather than the peer's culpability in the abstract; and it is
/// microseconds of framing against an allowance measured in seconds
/// ([`PEER_DRAIN_WINDOW`]), so it cannot move a healthy carrier across the line.
///
/// This measures temporal overlap and not causality: a wait that became ready
/// while the reader task was not yet scheduled carries that scheduling delay into
/// the interval. It is negligible beside the allowance, and it is the reason
/// nothing here is called an exact attribution.
struct PeerHold(std::sync::Mutex<Held>);

/// How long the peer has held writes, how much it took while holding them, and
/// how many chunks it is holding right now.
///
/// The count lives in here rather than beside it, and that is load-bearing. It
/// was an atomic of its own once, and then "the connection is holding a chunk"
/// and "the clock is running" were two facts settled a few instructions apart: a
/// reader landing between them saw a write still in flight whose bytes had not
/// been credited yet, and charged that write's whole elapsed time with no
/// allowance against it. Over-charging is the direction that closes a phone that
/// did nothing wrong, so the count and the clock it gates are now one object
/// under one lock, and there is no between.
struct Held {
    /// Time writes have been in flight, not counting the one in flight now.
    in_flight: std::time::Duration,
    /// When the write now in flight began, if one has.
    started: Option<tokio::time::Instant>,
    /// Bytes carried by every write that has completed.
    delivered: u64,
    /// Chunks the connection has taken off the queue and not finished writing.
    holding: usize,
}

impl PeerHold {
    fn new() -> PeerHold {
        PeerHold(std::sync::Mutex::new(Held {
            in_flight: std::time::Duration::ZERO,
            started: None,
            delivered: 0,
            holding: 0,
        }))
    }

    /// Taken the way the rest of this file takes a poisoned lock: a panic in one
    /// carrier must not take every later reading with it, and what is inside is a
    /// set of counters that cannot be left half-written.
    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A chunk has left the queue: from here the connection is holding it against
    /// the socket. The clock starts on the transition into holding and not per
    /// chunk, so a connection holding two would have the span timed and not the
    /// sum of the two.
    fn began(&self) {
        let mut held = self.held();
        held.holding += 1;
        if held.holding == 1 {
            held.started = Some(tokio::time::Instant::now());
        }
    }

    /// A chunk has gone to the socket, carrying `bytes`.
    ///
    /// The bytes are credited per chunk even though the clock stops only on the
    /// last, because they were all genuinely delivered: crediting only the last
    /// one's would hand back a fraction of the allowance actually earned, and the
    /// direction of that error is a charge the peer did not deserve.
    fn ended(&self, bytes: usize) {
        let mut held = self.held();
        held.delivered += bytes as u64;
        held.holding = held.holding.saturating_sub(1);
        if held.holding == 0 {
            if let Some(started) = held.started.take() {
                held.in_flight += started.elapsed();
            }
        }
    }

    /// Whether the connection is holding output against the socket.
    fn write_in_flight(&self) -> bool {
        self.held().holding > 0
    }

    /// What the peer has held and taken as of now, as one coherent reading.
    fn mark(&self) -> PeerMark {
        let held = self.held();
        PeerMark {
            held: held.in_flight
                + held
                    .started
                    .map_or(std::time::Duration::ZERO, |s| s.elapsed()),
            delivered: held.delivered,
        }
    }
}

/// A reading of [`PeerHold`] to charge forward from. Both fields come from one
/// lock hold, so the time and the bytes that excuse it are always the same
/// instant's answer.
#[derive(Clone, Copy)]
struct PeerMark {
    held: std::time::Duration,
    delivered: u64,
}

/// The carrier's end of the output stream: where the reader hands pane bytes
/// over, and where it asks whether any of them are still on their way.
pub struct OutputSender {
    chunks: mpsc::Sender<Vec<u8>>,
    /// Chunks handed over that the connection has not finished writing out —
    /// waiting in the queue, or taken from it with the write still in flight.
    /// "Written out" and "received" are not the same thing; [`forward`] says
    /// where that boundary is and what it costs.
    undelivered: Arc<AtomicUsize>,
    /// Chunks the connection has taken off the queue and not finished writing.
    ///
    /// `undelivered` counts everything on its way; this counts the part of it the
    /// connection is *holding against the socket*. The difference is the whole of
    /// the attribution: bytes still in the queue are the daemon's own backpressure
    /// and are forgiven without limit, and bytes in a write are the peer deciding
    /// when that write completes, which is forgiveness a peer can manufacture and
    /// so has to be budgeted. See [`forward`].
    hold: Arc<PeerHold>,
}

impl OutputSender {
    /// Whether any output this carrier handed over is still on its way. Those
    /// bytes have not been written out at all, so the phone cannot have credited
    /// them — which is what makes a credit wait beside them the daemon's own
    /// backpressure rather than a stalled consumer.
    ///
    /// Only ever compared against zero, and only by the same task that charges
    /// it, which is what makes `Relaxed` exact where it has to be: a load cannot
    /// see a value from before this task's own charge, so output just handed
    /// over can never read as delivered. The other direction — a settle not yet
    /// observed — costs one more deadline and never a false close.
    fn awaiting_delivery(&self) -> bool {
        self.undelivered.load(Relaxed) > 0
    }

    /// Whether the connection is holding output *against the socket*: taken off
    /// the queue with its write not yet returned. That write completes when the
    /// peer reads, so this is the one kind of waiting a peer can manufacture at
    /// will, and the one [`Delivery::peer_stall`] is charged for.
    ///
    /// Read under [`PeerHold`]'s lock and not off a `Relaxed` atomic the way
    /// [`Self::awaiting_delivery`] is, so it is not stale the way that one is:
    /// the count and the clock it gates are one object under one lock, which is
    /// the whole of what [`Held`] is for.
    ///
    /// It prices nothing, and that is what makes it cheap to be wrong about.
    /// The charge is elapsed time differenced from [`PeerHold::mark`] and never
    /// a verdict sampled at an expiry, so this answer can only name a wait. At
    /// [`Delivery::stall`], the one thing that asks it, this and the queue's own
    /// answer are folded together; all that turns on it is that neither is
    /// [`Stall::Phone`].
    fn write_in_flight(&self) -> bool {
        self.hold.write_in_flight()
    }

    /// Hand a chunk over from a test that cannot reach [`Delivery::hand_over`] —
    /// the connection's own tests live in another module. The same charge and the
    /// same queue, without the wait: the queues those tests use have room, so
    /// there is no stall for a stand-in deadline to answer for.
    #[cfg(test)]
    pub(crate) async fn hand_over_for_test(&self, bytes: Vec<u8>) -> bool {
        self.undelivered.fetch_add(1, Relaxed);
        if self.chunks.send(bytes).await.is_err() {
            self.undelivered.fetch_sub(1, Relaxed);
            return false;
        }
        true
    }

    /// What this stream still counts as on its way to the phone. Readable from
    /// the sender because a test may need it after the drain has been dropped.
    #[cfg(test)]
    pub(crate) fn undelivered_count(&self) -> usize {
        self.undelivered.load(Relaxed)
    }
}

/// The connection's end of the output stream. Dropping it is what tells the
/// carrier the connection is gone: the reader's next hand-over fails.
pub struct OutputDrain {
    chunks: mpsc::Receiver<Vec<u8>>,
    undelivered: Arc<AtomicUsize>,
    hold: Arc<PeerHold>,
}

impl OutputDrain {
    /// The next chunk, or `None` once the carrier's reader has ended.
    ///
    /// The chunk stays charged as undelivered until the value returned here is
    /// dropped, so a caller must hold it across the write — which is what makes
    /// the write, and not the dequeue, the moment the phone owes credit for
    /// those bytes.
    pub async fn recv(&mut self) -> Option<OutputChunk> {
        let bytes = self.chunks.recv().await?;
        // Counted as the chunk leaves the queue, because from here on it is the
        // connection holding it against the socket rather than the queue holding
        // it — the same bytes, but a different party to blame for the wait. The
        // count and the clock move together because they are the same object: the
        // span timed is exactly the span reported as in flight, with no window in
        // which one is true and the other is not.
        self.hold.began();
        Some(OutputChunk {
            bytes,
            undelivered: Arc::clone(&self.undelivered),
            hold: Arc::clone(&self.hold),
        })
    }

    /// How much is queued and not yet taken, for the tests that drive the
    /// connection's backpressure directly.
    #[cfg(test)]
    fn queued(&self) -> usize {
        self.chunks.len()
    }

    /// What this stream still counts as on its way to the phone: queued, or
    /// taken with the write unfinished. The fact the connection's own tests
    /// assert the write ordering against.
    #[cfg(test)]
    pub(crate) fn undelivered_count(&self) -> usize {
        self.undelivered.load(Relaxed)
    }
}

impl Drop for OutputDrain {
    fn drop(&mut self) {
        // What is still queued will never be written now, so it is settled here
        // rather than left owed for ever: the count means "on its way to the
        // phone", and a connection that has gone delivers nothing. Closing first
        // is what leaves almost nothing to arrive behind this drain — a
        // hand-over after it fails, and gives its own charge back.
        //
        // Almost, not nothing: a `send` that has already taken its permit can
        // still commit after the loop below has ended, and that one charge is
        // stranded. It is inert twice over — the connection's locals are
        // declared so the handle closes before the drain drops, which is what
        // wakes a reader parked on credit, and `forward`'s ceiling bounds the
        // call whatever this count says — so nothing is built on it being
        // exact. It is settled anyway because a count that means one thing
        // should mean it wherever it can.
        //
        // The peer's clock is deliberately untouched: what this loop can reach is
        // only ever queued chunks, and a chunk in flight is held by the
        // connection rather than by this queue. Its own `Drop` settles it, and
        // that runs however the write ends.
        self.chunks.close();
        while self.chunks.try_recv().is_ok() {
            self.undelivered.fetch_sub(1, Relaxed);
        }
    }
}

/// One chunk the connection has taken off the queue and is writing out.
///
/// It counts against the carrier's stall deadline until it is dropped, so
/// dropping it is the connection saying these bytes have *gone to the socket* —
/// never merely that it has them.
pub struct OutputChunk {
    bytes: Vec<u8>,
    undelivered: Arc<AtomicUsize>,
    hold: Arc<PeerHold>,
}

impl OutputChunk {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for OutputChunk {
    fn drop(&mut self) {
        // The peer's clock first, and that is the safer of the two orders rather
        // than an arbitrary one. The intermediate state a concurrent reader can
        // see is then "queued, not being written", which [`Delivery::stall`]
        // answers as the daemon's and forgives; the other order's intermediate
        // state is "being written", which would let a wait be charged for a write
        // that has in fact already returned.
        //
        // Settling the write is one step and not three — the count, the elapsed
        // time and the bytes go together under [`PeerHold`]'s lock — so there is
        // no state in which this chunk is still in flight but its bytes have not
        // been credited. That state was reachable when the count was an atomic
        // beside the clock, and a wait landing in it was charged the whole write
        // with no allowance against it.
        self.hold.ended(self.bytes.len());
        self.undelivered.fetch_sub(1, Relaxed);
    }
}

/// Who is holding a chunk up when its stall deadline expires.
enum Stall {
    /// Everything the carrier handed over has been written out, and the phone
    /// has stopped granting credit for it.
    Phone,
    /// Somebody on this side still has the output: either the connection has
    /// taken it off the queue and its write to the peer has not returned, or it
    /// has not taken it at all because it is busy with something of its own — a
    /// backlog replay, a store read.
    ///
    /// The two used to be separate answers. Nothing ever acted on the
    /// difference: the peer's stretch is charged over its interval by
    /// [`Delivery::charge_peer`] before this is asked, and the daemon's costs
    /// nothing, so both simply go round again. One name for "not the phone's"
    /// is the whole of what the caller reads.
    Ours,
}

/// Where the reader's bytes go: the credit they spend, the stream they are
/// handed to, the slot a close is recorded in, and what the attachment has left
/// to forgive the peer with. One value because every forward — live output, a
/// painted screen, a replayed buffer — needs all four.
struct Delivery {
    credit: Arc<Semaphore>,
    chunks: OutputSender,
    close_cause: Arc<std::sync::Mutex<Option<CloseCause>>>,
    /// How long this attachment has been parked with the peer holding a write,
    /// beyond what the bytes those writes carried account for. Spent against
    /// [`peer_stall_budget`], and **never given back**.
    ///
    /// Three things about the shape, each of which a previous shape got wrong.
    ///
    /// It is *elapsed time*, not a count of expired deadlines. A count can only
    /// see the peer at deadline boundaries, and the connection's own write
    /// deadline sits below the terminal's stall deadline — so a peer finishing
    /// every write just inside the write deadline held the reader parked for
    /// almost all of it and crossed no boundary at all. It was charged nothing
    /// because nothing was looking.
    ///
    /// It spans the *attachment*, and no event refills it. A budget handed back
    /// for any chunk that was charged nothing is no bound either: a peer needs
    /// only to let one chunk through cleanly between held ones to renew it for
    /// ever, and interleaving costs an attacker nothing it minds paying. What
    /// protects a phone that is honestly slow is the allowance on each write, not
    /// a refill — see [`PEER_DRAIN_WINDOW`].
    ///
    /// It does move *down*, and the distinction matters: delivered bytes take
    /// their allowance off this ledger rather than off the wait they landed in,
    /// because a wait can open and close inside a single write and see no bytes at
    /// all. That is arithmetic finishing late, not forgiveness — the only thing
    /// that ever reduces it is bytes the peer really took, at the floor rate, and
    /// it stops at zero so speed now cannot be banked against slowness later.
    /// An *event* — a clean chunk, a fresh call — reduces it by nothing.
    ///
    /// It is charged only across waits the reader was actually parked in, because
    /// the harm being bounded is *blindness*: while the reader is parked it
    /// cannot see the session's control notifications, and it is holding a lease
    /// and a tmux client while it cannot. A slow write the queue absorbed blinded
    /// nobody and costs nothing.
    ///
    /// A plain field, reached through the `&mut` every forward already takes.
    /// Only the reader task ever touches it, so an atomic would be claiming a
    /// synchronisation that is not there; a `Cell` would claim the same nothing
    /// and cost more, because the reader is spawned and a shared reference to a
    /// `Cell` is not `Send`.
    peer_stall: std::time::Duration,
}

impl Delivery {
    fn new(
        credit: Arc<Semaphore>,
        chunks: OutputSender,
        close_cause: Arc<std::sync::Mutex<Option<CloseCause>>>,
    ) -> Delivery {
        Delivery {
            credit,
            chunks,
            close_cause,
            peer_stall: std::time::Duration::ZERO,
        }
    }

    /// Who is holding a chunk up, asked when a stall deadline expires.
    ///
    /// An expiry is a point sample of a wait that has an interval, and it no
    /// longer decides the *charge*: the peer is measured over its interval by
    /// [`Delivery::charge_peer`], which is what the budget being elapsed time
    /// rather than a count of expiries means. All that turns on this answer is
    /// whether the phone is the one to close for it.
    ///
    /// Both questions are asked, and their order does not matter, because a
    /// disjunction has none. That is deliberate rather than incidental: a chunk
    /// being written is *also* counted as undelivered — [`Delivery::hand_over`]
    /// counts it before the connection can take it and [`OutputChunk::drop`]
    /// stops counting it after the write is settled — so the second question
    /// alone would answer this correctly today. Asking both keeps it correct
    /// without depending on that, and the cost of depending on it wrongly is a
    /// terminal closed as credit-starved while its write is in flight.
    fn stall(&self) -> Stall {
        if self.chunks.write_in_flight() || self.chunks.awaiting_delivery() {
            Stall::Ours
        } else {
            Stall::Phone
        }
    }

    /// Give the wait another stall deadline, and record when — which is what a
    /// test measuring "the close waited the whole deadline" anchors to, since the
    /// deadline that closed a carrier is the last one armed and not the first.
    fn rearm(&self, deadline: &mut tokio::time::Instant) {
        note_deadline_armed();
        *deadline = tokio::time::Instant::now() + output_stall_deadline();
    }

    /// Where the reader is about to park, as a reading of the peer's clock.
    fn mark(&self) -> PeerMark {
        self.chunks.hold.mark()
    }

    /// Charge the peer for a wait the reader has just come out of, and say
    /// whether the attachment has any budget left.
    ///
    /// The charge is the part of that wait a write was in flight for, less what
    /// the bytes those writes carried account for at the floor rate. Both are
    /// differences against `since`, so a wait the peer had no part in costs
    /// nothing at all rather than a rounded-down something.
    ///
    /// `Err` is the budget gone: across this attachment the peer has held the
    /// reader parked, and blind to the control stream, for [`peer_stall_budget`]
    /// longer than the bytes it accepted can explain. That is the blindness this
    /// can charge for and not all of it — only waits with a write in flight are
    /// on the clock at all, which [`forward`] states and prices.
    fn charge_peer(&mut self, since: PeerMark) -> Result<(), ForwardEnd> {
        let now = self.chunks.hold.mark();
        let held = now.held.saturating_sub(since.held);
        let delivered = now.delivered.saturating_sub(since.delivered);
        // The floor is taken against the *ledger* and not against this wait, and
        // the difference is the whole of whether an honest phone survives. A
        // write's time accrues while it is in flight but its bytes only land when
        // it completes, so a wait that begins and ends inside one write sees time
        // with no bytes against it. Floored per wait, that time is charged in full
        // and the allowance those bytes were worth is thrown away when they do
        // arrive — a peer at several times the floor rate is charged for its own
        // writes. Carried on the ledger, the same arithmetic comes out right one
        // wait later: the completion repays what the interval banked.
        //
        // Nothing is banked *below* zero, so this is not the refill that was
        // removed. A peer cannot build credit by being fast and then spend it on
        // being slow; it can only avoid being charged for bytes it really moved.
        self.peer_stall = (self.peer_stall + held).saturating_sub(drain_allowance(delivered));
        if held.is_zero() {
            // Only a wait the peer actually held may end the attachment, so a
            // wait it had no part in does not even reach the comparison:
            // `slow_consumer` must never be the daemon blaming the phone on a
            // wait the phone was absent from. A wait with no time in it can only
            // have moved the ledger down, so this cannot be hiding a crossing
            // either.
            return Ok(());
        }
        // `>=`, so the budget closes *at* four minutes of charge rather than at
        // the first charge past it. The counted-deadline version this replaces
        // went one over: eight forgivenesses each `checked_sub(1)` from eight
        // left the ninth expiry to fail, so what it actually allowed was nine
        // deadlines.
        if self.peer_stall >= peer_stall_budget() {
            return Err(ForwardEnd::SocketNotDraining);
        }
        Ok(())
    }

    /// Hand one chunk to the connection, waiting for room in its queue.
    ///
    /// A full queue is the connection not draining, which is ours to answer for
    /// and never the phone's — and that is why this can carry a deadline at all
    /// without the old defect coming back. A full queue *is* undelivered output,
    /// so [`Stall::Phone`] is not an answer this can get; it is handled with
    /// [`Stall::Ours`] rather than given a branch claiming to know what an
    /// impossible answer means. What the deadline adds is the case the old code
    /// had no answer for: a peer holding every write also holds this wait, and
    /// now spends the attachment's forgiveness doing it.
    ///
    /// `reserve` rather than `send`, because this wait has to be *retryable*: a
    /// `send(value)` whose future is dropped takes the value with it, so a losing
    /// race could not be gone round again. Reserving also lets the charge and the
    /// send sit adjacent with no await between them, which is strictly tighter
    /// than charging ahead of a wait that may never commit.
    async fn hand_over(
        &mut self,
        mut bytes: Vec<u8>,
        closed: &mut watch::Receiver<bool>,
    ) -> Result<(), ForwardEnd> {
        let mut deadline = tokio::time::Instant::now() + output_stall_deadline();
        loop {
            // Where the peer's clock stood before this wait. Taken every time
            // round, so a wait that expires several times is charged for each
            // stretch rather than once for the last of them.
            let since = self.mark();
            let mut queued = false;
            tokio::select! {
                reserved = tokio::time::timeout_at(deadline, self.chunks.chunks.reserve()) => {
                    match reserved {
                        Ok(Ok(permit)) => {
                            // Charged as the chunk is queued, with nothing
                            // awaited between the two: a settle that overtook its
                            // own charge would wrap the count and leave the
                            // carrier believing output was owed for ever.
                            self.chunks.undelivered.fetch_add(1, Relaxed);
                            // Taken out rather than moved, so the send is not a
                            // move out of a loop the compiler has to be argued
                            // with about. `queued` ends the loop on the next
                            // line but one, so the husk left here is never used.
                            permit.send(std::mem::take(&mut bytes));
                            queued = true;
                        }
                        Ok(Err(_)) => return Err(ForwardEnd::Closed),
                        // Expired with the queue still full. Who is holding it up
                        // is decided below, outside the select, so nothing here
                        // holds a borrow of the sender across that decision.
                        Err(_) => note_deadline_expired(),
                    }
                }
                _ = closed.changed() => return Err(ForwardEnd::Closed),
            }
            // Charged whether the wait ended in a slot or in an expiry, and
            // that is the point: a peer draining just fast enough to keep every
            // wait inside the deadline reaches the queue and never expires
            // anything. Charging only where a deadline expired is the defect
            // itself. Out here rather than in the arm because the reserve future
            // holds the sender borrowed for the whole of it.
            let charged = self.charge_peer(since);
            if queued {
                return charged;
            }
            charged?;
            // No question is asked of [`Delivery::stall`] here any more, and its
            // absence is the point. This wait can only ever be the peer's or the
            // daemon's — a full queue is undelivered output, so the phone cannot
            // be starving of credit — and the two are now told apart by the clock
            // rather than by an expiry: the peer's stretch has just been charged
            // above and the daemon's cost nothing to begin with. Asking would be
            // a load whose every answer is this line.
            self.rearm(&mut deadline);
        }
    }
}

/// Send one chunk to the phone, recording why the carrier ended if it did.
/// `false` means the carrier is ending and the reader must stop.
async fn deliver(bytes: &[u8], to: &mut Delivery, closed: &mut watch::Receiver<bool>) -> bool {
    match forward(bytes, to, closed).await {
        Ok(()) => true,
        Err(ForwardEnd::CreditStarved) => {
            note_cause(&to.close_cause, cause::CREDIT_STARVED);
            false
        }
        Err(ForwardEnd::SocketNotDraining) => {
            note_cause(&to.close_cause, cause::SOCKET_NOT_DRAINING);
            false
        }
        Err(ForwardEnd::Closed) => false,
    }
}

/// Why a [`forward`] ended without completing the chunk.
///
/// Both failures are the consumer failing to consume, so both are
/// `slow_consumer` on the wire; they are named apart here because the sentence
/// the phone is shown differs, and that is a thing this daemon may decide on its
/// own (see [`CloseCause`]). The third case the old code had — the daemon's own
/// loop being busy — is gone rather than renamed: it closes nothing at all now,
/// which is the honest removal of a misclassification.
#[derive(Debug)]
enum ForwardEnd {
    /// Everything the carrier handed over has been written out to the phone, and
    /// the phone has stopped granting credit for it.
    CreditStarved,
    /// The peer held the connection's writes across the whole of the
    /// attachment's forgiveness, keeping the reader parked and blind to the
    /// control stream for that entire run.
    ///
    /// The `terminal_closed` this produces rides the very socket that is not
    /// draining, so it is expected to hit the connection's own write deadline and
    /// take the whole connection with it. That is correct, and is why nothing
    /// further is needed for this case: a peer that will not read cannot be told
    /// anything, and the connection ending says everything the frame would have.
    SocketNotDraining,
    /// The carrier is closing (credit semaphore closed, receiver dropped, or
    /// the close signal fired).
    Closed,
}

/// Forward one decoded chunk under credit. Sends what the current grant covers
/// and waits for more only when none is left, so even one byte of credit makes
/// progress. Ends on the close signal, a dropped receiver, a closed credit
/// semaphore, or a stall the attachment will not forgive.
///
/// The deadline is per chunk and measures one thing: how long the phone has held
/// output that was *written out to it* without crediting it. It is taken once for
/// the whole chunk, and returning credit never extends it, so a peer that
/// trickles one byte before each deadline cannot keep the reader blocked, and
/// blind to control notifications, past the first one.
///
/// What the deadline may never charge the phone for is the daemon's own
/// backpressure. `to.chunks` is drained by the connection's select loop, which
/// polls it a pass at a time while it is doing anything else of its own — a
/// backlog replay, a store read — and which then holds each chunk it does take
/// across the socket write. So an expiry is not one condition but three, and
/// [`Delivery::stall`] asks which:
///
/// - Nothing is on its way. The phone has been given everything and has not
///   credited it, and that is [`ForwardEnd::CreditStarved`].
/// - Something is queued and untaken. The connection has not got to it; the phone
///   has never seen those bytes and cannot have credited them. Forgiven without
///   limit and **without spending anything**, because there is no bound a daemon
///   may put on its own busyness that is not just a worse version of blaming the
///   phone for it.
/// - Something is taken with its write unfinished. That write completes when the
///   peer reads its socket, so this is time the peer *manufactures* — and it is
///   spent out of [`Delivery::peer_stall`], which spans the attachment rather
///   than the chunk.
///
/// What the peer is charged, though, is not decided at those expiries. An expiry
/// is a point sample of something that has a duration, and sampling was the whole
/// defect: the connection's write deadline (20s) is *below* the stall deadline
/// (30s), so a peer that finished every write just inside its own deadline held
/// the reader parked for almost all of it and was sampled at none of it. It paid
/// nothing and — because the budget was handed back to any chunk that paid
/// nothing — it paid nothing for ever.
///
/// So every wait here is marked on entry and charged on the way out, whether it
/// expired or not, out of a budget that spans the attachment and is never given
/// back. See [`Delivery::charge_peer`] for the charge and [`PEER_DRAIN_WINDOW`]
/// for what keeps an honestly slow phone at zero.
///
/// Exhaustion is noticed when a wait ends rather than the instant it happens, so
/// the close can lag the budget by however long the write in progress runs — at
/// most [`crate::ws_server::WRITE_DEADLINE`], after which that write's own
/// deadline ends the connection anyway. Four minutes, detected inside twenty
/// seconds; a timer armed on the remainder would buy exactness worth less than
/// the arming.
///
/// So `slow_consumer` means: the consumer did not consume — either it stopped
/// crediting output it had been given, or it was charged four minutes of this
/// attachment holding output against a socket it was not draining. Charged, not
/// elapsed: what that does not reach, and what it costs, is below.
///
/// "Written out to it" is the honest boundary, and it is not the same as
/// received. The connection's `send` completes when the frame is flushed into the
/// socket, so up to a kernel send buffer of it can still be in this machine's
/// memory with the peer's receive window closed. A peer whose TCP window has
/// stalled can therefore be closed as `slow_consumer` having *received* none of
/// the bytes it is blamed for not crediting. The alternative is worse: the only
/// delivery signal below this one is the phone's own credit, which is the thing
/// being measured, so waiting for it would make the deadline unfalsifiable.
///
/// **What this does not reach**, plainly, and with the number it costs. The
/// connection's loop can also be held up by a write of some *other* frame to the
/// same peer — a flood of `ping`, whose `pong` rides the same socket, a credit
/// grant, a backlog page — and no chunk is in flight across those, so the clock
/// does not run and they are free. From where the carrier stands they are
/// indistinguishable from the daemon being busy, and charging them would put the
/// daemon's own choice of what to send on the phone's account.
///
/// The consequence is that four minutes bounds the blindness this can *see*, not
/// all of it. A peer that keeps several of the loop's arms ready — pings it sends
/// itself, input it is streaming, a replay it triggered — is served by
/// `tokio::select!` roughly one arm in `k`, so its terminal writes are that much
/// sparser and it buys around `k` times four minutes of total blindness for the
/// same charge. Measured against this loop at four ready arms: about twenty
/// minutes, for a pane cost of some 3 KiB/s.
///
/// It is bounded and finite in every case — the peer still loses the terminal —
/// but "four minutes" is the charged time and not the wall clock, and saying
/// otherwise would be the overstatement this budget was rewritten to stop
/// making. Closing the gap means timing every frame the connection writes, which
/// would charge the peer for the daemon's own sending; that is a trade worth
/// making deliberately or not at all, and it is not made here.
async fn forward(
    bytes: &[u8],
    to: &mut Delivery,
    closed: &mut watch::Receiver<bool>,
) -> Result<(), ForwardEnd> {
    // Noted before the deadline is taken, never after, so the recorded instant
    // cannot sit past the deadline's own base and a test measuring against it
    // can never see less than the full deadline.
    note_deadline_armed();
    let mut deadline = tokio::time::Instant::now() + output_stall_deadline();
    // Cloned out of `to` for the whole call: the permits taken below borrow the
    // semaphore, and `to` is needed mutably in the same breath to charge the
    // peer, so the two are kept apart rather than reasoned about.
    let credit = Arc::clone(&to.credit);
    let mut sent = 0;
    while sent < bytes.len() {
        // Block for the first byte of credit, bounded by the stall deadline so a
        // phone that has stopped rendering does not hold the reader (and tmux's
        // output buffer) open. Then take as much of the remainder as the grant
        // currently covers.
        let first = loop {
            // Marked round each turn of this wait, not once for the loop: a wait
            // that expires and re-arms is several stretches of the peer's time
            // and is charged as each of them.
            let since = to.mark();
            let acquired = tokio::select! {
                acquired = tokio::time::timeout_at(deadline, credit.acquire()) => acquired,
                _ = closed.changed() => return Err(ForwardEnd::Closed),
            };
            match acquired {
                Ok(Ok(permit)) => {
                    // Charged before the permit is used, because credit arriving
                    // is exactly when a write the peer was holding has landed —
                    // the stretch that just ended is the one it held.
                    to.charge_peer(since)?;
                    break permit;
                }
                Ok(Err(_)) => return Err(ForwardEnd::Closed),
                Err(_) => {
                    note_deadline_expired();
                    to.charge_peer(since)?;
                    match to.stall() {
                        Stall::Phone => return Err(ForwardEnd::CreditStarved),
                        // See the hand-over: the peer's stretch has just been
                        // charged, and the daemon's own costs nothing, so this
                        // simply goes round again.
                        Stall::Ours => to.rearm(&mut deadline),
                    }
                }
            }
        };
        first.forget();
        // Take as much of the remainder as the grant currently covers — never
        // more than is available, so a small grant against a big chunk sends a
        // big prefix, not a one-byte sliver. `try_acquire_many` is
        // all-or-nothing, so it is asked only for what `available_permits`
        // already shows (this task is the sole consumer, so that floor cannot
        // shrink under it); on the semaphore closing it yields zero and the
        // next `acquire` ends the loop.
        //
        // And never more than one frame may carry. The connection puts each
        // hand-over on the wire as a single `terminal_output`, and
        // `MAX_TERMINAL_CHUNK_BYTES` is documented as receiver-enforced — so
        // without this the phone's grant, not the protocol, decided the frame
        // size, and an attach snapshot of a busy pane routinely exceeded the
        // bound a conforming client would close the terminal over. The credit
        // machinery already sends chunks in pieces, so the cap costs a loop
        // turn and nothing else.
        let want = (bytes.len() - sent - 1).min(protocol::ws::MAX_TERMINAL_CHUNK_BYTES - 1) as u32;
        let grantable = want.min(credit.available_permits() as u32);
        let extra = match credit.try_acquire_many(grantable) {
            Ok(permits) => {
                let n = permits.num_permits();
                permits.forget();
                n
            }
            Err(_) => 0,
        };
        let take = 1 + extra;
        let parked = tokio::time::Instant::now();
        to.hand_over(bytes[sent..sent + take].to_vec(), closed)
            .await?;
        // Whatever that took is given back to the *credit* deadline, so the rest
        // of this chunk keeps the clock it had left rather than paying for the
        // hand-over out of it. The peer's budget is a separate ledger and is
        // deliberately not given anything back here or anywhere else.
        deadline += parked.elapsed();
        sent += take;
    }
    Ok(())
}

/// Queue what a command's reply will mean, then write the command. Tagging and
/// writing are one operation so a command cannot reach tmux untagged — the
/// reader's correlation is exact only if every block it sees has a tag behind
/// it — and the tag goes on first, so a reply can never overtake it. Raced
/// against the close signal like every other write.
async fn write_command(
    stdin: &mut ChildStdin,
    replies: &ReplyQueue,
    expect: ExpectedReply,
    line: &[u8],
    closed: &mut watch::Receiver<bool>,
) -> bool {
    queue_reply(replies, expect);
    tokio::select! {
        result = stdin.write_all(line) => result.is_ok(),
        _ = closed.changed() => false,
    }
}

/// The writer task: owns the client's stdin. Seeds, sizes, asks for a snapshot
/// of every pane the reader binds, and turns input into `send-keys` at the
/// *bound scoped target* and resizes into `refresh-client`. Input is held until
/// the target is bound, so a keystroke can never reach a pane the phone was not
/// shown. Every write is raced against the close signal, so a client that stops
/// reading can never hold anything but this task. Each input chunk's byte count
/// is reported back once it has reached the pipe, which is what replenishes the
/// phone's input credit — credit returns only for bytes that were handed to the
/// client. On close it asks the client to detach; the reaper enforces the
/// deadline behind it.
#[allow(clippy::too_many_arguments)]
async fn write_client(
    stdin: ChildStdin,
    session_id: String,
    mut input: mpsc::UnboundedReceiver<Vec<u8>>,
    mut resize: watch::Receiver<(u16, u16)>,
    mut target: watch::Receiver<Option<Target>>,
    replies: ReplyQueue,
    input_written: mpsc::UnboundedSender<u32>,
    close: watch::Sender<bool>,
    mut closed: watch::Receiver<bool>,
) {
    let mut stdin = stdin;

    // First, ask the client which window and pane it is on — its reply is how
    // the reader binds them, before any size change perturbs the layout. (A
    // test may delay this to guarantee the pane has produced output first.)
    hold_seed_for_test().await;
    let mut ok = if skip_seed_for_test() {
        true
    } else {
        let seed = tmux::seed_query_line(&session_id).into_bytes();
        write_command(
            &mut stdin,
            &replies,
            ExpectedReply::Seed,
            &seed,
            &mut closed,
        )
        .await
    };
    // Then the initial size the watch was seeded with.
    if ok {
        let (cols, rows) = *resize.borrow_and_update();
        let initial = tmux::resize_line(cols, rows).into_bytes();
        ok = write_command(
            &mut stdin,
            &replies,
            ExpectedReply::Empty,
            &initial,
            &mut closed,
        )
        .await;
    }
    'outer: while ok {
        tokio::select! {
            bytes = input.recv() => {
                let Some(bytes) = bytes else { break };
                // The target input goes to: whatever the reader has bound *now*,
                // never the one a snapshot is still being taken for. It is
                // always `Some` here — input only reaches this task after the
                // attach gate confirmed the seed — but a missing binding is
                // dropped rather than guessed.
                let Some(dest) = target.borrow().as_ref().map(|bound| bound.dest.clone()) else { continue };
                // Split across commands: tmux parses each argument onto a yacc
                // stack, and one maximal 16 KiB chunk as a single command
                // overflows it (measured: `parse error: yacc stack overflow`).
                // 1 KiB per command is well inside the limit, and `send-keys`
                // is ordered, so the pane sees one unbroken paste.
                for chunk in bytes.chunks(SEND_KEYS_BYTES_PER_LINE) {
                    let line = tmux::send_keys_line(&dest, chunk).into_bytes();
                    ok = write_command(&mut stdin, &replies, ExpectedReply::Empty, &line, &mut closed).await;
                    if !ok {
                        break 'outer;
                    }
                }
                // Delivered to the client: return exactly these input bytes of
                // credit.
                if input_written.send(bytes.len() as u32).is_err() {
                    break;
                }
            }
            // A newly bound or newly followed pane. Ask for its screen and its
            // cursor, in that order and adjacently: control mode only streams
            // what a pane prints *next*, so without this the phone waits at a
            // blank viewport for output that may never come. Both tags carry
            // the generation of the target read here, so a reply that arrives
            // after the carrier has followed the focus somewhere else is
            // recognised as the wrong pane's rather than painted. The reader is
            // holding that pane's output until the pair lands, so the paint and
            // the stream cannot both carry the same bytes. (A test may hold
            // either command to prove those windows are real.)
            changed = target.changed() => {
                if changed.is_err() {
                    break;
                }
                let bound = target.borrow_and_update().clone();
                let Some(Target { generation, dest }) = bound else { continue };
                let dest = snapshot_target_for_test(dest);
                hold_capture_for_test().await;
                let capture = tmux::capture_line(&dest).into_bytes();
                ok = write_command(&mut stdin, &replies, ExpectedReply::Capture(generation), &capture, &mut closed).await;
                if ok {
                    hold_cursor_for_test().await;
                    let dest = cursor_target_for_test(dest);
                    let cursor = tmux::cursor_query_line(&dest).into_bytes();
                    ok = write_command(&mut stdin, &replies, ExpectedReply::Cursor(generation), &cursor, &mut closed).await;
                }
            }
            changed = resize.changed() => {
                if changed.is_err() {
                    break;
                }
                let (cols, rows) = *resize.borrow_and_update();
                let line = tmux::resize_line(cols, rows).into_bytes();
                ok = write_command(&mut stdin, &replies, ExpectedReply::Empty, &line, &mut closed).await;
            }
            _ = closed.changed() => break,
        }
    }
    // Ask for the graceful end, bounded — a client no longer reading its pipe
    // gets the reaper's kill instead. Dropping stdin is the EOF backstop.
    queue_reply(&replies, ExpectedReply::Empty);
    let _ = tokio::time::timeout(
        TEARDOWN_GRACE,
        stdin.write_all(tmux::DETACH_LINE.as_bytes()),
    )
    .await;
    drop(stdin);
    let _ = close.send(true);
}

/// The reaper task: owns the child and the leases. Whatever path ends the
/// carrier, this is where the client is waited on — so the process is reaped
/// on every path and a session's one-terminal slot frees only after its
/// previous client is dead. A client that exits on its own (its session ended,
/// or tmux dropped it) broadcasts the close so the reader, writer and
/// connection all tear down rather than one of them lingering.
async fn reap_client(
    mut child: Child,
    lease: Lease,
    close: watch::Sender<bool>,
    mut closed: watch::Receiver<bool>,
) {
    let exited_first = tokio::select! {
        _ = child.wait() => true,
        _ = closed.changed() => false,
    };
    if exited_first {
        // The client died under us; tell everyone else.
        let _ = close.send(true);
    } else {
        // We are tearing it down; give the writer's detach its grace, then
        // kill. The child holds `kill_on_drop`, so even if the bounded waits
        // expire the process cannot survive this task returning.
        if tokio::time::timeout(TEARDOWN_GRACE, child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(TEARDOWN_GRACE, child.wait()).await;
        }
    }
    drop(lease);
}

/// Serialises the tmux-fixture terminal tests, in this module and in
/// `ws_server`, so they never run at once. One of them counts this process's
/// open descriptors to prove the attachment leaks none — a measurement that
/// only holds while no sibling test is holding a pipe or socket open. A
/// `tokio::sync::Mutex` because the guard is held across the tests' awaits;
/// it does not poison, so a panicking test still frees the next.
#[cfg(test)]
pub(crate) async fn fixture_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A private throwaway tmux server carrying one stamped session, torn down
    /// however the test ends. Shared with the `ws_server` tests so both layers
    /// prove themselves against the same fixture.
    pub(crate) struct Fixture {
        bin: std::path::PathBuf,
        dir: std::path::PathBuf,
        pub sock: String,
        pub uid: String,
    }

    impl Fixture {
        pub fn start() -> Option<Fixture> {
            Self::start_running("/bin/sh")
        }

        pub fn start_running(command: &str) -> Option<Fixture> {
            // Only a *missing* tmux is a skip; once tmux is present, a setup
            // failure is a real failure, not silently a passing "no tmux" — a
            // skip that hides broken setup would let a regression rot green.
            let bin = tmux::tmux_bin()?;
            let dir = std::env::temp_dir().join(format!(
                "cc-term-{}-{}",
                std::process::id(),
                protocol::time::now_unix_ms()
            ));
            std::fs::create_dir_all(&dir).expect("create the fixture dir");
            let sock = dir.join("sock").to_string_lossy().into_owned();
            // A real minted uid: the resolver refuses anything not well-formed.
            let uid = protocol::uid::new().expect("mint a uid");
            let status = std::process::Command::new(&bin)
                .args(["-S", &sock, "-f", "/dev/null"])
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    "cc-term",
                    "-x",
                    "80",
                    "-y",
                    "24",
                    "-e",
                    &format!("{}={uid}", protocol::ENV_SESSION_UID),
                    "--",
                    command,
                ])
                .status()
                .expect("run tmux new-session");
            assert!(status.success(), "tmux new-session failed");
            Some(Fixture {
                bin,
                dir,
                sock,
                uid,
            })
        }

        pub fn tmux(&self, args: &[&str]) -> std::process::Output {
            std::process::Command::new(&self.bin)
                .args(["-S", &self.sock, "-f", "/dev/null"])
                .args(args)
                .output()
                .unwrap()
        }

        pub fn alive(&self) -> bool {
            self.tmux(&["has-session", "-t", "=cc-term"])
                .status
                .success()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.tmux(&["kill-server"]);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The process's open *pipe* descriptors.
    ///
    /// Pipes and not everything, because everything is not this carrier's to
    /// answer for. A carrier holds exactly two descriptors of its own — its
    /// child's stdin and stdout, both pipes — while the process-wide total is
    /// dominated by the test suite around it: every `#[tokio::test]` running
    /// beside this one holds a kqueue and a waker socket, so an absolute count
    /// measures how many other tests happen to be in flight at that instant and
    /// moves by several whenever a test is added anywhere in the binary.
    /// Counting the class the carrier actually opens is the same claim without
    /// the noise. (The two pipes the harness itself holds — stdout and stderr —
    /// are in every reading and cancel out of the comparison.)
    fn open_pipes() -> usize {
        use std::os::unix::fs::FileTypeExt;
        let Ok(entries) = std::fs::read_dir("/dev/fd") else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .metadata()
                    .map(|meta| meta.file_type().is_fifo())
                    .unwrap_or(false)
            })
            .count()
    }

    /// The fixture session's one pane. A session anchor is not a pane target,
    /// so every pane-scoped tmux command in these tests names the id.
    fn only_pane(fx: &Fixture) -> String {
        let out = fx.tmux(&["list-panes", "-t", "=cc-term", "-F", "#{pane_id}"]);
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .expect("the session has a pane")
            .to_string()
    }

    /// The fixture session's pane that is *not* the active one, for tests that
    /// need a second live pane the carrier is not bound to.
    fn inactive_pane(fx: &Fixture) -> String {
        let out = fx.tmux(&[
            "list-panes",
            "-t",
            "=cc-term",
            "-F",
            "#{pane_active} #{pane_id}",
        ]);
        let out = String::from_utf8_lossy(&out.stdout);
        out.lines()
            .find(|line| line.starts_with('0'))
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("an inactive second pane")
            .to_string()
    }

    /// The fixture session's `session:window` prefix, so a test can name the
    /// same scoped `session:window.pane` composite the carrier does.
    fn window_scope(fx: &Fixture) -> String {
        let out = fx.tmux(&[
            "display-message",
            "-p",
            "-t",
            "=cc-term",
            "#{session_id}:#{window_id}",
        ]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn process_gone(pid: i32) -> bool {
        // `kill -0`: probes existence without signalling. ESRCH means gone;
        // a zombie still answers, so this also proves the reap happened.
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|out| !out.status.success())
            .unwrap_or(false)
    }

    /// Input bytes the writer has reported written to the client, accumulated
    /// by the ack drain below. Not a hook into the carrier: this is the
    /// production `input_written` channel — what the connection turns into
    /// replenished input credit — read by the test harness instead of dropped.
    /// The writer reports a chunk only after its `send-keys` has reached the
    /// pipe, and it reads the target *before* writing, so this is the barrier
    /// that says an input's target is already chosen and can no longer move.
    static TEST_INPUT_WRITTEN: AtomicUsize = AtomicUsize::new(0);

    /// Open a carrier and drain its input acks in the background, the way the
    /// connection's ack arm does, so the writer never blocks reporting delivery.
    async fn open_against(
        fx: &Fixture,
        leases: &TerminalLeases,
        credit: usize,
    ) -> (TerminalHandle, OutputDrain, Arc<Semaphore>) {
        // Every counter here is a per-carrier fact, so they all start from this
        // attach rather than from whatever the previous test left behind.
        // `TEST_WITHHELD` is also cleared by arming a `GateHold`; resetting it
        // here is what makes that true for a test that arms no gate.
        TEST_PUBLISHED.store(0, Relaxed);
        TEST_SNAPSHOT_REPLIES.store(0, Relaxed);
        TEST_WITHHELD.store(0, Relaxed);
        TEST_OUTPUT_SEEN.store(0, Relaxed);
        TEST_INPUT_WRITTEN.store(0, Relaxed);
        *TEST_DEADLINE_ARMED
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        let sem = Arc::new(Semaphore::new(credit));
        let (tx, rx) = output_channel(4);
        let (acks_tx, mut acks_rx) = mpsc::unbounded_channel::<u32>();
        tokio::spawn(async move {
            while let Some(bytes) = acks_rx.recv().await {
                TEST_INPUT_WRITTEN.fetch_add(bytes as usize, Relaxed);
            }
        });
        let handle = TerminalHandle::open(
            leases,
            &fx.sock,
            &fx.uid,
            80,
            24,
            Arc::clone(&sem),
            tx,
            acks_tx,
        )
        .await
        .expect("the stamped session accepts a terminal");
        (handle, rx, sem)
    }

    /// The [`Delivery`] `read_client` builds, over a channel a test made itself,
    /// for the tests that drive [`forward`] and the hand-over directly.
    ///
    /// Built through `Delivery::new` rather than field by field, so the
    /// forgiveness a test starts with is the one an attachment starts with — a
    /// literal here would let the budget under test drift from the constant the
    /// carrier actually uses.
    fn delivery_over(credit: &Arc<Semaphore>, chunks: OutputSender) -> Delivery {
        Delivery::new(
            Arc::clone(credit),
            chunks,
            Arc::new(std::sync::Mutex::new(None)),
        )
    }

    /// Poll `capture-pane` on a pane until its screen shows `needle` — a barrier
    /// proving that pane has actually produced the output, so a later "this
    /// pane's output must NOT appear in the stream" assertion is not vacuously
    /// true merely because the output had not been generated yet.
    async fn wait_pane_shows(fx: &Fixture, pane: &str, needle: &str) {
        for _ in 0..50 {
            let out = fx.tmux(&["capture-pane", "-p", "-t", pane]);
            if String::from_utf8_lossy(&out.stdout).contains(needle) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("pane {pane} never showed {needle:?}");
    }

    /// Wait for a carrier fact to become true, polling for two seconds and
    /// failing with what it saw instead — the shape every barrier below takes,
    /// so a test acts on the carrier having actually reached a state rather
    /// than on a timer that hopes it has.
    async fn wait_for<T, F>(what: &str, want: T, seen: F)
    where
        T: PartialOrd + std::fmt::Debug + Copy,
        F: Fn() -> T,
    {
        for _ in 0..100 {
            if seen() >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("{what} reached {:?}, not {want:?}", seen());
    }

    /// Wait until at least `chunks` `%output` chunks have been withheld, so a
    /// test can act on the pane having produced output the carrier held back or
    /// dropped rather than on a timer.
    async fn wait_withheld(chunks: usize) {
        wait_for("withheld output chunks", chunks, withheld).await
    }

    /// Wait until the reader has answered `replies` snapshot commands, which is
    /// the proof that tmux *ran* them — a stronger fact than the writer having
    /// written them.
    async fn wait_snapshot_replies(replies: usize) {
        wait_for("answered snapshot replies", replies, snapshot_replies).await
    }

    /// Wait until the reader has bound its `generation`th target, so a test can
    /// act on a pane switch having been published rather than merely requested.
    async fn wait_published(generation: u64) {
        wait_for(
            "published target generations",
            generation,
            published_generation,
        )
        .await
    }

    /// Wait until the writer has reported `bytes` of input written to the
    /// client. Handing bytes to [`TerminalHandle::input`] only queues them; the
    /// writer reads the target it will send them to when it dequeues, and
    /// reports them once the `send-keys` has reached the pipe. Waiting for the
    /// report is therefore how a test knows an input's target was chosen before
    /// it moves that target underneath the writer.
    async fn wait_input_written(bytes: usize) {
        wait_for("input bytes written", bytes, || {
            TEST_INPUT_WRITTEN.load(Relaxed)
        })
        .await
    }

    /// Wait until `commands` commands have reached a held gate, so a test knows
    /// the writer has committed to a target before it changes that target.
    async fn wait_arrived(hold: &GateHold, commands: usize) {
        wait_for("commands held at the gate", commands, || hold.arrived()).await
    }

    /// Keep draining and crediting for `window`, appending everything that
    /// arrives — for asserting on what follows a marker, or that nothing does.
    /// Each chunk is dropped where a connection would have finished writing it,
    /// which is what settles it against the carrier's delivery count.
    async fn drain_for(
        rx: &mut OutputDrain,
        sem: &Semaphore,
        into: &mut Vec<u8>,
        window: std::time::Duration,
    ) {
        let started = std::time::Instant::now();
        while started.elapsed() < window {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Some(chunk)) => {
                    sem.add_permits(chunk.bytes().len());
                    into.extend_from_slice(chunk.bytes());
                }
                Ok(None) => return,
                Err(_) => {}
            }
        }
    }

    /// Where the last cursor-position sequence (`ESC [ row ; col H`) starts in
    /// `bytes`, or `None` if there is none. The snapshot ends with one, so this
    /// is how a test tells a painted screen from a stream of live output.
    fn last_cursor_move(bytes: &[u8]) -> Option<usize> {
        (0..bytes.len()).rev().find(|&at| {
            let Some(rest) = bytes[at..].strip_prefix(b"\x1b[") else {
                return false;
            };
            let Some(end) = rest.iter().position(|&b| b == b'H') else {
                return false;
            };
            let mut halves = rest[..end].split(|&b| b == b';');
            let digits = |part: &[u8]| !part.is_empty() && part.iter().all(u8::is_ascii_digit);
            halves.next().is_some_and(digits)
                && halves.next().is_some_and(digits)
                && halves.next().is_none()
        })
    }

    /// How many times `needle` occurs in `bytes`. Overlap is impossible for the
    /// markers these tests use, so a simple forward scan is exact.
    fn occurrences(bytes: &[u8], needle: &str) -> usize {
        String::from_utf8_lossy(bytes).matches(needle).count()
    }

    /// Wait until the pane shows `needle`, draining and crediting output like
    /// a phone does, and return everything streamed meanwhile.
    async fn stream_until(
        rx: &mut OutputDrain,
        sem: &Semaphore,
        needle: &str,
        deadline: std::time::Duration,
    ) -> Option<Vec<u8>> {
        let mut streamed = Vec::new();
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Some(chunk)) => {
                    sem.add_permits(chunk.bytes().len());
                    streamed.extend_from_slice(chunk.bytes());
                    if String::from_utf8_lossy(&streamed).contains(needle) {
                        return Some(streamed);
                    }
                }
                Ok(None) => return None,
                Err(_) => {}
            }
        }
        None
    }

    /// The carrier's whole happy path against real tmux: attach to the exact
    /// stamped session, stream its output under credit, type into it —
    /// including multi-byte UTF-8 and a 16 KiB paste — resize it as sole
    /// client, and prove the prefix key is inert data. One test because each
    /// step depends on the state the previous one proved.
    #[tokio::test]
    async fn a_terminal_streams_types_resizes_and_contains() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let leases = TerminalLeases::new();
        let baseline = open_pipes();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        // Typed input reaches the pane and its echo streams back — with UTF-8
        // passing through both directions byte-exactly.
        handle
            .input("echo done-é€\r".as_bytes().to_vec())
            .expect("input reaches the writer");
        let streamed = stream_until(&mut rx, &sem, "done-é€", std::time::Duration::from_secs(5))
            .await
            .expect("the echoed marker streams back");
        assert!(String::from_utf8_lossy(&streamed).contains("done-é€"));

        // As sole client, the phone's size governs the window.
        handle.resize(120, 40);
        let mut sized = false;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let out = fx.tmux(&[
                "list-windows",
                "-t",
                "=cc-term",
                "-F",
                "#{window_width}x#{window_height}",
            ]);
            if String::from_utf8_lossy(&out.stdout).trim() == "120x40" {
                sized = true;
                break;
            }
        }
        assert!(sized, "the sole client's size governs");

        // Containment: Ctrl-B is the tmux prefix on a raw client. Here it is
        // data. A second session exists to switch into; prove the client stays
        // on the resolved session and the server keeps both sessions. The decoy
        // must actually exist or the containment claim would be vacuous.
        assert!(
            fx.tmux(&["new-session", "-d", "-s", "decoy", "--", "/bin/sh"])
                .status
                .success(),
            "the decoy session to switch into exists"
        );
        assert!(
            fx.tmux(&["has-session", "-t", "=decoy"]).status.success(),
            "the decoy session is switchable-to"
        );
        handle
            .input(vec![0x02, b')'])
            .expect("input reaches the writer");
        handle
            .input(b"echo after-prefix\r".to_vec())
            .expect("input reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "after-prefix",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the pane still answers after a would-be prefix");
        // The production format, borrowed rather than transcribed. A copy of it
        // here is how the reverify format would get left behind the next time the
        // wire shape changes — which is exactly how it carried a `\x1f` that no
        // daemon under launchd could read.
        let clients = fx.tmux(&["list-clients", "-F", protocol::tmux::CLIENTS_FMT]);
        let expected = format!("{} {}", handle.client_pid(), handle.session_id());
        assert!(
            String::from_utf8_lossy(&clients.stdout)
                .lines()
                .any(|l| l == expected),
            "the client never left the resolved session"
        );

        // Close: the session survives its viewer, the client is reaped, and
        // the process holds no more descriptors than it started with.
        let pid = handle.client_pid();
        drop(handle);
        let mut reaped = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if process_gone(pid) {
                reaped = true;
                break;
            }
        }
        assert!(reaped, "the disposable client is killed and reaped");
        assert!(fx.alive(), "the session outlives its viewer");
        // Polled to settle: the reap proves the process is gone, not that the
        // runtime has finished dropping the pipes it held, and a descriptor
        // count read a scheduling hop too early would blame a leak for a
        // close still in flight.
        for _ in 0..40 {
            if open_pipes() <= baseline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let after = open_pipes();
        assert!(
            // One of slack and not two: a carrier holds exactly two pipes, so
            // an allowance of two is an allowance for a whole leaked carrier.
            // The one covers a stray child elsewhere in the binary without
            // covering the thing this is looking for.
            after <= baseline + 1,
            "pipe descriptors grew from {baseline} to {after}"
        );
    }

    /// The carrier follows the active pane: in a split session only the active
    /// pane streams (no interleave), when focus moves the stream moves with it —
    /// so a pane that dies, replaced by tmux activating another, never leaves
    /// the carrier bound to something gone — and the first thing the new pane
    /// streams is a snapshot of what it was already showing.
    #[tokio::test]
    async fn the_stream_follows_the_active_pane() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        // Split but keep focus on the original pane (`-d`), targeting its pane
        // id (a session anchor is not a valid `split-window` target). The new
        // pane is a background `cat`, whose echo would betray any interleave.
        let original = {
            let out = fx.tmux(&["list-panes", "-t", "=cc-term", "-F", "#{pane_id}"]);
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .expect("the session has a pane")
                .to_string()
        };
        // The new pane runs `tr a-z A-Z` with tty echo OFF, so the only thing it
        // emits is the upper-cased transform of what it is sent — never a tty
        // echo of the raw input. That makes "did input reach *this* pane"
        // independently checkable: a regression that left input on the original
        // shell would surface the lower-case text, never the upper-case form.
        assert!(
            fx.tmux(&[
                "split-window",
                "-d",
                "-t",
                &original,
                "--",
                "/bin/sh",
                "-c",
                "stty -echo; exec tr a-z A-Z",
            ])
            .status
            .success(),
            "the window splits"
        );
        let panes = fx.tmux(&[
            "list-panes",
            "-t",
            "=cc-term",
            "-F",
            "#{pane_active} #{pane_id}",
        ]);
        let panes = String::from_utf8_lossy(&panes.stdout);
        let lines: Vec<&str> = panes.lines().collect();
        assert!(lines.len() >= 2, "the split created a second pane");
        let inactive = lines
            .iter()
            .find(|l| l.starts_with('0'))
            .and_then(|l| l.split_whitespace().nth(1))
            .expect("an inactive pane")
            .to_string();

        // The inactive `tr` pane is fed a marker that, upper-cased, must never
        // appear in the stream. A barrier waits until that pane has actually
        // produced `ZZZ_INACTIVE` *before* the active marker is sent, so the
        // exclusion is not vacuously true just because the output was late: a
        // "forward every pane" regression would have had it in hand.
        assert!(
            fx.tmux(&["send-keys", "-t", &inactive, "-l", "zzz_inactive\n"])
                .status
                .success(),
            "the inactive pane is fed a marker"
        );
        wait_pane_shows(&fx, &inactive, "ZZZ_INACTIVE").await;
        handle
            .input(b"echo ACTIVE_ONE\r".to_vec())
            .expect("input reaches the writer");
        let streamed = stream_until(
            &mut rx,
            &sem,
            "ACTIVE_ONE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the active pane streams");
        assert!(
            !String::from_utf8_lossy(&streamed).contains("ZZZ_INACTIVE"),
            "the inactive pane must not interleave, got {:?}",
            String::from_utf8_lossy(&streamed)
        );

        // Move focus to the `tr` pane and wait for the carrier to have bound it
        // before typing — input deliberately follows the pane the phone is
        // *shown*, so it must not be sent until the reader has moved its
        // target. Waiting on the second publish is that fact; a timer would
        // only hope for it, and would blame the input path when it lost.
        // Send lower-case; only if input reached the `tr` pane AND its output
        // is what is now forwarded does the upper-cased form appear. A
        // regression (input left on the shell, or the shell still followed)
        // would surface the lower-case echo, never this.
        assert!(
            fx.tmux(&["select-pane", "-t", &inactive]).status.success(),
            "focus moves to the tr pane"
        );
        wait_published(2).await;
        handle
            .input(b"followed_marker\n".to_vec())
            .expect("input reaches the writer");
        let followed = stream_until(
            &mut rx,
            &sem,
            "FOLLOWED_MARKER",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("input and output both moved to the newly active pane");
        assert!(
            !String::from_utf8_lossy(&followed).contains("followed_marker"),
            "the lower-case echo must not appear — that would mean input stayed on the shell"
        );
        // The switch repaints: `ZZZ_INACTIVE` was printed by the `tr` pane
        // *before* the focus moved and never printed again, so the only way it
        // can be in the stream after the switch is a snapshot of what that pane
        // was already showing — which is exactly what a fresh viewer needs and
        // what control mode's forward-only `%output` cannot give.
        let after = String::from_utf8_lossy(&followed).into_owned();
        let painted = after
            .find("ZZZ_INACTIVE")
            .unwrap_or_else(|| panic!("the newly active pane's screen is painted, got {after:?}"));
        assert!(
            after[..painted].contains("\u{1b}[2J"),
            "the pre-existing marker arrives inside a repaint, got {after:?}"
        );
        drop(handle);
    }

    /// A pane focus change in ANOTHER session — which tmux broadcasts to every
    /// control client — must not disturb this carrier: not seed its pane, not
    /// close it. Proven by streaming our own session before and after.
    #[tokio::test]
    async fn another_sessions_pane_change_does_not_disturb_the_carrier() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        // A second, unrelated session with two panes to switch between. Every
        // setup step is asserted so a tmux failure fails the test rather than
        // silently producing no stimulus.
        assert!(
            fx.tmux(&["new-session", "-d", "-s", "other", "--", "/bin/sh"])
                .status
                .success(),
            "the other session is created"
        );
        let first = {
            let out = fx.tmux(&["list-panes", "-t", "=other", "-F", "#{pane_id}"]);
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .expect("the other session has a pane")
                .to_string()
        };
        assert!(
            fx.tmux(&["split-window", "-d", "-t", &first, "--", "/bin/sh"])
                .status
                .success(),
            "the other session splits"
        );
        let other_inactive = {
            let out = fx.tmux(&[
                "list-panes",
                "-t",
                "=other",
                "-F",
                "#{pane_active} #{pane_id}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout);
            out.lines()
                .find(|l| l.starts_with('0'))
                .and_then(|l| l.split_whitespace().nth(1))
                .expect("the other session has an inactive pane")
                .to_string()
        };

        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        handle
            .input(b"echo BEFORE_X\r".to_vec())
            .expect("input reaches the writer");
        stream_until(&mut rx, &sem, "BEFORE_X", std::time::Duration::from_secs(5))
            .await
            .expect("our session streams");

        // Poke the OTHER session's active pane, and switch its active window:
        // both a cross-session %window-pane-changed and a cross-session
        // %session-window-changed reach our client. Both must be ignored. Each
        // stimulus is asserted so a silent tmux failure cannot rot the test.
        assert!(
            fx.tmux(&["select-pane", "-t", &other_inactive])
                .status
                .success(),
            "the other session's pane is selected"
        );
        assert!(
            fx.tmux(&["new-window", "-t", "=other"]).status.success(),
            "the other session gets a new active window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Our carrier is untouched: still open, still streaming our session.
        handle
            .input(b"echo AFTER_X\r".to_vec())
            .expect("input reaches the writer");
        stream_until(&mut rx, &sem, "AFTER_X", std::time::Duration::from_secs(5))
            .await
            .expect("our session still streams after another session's pane and window changes");
        drop(handle);
    }

    /// A valid attach to an actively-producing pane with only one byte of
    /// output credit still succeeds: nothing that can block on credit stands
    /// between the seed reply and the target publish the attach gate waits on.
    /// The seed is held *causally* until the pane has produced several chunks
    /// the carrier withheld, so the pane is proven busy when it binds — an
    /// arrangement that would flush-block on the one credit byte and time the
    /// 2s attach gate out.
    #[tokio::test]
    async fn a_noisy_pane_with_one_credit_still_attaches() {
        let _serial = fixture_test_guard().await;
        // A foreground pane printing continuously from before attach.
        let Some(fx) = Fixture::start_running("while :; do echo NOISE; sleep 0.02; done") else {
            eprintln!("skipped: no tmux");
            return;
        };
        // Hold the seed until at least three `%output` chunks have been
        // withheld, so the pane is genuinely producing when it binds. The
        // hold reopens the gate even if the attach panics.
        let _seed = GateHold::until_output(&SEED_GATE, 3);
        let leases = TerminalLeases::new();
        // One byte of credit: the snapshot blocks on it almost immediately. The
        // attach must still succeed (open_against panics if it does not).
        let (handle, mut rx, sem) = open_against(&fx, &leases, 1).await;
        // Fail closed: prove the seed was released by the threshold, not by the
        // hold's own timeout — otherwise a slow client could let this pass
        // without the pane ever having produced anything.
        assert!(
            output_seen() >= 3,
            "the seed was held until the pane had actually produced output"
        );
        // And the stream is live: crediting drains the noise the pane produces.
        stream_until(&mut rx, &sem, "NOISE", std::time::Duration::from_secs(5))
            .await
            .expect("the noisy pane streams once credited");
        drop(handle);
    }

    /// If the seed never binds — a wedged client or an errored seed query — the
    /// attach fails rather than returning a terminal that would show nothing and
    /// have no pane for input to target. The lease is released, so the session
    /// can be attached again.
    #[tokio::test]
    async fn a_seed_that_never_binds_fails_the_attach() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let suppressed = SeedSuppressed::hold();
        let leases = TerminalLeases::new();
        let sem = Arc::new(Semaphore::new(64 * 1024));
        let (tx, _rx) = output_channel(4);
        let (acks_tx, _acks_rx) = mpsc::unbounded_channel();
        let result =
            TerminalHandle::open(&leases, &fx.sock, &fx.uid, 80, 24, sem, tx, acks_tx).await;
        drop(suppressed);
        assert!(
            matches!(result, Err(OpenError::Unavailable(_))),
            "an unbound seed fails the attach, got {:?}",
            result.map(|_| "a live handle")
        );
        // The lease releases once the failed client is reaped, so the session
        // accepts a real attach afterwards (retried across that window).
        let mut reattached = None;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let sem = Arc::new(Semaphore::new(64 * 1024));
            let (tx, _rx) = output_channel(4);
            let (acks_tx, _acks_rx) = mpsc::unbounded_channel();
            match TerminalHandle::open(&leases, &fx.sock, &fx.uid, 80, 24, sem, tx, acks_tx).await {
                Ok(again) => {
                    reattached = Some(again);
                    break;
                }
                Err(OpenError::AttachmentLimit(_)) => continue,
                Err(err) => panic!("re-attach failed for the wrong reason: {err:?}"),
            }
        }
        drop(reattached.expect("the lease came back after the failed attach"));
    }

    /// A pane change in a *background window of our own session* — which carries
    /// a different `@window` — is not our view and must be ignored: the carrier
    /// keeps streaming the active window, stays bound to the pane it was bound
    /// to, and does not spuriously close.
    ///
    /// Staying bound is the assertion that matters, and "our window still
    /// streams" does not make it. A carrier that followed the background
    /// window's pane change would move the *input* target with it, so the marker
    /// typed below would be echoed by the pane the phone is not looking at and
    /// would arrive in the stream exactly as if nothing were wrong — a phone
    /// typing into a pane it cannot see, with nothing to notice it. So the bind
    /// is checked directly, by generation, and the background pane's own screen
    /// is checked never to appear.
    #[tokio::test]
    async fn a_background_windows_pane_change_is_ignored() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        handle
            .input(b"echo BEFORE_W\r".to_vec())
            .expect("input reaches the writer");
        stream_until(&mut rx, &sem, "BEFORE_W", std::time::Duration::from_secs(5))
            .await
            .expect("our window streams");

        // A second window (`-d`, so our active window is unchanged), split, then
        // a pane-change inside it. Its `%window-pane-changed @<bg>` must be
        // filtered out by window; `-d`/split emit no `%session-window-changed`.
        // `-s` lists panes across the whole session, so the background window's
        // pane is actually found — a plain `list-panes` sees only the active
        // window, so a plain `list-panes` would not find it at all. Every step
        // is asserted so a setup failure fails the test rather than skipping it.
        assert!(
            fx.tmux(&["new-window", "-d", "-t", "=cc-term", "--", "/bin/sh"])
                .status
                .success(),
            "the background window is created"
        );
        let bg = {
            let out = fx.tmux(&[
                "list-panes",
                "-s",
                "-t",
                "=cc-term",
                "-F",
                "#{window_active} #{pane_id}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout);
            out.lines()
                .find(|l| l.starts_with('0'))
                .and_then(|l| l.split_whitespace().nth(1))
                .expect("a background window's pane exists")
                .to_string()
        };
        assert!(
            fx.tmux(&["split-window", "-d", "-t", &bg, "--", "/bin/sh"])
                .status
                .success(),
            "the background window splits"
        );
        let bg_inactive = {
            let out = fx.tmux(&[
                "list-panes",
                "-s",
                "-t",
                "=cc-term",
                "-F",
                "#{window_active} #{pane_active} #{pane_id}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout);
            // A pane in a background window (window_active==0) that is not its
            // window's active pane: selecting it fires a pane-change for a
            // window that is not ours.
            out.lines()
                .find(|l| l.starts_with("0 0"))
                .and_then(|l| l.split_whitespace().nth(2))
                .expect("an inactive pane in the background window")
                .to_string()
        };
        // A marker on the background pane's screen before the switch, printed
        // once and never again — so it can reach the phone only as the repaint a
        // spurious re-bind would order, never as live output.
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &bg_inactive,
                "-l",
                "printf 'BGWIN_%s\\n' MARKER\n"
            ])
            .status
            .success(),
            "the background window's pane is given its marker"
        );
        wait_pane_shows(&fx, &bg_inactive, "BGWIN_MARKER").await;
        assert!(
            fx.tmux(&["select-pane", "-t", &bg_inactive])
                .status
                .success(),
            "the background window's pane is selected"
        );

        // The carrier is untouched: still streaming our active window. This is
        // also the fence for the assertions below — one client's commands run in
        // the order they were written, so `AFTER_W` arriving proves the reader
        // has already been past the `%window-pane-changed` it must ignore.
        handle
            .input(b"echo AFTER_W\r".to_vec())
            .expect("input reaches the writer");
        let mut streamed =
            stream_until(&mut rx, &sem, "AFTER_W", std::time::Duration::from_secs(5))
                .await
                .expect("our window still streams after a background window's pane change");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(300),
        )
        .await;

        // Still on the pane the seed bound, and only that. A followed change
        // would have published a second generation and repainted the pane it
        // followed to — and would have taken the input above with it, which is
        // the part no amount of "the stream is alive" can catch.
        assert_eq!(
            published_generation(),
            1,
            "the carrier re-bound to a pane in a window the phone is not looking at"
        );
        assert!(
            !String::from_utf8_lossy(&streamed).contains("BGWIN_MARKER"),
            "the background window's screen reached the phone, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// A background pane that outputs *first*, before the active pane, must not
    /// be what the carrier follows: the seed query binds the active pane
    /// authoritatively, so the noisy background pane never streams.
    #[tokio::test]
    async fn a_background_pane_outputting_first_is_not_followed() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        // A background pane that immediately and repeatedly prints a marker,
        // keeping focus on the original pane. Target the pane id (a session
        // anchor is not a valid `split-window` target); a failure is a failure.
        let original = {
            let out = fx.tmux(&["list-panes", "-t", "=cc-term", "-F", "#{pane_id}"]);
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .expect("the session has a pane")
                .to_string()
        };
        assert!(
            fx.tmux(&[
                "split-window",
                "-d",
                "-t",
                &original,
                "--",
                "/bin/sh",
                "-c",
                "while :; do echo BG_NOISE; sleep 0.1; done",
            ])
            .status
            .success(),
            "the window splits"
        );
        // The background (inactive) pane's id, for the output barrier below.
        let background = {
            let out = fx.tmux(&[
                "list-panes",
                "-t",
                "=cc-term",
                "-F",
                "#{pane_active} #{pane_id}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout);
            let panes: Vec<&str> = out.lines().collect();
            assert!(panes.len() >= 2, "the split created a second pane");
            panes
                .iter()
                .find(|l| l.starts_with('0'))
                .and_then(|l| l.split_whitespace().nth(1))
                .expect("an inactive background pane")
                .to_string()
        };

        // Hold the seed until output has actually arrived and been withheld:
        // the background pane is producing from before attach, so the reader
        // has non-active-pane output in hand when it binds — the exact
        // condition a "first output seeds the pane" regression would mis-handle.
        let _seed = GateHold::until_output(&SEED_GATE, 3);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        assert!(
            output_seen() >= 3,
            "output arrived before the seed bound the pane"
        );

        // Barrier: the background pane has produced `BG_NOISE`, so a
        // forward-every-pane regression would have it in hand before the active
        // marker is even sent — the exclusion below is not vacuously true.
        wait_pane_shows(&fx, &background, "BG_NOISE").await;
        // Type into the active pane; its echo must stream, the background pane's
        // noise must not.
        handle
            .input(b"echo ACTIVE_TWO\r".to_vec())
            .expect("input reaches the writer");
        let streamed = stream_until(
            &mut rx,
            &sem,
            "ACTIVE_TWO",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the active pane streams");
        assert!(
            !String::from_utf8_lossy(&streamed).contains("BG_NOISE"),
            "the background pane must not be followed, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// A fresh attach paints what the pane already shows. Control mode streams
    /// only what a pane prints *next*, so the marker here — printed before
    /// anything attached, to a pane that then goes quiet — can reach the phone
    /// only as a snapshot. Exactly once, cleared and homed first, cursor placed
    /// last.
    #[tokio::test]
    async fn a_fresh_attach_paints_the_existing_screen() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        // Spelled so the marker's literal text is on the screen exactly once:
        // the echoed command line shows `FRESH_%s`, only its output shows
        // `FRESH_PAINT`. The barrier is what makes the assertion about the
        // snapshot rather than about a race with the shell.
        let pane = only_pane(&fx);
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                "printf 'FRESH_%s\\n' PAINT\n"
            ])
            .status
            .success(),
            "the marker is typed into the pane"
        );
        wait_pane_shows(&fx, &pane, "FRESH_PAINT").await;

        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "FRESH_PAINT",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the pane's existing screen reaches the phone");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(500),
        )
        .await;

        assert!(
            streamed.starts_with(b"\x1b[0m\x1b[2J\x1b[H"),
            "the first bytes of the attach are a cleared, homed screen, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        assert_eq!(
            occurrences(&streamed, "FRESH_PAINT"),
            1,
            "the existing screen is painted once, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        let painted = String::from_utf8_lossy(&streamed)
            .find("FRESH_PAINT")
            .expect("the marker is in the stream");
        let cursor = last_cursor_move(&streamed).expect("the paint ends by placing the cursor");
        assert!(
            cursor > painted,
            "the cursor is placed after the screen is drawn, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        // The paint's geometry is the pane's. A capture whose trailing blank
        // rows were mis-counted would place every row — and the cursor — one
        // line out, which nothing about the marker's presence would catch.
        assert_eq!(
            streamed.windows(2).filter(|pair| *pair == b"\r\n").count(),
            23,
            "an 80x24 pane paints 24 rows, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// Following the focus to another pane repaints it. Each pane prints its own
    /// marker once, before the attach; the second pane's marker can therefore
    /// reach the phone only as the snapshot that follows the switch — and the
    /// first pane's attach paint must not have carried it.
    #[tokio::test]
    async fn a_pane_switch_repaints_the_new_pane() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let original = only_pane(&fx);
        assert!(
            fx.tmux(&["split-window", "-d", "-t", &original, "--", "/bin/sh"])
                .status
                .success(),
            "the window splits"
        );
        let second = inactive_pane(&fx);
        for (pane, marker) in [(&original, "ONE"), (&second, "TWO")] {
            assert!(
                fx.tmux(&[
                    "send-keys",
                    "-t",
                    pane,
                    "-l",
                    &format!("printf 'PANE_%s\\n' {marker}\n"),
                ])
                .status
                .success(),
                "pane {pane} is given its marker"
            );
        }
        wait_pane_shows(&fx, &original, "PANE_ONE").await;
        wait_pane_shows(&fx, &second, "PANE_TWO").await;

        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        let mut attach = stream_until(&mut rx, &sem, "PANE_ONE", std::time::Duration::from_secs(5))
            .await
            .expect("the active pane's screen is painted at attach");
        drain_for(
            &mut rx,
            &sem,
            &mut attach,
            std::time::Duration::from_millis(300),
        )
        .await;
        assert!(
            !String::from_utf8_lossy(&attach).contains("PANE_TWO"),
            "only the active pane is painted, got {:?}",
            String::from_utf8_lossy(&attach)
        );

        // Move focus. The second pane has printed nothing since its marker, so
        // a stream that only ever carries new output would show the phone a
        // pane that never speaks again.
        assert!(
            fx.tmux(&["select-pane", "-t", &second]).status.success(),
            "focus moves to the second pane"
        );
        let repaint = stream_until(&mut rx, &sem, "PANE_TWO", std::time::Duration::from_secs(5))
            .await
            .expect("the newly active pane's existing screen is repainted");
        let text = String::from_utf8_lossy(&repaint).into_owned();
        let at = text
            .find("PANE_TWO")
            .expect("the second marker is streamed");
        assert!(
            text[..at].contains("\u{1b}[2J"),
            "the second pane's marker arrives inside a repaint, got {text:?}"
        );
        drop(handle);
    }

    /// Bytes held for a pane the carrier then leaves are dropped with it, never
    /// replayed into the pane it moved to.
    ///
    /// The held buffer belongs to one bind. A carrier that carried it across a
    /// publish would flush the pane the phone has *left* into the pane it is now
    /// looking at — text from somewhere else, with nothing on screen to explain
    /// it. The window is real because the capture is held: the first pane's
    /// output is held for a snapshot that has not been asked for yet when the
    /// focus moves.
    ///
    /// **The second pane's capture is made to fail, and that is what makes the
    /// claim observable at all.** On the ordinary path the `%begin` mark would
    /// hide a carried buffer — everything behind the mark is taken to be in the
    /// captured screen and dropped — so a carrier that carried bytes across the
    /// publish would look identical to one that did not. It is the failed
    /// capture that flushes the whole held buffer to the phone, and only there
    /// does whose bytes those are become visible. Measured: without this, the
    /// carry-it-forward mutation survives.
    #[tokio::test]
    async fn bytes_held_for_a_pane_the_carrier_leaves_are_dropped_with_it() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let first = only_pane(&fx);
        assert!(
            fx.tmux(&["split-window", "-d", "-t", &first, "--", "/bin/sh"])
                .status
                .success(),
            "the window splits, leaving the first pane active"
        );
        let second = inactive_pane(&fx);

        // Held from the attach on: the seed publishes the first pane and its
        // snapshot is never asked for, so everything that pane prints is held.
        let hold = GateHold::close(&CAPTURE_GATE);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        let before = withheld();
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &first,
                "-l",
                "printf 'HELDA_%s\\n' MARKER\n"
            ])
            .status
            .success(),
            "the first pane is given the marker that must be dropped"
        );
        // Both barriers: the pane really printed it, and the reader really held
        // it. Without the second this test could pass on output that never
        // arrived rather than on output that was correctly discarded.
        wait_pane_shows(&fx, &first, "HELDA_MARKER").await;
        wait_withheld(before + 1).await;

        // Armed now, so it is the *second* pane's pair that cannot resolve: the
        // writer read and rewrote the first pane's target before it parked at
        // the gate, so this can only reach the pair it takes up next.
        let _vanished = VanishedTarget::arm();

        // Move on. The publish for the second pane is what drops the first
        // pane's held bytes.
        assert!(
            fx.tmux(&["select-pane", "-t", &second]).status.success(),
            "focus moves to the second pane"
        );
        wait_published(2).await;

        // The second pane's own output, held behind its pending snapshot. It is
        // the fence: the failed capture flushes everything held for *this* bind,
        // so this marker arriving is the flush having happened, and the first
        // pane's marker not arriving with it is the claim.
        let before = withheld();
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &second,
                "-l",
                "printf 'PANEB_%s\\n' LIVE\n"
            ])
            .status
            .success(),
            "the second pane is given its marker"
        );
        wait_pane_shows(&fx, &second, "PANEB_LIVE").await;
        wait_withheld(before + 1).await;

        hold.release();
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "PANEB_LIVE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the buffer held for the pane the carrier followed to is flushed");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(500),
        )
        .await;
        assert!(
            !String::from_utf8_lossy(&streamed).contains("HELDA_MARKER"),
            "output held for the pane the carrier left was replayed into the \
             pane it moved to, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// Output produced between the attach and the snapshot is painted once, not
    /// twice. The capture is held while the pane prints, so the reader has
    /// genuinely seen those bytes and held them back — they are already in the
    /// grid the capture will read, so the paint is their delivery, and a carrier
    /// that also replayed them would show the phone the same line twice.
    #[tokio::test]
    async fn output_between_attach_and_snapshot_is_not_double_painted() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        // Held indefinitely: the carrier binds and publishes its target, but
        // the screen it will paint is not asked for until this test says so.
        let pane = only_pane(&fx);
        let hold = GateHold::close(&CAPTURE_GATE);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        let before = withheld();
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                "printf 'ONCE_%s\\n' MARKER\n"
            ])
            .status
            .success(),
            "the marker is typed into the pane"
        );
        // Both barriers, because each proves half of it: the grid barrier that
        // the capture to come will carry the marker, the withheld barrier that
        // the reader had those same bytes in hand and held them.
        wait_pane_shows(&fx, &pane, "ONCE_MARKER").await;
        wait_withheld(before + 1).await;

        hold.release();
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "ONCE_MARKER",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the held snapshot arrives once released");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(700),
        )
        .await;
        assert_eq!(
            occurrences(&streamed, "ONCE_MARKER"),
            1,
            "the withheld output is painted exactly once, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        assert!(
            streamed.starts_with(b"\x1b[0m\x1b[2J\x1b[H"),
            "nothing was streamed ahead of the paint, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// A snapshot reply for a pane the carrier has already left is dropped, not
    /// painted.
    ///
    /// The race this closes is a real one: the writer reads a target, and by the
    /// time tmux answers the screen it asked for, the reader may have followed
    /// the focus twice more. Reproduced exactly — the capture for the second
    /// pane is held until the third has been bound, so its reply is answered
    /// against a carrier that has moved on, and the queue holds no newer capture
    /// to give the answer away. Only the generation on the tag can tell. The
    /// second pane's marker was printed before the attach and never again, so
    /// its appearance in the stream could only be that stale screen being
    /// painted over the pane the phone is actually on.
    #[tokio::test]
    async fn a_snapshot_for_a_pane_already_left_is_never_painted() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let first = only_pane(&fx);
        for _ in 0..2 {
            assert!(
                fx.tmux(&["split-window", "-d", "-t", &first, "--", "/bin/sh"])
                    .status
                    .success(),
                "the window splits"
            );
        }
        let idle: Vec<String> = {
            let out = fx.tmux(&[
                "list-panes",
                "-t",
                "=cc-term",
                "-F",
                "#{pane_active} #{pane_id}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout);
            out.lines()
                .filter(|l| l.starts_with('0'))
                .filter_map(|l| l.split_whitespace().nth(1))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(idle.len(), 2, "two panes to follow, and one to start on");
        let (second, third) = (&idle[0], &idle[1]);
        for (pane, marker) in [(&first, "ONE"), (second, "TWO"), (third, "THREE")] {
            assert!(
                fx.tmux(&[
                    "send-keys",
                    "-t",
                    pane,
                    "-l",
                    &format!("printf 'PANE_%s\\n' {marker}\n"),
                ])
                .status
                .success(),
                "pane {pane} is given its marker"
            );
            wait_pane_shows(&fx, pane, &format!("PANE_{marker}")).await;
        }

        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        stream_until(&mut rx, &sem, "PANE_ONE", std::time::Duration::from_secs(5))
            .await
            .expect("the pane the attach binds is painted");

        // Every snapshot from here is held at the gate.
        let capture = GateHold::close(&CAPTURE_GATE);
        assert!(
            fx.tmux(&["select-pane", "-t", second]).status.success(),
            "focus moves to the second pane"
        );
        // The writer has read the second pane's target and stopped on it.
        wait_arrived(&capture, 1).await;
        assert!(
            fx.tmux(&["select-pane", "-t", third]).status.success(),
            "focus moves on to the third pane"
        );
        // The third pane is bound before the second's screen is even asked for.
        wait_published(3).await;

        // Let the second pane's pair through, and wait for both its replies to
        // be answered: a capture and a cursor for each of the two panes bound so
        // far. Nothing of the second pane may reach the phone.
        capture.allow(1);
        wait_snapshot_replies(4).await;
        let mut stale = Vec::new();
        drain_for(
            &mut rx,
            &sem,
            &mut stale,
            std::time::Duration::from_millis(500),
        )
        .await;
        assert!(
            !String::from_utf8_lossy(&stale).contains("PANE_TWO"),
            "a screen captured for a pane the carrier has left was painted, got {:?}",
            String::from_utf8_lossy(&stale)
        );

        // And the pane the carrier is actually on still gets its screen.
        capture.release();
        let painted = stream_until(
            &mut rx,
            &sem,
            "PANE_THREE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the pane the carrier followed to is painted");
        assert!(
            !String::from_utf8_lossy(&painted).contains("PANE_TWO"),
            "the stale screen arrived late, got {:?}",
            String::from_utf8_lossy(&painted)
        );
        drop(handle);
    }

    /// Output that lands between the capture's reply and the cursor query's is
    /// delivered exactly once, after the paint.
    ///
    /// The two are separate reply blocks and tmux is free to send a notification
    /// between them, so this window is real. The bytes cannot be in the screen —
    /// the capture was already answered when they were printed — so a carrier
    /// that dropped them would lose a line outright, and one that sent them as
    /// they arrived would put them on the phone *before* the repaint that wipes
    /// the screen they belong on. The cursor query is held to hold the window
    /// open; the marker is proven to be in the pane and held by the reader before
    /// it is released.
    #[tokio::test]
    async fn output_between_the_capture_and_the_cursor_is_replayed_after_the_paint() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let pane = only_pane(&fx);
        let cursor = GateHold::close(&CURSOR_GATE);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        // The capture has been answered, so tmux has run it: everything the
        // pane prints from here is outside the screen the reader is holding.
        wait_snapshot_replies(1).await;

        let before = withheld();
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                "printf 'GAP_%s\\n' MARKER\n"
            ])
            .status
            .success(),
            "the marker is typed into the pane"
        );
        wait_pane_shows(&fx, &pane, "GAP_MARKER").await;
        wait_withheld(before + 1).await;

        cursor.release();
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "GAP_MARKER",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("output held across the pair is delivered, not dropped");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(500),
        )
        .await;
        assert!(
            streamed.starts_with(b"\x1b[0m\x1b[2J\x1b[H"),
            "nothing was streamed ahead of the paint, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        assert_eq!(
            occurrences(&streamed, "GAP_MARKER"),
            1,
            "the held output is delivered exactly once, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        let at = String::from_utf8_lossy(&streamed)
            .find("GAP_MARKER")
            .expect("the marker is in the stream");
        let painted = last_cursor_move(&streamed).expect("the paint ends by placing the cursor");
        assert!(
            painted < at,
            "the held output followed the whole paint, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// A capture that fails costs the snapshot and never the bytes. The pane's
    /// output is held from the publish, the capture is answered `%error`, and
    /// every held byte is delivered in order with no screen painted — then the
    /// stream carries on live. The alternative, which this rules out, is the
    /// phone silently losing everything the pane printed while the carrier was
    /// waiting for a screen that never came.
    #[tokio::test]
    async fn a_failed_capture_replays_everything_it_held() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let pane = only_pane(&fx);
        // The next snapshot pair asks about a pane that cannot exist, so tmux
        // answers `can't find pane` — what a pane that vanished between the
        // publish and the capture answers, without having to win a race against
        // tmux's own notification that it is gone.
        let _vanished = VanishedTarget::arm();
        let hold = GateHold::close(&CAPTURE_GATE);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        let before = withheld();
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                "printf 'ERR_%s\\n' REPLAY\n"
            ])
            .status
            .success(),
            "the marker is typed into the pane"
        );
        wait_pane_shows(&fx, &pane, "ERR_REPLAY").await;
        wait_withheld(before + 1).await;

        hold.release();
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "ERR_REPLAY",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("every byte held for a snapshot that failed is delivered");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(500),
        )
        .await;
        assert!(
            !streamed.windows(4).any(|w| w == b"\x1b[2J"),
            "a capture that errored painted a screen anyway, got {:?}",
            String::from_utf8_lossy(&streamed)
        );

        // And the carrier keeps streaming: the failed snapshot cost a paint,
        // not the terminal.
        handle
            .input(b"echo STILL_LIVE\r".to_vec())
            .expect("input reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "STILL_LIVE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the stream survives a snapshot that could not be taken");
        drop(handle);
    }

    /// A cursor query that comes back without a position costs the cursor and
    /// nothing else. The capture ahead of it answers with a real screen, so
    /// there *is* a screen to paint; only the second half of the pair is
    /// pointed at a bare pane id that cannot be resolved, which is the one way
    /// to make tmux answer a cursor query with no position at all (see
    /// [`TEST_CURSOR_TARGET`] — it is a synthetic condition, and the branch it
    /// reaches is what matters, not how it was reached). The screen must still
    /// be painted, with no cursor move behind it, and the bytes held across the
    /// pair must still be replayed exactly once — the alternatives this rules
    /// out are the phone losing a screen it could have had, being shown one
    /// twice, or losing the terminal, all over a cursor position.
    #[tokio::test]
    async fn a_cursor_query_without_a_position_still_paints_the_screen() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let pane = only_pane(&fx);
        // On the screen before anything attaches, so it can reach the phone
        // only as a paint — the proof that rows were painted and not merely a
        // clear. Spelled `%s` so the literal text is on the screen once: the
        // echoed command line does not carry it, only the output does.
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                "printf 'NOCUR_%s\\n' SCREEN\n"
            ])
            .status
            .success(),
            "the pre-attach marker is typed into the pane"
        );
        wait_pane_shows(&fx, &pane, "NOCUR_SCREEN").await;

        // The cursor query alone asks about a bare pane id that cannot exist,
        // so it answers an empty position while the capture ahead of it answers
        // a real screen. Holding the cursor gate opens the window the second
        // marker is printed into: after the capture was answered, before the
        // query that loses the cursor is even written.
        let _lost = CursorTarget::unresolvable();
        let cursor = GateHold::close(&CURSOR_GATE);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        wait_snapshot_replies(1).await;

        let before = withheld();
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                "printf 'NOCUR_%s\\n' HELD\n"
            ])
            .status
            .success(),
            "the held marker is typed into the pane"
        );
        wait_pane_shows(&fx, &pane, "NOCUR_HELD").await;
        wait_withheld(before + 1).await;

        cursor.release();
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "NOCUR_HELD",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the bytes held across the pair are delivered, not dropped");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(500),
        )
        .await;

        // The screen is painted: cleared and homed, ahead of everything else.
        assert!(
            streamed.starts_with(b"\x1b[0m\x1b[2J\x1b[H"),
            "the screen was not painted, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        // And it carries the rows: the pre-attach marker is in the stream, and
        // could have got there no other way.
        assert_eq!(
            occurrences(&streamed, "NOCUR_SCREEN"),
            1,
            "the captured rows were not painted exactly once, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        // No cursor move anywhere: the paint ends at its last row, and the
        // replay behind it is this pane's own bytes, which carry no
        // positioning. That absence is the whole finding — a cursor placed here
        // would have been placed from a reply that failed.
        assert_eq!(
            last_cursor_move(&streamed),
            None,
            "a failed cursor query still moved the cursor, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        // The bytes held across the pair are replayed once: neither dropped
        // with the cursor nor doubled by being painted and replayed both.
        assert_eq!(
            occurrences(&streamed, "NOCUR_HELD"),
            1,
            "the held output is delivered exactly once, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        let painted = String::from_utf8_lossy(&streamed)
            .find("NOCUR_SCREEN")
            .expect("the painted rows are in the stream");
        let replayed = String::from_utf8_lossy(&streamed)
            .find("NOCUR_HELD")
            .expect("the held bytes are in the stream");
        assert!(
            painted < replayed,
            "the held bytes did not follow the paint, got {:?}",
            String::from_utf8_lossy(&streamed)
        );

        // And the carrier keeps streaming: a cursor that could not be read cost
        // the cursor, not the terminal.
        handle
            .input(b"echo STILL_LIVE\r".to_vec())
            .expect("input reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "STILL_LIVE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the stream survives a cursor query that named no position");
        drop(handle);
    }

    /// A cursor query answered *for another pane* places no cursor.
    ///
    /// This is the reply a vanished pane really produces. Measured on 3.7b,
    /// `display-message -p -t <composite whose pane has gone>` does not error
    /// the way `capture-pane` does — it answers for the window's active pane,
    /// rc 0, a perfectly well-formed position belonging to a pane the phone is
    /// not looking at. Nothing but the pane id in the reply says so: the block
    /// did not error, the body is one parseable line, and the reply tag's
    /// generation still matches because the carrier has not moved anywhere.
    ///
    /// The condition is put on the wire by aiming the cursor query at a second
    /// live pane, which produces those exact bytes every run; killing the bound
    /// pane instead would race tmux's ordering of its own death notification
    /// against the reply. The screen must still be painted — the mismatch costs
    /// the cursor, not the snapshot — and no cursor move may follow it.
    #[tokio::test]
    async fn a_cursor_reply_from_another_pane_places_no_cursor() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let bound = only_pane(&fx);
        assert!(
            fx.tmux(&["split-window", "-d", "-t", &bound, "--", "/bin/sh"])
                .status
                .success(),
            "the window splits, leaving the bound pane active"
        );
        let other = inactive_pane(&fx);
        // On the screen before anything attaches, so it can reach the phone only
        // as a paint. Spelled `%s` so the literal text is on the screen once:
        // the echoed command line does not carry it, only the output does.
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &bound,
                "-l",
                "printf 'OTHER_%s\\n' SCREEN\n"
            ])
            .status
            .success(),
            "the pre-attach marker is typed into the bound pane"
        );
        wait_pane_shows(&fx, &bound, "OTHER_SCREEN").await;

        // The capture asks about the bound pane and gets its screen; the cursor
        // query alone asks about the other pane, scoped exactly as the carrier
        // scopes its own targets, and gets that pane's position back.
        let _aimed = CursorTarget::aimed_at(format!("{}.{other}", window_scope(&fx)));
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "OTHER_SCREEN",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the bound pane's screen is painted");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(500),
        )
        .await;

        assert!(
            streamed.starts_with(b"\x1b[0m\x1b[2J\x1b[H"),
            "the screen was not painted, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        assert_eq!(
            occurrences(&streamed, "OTHER_SCREEN"),
            1,
            "the captured rows were not painted exactly once, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        // The whole finding. The other pane's position is a real one, so a
        // carrier that trusted the reply would have placed it here — on a
        // screen belonging to a different pane, at a row and column that mean
        // nothing on it.
        assert_eq!(
            last_cursor_move(&streamed),
            None,
            "another pane's cursor was placed on this pane's screen, got {:?}",
            String::from_utf8_lossy(&streamed)
        );

        // And the carrier keeps streaming: the mismatch cost the cursor, not
        // the terminal.
        handle
            .input(b"echo STILL_LIVE\r".to_vec())
            .expect("input reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "STILL_LIVE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the stream survives a cursor query answered by another pane");
        drop(handle);
    }

    /// Every way a cursor query can come back unusable costs the cursor and
    /// nothing more, so none of them can be turned into a lost screen by
    /// accident.
    ///
    /// The one that is not defensive is the last: a well-formed position for a
    /// pane that is not the one captured. That is what `display-message` really
    /// answers once the captured pane has vanished — measured on 3.7b it
    /// reports the window's *active* pane, rc 0, where `capture-pane` would
    /// have errored — so it is the only signal separating this pane's cursor
    /// from another pane's, and painting it would put the wrong cursor on a
    /// screen that is otherwise right.
    #[test]
    fn a_cursor_reply_that_is_not_this_panes_position_yields_none() {
        let good = vec![b"%3 4 2".to_vec()];
        assert_eq!(
            super::cursor_from_reply(false, false, &good, "%3"),
            Some((4, 2))
        );
        assert_eq!(
            super::cursor_from_reply(true, false, &good, "%3"),
            None,
            "an errored block is not a position, whatever its body says"
        );
        assert_eq!(
            super::cursor_from_reply(false, true, &good, "%3"),
            None,
            "a body that outgrew the bound may be missing the line that mattered"
        );
        // What tmux really answers for a pane it cannot resolve: one line, all
        // three fields expanded to nothing, and no error.
        assert_eq!(
            super::cursor_from_reply(false, false, &[b"  ".to_vec()], "%3"),
            None
        );
        assert_eq!(super::cursor_from_reply(false, false, &[], "%3"), None);
        assert_eq!(
            super::cursor_from_reply(
                false,
                false,
                &[b"%3 4 2".to_vec(), b"%3 4 2".to_vec()],
                "%3"
            ),
            None,
            "a reply that is not exactly one line is not this query's answer"
        );
        assert_eq!(
            super::cursor_from_reply(false, false, &good, "%4"),
            None,
            "a position another pane answered with is not this pane's cursor"
        );
        // Prefix equality is not identity: `%3` must not satisfy a query about
        // `%30`, nor the other way round.
        assert_eq!(
            super::cursor_from_reply(false, false, &[b"%30 4 2".to_vec()], "%3"),
            None
        );
        assert_eq!(super::cursor_from_reply(false, false, &good, "%30"), None);
    }

    /// A pane that outruns the held buffer loses the snapshot, never its bytes.
    ///
    /// The capture is held at the gate, so the screen those bytes are being
    /// held for does not arrive while the pane floods. Past the bound the
    /// carrier must give the snapshot up rather than the output: the marker
    /// printed after a megabyte of flood can only reach the phone if the buffer
    /// was flushed and the stream went live behind it. Nothing is painted on
    /// this path, so the bound cannot cost the phone a screen it has already
    /// been shown.
    #[tokio::test]
    async fn a_pane_that_outruns_the_held_buffer_keeps_its_bytes() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let pane = only_pane(&fx);
        let hold = GateHold::close(&CAPTURE_GATE);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        // Comfortably past the bound, produced by `yes` rather than a shell
        // loop so the flood is over in seconds.
        let row = "X".repeat(62);
        let lines = HELD_OUTPUT_BYTES / 63 + 4096;
        assert!(
            fx.tmux(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                &format!("yes {row} | head -n {lines}; printf 'FLOOD_%s\\n' DONE\n"),
            ])
            .status
            .success(),
            "the pane is set flooding"
        );
        let streamed = stream_until(
            &mut rx,
            &sem,
            "FLOOD_DONE",
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("the buffer is flushed and the stream goes live past the bound");
        assert!(
            !streamed.windows(4).any(|w| w == b"\x1b[2J"),
            "a screen was painted while the held buffer's bytes were owed first"
        );
        assert!(
            streamed.len() as u64 > HELD_OUTPUT_BYTES as u64,
            "the flood reached the phone, got {} bytes",
            streamed.len()
        );
        // Exactly one capture attempt reached the gate. `Gate::pass` counts the
        // arrival before it blocks, so this says nothing about whether the
        // command later passed through to tmux — `GateHold::close` holds only
        // until `GATE_DEADLINE`, well inside the thirty seconds the flood above
        // is allowed, so a capture released after that is an ordinary end to
        // this test and not a finding. The arrival count is the signal this
        // rests on precisely because it is indifferent to when the gate opens;
        // `snapshot_replies()` would be the opposite — the gate letting the
        // capture through on its own turns it non-zero and fails a test about
        // the held buffer for a reason that has nothing to do with the buffer.
        // That no snapshot was *painted* is already proven above, by there
        // being no clear in the stream.
        assert_eq!(
            hold.arrived(),
            1,
            "one capture attempt for the bind reached the gate"
        );
        drop(handle);
    }

    /// A pane displaying the text of a reply terminator cannot end the block its
    /// own screen is being sent in. tmux does not escape command output, so
    /// `%end 1 2 3` on the screen arrives at the start of a line inside the
    /// capture (measured on 3.7b) — a reader keying on the prefix alone would
    /// stop there, truncate the paint, and mis-read the rest of the screen as
    /// notifications. Both markers must be painted, and the stream must stay
    /// live afterwards.
    #[tokio::test]
    async fn a_pane_showing_a_reply_terminator_does_not_truncate_its_snapshot() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let pane = only_pane(&fx);
        for command in [
            "printf '%%end 1 2 3\\n'\n",
            "printf 'AFTER_%s\\n' FORGERY\n",
        ] {
            assert!(
                fx.tmux(&["send-keys", "-t", &pane, "-l", command])
                    .status
                    .success(),
                "the pane is given {command:?}"
            );
        }
        wait_pane_shows(&fx, &pane, "AFTER_FORGERY").await;

        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "AFTER_FORGERY",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the whole screen is painted, past the line that looks like a terminator");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(300),
        )
        .await;
        assert!(
            String::from_utf8_lossy(&streamed).contains("%end 1 2 3"),
            "the forged terminator is painted as the screen content it is, got {:?}",
            String::from_utf8_lossy(&streamed)
        );

        // And correlation survived it: the carrier still streams.
        handle
            .input(b"echo STILL_LIVE\r".to_vec())
            .expect("input reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "STILL_LIVE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the stream survives a screen that forges a reply terminator");
        drop(handle);
    }

    /// A phone that returns credit but never enough loses its terminal too. The
    /// deadline is *absolute for the chunk*, so a peer trickling one byte at a
    /// time — making real progress, never completing a paint — must still be
    /// closed as a slow consumer at that deadline and its client reaped. A
    /// carrier that reset the deadline on every permit would keep this
    /// attachment, and the reader's blindness to control notifications, open
    /// forever; the trickle outlasts the assertion window so that regression
    /// cannot pass here. The deadline is shortened so the test does not wait the
    /// production 30s.
    #[tokio::test]
    async fn an_output_stall_closes_as_slow_consumer_and_reaps() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let stall = std::time::Duration::from_millis(2000);
        // One byte of credit every quarter of that: eight chances to make
        // progress inside one deadline, none of them enough to finish a chunk.
        let trickle_every = stall / 8;
        let _deadline = StallDeadline::shortened_to(stall);
        let leases = TerminalLeases::new();
        // One byte of credit to start: the snapshot painted at attach out-sizes
        // it immediately, so the reader is mid-chunk from the first moment.
        let (handle, mut rx, sem) = open_against(&fx, &leases, 1).await;
        // Trickled for far longer than this test waits, so a carrier that
        // restarted the deadline on every permit would never close here.
        let trickle = {
            let sem = Arc::clone(&sem);
            tokio::spawn(async move {
                for _ in 0..80 {
                    tokio::time::sleep(trickle_every).await;
                    sem.add_permits(1);
                }
            })
        };

        // The stream closes on its own once the absolute deadline passes.
        let mut delivered = 0usize;
        let closed = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while let Some(chunk) = rx.recv().await {
                delivered += chunk.bytes().len();
            }
        })
        .await
        .is_ok();
        trickle.abort();
        assert!(closed, "the trickling stream closes at the deadline");
        assert!(
            delivered > 1,
            "the trickle kept the carrier making progress, got {delivered} bytes"
        );
        // Measured against the deadline that actually closed this carrier, not
        // against the attach: the chunk that stalls is the attach's paint,
        // whose deadline can be armed while `open()` is still re-verifying, so
        // timing from the attach would charge a slow probe to the carrier. The
        // reader arms no further deadline after a stall, so the last one armed
        // is the one that expired. What this rules out is a deadline restarted
        // by each wait for credit: that closes a quarter-second after the last
        // permit, not the whole deadline after the first.
        let armed = deadline_armed().expect("the reader armed a stall deadline");
        let took = armed.elapsed();
        assert!(
            took >= stall,
            "the close waited the chunk's absolute deadline, took {took:?}"
        );
        assert_eq!(
            handle.close_reason(),
            protocol::ws::terminal_close::SLOW_CONSUMER,
            "a credit trickle that never completes a chunk closes as slow_consumer"
        );
        // And it survives the teardown that follows, which is the half the read
        // above cannot see: `close()` is what every other ending path also runs,
        // and the reason must still be the stall afterwards.
        let pid = handle.client_pid();
        handle.close();
        assert_eq!(
            handle.close_reason(),
            protocol::ws::terminal_close::SLOW_CONSUMER,
            "the teardown overwrote the reason that explains the close"
        );
        drop(handle);
        let mut reaped = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if process_gone(pid) {
                reaped = true;
                break;
            }
        }
        assert!(reaped, "the stalled client is reaped");
        assert!(fx.alive(), "the session outlives the stall");
    }

    /// A connection that stops draining is the daemon's own backpressure and
    /// must never be charged to the phone — for a pause of any length at all.
    ///
    /// The output queue is drained by the connection's select loop, which polls
    /// it a pass at a time while it is busy elsewhere — replaying a backlog is
    /// one event send per event, and a long one takes as long as it takes. The
    /// carrier must survive that and go on streaming rather than close as a slow
    /// consumer for a stall the phone had no part in. The deadline is shortened
    /// so the pause need not be measured in minutes.
    ///
    /// **The number of deadlines the pause covers is the point.** The defect
    /// being fixed was forgiveness that was finite in the wrong place — a budget
    /// that eventually ran out however honest the phone was — so a pause shorter
    /// than that budget proves nothing: it would pass against the code this
    /// replaces. The pause here is the whole of
    /// [`PEER_STALL_BUDGET_DEADLINES`] and two more on top, which the daemon
    /// clause has to forgive without spending any of it.
    ///
    /// The grant is deliberately *small*, and that is the rest of the design. A
    /// phone-sized grant leaves the reader parked in the hand-over, where a full
    /// queue is also the daemon's — so the carrier survives whether or not the
    /// credit wait's own attribution is right, and the test passes with the
    /// daemon clause deleted outright (measured). One grant smaller than the
    /// attach's own paint puts the reader where the claim lives instead: waiting
    /// on *credit*, with output it handed over still queued and slots to spare.
    /// Both facts are asserted below, so this cannot quietly stop covering them.
    #[tokio::test]
    async fn a_connection_that_stops_draining_is_not_a_slow_consumer() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let stall = std::time::Duration::from_millis(500);
        let _deadline = StallDeadline::shortened_to(stall);
        let leases = TerminalLeases::new();
        // Eight bytes: less than the screen the attach paints, so the grant is
        // spent on a prefix of the very first chunk and the reader is waiting on
        // credit before a second chunk can even be queued. A larger grant would
        // let several chunks pile up and park the reader in the hand-over
        // instead, which is the wrong wait for this claim.
        let (handle, mut rx, sem) = open_against(&fx, &leases, 8).await;

        // Marker for the liveness half, behind the paint the reader is already
        // stuck inside. Spelled so its literal text is only ever the pane's
        // output — the echoed command line shows `DRAIN_%s`.
        handle
            .input(b"printf 'DRAIN_%s\\n' RESUMED\r".to_vec())
            .expect("input reaches the writer");

        // The state the claim is about, asserted rather than assumed: the grant
        // is spent, and what it was spent on is sitting in a queue nobody is
        // draining. The permits are taken before the hand-over, so a queued
        // chunk means an exhausted grant already.
        wait_for("queued output chunks", 1, || rx.queued()).await;
        assert_eq!(
            sem.available_permits(),
            0,
            "the grant must be spent, or the reader is not waiting on credit"
        );
        assert!(
            rx.queued() < 4,
            "the queue is not full, so the reader is waiting on credit and not \
             parked in the hand-over — got {} queued",
            rx.queued()
        );

        // Past the whole of the attachment's forgiveness, with the connection
        // not draining a byte.
        tokio::time::sleep(stall * (PEER_STALL_BUDGET_DEADLINES + 2)).await;
        assert_ne!(
            handle.close_reason(),
            protocol::ws::terminal_close::SLOW_CONSUMER,
            "a connection that stopped draining was blamed on the phone"
        );
        // Open, not merely un-blamed. Input reaches the writer for exactly as
        // long as there is a carrier to reach: a torn-down one has dropped the
        // receiver, and this is the cheapest fact that distinguishes the two
        // before anything is streamed.
        handle
            .input(b"\r".to_vec())
            .expect("the carrier was torn down during the pause");

        // And live rather than merely open: the marker typed before the pause
        // still arrives, which it can only do by the reader having gone back to
        // the chunk it was stuck inside.
        stream_until(
            &mut rx,
            &sem,
            "DRAIN_RESUMED",
            std::time::Duration::from_secs(10),
        )
        .await
        .expect("the stream carries on once the connection drains again");
        drop(handle);
    }

    /// The stall deadline bounds waiting on the phone, not wall-clock.
    ///
    /// A chunk larger than the credit in hand is sent in pieces, and the pieces
    /// share one absolute deadline — which is what stops a peer trickling a byte
    /// before each one. But if the enqueue between two pieces parks because the
    /// connection is busy, and that time came out of the same deadline, the next
    /// piece would ask for credit against a deadline that had already run out
    /// and call a phone that answered in a third of it a slow consumer. So the
    /// enqueue's time is given back. Driven at [`forward`] directly, because
    /// what is under test is the arithmetic of one chunk and not a pane.
    ///
    /// The deadline is shortened, but not as far as the other stall tests: the
    /// credit has to land after the enqueue resumes (or a spent deadline would
    /// be satisfied before it was ever consulted, and the test would pass on the
    /// defect) and before the given-back deadline (or a correct carrier would
    /// stall). A third of the deadline either side of it is room no scheduler
    /// takes.
    #[tokio::test]
    async fn a_full_output_queue_does_not_spend_the_phones_stall_deadline() {
        // The stall deadline is a process-wide value, so this takes the same
        // serial guard the tmux tests take: without it a shortened deadline
        // leaks into whatever else is running, and this test's own restore
        // clears theirs — a flake that names the wrong test.
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(900);
        let _deadline = StallDeadline::shortened_to(stall);
        // One byte of credit against a two-byte chunk, and a one-slot queue
        // that is already full: the forward below spends its byte on the first
        // half and then parks in the enqueue.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(1);
        let (_close, mut closed) = watch::channel(false);
        let mut to = delivery_over(&credit, tx);
        assert!(
            to.hand_over(b"primed".to_vec(), &mut closed).await.is_ok(),
            "the queue takes its one slot"
        );

        // The connection is busy for three deadlines, then drains and the phone
        // answers with the byte the second half needs — promptly, but not so
        // instantly that an expired deadline could be satisfied before it was
        // ever consulted.
        // The receiver is handed back rather than dropped: dropping it would
        // end the forward as a closed carrier and mask everything under test.
        let resume = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                tokio::time::sleep(stall * 3).await;
                drop(rx.recv().await.expect("the slot the queue was primed with"));
                drop(rx.recv().await.expect("the half the forward enqueued"));
                tokio::time::sleep(stall / 3).await;
                credit.add_permits(1);
                rx
            })
        };

        assert!(
            matches!(forward(b"ab", &mut to, &mut closed).await, Ok(())),
            "a chunk the connection's own queue held up was charged to the phone"
        );
        drop(resume.await.expect("the drain finishes"));
    }

    /// `slow_consumer` means the phone stopped returning credit, and only that.
    ///
    /// A chunk bigger than the grant is sent in pieces, and one piece can carry
    /// the whole grant into a single queue slot — so the reader waits on credit
    /// with the queue holding bytes and three slots still free, which is the
    /// shape a snapshot or a flushed hold takes against one window. Those bytes
    /// have not reached the phone, because the connection has not even taken
    /// them yet, so the phone cannot credit them and a deadline expiring beside
    /// them is the daemon's own backpressure wearing the phone's name.
    ///
    /// The same wait with nothing owed *is* the phone's, and must still close.
    /// Both halves below are the same inputs and the same deadline; the only
    /// difference between them is whether the connection had delivered what it
    /// was handed, which is exactly the fact the close code is supposed to turn
    /// on. Output the connection has *taken* and not yet written is the same
    /// question one step further along the wire, and is
    /// `a_credit_wait_beside_a_pending_write_is_not_the_phones_fault`.
    #[tokio::test]
    async fn a_credit_wait_beside_unsent_output_is_not_the_phones_fault() {
        // Shortened process-wide, so this takes the serial guard for the same
        // reason every other stall test does.
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(400);
        let _deadline = StallDeadline::shortened_to(stall);
        let (_close, mut closed) = watch::channel(false);

        // One byte of credit against two, and room in the queue: the first
        // piece is enqueued without ever parking, and the second waits on
        // credit the phone cannot send because the first has not reached it.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let resume = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                // Three deadlines and a half with the connection sitting on what
                // it was given, then it takes it, writes it, and the phone
                // credits what it has now actually seen. The half is what keeps
                // the drain clear of the deadlines it must be seen across:
                // landing on one would let an expiry read the queue either side
                // of the drain.
                tokio::time::sleep(stall * 7 / 2).await;
                let piece = rx.recv().await.expect("the piece the forward enqueued");
                let written = piece.bytes().len();
                drop(piece);
                credit.add_permits(written);
                rx
            })
        };
        let mut to = delivery_over(&credit, tx);
        assert!(
            matches!(forward(b"ab", &mut to, &mut closed).await, Ok(())),
            "a credit wait beside output the connection had not sent was charged to the phone"
        );
        drop(resume.await.expect("the drain finishes"));

        // And with nothing owed it is the phone's: the piece is taken and
        // written well before the deadline, and no credit follows it.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let held = tokio::spawn(async move {
            drop(rx.recv().await.expect("the piece the forward enqueued"));
            rx
        });
        // Bounded, because what it guards against — a re-arm that stops
        // discriminating — does not make this assertion fail, it makes it wait
        // for a close that never comes. The bound is what turns that into this
        // test's own named failure rather than a suite that wedges here.
        let mut to = delivery_over(&credit, tx);
        assert!(
            matches!(
                tokio::time::timeout(stall * 5, forward(b"ab", &mut to, &mut closed)).await,
                Ok(Err(ForwardEnd::CreditStarved))
            ),
            "a phone that stops crediting output it has been given must still lose its terminal"
        );
        drop(held.await.expect("the drain finishes"));
    }

    /// The gap between a chunk leaving the queue and reaching the phone is the
    /// daemon's too.
    ///
    /// This is the shape queue occupancy cannot see, and the one production
    /// takes on every single chunk: the connection's select loop *removes* a
    /// chunk from the queue and only then awaits the socket write (`ws_server`'s
    /// output arm). Across that window the queue reads empty while the phone has
    /// been given nothing, and bytes still on their way are bytes it cannot
    /// credit — so a carrier that asked how full the queue was would name a
    /// healthy phone a slow consumer for a write the daemon had not finished.
    /// Reproduced exactly: the piece is taken off the queue before the first
    /// deadline, so every deadline here expires against an empty queue, and its
    /// write finishes three and a half deadlines later with the credit behind
    /// it.
    ///
    /// The second half is the same inputs, the same deadline and the same empty
    /// queue, differing only in that the write completes at once: a phone that
    /// has taken delivery and then stops crediting is a stalled consumer and
    /// must still lose its terminal. Together they are what makes the deadline
    /// mean one thing — the phone has been handed bytes and has not credited
    /// them — rather than two.
    ///
    /// Both forwards are bounded, generously, and the bound is part of the
    /// claim: a re-arm that stops discriminating leaves the second half waiting
    /// for a close that never comes, and an unbounded assertion reports that as
    /// a wedged suite rather than as the failure it is.
    ///
    /// The forgiveness the first half needs comes out of the attachment's budget,
    /// because a pending write is the peer's to end — three deadlines against a
    /// budget of [`PEER_STALL_BUDGET_DEADLINES`], so this is well inside it and
    /// says nothing about where the budget runs out.
    /// `a_peer_that_holds_each_write_is_still_a_slow_consumer` is that claim, and
    /// `the_peer_budget_is_spent_across_the_whole_attachment_and_never_renewed`
    /// is the same claim across several chunks.
    #[tokio::test]
    async fn a_credit_wait_beside_a_pending_write_is_not_the_phones_fault() {
        // Shortened process-wide, so this takes the serial guard for the same
        // reason every other stall test does.
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(400);
        let _deadline = StallDeadline::shortened_to(stall);
        let (_close, mut closed) = watch::channel(false);

        // One byte of credit against two, and room in the queue: the first piece
        // is handed over without ever parking, and the second waits on credit
        // the phone cannot send because the first has not reached it.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let started = tokio::time::Instant::now();
        let writing = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                // Taken the moment it is queued, held across three and a half
                // deadlines, then written — which is what dropping it says. The
                // half keeps the write clear of the deadlines it must be seen
                // across: landing on one would let an expiry read the count
                // either side of it.
                let piece = rx.recv().await.expect("the piece the forward handed over");
                // Fail closed on the two facts the window rests on, because a
                // test that assumed them would pass on the defect: the piece is
                // off the queue, and it was taken well inside the first
                // deadline. Nothing is handed over again until the credit below,
                // so from here every expiry the forward sees reads an empty
                // queue — which is what makes this the *pending write* and not
                // the queue re-arming it.
                assert_eq!(rx.queued(), 0, "the piece is off the queue");
                let taken = started.elapsed();
                assert!(
                    taken < stall / 2,
                    "the piece was taken {taken:?} in, not inside the first deadline"
                );
                tokio::time::sleep(stall * 7 / 2).await;
                let written = piece.bytes().len();
                drop(piece);
                credit.add_permits(written);
                rx
            })
        };
        let mut to = delivery_over(&credit, tx);
        assert!(
            matches!(
                tokio::time::timeout(stall * 10, forward(b"ab", &mut to, &mut closed)).await,
                Ok(Ok(()))
            ),
            "a credit wait beside a write the connection had not finished was charged to the phone"
        );
        drop(
            writing
                .await
                .expect("the write finishes, and takes its piece off an empty queue in time"),
        );

        // And once that write is done the wait is the phone's: the same empty
        // queue, nothing left on its way, and no credit.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let written = tokio::spawn(async move {
            drop(rx.recv().await.expect("the piece the forward handed over"));
            rx
        });
        let mut to = delivery_over(&credit, tx);
        assert!(
            matches!(
                tokio::time::timeout(stall * 5, forward(b"ab", &mut to, &mut closed)).await,
                Ok(Err(ForwardEnd::CreditStarved))
            ),
            "a phone that took delivery and stopped crediting must still lose its terminal"
        );
        drop(written.await.expect("the write finishes"));
    }

    /// Forgiving outstanding delivery is finite, because the peer decides how
    /// much of it there is.
    ///
    /// A write completes when the peer reads its socket. So a peer that stops
    /// reading holds delivery outstanding for as long as it likes, and every
    /// deadline that expires there simply re-arms, because an expiry with a write
    /// in flight is never the phone's to answer for. The peer below does exactly
    /// that at a rate no honest phone comes near: it takes each piece, holds the
    /// write open for one and a half deadlines (well inside the connection's own
    /// 20s write deadline, so nothing else closes it), then completes it and
    /// returns one byte. Delivery is outstanding at every tick, so nothing about
    /// the credit clock alone can ever fire.
    ///
    /// It must still lose its terminal, because what it is holding is not just a
    /// deadline: the reader is blind to `%session-changed` while it waits, and
    /// the attachment pins one of [`MAX_TERMINAL_ATTACHMENTS`] leases and a live
    /// tmux control client. [`Delivery::peer_stall`] is what ends it — charged
    /// the elapsed time of every wait the peer held a write across, expired or
    /// not, and reduced only by what the bytes it took are worth at the floor
    /// rate, which at a byte per one and a half deadlines is nothing. The bound
    /// below is what proves the ending is the budget rather than the hundred
    /// bytes eventually going through: a hundred bytes at one byte per one and a
    /// half deadlines is a hundred and fifty deadlines of stalling, and this is
    /// given forty.
    ///
    /// What it does *not* prove is that the budget spans the attachment: it is
    /// one `forward` call, so a budget that was a local of the call would end it
    /// exactly the same way. That is
    /// `the_peer_budget_is_spent_across_the_whole_attachment_and_never_renewed`.
    #[tokio::test]
    async fn a_peer_that_holds_each_write_is_still_a_slow_consumer() {
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(200);
        let _deadline = StallDeadline::shortened_to(stall);
        let (_close, mut closed) = watch::channel(false);

        // A byte of credit to start, and a byte returned after each write
        // completes: exactly what a healthy phone does, at a pathological rate.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let phone = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                while let Some(piece) = rx.recv().await {
                    // The socket is not being read: the write stays pending for
                    // one and a half deadlines, then completes.
                    tokio::time::sleep(stall * 3 / 2).await;
                    let written = piece.bytes().len();
                    drop(piece);
                    credit.add_permits(written);
                }
            })
        };

        let mut to = delivery_over(&credit, tx);
        let outcome =
            tokio::time::timeout(stall * 40, forward(&[b'x'; 100], &mut to, &mut closed)).await;
        phone.abort();
        assert!(
            matches!(outcome, Ok(Err(ForwardEnd::SocketNotDraining))),
            "a peer that takes one byte per one and a half deadlines by holding \
             the write is a slow consumer and must lose its terminal, got {outcome:?}"
        );
    }

    /// The reason the phone is told is the one that explains the close, not the
    /// generic teardown that follows it.
    ///
    /// Every path that ends a carrier records a reason and then tears the rest
    /// down, and the teardown can record one of its own. Last-writer-wins would
    /// turn every specific cause — a stall, an identity change, a window switch —
    /// into whatever the unwind happened to say last, and the phone would be
    /// shown "the session ended" for a terminal that was closed under it.
    ///
    /// Driven at [`note_cause`] because the rule is the ordering and nothing
    /// else: a carrier that produced two reasons would have to be raced to make
    /// them land in a chosen order, and the race would be what the test proved.
    #[test]
    fn the_first_reason_recorded_is_the_one_the_phone_is_told() {
        let slot = std::sync::Mutex::new(None);
        note_cause(&slot, cause::CREDIT_STARVED);
        note_cause(&slot, cause::SESSION_EXITED);
        note_cause(&slot, cause::IDENTITY_MISMATCH);
        assert_eq!(
            *slot.lock().unwrap(),
            Some(cause::CREDIT_STARVED),
            "a later reason overwrote the one that explains the close"
        );

        // And an empty slot still takes the first thing it is told, or nothing
        // would ever be recorded at all.
        let fresh = std::sync::Mutex::new(None);
        note_cause(&fresh, cause::WINDOW_CHANGED);
        assert_eq!(*fresh.lock().unwrap(), Some(cause::WINDOW_CHANGED));

        // The two stalls share one wire code and differ only in the sentence, so
        // a slot that kept the code alone could not tell the phone which of them
        // happened. Asserted here because it is the whole reason the slot holds a
        // pair rather than a `&'static str`.
        assert_eq!(
            cause::CREDIT_STARVED.code,
            cause::SOCKET_NOT_DRAINING.code,
            "both stalls are the consumer failing to consume, and say so"
        );
        assert_ne!(
            cause::CREDIT_STARVED.reason,
            cause::SOCKET_NOT_DRAINING.reason
        );
    }

    /// A drain dropped with output still queued settles what it will never
    /// write, rather than leaving it owed for ever.
    ///
    /// The count means "on its way", and a connection that has gone is not going
    /// to deliver anything — so without this the last thing a dying connection
    /// does is leave the carrier's accounting permanently non-zero. Nothing is
    /// built on it (the handle closes before the drain drops, and a reader parked
    /// on a closed queue is told so by the reserve failing), which is exactly why
    /// it needs a test: an unwitnessed line in the file whose subject is this
    /// count is a line that can be deleted with the suite still green.
    #[tokio::test]
    async fn a_dropped_drain_settles_what_it_will_never_write() {
        let credit = Arc::new(Semaphore::new(0));
        let (tx, rx) = output_channel(4);
        let (_close, mut closed) = watch::channel(false);
        let mut to = delivery_over(&credit, tx);
        for _ in 0..3 {
            assert!(
                to.hand_over(b"queued".to_vec(), &mut closed).await.is_ok(),
                "the queue takes the chunk"
            );
        }
        assert_eq!(to.chunks.undelivered_count(), 3, "three chunks are owed");

        drop(rx);
        assert_eq!(
            to.chunks.undelivered_count(),
            0,
            "a drain that will never write must settle what it still holds"
        );

        // And not merely zeroed once: a hand-over behind the drop fails, and
        // gives its own charge back rather than leaving a phantom behind it.
        assert!(
            to.hand_over(b"late".to_vec(), &mut closed).await.is_err(),
            "the carrier learns the connection is gone"
        );
        assert_eq!(to.chunks.undelivered_count(), 0);
    }

    /// A peer that never lets a deadline expire is bounded all the same.
    ///
    /// The connection's own write deadline (20s) sits *below* the stall deadline
    /// (30s), so "the peer completed its write" and "no stall deadline expired"
    /// are compatible: a peer that finishes every write just inside the write
    /// deadline holds the reader parked for almost the whole of it and never
    /// trips the terminal's clock once. Against a bound counted in *expiries*
    /// that peer is charged nothing at all, and against one that also hands the
    /// budget back for any chunk that was charged nothing it is charged nothing
    /// for ever. The reader is blind to the control stream that entire time,
    /// holding a lease and a tmux client, which is precisely the harm the budget
    /// exists to bound.
    ///
    /// So the premise is asserted, not merely described: `expiries()` must read
    /// zero at the end. A test that only watched for the close could pass because
    /// the peer's holds happened to expire something, which would be the old
    /// property over again and not this one.
    ///
    /// The hold is nine tenths of a deadline, which is the same shape as a real
    /// peer sitting just inside the 20s write deadline against the 30s stall
    /// deadline, and the credit is ample so the reader parks in the hand-over
    /// rather than on credit — the queue is what the peer is holding shut.
    #[tokio::test]
    async fn a_peer_that_never_lets_a_deadline_expire_still_loses_the_terminal() {
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(200);
        let _deadline = StallDeadline::shortened_to(stall);
        let expiries = count_deadline_expiries();
        let (_close, mut closed) = watch::channel(false);

        // Ample credit, returned immediately: the phone is a model citizen about
        // the one thing the credit clock can see. Every wait below is therefore
        // the hand-over's, on a queue the peer is holding shut.
        let credit = Arc::new(Semaphore::new(1024));
        let (tx, mut rx) = output_channel(1);
        let peer = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                while let Some(piece) = rx.recv().await {
                    // Just inside the deadline, so it never expires — and the
                    // queue holds one slot, so the reader is parked for all of
                    // it.
                    tokio::time::sleep(stall * 9 / 10).await;
                    let written = piece.bytes().len();
                    drop(piece);
                    credit.add_permits(written);
                }
            })
        };

        let mut to = delivery_over(&credit, tx);
        let mut chunks = 0u32;
        let mut ended = None;
        // Generous enough that a sound bound has run out well inside it, and
        // finite so an unsound one fails here with its own message instead of
        // wedging the suite.
        while chunks < PEER_STALL_BUDGET_DEADLINES * 8 {
            chunks += 1;
            match forward(b"ab", &mut to, &mut closed).await {
                Ok(()) => continue,
                Err(end) => {
                    ended = Some(end);
                    break;
                }
            }
        }
        peer.abort();

        assert!(
            matches!(ended, Some(ForwardEnd::SocketNotDraining)),
            "a peer that holds the reader parked for nine tenths of every \
             deadline must lose its terminal however carefully it stays inside \
             them, got {ended:?} after {chunks} chunks"
        );
        assert_eq!(
            expiries(),
            0,
            "the peer expired a deadline, so this proves the old expiry-counted \
             bound and not the elapsed-time one"
        );
    }

    /// The time a peer manufactures is bounded across the *attachment*, and a
    /// chunk it lets through cleanly in between does not renew it.
    ///
    /// A per-chunk budget is no bound at all, and that is the defect this pins.
    /// The peer below holds every write it is given for one and a half deadlines
    /// and lets each chunk through in the end, so against a budget that started
    /// afresh on every call it would keep the reader parked, and blind to the
    /// control stream, for as long as it liked.
    ///
    /// The *other* way a budget stops spanning the attachment — being handed back
    /// to any chunk that was charged nothing — is not pinned here, and deliberately
    /// not: with four slots and a peer that holds every write, chunks pipeline, so
    /// a forward that looks clean still overlaps an earlier write and is charged
    /// for it. Interleaving an instant chunk into this test therefore proves
    /// nothing about renewal (measured: it passes with the refill restored). That
    /// property is `a_clean_chunk_costs_nothing_and_refills_nothing`, which drives
    /// the two cases apart deliberately instead of hoping the scheduler does.
    ///
    /// Driven over *successive* forwards on one [`Delivery`], and that shape is
    /// the whole test: a single long forward is
    /// `a_peer_that_holds_each_write_is_still_a_slow_consumer`, and it passes
    /// just as well against a budget that is a local of the call. Only crossing a
    /// call boundary can tell the two apart. The chunk count is asserted for the
    /// same reason — a run that ended inside the first chunk would prove the old
    /// property again and not this one.
    ///
    /// The loop is bounded so a budget that never runs out fails here with its
    /// own message rather than wedging the suite.
    #[tokio::test]
    async fn the_peer_budget_is_spent_across_the_whole_attachment_and_never_renewed() {
        // Shortened process-wide, so this takes the serial guard for the same
        // reason every other stall test does.
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(200);
        let _deadline = StallDeadline::shortened_to(stall);
        let (_close, mut closed) = watch::channel(false);

        // One byte of credit, and one byte returned after each write completes.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let peer = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                while let Some(piece) = rx.recv().await {
                    // The socket is not being read: the write stays pending for
                    // one and a half deadlines, then completes. Every chunk is
                    // therefore charged for the stretch the peer held it, and
                    // goes through in the end.
                    tokio::time::sleep(stall * 3 / 2).await;
                    let written = piece.bytes().len();
                    drop(piece);
                    credit.add_permits(written);
                }
            })
        };

        let mut to = delivery_over(&credit, tx);
        let mut chunks = 0u32;
        let mut ended = None;
        // Four chunks per deadline of budget: room for the peer to be forgiven
        // its whole allowance and then some, and a finite failure if it is not.
        while chunks < PEER_STALL_BUDGET_DEADLINES * 4 {
            chunks += 1;
            // Two bytes against one byte of credit: the first piece goes at once
            // and the second waits on the peer's held write, which is where the
            // deadline expires.
            match forward(b"ab", &mut to, &mut closed).await {
                Ok(()) => continue,
                Err(end) => {
                    ended = Some(end);
                    break;
                }
            }
        }
        peer.abort();

        assert!(
            matches!(ended, Some(ForwardEnd::SocketNotDraining)),
            "a peer that holds every write must lose its terminal however many \
             chunks it spreads that over, got {ended:?} after {chunks} chunks"
        );
        assert!(
            chunks > 1,
            "the run ended inside the first chunk, so this proves the per-chunk \
             bound and not the attachment-wide one"
        );
    }

    /// A chunk that gets through clean costs nothing and puts nothing back.
    ///
    /// This test used to assert the opposite, and the thing it asserted was the
    /// defect. A budget handed back to any chunk that was charged nothing is a
    /// budget a peer renews at will: it need only let one chunk through cleanly
    /// between the ones it holds, and interleaving is free. The bound has to span
    /// the attachment to be a bound at all, so nothing refills it.
    ///
    /// What the refill was *for* is still needed, and is still here — a phone
    /// that is honestly slow must not be closed. That job now belongs to
    /// [`PEER_DRAIN_WINDOW`], which is a better fit for it: a peer draining at a
    /// sane rate is charged zero on every write rather than charged and then
    /// forgiven, so an honest phone never approaches the budget in the first
    /// place and does not depend on reaching a clean chunk to survive.
    ///
    /// Both halves are asserted against the ledger itself rather than against a
    /// close, because a test that only watched for the close could not tell a
    /// budget that refilled from one that was never spent.
    #[tokio::test]
    async fn a_clean_chunk_costs_nothing_and_refills_nothing() {
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(200);
        let _deadline = StallDeadline::shortened_to(stall);
        let (_close, mut closed) = watch::channel(false);

        // Spend some: the peer takes the first piece and holds its write across a
        // deadline and a half before crediting.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let held = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                let piece = rx.recv().await.expect("the piece the forward handed over");
                tokio::time::sleep(stall * 3 / 2).await;
                let written = piece.bytes().len();
                drop(piece);
                credit.add_permits(written);
                rx
            })
        };
        let mut to = delivery_over(&credit, tx);
        assert!(
            matches!(
                tokio::time::timeout(stall * 10, forward(b"ab", &mut to, &mut closed)).await,
                Ok(Ok(()))
            ),
            "the chunk still goes through; only the peer's time is spent"
        );
        let mut rx = held.await.expect("the peer finishes");
        let spent = to.peer_stall;
        assert!(
            spent >= stall,
            "a write the peer held for a deadline and a half must come out of \
             the attachment's budget, got {spent:?}"
        );

        // Then a chunk that never waits: credit already in hand and a drain that
        // takes it at once, so the peer holds nothing and is charged nothing.
        credit.add_permits(8);
        let drained = tokio::spawn(async move {
            drop(rx.recv().await.expect("the piece the forward handed over"));
            rx
        });
        assert!(
            matches!(forward(b"cd", &mut to, &mut closed).await, Ok(())),
            "a clean chunk goes through"
        );
        drop(drained.await.expect("the drain finishes"));
        assert_eq!(
            to.peer_stall, spent,
            "a clean chunk must cost nothing — and must not hand back what the \
             peer already spent, which is the renewal this bound exists without"
        );
    }

    /// A peer moving bytes faster than the floor rate is charged nothing, even
    /// for a wait that lies entirely inside one of its writes.
    ///
    /// This is the half that keeps an honestly slow phone alive, and it is the
    /// one a per-wait floor got wrong. A write's time accrues from the moment it
    /// leaves the queue, but its bytes are not credited until it lands. So a wait
    /// that opens and closes inside one write sees elapsed time and *no* bytes:
    /// floored at zero per wait, it is charged in full, and the allowance those
    /// bytes were worth is discarded when they finally arrive. A peer at several
    /// times the floor rate was charged for its own writes that way — measured at
    /// 202ms on a 160 KiB/s peer whose chunk was worth 625ms of allowance — and
    /// nothing ever gave it back, because the ledger only goes up.
    ///
    /// Driven at [`Delivery::charge_peer`] rather than through a peer task
    /// because the interval has to be placed *inside* the write deliberately. A
    /// test that let the scheduler decide would sometimes straddle the completion
    /// and pass for the wrong reason, which is exactly how this survived.
    ///
    /// Production values throughout: the allowance is what a real 32 KiB chunk
    /// earns, and the budget is the real four minutes of charged time, so nothing
    /// here is an artefact of a shortened clock.
    #[tokio::test]
    async fn a_peer_above_the_floor_rate_is_charged_nothing_mid_write() {
        let _serial = fixture_test_guard().await;
        let (_close, mut closed) = watch::channel(false);
        let credit = Arc::new(Semaphore::new(1 << 20));
        let (tx, mut rx) = output_channel(4);
        let mut to = delivery_over(&credit, tx);

        // 32 KiB is worth 625ms of allowance at the floor rate, and the peer
        // below takes it in 200ms — a little over three times the floor.
        let chunk = 32 * 1024;
        let held_for = std::time::Duration::from_millis(200);
        to.hand_over(vec![b'x'; chunk], &mut closed)
            .await
            .expect("the queue takes it");
        let piece = rx.recv().await.expect("the connection takes it off");
        assert!(
            to.chunks.write_in_flight(),
            "the write must be in flight, or the interval below is not inside it"
        );

        // A wait that begins and ends with the write still in flight. Its bytes
        // have not landed, so this is the moment the peer is charged for time it
        // is going to pay for with bytes a moment later.
        let since = to.mark();
        tokio::time::sleep(held_for).await;
        to.charge_peer(since).expect("far inside the budget");

        // And the completion that carries them. What the interval banked has to
        // come back off, or a phone beating the floor rate pays for its own
        // writes and the ledger never forgets it.
        let since = to.mark();
        drop(piece);
        to.charge_peer(since).expect("far inside the budget");
        assert_eq!(
            to.peer_stall,
            std::time::Duration::ZERO,
            "a peer at three times the floor rate was charged {:?} for a chunk \
             worth {:?} of allowance",
            to.peer_stall,
            drain_allowance(chunk as u64)
        );
    }

    /// The daemon's own busyness is forgiven out of nothing at all.
    ///
    /// This is the half that makes the budget safe to have. A connection that has
    /// not taken what it was given is busy with something of its own — a backlog
    /// replay, a store read — and there is no honest bound the daemon may put on
    /// that which is not just a worse way of blaming the phone for it. So the
    /// wait re-arms and charges nothing, and the proof is that a wait crossing
    /// *more* deadlines than the whole budget leaves the budget untouched.
    ///
    /// It costs nothing now by construction rather than by a branch that
    /// remembers to forgive it: output sitting in the queue has no write in
    /// flight, so the clock the budget is spent in does not run while it sits
    /// there.
    ///
    /// **Not** "the clock never starts", which is what this said until it was
    /// checked. The drain below finally takes its piece and drops it, and that
    /// dequeue-to-drop span falls inside the reader's last credit wait, so a real
    /// interval is measured — microseconds, absorbed by what one byte allows. The
    /// figure asserted is therefore a margin and not zero: the wait covers ten
    /// deadlines of the daemon's own idleness, so anything under a quarter of one
    /// is proof that idleness was not charged, and it cannot be turned green by a
    /// scheduler hiccup the way an exact zero could. A regression that blamed the
    /// daemon would land two orders of magnitude the other side of it.
    ///
    /// The unit twin of `a_connection_that_stops_draining_is_not_a_slow_consumer`:
    /// that one proves a real carrier survives the pause, this one proves what it
    /// cost, which no carrier-level assertion can see.
    #[tokio::test]
    async fn a_wait_on_the_daemons_own_queue_spends_none_of_the_peers_budget() {
        let _serial = fixture_test_guard().await;
        let stall = std::time::Duration::from_millis(150);
        let _deadline = StallDeadline::shortened_to(stall);
        let (_close, mut closed) = watch::channel(false);

        // One byte of credit against two, and room in the queue: the first piece
        // is queued and left there, which is the connection not draining.
        let credit = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = output_channel(4);
        let idle = {
            let credit = Arc::clone(&credit);
            tokio::spawn(async move {
                // Longer than the whole budget before the connection so much as
                // looks at its queue.
                tokio::time::sleep(stall * (PEER_STALL_BUDGET_DEADLINES + 2)).await;
                let piece = rx.recv().await.expect("the piece the forward handed over");
                let written = piece.bytes().len();
                drop(piece);
                credit.add_permits(written);
                rx
            })
        };
        let mut to = delivery_over(&credit, tx);
        assert!(
            matches!(
                tokio::time::timeout(
                    stall * (PEER_STALL_BUDGET_DEADLINES + 8),
                    forward(b"ab", &mut to, &mut closed)
                )
                .await,
                Ok(Ok(()))
            ),
            "a wait on the daemon's own queue must never end the chunk"
        );
        drop(idle.await.expect("the drain finishes"));
        assert!(
            to.peer_stall < stall / 4,
            "the daemon's own backpressure was charged to the peer's budget: \
             {:?} spent across a wait of {} deadlines that the connection sat \
             out entirely",
            to.peer_stall,
            PEER_STALL_BUDGET_DEADLINES + 2
        );
    }

    /// The bound the doc comments quote, in the numbers they quote it in.
    ///
    /// Both are prose everywhere else — "four minutes of charged time",
    /// "51.2 KiB/s" — and prose does not fail a build when someone halves a
    /// constant. A security bound whose stated size and whose actual size can
    /// drift apart is worth less than one that is merely small, so the two are
    /// pinned to each other here.
    ///
    /// Asserted against the constants rather than through
    /// [`peer_stall_budget`], which follows a shortened test deadline: this test
    /// is about the production figures and takes no serial guard, so it must not
    /// read anything another test can be holding down.
    #[test]
    fn the_stated_bound_is_four_minutes_of_charged_time_above_fifty_one_kib_a_second() {
        assert_eq!(
            OUTPUT_STALL_DEADLINE * PEER_STALL_BUDGET_DEADLINES,
            std::time::Duration::from_secs(240),
            "the budget is documented as four minutes of charged time — the \
             blindness it allows is larger still"
        );
        assert_eq!(
            drain_allowance(u64::from(protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT)),
            std::time::Duration::from_secs(5),
            "a maximal chunk is documented as getting five seconds"
        );
        // The allowance is a rate and not a flat grace, which is the whole reason
        // an honestly slow phone survives it: half a chunk gets half the window,
        // so 256 KiB per 5s is 51.2 KiB/s at every size rather than only at the
        // maximum. Asserted at an exact division, because the arithmetic at
        // 51.2 KiB itself does not land on a whole millisecond and a test that
        // rounds is a test that stops meaning its own number.
        assert_eq!(
            drain_allowance(u64::from(protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT) / 2),
            std::time::Duration::from_millis(2_500),
            "the floor drain rate is documented as 51.2 KiB/s at every chunk size"
        );
        // And strictly under the write deadline, which is what stops a peer
        // sending maximal chunks just inside that deadline from being free. The
        // compile-time assertion beside the constant is the real guard; this is
        // the one that says why in a sentence a reader sees.
        assert!(
            drain_allowance(u64::from(protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT))
                < crate::ws_server::WRITE_DEADLINE
        );
    }

    /// One maximal input chunk arrives intact. The pane runs raw from the
    /// start — a canonical-mode tty caps a line at the kernel's `MAX_CANON`,
    /// which is the pane's own business, not the carrier's — and the marker
    /// prints only after the pane program has read every byte.
    #[tokio::test]
    async fn a_maximal_paste_arrives_intact() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start_running(
            "stty raw -echo; head -c 16384 >/dev/null; stty sane; echo PASTE-DONE; exec /bin/sh",
        ) else {
            eprintln!("skipped: no tmux");
            return;
        };
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        handle
            .input(vec![b'a'; 16 * 1024])
            .expect("a maximal chunk reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "PASTE-DONE",
            std::time::Duration::from_secs(10),
        )
        .await
        .expect("all 16384 bytes reached the pane");
        drop(handle);
    }

    /// Teardown while the reader is parked waiting for credit: the close must
    /// wake it, reap the client, release the leases — a same-session re-attach
    /// succeeds — and strand no descriptors.
    #[tokio::test]
    async fn teardown_under_credit_starvation_leaks_nothing() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let leases = TerminalLeases::new();
        let baseline = open_pipes();

        // One byte of credit: the screen painted at attach out-sizes it
        // immediately and parks the reader mid-chunk.
        let (handle, mut rx, _sem) = open_against(&fx, &leases, 1).await;
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("one credited byte arrives")
            .expect("the stream is live");
        assert_eq!(first.bytes().len(), 1, "credit bounds the chunk exactly");

        let pid = handle.client_pid();
        handle.close();
        drop(handle);

        // The parked reader wakes, the child is reaped, both leases release:
        // the same session accepts a new terminal, at the first attempt. No
        // retry loop, and that is the assertion — the attach waits on the lease
        // itself now (see `acquire_superseding`), so a caller that has to poll
        // for it is a caller papering over the window this test is about.
        let sem = Arc::new(Semaphore::new(64 * 1024));
        let (tx, _rx2) = output_channel(4);
        let (acks_tx, _acks_rx) = mpsc::unbounded_channel();
        let again = TerminalHandle::open(&leases, &fx.sock, &fx.uid, 80, 24, sem, tx, acks_tx)
            .await
            .expect("the leases came back after teardown");
        assert!(
            process_gone(pid),
            "the starved client was killed and reaped"
        );
        assert!(fx.alive());

        drop(again);
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if open_pipes() <= baseline {
                break;
            }
        }
        let after = open_pipes();
        assert!(
            // One of slack and not two: a carrier holds exactly two pipes, so
            // an allowance of two is an allowance for a whole leaked carrier.
            // The one covers a stray child elsewhere in the binary without
            // covering the thing this is looking for.
            after <= baseline + 1,
            "pipe descriptors grew from {baseline} to {after}"
        );
    }

    /// The pane program exiting ends the carrier from the other side: the
    /// chunks channel closes, and the client is reaped without being asked.
    #[tokio::test]
    async fn a_pane_exit_ends_the_stream_and_reaps_the_client() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;

        handle
            .input(b"exit\r".to_vec())
            .expect("input reaches the writer");
        // Drain (crediting as a phone would) until the channel closes.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(chunk) = rx.recv().await {
                sem.add_permits(chunk.bytes().len());
            }
        })
        .await
        .is_ok();
        assert!(closed, "the stream ends when the session does");

        let pid = handle.client_pid();
        drop(handle);
        let mut reaped = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if process_gone(pid) {
                reaped = true;
                break;
            }
        }
        assert!(reaped);
    }

    /// The input target is the scoped `session:window.pane` composite, and that
    /// scoping is what makes a migrated pane fail closed rather than deliver
    /// keystrokes somewhere the phone is not looking.
    ///
    /// Proven against real tmux, not only by comparing strings. The followed
    /// pane is broken out into another window while the reader is parked on
    /// credit — so it has not yet seen the `%window-pane-changed` that follows —
    /// and input sent across that window reaches *no* pane: not the migrated
    /// one, and not the pane that took its place. Crediting the reader then
    /// lets it catch up, and a second input reaches the pane it re-binds: that
    /// is what makes the absence a refusal rather than a broken input path, and
    /// it is also the fence the absence is read behind, since one client's
    /// commands run in the order they were written.
    #[tokio::test]
    async fn the_input_target_is_session_window_pane_scoped() {
        assert_eq!(super::scoped_target("$0", "@0", "%0"), "$0:@0.%0");
        assert_eq!(super::scoped_target("$7", "@3", "%12"), "$7:@3.%12");
        assert_eq!(
            tmux::send_keys_line(&super::scoped_target("$0", "@0", "%0"), b"x"),
            "send-keys -t $0:@0.%0 -H 78\n"
        );

        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let followed = only_pane(&fx);
        assert!(
            fx.tmux(&["split-window", "-d", "-t", &followed, "--", "/bin/sh"])
                .status
                .success(),
            "the window splits, so breaking the followed pane out leaves one behind"
        );
        let sibling = {
            let out = fx.tmux(&[
                "list-panes",
                "-t",
                "=cc-term",
                "-F",
                "#{pane_active} #{pane_id}",
            ]);
            let out = String::from_utf8_lossy(&out.stdout);
            out.lines()
                .find(|l| l.starts_with('0'))
                .and_then(|l| l.split_whitespace().nth(1))
                .expect("an inactive sibling pane")
                .to_string()
        };

        let leases = TerminalLeases::new();
        // One byte of credit. The attach paints the followed pane's screen,
        // which out-sizes that byte, so the reader is parked inside the paint
        // the moment the first byte arrives — and parked, it reads no further
        // control lines. That is the barrier: everything below happens while
        // the carrier's target is provably the pane about to be migrated.
        let (handle, mut rx, sem) = open_against(&fx, &leases, 1).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("one credited byte arrives")
            .expect("the stream is live");

        assert!(
            fx.tmux(&["break-pane", "-d", "-s", &followed])
                .status
                .success(),
            "the followed pane is broken out into its own window"
        );
        let one = b"echo MIGRATED_ONE\r";
        handle
            .input(one.to_vec())
            .expect("input reaches the writer");
        // `input` only queues those bytes; the writer reads the target when it
        // dequeues them. Crediting the reader before that read would let it
        // publish the *post*-migration target first, and MIGRATED_ONE would
        // then be sent — correctly — to the pane that took the migrated one's
        // place, failing the refusal asserted below for a reason that is not a
        // bug. The ack says the `send-keys` has reached the pipe, so the target
        // it names is already the pre-migration composite and nothing after
        // this line can move it.
        wait_input_written(one.len()).await;

        // Credit the reader. It finishes the paint, then reads the pane change
        // it has been sitting on, re-binds to the pane left in the window, and
        // input works again — so the refusal proven below is the composite
        // failing closed, not input being broken.
        sem.add_permits(64 * 1024);
        wait_published(2).await;
        handle
            .input(b"echo MIGRATED_TWO\r".to_vec())
            .expect("input reaches the writer");
        stream_until(
            &mut rx,
            &sem,
            "MIGRATED_TWO",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("input reaches the pane the carrier re-binds");

        // Now the refusal, fenced causally rather than by a timer. Both
        // `send-keys` went down the one stdin in the order they were written
        // and tmux runs one client's commands in that order, so MIGRATED_TWO
        // arriving proves MIGRATED_ONE's command was already processed — and it
        // reached no pane at all: not the migrated one, and not the pane that
        // took its place. Reading the screens before that ordering is
        // established would let "not yet delivered" pass as "refused".
        for pane in [&followed, &sibling] {
            let screen = fx.tmux(&["capture-pane", "-p", "-t", pane]);
            assert!(
                !String::from_utf8_lossy(&screen.stdout).contains("MIGRATED_ONE"),
                "input across the migrated composite reached {pane}, got {:?}",
                String::from_utf8_lossy(&screen.stdout)
            );
        }
        drop(handle);
    }

    /// The painted screen, byte for byte: attributes reset so a previous pane's
    /// colour cannot bleed, cleared and homed, rows joined with CRLF (a bare LF
    /// only moves down a line), and the cursor addressed 1-based from tmux's
    /// 0-based report. A cursor tmux would not name costs the position, never
    /// the screen.
    #[test]
    fn a_snapshot_paints_the_rows_and_places_the_cursor() {
        let rows = vec![b"first".to_vec(), Vec::new(), b"\x1b[31mred".to_vec()];
        assert_eq!(
            super::paint_snapshot(&rows, Some((4, 2))),
            b"\x1b[0m\x1b[2J\x1b[Hfirst\r\n\r\n\x1b[31mred\x1b[3;5H".to_vec()
        );
        assert_eq!(
            super::paint_snapshot(&rows, None),
            b"\x1b[0m\x1b[2J\x1b[Hfirst\r\n\r\n\x1b[31mred".to_vec()
        );
        // An empty screen still clears: a pane showing nothing must not leave
        // the previous pane's screen on the phone.
        assert_eq!(
            super::paint_snapshot(&[], Some((0, 0))),
            b"\x1b[0m\x1b[2J\x1b[H\x1b[1;1H".to_vec()
        );
    }

    /// The reply-body bound is a bound on *everything* a body can cost, not
    /// only on the bytes it carries. An unending run of empty lines carries no
    /// bytes at all, so a body charged by bytes alone would let it push row
    /// entries for ever — the growth without end the bound exists to refuse.
    /// Charging each line the entry that holds it is what makes the number of
    /// lines finite too.
    #[test]
    fn a_reply_body_of_empty_lines_trips_the_bound() {
        let mut body: Vec<Vec<u8>> = Vec::new();
        let mut held = 0usize;
        let mut admitted = 0usize;
        while super::hold_body_line(&mut body, &mut held, b"") {
            admitted += 1;
            // One line per byte of the bound is already past any honest per-row
            // cost, so reaching this is a body that grows without end.
            assert!(
                admitted <= REPLY_BODY_BYTES,
                "empty body lines never tripped the bound: {admitted} rows held"
            );
        }
        assert_eq!(body.len(), admitted, "every admitted line was stored");
        assert!(
            admitted <= REPLY_BODY_BYTES / BODY_ROW_OVERHEAD,
            "more rows were admitted than their entries fit in: {admitted}"
        );
        assert!(held <= REPLY_BODY_BYTES, "the bound was exceeded: {held}");

        // And a line's bytes still count: a body of full rows trips the bound
        // in far fewer lines than empty ones do.
        let mut wide: Vec<Vec<u8>> = Vec::new();
        let mut wide_held = 0usize;
        let row = vec![b'X'; 1024];
        let mut wide_admitted = 0usize;
        while super::hold_body_line(&mut wide, &mut wide_held, &row) {
            wide_admitted += 1;
        }
        assert!(
            wide_admitted < admitted,
            "rows carrying bytes were not charged for them: {wide_admitted} vs {admitted}"
        );
        assert!(
            wide_held <= REPLY_BODY_BYTES,
            "the bound was exceeded: {wide_held}"
        );
    }

    /// A refusal costs the body nothing and does not close it to what still
    /// fits. That ordering is what the overflow latch is built on: the caller
    /// gives the whole body up when a line is refused, so a refused line must
    /// leave the count untouched — and a smaller line behind it is still
    /// admitted, exactly as it was when only bytes were counted.
    #[test]
    fn a_line_refused_by_the_bound_leaves_the_body_it_did_not_join() {
        let mut body: Vec<Vec<u8>> = Vec::new();
        let mut held = 0usize;
        assert!(super::hold_body_line(&mut body, &mut held, &[b'X'; 1024]));
        let after_first = held;

        let too_big = vec![b'Y'; REPLY_BODY_BYTES];
        assert!(
            !super::hold_body_line(&mut body, &mut held, &too_big),
            "a line larger than the whole bound cannot be held"
        );
        assert_eq!(held, after_first, "a refused line is charged to nothing");
        assert_eq!(body.len(), 1, "a refused line is not stored");

        assert!(
            super::hold_body_line(&mut body, &mut held, b"small"),
            "a line that fits is still admitted behind one that did not"
        );
        assert_eq!(body.len(), 2);
        assert_eq!(body[1], b"small".to_vec());
        assert_eq!(held, after_first + 5 + BODY_ROW_OVERHEAD);
    }

    /// A line the reader cannot finish under its cap is refused rather than
    /// allocated for, and the lines behind it still read.
    ///
    /// The refusal is the point: without it the buffer is whatever the stream
    /// says it is, and a client that stopped emitting newlines would take the
    /// daemon's memory with no bytes of forgery and no protocol violation the
    /// reader could name. Resynchronising at the newline is what keeps that
    /// refusal from being a second denial of service — one bad line costs one
    /// line, not the stream.
    #[tokio::test]
    async fn a_line_that_outruns_the_cap_is_refused_and_the_stream_reads_on() {
        let mut line = Vec::new();

        // An ordinary line, its newline stripped, and the stream left exactly
        // after it.
        let mut src = &b"%output %0 hi\n%exit\n"[..];
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 64).await,
            super::LineRead::Line
        ));
        assert_eq!(line, b"%output %0 hi".to_vec());
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 64).await,
            super::LineRead::Line
        ));
        assert_eq!(line, b"%exit".to_vec());
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 64).await,
            super::LineRead::Ended
        ));

        // Exactly the cap still fits: the bound is on what is kept, not on what
        // is one byte short of it.
        let mut src = &b"0123456789\n"[..];
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::Line
        ));
        assert_eq!(line, b"0123456789".to_vec());

        // One byte more is refused — and the buffer holds the cap, never the
        // line, which is the whole bound.
        let long = format!("{}\nbehind\n", "Z".repeat(4096));
        let mut src = long.as_bytes();
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::TooLong
        ));
        assert_eq!(line.len(), 10, "the buffer grew past the cap");
        // The next line is still whole: the refused line was drained to its
        // newline, not left to be mis-read as the lines behind it.
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::Line
        ));
        assert_eq!(line, b"behind".to_vec());

        // A last line with no newline is still a line; an over-long one with no
        // newline is still over-long.
        let mut src = &b"tail"[..];
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::Line
        ));
        assert_eq!(line, b"tail".to_vec());
        let unending = "Z".repeat(4096);
        let mut src = unending.as_bytes();
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::TooLong
        ));
        assert_eq!(line.len(), 10, "the buffer grew past the cap");

        // All of the above through a reader that hands over a few bytes at a
        // time, which is what a pipe does: the cap must hold across chunks, and
        // a line split over several of them must still arrive whole.
        let mut src = BufReader::with_capacity(4, long.as_bytes());
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::TooLong
        ));
        assert_eq!(line.len(), 10, "the buffer grew past the cap across chunks");
        assert!(matches!(
            super::read_capped_line(&mut src, &mut line, 10).await,
            super::LineRead::Line
        ));
        assert_eq!(
            line,
            b"behind".to_vec(),
            "a line spanning several chunks did not arrive whole"
        );
    }

    /// A pane row printed so that one line of its own screen outruns the cap.
    ///
    /// Eighty cells, each changing truecolour foreground, background and six
    /// attributes, which `capture-pane -e` reports as a single row of some
    /// 2.8 KB (measured on 3.7b).
    const ROW_OVER_THE_CAP: &str = concat!(
        "i=0; while [ $i -lt 80 ]; do ",
        r#"printf "\033[1;3;4;5;7;9;38;2;%d;%d;%d;48;2;%d;%d;%dm#" "#,
        "$i $i $i $((255-i)) $((255-i)) $((255-i)); i=$((i+1)); done; ",
        r#"printf "\033[0m\n""#,
        "\n"
    );

    /// The cap the two tests below read against. No pane can print a line near
    /// the production cap, so they shorten it to a number that sits in the gap
    /// between what the fixture legitimately emits and what they contrive —
    /// measured on 3.7b, six times the longest line of an ordinary attach (83
    /// bytes), five times under [`ROW_OVER_THE_CAP`]'s captured row (2864), and
    /// twice under the smallest `%output` chunk a flood produces (1035: tmux
    /// chunks pane bytes at 1024 or 2048 with an eleven-byte prefix, so no
    /// chunk of a burst this size lands below it).
    const SHORT_LINE_CAP: usize = 512;

    /// A capture body line past the cap loses the snapshot and keeps the bytes,
    /// exactly as a body past [`REPLY_BODY_BYTES`] does.
    ///
    /// The two bounds are deliberately the same number, so a line the reader
    /// refuses to finish is one the body would have refused to hold — and it
    /// must fail the same way: give the screen up, flush what was held, stay
    /// live. The alternative this rules out is the one that would matter, a
    /// carrier that painted a screen with a row silently truncated to the cap
    /// or missing altogether.
    #[tokio::test]
    async fn a_capture_row_past_the_line_cap_gives_up_the_screen_and_not_the_stream() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let pane = only_pane(&fx);
        // Printed before the attach, so it reaches the carrier only as a
        // capture row and never as `%output` — this test is about the body
        // path, and the notification path has its own.
        assert!(
            fx.tmux(&["send-keys", "-t", &pane, "-l", ROW_OVER_THE_CAP])
                .status
                .success(),
            "the over-long row is printed into the pane"
        );
        wait_pane_shows(&fx, &pane, "###").await;

        let _cap = LineCap::shortened_to(SHORT_LINE_CAP);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        // Both halves of the snapshot pair are answered, so the carrier has
        // been all the way through the path that would have painted.
        wait_snapshot_replies(2).await;

        // The stream is still live afterwards, and carries the marker the pane
        // prints next.
        handle
            .input(b"printf 'CAPPED_%s\\n' LIVE\r".to_vec())
            .expect("input reaches the writer");
        let mut streamed = stream_until(
            &mut rx,
            &sem,
            "CAPPED_LIVE",
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("the stream survives a capture row past the cap");
        drain_for(
            &mut rx,
            &sem,
            &mut streamed,
            std::time::Duration::from_millis(300),
        )
        .await;
        // And no screen was painted from a body that lost a line. A truncated
        // paint would have cleared first, which is the one byte sequence a
        // snapshot cannot reach the phone without.
        assert!(
            !streamed.windows(4).any(|w| w == b"\x1b[2J"),
            "a screen was painted from a body missing a line, got {:?}",
            String::from_utf8_lossy(&streamed)
        );
        drop(handle);
    }

    /// A notification line past the cap closes the carrier as a protocol error.
    ///
    /// Outside a reply block there is no body to give up, and no legitimate
    /// line to lose: tmux chunks `%output` itself, so a notification with no
    /// end is not the protocol any more. The carrier refuses to keep reading a
    /// stream it can no longer frame, rather than resynchronising on a newline
    /// it has no reason to trust — the same fail-closed choice it makes for a
    /// session it did not verify.
    #[tokio::test]
    async fn a_notification_line_past_the_cap_closes_the_carrier() {
        let _serial = fixture_test_guard().await;
        let Some(fx) = Fixture::start() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let _cap = LineCap::shortened_to(SHORT_LINE_CAP);
        let leases = TerminalLeases::new();
        let (handle, mut rx, sem) = open_against(&fx, &leases, 64 * 1024).await;
        // The attach's own snapshot first: an empty 80-column screen, every
        // line of it far inside the cap, so what closes the carrier below can
        // only be what is printed after this.
        wait_snapshot_replies(2).await;

        // Forty thousand characters in one burst. tmux chunks that into
        // `%output` lines of 1035 and 2059 bytes (measured on 3.7b), every one
        // of them past the shortened cap.
        handle
            .input(b"printf '%040000d\\n' 0\r".to_vec())
            .expect("input reaches the writer");

        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(chunk) = rx.recv().await {
                sem.add_permits(chunk.bytes().len());
            }
        })
        .await
        .is_ok();
        assert!(closed, "a notification line past the cap ends the stream");
        assert_eq!(
            handle.close_reason(),
            protocol::ws::terminal_close::PROTOCOL_ERROR,
            "a line the reader cannot frame closes as protocol_error"
        );
        assert!(
            fx.alive(),
            "the session outlives the carrier that refused it"
        );
        drop(handle);
    }

    /// The lease is one-per-session and globally bounded, and a dropped lease
    /// frees both.
    ///
    /// The refusal's *code* is what is pinned, not its prose. The two used to be
    /// one code told apart by an English sentence the phone also displays
    /// verbatim; they are now `session_busy` and `attachment_limit`, and a
    /// reader that has to tell them apart reads the code.
    #[tokio::test]
    async fn leases_are_bounded_and_release_on_drop() {
        let leases = TerminalLeases::new();
        let a = leases
            .acquire("uid-1", LeaseHolder::inert())
            .expect("first is free");
        let Err(busy) = leases.acquire("uid-1", LeaseHolder::inert()) else {
            panic!("a session that already has a terminal is refused the second");
        };
        assert_eq!(
            busy.close_code(),
            protocol::ws::terminal_close::SESSION_BUSY,
            "the per-session lease has a code of its own, not the global cap's"
        );
        let _b = leases
            .acquire("uid-2", LeaseHolder::inert())
            .expect("a second session is free");
        drop(a);
        let _again = leases
            .acquire("uid-1", LeaseHolder::inert())
            .expect("released by the drop");
    }

    /// The global cap refuses the ninth attachment whatever session it names,
    /// under the one code that now means only the cap.
    #[tokio::test]
    async fn the_global_cap_refuses_the_overflow_attachment() {
        let leases = TerminalLeases::new();
        let held: Vec<Lease> = (0..MAX_TERMINAL_ATTACHMENTS)
            .map(|n| {
                leases
                    .acquire(&format!("uid-{n}"), LeaseHolder::inert())
                    .expect("under the cap")
            })
            .collect();
        let Err(full) = leases.acquire("uid-overflow", LeaseHolder::inert()) else {
            panic!("the attachment past the cap is refused");
        };
        assert_eq!(
            full.close_code(),
            protocol::ws::terminal_close::ATTACHMENT_LIMIT
        );
        assert!(
            full.reason()
                .contains(&MAX_TERMINAL_ATTACHMENTS.to_string()),
            "the global cap names the count it is full at, got {:?}",
            full.reason()
        );
        drop(held);
        let _free = leases
            .acquire("uid-overflow", LeaseHolder::inert())
            .expect("the cap released");
    }

    /// A new attach takes a session's terminal over from whoever holds it: the
    /// holder is closed as `superseded`, and the newcomer gets the lease.
    ///
    /// This is the stale-lease lockout, in miniature. The holder here stands in
    /// for a carrier whose phone is gone — it never notices the close and never
    /// gives the lease back on its own — and the release comes from the reaper's
    /// stand-in, a task that drops the guard once it sees the close fire. Take
    /// the supersede out and this hangs on the wait and then answers
    /// `session_busy`, which is exactly what the reconnecting phone used to get
    /// about its own dead terminal.
    #[tokio::test]
    async fn a_new_attach_takes_a_sessions_terminal_over_from_a_dead_holder() {
        let leases = TerminalLeases::new();
        let holder = LeaseHolder {
            close: watch::channel(false).0,
            credit: Arc::new(Semaphore::new(0)),
            cause: Arc::new(Mutex::new(None)),
        };
        let lease = leases
            .acquire("uid-1", holder.clone())
            .expect("the holder has it");

        // The reaper's stand-in: the lease is released by the close being
        // *observed*, never by this test's own timing, so the wait below is the
        // real mechanism and not a sleep dressed up as one.
        let mut closed = holder.close.subscribe();
        tokio::spawn(async move {
            let _lease = lease;
            let _ = closed.changed().await;
        });

        let taken = leases
            .acquire_superseding(
                "uid-1",
                LeaseHolder::inert(),
                std::time::Duration::from_secs(5),
            )
            .await;
        assert!(
            taken.is_ok(),
            "the newcomer must get the lease, got {:?}",
            taken.err().map(|err| err.reason())
        );
        assert_eq!(
            holder
                .cause
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .map(|cause| cause.code),
            Some(protocol::ws::terminal_close::SUPERSEDED),
            "the displaced holder must be told it was superseded, so its own \
             connection puts that on the wire rather than 'the session ended'"
        );
    }

    /// A takeover succeeds against a Mac that is completely full, when one of
    /// the terminals filling it is the target session's own.
    ///
    /// The saturated case of the stale-lease lockout, and the one the existing
    /// cap test cannot see: it fills every slot with *unrelated* sessions on
    /// purpose. Here the eighth slot belongs to `uid-1` itself — a phone
    /// backgrounded with its terminal still leased — and the same phone comes
    /// back asking for `uid-1`. Take the global permit before the session map
    /// is consulted and this answers `attachment_limit`, the supersede branch
    /// never runs, and the phone is refused its own session for as long as TCP
    /// takes to notice the dead socket. Both leases come back together when the
    /// incumbent releases, so the newcomer finds the slot it needs.
    #[tokio::test]
    async fn a_takeover_wins_even_when_every_slot_on_the_mac_is_taken() {
        let leases = TerminalLeases::new();
        // Seven other sessions, and `uid-1` itself, is exactly the cap.
        let _others: Vec<Lease> = (1..MAX_TERMINAL_ATTACHMENTS)
            .map(|n| {
                leases
                    .acquire(&format!("uid-other-{n}"), LeaseHolder::inert())
                    .expect("under the cap")
            })
            .collect();
        let holder = LeaseHolder {
            close: watch::channel(false).0,
            credit: Arc::new(Semaphore::new(0)),
            cause: Arc::new(Mutex::new(None)),
        };
        let incumbent = leases
            .acquire("uid-1", holder.clone())
            .expect("the last slot goes to the target session");

        // The reaper's stand-in again: the lease — and with it the global
        // permit — comes back only because the close was observed.
        let mut closed = holder.close.subscribe();
        tokio::spawn(async move {
            let _lease = incumbent;
            let _ = closed.changed().await;
        });

        let taken = leases
            .acquire_superseding("uid-1", LeaseHolder::inert(), SUPERSEDE_WAIT)
            .await;
        assert!(
            taken.is_ok(),
            "a session with an incumbent must be superseded whatever the global \
             cap is doing, got {:?}",
            taken.err().map(|err| err.close_code())
        );
    }

    /// A release is atomic to anyone who can observe it: no acquirer ever finds
    /// the session gone from the map while the global permit is still out.
    ///
    /// The test above proves the *acquire* order and cannot prove this one. It
    /// is a current-thread test, so the supersede it runs cannot be executing
    /// while the incumbent's `Drop` is: whatever order those two steps happen
    /// in, the taker only ever looks after both are done. It passed against the
    /// old release just as it does against this one.
    ///
    /// So this stops the release *between* its two steps — the `release_seam`
    /// exists for exactly that — and asks a concurrent acquirer what it can see
    /// from there, on a Mac whose every other slot is taken so that a permit
    /// still out is a refusal rather than a detail. Put the permit's return back
    /// outside the map guard and the acquirer walks into an empty map with no
    /// slot to take and this fails on `attachment_limit`: a phone refused its
    /// own session a microsecond before there was room for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_release_never_shows_a_taker_a_free_session_without_the_slot_for_it() {
        // Its own uid, so the seam is armed for this test's lease and no other
        // test's release can trip it.
        const UID: &str = "uid-release-seam";
        let leases = TerminalLeases::new();
        // Seven unrelated sessions, and `UID` itself, is exactly the cap: the
        // slot the taker needs is the one the release is handing back.
        let _others: Vec<Lease> = (1..MAX_TERMINAL_ATTACHMENTS)
            .map(|n| {
                leases
                    .acquire(&format!("uid-other-{n}"), LeaseHolder::inert())
                    .expect("under the cap")
            })
            .collect();
        let incumbent = leases
            .acquire(UID, LeaseHolder::inert())
            .expect("the last slot goes to the target session");

        // Long enough that the acquirer below cannot merely be slow: with the
        // old ordering it is answered immediately and wrongly, and with this one
        // it waits on the map lock until both leases are genuinely back.
        let (armed, entered) = release_seam::arm(UID, std::time::Duration::from_millis(500));
        // On a blocking thread, because the seam parks it: the release is a
        // synchronous `Drop` and has no yield point to give a worker back.
        let releasing = tokio::task::spawn_blocking(move || drop(incumbent));
        entered
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the release reached the seam");

        // The release is now parked with the session removed and the permit not
        // yet returned. This is the only moment at which the two steps can be
        // told apart.
        let taken = leases.acquire(UID, LeaseHolder::inert());
        drop(armed);
        releasing.await.expect("the release finished");
        let taken = match taken {
            Ok(lease) => lease,
            Err(refused) => panic!(
                "a taker that reached the map mid-release was refused {:?} ({:?}): the \
                 session's own slot was in the middle of coming back to it",
                refused.close_code(),
                refused.reason()
            ),
        };
        drop(taken);
    }

    /// A takeover that the incumbent never lets go of ends as `session_busy` —
    /// and leaves the incumbent's lease exactly where it was.
    ///
    /// The distinction the wire code buys: `attachment_limit` is the Mac being
    /// full and is not worth retrying on its own; this is one stuck child and
    /// is. And the lease surviving is what says the newcomer did not half-take
    /// something — a session whose lease was released by a *failed* takeover
    /// would let two disposable clients overlap, which is the one thing the
    /// lease exists to stop.
    #[tokio::test]
    async fn a_takeover_the_incumbent_never_releases_ends_as_session_busy() {
        let leases = TerminalLeases::new();
        let holder = LeaseHolder::inert();
        // Held for the whole test: nothing releases it, which is the stimulus.
        let _stuck = leases
            .acquire("uid-1", holder.clone())
            .expect("the incumbent has it");

        let refused = leases
            .acquire_superseding(
                "uid-1",
                LeaseHolder::inert(),
                std::time::Duration::from_millis(200),
            )
            .await;
        let Err(busy) = refused else {
            panic!("a lease nobody releases cannot be taken over");
        };
        assert_eq!(
            busy.close_code(),
            protocol::ws::terminal_close::SESSION_BUSY,
            "not `attachment_limit`: the Mac is not full, one terminal is stuck"
        );
        assert!(
            leases.held("uid-1"),
            "the incumbent's lease survives the failed takeover"
        );
        // And it was asked to close, which is what makes a retry likely to work
        // rather than hopeful.
        assert_eq!(
            holder
                .cause
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .map(|cause| cause.code),
            Some(protocol::ws::terminal_close::SUPERSEDED)
        );
    }

    /// The bound is a wait on the *release*, not a sleep: a lease handed back
    /// early is taken early.
    ///
    /// Written against a five-second bound and asserted to finish in well under
    /// it, so a supersede that polled at some interval, or waited out the whole
    /// timeout because it registered for the release after checking for it,
    /// fails here rather than merely being slow.
    #[tokio::test]
    async fn a_takeover_ends_when_the_lease_is_handed_back_not_when_the_bound_expires() {
        let leases = TerminalLeases::new();
        let lease = leases
            .acquire("uid-1", LeaseHolder::inert())
            .expect("the incumbent has it");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(lease);
        });

        let started = std::time::Instant::now();
        leases
            .acquire_superseding(
                "uid-1",
                LeaseHolder::inert(),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("the lease came free");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the takeover waited {:?}, which is the bound expiring rather than \
             the release waking it",
            started.elapsed()
        );
    }

    /// No `terminal_output` piece exceeds what one frame may carry, however much
    /// credit the phone has granted.
    ///
    /// `MAX_TERMINAL_CHUNK_BYTES` is documented as receiver-enforced, and the
    /// forwarder used to hand over as much as the current grant covered — up to
    /// the full 256 KiB ceiling — which the connection then put on the wire as
    /// one frame. A client implementing the documented check would have killed
    /// healthy terminals on the attach snapshot alone. The grant here is far
    /// above the bound *and* above the chunk, so nothing but the cap can split
    /// this.
    #[tokio::test]
    async fn no_hand_over_exceeds_one_frames_worth_however_large_the_grant() {
        let max = protocol::ws::MAX_TERMINAL_CHUNK_BYTES;
        let credit = Arc::new(Semaphore::new(
            protocol::ws::TERMINAL_MAX_OUTSTANDING_CREDIT as usize,
        ));
        let (chunks, mut rx) = output_channel(64);
        let mut delivery = Delivery::new(
            Arc::clone(&credit),
            chunks,
            Arc::new(Mutex::new(None::<CloseCause>)),
        );
        let (_close, mut closed) = watch::channel(false);

        // Two and a bit frames' worth, so the split is exercised and the
        // remainder is not itself a multiple of the bound.
        let painted = vec![b'x'; max * 2 + 7];
        let forwarded = tokio::spawn(async move {
            forward(&painted, &mut delivery, &mut closed).await.unwrap();
            delivery
        });

        let mut pieces = Vec::new();
        let mut seen = 0;
        while seen < max * 2 + 7 {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("the forward keeps handing over")
                .expect("the stream is open");
            seen += chunk.bytes().len();
            pieces.push(chunk.bytes().len());
            // Dropped at once, standing in for the connection's write returning.
            drop(chunk);
        }
        let _ = forwarded.await;

        assert_eq!(seen, max * 2 + 7, "every byte was handed over");
        assert!(
            pieces.iter().all(|piece| *piece <= max),
            "a hand-over of {pieces:?} exceeds the {max}-byte bound one \
             terminal_output may carry"
        );
        assert!(
            pieces.len() >= 3,
            "the chunk must have been split at the bound, not handed over whole \
             ({pieces:?})"
        );
    }
}
