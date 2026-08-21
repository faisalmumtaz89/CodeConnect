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
//! universal exclusion: it excludes **other uids**, not a hostile process running
//! as *this* uid, which could still create a socket inside the directory between
//! the `mkdir` and the readiness poll. That process is out of scope by
//! construction — a same-uid attacker can already `ptrace`, signal, or replace the
//! binaries this host execs, so no filesystem check here would be a boundary
//! against it. What the host actually relies on is: a **trusted parent path**
//! (the host does not validate the parent chain, which the coordinator owns) plus
//! a non-hostile same-uid environment. Within that premise, freshness is enforced
//! by the host itself, and nothing else about the caller is trusted.
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
//! internal-codex-host --uid … --nonce … --tmux-socket … --codex … --run-dir …`.
//! Before anything exists — no run dir, no sockets, no children — it presents
//! itself to the D7 launch gate ([`crate::codex_custodian::late_host_admission`])
//! and is admitted or refused; a refused host destroys its own uid's session and
//! exits having created nothing. `codeconnect codex` itself is still GATED and
//! refuses, so nothing but the tests reaches any of this yet.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use codex_broker::relay::{Broker, EventSink};
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
/// handle — a change to shared D6 machinery in service of one caller. So the
/// window is minimised here and the residual is a documented pre-ungate gate.
const RECORD_LOCK_BUDGET: Duration = Duration::from_secs(1);

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
    /// The host takes this on trust and does **not** re-resolve, native-check or
    /// version-pin it — `codex::resolve_codex_bin` / `ensure_pinned_version` run in
    /// the caller (2e-2b's coordinator), which is what makes the reserved argv
    /// grammar's 0.147 grounding apply. Stated plainly because it is a real
    /// premise: `internal-codex-host --codex /any/path` execs that path twice.
    /// Same-uid, so not a privilege boundary — but not a guarantee this file makes.
    codex: PathBuf,
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
/// `--codex`, `--run-dir`, `--codex-home` and **all four fingerprint dimensions**
/// are required: the host applies no policy default, because a default is a
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
    let mut uid: Option<String> = None;
    let mut nonce: Option<String> = None;
    let mut tmux_socket: Option<String> = None;
    let mut run_dir: Option<PathBuf> = None;
    let mut codex_home: Option<PathBuf> = None;
    let mut approval_policy: Option<String> = None;
    let mut approvals_reviewer: Option<String> = None;
    let mut sandbox: Option<String> = None;
    let mut hooks_enabled: Option<bool> = None;
    let mut tui_args = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        match flag {
            "--codex" => set_once(&mut codex, flag, PathBuf::from(value_of(&mut it, flag)?))?,
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

    Ok(HostArgs {
        codex: codex.context("--codex <path> is required")?,
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
    // written immediately below is what then makes the directory say whose it is,
    // since its NAME cannot (the derivation is many-to-one).
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&args.run_dir)
        .with_context(|| {
            format!(
                "creating the host run dir {} exclusively — it must NOT already exist \
                 (the host owns it and refuses to adopt a directory it did not create)",
                args.run_dir.display()
            )
        })?;

    // Claim it, immediately and durably. The `mkdir` above proves the directory is
    // fresh; this says WHOSE it is, and the two must be inseparable — the marker is
    // written before anything else can exist inside, by the only process that could
    // have created it.
    //
    // It is what lets the other two actors stop trusting the NAME. The coordinator
    // will not accept sockets under this directory as evidence for a launch the
    // marker does not name, and the custodian will not delete a directory whose
    // marker is not its own — both real hazards, because the run-dir derivation is
    // many-to-one and the custodian deletes a path recorded before it existed.
    //
    // A failure here removes the directory and aborts: the invariant is that a host
    // which does not come up leaves nothing behind, and a claimed-but-unmarked
    // directory is worse than none — nobody could later prove whose it was.
    if let Err(err) = crate::codex_launch::write_owner_marker(&args.run_dir, &args.uid, &args.nonce)
    {
        let _ = std::fs::remove_dir_all(&args.run_dir);
        return Err(err);
    }

    let outcome = run_session(&args, &paths, &mut signals).await;

    // `run_session` has already run teardown, so each child has been given a
    // bounded best-effort stop and the aborted broker task was given
    // BROKER_ABORT_BUDGET to finish. None of that is a proof — `start_kill` can
    // fail, a reap can time out, a task can ignore its cancellation point — and
    // teardown reports (log + stderr) whatever it could not show stopped. This
    // sweep runs either way. It is safe that it does: `remove_dir_all` is
    // openat-based, and unlinking a bound socket or an open log file is harmless to
    // the fd holding it — a straggler that outlived the budget keeps working
    // against an unlinked inode rather than corrupting anything.
    //
    // Best-effort — a cleanup failure must not mask the session outcome — but NOT
    // silent. The module doc says the host *attempts* to remove the run dir, and an
    // attempt that failed with no record is how a leak becomes invisible.
    if let Err(err) = std::fs::remove_dir_all(&args.run_dir) {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "codex-host: could not remove the run dir {}: {err}",
                args.run_dir.display()
            );
        }
    }

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
    let sink = match file_event_sink(&paths.broker_log) {
        Ok(s) => s,
        Err(err) => return Outcome::Fatal(format!("{err:#}")),
    };

    // --- The first spawn. Nothing fallible may run before the guard owns it --
    let appserver = Command::new(&args.codex)
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
        .process_group(0)
        // Safety net beneath the explicit teardown: even an unwind or an
        // early return that somehow skipped `teardown` cannot leak this child.
        .kill_on_drop(true)
        .spawn();
    let appserver = match appserver {
        Ok(child) => child,
        Err(err) => {
            return Outcome::Fatal(format!(
                "spawning codex app-server ({}): {err}",
                args.codex.display()
            ))
        }
    };

    // Record the app-server's identity BEFORE anything depends on the session
    // being up. The coordinator commits `ready` once the broker's legs are bound,
    // which is strictly after this point, so a session that is ever declared ready
    // has a recorded, signalable app-server behind it.
    if let Err(err) = record_child(args, "app-server", &appserver) {
        // Launch-fatal. Nothing is up beyond this child and `Session` has not taken
        // it, so returning here drops it through `kill_on_drop`. A session whose
        // processes cleanup cannot name is the leak this chunk exists to remove.
        return Outcome::Fatal(format!("{err:#}"));
    }

    let mut session = Session::new(appserver, sink);
    let outcome = drive(&mut session, args, paths, signals, &mut as_stderr_file).await;
    session.teardown().await;
    match outcome {
        Ok(outcome) => outcome,
        Err(err) => Outcome::Fatal(format!("{err:#}")),
    }
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
fn record_child(args: &HostArgs, role: &str, child: &Child) -> Result<()> {
    let pid = child
        .id()
        .with_context(|| format!("the {role} child has already been reaped"))? as i32;
    let (Some(birth), Some(pgid)) = (
        protocol::proc_identity::read_birth_identity(pid),
        protocol::proc_identity::read_pgid(pid),
    ) else {
        bail!("could not read {role}'s (pid {pid}) birth identity and process group");
    };
    let entry = crate::codex_launch::ChildEntry {
        role: role.to_string(),
        identity: protocol::proc_identity::ProcessIdentity { pid, birth },
        pgid,
        nonce: args.nonce.clone(),
        argv_hash: String::new(),
        // Stamped by `record_host_child` from the verified lease holder.
        recorded_by: None,
    };
    let me = crate::codex_launch::require_current_identity()?;
    // A SHORT lock wait, deliberately, because this call sits inside a window.
    //
    // The child is already running by the time its identity can be read, so there
    // is an interval between the spawn and the record in which a SIGKILLed host
    // leaves a live child nobody has written down. The interval is bounded by how
    // long this takes, and the lock is the only part that can stretch — so it gets
    // a tight budget rather than the five seconds the non-urgent writers use. A
    // launch that cannot get the lock in a second is failed, which is the same
    // fail-closed answer as any other recording failure.
    //
    // This narrows the window; it does not close it. Closing it needs the child
    // spawned inert and released only after its identity is durable — see
    // `RECORD_LOCK_BUDGET`.
    let lock = crate::codex_launch::LaunchLock::acquire_bounded(&args.uid, RECORD_LOCK_BUDGET)?;
    crate::codex_launch::record_host_child(&lock, &args.uid, &me, entry)
        .with_context(|| format!("recording {role} in the launch record"))
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
    .with_event_sink(Arc::clone(&session.log));
    session.broker = Some(tokio::spawn(broker.serve()));

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
    let tui = Command::new(&args.codex)
        .arg("--remote")
        .arg(format!("unix://{}", paths.tui_sock.display()))
        .args(&args.tui_args)
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
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning the codex TUI ({})", args.codex.display()))?;
    // Recorded before the TUI is handed to the session guard, but the result is
    // checked AFTER, so a failure aborts through the guard that already owns both
    // children rather than leaking the one just spawned.
    let recorded = record_child(args, "tui", &tui);
    session.tui = Some(tui);
    recorded?;

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
        }
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
/// uid, but is not a claim about a hostile process running as this same uid. That
/// process is out of scope by construction; see invariant 1 in the module doc for
/// the actual premise (trusted parent path, non-hostile same-UID environment).
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
        assert_eq!(a.run_dir, PathBuf::from("/tmp/cc.host.1"));
        assert_eq!(a.codex_home, PathBuf::from("/tmp/cc.home.1"));
        assert_eq!(a.fingerprint.approval_policy, "untrusted");
        assert_eq!(a.fingerprint.approvals_reviewer, "user");
        assert_eq!(a.fingerprint.sandbox, "read-only");
        assert!(a.fingerprint.hooks_enabled);
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
        ]))
        .unwrap();
        assert_eq!(a.fingerprint.approval_policy, "on-request");
        assert_eq!(a.fingerprint.approvals_reviewer, "codex");
        assert_eq!(a.fingerprint.sandbox, "workspace-write");
        assert!(!a.fingerprint.hooks_enabled);
        assert!(a.tui_args.is_empty());
    }

    #[test]
    fn every_required_flag_is_required() {
        // Dropping any one of the seven required flags fails closed. Each pair is
        // the flag name and its value's position in `complete`.
        for drop in [
            "--uid",
            "--nonce",
            "--tmux-socket",
            "--codex",
            "--run-dir",
            "--codex-home",
            "--approval-policy",
            "--approvals-reviewer",
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
    #[test]
    fn an_existing_run_dir_is_refused() {
        let dir = std::env::temp_dir().join(format!("cc-host-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect_err("a non-recursive create must refuse an existing dir");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
