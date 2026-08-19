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
    fn cas_failed(&self, reason: &str) -> Result<()>;
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
                        deps.mark_clean_complete()?;
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
            CleanupState::NotRequired => Ok(Tick::Done),
            CleanupState::Pending => resolve_cleanup(deps, &record),
        },
    }
}

/// The `failed{cleanup:pending}` branch: destroy the uid's session and decide
/// whether the custodian is done or must stay armed.
fn resolve_cleanup<D: CustodianDeps>(deps: &D, record: &LaunchRecord) -> Result<Tick> {
    match deps.destroy() {
        CleanupOutcome::Killed => {
            deps.mark_clean_complete()?;
            Ok(Tick::CleanedAndDone)
        }
        CleanupOutcome::Absent => {
            if record.new_session_indeterminate {
                if deps.boot_changed(record) {
                    // The escape (finding 5): a reboot means any late session the
                    // indeterminate mutation might still have created cannot exist
                    // — the pre-reboot process that would create it is gone. So a
                    // proven absence now IS terminal; the custodian is not armed
                    // literally forever.
                    deps.mark_clean_complete()?;
                    return Ok(Tick::AlreadyAbsentDone);
                }
                // No guessed grace period: one absence is not proof after an
                // indeterminate new-session. Stay armed.
                Ok(Tick::StayArmedIndeterminate)
            } else {
                deps.mark_clean_complete()?;
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
            deps.mark_clean_complete()?;
            Ok(Tick::CleanedAndDone)
        }
        // Refuse to pick between duplicates, and never read Unavailable as gone.
        // BUT the reboot escape must fire here too (round-5 finding 5): a
        // post-reboot / no-server result is `Unavailable`, so the escape checked
        // only on `Absent` would never fire — a custodian could retry forever. A
        // reboot means the session (and any late one) cannot exist, so a proven
        // boot change is terminal even from the retry path.
        CleanupOutcome::Ambiguous(_) | CleanupOutcome::Unavailable(_) => {
            if deps.boot_changed(record) {
                deps.mark_clean_complete()?;
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
            Ok(record) => match record.server_a {
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
            },
            Err(_) => {
                CleanupOutcome::Unavailable("launch record unreadable — failing closed".into())
            }
        }
    }
    fn cas_failed(&self, reason: &str) -> Result<()> {
        // Bounded lock (round-5 finding 6): the custodian is the SOLE cleanup
        // owner — a stopped holder must never wedge it.
        let lock = LaunchLock::acquire_bounded(&self.cfg.uid, LOCK_BUDGET)?;
        codex_launch::to_failed(&lock, &self.cfg.uid, reason, CleanupState::Pending)?;
        Ok(())
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
    let deps = RealDeps { cfg };
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
    /// Anything else. The host ran cleanup-only and reports the outcome.
    CleanupOnly {
        reason: String,
        cleanup: CleanupOutcome,
    },
}

/// The `internal-codex-host-preflight` subcommand: the D7 gate a tmux-started
/// `codex-host` runs **before it creates anything**. It validates the launch
/// record and takes a live lease, or refuses and runs cleanup-only. Exits `0`
/// when admitted (the real host bring-up — app-server/broker/TUI — is 2d), and
/// `75` (`EX_TEMPFAIL`) on a cleanup-only refusal so the caller knows Codex must
/// not launch. Dispatchable now so the gate is exercised end-to-end; the real
/// host embeds this same [`late_host_admission`] call ahead of its bring-up.
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
        Err(_) => std::process::exit(70),
    }
}

/// The gate a tmux-started `codex-host` runs before it creates anything (D7).
/// On admission it holds the lease and may proceed (the real host bring-up is
/// 2d). On any doubt it destroys **only its own** uid's session and refuses —
/// "a frozen tmux server may run the old `new-session` arbitrarily late, but
/// what it runs cannot start Codex and removes its own stale session."
pub fn late_host_admission(uid: &str, nonce: &str, socket: &str) -> Result<HostAdmission> {
    let host = codex_launch::require_current_identity()?;
    // The host's own process group, recorded with the exclusive lease (finding
    // 9). A pgid we cannot read is fail-closed: refuse and clean up.
    let host_pgid = match protocol::proc_identity::read_pgid(host.pid) {
        Some(pgid) => pgid,
        None => {
            let cleanup = protocol::tmux::destroy_owned_session(socket, uid, None);
            return Ok(HostAdmission::CleanupOnly {
                reason: "could not read the host's own pgid".into(),
                cleanup,
            });
        }
    };
    // Bounded (round-5 finding 6): host admission must not hang on a stopped
    // lock holder.
    let lock = LaunchLock::acquire_bounded(uid, LOCK_BUDGET)?;
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
        cas_calls: RefCell<Vec<String>>,
        completed: RefCell<bool>,
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
                cas_calls: RefCell::new(vec![]),
                completed: RefCell::new(false),
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
                coordinator: current_identity().unwrap(),
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
        // Valid: pending, right nonce, live coordinator (us) ⇒ admitted.
        assert_eq!(
            late_host_admission(uid, "hostnonce", bogus_socket).unwrap(),
            HostAdmission::Admitted
        );
        // Wrong nonce ⇒ cleanup-only refusal.
        assert!(matches!(
            late_host_admission(uid, "wrong", bogus_socket).unwrap(),
            HostAdmission::CleanupOnly { .. }
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
}
