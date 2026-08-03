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

pub mod config;
pub mod event;
pub mod fsperm;
pub mod hash;
pub mod hook;
pub mod ipc;
pub mod pairing;
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
pub const PROTOCOL_MINOR: u32 = 7;

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
