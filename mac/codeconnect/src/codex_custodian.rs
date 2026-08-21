//! The **launch custodian** (D7) — independent cleanup authority, armed before
//! `tmux new-session`.
//!
//! The custodian is a minimal detached process (brought up through the D6 exec
//! gate) that outlives the coordinator's forward path. Its whole reason to exist
//! is that a timed-out or coordinator-killed launch must still reach a terminal
//! outcome and have its disposable tmux session cleaned — by someone whose
//! authority does not depend on the coordinator still breathing.
//!
//! ## What it does each pass (the `tick`)
//!
//!   * **`pending`** — monitor the deadline and the coordinator's birth
//!     identity. Coordinator loss or deadline expiry ⇒ CAS `pending →
//!     failed{cleanup:pending}` (the transition the resumed-but-stopped
//!     coordinator can then never overwrite).
//!   * **`ready`** — coordinator/supervisor loss is **session-fatal**: the
//!     custodian performs the bounded teardown of the live session.
//!   * **`failed{cleanup:pending}`** — resolve the uid → internal id + server
//!     epoch, revalidate, and kill only that id. `Unavailable` is **never**
//!     absence (retry). After an **indeterminate** `new-session`, one
//!     observation of UID absence is **insufficient** (no guessed grace period,
//!     D7): the custodian stays armed until it observes and cleans the late UID
//!     or the tmux server's identity changes.
//!
//! The `tick` is written against a [`CustodianDeps`] trait so every branch is
//! unit-tested without real processes or a live tmux; [`run`] wires the real
//! kernel + tmux behind it and loops.
//!
//! ## What cleanup covers, and the orphan story (2e-2b)
//!
//! Since the pane runs the real `internal-codex-host`
//! ([`crate::codex_host`]), killing the session is not the whole of cleanup —
//! there is a run directory and there are two codex children. Traced end to end:
//!
//!   * **The session kill reaches the host.** tmux ends a pane by hanging up its
//!     pty, and the host installs a SIGHUP handler precisely because it is
//!     tmux-hosted. A *live* host therefore runs its single teardown path on the
//!     kill: stop both children under a bound, and remove the run dir it created.
//!     This is the path that covers the overwhelming majority of teardowns, and
//!     the one the live gates exercise.
//!   * **Recorded children are stopped by identity.** The host records the
//!     app-server and the TUI as `(pid, birth, pgid)` before the session can be
//!     declared ready, so cleanup addresses them directly rather than hoping a
//!     signal aimed elsewhere arrives. That is what closes the hangup race below
//!     for a host that never runs teardown at all.
//!   * **The run-dir sweep is the backstop.** Two cases leave a directory the
//!     host will never remove: it was SIGKILLed (or aborted — the release profile
//!     is `panic = "abort"`, so a panicking broker takes the process down with no
//!     destructors), or it never ran at all because the pane died first. Neither
//!     leaves anything in-process to clean up, so the custodian removes the
//!     recorded path itself, **after** the session kill and best-effort, via
//!     [`CustodianDeps::sweep_run_dir`]. The path comes from the launch record,
//!     which the coordinator fsyncs *before* `tmux new-session` — so even a
//!     coordinator killed with tmux in flight leaves the directory nameable.
//!
//! ### The residual, and what actually closes it. MEASURED.
//!
//! An earlier draft claimed that because the host spawns both children into its
//! own process group, one `kill-session` reaches the host *and* the app-server
//! *and* the TUI. **Both halves were wrong.** The app-server is deliberately in
//! its OWN process group (`process_group(0)`, so cleanup can address a recorded
//! pgid); only the TUI shares the host's, because it must stay in the pane's
//! foreground group or lose the keyboard. And the signal claim was measured false:
//! killing the host under an otherwise-live tmux server left the `app-server`
//! running roughly one time in ten, permanently, more often under load.
//!
//! Two hypotheses were tested and **falsified**, so they are not the cause: the
//! children do not inherit a blocked SIGHUP (measured: empty blocked set), and
//! they do not inherit an ignored disposition (measured: `SIG_DFL`). What is left
//! is delivery. The hangup those children rely on is not one this custodian sends
//! — it is the kernel's, raised when the pane's **session leader** (the host) dies
//! with a controlling terminal, and it races tmux's own teardown of that pty. Lose
//! the race and no signal is ever raised for the group.
//!
//! So a SIGKILLed host does **not** get "only its run dir back", as this used to
//! say. It gets its run dir back from the sweep AND its children stopped by
//! recorded identity — see [`CustodianDeps::teardown_children`], which signals
//! each recorded `(pid, birth, pgid)` and kills a recorded GROUP whose leader has
//! already gone, since a descendant it forked is a member in its own right and
//! outlives it. The signal race is no longer load-bearing.
//!
//! Nothing here hunts processes by name, pid file, or argv scan to close that
//! gap, and deliberately so: a same-uid process table is not a boundary, and
//! cleanup that guesses at identity is how the wrong thing gets killed. What
//! closes it instead is evidence: the host records each child as
//! [`crate::codex_launch::ChildEntry`] — `(pid, birth, pgid)` — before the launch
//! can be declared ready, and [`CustodianDeps::teardown_children`] signals those
//! verified identities, killing a recorded GROUP whose leader has already gone
//! (a descendant it forked is a member in its own right and outlives it). That is
//! not process hunting, because every identity was durably recorded rather than
//! inferred.
//!
//! The residual is narrower than the paragraph above once described: a host
//! SIGKILLed in the window between spawning a child and recording it leaves that
//! child unrecorded. The window is minimised (see `codex_host`'s
//! `RECORD_LOCK_BUDGET`) and closing it entirely is a documented pre-ungate gate.
//!
use crate::codex_launch::{
    self, deadline_expiry, CleanupState, Expiry, LaunchLock, LaunchRecord, LaunchState,
};
use anyhow::Result;
use protocol::proc_identity::{liveness, Liveness, ProcessIdentity};
use protocol::tmux::CleanupOutcome;

/// One pass's verdict. Terminal variants stop the loop; the rest sleep + retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Healthy `pending`/`ready`; nothing owed this pass.
    Idle,
    /// Just CAS'd `pending → failed` (deadline or coordinator loss). Cleanup
    /// happens on the following pass.
    FailedPending(&'static str),
    /// Cleanup killed the session (or found the server gone) — done.
    CleanedAndDone,
    /// The session was already absent and the outcome was determinate — done.
    AlreadyAbsentDone,
    /// `ready` and the coordinator/supervisor is gone: session-fatal teardown
    /// performed — done.
    ReadyFatalTeardown,
    /// UID absent but the `new-session` was indeterminate: stay armed (D7).
    StayArmedIndeterminate,
    /// tmux was Unavailable/Ambiguous — retry next pass (never treated as done).
    RetryCleanup,
    /// The record is already terminal and clean — done.
    Done,
}

impl Tick {
    /// Whether this verdict ends the custodian's life.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Tick::CleanedAndDone | Tick::AlreadyAbsentDone | Tick::ReadyFatalTeardown | Tick::Done
        )
    }
}

/// The operations one custodian pass needs. A trait for testability.
pub trait CustodianDeps {
    fn load(&self) -> Result<LaunchRecord>;
    fn coordinator_liveness(&self) -> Liveness;
    fn deadline(&self, record: &LaunchRecord) -> Expiry;
    /// UID-atomic destroy of the uid's session (resolve → epoch-guard → kill).
    fn destroy(&self) -> CleanupOutcome;
    /// Whether this custodian has, on any pass so far, **positively observed**
    /// the launch's uid present on a live session.
    ///
    /// It exists for exactly one decision: after an indeterminate `new-session`,
    /// D7 says one observation of absence is not proof, because the late session
    /// might still be coming. That reasoning stops applying the moment the late
    /// session has actually been seen — it came, and what follows is the custodian
    /// killing it. Without this, a kill the custodian could not *confirm* in a
    /// single pass (the pane takes real time to die now that it runs a real host)
    /// reads as `Absent` on the next pass and the custodian stays armed forever.
    fn seen_session_present(&self) -> bool;
    /// Whether the **recorded server A** is provably dead — pid and birth, never
    /// socket reachability (the 2c invariant).
    /// Whether the server that hosted this session is provably gone — bound to a
    /// recorded identity, never to socket reachability. See
    /// [`server_gone_evidence`] for the two bindings and why each is safe.
    fn server_gone_evidence(&self, record: &LaunchRecord) -> bool;
    /// A freshly loaded record, for the destructive step.
    ///
    /// [`tick`] reads the record once and reasons about it; by the time cleanup
    /// actually signals processes and deletes a directory, that snapshot can be
    /// several tmux probes old — and in between, a host may have taken a lease,
    /// recorded children, or written its owner marker. Acting on the stale copy
    /// would mean signalling identities that are no longer authorised, or skipping
    /// ones that are. So the destructive step re-reads.
    fn current_record(&self) -> Result<LaunchRecord>;
    fn cas_failed(&self, reason: &str) -> Result<()>;
    /// Removal of the recorded run dir, called only from [`complete_cleanup`] —
    /// i.e. **after** the session has been killed, so the SIGHUP that reaches a
    /// live host has already given it the chance to remove its own directory.
    ///
    /// It **can** stop the record from reaching `Complete`, and that is the whole
    /// point of the return value. An earlier contract said the opposite — that a
    /// failed removal must never hold cleanup up, because a stranded directory is
    /// a smaller problem than a custodian that will not finish. The two halves of
    /// that sentence were both written here and they contradict: `Complete` is
    /// exactly what stops a later `recovery_sweep` from rearming anyone, so
    /// writing it over a directory this cleanup could not deal with converts a
    /// retriable failure into a permanent leak with nobody left to notice. A
    /// custodian that keeps retrying is visible and self-correcting; a `Complete`
    /// written too early is neither.
    ///
    /// Takes the record the caller already loaded rather than re-reading it. That
    /// is not an optimisation: a re-read can fail transiently (mid-rename), and a
    /// sweep that quietly skipped on that error while `complete_cleanup` went on to
    /// write the `Complete` marker would strand the directory **permanently** —
    /// `Complete` is precisely what stops a later `recovery_sweep` from rearming
    /// anyone to look again.
    ///
    /// Returns whether the directory is **dealt with** — nothing further owed —
    /// which covers more than removal:
    ///
    ///   * removed, or already absent: the ordinary successes;
    ///   * provably **not ours** — a path this launch could not have derived, or
    ///     one whose owner marker was READ and names someone else. Reported as
    ///     dealt-with on purpose: retrying a directory this custodian must never
    ///     touch would wedge the record forever, which is worse than leaving a
    ///     stranger's directory alone.
    ///
    /// `false` means genuinely unfinished, and there are exactly two ways to get
    /// it: a removal that failed, and a marker that could not be READ (as opposed
    /// to read and found foreign). The second is not a removal failure at all,
    /// which is why "only a removal failure returns false" — as this said before —
    /// was wrong: an unreadable marker leaves ownership unknown, and completing on
    /// it would abandon a directory this launch may well own.
    fn sweep_run_dir(&self, record: &LaunchRecord) -> bool;
    /// Stop the host's recorded children — the app-server and the TUI — by
    /// **verified identity**, under a bound. Called from [`complete_cleanup`],
    /// after the session kill.
    ///
    /// This is what closes the measured orphan race. A SIGKILLed or aborted host
    /// runs no teardown, and its children then depend on the kernel's hangup for
    /// the pane's session leader, which races tmux's teardown of that pty — lose
    /// that race and nothing ever signals them. The custodian does not need to win
    /// a race, because the record names them.
    fn teardown_children(&self, record: &LaunchRecord);
    fn mark_clean_complete(&self) -> Result<()>;
    /// Whether the OS **boot identity has changed** since this launch's record was
    /// written — i.e. a reboot happened (finding 5). A reboot is the escape from a
    /// permanently-armed `indeterminate + Absent` custodian: any late session the
    /// indeterminate mutation might still have created cannot exist across a
    /// reboot, because the pre-reboot process that would have created it is gone.
    fn boot_changed(&self, record: &LaunchRecord) -> bool;
}

/// Run one custodian pass. Pure with respect to [`CustodianDeps`]; every
/// scenario the D7 gates list is reachable by scripting the deps.
pub fn tick<D: CustodianDeps>(deps: &D) -> Result<Tick> {
    let record = deps.load()?;
    match &record.state {
        LaunchState::Pending => {
            if deps.coordinator_liveness() == Liveness::Gone {
                deps.cas_failed("coordinator lost while launch pending")?;
                return Ok(Tick::FailedPending("coordinator lost"));
            }
            match deps.deadline(&record) {
                Expiry::Live => Ok(Tick::Idle),
                Expiry::Expired => {
                    deps.cas_failed("launch deadline expired")?;
                    Ok(Tick::FailedPending("deadline expired"))
                }
                Expiry::Indeterminate => {
                    // A deadline we cannot judge is fail-closed: treat like
                    // expiry rather than wait forever on an unreadable clock.
                    deps.cas_failed("launch deadline could not be judged")?;
                    Ok(Tick::FailedPending("deadline indeterminate"))
                }
            }
        }
        LaunchState::Ready => {
            // Only a PROVEN-gone coordinator is session-fatal (Principle D): an
            // `Unknown` coordinator must NOT trigger a teardown.
            if deps.coordinator_liveness() == Liveness::Gone {
                // Ready is committed, so we do not rewrite the state — the
                // session ending is the outcome (reported on the supervisor/
                // daemon exit path, 2d/2e). But the teardown must actually
                // succeed: a `Killed`/`Absent`/`EpochChanged` is terminal, while
                // an `Unavailable`/`Ambiguous` is retried, never treated as done.
                match deps.destroy() {
                    CleanupOutcome::Killed
                    | CleanupOutcome::Absent
                    | CleanupOutcome::EpochChanged
                    // ServerGone (server proven dead) is terminal too (finding 1).
                    | CleanupOutcome::ServerGone => {
                        // Record a durable TEARDOWN MARKER (finding 8): set the
                        // Ready record's cleanup to `Complete`, so a later
                        // recovery_sweep does NOT rearm a fresh custodian on every
                        // sweep for a session that was already torn down.
                        if !complete_cleanup(deps)? {
                            return Ok(Tick::RetryCleanup);
                        }
                        Ok(Tick::ReadyFatalTeardown)
                    }
                    CleanupOutcome::Unavailable(_) | CleanupOutcome::Ambiguous(_) => {
                        Ok(Tick::RetryCleanup)
                    }
                }
            } else {
                Ok(Tick::Idle)
            }
        }
        LaunchState::Failed { .. } => match record.cleanup {
            CleanupState::Complete => {
                // Re-prove durability before exiting (round-5 finding 7): if the
                // Complete write's fsync failed on a prior tick, the record is
                // visibly Complete but not proven durable — re-fsync it (idempotent)
                // rather than exit on the re-read without re-fsyncing.
                deps.mark_clean_complete()?;
                Ok(Tick::Done)
            }
            CleanupState::NotRequired => {
                // `NotRequired` means no session was created, so there is no pane
                // and no host — but the record can still NAME a run dir, because
                // the coordinator fsyncs the path *before* `new-session` and a
                // definite new-session failure terminalizes to `NotRequired`. No
                // directory should exist on that path, and the sweep is a no-op
                // when none does; running it anyway costs one `remove_dir_all` and
                // removes the need to be right about that.
                if !complete_cleanup(deps)? {
                    return Ok(Tick::RetryCleanup);
                }
                Ok(Tick::Done)
            }
            CleanupState::Pending => resolve_cleanup(deps, &record),
        },
    }
}

/// Finish cleanup: stop the recorded children, sweep the run dir, then durably
/// mark the record `Complete`.
///
/// Takes **no record**: it re-reads one itself. The caller's snapshot decided
/// *whether* to clean up; this decides *what to act on*, and by now that snapshot
/// can be several tmux probes old.
///
/// The order is the whole point and it is not interchangeable. Every caller has
/// just reached a terminal destroy outcome — the session is killed, proven
/// absent, or on a server that no longer exists — so a host that was still alive
/// has already had its SIGHUP and its own chance to remove the directory. The
/// sweep runs after that, for the host that never got the chance. And it runs
/// *before* the `Complete` write, so the marker that stops a later sweep from
/// rearming a custodian is only written once the directory has actually been
/// dealt with.
fn complete_cleanup<D: CustodianDeps>(deps: &D) -> Result<bool> {
    // Re-read before anything destructive. See `CustodianDeps::current_record`: the
    // caller's snapshot is a decision input, not a licence to act on stale
    // identities.
    //
    // A record that cannot be re-read is a RETRY, never a fall back to the old
    // copy. Falling back was this fence failing open in the one case it exists
    // for: the snapshot is stale precisely when something changed underneath, and
    // an unreadable record is the strongest hint that something is changing right
    // now. Acting on the old copy could signal identities the current record no
    // longer authorises, and then write `Complete` over it.
    let Ok(record) = deps.current_record() else {
        return Ok(false);
    };
    let record = &record;
    // Children first, then the directory, then the marker. Processes before the
    // files they hold open: a straggler that outlives the sweep would otherwise go
    // on writing to an unlinked run dir, which is harmless but invisible.
    deps.teardown_children(record);
    // A sweep that could not deal with the directory must NOT reach the marker.
    // `Complete` is the durable statement that nothing is owed, and it is what
    // stops a later `recovery_sweep` from rearming anyone — so writing it over an
    // unremoved directory converts a retriable failure into a permanent leak with
    // nobody left to notice. Retry instead; the custodian's whole idiom is that an
    // unproven outcome is retried, never assumed.
    if !deps.sweep_run_dir(record) {
        return Ok(false);
    }
    deps.mark_clean_complete()?;
    Ok(true)
}

/// Whether the D7 "no guessed grace period" rule still applies — i.e. whether a
/// late session might STILL be coming, so one observation of absence is not proof.
///
/// It starts as the raw `new_session_indeterminate` flag and is retired by either
/// of two pieces of evidence that the late session has already arrived:
///
///   * **A host was admitted** (`host_admitted`, durable in the record). A host
///     only runs because a pane ran it, so the session existed. This is the strong
///     one, because it survives the custodian dying and being rearmed — and it is
///     the case that actually occurs: a host whose launch already failed refuses
///     admission and destroys its own session on the way out, which is correct,
///     but it means the custodian arrives to find an absence it would otherwise
///     refuse to believe.
///   * **This custodian saw the session itself**, which covers a host that never
///     got as far as admission.
///
/// Neither weakens the rule where it matters: with no host and no sighting, an
/// absence is still not proof and the custodian stays armed.
fn late_session_still_possible<D: CustodianDeps>(deps: &D, record: &LaunchRecord) -> bool {
    if !record.new_session_indeterminate {
        return false;
    }
    // A persisted lease or host identity is arrival evidence in its own right:
    // neither is ever written except by a host that reached the gate, so a record
    // carrying one has already seen its pane run — even if the arrival flag's own
    // write did not land.
    let arrived =
        record.host_reached_gate || record.host_lease.is_some() || record.host_identity.is_some();
    // The sighting is read from the RECORD first (durable, and therefore inherited
    // by a replacement custodian) and only then from this process's own memory.
    !arrived && !record.session_observed && !deps.seen_session_present()
}

/// Whether a process group still has any member this process could signal.
///
/// `kill(-pgid, 0)` sends nothing; it only asks. `ESRCH` is the proof of
/// extinction. Any other error means the question could not be answered, which is
/// reported as "still has members" — the fail-closed reading, since the caller
/// uses this to decide whether cleanup fell short.
fn group_has_members(pgid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing and only probes for the group's existence.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Whether the server that hosted this launch's session is provably gone.
///
/// Two bindings, both to a **recorded identity** and never to socket reachability:
///
///   * **Server A recorded** — the tmux server's own pid+birth. A session cannot
///     outlive the server process hosting it.
///   * **No server A** — the coordinator died between starting the pane and
///     persisting A, which leaves nothing to bind to and used to mean "armed until
///     reboot". The fallback is the HOST's recorded identity: the pane's command
///     *is* the host, so a host proven dead means tmux has already reaped the pane,
///     and the session with it. Arrival evidence is required too, so this can never
///     fire for a launch whose pane never ran.
///
/// Neither branch can conclude "gone" against a live session: both demand a
/// **proven** `Gone` (never `Unknown`), and the fallback's premise — a pane dies
/// when its command exits — holds because nothing here sets `remain-on-exit`.
fn server_gone_evidence(record: &LaunchRecord) -> bool {
    let proven_gone = |id: &ProcessIdentity| liveness(id) == Liveness::Gone;
    match &record.server_a {
        Some(a) => proven_gone(&ProcessIdentity {
            pid: a.server_pid as i32,
            birth: a.server_birth,
        }),
        None => {
            let arrived = record.host_reached_gate || record.host_identity.is_some();
            arrived && record.host_identity.as_ref().is_some_and(proven_gone)
        }
    }
}

/// The `failed{cleanup:pending}` branch: destroy the uid's session and decide
/// whether the custodian is done or must stay armed.
fn resolve_cleanup<D: CustodianDeps>(deps: &D, record: &LaunchRecord) -> Result<Tick> {
    match deps.destroy() {
        CleanupOutcome::Killed => {
            if !complete_cleanup(deps)? {
                return Ok(Tick::RetryCleanup);
            }
            Ok(Tick::CleanedAndDone)
        }
        CleanupOutcome::Absent => {
            // "Absent" after we have already SEEN the session is the tail of our
            // own kill, not evidence that a late session never arrived. Treating
            // it as the latter is what wedged this branch once the pane started
            // running a real host: the first pass kills, the pane takes a second
            // or two to actually die, so that pass can only report `Unavailable`,
            // and the next pass sees a clean absence it then refuses to believe.
            if late_session_still_possible(deps, record) {
                if deps.boot_changed(record) {
                    // The escape (finding 5): a reboot means any late session the
                    // indeterminate mutation might still have created cannot exist
                    // — the pre-reboot process that would create it is gone. So a
                    // proven absence now IS terminal; the custodian is not armed
                    // literally forever.
                    if !complete_cleanup(deps)? {
                        return Ok(Tick::RetryCleanup);
                    }
                    return Ok(Tick::AlreadyAbsentDone);
                }
                // No guessed grace period: one absence is not proof after an
                // indeterminate new-session. Stay armed.
                Ok(Tick::StayArmedIndeterminate)
            } else {
                if !complete_cleanup(deps)? {
                    return Ok(Tick::RetryCleanup);
                }
                Ok(Tick::AlreadyAbsentDone)
            }
        }
        CleanupOutcome::EpochChanged | CleanupOutcome::ServerGone => {
            // EpochChanged: the server's identity changed — any session our uid
            // could have named is on a lifetime that no longer exists.
            // ServerGone (finding 1): the kill drained the server, so the
            // disposable session AND its server are gone. In BOTH cases the server
            // that could have run a late `new-session` no longer exists, so this
            // is a valid exit even after an indeterminate `new-session` — the
            // reasoning D7 already applies to EpochChanged. Neither is a *proven*
            // "we killed our exact session", but the cleanup goal is met.
            if !complete_cleanup(deps)? {
                return Ok(Tick::RetryCleanup);
            }
            Ok(Tick::CleanedAndDone)
        }
        // Refuse to pick between duplicates, and never read Unavailable as gone.
        // BUT the reboot escape must fire here too (round-5 finding 5): a
        // post-reboot / no-server result is `Unavailable`, so the escape checked
        // only on `Absent` would never fire — a custodian could retry forever. A
        // reboot means the session (and any late one) cannot exist, so a proven
        // boot change is terminal even from the retry path.
        // Ambiguous means tmux showed something this code refuses to pick between —
        // two claimants for one uid, or a row it could not parse. There is no
        // reading of that which licenses a terminal cleanup: the escapes below are
        // about proving a session is GONE, and ambiguity is the opposite of proof.
        // Always retry, whatever the boot or server evidence says.
        CleanupOutcome::Ambiguous(_) => Ok(Tick::RetryCleanup),
        CleanupOutcome::Unavailable(_) => {
            // The second escape, and the one that matters on a real machine.
            //
            // `Unavailable` is overwhelmingly "no server answers the socket", and
            // tmux.rs is right to refuse to read that as absence — a socket is not
            // an identity. But the record holds one: **server A**, the pid+birth of
            // the exact tmux server this launch's session lived on. If that process
            // is provably dead, the session cannot exist, because the thing that
            // was hosting it is gone. That is the same identity-bound reasoning
            // `EpochChanged` already uses, applied to the case where there is no
            // server left to ask.
            //
            // Without it, a server whose LAST session was ours exits when we kill
            // that session — and then every later pass reads `Unavailable` and
            // retries until a reboot. Reachable on any shared server whose last
            // session this is, which every server is exactly once.
            //
            // Still fail-closed under the indeterminate flag: if A is gone but a
            // late `new-session` might yet land on a SUCCESSOR server, absence is
            // not settled, so that case keeps waiting unless we have already seen
            // the session ourselves.
            let a_gone = deps.server_gone_evidence(record);
            if deps.boot_changed(record) || (a_gone && !late_session_still_possible(deps, record)) {
                if !complete_cleanup(deps)? {
                    return Ok(Tick::RetryCleanup);
                }
                Ok(Tick::CleanedAndDone)
            } else {
                Ok(Tick::RetryCleanup)
            }
        }
    }
}

// ----------------------------------------------------------------------------
// The real deps + run loop (the `internal-codex-custodian` subcommand body).
// ----------------------------------------------------------------------------

/// Config carried into the detached custodian process.
pub struct CustodianCfg {
    pub uid: String,
    pub socket: String,
    pub coordinator: ProcessIdentity,
    pub poll: std::time::Duration,
}

struct RealDeps {
    cfg: CustodianCfg,
    /// Sticky: set the first time this custodian sees the uid actually present.
    /// A `Cell` because the loop is single-threaded and the flag is process-local
    /// — a custodian that dies and is replaced by the sweep correctly starts over
    /// with no memory, which is the conservative direction.
    seen_present: std::cell::Cell<bool>,
}

impl CustodianDeps for RealDeps {
    fn load(&self) -> Result<LaunchRecord> {
        codex_launch::load(&self.cfg.uid)
    }
    fn coordinator_liveness(&self) -> Liveness {
        liveness(&self.cfg.coordinator)
    }
    fn deadline(&self, record: &LaunchRecord) -> Expiry {
        deadline_expiry(record)
    }
    fn destroy(&self) -> CleanupOutcome {
        // Bind the cleanup to the PERSISTED server A (round-5 finding 1): read A's
        // identity from the record and pass it as the pin, so absent/killed/gone
        // is proven against the exact server the launch created — a different
        // server B rebinding the socket cannot fake a proven absence. When A was
        // never persisted (an indeterminate new-session created nothing yet), we
        // pass None and the destroy establishes A from the resolve as before (the
        // "late UID" case). An unreadable/unparseable record (finding 2) must
        // NOT collapse to the no-A path — that would do UNPINNED cleanup on
        // corrupt evidence. Fail closed: a load error is `Unavailable` (retry),
        // never the legitimate `record says no A` fallback.
        match codex_launch::load(&self.cfg.uid) {
            Ok(record) => {
                // Remember a positive sighting, once, and only while it can still
                // change a verdict: under the indeterminate flag, before we have
                // seen anything. That bounds the extra `tmux` probe to the one
                // branch whose escape depends on it, rather than paying for it on
                // every pass of every launch.
                if record.new_session_indeterminate
                    && !record.session_observed
                    && !self.seen_present.get()
                {
                    let pin = record
                        .server_a
                        .as_ref()
                        .map(|a| a.as_pin(&self.cfg.socket, &self.cfg.uid));
                    if protocol::tmux::owned_liveness(&self.cfg.socket, &self.cfg.uid, pin.as_ref())
                        == protocol::tmux::OwnedLiveness::Live
                    {
                        // PERSIST it BEFORE anything destructive. A process-local flag dies with this
                        // custodian, and the sweep's replacement — arriving after
                        // the session is already gone — would then refuse to
                        // believe an absence its predecessor had already
                        // explained, and stay armed until the next reboot. Writing
                        // it once, under the lock, is what makes the observation
                        // survive the observer.
                        let noted =
                            codex_launch::LaunchLock::acquire_bounded(&self.cfg.uid, LOCK_BUDGET)
                                .and_then(|lock| {
                                    codex_launch::note_session_observed(&lock, &self.cfg.uid)
                                });
                        if let Err(err) = noted {
                            // NOT log-and-continue. The sighting is what retires the
                            // "a late session may still be coming" rule, and the
                            // destroy about to run is the destructive act that
                            // consumes it. Proceeding on an unpersisted sighting
                            // means a custodian that dies mid-cleanup leaves a
                            // successor with no record of it — armed until reboot,
                            // for a session this pass had already seen and killed.
                            // Retry instead: `Unavailable` is the custodian's word
                            // for "no conclusion this pass".
                            return CleanupOutcome::Unavailable(format!(
                                "could not persist the session sighting for {}: {err:#}",
                                self.cfg.uid
                            ));
                        }
                        self.seen_present.set(true);
                    }
                }
                match record.server_a {
                    Some(a) => {
                        let pin = a.as_pin(&self.cfg.socket, &self.cfg.uid);
                        protocol::tmux::destroy_owned_session(
                            &self.cfg.socket,
                            &self.cfg.uid,
                            Some(&pin),
                        )
                    }
                    None => {
                        protocol::tmux::destroy_owned_session(&self.cfg.socket, &self.cfg.uid, None)
                    }
                }
            }
            Err(_) => {
                CleanupOutcome::Unavailable("launch record unreadable — failing closed".into())
            }
        }
    }

    fn teardown_children(&self, record: &LaunchRecord) {
        // Every shortfall is COLLECTED and reported. The point of this method is
        // that the custodian is the last actor; if it cannot prove it stopped
        // something, the only thing standing between that and an invisible leak is
        // saying so. Three things count as a shortfall, and the first two used to
        // be silently skipped:
        //
        //   * an identity whose liveness is `Unknown` — not proof it is gone, so
        //     not something to walk past;
        //   * a signal that failed for any reason other than "no such process";
        //   * a group that still has members after being killed.
        let mut shortfalls: Vec<String> = Vec::new();
        // Only children the HOST spawned. The custodian is in this list too (the
        // exec gate records it), and it is us — signalling ourselves mid-cleanup
        // would be an own goal.
        let host_children: Vec<&codex_launch::ChildEntry> = record
            .children
            .iter()
            .filter(|c| c.role != "custodian")
            .collect();

        for child in &host_children {
            let role = &child.role;
            let pid = child.identity.pid;
            // The D5 invariant: verify the recorded identity is still THAT process
            // before signalling anything. A pid whose birth no longer matches is
            // somebody else's process now.
            //
            // The check is immediately before the signal, which narrows the
            // pid-reuse window to that instant but does not close it — there is no
            // atomic check-and-signal for a pid on this platform. Stated plainly
            // because it is the residual, not a guarantee.
            let leads_own_group = child.pgid == pid;
            match liveness(&child.identity) {
                // The leader is proven gone — but a GROUP it led can still have
                // members, because a child it forked is a member in its own right
                // and does not die with its leader. "The pid is gone" therefore
                // says nothing about the group, and walking away here is precisely
                // how a forked descendant of the app-server survives every kill
                // (measured: it does).
                //
                // The group is only signalled if it still HAS members, which is
                // also what makes the number safe to use: a process group id stays
                // allocated while any member exists, so an occupied group cannot
                // have been recycled out from under the recorded pgid. The residual
                // is the same instant-wide check-then-signal window as for pids —
                // narrowed to that instant, not closed, because this platform has
                // no atomic check-and-signal.
                Liveness::Gone => {
                    if leads_own_group && group_has_members(child.pgid) {
                        // SAFETY: signal to a group proven occupied a moment ago.
                        let rc = unsafe { libc::kill(-child.pgid, libc::SIGKILL) };
                        if rc != 0 {
                            let err = std::io::Error::last_os_error();
                            if err.raw_os_error() != Some(libc::ESRCH) {
                                shortfalls.push(format!(
                                    "signalling {role}'s surviving group ({}) failed: {err}",
                                    child.pgid
                                ));
                            }
                        }
                    }
                    continue;
                }
                Liveness::Alive => {}
                Liveness::Unknown => {
                    shortfalls.push(format!(
                        "{role} (pid {pid}) liveness could not be read, so it was neither \
                         proven stopped nor safely signalled"
                    ));
                    continue;
                }
            }
            // A child that leads its own group (pgid == pid) is signalled as a
            // GROUP, which reaches anything it forked. One that shares the host's
            // group is signalled as a pid — killing that group would mean killing
            // by a number whose members we never recorded.
            let target = if leads_own_group { -child.pgid } else { pid };
            // SAFETY: `target` derives from an identity verified alive an instant
            // ago, so the pid — and the pgid equal to it — is still allocated.
            let rc = unsafe { libc::kill(target, libc::SIGKILL) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                // ESRCH means it went away between the check and the signal, which
                // is the outcome we wanted anyway. Anything else is a shortfall.
                if err.raw_os_error() != Some(libc::ESRCH) {
                    shortfalls.push(format!("signalling {role} (target {target}) failed: {err}"));
                }
            }
        }

        // Bounded proof, not a hopeful sleep. `Unknown` counts as still-unproven
        // here for the same reason it does above: the wait is for PROOF that each
        // recorded identity is gone, and an unreadable one is not that.
        let deadline = std::time::Instant::now() + CHILD_TEARDOWN_BUDGET;
        let unproven = loop {
            let unproven: Vec<&&codex_launch::ChildEntry> = host_children
                .iter()
                .filter(|c| liveness(&c.identity) != Liveness::Gone)
                .collect();
            if unproven.is_empty() {
                break Vec::new();
            }
            if std::time::Instant::now() >= deadline {
                break unproven
                    .iter()
                    .map(|c| {
                        format!(
                            "{} (pid {}) was not proven stopped within {CHILD_TEARDOWN_BUDGET:?}",
                            c.role, c.identity.pid
                        )
                    })
                    .collect();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        shortfalls.extend(unproven);

        // Group EXTINCTION, separately from the leader's death. A group leader can
        // be reaped while members it forked keep running — that is precisely why
        // the pgid is recorded and killed as a group — and "the leader is gone"
        // says nothing about them. `kill(-pgid, 0)` answering ESRCH is the proof
        // available here: no process remains in that group.
        for child in &host_children {
            if child.pgid != child.identity.pid {
                // Not a group we created; its members are not ours to reason about.
                continue;
            }
            if group_has_members(child.pgid) {
                shortfalls.push(format!(
                    "{}'s process group ({}) still has members after the group kill",
                    child.role, child.pgid
                ));
            }
        }

        if !shortfalls.is_empty() {
            // Reported, not fatal. The run-dir sweep and the `Complete` marker
            // still follow: a child that could not be proven stopped must not wedge
            // the record forever, but it must never be silent either — an
            // unreported shortfall is exactly how a leak becomes invisible.
            eprintln!(
                "codex-custodian: teardown shortfalls for {}: {}",
                record.uid,
                shortfalls.join("; ")
            );
        }
    }

    fn current_record(&self) -> Result<LaunchRecord> {
        codex_launch::load(&self.cfg.uid)
    }

    fn seen_session_present(&self) -> bool {
        self.seen_present.get()
    }

    fn server_gone_evidence(&self, record: &LaunchRecord) -> bool {
        server_gone_evidence(record)
    }
    fn cas_failed(&self, reason: &str) -> Result<()> {
        // Bounded lock (round-5 finding 6): the custodian is the SOLE cleanup
        // owner — a stopped holder must never wedge it.
        let lock = LaunchLock::acquire_bounded(&self.cfg.uid, LOCK_BUDGET)?;
        codex_launch::to_failed(&lock, &self.cfg.uid, reason, CleanupState::Pending)?;
        Ok(())
    }
    fn sweep_run_dir(&self, record: &LaunchRecord) -> bool {
        // The path comes from the record rather than the custodian's command line,
        // exactly as `destroy` reads server A: the coordinator fsyncs it before
        // `tmux new-session`, so the record is the one place that knows it even
        // when the coordinator died before telling anybody.
        let Some(run_dir) = record.run_dir.as_deref() else {
            // Nothing was ever recorded, so there is nothing owed. Dealt with.
            return true;
        };
        // The guard on a RECURSIVE DELETE driven by a stored string: the only path
        // this may remove is the one **this launch's own uid and nonce derive**.
        // Recomputed and compared for equality, not pattern-matched — a shape
        // check like "starts with the prefix" would still accept
        // `/tmp/cch.<some-other-launch>`, and unlinking a *live* session's bound
        // sockets and logs is a worse outcome than leaving a directory behind. A
        // corrupted or hand-edited field now matches nothing and sweeps nothing.
        //
        // `remove_dir_all` does not follow a symlink at the leaf either (verified
        // against rustc 1.97.1: it unlinks the link and returns Ok, leaving the
        // target untouched), so a planted link at the recorded name cannot redirect
        // the delete.
        let Ok(expected) =
            crate::codex_coordinator::choose_run_dir(&record.uid, &record.launch_nonce)
        else {
            return true;
        };
        if expected.to_str() != Some(run_dir) {
            // Not a path this launch could have chosen, so this custodian owes
            // nothing on it. Refusing to remove it is the point; refusing to
            // COMPLETE because of it would wedge the record forever on a value it
            // must never act on.
            return true;
        }
        // The name matching is necessary and NOT sufficient, and this is the
        // deletion warrant. The path was recorded *before* the directory existed,
        // and the derivation is many-to-one — so the directory standing here now
        // may have been created by a different, LIVE launch that derived the same
        // name. Removing it would take that session's bound sockets and logs with
        // it. Only the marker the owning host wrote can settle whose it is.
        match crate::codex_launch::run_dir_marker(
            std::path::Path::new(run_dir),
            &record.uid,
            &record.launch_nonce,
        ) {
            // Ours: proceed to remove it.
            crate::codex_launch::MarkerVerdict::Ours => {}
            // Read, and it names someone else (or nobody). Not ours to delete — and
            // nothing is owed on it either, so this reports DEALT WITH rather than
            // retrying. Retrying would wedge the record forever on a directory this
            // custodian must never touch, which is worse than leaving a stranger's
            // directory alone.
            crate::codex_launch::MarkerVerdict::Foreign => {
                eprintln!(
                    "codex-custodian: {run_dir} exists but its owner marker does not name {}; \
                     leaving it alone",
                    record.uid
                );
                return true;
            }
            // Already gone: the normal success.
            crate::codex_launch::MarkerVerdict::Absent => return true,
            // The question could not be ANSWERED. This is the one verdict that must
            // not become `Complete`: an unreadable marker is not proof the directory
            // is a stranger's, and treating it as such would abandon a directory
            // this custodian is responsible for, with the marker that says so
            // written and then never re-read. Retry — a transient read error is
            // exactly what a later pass fixes.
            crate::codex_launch::MarkerVerdict::Unknown(why) => {
                eprintln!(
                    "codex-custodian: could not read {run_dir}'s owner marker ({why}); \
                     leaving cleanup pending so a later pass retries"
                );
                return false;
            }
        }
        // NotFound is the normal, successful case: a live host removed its own
        // directory moments ago on the SIGHUP this cleanup just sent. Any OTHER
        // error means the directory may still be there, which is not something to
        // paper over with a `Complete` marker — report it and let the caller retry.
        match std::fs::remove_dir_all(run_dir) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => {
                eprintln!(
                    "codex-custodian: could not remove the run dir {run_dir} for {}: {err}; \
                     leaving cleanup pending so a later pass retries",
                    self.cfg.uid
                );
                false
            }
        }
    }
    fn mark_clean_complete(&self) -> Result<()> {
        let lock = LaunchLock::acquire_bounded(&self.cfg.uid, LOCK_BUDGET)?;
        codex_launch::set_cleanup(&lock, &self.cfg.uid, CleanupState::Complete)
    }
    fn boot_changed(&self, record: &LaunchRecord) -> bool {
        // A reboot changes the OS boot identity. Only a *readable* boot that
        // differs counts as changed — an unreadable boot is not proof of a reboot,
        // so it does not trigger the escape (the custodian stays armed rather than
        // completing on a hiccup).
        matches!(protocol::proc_identity::boot_identity(), Some(now) if now != record.boot)
    }
}

/// The detached custodian loop. Ticks until a terminal verdict, sleeping
/// `poll` between passes. Returns the terminal [`Tick`] (the subcommand maps it
/// to an exit code).
pub fn run_loop(cfg: CustodianCfg) -> Tick {
    let poll = cfg.poll;
    let deps = RealDeps {
        cfg,
        seen_present: std::cell::Cell::new(false),
    };
    loop {
        match tick(&deps) {
            Ok(verdict) if verdict.is_terminal() => return verdict,
            Ok(_) => {}
            // A transient error (unreadable record mid-rename) is retried, not
            // fatal — the custodian's whole point is durability.
            Err(_) => {}
        }
        std::thread::sleep(poll);
    }
}

/// Default time between custodian passes. Short enough that a coordinator death
/// is noticed promptly, long enough not to spin.
const DEFAULT_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// The bounded budget for acquiring the launch lock on the cleanup / sweep /
/// host-admission paths (round-5 finding 6): never block indefinitely on a
/// stopped holder.
const LOCK_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the custodian will wait to PROVE the host's recorded children have
/// stopped after signalling them. Bounded because cleanup must finish: an
/// unprovable child is reported, never waited on forever.
const CHILD_TEARDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The `internal-codex-custodian` subcommand: parse the config the coordinator
/// passed, run the loop, and exit 0 on any terminal verdict. Hidden machinery.
pub fn run_custodian(args: &[String]) -> ! {
    // Parse the charter BEFORE acking (round-4 finding 6): the readiness ACK must
    // mean "I am up AND my charter is valid", so a custodian with a bad charter
    // never ACKs — the owner's readiness fence then times out and the owner does
    // not proceed, instead of the owner believing an about-to-`_exit(64)` (and
    // then-zombie) custodian is armed.
    let cfg = match parse_custodian_args(args) {
        Ok(cfg) => cfg,
        // A custodian that cannot parse its own charter cannot safely act.
        Err(_) => std::process::exit(64),
    };
    // Only now confirm to the owner that we execed, started, AND hold a valid
    // charter, so "custodian armed before tmux" is a proven fact.
    crate::exec_gate::ack_started_if_gated();
    let _ = run_loop(cfg);
    std::process::exit(0)
}

fn parse_custodian_args(args: &[String]) -> Result<CustodianCfg> {
    let mut uid = None;
    let mut socket = None;
    let mut pid = None;
    let mut sec = None;
    let mut usec = None;
    let mut poll_ms: Option<u64> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--uid" => uid = it.next().cloned(),
            "--socket" => socket = it.next().cloned(),
            "--coordinator-pid" => pid = it.next().and_then(|v| v.parse::<i32>().ok()),
            "--coordinator-sec" => sec = it.next().and_then(|v| v.parse::<i64>().ok()),
            "--coordinator-usec" => usec = it.next().and_then(|v| v.parse::<i64>().ok()),
            "--poll-ms" => poll_ms = it.next().and_then(|v| v.parse::<u64>().ok()),
            _ => {}
        }
    }
    Ok(CustodianCfg {
        uid: uid.ok_or_else(|| anyhow::anyhow!("--uid required"))?,
        socket: socket.ok_or_else(|| anyhow::anyhow!("--socket required"))?,
        coordinator: ProcessIdentity {
            pid: pid.ok_or_else(|| anyhow::anyhow!("--coordinator-pid required"))?,
            birth: protocol::proc_identity::BirthIdentity {
                start_sec: sec.ok_or_else(|| anyhow::anyhow!("--coordinator-sec required"))?,
                start_usec: usec.ok_or_else(|| anyhow::anyhow!("--coordinator-usec required"))?,
            },
        },
        poll: poll_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(DEFAULT_POLL),
    })
}

/// Bring a custodian up through the D6 exec gate for `uid`, monitoring
/// `coordinator`, and fsync its identity into the record before releasing it.
/// Shared by the coordinator's initial arm and the recovery sweep's rearm.
pub fn spawn_custodian_through_gate(
    uid: &str,
    socket: &str,
    coordinator: ProcessIdentity,
    gate_program: std::path::PathBuf,
    nonce: &str,
) -> Result<ProcessIdentity> {
    let target = vec![
        gate_program.to_string_lossy().into_owned(),
        "internal-codex-custodian".into(),
        "--uid".into(),
        uid.to_string(),
        "--socket".into(),
        socket.to_string(),
        "--coordinator-pid".into(),
        coordinator.pid.to_string(),
        "--coordinator-sec".into(),
        coordinator.birth.start_sec.to_string(),
        "--coordinator-usec".into(),
        coordinator.birth.start_usec.to_string(),
    ];
    let uid_intent = uid.to_string();
    let uid_ready = uid.to_string();
    let ready = crate::exec_gate::launch_gated(
        crate::exec_gate::GateSpec {
            role: "custodian".into(),
            nonce: nonce.to_string(),
            gate_program,
            target_argv: target,
            target_envs: vec![],
        },
        // on_intent: fsync the spawn intent before the gate exists (finding 4).
        |intent| {
            let lock = LaunchLock::acquire_bounded(&uid_intent, LOCK_BUDGET)?;
            codex_launch::set_pending_spawn(
                &lock,
                &uid_intent,
                codex_launch::PendingSpawn {
                    role: intent.role.clone(),
                    nonce: intent.nonce.clone(),
                    argv_hash: intent.argv_hash.clone(),
                },
            )
        },
        // on_ready: atomically CAS the custodian slot + record the child. A
        // losing concurrent rearm's CAS refusal here withholds GO from the
        // redundant custodian, so only one is ever armed (finding 8).
        |ready, intent| {
            let lock = LaunchLock::acquire_bounded(&uid_ready, LOCK_BUDGET)?;
            codex_launch::cas_custodian_with_child(
                &lock,
                &uid_ready,
                ProcessIdentity {
                    pid: ready.pid,
                    birth: ready.birth,
                },
                ready.pgid,
                &intent.nonce,
                &intent.argv_hash,
            )
        },
    )?;
    Ok(ProcessIdentity {
        pid: ready.pid,
        birth: ready.birth,
    })
}

/// Run one bounded recovery sweep and rearm a replacement custodian for every
/// `failed{cleanup:pending}` record whose custodian died (D7: "both killed ⇒
/// next sweep rearms"; total-guardian-loss recovered from durable state). The
/// record transitions themselves are done inside [`codex_launch::recovery_sweep`];
/// this adds the process re-spawn the sweep cannot do on its own.
pub fn run_sweep_once(socket: &str) -> Result<()> {
    let gate_program = std::env::current_exe()?;
    // Attempt every replacement, but do NOT discard a rearm failure (finding 4):
    // a record that was flagged as needing a custodian and could not get one is
    // still guardianless, so the sweep must surface that rather than exit clean.
    // We try all actions first (one bad record must not starve the others), then
    // report the first failure so the caller/next sweep knows to retry.
    let mut first_err: Option<anyhow::Error> = None;
    for action in codex_launch::recovery_sweep() {
        // A record we could not examine leaves the sweep INCONCLUSIVE, not clean.
        // Reporting it as an error is what makes a single sweep's exit status
        // mean "everything owed was done" instead of "nothing went wrong while I
        // did possibly nothing" — the caller's cue to run another pass.
        if let codex_launch::SweepAction::Skipped { uid, why } = &action {
            first_err
                .get_or_insert_with(|| anyhow::anyhow!("could not examine {uid} this pass: {why}"));
            continue;
        }
        if let codex_launch::SweepAction::NeedsReplacementCustodian { uid } = action {
            // Re-read the record for the coordinator identity to monitor; if it
            // has vanished or is unreadable, record it and move on — a later sweep
            // retries.
            let record = match codex_launch::load(&uid) {
                Ok(record) => record,
                Err(err) => {
                    first_err.get_or_insert_with(|| {
                        err.context(format!("re-reading {uid} to rearm its custodian"))
                    });
                    continue;
                }
            };
            if let Err(err) = spawn_custodian_through_gate(
                &uid,
                socket,
                record.coordinator,
                gate_program.clone(),
                &codex_launch::mint_nonce(),
            ) {
                first_err.get_or_insert_with(|| {
                    err.context(format!("rearming a replacement custodian for {uid}"))
                });
            }
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// The `internal-codex-sweep` subcommand: one bounded recovery sweep. Wired at
/// CodeConnect invocation / ccd startup by the ungate (2d); dispatchable now so
/// the sweep machinery is exercised end-to-end.
///
/// The sweep's outcome is **surfaced**, not swallowed (finding 5): a rearm that
/// could not be completed leaves a guardianless record, so it exits non-zero (and
/// writes the reason to stderr) rather than always exiting 0.
pub fn run_sweep(args: &[String]) -> ! {
    let socket = parse_sweep_socket(args);
    match run_sweep_once(&socket) {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("codex sweep could not complete a rearm: {err:#}");
            std::process::exit(1)
        }
    }
}

fn parse_sweep_socket(args: &[String]) -> String {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--socket" {
            if let Some(v) = it.next() {
                return v.clone();
            }
        }
    }
    protocol::TMUX_SOCKET_NAME.to_string()
}

// ----------------------------------------------------------------------------
// Late-host self-refusal (D6/D7): a `codex-host` that starts must validate the
// launch record + take a live lease, else it may only clean up its own session.
// ----------------------------------------------------------------------------

/// A late host's admission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAdmission {
    /// Nonce/identities/deadline/pending held and a live lease was taken.
    Admitted,
    /// The gate was evaluated and said no. The host ran cleanup-only and reports
    /// the outcome. Arrival is durable by this point, so exiting is safe.
    CleanupOnly {
        reason: String,
        cleanup: CleanupOutcome,
    },
    /// The gate could not be reached at all — the launch lock, or the arrival
    /// write itself, failed — so **nothing durable says this pane ever ran**.
    ///
    /// This is not a refusal, and the host must not treat it as one. Exiting here
    /// ends the pane command, which ends the tmux session; on a launch whose
    /// creation was indeterminate that removes the last observable trace, and the
    /// custodian is then left with an absence it can never explain and stays armed
    /// until the next reboot.
    ///
    /// The host instead parks: it creates nothing and stays alive, so the session
    /// remains **visible**. The custodian's census then finds the uid Present,
    /// persists that sighting before killing it, and the wedge resolves.
    ParkInert { reason: String },
}

/// The `internal-codex-host-preflight` subcommand: the D7 gate a tmux-started
/// `codex-host` runs **before it creates anything**. It validates the launch
/// record and takes a live lease, or refuses and runs cleanup-only. Exits `0`
/// when admitted, and `75` (`EX_TEMPFAIL`) on a cleanup-only refusal so the caller
/// knows Codex must not launch.
///
/// **Superseded as a launch path.** The host now runs this same gate ITSELF:
/// [`crate::codex_host`] takes `--uid`/`--nonce`/`--tmux-socket` and calls
/// [`late_host_admission`] as its first fallible act, before its run dir, its
/// sockets or either child exists. That is what closes the case this comment was
/// written to flag — a `new-session` that timed out leaves the record
/// `failed{cleanup:pending, indeterminate}`, and a frozen tmux server that later
/// runs the queued pane command now meets a gate that refuses it, instead of
/// bringing a full live Codex session up for a launch already declared failed.
///
/// It had to be the host process and not a preflight child: the lease is an
/// IDENTITY. `admit_host` CASes the caller's pid+birth into `host_lease`, and
/// every later liveness question about "the host" reads it — so a preflight
/// child's lease would name a corpse the moment it exited.
///
/// This subcommand therefore remains as the standalone exercise of the gate, not
/// as the path any launch takes.
pub fn run_host_preflight(args: &[String]) -> ! {
    let mut uid = None;
    let mut nonce = None;
    let mut socket = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--uid" => uid = it.next().cloned(),
            "--nonce" => nonce = it.next().cloned(),
            "--socket" => socket = it.next().cloned(),
            _ => {}
        }
    }
    let (Some(uid), Some(nonce)) = (uid, nonce) else {
        std::process::exit(64);
    };
    let socket = socket.unwrap_or_else(|| protocol::TMUX_SOCKET_NAME.to_string());
    match late_host_admission(&uid, &nonce, &socket) {
        Ok(HostAdmission::Admitted) => std::process::exit(0),
        // Cleanup-only refusal — Codex never launches (EX_TEMPFAIL).
        Ok(HostAdmission::CleanupOnly { .. }) => std::process::exit(75),
        // The gate could not be reached. This standalone subcommand is not the
        // pane's host, so it has no session to keep observable and simply reports
        // the failure; the parking behaviour belongs to the real host, which is
        // the process inside the pane (see `codex_host::park_inert`).
        Ok(HostAdmission::ParkInert { .. }) => std::process::exit(70),
        Err(_) => std::process::exit(70),
    }
}

/// The gate a tmux-started `codex-host` runs before it creates anything (D7).
/// On admission it holds the lease and proceeds to bring the session up
/// ([`crate::codex_host`]). On any doubt it destroys **only its own** uid's
/// session and refuses —
/// "a frozen tmux server may run the old `new-session` arbitrarily late, but
/// what it runs cannot start Codex and removes its own stale session."
pub fn late_host_admission(uid: &str, nonce: &str, socket: &str) -> Result<HostAdmission> {
    // Everything up to and including the arrival write is PRE-ARRIVAL: a failure
    // there yields `ParkInert`, never an error the caller could mistake for a
    // refusal. Nothing durable records this pane yet, so ending it would erase the
    // session's last trace (see `HostAdmission::ParkInert`).
    let host = match codex_launch::require_current_identity() {
        Ok(host) => host,
        Err(err) => {
            return Ok(HostAdmission::ParkInert {
                reason: format!("could not read this host's own identity: {err:#}"),
            })
        }
    };
    // Bounded (round-5 finding 6): host admission must not hang on a stopped
    // lock holder.
    let lock = match LaunchLock::acquire_bounded(uid, LOCK_BUDGET) {
        Ok(lock) => lock,
        Err(err) => {
            return Ok(HostAdmission::ParkInert {
                reason: format!("could not take the launch lock: {err:#}"),
            })
        }
    };

    // ARRIVAL IS THE FIRST DURABLE ACT ON ANY PATH THAT MAY DESTROY.
    //
    // This flag and the host identity beside it are what later let the custodian
    // tell "the late session never came" from "it came and is gone" — and every
    // refusal below ends by destroying this uid's session. So the write goes
    // ahead of ALL of them, including the pgid read, which is itself a refusal
    // path with a `destroy_owned_session` on it.
    //
    // A failure to WRITE it — or a write that deliberately did NOT happen
    // (nonce mismatch: this host does not belong to the recorded launch, so no
    // arrival evidence exists for this uid's session) — must not fall through to
    // a refusal, because every refusal path below destroys the session. Park.
    match codex_launch::note_host_reached_gate(&lock, uid, nonce, &host) {
        Ok(codex_launch::ArrivalNote::Recorded) => {}
        Ok(codex_launch::ArrivalNote::NonceMismatch) => {
            return Ok(HostAdmission::ParkInert {
                reason: "this host's nonce does not match the recorded launch, so its \
                         arrival cannot be durably noted for this uid"
                    .into(),
            });
        }
        Err(err) => {
            return Ok(HostAdmission::ParkInert {
                reason: format!("could not record this host's arrival: {err:#}"),
            });
        }
    }

    // The host's own process group, recorded with the exclusive lease (finding
    // 9). A pgid we cannot read is fail-closed: refuse and clean up.
    let host_pgid = match protocol::proc_identity::read_pgid(host.pid) {
        Some(pgid) => pgid,
        None => {
            // Release the lock before the (bounded) tmux cleanup so we never hold
            // the interprocess lock across a tmux probe.
            drop(lock);
            let cleanup = protocol::tmux::destroy_owned_session(socket, uid, None);
            return Ok(HostAdmission::CleanupOnly {
                reason: "could not read the host's own pgid".into(),
                cleanup,
            });
        }
    };
    match codex_launch::admit_host(&lock, uid, nonce, &host, host_pgid, "codex-host")? {
        codex_launch::Admission::Admitted => Ok(HostAdmission::Admitted),
        codex_launch::Admission::Refused(reason) => {
            // Release the lock before the (bounded) tmux cleanup so we never hold
            // the interprocess lock across a tmux probe.
            drop(lock);
            let cleanup = protocol::tmux::destroy_owned_session(socket, uid, None);
            Ok(HostAdmission::CleanupOnly { reason, cleanup })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fully scripted deps: the test dictates the record, liveness, deadline,
    /// and destroy outcome, and observes the CAS/complete calls.
    struct Scripted {
        record: RefCell<LaunchRecord>,
        coordinator: Liveness,
        deadline: Expiry,
        destroy: CleanupOutcome,
        boot_changed: bool,
        seen_present: bool,
        server_a_gone: bool,
        current_record_fails: bool,
        cas_calls: RefCell<Vec<String>>,
        completed: RefCell<bool>,
        /// Observed so the ORDER can be asserted: the sweep must be recorded
        /// before `Complete` is, on every branch that completes cleanup.
        swept_before_complete: RefCell<Option<bool>>,
        tore_down_children: RefCell<bool>,
        sweep_succeeds: bool,
    }

    fn base_record(state: LaunchState, cleanup: CleanupState, indeterminate: bool) -> LaunchRecord {
        LaunchRecord {
            schema: 1,
            launch_nonce: "n".into(),
            uid: "u".into(),
            session_name: "cc-1".into(),
            coordinator: ProcessIdentity {
                pid: 1,
                birth: protocol::proc_identity::BirthIdentity {
                    start_sec: 1,
                    start_usec: 1,
                },
            },
            custodian: None,
            boot: protocol::proc_identity::BootIdentity {
                boot_sec: 1,
                boot_usec: 1,
            },
            deadline_monotonic_nanos: 0,
            state,
            cleanup,
            new_session_indeterminate: indeterminate,
            host_lease: None,
            pending_spawn: None,
            server_a: None,
            run_dir: None,
            host_reached_gate: false,
            host_identity: None,
            session_observed: false,
            children: vec![],
            created_ms: 0,
        }
    }

    impl Scripted {
        fn new(record: LaunchRecord) -> Scripted {
            Scripted {
                record: RefCell::new(record),
                coordinator: Liveness::Alive,
                deadline: Expiry::Live,
                destroy: CleanupOutcome::Killed,
                boot_changed: false,
                seen_present: false,
                server_a_gone: false,
                current_record_fails: false,
                cas_calls: RefCell::new(vec![]),
                completed: RefCell::new(false),
                swept_before_complete: RefCell::new(None),
                tore_down_children: RefCell::new(false),
                sweep_succeeds: true,
            }
        }
    }

    impl CustodianDeps for Scripted {
        fn load(&self) -> Result<LaunchRecord> {
            Ok(self.record.borrow().clone())
        }
        fn coordinator_liveness(&self) -> Liveness {
            self.coordinator
        }
        fn deadline(&self, _r: &LaunchRecord) -> Expiry {
            self.deadline
        }
        fn destroy(&self) -> CleanupOutcome {
            self.destroy.clone()
        }
        fn current_record(&self) -> Result<LaunchRecord> {
            if self.current_record_fails {
                anyhow::bail!("the record could not be re-read");
            }
            Ok(self.record.borrow().clone())
        }
        fn seen_session_present(&self) -> bool {
            self.seen_present
        }
        fn server_gone_evidence(&self, _record: &LaunchRecord) -> bool {
            self.server_a_gone
        }
        fn teardown_children(&self, _record: &LaunchRecord) {
            *self.tore_down_children.borrow_mut() = true;
        }
        fn sweep_run_dir(&self, _record: &LaunchRecord) -> bool {
            // Children must already have been dealt with when the dir is swept.
            assert!(
                *self.tore_down_children.borrow(),
                "complete_cleanup must stop the children before removing the dir they hold open"
            );
            // Records "the sweep happened, and `Complete` had not been written
            // yet" — the invariant `complete_cleanup` exists to hold.
            *self.swept_before_complete.borrow_mut() = Some(!*self.completed.borrow());
            self.sweep_succeeds
        }
        fn cas_failed(&self, reason: &str) -> Result<()> {
            self.cas_calls.borrow_mut().push(reason.to_string());
            self.record.borrow_mut().state = LaunchState::Failed {
                reason: reason.to_string(),
            };
            Ok(())
        }
        fn mark_clean_complete(&self) -> Result<()> {
            *self.completed.borrow_mut() = true;
            self.record.borrow_mut().cleanup = CleanupState::Complete;
            Ok(())
        }
        fn boot_changed(&self, _record: &LaunchRecord) -> bool {
            self.boot_changed
        }
    }

    #[test]
    fn pending_and_healthy_is_idle() {
        let d = Scripted::new(base_record(
            LaunchState::Pending,
            CleanupState::Pending,
            false,
        ));
        assert_eq!(tick(&d).unwrap(), Tick::Idle);
    }

    #[test]
    fn pending_with_a_dead_coordinator_cas_fails() {
        let mut d = Scripted::new(base_record(
            LaunchState::Pending,
            CleanupState::Pending,
            false,
        ));
        d.coordinator = Liveness::Gone;
        assert!(matches!(tick(&d).unwrap(), Tick::FailedPending(_)));
        assert_eq!(d.cas_calls.borrow().len(), 1);
    }

    #[test]
    fn pending_past_deadline_cas_fails() {
        let mut d = Scripted::new(base_record(
            LaunchState::Pending,
            CleanupState::Pending,
            false,
        ));
        d.deadline = Expiry::Expired;
        assert!(matches!(tick(&d).unwrap(), Tick::FailedPending(_)));
    }

    #[test]
    fn pending_with_an_unjudgeable_deadline_fails_closed() {
        let mut d = Scripted::new(base_record(
            LaunchState::Pending,
            CleanupState::Pending,
            false,
        ));
        d.deadline = Expiry::Indeterminate;
        assert!(matches!(tick(&d).unwrap(), Tick::FailedPending(_)));
    }

    #[test]
    fn failed_pending_cleanup_kills_and_completes() {
        let d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            false,
        ));
        assert_eq!(tick(&d).unwrap(), Tick::CleanedAndDone);
        assert!(*d.completed.borrow());
    }

    #[test]
    fn indeterminate_absent_escapes_on_a_boot_change() {
        // Finding 5 escape: an indeterminate launch whose destroy is Absent stays
        // armed — UNLESS the OS boot identity changed (a reboot), in which case any
        // late session the mutation might still have created cannot exist, so a
        // proven absence now completes. Without the reboot it stays armed; with it
        // it completes.
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            true, // new_session_indeterminate
        ));
        d.destroy = CleanupOutcome::Absent;
        assert_eq!(tick(&d).unwrap(), Tick::StayArmedIndeterminate);
        assert!(!*d.completed.borrow());
        // Now a reboot: the escape fires and the custodian completes.
        d.boot_changed = true;
        assert_eq!(tick(&d).unwrap(), Tick::AlreadyAbsentDone);
        assert!(*d.completed.borrow());
    }

    #[test]
    fn retry_cleanup_escapes_on_a_boot_change() {
        // Round-5 finding 5: the reboot escape must fire on the RETRY (Unavailable)
        // path too — a post-reboot/no-server destroy is Unavailable, so an escape
        // checked only on Absent would never fire and the custodian would retry
        // forever. A proven boot change is terminal here.
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            true, // indeterminate
        ));
        d.destroy = CleanupOutcome::Unavailable("no server answers; retry".into());
        // No reboot yet ⇒ retry (stays armed).
        assert_eq!(tick(&d).unwrap(), Tick::RetryCleanup);
        assert!(!*d.completed.borrow());
        // A reboot ⇒ the escape fires and the custodian completes.
        d.boot_changed = true;
        assert_eq!(tick(&d).unwrap(), Tick::CleanedAndDone);
        assert!(*d.completed.borrow());
    }

    #[test]
    fn ready_teardown_records_a_completion_marker() {
        // Finding 8: a Ready session whose coordinator died is torn down, and the
        // teardown records a durable marker (cleanup → Complete) so a later sweep
        // does not rearm a fresh custodian on every pass.
        let mut d = Scripted::new(base_record(
            LaunchState::Ready,
            CleanupState::NotRequired,
            false,
        ));
        d.coordinator = Liveness::Gone;
        d.destroy = CleanupOutcome::Killed;
        assert_eq!(tick(&d).unwrap(), Tick::ReadyFatalTeardown);
        assert!(
            *d.completed.borrow(),
            "the Ready teardown must record the completion marker"
        );
        assert_eq!(d.record.borrow().cleanup, CleanupState::Complete);
    }

    #[test]
    fn failed_pending_absent_determinate_completes() {
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            false,
        ));
        d.destroy = CleanupOutcome::Absent;
        assert_eq!(tick(&d).unwrap(), Tick::AlreadyAbsentDone);
        assert!(*d.completed.borrow());
    }

    #[test]
    fn failed_pending_absent_after_indeterminate_stays_armed() {
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            true, // new_session_indeterminate
        ));
        d.destroy = CleanupOutcome::Absent;
        assert_eq!(tick(&d).unwrap(), Tick::StayArmedIndeterminate);
        assert!(
            !*d.completed.borrow(),
            "an indeterminate launch must not complete on one absence"
        );
    }

    #[test]
    fn failed_pending_server_gone_completes_even_when_indeterminate() {
        // Finding 1 interaction: a kill that drains the server yields ServerGone.
        // The server (and any chance of a late session on it) is gone, so the
        // custodian COMPLETES even for an indeterminate launch — otherwise a
        // single-session cleanup would stay armed forever (the exact lifecycle
        // regression). Not a proven Killed, but a terminal cleanup state.
        for indeterminate in [false, true] {
            let mut d = Scripted::new(base_record(
                LaunchState::Failed { reason: "x".into() },
                CleanupState::Pending,
                indeterminate,
            ));
            d.destroy = CleanupOutcome::ServerGone;
            assert_eq!(tick(&d).unwrap(), Tick::CleanedAndDone);
            assert!(
                *d.completed.borrow(),
                "ServerGone completes cleanup (indeterminate={indeterminate})"
            );
        }
    }

    #[test]
    fn ready_with_a_dead_coordinator_and_server_gone_is_terminal() {
        // A committed session whose coordinator died and whose teardown drained
        // the server is a terminal ReadyFatalTeardown, not an infinite retry.
        let mut d = Scripted::new(base_record(
            LaunchState::Ready,
            CleanupState::NotRequired,
            false,
        ));
        d.coordinator = Liveness::Gone;
        d.destroy = CleanupOutcome::ServerGone;
        assert_eq!(tick(&d).unwrap(), Tick::ReadyFatalTeardown);
    }

    #[test]
    fn failed_pending_epoch_changed_is_a_valid_indeterminate_exit() {
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            true,
        ));
        d.destroy = CleanupOutcome::EpochChanged;
        assert_eq!(tick(&d).unwrap(), Tick::CleanedAndDone);
    }

    #[test]
    fn failed_pending_unavailable_retries_never_completes() {
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            false,
        ));
        d.destroy = CleanupOutcome::Unavailable("server wedged".into());
        assert_eq!(tick(&d).unwrap(), Tick::RetryCleanup);
        assert!(
            !*d.completed.borrow(),
            "Unavailable is never read as absence/done"
        );
    }

    #[test]
    fn failed_pending_ambiguous_retries() {
        let mut d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Pending,
            false,
        ));
        d.destroy = CleanupOutcome::Ambiguous("two claimants".into());
        assert_eq!(tick(&d).unwrap(), Tick::RetryCleanup);
    }

    #[test]
    fn ready_with_a_live_coordinator_is_idle() {
        let d = Scripted::new(base_record(
            LaunchState::Ready,
            CleanupState::NotRequired,
            false,
        ));
        assert_eq!(tick(&d).unwrap(), Tick::Idle);
    }

    #[test]
    fn ready_with_a_dead_coordinator_is_session_fatal() {
        let mut d = Scripted::new(base_record(
            LaunchState::Ready,
            CleanupState::NotRequired,
            false,
        ));
        d.coordinator = Liveness::Gone;
        assert_eq!(tick(&d).unwrap(), Tick::ReadyFatalTeardown);
    }

    #[test]
    fn failed_already_complete_is_done() {
        let d = Scripted::new(base_record(
            LaunchState::Failed { reason: "x".into() },
            CleanupState::Complete,
            false,
        ));
        assert_eq!(tick(&d).unwrap(), Tick::Done);
    }

    /// The late-host self-refusal gate (D6/D7): a host on a live, pending,
    /// nonce-matching launch whose coordinator is alive is admitted (and takes
    /// the lease); once the launch has failed, the same host is refused and runs
    /// cleanup-only. The socket points at a non-existent server, so cleanup is a
    /// harmless `Absent` — the admission decision is what this pins.
    #[test]
    fn late_host_is_admitted_when_valid_and_refused_after_failure() {
        use crate::codex_launch::{self, CleanupState, LaunchLock, NewLaunch};
        use protocol::proc_identity::{boot_identity, current_identity, monotonic_now_nanos};

        let uid = "host-gate-1";
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire(uid).unwrap();
        codex_launch::create_pending(
            &lock,
            NewLaunch {
                launch_nonce: "hostnonce".into(),
                uid: uid.into(),
                session_name: "cc-1".into(),
                coordinator: protocol::proc_identity::current_identity().unwrap(),
                boot: boot_identity().unwrap(),
                deadline_monotonic_nanos: far,
                created_ms: 1,
            },
        )
        .unwrap();
        // Admission now requires a recorded, proven-live custodian (finding 9).
        // Arm one — this process itself, which is provably alive — through the
        // real single-owner CAS before any host can be admitted.
        let me = current_identity().unwrap();
        codex_launch::cas_custodian_with_child(&lock, uid, me, me.pid, "cn", "ch").unwrap();
        drop(lock);

        let bogus_socket = "/tmp/cc-nonexistent-host-gate.sock";
        // Wrong nonce FIRST, on the fresh record — before any correct-nonce
        // arrival exists. A mismatched host cannot write arrival evidence for
        // this uid, and every refusal path destroys the session, so the only
        // sound verdict is to PARK (not a cleanup refusal, which would tear the
        // session down leaving the record with no arrival to explain it).
        assert!(matches!(
            late_host_admission(uid, "wrong", bogus_socket).unwrap(),
            HostAdmission::ParkInert { .. }
        ));
        // Valid: pending, right nonce, live coordinator (us) ⇒ admitted.
        assert_eq!(
            late_host_admission(uid, "hostnonce", bogus_socket).unwrap(),
            HostAdmission::Admitted
        );
        // A wrong nonce parks even after a correct-nonce arrival was recorded:
        // the mismatched host still cannot note ITS arrival, so the rule is
        // uniform rather than dependent on another host's history.
        assert!(matches!(
            late_host_admission(uid, "wrong", bogus_socket).unwrap(),
            HostAdmission::ParkInert { .. }
        ));
        // After failure ⇒ cleanup-only refusal even with the right nonce. The
        // bogus socket has no server, and round-4 forbids inferring absence from a
        // socket error (finding 1), so the cleanup is a harmless `Unavailable`
        // (retry) rather than a false `Absent`.
        let lock = LaunchLock::acquire(uid).unwrap();
        codex_launch::to_failed(&lock, uid, "x", CleanupState::Pending).unwrap();
        drop(lock);
        match late_host_admission(uid, "hostnonce", bogus_socket).unwrap() {
            HostAdmission::CleanupOnly { cleanup, .. } => {
                assert!(matches!(cleanup, CleanupOutcome::Unavailable(_)));
            }
            other => panic!("expected cleanup-only refusal, got {other:?}"),
        }
    }

    #[test]
    fn every_branch_that_completes_cleanup_sweeps_the_run_dir_first() {
        // The run dir must be swept on every path that marks cleanup Complete —
        // that marker is what stops a later sweep rearming a custodian, so a
        // branch that reached it without sweeping would strand the directory with
        // nobody left to look at it. And the sweep must happen BEFORE the marker.
        let branches: Vec<(&str, LaunchRecord, CleanupOutcome, bool)> = vec![
            (
                "failed/killed",
                base_record(fail(), CleanupState::Pending, false),
                CleanupOutcome::Killed,
                false,
            ),
            (
                "failed/absent-determinate",
                base_record(fail(), CleanupState::Pending, false),
                CleanupOutcome::Absent,
                false,
            ),
            (
                "failed/absent-indeterminate-after-reboot",
                base_record(fail(), CleanupState::Pending, true),
                CleanupOutcome::Absent,
                true,
            ),
            (
                "failed/epoch-changed",
                base_record(fail(), CleanupState::Pending, false),
                CleanupOutcome::EpochChanged,
                false,
            ),
            (
                "failed/server-gone",
                base_record(fail(), CleanupState::Pending, false),
                CleanupOutcome::ServerGone,
                false,
            ),
            (
                "failed/unavailable-after-reboot",
                base_record(fail(), CleanupState::Pending, false),
                CleanupOutcome::Unavailable("no server".into()),
                true,
            ),
            (
                "ready/coordinator-lost",
                base_record(LaunchState::Ready, CleanupState::Pending, false),
                CleanupOutcome::Killed,
                false,
            ),
        ];
        for (name, record, destroy, boot_changed) in branches {
            let ready = record.state == LaunchState::Ready;
            let mut deps = Scripted::new(record);
            deps.destroy = destroy;
            deps.boot_changed = boot_changed;
            if ready {
                // Only a proven-gone coordinator makes a Ready session fatal.
                deps.coordinator = Liveness::Gone;
            }
            let verdict = tick(&deps).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(verdict.is_terminal(), "{name} should complete: {verdict:?}");
            assert!(
                *deps.completed.borrow(),
                "{name} must mark cleanup complete"
            );
            assert_eq!(
                *deps.swept_before_complete.borrow(),
                Some(true),
                "{name} must sweep the run dir, and do it BEFORE writing Complete"
            );
        }

        // The counter-case: a branch that stays armed must NOT sweep. An
        // indeterminate new-session with the uid merely absent has not proven the
        // session gone, so a late host may still be about to create the very
        // directory a sweep would remove.
        let mut armed = Scripted::new(base_record(fail(), CleanupState::Pending, true));
        armed.destroy = CleanupOutcome::Absent;
        assert_eq!(tick(&armed).unwrap(), Tick::StayArmedIndeterminate);
        assert_eq!(*armed.swept_before_complete.borrow(), None);
        assert!(!*armed.completed.borrow());

        // `NotRequired` sweeps too. It means no session was created, so no pane
        // and no host ever existed — but the record can still NAME a run dir,
        // because the coordinator fsyncs the path before `new-session` and a
        // definite new-session failure terminalizes to `NotRequired`. Nothing
        // should be there to remove; the sweep runs anyway so the invariant is
        // "every terminal cleanup sweeps", with no branch that has to be reasoned
        // about separately.
        let none = Scripted::new(base_record(fail(), CleanupState::NotRequired, false));
        assert_eq!(tick(&none).unwrap(), Tick::Done);
        assert_eq!(*none.swept_before_complete.borrow(), Some(true));
    }

    #[test]
    fn an_indeterminate_absence_is_terminal_once_the_session_has_actually_been_seen() {
        // D7: after an indeterminate `new-session`, ONE observation of absence is
        // not proof, because the late session may still be coming. That reasoning
        // expires the moment the late session has been seen — it came, and this is
        // the tail of our own kill.
        let mut waiting = Scripted::new(base_record(fail(), CleanupState::Pending, true));
        waiting.destroy = CleanupOutcome::Absent;
        assert_eq!(tick(&waiting).unwrap(), Tick::StayArmedIndeterminate);
        assert!(!*waiting.completed.borrow());

        let mut seen = Scripted::new(base_record(fail(), CleanupState::Pending, true));
        seen.destroy = CleanupOutcome::Absent;
        seen.seen_present = true;
        assert_eq!(tick(&seen).unwrap(), Tick::AlreadyAbsentDone);
        assert!(*seen.completed.borrow());
        assert_eq!(*seen.swept_before_complete.borrow(), Some(true));

        // The durable half, and the one that actually fires in practice: a host
        // that was admitted proves the pane ran, so the late session already
        // arrived — even for a custodian that was rearmed later and saw nothing
        // itself. This is the case a refused host creates by destroying its own
        // session on the way out.
        let mut admitted = base_record(fail(), CleanupState::Pending, true);
        admitted.host_reached_gate = true;
        let mut deps = Scripted::new(admitted);
        deps.destroy = CleanupOutcome::Absent;
        assert!(
            !deps.seen_present,
            "no in-process sighting: the record alone decides"
        );
        assert_eq!(tick(&deps).unwrap(), Tick::AlreadyAbsentDone);
        assert!(*deps.completed.borrow());
    }

    #[test]
    fn an_unavailable_census_completes_only_when_server_a_is_provably_dead() {
        // The last-session wedge: killing our session drains the server, the
        // socket stops answering, and `Unavailable` is all any census can say. The
        // record still names server A by pid+birth, so its death is provable — and
        // a session cannot outlive the server that hosted it.
        let mut wedged = Scripted::new(base_record(fail(), CleanupState::Pending, false));
        wedged.destroy = CleanupOutcome::Unavailable("no server answers".into());
        assert_eq!(
            tick(&wedged).unwrap(),
            Tick::RetryCleanup,
            "an unreachable socket alone is never proof of absence"
        );

        let mut proven = Scripted::new(base_record(fail(), CleanupState::Pending, false));
        proven.destroy = CleanupOutcome::Unavailable("no server answers".into());
        proven.server_a_gone = true;
        assert_eq!(tick(&proven).unwrap(), Tick::CleanedAndDone);
        assert!(*proven.completed.borrow());

        // …but a dead A does NOT settle it while a late `new-session` could still
        // land on a successor server and we have never seen the session ourselves.
        let mut late_possible = Scripted::new(base_record(fail(), CleanupState::Pending, true));
        late_possible.destroy = CleanupOutcome::Unavailable("no server answers".into());
        late_possible.server_a_gone = true;
        assert_eq!(
            tick(&late_possible).unwrap(),
            Tick::RetryCleanup,
            "an indeterminate launch that was never seen must keep waiting"
        );
        // Once it HAS been seen, the same evidence is terminal.
        late_possible.seen_present = true;
        assert_eq!(tick(&late_possible).unwrap(), Tick::CleanedAndDone);
    }

    #[test]
    fn a_sweep_that_could_not_remove_the_dir_leaves_cleanup_pending_and_retries() {
        // `Complete` is the durable statement that nothing is owed, and the thing
        // that stops a later `recovery_sweep` rearming anyone. Writing it over a
        // directory the sweep could not remove converts a retriable failure into a
        // permanent leak nobody will look at again.
        let mut deps = Scripted::new(base_record(fail(), CleanupState::Pending, false));
        deps.destroy = CleanupOutcome::Killed;
        deps.sweep_succeeds = false;
        assert_eq!(tick(&deps).unwrap(), Tick::RetryCleanup);
        assert!(
            !*deps.completed.borrow(),
            "a failed sweep must never reach the Complete marker"
        );
        // The children were still stopped — cleanup does as much as it can prove.
        assert!(*deps.tore_down_children.borrow());

        // And the same record completes once the sweep can do its job.
        deps.sweep_succeeds = true;
        assert_eq!(tick(&deps).unwrap(), Tick::CleanedAndDone);
        assert!(*deps.completed.borrow());
    }

    #[test]
    fn the_real_sweep_reports_a_refusal_as_dealt_with_but_a_failure_as_retriable() {
        // The two `false`-looking cases are NOT the same. A path this launch could
        // never have chosen is nothing we owe — reporting it as unfinished would
        // wedge the record forever on a value the sweep must never act on. An
        // actual removal failure IS unfinished.
        let uid = "sweepverdict";
        let nonce = codex_launch::mint_nonce();
        let deps = RealDeps {
            cfg: CustodianCfg {
                uid: uid.into(),
                socket: "/tmp/cc-no-such-server.sock".into(),
                coordinator: protocol::proc_identity::current_identity().unwrap(),
                poll: std::time::Duration::from_millis(1),
            },
            seen_present: std::cell::Cell::new(false),
        };
        let record_with = |run_dir: Option<&str>| -> LaunchRecord {
            let mut r = base_record(fail(), CleanupState::Pending, false);
            r.uid = uid.into();
            r.launch_nonce = nonce.clone();
            r.run_dir = run_dir.map(|s| s.to_string());
            r
        };
        assert!(
            deps.sweep_run_dir(&record_with(None)),
            "nothing recorded is nothing owed"
        );
        assert!(
            deps.sweep_run_dir(&record_with(Some("/tmp/cch.someone.else"))),
            "a path this launch could not have chosen is not this custodian's debt"
        );
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        assert!(
            deps.sweep_run_dir(&record_with(ours.to_str())),
            "an already-absent dir is the normal success"
        );
        std::fs::create_dir_all(ours.join("inner")).unwrap();
        assert!(
            deps.sweep_run_dir(&record_with(ours.to_str())),
            "a directory with no owner marker is not ours to delete, and nothing is owed \
             on it — DEALT WITH, not a retry that would wedge the record"
        );
        assert!(ours.exists(), "an unmarked directory must be left alone");
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();
        assert!(deps.sweep_run_dir(&record_with(ours.to_str())));
        assert!(!ours.exists());
    }

    #[test]
    fn an_unreadable_current_record_retries_instead_of_completing() {
        // The fence exists because the caller's snapshot can be stale by the time
        // cleanup signals processes and deletes a directory. Falling back to that
        // snapshot when the re-read fails is the fence failing open in the exact
        // case it was built for — an unreadable record is the strongest hint that
        // something is changing underneath right now.
        let mut deps = Scripted::new(base_record(fail(), CleanupState::Pending, false));
        deps.destroy = CleanupOutcome::Killed;
        deps.current_record_fails = true;
        assert_eq!(tick(&deps).unwrap(), Tick::RetryCleanup);
        assert!(
            !*deps.completed.borrow(),
            "a cleanup that could not re-read the record must never write Complete"
        );
        assert!(
            !*deps.tore_down_children.borrow(),
            "nor may it signal identities it could not re-verify"
        );

        deps.current_record_fails = false;
        assert_eq!(tick(&deps).unwrap(), Tick::CleanedAndDone);
        assert!(*deps.completed.borrow());
    }

    #[test]
    fn an_ambiguous_census_never_terminalises_however_strong_the_other_evidence() {
        // Ambiguous means tmux showed two claimants for one uid, or a row that
        // could not be parsed. The escapes are about PROVING a session gone;
        // ambiguity is the opposite of proof, so neither a boot change nor a dead
        // server may convert it into a terminal cleanup.
        for (boot_changed, server_gone) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut deps = Scripted::new(base_record(fail(), CleanupState::Pending, false));
            deps.destroy = CleanupOutcome::Ambiguous("two sessions claim this uid".into());
            deps.boot_changed = boot_changed;
            deps.server_a_gone = server_gone;
            assert_eq!(
                tick(&deps).unwrap(),
                Tick::RetryCleanup,
                "ambiguous + boot_changed={boot_changed} + server_gone={server_gone} must retry"
            );
            assert!(!*deps.completed.borrow());
        }
    }

    #[test]
    fn an_unreadable_owner_marker_retries_rather_than_disowning_the_directory() {
        // `Unknown` is not `Foreign`. Collapsing them would let one transient read
        // error convince the custodian it owes nothing on a directory it is in fact
        // responsible for — and the `Complete` it would then write is what stops
        // anyone ever looking again.
        let uid = "markerretry";
        let nonce = codex_launch::mint_nonce();
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        std::fs::create_dir_all(&ours).unwrap();
        // A SYMLINK where the marker belongs: `O_NOFOLLOW` refuses to follow it, so
        // the marker cannot be read — unanswerable, not foreign.
        std::os::unix::fs::symlink("/etc/hosts", ours.join(codex_launch::RUN_DIR_OWNER_FILE))
            .unwrap();
        assert!(matches!(
            codex_launch::run_dir_marker(&ours, uid, &nonce),
            codex_launch::MarkerVerdict::Unknown(_)
        ));

        let deps = RealDeps {
            cfg: CustodianCfg {
                uid: uid.into(),
                socket: "/tmp/cc-no-such-server.sock".into(),
                coordinator: protocol::proc_identity::current_identity().unwrap(),
                poll: std::time::Duration::from_millis(1),
            },
            seen_present: std::cell::Cell::new(false),
        };
        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.uid = uid.into();
        record.launch_nonce = nonce.clone();
        record.run_dir = ours.to_str().map(|s| s.to_string());
        assert!(
            !deps.sweep_run_dir(&record),
            "an unreadable marker must leave cleanup pending, not report dealt-with"
        );
        assert!(
            ours.exists(),
            "and it must certainly not delete the directory"
        );

        // Once the marker can be read and it is ours, the same sweep completes.
        std::fs::remove_file(ours.join(codex_launch::RUN_DIR_OWNER_FILE)).unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();
        assert!(deps.sweep_run_dir(&record));
        assert!(!ours.exists());
    }

    fn fail() -> LaunchState {
        LaunchState::Failed {
            reason: "boom".into(),
        }
    }

    #[test]
    fn the_real_sweep_removes_only_the_dir_this_launchs_own_uid_and_nonce_derive() {
        // The sweep is a `remove_dir_all` driven by a stored string, so what it
        // will and will not act on is proven against a real filesystem rather than
        // reasoned about.
        let uid = "sweepuid";
        let nonce = codex_launch::mint_nonce();
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        std::fs::create_dir_all(ours.join("nested")).unwrap();
        std::fs::write(ours.join("nested").join("tui.sock"), b"x").unwrap();
        // The host's ownership marker: the sweep's deletion warrant. A directory
        // without it is never removed, so the fixture must carry one to stand in
        // for a directory a host really created.
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();

        // A directory belonging to a DIFFERENT launch. This is the case a
        // prefix-shaped guard would wave through: it is well-formed, it sits under
        // the same prefix, and removing it would unlink another live session's
        // bound sockets and logs.
        let other_nonce = codex_launch::mint_nonce();
        let others = crate::codex_coordinator::choose_run_dir("otheruid", &other_nonce).unwrap();
        std::fs::create_dir_all(&others).unwrap();
        codex_launch::write_owner_marker(&others, "otheruid", &other_nonce).unwrap();

        let deps = RealDeps {
            cfg: CustodianCfg {
                uid: uid.into(),
                socket: "/tmp/cc-no-such-server.sock".into(),
                coordinator: protocol::proc_identity::current_identity().unwrap(),
                poll: std::time::Duration::from_millis(1),
            },
            seen_present: std::cell::Cell::new(false),
        };
        let record_with = |run_dir: Option<&str>, launch_nonce: &str| -> LaunchRecord {
            let mut r = base_record(LaunchState::Ready, CleanupState::Pending, false);
            r.uid = uid.into();
            r.launch_nonce = launch_nonce.into();
            r.run_dir = run_dir.map(|s| s.to_string());
            r
        };

        // No run dir recorded: nothing to sweep, and nothing panics.
        deps.sweep_run_dir(&record_with(None, &nonce));
        assert!(ours.exists());

        // ANOTHER launch's well-formed run dir, presented in our record: refused,
        // because it is not what OUR uid+nonce derive.
        deps.sweep_run_dir(&record_with(others.to_str(), &nonce));
        assert!(
            others.exists(),
            "the sweep must refuse a well-formed run dir belonging to another launch"
        );

        // Our own path, but with the record's nonce changed so the derivation no
        // longer matches — a corrupted or hand-edited field.
        deps.sweep_run_dir(&record_with(ours.to_str(), &codex_launch::mint_nonce()));
        assert!(
            ours.exists(),
            "the guard is equality against the derivation, so a mismatched nonce sweeps nothing"
        );

        // Path traversal in the stored field is likewise not what the derivation
        // produces, so it is refused rather than followed.
        //
        // Asserted against a SENTINEL we own, not against `/etc`. "`/etc` still
        // exists" passes whether the guard worked or not — the sweep runs
        // unprivileged and could not remove it either way — so it proves the guard
        // only by coincidence. A directory this test created, sitting exactly where
        // the traversal lands, can actually be destroyed if the guard fails.
        // Under `/tmp`, which is where the traversal actually RESOLVES: the run dir
        // is `/tmp/cch.<...>`, so `<run>/..` is `/tmp`. `env::temp_dir()` on macOS
        // is `/var/folders/...`, so a sentinel there would sit somewhere the
        // traversal never reaches — and the test would pass without the guard doing
        // anything, which is the failure mode it exists to rule out.
        let victim = std::path::PathBuf::from(format!(
            "/tmp/cc-sweep-victim-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(victim.join("precious")).unwrap();
        let traversal = format!(
            "{}/../{}",
            ours.display(),
            victim.file_name().unwrap().to_str().unwrap()
        );
        deps.sweep_run_dir(&record_with(Some(&traversal), &nonce));
        assert!(
            victim.join("precious").exists(),
            "a traversal in the recorded path must not reach a directory outside the one \
             this launch derives"
        );
        let _ = std::fs::remove_dir_all(&victim);

        // The case the NAME check cannot catch, and the reason the marker exists:
        // a directory standing at exactly the path this launch derives, created by
        // a DIFFERENT launch that derived the same name. The derivation is
        // many-to-one and the path was recorded before the directory existed, so
        // this is reachable — and deleting it would take a live session's sockets
        // and logs with it.
        // Same path as `ours` — that is the point — so stand the impostor up in its
        // place rather than beside it.
        std::fs::remove_dir_all(&ours).unwrap();
        let impostor = ours.clone();
        std::fs::create_dir_all(impostor.join("live-session-data")).unwrap();
        codex_launch::write_owner_marker(&impostor, "someone-else", "other-nonce").unwrap();
        assert!(
            deps.sweep_run_dir(&record_with(ours.to_str(), &nonce)),
            "a directory claimed by another launch is not this custodian's debt, so the \
             sweep must report DEALT WITH rather than wedge the record retrying it"
        );
        assert!(
            impostor.join("live-session-data").exists(),
            "the sweep must not delete a directory whose owner marker names another launch"
        );
        std::fs::remove_dir_all(&impostor).unwrap();
        std::fs::create_dir_all(ours.join("nested")).unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();

        // The real thing: our own recorded path, with contents, removed whole.
        deps.sweep_run_dir(&record_with(ours.to_str(), &nonce));
        assert!(!ours.exists(), "the recorded run dir must be removed");
        // Idempotent: an already-removed dir is the normal case (a live host
        // removed it on the SIGHUP the session kill just sent).
        deps.sweep_run_dir(&record_with(ours.to_str(), &nonce));

        let _ = std::fs::remove_dir_all(&others);
    }
}
