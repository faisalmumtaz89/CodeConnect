//! The tailnet WebSocket protocol the iPhone speaks.
//!
//! Reconnect is one mechanism: `subscribe{session_id, after_seq}` replays the
//! log gap-free. When the daemon cannot guarantee gap-freedom (a slow client
//! fell off the broadcast ring) it emits an explicit `Resync` event rather than
//! silently skipping — a gap the client cannot see is the failure mode that
//! makes an event log worthless.

use serde::{Deserialize, Serialize};

use crate::event::{Event, SessionSummary};
use crate::ipc::PromptPresence;

/// Ceiling on a single client message. Answers and prompts are small; anything
/// larger is malformed.
pub const MAX_CLIENT_MESSAGE_BYTES: usize = 1024 * 1024;

/// Ceiling on one `send_text` body.
///
/// The message limit above is a transport bound; this is a *product* bound. A
/// megabyte typed into a TTY one keystroke-batch at a time is not a takeover,
/// it is a way to wedge a session, and nothing a human types on a phone comes
/// close to this.
pub const MAX_SEND_TEXT_BYTES: usize = 8 * 1024;

// ------------------------------------------------------------------ terminal
//
// The Terminal tab attaches to the exact live tmux session over this same
// paired connection. Bytes ride as base64 inside the JSON frames; flow is
// governed by a **credit** window in each direction (WebSocket already gives
// ordered reliable delivery, so there is no sequence number and no replay).
// A grant of `n` credit permits the peer to send `n` more decoded bytes; the
// receiver returns credit only once it has taken those bytes off the wire's
// hands — the phone after feeding its terminal view, the daemon after handing
// them to the tmux client for its active pane. Outstanding credit therefore
// bounds the buffer each side itself keeps. tmux may still hold a stalled
// pane's output in its own server-side buffer, but the daemon closes the
// attachment once output credit has stalled past a deadline, so that backlog
// is time-bounded rather than able to grow for as long as the phone is silent.

/// The largest decoded chunk one `terminal_input`/`terminal_output` may carry.
/// Small on purpose: a noisy pane must not monopolise the shared socket, and
/// interactive traffic is tiny. Enforced by the receiver before the bytes go
/// anywhere near the pane.
pub const MAX_TERMINAL_CHUNK_BYTES: usize = 16 * 1024;

/// Bytes the phone lets the daemon send before the first replenishment
/// (`terminal_attach.output_credit`), and the reverse (`terminal_attached.
/// input_credit`). Enough to redraw a screen without a stall on attach.
pub const TERMINAL_INITIAL_OUTPUT_CREDIT: u32 = 64 * 1024;
pub const TERMINAL_INITIAL_INPUT_CREDIT: u32 = 32 * 1024;

/// The most credit that may be outstanding in one direction. A grant that
/// would push the total past this is a protocol error: it is the bound that
/// keeps a misbehaving or hostile peer from asking for unbounded buffering.
pub const TERMINAL_MAX_OUTSTANDING_CREDIT: u32 = 256 * 1024;

// An initial grant at or above the ceiling would make the daemon's own first
// `terminal_attach` a protocol error against itself, and the ceiling is what
// bounds a hostile peer's buffering — so it stays the larger of the two, and
// says so at compile time rather than waiting for a test run.
const _: () = assert!(TERMINAL_INITIAL_OUTPUT_CREDIT < TERMINAL_MAX_OUTSTANDING_CREDIT);
const _: () = assert!(TERMINAL_INITIAL_INPUT_CREDIT < TERMINAL_MAX_OUTSTANDING_CREDIT);

/// Terminal geometry bounds. Below 2 is not a usable grid; the ceilings are
/// far above any real display and exist only to reject a hostile resize.
pub const TERMINAL_MIN_COLS: u16 = 2;
pub const TERMINAL_MAX_COLS: u16 = 512;
pub const TERMINAL_MIN_ROWS: u16 = 2;
pub const TERMINAL_MAX_ROWS: u16 = 256;

/// The longest an `attachment_id` may be. A canonical UUID is 36; this leaves
/// room for a client that uses its own scheme without inviting an unbounded
/// key.
pub const MAX_ATTACHMENT_ID_BYTES: usize = 64;

/// Why a terminal attachment ended, on the wire.
///
/// A stable string rather than a closed enum so a newer daemon can name a
/// reason an older client renders verbatim without a decode failure — the
/// same shape as [`ServerMessage::Error`]'s `code`. The canonical set:
///
///   * `session_not_hosted` — no live CodeConnect session carries that uid.
///   * `identity_mismatch` — the uid resolved to a session that changed under
///     us, or two sessions claim it; nothing was streamed.
///   * `tmux_unavailable` — tmux could not be run or answered indeterminately.
///   * `attachment_limit` — the Mac's *global* cap on open terminals, and only
///     that. A session that already has one is `session_busy` or a takeover.
///   * `session_busy` — the terminal this session already had was asked to
///     close so this attach could take it over, and it had not finished closing
///     within the daemon's bound. Retrying is the answer; the attach that was
///     refused streamed nothing.
///   * `superseded` — a *later* attach took this session's terminal over, so
///     this one ended. The phone that sees it on the id it is showing has lost
///     the terminal to another attach (its own reconnect, or another device);
///     the phone that sees it on an id it just asked for lost a race to a
///     third.
///   * `not_authorised` — this connection may not open a terminal: the static
///     bootstrap token, a connection whose transport is not private, or a
///     device revoked while the attach was in flight.
///   * `protocol_error` — a malformed, out-of-order, or over-credit input
///     message. Input past the granted window comes back as this.
///   * `slow_consumer` — the phone stopped returning *output* credit for long
///     enough that the daemon closed the attachment rather than stay blind to
///     the session behind a stalled stream. (Input has no such close: the
///     credit protocol simply stops the phone.)
///   * `window_changed` — the session is alive but its active window changed,
///     and the single-window terminal does not follow a window switch.
///   * `detached` — the phone asked to detach, or the tab closed.
///   * `session_exited` — the session ended under its viewer.
pub mod terminal_close {
    pub const SESSION_NOT_HOSTED: &str = "session_not_hosted";
    pub const IDENTITY_MISMATCH: &str = "identity_mismatch";
    pub const TMUX_UNAVAILABLE: &str = "tmux_unavailable";
    pub const ATTACHMENT_LIMIT: &str = "attachment_limit";
    pub const SESSION_BUSY: &str = "session_busy";
    pub const SUPERSEDED: &str = "superseded";
    pub const NOT_AUTHORISED: &str = "not_authorised";
    pub const PROTOCOL_ERROR: &str = "protocol_error";
    pub const SLOW_CONSUMER: &str = "slow_consumer";
    pub const WINDOW_CHANGED: &str = "window_changed";
    pub const DETACHED: &str = "detached";
    pub const SESSION_EXITED: &str = "session_exited";
}

/// What a client can understand, carried on `hello` and on `register_push`.
///
/// The one fact it holds today is which agents the client can render and drive.
/// A client that sends none — or a daemon reading a frame that predates this
/// field — is **Claude-only**: [`ClientFeatures::supports`] reads an empty set as
/// the floor and requires every other agent to be named, which is the honest,
/// fail-closed reading. It is a struct rather than a bare `Vec` so a later feature
/// is one additive field here, not a second parallel list on two messages.
///
/// **Wire-legal and inert on both messages today: the daemon accepts an
/// advertisement and stores nothing**, so what a client says here scopes nothing —
/// not a name resolution, not a push. See [`RegisterPush::features`] for why the
/// write side was deleted rather than kept working. The predicate below is live all
/// the same, because push eligibility already asks it — of the *device row*, which
/// this phase leaves `NULL` on every device.
///
/// [`RegisterPush::features`]: ClientMessage::RegisterPush::features
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientFeatures {
    /// Agents this client can observe and act on. Absent/empty ⇒ Claude-only.
    /// An unrecognised agent name round-trips as
    /// [`crate::agent::AgentKind::Unsupported`] and grants nothing.
    #[serde(default)]
    pub agents: Vec<crate::agent::AgentKind>,
}

impl ClientFeatures {
    /// True when the client advertised it can handle this agent. Claude-only is
    /// the floor, so Claude is supported by a client that named it *or* that
    /// named nothing at all (the legacy shape); every other agent must be
    /// explicitly advertised.
    ///
    /// **An unrecognised agent grants nothing, and that has to be said rather
    /// than left to the set.** `Vec::contains` compares
    /// [`crate::agent::AgentKind::Unsupported`] by its preserved name, so a
    /// client that advertised `["gemini"]` matched a doorbell *for* `gemini` and
    /// authorized it — a build that cannot name the agent vouching that a phone
    /// can render it. Both sides of that comparison are values this build does
    /// not understand, and agreeing about a name is not the same as being able
    /// to act on it.
    pub fn supports(&self, agent: &crate::agent::AgentKind) -> bool {
        if matches!(agent, crate::agent::AgentKind::Unsupported(_)) {
            return false;
        }
        if agent.is_claude() && self.agents.is_empty() {
            return true;
        }
        self.agents.contains(agent)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Must be the first message. The connection is closed if anything else
    /// arrives first, or if it does not arrive within the handshake window.
    ///
    /// Exactly one credential is expected. `token` is the steady state (the
    /// static bearer token, or a per-device token from a previous pairing);
    /// `pairing_code` is the one-shot alternative that *buys* a device token.
    /// A hello carrying neither is refused, and one carrying both prefers the
    /// token — re-pairing an already-paired device would mint a second
    /// credential for the same phone and leave the first one dangling.
    Hello {
        protocol_version: u32,
        /// Optional: a pairing hello has no token yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// Single-use, 5-minute pairing code from the QR.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pairing_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_name: Option<String>,
        /// What this client can understand (its agent set today). Absent from a
        /// client predating the agent seam, which is Claude-only.
        ///
        /// **Wire-legal and inert today, on the same terms as
        /// [`ClientMessage::RegisterPush::features`]: the daemon reads it off the
        /// wire and drops it.** It scopes no request on this connection — the
        /// daemon holds no connection-scoped feature state at all — because a
        /// scoping path with no input is machinery rather than a feature, and no
        /// shipping client encodes this field. It rides `hello` because it is a
        /// property of the whole connection rather than of one request, which is
        /// what the phase that lands the scoping will want.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        features: Option<ClientFeatures>,
    },
    Sessions,
    /// `session_id` is a **session reference**: either a `session_uid` (exact,
    /// preferred, `protocol_minor >= 2`) or a tmux name like `cc-1` (legacy,
    /// which resolves to the newest run under that name). The field keeps its
    /// original name because widening what a field accepts is additive;
    /// renaming it would not be.
    Subscribe {
        session_id: String,
        #[serde(default)]
        after_seq: u64,
    },
    Unsubscribe {
        session_id: String,
    },
    /// Remove one run's record from the Mac.
    ///
    /// Deliberately the only destructive verb the phone has, and deliberately
    /// narrow: `session_uid`, never a tmux name, because a name is handed to the
    /// next run and a phone that deleted "cc-1" could destroy a session it never
    /// saw. A **hosted** run is refused unless proven `exited`; an **adopted**
    /// one (empty `tmux_session` — CodeConnect never launched it, so no proof of
    /// its end can ever exist) is accepted at any lifecycle, and its deletion
    /// also stops observation of that conversation until a new session start
    /// re-adopts it.
    ///
    /// It removes CodeConnect's own record, not the conversation: Claude Code
    /// keeps its transcript under `~/.claude/projects`, so `claude --resume` still
    /// works afterwards. The phone's copy says so.
    DeleteSession {
        session_uid: String,
    },
    /// "Prove the doorbell rings." Sends one real APNs notification to **this
    /// connection's own device**, so the whole chain — stored token, provider
    /// key, Apple, banner — is demonstrated rather than implied.
    ///
    /// Correlated by `request_id` because two sheets on two phones may test at
    /// once, and gated on the `test_push` capability: a minor-6 daemon can
    /// advertise `push` without understanding this message.
    TestPush {
        request_id: String,
    },
    /// Idempotent, leased answer. `payload_hash` must match the text the phone
    /// displayed or the answer is refused.
    Answer {
        request_id: String,
        payload_hash: String,
        decision: AnswerDecision,
        /// Which run this answer belongs to, as a uid or a name. Optional so an
        /// older client keeps working; when present the daemon will not apply
        /// the answer to a *different* run that happens to be showing a card
        /// with the same `request_id`, which is the failure the session uid was
        /// introduced to make impossible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Free-text takeover: types at the Mac's TTY, gated on composer presence.
    ///
    /// A mutating command, so it carries an identity like every other one
    /// (`request_id` + `payload_hash`, minor 3). A retry with the same pair is
    /// a no-op that replays the original outcome instead of typing twice; a
    /// retry with the same id but different text is refused rather than
    /// silently treated as the same mutation.
    ///
    /// Both fields are optional so a client below minor 3 keeps working. Such a
    /// request is applied at-most-once *within one attempt* but cannot be made
    /// idempotent across retries — there is nothing to recognise it by — and
    /// the daemon says so in the log rather than pretending otherwise.
    SendText {
        session_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// [`crate::hash::send_text_hash`] over `session_id`, `text` and
        /// `submit`. Required whenever `request_id` is present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload_hash: Option<String>,
        /// **Ignored since minor 3.** Kept so a client below minor 3 still
        /// decodes, but the interlock is chosen by the server: letting the
        /// caller nominate the needle that authorises its own keystrokes made
        /// the safety check a formality the client could write itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        require: Option<PromptPresence>,
        #[serde(default = "default_true")]
        submit: bool,
        /// The client saw this command's consequences stated and the human
        /// tapped anyway, so the daemon may **complete** the confirmation
        /// Claude Code opens rather than dismissing it. Minor 10.
        ///
        /// Default false, and it is a *permission*, not an instruction: the
        /// daemon still intersects it with its own allowlist and its
        /// supervisor gate, so a client cannot nominate a command. It exists
        /// because a client that predates the disclosure must not trigger a
        /// committing keystroke it never told anybody about.
        #[serde(default)]
        complete_native_confirmation: bool,
    },
    Capture {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
    },
    /// "What has this agent actually changed?" — answered from git, not from
    /// the event log, because the log knows about edits that were later undone
    /// and the working tree is the only thing that knows the net result.
    GetDiff {
        session_id: String,
    },
    /// "Which slash commands does this session's Claude Code actually have?"
    ///
    /// Answered from the installed binary's own machine-readable init message,
    /// never from a hand-maintained list — the phone uses it to label commands
    /// honestly and to refuse, client-side, the picker-shaped ones that would
    /// open a dialog on the Mac's screen and lock the composer. Session-scoped
    /// because the binary is: each run registered the executable it was
    /// launched with, and two sessions may straddle an upgrade.
    GetCommandCatalog {
        session_id: String,
    },
    /// "Push me here." Sent after the phone has been granted notification
    /// permission and Apple has issued a token.
    ///
    /// Separate from `hello` on purpose: permission can be granted, revoked or
    /// re-granted at any point in a session's life, and the token itself is
    /// reissued on reinstall and on restore-from-backup. Tying it to the
    /// handshake would mean a phone that gained permission mid-session could
    /// not say so until it reconnected.
    RegisterPush {
        /// APNs device token, lowercase hex.
        token: String,
        /// `sandbox` or `production`. A development build's token is not valid
        /// on the production host and the failure looks like a corrupt token,
        /// so the phone — which is the only party that knows how it was signed
        /// — says which.
        #[serde(default)]
        environment: Option<String>,
        /// The opaque bearer the relay issued for this `(token, environment)`,
        /// for a daemon advertising `push_relay`. Absent from a direct-key
        /// registration, which needs no third party.
        ///
        /// It travels with the token rather than in `hello` because it is
        /// *about* the token: a reissued token earns a fresh credential, and a
        /// daemon that mixed a new token with an old bearer would present a
        /// pair the relay has no binding for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_credential: Option<crate::secret::Redacted>,
        /// What this device can be notified about (its agent set).
        ///
        /// **Wire-legal and inert today: the daemon accepts this field and stores
        /// nothing.** The write side was deleted rather than kept working, because
        /// nothing that ships can fill the field — no client encodes it — and a
        /// write path with no input is machinery, not a feature. So every device
        /// row reads `NULL`, which is the Claude floor, and a device hears about a
        /// non-Claude agent only once a later phase lands both the phone that
        /// advertises and the persistence that records it.
        ///
        /// It rides `register_push` rather than `hello` because push eligibility is
        /// a property of the *device*, projected per device at dequeue time, not of
        /// the live connection — which is what the read side already assumes and
        /// what the write side will land against.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        features: Option<ClientFeatures>,
    },

    /// Open a live terminal on the session named by `session_uid`. `cols`/
    /// `rows` are this device's viewport; `output_credit` is how many decoded
    /// bytes the daemon may send before the first [`ClientMessage::TerminalCredit`].
    /// The daemon answers [`ServerMessage::TerminalAttached`] or
    /// [`ServerMessage::TerminalClosed`]; it never streams a byte before the
    /// attach is verified. `attachment_id` is the client's handle for this
    /// stream and scopes every later terminal message.
    TerminalAttach {
        attachment_id: String,
        session_uid: String,
        cols: u16,
        rows: u16,
        output_credit: u32,
    },
    /// Keys for the pane, base64. Consumes the input credit the daemon
    /// granted; the daemon replenishes it only once it has handed the bytes to
    /// the tmux client, so credit reflects input the client has taken, not
    /// input still queued in the daemon.
    TerminalInput {
        attachment_id: String,
        /// base64 of the raw bytes.
        data: String,
    },
    /// This device's viewport changed. The daemon applies it to its own
    /// disposable client only; a human at the Mac is never resized by it.
    TerminalResize {
        attachment_id: String,
        cols: u16,
        rows: u16,
    },
    /// The phone has consumed `bytes` of output and grants that much more.
    TerminalCredit {
        attachment_id: String,
        bytes: u32,
    },
    /// Close this attachment. The tmux session and the agent are untouched;
    /// only the daemon's disposable viewing client goes.
    TerminalDetach {
        attachment_id: String,
    },

    /// Abort the running turn named by `turn_id` on a Codex session.
    ///
    /// A mutating operation, so it carries a ledger identity like every other:
    /// `request_id` makes a retry idempotent and `payload_hash` binds it to the
    /// exact turn it was issued against, so a replay can never abort a *different*
    /// turn the session has since moved on to.
    ///
    /// **Honoured**: the daemon genuinely aborts the turn a Codex session is
    /// running, and answers with a typed [`ServerMessage::InterruptResult`]
    /// whatever becomes of it. A Claude client never sends it — Claude's stop
    /// control is the keyboard at the Mac, and a Claude session is refused with
    /// the sentence that says so rather than by a bare no.
    ///
    /// Two gates stand between this message and a stopped turn, and they bind
    /// different things, which is why neither is redundant. The daemon's own gate
    /// binds the ask to the exact `turn_id` the hash names, to the thread its
    /// control link is *subscribed* to, and to that link's current visit
    /// generation — so the same `request_id` carrying a different turn, thread or
    /// visit is a conflict rather than a replay, and can never become a stop aimed
    /// at whatever is running now. It is also the only gate that can be checked
    /// before anything durable is claimed. The broker then binds the ask again, on
    /// its own side, to the session's *own* active turn, and refuses one that does
    /// not name it — and that is the only check with sight of the turn the session
    /// is actually running. Interrupt is bound to identity at both ends, never to
    /// a name.
    Interrupt {
        session_id: String,
        request_id: String,
        /// The turn to abort, as it was named when the card was shown.
        turn_id: String,
        /// [`crate::hash::interrupt_hash`] over `session_id`, `turn_id`.
        payload_hash: String,
    },

    Ping,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    HelloAck {
        protocol_version: u32,
        /// Additive feature level (see `protocol::PROTOCOL_MINOR`). Defaulted on
        /// decode so an ack from a daemon predating the field parses as minor 0.
        #[serde(default)]
        protocol_minor: u32,
        server_time: String,
        capabilities: Capabilities,
        /// Present exactly once, in the ack to a successful `pairing_code`
        /// hello. This is the only time the token crosses the wire; the phone
        /// stores it in the Keychain and never asks for it again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        /// The name this device is listed under by `codeconnect devices` and revoked by
        /// with `codeconnect revoke <name>`. May differ from the requested `client_name`
        /// when that name was already taken.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
        /// The APNs environment the daemon currently holds for **this device's**
        /// registered token, absent when it has none.
        ///
        /// The daemon is downstream of the authority here: a relay binding
        /// decides which host a token lives at and corrects the daemon on an
        /// accepted send. A phone that resends the environment it first cached
        /// would undo that correction on every handshake, so it compares this
        /// against its own copy and persists the difference instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        push_environment: Option<String>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Event {
        event: Event,
    },
    AnswerResult {
        request_id: String,
        result: AnswerResult,
    },
    /// The outcome of a `delete_session`. Typed rather than a bare ack: "there was
    /// nothing there" and "it is still running, so no" are different answers and
    /// the phone shows different things.
    DeleteSessionResult {
        session_uid: String,
        result: DeleteSessionResult,
    },
    TestPushResult {
        request_id: String,
        result: TestPushResult,
    },
    SendTextResult {
        session_id: String,
        result: SendTextResult,
    },
    CaptureResult {
        session_id: String,
        text: String,
    },
    CommandCatalog {
        session_id: String,
        result: CommandCatalogResult,
    },
    Diff {
        session_id: String,
        /// `git diff HEAD` plus a listing of untracked files. Empty when the
        /// tree is clean *or* when there is no git repository — `note` is what
        /// tells those two apart, so the phone never renders "no changes" for
        /// a directory that was never under version control.
        unified: String,
        /// True when the diff hit the size cap. The phone must say so rather
        /// than render a silently incomplete diff.
        truncated: bool,
        captured_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },

    /// The attach is verified and live. `input_credit` is how many decoded
    /// bytes the phone may send before the first [`ServerMessage::TerminalCredit`].
    /// No `terminal_output` precedes this.
    ///
    /// The two ceilings ride the ack for the same reason the credits do: a
    /// client that hard-codes them enforces this daemon's numbers for the life
    /// of the install, and raising either one later would kill every phone's
    /// terminal mid-session. Required, not defaulted — this message is minor
    /// 13, which has never shipped, so there is no older daemon to omit them.
    TerminalAttached {
        attachment_id: String,
        input_credit: u32,
        /// [`MAX_TERMINAL_CHUNK_BYTES`]: the largest decoded payload either
        /// side may put on one `terminal_input`/`terminal_output`.
        max_chunk_bytes: u32,
        /// [`TERMINAL_MAX_OUTSTANDING_CREDIT`]: the most credit that may be
        /// outstanding in one direction.
        max_outstanding_credit: u32,
    },
    /// Pane bytes, base64. Consumes the output credit the phone granted; the
    /// phone replenishes after `TerminalView.feed` returns.
    TerminalOutput {
        attachment_id: String,
        data: String,
    },
    /// The daemon has handed `bytes` of input to the tmux client and grants
    /// that much more input credit.
    TerminalCredit {
        attachment_id: String,
        bytes: u32,
    },
    /// The attachment ended. `code` is one of [`terminal_close`]; `reason` is
    /// human text the phone may show verbatim. Terminal for this
    /// `attachment_id`; a new terminal is a fresh `terminal_attach`.
    TerminalClosed {
        attachment_id: String,
        code: String,
        reason: String,
    },

    /// The outcome of an [`ClientMessage::Interrupt`]. Typed like every other
    /// mutation result so a retry replays a recorded outcome rather than
    /// aborting twice.
    ///
    /// **Every status is reachable**, and the four are genuinely different news
    /// a client has to render apart: the turn reached its aborted boundary
    /// ([`InterruptResult::Aborted`]); this exact ask already did that and is not
    /// doing it twice ([`InterruptResult::Duplicate`]); nothing was actuated and
    /// here is why ([`InterruptResult::Rejected`]); or the stop was issued and
    /// this daemon did not live to see what it did
    /// ([`InterruptResult::Indeterminate`]). Collapsing them — treating anything
    /// that is not `aborted` as a failure, or anything that is not `rejected` as a
    /// success — tells the operator something untrue about their own session.
    InterruptResult {
        session_id: String,
        request_id: String,
        result: InterruptResult,
    },

    Pong,
}

/// What became of an [`ClientMessage::Interrupt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InterruptResult {
    /// The turn reached its aborted boundary.
    Aborted { turn_id: String },
    /// This exact interrupt already ran; the turn was not aborted a second time.
    Duplicate { turn_id: String },
    /// Refused, with a reason: **nothing was actuated by this ask, and the record
    /// says so.** The clean complement of [`Self::Indeterminate`] — that one is an
    /// outcome nobody can name, because the ask went past the point where it was
    /// committed and what it did was never observed; this one is named precisely
    /// because the ask never reached that point. A client may say the turn is
    /// untouched by this ask, which no other status licenses.
    ///
    /// The reason is human text, safe to show verbatim: the run is not a Codex
    /// session (a Claude one is told where its stop control actually is), the
    /// control link is not watching the turn's thread so no stop could be
    /// confirmed, the hash does not bind this ask to the turn it names, or a
    /// settled claim that stopped nothing is being replayed.
    Rejected { reason: String },
    /// Issued, outcome unknown — the daemon was killed between claiming the
    /// interrupt and observing the turn terminate. Never retried automatically.
    Indeterminate { reason: String },
}

/// Ceiling on a `diff` payload. A phone screen cannot use more, and an
/// unbounded diff would blow past the client message limit on the way out.
pub const MAX_DIFF_BYTES: usize = 512 * 1024;

/// What CodeConnect can actually do, reported honestly rather than assumed.
/// Mirrors ACP's capability shape; the iPhone disables affordances it does not
/// see advertised instead of failing at tap time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// True when an answer is guaranteed to reach the agent.
    pub can_approve_reliably: bool,
    /// `fail_open` | `fail_closed`. CodeConnect is fail-open by construction:
    /// when the daemon is down the hook emits nothing and the session behaves
    /// exactly like plain `claude`.
    pub fail_mode: String,
    /// How an answer reaches the agent on this build.
    pub answer_path: AnswerPath,
    /// Seconds the daemon will hold a gate hook open. 0 = never hold.
    pub hold_secs: u64,
    pub send_text: bool,
    pub capture: bool,
    /// The daemon accepts `delete_session`. Advertised rather than assumed so an
    /// older Mac does not get a swipe that silently does nothing: this app's rule
    /// is that an action it cannot perform is not offered.
    #[serde(default)]
    pub delete_session: bool,
    /// `test_push` is answerable. Distinct from `push` — a minor-6 daemon can
    /// send pushes without understanding the test request.
    #[serde(default)]
    pub test_push: bool,
    /// **Direct** APNs, wired to a real key on this Mac. False while the sender
    /// is the logging stub — and false in relay mode, which is not an omission:
    /// a client predating `push_relay` reads only this flag, and a true here
    /// would send it to collect a token it has no credential for.
    pub push: bool,
    /// The daemon sends through the CodeConnect push relay, so a registration
    /// must carry `relay_credential` as well as the token. One-hot with `push`;
    /// a client seeing both true takes `push` and registers directly.
    #[serde(default)]
    pub push_relay: bool,
    /// The listener holds a `tailscale cert` and accepts `wss://`. Says nothing
    /// about *this* connection — see `tls_active`.
    pub tls: bool,
    /// Whether this particular connection is encrypted. The daemon accepts both
    /// schemes on one port during the migration, so "the server can do TLS" and
    /// "you are using it" are genuinely different facts and the phone is
    /// entitled to both.
    #[serde(default)]
    pub tls_active: bool,
    /// `get_diff` is answerable.
    #[serde(default)]
    pub diff: bool,
    /// Approval cards carry a daemon-computed `risk` block.
    #[serde(default)]
    pub risk_class: bool,
    /// Sessions and events carry a `session_uid`, and every message that names
    /// a session accepts one. A client that sees this false must keep treating
    /// `session_id` as the identity — and accept that a reused name will splice
    /// two runs together, which is what this flag being true fixes.
    #[serde(default)]
    pub session_uid: bool,
    /// `send_text` accepts `request_id` + `payload_hash` and is idempotent
    /// across retries. False means a retried takeover types twice, so a client
    /// must not retry one it did not see answered.
    #[serde(default)]
    pub send_text_idempotent: bool,
    /// `send_text` runs the composer-recovery postcondition for slash
    /// commands, and can answer `composer_recovered` / `composer_lost`. A
    /// client that sees this false must not offer the snapshot commands:
    /// their whole safety story is that the daemon closes the view again.
    #[serde(default)]
    pub slash_composer_recovery: bool,
    /// Approval cards carry a `generation` and an `identity_bound` flag, and
    /// the daemon refuses to answer a card whose prompt it cannot prove is
    /// still on screen. A client that sees this false must treat every remote
    /// approval as best-effort.
    #[serde(default)]
    pub prompt_identity: bool,
    /// `get_command_catalog` is answerable. False means the phone has no way
    /// to know which slash commands the Mac's Claude Code has, and must treat
    /// typed built-ins by its own static policy alone.
    #[serde(default)]
    pub command_catalog: bool,
    /// The daemon can stream a live terminal (`terminal_attach` …) over this
    /// connection. False means no terminal carrier — the phone offers no
    /// Terminal tab and says to update the Mac. **Connection-scoped, not a
    /// build fact:** a live terminal is shell-equivalent authority, so this is
    /// false for the static bootstrap token — only a paired device may open
    /// one. Transport is not this flag's concern: a connection whose wire
    /// could be read in transit is refused at admission, terminal or not.
    /// See `terminal_close::NOT_AUTHORISED`.
    #[serde(default)]
    pub terminal_pty: bool,
    /// **This daemon honours a stop for a Codex session whose control link is
    /// `Subscribed`** — it really aborts the turn and reports what became of it,
    /// rather than answering [`InterruptResult::Rejected`] to every ask. Nothing
    /// else on the wire tells those two daemons apart — both accept
    /// [`ClientMessage::Interrupt`] and both answer an `interrupt_result` — so
    /// without this flag a phone shipping a Stop button would have to tap one to find
    /// out, which is exactly the "offered and silently broken" affordance this app's
    /// rule forbids: an action the daemon cannot perform is not offered.
    ///
    /// # What it does NOT say, stated precisely because it was over-read
    ///
    /// It is **connection-global and build-shaped**: one capability set is composed
    /// per `hello_ack`, from this daemon's supported agents and nothing else. It is
    /// therefore true on a connection whose fleet is entirely Claude, and true for a
    /// Codex session whose control link is offline, unbound or merely bound — every
    /// one of which refuses an ask with a sentence of its own. The name carries
    /// `codex_` for that reason: a bare `interrupt` read as a promise about whatever
    /// session the reader had in mind, which is the one thing this flag never was.
    ///
    /// **Per-session actuatability is `summary.agent` plus the session's link
    /// state.** The agent is on every [`crate::event::SessionSummary`]; the link state
    /// is not — the summary carries `codex_thread_id`, which the daemon resolves from
    /// the addressee and which reads the same whether that link is subscribed,
    /// bound or reconnecting. So a client that wants to know whether THIS session can
    /// be stopped right now cannot compute it from the fleet today, and the field that
    /// would let it is Phase 5's, not this flag's.
    ///
    /// Until then the honest client rule is: offer the button on a Codex session
    /// hosted by a daemon that advertises this, and let the refusal — which always
    /// names which of the conditions failed — be what the operator reads.
    #[serde(default)]
    pub codex_interrupt: bool,
    /// The agents this daemon can actually host, named honestly. A client scopes
    /// what it offers to this set and intersects it with its own
    /// [`ClientFeatures`]. Empty — from any daemon predating the agent seam — is
    /// read as `["claude"]`: Claude is the floor, and this list only ever *adds*
    /// to it. **Omitted entirely while it would only name Claude** — an empty
    /// list is skipped on the wire, so a daemon with nothing to add beyond the
    /// floor sends no field at all and an older phone renders nothing new. It is
    /// populated once the daemon can actually drive a second agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_agents: Vec<crate::agent::AgentKind>,
}

/// How the daemon applies an answer on the *installed* Claude Code build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerPath {
    /// Structured hook return — the agent never renders a prompt.
    HookReturn,
    /// Keystrokes into the live prompt, after positive presence confirmation.
    /// Physically identical to answering at the Mac's keyboard, which is what
    /// makes first-answer-wins a property of the TTY rather than of a protocol.
    SendKeys,
    /// The JSON-RPC response to the app-server's own `requestApproval`, written
    /// on the Codex link's socket.
    ///
    /// Neither of the two above can describe it, and the difference is not
    /// cosmetic: nothing is rendered and nothing is typed, so "first answer wins"
    /// is a property of the broker's arbiter rather than of a TTY, and the loser
    /// of that race is a fact this daemon is *told* rather than one it infers.
    /// Additive (minor 16): a Claude answer never uses it, so Claude's
    /// `AnswerPath` serialization is unchanged.
    CodexResponse,
}

/// The phone's answer, expressed in terms of what Claude is showing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnswerDecision {
    Allow,
    Deny,
    /// Pick the nth option exactly as rendered (1-based, as Claude numbers them).
    Option {
        index: u32,
    },
    /// Free-text takeover.
    Text {
        text: String,
    },
    /// Pick a server-offered option by its **opaque** id, for an agent whose
    /// options are not a 1-based list (Codex's `availableDecisions`). The daemon
    /// validates it against the exact option set it stored for the request and
    /// the payload hash over that set — the id is never interpreted here. Additive
    /// (minor 15): a Claude answer never uses it, so Claude's `AnswerDecision`
    /// serialization is unchanged.
    OptionId {
        option_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    Phone,
    /// Answered at the Mac's keyboard. Detected, not reported: the daemon sees
    /// the prompt leave the pane without having typed into it, or sees the tool
    /// run. The phone should render "answered at the keyboard", never a
    /// rejection — which is the entire point of distinguishing this from
    /// `Timeout`. Best-effort by construction; check `AnswerOutcome::inferred`
    /// before presenting `decision` as fact.
    Local,
    Timeout,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerOutcome {
    pub request_id: String,
    pub session_id: String,
    pub decision: AnswerDecision,
    pub resolved_by: ResolvedBy,
    pub applied_via: AnswerPath,
    pub resolved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// True when `decision` is the daemon's *inference* rather than an observed
    /// answer — the local-resolution path, where all we truly know is that the
    /// prompt is gone. Recording a guess without labelling it as one would make
    /// the ledger lie, and the ledger is the thing a later duplicate tap is
    /// answered from.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inferred: bool,
    /// True when the daemon knows *what* was decided but not whether it landed:
    /// it was killed between claiming the answer and recording its outcome.
    ///
    /// Distinct from `inferred`, which is uncertainty about the decision. This
    /// is uncertainty about the *actuation*, and it is the reason such a request
    /// is never retried: a second injection into a live TTY cannot be taken
    /// back, while an unanswered prompt is still sitting in front of a human.
    #[serde(default, skip_serializing_if = "is_false")]
    pub indeterminate: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Who resolved a Codex approval, when that is known. Upstream carries no
/// provenance (A2: `serverRequest/resolved` is the same frame however it was
/// answered), so this is derived from the broker's own winner disposition, never
/// read off the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionActor {
    /// The phone's claim won.
    Phone,
    /// Answered at the Mac's keyboard — the TUI beat the phone, or the phone
    /// never claimed. Honest even when the specific decision is not known.
    Local,
}

/// Why a Codex approval was cleared without being answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearCause {
    /// A `turn/interrupt` retired the pending (A3).
    TurnAborted,
    /// The turn completed and took the pending with it.
    TurnCompleted,
    /// **A thread switch retired the old visit's pending — and no measured
    /// crossing produces one.**
    ///
    /// The sweep behind this is real and runs whenever the visit generation
    /// moves: a card filed under an earlier visit of the same thread is not the
    /// current visit's to hold, so it is cleared rather than left standing. What
    /// has no producer is the case this cause was written for — a switch
    /// admitted while an approval is still unresolved. Every wire position that
    /// could admit one was driven on a real 0.153.2 session, and none does: with
    /// a prompt showing, an interrupt is consumed by the prompt as its own
    /// decline, so the approval reaches a terminal of its own before any new
    /// thread is born; a resume only subscribes and never moves the head; and
    /// the only frame that does move it is refused while a turn or an approval
    /// is live.
    ///
    /// So this variant is kept as the honest handler for a crossing the wire has
    /// not been shown to produce, not deleted and not given an invented
    /// producer. It stays on the wire because the enum is decoded by clients
    /// that must not meet an unknown value, and it costs nothing to leave a
    /// truthful word ready for a release that starts admitting the crossing.
    Superseded,
    /// **The item the card was about finished, and no answer to it was ever
    /// observed.**
    ///
    /// The retirement a link that missed the answer still sees. `serverRequest/
    /// resolved` is broadcast once and never replayed, so a link that dropped
    /// between the request and its resolution comes back to a card whose
    /// question has already been settled at the keyboard — and the only frame
    /// left that says so is the item's own `item/completed`, which a resumed
    /// link does receive.
    ///
    /// **Deliberately not [`ClearCause::TurnCompleted`].** The turn is still
    /// running when this fires — measured on 0.147 in
    /// `fixtures/codex/file-change.jsonl`, where the approved `fileChange`
    /// item completes at line 22 and its turn does not terminalize until line
    /// 43, twenty-one frames later. Reporting a live turn as completed would
    /// be a false statement about the run, made to reuse a word; the item
    /// finishing is what actually happened and is what this says.
    ItemCompleted,
}

/// How far a phone claim got before delivery became uncertain. Present only on
/// [`CodexResolution::Unknown`] (D3): a claim recorded but not provably actuated
/// is terminal evidence, never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteStage {
    /// Claimed durably, never enqueued upstream.
    ClaimedNotEnqueued,
    /// The broker accepted it at ingress; upstream acceptance unproven.
    BrokerIngressAccepted,
    /// Written toward the app-server; the write itself is unconfirmed.
    UpstreamWriteUnconfirmed,
}

/// The terminal outcome of a Codex approval.
///
/// A **separate** discriminated type from [`AnswerOutcome`] on purpose: Codex's
/// resolution taxonomy (four terminals, upstream-provenance-free) does not fit
/// Claude's `decision + resolved_by + applied_via` shape, and forcing it in
/// would have changed Claude's serialization. Claude's `AnswerOutcome` and
/// `AnswerResult` are left byte-identical; this rides its own event. The
/// `decision`, when present, may be an opaque [`AnswerDecision::OptionId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CodexResolution {
    /// Answered. `decision` is absent when only the winner is known and not the
    /// choice (upstream resolution has no provenance, so a keyboard answer often
    /// arrives as `answered{by: local}` with no decision).
    Answered {
        by: ResolutionActor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<AnswerDecision>,
    },
    /// Cleared without an answer.
    Cleared { cause: ClearCause },
    /// No answer arrived in time.
    Timeout,
    /// A phone claim whose delivery could not be proven. Terminal, never retried.
    Unknown {
        attempted_by: ResolutionActor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempted_decision: Option<AnswerDecision>,
        write_stage: WriteStage,
        /// Human-readable cause of the uncertainty, for the log and the card.
        cause: String,
    },
}

/// What the daemon knows about the installed Claude Code's slash commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandCatalogResult {
    /// The binary's own inventory, read from its init message. Names come
    /// without the leading slash, exactly as emitted — the daemon adds and
    /// invents nothing.
    Available {
        commands: Vec<String>,
        /// The version the init message reported, when it did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_version: Option<String>,
        /// When this list was actually read from the binary. A cache hit
        /// reports the original probe time, not the request time — the age of
        /// a fact is part of the fact.
        probed_at: String,
    },
    /// No list. A complete answer, not an error: the phone must fall back to
    /// treating built-ins conservatively, never to guessing.
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AnswerResult {
    Applied {
        outcome: AnswerOutcome,
    },
    /// The request was already resolved. Returns the *original* outcome, which
    /// is the whole point of the ledger: a retried tap can never double-apply.
    Duplicate {
        outcome: AnswerOutcome,
        /// True when the retry also carried a hash that no longer matches —
        /// worth surfacing on the phone as "this card was out of date".
        stale_payload_hash: bool,
    },
    Rejected {
        reason: String,
    },
}

/// What became of a `delete_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeleteSessionResult {
    /// Gone, with what went. Reported so the phone can say what it removed rather
    /// than assume.
    Deleted { events: u64 },
    /// The run is not exited. Refusing is the store's own rule, enforced again
    /// here so the answer is a sentence rather than a silent no-op.
    StillRunning,
    /// Refused, but the daemon cannot say the run is alive either.
    ///
    /// **Separate from `still_running` because it is a different claim.** That
    /// one asserts the agent is there. This one is the absence of a claim: the
    /// daemon never established what happened to this run, and
    /// [`event::Lifecycle::Unknown`] is exactly the state `prune_exited_sessions`
    /// treats as the strongest reason of all not to delete — the record is the
    /// only evidence left. Reporting it as "still running" would invent a fact
    /// in the one place the daemon has none.
    NotExited { lifecycle: String },
    /// No such run. Not an error: two phones deleting the same row is a race the
    /// user does not need to hear about.
    NotFound,
    /// The daemon tried and could not — a database error, and nothing else.
    ///
    /// Carried here rather than sent as a bare `error` frame because an `error`
    /// names no session: a phone with a delete in flight cannot tell whether one
    /// belongs to it, so it waits out its timeout with a spinner up and then fails
    /// silently. Every request this daemon accepts gets an answer to *that*
    /// request.
    Failed { message: String },
}

/// What became of a `test_push`. Every refusal is a distinct fact the phone
/// renders differently, and `accepted` says exactly what Apple's 200 proves:
/// the notification was **accepted for delivery** — display is the device's
/// half, which is why the phone shows the banner as the final word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TestPushResult {
    Accepted {
        /// Apple's own `apns-id` response header, when it sent one — the
        /// receipt a reader can take to Apple's delivery logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        apns_id: Option<String>,
    },
    /// Push mode is `Off`, so the sender is the logging stub — push was
    /// disabled, a requested direct configuration was partial or its key
    /// unreadable, or the relay HTTPS client could not be constructed. (No APNs
    /// key on its own selects relay mode, not this.)
    PushUnconfigured,
    /// This connection authenticated with the static bootstrap token, so there
    /// is no device row — and no token — to send to.
    NotPairedDevice,
    /// No complete push registration: either no token is stored (notifications
    /// were never enabled, or registration has not completed yet), or relay mode
    /// found a stored token without its relay credential — an incomplete tuple,
    /// so nothing was sent and nothing was refused.
    NoRegisteredToken,
    /// The relay refused the credential registered with this token.
    ///
    /// **Not proof the token is dead — so a client must not clear the token on
    /// this alone.** What the relay rejected is the bearer, not the APNs
    /// registration: the bearer may be unknown, rotated or revoked, below the
    /// generation floor, or outside the active attestation namespace (active
    /// bearers do not expire on a clock). But it is not proof the token is
    /// *alive* either — the same answer comes back for a bearer bound to a
    /// different token, and for a binding the relay already retired on an APNs
    /// `410`. A client that treated this as `no_registered_token` would discard a
    /// token that may still be good; the repair is to renew the bearer and
    /// register again, letting normal status recheck the token.
    CredentialInvalid,
    RateLimited {
        retry_after_secs: u32,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SendTextResult {
    Sent {
        matched: String,
    },
    Refused {
        reason: String,
    },
    /// This exact mutation already ran; nothing was typed a second time.
    ///
    /// Only ever sent in reply to a request that carried a `request_id`, so a
    /// client below minor 3 — which cannot send one — can never receive a
    /// status it does not know how to decode.
    Duplicate {
        matched: String,
        applied_at: String,
    },
    /// It ran, or it did not, and the daemon cannot tell which.
    ///
    /// The one honest answer when the daemon was killed between claiming the
    /// mutation and recording its outcome. It is never retried automatically:
    /// typing a second time into a live TTY is not a recoverable mistake, and a
    /// human who can see the screen is a better judge than a guess.
    Indeterminate {
        reason: String,
    },
    /// The keys landed, Claude's composer disappeared, and the daemon's own
    /// `Escape` brought it back. Terminal like `Sent`: the mutation
    /// happened.
    ///
    /// Minor 9, and **sent to every client** — `hello` carries no client
    /// minor for the daemon to branch on. That is safe because a status a
    /// client does not know decodes as `indeterminate` ("typed, outcome
    /// unknown, a retry is recognised") rather than as a decode failure;
    /// this app's own decoder does exactly that, and any other client should.
    ComposerRecovered {
        matched: String,
        /// The Mac's visible pane while the view was up — present only for
        /// the snapshot commands (`/status`, `/usage`, `/cost`), and never
        /// stored: it is a picture for a human to read once.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_snapshot: Option<String>,
        captured_at: String,
    },
    /// The keys landed, the composer disappeared, and one `Escape` did not
    /// bring it back. Somebody has to look at the Mac.
    ComposerLost {
        matched: String,
    },
}

/// Payload of an `approval_request` event: everything the phone needs to render
/// a card, plus the hash it must echo back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCard {
    pub request_id: String,
    pub payload_hash: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    /// The exact text hashed into `payload_hash`.
    pub display_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Daemon-computed severity hint, so the phone can lead with the right
    /// affordance before the human has read the command. A hint for a person,
    /// never a gate — CodeConnect has no permission model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<crate::risk::RiskAssessment>,
    /// The prompt generation this card belongs to (minor 3).
    ///
    /// Per run, monotonic, one per structured permission request. A card whose
    /// generation is behind the session's current one is answering a prompt
    /// that is no longer on screen, and the daemon refuses it.
    #[serde(default)]
    pub generation: u64,
    /// True once the daemon has fingerprinted the prompt this card is for and
    /// can prove, at the moment of typing, that the same prompt is still up.
    ///
    /// False means the card is worth *showing* — a human should know an agent
    /// is waiting — but must be answered at the Mac. Defaulted false so a card
    /// from a daemon that had no such concept is never assumed to be bound.
    #[serde(default)]
    pub identity_bound: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flow-control numbers are a contract with a client this crate does
    /// not compile, so they are pinned as literals rather than compared to
    /// themselves.
    ///
    /// Every other reference to them in this workspace is symbolic — the
    /// daemon enforces the ceiling *against the constant* and the soak harness
    /// seeds its ledger *from the constant* — so both sides of every comparison
    /// move together and a changed value stays green. Measured: raising
    /// `TERMINAL_MAX_OUTSTANDING_CREDIT` to `999 * 1024` left all 659 Rust
    /// tests passing. The phone carries its own copy in `Wire.Terminal`, and a
    /// value that drifts from it is not a failing assertion but terminals dying
    /// mid-keystroke on a device this suite never builds.
    ///
    /// So the point of the literals is that they must be edited deliberately,
    /// in both places, by whoever changes the protocol.
    #[test]
    fn the_terminal_flow_control_constants_are_the_ones_the_phone_was_built_against() {
        assert_eq!(MAX_TERMINAL_CHUNK_BYTES, 16 * 1024);
        assert_eq!(TERMINAL_INITIAL_OUTPUT_CREDIT, 64 * 1024);
        assert_eq!(TERMINAL_INITIAL_INPUT_CREDIT, 32 * 1024);
        assert_eq!(TERMINAL_MAX_OUTSTANDING_CREDIT, 256 * 1024);
        assert_eq!(MAX_ATTACHMENT_ID_BYTES, 64);
        assert_eq!(TERMINAL_MIN_COLS, 2);
        assert_eq!(TERMINAL_MAX_COLS, 512);
        assert_eq!(TERMINAL_MIN_ROWS, 2);
        assert_eq!(TERMINAL_MAX_ROWS, 256);
    }

    /// Each refusal an attach can meet has a wire code of its own, so no client
    /// has to read the human `reason` to know which happened.
    ///
    /// This is the whole argument for the codes replacing the prose-matching
    /// module that used to live here: `attachment_limit` once meant both "the
    /// Mac is full" and "this session already has a terminal", and the only
    /// thing telling them apart was an unversioned English sentence the phone
    /// also displays verbatim.
    #[test]
    fn every_attach_refusal_has_a_code_of_its_own() {
        use terminal_close::*;
        let refusals = [
            SESSION_NOT_HOSTED,
            IDENTITY_MISMATCH,
            TMUX_UNAVAILABLE,
            ATTACHMENT_LIMIT,
            SESSION_BUSY,
            SUPERSEDED,
            NOT_AUTHORISED,
            PROTOCOL_ERROR,
        ];
        for (n, code) in refusals.iter().enumerate() {
            assert!(
                !refusals[..n].contains(code),
                "{code} is used for two different refusals"
            );
        }
        // Pinned as literals: the phone matches these strings by hand.
        assert_eq!(ATTACHMENT_LIMIT, "attachment_limit");
        assert_eq!(SESSION_BUSY, "session_busy");
        assert_eq!(SUPERSEDED, "superseded");
    }

    #[test]
    fn hello_round_trip() {
        let msg = ClientMessage::Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            token: Some("t".into()),
            pairing_code: None,
            client_id: None,
            client_name: Some("iPhone".into()),
            features: None,
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"type\":\"hello\""));
        assert!(!s.contains("client_id"));
        assert!(!s.contains("pairing_code"));
        // A hello that names no features is Claude-only, and omits the field.
        assert!(!s.contains("features"));
        let _: ClientMessage = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn a_hello_from_an_older_client_still_decodes() {
        // The token became optional once pairing arrived. A client built
        // against the original shape sends exactly this, and it must keep
        // working — additive-only means the old shape never stops being valid.
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"hello","protocol_version":1,"token":"deadbeef","client_name":"iPhone"}"#,
        )
        .unwrap();
        match msg {
            ClientMessage::Hello {
                token,
                pairing_code,
                ..
            } => {
                assert_eq!(token.as_deref(), Some("deadbeef"));
                assert_eq!(pairing_code, None);
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn a_pairing_hello_carries_no_token() {
        // The extra key stands in for any field this daemon does not know:
        // clients from other versions are decoded on the fields we recognise
        // and the rest is ignored, so an unknown key never fails a handshake.
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"hello","protocol_version":1,"pairing_code":"ABCD2345",
                "unknown_to_this_daemon":"ignored","client_name":"iPhone"}"#,
        )
        .unwrap();
        match msg {
            ClientMessage::Hello {
                token,
                pairing_code,
                client_name,
                ..
            } => {
                assert_eq!(token, None);
                assert_eq!(pairing_code.as_deref(), Some("ABCD2345"));
                assert_eq!(client_name.as_deref(), Some("iPhone"));
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn hello_ack_omits_pairing_fields_for_an_ordinary_connection() {
        let ack = ServerMessage::HelloAck {
            protocol_version: crate::PROTOCOL_VERSION,
            protocol_minor: crate::PROTOCOL_MINOR,
            server_time: "2026-07-31T10:00:00.000Z".into(),
            capabilities: capabilities_fixture(),
            device_token: None,
            device_id: None,
            device_name: None,
            push_environment: None,
        };
        let encoded = serde_json::to_string(&ack).unwrap();
        assert!(!encoded.contains("device_token"), "{encoded}");
        assert!(
            !encoded.contains("push_environment"),
            "a daemon holding no token for this device claims no environment: {encoded}"
        );
        assert!(
            encoded.contains(&format!("\"protocol_minor\":{}", crate::PROTOCOL_MINOR)),
            "{encoded}"
        );
    }

    #[test]
    fn the_minor_version_only_ever_goes_up() {
        // A `const` block, so a merge that reverts the constant fails to
        // *compile* rather than failing a test somebody might skip — and long
        // before it reaches a phone that quietly stops trusting `session_uid`.
        const _: () = assert!(crate::PROTOCOL_MINOR >= 2, "session uids are minor 2");
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 3,
            "idempotent send_text and prompt identity are minor 3"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 4,
            "the additive error codes and truncation payloads are minor 4"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 5,
            "reconciled liveness — `live` meaning proven rather than unrefuted — is minor 5"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 6,
            "`register_push` — the phone telling the daemon where to send a \
             notification — is minor 6"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 7,
            "`delete_session` — the phone's first and only destructive verb, and \
             the `delete_session` capability that gates it — is minor 7"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 8,
            "`get_command_catalog` — the phone asking which slash commands the \
             installed Claude Code actually has, and the `command_catalog` \
             capability that gates it — is minor 8"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 9,
            "composer recovery — the daemon closing a Mac view its own \
             injection opened, and the `slash_composer_recovery` capability \
             that gates it — is minor 9"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 10,
            "view confirmation — the daemon *completing* the confirmation its \
             own injection opened, for `/model` and `/effort` with an argument, \
             rather than dismissing it — is minor 10"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 11,
            "`project_label` — naming the project rather than the reused `cc-<n>` \
             counter — is minor 11"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 12,
            "`SendText.respond_by_monotonic_ms` — the daemon's answer deadline riding \
             in the request so the supervisor can budget against it — is minor 12"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 13,
            "the live terminal — `terminal_attach` … over the paired connection, gated \
             by the `terminal_pty` capability — is minor 13"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 14,
            "relay-backed push — the `push_relay` capability, \
             `register_push.relay_credential`, the `credential_invalid` test result \
             and `hello_ack.push_environment` — is minor 14"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 15,
            "the agent seam — `AgentKind`, `Capabilities.supported_agents`, the \
             `hello`/`register_push` client feature set, `SessionSummary.agent`, the \
             `CodexResolution` envelope, `AnswerDecision::OptionId`, the `interrupt` \
             operation and the composite-id codec — is minor 15"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 16,
            "`AnswerPath::CodexResponse` — the phone answering a Codex approval by \
             writing the app-server's own response, which is neither a hook return nor \
             a keystroke — is minor 16"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 17,
            "the honoured interrupt — the daemon actually aborting the turn a Codex \
             session is running rather than refusing every ask, every `InterruptResult` \
             status therefore reachable, and the `codex_interrupt` capability that \
             says so — is minor 17"
        );
        const _: () = assert!(crate::PROTOCOL_VERSION == 1, "no breaking change was made");
        // The equality is the point: every bump has to come here and say what it
        // added, so the list above stays a record rather than a guess.
        assert_eq!(crate::PROTOCOL_MINOR, 17);
    }

    /// **The tags, pinned on this side too.**
    ///
    /// Every one of these strings is matched by hand in Swift
    /// (`WireTypes.swift`), and nothing here asserted them, so renaming a serde
    /// variant would compile, pass, ship, and leave the phone decoding a status
    /// it has never seen. `unknown` is inert by design, so the failure would be
    /// a silent no-op on a destructive action rather than a crash.
    #[test]
    fn the_delete_wire_shape_is_exactly_what_the_phone_matches_on() {
        let request = serde_json::to_value(ClientMessage::DeleteSession {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
        })
        .unwrap();
        assert_eq!(request["type"], "delete_session");
        assert_eq!(request["session_uid"], "01K1B3XQ8ZC0DE5FGH7JKMNPQR");
        assert!(
            request.get("session_id").is_none(),
            "a tmux name is handed to the next run and must never carry a delete"
        );

        for (result, expected) in [
            (
                DeleteSessionResult::Deleted { events: 5 },
                serde_json::json!({"status": "deleted", "events": 5}),
            ),
            (
                DeleteSessionResult::StillRunning,
                serde_json::json!({"status": "still_running"}),
            ),
            (
                DeleteSessionResult::NotFound,
                serde_json::json!({"status": "not_found"}),
            ),
            (
                DeleteSessionResult::Failed {
                    message: "disk".into(),
                },
                serde_json::json!({"status": "failed", "message": "disk"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&result).unwrap(), expected);
        }

        let reply = serde_json::to_value(ServerMessage::DeleteSessionResult {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            result: DeleteSessionResult::Deleted { events: 5 },
        })
        .unwrap();
        assert_eq!(reply["type"], "delete_session_result");
        assert_eq!(reply["session_uid"], "01K1B3XQ8ZC0DE5FGH7JKMNPQR");
        assert_eq!(reply["result"]["status"], "deleted");
    }

    /// Same discipline for the push test: every status the daemon can answer
    /// with, pinned to the exact strings the phone matches on.
    #[test]
    fn the_test_push_wire_shape_is_exactly_what_the_phone_matches_on() {
        let request = serde_json::to_value(ClientMessage::TestPush {
            request_id: "tp-1".into(),
        })
        .unwrap();
        assert_eq!(request["type"], "test_push");
        assert_eq!(request["request_id"], "tp-1");

        for (result, expected) in [
            (
                TestPushResult::Accepted {
                    apns_id: Some("A1".into()),
                },
                serde_json::json!({"status": "accepted", "apns_id": "A1"}),
            ),
            (
                TestPushResult::Accepted { apns_id: None },
                serde_json::json!({"status": "accepted"}),
            ),
            (
                TestPushResult::PushUnconfigured,
                serde_json::json!({"status": "push_unconfigured"}),
            ),
            (
                TestPushResult::NotPairedDevice,
                serde_json::json!({"status": "not_paired_device"}),
            ),
            (
                TestPushResult::NoRegisteredToken,
                serde_json::json!({"status": "no_registered_token"}),
            ),
            (
                TestPushResult::CredentialInvalid,
                serde_json::json!({"status": "credential_invalid"}),
            ),
            (
                TestPushResult::RateLimited {
                    retry_after_secs: 12,
                },
                serde_json::json!({"status": "rate_limited", "retry_after_secs": 12}),
            ),
            (
                TestPushResult::Failed {
                    reason: "apns 500".into(),
                },
                serde_json::json!({"status": "failed", "reason": "apns 500"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&result).unwrap(), expected);
        }

        let reply = serde_json::to_value(ServerMessage::TestPushResult {
            request_id: "tp-1".into(),
            result: TestPushResult::Accepted { apns_id: None },
        })
        .unwrap();
        assert_eq!(reply["type"], "test_push_result");
        assert_eq!(reply["request_id"], "tp-1");
    }

    /// **Minor 14 is additive in both directions**, which is what the
    /// compatibility matrix in `docs/push-gateway.md` §5 turns on: a daemon that
    /// predates relay push says nothing about it and must read as "direct or
    /// nothing", and a registration carrying a credential must decode on a
    /// daemon that has no idea what one is.
    #[test]
    fn relay_push_is_absent_rather_than_false_on_an_older_peer() {
        // An ack from a direct-key daemon on minor 13: `push` is true and there
        // is no `push_relay` key at all. The phone must not read the silence as
        // an offer.
        let direct = r#"{"type":"hello_ack","protocol_version":1,"protocol_minor":13,
            "server_time":"t","capabilities":{"can_approve_reliably":true,
            "fail_mode":"fail_open","answer_path":"send_keys","hold_secs":0,
            "send_text":true,"capture":true,"push":true,"test_push":true,"tls":false}}"#;
        match serde_json::from_str::<ServerMessage>(direct).unwrap() {
            ServerMessage::HelloAck {
                capabilities,
                push_environment,
                ..
            } => {
                assert!(capabilities.push, "a minor-13 direct daemon still says so");
                assert!(
                    !capabilities.push_relay,
                    "absent must read as no relay, never as an offer"
                );
                assert_eq!(
                    push_environment, None,
                    "a daemon that cannot report an environment reports none"
                );
            }
            other => panic!("wrong message: {other:?}"),
        }

        // The same message from a relay daemon, and the one-hot rule the phone
        // resolves it with.
        let relay = direct.replace(r#""push":true"#, r#""push":false,"push_relay":true"#);
        match serde_json::from_str::<ServerMessage>(&relay).unwrap() {
            ServerMessage::HelloAck { capabilities, .. } => {
                assert!(!capabilities.push);
                assert!(capabilities.push_relay);
            }
            other => panic!("wrong message: {other:?}"),
        }

        // A minor-13 registration — no credential — decodes unchanged, which is
        // what lets a new daemon in direct mode accept an older phone.
        let legacy: ClientMessage = serde_json::from_str(
            r#"{"type":"register_push","token":"aabb","environment":"production"}"#,
        )
        .unwrap();
        match legacy {
            ClientMessage::RegisterPush {
                relay_credential, ..
            } => assert_eq!(relay_credential, None),
            other => panic!("wrong message: {other:?}"),
        }

        // And a minor-14 registration is exactly the same document plus one
        // optional key, so an older daemon ignores it rather than failing.
        let carried = serde_json::to_value(ClientMessage::RegisterPush {
            token: "aabb".into(),
            environment: Some("production".into()),
            relay_credential: Some("opaque".into()),
            features: None,
        })
        .unwrap();
        assert_eq!(carried["type"], "register_push");
        assert_eq!(carried["relay_credential"], "opaque");

        // **And the message cannot print the bearer it carries.** Every message
        // here derives `Debug`; a bare `String` would reach a log the first time
        // anybody wrote `{message:?}` in a parse-error branch or an error
        // context, without a line anywhere that looks like it logs a secret.
        let carried: ClientMessage = serde_json::from_value(carried.clone()).unwrap();
        let rendered = format!("{carried:?}");
        assert!(
            !rendered.contains("opaque"),
            "a registration must not print its credential: {rendered}"
        );
        assert!(rendered.contains("aabb"), "the token still prints");

        // Absent, not null: a daemon reading `relay_credential` as present-and-
        // empty would refuse a direct registration that is perfectly valid.
        let direct_registration = serde_json::to_value(ClientMessage::RegisterPush {
            token: "aabb".into(),
            environment: Some("production".into()),
            relay_credential: None,
            features: None,
        })
        .unwrap();
        assert!(direct_registration.get("relay_credential").is_none());
    }

    #[test]
    fn an_answer_may_name_its_session_and_a_legacy_one_still_decodes() {
        let legacy: ClientMessage = serde_json::from_str(
            r#"{"type":"answer","request_id":"toolu_1","payload_hash":"h",
                "decision":{"type":"allow"}}"#,
        )
        .unwrap();
        match legacy {
            ClientMessage::Answer { session_id, .. } => assert_eq!(session_id, None),
            other => panic!("wrong message: {other:?}"),
        }

        let scoped: ClientMessage = serde_json::from_str(
            r#"{"type":"answer","request_id":"toolu_1","payload_hash":"h",
                "decision":{"type":"allow"},"session_id":"01K1B3XQ8ZC0DE5FGH7JKMNPQR"}"#,
        )
        .unwrap();
        match scoped {
            ClientMessage::Answer { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR"));
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn a_hello_ack_without_a_minor_decodes_as_minor_zero() {
        let ack: ServerMessage = serde_json::from_str(
            r#"{"type":"hello_ack","protocol_version":1,"server_time":"t",
                "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
                "answer_path":"send_keys","hold_secs":0,"send_text":true,
                "capture":true,"push":false,"tls":false}}"#,
        )
        .unwrap();
        match ack {
            ServerMessage::HelloAck {
                protocol_minor,
                capabilities,
                ..
            } => {
                assert_eq!(protocol_minor, 0);
                // Capabilities added later default rather than failing to decode.
                assert!(!capabilities.diff);
                assert!(!capabilities.tls_active);
                assert!(!capabilities.session_uid);
                assert!(!capabilities.send_text_idempotent);
                assert!(!capabilities.prompt_identity);
                assert!(!capabilities.delete_session);
                assert!(!capabilities.test_push);
                assert!(!capabilities.push_relay);
                assert!(!capabilities.terminal_pty);
                // A daemon predating the honoured interrupt answered `rejected`
                // to every ask. Absent must therefore read as "does not honour
                // it", never as the permissive default — a true here would put a
                // Stop button on a phone talking to a daemon that cannot stop
                // anything.
                assert!(!capabilities.codex_interrupt);
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn a_minor_two_send_text_still_decodes_and_claims_no_identity() {
        // The whole additive contract in one message: a client built before
        // minor 3 sends exactly this, and it must keep working — without
        // accidentally being treated as an idempotent mutation it never was.
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"send_text","session_id":"cc-1","text":"hi",
                "require":{"mode":"input_box"},"submit":true}"#,
        )
        .unwrap();
        match msg {
            ClientMessage::SendText {
                request_id,
                payload_hash,
                require,
                submit,
                ..
            } => {
                assert_eq!(request_id, None);
                assert_eq!(payload_hash, None);
                assert!(submit);
                // Still decoded, deliberately ignored by the daemon.
                assert!(require.is_some());
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn an_idempotent_send_text_round_trips_with_its_identity() {
        let hash = crate::hash::send_text_hash("cc-1", "hi", true);
        let msg = ClientMessage::SendText {
            session_id: "cc-1".into(),
            text: "hi".into(),
            request_id: Some("st-1".into()),
            payload_hash: Some(hash.clone()),
            require: None,
            submit: true,
            complete_native_confirmation: false,
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        assert!(!encoded.contains("require"), "{encoded}");
        match serde_json::from_str::<ClientMessage>(&encoded).unwrap() {
            ClientMessage::SendText {
                request_id,
                payload_hash,
                ..
            } => {
                assert_eq!(request_id.as_deref(), Some("st-1"));
                assert_eq!(payload_hash.as_deref(), Some(hash.as_str()));
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn send_text_results_round_trip_including_the_new_ones() {
        for result in [
            SendTextResult::Sent {
                matched: "foragents".into(),
            },
            SendTextResult::Refused {
                reason: "composer busy".into(),
            },
            SendTextResult::Duplicate {
                matched: "foragents".into(),
                applied_at: "2026-07-31T10:00:00.000Z".into(),
            },
            SendTextResult::Indeterminate {
                reason: "the daemon restarted mid-injection".into(),
            },
        ] {
            let encoded = serde_json::to_string(&result).unwrap();
            assert_eq!(result, serde_json::from_str(&encoded).unwrap());
        }
    }

    #[test]
    fn an_indeterminate_outcome_says_so_and_a_settled_one_stays_silent() {
        let mut outcome = AnswerOutcome {
            request_id: "toolu_1".into(),
            session_id: "cc-1".into(),
            decision: AnswerDecision::Allow,
            resolved_by: ResolvedBy::Phone,
            applied_via: AnswerPath::SendKeys,
            resolved_at: "t".into(),
            detail: None,
            inferred: false,
            indeterminate: true,
        };
        assert!(serde_json::to_string(&outcome)
            .unwrap()
            .contains("\"indeterminate\":true"));
        outcome.indeterminate = false;
        assert!(!serde_json::to_string(&outcome)
            .unwrap()
            .contains("indeterminate"));
    }

    #[test]
    fn a_card_from_before_prompt_identity_never_claims_to_have_it() {
        // The safe direction for a missing field: an old card decodes as
        // unbound, so nothing treats it as provable.
        let card: ApprovalCard = serde_json::from_str(
            r#"{"request_id":"toolu_1","payload_hash":"h","tool_name":"Bash",
                "tool_input":{},"display_text":"Bash"}"#,
        )
        .unwrap();
        assert_eq!(card.generation, 0);
        assert!(!card.identity_bound);
    }

    fn capabilities_fixture() -> Capabilities {
        Capabilities {
            can_approve_reliably: true,
            fail_mode: "fail_open".into(),
            answer_path: AnswerPath::SendKeys,
            hold_secs: 0,
            send_text: true,
            capture: true,
            delete_session: true,
            test_push: true,
            push: false,
            push_relay: false,
            tls: true,
            tls_active: true,
            diff: true,
            risk_class: true,
            session_uid: true,
            send_text_idempotent: true,
            prompt_identity: true,
            command_catalog: true,
            slash_composer_recovery: true,
            terminal_pty: true,
            codex_interrupt: true,
            supported_agents: vec![crate::agent::AgentKind::Claude],
        }
    }

    /// **The Phase-1 byte-identical gate.** Adding the Codex resolution envelope
    /// and the `option_id` decision variant must not have moved a single byte of
    /// a Claude answer's serialization. These are the exact strings the shipped
    /// client already decodes; if a field reorders, a key renames, or an
    /// `option_id`/`codex` key leaks in, this fails.
    #[test]
    fn a_claude_answer_outcome_serializes_byte_for_byte_as_before() {
        let outcome = AnswerOutcome {
            request_id: "toolu_1".into(),
            session_id: "cc-1".into(),
            decision: AnswerDecision::Allow,
            resolved_by: ResolvedBy::Phone,
            applied_via: AnswerPath::SendKeys,
            resolved_at: "2026-08-18T00:00:00.000Z".into(),
            detail: None,
            inferred: false,
            indeterminate: false,
        };
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            r#"{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},"resolved_by":"phone","applied_via":"send_keys","resolved_at":"2026-08-18T00:00:00.000Z"}"#
        );

        let applied = AnswerResult::Applied { outcome };
        assert_eq!(
            serde_json::to_string(&applied).unwrap(),
            r#"{"status":"applied","outcome":{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},"resolved_by":"phone","applied_via":"send_keys","resolved_at":"2026-08-18T00:00:00.000Z"}}"#
        );
    }

    /// The existing `AnswerDecision` variants must serialize exactly as before;
    /// `option_id` is purely additive and a Claude answer never emits it.
    #[test]
    fn the_claude_decision_variants_are_unchanged_and_option_id_is_additive() {
        assert_eq!(
            serde_json::to_string(&AnswerDecision::Allow).unwrap(),
            r#"{"type":"allow"}"#
        );
        assert_eq!(
            serde_json::to_string(&AnswerDecision::Deny).unwrap(),
            r#"{"type":"deny"}"#
        );
        assert_eq!(
            serde_json::to_string(&AnswerDecision::Option { index: 2 }).unwrap(),
            r#"{"type":"option","index":2}"#
        );
        // The additive variant, and its round-trip.
        let by_id = AnswerDecision::OptionId {
            option_id: "acceptWithExecpolicyAmendment".into(),
        };
        assert_eq!(
            serde_json::to_string(&by_id).unwrap(),
            r#"{"type":"option_id","option_id":"acceptWithExecpolicyAmendment"}"#
        );
        assert_eq!(
            serde_json::from_str::<AnswerDecision>(
                r#"{"type":"option_id","option_id":"acceptWithExecpolicyAmendment"}"#
            )
            .unwrap(),
            by_id
        );
    }

    #[test]
    fn client_features_absent_is_claude_only() {
        // A hello/register_push that names no features is the legacy shape:
        // Claude-only, and Claude is supported by the empty set.
        let empty = ClientFeatures::default();
        assert!(empty.supports(&crate::agent::AgentKind::Claude));
        assert!(!empty.supports(&crate::agent::AgentKind::Codex));
        // Named agents: Claude must still be listed to be a member of a
        // non-empty set, but the empty-set case is the only Claude-implicit one.
        let codex_only = ClientFeatures {
            agents: vec![crate::agent::AgentKind::Codex],
        };
        assert!(!codex_only.supports(&crate::agent::AgentKind::Claude));
        assert!(codex_only.supports(&crate::agent::AgentKind::Codex));
        // An unknown agent name is preserved through decode and grants nothing —
        // not even to itself. Round-tripping the name honestly is a storage
        // property; authorizing on it would be this build vouching that a phone
        // can render an agent neither side can name.
        let json = r#"{"agents":["codex","gemini"]}"#;
        let decoded: ClientFeatures = serde_json::from_str(json).unwrap();
        assert!(decoded.supports(&crate::agent::AgentKind::Codex));
        assert!(!decoded.supports(&crate::agent::AgentKind::Claude));
        assert!(decoded
            .agents
            .contains(&crate::agent::AgentKind::Unsupported("gemini".into())));
    }

    /// **A device that advertised an unknown agent is not authorized for it.**
    ///
    /// The hole this pins was reachable the moment anything writes a feature set:
    /// `Vec::contains` matches `Unsupported("gemini")` against `Unsupported(
    /// "gemini")`, so the one set that could *possibly* claim the agent — the one
    /// that names it — was the one set that authorized it, in direct contradiction
    /// of the "grants nothing" contract two lines above it.
    ///
    /// **Mutation:** drop the `Unsupported` arm from `ClientFeatures::supports`
    /// and the first assertion fails.
    #[test]
    fn an_advertised_unknown_agent_authorizes_nothing() {
        let gemini = crate::agent::AgentKind::from_str_lossy("gemini");
        assert_eq!(
            gemini,
            crate::agent::AgentKind::Unsupported("gemini".into())
        );
        // The device named the very agent being asked about, and it still grants
        // nothing: this build cannot drive `gemini`, so it cannot vouch for a
        // phone's ability to render one.
        let advertised = ClientFeatures {
            agents: vec![gemini.clone()],
        };
        assert!(!advertised.supports(&gemini));
        // And naming an unknown agent does not buy the floor either: a non-empty
        // set grants exactly what it names, and it named nothing this build knows.
        assert!(!advertised.supports(&crate::agent::AgentKind::Claude));
        assert!(!advertised.supports(&crate::agent::AgentKind::Codex));
        // The empty set is still the legacy Claude floor, and an unknown agent
        // gets nothing from it.
        assert!(!ClientFeatures::default().supports(&gemini));
    }

    #[test]
    fn capabilities_supported_agents_defaults_empty_for_an_older_daemon() {
        // An ack from a daemon predating the seam omits the list; it decodes as
        // empty, which a client reads as Claude-only.
        let older = serde_json::json!({
            "can_approve_reliably": true, "fail_mode": "fail_open",
            "answer_path": "send_keys", "hold_secs": 0, "send_text": true,
            "capture": true, "push": false, "tls": true
        });
        let caps: Capabilities = serde_json::from_value(older).expect("decodes");
        assert!(caps.supported_agents.is_empty());

        // An empty list is **omitted** on the wire — a daemon with nothing to add
        // beyond the Claude floor sends no field, so an older phone renders no new
        // diagnostic row. This is what the current daemon emits in Phase 1.
        let mut floor = capabilities_fixture();
        floor.supported_agents = Vec::new();
        let s = serde_json::to_string(&floor).unwrap();
        assert!(
            !s.contains("supported_agents"),
            "empty must be omitted: {s}"
        );

        // A non-empty list (Phase 2, once a second agent can be driven) serializes.
        let s = serde_json::to_string(&capabilities_fixture()).unwrap();
        assert!(s.contains(r#""supported_agents":["claude"]"#), "{s}");
    }

    #[test]
    fn the_codex_resolution_envelope_round_trips_every_terminal() {
        for value in [
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: Some(AnswerDecision::OptionId {
                    option_id: "accept".into(),
                }),
            },
            CodexResolution::Answered {
                by: ResolutionActor::Local,
                decision: None,
            },
            CodexResolution::Cleared {
                cause: ClearCause::TurnAborted,
            },
            CodexResolution::Cleared {
                cause: ClearCause::Superseded,
            },
            CodexResolution::Cleared {
                cause: ClearCause::TurnCompleted,
            },
            CodexResolution::Cleared {
                cause: ClearCause::ItemCompleted,
            },
            CodexResolution::Timeout,
            CodexResolution::Unknown {
                attempted_by: ResolutionActor::Phone,
                attempted_decision: Some(AnswerDecision::Allow),
                write_stage: WriteStage::UpstreamWriteUnconfirmed,
                cause: "connection reset before ack".into(),
            },
        ] {
            let s = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<CodexResolution>(&s).unwrap(), value);
        }
        // Pin the wire strings the phone matches by hand.
        assert_eq!(
            serde_json::to_string(&CodexResolution::Cleared {
                cause: ClearCause::TurnAborted
            })
            .unwrap(),
            r#"{"status":"cleared","cause":"turn_aborted"}"#
        );
        assert_eq!(
            serde_json::to_string(&CodexResolution::Answered {
                by: ResolutionActor::Local,
                decision: None
            })
            .unwrap(),
            r#"{"status":"answered","by":"local"}"#
        );
        // The item-bound retirement, pinned by hand like its siblings. Written
        // out rather than derived from the variant name: this string is what a
        // phone matches on, so a rename that kept compiling would be a silent
        // wire change.
        assert_eq!(
            serde_json::to_string(&CodexResolution::Cleared {
                cause: ClearCause::ItemCompleted
            })
            .unwrap(),
            r#"{"status":"cleared","cause":"item_completed"}"#
        );
        // And it is a DIFFERENT string from the turn terminal it must never be
        // confused with — the distinction the variant exists to make.
        assert_ne!(
            serde_json::to_string(&CodexResolution::Cleared {
                cause: ClearCause::ItemCompleted
            })
            .unwrap(),
            serde_json::to_string(&CodexResolution::Cleared {
                cause: ClearCause::TurnCompleted
            })
            .unwrap()
        );
    }

    #[test]
    fn the_interrupt_operation_round_trips() {
        let msg = ClientMessage::Interrupt {
            session_id: "cc-1".into(),
            request_id: "r-1".into(),
            turn_id: "turn-7".into(),
            payload_hash: crate::hash::interrupt_hash("cc-1", "turn-7"),
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains(r#""type":"interrupt""#), "{s}");
        let _: ClientMessage = serde_json::from_str(&s).unwrap();
        let result = ServerMessage::InterruptResult {
            session_id: "cc-1".into(),
            request_id: "r-1".into(),
            result: InterruptResult::Rejected {
                reason: "interrupt is not supported yet".into(),
            },
        };
        let s = serde_json::to_string(&result).unwrap();
        assert!(s.contains(r#""status":"rejected""#), "{s}");
    }

    /// **The wire matrix, at the type level** (Phase-1 gate): a connection's
    /// advertised-agent set scopes what may be delivered to it, and the daemon's
    /// own `supported_agents` is intersected with it. A client that predates the
    /// seam (no features) is Claude-only, so a Codex session's records are never
    /// eligible for it; a Codex-aware client is. The per-connection *enforcement*
    /// of this scoping is a later phase; this pins the primitive it will use.
    #[test]
    fn the_advertised_agent_set_scopes_what_a_connection_may_receive() {
        use crate::agent::AgentKind;
        // The effective set a connection may receive = the agents the daemon
        // supports ∩ the agents the client advertised.
        fn deliverable(daemon: &[AgentKind], client: &ClientFeatures, agent: &AgentKind) -> bool {
            daemon.contains(agent) && client.supports(agent)
        }
        let daemon_supports = [AgentKind::Claude]; // Phase-1 daemon: Claude only.

        // An old reader (no features ⇒ Claude-only): Claude records deliver,
        // Codex records never do.
        let old_reader = ClientFeatures::default();
        assert!(deliverable(
            &daemon_supports,
            &old_reader,
            &AgentKind::Claude
        ));
        assert!(!deliverable(
            &daemon_supports,
            &old_reader,
            &AgentKind::Codex
        ));

        // A Codex-aware reader: Codex would be deliverable *once the daemon also
        // supports it* — but not while the daemon is Claude-only, so the daemon's
        // honesty is the backstop even for a client that asks for more.
        let codex_reader = ClientFeatures {
            agents: vec![AgentKind::Claude, AgentKind::Codex],
        };
        assert!(deliverable(
            &daemon_supports,
            &codex_reader,
            &AgentKind::Claude
        ));
        assert!(
            !deliverable(&daemon_supports, &codex_reader, &AgentKind::Codex),
            "a daemon must not deliver an agent it does not support, even when asked"
        );
        assert!(deliverable(
            &[AgentKind::Claude, AgentKind::Codex],
            &codex_reader,
            &AgentKind::Codex
        ));

        // An unknown advertised agent grants nothing actionable and is never
        // Claude.
        let unknown_reader = ClientFeatures {
            agents: vec![AgentKind::Unsupported("gemini".into())],
        };
        assert!(!deliverable(
            &daemon_supports,
            &unknown_reader,
            &AgentKind::Claude
        ));
    }

    #[test]
    fn command_catalog_round_trips_both_ways() {
        let ask = ClientMessage::GetCommandCatalog {
            session_id: "u-1".into(),
        };
        let encoded = serde_json::to_string(&ask).unwrap();
        assert!(
            encoded.contains(r#""type":"get_command_catalog""#),
            "{encoded}"
        );
        match serde_json::from_str::<ClientMessage>(&encoded).unwrap() {
            ClientMessage::GetCommandCatalog { session_id } => assert_eq!(session_id, "u-1"),
            other => panic!("wrong message: {other:?}"),
        }

        let available = ServerMessage::CommandCatalog {
            session_id: "u-1".into(),
            result: CommandCatalogResult::Available {
                commands: vec!["model".into(), "clear".into()],
                claude_version: Some("2.1.221".into()),
                probed_at: "2026-08-04T20:00:00Z".into(),
            },
        };
        let encoded = serde_json::to_string(&available).unwrap();
        assert!(encoded.contains(r#""status":"available""#), "{encoded}");
        match serde_json::from_str::<ServerMessage>(&encoded).unwrap() {
            ServerMessage::CommandCatalog {
                result: CommandCatalogResult::Available { commands, .. },
                ..
            } => assert_eq!(commands, vec!["model".to_string(), "clear".to_string()]),
            other => panic!("wrong message: {other:?}"),
        }

        let unavailable = ServerMessage::CommandCatalog {
            session_id: "u-1".into(),
            result: CommandCatalogResult::Unavailable {
                reason: "no binary".into(),
            },
        };
        let encoded = serde_json::to_string(&unavailable).unwrap();
        assert!(encoded.contains(r#""status":"unavailable""#), "{encoded}");
    }

    #[test]
    fn every_terminal_message_round_trips() {
        let client = [
            ClientMessage::TerminalAttach {
                attachment_id: "att-1".into(),
                session_uid: "01KZ".into(),
                cols: 80,
                rows: 24,
                output_credit: TERMINAL_INITIAL_OUTPUT_CREDIT,
            },
            ClientMessage::TerminalInput {
                attachment_id: "att-1".into(),
                data: "bHM=".into(),
            },
            ClientMessage::TerminalResize {
                attachment_id: "att-1".into(),
                cols: 100,
                rows: 40,
            },
            ClientMessage::TerminalCredit {
                attachment_id: "att-1".into(),
                bytes: 4096,
            },
            ClientMessage::TerminalDetach {
                attachment_id: "att-1".into(),
            },
        ];
        for message in client {
            let encoded = serde_json::to_string(&message).unwrap();
            let decoded: ClientMessage = serde_json::from_str(&encoded).unwrap();
            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                encoded,
                "round trip changed {encoded}"
            );
        }

        let server = [
            ServerMessage::TerminalAttached {
                attachment_id: "att-1".into(),
                input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
                max_chunk_bytes: MAX_TERMINAL_CHUNK_BYTES as u32,
                max_outstanding_credit: TERMINAL_MAX_OUTSTANDING_CREDIT,
            },
            ServerMessage::TerminalOutput {
                attachment_id: "att-1".into(),
                data: "aGk=".into(),
            },
            ServerMessage::TerminalCredit {
                attachment_id: "att-1".into(),
                bytes: 8192,
            },
            ServerMessage::TerminalClosed {
                attachment_id: "att-1".into(),
                code: terminal_close::DETACHED.into(),
                reason: "the tab was closed".into(),
            },
        ];
        for message in server {
            let encoded = serde_json::to_string(&message).unwrap();
            let decoded: ServerMessage = serde_json::from_str(&encoded).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
        }

        // The tags are snake_case and stable — the phone matches these strings
        // by hand.
        let attach = serde_json::to_string(&ClientMessage::TerminalAttach {
            attachment_id: "a".into(),
            session_uid: "u".into(),
            cols: 80,
            rows: 24,
            output_credit: 1,
        })
        .unwrap();
        assert!(attach.contains("\"type\":\"terminal_attach\""), "{attach}");
    }

    /// The attach ack carries both flow-control ceilings, under the names the
    /// phone reads them by.
    ///
    /// Without them a client can only hard-code this daemon's numbers, and the
    /// first daemon that raises either one kills the terminal of every phone
    /// already installed — which is the whole reason they are on the wire and
    /// not merely in this file.
    #[test]
    fn the_attach_ack_advertises_both_ceilings() {
        let encoded = serde_json::to_string(&ServerMessage::TerminalAttached {
            attachment_id: "att-1".into(),
            input_credit: TERMINAL_INITIAL_INPUT_CREDIT,
            max_chunk_bytes: MAX_TERMINAL_CHUNK_BYTES as u32,
            max_outstanding_credit: TERMINAL_MAX_OUTSTANDING_CREDIT,
        })
        .unwrap();
        assert!(
            encoded.contains(&format!("\"max_chunk_bytes\":{MAX_TERMINAL_CHUNK_BYTES}")),
            "{encoded}"
        );
        assert!(
            encoded.contains(&format!(
                "\"max_outstanding_credit\":{TERMINAL_MAX_OUTSTANDING_CREDIT}"
            )),
            "{encoded}"
        );
        // Required, not defaulted: an ack without them is a decode failure
        // rather than a silent zero a client would then enforce.
        let missing = r#"{"type":"terminal_attached","attachment_id":"a","input_credit":1}"#;
        assert!(serde_json::from_str::<ServerMessage>(missing).is_err());
    }

    /// The `terminal_pty` capability defaults to false, so a client parsing an
    /// ack from a daemon that predates minor 13 offers no Terminal rather than
    /// one that cannot work — and a new client decodes an old ack cleanly.
    #[test]
    fn terminal_capability_is_absent_by_default() {
        let old_ack = r#"{"type":"hello_ack","protocol_version":1,"server_time":"t",
            "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
            "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,
            "push":false,"tls":false}}"#;
        match serde_json::from_str::<ServerMessage>(old_ack).unwrap() {
            ServerMessage::HelloAck { capabilities, .. } => {
                assert!(
                    !capabilities.terminal_pty,
                    "absent must read as no terminal"
                );
                assert!(
                    !capabilities.command_catalog,
                    "and so must every later flag"
                );
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn get_diff_and_diff_round_trip() {
        let request: ClientMessage =
            serde_json::from_str(r#"{"type":"get_diff","session_id":"cc-1"}"#).unwrap();
        match request {
            ClientMessage::GetDiff { session_id } => assert_eq!(session_id, "cc-1"),
            other => panic!("wrong message: {other:?}"),
        }

        let reply = ServerMessage::Diff {
            session_id: "cc-1".into(),
            unified: "diff --git a/x b/x\n".into(),
            truncated: false,
            captured_at: "2026-07-31T10:00:00.000Z".into(),
            note: None,
        };
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.contains("\"type\":\"diff\""), "{encoded}");
        assert!(!encoded.contains("note"), "an absent note must not be sent");
        let _: ServerMessage = serde_json::from_str(&encoded).unwrap();
    }

    #[test]
    fn a_note_distinguishes_clean_from_not_a_repository() {
        let encoded = serde_json::to_string(&ServerMessage::Diff {
            session_id: "cc-1".into(),
            unified: String::new(),
            truncated: false,
            captured_at: "t".into(),
            note: Some("not a git repository".into()),
        })
        .unwrap();
        assert!(encoded.contains("not a git repository"), "{encoded}");
    }

    #[test]
    fn approval_card_carries_risk_and_omits_it_when_unset() {
        let mut card = ApprovalCard {
            request_id: "toolu_1".into(),
            payload_hash: "h".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf /tmp/x"}),
            display_text: "Bash\n{}".into(),
            permission_suggestions: None,
            prompt_id: None,
            permission_mode: None,
            risk: None,
            generation: 1,
            identity_bound: true,
        };
        assert!(!serde_json::to_string(&card).unwrap().contains("risk"));

        card.risk = Some(crate::risk::classify(&card.tool_name, &card.tool_input));
        let encoded = serde_json::to_string(&card).unwrap();
        assert!(encoded.contains(r#""class":"high""#), "{encoded}");
        assert!(encoded.contains("matched_pattern"), "{encoded}");
        assert_eq!(card, serde_json::from_str(&encoded).unwrap());
    }

    #[test]
    fn an_inferred_outcome_is_labelled_and_an_observed_one_is_not() {
        let mut outcome = AnswerOutcome {
            request_id: "toolu_1".into(),
            session_id: "cc-1".into(),
            decision: AnswerDecision::Deny,
            resolved_by: ResolvedBy::Local,
            applied_via: AnswerPath::SendKeys,
            resolved_at: "t".into(),
            detail: Some("prompt left the pane".into()),
            inferred: true,
            indeterminate: false,
        };
        assert!(serde_json::to_string(&outcome)
            .unwrap()
            .contains("\"inferred\":true"));

        outcome.inferred = false;
        // The common case stays off the wire entirely.
        assert!(!serde_json::to_string(&outcome)
            .unwrap()
            .contains("inferred"));
    }

    #[test]
    fn subscribe_after_seq_defaults_to_zero() {
        let msg: ClientMessage =
            serde_json::from_str(r#"{"type":"subscribe","session_id":"cc-1"}"#).unwrap();
        match msg {
            ClientMessage::Subscribe { after_seq, .. } => assert_eq!(after_seq, 0),
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn answer_decisions_round_trip() {
        for decision in [
            AnswerDecision::Allow,
            AnswerDecision::Deny,
            AnswerDecision::Option { index: 2 },
            AnswerDecision::Text {
                text: "use the scratchpad instead".into(),
            },
        ] {
            let s = serde_json::to_string(&decision).unwrap();
            assert_eq!(decision, serde_json::from_str(&s).unwrap());
        }
    }

    #[test]
    fn duplicate_answer_result_carries_original_outcome() {
        let outcome = AnswerOutcome {
            request_id: "toolu_1".into(),
            session_id: "cc-1".into(),
            decision: AnswerDecision::Allow,
            resolved_by: ResolvedBy::Phone,
            applied_via: AnswerPath::SendKeys,
            resolved_at: "2026-07-30T16:36:58.412Z".into(),
            detail: None,
            inferred: false,
            indeterminate: false,
        };
        let result = AnswerResult::Duplicate {
            outcome: outcome.clone(),
            stale_payload_hash: true,
        };
        let s = serde_json::to_string(&result).unwrap();
        assert!(s.contains("\"status\":\"duplicate\""));
        match serde_json::from_str::<AnswerResult>(&s).unwrap() {
            AnswerResult::Duplicate {
                outcome: back,
                stale_payload_hash,
            } => {
                assert_eq!(back, outcome);
                assert!(stale_payload_hash);
            }
            other => panic!("wrong result: {other:?}"),
        }
    }
}
