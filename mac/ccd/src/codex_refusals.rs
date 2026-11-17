//! **Every sentence this daemon can send a phone when a Stop (a Codex run's, or
//! a Claude run's CodeConnect started) or a compose did not happen.**
//!
//! One table, and production formats FROM it. The phone greys its Stop and
//! Compose controls for a few seconds when it recognises a refusal as a fact
//! about the *link* rather than about the ask — and it recognises them by
//! matching substrings of these sentences, because the wire carries a bare
//! `reason` string and no code. That match is correct today and would go
//! silently wrong the moment somebody reworded a literal, because nothing on
//! this side of the wire would fail: the daemon would keep saying something
//! true and the phone would quietly stop believing it.
//!
//! So the sentences live here, each said in exactly one place, and
//! `fixtures/codex/refusal-sentences.json` is emitted from this module by a
//! gate test in [`crate::state`]. The phone asserts its classifier against the
//! committed file; this build asserts the committed file against itself. A
//! reword now changes the fixture in the diff, where a reviewer sees it, and
//! the phone's own test tells its author which sentence moved.
//!
//! # What the categories mean
//!
//! They are the daemon's own claim about the refusal, not the phone's policy
//! about it — the phone decides what to do, and needs the fact to decide from.
//!
//!   * `link_state` — a statement about the control link *right now*. The same
//!     ask a moment later may well be written. These are the ones worth
//!     waiting out.
//!   * `permanent` — a settled fact: the ask was wrong, the run is not a Codex
//!     run, the request id is spent, or the outcome is already recorded.
//!     Retrying changes nothing, and for the `indeterminate` ones retrying is
//!     the one thing that must not happen.
//!   * `wire_code` — the app-server refused the write and the sentence carries
//!     its numeric code. The code is structural and is passed on; its *message*
//!     never is, because it has been measured naming a turn id the phone never
//!     sent.
//!   * `transient_local` — this Mac's own store or lookup failed, and it failed
//!     *before* anything was written: nothing reached Codex, no claim was
//!     taken, and the same ask may well succeed on a retry. Distinct from
//!     `link_state` because the thing that is unwell is this side's disk rather
//!     than the connection, and distinct from `permanent` because nothing about
//!     the ask was wrong.
//!
//! # Fragments
//!
//! Six of the values below are not sentences: the `_SUBJECT` and `_REMEDY`
//! parts the wire refusals are assembled from, and the two `_CONNECTION_ENDED`
//! causes a settlement prefixes. They carry no row of their own — they appear
//! inside the sentence they help build, which is the row.

// ---- The daemon's own pre-flight, before anything durable exists -------------

pub(crate) fn unknown_session(session_ref: &str) -> String {
    format!("unknown session {session_ref}")
}

/// **The lookup failed, and the failure's own words stay on this Mac.**
///
/// A `Display` on this daemon's store is an anyhow chain whose open paths name
/// absolute filesystem locations, and this sentence used to interpolate one — so a
/// phone learned where this Mac keeps its database from a bad disk day. A const
/// rather than a formatter: with nothing to interpolate there is nothing to leak by
/// accident, and the rule is enforced by the shape of the item rather than by every
/// caller remembering it. Each caller logs
/// the error itself with `{err:#}` before saying this, which is where an operator is
/// entitled to read it.
pub(crate) const SESSION_LOOKUP_FAILED: &str = "session lookup failed";

pub(crate) fn interrupt_on_claude(uid: &str) -> String {
    format!(
        "{uid} is a Claude session, which this daemon cannot stop from a phone; \
        press Escape at the computer"
    )
}

pub(crate) fn interrupt_on_unsupported(uid: &str, name: &str) -> String {
    format!(
        "{uid} is a {name} session, which this daemon does not know how to stop; \
        nothing was sent"
    )
}

pub(crate) const INTERRUPT_STALE_HASH: &str =
    "stale payload_hash: the turn you asked to stop is not the one this \
    request names; nothing was sent";

pub(crate) const INTERRUPT_NAMES_NO_TURN: &str =
    "this request names no turn, so there is nothing to stop";

pub(crate) const INTERRUPT_ID_REUSED: &str =
    "this request id was used to stop a different turn, so nothing \
    was sent; ask again under a new one";

/// The interrupt twin of [`SESSION_LOOKUP_FAILED`], and a const for its reason.
pub(crate) const INTERRUPT_LEDGER_UNREADABLE: &str =
    "could not read this run's interrupt ledger; nothing was sent";

pub(crate) const INTERRUPT_ALREADY_SENT_UNKNOWN: &str =
    "this interrupt was already sent and what became of it \
    is not known; it will not be sent again. Check the computer.";

// ---- The link's state as the daemon reads it, before it writes ---------------

/// A run started on the engine whose link is not attached, before its reconnect or after one
/// that failed: an answer, a stop or a message is refused with the fact alone. Such a run has
/// no screen at the computer to send the person to.
pub(crate) const ENGINE_NO_CONNECTION: &str =
    "CodeConnect has no connection to this run right now, so nothing was sent.";

pub(crate) fn compose_on_claude(uid: &str) -> String {
    format!(
        "{uid} is a Claude session, and this message is the Codex compose; \
        send text to a Claude session with send_text instead"
    )
}

pub(crate) fn compose_on_unsupported(uid: &str, name: &str) -> String {
    format!(
        "{uid} is a {name} session, which this daemon does not know how to speak \
        to; nothing was sent"
    )
}

pub(crate) const COMPOSE_EMPTY: &str = "this message is empty, so there is nothing to say";

pub(crate) fn compose_too_long(len: &str, ceiling: &str) -> String {
    format!("this message is {len} bytes; the ceiling is {ceiling}")
}

pub(crate) const COMPOSE_STALE_HASH: &str =
    "stale payload_hash: the message you asked to send is not the one this \
    request names; nothing was sent";

// ---- The channel to the link task, and the teardown that drains it -----------

pub(crate) fn interrupt_settled_unknown(cause: &str) -> String {
    format!(
        "{cause}, so whether that turn was stopped is not known; it will not be \
        sent again. Check the computer."
    )
}

pub(crate) fn compose_settled_unknown(cause: &str) -> String {
    format!(
        "{cause}, so whether that message reached the model is not known; it will \
        not be sent again. Check the computer."
    )
}

// ---- What the link refuses with the facts in hand, before the write ----------

pub(crate) const INTERRUPT_TURN_NOT_RUNNING: &str =
    "that turn is not the one this Codex session is running, so nothing was \
    sent; it may have finished already";

/// [`INTERRUPT_TURN_NOT_RUNNING`]'s twin for a Claude run CodeConnect started: its agent host
/// is not running that turn (another one, or the turn has ended), so nothing reached claude.
pub(crate) const CLAUDE_TURN_NOT_RUNNING: &str =
    "that turn is not the one Claude is running, so nothing was sent; it may have \
    finished already";

pub(crate) const INTERRUPT_ALREADY_WAITING: &str =
    "an interrupt for that turn is already waiting to take effect, so nothing \
    was sent again";

pub(crate) const INTERRUPT_ID_REUSED_ON_LINK: &str =
    "this request id has already been used for a different stop on this \
    run, so nothing was sent; ask again under a new one";

pub(crate) const INTERRUPT_RUN_IS_GONE: &str = "this run is gone, so its turn cannot be stopped";

/// [`COMPOSE_CLAIM_UNRECORDED`]'s twin, and a const for the same reason: the claim
/// failed on this Mac's own disk, and the words the disk used are this Mac's to read.
pub(crate) const INTERRUPT_CLAIM_UNRECORDED: &str =
    "could not record that this interrupt is being sent; nothing was sent";

pub(crate) const COMPOSE_RUN_IS_GONE: &str = "this run is gone, so nothing can be said to it";

pub(crate) const COMPOSE_CLAIM_UNRECORDED: &str =
    "the computer could not record that the message is being sent; nothing \
    was sent. Check the computer.";

/// **"you already said this and I cannot tell you what happened."**
///
/// A const rather than three literals: it is said by the link when its claim comes back
/// `Indeterminate`, by the link when the record already reads that way, and — since a
/// terminal row must outrank the link's current state — by
/// [`crate::state::Daemon::compose`] when there is no addressee to ask. Three copies of a
/// sentence are three chances for two of them to drift, and the drift would be invisible:
/// each copy is correct on its own.
pub(crate) const COMPOSE_ALREADY_SENT_UNKNOWN: &str =
    "this message was already sent and what became of it is not known; it will not be \
     sent again. Check the computer.";

/// **"that id already says something else."**
///
/// Named for [`COMPOSE_ALREADY_SENT_UNKNOWN`]'s reason, and one of its own: a conflict is
/// the one refusal that tells somebody they did something specific, so the two places that
/// can reach it — the link's claim, and the daemon's terminal read when there is no link to
/// claim against — must say it identically or the same mistake reads as two different ones.
pub(crate) const COMPOSE_ID_REUSED: &str =
    "this request id was used to say something else, so nothing was sent; ask again under \
     a new one";

// ---- The wire's own refusal, which is passed on as a CODE and nothing else ---

pub(crate) const INTERRUPT_WIRE_SUBJECT: &str = "that stop";

pub(crate) const COMPOSE_WIRE_SUBJECT: &str = "that message";

/// The remedies. A wire refusal reaches the phone only when it proves nothing reached
/// Codex, and its claim is given back, so the same ask from the phone is a first attempt
/// again.
pub(crate) const INTERRUPT_WIRE_REMEDY: &str = "the turn can be stopped again from the phone";

pub(crate) const COMPOSE_WIRE_REMEDY: &str = "it can be sent again from the phone";

// ---- After the write, when the evidence is missing or contradicts ------------

pub(crate) fn interrupt_turn_ended_itself(status: &str) -> String {
    format!(
        "that turn ended on its own ({status}) while the request to stop \
        it was in flight, so it was not stopped from here; it will not \
        be sent again"
    )
}

/// A const rather than a formatter: the settlement that failed logged the store's own
/// `Display` where it failed; what crosses the wire is the fact and the remedy.
pub(crate) const INTERRUPT_OUTCOME_UNRECORDED: &str =
    "the computer could not record what became of that request to stop the turn, so it \
    cannot say; it will not be sent again. Check the computer.";

pub(crate) const INTERRUPT_SETTLED_ELSEWHERE: &str =
    "this request to stop the turn was already settled elsewhere and what \
    became of it is not known; it will not be sent again. Check the computer.";

pub(crate) const COMPOSE_ANSWER_UNREADABLE: &str =
    "Codex answered this message with something the computer could not read, \
    so it cannot say what became of it; it will not be sent again. Check \
    the computer.";

// ---- Replaying a ledger row that is already terminal -------------------------

/// **Not catalogued, because a phone can never read it.**
///
/// [`crate::codex_host::replayed_interrupt_report`] answers `INTERRUPT_ABORTED` with a
/// structured `Duplicate` naming the turn and returns before the sentence path — and it is
/// the only production caller of [`crate::store::replayed_interrupt_sentence`]. So this arm
/// exists to keep the match total and to say the right thing if a second caller ever
/// appears; it has no row in [`catalogue`] because it cannot become a `Rejected { reason }`
/// today, and a fixture promising the phone a sentence it will never see would describe a
/// surface that does not exist.
pub(crate) const REPLAY_INTERRUPT_ABORTED: &str =
    "that turn was already stopped from a phone; nothing was sent \
    again";

pub(crate) const REPLAY_INTERRUPT_TURN_ENDED: &str =
    "that turn had already ended on its own when this was sent \
    before, so nothing was stopped; nothing was sent again";

pub(crate) const REPLAY_INTERRUPT_REFUSED: &str =
    "an earlier request to stop that turn was refused, so nothing \
    was sent again";

pub(crate) fn replay_interrupt_unknown_word(other: &str) -> String {
    format!("this interrupt is already settled ({other}); nothing was sent again")
}

pub(crate) const REPLAY_COMPOSE_REFUSED: &str =
    "an earlier attempt to say this was refused, so nothing was \
    sent again";

pub(crate) const REPLAY_COMPOSE_SETTLED: &str =
    "this message is already settled and will not be sent again; check the computer";

/// A replay of a message that joined a running turn and was dropped when that turn was
/// stopped ([`crate::store::COMPOSE_STOPPED`]).
pub(crate) const REPLAY_COMPOSE_STOPPED: &str =
    "this message was not sent: the turn it joined was stopped before Codex read it, and it \
    will not be sent again under this request";

/// [`INTERRUPT_CONNECTION_ENDED`]'s twin for a Claude run CodeConnect started: its agent host's
/// link ended after the stop was sent and before its outcome came.
pub(crate) const CLAUDE_CONNECTION_ENDED: &str =
    "the computer's connection to Claude ended while this request to stop the turn was in \
    flight";

pub(crate) const INTERRUPT_CONNECTION_ENDED: &str =
    "the link's connection to Codex ended while this request to stop the turn \
    was in flight";

pub(crate) const COMPOSE_CONNECTION_ENDED: &str =
    "the link's connection to Codex ended while this message was in flight";

/// **What a phone is told when the app-server refused the write, and it is the
/// CODE.**
///
/// The refusing party's `message` is logged on the machine entitled to it and never
/// interpolated into what a phone reads.
pub(crate) fn wire_refused(code: &str, what: &str, remedy: &str) -> String {
    format!("{what} was refused (code {code}) and nothing was changed; {remedy}")
}

/// One sentence, and what the phone may conclude from it.
#[cfg(test)]
pub(crate) struct Refusal {
    pub(crate) id: &'static str,
    pub(crate) verb: &'static str,
    pub(crate) outcome: &'static str,
    pub(crate) category: &'static str,
    pub(crate) text: String,
}

/// **Every sentence, assembled the way production assembles it.**
///
/// The interpolated ones are called with the fixture's own tokens as their
/// arguments — `"{uid}"`, `"{n}"` — so a row's text is produced by the very
/// formatter the daemon uses. The template in the fixture therefore cannot drift
/// from the code: there is no second copy of it to drift.
///
/// **No row is built from a local error.** There is no `{err}` token, because a
/// `Display` on this daemon's store is an anyhow chain whose open paths name
/// absolute filesystem locations and a phone is entitled to none of them.
#[cfg(test)]
pub(crate) fn catalogue() -> Vec<Refusal> {
    fn row(
        id: &'static str,
        verb: &'static str,
        outcome: &'static str,
        category: &'static str,
        text: String,
    ) -> Refusal {
        Refusal {
            id,
            verb,
            outcome,
            category,
            text,
        }
    }
    vec![
        row(
            "unknown_session",
            "both",
            "rejected",
            "permanent",
            unknown_session("{ref}"),
        ),
        row(
            "session_lookup_failed",
            "both",
            "rejected",
            "transient_local",
            SESSION_LOOKUP_FAILED.to_string(),
        ),
        row(
            "interrupt_on_claude",
            "interrupt",
            "rejected",
            "permanent",
            interrupt_on_claude("{uid}"),
        ),
        row(
            "interrupt_on_unsupported",
            "interrupt",
            "rejected",
            "permanent",
            interrupt_on_unsupported("{uid}", "{agent}"),
        ),
        row(
            "interrupt_stale_hash",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_STALE_HASH.to_string(),
        ),
        row(
            "interrupt_names_no_turn",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_NAMES_NO_TURN.to_string(),
        ),
        row(
            "interrupt_id_reused",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_ID_REUSED.to_string(),
        ),
        row(
            "interrupt_ledger_unreadable",
            "interrupt",
            "rejected",
            "transient_local",
            INTERRUPT_LEDGER_UNREADABLE.to_string(),
        ),
        row(
            "interrupt_already_sent_unknown",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_ALREADY_SENT_UNKNOWN.to_string(),
        ),
        row(
            "compose_on_claude",
            "compose",
            "rejected",
            "permanent",
            compose_on_claude("{uid}"),
        ),
        row(
            "compose_on_unsupported",
            "compose",
            "rejected",
            "permanent",
            compose_on_unsupported("{uid}", "{agent}"),
        ),
        row(
            "compose_empty",
            "compose",
            "rejected",
            "permanent",
            COMPOSE_EMPTY.to_string(),
        ),
        row(
            "compose_too_long",
            "compose",
            "rejected",
            "permanent",
            compose_too_long("{n}", "{max}"),
        ),
        row(
            "compose_stale_hash",
            "compose",
            "rejected",
            "permanent",
            COMPOSE_STALE_HASH.to_string(),
        ),
        row(
            "engine_no_connection",
            "both",
            "rejected",
            "link_state",
            ENGINE_NO_CONNECTION.to_string(),
        ),
        row(
            "interrupt_settled_unknown",
            "interrupt",
            "indeterminate",
            "permanent",
            interrupt_settled_unknown(INTERRUPT_CONNECTION_ENDED),
        ),
        row(
            "claude_stop_settled_unknown",
            "interrupt",
            "indeterminate",
            "permanent",
            interrupt_settled_unknown(CLAUDE_CONNECTION_ENDED),
        ),
        row(
            "compose_settled_unknown",
            "compose",
            "indeterminate",
            "permanent",
            compose_settled_unknown(COMPOSE_CONNECTION_ENDED),
        ),
        row(
            "interrupt_turn_not_running",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_TURN_NOT_RUNNING.to_string(),
        ),
        row(
            "claude_turn_not_running",
            "interrupt",
            "rejected",
            "permanent",
            CLAUDE_TURN_NOT_RUNNING.to_string(),
        ),
        row(
            "interrupt_already_waiting",
            "interrupt",
            "rejected",
            "transient_local",
            INTERRUPT_ALREADY_WAITING.to_string(),
        ),
        row(
            "interrupt_id_reused_on_link",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_ID_REUSED_ON_LINK.to_string(),
        ),
        row(
            "interrupt_run_is_gone",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_RUN_IS_GONE.to_string(),
        ),
        row(
            "interrupt_claim_unrecorded",
            "interrupt",
            "rejected",
            "transient_local",
            INTERRUPT_CLAIM_UNRECORDED.to_string(),
        ),
        row(
            "compose_id_reused",
            "compose",
            "rejected",
            "permanent",
            COMPOSE_ID_REUSED.to_string(),
        ),
        row(
            "compose_already_sent_unknown",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_ALREADY_SENT_UNKNOWN.to_string(),
        ),
        row(
            "compose_run_is_gone",
            "compose",
            "rejected",
            "permanent",
            COMPOSE_RUN_IS_GONE.to_string(),
        ),
        row(
            "compose_claim_unrecorded",
            "compose",
            "rejected",
            "transient_local",
            COMPOSE_CLAIM_UNRECORDED.to_string(),
        ),
        row(
            "interrupt_wire_refused",
            "interrupt",
            "rejected",
            "wire_code",
            wire_refused("{n}", INTERRUPT_WIRE_SUBJECT, INTERRUPT_WIRE_REMEDY),
        ),
        row(
            "compose_wire_refused",
            "compose",
            "rejected",
            "wire_code",
            wire_refused("{n}", COMPOSE_WIRE_SUBJECT, COMPOSE_WIRE_REMEDY),
        ),
        row(
            "interrupt_turn_ended_itself",
            "interrupt",
            "rejected",
            "permanent",
            interrupt_turn_ended_itself("{status}"),
        ),
        row(
            "interrupt_outcome_unrecorded",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_OUTCOME_UNRECORDED.to_string(),
        ),
        row(
            "interrupt_settled_elsewhere",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_SETTLED_ELSEWHERE.to_string(),
        ),
        row(
            "compose_answer_unreadable",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_ANSWER_UNREADABLE.to_string(),
        ),
        row(
            "replay_interrupt_turn_ended",
            "interrupt",
            "rejected",
            "permanent",
            REPLAY_INTERRUPT_TURN_ENDED.to_string(),
        ),
        row(
            "replay_interrupt_refused",
            "interrupt",
            "rejected",
            "permanent",
            REPLAY_INTERRUPT_REFUSED.to_string(),
        ),
        row(
            "replay_interrupt_unknown_word",
            "interrupt",
            "rejected",
            "permanent",
            replay_interrupt_unknown_word("{word}"),
        ),
        row(
            "replay_compose_refused",
            "compose",
            "rejected",
            "permanent",
            REPLAY_COMPOSE_REFUSED.to_string(),
        ),
        row(
            "replay_compose_settled",
            "compose",
            "rejected",
            "permanent",
            REPLAY_COMPOSE_SETTLED.to_string(),
        ),
        row(
            "replay_compose_stopped",
            "compose",
            "rejected",
            "permanent",
            REPLAY_COMPOSE_STOPPED.to_string(),
        ),
    ]
}

/// **Where the regenerator writes**, and the bytes the gate compares against.
///
/// The checked-in file by default; `CC_REFUSAL_FIXTURE_OUT` overrides it, which is
/// what lets the regenerator be run against a scratch path without touching the
/// tree. Resolved from `CARGO_MANIFEST_DIR` so it does not depend on the
/// directory the test was invoked from.
#[cfg(test)]
pub(crate) fn fixture_path() -> std::path::PathBuf {
    match std::env::var_os("CC_REFUSAL_FIXTURE_OUT") {
        Some(path) => std::path::PathBuf::from(path),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/codex/refusal-sentences.json"),
    }
}

/// **The file's exact bytes**, so the writer and the gate cannot disagree on
/// them — pretty JSON and the trailing newline a text file ends with.
#[cfg(test)]
pub(crate) fn fixture_bytes() -> String {
    format!("{}\n", serde_json::to_string_pretty(&fixture()).unwrap())
}

/// **Write the fixture where it belongs.**
///
/// A direct write rather than `println!` and a shell redirect: libtest prints its
/// own lines on the same stdout, so `--nocapture > file` captured the harness's
/// "running 1 test" beside the JSON and produced a file that did not parse. The
/// generator writing the file itself has no stream to share.
#[cfg(test)]
pub(crate) fn write_fixture() -> std::path::PathBuf {
    let path = fixture_path();
    std::fs::write(&path, fixture_bytes()).expect("the fixture path must be writable");
    path
}

/// `fixtures/codex/refusal-sentences.json`, as this build says it.
#[cfg(test)]
pub(crate) fn fixture() -> serde_json::Value {
    let rows = catalogue();
    let count = |c: &str| rows.iter().filter(|r| r.category == c).count();
    serde_json::json!({
        "what": "Every sentence ccd can send a phone when a Codex interrupt or \
                 compose did not happen. Emitted from mac/ccd/src/codex_refusals.rs, \
                 which is the single place each sentence is written; ccd's own gate \
                 test requires this file to be byte-identical to what the build emits.",
        "tokens": {
            "{uid}": "a session uid",
            "{ref}": "the session reference the caller used (a uid or a name)",
            "{agent}": "the name of an agent kind this build does not support",
            "{n}": "a number — the size of the thing asked about, or a wire error code",
            "{max}": "a ceiling this build enforces (today only MAX_COMPOSE_BYTES)",
            "{status}": "a turn terminal's own status word from the wire",
            "{word}": "an outcome word read back from the ledger that this build \
                       has no vocabulary for"
        },
        "categories": {
            "link_state": "a fact about the control link right now; the same ask a \
                           moment later may well be written",
            "permanent": "a settled fact — the ask was wrong, the id is spent, or \
                          the outcome is already recorded",
            "wire_code": "the app-server refused the write; the sentence carries \
                          its numeric code and never its message",
            "transient_local": "the computer's own store or lookup failed BEFORE anything \
                                was written; nothing reached Codex and the same ask may \
                                well succeed on a retry"
        },
        "counts": {
            "total": rows.len(),
            "link_state": count("link_state"),
            "permanent": count("permanent"),
            "transient_local": count("transient_local"),
            "wire_code": count("wire_code"),
        },
        "sentences": rows
            .iter()
            .map(|r| serde_json::json!({
                "id": r.id,
                "verb": r.verb,
                "outcome": r.outcome,
                "category": r.category,
                "text": r.text,
            }))
            .collect::<Vec<_>>(),
    })
}
