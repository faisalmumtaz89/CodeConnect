//! The `codeconnect codex` launcher foundation: binary resolution and the
//! reserved argv grammar.
//!
//! This is the pure, heavily-tested front of the Codex launcher. It resolves the
//! `codex` executable exactly as `claude` is resolved (config → env → well-known
//! → `PATH`, with the same self-resolution guard so an
//! `alias codex=codeconnect codex` cannot spawn-loop), requires the resolved file
//! to be the **native standalone executable** and not a `#!`-script / `.js`
//! wrapper (which could swap the real CLI out from under a pinned path), pins it
//! by **byte identity** rather than by pathname so the bytes inspected here are
//! provably the bytes every later `execve` runs (see [`ResolvedCodex`] and
//! [`verify_codex_identity`]), and reads the user's argv the way codex does.
//!
//! **The command is live.** [`start`] resolves, argv-validates and preflights the
//! daemon — surfacing every one of those failures honestly and before anything
//! exists — and then launches: it mints the session identity, spawns the launch
//! coordinator, and waits on the durable launch record. See [`start`] for the
//! boundary this launch path accepts, and [`launch`] for the shape it shares with
//! `codeconnect claude`.
//!
//! **The keyboard is as trusted as native codex.** Every flag reaches the TUI —
//! sandbox, approval, profile, `-c` and feature flags included, and flags this walk
//! does not know, unchanged — with three structural exceptions:
//! `--remote`/`--remote-auth-token-env` (the TUI must be the broker's client, or there
//! is no session for the phone to reach), subcommands (only the interactive session is
//! hosted: a new one, or `resume`/`fork` as the first positional — see
//! [`HOSTED_SUBCOMMANDS`]), and `--cd`, which becomes the session folder (see
//! [`session_folder`]).
//! `--help`/`--version` run codex itself and create no session ([`codex_itself`]). The
//! parser is a real parser, not a denylist scan: it normalizes spaced, `=`-joined and
//! attached short forms (`-C.`, `-mgpt-5`, `-ca=b`), knows the arity of the flags in
//! its table (so it can tell a flag's value from the next token, and a bare prompt from
//! a subcommand), honours the `--` boundary, and refuses subcommand **names and
//! aliases** anywhere codex would dispatch one. Flag arities and short forms were
//! probed on the live binary with invalid-enum sentinels, and the subcommand set and
//! its hidden entries/aliases come from clap's own completion output.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use protocol::config::Config;

/// Environment override for the `codex` binary, mirroring
/// `CODECONNECT_CLAUDE_BIN`.
const CODEX_BIN_ENV: &str = "CODECONNECT_CODEX_BIN";

/// A resolved `codex` executable: a pathname **and the identity of the bytes
/// behind it**.
///
/// [`path`](Self::path) is the fully-canonicalised versioned executable: the
/// invocation candidate with every symlink resolved. On a standalone install the
/// invocation hops through a moving `standalone/current` symlink to a
/// version-stamped release directory. Resolution canonicalises **once** and
/// fails closed if it cannot, and this single path is what is asked its version,
/// recorded as launch evidence and exec'd by the app-server and TUI alike, so a
/// `standalone/current` flip cannot make the recorded, checked and executed
/// binaries disagree: all spawned Codex processes use the same resolved
/// executable.
///
/// # Why the digest exists
///
/// **A canonical path is a name, not an executable.** Canonicalising pins which
/// name is used; it says nothing about which bytes that name reaches at any later
/// instant. Between resolution and the last `execve` this launch performs, the
/// pathname is opened by the kernel three separate times — `codex --version`, the
/// app-server spawn, the TUI spawn — in two different processes, and every one of
/// those opens is free to see a different file. An install, an `npm` replacement or
/// a `standalone/current` flip landing in that window would let the bytes that ran
/// differ from the bytes that were magic-checked and hashed.
///
/// [`sha256`](Self::sha256) closes it by carrying the *identity* forward instead of
/// the name alone: it is the SHA-256 of the exact bytes read during resolution,
/// **from the same single read whose first four bytes produced the Mach-O verdict**
/// (see [`inspect_candidate`]). Every site that is about to run this binary
/// re-derives the digest from the path and refuses on a mismatch
/// ([`verify_codex_identity`]), so a replacement anywhere along
/// inspect → version → coordinator → app-server → TUI is caught rather than
/// executed.
///
/// # The hash alone was not enough, and that was measured, not argued
///
/// A digest is taken through an **open file**; an `execve` is performed on a
/// **pathname**. A hash of this binary is about half a second of wall clock
/// (measured on the real 219,997,536-byte codex 0.147: 0.478 / 0.459 / 0.460 s
/// through the release-built hasher, ~8 s unoptimised), and an atomic `rename`
/// landing anywhere inside it leaves the read completely undisturbed — the fd still
/// refers to the old vnode, the digest still equals the pin, the check *passes*, and
/// the spawn that follows opens the name afresh and runs the replacement. That was
/// staged end to end against the real binary: three of three runs the digest matched
/// exactly and the substituted executable ran. So the window was never "the moment
/// before the spawn"; it was the entire read, at every one of the three sites.
///
/// Both the resolution read ([`inspect_candidate`]) and every verification read
/// ([`protocol::hash::sha256_file`]) therefore hold the fd open across a comparison
/// of `(st_dev, st_ino)` between the handle and the name — one rule,
/// [`protocol::hash::refuse_unless_path_still_names`], written once and applied at
/// both kinds of site.
///
/// # What it still does **not** claim
///
/// The residual is now the interval between that `stat` and the kernel's own open
/// inside `execve` — microseconds rather than half a second — and on macOS it cannot
/// be closed at all, because there is no way to exec the handle that was hashed.
/// Measured on this platform: `fexecve` is not declared anywhere in the SDK (a call
/// to it fails to compile and `grep -rl fexecve` over the SDK headers matches
/// nothing); `posix_spawn` has no descriptor-based variant; and
/// `execve("/dev/fd/N", …)` returns `EACCES` for a read handle *and* for an `O_EXEC`
/// handle, while an `O_EXEC` descriptor cannot be `read` at all (`EBADF`) and so
/// could never have been hashed. See
/// [`protocol::hash::refuse_unless_path_still_names`] for the full measurement.
///
/// Nor can any verify-by-content scheme see a replacement that is *reverted* before
/// the check runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCodex {
    pub path: PathBuf,
    /// Lowercase-hex SHA-256 of the whole file, as read during resolution.
    pub sha256: String,
}

/// The `codex` command entry point.
///
/// Resolves the binary, validates the argv against the reserved grammar, reads the
/// binary's version, resolves the session folder, preflights the daemon — every one
/// of those can fail with its own honest error, and all of them fail *before anything
/// exists* — and then launches ([`launch`]). An argv that asks for codex's help or
/// version is handed to codex itself first, instead ([`codex_itself`]).
///
/// # THE ACCEPTED BOUNDARY (the single place it is stated)
///
/// Everything above this function was built to hold a launch closed against a
/// binary that is not the one it inspected, a daemon that could never be told
/// about the session, a passthrough that detaches the TUI from the broker, and
/// a rolled-back peer that would file the run as the wrong agent. One class of
/// attacker is deliberately **out of scope**, and this is the place to say so
/// once, plainly, rather than to leave it implied by a dozen local caveats:
///
/// **A hostile process already running as the user's own uid is not defended
/// against.** It can `ptrace` this process, signal it, replace the binaries it is
/// about to `execve`, revoke the `UF_IMMUTABLE` freeze
/// ([`protocol::hash::FrozenExecutable`]) on the pinned executable, or hold a
/// writable descriptor opened before that freeze was ever set. None of those has a
/// userland answer on macOS: the mechanism that would close them — hashing a
/// descriptor and then executing *that descriptor* — does not exist on this
/// platform, and its absence was measured rather than assumed (`fexecve` is not
/// declared in the SDK; `execve("/dev/fd/N", …)` returns `EACCES` for a readable
/// handle and for an `O_EXEC` handle alike, and an `O_EXEC` handle cannot be read
/// and so could never have been hashed). This is the same posture
/// [`crate::codex_host`]'s invariant 1 already states for its run directory, and
/// it is stated **there by reference to here** so the two cannot drift into two
/// different boundaries.
///
/// **What is defended, and must stay defended:** every cross-uid vector, and — for
/// **the direct pinned executable** — the benign update race. A `codex` install or
/// update landing mid-launch changes the file the resolved `--codex` pathname names,
/// and that is refused at all three exec sites ([`verify_codex_identity`], with the
/// freeze held across each `execve`); `PATH`/`./` confusion is refused
/// ([`require_absolute_codex`]); cross-boot pid reuse is refused by boot identity; a
/// rolled-back daemon's storage is isolated; and the wire pins hold. Stated that
/// narrowly on purpose: the claim is about the bytes behind one pathname, not about
/// every race a launch can lose.
///
/// AMFI narrows one thing here and not another, and the difference is worth being
/// exact about, because page-hash validation checks an image against **its own**
/// signature. So mutating the bytes of the signed codex image under a running
/// process is bounded to denial of service — a page-hash mismatch is a `SIGKILL`,
/// not silently executed foreign code. It does **not** reduce *substitution of a
/// different, validly-signed binary* to denial of service: that image validates
/// against its own signature and runs, and nothing in this launch path enforces a
/// codex signing identity to compare it against.
///
/// **Separate accepted residuals — not instances of the boundary above, because
/// neither of them needs a hostile actor at all:**
///
///   * **The pgid-reuse gap**, a benign within-boot pid/pgid-reuse TOCTOU: the
///     custodian's group kill can land on an unrelated same-uid process that
///     inherited a recycled group id, with nobody attacking anything
///     (`codex_custodian::group_warrant` carries the measurement). Closing it
///     needs env-nonce provenance via `KERN_PROCARGS2`.
///   * **The unpinned dispatcher subordinates**, a benign package update: the
///     native `--codex` dispatcher is pinned faithfully and completely, but the
///     subordinates it selects for `--version`, `app-server` and the TUI are not,
///     so an ordinary update can change what actually runs while the pinned
///     dispatcher's own bytes are unchanged and every gate here passes. Closing
///     it needs a package-layout specification, which is a scoping decision about
///     what a supported install is; see [`is_native_magic`].
pub fn start(passthrough: &[String]) -> Result<()> {
    let config = Config::load();

    // `--help` and `--version` are codex's own answers, printed in this terminal as
    // native `codex` prints them — before any refusal and any launch check, because
    // nothing is launched. See `codex_itself`.
    if asks_help_or_version(passthrough) {
        use std::os::unix::process::CommandExt;
        let codex = first_codex(codex_candidates_for(&config))?;
        let err = codex_itself(&codex, passthrough).exec();
        return Err(err).with_context(|| format!("running {}", codex.display()));
    }

    // Binary first, exactly as the Claude path resolves its binary first: a
    // missing or unusable executable must surface before anything else. The
    // canonicalised path is what we check and exec.
    let resolved = resolve_codex_bin(&config)?;
    // Reserved grammar. A refused flag or subcommand surfaces here, naming what
    // was refused and why, before anything is created.
    let argv = scan_codex_argv(passthrough).map_err(|refusal| anyhow!("{refusal}"))?;
    // **The freeze a previous launch could not give back is cleared HERE, before this
    // one takes a freeze of its own.** See `clear_freezes_left_standing`.
    clear_freezes_left_standing();
    // The binary must say what it is before it is hosted. See `probe_codex`.
    probe_codex(&resolved)?;

    // A `--cd` that names no directory stops the launch here, before anything exists.
    let caller_cwd = std::env::current_dir().context("reading the current directory")?;
    let folder = session_folder(&argv.cd, &caller_cwd)?;

    // Daemon preflight, before anything is created. Ordering is load-bearing and
    // is what `new-old-new-real.sh` step 7(g) drives: a rolled-back daemon must
    // refuse the launch with no uid minted, no tmux name taken, no coordinator
    // spawned and no record written.
    refuse_unless_hostable(crate::daemon::agent_support(
        &protocol::agent::AgentKind::Codex,
    ))?;

    launch(&resolved, &folder, &argv.normalized)
}

/// Retry, before this launch freezes anything of its own, the clears that were left
/// owed on a codex binary — and say what came of each.
///
/// **This is one of the two production callers the recovery pass did not have**, and
/// without one the pass was machinery that ran when somebody ran it. The counterexample
/// it closes needs no attacker and no failure: a launch that set the `UF_IMMUTABLE`
/// pin is `SIGKILL`ed inside the hash→exec window; a concurrent launch had adopted the
/// same bit and an adopter never clears; the first launch's custodian stops waiting,
/// keeps the claim standing and exits (which is right — a custodian that kept polling
/// would be one idle process per leaked flag). The flag is then on a real binary with
/// no live holder and no actor. `codex` still RUNS, but it cannot be updated, and the
/// operator's way out was to work out for themselves that `chflags nouchg` was needed.
/// Now the next launch takes it off.
///
/// **Why the launcher, when the daemon sweeps too.** The daemon's tick is slow on
/// purpose and a Mac may have no daemon running at all. The launcher is the actor with
/// the motive: it is about to hash and freeze the very file a leaked claim names, and
/// an update refused because of a flag nobody owns is refused at exactly this moment.
///
/// **Why BEFORE the probe.** `probe_codex` holds the executable's own freeze lock for
/// its whole run, and this pass needs that same lock to prove that no live holder
/// stands behind the vnode. Run afterwards it would meet the launch's own lock, defer,
/// and clear nothing; run inside it, it would be reasoning about a bit this process
/// had just set. Before is the only position from which the answer is about anybody
/// else.
///
/// **What it costs a launch that has nothing to repair, which is every healthy one.**
/// A record with no freeze claim costs a read. A record whose claim names a holder that
/// is NOT proven dead — the live launch case, and the only one a busy machine has —
/// costs a liveness question and no lock at all: the holder is judged before the
/// executable's lock is reached, so a concurrent launch's pin is never something this
/// waits on. Only a claim whose holder is already proven dead reaches the lock. That
/// one is bounded by the lock's own budget and the withdrawal's, and the PASS is
/// bounded by [`crate::codex_launch::LAUNCH_FREEZE_SWEEP_BUDGET`] over all of them —
/// per-step bounds multiply, and a launcher that inherited the product of them would
/// be a human at a terminal waiting on other launches' wreckage with nothing said.
///
/// **What it does before the launch is admitted.** This runs ahead of the argv
/// grammar and the daemon preflight, so a launch that goes on to refuse may already
/// have taken a flag off a binary and withdrawn another launch's claim. That is not
/// this launch acting early: it is repair of somebody else's wreckage, owed whatever
/// this launch turns out to be, and it mints no uid, takes no tmux name and writes no
/// record of its own — which is what the rollback arm's "a refusal creates nothing"
/// is about. It cannot go later: the probe is what it must precede, and the probe is
/// itself ahead of both gates.
///
/// **It cannot end a launch, and it cannot take one's pin off.** Every warrant is
/// [`crate::codex_launch::resolve_standing_freeze`]'s — a holder proven dead by
/// identity, the executable's lock held across the scan and the clear, no other record
/// naming the vnode with a holder that is not proven dead, and an adopted claim
/// licensing nothing but its own withdrawal. Anything short of all four leaves the
/// flag exactly where it was. So the worst this can do is spend its bounded wait and
/// say what it was waiting for, which is why nothing here is fallible to the caller.
fn clear_freezes_left_standing() {
    let deadline = std::time::Instant::now() + crate::codex_launch::LAUNCH_FREEZE_SWEEP_BUDGET;
    crate::codex_launch::sweep_standing_freezes_each(deadline, &mut |action| {
        if let Some(line) = launcher_line(&action) {
            eprintln!("codeconnect: {line}");
        }
    });
}

/// What a launch says at the terminal about one thing the pass did, or `None` for
/// the ones it says nothing about.
///
/// Split from the loop so the choice is a value a test can read: the whole subject
/// here is which outcomes reach a human and which do not, and `eprintln!` from inside
/// a function that shells out to a real codex is not something a test can see.
fn launcher_line(action: &crate::codex_launch::SweepAction) -> Option<String> {
    use crate::codex_launch::SweepAction;
    match action {
        // A flag coming off a real binary is worth reading, and this is the one
        // place a human is present to read it.
        SweepAction::FreezeClaimSettled { uid, what } => Some(format!("{uid}: {what}")),
        // **The ordinary case is NOT printed here, and that is a decision about whose
        // output this is.** A claim left standing is almost always a live launch
        // running on those bytes, so on a machine with sessions on it this would be
        // said on every launch, for ever, naming other sessions' uids at a terminal
        // where somebody is waiting for a TUI. The distinction between a pass that
        // said nothing and one that found nothing is owed by the daemon's pass, which
        // has a log to put it in.
        SweepAction::FreezeClaimStanding { .. } => None,
        // **A record nobody could look at IS said, and it is not the same case.** One
        // `launch.json` that will not parse makes every clear on the machine defer,
        // for ever — so the flag stays on the binary, codex cannot be updated, and
        // the launcher used to be silent about both halves of that. It is rare by
        // construction (records are published by rename), so saying it does not
        // reintroduce the per-launch noise the arm above avoids.
        SweepAction::Skipped { uid, why } => Some(format!(
            "{uid}'s launch record could not be examined: {why}"
        )),
        SweepAction::ScanFailed { what, why } => {
            Some(format!("{what} could not be looked at: {why}"))
        }
        // Said, because this one explains an absence: a flag that is still on the
        // binary and a pass that stopped before it got there.
        SweepAction::RanOutOfTime { unreached } => Some(format!(
            "gave up clearing leftover codex freezes after {:?} with {unreached} launch \
             record(s) unexamined; if codex cannot be updated, the daemon's own pass will \
             come back for it",
            crate::codex_launch::LAUNCH_FREEZE_SWEEP_BUDGET
        )),
        // Unreachable by construction: `sweep_standing_freezes` repairs no record's
        // lifecycle and takes no launch lock to be contended for. Stated as an arm
        // rather than a `_`, because a wildcard here would silently absorb the day
        // somebody widened that scope and put the launcher back in the business of
        // filing other launches as failed on the path where a human is waiting for a
        // TUI.
        other => Some(format!(
            "the pre-launch freeze pass returned {other:?}, which it has no authority to \
             act on; it is reported and ignored"
        )),
    }
}

/// Refuse the launch when the daemon that is running cannot host Codex.
///
/// **The one case this exists for is a rollback.** A machine whose `ccd` has
/// been rolled back to a build that predates the agent seam still has this
/// launcher on it, and a Codex session started against that daemon is a session
/// it can never be told about: the supervisor asks the same question this asks,
/// reads the same answer, and withholds its registration for the life of the run
/// (`crate::supervisor::withhold_unless_hosted`). The run works — the TUI is
/// real, tmux is real — but nothing on the phone or in `codeconnect sessions`
/// will ever show it. Refusing here says that before a session exists, rather
/// than leaving somebody to discover it from an empty fleet.
///
/// **Only a decoded "no" refuses.** A daemon that is absent, or that we could not
/// establish anything about, is not an obstacle: a session launched while `ccd`
/// is down is a supported state, and it registers when the daemon comes back. The
/// safety property lives with the supervisor, which fails closed on doubt; this
/// only spends the operator's time well.
fn refuse_unless_hostable(support: crate::daemon::AgentSupport) -> Result<()> {
    match support {
        crate::daemon::AgentSupport::Hosted
        | crate::daemon::AgentSupport::Absent
        | crate::daemon::AgentSupport::Indeterminate(_) => Ok(()),
        crate::daemon::AgentSupport::Refused(why) => bail!(
            "refusing to launch: {why}. The session would run, but this daemon could \
             never be told about it — nothing would list it and the phone would not \
             see it. Update or restart ccd, then try again."
        ),
    }
}

// ------------------------------------------------------------------- the launch

/// How long the coordinator has to reach a terminal launch outcome.
///
/// 60s, which is what every live gate in this repo runs with, rather than the
/// coordinator's own 30s fallback — which no live launch has ever been measured on.
/// The work inside it is not small: the resolved codex is a 220 MB executable that
/// gets frozen and re-hashed before each of three `execve`s (~0.5 s apiece), and an
/// app-server has to come up and answer `initialize` before the TUI is spawned.
const LAUNCH_DEADLINE_MS: u64 = 60_000;

/// How long the launcher itself waits on the record, and how often it looks.
///
/// Strictly longer than [`LAUNCH_DEADLINE_MS`], because the deadline is the
/// coordinator's budget to *decide* and the record transition it writes when the
/// budget runs out is the thing worth waiting for: a launcher that gave up at the
/// same instant would report its own impatience instead of the coordinator's
/// recorded reason. The margin covers that write plus its fsync.
const LAUNCH_PATIENCE: std::time::Duration =
    std::time::Duration::from_millis(LAUNCH_DEADLINE_MS + 15_000);
/// See [`LAUNCH_PATIENCE`].
const RECORD_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Mint the session identity and spawn the detached launch coordinator.
/// Interactive launches attach once the coordinator has recorded and configured
/// its session, and the host starts the TUI once that attach has reported the
/// terminal's colors. The launcher keeps watching the durable outcome while it
/// shows the session.
fn launch(resolved: &ResolvedCodex, folder: &Path, passthrough: &[String]) -> Result<()> {
    // The folder is passed RAW, and that is not an oversight. The chain has exactly
    // one canonicalization, in the coordinator
    // (`codex_coordinator::canonical_launch_cwd`), and every later hop carries its
    // spelling verbatim. A second `canonicalize` here would be a second answer about
    // the same directory, taken at a different instant, with nothing requiring the
    // two to agree — the same failure the digest is carried rather than re-derived
    // to avoid.
    let cwd = folder.to_string_lossy().to_string();

    // The same namespace `codeconnect claude` draws from, on the same tmux server:
    // one `cc-N` sequence across both agents, so `ls` and `attach` see one fleet and
    // two sessions can never collide on a name. (The coordinator would default to a
    // fixed `cc-codex`, which is fine for a harness and wrong for a machine that
    // runs more than one.)
    let session_name = crate::tmux::next_session_name()?;
    // Minted here, once, before anything else knows the session exists — the same
    // rule and the same minter as the Claude path. The tmux name is reused as soon
    // as this session exits; this is not, and it is what the event log and the
    // launch record are keyed by.
    let session_uid = protocol::uid::new().context("minting a session uid")?;

    let terminal_size = crate::tmux::terminal_size();
    let charter = coordinator_charter(&CharterInputs {
        uid: &session_uid,
        launch_nonce: &crate::codex_launch::mint_nonce(),
        custodian_nonce: &crate::codex_launch::mint_nonce(),
        session_name: &session_name,
        cwd: &cwd,
        codex: resolved,
        codex_home: &codex_home(),
        tui_args: passthrough,
        terminal_size,
    });
    spawn_coordinator(&session_name, &session_uid, &charter)?;

    if terminal_size.is_some() {
        return wait_with_terminal(&session_uid, LAUNCH_PATIENCE, RECORD_POLL);
    }

    match crate::codex_coordinator::wait_on_record(&session_uid, LAUNCH_PATIENCE, RECORD_POLL) {
        // The record is `Ready` and proven durable. The coordinator has become the
        // session's supervisor, the pane is real, and the session is on the shared
        // tmux server under `session_name` — so this is the Claude path's own last
        // line, reached the same way and printing the same nothing.
        crate::codex_coordinator::LaunchWait::Ready => crate::tmux::attach(&session_name),
        // **The record's reason, verbatim.** It is already sanitized to one printable
        // bounded line by `wait_on_record`, and it is the only account of the failure
        // that survives the runtime dir being swept — so it is reported as the record
        // holds it rather than wrapped in a second story about it.
        // A TUI quit before any thread is the user leaving, as native codex's picker
        // lets them: nothing to report.
        crate::codex_coordinator::LaunchWait::Failed(_) if quit_before_thread(&session_uid) => {
            Ok(())
        }
        crate::codex_coordinator::LaunchWait::Failed(reason) => bail!("{reason}"),
        // Not a verdict. The launcher's patience ran out; the coordinator and the
        // custodian still own the outcome and will still drive the record to a
        // terminal state. Say exactly that, and say where the answer will be.
        crate::codex_coordinator::LaunchWait::TimedOut => bail!(
            "the codex launch did not reach a terminal state within {}s. It has not been \
             cancelled — the coordinator and its custodian still own it — but this command \
             has stopped waiting. The outcome is recorded at {}; `codeconnect ls` shows the \
             session if it came up.",
            LAUNCH_PATIENCE.as_secs(),
            crate::codex_launch::session_dir(&session_uid).display()
        ),
    }
}

/// Whether this launch ended because its TUI was quit cleanly before any thread
/// bound — Ctrl+C in codex's `resume` picker exits with status 0 and prints nothing
/// (measured on 0.155.1). See
/// [`crate::codex_launch::LaunchRecord::codex_quit_before_thread`].
fn quit_before_thread(uid: &str) -> bool {
    crate::codex_launch::load(uid).is_ok_and(|record| record.codex_quit_before_thread)
}

/// Own the attached client without moving it out of the terminal's foreground
/// process group. The detached coordinator and custodian have separate groups.
struct LaunchTerminal {
    client: crate::attach::Client,
}

impl LaunchTerminal {
    fn spawn(pin: &protocol::tmux::OwnedSession) -> Result<Self> {
        Ok(Self {
            client: crate::tmux::spawn_owned_attach(pin)?,
        })
    }

    fn stop(&mut self) -> Result<()> {
        self.client.stop()
    }
}

impl Drop for LaunchTerminal {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn wait_with_terminal(uid: &str, patience: Duration, poll: Duration) -> Result<()> {
    use crate::codex_coordinator::LaunchWait;
    use crate::codex_launch::LaunchState;

    let deadline = Instant::now() + patience;
    let mut terminal: Option<LaunchTerminal> = None;
    let mut pin = None;
    let mut verified = false;
    let mut ready = false;
    let outcome = (|| {
        loop {
            // Reuse the existing consumer-side fsync and sanitized failure path.
            // A zero patience is a single observation, not a launch timeout.
            if !ready {
                match crate::codex_coordinator::wait_on_record(uid, Duration::ZERO, poll) {
                    LaunchWait::Failed(_) if quit_before_thread(uid) => return Ok(()),
                    LaunchWait::Failed(reason) => bail!("{reason}"),
                    LaunchWait::Ready => ready = true,
                    LaunchWait::TimedOut => {}
                }
            }
            if terminal.is_none() {
                if let Ok(record) = crate::codex_launch::load(uid) {
                    if !matches!(record.state, LaunchState::Failed { .. })
                        && record.remain_on_exit_asserted
                        && crate::codex_launch::prove_record_durable(uid).is_ok()
                    {
                        if let Some(server) = record.server_a {
                            let expected = server.as_pin(protocol::TMUX_SOCKET_NAME, uid);
                            terminal = Some(LaunchTerminal::spawn(&expected)?);
                            pin = Some(expected);
                        }
                    }
                }
            }
            if let Some(client) = terminal.as_mut() {
                let exited = client
                    .client
                    .child
                    .try_wait()
                    .context("checking the terminal client")?;
                if exited.is_none() && !verified {
                    if let Some(expected) = pin.as_ref() {
                        verified = protocol::tmux::reverify_owned_client(
                            client.client.child.id() as i32,
                            expected,
                        )
                        .is_ok();
                    }
                }
                if ready && exited.is_some() {
                    return client
                        .client
                        .wait()
                        .context("the codex terminal client ended");
                }
            }
            if (!ready || !verified) && Instant::now() >= deadline {
                if ready {
                    bail!("the codex launch is ready, but its terminal attachment could not be verified; the session remains owned by its coordinator");
                }
                bail!(
                    "the codex launch did not reach a terminal state within {}s. It has not been \
                     cancelled — the coordinator and its custodian still own it — but this command \
                     has stopped waiting. The outcome is recorded at {}; `codeconnect ls` shows the \
                     session if it came up.",
                    patience.as_secs(),
                    crate::codex_launch::session_dir(uid).display()
                );
            }
            std::thread::sleep(poll);
        }
    })();
    // Restore the shell's terminal before reporting an error. Never signal the
    // session, coordinator, custodian, or a process group here.
    if let Some(client) = terminal.as_mut() {
        if let Err(cleanup) = client.stop() {
            return match outcome {
                Ok(()) => Err(cleanup),
                Err(error) => Err(error.context(format!("terminal cleanup failed: {cleanup:#}"))),
            };
        }
    }
    outcome
}

/// The isolated `CODEX_HOME` the app-server and the TUI both run under.
///
/// `CODEX_HOME` if the operator set one, else codex's own default of `~/.codex` —
/// which is the point: this is where the operator's `auth.json` lives, so a launch
/// that pointed anywhere else would open a TUI parked on "Sign in with ChatGPT".
/// The charter requires the value and defaults it nowhere, so it is named here
/// explicitly rather than inherited from this process's environment by accident.
fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| protocol::home_dir().join(".codex"))
}

/// Everything the launcher decides, gathered so [`coordinator_charter`] can stay a
/// pure function of it.
struct CharterInputs<'a> {
    uid: &'a str,
    launch_nonce: &'a str,
    custodian_nonce: &'a str,
    session_name: &'a str,
    cwd: &'a str,
    codex: &'a ResolvedCodex,
    codex_home: &'a Path,
    tui_args: &'a [String],
    terminal_size: Option<(u16, u16)>,
}

/// Build the `internal-codex-coordinator` charter argv.
///
/// Pure, and separated from the spawn precisely so the invariant below can be
/// asserted by a test rather than by this paragraph.
///
/// # The digest is CARRIED, never re-derived — and that is the whole point of the pin
///
/// This function takes a [`ResolvedCodex`], not a path, and emits
/// `--codex-sha256 {codex.sha256}`: the digest of the exact bytes
/// [`inspect_candidate`] read, from the same single read that produced the Mach-O
/// verdict, and that [`probe_codex`] then froze and re-verified across
/// `codex --version`. Re-hashing `codex.path` here instead would produce a digest of
/// whatever the name reaches *now* — which, in the one scenario the pin exists for
/// (an install or update landing mid-launch), is a truthful digest of the wrong
/// binary, and the host's two pre-exec verifications would then dutifully confirm
/// that the replacement is unchanged. The pin would still be a pin; it would just be
/// pinned to the attacker's file. The coordinator is a courier for the same reason
/// (`codex_coordinator::RealCoordinatorDeps::codex_sha256`), so the identity travels
/// unbroken from the one process that inspected the bytes to the two `execve`s that
/// run them.
fn coordinator_charter(inputs: &CharterInputs<'_>) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "--uid".into(),
        inputs.uid.into(),
        "--nonce".into(),
        inputs.launch_nonce.into(),
        "--custodian-nonce".into(),
        inputs.custodian_nonce.into(),
        "--session-name".into(),
        inputs.session_name.into(),
        "--cwd".into(),
        inputs.cwd.into(),
        "--deadline-ms".into(),
        LAUNCH_DEADLINE_MS.to_string(),
        "--codex".into(),
        inputs.codex.path.to_string_lossy().into_owned(),
        // The identity of the bytes, beside the name of the file. Carried from
        // resolution — see this function's doc.
        "--codex-sha256".into(),
        inputs.codex.sha256.clone(),
        "--codex-home".into(),
        inputs.codex_home.to_string_lossy().into_owned(),
    ];
    // `--tmux-socket` is deliberately absent: the coordinator's default is
    // `protocol::TMUX_SOCKET_NAME`, the one server `codeconnect claude`, `ls` and
    // `attach` all address. Naming it here would be a second copy of that constant
    // with nothing keeping the two equal.
    if let Some((cols, rows)) = inputs.terminal_size {
        argv.extend(["--terminal-size".into(), format!("{cols}x{rows}")]);
        argv.push("--wait-for-terminal".into());
    }
    if !inputs.tui_args.is_empty() {
        argv.push("--".into());
        argv.extend(inputs.tui_args.iter().cloned());
    }
    argv
}

/// Spawn the coordinator so it outlives this process *and* the terminal tab.
///
/// The same daemonisation shape as the Claude path's `spawn_supervisor`, for the
/// same two reasons and with one extra: `process_group(0)` keeps the SIGHUP/SIGINT
/// aimed at the tab's foreground group away from it, and the redirected stdio means
/// a log survives the tab. The extra is that this process is about to become the
/// terminal's tmux client and the coordinator will then *continue as the session's supervisor*
/// (`codex_coordinator::supervise_ready_session`) for the whole life of the run — a
/// coordinator sharing this process group would be killed by the first Ctrl-C after
/// the user detaches, and a `ready` record whose coordinator is gone is session-fatal.
///
/// Named by uid, like the supervisor's log and for the same reason: `cc-N` is reused
/// the moment a session exits, and two runs sharing one log file makes the file
/// useless exactly when it is needed.
fn spawn_coordinator(session_name: &str, session_uid: &str, charter: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let current = std::env::current_exe().context("locating the codeconnect binary")?;
    let log_path =
        protocol::logs_dir().join(format!("coordinator-{session_name}-{session_uid}.out"));
    std::fs::create_dir_all(protocol::logs_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    Command::new(current)
        .arg("internal-codex-coordinator")
        .args(charter)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning the codex coordinator for {session_name}"))?;
    Ok(())
}

// ----------------------------------------------------------- binary resolution

/// Find the real `codex`, never `codeconnect` itself.
///
/// launchd-safe and identical in shape to `resolve_claude_bin`: an explicit
/// candidate list first, `PATH` only as a fallback, and the same self-resolution
/// guard against resolving to this binary (which a shell alias like
/// `alias codex=codeconnect codex` would otherwise cause, spawn-looping).
///
/// The chosen path is canonicalised **once**, and a canonicalisation failure is
/// **fail-closed**: rather than fall back to the moving symlink (which could let
/// a later `standalone/current` flip change which executable runs), resolution
/// refuses. The returned path is the versioned executable behind any
/// `standalone/current` hop, and it is the single path everything downstream
/// uses.
///
/// **Executable identity.** Canonicalisation pins a pathname; it does not pin a
/// file. So each candidate is read exactly once ([`inspect_candidate`]) and that
/// single read yields both the Mach-O verdict and the SHA-256 that every later
/// exec site verifies against — see [`ResolvedCodex`] for why the name alone is not
/// enough and what the pin does and does not claim.
fn resolve_codex_bin(config: &Config) -> Result<ResolvedCodex> {
    let candidates = codex_candidates_for(config);

    let current_canonical = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok());

    // If nothing usable is found, the error names the FIRST thing that was found
    // and rejected, and says which of the two rejections it was — the supported
    // install is the standalone native binary, not a script/JS shim, and "we could
    // not read it" is a different fact from "it is a wrapper" and must not be
    // reported as one.
    let mut rejected: Option<(PathBuf, String)> = None;

    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        // Canonicalise once. `is_file` already followed the symlink to a real
        // file, so a failure here is a race or a permission fault — fail closed
        // rather than exec an executable we cannot pin an identity to.
        let canonical = candidate.canonicalize().with_context(|| {
            format!(
                "resolving the codex binary at {} to a versioned path",
                candidate.display()
            )
        })?;
        // Self-resolution guard on the canonical identity: an alias to this shim
        // is skipped so the real codex further down the list is found.
        if current_canonical.as_deref() == Some(canonical.as_path()) {
            continue;
        }
        // The resolved file must be the **actual native executable**. A generic
        // wrapper — the npm `codex.js` shebang shim at `/opt/homebrew/bin/codex`,
        // or any `#!`-script — selects and spawns a native binary at runtime, so
        // an npm replacement between our checks and the later app-server /
        // TUI spawns would swap the real CLI while our canonical path is
        // unchanged, defeating the identity pin. Skip
        // a wrapper so a native candidate later in the list still wins; only if
        // none is native do we refuse, naming the wrapper.
        //
        // The same read produces the digest, which is what makes the verdict and
        // the pin describe one file rather than two consecutive opens of one name.
        match inspect_candidate(&canonical) {
            CandidateIdentity::Native { sha256 } => {
                return Ok(ResolvedCodex {
                    path: canonical,
                    sha256,
                })
            }
            CandidateIdentity::Wrapper => {
                rejected.get_or_insert((
                    canonical,
                    "it is a wrapper, not a native executable".to_string(),
                ));
            }
            // Unusable is skipped rather than fatal, exactly as a wrapper is: a
            // candidate we could not pin an identity to, early in the list, must not
            // stop a perfectly good one later in it. It can never be *used*, because
            // this launch never runs bytes no digest is attributable to.
            CandidateIdentity::Unusable(why) => {
                rejected.get_or_insert((canonical, why));
            }
        }
    }
    match rejected {
        Some((path, why)) => bail!(
            "the codex at {} cannot be used: {why}; \
             CodeConnect supports the standalone native codex \
             (e.g. ~/.local/bin/codex → …/standalone/releases/…/bin/codex)",
            path.display()
        ),
        None => {
            bail!("could not find the codex binary; set codex_bin in ~/.codeconnect/config.json")
        }
    }
}

/// What one candidate turned out to be, decided from a **single** read of it.
enum CandidateIdentity {
    /// A native Mach-O executable, carrying the SHA-256 of the very bytes whose
    /// leading four produced that verdict.
    Native { sha256: String },
    /// Read end to end, but not a native Mach-O — a `#!`-script or `.js` shim.
    Wrapper,
    /// **No digest could be attributed to this pathname**, with the reason. Two
    /// different failures land here and they are one fact: the file could not be
    /// opened or read end to end, or it *was* read end to end but the pathname
    /// stopped naming it partway through (see [`inspect_candidate`]). In both cases
    /// there is nothing this launch could honestly pin — a digest of bytes we cannot
    /// reach by the name we would `execve` is not an identity — and this launch
    /// never runs what cannot be pinned. Not `Unreadable`: the second case reads
    /// perfectly, which is exactly what makes it dangerous.
    Unusable(String),
}

/// Read the file at `path` **once**, and derive from that one read both whether it
/// is a native Mach-O executable (thin or universal) and the SHA-256 of its bytes.
///
/// The single read is the whole point, and it is why the magic number comes back
/// out of the hashing pass rather than from a `read_exact` before it. Two opens of
/// one pathname can see two files; even two reads of one *handle* can straddle an
/// in-place rewrite. With one pass, "this is a native binary" and "this is its
/// digest" are statements about the same bytes by construction — so the digest
/// every exec site later verifies is provably the digest of the thing that passed
/// the wrapper check.
///
/// The price is that a candidate which turns out to be a wrapper has still been
/// read in full, because the verdict is only available once the pass that produced
/// it has finished. That is the right trade: wrappers on this candidate list are
/// shebang scripts of a few hundred bytes, and the alternative — peek, then hash —
/// is the two-read hole this exists to close. The cost that matters is the native
/// case, one whole-file read (measured: ~0.5 s for the 220 MB standalone codex in a
/// release build, ~8 s unoptimised).
///
/// # One read is not enough on its own: the name has to still be the file
///
/// That whole-file read is exactly the window a rename fits in, and this site has
/// the same hole every verification site had. The digest and the verdict come out of
/// a handle the kernel pinned at `open`; the thing they get attributed to is a
/// *pathname* that the rest of the launch carries around and eventually `execve`s.
/// An installer landing an atomic replacement half a second into the read leaves the
/// read undisturbed — and would mint a `ResolvedCodex` whose digest is a perfectly
/// truthful statement about a file that this pathname no longer reaches, which every
/// later verify would then dutifully confirm was "unchanged" only because the
/// replacement had settled before any of them looked.
///
/// So the handle is still open when
/// [`protocol::hash::refuse_unless_path_still_names`] compares it against the name —
/// the same single rule the verification sites apply through
/// [`protocol::hash::sha256_file`], written once and used at both kinds of site so
/// resolution and verification cannot come to disagree about what identity means.
fn inspect_candidate(path: &Path) -> CandidateIdentity {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => return CandidateIdentity::Unusable(format!("it could not be opened ({err})")),
    };
    let mut magic = [0u8; 4];
    let (sha256, magic_len) = match protocol::hash::sha256_stream_head(&mut file, &mut magic) {
        Ok(read) => read,
        Err(err) => {
            return CandidateIdentity::Unusable(format!("it could not be read in full ({err})"))
        }
    };
    // Before anything is concluded from those bytes: is this pathname still the file
    // they came from? `file` is deliberately still open — that is what makes the
    // comparison an identity rather than a coincidence of inode numbers.
    if let Err(err) = protocol::hash::refuse_unless_path_still_names(path, &file) {
        return CandidateIdentity::Unusable(format!("{err}"));
    }
    // A file shorter than the magic is not native, and the count is what says so.
    // `sha256_stream_head` does not zero the tail of `head` — it leaves whatever was
    // there — so judging the magic without checking `magic_len` first would be
    // reading this stack buffer's own initialiser and calling it file content.
    if magic_len < magic.len() || !is_native_magic(magic) {
        return CandidateIdentity::Wrapper;
    }
    CandidateIdentity::Native { sha256 }
}

/// Whether four leading bytes are a Mach-O / universal-binary magic number.
///
/// Pure, over bytes rather than a path, so the magic table is testable on its own
/// and cannot drift from the single read that produces those bytes.
///
/// # Accepted residual: a compiled dispatcher is pinned, and what it dispatches to is not
///
/// A magic number says "a native executable"; it does not say "standalone Codex". A
/// *compiled native dispatcher* — a small Mach-O binary that picks a real codex at
/// runtime and spawns it — passes this check, and states a version too if it
/// forwards `--version`.
///
/// Stated exactly, because a residual that is not exact is not a residual, it is a
/// hope. A launch execs the resolved `--codex` three times:
///
///   1. `codex --version`, in the launcher ([`probe_codex`]);
///   2. `codex app-server --listen unix://…`, in the host;
///   3. the interactive TUI, `codex --remote …`, in the host.
///
/// **Pinned:** the bytes of the file at the resolved path. All three execs open that
/// one canonical pathname, and each is bracketed by [`verify_codex_identity`] — with
/// the vnode arm, so the digest is attributable to the name and not merely to a vnode
/// (see [`ResolvedCodex`]). If `--codex` is a dispatcher, that is the *dispatcher*
/// that is pinned, faithfully and completely: the same dispatcher bytes run all three
/// times and a mid-launch swap of it is refused.
///
/// **Not pinned:** everything on the other side of it. Whatever binary the dispatcher
/// selects and spawns for `--version`, for `app-server` and for the TUI — three more
/// execs CodeConnect never sees — is not inspected, not magic-checked and not
/// hashed, and nothing requires the three to be the same
/// binary as each other. So a dispatcher can answer `--version` from one build
/// and then run something else entirely under the app-server and the TUI, which are
/// the two execs the whole command gate exists to contain: the app-server is what
/// executes the model's tool calls and the TUI is what the operator types into.
///
/// The identity chain is therefore closed up to the file we exec and **open past any
/// process that re-dispatches**. Closing it would need its own enforcement (for
/// example, verifying the standalone package layout). This is not the hash-pin's
/// residual and it is not narrowed by it; it is a separate hole, and the only
/// reason it is not gaping today is that the dispatcher shape anyone actually
/// ships — the npm `codex.js` shebang shim — is caught here as a
/// [`CandidateIdentity::Wrapper`], while a *compiled* one is caught nowhere.
///
/// **This is an accepted residual rather than a blocker** — [`start`] records it as
/// one of the two residuals that are separate from the accepted boundary. It stays
/// open because no layout verifier can be invented here: there is no specification
/// of a supported package layout, and picking one unilaterally would silently narrow
/// which installs CodeConnect supports — a scoping decision, not an implementation
/// detail. Closing it needs that ruling first.
fn is_native_magic(magic: [u8; 4]) -> bool {
    matches!(
        u32::from_be_bytes(magic),
        // Mach-O 32/64-bit, big- and little-endian (arm64 native is 0xCFFAEDFE).
        0xFEED_FACE | 0xFEED_FACF | 0xCEFA_EDFE | 0xCFFA_EDFE
        // Universal ("fat") binaries, 32- and 64-bit.
        | 0xCAFE_BABE | 0xBEBA_FECA | 0xCAFE_BABF | 0xBFBA_FECA
    )
}

// -------------------------------------------------------------- executable identity

/// The wire width of a pinned digest: SHA-256 as lowercase hex.
const CODEX_SHA256_HEX_LEN: usize = 64;

/// Parse the `--codex-sha256` wire form: exactly 64 **lowercase** hex characters.
///
/// One grammar, in the module that owns the concept, used by everything that reads
/// the digest off an argv — the coordinator's charter and the host's charter alike
/// (the same reason both already share [`validate_codex_argv`]). A coordinator that
/// accepted a spelling the host rejected would be a disagreement about an identity
/// check discovered at the pane.
///
/// Uppercase is refused rather than folded. [`protocol::hash::sha256_hex`] emits
/// one spelling, and a digest with two valid spellings is a digest whose equality
/// test can answer "different" about identical bytes — the failure mode this whole
/// mechanism exists to avoid, arriving through the front door.
pub(crate) fn parse_codex_sha256(raw: &str) -> Result<String> {
    let ok = raw.len() == CODEX_SHA256_HEX_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !ok {
        bail!(
            "a codex digest must be exactly {CODEX_SHA256_HEX_LEN} lowercase hex characters \
             (a sha256), got {raw:?}"
        );
    }
    Ok(raw.to_string())
}

/// Refuse a `--codex` that is not an **absolute** path.
///
/// **Two different resolvers read that one string.** The executable-identity guard
/// opens it ([`verify_codex_identity`] → `File::open`) and the spawns execute it
/// (`Command::new`), and those two disagree on exactly one class of input: a value
/// containing no `/`. `File::open("codex")` opens `./codex`; `Command::new("codex")`
/// searches `PATH`. A charter naming a bare `codex` would hash one file and execute
/// another, and the verify would pass — honestly, and about the wrong bytes. That is
/// the whole gate defeated by a spelling, so the spelling is refused.
///
/// A merely *relative* path (`./codex`) does not diverge that way, but it makes both
/// answers depend on the process's cwd, which is a second route to one string meaning
/// two files. Requiring absolute closes both and costs nothing real: the launcher
/// resolves to a canonical path, which is always absolute.
///
/// **It lives here, in the module that owns the identity policy, for the same reason
/// [`parse_codex_sha256`] does.** Two processes parse a charter carrying `--codex` —
/// the coordinator, which writes the host's charter and opens a pane, and the host,
/// which reads it back — and the rule has to be the same rule in both. It was not:
/// the host refused a relative path while the coordinator accepted one, so a bad
/// spelling got a session directory, a tmux pane and a launched host before the
/// fail-closed error arrived, and the error arrived where nobody is looking. Both
/// parsers now call this. Both, not one: the coordinator's call moves the refusal to
/// the process a human is watching, and the host keeps its own because a host must
/// never assume its parent checked anything.
pub(crate) fn require_absolute_codex(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!(
            "--codex must be an absolute path, got {}; a relative or bare name is \
             resolved one way by the identity check (which opens it) and another by \
             the spawn (which searches PATH), so the bytes verified need not be the \
             bytes executed",
            path.display()
        );
    }
    Ok(())
}

/// Re-read the file at `path` and refuse unless it still hashes to `expected`.
///
/// **This is the executable-identity guard.** It stands immediately before each
/// point where these bytes are about to become a running process, so that what
/// runs is what was inspected and hashed rather than merely whatever was reachable
/// through the same name. Three sites — [`probe_codex`]'s `--version` exec and the
/// host's two spawns (`codex_host::run_session` and `codex_host::drive`) — all now
/// the same flavour: **prevention with the freeze held across the exec.** A mismatch
/// means nothing runs.
///
/// **It returns a held freeze, and that is the point.** The digest is taken through
/// a handle whose bytes are pinned immutable *before* the read and kept immutable in
/// the returned [`protocol::hash::FrozenExecutable`]; the caller keeps that guard
/// across its `execve` and drops it once the child is past exec. Against the vector
/// this gate exists for — an install or update landing mid-launch — that makes the
/// bytes verified here and the bytes the kernel loads the same frozen vnode, closing
/// both holes a bare `(dev, ino)` comparison cannot: a same-inode content overwrite
/// behind the reader, and a rename over the name after the check. It is deliberately
/// **not** claimed against a hostile same-uid process, which can revoke the flag and
/// is out of scope by construction. See [`protocol::hash::FrozenExecutable`] for the
/// measurements behind all of that, the fallback when the freeze cannot be set, and
/// the demand-paging residual after it is cleared.
///
/// `when` names the moment, so a refusal tells an operator *where* in the launch the
/// file moved rather than only that it did.
///
/// A failure to re-read is a refusal, not a pass: "I could not check" and "it is
/// unchanged" are different answers, and only one of them licenses an `execve`.
///
/// **The caller owes one thing: a path that resolves the same way here as at the
/// exec.** This opens `path` directly; `Command::new` PATH-searches a value with no
/// `/` in it. Handing this a bare name would produce a truthful verification of a
/// file that is not the one that runs, which is the whole gate lost to a spelling.
/// The host enforces it (`codex_host::require_absolute_codex`) and resolution
/// produces only canonical absolute paths.
/// **`on_frozen` is where the freeze gets written down, and it runs while the hash
/// is still being taken.** The digest is a whole-file read of a 210 MB executable —
/// measured at 0.46 s in a release build and about eight seconds unoptimised — and
/// the flag is on for all of it, so a caller that recorded the freeze from the
/// return value left that whole interval with the bytes immutable and nothing
/// durable saying so. A `SIGKILL` there, which is exactly the load-induced ending
/// that produced the two real leaks, left a frozen binary with no claim for the
/// custodian to act on. The callback runs with the flag already set and the digest
/// not yet started, so every freeze site has to say what it records rather than
/// being able to forget.
///
/// It runs even on the paths that go on to refuse: a freeze that is about to be
/// dropped for a hash mismatch is still a freeze this process is holding, and a
/// record written and withdrawn a moment later costs one file write. Being wrong in
/// that direction is a stale claim the janitor's own checks discard; being wrong in
/// the other is the leak.
pub(crate) fn verify_codex_identity<F>(
    path: &Path,
    expected: &str,
    when: &str,
    hold: protocol::hash::LockHold,
    on_frozen: F,
) -> Result<protocol::hash::FrozenExecutable>
where
    F: FnOnce(&protocol::hash::FrozenExecutable) -> std::result::Result<(), String>,
{
    let (actual, frozen) = protocol::hash::freeze_and_hash_recording(path, hold, on_frozen)
        .with_context(|| {
            format!(
                "re-reading the codex binary at {} to verify its identity {when}",
                path.display()
            )
        })?;
    if actual != expected {
        // `frozen` drops here, clearing the freeze: nothing was spawned, and the
        // file this launch will not touch is left exactly as it was found.
        bail!(
            "the codex binary at {} is not the one this launch pinned: it hashed {expected} \
             when it was resolved and inspected, and hashes {actual} {when}. Refusing to run \
             it — the bytes that were checked are not the bytes that would execute. \
             (A codex install or update running alongside a launch produces exactly this; \
             let it finish, then launch again.)",
            path.display()
        );
    }
    // The freeze is HELD in the returned guard: the caller keeps it across its
    // `execve` and drops it once the child is past exec, so the bytes hashed here are
    // the bytes that run. See [`protocol::hash::FrozenExecutable`].
    Ok(frozen)
}

/// The ordered candidate list, factored out so the precedence is unit-tested
/// without touching the process environment: config `codex_bin`, then the
/// `CODECONNECT_CODEX_BIN` env override, then the well-known install locations,
/// then a `PATH` hit last.
fn codex_candidates(
    config: &Config,
    env_override: Option<PathBuf>,
    home: &Path,
    path_hit: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(configured) = &config.codex_bin {
        candidates.push(PathBuf::from(configured));
    }
    if let Some(env) = env_override {
        candidates.push(env);
    }
    // The standalone installer's stable entry point (`~/.local/bin/codex` → a
    // `standalone/current` symlink → the versioned release), then the two
    // generic bin dirs a package manager would link a `codex` into.
    candidates.push(home.join(".local/bin/codex"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    if let Some(found) = path_hit {
        candidates.push(found);
    }
    candidates
}

// ------------------------------------------------------------------- the version

/// Pull the version out of `codex --version` output.
///
/// Grounded on the installed shape `codex-cli 0.147.0`. The output must be
/// **exactly one** non-empty line, in one of the two measured forms —
/// `codex-cli <version>` or a bare `<version>` — and anything with an extra line,
/// or extra/ambiguous tokens on the line, is rejected rather than guessed. So
/// neither `codex-cli 0.148.0 compatibility 0.147.0` (extra tokens) nor
/// `codex-cli 0.147.0\ncompatibility 0.148.0` (extra line) is taken for a version
/// codex did not state. Pure, so the shape is pinned by tests rather than by the live
/// binary.
fn parse_codex_version(text: &str) -> Option<String> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        // More than one non-empty line: not the exact expected output.
        return None;
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let version = match tokens.as_slice() {
        [version] => *version,
        ["codex-cli", version] => *version,
        _ => return None,
    };
    // A version starts with a digit; reject a stray word in the version slot.
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

// ------------------------------------------------------------- the launch probe

/// The wall-clock budget for one launch probe.
///
/// Generous against the measurement — a probe takes well under a second on the real
/// binary — because the number is not a performance target, it is the point past which
/// the launch stops waiting for an answer it is never going to get.
const PROBE_BUDGET: Duration = Duration::from_secs(30);

/// How long to spend reaping a probe after its process group has been SIGKILLed.
const PROBE_REAP_BUDGET: Duration = Duration::from_secs(2);

/// The stdout ceiling for one probe. A version line is a few bytes; the ceiling is what
/// keeps a binary that streams from holding the launch's memory.
const PROBE_STDOUT_LIMIT: u64 = 8 << 20;

/// What is known about a probe's leader process — deliberately three-valued, because a
/// `try_wait` error proves only that *that call* collected no status. Filing it as
/// "unreaped" would license a `kill(-pgid)` on the strength of a failed syscall, against
/// a process-group number that may already have been recycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaderState {
    Unreaped,
    Reaped,
    Unknown,
}

/// One launch probe's child, with the cleanup that every exit path owes it.
struct Probe {
    child: std::process::Child,
    leader: LeaderState,
}

impl Probe {
    /// SIGKILL the probe's whole process group — best-effort, errors ignored.
    ///
    /// Guarded on [`LeaderState::Unreaped`], which is provable rather than hopeful: an
    /// unreaped leader is still in the process table, so its pid — and therefore this
    /// pgid — cannot have been handed to anything else.
    fn kill_group(&mut self) {
        if self.leader == LeaderState::Unreaped {
            unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            let _ = self.child.kill();
        }
    }

    /// Poll for the leader's status until `deadline`. Deliberately not `wait()`, which is
    /// unbounded: a probe that ignores SIGKILL (uninterruptible in a syscall) must cost
    /// the budget, not the launch.
    fn reap_bounded(&mut self, deadline: Instant) -> Option<std::process::ExitStatus> {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.leader = LeaderState::Reaped;
                    return Some(status);
                }
                Ok(None) => {}
                Err(_) => {
                    self.leader = LeaderState::Unknown;
                    return None;
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The one cleanup shape, used on every path including success: kill the group, then
    /// reap under a bound. Never the other order — see [`Probe::kill_group`].
    fn cleanup(&mut self) -> Option<std::process::ExitStatus> {
        self.kill_group();
        self.reap_bounded(Instant::now() + PROBE_REAP_BUDGET)
    }
}

impl Drop for Probe {
    /// The net beneath the explicit cleanup, so "every path kills the group" is a
    /// structural property rather than a promise about the code as currently written.
    /// `std::process::Child` has no kill-on-drop, so without this a probe's group would
    /// simply survive any early return added later.
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Drain one pipe on its own thread, under a byte ceiling, reporting overflow as a
/// FAILURE rather than as the end of output.
///
/// The read is moved off the launch's thread because a descendant that inherited the write
/// end can defer EOF forever; the caller bounds the *wait* with `recv_timeout`.
///
/// `LIMIT + 1` is asked for so that hitting the ceiling is detectable. `Read::take(N)`
/// reports EOF once N bytes are consumed, so a plain `take(LIMIT)` hands back a prefix
/// indistinguishable from a complete answer — and a prefix of a flood is exactly the
/// vacuous pass a probe must not take. A read error is reported for the same reason: a
/// truncated answer must never become a shorter one.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    limit: u64,
) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match pipe {
            Some(pipe) => {
                let mut buf = Vec::new();
                let mut bounded = std::io::Read::take(pipe, limit + 1);
                match std::io::Read::read_to_end(&mut bounded, &mut buf) {
                    Ok(_) if buf.len() as u64 > limit => {
                        Err(format!("it wrote more than {limit} bytes"))
                    }
                    Ok(_) => Ok(buf),
                    Err(e) => Err(format!("the read failed part-way ({e})")),
                }
            }
            None => Ok(Vec::new()),
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// Exec `bin args` under a wall-clock budget and an output ceiling, with the caller
/// holding the freeze, and return its stdout.
///
/// Split out so the caller can hold a verified freeze across it — see [`probe_codex`].
///
/// **Bounded on purpose.** The binary being probed is whatever is installed at the codex
/// path, and nothing has run it yet, so the probe cannot assume it behaves. A plain
/// `output()` gives an unknown executable an unbounded hold on the launch *and* on the
/// freeze — it can never exit, never close its pipes (a forked descendant inherits the
/// write ends, so EOF never arrives), or stream until the launcher runs out of memory.
/// Every wait here is against a deadline and every path attempts to kill the probe's
/// whole process group, pipes collected FIRST so the kill always happens while the
/// leader's pgid is provably not recycled.
fn run_under_freeze(bin: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run_bounded(bin, args, PROBE_BUDGET)
}

/// [`run_under_freeze`]'s body, with the budget a parameter so the boundedness itself can
/// be tested without the test paying the production budget to observe it.
fn run_bounded(bin: &Path, args: &[&str], budget: Duration) -> Result<Vec<u8>> {
    use std::os::unix::process::CommandExt;
    let what = format!("{} {}", bin.display(), args.join(" "));
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so a descendant that inherits the pipes can be reached.
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning `{what}`"))?;
    let mut probe = Probe {
        child,
        leader: LeaderState::Unreaped,
    };

    // Started before any wait, so a chatty probe cannot deadlock by filling a pipe buffer
    // while nobody is draining it.
    let stdout_rx = spawn_pipe_reader(probe.child.stdout.take(), PROBE_STDOUT_LIMIT);
    let stderr_rx = spawn_pipe_reader(probe.child.stderr.take(), PROBE_STDOUT_LIMIT);
    let deadline = Instant::now() + budget;

    let collect = |rx: &std::sync::mpsc::Receiver<Result<Vec<u8>, String>>, which: &str| match rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(why)) => Err(anyhow!("`{what}` on {which}: {why}")),
        Err(_) => Err(anyhow!(
            "`{what}` did not close its {which} within {budget:?}"
        )),
    };
    let stdout = collect(&stdout_rx, "stdout");
    let stderr = collect(&stderr_rx, "stderr");

    // The uniform cleanup, reached on the success path too, so nothing below has to
    // remember it.
    let status = probe.cleanup();
    let (stdout, stderr) = (stdout?, stderr?);
    let Some(status) = status else {
        bail!("`{what}` could not be reaped within {PROBE_REAP_BUDGET:?} of a SIGKILL");
    };
    if !status.success() {
        bail!(
            "`{what}` exited with {status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(stdout)
}

/// Ask the installed codex its version, under a held freeze, and refuse a binary that
/// does not state one. The value itself decides nothing.
///
/// The answer is attributable to the pinned bytes only because the freeze is held across
/// the exec: [`verify_codex_identity`] refuses a binary that is not the one resolution
/// inspected, and the flag keeps it from being replaced while it runs.
fn probe_codex(resolved: &ResolvedCodex) -> Result<()> {
    let bin = resolved.path.as_path();
    // **The launcher's freeze has no record behind it, so the signal handler is the
    // record.** This runs before a uid is minted: there is no launch record for a
    // janitor to read, and the only thing that would put the flag back is the
    // guard's own `Drop`. `SIGINT` runs no `Drop` — and with `panic = "abort"`
    // neither would an unwind — so `Ctrl-C` here left `UF_IMMUTABLE` on the real
    // codex binary every single time, invisibly, until the next update failed with
    // `Operation not permitted`. Installed before the freeze is taken and armed from
    // inside the verify, so the whole interval is covered: the flag goes on, the
    // handler already knows how to take it off, and the hash — most of a second in a
    // release build — happens under that cover rather than beside it.
    protocol::hash::install_freeze_signal_release();
    let frozen = verify_codex_identity(
        bin,
        &resolved.sha256,
        "before `codex --version`",
        // **The lock is held for the whole probe, and it is standing in for the record
        // this site cannot write.** There is no uid yet, so nothing a custodian scans
        // will ever name this freeze — and a custodian's scan-and-clear takes this same
        // lock, so while it is held the clear cannot happen at all. Released at the
        // empty record, the interval that follows (the 210 MB hash plus each exec
        // below) was a freeze every custodian on the machine was free to undo, and a
        // peer's stale record was enough to make one do it.
        protocol::hash::LockHold::UntilReleased,
        // Nothing durable to write: there is no uid yet, which is the whole reason
        // this site needs the handler. The freeze arms the release itself, from
        // inside `arm_then_freeze` and BEFORE the flag goes on, so the interval this
        // callback used to be responsible for no longer exists.
        |_| Ok(()),
    )?;

    let version_out = run_under_freeze(bin, &["--version"]);

    // Cleared only after the child has exited, so the answer is attributable to the
    // frozen bytes. A failure clears it too, with nothing having been admitted. The
    // output is interpreted only after that: the identity check has already run, so a
    // swapped binary is reported as swapped and never as an unreadable version.
    drop(frozen);
    let text = String::from_utf8_lossy(&version_out?).into_owned();
    parse_codex_version(&text)
        .map(|_| ())
        .ok_or_else(|| anyhow!("could not read a version from `codex --version`: {text:?}"))
}

/// A **test-only stand-in for the launcher's probe freeze**
/// (`internal-freeze-probe <path> <marker>`).
///
/// It does exactly what [`probe_codex`] does to the flag and nothing else: install
/// the signal release, freeze `<path>` (which arms the release itself, before the
/// flag goes on), touch `<marker>` so the test knows the freeze is on, and then
/// block. The test sends a
/// `SIGINT` — the ending that runs no `Drop`, and the one a `Ctrl-C` during a real
/// launch delivers — and reads the file's flags afterwards.
///
/// A real launcher run cannot stand in for this: it needs a codex whose answers get
/// past the probe, and the flag would then be cleared by the ordinary path rather than
/// by the handler. This is the smallest process that has the
/// property under test. Hidden machinery, never a human command.
pub fn run_freeze_probe(args: &[String]) -> ! {
    let (Some(path), Some(marker)) = (args.first(), args.get(1)) else {
        eprintln!("internal-freeze-probe needs <path> <marker>");
        std::process::exit(64);
    };
    protocol::hash::install_freeze_signal_release();
    let frozen = match protocol::hash::freeze_and_hash_recording(
        Path::new(path),
        protocol::hash::LockHold::UntilReleased,
        |_| Ok(()),
    ) {
        Ok((_digest, frozen)) => frozen,
        Err(err) => {
            eprintln!("internal-freeze-probe could not freeze {path}: {err}");
            std::process::exit(65);
        }
    };
    if !frozen.is_frozen() {
        eprintln!("internal-freeze-probe could not set the flag on {path}");
        std::process::exit(66);
    }
    // Only now: the marker means "the flag is on and armed", which is the state the
    // test is about to interrupt.
    let _ = std::fs::File::create(marker);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A **test-only stand-in for a freezer holding the freeze lock**
/// (`internal-freeze-lock-hold <path> <marker> <seconds>`).
///
/// A freezer holds `flock(LOCK_EX)` on the executable across its freeze and its
/// record write; a custodian holds the same lock across its scan and its clear. The
/// whole safety of the second depends on the first actually excluding it, and the two
/// are always different PROCESSES — so an in-process test of the lock (which
/// `protocol::hash` has) cannot say the thing that matters. This is the other half:
/// a real second process that takes the lock, says so, and holds it.
///
/// It carries its own deadline rather than blocking forever: a hidden helper that
/// outlived its test would be a lock nobody could see holding up every later launch on
/// the machine. Hidden machinery, never a human command.
pub fn run_freeze_lock_hold(args: &[String]) -> ! {
    let (Some(path), Some(marker), Some(seconds)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("internal-freeze-lock-hold needs <path> <marker> <seconds>");
        std::process::exit(64);
    };
    let held = match protocol::hash::FreezeLock::acquire(Path::new(path)) {
        Ok(held) => held,
        Err(why) => {
            eprintln!("internal-freeze-lock-hold could not lock {path}: {why}");
            std::process::exit(65);
        }
    };
    // Only now: the marker means "the lock is held", which is the state the test is
    // about to try to take it away from.
    let _ = std::fs::File::create(marker);
    let secs: u64 = seconds.parse().unwrap_or(5);
    std::thread::sleep(std::time::Duration::from_secs(secs));
    drop(held);
    std::process::exit(0)
}

// --------------------------------------------------------- reserved argv grammar

/// Why a `codex` argv was refused. Each variant renders a message that names what
/// was refused and why, so the refusal is legible at the terminal and pinned by
/// tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexRefusal {
    /// A flag that would make the TUI something other than the broker's client:
    /// `--remote` and `--remote-auth-token-env`. Without the broker there is no
    /// session for the phone to reach.
    OwnedFlag { flag: String, owner: &'static str },
    /// A subcommand name or alias. Only the interactive TUI is hosted — a new session,
    /// or `resume`/`fork` as the first positional ([`HOSTED_SUBCOMMANDS`]); `exec` and
    /// the rest are refused wherever codex would dispatch one.
    Subcommand { name: String },
}

impl std::fmt::Display for CodexRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexRefusal::OwnedFlag { flag, owner } => write!(
                f,
                "`{flag}` is set by CodeConnect ({owner}) and cannot be passed to `codeconnect codex`"
            ),
            CodexRefusal::Subcommand { name } => write!(
                f,
                "`codex {name}` is a subcommand; `codeconnect codex` hosts only the interactive \
                 session (a new one, `resume` or `fork`), so other subcommands and their \
                 aliases are refused"
            ),
        }
    }
}

/// A recognised flag's argument arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    /// Takes no value (`--search`).
    Bool,
    /// Takes exactly one value, spaced (`--model x`), `=`-joined (`--model=x`) or
    /// attached-short (`-mx`).
    Value,
    /// Greedy: one or more values consumed until the next flag or `--`
    /// (`-i a b`). Only `-i`/`--image` in 0.147.
    Values,
}

/// A recognised codex flag, canonicalised to its long name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KnownFlag {
    /// Canonical long spelling, e.g. `--ask-for-approval`.
    canonical: &'static str,
    arity: Arity,
}

/// Look up a long flag name (without a `=value` tail) in the known table.
///
/// The interactive/global flags of codex-cli 0.147.0 whose arity this walk needs to
/// know, plus the approval aliases `--yolo` and `--not-so-yolo`. A flag missing from
/// the table is not refused: it is forwarded unchanged and never takes the next word
/// as its value (see [`scan_codex_argv`]).
fn known_long(name: &str) -> Option<KnownFlag> {
    let flag = |canonical, arity| Some(KnownFlag { canonical, arity });
    match name {
        // Value flags.
        "--config" => flag("--config", Arity::Value),
        "--enable" => flag("--enable", Arity::Value),
        "--disable" => flag("--disable", Arity::Value),
        "--remote" => flag("--remote", Arity::Value),
        "--remote-auth-token-env" => flag("--remote-auth-token-env", Arity::Value),
        "--image" => flag("--image", Arity::Values),
        "--model" => flag("--model", Arity::Value),
        "--local-provider" => flag("--local-provider", Arity::Value),
        "--profile" => flag("--profile", Arity::Value),
        "--sandbox" => flag("--sandbox", Arity::Value),
        "--cd" => flag("--cd", Arity::Value),
        "--add-dir" => flag("--add-dir", Arity::Value),
        "--ask-for-approval" => flag("--ask-for-approval", Arity::Value),
        // Bool flags.
        "--strict-config" => flag("--strict-config", Arity::Bool),
        "--oss" => flag("--oss", Arity::Bool),
        "--approve-for-me" => flag("--approve-for-me", Arity::Bool),
        "--not-so-yolo" => flag("--not-so-yolo", Arity::Bool),
        "--dangerously-bypass-approvals-and-sandbox" => {
            flag("--dangerously-bypass-approvals-and-sandbox", Arity::Bool)
        }
        "--yolo" => flag("--yolo", Arity::Bool),
        "--dangerously-bypass-hook-trust" => flag("--dangerously-bypass-hook-trust", Arity::Bool),
        "--search" => flag("--search", Arity::Bool),
        "--no-alt-screen" => flag("--no-alt-screen", Arity::Bool),
        "--help" => flag("--help", Arity::Bool),
        "--version" => flag("--version", Arity::Bool),
        _ => None,
    }
}

/// Map a short flag letter to its canonical long flag.
fn known_short(letter: char) -> Option<KnownFlag> {
    match letter {
        'c' => known_long("--config"),
        'i' => known_long("--image"),
        'm' => known_long("--model"),
        'p' => known_long("--profile"),
        's' => known_long("--sandbox"),
        'C' => known_long("--cd"),
        'a' => known_long("--ask-for-approval"),
        'h' => known_long("--help"),
        'V' => known_long("--version"),
        _ => None,
    }
}

/// A classified token: a recognised flag (with any value attached to the token
/// itself), a cluster of only recognised bool short-flags, a flag-shaped token not in
/// the known table, a positional, or the `--` boundary.
enum Token {
    Flag {
        flag: KnownFlag,
        attached: Option<String>,
    },
    /// A short cluster of only bool flags (`-hV`), forwarded as-is.
    BoolCluster,
    /// A flag-shaped token not in the known table — an unknown long flag, an unknown
    /// short flag, or a short cluster with an unknown character. Forwarded unchanged,
    /// and it takes no following token as its value.
    Unknown,
    Positional,
    Boundary,
}

/// Classify a single argv token in isolation. Attached values (`--model=x`,
/// `-mx`, `-C.`) are split out here; a spaced value is the following token and is
/// pulled by the caller. Short clusters are **fully expanded** so a bool short in
/// front of a value short (`-hmgpt-5`) keeps the value with its flag. A flag-shaped
/// token not in the known table is `Unknown`.
fn classify(token: &str) -> Token {
    if token == "--" {
        return Token::Boundary;
    }
    if let Some(long) = token.strip_prefix("--") {
        let (name, attached) = match long.split_once('=') {
            Some((name, value)) => (format!("--{name}"), Some(value.to_string())),
            None => (format!("--{long}"), None),
        };
        return match known_long(&name) {
            Some(flag) => Token::Flag { flag, attached },
            None => Token::Unknown,
        };
    }
    // A single leading `-` and at least one more char: a short flag or cluster.
    // A bare `-` (a common stdin sentinel) is a positional, not a flag.
    if let Some(shorts) = token.strip_prefix('-') {
        if !shorts.is_empty() {
            return classify_short_cluster(shorts);
        }
    }
    Token::Positional
}

/// Fully expand a short-flag cluster (`shorts` is the token without its leading
/// `-`). Leading **bool** shorts (`-h`/`-V`) are stepped over; the first **value**
/// short terminates the cluster and takes the remainder as its attached value
/// (`-hcKEY=V` ⇒ `--config KEY=V`). A cluster of only bool shorts is a
/// `BoolCluster`; an unknown character anywhere (head or after a known short) makes
/// the whole cluster `Unknown`.
fn classify_short_cluster(shorts: &str) -> Token {
    for (offset, letter) in shorts.char_indices() {
        match known_short(letter) {
            Some(flag) if flag.arity == Arity::Bool => continue,
            Some(flag) => {
                let rest = &shorts[offset + letter.len_utf8()..];
                let attached = if rest.is_empty() {
                    None
                } else {
                    // `-c=key=val` and `-ckey=val` are both accepted; strip a
                    // single joining `=` if present.
                    Some(rest.strip_prefix('=').unwrap_or(rest).to_string())
                };
                return Token::Flag { flag, attached };
            }
            None => return Token::Unknown,
        }
    }
    // Every character was a recognised bool short.
    Token::BoolCluster
}

/// Validate a `codex` argv against the reserved grammar.
///
/// `Ok(())` means every token is a codex flag, a prompt, or content past the `--`
/// boundary — all forwarded to codex (`--cd` as the session folder). `Err` names the
/// first refused token.
///
/// Subcommand detection matches how codex actually dispatches (probed on 0.147):
/// a subcommand token is recognised in **any** positional slot, not just the
/// first — `codex please resume` dispatches Resume with `please` as the prompt,
/// and `codex --search resume` dispatches Resume behind a flag. An unknown flag takes
/// no value here, so the word after it is a positional and is judged as one. The one
/// exception is `resume` or `fork` as the first positional, which is hosted
/// ([`HOSTED_SUBCOMMANDS`]); every word after it is a session id or a prompt.
///
/// **This is the crate's single source of truth for the grammar.** It has two
/// callers: [`start`] (the user's `codeconnect codex` argv) and
/// [`crate::codex_host::parse_host_args`] (the passthrough the coordinator hands
/// the wrapper's TUI). The host deliberately reuses it rather than restating it —
/// it does not trust its caller, and a second copy of the grammar could drift on
/// which flags keep the TUI the broker's client.
pub fn validate_codex_argv(args: &[String]) -> Result<(), CodexRefusal> {
    scan_codex_argv(args).map(|_| ())
}

/// What one walk of a codex argv established.
struct ArgvScan {
    /// The argv to exec: a hosted subcommand ([`HOSTED_SUBCOMMANDS`]) if one was given,
    /// then every flag, value-taking ones rewritten into attached form, then `--`, then
    /// the positionals — and no `--cd`. See [`fence_positionals`].
    normalized: Vec<String>,
    /// Every `--cd` value, in order; a `--cd` with no value is an empty string. See
    /// [`session_folder`].
    cd: Vec<String>,
}

/// **Put every flag first and `--` in front of the positionals, so a bare token can
/// never dispatch.**
///
/// # Why the refusal table cannot be the safety property
///
/// `is_subcommand` is a closed list, and MEASURED: no enumeration codex emits carries
/// its hidden aliases — `cloud-tasks` dispatches on both binaries and appears in neither
/// `--help` nor any of the five completion shells. So a *future* hidden alias would not be
/// in the refusal table (nobody knew to add it), would be classified as prompt text, and
/// codex would dispatch it. That is the original escape class, and no amount of
/// list-keeping closes it, because the thing that would have to be enumerated cannot be.
///
/// `--` closes it structurally, and every claim here is measured on BOTH binaries:
///
/// | argv | result |
/// |---|---|
/// | `codex features` | **dispatches** — prints `Usage: codex features …` |
/// | `codex hello features` | **dispatches** — a prompt does not protect the token after it |
/// | `codex -- features` | prompt (`Error: stdin is not a terminal`) |
/// | `codex -- hello features` | clap error: `unexpected argument 'features' found` — refused, not dispatched |
/// | `codex -m gpt-5 -- features` | prompt — flags before the boundary still parse |
/// | `codex hello world` | clap error — identical to the `--` form, so the fence regresses nothing |
///
/// Both outcomes after the boundary are safe: one positional becomes the prompt, and a
/// second is a hard parse error. Neither dispatches.
///
/// # Why every flag moves in front of the fence
///
/// `--` terminates option parsing too, so a flag written after the prompt (`hi
/// --search`, which native codex accepts) would become prompt text behind it — 0.155.1
/// answers `unrecognized subcommand '--search'`. codex's parser does not care where a
/// flag sits relative to the positionals, so the flags are emitted first, in their own
/// relative order, then `--`, then the positionals in theirs.
///
/// # Why an unknown flag takes no value
///
/// A flag missing from the known table is forwarded unchanged, so a new codex flag
/// works the day it ships. Its arity is unknown, so it never takes the next word: that
/// word stays a positional behind the fence. A value flag written with a space then
/// gets codex's own "a value is required" error (codex's parser treats `--` as the end
/// of options, never as an option's value); written with `=` it works.
///
/// # Why every value-taking flag is REWRITTEN into attached form
///
/// Because otherwise the fence would rest on this walk's arity model agreeing with clap's,
/// and nothing checks that. So a future codex that kept the same flag spellings but changed
/// `-i` from greedy to single-value would be launched, and this walk — still sweeping
/// greedily — would consume `features` in
/// `-i a.png features` as an image, see no positional, insert no fence, and hand codex a
/// bare token it now dispatches.
///
/// The rewrite removes the dependency instead of trying to track it. Every flag in the
/// emitted argv is either a bool or carries its value **attached to the flag token**, so
/// no bare token is any flag's value under ANY arity model — which makes every remaining
/// bare token a positional, and all of them are fenced. This walk's arity model is
/// then only a UX classifier: get it wrong and codex reports a missing or surplus value,
/// which is legible and fail-closed. It can no longer expose a positional.
///
/// Both halves are MEASURED on both binaries. Every value-taking flag we admit accepts the
/// attached form (`--model=gpt-5`, `--config=a=b`, `--sandbox=read-only`, `--add-dir=/tmp`,
/// `--image=/tmp/a.png`, and the short spellings `-mgpt-5`, `-C/tmp`, `-ca=b`). And for the
/// one greedy flag, repeating the attached form is equivalent to the greedy sweep: driven
/// through a real 0.153 session, `--image=A --image=B` and `-i A B` produce **byte-identical**
/// `turn/start.params.input` — two `localImage` items, same paths, same order.
///
/// The refusal table stays, and is now the *legibility* layer rather than the safety one:
/// `codeconnect codex exec` still says exactly why it was refused instead of silently
/// becoming a prompt.
///
/// `resume` or `fork` as the first positional is the one bare token kept in front of the
/// fence: it leads the emitted argv ([`HOSTED_SUBCOMMANDS`]).
pub fn fence_positionals(args: &[String]) -> Result<Vec<String>, CodexRefusal> {
    Ok(scan_codex_argv(args)?.normalized)
}

fn scan_codex_argv(args: &[String]) -> Result<ArgvScan, CodexRefusal> {
    let mut i = 0;
    let mut flags: Vec<String> = Vec::with_capacity(args.len());
    let mut positionals: Vec<String> = Vec::new();
    let mut cd = Vec::new();
    let mut subcommand = None;
    let mut boundary = false;

    while i < args.len() {
        match classify(&args[i]) {
            // Everything after `--` is prompt content: forwarded verbatim, never
            // interpreted as a flag or a subcommand.
            Token::Boundary => {
                boundary = true;
                positionals.extend_from_slice(&args[i + 1..]);
                break;
            }

            Token::Flag { flag, attached } => match flag.arity {
                Arity::Bool => {
                    // A bool consumes no value, so its spelling cannot swallow anything;
                    // it is emitted canonically for uniformity, with any (rejectable)
                    // attached tail preserved so codex still sees what was written.
                    flags.push(match attached {
                        Some(value) => format!("{}={value}", flag.canonical),
                        None => flag.canonical.to_string(),
                    });
                    i += 1;
                }
                Arity::Value => {
                    let value = match attached {
                        Some(value) => {
                            i += 1;
                            Some(value)
                        }
                        None => match args.get(i + 1) {
                            // A spaced value: only a non-flag-shaped token is the
                            // value. codex (probed) does NOT let a value-option
                            // swallow a following flag-shaped token — it is a
                            // missing-value error and the follower is parsed as a
                            // flag. So a flag-shaped follower (or `--`, or nothing)
                            // is NOT consumed here; it stays in the stream to be
                            // re-classified and refused if it is forbidden.
                            Some(next) if !looks_like_flag(next) => {
                                let value = next.clone();
                                i += 2;
                                Some(value)
                            }
                            _ => {
                                i += 1;
                                None
                            }
                        },
                    };
                    refuse_owned_flag(flag.canonical)?;
                    // The session folder, not a TUI argument: see [`session_folder`].
                    if flag.canonical == "--cd" {
                        cd.push(value.unwrap_or_default());
                        continue;
                    }
                    flags.push(match &value {
                        Some(value) => format!("{}={value}", flag.canonical),
                        // No value to attach — codex will report the missing one, which
                        // is the same answer it would have given the spaced form.
                        None => flag.canonical.to_string(),
                    });
                }
                Arity::Values => {
                    // `-i`/`--image`. Grounded on 0.147: the **spaced** form is
                    // greedy (`-i a b c` ⇒ three image paths), but the **attached**
                    // form (`--image=a`, `-ia`) takes exactly that one value.
                    //
                    // MEASURED equivalent on 0.153, which is what lets the sweep be
                    // rewritten rather than passed through: `--image=A --image=B` and
                    // `-i A B` produce byte-identical `turn/start.params.input` — two
                    // `localImage` items, same paths, same order. So each swept value
                    // becomes its own attached occurrence.
                    i += 1;
                    match attached {
                        Some(value) => flags.push(format!("{}={value}", flag.canonical)),
                        None => {
                            let before = flags.len();
                            while i < args.len() && !looks_like_flag(&args[i]) {
                                flags.push(format!("{}={}", flag.canonical, args[i]));
                                i += 1;
                            }
                            // Nothing swept: forwarded bare, as a value flag is, for
                            // codex to report the missing value.
                            if flags.len() == before {
                                flags.push(flag.canonical.to_string());
                            }
                        }
                    }
                }
            },

            // A cluster of only bool short-flags (`-hV`), or a flag this walk does not
            // know: forwarded as written, consuming no value.
            Token::BoolCluster | Token::Unknown => {
                flags.push(args[i].clone());
                i += 1;
            }

            Token::Positional => {
                let token = &args[i];
                if subcommand.is_none()
                    && positionals.is_empty()
                    && HOSTED_SUBCOMMANDS.contains(&token.as_str())
                {
                    subcommand = Some(token.clone());
                } else if subcommand.is_none() && is_subcommand(token) {
                    return Err(CodexRefusal::Subcommand {
                        name: token.clone(),
                    });
                } else {
                    positionals.push(token.clone());
                }
                i += 1;
            }
        }
    }

    let mut normalized: Vec<String> = subcommand.into_iter().collect();
    normalized.extend(flags);
    if boundary || !positionals.is_empty() {
        normalized.push("--".to_string());
        normalized.extend(positionals);
    }
    Ok(ArgvScan { normalized, cd })
}

/// Whether an argv asks codex for its help or version text, before `--`: a flag that
/// is `--help` or `--version`, a short cluster that reaches `h` or `V` before its first
/// value short or unknown letter (codex acts on a cluster's letters in order), or a
/// first positional `help` (`codex help`, `codex help resume`).
///
/// Judged on the raw argv, ahead of the grammar, so no refusal can stand in front of
/// it. A spaced value is never flag-shaped (see [`scan_codex_argv`]), so every
/// flag-shaped token is a flag; a known flag's spaced value is stepped over so it is
/// never taken for the first positional.
fn asks_help_or_version(args: &[String]) -> bool {
    let mut i = 0;
    let mut positional_seen = false;
    while i < args.len() && args[i] != "--" {
        let token = &args[i];
        i += 1;
        if looks_like_flag(token) && flag_asks_help_or_version(token) {
            return true;
        }
        match classify(token) {
            Token::Positional => {
                if !positional_seen && token == "help" {
                    return true;
                }
                positional_seen = true;
            }
            Token::Flag {
                flag,
                attached: None,
            } if flag.arity != Arity::Bool => {
                while i < args.len() && !looks_like_flag(&args[i]) {
                    i += 1;
                    if flag.arity == Arity::Value {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn flag_asks_help_or_version(token: &str) -> bool {
    if let Some(long) = token.strip_prefix("--") {
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        return matches!(name, "help" | "version");
    }
    token.strip_prefix('-').is_some_and(|shorts| {
        shorts
            .chars()
            .map_while(known_short)
            .take_while(|flag| flag.arity == Arity::Bool)
            .any(|flag| matches!(flag.canonical, "--help" | "--version"))
    })
}

/// Whether a token would begin a flag to codex (used to bound greedy `--image`).
/// A bare `-` is a value (stdin sentinel), not a flag.
fn looks_like_flag(token: &str) -> bool {
    token.starts_with('-') && token != "-"
}

/// Codex, run with the caller's own arguments, in the caller's terminal.
///
/// What `codeconnect codex --help` (or `--version`, `-h`, `-V`, in any cluster) becomes:
/// the answer is codex's, so it is codex that is run, exactly as the caller would have
/// run it, inheriting stdio and handing back its exit status. The path is the codex a
/// launch would choose ([`first_codex`]); the launch's hash and freeze protect a
/// session this process hosts, and nothing is hosted here, so they are not paid.
fn codex_itself(codex: &Path, args: &[String]) -> Command {
    let mut command = Command::new(codex);
    command.args(args);
    command
}

/// The codex a launch would choose, for help and version: the first candidate that
/// is a native executable and not this binary, in the launch's own order
/// ([`codex_candidates`], as [`resolve_codex_bin`] walks it), judged by its magic number
/// alone — no hash, no freeze. When no candidate is native, the first existing one, so
/// a machine with only a script shim still gets codex's help.
fn first_codex(candidates: Vec<PathBuf>) -> Result<PathBuf> {
    let current = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok());
    let existing: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|candidate| candidate.is_file())
        .filter_map(|candidate| candidate.canonicalize().ok())
        .filter(|canonical| Some(canonical) != current.as_ref())
        .collect();
    let native = existing.iter().find(|path| starts_with_native_magic(path));
    native.or(existing.first()).cloned().ok_or_else(|| {
        anyhow!("could not find the codex binary; set codex_bin in ~/.codeconnect/config.json")
    })
}

/// Whether the file's first four bytes are a Mach-O magic number ([`is_native_magic`]).
fn starts_with_native_magic(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
        .is_ok_and(|()| is_native_magic(magic))
}

/// [`codex_candidates`] for this process: config, `CODECONNECT_CODEX_BIN`, the
/// well-known paths and `PATH`.
fn codex_candidates_for(config: &Config) -> Vec<PathBuf> {
    codex_candidates(
        config,
        std::env::var_os(CODEX_BIN_ENV).map(PathBuf::from),
        &protocol::home_dir(),
        protocol::tmux::search_path("codex"),
    )
}

/// Refuse the two flags that would take the TUI off the broker; forward the rest.
fn refuse_owned_flag(canonical: &str) -> Result<(), CodexRefusal> {
    match canonical {
        "--remote" | "--remote-auth-token-env" => Err(CodexRefusal::OwnedFlag {
            flag: canonical.to_string(),
            owner: "the app-server transport",
        }),
        _ => Ok(()),
    }
}

/// The directory the session runs in: the caller's `--cd`, resolved against the
/// directory `codeconnect codex` was run from, or that directory itself.
///
/// Native `codex --cd <dir>` makes `<dir>` the agent's working root. Here the
/// session's pane starts in it, so the app-server and the TUI both run there and the
/// session is registered under it. The TUI is handed no `--cd` of its own
/// ([`fence_positionals`] leaves it out): a relative one would be read against the
/// folder rather than against the caller's directory.
///
/// Refused before anything is created: a `--cd` with no directory, more than one
/// `--cd`, and a path that is not a directory.
fn session_folder(cd: &[String], caller_cwd: &Path) -> Result<PathBuf> {
    let dir = match cd {
        [] => return Ok(caller_cwd.to_path_buf()),
        [dir] if !dir.is_empty() => dir,
        [_] => bail!("`--cd` needs a directory"),
        _ => bail!("`--cd` was given more than once"),
    };
    let folder = caller_cwd.join(dir);
    match std::fs::metadata(&folder) {
        Ok(meta) if meta.is_dir() => Ok(folder),
        Ok(_) => bail!("`--cd {dir}`: {} is not a directory", folder.display()),
        Err(err) => bail!("`--cd {dir}`: {}: {err}", folder.display()),
    }
}

/// Whether a bare positional token is a codex subcommand name or alias.
///
/// The **union** of the top-level command sets of every codex build CodeConnect has
/// been measured against, enumerated from clap's own completion output — including the
/// **hidden** commands (`execpolicy`, `responses-api-proxy`, `stdio-to-uds`), the
/// visible aliases (`e` for `exec`, `a` for `apply`) and the hidden alias
/// (`cloud-tasks` for `cloud`, which completion does not emit). Matching a positional
/// token against this set is exactly how codex resolves a subcommand: `codex resume`
/// and `codex please resume` both dispatch Resume, never a prompt of the word.
///
/// **A union, not one version's set, and the difference is the whole point.** A token
/// this table does not know is classified [`Token::Positional`] and forwarded as prompt
/// text — so a subcommand introduced by a codex newer than the table is not refused,
/// it is *handed to codex, which dispatches it*. That is the escape class, and it was
/// live: 0.153 added `agents`, `queue` and `migrate-rollouts`, and `validate_codex_argv`
/// returned `Ok(())` for all three (measured). `codex agents` browses sessions on the
/// **shared local app-server daemon** and `codex queue` injects a message into another
/// session — both step around the broker entirely, which is the one thing this grammar
/// exists to prevent. Removing a token as codex retires it would re-open exactly that
/// hole for anyone still on the older build, so tokens are only ever added.
///
/// This table is the legibility layer, not the safety one: a subcommand it does not know
/// still cannot dispatch, because [`fence_positionals`] puts `--` in front of the first
/// positional.
const ROOT_SUBCOMMANDS: [&str; 34] = [
    // --- 0.153 additions. See this function's doc: each was measured dispatching
    // on a real 0.153 binary while `validate_codex_argv` waved it through.
    "agents",
    "queue",
    "migrate-rollouts",
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "mcp-server",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "execpolicy",
    "apply",
    "a",
    "resume",
    "archive",
    "delete",
    "unarchive",
    "fork",
    "cloud",
    "cloud-tasks",
    "responses-api-proxy",
    "stdio-to-uds",
    "exec-server",
    "features",
    "help",
];

fn is_subcommand(token: &str) -> bool {
    ROOT_SUBCOMMANDS.contains(&token)
}

/// The subcommands hosted like a new session, when they are the first positional before
/// `--`: each opens the same interactive TUI on an existing thread, and both accept
/// `--remote` (measured on 0.153.4 and 0.155.1). Neither has subcommands of its own, so
/// every positional after it is a session id or a prompt, fenced like any other. The
/// emitted argv leads with the subcommand, and the host puts it in front of `--remote`.
pub(crate) const HOSTED_SUBCOMMANDS: [&str; 2] = ["resume", "fork"];

/// The production CODE of a source file — everything before its test module, with
/// `//` comments (whole-line and trailing, doc comments included) removed — so a
/// source-reading test can find neither its own needle nor a commented-out call.
///
/// A `//` counts as a comment only outside a string literal on its line (an even number
/// of `"` before it), so `"unix://…"` is kept.
#[cfg(test)]
pub(crate) fn production_source(source: &str) -> String {
    let end = source
        .find("\n#[cfg(test)]\nmod tests {")
        .expect("the file has a test module");
    source[..end]
        .lines()
        .map(|line| {
            let comment = line
                .match_indices("//")
                .map(|(at, _)| at)
                .find(|&at| line[..at].matches('"').count() % 2 == 0);
            match comment {
                Some(at) => line[..at].trim_end(),
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body of the top-level production function whose signature starts with
    /// `signature`, from this file. Panics when there is none, so a renamed function
    /// fails the test that reads it rather than letting it read something else.
    fn production_fn(signature: &str) -> String {
        let source = production_source(include_str!("codex.rs"));
        let at = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} must exist in production code"));
        let rest = &source[at..];
        // rustfmt puts a top-level function's closing brace alone at column 0, so the
        // first `\n}\n` past the signature ends the body.
        rest[..rest.find("\n}\n").expect("a closed function body")].to_string()
    }

    // ------------------------------------------------------ binary resolution

    #[test]
    fn candidate_order_is_config_env_wellknown_path() {
        let config = Config {
            codex_bin: Some("/from/config/codex".into()),
            ..Config::default()
        };
        let candidates = codex_candidates(
            &config,
            Some(PathBuf::from("/from/env/codex")),
            Path::new("/home/u"),
            Some(PathBuf::from("/from/path/codex")),
        );
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/from/config/codex"),
                PathBuf::from("/from/env/codex"),
                PathBuf::from("/home/u/.local/bin/codex"),
                PathBuf::from("/opt/homebrew/bin/codex"),
                PathBuf::from("/usr/local/bin/codex"),
                PathBuf::from("/from/path/codex"),
            ]
        );
    }

    /// **The preflight refuses only a decoded "no".**
    ///
    /// The launch is stopped when the daemon that is running says it cannot host
    /// Codex — including the way a build predating the agent seam says it, by
    /// failing to decode the question at all. It is NOT stopped when no daemon is
    /// running, or when nothing could be established: a session started while
    /// `ccd` is down is a supported state, and it registers when `ccd` returns.
    /// Refusing on doubt would trade a real capability for a hiccup, and the
    /// property that actually protects a rolled-back daemon's history is the
    /// supervisor's withhold, which fails closed on this same answer.
    #[test]
    fn the_preflight_stops_a_launch_only_when_the_running_daemon_says_no() {
        use crate::daemon::AgentSupport;

        let refused = refuse_unless_hostable(AgentSupport::Refused(
            "the running ccd does not host codex; it hosts claude".into(),
        ))
        .expect_err("a daemon that says no must stop the launch");
        let refused = format!("{refused:#}");
        // The operator is told what is wrong and what it costs, not just "no".
        assert!(
            refused.contains("does not host codex"),
            "the refusal must carry the daemon's own reason: {refused}"
        );
        assert!(
            refused.contains("phone") || refused.contains("list"),
            "the refusal must say what the operator would lose: {refused}"
        );

        refuse_unless_hostable(AgentSupport::Absent)
            .expect("no daemon must never stop a launch: the session registers later");
        refuse_unless_hostable(AgentSupport::Indeterminate("timed out".into()))
            .expect("doubt must never stop a launch");
        refuse_unless_hostable(AgentSupport::Hosted).expect("a hosting daemon is the happy path");
    }

    // ------------------------------------------------------------ the charter

    /// The value of a flag in a charter argv, by name.
    fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .map(String::as_str)
    }

    /// A charter over a REAL file whose recorded digest is deliberately NOT that
    /// file's digest, so the two possible implementations give different answers.
    fn charter_over_a_real_file_with(
        sha256: &str,
        tui_args: &[String],
        terminal_size: Option<(u16, u16)>,
    ) -> (Vec<String>, PathBuf) {
        // `std::env::current_exe()` is a real, readable, absolute file — and one
        // whose actual sha256 is emphatically not the sentinel below.
        let real = std::env::current_exe().expect("this test binary is a real file");
        let resolved = ResolvedCodex {
            path: real.clone(),
            sha256: sha256.to_string(),
        };
        let argv = coordinator_charter(&CharterInputs {
            uid: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            launch_nonce: "nonce-a",
            custodian_nonce: "nonce-b",
            session_name: "cc-7",
            cwd: "/some/where",
            codex: &resolved,
            codex_home: Path::new("/home/u/.codex"),
            tui_args,
            terminal_size,
        });
        (argv, real)
    }

    /// The charter [`launch`] builds for this session folder and TUI argv.
    fn charter_for(folder: &Path, tui_args: &[String]) -> (Vec<String>, PathBuf) {
        let real = std::env::current_exe().expect("this test binary is a real file");
        let resolved = ResolvedCodex {
            path: real.clone(),
            sha256: "d".repeat(CODEX_SHA256_HEX_LEN),
        };
        let argv = coordinator_charter(&CharterInputs {
            uid: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            launch_nonce: "nonce-a",
            custodian_nonce: "nonce-b",
            session_name: "cc-7",
            cwd: &folder.to_string_lossy(),
            codex: &resolved,
            codex_home: Path::new("/home/u/.codex"),
            tui_args,
            terminal_size: None,
        });
        (argv, real)
    }

    #[test]
    fn the_launcher_carries_terminal_size_before_tui_arguments() {
        let prompt = vec!["a prompt".to_string()];
        let (argv, _) = charter_over_a_real_file_with(
            &"b".repeat(CODEX_SHA256_HEX_LEN),
            &prompt,
            Some((131, 43)),
        );
        assert_eq!(flag(&argv, "--terminal-size"), Some("131x43"));
        let wait_index = argv
            .iter()
            .position(|arg| arg == "--wait-for-terminal")
            .unwrap();
        let size_index = argv
            .iter()
            .position(|arg| arg == "--terminal-size")
            .unwrap();
        let boundary = argv.iter().position(|arg| arg == "--").unwrap();
        assert!(size_index < boundary);
        assert!(wait_index < boundary);
        assert_eq!(&argv[boundary + 1..], &prompt);
        let (argv, _) = charter_over_a_real_file_with(&"b".repeat(CODEX_SHA256_HEX_LEN), &[], None);
        assert!(!argv.iter().any(|arg| arg == "--terminal-size"));
        assert!(!argv.iter().any(|arg| arg == "--wait-for-terminal"));
    }

    /// **The executable-identity invariant, as a falsifiable test rather than a
    /// paragraph: the charter carries the digest resolution took, and never
    /// re-derives one.**
    ///
    /// The distinction is invisible on a quiet machine — re-hashing an unchanged
    /// file returns the same string — and it is the entire value of the pin on a
    /// busy one, where a `codex` install landing between resolution and this call
    /// makes the two answers differ and only the carried one still describes the
    /// bytes that were magic-checked and hashed.
    ///
    /// So the test makes them differ on purpose: the `ResolvedCodex` names a real
    /// file and records a digest that is *not* that file's. A carrying
    /// implementation emits the recorded value; a re-deriving one emits the file's.
    /// MUTATION-VERIFIED — replacing `inputs.codex.sha256.clone()` in
    /// [`coordinator_charter`] with `protocol::hash::sha256_file(&inputs.codex.path)`
    /// fails this assertion.
    #[test]
    fn the_charter_carries_the_resolve_time_digest_and_never_re_derives_it() {
        let pinned = "a".repeat(CODEX_SHA256_HEX_LEN);
        let (argv, real) = charter_over_a_real_file_with(&pinned, &[], None);

        // The premise: these two really are different answers about one path.
        let on_disk = protocol::hash::sha256_file(&real).expect("hash this test binary");
        assert_ne!(
            on_disk, pinned,
            "the fixture must make carrying and re-deriving distinguishable, \
             or this test proves nothing"
        );

        assert_eq!(
            flag(&argv, "--codex-sha256"),
            Some(pinned.as_str()),
            "the charter must carry the digest resolution recorded, not one re-derived \
             from the path: {argv:?}"
        );
        // And the well-formedness the coordinator will re-check on the way in.
        assert!(parse_codex_sha256(&pinned).is_ok());
    }

    /// The charter fills every dimension the coordinator and the host refuse to
    /// default, at the values the live gates run on, and leaves `--tmux-socket` to
    /// the coordinator's own default (the one shared server).
    #[test]
    fn the_charter_names_every_undefaulted_dimension_and_no_tmux_socket() {
        let (argv, _) = charter_over_a_real_file_with(&"b".repeat(CODEX_SHA256_HEX_LEN), &[], None);

        for required in [
            "--uid",
            "--nonce",
            "--custodian-nonce",
            "--session-name",
            "--cwd",
            "--deadline-ms",
            "--codex",
            "--codex-sha256",
            "--codex-home",
        ] {
            assert!(
                flag(&argv, required).is_some(),
                "the charter must name {required}, which nothing downstream defaults: {argv:?}"
            );
        }
        assert_eq!(flag(&argv, "--session-name"), Some("cc-7"));
        assert_eq!(flag(&argv, "--cwd"), Some("/some/where"));
        // The launcher names no session policy: the TUI chooses its sandbox and approval
        // policy exactly as native codex does.
        for policy in [
            "--approval-policy",
            "--approvals-reviewer",
            "--sandbox",
            "--hooks-enabled",
        ] {
            assert!(
                !argv.iter().any(|a| a == policy),
                "the charter must not name {policy}: {argv:?}"
            );
        }
        assert_eq!(
            flag(&argv, "--deadline-ms"),
            Some(LAUNCH_DEADLINE_MS.to_string().as_str())
        );
        assert!(
            !argv.iter().any(|a| a == "--tmux-socket"),
            "the launcher must not restate the tmux socket; the coordinator defaults to \
             the one server `claude`, `ls` and `attach` all use: {argv:?}"
        );
        // And no test-only charter flag can ever ride out of a shipping launcher.
        for forbidden in ["--test-bringup", "--test-newsession"] {
            assert!(
                !argv.iter().any(|a| a == forbidden),
                "{forbidden} must never appear in a real charter: {argv:?}"
            );
        }
    }

    /// The `--` boundary: a passthrough is forwarded verbatim behind it, and is
    /// absent entirely when there is none — so no empty boundary can turn a later
    /// coordinator flag into a TUI argument.
    #[test]
    fn the_charter_forwards_a_passthrough_only_behind_the_boundary() {
        let (bare, _) = charter_over_a_real_file_with(&"c".repeat(CODEX_SHA256_HEX_LEN), &[], None);
        assert!(
            !bare.iter().any(|a| a == "--"),
            "an empty passthrough must add no boundary: {bare:?}"
        );

        let passthrough = vec!["--model".to_string(), "gpt-5".to_string(), "hi".to_string()];
        let (with, _) =
            charter_over_a_real_file_with(&"c".repeat(CODEX_SHA256_HEX_LEN), &passthrough, None);
        let at = with
            .iter()
            .position(|a| a == "--")
            .expect("a passthrough must be fenced by the boundary");
        assert_eq!(
            &with[at + 1..],
            passthrough.as_slice(),
            "everything past the boundary is the TUI's, verbatim: {with:?}"
        );
        // The boundary is last: nothing the launcher owns may follow it.
        assert!(
            with[..at].iter().any(|a| a == "--codex-home"),
            "the launcher's own dimensions must all precede the boundary: {with:?}"
        );
    }

    /// **A launch that could not look at a record has to say so, and a launch that
    /// found a live one must not.**
    ///
    /// One `launch.json` that is not valid JSON at all makes `other_freeze_claim_on`
    /// answer `Standing` for EVERY vnode on the machine, so no freeze anywhere is ever
    /// cleared again — and the launcher swallowed both halves of that: the unreadable
    /// record (`Skipped`) and the veto it caused (`FreezeClaimStanding`). The user got
    /// a codex that could not be updated and not one word about why. A record that
    /// could not be read is rare by construction, so saying it does not put the
    /// per-launch noise back; a claim left standing is the ordinary live-launch case
    /// on every busy machine, and stays the daemon's to log.
    #[test]
    fn a_launch_says_what_it_could_not_look_at_and_stays_quiet_about_what_it_is_waiting_for() {
        use crate::codex_launch::SweepAction;
        for unreadable in [
            SweepAction::Skipped {
                uid: "u".into(),
                why: "corrupt or truncated".into(),
            },
            SweepAction::ScanFailed {
                what: "u".into(),
                why: "the record could not be stat'ed".into(),
            },
        ] {
            let said = launcher_line(&unreadable).unwrap_or_else(|| {
                panic!(
                    "a record nobody could look at must be said: \
                                           {unreadable:?}"
                )
            });
            assert!(said.contains("u"), "and it must name the record: {said}");
        }
        assert!(
            launcher_line(&SweepAction::FreezeClaimStanding {
                uid: "u".into(),
                why: "a live launch is behind the vnode".into(),
            })
            .is_none(),
            "a claim left standing is the ordinary case and belongs in the daemon's log, \
             not at a terminal on every launch"
        );
    }

    /// **The launch clears freezes left standing BEFORE it takes one, and the
    /// order is the whole of the fix.**
    ///
    /// `probe_codex` holds the executable's own freeze lock for its entire run, and
    /// the pass needs that same lock to prove no live holder stands behind the vnode.
    /// Run after the probe it would meet this launch's own lock, defer, and clear
    /// nothing; run inside it, it would be reasoning about a bit this process had just
    /// set. Before is the only position from which its answer is about anybody else.
    ///
    /// A source read for the same reason its neighbours are: `start` resolves a real
    /// codex, shells out for each of its questions and preflights the daemon, so a unit
    /// test cannot
    /// reach the ordering inside it — and an ordering that regressed here would fail
    /// silently, as a pass that runs on every launch and can never clear anything.
    #[test]
    fn the_launch_clears_freezes_left_standing_before_its_probe_takes_one() {
        let start = production_fn("pub fn start(passthrough: &[String]) -> Result<()> {");

        let sweep = start
            .find("clear_freezes_left_standing()")
            .expect("a launch must run the pass that clears a freeze left standing");
        let probe = start
            .find("probe_codex(")
            .expect("a launch must probe the binary it is about to host");
        assert!(
            sweep < probe,
            "the pass must run before the probe takes the lock it needs, or it can \
             only ever defer to this launch's own freeze"
        );
    }

    /// **The launcher's own freeze is armed for the ending that runs no `Drop`.**
    ///
    /// `probe_codex` takes a freeze before a uid exists, so no launch record names
    /// it and no janitor can ever act on it; the guard's `Drop` is the whole of its
    /// safety, and `SIGINT` — a keystroke away during a launch — runs no `Drop`. That
    /// leaked `UF_IMMUTABLE` on the real binary on demand, every time.
    ///
    /// `exec_freeze_signal.rs` proves the mechanism end-to-end against a real process
    /// and a real signal; what it cannot see is whether the PRODUCTION probe uses it.
    /// This is that half: a source read, for the same reason
    /// [`the_preflight_refuses_before_a_uid_or_a_tmux_name_is_taken`] is one — an
    /// ordering inside a function that shells out to a real codex is not reachable
    /// from a unit test, and a probe that quietly stopped installing the handler
    /// would otherwise regress in silence.
    #[test]
    fn the_launch_probe_installs_the_signal_release_before_it_takes_the_freeze() {
        let probe = production_fn("fn probe_codex(resolved: &ResolvedCodex) -> Result<()> {");

        let install = probe
            .find("install_freeze_signal_release()")
            .expect("the launch probe must install the signal release");
        let freeze = probe
            .find("verify_codex_identity(")
            .expect("the launch probe must take the freeze");
        assert!(
            install < freeze,
            "the handler must be installed before the flag goes on, or the window \
             between them is uncovered"
        );
        // The arming itself is no longer spelled here, and that is the fix rather
        // than a gap: it used to be a call this callback had to remember, which left
        // the interval between `fchflags` and the call uncovered. It now happens
        // inside the freeze primitive, before the flag goes on, and is pinned there
        // by `protocol::hash`'s own
        // `the_freeze_arms_the_release_before_it_sets_the_flag`.
    }

    /// The daemon preflight runs before ANY identity is minted.
    ///
    /// **This pins an ordering that only a source read can see.** `new-old-new-real.sh`
    /// step 7(g) measures the preflight's refusal by the absence of a launch record,
    /// which is the first *durable* artifact — so it proves no launch was recorded,
    /// and cannot distinguish that from a uid minted and a `cc-N` taken just before
    /// the refusal. Those two are the visible fleet: `next_session_name` consumes a
    /// name off the shared tmux server and `uid::new` burns a stamp the event log is
    /// keyed by. Nothing observable would fail if they moved in front of
    /// `refuse_unless_hostable`, so the ordering is asserted here instead.
    ///
    /// Read the source rather than call anything, for the same reason
    /// `cc-hook`'s `version_flag_is_recognised_before_stdin_is_touched` does: the
    /// order of two side effects inside a function that talks to a live daemon and a
    /// live tmux server is not reachable from a unit test, and this is exactly the
    /// class of regression that would otherwise land silently.
    #[test]
    fn the_preflight_refuses_before_a_uid_or_a_tmux_name_is_taken() {
        let body = production_fn;

        let start = body("pub fn start(passthrough: &[String]) -> Result<()> {");
        let preflight = start
            .find("refuse_unless_hostable(")
            .expect("start must run the preflight");
        let launch = start.find("launch(&resolved").expect("start must launch");
        assert!(
            preflight < launch,
            "the preflight must run before the launch it guards"
        );
        for minter in ["uid::new(", "next_session_name("] {
            assert!(
                !start.contains(minter),
                "{minter} moved into start(); it must stay behind the preflight, in launch()"
            );
        }

        // And the other half: the minters really are in `launch`, so the assertion
        // above is about where they are rather than about a spelling that vanished.
        let launch_body =
            body("fn launch(resolved: &ResolvedCodex, folder: &Path, passthrough: &[String])");
        for minter in ["uid::new(", "next_session_name("] {
            assert!(
                launch_body.contains(minter),
                "{minter} is no longer in launch(), so this test guards nothing"
            );
        }
    }

    /// `CODEX_HOME` is honoured when set, and codex's own default is used when it
    /// is not — because that is where the operator's `auth.json` lives.
    #[test]
    fn the_codex_home_default_is_the_operators_own() {
        let home = protocol::home_dir().join(".codex");
        // Read through the same accessor the launcher uses, so an override that
        // stopped being honoured would fail here.
        let observed = codex_home();
        match std::env::var_os("CODEX_HOME") {
            Some(explicit) => assert_eq!(observed, PathBuf::from(explicit)),
            None => assert_eq!(observed, home),
        }
    }

    #[test]
    fn candidate_list_omits_absent_config_env_and_path_and_the_unevidenced_dir() {
        let candidates = codex_candidates(&Config::default(), None, Path::new("/home/u"), None);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/home/u/.local/bin/codex"),
                PathBuf::from("/opt/homebrew/bin/codex"),
                PathBuf::from("/usr/local/bin/codex"),
            ]
        );
        // The invented `~/.codex/bin/codex` layout is not a candidate.
        assert!(!candidates.contains(&PathBuf::from("/home/u/.codex/bin/codex")));
    }

    #[test]
    fn config_override_wins_and_resolves_the_versioned_path() {
        // A standalone-shaped layout: a `current` symlink to a versioned release
        // dir, and an invocation symlink through it. Resolution must return the
        // canonicalised versioned file, not the moving symlink.
        let root = tempdir();
        let release = root.join("releases/0.147.0-test/bin");
        std::fs::create_dir_all(&release).unwrap();
        let real = release.join("codex");
        // Native Mach-O magic, so it passes the native-executable gate.
        std::fs::write(&real, [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0]).unwrap();
        make_executable(&real);

        let standalone = root.join("standalone");
        std::fs::create_dir_all(&standalone).unwrap();
        symlink(
            root.join("releases/0.147.0-test"),
            standalone.join("current"),
        );

        let bindir = root.join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let invocation = bindir.join("codex");
        symlink(standalone.join("current/bin/codex"), &invocation);

        let config = Config {
            codex_bin: Some(invocation.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let resolved = resolve_codex_bin(&config).expect("must resolve the configured codex");
        assert_eq!(resolved.path, real.canonicalize().unwrap());
        assert!(resolved
            .path
            .to_string_lossy()
            .contains("releases/0.147.0-test/bin/codex"));

        cleanup(&root);
    }

    #[test]
    fn a_missing_override_falls_through_to_a_well_known_path() {
        let root = tempdir();
        let home = root.join("home");
        let wellknown = home.join(".local/bin");
        std::fs::create_dir_all(&wellknown).unwrap();
        let real = wellknown.join("codex");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        make_executable(&real);

        let candidates = codex_candidates(
            &Config {
                codex_bin: Some("/nonexistent/codex".into()),
                ..Config::default()
            },
            None,
            &home,
            None,
        );
        let first_real = candidates.into_iter().find(|c| c.is_file()).unwrap();
        assert_eq!(first_real, real);

        cleanup(&root);
    }

    #[test]
    fn the_self_resolution_guard_skips_this_binary() {
        let me = std::env::current_exe().unwrap();
        let config = Config {
            codex_bin: Some(me.to_string_lossy().into_owned()),
            ..Config::default()
        };
        // Resolution finds a real codex further down the list or fails, but never
        // returns the shim itself.
        if let Ok(resolved) = resolve_codex_bin(&config) {
            assert_ne!(
                resolved.path.canonicalize().ok(),
                me.canonicalize().ok(),
                "must never resolve to the shim itself"
            );
        }
    }

    #[test]
    fn resolves_the_real_codex_on_this_machine() {
        if !on_path("codex") {
            eprintln!("skipped: no `codex` on PATH — nothing to resolve");
            return;
        }
        let resolved =
            resolve_codex_bin(&Config::default()).expect("codex must be installed on PATH");
        // A single canonicalised path, which is a real file and not the shim.
        assert!(resolved.path.is_file());
        assert_eq!(resolved.path, resolved.path.canonicalize().unwrap());
        assert!(
            !resolved.path.ends_with("codeconnect"),
            "must never resolve to the shim itself: {}",
            resolved.path.display()
        );
        // The resolved standalone binary is a real native executable.
        assert!(
            is_native(&resolved.path),
            "the standalone binary must be native: {}",
            resolved.path.display()
        );
    }

    #[test]
    fn a_script_wrapper_is_never_resolved_but_a_native_binary_is() {
        let root = tempdir();

        // A shebang script masquerading as codex (the npm `codex.js` shape).
        let wrapper = root.join("codex.js");
        std::fs::write(&wrapper, b"#!/usr/bin/env node\nconsole.log('x');\n").unwrap();
        make_executable(&wrapper);
        assert!(!is_native(&wrapper));

        // A file carrying Mach-O 64-bit little-endian magic is treated as native.
        let native = root.join("codex-native");
        std::fs::write(&native, [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0]).unwrap();
        make_executable(&native);
        assert!(is_native(&native));

        // Pointing `codex_bin` at the wrapper never yields the wrapper: either a
        // native candidate later in the list wins, or resolution refuses naming
        // the wrapper. (Robust whether or not a real codex exists in this env.)
        let cfg_wrapper = Config {
            codex_bin: Some(wrapper.to_string_lossy().into_owned()),
            ..Config::default()
        };
        match resolve_codex_bin(&cfg_wrapper) {
            Ok(resolved) => {
                assert_ne!(resolved.path, wrapper.canonicalize().unwrap());
                assert!(is_native(&resolved.path));
            }
            Err(e) => assert!(
                e.to_string().contains("wrapper"),
                "refusal should name the wrapper: {e}"
            ),
        }

        // Pointing it at the native file resolves to exactly that file.
        let cfg_native = Config {
            codex_bin: Some(native.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let resolved = resolve_codex_bin(&cfg_native).expect("native binary must be accepted");
        assert_eq!(resolved.path, native.canonicalize().unwrap());

        cleanup(&root);
    }

    // ------------------------------------------------------- executable identity

    #[test]
    fn resolution_pins_the_bytes_it_inspected_not_just_the_name() {
        let root = tempdir();
        let bin = root.join("codex");
        let bytes = [0xCFu8, 0xFA, 0xED, 0xFE, 1, 2, 3, 4];
        std::fs::write(&bin, bytes).unwrap();
        make_executable(&bin);

        let config = Config {
            codex_bin: Some(bin.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let resolved = resolve_codex_bin(&config).expect("a native candidate resolves");
        // The digest is of the file, computed independently of the code under test.
        assert_eq!(resolved.sha256, protocol::hash::sha256_hex(&bytes));
        // And it is the wire form the two charters will accept.
        assert_eq!(
            parse_codex_sha256(&resolved.sha256).unwrap(),
            resolved.sha256
        );

        cleanup(&root);
    }

    /// **The gate's own scenario, staged.** Resolve the binary, then replace the
    /// bytes at that exact resolved path — the `standalone/current` flip, the npm
    /// overwrite, the install landing mid-launch — and prove the verify that stands
    /// in front of every exec refuses.
    #[test]
    fn a_binary_swapped_after_resolution_is_caught_before_it_can_be_exec_d() {
        let root = tempdir();
        let bin = root.join("codex");
        std::fs::write(&bin, [0xCFu8, 0xFA, 0xED, 0xFE, b'o', b'l', b'd']).unwrap();
        make_executable(&bin);

        let config = Config {
            codex_bin: Some(bin.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let resolved = resolve_codex_bin(&config).expect("a native candidate resolves");

        // Unchanged: every exec site is free to proceed.
        verify_codex_identity(
            &resolved.path,
            &resolved.sha256,
            "in the unchanged case",
            protocol::hash::LockHold::UntilReleased,
            |_| Ok(()),
        )
        .expect("an untouched binary must verify");

        // The swap. Same path, same canonical name, different bytes — and still a
        // perfectly valid native Mach-O, so the magic check alone would wave it
        // through. Only the digest sees it.
        std::fs::write(&bin, [0xCFu8, 0xFA, 0xED, 0xFE, b'n', b'e', b'w']).unwrap();
        let err = verify_codex_identity(
            &resolved.path,
            &resolved.sha256,
            "immediately before the app-server spawn",
            protocol::hash::LockHold::UntilReleased,
            |_| Ok(()),
        )
        .expect_err("a replaced binary must be refused");
        let text = format!("{err:#}");
        assert!(
            text.contains(&resolved.sha256),
            "names what was pinned: {text}"
        );
        assert!(
            text.contains(&protocol::hash::sha256_hex(&[
                0xCFu8, 0xFA, 0xED, 0xFE, b'n', b'e', b'w'
            ])),
            "names what is there now: {text}"
        );
        assert!(
            text.contains("immediately before the app-server spawn"),
            "names WHERE in the launch it moved: {text}"
        );
        assert!(
            text.contains("Refusing to run it"),
            "says plainly that nothing was executed: {text}"
        );

        // A truncation is a swap too — the digest covers the whole file, not a
        // prefix, so a binary that keeps its magic and loses its tail is refused.
        std::fs::write(&bin, [0xCFu8, 0xFA, 0xED, 0xFE]).unwrap();
        assert!(verify_codex_identity(
            &resolved.path,
            &resolved.sha256,
            "after truncation",
            protocol::hash::LockHold::UntilReleased,
            |_| Ok(())
        )
        .is_err());

        // And a file that is gone is a refusal, never a pass: "I could not check"
        // and "it is unchanged" must not share an answer.
        std::fs::remove_file(&bin).unwrap();
        let err = verify_codex_identity(
            &resolved.path,
            &resolved.sha256,
            "after deletion",
            protocol::hash::LockHold::UntilReleased,
            |_| Ok(()),
        )
        .expect_err("an unreadable binary must be refused");
        assert!(
            format!("{err:#}").contains("re-reading"),
            "the refusal must say the check itself failed: {err:#}"
        );

        cleanup(&root);
    }

    #[test]
    fn the_magic_verdict_and_the_digest_come_from_one_read() {
        let root = tempdir();

        // A shebang script is a Wrapper and carries no digest to pin.
        let wrapper = root.join("codex.js");
        std::fs::write(&wrapper, b"#!/usr/bin/env node\n").unwrap();
        assert!(matches!(
            inspect_candidate(&wrapper),
            CandidateIdentity::Wrapper
        ));

        // A file too short to hold a magic number is a Wrapper, not a Native whose
        // magic was read out of zero padding.
        let stub = root.join("stub");
        std::fs::write(&stub, [0xCFu8, 0xFA]).unwrap();
        assert!(matches!(
            inspect_candidate(&stub),
            CandidateIdentity::Wrapper
        ));

        // A native file yields the digest of the WHOLE file — the four magic bytes
        // included — which is what makes the verdict and the pin one statement.
        let native = root.join("codex");
        let bytes: Vec<u8> = [0xCAu8, 0xFE, 0xBA, 0xBE]
            .iter()
            .copied()
            .chain((0u8..=255).cycle().take(5000))
            .collect();
        std::fs::write(&native, &bytes).unwrap();
        match inspect_candidate(&native) {
            CandidateIdentity::Native { sha256 } => {
                assert_eq!(sha256, protocol::hash::sha256_hex(&bytes))
            }
            _ => panic!("universal-binary magic must be accepted as native"),
        }

        // A path that is not there is Unusable — distinct from Wrapper, because
        // "we could not look" is not "we looked and it was a script".
        assert!(matches!(
            inspect_candidate(&root.join("absent")),
            CandidateIdentity::Unusable(_)
        ));

        cleanup(&root);
    }

    /// **The rename window at the resolution site.** The verification sites are not
    /// the only place a 220 MB read happens: resolution takes one too, and the digest
    /// it mints is the pin everything downstream compares against. A rename landing
    /// inside *that* read produces a `ResolvedCodex` whose digest describes bytes the
    /// pathname no longer reaches — and because the replacement has settled by the
    /// time any verify runs, every later check confirms it as "unchanged". The whole
    /// chain would be internally consistent and about the wrong file.
    ///
    /// Staged as the real shape (an atomic `rename` over the name), synchronised on
    /// an observable fact rather than a sleep: a descriptor in this process standing
    /// open on the target's inode is proof `inspect_candidate` has opened it. The
    /// file is large enough that the swap lands hundreds of milliseconds short of
    /// EOF, so the ordering holds by construction.
    #[test]
    fn a_binary_renamed_over_mid_inspection_is_never_resolved() {
        let root = tempdir();
        let target = root.join("codex");
        let replacement = root.join("codex.new");

        // 32 MiB of native-looking bytes: the magic and the digest would both be
        // perfectly valid, so nothing but the vnode comparison can refuse this.
        let mut original = Vec::with_capacity(32 * 1024 * 1024);
        original.extend_from_slice(&[0xCFu8, 0xFA, 0xED, 0xFE]);
        original.extend((0u8..=255).cycle().take(32 * 1024 * 1024 - 4));
        std::fs::write(&target, &original).unwrap();
        std::fs::write(&replacement, [0xCFu8, 0xFA, 0xED, 0xFE, b'n', b'e', b'w']).unwrap();
        make_executable(&target);
        make_executable(&replacement);
        let (ino, size) = {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&target).unwrap();
            (meta.ino(), meta.len())
        };

        let inspected = target.clone();
        let inspector = std::thread::spawn(move || match inspect_candidate(&inspected) {
            CandidateIdentity::Native { sha256 } => Err(sha256),
            CandidateIdentity::Wrapper => Ok("wrapper".to_string()),
            CandidateIdentity::Unusable(why) => Ok(why),
        });

        // Wait for the read to have started, then swap. `/dev/fd` is this process's
        // own descriptor table; an entry reporting the target's inode is the proof.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            use std::os::unix::fs::MetadataExt;
            assert!(
                std::time::Instant::now() < deadline,
                "inspect_candidate never opened the target: the race was not staged"
            );
            let open = std::fs::read_dir("/dev/fd").into_iter().flatten().any(|e| {
                e.ok()
                    .and_then(|e| std::fs::metadata(e.path()).ok())
                    .is_some_and(|m| m.ino() == ino && m.len() == size)
            });
            if open {
                break;
            }
            std::thread::yield_now();
        }
        std::fs::rename(&replacement, &target).expect("the atomic replacement");

        match inspector
            .join()
            .expect("the inspecting thread must not panic")
        {
            Ok(why) => assert!(
                why.contains("replaced while it was being read"),
                "the rejection must say the file moved under the read: {why}"
            ),
            // Native is the dangerous answer: a pin minted over a name that has
            // already moved on to somebody else's bytes.
            Err(sha256) => panic!(
                "the mid-inspection swap was not caught; resolution minted the pin {sha256} \
                 (the original hashes {})",
                protocol::hash::sha256_hex(&original)
            ),
        }

        cleanup(&root);
    }

    #[test]
    fn every_mach_o_magic_is_recognised_and_nothing_else_is() {
        for magic in [
            0xFEED_FACEu32,
            0xFEED_FACF,
            0xCEFA_EDFE,
            0xCFFA_EDFE,
            0xCAFE_BABE,
            0xBEBA_FECA,
            0xCAFE_BABF,
            0xBFBA_FECA,
        ] {
            assert!(is_native_magic(magic.to_be_bytes()), "{magic:#x}");
        }
        // `#!/` and ELF are the two shapes that actually turn up here.
        assert!(!is_native_magic(*b"#!/u"));
        assert!(!is_native_magic([0x7F, b'E', b'L', b'F']));
        assert!(!is_native_magic([0, 0, 0, 0]));
    }

    #[test]
    fn a_digest_on_the_wire_has_exactly_one_valid_spelling() {
        let good = protocol::hash::sha256_hex(b"codex");
        assert_eq!(parse_codex_sha256(&good).unwrap(), good);

        // Uppercase is refused rather than folded: one digest, one spelling, so a
        // string comparison can never call identical bytes different.
        assert!(parse_codex_sha256(&good.to_uppercase()).is_err());
        // Wrong width in both directions, and non-hex characters.
        assert!(parse_codex_sha256(&good[..63]).is_err());
        assert!(parse_codex_sha256(&format!("{good}0")).is_err());
        assert!(parse_codex_sha256(&"g".repeat(64)).is_err());
        assert!(parse_codex_sha256("").is_err());
        // The refusal says what was expected, so a caller can fix it.
        let err = parse_codex_sha256("nope").unwrap_err().to_string();
        assert!(err.contains("64"), "names the width: {err}");
        assert!(err.contains("lowercase"), "names the case: {err}");
    }

    /// `codex --version` is itself one of the pathname opens, so a version that
    /// parsed is not on its own a version that describes the bytes this launch will
    /// carry. Against the **real** installed codex: the launch is refused because the
    /// file does not match what was pinned.
    ///
    /// The refusal lands **before** the exec rather than after it. The bytes are
    /// frozen and verified first, so a binary that does not match the pin never runs
    /// as `codex --version` at all — no unpinned bytes are exec'd pre-gate. The
    /// `when` assertion below is what pins that ordering.
    ///
    /// A digest that never matched stands in for a swap that happened before the
    /// check — which `probe_codex` cannot tell apart from any other mismatch,
    /// and does not need to: it asks only "are these still the pinned bytes?".
    #[test]
    fn a_parsed_version_alone_does_not_let_a_launch_through() {
        if !on_path("codex") {
            eprintln!("skipped: no `codex` on PATH — nothing to version-check");
            return;
        }
        let real = resolve_codex_bin(&Config::default()).unwrap();
        // Only the mismatch arm is staged, so the suite pays for one hash of a 220 MB
        // binary rather than two; the matching arm is the ordinary launch.
        let swapped = ResolvedCodex {
            path: real.path.clone(),
            sha256: protocol::hash::sha256_hex(b"some other codex"),
        };
        let err = probe_codex(&swapped)
            .expect_err("a version check whose binary does not match the pin must refuse");
        let text = format!("{err:#}");
        assert!(
            text.contains("not the one this launch pinned"),
            "the refusal must be the identity check, not a parse failure: {text}"
        );
        assert!(
            text.contains("before `codex --version`"),
            "it must name the moment, so an operator can see the exec is guarded — and the \
             moment is now BEFORE the exec, not after it: the bytes are frozen and verified \
             first, so a mismatched pin never becomes a running `--version` at all: {text}"
        );
    }

    /// The identity check runs BEFORE the version output is interpreted, so a
    /// binary that moved is always reported as a binary that moved.
    ///
    /// Staged with a script whose `--version` is deliberately unparseable: with the
    /// checks in the other order this reports "could not read a version", which is
    /// true and sends the operator after the wrong problem — a malformed codex
    /// rather than a swapped one. No native binary is needed, because
    /// `probe_codex` only execs the path it is given.
    #[test]
    fn a_swapped_binary_is_reported_as_swapped_and_not_as_malformed() {
        let root = tempdir();
        let script = root.join("codex");
        std::fs::write(&script, b"#!/bin/sh\necho 'not a version at all'\n").unwrap();
        make_executable(&script);

        let swapped = ResolvedCodex {
            path: script.clone(),
            sha256: protocol::hash::sha256_hex(b"what was actually pinned"),
        };
        let err = probe_codex(&swapped).expect_err("a moved binary must be refused");
        let text = format!("{err:#}");
        assert!(
            text.contains("not the one this launch pinned"),
            "the operator must be told the binary MOVED, not that it is malformed: {text}"
        );
        assert!(
            !text.contains("could not read a version"),
            "the parse failure must not be what surfaces: {text}"
        );

        // With the right digest, the same script reaches the parse and fails there —
        // proving the identity check is not simply swallowing every error.
        let honest = ResolvedCodex {
            path: script.clone(),
            sha256: protocol::hash::sha256_file(&script).unwrap(),
        };
        let err = probe_codex(&honest).expect_err("an unparseable version must be refused");
        assert!(
            format!("{err:#}").contains("could not read a version"),
            "an unmoved binary's real problem must still surface: {err:#}"
        );

        cleanup(&root);
    }

    // ------------------------------------------------------------- the version

    #[test]
    fn parses_only_the_two_exact_version_shapes() {
        assert_eq!(
            parse_codex_version("codex-cli 0.147.0\n").as_deref(),
            Some("0.147.0")
        );
        assert_eq!(parse_codex_version("0.147.0").as_deref(), Some("0.147.0"));
        assert_eq!(
            parse_codex_version("  codex-cli   1.2.3-rc1  \n").as_deref(),
            Some("1.2.3-rc1")
        );
        // Ambiguous / extra tokens are rejected rather than guessed.
        assert_eq!(
            parse_codex_version("codex-cli 0.148.0 compatibility 0.147.0").as_deref(),
            None
        );
        // A second non-empty line is rejected: the first line alone must not pass.
        assert_eq!(
            parse_codex_version("codex-cli 0.147.0\ncompatibility 0.148.0").as_deref(),
            None
        );
        assert_eq!(parse_codex_version("codex-cli").as_deref(), None);
        assert_eq!(parse_codex_version("some tool 0.147.0").as_deref(), None);
        assert_eq!(
            parse_codex_version("codex-cli notaversion").as_deref(),
            None
        );
        assert_eq!(parse_codex_version("").as_deref(), None);
    }

    /// The version is READ — a binary that cannot state one is not hosted — and its
    /// value decides nothing: [`probe_codex`] returns `()`, so no version reaches
    /// `start`.
    ///
    /// Read the source rather than call anything, the same idiom
    /// `the_preflight_refuses_before_a_uid_or_a_tmux_name_is_taken` uses and for the
    /// same reason: `start` talks to a live binary, which no unit test can reach by
    /// calling it.
    #[test]
    fn the_version_is_read_and_its_value_decides_nothing() {
        let start = production_fn("pub fn start(passthrough: &[String]) -> Result<()> {");

        assert!(
            start.contains("probe_codex("),
            "the version must still be READ — a binary that cannot say what it is is not \
             launched"
        );
        let probe = production_fn("fn probe_codex(resolved: &ResolvedCodex) -> Result<()> {");
        assert!(
            probe.contains("parse_codex_version("),
            "the probe must parse what it read"
        );
    }

    /// **A shipping build cannot record a session, whatever its environment says.**
    ///
    /// The tee records every frame VERBATIM — that is its whole purpose, and it is why
    /// `crate::redact`'s guarantee (a log line never carries client-chosen text) does
    /// not apply to it. A production launch that could enable it would be a production
    /// launch that could write a user's prompts, file contents and tool output to a
    /// plaintext file at a path someone else chose.
    ///
    /// The containment is COMPILE-TIME: the recorder is behind the `frame-tee` cargo
    /// feature, and without it `codex_broker::FrameTee::from_env` does not read the
    /// variable at all. That is what this asserts first, by calling it with the variable
    /// set. The source scan below is the second line — a capture build should still not
    /// have a launcher that switches the recorder on by itself — and it reads the source
    /// for the same reason `the_version_is_read_and_its_value_decides_nothing` does: what
    /// must be proven is the ABSENCE of a call, which no unit test reaches by calling
    /// anything.
    #[test]
    fn a_shipping_build_cannot_enable_the_frame_tee() {
        // A REGULAR FILE in a scratch directory, not `/dev/null`: the tee refuses a
        // non-regular target (a device or pipe would let a write block the relay), so
        // `/dev/null` here made this assertion unreachable under `--features frame-tee` —
        // the call errored before the thing under test was ever evaluated.
        let dir = ScratchDir::new().expect("scratch dir");
        let path = dir.0.join("capture.jsonl");
        std::env::set_var(codex_broker::FRAME_TEE_ENV, &path);
        let tee = codex_broker::FrameTee::from_env().expect("from_env must not fail");
        std::env::remove_var(codex_broker::FRAME_TEE_ENV);
        if cfg!(feature = "frame-tee") {
            assert!(tee.is_on(), "a capture build must honour the variable");
        } else {
            assert!(
                !tee.is_on(),
                "the default build must ignore {} entirely — the env read is not compiled",
                codex_broker::FRAME_TEE_ENV
            );
        }
    }

    /// **The host's call is gated on THIS crate's feature, not only the broker's.**
    ///
    /// Cargo unifies features across a dependency graph, so another crate in a future
    /// build could enable `codex-broker/frame-tee` while CodeConnect's own feature stays
    /// off — and an ungated `FrameTee::from_env()` in the host would then read the
    /// environment even though nothing in this binary asked it to. The invariant has to be
    /// local to the binary a user runs, and what has to be proven is the absence of an
    /// unconditional call.
    #[test]
    fn the_hosts_frame_tee_call_is_gated_on_this_crates_feature() {
        let host = production_source(include_str!("codex_host.rs"));
        let at = host
            .find("FrameTee::from_env()")
            .expect("the host builds the tee");
        let before = &host[at.saturating_sub(400)..at];
        assert!(
            before.contains("cfg!(feature = \"frame-tee\")"),
            "the host's FrameTee::from_env call must sit behind CodeConnect's own \
             `frame-tee` feature, not only the dependency's: {before}"
        );
    }

    /// The launcher itself neither sets the variable nor offers a way to.
    #[test]
    fn the_shipping_launcher_cannot_enable_the_frame_tee() {
        // Every source file the `codeconnect` binary is built from.
        for (name, source) in [
            ("codex.rs", include_str!("codex.rs")),
            ("codex_host.rs", include_str!("codex_host.rs")),
            ("codex_coordinator.rs", include_str!("codex_coordinator.rs")),
            ("codex_launch.rs", include_str!("codex_launch.rs")),
            ("codex_custodian.rs", include_str!("codex_custodian.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            // Nothing may SET the variable. Reading it (the host's `from_env`) is the
            // one legitimate use, and it is a read.
            for setter in [
                &format!("set_var({:?}", codex_broker::FRAME_TEE_ENV) as &str,
                &format!(".env({:?}", codex_broker::FRAME_TEE_ENV),
                &format!("env(\"{}\"", codex_broker::FRAME_TEE_ENV),
            ] {
                assert!(
                    !source.contains(setter),
                    "{name} sets {} — the shipping launcher must never be able to turn \
                     the verbatim frame recorder on",
                    codex_broker::FRAME_TEE_ENV
                );
            }
            // And no config key or charter flag may reach it: those ARE operator-
            // writable, which is exactly what an env-only switch avoids.
            // Only three files may mention it at all, and each for one stated reason:
            // the host READS the variable to build the tee, the coordinator FORWARDS it
            // into the pane, and this file holds the test. Anywhere else is a fourth way
            // to reach a verbatim recorder, which is what this test exists to prevent.
            assert!(
                !source.contains("frame_tee")
                    || matches!(name, "codex_host.rs" | "codex_coordinator.rs" | "codex.rs"),
                "{name} references the frame tee; only the host (which reads the env), \
                 the coordinator (which forwards it) and this test may"
            );
        }
        // The charter grammar must not carry it either: a charter flag would make the
        // recorder reachable from any process that can spawn a coordinator.
        //
        // Scoped to `coordinator_charter`'s body — the one function that emits charter
        // flags — rather than the whole file, and the needles are BUILT rather than
        // written, because a literal here would appear in this test's own source and
        // match itself. (It did, on the first run.)
        let charter =
            production_fn("fn coordinator_charter(inputs: &CharterInputs<'_>) -> Vec<String> {");
        for needle in ["frame-tee", "capture", "tee"] {
            let flag = format!("--{needle}");
            assert!(
                !charter.contains(&flag),
                "coordinator_charter emits {flag}: a charter flag would make the \
                 verbatim frame recorder reachable from a spawned process"
            );
        }
        // And it is off unless the environment says otherwise.
        assert!(
            !codex_broker::FrameTee::off().is_on(),
            "the default must be off"
        );

        // The coordinator FORWARDS the variable into the tmux pane (otherwise the
        // instrument can never reach the host, since the pane gets an explicit `-e`
        // allowlist rather than the parent environment) — but it must only ever pass
        // through a value it already found, never invent one. Both halves are asserted:
        // the forward exists, and it is guarded by a read of the same variable.
        let coord = production_source(include_str!("codex_coordinator.rs"));
        assert!(
            coord.contains("codex_broker::FRAME_TEE_ENV"),
            "the coordinator must forward the frame-tee variable into the pane, or the \
             instrument cannot reach the host that builds the broker"
        );
        assert!(
            coord.contains("std::env::var(codex_broker::FRAME_TEE_ENV)"),
            "the forward must be guarded by a READ of the variable — a pass-through, \
             never a switch the coordinator can flip on its own"
        );
    }

    /// **A probe is bounded in both directions, proven deterministically.**
    ///
    /// No codex and no `CC_CODEX_LIVE`, because the dangerous shapes are invisible
    /// against the real binary: it answers and exits in the same breath, so an unbounded
    /// collection looks fine forever. The probe runs whatever is installed at the codex
    /// path, before anything has vetted it, so it must survive a binary
    /// that never closes its pipes and one that streams without end.
    ///
    /// The flood goes on **stdout** here, and the ceiling is what has to catch it: a
    /// prefix of a flood is not a shorter answer.
    #[test]
    fn a_probe_that_hangs_or_floods_is_refused_rather_than_waited_on() {
        use std::os::unix::fs::PermissionsExt;
        // Two budgets, by a single rule: **the budget may be short only in the arm
        // where the budget expiring is itself the mechanism under test.**
        //
        // That is the hang arm and nowhere else. A descendant holds the write end
        // open, so EOF never arrives and the deadline is the only thing that can end
        // the wait — which makes this arm load-immune, and makes paying the production
        // budget to watch a clock run out pure suite latency.
        const HANG_BUDGET: Duration = Duration::from_secs(2);
        // Every other arm gets a generous one, because in those the budget is not what
        // is being tested and must not become the binding constraint on what is:
        //
        //   * the well-behaved arm claims a good probe is READ TO COMPLETION;
        //   * the flood arm claims a flood is refused BY THE CEILING — the whole point
        //     of "a prefix of a flood is not a shorter answer" — which requires the
        //     flooder to actually exceed `PROBE_STDOUT_LIMIT` (8 MiB) first.
        //
        // Both failed under a saturated machine while sharing the short budget, in the
        // repo's own preflight and in a whole-crate parallel run: the good probe was
        // not scheduled in time, and the flooder was preempted before it reached the
        // ceiling, so the arm asserting the CEILING refusal got the pipe-close refusal
        // instead. Neither was a defect in the probe; both were the test measuring the
        // machine. Headroom is free here — a ceiling costs nothing when it is not
        // reached, and these two arms end on their own condition, not on the clock —
        // and ten seconds is still finite, so a genuinely unbounded read remains a
        // failure rather than a hang.
        const AMPLE_BUDGET: Duration = Duration::from_secs(10);
        let dir = ScratchDir::new().expect("scratch dir");
        let write = |name: &str, body: &str| {
            let p = dir.0.join(name);
            std::fs::write(&p, body).expect("write fake");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            p
        };

        // The well-behaved shape first, so a runner that refused everything could not
        // pass this test.
        let good = write("good", "#!/bin/sh\necho hello\n");
        assert_eq!(
            run_bounded(&good, &[], AMPLE_BUDGET).expect("a well-behaved probe is read normally"),
            b"hello\n"
        );

        // A descendant inherits the write end and never exits: EOF never arrives, so an
        // `output()` would block past any deadline above it.
        let forker = write("forker", "#!/bin/sh\necho hello\nsleep 600 &\nexit 0\n");
        let started = Instant::now();
        let why = run_bounded(&forker, &[], HANG_BUDGET).expect_err("a held pipe must be refused");
        assert!(
            why.to_string().contains("did not close its"),
            "expected a pipe-close refusal, got: {why:#}"
        );
        // The claim is BOUNDED versus UNBOUNDED — a 300x gap between the budget and
        // the sleeper's 600 seconds — so the ceiling is set far above the one and far
        // below the other. A tight margin here would be measuring the machine's load
        // under the name of the probe's behaviour, which is the mistake the arm above
        // was making.
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the probe must cost its budget, not the sleeper's lifetime (took {:?})",
            started.elapsed()
        );

        // A flood: valid-looking first line, then more bytes than the ceiling allows.
        let flooder = write(
            "flooder",
            "#!/bin/sh\necho hello\nyes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        );
        let why = run_bounded(&flooder, &[], AMPLE_BUDGET).expect_err("a flood must be refused");
        assert!(
            why.to_string().contains("wrote more than"),
            "expected an output-ceiling refusal, got: {why:#}"
        );
    }

    /// **A launch its TUI was quit out of ends quietly; any other failure is printed.**
    /// Ctrl+C in codex's `resume` picker exits with status 0 and prints nothing
    /// (measured on 0.155.1), so the launcher returns success with nothing said. A
    /// launch that failed without that mark still reports its reason.
    #[test]
    fn a_launch_quit_before_a_thread_ends_quietly() {
        use crate::codex_launch::{CleanupState, LaunchLock, NewLaunch};
        use protocol::proc_identity::{boot_identity, current_identity, monotonic_now_nanos};
        let ended = |uid: &str, quit: bool| {
            let lock = LaunchLock::acquire(uid).unwrap();
            crate::codex_launch::create_pending(
                &lock,
                NewLaunch {
                    launch_nonce: "n".into(),
                    uid: uid.into(),
                    session_name: "cc-1".into(),
                    coordinator: current_identity().unwrap(),
                    boot: boot_identity().unwrap(),
                    deadline_monotonic_nanos: monotonic_now_nanos().unwrap() + 60_000_000_000,
                    created_ms: 1,
                },
            )
            .unwrap();
            if quit {
                crate::codex_launch::note_codex_quit_before_thread(&lock, uid).unwrap();
            }
            crate::codex_launch::to_failed(&lock, uid, "the reason", CleanupState::Pending)
                .unwrap();
        };
        let patience = Duration::from_millis(200);
        let poll = Duration::from_millis(5);

        ended("quit-launch", true);
        wait_with_terminal("quit-launch", patience, poll).expect("a quit launch is quiet");

        ended("failed-launch", false);
        let err = wait_with_terminal("failed-launch", patience, poll)
            .expect_err("a failed launch is reported");
        assert_eq!(err.to_string(), "the reason");
    }

    // ----------------------------------------------------- reserved argv grammar

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn refuse(parts: &[&str]) -> CodexRefusal {
        validate_codex_argv(&argv(parts)).expect_err(&format!("expected {parts:?} to be refused"))
    }

    fn accept(parts: &[&str]) {
        validate_codex_argv(&argv(parts))
            .unwrap_or_else(|e| panic!("expected {parts:?} to be accepted, got: {e}"));
    }

    #[test]
    fn the_transport_flags_are_refused_every_form() {
        for parts in [
            &["--remote", "unix:///x"][..],
            &["--remote=unix:///x"][..],
            &["--remote-auth-token-env", "TOK"][..],
            &["--remote-auth-token-env=TOK"][..],
            &["--remote"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::OwnedFlag { .. }),
                "{parts:?} should be an owned-flag refusal"
            );
        }
    }

    /// **The keyboard is as trusted as native codex:** every sandbox, approval, profile,
    /// config and feature flag reaches the TUI, in every spelling, rewritten only into
    /// its attached form.
    ///
    /// **Mutation:** refuse any of these in `refuse_owned_flag` and this fails.
    #[test]
    fn the_keyboards_own_policy_flags_reach_the_tui() {
        for (parts, fenced) in [
            (
                &["--sandbox", "danger-full-access"][..],
                &["--sandbox=danger-full-access"][..],
            ),
            (&["-sread-only"][..], &["--sandbox=read-only"][..]),
            (
                &["-hs", "workspace-write"][..],
                &["--sandbox=workspace-write"][..],
            ),
            (&["--add-dir", "/repo"][..], &["--add-dir=/repo"][..]),
            (&["-p", "work"][..], &["--profile=work"][..]),
            (&["-a", "never"][..], &["--ask-for-approval=never"][..]),
            (&["--approve-for-me"][..], &["--approve-for-me"][..]),
            (&["--yolo"][..], &["--yolo"][..]),
            (
                &["--dangerously-bypass-approvals-and-sandbox"][..],
                &["--dangerously-bypass-approvals-and-sandbox"][..],
            ),
            (
                &["--dangerously-bypass-hook-trust"][..],
                &["--dangerously-bypass-hook-trust"][..],
            ),
            (
                &["-c", "approval_policy=never"][..],
                &["--config=approval_policy=never"][..],
            ),
            (
                &["-c", "features={hooks=false"][..],
                &["--config=features={hooks=false"][..],
            ),
            (
                &["-c", "features.hooks=true"][..],
                &["--config=features.hooks=true"][..],
            ),
            (&["--enable", "hooks"][..], &["--enable=hooks"][..]),
            (&["--disable", "hooks"][..], &["--disable=hooks"][..]),
        ] {
            assert_eq!(
                fence_positionals(&argv(parts)).unwrap_or_else(|e| panic!("{parts:?}: {e}")),
                argv(fenced),
                "{parts:?} must reach the TUI"
            );
        }
    }

    /// **`--cd` is the session folder, in every spelling codex accepts** (`--help` on
    /// 0.155.1: `-C, --cd <DIR>`), and the TUI is handed none of its own.
    ///
    /// Driven through the real argv parser, then [`session_folder`], then the charter
    /// the coordinator is spawned with — whose `--cwd` is the pane's start directory and,
    /// canonicalized by the coordinator, the directory the session is registered under.
    #[test]
    fn cd_is_the_session_folder_in_every_form() {
        let caller = ScratchDir::new().expect("scratch dir");
        let folder = ScratchDir::new().expect("scratch dir");
        let dir = folder.0.to_str().expect("utf-8").to_string();
        let attached_short = format!("-C{dir}");
        let equals_short = format!("-C={dir}");
        let equals_long = format!("--cd={dir}");
        for parts in [
            vec!["--cd", dir.as_str()],
            vec![equals_long.as_str()],
            vec!["-C", dir.as_str()],
            vec![attached_short.as_str()],
            vec![equals_short.as_str()],
            vec!["-hC", dir.as_str()],
            vec!["-m", "gpt-5", "--cd", dir.as_str(), "fix it"],
        ] {
            let scan = scan_codex_argv(&argv(&parts)).unwrap_or_else(|e| panic!("{parts:?}: {e}"));
            assert_eq!(scan.cd, vec![dir.clone()], "{parts:?}");
            assert!(
                !scan
                    .normalized
                    .iter()
                    .any(|t| t == "--cd" || t.starts_with("--cd=") || t.starts_with("-C")),
                "the TUI must not be handed a --cd of its own: {:?}",
                scan.normalized
            );
            let resolved = session_folder(&scan.cd, &caller.0).expect("an existing directory");
            assert_eq!(resolved, folder.0);
            let (charter, _) = charter_for(&resolved, &scan.normalized);
            assert_eq!(flag(&charter, "--cwd"), Some(dir.as_str()));
        }
        // The rest of the argv is untouched by taking `--cd` out of it.
        let scan = scan_codex_argv(&argv(&["-m", "gpt-5", "--cd", &dir, "fix it"])).unwrap();
        assert_eq!(scan.normalized, argv(&["--model=gpt-5", "--", "fix it"]));
    }

    /// Without `--cd` the session folder is the directory `codeconnect codex` was run
    /// from, exactly as before.
    #[test]
    fn without_cd_the_session_folder_is_the_callers_directory() {
        let caller = ScratchDir::new().expect("scratch dir");
        let scan = scan_codex_argv(&argv(&["-m", "gpt-5", "hi"])).unwrap();
        assert!(scan.cd.is_empty());
        assert_eq!(session_folder(&scan.cd, &caller.0).unwrap(), caller.0);
    }

    /// A relative `--cd` is read against the caller's directory, as native codex reads
    /// it — not against wherever the coordinator or the pane happens to run.
    #[test]
    fn a_relative_cd_is_resolved_against_the_callers_directory() {
        let caller = ScratchDir::new().expect("scratch dir");
        std::fs::create_dir(caller.0.join("sub")).unwrap();
        for parts in [&["--cd", "sub"][..], &["-Csub"][..], &["--cd=./sub"][..]] {
            let scan = scan_codex_argv(&argv(parts)).unwrap();
            let resolved = session_folder(&scan.cd, &caller.0).expect("sub exists");
            assert!(resolved.is_absolute(), "{resolved:?}");
            assert_eq!(
                std::fs::canonicalize(&resolved).unwrap(),
                std::fs::canonicalize(caller.0.join("sub")).unwrap(),
                "{parts:?}"
            );
        }
        let scan = scan_codex_argv(&argv(&["-C", ".."])).unwrap();
        assert_eq!(
            std::fs::canonicalize(session_folder(&scan.cd, &caller.0.join("sub")).unwrap())
                .unwrap(),
            std::fs::canonicalize(&caller.0).unwrap()
        );
    }

    /// A `--cd` that names no usable directory stops the launch with a reason, before
    /// anything is created.
    #[test]
    fn a_cd_that_names_no_directory_is_refused_with_a_reason() {
        let caller = ScratchDir::new().expect("scratch dir");
        std::fs::write(caller.0.join("a-file"), b"x").unwrap();
        for (parts, said) in [
            (&["--cd", "missing"][..], "No such file or directory"),
            (
                &["--cd", "/definitely/not/a/real/dir/xyzzy"][..],
                "No such file or directory",
            ),
            (&["-C", "a-file"][..], "is not a directory"),
            (&["--cd"][..], "needs a directory"),
            (&["--cd="][..], "needs a directory"),
            (&["--cd", "--search"][..], "needs a directory"),
            (&["--cd", ".", "-C", "."][..], "more than once"),
        ] {
            let scan = scan_codex_argv(&argv(parts)).unwrap_or_else(|e| panic!("{parts:?}: {e}"));
            let err = session_folder(&scan.cd, &caller.0)
                .expect_err(&format!("{parts:?} must be refused"))
                .to_string();
            assert!(err.contains(said), "{parts:?}: {err}");
        }
    }

    #[test]
    fn subcommand_names_and_aliases_are_refused_including_hidden() {
        for name in [
            "exec",
            "e",
            "review",
            "login",
            "logout",
            "mcp",
            "plugin",
            "mcp-server",
            "app-server",
            "remote-control",
            "app",
            "completion",
            "update",
            "doctor",
            "sandbox",
            "debug",
            "execpolicy",
            "apply",
            "a",
            "archive",
            "delete",
            "unarchive",
            "cloud",
            "cloud-tasks",
            "responses-api-proxy",
            "stdio-to-uds",
            "exec-server",
            "features",
            "help",
        ] {
            assert!(
                matches!(refuse(&[name]), CodexRefusal::Subcommand { .. }),
                "`codex {name}` should be refused as a subcommand"
            );
        }
    }

    /// **`resume` and `fork` are hosted like a new session.** The subcommand leads the
    /// emitted argv, its own options follow the flags-then-fence rule, and `--cd` is still
    /// the session folder. The host re-fences what the launcher emitted, so the emitted
    /// form must come back unchanged.
    #[test]
    fn resume_and_fork_are_hosted_with_their_own_options() {
        let fence = |parts: &[&str]| {
            let out = fence_positionals(&argv(parts)).unwrap_or_else(|e| panic!("{parts:?}: {e}"));
            assert_eq!(fence_positionals(&out).unwrap(), out, "{parts:?} re-fences");
            out
        };
        assert_eq!(fence(&["resume"]), argv(&["resume"]));
        assert_eq!(fence(&["resume", "--last"]), argv(&["resume", "--last"]));
        assert_eq!(
            fence(&["resume", "0199-id", "fix the test"]),
            argv(&["resume", "--", "0199-id", "fix the test"])
        );
        assert_eq!(fence(&["fork", "--last"]), argv(&["fork", "--last"]));
        assert_eq!(
            fence(&["fork", "--all", "0199-id"]),
            argv(&["fork", "--all", "--", "0199-id"])
        );
        assert_eq!(
            fence(&[
                "resume",
                "-m",
                "gpt-5",
                "--include-non-interactive",
                "0199-id"
            ]),
            argv(&[
                "resume",
                "--model=gpt-5",
                "--include-non-interactive",
                "--",
                "0199-id"
            ])
        );
        assert_eq!(
            fence(&["-m", "gpt-5", "--search", "resume", "--last"]),
            argv(&["resume", "--model=gpt-5", "--search", "--last"])
        );
        assert_eq!(
            fence(&["resume", "--last", "--", "--not-a-flag"]),
            argv(&["resume", "--last", "--", "--not-a-flag"])
        );
        let scan = scan_codex_argv(&argv(&["fork", "--cd", "/x", "--last"])).unwrap();
        assert_eq!(scan.cd, vec!["/x"]);
        assert_eq!(scan.normalized, argv(&["fork", "--last"]));
        let scan = scan_codex_argv(&argv(&["-C", "/y", "resume", "0199-id"])).unwrap();
        assert_eq!(scan.cd, vec!["/y"]);
        assert_eq!(scan.normalized, argv(&["resume", "--", "0199-id"]));
        // Past the caller's own `--` the word is prompt text, as before.
        assert_eq!(fence(&["--", "resume"]), argv(&["--", "resume"]));
    }

    /// Only `resume` and `fork`, and only as the first positional: every other subcommand
    /// is still refused, and so is a subcommand name after a prompt.
    #[test]
    fn only_resume_and_fork_are_hosted_and_only_as_the_first_positional() {
        for parts in [
            &["exec"][..],
            &["app-server"][..],
            &["-m", "gpt-5", "exec", "hi"][..],
            &["please", "resume"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::Subcommand { .. }),
                "{parts:?} must be refused as a subcommand"
            );
        }
        assert!(matches!(
            refuse(&["resume", "--remote", "unix:///x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
    }

    /// **After `resume` or `fork`, every word is a session id or a prompt**, never a
    /// subcommand: `resume` and `fork` have none of their own, so native codex reads
    /// `codex resume --last review` as a prompt and `codex fork 0199 e` as an id and a
    /// prompt. The fence keeps each behind `--`.
    #[test]
    fn words_after_resume_or_fork_are_ids_and_prompts() {
        for (parts, fenced) in [
            (&["resume", "review"][..], &["resume", "--", "review"][..]),
            (
                &["resume", "--last", "a"][..],
                &["resume", "--last", "--", "a"][..],
            ),
            (&["fork", "0199", "e"][..], &["fork", "--", "0199", "e"][..]),
            (&["resume", "help"][..], &["resume", "--", "help"][..]),
            (
                &["resume", "0199-id", "fork"][..],
                &["resume", "--", "0199-id", "fork"][..],
            ),
        ] {
            let out = fence_positionals(&argv(parts)).unwrap_or_else(|e| panic!("{parts:?}: {e}"));
            assert_eq!(out, argv(fenced), "{parts:?}");
            assert_eq!(fence_positionals(&out).unwrap(), out, "{parts:?} re-fences");
        }
    }

    /// `resume --help` and `fork -h` are codex's own help, run directly as before.
    #[test]
    fn help_for_resume_and_fork_runs_codex_itself() {
        for parts in [
            &["resume", "--help"][..],
            &["fork", "-h"][..],
            &["resume", "--last", "--help"][..],
            &["help", "fork"][..],
        ] {
            assert!(asks_help_or_version(&argv(parts)), "{parts:?}");
        }
        assert!(!asks_help_or_version(&argv(&["resume", "--last"])));
    }

    /// The three subcommands codex 0.153 added, which this grammar forwarded as prompt
    /// text until they were pinned.
    ///
    /// Kept as its own test rather than three more strings in the list above, because
    /// the regression it guards is specific and worth naming: `validate_codex_argv`
    /// returned `Ok(())` for each of these against a real 0.153 install, so `codeconnect
    /// codex agents` handed `agents` to codex, which dispatched its session browser
    /// against the shared local app-server daemon — outside the broker, which is the one
    /// place a hosted session is supposed to be observable and containable from.
    #[test]
    fn the_subcommands_codex_0153_added_are_refused() {
        for name in ["agents", "queue", "migrate-rollouts"] {
            assert!(
                matches!(refuse(&[name]), CodexRefusal::Subcommand { .. }),
                "`codex {name}` dispatches on 0.153 and must be refused, not forwarded \
                 as a prompt"
            );
        }
    }

    /// **`-i`/`--image` with no value reaches codex, which refuses it.** Native codex
    /// answers `a value is required` (rc 2); dropping the flag would launch a session
    /// the caller did not ask for.
    #[test]
    fn an_image_flag_with_no_value_is_forwarded_for_codex_to_refuse() {
        let fence = |parts: &[&str]| fence_positionals(&argv(parts)).expect("accepted");
        assert_eq!(fence(&["-i"]), argv(&["--image"]));
        assert_eq!(fence(&["--image"]), argv(&["--image"]));
        assert_eq!(fence(&["-i", "--", "hi"]), argv(&["--image", "--", "hi"]));
        assert_eq!(fence(&["-i", "--search"]), argv(&["--image", "--search"]));
        assert_eq!(fence(&["-i", "a.png"]), argv(&["--image=a.png"]));
    }

    /// **Help and version come first:** before the refusals, the binary checks and
    /// every launch step — native codex prints help for `--help resume` and for
    /// `--remote ws://x --help`, and runs from a script shim as readily as from the
    /// native binary.
    #[test]
    fn help_and_version_run_before_any_refusal_or_launch_check() {
        let start = production_fn("pub fn start(passthrough: &[String]) -> Result<()> {");
        let native = start
            .find("codex_itself(")
            .expect("start runs codex itself");
        for later in [
            "resolve_codex_bin(",
            "scan_codex_argv(",
            "clear_freezes_left_standing()",
            "probe_codex(",
            "launch(&resolved",
        ] {
            assert!(
                native < start.find(later).expect(later),
                "codex itself must run before {later}"
            );
        }
    }

    /// **A flag after the prompt still reaches codex as a flag.** Native `codex hi
    /// --search` searches; a fence in front of `hi` would turn `--search` into prompt
    /// text (0.155.1: `unrecognized subcommand '--search'`). So every flag is emitted
    /// first, in its own relative order, then `--`, then the positionals in order.
    #[test]
    fn flags_after_the_prompt_are_emitted_before_the_fence() {
        let fence = |parts: &[&str]| fence_positionals(&argv(parts)).expect("accepted");
        assert_eq!(fence(&["hi", "--search"]), argv(&["--search", "--", "hi"]));
        assert_eq!(
            fence(&["fix it", "-m", "gpt-5", "--search"]),
            argv(&["--model=gpt-5", "--search", "--", "fix it"])
        );
        assert_eq!(
            fence(&["one", "-c", "x=1", "two", "-c", "y=2"]),
            argv(&["--config=x=1", "--config=y=2", "--", "one", "two"])
        );
        assert_eq!(
            fence(&["hi", "--search", "--", "--not-a-flag"]),
            argv(&["--search", "--", "hi", "--not-a-flag"])
        );
    }

    /// **A flag this launcher has never seen reaches codex unchanged**, and never takes
    /// the next word as its value — so the next word is a positional behind the fence
    /// and cannot be dispatched. A value flag written with a space then gets codex's
    /// own "a value is required" error; the `=` form works.
    #[test]
    fn flags_the_launcher_does_not_know_pass_through_unchanged() {
        let fence = |parts: &[&str]| fence_positionals(&argv(parts)).expect("accepted");
        assert_eq!(fence(&["--worktree"]), argv(&["--worktree"]));
        assert_eq!(
            fence(&["--worktree", "hi"]),
            argv(&["--worktree", "--", "hi"])
        );
        assert_eq!(
            fence(&["--future-flag=v", "hi"]),
            argv(&["--future-flag=v", "--", "hi"])
        );
        assert_eq!(fence(&["-Zq", "hi"]), argv(&["-Zq", "--", "hi"]));
        // Known value flags keep their arity.
        assert_eq!(
            fence(&["--worktree", "-m", "gpt-5", "hi"]),
            argv(&["--worktree", "--model=gpt-5", "--", "hi"])
        );
        // The word after an unknown flag is a positional, so a subcommand name there is
        // still refused rather than handed to codex.
        assert!(matches!(
            refuse(&["--future-flag", "features"]),
            CodexRefusal::Subcommand { .. }
        ));
        // The transport stays owned in every spelling.
        assert!(matches!(
            refuse(&["--worktree", "--remote=x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
    }

    /// **`--help` and `--version` are codex's own**, alone or inside a short cluster,
    /// anywhere before `--`, whatever else the argv holds: the launch runs codex with the
    /// caller's own arguments and creates no session.
    #[test]
    fn help_and_version_run_codex_itself_with_the_callers_arguments() {
        for parts in [
            &["--help"][..],
            &["-h"][..],
            &["--version"][..],
            &["-V"][..],
            &["-hC", "x"][..],
            &["-Vm", "gpt-5"][..],
            &["hi", "--help"][..],
            &["--worktree", "-h"][..],
            // Native codex answers these with help, so no refusal may come first.
            &["--help", "resume"][..],
            &["--version", "resume"][..],
            &["--help", "--remote", "ws://x"][..],
            &["--remote", "ws://x", "--help"][..],
            // `codex help` and `codex help <sub>` print help natively.
            &["help"][..],
            &["help", "resume"][..],
            &["-m", "gpt-5", "help"][..],
        ] {
            assert!(
                asks_help_or_version(&argv(parts)),
                "{parts:?} asks codex itself"
            );
        }
        for parts in [
            &["--", "--help"][..],
            &["-m", "gpt-5"][..],
            &["-mh"][..],
            &["hi"][..],
            // Only the FIRST positional, and never a flag's value or past `--`.
            &["hi", "help"][..],
            &["-m", "help"][..],
            &["-i", "a.png", "help"][..],
            &["--", "help"][..],
        ] {
            assert!(
                !asks_help_or_version(&argv(parts)),
                "{parts:?} is an ordinary launch"
            );
        }
        let codex = Path::new("/opt/codex/bin/codex");
        let raw = argv(&["-hC", "x"]);
        let cmd = codex_itself(codex, &raw);
        assert_eq!(cmd.get_program(), codex.as_os_str());
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            raw.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
            "the caller's arguments, verbatim — the `h` in `-hC` included"
        );
    }

    /// **Help runs the codex a launch would choose**: the first native executable in the
    /// launch's own candidate order ([`codex_candidates`]), with none of the launch's
    /// hash or freeze. A script shim earlier in the list is skipped exactly as a launch
    /// skips it; only when no native binary exists at all does help fall back to the
    /// first candidate, so a shim-only machine still gets codex's help. Nothing at all is
    /// a clear error.
    #[test]
    fn help_runs_the_codex_a_launch_would_choose() {
        let root = tempdir();
        let shim = root.join("codex-shim");
        std::fs::write(&shim, b"#!/bin/sh\necho help\n").unwrap();
        make_executable(&shim);
        assert!(
            matches!(inspect_candidate(&shim), CandidateIdentity::Wrapper),
            "the premise: a launch would refuse this file"
        );
        let native = root.join("codex-native");
        std::fs::write(&native, [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0]).unwrap();
        make_executable(&native);

        // `codex_bin` names the shim; the native binary comes later in the same order.
        let config = Config {
            codex_bin: Some(shim.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let candidates = codex_candidates(&config, Some(native.clone()), &root, None);
        assert_eq!(
            first_codex(candidates).expect("a codex"),
            native.canonicalize().unwrap(),
            "help must run the native binary a launch would run, not the shim"
        );

        // Shim-only: help still runs it.
        let found = first_codex(vec![root.join("absent"), shim.clone()]).expect("a codex");
        assert_eq!(found, shim.canonicalize().unwrap());
        let none = first_codex(vec![root.join("absent")]).expect_err("nothing to run");
        assert!(format!("{none:#}").contains("could not find the codex binary"));
        cleanup(&root);
    }

    /// The fence goes in front of the first positional, and every value-taking flag is
    /// rewritten into attached form so no bare token can be a flag's value.
    #[test]
    fn the_fence_lands_before_the_first_positional_only() {
        let fence = |parts: &[&str]| -> Vec<String> {
            fence_positionals(&argv(parts)).expect("accepted argv fences")
        };
        // A bare prompt gets the boundary in front of it.
        assert_eq!(fence(&["hello"]), argv(&["--", "hello"]));
        // A spaced value is ATTACHED to its flag, so the value is no longer a bare token
        // that any arity model could disagree about.
        assert_eq!(
            fence(&["-m", "gpt-5", "hello"]),
            argv(&["--model=gpt-5", "--", "hello"])
        );
        assert_eq!(
            fence(&["--search", "--model=gpt-5", "hi"]),
            argv(&["--search", "--model=gpt-5", "--", "hi"])
        );
        // Short attached forms are canonicalised to the long attached form; a `--config`
        // value carrying its own `=` survives intact (measured to parse).
        assert_eq!(fence(&["-ca=b"]), argv(&["--config=a=b"]));
        assert_eq!(fence(&["--config", "a=b"]), argv(&["--config=a=b"]));
        // The greedy sweep becomes one attached occurrence PER value — measured
        // byte-identical to the spaced form on a real 0.153 turn.
        assert_eq!(
            fence(&["-i", "a.png", "b.png", "hi"]),
            argv(&["--image=a.png", "--image=b.png", "--image=hi"]),
            "the sweep is rewritten value by value, so no bare token is left over"
        );
        // The ATTACHED form takes exactly one value, so what follows really is a
        // positional — and the token after THAT is the subcommand slot codex would
        // dispatch from. This is the case the fence has to catch.
        assert_eq!(
            fence(&["--image=a.png", "hi"]),
            argv(&["--image=a.png", "--", "hi"])
        );
        // Nothing positional, nothing to fence.
        assert_eq!(fence(&["--search"]), argv(&["--search"]));
        assert_eq!(fence(&[]), argv(&[]));
        // A boundary the caller already supplied is not doubled.
        assert_eq!(fence(&["--", "hello"]), argv(&["--", "hello"]));
        assert_eq!(fence(&["--search", "--"]), argv(&["--search", "--"]));
        // Only the FIRST positional is fenced; a second is already behind the boundary.
        assert_eq!(
            fence(&["one", "two"]),
            argv(&["--", "one", "two"]),
            "one boundary, at the front of the positionals"
        );
        // A bare `-` is codex's stdin sentinel — a positional, so it is fenced.
        assert_eq!(fence(&["-"]), argv(&["--", "-"]));

        // **The invariant the fence now rests on**, asserted over every arm above: in the
        // emitted argv, no token before the `--` is bare. Each is either a `--flag` or a
        // `--flag=value`, so nothing there can be a flag's value under any arity model,
        // and everything after the `--` is positional by construction.
        for parts in [
            &["-m", "gpt-5", "hello"][..],
            &["-i", "a.png", "b.png", "hi"][..],
            &["--config", "a=b", "--local-provider", "ollama", "prompt"][..],
            &["--search", "-ca=b", "-i", "x.png", "the prompt"][..],
        ] {
            let out = fence(parts);
            let head = out.split(|t| t == "--").next().unwrap_or_default();
            for token in head {
                assert!(
                    token.starts_with('-'),
                    "{parts:?} emitted a bare token before the fence: {out:?}"
                );
            }
        }
    }

    /// A refused argv never reaches the fence: the grammar's answer comes first, so a
    /// refusal message is not replaced by a silent prompt.
    #[test]
    fn the_fence_refuses_what_the_grammar_refuses() {
        for parts in [
            &["exec"][..],
            &["--remote=x"][..],
            &["--unknown-flag", "features"][..],
        ] {
            assert!(
                fence_positionals(&argv(parts)).is_err(),
                "{parts:?} must still be refused"
            );
        }
    }

    /// **The escape class, closed structurally — proven against a binary that DISPATCHES
    /// a token no list in this repository knows.**
    ///
    /// The test carries its own mutation: the same shim is run with the fence and
    /// without it. Unfenced, the never-seen alias reaches dispatch; fenced, it cannot.
    /// No real codex is needed, and that is the point — the property under test is
    /// about a FUTURE binary, so it must be provable against one that behaves like the
    /// future rather than like today's install.
    ///
    /// The shim mimics what was MEASURED of clap: a bare first token is matched against
    /// the subcommand set (aliases included, whether or not any `--help` or completion
    /// output lists them), and everything after a `--` is positional.
    #[test]
    fn a_never_seen_hidden_alias_cannot_dispatch_behind_the_fence() {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDir::new().expect("scratch dir");
        let shim = dir.0.join("codex-shim");
        // Dispatches `zzz-future-alias` — a token ROOT_SUBCOMMANDS does not know.
        std::fs::write(
            &shim,
            "#!/bin/sh\nfor a in \"$@\"; do\n  case \"$a\" in\n    --) echo PROMPT; exit 0 ;;\n \
             zzz-future-alias) echo DISPATCHED; exit 0 ;;\n  esac\ndone\necho PROMPT\n",
        )
        .expect("write shim");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        let run = |args: &[String]| -> String {
            let out = Command::new(&shim).args(args).output().expect("shim runs");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let passthrough = argv(&["zzz-future-alias"]);

        // The grammar cannot refuse what it has never heard of — this is the premise, and
        // asserting it stops the test passing for the wrong reason.
        assert!(
            validate_codex_argv(&passthrough).is_ok(),
            "the refusal table must NOT know this token, or the fence is not what is \
             being tested"
        );
        // WITHOUT the fence — the mutation — it dispatches.
        assert_eq!(
            run(&passthrough),
            "DISPATCHED",
            "the unfenced argv must reach dispatch, or the shim proves nothing"
        );
        // WITH it, it cannot.
        let fenced = fence_positionals(&passthrough).expect("accepted");
        assert_eq!(fenced, argv(&["--", "zzz-future-alias"]));
        assert_eq!(run(&fenced), "PROMPT");
    }

    /// **A future codex that changed a flag's ARITY still cannot be made to dispatch.**
    ///
    /// The case, staged exactly: a binary whose `-i` consumes ONE value, given
    /// `-i a.png features`. Our walk still believes `-i` is greedy — nothing checks
    /// option arity — so under the old passthrough it swept both tokens, saw no
    /// positional, inserted no fence, and handed the binary a bare `features` to
    /// dispatch.
    ///
    /// The mutation is carried inside the test: the same shim is run with the emitted argv
    /// and with the raw one. Raw dispatches; emitted cannot, because every value is glued
    /// to its flag and there is no bare token left for any arity to disagree about.
    #[test]
    fn a_future_arity_change_cannot_expose_a_positional() {
        use std::os::unix::fs::PermissionsExt;
        let dir = ScratchDir::new().expect("scratch dir");
        let shim = dir.0.join("codex-shim");
        // `-i` takes exactly ONE value; anything after it is a subcommand slot. `features`
        // is a real codex subcommand, so this is the shape that would dispatch.
        std::fs::write(
            &shim,
            "#!/bin/sh\nskip=0\nfor a in \"$@\"; do\n  if [ $skip = 1 ]; then skip=0; \
             continue; fi\n  case \"$a\" in\n    --) echo PROMPT; exit 0 ;;\n    -i) skip=1 \
             ;;\n    --image=*) ;;\n    features) echo DISPATCHED; exit 0 ;;\n  esac\ndone\n\
             echo PROMPT\n",
        )
        .expect("write shim");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        let run = |args: &[String]| -> String {
            let out = Command::new(&shim).args(args).output().expect("shim runs");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let passthrough = argv(&["-i", "a.png", "features"]);

        // The premise: our own grammar accepts this, because it reads `features` as the
        // second image of a greedy sweep rather than as a token to refuse.
        assert!(
            validate_codex_argv(&passthrough).is_ok(),
            "the grammar must NOT refuse this, or the rewrite is not what is being tested"
        );
        // THE MUTATION: raw argv against the single-value shim → it dispatches.
        assert_eq!(
            run(&passthrough),
            "DISPATCHED",
            "the raw argv must reach dispatch, or the shim proves nothing"
        );
        // The emitted argv: every value attached, so nothing is left bare to dispatch.
        let emitted = fence_positionals(&passthrough).expect("accepted");
        assert_eq!(
            emitted,
            argv(&["--image=a.png", "--image=features"]),
            "each swept value becomes its own attached occurrence"
        );
        assert_eq!(run(&emitted), "PROMPT");
    }

    /// The table is a set: a duplicate would make its length lie about its contents.
    #[test]
    fn the_refusal_table_has_no_duplicates() {
        let unique: std::collections::BTreeSet<&str> = ROOT_SUBCOMMANDS.into_iter().collect();
        assert_eq!(unique.len(), ROOT_SUBCOMMANDS.len());
    }

    #[test]
    fn a_subcommand_after_a_consumed_flag_value_is_refused() {
        assert!(matches!(
            refuse(&["-m", "gpt", "exec"]),
            CodexRefusal::Subcommand { .. }
        ));
        assert!(matches!(
            refuse(&["--model=gpt", "review"]),
            CodexRefusal::Subcommand { .. }
        ));
    }

    #[test]
    fn a_subcommand_after_a_leading_prompt_is_refused() {
        // `codex please resume` dispatches Resume in 0.147 — the leading prompt
        // does not shield the subcommand.
        assert!(matches!(
            refuse(&["please", "resume"]),
            CodexRefusal::Subcommand { .. }
        ));
        assert!(matches!(
            refuse(&["please", "resume", "the", "task"]),
            CodexRefusal::Subcommand { .. }
        ));
        assert!(matches!(
            refuse(&["run", "fork"]),
            CodexRefusal::Subcommand { .. }
        ));
    }

    #[test]
    fn a_flag_cannot_smuggle_a_subcommand() {
        // `codex --search exec` dispatches Exec; a flag in front must not shield it.
        assert!(matches!(
            refuse(&["--search", "exec"]),
            CodexRefusal::Subcommand { .. }
        ));
        // An unknown flag takes no value, so the word after it is judged as a
        // positional and refused.
        assert!(matches!(
            refuse(&["--not-a-real-flag", "exec"]),
            CodexRefusal::Subcommand { .. }
        ));
    }

    #[test]
    fn a_single_prompt_token_or_multiword_prompt_is_not_a_subcommand() {
        // One token that is not a subcommand name is a prompt.
        accept(&["please"]);
        accept(&["what is the capital of france"]);
        accept(&["cloud-task"]);
        accept(&["tasks"]);
        accept(&[]);
        // After `--`, even a bare subcommand word is prompt content.
        accept(&["--", "resume"]);
        accept(&["--", "fork", "the", "thing"]);
    }

    #[test]
    fn spaced_image_is_greedy_attached_image_is_single_value() {
        // Spaced `-i` is greedy (grounded: `codex -i a b c` launches with three
        // images), so trailing paths are values, not subcommands.
        accept(&["-i", "a", "resume"]);
        accept(&["--image", "a", "b", "fork"]);
        // But a real flag terminates the greedy consumption.
        assert!(matches!(
            refuse(&["-i", "a", "--remote", "x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
        // Attached `--image=a` / `-ia` takes exactly one value (grounded:
        // `codex --image=a b c` parses `b` as prompt and errors on `c` as a
        // subcommand). So a following prompt forwards, and a following subcommand
        // name is refused exactly as codex would dispatch it.
        accept(&["--image=a", "b"]);
        accept(&["-ia", "b"]);
        assert!(matches!(
            refuse(&["--image=a", "exec"]),
            CodexRefusal::Subcommand { .. }
        ));
        assert!(matches!(
            refuse(&["-ia", "review"]),
            CodexRefusal::Subcommand { .. }
        ));
    }

    #[test]
    fn a_value_option_does_not_swallow_a_flag_shaped_follower() {
        // Grounded: `codex --model --yolo` is a missing-value error, and `--yolo`
        // is parsed as a flag — so a forbidden follower must reach our refusal,
        // never ride through as the option's value.
        assert!(matches!(
            refuse(&["--model", "--remote=x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
        assert!(matches!(
            refuse(&["--config", "--remote", "x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
        assert_eq!(
            fence_positionals(&argv(&["-m", "--nope"])).unwrap(),
            argv(&["--model", "--nope"])
        );
        // A benign flag follower is still just the next flag; the value-option had
        // no value (codex would error), but nothing forbidden rode through.
        accept(&["--model", "--search"]);
    }

    #[test]
    fn short_clusters_are_fully_expanded() {
        // A bool short in front of a value short must not discard the suffix: the
        // `-c model=o3` inside `-hcmodel=o3` keeps its value.
        assert_eq!(
            fence_positionals(&argv(&["-hcmodel=o3"])).unwrap(),
            argv(&["--config=model=o3"])
        );
        // A cluster of only bool shorts forwards.
        accept(&["-hV"]);
        accept(&["-h"]);
        // A value short attached after a bool short still consumes its own value.
        assert_eq!(scan_codex_argv(&argv(&["-hC."])).unwrap().cd, vec!["."]);
    }

    #[test]
    fn repeated_flags_are_forwarded_each_time() {
        assert_eq!(
            fence_positionals(&argv(&["-c", "model=o3", "-c", "approval_policy=never"])).unwrap(),
            argv(&["--config=model=o3", "--config=approval_policy=never"])
        );
        assert!(matches!(
            refuse(&["-c", "model=o3", "--remote", "x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
    }

    #[test]
    fn the_known_interactive_flags_are_forwarded() {
        // Every interactive flag from `codex --help` (plus a prompt) is forwarded.
        accept(&["-m", "gpt-5"]);
        accept(&["--model", "gpt-5"]);
        accept(&["-i", "shot.png"]);
        accept(&["--local-provider", "ollama"]);
        accept(&["--oss"]);
        accept(&["--search"]);
        accept(&["--no-alt-screen"]);
        accept(&["--strict-config"]);
        accept(&["-h"]);
        accept(&["-V"]);
        // A realistic benign invocation: a prompt plus a known flag.
        accept(&["-m", "gpt-5", "fix the flaky test"]);
    }

    #[test]
    fn unknown_flags_are_forwarded_as_written() {
        // An unknown long flag, an unknown short flag, and a short cluster with an
        // unknown character are all forwarded unchanged, taking no value.
        for (parts, fenced) in [
            (&["--not-a-real-flag"][..], &["--not-a-real-flag"][..]),
            (
                &["--future-approval-flag", "x"][..],
                &["--future-approval-flag", "--", "x"][..],
            ),
            (&["-Z"][..], &["-Z"][..]),
            (&["-hq"][..], &["-hq"][..]),
        ] {
            assert_eq!(
                fence_positionals(&argv(parts)).unwrap(),
                argv(fenced),
                "{parts:?}"
            );
        }
        // A known short with an attached value is still a value, not "unknown":
        // `-mZq` is `-m` with value `Zq`.
        accept(&["-mZq"]);
    }

    #[test]
    fn the_boundary_stops_all_scanning() {
        accept(&["--", "--remote", "unix:///x"]);
        accept(&["--", "-C", "/x"]);
        accept(&["--", "exec"]);
        // Past the boundary `-C` is prompt text, not the session folder.
        assert!(scan_codex_argv(&argv(&["--", "-C", "/x"]))
            .unwrap()
            .cd
            .is_empty());
        assert!(matches!(
            refuse(&["--remote", "x", "--", "prompt"]),
            CodexRefusal::OwnedFlag { .. }
        ));
    }

    #[test]
    fn refusal_messages_name_what_and_why() {
        assert!(refuse(&["--remote", "x"])
            .to_string()
            .contains("--remote` is set by CodeConnect (the app-server transport)"));
        assert!(refuse(&["exec"]).to_string().contains("exec"));
    }

    // ------------------------------------------------------------- test helpers

    fn on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
            .unwrap_or(false)
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codeconnect-codex-test-{}-{}",
            std::process::id(),
            unique()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // A process-wide counter in the high bits guarantees two concurrent tests
        // never collide on the same temp dir name — a nanosecond timestamp alone
        // can repeat under load, letting one test's cleanup delete another's file.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        (seq << 40) ^ nanos
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A private temp directory, removed when it drops.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Result<ScratchDir> {
            Ok(ScratchDir(tempdir()))
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            cleanup(&self.0);
        }
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    /// "Is this a native executable?" as the old boolean helper answered it, now
    /// read off the one-pass inspection so the tests exercise the real path.
    fn is_native(path: &Path) -> bool {
        matches!(inspect_candidate(path), CandidateIdentity::Native { .. })
    }
}
