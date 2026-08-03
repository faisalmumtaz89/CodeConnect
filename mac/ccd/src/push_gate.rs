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
//!   * **The ambient latch** is per session: Claude re-fires `idle_prompt` on
//!     a timer, so "still waiting" arrives as a stream of new events that all
//!     carry the same news. The latch pushes a class once and stays shut until
//!     the class changes or the run demonstrably moves (a tool runs, a turn
//!     ends, text lands) — at which point a later wait is a new fact again.
//!
//! Permission requests are deliberately **not** ambient: a second approval is
//! a second decision, always news. They are keyed by `prompt_id`, which also
//! silences their `permission_prompt` notification twin — the same prompt
//! arrives through two hooks, and one decision deserves one ring.
//!
//! In memory only, on purpose. A subscribe reseeds delivery from the client's
//! own `after_seq`, and the cost of a daemon restart is at most one repeated
//! ring for a state that predates it — not worth a table.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// The two ambient classes a session's quiet-state pushes collapse into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ambient {
    /// `agent_needs_input` and `idle_prompt`: the run wants a human.
    Waiting,
    /// `agent_completed`: the run finished a turn.
    Done,
}

/// What a scheduled push captures at gating time and re-checks at dispatch,
/// so the grace window cannot deliver a fact the world has moved past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    session_epoch: u64,
    progress_epoch: u64,
}

/// **Lock order: `epochs` → `ambient` → `permission` → `delivered`.** Every
/// compound operation acquires in that order and holds what it needs
/// simultaneously, which is what makes the gate's promises *constructive*
/// rather than probabilistic: admission reads its ticket under the same
/// guard that flips the latch, so progress cannot slip between them; the
/// dispatch-time validity check and exclusion snapshot share one guard pair
/// with eviction, so a delete can never be half-visible — either the push
/// sees the old epoch with the watermarks intact, or the new epoch and
/// drops.
#[derive(Default)]
pub struct PushGate {
    /// device_id -> session_uid -> highest seq written to that device's socket.
    delivered: Mutex<HashMap<String, HashMap<String, u64>>>,
    /// session_uid -> the last ambient class actually pushed.
    ambient: Mutex<HashMap<String, Ambient>>,
    /// session_uid -> prompt_ids whose approval already rang.
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

    /// Whether an ambient push of `class` is news for this session; on yes,
    /// the ticket the dispatch must carry. Latch and ticket move under one
    /// guard: read apart, a `note_progress` landing between them would fold
    /// into the ticket, and the push it should have cancelled would ring.
    pub fn admit_ambient(&self, session_uid: &str, class: Ambient) -> Option<Ticket> {
        let epochs = self.epochs.lock().unwrap();
        let mut ambient = self.ambient.lock().unwrap();
        match ambient.get(session_uid) {
            Some(last) if *last == class => None,
            _ => {
                ambient.insert(session_uid.to_string(), class);
                Some(ticket_under_lock(&epochs, session_uid))
            }
        }
    }

    /// The run demonstrably moved — a tool ran, a turn ended, injected text
    /// landed. The latch opens so a *later* quiet state is news again, and any
    /// quiet-state push still waiting out its grace is stale and must not
    /// ring: the wait it announces is over.
    pub fn note_progress(&self, session_uid: &str) {
        let mut epochs = self.epochs.lock().unwrap();
        epochs.entry(session_uid.to_string()).or_default().1 += 1;
        self.ambient.lock().unwrap().remove(session_uid);
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

    /// Whether this approval is news; on yes, the ticket the dispatch must
    /// carry. Records on first ask, so the `permission_prompt` notification
    /// that follows the `PermissionRequest` hook — same prompt, second
    /// delivery — finds it and stays silent.
    pub fn admit_permission(&self, session_uid: &str, prompt_id: &str) -> Option<Ticket> {
        let epochs = self.epochs.lock().unwrap();
        let mut permission = self.permission.lock().unwrap();
        permission
            .entry(session_uid.to_string())
            .or_default()
            .insert(prompt_id.to_string())
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
        self.ambient.lock().unwrap().remove(session_uid);
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
        let ticket = gate
            .admit_ambient(run, Ambient::Waiting)
            .unwrap_or_else(|| {
                gate.note_progress(run);
                gate.admit_ambient(run, Ambient::Waiting).unwrap()
            });
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
    fn the_ambient_latch_pushes_a_class_once() {
        let gate = PushGate::new();
        assert!(
            gate.admit_ambient("run", Ambient::Waiting).is_some(),
            "first wait is news"
        );
        assert!(
            gate.admit_ambient("run", Ambient::Waiting).is_none(),
            "idle_prompt re-fires are the same news"
        );
        assert!(
            gate.admit_ambient("run", Ambient::Done).is_some(),
            "the turn ending is a change"
        );
        assert!(
            gate.admit_ambient("run", Ambient::Waiting).is_some(),
            "waiting after done is a change"
        );
    }

    #[test]
    fn progress_reopens_the_latch() {
        let gate = PushGate::new();
        assert!(gate.admit_ambient("run", Ambient::Waiting).is_some());
        gate.note_progress("run");
        assert!(
            gate.admit_ambient("run", Ambient::Waiting).is_some(),
            "after a tool ran, a new wait is a new fact"
        );
    }

    #[test]
    fn each_approval_rings_once_including_its_notification_twin() {
        let gate = PushGate::new();
        assert!(
            gate.admit_permission("run", "prompt-1").is_some(),
            "the request rings"
        );
        assert!(
            gate.admit_permission("run", "prompt-1").is_none(),
            "its permission_prompt notification is the same prompt"
        );
        assert!(
            gate.admit_permission("run", "prompt-2").is_some(),
            "a second decision is second news"
        );
    }

    /// The admission ticket is *of the admission moment*: progress landing
    /// after it must invalidate the dispatch, and eviction between the check
    /// and the snapshot cannot be half-seen — `exclusions_if_valid` refuses
    /// outright once the epoch moved.
    #[test]
    fn dispatch_sees_eviction_and_late_progress_whole() {
        let gate = PushGate::new();
        gate.note_delivered("phone", "run", 10);

        let ticket = gate.admit_ambient("run", Ambient::Waiting).unwrap();
        gate.note_progress("run");
        assert_eq!(
            gate.exclusions_if_valid("run", ticket, true, 5),
            None,
            "progress after admission cancels a quiet-state push"
        );

        let ticket = gate.admit_ambient("run", Ambient::Waiting).unwrap();
        gate.evict_session("run");
        assert_eq!(
            gate.exclusions_if_valid("run", ticket, false, 5),
            None,
            "eviction cancels even an approval push — never an empty exclusion list"
        );

        let ticket = gate.admit_ambient("run", Ambient::Waiting).unwrap();
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
        assert!(gate.admit_ambient("run", Ambient::Waiting).is_some());
        assert!(gate.admit_permission("run", "p1").is_some());
        gate.evict_session("run");
        assert!(
            gate.admit_ambient("run", Ambient::Waiting).is_some(),
            "a fresh run starts fresh"
        );
        assert!(gate.admit_permission("run", "p1").is_some());
        // Last: the helper admits (or forces) a ticket of its own, which
        // would close the fresh latch the assertions above exist to observe.
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
