//! **Every sentence this daemon can send a phone when a Codex interrupt or
//! compose did not happen.**
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
//!   * `wire_code` — the app-server or the broker refused the write and the
//!     sentence carries their numeric code. The code is structural and is
//!     passed on; their *message* never is, because it has been measured
//!     naming a turn id the phone never sent.
//!   * `transient_local` — this Mac's own store or lookup failed, and it failed
//!     *before* anything was written: nothing reached Codex, no claim was
//!     taken, and the same ask may well succeed on a retry. Distinct from
//!     `link_state` because the thing that is unwell is this side's disk rather
//!     than the connection, and distinct from `permanent` because nothing about
//!     the ask was wrong.
//!
//! # Fragments
//!
//! Four of the values below are not sentences: the two `_SUBJECT`/`_REMEDY`
//! halves the wire refusal is assembled from, and the two `_CONNECTION_ENDED`
//! causes a settlement prefixes. They carry no row of their own — they appear
//! inside the sentence they help build, which is the row.

// ---- The daemon's own pre-flight, before anything durable exists -------------

pub(crate) fn unknown_session(session_ref: &str) -> String {
    format!("unknown session {session_ref}")
}

pub(crate) fn session_lookup_failed(err: &str) -> String {
    format!("session lookup failed: {err}")
}

pub(crate) fn interrupt_on_claude(uid: &str) -> String {
    format!(
        "{uid} is a Claude session, which this daemon cannot stop from a phone; \
        press Escape at the Mac"
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

pub(crate) fn interrupt_ledger_unreadable(err: &str) -> String {
    format!("could not read this run's interrupt ledger ({err}); nothing was sent")
}

pub(crate) const INTERRUPT_ALREADY_SENT_UNKNOWN: &str =
    "this interrupt was already sent and what became of it \
    is not known; it will not be sent again. Check the Mac.";

// ---- The link's state as the daemon reads it, before it writes ---------------

pub(crate) const INTERRUPT_LINK_BOUND: &str =
    "this Mac is connected to the Codex session but is not yet watching \
    its thread, so a stop cannot be confirmed; nothing was sent";

pub(crate) const INTERRUPT_LINK_RECONNECTING: &str =
    "this Mac has lost its control link to the Codex session and is \
    reconnecting, so nothing was sent; try again shortly, or stop the \
    turn at the Mac";

pub(crate) const INTERRUPT_LINK_NOT_REACHED: &str =
    "this Mac has not yet reached the Codex session, so nothing was \
    sent; try again shortly, or stop the turn at the Mac";

pub(crate) const LINK_STILL_PICKING_UP_THREAD: &str =
    "this Mac is connected to the Codex session and is still picking up \
    its thread, so nothing was sent; try again in a few seconds";

pub(crate) const INTERRUPT_NO_LINK: &str =
    "there is no live link to this Codex session, so nothing was sent; \
    stop the turn at the Mac";

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

pub(crate) const COMPOSE_LINK_BOUND: &str =
    "this Mac is connected to the Codex session but is not yet watching its \
    thread, so a message cannot be confirmed; nothing was sent";

pub(crate) const COMPOSE_LINK_RECONNECTING: &str =
    "this Mac has lost its control link to the Codex session and is \
    reconnecting, so nothing was sent; try again shortly, or say it at the \
    Mac";

pub(crate) const COMPOSE_LINK_NOT_REACHED: &str =
    "this Mac has not yet reached the Codex session, so nothing was sent; try \
    again shortly, or say it at the Mac";

pub(crate) const COMPOSE_NO_LINK: &str =
    "there is no live link to this Codex session, so nothing was sent; say it \
    at the Mac";

// ---- The channel to the link task, and the teardown that drains it -----------

pub(crate) const INTERRUPT_LINK_NOT_RUNNING: &str =
    "the link to this Codex session is not running, so nothing was sent; \
    stop the turn at the Mac";

pub(crate) const INTERRUPT_LINK_STOPPED_MID_WRITE: &str =
    "the link stopped while this interrupt was being written, so whether it \
    reached Codex is not known; it will not be sent again. Check the Mac.";

pub(crate) const INTERRUPT_NO_CONNECTION: &str =
    "the link to this Codex session has no live connection, so nothing was \
    sent; stop the turn at the Mac";

pub(crate) fn interrupt_settled_unknown(cause: &str) -> String {
    format!(
        "{cause}, so whether that turn was stopped is not known; it will not be \
        sent again. Check the Mac."
    )
}

pub(crate) fn interrupt_settled_unrecorded(cause: &str, err: &str) -> String {
    format!(
        "{cause}, and this Mac could not record that either ({err}), so it \
        cannot say what became of it. Check the Mac."
    )
}

pub(crate) const COMPOSE_LINK_WENT_AWAY: &str =
    "this Mac's link to the Codex session went away before that message was \
    written, so nothing was said; try again";

pub(crate) const COMPOSE_LINK_STOPPED_MID_WRITE: &str =
    "this Mac's link to the Codex session stopped while that message was in \
    flight, so whether it was said is not known; it will not be sent again. \
    Check the Mac.";

pub(crate) const COMPOSE_NO_CONNECTION: &str =
    "the link to this Codex session has no live connection, so nothing was \
    said; say it at the Mac";

pub(crate) fn compose_settled_unknown(cause: &str) -> String {
    format!(
        "{cause}, so whether that message reached the model is not known; it will \
        not be sent again. Check the Mac."
    )
}

pub(crate) fn compose_settled_unrecorded(cause: &str) -> String {
    format!(
        "{cause}, and this Mac could not record that either, so it cannot say \
        what became of it. Check the Mac."
    )
}

// ---- What the link refuses with the facts in hand, before the write ----------

/// **Not catalogued, for [`REPLAY_INTERRUPT_ABORTED`]'s reason in the other direction.**
///
/// [`crate::state::Daemon::interrupt`] refuses an empty `turn_id` with
/// [`INTERRUPT_NAMES_NO_TURN`] several steps before it builds the only
/// [`crate::store::ClaimedMaterial`] an interrupt is ever claimed with, so nothing that
/// clears admission can reach this guard with an empty target. The guard stays: it is the
/// link's own statement of what it requires, and a future caller that builds material
/// elsewhere should meet a sentence rather than a write. It carries no row because the
/// fixture describes what a phone can be told, not what the code can defend against.
pub(crate) const LINK_INTERRUPT_NAMES_NO_TURN: &str =
    "this request names no turn, so there is nothing to stop; nothing was sent";

pub(crate) const INTERRUPT_RECONNECTED_IN_FLIGHT: &str =
    "this Mac's link to the Codex session reconnected while that request was \
    in flight, so nothing was sent; try again";

pub(crate) const INTERRUPT_OTHER_THREAD: &str =
    "this Codex session is not on the thread that turn belongs to, so nothing was \
    sent; stop it at the Mac";

pub(crate) const INTERRUPT_VISIT_MOVED_ON: &str =
    "this Codex session has moved on since that turn was shown, so nothing was \
    sent; open the run again";

pub(crate) const INTERRUPT_TURN_NOT_RUNNING: &str =
    "that turn is not the one this Codex session is running, so nothing was \
    sent; it may have finished already";

pub(crate) const INTERRUPT_THREAD_SWITCHING: &str =
    "this Codex session is moving to another thread, so nothing was sent; try \
    again once it has settled";

pub(crate) const INTERRUPT_ALREADY_WAITING: &str =
    "an interrupt for that turn is already waiting to take effect, so nothing \
    was sent again";

pub(crate) const COMPOSE_RECONNECTED_IN_FLIGHT: &str =
    "this Mac's link to the Codex session reconnected while that message was in \
    flight, so nothing was said; try again";

pub(crate) const COMPOSE_OTHER_THREAD: &str =
    "this Codex session is not on the thread that message was addressed to, so \
    nothing was said; say it at the Mac";

pub(crate) const COMPOSE_VISIT_MOVED_ON: &str =
    "this Codex session has moved on since that message was composed, so nothing \
    was said; open the run again";

pub(crate) const COMPOSE_THREAD_SWITCHING: &str =
    "this Codex session is moving to another thread, so nothing was said; try \
    again once it has settled";

pub(crate) const COMPOSE_LAUNCH_UNREAD: &str =
    "this Mac has not yet read what this Codex thread runs under, so it \
    cannot start a turn on it; try again shortly, or say it at the Mac";

pub(crate) const INTERRUPT_ID_REUSED_ON_LINK: &str =
    "this request id has already been used for a different stop on this \
    run, so nothing was sent; ask again under a new one";

pub(crate) const INTERRUPT_RUN_IS_GONE: &str = "this run is gone, so its turn cannot be stopped";

pub(crate) fn interrupt_claim_unrecorded(err: &str) -> String {
    format!(
        "could not record that this interrupt is being sent ({err}); \
        nothing was sent"
    )
}

pub(crate) const COMPOSE_RUN_IS_GONE: &str = "this run is gone, so nothing can be said to it";

pub(crate) const COMPOSE_CLAIM_UNRECORDED: &str =
    "this Mac could not record that the message is being sent; nothing \
    was sent. Check the Mac.";

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
     sent again. Check the Mac.";

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

pub(crate) const INTERRUPT_WIRE_REMEDY: &str = "stop the turn at the Mac";

pub(crate) const COMPOSE_WIRE_SUBJECT: &str = "that message";

pub(crate) const COMPOSE_WIRE_REMEDY: &str = "say it at the Mac";

// ---- After the write, when the evidence is missing or contradicts ------------

pub(crate) fn interrupt_turn_ended_itself(status: &str) -> String {
    format!(
        "that turn ended on its own ({status}) while the request to stop \
        it was in flight, so it was not stopped from here; it will not \
        be sent again"
    )
}

pub(crate) fn interrupt_outcome_unrecorded(err: &str) -> String {
    format!(
        "this Mac could not record what became of that request to stop the turn \
        ({err}), so it cannot say; it will not be sent again. Check the Mac."
    )
}

pub(crate) const INTERRUPT_SETTLED_ELSEWHERE: &str =
    "this request to stop the turn was already settled elsewhere and what \
    became of it is not known; it will not be sent again. Check the Mac.";

pub(crate) const INTERRUPT_SETTLED_ELSEWHERE_UNREADABLE: &str =
    "this request to stop the turn was settled elsewhere and this Mac cannot \
    read what became of it; it will not be sent again. Check the Mac.";

pub(crate) const INTERRUPT_BOUNDARY_UNRECORDED: &str =
    "this Mac could not record that the turn ended (turn boundary \
    unrecorded), so it cannot say the turn was stopped from here; \
    it will not be sent again. Check the Mac.";

pub(crate) const INTERRUPT_BOUNDARY_UNREADABLE: &str =
    "this Mac could not read whether the turn boundary was recorded, so it \
    cannot say the turn was stopped from here; it will not be sent \
    again. Check the Mac.";

pub(crate) const COMPOSE_ANSWER_UNREADABLE: &str =
    "Codex answered this message with something this Mac could not read, \
    so it cannot say what became of it; it will not be sent again. Check \
    the Mac.";

pub(crate) const COMPOSE_SETTLED_ELSEWHERE: &str =
    "this message was sent and what became of it is not known; it will \
    not be sent again. Check the Mac.";

pub(crate) const COMPOSE_SETTLED_ELSEWHERE_UNREADABLE: &str =
    "this message was sent and this Mac could not read what became of it; \
    it will not be sent again. Check the Mac.";

pub(crate) const COMPOSE_OUTCOME_UNRECORDED: &str =
    "this message was sent and this Mac could not record what became of it; \
    it will not be sent again. Check the Mac.";

// ---- Replaying a ledger row that is already terminal -------------------------

/// **Not catalogued, because a phone can never read it.**
///
/// [`crate::codex_link::replayed_interrupt_report`] answers `INTERRUPT_ABORTED` with a
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
    "this message is already settled and will not be sent again; check the Mac";

pub(crate) const INTERRUPT_CONNECTION_ENDED: &str =
    "the link's connection to Codex ended while this request to stop the turn \
    was in flight";

pub(crate) const COMPOSE_CONNECTION_ENDED: &str =
    "the link's connection to Codex ended while this message was in flight";

/// **What a phone is told when the app-server or the broker refused the write,
/// and it is the CODE.**
///
/// Split out of [`crate::codex_link`]'s own frame reader so the assembled
/// sentence can be built from a token in the table without a frame to read it
/// from. The rule it enforces is that reader's, unchanged: the refusing party's
/// `message` is logged on the machine entitled to it and never interpolated
/// into what a phone reads.
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
/// arguments — `"{uid}"`, `"{n}"`, `"{err}"` — so a row's text is produced by
/// the very formatter the daemon uses. The template in the fixture therefore
/// cannot drift from the code: there is no second copy of it to drift.
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
            session_lookup_failed("{err}"),
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
            interrupt_ledger_unreadable("{err}"),
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
            "interrupt_link_bound",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_LINK_BOUND.to_string(),
        ),
        row(
            "interrupt_link_reconnecting",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_LINK_RECONNECTING.to_string(),
        ),
        row(
            "interrupt_link_not_reached",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_LINK_NOT_REACHED.to_string(),
        ),
        row(
            "link_still_picking_up_thread",
            "both",
            "rejected",
            "link_state",
            LINK_STILL_PICKING_UP_THREAD.to_string(),
        ),
        row(
            "interrupt_no_link",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_NO_LINK.to_string(),
        ),
        row(
            "compose_link_bound",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_LINK_BOUND.to_string(),
        ),
        row(
            "compose_link_reconnecting",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_LINK_RECONNECTING.to_string(),
        ),
        row(
            "compose_link_not_reached",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_LINK_NOT_REACHED.to_string(),
        ),
        row(
            "compose_no_link",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_NO_LINK.to_string(),
        ),
        row(
            "interrupt_link_not_running",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_LINK_NOT_RUNNING.to_string(),
        ),
        row(
            "interrupt_link_stopped_mid_write",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_LINK_STOPPED_MID_WRITE.to_string(),
        ),
        row(
            "interrupt_no_connection",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_NO_CONNECTION.to_string(),
        ),
        row(
            "interrupt_settled_unknown",
            "interrupt",
            "indeterminate",
            "permanent",
            interrupt_settled_unknown(INTERRUPT_CONNECTION_ENDED),
        ),
        row(
            "interrupt_settled_unrecorded",
            "interrupt",
            "indeterminate",
            "permanent",
            interrupt_settled_unrecorded(INTERRUPT_CONNECTION_ENDED, "{err}"),
        ),
        row(
            "compose_link_went_away",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_LINK_WENT_AWAY.to_string(),
        ),
        row(
            "compose_link_stopped_mid_write",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_LINK_STOPPED_MID_WRITE.to_string(),
        ),
        row(
            "compose_no_connection",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_NO_CONNECTION.to_string(),
        ),
        row(
            "compose_settled_unknown",
            "compose",
            "indeterminate",
            "permanent",
            compose_settled_unknown(COMPOSE_CONNECTION_ENDED),
        ),
        row(
            "compose_settled_unrecorded",
            "compose",
            "indeterminate",
            "permanent",
            compose_settled_unrecorded(COMPOSE_CONNECTION_ENDED),
        ),
        row(
            "interrupt_reconnected_in_flight",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_RECONNECTED_IN_FLIGHT.to_string(),
        ),
        row(
            "interrupt_other_thread",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_OTHER_THREAD.to_string(),
        ),
        row(
            "interrupt_visit_moved_on",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_VISIT_MOVED_ON.to_string(),
        ),
        row(
            "interrupt_turn_not_running",
            "interrupt",
            "rejected",
            "permanent",
            INTERRUPT_TURN_NOT_RUNNING.to_string(),
        ),
        row(
            "interrupt_thread_switching",
            "interrupt",
            "rejected",
            "link_state",
            INTERRUPT_THREAD_SWITCHING.to_string(),
        ),
        row(
            "interrupt_already_waiting",
            "interrupt",
            "rejected",
            "transient_local",
            INTERRUPT_ALREADY_WAITING.to_string(),
        ),
        row(
            "compose_reconnected_in_flight",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_RECONNECTED_IN_FLIGHT.to_string(),
        ),
        row(
            "compose_other_thread",
            "compose",
            "rejected",
            "permanent",
            COMPOSE_OTHER_THREAD.to_string(),
        ),
        row(
            "compose_visit_moved_on",
            "compose",
            "rejected",
            "permanent",
            COMPOSE_VISIT_MOVED_ON.to_string(),
        ),
        row(
            "compose_thread_switching",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_THREAD_SWITCHING.to_string(),
        ),
        row(
            "compose_launch_unread",
            "compose",
            "rejected",
            "link_state",
            COMPOSE_LAUNCH_UNREAD.to_string(),
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
            interrupt_claim_unrecorded("{err}"),
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
            interrupt_outcome_unrecorded("{err}"),
        ),
        row(
            "interrupt_settled_elsewhere",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_SETTLED_ELSEWHERE.to_string(),
        ),
        row(
            "interrupt_settled_elsewhere_unreadable",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_SETTLED_ELSEWHERE_UNREADABLE.to_string(),
        ),
        row(
            "interrupt_boundary_unrecorded",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_BOUNDARY_UNRECORDED.to_string(),
        ),
        row(
            "interrupt_boundary_unreadable",
            "interrupt",
            "indeterminate",
            "permanent",
            INTERRUPT_BOUNDARY_UNREADABLE.to_string(),
        ),
        row(
            "compose_answer_unreadable",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_ANSWER_UNREADABLE.to_string(),
        ),
        row(
            "compose_settled_elsewhere",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_SETTLED_ELSEWHERE.to_string(),
        ),
        row(
            "compose_settled_elsewhere_unreadable",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_SETTLED_ELSEWHERE_UNREADABLE.to_string(),
        ),
        row(
            "compose_outcome_unrecorded",
            "compose",
            "indeterminate",
            "permanent",
            COMPOSE_OUTCOME_UNRECORDED.to_string(),
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
            "{ref}": "the session reference the caller used (a uid or a tmux name)",
            "{agent}": "the name of an agent kind this build does not support",
            "{n}": "a number — the size of the thing asked about, or a wire error code",
            "{max}": "a ceiling this build enforces (today only MAX_COMPOSE_BYTES)",
            "{err}": "a local store error, rendered by this Mac and never by Codex",
            "{status}": "a turn terminal's own status word from the wire",
            "{word}": "an outcome word read back from the ledger that this build \
                       has no vocabulary for"
        },
        "categories": {
            "link_state": "a fact about the control link right now; the same ask a \
                           moment later may well be written",
            "permanent": "a settled fact — the ask was wrong, the id is spent, or \
                          the outcome is already recorded",
            "wire_code": "the app-server or the broker refused the write; the \
                          sentence carries their numeric code and never their message",
            "transient_local": "this Mac's own store or lookup failed BEFORE anything \
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
