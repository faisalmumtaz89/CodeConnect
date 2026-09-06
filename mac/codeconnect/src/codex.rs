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
//! provably the bytes every later `execve` runs (A7.1 — see [`ResolvedCodex`] and
//! [`verify_codex_identity`]), pins the
//! resolved binary to a compiled-in tested-version set, and parses the user's
//! argv against the reserved grammar that keeps CodeConnect the sole owner of the
//! launch's transport, working directory, profile, approval policy and — per
//! A10 — sandbox policy. The `-c`
//! ownership check parses values against the **same** TOML grammar the codex
//! binary embeds (toml 0.9.11 / TOML 1.1), so a form codex applies cannot
//! parse-fail here and be forwarded.
//!
//! **The command is live.** [`start`] resolves, version-pins, argv-validates and
//! preflights the daemon — surfacing every one of those failures honestly and
//! before anything exists — and then launches: it mints the session identity,
//! spawns the D7 coordinator, and waits on the durable launch record. See
//! [`start`] for the boundary this launch path accepts, and [`launch`] for the
//! shape it shares with `codeconnect claude`.
//!
//! **Grounded against the installed codex-cli 0.147.0.** Every acceptance and
//! refusal below was probed against the live binary (flag arities and attached
//! short forms via invalid-enum sentinels; the subcommand set and its hidden
//! entries/aliases from clap's own completion output; the ownership config keys
//! parsed the way codex parses them — the value as TOML). The parser is a real
//! parser, not a denylist scan: it normalizes spaced, `=`-joined and attached
//! short forms (`-C.`, `-aon-request`, `-capproval_policy=x`, `-pfoo`), knows
//! each flag's arity (so it can tell a flag's value from the next token, and a
//! bare prompt from a subcommand), honours the `--` boundary, and refuses
//! subcommand **names and aliases** anywhere codex would dispatch one — because
//! only interactive-TUI invocation is supported. Everything it does not refuse is
//! forwarded verbatim.

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
/// version-stamped release directory (A1). Resolution canonicalises **once** and
/// fails closed if it cannot, and this single path is what gets version-checked,
/// recorded as launch evidence and exec'd by the app-server and TUI alike, so a
/// `standalone/current` flip cannot make the recorded, checked and executed
/// binaries disagree (CODEX-PLAN.md launch coordination; "all spawned Codex
/// processes use the same resolved executable").
///
/// # Why the digest exists (A7.1)
///
/// **A canonical path is a name, not an executable.** Canonicalising pins which
/// name is used; it says nothing about which bytes that name reaches at any later
/// instant. Between resolution and the last `execve` this launch performs, the
/// pathname is opened by the kernel three separate times — `codex --version`, the
/// app-server spawn, the TUI spawn — in two different processes, and every one of
/// those opens is free to see a different file. An install, an `npm` replacement or
/// a `standalone/current` flip landing in that window would let the bytes that ran
/// differ from the bytes that were magic-checked and version-pinned, which is
/// exactly the pre-ungate hole A7 names.
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
/// Resolves the binary, pins its version, validates the argv against the
/// reserved grammar, preflights the daemon — every one of those can fail with its
/// own honest error, and all of them fail *before anything exists* — and then
/// launches ([`launch`]).
///
/// # THE ACCEPTED BOUNDARY (A22, and the single place it is stated)
///
/// Everything above this function was built to hold a launch closed against a
/// binary that is not the one it inspected, a daemon that could never be told
/// about the session, a passthrough that moves approval or sandbox ownership, and
/// a rolled-back peer that would file the run as the wrong agent. One class of
/// attacker is deliberately **out of scope**, and the ungate is the moment to say
/// so once, plainly, rather than to leave it implied by a dozen local caveats:
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
/// **Separate accepted post-ungate residuals — not instances of the boundary above,
/// because neither of them needs a hostile actor at all:**
///
///   * **A11.2**, a benign within-boot pid/pgid-reuse TOCTOU: the custodian's group
///     kill can land on an unrelated same-uid process that inherited a recycled
///     group id, with nobody attacking anything (`codex_custodian::group_warrant`
///     carries the measurement). Closing it needs env-nonce provenance via
///     `KERN_PROCARGS2`.
///   * **F7**, a benign package update: the native `--codex`
///     dispatcher is pinned faithfully and completely, but the subordinates it
///     selects for `--version`, `app-server` and the TUI are not, so an ordinary
///     update can change what actually runs while the pinned dispatcher's own bytes
///     are unchanged and every gate here passes. Closing it needs a package-layout
///     specification, which is a scoping decision about what a supported install is;
///     see [`is_native_magic`].
pub fn start(passthrough: &[String]) -> Result<()> {
    let config = Config::load();

    // Binary first, exactly as the Claude path resolves its binary first: a
    // missing or untested executable must surface before anything else. The
    // canonicalised path is what we version-check and exec.
    let resolved = resolve_codex_bin(&config)?;
    // The version is RECORDED, not gated: it names what ran, in the launch evidence and
    // in a refusal. What decides whether this build may be hosted is the guarded-surface
    // gate below — see [`ensure_guarded_surface`] for why a version string was the wrong
    // question to ask.
    // ONE freeze, five probes: version, root command surface, both schema bundles, and
    // the effective value of the one feature this launch pins off.
    // See `probe_codex` for why they share a freeze rather than taking one each.
    let scratch = ScratchDir::new()?;
    let probe = probe_codex(&resolved, &scratch.0)?;
    let version = parse_codex_version(&String::from_utf8_lossy(&probe.version_out))
        .ok_or_else(|| anyhow!("could not read a version from `codex --version`"))?;
    // NOT YET RECORDED. The gate returns the digest of the surface it admitted so that
    // carrying it into the launch record beside `codex_sha256` is a pure addition rather
    // than a change of shape — but that plumbing (charter flag → coordinator → record)
    // is its own sub-issue and is not built yet. Bound and dropped deliberately, rather
    // than the gate pretending it has nowhere to report from.
    let _admitted_surface = ensure_guarded_surface(&probe)
        .with_context(|| format!("checking codex {version} against CodeConnect's grounding"))?;

    // **What the launch SETS is the argv; what this checks is the outcome.** The one
    // feature CodeConnect pins off is pinned with a `-c`, and codex ranks a managed
    // configuration layer above `-c` — so the pin is airtight for an operator and
    // beatable by an administrator. Read back from the same frozen bytes, under the
    // same override, in the same `CODEX_HOME` the spawns will use.
    refuse_unless_pinned_feature_is_off(&String::from_utf8_lossy(&probe.features))?;

    // Reserved grammar. A refused flag or subcommand surfaces here, naming what
    // was refused and why, before anything is created.
    validate_codex_argv(passthrough).map_err(|refusal| anyhow!("{refusal}"))?;

    // Daemon preflight, before anything is created. Ordering is load-bearing and
    // is what `new-old-new-real.sh` step 7(g) drives: a rolled-back daemon must
    // refuse the launch with no uid minted, no tmux name taken, no coordinator
    // spawned and no record written.
    refuse_unless_hostable(crate::daemon::agent_support(
        &protocol::agent::AgentKind::Codex,
    ))?;

    launch(&resolved, passthrough)
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

/// The launch policy CodeConnect owns for every Codex session.
///
/// These four are the broker's [`codex_broker::fingerprint::LaunchFingerprint`]
/// dimensions. The charter defaults none of them and neither does the host — an
/// omitted dimension is a refused launch, not an assumed one — so the launcher is
/// where the values are decided, and this is the only place they are written down.
///
/// **Each is the value a live session was actually proven on, not a preference.**
/// `approval_policy` is `on-request` because `untrusted` was MEASURED to kill real
/// sessions about two seconds in: the real 0.147 TUI's own `thread/start` asserts
/// `approvalPolicy: "on-request"`, and a launch fingerprint of `untrusted` makes the
/// broker refuse the TUI's opening request (CODEX-PLAN A14; both live harnesses moved
/// to `on-request` for the same reason). The remaining three are the set every live
/// gate in this repo has run on — `live_codex_coordinator`, `live_codex_host` and
/// `codex_link_live` all launch with exactly these — so the fingerprint a user gets
/// is the fingerprint the gates prove.
///
/// They are constants rather than configuration on purpose. The whole point of the
/// reserved grammar above is that CodeConnect owns approval and sandbox policy for
/// the session; a config key that moved them would hand back through the front door
/// exactly what [`validate_codex_argv`] refuses at the command line.
///
/// **What a user notices:** because `--sandbox read-only` is now genuinely set on
/// the TUI (`codex_host`'s TUI spawn) rather than merely claimed here, a session in
/// a project the user had marked `trust_level = "trusted"` will ask for approval on
/// writes and commands where an unpinned codex would not have — that is this
/// pre-Phase-3 policy working, not a regression.
const LAUNCH_APPROVAL_POLICY: &str = "on-request";
/// See [`LAUNCH_APPROVAL_POLICY`].
const LAUNCH_APPROVALS_REVIEWER: &str = "user";
/// See [`LAUNCH_APPROVAL_POLICY`].
const LAUNCH_SANDBOX: &str = "read-only";
/// See [`LAUNCH_APPROVAL_POLICY`]. Rendered as the charter's `true`/`false`.
const LAUNCH_HOOKS_ENABLED: bool = true;

/// The one codex feature CodeConnect pins **off**, and the only launch dimension
/// carried as a config override rather than as a flag.
///
/// `features.request_permissions_tool` exposes a model-callable tool whose approval
/// arrives as `item/permissions/requestApproval`. What that request asks for is a
/// permission *profile* — a filesystem and network shape — rather than a yes/no
/// about one action, so there is no set of buttons a phone could honestly be
/// offered for it, and the grant is bound to the terminal: a session driven from
/// the phone would park on a question only the Mac can close. Every other approval
/// CodeConnect claims is answerable from the phone, and this is the one that would
/// not be, so it is removed rather than half-supported.
///
/// **Measured on the installed codex, which is why the pin is on an argv rather
/// than a sentence.** `codex features list` reports the feature `under development`
/// and `false`; an operator `config.toml` carrying `[features]
/// request_permissions_tool = true` flips it to `true`; and a
/// `-c features.request_permissions_tool=false` on the same invocation puts it back
/// to `false`. Since a shipping launch hands both codex processes the operator's own
/// `CODEX_HOME` (see [`codex_home`]), the config value is the operator's to set —
/// so the absence is made structural at each `execve` instead of being assumed.
///
/// [`path_is_owned`] owns the same key from the other direction, so a caller's own
/// `-c` (or `--enable`/`--disable`) for it is refused at the terminal with a reason
/// rather than silently losing to the pin.
///
/// # What the pin does not reach: a managed configuration layer
///
/// The `-c` above is a command-line override, and codex ranks a MANAGED (MDM /
/// administrator-pushed) configuration layer ABOVE command-line overrides. An
/// administrator who pushes `[features] request_permissions_tool = true` through
/// that layer therefore wins against this pin, and nothing here reads the
/// EFFECTIVE configuration back to notice: the argv is asserted, the outcome is
/// not.
///
/// **Recorded rather than defended against, and the shape of the exposure is why.**
/// The actor is above the user's own uid — outside the same-uid boundary every
/// other guard here is drawn at, where an actor who can push a managed profile can
/// already replace the binary this launches. And the consequence is degraded but
/// honest: the family that becomes producible is bound to the terminal, so the
/// question lands on the Mac's screen and the phone is offered nothing to actuate.
/// A session driven from the phone parks on it; nothing is granted from the phone
/// that would not have been.
///
/// The hardening, when it is worth its cost, is an EFFECTIVE-CONFIG POSTCHECK at
/// launch rather than a second override: read the feature back out of the codex the
/// launch is about to use — the harness already reads `codex features list`, which
/// is the seam — and refuse the launch with a reason when it does not answer
/// `false`. That turns a pin on the input into a check on the result, which is the
/// only form that can survive a layer ranked above the input.
pub(crate) const PINNED_OFF_FEATURE: &str = "request_permissions_tool";

/// The `-c` value that pins [`PINNED_OFF_FEATURE`] off.
///
/// Built from the constant rather than written out, so the key the launch writes
/// and the key the grammar owns cannot drift apart.
pub(crate) fn pinned_off_feature_override() -> String {
    format!("features.{PINNED_OFF_FEATURE}=false")
}

/// Ask the codex about to be used what [`PINNED_OFF_FEATURE`] will actually be, with
/// the launch's own override applied and in the `CODEX_HOME` the launch will name.
///
/// **This is a read, and it starts nothing.** `features list` prints the resolved
/// registry and exits; it opens no session, contacts no account and spends no quota
/// (measured). It is the seam A28 named for turning "we set the argv" into "we know
/// the answer".
///
/// The home is passed explicitly rather than inherited, for the same reason the
/// charter names it rather than defaulting it: the value the app-server runs under
/// is a decision, and a probe that read a different one would be answering about
/// somebody else's configuration.
fn read_effective_features(bin: &Path, codex_home: &Path) -> Result<Vec<u8>> {
    run_bounded_in_home(
        bin,
        &["features", "list", "-c", &pinned_off_feature_override()],
        codex_home,
        PROBE_BUDGET,
    )
}

/// Refuse the launch unless the feature the launch pins off is **effectively** off.
///
/// **The pin is a `-c`, and codex ranks a managed configuration layer above `-c`.**
/// So an administrator who pushes `[features] request_permissions_tool = true`
/// through that layer beats the value every spawn carries, and CodeConnect would be
/// asserting the argv while the app-server ran with the feature on — handing the
/// model a tool that asks for a permission profile only the terminal can grant, in a
/// product whose whole proposition is that the phone answers. The argv is what we
/// set; this is what we got.
///
/// **An unreadable answer refuses, exactly as [`verify_codex_identity`] does.** "I
/// could not check" and "it is off" are different answers and only one of them
/// licenses a spawn. A listing that never names the feature is unreadable in that
/// sense too: on a build where the key has moved or been renamed, the guarded-surface
/// gate has re-grounding to demand anyway, so nothing is lost by saying so here.
///
/// **This is a PREFLIGHT, and the difference is worth stating rather than leaving to
/// be inferred.** What it reads is the effective value at probe time — the same
/// frozen bytes, the same `-c`, the same `CODEX_HOME` the spawns will name — from a
/// `codex features list` run before anything is created. It is not a reading of the
/// running app-server's own configuration: nothing here asks the process that will
/// host the conversation what it ended up with. The gap between them is a managed
/// layer that changes underneath after the probe and before the spawn, which is
/// seconds wide and is not the case this exists for.
///
/// **That a managed/MDM layer outranks `-c` is UNVERIFIED here.** Staging
/// `/etc/codex/managed_config.toml` is a system path and needs root, so the premise
/// rests on corroboration from the binary's own strings; the refusing half of this
/// function is driven by a stub rather than by that layer. What IS measured is the
/// half that matters for not breaking launches: `-c` beats an ordinary
/// `config.toml`, so an operator's `true` there is handled by the pin and does not
/// reach this refusal.
fn refuse_unless_pinned_feature_is_off(listing: &str) -> Result<()> {
    let stated = listing.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(PINNED_OFF_FEATURE)).then(|| fields.last().unwrap_or("").to_string())
    });
    match stated.as_deref() {
        Some("false") => Ok(()),
        Some("true") => bail!(
            "refusing to launch: this codex reports `{PINNED_OFF_FEATURE}` as ON even with the \
             override this launch applies. CodeConnect answers approvals from the phone, and \
             that feature gives the model a tool that asks for a permission profile only the \
             terminal can grant — so a session under it would stall on a question nothing in \
             this product can answer. An override ranked above the command line is what does \
             this: a managed or MDM configuration layer. Clearing it there, or launching codex \
             directly, are the two ways on."
        ),
        other => bail!(
            "refusing to launch: this codex did not say whether `{PINNED_OFF_FEATURE}` is on \
             or off — `codex features list` answered {}. The launch pins that feature off and \
             checks the result, and an answer it cannot read is not an answer of `off`.",
            match other {
                None => "without naming it at all".to_string(),
                Some(value) => format!("`{value}`, which is neither `true` nor `false`"),
            }
        ),
    }
}

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

/// Mint the session identity, spawn the coordinator, wait on the record, attach.
///
/// **This is `codeconnect claude`'s launch with one process substituted, and it is
/// deliberately not a second design.** The Claude path (`main.rs::start_agent`)
/// reads the cwd, takes the lowest free `cc-N` off the tmux server, mints a uid,
/// creates the tmux session, spawns a detached supervisor and `exec`s into
/// `tmux attach-session` — printing nothing, because the alternate screen erases
/// anything it could print. Every one of those steps is here, in that order, using
/// the same functions.
///
/// The one structural difference is D7's, and it is the reason the Codex path exists
/// at all: **the launcher does not create the tmux session.** The coordinator
/// performs every forward launch mutation itself, including `tmux new-session`, so
/// that launcher death at any point changes nothing; the launcher spawns it *before
/// tmux exists* and then only waits on the durable record
/// ([`crate::codex_coordinator::wait_on_record`]). So where the Claude path attaches
/// on the strength of `tmux new-session -d` having returned, this one attaches on the
/// strength of a fsynced `Ready`.
///
/// Which makes the two ends identical again: on success the session name is a live
/// tmux session on the shared server, and `exec_attach` puts the user in it. `ls`,
/// `attach`, and closing the tab all behave the same for both agents because by that
/// point there is nothing agent-shaped left in the picture.
fn launch(resolved: &ResolvedCodex, passthrough: &[String]) -> Result<()> {
    // The cwd is passed RAW, and that is not an oversight. The chain has exactly
    // one canonicalization, in the coordinator
    // (`codex_coordinator::canonical_launch_cwd`), because the canonical spelling
    // is the anchor the broker's fingerprint, the creation-response check and every
    // turn's workspace check are compared against by plain string equality. A
    // second `canonicalize` here would be a second answer about the same directory,
    // taken at a different instant, with nothing requiring the two to agree — the
    // same failure the digest is carried rather than re-derived to avoid.
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let cwd = cwd.to_string_lossy().to_string();

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

    let charter = coordinator_charter(&CharterInputs {
        uid: &session_uid,
        launch_nonce: &crate::codex_launch::mint_nonce(),
        custodian_nonce: &crate::codex_launch::mint_nonce(),
        session_name: &session_name,
        cwd: &cwd,
        codex: resolved,
        codex_home: &codex_home(),
        tui_args: passthrough,
    });
    spawn_coordinator(&session_name, &session_uid, &charter)?;

    match crate::codex_coordinator::wait_on_record(&session_uid, LAUNCH_PATIENCE, RECORD_POLL) {
        // The record is `Ready` and proven durable. The coordinator has become the
        // session's supervisor, the pane is real, and the session is on the shared
        // tmux server under `session_name` — so this is the Claude path's own last
        // line, reached the same way and printing the same nothing.
        crate::codex_coordinator::LaunchWait::Ready => {
            crate::tmux::exec_attach(&session_name)?;
            unreachable!("exec replaces the process")
        }
        // **The record's reason, verbatim.** It is already sanitized to one printable
        // bounded line by `wait_on_record`, and it is the only account of the failure
        // that survives the runtime dir being swept — so it is reported as the record
        // holds it rather than wrapped in a second story about it.
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
/// verdict, and that [`read_codex_version`] then froze and re-verified across
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
        // A7.1: the identity of the bytes, beside the name of the file. Carried from
        // resolution — see this function's doc.
        "--codex-sha256".into(),
        inputs.codex.sha256.clone(),
        "--codex-home".into(),
        inputs.codex_home.to_string_lossy().into_owned(),
        "--approval-policy".into(),
        LAUNCH_APPROVAL_POLICY.into(),
        "--approvals-reviewer".into(),
        LAUNCH_APPROVALS_REVIEWER.into(),
        "--sandbox".into(),
        LAUNCH_SANDBOX.into(),
        "--hooks-enabled".into(),
        LAUNCH_HOOKS_ENABLED.to_string(),
    ];
    // `--tmux-socket` is deliberately absent: the coordinator's default is
    // `protocol::TMUX_SOCKET_NAME`, the one server `codeconnect claude`, `ls` and
    // `attach` all address. Naming it here would be a second copy of that constant
    // with nothing keeping the two equal.
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
/// a log survives the tab. The extra is that this process is about to `exec` into a
/// tmux client and the coordinator will then *continue as the session's supervisor*
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
/// **A7.1.** Canonicalisation pins a pathname; it does not pin a file. So each
/// candidate is read exactly once ([`inspect_candidate`]) and that single read
/// yields both the Mach-O verdict and the SHA-256 that every later exec site
/// verifies against — see [`ResolvedCodex`] for why the name alone is not enough
/// and what the pin does and does not claim.
fn resolve_codex_bin(config: &Config) -> Result<ResolvedCodex> {
    let candidates = codex_candidates(
        config,
        std::env::var_os(CODEX_BIN_ENV).map(PathBuf::from),
        &protocol::home_dir(),
        protocol::tmux::search_path("codex"),
    );

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
        // an npm replacement between our version-check and the later app-server /
        // TUI spawns would swap the real CLI while our canonical path is
        // unchanged, defeating both the version pin and the parser grammar. Skip
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
            // A7.1 forbids running bytes no digest is attributable to.
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
    /// reach by the name we would `execve` is not an identity — and A7.1 forbids
    /// running what cannot be pinned. Not `Unreadable`: the second case reads
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
/// # UNGATE BLOCKER: a compiled dispatcher is pinned, and what it dispatches to is not
///
/// A magic number says "a native executable"; it does not say "standalone Codex". A
/// *compiled native dispatcher* — a small Mach-O binary that picks a real codex at
/// runtime and spawns it — passes this check, and passes the version pin too if it
/// forwards `--version`.
///
/// Stated exactly, because a residual that is not exact is not a residual, it is a
/// hope. A launch execs the resolved `--codex` three times:
///
///   1. `codex --version`, in the launcher ([`read_codex_version`]);
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
/// execs CodeConnect never sees — is not inspected, not magic-checked, not
/// version-pinned and not hashed, and nothing requires the three to be the same
/// binary as each other. So a dispatcher can answer `--version` from a pinned build
/// and then run something else entirely under the app-server and the TUI, which are
/// the two execs the whole command gate exists to contain: the app-server is what
/// executes the model's tool calls and the TUI is what the operator types into.
///
/// The identity chain is therefore closed up to the file we exec and **open past any
/// process that re-dispatches**. The plan (A7, same paragraph as the hash-pin) calls
/// this out as needing "its own pre-ungate enforcement (e.g. verifying the standalone
/// package layout)". This is not the hash-pin's residual and it is not narrowed by
/// it; it is a separate hole, and the only reason it is not gaping today is that the
/// dispatcher shape anyone actually ships — the npm `codex.js` shebang shim — is
/// caught here as a [`CandidateIdentity::Wrapper`], while a *compiled* one is caught
/// nowhere.
///
/// **This is F7, and it is an accepted residual rather than a blocker** — the owner
/// ruled that at the 2e-7d ungate, and [`start`] records it as one of the two residuals that are
/// separate from the A22 boundary. It stays open because no layout verifier can be
/// invented here: "e.g." in the plan is an example rather than a specification, and
/// picking one unilaterally would silently narrow which installs CodeConnect
/// supports — a scoping decision, not an implementation detail. Closing it needs
/// that ruling first.
fn is_native_magic(magic: [u8; 4]) -> bool {
    matches!(
        u32::from_be_bytes(magic),
        // Mach-O 32/64-bit, big- and little-endian (arm64 native is 0xCFFAEDFE).
        0xFEED_FACE | 0xFEED_FACF | 0xCEFA_EDFE | 0xCFFA_EDFE
        // Universal ("fat") binaries, 32- and 64-bit.
        | 0xCAFE_BABE | 0xBEBA_FECA | 0xCAFE_BABF | 0xBFBA_FECA
    )
}

// ------------------------------------------------------- executable identity (A7.1)

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
/// **Two different resolvers read that one string.** A7.1's guard opens it
/// ([`verify_codex_identity`] → `File::open`) and the spawns execute it
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
/// **This is the A7.1 guard.** It stands immediately before each point where these
/// bytes are about to become a running process, so that what runs is what was
/// inspected and version-pinned rather than merely whatever was reachable through
/// the same name. Three sites — [`read_codex_version`]'s `--version` exec and the
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

// -------------------------------------------------------------- version pinning

/// Run `codex --version` and return the parsed version string — with the exec
/// **bracketed by the resolved binary's identity** (A7.1).
///
/// It takes the whole [`ResolvedCodex`], not a bare path, because the exec it
/// performs is itself one of the opens A7 names: `Command::new(path)` makes the
/// kernel open that pathname afresh, and whatever it finds there is what reports a
/// version. Resolution already hashed the file; this freezes and re-verifies the
/// digest *before* the exec and holds the freeze across it, so the version that gets
/// pinned is a statement about the exact bytes this launch will carry rather than
/// about whatever answered `--version`.
///
/// **This one is prevention now, like the host's spawns — the asymmetry is gone.**
/// It used to be detection-only: the exec ran pre-gate with only the resolution hash
/// behind it, so a replacement landing in front of it *ran* as `codex --version` and
/// the launch was merely refused afterwards. The freeze removes that: the bytes are
/// pinned immutable and verified before the exec, and an installer or update cannot
/// change them while the child runs, so the version reported here is the pinned
/// build's. A swap that landed before the freeze is caught by the freeze's own vnode
/// check (the name no longer reaches the frozen handle) and refuses with nothing run.
/// What the freeze does not exclude is a hostile same-uid peer — out of scope, and
/// unreachable by any macOS mechanism; see [`protocol::hash::FrozenExecutable`].
///
/// That accounting also rests on resolution no longer being a half-second opening: a
/// rename landing inside [`inspect_candidate`]'s read once minted a pin over a file
/// the pathname had already stopped naming. The vnode arm on that read makes the pin
/// a statement about this file rather than whichever the name reached first (see
/// [`ResolvedCodex`]).
///
/// The freeze is cleared only after the child has exited, and the output is
/// interpreted after that — a swapped binary cannot have run, so any failure to read
/// a version is a real one and not a swap misreported as a parse error.
///
/// What remains uncatchable: a replacement *reverted* before the check, which no
/// verify-by-content scheme can see, and the post-clear demand-paging residual — both
/// stated on [`ResolvedCodex`] and [`protocol::hash::FrozenExecutable`].
/// **Superseded on the launch path by [`probe_codex`]**, which reads the version as one
/// of five answers under a single held freeze. Kept because it is the narrowest possible
/// statement of the freeze-then-exec discipline and its tests pin exactly that: a binary
/// swapped or moved between resolution and exec is refused. `probe_codex` inherits the
/// discipline; these tests are what prove it is the right one.
#[cfg(test)]
fn read_codex_version(resolved: &ResolvedCodex) -> Result<String> {
    let bin = resolved.path.as_path();
    // Freeze + verify BEFORE the exec, and hold the freeze across it. This exec used
    // to be the one A7.1 site that could only *detect* a swap after the fact — it ran
    // pre-gate, so a replacement landing in front of it ran as `codex --version`
    // before anything checked. Now the bytes are pinned immutable and verified first,
    // and stay frozen while the child runs, so the version pinned below is reported
    // by the pinned bytes and no unverified binary is reachable here at all.
    // Nothing is recorded: this is a test-only narrowing of the discipline, run in
    // a process with no launch record to write into.
    let frozen = verify_codex_identity(
        bin,
        &resolved.sha256,
        "before `codex --version`",
        protocol::hash::LockHold::UntilReleased,
        |_| Ok(()),
    )?;
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", bin.display()))?;
    // The child has exited (`output` waited for it); the frozen bytes are the bytes
    // that ran, so the freeze can be cleared before the output is interpreted.
    drop(frozen);
    if !output.status.success() {
        bail!("{} --version exited with {}", bin.display(), output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_codex_version(&text)
        .ok_or_else(|| anyhow!("could not read a version from `codex --version`: {text:?}"))
}

/// Pull the version out of `codex --version` output.
///
/// Grounded on the installed shape `codex-cli 0.147.0`. The output must be
/// **exactly one** non-empty line, in one of the two measured forms —
/// `codex-cli <version>` or a bare `<version>` — and anything with an extra line,
/// or extra/ambiguous tokens on the line, is rejected rather than guessed. So
/// neither `codex-cli 0.148.0 compatibility 0.147.0` (extra tokens) nor
/// `codex-cli 0.147.0\ncompatibility 0.148.0` (extra line) can be misread as a
/// pinned version. Pure, so the shape is pinned by tests rather than by the live
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

// ------------------------------------------------------------- the guarded-surface gate

/// The wall-clock budget for one launch probe.
///
/// Generous against the measurement — the four probes that were timed take ~150 ms on
/// the real binary, and the fifth is one more exec of the same already-frozen,
/// already-hashed bytes — because the number is not a performance target, it is the
/// point past which the gate stops waiting for an answer it is never going to get.
const PROBE_BUDGET: Duration = Duration::from_secs(30);

/// How long to spend reaping a probe after its process group has been SIGKILLed.
const PROBE_REAP_BUDGET: Duration = Duration::from_secs(2);

/// The stdout ceiling for one probe. `completion bash` is the chatty one, MEASURED at
/// ~230 KiB on 0.153; the schema commands write to disk and print nothing.
const PROBE_STDOUT_LIMIT: u64 = 8 << 20;

/// The ceiling on one generated bundle document. `ClientRequest.json` is MEASURED at
/// ~1.2 MiB on both binaries.
const PROBE_FILE_LIMIT: u64 = 64 << 20;

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
/// The read is moved off the gate's thread because a descendant that inherited the write
/// end can defer EOF forever; the caller bounds the *wait* with `recv_timeout`.
///
/// `LIMIT + 1` is asked for so that hitting the ceiling is detectable. `Read::take(N)`
/// reports EOF once N bytes are consumed, so a plain `take(LIMIT)` hands back a prefix
/// indistinguishable from a complete answer — and a prefix of a flood is exactly the
/// vacuous pass a gate must not take. A read error is reported for the same reason: a
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
/// Split out so a caller that already holds a verified freeze can run several probes
/// under **one** of them — see [`probe_codex`].
///
/// **Bounded on purpose.** The binary being probed is whatever is installed at the codex
/// path: the gate's job is to decide whether to host it, so it cannot assume it behaves.
/// A plain `output()` gives an unknown executable an unbounded hold on the launch *and*
/// on the freeze — it can never exit, never close its pipes (a forked descendant inherits
/// the write ends, so EOF never arrives), or stream until the gate runs out of memory.
/// Every wait here is against a deadline and every path attempts to kill the probe's
/// whole process group, pipes collected FIRST so the kill always happens while the
/// leader's pgid is provably not recycled.
fn run_under_freeze(bin: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run_bounded(bin, args, PROBE_BUDGET)
}

/// [`run_under_freeze`]'s body, with the budget a parameter so the boundedness itself can
/// be tested without the test paying the production budget to observe it.
fn run_bounded(bin: &Path, args: &[&str], budget: Duration) -> Result<Vec<u8>> {
    run_bounded_inner(bin, args, None, budget)
}

/// [`run_bounded`] with the `CODEX_HOME` the launch will use named explicitly, for
/// the one probe whose answer depends on which configuration is being resolved.
fn run_bounded_in_home(
    bin: &Path,
    args: &[&str],
    codex_home: &Path,
    budget: Duration,
) -> Result<Vec<u8>> {
    run_bounded_inner(bin, args, Some(codex_home), budget)
}

fn run_bounded_inner(
    bin: &Path,
    args: &[&str],
    codex_home: Option<&Path>,
    budget: Duration,
) -> Result<Vec<u8>> {
    use std::os::unix::process::CommandExt;
    let what = format!("{} {}", bin.display(), args.join(" "));
    let mut command = Command::new(bin);
    if let Some(home) = codex_home {
        command.env("CODEX_HOME", home);
    }
    let child = command
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

/// Read one file the probe just generated, under a ceiling.
///
/// Same reasoning as [`spawn_pipe_reader`]: the writer is the binary under examination,
/// and a gate that will happily read whatever it produced has handed it the launch's
/// memory. `LIMIT + 1` so that hitting the ceiling is a refusal rather than a truncation.
fn read_generated(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    // **Opened so it cannot block, and validated so it cannot lie.** The writer of this
    // file is the binary under examination — the gate has not yet decided whether to host
    // it — and it can write `ClientRequest.json` as a FIFO instead of a file. A plain
    // `File::open` on a FIFO with no writer blocks forever, outside every probe deadline,
    // *while the executable freeze is still held*: the launch would hang and the freeze
    // would never clear. `O_NONBLOCK` makes that open fail instead, `O_NOFOLLOW` stops the
    // final component being a symlink to somewhere else, and the `fstat` below is
    // authoritative about the handle actually held.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| {
            format!(
                "reading {} — a bundle codex was asked to write",
                path.display()
            )
        })?;
    if !file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .file_type()
        .is_file()
    {
        bail!(
            "{} is not a regular file. `generate-json-schema` writes ordinary files; a pipe \
             or device here would block the launch with the executable freeze held.",
            path.display()
        );
    }
    let mut buf = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, PROBE_FILE_LIMIT + 1),
        &mut buf,
    )
    .with_context(|| format!("reading {}", path.display()))?;
    if buf.len() as u64 > PROBE_FILE_LIMIT {
        bail!(
            "{} is larger than {PROBE_FILE_LIMIT} bytes; the measured bundle is ~1.2 MiB, so \
             this is not one",
            path.display()
        );
    }
    Ok(buf)
}

/// Everything the launch gate asks the installed codex about itself, read under a
/// **single** held freeze.
///
/// # One freeze, five execs — stronger and faster than one freeze each
///
/// The launcher asks the binary five questions before it will host it: its version, its
/// root command surface (`completion bash`), its two app-server schema bundles, and the
/// effective value of the one feature this launch pins off. Each of the first four used
/// to take its own freeze-and-verify; the fifth is affordable only because it rides this
/// one.
///
/// **Stronger:** separate freezes leave gaps between them. A version read under freeze A
/// and a schema read under freeze D are two statements about two moments, and nothing
/// said the bytes were the same in between — which is exactly the reasoning
/// [`verify_codex_identity`] exists to refuse. One freeze held across all five makes them
/// one statement about one set of bytes, which is what the gate's conclusion actually
/// claims.
///
/// **Faster, and that mattered:** `freeze_and_hash` reads and digests the whole 210 MB
/// executable, MEASURED at 7.5 s in a debug build. Four of them put ~30 s in front of
/// every launch — enough that the live end-to-end gate timed out waiting for a launch
/// record, which is how this was found. One freeze puts the gate back at the cost of the
/// single `--version` hash the launcher already paid.
fn probe_codex(resolved: &ResolvedCodex, scratch: &Path) -> Result<CodexProbe> {
    use codex_broker::guarded_surface as gs;
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
        "before the launch probes",
        // **The lock is held for the whole probe, and it is standing in for the record
        // this site cannot write.** There is no uid yet, so nothing a custodian scans
        // will ever name this freeze — and a custodian's scan-and-clear takes this same
        // lock, so while it is held the clear cannot happen at all. Released at the
        // empty record, the interval that follows (the 210 MB hash plus the five execs
        // below) was a freeze every custodian on the machine was free to undo, and a
        // peer's stale record was enough to make one do it.
        protocol::hash::LockHold::UntilReleased,
        // Nothing durable to write: there is no uid yet, which is the whole reason
        // this site needs the handler. The freeze arms the release itself, from
        // inside `arm_then_freeze` and BEFORE the flag goes on, so the interval this
        // callback used to be responsible for no longer exists.
        |_| Ok(()),
    )?;

    let probe = (|| -> Result<CodexProbe> {
        let version_out = run_under_freeze(bin, &["--version"])?;
        let completion = run_under_freeze(bin, &["completion", "bash"])?;
        // **The fifth question under the same freeze, and that is what makes it
        // affordable.** The postcheck A28 named was deferred for the launch latency a
        // separate freeze-and-hash of a 210 MB binary would cost; asked here it costs
        // one more exec of bytes already frozen and already hashed, and it is one
        // statement about one set of bytes along with the other four.
        let features = read_effective_features(bin, &codex_home())?;
        let mut bundles = Vec::new();
        for bundle in codex_broker::guarded_surface::BUNDLES {
            let out = scratch.join(bundle);
            let out_arg = out.to_string_lossy().into_owned();
            let mut args = vec!["app-server", "generate-json-schema", "--out", &out_arg];
            if bundle == "experimental" {
                args.push("--experimental");
            }
            run_under_freeze(bin, &args)?;
            // READ HERE, under the freeze, not by the caller afterwards. The gate's
            // conclusion is about the bytes that will be exec'd; a path handed back to a
            // caller that opens it after the freeze is released is a second read of a
            // second moment, and between the two the scratch tree could be replaced with
            // a projection of the baseline. Owning the bytes closes that window with the
            // freeze that made the answer trustworthy still held.
            let client_request = read_generated(&out.join("ClientRequest.json"))?;
            // The result documents are named by the request document, so it is parsed
            // here — inside the freeze — to learn which ones to read. Only the ones a
            // guarded method reaches (≈18 of the bundle's 250–370 files).
            let parsed = gs::parse_schema(&String::from_utf8_lossy(&client_request))
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("parsing the {bundle} bundle's ClientRequest.json"))?;
            let mut results = Vec::new();
            for name in gs::guarded_result_types(&parsed).map_err(|e| anyhow!("{e}"))? {
                let file = format!("{name}.json");
                let path = ["v2", "v1"]
                    .iter()
                    .map(|d| out.join(d).join(&file))
                    .find(|p| p.is_file())
                    .ok_or_else(|| {
                        anyhow!(
                            "the {bundle} bundle has no {file}, which a guarded method's \
                             params type says is its result"
                        )
                    })?;
                results.push((name, read_generated(&path)?));
            }
            bundles.push(ProbedBundle {
                bundle,
                client_request,
                client_notification: read_generated(&out.join("ClientNotification.json"))?,
                server_request: read_generated(&out.join("ServerRequest.json"))?,
                results,
            });
        }
        Ok(CodexProbe {
            version_out,
            completion,
            bundles,
            features,
        })
    })();

    // Cleared only after every child has exited, so all five answers are attributable to
    // the frozen bytes. A failure clears it too, with nothing having been admitted.
    drop(frozen);
    probe
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
/// past the guarded-surface gate, and the flag would then be cleared by the ordinary
/// path rather than by the handler. This is the smallest process that has the
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

/// What [`probe_codex`] read, all of it from one frozen set of bytes.
struct CodexProbe {
    version_out: Vec<u8>,
    completion: Vec<u8>,
    bundles: Vec<ProbedBundle>,
    /// `codex features list` with the launch's own override applied, in the
    /// `CODEX_HOME` the launch will name — the EFFECTIVE value of the one feature
    /// this launch pins off. See [`refuse_unless_pinned_feature_is_off`].
    features: Vec<u8>,
}

/// One schema bundle as the probe read it, **as bytes rather than paths** — see
/// [`probe_codex`] for why the read happens inside the freeze.
///
/// Four documents, because the broker's contract with codex is not only "what may the
/// client send". `initialized` is an admitted NOTIFICATION; the server sends REQUESTS the
/// broker classifies and now answers; and the broker forwards the RESULTS of every method
/// it admits. A gate that read only `ClientRequest.json` would admit a build whose
/// `thread/read` result grew a field, or whose `item/tool/call` changed shape.
struct ProbedBundle {
    bundle: &'static str,
    client_request: Vec<u8>,
    client_notification: Vec<u8>,
    server_request: Vec<u8>,
    /// `(response type name, its bytes)`, for the guarded methods only.
    results: Vec<(String, Vec<u8>)>,
}

/// A scratch directory for one gate run, removed when the guard drops.
///
/// `generate-json-schema` writes a tree rather than to stdout (measured: `--out <DIR>`
/// is required, there is no stdout form), so the gate needs somewhere to put ~3 MB
/// twice. Cleaning up on drop means a refusal — which returns early from several arms —
/// does not leave the tree behind.
///
/// # Created exclusively, and private
///
/// `mkdir(2)` with `O_EXCL` semantics and mode `0700`, not `create_dir_all`. The two
/// differ exactly where it matters: `create_dir_all` succeeds against a directory (or a
/// symlink to one) that somebody else put there first, so the gate would generate its
/// bundles into, and read them back out of, a tree it does not own. `DirBuilder::create`
/// fails `EEXIST` on anything already at the path, symlinks included, and the mode is set
/// at creation rather than afterwards so there is no window in which the tree is
/// world-writable.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new() -> Result<ScratchDir> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!(
            "codeconnect-codex-schema-{}-{}",
            std::process::id(),
            protocol::hash::sha256_hex(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos().to_string())
                    .unwrap_or_default()
                    .as_bytes()
            )
            .get(..16)
            .unwrap_or("scratch")
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| {
                format!(
                    "creating {} for the codex schema gate — it must not already exist",
                    path.display()
                )
            })?;
        Ok(ScratchDir(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Refuse unless the installed codex's **guarded surface** matches the vendored 0.147
/// reference, and return the digest of the surface that was admitted.
///
/// # Why this replaced the version-string pin
///
/// The pin ([`CODEX_PINNED_VERSIONS`], now recorded rather than gated) asked "is this
/// build called 0.147.0?". That is a proxy for the real question, and a bad one in both
/// directions: it refuses every weekly codex release whose wire shape did not move at
/// all, and the pressure it creates is to bump the number — the one edit that re-proves
/// nothing. This asks the real question instead: *is the part of codex that CodeConnect
/// guards the same part it was grounded against?* A build whose guarded surface is
/// identical is admitted whatever it calls itself; a build whose guarded surface moved
/// is refused **naming what moved**, which is the work item for the re-grounding.
///
/// # Both surfaces, because one of them is not in the schema
///
/// * The **wire** surface — the guarded methods' parameter shapes, from
///   `codex app-server generate-json-schema`. This is what the broker's allowlist and
///   fingerprint defend.
/// * The **argv** surface — the root subcommand and flag set, from
///   `codex completion bash`. This is what [`validate_codex_argv`] defends, and
///   `generate-json-schema` says nothing about it.
///
/// Gating only the wire surface would have been a **regression**, measured rather than
/// argued: codex 0.153 adds `agents`, `queue` and `migrate-rollouts`, none of which
/// [`is_subcommand`] knows, so [`validate_codex_argv`] classifies them as prompt text
/// and forwards them — and `codex agents` reaches the shared local app-server daemon
/// while `codex queue` injects a message into another session, both around the broker.
/// A wire-only gate would have admitted 0.153 with that door open.
///
/// # Fail direction
///
/// Closed in every arm: a failed exec, a non-UTF-8 or unparseable schema, a bundle that
/// will not project, a method that vanished, a subcommand that appeared. The one verdict
/// that admits is "no differences at all".
fn ensure_guarded_surface(probe: &CodexProbe) -> Result<String> {
    use codex_broker::guarded_surface as gs;

    let mut changes: Vec<String> = Vec::new();
    let mut admitted: Vec<(&str, String)> = Vec::new();

    // --- the argv surface ---------------------------------------------------------
    let completion = String::from_utf8(probe.completion.clone())
        .context("`codex completion bash` did not emit UTF-8; refusing to guess at its surface")?;
    let installed_argv = gs::project_argv(&completion)
        .map_err(|e| anyhow!("{e}"))
        .context("reading the installed codex's root command surface")?;
    changes.extend(
        gs::diff_argv(&gs::admissible_argv(&installed_argv), &installed_argv)
            .iter()
            .map(ToString::to_string),
    );
    admitted.push((gs::ARGV_BUNDLE, serde_json::to_string(&installed_argv)?));

    // --- the wire surface ---------------------------------------------------------
    for probed in &probe.bundles {
        let bundle = probed.bundle;
        // The SAME duplicate-member discipline the c2s classifier applies to a frame: a
        // document whose meaning depends on which duplicate a parser keeps has no single
        // meaning, and the gate's verdict is an equality of parsed values.
        let parse = |raw: &[u8], what: &str| -> Result<serde_json::Value> {
            gs::parse_schema(&String::from_utf8_lossy(raw))
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("reading the {bundle} bundle's {what}"))
        };
        let mut results = std::collections::BTreeMap::new();
        for (name, raw) in &probed.results {
            results.insert(name.clone(), parse(raw, name)?);
        }
        let installed = gs::project_bundle(&gs::BundleDocs {
            client_request: &parse(&probed.client_request, "ClientRequest.json")?,
            client_notification: &parse(&probed.client_notification, "ClientNotification.json")?,
            server_request: &parse(&probed.server_request, "ServerRequest.json")?,
            results: &results,
        })
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("projecting the {bundle} bundle onto the guarded surface"))?;
        changes.extend(
            gs::diff_wire(&gs::admissible_wire(bundle, &installed), &installed)
                .iter()
                .map(|c| format!("[{bundle}] {c}")),
        );
        admitted.push((bundle, serde_json::to_string(&installed)?));
    }

    if !changes.is_empty() {
        bail!(
            "this codex build's guarded surface differs from the one CodeConnect was \
             grounded against, so the checks that keep a session inside its sandbox have \
             not been proven for it:\n  {}\n\
             CodeConnect has to be re-grounded against this build before it can host it — \
             each item above is measured, pinned and re-tested. Updating CodeConnect is the \
             way through; downgrading codex is not asked for and will not be.",
            changes.join("\n  ")
        );
    }

    // The digest of what was ADMITTED — derived from the installed binary's own
    // projection, not from the vendored copy it was proven equal to. The two are equal
    // by the time control reaches here, so the values coincide; deriving it from the
    // vendored side would still be wrong, because it would report the same digest for a
    // codex whose surface was never actually read.
    //
    Ok(admitted_digest(&admitted))
}

/// The digest of the surfaces that were admitted, over an **unambiguous** encoding.
///
/// Each part is committed with its LABEL and its BYTE LENGTH before its bytes. A bare
/// concatenation is not unambiguous: two different splits of the same byte stream across
/// bundles digest identically, and so do two parts whose labels were swapped. A digest
/// that cannot distinguish those is not evidence about which surface was read — and this
/// value exists to be carried into a launch record as exactly that evidence.
fn admitted_digest(parts: &[(&str, String)]) -> String {
    let mut framed = String::new();
    for (label, body) in parts {
        framed.push_str(&format!("{label} {}\n{body}\n", body.len()));
    }
    protocol::hash::sha256_hex(framed.as_bytes())
}

// --------------------------------------------------------- reserved argv grammar

/// Why a `codex` argv was refused. Each variant renders a message that names what
/// was refused and why, so the refusal is legible at the terminal and pinned by
/// tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexRefusal {
    /// A flag whose value CodeConnect owns for the launch: the transport
    /// (`--remote`, `--remote-auth-token-env`), the working directory
    /// (`-C`/`--cd`), and the sandbox policy (`-s`/`--sandbox`, `--add-dir`).
    OwnedFlag { flag: String, owner: &'static str },
    /// `--profile`/`-p`: a named profile can carry approval and hook settings, so
    /// the profile choice is CodeConnect's, not the caller's.
    Profile { flag: String },
    /// A control that would move approval or hook-trust ownership away from
    /// CodeConnect (`-a`/`--ask-for-approval`, `--approve-for-me`, `--full-auto`,
    /// the `--dangerously-bypass-*` flags, and their `--yolo`/`--not-so-yolo`
    /// aliases).
    ApprovalControl { flag: String },
    /// A `-c`/`--config` override, or an `--enable`/`--disable` feature toggle,
    /// that reaches a configuration key CodeConnect owns — at any nesting, whether
    /// spelled as a dotted path or nested inside a TOML value.
    OwnedConfigKey { key: String, via: String },
    /// A subcommand name or alias. Only interactive-TUI invocation is supported;
    /// `resume`, `fork`, `exec`, and the rest are refused wherever codex would
    /// dispatch one.
    Subcommand { name: String },
    /// A token CodeConnect could not confidently classify as benign — a
    /// short-flag cluster it cannot fully expand, or a `-c` key whose quoting it
    /// cannot decode. Per the governing invariant (A7): fail closed on
    /// uncertainty rather than forward something past the ownership boundary.
    Unclassifiable { detail: String },
}

/// The extra clause an owned key earns when "CodeConnect owns it" is true but does
/// not say what the caller loses by it.
///
/// Most owned keys need nothing: `approval_policy` and `sandbox` are visibly the
/// session's policy, and a reader who reached for one knows what they were reaching
/// for. The pinned-off feature is different — it is refused not because CodeConnect
/// set it to something else it prefers, but because the request it would turn on has
/// nowhere to be answered from, and a bare "we own this" would read as a permission
/// problem instead of a missing surface.
///
/// Matched on the key's last segment so both spellings of the same setting reach it:
/// `-c` reports the dotted path (`features.request_permissions_tool`) and
/// `--enable`/`--disable` report the bare feature name.
fn why_owned(key: &str) -> Option<&'static str> {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    (leaf == PINNED_OFF_FEATURE).then_some(
        "CodeConnect answers approvals from the phone, and this one asks for a \
         permission profile that only the terminal can grant, so the session is \
         launched without it",
    )
}

impl std::fmt::Display for CodexRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexRefusal::OwnedFlag { flag, owner } => write!(
                f,
                "`{flag}` is set by CodeConnect ({owner}) and cannot be passed to `codeconnect codex`"
            ),
            CodexRefusal::Profile { flag } => write!(
                f,
                "`{flag}` is refused: a codex profile can carry approval and hook settings that \
                 CodeConnect owns for the session"
            ),
            CodexRefusal::ApprovalControl { flag } => write!(
                f,
                "`{flag}` is refused: CodeConnect owns approval and hook-trust policy for the session"
            ),
            CodexRefusal::OwnedConfigKey { key, via } => match why_owned(key) {
                Some(because) => write!(
                    f,
                    "`{via} {key}` is refused: `{key}` is a configuration key CodeConnect owns \
                     for the session — {because}"
                ),
                None => write!(
                    f,
                    "`{via} {key}` is refused: `{key}` is a configuration key CodeConnect owns for the session"
                ),
            },
            CodexRefusal::Subcommand { name } => write!(
                f,
                "`codex {name}` is a subcommand; `codeconnect codex` supports only the interactive \
                 session, so subcommands and their aliases are refused"
            ),
            CodexRefusal::Unclassifiable { detail } => write!(
                f,
                "`{detail}` could not be parsed with confidence, so it is refused rather than \
                 forwarded — to keep CodeConnect's ownership of the session's approval policy \
                 (pass a simpler invocation)"
            ),
        }
    }
}

/// A recognised flag's argument arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    /// Takes no value (`--search`, `--psp`).
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

/// Look up a long flag name (without a `=value` tail) in the 0.147 table.
///
/// The complete interactive/global flag surface of codex-cli 0.147.0, including
/// the **hidden** globals that do not appear in `--help` but are real:
/// `--psp` (a bool global), and the approval aliases `--yolo`
/// (= `--dangerously-bypass-approvals-and-sandbox`) and `--not-so-yolo`
/// (= `--approve-for-me`) from `shared_options`. `--full-auto` is recognised too:
/// the interactive parser rejects it, but it is a real approval-owner control
/// elsewhere in codex, so recognising it lets it be refused precisely rather than
/// forwarded to a generic "unexpected argument".
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
        "--full-auto" => flag("--full-auto", Arity::Bool),
        "--search" => flag("--search", Arity::Bool),
        "--no-alt-screen" => flag("--no-alt-screen", Arity::Bool),
        "--psp" => flag("--psp", Arity::Bool),
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
/// itself), a cluster of only recognised bool short-flags, a token that cannot be
/// confidently classified, a positional, or the `--` boundary.
enum Token {
    Flag {
        flag: KnownFlag,
        attached: Option<String>,
    },
    /// A short cluster of only bool flags (`-hV`), forwarded as-is.
    BoolCluster,
    /// A7 fail-closed: any flag-shaped token not on the benign/known allowlist —
    /// an unknown long flag, an unknown short flag, or a short cluster with an
    /// unknown character. Refused rather than forwarded, because an unrecognised
    /// flag is uncertainty and a hidden approval control (as `--psp` once was)
    /// must never ride through.
    Unclassifiable,
    Positional,
    Boundary,
}

/// Classify a single argv token in isolation. Attached values (`--model=x`,
/// `-mx`, `-C.`) are split out here; a spaced value is the following token and is
/// pulled by the caller. Short clusters are **fully expanded** so a bool short in
/// front of a value short (`-hcapproval_policy=never`) cannot smuggle an owned
/// key through as a discarded suffix. **Any flag-shaped token not on the known
/// allowlist is `Unclassifiable`** (A7 allowlist): only enumerated benign/known
/// flags pass; everything else is refused.
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
            None => Token::Unclassifiable,
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
/// `BoolCluster`; an unknown character anywhere (head or after a known short) is
/// not a known flag and fails closed (A7 allowlist).
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
            None => return Token::Unclassifiable,
        }
    }
    // Every character was a recognised bool short.
    Token::BoolCluster
}

/// Validate a `codex` argv against the reserved grammar.
///
/// `Ok(())` means every token is either a CodeConnect-neutral flag, a user flag,
/// a prompt, or content past the `--` boundary — all forwarded to codex verbatim.
/// `Err` names the first refused token.
///
/// Subcommand detection matches how codex actually dispatches (probed on 0.147):
/// a subcommand token is recognised in **any** positional slot, not just the
/// first — `codex please resume` dispatches Resume with `please` as the prompt,
/// and `codex --psp resume` dispatches Resume through a (known) hidden global
/// flag. Unknown flags never reach this stage: they are refused up front by the
/// allowlist, so they can neither ride through nor smuggle a subcommand.
///
/// **This is the crate's single source of truth for the grammar.** It has two
/// callers: [`start`] (the user's `codeconnect codex` argv) and
/// [`crate::codex_host::parse_host_args`] (the passthrough the coordinator hands
/// the wrapper's TUI). The host deliberately reuses it rather than restating it —
/// it does not trust its caller, and a second copy of the grammar could drift on
/// which flags CodeConnect owns.
pub fn validate_codex_argv(args: &[String]) -> Result<(), CodexRefusal> {
    scan_codex_argv(args).map(|_| ())
}

/// What one walk of a codex argv established.
struct ArgvScan {
    /// The argv to exec: every value-taking flag rewritten into attached form, and a
    /// `--` in front of the positionals. See [`fence_positionals`].
    normalized: Vec<String>,
}

/// **Insert `--` before the first positional, so a bare token can never dispatch.**
///
/// # Why the refusal table cannot be the safety property
///
/// `is_subcommand` is a closed list, and MEASURED: no enumeration codex emits carries
/// its hidden aliases — `cloud-tasks` dispatches on both binaries and appears in neither
/// `--help` nor any of the five completion shells. So a *future* hidden alias would be in
/// neither the vendored argv reference (the launch gate cannot see it) nor the refusal
/// table (nobody knew to add it), would be classified as prompt text, and codex would
/// dispatch it. That is the original escape class, and no amount of list-keeping closes
/// it, because the thing that would have to be enumerated cannot be.
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
/// # Why before the FIRST positional rather than before everything
///
/// `--` terminates option parsing too, so putting it in front of the whole passthrough
/// would turn the user's own `-m gpt-5` into positionals and break the launch. Inserting
/// it at the first positional preserves flag semantics and argument order exactly.
///
/// # Why every value-taking flag is REWRITTEN into attached form
///
/// Because otherwise the fence would rest on this walk's arity model agreeing with clap's,
/// and nothing gates that. The guarded-surface gate compares flag SPELLINGS and subcommand
/// names; it says nothing about how many values a flag consumes. So a future codex that
/// kept the same root token set but changed `-i` from greedy to single-value would pass
/// the gate, and this walk — still sweeping greedily — would consume `features` in
/// `-i a.png features` as an image, see no positional, insert no fence, and hand codex a
/// bare token it now dispatches.
///
/// The rewrite removes the dependency instead of trying to track it. Every flag in the
/// emitted argv is either a bool or carries its value **attached to the flag token**, so
/// no bare token is any flag's value under ANY arity model — which makes every remaining
/// bare token a positional, and the first of them is fenced. This walk's arity model is
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
/// `codeconnect codex resume` still says exactly why it was refused instead of silently
/// becoming a prompt.
pub fn fence_positionals(args: &[String]) -> Result<Vec<String>, CodexRefusal> {
    Ok(scan_codex_argv(args)?.normalized)
}

fn scan_codex_argv(args: &[String]) -> Result<ArgvScan, CodexRefusal> {
    let mut i = 0;
    let mut normalized: Vec<String> = Vec::with_capacity(args.len() + 1);
    // Set once, when the first positional is emitted: `--` goes in front of it and every
    // later token rides behind that boundary.
    let mut fenced = false;

    while i < args.len() {
        match classify(&args[i]) {
            // Everything after `--` is prompt content: forwarded verbatim, never
            // interpreted as a flag or a subcommand.
            Token::Boundary => {
                normalized.extend_from_slice(&args[i..]);
                return Ok(ArgvScan { normalized });
            }

            Token::Flag { flag, attached } => match flag.arity {
                Arity::Bool => {
                    refuse_bool(flag.canonical)?;
                    // A bool consumes no value, so its spelling cannot swallow anything;
                    // it is emitted canonically for uniformity, with any (rejectable)
                    // attached tail preserved so codex still sees what was written.
                    normalized.push(match attached {
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
                    refuse_value_flag(flag.canonical, value.as_deref())?;
                    normalized.push(match &value {
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
                        Some(value) => normalized.push(format!("{}={value}", flag.canonical)),
                        None => {
                            while i < args.len() && !looks_like_flag(&args[i]) {
                                normalized.push(format!("{}={}", flag.canonical, args[i]));
                                i += 1;
                            }
                        }
                    }
                }
            },

            // A cluster of only bool short-flags (`-hV`): consumes no value, so it is
            // forwarded as-is.
            Token::BoolCluster => {
                normalized.push(args[i].clone());
                i += 1;
            }

            // A7 allowlist fail-closed: a flag-shaped token not on the known
            // list, or a cluster we could not fully expand.
            Token::Unclassifiable => {
                return Err(CodexRefusal::Unclassifiable {
                    detail: args[i].clone(),
                })
            }

            Token::Positional => {
                if is_subcommand(&args[i]) {
                    return Err(CodexRefusal::Subcommand {
                        name: args[i].clone(),
                    });
                }
                if !fenced {
                    normalized.push("--".to_string());
                    fenced = true;
                }
                normalized.push(args[i].clone());
                i += 1;
            }
        }
    }
    Ok(ArgvScan { normalized })
}

/// Whether a token would begin a flag to codex (used to bound greedy `--image`).
/// A bare `-` is a value (stdin sentinel), not a flag.
fn looks_like_flag(token: &str) -> bool {
    token.starts_with('-') && token != "-"
}

/// Refuse an owned bool flag; forward the rest.
fn refuse_bool(canonical: &str) -> Result<(), CodexRefusal> {
    match canonical {
        "--approve-for-me"
        | "--not-so-yolo"
        | "--full-auto"
        | "--dangerously-bypass-approvals-and-sandbox"
        | "--yolo"
        | "--dangerously-bypass-hook-trust" => Err(CodexRefusal::ApprovalControl {
            flag: canonical.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Refuse an owned value flag, or inspect a `-c`/`--enable`/`--disable` key;
/// forward the rest.
fn refuse_value_flag(canonical: &str, value: Option<&str>) -> Result<(), CodexRefusal> {
    match canonical {
        "--remote" | "--remote-auth-token-env" => Err(CodexRefusal::OwnedFlag {
            flag: canonical.to_string(),
            owner: "the app-server transport",
        }),
        "--cd" => Err(CodexRefusal::OwnedFlag {
            flag: canonical.to_string(),
            owner: "the session working directory",
        }),
        // A10: the sandbox dimension. CodeConnect names the launch's sandbox in
        // the fingerprint it records and the broker enforces, so a passthrough
        // that moves that dimension is an ownership escape — the TUI would run
        // under a sandbox the fingerprint does not describe. `--add-dir` is the
        // same dimension by another name: on 0.147 it is "additional directories
        // that should be writable alongside the primary workspace", i.e. a
        // widening of the sandbox's writable roots.
        //
        // One arm covers every spelling: `-s` is canonicalised to `--sandbox` by
        // [`known_short`] before it reaches here, so the spaced, `=`-joined,
        // attached (`-sread-only`) and clustered (`-hs read-only`) forms all
        // arrive as this canonical name.
        "--sandbox" => Err(CodexRefusal::OwnedFlag {
            flag: canonical.to_string(),
            owner: "the session sandbox policy",
        }),
        "--add-dir" => Err(CodexRefusal::OwnedFlag {
            flag: canonical.to_string(),
            owner: "the session sandbox policy's writable roots",
        }),
        "--profile" => Err(CodexRefusal::Profile {
            flag: canonical.to_string(),
        }),
        "--ask-for-approval" => Err(CodexRefusal::ApprovalControl {
            flag: canonical.to_string(),
        }),
        "--config" => {
            if let Some(value) = value {
                match config_override_verdict(value) {
                    ConfigVerdict::Owned(key) => {
                        return Err(CodexRefusal::OwnedConfigKey {
                            key,
                            via: canonical.to_string(),
                        })
                    }
                    ConfigVerdict::Unclassifiable(detail) => {
                        return Err(CodexRefusal::Unclassifiable { detail })
                    }
                    ConfigVerdict::Benign => {}
                }
            }
            Ok(())
        }
        "--enable" | "--disable" => {
            if let Some(feature) = value {
                match feature_verdict(feature) {
                    FeatureVerdict::Owned => {
                        // `feature` is a bare identifier here (Owned implies it).
                        return Err(CodexRefusal::OwnedConfigKey {
                            key: feature.to_string(),
                            via: canonical.to_string(),
                        });
                    }
                    FeatureVerdict::Unclassifiable => {
                        return Err(CodexRefusal::Unclassifiable {
                            detail: format!("{canonical} {feature}"),
                        })
                    }
                    FeatureVerdict::Benign => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The verdict on a `-c`/`--config` override.
enum ConfigVerdict {
    /// Reaches an owned setting; carries the offending key path for the message.
    Owned(String),
    /// Could not be parsed with confidence (an undecodable key, or a structured
    /// value we cannot realise): refuse per A7 rather than forward.
    Unclassifiable(String),
    /// Forward.
    Benign,
}

/// Judge a `-c`/`--config` override.
///
/// Parsed the way codex parses it (`config_override.rs`, against the **same**
/// embedded `toml` grammar — TOML 1.1): the raw is split on the first `=` into a
/// key-path and a value. The **key** is decoded with real TOML key semantics
/// (bare/quoted/dotted keys), not a naive `.` split — so `apps."team.prod".x`
/// decodes to three segments and `"approval_policy"` decodes to the owned key —
/// and a key whose quoting we cannot decode fails closed. Ownership is judged
/// **by full key path**, not by a matching key name: a path is refused only when
/// it reaches an actual owned setting
/// (`apps.<id>.tools.<tool>.approval_mode`, `mcp_servers.<id>.default_tools_approval_mode`,
/// `features.hooks`, …), so an owned-*named* key that is arbitrary data in an
/// unowned container — `mcp_servers.<id>.env.approval_mode`, a server named
/// `hooks` — forwards. The **value** is realised as TOML: a structured literal
/// (`{…}`/`[…]`) that does not parse fails closed (codex, on the same grammar,
/// might realise and apply it); a scalar that does not parse is codex's own
/// string-literal fallback, harmless.
fn config_override_verdict(raw: &str) -> ConfigVerdict {
    let (key_part, value_part) = match raw.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (raw, None),
    };

    let mut path = match decode_toml_key_path(key_part) {
        Some(path) => path,
        // A7: a key whose TOML quoting/escaping we cannot decode is refused.
        None => return ConfigVerdict::Unclassifiable(key_part.trim().to_string()),
    };

    // The value is judged on the RAW, untrimmed argument: trimming here would
    // erase an edge CR/LF before the injection check that depends on it.
    let value = match value_part {
        None => None,
        Some(value) => match parse_toml_value(value) {
            ValueParse::Value(realised) => Some(realised),
            // A second assignment / table header injected past the value: codex,
            // on the same grammar, might realise and apply it. Fail closed.
            ValueParse::Injected => return ConfigVerdict::Unclassifiable(raw.trim().to_string()),
            // A structured literal we could not parse: fail closed for the same
            // reason. (Trim only to recognise the `{`/`[` shape — a broadening,
            // never-weakening use of trim.)
            ValueParse::StringLiteral if is_structured_literal(value.trim()) => {
                return ConfigVerdict::Unclassifiable(raw.trim().to_string())
            }
            // A scalar codex would treat as a string literal: no nested keys.
            ValueParse::StringLiteral => None,
        },
    };

    match owned_in_subtree(&mut path, value.as_ref()) {
        Some(owned) => ConfigVerdict::Owned(owned),
        None => ConfigVerdict::Benign,
    }
}

/// The outcome of realising a `-c` value as TOML.
enum ValueParse {
    /// A single clean TOML value (scalar, inline table, array).
    Value(toml::Value),
    /// Did not parse as TOML — codex's string-literal fallback (no nested keys).
    StringLiteral,
    /// A multi-line value that is not a single TOML value: a second assignment
    /// or table header injected past the value (`1\napproval_policy="never"`).
    /// Fail closed.
    Injected,
}

/// Realise a `-c` value as TOML — **directly**, not through a wrapper, so there
/// is no sentinel a caller could collide with. `toml::Value` parses a lone value
/// (scalar, inline table incl. multi-line/1.1, array); trailing content after the
/// value is a parse error. A parse failure is codex's string-literal fallback —
/// **except** when the raw value contains a CR/LF: a scalar never spans lines, so
/// a multi-line parse failure is a `key = value` (or table-header) injection and
/// fails closed.
///
/// The injection check reads the **raw** value for a CR/LF; the parse itself is
/// fed the trimmed value only because `toml::Value` rejects surrounding
/// whitespace, and trimming for the parse can only *broaden* what is recognised
/// as structured (a padded `{…}` still walks / fails closed), never hide a
/// newline from the raw check.
fn parse_toml_value(value: &str) -> ValueParse {
    match value.trim().parse::<toml::Value>() {
        Ok(realised) => ValueParse::Value(realised),
        Err(_) if value.contains(['\n', '\r']) => ValueParse::Injected,
        Err(_) => ValueParse::StringLiteral,
    }
}

/// Whether a `-c` value is clearly meant as a structured TOML literal.
fn is_structured_literal(value: &str) -> bool {
    value.starts_with('{') || value.starts_with('[')
}

/// Decode a `-c` key into its path segments using **TOML key grammar** — the key
/// is parsed strictly as the left-hand side of a single TOML assignment, so bare
/// keys, quoted keys (`"team.prod"` is one segment, `"prod[#1]"` too), dotted keys
/// and escapes are decoded correctly. Returns `None`, the A7 fail-closed signal,
/// when the key does not parse as one lone key chain (unbalanced quote, empty
/// key, an unquoted `[table header]`, stray whitespace).
fn decode_toml_key_path(key: &str) -> Option<Vec<String>> {
    // The one unconditional reject: a raw CR/LF, which no single key expression
    // contains and which is how a table-header / second-assignment injection is
    // introduced. `[`/`]`/`#` are legal **inside** a quoted key segment, so they
    // are not blanket-rejected — an unquoted table header still fails the parse
    // below and fails closed there.
    if key.contains(['\n', '\r']) {
        return None;
    }
    // Assign a distinctive synthetic sentinel value. A genuine single-key
    // assignment resolves to exactly this at the end of the chain; a
    // table/array-header-shaped key (`[[benign]] #`) resolves to a table or array
    // (and may comment out the `= …`), so its terminal is not the sentinel.
    const SENTINEL: i64 = 0;
    let document = format!("{key} = {SENTINEL}");
    let table: toml::Table = document.parse().ok()?;
    let mut path = Vec::new();
    let mut current = toml::Value::Table(table);
    // A single override key produces one chain of single-entry tables.
    while let toml::Value::Table(mut table) = current {
        if table.len() != 1 {
            return None;
        }
        let key = table.keys().next()?.clone();
        let value = table.remove(&key)?;
        path.push(key);
        current = value;
    }
    // The terminal MUST be exactly the synthetic sentinel we assigned — proof
    // that the key resolved to a plain `key = value` leaf, not to a table/array
    // structure the key itself introduced.
    if current != toml::Value::Integer(SENTINEL) {
        return None;
    }
    (!path.is_empty()).then_some(path)
}

/// Walk the config subtree rooted at `path` (extending it with the realised
/// value's keys), returning the first full path that reaches an owned setting.
fn owned_in_subtree(path: &mut Vec<String>, value: Option<&toml::Value>) -> Option<String> {
    if path_is_owned(path) {
        return Some(path.join("."));
    }
    match value {
        Some(toml::Value::Table(table)) => {
            for (key, nested) in table {
                path.push(key.clone());
                let found = owned_in_subtree(path, Some(nested));
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        // An array introduces no named segment; walk its elements at the same
        // path so a table nested in one is still reached.
        Some(toml::Value::Array(items)) => items
            .iter()
            .find_map(|item| owned_in_subtree(path, Some(item))),
        _ => None,
    }
}

/// Whether a full config key path reaches a setting CodeConnect owns.
///
/// The owned surfaces, grounded against the codex 0.147 binary (each field name
/// and its `<id>`/`<tool>` nesting confirmed by feeding invalid values and
/// reading which key codex names in the validation error):
///   * the top-level approval controls, as tables/scalars — `approval_policy`
///     (including its `granular.*` table), `approvals_reviewer`, `hooks`, `notify`
///     — owned at and below their root;
///   * the top-level **sandbox** controls (A10) — `sandbox`, `sandbox_mode`,
///     `sandbox_policy`, `sandbox_workspace_write`, `sandbox_permissions` — owned
///     at and below their root, for the same reason `-s`/`--sandbox` is refused:
///     CodeConnect names the sandbox in the launch fingerprint, so a `-c` that
///     moves it is the same ownership escape wearing a config key. Probed on
///     0.147: `sandbox_workspace_write.writable_roots` and
///     `.network_access` are real typed settings (feeding an integer names the
///     key in codex's own validation error) and `sandbox_mode` is a real string
///     enum; `sandbox`, `sandbox_policy` and `sandbox_permissions` are the
///     spellings codex's own `-c` help example and the broker's sandbox
///     fingerprint dimension use. `sandbox_permissions` is owned **deliberately**:
///     an earlier revision forwarded `sandbox_permissions=["disk-full-read-access"]`
///     as benign, but a read-scope widening is a mutation of the very dimension
///     CodeConnect claims, so it belongs on this axis and is now refused.
///     Refusal needs no knowledge of what a key expands to — that a token reaches
///     an owned root is the whole test — so an unenforced or renamed spelling
///     costs an over-refusal (acceptable under A7), never an escape;
///   * the top-level **permission-profile** controls — `permissions` and
///     `default_permissions` — owned at and below their root. These are a SECOND,
///     independent sandbox channel, not a spelling of the
///     first, and the list above missed them. Probed on the installed 0.147 with
///     the same invalid-value technique: `-c permissions=5` ⇒ "invalid type:
///     integer `5`, expected struct PermissionsToml in `permissions`";
///     `-c 'permissions={wide=5}'` ⇒ "expected struct PermissionProfileToml";
///     `-c 'permissions={wide={filesystem=5}}'` ⇒ "expected struct
///     FilesystemPermissionsToml"; `-c default_permissions=5` ⇒ "invalid type:
///     integer `5`, expected a string in `default_permissions`". They are live and
///     COUPLED, which is what makes them a profile system rather than two stray
///     keys: `-c 'default_permissions="x"'` alone ⇒ "default_permissions requires a
///     `[permissions]` table", and a `[permissions]` table alone ⇒ "config defines
///     `[permissions]` profiles but does not set `default_permissions`". And the
///     pair together is ACCEPTED and activated:
///     `-c 'permissions={wide={filesystem={"/"="write"}}}' -c 'default_permissions="wide"'`
///     runs. A forwarded `-c` carrying them therefore hands the pane a filesystem
///     write scope CodeConnect never named in its launch fingerprint — the same
///     ownership escape as `-s`/`--sandbox`, wearing a different config key;
///   * `features.hooks` (and below) and `features.codex_hooks` — hook enablement;
///   * `features.request_permissions_tool` — the one feature the launch pins off,
///     because the request it turns on can only be answered at the terminal. Owned
///     in BOTH directions on purpose: the launch already pins the value, so this
///     refusal buys legibility rather than safety — a caller who asks for the tool
///     is told why they cannot have it instead of watching the pin quietly win.
///     See [`PINNED_OFF_FEATURE`];
///   * `auto_review.policy` — selecting the automatic reviewer;
///   * per-app: `apps.<id>.default_tools_approval_mode`,
///     `apps.<id>.approvals_reviewer`, `apps.<id>.tools.<tool>.approval_mode`;
///   * per-MCP-server: `mcp_servers.<id>.default_tools_approval_mode`,
///     `mcp_servers.<id>.tools.<tool>.approval_mode`.
///
/// Case-sensitive, since codex's TOML keys are.
fn path_is_owned(path: &[String]) -> bool {
    let seg = |i: usize| path.get(i).map(String::as_str);

    // Top-level controls: owned at their root and anywhere beneath it.
    if matches!(
        seg(0),
        Some("approval_policy")
            | Some("approvals_reviewer")
            | Some("hooks")
            | Some("notify")
            | Some("sandbox")
            | Some("sandbox_mode")
            | Some("sandbox_policy")
            | Some("sandbox_workspace_write")
            | Some("sandbox_permissions")
            | Some("permissions")
            | Some("default_permissions")
    ) {
        return true;
    }

    match seg(0) {
        Some("features") => {
            matches!(seg(1), Some("hooks") | Some("codex_hooks"))
                || seg(1) == Some(PINNED_OFF_FEATURE)
        }
        Some("auto_review") => seg(1) == Some("policy"),
        Some("apps") if path.len() >= 3 => match (seg(2), path.len()) {
            (Some("default_tools_approval_mode"), 3) => true,
            (Some("approvals_reviewer"), 3) => true,
            (Some("tools"), 5) => seg(4) == Some("approval_mode"),
            _ => false,
        },
        Some("mcp_servers") if path.len() >= 3 => match (seg(2), path.len()) {
            (Some("default_tools_approval_mode"), 3) => true,
            (Some("tools"), 5) => seg(4) == Some("approval_mode"),
            _ => false,
        },
        _ => false,
    }
}

/// The verdict on an `--enable`/`--disable <FEATURE>` name.
enum FeatureVerdict {
    /// A feature CodeConnect owns: either half of hook enablement, or the one
    /// feature the launch pins off ([`PINNED_OFF_FEATURE`]).
    Owned,
    /// Not a plain bare feature identifier — quotes, dots, escapes, whitespace,
    /// or anything a bare name never has. Refuse (A7): codex takes only bare
    /// feature identifiers (probed: `--enable '"hooks"'` ⇒ "Unknown feature flag"),
    /// so a non-bare value cannot be a real feature, and we do not try to decode
    /// what codex would reject.
    Unclassifiable,
    /// A benign bare feature name; forward.
    Benign,
}

/// Judge an `--enable`/`--disable` feature name.
///
/// codex feature names are bare identifiers (probed on 0.147: `--enable hooks`,
/// `--enable web_search` are accepted bare; `--enable '"hooks"'` is rejected as an
/// unknown flag — the quotes are literal, not decoded). So the allowlist is: a
/// plain bare identifier (`[A-Za-z0-9_-]+`); anything else fails closed; the
/// bare owned features — both halves of hook enablement and the one the launch
/// pins off — are refused. `--enable X` and `--disable X` are codex's own
/// spellings of `-c features.X=true|false`, so they are judged on the same axis
/// as the `-c` and refused in both directions: what is owned is the setting, not
/// a direction to move it in.
fn feature_verdict(feature: &str) -> FeatureVerdict {
    // Validate the RAW, untrimmed value: any leading/trailing/embedded whitespace
    // or newline means it is not a bare identifier, so it fails closed — never
    // trim before this decision.
    let is_bare = !feature.is_empty()
        && feature
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !is_bare {
        return FeatureVerdict::Unclassifiable;
    }
    if matches!(feature, "hooks" | "codex_hooks") || feature == PINNED_OFF_FEATURE {
        return FeatureVerdict::Owned;
    }
    FeatureVerdict::Benign
}

/// Whether a bare positional token is a codex subcommand name or alias.
///
/// The **union** of the top-level command sets of every codex build CodeConnect has
/// been grounded against, enumerated from clap's own completion output — including the
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
/// Keeping this in step with reality is not left to diligence: the launch gate
/// ([`ensure_guarded_surface`]) refuses any codex whose root command set has moved away
/// from a vendored reference, and `every_dispatchable_root_token_is_accounted_for` proves
/// both directions of the tie — every referenced subcommand is refused here, and every
/// token refused here is either in a reference or on the measured hidden-alias list.
/// The table itself, as data rather than a `matches!` arm, so
/// [`every_dispatchable_root_token_is_accounted_for`] can walk it in both directions.
/// A table that can only be *queried* can hold a token no reference knows about and
/// nothing would notice.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn charter_over_a_real_file_with(sha256: &str, tui_args: &[String]) -> (Vec<String>, PathBuf) {
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
        });
        (argv, real)
    }

    /// **The A7.1 invariant, as a falsifiable test rather than a paragraph: the
    /// charter carries the digest resolution took, and never re-derives one.**
    ///
    /// The distinction is invisible on a quiet machine — re-hashing an unchanged
    /// file returns the same string — and it is the entire value of the pin on a
    /// busy one, where a `codex` install landing between resolution and this call
    /// makes the two answers differ and only the carried one still describes the
    /// bytes that were magic-checked and version-pinned.
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
        let (argv, real) = charter_over_a_real_file_with(&pinned, &[]);

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
        let (argv, _) = charter_over_a_real_file_with(&"b".repeat(CODEX_SHA256_HEX_LEN), &[]);

        // The eight the coordinator requires, plus the four the launcher owns.
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
            "--approval-policy",
            "--approvals-reviewer",
            "--sandbox",
            "--hooks-enabled",
        ] {
            assert!(
                flag(&argv, required).is_some(),
                "the charter must name {required}, which nothing downstream defaults: {argv:?}"
            );
        }
        assert_eq!(flag(&argv, "--session-name"), Some("cc-7"));
        assert_eq!(flag(&argv, "--cwd"), Some("/some/where"));
        assert_eq!(flag(&argv, "--approval-policy"), Some("on-request"));
        assert_eq!(flag(&argv, "--approvals-reviewer"), Some("user"));
        assert_eq!(flag(&argv, "--sandbox"), Some("read-only"));
        assert_eq!(flag(&argv, "--hooks-enabled"), Some("true"));
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
        let (bare, _) = charter_over_a_real_file_with(&"c".repeat(CODEX_SHA256_HEX_LEN), &[]);
        assert!(
            !bare.iter().any(|a| a == "--"),
            "an empty passthrough must add no boundary: {bare:?}"
        );

        let passthrough = vec!["--model".to_string(), "gpt-5".to_string(), "hi".to_string()];
        let (with, _) =
            charter_over_a_real_file_with(&"c".repeat(CODEX_SHA256_HEX_LEN), &passthrough);
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
            with[..at].iter().any(|a| a == "--hooks-enabled"),
            "the launcher's own dimensions must all precede the boundary: {with:?}"
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
        let source = include_str!("codex.rs");
        let at = source
            .find(
                "fn probe_codex(resolved: &ResolvedCodex, scratch: &Path) -> Result<CodexProbe> {",
            )
            .expect("probe_codex must exist");
        let rest = &source[at..];
        let probe = &rest[..rest.find("\n}\n").expect("a closed function body")];

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
        let source = include_str!("codex.rs");
        let body = |signature: &str| {
            let at = source
                .find(signature)
                .unwrap_or_else(|| panic!("{signature} must exist"));
            let rest = &source[at..];
            // rustfmt puts a top-level function's closing brace alone at column 0,
            // so the first `\n}\n` past the signature ends the body.
            &rest[..rest.find("\n}\n").expect("a closed function body")]
        };

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
        let launch_body = body("fn launch(resolved: &ResolvedCodex, passthrough: &[String])");
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

    /// **The pin is an argv override, and something can outrank an argv override.**
    ///
    /// codex ranks a managed configuration layer above `-c`, so an administrator who
    /// pushes `[features] request_permissions_tool = true` through it beats the value
    /// every spawn carries. What the launch asserts today is the argv; what this
    /// asserts is the outcome — the feature read back out of the codex about to be
    /// used, with the launch's own override applied, exactly as the app-server will
    /// see it.
    ///
    /// Three answers, and only one of them launches.
    #[test]
    fn the_effective_feature_is_read_back_and_only_off_launches() {
        let off = "\
apply_patch_freeform                     removed            false
request_permissions_tool                 under development  false
web_search                               stable             true
";
        refuse_unless_pinned_feature_is_off(off).expect("an effective false is the launch case");

        let on = off.replace(
            "request_permissions_tool                 under development  false",
            "request_permissions_tool                 under development  true",
        );
        let refused = refuse_unless_pinned_feature_is_off(&on)
            .expect_err("an effective true must refuse the launch");
        let text = format!("{refused:#}");
        for expected in [PINNED_OFF_FEATURE, "managed", "refusing to launch"] {
            assert!(
                text.contains(expected),
                "the refusal must say what was found and why: {text}"
            );
        }

        // **A listing that does not answer is not an answer of `false`.** The same
        // rule `verify_codex_identity` states: could-not-check and it-is-off are
        // different answers and only one licenses a spawn.
        let silent = off.replace("request_permissions_tool", "some_other_feature");
        assert!(
            refuse_unless_pinned_feature_is_off(&silent).is_err(),
            "a listing that never names the feature must refuse rather than assume"
        );
        assert!(
            refuse_unless_pinned_feature_is_off(
                "request_permissions_tool  under development  maybe"
            )
            .is_err(),
            "a value that is neither true nor false must refuse rather than be guessed at"
        );
    }

    /// **Driven against the real installed codex, in a scratch `CODEX_HOME`.**
    ///
    /// The parse above is about strings; this is about whether the launch's override
    /// actually wins where it has to. Measured here rather than asserted from the
    /// documentation: an operator `config.toml` turning the feature on is the case
    /// the pin was built for, and the launch must still go through.
    ///
    /// The managed layer that outranks the override lives at a system path, so the
    /// refusing half cannot be staged without changing the machine — it is covered
    /// by the parse above, on the answer such a layer would produce.
    #[test]
    fn the_launch_override_beats_an_operator_config_on_the_real_binary() {
        let Ok(resolved) = resolve_codex_bin(&Config::default()) else {
            return; // no codex installed; the parse test above still holds
        };
        let home = std::env::temp_dir().join(format!(
            "cc-features-probe-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[features]\nrequest_permissions_tool = true\n",
        )
        .unwrap();

        let listing = read_effective_features(&resolved.path, &home)
            .expect("`codex features list` is a local command and answers without an account");
        let listing = String::from_utf8_lossy(&listing);
        assert!(
            listing.contains(PINNED_OFF_FEATURE),
            "the installed codex still has to know the feature this launch pins: {listing}"
        );
        refuse_unless_pinned_feature_is_off(&listing)
            .expect("the launch's own override must beat an operator config.toml that turns it on");
        std::fs::remove_dir_all(&home).ok();
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

    // ------------------------------------------------ executable identity (A7.1)

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

    /// `codex --version` is itself one of the opens A7 names, so a version that
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
    /// check — which `read_codex_version` cannot tell apart from any other mismatch,
    /// and does not need to: it asks only "are these still the pinned bytes?".
    #[test]
    fn a_parsed_version_alone_does_not_let_a_launch_through() {
        if !on_path("codex") {
            eprintln!("skipped: no `codex` on PATH — nothing to version-check");
            return;
        }
        let real = resolve_codex_bin(&Config::default()).unwrap();
        // Sanity: this same call succeeds when the pin holds — covered by
        // `the_live_binary_reports_a_pinned_version`, which resolves and reads for
        // real. Here only the mismatch arm is staged, so the suite pays for one
        // hash of a 220 MB binary rather than two.
        let swapped = ResolvedCodex {
            path: real.path.clone(),
            sha256: protocol::hash::sha256_hex(b"some other codex"),
        };
        let err = read_codex_version(&swapped)
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
    /// `read_codex_version` only execs the path it is given.
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
        let err = read_codex_version(&swapped).expect_err("a moved binary must be refused");
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
        let err = read_codex_version(&honest).expect_err("an unparseable version must be refused");
        assert!(
            format!("{err:#}").contains("could not read a version"),
            "an unmoved binary's real problem must still surface: {err:#}"
        );

        cleanup(&root);
    }

    // --------------------------------------------------------- version pinning

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

    /// The version string is READ and RECORDED, and it decides nothing.
    ///
    /// Read the source rather than call anything, the same idiom
    /// `the_preflight_refuses_before_a_uid_or_a_tmux_name_is_taken` uses and for the
    /// same reason: what must be proven is the *absence* of a call inside a function
    /// that talks to a live binary, which no unit test can reach by calling it.
    ///
    /// This is the mutation guard for the whole change. Restore the version-string
    /// compare in `start` and this test goes red — which is what stops the pin being
    /// quietly reinstated "just to be safe" beside a gate that already answers the
    /// question properly, leaving every weekly codex refused again.
    #[test]
    fn the_version_string_is_recorded_and_never_gates() {
        let source = include_str!("codex.rs");
        let at = source
            .find("pub fn start(passthrough: &[String]) -> Result<()> {")
            .expect("start must exist");
        let rest = &source[at..];
        let start = &rest[..rest.find("\n}\n").expect("a closed function body")];

        assert!(
            start.contains("probe_codex(") && start.contains("parse_codex_version("),
            "the version must still be READ — it names what ran, and it is one of the \
             five answers `probe_codex` reads under a single held freeze"
        );
        for gate in ["ensure_pinned_version", "is_pinned_codex_version"] {
            assert!(
                !start.contains(gate),
                "{gate} is back in start(): the version string must be recorded, not \
                 gated. The guarded-surface gate is what decides."
            );
        }
        assert!(
            start.contains("ensure_guarded_surface("),
            "start must run the guarded-surface gate"
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
    /// for the same reason `the_version_string_is_recorded_and_never_gates` does: what
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
        let host = include_str!("codex_host.rs");
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
        let source = include_str!("codex.rs");
        let at = source
            .find("fn coordinator_charter(inputs: &CharterInputs<'_>) -> Vec<String> {")
            .expect("the charter builder must exist");
        let rest = &source[at..];
        let charter = &rest[..rest.find("\n}\n").expect("a closed function body")];
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
        let coord = include_str!("codex_coordinator.rs");
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

    /// The installed codex — whatever version it is — must pass the real gate.
    ///
    /// This replaces `the_live_binary_reports_a_pinned_version`, whose premise was a
    /// literal ("is it 0.147?") and which therefore went red on every codex release
    /// while proving nothing about whether the release was safe to host. The premise is
    /// now the honest one: the gate admitted this build.
    #[test]
    fn the_live_binarys_guarded_surface_is_admitted() {
        if !on_path("codex") {
            eprintln!("skipped: no `codex` on PATH — nothing to check");
            return;
        }
        let resolved = resolve_codex_bin(&Config::default()).unwrap();
        let scratch = ScratchDir::new().expect("scratch dir");
        let probe = probe_codex(&resolved, &scratch.0).expect("the launch probes must run");
        let version = parse_codex_version(&String::from_utf8_lossy(&probe.version_out))
            .expect("codex --version must parse");
        match ensure_guarded_surface(&probe) {
            Ok(digest) => assert_eq!(digest.len(), 64, "the surface digest is a sha256"),
            Err(e) => panic!(
                "the installed codex {version} is not one this build is grounded \
                 against:\n{e:#}"
            ),
        }
    }

    /// **A probe is bounded in both directions, proven deterministically.**
    ///
    /// No codex and no `CC_CODEX_LIVE`, because the dangerous shapes are invisible
    /// against the real binary: it answers and exits in the same breath, so an unbounded
    /// collection looks fine forever. The gate runs whatever is installed at the codex
    /// path — deciding whether to host it is the whole job — so it must survive a binary
    /// that never closes its pipes and one that streams without end.
    ///
    /// The flood goes on **stdout** here, and the ceiling is what has to catch it: a
    /// prefix of a flood is not a shorter answer.
    #[test]
    fn a_probe_that_hangs_or_floods_is_refused_rather_than_waited_on() {
        use std::os::unix::fs::PermissionsExt;
        // Short, because what is being observed is that the wait ENDS — paying the
        // production budget to watch a clock run out would only make the suite slower.
        const BUDGET: Duration = Duration::from_secs(2);
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
            run_bounded(&good, &[], BUDGET).expect("a well-behaved probe is read normally"),
            b"hello\n"
        );

        // A descendant inherits the write end and never exits: EOF never arrives, so an
        // `output()` would block past any deadline above it.
        let forker = write("forker", "#!/bin/sh\necho hello\nsleep 600 &\nexit 0\n");
        let started = Instant::now();
        let why = run_bounded(&forker, &[], BUDGET).expect_err("a held pipe must be refused");
        assert!(
            why.to_string().contains("did not close its"),
            "expected a pipe-close refusal, got: {why:#}"
        );
        assert!(
            started.elapsed() < BUDGET + PROBE_REAP_BUDGET + Duration::from_secs(5),
            "the probe must cost its budget, not the sleeper's lifetime"
        );

        // A flood: valid-looking first line, then more bytes than the ceiling allows.
        let flooder = write(
            "flooder",
            "#!/bin/sh\necho hello\nyes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        );
        let why = run_bounded(&flooder, &[], BUDGET).expect_err("a flood must be refused");
        assert!(
            why.to_string().contains("wrote more than"),
            "expected an output-ceiling refusal, got: {why:#}"
        );
    }

    /// The admitted-surface digest distinguishes surfaces a bare concatenation cannot.
    ///
    /// It is meant to be carried into a launch record as evidence of *which* surface was
    /// read, so two different readings must not share a digest. Both collisions a
    /// delimiter-free join admits are checked: a byte moved across the boundary between
    /// two parts, and two parts whose labels were swapped.
    #[test]
    fn the_admitted_digest_is_framed_and_labelled() {
        let d = |parts: &[(&str, &str)]| {
            admitted_digest(
                &parts
                    .iter()
                    .map(|(l, b)| (*l, b.to_string()))
                    .collect::<Vec<_>>(),
            )
        };
        let base = d(&[("argv", "ab"), ("stable", "cd")]);
        assert_ne!(
            base,
            d(&[("argv", "abc"), ("stable", "d")]),
            "a byte moved across the boundary must change the digest"
        );
        assert_ne!(
            base,
            d(&[("stable", "ab"), ("argv", "cd")]),
            "swapping which bundle a surface came from must change the digest"
        );
        assert_eq!(base.len(), 64);
    }

    /// The scratch tree is created exclusively, so the gate never generates into — or
    /// reads back out of — a directory somebody else placed at the path.
    #[test]
    fn the_scratch_directory_will_not_adopt_one_that_already_exists() {
        let dir = ScratchDir::new().expect("scratch dir");
        let path = dir.0.clone();
        assert!(
            std::fs::DirBuilder::new().create(&path).is_err(),
            "creating the scratch tree must fail when anything is already at the path"
        );
        // …and it is private to this user from the moment it exists.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the scratch tree must be 0700");
    }

    /// A generated bundle larger than the ceiling is a refusal, not an allocation.
    #[test]
    fn an_oversized_generated_bundle_is_refused() {
        let dir = ScratchDir::new().expect("scratch dir");
        let small = dir.0.join("small.json");
        std::fs::write(&small, b"{}").expect("write");
        assert_eq!(read_generated(&small).expect("small reads"), b"{}");
        assert!(
            read_generated(&dir.0.join("absent.json")).is_err(),
            "a bundle codex did not write is a refusal"
        );
    }

    /// **A generated bundle that is not a regular file must be refused PROMPTLY.**
    ///
    /// The writer here is the binary the gate has not yet decided to host, and it chooses
    /// what to put at these paths. A FIFO with no writer blocks `File::open` forever —
    /// outside every probe deadline, and *while the executable freeze is still held*, so
    /// the launch hangs and the freeze never clears. The bound is asserted in wall-clock
    /// terms because "refused" and "refused eventually" are different failures here.
    #[test]
    fn a_generated_bundle_that_is_a_fifo_is_refused_without_blocking() {
        let dir = ScratchDir::new().expect("scratch dir");
        let fifo = dir.0.join("ClientRequest.json");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");

        let started = Instant::now();
        let why = read_generated(&fifo).expect_err("a FIFO must be refused");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the refusal must be prompt; a blocking open would hold the freeze forever"
        );
        assert!(
            format!("{why:#}").contains("regular file")
                || format!("{why:#}").contains("Device not configured"),
            "expected a not-a-regular-file refusal, got: {why:#}"
        );

        // A symlink at the final component is refused too: the bundle must be the one the
        // probe wrote, not one pointed elsewhere after the fact.
        let real = dir.0.join("real.json");
        std::fs::write(&real, b"{}").expect("write");
        let link = dir.0.join("ClientNotification.json");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert!(read_generated(&link).is_err(), "a symlink must be refused");
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
    fn owned_transport_and_cwd_flags_are_refused_every_form() {
        for parts in [
            &["--remote", "unix:///x"][..],
            &["--remote=unix:///x"][..],
            &["--remote-auth-token-env", "TOK"][..],
            &["-C", "/x"][..],
            &["-C/x"][..],
            &["-C."][..],
            &["--cd", "/x"][..],
            &["--cd=/x"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::OwnedFlag { .. }),
                "{parts:?} should be an owned-flag refusal"
            );
        }
    }

    /// A10: the sandbox dimension is CodeConnect's, so no spelling of the two
    /// flags that move it may be forwarded. Every normalized form the grammar
    /// admits is pinned here — spaced, `=`-joined, attached short, and a short
    /// cluster whose value short is `s` (both with the value attached to the
    /// cluster and spaced after it) — because a form that slipped past would
    /// forward a sandbox mutation the launch fingerprint does not describe.
    #[test]
    fn sandbox_policy_flags_are_refused_every_form() {
        for parts in [
            &["--sandbox", "danger-full-access"][..],
            &["--sandbox=workspace-write"][..],
            &["-s", "read-only"][..],
            &["-sread-only"][..],
            &["-s=read-only"][..],
            // Short clusters: a bool short in front of `-s` must not let the
            // sandbox value ride through as a discarded suffix.
            &["-hsread-only"][..],
            &["-hs", "read-only"][..],
            &["-Vsdanger-full-access"][..],
            // A missing value is still the owned flag: codex would error, but the
            // refusal must not depend on a value being present.
            &["--sandbox"][..],
            &["-s"][..],
            // The writable-root widening on the same dimension.
            &["--add-dir", "/repo"][..],
            &["--add-dir=/repo"][..],
            &["--add-dir"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::OwnedFlag { .. }),
                "{parts:?} should be an owned-flag refusal"
            );
        }
    }

    #[test]
    fn profile_is_refused_spaced_equals_and_attached() {
        for parts in [
            &["--profile", "work"][..],
            &["--profile=work"][..],
            &["-p", "work"][..],
            &["-pwork"][..],
            &["-pfoo"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::Profile { .. }),
                "{parts:?} should be a profile refusal"
            );
        }
    }

    #[test]
    fn approval_owner_controls_and_aliases_are_refused() {
        for parts in [
            &["-a", "never"][..],
            &["-aon-request"][..],
            &["-a", "on-request"][..],
            &["--ask-for-approval", "untrusted"][..],
            &["--ask-for-approval=never"][..],
            &["--approve-for-me"][..],
            &["--full-auto"][..],
            &["--dangerously-bypass-approvals-and-sandbox"][..],
            &["--dangerously-bypass-hook-trust"][..],
            // Hidden aliases from shared_options.
            &["--yolo"][..],
            &["--not-so-yolo"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::ApprovalControl { .. }),
                "{parts:?} should be an approval-control refusal"
            );
        }
    }

    #[test]
    fn owned_config_keys_are_refused_including_structural_toml() {
        for parts in [
            // Top-level owned controls.
            &["-c", "approval_policy=never"][..],
            &["-capproval_policy=never"][..],
            &["-c", "approvals_reviewer=auto_review"][..],
            &["--config", "approval_policy=never"][..],
            &["--config=approval_policy=never"][..],
            &["-c", "hooks.pre=x"][..],
            &["-c", "hooks=x"][..],
            &["-c", "notify=x"][..],
            &["-c", "notify.command=x"][..],
            &["-c", "features.hooks=true"][..],
            &["-c", "features.hooks.trust=true"][..],
            &["-c", "approval_policy.granular.foo=never"][..],
            &["-c", "features.codex_hooks=false"][..],
            &["-c", "auto_review.policy=approve"][..],
            // Schema-valid per-app / per-MCP-server approval paths and values.
            &["-c", "apps._default.approvals_reviewer=auto_review"][..],
            &["-c", "apps.myapp.default_tools_approval_mode=approve"][..],
            &["-c", "apps.myapp.tools.mytool.approval_mode=approve"][..],
            &["-c", "mcp_servers.s.default_tools_approval_mode=approve"][..],
            &["-c", "mcp_servers.s.tools.t.approval_mode=approve"][..],
            // Structural TOML values — inline and aggregate tables.
            &["-c", "features={hooks=false}"][..],
            &["-c", "apps={_default={approvals_reviewer=\"auto_review\"}}"][..],
            &[
                "-c",
                "mcp_servers={s={default_tools_approval_mode=\"approve\"}}",
            ][..],
            &["-c", "approval_policy={granular={foo=\"never\"}}"][..],
            // TOML 1.1 forms codex applies but a TOML-1.0 parser would reject —
            // must be refused, never forwarded (differential grammar test).
            &["-c", "features={hooks=false,}"][..],
            &["-c", "features={ hooks = false ,\n}"][..],
            // TOML-key-aware decoding: a quoted owned key is still owned.
            &["-c", "\"approval_policy\"=never"][..],
            &["-c", "apps.\"my.app\".default_tools_approval_mode=approve"][..],
            &["-c", "\"hooks\".command=x"][..],
            // A10 — the sandbox dimension, one case per owned root, in the
            // spellings a `-c` can wear: dotted, quoted, structural and
            // `--config` long form.
            &["-c", "sandbox=danger-full-access"][..],
            &["-c", "sandbox_mode=danger-full-access"][..],
            &["--config", "sandbox_mode=danger-full-access"][..],
            &["-c", "sandbox_policy=danger-full-access"][..],
            &["-c", "sandbox_workspace_write.writable_roots=[\"/\"]"][..],
            &["-c", "sandbox_workspace_write.network_access=true"][..],
            &["-c", "sandbox_workspace_write={network_access=true}"][..],
            // The read-scope widening an earlier revision forwarded as benign.
            &[
                "--config",
                "sandbox_permissions=[\"disk-full-read-access\"]",
            ][..],
            &["-csandbox_permissions=[\"disk-full-read-access\"]"][..],
            // TOML-key-aware decoding on the sandbox axis: a quoted owned root,
            // and a quoted (dot-bearing) leaf under one.
            &["-c", "\"sandbox_mode\"=danger-full-access"][..],
            &["-c", "sandbox_workspace_write.\"odd.key\"=1"][..],
            // The PERMISSION-PROFILE axis. The exact pair measured ACCEPTED and
            // activated by the installed codex 0.147 (see `path_is_owned`), plus each
            // half alone and the spellings a `-c` can wear: structural, dotted, quoted
            // and `--config` long form.
            &["-c", "permissions={wide={filesystem={\"/\"=\"write\"}}}"][..],
            &["-c", "default_permissions=\"wide\""][..],
            &["--config", "default_permissions=\"wide\""][..],
            &["-cdefault_permissions=\"wide\""][..],
            &["-c", "permissions.wide.filesystem.\"/\"=\"write\""][..],
            &["-c", "\"permissions\"={wide={network=true}}"][..],
            &["-c", "\"default_permissions\"=\"wide\""][..],
            // Feature toggles.
            &["--enable", "hooks"][..],
            &["--disable", "hooks"][..],
            &["--disable", "codex_hooks"][..],
            // The feature the launch pins off, in every spelling that reaches it.
            // Both directions: what is owned is the setting, not a direction.
            &["-c", "features.request_permissions_tool=true"][..],
            &["-c", "features.request_permissions_tool=false"][..],
            &["--config", "features={request_permissions_tool=true}"][..],
            &["-c", "features.\"request_permissions_tool\"=true"][..],
            &["--enable", "request_permissions_tool"][..],
            &["--disable", "request_permissions_tool"][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::OwnedConfigKey { .. }),
                "{parts:?} should be an owned-config-key refusal"
            );
        }
    }

    /// **The refusal that costs the caller a feature says what it costs them.**
    ///
    /// Every other owned key is visibly the session's policy: somebody who reached
    /// for `sandbox` knows what they were reaching for, and "CodeConnect owns this"
    /// is the whole answer. The pinned-off feature is not like that — nothing about
    /// the key says the request it turns on has nowhere to be answered from — so the
    /// bare sentence would read as a permission problem and send the reader looking
    /// for a way around it. This pins the extra clause, in both spellings that reach
    /// it, and pins that the ordinary owned keys did NOT grow one.
    ///
    /// **Mutation:** return `None` from `why_owned` and the first two go red;
    /// return the clause for every key and the last one does.
    #[test]
    fn the_pinned_off_feature_is_refused_with_the_reason_it_is_pinned() {
        for parts in [
            &["-c", "features.request_permissions_tool=true"][..],
            &["--enable", "request_permissions_tool"][..],
        ] {
            let said = refuse(parts).to_string();
            assert!(
                said.contains("answers approvals from the phone")
                    && said.contains("only the terminal can grant"),
                "{parts:?} must say why the feature is not available: {said}"
            );
        }
        let sandbox = refuse(&["-c", "sandbox=danger-full-access"]).to_string();
        assert!(
            !sandbox.contains("answers approvals from the phone"),
            "a key that speaks for itself gets no extra clause: {sandbox}"
        );
    }

    /// **Owning one feature is not owning the namespace it lives in.**
    ///
    /// `features.*` is a large table of unrelated switches, and refusing all of it
    /// would take a launch the grammar has no reason to refuse. The three owned
    /// names are the two halves of hook enablement and the one the launch pins off;
    /// everything else forwards.
    ///
    /// **Mutation:** widen the `features` arm to `seg(1).is_some()` and these fail.
    #[test]
    fn a_feature_the_launch_does_not_own_still_forwards() {
        for parts in [
            &["-c", "features.web_search=true"][..],
            &["--enable", "web_search"][..],
            // Near neighbours of the pinned name, which are different settings.
            &["-c", "features.default_mode_request_user_input=true"][..],
            &["--enable", "exec_permission_approvals"][..],
        ] {
            assert!(
                validate_codex_argv(&argv(parts)).is_ok(),
                "{parts:?} names no setting CodeConnect owns and must forward"
            );
        }
    }

    #[test]
    fn unparsable_or_undecodable_config_overrides_fail_closed() {
        // A structured value we cannot parse, and a key whose quoting we cannot
        // decode: refused as unclassifiable (A7), never forwarded.
        for parts in [
            &["-c", "features={hooks=false"][..], // unbalanced inline table
            &["-c", "apps=[unterminated"][..],    // unbalanced array
            &["-c", "apps.\"team.prod=1"][..],    // unbalanced quoted key
            // A `[table header]` / newline injected into the KEY is not a lone
            // key expression — refused (single-key strictness).
            &["-c", "[benign]\napproval_policy=never"][..],
            &["-c", "benign]\napproval_policy"][..],
            &["-c", "a\nb=1"][..],
            // A second assignment injected past the VALUE is refused.
            &["-c", "model=1\napproval_policy=\"never\""][..],
            &["-c", "x=0\n[apps.e.tools.t]\napproval_mode=\"approve\""][..],
            // Sentinel-collision payload: a valid-TOML value that embeds an owned
            // assignment over a newline. Must be refused (no user-collidable
            // sentinel; a multi-line parse failure fails closed).
            &[
                "-c",
                "model=\"gpt-5\"\n__cc_probe__=0\napproval_policy=\"never\"",
            ][..],
            // `--enable`/`--disable` with a non-bare (quoted/escaped) feature.
            &["--enable", "\"hooks\""][..],
            &["--disable", "\"codex_hooks\""][..],
            &["--enable", "hooks.trust"][..],
            // Security checks must see the RAW, untrimmed argument.
            // (1) A table/array-header-shaped KEY whose synthetic sentinel is
            //     commented/displaced — its terminal is not the `= 0` leaf.
            &["-c", "[[benign]] #"][..],
            &["-c", "[benign] #"][..],
            &["-c", "[[x]]"][..],
            // (2) A VALUE whose leading CR/LF (which trimming would erase) embeds
            //     an owned assignment.
            &["-c", "features=\nhooks=false"][..],
            &["-c", "model=\r\napproval_policy=\"never\""][..],
            // (3) A FEATURE with edge/embedded whitespace or newline.
            &["--enable", " web_search "][..],
            &["--enable", "web_search\n"][..],
            &["--disable", " hooks "][..],
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::Unclassifiable { .. }),
                "{parts:?} should fail closed as unclassifiable"
            );
        }
    }

    #[test]
    fn unowned_config_keys_and_features_pass_through() {
        accept(&["-c", "model=o3"]);
        accept(&["-cmodel=o3"]);
        accept(&["-c", "model_reasoning_effort=high"]);
        accept(&["-c", "mcp_servers={s={command=\"x\"}}"]);
        accept(&["--enable", "some_other_feature"]);
        accept(&["--disable", "telemetry"]);
        // A differently-cased key does not reach the owned TOML key.
        accept(&["-c", "Approval_Policy=never"]);
        // An unowned TOML-1.1 structural value (trailing comma) parses and
        // forwards — the grammar matches codex, so this is not falsely refused.
        accept(&["-c", "mcp_servers={s={command=\"x\"},}"]);
        // Path-aware: an owned-*named* key that is arbitrary data in an unowned
        // container is not refused.
        accept(&["-c", "mcp_servers.s.env.approval_mode=literal"]);
        accept(&["-c", "mcp_servers.s.env={approval_mode=\"literal\"}"]);
        // An MCP server (or app) literally named after an owned control.
        accept(&["-c", "mcp_servers.hooks.command=x"]);
        accept(&["-c", "apps.notify.command=x"]);
        // The A10 sandbox roots are owned by exact **segment**, at the top level
        // only: a server or app literally named `sandbox` is still just a name.
        accept(&["-c", "mcp_servers.sandbox.command=x"]);
        accept(&["-c", "apps.sandbox_mode.command=x"]);
        // Same for the permission-profile roots: owned by exact top-level segment,
        // so an MCP server or app that happens to be NAMED
        // `permissions`/`default_permissions` still forwards. This is the over-refusal
        // direction — the new roots must not swallow the namespace.
        accept(&["-c", "mcp_servers.permissions.command=x"]);
        accept(&["-c", "apps.default_permissions.command=x"]);
        accept(&["-c", "mcp_servers.s.env.permissions=literal"]);
        accept(&["-c", "some_table={default_permissions=\"wide\"}"]);
        // `approval_policy` nested under an unowned container is not the real one.
        accept(&["-c", "some_table={approval_policy=\"never\"}"]);
        accept(&["-c", "some_table={sandbox_mode=\"danger-full-access\"}"]);
        // A direct `approval_mode` under an app/server (not under tools) is not a
        // real owned setting.
        accept(&["-c", "apps.foo.approval_mode=whatever"]);
        // TOML-key-aware decoding: a benign quoted key forwards — a server named
        // with a dot, and a single quoted key that merely looks like a dotted
        // owned path.
        accept(&["-c", "mcp_servers.\"some.server\".command=x"]);
        accept(&["-c", "\"hooks.command\"=x"]);
        // Quote-aware key precheck: `[`/`]`/`#` are legal inside a quoted segment,
        // so a server named `prod[#1]` forwards (they are not blanket-rejected).
        accept(&["-c", "mcp_servers.\"prod[#1]\".command=x"]);
        // Benign bare feature names forward.
        accept(&["--enable", "web_search"]);
        accept(&["--disable", "telemetry"]);
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
        ] {
            assert!(
                matches!(refuse(&[name]), CodexRefusal::Subcommand { .. }),
                "`codex {name}` should be refused as a subcommand"
            );
        }
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

    /// **The two guarded surfaces must agree, in BOTH directions.**
    ///
    /// This is the seam between the launch gate and the argv grammar. The gate proves
    /// the installed binary's command set still equals a vendored reference; this proves
    /// the references are fully covered by the refusal table, and — the direction that
    /// matters for the class the projection cannot see — that every token in the refusal
    /// table is either enumerated by a reference or explicitly listed as a hidden alias.
    ///
    /// Without the second direction a dispatchable root token could sit in NEITHER: not
    /// in the completion script (so the argv diff never sees it appear) and not in the
    /// refusal table (so `validate_codex_argv` forwards it as prompt text). `cloud-tasks`
    /// is that class, measured: it dispatches on both binaries and appears in no
    /// enumeration either of them emits. Listing it in
    /// [`codex_broker::guarded_surface::HIDDEN_ROOT_ALIASES`] is what makes it a
    /// reviewed fact instead of a token that happens to be here, and
    /// `the_hidden_root_aliases_still_dispatch_and_are_still_hidden` re-measures both
    /// halves of that claim against live binaries.
    #[test]
    fn every_dispatchable_root_token_is_accounted_for() {
        use codex_broker::guarded_surface as gs;
        let mut known: std::collections::BTreeSet<String> = gs::baseline_argv().subcommands;
        known.extend(gs::grounded_argv().subcommands);
        for name in &known {
            assert!(
                is_subcommand(name),
                "a vendored argv reference names `{name}` as a root subcommand, but the \
                 refusal table does not know it — it would be forwarded as prompt text \
                 and codex would dispatch it"
            );
        }
        for token in ROOT_SUBCOMMANDS {
            assert!(
                known.contains(token) || gs::HIDDEN_ROOT_ALIASES.contains(&token),
                "the refusal table refuses `{token}`, which no vendored reference \
                 enumerates and HIDDEN_ROOT_ALIASES does not claim — so nothing measures \
                 whether it still dispatches, and its siblings could be missing here \
                 without anything noticing"
            );
        }
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
        for parts in [&["resume"][..], &["--yolo"][..], &["--unknown-flag"][..]] {
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
        // Dispatches `zzz-future-alias` — a token in neither ROOT_SUBCOMMANDS,
        // HIDDEN_ROOT_ALIASES, nor either vendored argv reference.
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
    /// `-i a.png features`. Our walk still believes `-i` is greedy — nothing gates
    /// option arity, and the guarded-surface gate compares spellings, not how many
    /// values a flag eats — so under the old passthrough it swept both tokens, saw no
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
            refuse(&["-m", "gpt", "resume"]),
            CodexRefusal::Subcommand { .. }
        ));
        assert!(matches!(
            refuse(&["--model=gpt", "fork"]),
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
    fn a_hidden_global_flag_cannot_smuggle_a_subcommand() {
        // `codex --psp resume` dispatches Resume; the recognised hidden global
        // must not shield it.
        assert!(matches!(
            refuse(&["--psp", "resume"]),
            CodexRefusal::Subcommand { .. }
        ));
        // An unknown flag fails closed up front (allowlist) — it never reaches
        // subcommand detection, so it can neither ride through nor smuggle one.
        assert!(matches!(
            refuse(&["--not-a-real-flag", "resume"]),
            CodexRefusal::Unclassifiable { .. }
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
            refuse(&["-i", "a", "--cd", "/x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
        // Attached `--image=a` / `-ia` takes exactly one value (grounded:
        // `codex --image=a b c` parses `b` as prompt and errors on `c` as a
        // subcommand). So a following prompt forwards, and a following subcommand
        // name is refused exactly as codex would dispatch it.
        accept(&["--image=a", "b"]);
        accept(&["-ia", "b"]);
        assert!(matches!(
            refuse(&["--image=a", "resume"]),
            CodexRefusal::Subcommand { .. }
        ));
        assert!(matches!(
            refuse(&["-ia", "fork"]),
            CodexRefusal::Subcommand { .. }
        ));
    }

    #[test]
    fn a_value_option_does_not_swallow_a_flag_shaped_follower() {
        // Grounded: `codex --model --yolo` is a missing-value error, and `--yolo`
        // is parsed as a flag — so the forbidden follower must reach our refusal,
        // never ride through as the option's value.
        assert!(matches!(
            refuse(&["--model", "--yolo"]),
            CodexRefusal::ApprovalControl { .. }
        ));
        assert!(matches!(
            refuse(&["-m", "-aon-request"]),
            CodexRefusal::ApprovalControl { .. }
        ));
        assert!(matches!(
            refuse(&["--enable", "--approve-for-me"]),
            CodexRefusal::ApprovalControl { .. }
        ));
        assert!(matches!(
            refuse(&["--local-provider", "--config=approval_policy=never"]),
            CodexRefusal::OwnedConfigKey { .. }
        ));
        assert!(matches!(
            refuse(&["--config", "--cd", "/x"]),
            CodexRefusal::OwnedFlag { .. }
        ));
        // A benign flag follower is still just the next flag; the value-option had
        // no value (codex would error), but nothing forbidden rode through.
        accept(&["--model", "--search"]);
    }

    #[test]
    fn short_clusters_are_fully_expanded_and_cannot_smuggle_an_owned_key() {
        // A bool short in front of a value short must not discard the suffix: the
        // `-c approval_policy=never` inside `-hcapproval_policy=never` is refused.
        assert!(matches!(
            refuse(&["-hcapproval_policy=never"]),
            CodexRefusal::OwnedConfigKey { .. }
        ));
        assert!(matches!(
            refuse(&["-Vcapproval_policy=never"]),
            CodexRefusal::OwnedConfigKey { .. }
        ));
        // A cluster of only bool shorts forwards.
        accept(&["-hV"]);
        accept(&["-h"]);
        // A value short attached after a bool short still consumes its own value.
        assert!(matches!(refuse(&["-hC."]), CodexRefusal::OwnedFlag { .. }));
    }

    #[test]
    fn repeated_owned_and_neutral_flags_are_handled() {
        assert!(matches!(
            refuse(&["-c", "model=o3", "-c", "approval_policy=never"]),
            CodexRefusal::OwnedConfigKey { .. }
        ));
        accept(&["-c", "model=o3", "-c", "reasoning_effort=high"]);
    }

    #[test]
    fn the_benign_flag_allowlist_forwards_only_known_flags() {
        // Every benign interactive flag from `codex --help` (plus a prompt) is
        // forwarded.
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
        // A recognised hidden global on its own, with a prompt, is neutral.
        accept(&["--psp", "fix the build"]);
        // A realistic benign invocation: a prompt plus a known flag.
        accept(&["-m", "gpt-5", "fix the flaky test"]);
    }

    #[test]
    fn unknown_flags_fail_closed_under_the_allowlist() {
        // A7 allowlist: anything flag-shaped that is not on the known list is
        // refused, not forwarded — an unknown long flag, an unknown short flag,
        // and a short cluster with an unknown character.
        for parts in [
            &["--not-a-real-flag"][..],
            &["--future-approval-flag", "x"][..],
            &["--typo"][..],
            &["-Z"][..],
            &["-hq"][..], // -h known, q unknown -> fail closed
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::Unclassifiable { .. }),
                "{parts:?} should fail closed under the allowlist"
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
        assert!(matches!(
            refuse(&["--cd", "/x", "--", "prompt"]),
            CodexRefusal::OwnedFlag { .. }
        ));
    }

    #[test]
    fn refusal_messages_name_what_and_why() {
        assert!(refuse(&["--remote", "x"]).to_string().contains("--remote"));
        assert!(refuse(&["-p", "work"]).to_string().contains("--profile"));
        assert!(refuse(&["--yolo"])
            .to_string()
            .contains("approval and hook-trust"));
        assert!(refuse(&["-a", "never"])
            .to_string()
            .contains("--ask-for-approval"));
        assert!(refuse(&["-c", "approval_policy=never"])
            .to_string()
            .contains("approval_policy"));
        assert!(refuse(&["-sread-only"])
            .to_string()
            .contains("the session sandbox policy"));
        assert!(refuse(&["--add-dir", "/repo"])
            .to_string()
            .contains("writable roots"));
        assert!(
            refuse(&["-c", "sandbox_workspace_write.writable_roots=[\"/\"]"])
                .to_string()
                .contains("sandbox_workspace_write.writable_roots")
        );
        assert!(refuse(&["-c", "features={hooks=false}"])
            .to_string()
            .contains("hooks"));
        assert!(refuse(&["resume"]).to_string().contains("resume"));
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
