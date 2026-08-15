//! Shared CodeConnect wire types.
//!
//! Three surfaces live here so `ccd`, `cc` and `cc-hook` can never disagree:
//!   * [`event`] — the daemon-assigned event log envelope (source of truth).
//!   * [`config`] — one config file, shared: the shim generates hooks that
//!     match exactly what the daemon expects to gate on.
//!   * [`ipc`]   — unix-socket frames (hook posts + supervisor registration).
//!   * [`ws`]    — the tailnet WebSocket protocol the iPhone speaks.
//!
//! [`tmux`] is here for the same reason, even though it is not a wire type: two
//! processes ask tmux whether a session is still alive, and a machine where they
//! answered that differently would report exits that never happened. The
//! question is shared vocabulary; only the way each crate runs a child is not.
//!
//! Everything is plain serde JSON. Unknown fields are tolerated on decode and
//! unknown event kinds round-trip as [`event::EventKind::Other`], so a newer
//! daemon can add facts without breaking an older client (additive-only rule).

use std::path::PathBuf;

pub mod build_identity;
pub mod config;
pub mod event;
pub mod fsperm;
pub mod hash;
pub mod hook;
pub mod ipc;
pub mod pairing;
pub mod proc;
pub mod risk;
pub mod secret;
pub mod time;
pub mod tmux;
pub mod uid;
pub mod ws;

/// Bumped only on breaking changes; negotiated in `hello`/`hello_ack`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Bumped on every *additive* change, and reported in `hello_ack`.
///
/// The major version answers "can we talk at all"; this answers "what may I
/// assume is present". A client that needs `TurnComplete` or `get_diff` tests
/// `protocol_minor >= 1` rather than probing for the feature and guessing from
/// the silence — which is the same honesty rule the event log follows.
///
///   * `0` — hello/sessions/subscribe/answer/send_text/capture.
///   * `1` — QR pairing + per-device tokens, `wss://`, `get_diff`,
///     `EventKind::TurnComplete`, `ResolvedBy::Local`, `risk_class`.
///   * `2` — `session_uid` on every session and event, accepted wherever a
///     `session_id` is accepted, and `answer{session_id}`. A client on minor 2
///     keys its local store by `session_uid`; one on minor 1 keeps using
///     `session_id` and gets the newest run under that name.
///   * `3` — `send_text{request_id, payload_hash}` (idempotent, and the
///     server now chooses the interlock — `send_text.require` is ignored),
///     `SendTextResult::{Duplicate, Indeterminate}`, `ApprovalCard{generation,
///     identity_bound}`, `AnswerOutcome{indeterminate}`, and the
///     `send_text_idempotent` / `prompt_identity` capabilities. The supervisor
///     side of the same number is [`ipc::RegisterSession::protocol_minor`]:
///     minor 3 is what makes a supervisor able to honour a prompt fingerprint.
///   * `4` — hardening. Everything here is additive, and a client written
///     against minor 3 needs no change:
///       - a new `error.code`, `protocol_mismatch`, sent to a client whose
///         **major** differs. Minor 3 clients speak major 1 and never see it —
///         the majors have not moved. What did change is that a mismatched major
///         is now *refused* rather than warned about and admitted.
///       - a new `error.code`, `message_too_large`, for a reply that would not
///         fit one WebSocket frame.
///       - an `event` whose payload carries `codeconnect_truncated: true` in
///         place of an oversized one. Same envelope, same `seq`, so a client
///         that does not know the key still advances its watermark correctly.
///       - two new [`event::EventKind::Error`] payload shapes from the
///         transcript tailer, discriminated by an `error` field:
///         `transcript_line_too_long` and `transcript_line_unreadable`. `Error`
///         is not a new kind, so an older client renders them as it already
///         renders any error.
///   * `5` — **`lifecycle` is reconciled rather than merely remembered.** Up to
///     minor 4 the only thing that could ever set [`event::Lifecycle::Exited`]
///     was a supervisor reporting its own exit, so a session whose supervisor
///     was killed, or that died while `ccd` was down, stayed `live` in the fleet
///     for ever. A client had no way to know that, and rendered "Running" for
///     agents that had been gone for days. From minor 5 the daemon proves
///     liveness against tmux at startup and on a sweep, so `live` means it has
///     positive evidence rather than an absence of news. Additive on the wire:
///       - a `session_end` the daemon *derived* that way carries a `reason`
///         string alongside the existing `exit_code`, so the log says how the
///         end was established rather than implying somebody watched it happen.
///         The envelope and the kind are unchanged.
///       - nothing is ever marked exited on ambiguous evidence, so
///         [`event::Lifecycle::Unknown`] remains a state a client must render.
///   * `6` — push. A new client message, [`ws::ClientMessage::RegisterPush`],
///     carrying an APNs token and the environment it was issued for, plus the
///     `push` capability that says the daemon can act on one. Separate from
///     `hello` because notification permission can be granted or revoked at any
///     point in a session's life. Additive: a client that never sends it is
///     unchanged.
///   * `7` — a client can delete one ended run. A new client message,
///     [`ws::ClientMessage::DeleteSession`], a new
///     [`ws::ServerMessage::DeleteSessionResult`] answering it, and the
///     `delete_session` capability. Three things a client must know:
///       - it names the run by `session_uid` and never by `session_id`. A tmux
///         name is handed to the next run, so a name is not an identity a
///         destructive verb may be pointed at.
///       - for a run the daemon hosts, it refuses anything not `Exited`, and
///         anything it still holds live state for. `still_running` and
///         `not_exited` are complete answers, not errors. A run it never hosted
///         — an adopted session, recognisable by its empty `tmux_session` in
///         the summary — is deletable at any lifecycle: no probe can ever prove
///         such a run ended, and a record that cannot be removed until an
///         unobtainable proof arrives would be immortal. Claude Code's own
///         transcript is untouched either way.
///       - the daemon deletes its own record. Claude Code's transcript is its
///         own file and is untouched, so `claude --resume` still works
///         afterwards.
///
///     Also in 7: [`ws::ClientMessage::TestPush`] and its
///     [`ws::ServerMessage::TestPushResult`], gated by the `test_push`
///     capability — one real APNs notification to the requesting device, so
///     the doorbell can be proven rather than trusted. Refused for
///     static-token connections and rate-limited per device.
///
///   * `8` — a client can ask which slash commands the session's Claude Code
///     actually has. [`ws::ClientMessage::GetCommandCatalog`], answered by
///     [`ws::ServerMessage::CommandCatalog`], gated by the `command_catalog`
///     capability. The list is read from the installed binary's own
///     machine-readable init message and cached per binary fingerprint —
///     never hand-maintained, so a Claude Code upgrade changes the answer
///     instead of rotting a copy. `unavailable` is a complete answer: the
///     phone falls back to its conservative static policy, never to guessing.
///   * `9` — the daemon closes a Mac view its own injection opened.
///     Measured problem: `/status`, `/usage`, `/help`, `/export`, `/diff`
///     and bare `/model` replace Claude's composer, and while it is gone the
///     presence interlock refuses every further send — a phone that typed
///     one was locked out of its own session until somebody pressed Esc at
///     the Mac. From minor 9 the supervisor checks the composer after any
///     word-shaped slash injection, sends exactly one `Escape` if it is
///     gone, and reports what it observed:
///     [`ws::SendTextResult::ComposerRecovered`] (with the pane it captured,
///     for `/status`, `/usage` and `/cost` only) or
///     [`ws::SendTextResult::ComposerLost`] when one Escape was not enough —
///     measured on `/config`, and on `/keybindings`, which opens an editor.
///     Advertised as the `slash_composer_recovery` capability, and enforced
///     per session: a supervisor below minor 9 accepts the request fields and
///     drops them, so the daemon refuses word-shaped slash commands for that
///     session rather than typing one it cannot rescue.
///
///     **The new statuses are sent to every client**, because a `hello`
///     carries no client minor for the daemon to branch on. That is safe
///     forwards — a client built against this or later knows them — and it is
///     the reason `SendTextResult` decoding should treat an unknown status as
///     indeterminate rather than as a decode failure.
///   * `10` — the daemon *completes* a confirmation its own injection
///     opened, rather than dismissing it. `/model <value>` and
///     `/effort <value>` make Claude Code ask before switching, and the
///     composer-recovery Escape from minor 9 answered "no" to a question the
///     human had already answered on the phone. From minor 10 the supervisor
///     may send `Enter` instead, but only when the client set
///     `complete_native_confirmation`, the command is one of those two with an
///     argument, and the pane it captured still shows that argument selected.
///     Advertised per session, so a supervisor below minor 10 keeps escaping.
///   * `13` — a client can open a **live terminal** on a hosted session over
///     this same connection. `terminal_attach`/`terminal_input`/
///     `terminal_resize`/`terminal_credit`/`terminal_detach` from the client;
///     `terminal_attached`/`terminal_output`/`terminal_credit`/
///     `terminal_closed` from the daemon; gated by the `terminal_pty`
///     capability. Bytes ride as base64 in the JSON frames, flow-controlled by
///     a credit window in each direction (no sequence number — the socket is
///     ordered). The daemon speaks tmux **control mode** through a disposable
///     `tmux -N -C attach -f ignore-size` client against the exact session:
///     keystrokes are delivered with `send-keys` to a scoped
///     `session:window.pane` target — the bound pane the phone is shown, so a
///     migrated pane fails closed — and can never be interpreted as a tmux
///     control-protocol command (the shell in the pane can of course run `tmux`
///     itself — a terminal is a real shell); the phone never resizes a human at
///     the Mac; and the tmux
///     server, agent and supervisor outlive the attachment. The first bytes
///     after `terminal_attached` repaint the pane's current screen (clear,
///     rows, cursor), so an attach shows the session as it stands rather than
///     a blank viewport waiting for new output — ordinary `terminal_output`
///     bytes, spending credit like any others. Because a terminal
///     is shell-equivalent authority, the capability is connection-scoped:
///     false for the static bootstrap token.
///     Additive: an older daemon omits the capability and the phone offers no
///     Terminal; an older client ignores the messages.
///
///     *Shipped under 13 without adding to the wire.* The composer is now
///     recognised by the box Claude draws it in rather than by footer copy that
///     yields to other hints, and a pane where tmux holds the keyboard refuses
///     a send instead of reporting one that never arrived. No message, field or
///     capability changed — but [`ws::SendTextResult::Sent`]`.matched` gained a
///     value it can carry, the literal `composer`, for a send no needle
///     authorised. It is a reason, not an enumerable set; a client renders it
///     and must not match on it.
///   * `12` — a send carries **when its asker stops listening**.
///     `SupervisorRequest::SendText.respond_by_monotonic_ms` stamps the
///     daemon's own answer deadline, on the host's monotonic clock, into the
///     request. The supervisor budgets every deliberate wait against it —
///     stopping recovery honestly when the remainder cannot fit the next
///     step — so an answer computed in time is delivered in time, including
///     time the request spent queued. Additive: an older supervisor ignores
///     the field and budgets from its own config; an older daemon omits it.
///   * `11` — a run carries the **project** it is working in.
///     `SessionSummary.project_label` is the final component of `cwd` — trimmed,
///     stripped of anything that would break a line, and bounded — resolved by
///     the daemon rather than by each client, so that every surface naming a
///     run names it the same. A client must not compute its own from `cwd`:
///     two rules produce two names for one run. Empty when `cwd` names nothing, and
///     empty from any daemon below this minor — the two mean the same thing to a
///     reader: nobody has said what this project is. A client says so rather
///     than falling back to the tmux name, which is a reused counter.
///     Additive; an older client ignores the field and an older daemon omits it.
///     The notification now names that project and nothing else: the title is
///     the label (or `CodeConnect` when there is none), and the body is one of
///     four canned sentences chosen by the hook's kind — or `{n} agents need
///     you` once more than one session is blocked. The tool name and the
///     risk class it used to carry are gone.
///     A tap opens the phone's decision list when an approval rang, and the
///     fleet otherwise — the list, never a particular card. The payload carries
///     no routing, session, request or device identifier, only which of four
///     kinds rang — so there is nothing in it that could point at a decision
///     somebody has since answered. The words themselves are a snapshot, like
///     any notification's.
///     Every surface that names a run — the fleet list, the decision card, the
///     session header, the diff title and the notification — takes its name
///     from this field and never derives one of its own, so a name on a lock
///     screen and a name in the app are the same name rather than two rules'
///     answers. A client may *add* to it where a screen has to tell two runs in
///     one project apart; it may not replace it. A run that changes directory
///     while a card is open takes that card with it: every writer of a run's
///     `cwd` relabels the cards it is holding, so a doorbell names the project
///     the run is in when it rings, which is the project the app is showing.
///
///     **One bound, stated rather than implied.** A card takes its run's name
///     when it is filed, and filing is not atomic with the relabel: a card
///     raised in the same instant a run moves can be inserted carrying the
///     previous name. It is corrected by the next thing that writes that run's
///     `cwd`, and it is gone when the card resolves. The daemon does not
///     serialise every hook behind a database write to close that instant — a
///     doorbell is best-effort by construction, the app reconciles from the
///     event log, and the cost of the alternative is paid by every hook.
///   * `14` — a daemon with **no Apple key of its own** can ring a phone. The
///     key cannot be shipped to a customer's Mac, so a CodeConnect-operated
///     relay holds it: the daemon hands over a closed event document — which of
///     four kinds rang, how many runs are blocked, the device's token and an
///     opaque credential — and the relay composes a generic alert and talks to
///     Apple. Every decision about *whether* to ring stays on the Mac. Four
///     additions, and a client written against minor 13 needs none of them:
///       - [`ws::Capabilities::push_relay`] — this daemon sends through the
///         relay. **One-hot with the legacy `push`,** which from here means
///         *direct key* and nothing else: a relay daemon advertises
///         `push = false` on purpose, because a client that predates this minor
///         would otherwise ask for notification permission and register a token
///         with no credential attached, and every send would be refused. A
///         client on this minor resolves the two flags with `push` taking
///         precedence.
///       - [`ws::ClientMessage::RegisterPush`]`.relay_credential` — the opaque
///         bearer the phone obtained from the relay, absent for a direct
///         registration. The daemon stores it beside the token and presents it
///         on every send; it never mints one, and never sees the attestation
///         that authorised it.
///       - [`ws::TestPushResult::CredentialInvalid`] — the relay refused the
///         credential. Distinct from `no_registered_token`, and the distinction
///         is the whole point: the APNs token is still good, so a client that
///         read this as a dead token would throw away a working registration
///         instead of renewing a bearer.
///       - `hello_ack.push_environment` — the daemon's authoritative APNs
///         environment for this device's registered token, absent when no token
///         is registered. The relay's binding is the single authority for a
///         token's environment and corrects the daemon on an accepted send;
///         this is how that correction reaches the phone, which persists it
///         rather than resending the value it first cached.
pub const PROTOCOL_MINOR: u32 = 14;

/// Private tmux server name. Never the user's default server.
pub const TMUX_SOCKET_NAME: &str = "codeconnect";

/// Session names are `cc-<n>`; the prefix is also the tmux session prefix.
///
/// The name is reused: `codeconnect claude` picks the lowest free number, so a `cc-1`
/// that exits frees the name for the next session. That is deliberate — it is
/// what keeps names short and typeable — and it is exactly why the *identity*
/// of a run is [`uid`], not this.
pub const SESSION_PREFIX: &str = "cc-";

/// Environment variable carrying the CodeConnect session id into the agent
/// process, so hooks can identify themselves even without an explicit `--session`.
pub const ENV_SESSION: &str = "CODECONNECT_SESSION";

/// Environment variable carrying the session's unique id, for the same reason
/// and with the same fallback role as [`ENV_SESSION`].
pub const ENV_SESSION_UID: &str = "CODECONNECT_SESSION_UID";

/// The LaunchAgent label. One constant so `codeconnect daemon install`, `codeconnect daemon
/// status` and the daemon's own "am I launchd-managed?" check can never disagree
/// about which job they are talking about.
pub const LAUNCHD_LABEL: &str = "com.codeconnect.ccd";

/// Root of all CodeConnect state. `CODECONNECT_HOME` exists so tests never
/// touch the real `~/.codeconnect`.
pub fn root_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CODECONNECT_HOME") {
        return PathBuf::from(dir);
    }
    home_dir().join(".codeconnect")
}

/// `$HOME`, falling back to the current directory so nothing panics in a
/// launchd context with a stripped environment.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn socket_path() -> PathBuf {
    root_dir().join("ccd.sock")
}

pub fn db_path() -> PathBuf {
    root_dir().join("events.db")
}

/// The static bearer token. QR-delivered per-device tokens live alongside it;
/// this one stays valid until the operator deletes it, so a phone that paired
/// before per-device tokens existed never locks itself out.
pub fn token_path() -> PathBuf {
    root_dir().join("token")
}

/// Cached `tailscale cert` material. Owner-only: the private key lives here.
pub fn tls_dir() -> PathBuf {
    root_dir().join("tls")
}

pub fn sessions_dir() -> PathBuf {
    root_dir().join("sessions")
}

pub fn logs_dir() -> PathBuf {
    root_dir().join("logs")
}

/// Where launchd is told to send the daemon's stdout, and where the daemon
/// looks when it rotates its own log.
///
/// Both halves of that sentence are why these are functions here rather than
/// strings in two files: `codeconnect daemon install` writes the path into the plist and
/// `ccd` truncates the same path when it grows past the cap. If they ever
/// disagreed the log would grow without bound and nothing would say so.
pub fn daemon_stdout_log() -> PathBuf {
    logs_dir().join("ccd.out.log")
}

pub fn daemon_stderr_log() -> PathBuf {
    logs_dir().join("ccd.err.log")
}

pub fn config_path() -> PathBuf {
    root_dir().join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_honours_override() {
        // Serialised implicitly: this is the only test touching the var.
        std::env::set_var("CODECONNECT_HOME", "/tmp/cc-test-home");
        assert_eq!(root_dir(), PathBuf::from("/tmp/cc-test-home"));
        assert_eq!(socket_path(), PathBuf::from("/tmp/cc-test-home/ccd.sock"));
        std::env::remove_var("CODECONNECT_HOME");
    }
}
