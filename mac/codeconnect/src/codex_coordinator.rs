//! The **coordinator** and the launcher's record-wait (D7 launch coordination).
//!
//! `codeconnect codex` (the launcher, gated in this chunk) spawns the
//! coordinator **before tmux exists** and then only *waits on the launch
//! record*. The coordinator performs **every forward launch mutation itself** —
//! it arms the custodian, runs `tmux new-session`, brings the wrapper up, and
//! drives the record `pending → ready | failed`. The launcher never mutates, so
//! launcher death at any point changes nothing (CODEX-PLAN.md §Launch
//! coordination, steps 3–4).
//!
//! ## The ordering that matters (D7)
//!
//!   1. Write the `pending` record (the coordinator owns it from birth).
//!   2. Arm the custodian **before** `tmux new-session` — and immediately verify
//!      it is alive: a live coordinator that has already lost its custodian
//!      **fails the launch** rather than mutate tmux without an independent
//!      cleanup owner.
//!   3. `tmux new-session`. A **timed-out** (indeterminate) mutation is *not*
//!      cleaned by the coordinator — it records `failed{cleanup:pending,
//!      new_session_indeterminate}` and hands the late-UID problem to the
//!      retained custodian (tmux.rs:42; no probe-then-expire).
//!   4. Bring the wrapper up and **prove** it, then re-check the custodian and
//!      the deadline before committing `ready`.
//!
//! The wrapper bring-up stays a seam (`CoordinatorDeps::bring_up_wrapper`) so the
//! state machine can be driven at every boundary with fakes — but the real
//! implementation is no longer a placeholder: the pane runs the real
//! `internal-codex-host` ([`crate::codex_host`]), and bring-up is proven from the
//! host's own evidence rather than asserted.
//!
//! ## What "the wrapper is up" means here (2e-2b)
//!
//! The coordinator chooses a fresh, SUN_LEN-safe **run dir**, records it durably,
//! and passes it to the host, which creates it itself with one exclusive
//! `mkdir(0700)` and binds three sockets under it. Readiness is then two
//! independent facts, both required:
//!
//!   * **Both broker legs are bound** — `tui.sock` and `ccd.sock` exist as
//!     sockets under the run dir. That is the host's step 2 having completed, and
//!     it is meaningful *because* the directory was provably fresh: nothing under
//!     it can be stale or pre-planted (see [`crate::codex_host`]'s invariant 1).
//!     The host binds both legs before it spawns the TUI, so this is the last
//!     moment at which a failure is still the wrapper's rather than the user's.
//!   * **The pane's session is still ours** — `owned_liveness` against the
//!     persisted server A, i.e. the 2c UID-atomic census, not a name-addressed
//!     `has-session`. Sockets on disk say nothing about whether the pane that made
//!     them still exists.
//!
//! Neither is inferred from the other, and an *unprovable* census is never read as
//! `Live`: a launch that cannot prove both facts inside the launch deadline fails
//! rather than commits.

use crate::codex_launch::{
    self, deadline_expiry, CleanupState, Expiry, LaunchLock, LaunchRecord, LaunchState, NewLaunch,
};
use anyhow::{Context, Result};
use protocol::proc_identity::{liveness, Liveness, ProcessIdentity};
use protocol::tmux::OwnedSession;

#[cfg(test)]
thread_local! {
    /// One-shot: the next `remain-on-exit` assertion fails with this reason instead
    /// of running tmux. Per test *thread*, like `codex_launch`'s fault seams, so
    /// parallel tests cannot arm each other's.
    static ASSERT_FAULT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    /// One-shot: the next durable note of that assertion fails.
    static NOTE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot assertion failure inside
/// [`RealCoordinatorDeps::finish_new_session`].
///
/// The two post-create failure arms live in the REAL coordinator deps and are
/// reachable on a live machine only with real tmux AND an assertion that refuses —
/// a combination no test can stage. That is what let round-2's finding-4 fix be
/// placed by inspection and let round-3's finding 6 (the persistence being undone
/// one statement later) survive a passing suite: the helper was tested, the caller
/// was not. These seams drive the actual caller.
#[cfg(test)]
pub(crate) fn fail_next_remain_assertion(why: &str) {
    ASSERT_FAULT.with(|f| *f.borrow_mut() = Some(why.to_string()));
}

#[cfg(test)]
fn take_assert_fault() -> Option<String> {
    ASSERT_FAULT.with(|f| f.borrow_mut().take())
}

/// Arm the one-shot failure of the durable note (see [`fail_next_remain_assertion`]).
#[cfg(test)]
pub(crate) fn fail_next_remain_note() {
    NOTE_FAULT.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_note_fault() -> bool {
    NOTE_FAULT.with(|armed| armed.replace(false))
}

/// The outcome of the coordinator's forward `tmux new-session`.
pub enum NewSessionOutcome {
    /// The session was created and resolved to a pinned [`OwnedSession`].
    Created(Box<OwnedSession>),
    /// tmux ran past its deadline: **indeterminate**. The coordinator records
    /// failure with the indeterminate flag and hands off to the custodian.
    Indeterminate,
    /// **The session was created, resolved and PERSISTED — and then a later step of
    /// the same call failed** (round-3 finding 6).
    ///
    /// Distinct from [`NewSessionOutcome::Indeterminate`], and the distinction is
    /// the whole point. These paths used to return `Indeterminate` after having
    /// written server A down, and `after_pending`'s indeterminate arm then called
    /// `fail_new_session_indeterminate`, which sets `new_session_indeterminate` back
    /// to **true** — undoing, one statement later, the very fact the persist had just
    /// established. A record in that shape says "a late session may still appear",
    /// which is false: the session appeared, it was resolved, and its identity is on
    /// disk. The custodian then refuses to believe any absence it observes and stays
    /// armed until the next reboot.
    ///
    /// Nothing about this outcome is indeterminate, so it terminalizes **determinate**
    /// with `cleanup: Pending` — a session exists, it is pinned, and it is owed
    /// cleanup that can now be bound to a proven server identity.
    CreatedThenFailed(String),
    /// tmux answered with a definite failure.
    Failed(String),
}

/// What the wrapper bring-up reported.
#[derive(Debug, PartialEq, Eq)]
pub enum BringUp {
    /// The host's evidence was observed: both broker legs bound under the run
    /// dir, and the pane's session still proven ours.
    Ready,
    /// Bring-up could not be proven inside the deadline, or was proven lost.
    Failed(String),
}

/// The forward-launch operations the coordinator performs. A trait so the state
/// machine can be exercised at every boundary with fakes.
pub trait CoordinatorDeps {
    /// Spawn the custodian (through the D6 exec gate) for `uid`, **arming it into
    /// the launch record** (the CAS into the custodian slot), and return its
    /// identity. The uid is passed so the arm targets the right record; the real
    /// impl arms inside the gate's `on_ready` (atomic with recording the child).
    fn spawn_custodian(&mut self, uid: &str) -> Result<ProcessIdentity>;
    /// Perform `tmux new-session` (the coordinator's own forward mutation).
    fn new_session(&mut self) -> NewSessionOutcome;
    /// Wait for the pane's `codex-host` to prove itself up, bounded by the launch
    /// deadline. Never fabricates readiness: see [`RealCoordinatorDeps`].
    fn bring_up_wrapper(&mut self) -> BringUp;
    /// Liveness of the armed custodian. Defaulted to the real kernel check.
    fn custodian_liveness(&self, id: &ProcessIdentity) -> Liveness {
        liveness(id)
    }
    /// The disposable run dir the pane's host will own, recorded durably before
    /// `tmux new-session` so the custodian can sweep it even if the host never
    /// ran. `None` only for deps that create no pane at all — there is then no
    /// directory to name.
    ///
    /// Deliberately **not** defaulted: a default of `None` is fail-open on a
    /// cleanup-critical value, and a future dep that forgot to override it would
    /// silently record no run dir and silently sweep nothing. Every implementor
    /// must say which it is.
    fn run_dir(&self) -> Option<&str>;
}

/// Everything the launcher hands the coordinator.
pub struct CoordinateSetup {
    pub uid: String,
    pub launch_nonce: String,
    pub session_name: String,
    pub coordinator: ProcessIdentity,
    pub boot: protocol::proc_identity::BootIdentity,
    /// Absolute `CLOCK_MONOTONIC` nanoseconds deadline.
    pub deadline_monotonic_nanos: u64,
    pub created_ms: i64,
}

/// The coordinator's terminal verdict for this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinateOutcome {
    Ready,
    Failed(String),
}

/// Drive one launch from `pending` to a terminal record state.
///
/// This is the whole D7 forward path as a linear, fail-closed sequence. Every
/// exit writes a terminal record (`ready` or `failed`) so the launcher's wait
/// always resolves; the only thing that can leave the record `pending` is the
/// coordinator dying, which is exactly what the custodian's deadline/identity
/// monitor exists to convert into `failed`.
pub fn coordinate<D: CoordinatorDeps>(
    setup: CoordinateSetup,
    deps: &mut D,
) -> Result<CoordinateOutcome> {
    let uid = setup.uid.clone();

    // Step 1: the pending record — a single-owner ABSENT→Pending CAS, the
    // coordinator's first durable act.
    {
        let lock = LaunchLock::acquire(&uid)?;
        let created = codex_launch::create_pending(
            &lock,
            NewLaunch {
                launch_nonce: setup.launch_nonce.clone(),
                uid: uid.clone(),
                session_name: setup.session_name.clone(),
                coordinator: setup.coordinator,
                boot: setup.boot,
                deadline_monotonic_nanos: setup.deadline_monotonic_nanos,
                created_ms: setup.created_ms,
            },
        );
        drop(lock);
        if let Err(err) = created {
            // create_pending can fail *after* the record's rename landed but a
            // later durability step did not (finding 7), leaving a guardianless
            // `pending` that is unambiguously ours. Terminalize precisely that —
            // never another coordinator's record (the duplicate-existing case) —
            // so no path exits with a guardianless pending. A record that is not
            // ours, or no record at all, needs nothing.
            terminalize_own_orphan(
                &uid,
                &setup.coordinator,
                &setup.launch_nonce,
                &format!("create_pending failed: {err:#}"),
            )?;
            return Err(err);
        }
    }

    // Everything after the record exists runs inside `after_pending`; any error
    // it returns must NOT leave a `pending` record with no guardian — it is
    // terminalized to `failed{cleanup:pending}` so the custodian/sweep can reap
    // it. Terminalization is **not best-effort** (finding 7): a record that
    // cannot be driven to a terminal, guardian-owned state is a hard error, never
    // a quiet Ok(Failed).
    match after_pending(&setup, deps) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let reason = format!("coordinator error after the record was written: {err:#}");
            terminalize(&uid, &reason)?;
            Ok(CoordinateOutcome::Failed(reason))
        }
    }
}

/// Drive the record to `failed` **and durably disarm the indeterminate flag**,
/// within a bounded deadline. The catch-all for any error out of `after_pending`.
///
///   * Uses a **non-blocking** `try_acquire` in a bounded retry loop, NOT the
///     blocking `acquire` (finding 3): a stopped lock holder must not hang the
///     coordinator forever — a lock we cannot get within the deadline is a hard
///     error, not a hang.
///   * Uses [`codex_launch::fail_new_session_determinate`], which **clears**
///     `new_session_indeterminate` atomically (finding 4): a coordinator that is
///     alive to terminalize knows the outcome is determinate (either tmux never
///     ran, or it ran and returned), so the speculative flag must not survive to
///     trap the custodian on a later `Absent`. The custodian's own
///     coordinator-loss failure keeps using flag-preserving `to_failed`.
///   * A persistent failure is a **hard error** (finding 7): a record that cannot
///     be driven terminal is never reported as a clean `Ok(Failed)`.
///
/// **The cleanup disposition is derived from the record, never assumed (A9.3).**
/// It used to be hard-coded `Pending` for every error on this path, including the
/// ones raised before tmux was ever touched (a failed custodian spawn, a record
/// re-load, the pre-mutation lock/record_run_dir/mark_new_session_starting block).
/// That wrote `Failed{Pending}` on a record with no session, no server A, and no
/// host — and the sweep would then rearm a custodian for it forever: an unpinned
/// `destroy()` returns `Unavailable`, and the retry path's escapes need either
/// server-gone evidence (there is no server to be gone) or a boot change. Armed
/// until reboot, for a session that never existed.
///
/// The rule is **total**, because since A9.1 the two fields it reads partition
/// every durable record this path can observe: `server_a.is_some()` is exactly "a
/// session was created" (A is persisted in the *same* write that records the
/// creation, so there is no window where one holds without the other), and
/// `new_session_indeterminate` is exactly "a mutation may be in flight". Neither
/// set means no session can exist ⇒ `NotRequired`; either set means one may ⇒
/// `Pending`. A record we cannot read falls to the conservative `Pending`.
fn terminalize(uid: &str, reason: &str) -> Result<()> {
    let uid_owned = uid.to_string();
    let reason = reason.to_string();
    bounded_lock_write(uid, std::time::Duration::from_secs(5), move |lock| {
        // Read UNDER the lock the write takes, so the disposition is derived from
        // the record the transition is about to act on, not a stale snapshot the
        // custodian could have moved underneath us.
        let cleanup = match codex_launch::load(&uid_owned) {
            Ok(rec) if rec.server_a.is_none() && !rec.new_session_indeterminate => {
                CleanupState::NotRequired
            }
            _ => CleanupState::Pending,
        };
        codex_launch::fail_new_session_determinate(lock, &uid_owned, &reason, cleanup).map(|_| ())
    })
}

/// [`terminalize`] with an explicit cleanup disposition and budget. `cleanup` is
/// `NotRequired` when the failure happened before any session was created (round-4:
/// so the custodian completes via `Done` instead of probing a nonexistent server
/// forever) and `Pending` when a session may exist.
fn terminalize_within(
    uid: &str,
    reason: &str,
    cleanup: CleanupState,
    budget: std::time::Duration,
) -> Result<()> {
    let uid_owned = uid.to_string();
    let reason = reason.to_string();
    bounded_lock_write(uid, budget, move |lock| {
        codex_launch::fail_new_session_determinate(lock, &uid_owned, &reason, cleanup).map(|_| ())
    })
}

/// Preserve-flag terminalization for the genuinely-**indeterminate** path (finding
/// 5): records `failed` while KEEPING `new_session_indeterminate` set (via
/// [`codex_launch::fail_new_session_indeterminate`]) — the opposite of
/// [`terminalize`]. Bounded like `terminalize`.
fn terminalize_indeterminate(uid: &str, reason: &str) -> Result<()> {
    let uid_owned = uid.to_string();
    let reason = reason.to_string();
    bounded_lock_write(uid, std::time::Duration::from_secs(5), move |lock| {
        codex_launch::fail_new_session_indeterminate(lock, &uid_owned, &reason)
    })
}

/// Commit `pending → ready` with a bounded, durability-proving retry (round-5
/// finding 7). `to_ready` re-fsyncs an already-`Ready` record (its own
/// coordinator), so a retry after a post-rename fsync failure re-proves
/// durability rather than leaving a half-durable visible `Ready`. A stopped lock
/// holder cannot hang it; a persistent failure is a hard error.
fn commit_ready(uid: &str, coordinator: &ProcessIdentity) -> Result<()> {
    let uid_owned = uid.to_string();
    let coord = *coordinator;
    bounded_lock_write(uid, std::time::Duration::from_secs(5), move |lock| {
        codex_launch::to_ready(lock, &uid_owned, &coord)
    })
}

/// Run `write` under the launch lock, acquired **non-blocking** within `budget`
/// (finding 3/7): a stopped lock holder must never hang a terminalization path.
/// A lock unobtainable within the budget — or a persistent write failure — is a
/// hard error.
fn bounded_lock_write(
    uid: &str,
    budget: std::time::Duration,
    write: impl Fn(&LaunchLock) -> Result<()>,
) -> Result<()> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let err = match LaunchLock::try_acquire(uid) {
            Ok(Some(lock)) => match write(&lock) {
                Ok(()) => return Ok(()),
                Err(e) => e,
            },
            Ok(None) => anyhow::anyhow!("the launch lock is held by another holder"),
            Err(e) => e,
        };
        if std::time::Instant::now() >= deadline {
            return Err(err)
                .context("could not write the launch record within the bounded lock deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Terminalize a guardianless `pending` **only when it is unambiguously ours** —
/// same coordinator identity and launch nonce, still `pending`. Used on the
/// `create_pending` failure path so we never touch another coordinator's record
/// (the duplicate-existing case) or a record that already reached a terminal
/// state. A missing/unreadable/foreign record needs nothing and is success.
fn terminalize_own_orphan(
    uid: &str,
    coordinator: &ProcessIdentity,
    launch_nonce: &str,
    reason: &str,
) -> Result<()> {
    match codex_launch::load(uid) {
        Ok(rec)
            if rec.state == LaunchState::Pending
                && rec.coordinator == *coordinator
                && rec.launch_nonce == launch_nonce =>
        {
            // A create_pending orphan never reached new-session ⇒ no session ⇒
            // NotRequired (the custodian/sweep completes it without probing).
            terminalize_within(
                uid,
                reason,
                CleanupState::NotRequired,
                std::time::Duration::from_secs(5),
            )
        }
        _ => Ok(()),
    }
}

/// The forward path once the `pending` record exists. Split out so
/// [`coordinate`] can terminalize the record on any error it returns.
fn after_pending<D: CoordinatorDeps>(
    setup: &CoordinateSetup,
    deps: &mut D,
) -> Result<CoordinateOutcome> {
    let uid = setup.uid.clone();
    let deadline_ok = |record: &LaunchRecord| matches!(deadline_expiry(record), Expiry::Live);

    // Step 2: arm the custodian BEFORE any tmux mutation (the CAS into the
    // record happens inside `spawn_custodian`), then require it **proven Live**
    // — an `Unknown` custodian is not proof of an independent cleanup owner
    // (Principle D).
    let custodian = deps
        .spawn_custodian(&uid)
        .context("spawning the launch custodian")?;
    if deps.custodian_liveness(&custodian) != Liveness::Alive {
        return fail(
            &uid,
            "the launch custodian is not proven live before tmux",
            CleanupState::NotRequired,
        );
    }
    {
        let record = codex_launch::load(&uid)?;
        if !deadline_ok(&record) {
            return fail(
                &uid,
                "launch deadline passed before new-session",
                CleanupState::NotRequired,
            );
        }
    }

    // Step 3: the forward mutation. **Durable-before-mutation** (Principle B):
    // mark the new-session as in-flight and fsync it BEFORE issuing it, so a
    // coordinator death mid-`new-session` leaves a record that already says
    // "indeterminate" and the custodian stays armed.
    let prepared = (|| -> Result<()> {
        let lock = LaunchLock::acquire(&uid)?;
        // The run dir goes into the record under the SAME lock, and before the
        // mutation, for the same Principle-B reason: the host that will own the
        // directory is about to be started by tmux, so a coordinator killed from
        // here on must leave a record that already names what has to be swept.
        if let Some(run_dir) = deps.run_dir() {
            let run_dir = run_dir.to_string();
            codex_launch::record_run_dir(&lock, &uid, &run_dir)?;
        }
        codex_launch::mark_new_session_starting(&lock, &uid)
    })();
    // **A9.3: this block's failures are pinned HERE, not left to the catch-all.**
    //
    // `terminalize` derives the disposition from the record, which is right for
    // every error it cannot attribute — but this one it does not have to guess at.
    // Nothing below has run: `deps.new_session()` is the next statement, so a
    // failure here means tmux was never invoked and no session can exist.
    //
    // The case that makes the difference a wedge rather than a nicety is
    // `mark_new_session_starting` failing *inside* `store_atomic`, after its rename
    // has already published `new_session_indeterminate: true`. The flag is then
    // visible to every reader while the write that set it returned `Err`. Handed to
    // the catch-all, that flag is read as "a mutation may be in flight" and the
    // record is terminalized `Failed{Pending}` — with no session, no server A and
    // no host. The custodian's unpinned `destroy()` can only answer `Unavailable`
    // or `Absent`, `server_gone_evidence` has no identity to bind to, and
    // `late_session_still_possible` refuses to believe the absence: armed until the
    // next reboot, for a session that was never created.
    if let Err(err) = prepared {
        return fail(
            &uid,
            &format!("the launch record could not be prepared for new-session: {err:#}"),
            CleanupState::NotRequired,
        );
    }
    match deps.new_session() {
        NewSessionOutcome::Created(session) => {
            // Determinate: the flag can be cleared. And PERSIST server A (round-5
            // finding 1) — the resolved session+server identity — so the separate
            // custodian and supervisor bind cleanup/liveness to it instead of
            // re-establishing "A" from whatever server owns the socket later. A
            // session without a proven server birth is fail-closed (finding 3).
            //
            // ONE durable write for both (A9.1): clearing the flag and recording A
            // used to be two `store_atomic`s, and a crash between them left a
            // created-but-unpinned record — `new_session_indeterminate: false` with
            // `server_a: null` — which is exactly the shape A9.3's disposition rule
            // must be able to read as "a session exists".
            let a = codex_launch::ServerA::from_owned(&session)
                .context("recording server A from the created session")?;
            let lock = LaunchLock::acquire(&uid)?;
            codex_launch::record_new_session_created(&lock, &uid, a, false)?;
        }
        NewSessionOutcome::Indeterminate => {
            // A genuinely-timed-out mutation must STAY indeterminate (round-4
            // finding 5): the flag was set durably by `mark_new_session_starting`
            // before the mutation, so it must NEVER fall through to the
            // determinate flag-clearing path. `terminalize_indeterminate` records
            // `failed` while PRESERVING the flag, via the bounded try-lock; even
            // if it cannot write, the flag is already durable and the custodian
            // (on our exit) preserves it through flag-preserving `to_failed`.
            let reason = "tmux new-session outcome was indeterminate; custodian retained";
            let _ = terminalize_indeterminate(&uid, reason);
            return Ok(CoordinateOutcome::Failed(reason.into()));
        }
        NewSessionOutcome::CreatedThenFailed(why) => {
            // The session EXISTS, is resolved, and its server A is already on disk —
            // `new_session` persisted it before returning this. So this is a
            // determinate outcome about a known session, and it must terminalize as
            // one: `fail` clears `new_session_indeterminate` in the same durable
            // write that records the failure, which is exactly what the old
            // `Indeterminate` return then un-did (round-3 finding 6).
            //
            // `Pending`, not `NotRequired`: something was created and is owed
            // cleanup — and cleanup can now bind to the persisted A rather than
            // probing by socket+uid.
            return fail(
                &uid,
                &format!("the created tmux session could not be made safe: {why}"),
                CleanupState::Pending,
            );
        }
        NewSessionOutcome::Failed(why) => {
            // A definite failure is determinate: terminalize AND clear the flag
            // (findings 6/7), via the bounded try-lock (never a blocking acquire).
            return fail(
                &uid,
                &format!("tmux new-session failed: {why}"),
                // A definite new-session failure created no session ⇒ nothing to
                // clean (round-4): NotRequired so the custodian completes cleanly.
                CleanupState::NotRequired,
            );
        }
    }

    // Between mutation and commit: custodian proven Live and deadline still held.
    // A session WAS created (new-session succeeded above), so failures here need
    // cleanup ⇒ Pending.
    if deps.custodian_liveness(&custodian) != Liveness::Alive {
        return fail(
            &uid,
            "the launch custodian is not proven live before the session was ready",
            CleanupState::Pending,
        );
    }
    {
        let record = codex_launch::load(&uid)?;
        if !deadline_ok(&record) {
            return fail(
                &uid,
                "launch deadline passed before the wrapper was ready",
                CleanupState::Pending,
            );
        }
    }

    // Step 4: bring the wrapper up and commit. `to_ready` itself re-checks,
    // under the lock at commit time, that the deadline still holds and the
    // custodian is still live (Principle A / finding 7).
    match deps.bring_up_wrapper() {
        BringUp::Ready => {
            // Commit `ready` with a **bounded retry** so it PROVES durability
            // before the launcher can consume the visible Ready (round-5 finding
            // 7): a post-rename fsync failure must be retried (to_ready re-fsyncs
            // an already-Ready record for us), not left half-durable — and a
            // stopped lock holder must not hang the commit. A persistent failure
            // is a hard error (the launch does not falsely report Ready).
            commit_ready(&uid, &setup.coordinator)?;
            Ok(CoordinateOutcome::Ready)
        }
        BringUp::Failed(why) => fail(
            &uid,
            &format!("wrapper bring-up failed: {why}"),
            CleanupState::Pending,
        ),
    }
}

/// CAS the record to `failed{cleanup:pending}` and return the sanitized reason.
/// If the record is already `failed` (e.g. the custodian's deadline transition
/// beat us), the first reason stands and this reports it.
///
/// Every coordinator determinate-failure flows through here, so it uses
/// [`codex_launch::fail_new_session_determinate`] to **clear the indeterminate
/// flag** atomically (finding 4): a coordinator that reached a determinate
/// failure — including one before `new-session` ever ran — must not leave the
/// speculative flag set to trap the custodian on a later `Absent`.
fn fail(uid: &str, reason: &str, cleanup: CleanupState) -> Result<CoordinateOutcome> {
    // Bounded, non-blocking terminalization (finding 7): a stopped lock holder
    // must never hang a lost-custodian / deadline / wrapper-failure path. The
    // caller passes `NotRequired` when no session was created (round-4) so the
    // custodian does not probe a nonexistent server forever.
    terminalize_within(uid, reason, cleanup, std::time::Duration::from_secs(5))?;
    // Report the record's ACTUAL reason (first-reason-wins if the custodian's
    // transition beat us); fall back to the reason we tried to write.
    let reported = codex_launch::load(uid)
        .ok()
        .and_then(|r| match r.state {
            LaunchState::Failed { reason } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| reason.to_string());
    Ok(CoordinateOutcome::Failed(reported))
}

// ----------------------------------------------------------------------------
// The launcher side: wait on the record, never mutate.
// ----------------------------------------------------------------------------

/// What the launcher's wait resolved to.
///
/// `LaunchWait`/[`wait_on_record`] are the **launcher** half of D7, consumed by
/// [`crate::codex::launch`]: `codeconnect codex` spawns the coordinator and then
/// only waits here, because the coordinator owns every forward mutation and the
/// record owns the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchWait {
    /// `ready` — the launcher attaches.
    Ready,
    /// `failed` — the launcher prints this sanitized reason and exits non-zero.
    Failed(String),
    /// The launcher gave up displaying before a terminal state (its own patience
    /// ran out). The record still owns the outcome — the coordinator/custodian
    /// continue to drive it; the launcher just stops waiting.
    TimedOut,
}

/// Poll the launch record until it reaches a terminal state or `patience`
/// elapses. A not-yet-created or transiently-unreadable record is treated as
/// "still coming up" (the coordinator writes it as its first act), never as
/// failure — only a durable `failed` fails the wait.
pub fn wait_on_record(
    uid: &str,
    patience: std::time::Duration,
    poll: std::time::Duration,
) -> LaunchWait {
    let deadline = std::time::Instant::now() + patience;
    loop {
        if let Ok(record) = codex_launch::load(uid) {
            match record.state {
                LaunchState::Ready => {
                    // The reader's half of the durability handoff (A9.6a).
                    // `store_atomic` makes the Ready visible at the rename and only
                    // then fsyncs the directory, and `commit_ready` cannot
                    // un-publish a rename whose dir-fsync afterwards failed — so a
                    // bare sighting of Ready is not proof the entry survives power
                    // loss. Re-fsync it here, on the consuming side, before telling
                    // the caller the session exists. A failure is "not proven
                    // durable *yet*", not a failure of the launch: keep polling
                    // (the writer's own bounded retry may still prove it) until
                    // `patience` runs out, which reports TimedOut rather than a
                    // Ready the disk may not have.
                    if codex_launch::prove_record_durable(uid).is_ok() {
                        return LaunchWait::Ready;
                    }
                }
                LaunchState::Failed { reason } => return LaunchWait::Failed(sanitize(&reason)),
                LaunchState::Pending => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            return LaunchWait::TimedOut;
        }
        std::thread::sleep(poll);
    }
}

/// Reduce a failure reason to something safe to print at a terminal: single
/// line, printable, length-bounded. The reasons this crate writes are already
/// benign; this is defense in depth for anything that flows in from a step.
///
/// `pub(crate)` for the host, which quotes a broker refusal into a record reason
/// — material that ultimately came off the wire, and the one input this function's
/// "defense in depth" wording was always about. Bounding it where it is composed
/// rather than only where it is printed keeps the record itself readable too.
pub(crate) fn sanitize(reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    // Truncate on a **character** boundary, not a byte index: `&trimmed[..300]`
    // panics when byte 300 lands inside a multibyte UTF-8 sequence (the "Plus"
    // finding). Taking 300 `chars` is always valid and bounds the rendered
    // length without splitting a code point.
    if trimmed.chars().count() > 300 {
        let cut: String = trimmed.chars().take(300).collect();
        format!("{cut}…")
    } else {
        trimmed.to_string()
    }
}

// ----------------------------------------------------------------------------
// Real deps + the `internal-codex-coordinator` subcommand.
// ----------------------------------------------------------------------------

/// The `/tmp` prefix every coordinator-chosen run dir carries.
///
/// **Why `/tmp` and not `~/.codeconnect/sessions/<uid>/…`** — the durable record
/// lives under the session dir precisely because it must survive cleanup, but the
/// run dir is the opposite kind of thing: three unix-domain sockets, and a unix
/// socket path must be shorter than `SUN_LEN` (104 on macOS). A home-rooted path
/// spends that budget on someone else's name: `$HOME/.codeconnect/sessions/` is 23
/// bytes past `$HOME` before the 26-byte uid, and `CODECONNECT_HOME` is routinely
/// pointed at a deep temp path in tests and by operators. The bind would then fail
/// as a function of how long the user's home path is — a launch that works on one
/// machine and not another for a reason nothing in the error names.
///
/// So the run dir is rooted at a fixed short prefix and sized in code
/// ([`choose_run_dir`], which asserts the worst case rather than trusting this
/// paragraph). The durable record is unaffected: it still lives in the session
/// dir and still outlives the run dir it names.
///
/// **What is given up, stated plainly.** `/tmp` is world-writable and sticky, so
/// the parent chain is not the "0700 dir the caller owns" that
/// [`crate::codex_host`]'s invariant 1 originally described as its premise (that
/// module's doc now records this change rather than contradicting it). The load-
/// bearing protection is therefore **not** secrecy of the name: it is that the
/// host `mkdir`s the directory **exclusively**, so a squatter who did guess the
/// name gets the launch refused (`EEXIST`), never adopted. The failure mode is a
/// denied launch, not a hijacked one, and sticky `/tmp` means another uid cannot
/// remove or replace the directory once it exists.
///
/// The nonce in the leaf is defence in depth on top of that, and it is worth
/// being precise about its strength rather than leaning on it: it is 64 bits of
/// [`codex_launch::mint_nonce`], which is unguessable **when `getentropy`
/// succeeds**. That function has a documented fallback to time+pid, justified
/// there by the record being 0700 — reasoning that does not carry over to a
/// world-writable path. So on that fallback the name becomes predictable, and the
/// exclusive `mkdir` is all that is left. That is why the `mkdir` is named as the
/// protection and the nonce is not.
pub(crate) const RUN_DIR_PREFIX: &str = "/tmp/cch.";

/// How much of the uid goes into the run-dir name, and **which end**.
///
/// The **last** ten characters, not the first. A ULID is a 10-character
/// millisecond timestamp followed by 16 characters of randomness, so a prefix
/// slug renders every session of the same era identically — measured: six
/// distinct uids in one test run all produced `/tmp/cch.01JQXV9K7B.*`, which is
/// not an identity, it is a clock. The tail is the random half, so two live run
/// dirs are told apart at a glance and a directory in `ls /tmp` can be matched
/// back to the uid in its launch record.
const RUN_DIR_UID_CHARS: usize = 10;

/// How much of the launch nonce goes into the run-dir name: 16 hex characters of
/// the 128 bits [`codex_launch::mint_nonce`] produces. This is what makes two
/// launches of the *same* uid choose different directories, which is what lets
/// the host insist the directory not already exist.
const RUN_DIR_NONCE_CHARS: usize = 16;

/// The longest path the host appends to the run dir *that has to bind*, derived
/// from the host's own list rather than restated — so a fourth leg, or a longer
/// name, cannot leave this sizing quietly wrong. `SUN_LEN` constrains sockets
/// alone, so the (longer) log names are correctly absent from that list.
fn longest_host_socket_len() -> usize {
    // +1 for the `/` separator the host's `run_dir.join(..)` adds.
    1 + crate::codex_host::SOCKET_NAMES
        .iter()
        .map(|n| n.len())
        .max()
        .unwrap_or(0)
}

/// How often the bring-up wait looks at the run dir.
const BRINGUP_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// How often the bring-up wait censuses tmux while the sockets are still absent.
/// The filesystem poll is nearly free; a census forks `tmux`, so it runs on a
/// slower cadence — often enough to notice a dead pane in about a second rather
/// than at the deadline, rarely enough not to fork thirty times a second.
const BRINGUP_CENSUS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Choose the fresh run dir for one launch: `/tmp/cch.<uid-prefix>.<nonce-prefix>`.
///
/// The path is **not** created here — the host owns that, with one exclusive
/// `mkdir(0700)`, and refuses to adopt a directory it did not create. This
/// function only picks a name and proves it fits.
///
/// Both components are filtered to ASCII alphanumerics and truncated. The uid is
/// a ULID and the nonce is hex on every real path, so neither step changes
/// anything there; they exist because both arrive from argv, and a value carrying
/// `/` or `..` would otherwise choose a *different* directory than the one this
/// function claims to name — the run dir is later swept by the custodian, so a
/// name that can escape its prefix is the one input worth refusing outright.
///
/// **This mapping is many-to-one, and nothing here pretends otherwise.** Filtering
/// and truncating means distinct `(uid, nonce)` pairs can name the same directory
/// — `a/b` and `ab` collide, as do two nonces sharing their first sixteen
/// characters. Uniqueness is therefore NOT a property of this function, and no
/// caller may treat the name as an identity. What actually makes a run dir safe to
/// use is the host's **exclusive `mkdir`**: whoever gets there first owns it, and
/// a second launch that derived the same name is refused rather than handed a
/// directory in use. Collision is a failed launch, never a shared one.
///
/// Fails closed if the worst-case socket path would reach `SUN_LEN`: a launch
/// whose sockets cannot bind must be refused at the charter, not discovered by
/// the host as an unexplained bind failure.
pub(crate) fn choose_run_dir(uid: &str, launch_nonce: &str) -> Result<std::path::PathBuf> {
    /// The last `take` ASCII-alphanumeric characters, in order.
    fn tail_slug(s: &str, take: usize) -> String {
        let kept: Vec<char> = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        kept[kept.len().saturating_sub(take)..].iter().collect()
    }
    fn head_slug(s: &str, take: usize) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(take)
            .collect()
    }
    let uid_slug = tail_slug(uid, RUN_DIR_UID_CHARS);
    let nonce_slug = head_slug(launch_nonce, RUN_DIR_NONCE_CHARS);
    if uid_slug.is_empty() || nonce_slug.is_empty() {
        anyhow::bail!(
            "cannot name a run dir from uid {uid:?} and nonce {launch_nonce:?}: \
             both must contain at least one alphanumeric character"
        );
    }
    let path = format!("{RUN_DIR_PREFIX}{uid_slug}.{nonce_slug}");
    // The assertion the placement decision above rests on, checked rather than
    // asserted in prose: the longest socket the host will bind under this dir.
    let worst = path.len() + longest_host_socket_len();
    if worst >= crate::codex_host::SUN_LEN_LIMIT {
        anyhow::bail!(
            "the chosen run dir {path} would put a {worst}-byte socket path under it, \
             and a unix socket path must be < {} (SUN_LEN)",
            crate::codex_host::SUN_LEN_LIMIT
        );
    }
    Ok(std::path::PathBuf::from(path))
}

/// The real forward-launch operations, backed by the exec gate + tmux.
pub struct RealCoordinatorDeps {
    pub uid: String,
    pub session_name: String,
    pub cwd: String,
    pub tmux_socket: String,
    /// This `codeconnect` binary. It is both the exec-gate program the custodian
    /// is launched through and the `internal-codex-host` the pane runs — one
    /// executable wearing two hats, so it is resolved once
    /// (`std::env::current_exe`) and fails the launch closed when it cannot be.
    pub self_exe: std::path::PathBuf,
    pub custodian_nonce: String,
    /// The launch nonce, carried into the pane so the host can prove to the D7
    /// gate that it is the host THIS launch invited.
    pub launch_nonce: String,
    pub coordinator: ProcessIdentity,
    /// The resolved, version-pinned `codex` executable the host execs for BOTH the
    /// app-server and the TUI. The coordinator does not re-resolve it: resolution
    /// and the version pin are the launcher's (`codex::start`), and passing the
    /// single canonicalised path through is what keeps the recorded, checked and
    /// executed binaries the same file.
    pub codex: String,
    /// The SHA-256 of that executable as the launcher inspected it (A7.1), carried
    /// to the host as `--codex-sha256` and re-verified there before each exec.
    ///
    /// **Carried, never computed here.** A path is not a file, so a digest is only
    /// worth anything if it comes from the process that did the inspecting: hashing
    /// `codex` in this process would pin whatever is at that name now and would say
    /// nothing about the bytes that passed the native-executable check and reported
    /// `--version`. The coordinator is the courier for the same reason it is the
    /// courier for `launch_cwd` — the authority upstream resolved it once, and a
    /// second, later derivation is a second answer that can disagree.
    ///
    /// **Not verified here either**, and that is deliberate rather than an omission.
    /// This process never execs codex; the only check that can bound an exec is the
    /// one taken immediately before it, which the host does twice. A verify here
    /// would be a third answer about a third moment, and its passing would tempt a
    /// reader into believing the launch was bound when the binding that matters
    /// still lives entirely in the host.
    pub codex_sha256: String,
    /// The isolated `CODEX_HOME` for this session.
    pub codex_home: String,
    /// The four launch-policy dimensions the broker enforces, carried verbatim to
    /// the host. The coordinator applies no default to any of them, for the same
    /// reason the host does not: a default here is a silent disagreement with
    /// whatever the record says was enforced.
    pub approval_policy: String,
    pub approvals_reviewer: String,
    pub sandbox: String,
    pub hooks_enabled: bool,
    /// The FIFTH launch-policy dimension: the workspace this session is launched in,
    /// **already canonicalized**, carried to the host as `--launch-cwd` and from there into
    /// the broker's `LaunchFingerprint`.
    ///
    /// It is the workspace anchor the client cannot choose. Everything else the broker can
    /// see about a workspace is client-supplied — `thread/start`'s `cwd` comes from the
    /// TUI, and the creation response's `cwd` is the app-server echoing that ask back — so
    /// without this a client could name any directory and the thread binding would follow
    /// it there.
    ///
    /// ## Canonicalization happens HERE, exactly once
    ///
    /// Measured: with `--cwd /tmp` the app-server reports the resolved `/private/tmp`
    /// (macOS `/tmp` is a symlink). Exact equality of those two strings is FALSE; `realpath`
    /// equality is TRUE. The coordinator is the authority that owns the launch cwd, so it
    /// resolves the path ONCE, before it enters the argv. The broker then does pure exact
    /// string equality and needs no filesystem access at all — deliberately, since it
    /// compares paths a client controls. Do not add a normalizer downstream.
    pub launch_cwd: String,
    /// The user's vetted TUI passthrough, appended after `--`.
    pub tui_args: Vec<String>,
    /// The run dir this launch's host will own ([`choose_run_dir`]).
    pub run_dir: std::path::PathBuf,
    /// Absolute `CLOCK_MONOTONIC` nanoseconds past which the launch is expired —
    /// the same deadline the record carries, so the bring-up wait is bounded by
    /// the launch's own budget rather than a second, disagreeing one.
    pub deadline_monotonic_nanos: u64,
    /// The resolved session `new_session` created, kept so `bring_up_wrapper` can
    /// census the pane bound to **this exact** server rather than to whatever
    /// currently answers the socket.
    pub session_a: Option<OwnedSession>,
    /// Test-only (Principle B): hang **inside** `new_session`, after the tmux
    /// session is created and resolved but **before returning** — so the
    /// integration test can SIGKILL the coordinator while it is literally still
    /// in the new-session call, with `new_session_indeterminate` durably set. A
    /// coordinator killed "with tmux in flight" must leave the custodian armed to
    /// clean the (late) session. Never set on a real launch.
    pub hang_in_new_session: bool,
    /// Test-only: hang **inside** `bring_up_wrapper`, before any evidence is
    /// looked at, so the kill-at-boundary tests can hold the coordinator after
    /// `new-session` and SIGKILL it there. It is the exact counterpart of
    /// `hang_in_new_session` and, like it, never set on a real launch.
    ///
    /// Note what is deliberately absent: there is no injection that *reports*
    /// readiness. A bring-up that has not observed the host's evidence has no way
    /// to say `Ready` — not in a test, not behind a flag, not by default — because
    /// a switch that fakes readiness is a switch that can be left on.
    pub hang_in_bringup: bool,
}

impl RealCoordinatorDeps {
    fn server_args(&self) -> Vec<String> {
        if self.tmux_socket.contains('/') {
            vec!["-S".into(), self.tmux_socket.clone()]
        } else {
            vec!["-L".into(), self.tmux_socket.clone()]
        }
    }

    /// Write down a session this launch is KNOWN to have created.
    ///
    /// The difference between a custodian that can bind cleanup to a proven server
    /// identity and one that cannot bind it to anything — a record with no A is the
    /// shape that stays armed until the next reboot.
    fn persist_created_session(
        &self,
        resolved: &protocol::tmux::OwnedSession,
        remain: bool,
    ) -> Result<()> {
        codex_launch::ServerA::from_owned(resolved).and_then(|a| {
            codex_launch::LaunchLock::acquire_bounded(&self.uid, std::time::Duration::from_secs(5))
                .and_then(|lock| {
                    codex_launch::record_new_session_created(&lock, &self.uid, a, remain)
                })
        })
    }

    /// Whether the record on disk NOW names `resolved` as server A with the
    /// indeterminate flag cleared — i.e. whether [`persist_created_session`]'s
    /// `store_atomic` got as far as its rename (round-4 finding 2).
    ///
    /// [`persist_created_session`]: RealCoordinatorDeps::persist_created_session
    ///
    /// Deliberately compares the whole [`codex_launch::ServerA`], not merely
    /// `is_some()`: a record can carry an A from some earlier write, and "an A is
    /// present" would then read a stale pin as proof that *this* session was
    /// published. Only the identity we just tried to write counts.
    ///
    /// Every way of not knowing answers `false`. The record cannot be read, cannot
    /// be parsed, carries no A, carries a different A, or still carries the
    /// indeterminate flag — none of those is evidence the rename landed, and the
    /// caller's fail-closed direction is `Indeterminate`.
    fn published_server_a(&self, resolved: &protocol::tmux::OwnedSession) -> bool {
        let Ok(expected) = codex_launch::ServerA::from_owned(resolved) else {
            return false;
        };
        match codex_launch::load(&self.uid) {
            Ok(rec) => rec.server_a.as_ref() == Some(&expected) && !rec.new_session_indeterminate,
            Err(_) => false,
        }
    }

    /// Everything between "tmux created a session and we resolved it" and the
    /// outcome the state machine acts on.
    ///
    /// Split out from [`RealCoordinatorDeps::new_session`] so the steps that can
    /// fail here are reachable from a test **through this same code**, rather than
    /// only from a live tmux that happens to refuse an assertion (round-3 finding 6,
    /// and the coverage half of finding 4). `new_session` supplies the resolved
    /// session and nothing else; every decision below is made here.
    ///
    /// **Server A is persisted FIRST, before anything else can fail** (round-3
    /// finding 5). It used to be written only by the two failure arms below and by
    /// the caller's `Created` arm, which left a window — resolved session, no A on
    /// disk — whose only escape was an inference the custodian should never have had
    /// to make: "no A recorded, but the host is proven dead, and a pane dies with its
    /// command, so the session must be gone". That inference rests on
    /// `remain_on_exit_asserted` being a LASTING property, which it is not: the
    /// assertion covers the targeted window and pane at one moment, config hooks can
    /// add panes afterwards, and the options stay mutable. Persisting A at the
    /// earliest moment there is an A to persist deletes the window, and with it the
    /// need for the inference at all.
    fn finish_new_session(&mut self, resolved: OwnedSession) -> NewSessionOutcome {
        if let Err(why) = self.persist_created_session(&resolved, false) {
            eprintln!(
                "codeconnect: created and resolved {} but could not persist server A \
                 ({why:#}); the custodian owns the outcome",
                resolved.session_id
            );
            // **An `Err` from the persist does not mean nothing was published**
            // (round-4 finding 2). `store_atomic`'s contract splits the write in
            // two: the RENAME makes the successor visible, and the directory
            // `fsync` after it makes that entry durable. A failure of the second
            // returns `Err` for a record every reader can nonetheless already see —
            // `{server_a: Some(A), new_session_indeterminate: false}`, on disk, now.
            //
            // Mapping that to `Indeterminate` was the same re-arming defect
            // `CreatedThenFailed` was introduced to close, one layer down:
            // `after_pending`'s indeterminate arm calls
            // `fail_new_session_indeterminate`, which durably sets the flag back to
            // **true**, undoing the very fact the rename had just published. The
            // record then says "a late session may still appear" about a session
            // that has appeared, been resolved, and had its identity written down —
            // and `late_session_still_possible` refuses every absence the custodian
            // observes until the next reboot.
            //
            // So the mapping is made to respect what was PUBLISHED rather than what
            // was RETURNED: re-read the record and ask whether the rename landed.
            // The read needs no lock — it is exactly the atomic post-image the
            // rename installed — and it fails closed in both directions it can fail:
            // an unreadable or unparsable record, or one that does not name this
            // session, is not proof of publication, so it stays `Indeterminate`.
            return match self.published_server_a(&resolved) {
                true => NewSessionOutcome::CreatedThenFailed(format!(
                    "server A for {} was published but its write did not complete: {why:#}",
                    resolved.session_id
                )),
                // Nothing reached the disk — the genuinely INDETERMINATE outcome.
                // A session exists but its identity did not, so the flag
                // `mark_new_session_starting` already set must STAY set: an
                // unpinned created session is exactly what it is for.
                false => NewSessionOutcome::Indeterminate,
            };
        }
        // A11.3: make the premise cleanup rests on TRUE, rather than assuming it.
        //
        // A user's own `~/.tmux.conf` can set `remain-on-exit on` — no bug of ours
        // required — and then a pane, and its session, outlive the command. Asserted
        // here against the birth-pinned handle, which is what binds the mutation to
        // the server this session was actually created on (finding 4).
        //
        // Not fatal-with-no-cleanup: the session exists either way, so the outcome is
        // `CreatedThenFailed` — determinate, pinned, and owed cleanup.
        if let Err(why) = self.assert_remain_on_exit(&resolved) {
            eprintln!(
                "codeconnect: could not clear remain-on-exit on {} ({why}); \
                 handing the launch to the custodian",
                resolved.session_id
            );
            return NewSessionOutcome::CreatedThenFailed(format!(
                "remain-on-exit could not be cleared on {}: {why}",
                resolved.session_id
            ));
        }
        // …and write down that it held. **History about one assertion**, not a
        // standing guarantee — see [`codex_launch::LaunchRecord::remain_on_exit_asserted`]
        // for what it may and may not be read as. Nothing gates on it any more, so a
        // failure to record it costs only that observation.
        if let Err(why) = self.note_remain_asserted() {
            eprintln!(
                "codeconnect: cleared remain-on-exit on {} but could not record it \
                 ({why:#}); handing the launch to the custodian",
                resolved.session_id
            );
            return NewSessionOutcome::CreatedThenFailed(format!(
                "the remain-on-exit assertion on {} could not be recorded: {why:#}",
                resolved.session_id
            ));
        }
        // Keep the pin: the bring-up census below binds to THIS server, not to
        // whatever happens to answer the socket a few seconds from now.
        self.session_a = Some(resolved.clone());
        NewSessionOutcome::Created(Box::new(resolved))
    }

    /// The epoch-pinned `remain-on-exit off` assertion, behind a test seam.
    fn assert_remain_on_exit(&self, resolved: &OwnedSession) -> std::result::Result<(), String> {
        #[cfg(test)]
        if let Some(why) = take_assert_fault() {
            return Err(why);
        }
        protocol::tmux::assert_remain_on_exit_off(&self.tmux_socket, resolved)
    }

    /// The durable note that the assertion held, behind a test seam.
    fn note_remain_asserted(&self) -> Result<()> {
        #[cfg(test)]
        if take_note_fault() {
            anyhow::bail!("injected fault: the remain-on-exit note could not be written");
        }
        codex_launch::LaunchLock::acquire_bounded(&self.uid, std::time::Duration::from_secs(5))
            .and_then(|lock| codex_launch::note_remain_on_exit_asserted(&lock, &self.uid))
    }

    /// The full `tmux new-session` argv, including the pane command.
    ///
    /// Split out as a pure function of `self` so the exact argv — the one thing
    /// that decides what actually runs in the pane — is asserted in a unit test
    /// rather than inferred from a live session.
    fn new_session_argv(&self) -> Vec<String> {
        let mut argv = self.server_args();
        argv.extend(
            [
                "new-session",
                "-d",
                "-s",
                &self.session_name,
                "-c",
                &self.cwd,
                "-e",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        argv.push(format!("{}={}", protocol::ENV_SESSION_UID, self.uid));
        // Pin the launch-record root into the pane as well.
        //
        // The host now reads the launch record to present itself to the D7 gate,
        // so it and this coordinator have to agree on WHERE that record lives —
        // and without this they need not. A pane inherits the **tmux server's**
        // environment, and the server was started by whichever client happened to
        // reach it first, which may be a process with a different (or absent)
        // `CODECONNECT_HOME`. The uid stamp beside it has been passed this way all
        // along; this is the same idea applied to the other thing the pane must
        // not have to guess.
        //
        // In production both resolve to `~/.codeconnect` and this changes nothing.
        // It is load-bearing exactly where the override is in play — tests, and any
        // operator running against a non-default root.
        argv.push("-e".into());
        argv.push(format!(
            "CODECONNECT_HOME={}",
            protocol::root_dir().display()
        ));
        // The frame tee, FORWARDED and never originated.
        //
        // `tmux new-session` gives the pane an explicit `-e` allowlist rather than this
        // process's whole environment, so without this the measurement instrument
        // (`codex_broker::frame_tee`) can never reach the host that builds the broker —
        // which made it unusable for the live harnesses it exists to serve.
        //
        // Forwarded only when it is ALREADY set here: this is a pass-through, not a
        // switch. The shipping launcher never sets it and offers no way to — no config
        // key, no charter flag, no CLI option — which is what
        // `the_shipping_launcher_cannot_enable_the_frame_tee` proves.
        //
        // Forwarding it is safe even from a hostile parent environment, and that is a
        // COMPILE-TIME fact rather than an argument about who sets variables: the host at
        // the other end only reads this one if it was built with the `frame-tee` feature,
        // which the shipping build is not. A capture build gets a capture; the binary a
        // user runs gets nothing from it.
        if let Ok(path) = std::env::var(codex_broker::FRAME_TEE_ENV) {
            argv.push("-e".into());
            argv.push(format!("{}={path}", codex_broker::FRAME_TEE_ENV));
        }
        argv.push("--".into());
        argv.extend(self.host_argv());
        argv
    }

    /// The pane command: this binary re-invoked as `internal-codex-host`.
    ///
    /// Every flag the host requires is passed explicitly — it applies no default
    /// to any of them — and the user's vetted passthrough follows a bare `--`,
    /// which is the boundary the host parses back.
    fn host_argv(&self) -> Vec<String> {
        let mut argv = vec![
            self.self_exe.to_string_lossy().into_owned(),
            "internal-codex-host".into(),
            // The launch identity the host presents to the D7 gate before it
            // creates anything. Without these a pane started late by a frozen
            // tmux server could not tell that its launch had already failed.
            "--uid".into(),
            self.uid.clone(),
            "--nonce".into(),
            self.launch_nonce.clone(),
            "--tmux-socket".into(),
            self.tmux_socket.clone(),
            "--codex".into(),
            self.codex.clone(),
            // A7.1: the path's identity travels beside the path. The host refuses
            // without it rather than falling back to trusting the name.
            "--codex-sha256".into(),
            self.codex_sha256.clone(),
            "--run-dir".into(),
            self.run_dir.to_string_lossy().into_owned(),
            "--codex-home".into(),
            self.codex_home.clone(),
            "--approval-policy".into(),
            self.approval_policy.clone(),
            "--approvals-reviewer".into(),
            self.approvals_reviewer.clone(),
            "--sandbox".into(),
            self.sandbox.clone(),
            "--hooks-enabled".into(),
            self.hooks_enabled.to_string(),
            // The fifth fingerprint dimension (round-2 P4), plumbed exactly like the four
            // above. Already canonicalized — see `launch_cwd`.
            "--launch-cwd".into(),
            self.launch_cwd.clone(),
        ];
        if !self.tui_args.is_empty() {
            argv.push("--".into());
            argv.extend(self.tui_args.iter().cloned());
        }
        argv
    }
}

/// What one bring-up poll saw. Plain data, so the decision it feeds is a pure
/// function ([`bringup_step`]) that can be exercised without a filesystem or a
/// live tmux server.
struct BringupObservation {
    /// BOTH broker legs (`tui.sock`, `ccd.sock`) **accepted a connection** just
    /// now. Never "one of them": a half-bound broker is not a boundary.
    sockets_bound: bool,
    /// The host that took this launch's lease is recorded and proven live, AND
    /// both of its children are recorded with a proven identity.
    ///
    /// One field because they are one question — "is there a host here that
    /// cleanup can act on?" — and neither half is worth committing without the
    /// other: a live host whose children nobody can name is precisely the session
    /// that leaks when it dies.
    host_live: bool,
    /// The run dir itself exists — i.e. the host got as far as creating it.
    run_dir_present: bool,
    /// The pane's session liveness, when it was censused this pass. `None` means
    /// *not looked at*, which is not the same as "unknown" and must never be read
    /// as evidence either way.
    session: Option<protocol::tmux::OwnedLiveness>,
}

/// One poll's verdict.
enum BringupStep {
    Ready,
    Failed(String),
    KeepWaiting,
}

/// Decide a bring-up poll, fail-closed in both directions.
///
///   * `Ready` requires **both** facts positively: the legs are bound AND the
///     census proved the session is still ours. An `Unknown` census is a reason to
///     keep waiting, never a reason to commit — the coordinator is about to write
///     a durable `ready` that a launcher will attach to.
///   * `Failed` is only returned on a **proven** loss: `Gone` is the 2c census's
///     positive absence (a successful listing without our uid, or a server
///     identity that no longer matches A), not a probe that failed to answer.
///     Everything else waits for the deadline, which the caller owns.
fn bringup_step(obs: &BringupObservation) -> BringupStep {
    use protocol::tmux::OwnedLiveness;
    // A live HOST is a precondition of readiness, not one of the racing facts.
    // The sockets and the pane can both look right while the process that owns
    // them is gone: a unix socket inode outlives an abnormal exit, and the tmux
    // session outlives its pane command by however long tmux takes to notice. The
    // lease is the one piece of evidence that names the process itself.
    if obs.sockets_bound && !obs.host_live {
        return BringupStep::KeepWaiting;
    }
    match (&obs.session, obs.sockets_bound) {
        (Some(OwnedLiveness::Live), true) => BringupStep::Ready,
        (Some(OwnedLiveness::Gone), true) => BringupStep::Failed(
            "both broker sockets were bound under the run dir, but the pane's tmux session \
             is gone — the wrapper came up and its pane did not survive"
                .into(),
        ),
        (Some(OwnedLiveness::Gone), false) => BringupStep::Failed(format!(
            "the pane's tmux session is gone and the wrapper never bound its broker \
             sockets (run dir {}) — the host died before it was up",
            if obs.run_dir_present {
                "created but empty"
            } else {
                "never created"
            }
        )),
        // Live-but-not-yet-bound, an unprovable census, and a pass that did not
        // census at all all mean the same thing: no verdict yet.
        _ => BringupStep::KeepWaiting,
    }
}

/// After the pane is proven up, wait a bounded while for it to prove a **session**
/// is up: a thread bound in the broker, or a failure the host recorded instead.
///
/// # Why `Ready` was not enough, measured
///
/// Everything [`bringup_step`] checks — both broker legs serving under a run dir
/// this uid owns, a live host lease, both children past `execve` and alive, the
/// tmux session `Live` — is true of a pane that is about to die. From a directory
/// the owner's `~/.codex` marks `trust_level = "trusted"`, the TUI's first
/// `thread/start` is refused by the launch fingerprint and codex 0.153 exits
/// immediately; for the whole ~2 s before it does, every one of those facts holds.
/// So `Ready` committed, `codeconnect codex` `exec`ed into `tmux attach-session`,
/// and the user got a pane that flickered to `[exited]` with nothing said anywhere
/// they could see it. The legs prove the HOST came up. Only a bound thread proves
/// the session did, and that is what the launcher is actually waiting for.
///
/// # The three ways out, and why only one of them is a verdict
///
///   * **A bound thread** — [`codex_launch::LaunchRecord::codex_thread_bound`],
///     written by the host from the broker's own verified binding. Measured at
///     ~225 ms past leg attach on a healthy launch, which is what this costs.
///   * **A recorded failure** — the host's `pending → failed`, written the moment
///     its TUI dies without a thread. Reported as [`BringUp::Failed`], whose reason
///     [`fail`] then reads back out of the record first-reason-wins, so what
///     reaches the user's terminal is the host's sentence and not a wrapper around
///     it.
///   * **The grace, or the launch deadline, running out** — [`BringUp::Ready`],
///     deliberately. Expiry means no evidence either way, and turning "we did not
///     hear" into a failed launch would break every slow-but-healthy launch to
///     catch a fast broken one. That case attaches exactly as it did before this
///     function existed; `broker.log` now survives the run dir, and `ccd` files a
///     `session_end` reason for it, so it is no longer silent either.
fn await_thread_binding(uid: &str, deadline_monotonic_nanos: u64) -> BringUp {
    let grace_expires = std::time::Instant::now() + codex_launch::THREAD_BINDING_GRACE;
    loop {
        // Evidence before the clock, on every pass, for the same reason
        // `bring_up_wrapper`'s own loop checks it in that order: a binding that
        // lands in the instant the grace does should be read, not discarded.
        if let Ok(record) = codex_launch::load(uid) {
            // **`Failed` is read BEFORE the binding bit, and the order is the rule.**
            // `note_codex_thread_bound` is history, not state: it is written with no
            // `pending` guard and is never cleared, so `Failed { .. }` and
            // `codex_thread_bound: true` are reachable together — a broker or
            // app-server that dies after a thread bound but before the launch commits
            // is exactly that shape. Reading the bit first would answer `Ready` for a
            // launch whose failure had already been recorded, and the launcher would
            // `exec` into a pane that is on its way out: the very defect this whole
            // wait exists to remove, reintroduced one layer up. A recorded failure is
            // a verdict; a binding is only evidence that one part of bring-up got
            // somewhere.
            if let LaunchState::Failed { reason } = record.state {
                return BringUp::Failed(reason);
            }
            if record.codex_thread_bound {
                return BringUp::Ready;
            }
        }
        if std::time::Instant::now() >= grace_expires {
            return BringUp::Ready;
        }
        // The launch deadline still dominates: this grace can only spend budget the
        // coordinator already had. An unreadable clock ends the wait — but with
        // `Ready`, not the `Failed` its counterpart in `bring_up_wrapper` returns,
        // because by this point readiness is already proven and the only thing that
        // cannot be bounded is the extra look.
        match protocol::proc_identity::monotonic_now_nanos() {
            Some(now) if now < deadline_monotonic_nanos => {}
            _ => return BringUp::Ready,
        }
        std::thread::sleep(BRINGUP_POLL);
    }
}

/// Whether the run dir is one **this uid** owns, private (0700), and a real
/// directory rather than a symlink to one.
///
/// This is the coordinator's own check on the premise the host's readiness
/// argument rests on. `symlink_metadata` (not `metadata`) so a link planted at
/// the name is judged as a link and rejected, rather than followed to whatever it
/// points at.
///
/// Stated at its real strength, which is the same strength the host claims for
/// its `mkdir`: this excludes **other uids**, not a hostile process running as
/// this one. A same-uid attacker can already replace the binaries involved, so no
/// filesystem check here is a boundary against it.
fn run_dir_is_ours(path: &std::path::Path, uid: &str, launch_nonce: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let shape_ok = match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            meta.is_dir()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.permissions().mode() & 0o777 == 0o700
        }
        Err(_) => false,
    };
    // Shape is not identity. The run-dir NAME is a many-to-one derivation, so a
    // correctly-shaped 0700 directory at the expected path can belong to a
    // different launch — and its sockets would then be accepted as evidence for
    // THIS one. Combined with a host that is merely paused between taking its
    // lease and creating its own directory, that is a false `ready`: host A's
    // serving legs under host B's lease. The marker is what makes the directory
    // say which launch it belongs to.
    // Only a marker that was READ and names this launch counts. `Unknown` — an
    // unreadable or symlinked marker — is not evidence in either direction, so it
    // simply is not readiness; the bring-up loop looks again next pass.
    shape_ok
        && codex_launch::run_dir_marker(path, uid, launch_nonce)
            == codex_launch::MarkerVerdict::Ours
}

/// How long a single connect probe may take. A leg that cannot accept inside this
/// is not counted as serving on that pass; the bring-up loop simply looks again.
const LEG_CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// The broker's ccd leg under a run dir.
///
/// Named once because two places depend on it being the same path: the bring-up
/// proves this socket is *serving* before `Ready`, and the registration hands
/// this socket to the daemon as the session's control link. A drift between them
/// is a session the daemon is told to observe on a path nothing listens to.
fn ccd_leg(run_dir: &std::path::Path) -> std::path::PathBuf {
    run_dir.join(crate::codex_host::SOCKET_NAMES[2])
}

/// Whether a broker leg is **serving**: it is a unix-domain socket AND a
/// connection attempt reaches a listener within [`LEG_CONNECT_BUDGET`].
///
/// The file-type check alone is not evidence and this is the difference that
/// matters: a socket inode is an ordinary directory entry that outlives the
/// process that bound it. A host killed abnormally leaves both legs sitting on
/// disk looking exactly like a healthy broker, and readiness built on `stat`
/// would commit a durable `ready` for a session with nothing behind it.
///
/// **The connect is bounded, and that is not a detail.** `UnixStream::connect` is
/// blocking, and AF_UNIX `connect` blocks when the listener's backlog is full and
/// nothing is accepting — a broker whose accept loop has stalled is exactly the
/// sick-but-present case readiness has to survive. A blocking probe there would
/// sit inside a single poll iteration past the launch deadline, defeating the one
/// property the bring-up loop exists to have. So the socket is created
/// non-blocking and the wait is `poll(2)` with an explicit timeout.
///
/// What each outcome means:
///   * connect succeeds ⇒ a listener accepted us. Serving.
///   * `EAGAIN` ⇒ the backlog is full, which only a bound listening socket can
///     report. Serving. **Measured note:** this does not occur for AF_UNIX on
///     macOS — see below — but it is the documented meaning where it does, and
///     the arm is kept rather than pruned to a platform.
///   * `ECONNREFUSED` / `ENOENT` ⇒ not serving.
///   * anything else, including the timeout ⇒ not proven, so not serving.
///
/// **`ECONNREFUSED` is ambiguous on this platform, and the ambiguity is safe.**
/// Measured on macOS: a listener whose backlog is saturated refuses further
/// connects with `ECONNREFUSED` (61) — the *same* errno as an unbound socket, and
/// `EAGAIN` (35) is never seen. So a live-but-stalled broker is indistinguishable
/// here from an abandoned inode, and both read as not-serving.
///
/// That is the harmless direction. Readiness only ever *withholds* on it: the
/// bring-up loop looks again next pass, and a broker that cannot accept a
/// connection is not one this launch should be declared ready on. The direction
/// that would matter — an abandoned inode read as serving, committing a durable
/// `ready` for a dead host — is exactly what the connect exists to prevent, and no
/// errno makes that happen.
///
/// The connection is closed immediately; the broker sees a client that hung up
/// before the WS handshake, which is exactly what it is.
fn leg_is_serving(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    // Check the type first so a regular file or FIFO at the name is rejected
    // without any connect attempt.
    let is_socket = std::fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false);
    if !is_socket {
        return false;
    }
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    // `sun_path` must hold the path AND its NUL terminator.
    if bytes.len() >= std::mem::size_of_val(&addr.sun_path) {
        return false;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    // SAFETY: a plain socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return false;
    }
    // Owned from here: every return below goes through `close`.
    let serving = (|| {
        // SAFETY: `fd` is a socket we just created and still own.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return false;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        // SAFETY: `addr` is a fully initialised sockaddr_un and `fd` is ours.
        let rc = unsafe { libc::connect(fd, std::ptr::addr_of!(addr).cast(), len) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // The backlog is full: only a listening socket answers this way.
            // (`EWOULDBLOCK` is the same value as `EAGAIN` on this platform, so
            // one arm covers both spellings.)
            Some(libc::EAGAIN) => return true,
            // The one case that needs waiting.
            Some(libc::EINPROGRESS) => {}
            // ECONNREFUSED, ENOENT and everything else: not proven serving.
            _ => return false,
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let timeout = LEG_CONNECT_BUDGET.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: one pollfd, owned fd, explicit timeout.
        let polled = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if polled <= 0 {
            // 0 is the timeout — a leg that did not answer in the budget is not
            // proven serving this pass, which is the fail-closed reading.
            return false;
        }
        // Writability alone is not success: the error is retrieved explicitly.
        let mut so_error: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `so_error`/`len` are correctly sized for SO_ERROR.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                std::ptr::addr_of_mut!(so_error).cast(),
                &mut len,
            )
        };
        rc == 0 && so_error == 0
    })();
    // SAFETY: `fd` is ours and not used again.
    unsafe { libc::close(fd) };
    serving
}

/// Whether the launch record names a host whose process is **proven live**.
///
/// Read from the record rather than passed in, because the host writes it: the
/// lease is CAS'd by the host process itself at the D7 gate, so it names the
/// process actually running in the pane. `Unknown` liveness is not proof and does
/// not commit — the same fail-closed rule the custodian applies.
fn host_lease_is_live(uid: &str) -> bool {
    match codex_launch::load(uid) {
        Ok(record) => {
            let leased = match &record.host_lease {
                Some(lease) => liveness(&lease.identity) == Liveness::Alive,
                None => false,
            };
            // Both children too, and as ONE question rather than two (round-3
            // finding 1). "Recorded by this lease, past `execve`, and alive now" has
            // to hold of a SINGLE entry per role: asked as two separate existential
            // searches over the same list, a retained predecessor's still-breathing
            // TUI could supply the liveness while the current host's confirmed TUI
            // was already dead. [`codex_launch::host_children_ready`] carries the
            // argument for each conjunct.
            leased && codex_launch::host_children_ready(&record)
        }
        Err(_) => false,
    }
}

impl CoordinatorDeps for RealCoordinatorDeps {
    fn spawn_custodian(&mut self, _uid: &str) -> Result<ProcessIdentity> {
        // Bring the custodian up inertly through the D6 gate; its identity is
        // CAS'd into the record (atomic with the child entry) before it is
        // released to run. Shared with the recovery sweep's rearm. (`_uid` equals
        // `self.uid`; the real arm reads it from `self`.)
        crate::codex_custodian::spawn_custodian_through_gate(
            &self.uid,
            &self.tmux_socket,
            self.coordinator,
            self.self_exe.clone(),
            &self.custodian_nonce,
        )
    }

    fn run_dir(&self) -> Option<&str> {
        self.run_dir.to_str()
    }

    fn new_session(&mut self) -> NewSessionOutcome {
        let Some(bin) = protocol::tmux::tmux_bin() else {
            return NewSessionOutcome::Failed("tmux not found".into());
        };
        let argv = self.new_session_argv();
        match protocol::proc::run_deadlined(
            std::process::Command::new(&bin)
                .args(&argv)
                .stdin(std::process::Stdio::null()),
            std::time::Duration::from_secs(5),
        ) {
            Ok(protocol::proc::RunOutcome::Completed { status, stderr, .. }) => {
                if !status.success() {
                    return NewSessionOutcome::Failed(
                        String::from_utf8_lossy(&stderr).trim().to_string(),
                    );
                }
            }
            // A timed-out mutation is indeterminate (tmux.rs:42), never a clean
            // failure — hand to the custodian.
            Ok(protocol::proc::RunOutcome::TimedOut { .. }) => {
                return NewSessionOutcome::Indeterminate
            }
            Err(err) => return NewSessionOutcome::Failed(format!("could not run tmux: {err}")),
        }
        // Test-only Principle B window, and it has to be HERE (round-3 finding 5).
        //
        // "tmux in flight" means: the session exists and the record does not yet know
        // which one it is — `new_session_indeterminate` still set, `server_a` still
        // null. That used to be true all the way to the end of this function, so the
        // hang could sit at the bottom. It no longer is: `finish_new_session` persists
        // A as its first act, which both clears the flag and pins the session. A hang
        // placed after that stages a record that is neither in flight nor unpinned —
        // the opposite of what this seam exists to produce.
        if self.hang_in_new_session {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        // Resolve the created session by uid to pin it.
        let resolved = match protocol::tmux::resolve_owned_session(&self.tmux_socket, &self.uid) {
            Ok(session) => session,
            // The session was created (tmux said success) but we cannot resolve
            // it: treat as indeterminate so the custodian owns the outcome.
            Err(_) => return NewSessionOutcome::Indeterminate,
        };
        self.finish_new_session(resolved)
    }

    /// Wait for the pane's host to prove itself up, or fail with a precise reason.
    ///
    /// The two facts and why neither is inferred from the other are in the module
    /// doc. The loop's shape is the part worth stating here: evidence is checked
    /// **before** the deadline on every pass, so a bring-up that completes in the
    /// same instant the deadline lands is committed rather than discarded; and an
    /// unreadable monotonic clock ends the wait immediately, because a wait that
    /// cannot be bounded is not a bounded wait.
    fn bring_up_wrapper(&mut self) -> BringUp {
        if self.hang_in_bringup {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        let Some(pin) = self.session_a.clone() else {
            return BringUp::Failed(
                "no resolved tmux session to bind the bring-up evidence to; refusing to \
                 judge readiness unpinned"
                    .into(),
            );
        };
        let tui_sock = self.run_dir.join("tui.sock");
        let ccd_sock = ccd_leg(&self.run_dir);
        let mut last_census: Option<std::time::Instant> = None;
        let mut censused_while_bound = false;
        loop {
            // Sockets count as evidence only under a directory that is OURS.
            //
            // The readiness argument is "a socket appeared here, so our host bound
            // it", and that only follows because the host `mkdir`s the directory
            // exclusively as 0700. The coordinator never witnesses that `mkdir`,
            // though — it sees a path — so it checks the property directly rather
            // than inheriting it by assumption: owned by this uid, and 0700. A
            // pre-existing directory with planted sockets makes the host abort with
            // `EEXIST`, and without this check the coordinator's evidence would
            // read `bound` with only the census standing between that and a false
            // `Ready`.
            let run_dir_present = self.run_dir.exists();
            let sockets_bound = run_dir_is_ours(&self.run_dir, &self.uid, &self.launch_nonce)
                && leg_is_serving(&tui_sock)
                && leg_is_serving(&ccd_sock);
            // Only asked once the legs answer, so the common waiting case still
            // costs one `stat` per poll and no record read.
            let host_live = sockets_bound && host_lease_is_live(&self.uid);
            // Census when it can change the verdict, or when the slow cadence is
            // due. "Can change the verdict" means the legs have just become bound:
            // a proven-live pane then commits, so that pass censuses immediately
            // rather than waiting out the cadence. It deliberately does NOT mean
            // "every pass while bound" — if the census keeps answering `Unknown`,
            // that would fork `tmux` ten times a second for the rest of the
            // deadline. After the first look, the cadence governs again.
            let newly_bound = sockets_bound && !censused_while_bound;
            let census_due = last_census
                .map(|at| at.elapsed() >= BRINGUP_CENSUS_INTERVAL)
                .unwrap_or(true);
            if sockets_bound {
                censused_while_bound = true;
            }
            let session = if newly_bound || census_due {
                last_census = Some(std::time::Instant::now());
                Some(protocol::tmux::owned_liveness(
                    &self.tmux_socket,
                    &self.uid,
                    Some(&pin),
                ))
            } else {
                None
            };
            let observed = BringupObservation {
                sockets_bound,
                host_live,
                run_dir_present,
                session,
            };
            match bringup_step(&observed) {
                // The pane is up. That is not yet the thing the launcher is
                // waiting for — see [`await_thread_binding`].
                BringupStep::Ready => {
                    return await_thread_binding(&self.uid, self.deadline_monotonic_nanos)
                }
                BringupStep::Failed(why) => return BringUp::Failed(why),
                BringupStep::KeepWaiting => {}
            }
            match protocol::proc_identity::monotonic_now_nanos() {
                Some(now) if now < self.deadline_monotonic_nanos => {}
                Some(_) => {
                    return BringUp::Failed(format!(
                        "the wrapper did not prove ready before the launch deadline \
                         (broker legs serving: {}, host lease live: {}, run dir present: {})",
                        observed.sockets_bound, observed.host_live, observed.run_dir_present
                    ))
                }
                None => {
                    return BringUp::Failed(
                        "the monotonic clock could not be read, so the bring-up wait cannot \
                         be bounded; failing closed"
                            .into(),
                    )
                }
            }
            std::thread::sleep(BRINGUP_POLL);
        }
    }
}

/// The `internal-codex-coordinator` subcommand. Parses the launcher's charter,
/// runs [`coordinate`], and — on `Ready` — **stays, as the session's
/// supervisor**, exiting only when the session has ended. The launch outcome
/// lives in the durable record either way; the launcher reads it there, not from
/// this process's exit.
pub fn run_coordinator(args: &[String]) -> ! {
    match run_coordinator_inner(args) {
        Ok(_) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}

/// Everything the launcher hands the coordinator on the command line.
///
/// The seven host dimensions are **required and defaulted nowhere**, parsed with
/// [`crate::codex_host`]'s own `value_of`/`set_once`/`parse_hooks_enabled` rather
/// than a second copy of them. That is the point: the coordinator's only job with
/// these values is to hand them to the host, so a coordinator that accepted a
/// value the host will reject would turn a legible charter error into a pane that
/// flashes and dies.
#[derive(Debug)]
struct Charter {
    uid: String,
    launch_nonce: String,
    custodian_nonce: String,
    session_name: String,
    cwd: String,
    tmux_socket: String,
    deadline_ms: u64,
    codex: String,
    codex_sha256: String,
    codex_home: String,
    approval_policy: String,
    approvals_reviewer: String,
    sandbox: String,
    hooks_enabled: bool,
    tui_args: Vec<String>,
    hang_in_new_session: bool,
    hang_in_bringup: bool,
}

/// The default launch deadline when the launcher does not set one.
const DEFAULT_DEADLINE_MS: u64 = 30_000;

/// Resolve the launch cwd to the SAME spelling the app-server will report (round-2 P4).
///
/// This is the single canonicalization in the whole chain. It lives here, at the authority
/// that owns the launch cwd, so that everything downstream — the host argv, the broker's
/// `LaunchFingerprint`, the creation-response check, the turn workspace check — is plain
/// exact string equality with no filesystem access.
///
/// MEASURED: the coordinator is given `--cwd /tmp`; the app-server (which inherits the
/// pane's cwd) reports `/private/tmp`, because macOS `/tmp` is a symlink. `"/tmp" ==
/// "/private/tmp"` is FALSE, and `realpath` equality is TRUE — so without this the broker
/// would refuse every real turn of a session launched anywhere under a symlinked path.
///
/// Fails closed: a cwd that cannot be canonicalized (missing, unreadable, not a directory)
/// aborts the launch. Starting anyway would produce a session whose broker can never verify
/// a thread creation, i.e. a TUI that opens and then refuses the user's first turn.
fn canonical_launch_cwd(cwd: &str) -> Result<String> {
    let resolved = std::fs::canonicalize(cwd)
        .with_context(|| format!("resolving the launch cwd {cwd:?} to its canonical path"))?;
    if !resolved.is_dir() {
        anyhow::bail!("the launch cwd {cwd:?} is not a directory");
    }
    resolved
        .to_str()
        .map(str::to_string)
        .with_context(|| format!("the canonical launch cwd for {cwd:?} is not valid UTF-8"))
}

fn parse_charter(args: &[String]) -> Result<Charter> {
    use crate::codex_host::{parse_hooks_enabled, set_once, value_of};

    let mut uid = None;
    let mut launch_nonce = None;
    let mut custodian_nonce = None;
    let mut session_name = None;
    let mut cwd = None;
    let mut tmux_socket = None;
    let mut deadline_ms: Option<u64> = None;
    let mut codex = None;
    let mut codex_sha256 = None;
    let mut codex_home = None;
    let mut approval_policy = None;
    let mut approvals_reviewer = None;
    let mut sandbox = None;
    let mut hooks_enabled = None;
    let mut tui_args = Vec::new();
    let mut hang_in_new_session = false;
    let mut hang_in_bringup = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        match flag {
            "--uid" => set_once(&mut uid, flag, value_of(&mut it, flag)?)?,
            "--nonce" => set_once(&mut launch_nonce, flag, value_of(&mut it, flag)?)?,
            "--custodian-nonce" => set_once(&mut custodian_nonce, flag, value_of(&mut it, flag)?)?,
            "--session-name" => set_once(&mut session_name, flag, value_of(&mut it, flag)?)?,
            "--cwd" => set_once(&mut cwd, flag, value_of(&mut it, flag)?)?,
            "--tmux-socket" => set_once(&mut tmux_socket, flag, value_of(&mut it, flag)?)?,
            "--deadline-ms" => {
                let raw = value_of(&mut it, flag)?;
                let parsed = raw.parse::<u64>().with_context(|| {
                    format!("--deadline-ms must be a whole number, got {raw:?}")
                })?;
                set_once(&mut deadline_ms, flag, parsed)?;
            }
            "--codex" => set_once(&mut codex, flag, value_of(&mut it, flag)?)?,
            // A7.1, checked against `crate::codex`'s grammar — the same one the
            // launcher writes it with and the host reads it back with.
            "--codex-sha256" => {
                let parsed = crate::codex::parse_codex_sha256(&value_of(&mut it, flag)?)
                    .with_context(|| flag.to_string())?;
                set_once(&mut codex_sha256, flag, parsed)?;
            }
            "--codex-home" => set_once(&mut codex_home, flag, value_of(&mut it, flag)?)?,
            "--approval-policy" => set_once(&mut approval_policy, flag, value_of(&mut it, flag)?)?,
            "--approvals-reviewer" => {
                set_once(&mut approvals_reviewer, flag, value_of(&mut it, flag)?)?
            }
            "--sandbox" => set_once(&mut sandbox, flag, value_of(&mut it, flag)?)?,
            "--hooks-enabled" => {
                let parsed = parse_hooks_enabled(&value_of(&mut it, flag)?)?;
                set_once(&mut hooks_enabled, flag, parsed)?;
            }
            // Test-only (Principle B): hang at one of the two kill boundaries.
            // Only `hang` is a value either flag accepts — there is no spelling
            // that makes bring-up *succeed* without the host's evidence.
            "--test-newsession" | "--test-bringup" => {
                let raw = value_of(&mut it, flag)?;
                if raw != "hang" {
                    anyhow::bail!("{flag} accepts only \"hang\", got {raw:?}");
                }
                if flag == "--test-newsession" {
                    hang_in_new_session = true;
                } else {
                    hang_in_bringup = true;
                }
            }
            // Everything past the boundary belongs to the TUI, verbatim.
            "--" => {
                tui_args.extend(it.by_ref().cloned());
                break;
            }
            other => anyhow::bail!("unexpected argument to internal-codex-coordinator: {other:?}"),
        }
    }

    // The same reserved grammar `codeconnect codex` and the host both apply. It
    // runs here as well — not instead of the host's check, which stays the last
    // word — so a refused flag is reported by the process a human is looking at
    // rather than by a pane that has already opened.
    crate::codex::validate_codex_argv(&tui_args)
        .map_err(|refusal| anyhow::anyhow!("refused passthrough TUI argument: {refusal}"))?;

    // A7.1's other charter rule, applied by the same function the host applies —
    // `crate::codex` owns both the digest grammar and this one so the two parsers
    // cannot drift. Checked HERE and not only at the host because this process is the
    // one that mints a run directory and opens a tmux pane: a relative or bare
    // `--codex` used to survive parsing, acquire all of that, and only then die in a
    // pane the operator never sees. The refusal now lands before anything exists.
    let codex = codex.context("--codex <path> is required (the coordinator resolves nothing)")?;
    crate::codex::require_absolute_codex(std::path::Path::new(&codex))?;

    Ok(Charter {
        uid: uid.context("--uid <value> is required")?,
        launch_nonce: launch_nonce.unwrap_or_else(codex_launch::mint_nonce),
        custodian_nonce: custodian_nonce.unwrap_or_else(codex_launch::mint_nonce),
        session_name: session_name.unwrap_or_else(|| "cc-codex".into()),
        cwd: cwd.unwrap_or_else(|| "/".into()),
        tmux_socket: tmux_socket.unwrap_or_else(|| protocol::TMUX_SOCKET_NAME.to_string()),
        deadline_ms: deadline_ms.unwrap_or(DEFAULT_DEADLINE_MS),
        codex,
        codex_sha256: codex_sha256.context(
            "--codex-sha256 <hex> is required (the coordinator inspects nothing, so the \
             identity of the binary it hands the host has to come from whoever did)",
        )?,
        codex_home: codex_home.context("--codex-home <path> is required")?,
        approval_policy: approval_policy
            .context("--approval-policy <value> is required (no default is applied)")?,
        approvals_reviewer: approvals_reviewer
            .context("--approvals-reviewer <value> is required (no default is applied)")?,
        sandbox: sandbox.context("--sandbox <value> is required (no default is applied)")?,
        hooks_enabled: hooks_enabled
            .context("--hooks-enabled true|false is required (no default is applied)")?,
        tui_args,
        hang_in_new_session,
        hang_in_bringup,
    })
}

fn run_coordinator_inner(args: &[String]) -> Result<CoordinateOutcome> {
    let charter = parse_charter(args)?;
    let coordinator = codex_launch::require_current_identity()?;
    let boot = protocol::proc_identity::boot_identity().context("reading boot identity")?;
    let now = protocol::proc_identity::monotonic_now_nanos().context("reading monotonic clock")?;
    // Checked, not bare arithmetic. `--deadline-ms` is a `u64` from argv, and the
    // ×1e6 to nanoseconds overflows well inside that range: in debug that panics,
    // and in release — where this workspace sets `panic = "abort"` and leaves
    // `overflow-checks` off — it WRAPS, handing the launch a deadline that is
    // either already past or centuries away. The second case defeats both the
    // bring-up loop's bound and the custodian's expiry check at once, so an
    // out-of-range value has to be refused at the charter like every other
    // malformed dimension.
    let deadline = charter
        .deadline_ms
        .checked_mul(1_000_000)
        .and_then(|ns| now.checked_add(ns))
        .with_context(|| {
            format!(
                "--deadline-ms {} is out of range: it does not fit as nanoseconds past the \
                 current monotonic clock",
                charter.deadline_ms
            )
        })?;
    let run_dir = choose_run_dir(&charter.uid, &charter.launch_nonce)?;
    // The workspace anchor, canonicalized ONCE here (see `RealCoordinatorDeps::launch_cwd`).
    // Fail closed: a launch cwd that cannot be resolved is a launch whose broker could never
    // prove a thread binding, so it must not start rather than start un-anchored.
    let launch_cwd = canonical_launch_cwd(&charter.cwd)?;
    let mut deps = RealCoordinatorDeps {
        uid: charter.uid.clone(),
        session_name: charter.session_name.clone(),
        cwd: charter.cwd,
        launch_cwd,
        tmux_socket: charter.tmux_socket,
        // Fail closed: a coordinator that cannot name its own executable cannot
        // put a host in the pane, and must not create a session it cannot fill.
        self_exe: std::env::current_exe().context("locating this binary")?,
        custodian_nonce: charter.custodian_nonce,
        launch_nonce: charter.launch_nonce.clone(),
        coordinator,
        codex: charter.codex,
        codex_sha256: charter.codex_sha256,
        codex_home: charter.codex_home,
        approval_policy: charter.approval_policy,
        approvals_reviewer: charter.approvals_reviewer,
        sandbox: charter.sandbox,
        hooks_enabled: charter.hooks_enabled,
        tui_args: charter.tui_args,
        run_dir,
        deadline_monotonic_nanos: deadline,
        session_a: None,
        hang_in_new_session: charter.hang_in_new_session,
        hang_in_bringup: charter.hang_in_bringup,
    };
    let outcome = coordinate(
        CoordinateSetup {
            uid: charter.uid,
            launch_nonce: charter.launch_nonce,
            session_name: charter.session_name,
            coordinator,
            boot,
            deadline_monotonic_nanos: deadline,
            created_ms: protocol::time::now_unix_ms(),
        },
        &mut deps,
    )?;
    if outcome == CoordinateOutcome::Ready {
        supervise_ready_session(&deps)?;
    }
    Ok(outcome)
}

/// Hold the session as its supervisor, until it ends.
///
/// **Why this process and not a spawned one.** A `ready` record whose
/// coordinator is proven gone is *session-fatal*: the custodian's `ready` arm
/// tears the live session down ([`crate::codex_custodian`], "coordinator/
/// supervisor loss"). That rule was written for the merged role and has been
/// waiting for it — until now the coordinator exited the moment it committed
/// `Ready`, so committing readiness is what *started* the teardown, and every
/// live gate that needed a session to outlive its launch held the coordinator at
/// `--test-bringup hang` to get one. Supervising here makes the custodian's
/// premise true rather than working around it, and it costs no new identity: the
/// process the record already names is the one now doing the supervising, so
/// there is no window in which the launch is `ready` and nobody is watching, and
/// nothing has to be re-CAS'd into the record.
///
/// **Why no argv.** `codeconnect supervise` exists for `codeconnect claude`,
/// which has to hand its supervisor across a process boundary. Here the facts
/// are already in hand — the uid, the tmux name and server, the canonical cwd,
/// the resolved codex binary, and the run dir whose ccd leg the bring-up just
/// proved is serving — so they travel as values. No flag, no parse, nothing a
/// caller can forget.
///
/// The cwd registered is the **canonical** one, the same string the broker
/// fingerprints and the app-server reports, not the launcher's spelling of it: a
/// session launched under a symlinked path would otherwise be listed at a path
/// that disagrees with every other record of the same run.
fn supervise_ready_session(deps: &RealCoordinatorDeps) -> Result<()> {
    crate::supervisor::run(
        crate::supervisor::SupervisorArgs {
            session_id: deps.session_name.clone(),
            session_uid: Some(deps.uid.clone()),
            tmux_session: deps.session_name.clone(),
            tmux_socket: deps.tmux_socket.clone(),
            cwd: deps.launch_cwd.clone(),
            claude_bin: None,
            codex: Some(crate::supervisor::CodexSeat {
                codex_bin: deps.codex.clone(),
                ccd_socket: ccd_leg(&deps.run_dir).to_string_lossy().into_owned(),
                // A launch creates the thread it runs on, so this is the first
                // visit (D4). A later visit is a `/new` inside the TUI, which
                // this side never learns and never needs to: the daemon's link
                // counts visits off the broker's own stream.
                generation: 1,
            }),
            // **The server this launch was pinned to, handed on.** The coordinator
            // resolved it before it brought the wrapper up and has judged every
            // bring-up poll against it; the supervisor half needs the same pin for
            // the opposite question. A Codex launch owns its tmux server outright,
            // so the last session leaving drains it — and a probe with no pin can
            // only call that `Unknown` and reset its absence streak for ever, which
            // is a wedge and not a wait (see [`crate::supervisor::SupervisorArgs::server_a`]).
            //
            // `Ready` is only reachable through `bring_up_wrapper`, which refuses to
            // judge readiness unpinned, so this is `Some` on every path that gets
            // here. It is passed as the `Option` it is rather than unwrapped: an
            // unpinned supervisor is the old behaviour, not a panic.
            server_a: deps.session_a.clone(),
        },
        &protocol::config::Config::load(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::proc_identity::{boot_identity, current_identity, monotonic_now_nanos};

    // Coordinator tests write launch records through `codex_launch`'s
    // process-global test root (cfg(test) `sessions_root()`), using distinct
    // uids — no env-var mutation, no cross-test serialization needed.

    /// Scripted forward-launch deps. `spawn_custodian` mimics the real arm — it
    /// CAS's a **real, live** custodian identity (this process) into the record —
    /// so `to_ready`'s under-the-lock re-verification of a live custodian
    /// (finding 7) sees a genuinely live one. Its *observed* liveness in the
    /// coordinator's own pre-commit checks is still overridable via
    /// `custodian_alive` to drive the "custodian lost" boundaries.
    struct FakeDeps {
        custodian: ProcessIdentity,
        custodian_alive: bool,
        new_session: Option<NewSessionOutcome>,
        bring_up: Option<BringUp>,
        spawn_custodian_fails: bool,
        /// A9.3: arm the one-shot post-rename fsync failure just before the
        /// coordinator's pre-mutation block, so `mark_new_session_starting`
        /// publishes the in-flight flag and then returns `Err`.
        fail_the_pre_mutation_write: bool,
        /// Captured at `spawn_custodian`, so the scripted bring-up can write the
        /// record the way a real host does. The fake is handed the uid nowhere else.
        uid: String,
    }
    impl Default for FakeDeps {
        fn default() -> Self {
            FakeDeps {
                // A real, live identity (us) so the record's custodian passes
                // to_ready's real-kernel liveness re-check on the happy path.
                custodian: current_identity().unwrap(),
                custodian_alive: true,
                new_session: Some(NewSessionOutcome::Created(Box::new(fake_owned()))),
                bring_up: Some(BringUp::Ready),
                spawn_custodian_fails: false,
                fail_the_pre_mutation_write: false,
                uid: String::new(),
            }
        }
    }

    /// Write the record the way a real host does once its pane is up: take the
    /// lease, then record and exec-confirm both roles (round-3 finding 2).
    ///
    /// `to_ready` re-asks the host census at commit time, so a fake that reports
    /// `Ready` without having produced this shape is not simulating a launch that
    /// came up — it is simulating one that did not. Every identity here is this
    /// process's own, which is the only one a unit test can rely on being `Alive`
    /// for as long as the commit takes.
    fn fake_host_comes_up(uid: &str) {
        let me = current_identity().unwrap();
        let lock = LaunchLock::acquire(uid).unwrap();
        assert_eq!(
            codex_launch::admit_host(&lock, uid, "n0nce", &me, me.pid, "codex-host").unwrap(),
            codex_launch::Admission::Admitted
        );
        for role in codex_launch::HOST_CHILD_ROLES {
            codex_launch::record_host_child(
                &lock,
                uid,
                &me,
                codex_launch::ChildEntry {
                    role: role.to_string(),
                    identity: me,
                    pgid: me.pid,
                    nonce: "n0nce".into(),
                    argv_hash: String::new(),
                    recorded_by: None,
                    exec_confirmed: false,
                },
            )
            .unwrap();
            codex_launch::confirm_host_child_exec(&lock, uid, &me, role, &me).unwrap();
        }
    }
    impl CoordinatorDeps for FakeDeps {
        fn spawn_custodian(&mut self, uid: &str) -> Result<ProcessIdentity> {
            if self.spawn_custodian_fails {
                anyhow::bail!("gate refused the custodian");
            }
            self.uid = uid.to_string();
            // Mimic the real path: arm the custodian into the record via the
            // single-owner CAS, so to_ready can re-verify a recorded live one.
            let lock = LaunchLock::acquire(uid)?;
            codex_launch::cas_custodian_with_child(
                &lock,
                uid,
                self.custodian,
                self.custodian.pid,
                "fake-nonce",
                "fake-hash",
            )?;
            Ok(self.custodian)
        }
        fn new_session(&mut self) -> NewSessionOutcome {
            self.new_session
                .take()
                .unwrap_or(NewSessionOutcome::Failed("no scripted outcome".into()))
        }
        fn bring_up_wrapper(&mut self) -> BringUp {
            let out = self
                .bring_up
                .take()
                .unwrap_or(BringUp::Failed("no scripted bring-up".into()));
            // A bring-up that reports `Ready` is reporting that a host came up, and
            // the record has to say so — `to_ready` re-asks at commit time.
            if matches!(out, BringUp::Ready) && !self.uid.is_empty() {
                fake_host_comes_up(&self.uid);
            }
            out
        }
        fn custodian_liveness(&self, _id: &ProcessIdentity) -> Liveness {
            // The last hook before the pre-mutation block, and the only durable
            // write between here and it is `mark_new_session_starting` (this fake
            // names no run dir), so a one-shot armed here lands on exactly that
            // write.
            if self.fail_the_pre_mutation_write {
                codex_launch::fail_next_dir_fsync();
            }
            if self.custodian_alive {
                Liveness::Alive
            } else {
                Liveness::Gone
            }
        }
        /// This fake creates no pane, so there is no directory to name.
        fn run_dir(&self) -> Option<&str> {
            None
        }
    }

    fn fake_owned() -> OwnedSession {
        OwnedSession {
            socket: "codeconnect".into(),
            session_id: "$7".into(),
            uid: "u".into(),
            server_pid: 1,
            server_start_time: 2,
            session_created: 3,
            // A resolved session always carries a proven server birth now
            // (round-5 finding 3); the coordinator persists it as server A.
            server_birth: Some(protocol::proc_identity::BirthIdentity {
                start_sec: 100,
                start_usec: 200,
            }),
        }
    }

    /// **A11.1, readiness half: confirmed-once is not alive-now** (round-2 finding 2).
    ///
    /// `exec_confirmed` records that the host proved a child got past `execve`. It is
    /// never rewritten, so on its own it certifies `Ready` for a session whose TUI or
    /// app-server has since exited — and nothing else the coordinator polls
    /// contradicts that, because the broker's listeners belong to the HOST and go on
    /// answering after either child dies.
    #[test]
    fn a_recorded_and_confirmed_child_that_is_dead_does_not_count_as_live() {
        use protocol::proc_identity::{current_identity, BirthIdentity};

        let entry = |role: &str, identity| codex_launch::ChildEntry {
            role: role.to_string(),
            identity,
            pgid: 1,
            nonce: "n".into(),
            argv_hash: String::new(),
            recorded_by: None,
            exec_confirmed: true,
        };
        let alive = current_identity().expect("this process");
        // A pid that cannot be alive as this identity: pid 1 with a birth stamp
        // nothing has. Bound to the birth, so it is `Gone` rather than "some pid 1".
        let gone = ProcessIdentity {
            pid: 1,
            birth: BirthIdentity {
                start_sec: 1,
                start_usec: 1,
            },
        };

        let mut record = crate::codex_custodian::tests::base_record(
            codex_launch::LaunchState::Pending,
            codex_launch::CleanupState::Pending,
            false,
        );
        record.host_lease = Some(codex_launch::HostLease {
            identity: alive,
            pgid: 1,
            nonce: "n".into(),
            role: "codex-host".into(),
        });
        let under_lease = |children: Vec<codex_launch::ChildEntry>| {
            children
                .into_iter()
                .map(|mut c| {
                    c.recorded_by = Some(alive);
                    c
                })
                .collect::<Vec<_>>()
        };

        record.children = under_lease(vec![entry("app-server", alive), entry("tui", alive)]);
        assert!(
            codex_launch::host_children_ready(&record),
            "two live, confirmed, lease-owned children are ready"
        );

        record.children = under_lease(vec![entry("app-server", alive), entry("tui", gone)]);
        // The record half is untouched by the death — both roles are still recorded
        // by the live lease and still flagged `exec_confirmed`, because nothing
        // rewrites those when a child exits. That is precisely why the liveness
        // conjunct has to be part of the same question.
        assert!(
            record
                .children
                .iter()
                .all(|c| c.recorded_by == Some(alive) && c.exec_confirmed),
            "the premise: the record still attests everything it ever attested"
        );
        assert!(
            !codex_launch::host_children_ready(&record),
            "THE GATE: a child that is gone must not be read as ready merely because \
             its exec was confirmed once"
        );

        // …and the same shape, on disk, through the predicate readiness actually
        // consults. Built with the real writers so the lease is one this process
        // holds, which is what `record_host_child` and `confirm_host_child_exec`
        // demand — and what makes the record indistinguishable, to every other
        // check, from a healthy one.
        let uid = "readyalive";
        let lock = codex_launch::LaunchLock::acquire(uid).unwrap();
        codex_launch::create_pending(
            &lock,
            codex_launch::NewLaunch {
                launch_nonce: "cafef00d".into(),
                uid: uid.into(),
                session_name: "cc-ra".into(),
                coordinator: alive,
                boot: protocol::proc_identity::boot_identity().unwrap(),
                deadline_monotonic_nanos: protocol::proc_identity::monotonic_now_nanos().unwrap()
                    + 60_000_000_000,
                created_ms: 1,
            },
        )
        .unwrap();
        codex_launch::cas_custodian_with_child(&lock, uid, alive, alive.pid, "n", "h").unwrap();
        assert_eq!(
            codex_launch::admit_host(&lock, uid, "cafef00d", &alive, alive.pid, "codex-host")
                .unwrap(),
            codex_launch::Admission::Admitted
        );
        for (role, identity) in [("app-server", alive), ("tui", gone)] {
            codex_launch::record_host_child(&lock, uid, &alive, entry(role, identity)).unwrap();
            codex_launch::confirm_host_child_exec(&lock, uid, &alive, role, &identity).unwrap();
        }
        drop(lock);
        let on_disk = codex_launch::load(uid).unwrap();
        assert!(
            on_disk
                .children
                .iter()
                .filter(|c| c.role != "custodian")
                .all(|c| c.recorded_by == Some(alive) && c.exec_confirmed),
            "everything the record can attest to is in place"
        );
        assert!(
            !host_lease_is_live(uid),
            "THE GATE, wired: readiness must refuse a launch whose TUI is gone, even \
             though the lease is live and both roles are recorded and confirmed"
        );
    }

    /// Bring a `pending` record into existence for `deps`, as `coordinate` would.
    fn pending_for(deps: &RealCoordinatorDeps) {
        let lock = codex_launch::LaunchLock::acquire(&deps.uid).unwrap();
        codex_launch::create_pending(
            &lock,
            codex_launch::NewLaunch {
                launch_nonce: deps.launch_nonce.clone(),
                uid: deps.uid.clone(),
                session_name: "cc-1".into(),
                coordinator: deps.coordinator,
                boot: protocol::proc_identity::boot_identity().unwrap(),
                deadline_monotonic_nanos: protocol::proc_identity::monotonic_now_nanos().unwrap()
                    + 60_000_000_000,
                created_ms: 1,
            },
        )
        .unwrap();
        // The forward path always marks the mutation in flight before issuing it,
        // and that flag is what the outcome below has to end up disarming.
        codex_launch::mark_new_session_starting(&lock, &deps.uid).unwrap();
    }

    /// **The post-create failure paths, driven THROUGH THE REAL CALLER** (round-3
    /// findings 4-coverage, 5 and 6).
    ///
    /// These two arms live inside `RealCoordinatorDeps`, past a real `tmux
    /// new-session` and a real resolve, so nothing used to reach them: round-2's fix
    /// was placed by inspection and asserted against the persistence HELPER. That is
    /// how finding 6 survived a green suite — the helper cleared
    /// `new_session_indeterminate`, and the caller then returned `Indeterminate`,
    /// which `after_pending` handed to `fail_new_session_indeterminate`, setting it
    /// straight back to true. Every claim below is now made against
    /// `finish_new_session` itself, with the failure injected at the seam the real
    /// code calls.
    #[test]
    fn a_post_create_failure_persists_the_session_and_stays_determinate() {
        // ── The assertion fails. A is known; the premise is not. ──────────────
        let mut deps = real_deps("/tmp/cch.persist.0123456789abcdef");
        let uid = deps.uid.clone();
        pending_for(&deps);
        assert!(
            codex_launch::load(&uid).unwrap().server_a.is_none(),
            "nothing is pinned before the session is created"
        );

        fail_next_remain_assertion("tmux refused");
        let outcome = deps.finish_new_session(fake_owned());
        assert!(
            matches!(outcome, NewSessionOutcome::CreatedThenFailed(_)),
            "a created, resolved, PERSISTED session that then fails is not indeterminate"
        );

        let record = codex_launch::load(&uid).unwrap();
        assert!(
            record.server_a.is_some(),
            "THE GATE (finding 5): the session tmux created is written down BEFORE \
             anything else can fail — cleanup has nothing to bind to otherwise"
        );
        assert!(
            !record.remain_on_exit_asserted,
            "and the premise must NOT be claimed: asserting it is what just failed"
        );
        assert!(
            !record.new_session_indeterminate,
            "a session that is pinned is not an in-flight mutation"
        );

        // ── …and the outcome survives the state machine ────────────────────────
        //
        // THE FINDING-6 GATE. The persistence above is only worth anything if the
        // arm that consumes this outcome does not undo it. Driven through
        // `after_pending`'s real dispatch.
        let mut fake = FakeDeps {
            new_session: Some(outcome),
            ..FakeDeps::default()
        };
        let out = after_pending(&setup(&uid, far()), &mut fake).unwrap();
        assert!(
            matches!(out, CoordinateOutcome::Failed(_)),
            "the launch fails: {out:?}"
        );
        let record = codex_launch::load(&uid).unwrap();
        assert!(
            !record.new_session_indeterminate,
            "THE GATE: the recorded creation must NOT be re-armed as indeterminate — \
             a known, pinned session cannot also be a mutation that might yet land, \
             and a record in that shape keeps the custodian armed until reboot"
        );
        assert!(
            record.server_a.is_some(),
            "and A survives the terminalization, so cleanup can bind to it"
        );
        assert_eq!(
            record.cleanup,
            codex_launch::CleanupState::Pending,
            "the session exists, so it is owed cleanup"
        );
        assert!(matches!(record.state, LaunchState::Failed { .. }));

        // ── The note fails after a SUCCESSFUL assertion ────────────────────────
        let mut deps = real_deps("/tmp/cch.persist2.0123456789abcde");
        deps.uid = "01JQXV9K7B8N4M2P6R3T5W9YQE".into();
        let uid2 = deps.uid.clone();
        pending_for(&deps);
        fail_next_remain_note();
        let outcome = deps.finish_new_session(fake_owned());
        assert!(matches!(outcome, NewSessionOutcome::CreatedThenFailed(_)));
        let record = codex_launch::load(&uid2).unwrap();
        assert!(
            record.server_a.is_some(),
            "A is persisted on this arm too, and before the assertion ran"
        );
        assert!(
            !record.new_session_indeterminate,
            "and this arm is determinate as well"
        );
    }

    /// **A persist that RETURNS `Err` after its rename PUBLISHED is not
    /// indeterminate** (round-4 finding 2).
    ///
    /// `store_atomic` publishes with the rename and makes it durable with the
    /// directory fsync after it, and A9.3's injected fault is exactly the gap
    /// between them: `{server_a: Some(A), new_session_indeterminate: false}` is on
    /// disk and readable by everyone, and the call that put it there returns `Err`.
    ///
    /// The old mapping read the RETURN and answered `Indeterminate`, which
    /// `after_pending` hands to `fail_new_session_indeterminate` — setting the flag
    /// durably back to **true**, one statement after the rename cleared it. The
    /// record then claims a late session may still appear about a session that has
    /// already appeared and been pinned, and `late_session_still_possible` refuses
    /// every absence the custodian observes until the next reboot. That is the same
    /// re-arming defect `CreatedThenFailed` exists to close, reached one layer down.
    ///
    /// Two gates: the outcome is determinate, and the flag STAYS cleared through the
    /// state machine that consumes it.
    #[test]
    fn a_persist_that_published_before_failing_terminalizes_rather_than_rearming() {
        let mut deps = real_deps("/tmp/cch.persist4.0123456789abcde");
        deps.uid = "01JQXV9K7B8N4M2P6R3T5W9YQG".into();
        let uid = deps.uid.clone();
        pending_for(&deps);
        assert!(
            codex_launch::load(&uid).unwrap().new_session_indeterminate,
            "the premise: the forward path armed the flag before the mutation"
        );

        // The one fault this module cannot otherwise stage: the rename lands, the
        // directory fsync then fails, and `store_atomic` returns `Err`.
        codex_launch::fail_next_dir_fsync();
        let outcome = deps.finish_new_session(fake_owned());
        assert!(
            matches!(outcome, NewSessionOutcome::CreatedThenFailed(_)),
            "a published A is a determinate fact about a known session, whatever the \
             write that published it returned: {}",
            match &outcome {
                NewSessionOutcome::Indeterminate => "got Indeterminate".to_string(),
                NewSessionOutcome::Created(_) => "got Created".to_string(),
                NewSessionOutcome::Failed(w) => format!("got Failed({w})"),
                NewSessionOutcome::CreatedThenFailed(w) => format!("got CreatedThenFailed({w})"),
            }
        );
        let record = codex_launch::load(&uid).unwrap();
        assert!(
            record.server_a.is_some(),
            "the premise the whole finding rests on: the rename really did publish A"
        );
        assert!(
            !record.new_session_indeterminate,
            "and the rename really did clear the flag"
        );

        // THE GATE. The mapping is only worth anything if the arm consuming it does
        // not re-arm the flag. Driven through `after_pending`'s real dispatch.
        let mut fake = FakeDeps {
            new_session: Some(outcome),
            ..FakeDeps::default()
        };
        let out = after_pending(&setup(&uid, far()), &mut fake).unwrap();
        assert!(
            matches!(out, CoordinateOutcome::Failed(_)),
            "the launch fails: {out:?}"
        );
        let record = codex_launch::load(&uid).unwrap();
        assert!(
            !record.new_session_indeterminate,
            "THE GATE: the flag stays CLEARED. Re-arming it makes the custodian \
             refuse every absence it observes until the next reboot"
        );
        assert_eq!(
            record.cleanup,
            CleanupState::Pending,
            "and the session that exists is owed cleanup, bound to the A on disk"
        );
    }

    /// The direction the fix must NOT over-reach: a persist that failed BEFORE its
    /// rename published anything is still genuinely indeterminate, and the flag must
    /// stay armed. `fake_owned()` carries a server birth, so `ServerA::from_owned`
    /// succeeds; the failure is the record read, because no record exists for this
    /// uid at all — nothing was ever published, so there is nothing to respect.
    #[test]
    fn a_persist_that_published_nothing_stays_indeterminate() {
        let mut deps = real_deps("/tmp/cch.persist5.0123456789abcde");
        deps.uid = "01JQXV9K7B8N4M2P6R3T5W9YQH".into();
        // Deliberately NO `pending_for`: `record_new_session_created` loads before it
        // stores, so it fails with nothing renamed and nothing on disk.
        let outcome = deps.finish_new_session(fake_owned());
        assert!(
            matches!(outcome, NewSessionOutcome::Indeterminate),
            "an unpinned created session is exactly what the fail-closed flag is for"
        );
    }

    /// The seam is a SEAM, not the thing under test: with no fault armed,
    /// `finish_new_session` runs its real steps and reports `Created`.
    ///
    /// Without this, every assertion in the test above would also hold of a
    /// `finish_new_session` that failed unconditionally.
    #[test]
    fn the_post_create_path_reports_created_when_nothing_fails() {
        let mut deps = real_deps("/tmp/cch.persist3.0123456789abcde");
        deps.uid = "01JQXV9K7B8N4M2P6R3T5W9YQF".into();
        let uid = deps.uid.clone();
        pending_for(&deps);
        // `fake_owned()` names no live tmux server, so the epoch-pinned assertion
        // refuses on its own terms — which is the real code path, not a seam. The
        // seam is exercised by arming nothing and letting the assertion be the only
        // thing that decides.
        let outcome = deps.finish_new_session(fake_owned());
        assert!(
            matches!(outcome, NewSessionOutcome::CreatedThenFailed(_)),
            "an assertion that cannot bind to a live server epoch must refuse"
        );
        assert!(
            codex_launch::load(&uid).unwrap().server_a.is_some(),
            "and A is persisted regardless, because it was known before the refusal"
        );
    }

    fn setup(uid: &str, deadline: u64) -> CoordinateSetup {
        CoordinateSetup {
            uid: uid.into(),
            launch_nonce: "n0nce".into(),
            session_name: "cc-1".into(),
            coordinator: current_identity().unwrap(),
            boot: boot_identity().unwrap(),
            deadline_monotonic_nanos: deadline,
            created_ms: 1,
        }
    }

    fn far() -> u64 {
        monotonic_now_nanos().unwrap() + 60_000_000_000
    }

    #[test]
    fn a_clean_launch_reaches_ready() {
        let mut deps = FakeDeps::default();
        let out = coordinate(setup("c1", far()), &mut deps).unwrap();
        assert_eq!(out, CoordinateOutcome::Ready);
        assert_eq!(codex_launch::load("c1").unwrap().state, LaunchState::Ready);
        assert_eq!(wait_on_record("c1", ms(200), ms(5)), LaunchWait::Ready);
    }

    #[test]
    fn losing_the_custodian_before_tmux_fails_the_launch() {
        let mut deps = FakeDeps {
            custodian_alive: false,
            ..Default::default()
        };
        let out = coordinate(setup("c2", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        assert!(matches!(
            codex_launch::load("c2").unwrap().state,
            LaunchState::Failed { .. }
        ));
        // The custodian was lost BEFORE tmux, so no session was created ⇒ nothing
        // to clean (round-4): cleanup is NotRequired, and the custodian completes
        // via `Done` rather than probing a nonexistent server forever.
        assert_eq!(
            codex_launch::load("c2").unwrap().cleanup,
            CleanupState::NotRequired
        );
    }

    #[test]
    fn an_indeterminate_new_session_fails_with_the_flag_and_does_not_clean() {
        let mut deps = FakeDeps {
            new_session: Some(NewSessionOutcome::Indeterminate),
            ..Default::default()
        };
        let out = coordinate(setup("c3", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        let rec = codex_launch::load("c3").unwrap();
        assert!(matches!(rec.state, LaunchState::Failed { .. }));
        assert!(
            rec.new_session_indeterminate,
            "the custodian must know the outcome was indeterminate"
        );
        assert_eq!(rec.cleanup, CleanupState::Pending);
    }

    #[test]
    fn a_failed_new_session_fails_the_launch() {
        let mut deps = FakeDeps {
            new_session: Some(NewSessionOutcome::Failed("boom".into())),
            ..Default::default()
        };
        let out = coordinate(setup("c4", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        let rec = codex_launch::load("c4").unwrap();
        assert!(matches!(rec.state, LaunchState::Failed { .. }));
        assert!(!rec.new_session_indeterminate);
    }

    #[test]
    fn a_wrapper_bringup_failure_fails_the_launch() {
        let mut deps = FakeDeps {
            bring_up: Some(BringUp::Failed("app-server never initialized".into())),
            ..Default::default()
        };
        let out = coordinate(setup("c5", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
    }

    #[test]
    fn a_passed_deadline_before_new_session_fails_closed() {
        // Deadline already in the past.
        let past = monotonic_now_nanos().unwrap().saturating_sub(1);
        let mut deps = FakeDeps::default();
        let out = coordinate(setup("c6", past), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
    }

    #[test]
    fn a_post_record_coordinator_error_terminalizes_to_failed_not_a_guardianless_pending() {
        let mut deps = FakeDeps {
            spawn_custodian_fails: true,
            ..Default::default()
        };
        // The pending record was written, then the custodian spawn errored. A
        // coordinator error after the record exists must NOT leave a Pending with
        // no guardian (the "Plus" finding): it is terminalized to Failed.
        let out = coordinate(setup("c7", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        let rec = codex_launch::load("c7").unwrap();
        assert!(
            matches!(rec.state, LaunchState::Failed { .. }),
            "the record must be terminalized, not left Pending"
        );
        // A9.3: and the disposition is NotRequired, not the old hard-coded
        // Pending. The spawn failed BEFORE any tmux mutation, so no session can
        // exist — A is unrecorded and the in-flight flag was never set. A
        // `Failed{Pending}` here is the wedge: the sweep rearms a custodian, whose
        // unpinned `destroy()` returns Unavailable forever, with no server-gone
        // evidence and no boot change to escape on. Armed until reboot for a
        // session that never existed.
        assert!(rec.server_a.is_none() && !rec.new_session_indeterminate);
        assert_eq!(rec.cleanup, CleanupState::NotRequired);
    }

    #[test]
    fn a_pre_mutation_failure_that_published_the_flag_still_pins_cleanup_not_required() {
        // A9.3, the case the derived rule cannot see. `store_atomic` publishes by
        // rename and fsyncs afterwards, so `mark_new_session_starting` can leave
        // `new_session_indeterminate: true` VISIBLE and still return `Err`. The
        // error stops the coordinator before `deps.new_session()` is called at all
        // — no session, no server A, no host — but the catch-all's rule reads that
        // published flag as "a mutation may be in flight" and writes
        // `Failed{Pending}`. The custodian then has nothing to prove absence
        // against: an unpinned `destroy()` answers `Unavailable`/`Absent`,
        // `server_gone_evidence` has no identity, and the late-session rule refuses
        // one absence as proof. Armed until reboot for a session that never
        // existed. So the disposition is pinned by the caller that KNOWS tmux was
        // never invoked.
        let mut deps = FakeDeps {
            fail_the_pre_mutation_write: true,
            ..Default::default()
        };
        let out = coordinate(setup("c10", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        let rec = codex_launch::load("c10").unwrap();
        assert!(
            matches!(rec.state, LaunchState::Failed { .. }),
            "the record must be terminalized: {rec:?}"
        );
        assert!(
            rec.server_a.is_none(),
            "tmux was never invoked, so there is no server A: {rec:?}"
        );
        assert!(
            !rec.new_session_indeterminate,
            "a determinate coordinator failure disarms the published flag: {rec:?}"
        );
        assert_eq!(
            rec.cleanup,
            CleanupState::NotRequired,
            "no session can exist, so nothing is owed — a `Pending` here is the \
             until-reboot wedge: {rec:?}"
        );
        // The scripted new-session outcome is still sitting unconsumed, which is
        // the direct proof that tmux was never reached.
        assert!(
            deps.new_session.is_some(),
            "the launch must have failed BEFORE new_session was called"
        );
    }

    #[test]
    fn a_coordinator_error_after_the_session_was_created_still_terminalizes_to_pending() {
        // The symmetric half of A9.3: the derived rule must not slide the other
        // way. `ServerA::from_owned` fails closed on a session with no proven
        // server birth — an error raised AFTER tmux created the session — and the
        // record the disposition is derived from still carries
        // `mark_new_session_starting`'s in-flight flag, so a mutation may well
        // have landed. Cleanup is owed: Pending. (The write itself then disarms
        // the flag, as every determinate coordinator failure does — the flag is
        // the *input* to the rule, not its output.)
        let mut unproven = fake_owned();
        unproven.server_birth = None;
        let mut deps = FakeDeps {
            new_session: Some(NewSessionOutcome::Created(Box::new(unproven))),
            ..Default::default()
        };
        let out = coordinate(setup("c9", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        let rec = codex_launch::load("c9").unwrap();
        assert!(matches!(rec.state, LaunchState::Failed { .. }));
        assert!(rec.server_a.is_none(), "A was never recordable: {rec:?}");
        assert_eq!(
            rec.cleanup,
            CleanupState::Pending,
            "a session may exist, so cleanup is owed: {rec:?}"
        );
    }

    #[test]
    fn terminalize_disarms_the_indeterminate_flag_and_is_bounded_when_the_lock_is_held() {
        // Finding 4: terminalize (the coordinator's determinate-failure path)
        // durably DISARMS the in-flight flag, so a coordinator error after
        // `mark_new_session_starting` (even the not-yet-run case) cannot leave the
        // custodian armed forever on a later `Absent`.
        {
            let lock = LaunchLock::acquire("term_flag").unwrap();
            codex_launch::create_pending(
                &lock,
                NewLaunch {
                    launch_nonce: "n".into(),
                    uid: "term_flag".into(),
                    session_name: "cc-1".into(),
                    coordinator: current_identity().unwrap(),
                    boot: boot_identity().unwrap(),
                    deadline_monotonic_nanos: far(),
                    created_ms: 1,
                },
            )
            .unwrap();
            codex_launch::mark_new_session_starting(&lock, "term_flag").unwrap();
        }
        assert!(
            codex_launch::load("term_flag")
                .unwrap()
                .new_session_indeterminate
        );
        terminalize("term_flag", "coordinator error before new-session").unwrap();
        let rec = codex_launch::load("term_flag").unwrap();
        assert!(
            !rec.new_session_indeterminate,
            "terminalize must disarm the speculative in-flight flag"
        );
        assert!(matches!(rec.state, LaunchState::Failed { .. }));

        // Finding 3: with the lock HELD, terminalize must be bounded — it uses a
        // non-blocking try-lock, so a stopped holder cannot hang it forever. It
        // returns Err well within the budget, never blocking indefinitely.
        let lock = LaunchLock::acquire("term_held").unwrap();
        codex_launch::create_pending(
            &lock,
            NewLaunch {
                launch_nonce: "n".into(),
                uid: "term_held".into(),
                session_name: "cc-1".into(),
                coordinator: current_identity().unwrap(),
                boot: boot_identity().unwrap(),
                deadline_monotonic_nanos: far(),
                created_ms: 1,
            },
        )
        .unwrap();
        // Keep `lock` held across the terminalize attempt.
        let start = std::time::Instant::now();
        let r = terminalize_within(
            "term_held",
            "x",
            CleanupState::Pending,
            std::time::Duration::from_millis(150),
        );
        assert!(
            r.is_err(),
            "a held lock must make terminalize fail (a hard error), not hang"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "terminalize must return within its bounded deadline, never block forever"
        );
        drop(lock);
    }

    #[test]
    fn terminalize_own_orphan_reaps_our_pending_but_never_a_foreign_record() {
        // Finding 7: the create_pending-failure terminalization must reap only a
        // guardianless pending that is unambiguously ours (same coordinator +
        // nonce), never another coordinator's record.
        use protocol::proc_identity::BirthIdentity;
        let me = current_identity().unwrap();

        // Our own pending is reaped to Failed{cleanup:pending}.
        {
            let lock = LaunchLock::acquire("orphan_mine").unwrap();
            codex_launch::create_pending(
                &lock,
                NewLaunch {
                    launch_nonce: "mynonce".into(),
                    uid: "orphan_mine".into(),
                    session_name: "cc-1".into(),
                    coordinator: me,
                    boot: boot_identity().unwrap(),
                    deadline_monotonic_nanos: far(),
                    created_ms: 1,
                },
            )
            .unwrap();
        }
        terminalize_own_orphan("orphan_mine", &me, "mynonce", "boom").unwrap();
        let rec = codex_launch::load("orphan_mine").unwrap();
        assert!(matches!(rec.state, LaunchState::Failed { .. }));
        // A create_pending orphan never created a session ⇒ NotRequired (round-4).
        assert_eq!(rec.cleanup, CleanupState::NotRequired);

        // A record whose coordinator/nonce differ is NOT ours — left untouched.
        let other = ProcessIdentity {
            pid: 0x3FFF_FFAA,
            birth: BirthIdentity {
                start_sec: 7,
                start_usec: 7,
            },
        };
        {
            let lock = LaunchLock::acquire("orphan_other").unwrap();
            codex_launch::create_pending(
                &lock,
                NewLaunch {
                    launch_nonce: "othernonce".into(),
                    uid: "orphan_other".into(),
                    session_name: "cc-1".into(),
                    coordinator: other,
                    boot: boot_identity().unwrap(),
                    deadline_monotonic_nanos: far(),
                    created_ms: 1,
                },
            )
            .unwrap();
        }
        // We present OUR identity/nonce; the record's differ ⇒ untouched.
        terminalize_own_orphan("orphan_other", &me, "mynonce", "boom").unwrap();
        assert_eq!(
            codex_launch::load("orphan_other").unwrap().state,
            LaunchState::Pending,
            "a foreign coordinator's record must never be terminalized by us"
        );
    }

    #[test]
    fn sanitize_truncates_on_a_char_boundary_without_panicking() {
        // A reason whose byte length exceeds the cap but whose 300th byte lands
        // mid-character must truncate on a char boundary, never panic (the "Plus"
        // finding). Multibyte 'é' (2 bytes) repeated well past 300 chars.
        let reason = "é".repeat(400);
        let out = sanitize(&reason);
        assert!(out.ends_with('…'));
        // 300 kept chars + the ellipsis.
        assert_eq!(out.chars().count(), 301);
        // Control characters are flattened to spaces and the whole thing trimmed.
        assert_eq!(sanitize("  a\nb\t  "), "a b");
    }

    #[test]
    fn the_wait_reports_failed_reason_and_times_out_while_pending() {
        // A pending record that never terminates ⇒ the wait times out (but the
        // record is untouched — outcome still owned elsewhere).
        let lock = LaunchLock::acquire("c8").unwrap();
        codex_launch::create_pending(
            &lock,
            NewLaunch {
                launch_nonce: "n".into(),
                uid: "c8".into(),
                session_name: "cc-1".into(),
                coordinator: current_identity().unwrap(),
                boot: boot_identity().unwrap(),
                deadline_monotonic_nanos: far(),
                created_ms: 1,
            },
        )
        .unwrap();
        drop(lock);
        assert_eq!(wait_on_record("c8", ms(30), ms(5)), LaunchWait::TimedOut);
        // Now fail it and confirm the wait reports the sanitized reason.
        let lock = LaunchLock::acquire("c8").unwrap();
        codex_launch::to_failed(&lock, "c8", "line one\nline two", CleanupState::Pending).unwrap();
        drop(lock);
        assert_eq!(
            wait_on_record("c8", ms(200), ms(5)),
            LaunchWait::Failed("line one line two".into())
        );
    }

    #[test]
    fn a_ready_whose_entry_cannot_be_proven_durable_is_not_reported_ready() {
        // A9.6(a): `store_atomic` renames (Ready becomes visible) and only THEN
        // fsyncs the directory, and `commit_ready` cannot un-publish a rename
        // whose dir-fsync afterwards failed. So the consumer re-proves the entry
        // before acting on it. A session dir stripped of read permission (search
        // still allowed, so `load` still parses the record and still sees Ready)
        // makes that proof fail deterministically — and the wait must keep polling
        // to TimedOut rather than report a Ready it cannot vouch for.
        let mut deps = FakeDeps::default();
        assert_eq!(
            coordinate(setup("c10", far()), &mut deps).unwrap(),
            CoordinateOutcome::Ready
        );
        assert_eq!(wait_on_record("c10", ms(200), ms(5)), LaunchWait::Ready);

        let dir = codex_launch::session_dir("c10");
        set_mode(&dir, 0o100);
        if std::fs::File::open(&dir).is_ok() {
            // Root, or a filesystem that ignores the mode: the environment refuses
            // to create the condition, so there is nothing to assert.
            set_mode(&dir, 0o700);
            return;
        }
        assert_eq!(
            codex_launch::load("c10").unwrap().state,
            LaunchState::Ready,
            "the record is still visibly Ready — this pins the durability proof"
        );
        assert_eq!(wait_on_record("c10", ms(60), ms(5)), LaunchWait::TimedOut);
        set_mode(&dir, 0o700);
        // Provable again ⇒ Ready again.
        assert_eq!(wait_on_record("c10", ms(200), ms(5)), LaunchWait::Ready);
    }

    /// Seed a `pending` record for `uid` so the thread-binding wait has something
    /// to read. The three tests below drive it to each of that wait's exits.
    fn pending_record(uid: &str) {
        let lock = LaunchLock::acquire(uid).unwrap();
        codex_launch::create_pending(
            &lock,
            NewLaunch {
                launch_nonce: "n".into(),
                uid: uid.into(),
                session_name: "cc-1".into(),
                coordinator: current_identity().unwrap(),
                boot: boot_identity().unwrap(),
                deadline_monotonic_nanos: far(),
                created_ms: 1,
            },
        )
        .unwrap();
    }

    /// **The healthy launch's cost is the binding, not the grace.**
    ///
    /// Measured in production: the ccd leg's `session_start` lands 225 ms after the
    /// legs attach, and the broker's own binding — what the host records here — is
    /// earlier still. So the wait must return the instant the bit is on the record,
    /// not when [`codex_launch::THREAD_BINDING_GRACE`] runs out; a wait that only
    /// checked on expiry would add ten seconds to every launch that works.
    #[test]
    fn a_bound_thread_ends_the_wait_at_once() {
        pending_record("tb1");
        let lock = LaunchLock::acquire("tb1").unwrap();
        codex_launch::note_codex_thread_bound(&lock, "tb1").unwrap();
        drop(lock);
        let began = std::time::Instant::now();
        assert_eq!(await_thread_binding("tb1", far()), BringUp::Ready);
        assert!(
            began.elapsed() < codex_launch::THREAD_BINDING_GRACE,
            "a bound thread must not be made to wait out the grace"
        );
    }

    /// **The failure the host recorded is the failure the launcher prints.**
    ///
    /// The reason travels verbatim: `fail` reads it back off the record
    /// first-reason-wins, so what reaches the user's terminal is the host's own
    /// sentence about its dead TUI rather than a coordinator's wrapper around it.
    #[test]
    fn a_recorded_failure_ends_the_wait_with_that_reason() {
        pending_record("tb2");
        let lock = LaunchLock::acquire("tb2").unwrap();
        codex_launch::to_failed(
            &lock,
            "tb2",
            "the codex TUI exited without ever starting a thread",
            CleanupState::Pending,
        )
        .unwrap();
        drop(lock);
        assert_eq!(
            await_thread_binding("tb2", far()),
            BringUp::Failed("the codex TUI exited without ever starting a thread".into())
        );
    }

    /// **A RECORDED FAILURE WINS OVER THE BINDING BIT WHEN BOTH ARE SET.**
    ///
    /// `note_codex_thread_bound` is history and carries no `pending` guard, so
    /// `Failed { .. }` together with `codex_thread_bound: true` is a reachable state:
    /// a broker or app-server that dies after a thread bound but before the launch
    /// commits produces exactly it. Reading the bit first answered `Ready` for a
    /// launch whose failure was already on the record — the launcher would then
    /// `exec` into a dying pane, which is the defect this wait exists to prevent,
    /// reintroduced one layer up.
    ///
    /// **Mutation:** swap the two checks back and this fails with
    /// `left: Ready right: Failed("the broker died after the thread bound")`.
    #[test]
    fn a_recorded_failure_beats_the_binding_bit() {
        pending_record("tb4");
        let lock = LaunchLock::acquire("tb4").unwrap();
        codex_launch::note_codex_thread_bound(&lock, "tb4").unwrap();
        codex_launch::to_failed(
            &lock,
            "tb4",
            "the broker died after the thread bound",
            CleanupState::Pending,
        )
        .unwrap();
        drop(lock);
        let record = codex_launch::load("tb4").unwrap();
        assert!(
            record.codex_thread_bound && matches!(record.state, LaunchState::Failed { .. }),
            "the premise: both are set at once, which is why the ORDER of the two \
             checks is the thing under test"
        );
        assert_eq!(
            await_thread_binding("tb4", far()),
            BringUp::Failed("the broker died after the thread bound".into())
        );
    }

    /// **Running out of time is not a verdict.**
    ///
    /// No binding, no recorded failure, and the launch deadline already past: the
    /// pane is up and nothing has died, so this attaches exactly as it did before
    /// the wait existed. Turning "we did not hear" into a failed launch would break
    /// every slow-but-healthy launch in order to catch a fast broken one.
    #[test]
    fn an_expired_wait_still_attaches() {
        pending_record("tb3");
        let began = std::time::Instant::now();
        assert_eq!(
            await_thread_binding("tb3", monotonic_now_nanos().unwrap() - 1),
            BringUp::Ready
        );
        assert!(
            began.elapsed() < codex_launch::THREAD_BINDING_GRACE,
            "the launch deadline bounds the grace, not the other way round"
        );
    }

    /// **A SLOW-BUT-HEALTHY LAUNCH MUST STILL ATTACH.**
    ///
    /// The guard on the failure mode this whole wait could have introduced. A launch
    /// that is progressing normally and simply has not bound a thread yet — no
    /// binding, no recorded failure, and the launch deadline still far away — must
    /// come out of the grace as [`BringUp::Ready`] and attach. The alternative is to
    /// break every launch that is merely slow in order to catch a fast broken one,
    /// which is a strictly worse product than the silent-dead-pane bug being fixed:
    /// that one was rare, and this one would be every user on a cold cache.
    ///
    /// Distinct from [`an_expired_wait_still_attaches`], which exercises the OTHER
    /// exit — the launch deadline running out. This one leaves the deadline far away
    /// so the `grace_expires` branch is the branch that fires; that is why it is a
    /// separate test and not another assertion in that one.
    ///
    /// **It really does pay [`codex_launch::THREAD_BINDING_GRACE`]**, and the elapsed
    /// assertions are load-bearing in both directions: it must not return early (an
    /// early return would mean it never entered the branch this test is about) and it
    /// must not run past the grace (an unbounded wait is its own bug). The wall time
    /// is the price of testing a timeout, and this timeout is worth a test.
    ///
    /// **Mutation:** make the `grace_expires` branch return
    /// `BringUp::Failed(...)` — the "no news is bad news" reading — and this fails.
    #[test]
    fn a_launch_that_is_merely_slow_still_attaches_when_the_grace_runs_out() {
        pending_record("tb4");
        let began = std::time::Instant::now();
        assert_eq!(
            await_thread_binding("tb4", far()),
            BringUp::Ready,
            "a launch with no bad news must attach, not fail"
        );
        let waited = began.elapsed();
        assert!(
            waited >= codex_launch::THREAD_BINDING_GRACE,
            "the grace must actually be waited out, or this test proves nothing about \
             its expiry: waited {waited:?}"
        );
        assert!(
            waited < codex_launch::THREAD_BINDING_GRACE * 2,
            "the grace must also BOUND the wait: waited {waited:?}"
        );
    }

    fn set_mode(dir: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn an_absent_record_is_still_coming_up_not_a_failure() {
        // Nothing written yet: the wait must treat it as pending and time out,
        // never fabricate a Failed.
        assert_eq!(wait_on_record("never", ms(20), ms(5)), LaunchWait::TimedOut);
    }

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    // ------------------------------------------------------------ the charter

    /// A charter with every required dimension present, as `&str`s the tests
    /// extend or corrupt.
    fn full_charter() -> Vec<String> {
        [
            "--uid",
            "01JQXV9K7B8N4M2P6R3T5W9YQD",
            "--nonce",
            "0123456789abcdef0123456789abcdef",
            "--session-name",
            "cc-1",
            "--cwd",
            "/tmp",
            "--tmux-socket",
            "/tmp/t.sock",
            "--deadline-ms",
            "45000",
            "--codex",
            "/opt/codex/bin/codex",
            // A7.1: the identity of those bytes, as the launcher inspected them.
            "--codex-sha256",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "--codex-home",
            "/tmp/cch.home",
            "--approval-policy",
            "untrusted",
            "--approvals-reviewer",
            "user",
            "--sandbox",
            "read-only",
            "--hooks-enabled",
            "true",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// `full_charter()` with the value following `flag` replaced, or with the
    /// flag+value pair removed when `value` is `None`.
    fn charter_with(flag: &str, value: Option<&str>) -> Vec<String> {
        let mut args = full_charter();
        let at = args.iter().position(|a| a == flag).expect("flag present");
        match value {
            Some(v) => args[at + 1] = v.to_string(),
            None => {
                args.remove(at);
                args.remove(at);
            }
        }
        args
    }

    #[test]
    fn the_charter_requires_every_host_dimension_and_never_defaults_one() {
        // The eight values the host itself refuses to default. The coordinator
        // must refuse them too: it is the process a human is looking at, so a
        // missing dimension has to fail here rather than inside a pane.
        for flag in [
            "--uid",
            "--codex",
            "--codex-sha256",
            "--codex-home",
            "--approval-policy",
            "--approvals-reviewer",
            "--sandbox",
            "--hooks-enabled",
        ] {
            let err = parse_charter(&charter_with(flag, None))
                .expect_err(&format!("{flag} must be required, never defaulted"))
                .to_string();
            assert!(
                err.contains(flag),
                "the refusal must name the missing flag, got {err:?}"
            );
        }
        // And a full charter parses, carrying the values through verbatim.
        let charter = parse_charter(&full_charter()).expect("a full charter parses");
        assert_eq!(charter.codex, "/opt/codex/bin/codex");
        assert_eq!(
            charter.codex_sha256,
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(charter.codex_home, "/tmp/cch.home");
        assert_eq!(charter.approval_policy, "untrusted");
        assert_eq!(charter.approvals_reviewer, "user");
        assert_eq!(charter.sandbox, "read-only");
        assert!(charter.hooks_enabled);
        assert_eq!(charter.deadline_ms, 45_000);
        assert!(!charter.hang_in_new_session && !charter.hang_in_bringup);
    }

    /// The coordinator applies the host's `--codex` rule, at the charter, before it
    /// has built anything.
    ///
    /// It is the same function the host calls (`codex::require_absolute_codex`), for
    /// the same reason the two share the digest grammar: a coordinator that accepted
    /// a spelling the host refuses is not a lenient parser, it is a run directory, a
    /// tmux session and a launched host created for an invocation that was always
    /// going to die — and dying in a pane, where the operator is not looking.
    #[test]
    fn the_charter_refuses_a_codex_the_host_would_refuse_later() {
        // A bare name is the case that matters: the identity check opens `./codex`
        // and the spawn searches PATH, so the verify can pass about a file that is
        // not the one that runs.
        let err = parse_charter(&charter_with("--codex", Some("codex")))
            .expect_err("a bare --codex name must be refused at the charter")
            .to_string();
        assert!(err.contains("absolute"), "names the requirement: {err}");
        assert!(
            err.contains("PATH"),
            "and says why the two resolvers disagree: {err}"
        );

        // Relative spellings resolve alike for both, but make both depend on a cwd.
        for relative in ["./codex", "../bin/codex", "bin/codex"] {
            assert!(
                parse_charter(&charter_with("--codex", Some(relative))).is_err(),
                "--codex {relative:?} must be refused"
            );
        }

        // And the two parsers agree on the accepting case as well, which is the half
        // that would otherwise let them drift apart unnoticed.
        assert!(parse_charter(&full_charter()).is_ok());
    }

    #[test]
    fn the_charter_refuses_a_duplicate_rather_than_letting_the_last_one_win() {
        for (flag, second) in [
            ("--codex", "/other/codex"),
            ("--run-dir-is-not-a-flag-here", "x"),
            ("--sandbox", "danger-full-access"),
            ("--hooks-enabled", "false"),
            ("--uid", "01JQXV9K7B8N4M2P6R3T5W9YQE"),
        ] {
            if flag == "--run-dir-is-not-a-flag-here" {
                // The run dir is chosen by the coordinator, never accepted from
                // its charter — passing one is an unknown flag, not an override.
                let mut args = full_charter();
                args.push("--run-dir".into());
                args.push("/tmp/anything".into());
                assert!(
                    parse_charter(&args).is_err(),
                    "the run dir is the coordinator's to choose, not its caller's"
                );
                continue;
            }
            let mut args = full_charter();
            args.push(flag.into());
            args.push(second.into());
            let err = parse_charter(&args)
                .expect_err(&format!("a repeated {flag} must be refused"))
                .to_string();
            assert!(
                err.contains("more than once"),
                "the refusal must say the flag was repeated, got {err:?}"
            );
        }
    }

    #[test]
    fn the_charter_refuses_empty_flag_shaped_and_unknown_arguments() {
        // Empty and control-character values, and a value that is itself a flag —
        // the swallowing hole `value_of` exists to close.
        assert!(parse_charter(&charter_with("--sandbox", Some(""))).is_err());
        assert!(parse_charter(&charter_with("--codex-home", Some("a\nb"))).is_err());
        assert!(parse_charter(&charter_with("--codex", Some("--sandbox"))).is_err());
        // A dangling flag with no value at all.
        let mut dangling = full_charter();
        dangling.push("--sandbox".into());
        assert!(parse_charter(&dangling).is_err());
        // An unknown flag is refused rather than silently ignored, so a typo
        // cannot quietly drop a dimension the launch depends on.
        let mut unknown = full_charter();
        unknown.push("--poll-ms".into());
        unknown.push("100".into());
        let err = parse_charter(&unknown).unwrap_err().to_string();
        assert!(err.contains("--poll-ms"), "got {err:?}");
        // `--hooks-enabled` takes exactly true|false; nothing else is guessed.
        for bad in ["yes", "1", "True", ""] {
            assert!(
                parse_charter(&charter_with("--hooks-enabled", Some(bad))).is_err(),
                "--hooks-enabled {bad:?} must be refused, never defaulted"
            );
        }
        // The test-only hang injections accept ONLY "hang": there is no spelling
        // of either flag that makes bring-up report ready without evidence.
        for flag in ["--test-bringup", "--test-newsession"] {
            let mut ready = full_charter();
            ready.push(flag.into());
            ready.push("ready".into());
            assert!(
                parse_charter(&ready).is_err(),
                "{flag} must accept only \"hang\""
            );
            let mut hang = full_charter();
            hang.push(flag.into());
            hang.push("hang".into());
            let charter = parse_charter(&hang).expect("hang parses");
            assert!(charter.hang_in_new_session || charter.hang_in_bringup);
        }
    }

    #[test]
    fn passthrough_past_the_boundary_is_carried_and_still_meets_the_reserved_grammar() {
        let mut args = full_charter();
        args.extend(
            ["--", "hello world", "--search"]
                .iter()
                .map(|s| s.to_string()),
        );
        let charter = parse_charter(&args).expect("a benign passthrough is carried");
        assert_eq!(charter.tui_args, vec!["hello world", "--search"]);

        // A flag CodeConnect owns is refused here as well as at the host — the
        // coordinator is the process whose stderr a human can actually read.
        let mut owned = full_charter();
        owned.extend(["--", "--cd", "/elsewhere"].iter().map(|s| s.to_string()));
        let err = parse_charter(&owned).unwrap_err().to_string();
        assert!(
            err.contains("refused passthrough TUI argument"),
            "got {err:?}"
        );
    }

    // ----------------------------------------------------------- the run dir

    #[test]
    fn the_chosen_run_dir_is_short_enough_for_every_socket_the_host_binds() {
        let uid = "01JQXV9K7B8N4M2P6R3T5W9YQD";
        let nonce = codex_launch::mint_nonce();
        let dir = choose_run_dir(uid, &nonce).expect("a ULID + a minted nonce always fit");
        let worst = dir.as_os_str().len() + longest_host_socket_len();
        assert!(
            worst < crate::codex_host::SUN_LEN_LIMIT,
            "{}/<socket> is {worst} bytes, which must stay under SUN_LEN",
            dir.display()
        );
        // The measured worst case for a real launch, so a future change to the
        // name shape cannot quietly eat the headroom: prefix + 10 uid chars + a
        // dot + 16 nonce chars + the longest socket.
        assert_eq!(
            worst,
            RUN_DIR_PREFIX.len() + 10 + 1 + 16 + longest_host_socket_len()
        );
        assert_eq!(worst, 45);

        // The sizing is only sound while the host's socket list is the whole list,
        // so this reads the HOST's constant rather than restating the name. Adding
        // a longer leg there must move this number, not slip past it.
        assert_eq!(longest_host_socket_len(), "/tui.sock".len());
        assert!(crate::codex_host::SOCKET_NAMES.contains(&"tui.sock"));
        assert_eq!(crate::codex_host::SOCKET_NAMES.len(), 3);

        // Different per launch in practice — the nonce is 128 random bits and only
        // its first sixteen hex characters are used, so two launches of the same
        // uid land on different names with overwhelming probability. Note the
        // careful wording: this is not a uniqueness GUARANTEE (the derivation is
        // many-to-one), and the host's exclusive `mkdir` is what makes a collision
        // a refused launch rather than a shared directory.
        let again = choose_run_dir(uid, &codex_launch::mint_nonce()).unwrap();
        assert_ne!(dir, again);

        // Nothing is created here — the host owns the mkdir.
        assert!(!dir.exists(), "choose_run_dir must not create anything");
    }

    #[test]
    fn the_uid_slug_is_the_random_half_of_the_ulid_not_the_clock() {
        // A ULID is 10 timestamp chars + 16 random ones. Two sessions from the
        // same millisecond era share the prefix entirely, so a prefix slug names
        // a clock rather than a session. Taking the tail keeps two live run dirs
        // distinguishable in `ls /tmp`, which is the whole point of putting the
        // uid in the name at all.
        let nonce = "0123456789abcdef0123456789abcdef";
        let a = choose_run_dir("01JQXV9K7BAAAAAAAAAAAAAAAA", nonce).unwrap();
        let b = choose_run_dir("01JQXV9K7BBBBBBBBBBBBBBBBB", nonce).unwrap();
        assert_ne!(
            a, b,
            "two uids sharing a ULID timestamp must still get different run dirs"
        );
        assert_eq!(a.to_str().unwrap(), "/tmp/cch.AAAAAAAAAA.0123456789abcdef");
        // A uid shorter than the slug width is taken whole rather than padded.
        assert_eq!(
            choose_run_dir("abc", nonce).unwrap().to_str().unwrap(),
            "/tmp/cch.abc.0123456789abcdef"
        );
    }

    #[test]
    fn a_run_dir_name_can_never_escape_its_prefix() {
        // Path separators and dot-dots are filtered out of both components, so a
        // hostile uid or nonce cannot aim the directory — or the custodian's later
        // recursive removal of it — anywhere else.
        let dir = choose_run_dir("../../etc", "..//passwd").expect("filtered, not refused");
        let path = dir.to_str().unwrap();
        assert_eq!(path, "/tmp/cch.etc.passwd");
        assert!(!path.contains(".."), "no dot-dot may survive into the name");

        // A uid or nonce with nothing usable in it is refused rather than
        // collapsing to a shared, guessable name.
        assert!(choose_run_dir("///", "abcdef").is_err());
        assert!(choose_run_dir("01JQXV", "///").is_err());

        // The custodian's sweep guard is EQUALITY against this function's output
        // for the record's own uid+nonce, not a shape match on the path — so the
        // property that matters is that the same inputs always name the same
        // directory and different inputs never collide onto one.
        let mine = choose_run_dir("01JQXV9K7B8N4M2P6R3T5W9YQD", "aaaabbbbccccdddd").unwrap();
        assert_eq!(
            mine,
            choose_run_dir("01JQXV9K7B8N4M2P6R3T5W9YQD", "aaaabbbbccccdddd").unwrap(),
            "the guard is only usable if the derivation is deterministic"
        );
        assert_ne!(
            mine,
            choose_run_dir("01JQXV9K7B8N4M2P6R3T5W9YQE", "aaaabbbbccccdddd").unwrap()
        );
    }

    #[test]
    fn only_a_private_directory_this_launch_claimed_counts_as_a_run_dir() {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;
        let base = std::path::PathBuf::from(format!(
            "/tmp/cch-owntest-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let (uid, nonce) = ("01JQXV9K7B8N4M2P6R3T5W9YQD", "0123456789abcdef");
        let mk = |name: &str, mode: u32| -> std::path::PathBuf {
            let p = base.join(name);
            std::fs::DirBuilder::new().mode(mode).create(&p).unwrap();
            // `DirBuilder::mode` is filtered by the umask, which can only CLEAR
            // bits — so a 0755 fixture can silently come out 0700 and then pass
            // this test for the wrong reason (refused for its mode, when the mode
            // was never actually loose). Set it explicitly after creation, and
            // assert it below.
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        std::fs::create_dir_all(&base).unwrap();
        let good = mk("good", 0o700);
        let loose = mk("loose", 0o755);
        let unclaimed = mk("unclaimed", 0o700);
        let foreign = mk("foreign", 0o700);
        codex_launch::write_owner_marker(&good, uid, nonce).unwrap();
        codex_launch::write_owner_marker(&foreign, "01JQXV9K7B8N4M2P6R3T5W9YQE", nonce).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();

        assert!(
            run_dir_is_ours(&good, uid, nonce),
            "0700, ours, and carrying OUR marker is the only yes"
        );
        assert_eq!(
            std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
            0o755,
            "the loose fixture must really be 0755 or it proves nothing"
        );
        assert!(
            !run_dir_is_ours(&loose, uid, nonce),
            "a group/world-readable run dir must be refused: its 0700-ness is what makes \
             'a socket appeared here' mean anything"
        );
        assert!(
            !run_dir_is_ours(&link, uid, nonce),
            "a SYMLINK to a good dir must be refused — judged as a link, not followed"
        );
        assert!(
            !run_dir_is_ours(&file, uid, nonce),
            "a regular file is not a directory"
        );
        assert!(
            !run_dir_is_ours(&base.join("nope"), uid, nonce),
            "absent is not ours"
        );
        assert!(
            !run_dir_is_ours(&unclaimed, uid, nonce),
            "a correctly-shaped directory with NO marker is not ours: the shape is the \
             same for every launch, which is exactly why the marker exists"
        );
        assert!(
            !run_dir_is_ours(&foreign, uid, nonce),
            "a directory claimed by ANOTHER launch must never be read as ours — this is \
             the false-Ready the many-to-one name derivation makes possible"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_broker_leg_counts_only_when_something_is_actually_listening() {
        let base = std::path::PathBuf::from(format!(
            "/tmp/cch-legtest-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let plain = base.join("plain.sock");
        let live = base.join("live.sock");
        std::fs::write(&plain, b"not a socket").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&live).unwrap();

        assert!(
            !leg_is_serving(&base.join("absent.sock")),
            "an absent path is not a leg"
        );
        assert!(
            !leg_is_serving(&plain),
            "a REGULAR FILE at the leg's name must never be accepted as a bound socket"
        );
        // Retried within a small window rather than asserted on one probe. A single
        // connect is bounded by LEG_CONNECT_BUDGET, and on a loaded machine a local
        // connect can genuinely exceed it — which production handles by looking
        // again on the next poll, so a test that demands one-shot success is
        // asserting something the system never promises. (Observed: one failure
        // here across ~30 suite runs, only while the machine was saturated.)
        let serving_soon = |path: &std::path::Path| -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if leg_is_serving(path) {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert!(serving_soon(&live), "a bound, listening socket is a leg");

        // The case `stat` cannot see, and the reason readiness connects instead of
        // stat-ing: a socket INODE outlives the process that bound it. Drop the
        // listener and the file is still there, still a socket, still passing every
        // file-type check — and serving nothing.
        drop(listener);
        assert!(
            std::fs::symlink_metadata(&live).is_ok(),
            "the inode should still exist after the listener is gone"
        );
        assert!(
            !leg_is_serving(&live),
            "an unbound socket inode must not count as a serving leg — this is exactly \
             what a host killed abnormally leaves behind"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Bind a unix socket at `path` with an explicit, deliberately tiny backlog,
    /// and return its raw fd (closed by the caller).
    fn listener_with_backlog(path: &std::path::Path, backlog: i32) -> i32 {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket(): {}", std::io::Error::last_os_error());
            assert_eq!(
                libc::bind(fd, std::ptr::addr_of!(addr).cast(), len),
                0,
                "bind(): {}",
                std::io::Error::last_os_error()
            );
            // The whole point: a queue small enough that a handful of connects
            // provably fills it, rather than hoping the default depth is exceeded.
            assert_eq!(
                libc::listen(fd, backlog),
                0,
                "listen(): {}",
                std::io::Error::last_os_error()
            );
            fd
        }
    }

    /// A non-blocking connect attempt: `Ok(fd)` when it completed, `Err(errno)`
    /// otherwise. Used to FILL a listener's queue and to detect saturation.
    fn try_connect(path: &std::path::Path) -> Result<i32, i32> {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
            if libc::connect(fd, std::ptr::addr_of!(addr).cast(), len) == 0 {
                return Ok(fd);
            }
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            libc::close(fd);
            Err(errno)
        }
    }

    #[test]
    fn a_leg_probe_is_bounded_against_a_saturated_listener_and_withholds_on_this_platform() {
        // The hazard the bounded connect exists for: a broker whose accept loop has
        // stalled. The socket is bound and listening, so `stat` says "socket" and a
        // BLOCKING `connect` blocks once the backlog fills — inside a single
        // bring-up poll, past the launch deadline, defeating the one property that
        // loop has.
        //
        // Saturation is PROVEN here rather than assumed. An earlier version fired 64
        // connects at a default-backlog listener and hoped; the queue depth is
        // unspecified, so that fixture could silently test an unsaturated socket and
        // prove nothing. This binds the listener with an explicit backlog of 1 and
        // fills it with non-blocking connects until one is refused — and asserts
        // that refusal was observed before probing.
        let base = std::path::PathBuf::from(format!(
            "/tmp/cch-stall-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let stalled = base.join("stalled.sock");
        let listener = listener_with_backlog(&stalled, 1);

        let mut held = Vec::new();
        let mut saturation_errno = None;
        for _ in 0..256 {
            match try_connect(&stalled) {
                Ok(fd) => held.push(fd),
                // EAGAIN (queue full) or ECONNREFUSED (BSD reports a full unix
                // backlog this way) both mean the listener cannot take another —
                // which is saturation, and exactly the state under test.
                Err(errno) => {
                    saturation_errno = Some(errno);
                    break;
                }
            }
        }
        let errno = saturation_errno.expect(
            "the listener never refused a connection, so the queue was NEVER saturated and \
             this fixture would prove nothing about a stalled accept loop",
        );
        assert!(
            errno == libc::EAGAIN || errno == libc::ECONNREFUSED,
            "saturation should surface as EAGAIN or ECONNREFUSED, got errno {errno}"
        );

        // Now the real probe, on its own thread so the WAIT is what is bounded: a
        // regression to a blocking connect fails in seconds rather than hanging the
        // suite forever.
        let probe = stalled.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let verdict = leg_is_serving(&probe);
            let _ = tx.send((verdict, started.elapsed()));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok((verdict, took)) => {
                assert!(
                    took < std::time::Duration::from_secs(2),
                    "the leg probe took {took:?} against a saturated listener; it must be \
                     bounded by LEG_CONNECT_BUDGET, not by the listener's willingness to accept"
                );
                // The VERDICT is asserted, not discarded — and asserted against
                // what the kernel actually reports, which was measured rather than
                // assumed. On macOS a saturated AF_UNIX listener refuses with
                // `ECONNREFUSED`, the same errno as an unbound socket; `EAGAIN`
                // never appears. So the probe cannot tell a stalled broker from an
                // abandoned inode, and reads both as not-serving.
                //
                // Keyed off the observed errno rather than hardcoded, so this stays
                // correct on a platform that does report `EAGAIN` — and so the day
                // that changes, this fails and says why instead of drifting.
                if errno == libc::EAGAIN {
                    assert!(
                        verdict,
                        "EAGAIN is reportable only by a bound, listening socket, so the \
                         probe must read it as serving"
                    );
                } else {
                    assert!(
                        !verdict,
                        "a saturated listener refusing with ECONNREFUSED is indistinguishable \
                         from an unbound socket, so the probe must withhold — readiness then \
                         retries, which is the fail-closed direction"
                    );
                }
            }
            Err(_) => panic!(
                "the leg probe did not return within 5s against a saturated listener — the \
                 connect is BLOCKING again, which is the exact regression this test exists \
                 to catch"
            ),
        }

        for fd in held {
            unsafe { libc::close(fd) };
        }
        unsafe { libc::close(listener) };
        let _ = std::fs::remove_dir_all(&base);
    }

    // ------------------------------------------------------ the pane command

    fn real_deps(run_dir: &str) -> RealCoordinatorDeps {
        RealCoordinatorDeps {
            uid: "01JQXV9K7B8N4M2P6R3T5W9YQD".into(),
            session_name: "cc-1".into(),
            cwd: "/work".into(),
            tmux_socket: "/tmp/t.sock".into(),
            self_exe: std::path::PathBuf::from("/opt/cc/codeconnect"),
            custodian_nonce: "cust".into(),
            launch_nonce: "0123456789abcdef0123456789abcdef".into(),
            coordinator: current_identity().unwrap(),
            codex: "/opt/codex/bin/codex".into(),
            codex_sha256: "1111111111111111111111111111111111111111111111111111111111111111".into(),
            codex_home: "/tmp/cch.home".into(),
            approval_policy: "untrusted".into(),
            approvals_reviewer: "user".into(),
            sandbox: "read-only".into(),
            hooks_enabled: true,
            // Already canonicalized by `run_coordinator_inner` before it lands here.
            launch_cwd: "/work".into(),
            tui_args: vec![],
            run_dir: std::path::PathBuf::from(run_dir),
            deadline_monotonic_nanos: 0,
            session_a: None,
            hang_in_new_session: false,
            hang_in_bringup: false,
        }
    }

    #[test]
    fn the_pane_runs_the_real_host_with_an_exact_argv() {
        let deps = real_deps("/tmp/cch.01JQXV9K7B.0123456789abcdef");
        let argv = deps.new_session_argv();
        // Resolved, not hardcoded: the root is whatever this process's
        // `CODECONNECT_HOME`/`$HOME` resolve to, and the pane must be told exactly
        // that.
        let home_env = format!("CODECONNECT_HOME={}", protocol::root_dir().display());
        assert_eq!(
            argv,
            vec![
                "-S",
                "/tmp/t.sock",
                "new-session",
                "-d",
                "-s",
                "cc-1",
                "-c",
                "/work",
                "-e",
                // The uid stamp is injected exactly as before this chunk: an
                // `-e` ENV VAR on the session, not an @-option.
                "CODECONNECT_SESSION_UID=01JQXV9K7B8N4M2P6R3T5W9YQD",
                "-e",
                &home_env,
                "--",
                "/opt/cc/codeconnect",
                "internal-codex-host",
                "--uid",
                "01JQXV9K7B8N4M2P6R3T5W9YQD",
                "--nonce",
                "0123456789abcdef0123456789abcdef",
                "--tmux-socket",
                "/tmp/t.sock",
                "--codex",
                "/opt/codex/bin/codex",
                // A7.1: the identity travels beside the path, in the exact place
                // the host's parser expects it.
                "--codex-sha256",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "--run-dir",
                "/tmp/cch.01JQXV9K7B.0123456789abcdef",
                "--codex-home",
                "/tmp/cch.home",
                "--approval-policy",
                "untrusted",
                "--approvals-reviewer",
                "user",
                "--sandbox",
                "read-only",
                "--hooks-enabled",
                "true",
                // The fifth fingerprint dimension (round-2 P4): the CANONICAL launch cwd,
                // resolved once by the coordinator so the broker only ever compares strings.
                "--launch-cwd",
                "/work",
            ]
        );
        // A9.6(c): what the pane is told must be resolvable FROM THE PANE, and the
        // pane's cwd is the `-c` above — not this process's. A relative root would
        // make the two address different records; `protocol::root_dir()` is what
        // guarantees it cannot be relative, so this asserts the property rather
        // than the string.
        assert!(
            std::path::Path::new(home_env.trim_start_matches("CODECONNECT_HOME=")).is_absolute(),
            "the pane must be handed an absolute launch-record root: {home_env}"
        );

        // A label socket (no slash) still uses -L, unchanged by this chunk.
        let mut labelled = real_deps("/tmp/cch.a.b");
        labelled.tmux_socket = "codeconnect".into();
        assert_eq!(&labelled.new_session_argv()[..2], &["-L", "codeconnect"]);
    }

    // ROUND-2 P4 — the ONE canonicalization in the chain, and the measurement that forced
    // its placement. `/tmp` is a symlink on macOS, so the app-server resolves and reports
    // `/private/tmp`; exact equality of the raw strings is FALSE and realpath equality is
    // TRUE. Canonicalizing HERE — at the authority that owns the launch cwd, before the
    // path enters the host argv — is what lets the broker stay a pure string comparator.
    #[test]
    fn the_launch_cwd_is_canonicalized_once_here() {
        let raw = "/tmp";
        let canonical = canonical_launch_cwd(raw).expect("/tmp resolves");
        let reported_by_app_server = std::fs::canonicalize(raw)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            canonical, reported_by_app_server,
            "the coordinator must hand the broker the SAME spelling the app-server reports"
        );
        // The measurement itself: on this platform the raw and resolved spellings differ,
        // so a broker doing exact equality against the RAW path would refuse every turn.
        if canonical != raw {
            assert_ne!(
                canonical, raw,
                "measured: /tmp resolves to a different path ({canonical})"
            );
        }
        // Fail closed: a launch cwd that cannot be resolved aborts the launch rather than
        // producing a session whose broker can never verify a thread creation.
        assert!(canonical_launch_cwd("/definitely/not/a/real/dir/xyzzy").is_err());
        let file = std::env::temp_dir().join("cc-launch-cwd-probe");
        std::fs::write(&file, b"x").unwrap();
        assert!(
            canonical_launch_cwd(file.to_str().unwrap()).is_err(),
            "a regular file is not a workspace"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn the_pane_argv_is_one_the_host_itself_accepts() {
        // The differential check: the coordinator writes this charter, a separate
        // process reads it. Feed the argv the pane will actually run back into the
        // HOST's own parser — not a restatement of it — so the two cannot drift.
        let mut deps = real_deps("/tmp/cch.a.b");
        deps.tui_args = vec!["a prompt".into()];
        let argv = deps.host_argv();
        assert_eq!(argv[1], "internal-codex-host");
        crate::codex_host::parse_host_args(&argv[2..])
            .expect("the host must accept the charter the coordinator writes");

        // The passthrough boundary is only emitted when there is something past
        // it, and what follows it is still subject to the reserved grammar.
        assert!(deps.host_argv().contains(&"--".to_string()));
        let none = real_deps("/tmp/cch.a.b");
        assert!(!none.host_argv().contains(&"--".to_string()));
        let mut owned = real_deps("/tmp/cch.a.b");
        owned.tui_args = vec!["--cd".into(), "/elsewhere".into()];
        assert!(
            crate::codex_host::parse_host_args(&owned.host_argv()[2..]).is_err(),
            "an owned flag must be refused by the host even if it reached the argv"
        );
    }

    // --------------------------------------------------------- the bring-up

    #[test]
    fn bringup_reports_ready_only_when_both_facts_are_proven() {
        use protocol::tmux::OwnedLiveness;
        let observe = |bound: bool, present: bool, session: Option<OwnedLiveness>| {
            bringup_step(&BringupObservation {
                sockets_bound: bound,
                // The lease-live precondition has its own case below; the rest of
                // the table holds it true so each row varies one thing.
                host_live: bound,
                run_dir_present: present,
                session,
            })
        };

        // The only Ready: both legs bound AND the pane proven ours.
        assert!(matches!(
            observe(true, true, Some(OwnedLiveness::Live)),
            BringupStep::Ready
        ));

        // Sockets bound but the census could not answer: NOT ready. A durable
        // `ready` is what a launcher attaches to, so an unprovable pane waits.
        assert!(matches!(
            observe(true, true, Some(OwnedLiveness::Unknown("no server".into()))),
            BringupStep::KeepWaiting
        ));
        // Sockets bound but nothing was censused this pass: also not ready.
        assert!(matches!(
            observe(true, true, None),
            BringupStep::KeepWaiting
        ));
        // A live pane that has not bound its legs yet is simply still coming up.
        assert!(matches!(
            observe(false, true, Some(OwnedLiveness::Live)),
            BringupStep::KeepWaiting
        ));
        // An unprovable census with nothing bound: no verdict either way.
        assert!(matches!(
            observe(false, false, Some(OwnedLiveness::Unknown("hiccup".into()))),
            BringupStep::KeepWaiting
        ));

        // Serving legs and a live pane are NOT enough on their own: the process
        // that owns them must be proven alive. A socket inode outlives an abnormal
        // exit and a tmux session outlives its pane command, so without the lease
        // this row would commit a durable `ready` for a dead host.
        assert!(matches!(
            bringup_step(&BringupObservation {
                sockets_bound: true,
                host_live: false,
                run_dir_present: true,
                session: Some(OwnedLiveness::Live),
            }),
            BringupStep::KeepWaiting
        ));

        // The two proven-loss failures, each naming what it saw.
        let bound_but_gone = observe(true, true, Some(OwnedLiveness::Gone));
        match bound_but_gone {
            BringupStep::Failed(why) => assert!(why.contains("pane's tmux session is gone")),
            _ => panic!("a gone pane under bound sockets must fail the launch"),
        }
        match observe(false, false, Some(OwnedLiveness::Gone)) {
            BringupStep::Failed(why) => assert!(
                why.contains("never created"),
                "the reason must say the run dir was never created: {why}"
            ),
            _ => panic!("a gone pane with no run dir must fail the launch"),
        }
        match observe(false, true, Some(OwnedLiveness::Gone)) {
            BringupStep::Failed(why) => assert!(
                why.contains("created but empty"),
                "the reason must distinguish a created-but-empty run dir: {why}"
            ),
            _ => panic!("a gone pane with an empty run dir must fail the launch"),
        }
    }

    #[test]
    fn a_bringup_that_proves_nothing_fails_at_the_deadline_and_never_at_ready() {
        // A run dir that will never be created, against a tmux socket no server
        // answers, with a deadline already in the past: the wait must end — and it
        // must end as a failure, because nothing was ever proven.
        let mut deps = real_deps("/tmp/cch.nosuch.deadline");
        deps.tmux_socket = "/tmp/cc-no-such-server.sock".into();
        deps.session_a = Some(fake_owned());
        deps.deadline_monotonic_nanos = monotonic_now_nanos().unwrap().saturating_sub(1);
        let started = std::time::Instant::now();
        match deps.bring_up_wrapper() {
            BringUp::Failed(why) => {
                assert!(
                    why.contains("deadline") || why.contains("gone"),
                    "the failure must say why: {why}"
                );
            }
            BringUp::Ready => panic!("bring-up must never report ready without evidence"),
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "a passed deadline must end the wait promptly"
        );
    }

    #[test]
    fn bringup_itself_refuses_a_run_dir_another_launch_claimed() {
        // Drives the PRODUCTION path — `bring_up_wrapper`, not the helper — against
        // the fixture that matters: a correctly-shaped, correctly-named run dir
        // with SERVING broker legs that belongs to somebody else.
        //
        // This is the false-`Ready` the many-to-one name derivation makes
        // reachable: host A's live sockets standing where this launch expects its
        // own, while host B (paused between taking its lease and its own `mkdir`)
        // supplies the live lease. Everything the old readiness looked at is
        // present and correct. Only the owner marker disagrees.
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let run = std::path::PathBuf::from(format!(
            "/tmp/cch-collide-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Real, serving legs — the strongest evidence readiness has.
        let _tui = std::os::unix::net::UnixListener::bind(run.join("tui.sock")).unwrap();
        let _ccd = std::os::unix::net::UnixListener::bind(run.join("ccd.sock")).unwrap();
        // …claimed by a DIFFERENT launch.
        codex_launch::write_owner_marker(&run, "01JQXV9K7B8N4M2P6R3T5W9YQZ", "someone-elses-nonce")
            .unwrap();

        let mut deps = real_deps(run.to_str().unwrap());
        deps.session_a = Some(fake_owned());
        // A deadline far enough out that a `Ready` would have every chance to fire.
        deps.deadline_monotonic_nanos = monotonic_now_nanos().unwrap() + 1_500_000_000;
        match deps.bring_up_wrapper() {
            BringUp::Ready => panic!(
                "bring-up committed READY against a run dir claimed by another launch — \
                 serving sockets under someone else's directory are not this launch's host"
            ),
            BringUp::Failed(why) => assert!(
                why.contains("deadline") || why.contains("gone"),
                "it should time out having never accepted the evidence: {why}"
            ),
        }

        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn a_bringup_with_no_pinned_session_refuses_rather_than_judging_unpinned() {
        // `new_session` sets the pin; without it there is no server identity to
        // bind the census to, and an unpinned census could be answered by a
        // different server that rebound the socket. Fail closed.
        let mut deps = real_deps("/tmp/cch.a.b");
        deps.deadline_monotonic_nanos = far();
        match deps.bring_up_wrapper() {
            BringUp::Failed(why) => assert!(why.contains("unpinned"), "got {why}"),
            BringUp::Ready => panic!("an unpinned bring-up must never report ready"),
        }
    }
}
