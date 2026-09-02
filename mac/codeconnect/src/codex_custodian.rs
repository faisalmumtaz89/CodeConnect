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
use protocol::proc_identity::{liveness, BootIdentity, Liveness, ProcessIdentity};
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
        // A9.6(b): the symmetric first arm to the `Failed` one below. A Ready
        // record whose cleanup already reads `Complete` has been torn down, but
        // the write that said so may have been rendered visible without its
        // dir-fsync succeeding — exactly the round-5 finding 7 case the Failed
        // path already re-proves. Re-fsync (idempotent) and exit, instead of
        // exiting on the bare re-read, or — worse — destroying a second time.
        LaunchState::Ready if record.cleanup == CleanupState::Complete => {
            deps.mark_clean_complete()?;
            Ok(Tick::Done)
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
                    CleanupOutcome::Unavailable(_) => {
                        // A9.5: the Ready path needs the same escapes the retry
                        // path has. A prior-boot Ready record is rearmed by
                        // `recovery_sweep` (it flags any non-Complete cleanup whose
                        // custodian is gone, with no boot check) and
                        // `cas_custodian_with_child` admits Ready — so the fresh
                        // custodian starts probing a socket nothing answers, reads
                        // `Unavailable`, and without an escape retries until the
                        // next reboot. A proven boot change means the session
                        // cannot exist; proven-dead server A means the process that
                        // hosted it is gone. Either is terminal here for the same
                        // identity-bound reason `resolve_cleanup` uses. (Ready
                        // implies the session was created and observed determinate,
                        // so there is no late-session caveat to weigh.)
                        if deps.boot_changed(&record) || deps.server_gone_evidence(&record) {
                            if !complete_cleanup(deps)? {
                                return Ok(Tick::RetryCleanup);
                            }
                            return Ok(Tick::ReadyFatalTeardown);
                        }
                        Ok(Tick::RetryCleanup)
                    }
                    // Ambiguity is never proof: tmux showed something this code
                    // refuses to pick between, which is the opposite of evidence
                    // that the session is gone. Retry whatever the boot or server
                    // evidence says — the same stance `resolve_cleanup` takes.
                    CleanupOutcome::Ambiguous(_) => Ok(Tick::RetryCleanup),
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

/// Whether a recorded pgid may still be signalled as a group (A11.2).
#[derive(Debug, PartialEq, Eq)]
enum GroupWarrant {
    Warranted,
    Refused(String),
}

/// Bind a process group to the identity that was recorded as leading it.
///
/// The reasoning, which rests on one kernel rule: a process group id can only come
/// into existence in a process whose **pid equals that id** (`setpgid(0, 0)`). So
/// the only way the recorded number can name a group that is not ours is if the
/// recorded leader's pid was freed and handed to somebody else who then led a group
/// with it. Deciding whether that happened is therefore a question about the PID,
/// and it is answerable:
///
///   * the pid is **unoccupied** — nothing can currently be leading a fresh group
///     with that id, so whatever is still in the group inherited it from our leader.
///     This is the case that actually occurs: the app-server is reaped and the
///     descendants it forked keep the group alive (measured);
///   * the pid is still **our recorded process** — the identity matches outright;
///   * anything else — a live pid with a different birth stamp, or one whose birth
///     cannot be read — is the ABA case, or indistinguishable from it. Refused.
///
/// **Residual, stated exactly, and it is bigger than this used to claim.**
///
/// The first part is the gap between the answer and the signal that follows it:
/// Darwin has no atomic check-and-signal for a process group, so that window is
/// narrowed to an instant rather than closed.
///
/// The second part is the **unoccupied-leader arm itself, which is an unbounded
/// ABA** — this doc used to say the opposite, and that was wrong. "Nothing can
/// currently be leading a fresh group with that id" is true and does not imply what
/// the arm concludes from it. Counterexample: our group empties; the pid is reused
/// by a new process that leads a group with it; that process forks and exits; its
/// unrelated descendants keep the group alive while the pid is unoccupied *again*.
/// The arm then SIGKILLs a group that was never ours, and the age of the record
/// does not bound it.
///
/// Measured (`KERN_PROC_PGRP` on the constructed state): the surviving members'
/// only distinguishing fields are pid, ppid and start time. Every member has
/// reparented to `launchd`, so ppid says nothing; and members of a recycled group
/// start *after* the recycled leader's birth, which is after our leader's death,
/// which is after our leader's birth — so start time does not separate them either.
/// Nothing Darwin exposes today tells our own descendants from a stranger's.
///
/// The arm is nevertheless KEPT, deliberately, and the alternative is worse: the
/// host reaps its children by pid (`Child::start_kill`), so the app-server's
/// surviving descendants are exactly what this arm reaches on the ordinary teardown
/// path. Refusing here would trade a narrow within-boot risk for a guaranteed leak of
/// codex subprocesses on every session that ends normally — the leak this whole chunk
/// exists to close.
///
/// **The worst arm of that residual is closed rather than recorded** (round-4 finding
/// 10). The residual above is a within-boot one, and the sentences describing it used
/// to say the unrelated-group risk "requires pid wraparound". Across a REBOOT it
/// requires nothing at all: the recorded pgid is a number from a boot that no longer
/// exists, the new boot allocates pids from the bottom and reaches that number in the
/// ordinary course of starting up, and if the new occupant has exited while its
/// children live, the unoccupied-leader arm authorizes `SIGKILL` on a process group
/// belonging to a machine-lifetime this launch never touched. Nothing about the
/// recorded identity distinguishes it, because every field in it — pid, birth,
/// pgid — is scoped to the boot it was written under.
///
/// So the warrant now takes the record's [`BootIdentity`] and refuses outright when
/// the current boot differs. This costs nothing real: no process recorded before a
/// reboot can still be running after it, so there is never anything of ours on the
/// other side of that comparison to reach. It is the one place in this residual where
/// fail-closed is free. An UNREADABLE current boot also refuses — the same rule
/// `boot_changed` uses in the other direction is inverted here on purpose, because
/// this side is authorizing a `SIGKILL` rather than declining to conclude an absence.
///
/// **What would close it**, and why it is a chunk of its own rather than a line
/// here: provenance the members carry themselves. The host would stamp the
/// app-server's environment with this launch's nonce, every descendant would inherit
/// it, and the warrant would enumerate `KERN_PROC_PGRP` and require every member to
/// present it (via `KERN_PROCARGS2`, which is readable for same-uid processes). That
/// is new kernel-introspection machinery with its own measurement and failure modes,
/// and until it exists this arm signals on a warrant it cannot corroborate.
///
/// A second, smaller residual: a same-uid process in the same session may join an
/// existing group deliberately with `setpgid(self, pgid)`. Such a process would be
/// killed by the group signal. It is not reachable by accident — it requires
/// guessing the pgid and choosing to join it — and no privilege boundary is crossed.
fn group_warrant(identity: &ProcessIdentity, recorded_boot: BootIdentity) -> GroupWarrant {
    // Round-4 finding 10. Asked FIRST, before the pid is even probed: across a boot
    // change every field of `identity` names a machine-lifetime that is over, so no
    // answer the kernel gives about that pid can be about our process.
    match protocol::proc_identity::boot_identity() {
        Some(now) if now == recorded_boot => {}
        Some(_) => {
            return GroupWarrant::Refused(
                "the machine has rebooted since this launch was recorded, so the recorded \
                 pgid names a process group from a boot that no longer exists"
                    .to_string(),
            )
        }
        None => {
            return GroupWarrant::Refused(
                "the current boot identity could not be read, so the recorded pgid cannot \
                 be proven to belong to this boot at all"
                    .to_string(),
            )
        }
    }
    let pid = identity.pid;
    // SAFETY: signal 0 delivers nothing; it only asks whether the pid is occupied.
    if unsafe { libc::kill(pid, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            // Unoccupied: no live leader. This is the UNCORROBORATED arm — see the
            // residual above. It is warranted because the alternative leaks, not
            // because the members are proven ours.
            Some(libc::ESRCH) => GroupWarrant::Warranted,
            // Occupied by a process we may not signal — fall through to the birth
            // compare, which is what decides whether it is still ours.
            Some(libc::EPERM) => birth_matches(identity),
            _ => GroupWarrant::Refused(format!("pid {pid} could not be probed: {err}")),
        };
    }
    birth_matches(identity)
}

fn birth_matches(identity: &ProcessIdentity) -> GroupWarrant {
    let pid = identity.pid;
    match protocol::proc_identity::read_birth_identity(pid) {
        Some(birth) if birth == identity.birth => GroupWarrant::Warranted,
        Some(_) => GroupWarrant::Refused(format!(
            "pid {pid} is now a different process, so the group id may have been recycled"
        )),
        // A pid that exists but cannot be described is not evidence of anything.
        None => GroupWarrant::Refused(format!(
            "pid {pid} exists but its birth identity could not be read"
        )),
    }
}

/// Whether the server that hosted this launch's session is provably gone.
///
/// Two bindings, both to a **recorded identity** and never to socket reachability:
///
///   * **Server A recorded** — the tmux server's own pid+birth. A session cannot
///     outlive the server process hosting it.
///   * **No server A** — nothing to bind to, so **nothing is concluded**.
///
/// The pinned branch demands a *proven* `Gone` (never `Unknown`), so it can never
/// conclude "gone" against a live session. Server A's death is the death of the
/// process hosting the session, which no pane option and no config hook survives.
///
/// **The no-A branch used to infer death from the HOST's death, and that inference
/// is retired** (round-3 finding 5). It read "the pane's command *is* the host, so a
/// host proven dead means tmux reaped the pane and the session with it" — which
/// holds only because a pane dies when its command exits, which is only true because
/// `remain-on-exit` is off. A11.3 made that premise something the coordinator
/// asserts and records, and the branch then demanded the recorded bit
/// ([`LaunchRecord::remain_on_exit_asserted`]) before firing.
///
/// That was still not sound, and the reason is what the bit actually covers. It is a
/// fact about ONE assertion, against the session's window and its current pane, at
/// one moment. It is not a property of the session for the rest of its life: a
/// config hook can create another pane or window afterwards, and the options stay
/// mutable by anything running as this uid. So a historical assertion cannot license
/// a present-tense claim that the session died with its host.
///
/// It is not needed either, which is what makes retiring it a deletion rather than a
/// regression. The branch existed for exactly one window — a coordinator that died
/// after creating a session and before persisting A — and the coordinator now
/// persists A **immediately after resolving it**, before the assertion and before
/// anything else that can fail. A record with no A is therefore one where no session
/// was ever resolved, and there is no session death to infer. Fail-closed is the
/// honest answer: with nothing to bind to, this reports no evidence, and cleanup
/// settles through the paths that rest on an observation rather than an inference —
/// a positive `Absent`/`Killed` from tmux, or the boot-change escape.
fn server_gone_evidence(record: &LaunchRecord) -> bool {
    match &record.server_a {
        Some(a) => {
            liveness(&ProcessIdentity {
                pid: a.server_pid as i32,
                birth: a.server_birth,
            }) == Liveness::Gone
        }
        None => false,
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
                // The group is only signalled if it still HAS members. That was once
                // the whole warrant — "a process group id stays allocated while any
                // member exists, so an occupied group cannot have been recycled" —
                // but A11.2 is precisely that the premise holds only while the group
                // stays occupied. A group that EMPTIES and has its id reused between
                // the probe and the signal is exactly the case the sentence does not
                // cover, and `kill(-pgid, 0)` cannot tell "our forked descendants"
                // from "a stranger now occupying that number".
                //
                // So membership is bound to the recorded identity as well: a group id
                // can only be created afresh by a process whose PID equals it, so
                // proving the recorded leader's pid has not been taken over by
                // somebody else is what rules the reuse out.
                Liveness::Gone => {
                    if leads_own_group && group_has_members(child.pgid) {
                        match group_warrant(&child.identity, record.boot) {
                            GroupWarrant::Warranted => {
                                // SAFETY: signal to a group proven occupied a moment
                                // ago, whose id passed [`group_warrant`].
                                //
                                // NOT "proven not to have been recycled", which is
                                // what this comment used to say and what A11.2's row
                                // says is false: `Warranted` includes the
                                // FREE-LEADER arm, which is deliberately
                                // uncorroborated — nothing Darwin offers can tell
                                // our leader's surviving descendants from a stranger
                                // now occupying that number. Kept anyway, because it
                                // is the only arm that reaches the app-server's
                                // descendants on ordinary teardown, and refusing it
                                // trades a within-boot pid-recycling risk for a
                                // guaranteed leak on every normal session end. The
                                // CROSS-boot half of that residual, which needed no
                                // recycling at all, is refused outright by
                                // `group_warrant`'s boot check (round-4 finding 10).
                                // The
                                // command stays gated until the nonce/KERN_PROCARGS2
                                // provenance design lands; see [`group_warrant`].
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
                            // Fail closed: not signalling leaks at worst OUR OWN
                            // descendants, which the shortfall report names. Signalling
                            // would SIGKILL a process that was never part of this
                            // launch. Those are not symmetric mistakes.
                            GroupWarrant::Refused(why) => shortfalls.push(format!(
                                "{role}'s surviving group ({}) was NOT signalled: {why}",
                                child.pgid
                            )),
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
            //
            // A11.2: "it is alive" is not "it is still in the group we recorded".
            // The membership is re-proven here, against the kernel, immediately
            // before the group signal — a child that has since called `setpgid` is
            // no longer a warrant for killing that number, so it is signalled by pid
            // alone rather than by a group it has left.
            let target = if leads_own_group {
                match protocol::proc_identity::read_pgid(pid) {
                    Some(now) if now == child.pgid => -child.pgid,
                    other => {
                        shortfalls.push(format!(
                            "{role} (pid {pid}) is no longer in its recorded group {} \
                             (now {other:?}); signalled by pid alone",
                            child.pgid
                        ));
                        pid
                    }
                }
            } else {
                pid
            };
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
        // it. Only the marker the owning host wrote can settle whose it is, and
        // only an fd can bind the reading of that marker to the thing deleted.
        //
        // A11.5 lives in [`codex_launch::sweep_owned_run_dir`] rather than here,
        // because the HOST tears the same directory down on its own exit and had
        // kept a path-addressed `remove_dir_all` — two callers, one discipline, so
        // there is nothing for them to drift apart on.
        match crate::codex_launch::sweep_owned_run_dir(
            std::path::Path::new(run_dir),
            &record.uid,
            &record.launch_nonce,
        ) {
            crate::codex_launch::RunDirSweep::Settled(note) => {
                if let Some(note) = note {
                    eprintln!("codex-custodian: {note}");
                }
                true
            }
            crate::codex_launch::RunDirSweep::Retry(why) => {
                eprintln!(
                    "codex-custodian: {why} (for {}); leaving cleanup pending so a later pass \
                     retries",
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
pub(crate) mod tests {
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

    /// `pub(crate)` because the coordinator's readiness tests need the same record
    /// shape, and a second copy of a fourteen-field literal is a second thing to
    /// keep in step with the record.
    pub(crate) fn base_record(
        state: LaunchState,
        cleanup: CleanupState,
        indeterminate: bool,
    ) -> LaunchRecord {
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
            host_claimed_run_dir: false,
            session_observed: false,
            // A11.3: the ordinary case — the coordinator got past `new-session` and
            // asserted the option, so the premise the no-A escape rests on holds.
            // The test that cares about its ABSENCE clears it explicitly.
            remain_on_exit_asserted: true,
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
    fn a_dead_host_is_never_read_as_a_dead_session_without_server_a() {
        // **Round-3 finding 5: the no-A inference is RETIRED, and this pins that.**
        //
        // The branch used to conclude "the session is gone" from "the host is proven
        // dead", which is only sound because a pane dies when its command exits —
        // and `remain-on-exit on` breaks that, reachable from a user's own
        // `~/.tmux.conf` with no bug of ours. A11.3 gated the inference on the
        // recorded fact that the coordinator had asserted the option away.
        //
        // That gate was not enough. `remain_on_exit_asserted` covers ONE assertion,
        // on the session's window and its then-current pane, at one moment; a config
        // hook can add a pane afterwards and the options stay mutable. A historical
        // fact cannot license a present-tense claim about a session's death.
        //
        // So the inference is gone rather than better-gated, and it costs nothing:
        // the coordinator persists A immediately after resolving the session, so a
        // record with no A is one where no session was ever resolved. This asserts
        // the strong form — no combination of host death and recorded premise
        // produces "gone" without A.
        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.server_a = None;
        record.host_reached_gate = true;
        // A pid that cannot be alive with this birth stamp: `liveness` proves Gone.
        record.host_identity = Some(ProcessIdentity {
            pid: 0x3FFF_FFFE,
            birth: protocol::proc_identity::BirthIdentity {
                start_sec: 7,
                start_usec: 7,
            },
        });
        // The premise is established, the host is provably dead, and arrival is
        // recorded — every input the old inference wanted.
        record.remain_on_exit_asserted = true;
        assert!(
            !server_gone_evidence(&record),
            "THE GATE: a proven-dead host must NOT be read as a dead session. The pane \
             may be sitting there under a remain-on-exit set after the assertion, and \
             completing cleanup would abandon a LIVE session while writing down that \
             nothing is owed"
        );
        record.remain_on_exit_asserted = false;
        assert!(
            !server_gone_evidence(&record),
            "and without the premise, just as certainly not"
        );

        // The PINNED branch needs no such condition — server A's death is the death
        // of the process hosting the session, which no pane option survives.
        record.server_a = Some(codex_launch::ServerA {
            session_id: "$1".into(),
            server_pid: 0x3FFF_FFFD,
            server_start_time: 9,
            session_created: 9,
            server_birth: protocol::proc_identity::BirthIdentity {
                start_sec: 9,
                start_usec: 9,
            },
        });
        assert!(
            server_gone_evidence(&record),
            "a proven-dead server A is terminal whatever the pane option said"
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
    fn ready_cleanup_retry_escapes_on_a_boot_change_or_a_dead_server_a() {
        // A9.5: a prior-boot Ready record gets a fresh custodian from
        // `recovery_sweep` (Ready + cleanup != Complete + guardian gone, with no
        // boot check), and `cas_custodian_with_child` admits Ready. That custodian
        // probes a socket nothing answers, reads `Unavailable`, and — before this
        // — had no escape on the Ready arm at all, so it retried until reboot.
        let ready = || {
            let mut d = Scripted::new(base_record(
                LaunchState::Ready,
                CleanupState::NotRequired,
                false,
            ));
            d.coordinator = Liveness::Gone;
            d.destroy = CleanupOutcome::Unavailable("no server answers the socket".into());
            d
        };

        // No evidence either way ⇒ stay armed, exactly as before.
        let mut d = ready();
        assert_eq!(tick(&d).unwrap(), Tick::RetryCleanup);
        assert!(!*d.completed.borrow());

        // A proven reboot ⇒ the session cannot exist. Terminal.
        d.boot_changed = true;
        assert_eq!(tick(&d).unwrap(), Tick::ReadyFatalTeardown);
        assert!(*d.completed.borrow());

        // And, independently, a proven-dead server A ⇒ the process hosting the
        // session is gone. Also terminal.
        let mut d = ready();
        d.server_a_gone = true;
        assert_eq!(tick(&d).unwrap(), Tick::ReadyFatalTeardown);
        assert!(*d.completed.borrow());
    }

    #[test]
    fn ready_cleanup_never_escapes_on_ambiguity() {
        // A9.5's deliberate asymmetry, matching `resolve_cleanup`: `Ambiguous` is
        // tmux showing something this code refuses to pick between, which is the
        // opposite of proof that the session is gone. No amount of boot or
        // server-gone evidence licenses a terminal cleanup on it.
        let mut d = Scripted::new(base_record(
            LaunchState::Ready,
            CleanupState::NotRequired,
            false,
        ));
        d.coordinator = Liveness::Gone;
        d.destroy = CleanupOutcome::Ambiguous("two claimants for one uid".into());
        d.boot_changed = true;
        d.server_a_gone = true;
        assert_eq!(tick(&d).unwrap(), Tick::RetryCleanup);
        assert!(!*d.completed.borrow());
    }

    #[test]
    fn ready_already_complete_is_done() {
        // A9.6(b): the symmetry the Failed arm has had since round-5 finding 7. A
        // visibly-Complete record may have been published by a write whose
        // dir-fsync failed, so the exit re-proves it (idempotent) instead of
        // trusting the re-read — and does NOT fall through to a second destroy.
        let mut d = Scripted::new(base_record(
            LaunchState::Ready,
            CleanupState::Complete,
            false,
        ));
        d.coordinator = Liveness::Gone;
        // If this arm were missing, the Gone coordinator would drive a fresh
        // teardown instead of exiting.
        d.destroy = CleanupOutcome::Killed;
        assert_eq!(tick(&d).unwrap(), Tick::Done);
        assert!(
            *d.completed.borrow(),
            "the exit must re-prove the Complete marker durable"
        );
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

    /// **An arrival write that FAILS parks; it does not fall through to a verdict
    /// that destroys** (A11.8, the arrival-write-failure-mid-admission boundary).
    ///
    /// `late_host_admission` calls the arrival write first on every path, because
    /// "ARRIVAL IS THE FIRST DURABLE ACT ON ANY PATH THAT MAY DESTROY" — every
    /// refusal below it ends in `destroy_owned_session`. So the `Err` arm of
    /// [`codex_launch::note_host_reached_gate`] is load-bearing in a way the
    /// `NonceMismatch` arm beside it is not: the sibling test
    /// `late_host_is_admitted_when_valid_and_refused_after_failure` stages a
    /// caller-supplied nonce that makes the writer decline to write, which is a
    /// deliberate NON-write. This stages a write that was ATTEMPTED and FAILED.
    ///
    /// **Faulted at the right end of `store_atomic`.** The knob this uses is
    /// [`codex_launch::fail_next_record_publish`], not the older
    /// `fail_next_dir_fsync` — the latter fails the directory fsync *after* the
    /// rename has already published, so the arrival flag DID land and a test built
    /// on it would pass while proving the opposite of the thing named here. The
    /// fault is proven to be the one that fired, and to have fired on THIS call, by
    /// reading its text back out of the park reason.
    ///
    /// **The consequence is staged as a counterfactual, not asserted as a type.**
    /// A second record of identical shape, run without the fault, reaches
    /// `CleanupOnly` — a verdict whose path tears the uid's session down. Same
    /// record, same gate, same host; the failed write is the only difference, and it
    /// is what turns a destroying verdict into a park. Without that contrast the
    /// test would only be observing that `ParkInert` is returned, which the arm's
    /// own `return` makes trivially true.
    #[test]
    fn an_arrival_write_that_fails_parks_rather_than_destroying() {
        use crate::codex_launch::{self, LaunchState, SweepAction};
        use protocol::proc_identity::{boot_identity, monotonic_now_nanos};

        // Both guardians are proven GONE — pids that cannot be alive with these
        // birth stamps, so `liveness` proves Gone rather than merely Unknown. That
        // is not incidental: it is what makes the un-faulted gate refuse (and
        // therefore destroy), and what lets the recovery sweep below prove the
        // parked record is still ownable.
        let gone = |pid: i32| ProcessIdentity {
            pid,
            birth: protocol::proc_identity::BirthIdentity {
                start_sec: 7,
                start_usec: 7,
            },
        };
        let dead_coordinator = gone(0x3FFF_FFF3);
        let dead_custodian = gone(0x3FFF_FFF4);
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let arm = |uid: &str| {
            let lock = codex_launch::LaunchLock::acquire(uid).unwrap();
            codex_launch::create_pending(
                &lock,
                codex_launch::NewLaunch {
                    launch_nonce: "arrivalnonce".into(),
                    uid: uid.into(),
                    session_name: "cc-arrival".into(),
                    coordinator: dead_coordinator,
                    boot: boot_identity().unwrap(),
                    deadline_monotonic_nanos: far,
                    created_ms: 1,
                },
            )
            .unwrap();
            codex_launch::cas_custodian_with_child(
                &lock,
                uid,
                dead_custodian,
                dead_custodian.pid,
                "cn",
                "ch",
            )
            .unwrap();
            // Released so the gate can take it itself, exactly as a real host's does.
            drop(lock);
        };

        let uid = "arrival-write-fault";
        arm(uid);
        let bogus_socket = "/tmp/cc-nonexistent-arrival-fault.sock";
        // The pre-image, captured whole: the strongest statement of "nothing was
        // published" is that the record is the SAME record, not merely that two
        // fields still read false.
        let before = codex_launch::load(uid).unwrap();
        assert!(!before.host_reached_gate);

        codex_launch::fail_next_record_publish();
        let verdict = late_host_admission(uid, "arrivalnonce", bogus_socket).unwrap();
        let HostAdmission::ParkInert { reason } = &verdict else {
            panic!("a failed arrival write must PARK, not {verdict:?}");
        };
        assert!(
            reason.contains("could not record this host's arrival"),
            "the park must be attributed to the ARRIVAL WRITE, not to some earlier \
             pre-arrival failure that would park for a different reason: {reason}"
        );
        assert!(
            reason.contains("BEFORE the rename"),
            "and the fault that fired must be the pre-publish one — a post-rename \
             fsync fault would mean the flag landed after all: {reason}"
        );

        // NOTHING WAS PUBLISHED. The record a later reader sees is the pre-image.
        let after = codex_launch::load(uid).unwrap();
        assert_eq!(
            after, before,
            "a failed arrival write must leave the record byte-identical"
        );
        assert!(
            !after.host_reached_gate && after.host_identity.is_none(),
            "and specifically must not have recorded the arrival it failed to write"
        );
        assert_eq!(
            after.state,
            LaunchState::Pending,
            "the parked host must not have advanced the launch's state"
        );

        // THE COUNTERFACTUAL. The same record, the same gate, the same host — and no
        // fault. It refuses, and the refusal path destroys this uid's session. That
        // is the verdict the failed write suppressed.
        let twin = "arrival-write-fault-twin";
        arm(twin);
        match late_host_admission(twin, "arrivalnonce", bogus_socket).unwrap() {
            HostAdmission::CleanupOnly { cleanup, .. } => {
                // The bogus socket has no server, and round-4 forbids inferring
                // absence from a socket error, so the destroy reports `Unavailable`.
                // What matters here is that the destroy was REACHED at all.
                assert!(matches!(cleanup, CleanupOutcome::Unavailable(_)));
            }
            other => panic!(
                "without the fault this record must reach the DESTROYING verdict, \
                 or the parked run proves nothing: got {other:?}"
            ),
        }
        assert!(
            codex_launch::load(twin).unwrap().host_reached_gate,
            "and the twin's arrival must have landed, which is what let it proceed"
        );

        // AND THE PARKED RECORD IS STILL OWNABLE. The park released the lock and left
        // a readable `pending` record whose guardians are both gone, so the backstop
        // sweep can still fail it and call for a replacement custodian. This is the
        // half that would be lost if the host had destroyed the session and exited:
        // the record would be an arrival-less absence no sweep could explain.
        let actions = codex_launch::recovery_sweep();
        let mine: Vec<_> = actions
            .iter()
            .filter(|a| match a {
                SweepAction::FailedStalePending { uid: u }
                | SweepAction::NeedsReplacementCustodian { uid: u }
                | SweepAction::Skipped { uid: u, .. } => u == uid,
            })
            .collect();
        assert!(
            mine.iter()
                .any(|a| matches!(a, SweepAction::FailedStalePending { .. })),
            "the sweep must still be able to take the parked record: {mine:?}"
        );
        assert!(
            mine.iter()
                .any(|a| matches!(a, SweepAction::NeedsReplacementCustodian { .. })),
            "and to call for the custodian that will clean it up: {mine:?}"
        );
        assert!(
            !mine
                .iter()
                .any(|a| matches!(a, SweepAction::Skipped { .. })),
            "and must not find it unreadable or its lock stuck: {mine:?}"
        );
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

    /// A real child in its own process group, plus its true recorded identity.
    /// The caller is responsible for reaping it.
    fn spawn_group_leader() -> (std::process::Child, codex_launch::ChildEntry) {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn a group leader");
        let pid = child.id() as i32;
        let birth = protocol::proc_identity::read_birth_identity(pid).expect("birth");
        let pgid = protocol::proc_identity::read_pgid(pid).expect("pgid");
        assert_eq!(pgid, pid, "the stand-in must lead its own group");
        (
            child,
            codex_launch::ChildEntry {
                role: "app-server".into(),
                identity: ProcessIdentity { pid, birth },
                pgid,
                nonce: "n".into(),
                argv_hash: String::new(),
                recorded_by: None,
                exec_confirmed: false,
            },
        )
    }

    fn real_deps(uid: &str) -> RealDeps {
        RealDeps {
            cfg: CustodianCfg {
                uid: uid.into(),
                socket: "/tmp/cc-no-such-server.sock".into(),
                coordinator: protocol::proc_identity::current_identity().unwrap(),
                poll: std::time::Duration::from_millis(1),
            },
            seen_present: std::cell::Cell::new(false),
        }
    }

    /// A11.2: the group kill is warranted by an occupied group id, and that warrant
    /// expires the moment the id could have been recycled.
    ///
    /// Here the recorded leader's pid is occupied by a process with a DIFFERENT
    /// birth stamp — which is exactly what pid reuse looks like from the outside.
    /// The old code read only "the leader's identity is `Gone`" plus "the group has
    /// members" and killed the group, which in this state means SIGKILLing a process
    /// that was never part of the launch.
    #[test]
    fn a_group_whose_leader_pid_was_recycled_is_never_signalled() {
        let (mut child, mut entry) = spawn_group_leader();
        let real_pid = entry.identity.pid;
        // Corrupt ONLY the birth stamp: same pid, same pgid, different process.
        // `liveness` reads this as `Gone` (reuse), which is the arm under test.
        entry.identity.birth.start_usec ^= 0x5A5A;
        assert_eq!(
            liveness(&entry.identity),
            Liveness::Gone,
            "a wrong birth on a live pid must read as Gone, or this proves nothing"
        );
        assert_eq!(
            group_warrant(&entry.identity, this_boot()),
            GroupWarrant::Refused(format!(
                "pid {real_pid} is now a different process, so the group id may have been recycled"
            ))
        );

        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.uid = "abaguard".into();
        record.children = vec![entry];
        real_deps("abaguard").teardown_children(&record);

        // THE GATE: the innocent process is still alive, and STAYS alive.
        //
        // Polled rather than checked once: `teardown_children` returns as soon as
        // the recorded identity reads `Gone`, which a corrupted birth stamp does
        // immediately — so it returns before a signal it did send could land. A
        // single `try_wait` here passes by luck even when the group was killed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            assert!(
                matches!(child.try_wait(), Ok(None)),
                "a group whose recorded leader pid was recycled must NOT be signalled"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A11.2, the other side: the measured case must still work. When the recorded
    /// leader's pid is genuinely FREE, nothing can be leading a fresh group with
    /// that id, so the members still in it inherited it from our leader — and they
    /// are exactly the forked descendants that used to survive every kill.
    ///
    /// Without this, the guard above could be "closed" by never signalling at all.
    #[test]
    fn a_group_whose_leader_pid_is_free_is_still_killed() {
        let (mut leader, entry) = spawn_group_leader();
        let pgid = entry.pgid;
        // A second process that JOINS the leader's group and outlives it — the
        // forked descendant the group kill exists for.
        use std::os::unix::process::CommandExt;
        let mut descendant = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(pgid)
            .spawn()
            .expect("spawn a group member");
        // Kill and REAP the leader, so its pid is genuinely free.
        let _ = leader.kill();
        let _ = leader.wait();
        assert_eq!(
            group_warrant(&entry.identity, this_boot()),
            GroupWarrant::Warranted,
            "a freed leader pid must still warrant the group kill"
        );
        assert!(
            group_has_members(pgid),
            "the descendant must still hold the group open"
        );

        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.uid = "abasurvivor".into();
        record.children = vec![entry];
        // The group-kill arm is warranted only WITHIN the boot that recorded the
        // group (round-4 finding 10), and `base_record`'s fixture boot is a fake.
        // These children are real processes of THIS boot, so the record has to say
        // so or the test would prove the refusal rather than the kill.
        record.boot = this_boot();
        real_deps("abasurvivor").teardown_children(&record);

        // THE GATE: the descendant was reached by the group signal.
        let reaped = descendant.wait().expect("reap the descendant");
        assert!(
            !reaped.success(),
            "the surviving group member must have been SIGKILLed"
        );
    }

    /// **A11.2's worst arm: cross-BOOT pgid reuse** (round-4 finding 10).
    ///
    /// The unoccupied-leader arm is deliberately uncorroborated within one boot, and
    /// the recorded bound used to say the unrelated-group risk needs pid wraparound.
    /// Across a reboot it needs nothing: pids restart from the bottom, so the new
    /// boot reaches the recorded pgid in the ordinary course of starting up, and a
    /// new occupant that has exited while its children live puts the arm in exactly
    /// the state that authorizes `SIGKILL` — on a process group from a machine
    /// lifetime this launch never touched.
    ///
    /// Staged as the real thing rather than argued: the SAME live group that the
    /// test above proves IS killed within its own boot must be refused when the
    /// record names a different boot. Nothing but the boot field changes between the
    /// two, which is what makes the boot field the cause.
    #[test]
    fn a_group_recorded_under_a_different_boot_is_never_signalled() {
        let (mut leader, entry) = spawn_group_leader();
        let pgid = entry.pgid;
        use std::os::unix::process::CommandExt;
        let mut descendant = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(pgid)
            .spawn()
            .expect("spawn a group member");
        let _ = leader.kill();
        let _ = leader.wait();

        // The premise: within THIS boot the arm warrants the kill. Asserted first so
        // a refusal below cannot be a vacuous pass on some other precondition.
        assert_eq!(
            group_warrant(&entry.identity, this_boot()),
            GroupWarrant::Warranted,
            "the within-boot warrant is the premise this test inverts"
        );

        let prior_boot = protocol::proc_identity::BootIdentity {
            boot_sec: this_boot().boot_sec - 1,
            boot_usec: this_boot().boot_usec,
        };
        match group_warrant(&entry.identity, prior_boot) {
            GroupWarrant::Refused(why) => assert!(
                why.contains("rebooted"),
                "the refusal must name the reason it refused: {why}"
            ),
            other => panic!("a cross-boot pgid must never be signalled: {other:?}"),
        }

        // …and end to end, through the caller that does the signalling.
        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.uid = "abareboot".into();
        record.children = vec![entry];
        record.boot = prior_boot;
        real_deps("abareboot").teardown_children(&record);

        // THE GATE: the group is untouched. A stranger's group surviving is the
        // whole point — this is the one place in A11.2's residual where failing
        // closed costs nothing, because nothing of ours can outlive a reboot.
        assert!(
            group_has_members(pgid),
            "a group recorded under another boot must be left alone"
        );
        let _ = descendant.kill();
        let _ = descendant.wait();
    }

    /// A11.2, the live-child half: "the recorded identity is alive" is not "it is
    /// still in the group we recorded", and the group signal needs the second fact.
    ///
    /// A record claiming `pgid == pid` for a process that is NOT a group leader used
    /// to send `kill(-pid, ...)` at a group id that does not exist. That returns
    /// `ESRCH`, which this code reads as "it went away anyway" — so the child was
    /// silently never signalled at all. Re-proving membership against the kernel
    /// turns that into a signal by pid, which actually reaches it.
    #[test]
    fn a_live_child_that_is_not_in_its_recorded_group_is_still_signalled() {
        // NO `process_group`: this child sits in the test runner's group, so the
        // recorded `pgid == pid` below is a claim the kernel does not back.
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        let birth = protocol::proc_identity::read_birth_identity(pid).expect("birth");
        assert_ne!(
            protocol::proc_identity::read_pgid(pid),
            Some(pid),
            "the stand-in must NOT lead its own group, or this proves nothing"
        );
        let entry = codex_launch::ChildEntry {
            role: "app-server".into(),
            identity: ProcessIdentity { pid, birth },
            // The stale/false claim under test.
            pgid: pid,
            nonce: "n".into(),
            argv_hash: String::new(),
            recorded_by: None,
            exec_confirmed: false,
        };
        assert_eq!(liveness(&entry.identity), Liveness::Alive);

        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.uid = "pgidmismatch".into();
        record.children = vec![entry];
        real_deps("pgidmismatch").teardown_children(&record);

        // THE GATE: it was actually reached, not signalled at a group id that has
        // no members and reported as "already gone".
        let reaped = child.wait().expect("reap");
        assert!(
            !reaped.success(),
            "a live child must be signalled by pid when it is not in its recorded group"
        );
    }

    /// A11.5: the delete is bound to the INODE the marker was read from, not to the
    /// path it was reached by.
    ///
    /// The window this closes: the custodian read the owner marker by path and then
    /// called `remove_dir_all` on that same path, re-resolving it. In sticky,
    /// world-writable `/tmp` a same-uid process — or a legitimate colliding launch,
    /// since the run-dir derivation is many-to-one — could swap the directory
    /// between those two steps, and the thing deleted would not be the thing
    /// verified.
    ///
    /// The swap is performed here in the middle, deterministically, rather than
    /// raced: the descriptor is taken, the name is then repointed at an impostor,
    /// and the removal must still empty the directory the descriptor names.
    #[test]
    fn the_sweep_deletes_the_inode_it_verified_not_the_name() {
        let base = std::env::temp_dir().join(format!("cc-a11-5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ours = base.join("run");
        std::fs::create_dir_all(ours.join("nested")).unwrap();
        std::fs::write(ours.join("as.sock.log"), b"ours").unwrap();
        std::fs::write(ours.join("nested/deep"), b"ours").unwrap();

        // The descriptor is taken while the name still points at OUR directory —
        // this is the moment the real sweep reads the marker.
        let fd = codex_launch::open_dir_nofollow(&ours).expect("open the run dir");

        // Now the swap. `ours` is moved aside and an impostor takes the name.
        let moved = base.join("moved");
        std::fs::rename(&ours, &moved).unwrap();
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("sentinel"), b"impostor").unwrap();

        codex_launch::remove_tree_beneath(&fd, 0, None).expect("empty the verified directory");
        drop(fd);

        // THE GATE: the verified inode was emptied — including its subdirectory...
        assert!(
            moved.exists(),
            "the verified directory itself is not removed by this step"
        );
        assert_eq!(
            std::fs::read_dir(&moved).unwrap().count(),
            0,
            "the verified inode must have been emptied, nested contents included"
        );
        // ...and the impostor that took the NAME was never touched.
        assert_eq!(
            std::fs::read(ours.join("sentinel")).unwrap(),
            b"impostor",
            "the directory that took the name must be untouched: the delete follows \
             the descriptor, not the path"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A11.5, the other half of the same binding: the marker is read through the
    /// descriptor too, so the verdict describes the directory that will actually be
    /// emptied rather than whatever now answers to the name.
    #[test]
    fn the_owner_marker_is_read_through_the_descriptor() {
        let base = std::env::temp_dir().join(format!("cc-a11-5m-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ours = base.join("run");
        std::fs::create_dir_all(&ours).unwrap();
        codex_launch::write_owner_marker(&ours, "uid-real", "nonce-real").unwrap();

        let fd = codex_launch::open_dir_nofollow(&ours).expect("open");

        // Swap the name to a directory claiming a DIFFERENT owner.
        let moved = base.join("moved");
        std::fs::rename(&ours, &moved).unwrap();
        std::fs::create_dir_all(&ours).unwrap();
        codex_launch::write_owner_marker(&ours, "uid-other", "nonce-other").unwrap();

        // Through the fd: still ours. By path: the impostor's.
        assert_eq!(
            codex_launch::run_dir_marker_at(fd.0, "uid-real", "nonce-real"),
            codex_launch::MarkerVerdict::Ours,
            "the descriptor must still name the directory whose marker we wrote"
        );
        assert_eq!(
            codex_launch::run_dir_marker(&ours, "uid-real", "nonce-real"),
            codex_launch::MarkerVerdict::Foreign,
            "and the path must now resolve to the impostor — which is the whole gap"
        );
        drop(fd);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A11.5, end to end through `sweep_run_dir`: a marked run dir with nested
    /// contents is removed whole, and a tree deeper than the sweep will descend is
    /// REFUSED rather than removed by some other route.
    ///
    /// The depth arm is what pins the wiring. `remove_dir_all` — the path-addressed
    /// call this replaced — has no such bound, so a sweep that quietly went back to
    /// it would remove the deep tree and report success here.
    #[test]
    fn the_sweep_removes_a_nested_run_dir_and_refuses_an_unreasonably_deep_one() {
        let uid = "inodesweep";
        let nonce = codex_launch::mint_nonce();
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        let _ = std::fs::remove_dir_all(&ours);
        std::fs::create_dir_all(ours.join("a/b/c")).unwrap();
        std::fs::write(ours.join("a/b/c/deep"), b"x").unwrap();
        std::fs::write(ours.join("as.stderr"), b"x").unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();

        let mut record = base_record(fail(), CleanupState::Pending, false);
        record.uid = uid.into();
        record.launch_nonce = nonce.clone();
        record.run_dir = ours.to_str().map(|s| s.to_string());

        assert!(
            real_deps(uid).sweep_run_dir(&record),
            "a marked run dir with nested contents must be swept"
        );
        assert!(!ours.exists(), "and removed whole: {}", ours.display());

        // Now a tree deeper than the sweep is willing to descend.
        let mut deep = ours.clone();
        for _ in 0..(codex_launch::SWEEP_MAX_DEPTH + 4) {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();
        assert!(
            !real_deps(uid).sweep_run_dir(&record),
            "a tree deeper than the sweep descends must leave cleanup pending, \
             not be removed by an unbounded path-addressed delete"
        );
        assert!(
            ours.exists(),
            "and the directory must still be there to retry on"
        );

        // A11.5, the half a single pass cannot see: the failed pass must still hold
        // its own RETRY WARRANT. The owner marker is the only thing that makes this
        // directory provably ours, and the sweep used to unlink it with every other
        // entry — so the second pass read the missing marker as `Foreign`, reported
        // DEALT WITH, and the record went `Complete` over an unremoved tree with
        // nobody left to notice.
        assert!(
            ours.join(codex_launch::RUN_DIR_OWNER_FILE).exists(),
            "the owner marker must OUTLIVE a failed pass — it is the warrant the \
             next pass needs to prove the directory is this launch's"
        );
        assert!(
            !real_deps(uid).sweep_run_dir(&record),
            "so a SECOND pass over the same unremoved tree must still report retry, \
             never a success that abandons it"
        );
        assert!(ours.exists(), "and still leave it standing to retry on");

        // Once the obstruction is gone, the retry the warrant preserved completes.
        let mut top = ours.clone();
        top.push("d");
        std::fs::remove_dir_all(&top).unwrap();
        assert!(
            real_deps(uid).sweep_run_dir(&record),
            "and the pass that finally can finish, does"
        );
        assert!(!ours.exists(), "removed whole: {}", ours.display());
        let _ = std::fs::remove_dir_all(&ours);
    }

    /// **Round-2 finding 5: a straggler after the enumeration must not settle the
    /// directory.**
    ///
    /// The one failure the marker's "unlinked last" discipline could not cover, and
    /// the reason it could not: the marker lives INSIDE the directory `rmdir`
    /// removes, so it has to go first. A process that creates an entry after the
    /// sweep has listed the directory therefore makes `rmdir` return `ENOTEMPTY`
    /// with the warrant already gone — and the next pass reads a missing marker as
    /// `Foreign`, ignores its own `remove_dir` failure, and reports `Settled`.
    /// Cleanup goes `Complete` over a tree that is still standing.
    ///
    /// The boot this test process is running under — the only boot under which a
    /// group warrant can be granted (round-4 finding 10).
    fn this_boot() -> BootIdentity {
        protocol::proc_identity::boot_identity().expect("this boot's identity is readable")
    }

    /// Every warrant-restore staging file left in `dir`, by name.
    ///
    /// Scans for the PREFIX, not one fixed name: the staging name is unique per
    /// attempt (round-4 finding 3), so `dir.join(".owner.restore")` would assert
    /// about a file that is never created under that exact name — which is how the
    /// litter assertion came to be unobservable in the first place.
    fn staging_litter(dir: &std::path::Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .expect("the run dir is readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(codex_launch::MARKER_RESTORE_TEMP))
            .collect();
        found.sort();
        found
    }

    /// So the failing arm restores the marker, and the second pass is the assertion
    /// that matters: it must still say RETRY.
    #[test]
    fn a_straggler_after_the_enumeration_leaves_the_sweep_owed_not_settled() {
        let uid = "straggler";
        let nonce = codex_launch::mint_nonce();
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        let _ = std::fs::remove_dir_all(&ours);
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("as.stderr"), b"x").unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();

        codex_launch::straggle_next_sweep();
        let first = codex_launch::sweep_owned_run_dir(&ours, uid, &nonce);
        assert!(
            matches!(first, codex_launch::RunDirSweep::Retry(_)),
            "a failed rmdir is unfinished work, not a settled directory: {first:?}"
        );
        assert!(ours.exists(), "and the tree is still standing");
        assert!(
            ours.join(codex_launch::STRAGGLER_FILE).exists(),
            "the straggler is what made rmdir fail"
        );
        // THE GATE. Without the restore this file is gone, the marker reads
        // `Foreign`, and the next line gets `Settled` over a live directory.
        assert!(
            ours.join(codex_launch::RUN_DIR_OWNER_FILE).exists(),
            "the deletion warrant must be RESTORED when rmdir fails after it was \
             unlinked — it is the only thing that makes this directory provably ours"
        );
        // **Existing is not the same as being a WARRANT** (round-3 finding 3). A
        // half-written or empty file at the marker's name exists too, and the reader
        // files it as `Foreign` — the verdict that settles and abandons. So the
        // restored marker is put to the only test that matters: what the reader says
        // about it.
        assert_eq!(
            codex_launch::run_dir_marker(&ours, uid, &nonce),
            codex_launch::MarkerVerdict::Ours,
            "a restored warrant must READ as ours, not merely occupy the name"
        );
        assert_eq!(
            std::fs::read(ours.join(codex_launch::RUN_DIR_OWNER_FILE)).unwrap(),
            format!("{uid}\n{nonce}\n").into_bytes(),
            "and be byte-identical to the warrant the host originally wrote"
        );
        // No staging litter is left behind either.
        assert_eq!(
            staging_litter(&ours),
            Vec::<String>::new(),
            "the staging file must not survive a successful restore"
        );
        // Round-4 finding 4: the restored warrant is 0600 as a FACT, not as a
        // creation-mode request the umask may have narrowed. A mode-000 marker reads
        // `EACCES` ⇒ `Unknown` ⇒ Pending for ever, on a restore that reported success.
        assert_eq!(
            protocol::fsperm::mode_of(&ours.join(codex_launch::RUN_DIR_OWNER_FILE)).unwrap(),
            0o600,
            "a restored warrant must be readable by the passes that have to read it"
        );

        let second = codex_launch::sweep_owned_run_dir(&ours, uid, &nonce);
        assert!(
            matches!(second, codex_launch::RunDirSweep::Settled(None)),
            "and the retry the warrant preserved removes the straggler too: {second:?}"
        );
        assert!(!ours.exists(), "removed whole: {}", ours.display());
        let _ = std::fs::remove_dir_all(&ours);
    }

    /// **A restore that FAILS is reported, and never as a settled directory**
    /// (round-3 finding 3).
    ///
    /// The straggler test above proves the restore works. This one proves what
    /// happens when it does not — the residual the arm's own comment claims is "not
    /// pretended away", which nothing tested. Only the filesystem decides when a
    /// write fails, so the failure is injected at the seam.
    ///
    /// **The seam fires AFTER the staging file is created** (round-4 finding 8). It
    /// used to return at the top of `restore_marker_at`, before `.owner.restore`
    /// existed — which made "a failed restore leaves no staging litter behind" an
    /// assertion about a file nothing could have created. Deleting the staging
    /// cleanup left it green. Now the fault reaches the arm a real write failure
    /// reaches: a staging file exists, and the assertion is what proves it was
    /// removed. Because the staging name is unique per attempt (round-4 finding 3),
    /// the assertion scans for the PREFIX rather than one fixed name.
    ///
    /// Three things must hold, and the third is stated correctly here for the first
    /// time. The pass must say the warrant was lost, in as many words, rather than
    /// reporting the `rmdir` failure alone. It must leave no staging litter. And the
    /// pass AFTER it must report `Settled(Some(_))` — this is the recorded residual,
    /// not a contradiction of it: with its warrant gone the directory reads
    /// `Foreign`, and a POPULATED foreign directory is one this code may never touch,
    /// so settling is the only answer that does not wedge cleanup for ever on a
    /// stranger's tree. What the earlier prose here claimed — "the pass after it must
    /// NOT report the directory settled" — contradicted the assertion directly below
    /// it and described a different, unimplemented policy.
    #[test]
    fn a_warrant_that_cannot_be_restored_is_reported_and_never_settles() {
        let uid = "straggler-nofix";
        let nonce = codex_launch::mint_nonce();
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        let _ = std::fs::remove_dir_all(&ours);
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("as.stderr"), b"x").unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();

        codex_launch::straggle_next_sweep();
        codex_launch::fail_next_marker_restore();
        let first = codex_launch::sweep_owned_run_dir(&ours, uid, &nonce);
        match &first {
            codex_launch::RunDirSweep::Retry(why) => assert!(
                why.contains("could not be restored"),
                "the pass must name the residual it actually hit — a lost warrant is \
                 worse than a failed rmdir and must not be reported as one: {why}"
            ),
            other => panic!("a lost warrant is owed work, not settled: {other:?}"),
        }
        assert!(ours.exists(), "the tree is still standing");
        assert!(
            !ours.join(codex_launch::RUN_DIR_OWNER_FILE).exists(),
            "the premise: the warrant really is gone, or the pass below proves nothing"
        );
        assert_eq!(
            staging_litter(&ours),
            Vec::<String>::new(),
            "and a failed restore leaves no staging litter behind — the fault fires \
             AFTER the staging file exists, so this assertion has something to see \
             and deleting the cleanup makes it RED"
        );

        // THE GATE. The next pass reads a missing marker as `Foreign`. A populated
        // foreign directory settles — see the doc above for why that is the recorded
        // residual and not the "cleanup complete, leftovers until reboot" defect.
        assert_eq!(
            codex_launch::run_dir_marker(&ours, uid, &nonce),
            codex_launch::MarkerVerdict::Foreign,
            "the premise: without its warrant the directory reads as a stranger's"
        );
        let second = codex_launch::sweep_owned_run_dir(&ours, uid, &nonce);
        assert!(
            matches!(second, codex_launch::RunDirSweep::Settled(Some(_))),
            "a POPULATED foreign directory settles — retrying would wedge for ever on \
             a directory we may never touch: {second:?}"
        );
        assert!(
            ours.exists(),
            "and it is left standing, untouched, because it is not provably ours"
        );
        let _ = std::fs::remove_dir_all(&ours);
    }

    /// **The Foreign arm tells "not ours" from "could not tell"** (round-3 finding 3).
    ///
    /// A missing marker reads `Foreign`, and this arm then tries to collect what may
    /// be its own empty residue. The `remove_dir` used to be `let _ = …`: every
    /// outcome, including failures it never looked at, was reported `Settled` —
    /// "nothing is owed here" — which is what turns an unfinished cleanup into a
    /// completed one with leftovers until the next reboot.
    ///
    /// Staged against a real `EACCES`: an EMPTY unmarked directory whose PARENT is
    /// not writable, so `rmdir` genuinely cannot remove it for a reason that says
    /// nothing whatever about whose it is.
    #[test]
    fn a_foreign_directory_that_cannot_be_collected_is_owed_not_settled() {
        let parent = std::env::temp_dir().join(format!("cc-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let dir = parent.join("run");
        std::fs::create_dir(&dir).unwrap();

        // No marker at all, so the verdict is Foreign; and empty, so this is exactly
        // the residue shape the arm exists to collect.
        assert_eq!(
            codex_launch::run_dir_marker(&dir, "someuid", "somenonce"),
            codex_launch::MarkerVerdict::Foreign
        );

        // Control: with a writable parent it IS collected, and settles.
        let control = parent.join("run2");
        std::fs::create_dir(&control).unwrap();
        assert!(
            matches!(
                codex_launch::sweep_owned_run_dir(&control, "someuid", "somenonce"),
                codex_launch::RunDirSweep::Settled(None)
            ),
            "the premise: an empty foreign residue IS collectable when nothing stops it"
        );

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
        let verdict = codex_launch::sweep_owned_run_dir(&dir, "someuid", "somenonce");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        match &verdict {
            codex_launch::RunDirSweep::Retry(why) => assert!(
                why.contains("could not be completed"),
                "and it must say the collection failed, not that nothing was owed: {why}"
            ),
            other => panic!(
                "THE GATE: a removal that failed for an unexplained reason must not be \
                 reported as a settled directory: {other:?}"
            ),
        }
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// **Round-2 finding 6: a `readdir` that FAILS is not a directory that ENDED.**
    ///
    /// `readdir` reports end-of-directory and a read error the same way — NULL — and
    /// they are told apart only by `errno`. Read as EOF unconditionally, an I/O
    /// error part way through made a partial listing look like a complete one: the
    /// sweep unlinked the marker and reported success over entries it never saw.
    #[test]
    fn a_failed_enumeration_is_not_an_emptied_directory() {
        let uid = "readdirfail";
        let nonce = codex_launch::mint_nonce();
        let ours = crate::codex_coordinator::choose_run_dir(uid, &nonce).unwrap();
        let _ = std::fs::remove_dir_all(&ours);
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("as.stderr"), b"x").unwrap();
        codex_launch::write_owner_marker(&ours, uid, &nonce).unwrap();

        // Pinned, because "the marker is there" is not the claim — `sweep_owned_run_dir`
        // RESTORES a marker it unlinked when the `rmdir` behind it fails, so a
        // sweep that wrongly ran to completion on a partial listing would ALSO
        // leave a marker at this path. The claim is that the marker was never
        // touched, and only its inode says that.
        use std::os::unix::fs::MetadataExt;
        let marker = ours.join(codex_launch::RUN_DIR_OWNER_FILE);
        let before = std::fs::metadata(&marker).unwrap().ino();

        codex_launch::fail_next_readdir();
        let first = codex_launch::sweep_owned_run_dir(&ours, uid, &nonce);
        assert!(
            matches!(first, codex_launch::RunDirSweep::Retry(_)),
            "an enumeration that errored must leave the directory owed: {first:?}"
        );
        assert_eq!(
            std::fs::metadata(&marker).unwrap().ino(),
            before,
            "THE GATE: the failure must be propagated BEFORE any unlinking, so the \
             warrant is the ORIGINAL file — not one the rmdir arm put back after \
             a sweep that believed a partial listing was the whole directory"
        );
        assert!(
            ours.join("as.stderr").exists(),
            "and the tree is exactly as it was found"
        );

        let second = codex_launch::sweep_owned_run_dir(&ours, uid, &nonce);
        assert!(
            matches!(second, codex_launch::RunDirSweep::Settled(None)),
            "and the pass whose enumeration works, finishes: {second:?}"
        );
        assert!(!ours.exists(), "removed whole: {}", ours.display());
        let _ = std::fs::remove_dir_all(&ours);
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
