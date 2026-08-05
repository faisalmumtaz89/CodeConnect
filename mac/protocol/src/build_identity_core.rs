// Shared between `build.rs` (via include!) and the `build_identity` module:
// the ship-source fingerprint must be computed by ONE piece of code, because
// the entire advisory rests on comparing a fingerprint embedded at build
// time against one computed at launch time — two implementations would drift
// and the comparison would lie.
//
// Everything here is deliberately dependency-light (std + sha2) and never
// panics: an identity that cannot be established is `None`, and every caller
// treats `None` as "make no claim".

use std::path::Path;
use std::time::{Duration, Instant};

/// The Mac ship-source set, repo-relative. Exactly what `install.sh` builds:
/// a change outside this set (iOS, docs, soak) is not a reason to rebuild
/// the Mac binaries, and must not trigger the advisory.
const SHIP_SOURCE_PATHS: &[&str] = &[
    "mac/Cargo.toml",
    "mac/Cargo.lock",
    "mac/protocol",
    "mac/codeconnect",
    "mac/ccd",
    "mac/cc-hook",
];

/// Bounds. A repo that trips these gets no identity claim rather than a slow
/// launch or an unbounded read: 4096 files and 64 MiB are far above the real
/// tree (~200 files, ~2 MiB), so hitting them means something is wrong.
const MAX_FINGERPRINT_FILES: usize = 4096;
const MAX_FINGERPRINT_BYTES: u64 = 64 * 1024 * 1024;

/// Wall-clock budget for one whole identity computation at launch.
/// `codeconnect claude` runs this on every start; a git that hangs on a
/// stale lock must cost the user well under a second, not a wait.
const LOCAL_COMPARISON_BUDGET: Duration = Duration::from_millis(750);

/// A deadline that `None` disarms (build time is allowed to be slow; launch
/// time is not).
#[derive(Clone, Copy)]
pub(crate) struct Budget {
    pub(crate) deadline: Option<Instant>,
}

impl Budget {
    /// Exercised by `build.rs` and the tests; the library's own non-test
    /// compilation has no caller — the include! sharing makes deadness
    /// contextual, not real.
    #[allow(dead_code)]
    pub(crate) fn unbounded() -> Self {
        Budget { deadline: None }
    }

    pub(crate) fn launch() -> Self {
        Budget {
            deadline: Some(Instant::now() + LOCAL_COMPARISON_BUDGET),
        }
    }

    pub(crate) fn expired(&self) -> bool {
        self.deadline.is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// A full git object id: 40 (SHA-1) or 64 (SHA-256 repos) lowercase hex.
/// Anything else — capitalised, short, or decorated — is not an identity
/// this module will compare against.
pub(crate) fn is_valid_commit(id: &str) -> bool {
    (id.len() == 40 || id.len() == 64)
        && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Run git and return stdout on success, or `None` for any failure at all.
/// See [`run_git_exit`] for the callers that need to tell "git answered
/// no" apart from "git gave no answer".
pub(crate) fn run_git(repo_root: &Path, args: &[&str], budget: Budget) -> Option<Vec<u8>> {
    let (code, output) = run_git_exit(repo_root, args, budget)?;
    (code == 0).then_some(output)
}

/// Exit code plus stdout when git actually RAN; `None` when it could not be
/// run or answer — missing binary, spawn failure, deadline breach, killed by
/// signal. The two are different facts: a non-zero exit is git *answering*
/// (not-an-ancestor, unknown object), and no answer at all must never be
/// dressed up as one. The child is killed on breach and its stdout drained
/// on a thread so a large listing cannot deadlock the pipe.
pub(crate) fn run_git_exit(
    repo_root: &Path,
    args: &[&str],
    budget: Budget,
) -> Option<(i32, Vec<u8>)> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    if budget.expired() {
        return None;
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if budget.expired() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let output = reader.join().ok()?;
    // Killed by a signal is not an answer.
    Some((status.code()?, output))
}

/// Every ship-source path git knows about: tracked plus non-ignored
/// untracked, sorted and deduplicated. Tracked-but-deleted files stay in the
/// list — their absence is part of the tree's identity.
pub(crate) fn ship_source_files(repo_root: &Path, budget: Budget) -> Option<Vec<String>> {
    let mut args = vec![
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--",
    ];
    args.extend_from_slice(SHIP_SOURCE_PATHS);
    let listing = run_git(repo_root, &args, budget)?;
    let mut files: Vec<String> = listing
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect();
    files.sort();
    files.dedup();
    if files.len() > MAX_FINGERPRINT_FILES {
        return None;
    }
    Some(files)
}

/// SHA-256 over the working state of the ship-source set: each path with its
/// kind (file/executable/symlink/absent) and exact contents or target.
/// Length-prefixed fields, so no crafted path or content can make two
/// different trees serialize identically.
pub(crate) fn fingerprint_ship_source(repo_root: &Path, budget: Budget) -> Option<String> {
    use sha2::{Digest, Sha256};

    let files = ship_source_files(repo_root, budget)?;
    let mut hasher = Sha256::new();
    let mut total_bytes: u64 = 0;

    let feed = |hasher: &mut Sha256, field: &[u8]| {
        hasher.update(field.len().to_le_bytes());
        hasher.update(field);
    };

    for relative in &files {
        if budget.expired() {
            return None;
        }
        feed(&mut hasher, relative.as_bytes());
        let absolute = repo_root.join(relative);
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(_) => {
                // Tracked but deleted from the working tree: a real state,
                // hashed as such rather than skipped.
                feed(&mut hasher, b"absent");
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            feed(&mut hasher, b"link");
            let target = std::fs::read_link(&absolute).ok()?;
            feed(&mut hasher, target.as_os_str().as_encoded_bytes());
            continue;
        }
        if !metadata.is_file() {
            // A directory or oddity under a tracked name; identity cannot be
            // established honestly.
            return None;
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        feed(&mut hasher, if executable { b"exec" } else { b"file" });

        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > MAX_FINGERPRINT_BYTES {
            return None;
        }
        let contents = std::fs::read(&absolute).ok()?;
        feed(&mut hasher, &contents);
    }

    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    Some(out)
}

/// `git status --porcelain` over the ship-source set: any line at all —
/// modified, staged, or untracked — means the working state is not a plain
/// checkout of HEAD.
pub(crate) fn ship_source_dirty(repo_root: &Path, budget: Budget) -> Option<bool> {
    let mut args = vec!["status", "--porcelain", "--"];
    args.extend_from_slice(SHIP_SOURCE_PATHS);
    let output = run_git(repo_root, &args, budget)?;
    Some(!output.is_empty())
}

/// HEAD's full object id, validated.
///
/// Same contextual-deadness note as [`Budget::unbounded`]: `build.rs` is a
/// real caller, the library's tests are the other.
#[allow(dead_code)]
pub(crate) fn checkout_head(repo_root: &Path, budget: Budget) -> Option<String> {
    let output = run_git(repo_root, &["rev-parse", "HEAD"], budget)?;
    let head = String::from_utf8_lossy(&output).trim().to_string();
    is_valid_commit(&head).then_some(head)
}
