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
        /// The app's ed25519 SSH public key, offered during pairing.
        ///
        /// Offering it is always allowed; *installing* it is not. The daemon
        /// appends it to `~/.ssh/authorized_keys` only when the operator ran
        /// `codeconnect pair --ssh`, because consent to hand out shell access has to be
        /// given at the Mac's keyboard and cannot be requested by the peer that
        /// benefits from it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_pubkey: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_name: Option<String>,
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
        /// Whether the offered `ssh_pubkey` was actually installed. Reported
        /// even when false, so the app can tell "the operator did not consent"
        /// from "the key was never offered" and pick its SSH auth accordingly
        /// instead of failing at connect time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_key_installed: Option<bool>,
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
    Pong,
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
    /// APNs wired to a real key. False while the sender is the logging stub.
    pub push: bool,
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
    /// The daemon has no APNs key configured; the sender is the logging stub.
    PushUnconfigured,
    /// This connection authenticated with the static bootstrap token, so there
    /// is no device row — and no token — to send to.
    NotPairedDevice,
    /// The device exists but has never registered an APNs token (notifications
    /// were never enabled, or registration has not completed yet).
    NoRegisteredToken,
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

    #[test]
    fn hello_round_trip() {
        let msg = ClientMessage::Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            token: Some("t".into()),
            pairing_code: None,
            ssh_pubkey: None,
            client_id: None,
            client_name: Some("iPhone".into()),
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"type\":\"hello\""));
        assert!(!s.contains("client_id"));
        assert!(!s.contains("pairing_code"));
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
                ssh_pubkey,
                ..
            } => {
                assert_eq!(token.as_deref(), Some("deadbeef"));
                assert_eq!(pairing_code, None);
                assert_eq!(ssh_pubkey, None);
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn a_pairing_hello_carries_no_token() {
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"hello","protocol_version":1,"pairing_code":"ABCD2345",
                "ssh_pubkey":"ssh-ed25519 AAAAC3Nz phone","client_name":"iPhone"}"#,
        )
        .unwrap();
        match msg {
            ClientMessage::Hello {
                token,
                pairing_code,
                ssh_pubkey,
                ..
            } => {
                assert_eq!(token, None);
                assert_eq!(pairing_code.as_deref(), Some("ABCD2345"));
                assert!(ssh_pubkey.is_some());
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
            ssh_key_installed: None,
        };
        let encoded = serde_json::to_string(&ack).unwrap();
        assert!(!encoded.contains("device_token"), "{encoded}");
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
        const _: () = assert!(crate::PROTOCOL_VERSION == 1, "no breaking change was made");
        // The equality is the point: every bump has to come here and say what it
        // added, so the list above stays a record rather than a guess.
        assert_eq!(crate::PROTOCOL_MINOR, 9);
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
            tls: true,
            tls_active: true,
            diff: true,
            risk_class: true,
            session_uid: true,
            send_text_idempotent: true,
            prompt_identity: true,
            command_catalog: true,
            slash_composer_recovery: true,
        }
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
