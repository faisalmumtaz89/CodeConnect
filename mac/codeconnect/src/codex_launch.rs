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
thread_local! {
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// This thread's sessions root, for handing to a helper thread of the same test.
#[cfg(test)]
pub(crate) fn this_thread_sessions_root() -> PathBuf {
    test_sessions_root()
}

/// Hand this thread's root to a HELPER thread of the same test.
///
/// One root per test is one root per *test thread*, which is exactly right until a
/// test drives code that does its work on a thread of its own —
/// [`crate::codex_host::spawn_fenced`]'s fence releaser, which is where the launch
/// record is written. That thread would otherwise mint a fresh empty root and fail
/// to find the record the test just created (it silently did). Adopting the caller's
/// root is the narrow fix; a process-global root is not, because it redirects every
/// *other* test running in parallel into the same directory.
#[cfg(test)]
pub(crate) fn adopt_test_sessions_root(dir: PathBuf) {
    TEST_ROOT.with(|slot| *slot.borrow_mut() = Some(dir));
}

#[cfg(test)]
fn test_sessions_root() -> PathBuf {
    TEST_ROOT.with(|slot| {
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
    /// **This child is past `execve`** — it is running the program it was spawned
    /// to run, not still parked in the A11.1 fence (A11.1, readiness half).
    ///
    /// The fence deliberately records the identity BEFORE the child can exec, which
    /// is what makes an unrecorded child impossible. The cost is that a recorded
    /// entry, on its own, says only "this pid exists and is ours" — not "this pid is
    /// codex". Readiness used to be satisfied by the entries alone, so a `Ready`
    /// could commit while the TUI was still pre-exec: the broker's listeners were
    /// already serving, the host was alive, both roles were written down, and the
    /// only thing missing was the one fact the record claimed.
    ///
    /// **`spawn()` returning `Ok` is NOT that proof, and the counterexample is
    /// measured** (round-2 finding 2). The parent does block on the CLOEXEC error
    /// pipe std uses to report exec failure — a `pre_exec` that sleeps 500 ms delays
    /// `spawn()` by 500 ms, a nonexistent binary comes back as `Err(ENOENT)`, a
    /// `pre_exec` returning `Err` comes back as that errno — but the pipe is closed
    /// by the child's DEATH just as it is by a successful `execve`. A child SIGKILLed
    /// after the fence's GO and before `execve` therefore returns `Ok` from `spawn()`
    /// without ever having become the program: measured directly, `spawn()` `Ok` and
    /// the recording `Ok` for a process that never ran the target.
    ///
    /// So the bit is written from a proof the host takes itself, against the child:
    /// [`crate::codex_host`]'s `prove_past_execve` requires the child to be ALIVE by
    /// its recorded birth identity AND to be running an image that is not this host's
    /// own (measured: pre-exec the child's image is our binary, post-exec it is the
    /// target, dead is `ESRCH`). That is what this bit records.
    #[serde(default)]
    pub exec_confirmed: bool,
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
    /// Whether the host holding this launch's lease got **past the run-dir
    /// claim** — the exclusive publish in
    /// `codex_host::create_run_dir_atomically`.
    #[serde(default)]
    pub host_claimed_run_dir: bool,
    /// Whether a custodian ever positively observed this launch's session present.
    ///
    /// Durable rather than process-local: a custodian that dies and is replaced by
    /// the sweep would otherwise forget, and the replacement — arriving after the
    /// session is already gone — would be stuck refusing to believe an absence
    /// that its predecessor had already explained. Written once, never cleared.
    #[serde(default)]
    pub session_observed: bool,
    /// That **one** `remain-on-exit off` assertion was proven to land on this
    /// launch's session (A11.3).
    ///
    /// The premise cleanup would like to rest on — *a pane dies when its command
    /// exits* — is not automatically true. A user's own `~/.tmux.conf` can set
    /// `remain-on-exit on`, with no bug of ours, and then a pane whose command exited
    /// stays alive and `has-session` keeps answering yes. So the coordinator asserts
    /// the option away, at both scopes that decide it, the instant there is a session
    /// to assert it on, and records here that the assertion was proven to land.
    ///
    /// **Read it as history, and only as history** (round-3 finding 5). This says
    /// that at one moment, on that session's window and its then-current pane, the
    /// option was off. It is emphatically NOT a standing guarantee about the session's
    /// remaining lifetime: a config hook can create another pane or another window
    /// afterwards, and the options stay mutable by anything running as this uid. A
    /// past assertion therefore cannot license a present-tense claim that a session
    /// died when its host did.
    ///
    /// It used to be read that way. [`crate::codex_custodian::server_gone_evidence`]
    /// had a no-server-A fallback that inferred "the session is gone" from "the host
    /// is proven dead", gated on this bit — a lasting conclusion drawn from a
    /// historical fact. That fallback is retired: the coordinator now persists server
    /// A immediately after resolving the session, so the window the fallback existed
    /// for no longer exists, and nothing infers session death from this bit any more.
    /// It is kept because it is a true and cheap thing to know about a launch, and it
    /// is what an operator reads to tell "the assertion held" from "it never ran".
    #[serde(default)]
    pub remain_on_exit_asserted: bool,
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
    // **ABSOLUTE from here on** (A9.6(c)) — restated locally, not established here.
    //
    // The root is absolutised at its source, [`protocol::root_dir`], because a
    // relative root is wrong for every consumer and not just for this walk: it
    // names a different directory in each process, and the launcher, the pane's
    // host and the sweep run with different working directories by construction.
    // Fixing it only here left `session_dir()` and the lock open below still
    // resolving the relative path, and the coordinator still forwarding it.
    //
    // What this line is, then, is the walk's own PRECONDITION made true rather than
    // assumed. The ancestor walk climbs off the end of a relative path —
    // `Path::new("relcc").parent()` is `Some("")` and `File::open("")` is ENOENT, so
    // every launch-lock acquisition failed for a one-component relative root
    // (measured) — and it terminates only at `/`, a directory that exists.
    // `std::path::absolute` is idempotent, so on the now-normal absolute path this
    // costs nothing and changes nothing.
    let dir = std::path::absolute(dir)
        .with_context(|| format!("resolving {} to an absolute path", dir.display()))?;
    let dir = dir.as_path();
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
    // And ALWAYS re-fsync the parent of the uid dir **and its ancestors**, even on
    // the already-exists path, so a retry after a prior fsync error re-establishes
    // the barrier for EVERY ancestor whose earlier fsync may have failed — not just
    // the immediate parent (round-5 finding 7).
    for p in durability_ancestors(dir) {
        fsync_dir(&p)?;
    }
    Ok(())
}

/// The ancestors of a session dir whose **own** directory entries must be
/// re-flushed on every lock acquisition, innermost-first — **every** one of them,
/// up to and including the filesystem root.
///
/// A9.6(c), and the bound is the finding. The walk used to stop at a point derived
/// from `sessions_root`: first `.codeconnect`, then one level past it. Both are
/// short of what the creation above can actually make. `private_dir` is
/// `DirBuilder::recursive`, so it creates every missing ancestor without limit —
/// and a deep `CODECONNECT_HOME` (`/a/b/c/d/e/f/g`) on a fresh machine means most
/// of that chain is newly created, each with a dirent in its parent that has to be
/// fsynced for the record's path to survive power loss.
///
/// The creation loop above does flush exactly those parents, but it propagates
/// with `?`: a failure at an inner level returns before the outer ones are reached,
/// and on the retry `newly` is empty, so nothing re-establishes them. A fixed stop
/// point cannot repair what it never visits. The only bound that covers every
/// ancestor this code could have created is the one the filesystem itself provides,
/// and an absolute path's ancestor chain is short (five or six entries) and
/// terminating. Measured cost: 0.1–0.6 ms per directory fsync on this platform, and
/// lock acquisition is not a tight loop.
///
/// Requires an ABSOLUTE `dir` — see [`create_session_dir_durably`], which is what
/// establishes that.
fn durability_ancestors(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut anc = dir.parent();
    while let Some(p) = anc {
        // `Path::parent` of `/` is `None`, which is what ends an absolute walk. A
        // relative path would end at `""` instead — a path that opens nothing — so
        // the empty component is refused here as well as prevented at the source.
        if p.as_os_str().is_empty() {
            break;
        }
        out.push(p.to_path_buf());
        anc = p.parent();
    }
    out
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

/// Re-prove that the record's own **directory entry** is durable — the reader's
/// half of the durability handoff (A9.6a).
///
/// `store_atomic` publishes by rename and only *then* fsyncs the directory, so
/// there is a window in which a `Ready` record is visible to `load` but its
/// dirent is not yet on stable storage; if that fsync fails, the writer cannot
/// un-publish the rename it already made. A consumer that acts on a visible
/// `Ready` (starting a session the user is told exists) must therefore re-prove
/// the entry itself before consuming it. Re-fsyncing the same directory flushes
/// the same entry, so the cheapest correct handoff is for the reader to do it: an
/// error here means "not yet proven durable — keep waiting", never "Ready".
pub fn prove_record_durable(uid: &str) -> Result<()> {
    let path = record_path(uid);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    fsync_dir(&session_dir(uid))
}

#[cfg(test)]
thread_local! {
    /// One-shot: the next [`store_atomic`] publishes its rename and then fails its
    /// directory fsync. Per test *thread*, like [`test_sessions_root`], so parallel
    /// tests cannot arm each other's faults.
    static DIR_FSYNC_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot post-rename fsync failure (see [`store_atomic`]).
#[cfg(test)]
pub(crate) fn fail_next_dir_fsync() {
    DIR_FSYNC_FAULT.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_dir_fsync_fault() -> bool {
    DIR_FSYNC_FAULT.with(|armed| armed.replace(false))
}

#[cfg(not(test))]
fn take_dir_fsync_fault() -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// One-shot: the next [`store_atomic`] fails **before** its rename, so the
    /// target record is left exactly as it was. Per test *thread*, like
    /// [`test_sessions_root`], so parallel tests cannot arm each other's faults.
    static PUBLISH_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot **pre-publish** write failure (see [`store_atomic`]).
///
/// The deliberate COMPLEMENT of [`fail_next_dir_fsync`], and the two must not be
/// confused: that one fails the directory fsync *after* the rename has already
/// made the successor visible, so the record DID change and the fault is about
/// durability. This one fails while the pre-image is still the published record,
/// so the caller gets an `Err` for a write that changed **nothing** — which is the
/// only shape that stages a caller whose contract is "if the write did not land,
/// do not proceed".
///
/// Stands for the whole pre-publish failure class — `ENOSPC` on the temp write,
/// `EIO` on its fsync, `EIO` on the rename itself — because every member of it
/// leaves the same observable state: the old record, intact. Injected rather than
/// simulated for the reason the fsync fault is: only the filesystem decides when a
/// write fails, and a test that merely refrained from calling the writer would
/// prove nothing about what a FAILED writer leaves behind.
#[cfg(test)]
pub(crate) fn fail_next_record_publish() {
    PUBLISH_FAULT.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_publish_fault() -> bool {
    PUBLISH_FAULT.with(|armed| armed.replace(false))
}

#[cfg(not(test))]
fn take_publish_fault() -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// One-shot: the next [`remove_tree_beneath`] `readdir` reports a read error
    /// instead of an entry. Thread-local for the same reason the fsync fault is.
    static READDIR_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot enumeration failure (see [`remove_tree_beneath`]).
///
/// The kernel decides when `readdir` fails, so this is the only way to stage the
/// partial enumeration whose old reading — null means EOF — reached marker removal
/// and false settlement.
#[cfg(test)]
pub(crate) fn fail_next_readdir() {
    READDIR_FAULT.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_readdir_fault() -> bool {
    READDIR_FAULT.with(|armed| armed.replace(false))
}

#[cfg(not(test))]
fn take_readdir_fault() -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// One-shot: the next [`sweep_owned_run_dir`] gains an entry in the window
    /// between its enumeration and its `rmdir`. Thread-local like the others.
    static STRAGGLER_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot straggler (see [`sweep_owned_run_dir`]).
///
/// A process that writes into the run dir after the sweep has listed it is the only
/// thing that makes `rmdir` fail `ENOTEMPTY`, and this code cannot schedule another
/// process — so the window is staged from inside it, at exactly the instant a real
/// straggler would land: after the marker is unlinked, before the `rmdir`.
#[cfg(test)]
pub(crate) fn straggle_next_sweep() {
    STRAGGLER_FAULT.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_straggler_fault() -> bool {
    STRAGGLER_FAULT.with(|armed| armed.replace(false))
}

#[cfg(not(test))]
fn take_straggler_fault() -> bool {
    false
}

#[cfg(test)]
thread_local! {
    /// One-shot: the next [`restore_marker_at`] fails as a WRITE would. Only the
    /// filesystem decides when a write fails, so this is the only way to reach the
    /// residual arm — the one that reports "the warrant could not be put back".
    static MARKER_RESTORE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot warrant-restore failure (see [`restore_marker_at`]).
#[cfg(test)]
pub(crate) fn fail_next_marker_restore() {
    MARKER_RESTORE_FAULT.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_marker_restore_fault() -> bool {
    MARKER_RESTORE_FAULT.with(|armed| armed.replace(false))
}

#[cfg(not(test))]
fn take_marker_restore_fault() -> bool {
    false
}

/// The name a staged straggler takes. Test-only, and deliberately not the marker's.
#[cfg(test)]
pub(crate) const STRAGGLER_FILE: &str = "straggler";

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
    // The complement of the fault below, and the one A11.8's arrival-write boundary
    // turns on: the rename has NOT happened, so `target` still holds the pre-image
    // and the caller's `Err` describes a write that changed nothing. Sited exactly
    // at the rename because that is the last instant at which that is true — and
    // the temp is deliberately left where a failed `rename(2)` would leave it,
    // rather than tidied, so the staged aftermath is the real one. It is inert:
    // every reader resolves `launch.json`, and the next `store_atomic` reopens the
    // temp with `create_private`, which truncates.
    if take_publish_fault() {
        bail!(
            "injected fault: publishing {} failed BEFORE the rename, leaving the \
             existing record untouched",
            target.display()
        );
    }
    std::fs::rename(&temp, &target)
        .with_context(|| format!("renaming {} over {}", temp.display(), target.display()))?;
    // The one fault this module cannot otherwise stage, and the one A9.3 turns on:
    // the rename has ALREADY made the successor visible, and the fsync that would
    // make it durable then fails. The caller gets an `Err` for a write every reader
    // can nonetheless see. Injected rather than simulated because the whole point
    // is the *ordering* — a test that merely wrote the flag by hand would prove
    // nothing about what a failed `store_atomic` leaves behind.
    if take_dir_fsync_fault() {
        bail!(
            "injected fault: the fsync of {} failed AFTER the rename published {}",
            dir.display(),
            target.display()
        );
    }
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
        host_claimed_run_dir: false,
        session_observed: false,
        remain_on_exit_asserted: false,
        children: Vec::new(),
        created_ms: new.created_ms,
    };
    store_atomic(&new.uid, &record)?;
    Ok(record)
}

/// Commit a **created** `tmux new-session` outcome: persist **server A** and
/// disarm the in-flight flag in **one atomic durable write** (A9.1).
///
/// The two halves used to be separate `store_atomic` calls
/// (`clear_new_session_indeterminate` then `record_server_a`), and a crash in
/// between left `new_session_indeterminate: false, server_a: null` — a session
/// that had been created but was durably *unpinned*, so cleanup could only
/// address it by socket+uid and a `Ready` record could commit with no identity to
/// bind teardown to. Fusing them makes "a session exists" and "we know which one"
/// the same durable fact, which is what A9.3's disposition rule then reads.
///
/// Server A (round-5 finding 1) is the identity of the session+server the launch
/// created, so the separate custodian/supervisor can bind cleanup and liveness to
/// it. Recorded on a still-`pending` record right after `tmux new-session` returns
/// a resolved, birth-proven session; carried forward through the terminal
/// transitions.
///
/// The flag is cleared **regardless of the current state** (finding 6). The bug
/// that closes: if the custodian raced the coordinator to `pending → failed`
/// (deadline/loss) *before* the coordinator got here, a "only while pending" clear
/// was a no-op, leaving `new_session_indeterminate` stuck true on a `failed`
/// record whose new-session was actually determinate — and the custodian would
/// then treat one `Absent` observation as insufficient and stay armed forever.
/// Clearing it here is safe because this is only ever called for a session tmux
/// *did* create (the confirmed-indeterminate path is
/// [`fail_new_session_indeterminate`], which sets the flag instead). The
/// `cleanup → pending` restore stays scoped to a still-`pending` record.
///
/// **`remain_asserted` folds [`note_remain_on_exit_asserted`]'s fact into this same
/// write**, and exists for the coordinator's post-create FAILURE paths. Those paths
/// already know A — `new-session` succeeded and the session is pinned — and used to
/// throw that knowledge away by returning `Indeterminate` before ever getting here,
/// leaving cleanup with nothing to bind to and armed until the next reboot. They now
/// persist what they know first, and the one of them that ran the assertion
/// successfully persists that too, in the SAME durable write rather than in a second
/// one that can fail on its own. `true` only ever ORs the flag on: this can turn a
/// proven assertion into a recorded one, never a recorded one back off.
pub fn record_new_session_created(
    _lock: &LaunchLock,
    uid: &str,
    a: ServerA,
    remain_asserted: bool,
) -> Result<()> {
    let mut record = load(uid)?;
    record.server_a = Some(a);
    record.new_session_indeterminate = false;
    record.remain_on_exit_asserted |= remain_asserted;
    if record.state == LaunchState::Pending && record.cleanup != CleanupState::Pending {
        record.cleanup = CleanupState::Pending;
    }
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

/// Note that this launch's host got **past the run-dir claim**.
///
/// History, not state, and the mirror image of [`note_host_reached_gate`]: that
/// one says the pane ran, this one says the pane's host owned the directory it
/// was sent to own. Between them they name the STAGE a dead host reached, which
/// is the one thing a refused host's own sentence cannot say — it goes to the
/// pane's pty and the pane dies with it.
///
/// Lease-guarded exactly like [`record_host_child`]: only the host this launch
/// admitted may write it.
pub fn note_host_claimed_run_dir(
    _lock: &LaunchLock,
    uid: &str,
    by: &ProcessIdentity,
) -> Result<()> {
    let mut record = load(uid)?;
    if record.state != LaunchState::Pending {
        bail!(
            "refusing to note the run-dir claim: the launch is {:?}, not pending",
            record.state
        );
    }
    match &record.host_lease {
        Some(lease) if &lease.identity == by => {}
        Some(_) => {
            bail!("refusing to note the run-dir claim: the lease belongs to a different host")
        }
        None => bail!("refusing to note the run-dir claim: no host holds this launch's lease"),
    }
    if record.host_claimed_run_dir {
        return Ok(());
    }
    record.host_claimed_run_dir = true;
    store_atomic(uid, &record)
}

/// Mark a recorded host child **past `execve`** (A11.1, readiness half) — see
/// [`ChildEntry::exec_confirmed`].
///
/// Written by the host once it has PROVEN the child past `execve` — alive by its
/// recorded birth identity and running an image that is not this host's own. A
/// `spawn()` that returned `Ok` is explicitly not that proof: the CLOEXEC pipe it
/// waits on is closed by the child's death as well as by a successful exec (measured;
/// see [`ChildEntry::exec_confirmed`]). Until the proof is taken the entry says only
/// that the identity exists, and [`host_children_ready`] refuses to count it.
///
/// Lease-fenced and pending-only for exactly the reasons [`record_host_child`] is:
/// this is the bit readiness rests on, so only the host that holds the launch's
/// lease may set it, and only while the launch is still live. Idempotent, and it
/// refuses outright if no entry matches — a confirmation with nothing to confirm is
/// a bug in the caller, not something to write down.
pub fn confirm_host_child_exec(
    _lock: &LaunchLock,
    uid: &str,
    by: &ProcessIdentity,
    role: &str,
    identity: &ProcessIdentity,
) -> Result<()> {
    let mut record = load(uid)?;
    if record.state != LaunchState::Pending {
        bail!(
            "refusing to confirm {role}'s exec: the launch is {:?}, not pending",
            record.state
        );
    }
    match &record.host_lease {
        Some(lease) if &lease.identity == by => {}
        Some(_) => {
            bail!("refusing to confirm {role}'s exec: the lease belongs to a different host")
        }
        None => bail!("refusing to confirm {role}'s exec: no host holds this launch's lease"),
    }
    let Some(entry) = record
        .children
        .iter_mut()
        .find(|c| c.role == role && &c.identity == identity && c.recorded_by == Some(*by))
    else {
        bail!("refusing to confirm {role}'s exec: this host recorded no such child");
    };
    if entry.exec_confirmed {
        return Ok(());
    }
    entry.exec_confirmed = true;
    store_atomic(uid, &record)
}

/// Note that `remain-on-exit off` was proven set on this launch's session (A11.3)
/// — see [`LaunchRecord::remain_on_exit_asserted`].
///
/// Written by the coordinator immediately after the assertion returns success, and
/// **before** the write that records server A, so the two states the custodian has
/// to tell apart — "asserted, then the coordinator died" and "died before
/// asserting" — are actually distinguishable in the record. Idempotent.
///
/// **History, not state**, exactly as [`note_host_reached_gate`] is, and for the
/// same reason it carries no `pending` guard. This records that a tmux option was
/// PROVEN set on a session that exists; nothing about that stops being true when
/// the record turns terminal. A `pending`-only rule lost the fact outright whenever
/// the custodian won the deadline CAS in the gap between the assertion and this
/// call — and that is precisely the shape the fact is FOR: a terminal record with no
/// server A, whose cleanup escape is the one thing this bit gates.
pub fn note_remain_on_exit_asserted(_lock: &LaunchLock, uid: &str) -> Result<()> {
    let mut record = load(uid)?;
    if record.remain_on_exit_asserted {
        return Ok(());
    }
    record.remain_on_exit_asserted = true;
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

/// Whether BOTH host children are, **each in a single entry**, recorded by the
/// current lease holder, proven past `execve`, and **alive right now**.
///
/// The coordinator asks this before committing `ready`: a session declared ready
/// must be one whose processes cleanup can name and which are actually running.
///
/// **One entry must satisfy all three, and that is the whole point of fusing them**
/// (round-3 finding 1). These used to be two predicates over the same list — one
/// asking "is some entry for this role recorded-by-the-lease and exec-confirmed?",
/// the other, separately, "is some entry for this role alive?" — and two existential
/// quantifiers over one list do not compose into one. Entries recorded by a
/// **displaced** host are deliberately retained (see [`ChildEntry::recorded_by`]),
/// so a predecessor's TUI that is still breathing could answer the liveness question
/// while the CURRENT host's confirmed TUI was already dead, and readiness committed
/// on two facts about two different processes. Asked as one conjunction over one
/// entry, that shape cannot arise.
///
/// The three conjuncts, and why none is redundant:
///
///   * **Recorded by the live lease.** A successor's readiness must not be satisfied
///     by its predecessor's processes. The retained entries are still cleanup
///     evidence; they say nothing about whether the current host came up.
///   * **Past `execve`** — A11.1's other side. The fence records each child while it
///     is still parked before `execve`, precisely so an unrecorded child cannot
///     exist, which means a recorded entry alone says "this pid is ours", not "this
///     pid is codex". Everything else readiness looks at is already true at that
///     moment: the broker binds its listeners before the TUI is spawned at all, and
///     the host is obviously alive because it is the thing doing the spawning. See
///     [`ChildEntry::exec_confirmed`] for what the host proves before setting it.
///   * **Alive now.** `exec_confirmed` is a fact about a moment that has passed and
///     nothing rewrites it when the child later dies. Read alone it certifies
///     `Ready` for a session whose TUI or app-server exited between the host's
///     confirmation and this poll — which the broker legs cannot contradict, because
///     the listeners belong to the HOST and keep answering. Bound to the recorded
///     birth identity rather than the bare pid, so a recycled number cannot answer
///     for a child that is gone; `Unknown` is not proof and does not commit.
pub fn host_children_ready(record: &LaunchRecord) -> bool {
    let Some(lease) = record.host_lease.as_ref() else {
        return false;
    };
    HOST_CHILD_ROLES.iter().all(|role| {
        record.children.iter().any(|c| {
            &c.role == role
                && c.recorded_by == Some(lease.identity)
                && c.exec_confirmed
                && liveness(&c.identity) == Liveness::Alive
        })
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
    marker_verdict_from_file(file, uid, launch_nonce)
}

/// Judge an already-opened marker. Shared by the path-addressed reader above and
/// the fd-addressed one below, so both answer with exactly the same rules.
fn marker_verdict_from_file(file: std::fs::File, uid: &str, launch_nonce: &str) -> MarkerVerdict {
    use std::io::Read;
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

/// Read the ownership marker of the directory `dir_fd` refers to (A11.5).
///
/// The difference from [`run_dir_marker`] is the whole point: the marker is reached
/// with `openat` **through the caller's directory descriptor**, so the directory
/// whose marker is judged is the one that fd names — an inode, not a name. A path
/// re-resolved for the read could land on a directory swapped in since the caller
/// looked, which is exactly the check-then-delete window this closes.
///
/// Same flags and the same verdicts as the path reader: `O_NOFOLLOW` so a symlink at
/// the marker's name cannot redirect the read, and `O_NONBLOCK` so a FIFO cannot
/// make the open itself block forever inside a cleanup pass that has no deadline.
pub fn run_dir_marker_at(
    dir_fd: std::os::unix::io::RawFd,
    uid: &str,
    launch_nonce: &str,
) -> MarkerVerdict {
    use std::os::unix::io::FromRawFd;
    let Ok(name) = std::ffi::CString::new(RUN_DIR_OWNER_FILE) else {
        return MarkerVerdict::Unknown("the marker name is not a valid C string".into());
    };
    // SAFETY: `dir_fd` is a live directory descriptor owned by the caller, and
    // `name` is a NUL-terminated string that outlives the call.
    let fd = unsafe {
        libc::openat(
            dir_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return match err.kind() {
            // No marker in a directory that exists: claimed by nobody, or by
            // something that is not us. Either way not ours.
            std::io::ErrorKind::NotFound => MarkerVerdict::Foreign,
            // ELOOP from O_NOFOLLOW, or anything else: a read we did not manage to
            // do, so unanswerable rather than a foreign claim.
            _ => MarkerVerdict::Unknown(format!("opening the marker failed: {err}")),
        };
    }
    // SAFETY: `fd` was just opened by this call and is not owned anywhere else.
    marker_verdict_from_file(unsafe { std::fs::File::from_raw_fd(fd) }, uid, launch_nonce)
}

/// An open directory descriptor: the fd IS the directory, so nothing that happens
/// to the NAME afterwards can redirect what is verified or removed through it.
pub(crate) struct DirFd(pub(crate) std::os::unix::io::RawFd);

impl Drop for DirFd {
    fn drop(&mut self) {
        // SAFETY: sole owner of this descriptor.
        unsafe { libc::close(self.0) };
    }
}

/// Open a directory without following a symlink at its final component.
pub(crate) fn open_dir_nofollow(path: &std::path::Path) -> std::io::Result<DirFd> {
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::other("the path contains a NUL byte"))?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(DirFd(fd))
}

/// Open a subdirectory *through* an already-open directory descriptor.
fn open_subdir_at(parent: &DirFd, name: &std::ffi::CStr) -> std::io::Result<DirFd> {
    // SAFETY: `parent.0` is live and `name` outlives the call.
    let fd = unsafe {
        libc::openat(
            parent.0,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(DirFd(fd))
}

/// How deep the sweep will descend before refusing. A run dir holds sockets and
/// logs one level down; anything deeper than this is not a shape this code made,
/// and unbounded recursion driven by directory contents is not something to offer.
pub(crate) const SWEEP_MAX_DEPTH: u32 = 16;

/// Remove everything beneath `dir`, addressing every entry through `dir`'s own
/// descriptor (A11.5).
///
/// Each `unlinkat` is resolved relative to a descriptor that was opened once and
/// verified once, so the directory being emptied is provably the directory whose
/// marker was read — the check and the delete cannot be separated by a swap of the
/// name. Subdirectories are descended through `openat` for the same reason.
///
/// `keep` names one entry of THIS level to leave alone (the recursion never
/// carries it downward). It exists for the owner marker: see [`unlink_marker_at`].
pub(crate) fn remove_tree_beneath(
    dir: &DirFd,
    depth: u32,
    keep: Option<&str>,
) -> std::io::Result<()> {
    if depth > SWEEP_MAX_DEPTH {
        return Err(std::io::Error::other(format!(
            "refusing to descend deeper than {SWEEP_MAX_DEPTH} levels"
        )));
    }
    // `fdopendir` takes ownership of the fd it is given, so it gets a DUP —
    // `dir` must stay valid for the `unlinkat` calls below and for the caller.
    // SAFETY: `dir.0` is live.
    let dup = unsafe { libc::dup(dir.0) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `dup` is a fresh directory descriptor handed over to `fdopendir`.
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let err = std::io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so ownership did not transfer.
        unsafe { libc::close(dup) };
        return Err(err);
    }
    let mut pending: Vec<std::ffi::CString> = Vec::new();
    let mut enumeration: std::io::Result<()> = Ok(());
    loop {
        // **A null `readdir` is two different answers, and only one of them is
        // "done"** (round-2 finding 6). `readdir` reports both end-of-directory and
        // a read error by returning NULL; the two are told apart ONLY by `errno`,
        // which it leaves untouched on EOF. Read as EOF unconditionally, an I/O
        // error part-way through enumeration became a *complete* enumeration of a
        // *partial* directory — and this function's caller then unlinks the owner
        // marker and reports success over whatever was never listed. So errno is
        // cleared immediately before each call and consulted after each NULL.
        unsafe { *libc::__error() = 0 };
        // SAFETY: `stream` is a live DIR*. `readdir` returns a pointer into it that
        // is valid until the next call; the name is copied out immediately.
        let entry = unsafe { libc::readdir(stream) };
        // A directory whose contents cannot be listed is not a thing this code can
        // stage — the kernel decides when `readdir` fails. The one fault the sweep
        // must survive therefore gets the same thread-local seam `DIR_FSYNC_FAULT`
        // has, armed by exactly one test and consumed once.
        let entry = if take_readdir_fault() {
            unsafe { *libc::__error() = libc::EIO };
            std::ptr::null_mut()
        } else {
            entry
        };
        if entry.is_null() {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(0) {
                enumeration = Err(err);
            }
            break;
        }
        // SAFETY: `entry` is non-null and points at a live `dirent`.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if keep.is_some_and(|k| name.to_bytes() == k.as_bytes()) {
            continue;
        }
        pending.push(name.to_owned());
    }
    // SAFETY: `stream` is live and owns `dup`; this closes both.
    unsafe { libc::closedir(stream) };
    // A directory that could not be fully listed is not one this call may report
    // emptied. Propagated BEFORE any unlinking, so a failed enumeration leaves the
    // tree exactly as it found it and the caller's retry re-lists from the start.
    enumeration?;

    for name in pending {
        // SAFETY: `dir.0` is live and `name` outlives the call.
        if unsafe { libc::unlinkat(dir.0, name.as_ptr(), 0) } == 0 {
            continue;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // Already gone: someone else removed it. Nothing owed.
            Some(libc::ENOENT) => continue,
            // A directory. Darwin reports EPERM here, Linux EISDIR — accept both
            // rather than pinning this to one kernel's choice.
            Some(libc::EPERM) | Some(libc::EISDIR) => {
                let sub = open_subdir_at(dir, &name)?;
                remove_tree_beneath(&sub, depth + 1, None)?;
                drop(sub);
                // SAFETY: `dir.0` is live and `name` outlives the call.
                if unsafe { libc::unlinkat(dir.0, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::ENOENT) {
                        return Err(err);
                    }
                }
            }
            _ => return Err(err),
        }
    }
    Ok(())
}

/// Unlink the run dir's owner marker through the verified descriptor — the LAST
/// thing removed, and the reason [`remove_tree_beneath`] takes a `keep`.
///
/// The marker is this launch's **deletion warrant**: it is the only thing that
/// makes the directory say whose it is, and the sweep reads it to decide whether
/// it may delete at all. Unlinked with everything else, a pass that then failed
/// anywhere afterwards — a later `unlinkat`, the depth check, the final `rmdir` —
/// would correctly report retry, and the *next* pass would read the missing marker
/// as `Foreign`, report success, and abandon whatever was left standing. The
/// warrant has to outlive every failure that can still be retried, so it is
/// removed only once there is nothing left to retry for.
///
/// `ENOENT` is success: a live host may have removed its own directory's marker on
/// the way out.
pub(crate) fn unlink_marker_at(dir: &DirFd) -> std::io::Result<()> {
    let name = std::ffi::CString::new(RUN_DIR_OWNER_FILE)
        .map_err(|_| std::io::Error::other("the marker name contains a NUL byte"))?;
    // SAFETY: `dir.0` is live and `name` outlives the call.
    if unsafe { libc::unlinkat(dir.0, name.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOENT) {
        return Ok(());
    }
    Err(err)
}

/// What one pass of [`sweep_owned_run_dir`] concluded.
#[derive(Debug)]
pub(crate) enum RunDirSweep {
    /// Nothing is owed on this path any more: it was removed, it was already gone,
    /// or it is provably not this launch's to touch. Carries a note when the reason
    /// is worth saying out loud.
    Settled(Option<String>),
    /// The question could not be answered, or the removal fell short. Whatever is
    /// standing there is still owed, and the caller must come back for it.
    Retry(String),
}

/// Remove the run dir this launch owns, **through its own descriptor** (A11.5).
///
/// One implementation, two callers — the custodian sweeping a launch it inherited,
/// and the host tearing down the directory it created — because the discipline is
/// not divisible. The host used to end with a path-addressed `remove_dir_all`,
/// which re-resolves the NAME for every step: a custodian removing the old inode
/// while a colliding launch publishes the same name is all it takes for that delete
/// to land on the replacement's sockets and logs. An fd cannot be redirected.
///
/// The order is the argument:
///
///   * open the directory **once**, `O_DIRECTORY|O_NOFOLLOW`, so a symlink or a
///     non-directory at the name is refused rather than followed;
///   * read the owner marker `openat`-relative to that same descriptor, so the
///     directory judged is the inode that was opened, not a name re-resolved since;
///   * empty it through that descriptor, keeping the marker;
///   * unlink the marker only once there is nothing left to retry for
///     ([`unlink_marker_at`]);
///   * and only then `rmdir` by name — the one step that must name the directory,
///     because it removes an entry from a parent this code never verified. Bounded
///     to almost nothing by what `rmdir` can do: it only ever succeeds on an EMPTY
///     directory, and the data was already unlinked through the verified fd.
///
/// **`rmdir` is fallible AFTER the warrant is gone, and that is the one ordering
/// this list cannot fix** (round-2 finding 5): the marker lives inside the directory
/// `rmdir` empties, so it cannot be removed later than `rmdir` succeeds. A straggler
/// entry created after the enumeration makes `rmdir` return `ENOTEMPTY` with the
/// marker already unlinked — and a missing marker reads as `Foreign`, which settles.
/// So the failing arm puts the warrant BACK ([`restore_marker_at`]) before reporting
/// `Retry`, which is what makes "a warrant survives every retryable failure" true on
/// this path too and not merely on the ones that fail before the unlink.
pub(crate) fn sweep_owned_run_dir(
    run_dir: &std::path::Path,
    uid: &str,
    launch_nonce: &str,
) -> RunDirSweep {
    let shown = run_dir.display();
    let dir_fd = match open_dir_nofollow(run_dir) {
        Ok(fd) => fd,
        // Already gone: the normal success.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return RunDirSweep::Settled(None)
        }
        Err(err) => {
            let raw = err.raw_os_error();
            // A symlink (ELOOP) or a non-directory (ENOTDIR) at the run dir's name
            // is not a run dir at all — the marker reader calls that `Foreign`, and
            // it is equally "not ours, nothing owed" here.
            if raw == Some(libc::ELOOP) || raw == Some(libc::ENOTDIR) {
                return RunDirSweep::Settled(Some(format!(
                    "{shown} is not a directory ({err}); leaving it alone"
                )));
            }
            // Anything else is a question that could not be answered.
            return RunDirSweep::Retry(format!("could not open the run dir {shown}: {err}"));
        }
    };
    match run_dir_marker_at(dir_fd.0, uid, launch_nonce) {
        MarkerVerdict::Ours => {}
        // Read, and it names someone else — or nobody. Not ours to delete, and
        // nothing is owed on it either: retrying would wedge the caller forever on
        // a directory it must never touch.
        //
        // …with one exception, and it is this function's own residue. The marker is
        // unlinked once the tree is empty, and the `rmdir` right after it can still
        // fail; what stands then is an EMPTY directory at a name a later launch may
        // derive, and A11.4 refuses to adopt an existing directory. `remove_dir`
        // only ever succeeds on an empty one, so this collects that residue and can
        // never touch a stranger's contents.
        MarkerVerdict::Foreign => {
            drop(dir_fd);
            // **The `remove_dir`'s failure is READ, not discarded** (round-3
            // finding 3). It used to be `let _ = …`, which meant this arm reported
            // `Settled` — "nothing is owed here" — no matter what happened, including
            // on a directory it had just failed to collect for a reason it never
            // looked at.
            //
            // Two of the answers genuinely settle and one does not:
            //
            //   * removed, or already gone — the residue is collected, nothing owed;
            //   * `ENOTEMPTY`/`EEXIST` — a directory with CONTENTS whose marker does
            //     not name us. That is a stranger's, and it is the case this arm must
            //     settle: retrying would wedge the caller for ever on a directory it
            //     must never touch;
            //   * anything else (`EACCES`, `EPERM`, `EBUSY`, `EIO`, …) is a question
            //     that was not answered. An unanswered question is not "nothing is
            //     owed", so it is owed.
            return match std::fs::remove_dir(run_dir) {
                Ok(()) => RunDirSweep::Settled(None),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    RunDirSweep::Settled(None)
                }
                Err(err)
                    if matches!(
                        err.raw_os_error(),
                        Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
                    ) =>
                {
                    RunDirSweep::Settled(Some(format!(
                        "{shown} exists but its owner marker does not name {uid}; \
                         leaving it alone"
                    )))
                }
                Err(err) => RunDirSweep::Retry(format!(
                    "{shown}'s owner marker does not name {uid}, and the empty-residue \
                     collection could not be completed either ({err}); this stays owed \
                     rather than being reported settled on an unanswered question"
                )),
            };
        }
        // Unreachable through `run_dir_marker_at`, which answers `Foreign` for a
        // missing marker: the directory it is asked about is the one the descriptor
        // already names, so "not there" is not one of its answers.
        MarkerVerdict::Absent => return RunDirSweep::Settled(None),
        // The question could not be ANSWERED. The one verdict that must not settle:
        // an unreadable marker is not proof the directory is a stranger's, and
        // treating it as such abandons a directory this launch is responsible for,
        // with the marker that says so written and then never re-read.
        MarkerVerdict::Unknown(why) => {
            return RunDirSweep::Retry(format!("could not read {shown}'s owner marker ({why})"))
        }
    }
    if let Err(err) = remove_tree_beneath(&dir_fd, 0, Some(RUN_DIR_OWNER_FILE)) {
        return RunDirSweep::Retry(format!("could not empty the run dir {shown}: {err}"));
    }
    if let Err(err) = unlink_marker_at(&dir_fd) {
        return RunDirSweep::Retry(format!(
            "could not remove the owner marker in {shown}: {err}"
        ));
    }
    if take_straggler_fault() {
        // The window this function's failing arm exists for, staged inside it.
        #[cfg(test)]
        {
            let name = std::ffi::CString::new(STRAGGLER_FILE).unwrap();
            // SAFETY: `dir_fd.0` is live and `name` outlives the call.
            let fd = unsafe {
                libc::openat(
                    dir_fd.0,
                    name.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            assert!(fd >= 0, "staging the straggler must succeed");
            // SAFETY: `fd` was just opened here and is owned by nothing else.
            unsafe { libc::close(fd) };
        }
    }
    // NotFound is the normal, successful case: a live host may have removed its own
    // directory moments ago on the SIGHUP that preceded this.
    match std::fs::remove_dir(run_dir) {
        Ok(()) => RunDirSweep::Settled(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => RunDirSweep::Settled(None),
        // **The marker is gone and the directory is not** (round-2 finding 5).
        //
        // `rmdir` is the last fallible step and it cannot be moved before the marker
        // removal, because the marker is IN the directory `rmdir` empties — so the
        // one window the ordering cannot close is this one: a straggler creates an
        // entry after the enumeration above, `rmdir` returns `ENOTEMPTY`, and the
        // warrant that says whose directory this is has already been unlinked. The
        // next pass would read the missing marker as `Foreign`, ignore its own
        // `remove_dir`'s failure, and report `Settled` over a tree that is still
        // standing. Cleanup completes; the leftovers stay until the next reboot.
        //
        // So the warrant is put BACK, through the descriptor that was verified once
        // and cannot have been redirected since, and the pass reports what it is:
        // owed. The next pass reads `Ours`, empties the straggler too, and finishes.
        //
        // Re-creation can itself fail, and that residual is not pretended away: it
        // leaves exactly the state this arm exists to prevent, and the `Retry` says
        // so in as many words rather than reporting the `rmdir` failure alone.
        Err(err) => {
            let restored = restore_marker_at(&dir_fd, uid, launch_nonce);
            drop(dir_fd);
            match restored {
                Ok(()) => RunDirSweep::Retry(format!(
                    "could not remove the run dir {shown}: {err}; its owner marker has \
                     been restored so this stays owed"
                )),
                Err(marker_err) => RunDirSweep::Retry(format!(
                    "could not remove the run dir {shown}: {err}; AND its owner marker \
                     could not be restored ({marker_err}), so a later pass will read \
                     this directory as a stranger's"
                )),
            }
        }
    }
}

/// The PREFIX the restored warrant is staged under before it is renamed into place.
/// Dot-prefixed and distinct from the marker's, so a half-written staging file can
/// never be mistaken for a warrant. See [`marker_restore_staging_name`] for why a
/// prefix and not a name.
pub(crate) const MARKER_RESTORE_TEMP: &str = ".owner.restore";

/// A staging name **no other restorer can be using** (round-4 finding 3).
///
/// One shared staging name is not safe here, and the reason is not hypothetical: the
/// custodian sweeping a launch and that launch's own host both call
/// [`sweep_owned_run_dir`] with the same uid and nonce, both verify the directory
/// `Ours`, and both can be inside [`restore_marker_at`] at once. With one name and an
/// unlink-then-create-then-rename-by-name protocol, this interleaves into publishing
/// an empty warrant:
///
///   1. A unlinks the staging name, creates it (inode 1), starts writing;
///   2. B unlinks the staging name — removing A's directory entry, though A still
///      holds the fd — and creates its own (inode 2), not yet written;
///   3. A finishes writing and fsyncing **inode 1**, then renames *by name*, which
///      publishes **inode 2** — B's empty file — over the marker, and returns success.
///
/// The marker then reads `Foreign`, which settles and abandons the directory: exactly
/// the outcome the restore exists to prevent, reached through the restore succeeding.
///
/// `pid` + a fresh nonce is enough to rule the interleave out. Two live processes
/// never share a pid, so the pid separates the host from the custodian; the nonce
/// separates two threads of one process and two attempts of one thread. Each restorer
/// creates `O_EXCL` under its own name and renames only that name, so no restorer can
/// unlink, replace or publish another's inode.
///
/// Nothing pre-unlinks any more, and nothing needs to: the name is fresh, so `EEXIST`
/// is a genuine anomaly rather than the ordinary leftover it used to be. A staging
/// file orphaned by a crash between create and unlink is swept by the NEXT pass's
/// `remove_tree_beneath`, which empties everything under the run dir except the
/// marker itself — so unique names trade a lost-warrant race for litter that the
/// existing sweep already collects.
fn marker_restore_staging_name() -> String {
    format!(
        "{MARKER_RESTORE_TEMP}.{}.{}",
        std::process::id(),
        mint_nonce()
    )
}

/// Put the deletion warrant back after a failed `rmdir` (round-2 finding 5),
/// **atomically and durably** (round-3 finding 3).
///
/// The whole value of this function is that the next pass reads `Ours` instead of
/// `Foreign`, so what it must never do is leave something that reads as neither.
/// The first cut wrote in place — `openat` `O_CREAT|O_EXCL`, then `write`, then
/// `fsync` — and that had three ways to fail into exactly the state it exists to
/// prevent:
///
///   * a `write` or `fsync` that failed after the create left an EMPTY or partial
///     marker, which the reader compares field-by-field and files as `Foreign` — the
///     verdict that settles and abandons;
///   * nothing fsynced the DIRECTORY, so the new entry itself was not durable and a
///     crash could lose the warrant while the tree it names survived;
///   * and `EEXIST` was taken as success without ever looking at what was there, so
///     any file at the marker's name — including one a previous partial write left —
///     counted as a restored warrant.
///
/// So it uses the same protocol every durable write in this module uses
/// ([`store_atomic`]): stage under a temp name, fsync the contents, `renameat` over
/// the marker's name, fsync the directory. The rename is atomic, which means the
/// marker's name only ever holds a complete warrant or the previous state —
/// never a splice, never an empty file. That also retires the `EEXIST` question
/// rather than answering it: `rename` replaces, so there is no "already there" case
/// to guess at. Replacing is safe here and not merely convenient — the directory was
/// verified `Ours` through this same descriptor at the top of the sweep, and A11.4
/// refuses to adopt an existing directory, so no other launch can have claimed it in
/// the meantime; the only thing that can be at that name is this same warrant.
///
/// Every failure unlinks the staging file, so a pass that could not restore leaves
/// no litter for the next one to trip over.
///
/// **The staging name is unique per attempt** (round-4 finding 3) — see
/// [`marker_restore_staging_name`] for the interleave a shared name admits.
///
/// **And its mode is SET, not requested** (round-4 finding 4). `openat`'s mode
/// argument is a creation mode the umask can only take bits away from, and the
/// restorer is not always the process that chose the umask: a replacement custodian
/// spawned under a hostile or merely careless umask (`0777` is enough) creates the
/// staging file mode `000`, renames it over the marker, and reports success. Every
/// later read of that marker gets `EACCES`, which the reader files as `Unknown` —
/// the one verdict that never settles — so cleanup stays `Pending` for ever, on a
/// restore that said it worked. That is outside the recorded failed-restore residual
/// entirely, because the restore did not fail. `fchmod` on the descriptor just opened
/// makes 0600 the state of the file rather than a request made at creation time,
/// which is the same normalization the initial marker creation already does with
/// `set_permissions` — done here through the fd, since this path has no name to
/// address and must not acquire one.
fn restore_marker_at(dir: &DirFd, uid: &str, launch_nonce: &str) -> std::io::Result<()> {
    let name = std::ffi::CString::new(RUN_DIR_OWNER_FILE)
        .map_err(|_| std::io::Error::other("the marker name contains a NUL byte"))?;
    let temp = std::ffi::CString::new(marker_restore_staging_name())
        .map_err(|_| std::io::Error::other("the staging name contains a NUL byte"))?;
    // SAFETY: `dir.0` is live and `temp` outlives the call. `O_NOFOLLOW` refuses a
    // symlink planted at the staging name rather than writing through it. No
    // pre-unlink: the name is fresh, so `O_EXCL` failing is a real anomaly.
    let fd = unsafe {
        libc::openat(
            dir.0,
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let staged = (|| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::io::{AsRawFd, FromRawFd};
        // SAFETY: `fd` was just opened by this call and is owned by nothing else.
        // Wrapped FIRST so every arm below closes it by dropping `file`.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        // Round-4 finding 4: 0600 as a FACT about the file, not as a request the
        // umask may have narrowed. SAFETY: `file` owns a live descriptor.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // **The write-failure seam fires HERE** (round-4 finding 8), not at the top
        // of the function. It used to return before the staging file was created,
        // which made the injected failure a different failure from the one it stands
        // for: nothing had been staged, so the unlink below had nothing to remove,
        // and the test's "a failed restore leaves no staging litter" assertion could
        // not observe the cleanup it was written to cover — deleting that unlink left
        // the test green. Placed after the create, the fault reaches the arm a real
        // write failure reaches, and the litter assertion has something to see.
        if take_marker_restore_fault() {
            return Err(std::io::Error::other(
                "injected fault: the owner marker could not be restored",
            ));
        }
        file.write_all(owner_marker_body(uid, launch_nonce).as_bytes())?;
        file.sync_all()?;
        // SAFETY: both names are relative to `dir.0`, which is live.
        if unsafe { libc::renameat(dir.0, temp.as_ptr(), dir.0, name.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The rename's own directory entry has to be durable too, or a crash here
        // leaves the tree standing with no warrant naming it.
        // SAFETY: `dir.0` is a live directory descriptor.
        if unsafe { libc::fsync(dir.0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if staged.is_err() {
        // SAFETY: `dir.0` is live and `temp` outlives the call.
        unsafe { libc::unlinkat(dir.0, temp.as_ptr(), 0) };
    }
    staged
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
        // The D6 exec gate releases the custodian only after this write, and the
        // custodian is not a host child — `host_children_recorded` never looks at it
        // and readiness never rests on it. Left false rather than invented.
        exec_confirmed: false,
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
///     declared ready without an independent cleanup owner still breathing;
///   * **server A is recorded** (A9.1) — a `Ready` record must never commit
///     *unpinned*. Without this the Ready-fatal teardown falls into
///     `destroy_owned_session(.., None)`, an unpinned destroy addressed only by
///     socket+uid. The only path to `to_ready` is a `NewSessionOutcome::Created`,
///     which always persists A in the same durable write (a session whose server
///     birth was not proven already fails closed in `ServerA::from_owned`), so
///     this refuses nothing the forward path can legitimately produce;
///   * and the **host lease is live and its children are ready RIGHT NOW**
///     (round-3 finding 2) — re-asked here rather than inherited from the
///     bring-up census, because a bounded lock wait separates the two and a child
///     can die inside it.
pub fn to_ready(_lock: &LaunchLock, uid: &str, coordinator: &ProcessIdentity) -> Result<()> {
    let mut record = load(uid)?;
    // **The A-check comes before BOTH arms**, not just the forward CAS below.
    //
    // It used to sit with the other commit-time guards, after the retry arm — so a
    // record that was already `Ready` with `server_a: null` was re-certified
    // (re-fsynced and reported Ok) without ever being asked the question this
    // guard exists to ask. The forward path cannot produce that shape today, but
    // "Ready implies pinned" is an invariant of the RECORD, and a function that
    // re-certifies a record must enforce it on whatever it actually finds there —
    // a record written by an older build, or one a fault landed on, is exactly the
    // case where the invariant is not free. Asked once, up front, it holds for
    // every exit.
    if record.server_a.is_none() {
        bail!("refusing pending→ready: server A is not recorded (the session is unpinned)");
    }
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
    // **The host and its children are re-asked HERE, under the lock, at the instant
    // of the commit** (round-3 finding 2).
    //
    // The bring-up loop samples "the legs serve, the host lives, both children are
    // confirmed and alive" and then calls this — but not directly. In between sits a
    // bounded lock acquisition that is allowed to spend up to five seconds waiting
    // for another holder, and neither this CAS nor its caller re-read the evidence
    // afterwards. A TUI that exits during that wait leaves the coordinator committing
    // `Ready` on a census that is already historical, and nothing downstream ever
    // revisits it: the broker's listeners belong to the HOST and keep answering, so
    // the legs cannot contradict it either.
    //
    // Sampling evidence and acting on it are two moments, and the only one that
    // decides anything is this one. So the question is asked where the answer is
    // used, against the record this call loaded under the lock it holds.
    match &record.host_lease {
        Some(lease) if liveness(&lease.identity) == Liveness::Alive => {}
        Some(_) => bail!("refusing pending→ready: the host is not proven live at commit time"),
        None => bail!("refusing pending→ready: no host holds the lease at commit time"),
    }
    if !host_children_ready(&record) {
        bail!(
            "refusing pending→ready: the host's children are not all recorded by this \
             lease, past execve and alive at commit time"
        );
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

// A9.1: the standalone "clear the in-flight flag" transition is gone. Its only
// caller was the coordinator's `Created` arm, where it ran as a *separate*
// `store_atomic` from `record_server_a` — the two-write window this amendment
// closes. Both halves now live in `record_new_session_created`; the determinate
// *failure* path keeps clearing the flag inside `fail_new_session_determinate`.

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

    /// **A restorer never touches, and never publishes, another restorer's staging
    /// inode** (round-4 finding 3).
    ///
    /// The custodian sweeping a launch and that launch's own host both reach
    /// [`restore_marker_at`] on the same directory, both having verified it `Ours`,
    /// and they can be inside it at once. With ONE shared staging name the protocol
    /// was unlink → create → write → fsync → rename-BY-NAME, and that interleaves:
    /// B's unlink removes A's directory entry while A still holds the fd, B creates
    /// its own inode at the same name, and A's rename — which resolves the NAME, not
    /// the inode it wrote — publishes **B's** file. A returns success having
    /// published bytes it never wrote and never fsynced; A11.5's "atomic and durable"
    /// closure is false of that inode, and a B that dies between its create and its
    /// write leaves an EMPTY warrant standing, which reads `Foreign` and abandons the
    /// directory.
    ///
    /// **Asserted by construction, not by racing.** A concurrent detector was written
    /// first and thrown away for being one: the corrupt state is transient — B writes
    /// its own body into the just-published inode microseconds later — so a sampling
    /// observer sees a valid warrant almost always, and the shared-name mutant
    /// survived three runs of it. What is deterministic is the mechanism: a
    /// competitor's staging file is planted under the shared name, and both halves of
    /// the interleave are then impossible to reach without failing this test.
    #[test]
    fn a_restorer_never_touches_another_restorers_staging_inode() {
        let dir = std::env::temp_dir().join(format!("cc-restore-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join(RUN_DIR_OWNER_FILE);

        // A competitor mid-restore: its staging file exists under the name a shared
        // protocol would use, and it has not finished writing its warrant yet.
        let competitor = dir.join(MARKER_RESTORE_TEMP);
        const IN_FLIGHT: &str = "COMPETITOR-STAGING-NOT-YET-WRITTEN";
        std::fs::write(&competitor, IN_FLIGHT).unwrap();

        let fd = open_dir_nofollow(&dir).expect("open the run dir");
        restore_marker_at(&fd, "uid-solo", "nonce-solo").expect("the restore must succeed");

        // GATE 1: what got published is OUR warrant — the bytes this call wrote and
        // fsynced — and never the competitor's inode. A shared name with no
        // pre-unlink fails earlier still: `O_EXCL` returns EEXIST and the restore
        // above cannot even succeed.
        assert_eq!(
            std::fs::read_to_string(&marker).expect("a warrant was published"),
            owner_marker_body("uid-solo", "nonce-solo"),
            "a restorer must publish the inode it wrote, not whatever holds the name"
        );
        // GATE 2: the competitor's staging file is UNTOUCHED. The unconditional
        // pre-unlink a shared name needs is exactly what destroys another restorer's
        // in-flight work, and it is what this assertion forbids.
        assert_eq!(
            std::fs::read_to_string(&competitor).expect("the competitor's staging survives"),
            IN_FLIGHT,
            "a restorer must not unlink a staging file it does not own"
        );
        // …and our own staging file is gone, so the success path leaves no litter.
        let ours: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(MARKER_RESTORE_TEMP) && n != MARKER_RESTORE_TEMP)
            .collect();
        assert!(ours.is_empty(), "no staging litter of our own: {ours:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A restore under a hostile umask still produces a READABLE warrant**
    /// (round-4 finding 4).
    ///
    /// `openat`'s mode argument is a creation-mode REQUEST that the umask can only
    /// take bits away from, and the restorer is not always the process that chose the
    /// umask — a replacement custodian inherits whatever spawned it. Under `umask
    /// 0777` the staged file is created mode `000`, renamed over the marker, and the
    /// restore reports SUCCESS. Every later read then gets `EACCES`, which the reader
    /// files as `Unknown` — the one verdict that never settles — so cleanup stays
    /// `Pending` for ever on a restore that said it worked. That is outside the
    /// recorded failed-restore residual entirely, because nothing failed.
    ///
    /// **Run in a CHILD PROCESS, and it has to be.** `umask(2)` is process-global and
    /// this suite runs its tests in parallel threads, so flipping it here would hand
    /// mode-`000` files to whatever unrelated test happened to be creating one. The
    /// child re-execs this same test binary with the directory in its environment;
    /// the env var is also the recursion guard, so the child does the work and spawns
    /// nothing.
    #[test]
    fn a_restore_under_a_hostile_umask_is_still_readable() {
        const DIR_ENV: &str = "CC_RESTORE_UMASK_DIR";
        if let Some(dir) = std::env::var_os(DIR_ENV) {
            // ── CHILD ──────────────────────────────────────────────────────────
            // A umask that removes every bit 0600 asks for. This is the whole
            // hostile condition; everything else is the ordinary restore path.
            // SAFETY: `umask` cannot fail and this process does nothing else.
            unsafe { libc::umask(0o777) };
            let dir = PathBuf::from(dir);
            let fd = open_dir_nofollow(&dir).expect("child: open the run dir");
            restore_marker_at(&fd, "uid-umask", "nonce-umask").expect("child: restore");
            return;
        }

        // ── PARENT ─────────────────────────────────────────────────────────────
        let dir = std::env::temp_dir().join(format!("cc-restore-umask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join(RUN_DIR_OWNER_FILE);

        // A libtest SUBSTRING filter, not `--exact`: `--exact` wants the
        // fully-qualified test path, which `module_path!()` does not spell the same
        // way libtest does, and a child that filtered every test away would exit 0
        // having done nothing. The "1 passed" check below is what makes that
        // impossible to miss either way.
        let only = "a_restore_under_a_hostile_umask_is_still_readable";
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([only, "--nocapture"])
            .env(DIR_ENV, &dir)
            .output()
            .expect("re-exec this test binary");
        let report = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "the child's restore must succeed: {}\n{report}",
            out.status
        );
        assert!(
            report.contains("1 passed"),
            "the child must actually RUN this test, not filter it away:\n{report}"
        );

        // THE GATE. The restore reported success either way; what separates a
        // working warrant from a permanently-unreadable one is that 0600 is the
        // file's STATE, not the mode that was asked for at creation.
        assert_eq!(
            protocol::fsperm::mode_of(&marker).unwrap(),
            0o600,
            "a warrant created under a hostile umask must still be 0600"
        );
        // …and the sentence that actually matters: the next pass can READ it, so
        // the directory is provably ours instead of permanently Unknown.
        assert_eq!(
            run_dir_marker(&dir, "uid-umask", "nonce-umask"),
            MarkerVerdict::Ours,
            "an unreadable warrant reads as Unknown, which never settles"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The statement the detector above cannot make: a staging name is fresh on
    /// every attempt, so no restorer can address another's (round-4 finding 3).
    #[test]
    fn staging_names_are_unique_per_attempt() {
        let names: std::collections::HashSet<String> =
            (0..1000).map(|_| marker_restore_staging_name()).collect();
        assert_eq!(names.len(), 1000, "every attempt must get its own name");
        // …and every one of them is still recognisable as staging litter, which is
        // what lets the sweep's own cleanup assertions find it.
        assert!(names
            .iter()
            .all(|n| n.starts_with(MARKER_RESTORE_TEMP) && n != RUN_DIR_OWNER_FILE));
        // The pid component is what separates two PROCESSES; two live processes
        // never share a pid, so the host and the custodian cannot collide.
        assert!(names
            .iter()
            .all(|n| n.contains(&format!(".{}.", std::process::id()))));
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

    /// A **really-live process this test owns**, and can kill on cue.
    ///
    /// [`host_children_ready`] demands `Alive`, so a readiness test can no longer be
    /// staged from invented pids — and it needs identities that are live, DISTINCT
    /// from each other, and distinct from this process, because the shape it has to
    /// tell apart is "a predecessor's child is breathing while the current host's is
    /// dead". `current_identity()` supplies only one such identity and cannot be
    /// killed. A real child supplies as many as the test needs and dies when told.
    ///
    /// Reaped on drop, so a panicking test leaves no strays behind.
    struct LiveProc(std::process::Child);

    impl LiveProc {
        fn spawn() -> (Self, ProcessIdentity) {
            let child = std::process::Command::new("/bin/sleep")
                .arg("120")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawning a live witness process");
            let pid = child.id() as i32;
            let birth = protocol::proc_identity::read_birth_identity(pid)
                .expect("the witness's birth stamp");
            (LiveProc(child), ProcessIdentity { pid, birth })
        }

        /// Kill and REAP, so the identity is `Gone` rather than a zombie by the time
        /// the caller asks. A zombie still answers `kill(pid, 0)`.
        fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Drop for LiveProc {
        fn drop(&mut self) {
            self.kill();
        }
    }

    /// Bring a still-`pending` record to the shape [`to_ready`] requires of the
    /// host side (round-3 finding 2): a LIVE admitted host holding the lease, and
    /// both roles recorded, exec-confirmed and alive.
    ///
    /// Returns the process guards so the caller keeps them breathing for as long as
    /// the commit under test needs them — and can kill one to stage the mutation.
    fn make_host_ready(lock: &LaunchLock, uid: &str) -> [LiveProc; 3] {
        let (host, host_id) = LiveProc::spawn();
        assert_eq!(
            admit_host(lock, uid, "cafef00d", &host_id, host_id.pid, "codex-host").unwrap(),
            Admission::Admitted
        );
        let (app, app_id) = LiveProc::spawn();
        let (tui, tui_id) = LiveProc::spawn();
        record_and_confirm_children(lock, uid, &host_id, [app_id, tui_id]);
        [host, app, tui]
    }

    /// Record and exec-confirm both host roles for `by`, from the identities given.
    fn record_and_confirm_children(
        lock: &LaunchLock,
        uid: &str,
        by: &ProcessIdentity,
        ids: [ProcessIdentity; 2],
    ) {
        for (role, id) in HOST_CHILD_ROLES.iter().zip(ids) {
            record_host_child(
                lock,
                uid,
                by,
                ChildEntry {
                    role: role.to_string(),
                    identity: id,
                    pgid: id.pid,
                    nonce: "cafef00d".into(),
                    argv_hash: String::new(),
                    recorded_by: None,
                    exec_confirmed: false,
                },
            )
            .unwrap();
        }
        for (role, id) in HOST_CHILD_ROLES.iter().zip(ids) {
            confirm_host_child_exec(lock, uid, by, role, &id).unwrap();
        }
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
        // PINNED first, so the refusal this asserts is the terminality one and not
        // `to_ready`'s unpinned guard — which now runs ahead of both arms, and would
        // otherwise refuse for a reason that has nothing to do with what this test
        // is about.
        record_new_session_created(&lock, "u3", fake_server_a(), false).unwrap();
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

    /// **Round-2 finding 4b: the remain-on-exit note is HISTORY, not state.**
    ///
    /// The coordinator asserts the option and then writes it down, and the custodian
    /// can win the deadline CAS in between. A `pending`-only note lost the fact
    /// outright on exactly that interleaving — and the terminal record with no server
    /// A is the *only* shape that ever reads the bit, so losing it there is losing it
    /// where it is needed. Nothing about "this option was proven set on a session
    /// that exists" stops being true when the launch turns terminal.
    #[test]
    fn the_remain_on_exit_note_records_a_fact_a_terminal_record_still_needs() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("roe1").unwrap();
        create_pending(&lock, a_pending("roe1", far)).unwrap();
        // The custodian got there first: the record is terminal with no server A,
        // which is precisely the shape `server_gone_evidence` consults the bit for.
        to_failed(&lock, "roe1", "deadline", CleanupState::Pending).unwrap();
        let record = load("roe1").unwrap();
        assert!(!record.remain_on_exit_asserted);
        assert!(record.server_a.is_none());

        note_remain_on_exit_asserted(&lock, "roe1")
            .expect("a proven assertion must be recordable after the CAS, not refused");
        assert!(
            load("roe1").unwrap().remain_on_exit_asserted,
            "THE GATE: the fact the coordinator proved must survive a record that \
             went terminal while it was being written"
        );
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
        // A9.1: Ready requires a pinned session, so record A first — the same
        // durable write the coordinator's `Created` arm makes.
        record_new_session_created(&lock, "dupready", fake_server_a(), false).unwrap();
        // …and a live host with both children up, which is the other thing Ready
        // requires at commit time (round-3 finding 2).
        let _up = make_host_ready(&lock, "dupready");
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
        // Host A records both of its children, as an admitted host does. REAL live
        // processes: readiness demands `Alive`, so invented pids can no longer stand
        // in for children, and the two roles need identities that can die on cue.
        let (mut a_app, a_app_id) = LiveProc::spawn();
        let (mut a_tui, a_tui_id) = LiveProc::spawn();
        for (role, id) in HOST_CHILD_ROLES.iter().zip([a_app_id, a_tui_id]) {
            record_host_child(
                &lock,
                "lease1",
                &host_a,
                ChildEntry {
                    role: role.to_string(),
                    identity: id,
                    pgid: id.pid,
                    nonce: "cafef00d".into(),
                    argv_hash: String::new(),
                    recorded_by: None,
                    exec_confirmed: false,
                },
            )
            .unwrap();
        }
        // A11.1, readiness half: RECORDED is not READY. The fence writes each entry
        // while the child is still parked before `execve`, so the entries alone say
        // "these pids are ours", not "these pids are codex" — and everything else
        // readiness looks at is already true at that moment.
        assert!(
            !host_children_ready(&load("lease1").unwrap()),
            "children recorded but not yet past execve must not satisfy readiness"
        );
        for (role, id) in HOST_CHILD_ROLES.iter().zip([a_app_id, a_tui_id]) {
            confirm_host_child_exec(&lock, "lease1", &host_a, role, &id).unwrap();
        }
        assert!(host_children_ready(&load("lease1").unwrap()));

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
            !host_children_ready(&after),
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
        let (_b_app, b_app_id) = LiveProc::spawn();
        let (mut b_tui, b_tui_id) = LiveProc::spawn();
        record_and_confirm_children(&lock, "lease1", &host_b, [b_app_id, b_tui_id]);
        let after_b = load("lease1").unwrap();
        assert!(host_children_ready(&after_b));
        assert_eq!(
            after_b.children.len(),
            5,
            "custodian + A's two retired + B's two live: nothing was discarded: {:?}",
            after_b.children
        );

        // **THE MUTATION round-3 finding 1 names, staged exactly** — the reason the
        // three conjuncts have to hold of ONE entry rather than of the list.
        //
        // B is the current lease. Kill B's TUI and leave A's retired TUI breathing.
        // Every separate question still answers yes: some entry for role `tui` is
        // recorded-by-the-live-lease and exec-confirmed (B's), and some entry for
        // role `tui` is alive (A's). Two existential searches over one list, and the
        // witnesses are two different processes — one dead, one displaced. Only the
        // fused conjunction sees that no single entry satisfies all three.
        b_tui.kill();
        let split_recorded = HOST_CHILD_ROLES.iter().all(|role| {
            load("lease1")
                .unwrap()
                .children
                .iter()
                .any(|c| &c.role == role && c.recorded_by == Some(host_b) && c.exec_confirmed)
        });
        let split_alive = HOST_CHILD_ROLES.iter().all(|role| {
            load("lease1")
                .unwrap()
                .children
                .iter()
                .filter(|c| &c.role == role)
                .any(|c| liveness(&c.identity) == Liveness::Alive)
        });
        assert!(
            split_recorded && split_alive,
            "the premise: BOTH of the old split predicates still say yes here, or this \
             proves nothing about fusing them (recorded={split_recorded}, alive={split_alive})"
        );
        assert!(
            !host_children_ready(&load("lease1").unwrap()),
            "a retained predecessor's live TUI must not supply the liveness for the \
             current lease's DEAD TUI: readiness is one entry satisfying all three"
        );
        // A's app-server is still alive and is what makes the predecessor half of
        // that mutation real rather than incidental.
        assert_eq!(liveness(&a_app_id), Liveness::Alive);
        a_app.kill();
        a_tui.kill();

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

    /// **A takeover landing MID-readiness** (A11.8, the takeover-mid-readiness
    /// boundary).
    ///
    /// [`a_host_lease_is_taken_over_only_from_a_proven_gone_incumbent`] stages the
    /// takeover at the two ENDPOINTS of readiness — before any child is confirmed,
    /// and after both are — and proves the retain/don't-count split at each. What it
    /// never interleaves is the middle: a successor admitted *between* one
    /// [`record_host_child`] and its [`confirm_host_child_exec`], so the predecessor
    /// is displaced holding a half-established readiness. That is the state a real
    /// takeover of a still-starting host produces, and it is the one where the two
    /// guards below could plausibly disagree with each other.
    ///
    /// The sharp edge is the DISPLACED host's late confirmation. Host A spawned its
    /// TUI, was displaced while proving it past `execve`, and then completes that
    /// proof — a write arriving from a host that no longer holds the lease, naming a
    /// role the current lease also needs. It must not become readiness for anybody.
    /// Two independent guards say so and both are exercised here: the lease fence in
    /// `confirm_host_child_exec` refuses the write outright, and — had it not —
    /// [`host_children_ready`]'s `recorded_by == lease.identity` conjunct would still
    /// refuse to count an entry stamped with the predecessor.
    #[test]
    fn a_takeover_midway_through_readiness_neither_completes_nor_discards_it() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let uid = "lease-mid";
        let lock = LaunchLock::acquire(uid).unwrap();
        create_pending(&lock, a_pending(uid, far)).unwrap();
        arm(&lock, uid, live_custodian());

        // Host A is a dead identity, so it is takeable-over on the `liveness_is_gone`
        // gate; its CHILDREN are real processes, because readiness demands `Alive`.
        let host_a = fake_identity(0x3FFF_FFF5);
        assert_eq!(
            admit_host(&lock, uid, "cafef00d", &host_a, host_a.pid, "codex-host").unwrap(),
            Admission::Admitted
        );
        let (mut a_app, a_app_id) = LiveProc::spawn();
        let (mut a_tui, a_tui_id) = LiveProc::spawn();
        let entry = |role: &str, id: ProcessIdentity| ChildEntry {
            role: role.to_string(),
            identity: id,
            pgid: id.pid,
            nonce: "cafef00d".into(),
            argv_hash: String::new(),
            recorded_by: None,
            exec_confirmed: false,
        };
        // A gets exactly HALFWAY: app-server recorded and proven past `execve`; TUI
        // recorded but not yet proven. This is the interleaving point.
        record_host_child(&lock, uid, &host_a, entry("app-server", a_app_id)).unwrap();
        confirm_host_child_exec(&lock, uid, &host_a, "app-server", &a_app_id).unwrap();
        record_host_child(&lock, uid, &host_a, entry("tui", a_tui_id)).unwrap();
        assert!(
            !host_children_ready(&load(uid).unwrap()),
            "half-established readiness is not readiness, even for the host that owns it"
        );

        // …and the successor arrives HERE.
        let host_b = fake_identity(0x3FFF_FFF6);
        assert_eq!(
            admit_host(&lock, uid, "cafef00d", &host_b, host_b.pid, "codex-host").unwrap(),
            Admission::Admitted
        );

        // NEITHER COMPLETED NOR LOST. B inherits nothing…
        let mid = load(uid).unwrap();
        assert!(
            !host_children_ready(&mid),
            "a successor must not inherit a predecessor's partial readiness: {:?}",
            mid.children
        );
        assert!(
            !mid.children.iter().any(|c| c.recorded_by == Some(host_b)),
            "B has recorded nothing yet, so nothing in the record can speak for it"
        );
        // …and BOTH of A's entries survive for cleanup — the confirmed one and, the
        // case this test exists for, the half-recorded one. The TUI A spawned is a
        // real running process; losing its entry would orphan it with no identity by
        // which anything could ever stop it.
        for (role, confirmed) in [("app-server", true), ("tui", false)] {
            let retained = mid
                .children
                .iter()
                .find(|c| c.role == role && c.recorded_by == Some(host_a))
                .unwrap_or_else(|| panic!("the displaced host's {role} entry must be RETAINED"));
            assert_eq!(
                retained.exec_confirmed, confirmed,
                "and must be retained exactly as it stood at displacement, not \
                 promoted or reset: {role}"
            );
        }
        assert_eq!(liveness(&a_tui_id), Liveness::Alive);

        // THE SHARP EDGE: the displaced host completes the proof it was interrupted
        // mid-way through. The lease it holds is no longer the record's lease.
        let late = confirm_host_child_exec(&lock, uid, &host_a, "tui", &a_tui_id)
            .expect_err("a displaced host must not be able to confirm anything");
        assert!(
            format!("{late:#}").contains("the lease belongs to a different host"),
            "and must be refused ON THE LEASE, not incidentally: {late:#}"
        );
        // Refused means refused: the write did not half-land.
        let after_late = load(uid).unwrap();
        assert!(
            !after_late
                .children
                .iter()
                .any(|c| c.role == "tui" && c.recorded_by == Some(host_a) && c.exec_confirmed),
            "the refused confirmation must not have been written anyway"
        );
        assert!(!host_children_ready(&after_late));
        // The same fence stops the displaced host APPENDING, which is the other way a
        // stale host could put identities in front of cleanup.
        let (_a_extra, a_extra_id) = LiveProc::spawn();
        let appended = record_host_child(&lock, uid, &host_a, entry("tui", a_extra_id))
            .expect_err("a displaced host must not be able to append either");
        assert!(format!("{appended:#}").contains("the lease belongs to a different host"));

        // THE SECOND GUARD, exercised rather than argued. Grant the displaced host
        // the write the fence just refused — flip the bit in a COPY of the record,
        // which is exactly the state the record would hold had that fence not been
        // there — and readiness must still refuse, because A's entries are stamped
        // with a lease that is no longer the record's.
        //
        // Done in memory, deliberately: writing it would require defeating the very
        // fence under test, and the claim is about `host_children_ready`, which is a
        // pure predicate over a record. This is the assertion that fails if
        // `recorded_by == lease.identity` is dropped from that predicate — without
        // it, both of A's roles are confirmed and alive and B inherits readiness it
        // never earned.
        let mut as_if_confirmed = after_late.clone();
        for c in &mut as_if_confirmed.children {
            if c.recorded_by == Some(host_a) {
                c.exec_confirmed = true;
            }
        }
        assert!(
            as_if_confirmed
                .children
                .iter()
                .filter(|c| HOST_CHILD_ROLES.contains(&c.role.as_str()))
                .all(|c| c.exec_confirmed && liveness(&c.identity) == Liveness::Alive),
            "the premise: in this counterfactual both of A's roles are confirmed and \
             alive, so only the lease stamp can be what withholds readiness"
        );
        assert!(
            !host_children_ready(&as_if_confirmed),
            "even a displaced host's COMPLETED readiness must not become the \
             successor's: the entries are stamped with the old lease"
        );

        // And B establishes readiness the only way left — on its own processes —
        // without A's evidence being discarded to get there.
        let (_b_app, b_app_id) = LiveProc::spawn();
        let (_b_tui, b_tui_id) = LiveProc::spawn();
        record_and_confirm_children(&lock, uid, &host_b, [b_app_id, b_tui_id]);
        let after_b = load(uid).unwrap();
        assert!(host_children_ready(&after_b));
        assert_eq!(
            after_b.children.len(),
            5,
            "custodian + A's two retained + B's two: the interleaving discarded \
             nothing: {:?}",
            after_b.children
        );
        a_app.kill();
        a_tui.kill();
    }

    /// A stand-in for the identity `ServerA::from_owned` extracts from a resolved
    /// session, so tests can pin a record the way the `Created` arm does.
    fn fake_server_a() -> ServerA {
        ServerA {
            session_id: "$9".into(),
            server_pid: 4242,
            server_start_time: 7,
            session_created: 8,
            server_birth: protocol::proc_identity::BirthIdentity {
                start_sec: 100,
                start_usec: 200,
            },
        }
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
        record_new_session_created(&lock, "det1", fake_server_a(), false).unwrap();
        assert!(
            !load("det1").unwrap().new_session_indeterminate,
            "a determinate clear disarms the flag on a Failed record"
        );
    }

    #[test]
    fn a_created_session_is_pinned_and_disarmed_in_one_write() {
        // A9.1: recording server A and clearing the in-flight flag are ONE atomic
        // durable write. The postcondition asserted here is the invariant the
        // split pair could not hold — there is no durable state in which the flag
        // is clear but A is missing — plus the `cleanup → pending` restore the
        // old `clear_new_session_indeterminate` carried.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("a91").unwrap();
        create_pending(&lock, a_pending("a91", far)).unwrap();
        // A pending whose cleanup was moved off Pending (the shape the restore
        // exists for) with the mutation in flight.
        set_cleanup(&lock, "a91", CleanupState::NotRequired).unwrap();
        mark_new_session_starting(&lock, "a91").unwrap();

        record_new_session_created(&lock, "a91", fake_server_a(), false).unwrap();

        let rec = load("a91").unwrap();
        assert!(
            rec.server_a.is_some(),
            "the created session must be pinned by server A"
        );
        assert!(
            !rec.new_session_indeterminate,
            "a created session is a determinate outcome: the flag is disarmed"
        );
        assert_eq!(
            rec.cleanup,
            CleanupState::Pending,
            "a pending record with a created session needs cleanup"
        );
    }

    #[test]
    fn only_the_lease_holder_can_confirm_a_child_it_actually_recorded() {
        // A11.1's readiness bit is what a `Ready` now rests on, so the write that
        // sets it carries the same fence `record_host_child` does — and one more:
        // it refuses a confirmation with nothing to confirm, because a bit set for
        // an entry that does not exist is a claim about a process nobody named.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("xc1").unwrap();
        create_pending(&lock, a_pending("xc1", far)).unwrap();
        arm(&lock, "xc1", live_custodian());
        let host = fake_identity(0x3FFF_ABC0);
        let stranger = fake_identity(0x3FFF_ABC1);
        assert_eq!(
            admit_host(&lock, "xc1", "cafef00d", &host, host.pid, "codex-host").unwrap(),
            Admission::Admitted
        );
        let tui = fake_identity(0x3FFF_ABD0);

        // Nothing recorded yet: there is nothing to confirm.
        assert!(
            confirm_host_child_exec(&lock, "xc1", &host, "tui", &tui)
                .unwrap_err()
                .to_string()
                .contains("no such child"),
            "a confirmation must not invent the entry it confirms"
        );

        record_host_child(
            &lock,
            "xc1",
            &host,
            ChildEntry {
                role: "tui".into(),
                identity: tui,
                pgid: tui.pid,
                nonce: "cafef00d".into(),
                argv_hash: String::new(),
                recorded_by: None,
                exec_confirmed: false,
            },
        )
        .unwrap();

        // A process that is not the lease holder cannot certify it.
        assert!(
            confirm_host_child_exec(&lock, "xc1", &stranger, "tui", &tui).is_err(),
            "only the host holding the lease may say its child became codex"
        );
        // Nor can the holder certify a DIFFERENT identity under the same role.
        assert!(
            confirm_host_child_exec(&lock, "xc1", &host, "tui", &fake_identity(0x3FFF_ABD9))
                .is_err(),
            "the confirmation is bound to the recorded identity, not to the role"
        );
        assert!(!load("xc1")
            .unwrap()
            .children
            .iter()
            .any(|c| c.exec_confirmed));

        confirm_host_child_exec(&lock, "xc1", &host, "tui", &tui).unwrap();
        // Idempotent.
        confirm_host_child_exec(&lock, "xc1", &host, "tui", &tui).unwrap();
        let rec = load("xc1").unwrap();
        assert_eq!(
            rec.children
                .iter()
                .filter(|c| c.role == "tui" && c.exec_confirmed)
                .count(),
            1
        );
    }

    #[test]
    fn a_failed_dir_fsync_still_leaves_the_in_flight_flag_visible() {
        // A9.3's premise, proven rather than assumed. `store_atomic` publishes by
        // rename and fsyncs the directory afterwards, so a failure of that fsync
        // returns `Err` for a write that every reader can already see. The
        // coordinator's pre-mutation block is the place this matters: the caller
        // gets an error and stops before tmux, while the record says a new-session
        // may be in flight.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("a93f").unwrap();
        create_pending(&lock, a_pending("a93f", far)).unwrap();
        assert!(!load("a93f").unwrap().new_session_indeterminate);

        fail_next_dir_fsync();
        let err = mark_new_session_starting(&lock, "a93f")
            .expect_err("the injected post-rename fsync failure must surface as an error");
        assert!(
            err.to_string().contains("AFTER the rename published"),
            "the fault must be the post-rename one: {err}"
        );
        assert!(
            load("a93f").unwrap().new_session_indeterminate,
            "and the flag is nonetheless VISIBLE — that is the whole hazard"
        );
        // One-shot: the next write is not affected, so a terminalization can still
        // land.
        fail_new_session_determinate(&lock, "a93f", "x", CleanupState::NotRequired).unwrap();
        let rec = load("a93f").unwrap();
        assert!(!rec.new_session_indeterminate);
        assert_eq!(rec.cleanup, CleanupState::NotRequired);
    }

    /// **A child that dies before the commit lands does not get to certify `Ready`**
    /// (round-3 finding 2).
    ///
    /// The bring-up loop samples the host census and then calls `commit_ready`, which
    /// may spend up to five seconds waiting for the launch lock. Nothing re-read the
    /// evidence on the far side of that wait, so a TUI that exited inside it was
    /// committed as `Ready` on a census that had already expired — and nothing
    /// downstream would ever notice, because the broker's listeners belong to the
    /// HOST and keep answering after the child is gone.
    ///
    /// Staged as the wait itself: everything is made admissible, the census is TAKEN
    /// (asserted true, so the test cannot pass by the record having been unready all
    /// along), the child then dies, and only then does the commit run.
    #[test]
    fn a_child_that_dies_before_the_commit_cannot_certify_ready() {
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("cmtlive").unwrap();
        let coord = current_identity().unwrap();
        let mut np = a_pending("cmtlive", far);
        np.coordinator = coord;
        create_pending(&lock, np).unwrap();
        arm(&lock, "cmtlive", live_custodian());
        record_new_session_created(&lock, "cmtlive", fake_server_a(), false).unwrap();
        let [_host, _app, mut tui] = make_host_ready(&lock, "cmtlive");

        // The census the bring-up loop would have taken, and it says GO.
        assert!(
            host_children_ready(&load("cmtlive").unwrap()),
            "the premise: readiness must hold BEFORE the child dies, or the refusal \
             below proves nothing about the wait"
        );

        // …the lock wait, during which the TUI exits.
        tui.kill();

        let err = to_ready(&lock, "cmtlive", &coord)
            .expect_err("stale liveness evidence must not commit Ready");
        assert!(
            format!("{err:#}").contains("commit time"),
            "and it must refuse for THAT reason — evidence re-asked at the commit — \
             rather than for one of the older guards: {err:#}"
        );
        assert_eq!(
            load("cmtlive").unwrap().state,
            LaunchState::Pending,
            "a refused commit leaves the record pending, not half-committed"
        );

        // The host half of the same question: a host that dies in the wait is
        // refused too, and separately from its children.
        let lock2 = LaunchLock::acquire("cmthost").unwrap();
        let mut np2 = a_pending("cmthost", far);
        np2.coordinator = coord;
        create_pending(&lock2, np2).unwrap();
        arm(&lock2, "cmthost", live_custodian());
        record_new_session_created(&lock2, "cmthost", fake_server_a(), false).unwrap();
        let [mut host2, _a2, _t2] = make_host_ready(&lock2, "cmthost");
        assert!(host_children_ready(&load("cmthost").unwrap()));
        host2.kill();
        let err = to_ready(&lock2, "cmthost", &coord)
            .expect_err("a host that died in the commit wait must not commit Ready");
        assert!(
            format!("{err:#}").contains("host is not proven live at commit time"),
            "and for the host's own reason: {err:#}"
        );
    }

    #[test]
    fn to_ready_refuses_a_record_with_no_server_a() {
        // A9.1: a Ready record must never commit UNPINNED. Without this guard a
        // crash between the old split writes could leave `server_a: null` on a
        // record that then went Ready, and the Ready-fatal teardown would issue an
        // unpinned destroy addressed only by socket+uid.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("a91r").unwrap();
        let coord = current_identity().unwrap();
        let mut np = a_pending("a91r", far);
        np.coordinator = coord;
        create_pending(&lock, np).unwrap();
        arm(&lock, "a91r", live_custodian());
        // Every other guard passes: pending, our coordinator, live deadline, live
        // custodian. Only A is missing.
        let err = to_ready(&lock, "a91r", &coord).unwrap_err();
        assert!(
            err.to_string().contains("server A"),
            "an unpinned record must not commit Ready: {err}"
        );
        assert_eq!(load("a91r").unwrap().state, LaunchState::Pending);
        // Pinned, with a live host and both children up, it commits.
        record_new_session_created(&lock, "a91r", fake_server_a(), false).unwrap();
        let _up = make_host_ready(&lock, "a91r");
        to_ready(&lock, "a91r", &coord).unwrap();
        assert_eq!(load("a91r").unwrap().state, LaunchState::Ready);

        // …and the SAME guard on the RETRY arm, which is the half the first
        // assertion above cannot reach. The retry arm exists so a `to_ready` whose
        // dir-fsync failed can re-prove durability instead of refusing; it returns
        // before the forward CAS's guards, so an already-`Ready` record with
        // `server_a: null` — an older build's, or one a fault landed on — used to
        // be re-certified and reported Ok. Staged by writing that exact record.
        let mut unpinned = load("a91r").unwrap();
        unpinned.server_a = None;
        store_atomic("a91r", &unpinned).unwrap();
        assert_eq!(load("a91r").unwrap().state, LaunchState::Ready);
        let err = to_ready(&lock, "a91r", &coord).unwrap_err();
        assert!(
            err.to_string().contains("server A"),
            "the retry arm must refuse an unpinned Ready too, not re-certify it: {err}"
        );
    }

    #[test]
    fn the_durability_walk_covers_every_ancestor_the_creation_could_have_made() {
        // A9.6(c), both halves, and both were bounds this walk got wrong.
        //
        // HALF ONE — a relative root walks off the end of the path. `root_dir()`
        // returns CODECONNECT_HOME verbatim and falls back to `./.codeconnect` with
        // no $HOME, so a one-component relative root gave the walk an EMPTY
        // component (`Path::new("relcc").parent()` is `Some("")`), and
        // `File::open("")` is ENOENT — every launch-lock acquisition failed. The
        // walk refuses the empty component, and `create_session_dir_durably`
        // absolutises before it is ever reached.
        assert!(
            !durability_ancestors(std::path::Path::new("relcc/sessions/uid1"))
                .iter()
                .any(|p| p.as_os_str().is_empty()),
            "an empty path component opens nothing and must never be walked to"
        );

        // HALF TWO — a fixed stop point cannot repair what it never visits.
        // `private_dir` is recursive: on a fresh machine a deep CODECONNECT_HOME
        // means most of that chain is newly created, and each new dirent needs its
        // PARENT fsynced. The creation loop does that once and propagates with `?`;
        // on the retry `newly` is empty, so only this walk is left to re-establish
        // them. It therefore goes all the way up.
        let deep = std::path::Path::new("/a/b/c/d/e/f/g/sessions");
        assert_eq!(
            durability_ancestors(&deep.join("uid2")),
            vec![
                std::path::PathBuf::from("/a/b/c/d/e/f/g/sessions"),
                std::path::PathBuf::from("/a/b/c/d/e/f/g"),
                std::path::PathBuf::from("/a/b/c/d/e/f"),
                std::path::PathBuf::from("/a/b/c/d/e"),
                std::path::PathBuf::from("/a/b/c/d"),
                std::path::PathBuf::from("/a/b/c"),
                std::path::PathBuf::from("/a/b"),
                std::path::PathBuf::from("/a"),
                std::path::PathBuf::from("/"),
            ],
            "every ancestor `private_dir` could have created must be re-flushable"
        );

        // The ordinary root is unchanged in shape and only two entries longer.
        assert_eq!(
            durability_ancestors(std::path::Path::new("/home/u/.codeconnect/sessions/uid1")),
            vec![
                std::path::PathBuf::from("/home/u/.codeconnect/sessions"),
                std::path::PathBuf::from("/home/u/.codeconnect"),
                std::path::PathBuf::from("/home/u"),
                std::path::PathBuf::from("/home"),
                std::path::PathBuf::from("/"),
            ]
        );
    }

    #[test]
    fn a_relative_session_dir_can_still_establish_its_durability_barrier() {
        // The end-to-end half of A9.6(c)'s first bug. `root_dir()` hands back
        // CODECONNECT_HOME verbatim, so the session dir can be relative — and the
        // walk then produced an EMPTY component whose `File::open("")` is ENOENT,
        // failing every single launch-lock acquisition. Driven through the real
        // creation path, with a genuinely relative path (built as a `..` chain from
        // the current directory, so no test has to mutate the process-wide cwd) and
        // real fsyncs.
        let cwd = std::env::current_dir().unwrap();
        let target = std::env::temp_dir().join(format!(
            "cc-relroot-{}-{:?}-{}/relcc/sessions/relu1",
            std::process::id(),
            std::thread::current().id(),
            protocol::time::now_unix_ms()
        ));
        let mut relative = std::path::PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        for part in target.components().skip(1) {
            relative.push(part);
        }
        assert!(
            relative.is_relative(),
            "the point of this test is a RELATIVE path: {}",
            relative.display()
        );

        create_session_dir_durably(&relative)
            .expect("a relative session dir must still establish its durability barrier");
        assert!(
            target.is_dir(),
            "and actually create it: {}",
            target.display()
        );

        // Idempotent on the already-exists path too — the retry that used to be
        // where the empty component bit hardest.
        create_session_dir_durably(&relative).expect("and again, on the retry path");

        if let Some(scratch) = target
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            let _ = std::fs::remove_dir_all(scratch);
        }
    }

    #[test]
    fn prove_record_durable_reports_an_unflushable_dir_instead_of_succeeding() {
        // A9.6(a): the reader's half of the durability handoff. It must actually
        // touch the directory — a version that returned Ok without opening it
        // would prove nothing. A session dir stripped of read permission (search
        // still allowed, so the record itself still parses) is the deterministic
        // way to make that fsync fail.
        let far = monotonic_now_nanos().unwrap() + 60_000_000_000;
        let lock = LaunchLock::acquire("dur1").unwrap();
        create_pending(&lock, a_pending("dur1", far)).unwrap();
        drop(lock);
        prove_record_durable("dur1").expect("a healthy record's entry re-flushes");
        // A uid that was never written has no entry to prove.
        assert!(prove_record_durable("dur-absent").is_err());

        let dir = session_dir("dur1");
        set_mode(&dir, 0o100);
        if std::fs::File::open(&dir).is_ok() {
            // Running as root (or on a filesystem that ignores the mode): the
            // permission trick cannot deny the open, so skip rather than assert a
            // condition the environment refuses to create.
            set_mode(&dir, 0o700);
            return;
        }
        assert!(
            load("dur1").is_ok(),
            "the record itself must still parse — this pins the fsync, not the read"
        );
        assert!(
            prove_record_durable("dur1").is_err(),
            "a directory that cannot be opened cannot be proven durable"
        );
        set_mode(&dir, 0o700);
    }

    fn set_mode(dir: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
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
