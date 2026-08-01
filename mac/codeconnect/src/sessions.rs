//! `codeconnect sessions` and `codeconnect sessions prune`.
//!
//! `codeconnect ls` answers "what can I attach to?" and asks tmux, so it keeps working
//! with the daemon down. This answers a different question — "what does the
//! event log think happened?" — and so it asks the daemon. Both exist because a
//! disagreement between them is exactly the symptom worth seeing: a run listed
//! here as `live` that tmux does not have is a session the daemon has not yet
//! reconciled, or one it cannot.
//!
//! ## Why pruning is a command and not a policy
//!
//! A machine accumulates ended runs the way the owner's did: 45 rows, most of
//! them soak runs and long-dead experiments, and no supported way to clear them
//! short of deleting `events.db` — which throws away the history of the sessions
//! that *are* running along with it.
//!
//! The event log is the source of truth, so removing from it is destructive in
//! the way that matters: there is no second copy. That makes it something an
//! operator asks for, never something a daemon decides on a timer or a size
//! threshold. It is defined only over runs that have **ended**; a run that is
//! still going, or one nothing could be established about, is left where it is
//! and reported rather than swept up. `--dry-run` prints exactly what would go,
//! including how many facts each run would take with it, because "18 sessions"
//! is not something anybody can check after the fact.

use std::time::Duration;

use anyhow::{bail, Result};
use protocol::event::{Lifecycle, SessionSummary};
use protocol::ipc::{ClientFrame, DaemonFrame, PrunedSummary};

use crate::daemon;

/// A prune deletes across seven tables in one transaction. On a machine with a
/// long history that is real work, and abandoning the reply while the daemon
/// commits would leave the operator unable to tell whether it happened.
const PRUNE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn command(args: &[String]) -> Result<()> {
    let (sub, rest) = args
        .split_first()
        .map(|(head, tail)| (head.as_str(), tail))
        .unwrap_or(("list", &[]));
    match sub {
        "list" | "ls" => list(),
        "prune" => prune(rest),
        other => {
            bail!("unknown subcommand {other:?}; usage: codeconnect sessions [prune [--dry-run]]")
        }
    }
}

fn list() -> Result<()> {
    let sessions = fetch()?;
    if sessions.is_empty() {
        println!("the daemon has no sessions on record");
        return Ok(());
    }
    println!(
        "{:<12} {:<28} {:<9} {:<9} {:>7}  CWD",
        "SESSION", "UID", "STATE", "LINK", "EVENTS"
    );
    for session in &sessions {
        println!(
            "{:<12} {:<28} {:<9} {:<9} {:>7}  {}",
            session.session_id,
            session.session_uid,
            lifecycle_word(session.lifecycle),
            format!("{:?}", session.link).to_lowercase(),
            session.last_seq,
            session.cwd
        );
    }
    println!();
    println!("{}", tally(&sessions));
    Ok(())
}

fn prune(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            other => {
                bail!("unknown option {other:?}; usage: codeconnect sessions prune [--dry-run]")
            }
        }
    }

    let reply = daemon::request_within(&ClientFrame::PruneSessions { dry_run }, PRUNE_TIMEOUT)?;
    let DaemonFrame::Pruned {
        removed,
        kept_live,
        kept_unknown,
        kept_held,
        orphan_events,
        dry_run,
    } = reply
    else {
        bail!("unexpected reply from ccd: {reply:?}");
    };

    report(
        &removed,
        kept_live,
        kept_unknown,
        kept_held,
        orphan_events,
        dry_run,
    );
    Ok(())
}

/// Print what went, and what did not, and why.
///
/// Split out so the wording is exercised by a test rather than only by running
/// it: the difference between "would remove" and "removed" is the difference
/// between a rehearsal and a deletion, and it must not be possible for the
/// dry-run path to print the past tense.
fn report(
    removed: &[PrunedSummary],
    kept_live: usize,
    kept_unknown: usize,
    kept_held: usize,
    orphan_events: u64,
    dry_run: bool,
) {
    let events: u64 = removed.iter().map(|row| row.events).sum();
    if removed.is_empty() {
        // "None had ended" and "some had ended and were held back" are not the
        // same answer, and printing the first for the second sent an operator
        // to `codeconnect sessions`, where the ended runs were plainly listed, with no
        // way to find out why they had survived.
        if kept_held > 0 {
            println!("no ended sessions could be removed right now");
        } else {
            println!("no ended sessions to remove");
        }
    } else {
        println!(
            "{} {} ended session(s) and {events} event(s):",
            if dry_run { "would remove" } else { "removed" },
            removed.len()
        );
        for row in removed {
            println!(
                "  {:<12} {:<28} {:>7} events  ended {}  {}",
                row.session_id, row.session_uid, row.events, row.updated_at, row.cwd
            );
        }
    }
    // Always said, including when nothing was removed: "nothing to prune" and
    // "nothing here has ended yet" are different situations, and the second is
    // the one where a fleet full of `live` rows might mean the daemon cannot
    // see tmux rather than that the agents are working.
    println!();
    println!("kept {kept_live} still running, {kept_unknown} whose state could not be established");
    if kept_held > 0 {
        println!(
            "  {kept_held} ended run(s) were held back: the daemon still has live state for them \
             — a supervisor attached, or an approval still open. Deleting the record while it \
             holds the state would strand it. They are released once the approval ages out \
             (15 minutes), so run this again shortly."
        );
    }
    if kept_unknown > 0 {
        println!(
            "  a session whose state is unknown is never removed: the daemon could not prove what \
             happened to it, and that is exactly when the record is worth keeping"
        );
    }
    if orphan_events > 0 {
        // Not removed, and deliberately so: these belong to no session row at
        // all, so nothing here knows what they are. Saying they exist is the
        // whole contribution — before this they were reachable by no command
        // in the product.
        println!(
            "  note: {orphan_events} event(s) belong to runs with no session row and are \
             reachable by nothing here; they were left untouched"
        );
    }
    if dry_run && !removed.is_empty() {
        println!();
        println!("nothing was removed; run without --dry-run to do it");
    }
}

fn fetch() -> Result<Vec<SessionSummary>> {
    match daemon::request(&ClientFrame::ListSessions)? {
        DaemonFrame::Sessions { sessions } => Ok(sessions),
        other => bail!("unexpected reply from ccd: {other:?}"),
    }
}

fn lifecycle_word(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Spawning => "spawning",
        Lifecycle::Live => "live",
        Lifecycle::Exited => "exited",
        Lifecycle::Unknown => "unknown",
    }
}

/// One line summarising a fleet, with each state named rather than reduced to a
/// total. A count of 45 says nothing; "26 live, 18 exited, 1 unknown" is the
/// sentence that made the original defect visible.
fn tally(sessions: &[SessionSummary]) -> String {
    let count = |wanted: Lifecycle| {
        sessions
            .iter()
            .filter(|session| session.lifecycle == wanted)
            .count()
    };
    format!(
        "{} session(s): {} live, {} exited, {} unknown, {} spawning",
        sessions.len(),
        count(Lifecycle::Live),
        count(Lifecycle::Exited),
        count(Lifecycle::Unknown),
        count(Lifecycle::Spawning),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(uid: &str, lifecycle: Lifecycle) -> SessionSummary {
        SessionSummary {
            session_uid: uid.into(),
            session_id: "cc-1".into(),
            tmux_session: "cc-1".into(),
            cwd: "/tmp".into(),
            lifecycle,
            link: protocol::event::Link::Detached,
            claude_session_id: None,
            transcript_path: None,
            last_seq: 7,
            created_at: "t".into(),
            updated_at: "t".into(),
            blocked_on: Vec::new(),
        }
    }

    #[test]
    fn a_fleet_is_counted_by_state_rather_than_reduced_to_a_total() {
        // The sentence that made the defect visible in the first place: 26 of
        // 45 sessions reporting `live` on a Mac with no tmux server. A bare
        // total would have said "45 sessions" and hidden it completely.
        let fleet = vec![
            summary("a", Lifecycle::Live),
            summary("b", Lifecycle::Live),
            summary("c", Lifecycle::Exited),
            summary("d", Lifecycle::Unknown),
        ];
        let tally = tally(&fleet);
        assert!(tally.contains("2 live"), "{tally}");
        assert!(tally.contains("1 exited"), "{tally}");
        assert!(tally.contains("1 unknown"), "{tally}");
    }

    #[test]
    fn every_lifecycle_has_a_word_and_none_of_them_is_blank() {
        for lifecycle in [
            Lifecycle::Spawning,
            Lifecycle::Live,
            Lifecycle::Exited,
            Lifecycle::Unknown,
        ] {
            assert!(!lifecycle_word(lifecycle).is_empty());
        }
        // A run that ended and one nothing is known about must never render
        // the same: the whole point of keeping `unknown` is that it is visibly
        // not a claim.
        assert_ne!(
            lifecycle_word(Lifecycle::Exited),
            lifecycle_word(Lifecycle::Unknown)
        );
    }
}
