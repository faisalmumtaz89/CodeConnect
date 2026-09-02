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
//! **The command is gated.** Per the plan's pre-exposure gates (A5), the `codex`
//! command may not actually launch a session until the wrapper and the two
//! Phase-2 pre-exposure gates land. So [`start`] does the real work — resolve,
//! version-pin, argv-validate, surfacing every one of those failures honestly —
//! and then refuses with a "not yet enabled" message instead of spawning
//! anything. A later chunk removes that final refusal.
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
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use protocol::config::{self, Config, CODEX_PINNED_VERSIONS};

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
/// reserved grammar, and then — because the command is gated ahead of the
/// wrapper and the pre-exposure gates — refuses to launch. Every earlier step
/// can fail with its own honest error; only a fully-valid invocation reaches the
/// gate.
pub fn start(passthrough: &[String]) -> Result<()> {
    let config = Config::load();

    // Binary first, exactly as the Claude path resolves its binary first: a
    // missing or untested executable must surface before anything else. The
    // canonicalised path is what we version-check and would exec.
    let resolved = resolve_codex_bin(&config)?;
    let version = read_codex_version(&resolved)?;
    ensure_pinned_version(&version)?;

    // Reserved grammar. A refused flag or subcommand surfaces here, naming what
    // was refused and why, before the gate.
    validate_codex_argv(passthrough).map_err(|refusal| anyhow!("{refusal}"))?;

    // Daemon preflight, before anything is created.
    refuse_unless_hostable(crate::daemon::agent_support(
        &protocol::agent::AgentKind::Codex,
    ))?;

    // The gate. Resolution and parsing above are wired and exercised; the launch
    // itself is withheld until the wrapper and the Phase-2 pre-exposure gates
    // land. A later chunk removes this line.
    bail!(
        "codex support is not yet enabled in this build; \
         resolved {} ({}, sha256 {}), arguments accepted, but the launcher is still gated",
        resolved.path.display(),
        version,
        resolved.sha256
    );
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
/// **This is an explicit ungate blocker awaiting an owner decision.** No layout
/// verifier is invented here: "e.g." in the plan is an example rather than a
/// specification, and picking one unilaterally would silently narrow which installs
/// CodeConnect supports — a scoping decision, not an implementation detail. Codex
/// must not be ungated until the owner rules on what a supported install is and how
/// it is verified.
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
pub(crate) fn verify_codex_identity(
    path: &Path,
    expected: &str,
    when: &str,
) -> Result<protocol::hash::FrozenExecutable> {
    let (actual, frozen) = protocol::hash::freeze_and_hash(path).with_context(|| {
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
fn read_codex_version(resolved: &ResolvedCodex) -> Result<String> {
    let bin = resolved.path.as_path();
    // Freeze + verify BEFORE the exec, and hold the freeze across it. This exec used
    // to be the one A7.1 site that could only *detect* a swap after the fact — it ran
    // pre-gate, so a replacement landing in front of it ran as `codex --version`
    // before anything checked. Now the bytes are pinned immutable and verified first,
    // and stay frozen while the child runs, so the version pinned below is reported
    // by the pinned bytes and no unverified binary is reachable here at all.
    let frozen = verify_codex_identity(bin, &resolved.sha256, "before `codex --version`")?;
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

/// Refuse a resolved version outside the compiled-in tested set, naming the set.
///
/// The allowlist is compiled in ([`CODEX_PINNED_VERSIONS`]), never configuration:
/// Codex's app-server protocol is experimental, so the only guarantee CodeConnect
/// can make is for the builds it was actually tested against.
fn ensure_pinned_version(version: &str) -> Result<()> {
    if config::is_pinned_codex_version(version) {
        return Ok(());
    }
    bail!(
        "codex {version} is not a tested version; \
         this build of CodeConnect supports codex {}",
        CODEX_PINNED_VERSIONS.join(", ")
    )
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
            CodexRefusal::OwnedConfigKey { key, via } => write!(
                f,
                "`{via} {key}` is refused: `{key}` is a configuration key CodeConnect owns for the session"
            ),
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
    let mut i = 0;

    while i < args.len() {
        match classify(&args[i]) {
            // Everything after `--` is prompt content: forwarded verbatim, never
            // interpreted as a flag or a subcommand.
            Token::Boundary => break,

            Token::Flag { flag, attached } => match flag.arity {
                Arity::Bool => {
                    refuse_bool(flag.canonical)?;
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
                }
                Arity::Values => {
                    // `-i`/`--image`. Grounded on 0.147: the **spaced** form is
                    // greedy (`-i a b c` ⇒ three image paths), but the **attached**
                    // form (`--image=a`, `-ia`) takes exactly that one value —
                    // `--image=a b c` parses `b` as the prompt and `c` as a
                    // subcommand slot. So the greedy sweep runs only for the spaced
                    // form; after an attached value the following tokens are real
                    // positionals and a subcommand among them is refused, exactly
                    // as codex would dispatch it.
                    i += 1;
                    if attached.is_none() {
                        while i < args.len() && !looks_like_flag(&args[i]) {
                            i += 1;
                        }
                    }
                }
            },

            // A cluster of only bool short-flags (`-hV`): forwarded as-is.
            Token::BoolCluster => {
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
                i += 1;
            }
        }
    }
    Ok(())
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
///     an owned root is the whole finding — so an unenforced or renamed spelling
///     costs an over-refusal (acceptable under A7), never an escape;
///   * the top-level **permission-profile** controls (round-4 finding 1) —
///     `permissions` and `default_permissions` — owned at and below their root.
///     These are a SECOND, independent sandbox channel, not a spelling of the
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
        Some("features") => matches!(seg(1), Some("hooks") | Some("codex_hooks")),
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
    /// A hook feature CodeConnect owns.
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
/// bare owned hook features are refused.
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
    if matches!(feature, "hooks" | "codex_hooks") {
        return FeatureVerdict::Owned;
    }
    FeatureVerdict::Benign
}

/// Whether a bare positional token is a codex subcommand name or alias.
///
/// The full 0.147 top-level command set, enumerated from clap's own completion
/// output, including the **hidden** commands (`execpolicy`, `responses-api-proxy`,
/// `stdio-to-uds`) and both the visible aliases (`e` for `exec`, `a` for `apply`)
/// and the hidden alias (`cloud-tasks` for `cloud`). Matching a positional token
/// against this set is exactly how codex resolves a subcommand: `codex resume`
/// and `codex please resume` both dispatch Resume, never a prompt of the word.
fn is_subcommand(token: &str) -> bool {
    matches!(
        token,
        "exec"
            | "e"
            | "review"
            | "login"
            | "logout"
            | "mcp"
            | "plugin"
            | "mcp-server"
            | "app-server"
            | "remote-control"
            | "app"
            | "completion"
            | "update"
            | "doctor"
            | "sandbox"
            | "debug"
            | "execpolicy"
            | "apply"
            | "a"
            | "resume"
            | "archive"
            | "delete"
            | "unarchive"
            | "fork"
            | "cloud"
            | "cloud-tasks"
            | "responses-api-proxy"
            | "stdio-to-uds"
            | "exec-server"
            | "features"
            | "help"
    )
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
        verify_codex_identity(&resolved.path, &resolved.sha256, "in the unchanged case")
            .expect("an untouched binary must verify");

        // The swap. Same path, same canonical name, different bytes — and still a
        // perfectly valid native Mach-O, so the magic check alone would wave it
        // through. Only the digest sees it.
        std::fs::write(&bin, [0xCFu8, 0xFA, 0xED, 0xFE, b'n', b'e', b'w']).unwrap();
        let err = verify_codex_identity(
            &resolved.path,
            &resolved.sha256,
            "immediately before the app-server spawn",
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
        assert!(
            verify_codex_identity(&resolved.path, &resolved.sha256, "after truncation").is_err()
        );

        // And a file that is gone is a refusal, never a pass: "I could not check"
        // and "it is unchanged" must not share an answer.
        std::fs::remove_file(&bin).unwrap();
        let err = verify_codex_identity(&resolved.path, &resolved.sha256, "after deletion")
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

    /// **Finding 1 at the resolution site.** The verification sites are not the only
    /// place a 220 MB read happens: resolution takes one too, and the digest it mints
    /// is the pin everything downstream compares against. A rename landing inside
    /// *that* read produces a `ResolvedCodex` whose digest describes bytes the
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
    /// Round 2: the refusal now lands **before** the exec rather than after it. The
    /// bytes are frozen and verified first, so a binary that does not match the pin
    /// never runs as `codex --version` at all — the pre-gate exec of unpinned bytes
    /// that finding 1 named is gone. The `when` assertion below is what pins that
    /// ordering.
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

    #[test]
    fn the_pinned_version_is_accepted_and_others_are_refused_by_name() {
        assert!(ensure_pinned_version("0.147.0").is_ok());
        let err = ensure_pinned_version("0.148.0").unwrap_err().to_string();
        assert!(err.contains("0.148.0"), "names the rejected version: {err}");
        assert!(err.contains("0.147.0"), "names the tested set: {err}");
    }

    #[test]
    fn the_live_binary_reports_a_pinned_version() {
        if !on_path("codex") {
            eprintln!("skipped: no `codex` on PATH — nothing to version-check");
            return;
        }
        let resolved = resolve_codex_bin(&Config::default()).unwrap();
        let version = read_codex_version(&resolved).expect("codex --version must run");
        ensure_pinned_version(&version)
            .unwrap_or_else(|e| panic!("installed codex {version} is unpinned: {e}"));
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
            // Round-4 finding 1 — the PERMISSION-PROFILE axis. The exact pair
            // measured ACCEPTED and activated by the installed codex 0.147 (see
            // `path_is_owned`), plus each half alone and the spellings a `-c`
            // can wear: structural, dotted, quoted and `--config` long form.
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
        ] {
            assert!(
                matches!(refuse(parts), CodexRefusal::OwnedConfigKey { .. }),
                "{parts:?} should be an owned-config-key refusal"
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
            // Round-7: security checks must see the RAW, untrimmed argument.
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
        // container is not refused (finding 5).
        accept(&["-c", "mcp_servers.s.env.approval_mode=literal"]);
        accept(&["-c", "mcp_servers.s.env={approval_mode=\"literal\"}"]);
        // An MCP server (or app) literally named after an owned control.
        accept(&["-c", "mcp_servers.hooks.command=x"]);
        accept(&["-c", "apps.notify.command=x"]);
        // The A10 sandbox roots are owned by exact **segment**, at the top level
        // only: a server or app literally named `sandbox` is still just a name.
        accept(&["-c", "mcp_servers.sandbox.command=x"]);
        accept(&["-c", "apps.sandbox_mode.command=x"]);
        // Same for the permission-profile roots (round-4 finding 1): owned by
        // exact top-level segment, so an MCP server or app that happens to be
        // NAMED `permissions`/`default_permissions` still forwards. This is the
        // over-refusal direction — the new roots must not swallow the namespace.
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
