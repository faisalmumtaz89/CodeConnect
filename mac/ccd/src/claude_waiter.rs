//! **The Claude waiter lease: one waiting hook per run, and only the newest.**
//!
//! Claude Code runs an `asyncRewake` hook on every `Stop` and `SessionStart`, and
//! each one connects here and waits. An exit 2 from it wakes Claude, even when the
//! session is idle. The hook processes pile up: one per turn end, each alive until
//! Claude's 12-hour timeout. So the daemon keeps exactly one lease per run:
//! - a newer waiter supersedes the older one, which is told so and exits quietly;
//! - a waiter going away clears only its own lease, never a newer one;
//! - a wake goes to the live lease alone, and spends it.
//!
//! The lease never decides WHEN to wake. That is the mailbox's wake rule
//! ([`crate::mail`]), which is also told about every registration so that mail
//! already waiting is seen at once.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// What a waiting hook is told: wake Claude, or step aside for a newer waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaiterOutcome {
    Wake,
    Superseded,
}

struct Lease {
    generation: u64,
    tell: oneshot::Sender<WaiterOutcome>,
}

/// Every run's live lease.
#[derive(Default)]
pub(crate) struct Waiters(Mutex<Leases>);

#[derive(Default)]
struct Leases {
    by_run: HashMap<String, Lease>,
    last_generation: u64,
}

impl Waiters {
    /// **Take the lease for `run_uid`.** The waiter it replaces is told
    /// [`WaiterOutcome::Superseded`]. The generation is what [`Waiters::release`]
    /// is scoped to; the receiver is where this waiter's outcome arrives.
    pub(crate) fn register(&self, run_uid: &str) -> (u64, oneshot::Receiver<WaiterOutcome>) {
        let mut leases = self.0.lock().unwrap();
        leases.last_generation += 1;
        let generation = leases.last_generation;
        let (tell, outcome) = oneshot::channel();
        if let Some(old) = leases
            .by_run
            .insert(run_uid.to_string(), Lease { generation, tell })
        {
            let _ = old.tell.send(WaiterOutcome::Superseded);
        }
        (generation, outcome)
    }

    /// **A waiter has gone** (its hook exited, or Claude ended it). Clears the lease
    /// only if it is still this waiter's: a newer one is left alone.
    pub(crate) fn release(&self, run_uid: &str, generation: u64) {
        let mut leases = self.0.lock().unwrap();
        if leases
            .by_run
            .get(run_uid)
            .is_some_and(|lease| lease.generation == generation)
        {
            leases.by_run.remove(run_uid);
        }
    }

    /// **Wake Claude through the live lease**, spending it. `false` when no waiter
    /// holds it, or the one that did is already gone; the mail then waits for the
    /// next registration or the person's next prompt.
    pub(crate) fn wake(&self, run_uid: &str) -> bool {
        let lease = self.0.lock().unwrap().by_run.remove(run_uid);
        lease.is_some_and(|lease| lease.tell.send(WaiterOutcome::Wake).is_ok())
    }
}

/// **Wake the Claude run `run_uid`**: its waiter's hook exits 2 with
/// [`protocol::ipc::MAIL_NUDGE`]. `false` when no live waiter holds the lease.
pub(crate) fn wake_claude(daemon: &std::sync::Arc<crate::state::Daemon>, run_uid: &str) -> bool {
    daemon.waiters.wake(run_uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";

    #[test]
    fn a_wake_reaches_the_live_waiter_and_spends_the_lease() {
        let waiters = Waiters::default();
        let (_, mut outcome) = waiters.register(RUN);
        assert!(waiters.wake(RUN));
        assert_eq!(outcome.try_recv(), Ok(WaiterOutcome::Wake));
        assert!(!waiters.wake(RUN), "one lease, one wake");
    }

    #[test]
    fn with_no_waiter_there_is_no_wake() {
        let waiters = Waiters::default();
        assert!(!waiters.wake(RUN));
        let (generation, _) = waiters.register("01K1B3XQ8ZC0DE5FGH7JKMNPQS");
        assert!(!waiters.wake(RUN), "another run's waiter is not this run's");
        waiters.release("01K1B3XQ8ZC0DE5FGH7JKMNPQS", generation);
    }

    #[test]
    fn a_newer_waiter_supersedes_the_older_and_takes_the_wake() {
        let waiters = Waiters::default();
        let (_, mut old) = waiters.register(RUN);
        let (_, mut new) = waiters.register(RUN);
        assert_eq!(old.try_recv(), Ok(WaiterOutcome::Superseded));
        assert!(waiters.wake(RUN));
        assert_eq!(new.try_recv(), Ok(WaiterOutcome::Wake));
    }

    #[test]
    fn an_old_waiter_going_away_cannot_clear_a_newer_lease() {
        let waiters = Waiters::default();
        let (old_generation, _old) = waiters.register(RUN);
        let (_, mut new) = waiters.register(RUN);
        waiters.release(RUN, old_generation);
        assert!(
            waiters.wake(RUN),
            "the newer lease survives the older waiter leaving"
        );
        assert_eq!(new.try_recv(), Ok(WaiterOutcome::Wake));
    }

    #[test]
    fn a_waiter_going_away_clears_its_own_lease() {
        let waiters = Waiters::default();
        let (generation, outcome) = waiters.register(RUN);
        drop(outcome);
        waiters.release(RUN, generation);
        assert!(!waiters.wake(RUN));
    }

    #[test]
    fn a_waiter_that_died_unnoticed_is_no_wake() {
        let waiters = Waiters::default();
        let (_, outcome) = waiters.register(RUN);
        drop(outcome);
        assert!(
            !waiters.wake(RUN),
            "a wake into a gone waiter must not count as issued"
        );
    }
}
