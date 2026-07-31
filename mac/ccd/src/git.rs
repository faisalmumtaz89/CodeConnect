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

/// launchd gives the daemon no PATH. `/usr/bin/git` is the Command Line Tools
/// shim and is present on any Mac that has ever built anything.
const GIT_CANDIDATES: &[&str] = &[
    "/usr/bin/git",
    "/opt/homebrew/bin/git",
    "/usr/local/bin/git",
];

/// Ceiling for a `rev-parse` probe. Its answer is one word.
const PROBE_CAP: usize = 4 * 1024;

/// Ceiling for the untracked-file listing. Names, not contents.
const UNTRACKED_CAP: usize = 64 * 1024;

pub struct Diff {
    pub unified: String,
    pub truncated: bool,
    pub note: Option<String>,
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
        .find(|path| path.is_file())
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
        Err(err) => Diff {
            unified: String::new(),
            truncated: false,
            note: Some(format!("could not read the diff: {err:#}")),
        },
    }
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
        &["ls-files", "--others", "--exclude-standard"],
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

    Ok(Diff {
        unified,
        truncated,
        note,
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

    let mut child = command
        .spawn()
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
