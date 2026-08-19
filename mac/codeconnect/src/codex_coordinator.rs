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
//!   4. Bring the wrapper up (the 2d work; here an injected step because the
//!      `codex` command is still gated). Re-check the custodian and the deadline
//!      before committing `ready`.
//!
//! The wrapper bring-up is a seam (`CoordinatorDeps::bring_up_wrapper`) so this
//! chunk can drive every boundary in a test without a live Codex/app-server.

use crate::codex_launch::{
    self, deadline_expiry, CleanupState, Expiry, LaunchLock, LaunchRecord, LaunchState, NewLaunch,
};
use anyhow::{Context, Result};
use protocol::proc_identity::{liveness, Liveness, ProcessIdentity};
use protocol::tmux::OwnedSession;

/// The outcome of the coordinator's forward `tmux new-session`.
pub enum NewSessionOutcome {
    /// The session was created and resolved to a pinned [`OwnedSession`].
    Created(Box<OwnedSession>),
    /// tmux ran past its deadline: **indeterminate**. The coordinator records
    /// failure with the indeterminate flag and hands off to the custodian.
    Indeterminate,
    /// tmux answered with a definite failure.
    Failed(String),
}

/// What the wrapper bring-up reported (2d). Injected in this chunk.
pub enum BringUp {
    Ready,
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
    /// Bring the wrapper up and validate its thread evidence (2d seam).
    fn bring_up_wrapper(&mut self) -> BringUp;
    /// Liveness of the armed custodian. Defaulted to the real kernel check.
    fn custodian_liveness(&self, id: &ProcessIdentity) -> Liveness {
        liveness(id)
    }
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

/// Drive the record to `failed{cleanup:pending}` **and durably disarm the
/// indeterminate flag**, within a bounded deadline.
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
fn terminalize(uid: &str, reason: &str) -> Result<()> {
    // Conservative default: a session MAY exist (used by the unexpected-error
    // catch), so leave cleanup Pending.
    terminalize_within(
        uid,
        reason,
        CleanupState::Pending,
        std::time::Duration::from_secs(5),
    )
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
    {
        let lock = LaunchLock::acquire(&uid)?;
        codex_launch::mark_new_session_starting(&lock, &uid)?;
    }
    match deps.new_session() {
        NewSessionOutcome::Created(session) => {
            // Determinate: the flag can be cleared. And PERSIST server A (round-5
            // finding 1) — the resolved session+server identity — so the separate
            // custodian and supervisor bind cleanup/liveness to it instead of
            // re-establishing "A" from whatever server owns the socket later. A
            // session without a proven server birth is fail-closed (finding 3).
            let a = codex_launch::ServerA::from_owned(&session)
                .context("recording server A from the created session")?;
            let lock = LaunchLock::acquire(&uid)?;
            codex_launch::clear_new_session_indeterminate(&lock, &uid)?;
            codex_launch::record_server_a(&lock, &uid, a)?;
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

    // Step 4: bring the wrapper up (2d) and commit. `to_ready` itself re-checks,
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
/// `LaunchWait`/[`wait_on_record`] are the **launcher** half of D7 — consumed by
/// `codeconnect codex` once it is ungated (2d), which spawns the coordinator and
/// then only waits here. Built and unit-tested now (the machinery this chunk
/// delivers); dispatched by the ungated launcher later.
#[allow(dead_code)]
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
#[allow(dead_code)] // launcher-side (2d ungate); unit-tested here.
pub fn wait_on_record(
    uid: &str,
    patience: std::time::Duration,
    poll: std::time::Duration,
) -> LaunchWait {
    let deadline = std::time::Instant::now() + patience;
    loop {
        if let Ok(record) = codex_launch::load(uid) {
            match record.state {
                LaunchState::Ready => return LaunchWait::Ready,
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
#[allow(dead_code)] // launcher-side (2d ungate); unit-tested via wait_on_record.
fn sanitize(reason: &str) -> String {
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

/// The wrapper bring-up mode. In this chunk the real wrapper (2d) is not built,
/// so the coordinator's bring-up is a seam the launcher/tests choose: `Ready`
/// commits, `Fail` fails the launch, `Hang` blocks forever (used by the
/// kill-at-boundary integration test to hold the coordinator after
/// `new-session` so it can be killed there). The default is `Fail` — a build
/// with no wrapper must never claim `ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BringupMode {
    Ready,
    Fail,
    Hang,
}

impl BringupMode {
    fn parse(s: &str) -> BringupMode {
        match s {
            "ready" => BringupMode::Ready,
            "hang" => BringupMode::Hang,
            _ => BringupMode::Fail,
        }
    }
}

/// The real forward-launch operations, backed by the exec gate + tmux.
pub struct RealCoordinatorDeps {
    pub uid: String,
    pub session_name: String,
    pub cwd: String,
    pub tmux_socket: String,
    pub gate_program: std::path::PathBuf,
    pub custodian_nonce: String,
    pub coordinator: ProcessIdentity,
    pub bringup: BringupMode,
    /// Test-only (Principle B): hang **inside** `new_session`, after the tmux
    /// session is created and resolved but **before returning** — so the
    /// integration test can SIGKILL the coordinator while it is literally still
    /// in the new-session call, with `new_session_indeterminate` durably set. A
    /// coordinator killed "with tmux in flight" must leave the custodian armed to
    /// clean the (late) session. Never set on a real launch.
    pub hang_in_new_session: bool,
}

impl RealCoordinatorDeps {
    fn server_args(&self) -> Vec<String> {
        if self.tmux_socket.contains('/') {
            vec!["-S".into(), self.tmux_socket.clone()]
        } else {
            vec!["-L".into(), self.tmux_socket.clone()]
        }
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
            self.gate_program.clone(),
            &self.custodian_nonce,
        )
    }

    fn new_session(&mut self) -> NewSessionOutcome {
        let Some(bin) = protocol::tmux::tmux_bin() else {
            return NewSessionOutcome::Failed("tmux not found".into());
        };
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
        // A placeholder pane stands in for the 2d `codex-host`; the custodian
        // cleans the session up regardless of what runs inside it.
        argv.extend(
            ["--", "/bin/sh", "-c", "while :; do sleep 1; done"]
                .iter()
                .map(|s| s.to_string()),
        );
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
        // Resolve the created session by uid to pin it.
        let resolved = match protocol::tmux::resolve_owned_session(&self.tmux_socket, &self.uid) {
            Ok(session) => session,
            // The session was created (tmux said success) but we cannot resolve
            // it: treat as indeterminate so the custodian owns the outcome.
            Err(_) => return NewSessionOutcome::Indeterminate,
        };
        // Test-only Principle B window: the session now exists and the record
        // still says `new_session_indeterminate` (set before this call). Hang so
        // the test can kill us here — "coordinator killed with tmux in flight".
        if self.hang_in_new_session {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        NewSessionOutcome::Created(Box::new(resolved))
    }

    fn bring_up_wrapper(&mut self) -> BringUp {
        match self.bringup {
            BringupMode::Ready => BringUp::Ready,
            BringupMode::Fail => {
                BringUp::Failed("wrapper bring-up is not built in this chunk (2d)".into())
            }
            BringupMode::Hang => loop {
                // Block forever: the integration test kills the coordinator here,
                // after new-session, to prove the custodian owns the outcome.
                std::thread::sleep(std::time::Duration::from_secs(3600));
            },
        }
    }
}

/// The `internal-codex-coordinator` subcommand. Parses the launcher's charter,
/// runs [`coordinate`], and exits 0 (the outcome lives in the durable record;
/// the launcher reads it there, not from this process's exit).
pub fn run_coordinator(args: &[String]) -> ! {
    match run_coordinator_inner(args) {
        Ok(_) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}

fn run_coordinator_inner(args: &[String]) -> Result<CoordinateOutcome> {
    let mut uid = None;
    let mut nonce = None;
    let mut custodian_nonce = None;
    let mut session_name = None;
    let mut cwd = None;
    let mut socket = None;
    let mut deadline_ms: Option<u64> = None;
    let mut bringup = BringupMode::Fail;
    let mut hang_in_new_session = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--uid" => uid = it.next().cloned(),
            "--nonce" => nonce = it.next().cloned(),
            "--custodian-nonce" => custodian_nonce = it.next().cloned(),
            "--session-name" => session_name = it.next().cloned(),
            "--cwd" => cwd = it.next().cloned(),
            "--tmux-socket" => socket = it.next().cloned(),
            "--deadline-ms" => deadline_ms = it.next().and_then(|v| v.parse().ok()),
            "--test-bringup" => {
                bringup = it
                    .next()
                    .map(|s| BringupMode::parse(s))
                    .unwrap_or(BringupMode::Fail)
            }
            // Test-only (Principle B): hang inside new-session after the session
            // is created, so the coordinator can be killed "with tmux in flight".
            "--test-newsession" => {
                hang_in_new_session = it.next().map(|s| s == "hang").unwrap_or(false)
            }
            _ => {}
        }
    }
    let uid = uid.context("--uid required")?;
    let coordinator = codex_launch::require_current_identity()?;
    let boot = protocol::proc_identity::boot_identity().context("reading boot identity")?;
    let now = protocol::proc_identity::monotonic_now_nanos().context("reading monotonic clock")?;
    let deadline = now + deadline_ms.unwrap_or(30_000) * 1_000_000;
    let mut deps = RealCoordinatorDeps {
        uid: uid.clone(),
        session_name: session_name.clone().unwrap_or_else(|| "cc-codex".into()),
        cwd: cwd.unwrap_or_else(|| "/".into()),
        tmux_socket: socket.unwrap_or_else(|| protocol::TMUX_SOCKET_NAME.to_string()),
        gate_program: std::env::current_exe().context("locating this binary")?,
        custodian_nonce: custodian_nonce.unwrap_or_else(codex_launch::mint_nonce),
        coordinator,
        bringup,
        hang_in_new_session,
    };
    coordinate(
        CoordinateSetup {
            uid,
            launch_nonce: nonce.unwrap_or_else(codex_launch::mint_nonce),
            session_name: session_name.unwrap_or_else(|| "cc-codex".into()),
            coordinator,
            boot,
            deadline_monotonic_nanos: deadline,
            created_ms: protocol::time::now_unix_ms(),
        },
        &mut deps,
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
            }
        }
    }
    impl CoordinatorDeps for FakeDeps {
        fn spawn_custodian(&mut self, uid: &str) -> Result<ProcessIdentity> {
            if self.spawn_custodian_fails {
                anyhow::bail!("gate refused the custodian");
            }
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
            self.bring_up
                .take()
                .unwrap_or(BringUp::Failed("no scripted bring-up".into()))
        }
        fn custodian_liveness(&self, _id: &ProcessIdentity) -> Liveness {
            if self.custodian_alive {
                Liveness::Alive
            } else {
                Liveness::Gone
            }
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
        // no guardian (the "Plus" finding): it is terminalized to
        // Failed{cleanup:pending} so the recovery sweep can rearm a custodian and
        // finish cleanup.
        let out = coordinate(setup("c7", far()), &mut deps).unwrap();
        assert!(matches!(out, CoordinateOutcome::Failed(_)));
        let rec = codex_launch::load("c7").unwrap();
        assert!(
            matches!(rec.state, LaunchState::Failed { .. }),
            "the record must be terminalized, not left Pending"
        );
        assert_eq!(rec.cleanup, CleanupState::Pending);
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
    fn an_absent_record_is_still_coming_up_not_a_failure() {
        // Nothing written yet: the wait must treat it as pending and time out,
        // never fabricate a Failed.
        assert_eq!(wait_on_record("never", ms(20), ms(5)), LaunchWait::TimedOut);
    }

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }
}
