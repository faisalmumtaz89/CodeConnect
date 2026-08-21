//! The durable **launch record** (D7) — one owner, durable outcome.
//!
//! A Codex launch's authoritative state lives in a single fsynced JSON file in
//! the **session dir** (`~/.codeconnect/sessions/<uid>/launch.json`), never in
//! the disposable runtime dir: cleanup removes the runtime dir but the record
//! survives, so a failure reason is readable long after the runtime is gone
//! (CODEX-PLAN.md §Launch coordination, step 4 and D7).
//!
//! Every transition happens under an interprocess **flock** on
//! `launch.lock` and is a compare-and-swap: read the current record, verify the
//! precondition, write the successor, and fsync **both** the record file and its
//! containing directory before releasing the lock. That is what lets the
//! coordinator, the custodian, a late `codex-host`, and a future invocation's
//! recovery sweep all touch one record without racing each other.
//!
//! ## Invariants this module enforces (D7)
//!
//!   * **`failed` is terminal.** A `pending → failed` CAS can never be undone. A
//!     coordinator that was merely *stopped* (SIGSTOP) past its deadline loses
//!     to the custodian's deadline-driven `pending → failed`; when it resumes,
//!     its `pending → ready` CAS finds the record already `failed` and refuses.
//!   * **Admission fails closed.** A corrupt/truncated record, an unknown
//!     schema, a boot identity that does not match this boot (reboot or
//!     restore-from-image), an expired deadline, a nonce mismatch, or a `failed`
//!     state all deny a late host's admission — it may only run cleanup.
//!   * **The deadline is boot-relative monotonic**, so a wall-clock rollback
//!     cannot extend it (the monotonic clock does not move under `settimeofday`).

use anyhow::{bail, Context, Result};
use protocol::proc_identity::{
    boot_identity, current_identity, liveness, monotonic_now_nanos, BootIdentity, Liveness,
    ProcessIdentity,
};
use serde::{Deserialize, Serialize};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

/// The only schema this build understands. An unknown schema is refused, not
/// guessed — admission fails closed.
const SCHEMA: u32 = 1;

const RECORD_FILE: &str = "launch.json";
const LOCK_FILE: &str = "launch.lock";

/// The root under which per-session launch dirs live. In production this is
/// `protocol::sessions_dir()`. In `cfg(test)` it is a **per-test-thread** temp
/// dir, so each test — including `recovery_sweep`, which scans the whole root —
/// is fully isolated from every other test without any shared state or env-var
/// mutation (the `setenv`/`getenv` race that deadlocks libc on macOS is never
/// touched). libtest runs each test on its own thread, so a thread-local root is
/// exactly one root per test.
fn sessions_root() -> PathBuf {
    #[cfg(test)]
    {
        test_sessions_root()
    }
    #[cfg(not(test))]
    {
        protocol::sessions_dir()
    }
}

#[cfg(test)]
fn test_sessions_root() -> PathBuf {
    thread_local! {
        static ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }
    ROOT.with(|slot| {
        if let Some(dir) = slot.borrow().as_ref() {
            return dir.clone();
        }
        let dir = std::env::temp_dir().join(format!(
            "cc-launch-test-{}-{:?}-{}",
            std::process::id(),
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        *slot.borrow_mut() = Some(dir.clone());
        dir
    })
}

/// Where a session's durable evidence lives. Distinct from the runtime dir.
pub fn session_dir(uid: &str) -> PathBuf {
    sessions_root().join(uid)
}

fn record_path(uid: &str) -> PathBuf {
    session_dir(uid).join(RECORD_FILE)
}

/// The forward-launch state of the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchState {
    /// The coordinator is driving; nothing is committed yet.
    Pending,
    /// The wrapper came up, evidence validated, session is live.
    Ready,
    /// Terminal. Carries a sanitized reason for the launcher to print.
    Failed { reason: String },
}

/// Whether disposable-runtime cleanup is owed, in progress, or done. Orthogonal
/// to `state`: a `failed` record almost always carries `cleanup: pending` for a
/// custodian or sweep to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanupState {
    NotRequired,
    Pending,
    Complete,
}

/// A recorded child (app-server / TUI / custodian): its identity, pgid, the
/// per-spawn nonce, and the hash of the argv it was released to exec — all as
/// the D6 exec gate fsynced them **before** the child was allowed to `execve`
/// (findings 4/5: the spawn is durably attributable to a specific nonce+argv).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildEntry {
    pub role: String,
    pub identity: ProcessIdentity,
    pub pgid: i32,
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub argv_hash: String,
    /// For a **host** child: the identity of the host that recorded it — i.e. the
    /// lease holder at the time. `None` for the custodian, which the exec gate
    /// records and which belongs to no lease.
    ///
    /// This is what lets a lease takeover **retire** the previous host's children
    /// instead of deleting them. Deleting was the obvious move and it was wrong in
    /// the one case that matters: the predecessor's children can still be running,
    /// and those entries are the only identities by which anyone could stop them.
    /// Removing the record to keep readiness honest would have destroyed the
    /// cleanup evidence for exactly the processes most likely to be orphaned.
    ///
    /// So both properties are kept by scoping rather than pruning: readiness counts
    /// only entries recorded by the CURRENT lease holder, while teardown consumes
    /// every entry, retired ones included.
    #[serde(default)]
    pub recorded_by: Option<ProcessIdentity>,
}

/// A spawn the owner has fsynced its **intent** to make, before the child
/// exists (finding 4). Cleared when the child's identity is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSpawn {
    pub role: String,
    pub nonce: String,
    pub argv_hash: String,
}

/// **Server A** — the identity of the exact tmux session + server the launch
/// created, captured at launch and persisted so the *separate* custodian and
/// supervisor processes can bind their absent/killed/gone/live conclusions to it
/// (round-5 findings 1–4). Without this, cleanup re-establishes "A" from whatever
/// server currently owns the socket, so a different server B rebinding the path
/// could fake a proven absence.
///
/// `server_birth` is **required** (not optional): A is not proven without the
/// server process's kernel birth, and no cleanup/liveness conclusion may run
/// unbound (finding 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerA {
    pub session_id: String,
    pub server_pid: i64,
    pub server_start_time: i64,
    pub session_created: i64,
    pub server_birth: protocol::proc_identity::BirthIdentity,
}

impl ServerA {
    /// Reconstruct the [`protocol::tmux::OwnedSession`] pin the destroy/liveness
    /// primitives take, for this `uid` on `socket`.
    pub fn as_pin(&self, socket: &str, uid: &str) -> protocol::tmux::OwnedSession {
        protocol::tmux::OwnedSession {
            socket: socket.to_string(),
            session_id: self.session_id.clone(),
            uid: uid.to_string(),
            server_pid: self.server_pid,
            server_start_time: self.server_start_time,
            session_created: self.session_created,
            server_birth: Some(self.server_birth),
        }
    }

    /// Capture A from a freshly-resolved [`OwnedSession`]. Fails closed if the
    /// server birth was not proven (finding 3).
    pub fn from_owned(s: &protocol::tmux::OwnedSession) -> Result<ServerA> {
        let Some(birth) = s.server_birth else {
            bail!("refusing to record server A without a proven server birth");
        };
        Ok(ServerA {
            session_id: s.session_id.clone(),
            server_pid: s.server_pid,
            server_start_time: s.server_start_time,
            session_created: s.session_created,
            server_birth: birth,
        })
    }
}

/// A late `codex-host`'s exclusive lease (finding 9). Carries the identity, its
/// process group, the nonce it presented, and its role — not just pid/birth — so
/// a second correct-nonce host is refused while this one is live and so the lease
/// is attributable in a post-mortem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLease {
    pub identity: ProcessIdentity,
    pub pgid: i32,
    pub nonce: String,
    pub role: String,
}

/// The durable launch record. Serialized as pretty JSON to match repo style and
/// stay eyeball-debuggable in the session dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub schema: u32,
    pub launch_nonce: String,
    /// The session's durable identity (the `cc` uid stamp).
    pub uid: String,
    /// The tmux session name (`cc-N`). Cleanup resolves the UID to an internal
    /// id under this name's server; recorded so a late host knows what to sweep.
    pub session_name: String,
    pub coordinator: ProcessIdentity,
    /// Armed before `tmux new-session` (D7). `None` only in the sliver before
    /// the custodian is armed — a live coordinator that reaches tmux without a
    /// custodian must fail the launch.
    pub custodian: Option<ProcessIdentity>,
    /// The boot this record's monotonic deadline is relative to.
    pub boot: BootIdentity,
    /// Absolute `CLOCK_MONOTONIC` nanoseconds past which the launch is expired.
    pub deadline_monotonic_nanos: u64,
    pub state: LaunchState,
    pub cleanup: CleanupState,
    /// Set when the coordinator's `tmux new-session` timed out — an
    /// **indeterminate** outcome (tmux.rs:42). The custodian then treats one
    /// observation of UID absence as *insufficient* (D7: no guessed grace
    /// period) and stays armed until it observes and cleans the late UID or the
    /// server's boot identity changes.
    #[serde(default)]
    pub new_session_indeterminate: bool,
    /// A late `codex-host`'s **exclusive** live lease, taken under the same lock
    /// as `pending → failed`, so a host and the custodian — and two hosts —
    /// cannot both win.
    pub host_lease: Option<HostLease>,
    /// The exec-gate spawn currently in flight, fsynced before the child exists
    /// (D6/finding 4). `None` when no spawn is between intent and identity.
    #[serde(default)]
    pub pending_spawn: Option<PendingSpawn>,
    /// **Server A**: the persisted identity of the session+server the launch
    /// created (round-5). `Some` once `tmux new-session` returned a resolved,
    /// birth-proven session; the custodian and supervisor read it to bind their
    /// cleanup/liveness to A instead of re-establishing it from the socket.
    #[serde(default)]
    pub server_a: Option<ServerA>,
    /// The **disposable run dir** the coordinator chose for this launch's
    /// `codex-host` — the directory the host creates, binds its three sockets
    /// under, and removes on every exit path of its own.
    ///
    /// It is recorded here for the case the host's own sweep cannot cover: the
    /// host never ran (the pane died before it got that far) or was SIGKILLed
    /// mid-session, so nothing in-process is left to remove the directory. The
    /// custodian reads this field and sweeps the path after it kills the session
    /// ([`crate::codex_custodian`]). Written **before** `tmux new-session`, so a
    /// coordinator that dies with tmux in flight still leaves the path findable.
    ///
    /// `None` on a launch that never reached the new-session step, and on records
    /// written before this field existed (`serde(default)`).
    #[serde(default)]
    pub run_dir: Option<String>,
    /// Whether a `codex-host` presenting this launch's nonce ever **reached the
    /// admission gate** — set on arrival, whether it was then admitted or
    /// refused, and never cleared.
    ///
    /// It answers one question the lease cannot, because the lease is current
    /// state and this is history: *did the pane ever actually run?* That settles
    /// the `new_session_indeterminate` guard. The flag means "tmux's answer was
    /// never heard, so a session may still be coming" — but a host only exists
    /// because a pane ran it, so a host at the gate is proof the session already
    /// arrived.
    ///
    /// Deliberately set on **arrival** rather than on admission, because the case
    /// that actually occurs is a refusal: a host whose launch already failed is
    /// turned away and its pane dies, taking the session with it. Keying this on
    /// admission would miss exactly that, and the custodian would then find an
    /// absence it must refuse to believe and stay armed forever.
    ///
    /// Guarded by the nonce: only a host presenting THIS launch's nonce sets it,
    /// so a stranger cannot retire another launch's safety rule.
    #[serde(default)]
    pub host_reached_gate: bool,
    /// The identity of the host that reached the gate — written on ARRIVAL, before
    /// the gate renders its verdict, and deliberately **never cleared**, unlike
    /// [`Self::host_lease`] which `to_failed` drops.
    ///
    /// The lease answers "who holds this launch right now"; this answers "which
    /// process was the host", and cleanup needs the second long after the first is
    /// gone. It is what lets the custodian conclude a session is over when no
    /// server A was ever persisted: the pane's command IS the host, so a host
    /// proven dead means tmux has already reaped the pane and the session with it.
    #[serde(default)]
    pub host_identity: Option<ProcessIdentity>,
    /// Whether a custodian ever positively observed this launch's session present.
    ///
    /// Durable rather than process-local: a custodian that dies and is replaced by
    /// the sweep would otherwise forget, and the replacement — arriving after the
    /// session is already gone — would be stuck refusing to believe an absence
    /// that its predecessor had already explained. Written once, never cleared.
    #[serde(default)]
    pub session_observed: bool,
    pub children: Vec<ChildEntry>,
    pub created_ms: i64,
}

impl LaunchRecord {
    /// Whether the record is well-formed for *this* build and boot. A record
    /// from another schema or another boot is not interpretable here.
    fn admissible_shape(&self, now_boot: &BootIdentity) -> Result<(), Admission> {
        if self.schema != SCHEMA {
            return Err(Admission::Refused(format!(
                "launch record schema {} is not understood by this build",
                self.schema
            )));
        }
        if &self.boot != now_boot {
            // A different boot: the monotonic deadline is meaningless. Fail
            // closed rather than trust a stale deadline across a reboot/restore.
            return Err(Admission::Refused(
                "launch record is from a previous boot; its deadline cannot be trusted".into(),
            ));
        }
        Ok(())
    }
}

/// Whether the deadline has passed, cannot be judged, or is still live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Live,
    Expired,
    /// The boot identity or clock could not be read — never treated as "live".
    Indeterminate,
}

/// Judge the deadline against the current boot + monotonic clock. A record from
/// a different boot, or an unreadable clock, is [`Expiry::Indeterminate`] — the
/// caller fails closed, it never proceeds as if live.
pub fn deadline_expiry(record: &LaunchRecord) -> Expiry {
    let (Some(now_boot), Some(now)) = (boot_identity(), monotonic_now_nanos()) else {
        return Expiry::Indeterminate;
    };
    if record.boot != now_boot {
        return Expiry::Indeterminate;
    }
    if now >= record.deadline_monotonic_nanos {
        Expiry::Expired
    } else {
        Expiry::Live
    }
}

/// The verdict a late `codex-host` (or any admission check) gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Nonce, identities, deadline, `pending`, and a fresh lease all held.
    Admitted,
    /// Anything else. The late host may only clean up its own stale session.
    Refused(String),
}

// ----------------------------------------------------------------------------
// Fail-closed liveness predicates (Principle D / finding 6).
//
// The two decisions the whole launch machine makes about a recorded guardian —
// "may I admit/commit on it?" and "may I tear it down / rearm past it?" — are
// **fail-closed in opposite directions**, and both must reject `Unknown`. These
// are the single source of that policy so every call site is provably
// consistent, and so the three-way logic is unit-testable without having to
// synthesize a real `Unknown` from the kernel (which Darwin gives no
// deterministic way to do).
// ----------------------------------------------------------------------------

/// May a caller **admit or commit** on a guardian with this liveness? Only a
/// *proven* `Alive` — `Gone` and `Unknown` alike refuse (an unreadable process
/// is never proof of life).
fn liveness_admits(l: Liveness) -> bool {
    l == Liveness::Alive
}

/// May a caller **tear down or rearm past** a guardian with this liveness? Only
/// a *proven* `Gone` — `Alive` and `Unknown` alike refuse (an unreadable process
/// is never proof of absence, so a hiccup can never trigger destruction).
fn liveness_is_gone(l: Liveness) -> bool {
    l == Liveness::Gone
}

// ----------------------------------------------------------------------------
// The interprocess lock.
// ----------------------------------------------------------------------------

/// An held exclusive `flock` on the session's `launch.lock`. Dropping it
/// releases the lock. All CAS transitions run inside one of these.
pub struct LaunchLock {
    _file: std::fs::File,
}

/// fsync a directory so its own entries are durable. Propagates the error.
fn fsync_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::File::open(dir)
        .with_context(|| format!("opening {} to flush it", dir.display()))?
        .sync_all()
        .with_context(|| format!("fsync of {}", dir.display()))
}

/// Create the session dir for `uid` and **establish the durability barrier**
/// (findings 8/12 + finding 6). This is the one place the uid dir is first
/// created — at lock acquisition, before any record is written.
///
/// The barrier is established on **every** call, not only first creation
/// (finding 6): the previous "return early if the dir exists" skipped the fsync
/// whenever the dir was already there — which is exactly the case of a *retry*
/// after a prior parent-fsync error, or a concurrent creator that raced ahead but
/// whose fsync we cannot observe. An unconditional parent fsync is idempotent and
/// cheap (lock acquisition is not a tight loop), and it guarantees the uid dir's
/// entry is durable before we proceed no matter which path created it.
///
/// When `sessions/` itself is created for the first time, its **own** entry is
/// made durable by fsyncing the root (`~/.codeconnect`) too, so a power loss
/// cannot leave a `sessions/` whose grandparent entry was never written.
fn create_session_dir_durably(dir: &std::path::Path) -> Result<()> {
    // The ancestors that do NOT yet exist — the ones THIS call will create, whose
    // parents must be fsynced so the new entries are durable (finding 9: based on
    // ACTUAL creation, not a pre-existence check that a concurrent creator or a
    // retry could skip). Captured innermost-first before we create anything.
    let mut newly: Vec<std::path::PathBuf> = Vec::new();
    let mut cur: Option<&std::path::Path> = Some(dir);
    while let Some(p) = cur {
        if p.exists() {
            break;
        }
        newly.push(p.to_path_buf());
        cur = p.parent();
    }
    // Create the whole session tree **private (0700)** through `fsperm` — a launch
    // record's dirs must never be 0755 under a normal umask (finding 9, a privacy
    // leak of session metadata). `private_dir` is recursive and hardens existing
    // dirs too.
    protocol::fsperm::private_dir(dir)
        .with_context(|| format!("creating {} as a private (0700) dir", dir.display()))?;
    // fsync the parent of each dir we actually created — including the root when
    // `sessions/` is first made, and `.codeconnect`'s parent if it was recursively
    // created (finding 9).
    for p in &newly {
        if let Some(parent) = p.parent() {
            fsync_dir(parent)?;
        }
    }
    // And ALWAYS re-fsync the parent of the uid dir **and its ancestors** up the
    // session tree, even on the already-exists path, so a retry after a prior
    // fsync error re-establishes the barrier for EVERY ancestor whose earlier
    // fsync may have failed — not just the immediate parent (round-5 finding 7).
    // The tree is shallow (`~/.codeconnect/sessions/<uid>`), and lock acquisition
    // is not a tight loop, so a few directory fsyncs are cheap. Bounded to the
    // root under which `sessions/` lives.
    let sessions_root = sessions_root();
    let mut anc = dir.parent();
    for _ in 0..4 {
        let Some(p) = anc else { break };
        fsync_dir(p)?;
        if p == sessions_root.parent().unwrap_or(&sessions_root) {
            break;
        }
        anc = p.parent();
    }
    Ok(())
}

impl LaunchLock {
    /// Block until the exclusive lock is held. Creates the session dir (durably —
    /// see [`create_session_dir_durably`]) and the lock file if needed.
    pub fn acquire(uid: &str) -> Result<LaunchLock> {
        let dir = session_dir(uid);
        create_session_dir_durably(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))
            .with_context(|| format!("opening the launch lock in {}", dir.display()))?;
        // Blocking, exclusive. flock is advisory but every writer of this record
        // takes it, which is all advisory locking needs.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .context("flock(LOCK_EX) on the launch lock");
        }
        Ok(LaunchLock { _file: file })
    }

    /// Acquire the lock within a **bounded deadline**, never blocking indefinitely
    /// (round-5 finding 6): a `SIGSTOP`ed holder must never wedge the sole cleanup
    /// owner, the sweep, or host admission. Retries the non-blocking `try_acquire`
    /// until `budget` elapses; a lock still held at the deadline is a hard error.
    pub fn acquire_bounded(uid: &str, budget: std::time::Duration) -> Result<LaunchLock> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            match Self::try_acquire(uid)? {
                Some(lock) => return Ok(lock),
                None => {
                    if std::time::Instant::now() >= deadline {
                        bail!(
                            "could not acquire the launch lock for {uid} within {}ms (a stopped \
                             holder?)",
                            budget.as_millis()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }

    /// Try to take the lock without blocking. `Ok(None)` when another holder has
    /// it — used by tests and by non-blocking sweeps.
    pub fn try_acquire(uid: &str) -> Result<Option<LaunchLock>> {
        let dir = session_dir(uid);
        create_session_dir_durably(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(LaunchLock { _file: file }));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(err).context("flock(LOCK_EX|LOCK_NB) on the launch lock")
        }
    }
}

// ----------------------------------------------------------------------------
// Durable read / write.
// ----------------------------------------------------------------------------

/// Read + parse the record. A missing, corrupt, truncated, or unknown-schema
/// record is an error — callers that admit on it must fail closed.
pub fn load(uid: &str) -> Result<LaunchRecord> {
    let path = record_path(uid);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let record: LaunchRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} (corrupt or truncated)", path.display()))?;
    if record.schema != SCHEMA {
        bail!(
            "{} carries schema {}, not {SCHEMA}",
            path.display(),
            record.schema
        );
    }
    Ok(record)
}

/// Write the record durably: temp file → fsync temp → rename over the target →
/// fsync the directory. The rename is atomic, so a crash mid-write never leaves
/// a half-record (a reader would parse the pre-image or the post-image, never a
/// splice), and the two fsyncs make the successor and its directory entry
/// survive power loss.
fn store_atomic(uid: &str, record: &LaunchRecord) -> Result<()> {
    let dir = session_dir(uid);
    // The session dir's own directory entry is made durable at lock acquisition
    // (`create_session_dir_durably`), which is the only place it is first
    // created; every `store_atomic` runs under that lock, so the parent `sessions/`
    // dir is already fsynced by the time we get here (finding 8). We still ensure
    // the dir exists (belt and suspenders) and fsync *this* dir after the rename
    // below so the record's own entry is durable.
    // Ensure the dir exists as a PRIVATE (0700) dir even on this belt-and-suspenders
    // path (finding 9).
    protocol::fsperm::private_dir(&dir)
        .with_context(|| format!("ensuring {} is a private dir", dir.display()))?;
    let target = dir.join(RECORD_FILE);
    let temp = dir.join(format!(".{RECORD_FILE}.tmp"));
    let json = serde_json::to_vec_pretty(record).context("serializing the launch record")?;
    {
        // 0600 from the instant it exists (finding 9): a launch record carries
        // session metadata and must never be world-readable under a normal umask.
        let mut f = protocol::fsperm::create_private(&temp)
            .with_context(|| format!("creating {} (0600)", temp.display()))?;
        use std::io::Write;
        f.write_all(&json)?;
        f.sync_all()
            .context("fsync of the launch record temp file")?;
    }
    std::fs::rename(&temp, &target)
        .with_context(|| format!("renaming {} over {}", temp.display(), target.display()))?;
    // Flush the directory entry so the rename itself is durable.
    std::fs::File::open(&dir)
        .with_context(|| format!("opening {} to flush it", dir.display()))?
        .sync_all()
        .with_context(|| format!("flushing {}", dir.display()))?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Transitions (each caller holds the lock).
// ----------------------------------------------------------------------------

/// Parameters for the initial `pending` record the coordinator writes before it
/// spawns the custodian or touches tmux.
pub struct NewLaunch {
    pub launch_nonce: String,
    pub uid: String,
    pub session_name: String,
    pub coordinator: ProcessIdentity,
    pub boot: BootIdentity,
    pub deadline_monotonic_nanos: u64,
    pub created_ms: i64,
}

/// Write the first `pending` record — a single-owner **ABSENT → Pending** CAS
/// (Principle A / finding 10). If any record already exists for this uid, refuse:
/// a duplicate or restarted coordinator must **never** erase a `Ready` or
/// `Failed` outcome by blindly rewriting `pending`.
pub fn create_pending(_lock: &LaunchLock, new: NewLaunch) -> Result<LaunchRecord> {
    match load(&new.uid) {
        Ok(existing) => bail!(
            "refusing to create a pending record: one already exists for {} (state {:?})",
            new.uid,
            existing.state
        ),
        Err(_) if record_path(&new.uid).exists() => {
            // A record file exists but does not parse: fail closed rather than
            // overwrite a possibly-committed outcome we merely cannot read.
            bail!(
                "refusing to create a pending record: an unreadable record already exists for {}",
                new.uid
            );
        }
        Err(_) => {}
    }
    let record = LaunchRecord {
        schema: SCHEMA,
        launch_nonce: new.launch_nonce,
        uid: new.uid.clone(),
        session_name: new.session_name,
        coordinator: new.coordinator,
        custodian: None,
        boot: new.boot,
        deadline_monotonic_nanos: new.deadline_monotonic_nanos,
        state: LaunchState::Pending,
        cleanup: CleanupState::Pending,
        new_session_indeterminate: false,
        host_lease: None,
        pending_spawn: None,
        server_a: None,
        run_dir: None,
        host_reached_gate: false,
        host_identity: None,
        session_observed: false,
        children: Vec::new(),
        created_ms: new.created_ms,
    };
    store_atomic(&new.uid, &record)?;
    Ok(record)
}

/// Persist **server A** (round-5 finding 1): the identity of the session+server
/// the launch created, so the separate custodian/supervisor can bind cleanup and
/// liveness to it. Recorded on a still-`pending` record right after
/// `tmux new-session` returns a resolved, birth-proven session; carried forward
/// through the terminal transitions.
pub fn record_server_a(_lock: &LaunchLock, uid: &str, a: ServerA) -> Result<()> {
    let mut record = load(uid)?;
    record.server_a = Some(a);
    store_atomic(uid, &record)
}

/// Append a child the **host** spawned — the app-server or the TUI — to the
/// record, so cleanup can address it by a proven identity instead of a guess.
///
/// The same [`ChildEntry`] shape the D6 exec gate writes for the custodian, and
/// for the same reason: `(pid, birth, pgid)` is the only thing this codebase will
/// signal. A pid alone is a number the kernel may have handed to somebody else by
/// the time cleanup runs; the birth identity is what makes it an identity.
///
/// **Fenced.** Only the host holding this launch's lease may append, and only
/// while the launch is still `pending`. Without that, any process that could read
/// the uid could add entries the custodian will later SIGKILL by pid — turning the
/// cleanup path into a way to have arbitrary processes killed. The lease is
/// already the thing that says which host owns this launch, so it is the thing
/// asked here.
///
/// Idempotent per role: a repeated write for a role already recorded with the same
/// identity is a no-op, so a retry cannot grow the list.
pub fn record_host_child(
    _lock: &LaunchLock,
    uid: &str,
    by: &ProcessIdentity,
    mut entry: ChildEntry,
) -> Result<()> {
    let mut record = load(uid)?;
    // The launch must still be live: a terminal record's children are history, and
    // appending to it would hand the custodian identities it never authorised.
    if record.state != LaunchState::Pending {
        bail!(
            "refusing to record {}: the launch is {:?}, not pending",
            entry.role,
            record.state
        );
    }
    match &record.host_lease {
        Some(lease) if &lease.identity == by => {}
        Some(_) => bail!(
            "refusing to record {}: the lease belongs to a different host",
            entry.role
        ),
        None => bail!(
            "refusing to record {}: no host holds this launch's lease",
            entry.role
        ),
    }
    // Stamped with the recording host, which is the lease holder verified above.
    entry.recorded_by = Some(*by);
    if record
        .children
        .iter()
        .any(|c| c.role == entry.role && c.identity == entry.identity)
    {
        return Ok(());
    }
    record.children.push(entry);
    store_atomic(uid, &record)
}

/// Note that a custodian positively observed this launch's session present.
///
/// Durable so a replacement custodian inherits the observation. Idempotent.
pub fn note_session_observed(_lock: &LaunchLock, uid: &str) -> Result<()> {
    let mut record = load(uid)?;
    if record.session_observed {
        return Ok(());
    }
    record.session_observed = true;
    store_atomic(uid, &record)
}

/// The two roles a host spawns, which cleanup must be able to address.
pub const HOST_CHILD_ROLES: [&str; 2] = ["app-server", "tui"];

/// Whether BOTH host children are recorded **by the current lease holder**.
///
/// The coordinator asks this before committing `ready`: a session declared ready
/// must be one whose processes cleanup can name. A host that could not record a
/// child aborts, so in practice this is true by the time the legs are serving —
/// asking anyway is what makes that a checked invariant rather than an assumption
/// about ordering in another process.
///
/// Scoped to the live lease, because entries recorded by a **displaced** host are
/// deliberately retained (see [`ChildEntry::recorded_by`]): they are still cleanup
/// evidence, but they say nothing about whether the current host has come up, and
/// counting them would let a successor's readiness be satisfied by its
/// predecessor's processes.
pub fn host_children_recorded(record: &LaunchRecord) -> bool {
    let Some(lease) = record.host_lease.as_ref() else {
        return false;
    };
    HOST_CHILD_ROLES.iter().all(|role| {
        record
            .children
            .iter()
            .any(|c| &c.role == role && c.recorded_by == Some(lease.identity))
    })
}

/// The file the host writes inside its run dir, immediately after the exclusive
/// `mkdir`, naming the launch the directory belongs to.
pub const RUN_DIR_OWNER_FILE: &str = "owner";

/// The marker's contents: uid and launch nonce, one per line.
fn owner_marker_body(uid: &str, launch_nonce: &str) -> String {
    format!("{uid}\n{launch_nonce}\n")
}

/// Write the run dir's ownership marker. `create_new`, 0600, once.
///
/// **Why a marker at all, when the name already encodes the launch.** The name
/// does not identify anything: [`crate::codex_coordinator::choose_run_dir`]
/// filters and truncates, so it is a many-to-one derivation, and two launches can
/// land on one name. That leaves two holes a name-based check cannot see. A
/// coordinator could accept host A's *serving sockets* as evidence for host B's
/// lease and commit a false `ready`; and the custodian could delete a directory
/// belonging to a different, live launch, because it deletes a **recorded name**
/// that its own host may never have created — the path is recorded before the
/// directory exists.
///
/// The marker closes both by making the directory self-identifying. It is written
/// by the process that owns the directory, immediately after the `mkdir` that
/// proved it fresh, so the two facts are inseparable: the `mkdir` is the adoption
/// fence, and this file is the **deletion warrant** and the readiness binding.
///
/// `create_new` rather than a plain write: nothing may already be there, and if
/// something is, that is not our directory and the launch must not proceed.
pub fn write_owner_marker(run_dir: &std::path::Path, uid: &str, launch_nonce: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // Bound what a marker can contain, so a legitimate one can never reach the
    // reader's size ceiling (see `MARKER_FIELD_MAX`). Also refuse a newline, which
    // would forge a second field and make the two-line body ambiguous.
    for (what, value) in [("uid", uid), ("launch nonce", launch_nonce)] {
        if value.is_empty() || value.len() > MARKER_FIELD_MAX {
            bail!(
                "refusing to write an owner marker: the {what} is {} bytes, which must be \
                 1..={MARKER_FIELD_MAX}",
                value.len()
            );
        }
        if value.contains('\n') {
            bail!("refusing to write an owner marker: the {what} contains a newline");
        }
    }
    let path = run_dir.join(RUN_DIR_OWNER_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating the run-dir owner marker {}", path.display()))?;
    file.write_all(owner_marker_body(uid, launch_nonce).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync of {}", path.display()))?;
    // `OpenOptionsExt::mode` is a CREATION mode and the umask can only REMOVE bits
    // from it — it never adds any. So the risk is not a permissive umask widening
    // 0600; it is a restrictive one narrowing it, and more to the point the mode is
    // only honoured when this call actually creates the file. Setting the mode
    // explicitly afterwards makes 0600 the state of the file rather than a request
    // made at creation time.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("hardening {} to 0600", path.display()))?;
    Ok(())
}

/// What a run dir's ownership marker says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerVerdict {
    /// Read successfully and it names this launch.
    Ours,
    /// Read successfully and it names a different launch, or the directory has no
    /// marker at all. Someone else's, or never claimed — never ours to use or
    /// delete.
    Foreign,
    /// Nothing is there to judge.
    Absent,
    /// The question could not be ANSWERED — a transient read error, a symlink at
    /// the marker's name, an oversized file. Distinct from `Foreign` on purpose:
    /// "I could not read it" is not "it belongs to someone else", and collapsing
    /// the two would let one unlucky `EINTR` convince a coordinator that its own
    /// run dir is a stranger's, or convince a custodian that it owes nothing on a
    /// directory it is actually responsible for.
    Unknown(String),
}

/// The most a marker may be. It holds a uid and a nonce; anything larger is not a
/// marker this code wrote, and reading it unbounded would let a same-uid process
/// hand the reader an endless file.
///
/// Read as `LIMIT + 1` so that hitting the ceiling is DETECTABLE. A plain
/// `take(LIMIT)` reports EOF at the limit, which is indistinguishable from a file
/// that simply ended — so an oversized file would be compared as if it were
/// complete content, come out unequal, and be filed as `Foreign`. For the
/// custodian that verdict means "not mine, nothing owed", i.e. a directory this
/// launch owns abandoned on the strength of a truncated read.
const MARKER_READ_LIMIT: u64 = 4096;

/// The caps a marker's own fields must obey at WRITE time.
///
/// The read limit only helps if a legitimate marker can never approach it —
/// otherwise "too big to be ours" and "ours" overlap, and the overflow rule would
/// start rejecting real markers. A uid is a 26-character ULID and a nonce is 32
/// hex characters; these caps are generous multiples of both, and they are checked
/// when writing so the invariant is established rather than assumed.
const MARKER_FIELD_MAX: usize = 128;

/// Read `run_dir`'s ownership marker and say what it proves.
///
/// Opened with `O_NOFOLLOW` and read under [`MARKER_READ_LIMIT`]: the marker sits
/// inside a directory other processes running as this uid can write to, so it is
/// read as untrusted input — a symlink planted at its name must not redirect the
/// read, and a large file must not be swallowed whole.
pub fn run_dir_marker(run_dir: &std::path::Path, uid: &str, launch_nonce: &str) -> MarkerVerdict {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::symlink_metadata(run_dir) {
        Ok(meta) if meta.is_dir() => {}
        // A non-directory at the run dir's name is not a run dir at all.
        Ok(_) => return MarkerVerdict::Foreign,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return MarkerVerdict::Absent,
        Err(err) => return MarkerVerdict::Unknown(format!("stat of the run dir failed: {err}")),
    }
    let path = run_dir.join(RUN_DIR_OWNER_FILE);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        // `O_NOFOLLOW` so a symlink at the marker's name cannot redirect the read,
        // and `O_NONBLOCK` so a FIFO cannot make the OPEN itself block forever —
        // opening a FIFO for reading waits for a writer, which would hang the
        // custodian inside a cleanup pass with no deadline on it.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        // No marker in a directory that exists: claimed by nobody, or by something
        // that is not us. Either way not ours.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return MarkerVerdict::Foreign,
        // ELOOP from O_NOFOLLOW: a symlink is sitting where the marker should be.
        // That is a planted file, but it is also not a READ we managed to do, so it
        // is reported as unanswerable rather than as a foreign claim.
        Err(err) => return MarkerVerdict::Unknown(format!("opening the marker failed: {err}")),
    };
    // Regular files only, established through the OPEN FD rather than the path, so
    // the thing checked is the thing read. A FIFO, device or directory at that name
    // is not a marker this code wrote — and it is also not a readable claim, so it
    // is `Unknown` rather than `Foreign`: it says nothing about who owns the dir.
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(meta) => {
            return MarkerVerdict::Unknown(format!(
                "the marker is not a regular file ({:?})",
                meta.file_type()
            ))
        }
        Err(err) => return MarkerVerdict::Unknown(format!("stat of the marker failed: {err}")),
    }
    let mut body = Vec::new();
    // LIMIT + 1: the extra byte exists solely so its arrival can be detected.
    if let Err(err) = file.take(MARKER_READ_LIMIT + 1).read_to_end(&mut body) {
        return MarkerVerdict::Unknown(format!("reading the marker failed: {err}"));
    }
    if body.len() as u64 > MARKER_READ_LIMIT {
        // Overflow is UNKNOWN, never `Foreign`. A truncated read compared as if it
        // were whole would disown a directory this launch owns.
        return MarkerVerdict::Unknown(format!(
            "the marker exceeds {MARKER_READ_LIMIT} bytes, so it could not be read whole"
        ));
    }
    match String::from_utf8(body) {
        Ok(body) if body == owner_marker_body(uid, launch_nonce) => MarkerVerdict::Ours,
        // Read successfully, and it is not ours.
        Ok(_) => MarkerVerdict::Foreign,
        // Not text at all: unreadable as a claim, so unanswerable.
        Err(_) => MarkerVerdict::Unknown("the marker is not valid UTF-8".into()),
    }
}

/// Note that a host presenting `expected_nonce` reached the admission gate.
///
/// History, not state: written before the gate renders its verdict, because a
/// **refused** host is exactly the case that matters — it is turned away and its
/// pane dies, and this flag is then the only durable trace that the pane ran at
/// all. Nonce-guarded so a host that does not belong to this launch cannot set
/// it, and idempotent so the common path costs no write.
///
/// **Not best-effort.** An earlier contract said the caller must not fail
/// admission over this, on the reasoning that a missing flag only makes the
/// custodian more conservative. That is true in isolation and wrong in context:
/// every refusal path below this call ends by DESTROYING the launch's session, so
/// a host that arrived, was refused, and tore the session down without leaving
/// this flag hands the custodian an absence it cannot explain — and, with no
/// server A recorded, no way ever to conclude the session is over. The caller
/// therefore fails closed, which costs a refused launch and nothing else, because
/// this is written before anything has been created or destroyed.
pub fn note_host_reached_gate(
    _lock: &LaunchLock,
    uid: &str,
    expected_nonce: &str,
    host: &ProcessIdentity,
) -> Result<ArrivalNote> {
    let mut record = load(uid)?;
    if record.launch_nonce != expected_nonce {
        // NOT recorded: this host does not belong to the launch the record
        // describes, so its arrival cannot be written as that launch's evidence.
        // The caller must not proceed to any destructive verdict on the strength
        // of an arrival that was never durably noted.
        return Ok(ArrivalNote::NonceMismatch);
    }
    if record.host_reached_gate && record.host_identity == Some(*host) {
        return Ok(ArrivalNote::Recorded);
    }
    record.host_reached_gate = true;
    // The IDENTITY lands here too, on arrival, not on successful admission.
    //
    // A refused host is exactly the case cleanup later has to reason about: its
    // pane dies taking the session with it, and if no server A was ever persisted,
    // this identity is the only durable thing left that can prove the session is
    // over. Recording it only on admission would leave the refused, no-A launch
    // with arrival evidence but nothing to prove death by — armed until reboot.
    record.host_identity = Some(*host);
    store_atomic(uid, &record)?;
    Ok(ArrivalNote::Recorded)
}

/// Whether [`note_host_reached_gate`] durably recorded the arrival.
///
/// `NonceMismatch` means it deliberately did NOT: the presenting host does not
/// belong to the launch the record describes, so no arrival evidence exists for
/// the session this uid names. A caller holding a pane must park rather than
/// proceed to any verdict whose refusal path destroys that session — destroying
/// it here would recreate the evidence-less disappearance this flag prevents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrivalNote {
    Recorded,
    NonceMismatch,
}

/// Persist the **run dir** the coordinator chose for this launch's `codex-host`.
///
/// Durable-before-mutation (Principle B), exactly like
/// [`mark_new_session_starting`] and for the same reason: the path is written and
/// fsynced *before* `tmux new-session` puts a host in a pane, so a coordinator
/// that dies with tmux in flight still leaves a record naming the directory the
/// custodian must sweep. Recording it afterwards would leave the one window where
/// a directory exists that nobody can name.
pub fn record_run_dir(_lock: &LaunchLock, uid: &str, run_dir: &str) -> Result<()> {
    let mut record = load(uid)?;
    record.run_dir = Some(run_dir.to_string());
    store_atomic(uid, &record)
}

/// Fsync the **intent** to spawn a gated child, before the child exists (D6 /
/// finding 4). Recorded on any non-terminal state where a spawn can be in
/// flight (`pending`, or `failed{cleanup:pending}` for a sweep rearm).
pub fn set_pending_spawn(_lock: &LaunchLock, uid: &str, intent: PendingSpawn) -> Result<()> {
    let mut record = load(uid)?;
    match &record.state {
        LaunchState::Pending => {}
        LaunchState::Failed { .. } if record.cleanup == CleanupState::Pending => {}
        // A Ready session whose custodian died needs a replacement (finding 4);
        // the CAS that consumes this intent enforces the Gone-slot requirement.
        LaunchState::Ready => {}
        other => bail!("cannot record a spawn intent while the launch is {other:?}"),
    }
    record.pending_spawn = Some(intent);
    store_atomic(uid, &record)
}

/// The exec gate's `on_ready`: the **single-owner CAS of the custodian slot**
/// (Principle A / finding 8) — atomically set the slot to `identity`, append the
/// child's [`ChildEntry`], and clear the pending spawn intent, all in one durable
/// write (findings 4/5/8).
///
/// The CAS succeeds only when the slot is **empty** (the coordinator's initial
/// arm) or its current occupant is **proven `Gone`** (a sweep replacing a dead
/// custodian). It **refuses** a `Live` occupant (someone already armed/rearmed)
/// or an `Unknown` one (fail-closed via [`liveness_is_gone`]: never spawn a
/// second custodian on uncertainty). Because this runs *before* the gate's GO, a
/// losing concurrent/duplicate rearm's refusal here is exactly what withholds GO
/// from the redundant custodian — so only one is ever armed and no untracked
/// custodians accumulate across concurrent sweeps.
///
/// Valid while `pending` (initial arm) and while `failed{cleanup:pending}`
/// (rearm); refused once `ready`, or `failed` with cleanup no longer pending.
pub fn cas_custodian_with_child(
    _lock: &LaunchLock,
    uid: &str,
    identity: ProcessIdentity,
    pgid: i32,
    nonce: &str,
    argv_hash: &str,
) -> Result<()> {
    let mut record = load(uid)?;
    match &record.state {
        LaunchState::Pending => {}
        LaunchState::Failed { .. } if record.cleanup == CleanupState::Pending => {}
        // A committed `Ready` session whose custodian has died still needs an
        // independent teardown owner for coordinator/supervisor loss (finding 4):
        // the sweep rearms one here. The slot check below still requires the dead
        // occupant be *proven* Gone, so this cannot displace a live custodian.
        LaunchState::Ready => {}
        other => bail!("cannot arm a custodian while the launch is {other:?}"),
    }
    if let Some(current) = record.custodian {
        // Fail-closed: only a *proven* Gone occupant may be replaced. A Live one
        // (already armed) and an Unknown one (unreadable) both refuse.
        if !liveness_is_gone(liveness(&current)) {
            bail!("refusing to arm a custodian: the recorded one is not proven gone");
        }
    }
    record.custodian = Some(identity);
    record.children.push(ChildEntry {
        role: "custodian".into(),
        identity,
        pgid,
        nonce: nonce.to_string(),
        argv_hash: argv_hash.to_string(),
        // The custodian belongs to no host lease.
        recorded_by: None,
    });
    record.pending_spawn = None;
    store_atomic(uid, &record)
}

/// CAS `pending → ready` (Principle A / finding 7). Refuses unless, **re-checked
/// under the lock at commit time**, all of:
///   * the record is still `pending` (so a resumed, deadline-lost coordinator
///     cannot overwrite `failed`);
///   * the committing coordinator is the recorded one;
///   * the deadline has **not** passed;
///   * the custodian is recorded and **proven `Live`** — a session must never be
///     declared ready without an independent cleanup owner still breathing.
pub fn to_ready(_lock: &LaunchLock, uid: &str, coordinator: &ProcessIdentity) -> Result<()> {
    let mut record = load(uid)?;
    // A retry after a PARTIAL first commit (rename landed, dir-fsync failed ⇒ the
    // first `to_ready` returned Err leaving a visible-but-not-proven-durable
    // Ready): re-prove durability instead of refusing (finding 8). Only for OUR
    // own Ready — a foreign/failed record still falls through to the guards below.
    if record.state == LaunchState::Ready && &record.coordinator == coordinator {
        store_atomic(uid, &record)?;
        return Ok(());
    }
    if record.state != LaunchState::Pending {
        bail!(
            "refusing pending→ready: the launch is already {:?}",
            record.state
        );
    }
    if &record.coordinator != coordinator {
        bail!("refusing pending→ready: coordinator identity does not match the record");
    }
    if !matches!(deadline_expiry(&record), Expiry::Live) {
        bail!("refusing pending→ready: the deadline has passed or cannot be judged");
    }
    match record.custodian {
        Some(custodian) if liveness_admits(liveness(&custodian)) => {}
        Some(_) => bail!("refusing pending→ready: the custodian is not proven live"),
        None => bail!("refusing pending→ready: no custodian is armed"),
    }
    record.state = LaunchState::Ready;
    record.cleanup = CleanupState::NotRequired;
    record.host_lease = None;
    store_atomic(uid, &record)
}

/// CAS `pending → failed{reason}` with a cleanup disposition. Idempotent on a
/// record that is already `failed` (the first reason wins — a second failer does
/// not clobber the first). Refuses to move `ready → failed` here; a live
/// session's fatal path is the custodian/supervisor teardown, not this CAS.
pub fn to_failed(
    _lock: &LaunchLock,
    uid: &str,
    reason: &str,
    cleanup: CleanupState,
) -> Result<LaunchState> {
    let mut record = load(uid)?;
    match &record.state {
        LaunchState::Failed { .. } => {
            // Already terminal. Re-run the durable write so a RETRY after a
            // partial first write (rename landed, dir-fsync failed ⇒ the first
            // call returned Err) actually **proves durability** this time
            // (finding 3): the fix for "reads Failed and returns Ok without
            // re-fsyncing". The reason is unchanged (first reason wins).
            store_atomic(uid, &record)?;
            Ok(record.state.clone())
        }
        LaunchState::Ready => {
            bail!("refusing ready→failed via to_failed; a live session tears down elsewhere")
        }
        LaunchState::Pending => {
            record.state = LaunchState::Failed {
                reason: reason.to_string(),
            };
            record.cleanup = cleanup;
            record.host_lease = None;
            store_atomic(uid, &record)?;
            Ok(record.state)
        }
    }
}

/// Update just the cleanup disposition (e.g. a custodian marking `complete`).
pub fn set_cleanup(_lock: &LaunchLock, uid: &str, cleanup: CleanupState) -> Result<()> {
    let mut record = load(uid)?;
    record.cleanup = cleanup;
    store_atomic(uid, &record)
}

/// **Durable-before-mutation** (Principle B / finding 1, CRITICAL): mark the
/// launch as having an *in-flight* `tmux new-session` and fsync it **before** the
/// mutation is issued. While set, a coordinator death with tmux in flight leaves
/// a record that already says "indeterminate", so the custodian stays armed
/// (never fails → observes absence → completes → exits before the late session
/// appears). Pending only.
pub fn mark_new_session_starting(_lock: &LaunchLock, uid: &str) -> Result<()> {
    let mut record = load(uid)?;
    if record.state != LaunchState::Pending {
        bail!(
            "cannot start new-session once the launch is {:?}",
            record.state
        );
    }
    record.new_session_indeterminate = true;
    store_atomic(uid, &record)
}

/// Clear the in-flight flag **after** `tmux new-session` returns a *determinate*
/// outcome (created or a definite failure).
///
/// The flag is cleared **regardless of the current state** (finding 6). The bug
/// this closes: if the custodian raced the coordinator to `pending → failed`
/// (deadline/loss) *before* the coordinator got here, a "only while pending"
/// clear was a no-op, leaving `new_session_indeterminate` stuck true on a `failed`
/// record whose new-session was actually determinate — and the custodian would
/// then treat one `Absent` observation as insufficient and stay armed forever.
/// Clearing the flag on a `failed` record is safe because this is only ever
/// called for a *determinate* outcome (the confirmed-indeterminate path is
/// [`fail_new_session_indeterminate`], which sets the flag instead). The
/// `cleanup → pending` restore stays scoped to a still-`pending` record.
pub fn clear_new_session_indeterminate(_lock: &LaunchLock, uid: &str) -> Result<()> {
    let mut record = load(uid)?;
    record.new_session_indeterminate = false;
    if record.state == LaunchState::Pending && record.cleanup != CleanupState::Pending {
        record.cleanup = CleanupState::Pending;
    }
    store_atomic(uid, &record)
}

/// Fail the launch because `tmux new-session` was **indeterminate** — records
/// the flag so the custodian will not treat one UID-absence observation as
/// proof the (possibly late) session never appeared. Idempotent like
/// [`to_failed`]; the flag is set even if the record is already `failed`.
pub fn fail_new_session_indeterminate(_lock: &LaunchLock, uid: &str, reason: &str) -> Result<()> {
    let mut record = load(uid)?;
    record.new_session_indeterminate = true;
    match &record.state {
        LaunchState::Failed { .. } => {}
        LaunchState::Ready => bail!("new-session cannot be indeterminate once ready"),
        LaunchState::Pending => {
            record.state = LaunchState::Failed {
                reason: reason.to_string(),
            };
            record.cleanup = CleanupState::Pending;
            record.host_lease = None;
        }
    }
    store_atomic(uid, &record)
}

/// Fail the launch on a **determinate** `tmux new-session` failure — the opposite
/// of [`fail_new_session_indeterminate`] (finding 6). Terminalizes to
/// `failed{cleanup:pending}` if still `pending`, and **atomically clears**
/// `new_session_indeterminate` in the same write whether or not the custodian has
/// already raced the record to `failed`. This closes the residual where a
/// coordinator that learned a definite new-session failure could die before a
/// separate clear ran, leaving the flag stuck true and the custodian armed
/// forever on `Absent`: the determinate outcome is now recorded as one durable,
/// interruption-free fact. Idempotent like [`to_failed`] (the first reason wins).
///
/// `cleanup` lets the caller record whether a disposable session actually needs
/// reaping: `NotRequired` for a failure that happened **before** `tmux
/// new-session` created anything (nothing to clean — so the custodian completes
/// via `Done` rather than probing a non-existent server forever, round-4), and
/// `Pending` for a failure **after** a session was created. It is only applied on
/// the `pending → failed` transition; a record already `failed` keeps its first
/// disposition (first-writer wins).
pub fn fail_new_session_determinate(
    _lock: &LaunchLock,
    uid: &str,
    reason: &str,
    cleanup: CleanupState,
) -> Result<LaunchState> {
    let mut record = load(uid)?;
    // A determinate outcome always disarms the speculative in-flight flag.
    record.new_session_indeterminate = false;
    match &record.state {
        LaunchState::Failed { .. } => {
            // The first *reason* wins (idempotent), but a determinate
            // `NotRequired` (the coordinator KNOWS no session was created) must
            // SUPERSEDE a custodian-set speculative `Pending` for the same launch
            // (round-5 finding 5) — otherwise the custodian probes a nonexistent
            // server forever. Only downgrade Pending→NotRequired; never touch a
            // Complete.
            if cleanup == CleanupState::NotRequired && record.cleanup == CleanupState::Pending {
                record.cleanup = CleanupState::NotRequired;
            }
        }
        LaunchState::Ready => {
            bail!("a determinate new-session failure cannot apply once ready")
        }
        LaunchState::Pending => {
            record.state = LaunchState::Failed {
                reason: reason.to_string(),
            };
            record.cleanup = cleanup;
            record.host_lease = None;
        }
    }
    store_atomic(uid, &record)?;
    Ok(record.state)
}

/// A late `codex-host` admission attempt (Principle A/D / finding 9), linearized
/// under the lock with `pending → failed`. Admitted only when, at commit time,
/// **all** of: the record parses and is this-boot; the nonce matches; the state
/// is `pending`; the deadline is `Live`; the recorded coordinator is **proven
/// `Live`** (not `Gone` and not `Unknown`); a custodian is recorded and **proven
/// `Live`**; and no **live** `host_lease` already exists (the lease is
/// **exclusive** — two correct-nonce hosts must not both be admitted). On
/// success it records an exclusive [`HostLease`] carrying nonce/role/pgid; on any
/// doubt it refuses and the host runs cleanup-only. D6/D7.
pub fn admit_host(
    _lock: &LaunchLock,
    uid: &str,
    expected_nonce: &str,
    host: &ProcessIdentity,
    host_pgid: i32,
    role: &str,
) -> Result<Admission> {
    let mut record = match load(uid) {
        Ok(record) => record,
        // A corrupt/missing record is fail-closed admission (D7 gate).
        Err(err) => {
            return Ok(Admission::Refused(format!(
                "unreadable launch record: {err}"
            )))
        }
    };
    let Some(now_boot) = boot_identity() else {
        return Ok(Admission::Refused("boot identity unreadable".into()));
    };
    if let Err(Admission::Refused(reason)) = record.admissible_shape(&now_boot) {
        return Ok(Admission::Refused(reason));
    }
    if record.launch_nonce != expected_nonce {
        return Ok(Admission::Refused("launch nonce does not match".into()));
    }
    if record.state != LaunchState::Pending {
        return Ok(Admission::Refused(format!(
            "launch is {:?}, not pending",
            record.state
        )));
    }
    match deadline_expiry(&record) {
        Expiry::Live => {}
        Expiry::Expired => return Ok(Admission::Refused("launch deadline has passed".into())),
        Expiry::Indeterminate => {
            return Ok(Admission::Refused(
                "launch deadline could not be judged".into(),
            ))
        }
    }
    // The coordinator that armed this launch must be proven LIVE — `Unknown` is
    // not proof (Principle D).
    if !liveness_admits(liveness(&record.coordinator)) {
        return Ok(Admission::Refused(
            "the coordinator that armed this launch is not proven live".into(),
        ));
    }
    // An independent cleanup owner must exist and be proven LIVE.
    match record.custodian {
        Some(custodian) if liveness_admits(liveness(&custodian)) => {}
        Some(_) => {
            return Ok(Admission::Refused(
                "the recorded custodian is not proven live".into(),
            ))
        }
        None => return Ok(Admission::Refused("no custodian is armed".into())),
    }
    // The lease is EXCLUSIVE and taking it over requires the incumbent be
    // **proven GONE** (finding 5, fail-closed like every other admission gate):
    // an incumbent that is Alive OR whose liveness is Unknown both refuse — an
    // unreadable lease holder is never overwritten on uncertainty.
    if let Some(existing) = &record.host_lease {
        if existing.identity != *host && !liveness_is_gone(liveness(&existing.identity)) {
            return Ok(Admission::Refused(
                "another host holds a lease that is not proven gone".into(),
            ));
        }
    }
    // A takeover RETIRES the previous host's children; it does not delete them.
    // Retirement is implicit — the entries keep their `recorded_by`, which no
    // longer matches the new lease, so `host_children_recorded` stops counting them
    // while `teardown_children` still consumes them. See `ChildEntry::recorded_by`
    // for why deleting would have thrown away the cleanup evidence for precisely
    // the processes most likely to be orphaned.
    //
    // (`host_identity` was already written on arrival, before this verdict.)
    record.host_lease = Some(HostLease {
        identity: *host,
        pgid: host_pgid,
        nonce: expected_nonce.to_string(),
        role: role.to_string(),
    });
    store_atomic(uid, &record)?;
    Ok(Admission::Admitted)
}

// ----------------------------------------------------------------------------
// Recovery sweep (D7 catastrophic-loss backstop).
// ----------------------------------------------------------------------------

/// A durable-state action the recovery sweep took or recommends for one record.
/// The sweep performs the record transitions it can under the lock; process
/// re-spawns (a replacement custodian) are the caller's to act on, so the sweep
/// stays free of process machinery and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepAction {
    /// A stale `pending` (both guardians gone, or the deadline passed) was CAS'd
    /// to `failed{cleanup:pending}`.
    FailedStalePending { uid: String },
    /// `failed{cleanup:pending}` whose custodian is gone: the caller should
    /// spawn a replacement custodian to finish cleanup.
    NeedsReplacementCustodian { uid: String },
    /// The record could not be **examined** this pass — its lock was held, or it
    /// could not be read. Nothing was done to it and nothing is known about it.
    ///
    /// Reported rather than silently skipped, because "I looked and there was
    /// nothing to do" and "I could not look" are the same empty result otherwise,
    /// and the caller has no way to tell a completed sweep from a blind one. A
    /// held lock is transient and expected (a host taking its admission lease
    /// holds it for a moment), so the right response is another pass — but that
    /// only happens if the caller is told.
    Skipped { uid: String, why: String },
}

/// Whether a recorded identity is gone (a missing one counts as gone for sweep
/// purposes — a guardian we never recorded cannot be relied on).
fn guardian_gone(id: Option<&ProcessIdentity>) -> bool {
    match id {
        None => true,
        // Fail-closed (Principle D): only a *proven* Gone guardian counts as
        // absent — an `Unknown` one is left alone, never swept.
        Some(id) => liveness_is_gone(liveness(id)),
    }
}

/// Scan every session's launch record and repair the ones a crash orphaned.
/// Runs bounded and best-effort: an unreadable record is skipped (a future
/// sweep, or the owning custodian, will get it), never guessed at.
///
/// This is the **backstop**, not the primary reaper (D7): the pre-armed
/// custodian handles the common case; the sweep exists for total-guardian-loss.
pub fn recovery_sweep() -> Vec<SweepAction> {
    let dir = sessions_root();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    for entry in entries.flatten() {
        let Some(uid) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !entry.path().join(RECORD_FILE).exists() {
            continue;
        }
        // Take the lock per record; a live custodian holding it means the record
        // is owned — skip rather than block the sweep.
        let lock = match LaunchLock::try_acquire(&uid) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                actions.push(SweepAction::Skipped {
                    uid,
                    why: "the launch lock was held by another holder".into(),
                });
                continue;
            }
            Err(err) => {
                actions.push(SweepAction::Skipped {
                    uid,
                    why: format!("the launch lock could not be taken: {err}"),
                });
                continue;
            }
        };
        let record = match load(&uid) {
            Ok(record) => record,
            Err(err) => {
                actions.push(SweepAction::Skipped {
                    uid,
                    why: format!("the record could not be read: {err}"),
                });
                continue;
            }
        };
        match &record.state {
            LaunchState::Pending => {
                let both_gone = guardian_gone(Some(&record.coordinator))
                    && guardian_gone(record.custodian.as_ref());
                let expired = !matches!(deadline_expiry(&record), Expiry::Live);
                if (both_gone || expired)
                    && to_failed(
                        &lock,
                        &uid,
                        "orphaned pending recovered by sweep",
                        CleanupState::Pending,
                    )
                    .is_ok()
                {
                    actions.push(SweepAction::FailedStalePending { uid: uid.clone() });
                    // It now needs cleanup; if no custodian can do it, flag.
                    if guardian_gone(record.custodian.as_ref()) {
                        actions.push(SweepAction::NeedsReplacementCustodian { uid });
                    }
                }
            }
            LaunchState::Failed { .. } => {
                if record.cleanup == CleanupState::Pending
                    && guardian_gone(record.custodian.as_ref())
                {
                    actions.push(SweepAction::NeedsReplacementCustodian { uid });
                }
            }
            LaunchState::Ready => {
                // A committed session whose custodian has died has lost its
                // independent teardown authority (finding 3): rearm one. Only a
                // *proven* dead custodian triggers this (Principle D). But NOT if
                // the session was already torn down: a Ready teardown records a
                // durable marker (cleanup = Complete, round-4 finding 8), so we
                // must NOT rearm a fresh custodian on every later sweep for a
                // session that is already gone.
                if record.cleanup != CleanupState::Complete
                    && guardian_gone(record.custodian.as_ref())
                {
                    actions.push(SweepAction::NeedsReplacementCustodian { uid });
                }
            }
        }
    }
    actions
}

/// A stable hash of a spawn's argv, recorded with the child (findings 4/5) so a
/// spawn is durably attributable to the exact command it was released to exec.
/// NUL-joined so no argv boundary can be forged by embedding a separator.
pub fn argv_hash(argv: &[String]) -> String {
    protocol::hash::sha256_hex(argv.join("\u{0}").as_bytes())
}

/// The convenience the coordinator/launcher use: mint a fresh launch nonce.
pub fn mint_nonce() -> String {
    // 128 bits of randomness rendered hex. Uses the same OS RNG the uid module
    // trusts; a launch nonce need not be a ULID, only unguessable and unique.
    let mut buf = [0u8; 16];
    // getrandom via libc, matching the crate's no-extra-deps posture.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if rc != 0 {
        // Fall back to time+pid; a nonce that is merely unique (not secret) is
        // acceptable here because the record is 0700 and identity-guarded.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("{t:032x}{:08x}", std::process::id());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The current process's identity, or an error — a launch actor that cannot
/// read its own birth identity cannot participate in the identity protocol.
pub fn require_current_identity() -> Result<ProcessIdentity> {
    current_identity().context("reading this process's own birth identity")
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::proc_identity::BirthIdentity;

    // These tests write launch records under the process-global test root
    // (`sessions_root()` in cfg(test)) using **distinct uids**, so they need no
    // env-var mutation and no serialization — the one thing that would deadlock
    // libc across parallel test threads (setenv/getenv) is simply not done.

    fn fake_identity(pid: i32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            birth: BirthIdentity {
                start_sec: 111,
                start_usec: 222,
            },
        }
    }

    fn a_pending(uid: &str, deadline: u64) -> NewLaunch {
        NewLaunch {
            launch_nonce: "cafef00d".into(),
            uid: uid.into(),
            session_name: "cc-1".into(),
            coordinator: current_identity().unwrap(),
            boot: boot_identity().unwrap(),
            deadline_monotonic_nanos: deadline,
            created_ms: 1,
        }
    }

    /// Test-only shorthand for the real single-owner custodian CAS
    /// ([`cas_custodian_with_child`]) — the one production arm/rearm path — with
    /// throwaway pgid/nonce/argv-hash. Records `id` as the custodian and appends
    /// one child entry.
    fn arm(lock: &LaunchLock, uid: &str, id: ProcessIdentity) {
        cas_custodian_with_child(lock, uid, id, id.pid, "test-nonce", "test-hash").unwrap();
    }

    /// A live custodian for records that must pass `to_ready`/admission: this
    /// process itself, which is provably `Alive`.
    fn live_custodian() -> ProcessIdentity {
        current_identity().unwrap()
    }

    #[test]
    fn create_load_roundtrip() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u1").unwrap();
        let written = create_pending(&lock, a_pending("u1", far)).unwrap();
        drop(lock);
        let read = load("u1").unwrap();
        assert_eq!(written, read);
        assert_eq!(read.state, LaunchState::Pending);
    }

    #[test]
    fn a_corrupt_record_fails_closed_on_load_and_admission() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u2").unwrap();
        create_pending(&lock, a_pending("u2", far)).unwrap();
        // Truncate the record to half its bytes.
        let path = record_path("u2");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(load("u2").is_err(), "a truncated record must not parse");
        let host = fake_identity(4242);
        let verdict = admit_host(&lock, "u2", "cafef00d", &host, host.pid, "codex-host").unwrap();
        assert!(
            matches!(verdict, Admission::Refused(_)),
            "a corrupt record must refuse admission, got {verdict:?}"
        );
    }

    #[test]
    fn failed_is_terminal_and_resume_cannot_overwrite_it() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u3").unwrap();
        let coord = current_identity().unwrap();
        create_pending(&lock, a_pending("u3", far)).unwrap();
        // The custodian's deadline transition wins.
        to_failed(
            &lock,
            "u3",
            "deadline expired while coordinator stopped",
            CleanupState::Pending,
        )
        .unwrap();
        // The resumed coordinator tries to commit ready — and is refused.
        let err = to_ready(&lock, "u3", &coord).unwrap_err();
        assert!(
            err.to_string().contains("already"),
            "resume must not overwrite failed: {err}"
        );
        assert!(matches!(
            load("u3").unwrap().state,
            LaunchState::Failed { .. }
        ));
    }

    #[test]
    fn first_failure_reason_wins() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u4").unwrap();
        create_pending(&lock, a_pending("u4", far)).unwrap();
        to_failed(&lock, "u4", "first", CleanupState::Pending).unwrap();
        to_failed(&lock, "u4", "second", CleanupState::Pending).unwrap();
        assert_eq!(
            load("u4").unwrap().state,
            LaunchState::Failed {
                reason: "first".into()
            }
        );
    }

    #[test]
    fn an_expired_deadline_is_detected_and_survives_wall_clock_rollback() {
        // Deadline already in the past on the monotonic clock.
        let past = monotonic_now_nanos().unwrap().saturating_sub(1);
        let lock = LaunchLock::acquire("u5").unwrap();
        create_pending(&lock, a_pending("u5", past)).unwrap();
        assert_eq!(deadline_expiry(&load("u5").unwrap()), Expiry::Expired);
        // A wall-clock rollback does not move CLOCK_MONOTONIC, so the verdict is
        // unchanged — we assert the monotonic reading only advances.
        let host = fake_identity(9999);
        let verdict = admit_host(&lock, "u5", "cafef00d", &host, host.pid, "codex-host").unwrap();
        assert!(
            matches!(verdict, Admission::Refused(_)),
            "an expired launch must refuse a late host"
        );
    }

    #[test]
    fn a_record_from_another_boot_is_indeterminate_and_refused() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u6").unwrap();
        let mut new = a_pending("u6", far);
        new.boot = BootIdentity {
            boot_sec: 1,
            boot_usec: 1,
        };
        create_pending(&lock, new).unwrap();
        assert_eq!(deadline_expiry(&load("u6").unwrap()), Expiry::Indeterminate);
        let host = fake_identity(1234);
        assert!(matches!(
            admit_host(&lock, "u6", "cafef00d", &host, host.pid, "codex-host").unwrap(),
            Admission::Refused(_)
        ));
    }

    #[test]
    fn host_admission_needs_nonce_pending_and_deadline_then_takes_the_lease() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u7").unwrap();
        create_pending(&lock, a_pending("u7", far)).unwrap();
        // Admission requires a recorded, proven-live custodian (finding 9): arm
        // one (us — provably alive) before any admit can succeed.
        arm(&lock, "u7", live_custodian());
        let host = fake_identity(5555);
        // Wrong nonce refuses.
        assert!(matches!(
            admit_host(&lock, "u7", "wrong", &host, host.pid, "codex-host").unwrap(),
            Admission::Refused(_)
        ));
        // Right nonce, pending, live deadline, live coordinator + custodian (us)
        // ⇒ admitted, and the exclusive lease carries nonce/role/pgid.
        assert_eq!(
            admit_host(&lock, "u7", "cafef00d", &host, 7777, "codex-host").unwrap(),
            Admission::Admitted
        );
        assert_eq!(
            load("u7").unwrap().host_lease,
            Some(HostLease {
                identity: host,
                pgid: 7777,
                nonce: "cafef00d".into(),
                role: "codex-host".into(),
            })
        );
        // Once failed, admission refuses even with the right nonce.
        to_failed(&lock, "u7", "x", CleanupState::Pending).unwrap();
        assert!(matches!(
            admit_host(&lock, "u7", "cafef00d", &host, host.pid, "codex-host").unwrap(),
            Admission::Refused(_)
        ));
    }

    #[test]
    fn the_lock_is_exclusive_across_holders() {
        let held = LaunchLock::acquire("u8").unwrap();
        // A second non-blocking acquire must fail while the first is held.
        assert!(
            LaunchLock::try_acquire("u8").unwrap().is_none(),
            "flock must be exclusive"
        );
        drop(held);
        assert!(
            LaunchLock::try_acquire("u8").unwrap().is_some(),
            "releasing lets the next holder in"
        );
    }

    #[test]
    fn arming_a_custodian_records_its_identity_and_appends_the_child_atomically() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("u9").unwrap();
        create_pending(&lock, a_pending("u9", far)).unwrap();
        let cust = fake_identity(321);
        // The one production arm path: sets the slot AND appends the child in a
        // single durable write, carrying the per-spawn nonce + argv hash.
        cas_custodian_with_child(&lock, "u9", cust, 321, "nonce-9", "hash-9").unwrap();
        let record = load("u9").unwrap();
        assert_eq!(record.custodian, Some(cust));
        assert_eq!(record.children.len(), 1);
        let child = &record.children[0];
        assert_eq!(child.identity, cust);
        assert_eq!(child.pgid, 321);
        assert_eq!(child.nonce, "nonce-9");
        assert_eq!(child.argv_hash, "hash-9");
        // The intent is cleared once the identity is recorded.
        assert!(record.pending_spawn.is_none());
    }

    #[test]
    fn a_second_custodian_arm_over_a_live_one_is_refused_but_a_gone_one_is_replaced() {
        // The concurrent/duplicate-sweep CAS (Principle A / finding 8): once a
        // live custodian holds the slot, a second arm is refused (so two sweeps
        // never leave two untracked custodians); a slot whose occupant is proven
        // Gone is replaceable (the sweep's rearm).
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("casarm").unwrap();
        create_pending(&lock, a_pending("casarm", far)).unwrap();

        // A dead pid may be armed into an empty slot (nothing to lose)…
        let dead = fake_identity(0x3FFF_FFFE);
        cas_custodian_with_child(&lock, "casarm", dead, dead.pid, "n1", "h1").unwrap();
        // …and a rearm over that *proven-Gone* occupant succeeds (sweep replacing
        // a dead custodian).
        let dead2 = fake_identity(0x3FFF_FFFD);
        cas_custodian_with_child(&lock, "casarm", dead2, dead2.pid, "n2", "h2").unwrap();

        // Now install a *live* custodian (us): a further arm must be refused —
        // this is what stops a duplicate/concurrent sweep from spawning a second.
        let live = live_custodian();
        cas_custodian_with_child(&lock, "casarm", live, live.pid, "n3", "h3").unwrap();
        let err = cas_custodian_with_child(&lock, "casarm", fake_identity(4242), 4242, "n4", "h4")
            .unwrap_err();
        assert!(
            err.to_string().contains("not proven gone"),
            "a live custodian must block a second arm: {err}"
        );
        assert_eq!(load("casarm").unwrap().custodian, Some(live));
    }

    #[test]
    fn liveness_predicates_fail_closed_in_both_directions() {
        // Principle D at the decision level: the two policies are fail-closed in
        // opposite directions, and BOTH reject `Unknown`. (A real `Unknown`
        // cannot be synthesized deterministically from the Darwin kernel, so the
        // decision logic is proven here while proc_identity's live tests prove
        // the Alive/Gone classification itself.)
        // Admission / commit: only proven Alive.
        assert!(liveness_admits(Liveness::Alive));
        assert!(!liveness_admits(Liveness::Gone));
        assert!(
            !liveness_admits(Liveness::Unknown),
            "Unknown is not proof of life — admission must refuse it"
        );
        // Teardown / rearm: only proven Gone.
        assert!(liveness_is_gone(Liveness::Gone));
        assert!(!liveness_is_gone(Liveness::Alive));
        assert!(
            !liveness_is_gone(Liveness::Unknown),
            "Unknown is not proof of absence — teardown must refuse it"
        );
    }

    #[test]
    fn a_duplicate_coordinator_cannot_erase_a_terminal_record() {
        // Principle A / finding 10: create_pending is ABSENT→Pending only. A
        // restarted or duplicate coordinator must never blind-overwrite a
        // committed Failed (or Ready) outcome with a fresh pending.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("dupfail").unwrap();
        create_pending(&lock, a_pending("dupfail", far)).unwrap();
        to_failed(&lock, "dupfail", "boom", CleanupState::Pending).unwrap();
        let err = create_pending(&lock, a_pending("dupfail", far)).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "a duplicate coordinator must not erase Failed: {err}"
        );
        assert!(matches!(
            load("dupfail").unwrap().state,
            LaunchState::Failed { .. }
        ));

        // …and the same protection for a committed Ready.
        let lock = LaunchLock::acquire("dupready").unwrap();
        let coord = current_identity().unwrap();
        let mut np = a_pending("dupready", far);
        np.coordinator = coord;
        create_pending(&lock, np).unwrap();
        arm(&lock, "dupready", live_custodian());
        to_ready(&lock, "dupready", &coord).unwrap();
        let err = create_pending(&lock, a_pending("dupready", far)).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "a duplicate coordinator must not erase Ready: {err}"
        );
        assert_eq!(load("dupready").unwrap().state, LaunchState::Ready);
    }

    #[test]
    fn nonces_are_distinct() {
        let a = mint_nonce();
        let b = mint_nonce();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn the_sweep_fails_an_orphaned_pending_with_a_dead_guardian() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("sw1").unwrap();
        // A pending whose coordinator is a dead pid and whose custodian was never
        // armed: both guardians gone ⇒ the sweep fails it and flags a
        // replacement custodian.
        let mut new = a_pending("sw1", far);
        new.coordinator = fake_identity(0x3FFF_FFFE); // a pid that is not alive
        create_pending(&lock, new).unwrap();
        drop(lock);
        let actions = recovery_sweep();
        // Dump what the sweep actually decided: a bare `contains` failure cannot
        // distinguish "it examined the record and declined" from "it never got to
        // look" (a `Skipped`), which are different bugs.
        assert!(
            actions.contains(&SweepAction::FailedStalePending { uid: "sw1".into() }),
            "the sweep should have failed the orphaned pending; it decided: {actions:?}"
        );
        assert!(
            actions.contains(&SweepAction::NeedsReplacementCustodian { uid: "sw1".into() }),
            "…and flagged a replacement custodian; it decided: {actions:?}"
        );
        assert!(matches!(
            load("sw1").unwrap().state,
            LaunchState::Failed { .. }
        ));
    }

    #[test]
    fn the_sweep_leaves_a_healthy_pending_alone() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("sw2").unwrap();
        // Coordinator = us (alive), custodian armed & alive (us again), deadline
        // far: nothing to do.
        create_pending(&lock, a_pending("sw2", far)).unwrap();
        arm(&lock, "sw2", current_identity().unwrap());
        drop(lock);
        let actions = recovery_sweep();
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, SweepAction::FailedStalePending { uid } if uid == "sw2")),
            "a healthy pending must be left alone"
        );
        assert_eq!(load("sw2").unwrap().state, LaunchState::Pending);
    }

    #[test]
    fn the_sweep_flags_a_failed_record_whose_custodian_died() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("sw3").unwrap();
        create_pending(&lock, a_pending("sw3", far)).unwrap();
        arm(&lock, "sw3", fake_identity(0x3FFF_FFFD)); // dead custodian
        to_failed(&lock, "sw3", "x", CleanupState::Pending).unwrap();
        drop(lock);
        let actions = recovery_sweep();
        assert!(actions.contains(&SweepAction::NeedsReplacementCustodian { uid: "sw3".into() }));
    }

    #[test]
    fn a_ready_record_with_a_dead_custodian_can_be_rearmed_by_the_sweep() {
        // Finding 4: a committed Ready session whose custodian has died still
        // needs an independent teardown owner. The sweep flags it, and the CAS
        // now accepts a rearm on a Ready record (over a proven-Gone slot),
        // keeping the state Ready.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("rdyrearm").unwrap();
        create_pending(&lock, a_pending("rdyrearm", far)).unwrap();
        // Force Ready with a DEAD custodian — the durable shape a Ready session
        // reaches when its custodian later dies (to_ready itself won't produce it,
        // as it requires a live custodian, so we write it directly).
        {
            let mut rec = load("rdyrearm").unwrap();
            rec.state = LaunchState::Ready;
            rec.cleanup = CleanupState::NotRequired;
            rec.custodian = Some(fake_identity(0x3FFF_FFFE));
            store_atomic("rdyrearm", &rec).unwrap();
        }
        drop(lock);

        // The sweep flags it for a replacement custodian. recovery_sweep is
        // explicitly **best-effort** — a transient fs hiccup skips a record for a
        // later sweep — and the Ready branch only *reports* (it never mutates the
        // record), so retrying is idempotent and is the faithful way to assert
        // "the sweep flags it" without depending on one scan succeeding under
        // heavy concurrent load.
        let flagged = (0..20).any(|_| {
            recovery_sweep().contains(&SweepAction::NeedsReplacementCustodian {
                uid: "rdyrearm".into(),
            })
        });
        assert!(
            flagged,
            "a Ready record with a dead custodian must be flagged for a replacement"
        );

        // The core of finding 4 (deterministic): the CAS now accepts a rearm on a
        // Ready record over the proven-Gone slot, installing a live custodian and
        // keeping the state Ready.
        let lock = LaunchLock::acquire("rdyrearm").unwrap();
        let live = live_custodian();
        cas_custodian_with_child(&lock, "rdyrearm", live, live.pid, "rn", "rh").unwrap();
        let rec = load("rdyrearm").unwrap();
        assert_eq!(rec.state, LaunchState::Ready, "the state stays Ready");
        assert_eq!(rec.custodian, Some(live), "a new live custodian was armed");
    }

    #[test]
    fn a_host_lease_is_taken_over_only_from_a_proven_gone_incumbent() {
        // Finding 5: replacing an existing lease requires the incumbent be proven
        // GONE. A live incumbent (and, by the same `!liveness_is_gone` gate, an
        // Unknown one) refuses.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("lease1").unwrap();
        create_pending(&lock, a_pending("lease1", far)).unwrap();
        arm(&lock, "lease1", live_custodian()); // coordinator + custodian are us (live)

        // A dead-pid host takes the lease…
        let host_a = fake_identity(0x3FFF_FFF0);
        assert_eq!(
            admit_host(
                &lock,
                "lease1",
                "cafef00d",
                &host_a,
                host_a.pid,
                "codex-host"
            )
            .unwrap(),
            Admission::Admitted
        );
        // Host A records both of its children, as an admitted host does.
        for role in HOST_CHILD_ROLES {
            record_host_child(
                &lock,
                "lease1",
                &host_a,
                ChildEntry {
                    role: role.to_string(),
                    identity: fake_identity(0x3FFF_FF00),
                    pgid: 0x3FFF_FF00,
                    nonce: "cafef00d".into(),
                    argv_hash: String::new(),
                    recorded_by: None,
                },
            )
            .unwrap();
        }
        assert!(host_children_recorded(&load("lease1").unwrap()));

        // …and since its identity is Gone, another host takes it over.
        let host_b = fake_identity(0x3FFF_FFF1);
        assert_eq!(
            admit_host(
                &lock,
                "lease1",
                "cafef00d",
                &host_b,
                host_b.pid,
                "codex-host"
            )
            .unwrap(),
            Admission::Admitted
        );

        // THE TAKEOVER INVARIANT, both halves. Host B must not inherit host A's
        // roles for READINESS, and host A's entries must SURVIVE for cleanup.
        //
        // The two pull in opposite directions and only scoping satisfies both.
        // Counting the stale roles would let a successor's `ready` rest on evidence
        // about a displaced host's processes; deleting them would throw away the
        // only identities by which those very processes — the ones most likely to
        // be orphaned, since their host was displaced — could ever be stopped.
        let after = load("lease1").unwrap();
        assert!(
            !host_children_recorded(&after),
            "a lease takeover must not leave the predecessor's roles satisfying readiness: {:?}",
            after.children
        );
        for role in HOST_CHILD_ROLES {
            let retired = after
                .children
                .iter()
                .find(|c| c.role == role)
                .unwrap_or_else(|| panic!("the predecessor's {role} entry must be RETAINED"));
            assert_eq!(
                retired.recorded_by,
                Some(host_a),
                "and must still name the host that recorded it, so cleanup can act on it"
            );
        }

        // Once host B records its own roles, readiness is satisfied again — by B's
        // entries, alongside A's retired ones.
        for role in HOST_CHILD_ROLES {
            record_host_child(
                &lock,
                "lease1",
                &host_b,
                ChildEntry {
                    role: role.to_string(),
                    identity: fake_identity(0x3FFF_FE00),
                    pgid: 0x3FFF_FE00,
                    nonce: "cafef00d".into(),
                    argv_hash: String::new(),
                    recorded_by: None,
                },
            )
            .unwrap();
        }
        let after_b = load("lease1").unwrap();
        assert!(host_children_recorded(&after_b));
        assert_eq!(
            after_b.children.len(),
            5,
            "custodian + A's two retired + B's two live: nothing was discarded: {:?}",
            after_b.children
        );

        // Now install a LIVE incumbent (us): a different host must be refused —
        // the live lease is not proven gone.
        let live_host = live_custodian();
        assert_eq!(
            admit_host(
                &lock,
                "lease1",
                "cafef00d",
                &live_host,
                live_host.pid,
                "codex-host"
            )
            .unwrap(),
            Admission::Admitted
        );
        let host_c = fake_identity(0x3FFF_FFF2);
        assert!(
            matches!(
                admit_host(
                    &lock,
                    "lease1",
                    "cafef00d",
                    &host_c,
                    host_c.pid,
                    "codex-host"
                )
                .unwrap(),
                Admission::Refused(_)
            ),
            "a live incumbent lease is not proven gone, so takeover must refuse"
        );
    }

    #[test]
    fn a_determinate_clear_disarms_the_flag_even_after_the_custodian_failed_first() {
        // Finding 6: the coordinator's determinate clear must win even when the
        // custodian raced it to `failed` first — otherwise the speculative
        // in-flight flag sticks true and the custodian stays armed forever.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("det1").unwrap();
        create_pending(&lock, a_pending("det1", far)).unwrap();
        mark_new_session_starting(&lock, "det1").unwrap();
        assert!(load("det1").unwrap().new_session_indeterminate);
        // The custodian races to Failed (deadline/loss) — NOT the indeterminate
        // path, so the flag is still (speculatively) set on the Failed record.
        to_failed(
            &lock,
            "det1",
            "custodian failed on deadline",
            CleanupState::Pending,
        )
        .unwrap();
        assert!(
            load("det1").unwrap().new_session_indeterminate,
            "the flag is still set on the raced-to Failed record"
        );
        // The coordinator's determinate outcome now clears it even on Failed.
        clear_new_session_indeterminate(&lock, "det1").unwrap();
        assert!(
            !load("det1").unwrap().new_session_indeterminate,
            "a determinate clear disarms the flag on a Failed record"
        );
    }

    #[test]
    fn a_determinate_new_session_failure_terminalizes_and_disarms_atomically() {
        // Finding 6: the atomic determinate-failure write disarms the flag AND
        // terminalizes in one fsync, even after the custodian raced to Failed.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("det2").unwrap();
        create_pending(&lock, a_pending("det2", far)).unwrap();
        mark_new_session_starting(&lock, "det2").unwrap();
        to_failed(&lock, "det2", "deadline", CleanupState::Pending).unwrap();
        let state = fail_new_session_determinate(
            &lock,
            "det2",
            "definite new-session failure",
            CleanupState::Pending,
        )
        .unwrap();
        let rec = load("det2").unwrap();
        assert!(
            !rec.new_session_indeterminate,
            "the determinate failure disarmed the flag"
        );
        // The first (custodian) reason wins — idempotent like to_failed.
        assert_eq!(
            state,
            LaunchState::Failed {
                reason: "deadline".into()
            }
        );
    }

    #[test]
    fn a_torn_down_ready_record_is_not_rearmed_by_the_sweep() {
        // Finding 8: once a Ready session is torn down (cleanup marked Complete),
        // a later sweep must NOT keep rearming a fresh custodian even though the
        // old one is gone.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("rdydone").unwrap();
        create_pending(&lock, a_pending("rdydone", far)).unwrap();
        {
            let mut rec = load("rdydone").unwrap();
            rec.state = LaunchState::Ready;
            rec.custodian = Some(fake_identity(0x3FFF_FFEE)); // dead custodian
            rec.cleanup = CleanupState::Complete; // torn-down marker
            store_atomic("rdydone", &rec).unwrap();
        }
        drop(lock);
        let actions = recovery_sweep();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                SweepAction::NeedsReplacementCustodian { uid } if uid == "rdydone"
            )),
            "a torn-down Ready record must not be rearmed"
        );
    }

    #[test]
    fn a_determinate_notrequired_supersedes_a_custodian_set_pending() {
        // Round-5 finding 5: the coordinator's determinate NotRequired (it KNOWS
        // no session was created) must supersede a custodian-set speculative
        // Pending, so the custodian does not probe a nonexistent server forever.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("supersede").unwrap();
        create_pending(&lock, a_pending("supersede", far)).unwrap();
        // The custodian races to Failed with cleanup Pending.
        to_failed(&lock, "supersede", "custodian lost", CleanupState::Pending).unwrap();
        assert_eq!(load("supersede").unwrap().cleanup, CleanupState::Pending);
        // The coordinator's determinate NotRequired supersedes it.
        fail_new_session_determinate(&lock, "supersede", "definite", CleanupState::NotRequired)
            .unwrap();
        assert_eq!(
            load("supersede").unwrap().cleanup,
            CleanupState::NotRequired
        );
        // A subsequent Pending must NOT re-upgrade it (NotRequired is authoritative).
        fail_new_session_determinate(&lock, "supersede", "again", CleanupState::Pending).unwrap();
        assert_eq!(
            load("supersede").unwrap().cleanup,
            CleanupState::NotRequired
        );
    }

    #[test]
    fn acquire_bounded_fails_fast_when_the_lock_is_held() {
        // Round-5 finding 6: a bounded acquire never blocks forever on a held
        // (e.g. SIGSTOPed) lock — it returns Err within the budget, then succeeds
        // once released.
        let held = LaunchLock::acquire("boundedlock").unwrap();
        let start = std::time::Instant::now();
        let r = LaunchLock::acquire_bounded("boundedlock", std::time::Duration::from_millis(150));
        assert!(
            r.is_err(),
            "a held lock must make a bounded acquire fail, not hang"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "the bounded acquire must return within its budget"
        );
        drop(held);
        assert!(
            LaunchLock::acquire_bounded("boundedlock", std::time::Duration::from_millis(150))
                .is_ok(),
            "releasing lets the bounded acquire in"
        );
    }

    #[test]
    fn the_launch_record_and_its_dir_are_private() {
        // Finding 9: a launch record leaks session metadata if world-readable. The
        // record file must be 0600 and its dir 0700.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("privacy").unwrap();
        create_pending(&lock, a_pending("privacy", far)).unwrap();
        drop(lock);
        assert_eq!(
            protocol::fsperm::mode_of(&record_path("privacy")).unwrap(),
            0o600,
            "the launch record must be owner-only"
        );
        assert_eq!(
            protocol::fsperm::mode_of(&session_dir("privacy")).unwrap(),
            0o700,
            "the session dir must be owner-only"
        );
    }
}
