//! The **`internal-codex-host` wrapper** (Phase 2e-2a) — the process that runs
//! inside a tmux pane and hosts ONE Codex session end to end.
//!
//! A host owns three moving parts for the lifetime of one pane:
//!
//!   1. a **`codex app-server`** child, bound to a private unix socket under an
//!      isolated `CODEX_HOME` — the real upstream;
//!   2. the in-process **broker** ([`codex_broker`]) in front of it, serving the
//!      `tui.sock`/`ccd.sock` legs and enforcing the A4 security core against the
//!      durable launch fingerprint;
//!   3. the real interactive **TUI** (`codex --remote unix://<tui.sock>`), spawned
//!      in the foreground inheriting this process's tty — this is what the user
//!      sees in the pane.
//!
//! ## Why a tokio runtime lives HERE and nowhere else
//!
//! `codeconnect` is deliberately synchronous: the shim execs away within
//! milliseconds and the supervisor/coordinator/custodian are blocking loops, so
//! an async runtime would be pure startup cost on those paths (see the Cargo
//! manifest's no-tokio note). The host is the exception — it is a **long-lived
//! async orchestrator** driving the broker's two UDS listeners and racing two
//! child processes — so it builds a tokio runtime *inside* [`run_host`] and
//! nowhere else, keeping the fast-exec paths free of it.
//!
//! ## The four structural invariants
//!
//! **1. The host OWNS its run directory.** `--run-dir` must NOT exist: the host
//! creates it itself with a single exclusive `mkdir(0700)`. That is the whole
//! basis of readiness: because the directory is provably fresh, nothing under it
//! can pre-exist, so "a 0600 socket appeared at `as.sock`" means "our app-server
//! bound it" rather than "something stale or pre-planted was already there" — and
//! the final path component cannot be a symlink someone left behind, because
//! `mkdir` does not follow one (it gets `EEXIST`).
//!
//! State the premise honestly rather than overclaiming, because 0700 is not a
//! universal exclusion: it excludes **other uids**, and the class it does not
//! exclude is THE accepted boundary of the whole Codex launch path — stated once at
//! [`crate::codex::start`] (A22) and deliberately not restated here or at any other
//! site that leans on it, so the two cannot drift into two different boundaries.
//! What the host actually relies on is: a **trusted parent path** (the host does not
//! validate the parent chain, which the coordinator owns) plus that boundary's
//! premise. Within it, freshness is enforced by the host itself, and nothing else
//! about the caller is trusted.
//!
//! 2e-2b settled what that parent chain actually is, and it is not what this
//! paragraph originally assumed ("a fresh path under the caller's own 0700 session
//! dir"). The coordinator passes a path directly under **`/tmp`**, which is
//! world-writable and sticky, because a home-rooted run dir spends the `SUN_LEN`
//! budget on the length of the user's home path
//! ([`crate::codex_coordinator::RUN_DIR_PREFIX`] carries the full reasoning). So
//! the parent chain is *not* private, and the exclusive `mkdir` below is
//! correspondingly load-bearing rather than belt-and-braces: it is what turns a
//! squatted name into a refused launch instead of an adopted directory. The
//! coordinator additionally checks, before it reads any socket under that
//! directory as bring-up evidence, that the directory is owned by this uid and is
//! 0700 — a check this module cannot make for itself, since it only ever sees the
//! directory it just created.
//!
//! **2. No fallible step runs between a spawn and the teardown guard.**
//! Everything that can fail without a child — signal handlers, the log files, the
//! event sink — is hoisted *before* the first spawn. The instant the app-server
//! exists it is moved into [`Session`], whose [`Session::teardown`] is the single
//! path every abort and every exit runs through. `.kill_on_drop(true)` is set on
//! both children as the last-resort net beneath that.
//!
//! **3. Bring-up is cancellation-aware.** SIGTERM/SIGINT/SIGHUP handlers are
//! installed before the run directory is even created — i.e. before anything exists
//! to clean up, let alone before the first spawn (SIGHUP matters: the host is
//! tmux-hosted) — and every bounded wait races them. A signal during bring-up tears
//! down whatever started and exits 130, never a half-up session left behind by a
//! closing pane.
//!
//! **4. Broker death is session-fatal, and cannot be masked.** The broker IS the
//! security boundary; a TUI must never outlive it, and never talk to a dead
//! upstream. The main race watches three things — the app-server, the broker task,
//! and the TUI — and is `biased` with the *fatal* arms first. But `biased` only
//! orders the *polling*; it does not snapshot readiness, so the broker task can
//! poll `Pending` and then finish on another worker before `tui.wait()` returns
//! `Ready` in the same pass. Ordering alone would therefore let a dead broker be
//! reported as a clean TUI exit. So the benign-looking arms **re-observe** the
//! other two parts and resolve through one pure function,
//! [`resolve_outcome`]: broker-death takes precedence over every other reading,
//! including a host signal. That function — not the arms, which are deliberately
//! thin — is the tested seam (see its unit tests).
//!
//! What "broker death" covers, exactly, and what it does not: `serve()` returning
//! `Err` (or `Ok`) ends the task, the race observes it, and the session exits
//! [`EX_HOST_FATAL`] after a **bounded best-effort** stop of both children (see
//! the teardown note below — a reap that cannot be proven is reported, not
//! assumed). A broker **panic** does not take
//! that path under this workspace's release profile, which sets
//! `panic = "abort"`: a panic aborts the whole host process immediately — no
//! unwinding, no destructors, no `kill_on_drop`, and the two codex children are
//! left running. Recovering from that is explicitly **not** this file's job; it
//! belongs to the external supervisor, the D7 launch custodian
//! ([`crate::codex_custodian`]), which is armed independently of this process and
//! is wired to the host in 2e-2b. (Under `cargo test`'s dev profile panics unwind,
//! so a panicking broker there surfaces as a `JoinError` on the race's broker arm —
//! handled, but not the release behaviour.)
//!
//! Every exit path aborts the broker, makes a bounded best-effort to reap both
//! children, and then attempts to remove the whole run directory it created.
//! "Best-effort" is literal, and it applies to all three: an aborted broker task
//! that does not finish inside [`BROKER_ABORT_BUDGET`], and a reap that cannot be
//! proven inside [`REAP_BUDGET`], are **logged and printed** rather than silently
//! assumed, with `kill_on_drop` as the net beneath the children. The run-dir
//! removal is likewise attempted, not guaranteed — a failure is reported on
//! stderr, because a broken promise with no record is how a leak becomes
//! invisible. What the host does NOT claim on any path is that the directory is
//! provably gone or that both children are provably reaped; it claims that each
//! was tried under a bound and that any shortfall was said out loud.
//!
//! The same discipline covers the host's own **exit**: the tokio runtime is shut
//! down explicitly with [`RUNTIME_SHUTDOWN_BUDGET`] rather than dropped, because an
//! implicit drop blocks until the worker threads join and would let exactly the
//! tasks that ignored their cancellation point hang the process forever — after
//! teardown had already honestly reported that it could not stop them.
//!
//! ## How this host is reached (2e-2b)
//!
//! The coordinator puts it in a tmux pane: `tmux new-session … -- <codeconnect>
//! internal-codex-host --uid … --nonce … --tmux-socket … --codex … --codex-sha256 …
//! --run-dir …`.
//! Before anything exists — no run dir, no sockets, no children — it presents
//! itself to the D7 launch gate ([`crate::codex_custodian::late_host_admission`])
//! and is admitted or refused; a refused host destroys its own uid's session and
//! exits having created nothing. `codeconnect codex` itself is still GATED and
//! refuses, so nothing but the tests reaches any of this yet.

use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use codex_broker::relay::{BoundThreadProbe, Broker, EventSink};
use codex_broker::upstream::WsUdsUpstreamFactory;
use codex_broker::LaunchFingerprint;
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::task::JoinHandle;

/// A unix-domain socket PATH must be shorter than `sun_len` (~104 bytes on
/// macOS); binding a longer path fails `path must be shorter than SUN_LEN`. Every
/// socket the host binds is asserted against this before use, so a long run dir
/// fails loudly at bring-up rather than deep inside a bind.
///
/// `pub(crate)` because the coordinator sizes the run dir it hands this host
/// against the same number ([`crate::codex_coordinator::choose_run_dir`]); two
/// copies of a kernel constant is one copy too many.
pub(crate) const SUN_LEN_LIMIT: usize = 104;

/// Bounded budget for waiting on a child's socket / the broker's listeners.
const BRINGUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded budget for reaping a child after `start_kill` on any teardown path — a
/// SIGKILL'd child reaps within a few polls; past this the host gives up rather
/// than block teardown forever, and says so.
const REAP_BUDGET: Duration = Duration::from_secs(5);

/// Bounded budget for the aborted broker task to actually finish, so teardown
/// cannot wedge on a task that ignores its cancellation point.
///
/// Note what this does and does not buy, because the obvious rationale is wrong:
/// [`Broker::serve`] binds both listeners **once, before** its accept loop, so an
/// aborted serve task cannot re-create a listener socket behind the run-dir sweep.
/// The await is about bounded, orderly teardown, not about that race. It also does
/// not reach the per-connection leg tasks: `serve` spawns each accepted connection
/// into its own task, which this handle does not own and `abort()` does not touch —
/// those are ended by process exit.
const BROKER_ABORT_BUDGET: Duration = Duration::from_secs(2);

/// Bounded budget for the tokio runtime's own shutdown, after the session is over
/// and teardown has run. Dropping a multi-thread runtime *blocks* until its workers
/// join, so without an explicit bound the tasks teardown already failed to stop
/// would get an unbounded veto over the host's exit. See [`run_host_inner`].
const RUNTIME_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

/// How much of a failed child's stderr is quoted into an error message. A child
/// can write unbounded stderr; the host surfaces only the tail, marked truncated.
const STDERR_EXCERPT_LIMIT: usize = 4096;

/// How often the thread-binding watcher asks the broker whether a thread has
/// bound yet.
///
/// Half the coordinator's `BRINGUP_POLL`, so the record has normally been written
/// by the time the coordinator's next look reads it — the point of the watcher is
/// to shorten the healthy launch's extra wait, and a poll slower than the reader's
/// would spend that saving again.
const THREAD_BINDING_POLL: Duration = Duration::from_millis(50);

/// How much of a preserved broker log is kept at each end. See
/// [`preserve_broker_log`] for the measurement that made it both ends.
///
/// 64 KiB apiece, so a bounded copy is at most 128 KiB plus one elision line. A real
/// failed launch measured ~17 KB entire, which is the case this feature exists for
/// and which the bound therefore never touches.
const PRESERVED_LOG_EDGE_BYTES: usize = 64 * 1024;

/// The filename prefix the preserved broker logs share, and the number of them kept.
///
/// The prefix must agree with [`protocol::codex_broker_log`]'s spelling; it is named
/// once here so the pruner and the writer cannot disagree about which family is
/// being bounded.
///
/// 50 files, which with the size bound above is a hard ceiling of ~6.4 MB for the
/// family — a number chosen against the measured directory (2542 files, 10 MB, none
/// of it ever pruned) so that adding a durable per-session artifact makes that
/// directory smaller-bounded rather than larger.
const PRESERVED_LOG_PREFIX: &str = "broker-";
/// See [`PRESERVED_LOG_PREFIX`].
const PRESERVED_LOG_KEEP: usize = 50;

/// The substring that marks the one broker disposition which ENDS a TUI.
///
/// Measured on codex 0.153: a `thread/start` the fingerprint refuses is answered
/// with a synthetic JSON-RPC error, and the TUI exits within milliseconds — the
/// broker log's very next line is `Tui leg ended: IO error: Broken pipe`. The
/// other two refusal dispositions (`drop, keep open`, `drop, close leg`) do not
/// produce that, so quoting one of them as the reason a session never started
/// would be a guess dressed as evidence. This is deliberately the narrow marker.
const REFUSAL_MARKER: &str = ": refuse->synthetic error (";

/// Exit code when bring-up could not be proven, or the app-server / broker died
/// while the session was up (session-fatal). `EX_SOFTWARE`.
///
/// **These codes share a namespace with the TUI's own status.** On a clean TUI
/// exit the host returns the TUI's code verbatim, so a codex that exits 70 or 130
/// is indistinguishable here from a host-detected fatal or a host signal. The
/// coordinator (2e-2b) must therefore not treat 70/130 as proof of *which* thing
/// happened — the broker log and the host's stderr line are the discriminators.
/// Malformed-charter errors also land on 70 rather than `EX_USAGE`, since the only
/// caller is the coordinator and every one of these means "this pane did not run".
const EX_HOST_FATAL: i32 = 70;

/// The lock budget for writing a spawned child's identity into the record.
///
/// Short on purpose: it is the only stretchable part of the spawn→record window,
/// during which a SIGKILLed host would leave an unrecorded live child.
///
/// **Why the D6 exec gate is not used here**, which would close the window at the
/// root by spawning inert and releasing only after the identity is durable — it
/// is the right shape and it does not fit these two children:
///
///   1. the gate hardcodes `stdin/stdout/stderr` to `/dev/null`, and the target
///      inherits that across `execve`. The TUI must inherit the **pane's tty** (it
///      is the session the user drives) and the app-server's stderr must reach the
///      held log file the bring-up error path reads back;
///   2. the gate `setpgid(0, 0)`s every child. That is wanted for the app-server
///      and is exactly what must NOT happen to the TUI, which has to stay in the
///      pane's foreground process group or lose the keyboard (measured);
///   3. the race that ends this session `wait()`s on `tokio::process::Child`
///      handles with `kill_on_drop`; the gate owns a `std::process::Child` and
///      hands back only an identity;
///   4. the gate's readiness fence is blocking, and this is an async orchestrator.
///
/// Making it fit means giving the gate stdio and process-group knobs and an async
/// handle — a change to shared D6 machinery in service of one caller.
///
/// **A11.1: closed instead by [`spawn_fenced`]**, the host-local equivalent. It
/// keeps all four facts above true — it adds only a `pre_exec` closure, so stdio and
/// process-group inheritance are untouched, the handles stay `tokio::process::Child`,
/// and the blocking part runs on a releaser thread rather than the runtime. What is
/// left for this budget to bound is how long a launch sits inert, not how long a
/// stray child can outlive its record.
const RECORD_LOCK_BUDGET: Duration = Duration::from_secs(1);

/// How long [`note_run_dir_claimed`] may wait for the launch-record lock.
///
/// Deliberately its own budget, and deliberately longer than [`RECORD_LOCK_BUDGET`].
/// That one bounds how long a launch sits inert while a *child's* identity is
/// recorded, where giving up cheaply is right because the fence refuses the child and
/// nothing is lost. This one guards a write that is now **launch-fatal**: losing the
/// lock here does not degrade the launch, it ends it. And the contention is real
/// rather than hypothetical — the coordinator and the custodian are both legitimate
/// writers of this record, and each of their stores is a file write plus an fsync
/// plus a rename plus a directory fsync, so a legitimate winner can lose a
/// one-second race under I/O pressure through no fault of its own.
///
/// Five seconds is the trade: long enough that a launch is not failed for ordinary
/// contention, short enough that a genuinely stuck lock still ends the launch well
/// inside the coordinator's own bring-up patience rather than hanging the pane.
const CLAIM_RECORD_BUDGET: Duration = Duration::from_secs(5);

/// Exit code when the D7 gate refused this host admission to its launch:
/// `EX_TEMPFAIL`, matching what `internal-codex-host-preflight` already returns
/// for the same refusal so one meaning has one number. It is not a failure of the
/// session — there is no session — it is "this launch is not mine to run".
///
/// **What is guaranteed is the no-artifact invariant, not this status.** A refusal
/// destroys the launch's own tmux session, and this host is inside its pane — so
/// the pane's hangup can reach this process before it returns, and the observed
/// status is then a signal rather than 75. Callers must therefore not treat 75 as
/// the signature of a refusal. What holds on every ordering is the thing that
/// matters: a host that was not admitted created no run directory, no sockets and
/// no children, because admission runs before any of them exist.
const EX_HOST_NOT_ADMITTED: i32 = 75;

/// Exit code when the host itself was signalled (SIGTERM/SIGINT/SIGHUP). Shares
/// the TUI's status namespace — see [`EX_HOST_FATAL`].
const EX_HOST_SIGNALLED: i32 = 130;

/// The `internal-codex-host` subcommand: build the runtime, orchestrate the
/// session, and exit with the resolved status. Machinery — never typed by a
/// human; the coordinator spawns it into the pane (2e-2b).
pub fn run_host(args: &[String]) -> ! {
    match run_host_inner(args) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("codex-host: {err:#}");
            std::process::exit(EX_HOST_FATAL)
        }
    }
}

/// Everything the host needs to bring a session up, parsed from argv.
///
/// `pub(crate)` only so [`parse_host_args`] can be, for the coordinator's
/// differential test. Every **field** stays private to this module, so the type
/// is a receipt that a charter parsed and nothing else: no other module can read
/// a value out of it.
pub(crate) struct HostArgs {
    /// The codex binary to exec for BOTH the app-server and the TUI.
    ///
    /// The host does **not** re-resolve, native-check or version-pin this path —
    /// `codex::resolve_codex_bin` / `ensure_pinned_version` run upstream in the
    /// launcher, which is what makes the reserved argv grammar's 0.147 grounding
    /// apply. Stated plainly because it is a real premise:
    /// `internal-codex-host --codex /any/path` will exec that path twice. Same-uid,
    /// so not a privilege boundary — but not a guarantee this file makes.
    ///
    /// What the host **does** guarantee is [`codex_sha256`](Self::codex_sha256):
    /// whatever path it is handed, the bytes it execs are the bytes the upstream
    /// checks were performed on, or it refuses.
    codex: PathBuf,
    /// The digest of the codex binary the upstream resolution inspected and
    /// version-pinned (A7.1). Required — never defaulted, never derived here.
    ///
    /// **Why the host cannot compute this itself.** Hashing `--codex` on arrival
    /// would pin whatever is at that path *now*, which is a statement about the
    /// host's own moment and says nothing about the file that was magic-checked and
    /// `--version`-pinned in another process some seconds earlier. That is precisely
    /// the gap A7 names, re-opened one hop further down. The digest has to travel
    /// with the path, from the process that did the inspecting.
    ///
    /// **Why a missing one is a refusal.** An absent digest would mean falling back
    /// to trusting a pathname, silently, on the one dimension that decides which
    /// code runs — the same reason no fingerprint dimension has a default here.
    /// [`crate::codex::verify_codex_identity`] is then re-run immediately before
    /// **each** of the two spawns, not once at parse time: the app-server and the
    /// TUI start at different moments, separated by the app-server's bring-up and
    /// the broker's bind, and a single early check would leave the second exec
    /// unbound.
    codex_sha256: String,
    /// The short, SUN_LEN-safe directory the host creates and owns.
    run_dir: PathBuf,
    /// The isolated `CODEX_HOME` passed to both the app-server and the TUI.
    codex_home: PathBuf,
    /// The launch this host belongs to, and the nonce proving it was invited.
    /// Both are required: they are what [`admit`] presents to the D7 gate, and a
    /// host that cannot name its launch cannot be admitted to it.
    uid: String,
    nonce: String,
    /// The tmux socket the launch lives on. Needed only on the **refusal** path,
    /// where a host that was not admitted destroys its own uid's session rather
    /// than leaving a pane running for a launch that is already over.
    tmux_socket: String,
    /// The durable launch-policy fingerprint the broker enforces.
    fingerprint: LaunchFingerprint,
    /// Passthrough args appended to the TUI invocation, after `--`. Vetted
    /// against the reserved argv grammar at parse time (see [`parse_host_args`]).
    tui_args: Vec<String>,
}

/// The five paths under the owned run dir. Derived once, before the dir exists.
struct Paths {
    as_sock: PathBuf,
    tui_sock: PathBuf,
    ccd_sock: PathBuf,
    broker_log: PathBuf,
    as_stderr: PathBuf,
}

impl Paths {
    fn under(run_dir: &Path) -> Self {
        Self {
            as_sock: run_dir.join(SOCKET_NAMES[0]),
            tui_sock: run_dir.join(SOCKET_NAMES[1]),
            ccd_sock: run_dir.join(SOCKET_NAMES[2]),
            broker_log: run_dir.join("broker.log"),
            as_stderr: run_dir.join("appserver.stderr.log"),
        }
    }
}

/// Every socket basename the host binds under its run dir, in the order
/// [`Paths`] lays them out.
///
/// It is a named constant rather than three literals because the **coordinator**
/// sizes the run dir against the longest of them
/// ([`crate::codex_coordinator::choose_run_dir`]), and that sizing is only sound
/// while this list is the whole list. With literals, adding a fourth leg with a
/// longer name would leave the coordinator's SUN_LEN guard quietly
/// under-measuring while every test stayed green; with this, the coordinator's
/// test reads the list and fails.
///
/// Log files are deliberately absent: `SUN_LEN` constrains sockets alone.
pub(crate) const SOCKET_NAMES: [&str; 3] = ["as.sock", "tui.sock", "ccd.sock"];

/// Present this process to the D7 launch gate. `Ok(None)` means admitted and the
/// caller proceeds; `Ok(Some(code))` means refused and the caller must exit with
/// that code having created nothing.
///
/// The gate itself is [`crate::codex_custodian::late_host_admission`] — the 2c
/// primitive, called rather than reimplemented, so the lease rules (exclusive,
/// taken over only from a *proven gone* incumbent) have exactly one
/// implementation. A refusal also destroys this uid's own tmux session, which is
/// what stops the refused pane from simply sitting there: the session should not
/// exist, and this host is inside it.
/// What the host must do about an admission outcome.
///
/// A separate, pure decision so the branch that must NOT exit is testable without
/// a real forever-sleep — the whole point of that branch is that it never returns.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HostAction {
    /// Admitted: bring the session up.
    Proceed,
    /// The gate answered no. Arrival is durable, so exiting is safe.
    Exit(i32),
    /// The gate could not be REACHED, so nothing durable records this pane. Stay
    /// alive and inert, keeping the session observable.
    ParkInert(String),
}

/// Map an admission outcome to the host's action.
///
/// The distinction that matters is **not** admitted-vs-refused; it is whether
/// ARRIVAL was durably recorded. A refusal is a decision made after the record
/// says this pane ran, and ending the pane then is fine — the custodian has what
/// it needs. A failure to reach the gate at all is different in kind: nothing says
/// the pane ever existed, and the pane IS the session's last observable trace when
/// creation was indeterminate, so ending it strands cleanup until reboot.
///
/// An `Err` lands with the pre-arrival case for the same reason: a gate whose
/// outcome could not be established has not told us that arrival is durable.
pub(crate) fn admission_action(
    outcome: Result<crate::codex_custodian::HostAdmission>,
) -> HostAction {
    use crate::codex_custodian::HostAdmission;
    match outcome {
        Ok(HostAdmission::Admitted) => HostAction::Proceed,
        Ok(HostAdmission::CleanupOnly { reason, cleanup }) => {
            HostAction::Exit(refused_exit_code(&reason, &format!("{cleanup:?}")))
        }
        Ok(HostAdmission::ParkInert { reason }) => HostAction::ParkInert(reason),
        Err(err) => HostAction::ParkInert(format!(
            "the admission gate could not be evaluated: {err:#}"
        )),
    }
}

/// The refusal exit code. A function so [`admission_action`] stays total and the
/// reason/cleanup are threaded to the caller's message rather than lost.
fn refused_exit_code(_reason: &str, _cleanup: &str) -> i32 {
    EX_HOST_NOT_ADMITTED
}

/// Present this process to the D7 launch gate. `Ok(None)` means admitted and the
/// caller proceeds; `Ok(Some(code))` means refused and the caller must exit with
/// that code having created nothing. It may also never return — see
/// [`HostAction::ParkInert`].
///
/// The gate itself is [`crate::codex_custodian::late_host_admission`] — the 2c
/// primitive, called rather than reimplemented, so the lease rules have exactly
/// one implementation. A refusal also destroys this uid's own tmux session, which
/// is what stops the refused pane from simply sitting there: the session should
/// not exist, and this host is inside it.
fn admit(args: &HostArgs) -> Result<Option<i32>> {
    let outcome =
        crate::codex_custodian::late_host_admission(&args.uid, &args.nonce, &args.tmux_socket);
    // Keep the detail for the operator-facing line before the outcome is reduced.
    let detail = match &outcome {
        Ok(crate::codex_custodian::HostAdmission::CleanupOnly { reason, cleanup }) => {
            Some(format!("{reason}; session cleanup: {cleanup:?}"))
        }
        _ => None,
    };
    match admission_action(outcome) {
        HostAction::Proceed => Ok(None),
        HostAction::Exit(code) => {
            eprintln!(
                "codex-host: refused admission to launch {} ({}); created nothing",
                args.uid,
                detail.as_deref().unwrap_or("no reason given")
            );
            Ok(Some(code))
        }
        HostAction::ParkInert(reason) => park_inert(args, &reason),
    }
}

/// Stay alive, inert and visible, forever.
///
/// Reached only when the gate could not be REACHED, before arrival was durable.
/// Nothing exists to clean up — no run dir, no sockets, no children — and nothing
/// will be created, so this pane is inert in every sense except one that matters:
/// it keeps the tmux session **observable**.
///
/// That is the whole point. On a launch whose `new-session` outcome was
/// indeterminate, this pane is the only evidence the session was ever created.
/// Exiting would take the session with it and leave the custodian an absence it
/// cannot distinguish from "the late session never arrived" — armed until the next
/// reboot. Parking instead lets the custodian's census find the uid **Present**,
/// persist that sighting (before it kills anything), and reach a terminal cleanup.
///
/// **Termination is the custodian's, not ours.** This loop has no deadline
/// because it must not have one: a self-imposed exit would recreate the very
/// disappearance it exists to prevent. The host dies when the session is killed.
/// The residual, stated plainly: a pane parked with no custodian left alive sits
/// there visibly until someone kills it — a wedge a human can see in
/// `tmux ls`, which is the point. A silent one is what this replaces.
fn park_inert(args: &HostArgs, reason: &str) -> ! {
    // One line, to the pane's tty — best-effort: `eprintln!` PANICS if the stderr
    // write fails, and a panic aborts under the release profile, which would make
    // this pane disappear — the exact evidence-less vanishing parking prevents.
    // The diagnostic must never be able to defeat the park.
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "codex-host: could not reach the launch gate for {} ({reason}). Nothing was \
         created. Holding this pane open so the session stays visible to cleanup — \
         it will end when the session is killed.",
        args.uid
    );
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn run_host_inner(args: &[String]) -> Result<i32> {
    let parsed = parse_host_args(args)?;

    // --- The D7 gate, run by THIS process, before anything exists ------------
    //
    // Ordering is the whole point and it is deliberately the first fallible thing
    // after argv: no run dir, no sockets, no children, and not even a tokio
    // runtime yet. A host that is not admitted must leave the world exactly as it
    // found it.
    //
    // It runs **in the host process itself** rather than in a short-lived
    // preflight child, because the lease is an identity: `admit_host` CASes THIS
    // pid+birth into the record's `host_lease`, and the custodian later reasons
    // about whether the lease holder is still alive. A preflight child's identity
    // dies with the child, so its lease would name a corpse and every later
    // liveness question about "the host" would answer wrongly.
    //
    // The case this exists for is the timed-out `new-session`: the coordinator
    // recorded the launch `failed{indeterminate}` and moved on, and then a frozen
    // tmux server finally runs the queued pane command. Admission finds a record
    // that is no longer `pending` and refuses, so that late pane never brings a
    // real Codex session up for a launch that was already declared over.
    if let Some(code) = admit(&parsed)? {
        return Ok(code);
    }

    // The one place codeconnect pays for an async runtime: the host is the
    // long-lived orchestrator, not the fast-exec shim (see the module doc).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the host tokio runtime")?;
    let outcome = runtime.block_on(orchestrate(parsed));

    // The last unbounded wait in the host, and the least obvious one: **dropping a
    // multi-thread runtime blocks** until its worker threads join, which means
    // until every task still on them reaches a yield point. That is precisely the
    // set of tasks teardown could NOT stop — a broker serve task that outlived
    // [`BROKER_ABORT_BUDGET`], and the per-connection leg tasks `Broker::serve`
    // spawns, which nothing owns and `abort()` never touched (see
    // [`BROKER_ABORT_BUDGET`]'s note). An implicit drop here would hand those tasks
    // an unbounded veto over the host's exit: teardown would report honestly that
    // it could not prove a stop, and then the process would silently hang anyway,
    // never reaching the `std::process::exit` in [`run_host`].
    //
    // `shutdown_timeout` is what makes host EXIT bounded even when tasks will not
    // die. Past the budget it stops waiting and leaks the still-running worker
    // threads, which is the correct trade here and not a leak in any lasting sense:
    // the caller exits the process immediately after, so those threads are torn
    // down with it, and the two codex children have already been SIGKILLed (or
    // reported as unproven) by teardown. Bounded and honest beats tidy and hung.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_BUDGET);
    outcome
}

// ------------------------------------------------------------------ argv

/// Parse the host's charter, fail-closed in every dimension.
///
/// `--codex`, `--codex-sha256`, `--run-dir`, `--codex-home` and **all five fingerprint
/// dimensions** (`--approval-policy`, `--approvals-reviewer`, `--sandbox`, `--hooks-enabled`,
/// `--launch-cwd`) are required: the host applies no policy default, because a default is a
/// silent disagreement waiting to happen between the coordinator's durable launch
/// record and what the broker actually enforces. Every flag takes a required,
/// non-empty, control-character-free value; `--hooks-enabled` takes exactly
/// `true` or `false`; any flag given twice is an error rather than a last-wins
/// race.
///
/// Everything after a bare `--` is passthrough for the TUI — and is validated
/// against [`crate::codex::validate_codex_argv`], the **same** 2a reserved-argv
/// grammar `codeconnect codex` enforces. The host does not trust its caller: a
/// refusal aborts before anything is created or spawned.
///
/// `pub(crate)` so the coordinator's unit tests can feed the pane argv it builds
/// straight back into this parser. That differential check is the only thing that
/// actually holds the two sides together: the coordinator writes this charter and
/// the host reads it, in different processes, and a disagreement between them
/// would otherwise surface as a pane that opens and immediately dies.
pub(crate) fn parse_host_args(args: &[String]) -> Result<HostArgs> {
    let mut codex: Option<PathBuf> = None;
    let mut codex_sha256: Option<String> = None;
    let mut uid: Option<String> = None;
    let mut nonce: Option<String> = None;
    let mut tmux_socket: Option<String> = None;
    let mut run_dir: Option<PathBuf> = None;
    let mut codex_home: Option<PathBuf> = None;
    let mut approval_policy: Option<String> = None;
    let mut approvals_reviewer: Option<String> = None;
    let mut sandbox: Option<String> = None;
    let mut hooks_enabled: Option<bool> = None;
    let mut launch_cwd: Option<String> = None;
    let mut tui_args = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        match flag {
            "--codex" => set_once(&mut codex, flag, PathBuf::from(value_of(&mut it, flag)?))?,
            // A7.1: the identity of the bytes `--codex` must still be, checked
            // against the same grammar the launcher writes it with.
            "--codex-sha256" => {
                let parsed = crate::codex::parse_codex_sha256(&value_of(&mut it, flag)?)
                    .with_context(|| flag.to_string())?;
                set_once(&mut codex_sha256, flag, parsed)?;
            }
            "--uid" => set_once(&mut uid, flag, value_of(&mut it, flag)?)?,
            "--nonce" => set_once(&mut nonce, flag, value_of(&mut it, flag)?)?,
            "--tmux-socket" => set_once(&mut tmux_socket, flag, value_of(&mut it, flag)?)?,
            "--run-dir" => set_once(&mut run_dir, flag, PathBuf::from(value_of(&mut it, flag)?))?,
            "--codex-home" => set_once(
                &mut codex_home,
                flag,
                PathBuf::from(value_of(&mut it, flag)?),
            )?,
            "--approval-policy" => set_once(&mut approval_policy, flag, value_of(&mut it, flag)?)?,
            "--approvals-reviewer" => {
                set_once(&mut approvals_reviewer, flag, value_of(&mut it, flag)?)?
            }
            "--sandbox" => set_once(&mut sandbox, flag, value_of(&mut it, flag)?)?,
            "--hooks-enabled" => {
                let parsed = parse_hooks_enabled(&value_of(&mut it, flag)?)?;
                set_once(&mut hooks_enabled, flag, parsed)?;
            }
            // The workspace anchor (round-2 P4): the CANONICAL cwd this session was
            // launched in. The coordinator resolved it once; the host passes it through
            // verbatim and never re-resolves, so the broker compares exact strings.
            "--launch-cwd" => set_once(&mut launch_cwd, flag, value_of(&mut it, flag)?)?,
            // Everything past the boundary belongs to the TUI, verbatim.
            "--" => {
                tui_args.extend(it.by_ref().cloned());
                break;
            }
            other => bail!("unexpected argument to internal-codex-host: {other:?}"),
        }
    }

    // Single source of truth for what a caller may hand the TUI: the 2a grammar.
    // Reusing it (rather than restating it) is the point — the host cannot drift
    // from `codeconnect codex` on which flags are CodeConnect's to own.
    crate::codex::validate_codex_argv(&tui_args)
        .map_err(|refusal| anyhow!("refused passthrough TUI argument: {refusal}"))?;

    // The absolute-path rule, from the module that owns it and that the coordinator
    // applies to the same string. The host repeats it rather than trusting its
    // parent: the process whose spawns this decides is the one that has to be sure.
    let codex = codex.context("--codex <path> is required")?;
    crate::codex::require_absolute_codex(&codex)?;

    Ok(HostArgs {
        codex,
        codex_sha256: codex_sha256.context(
            "--codex-sha256 <hex> is required (the host verifies the codex binary's identity \
             before each exec and applies no default: without it the launch would be trusting \
             a pathname)",
        )?,
        run_dir: run_dir.context("--run-dir <path> is required")?,
        codex_home: codex_home.context("--codex-home <path> is required")?,
        uid: uid.context("--uid <value> is required (the host must name its launch)")?,
        nonce: nonce.context("--nonce <value> is required (the host must prove it was invited)")?,
        tmux_socket: tmux_socket.context("--tmux-socket <value> is required")?,
        fingerprint: LaunchFingerprint {
            approval_policy: approval_policy
                .context("--approval-policy <value> is required (the host applies no default)")?,
            approvals_reviewer: approvals_reviewer.context(
                "--approvals-reviewer <value> is required (the host applies no default)",
            )?,
            sandbox: sandbox
                .context("--sandbox <value> is required (the host applies no default)")?,
            hooks_enabled: hooks_enabled
                .context("--hooks-enabled true|false is required (the host applies no default)")?,
            launch_cwd: launch_cwd.context(
                "--launch-cwd <canonical path> is required (the host applies no default, and \
                 re-resolving it here would be a second, disagreeing canonicalization)",
            )?,
        },
        tui_args,
    })
}

/// Take a flag's required value: present, non-empty, free of control characters
/// (which could smuggle terminal escapes into a log or an error message, or a NUL
/// into a path), and not itself flag-shaped.
///
/// `pub(crate)` because the **coordinator** builds this host's charter and parses
/// its own with the same two helpers (2e-2b). One grammar, one place: a
/// coordinator that validated its `--codex` / `--sandbox` / `--hooks-enabled`
/// arguments more loosely than the host validates the same arguments would be a
/// second grammar that can disagree with this one.
///
/// The shape check closes a swallowing hole. Without it `--codex --run-dir` sets
/// `codex` to the literal `"--run-dir"` and eats the flag, and — the case that
/// actually bites — `--codex --` sets `codex` to `"--"` and **destroys the
/// passthrough boundary**, so every following token is re-read as a host flag.
/// Those inputs happen to fail closed today via a later missing-flag error, but by
/// accident of the error paths rather than by construction. No legitimate value
/// here starts with `-`: all seven are paths or policy words.
pub(crate) fn value_of(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String> {
    let value = it
        .next()
        .with_context(|| format!("{flag} requires a value"))?;
    if value.is_empty() {
        bail!("{flag} requires a non-empty value");
    }
    if value.chars().any(char::is_control) {
        bail!("{flag} value contains a control character and is refused");
    }
    if value.starts_with('-') {
        bail!(
            "{flag} requires a value, but the next argument is {value:?}; a flag-shaped \
             value is refused rather than swallowed"
        );
    }
    Ok(value.clone())
}

/// The `--hooks-enabled` spelling, shared with the coordinator that writes it.
///
/// Never a silent default: an unrecognised spelling would otherwise decide hook
/// trust by accident, and a coordinator that accepted `yes` while the host
/// accepted only `true` would be a disagreement about a security dimension
/// discovered at the pane rather than at the charter.
pub(crate) fn parse_hooks_enabled(raw: &str) -> Result<bool> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => bail!("--hooks-enabled must be exactly \"true\" or \"false\", got {other:?}"),
    }
}

/// Record a flag's value, refusing a second occurrence — a duplicate is an
/// ambiguous charter, not a last-wins override. `pub(crate)` for the same
/// one-grammar reason as [`value_of`].
pub(crate) fn set_once<T>(slot: &mut Option<T>, flag: &str, value: T) -> Result<()> {
    if slot.is_some() {
        bail!("{flag} was given more than once");
    }
    *slot = Some(value);
    Ok(())
}

// ------------------------------------------------------------- orchestration

/// Bring the three parts up fail-closed, race them plus a host signal, tear
/// everything down, and return the session's exit code.
/// Build the run dir and its owner marker under a temp name, then publish the
/// finished thing with one `rename` (A11.4).
///
/// The invariant this buys: **a directory at the final run-dir path always carries
/// its owner marker.** Nothing else can be observed there, because the only thing
/// ever placed at that name is a directory that already contains the marker.
///
/// `renamex_np(..., RENAME_EXCL)` rather than plain `rename(2)`, and that is not a
/// stylistic choice — measured: a plain rename onto an existing *empty* directory
/// SUCCEEDS and replaces it. That would silently convert the host's "refuses to
/// adopt a directory it did not create" rule into "quietly takes over a squatted
/// name", which is the opposite of what the `mkdir` it replaces was for.
/// `RENAME_EXCL` fails `EEXIST` instead, so a squatted final name is still a
/// refused launch.
///
/// The residual, stated plainly: a host SIGKILLed between the temp `mkdir` and the
/// marker write leaks an unmarked `<run_dir>.tmp`. It is never RECORDED, so cleanup
/// never reasons about it the way it had to about an unmarked dir at the final path.
///
/// It is not, however, never consulted: the next launch that derives the same run-dir
/// name stages through this exact path, and its `create_dir` is `create_new`, so a
/// leftover `.tmp` there makes that launch fail `EEXIST` — refused rather than
/// adopted, which is the fail-closed direction, but a refusal all the same. The name
/// carries the launch nonce, so recurrence needs a nonce collision; the residual is
/// negligible, not absent, and it is stated that way here.
fn create_run_dir_atomically(run_dir: &Path, uid: &str, launch_nonce: &str) -> Result<()> {
    let parent = run_dir
        .parent()
        .ok_or_else(|| anyhow!("the run dir {} has no parent", run_dir.display()))?;
    let name = run_dir
        .file_name()
        .ok_or_else(|| anyhow!("the run dir {} has no final component", run_dir.display()))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(".tmp");
    let temp = parent.join(temp_name);

    // Exclusive, private, and fresh — the same guarantee the final `mkdir` used to
    // give, just moved to a name nobody else consults.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&temp)
        .with_context(|| {
            format!(
                "creating the staging run dir {} exclusively — it must NOT already exist",
                temp.display()
            )
        })?;

    // Everything from here removes the staging dir on failure: a host that does not
    // come up leaves nothing behind.
    let staged = (|| -> Result<()> {
        crate::codex_launch::write_owner_marker(&temp, uid, launch_nonce)?;
        // The marker's dirent must be durable BEFORE the rename publishes the
        // directory, or a crash could expose a dir at the final name whose marker
        // has not landed — exactly the state this is built to make impossible.
        std::fs::File::open(&temp)
            .with_context(|| format!("opening {} to flush it", temp.display()))?
            .sync_all()
            .with_context(|| format!("fsync of {}", temp.display()))
    })();
    if let Err(err) = staged {
        let _ = std::fs::remove_dir_all(&temp);
        return Err(err);
    }

    let c_from = std::ffi::CString::new(temp.as_os_str().as_encoded_bytes())
        .with_context(|| format!("{} contains a NUL byte", temp.display()))?;
    let c_to = std::ffi::CString::new(run_dir.as_os_str().as_encoded_bytes())
        .with_context(|| format!("{} contains a NUL byte", run_dir.display()))?;
    // SAFETY: two NUL-terminated paths owned by the CStrings above, live for the call.
    let rc = unsafe { libc::renamex_np(c_from.as_ptr(), c_to.as_ptr(), libc::RENAME_EXCL) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir_all(&temp);
        if err.raw_os_error() == Some(libc::EEXIST) {
            bail!(
                "creating the host run dir {} exclusively — it must NOT already exist \
                 (the host owns it and refuses to adopt a directory it did not create)",
                run_dir.display()
            );
        }
        return Err(err).with_context(|| {
            format!(
                "publishing the staged run dir {} as {}",
                temp.display(),
                run_dir.display()
            )
        });
    }

    // And make the published name itself durable, so a crash cannot lose the dirent
    // the record already points at.
    //
    // A11.7 / round-2 finding 3: this is the one step that runs AFTER the publish, so
    // a failure here is a failure by a host that has ALREADY claimed the directory.
    // Returning straight out would strand that claimed directory and leave the caller
    // exiting before it can either record the claim or sweep — a host that got past
    // the fence, leaving no trace that it did. Take the directory back down first, so
    // the only thing a post-publish failure leaves behind is the failure itself.
    let durable = std::fs::File::open(parent)
        .with_context(|| format!("opening {} to flush it", parent.display()))
        .and_then(|dir| {
            dir.sync_all()
                .with_context(|| format!("fsync of {}", parent.display()))
        });
    if let Err(err) = durable {
        sweep_own_run_dir(run_dir, uid, launch_nonce);
        return Err(err);
    }
    Ok(())
}

/// Remove the run dir this host created, on the way out.
///
/// `run_session` has already run teardown, so each child has been given a bounded
/// best-effort stop and the aborted broker task was given `BROKER_ABORT_BUDGET` to
/// finish. None of that is a proof — `start_kill` can fail, a reap can time out, a
/// task can ignore its cancellation point — so this runs either way. It is safe
/// that it does: unlinking a bound socket or an open log file is harmless to the fd
/// holding it, and a straggler that outlived the budget keeps working against an
/// unlinked inode rather than corrupting anything.
///
/// Best-effort — a cleanup failure must not mask the session outcome — but NOT
/// silent. The module doc says the host *attempts* to remove the run dir, and an
/// attempt that failed with no record is how a leak becomes invisible.
///
/// **A11.5: fd-anchored, not path-addressed.** This used to be
/// `remove_dir_all(&args.run_dir)`, which re-resolves the NAME at every step. The
/// name is not an identity — [`crate::codex_coordinator::choose_run_dir`]
/// truncates, so the derivation is many-to-one — and this host can still be inside
/// its bounded teardown while a custodian removes the old inode and a colliding
/// launch publishes the same name. A path-addressed delete would then land on the
/// replacement's bound sockets and logs. The shared sweep opens the directory once,
/// proves the owner marker **through that descriptor**, and unlinks everything
/// relative to it, so the thing deleted is provably the thing verified — and a
/// directory whose marker is not ours is refused, which is a bar this host, the
/// process that wrote that marker, is the one thing that always clears.
fn sweep_own_run_dir(run_dir: &Path, uid: &str, nonce: &str) {
    match crate::codex_launch::sweep_owned_run_dir(run_dir, uid, nonce) {
        crate::codex_launch::RunDirSweep::Settled(note) => {
            if let Some(note) = note {
                eprintln!("codex-host: {note}");
            }
        }
        // The host has no later pass. Saying so is the point: the custodian is the
        // actor that comes back for it, and this line is what tells whoever reads
        // the pane why it had to.
        crate::codex_launch::RunDirSweep::Retry(why) => {
            eprintln!("codex-host: {why}; leaving it for the custodian");
        }
    }
}

async fn orchestrate(args: HostArgs) -> Result<i32> {
    let paths = Paths::under(&args.run_dir);
    for sock in [&paths.as_sock, &paths.tui_sock, &paths.ccd_sock] {
        assert_sun_len(sock)?;
    }

    // Signals BEFORE the first thing that needs cleaning up. The run dir is created
    // on the next statement, so installing here (rather than inside `run_session`)
    // closes the window where a SIGTERM would kill the host at default disposition
    // and strand the directory.
    let mut signals = Signals::install()?;

    // OWN the run dir. A single non-recursive `mkdir(0700)` fails if the path
    // exists at all, so from here the directory is provably fresh, private, and
    // ours: no socket under it can pre-exist, and the final path component cannot
    // be a planted symlink (`mkdir` does not follow one — it gets `EEXIST`). That
    // is what makes "a 0600 socket appeared" mean "our app-server bound it".
    // Nothing has been created yet on failure.
    //
    // Scope, stated honestly: this covers the final component only. The host does
    // not validate the PARENT chain of `--run-dir`, so a same-uid attacker who
    // controls a parent directory can still redirect where the dir is made — and
    // therefore what the sweep below removes.
    //
    // And the parent is **not** the caller's own 0700 session dir, as this used to
    // say. The coordinator passes a path directly under world-writable, sticky
    // `/tmp`, because a home-rooted run dir spends the SUN_LEN budget on the length
    // of the user's home path (see
    // `crate::codex_coordinator::RUN_DIR_PREFIX`). So this `mkdir` is not one
    // safeguard among several — it is the load-bearing one: it turns a squatted
    // name into a refused launch rather than an adopted directory. The marker
    // written inside it is what then makes the directory say whose it is, since its
    // NAME cannot (the derivation is many-to-one).
    //
    // A11.4: the dir and its marker are built under a TEMP name and published by a
    // single `rename`, so no unmarked directory can ever exist at the final path.
    // The old order — `mkdir(final)` then write the marker — left a window in which
    // a SIGKILLed host stranded an unmarked dir at exactly the path the custodian
    // later consults, and the custodian correctly refuses to delete a directory it
    // cannot prove is the launch's. Publishing atomically makes that state
    // structurally impossible rather than merely unlikely, which is why this is
    // preferred over an age-bounded sweep of unmarked dirs (no new clock, no
    // heuristic).
    create_run_dir_atomically(&args.run_dir, &args.uid, &args.nonce)?;
    // A11.7: recording the claim is launch-fatal (see `note_run_dir_claimed`). A host
    // that claimed the dir but cannot record it must not run on with the bit false —
    // that is the exact state the gate's negative proof would misread as a fence
    // refusal. Tear down the directory just created and exit fatal, so the only host
    // that ever proceeds past here is one whose claim is on the record.
    if let Err(err) = note_run_dir_claimed(&args) {
        eprintln!("codex-host: {err:#}");
        sweep_own_run_dir(&args.run_dir, &args.uid, &args.nonce);
        return Ok(EX_HOST_FATAL);
    }

    let outcome = run_session(&args, &paths, &mut signals).await;

    sweep_own_run_dir(&args.run_dir, &args.uid, &args.nonce);

    Ok(match outcome {
        Outcome::TuiExited(status) => {
            // The user's TUI ended; its status is the session's status.
            // Deliberately coarsened: a signal-terminated TUI has no `code()`, and
            // the host reports 1 rather than inventing a 128+N encoding — the pane
            // is closing either way and the distinction has no consumer.
            status.code().unwrap_or(1)
        }
        Outcome::Fatal(detail) => {
            eprintln!("codex-host: {detail}");
            EX_HOST_FATAL
        }
        Outcome::Signalled(name) => {
            eprintln!("codex-host: received {name}; the session was torn down");
            EX_HOST_SIGNALLED
        }
    })
}

/// Record, durably, that this host got past the run-dir claim.
///
/// **LAUNCH-FATAL, not best-effort — the caller must refuse if this fails.** The
/// bit it writes is read *negatively* by the A11.7 gate: a losing host with
/// `host_claimed_run_dir == false` is taken as proof it was refused at the fence
/// and never adopted the winner's directory. A best-effort write cannot support
/// that proof. If this were allowed to fail-and-continue, a host that DID claim the
/// dir (its `mkdir` won) but could not *record* the claim — the record lock held
/// past [`RECORD_LOCK_BUDGET`], its identity unresolvable, the store faulted — would
/// run on with the bit still false, and a later death (a colliding log, a socket in
/// use) would present to the gate as a clean fence-refusal. The negative proof would
/// then be true for exactly the host it must exclude.
///
/// So a failure here is a refusal. A host that cannot write its own claim cannot
/// establish its lease/identity either — the same lease this write is fenced on — so
/// it is in an unknown state with respect to the very record the custodian will
/// later act on, and must not proceed. The caller tears down the directory it just
/// created and exits fatal.
///
/// **What the false bit then licenses, stated exactly.** The invariant this buys is
/// *no host enters `run_session` or spawns a child without its claim on the record* —
/// so `host_claimed_run_dir == false` proves the host never got past the claim into
/// the stages beyond it, which is what the A11.7 gate needs. It is NOT the stronger
/// "this host never touched the directory": a host can still die between the
/// `RENAME_EXCL` publish and this write (a SIGKILL, or the post-publish parent fsync
/// failing — which `create_run_dir_atomically` now sweeps before returning, precisely
/// so that case strands nothing). For the A11.7 *loser* the distinction cannot arise
/// at all: it meets `EEXIST` at the publish and never owns a directory to claim.
///
/// A bool, not the claimed `(dev, ino)`. `host_claimed_run_dir == false` on a
/// loser directly refutes "the host got past the claim", which is the whole of
/// finding 11; carrying the inode too would let a gate ALSO prove an adopter
/// walked past onto the *winner's* directory, but that is a strengthening this
/// gate does not need and it is left unbuilt.
fn note_run_dir_claimed(args: &HostArgs) -> Result<()> {
    let me = crate::codex_launch::require_current_identity()?;
    let lock = crate::codex_launch::LaunchLock::acquire_bounded(&args.uid, CLAIM_RECORD_BUDGET)?;
    crate::codex_launch::note_host_claimed_run_dir(&lock, &args.uid, &me)
        .context("recording that this host claimed the run dir")
}

/// How the session ended, after teardown has already run.
#[derive(Debug)]
enum Outcome {
    /// The TUI exited on its own, with the broker and the app-server both still
    /// alive — the only benign ending, and the only one that carries a status.
    TuiExited(std::process::ExitStatus),
    /// Bring-up could not be proven, or the app-server / broker died under a live
    /// session. Both are session-fatal and exit [`EX_HOST_FATAL`].
    Fatal(String),
    /// The host itself was signalled, during bring-up or during the session.
    Signalled(&'static str),
}

// ------------------------------------------------- the outcome resolution seam

/// Which arm of the session race fired first, reduced to plain data.
///
/// Deliberately carries *only* what is unique to the arm: the app-server's state
/// is a separate observation ([`AppServerState`]) so the app-server arm and the
/// reconciling arms cannot disagree about it.
#[derive(Debug)]
enum Fired {
    /// `appserver.wait()` returned — what it reported is in the `appserver`
    /// observation passed alongside.
    AppServer,
    /// The broker's `JoinHandle` completed.
    Broker(BrokerEnd),
    /// `tui.wait()` returned.
    Tui(WaitReport),
    /// A host signal (SIGTERM/SIGINT/SIGHUP) was delivered.
    Signal(&'static str),
}

/// How the broker's serve task ended. A panic is **absent on purpose**: under this
/// workspace's release `panic = "abort"` profile a panicking broker aborts the
/// host outright and never reaches a `JoinHandle` at all (see the module doc);
/// under the unwinding dev/test profile it arrives as [`BrokerEnd::Abnormal`].
#[derive(Debug)]
enum BrokerEnd {
    /// `serve()` returned `Ok(())` — it stopped accepting.
    Stopped,
    /// `serve()` returned an error.
    Failed(String),
    /// The task itself ended abnormally: a `JoinError` (cancellation, or a panic
    /// on an unwinding profile).
    Abnormal(String),
}

/// What a `wait()` on a child reported.
#[derive(Debug)]
enum WaitReport {
    Exited(std::process::ExitStatus),
    /// The wait itself failed. This proves nothing about the child, so it is never
    /// read as a benign ending — see [`resolve_outcome`].
    Undetermined(String),
}

/// What a `try_wait()` (or the app-server arm's own `wait()`) said about the
/// app-server at the moment another arm fired.
#[derive(Debug)]
enum AppServerState {
    Alive,
    Exited(std::process::ExitStatus),
    /// Its state could not be determined.
    Undetermined(String),
}

impl AppServerState {
    /// From the app-server arm's own `wait()`: it returned, so the child is not
    /// `Alive` on any path.
    fn from_wait(status: std::io::Result<std::process::ExitStatus>) -> Self {
        match status {
            Ok(status) => AppServerState::Exited(status),
            Err(err) => AppServerState::Undetermined(err.to_string()),
        }
    }

    /// From a reconciling arm's `try_wait()`.
    fn from_try_wait(probe: std::io::Result<Option<std::process::ExitStatus>>) -> Self {
        match probe {
            Ok(None) => AppServerState::Alive,
            Ok(Some(status)) => AppServerState::Exited(status),
            Err(err) => AppServerState::Undetermined(err.to_string()),
        }
    }
}

/// **The tested seam.** Decide the session's outcome from the three observed
/// states — which arm fired, what the app-server is doing, and whether the broker
/// task has finished — as a pure function.
///
/// This exists because the race's arms cannot be tested directly. A live
/// broker-death end-to-end test is impractical: [`Broker::serve`] only returns on
/// an accept error, and nothing external can deterministically end a tokio task
/// inside another process. So the arms are kept **thin** — observe, set the reaped
/// flags, call this — and every interesting decision is made and unit-tested here.
///
/// Two precedence rules, both "fail closed", and both **universal** — they hold on
/// every arm, not only on the ones that look benign:
///
/// 1. **Broker death outranks everything**, including a host signal. The broker is
///    the security boundary in front of the app-server; children must never be
///    reported healthy past a dead boundary. A signal delivered while the broker
///    is already gone is therefore [`Outcome::Fatal`], not a clean 130 — the
///    signal explains why the *host* is stopping, not why the boundary vanished,
///    and 130 would tell the coordinator the session ended by request. The rule
///    also governs *attribution*, not just the verdict: when the app-server arm
///    fires and the broker had also finished when observed, the outcome is fatal
///    either way, but the message must name the broker too rather than pin both
///    facts on the upstream alone.
/// 2. **Failure to determine a state is never benign.** An app-server whose state
///    is unknown, or a TUI whose `wait()` errored, resolves to
///    [`Outcome::Fatal`] rather than to a status the host cannot stand behind.
///    This applies to the signal arm as well: a signal that arrives while the
///    app-server is dead or unaccountable is not "the session ended by request".
fn resolve_outcome(fired: Fired, appserver: AppServerState, broker_finished: bool) -> Outcome {
    match fired {
        // --- Already-fatal arms: nothing to reconcile, only to word ----------
        Fired::AppServer => {
            let mut detail = match appserver {
                AppServerState::Exited(status) => format!(
                    "the codex app-server exited ({status}) while the session was up; \
                     a TUI must never be left talking to a dead upstream"
                ),
                AppServerState::Undetermined(err) => format!(
                    "lost track of the codex app-server ({err}) while the session was up; \
                     treating it as dead and failing closed"
                ),
                // Unreachable by construction (`from_wait` never yields `Alive`), and
                // still fatal rather than a panic: this is the session-ending path.
                AppServerState::Alive => {
                    "the codex app-server's wait returned while the session was \
                     up, but it was then reported alive; failing closed"
                        .to_string()
                }
            };
            if broker_finished {
                // Rule 1 as an ATTRIBUTION rule. The verdict does not change — this
                // arm is fatal regardless — but a reader of the pane's stderr must
                // not be told the upstream died in isolation when the security
                // boundary in front of it had gone too. "The upstream died" sends
                // someone looking for an upstream cause; a boundary that had also
                // finished is a different and more serious fact, and it is the one
                // that must not be lost.
                //
                // No claim is made about how OFTEN this happens — not "common", not
                // "rare" — because nothing here measures it. What IS known is the
                // mechanism, and the tempting inference from it would be wrong:
                // `broker_finished` observes the `serve()` task alone, which ends on
                // an accept error — not the per-connection leg tasks, which `serve`
                // spawns detached and which nobody here owns (see
                // [`BROKER_ABORT_BUDGET`]). So an app-server death does not by itself
                // finish the serve task. This is a corner covered for attribution:
                // losing the boundary in the wording would be a security-relevant
                // error whenever it does occur.
                detail.push_str(
                    "; the broker — the security boundary in front of the app-server — had \
                     also stopped, so this is a dead boundary as well as a dead upstream",
                );
            }
            Outcome::Fatal(detail)
        }
        Fired::Broker(end) => Outcome::Fatal(match end {
            BrokerEnd::Stopped => "the broker stopped serving while the session was up".to_string(),
            BrokerEnd::Failed(err) => {
                format!("the broker failed while the session was up: {err}")
            }
            BrokerEnd::Abnormal(err) => {
                format!("the broker task ended abnormally ({err}) while the session was up")
            }
        }),

        // --- The arms that only LOOK benign ----------------------------------
        Fired::Tui(report) => {
            if broker_finished {
                // Rule 1. `biased` orders polling, not readiness: the broker task
                // can poll Pending and then finish before `tui.wait()` returns
                // Ready in the same pass, so without this check a dead security
                // boundary is reported as a clean user quit.
                return Outcome::Fatal(
                    "the codex TUI exited while the broker — the security boundary in front of \
                     the app-server — had already stopped; a clean-looking TUI exit past a dead \
                     broker is a masked fatal, not a benign ending"
                        .to_string(),
                );
            }
            match appserver {
                AppServerState::Exited(as_status) => Outcome::Fatal(format!(
                    "the codex TUI exited together with its app-server ({as_status}); \
                     the upstream died first and the session is fatal, not clean"
                )),
                AppServerState::Undetermined(err) => Outcome::Fatal(format!(
                    "the codex TUI exited and the app-server's state could not be \
                     determined ({err}); failing closed"
                )),
                AppServerState::Alive => match report {
                    WaitReport::Exited(status) => Outcome::TuiExited(status),
                    // Rule 2. Failing to DETERMINE the TUI's state is not a benign
                    // exit: there is no status to report, and the host must not
                    // coarsen "I do not know" into an exit code that reads as one.
                    WaitReport::Undetermined(err) => Outcome::Fatal(format!(
                        "the codex TUI ended but its state could not be determined ({err}); \
                         a session whose TUI cannot be accounted for is not a clean exit"
                    )),
                },
            }
        }
        Fired::Signal(name) => {
            if broker_finished {
                // Rule 1 again, and the deliberate choice: broker death takes
                // precedence over the signal. Both are true, but only one of them
                // is a security fact the coordinator must not mistake for a
                // requested shutdown.
                return Outcome::Fatal(format!(
                    "received {name}, but the broker — the security boundary in front of the \
                     app-server — had already stopped; reporting the dead boundary rather than \
                     a clean signalled shutdown"
                ));
            }
            // The signal arm re-observes the app-server for the same reason the TUI
            // arm does: a signal and an upstream death can land in the same pass,
            // and 130 tells the coordinator "the session ended by request" — which
            // is the wrong story when the upstream was already gone. Rules 1 and 2
            // are universal, so this arm applies them too rather than assuming the
            // app-server is alive because nothing on this arm looked at it.
            match appserver {
                AppServerState::Alive => Outcome::Signalled(name),
                AppServerState::Exited(status) => Outcome::Fatal(format!(
                    "received {name}, but the codex app-server had already exited ({status}); \
                     an upstream death racing the signal is session-fatal, not a shutdown \
                     ended by request"
                )),
                AppServerState::Undetermined(err) => Outcome::Fatal(format!(
                    "received {name}, but the codex app-server's state could not be determined \
                     ({err}); failing closed rather than reporting a clean signalled shutdown"
                )),
            }
        }
    }
}

/// Spawn the app-server, hand it straight to the teardown guard, and drive the
/// session. Every return path — including every `?` inside [`drive`] — passes
/// through exactly one [`Session::teardown`].
async fn run_session(args: &HostArgs, paths: &Paths, signals: &mut Signals) -> Outcome {
    // --- Everything fallible that does NOT need a child, hoisted ------------
    // (Signals are already installed by the caller, before the run dir existed.)
    // `create_new` in a directory we just exclusively created: both logs are
    // provably fresh, ours, and 0600. Never an adopted or appended-to file.
    //
    // The host keeps THIS handle — the open file description on the inode it just
    // created — and hands the child a `dup` of it. Every stderr excerpt is then
    // read back through the handle, and the path is never reopened after the
    // spawn. That is deliberate: the path is inside a directory the child can
    // write to, so a reopen-by-path reads whatever is at that name *now* — which a
    // child could have replaced with a FIFO or a device node that blocks the host
    // forever inside a bring-up error path. A held fd cannot be redirected.
    let mut as_stderr_file = match create_private_log(&paths.as_stderr) {
        Ok(f) => f,
        Err(err) => return Outcome::Fatal(format!("{err:#}")),
    };
    let as_stderr_for_child = match as_stderr_file.try_clone() {
        Ok(f) => f,
        Err(err) => {
            return Outcome::Fatal(format!(
                "duplicating the app-server stderr handle for the child: {err}"
            ))
        }
    };
    // The broker's decision lines go to the log file as before, and the ONE line
    // that explains a session that never started is kept in memory as well — the
    // run dir is swept on the way out, and the record write that quotes it happens
    // after the session is already over.
    let first_refusal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink = match file_event_sink(&paths.broker_log) {
        Ok(s) => refusal_watching_sink(s, Arc::clone(&first_refusal)),
        Err(err) => return Outcome::Fatal(format!("{err:#}")),
    };

    // A7.1: the last check before these bytes become a process. Deliberately here,
    // among the fallible-but-childless work and *before* the guard owns anything:
    // a mismatch must abort with nothing spawned, since the entire point is that
    // this file never runs. Re-derived rather than remembered — the digest was
    // taken in the launcher, in another process, and the question now is about this
    // instant.
    //
    // Inline and blocking, unlike its counterpart before the TUI spawn, and the
    // difference is the state of the world at each point. Here nothing is serving
    // and nothing is spawned: the two log-file calls above are blocking too, so a
    // signal arriving mid-check costs one whole-file read before a host that has
    // created nothing exits. There the broker IS serving, and blocking a worker
    // would make the host deaf to SIGTERM while a live session depends on it.
    //
    // ## The update race from here to `execve` is closed; a hostile peer is not
    //
    // `verify_codex_identity` no longer just reads and compares — it FREEZES the
    // bytes immutable (`fchflags(fd, UF_IMMUTABLE)`) before hashing and hands back a
    // guard that keeps them frozen. `frozen` below holds that guard across the whole
    // tail this check used to leave open:
    //
    //   verify returns → `Command::spawn` → fork → the A11.1 fence rendezvous
    //   (the child reports its pid, and `record_child_pid` writes this child's
    //   identity DURABLY before the GO byte is sent) → GO → the child's `execve`
    //
    // While the flag is held, every way of replacing or rewriting that pathname FROM
    // A FRESH START is refused by the kernel — `open(O_WRONLY)`/`open(O_RDWR)`,
    // `ftruncate`, a write through any other hard link, `rename`-over,
    // `renamex_np(RENAME_SWAP)`, `clonefile`-over, `unlink`, all measured
    // `EPERM`/`EEXIST`/`EINVAL`. That is exactly the shape of the vector this gate
    // was built for — an installer, an `npm` overwrite, a `standalone/current` flip
    // landing mid-launch — so for the update race the bytes `execve` loads are the
    // bytes hashed here, including against the same-inode overwrite a bare
    // `(dev, ino)` comparison can never see.
    //
    // It is NOT a boundary against a hostile process running as this uid, and must
    // not be read as one: `UF_IMMUTABLE` is owner-revocable (`chflags nouchg`), and a
    // writable fd opened BEFORE the freeze keeps writing straight through it — both
    // measured. That class is already out of scope by construction — invariant 1 in
    // this module's doc, which defers to `codex::start` (A22) for what it covers —
    // and macOS offers nothing that would change it: there is no exec-by-descriptor
    // (`fexecve` is not even a symbol in libSystem; `execve("/dev/fd/N", …)` is
    // `EACCES`), and a private staging copy is equally writable by that same uid.
    // `protocol::hash::FrozenExecutable` carries all three measurements.
    //
    // ## What is left after the clear
    //
    // The freeze is cleared once the child is proven past `execve` (see the `drop`
    // below). macOS demand-pages a Mach-O's text over the process's life and `execve`
    // takes no snapshot, so an in-place write after the clear can reach a
    // not-yet-faulted page — but codex is signed and hardened, and was measured
    // running with `CS_HARD | CS_KILL`, so a substituted page is refused and the
    // process KILLED rather than steered. Denial-of-service, not code execution, for
    // a signed build. (A18/A20 number this gate "A7.1 executable hash-pin".)
    let frozen = match crate::codex::verify_codex_identity(
        &args.codex,
        &args.codex_sha256,
        "immediately before the app-server spawn",
    ) {
        Ok(frozen) => frozen,
        Err(err) => return Outcome::Fatal(format!("{err:#}")),
    };

    // --- The first spawn. Nothing fallible may run before the guard owns it --
    //
    // A11.1: spawned through the fence, so its identity is durable before it is ever
    // allowed to become codex. The recording that used to follow the spawn now
    // happens *inside* it, while the child is still parked before `execve`.
    let mut appserver_cmd = Command::new(&args.codex);
    appserver_cmd
        .arg("app-server")
        .arg("--listen")
        .arg(format!("unix://{}", paths.as_sock.display()))
        .env("CODEX_HOME", &args.codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(as_stderr_for_child))
        // Its OWN process group, so cleanup can address it — and anything it
        // forks — by a recorded pgid rather than hoping a signal aimed elsewhere
        // reaches it. Safe here precisely because this child has no tty: its
        // stdio is null/null/logfile, so being a background process group costs
        // it nothing. (The TUI is the opposite case; see its spawn below.)
        //
        // Measured, and the reason the fence can record a pgid at all: this is
        // applied BEFORE the `pre_exec` closure runs, so the group the releaser
        // reads while the child is fenced is already the final one.
        .process_group(0)
        // Safety net beneath the explicit teardown: even an unwind or an
        // early return that somehow skipped `teardown` cannot leak this child.
        .kill_on_drop(true);
    // The app-server's identity is durable BEFORE anything depends on the session
    // being up. The coordinator commits `ready` once the broker's legs are bound,
    // which is strictly after this point, so a session that is ever declared ready
    // has a recorded, signalable app-server behind it.
    let appserver = match spawn_fenced(&mut appserver_cmd, args, "app-server") {
        Ok(child) => child,
        // Launch-fatal. Nothing is up beyond this child and `Session` has not taken
        // it, so returning here drops it through `kill_on_drop`. A session whose
        // processes cleanup cannot name is the leak this chunk exists to remove.
        // `frozen` also drops on this path, clearing the freeze — nothing ran.
        Err(err) => return Outcome::Fatal(format!("{err:#}")),
    };
    // `spawn_fenced` returns only after the app-server is proven past `execve`
    // (`prove_past_execve` + `confirm_child_exec`), so the bytes it loaded are the
    // frozen, hashed bytes. The freeze has done its whole job; clear it. The tail
    // after this — demand-paged text from a signed, hardened-runtime binary — is
    // defended by the kernel's per-page code-signature check, not by this flag.
    drop(frozen);

    let mut session = Session::new(appserver, sink);
    let outcome = drive(&mut session, args, paths, signals, &mut as_stderr_file).await;
    session.teardown().await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => Outcome::Fatal(format!("{err:#}")),
    };

    // **The session is over and the run dir is about to be swept, so this is the
    // last moment either of these facts exists.** `orchestrate` calls
    // `sweep_own_run_dir` the instant this function returns, and that removes
    // `broker.log` — the only account of why a session ended the way it did.
    // **On the blocking pool.** Even bounded, this is file I/O plus a directory scan
    // and a handful of unlinks, and it sits on the path a live runtime is trying to
    // shut down. `spawn_blocking` is the same placement the thread-binding watcher's
    // record write uses, and for the same reason: an async worker is the wrong thread
    // to do syscall-bound work on. Awaited rather than detached — the reason text
    // below names the file this produces, so it has to exist before that sentence is
    // written; and a join error yields `None`, which reads as "not preserved" and is
    // exactly what it is.
    let preserved = {
        let uid = args.uid.clone();
        let broker_log = paths.broker_log.clone();
        tokio::task::spawn_blocking(move || preserve_broker_log(&uid, &broker_log))
            .await
            .ok()
            .flatten()
    };
    // Reported for a TUI that RAN AND EXITED, and for nothing else. A `Fatal`
    // outcome is a dead broker or a dead app-server, which already has its own
    // reason, its own `EX_HOST_FATAL` and its own stderr line; a `Signalled` one is
    // a pane that was torn down on request, which is the custodian's story to tell.
    // Neither is "the session never started", and wording them that way would put a
    // sentence in the record that is not true of them.
    if matches!(outcome, Outcome::TuiExited(_)) && !session.thread_ever_bound() {
        let quoted = first_refusal
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .map(|line| crate::codex_coordinator::sanitize(&line));
        let reason = no_thread_reason(quoted.as_deref(), preserved.as_deref());
        // Two writes, because they answer two different readers and only one of them
        // is still listening. `report_launch_without_a_thread` drives the RECORD to
        // `failed`, which is what the launcher is blocked on — and `to_failed`
        // refuses `ready → failed`, so it is a no-op once the launch has committed.
        // The line below is the durable FACT, written whatever the record's state,
        // and it is what the supervisor reads to tell `ccd` why a committed session
        // ended. Without it a session that reached `ready` and then died unbound had
        // nowhere to say so, which is precisely the case the daemon used to guess at.
        report_launch_without_a_thread(&args.uid, &reason);
        record_unbound_exit(&args.uid, &reason);
    }
    outcome
}

/// Wrap `sink` so the host keeps the **first** refusal the broker sent a leg.
///
/// A wrapper rather than a second responsibility inside [`file_event_sink`],
/// because the two have different lifetimes: the file is closed with the run dir,
/// and this outlives it.
///
/// # First, not last — measured on a live refused launch
///
/// A dying TUI produces more than one refusal. From a directory the owner's
/// `~/.codex` marks `trust_level = "trusted"`, the preserved log of a session that
/// never started holds, in this order:
///
/// ```text
/// 317: Tui: refuse->synthetic error (thread/start: fingerprint refused (Conflict): params.sandbox: …)
/// 319: Tui: refuse->synthetic error (thread/list: refused (NotAllowlisted))
/// 320: Tui: refuse->synthetic error (thread/list: refused (NotAllowlisted))
/// ```
///
/// The `thread/start` is the refusal that ended the session; the two `thread/list`
/// refusals are what a TUI already on its way out asks next. Keeping the last
/// match named the `thread/list` one, and the user's terminal then reported an
/// allowlist problem that is not the bug — a red herring pointing away from the
/// sandbox mismatch that actually caused it. In a session that never bound a
/// thread the first synthetic-error refusal is the cause and everything after it
/// is consequence, so the slot is written once and never overwritten.
fn refusal_watching_sink(sink: EventSink, first_refusal: Arc<Mutex<Option<String>>>) -> EventSink {
    Arc::new(move |line: &str| {
        if line.contains(REFUSAL_MARKER) {
            if let Ok(mut slot) = first_refusal.lock() {
                if slot.is_none() {
                    *slot = Some(line.to_string());
                }
            }
        }
        sink(line);
    })
}

/// Copy this session's `broker.log` out of the disposable run dir and into
/// `~/.codeconnect/logs/`, beside the supervisor and coordinator logs. Returns
/// where it landed, or `None` if it could not be kept.
///
/// **The run dir is ephemeral by design; the log is not.** Measured: a launch
/// whose first `thread/start` the fingerprint refuses records that refusal in
/// exactly one place — `<run_dir>/broker.log` — and every one of the host's own
/// exit paths then removes the directory. The user is left with a pane that
/// flickers, `[exited]`, and no artifact at all. Nothing else in the system holds
/// that sentence.
///
/// Best-effort in the same literal sense as [`crate::supervisor`]'s `log_for`: a
/// failure to keep a log must never change how a session ended, so every error is
/// swallowed. Owner-only, because a broker log quotes material that came off the
/// wire.
///
/// The session name is read from the launch record rather than carried in the
/// charter. The host's argv is the coordinator's entire agreement with it, and a
/// flag added only so a file could be named is one more thing the two can disagree
/// about; the uid in the filename is the identity either way.
///
/// # Bounded, and bounded at BOTH ends — measured
///
/// A broker log grows with the session: every forwarded request is a line, so a long
/// working session has no size this function can assume. It used to `io::copy` the
/// whole thing, which made the durable file as large as the session was chatty and
/// put an unbounded synchronous copy in the teardown path.
///
/// The bound keeps a head and a tail rather than either alone, because the line that
/// explains a failure is not where it seems it should be. MEASURED on the real
/// refused launch this whole change exists for: the log is 332 lines and the causal
/// `thread/start` refusal is **line 317** — the first ~316 are handshake, capability
/// reads and bootstrap forwards. A head-only bound would therefore have thrown away
/// precisely the sentence being preserved. The tail carries the verdict; the head
/// carries the session's opening, which is what says which fingerprint it ran under.
///
/// [`PRESERVED_LOG_EDGE_BYTES`] each end. A whole failed launch is ~17 KB, so the
/// common case — every case this feature is for — is copied entire and the bound
/// never engages; it exists for the long healthy session, whose middle is the part
/// nobody reads.
fn preserve_broker_log(uid: &str, broker_log: &Path) -> Option<PathBuf> {
    preserve_broker_log_into(&protocol::logs_dir(), uid, broker_log)
}

/// [`preserve_broker_log`] with the destination directory passed in rather than read
/// from process-global state.
///
/// Split for the same reason [`protocol::prune_session_logs`] is, and the reason is a
/// scar: the retention test's first version set `CODECONNECT_HOME` to point
/// `logs_dir()` at a scratch directory, raced another test that clears the same
/// variable, and pruned the REAL log directory instead — deleting nine of an
/// operator's preserved broker logs. A function that takes its directory cannot be
/// aimed at the wrong one by a concurrent test, and no test now reaches the env at
/// all.
fn preserve_broker_log_into(dir: &Path, uid: &str, broker_log: &Path) -> Option<PathBuf> {
    let session_name = crate::codex_launch::load(uid)
        .map(|record| record.session_name)
        .unwrap_or_else(|_| "unknown".to_string());
    protocol::fsperm::private_dir(dir).ok()?;
    let dest = dir.join(format!("broker-{session_name}-{uid}.log"));
    let mut src = std::fs::File::open(broker_log).ok()?;
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(protocol::fsperm::FILE_MODE)
        .open(&dest)
        .ok()?;
    let len = src.metadata().ok()?.len();
    let edge = PRESERVED_LOG_EDGE_BYTES as u64;
    if len <= edge * 2 {
        std::io::copy(&mut src, &mut out).ok()?;
    } else {
        let mut head = vec![0u8; PRESERVED_LOG_EDGE_BYTES];
        src.read_exact(&mut head).ok()?;
        out.write_all(&head).ok()?;
        // The elision is stated in the file, with the exact byte count, so a reader
        // can never mistake a bounded copy for a complete one.
        let dropped = len - edge * 2;
        out.write_all(
            format!("\n… {dropped} bytes elided by the preserved-log bound …\n").as_bytes(),
        )
        .ok()?;
        src.seek(SeekFrom::End(-(edge as i64))).ok()?;
        let mut tail = vec![0u8; PRESERVED_LOG_EDGE_BYTES];
        src.read_exact(&mut tail).ok()?;
        out.write_all(&tail).ok()?;
    }
    protocol::prune_logs_in_dir(dir, PRESERVED_LOG_PREFIX, PRESERVED_LOG_KEEP);
    Some(dest)
}

/// The reason a launch is failed with when its TUI exited without a thread ever
/// binding.
///
/// The preserved log path comes **before** the quoted refusal, and that ordering
/// is load-bearing rather than stylistic: the launcher prints this line through
/// `codex_coordinator::sanitize`, which bounds it at 300 characters, and the
/// measured `thread/start` refusal is 232 characters on its own. Putting the path
/// second would see it truncated away in exactly the case an operator needs it —
/// leaving a message that describes a problem and names nothing to read about it.
///
/// So the refusal is the half that truncates, and that is the right half to lose
/// the tail of: measured live, what survives the bound is
/// `thread/start: fingerprint refused (Conflict): params.sandbox: a sandbox mode
/// string(len=15) t…` — method, verdict, offending parameter and the value's
/// length, which is the whole diagnosis. The rest is in the log this names.
fn no_thread_reason(first_refusal: Option<&str>, broker_log: Option<&Path>) -> String {
    let mut reason = "the codex TUI exited without ever starting a thread".to_string();
    if let Some(path) = broker_log {
        reason.push_str(&format!(
            "; the broker's decision log is at {}",
            path.display()
        ));
    }
    if let Some(line) = first_refusal {
        reason.push_str(&format!("; first refusal: {line}"));
    }
    reason
}

/// Fail the launch record with `reason`, so the launcher prints it instead of
/// `exec`ing into a pane that is already gone.
///
/// **`to_failed` refusing `ready → failed` is the guard this leans on, not an
/// obstacle to it.** A launch the coordinator has already committed is one the
/// user is attached to, and a live session's teardown belongs to the supervisor —
/// so a session that outlived [`crate::codex_launch::THREAD_BINDING_GRACE`]
/// without binding a thread is reported by `ccd`'s `session_end` reason instead,
/// and this call is a no-op for it. While the record is still `pending`, though,
/// the launcher is sitting in `wait_on_record` with nothing yet printed, and this
/// is what turns the flicker into a sentence.
///
/// Best-effort: a record this host cannot write is one the coordinator's own
/// deadline still owns.
fn report_launch_without_a_thread(uid: &str, reason: &str) {
    if let Ok(lock) = crate::codex_launch::LaunchLock::acquire_bounded(uid, RECORD_LOCK_BUDGET) {
        let _ = crate::codex_launch::to_failed(
            &lock,
            uid,
            reason,
            crate::codex_launch::CleanupState::Pending,
        );
    }
}

/// Record, durably and whatever the launch record's state, that this session's TUI
/// exited without a thread ever binding. See [`LaunchRecord::codex_unbound_exit`].
///
/// Best-effort for the same reason every other host write at teardown is: a record
/// this host cannot take the lock on is one whose outcome the coordinator's own
/// deadline still owns, and failing the session over a missing explanation would
/// trade a legibility gap for an availability one.
fn record_unbound_exit(uid: &str, reason: &str) {
    if let Ok(lock) = crate::codex_launch::LaunchLock::acquire_bounded(uid, RECORD_LOCK_BUDGET) {
        let _ = crate::codex_launch::note_codex_unbound_exit(&lock, uid, reason);
    }
}

/// Watch for the broker's first thread binding and record it durably, so the
/// coordinator's bring-up can stop waiting and attach.
///
/// **It runs until teardown aborts it, and deliberately carries no deadline of its
/// own.** It used to stop after [`crate::codex_launch::THREAD_BINDING_GRACE`], on the
/// reasoning that the coordinator had stopped asking by then — but the two intervals
/// do not start together. This watcher starts the moment the broker does, BEFORE both
/// legs are proven and before the TUI is spawned; the coordinator's grace starts only
/// once classic bring-up reaches `Ready`. On a slow start-up the writer's ten seconds
/// could therefore expire before the reader's began, and a thread that bound in that
/// window went unrecorded — leaving the launcher to sit out the whole grace and then
/// attach, which is the right outcome reached the slow way, for no reason.
///
/// Ending at teardown removes the mismatch without needing the two clocks to agree:
/// the task is a cancellable `tokio` task held on [`Session::thread_watcher`], and
/// teardown aborts it. The cost of the longer life is one atomic-bool probe every
/// [`THREAD_BINDING_POLL`] until either the binding lands or the session ends, and
/// the write it guards happens at most once.
///
/// **An async task, cancellable, and NOT `spawn_blocking` — measured.** The first
/// version was a blocking-pool thread sleeping between polls, and a blocking task
/// cannot be aborted: it held the host's exit for the whole of
/// [`RUNTIME_SHUTDOWN_BUDGET`] after teardown had finished and the run dir had
/// already been swept. The lifecycle suite caught it exactly there — a host still
/// referencing a run directory that no longer existed. So the poll is a
/// `tokio::time::sleep` the teardown's `abort` can interrupt, and only the write
/// itself — one file lock and one fsync, once per session — goes to the blocking
/// pool, where that kind of work belongs.
///
/// A failed write is retried on the next poll rather than reported. There is
/// nowhere to report it to — the TUI owns the tty and host chatter over it
/// corrupts the user's display — and a lock held by another writer for one poll is
/// the ordinary case this would otherwise give up on.
fn spawn_thread_binding_watcher(uid: String, bound: BoundThreadProbe) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if bound() {
                let for_write = uid.clone();
                let wrote =
                    tokio::task::spawn_blocking(move || note_thread_bound(&for_write).is_ok())
                        .await
                        .unwrap_or(false);
                if wrote {
                    return;
                }
            }
            tokio::time::sleep(THREAD_BINDING_POLL).await;
        }
    })
}

/// The record write [`spawn_thread_binding_watcher`] makes, under the same bounded
/// lock every other host write takes.
fn note_thread_bound(uid: &str) -> Result<()> {
    let lock = crate::codex_launch::LaunchLock::acquire_bounded(uid, RECORD_LOCK_BUDGET)?;
    crate::codex_launch::note_codex_thread_bound(&lock, uid)
        .context("recording that a thread bound in the launch record")
}

/// The fixed-width frame a fenced child writes to report its own pid (A11.1).
///
/// Fixed width, never a delimiter scan, because the parent's copy of the write end
/// is still open while it sits inside `spawn()` — so the read end can never see
/// EOF, and a read-to-EOF here would deadlock the rendezvous rather than end it.
const FENCE_ID_LEN: usize = 16;

/// The single byte that releases a fenced child. Anything else — including EOF,
/// which is what a SIGKILLed host produces — means "never exec".
const FENCE_GO: u8 = b'G';

/// A fenced child that is abandoned exits with this instead of `execve`-ing.
/// Matches the exec gate's own inert exit, for one meaning per number.
const FENCE_ABANDONED_EXIT: i32 = 70;

/// Write a pid into a fixed-width ASCII frame without allocating.
///
/// `pre_exec` runs between `fork` and `execve` in a process that has copied a
/// multi-threaded runtime's address space, so only async-signal-safe work is legal
/// there. `format!` allocates and the allocator lock may have been held by another
/// thread at the instant of the fork; this is the reason the frame is built by
/// hand rather than with the obvious `format!("{pid:<16}")`.
fn encode_fence_pid(pid: i32, out: &mut [u8; FENCE_ID_LEN]) {
    *out = [b' '; FENCE_ID_LEN];
    let mut digits = [0u8; FENCE_ID_LEN];
    let mut n = pid.max(0) as u64;
    let mut len = 0;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
        if n == 0 {
            break;
        }
    }
    for i in 0..len {
        out[i] = digits[len - 1 - i];
    }
}

fn decode_fence_pid(frame: &[u8; FENCE_ID_LEN]) -> Option<i32> {
    let text = std::str::from_utf8(frame).ok()?.trim();
    text.parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// A pipe whose ends are **owned** by the caller, for the fence.
///
/// `OwnedFd` rather than a bare `RawFd` pair because these descriptors carry
/// meaning by being closed: the write end of the release pipe closing is what tells
/// an unrecorded child to die, and the parent's copies closing are what stop the
/// releaser blocking forever on a spawn that failed. Every one of those closes used
/// to be a hand-written `libc::close` on a normal path only — so a panic anywhere
/// between them (a releaser panic is the reachable one) leaked the descriptor and
/// left the child parked on a pipe that would never reach EOF. Ownership makes the
/// close happen on every path, and the load-bearing ones are still written out
/// explicitly as `drop`s so the ORDER stays visible.
fn fence_pipe() -> Result<(std::os::unix::io::OwnedFd, std::os::unix::io::OwnedFd)> {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a live two-element array, which is what `pipe(2)` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("creating a spawn-fence pipe");
    }
    // SAFETY: both descriptors were just created by `pipe(2)` and are owned by
    // nothing else.
    Ok(unsafe {
        (
            std::os::unix::io::OwnedFd::from_raw_fd(fds[0]),
            std::os::unix::io::OwnedFd::from_raw_fd(fds[1]),
        )
    })
}

/// Read exactly `buf.len()` bytes, retrying short reads and `EINTR`.
/// `false` on EOF or a hard error.
///
/// # Safety
/// `fd` must be a readable descriptor owned by the caller.
unsafe fn fence_read_exact(fd: std::os::unix::io::RawFd, buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf.as_mut_ptr().add(got) as *mut libc::c_void,
                buf.len() - got,
            )
        };
        if n > 0 {
            got += n as usize;
            continue;
        }
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return false;
    }
    true
}

/// Write all of `buf`, retrying short writes and `EINTR`.
///
/// # Safety
/// `fd` must be a writable descriptor owned by the caller.
unsafe fn fence_write_all(fd: std::os::unix::io::RawFd, buf: &[u8]) -> bool {
    let mut sent = 0;
    while sent < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf.as_ptr().add(sent) as *const libc::c_void,
                buf.len() - sent,
            )
        };
        if n > 0 {
            sent += n as usize;
            continue;
        }
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return false;
    }
    true
}

/// Spawn `cmd` **inert** and release it only once its identity is durable (A11.1).
///
/// This closes the spawn→record window at the root. Before it, the child was
/// already running the target program by the time its pid could be read, so a host
/// SIGKILLed in that interval left a live codex process nobody had written down and
/// nobody could later name. Now the child parks in `pre_exec` — after `fork`, before
/// `execve` — until the host has written `(pid, birth, pgid)` into the launch record.
/// If the host dies first, the release pipe's last writer goes with it, the child
/// reads EOF and `_exit`s **without ever becoming codex**. Fail-closed: an
/// unrecorded child is not a leak, it is a process that never existed.
///
/// **Why a second thread rather than releasing in line.** Measured: `spawn()` does
/// not return until the child has exec'd — the parent blocks reading the CLOEXEC
/// error pipe std uses to report `execve` failure. So a fence released after
/// `spawn()` returns can never be released at all; the obvious in-line version
/// deadlocks (measured, both ways). The child therefore reports its own pid through
/// a pipe while still fenced, and a releaser thread does the recording and the
/// release while the calling thread is still inside `spawn()`.
///
/// **Why the identity is trustworthy pre-`execve`.** Measured: a birth stamp read
/// while the child is fenced is byte-identical to the one read after it execs, and
/// its pgid is already final because `process_group(0)` is applied *before* the
/// `pre_exec` closure runs. So this records exactly what the custodian will later
/// verify — the module doc's `[UNVERIFIED]` note on `execve` no longer applies here.
///
/// **What this does NOT touch**, deliberately: stdio and process-group inheritance.
/// The fence adds a `pre_exec` closure and nothing else, so the TUI still inherits
/// the pane's tty and the host's process group — the measured dead-keyboard
/// constraint at its spawn site is untouched — and the app-server still gets its own
/// group. That is the whole reason this is a host-local fence rather than the D6
/// exec gate, which hardcodes both.
fn spawn_fenced(cmd: &mut Command, args: &HostArgs, role: &str) -> Result<Child> {
    use std::os::unix::io::AsRawFd;
    let (id_r, id_w) = fence_pipe()?;
    let (go_r, go_w) = fence_pipe()?;
    // The raw numbers the CHILD will use. They name the child's own inherited
    // copies after the fork, and `pre_exec` may not allocate or run a destructor,
    // so the closure captures plain integers — the owners below stay alive in the
    // parent until after `spawn()` has forked, which is what keeps them valid.
    let (child_id_r, child_id_w) = (id_r.as_raw_fd(), id_w.as_raw_fd());
    let (child_go_r, child_go_w) = (go_r.as_raw_fd(), go_w.as_raw_fd());

    // SAFETY: the closure runs between `fork` and `execve`. Everything it calls —
    // `getpid`, `read`, `write`, `close`, `_exit` — is async-signal-safe, and the
    // frame is built without allocating (see `encode_fence_pid`).
    unsafe {
        cmd.pre_exec(move || {
            let (id_r, id_w) = (child_id_r, child_id_w);
            let (go_r, go_w) = (child_go_r, child_go_w);
            // Drop the ends this side must not hold. Closing the WRITE end of the
            // release pipe is load-bearing, not tidiness: while the child holds a
            // writer open it is itself a writer, so the read below could never see
            // the EOF that a dead host is supposed to produce.
            libc::close(id_r);
            libc::close(go_w);

            let mut frame = [0u8; FENCE_ID_LEN];
            encode_fence_pid(libc::getpid(), &mut frame);
            if !fence_write_all(id_w, &frame) {
                libc::_exit(FENCE_ABANDONED_EXIT);
            }
            libc::close(id_w);

            let mut go = [0u8; 1];
            if !fence_read_exact(go_r, &mut go) || go[0] != FENCE_GO {
                // The host died, or refused to record us. Never become codex.
                libc::_exit(FENCE_ABANDONED_EXIT);
            }
            libc::close(go_r);
            Ok(())
        });
    }

    // Read HERE, on the calling thread, and carried into the releaser: both of
    // these are per-thread test state, and the releaser is a thread of its own.
    #[cfg(test)]
    let kill_after_go = take_kill_after_go_fault();
    #[cfg(test)]
    let test_root = crate::codex_launch::this_thread_sessions_root();

    let (spawned, recorded) = std::thread::scope(|scope| {
        // Both ends this thread is responsible for are MOVED in, so they are closed
        // when it ends — including when it ends by panicking. The child is parked in
        // `read(go_r)`, and a releaser that panicked while still holding `go_w` open
        // would leave it parked there for ever, with the caller still blocked inside
        // `spawn()` and so never reaching the `join()` that reports the panic.
        let releaser = scope.spawn(
            move || -> Result<protocol::proc_identity::ProcessIdentity> {
                #[cfg(test)]
                crate::codex_launch::adopt_test_sessions_root(test_root);
                let id_r = id_r;
                let go_w = go_w;
                let mut frame = [0u8; FENCE_ID_LEN];
                // SAFETY: `id_r` is owned by this thread for the whole call.
                if !unsafe { fence_read_exact(id_r.as_raw_fd(), &mut frame) } {
                    bail!("the {role} child never reported its identity through the spawn fence");
                }
                let pid = decode_fence_pid(&frame)
                    .ok_or_else(|| anyhow!("the {role} child reported an unreadable pid frame"))?;
                let outcome = record_child_pid(args, role, pid);
                if outcome.is_ok() {
                    // SAFETY: `go_w` is owned by this thread and dropped just below.
                    if !unsafe { fence_write_all(go_w.as_raw_fd(), &[FENCE_GO]) } {
                        // The child is gone; it cannot have exec'd, since only this
                        // write releases it.
                        bail!("releasing the fenced {role} child failed");
                    }
                    // The window round-2 finding 2 measured, staged from inside it.
                    // A released child that dies before `spawn()` gets back out is
                    // indistinguishable, to `spawn()`, from one that exec'd — and no
                    // test can schedule that death from outside this thread.
                    //
                    // The whole block is `cfg(test)`, not merely guarded by a flag
                    // that is always false in production: a `kill` this process can
                    // aim at its own child has no business existing in the shipped
                    // binary at all, however unreachable it is.
                    #[cfg(test)]
                    if kill_after_go {
                        // SAFETY: a signal to the pid this fence just released.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                        // SIGKILL is delivered asynchronously, and the racing
                        // question is whether the child gets to `execve` first —
                        // which is the very race this stages, so it must not be left
                        // to chance in a test. Wait for the death to have LANDED
                        // before letting the caller out of `spawn()`. The seam is
                        // establishing its own precondition, not asserting anything.
                        let until = Instant::now() + Duration::from_secs(5);
                        while child_image(pid).is_some() && Instant::now() < until {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
                // Closing without a GO is what tells an unrecorded child to die, so the
                // close stays written out rather than left to the end of the scope.
                drop(go_w);
                outcome
            },
        );

        let spawned = cmd.spawn();
        // The child has forked and owns its own copies; the parent's are what would
        // otherwise keep the releaser blocked forever if the spawn failed outright.
        drop(id_w);
        drop(go_r);
        let recorded = releaser
            .join()
            .unwrap_or_else(|_| Err(anyhow!("the {role} spawn-fence releaser panicked")));
        (spawned, recorded)
    });

    let child = spawned.with_context(|| format!("spawning the {role} child"))?;
    // Checked AFTER the spawn is unwrapped so the `Child` exists to be dropped —
    // `kill_on_drop` then reaps a child that was released but could not be used.
    let identity = recorded?;
    // A11.1, readiness half — and `spawn()` returning `Ok` is NOT the proof.
    //
    // It was taken to be: the parent blocks on std's CLOEXEC error pipe until the
    // exec either succeeds or reports its errno, so `Ok` was read as "the exec
    // succeeded". What actually reaches the parent is the pipe CLOSING, and a pipe
    // closes for two reasons — the exec that set `O_CLOEXEC` on it, or the child
    // dying with every descriptor it held. **Measured** (round-2 finding 2): a child
    // SIGKILLed after the GO byte and before `execve` gives `spawn()` → `Ok(pid)`,
    // `recorded` → `Ok`, and a process that never became codex. The interval is real
    // — the fence releases the child and then the parent has to get back out of
    // `spawn()` — and everything downstream of this point would have certified it.
    //
    // What IS distinguishable, measured on Darwin: the child's IMAGE. Before
    // `execve` it is still this host's own binary (the fork's inherited image);
    // after `execve` it is the program that was spawned. `proc_pidpath` reports the
    // first as our own path, the second as the target's, and a child that died
    // reports nothing at all (`ESRCH`). Asked together with liveness, which is what
    // binds the pid to OUR child rather than to whoever the kernel handed the number
    // to next — the birth stamp survives `execve` unchanged (measured), so the
    // identity recorded pre-exec is still the right question post-exec.
    //
    // Written down rather than kept, because the process that needs it is the
    // COORDINATOR: it is deciding whether to certify a session as `Ready`, and every
    // other fact it can see — listeners serving, host alive, both roles recorded —
    // is already true while the TUI is still parked in the fence.
    prove_past_execve(&identity, role)?;
    confirm_child_exec(args, role, &identity)?;
    Ok(child)
}

#[cfg(test)]
thread_local! {
    /// One-shot: the next [`spawn_fenced`] kills its child in the instant after the
    /// GO byte — the measured window in which `spawn()` still returns `Ok` and the
    /// child never became the target program. Thread-local like every other fault
    /// seam here, and READ ON THE CALLER'S THREAD before the fence starts, because
    /// the releaser that acts on it is a thread of its own.
    static KILL_AFTER_GO: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the one-shot post-GO kill (see [`spawn_fenced`]).
#[cfg(test)]
fn kill_next_child_after_go() {
    KILL_AFTER_GO.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_kill_after_go_fault() -> bool {
    KILL_AFTER_GO.with(|armed| armed.replace(false))
}

/// The child's own image, or `None` if it has no image to report (it is gone).
fn child_image(pid: i32) -> Option<std::path::PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is exactly the size this call documents and outlives it.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(std::path::PathBuf::from(std::ffi::OsString::from(
        String::from_utf8(buf).ok()?,
    )))
}

/// Whether two image paths name the **same file**, not merely the same spelling
/// (round-3 finding 9).
///
/// The discriminator below asks "is the child still running MY image?", and it used
/// to ask it by comparing [`std::env::current_exe`]'s bytes against `proc_pidpath`'s.
/// Those two are not obliged to agree on spelling for one file. Darwin's firmlinks
/// alias the data volume, so the very same inode is reachable as `/Users/…` and as
/// `/System/Volumes/Data/Users/…` — measured on this platform: the two paths compare
/// UNEQUAL as strings and identical as `(st_dev, st_ino)`. A byte compare that says
/// "different image" for one file is the wrong answer in the unsafe direction: it
/// reads a child still parked pre-`execve`, running our own binary under the other
/// spelling, as PROOF that it exec'd.
///
/// So identity is compared, not spelling. The path equality stays as a fast path
/// (it is sufficient, never necessary), and a `stat` that cannot be taken is an
/// `Err` rather than a `false`: an image question that cannot be answered must
/// refuse, exactly like the liveness question above it.
fn same_image(ours: &std::path::Path, theirs: &std::path::Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    if ours == theirs {
        return Ok(true);
    }
    let a = std::fs::metadata(ours)
        .with_context(|| format!("stat of this host's own image {}", ours.display()))?;
    let b = std::fs::metadata(theirs)
        .with_context(|| format!("stat of the child's image {}", theirs.display()))?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

/// Refuse unless this child is **alive and past `execve`** (A11.1, readiness half).
///
/// Two questions, and both are needed:
///
///   * **Alive**, by the recorded birth identity rather than by the bare pid, so a
///     recycled number cannot answer for a child that is gone. A child that died —
///     before the exec or immediately after it — must not be certified either way:
///     `Ready` is a claim about a session that is running.
///   * **Past `execve`**, by the image differing from this host's own. A fenced
///     child that has not exec'd is still running the host's binary, because that
///     is what `fork` gave it; the target's image is the first thing about it that
///     is not inherited. Compared by file identity rather than by path spelling —
///     see [`same_image`] for the aliasing this closes.
///
/// Fail-closed on every unreadable answer, including our own path: an image
/// question that cannot be asked is not an image question that was answered.
fn prove_past_execve(
    identity: &protocol::proc_identity::ProcessIdentity,
    role: &str,
) -> Result<()> {
    use protocol::proc_identity::{liveness, Liveness};
    if liveness(identity) != Liveness::Alive {
        bail!(
            "refusing to confirm {role}'s exec: the child (pid {}) is not proven live",
            identity.pid
        );
    }
    let ours = std::env::current_exe().context("reading this host's own image path")?;
    let Some(theirs) = child_image(identity.pid) else {
        bail!(
            "refusing to confirm {role}'s exec: the child (pid {}) reports no image",
            identity.pid
        );
    };
    if same_image(&ours, &theirs)
        .with_context(|| format!("comparing {role}'s image against this host's own"))?
    {
        bail!(
            "refusing to confirm {role}'s exec: the child (pid {}) is still running this \
             host's own image ({}), so it has not passed execve",
            identity.pid,
            ours.display()
        );
    }
    Ok(())
}

/// Record a spawned child's `(pid, birth, pgid)` in the launch record.
///
/// **Launch-fatal, not best-effort.** An earlier version logged and continued, on
/// the reasoning that the host's own teardown stops these children anyway. That
/// reasoning is exactly backwards: this record exists for the paths where the host
/// does NOT get to run teardown — SIGKILL, or an abort under `panic = "abort"` —
/// and on those paths the custodian is the only actor left. A session that comes
/// up with an unrecorded child is one whose processes nobody can name afterwards,
/// which is the leak this whole chunk is about. So a failure here aborts the
/// launch: no session is better than an unreapable one.
///
/// A child whose birth or pgid cannot be read is an error rather than a partial
/// entry: a `(pid, ?)` record is a bare number, and this codebase does not signal
/// bare numbers.
///
/// A11.1: called by [`spawn_fenced`] while the child is still parked before
/// `execve`, so "recorded" now strictly precedes "running the target program".
fn record_child_pid(
    args: &HostArgs,
    role: &str,
    pid: i32,
) -> Result<protocol::proc_identity::ProcessIdentity> {
    let (Some(birth), Some(pgid)) = (
        protocol::proc_identity::read_birth_identity(pid),
        protocol::proc_identity::read_pgid(pid),
    ) else {
        bail!("could not read {role}'s (pid {pid}) birth identity and process group");
    };
    let identity = protocol::proc_identity::ProcessIdentity { pid, birth };
    let entry = crate::codex_launch::ChildEntry {
        role: role.to_string(),
        identity,
        pgid,
        nonce: args.nonce.clone(),
        argv_hash: String::new(),
        // Stamped by `record_host_child` from the verified lease holder.
        recorded_by: None,
        // A11.1: NOT yet — the child is still parked before `execve`. Set by
        // `confirm_child_exec` once `spawn()` proves it got past it.
        exec_confirmed: false,
    };
    let me = crate::codex_launch::require_current_identity()?;
    // A SHORT lock wait, deliberately, because this call sits inside a window.
    //
    // The interval is bounded by how long this takes, and the lock is the only part
    // that can stretch — so it gets a tight budget rather than the five seconds the
    // non-urgent writers use. A launch that cannot get the lock in a second is
    // failed, which is the same fail-closed answer as any other recording failure.
    //
    // A11.1: for a child spawned through [`spawn_fenced`] that window is no longer a
    // leak. The child is parked before `execve` until this write returns, so a host
    // SIGKILLed here leaves a process that has not become codex and that dies on the
    // release pipe's EOF. The budget still matters — it bounds how long the launch
    // is held inert — but missing it now costs a slow launch, not a stray process.
    let lock = crate::codex_launch::LaunchLock::acquire_bounded(&args.uid, RECORD_LOCK_BUDGET)?;
    crate::codex_launch::record_host_child(&lock, &args.uid, &me, entry)
        .with_context(|| format!("recording {role} in the launch record"))?;
    Ok(identity)
}

/// Mark a recorded child **past `execve`** (A11.1, readiness half).
///
/// Called only once [`prove_past_execve`] has SUCCEEDED — never on the strength of
/// `Command::spawn()` returning `Ok`, which is measurably not the same claim: the
/// parent blocks on std's CLOEXEC error pipe until the exec has succeeded (EOF) or
/// reported its errno, and the child's DEATH closes that pipe too, so a child
/// SIGKILLed between the fence's GO and `execve` yields `Ok` having never become the
/// program. See [`crate::codex_launch::ChildEntry::exec_confirmed`] for why the
/// coordinator cannot commit `Ready` without this, and why nothing else it looks at
/// can stand in.
///
/// Launch-fatal for the same reason the recording is: a child this host cannot
/// confirm is one the coordinator will never be able to certify, so failing here
/// costs a refused launch rather than a session that waits out its deadline.
fn confirm_child_exec(
    args: &HostArgs,
    role: &str,
    identity: &protocol::proc_identity::ProcessIdentity,
) -> Result<()> {
    let me = crate::codex_launch::require_current_identity()?;
    let lock = crate::codex_launch::LaunchLock::acquire_bounded(&args.uid, RECORD_LOCK_BUDGET)?;
    crate::codex_launch::confirm_host_child_exec(&lock, &args.uid, &me, role, identity)
        .with_context(|| format!("confirming {role}'s exec in the launch record"))
}

/// The session proper, run under the teardown guard so it may use `?` freely.
async fn drive(
    session: &mut Session,
    args: &HostArgs,
    paths: &Paths,
    signals: &mut Signals,
    as_stderr: &mut std::fs::File,
) -> Result<Outcome> {
    // --- Step 1: prove the app-server's socket (cancellable) ----------------
    tokio::select! {
        biased;
        name = signals.recv() => return Ok(Outcome::Signalled(name)),
        ready = wait_for_appserver_socket(
            &mut session.appserver,
            &mut session.appserver_reaped,
            &paths.as_sock,
            as_stderr,
            BRINGUP_TIMEOUT,
        ) => ready?,
    }

    // --- Step 2: the broker in front of the app-server ----------------------
    let factory = WsUdsUpstreamFactory::new(paths.as_sock.clone());
    let broker = Broker::new(
        paths.tui_sock.clone(),
        paths.ccd_sock.clone(),
        args.fingerprint.clone(),
        factory,
    )
    .with_event_sink(Arc::clone(&session.log))
    // The measurement instrument, off unless a live harness asked for it by setting
    // `CC_CODEX_FRAME_TEE`, and — the containment that matters — not COMPILED unless
    // this crate was built with its own `frame-tee` feature.
    //
    // Gated on CodeConnect's feature rather than only on the broker's, and the
    // difference is not cosmetic: cargo unifies features across a dependency graph, so
    // some other crate in a future build could turn on `codex-broker/frame-tee` while
    // CodeConnect's own feature stays off. `FrameTee::from_env` would then read the
    // environment even though nothing in this crate asked it to. Gating the CALL here
    // makes the invariant local to the binary a user runs: no feature, no call, no read.
    //
    // A named path that will not open is fatal rather than silently off: a harness that
    // asked for a capture and got an empty file would draw conclusions from frames
    // nobody recorded. See `codex_broker::frame_tee`.
    .with_frame_tee(if cfg!(feature = "frame-tee") {
        match codex_broker::FrameTee::from_env() {
            Ok(tee) => tee,
            Err(why) => return Ok(Outcome::Fatal(why)),
        }
    } else {
        codex_broker::FrameTee::off()
    });
    // Taken LAST, after both builder methods above: they replace fields through
    // `Arc::get_mut` and would panic on a context this has already cloned. The
    // handle has to outlive the broker, since `serve` consumes it on the next line.
    let bound_thread = broker.bound_thread_probe();
    session.broker = Some(tokio::spawn(broker.serve()));
    session.bound_thread = Some(Arc::clone(&bound_thread));
    // The coordinator is holding the launcher at the record until this says a
    // thread bound, so start looking now rather than after the TUI is spawned:
    // nothing binds before the TUI exists, but the watcher's first successful poll
    // is what the healthy launch's extra ~225 ms is spent waiting for.
    session.thread_watcher = Some(spawn_thread_binding_watcher(args.uid.clone(), bound_thread));

    // Wait until BOTH legs are bound (or the serve task ended early — a bind
    // failure). Any doubt aborts the whole host. Cancellable, like every wait.
    let listeners = match session.broker.as_ref() {
        Some(task) => {
            wait_for_broker_listeners(task, &paths.tui_sock, &paths.ccd_sock, BRINGUP_TIMEOUT)
        }
        None => bail!("internal error: the broker task handle went missing during bring-up"),
    };
    tokio::select! {
        biased;
        name = signals.recv() => return Ok(Outcome::Signalled(name)),
        bound = listeners => bound?,
    }

    // --- Step 3: the real interactive TUI, foreground, inheriting our tty ----
    //
    // A7.1, checked a SECOND time and not because the first was in doubt. Steps 1
    // and 2 stand between the two spawns — the app-server's bring-up and the
    // broker's bind, seconds of real time during which the path is not being
    // watched — so the app-server's check says nothing about the file this is about
    // to exec. Each exec gets its own verify, immediately before it.
    //
    // On a blocking thread, and raced against a signal, because by this point the
    // broker is SERVING. The verify is a whole-file read — measured at 0.478 / 0.459
    // / 0.460 s for the 220 MB standalone codex in a release build, ~8 s unoptimised
    // — and running that inline would park a runtime worker for its duration and,
    // worse, leave the host deaf to SIGTERM for just as long. The host's whole
    // contract is a bounded, signal-responsive exit; a guard that suspends it is not
    // an acceptable guard, however correct. So this waits exactly like every other
    // wait in this function.
    //
    // The verify FREEZES the bytes immutable and returns a guard that keeps them
    // frozen; `frozen` below holds it across the spawn, so no installer or update can
    // swap or rewrite the pathname between this check and the TUI's `execve`. The
    // app-server verify above records the full accounting: what that closes (the
    // update race, completely), what it does not (a hostile same-uid peer, which is
    // out of scope and which macOS gives no way to exclude), and the post-clear
    // demand-paging residual.
    let (codex, codex_sha256) = (args.codex.clone(), args.codex_sha256.clone());
    let verify = tokio::task::spawn_blocking(move || {
        crate::codex::verify_codex_identity(
            &codex,
            &codex_sha256,
            "immediately before the TUI spawn",
        )
    });
    let frozen = tokio::select! {
        biased;
        name = signals.recv() => return Ok(Outcome::Signalled(name)),
        // A join error here is a panic inside the verify. Failing closed on it is
        // the only honest reading: a check that crashed did not pass. The `Ok` is the
        // held freeze (see `verify_codex_identity`), kept across the spawn below.
        verified = verify => verified
            .context("the codex identity check panicked before the TUI spawn")??,
    };
    // **The `--` fence, applied at the exec that actually runs.** Not in the coordinator
    // and not at parse time: the process whose spawn this is, is the one that has to be
    // sure — the same reason this function re-validates the passthrough rather than
    // trusting its parent. See `codex::fence_positionals` for the measurement that makes
    // a positional unable to dispatch, and for why the refusal table is no longer what
    // the safety rests on.
    let fenced = crate::codex::fence_positionals(&args.tui_args)
        .map_err(|refusal| anyhow!("refused passthrough TUI argument: {refusal}"))?;
    let mut tui_cmd = Command::new(&args.codex);
    tui_cmd
        .arg("--remote")
        .arg(format!("unix://{}", paths.tui_sock.display()))
        // **The sandbox CodeConnect owns, actually set on the process that asks for
        // it.** The reserved grammar refuses a user-supplied `--sandbox` with
        // "`--sandbox` is set by CodeConnect (the session sandbox policy)" — and
        // until this line that sentence was FALSE. The TUI was spawned with no
        // sandbox flag at all, so it chose its own from the project's `trust_level`
        // in the user's `~/.codex/config.toml`: measured, a `trust_level =
        // "trusted"` project produced a `thread/start` asserting
        // `"sandbox":"workspace-write"` against a fingerprint pinned `read-only`,
        // and the broker refused it (`params.sandbox` Conflict) — a launch the user
        // could do nothing about, killed by a knob the grammar claimed to own and
        // nobody set. The grammar's claim is now true by construction: the flag is
        // on the argv of the process that sends the request, so the request carries
        // this mode whatever the config says.
        //
        // The value is `args.fingerprint.sandbox` — the very string the broker
        // asserts, handed to this host on its own argv — and not a second copy of
        // `codex::LAUNCH_SANDBOX`. A separate constant could drift from the pin; a
        // shared field cannot. If it is ever a mode codex does not know, codex
        // refuses at parse time (`invalid value '…' for '--sandbox <SANDBOX_MODE>'
        // [possible values: read-only, workspace-write, danger-full-access]`) and
        // no session comes up, which is the correct outcome for a fingerprint the
        // TUI could not have honoured.
        //
        // Measured on codex 0.153: `--sandbox` is accepted alongside `--remote` (a
        // bad value gives the `invalid value` error above; an unrecognised flag
        // gives `unexpected argument` instead, so the flag really is parsed here).
        // There is deliberately no matching flag on the app-server spawn — `codex
        // app-server --help` contains no `--sandbox` at all, because the sandbox is
        // a PER-THREAD parameter carried on `thread/start`, which the TUI sends.
        // Nobody should go looking for the other half; this is the whole of it.
        //
        // Before the fenced passthrough, so it is a flag rather than something that
        // could be read as one of the TUI's own positionals.
        .arg("--sandbox")
        .arg(&args.fingerprint.sandbox)
        .args(&fenced)
        .env("CODEX_HOME", &args.codex_home)
        // Inherit stdio (the pane's tty) so this IS the session the user drives —
        // and, deliberately, inherit the host's PROCESS GROUP too.
        //
        // The app-server below gets its own group so cleanup can `killpg` a
        // recorded pgid. The TUI must not, and this was measured rather than
        // assumed: with `.process_group(0)` the real codex TUI comes up with
        // `pgid == its own pid` while the pane's terminal keeps `tpgid == the
        // host's group` — i.e. the TUI is a BACKGROUND process group on the tty it
        // is supposed to own. It does not get stopped (it evidently blocks
        // SIGTTIN), so nothing looks broken from the outside: the pane renders and
        // the broker handshake succeeds. What breaks is the user's keyboard, which
        // keeps going to the foreground group. Making it correct would mean the
        // host also handing over terminal control with `tcsetpgrp`, and restoring
        // it on every exit path — real terminal-ownership machinery, for a benefit
        // already obtained without it.
        //
        // Nothing is lost for cleanup: the TUI's identity is recorded as
        // (pid, birth, pgid) like the app-server's, and the custodian signals it by
        // that verified identity. A group kill buys nothing here anyway, since the
        // TUI's group would be the host's own.
        //
        // A11.1: the fence below preserves every one of those measured facts. It
        // installs a `pre_exec` closure and touches nothing else — no stdio
        // redirection, no `setpgid` — so the TUI still comes up on the pane's tty in
        // the host's foreground process group, and the keyboard still works.
        .kill_on_drop(true);
    // A11.1: fenced, so the TUI's identity is durable before it can become codex.
    // The spawn and the record are now one step, which is what removes the interval
    // in which a SIGKILLed host left a live, unrecorded TUI on the user's pane.
    let tui = match spawn_fenced(&mut tui_cmd, args, "tui") {
        Ok(child) => child,
        // `frozen` drops here too, clearing the freeze — nothing became codex.
        Err(err) => {
            return Err(err)
                .with_context(|| format!("spawning the codex TUI ({})", args.codex.display()))
        }
    };
    // The TUI is past `execve` (spawn_fenced proved it); the frozen bytes are the
    // bytes that ran. Clear the freeze — the post-clear tail is the signed-binary
    // demand-paging residual documented at the app-server spawn.
    drop(frozen);
    session.tui = Some(tui);

    // --- Step 4: race the two children, the broker, and a host signal -------
    // Destructured so all three can be polled in one `select!`.
    let Session {
        appserver,
        appserver_reaped,
        tui,
        tui_reaped,
        broker,
        ..
    } = session;
    let Some(tui) = tui.as_mut() else {
        bail!("internal error: the TUI child handle went missing after spawn")
    };
    let Some(broker) = broker.as_mut() else {
        bail!("internal error: the broker task handle went missing after bring-up")
    };

    // A non-consuming probe for the same task the broker arm below awaits. The arm
    // takes `&mut *broker`, which borrows the handle for the whole `select!`, so the
    // other arms cannot ask it anything — this handle can, and `is_finished()` on
    // it means exactly "the broker task has completed".
    let broker_probe = broker.abort_handle();

    // `biased` and ordered fatal-first on purpose: if the app-server dies and the
    // TUI notices and exits in the same instant, both arms are ready, and the
    // truthful reading of that race is "the upstream died", not "the user quit".
    // An unbiased `select!` would pick pseudo-randomly and report a clean exit
    // half the time.
    //
    // But ordering is NOT sufficient, which is why every arm below is thin and
    // hands three freshly observed states to [`resolve_outcome`]: `biased` decides
    // the order in which futures are *polled*, and a task that answered `Pending`
    // early in a pass can finish before a later arm answers `Ready` in that same
    // pass. Snapshotting after the fact is what closes that window.
    Ok(tokio::select! {
        biased;
        status = appserver.wait() => {
            // A `wait()` Err proves nothing about the child's state, so the reaped
            // flag is set only on a real status: teardown then still kills+reaps.
            if status.is_ok() {
                *appserver_reaped = true;
            }
            resolve_outcome(
                Fired::AppServer,
                AppServerState::from_wait(status),
                broker_probe.is_finished(),
            )
        }
        joined = &mut *broker => {
            // The broker IS the security boundary between the TUI and the real
            // app-server. If its serve task ends, the boundary is gone and the
            // session is over. Both children are killed by the teardown that follows.
            let end = match joined {
                Ok(Ok(())) => BrokerEnd::Stopped,
                Ok(Err(err)) => BrokerEnd::Failed(err.to_string()),
                Err(err) => BrokerEnd::Abnormal(err.to_string()),
            };
            resolve_outcome(Fired::Broker(end), AppServerState::Alive, true)
        }
        status = tui.wait() => {
            if status.is_ok() {
                *tui_reaped = true;
            }
            let report = match status {
                Ok(status) => WaitReport::Exited(status),
                Err(err) => WaitReport::Undetermined(err.to_string()),
            };
            // Re-observe BOTH of the other two parts before calling this benign: a
            // TUI exiting *because* its upstream or its broker vanished looks
            // identical to a user quitting from this arm alone.
            let as_state = AppServerState::from_try_wait(appserver.try_wait());
            if matches!(as_state, AppServerState::Exited(_)) {
                *appserver_reaped = true;
            }
            resolve_outcome(Fired::Tui(report), as_state, broker_probe.is_finished())
        }
        name = signals.recv() => {
            // The SAME observation the TUI arm makes, for the same reason: a signal
            // does not tell you anything about the app-server, and fabricating
            // `Alive` here would let an upstream death racing a SIGTERM be reported
            // as a clean 130 ("ended by request"). Observe it, then let
            // `resolve_outcome` decide.
            let as_state = AppServerState::from_try_wait(appserver.try_wait());
            if matches!(as_state, AppServerState::Exited(_)) {
                *appserver_reaped = true;
            }
            resolve_outcome(Fired::Signal(name), as_state, broker_probe.is_finished())
        }
    })
}

// ------------------------------------------------------------------ teardown

/// Everything the host must dispose of, and the single place that disposes of it.
///
/// Constructed the instant the app-server exists; the TUI and the broker task
/// join it as they come up. [`Self::teardown`] is called exactly once, from
/// [`run_session`], and covers **every** exit path — bring-up abort, signal,
/// fatal, and clean TUI exit alike — because `drive`'s whole body sits between the
/// spawn and that one call.
struct Session {
    appserver: Child,
    /// Whether this child's status was already collected — set only when a `wait()`
    /// or `try_wait()` returned a real status, never on an `Err` (which proves
    /// nothing about the child, so teardown must still kill and reap).
    ///
    /// These two flags express intent, not necessity: probed on tokio 1.53.1,
    /// `start_kill()` on an already-reaped child returns `Ok(())` and `wait()`
    /// replays the cached status, so [`kill_and_reap`] would report success either
    /// way. They exist so that "already reaped" is decided in one place — the arm
    /// that actually observed the exit — rather than inferred from tokio's
    /// forgiveness, which is a behaviour this file would rather not depend on.
    appserver_reaped: bool,
    tui: Option<Child>,
    tui_reaped: bool,
    broker: Option<JoinHandle<std::io::Result<()>>>,
    log: EventSink,
    /// The broker's own answer to "did a thread ever bind here?", kept past the
    /// serve task that `Broker::serve` consumed. `None` until the broker exists —
    /// i.e. only on the bring-up paths that abort before step 2, which never had a
    /// TUI to draw a conclusion about.
    bound_thread: Option<BoundThreadProbe>,
    /// [`spawn_thread_binding_watcher`]'s task, so teardown can end it rather than
    /// leave the runtime's own shutdown to.
    thread_watcher: Option<JoinHandle<()>>,
}

impl Session {
    fn new(appserver: Child, log: EventSink) -> Self {
        Self {
            appserver,
            appserver_reaped: false,
            tui: None,
            tui_reaped: false,
            broker: None,
            log,
            bound_thread: None,
            thread_watcher: None,
        }
    }

    /// Whether a thread ever bound in this session.
    ///
    /// Asked of the broker rather than of the record: the record's bit is written
    /// under a grace that has usually expired by the time a session ends, and this
    /// is the fact itself. Fails closed — no broker means no thread.
    fn thread_ever_bound(&self) -> bool {
        self.bound_thread
            .as_ref()
            .is_some_and(|has_bound| has_bound())
    }

    /// Abort the broker, then make a bounded best-effort to reap both children,
    /// then report anything that could not be proven.
    ///
    /// Order is deliberate. The broker goes first because aborting it is cheap and
    /// because teardown should be orderly: its handle is *awaited* (bounded) here
    /// rather than merely signalled, so the task has normally finished before
    /// anything else happens — and when it does not finish inside
    /// [`BROKER_ABORT_BUDGET`] that is reported rather than waited on forever.
    /// Note the rationale is **not** "the serve task could otherwise re-create a
    /// listener socket ahead of the run-dir sweep" — it cannot; [`Broker::serve`]
    /// binds both listeners once, before its accept loop (the same correction as on
    /// [`BROKER_ABORT_BUDGET`]). Then the TUI, which frees the tty. Then the
    /// app-server.
    ///
    /// Nothing reaches stderr until both children have been dealt with **as far as
    /// this host can prove**, because the TUI owns the terminal and host chatter
    /// over a live TUI corrupts the user's display. That ordering is not a
    /// guarantee that the TUI is gone: an un-proven reap is exactly the case where
    /// it might still be running and holding the tty — and is exactly what these
    /// lines report. Reaping is best-effort throughout: bounded, never assumed,
    /// logged and printed when it cannot be shown, with `kill_on_drop` beneath.
    async fn teardown(&mut self) {
        let mut unproven: Vec<&'static str> = Vec::new();

        // First, and not awaited. It writes nothing the session still needs — the
        // coordinator stopped reading the bit when it stopped waiting — and it is
        // sleeping between polls, so `abort` lands at once. Ending it here rather
        // than leaving it to the runtime's shutdown is what keeps the host's exit
        // as prompt as it was before the watcher existed.
        if let Some(watcher) = self.thread_watcher.take() {
            watcher.abort();
        }

        if let Some(task) = self.broker.take() {
            task.abort();
            // The race's broker-death arm polls this very handle to completion, and
            // awaiting a `JoinHandle` again after it returned `Ready` PANICS
            // ("JoinHandle polled after completion"). On that path there is also
            // nothing left to wait for: the await below exists to make teardown
            // ORDERLY — to not move on to the children while the serve task may
            // still be running — and a task that is already finished satisfies that
            // by definition. (It is emphatically NOT about the serve task
            // re-creating a listener socket ahead of the run-dir sweep; it cannot,
            // per the doc above.) `is_finished()` is the non-consuming check for
            // exactly that, and it is true whenever the handle has been driven to
            // `Ready`.
            if !task.is_finished()
                && tokio::time::timeout(BROKER_ABORT_BUDGET, task)
                    .await
                    .is_err()
            {
                unproven.push("the broker serve task");
            }
        }

        if let Some(tui) = self.tui.as_mut() {
            if !self.tui_reaped && !kill_and_reap(tui, REAP_BUDGET).await {
                unproven.push("the codex TUI");
            }
        }

        if !self.appserver_reaped && !kill_and_reap(&mut self.appserver, REAP_BUDGET).await {
            unproven.push("the codex app-server");
        }

        for what in unproven {
            // Both destinations on purpose: the log is the durable record the
            // custodian and the live gates read, and stderr is what an operator
            // watching the pane sees. `kill_on_drop` is still the net beneath
            // this, but an unproven reap is worth saying out loud.
            let line = format!(
                "codex-host: could not prove {what} was stopped within the teardown budget"
            );
            (self.log)(&line);
            eprintln!("{line}");
        }
    }
}

/// SIGKILL a child and reap it with a bounded poll — never an unbounded `wait`
/// that could wedge teardown. Returns whether the reap was **proven**: a
/// `start_kill` error, a timeout, or a `wait` error all report `false` so the
/// caller can say so rather than assume success.
async fn kill_and_reap(child: &mut Child, budget: Duration) -> bool {
    if child.start_kill().is_err() {
        return false;
    }
    matches!(tokio::time::timeout(budget, child.wait()).await, Ok(Ok(_)))
}

// ------------------------------------------------------------------- signals

/// The three signals that mean "this pane is going away", installed as one unit
/// **before** the first spawn so bring-up itself is interruptible.
struct Signals {
    term: Signal,
    int: Signal,
    hup: Signal,
}

impl Signals {
    fn install() -> Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate()).context("installing the SIGTERM handler")?,
            int: signal(SignalKind::interrupt()).context("installing the SIGINT handler")?,
            // The host runs inside a tmux pane: a closing pane / dying terminal
            // delivers SIGHUP, and without this arm the host would be killed
            // outright and leak both children.
            hup: signal(SignalKind::hangup()).context("installing the SIGHUP handler")?,
        })
    }

    /// Resolve to the name of the first signal delivered.
    ///
    /// Safe to drop mid-poll (every bring-up wait races it): tokio's unix `Signal`
    /// registration outlives the future and holds the pending notification, so a
    /// signal that arrives while this future is being cancelled is still observed
    /// by the next call. `recv()` yields `None` only if the registration is
    /// dropped — impossible here, since `self` owns it for the host's lifetime.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
            _ = self.hup.recv() => "SIGHUP",
        }
    }
}

// ------------------------------------------------------------- bring-up waits

/// Build the broker's [`EventSink`], appending each decision line to `path`.
///
/// The sink writes **only** to the log file, never to stderr: the TUI inherits
/// this process's tty, and broker chatter on stderr would corrupt its display.
///
/// Each line is written **`Debug`-escaped**. The broker's notes quote material
/// that ultimately came off the wire, and a hostile frame must not be able to put
/// a raw CSI sequence into a file an operator will later `cat`. Escaping costs the
/// log its bare-string look and is worth it.
fn file_event_sink(path: &Path) -> Result<EventSink> {
    let file = create_private_log(path)?;
    let file = Arc::new(Mutex::new(file));
    Ok(Arc::new(move |line: &str| {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line:?}");
        }
    }))
}

/// Create a log file that must not already exist, owner-readable only. Paired
/// with the exclusively-created run dir, `create_new` can only fail here if
/// something raced us into our own private directory — which is exactly when the
/// host should refuse.
///
/// Opened `read` as well as `append` so the caller can keep the handle and read
/// the file back through it. The app-server's stderr log needs that: reopening it
/// by path after the child has been spawned would resolve whatever is at the name
/// *then*, and the excerpt path must never depend on that (see
/// [`stderr_excerpt`]). `append` still means every write lands at the end
/// regardless of where a read left the shared offset.
fn create_private_log(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {} fresh (0600)", path.display()))
}

/// Poll until `path` is a real 0600 socket (past the app-server's bind-then-chmod
/// race), bounded by `timeout`. Fails closed — surfacing the child's captured
/// stderr — if the app-server exits before binding or the socket never appears.
///
/// `reaped` is the caller's [`Session::appserver_reaped`] flag, set here when this
/// poll's own `try_wait` collects the child's status — that call **is** the reap,
/// so the teardown that follows must not treat the child as still running.
/// (Probed: tokio caches the status, so a redundant `start_kill`/`wait` would
/// still return `Ok` — this is precision, not a latent bug. It is worth having
/// anyway so `*_reaped` means exactly one thing at every site in [`Session`],
/// rather than "reaped, except on the one path that reaps somewhere else".)
async fn wait_for_appserver_socket(
    child: &mut Child,
    reaped: &mut bool,
    path: &Path,
    stderr: &mut std::fs::File,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                *reaped = true;
                bail!(
                    "codex app-server exited before binding (status {status}). stderr:\n{}",
                    stderr_excerpt(stderr)
                );
            }
            Ok(None) => {}
            // Not proof of exit, but proof we cannot supervise this child. Failing
            // here beats degrading into the timeout below, which would report a
            // misleading "the socket never appeared".
            Err(err) => bail!(
                "cannot determine the codex app-server's state while waiting for its \
                 socket ({err}); failing closed. stderr:\n{}",
                stderr_excerpt(stderr)
            ),
        }
        if is_ready_socket(path, Some(0o600)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "codex app-server socket {} did not appear as a 0600 socket within {:?}. stderr:\n{}",
                path.display(),
                timeout,
                stderr_excerpt(stderr)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until BOTH broker legs are bound sockets, bounded by `timeout`. Fails
/// closed if the serve task ends first (a bind failure) or the deadline passes —
/// a broker whose listeners we cannot prove bound is never handed a TUI.
async fn wait_for_broker_listeners(
    task: &JoinHandle<std::io::Result<()>>,
    tui_sock: &Path,
    ccd_sock: &Path,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if task.is_finished() {
            bail!("the broker serve task ended before binding its listeners");
        }
        if is_ready_socket(tui_sock, None) && is_ready_socket(ccd_sock, None) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "the broker did not bind both legs ({}, {}) within {:?}",
                tui_sock.display(),
                ccd_sock.display(),
                timeout
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether `path` is a bound unix socket — and, when `mode` is given, at exactly
/// that permission (the app-server's 0600). `tokio`/`std` create a listener's
/// socket file atomically at bind, so a socket file appearing IS the "bound"
/// signal; the mode gate additionally waits past the app-server's bind→chmod.
///
/// This is only a *readiness* test, never an *ownership* test. What makes it
/// trustworthy is the run dir having been exclusively created by this process
/// moments earlier — which rules out stale residue and anything left by another
/// uid, but is not a claim about the class invariant 1 puts out of scope. See
/// invariant 1 in the module doc for the actual premise, and `codex::start` (A22)
/// for what that class is.
fn is_ready_socket(path: &Path, mode: Option<u32>) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => match mode {
            Some(want) => (meta.permissions().mode() & 0o777) == want,
            None => true,
        },
        _ => false,
    }
}

/// The tail of a failed child's stderr, for quoting into an error message.
///
/// Takes the **held file handle**, never a path. Two properties follow from that,
/// and neither survives a reopen-by-path:
///
///   * **It cannot be redirected.** The log lives in a directory the app-server
///     can write to, so by the time bring-up fails the *name* may no longer be the
///     file the host created. Reopening it could land on a FIFO or a device node
///     the child planted, and `read()` on one of those blocks — wedging the host
///     inside its own error path, forever, with no timeout above it. An fd opened
///     before the spawn refers to the original inode for good.
///   * **It cannot be made unbounded.** The read is a *seek to the tail* plus at
///     most [`STDERR_EXCERPT_LIMIT`] bytes, so the host never materialises a
///     child-controlled file of arbitrary size in memory just to throw away all
///     but its last 4 KiB.
///
/// **Escaped, for the same reason the event sink is.** This text is child-controlled
/// and every one of its consumers prints it to *stderr* — and a bring-up failure is
/// precisely when the TUI is not up, so stderr is the operator's bare terminal.
/// Truncation is not sanitisation: the excerpt is a tail, so a hostile app-server
/// controls its final bytes exactly and could otherwise replay CSI/OSC sequences
/// (alt-screen switches, `OSC 52` clipboard writes) straight into that terminal.
/// `escape_debug` covers Cc **and** Cf, so nothing that survives can start an
/// escape sequence.
fn stderr_excerpt(file: &mut std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(err) => return format!("<unreadable: {err}>"),
    };
    let limit = STDERR_EXCERPT_LIMIT as u64;
    let start = len.saturating_sub(limit);
    if let Err(err) = file.seek(SeekFrom::Start(start)) {
        return format!("<unreadable: {err}>");
    }
    let mut bytes = Vec::new();
    // `take` is the second bound, beneath the seek: a child appending while this
    // runs must not be able to extend the read past the limit.
    if let Err(err) = Read::take(&mut *file, limit).read_to_end(&mut bytes) {
        return format!("<unreadable: {err}>");
    }
    let body = String::from_utf8_lossy(&bytes).escape_debug().to_string();
    if start > 0 {
        format!("<truncated: last {STDERR_EXCERPT_LIMIT} of {len} bytes>\n{body}")
    } else {
        body
    }
}

/// Assert a socket path is short enough to `bind()`. A run dir long enough to
/// push a socket past SUN_LEN is a bring-up error, surfaced here.
fn assert_sun_len(path: &Path) -> Result<()> {
    let len = path.as_os_str().len();
    if len >= SUN_LEN_LIMIT {
        bail!(
            "socket path {} is {len} bytes; a unix socket path must be < {SUN_LEN_LIMIT} (SUN_LEN) \
             — pass a shorter --run-dir",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// **THE WATCHER IS STILL WATCHING AFTER THE OLD DEADLINE.**
    ///
    /// It used to stop after [`crate::codex_launch::THREAD_BINDING_GRACE`], which
    /// looked equivalent to the coordinator's wait and is not: this timer starts when
    /// the broker does — before leg readiness, before the TUI is spawned — while the
    /// coordinator's starts only at `Ready`. A slow start-up could therefore retire
    /// the writer before the reader began waiting, so a thread binding in that window
    /// went unrecorded and the launcher sat out the whole grace before attaching
    /// anyway: the right answer reached the slow way, for no reason.
    ///
    /// **What this asserts, and why it is the poll and not the write.** The record
    /// write happens on the blocking pool, and `codex_launch`'s test sandbox is keyed
    /// by THREAD — so a write from that pool lands in a different sandbox than the one
    /// this test can read, which measures the harness rather than the fix. The
    /// property the fix is actually about is the watcher's LIFETIME, and that is
    /// observable directly: the probe must still be being called after the old
    /// deadline has passed. Under the old code the task had returned by then and the
    /// count would be frozen.
    ///
    /// **Mutation:** restore the `deadline` and its `return`, and `after` stops
    /// advancing past `at_deadline`.
    #[tokio::test]
    async fn the_watcher_is_still_watching_after_the_old_deadline() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        // Never binds: the watcher has no reason to return except a deadline, which
        // is exactly the thing under test.
        let probe: BoundThreadProbe = Arc::new(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        });
        let watcher = spawn_thread_binding_watcher("UIDWATCHLIFE".to_string(), probe);

        let grace = crate::codex_launch::THREAD_BINDING_GRACE;
        tokio::time::sleep(grace + Duration::from_millis(500)).await;
        let at_deadline = calls.load(std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let after = calls.load(std::sync::atomic::Ordering::Relaxed);
        watcher.abort();

        assert!(
            at_deadline > 0,
            "the watcher must have been polling at all before this proves anything"
        );
        assert!(
            after > at_deadline,
            "the watcher must still be polling past the old {}s deadline — it stopped \
             at {at_deadline} polls and was still at {after} a second later",
            grace.as_secs()
        );
    }

    /// **A PRESERVED LOG IS BOUNDED, AND KEEPS BOTH ENDS.**
    ///
    /// The bound exists because a broker log grows with the session and the copy used
    /// to be unbounded. Which ends it keeps is the measured part: on the real refused
    /// launch this feature exists for, the causal `thread/start` refusal is line 317
    /// of 332 — so a head-only bound would have discarded exactly the sentence being
    /// preserved. This drives a log far larger than the bound and asserts that a
    /// marker planted at each end survives and the middle does not.
    ///
    /// **Mutation:** drop the tail branch and the `TAIL-MARKER` assertion fails,
    /// which is the failure that matters.
    #[test]
    fn a_preserved_broker_log_is_bounded_at_both_ends() {
        let dir = std::env::temp_dir().join(format!("cc-preserve-{}", std::process::id()));
        let logs = dir.join("logs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&logs).unwrap();

        // A run log an order of magnitude past the bound, with a marker at each end.
        let src = dir.join("broker.log");
        let filler = "x".repeat(1024);
        let mut body = String::from("HEAD-MARKER\n");
        for _ in 0..(PRESERVED_LOG_EDGE_BYTES * 3 / 1024) {
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str("TAIL-MARKER\n");
        std::fs::write(&src, &body).unwrap();
        assert!(
            body.len() > PRESERVED_LOG_EDGE_BYTES * 2,
            "the bound must bite"
        );

        let dest = preserve_broker_log_into(&logs, "UIDPRESERVE", &src).expect("preserved");
        let kept = std::fs::read_to_string(&dest).unwrap();

        assert!(
            kept.len() < body.len(),
            "the copy must be smaller than the log"
        );
        assert!(
            kept.len() <= PRESERVED_LOG_EDGE_BYTES * 2 + 128,
            "and within the stated bound, got {}",
            kept.len()
        );
        assert!(kept.starts_with("HEAD-MARKER"), "the head is kept");
        assert!(
            kept.contains("TAIL-MARKER"),
            "the TAIL is kept — this is the half the measurement says carries the verdict"
        );
        assert!(
            kept.contains("bytes elided by the preserved-log bound"),
            "and a bounded copy says so, so it is never mistaken for a whole one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **THE FIRST REFUSAL IS THE CAUSE; EVERYTHING AFTER IT IS CONSEQUENCE.**
    ///
    /// Replays the exact sequence a live refused launch wrote to `broker.log`: the
    /// `thread/start` the fingerprint refused, then the two `thread/list` refusals a
    /// TUI already on its way out produces. Keeping the last match put the
    /// `thread/list` line in the user's terminal — an allowlist problem that is not
    /// the bug, pointing an operator away from the sandbox mismatch that is.
    ///
    /// **Mutation:** change the sink's `slot.is_none()` guard back to an
    /// unconditional write and this fails, naming `thread/list`.
    #[test]
    fn the_first_refusal_is_the_one_reported() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let inner: EventSink = Arc::new(move |line: &str| {
            recorder.lock().unwrap().push(line.to_string());
        });
        let refusal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = refusal_watching_sink(inner, Arc::clone(&refusal));

        sink("Tui: leg opened (conn 155)");
        sink(
            "Tui: refuse->synthetic error (thread/start: fingerprint refused (Conflict): \
             params.sandbox: a sandbox mode string(len=15) that is not the fingerprint's \
             \"read-only\") (conn 155)",
        );
        sink("Tui: refuse->synthetic error (thread/list: refused (NotAllowlisted)) (conn 155)");
        sink("Tui: refuse->synthetic error (thread/list: refused (NotAllowlisted)) (conn 155)");

        let kept = refusal.lock().unwrap().clone().expect("a refusal was seen");
        assert!(
            kept.contains("thread/start") && kept.contains("params.sandbox"),
            "the causal refusal must be the one kept, not the fallout: {kept}"
        );
        // And the wrapper is a wrapper: every line still reaches the log unchanged,
        // refusals included.
        assert_eq!(
            seen.lock().unwrap().len(),
            4,
            "watching the stream must not consume any of it"
        );

        // The reason names it, after the path — which is what survives `sanitize`'s
        // 300-character bound (see `no_thread_reason`).
        let reason = no_thread_reason(Some(&kept), Some(Path::new("/tmp/logs/broker-cc-1-U.log")));
        let printed = crate::codex_coordinator::sanitize(&reason);
        assert!(
            printed.contains("/tmp/logs/broker-cc-1-U.log") && printed.contains("thread/start"),
            "the printed line must name both the log and the causal refusal: {printed}"
        );
    }

    /// The minimum complete charter: every required flag, no passthrough.
    fn complete(extra: &[&str]) -> Vec<String> {
        let mut parts = vec![
            "--uid",
            "01JQXV9K7B8N4M2P6R3T5W9YQD",
            "--nonce",
            "0123456789abcdef0123456789abcdef",
            "--tmux-socket",
            "/tmp/cc.tmux.sock",
            "--codex",
            "/usr/local/bin/codex",
            // A7.1: the identity of the bytes at that path, as the launcher
            // inspected them. Required — the host verifies it before each exec.
            "--codex-sha256",
            "2222222222222222222222222222222222222222222222222222222222222222",
            "--run-dir",
            "/tmp/cc.host.1",
            "--codex-home",
            "/tmp/cc.home.1",
            "--approval-policy",
            "untrusted",
            "--approvals-reviewer",
            "user",
            "--sandbox",
            "read-only",
            "--hooks-enabled",
            "true",
            // The fifth fingerprint dimension (round-2 P4): the CANONICAL launch cwd the
            // coordinator resolved. The host passes it through and never re-resolves it.
            "--launch-cwd",
            "/work/proj",
        ];
        parts.extend_from_slice(extra);
        argv(&parts)
    }

    #[test]
    fn a_gate_that_could_not_be_reached_parks_while_a_refusal_exits() {
        use crate::codex_custodian::HostAdmission;
        use protocol::tmux::CleanupOutcome;

        // The distinction is NOT admitted-vs-refused. It is whether ARRIVAL was
        // durably recorded.
        //
        // A refusal is a decision made after the record already says this pane ran,
        // so ending the pane is safe — the custodian has what it needs. A gate that
        // could not be REACHED is different in kind: nothing says the pane ever
        // existed, and on a launch whose creation was indeterminate this pane is
        // the session's last observable trace. Exiting there strands cleanup until
        // the next reboot, so the host parks instead and stays visible.
        assert_eq!(
            admission_action(Ok(HostAdmission::Admitted)),
            HostAction::Proceed
        );
        assert_eq!(
            admission_action(Ok(HostAdmission::CleanupOnly {
                reason: "launch is Failed, not pending".into(),
                cleanup: CleanupOutcome::Killed,
            })),
            HostAction::Exit(EX_HOST_NOT_ADMITTED),
            "a post-arrival refusal exits as before"
        );
        match admission_action(Ok(HostAdmission::ParkInert {
            reason: "could not take the launch lock".into(),
        })) {
            HostAction::ParkInert(why) => assert!(why.contains("launch lock")),
            other => panic!("a pre-arrival failure must PARK, not {other:?}"),
        }
        // An unevaluable gate is pre-arrival too: it never told us arrival is
        // durable, so it must not be read as a refusal.
        match admission_action(Err(anyhow!("the record could not be read"))) {
            HostAction::ParkInert(why) => assert!(
                why.contains("could not be evaluated"),
                "and it must say why: {why}"
            ),
            other => panic!("an unevaluable gate must PARK, not {other:?}"),
        }
    }

    #[test]
    fn parses_a_complete_charter_and_passthrough() {
        let a = parse_host_args(&complete(&["--", "--search", "hello world"])).unwrap();
        assert_eq!(a.codex, PathBuf::from("/usr/local/bin/codex"));
        assert_eq!(
            a.codex_sha256,
            "2222222222222222222222222222222222222222222222222222222222222222"
        );
        assert_eq!(a.run_dir, PathBuf::from("/tmp/cc.host.1"));
        assert_eq!(a.codex_home, PathBuf::from("/tmp/cc.home.1"));
        assert_eq!(a.fingerprint.approval_policy, "untrusted");
        assert_eq!(a.fingerprint.approvals_reviewer, "user");
        assert_eq!(a.fingerprint.sandbox, "read-only");
        assert!(a.fingerprint.hooks_enabled);
        assert_eq!(a.fingerprint.launch_cwd, "/work/proj");
        // Everything past `--` is the TUI's, verbatim.
        assert_eq!(a.tui_args, vec!["--search", "hello world"]);
    }

    #[test]
    fn parses_explicit_fingerprint_dimensions() {
        let a = parse_host_args(&argv(&[
            "--uid",
            "u",
            "--nonce",
            "n",
            "--tmux-socket",
            "/s",
            "--codex",
            "/c",
            "--codex-sha256",
            "2222222222222222222222222222222222222222222222222222222222222222",
            "--run-dir",
            "/r",
            "--codex-home",
            "/h",
            "--approval-policy",
            "on-request",
            "--approvals-reviewer",
            "codex",
            "--sandbox",
            "workspace-write",
            "--hooks-enabled",
            "false",
            "--launch-cwd",
            "/private/tmp/ws",
        ]))
        .unwrap();
        assert_eq!(a.fingerprint.approval_policy, "on-request");
        assert_eq!(a.fingerprint.approvals_reviewer, "codex");
        assert_eq!(a.fingerprint.sandbox, "workspace-write");
        assert!(!a.fingerprint.hooks_enabled);
        assert_eq!(a.fingerprint.launch_cwd, "/private/tmp/ws");
        assert!(a.tui_args.is_empty());
    }

    #[test]
    fn every_required_flag_is_required() {
        // Dropping any one of the required flags fails closed. Each pair is
        // the flag name and its value's position in `complete`.
        for drop in [
            "--uid",
            "--nonce",
            "--tmux-socket",
            "--codex",
            "--codex-sha256",
            "--run-dir",
            "--codex-home",
            "--approval-policy",
            "--approvals-reviewer",
            "--launch-cwd",
            "--sandbox",
            "--hooks-enabled",
        ] {
            let full = complete(&[]);
            let mut kept = Vec::new();
            let mut it = full.iter();
            while let Some(a) = it.next() {
                if a == drop {
                    let _ = it.next(); // drop its value too
                    continue;
                }
                kept.push(a.clone());
            }
            assert!(
                parse_host_args(&kept).is_err(),
                "omitting {drop} must fail closed — the host applies no default"
            );
        }
    }

    /// A7.1 at the charter: the host will not run a binary it cannot check, and it
    /// will not accept a digest in a shape it and the launcher could spell two ways.
    #[test]
    fn the_codex_digest_is_required_and_strictly_shaped() {
        // Absent: a refusal that says why, not a fall-back to trusting the path.
        let full = complete(&[]);
        let at = full.iter().position(|a| a == "--codex-sha256").unwrap();
        let mut without: Vec<String> = full.clone();
        without.drain(at..at + 2);
        // `HostArgs` is deliberately not `Debug` (it is a receipt, not a record),
        // so the refusal is read out by hand rather than with `expect_err`.
        let err = match parse_host_args(&without) {
            Ok(_) => panic!("a host with no digest must refuse"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("--codex-sha256"), "names the flag: {err}");

        // Malformed: too short, too long, non-hex, uppercase. Each is refused
        // rather than normalised — see `codex::parse_codex_sha256`.
        //
        // The case probe is built from a real digest and checked to CONTAIN a
        // letter first. The obvious spelling — uppercasing the all-digit fixture
        // above — is a probe that tests nothing, because digits have no case; it
        // passed here for exactly that reason until this assertion caught it.
        let upper = protocol::hash::sha256_hex(b"codex").to_uppercase();
        assert!(
            upper.chars().any(|c| c.is_ascii_uppercase()),
            "the case probe must actually differ from its lowercase form: {upper}"
        );
        for bad in [
            "deadbeef",
            "2222222222222222222222222222222222222222222222222222222222222222f",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            upper.as_str(),
        ] {
            let mut args = full.clone();
            args[at + 1] = bad.to_string();
            assert!(
                parse_host_args(&args).is_err(),
                "--codex-sha256 {bad:?} must be refused"
            );
        }

        // And a second one is an ambiguous charter, not a last-wins override: two
        // digests for one binary is a disagreement about which bytes are allowed.
        let mut twice = full.clone();
        twice.push("--codex-sha256".into());
        twice.push("3333333333333333333333333333333333333333333333333333333333333333".into());
        assert!(parse_host_args(&twice).is_err());
    }

    /// The digest is only worth what the path is worth. A `--codex` with no `/` is
    /// read by `File::open` (the identity check) as `./codex` and by `Command::new`
    /// (the spawn) as a `PATH` search — two files, one string, and a verify that
    /// passes truthfully about the wrong bytes. That spelling is refused.
    #[test]
    fn a_codex_path_that_the_check_and_the_spawn_would_resolve_differently_is_refused() {
        let full = complete(&[]);
        let at = full.iter().position(|a| a == "--codex").unwrap();

        // A bare name — the case where the two resolvers genuinely diverge.
        let mut bare = full.clone();
        bare[at + 1] = "codex".to_string();
        let err = match parse_host_args(&bare) {
            Ok(_) => panic!("a bare --codex name must be refused"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("absolute"), "names the requirement: {err}");
        assert!(
            err.contains("PATH"),
            "and says why the two resolvers disagree: {err}"
        );

        // Relative spellings resolve the same way for both, but make both depend on
        // the process's cwd — a second route to one string meaning two files.
        for relative in ["./codex", "../bin/codex", "bin/codex"] {
            let mut args = full.clone();
            args[at + 1] = relative.to_string();
            assert!(
                parse_host_args(&args).is_err(),
                "--codex {relative:?} must be refused"
            );
        }

        // The absolute path the coordinator actually sends is accepted.
        assert!(parse_host_args(&full).is_ok());
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(parse_host_args(&complete(&["--bogus"])).is_err());
        assert!(parse_host_args(&complete(&["stray-positional"])).is_err());
    }

    #[test]
    fn fingerprint_values_are_strictly_parsed() {
        // A missing value is never a silent default.
        assert!(parse_host_args(&argv(&[
            "--codex",
            "/c",
            "--run-dir",
            "/r",
            "--codex-home",
            "/h",
            "--approval-policy"
        ]))
        .is_err());
        // Empty and control-bearing values are refused.
        let mut empty = complete(&[]);
        let idx = empty.iter().position(|a| a == "--sandbox").unwrap();
        empty[idx + 1] = String::new();
        assert!(parse_host_args(&empty).is_err());
        let mut ctrl = complete(&[]);
        let idx = ctrl.iter().position(|a| a == "--approval-policy").unwrap();
        ctrl[idx + 1] = "read\u{1b}[2Jonly".to_string();
        assert!(parse_host_args(&ctrl).is_err());
    }

    #[test]
    fn hooks_enabled_accepts_only_true_or_false() {
        for good in ["true", "false"] {
            let mut a = complete(&[]);
            let idx = a.iter().position(|x| x == "--hooks-enabled").unwrap();
            a[idx + 1] = good.to_string();
            assert!(parse_host_args(&a).is_ok(), "{good} should parse");
        }
        // The old lenient spellings are now errors, not silent truths.
        for bad in ["1", "0", "yes", "no", "TRUE", "False", ""] {
            let mut a = complete(&[]);
            let idx = a.iter().position(|x| x == "--hooks-enabled").unwrap();
            a[idx + 1] = bad.to_string();
            assert!(
                parse_host_args(&a).is_err(),
                "--hooks-enabled {bad:?} must be refused, never defaulted"
            );
        }
    }

    #[test]
    fn duplicate_flags_are_refused() {
        for dup in [
            ["--uid", "other"],
            ["--nonce", "other"],
            ["--tmux-socket", "/other.sock"],
            ["--codex", "/other"],
            ["--run-dir", "/other"],
            ["--codex-home", "/other"],
            ["--approval-policy", "never"],
            ["--approvals-reviewer", "codex"],
            ["--sandbox", "danger-full-access"],
            ["--hooks-enabled", "false"],
        ] {
            assert!(
                parse_host_args(&complete(&dup)).is_err(),
                "a duplicate {} must be refused, not last-wins",
                dup[0]
            );
        }
    }

    /// The host does not trust its caller: passthrough runs through the SAME 2a
    /// reserved-argv grammar `codeconnect codex` uses, so the transport, the
    /// working directory, profiles, approval controls and subcommands cannot be
    /// smuggled in behind `--`.
    #[test]
    fn passthrough_tui_args_are_vetted_by_the_reserved_grammar() {
        for refused in [
            vec!["--remote", "unix:///tmp/evil.sock"],
            vec!["--remote-auth-token-env", "TOKEN"],
            vec!["--profile", "yolo"],
            vec!["--dangerously-bypass-approvals-and-sandbox"],
            vec!["--yolo"],
            vec!["-a", "never"],
            vec!["-C", "/"],
            vec!["--config", "approval_policy=never"],
            vec!["exec"],
            vec!["resume"],
            vec!["--not-a-real-flag"],
        ] {
            let mut parts = vec!["--"];
            parts.extend_from_slice(&refused);
            assert!(
                parse_host_args(&complete(&parts)).is_err(),
                "{refused:?} must be refused as TUI passthrough"
            );
        }
        // Benign passthrough still rides through untouched.
        let ok = parse_host_args(&complete(&["--", "--search", "--model", "gpt-5"])).unwrap();
        assert_eq!(ok.tui_args, vec!["--search", "--model", "gpt-5"]);
    }

    /// **The TUI is spawned with the FENCED argv, never the raw passthrough.**
    ///
    /// `codex::fence_positionals` is what makes a bare token unable to dispatch
    /// (measured on both binaries; see its docs), and it only does that if this spawn
    /// actually uses it. What has to be proven is the absence of the raw form at that
    /// call site, which no unit test reaches by calling anything — the same reason
    /// `codex::the_version_string_is_recorded_and_never_gates` reads its own source.
    #[test]
    fn the_tui_is_spawned_with_the_fenced_argv() {
        let src = include_str!("codex_host.rs");
        let spawn = src
            .split("let mut tui_cmd = Command::new(&args.codex);")
            .nth(1)
            .expect("the TUI spawn is in this file");
        let spawn = &spawn[..spawn.find("CODEX_HOME").unwrap_or(spawn.len())];
        assert!(
            spawn.contains(".args(&fenced)"),
            "the TUI spawn must pass the fenced argv: {spawn}"
        );
        assert!(
            !spawn.contains("args.tui_args"),
            "the raw passthrough must not reach the TUI spawn: {spawn}"
        );
    }

    /// **THE SANDBOX THE GRAMMAR CLAIMS IS THE SANDBOX THE TUI IS SPAWNED WITH.**
    ///
    /// `codex::validate_codex_argv` refuses a user's `--sandbox` with "`--sandbox` is
    /// set by CodeConnect (the session sandbox policy)". That sentence is only true
    /// if this spawn sets it. Without the flag the TUI picks its mode from the
    /// project's `trust_level`, and a `trusted` project sends
    /// `"sandbox":"workspace-write"` at a fingerprint pinned `read-only` — a launch
    /// the broker refuses and the user cannot fix.
    ///
    /// Read from source for the same reason as the fenced-argv test above: what has
    /// to be proven is a property of the one call site, and no unit test reaches it.
    /// The value is asserted to be the fingerprint's own field rather than a second
    /// copy of `codex::LAUNCH_SANDBOX`, because two constants can drift and the flag
    /// must never disagree with the pin the broker enforces.
    ///
    /// **Mutation:** delete the `.arg("--sandbox")` pair from the spawn and this
    /// fails; spell the value as a literal or as `codex::LAUNCH_SANDBOX` and it
    /// fails too.
    #[test]
    fn the_tui_is_spawned_with_the_fingerprints_own_sandbox() {
        let src = include_str!("codex_host.rs");
        let spawn = src
            .split("let mut tui_cmd = Command::new(&args.codex);")
            .nth(1)
            .expect("the TUI spawn is in this file");
        let spawn = &spawn[..spawn.find("CODEX_HOME").unwrap_or(spawn.len())];
        assert!(
            spawn.contains(".arg(\"--sandbox\")"),
            "the TUI spawn must set the sandbox CodeConnect claims to own: {spawn}"
        );
        assert!(
            spawn.contains(".arg(&args.fingerprint.sandbox)"),
            "the flag must carry the fingerprint's own value, so it cannot drift from \
             the pin the broker enforces: {spawn}"
        );
    }

    /// The main race polls the broker's `JoinHandle` to completion on its
    /// broker-death arm. Awaiting that same handle a second time **panics**
    /// (probed: tokio `JoinHandle polled after completion`), which would turn the
    /// session-fatal path into an unwind — exit 101 instead of 70, and the run dir
    /// never swept, because the cleanup sits after `run_session` returns. So
    /// teardown must notice a handle that is already finished.
    #[tokio::test]
    async fn teardown_survives_a_broker_handle_that_already_completed() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn a stand-in child");
        let sink: EventSink = Arc::new(|_: &str| {});
        let mut session = Session::new(child, sink);

        let mut handle: JoinHandle<std::io::Result<()>> = tokio::spawn(async { Ok(()) });
        // Consume it exactly the way the race's broker arm does.
        (&mut handle)
            .await
            .expect("the stand-in broker task joins")
            .expect("and returns Ok");
        session.broker = Some(handle);

        // The assertion IS "this returns at all": before the fix it panicked here.
        session.teardown().await;
        assert!(session.broker.is_none(), "teardown must consume the handle");
    }

    #[test]
    fn sun_len_is_enforced() {
        assert!(assert_sun_len(Path::new("/tmp/cc/as.sock")).is_ok());
        let long = PathBuf::from(format!("/tmp/{}/as.sock", "x".repeat(120)));
        assert!(assert_sun_len(&long).is_err());
    }

    #[test]
    fn ready_socket_rejects_absent_and_non_socket_paths() {
        // A regular file is not a bound socket.
        let dir = std::env::temp_dir().join(format!("cc-host-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("not-a-sock");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_ready_socket(&file, None));
        assert!(!is_ready_socket(&dir.join("absent.sock"), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The event sink must not be a terminal-escape injection channel: a broker
    /// note quoting hostile wire material lands in the log escaped.
    #[test]
    fn the_event_sink_escapes_control_characters() {
        let dir = std::env::temp_dir().join(format!("cc-host-sink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("broker.log");
        let sink = file_event_sink(&log).unwrap();
        sink("Tui: forward (\u{1b}[2J\u{7}nasty\nsecond line)");
        drop(sink);
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains('\u{1b}'),
            "raw ESC reached the log: {body:?}"
        );
        assert!(!body.contains('\u{7}'), "raw BEL reached the log: {body:?}");
        // Exactly one physical line: the embedded newline was escaped too.
        assert_eq!(body.lines().count(), 1, "{body:?}");
        // The greppable prefix the live gates rely on survives escaping.
        assert!(body.contains("Tui: forward"), "{body:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second `create_private_log` on the same path refuses rather than adopts.
    #[test]
    fn private_logs_are_created_fresh_and_0600() {
        let dir = std::env::temp_dir().join(format!("cc-host-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("broker.log");
        let f = create_private_log(&log).unwrap();
        drop(f);
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log must be owner-only, got {mode:o}");
        assert!(
            create_private_log(&log).is_err(),
            "an existing log must be refused, never adopted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A flag-shaped value is refused rather than swallowed. The `--` case is the
    /// one that matters: swallowing it would silently destroy the passthrough
    /// boundary and re-read the TUI's args as host flags.
    #[test]
    fn flag_shaped_values_are_refused_not_swallowed() {
        for argv_parts in [
            vec!["--codex", "--run-dir", "/r"],
            vec!["--codex", "--"],
            vec!["--run-dir", "--codex-home", "/h"],
            vec!["--sandbox", "--hooks-enabled", "true"],
        ] {
            let msg = match parse_host_args(&argv(&argv_parts)) {
                Ok(_) => panic!("{argv_parts:?}: a flag-shaped value must be refused"),
                Err(err) => format!("{err:#}"),
            };
            assert!(
                msg.contains("flag-shaped value is refused"),
                "{argv_parts:?} should be refused for its SHAPE, got: {msg}"
            );
        }
    }

    /// Write `body` into a fresh 0600 log and hand back the held handle, exactly
    /// the way [`run_session`] holds the app-server's stderr file.
    fn held_stderr_log(dir: &Path, body: &[u8]) -> std::fs::File {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut file = create_private_log(&dir.join("stderr.log")).unwrap();
        file.write_all(body).unwrap();
        file
    }

    /// The child's stderr is child-controlled text that the host prints to a bare
    /// terminal on a bring-up failure, so it must be escaped, not merely truncated.
    #[test]
    fn the_stderr_excerpt_escapes_control_characters() {
        let dir = std::env::temp_dir().join(format!("cc-host-esc-{}", std::process::id()));
        // An alt-screen switch and an OSC 52 clipboard write, as a hostile
        // app-server could emit them.
        let mut file = held_stderr_log(&dir, b"boom\x1b[?1049h\x1b]52;c;ZXZpbA==\x07\n");
        let excerpt = stderr_excerpt(&mut file);
        assert!(!excerpt.contains('\u{1b}'), "raw ESC survived: {excerpt:?}");
        assert!(!excerpt.contains('\u{7}'), "raw BEL survived: {excerpt:?}");
        assert!(
            excerpt.contains("boom"),
            "the readable text is kept: {excerpt:?}"
        );
        // Truncated excerpts are escaped too, not just short ones.
        let mut file = held_stderr_log(&dir, "\u{1b}".repeat(STDERR_EXCERPT_LIMIT * 2).as_bytes());
        let big = stderr_excerpt(&mut file);
        assert!(big.contains("<truncated:"), "{big:.60}");
        assert!(!big.contains('\u{1b}'), "raw ESC survived truncation path");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The excerpt is bounded and says so — an error message is not a log.
    #[test]
    fn stderr_excerpt_is_bounded_and_marks_truncation() {
        let dir = std::env::temp_dir().join(format!("cc-host-excerpt-{}", std::process::id()));
        let mut file = held_stderr_log(&dir, "z".repeat(STDERR_EXCERPT_LIMIT * 3).as_bytes());
        let excerpt = stderr_excerpt(&mut file);
        assert!(excerpt.contains("<truncated:"), "{excerpt:.80}");
        assert!(
            excerpt.len() < STDERR_EXCERPT_LIMIT + 200,
            "excerpt was {} bytes",
            excerpt.len()
        );
        // A short file is quoted whole, unmarked.
        let mut file = held_stderr_log(&dir, b"boom");
        assert_eq!(stderr_excerpt(&mut file), "boom");
        // The excerpt is repeatable: it seeks every time rather than consuming the
        // handle's offset, so a second bring-up error still quotes the tail.
        assert_eq!(stderr_excerpt(&mut file), "boom");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The excerpt reads the HELD handle, so replacing the *path* with something
    /// hostile after the spawn changes nothing. This is the property that keeps the
    /// error path off a child-planted FIFO (whose `read` would block the host
    /// forever) — and a FIFO cannot be used in the test itself for exactly that
    /// reason, so a decoy regular file stands in for "the name now resolves
    /// elsewhere".
    #[test]
    fn the_stderr_excerpt_reads_the_held_handle_not_the_path() {
        let dir = std::env::temp_dir().join(format!("cc-host-fd-{}", std::process::id()));
        let mut file = held_stderr_log(&dir, b"the real child stderr");
        let path = dir.join("stderr.log");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"DECOY planted after the spawn").unwrap();
        let excerpt = stderr_excerpt(&mut file);
        assert_eq!(
            excerpt, "the real child stderr",
            "the excerpt followed the path instead of the fd: {excerpt:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------ the resolution seam
    //
    // These are the tests for broker death under a live session. A real
    // broker-death E2E is impractical — `Broker::serve` only returns on an accept
    // error, and nothing outside the process can deterministically end a tokio
    // task — so the race's arms were made thin and every decision they make was
    // moved into `resolve_outcome`. This IS the coverage; `codex_host_fatal.rs`
    // covers the arms that CAN be driven from outside (app-server death, signals).

    fn exited(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn io_err(msg: &str) -> std::io::Error {
        std::io::Error::other(msg)
    }

    /// **The masking scenario.** `biased` orders polling, not readiness: the broker
    /// task can answer `Pending` and then finish before `tui.wait()` answers
    /// `Ready` in the same pass, so the TUI arm fires with the security boundary
    /// already dead. Reported as a clean TUI exit that is a leak of trust — the
    /// pane's status would say "the user quit" while the broker was gone.
    #[test]
    fn a_tui_exit_past_a_dead_broker_is_fatal_not_clean() {
        let outcome = resolve_outcome(
            Fired::Tui(WaitReport::Exited(exited(0))),
            AppServerState::Alive,
            /* broker_finished */ true,
        );
        match outcome {
            Outcome::Fatal(detail) => assert!(
                detail.contains("broker") && detail.contains("masked fatal"),
                "the refusal must name the dead boundary: {detail}"
            ),
            other => panic!("a dead broker must never resolve to a clean exit: {other:?}"),
        }
    }

    /// Broker death outranks the app-server reading too: with BOTH gone the
    /// message must be about the boundary, not merely about the upstream.
    #[test]
    fn broker_death_outranks_the_app_server_reading() {
        let outcome = resolve_outcome(
            Fired::Tui(WaitReport::Exited(exited(0))),
            AppServerState::Exited(exited(1)),
            true,
        );
        assert!(
            matches!(&outcome, Outcome::Fatal(d) if d.contains("broker")),
            "{outcome:?}"
        );
    }

    /// The pre-existing reconciliation, still enforced: a TUI that exited together
    /// with its app-server is the upstream dying, not a user quitting.
    #[test]
    fn a_tui_exit_together_with_its_app_server_is_fatal() {
        let outcome = resolve_outcome(
            Fired::Tui(WaitReport::Exited(exited(0))),
            AppServerState::Exited(exited(0)),
            false,
        );
        assert!(
            matches!(&outcome, Outcome::Fatal(d) if d.contains("upstream died first")),
            "{outcome:?}"
        );
    }

    /// Failing to DETERMINE a state is never benign — neither the app-server's nor
    /// the TUI's own. A `wait()` error carries no status the host can stand behind,
    /// so it must not degrade into exit 1, which reads as "the TUI exited 1".
    #[test]
    fn undetermined_states_fail_closed() {
        let unknown_appserver = resolve_outcome(
            Fired::Tui(WaitReport::Exited(exited(0))),
            AppServerState::Undetermined(io_err("EIO").to_string()),
            false,
        );
        assert!(
            matches!(&unknown_appserver, Outcome::Fatal(d) if d.contains("could not be determined")),
            "{unknown_appserver:?}"
        );

        let unknown_tui = resolve_outcome(
            Fired::Tui(WaitReport::Undetermined(io_err("ECHILD").to_string())),
            AppServerState::Alive,
            false,
        );
        match unknown_tui {
            Outcome::Fatal(detail) => assert!(
                detail.contains("ECHILD") && detail.contains("not a clean exit"),
                "{detail}"
            ),
            other => panic!("an undetermined TUI must be fatal, not exit 1: {other:?}"),
        }
    }

    /// The ONE benign ending, and the exact shape of it: the TUI exited with a real
    /// status while both the broker and the app-server were still alive.
    #[test]
    fn the_only_benign_ending_is_a_tui_exit_with_everything_else_alive() {
        let outcome = resolve_outcome(
            Fired::Tui(WaitReport::Exited(exited(3))),
            AppServerState::Alive,
            false,
        );
        match outcome {
            Outcome::TuiExited(status) => assert_eq!(status.code(), Some(3)),
            other => panic!("this is the benign case: {other:?}"),
        }
    }

    /// The decision recorded in the module doc: a signal delivered while the broker
    /// is already gone reports the dead boundary (70), not a clean signalled
    /// shutdown (130). Both facts are true; only one of them must not be mistaken
    /// by the coordinator for "the session ended by request".
    #[test]
    fn a_signal_past_a_dead_broker_is_fatal_not_signalled() {
        let clean = resolve_outcome(Fired::Signal("SIGTERM"), AppServerState::Alive, false);
        assert!(matches!(clean, Outcome::Signalled("SIGTERM")), "{clean:?}");

        let masked = resolve_outcome(Fired::Signal("SIGTERM"), AppServerState::Alive, true);
        match masked {
            Outcome::Fatal(detail) => assert!(
                detail.contains("SIGTERM") && detail.contains("broker"),
                "the line must name both facts: {detail}"
            ),
            other => panic!("broker death takes precedence over the signal: {other:?}"),
        }
    }

    /// The signal arm is subject to the SAME two rules as the TUI arm. An
    /// app-server death racing a SIGTERM is 70, never a clean 130: 130 tells the
    /// coordinator "the session ended by request", which is a false story about a
    /// dead upstream that happened to die in the same pass as the signal.
    #[test]
    fn a_signal_past_a_dead_app_server_is_fatal_not_signalled() {
        let outcome = resolve_outcome(
            Fired::Signal("SIGTERM"),
            AppServerState::Exited(exited(7)),
            /* broker_finished */ false,
        );
        match outcome {
            Outcome::Fatal(detail) => assert!(
                detail.contains("SIGTERM") && detail.contains("app-server"),
                "the line must name both facts: {detail}"
            ),
            other => panic!("a dead app-server must outrank the signal's clean 130: {other:?}"),
        }
    }

    /// Rule 2 on the signal arm: "undetermined is never benign" applies to EVERY
    /// arm, so a signal delivered while the app-server cannot be accounted for
    /// fails closed rather than reporting a shutdown the host cannot stand behind.
    #[test]
    fn a_signal_with_an_undetermined_app_server_fails_closed() {
        let outcome = resolve_outcome(
            Fired::Signal("SIGHUP"),
            AppServerState::Undetermined(io_err("EIO").to_string()),
            false,
        );
        match outcome {
            Outcome::Fatal(detail) => assert!(
                detail.contains("SIGHUP") && detail.contains("EIO"),
                "the line must name the signal and why the state is unknown: {detail}"
            ),
            other => panic!("an unaccountable app-server must be fatal, not 130: {other:?}"),
        }
    }

    /// Broker-death precedence is about ATTRIBUTION as well as the verdict: the
    /// app-server arm is fatal either way, but when the broker had also finished
    /// when observed, the message must not pin both facts on the upstream alone.
    ///
    /// No frequency is claimed — not "common", not "rare", since nothing measures
    /// it — and the tempting inference would be false: `broker_finished` observes
    /// the `serve()` task alone, which ends on an accept error, not the detached
    /// per-connection leg tasks, so an app-server death does not by itself finish
    /// the broker. A corner covered for attribution, because losing the boundary in
    /// the wording would be security-relevant whenever it does occur.
    #[test]
    fn the_app_server_arm_attributes_a_broker_that_had_also_finished() {
        let alone = resolve_outcome(
            Fired::AppServer,
            AppServerState::from_wait(Ok(exited(9))),
            /* broker_finished */ false,
        );
        match &alone {
            Outcome::Fatal(detail) => assert!(
                detail.contains("app-server exited") && !detail.contains("broker"),
                "an app-server death with a LIVE broker must not mention one: {detail}"
            ),
            other => panic!("{other:?}"),
        }

        let both = resolve_outcome(
            Fired::AppServer,
            AppServerState::from_wait(Ok(exited(9))),
            /* broker_finished */ true,
        );
        match &both {
            Outcome::Fatal(detail) => assert!(
                detail.contains("app-server exited") && detail.contains("broker"),
                "a broker that had also finished must be named too: {detail}"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// The two arms that are already fatal stay fatal, and say which part died.
    #[test]
    fn the_fatal_arms_name_what_died() {
        let as_dead = resolve_outcome(
            Fired::AppServer,
            AppServerState::from_wait(Ok(exited(9))),
            false,
        );
        assert!(
            matches!(&as_dead, Outcome::Fatal(d) if d.contains("app-server exited")),
            "{as_dead:?}"
        );

        let as_lost = resolve_outcome(
            Fired::AppServer,
            AppServerState::from_wait(Err(io_err("ESRCH"))),
            false,
        );
        assert!(
            matches!(&as_lost, Outcome::Fatal(d) if d.contains("lost track") && d.contains("ESRCH")),
            "{as_lost:?}"
        );

        for (end, needle) in [
            (BrokerEnd::Stopped, "stopped serving"),
            (BrokerEnd::Failed("bind: EADDRINUSE".into()), "EADDRINUSE"),
            (BrokerEnd::Abnormal("task panicked".into()), "abnormally"),
        ] {
            let outcome = resolve_outcome(Fired::Broker(end), AppServerState::Alive, true);
            assert!(
                matches!(&outcome, Outcome::Fatal(d) if d.contains(needle)),
                "{outcome:?} should mention {needle}"
            );
        }
    }

    /// The whole broker-death check rests on one tokio behaviour: an
    /// [`tokio::task::AbortHandle`] taken *before* a `select!` reports
    /// `is_finished()` once its task has completed **normally**, not only when it
    /// was aborted. The docs are explicit that the converse is not guaranteed
    /// (`is_finished()` can be false right after `abort()`), so the direction this
    /// file actually depends on is pinned here rather than assumed. If this ever
    /// stopped holding, `resolve_outcome` would keep passing while the race's arms
    /// silently went back to reporting a dead broker as a clean exit.
    ///
    /// **Pinned in the production shape: the `JoinHandle` is still UNAWAITED.** In
    /// the real race the TUI and signal arms probe while the broker arm's
    /// `&mut *broker` has *not* consumed the handle — the join future is merely
    /// parked. Observing `is_finished()` only *after* awaiting the handle to
    /// completion would prove a strictly weaker thing (that a joined task reports
    /// finished) and would keep passing even if the flag only flipped at join time,
    /// which is exactly the regression that would re-open the masking window. So
    /// the flip is observed in a bounded poll BEFORE the handle is awaited at all.
    #[tokio::test]
    async fn an_abort_handle_reports_a_normally_finished_task_as_finished() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task: JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            let _ = rx.await;
            Ok(())
        });
        let probe = task.abort_handle();
        assert!(
            !probe.is_finished(),
            "a task still parked on its channel is not finished"
        );

        // Let it complete on its own — no abort anywhere in this test.
        let _ = tx.send(());

        // Poll the probe while `task` sits untouched, exactly as the race's other
        // arms do. Bounded rather than a single check because completion is
        // genuinely concurrent: the task has to be scheduled and run to its end,
        // and each `sleep` below is the yield that lets that happen.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut flipped = false;
        while Instant::now() < deadline {
            if probe.is_finished() {
                flipped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            flipped,
            "an AbortHandle taken before the task ran must observe its NORMAL \
             completion WITHOUT the JoinHandle being awaited — that is the shape the \
             TUI and signal arms snapshot in, and the only shape that closes the \
             masking window"
        );

        // Only now consume it, the way the broker arm eventually would.
        task.await.expect("the task joins").expect("and returns Ok");
        assert!(probe.is_finished(), "and it stays finished after the join");
    }

    /// `try_wait`'s three answers map onto the three app-server states, which is
    /// what the TUI arm's reconciliation reads.
    #[test]
    fn app_server_state_maps_try_wait_faithfully() {
        assert!(matches!(
            AppServerState::from_try_wait(Ok(None)),
            AppServerState::Alive
        ));
        assert!(matches!(
            AppServerState::from_try_wait(Ok(Some(exited(0)))),
            AppServerState::Exited(_)
        ));
        assert!(matches!(
            AppServerState::from_try_wait(Err(io_err("EIO"))),
            AppServerState::Undetermined(_)
        ));
    }

    /// Readiness rests on owning the directory, so the host must refuse a run dir
    /// it did not create — that is what makes a planted `as.sock` impossible.
    ///
    /// A11.4: asserted against `create_run_dir_atomically`, the function that
    /// actually creates it, rather than against a bare `DirBuilder` call that no
    /// longer appears on the path.
    ///
    /// **The destination is EMPTY, and that is the whole point.** A non-empty
    /// destination proves nothing about `RENAME_EXCL`: plain `rename(2)` already
    /// refuses to replace a directory that has anything in it (`ENOTEMPTY`), so a
    /// build that dropped the flag entirely would still pass. The load-bearing
    /// Darwin behaviour is the other one — plain `rename(2)` onto an existing
    /// **empty** directory SUCCEEDS, silently replacing it — and the only way to
    /// see the difference is to test that shape and prove the inode standing there
    /// afterwards is the same one, never a replacement.
    #[test]
    fn an_existing_run_dir_is_refused() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("cc-host-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(
            dir.with_file_name(format!("cc-host-owned-{}.tmp", std::process::id())),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let squatted = std::fs::metadata(&dir).unwrap().ino();
        let err = create_run_dir_atomically(&dir, "uid-a", "nonce-a")
            .expect_err("publishing must refuse a run dir the host did not create");
        assert!(
            format!("{err:#}").contains("must NOT already exist"),
            "the refusal must name the exclusivity rule: {err:#}"
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().ino(),
            squatted,
            "the squatted EMPTY directory must be the same inode afterwards — a plain \
             rename(2) would have replaced it and left our staged dir standing at that name"
        );
        assert!(
            !dir.join(crate::codex_launch::RUN_DIR_OWNER_FILE).exists(),
            "and it must not have acquired our marker: it was never adopted"
        );
        // The staging dir is cleaned up on the refusal path, not leaked.
        let staged = dir.with_file_name(format!("cc-host-owned-{}.tmp", std::process::id()));
        assert!(
            !staged.exists(),
            "the staging dir must be removed when publishing is refused: {}",
            staged.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Finding 3 (round 2): recording the run-dir claim is LAUNCH-FATAL, so
    /// `host_claimed_run_dir == false` is a sound negative proof.**
    ///
    /// The A11.7 loser-refusal gate reads that bit negatively: a losing host with the
    /// bit false is taken as proof it was refused at the fence and never adopted the
    /// winner's directory. That only holds if a host which got PAST the claim — won
    /// its `mkdir` — always records it. Under the old best-effort write it did not: a
    /// host that won the `mkdir` but could not record the claim (record lock past
    /// budget, identity unresolvable, store faulted) ran on with the bit false, and a
    /// later death then presented to the gate as a clean fence refusal.
    ///
    /// Here the record cannot be written — the uid names no launch record to write
    /// under, the same "the host did not get to write it down" outcome a mid-launch
    /// fault produces. The host wins its `mkdir`, then `note_run_dir_claimed` surfaces
    /// the failure the old code swallowed, and the fatal path sweeps the directory it
    /// created — so it never runs on with the bit false.
    ///
    /// **Mutation:** make `note_run_dir_claimed` best-effort again (swallow the error
    /// and return) and this fails at the `expect_err` — the exact masking finding 3
    /// names, back in place.
    #[test]
    fn a_host_that_cannot_record_its_claim_refuses_and_tears_down_the_dir_it_made() {
        // A uid with no launch record, so the claim has nothing to be written into —
        // exactly as `an_unrecordable_child_never_execs` forces `record_child_pid` to
        // fail.
        let mut args = parse_host_args(&complete(&[])).expect("charter");
        args.uid = "01JQXV9K7B8N4M2P6R3T5WZZZZ".into();
        let run = std::env::temp_dir().join(format!("cc-claim-fatal-{}", std::process::id()));
        let staged = run.with_file_name(format!("cc-claim-fatal-{}.tmp", std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        let _ = std::fs::remove_dir_all(&staged);
        args.run_dir = run.clone();

        // The host WINS its mkdir — it is past the fence, directory created and owned.
        create_run_dir_atomically(&args.run_dir, &args.uid, &args.nonce).expect("win the mkdir");
        assert!(run.exists(), "the host created and owns the run dir");

        // But it cannot RECORD the claim. Best-effort would have returned `()` here
        // and let the launch proceed with `host_claimed_run_dir == false`; now the
        // failure is surfaced so the caller can refuse.
        let err = note_run_dir_claimed(&args)
            .expect_err("a host that cannot record its claim must not silently pass it");
        assert!(
            format!("{err:#}").contains("claimed the run dir"),
            "the refusal should name the claim it could not record: {err:#}"
        );

        // And the fatal path tears down the directory it created, exactly as
        // `orchestrate` does before returning EX_HOST_FATAL — so nothing is left
        // running against a dir whose claim never reached the record.
        sweep_own_run_dir(&args.run_dir, &args.uid, &args.nonce);
        assert!(
            !run.exists(),
            "the run dir must be swept when the claim cannot be recorded: {}",
            run.display()
        );
        let _ = std::fs::remove_dir_all(&run);
        let _ = std::fs::remove_dir_all(&staged);
    }

    /// A11.5: the host's own teardown is bound to the directory it PROVED is its
    /// own, not to the name it was handed.
    ///
    /// The collision this refuses is the one A11.7 names: the host is still inside
    /// its bounded teardown when a custodian removes the old inode and a colliding
    /// launch — the derivation is many-to-one, so two launches can land on one name
    /// — publishes a fresh directory at exactly that path. A path-addressed
    /// `remove_dir_all` re-resolves the name and takes the replacement's bound
    /// sockets and logs with it. Staged here as the state that collision produces:
    /// a directory at our path whose marker names somebody else.
    ///
    /// **Mutation:** put `remove_dir_all(run_dir)` back in `sweep_own_run_dir` and
    /// this fails — the stranger's directory and its socket are gone.
    #[test]
    fn the_hosts_teardown_refuses_a_run_dir_whose_marker_is_not_its_own() {
        let dir = std::env::temp_dir().join(format!("cc-host-teardown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // What a colliding launch publishes at our name: its own marker, and the
        // sockets and logs a delete must never reach.
        crate::codex_launch::write_owner_marker(&dir, "someone-elses-uid", "someone-elses-nonce")
            .unwrap();
        std::fs::write(dir.join("tui.sock"), b"a live session's socket").unwrap();

        sweep_own_run_dir(&dir, "our-uid", "our-nonce");

        assert!(
            dir.exists(),
            "a directory whose marker names another launch must survive our teardown"
        );
        assert_eq!(
            std::fs::read(dir.join("tui.sock")).unwrap(),
            b"a live session's socket",
            "and so must everything in it — this is a LIVE session's run dir"
        );

        // And the directory this host really does own is removed whole.
        let _ = std::fs::remove_dir_all(&dir);
        let staged = dir.with_file_name(format!("cc-host-teardown-{}.tmp", std::process::id()));
        let _ = std::fs::remove_dir_all(&staged);
        create_run_dir_atomically(&dir, "our-uid", "our-nonce").unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("logs/as.stderr"), b"x").unwrap();
        sweep_own_run_dir(&dir, "our-uid", "our-nonce");
        assert!(
            !dir.exists(),
            "our own run dir is removed whole, nested contents and all: {}",
            dir.display()
        );
    }

    /// A11.4: the run dir is only ever observable WITH its owner marker.
    ///
    /// The window this closes: `mkdir(final)` followed by a separate marker write
    /// let a SIGKILLed host strand an unmarked directory at exactly the path the
    /// custodian later consults — and the custodian correctly refuses to delete a
    /// directory it cannot prove is the launch's, so it stayed forever.
    #[test]
    fn a_published_run_dir_always_carries_its_owner_marker() {
        let dir = std::env::temp_dir().join(format!("cc-host-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(
            dir.with_file_name(format!("cc-host-atomic-{}.tmp", std::process::id())),
        );

        create_run_dir_atomically(&dir, "uid-b", "nonce-b").expect("publish");
        let marker = dir.join(crate::codex_launch::RUN_DIR_OWNER_FILE);
        assert!(
            marker.exists(),
            "a published run dir must already contain its marker"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "uid-b\nnonce-b\n",
            "and the marker must name this launch"
        );
        // 0700, as the old `DirBuilder::new().mode(0o700)` guaranteed.
        assert_eq!(
            protocol::fsperm::mode_of(&dir).unwrap(),
            0o700,
            "the run dir must stay private through the rename"
        );
        // Nothing staged is left lying around on the success path.
        let staged = dir.with_file_name(format!("cc-host-atomic-{}.tmp", std::process::id()));
        assert!(
            !staged.exists(),
            "the staging dir must be gone once published: {}",
            staged.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A11.1: the pid frame is built by hand because `pre_exec` may not allocate.
    /// Hand-rolled integer formatting is exactly the kind of thing that is right
    /// for four years and then wrong for pid 1000000, so it is pinned here.
    #[test]
    fn the_fence_pid_frame_round_trips() {
        for pid in [1, 7, 42, 999, 1000, 99999, 1_000_000, i32::MAX] {
            let mut frame = [0u8; FENCE_ID_LEN];
            encode_fence_pid(pid, &mut frame);
            assert_eq!(
                decode_fence_pid(&frame),
                Some(pid),
                "frame {:?} did not round-trip",
                String::from_utf8_lossy(&frame)
            );
        }
        // A frame that never got written is not a pid 0 — it is unreadable.
        assert_eq!(decode_fence_pid(&[b' '; FENCE_ID_LEN]), None);
        assert_eq!(decode_fence_pid(&[0u8; FENCE_ID_LEN]), None);
    }

    /// A11.1, the gate itself: a child whose identity could NOT be recorded never
    /// becomes the target program.
    ///
    /// The window this closes: the child used to be running codex by the time its
    /// pid could be read, so a host SIGKILLed between the spawn and the record left
    /// a live process nobody had written down and nobody could later name. Here the
    /// recording fails (there is no launch record for this uid), which is the same
    /// "the host did not get to write it down" outcome a SIGKILL produces — and the
    /// proof is that the child's `execve` never ran at all.
    #[tokio::test]
    async fn an_unrecordable_child_never_execs() {
        let args = parse_host_args(&complete(&[]))
            .map(|mut a| {
                // A uid with no launch record, so `record_child_pid` must fail.
                a.uid = "01JQXV9K7B8N4M2P6R3T5WZZZZ".into();
                a
            })
            .expect("charter");

        let witness = std::env::temp_dir().join(format!("cc-fence-execd-{}", std::process::id()));
        let _ = std::fs::remove_file(&witness);

        // If this child is ever released, it exec's `/bin/sh` and the witness
        // appears. That file is the whole assertion.
        //
        // Deliberately NOT `kill_on_drop`, and deliberately polled rather than
        // checked once: a released child races the drop that would kill it, so an
        // immediate `!exists()` can pass by luck while the child is mid-`execve`.
        // Nothing may kill it and the window must be generous, or this test reports
        // a fence that is not there. (It was written the racy way first, and a
        // mutation that released every child still passed.)
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("printf execd > {}", witness.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let err = spawn_fenced(&mut cmd, &args, "witness")
            .expect_err("an unrecordable child must fail the spawn");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            assert!(
                !witness.exists(),
                "the fenced child must never have exec'd, but it wrote {}: {err:#}",
                witness.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&witness);
    }

    /// **A11.1, readiness half: what `exec_confirmed` is allowed to rest on.**
    ///
    /// `spawn()` returning `Ok` is not it. What the parent observes is the CLOEXEC
    /// error pipe closing, and that happens both when `execve` closes it and when
    /// the child dies holding it — measured (round-2 finding 2): a child SIGKILLed
    /// after the fence's GO byte and before `execve` yields `spawn()` → `Ok(pid)`
    /// with a recorded child that never became codex.
    ///
    /// The three answers the discriminator must give, each staged from a process
    /// whose state is known rather than raced:
    ///
    ///   * a process still running THIS binary — the shape of a fenced child before
    ///     `execve`, since `fork` gives it our image — is refused;
    ///   * one that has genuinely exec'd a different program is accepted;
    ///   * one that is dead is refused, however it died.
    #[test]
    fn only_a_live_child_running_a_different_image_proves_execve() {
        use protocol::proc_identity::{current_identity, read_birth_identity, ProcessIdentity};

        // 1. Our own process: alive, and running the image a pre-exec child runs.
        let me = current_identity().expect("this process's identity");
        let err = prove_past_execve(&me, "witness")
            .expect_err("a process still running this binary has not passed execve");
        assert!(
            format!("{err:#}").contains("has not passed execve"),
            "and it must be refused FOR that reason: {err:#}"
        );

        // 2. A child that really did exec a different program.
        // `std`'s Command, not the module's tokio one: this test is synchronous,
        // and `std::process::Command::spawn` is the very call whose return value
        // this function exists to stop standing in for a proof.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id() as i32;
        let birth = read_birth_identity(pid).expect("the child's birth stamp");
        let exec_d = ProcessIdentity { pid, birth };
        prove_past_execve(&exec_d, "witness")
            .expect("a live child running /bin/sleep is past execve");

        // 3. The same child, dead. `exec_confirmed` is a fact about a moment that
        //    has passed; a child that is gone must not certify anything.
        child.kill().expect("kill");
        child.wait().expect("reap");
        let err = prove_past_execve(&exec_d, "witness")
            .expect_err("a dead child proves nothing about a live session");
        assert!(
            format!("{err:#}").contains("not proven live"),
            "and liveness must be the reason, not the image: {err:#}"
        );
    }

    /// **The image discriminator compares files, not spellings** (round-3 finding 9).
    ///
    /// Darwin reaches one inode by two paths — `/x` and `/System/Volumes/Data/x` —
    /// and a byte compare calls that "a different image", which is the answer that
    /// would certify a child still parked pre-`execve` as having exec'd. Staged
    /// against a real alias of a real file rather than argued about: the two paths
    /// are asserted UNEQUAL as strings first, so the test cannot pass by the alias
    /// having quietly stopped existing, and `same_image` is then required to say
    /// they are one file anyway.
    #[test]
    fn the_image_compare_sees_through_the_data_volume_alias() {
        let ours = std::env::current_exe().expect("this test binary's own path");
        let aliased = std::path::Path::new("/System/Volumes/Data").join(
            ours.strip_prefix("/")
                .expect("current_exe is absolute on this platform"),
        );
        if !aliased.exists() {
            // The alias is a property of the volume layout, not of this code. If it
            // is not present the premise does not hold and there is nothing to
            // assert — but say so rather than passing silently.
            eprintln!("no data-volume alias for {}; skipping", ours.display());
            return;
        }
        assert_ne!(
            ours, aliased,
            "the premise: the alias must be a DIFFERENT spelling, or this proves nothing"
        );
        assert!(
            same_image(&ours, &aliased).expect("both images are stat-able"),
            "one file reached by two paths must compare as one image: {} vs {}",
            ours.display(),
            aliased.display()
        );
        // …and the discriminator still separates genuinely different files, so the
        // fix above is not "say yes to everything".
        assert!(
            !same_image(&ours, std::path::Path::new("/bin/sleep"))
                .expect("both images are stat-able"),
            "two different files must still compare as different images"
        );
        // An image question that cannot be ASKED refuses rather than answering.
        same_image(
            &ours,
            std::path::Path::new("/nonexistent/codeconnect/image"),
        )
        .expect_err("an unstat-able image is an unanswered question, not a difference");
    }

    /// **A11.1, the window itself: a child that dies in the confirmation interval
    /// is not confirmed** (round-2 finding 2).
    ///
    /// `spawn_fenced`'s own wiring, not just the predicate. The fence releases the
    /// child and the parent then has to get back out of `spawn()`; a child killed in
    /// that interval closes the CLOEXEC error pipe by dying rather than by exec'ing,
    /// which `spawn()` cannot tell apart — measured: `Ok(pid)`, a successful
    /// recording, and a process that never ran the target program. So the kill is
    /// staged from inside the releaser, at the one instant no test can reach from
    /// outside, and the whole call must refuse.
    #[tokio::test]
    async fn a_child_that_dies_in_the_confirmation_window_is_not_confirmed() {
        use crate::codex_launch as cl;
        use protocol::proc_identity::{current_identity, monotonic_now_nanos};

        let mut args = parse_host_args(&complete(&[])).expect("charter");
        args.uid = "01JQXV9K7B8N4M2P6R3T5WEXEC".into();

        // A launch this host holds the lease on, so the recording inside the fence
        // succeeds and the ONLY thing left to refuse on is the exec proof.
        let me = current_identity().unwrap();
        let lock = cl::LaunchLock::acquire(&args.uid).unwrap();
        cl::create_pending(
            &lock,
            cl::NewLaunch {
                launch_nonce: args.nonce.clone(),
                uid: args.uid.clone(),
                session_name: "cc-exec".into(),
                coordinator: me,
                boot: protocol::proc_identity::boot_identity().unwrap(),
                deadline_monotonic_nanos: monotonic_now_nanos().unwrap() + 60_000_000_000,
                created_ms: 1,
            },
        )
        .unwrap();
        cl::cas_custodian_with_child(&lock, &args.uid, me, me.pid, "t-nonce", "t-hash").unwrap();
        assert_eq!(
            cl::admit_host(&lock, &args.uid, &args.nonce, &me, me.pid, "codex-host").unwrap(),
            cl::Admission::Admitted
        );
        drop(lock);

        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        kill_next_child_after_go();
        let err = spawn_fenced(&mut cmd, &args, "app-server")
            .expect_err("a child that died in the confirmation window must not be confirmed");

        // THE GATE. Without the exec proof on this path, `spawn()` returned `Ok`,
        // the recording returned `Ok`, and `exec_confirmed: true` went into the
        // record for a process that is not running.
        let record = cl::load(&args.uid).unwrap();
        assert!(
            !cl::host_children_ready(&record),
            "no child may count as recorded-and-confirmed: {err:#}"
        );
        assert!(
            record.children.iter().all(|c| !c.exec_confirmed),
            "and exec_confirmed must not have been written for any of them"
        );
    }

    /// A11.4: a marker that cannot be written publishes NOTHING — neither the final
    /// name nor the staging dir survives. A host that does not come up leaves
    /// nothing behind, and a claimed-but-unmarked directory is worse than none.
    #[test]
    fn a_marker_that_cannot_be_written_publishes_nothing() {
        let dir = std::env::temp_dir().join(format!("cc-host-nomarker-{}", std::process::id()));
        let staged = dir.with_file_name(format!("cc-host-nomarker-{}.tmp", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&staged);

        // A newline forges a second marker field, so `write_owner_marker` refuses it.
        let err = create_run_dir_atomically(&dir, "uid-c", "nonce\nc")
            .expect_err("a refused marker must fail the whole publish");
        assert!(
            format!("{err:#}").contains("newline"),
            "the marker's own refusal must surface: {err:#}"
        );
        assert!(
            !dir.exists(),
            "nothing may appear at the final run-dir path"
        );
        assert!(
            !staged.exists(),
            "and the staging dir must be cleaned up: {}",
            staged.display()
        );
    }
}
