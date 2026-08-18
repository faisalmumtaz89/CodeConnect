//! The `codeconnect codex` launcher foundation: binary resolution and the
//! reserved argv grammar.
//!
//! This is the pure, heavily-tested front of the Codex launcher. It resolves the
//! `codex` executable exactly as `claude` is resolved (config → env → well-known
//! → `PATH`, with the same self-resolution guard so an
//! `alias codex=codeconnect codex` cannot spawn-loop), requires the resolved file
//! to be the **native standalone executable** and not a `#!`-script / `.js`
//! wrapper (which could swap the real CLI out from under a pinned path), pins the
//! resolved binary to a compiled-in tested-version set, and parses the user's
//! argv against the reserved grammar that keeps CodeConnect the sole owner of the
//! launch's transport, working directory, profile and approval policy. The `-c`
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

/// A resolved `codex` executable.
///
/// [`path`](Self::path) is the fully-canonicalised versioned executable: the
/// invocation candidate with every symlink resolved. On a standalone install the
/// invocation hops through a moving `standalone/current` symlink to a
/// version-stamped release directory (A1). Resolution canonicalises **once** and
/// fails closed if it cannot, and this single path is what gets version-checked
/// and — in later chunks — recorded as launch evidence and exec'd by the wrapper,
/// app-server and TUI alike, so a `standalone/current` flip cannot make the
/// recorded, checked and executed binaries disagree (CODEX-PLAN.md launch
/// coordination; "all spawned Codex processes use the same resolved executable").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCodex {
    pub path: PathBuf,
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
    let version = read_codex_version(&resolved.path)?;
    ensure_pinned_version(&version)?;

    // Reserved grammar. A refused flag or subcommand surfaces here, naming what
    // was refused and why, before the gate.
    validate_codex_argv(passthrough).map_err(|refusal| anyhow!("{refusal}"))?;

    // The gate. Resolution and parsing above are wired and exercised; the launch
    // itself is withheld until the wrapper and the Phase-2 pre-exposure gates
    // land. A later chunk removes this line.
    bail!(
        "codex support is not yet enabled in this build; \
         resolved {} ({}), arguments accepted, but the launcher is still gated",
        resolved.path.display(),
        version
    );
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

    // If the only thing found is a wrapper, the error names it — the supported
    // install is the standalone native binary, not a script/JS shim.
    let mut wrapper_seen: Option<PathBuf> = None;

    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        // Canonicalise once. `is_file` already followed the symlink to a real
        // file, so a failure here is a race or a permission fault — fail closed
        // rather than exec an executable we cannot pin an identity to.
        //
        // A7 PRE-UNGATE (tracked for the launch/exec chunk, not fixed here — the
        // command is gated so this is not exploitable now): canonicalisation pins
        // a **pathname**, not a **file identity**. The race is wider than
        // check→launch: the version-check below already spawns this pathname, so
        // the window spans magic-check → `codex --version` exec → the later
        // app-server/TUI spawns, and a `standalone/current` flip or same-path
        // replacement at any point could swap the executable. The native-Mach-O
        // check is also not sufficient: it proves "a native executable", not
        // "standalone Codex" — a compiled native dispatcher would pass it. Before
        // ungating, bind to the file's identity (open a handle here and exec by
        // that handle / verify identity at spawn) AND verify the standalone
        // package layout, so every spawned Codex process is provably the one we
        // version-pinned.
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
        if !is_native_executable(&canonical) {
            wrapper_seen.get_or_insert(canonical);
            continue;
        }
        return Ok(ResolvedCodex { path: canonical });
    }
    match wrapper_seen {
        Some(wrapper) => bail!(
            "the codex at {} is a wrapper, not a native executable; \
             CodeConnect supports the standalone native codex \
             (e.g. ~/.local/bin/codex → …/standalone/releases/…/bin/codex)",
            wrapper.display()
        ),
        None => {
            bail!("could not find the codex binary; set codex_bin in ~/.codeconnect/config.json")
        }
    }
}

/// Whether the file at `path` is a native Mach-O executable (thin or universal),
/// as opposed to a `#!`-script or `.js` wrapper. Read as the first four bytes and
/// matched against the Mach-O / universal-binary magic numbers.
fn is_native_executable(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        u32::from_be_bytes(magic),
        // Mach-O 32/64-bit, big- and little-endian (arm64 native is 0xCFFAEDFE).
        0xFEED_FACE | 0xFEED_FACF | 0xCEFA_EDFE | 0xCFFA_EDFE
        // Universal ("fat") binaries, 32- and 64-bit.
        | 0xCAFE_BABE | 0xBEBA_FECA | 0xCAFE_BABF | 0xBFBA_FECA
    )
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

/// Run `codex --version` and return the parsed version string.
fn read_codex_version(bin: &Path) -> Result<String> {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", bin.display()))?;
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
    /// (`-C`/`--cd`).
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
        Some("approval_policy") | Some("approvals_reviewer") | Some("hooks") | Some("notify")
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
            is_native_executable(&resolved.path),
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
        assert!(!is_native_executable(&wrapper));

        // A file carrying Mach-O 64-bit little-endian magic is treated as native.
        let native = root.join("codex-native");
        std::fs::write(&native, [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0]).unwrap();
        make_executable(&native);
        assert!(is_native_executable(&native));

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
                assert!(is_native_executable(&resolved.path));
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
        let version = read_codex_version(&resolved.path).expect("codex --version must run");
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
        accept(&[
            "--config",
            "sandbox_permissions=[\"disk-full-read-access\"]",
        ]);
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
        // `approval_policy` nested under an unowned container is not the real one.
        accept(&["-c", "some_table={approval_policy=\"never\"}"]);
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
        accept(&["-s", "read-only"]);
        accept(&["--sandbox=workspace-write"]);
        accept(&["--add-dir", "/repo"]);
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
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
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
}
