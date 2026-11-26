//! `get_diff` — what has this agent actually changed?
//!
//! The event log knows every edit that was *attempted*, including the ones the
//! agent later undid. Only the working tree knows the net result, so this asks
//! git rather than replaying the log. It is a read-only observation of a
//! directory the daemon already tracks: the client names a session, never a
//! path, so no request can point this at somewhere else on the filesystem.
//!
//! Everything degrades to a `note` rather than an error. "This is not a git
//! repository" and "nothing has changed" produce the same empty diff and must
//! not look the same on the phone — an empty screen with no explanation is the
//! failure mode this exists to avoid.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// launchd gives the daemon no PATH, so git is looked for where it lives.
const GIT_CANDIDATES: &[&str] = &[SHIM_GIT, "/opt/homebrew/bin/git", "/usr/local/bin/git"];

/// On every Mac, but on one without Apple's developer tools it is their
/// installer's stub: running it opens the dialog offering to install them. It
/// counts only when the tools are there with a git for it to run.
const SHIM_GIT: &str = "/usr/bin/git";

/// Ceiling for a `rev-parse` probe. Its answer is one word.
const PROBE_CAP: usize = 4 * 1024;

/// Ceiling for the untracked-file listing. Names, not contents.
const UNTRACKED_CAP: usize = 64 * 1024;

pub struct Diff {
    pub unified: String,
    pub truncated: bool,
    pub note: Option<String>,
    /// Every changed file, whole even when `unified` is cut; `None` when git could not list them.
    pub files: Option<Vec<protocol::ws::DiffFile>>,
    /// `files` was cut at [`protocol::ws::MAX_DIFF_FILES`].
    pub files_truncated: bool,
}

pub fn git_bin(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = configured.map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    GIT_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file() && (path != Path::new(SHIM_GIT) || shim_has_git()))
}

/// Whether Apple's developer tools are installed with a git for `/usr/bin/git`
/// to run. Asked once per daemon: `xcode-select -p` names their folder, and
/// without them it fails and opens nothing.
#[cfg(target_os = "macos")]
fn shim_has_git() -> bool {
    static HAS_GIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS_GIT.get_or_init(|| {
        crate::spawn::output_std(
            std::process::Command::new("/usr/bin/xcode-select")
                .arg("-p")
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .ok()
        .filter(|out| out.status.success())
        .is_some_and(|out| {
            Path::new(String::from_utf8_lossy(&out.stdout).trim())
                .join("usr/bin/git")
                .is_file()
        })
    })
}

/// Elsewhere `/usr/bin/git` is git itself.
#[cfg(not(target_os = "macos"))]
fn shim_has_git() -> bool {
    true
}

/// Collect `git diff HEAD` plus the untracked files, capped at `max_bytes`.
pub async fn collect(
    cwd: &str,
    configured_bin: Option<&str>,
    max_bytes: usize,
    timeout: Duration,
) -> Diff {
    match collect_inner(cwd, configured_bin, max_bytes, timeout).await {
        Ok(diff) => diff,
        // A diff that cannot be produced is a note, not a protocol error: the
        // phone should say why and carry on, not show a failed request.
        Err(err) => note_only(format!("could not read the diff: {err:#}")),
    }
}

/// The branch checked out in `cwd`; `None` outside a repository, on a detached
/// head, or when git cannot answer within `timeout`.
pub async fn branch(cwd: &str, configured_bin: Option<&str>, timeout: Duration) -> Option<String> {
    let git = git_bin(configured_bin)?;
    let out = run(
        &git,
        cwd,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    let name = out.stdout.trim();
    (out.ok && !name.is_empty()).then(|| name.to_string())
}

/// The top folder of the work tree `cwd` is in, as git names it; `None` outside a
/// repository, or when git cannot answer within `timeout`.
pub async fn toplevel(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Option<PathBuf> {
    let git = git_bin(configured_bin)?;
    let out = run(
        &git,
        cwd,
        &["rev-parse", "--show-toplevel"],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    let top = out.stdout.trim_end_matches('\n');
    (out.ok && !out.stdout_truncated && !top.is_empty()).then(|| PathBuf::from(top))
}

/// A working tree as the session list shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeFacts {
    pub branch: Option<String>,
    pub diff_stat: Option<protocol::event::DiffStat>,
}

/// The branch and line counts of the working tree at `cwd`, each `None` when git
/// cannot say.
pub async fn tree_facts(cwd: &str, configured_bin: Option<&str>, timeout: Duration) -> TreeFacts {
    TreeFacts {
        branch: branch(cwd, configured_bin, timeout).await,
        diff_stat: diff_stat(cwd, configured_bin, timeout).await,
    }
}

/// The sum of [`files`]' counted lines: what the open diff's files add up to. A file with
/// no count (binary, not a regular file, past the reading budget) adds nothing, as a binary
/// change always has. `None` outside a repository, when git cannot list the tracked files,
/// and when the untracked names passed [`LISTING_CAP`]: files left out would be lines left out.
pub async fn diff_stat(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Option<protocol::event::DiffStat> {
    let git = git_bin(configured_bin)?;
    let has_head = run(
        &git,
        cwd,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?
    .ok;
    let listed = files(&git, cwd, has_head, timeout).await?;
    if listed.truncated {
        return None;
    }
    Some(protocol::event::DiffStat {
        added: listed.files.iter().filter_map(|file| file.added).sum(),
        removed: listed.files.iter().filter_map(|file| file.removed).sum(),
    })
}

/// Bytes read from untracked files, all of them together, to count their lines.
const UNTRACKED_READ_BUDGET: u64 = 4 * 1024 * 1024;

/// Git's own test for a binary file: a NUL in its first 8000 bytes.
const BINARY_PROBE: u64 = 8000;

/// The untracked files, from the repository's top, with no configured program run.
const UNTRACKED_ARGS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "ls-files",
    "-z",
    "--others",
    "--exclude-standard",
    "--full-name",
    "--",
    ":/",
];

/// A working tree's changed files.
struct Files {
    files: Vec<protocol::ws::DiffFile>,
    /// The untracked listing passed [`LISTING_CAP`]: `files` holds the names that fit.
    truncated: bool,
}

/// The changed files at `cwd`, from the repository's top: the tracked ones against `HEAD`
/// (against the index before the first commit), compared as the diff compares them, then the
/// untracked ones. `None` when git cannot list them, or lists more tracked files than
/// [`LISTING_CAP`] holds. Run with the diff's switches, so no configured program runs.
async fn files(git: &Path, cwd: &str, has_head: bool, timeout: Duration) -> Option<Files> {
    let mut args = diff_args(false);
    args.extend(["--no-relative", "-z", "--raw", "--numstat"]);
    if has_head {
        args.push("HEAD");
    }
    let tracked = run(git, cwd, &args, timeout, LISTING_CAP).await.ok()?;
    let untracked = run(git, cwd, UNTRACKED_ARGS, timeout, LISTING_CAP)
        .await
        .ok()?;
    let top = run(
        git,
        cwd,
        &["rev-parse", "--show-toplevel"],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    if !tracked.ok || tracked.stdout_truncated || !untracked.ok || !top.ok || top.stdout_truncated {
        return None;
    }
    let top = PathBuf::from(top.stdout.trim_end_matches('\n'));
    let mut names: Vec<String> = untracked.stdout.split('\0').map(String::from).collect();
    // The last name is whole only when the listing ended with its NUL: a cut one is dropped.
    names.pop();
    let new_files = tokio::task::spawn_blocking(move || count_untracked(&top, names))
        .await
        .ok()?;
    let mut files = parse_raw_numstat(&tracked.stdout);
    files.extend(new_files);
    Some(Files {
        files,
        truncated: untracked.stdout_truncated,
    })
}

/// `git diff -z --raw --numstat`: every file's raw entry (`:modes shas status`, then its
/// path, or a rename's or copy's two), then every file's counts (`added\tremoved\tpath`, or
/// for two paths an empty third column followed by both). A binary file counts `-`.
fn parse_raw_numstat(listing: &str) -> Vec<protocol::ws::DiffFile> {
    use protocol::ws::{DiffFile, DiffFileStatus};
    let tokens: Vec<&str> = listing.split('\0').collect();
    let mut entries = Vec::new();
    let mut at = 0;
    while let Some(header) = tokens.get(at).filter(|token| token.starts_with(':')) {
        let letter = header
            .rsplit(' ')
            .next()
            .and_then(|code| code.chars().next());
        let status = match letter {
            Some('A') => DiffFileStatus::Added,
            Some('D') => DiffFileStatus::Deleted,
            Some('R') => DiffFileStatus::Renamed,
            Some('C') => DiffFileStatus::Copied,
            Some('T') => DiffFileStatus::TypeChanged,
            _ => DiffFileStatus::Modified,
        };
        let paths = if matches!(letter, Some('R' | 'C')) {
            2
        } else {
            1
        };
        let named: Vec<&str> = tokens.iter().skip(at + 1).take(paths).copied().collect();
        if named.len() < paths {
            break;
        }
        at += 1 + paths;
        let (old_path, path) = match named.as_slice() {
            [old, new] => (Some(old.to_string()), new.to_string()),
            [path] => (None, path.to_string()),
            _ => break,
        };
        entries.push((path, old_path, status));
    }
    let mut counts = std::collections::HashMap::new();
    while let Some(token) = tokens.get(at) {
        let mut columns = token.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) =
            (columns.next(), columns.next(), columns.next())
        else {
            at += 1;
            continue;
        };
        let path = if path.is_empty() {
            at += 2;
            tokens.get(at).copied().unwrap_or_default()
        } else {
            path
        };
        at += 1;
        counts.insert(
            path,
            (
                added.parse::<u64>().ok(),
                removed.parse::<u64>().ok(),
                added == "-",
            ),
        );
    }
    entries
        .into_iter()
        .map(|(path, old_path, status)| {
            let (added, removed, binary) = counts
                .get(path.as_str())
                .copied()
                .unwrap_or((None, None, false));
            DiffFile {
                path,
                old_path,
                status,
                added,
                removed,
                binary,
            }
        })
        .collect()
}

/// The untracked files under `top`, each with its lines: every line is added. Only a
/// regular file is read, never followed through a symlink; a symlink, socket or pipe has no
/// lines. A file with a NUL in its first [`BINARY_PROBE`] bytes is binary, git's own content
/// test; a `.gitattributes` `-diff` or `binary` is not read for an untracked file.
/// Every byte read counts against [`UNTRACKED_READ_BUDGET`]: a file larger than what is left
/// of it is not opened, and it and a file that cannot be read have no count; later files
/// still may.
fn count_untracked(top: &Path, names: Vec<String>) -> Vec<protocol::ws::DiffFile> {
    use std::io::Read;
    let mut budget = UNTRACKED_READ_BUDGET;
    let mut lines_of = |path: &Path| -> (Option<u64>, bool) {
        if budget == 0 {
            return (None, false);
        }
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return (None, false);
        };
        if !meta.is_file() || meta.len() > budget {
            return (None, false);
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            return (None, false);
        };
        let mut text = Vec::new();
        if (&mut file)
            .take(BINARY_PROBE.min(budget))
            .read_to_end(&mut text)
            .is_err()
        {
            return (None, false);
        }
        budget -= text.len() as u64;
        if text.contains(&0) {
            return (None, true);
        }
        let probed = text.len();
        if (&mut file).take(budget + 1).read_to_end(&mut text).is_err() {
            return (None, false);
        }
        let rest = (text.len() - probed) as u64;
        if rest > budget {
            budget = 0;
            return (None, false);
        }
        budget -= rest;
        let breaks = text.iter().filter(|byte| **byte == b'\n').count() as u64;
        let unended = u64::from(text.last().is_some_and(|byte| *byte != b'\n'));
        (Some(breaks + unended), false)
    };
    names
        .into_iter()
        .map(|path| {
            let (added, binary) = lines_of(&top.join(&path));
            protocol::ws::DiffFile {
                path,
                old_path: None,
                status: protocol::ws::DiffFileStatus::Untracked,
                added,
                removed: added.map(|_| 0),
                binary,
            }
        })
        .collect()
}

/// Ceiling for a listing of worktrees or branch names.
const LISTING_CAP: usize = 1024 * 1024;

/// One worktree, as `git worktree list --porcelain` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: String,
    /// Absent on a detached head.
    pub branch: Option<String>,
    /// git still lists it, but its folder is gone.
    pub prunable: bool,
}

/// The worktrees of the repository `cwd` is in, the main checkout first; `None`
/// outside a repository or when git cannot answer.
pub async fn worktrees(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Option<Vec<WorktreeEntry>> {
    let git = git_bin(configured_bin)?;
    let out = run(
        &git,
        cwd,
        &["worktree", "list", "--porcelain", "-z"],
        timeout,
        LISTING_CAP,
    )
    .await
    .ok()?;
    (out.ok && !out.stdout_truncated).then(|| parse_worktrees(&out.stdout))
}

/// `git worktree list --porcelain -z`: one NUL per attribute, an empty one
/// between worktrees.
fn parse_worktrees(listing: &str) -> Vec<WorktreeEntry> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    for field in listing.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            entries.push(WorktreeEntry {
                path: path.to_string(),
                branch: None,
                prunable: false,
            });
        } else if let Some(entry) = entries.last_mut() {
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                entry.branch = Some(branch.to_string());
            } else if field == "prunable" || field.starts_with("prunable ") {
                entry.prunable = true;
            }
        }
    }
    entries
}

/// The local branches of the repository `cwd` is in, newest commit first.
pub async fn local_branches(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Option<Vec<String>> {
    let git = git_bin(configured_bin)?;
    let out = run(
        &git,
        cwd,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)",
            "refs/heads/",
        ],
        timeout,
        LISTING_CAP,
    )
    .await
    .ok()?;
    (out.ok && !out.stdout_truncated).then(|| {
        out.stdout
            .lines()
            .filter_map(|line| line.strip_prefix("refs/heads/"))
            .map(str::to_string)
            .collect()
    })
}

/// Whether the worktree at `cwd` has uncommitted changes, untracked files
/// included. Reading the status never writes the index (`GIT_OPTIONAL_LOCKS=0`)
/// and runs no fsmonitor hook; a status git cannot give reads as clean.
pub async fn dirty(cwd: &str, configured_bin: Option<&str>, timeout: Duration) -> bool {
    let Some(git) = git_bin(configured_bin) else {
        return false;
    };
    // One byte is all the answer needs.
    run(
        &git,
        cwd,
        &["-c", "core.fsmonitor=false", "status", "--porcelain"],
        timeout,
        1,
    )
    .await
    .is_ok_and(|out| out.ok && !out.stdout.is_empty())
}

async fn collect_inner(
    cwd: &str,
    configured_bin: Option<&str>,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Diff> {
    if cwd.is_empty() || !Path::new(cwd).is_dir() {
        return Ok(note_only(format!(
            "session directory {cwd:?} is not readable"
        )));
    }
    let Some(git) = git_bin(configured_bin) else {
        return Ok(note_only("git is not installed at a known location".into()));
    };

    // `--is-inside-work-tree` rather than looking for a `.git` entry: it is
    // correct for worktrees, submodules and a cwd well below the root.
    //
    // The probes are capped tightly: their answers are one word and one line,
    // and an unbounded read of a `rev-parse` would be the same defect in a
    // place nobody would think to look for it.
    let inside = run(
        &git,
        cwd,
        &["rev-parse", "--is-inside-work-tree"],
        timeout,
        PROBE_CAP,
    )
    .await?;
    if !inside.ok || inside.stdout.trim() != "true" {
        return Ok(note_only("not a git repository".into()));
    }

    // A repository with no commits has no HEAD to diff against; `git diff`
    // alone still shows unstaged work, which is what the operator wants to see.
    let has_head = run(
        &git,
        cwd,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        timeout,
        PROBE_CAP,
    )
    .await?
    .ok;
    let mut note = None;
    // Headroom for the untracked-file list and the truncation marker appended
    // below, so the assembled string still lands inside `max_bytes`.
    let diff_cap = max_bytes.saturating_add(1024);
    let tracked = if has_head {
        run(&git, cwd, &diff_args(true), timeout, diff_cap).await?
    } else {
        note = Some("repository has no commits yet; showing unstaged changes".into());
        run(&git, cwd, &diff_args(false), timeout, diff_cap).await?
    };
    if !tracked.ok {
        return Ok(note_only(format!(
            "git diff failed: {}",
            tracked.stderr.trim()
        )));
    }

    let untracked = run(
        &git,
        cwd,
        &[
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ],
        timeout,
        // A name list, not file contents. Capped well below the diff so a
        // directory of a million generated files cannot dominate the reply.
        UNTRACKED_CAP,
    )
    .await?;

    let mut unified = tracked.stdout;
    if untracked.ok {
        // Names only. Their contents are not diff output, and a phone rendering
        // a unified diff would have nowhere sensible to put a whole new file.
        let names: Vec<&str> = untracked
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if !names.is_empty() {
            if !unified.is_empty() && !unified.ends_with('\n') {
                unified.push('\n');
            }
            unified.push_str(&format!(
                "\n# {} untracked file(s), not shown as a diff:\n",
                names.len()
            ));
            for name in names {
                unified.push_str("#   ");
                unified.push_str(name);
                unified.push('\n');
            }
        }
    }

    // Either the child produced more than we were willing to read, or the
    // assembled text is over `max_bytes`. Both are the same fact to a client:
    // what it is looking at is incomplete.
    let truncated =
        tracked.stdout_truncated || untracked.stdout_truncated || unified.len() > max_bytes;
    if unified.len() > max_bytes {
        truncate_on_char_boundary(&mut unified, max_bytes);
    }
    if truncated {
        unified.push_str("\n… diff truncated by CodeConnect\n");
    }

    let listed = files(&git, cwd, has_head, timeout).await;
    let files_truncated = listed.as_ref().is_some_and(|listed| {
        listed.truncated || listed.files.len() > protocol::ws::MAX_DIFF_FILES
    });
    let mut files = listed.map(|listed| listed.files);
    if let Some(files) = files.as_mut() {
        files.truncate(protocol::ws::MAX_DIFF_FILES);
    }

    Ok(Diff {
        unified,
        truncated,
        note,
        files,
        files_truncated,
    })
}

/// The diff invocation, with every hook that could run somebody else's code
/// turned off.
///
/// `--no-ext-diff` and `--no-textconv` are not tidiness. `diff.external` and a
/// `textconv` filter are ordinary repository/user configuration that name a
/// **program git will execute** to render a diff, and this path is reachable
/// from the phone: `get_diff` on a session whose working directory is a
/// repository somebody else prepared would run that program as the daemon's
/// user. The daemon's job here is to *observe* a working tree, and observing
/// must not be a way to execute anything.
///
/// `-c core.fsmonitor=false` for the same reason: fsmonitor is a configured
/// hook program too. `--no-color` because a phone renders the text itself, and
/// colour would arrive as escape sequences inside the payload.
fn diff_args(against_head: bool) -> Vec<&'static str> {
    let mut args = vec![
        "-c",
        "core.fsmonitor=false",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ];
    if against_head {
        args.push("HEAD");
    }
    args
}

fn note_only(note: String) -> Diff {
    Diff {
        unified: String::new(),
        truncated: false,
        note: Some(note),
        files: None,
        files_truncated: false,
    }
}

/// Cut to at most `max` bytes without splitting a UTF-8 sequence.
fn truncate_on_char_boundary(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Stderr ceiling. A git error message is one or two lines; anything past this
/// is not diagnostics, and it only ever reaches a log or a `note`.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// Read at most `cap` bytes from a pipe, then **close it**.
///
/// The close is the mechanism, not a side effect: the reader owns the pipe, so
/// returning drops it, the child takes `EPIPE` on its next write, and everything
/// else unblocks without anybody having to hold a kill handle.
async fn read_capped<R>(mut pipe: R, cap: usize) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    (&mut pipe)
        .take(cap as u64)
        .read_to_end(&mut buffer)
        .await?;
    Ok(buffer)
}

#[derive(Debug)]
struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
    /// The child produced more than the cap and was stopped.
    stdout_truncated: bool,
}

/// Run one git subcommand against `cwd`, bounded in time **and in bytes**.
///
/// `-C` rather than `current_dir`: it is what git itself documents for
/// operating on another repository, and it keeps the daemon's own working
/// directory out of the picture entirely.
///
/// This used to be `Command::output()`, which reads the child to EOF into
/// memory and only *then* lets the caller apply its cap. The cap was therefore
/// a cap on what was sent, not on what was allocated: a single generated file —
/// a lockfile, a minified bundle, a database dump committed by accident —
/// produces a diff of arbitrary size, and the daemon buffered every byte of it
/// before deciding to show 512KB. `output()` also has no ceiling on stderr at
/// all, so a repository that prints a warning per file could exhaust memory
/// without producing a single byte of diff.
///
/// So: read at most `stdout_cap + 1` bytes and a bounded stderr, then **kill the
/// child** rather than politely draining it. `cap + 1` and not `cap` because one
/// extra byte is exactly what distinguishes "this is the whole output" from
/// "there is more", and a truncated diff that does not know it is truncated is
/// the one result that must not be produced.
async fn run(
    git: &Path,
    cwd: &str,
    args: &[&str],
    timeout: Duration,
    stdout_cap: usize,
) -> Result<Output> {
    let mut command = tokio::process::Command::new(git);
    command.arg("-C").arg(cwd).args(args);
    // Git will happily block on a credential prompt or a pager. Neither has a
    // terminal here, and either would hang the request until the timeout.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Kill the child if this future is dropped — a cancelled request must
        // not leave a git process reading the disk on the operator's behalf.
        .kill_on_drop(true);

    let mut child = crate::spawn::spawn(&mut command)
        .with_context(|| format!("running git {}", args.join(" ")))?;
    // Each pipe is *moved* into its own task, and this is load-bearing rather
    // than stylistic. Reading them in sequence deadlocks the moment git fills
    // the pipe nobody is reading — which is exactly what a repository that
    // prints a warning per file does. Reading them concurrently but waiting for
    // both deadlocks too, and more subtly: the stream that hits its cap stops
    // reading while the child blocks writing to it, so the *other* stream never
    // reaches EOF either. Giving each reader ownership makes the pipe close
    // when its cap is reached, the child take EPIPE on its next write, and the
    // other reader finish — no coordination, no shared kill handle.
    let stdout = child.stdout.take().context("git stdout was not piped")?;
    let stderr = child.stderr.take().context("git stderr was not piped")?;

    // `cap + 1` is the whole trick: one byte past the cap proves there was
    // more, and it is the only extra byte ever allocated. A truncated diff that
    // does not know it is truncated is the one result that must not be produced.
    let out_task = tokio::spawn(read_capped(stdout, stdout_cap.saturating_add(1)));
    let err_task = tokio::spawn(read_capped(stderr, MAX_STDERR_BYTES));

    let collected = tokio::time::timeout(timeout, async {
        let out = out_task.await;
        let err = err_task.await;
        (out, err)
    })
    .await;

    let Ok((out, err)) = collected else {
        // The deadline is the deadline. A child left running would keep holding
        // the repository's locks and the daemon's descriptors; `kill_on_drop`
        // covers the abandoned-future case, and this covers this one.
        let _ = child.kill().await;
        let _ = child.wait().await;
        anyhow::bail!("git {} timed out", args.join(" "));
    };
    let out_bytes = out
        .ok()
        .transpose()
        .with_context(|| format!("reading git {}", args.join(" ")))?
        .unwrap_or_default();
    // A failed stderr read is not worth failing the request over: it is
    // diagnostics, and losing it costs a less specific message.
    let err_bytes = err.ok().and_then(Result::ok).unwrap_or_default();

    let stdout_truncated = out_bytes.len() > stdout_cap;
    // Killed rather than drained. Draining would reintroduce the unbounded read
    // this function exists to remove — the bytes would not be *kept*, but the
    // daemon would still sit reading a multi-gigabyte diff it has already
    // decided to discard.
    let status = if stdout_truncated {
        let _ = child.kill().await;
        child.wait().await.ok()
    } else {
        // Both pipes are at EOF, so the child has finished writing. Its exit is
        // still bounded by the same deadline.
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status.ok(),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                anyhow::bail!("git {} timed out", args.join(" "));
            }
        }
    };

    let mut out_bytes = out_bytes;
    if stdout_truncated {
        out_bytes.truncate(stdout_cap);
    }
    Ok(Output {
        // A child we killed at the cap has no meaningful exit status, and it
        // did produce the output we asked for — so it counts as success.
        ok: stdout_truncated || status.is_some_and(|status| status.success()),
        // Paths and diffs are not guaranteed UTF-8; lossy keeps a mangled
        // filename from failing the whole request.
        stdout: String::from_utf8_lossy(&out_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&err_bytes).into_owned(),
        stdout_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`git worktree list --porcelain -z`, read as git writes it**: the main
    /// checkout first, a detached head with no branch, a folder git calls
    /// prunable, and a path with a space and a newline kept whole.
    #[test]
    fn a_worktree_listing_reads_as_git_writes_it() {
        let listing = "worktree /r/app\0HEAD 1111\0branch refs/heads/main\0\0\
            worktree /r/app/.claude/worktrees/fix+login\0HEAD 2222\0branch refs/heads/worktree-fix+login\0locked\0\0\
            worktree /r/old\0HEAD 3333\0detached\0prunable gitdir file points to non-existent location\0\0\
            worktree /r/a b\nc\0HEAD 4444\0branch refs/heads/feature/x\0\0";
        assert_eq!(
            parse_worktrees(listing),
            [
                ("/r/app", Some("main"), false),
                (
                    "/r/app/.claude/worktrees/fix+login",
                    Some("worktree-fix+login"),
                    false
                ),
                ("/r/old", None, true),
                ("/r/a b\nc", Some("feature/x"), false),
            ]
            .map(|(path, branch, prunable)| WorktreeEntry {
                path: path.into(),
                branch: branch.map(str::to_string),
                prunable,
            })
        );
    }
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccd-git-{}-{}-{}-{}",
            tag,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_sync(dir: &Path, args: &[&str]) {
        let git = git_bin(None).expect("git must be installed");
        let status = std::process::Command::new(git)
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    async fn diff_of(dir: &Path, max: usize) -> Diff {
        collect(dir.to_str().unwrap(), None, max, Duration::from_secs(30)).await
    }

    #[tokio::test]
    async fn a_non_git_directory_says_so_instead_of_looking_clean() {
        let dir = temp_dir("plain");
        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(diff.unified.is_empty());
        assert!(!diff.truncated);
        assert_eq!(diff.note.as_deref(), Some("not a git repository"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_directory_is_a_note_not_a_failure() {
        let diff = collect(
            "/nonexistent/codeconnect/test",
            None,
            1024,
            Duration::from_secs(5),
        )
        .await;
        assert!(diff.note.unwrap().contains("not readable"));
    }

    #[tokio::test]
    async fn tracked_edits_and_untracked_files_both_appear() {
        let dir = temp_dir("repo");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);

        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(dir.join("brand-new.txt"), "hello\n").unwrap();

        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(diff.note.is_none(), "{:?}", diff.note);
        assert!(diff.unified.contains("diff --git"), "{}", diff.unified);
        assert!(diff.unified.contains("-one"), "{}", diff.unified);
        assert!(diff.unified.contains("+two"), "{}", diff.unified);
        assert!(
            diff.unified.contains("brand-new.txt"),
            "untracked files must be listed: {}",
            diff.unified
        );
        assert!(!diff.truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_clean_repository_is_empty_with_no_note() {
        // The pairing with the not-a-repository case: same empty diff, and the
        // absence of a note is what tells the phone it really is clean.
        let dir = temp_dir("clean");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);

        let diff = diff_of(&dir, 512 * 1024).await;
        assert_eq!(diff.unified, "");
        assert_eq!(diff.note, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_repository_with_no_commits_still_reports_work() {
        let dir = temp_dir("nohead");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("staged.txt"), "x\n").unwrap();
        git_sync(&dir, &["add", "staged.txt"]);
        std::fs::write(dir.join("loose.txt"), "y\n").unwrap();

        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(
            diff.note.as_deref().unwrap().contains("no commits"),
            "{:?}",
            diff.note
        );
        assert!(diff.unified.contains("loose.txt"), "{}", diff.unified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn tree_of(dir: &Path) -> TreeFacts {
        tree_facts(dir.to_str().unwrap(), None, Duration::from_secs(30)).await
    }

    /// **The session list's branch and line counts**: tracked edits counted
    /// against `HEAD`, a binary change counted as nothing, an untracked file's
    /// every line added, and nothing at all outside a repository.
    #[tokio::test]
    async fn tree_facts_count_tracked_lines_on_the_checked_out_branch() {
        let dir = temp_dir("tree");
        git_sync(&dir, &["init", "-q", "-b", "feature/x"]);
        std::fs::write(dir.join("a.txt"), "1\n2\n3\n").unwrap();
        std::fs::write(dir.join("b.bin"), b"\0\x01\x02").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        assert_eq!(
            tree_of(&dir).await,
            TreeFacts {
                branch: Some("feature/x".into()),
                diff_stat: Some(protocol::event::DiffStat {
                    added: 0,
                    removed: 0
                }),
            },
            "a clean tree is zero lines, not unknown"
        );

        std::fs::write(dir.join("a.txt"), "1\ntwo\n3\n4\n").unwrap();
        std::fs::write(dir.join("b.bin"), b"\0\x09").unwrap();
        std::fs::write(dir.join("new.txt"), "untracked\n").unwrap();
        git_sync(&dir, &["add", "a.txt"]);
        std::fs::write(dir.join("c.txt"), "never committed\n").unwrap();
        assert_eq!(
            tree_of(&dir).await.diff_stat,
            Some(protocol::event::DiffStat {
                added: 4,
                removed: 1
            }),
            "staged and unstaged tracked edits against HEAD, and each untracked file's line; binary adds nothing"
        );

        git_sync(&dir, &["checkout", "-q", "--detach"]);
        assert_eq!(
            tree_of(&dir).await.branch,
            None,
            "a detached head has no branch"
        );
        let _ = std::fs::remove_dir_all(&dir);

        let plain = temp_dir("tree-plain");
        assert_eq!(tree_of(&plain).await, TreeFacts::default());
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[tokio::test]
    async fn tree_facts_without_commits_count_unstaged_lines() {
        let dir = temp_dir("tree-nohead");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        git_sync(&dir, &["add", "a.txt"]);
        std::fs::write(dir.join("a.txt"), "x\ny\nz\n").unwrap();
        assert_eq!(
            tree_of(&dir).await,
            TreeFacts {
                branch: Some("main".into()),
                diff_stat: Some(protocol::event::DiffStat {
                    added: 2,
                    removed: 0
                }),
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn file(
        path: &str,
        old_path: Option<&str>,
        status: protocol::ws::DiffFileStatus,
        counts: Option<(u64, u64)>,
        binary: bool,
    ) -> protocol::ws::DiffFile {
        protocol::ws::DiffFile {
            path: path.into(),
            old_path: old_path.map(String::from),
            status,
            added: counts.map(|(added, _)| added),
            removed: counts.map(|(_, removed)| removed),
            binary,
        }
    }

    /// **`git diff -z --raw --numstat`, read as git 2.54 wrote it** for a binary
    /// change, a deletion, a mode change with edits and a rename: raw entries first,
    /// then counts, a rename's counts with an empty path column before its two paths.
    #[test]
    fn raw_and_numstat_read_as_git_writes_them() {
        use protocol::ws::DiffFileStatus::*;
        let listing = ":100644 100644 badc806 0000000 M\0bin.dat\0\
            :100644 000000 587be6b 0000000 D\0gone.txt\0\
            :100644 100755 422c2b7 0000000 M\0keep.txt\0\
            :100644 100644 28ce6a8 28ce6a8 R100\0mv-src.txt\0mv-dst.txt\0\
            -\t-\tbin.dat\x000\t1\tgone.txt\x002\t1\tkeep.txt\x000\t0\t\0mv-src.txt\0mv-dst.txt\0";
        assert_eq!(
            parse_raw_numstat(listing),
            [
                file("bin.dat", None, Modified, None, true),
                file("gone.txt", None, Deleted, Some((0, 1)), false),
                file("keep.txt", None, Modified, Some((2, 1)), false),
                file(
                    "mv-dst.txt",
                    Some("mv-src.txt"),
                    Renamed,
                    Some((0, 0)),
                    false
                ),
            ]
        );
        assert_eq!(parse_raw_numstat(""), []);
    }

    /// **Every changed file, from the repository's top, with its lines**, read from a
    /// folder below the top: an edit, a deletion, a rename, a binary change, untracked
    /// text (a last line with no newline counts), untracked binary, an untracked
    /// symlink with no lines and never followed, and an ignored file left out. The
    /// session list's count is their sum.
    #[tokio::test]
    async fn a_diff_lists_every_changed_file_with_its_lines() {
        use protocol::ws::DiffFileStatus::*;
        let dir = temp_dir("files");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("keep.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "x\n").unwrap();
        std::fs::write(dir.join("mv-src.txt"), "m\n").unwrap();
        std::fs::write(dir.join("bin.dat"), b"\0one").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "base"]);
        std::fs::write(dir.join("keep.txt"), "a\nB\nc\n").unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        git_sync(&dir, &["mv", "mv-src.txt", "mv-dst.txt"]);
        std::fs::write(dir.join("bin.dat"), b"\0two").unwrap();
        std::fs::write(dir.join("sub/new.txt"), "one\ntwo").unwrap();
        std::fs::write(dir.join("sub/new.bin"), b"\0\x01").unwrap();
        std::fs::write(dir.join("noise.log"), "ignored\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", dir.join("link")).unwrap();

        let diff = diff_of(&dir.join("sub"), 512 * 1024).await;
        let mut expected = vec![
            file("bin.dat", None, Modified, None, true),
            file("gone.txt", None, Deleted, Some((0, 1)), false),
            file("keep.txt", None, Modified, Some((2, 1)), false),
            file(
                "mv-dst.txt",
                Some("mv-src.txt"),
                Renamed,
                Some((0, 0)),
                false,
            ),
            file("link", None, Untracked, None, false),
            file("sub/new.bin", None, Untracked, None, true),
            file("sub/new.txt", None, Untracked, Some((2, 0)), false),
        ];
        let mut listed = diff.files.clone().expect("a repository's files are listed");
        listed.sort_by(|a, b| {
            (a.status == Untracked, &a.path).cmp(&(b.status == Untracked, &b.path))
        });
        expected.sort_by(|a, b| {
            (a.status == Untracked, &a.path).cmp(&(b.status == Untracked, &b.path))
        });
        assert_eq!(listed, expected);
        assert!(!diff.files_truncated);
        assert_eq!(
            tree_of(&dir.join("sub")).await.diff_stat,
            Some(protocol::event::DiffStat {
                added: 4,
                removed: 2
            }),
            "the session list's count is the files' sum"
        );

        let plain = temp_dir("files-plain");
        assert_eq!(diff_of(&plain, 512 * 1024).await.files, None);
        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Untracked text past the reading budget is uncounted, never guessed, and costs
    /// nothing**: the big file is listed with no lines, a smaller one after it still counts,
    /// and the session list's count is the counted lines, as a binary change's none.
    #[tokio::test]
    async fn untracked_text_past_the_budget_is_uncounted_and_later_files_still_count() {
        let dir = temp_dir("files-budget");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let line = "x".repeat(1023) + "\n";
        std::fs::write(
            dir.join("big.txt"),
            line.repeat((UNTRACKED_READ_BUDGET / 1024) as usize + 1),
        )
        .unwrap();
        std::fs::write(dir.join("small.txt"), "one\n").unwrap();
        let diff = diff_of(&dir, 512 * 1024).await;
        let files = diff.files.expect("listed");
        let counted: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.added))
            .collect();
        assert_eq!(counted, [("big.txt", None), ("small.txt", Some(1))]);
        assert_eq!(
            tree_of(&dir).await.diff_stat,
            Some(protocol::event::DiffStat {
                added: 1,
                removed: 0
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Every byte read counts against the budget, a binary probe's too**: once probes
    /// of binary files have used it up, a text file after them is left uncounted.
    #[tokio::test]
    async fn binary_probes_count_against_the_reading_budget() {
        let dir = temp_dir("files-probes");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let mut blob = vec![b'b'; BINARY_PROBE as usize];
        blob[0] = 0;
        for n in 0..(UNTRACKED_READ_BUDGET / BINARY_PROBE + 1) {
            std::fs::write(dir.join(format!("a{n:04}.bin")), &blob).unwrap();
        }
        // Larger than what the probes leave (4194304 - 524 * 8000 = 2304 bytes), smaller
        // than a whole budget: counted only if the probes cost nothing.
        std::fs::write(dir.join("z.txt"), "z\n".repeat(1500)).unwrap();
        let files = diff_of(&dir, 512 * 1024).await.files.expect("listed");
        let last = files.last().expect("z.txt is listed last");
        assert_eq!((last.path.as_str(), last.added), ("z.txt", None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A file larger than what is left of the budget costs none of it**: it is not read,
    /// so a smaller file after it is still counted.
    #[tokio::test]
    async fn a_file_past_what_is_left_of_the_budget_leaves_it_for_later_files() {
        let dir = temp_dir("files-left");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let line = "x".repeat(99) + "\n";
        std::fs::write(
            dir.join("a.txt"),
            line.repeat((UNTRACKED_READ_BUDGET / 100) as usize - 10),
        )
        .unwrap();
        std::fs::write(dir.join("b.txt"), "y".repeat(10_000)).unwrap();
        std::fs::write(dir.join("c.txt"), "c\n".repeat(10)).unwrap();
        let files = diff_of(&dir, 512 * 1024).await.files.expect("listed");
        let counted: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.added))
            .collect();
        assert_eq!(
            counted,
            [
                ("a.txt", Some(UNTRACKED_READ_BUDGET / 100 - 10)),
                ("b.txt", None),
                ("c.txt", Some(10))
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The diff keeps the repository's own `diff.relative`, the list does not**: read
    /// from a folder below the top, the diff's paths are that folder's, as they were, so a
    /// hunk comment names a path the run can open; the list's are from the top.
    #[tokio::test]
    async fn a_relative_diff_stays_relative_and_the_list_is_from_the_top() {
        let dir = temp_dir("files-relative");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["config", "diff.relative", "true"]);
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/x.txt"), "1\n").unwrap();
        std::fs::write(dir.join("top.txt"), "1\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "base"]);
        std::fs::write(dir.join("sub/x.txt"), "2\n").unwrap();
        std::fs::write(dir.join("top.txt"), "2\n").unwrap();
        let diff = diff_of(&dir.join("sub"), 512 * 1024).await;
        assert!(diff.unified.contains("+++ b/x.txt"), "{}", diff.unified);
        assert!(!diff.unified.contains("top.txt"), "{}", diff.unified);
        let paths: Vec<_> = diff
            .files
            .expect("listed")
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(paths, ["sub/x.txt", "top.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **No read runs the repository's fsmonitor**: with `core.fsmonitor` naming a program
    /// that leaves a mark, neither the diff nor the session list's read runs it.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_read_runs_a_configured_fsmonitor() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("fsmonitor");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let mark = dir.join("ran");
        let hook = dir.join("monitor.sh");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho x >> '{}'\n", mark.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_sync(&dir, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        std::fs::write(dir.join("loose.txt"), "x\n").unwrap();
        let _ = std::fs::remove_file(&mark);
        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(diff.unified.contains("loose.txt"), "{}", diff.unified);
        let _ = tree_of(&dir).await;
        assert!(!mark.exists(), "a read ran the configured fsmonitor");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **More untracked names than one listing holds**: the names that fit are listed whole
    /// and the list says it was cut, rather than no list at all.
    #[tokio::test]
    async fn an_untracked_listing_past_its_cap_keeps_the_names_that_fit() {
        let dir = temp_dir("files-many");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let name = "n".repeat(200);
        for n in 0..(LISTING_CAP / 200 + 10) {
            std::fs::write(dir.join(format!("{name}{n:05}")), "").unwrap();
        }
        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(diff.files_truncated);
        let files = diff.files.expect("the names that fit are listed");
        assert_eq!(files.len(), protocol::ws::MAX_DIFF_FILES);
        assert!(
            files.iter().all(|file| file.path.len() == 205),
            "no cut name"
        );
        assert_eq!(
            tree_of(&dir).await.diff_stat,
            None,
            "a total with names left out would read as the whole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The list compares files as the diff does**: with copy detection on in the
    /// repository's config, a copy is listed as one, with where it came from.
    #[tokio::test]
    async fn the_list_follows_the_repositorys_rename_and_copy_setting() {
        let dir = temp_dir("files-copies");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["config", "diff.renames", "copies"]);
        let text: String = (0..40).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("src.txt"), &text).unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "base"]);
        std::fs::write(dir.join("src.txt"), text.clone() + "more\n").unwrap();
        std::fs::write(dir.join("dst.txt"), &text).unwrap();
        git_sync(&dir, &["add", "."]);
        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(
            diff.unified.contains("copy from src.txt"),
            "{}",
            diff.unified
        );
        let copy = diff
            .files
            .expect("listed")
            .into_iter()
            .find(|file| file.path == "dst.txt")
            .expect("the copy is listed");
        assert_eq!(copy.status, protocol::ws::DiffFileStatus::Copied);
        assert_eq!(copy.old_path.as_deref(), Some("src.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_oversized_diff_is_capped_and_says_it_was_capped() {
        let dir = temp_dir("big");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("big.txt"), "x\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        let bulk: String = (0..5000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("big.txt"), bulk).unwrap();

        let diff = diff_of(&dir, 4096).await;
        assert!(diff.truncated, "a capped diff must admit it");
        assert!(
            diff.unified.contains("truncated"),
            "{}",
            &diff.unified[..200]
        );
        // The cap plus the marker; never unbounded.
        assert!(diff.unified.len() < 4096 + 128, "{}", diff.unified.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_user_configured_diff_helper_is_never_executed() {
        // `diff.external` names a program git *runs* to render a diff, and this
        // path is reachable from the phone: `get_diff` on a session whose
        // working directory is a repository somebody else prepared would have
        // executed that program as the daemon's user. Observing a working tree
        // must not be a way to execute anything.
        let dir = temp_dir("extdiff");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        std::fs::write(dir.join("f.txt"), "two\n").unwrap();

        // A "helper" whose only job is to leave evidence that it ran.
        let marker = dir.join("EXECUTED");
        let helper = dir.join("helper.sh");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\ntouch {}\necho pwned\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git_sync(&dir, &["config", "diff.external", helper.to_str().unwrap()]);

        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(
            !marker.exists(),
            "git ran a repository-configured program: {}",
            diff.unified
        );
        assert!(
            !diff.unified.contains("pwned"),
            "and its output reached the client: {}",
            diff.unified
        );
        // The real diff is still produced.
        assert!(diff.unified.contains("-one"), "{}", diff.unified);
        assert!(diff.unified.contains("+two"), "{}", diff.unified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_textconv_filter_is_never_executed_either() {
        // The second configured-program hook on the same path. `textconv` is
        // attached through `.gitattributes`, which lives *in the repository* —
        // so unlike `diff.external` it does not even need the user's config.
        let dir = temp_dir("textconv");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.bin"), "one\n").unwrap();
        std::fs::write(dir.join(".gitattributes"), "*.bin diff=cc\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        std::fs::write(dir.join("a.bin"), "two\n").unwrap();

        let marker = dir.join("EXECUTED");
        let helper = dir.join("conv.sh");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\ntouch {}\necho converted\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git_sync(
            &dir,
            &["config", "diff.cc.textconv", helper.to_str().unwrap()],
        );

        let diff = diff_of(&dir, 512 * 1024).await;
        assert!(!marker.exists(), "a textconv filter ran: {}", diff.unified);
        assert!(!diff.unified.contains("converted"), "{}", diff.unified);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_huge_diff_is_bounded_at_the_read_not_after_it() {
        // The defect: `Command::output()` reads the child to EOF into memory
        // and only *then* lets the caller apply its cap, so the cap bounded
        // what was *sent* rather than what was allocated. A single generated
        // file — a lockfile, a minified bundle, a dump committed by accident —
        // makes that an arbitrary allocation from one `get_diff`.
        let dir = temp_dir("huge");
        git_sync(&dir, &["init", "-q"]);
        std::fs::write(dir.join("big.txt"), "seed\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        // ~24MB of diff from one file.
        let bulk: String = (0..1_000_000).map(|n| format!("line {n}\n")).collect();
        assert!(bulk.len() > 8 * 1024 * 1024);
        std::fs::write(dir.join("big.txt"), bulk).unwrap();

        let cap = 8 * 1024;
        let diff = diff_of(&dir, cap).await;
        assert!(diff.truncated, "a capped diff must admit it");
        assert!(
            diff.unified.len() < cap + 128,
            "the assembled diff is {} bytes",
            diff.unified.len()
        );
        assert!(diff.unified.contains("truncated"));

        // And the read itself was bounded: asking `run` directly proves the
        // buffer never grew past the cap, which is the property `output()`
        // could not give at any cap.
        let raw = run(
            &git_bin(None).unwrap(),
            dir.to_str().unwrap(),
            &diff_args(true),
            Duration::from_secs(30),
            cap,
        )
        .await
        .unwrap();
        assert!(raw.stdout_truncated);
        assert_eq!(
            raw.stdout.len(),
            cap,
            "the child's output must be bounded at the read"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_child_that_never_finishes_is_killed_at_the_deadline() {
        // `git` here is a stand-in for the class: a child holding the pipe open
        // must not outlive the request. Before, a timeout dropped the `output()`
        // future and left the process running.
        let dir = temp_dir("hang");
        let sleeper = dir.join("slowgit.sh");
        std::fs::write(&sleeper, "#!/bin/sh\nsleep 30\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sleeper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let started = std::time::Instant::now();
        let err = run(
            &sleeper,
            dir.to_str().unwrap(),
            &["rev-parse"],
            Duration::from_millis(200),
            1024,
        )
        .await
        .expect_err("a child that never finishes must be an error");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline must be the deadline: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_enormous_stderr_cannot_exhaust_memory() {
        // `output()` had no ceiling on stderr at all, so a repository that
        // printed a warning per file could exhaust memory without producing a
        // single byte of diff.
        let dir = temp_dir("noisy");
        let noisy = dir.join("noisy.sh");
        std::fs::write(
            &noisy,
            "#!/bin/sh\nyes 'warning: something is wrong' | head -c 5000000 >&2\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&noisy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let out = run(
            &noisy,
            dir.to_str().unwrap(),
            &["rev-parse"],
            Duration::from_secs(20),
            1024,
        )
        .await
        .unwrap();
        assert!(
            out.stderr.len() <= MAX_STDERR_BYTES,
            "stderr grew to {} bytes",
            out.stderr.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_diff_invocation_disables_every_configured_program() {
        // Asserted on the argument list as well as on behaviour, so removing a
        // flag fails here loudly rather than silently re-enabling execution in
        // a git version whose defaults happen to differ.
        for against_head in [true, false] {
            let args = diff_args(against_head);
            for required in ["--no-ext-diff", "--no-textconv", "core.fsmonitor=false"] {
                assert!(args.contains(&required), "{required} missing from {args:?}");
            }
            assert_eq!(args.contains(&"HEAD"), against_head);
        }
        assert!(
            UNTRACKED_ARGS.contains(&"core.fsmonitor=false"),
            "the untracked listing runs on every turn's end too: {UNTRACKED_ARGS:?}"
        );
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let mut text = "é".repeat(10); // 20 bytes
        truncate_on_char_boundary(&mut text, 5);
        assert_eq!(text, "éé", "must round down to a boundary");
        let mut ascii = "abcdef".to_string();
        truncate_on_char_boundary(&mut ascii, 3);
        assert_eq!(ascii, "abc");
        let mut short = "ab".to_string();
        truncate_on_char_boundary(&mut short, 99);
        assert_eq!(short, "ab");
    }

    #[test]
    fn git_is_locatable_on_this_machine() {
        assert!(git_bin(None).is_some(), "get_diff needs git");
        // A configured path that does not exist falls through rather than
        // taking the feature down with it.
        assert!(git_bin(Some("/nonexistent/git")).is_some());
    }
}
