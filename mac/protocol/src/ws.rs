//! The tailnet WebSocket protocol the iPhone speaks.
//!
//! Reconnect is one mechanism: `subscribe{session_id, after_seq}` replays the
//! log gap-free. When the daemon cannot guarantee gap-freedom (a slow client
//! fell off the broadcast ring) it emits an explicit `Resync` event rather than
//! silently skipping — a gap the client cannot see is the failure mode that
//! makes an event log worthless.

use serde::{Deserialize, Serialize};

use crate::event::{Event, SessionSummary};

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

/// Ceiling on one `compose`'s text.
///
/// The same number as [`MAX_SEND_TEXT_BYTES`] and for the same reason stated the other
/// way round: a megabyte typed into a TTY is not a takeover, and a megabyte put into a
/// model's mouth is not a message. It is a bound on what one tap can inject, chosen to
/// match the sibling operation so a phone has one answer to "how long may this be".
pub const MAX_COMPOSE_BYTES: usize = 8 * 1024;

/// Ceiling on one uploaded file ([`ClientMessage::PutFile`]).
pub const MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024;

/// Ceiling on one [`ClientMessage::PutFile`] chunk, decoded: its base64 stays well under
/// [`MAX_CLIENT_MESSAGE_BYTES`].
pub const MAX_UPLOAD_CHUNK_BYTES: usize = 512 * 1024;

/// Ceiling on one terminal chunk, decoded, in either direction ([`ClientMessage::TerminalInput`],
/// [`ServerMessage::TerminalOutput`]): its base64 is 21,848 characters, far under [`MAX_CLIENT_MESSAGE_BYTES`].
pub const MAX_TERMINAL_CHUNK_BYTES: u32 = 16 * 1024;

/// The input a terminal's viewer may have sent and not yet had written to the shell: granted at
/// [`ServerMessage::TerminalOpened`] and given back by [`ServerMessage::TerminalCredit`] as it is written.
pub const TERMINAL_INPUT_CREDIT: u32 = 32 * 1024;

/// The most output credit a terminal's viewer may hold at once ([`ClientMessage::TerminalOpen`]'s
/// `output_credit` plus every [`ClientMessage::TerminalCredit`] not yet spent). A grant past it closes the
/// terminal `protocol_error`.
pub const TERMINAL_MAX_OUTSTANDING_CREDIT: u32 = 256 * 1024;

/// A terminal's columns, at least and at most.
pub const TERMINAL_COLS: std::ops::RangeInclusive<u16> = 2..=512;

/// A terminal's rows, at least and at most.
pub const TERMINAL_ROWS: std::ops::RangeInclusive<u16> = 2..=256;

/// Ceiling on an `attachment_id`.
pub const MAX_ATTACHMENT_ID_BYTES: usize = 64;

/// Why a terminal's viewer was closed ([`ServerMessage::TerminalClosed`]'s `reason`).
pub mod terminal_closed {
    /// The shell exited by itself. The next open starts a fresh one.
    pub const EXITED: &str = "exited";
    /// The run ended, and its shell with it.
    pub const RUN_ENDED: &str = "run_ended";
    /// The daemon is stopping, and the shell with it.
    pub const STOPPING: &str = "stopping";
    /// Another viewer took the shell (an open with `takeover`), or held it when this open did not take over.
    pub const SUPERSEDED: &str = "superseded";
    /// The run is not live: ended, or not known here.
    pub const NOT_LIVE: &str = "not_live";
    /// The run is live but has not said which folder it is in yet: a Claude run before its first report.
    pub const NOT_READY: &str = "not_ready";
    /// This computer has as many shells as it keeps, or the shell could not be started.
    pub const UNAVAILABLE: &str = "unavailable";
    /// Output waited on spent credit for longer than the daemon waits.
    pub const SLOW_CONSUMER: &str = "slow_consumer";
    /// Only a paired device, on a connection nothing on the path can read, opens a terminal.
    pub const NOT_AUTHORISED: &str = "not_authorised";
    /// A frame broke the terminal's rules: credit, size, an unknown attachment.
    pub const PROTOCOL_ERROR: &str = "protocol_error";
}

/// The most uploads one `send_text` or `compose` attaches.
pub const MAX_ATTACHMENTS: usize = 10;

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
        /// What the device calls itself, for the daemon's list of paired devices: a Mac's computer name, or
        /// "iPhone" (all an iPhone app may read of its name). A new device is listed under it, made unique; a
        /// device already paired is renamed to it when it differs. Absent, the device is listed under
        /// `client_name`, as before. `client_name` keeps naming the app, which is how an answer says which kind of
        /// device sent it ([`AnsweredOn::from_client_name`]). Minor 44.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
    },
    Sessions,
    /// `session_id` is a **session reference**: either a `session_uid` (exact,
    /// preferred, `protocol_minor >= 2`) or a run's name (legacy,
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
    /// **One earlier page of a run's log** (minor 23, `history_pages`): the newest
    /// `limit` events with `seq < before_seq`, sent oldest first as ordinary
    /// `event` frames, then one [`ServerMessage::HistoryEnd`] carrying the same
    /// `request_id`. The daemon serves at most [`MAX_HISTORY_PAGE`] whatever
    /// `limit` asks for.
    ///
    /// Independent of `subscribe`: it moves no subscription and needs none.
    /// `session_id` is a session reference, as for `subscribe`.
    History {
        session_id: String,
        before_seq: u64,
        limit: u32,
        request_id: String,
    },
    /// Remove one run's record from the Mac.
    ///
    /// Deliberately the only verb that removes a run's record, and deliberately
    /// narrow: `session_uid`, never a name, because a later run can carry the
    /// same name and a phone that deleted "cc-1" could destroy a session it never
    /// saw. A run is refused until it has ended: `exited`, or `failed` for a
    /// start that never began (its host did not come up, or its start was
    /// refused).
    ///
    /// It removes CodeConnect's own record, not the conversation: Claude Code
    /// keeps its transcript under `~/.claude/projects`, so `claude --resume` still
    /// works afterwards. The phone's copy says so.
    DeleteSession {
        session_uid: String,
    },
    /// **End a run CodeConnect started, Claude's or Codex's** (minor 35), answered with
    /// [`ServerMessage::StopRunResult`]. A run CodeConnect did not start is refused.
    ///
    /// A `live` run becomes [`crate::event::Lifecycle::Stopping`]: its host, if its link is
    /// attached, is told to stop (Claude's agent host interrupts claude and ends its input),
    /// and a host that has not exited 10 s later, told or not, is stopped by its process group. The run then ends `exited`
    /// with the reason "Stopped". Nothing is kept per `request_id`: a retry reads the
    /// run as it is, `stopping` while it stops and `already_ended` once it has.
    ///
    /// A client sends it only when [`Capabilities::stop_run`] is advertised: a daemon
    /// below minor 35 answers `bad_request`.
    StopRun {
        session_id: String,
        request_id: String,
    },
    /// **Change a run's model, its effort, or both** (minor 39; a Codex run CodeConnect
    /// started from minor 43), for the rest of the session, answered with
    /// [`ServerMessage::SettingResult`]. At least one of the two, each
    /// one the run's [`crate::event::RunMenus`] lists: `model` a model's `value`, `effort` one
    /// of the efforts listed. A client sends it only for a run whose summary carries `menus`.
    /// Nothing is kept per `request_id`: a setting is the same state however often it is
    /// applied, so a retry applies it again.
    SetModel {
        session_id: String,
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// **Change a run's permission mode** (minor 39; a Codex run CodeConnect started from
    /// minor 43) to one of its `menus`' modes, for the rest of the session, answered with
    /// [`ServerMessage::SettingResult`]. As
    /// [`ClientMessage::SetModel`], a retry applies it again.
    SetMode {
        session_id: String,
        request_id: String,
        mode: String,
        /// The person confirmed the mode in the app (minor 40), as a mode whose menu entry is
        /// `confirm` requires; refused without it, nothing sent.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        confirmed: bool,
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
        #[serde(default = "default_true")]
        submit: bool,
        /// Uploads ([`ClientMessage::PutFile`]) this message carries, in order, at most
        /// [`MAX_ATTACHMENTS`] (minor 50). Each reaches the agent as its absolute path on a line
        /// after the text. Absent means none, and the hash is then exactly a minor-49 one.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// "What has this agent actually changed?" — answered from git, not from
    /// the event log, because the log knows about edits that were later undone
    /// and the working tree is the only thing that knows the net result.
    GetDiff {
        session_id: String,
    },
    /// **A run's repository as its Changes panel shows it** (minor 58, `changes`), answered
    /// with one [`ServerMessage::RunGit`]: branch, head, base, the changed files' lines, how
    /// far the branch is ahead of its base, whether it is pushed, and whether a merge is
    /// unresolved. Read from git in the run's folder; nothing is fetched.
    /// Paired devices only.
    GetRunGit {
        session_id: String,
    },
    /// **An action on a run's repository from its Changes panel** (minor 59, `git_actions`
    /// names the ones this daemon takes), answered with one [`ServerMessage::GitActionResult`]
    /// carrying the same `request_id`. Paired devices only. By `session_uid`, never by name.
    /// Kept per `request_id`: the same id again replays the first answer and acts nothing
    /// twice, and the same id with other arguments is refused `bad_request`.
    GitAction {
        request_id: String,
        session_uid: String,
        action: GitAction,
    },
    /// **A run's conflicts with its base** (minor 66, `conflicts`), answered with one
    /// [`ServerMessage::Conflicts`]: the commit checked out merged, in git only, with
    /// [`RunGit`]'s base as fetched now (at most one fetch per 30 s for a repository, remote and
    /// base). Uncommitted changes are not in it. Paired devices only.
    GetConflicts {
        session_id: String,
    },
    /// **One conflicted file's hunks** (minor 66, `conflicts`), answered with one
    /// [`ServerMessage::Conflict`]: `path` as [`RunConflicts::files`] names it, in the merge
    /// of `head_oid` with `base_oid` that reply compared. Nothing is fetched. Paired devices only.
    GetConflict {
        session_id: String,
        path: String,
        head_oid: String,
        base_oid: String,
    },
    /// "Which slash commands does this session's Claude Code actually have?"
    ///
    /// Answered from the installed binary's own machine-readable init message,
    /// never from a hand-maintained list — the phone uses it to label commands
    /// honestly and to refuse, client-side, the picker-shaped ones that would
    /// open a dialog on the Mac's screen and lock the composer. Session-scoped
    /// because the binary is: each run registered the executable it was
    /// launched with, and two sessions may straddle an upgrade. A run
    /// CodeConnect started is answered from the list its own claude last
    /// reported (minor 38).
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
    },

    /// Abort the running turn named by `turn_id`, on a Codex session or on a Claude run
    /// CodeConnect started.
    ///
    /// A mutating operation, so it carries a ledger identity like every other:
    /// `request_id` makes a retry idempotent and `payload_hash` binds it to the
    /// exact turn it was issued against, so a replay can never abort a *different*
    /// turn the session has since moved on to.
    ///
    /// **Honoured**: the daemon genuinely aborts the turn a Codex session is
    /// running, and answers with a typed [`ServerMessage::InterruptResult`]
    /// whatever becomes of it. For a Claude run CodeConnect started (minor 37,
    /// [`Capabilities::claude_interrupt`]) the turn is the `turn_id` of its events, its
    /// `system/init` uuid, and the run's agent host stops it only while claude is running
    /// it. Any other Claude session is refused with the sentence that says where its stop
    /// control is, rather than by a bare no.
    ///
    /// For a Codex session, two gates stand between this message and a stopped turn, and they bind
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
    /// **Say something to a Codex session from a phone.**
    ///
    /// The phone composes words; it does not decide what becomes of them. If the
    /// session's thread is idle the daemon starts a turn with them; if a turn is
    /// running it steers that turn with them. Which one happened comes back in the
    /// [`ComposeResult`], and the phone renders what it is told rather than
    /// predicting it — the session can move between the tap and the write, and a
    /// client that guessed would show the wrong thing exactly when it mattered.
    ///
    /// **This is not [`ClientMessage::SendText`], and the two must not be
    /// confused.** `send_text` types at the Mac's TTY and is refused for a Codex
    /// session by name; this speaks the app-server's own `turn/start` /
    /// `turn/steer` through the broker and is refused for a Claude session by
    /// name. They share no ledger, no result type and no failure vocabulary: one
    /// can report that a composer disappeared, and the other can report which turn
    /// it joined.
    ///
    /// **The compatibility contract is the CLIENT's, and it is a transmission gate rather
    /// than a UI one.** This is a new message, so a daemon below minor 18 cannot decode it:
    /// it answers `ServerMessage::Error{code:"bad_request"}` — legible, but not a
    /// `ComposeResult`, so a client waiting for one waits for something that is not coming.
    /// A client must therefore not SEND this unless [`Capabilities::codex_compose`] is
    /// true; hiding the affordance is not the same thing, and the client's tests must
    /// prove the transmission is gated and not merely the button.
    ///
    /// A mutating operation, so it carries a ledger identity like every other.
    /// `request_id` makes a retry idempotent and `payload_hash` binds it to the
    /// exact text, so a replay can never say something else. **The route is
    /// snapshotted at the claim, not recomputed on the retry**: a compose that was
    /// issued as a `turn/start` is re-issued as a `turn/start`, never converted
    /// into a steer because the session has since become busy — that conversion
    /// would put the words into a turn nobody composed them for.
    ///
    /// Two gates stand between this message and the model, exactly as they do for
    /// [`ClientMessage::Interrupt`]. The daemon's binds the ask to the thread its
    /// control link is *subscribed* to and to that link's visit generation, before
    /// anything durable is claimed. The broker's binds it again to the session's
    /// own head thread — and, for a steer, to the turn the session is actually
    /// running — and refuses one that does not name it.
    Compose {
        session_id: String,
        request_id: String,
        /// What to say. Bounded by [`MAX_COMPOSE_BYTES`].
        text: String,
        /// [`crate::hash::compose_hash`] over `session_id`, `text`, and the attachments when
        /// there are any ([`crate::hash::compose_hash_with`]).
        payload_hash: String,
        /// Uploads this message carries, as [`ClientMessage::SendText`]'s (minor 50).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },

    /// **One chunk of a file for a session's agent** (minor 50, [`Capabilities::uploads`]),
    /// answered with a [`ServerMessage::PutFileResult`]. The first chunk names the file and
    /// its size and binds the upload to this connection and to the session's run; each next
    /// chunk starts where the last answer's `received` says. At exactly `size` the file is
    /// complete, under `~/.codeconnect/uploads/<session_uid>/<upload_id>/<name>`, and a
    /// `send_text` or `compose` on this connection may attach it. Without `session_id` it is
    /// for a run not started yet: held under `~/.codeconnect/uploads/.pending/<upload_id>/`,
    /// bound to this connection alone, for a [`ClientMessage::StartSession`] on it to attach.
    /// One that nothing attached is removed when the connection closes, or when ccd starts
    /// again. A refused chunk ends the upload for the client: it sends the file again under a
    /// new id, from offset 0.
    PutFile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// The client's id for this upload: one path component, no separators, not
        /// starting with a dot.
        upload_id: String,
        /// The file's name as the agent will see it: the same rule as `upload_id`, at most
        /// 255 bytes.
        name: String,
        /// The whole file's bytes, at most [`MAX_UPLOAD_BYTES`]; fixed by the first chunk.
        size: u64,
        /// Where this chunk starts: the bytes received so far.
        offset: u64,
        /// The chunk, base64, at most [`MAX_UPLOAD_CHUNK_BYTES`] decoded.
        data: String,
    },

    /// **A shell in a run's folder, on this computer** (minor 56, `run_terminal`): opened or attached to,
    /// answered with a [`ServerMessage::TerminalOpened`] and then its output, or with a
    /// [`ServerMessage::TerminalClosed`]. The shell is the run's: one per run, started by the first open,
    /// in the folder the daemon holds for the run, as its own user's login shell; it ends with the run, never
    /// with a viewer. `session_id` is the run's uid; nothing in this frame names a path, a program or a user.
    /// One terminal per connection: an open replaces the connection's earlier one.
    TerminalOpen {
        /// The client's name for this attachment, at most [`MAX_ATTACHMENT_ID_BYTES`]; every later frame of it
        /// carries it.
        attachment_id: String,
        session_id: String,
        cols: u16,
        rows: u16,
        /// Output bytes the client is ready for, at most [`TERMINAL_MAX_OUTSTANDING_CREDIT`].
        output_credit: u32,
        /// The output offset this client already shows, when it still holds the screen ([`ServerMessage::TerminalOutput`]'s
        /// `offset`): output resumes after it, with no reset, while the daemon still holds what follows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_after: Option<u64>,
        /// The person asked for this shell (they opened it, or typed in a view another one had taken): it takes
        /// the shell from any other viewer. Without it, an open finding another viewer is closed `superseded`.
        #[serde(default)]
        takeover: bool,
    },
    /// Bytes typed or pasted into an open terminal, base64, at most [`MAX_TERMINAL_CHUNK_BYTES`] decoded and
    /// never more than the input credit left.
    TerminalInput {
        attachment_id: String,
        data: String,
    },
    /// The terminal's view changed size.
    TerminalResize {
        attachment_id: String,
        cols: u16,
        rows: u16,
    },
    /// The client fed `bytes` more of the output and is ready for as many again.
    TerminalCredit {
        attachment_id: String,
        bytes: u32,
    },
    /// The client stopped viewing (the bar folded, the panel closed): the shell keeps running.
    TerminalDetach {
        attachment_id: String,
    },

    /// **The folders sessions have run in on this computer** (minor 28,
    /// `start_session`), answered with a [`ServerMessage::Projects`], and with a
    /// second one when the agent check that answer started finds a change.
    ListProjects,
    /// **Start an agent in one of those folders** (minor 28, `start_session`), as
    /// if `codeconnect <agent>` had been typed there. Answered with one
    /// [`ServerMessage::StartSessionResult`] carrying the same `request_id`.
    ///
    /// `path` is an absolute path: one [`ServerMessage::Projects`] listed, or, from
    /// minor 30 (`browse_folders`), any folder; a relative one is refused before
    /// anything runs. `prompt` is the agent's own first prompt. A start without
    /// one, or with one of only white space, starts the run waiting for the
    /// person's first message (a daemon before 0.19.1 refuses it
    /// `launch_failed`). It is bounded by
    /// [`MAX_COMPOSE_BYTES`] and passed as one argument, never through a shell
    /// string.
    ///
    /// `request_id` makes the start idempotent: the same id sent again, on this
    /// connection or a later one, gets the first answer and starts nothing new.
    ///
    /// `worktree` (minor 29, `start_worktree`) starts it in another worktree of
    /// that folder's repository instead; absent, it starts in `path` itself.
    ///
    /// `model` (minor 33) is the `value` of one of the
    /// [`AgentAvailability::models`] this computer listed for `agent`, given to the
    /// agent as `--model=<value>`. Absent, or Claude Code's own `default`, the agent
    /// is given no `--model` and starts with the model its settings name. A daemon
    /// below 33 lists no models, so a client sends it only for one listed.
    ///
    /// `trust` is the person's answer to "trust this folder?". The first start in a
    /// folder this computer keeps no answer for, sent without one, is answered
    /// [`StartSessionResult::NeedsTrust`] and runs nothing; the app asks, then sends
    /// a new start, with a new `request_id`, carrying the answer. An answer given is
    /// kept for the folder, replacing any earlier one.
    ///
    /// `attachments` (minor 50, `uploads`) are this connection's uploads sent without a
    /// `session_id`, in order, at most [`MAX_ATTACHMENTS`]. Once the run is named, before its
    /// first message is written, they move into its uploads folder, and the first message
    /// carries each as its absolute path on a line after "Attached:"; with no words, those
    /// lines are the first message. A start that cannot take one of them takes none and
    /// starts nothing; they stay this connection's. The start has no payload hash: its
    /// `request_id` alone makes it idempotent, as above, and the attachments are the first
    /// answer's.
    StartSession {
        request_id: String,
        path: String,
        agent: crate::agent::AgentKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<StartWorktree>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trust: Option<crate::host::Trust>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// **Every folder's answer to "trust this folder?"**, answered with one
    /// [`ServerMessage::FolderTrust`] carrying the same `request_id`.
    ListFolderTrust {
        request_id: String,
    },
    /// **Changes one folder's answer**, or keeps it if there was none. `path` is
    /// as [`ServerMessage::FolderTrust`] or [`StartSessionResult::NeedsTrust`] gave
    /// it. Answered with one [`ServerMessage::FolderTrust`].
    SetFolderTrust {
        request_id: String,
        path: String,
        trust: crate::host::Trust,
    },
    /// **Forgets one folder's answer**, so the next start there asks again. `path`
    /// is as [`ServerMessage::FolderTrust`] gave it, so a folder that no longer
    /// exists can still be forgotten. Answered with one [`ServerMessage::FolderTrust`].
    ForgetFolderTrust {
        request_id: String,
        path: String,
    },
    /// **Each agent's accounts on this computer, and the one its starts use** (minor 48, `accounts`),
    /// answered with one [`ServerMessage::Accounts`] carrying the same `request_id`. Paired devices only.
    ListAccounts {
        request_id: String,
    },
    /// **Adds an account for `agent`**, labelled `label`, with an id the computer makes; it is not made the active
    /// one. Answered with one [`ServerMessage::Accounts`].
    AddAccount {
        request_id: String,
        agent: crate::agent::AgentKind,
        label: String,
    },
    /// **Makes `account_id` the account `agent`'s starts use**, or its Default (the agent as it is on the computer)
    /// for none. An id that is not one of `agent`'s accounts is refused. Each of `agent`'s sessions running then moves
    /// to it at the person's next message, when that finds it certainly quiet (its turn over, nothing of the person's
    /// unanswered, no card waiting): its process is ended and the message starts it again on the account active then,
    /// in the same session (minor 52). Nothing is stopped by the choice itself. It reads live throughout, with
    /// [`crate::event::SessionSummary::switching`] saying where its move stands. Answered with one
    /// [`ServerMessage::Accounts`].
    ActivateAccount {
        request_id: String,
        agent: crate::agent::AgentKind,
        #[serde(default)]
        account_id: Option<String>,
    },
    /// **Signs `account_id` in with its agent's own login, on this computer**: the agent opens the sign-in in a
    /// browser here. Answered at once with one [`ServerMessage::Accounts`] in which that account is `signing_in`; when
    /// the login ends (finished in the browser, replaced by a newer sign-in, or after ten minutes) the list is pushed
    /// with its `signed_in` as it then is and `signing_in` gone. One sign-in at a time per computer: a new one
    /// replaces the one going. Codex runs `codex login` in the account's home; Claude Code runs `claude auth login` with
    /// its storage and config directories the account's folder.
    SignInAccount {
        request_id: String,
        account_id: String,
    },
    /// **Removes `account_id`**: its agent's own logout in its home, then the account is gone from the list; an agent
    /// whose active account it was goes back to its Default. A thread started on it still resumes on another account.
    /// Answered with one [`ServerMessage::Accounts`], or [`ServerMessage::AccountsRefused`] when the agent could not
    /// sign it out (nothing is removed then). Codex runs `codex logout`, Claude Code `claude auth logout`, each in the
    /// account's own folder.
    RemoveAccount {
        request_id: String,
        account_id: String,
    },
    /// **Signs `agent` in with its own sign-in, on this computer** (minor 51): Antigravity's, which keeps one sign-in
    /// and no accounts of CodeConnect's; agy opens Google's sign-in in a browser here. Answered at once with one
    /// [`ServerMessage::Accounts`] whose [`AgentSignIn`] for it is `signing_in`; when the sign-in ends (finished in the
    /// browser, replaced by a newer sign-in, or after ten minutes) the list is pushed with its sign-in as it then is.
    /// It takes the computer's one sign-in at a time, as [`ClientMessage::SignInAccount`] does. Any other agent is
    /// refused `bad_request`.
    SignInAgent {
        request_id: String,
        agent: crate::agent::AgentKind,
    },
    /// **Signs `agent` out of this computer** (minor 51): Antigravity's own logout. Answered with one
    /// [`ServerMessage::Accounts`], or [`ServerMessage::AccountsRefused`] when agy could not sign out. Any other agent
    /// is refused `bad_request`.
    SignOutAgent {
        request_id: String,
        agent: crate::agent::AgentKind,
    },
    /// **The git worktrees and branches of one folder** (minor 29,
    /// `start_worktree`), answered with one [`ServerMessage::ProjectGit`]. Read
    /// only: nothing is fetched, written or run. `path` is absolute.
    GetProjectGit {
        path: String,
    },
    /// **The folders in one folder** (minor 30, `browse_folders`), answered with one
    /// [`ServerMessage::FolderListing`] carrying the same `request_id`. Absent,
    /// `path` is the home folder. Names starting with `.` are left out unless
    /// `show_hidden`. Read only.
    ListFolder {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        show_hidden: bool,
    },
    /// **Make one folder, as `mkdir` does** (minor 30, `browse_folders`): `name`
    /// in `parent`, one level, answered with one [`ServerMessage::CreateFolderResult`].
    CreateFolder {
        request_id: String,
        parent: String,
        name: String,
    },
    /// **This computer's CPU, memory and disk, as one sample** (minor 31,
    /// `machine_stats`), answered with one [`ServerMessage::MachineStats`].
    GetMachineStats,

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
        /// What the owner calls the computer this daemon runs on — on a Mac, its
        /// computer name; on Linux, its hostname. Absent when it could not be read;
        /// the phone then names the machine by the host it paired with. Minor 25.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine_name: Option<String>,
        /// The computer's operating system: `"macos"` or `"linux"`. Absent when the
        /// daemon does not say. Minor 25.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine_os: Option<String>,
        /// Which daemon this is, whatever address reached it: minted once into
        /// the daemon's home and the same on every handshake. A phone that paired
        /// the same daemon under two addresses knows by this that they are one
        /// machine. Minor 26.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine_id: Option<String>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Event {
        event: Event,
    },
    /// The end of one [`ClientMessage::History`] page, sent exactly once per
    /// request after the page's events. `session_id` and `request_id` are echoed
    /// as the client sent them.
    ///
    /// `first_seq` is the lowest seq the page sent, or `before_seq` when it sent
    /// none. `error` says why the page is incomplete — an unknown run, a store
    /// failure, or a run removed while its page was being sent — and a page that
    /// carries one may have sent some of its events first.
    HistoryEnd {
        session_id: String,
        request_id: String,
        before_seq: u64,
        first_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
    /// What became of a [`ClientMessage::StopRun`].
    StopRunResult {
        session_id: String,
        request_id: String,
        result: StopRunResult,
    },
    /// What became of a [`ClientMessage::SetModel`] or [`ClientMessage::SetMode`].
    SettingResult {
        session_id: String,
        request_id: String,
        result: SettingResult,
    },
    TestPushResult {
        request_id: String,
        result: TestPushResult,
    },
    SendTextResult {
        session_id: String,
        result: SendTextResult,
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
        /// Every changed file (minor 53): the tracked ones against `HEAD` and the untracked
        /// ones, each with its lines, whole even when `unified` was cut. Absent when git could
        /// not list them, outside a repository, and from an older daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<Vec<DiffFile>>,
        /// `files` stops short of the tree's changes: more than [`MAX_DIFF_FILES`], more
        /// untracked names than the daemon lists at once, or more than one message carries.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        files_truncated: bool,
    },
    /// The answer to [`ClientMessage::GetRunGit`]. `git` is absent, with a `note` saying why,
    /// outside a repository, for an unknown run, and when git could not answer.
    RunGit {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git: Option<RunGit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// What became of a [`ClientMessage::GitAction`]. A client reads the run's repository
    /// again after it ([`ClientMessage::GetRunGit`]): a read answered before this one was
    /// read before the action.
    GitActionResult {
        request_id: String,
        session_uid: String,
        result: GitActionResult,
    },
    /// The answer to [`ClientMessage::GetConflicts`]. `note` alone: no comparison (an unknown
    /// run, no repository, no head, no base, a git without `merge-tree --write-tree`, unrelated
    /// histories). Both: the comparison is with the base as last fetched, and `note` says why
    /// it could not be fetched now.
    Conflicts {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conflicts: Option<RunConflicts>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The answer to [`ClientMessage::GetConflict`]. `hunks` empty: the file conflicts with no
    /// lines to show (binary, deleted on one side, renamed). Absent, with a `note`: the file
    /// could not be read (no conflict in that merge, over 1 MB, marker-like lines of its own, its
    /// commits gone).
    Conflict {
        session_id: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hunks: Option<Vec<ConflictHunk>>,
        /// `false`: the reply holds the hunks that fit, in order, and the file has more.
        #[serde(default)]
        hunks_complete: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Error {
        code: String,
        message: String,
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
    /// The outcome of a [`ClientMessage::Compose`]. Typed like every other mutation
    /// result so a retry replays a recorded outcome rather than speaking twice.
    ComposeResult {
        session_id: String,
        request_id: String,
        result: ComposeResult,
    },
    /// The outcome of one [`ClientMessage::PutFile`] chunk (minor 50).
    PutFileResult {
        upload_id: String,
        result: PutFileResult,
    },

    /// A [`ClientMessage::TerminalOpen`] took the run's shell. Output follows as [`ServerMessage::TerminalOutput`].
    TerminalOpened {
        attachment_id: String,
        /// Input the client may send before a [`ServerMessage::TerminalCredit`] gives more back.
        input_credit: u32,
        max_chunk_bytes: u32,
        max_outstanding_credit: u32,
        /// The shell's folder: the run's, as the daemon holds it.
        folder: String,
        /// The offset the output already held reaches: what comes before it is replayed, so a client sends no
        /// answer of its own to it (a terminal answers some sequences it is fed).
        replay_end: u64,
    },
    /// The shell's output, base64, at most [`MAX_TERMINAL_CHUNK_BYTES`] decoded and never more than the output
    /// credit left. The first after [`ServerMessage::TerminalOpened`] has `reset` unless the open resumed:
    /// the client clears its screen before feeding it.
    TerminalOutput {
        attachment_id: String,
        data: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reset: bool,
        /// The output offset after this chunk: what a later open's `resume_after` names.
        offset: u64,
    },
    /// `bytes` of the client's input were written to the shell: it may send as many more.
    TerminalCredit {
        attachment_id: String,
        bytes: u32,
    },
    /// The attachment is over, `reason` one of [`terminal_closed`]. Nothing more of it follows.
    TerminalClosed {
        attachment_id: String,
        reason: String,
    },

    /// The answer to [`ClientMessage::ListProjects`]: most recently used first.
    /// `agents` says which agents this computer can start, for every project. An
    /// answer from the last agent check is followed, unasked, by another to the
    /// device that asked, when the fresh check it started finds the agents changed:
    /// a reader keeps the latest.
    Projects {
        projects: Vec<Project>,
        agents: Vec<AgentAvailability>,
    },
    /// The outcome of a [`ClientMessage::StartSession`]. A repeat of the same
    /// `request_id` gets the same outcome again.
    StartSessionResult {
        request_id: String,
        result: StartSessionResult,
    },
    /// The answer to [`ClientMessage::ListFolderTrust`], [`ClientMessage::SetFolderTrust`]
    /// and [`ClientMessage::ForgetFolderTrust`]: every answer this computer keeps,
    /// newest first, once the request's change is made.
    FolderTrust {
        request_id: String,
        folders: Vec<FolderAnswer>,
    },
    /// Every account, oldest first, and the one each agent's starts use. The answer to
    /// [`ClientMessage::ListAccounts`], [`ClientMessage::AddAccount`], [`ClientMessage::ActivateAccount`],
    /// [`ClientMessage::SignInAccount`] and [`ClientMessage::RemoveAccount`] once the request's change is made, naming
    /// its `request_id`; and pushed, with no `request_id`, to every paired device whenever the list changes (an
    /// account added, chosen or removed from any device, a sign-in started or ended). A reader keeps the latest. An
    /// agent missing from `active` uses its Default.
    Accounts {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        accounts: Vec<AgentAccount>,
        active: Vec<ActiveAccount>,
        /// Each agent's Default with what it says of it, once read; omitted when none is.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        defaults: Vec<DefaultAccount>,
        /// The sign-in of each agent that keeps one and no accounts of CodeConnect's (minor 51: Antigravity), as
        /// last read; omitted when none is.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        agent_sign_ins: Vec<AgentSignIn>,
    },
    /// A refused account request (any of [`ClientMessage::ListAccounts`], [`ClientMessage::AddAccount`],
    /// [`ClientMessage::ActivateAccount`], [`ClientMessage::SignInAccount`], [`ClientMessage::RemoveAccount`]), naming
    /// the request, as a refused start or folder names its own: nothing changed. `code` is `bad_request` (an agent with
    /// no accounts, an id not the agent's or not one of the computer's, a name outside 1 to 64 characters, an agent
    /// whose sign-in or removal is not offered here yet, Codex not installed), `accounts_in_use` (a removal while a
    /// session runs on the account) or `accounts_unavailable` (the computer could not read or keep its accounts, or
    /// the agent could not sign the account out); `message` says which, in words.
    AccountsRefused {
        request_id: String,
        code: String,
        message: String,
    },
    /// The answer to [`ClientMessage::GetProjectGit`]; `git` is absent when
    /// `path` is not in a git repository.
    ProjectGit {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git: Option<ProjectGit>,
    },
    /// The answer to [`ClientMessage::ListFolder`]. `path` is the folder's real
    /// path, symlinks resolved, as Finder shows it: the phone names its children
    /// and its parent from it.
    FolderListing {
        request_id: String,
        path: String,
        #[serde(flatten)]
        contents: FolderContents,
    },
    /// The answer to [`ClientMessage::CreateFolder`].
    CreateFolderResult {
        request_id: String,
        result: CreateFolderResult,
    },
    /// The answer to [`ClientMessage::GetMachineStats`]: one sample, taken when
    /// asked. Each value is absent when this computer could not read it. The same
    /// definitions on macOS and Linux:
    MachineStats {
        /// The share of all cores busy over half a second, from 0 to 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpu: Option<f64>,
        /// Bytes of memory not available for new work: total less available.
        /// On macOS what Activity Monitor calls Memory Used (app memory, wired
        /// and compressed); on Linux `MemTotal - MemAvailable`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory_used: Option<u64>,
        /// Bytes of physical memory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory_total: Option<u64>,
        /// Bytes in use on the volume holding the daemon user's home folder:
        /// its size less what this user can still write, so blocks reserved
        /// for root count as used.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disk_used: Option<u64>,
        /// Bytes on that volume.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disk_total: Option<u64>,
        captured_at: String,
    },
    /// The owner's other computers this one can reach on Tailscale, and this
    /// computer's own Tailscale state. Built by [`crate::tailnet::peers_frame`].
    Peers {
        tailnet: TailnetState,
        /// This computer's Tailscale login, when Tailscale is running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<String>,
        /// This computer is a tagged device, which Tailscale vouches for to no
        /// one: every connection to it needs a code.
        tagged: bool,
        computers: Vec<PeerComputer>,
    },

    Pong,
}

/// Tailscale on this computer, as [`ServerMessage::Peers`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailnetState {
    Running,
    /// Installed, but stopped, signed out, or not answering.
    NotRunning,
    NotInstalled,
}

/// One computer in [`ServerMessage::Peers`]: where its daemon listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerComputer {
    /// The first label of its MagicDNS name, as Mail names it.
    pub name: String,
    /// Its whole MagicDNS name, or its IPv4 address when it has none.
    pub host: String,
    pub port: u16,
}

/// What a [`ServerMessage::FolderListing`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FolderContents {
    /// The names of its folders, and of its symlinks to folders, in byte order: at
    /// most [`MAX_FOLDER_NAMES`], the first ones, with `truncated` saying there were
    /// more.
    Listed {
        folders: Vec<String>,
        truncated: bool,
    },
    /// It could not be read. `error` is the system's own words, as `ls` prints
    /// them (`Permission denied`), safe to show verbatim. On macOS an `Operation
    /// not permitted` in Desktop, Documents or Downloads is followed by where to
    /// change the answer.
    Unreadable { error: String },
}

/// The most names one [`ServerMessage::FolderListing`] carries: well inside the
/// phone's message limit even when every name is as long as a name can be.
pub const MAX_FOLDER_NAMES: usize = 2000;

/// How a [`ClientMessage::CreateFolder`] went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CreateFolderResult {
    Created,
    /// No folder was made. `detail` is the system's own words (`File exists`),
    /// safe to show verbatim, with the same addition as
    /// [`FolderContents::Unreadable`].
    Failed {
        detail: String,
    },
}

/// Where in a folder's repository a [`ClientMessage::StartSession`] runs. Its
/// main folder is never switched: another branch gets a worktree of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartWorktree {
    /// In a worktree [`ProjectGit::worktrees`] listed, by its `path`.
    Existing { path: String },
    /// In a new worktree for a local branch [`ProjectGit::branches`] listed.
    Branch { branch: String },
    /// In a new worktree named `name`: Claude makes it (`claude --worktree <name>`,
    /// on its branch `worktree-<name>`, each `/` written `+`); for Codex and Antigravity it is made in the
    /// same place, on a new branch named `name`. A name the repository already has
    /// (that branch, a branch clashing with its path, or the worktree's folder) goes
    /// as `<name>-2`, `<name>-3`, …: the computer sees every branch, the app only
    /// the newest [`MAX_PROJECT_BRANCHES`].
    New { name: String },
}

/// One folder's repository, as [`ClientMessage::GetProjectGit`] reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGit {
    /// `git worktree list`, in git's order: the main checkout first.
    pub worktrees: Vec<Worktree>,
    /// Local branches checked out in no worktree, newest commit first, at most
    /// [`MAX_PROJECT_BRANCHES`].
    pub branches: Vec<String>,
    /// More branches exist than `branches` holds.
    pub branches_truncated: bool,
}

/// The most branches one [`ProjectGit`] lists.
pub const MAX_PROJECT_BRANCHES: usize = 100;

/// One worktree of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    /// The repository's main checkout.
    pub main: bool,
    /// Absent on a detached head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// It has uncommitted changes, untracked files included.
    pub dirty: bool,
    /// git lists it, but its folder is gone; nothing can start there.
    pub missing: bool,
}

/// A folder sessions have run in, from every session this computer has hosted,
/// including ended and removed ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The folder, exactly as the sessions recorded it. A
    /// [`ClientMessage::StartSession`] names it by this string.
    pub path: String,
    /// What the fleet calls the project.
    pub name: String,
    /// When a session last started there (RFC 3339).
    pub last_used: String,
    /// The agent of that session.
    pub last_agent: crate::agent::AgentKind,
    /// The folder's current git branch; absent outside a git repository or on a
    /// detached head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// False when the folder is gone: listed, but a start there fails.
    pub exists: bool,
}

/// Whether this computer can start one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAvailability {
    pub agent: crate::agent::AgentKind,
    pub available: bool,
    /// Why a start would be refused: for an agent not available, and for a
    /// signed-out Claude Code. Absent when a start goes ahead. May include output
    /// from other programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The models the agent offers a start (minor 33), in its own order and its
    /// own words. Claude Code lists its default first, as `default`, its detail
    /// naming the model that is now; Codex lists no such entry, and a start that
    /// names no model gets its default. Empty when the agent could not be asked,
    /// for a signed-out Claude Code, which is not asked, and from a daemon below 33.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<AgentModel>,
    /// Whether the agent is signed in (minor 33), to show; absent when not known.
    /// A signed-out Claude Code is refused at the start; a signed-out Codex asks for
    /// a sign-in itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<bool>,
}

/// One model an agent can be started with (minor 33), as the agent names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModel {
    /// What a [`ClientMessage::StartSession`] names it by, and the agent is given
    /// as `--model=<value>`.
    pub value: String,
    /// The agent's own name for it.
    pub label: String,
    /// The agent's own description; absent when it gives none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What became of a [`ClientMessage::StartSession`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StartSessionResult {
    /// The session exists; `session_uid` is how the fleet names it. It may reach
    /// the fleet a moment after this answer. `cwd` is the folder it started in
    /// (minor 29), absent when the agent makes that folder itself (a new Claude
    /// worktree): the fleet shows it once the agent is there.
    Started {
        session_uid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// The start was refused, or its success could not be confirmed; `detail` says
    /// which, and may include output from other programs.
    /// A Claude run whose host lost its link after ccd sent it `start`, before saying
    /// how the start went, stays in the fleet as `starting` while its host runs, and
    /// becomes `failed` once its host is gone.
    Failed {
        reason: StartFailure,
        detail: String,
    },
    /// Nothing ran: this computer keeps no answer to "trust this folder?" for
    /// `path`, the folder's real path, and the start gave none.
    NeedsTrust { path: String },
}

/// One of an agent's accounts on a computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAccount {
    /// The computer's own id for it.
    pub id: String,
    pub agent: crate::agent::AgentKind,
    pub label: String,
    /// Whether its agent says it is signed in, asked with the account's home; absent when not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<bool>,
    /// A sign-in for it is going on this computer, waiting on the browser. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signing_in: bool,
    /// Its agent says it is signed in, but the sign-in is not this account's own: Codex keeps its credential in the
    /// computer's credential store (`cli_auth_credentials_store`), not in the account's home, so the account is not
    /// separate. `signed_in` then reads false. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_separate: bool,
    /// What its agent says of the account, read on demand ([`AccountFacts`]); absent until read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<AccountFacts>,
    /// Another of its agent's accounts on this computer, its Default included, is signed in as the same email: the
    /// two are one account. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub same_as_another: bool,
}

/// What an agent says of one of its accounts, read from it on demand (Codex: `account/read` and
/// `account/rateLimits/read` in that account's home) and kept in the computer's memory only: never stored, never
/// logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountFacts {
    /// The address it is signed in as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Its plan, in the agent's own words (Codex's `planType`: `plus`, `pro`, …; Antigravity's tier name, as agy
    /// names it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Each usage window the agent names, as it names it (of Antigravity's, only the Gemini models' week); a window it
    /// does not name is absent, so an app labels a figure from its own `window_minutes`, never from a guess.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<UsageWindow>,
    /// Whether plan limits apply to this sign-in, where the agent says (Claude Code's `rate_limits_available`): false
    /// for a sign-in with none (an API key's), so an app says "no plan limits"; absent while not reported yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits_available: Option<bool>,
    /// When the computer read these (RFC 3339), so an app says "as of".
    pub read_at: String,
}

/// One usage window, as the agent reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// The share of the window used, in percent.
    pub used_percent: u32,
    /// The window's length, in minutes.
    pub window_minutes: u64,
    /// When the window resets (Unix seconds); 0 for a window the agent has not opened yet (Antigravity's, unused).
    pub resets_at: i64,
}

/// An agent's Default account (the agent as it is on the computer), with what it says of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultAccount {
    pub agent: crate::agent::AgentKind,
    pub facts: AccountFacts,
}

/// The sign-in of an agent that keeps one and no accounts of CodeConnect's (minor 51: Antigravity's), its plan in
/// [`ServerMessage::Accounts`]' `defaults`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSignIn {
    pub agent: crate::agent::AgentKind,
    /// Whether it is signed in, as it last said; absent while not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<bool>,
    /// A sign-in for it is going on this computer, waiting on the browser. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signing_in: bool,
}

/// The account an agent's starts use now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAccount {
    pub agent: crate::agent::AgentKind,
    pub account_id: String,
}

/// One folder's answer to "trust this folder?", kept by the computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderAnswer {
    /// The folder's real path, symlinks resolved.
    pub path: String,
    pub trust: crate::host::Trust,
    /// When it was answered, RFC 3339.
    pub answered_at: String,
}

/// Why a start failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartFailure {
    /// The agent is not installed on this computer, or cannot be found.
    AgentMissing,
    /// The folder no longer exists.
    FolderMissing,
    /// Anything else, the detail says what.
    LaunchFailed,
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
    /// The reason is ccd's sentence, which can carry the app-server's own turn
    /// status. It says the run cannot be stopped from here (a Claude session
    /// CodeConnect did not start is told where its stop control actually is), the
    /// control link is not watching the turn's thread so no stop could be
    /// confirmed, the hash does not bind this ask to the turn it names, or a
    /// settled claim that stopped nothing is being replayed.
    Rejected { reason: String },
    /// Issued, outcome unknown — the daemon was killed between claiming the
    /// interrupt and observing the turn terminate. Never retried automatically.
    Indeterminate { reason: String },
}

/// What became of one [`ClientMessage::PutFile`] chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PutFileResult {
    /// Written. `received` is the bytes held so far: the next chunk's `offset`, or the
    /// file's `size` once it is complete.
    Received { received: u64 },
    /// Refused; the client sends the file again under a new id. Nothing is kept of an upload a
    /// bound refused, and one this connection leaves unattached goes when it closes. The reason
    /// is human text.
    Refused { reason: String },
}

/// What became of a [`ClientMessage::Compose`].
///
/// **Five statuses, and the first two are the ones a client must not collapse.** Which
/// of them arrives is the answer to "what did my words do" — they began a turn, or they
/// joined one already running — and it is decided by what the session was doing at the
/// instant the daemon wrote, not by anything the phone could know when it tapped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ComposeResult {
    /// The thread was idle and these words began a turn. `turn_id` is that turn, as the
    /// app-server named it in `turn/started`.
    Started { turn_id: String },
    /// A turn was running and these words joined it. `turn_id` is that turn — the SAME
    /// turn, not a new one: a steer produces no `turn/started` and the only terminal is
    /// the original turn's.
    Steered { turn_id: String },
    /// This exact compose already ran; nothing was said a second time. It replays the
    /// recorded outcome, including which of `started`/`steered` it was — the route is
    /// snapshotted at the claim, so a retry after the session moved reports what actually
    /// happened rather than what would happen now.
    Duplicate {
        turn_id: String,
        /// Whether the original was a start or a steer, so a client can render the
        /// replay with the same words it rendered the first time.
        started: bool,
    },
    /// Refused, with a reason: **nothing was said by this ask, and the record says so.**
    /// The clean complement of [`Self::Indeterminate`], exactly as it is for
    /// [`InterruptResult::Rejected`].
    ///
    /// The reason is human text, safe to show verbatim. **It never carries an id the
    /// phone did not send**: the app-server's own refusal for a stale steer names the
    /// turn the session is really running, and neither the broker's refusal nor this
    /// daemon's passes that on.
    Rejected { reason: String },
    /// Written, outcome unknown — the daemon was killed between claiming the compose and
    /// learning what it did. Never retried automatically: saying something twice is not a
    /// recoverable mistake, and a person who can see the screen is a better judge.
    Indeterminate { reason: String },
}

/// Ceiling on a `diff` payload. A phone screen cannot use more, and an
/// unbounded diff would blow past the client message limit on the way out.
pub const MAX_DIFF_BYTES: usize = 512 * 1024;

/// Ceiling on the files one `diff` lists ([`ServerMessage::Diff`]'s `files`).
pub const MAX_DIFF_FILES: usize = 1000;

/// One changed file of a [`ServerMessage::Diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    /// From the repository's top folder.
    pub path: String,
    /// Where a renamed or copied file came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: DiffFileStatus,
    /// Lines added and removed, as `git diff --numstat` counts them; an untracked file's
    /// every line is added. Absent for a binary file, for an untracked one that is not a
    /// regular file, and for one past the daemon's reading budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
}

/// What happened to a [`DiffFile`], from git's own status letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    /// A file became a symlink, or the other way round.
    TypeChanged,
    /// Not tracked by git yet, and not ignored.
    Untracked,
}

/// A run's repository, as [`ClientMessage::GetRunGit`] reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGit {
    /// The branch checked out. Absent on a detached head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit checked out. Absent before the first commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    /// The branch this one is measured against: the default branch of the remote named
    /// `upstream` (a fork's parent), else of the upstream's remote, else of `origin`, else of
    /// the only remote, as its `refs/remotes/<remote>/HEAD` names it. Absent when no such
    /// remote HEAD is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<RunGitBase>,
    /// The changed files, as [`ServerMessage::Diff`]'s `files` count them: every one, before
    /// that reply's row and frame limits.
    pub changed: ChangedFiles,
    /// Commits on `HEAD` that are not on [`RunGitBase::reference`]. Absent without a head or
    /// a base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    /// The push remote git names for this branch (its `pushRemote`, `remote.pushDefault`, or
    /// the upstream's), else `origin`, has the branch of this name at `HEAD`.
    pub pushed: bool,
    /// The index holds unresolved merge entries.
    pub unmerged: bool,
    pub read_at: String,
    /// Whether this computer's `gh` can read the repository on github.com (minor 60). Absent
    /// from an older daemon and on a detached head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh: Option<GhState>,
    /// The GitHub host the repository is on: `github.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_host: Option<String>,
    /// The branch's pull request: the open one from this branch, else the newest closed or
    /// merged one whose head is the commit checked out. Absent when there is none, and when
    /// `gh` is not signed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequest>,
    /// When GitHub was asked: a read within 30 s of one for the same branch and commit is
    /// answered from it. Absent when GitHub was not asked: no `gh`, no account in it,
    /// or its rate limit pausing the host's reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_read_at: Option<String>,
    /// The repository on `gh_host` a pull request from this branch goes into, `owner/name`: the
    /// remote named `upstream` (a fork's parent), else `origin`, else the push remote
    /// (minor 61). Absent when no remote names a github.com repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_repo: Option<String>,
}

/// Whether `gh` can read a [`RunGit`]'s repository on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhState {
    SignedIn,
    SignedOut,
    /// No `gh` on this computer.
    Missing,
    /// GitHub could not be reached, or asked to wait (a rate limit).
    Unknown,
    /// The repository's remotes name no github.com repository (GitHub Enterprise hosts and SSH
    /// host aliases among them).
    NoGithubRemote,
}

/// A [`RunGit`]'s pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    /// The branch it would merge into.
    pub base: String,
    /// The commit its head is at.
    pub head_oid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    /// An open one's checks at its head (minor 62), sorted failed, pending, cancelled, unknown,
    /// passed, skipped, then by name; at most 100. Absent when not read: a closed or merged
    /// pull request, an older daemon, or GitHub not answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<Check>>,
    /// False when there were more than 100, or the head moved while they were read (then
    /// `checks` is empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_complete: Option<bool>,
    /// An open one's merge state as GitHub reports it (minor 64); `unknown` while GitHub is still
    /// working it out, as it is just after a push. Absent when not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_state: Option<MergeState>,
    /// The merge methods the repository allows, in this order: merge, squash, rebase (minor 64).
    /// Absent when not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_methods: Option<Vec<MergeMethod>>,
    /// An open one waits to be merged: a merge queue holds it, or GitHub's auto-merge will
    /// queue it, as a merge on a branch whose queue is required does (minor 64). Absent when not
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queued: Option<bool>,
}

/// A [`PullRequest`]'s merge state: GitHub's `mergeStateStatus`, `draft` from `isDraft`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeState {
    Clean,
    Blocked,
    Behind,
    Dirty,
    Unstable,
    Draft,
    HasHooks,
    /// GitHub has not worked it out yet, or a state this daemon does not name.
    Unknown,
}

/// A way to merge a [`PullRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

/// One of a [`PullRequest`]'s checks: a check run, or a commit status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// A check run's name, or a status's context.
    pub name: String,
    pub state: CheckState,
    /// A check run's start; a status has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Its log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A failed check run's first failure annotation's first line, when it says more than the
    /// runner's "Process completed with exit code N.".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_line: Option<String>,
}

/// A [`Check`]'s state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Fail,
    Pending,
    Cancelled,
    /// A state this daemon does not name.
    Unknown,
    Pass,
    Skipped,
}

/// A [`PullRequest`]'s state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// One action of [`ClientMessage::GitAction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitAction {
    /// Every change in the run's folder, tracked and untracked, committed on its branch with
    /// `message` (at most [`MAX_COMMIT_MESSAGE_BYTES`]), the repository's hooks and signing
    /// running as they do for the person's own `git commit`. Files a
    /// [`crate::event::notice::COMMITTED`] notice into the run's conversation.
    Commit { message: String },
    /// The branch pushed to its push remote (no force) and a pull request opened from it into
    /// [`RunGit::gh_repo`], titled and described by gh from its commits (minor 61). An open one
    /// from the branch already there is the answer, and nothing is pushed.
    CreatePr,
    /// The run's open pull request merged by `method` on GitHub, only while its head is
    /// `head_oid`, the commit the panel showed, when GitHub accepts the merge (minor 64). On a
    /// branch whose merge queue is required that is when it is queued: a later push to the queued
    /// pull request can change what lands. Nothing local changes, and no branch is deleted.
    Merge {
        method: MergeMethod,
        head_oid: String,
        /// The pull request the panel showed: another one from the branch is not merged.
        number: u64,
    },
}

/// Ceiling on a [`GitAction::Commit`]'s message, the same as a typed message's.
pub const MAX_COMMIT_MESSAGE_BYTES: usize = MAX_SEND_TEXT_BYTES;

/// What became of a [`ClientMessage::GitAction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GitActionResult {
    /// Done: for a commit, `sha` is the commit made; for a pull request, `pr` is it.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr: Option<PrLink>,
    },
    /// Not done, with one of [`git_refusal`]'s codes and, for some, git's own words.
    Refused {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// A pull request a [`GitAction::CreatePr`] opened or found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLink {
    pub number: u64,
    pub url: String,
}

/// The codes of a refused [`GitActionResult`]; an app words each itself.
pub mod git_refusal {
    /// The request was malformed, named another action under an id already used, or a
    /// message that is empty, too long or holds a NUL.
    pub const BAD_REQUEST: &str = "bad_request";
    /// No such run on this computer.
    pub const UNKNOWN_SESSION: &str = "unknown_session";
    /// The run is working, or waits on a card.
    pub const BUSY: &str = "busy";
    /// The run's folder is not in a git repository.
    pub const NOT_A_REPOSITORY: &str = "not_a_repository";
    /// No branch is checked out.
    pub const DETACHED: &str = "detached";
    /// A merge has unresolved files.
    pub const UNMERGED: &str = "unmerged";
    /// Nothing changed since the last commit.
    pub const NOTHING_TO_COMMIT: &str = "nothing_to_commit";
    /// Git refused or failed; `detail` is its first line (a hook's, a signer's).
    pub const GIT_FAILED: &str = "git_failed";
    /// A step took past its time; `detail` names it. Nothing was committed.
    pub const TIMEOUT: &str = "timeout";
    /// Whether it was done is not known (an earlier try under this id did not finish, or
    /// git did not answer after it may have committed). Never tried again under this id: the
    /// run's repository, read again, shows what git has.
    pub const INDETERMINATE: &str = "indeterminate";
    /// No `gh` on this computer.
    pub const GH_MISSING: &str = "gh_missing";
    /// `gh` has no account for the repository's host, or GitHub refused its token.
    pub const GH_SIGNED_OUT: &str = "gh_signed_out";
    /// GitHub could not be reached, asked to wait, or gave no answer gh could read. Nothing was
    /// pushed.
    pub const GITHUB_UNAVAILABLE: &str = "github_unavailable";
    /// No remote names a github.com repository.
    pub const NO_GITHUB_REMOTE: &str = "no_github_remote";
    /// The branch checked out is the base branch itself.
    pub const ON_BASE_BRANCH: &str = "on_base_branch";
    /// No base branch is known (no `refs/remotes/<remote>/HEAD`, as a repository made by
    /// `git init` and pushed has none): nothing is pushed.
    pub const BASE_UNKNOWN: &str = "base_unknown";
    /// The branch has no commits the base lacks.
    pub const NOTHING_TO_PROPOSE: &str = "nothing_to_propose";
    /// gh refused to open the pull request; `detail` is its line, after "pushed; " when the
    /// branch was pushed first.
    pub const CREATE_FAILED: &str = "create_failed";
    /// No open pull request from the run's branch.
    pub const NO_PULL_REQUEST: &str = "no_pull_request";
    /// The repository does not allow that merge method.
    pub const METHOD_NOT_ALLOWED: &str = "method_not_allowed";
    /// The pull request is not the one the panel showed, or its head is not the commit the panel
    /// showed: nothing was merged.
    pub const HEAD_MOVED: &str = "head_moved";
    /// GitHub refused the merge; `detail` is gh's line (a rule, a review, a conflict).
    pub const MERGE_FAILED: &str = "merge_failed";
}

/// What [`ClientMessage::GetConflicts`] compared, and what conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConflicts {
    /// [`RunGit`]'s base.
    pub base: RunGitBase,
    /// The base's commit compared.
    pub base_oid: String,
    /// The commit checked out, compared.
    pub head_oid: String,
    /// When ccd fetched `base_oid`. Absent: it is the base as last fetched before, and the
    /// reply's `note` says why it was not fetched now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    /// Git merges the two with no conflict. `false` with no `files` is a conflict git names no
    /// file for (a folder renamed on one side).
    pub clean: bool,
    /// The conflicted paths, from the repository's top, at most 100.
    pub files: Vec<String>,
    pub files_complete: bool,
    pub read_at: String,
}

/// One conflicting stretch of a file: the lines on the branch checked out, then the base's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictHunk {
    pub ours: String,
    pub theirs: String,
}

/// A [`RunGit`]'s base branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGitBase {
    /// The branch's name on its remote, e.g. `main`.
    pub name: String,
    /// The remote, e.g. `origin`.
    pub remote: String,
    /// Its full local ref, e.g. `refs/remotes/origin/main`.
    #[serde(rename = "ref")]
    pub reference: String,
}

/// The changed files of a [`RunGit`], untracked ones included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFiles {
    pub files: u64,
    pub added: u64,
    pub removed: u64,
    /// Every changed file was listed and every one with lines was counted. False when the
    /// untracked names or the bytes read to count them ran past the daemon's limits, or git
    /// could not open a folder to list it: the numbers are then at least these.
    pub complete: bool,
}

/// What CodeConnect can actually do, reported honestly rather than assumed.
/// Mirrors ACP's capability shape; the iPhone disables affordances it does not
/// see advertised instead of failing at tap time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// True when an answer is guaranteed to reach the agent.
    pub can_approve_reliably: bool,
    pub send_text: bool,
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
    /// **Whether this connection's bytes are unreadable in transit**: it is TLS, or
    /// plaintext on loopback or the daemon's own tailnet address. False on a LAN the
    /// operator admitted with `ws_allow_plaintext`, which vouches for their network
    /// and not for the bytes on it. Connection-scoped, and absent reads as false: a
    /// daemon that does not say is never taken to claim privacy.
    #[serde(default)]
    pub private_transport: bool,
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
    /// Codex session whose `codex_link` is not `subscribed`, which refuses an ask with
    /// a sentence of its own. The name carries
    /// `codex_` for that reason: a bare `interrupt` read as a promise about whatever
    /// session the reader had in mind, which is the one thing this flag never was.
    ///
    /// **Per-session actuatability is `summary.agent` plus the session's link
    /// state, and BOTH are now on the summary.** The agent always was;
    /// `codex_thread_id` never answered the second half: it is the thread the run's row
    /// holds, and reads the same whether or not its link is attached.
    /// [`crate::event::SessionSummary::codex_link`] closes it: it says whether an ask
    /// aimed at the run can land. A client that wants to know whether THIS
    /// session can be stopped right now computes it from the fleet — this flag AND
    /// `agent == codex` AND `codex_link == subscribed`.
    ///
    /// The honest client rule from a daemon that does not send that field (below its
    /// minor, where it decodes as [`crate::event::CodexLink::None`]) is unchanged and
    /// is still the fallback: offer the button on a Codex session hosted by a daemon
    /// that advertises this, and let the refusal — which always names which of the
    /// conditions failed — be what the operator reads. A refusal is still the last
    /// word even with the field, since a link can move between the summary and the
    /// tap.
    #[serde(default)]
    pub codex_interrupt: bool,
    /// **This daemon honours a compose for a Codex session whose control link is
    /// `Subscribed`** — it really speaks to the model, and reports which turn heard it.
    ///
    /// Advertised for [`Capabilities::codex_interrupt`]'s reason, and it is a stronger
    /// one here: `compose` is a NEW message, so a daemon below minor 18 does not decode
    /// it at all and answers nothing. A phone that sent one and waited would wait for
    /// ever. Absent decodes `false`, which is exactly what such a daemon meant.
    ///
    /// **Connection-global and build-shaped**, with the same caveat spelled out on its
    /// sibling: it says what this daemon honours and nothing about one session. Compose
    /// exists only for Codex, so a client scopes the affordance by the session's
    /// [`crate::event::SessionSummary::agent`] and its
    /// [`crate::event::SessionSummary::codex_link`] as well — the per-session half its
    /// sibling's doc describes — and lets the refusal, which always names which
    /// condition failed, be what the operator reads when the link moves between the
    /// summary and the send.
    #[serde(default)]
    pub codex_compose: bool,
    /// The agents this daemon can actually host, named honestly. A client scopes
    /// what it offers to this set. Empty — from any daemon predating the agent seam — is
    /// read as `["claude"]`: Claude is the floor, and this list only ever *adds*
    /// to it. **Omitted entirely while it would only name Claude** — an empty
    /// list is skipped on the wire, so a daemon with nothing to add beyond the
    /// floor sends no field at all and an older phone renders nothing new. It is
    /// populated once the daemon can actually drive a second agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_agents: Vec<crate::agent::AgentKind>,
    /// **This daemon answers Claude's `AskUserQuestion` from the phone** (minor
    /// 21): its cards carry `question_hold`, it takes
    /// [`AnswerDecision::Answers`] and [`AnswerDecision::Decline`], and it
    /// refuses `allow` and `option` on a question card. Absent decodes `false`,
    /// which is what an older daemon meant: it shows the question as an ordinary
    /// approval.
    #[serde(default)]
    pub question_card: bool,
    /// **Every Claude card decision this daemon takes reaches the card's own
    /// call through the run's agent host (an earlier daemon: the hook Claude held
    /// for it), and nothing is ever typed** (minor 22). Its Claude cards carry `question_hold`. Absent decodes
    /// `false`: an older daemon
    /// types an approval's answer into whatever prompt is on screen, so a phone
    /// shows its Claude approval cards read-only.
    #[serde(default)]
    pub hook_only_approvals: bool,
    /// **This daemon serves earlier pages** ([`ClientMessage::History`]) and its
    /// sessions carry their open cards and current facts
    /// ([`crate::event::SessionSummary::pending`], `model`, `permission_mode`)
    /// (minor 23). Absent decodes `false`: an older daemon answers `history` with
    /// `bad_request`, so a client sends it only when this is advertised.
    #[serde(default)]
    pub history_pages: bool,
    /// **This daemon lists its projects and starts sessions in them**
    /// ([`ClientMessage::ListProjects`], [`ClientMessage::StartSession`], minor 28).
    /// Absent decodes `false`: an older daemon answers both with `bad_request`, so
    /// a client sends them, and offers a new session, only when this is advertised.
    #[serde(default)]
    pub start_session: bool,
    /// **A start can pick a branch or worktree** ([`ClientMessage::GetProjectGit`],
    /// [`ClientMessage::StartSession`]'s `worktree`, minor 29). Absent decodes
    /// `false`: an older daemon answers `get_project_git` with `bad_request` and
    /// starts in the main folder whatever `worktree` says.
    #[serde(default)]
    pub start_worktree: bool,
    /// **The phone can browse this computer's folders, make one, and start in any
    /// of them** ([`ClientMessage::ListFolder`], [`ClientMessage::CreateFolder`],
    /// minor 30). Absent decodes `false`: an older daemon answers both with
    /// `bad_request` and starts only in a folder [`ServerMessage::Projects`] listed.
    #[serde(default)]
    pub browse_folders: bool,
    /// **This daemon answers [`ClientMessage::GetMachineStats`]** (minor 31).
    /// Absent decodes `false`: an older daemon answers it with `bad_request`, so a
    /// client asks only when this is advertised.
    #[serde(default)]
    pub machine_stats: bool,
    /// **This daemon ends a Claude run it started** ([`ClientMessage::StopRun`],
    /// minor 35). Absent decodes `false`: an older daemon answers `stop_run` with
    /// `bad_request`, so a client sends it, and offers to end a run, only when this
    /// is advertised.
    #[serde(default)]
    pub stop_run: bool,
    /// **This daemon stops the running turn of a Claude run it started**
    /// ([`ClientMessage::Interrupt`], minor 37), through the run's agent host. Absent
    /// decodes `false`: an older daemon refuses every Claude Stop, so a client offers
    /// Stop on a Claude run only when this is advertised. It is
    /// [`Self::codex_interrupt`]'s twin for the other engine.
    #[serde(default)]
    pub claude_interrupt: bool,
    /// **This daemon keeps several accounts per agent** ([`ClientMessage::ListAccounts`],
    /// [`ClientMessage::AddAccount`], [`ClientMessage::ActivateAccount`], [`ClientMessage::SignInAccount`],
    /// [`ClientMessage::RemoveAccount`], and the pushed [`ServerMessage::Accounts`], minor 48), to paired devices only. Absent
    /// decodes `false`: an older daemon answers them with `bad_request`, so a client asks only when this is
    /// advertised.
    #[serde(default)]
    pub accounts: bool,
    /// **This daemon takes files for a session's agent** ([`ClientMessage::PutFile`] and the
    /// `attachments` of [`ClientMessage::SendText`] and [`ClientMessage::Compose`], minor 50),
    /// to paired devices only. Absent decodes `false`: an older daemon answers `put_file` with
    /// `bad_request`, so a client sends neither unless this is advertised.
    #[serde(default)]
    pub uploads: bool,
    /// **This daemon serves a run's shell** ([`ClientMessage::TerminalOpen`] and the terminal frames, minor 56),
    /// to a paired device on a connection nothing on the path can read (`private_transport`) only. Absent decodes
    /// `false`: an older daemon answers `terminal_open` with `bad_request`, so a client sends none unless this is
    /// advertised.
    #[serde(default)]
    pub run_terminal: bool,
    /// **This daemon answers [`ClientMessage::GetRunGit`]** (minor 58), to paired devices
    /// only. Absent decodes `false`: an older daemon answers it with `bad_request`.
    #[serde(default)]
    pub changes: bool,
    /// **The [`ClientMessage::GitAction`] kinds this daemon takes** (minor 59, `commit`), to
    /// paired devices only; absent or empty, none. Named one by one so each later action is
    /// offered only by a daemon that has it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_actions: Vec<String>,
    /// **This daemon answers [`ClientMessage::GetConflicts`] and
    /// [`ClientMessage::GetConflict`]** (minor 66), to paired devices only. Absent decodes
    /// `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflicts: bool,
}

/// The most events one [`ClientMessage::History`] page carries, whatever its
/// `limit` asks for.
pub const MAX_HISTORY_PAGE: u32 = 500;

/// How the daemon applies an answer on the *installed* Claude Code build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerPath {
    /// Returned to Claude through a channel that confirms it: the hook Claude
    /// holds for the call, or, for a run started in an agent host, the host's
    /// return to the SDK's `canUseTool` request. The value names the
    /// confirmation, not the carrier.
    HookReturn,
    /// The JSON-RPC response to the app-server's own `requestApproval`, written
    /// on the Codex link's socket.
    ///
    /// The one above cannot describe it, and the difference is not
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
    /// On a Claude approval from minor 22 only `1`, the dialog's plain yes, is
    /// taken; any other is refused before anything is claimed.
    Option {
        index: u32,
    },
    /// Free text. On a Claude approval from minor 22: a denial with this reason,
    /// returned to the agent that asked through its run's agent host.
    Text {
        text: String,
    },
    /// Pick a server-offered option by its **opaque** id, for an agent whose
    /// options are not a 1-based list (Codex's `availableDecisions`). The daemon
    /// validates it against the exact option set it stored for the request and
    /// the payload hash over that set — the id is never interpreted here. Additive
    /// (minor 15). On a Claude card it is only `"acceptForSession"`, "Allow for this
    /// session", and only where the card offers it ([`ApprovalCard::allow_for_session`],
    /// minor 36); every other Claude answer is refused.
    OptionId {
        option_id: String,
    },
    /// Answers to a Claude `AskUserQuestion` card, one per question in the order
    /// the card lists them (minor 21). The daemon builds Claude's answer from the
    /// question it stored and these indices, and refuses anything incomplete or
    /// out of range; nothing is typed. A Codex question card
    /// (`request_user_input`) takes the same answers, one option or "Other" with
    /// its text per question and no notes, which the daemon sends Codex as
    /// Codex's own terminal does.
    Answers {
        answers: Vec<QuestionAnswer>,
    },
    /// Decline a Claude `AskUserQuestion` card (minor 21): the daemon denies it
    /// through the run's agent host. The main conversation's question is denied with an interrupt,
    /// so the turn stops as Escape at its dialog does; a background agent's is
    /// denied without one. A Codex question card declined is answered with no
    /// answers, and Codex's turn goes on.
    Decline,
}

/// One question's answer on an `AskUserQuestion` card.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    /// 0-based indices into the question's `options`, in the order they were
    /// chosen. At most one on a single-select question.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<u32>,
    /// The "Other" text, when it was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    /// Notes on the chosen preview. Only a single-select question whose options
    /// carry previews takes notes, as at the terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Whether the phone can answer a Claude card: an `AskUserQuestion` card from
/// minor 21, every Claude card from minor 22.
///
/// Present on such a card, absent on a Codex card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionHold {
    /// Claude is waiting on the daemon for this question: the phone can answer
    /// it. For the main conversation the Mac shows the same question at the same
    /// time, and the first complete answer wins.
    Held,
}

impl QuestionHold {
    /// The hold as it is spelled on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionHold::Held => "held",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    Phone,
    Timeout,
}

/// The kind of paired device an answer came from (minor 34), by the
/// `client_name` its connection said hello with. Attribution only, never trust:
/// a client that calls itself the Mac app changes the words its answers are
/// shown with, and nothing it may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredOn {
    Iphone,
    Mac,
}

impl AnsweredOn {
    /// The `client_name` every Mac app says hello with: CodeConnectKit's
    /// `"CodeConnect \(HostDevice.mac.name)"` (`DaemonConnection.clientName`).
    pub const MAC_CLIENT_NAME: &'static str = "CodeConnect Mac";

    /// A Mac's when the hello named the Mac app, and an iPhone's otherwise, as
    /// every answer was recorded before there was a Mac app.
    pub fn from_client_name(client_name: Option<&str>) -> Self {
        if client_name == Some(Self::MAC_CLIENT_NAME) {
            AnsweredOn::Mac
        } else {
            AnsweredOn::Iphone
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerOutcome {
    pub request_id: String,
    pub session_id: String,
    pub decision: AnswerDecision,
    pub resolved_by: ResolvedBy,
    /// Which kind of device sent an answer from a paired device
    /// (`resolved_by: phone`), from minor 34. Absent on every other outcome, from
    /// an older daemon, and on an answer a restart settled, which keeps no record
    /// of the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_on: Option<AnsweredOn>,
    pub applied_via: AnswerPath,
    pub resolved_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// True when the daemon knows *what* was decided but not whether it landed:
    /// it was killed between claiming the answer and recording its outcome, or
    /// (from minor 22, on an earlier daemon) it is a phone answer to a Claude
    /// approval, sent to the call's own hook, which Claude may not use because it
    /// takes the first answer (detail "Sent to Claude from a phone").
    ///
    /// It is uncertainty about the *actuation*, and it is the reason such a request
    /// is never retried: an answer sent twice cannot be taken back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub indeterminate: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Who answered a Codex approval: an app, whose answer the daemon wrote to Codex. A
/// card Codex settled without one is cleared, not answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionActor {
    /// The phone's claim won.
    Phone,
}

/// Why a Codex approval was cleared without being answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearCause {
    /// A `turn/interrupt` retired the pending, or the run ended with it still open.
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
/// [`CodexResolution::Unknown`]: a claim recorded but not provably actuated
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
/// `decision`, when present, may be an opaque [`AnswerDecision::OptionId`], or a
/// question card's [`AnswerDecision::Answers`] or [`AnswerDecision::Decline`] as
/// the app sent it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CodexResolution {
    /// Answered by an app, with its `decision`. An earlier daemon could send one
    /// without the decision, when it knew only who answered.
    Answered {
        by: ResolutionActor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<AnswerDecision>,
        /// Which kind of device sent a paired device's answer (`by: phone`), from
        /// minor 41, as [`AnswerOutcome::answered_on`] says for Claude. Absent on
        /// every other resolution, from an older daemon, on an answer a restart
        /// settled, which keeps no record of the device, and on an answer through a
        /// tmux-era control link.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answered_on: Option<AnsweredOn>,
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

/// **The `approval_resolved` payload for a Codex card: the terminal, plus the
/// request it is the terminal *of*.**
///
/// A *payload*, deliberately not an "envelope": the envelope is the [`crate::event::Event`]
/// this rides, and what it carries — `turn_id`, `item_id`, `source_event_id` — is a
/// different set of facts from these.
///
/// [`CodexResolution`] says what became of an approval and nothing about which
/// approval. Until this existed, the only correlation back to the card was the
/// event's `source_event_id` — the literal `"resolved:"` followed by the request
/// id — so a client had to strip a prefix off a field that is `Option<String>` on
/// the wire and is not typed as an identity at all. The failure mode of a prefix
/// parse is silence: one release renames the prefix, every parse misses, and the
/// card the operator already answered goes on standing on their phone for ever.
///
/// Claude's [`AnswerOutcome`] has carried `request_id` inside the payload since
/// minor 0. This is the same field, in the same place, for the other agent — so a
/// client correlates a resolution the one way for both agents, and never parses an
/// id out of a string.
///
/// **Flattened, so this is the resolution's own shape with one key added.** The
/// status tag and its arms are byte-identical to what they were; a client already
/// matching on `status` and reading `by` / `cause` / `write_stage` off the payload
/// is unaffected. And a decoder for a bare [`CodexResolution`] still decodes one of
/// these — serde's internally-tagged enums ignore keys they do not know — which is
/// what makes the change additive for the daemon's own persisted events as well as
/// for the phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexResolutionPayload {
    /// The card this resolves, spelled exactly as [`ApprovalCard::request_id`]
    /// spelled it — the opaque composite id, never the app-server's per-connection
    /// integer.
    pub request_id: String,
    #[serde(flatten)]
    pub resolution: CodexResolution,
}

/// What the daemon knows about a Claude run's slash commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandCatalogResult {
    /// For a run CodeConnect started, the list its claude last reported. Names
    /// come without the leading slash, exactly as emitted — the daemon adds and
    /// invents nothing.
    Available {
        commands: Vec<String>,
        /// What claude says of each command (minor 38); left out when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        entries: Vec<CommandEntry>,
        /// The version claude reported in its initialize answer, when it did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_version: Option<String>,
        /// When ccd took this list from claude: the start's initialize answer or
        /// its latest `commands_changed`. The age of a fact is part of the fact.
        probed_at: String,
    },
    /// No list. A complete answer, not an error: the phone must fall back to
    /// treating built-ins conservatively, never to guessing.
    Unavailable { reason: String },
}

/// One command of a catalog, as claude describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEntry {
    /// Without the leading slash, as in `commands`.
    pub name: String,
    pub description: String,
    /// What the command takes after its name, when claude names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
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

/// What became of a [`ClientMessage::StopRun`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StopRunResult {
    /// The stop is accepted: the run is `stopping`, and reads ended once it has exited.
    Stopping,
    /// The run had ended; nothing was done.
    AlreadyEnded,
    /// Nothing was done, and `reason` says why: human text, safe to show.
    Refused { reason: String },
}

/// What became of a [`ClientMessage::SetModel`] or [`ClientMessage::SetMode`] (minor 39).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SettingResult {
    /// claude took it, from `applies` on.
    Applied { applies: Applies },
    /// Nothing more was changed, and `reason` says why: human text, safe to show. When the
    /// model was changed and the effort was not, `reason` says so.
    Refused { reason: String },
    /// Sent, and no answer came: it may or may not have been applied.
    Indeterminate { reason: String },
}

/// When an applied setting takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// From the agent's next request: between turns, the next turn; during one, the running
    /// turn's next step (a Claude run's model or mode).
    Now,
    /// A turn was running and the change holds an effort: from the next message that starts a
    /// turn.
    NextTurn,
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
    /// This exact mutation already ran; nothing was sent a second time.
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
    /// The card's place in its run's sequence of permission requests (minor 3):
    /// per run, monotonic, one per structured permission request. From minor
    /// 22 a Claude card carries 0: it is answered through its run's agent host
    /// (an earlier daemon: its own call's hook), so its place in the run decides nothing.
    #[serde(default)]
    pub generation: u64,
    /// True when the phone can answer this card with an allow or a deny. From
    /// minor 22, for a Claude card: a held approval, answered through its run's
    /// agent host (a question takes no allow, so it is never true there).
    /// Before, it meant the prompt on screen had been fingerprinted for typing.
    ///
    /// False means the card is worth *showing* — a human should know an agent
    /// is waiting — but must be answered at the Mac. Defaulted false so a card
    /// from a daemon that had no such concept is never assumed to be bound.
    #[serde(default)]
    pub identity_bound: bool,
    /// Whether the phone can answer this card now: on an `AskUserQuestion` card
    /// from minor 21, and on every Claude card from minor 22. See
    /// [`QuestionHold`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_hold: Option<QuestionHold>,
    /// The card offers "Allow for this session" (minor 36): a Claude approval in a run
    /// started in an agent host whose request suggested an allow rule. The app answers it
    /// with [`AnswerDecision::OptionId`] `"acceptForSession"`; the daemon then returns that
    /// rule to Claude for this session only, never written to its settings. Absent and false
    /// mean the same.
    ///
    /// The answer side is already one way for both engines: `option_id:
    /// "acceptForSession"`. The offer side differs because a Codex card's `tool_input` is
    /// ccd's own description, whose hash binds its options, while a Claude card's
    /// `tool_input` is Claude's `canUseTool` input and must stay byte for byte Claude's.
    /// Moving Codex's table and its hash binding across engines for one button would be
    /// over-engineering.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_for_session: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trip() {
        let msg = ClientMessage::Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            token: Some("t".into()),
            pairing_code: None,
            client_id: None,
            client_name: Some("iPhone".into()),
            device_name: None,
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(
            !s.contains("device_name"),
            "a hello that names no device sends no key"
        );
        assert!(s.contains("\"type\":\"hello\""));
        assert!(!s.contains("client_id"));
        assert!(!s.contains("pairing_code"));
        let _: ClientMessage = serde_json::from_str(&s).unwrap();
        // A key this build does not name is ignored rather than refused: a hello
        // still carrying the `features` list minor 15 defined decodes as the
        // same hello.
        let with_features = s.replacen('{', r#"{"features":{"agents":["codex"]},"#, 1);
        match serde_json::from_str::<ClientMessage>(&with_features).unwrap() {
            ClientMessage::Hello {
                token, client_name, ..
            } => {
                assert_eq!(token.as_deref(), Some("t"));
                assert_eq!(client_name.as_deref(), Some("iPhone"));
            }
            other => panic!("wrong message: {other:?}"),
        }
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
            machine_name: None,
            machine_os: None,
            machine_id: None,
        };
        let encoded = serde_json::to_string(&ack).unwrap();
        assert!(!encoded.contains("device_token"), "{encoded}");
        for key in ["\"machine_name\"", "\"machine_os\"", "\"machine_id\""] {
            assert!(
                !encoded.contains(key),
                "a name that could not be read is left out, never sent empty: {encoded}"
            );
        }
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
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 18,
            "compose — a phone saying something to a Codex session, the daemon choosing \
             `turn/start` or `turn/steer` by what the session is doing, the `ComposeResult` \
             statuses that say which happened, and the `codex_compose` capability that \
             says the daemon understands the message at all — is minor 18"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 19,
            "the three facts a phone needs to drive a Codex session from the fleet — \
             `request_id` inside the `approval_resolved` payload, `turn_id` on the \
             approval card's event envelope, and `SessionSummary.codex_link` — are \
             minor 19"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 20,
            "`SessionSummary.codex_link`'s fifth word, `bound_not_started` — the one \
             un-subscribed state a compose may be admitted in, on a thread the daemon \
             has proved has never run a turn — is minor 20. It was written into 19 on \
             the ground that 19 had never shipped; build 72 (`66a03a3`, 2026-09-08) \
             shipped it, so the word costs a number of its own"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 21,
            "Claude's `AskUserQuestion` answered from the phone — `question_card`, the \
             card's `question_hold`, and the `answers` and `decline` decisions — is \
             minor 21"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 22,
            "every Claude card answered only through its own call's hook — \
             `hook_only_approvals`, `question_hold` on every Claude card, and \
             `text` as a denial with that reason — is minor 22"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 23,
            "earlier pages — `history`, `history_end`, `history_pages` — and the \
             session's open cards and current facts on its summary are minor 23"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 24,
            "`belongs_before` on a Codex item read back from history is minor 24"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 25,
            "`machine_name` and `machine_os` on `hello_ack` are minor 25"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 26,
            "`machine_id` on `hello_ack` is minor 26"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 27,
            "agents' messages (`send_agent_message`, `list_agents`, `protocol::peer`) are \
             minor 27"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 28,
            "starting a session from the phone (`list_projects`, `start_session`, \
             `start_session` capability) is minor 28"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 29,
            "picking a branch or worktree at start (`get_project_git`, `start_session`'s \
             `worktree`, `started.cwd`, `start_worktree` capability) is minor 29"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 30,
            "browsing folders (`list_folder`, `create_folder`, `browse_folders` capability, a \
             start in any folder) is minor 30"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 31,
            "each session's branch and line counts (`git_branch`, `diff_stat`) and the \
             computer's CPU, memory and disk (`get_machine_stats`, `machine_stats` \
             capability) are minor 31"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 32,
            "a hello with neither token nor code from the owner's own Tailscale device, and \
             the `peers` frame, are minor 32"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 33,
            "the models each agent offers and whether it is signed in (`models`, \
             `signed_in`), and `start_session`'s `model`, are minor 33"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 34,
            "which kind of device sent an answer (`answered_on`) is minor 34"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 35,
            "ending a Claude run from an app (`stop_run`, `stop_run_result`, the `stop_run` \
             capability) and the `stopping` lifecycle are minor 35"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 36,
            "\"Allow for this session\" on a hosted Claude run's card (`allow_for_session`) \
             is minor 36"
        );
        const _: () = assert!(
            crate::PROTOCOL_VERSION == 2,
            "major 2 is the one engine's protocol"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 37,
            "Stop on a Claude run CodeConnect started (`claude_interrupt`) is minor 37"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 38,
            "the command menu and a command's output on a Claude run CodeConnect started \
             (`entries`) are minor 38"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 39,
            "a Claude run's settings from an app (`set_model`, `set_mode`, `menus`) are minor 39"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 40,
            "the mode that asks nothing behind a confirmation (`confirm`, `confirmed`) is minor 40"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 41,
            "which kind of device answered a Codex card (`answered_on` on its resolution) is \
             minor 41"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 42,
            "a summary saying the next message relaunches the run (`relaunchable`) is minor 42"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 43,
            "a Codex run's settings from an app (`menus`, `set_model`, `set_mode`) are minor 43"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 44,
            "a device naming itself in its hello (`device_name`) is minor 44"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 45,
            "a menu entry naming the model it runs (`resolved_model`) is minor 45"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 46,
            "an ended run naming its model by its menu's label (`model_label`) is minor 46"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 47,
            "a menu entry naming its model's default effort (`default_effort`) is minor 47"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 48,
            "several accounts per agent (`list_accounts`, `add_account`, `activate_account`) is minor 48"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 49,
            "a Claude turn's usage naming its context (`last.totalTokens`, `modelContextWindow`) is minor 49"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 50,
            "files for a session's agent (`put_file`, `attachments`, `uploads`) are minor 50"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 51,
            "Antigravity's sign-in from an app (`sign_in_agent`, `sign_out_agent`, `agent_sign_ins`) is minor 51"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 52,
            "a switch of account moving a run at its next message, a moving run's `menus` offered, is minor 52"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 53,
            "a diff listing its files (`files`, `files_truncated`) is minor 53"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 54,
            "a usage window's model group (`group`) is minor 54"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 55,
            "Antigravity's one window, Gemini's week, as a plain window (no `group`, no `grouped_windows`) is minor 55"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 56,
            "a run's shell (`terminal_open`, `run_terminal`) is minor 56"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 57,
            "Antigravity's accounts (`add_account` and the rest for `antigravity`) are minor 57"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 58,
            "a run's repository for its Changes panel (`get_run_git`, `run_git`, `changes`) is minor 58"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 59,
            "a commit from the Changes panel (`git_action`, `git_action_result`, `git_actions`, `committed`) is minor 59"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 60,
            "a run's GitHub state and its pull request (`gh`, `gh_host`, `pr`, `gh_read_at`) is minor 60"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 61,
            "a pull request from the Changes panel (`create_pr`, `pr` on `done`, `gh_repo`) is minor 61"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 62,
            "a pull request's checks (`checks`, `checks_complete`) are minor 62"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 63,
            "a run's title (`title`) is minor 63"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 64,
            "merging a pull request (`merge`, `merge_state`, `merge_methods`) is minor 64"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 65,
            "a run's usage limit (`limit`) is minor 65"
        );
        const _: () = assert!(
            crate::PROTOCOL_MINOR >= 66,
            "a run's conflicts with its base (`get_conflicts`, `get_conflict`, `conflicts`) are minor 66"
        );
        // The equality is the point: every bump has to come here and say what it
        // added, so the list above stays a record rather than a guess.
        assert_eq!(crate::PROTOCOL_MINOR, 66);
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
            "a later run can carry the same name, so a name must never carry a delete"
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
            "fail_mode":"fail_open","answer_path":"hook_return","hold_secs":0,
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

        // A registration still carrying the `features` list minor 15 defined is
        // the same registration: the key is ignored rather than refused.
        let advertising: ClientMessage = serde_json::from_str(
            r#"{"type":"register_push","token":"aabb","environment":"production",
                "features":{"agents":["claude","codex"]}}"#,
        )
        .unwrap();
        match advertising {
            ClientMessage::RegisterPush {
                token, environment, ..
            } => {
                assert_eq!(token, "aabb");
                assert_eq!(environment.as_deref(), Some("production"));
            }
            other => panic!("wrong message: {other:?}"),
        }

        // And a minor-14 registration is exactly the same document plus one
        // optional key, so an older daemon ignores it rather than failing.
        let carried = serde_json::to_value(ClientMessage::RegisterPush {
            token: "aabb".into(),
            environment: Some("production".into()),
            relay_credential: Some("opaque".into()),
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
                "answer_path":"hook_return","hold_secs":0,"send_text":true,
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
                assert!(!capabilities.private_transport);
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
                submit,
                ..
            } => {
                assert_eq!(request_id, None);
                assert_eq!(payload_hash, None);
                assert!(submit);
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    /// **A `send_text` is its session, text, identity and `submit`, and nothing else.** A
    /// client that still sends `require` or `complete_native_confirmation` is read without
    /// them: an unknown key is ignored, not refused.
    #[test]
    fn a_send_text_still_carrying_require_and_a_confirmation_is_read_without_them() {
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"send_text","session_id":"cc-1","text":"/model x","request_id":"st-1",
                "payload_hash":"h","require":{"mode":"input_box"},"submit":true,
                "complete_native_confirmation":true}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            serde_json::json!({"type": "send_text", "session_id": "cc-1", "text": "/model x",
                               "request_id": "st-1", "payload_hash": "h", "submit": true})
        );
    }

    /// **A run's shell, as an app and the daemon write its frames** (minor 56): every tag and field the Mac app
    /// matches by hand, and an open that names no resume and takes nothing over reads that way.
    #[test]
    fn the_terminal_frames_read_and_write_as_the_app_matches_them() {
        let open: ClientMessage = serde_json::from_str(
            r#"{"type":"terminal_open","attachment_id":"t1","session_id":"01RUN","cols":80,"rows":24,
                "output_credit":65536}"#,
        )
        .unwrap();
        assert!(matches!(
            open,
            ClientMessage::TerminalOpen {
                cols: 80,
                rows: 24,
                output_credit: 65536,
                resume_after: None,
                takeover: false,
                ..
            }
        ));
        let resumed: ClientMessage = serde_json::from_str(
            r#"{"type":"terminal_open","attachment_id":"t2","session_id":"01RUN","cols":80,"rows":24,
                "output_credit":65536,"resume_after":4096,"takeover":true}"#,
        )
        .unwrap();
        assert!(matches!(
            resumed,
            ClientMessage::TerminalOpen {
                resume_after: Some(4096),
                takeover: true,
                ..
            }
        ));
        for (json, expected) in [
            (
                r#"{"type":"terminal_input","attachment_id":"t1","data":"bHMK"}"#,
                "terminal_input",
            ),
            (
                r#"{"type":"terminal_resize","attachment_id":"t1","cols":100,"rows":30}"#,
                "terminal_resize",
            ),
            (
                r#"{"type":"terminal_credit","attachment_id":"t1","bytes":16384}"#,
                "terminal_credit",
            ),
            (
                r#"{"type":"terminal_detach","attachment_id":"t1"}"#,
                "terminal_detach",
            ),
        ] {
            let parsed: ClientMessage = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap()["type"], expected);
        }
        assert_eq!(
            serde_json::to_value(ServerMessage::TerminalOpened {
                attachment_id: "t1".into(),
                input_credit: TERMINAL_INPUT_CREDIT,
                max_chunk_bytes: MAX_TERMINAL_CHUNK_BYTES,
                max_outstanding_credit: TERMINAL_MAX_OUTSTANDING_CREDIT,
                folder: "/r/.claude/worktrees/x".into(),
                replay_end: 12,
            })
            .unwrap(),
            serde_json::json!({"type": "terminal_opened", "attachment_id": "t1", "input_credit": 32768,
                               "max_chunk_bytes": 16384, "max_outstanding_credit": 262144,
                               "folder": "/r/.claude/worktrees/x", "replay_end": 12})
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::TerminalOutput {
                attachment_id: "t1".into(),
                data: "aGk=".into(),
                reset: false,
                offset: 14,
            })
            .unwrap(),
            serde_json::json!({"type": "terminal_output", "attachment_id": "t1", "data": "aGk=", "offset": 14})
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::TerminalOutput {
                attachment_id: "t1".into(),
                data: String::new(),
                reset: true,
                offset: 0,
            })
            .unwrap()["reset"],
            true
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::TerminalCredit {
                attachment_id: "t1".into(),
                bytes: 3
            })
            .unwrap(),
            serde_json::json!({"type": "terminal_credit", "attachment_id": "t1", "bytes": 3})
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::TerminalClosed {
                attachment_id: "t1".into(),
                reason: terminal_closed::RUN_ENDED.into(),
            })
            .unwrap(),
            serde_json::json!({"type": "terminal_closed", "attachment_id": "t1", "reason": "run_ended"})
        );
    }

    /// **A file's chunk and a message's attachments, as an app sends them** (minor 50): a
    /// `send_text` or `compose` attaching nothing is written exactly as before.
    #[test]
    fn a_put_file_chunk_and_attachments_read_as_sent() {
        let chunk: ClientMessage = serde_json::from_str(
            r#"{"type":"put_file","session_id":"cc-1","upload_id":"01UP","name":"a.png",
                "size":5,"offset":0,"data":"YWJj"}"#,
        )
        .unwrap();
        assert!(matches!(
            chunk,
            ClientMessage::PutFile {
                size: 5,
                offset: 0,
                ..
            }
        ));
        // A file for a start names no session, and a start attaches it.
        let pending: ClientMessage = serde_json::from_str(
            r#"{"type":"put_file","upload_id":"01UR","name":"b.txt","size":1,"offset":0,
                "data":"Yg=="}"#,
        )
        .unwrap();
        assert!(matches!(
            pending,
            ClientMessage::PutFile {
                session_id: None,
                ..
            }
        ));
        let start: ClientMessage = serde_json::from_str(
            r#"{"type":"start_session","request_id":"r1","path":"/p","agent":"claude",
                "attachments":["01UR"]}"#,
        )
        .unwrap();
        match start {
            ClientMessage::StartSession { attachments, .. } => assert_eq!(attachments, ["01UR"]),
            other => panic!("wrong message: {other:?}"),
        }
        let compose: ClientMessage = serde_json::from_str(
            r#"{"type":"compose","session_id":"cc-1","request_id":"c-1","text":"",
                "payload_hash":"h","attachments":["01UP","01UQ"]}"#,
        )
        .unwrap();
        match compose {
            ClientMessage::Compose { attachments, .. } => {
                assert_eq!(attachments, ["01UP", "01UQ"])
            }
            other => panic!("wrong message: {other:?}"),
        }
        let plain = ClientMessage::Compose {
            session_id: "cc-1".into(),
            request_id: "c-1".into(),
            text: "hi".into(),
            payload_hash: "h".into(),
            attachments: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"type": "compose", "session_id": "cc-1", "request_id": "c-1",
                               "text": "hi", "payload_hash": "h"})
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::PutFileResult {
                upload_id: "01UP".into(),
                result: PutFileResult::Received { received: 3 },
            })
            .unwrap(),
            serde_json::json!({"type": "put_file_result", "upload_id": "01UP",
                               "result": {"status": "received", "received": 3}})
        );
    }

    #[test]
    fn an_idempotent_send_text_round_trips_with_its_identity() {
        let hash = crate::hash::send_text_hash("cc-1", "hi", true);
        let msg = ClientMessage::SendText {
            session_id: "cc-1".into(),
            text: "hi".into(),
            request_id: Some("st-1".into()),
            payload_hash: Some(hash.clone()),
            submit: true,
            attachments: Vec::new(),
        };
        let encoded = serde_json::to_string(&msg).unwrap();
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
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: "t".into(),
            detail: None,
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

    /// The two minor-21 decisions, byte for byte, since the phone encodes them
    /// itself and the daemon decodes exactly these bytes.
    #[test]
    fn question_decisions_have_the_wire_shape_the_phone_sends() {
        let answers = AnswerDecision::Answers {
            answers: vec![
                QuestionAnswer {
                    selected: vec![0],
                    ..Default::default()
                },
                QuestionAnswer {
                    selected: vec![2, 0],
                    other: Some("café ☕".into()),
                    notes: None,
                },
                QuestionAnswer {
                    selected: vec![1],
                    other: None,
                    notes: Some("wide".into()),
                },
            ],
        };
        let encoded = serde_json::to_string(&answers).unwrap();
        assert_eq!(
            encoded,
            r#"{"type":"answers","answers":[{"selected":[0]},{"selected":[2,0],"other":"café ☕"},{"selected":[1],"notes":"wide"}]}"#
        );
        assert_eq!(
            serde_json::from_str::<AnswerDecision>(&encoded).unwrap(),
            answers
        );
        // An answer that is only "Other" omits `selected` and still decodes.
        assert_eq!(
            serde_json::from_str::<AnswerDecision>(
                r#"{"type":"answers","answers":[{"other":"x"}]}"#
            )
            .unwrap(),
            AnswerDecision::Answers {
                answers: vec![QuestionAnswer {
                    other: Some("x".into()),
                    ..Default::default()
                }]
            }
        );
        assert_eq!(
            serde_json::to_string(&AnswerDecision::Decline).unwrap(),
            r#"{"type":"decline"}"#
        );
    }

    /// **A hold is `held`**, spelled by hand, and `at_mac` and `ended` do not decode.
    #[test]
    fn a_hold_is_held() {
        let encoded = serde_json::to_string(&QuestionHold::Held).unwrap();
        assert_eq!(encoded, "\"held\"");
        assert_eq!(encoded, format!("\"{}\"", QuestionHold::Held.as_str()));
        assert_eq!(
            serde_json::from_str::<QuestionHold>(&encoded).unwrap(),
            QuestionHold::Held
        );
        for gone in ["\"at_mac\"", "\"ended\""] {
            assert!(
                serde_json::from_str::<QuestionHold>(gone).is_err(),
                "{gone}"
            );
        }
    }

    /// A card with no `question_hold` decodes as none: a card from before minor
    /// 22 that is not a question, or a Codex card.
    #[test]
    fn only_a_question_card_names_its_hold() {
        let card: ApprovalCard = serde_json::from_str(
            r#"{"request_id":"toolu_1","payload_hash":"h","tool_name":"Bash",
                "tool_input":{},"display_text":"Bash"}"#,
        )
        .unwrap();
        assert_eq!(card.question_hold, None);
        assert!(!serde_json::to_string(&card)
            .unwrap()
            .contains("question_hold"));
        let mut question = card.clone();
        question.question_hold = Some(QuestionHold::Held);
        assert!(serde_json::to_string(&question)
            .unwrap()
            .contains(r#""question_hold":"held""#));
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
            send_text: true,
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
            private_transport: true,
            codex_interrupt: true,
            codex_compose: true,
            supported_agents: vec![crate::agent::AgentKind::Claude],
            question_card: true,
            hook_only_approvals: true,
            history_pages: true,
            start_session: true,
            start_worktree: true,
            browse_folders: true,
            machine_stats: true,
            stop_run: true,
            claude_interrupt: true,
            accounts: true,
            uploads: true,
            run_terminal: true,
            changes: true,
            git_actions: vec!["commit".into(), "create_pr".into(), "merge".into()],
            conflicts: true,
        }
    }

    /// **The Claude byte-identical gate.** Adding the Codex resolution envelope
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
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: "2026-08-18T00:00:00.000Z".into(),
            detail: None,
            indeterminate: false,
        };
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            r#"{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},"resolved_by":"phone","applied_via":"hook_return","resolved_at":"2026-08-18T00:00:00.000Z"}"#
        );

        let applied = AnswerResult::Applied { outcome };
        assert_eq!(
            serde_json::to_string(&applied).unwrap(),
            r#"{"status":"applied","outcome":{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},"resolved_by":"phone","applied_via":"hook_return","resolved_at":"2026-08-18T00:00:00.000Z"}}"#
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
    fn capabilities_supported_agents_defaults_empty_for_an_older_daemon() {
        // An ack from a daemon predating the seam omits the list; it decodes as
        // empty, which a client reads as Claude-only.
        let older = serde_json::json!({
            "can_approve_reliably": true, "fail_mode": "fail_open",
            "answer_path": "hook_return", "hold_secs": 0, "send_text": true,
            "capture": true, "push": false, "tls": true
        });
        let caps: Capabilities = serde_json::from_value(older).expect("decodes");
        assert!(caps.supported_agents.is_empty());

        // An empty list is **omitted** on the wire — a daemon with nothing to add
        // beyond the Claude floor sends no field, so an older phone renders no new
        // diagnostic row.
        let mut floor = capabilities_fixture();
        floor.supported_agents = Vec::new();
        let s = serde_json::to_string(&floor).unwrap();
        assert!(
            !s.contains("supported_agents"),
            "empty must be omitted: {s}"
        );

        // A non-empty list (once a second agent can be driven) serializes.
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
                answered_on: Some(AnsweredOn::Mac),
            },
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: None,
                answered_on: None,
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
                by: ResolutionActor::Phone,
                decision: None,
                answered_on: None
            })
            .unwrap(),
            r#"{"status":"answered","by":"phone"}"#
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

    /// **A Codex card is answered by `phone`**, spelled by hand, and `local` does not
    /// decode.
    #[test]
    fn a_codex_card_is_answered_by_the_phone_alone() {
        let encoded = serde_json::to_string(&ResolutionActor::Phone).unwrap();
        assert_eq!(encoded, "\"phone\"");
        assert_eq!(
            serde_json::from_str::<ResolutionActor>(&encoded).unwrap(),
            ResolutionActor::Phone
        );
        assert!(serde_json::from_str::<ResolutionActor>("\"local\"").is_err());
    }

    /// **The correlation the phone actually uses, pinned as a wire string.**
    ///
    /// Before this field the only route from a resolution back to its card was the
    /// event's `source_event_id`, `"resolved:<request_id>"` — a prefix parse whose
    /// failure is silent and whose symptom is an answered card standing on a phone
    /// for ever. `request_id` now rides the payload, where Claude's has ridden since
    /// minor 0, so one rule correlates both agents.
    ///
    /// **Mutation:** drop the `flatten`, or rename the field, and the first
    /// assertion fails naming the exact JSON the phone decodes.
    #[test]
    fn a_codex_resolution_payload_names_the_request_it_resolves() {
        let payload = CodexResolutionPayload {
            request_id: "AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QQ1g".into(),
            resolution: CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: Some(AnswerDecision::OptionId {
                    option_id: "accept".into(),
                }),
                answered_on: None,
            },
        };
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            r#"{"request_id":"AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QQ1g","status":"answered","by":"phone","decision":{"type":"option_id","option_id":"accept"}}"#,
            "this is the object the phone decodes; `request_id` sits beside `status`, \
             exactly where `AnswerOutcome` puts Claude's"
        );
        assert_eq!(
            serde_json::from_str::<CodexResolutionPayload>(
                &serde_json::to_string(&payload).unwrap()
            )
            .unwrap(),
            payload
        );

        // **Additive, proven rather than asserted**: every terminal still decodes as
        // a bare `CodexResolution` with the new key present, which is what keeps the
        // daemon's own readers — and any client written against minor 18 — working.
        for resolution in [
            CodexResolution::Answered {
                by: ResolutionActor::Phone,
                decision: None,
                answered_on: None,
            },
            CodexResolution::Cleared {
                cause: ClearCause::ItemCompleted,
            },
            CodexResolution::Timeout,
            CodexResolution::Unknown {
                attempted_by: ResolutionActor::Phone,
                attempted_decision: None,
                write_stage: WriteStage::ClaimedNotEnqueued,
                cause: "the daemon was killed mid-write".into(),
            },
        ] {
            let with_id = CodexResolutionPayload {
                request_id: "rq-1".into(),
                resolution: resolution.clone(),
            };
            let encoded = serde_json::to_string(&with_id).unwrap();
            assert!(encoded.contains(r#""request_id":"rq-1""#), "{encoded}");
            assert_eq!(
                serde_json::from_str::<CodexResolution>(&encoded).unwrap(),
                resolution,
                "a minor-18 decoder must still read this payload: {encoded}"
            );
        }
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
                entries: Vec::new(),
                claude_version: Some("2.1.221".into()),
                probed_at: "2026-08-04T20:00:00Z".into(),
            },
        };
        let encoded = serde_json::to_string(&available).unwrap();
        assert!(encoded.contains(r#""status":"available""#), "{encoded}");
        assert!(!encoded.contains("entries"), "{encoded}");
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

    /// The `private_transport` capability defaults to false, so an ack from a
    /// daemon that does not send it is never read as a claim that the connection
    /// is private — and a new client decodes an old ack cleanly.
    #[test]
    fn private_transport_is_false_when_absent() {
        let old_ack = r#"{"type":"hello_ack","protocol_version":1,"server_time":"t",
            "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
            "answer_path":"hook_return","hold_secs":0,"send_text":true,"capture":true,
            "push":false,"tls":false}}"#;
        match serde_json::from_str::<ServerMessage>(old_ack).unwrap() {
            ServerMessage::HelloAck { capabilities, .. } => {
                assert!(
                    !capabilities.private_transport,
                    "absent must read as not private"
                );
                assert!(
                    !capabilities.command_catalog,
                    "and so must every later flag"
                );
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    /// **The accounts wire, pinned as the apps will match it** (minor 48). Each frame encodes to exactly this JSON
    /// and decodes back to it; Default is an explicit null; an older daemon's capabilities decode `accounts` false.
    #[test]
    fn the_accounts_wire_is_pinned() {
        fn round_trip_client(message: ClientMessage, expected: serde_json::Value) {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            let decoded: ClientMessage = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
        round_trip_client(
            ClientMessage::ListAccounts {
                request_id: "a1".into(),
            },
            serde_json::json!({"type": "list_accounts", "request_id": "a1"}),
        );
        round_trip_client(
            ClientMessage::AddAccount {
                request_id: "a2".into(),
                agent: crate::agent::AgentKind::Codex,
                label: "Work".into(),
            },
            serde_json::json!({"type": "add_account", "request_id": "a2", "agent": "codex", "label": "Work"}),
        );
        round_trip_client(
            ClientMessage::ActivateAccount {
                request_id: "a3".into(),
                agent: crate::agent::AgentKind::Claude,
                account_id: Some("01M80QGTWR9RCHBSJWR169GXCN".into()),
            },
            serde_json::json!({"type": "activate_account", "request_id": "a3", "agent": "claude",
                "account_id": "01M80QGTWR9RCHBSJWR169GXCN"}),
        );
        round_trip_client(
            ClientMessage::ActivateAccount {
                request_id: "a4".into(),
                agent: crate::agent::AgentKind::Claude,
                account_id: None,
            },
            serde_json::json!({"type": "activate_account", "request_id": "a4", "agent": "claude",
                "account_id": null}),
        );
        // A Default asked for with the field left out reads the same.
        let bare: ClientMessage = serde_json::from_value(
            serde_json::json!({"type": "activate_account", "request_id": "a5", "agent": "codex"}),
        )
        .unwrap();
        assert!(matches!(
            bare,
            ClientMessage::ActivateAccount {
                account_id: None,
                ..
            }
        ));

        // An id ccd made (a store's `add_account` in a test run), as every account id is.
        assert!(crate::uid::is_well_formed("01M80QGTWR9RCHBSJWR169GXCN"));
        let reply = ServerMessage::Accounts {
            request_id: Some("a2".into()),
            accounts: vec![AgentAccount {
                id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
                agent: crate::agent::AgentKind::Codex,
                label: "Work".into(),
                signed_in: None,
                signing_in: false,
                not_separate: false,
                facts: None,
                same_as_another: false,
            }],
            active: vec![ActiveAccount {
                agent: crate::agent::AgentKind::Codex,
                account_id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
            }],
            defaults: Vec::new(),
            agent_sign_ins: Vec::new(),
        };
        let expected = serde_json::json!({"type": "accounts", "request_id": "a2",
            "accounts": [{"id": "01M80QGTWR9RCHBSJWR169GXCN", "agent": "codex", "label": "Work"}],
            "active": [{"agent": "codex", "account_id": "01M80QGTWR9RCHBSJWR169GXCN"}]});
        assert_eq!(serde_json::to_value(&reply).unwrap(), expected);
        let decoded: ServerMessage = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);

        // A sign-in names the account; a listed account says whether it is signed in, when that is known.
        round_trip_client(
            ClientMessage::SignInAccount {
                request_id: "a7".into(),
                account_id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
            },
            serde_json::json!({"type": "sign_in_account", "request_id": "a7",
                "account_id": "01M80QGTWR9RCHBSJWR169GXCN"}),
        );
        let known = AgentAccount {
            id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
            agent: crate::agent::AgentKind::Codex,
            label: "Work".into(),
            signed_in: Some(true),
            signing_in: true,
            not_separate: false,
            facts: None,
            same_as_another: false,
        };
        assert_eq!(
            serde_json::to_value(&known).unwrap(),
            serde_json::json!({"id": "01M80QGTWR9RCHBSJWR169GXCN", "agent": "codex", "label": "Work", "signed_in": true,
                "signing_in": true})
        );
        let older: AgentAccount = serde_json::from_value(
            serde_json::json!({"id": "01M80QGTWR9RCHBSJWR169GXCN", "agent": "codex", "label": "Work"}),
        )
        .unwrap();
        assert_eq!(
            older.signed_in, None,
            "a listing without it reads not known"
        );
        assert!(!older.signing_in, "nor signing in");
        assert!(!older.not_separate, "nor not separate");
        // What the agent says of an account, and of its Default, made-up values in the agent's shapes: each window
        // the agent names, an unnamed one absent.
        let facts = AccountFacts {
            limits_available: None,
            email: Some("work@example.test".into()),
            plan: Some("plus".into()),
            windows: vec![UsageWindow {
                used_percent: 42,
                window_minutes: 300,
                resets_at: 1_800_000_000,
            }],
            read_at: "2026-11-22T21:00:00.000Z".into(),
        };
        let told = ServerMessage::Accounts {
            request_id: None,
            accounts: vec![AgentAccount {
                id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
                agent: crate::agent::AgentKind::Codex,
                label: "Work".into(),
                signed_in: Some(true),
                signing_in: false,
                not_separate: false,
                facts: Some(facts.clone()),
                same_as_another: true,
            }],
            active: Vec::new(),
            defaults: vec![DefaultAccount {
                agent: crate::agent::AgentKind::Codex,
                facts: AccountFacts {
                    windows: Vec::new(),
                    ..facts
                },
            }],
            agent_sign_ins: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&told).unwrap(),
            serde_json::json!({"type": "accounts", "active": [],
                "accounts": [{"id": "01M80QGTWR9RCHBSJWR169GXCN", "agent": "codex", "label": "Work",
                    "signed_in": true, "same_as_another": true,
                    "facts": {"email": "work@example.test", "plan": "plus", "read_at": "2026-11-22T21:00:00.000Z",
                        "windows": [{"used_percent": 42, "window_minutes": 300, "resets_at": 1800000000}]}}],
                "defaults": [{"agent": "codex",
                    "facts": {"email": "work@example.test", "plan": "plus", "read_at": "2026-11-22T21:00:00.000Z"}}]})
        );
        let shared = AgentAccount {
            id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
            agent: crate::agent::AgentKind::Codex,
            label: "Work".into(),
            signed_in: Some(false),
            signing_in: false,
            not_separate: true,
            facts: None,
            same_as_another: false,
        };
        assert_eq!(
            serde_json::to_value(&shared).unwrap(),
            serde_json::json!({"id": "01M80QGTWR9RCHBSJWR169GXCN", "agent": "codex", "label": "Work",
                "signed_in": false, "not_separate": true})
        );
        // Pushed, a listing names no request.
        let pushed = ServerMessage::Accounts {
            request_id: None,
            accounts: Vec::new(),
            active: Vec::new(),
            defaults: Vec::new(),
            agent_sign_ins: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&pushed).unwrap(),
            serde_json::json!({"type": "accounts", "accounts": [], "active": []})
        );
        // Antigravity's own sign-in and sign-out (minor 51) name its agent; its sign-in state rides the listing.
        round_trip_client(
            ClientMessage::SignInAgent {
                request_id: "a9".into(),
                agent: crate::agent::AgentKind::Antigravity,
            },
            serde_json::json!({"type": "sign_in_agent", "request_id": "a9", "agent": "antigravity"}),
        );
        round_trip_client(
            ClientMessage::SignOutAgent {
                request_id: "a10".into(),
                agent: crate::agent::AgentKind::Antigravity,
            },
            serde_json::json!({"type": "sign_out_agent", "request_id": "a10", "agent": "antigravity"}),
        );
        let states = ServerMessage::Accounts {
            request_id: None,
            accounts: Vec::new(),
            active: Vec::new(),
            defaults: Vec::new(),
            agent_sign_ins: vec![AgentSignIn {
                agent: crate::agent::AgentKind::Antigravity,
                signed_in: Some(false),
                signing_in: true,
            }],
        };
        let expected = serde_json::json!({"type": "accounts", "accounts": [], "active": [],
            "agent_sign_ins": [{"agent": "antigravity", "signed_in": false, "signing_in": true}]});
        assert_eq!(serde_json::to_value(&states).unwrap(), expected);
        let decoded: ServerMessage = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        // A removal names the account.
        round_trip_client(
            ClientMessage::RemoveAccount {
                request_id: "a8".into(),
                account_id: "01M80QGTWR9RCHBSJWR169GXCN".into(),
            },
            serde_json::json!({"type": "remove_account", "request_id": "a8",
                "account_id": "01M80QGTWR9RCHBSJWR169GXCN"}),
        );

        // A refusal names the request it answers.
        let refused = ServerMessage::AccountsRefused {
            request_id: "a6".into(),
            code: "bad_request".into(),
            message: "an account's name is 1 to 64 characters".into(),
        };
        let expected = serde_json::json!({"type": "accounts_refused", "request_id": "a6",
            "code": "bad_request", "message": "an account's name is 1 to 64 characters"});
        assert_eq!(serde_json::to_value(&refused).unwrap(), expected);
        let decoded: ServerMessage = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);

        let older = serde_json::json!({
            "can_approve_reliably": true, "fail_mode": "fail_open",
            "answer_path": "hook_return", "hold_secs": 0, "send_text": true,
            "capture": true, "push": false, "tls": true
        });
        let caps: Capabilities = serde_json::from_value(older).expect("decodes");
        assert!(
            !caps.accounts,
            "a daemon that does not say keeps no accounts"
        );
    }

    /// **The start-session wire, pinned as the phone matches it** (minor 28).
    /// Each message encodes to exactly this JSON and decodes back to it.
    #[test]
    fn the_start_session_messages_round_trip_as_the_phone_matches_them() {
        fn round_trip_client(message: ClientMessage, expected: serde_json::Value) {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            let decoded: ClientMessage = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }
        fn round_trip_server(message: ServerMessage, expected: serde_json::Value) {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            let decoded: ServerMessage = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }

        round_trip_client(
            ClientMessage::ListProjects,
            serde_json::json!({"type": "list_projects"}),
        );
        round_trip_client(
            ClientMessage::StartSession {
                request_id: "r1".into(),
                path: "/Users/me/src/app".into(),
                agent: crate::agent::AgentKind::Claude,
                prompt: Some("-fix \"it\"; $(x)\nnow;".into()),
                worktree: None,
                model: None,
                trust: Some(crate::host::Trust::NotTrusted),
                attachments: Vec::new(),
            },
            serde_json::json!({"type": "start_session", "request_id": "r1",
                "path": "/Users/me/src/app", "agent": "claude",
                "prompt": "-fix \"it\"; $(x)\nnow;", "trust": "not_trusted"}),
        );
        round_trip_client(
            ClientMessage::StartSession {
                request_id: "r2".into(),
                path: "/srv/x".into(),
                agent: crate::agent::AgentKind::Codex,
                prompt: None,
                worktree: None,
                model: None,
                trust: None,
                attachments: Vec::new(),
            },
            serde_json::json!({"type": "start_session", "request_id": "r2",
                "path": "/srv/x", "agent": "codex"}),
        );
        round_trip_client(
            ClientMessage::ListFolderTrust {
                request_id: "t1".into(),
            },
            serde_json::json!({"type": "list_folder_trust", "request_id": "t1"}),
        );
        round_trip_client(
            ClientMessage::SetFolderTrust {
                request_id: "t2".into(),
                path: "/Users/me/src/app".into(),
                trust: crate::host::Trust::Trusted,
            },
            serde_json::json!({"type": "set_folder_trust", "request_id": "t2",
                "path": "/Users/me/src/app", "trust": "trusted"}),
        );
        round_trip_client(
            ClientMessage::ForgetFolderTrust {
                request_id: "t3".into(),
                path: "/Users/me/src/gone".into(),
            },
            serde_json::json!({"type": "forget_folder_trust", "request_id": "t3",
                "path": "/Users/me/src/gone"}),
        );
        round_trip_server(
            ServerMessage::StartSessionResult {
                request_id: "r1".into(),
                result: StartSessionResult::NeedsTrust {
                    path: "/Users/me/src/app".into(),
                },
            },
            serde_json::json!({"type": "start_session_result", "request_id": "r1",
                "result": {"status": "needs_trust", "path": "/Users/me/src/app"}}),
        );
        round_trip_server(
            ServerMessage::FolderTrust {
                request_id: "t1".into(),
                folders: vec![FolderAnswer {
                    path: "/Users/me/src/app".into(),
                    trust: crate::host::Trust::NotTrusted,
                    answered_at: "2026-11-12T20:00:00Z".into(),
                }],
            },
            serde_json::json!({"type": "folder_trust", "request_id": "t1",
                "folders": [{"path": "/Users/me/src/app", "trust": "not_trusted",
                    "answered_at": "2026-11-12T20:00:00Z"}]}),
        );

        round_trip_server(
            ServerMessage::Projects {
                projects: vec![
                    Project {
                        path: "/Users/me/src/app".into(),
                        name: "app".into(),
                        last_used: "2026-11-04T01:00:00.000Z".into(),
                        last_agent: crate::agent::AgentKind::Codex,
                        git_branch: Some("main".into()),
                        exists: true,
                    },
                    Project {
                        path: "/Users/me/gone".into(),
                        name: "gone".into(),
                        last_used: "2026-10-01T09:00:00.000Z".into(),
                        last_agent: crate::agent::AgentKind::Claude,
                        git_branch: None,
                        exists: false,
                    },
                ],
                agents: vec![
                    AgentAvailability {
                        agent: crate::agent::AgentKind::Claude,
                        available: true,
                        detail: None,
                        models: Vec::new(),
                        signed_in: None,
                    },
                    AgentAvailability {
                        agent: crate::agent::AgentKind::Codex,
                        available: false,
                        detail: Some("codex is not installed".into()),
                        models: Vec::new(),
                        signed_in: None,
                    },
                ],
            },
            serde_json::json!({"type": "projects",
                "projects": [
                    {"path": "/Users/me/src/app", "name": "app",
                     "last_used": "2026-11-04T01:00:00.000Z", "last_agent": "codex",
                     "git_branch": "main", "exists": true},
                    {"path": "/Users/me/gone", "name": "gone",
                     "last_used": "2026-10-01T09:00:00.000Z", "last_agent": "claude",
                     "exists": false}],
                "agents": [
                    {"agent": "claude", "available": true},
                    {"agent": "codex", "available": false,
                     "detail": "codex is not installed"}]}),
        );

        round_trip_server(
            ServerMessage::StartSessionResult {
                request_id: "r1".into(),
                result: StartSessionResult::Started {
                    session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
                    cwd: None,
                },
            },
            serde_json::json!({"type": "start_session_result", "request_id": "r1",
                "result": {"status": "started",
                           "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR"}}),
        );
        for (reason, word) in [
            (StartFailure::AgentMissing, "agent_missing"),
            (StartFailure::FolderMissing, "folder_missing"),
            (StartFailure::LaunchFailed, "launch_failed"),
        ] {
            round_trip_server(
                ServerMessage::StartSessionResult {
                    request_id: "r2".into(),
                    result: StartSessionResult::Failed {
                        reason,
                        detail: "why".into(),
                    },
                },
                serde_json::json!({"type": "start_session_result", "request_id": "r2",
                    "result": {"status": "failed", "reason": word, "detail": "why"}}),
            );
        }
    }

    /// **A start that names a model, and the models an agent offers** (minor 33),
    /// pinned as the apps match them. Without them, both read as minor 32 wrote
    /// them (above).
    #[test]
    fn a_model_and_the_models_an_agent_offers_round_trip_as_the_apps_match_them() {
        fn round_trip<T>(message: T, expected: serde_json::Value)
        where
            T: Serialize + serde::de::DeserializeOwned,
        {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            let decoded: T = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }

        round_trip(
            ClientMessage::StartSession {
                request_id: "r3".into(),
                path: "/Users/me/src/app".into(),
                agent: crate::agent::AgentKind::Claude,
                prompt: None,
                worktree: None,
                model: Some("claude-fable-5-1[1m]".into()),
                trust: None,
                attachments: Vec::new(),
            },
            serde_json::json!({"type": "start_session", "request_id": "r3",
                "path": "/Users/me/src/app", "agent": "claude",
                "model": "claude-fable-5-1[1m]"}),
        );
        round_trip(
            AgentAvailability {
                agent: crate::agent::AgentKind::Codex,
                available: true,
                detail: None,
                models: vec![
                    AgentModel {
                        value: "gpt-6.1-sol".into(),
                        label: "GPT-6.1-Sol".into(),
                        detail: Some("Latest workhorse model for coding and everyday work.".into()),
                    },
                    AgentModel {
                        value: "local".into(),
                        label: "Local".into(),
                        detail: None,
                    },
                ],
                signed_in: Some(false),
            },
            serde_json::json!({"agent": "codex", "available": true,
                "models": [
                    {"value": "gpt-6.1-sol", "label": "GPT-6.1-Sol",
                     "detail": "Latest workhorse model for coding and everyday work."},
                    {"value": "local", "label": "Local"}],
                "signed_in": false}),
        );
    }

    /// A minor-27 ack has no `start_session`, so a client reading it offers no
    /// new session; this daemon's own ack says it.
    #[test]
    fn start_session_is_absent_from_an_older_ack_and_present_in_this_one() {
        let old: Capabilities = serde_json::from_str(
            r#"{"can_approve_reliably":true,"fail_mode":"fail_open",
                "answer_path":"hook_return","hold_secs":0,"send_text":true,
                "capture":true,"push":false,"tls":true,"history_pages":true}"#,
        )
        .unwrap();
        assert!(!old.start_session);

        let current = serde_json::to_value(capabilities_fixture()).unwrap();
        assert_eq!(current["start_session"], true);
    }

    /// **The branch and worktree wire, pinned as the phone matches it** (minor 29).
    #[test]
    fn the_worktree_messages_round_trip_as_the_phone_matches_them() {
        fn round_trip<T>(message: T, expected: serde_json::Value)
        where
            T: Serialize + serde::de::DeserializeOwned,
        {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            let decoded: T = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }

        round_trip(
            ClientMessage::GetProjectGit {
                path: "/Users/me/src/app".into(),
            },
            serde_json::json!({"type": "get_project_git", "path": "/Users/me/src/app"}),
        );
        for (worktree, expected) in [
            (
                StartWorktree::Existing {
                    path: "/Users/me/src/app/.claude/worktrees/fix".into(),
                },
                serde_json::json!({"kind": "existing",
                    "path": "/Users/me/src/app/.claude/worktrees/fix"}),
            ),
            (
                StartWorktree::Branch {
                    branch: "feature/search".into(),
                },
                serde_json::json!({"kind": "branch", "branch": "feature/search"}),
            ),
            (
                StartWorktree::New {
                    name: "fix-login".into(),
                },
                serde_json::json!({"kind": "new", "name": "fix-login"}),
            ),
        ] {
            round_trip(
                ClientMessage::StartSession {
                    request_id: "r1".into(),
                    path: "/Users/me/src/app".into(),
                    agent: crate::agent::AgentKind::Claude,
                    prompt: None,
                    worktree: Some(worktree),
                    model: None,
                    trust: None,
                    attachments: Vec::new(),
                },
                serde_json::json!({"type": "start_session", "request_id": "r1",
                    "path": "/Users/me/src/app", "agent": "claude", "worktree": expected}),
            );
        }

        round_trip(
            ServerMessage::ProjectGit {
                path: "/Users/me/src/app".into(),
                git: Some(ProjectGit {
                    worktrees: vec![
                        Worktree {
                            path: "/Users/me/src/app".into(),
                            main: true,
                            branch: Some("main".into()),
                            dirty: true,
                            missing: false,
                        },
                        Worktree {
                            path: "/Users/me/src/app-old".into(),
                            main: false,
                            branch: None,
                            dirty: false,
                            missing: true,
                        },
                    ],
                    branches: vec!["feature/search".into()],
                    branches_truncated: false,
                }),
            },
            serde_json::json!({"type": "project_git", "path": "/Users/me/src/app", "git": {
                "worktrees": [
                    {"path": "/Users/me/src/app", "main": true, "branch": "main",
                     "dirty": true, "missing": false},
                    {"path": "/Users/me/src/app-old", "main": false,
                     "dirty": false, "missing": true}],
                "branches": ["feature/search"], "branches_truncated": false}}),
        );
        round_trip(
            ServerMessage::ProjectGit {
                path: "/Users/me/notes".into(),
                git: None,
            },
            serde_json::json!({"type": "project_git", "path": "/Users/me/notes"}),
        );
        round_trip(
            ServerMessage::StartSessionResult {
                request_id: "r1".into(),
                result: StartSessionResult::Started {
                    session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
                    cwd: Some("/Users/me/src/app/.claude/worktrees/feature+search".into()),
                },
            },
            serde_json::json!({"type": "start_session_result", "request_id": "r1",
                "result": {"status": "started", "session_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                           "cwd": "/Users/me/src/app/.claude/worktrees/feature+search"}}),
        );
    }

    /// A minor-28 ack has no `start_worktree`, so a client reading it offers no
    /// branch or worktree choice; this daemon's own ack says it.
    #[test]
    fn start_worktree_is_absent_from_an_older_ack_and_present_in_this_one() {
        let old: Capabilities = serde_json::from_str(
            r#"{"can_approve_reliably":true,"fail_mode":"fail_open",
                "answer_path":"hook_return","hold_secs":0,"send_text":true,
                "capture":true,"push":false,"tls":true,"start_session":true}"#,
        )
        .unwrap();
        assert!(old.start_session && !old.start_worktree);

        let current = serde_json::to_value(capabilities_fixture()).unwrap();
        assert_eq!(current["start_worktree"], true);
    }

    /// **The folder wire, pinned as the phone matches it** (minor 30).
    #[test]
    fn the_folder_messages_round_trip_as_the_phone_matches_them() {
        fn round_trip<T>(message: T, expected: serde_json::Value)
        where
            T: Serialize + serde::de::DeserializeOwned,
        {
            assert_eq!(serde_json::to_value(&message).unwrap(), expected);
            let decoded: T = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        }

        round_trip(
            ClientMessage::ListFolder {
                request_id: "b0".into(),
                path: None,
                show_hidden: false,
            },
            serde_json::json!({"type": "list_folder", "request_id": "b0"}),
        );
        round_trip(
            ClientMessage::ListFolder {
                request_id: "b1".into(),
                path: Some("/Users/me/src".into()),
                show_hidden: true,
            },
            serde_json::json!({"type": "list_folder", "request_id": "b1",
                "path": "/Users/me/src", "show_hidden": true}),
        );
        round_trip(
            ServerMessage::FolderListing {
                request_id: "b1".into(),
                path: "/Users/me/src".into(),
                contents: FolderContents::Listed {
                    folders: vec!["app".into(), "semi;".into()],
                    truncated: false,
                },
            },
            serde_json::json!({"type": "folder_listing", "request_id": "b1",
                "path": "/Users/me/src", "folders": ["app", "semi;"], "truncated": false}),
        );
        round_trip(
            ServerMessage::FolderListing {
                request_id: "b2".into(),
                path: "/Users/me/locked".into(),
                contents: FolderContents::Unreadable {
                    error: "Permission denied".into(),
                },
            },
            serde_json::json!({"type": "folder_listing", "request_id": "b2",
                "path": "/Users/me/locked", "error": "Permission denied"}),
        );
        round_trip(
            ClientMessage::CreateFolder {
                request_id: "c1".into(),
                parent: "/Users/me/src".into(),
                name: "new-app".into(),
            },
            serde_json::json!({"type": "create_folder", "request_id": "c1",
                "parent": "/Users/me/src", "name": "new-app"}),
        );
        round_trip(
            ServerMessage::CreateFolderResult {
                request_id: "c1".into(),
                result: CreateFolderResult::Created,
            },
            serde_json::json!({"type": "create_folder_result", "request_id": "c1",
                "result": {"status": "created"}}),
        );
        round_trip(
            ServerMessage::CreateFolderResult {
                request_id: "c2".into(),
                result: CreateFolderResult::Failed {
                    detail: "File exists".into(),
                },
            },
            serde_json::json!({"type": "create_folder_result", "request_id": "c2",
                "result": {"status": "failed", "detail": "File exists"}}),
        );
    }

    /// A minor-29 ack has no `browse_folders`, so a client reading it offers no
    /// folder browsing; this daemon's own ack says it.
    #[test]
    fn browse_folders_is_absent_from_an_older_ack_and_present_in_this_one() {
        let old: Capabilities = serde_json::from_str(
            r#"{"can_approve_reliably":true,"fail_mode":"fail_open",
                "answer_path":"hook_return","hold_secs":0,"send_text":true,
                "capture":true,"push":false,"tls":true,"start_session":true,
                "start_worktree":true}"#,
        )
        .unwrap();
        assert!(old.start_worktree && !old.browse_folders);

        let current = serde_json::to_value(capabilities_fixture()).unwrap();
        assert_eq!(current["browse_folders"], true);
    }

    /// `machine_stats` (minor 31): absent from a minor-30 ack, so a client there
    /// never asks; the request and its answer read as written, and a value the
    /// computer could not read is left off rather than sent as zero.
    #[test]
    fn machine_stats_is_gated_and_reads_as_written() {
        let old: Capabilities = serde_json::from_str(
            r#"{"can_approve_reliably":true,"fail_mode":"fail_open",
                "answer_path":"hook_return","hold_secs":0,"send_text":true,
                "capture":true,"push":false,"tls":true,"browse_folders":true}"#,
        )
        .unwrap();
        assert!(old.browse_folders && !old.machine_stats);
        let current = serde_json::to_value(capabilities_fixture()).unwrap();
        assert_eq!(current["machine_stats"], true);

        let request: ClientMessage =
            serde_json::from_str(r#"{"type":"get_machine_stats"}"#).unwrap();
        assert!(matches!(request, ClientMessage::GetMachineStats));

        let reply = ServerMessage::MachineStats {
            cpu: Some(0.38),
            memory_used: Some(71 << 30),
            memory_total: Some(192 << 30),
            disk_used: None,
            disk_total: None,
            captured_at: "t".into(),
        };
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::json!({"type": "machine_stats", "cpu": 0.38,
                "memory_used": 76_235_669_504u64, "memory_total": 206_158_430_208u64,
                "captured_at": "t"}),
        );
        let ServerMessage::MachineStats { cpu, disk_used, .. } =
            serde_json::from_str(r#"{"type":"machine_stats","captured_at":"t"}"#).unwrap()
        else {
            panic!("wrong message");
        };
        assert_eq!((cpu, disk_used), (None, None));
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
            files: None,
            files_truncated: false,
        };
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.contains("\"type\":\"diff\""), "{encoded}");
        assert!(!encoded.contains("note"), "an absent note must not be sent");
        assert!(!encoded.contains("files"), "{encoded}");
        match serde_json::from_str(&encoded).unwrap() {
            ServerMessage::Diff {
                files,
                files_truncated,
                ..
            } => assert_eq!(
                (files, files_truncated),
                (None, false),
                "a minor-51 diff decodes with no files"
            ),
            other => panic!("wrong message: {other:?}"),
        }
    }

    /// **A diff's files on the wire**: snake-case statuses, a rename's old path, a
    /// binary file with no counts and no `binary: false` noise on a text one.
    #[test]
    fn a_diffs_files_encode_as_the_apps_read_them() {
        let files = vec![
            DiffFile {
                path: "b.txt".into(),
                old_path: Some("a.txt".into()),
                status: DiffFileStatus::Renamed,
                added: Some(0),
                removed: Some(0),
                binary: false,
            },
            DiffFile {
                path: "img.png".into(),
                old_path: None,
                status: DiffFileStatus::TypeChanged,
                added: None,
                removed: None,
                binary: true,
            },
        ];
        let reply = ServerMessage::Diff {
            session_id: "cc-1".into(),
            unified: String::new(),
            truncated: false,
            captured_at: "t".into(),
            note: None,
            files: Some(files),
            files_truncated: true,
        };
        let encoded = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            encoded["files"],
            serde_json::json!([
                {"path": "b.txt", "old_path": "a.txt", "status": "renamed", "added": 0, "removed": 0},
                {"path": "img.png", "status": "type_changed", "binary": true},
            ])
        );
        assert_eq!(encoded["files_truncated"], true);
        let decoded: ServerMessage = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
    }

    /// **A pull request from the Changes panel, as the apps send and read it** (minor 61): the
    /// action has no arguments, its `done` carries `pr` and no `sha`, and a commit's `done`
    /// still carries `sha` alone.
    #[test]
    fn a_pull_request_action_encodes_as_the_apps_read_it() {
        let ask: ClientMessage = serde_json::from_str(
            r#"{"type":"git_action","request_id":"p-1","session_uid":"u","action":{"kind":"create_pr"}}"#,
        )
        .unwrap();
        assert!(
            matches!(ask, ClientMessage::GitAction { ref action, .. } if *action == GitAction::CreatePr),
            "{ask:?}"
        );
        let opened = GitActionResult::Done {
            sha: None,
            pr: Some(PrLink {
                number: 7,
                url: "https://github.com/o/r/pull/7".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&opened).unwrap(),
            serde_json::json!({"status": "done", "pr": {"number": 7, "url": "https://github.com/o/r/pull/7"}})
        );
        let committed = GitActionResult::Done {
            sha: Some("abc".into()),
            pr: None,
        };
        assert_eq!(
            serde_json::to_value(&committed).unwrap(),
            serde_json::json!({"status": "done", "sha": "abc"})
        );
        let merge: ClientMessage = serde_json::from_str(
            r#"{"type":"git_action","request_id":"m-1","session_uid":"u","action":{"kind":"merge","method":"squash","head_oid":"abc","number":7}}"#,
        )
        .unwrap();
        assert!(
            matches!(merge, ClientMessage::GitAction { ref action, .. }
                if *action == GitAction::Merge { method: MergeMethod::Squash, head_oid: "abc".into(), number: 7 }),
            "{merge:?}"
        );
        for done in [opened, committed] {
            let back: GitActionResult =
                serde_json::from_value(serde_json::to_value(&done).unwrap()).unwrap();
            assert_eq!(back, done);
        }
    }

    /// **A run's repository, as the apps read it**: the request by session, the base's
    /// `ref` spelled as git spells it, absent fields omitted, and a repository-less answer
    /// as a bare note.
    #[test]
    fn a_runs_repository_encodes_as_the_apps_read_it() {
        let ask: ClientMessage =
            serde_json::from_str(r#"{"type":"get_run_git","session_id":"cc-1"}"#).unwrap();
        assert!(matches!(ask, ClientMessage::GetRunGit { ref session_id } if session_id == "cc-1"));
        let reply = ServerMessage::RunGit {
            session_id: "cc-1".into(),
            git: Some(RunGit {
                branch: Some("feat/x".into()),
                head_oid: Some("abc".into()),
                base: Some(RunGitBase {
                    name: "main".into(),
                    remote: "origin".into(),
                    reference: "refs/remotes/origin/main".into(),
                }),
                changed: ChangedFiles {
                    files: 2,
                    added: 5,
                    removed: 1,
                    complete: true,
                },
                ahead: Some(1),
                pushed: false,
                unmerged: false,
                read_at: "t".into(),
                gh: Some(GhState::SignedIn),
                gh_host: Some("github.com".into()),
                pr: Some(PullRequest {
                    number: 7,
                    url: "https://github.com/o/r/pull/7".into(),
                    state: PrState::Open,
                    base: "main".into(),
                    head_oid: "abc".into(),
                    merged_at: None,
                    checks: Some(vec![Check {
                        name: "test".into(),
                        state: CheckState::Fail,
                        started_at: Some("s".into()),
                        completed_at: None,
                        url: Some("l".into()),
                        failure_line: Some("expected 2, got 3".into()),
                    }]),
                    checks_complete: Some(true),
                    merge_state: Some(MergeState::Blocked),
                    merge_methods: Some(vec![MergeMethod::Squash]),
                    merge_queued: Some(false),
                }),
                gh_read_at: Some("u".into()),
                gh_repo: Some("o/r".into()),
            }),
            note: None,
        };
        let encoded = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "run_git", "session_id": "cc-1",
                "git": {
                    "branch": "feat/x", "head_oid": "abc",
                    "base": {"name": "main", "remote": "origin", "ref": "refs/remotes/origin/main"},
                    "changed": {"files": 2, "added": 5, "removed": 1, "complete": true},
                    "ahead": 1, "pushed": false, "unmerged": false, "read_at": "t",
                    "gh": "signed_in", "gh_host": "github.com", "gh_read_at": "u", "gh_repo": "o/r",
                    "pr": {"number": 7, "url": "https://github.com/o/r/pull/7", "state": "open",
                           "base": "main", "head_oid": "abc", "checks_complete": true,
                           "checks": [{"name": "test", "state": "fail", "started_at": "s", "url": "l",
                                       "failure_line": "expected 2, got 3"}],
                           "merge_state": "blocked", "merge_methods": ["squash"], "merge_queued": false},
                },
            })
        );
        let decoded: ServerMessage = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
        let bare = ServerMessage::RunGit {
            session_id: "cc-1".into(),
            git: None,
            note: Some("not a git repository".into()),
        };
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            serde_json::json!({"type": "run_git", "session_id": "cc-1", "note": "not a git repository"})
        );
    }

    #[test]
    fn a_note_distinguishes_clean_from_not_a_repository() {
        let encoded = serde_json::to_string(&ServerMessage::Diff {
            session_id: "cc-1".into(),
            unified: String::new(),
            truncated: false,
            captured_at: "t".into(),
            note: Some("not a git repository".into()),
            files: None,
            files_truncated: false,
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
            question_hold: None,
            allow_for_session: false,
        };
        assert!(!serde_json::to_string(&card).unwrap().contains("risk"));

        card.risk = Some(crate::risk::classify(&card.tool_name, &card.tool_input));
        let encoded = serde_json::to_string(&card).unwrap();
        assert!(encoded.contains(r#""class":"high""#), "{encoded}");
        assert!(encoded.contains("matched_pattern"), "{encoded}");
        assert_eq!(card, serde_json::from_str(&encoded).unwrap());
    }

    /// **An outcome carries no `inferred`**: one recorded with it reads back without it.
    #[test]
    fn an_outcome_reads_back_without_inferred() {
        let recorded = r#"{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"deny"},
            "resolved_by":"timeout","applied_via":"hook_return","resolved_at":"t","inferred":true}"#;
        let outcome: AnswerOutcome = serde_json::from_str(recorded).unwrap();
        assert!(!serde_json::to_string(&outcome)
            .unwrap()
            .contains("inferred"));
    }

    /// **An answer is resolved by `phone` or `timeout`**, spelled by hand, and `local` and
    /// `superseded` do not decode.
    #[test]
    fn an_answer_is_resolved_by_phone_or_timeout() {
        for (by, word) in [
            (ResolvedBy::Phone, "phone"),
            (ResolvedBy::Timeout, "timeout"),
        ] {
            let encoded = serde_json::to_string(&by).unwrap();
            assert_eq!(encoded, format!("\"{word}\""));
            assert_eq!(serde_json::from_str::<ResolvedBy>(&encoded).unwrap(), by);
        }
        for gone in ["\"local\"", "\"superseded\""] {
            assert!(serde_json::from_str::<ResolvedBy>(gone).is_err(), "{gone}");
        }
    }

    /// **The capabilities carry no `fail_mode`, `answer_path` or `hold_secs`**, constants no
    /// app reads.
    #[test]
    fn the_capabilities_carry_no_fail_mode_answer_path_or_hold_secs() {
        let sent = serde_json::to_value(capabilities_fixture()).unwrap();
        for gone in ["fail_mode", "answer_path", "hold_secs"] {
            assert!(sent.get(gone).is_none(), "{gone}");
        }
    }

    /// **An answer is applied as `hook_return` or `codex_response`**, spelled by hand,
    /// and `send_keys` does not decode.
    #[test]
    fn an_answer_is_applied_as_a_hook_return_or_a_codex_response() {
        for (path, word) in [
            (AnswerPath::HookReturn, "hook_return"),
            (AnswerPath::CodexResponse, "codex_response"),
        ] {
            let encoded = serde_json::to_string(&path).unwrap();
            assert_eq!(encoded, format!("\"{word}\""));
            assert_eq!(serde_json::from_str::<AnswerPath>(&encoded).unwrap(), path);
        }
        assert!(serde_json::from_str::<AnswerPath>("\"send_keys\"").is_err());
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
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: "2026-07-30T16:36:58.412Z".into(),
            detail: None,
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
