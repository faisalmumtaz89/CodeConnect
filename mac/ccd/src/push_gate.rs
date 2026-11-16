//! What may ring a phone, and what may not.
//!
//! Two independent gates, both answering the owner's rule — *push only when
//! there is a change while I am away*:
//!
//!   * **The seen-filter** is per device: a phone that received an event over
//!     its live socket has seen it, and ringing it later about the same fact
//!     is noise. Fed by the WebSocket layer after every successful send —
//!     never before, because "written to the socket" is the only delivery this
//!     side can attest — and consulted at push-dispatch time.
//!   * **The quiet-state ticket** is per session: a run is in one quiet state
//!     at a time, so a new one retires the run's older quiet-state push, held
//!     or in its grace, and the run demonstrably moving retires one still
//!     waiting — it never rings a state the run has left. Moving is a Codex turn
//!     or thread starting ([`PushGate::note_progress`]), and a decision or a turn
//!     end, whose admissions take the same step ([`PushGate::admit_decision`],
//!     [`PushGate::admit_turn_end`]).
//!
//! Permission requests are not quiet states: a second approval is a second
//! decision, always news, and each card rings once.
//!
//! In memory only, on purpose. A subscribe reseeds delivery from the client's
//! own `after_seq`, and the cost of a daemon restart is at most one repeated
//! ring for a state that predates it — not worth a table.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// What a scheduled push captures at gating time and re-checks at dispatch,
/// so the grace window cannot deliver a fact the world has moved past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    session_epoch: u64,
    progress_epoch: u64,
}

/// **Lock order: `epochs` → `permission` → `delivered`.** Every compound
/// operation acquires in that order and holds what it needs simultaneously,
/// which is what makes the gate's promises *constructive* rather than
/// probabilistic: admission reads its ticket under the same guard that bumps
/// the epoch, so progress cannot slip between them; the dispatch-time validity
/// check and exclusion snapshot share one guard pair with eviction, so a delete
/// can never be half-visible — either the push sees the old epoch with the
/// watermarks intact, or the new epoch and drops.
#[derive(Default)]
pub struct PushGate {
    /// device_id -> session_uid -> highest seq written to that device's socket.
    delivered: Mutex<HashMap<String, HashMap<String, u64>>>,
    /// session_uid -> the cards that already rang.
    permission: Mutex<HashMap<String, HashSet<String>>>,
    /// session_uid -> (bumped on evict, bumped on progress). A push dispatched
    /// after its grace compares its ticket against these — see
    /// `exclusions_if_valid`.
    epochs: Mutex<HashMap<String, (u64, u64)>>,
}

fn ticket_under_lock(epochs: &HashMap<String, (u64, u64)>, session_uid: &str) -> Ticket {
    let (session_epoch, progress_epoch) = epochs.get(session_uid).copied().unwrap_or((0, 0));
    Ticket {
        session_epoch,
        progress_epoch,
    }
}

/// A quiet state's ticket, past the run's older one: see the module notes.
fn quiet_ticket(epochs: &mut HashMap<String, (u64, u64)>, session_uid: &str) -> Ticket {
    epochs.entry(session_uid.to_string()).or_default().1 += 1;
    ticket_under_lock(epochs, session_uid)
}

impl PushGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// A frame reached this device. Monotonic: a second connection replaying
    /// from an older watermark must not un-see what the first delivered.
    pub fn note_delivered(&self, device_id: &str, session_uid: &str, seq: u64) {
        let mut delivered = self.delivered.lock().unwrap();
        let per_session = delivered.entry(device_id.to_string()).or_default();
        let entry = per_session.entry(session_uid.to_string()).or_insert(0);
        if seq > *entry {
            *entry = seq;
        }
    }

    /// The run demonstrably moved — a Codex turn or thread started. Any
    /// quiet-state push still waiting out its grace is stale and must not ring:
    /// the wait it announces is over.
    pub fn note_progress(&self, session_uid: &str) {
        let mut epochs = self.epochs.lock().unwrap();
        epochs.entry(session_uid.to_string()).or_default().1 += 1;
    }

    /// The dispatch step, whole: either the ticket still stands and here are
    /// the devices that must not ring, or the world moved and the push dies.
    /// Check and snapshot share one guard pair with `evict_session`, so an
    /// eviction is never half-visible — the sequence "validity passes, then
    /// eviction empties the watermarks, then the snapshot reads them empty"
    /// cannot be scheduled.
    pub fn exclusions_if_valid(
        &self,
        session_uid: &str,
        ticket: Ticket,
        heed_progress: bool,
        seq: u64,
    ) -> Option<Vec<String>> {
        let epochs = self.epochs.lock().unwrap();
        let delivered = self.delivered.lock().unwrap();
        let now = ticket_under_lock(&epochs, session_uid);
        if now.session_epoch != ticket.session_epoch {
            return None;
        }
        if heed_progress && now.progress_epoch != ticket.progress_epoch {
            return None;
        }
        Some(
            delivered
                .iter()
                .filter(|(_, sessions)| sessions.get(session_uid).is_some_and(|&s| s >= seq))
                .map(|(device, _)| device.clone())
                .collect(),
        )
    }

    /// **A turn ended, and that is both the movement and the news — one
    /// transition.**
    ///
    /// It arrives exactly once per turn. A quiet-state push still sitting out
    /// its dispatch grace is describing a wait this turn's end has ended, and the
    /// moved epoch kills it; the next turn to finish is news again.
    ///
    /// Taken under one guard for the reason [`admit_decision`](Self::admit_decision)
    /// is: read apart, a `note_progress` from some other transition landing between
    /// the bump and the admission folds into this ticket, so the progress that
    /// exists to cancel this push arrives just too early to.
    ///
    /// What the ticket does *not* promise is that this push will ring: a later
    /// turn ending inside the dispatch grace bumps the epoch again and supersedes
    /// it, which is the same coalescing every quiet-state push on either agent's
    /// path is subject to — see [`crate::state::Daemon::push_codex_turn_complete`].
    pub fn admit_turn_end(&self, session_uid: &str) -> Ticket {
        let mut epochs = self.epochs.lock().unwrap();
        quiet_ticket(&mut epochs, session_uid)
    }

    /// **A decision arrived, and the run moved to meet it — one transition.**
    ///
    /// Recording progress and admitting the card are the same event and are
    /// taken under the same guard, so the progress that cancels an older
    /// quiet-state push cannot land between them. `None` means this card already
    /// rang: one decision deserves one ring.
    pub fn admit_decision(&self, session_uid: &str, card: &str) -> Option<Ticket> {
        let mut epochs = self.epochs.lock().unwrap();
        let mut permission = self.permission.lock().unwrap();
        epochs.entry(session_uid.to_string()).or_default().1 += 1;
        permission
            .entry(session_uid.to_string())
            .or_default()
            .insert(card.to_string())
            .then(|| ticket_under_lock(&epochs, session_uid))
    }

    /// A deleted or pruned run has nothing left to ring about — including any
    /// push still waiting out its dispatch grace. The epoch guard is held
    /// across the whole teardown (see the lock-order note on the struct):
    /// `exclusions_if_valid` holding the same guard therefore sees either the
    /// world before this line or the world after it, never the middle.
    pub fn evict_session(&self, session_uid: &str) {
        let mut epochs = self.epochs.lock().unwrap();
        epochs.entry(session_uid.to_string()).or_default().0 += 1;
        self.permission.lock().unwrap().remove(session_uid);
        for sessions in self.delivered.lock().unwrap().values_mut() {
            sessions.remove(session_uid);
        }
    }

    /// A revoked device is nobody's push target and holds no watermarks.
    pub fn evict_device(&self, device_id: &str) {
        self.delivered.lock().unwrap().remove(device_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exclusion list a fresh (still-valid) ticket sees.
    fn saw(gate: &PushGate, run: &str, seq: u64) -> Vec<String> {
        let ticket = gate.admit_turn_end(run);
        gate.exclusions_if_valid(run, ticket, false, seq).unwrap()
    }

    #[test]
    fn delivery_is_monotonic_and_per_session() {
        let gate = PushGate::new();
        gate.note_delivered("phone", "run-a", 10);
        gate.note_delivered("phone", "run-a", 4);
        assert_eq!(saw(&gate, "run-a", 10), vec!["phone".to_string()]);
        assert!(
            saw(&gate, "run-a", 11).is_empty(),
            "an older replay must not un-see seq 10"
        );
        assert!(
            saw(&gate, "run-b", 1).is_empty(),
            "sessions do not share watermarks"
        );
    }

    #[test]
    fn each_approval_rings_once() {
        let gate = PushGate::new();
        assert!(
            gate.admit_decision("run", "card-1").is_some(),
            "the request rings"
        );
        assert!(
            gate.admit_decision("run", "card-1").is_none(),
            "the same card again is the same news"
        );
        assert!(
            gate.admit_decision("run", "card-2").is_some(),
            "a second decision is second news"
        );
    }

    /// A decision retires its run's quiet-state push still in its grace: the run
    /// moved to meet it.
    #[test]
    fn a_decision_retires_its_runs_waiting_quiet_push() {
        let gate = PushGate::new();
        let quiet = gate.admit_turn_end("run");
        assert!(gate.exclusions_if_valid("run", quiet, true, 1).is_some());
        assert!(gate.admit_decision("run", "card").is_some());
        assert_eq!(gate.exclusions_if_valid("run", quiet, true, 1), None);
    }

    /// The admission ticket is *of the admission moment*: progress landing
    /// after it must invalidate the dispatch, and eviction between the check
    /// and the snapshot cannot be half-seen — `exclusions_if_valid` refuses
    /// outright once the epoch moved.
    #[test]
    fn dispatch_sees_eviction_and_late_progress_whole() {
        let gate = PushGate::new();
        gate.note_delivered("phone", "run", 10);

        let ticket = gate.admit_turn_end("run");
        gate.note_progress("run");
        assert_eq!(
            gate.exclusions_if_valid("run", ticket, true, 5),
            None,
            "progress after admission cancels a quiet-state push"
        );

        let ticket = gate.admit_turn_end("run");
        gate.evict_session("run");
        assert_eq!(
            gate.exclusions_if_valid("run", ticket, false, 5),
            None,
            "eviction cancels even an approval push — never an empty exclusion list"
        );

        let ticket = gate.admit_turn_end("run");
        gate.note_delivered("phone", "run", 10);
        assert_eq!(
            gate.exclusions_if_valid("run", ticket, true, 5),
            Some(vec!["phone".to_string()]),
            "a standing ticket yields the exclusions in the same breath"
        );
    }

    #[test]
    fn eviction_forgets_a_session_everywhere() {
        let gate = PushGate::new();
        gate.note_delivered("phone", "run", 9);
        assert!(gate.admit_decision("run", "c1").is_some());
        gate.evict_session("run");
        assert!(
            gate.admit_decision("run", "c1").is_some(),
            "a fresh run starts fresh"
        );
        assert!(saw(&gate, "run", 1).is_empty());
    }

    #[test]
    fn a_revoked_device_is_forgotten() {
        let gate = PushGate::new();
        gate.note_delivered("phone", "run", 9);
        gate.evict_device("phone");
        assert!(saw(&gate, "run", 1).is_empty());
    }
}
