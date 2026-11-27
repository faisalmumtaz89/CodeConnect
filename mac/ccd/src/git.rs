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
    pub repo: Option<protocol::event::RepoFact>,
}

/// The branch, line counts and repository of the working tree at `cwd`, each `None` when git
/// cannot say.
pub async fn tree_facts(cwd: &str, configured_bin: Option<&str>, timeout: Duration) -> TreeFacts {
    TreeFacts {
        branch: branch(cwd, configured_bin, timeout).await,
        diff_stat: diff_stat(cwd, configured_bin, timeout).await,
        repo: repo_fact(cwd, configured_bin, timeout).await,
    }
}

/// The repository `cwd` is in: its main checkout, the folder holding the `.git` its worktrees
/// share, else (a submodule) the top of `cwd`'s own work tree; and its `origin` ([`remote_key`]).
/// `None` outside a repository, or when git cannot answer.
pub async fn repo_fact(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Option<protocol::event::RepoFact> {
    let git = git_bin(configured_bin)?;
    let dirs = run(
        &git,
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-toplevel",
        ],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    if !dirs.ok || dirs.stdout_truncated {
        return None;
    }
    let mut lines = dirs.stdout.lines();
    let (common, top) = (Path::new(lines.next()?), lines.next()?);
    let main_path = match common.parent() {
        Some(main) if common.file_name() == Some(".git".as_ref()) => main.to_str()?.to_string(),
        _ => top.to_string(),
    };
    let origin = run(
        &git,
        cwd,
        &["remote", "get-url", "origin"],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    let remote = origin.ok.then(|| remote_key(&origin.stdout)).flatten();
    Some(protocol::event::RepoFact {
        name: Path::new(&main_path).file_name()?.to_str()?.to_string(),
        remote,
        main_path,
    })
}

/// A remote's URL as one key for its repository: `github.com/<owner>/<name>` in lower case, as
/// [`crate::github::parse_remote`] reads it; `None` for any other host.
pub fn remote_key(url: &str) -> Option<String> {
    let repo = crate::github::parse_remote(url)?;
    Some(format!("{}/{}/{}", repo.host, repo.owner, repo.name).to_ascii_lowercase())
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

/// The longest one [`run_git`] read takes, its git children together.
const RUN_GIT_DEADLINE: Duration = Duration::from_secs(20);

/// A run's repository for its Changes panel; `Err` is the note saying why there is none.
/// Git's children run one after another, each within `timeout`, all within
/// [`RUN_GIT_DEADLINE`], with the diff's switches: no fsmonitor, external diff or textconv
/// program runs. A filter the repository's attributes name (Git LFS) still runs, and the
/// diff listing may refresh the index, as any `git status` or `git diff` does. `changed`
/// counts every file listed, before the row and frame limits of a `diff` reply.
pub async fn run_git(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Result<protocol::ws::RunGit, String> {
    tokio::time::timeout(
        RUN_GIT_DEADLINE,
        run_git_inner(cwd, configured_bin, timeout),
    )
    .await
    .unwrap_or_else(|_| {
        Err(format!(
            "git did not answer within {} s",
            RUN_GIT_DEADLINE.as_secs()
        ))
    })
}

async fn run_git_inner(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> Result<protocol::ws::RunGit, String> {
    if cwd.is_empty() || !Path::new(cwd).is_dir() {
        return Err(format!("session directory {cwd:?} is not readable"));
    }
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let failed = |err: anyhow::Error| format!("could not read git: {err:#}");
    let inside = run(
        &git,
        cwd,
        &["rev-parse", "--is-inside-work-tree"],
        timeout,
        PROBE_CAP,
    )
    .await
    .map_err(failed)?;
    if !inside.ok || inside.stdout.trim() != "true" {
        return Err("not a git repository".into());
    }
    let status = run(&git, cwd, STATUS_ARGS, timeout, LISTING_CAP)
        .await
        .map_err(failed)?;
    if !status.ok {
        return Err(format!(
            "git status failed: {}",
            status.stderr.lines().next().unwrap_or_default()
        ));
    }
    let head = parse_status(&status.stdout);
    let listed = files(&git, cwd, head.oid.is_some(), timeout)
        .await
        .ok_or("git could not list the changed files")?;
    let Remotes { base, pushed } = remotes(
        &git,
        cwd,
        head.branch.as_deref(),
        head.oid.as_deref(),
        timeout,
    )
    .await;
    // Counted from the commit status read, so a commit made during the read cannot make
    // `ahead` disagree with `head_oid`.
    let ahead = match (&head.oid, &base) {
        (Some(oid), Some(base)) => {
            let range = format!("{}..{oid}", base.reference);
            run(
                &git,
                cwd,
                &["rev-list", "--count", &range],
                timeout,
                PROBE_CAP,
            )
            .await
            .ok()
            .filter(|out| out.ok)
            .and_then(|out| out.stdout.trim().parse().ok())
        }
        _ => None,
    };
    Ok(protocol::ws::RunGit {
        branch: head.branch,
        head_oid: head.oid,
        base,
        changed: protocol::ws::ChangedFiles {
            files: listed.files.len() as u64,
            added: listed.files.iter().filter_map(|file| file.added).sum(),
            removed: listed.files.iter().filter_map(|file| file.removed).sum(),
            complete: !listed.truncated && !listed.uncounted,
        },
        ahead,
        pushed,
        unmerged: head.unmerged,
        read_at: protocol::time::now_rfc3339(),
        gh: None,
        gh_host: None,
        pr: None,
        gh_read_at: None,
        gh_repo: None,
    })
}

/// The remote `branch` pushes to (`%(push:remotename)`: its `pushRemote`,
/// `remote.pushDefault`, or the upstream's), else `origin`: the URL it pushes to; and the URL
/// the remote named `upstream` (a fork's parent) fetches from, else `origin`'s, else the push
/// remote's: what [`crate::github`] names the head and target repositories by. Never the
/// branch's own upstream remote, which a push (`-u`) changes.
pub(crate) async fn push_remote_urls(
    cwd: &str,
    configured_bin: Option<&str>,
    branch: &str,
    timeout: Duration,
) -> Option<PrRemotes> {
    let git = git_bin(configured_bin)?;
    let local = format!("refs/heads/{branch}");
    let named = run(
        &git,
        cwd,
        &["for-each-ref", "--format=%(push:remotename)", &local],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()
    .filter(|out| out.ok)?;
    let push = Some(named.stdout.trim().to_string())
        .filter(|remote| !remote.is_empty())
        .unwrap_or_else(|| "origin".to_string());
    let head = remote_url(&git, cwd, &push, true, timeout).await?;
    let mut target = None;
    for remote in ["upstream", "origin", push.as_str()] {
        target = remote_url(&git, cwd, remote, false, timeout).await;
        if target.is_some() {
            break;
        }
    }
    let target = target?;
    Some(PrRemotes { push, head, target })
}

/// Where a branch's pull request comes from and goes: [`push_remote_urls`].
pub(crate) struct PrRemotes {
    /// The remote the branch pushes to: its name, or the URL a branch names as its remote.
    pub push: String,
    /// The URL it pushes to.
    pub head: String,
    /// The URL of the repository a pull request goes into.
    pub target: String,
}

/// The longest a push may take: a repository's `pre-push` hook (a test run) works inside it.
const PUSH_TIMEOUT: Duration = Duration::from_secs(120);

/// Why [`push`] did not push.
#[derive(Debug, PartialEq, Eq)]
pub enum PushFailure {
    /// Git refused (a hook, the remote, credentials): its first line.
    Refused(String),
    /// No answer within [`PUSH_TIMEOUT`]; it may have pushed.
    Timeout,
}

/// Push `branch` to `remote` (a remote's name or a URL) as the branch of the same name there,
/// and make that the branch's upstream: no force, no tags, the repository's `pre-push` hook
/// running as it does for the person's own `git push`, through their login shell's `PATH`.
pub async fn push(
    cwd: &str,
    configured_bin: Option<&str>,
    remote: &str,
    branch: &str,
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> std::result::Result<(), PushFailure> {
    let Some(git) = git_bin(configured_bin) else {
        return Err(PushFailure::Refused("git is not installed".into()));
    };
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let args = [
        "push",
        "--no-follow-tags",
        "-u",
        "--",
        remote,
        refspec.as_str(),
    ];
    match run_write(&git, cwd, &args, PUSH_TIMEOUT, environment).await {
        Ok(out) if out.ok => Ok(()),
        // Git's first line names the remote ("To <url>"); the reason follows it.
        Ok(out) => Err(PushFailure::Refused(
            out.stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with("To "))
                .map(|line| line.chars().take(300).collect())
                .unwrap_or_default(),
        )),
        Err(err) if format!("{err:#}").contains("timed out") => Err(PushFailure::Timeout),
        Err(err) => Err(PushFailure::Refused(format!("{err:#}"))),
    }
}

/// The two ends a run's conflicts are read between: the commit checked out and [`RunGit`]'s
/// base, with the repository's common git folder (the same for all its worktrees).
///
/// [`RunGit`]: protocol::ws::RunGit
pub struct ConflictEnds {
    pub head_oid: String,
    pub base: protocol::ws::RunGitBase,
    pub common_dir: String,
}

/// [`ConflictEnds`] at `cwd`, read before anything is fetched so a commit made meanwhile is not
/// what is compared. `Err` is the note saying why there are none.
pub async fn conflict_ends(
    cwd: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> std::result::Result<ConflictEnds, String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let common_dir = run(
        &git,
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()
    .filter(|out| out.ok)
    .map(|out| out.stdout.trim().to_string())
    .ok_or("not a git repository")?;
    let head_oid = commit_oid(&git, cwd, "HEAD", timeout)
        .await
        .ok_or("no commit is checked out")?;
    let branch = branch(cwd, configured_bin, timeout).await;
    let base = remotes(&git, cwd, branch.as_deref(), Some(&head_oid), timeout)
        .await
        .base
        .ok_or("no base branch is known: no remote names its default branch")?;
    Ok(ConflictEnds {
        head_oid,
        base,
        common_dir,
    })
}

/// The commit `name` resolves to at `cwd` now.
pub async fn ref_oid(
    cwd: &str,
    configured_bin: Option<&str>,
    name: &str,
    timeout: Duration,
) -> Option<String> {
    commit_oid(&git_bin(configured_bin)?, cwd, name, timeout).await
}

async fn commit_oid(git: &Path, cwd: &str, name: &str, timeout: Duration) -> Option<String> {
    let rev = format!("{name}^{{commit}}");
    let out = run(
        git,
        cwd,
        &["rev-parse", "--verify", "--quiet", &rev],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    let oid = out.stdout.trim();
    (out.ok && !oid.is_empty()).then(|| oid.to_string())
}

/// How long a fetch of a base may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// `base` fetched from its remote into its own ref, through the login shell as [`push`] is (the
/// person's credentials), with nothing that could ask: no askpass program (`GIT_ASKPASS` set
/// empty), no ssh askpass, no credential manager window. `Err` is git's line.
pub async fn fetch_base(
    cwd: &str,
    configured_bin: Option<&str>,
    base: &protocol::ws::RunGitBase,
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> std::result::Result<(), String> {
    let git = git_bin(configured_bin).ok_or("git is not installed")?;
    let mut environment = environment.clone();
    environment.insert("GIT_ASKPASS".into(), "".into());
    environment.insert("SSH_ASKPASS_REQUIRE".into(), "never".into());
    let refspec = format!("+refs/heads/{}:{}", base.name, base.reference);
    let args = [
        "-c",
        "credential.interactive=false",
        "fetch",
        "--no-tags",
        "--no-write-fetch-head",
        "--recurse-submodules=no",
        "--",
        base.remote.as_str(),
        refspec.as_str(),
    ];
    match run_write(&git, cwd, &args, FETCH_TIMEOUT, &environment).await {
        Ok(out) if out.ok => Ok(()),
        Ok(out) => Err(out
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(300).collect())
            .unwrap_or_else(|| "git fetch failed".into())),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// How long a merge of the two ends may take.
const MERGE_TREE_TIMEOUT: Duration = Duration::from_secs(15);

/// The most conflicted paths a reply names.
const MAX_CONFLICT_FILES: usize = 100;

/// `head_oid` merged with `base_oid` by git, in its object store only, from the repository's top
/// (git names the paths from where it runs): that top, the tree it wrote, whether it merged
/// cleanly (its exit status alone), and the conflicted paths. `Err` is the note.
async fn merge_tree(
    git: &Path,
    cwd: &str,
    head_oid: &str,
    base_oid: &str,
) -> std::result::Result<(String, String, bool, Vec<String>), String> {
    let top = run(
        git,
        cwd,
        &["rev-parse", "--show-toplevel"],
        MERGE_TREE_TIMEOUT,
        PROBE_CAP,
    )
    .await
    .ok()
    .filter(|out| out.ok)
    .map(|out| out.stdout.trim().to_string())
    .filter(|top| !top.is_empty())
    .ok_or("not a git work tree")?;
    let out = run(
        git,
        &top,
        &[
            "-c",
            "merge.conflictStyle=merge",
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            head_oid,
            base_oid,
        ],
        MERGE_TREE_TIMEOUT,
        LISTING_CAP,
    )
    .await
    .map_err(|err| format!("{err:#}"))?;
    let clean = match out.code {
        Some(0) => true,
        Some(1) => false,
        // 129 is git's usage error: a git before 2.38 has no `--write-tree`.
        Some(129) => return Err("comparing with the base needs git 2.38 or later".into()),
        _ => {
            return Err(out
                .stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| line.chars().take(300).collect())
                .unwrap_or_else(|| "git merge-tree did not finish".into()))
        }
    };
    // The tree, then the conflicted paths up to an empty field; git's messages after it are
    // not read (their layout differs between git versions).
    let mut fields = out.stdout.split('\0');
    let tree = fields.next().unwrap_or_default().trim().to_string();
    let files = fields
        .take_while(|field| !field.is_empty())
        .map(str::to_string)
        .collect();
    Ok((top, tree, clean, files))
}

/// `head_oid` and `base_oid` at `cwd` compared: whether they merge cleanly, and the first
/// [`MAX_CONFLICT_FILES`] conflicted paths with whether that is all of them.
pub async fn conflicted_files(
    cwd: &str,
    configured_bin: Option<&str>,
    head_oid: &str,
    base_oid: &str,
) -> std::result::Result<(bool, Vec<String>, bool), String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let (_, _, clean, mut files) = merge_tree(&git, cwd, head_oid, base_oid).await?;
    let complete = files.len() <= MAX_CONFLICT_FILES;
    files.truncate(MAX_CONFLICT_FILES);
    Ok((clean, files, complete))
}

/// The largest conflicted file whose hunks are read.
const MAX_CONFLICT_FILE_BYTES: usize = 1024 * 1024;

/// `path`'s hunks in the merge of `head_oid` with `base_oid`. `Err` is the note: the file has no
/// conflict in that merge, is over [`MAX_CONFLICT_FILE_BYTES`], or has marker-like lines of its own
/// that cannot be told apart from git's (a quoted marker, a title underlined as wide as the separator).
pub async fn conflict_hunks(
    cwd: &str,
    configured_bin: Option<&str>,
    head_oid: &str,
    base_oid: &str,
    path: &str,
    timeout: Duration,
) -> std::result::Result<Vec<protocol::ws::ConflictHunk>, String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let (top, tree, _, files) = merge_tree(&git, cwd, head_oid, base_oid).await?;
    if !files.iter().any(|file| file == path) {
        return Err("the file has no conflict in that comparison".into());
    }
    let object = format!("{tree}:{path}");
    let size = run(&git, &top, &["cat-file", "-s", &object], timeout, PROBE_CAP)
        .await
        .ok()
        .filter(|out| out.ok)
        .and_then(|out| out.stdout.trim().parse::<usize>().ok());
    let Some(size) = size else {
        // Deleted on one side, or a submodule: git kept no file to show.
        return Ok(Vec::new());
    };
    if size > MAX_CONFLICT_FILE_BYTES {
        return Err("the file is over 1 MB".into());
    }
    let blob = run(
        &git,
        &top,
        &["cat-file", "blob", &object],
        timeout,
        MAX_CONFLICT_FILE_BYTES,
    )
    .await
    .map_err(|err| format!("{err:#}"))?;
    if !blob.ok || blob.stdout_truncated {
        return Err("git could not read the file".into());
    }
    if blob.stdout.contains('\0') {
        return Ok(Vec::new());
    }
    conflict_blocks(&blob.stdout, head_oid, base_oid)
        .ok_or_else(|| "the file's conflict markers cannot be told apart from its own lines".into())
}

/// The conflict blocks of a file git merged `head_oid` and `base_oid` into, as `merge` style
/// writes them: a line of 7 or more `<` and `head_oid` (a `:path` after it when git renamed the
/// file), this branch's lines, a line of as many `=`, the base's lines, a line of as many `>`
/// and `base_oid`. `None` when any other marker-like line does not fit that: a block written by
/// someone else (a file that quotes markers, a merge driver's own format). An `=` line other than
/// a block's one separator is the file's own (a heading's underline), inside a block or out.
fn conflict_blocks(
    text: &str,
    head_oid: &str,
    base_oid: &str,
) -> Option<Vec<protocol::ws::ConflictHunk>> {
    let opening = |line: &str| -> Option<usize> {
        let width = line.bytes().take_while(|byte| *byte == b'<').count();
        let label = line[width..].strip_prefix(' ')?;
        (width >= 7 && (label == head_oid || label.strip_prefix(head_oid)?.starts_with(':')))
            .then_some(width)
    };
    let looks_like_marker = |line: &str| {
        ["<<<<<<<", "=======", ">>>>>>>", "|||||||"]
            .iter()
            .any(|marker| line.starts_with(marker))
    };
    let mut hunks = Vec::new();
    let mut lines = text.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let line_text = line.trim_end_matches(['\n', '\r']);
        let Some(width) = opening(line_text) else {
            // An `=` line opens nothing: outside a block it is a heading's underline.
            if !line_text.starts_with('=') && looks_like_marker(line_text) {
                return None;
            }
            continue;
        };
        let separator = "=".repeat(width);
        let closing = ">".repeat(width);
        let (mut ours, mut theirs) = (String::new(), String::new());
        let mut in_theirs = false;
        let mut closed = false;
        for line in lines.by_ref() {
            let line_text = line.trim_end_matches(['\n', '\r']);
            if !in_theirs && line_text == separator {
                in_theirs = true;
            } else if in_theirs
                && line_text
                    .strip_prefix(closing.as_str())
                    .and_then(|rest| rest.strip_prefix(' '))
                    .is_some_and(|label| {
                        label == base_oid
                            || label
                                .strip_prefix(base_oid)
                                .is_some_and(|rest| rest.starts_with(':'))
                    })
            {
                closed = true;
                break;
            } else if line_text == separator
                || (!line_text.starts_with('=') && looks_like_marker(line_text))
            {
                // A second separator, or another marker: not a block git wrote. Any other `=`
                // line is the file's own (a heading's underline).
                return None;
            } else if in_theirs {
                theirs.push_str(line);
            } else {
                ours.push_str(line);
            }
        }
        if !closed {
            return None;
        }
        hunks.push(protocol::ws::ConflictHunk { ours, theirs });
    }
    Some(hunks)
}

/// The URL `remote` pushes to (`push`) or fetches from. A name holding a `:` is itself the URL:
/// git refuses one in a remote's name, and `gh pr checkout` of a fork's PR sets the fork's URL
/// as the branch's remote.
async fn remote_url(
    git: &Path,
    cwd: &str,
    remote: &str,
    push: bool,
    timeout: Duration,
) -> Option<String> {
    if remote.contains(':') {
        return Some(remote.to_string());
    }
    let mut args = vec!["remote", "get-url"];
    if push {
        args.push("--push");
    }
    args.extend(["--", remote]);
    run(git, cwd, &args, timeout, PROBE_CAP)
        .await
        .ok()
        .filter(|out| out.ok && !out.stdout_truncated)
        .map(|out| out.stdout.trim().to_string())
}

/// The longest a commit's staging or the commit itself may take: a repository's hooks
/// (a formatter, a test run) work inside it.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);

/// What [`commit`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Committed as `sha`, changing `files` files, with `message` as the commit has it (a
    /// `commit-msg` hook may have rewritten the one asked for).
    Committed {
        sha: String,
        files: u64,
        message: String,
    },
    /// Nothing committed: a [`protocol::ws::git_refusal`] code and, for some, git's words.
    Refused {
        code: &'static str,
        detail: Option<String>,
    },
    /// It may have committed, and git could not say: never tried again.
    Unknown(String),
}

fn refused(code: &'static str, detail: Option<String>) -> CommitOutcome {
    CommitOutcome::Refused { code, detail }
}

/// The first line git, or a hook, said: its error, else what it printed.
pub(crate) fn first_words(out: &Output) -> Option<String> {
    [&out.stderr, &out.stdout]
        .into_iter()
        .flat_map(|text| text.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(300).collect())
}

/// Commit every change at `cwd`, tracked and untracked, with `message`, on the branch
/// checked out. Refused without staging anything on a detached head or while a merge has
/// unresolved files (`git add` would mark them resolved), and when nothing changed. The
/// repository's hooks and signing run as for the person's own `git commit`; staging and
/// the commit each have [`COMMIT_TIMEOUT`], the probes `timeout`.
pub async fn commit(
    cwd: &str,
    configured_bin: Option<&str>,
    message: &str,
    timeout: Duration,
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> CommitOutcome {
    use protocol::ws::git_refusal as code;
    if cwd.is_empty() || !Path::new(cwd).is_dir() {
        return refused(
            code::NOT_A_REPOSITORY,
            Some(format!("{cwd:?} is not readable")),
        );
    }
    let Some(git) = git_bin(configured_bin) else {
        return refused(
            code::GIT_FAILED,
            Some("git is not installed at a known location".into()),
        );
    };
    let failed = |err: anyhow::Error| refused(code::GIT_FAILED, Some(format!("{err:#}")));
    let inside = match run(
        &git,
        cwd,
        &["rev-parse", "--is-inside-work-tree"],
        timeout,
        PROBE_CAP,
    )
    .await
    {
        Ok(out) => out,
        Err(err) => return failed(err),
    };
    if !inside.ok || inside.stdout.trim() != "true" {
        return refused(code::NOT_A_REPOSITORY, None);
    }
    if branch(cwd, configured_bin, timeout).await.is_none() {
        return refused(code::DETACHED, None);
    }
    // One byte of output is the proof; nothing more is read.
    match run(
        &git,
        cwd,
        &["ls-files", "--unmerged", "-z", "--", ":/"],
        timeout,
        0,
    )
    .await
    {
        Ok(out) if out.stdout_truncated => return refused(code::UNMERGED, None),
        Ok(out) if out.ok => {}
        Ok(out) => return refused(code::GIT_FAILED, first_words(&out)),
        Err(err) => return failed(err),
    }
    match run_write(
        &git,
        cwd,
        &[
            "-c",
            "core.fsmonitor=false",
            "add",
            "-A",
            "--",
            ":/",
            CLAUDE_WORKTREES,
        ],
        COMMIT_TIMEOUT,
        environment,
    )
    .await
    {
        Ok(out) if out.ok => {}
        Ok(out) => return refused(code::GIT_FAILED, first_words(&out)),
        Err(_) => return refused(code::TIMEOUT, Some("add".into())),
    }
    match run(
        &git,
        cwd,
        &[
            "diff",
            "--cached",
            "--no-relative",
            "--ignore-submodules=none",
            "--name-only",
            "-z",
        ],
        timeout,
        0,
    )
    .await
    {
        Ok(out) if out.stdout_truncated => {}
        Ok(out) if out.ok => return refused(code::NOTHING_TO_COMMIT, None),
        Ok(out) => return refused(code::GIT_FAILED, first_words(&out)),
        Err(err) => return failed(err),
    }
    match run_write(
        &git,
        cwd,
        &["commit", "-q", "-m", message],
        COMMIT_TIMEOUT,
        environment,
    )
    .await
    {
        Ok(out) if out.ok => {}
        Ok(out) => return refused(code::GIT_FAILED, first_words(&out)),
        // Killed at the deadline, a commit may have written its ref already.
        Err(err) => return CommitOutcome::Unknown(format!("{err:#}")),
    }
    let unnamed = || CommitOutcome::Unknown("committed, but git did not name the commit".into());
    let named = match run(
        &git,
        cwd,
        // A person's `log.showSignature` would print the signature's check before the sha.
        &[
            "log",
            "-1",
            "--no-show-signature",
            "--format=%H%x00%B",
            "HEAD",
        ],
        timeout,
        LISTING_CAP,
    )
    .await
    {
        Ok(out) if out.ok && !out.stdout_truncated => out.stdout,
        _ => return unnamed(),
    };
    let Some((sha, message)) = named.split_once('\0') else {
        return unnamed();
    };
    let (sha, message) = (sha.to_string(), message.trim_end().to_string());
    // A count, not a listing: a commit of many thousand files is one line.
    let counted = run(
        &git,
        cwd,
        &[
            "diff-tree",
            "--root",
            "--diff-merges=first-parent",
            "--no-commit-id",
            "-r",
            "--shortstat",
            &sha,
        ],
        timeout,
        PROBE_CAP,
    )
    .await;
    match counted {
        Ok(out) if out.ok => CommitOutcome::Committed {
            files: out
                .stdout
                .split_whitespace()
                .next()
                .and_then(|count| count.parse().ok())
                .unwrap_or(0),
            sha,
            message,
        },
        _ => CommitOutcome::Unknown(format!("committed {sha}, but git did not count its files")),
    }
}

/// The branch header and the unresolved entries, tracked files only, no fsmonitor run.
const STATUS_ARGS: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "status",
    "--porcelain=v2",
    "--branch",
    "--no-ahead-behind",
    "-z",
    "--untracked-files=no",
];

/// What `git status --porcelain=v2 --branch` says of `HEAD`.
#[derive(Debug, Default, PartialEq, Eq)]
struct StatusHead {
    /// Absent before the first commit.
    oid: Option<String>,
    /// Absent on a detached head.
    branch: Option<String>,
    unmerged: bool,
}

/// `# branch.*` headers, then one entry per changed file; an unresolved one starts `u `,
/// and a rename's (`2 `) is followed by its original path. Read from as much as fit in
/// [`LISTING_CAP`]: an unresolved entry past it is not seen.
fn parse_status(listing: &str) -> StatusHead {
    let mut head = StatusHead::default();
    let mut entries = listing.split('\0');
    while let Some(entry) = entries.next() {
        if let Some(oid) = entry.strip_prefix("# branch.oid ") {
            head.oid = (oid != "(initial)").then(|| oid.to_string());
        } else if let Some(branch) = entry.strip_prefix("# branch.head ") {
            head.branch = (branch != "(detached)").then(|| branch.to_string());
        } else if entry.starts_with("u ") {
            head.unmerged = true;
        } else if entry.starts_with("2 ") {
            entries.next();
        }
    }
    head
}

/// What the remotes say of `HEAD`.
#[derive(Debug, Default)]
struct Remotes {
    /// The default branch, as `refs/remotes/<remote>/HEAD` names it, of the remote named
    /// `upstream` (a fork's parent, as `gh repo fork` names it), else of the upstream's
    /// remote, else of `origin`, else of the only remote that has one.
    base: Option<protocol::ws::RunGitBase>,
    /// `HEAD` is the branch of the same name on the push remote git names for it
    /// (`%(push:remotename)`: its `pushRemote`, `remote.pushDefault`, or the upstream's),
    /// else on `origin`.
    pushed: bool,
}

/// [`Remotes`] from one `for-each-ref`. A remote whose name holds a `/` is not seen.
async fn remotes(
    git: &Path,
    cwd: &str,
    branch: Option<&str>,
    head_oid: Option<&str>,
    timeout: Duration,
) -> Remotes {
    let local = branch.map(|name| format!("refs/heads/{name}"));
    let tracking = branch.map(|name| format!("refs/remotes/*/{name}"));
    let mut args = vec![
        "for-each-ref",
        "--format=%(refname)%00%(symref)%00%(upstream:remotename)%00%(objectname)%00%(push:remotename)",
        "refs/remotes/*/HEAD",
    ];
    args.extend(local.as_deref());
    args.extend(tracking.as_deref());
    let Some(out) = run(git, cwd, &args, timeout, LISTING_CAP)
        .await
        .ok()
        .filter(|out| out.ok && !out.stdout_truncated)
    else {
        return Remotes::default();
    };
    let mut upstream_remote = None;
    let mut push_remote = None;
    let mut heads = Vec::new();
    let mut pushed_to = Vec::new();
    for line in out.stdout.lines() {
        let mut fields = line.split('\0');
        let (Some(refname), Some(target), Some(remote), Some(oid), Some(push)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if Some(refname) == local.as_deref() {
            upstream_remote = Some(remote.to_string()).filter(|remote| !remote.is_empty());
            push_remote = Some(push.to_string()).filter(|remote| !remote.is_empty());
            continue;
        }
        let Some((remote, name)) = refname
            .strip_prefix("refs/remotes/")
            .and_then(|rest| rest.split_once('/'))
        else {
            continue;
        };
        if name == "HEAD" {
            if let Some(default) = target
                .strip_prefix("refs/remotes/")
                .and_then(|rest| rest.strip_prefix(remote))
                .and_then(|rest| rest.strip_prefix('/'))
            {
                heads.push(protocol::ws::RunGitBase {
                    name: default.to_string(),
                    remote: remote.to_string(),
                    reference: target.to_string(),
                });
            }
        } else if Some(name) == branch {
            pushed_to.push((remote.to_string(), oid.to_string()));
        }
    }
    let push_remote = push_remote.as_deref().unwrap_or("origin");
    let pushed = pushed_to
        .iter()
        .any(|(remote, oid)| remote == push_remote && Some(oid.as_str()) == head_oid);
    let named = |remote: &str| heads.iter().position(|head| head.remote == remote);
    let base = named("upstream")
        .or_else(|| upstream_remote.as_deref().and_then(named))
        .or_else(|| named("origin"))
        .or_else(|| (heads.len() == 1).then_some(0))
        .map(|at| heads.swap_remove(at));
    Remotes { base, pushed }
}

/// Bytes read from untracked files, all of them together, to count their lines.
const UNTRACKED_READ_BUDGET: u64 = 4 * 1024 * 1024;

/// Git's own test for a binary file: a NUL in its first 8000 bytes.
const BINARY_PROBE: u64 = 8000;

/// Claude Code's own worktrees (`claude --worktree`), which it makes in `.claude/worktrees/<name>` inside the
/// repository and ignores nowhere: never the main checkout's changes, so never listed (in its changes or its diff)
/// or committed there.
const CLAUDE_WORKTREES: &str = ":(exclude,top).claude/worktrees/";

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
    CLAUDE_WORKTREES,
];

/// A working tree's changed files.
struct Files {
    files: Vec<protocol::ws::DiffFile>,
    /// The untracked listing passed [`LISTING_CAP`]: `files` holds the names that fit.
    truncated: bool,
    /// An untracked file was left uncounted or unlisted: past the reading budget, unreadable,
    /// or in a folder git could not open (it warns and lists what it can).
    uncounted: bool,
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
    let (new_files, uncounted) = tokio::task::spawn_blocking(move || count_untracked(&top, names))
        .await
        .ok()?;
    let mut files = parse_raw_numstat(&tracked.stdout);
    files.extend(new_files);
    Some(Files {
        files,
        truncated: untracked.stdout_truncated,
        uncounted: uncounted || !untracked.stderr.trim().is_empty(),
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

/// The untracked files under `top`, each with its lines: every line is added, and whether a
/// regular file was left uncounted. Only a regular file is read, never followed through a
/// symlink; a symlink, socket or pipe has no lines. A file with a NUL in its first
/// [`BINARY_PROBE`] bytes is binary, git's own content test; a `.gitattributes` `-diff` or
/// `binary` is not read for an untracked file.
/// Every byte read counts against [`UNTRACKED_READ_BUDGET`]: a file larger than what is left
/// of it is not opened, and it and a file that cannot be read have no count; later files
/// still may.
fn count_untracked(top: &Path, names: Vec<String>) -> (Vec<protocol::ws::DiffFile>, bool) {
    use std::io::Read;
    let mut budget = UNTRACKED_READ_BUDGET;
    // `None` for a regular file left uncounted, else its lines (none for one that is not a
    // regular file or is binary) and whether it is binary.
    let mut lines_of = |path: &Path| -> Option<(Option<u64>, bool)> {
        if budget == 0 {
            return None;
        }
        let meta = std::fs::symlink_metadata(path).ok()?;
        if !meta.is_file() {
            return Some((None, false));
        }
        if meta.len() > budget {
            return None;
        }
        let mut file = std::fs::File::open(path).ok()?;
        let mut text = Vec::new();
        (&mut file)
            .take(BINARY_PROBE.min(budget))
            .read_to_end(&mut text)
            .ok()?;
        budget -= text.len() as u64;
        if text.contains(&0) {
            return Some((None, true));
        }
        let probed = text.len();
        (&mut file).take(budget + 1).read_to_end(&mut text).ok()?;
        let rest = (text.len() - probed) as u64;
        if rest > budget {
            budget = 0;
            return None;
        }
        budget -= rest;
        let breaks = text.iter().filter(|byte| **byte == b'\n').count() as u64;
        let unended = u64::from(text.last().is_some_and(|byte| *byte != b'\n'));
        Some((Some(breaks + unended), false))
    };
    let mut uncounted = false;
    let mut files = Vec::with_capacity(names.len());
    for path in names {
        let (added, binary) = lines_of(&top.join(&path)).unwrap_or_else(|| {
            uncounted = true;
            (None, false)
        });
        files.push(protocol::ws::DiffFile {
            path,
            old_path: None,
            status: protocol::ws::DiffFileStatus::Untracked,
            added,
            removed: added.map(|_| 0),
            binary,
        });
    }
    (files, uncounted)
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
    /// git has it locked (`git worktree lock`), with the lock's reason, empty when none was given.
    pub locked: Option<String>,
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
                locked: None,
            });
        } else if let Some(entry) = entries.last_mut() {
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                entry.branch = Some(branch.to_string());
            } else if field == "prunable" || field.starts_with("prunable ") {
                entry.prunable = true;
            } else if field == "locked" {
                entry.locked = Some(String::new());
            } else if let Some(reason) = field.strip_prefix("locked ") {
                entry.locked = Some(reason.to_string());
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

/// The switches Archive reads a worktree's changes with, and removes it with: untracked files
/// counted whatever `status.showUntrackedFiles` says (a person's `no` would otherwise let git
/// remove them unseen), and no fsmonitor program run.
const ARCHIVE_STATUS: [&str; 4] = [
    "-c",
    "status.showUntrackedFiles=normal",
    "-c",
    "core.fsmonitor=false",
];

/// Whether the worktree at `top` has uncommitted or untracked changes, for Archive: `Err` when
/// git cannot say (it fails, or warns, as for a folder it cannot open), which Archive takes as a
/// refusal, never as clean. A tracked file git is told to
/// assume unchanged or to skip (`update-index`) counts as one: status does not look at it, and an
/// edit there would go with the worktree.
pub async fn archive_dirty(
    top: &str,
    configured_bin: Option<&str>,
    timeout: Duration,
) -> std::result::Result<bool, String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let mut args = ARCHIVE_STATUS.to_vec();
    args.extend(["status", "--porcelain"]);
    let out = run(&git, top, &args, timeout, 1)
        .await
        .map_err(|err| format!("{err:#}"))?;
    match out.code {
        Some(0) if !out.stdout.is_empty() => return Ok(true),
        // A warning (a folder git could not open) means files it could not count.
        Some(0) if !out.stderr.trim().is_empty() => {
            return Err(out
                .stderr
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string())
        }
        Some(0) => {}
        // Stopped at the one byte the answer needs: there are changes.
        None if out.stdout_truncated => return Ok(true),
        _ => {
            return Err(out
                .stderr
                .lines()
                .next()
                .unwrap_or("git status failed")
                .to_string())
        }
    }
    // `ls-files -v` tags an assume-unchanged entry in lower case and a skip-worktree one `S`.
    let mut args = ARCHIVE_STATUS.to_vec();
    args.extend(["ls-files", "-v"]);
    let tagged = run(&git, top, &args, timeout, ARCHIVE_LISTING_CAP)
        .await
        .map_err(|err| format!("{err:#}"))?;
    if !tagged.ok || tagged.stdout_truncated {
        return Err("git could not list the worktree's files".into());
    }
    Ok(tagged
        .stdout
        .lines()
        .any(|line| line.starts_with(|tag: char| tag.is_ascii_lowercase() || tag == 'S')))
}

/// The most of a worktree's `ls-files -v` Archive reads (about 700,000 files); past it, it refuses.
const ARCHIVE_LISTING_CAP: usize = 64 * 1024 * 1024;

/// How long removing a worktree may take: its files are deleted.
const WORKTREE_REMOVE_TIMEOUT: Duration = Duration::from_secs(60);

/// The linked worktree `worktree` removed, from the main checkout `main`, never forced: git
/// itself refuses one with uncommitted or untracked changes ([`ARCHIVE_STATUS`]); files it
/// ignores go with it. `Err` is git's line.
pub async fn remove_worktree(
    main: &str,
    configured_bin: Option<&str>,
    worktree: &str,
) -> std::result::Result<(), String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let mut args = ARCHIVE_STATUS.to_vec();
    args.extend(["worktree", "remove", worktree]);
    git_line(&git, main, &args, WORKTREE_REMOVE_TIMEOUT).await
}

/// The pid of the claude whose own lock `reason` is on the worktree `worktree`: Claude Code locks a
/// `--worktree` it makes with "claude session <name> (pid N)", or "(pid N start <time>)" where `ps`
/// gave its start, `<name>` the worktree's folder name with a `/` in it written `+`, and leaves the
/// lock when its claude is stopped. `None` for any other lock.
pub fn claude_lock_pid(reason: &str, worktree: &str) -> Option<i32> {
    let folder = Path::new(worktree).file_name()?.to_str()?;
    let (name, rest) = reason
        .strip_prefix("claude session ")?
        .split_once(" (pid ")?;
    if name.replace('/', "+") != folder {
        return None;
    }
    let inner = rest.strip_suffix(')')?;
    let pid = match inner.split_once(' ') {
        Some((pid, start))
            if start
                .strip_prefix("start ")
                .is_some_and(|time| !time.is_empty()) =>
        {
            pid
        }
        Some(_) => return None,
        None => inner,
    };
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok().filter(|pid| *pid > 0)
}

/// Whether the process `pid` is alive: there, whether or not it is ours to signal.
pub fn pid_alive(pid: i32) -> bool {
    let signalled = unsafe { libc::kill(pid, 0) };
    signalled == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The linked worktree `worktree` unlocked, from the main checkout `main`. `Err` is git's line.
pub async fn unlock_worktree(
    main: &str,
    configured_bin: Option<&str>,
    worktree: &str,
) -> std::result::Result<(), String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    git_line(
        &git,
        main,
        &["worktree", "unlock", worktree],
        WORKTREE_REMOVE_TIMEOUT,
    )
    .await
}

/// The local `branch` deleted from `main`, only while its tip is `head_oid`. `Err` is the
/// reason it was kept.
pub async fn delete_branch(
    main: &str,
    configured_bin: Option<&str>,
    branch: &str,
    head_oid: &str,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let git = git_bin(configured_bin).ok_or("git is not installed at a known location")?;
    let tip = commit_oid(&git, main, &format!("refs/heads/{branch}"), timeout).await;
    if tip.as_deref() != Some(head_oid) {
        return Err(format!("{branch} moved"));
    }
    git_line(&git, main, &["branch", "-D", branch], timeout).await
}

/// Whether `oid` is already on `reference` (`merge-base --is-ancestor`); `None` when git
/// cannot say.
pub async fn on_reference(
    cwd: &str,
    configured_bin: Option<&str>,
    oid: &str,
    reference: &str,
    timeout: Duration,
) -> Option<bool> {
    let git = git_bin(configured_bin)?;
    let out = run(
        &git,
        cwd,
        &["merge-base", "--is-ancestor", oid, reference],
        timeout,
        PROBE_CAP,
    )
    .await
    .ok()?;
    match out.code {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// `args` run in `cwd`: `Err` is git's first line when it refuses.
async fn git_line(
    git: &Path,
    cwd: &str,
    args: &[&str],
    timeout: Duration,
) -> std::result::Result<(), String> {
    let out = run(git, cwd, args, timeout, PROBE_CAP)
        .await
        .map_err(|err| format!("{err:#}"))?;
    if out.ok {
        Ok(())
    } else {
        Err(out
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(300).collect())
            .unwrap_or_else(|| format!("git {} failed", args.join(" "))))
    }
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
            "--",
            ".",
            CLAUDE_WORKTREES,
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

/// Keep at most `cap` bytes from a pipe, then **close it**, or, `drain`ing, read the
/// rest to its end and drop it.
///
/// The close is the mechanism, not a side effect: the reader owns the pipe, so
/// returning drops it, the child takes `EPIPE` on its next write, and everything
/// else unblocks without anybody having to hold a kill handle. A write (a commit,
/// whose hooks may print at length) drains instead: a hook killed by `EPIPE` would
/// fail the commit, or leave it half done.
async fn read_capped<R>(mut pipe: R, cap: usize, drain: bool) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    (&mut pipe)
        .take(cap as u64)
        .read_to_end(&mut buffer)
        .await?;
    if drain {
        tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await?;
    }
    Ok(buffer)
}

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// The child produced more than the cap and was stopped.
    pub(crate) stdout_truncated: bool,
    /// Its exit code; `None` when it was stopped (at the cap, or by a signal).
    pub(crate) code: Option<i32>,
}

/// Run `command`, any short read-only child of ccd's (`gh`), as [`run`] runs git's: bounded
/// in time and bytes, stdin closed, killed on drop; `label` names it in errors.
pub(crate) async fn run_program(
    command: tokio::process::Command,
    label: &[&str],
    timeout: Duration,
    stdout_cap: usize,
) -> Result<Output> {
    run_child(command, label, timeout, stdout_cap, false).await
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
    run_child(command, args, timeout, stdout_cap, false).await
}

/// Output kept from a git child that writes (a commit): its hooks' words for a refusal.
const WRITE_OUTPUT_CAP: usize = 64 * 1024;

/// How long a write's output is still read once git has exited.
const WRITE_OUTPUT_GRACE: Duration = Duration::from_secs(1);

/// What a pipe's reader has within [`WRITE_OUTPUT_GRACE`]; past it the reader is stopped,
/// dropping the pipe, and what it had is lost.
async fn read_within_grace(
    task: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Vec<u8> {
    match tokio::time::timeout(WRITE_OUTPUT_GRACE, &mut *task).await {
        Ok(Ok(Ok(bytes))) => bytes,
        _ => {
            task.abort();
            Vec::new()
        }
    }
}

/// [`run`] for a git child that changes the repository, run through the person's login
/// shell with `environment` ([`crate::start_session::login_shell_command`]), so its hooks
/// find the tools a terminal finds (a `cargo fmt` or `npm` hook), as `git worktree add`
/// does. Every byte it prints is read (past [`WRITE_OUTPUT_CAP`], read and dropped), so it
/// is never cut off part way, and its exit status alone is the answer.
async fn run_write(
    git: &Path,
    cwd: &str,
    args: &[&str],
    timeout: Duration,
    environment: &std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
) -> Result<Output> {
    use std::ffi::OsStr;
    let mut argv: Vec<&OsStr> = vec![git.as_os_str(), OsStr::new("-C"), OsStr::new(cwd)];
    argv.extend(args.iter().map(OsStr::new));
    let command = crate::start_session::login_shell_write_command(environment, &argv);
    run_child(command, args, timeout, WRITE_OUTPUT_CAP, true).await
}

/// Run `command`, a git child (`args` names it in errors), as [`run`] describes.
async fn run_child(
    mut command: tokio::process::Command,
    args: &[&str],
    timeout: Duration,
    stdout_cap: usize,
    drain: bool,
) -> Result<Output> {
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
    let mut out_task = tokio::spawn(read_capped(stdout, stdout_cap.saturating_add(1), drain));
    let mut err_task = tokio::spawn(read_capped(stderr, MAX_STDERR_BYTES, drain));

    if drain {
        // A write is over when git exits, not when its pipes close: a hook may leave a
        // background job holding them. What was printed by then is read for a moment more.
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status.ok(),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                out_task.abort();
                err_task.abort();
                anyhow::bail!("git {} timed out", args.join(" "));
            }
        };
        let mut out_bytes = read_within_grace(&mut out_task).await;
        let err_bytes = read_within_grace(&mut err_task).await;
        let stdout_truncated = out_bytes.len() > stdout_cap;
        out_bytes.truncate(stdout_cap);
        return Ok(Output {
            ok: status.is_some_and(|status| status.success()),
            stdout: String::from_utf8_lossy(&out_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&err_bytes).into_owned(),
            stdout_truncated,
            code: status.and_then(|status| status.code()),
        });
    }

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
        code: if stdout_truncated {
            None
        } else {
            status.and_then(|status| status.code())
        },
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
                ("/r/app", Some("main"), false, None),
                (
                    "/r/app/.claude/worktrees/fix+login",
                    Some("worktree-fix+login"),
                    false,
                    Some("")
                ),
                ("/r/old", None, true, None),
                ("/r/a b\nc", Some("feature/x"), false, None),
            ]
            .map(|(path, branch, prunable, locked)| WorktreeEntry {
                path: path.into(),
                branch: branch.map(str::to_string),
                prunable,
                locked: locked.map(str::to_string),
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
                repo: Some(local_repo(&dir)),
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
                repo: Some(local_repo(&dir)),
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

    async fn run_git_of(dir: &Path) -> Result<protocol::ws::RunGit, String> {
        run_git(dir.to_str().unwrap(), None, Duration::from_secs(30)).await
    }

    /// A repository on `main` with one commit, pushed to a bare `origin` whose `HEAD` is
    /// `main`, so `refs/remotes/origin/HEAD` names it.
    fn repo_with_origin(tag: &str) -> (PathBuf, PathBuf) {
        let remote = temp_dir(&format!("{tag}-origin"));
        git_sync(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let dir = temp_dir(tag);
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1\n2\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        git_sync(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git_sync(&dir, &["push", "-q", "-u", "origin", "main"]);
        git_sync(&dir, &["remote", "set-head", "origin", "main"]);
        (dir, remote)
    }

    fn head_of(dir: &Path) -> String {
        let out = std::process::Command::new(git_bin(None).unwrap())
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// **A run's repository as its Changes panel reads it**: a branch pushed and level with
    /// its upstream, then a commit past it with a modified and an untracked file: ahead of
    /// the base, not pushed, and the changed lines the diff's files count.
    #[tokio::test]
    async fn a_runs_repository_reads_branch_base_lines_and_push_state() {
        let (dir, remote) = repo_with_origin("rungit");
        git_sync(&dir, &["switch", "-q", "-c", "feat/x"]);
        git_sync(&dir, &["push", "-q", "-u", "origin", "feat/x"]);
        let base = protocol::ws::RunGitBase {
            name: "main".into(),
            remote: "origin".into(),
            reference: "refs/remotes/origin/main".into(),
        };
        let level = run_git_of(&dir).await.unwrap();
        assert_eq!(level.branch.as_deref(), Some("feat/x"));
        assert_eq!(level.head_oid, Some(head_of(&dir)));
        assert_eq!(level.base.as_ref(), Some(&base));
        assert_eq!(level.ahead, Some(0));
        assert!(level.pushed, "level with its upstream is pushed");
        assert!(!level.unmerged);
        assert_eq!(
            level.changed,
            protocol::ws::ChangedFiles {
                files: 0,
                added: 0,
                removed: 0,
                complete: true
            }
        );

        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        git_sync(&dir, &["add", "b.txt"]);
        git_sync(&dir, &["commit", "-qm", "second"]);
        std::fs::write(dir.join("a.txt"), "1\ntwo\n3\n").unwrap();
        std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();
        let ahead = run_git_of(&dir).await.unwrap();
        assert_eq!(ahead.ahead, Some(1));
        assert!(
            !ahead.pushed,
            "a commit past the remote branch is not pushed"
        );
        let stat = diff_stat(dir.to_str().unwrap(), None, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            ahead.changed,
            protocol::ws::ChangedFiles {
                files: 2,
                added: stat.added,
                removed: stat.removed,
                complete: true
            }
        );
        assert_eq!((stat.added, stat.removed), (4, 1));

        std::fs::write(
            dir.join("big.txt"),
            vec![b'x'; UNTRACKED_READ_BUDGET as usize + 1],
        )
        .unwrap();
        let past = run_git_of(&dir).await.unwrap();
        assert_eq!(past.changed.files, 3);
        assert!(!past.changed.complete, "a file left uncounted");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&remote);
    }

    /// **Pushed means the remote has this branch at `HEAD`**: a branch made from
    /// `origin/main` tracks it and is not pushed; pushed without `-u` it is, with a commit
    /// past it it is not, and a remote branch of the same name on another remote is not the
    /// one it pushes to.
    #[tokio::test]
    async fn pushed_is_the_same_branch_on_its_remote_at_head() {
        let (dir, origin) = repo_with_origin("rungit-pushed");
        git_sync(
            &dir,
            &["switch", "-q", "-c", "from-main", "--track", "origin/main"],
        );
        let tracking_main = run_git_of(&dir).await.unwrap();
        assert!(
            !tracking_main.pushed,
            "level with origin/main, but origin has no from-main"
        );
        git_sync(&dir, &["push", "-q", "origin", "from-main"]);
        assert!(
            run_git_of(&dir).await.unwrap().pushed,
            "on origin at HEAD, though its upstream is still origin/main"
        );
        git_sync(&dir, &["commit", "-q", "--allow-empty", "-m", "more"]);
        assert!(!run_git_of(&dir).await.unwrap().pushed);

        let fork = temp_dir("rungit-pushed-fork");
        git_sync(&fork, &["init", "-q", "--bare", "-b", "main"]);
        git_sync(&dir, &["remote", "add", "fork", fork.to_str().unwrap()]);
        git_sync(&dir, &["push", "-q", "fork", "from-main"]);
        assert!(
            !run_git_of(&dir).await.unwrap().pushed,
            "on fork at HEAD, but it pushes to origin"
        );
        git_sync(&dir, &["push", "-q", "-u", "fork", "from-main"]);
        assert!(
            run_git_of(&dir).await.unwrap().pushed,
            "its upstream is fork's now"
        );

        git_sync(&dir, &["switch", "-q", "-c", "fork-only"]);
        git_sync(&dir, &["push", "-q", "origin", "fork-only"]);
        git_sync(&dir, &["config", "branch.fork-only.pushRemote", "fork"]);
        assert!(
            !run_git_of(&dir).await.unwrap().pushed,
            "on origin at HEAD, but it pushes to fork, which has no fork-only"
        );
        git_sync(&dir, &["push", "-q", "fork", "fork-only"]);
        assert!(
            run_git_of(&dir).await.unwrap().pushed,
            "now on fork at HEAD"
        );
        for path in [&dir, &origin, &fork] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// **Before the first commit, on a detached head, and outside a repository.**
    #[tokio::test]
    async fn a_runs_repository_without_a_head_branch_or_repository() {
        let dir = temp_dir("rungit-unborn");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1\n").unwrap();
        let unborn = run_git_of(&dir).await.unwrap();
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert_eq!(
            (unborn.head_oid, unborn.base, unborn.ahead),
            (None, None, None)
        );
        assert!(!unborn.pushed);
        assert_eq!((unborn.changed.files, unborn.changed.added), (1, 1));

        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        git_sync(&dir, &["checkout", "-q", "--detach"]);
        let detached = run_git_of(&dir).await.unwrap();
        assert_eq!(detached.branch, None);
        assert_eq!(detached.head_oid, Some(head_of(&dir)));
        assert_eq!(detached.base, None, "no remote, no base");
        assert_eq!(detached.ahead, None);
        let _ = std::fs::remove_dir_all(&dir);

        let plain = temp_dir("rungit-plain");
        assert_eq!(
            run_git_of(&plain).await,
            Err("not a git repository".to_string())
        );
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// **An unresolved merge reads `unmerged`.**
    #[tokio::test]
    async fn a_runs_repository_with_an_unresolved_merge() {
        let dir = temp_dir("rungit-conflict");
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "base"]);
        git_sync(&dir, &["switch", "-q", "-c", "other"]);
        std::fs::write(dir.join("a.txt"), "other\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "other"]);
        git_sync(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("a.txt"), "main\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "main"]);
        assert!(!run_git_of(&dir).await.unwrap().unmerged);
        let git = git_bin(None).unwrap();
        let merged = std::process::Command::new(git)
            .arg("-C")
            .arg(&dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "merge",
                "-q",
                "other",
            ])
            .output()
            .unwrap();
        assert!(!merged.status.success(), "the merge must conflict");
        assert!(run_git_of(&dir).await.unwrap().unmerged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The base is a fork's parent's default branch, else the upstream's remote's, else
    /// origin's, else the only remote's**; with no remote `HEAD` there is none.
    #[tokio::test]
    async fn a_runs_base_follows_the_upstreams_remote() {
        let (dir, origin) = repo_with_origin("rungit-base");
        let fork = temp_dir("rungit-base-fork");
        git_sync(&fork, &["init", "-q", "--bare", "-b", "trunk"]);
        git_sync(&dir, &["remote", "add", "fork", fork.to_str().unwrap()]);
        git_sync(&dir, &["push", "-q", "fork", "main:trunk"]);
        git_sync(&dir, &["remote", "set-head", "fork", "trunk"]);
        git_sync(&dir, &["switch", "-q", "-c", "feat"]);
        assert_eq!(
            run_git_of(&dir).await.unwrap().base.map(|base| base.remote),
            Some("origin".into()),
            "no upstream: origin"
        );
        git_sync(&dir, &["push", "-q", "-u", "fork", "feat"]);
        let base = run_git_of(&dir).await.unwrap().base.unwrap();
        assert_eq!(
            (
                base.name.as_str(),
                base.remote.as_str(),
                base.reference.as_str()
            ),
            ("trunk", "fork", "refs/remotes/fork/trunk"),
            "the upstream's remote"
        );
        let parent = temp_dir("rungit-base-parent");
        git_sync(&parent, &["init", "-q", "--bare", "-b", "main"]);
        git_sync(
            &dir,
            &["remote", "add", "upstream", parent.to_str().unwrap()],
        );
        git_sync(&dir, &["push", "-q", "upstream", "main"]);
        git_sync(&dir, &["remote", "set-head", "upstream", "main"]);
        assert_eq!(
            run_git_of(&dir)
                .await
                .unwrap()
                .base
                .map(|base| base.reference),
            Some("refs/remotes/upstream/main".into()),
            "a fork's parent, whatever the branch is pushed to"
        );
        git_sync(&dir, &["remote", "remove", "upstream"]);
        let _ = std::fs::remove_dir_all(&parent);
        git_sync(&dir, &["remote", "remove", "origin"]);
        git_sync(&dir, &["branch", "-q", "--unset-upstream"]);
        assert_eq!(
            run_git_of(&dir).await.unwrap().base.map(|base| base.remote),
            Some("fork".into()),
            "the only remote with a HEAD"
        );
        git_sync(&dir, &["remote", "set-head", "fork", "-d"]);
        let none = run_git_of(&dir).await.unwrap();
        assert_eq!((none.base, none.ahead), (None, None));
        for path in [&dir, &origin, &fork] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// **A folder git cannot open leaves the count incomplete**: git lists what it can and
    /// warns, and a tree that only looks clean is not called complete.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_untracked_folder_git_cannot_open_leaves_the_count_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, remote) = repo_with_origin("rungit-unreadable");
        let data = dir.join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("x.txt"), "x\n").unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o000)).unwrap();
        let still_readable = std::fs::read_dir(&data).is_ok();
        let read = run_git_of(&dir).await.unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !still_readable {
            assert_eq!(read.changed.files, 0, "git listed nothing inside it");
            assert!(!read.changed.complete, "{:?}", read.changed);
        }
        assert!(
            run_git_of(&dir).await.unwrap().changed.complete,
            "readable again"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&remote);
    }

    /// **`ahead` counts from the commit the read reports**: a commit made while the read is
    /// under way (here by a git that commits just before `rev-list`) changes neither.
    #[cfg(unix)]
    #[tokio::test]
    async fn ahead_is_counted_from_the_head_the_read_reports() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, remote) = repo_with_origin("rungit-race");
        let before = head_of(&dir);
        let real = git_bin(None).unwrap();
        let racing = dir.join("..").join(format!(
            "{}-racing-git",
            dir.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::write(
            &racing,
            format!(
                "#!/bin/sh\ncase \" $* \" in *\" rev-list \"*) '{git}' -C '{dir}' -c user.name=t -c user.email=t@e commit -q --allow-empty -m race ;; esac\nexec '{git}' \"$@\"\n",
                git = real.display(),
                dir = dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&racing, std::fs::Permissions::from_mode(0o755)).unwrap();
        let read = run_git(
            dir.to_str().unwrap(),
            Some(racing.to_str().unwrap()),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_ne!(head_of(&dir), before, "the racing commit landed");
        assert_eq!(read.head_oid, Some(before));
        assert_eq!(read.ahead, Some(0), "counted from the head it reports");
        let _ = std::fs::remove_file(&racing);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&remote);
    }

    async fn commit_in(dir: &Path, message: &str) -> CommitOutcome {
        commit(
            dir.to_str().unwrap(),
            None,
            message,
            Duration::from_secs(30),
            &login_environment(dir, "/usr/bin:/bin"),
        )
        .await
    }

    /// A login shell's environment for a test: a temporary home inside `dir`, `/bin/sh`,
    /// and `path`.
    fn login_environment(
        dir: &Path,
        path: &str,
    ) -> std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> {
        let home = dir.join(".git").join("test-home");
        std::fs::create_dir_all(&home).unwrap();
        [
            ("HOME", home.into_os_string()),
            ("SHELL", "/bin/sh".into()),
            ("PATH", path.into()),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value))
        .collect()
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(git_bin(None).unwrap())
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// **A conflict block is the merge's own**: blocks labelled with the two commits compared
    /// (a renamed file's `:path` after them, a person's wider markers) are read; a file quoting
    /// markers of its own, or a block left open, gives none rather than a wrong hunk.
    #[test]
    fn a_conflict_block_is_the_merges_own() {
        let (head, base) = ("a".repeat(40), "b".repeat(40));
        let hunk = |ours: &str, theirs: &str| protocol::ws::ConflictHunk {
            ours: ours.into(),
            theirs: theirs.into(),
        };
        let merged = format!(
            "one\n<<<<<<< {head}\nFEAT\n=======\nMAIN\n>>>>>>> {base}\ntwo\n\
             <<<<<<<<<< {head}:new.txt\nx\r\n==========\n>>>>>>>>>> {base}:old.txt\n"
        );
        assert_eq!(
            conflict_blocks(&merged, &head, &base),
            Some(vec![hunk("FEAT\n", "MAIN\n"), hunk("x\r\n", "")])
        );
        assert_eq!(conflict_blocks("no markers\n", &head, &base), Some(vec![]));
        let quoted = format!(
            "<<<<<<< HEAD\nexample\n=======\nother\n>>>>>>> topic\n\
             <<<<<<< {head}\nFEAT\n=======\nMAIN\n>>>>>>> {base}\n"
        );
        assert_eq!(conflict_blocks(&quoted, &head, &base), None);
        let open = format!("<<<<<<< {head}\nFEAT\n=======\nMAIN\n");
        assert_eq!(conflict_blocks(&open, &head, &base), None);
        let underlined =
            format!("Title\n=======\n\n<<<<<<< {head}\nFEAT\n=======\nMAIN\n>>>>>>> {base}\n");
        assert_eq!(
            conflict_blocks(&underlined, &head, &base),
            Some(vec![hunk("FEAT\n", "MAIN\n")]),
            "a heading's underline is not a marker"
        );
        let entries = format!(
            "Changes\n=======\n\n<<<<<<< {head}\n1.1.0\n=====\n\n- feat\n=======\n\
             1.0.1\n==================\n\n- fix\n>>>>>>> {base}\n"
        );
        assert_eq!(
            conflict_blocks(&entries, &head, &base),
            Some(vec![hunk(
                "1.1.0\n=====\n\n- feat\n",
                "1.0.1\n==================\n\n- fix\n"
            )]),
            "underlines inside a block are its lines"
        );
        let twice = format!("<<<<<<< {head}\nA\n=======\nB\n=======\nC\n>>>>>>> {base}\n");
        assert_eq!(
            conflict_blocks(&twice, &head, &base),
            None,
            "a second separator is not git's"
        );
        let other_commit = format!(
            "<<<<<<< {}\nFEAT\n=======\nMAIN\n>>>>>>> {base}\n",
            "c".repeat(40)
        );
        assert_eq!(conflict_blocks(&other_commit, &head, &base), None);
    }

    /// **A branch is compared with its base as git merges them**: the base fetched from its
    /// remote where it moved on, the conflicted files named by git, a text file's hunks, a file
    /// deleted on one side with none to show, a path with no conflict refused; and the base
    /// compared with itself merges cleanly.
    #[tokio::test]
    async fn a_branch_is_compared_with_its_base_as_git_merges_them() {
        let (dir, remote) = repo_with_origin("conflicts");
        let cwd = dir.to_str().unwrap();
        let timeout = Duration::from_secs(10);
        std::fs::write(dir.join("gone.txt"), "x\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "gone"]);
        git_sync(&dir, &["push", "-q", "origin", "main"]);
        // The base moves on at its remote, and this clone has not fetched it.
        let other = temp_dir("conflicts-other");
        git_sync(&other, &["clone", "-q", remote.to_str().unwrap(), "."]);
        std::fs::write(other.join("a.txt"), "1\nMAIN\n").unwrap();
        git_sync(&other, &["rm", "-q", "gone.txt"]);
        git_sync(&other, &["commit", "-qam", "main"]);
        git_sync(&other, &["push", "-q", "origin", "main"]);
        git_sync(&dir, &["switch", "-q", "-c", "feat"]);
        std::fs::write(dir.join("a.txt"), "1\nFEAT\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "x\nmine\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "feat"]);

        let ends = conflict_ends(cwd, None, timeout).await.unwrap();
        assert_eq!(ends.head_oid, head_of(&dir));
        assert_eq!(ends.base.reference, "refs/remotes/origin/main");
        assert!(ends.common_dir.ends_with(".git"), "{}", ends.common_dir);
        let stale = ref_oid(cwd, None, &ends.base.reference, timeout)
            .await
            .unwrap();
        let environment = login_environment(&dir, "/usr/bin:/bin");
        fetch_base(cwd, None, &ends.base, &environment)
            .await
            .unwrap();
        let base_oid = ref_oid(cwd, None, &ends.base.reference, timeout)
            .await
            .unwrap();
        assert_eq!(base_oid, head_of(&other), "the base as its remote has it");
        assert_ne!(base_oid, stale);

        let (clean, files, complete) = conflicted_files(cwd, None, &ends.head_oid, &base_oid)
            .await
            .unwrap();
        assert!(!clean);
        assert_eq!(files, ["a.txt", "gone.txt"]);
        assert!(complete);
        let hunks = conflict_hunks(cwd, None, &ends.head_oid, &base_oid, "a.txt", timeout)
            .await
            .unwrap();
        assert_eq!(
            hunks,
            [protocol::ws::ConflictHunk {
                ours: "FEAT\n".into(),
                theirs: "MAIN\n".into()
            }]
        );
        assert_eq!(
            conflict_hunks(cwd, None, &ends.head_oid, &base_oid, "gone.txt", timeout).await,
            Ok(vec![]),
            "deleted on one side: no lines to show"
        );
        assert!(
            conflict_hunks(cwd, None, &ends.head_oid, &base_oid, "b.txt", timeout)
                .await
                .is_err()
        );
        assert_eq!(
            conflicted_files(cwd, None, &base_oid, &base_oid).await,
            Ok((true, vec![], true))
        );
        for path in [dir, remote, other] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// **A run in a subfolder reads its conflicts from the repository's top**: the path is named
    /// from the top and its hunks are that file's, not a root file of the same name; a valid
    /// UTF-8 file holding a replacement character is still read; a file whose markers git did
    /// not write answers why.
    #[tokio::test]
    async fn a_run_in_a_subfolder_reads_its_conflicts_from_the_top() {
        let dir = committable("subfolder");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "1\n2\n").unwrap();
        std::fs::write(dir.join("a.txt"), "root\n").unwrap();
        std::fs::write(dir.join("q.md"), "quoted\n<<<<<<< HEAD\n1\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "base"]);
        git_sync(&dir, &["switch", "-q", "-c", "feat"]);
        std::fs::write(dir.join("sub/a.txt"), "\u{FFFD} kept\nFEAT\n").unwrap();
        std::fs::write(dir.join("q.md"), "quoted\n<<<<<<< HEAD\nFEAT\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "feat"]);
        let head = head_of(&dir);
        git_sync(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("sub/a.txt"), "\u{FFFD} kept\nMAIN\n").unwrap();
        std::fs::write(dir.join("q.md"), "quoted\n<<<<<<< HEAD\nMAIN\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "main"]);
        let base = head_of(&dir);
        let sub = dir.join("sub");
        let cwd = sub.to_str().unwrap();
        let timeout = Duration::from_secs(10);
        assert_eq!(
            conflicted_files(cwd, None, &head, &base).await,
            Ok((
                false,
                vec!["q.md".to_string(), "sub/a.txt".to_string()],
                true
            ))
        );
        assert_eq!(
            conflict_hunks(cwd, None, &head, &base, "sub/a.txt", timeout).await,
            Ok(vec![protocol::ws::ConflictHunk {
                ours: "FEAT\n".into(),
                theirs: "MAIN\n".into()
            }])
        );
        assert!(
            conflict_hunks(cwd, None, &head, &base, "q.md", timeout)
                .await
                .is_err(),
            "marker-like lines of its own: a note, not an empty list"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **A conflict git names no file for is not clean**: one side splits a folder between two
    /// new ones and the other adds a file to it; git says it conflicts and lists no path.
    #[tokio::test]
    async fn a_conflict_git_names_no_file_for_is_not_clean() {
        let dir = committable("split");
        std::fs::create_dir(dir.join("dir")).unwrap();
        std::fs::write(dir.join("dir/a"), "a\n").unwrap();
        std::fs::write(dir.join("dir/b"), "b\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "base"]);
        git_sync(&dir, &["switch", "-q", "-c", "split"]);
        std::fs::create_dir(dir.join("x")).unwrap();
        std::fs::create_dir(dir.join("y")).unwrap();
        git_sync(&dir, &["mv", "dir/a", "x/a"]);
        git_sync(&dir, &["mv", "dir/b", "y/b"]);
        git_sync(&dir, &["commit", "-qm", "split"]);
        let split = head_of(&dir);
        git_sync(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("dir/c"), "c\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "add"]);
        let main = head_of(&dir);
        assert_eq!(
            conflicted_files(dir.to_str().unwrap(), None, &split, &main).await,
            Ok((false, vec![], true))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **A base is fetched with nothing that can ask**: the fetch's ssh is given no askpass
    /// program and told never to ask, and a fetch that fails answers git's line.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_base_is_fetched_with_nothing_that_can_ask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = committable("fetch-asks");
        let ssh = dir.join(".git").join("ssh");
        let seen = dir.join(".git").join("ssh.env");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\necho \"askpass=${{GIT_ASKPASS-unset}} ssh=${{SSH_ASKPASS_REQUIRE-unset}}\" > '{}'\n\
                 echo 'Permission denied (publickey).' >&2\nexit 255\n",
                seen.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_sync(
            &dir,
            &["remote", "add", "origin", "ssh://git@example.invalid/x.git"],
        );
        git_sync(&dir, &["config", "core.sshCommand", ssh.to_str().unwrap()]);
        let base = protocol::ws::RunGitBase {
            name: "main".into(),
            remote: "origin".into(),
            reference: "refs/remotes/origin/main".into(),
        };
        let environment = login_environment(&dir, "/usr/bin:/bin");
        let refused = fetch_base(dir.to_str().unwrap(), None, &base, &environment).await;
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap().trim(),
            "askpass= ssh=never"
        );
        assert_eq!(refused, Err("Permission denied (publickey).".into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **Claude Code's own worktree lock is read as Claude writes it**: "claude session <name>
    /// (pid N)", with its start time after N where `ps` gave one, and a `/` in the name written `+` in
    /// the folder; any other reason, or another worktree's name, is no Claude lock.
    #[test]
    fn claude_code_s_own_worktree_lock_is_read_as_claude_writes_it() {
        let at = "/r/app/.claude/worktrees/x";
        assert_eq!(
            claude_lock_pid("claude session x (pid 12345)", at),
            Some(12345)
        );
        assert_eq!(
            claude_lock_pid(
                "claude session x (pid 12345 start Fri Nov 27 10:11:12 2026)",
                at
            ),
            Some(12345)
        );
        assert_eq!(
            claude_lock_pid(
                "claude session fix/login (pid 7)",
                "/r/app/.claude/worktrees/fix+login"
            ),
            Some(7)
        );
        for other in [
            "claude session y (pid 12345)",
            "claude session x (pid )",
            "claude session x (pid 0)",
            "claude session x (pid 12a45)",
            "claude session x (pid 12345) and a note",
            "claude session x (pid 12345 started yesterday)",
            "claude agent",
            "",
        ] {
            assert_eq!(claude_lock_pid(other, at), None, "{other:?}");
        }
    }

    /// **A Claude worktree in the main checkout is not one of its changes**: `claude --worktree` makes
    /// `.claude/worktrees/<name>` inside the repository and ignores it nowhere, so git lists it as
    /// untracked. The run in the main checkout counts no change for it, a commit there never stages it
    /// (as a gitlink no clone could resolve), and with nothing else changed there is nothing to commit.
    #[tokio::test]
    async fn a_claude_worktree_in_the_main_checkout_is_not_one_of_its_changes() {
        let dir = committable("claude-worktree");
        git_sync(
            &dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "worktree-fix+login",
                ".claude/worktrees/fix+login",
            ],
        );
        let cwd = dir.to_str().unwrap();
        let read = run_git(cwd, None, Duration::from_secs(10)).await.unwrap();
        assert_eq!(read.changed.files, 0, "{:?}", read.changed);
        assert!(matches!(
            commit_in(&dir, "nothing").await,
            CommitOutcome::Refused {
                code: protocol::ws::git_refusal::NOTHING_TO_COMMIT,
                ..
            }
        ));

        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        assert_eq!(
            run_git(cwd, None, Duration::from_secs(10))
                .await
                .unwrap()
                .changed
                .files,
            1
        );
        let CommitOutcome::Committed { files, .. } = commit_in(&dir, "edit a").await else {
            panic!("not committed");
        };
        assert_eq!(files, 1);
        let tree = git_out(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(!tree.contains(".claude"), "{tree}");

        // The run's diff names its untracked files, not the worktree.
        std::fs::write(dir.join("new.txt"), "n\n").unwrap();
        let diff = collect(cwd, None, 512 * 1024, Duration::from_secs(10)).await;
        assert!(diff.unified.contains("new.txt"), "{}", diff.unified);
        assert!(
            !diff.unified.contains(".claude/worktrees"),
            "{}",
            diff.unified
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A repository on `main` with one commit, its own identity, and nothing else.
    fn committable(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        git_sync(&dir, &["init", "-q", "-b", "main"]);
        git_sync(&dir, &["config", "user.name", "t"]);
        git_sync(&dir, &["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("a.txt"), "1\n").unwrap();
        git_sync(&dir, &["add", "."]);
        git_sync(&dir, &["commit", "-qm", "first"]);
        dir
    }

    /// **Commit takes every change**: a modified and an untracked file, committed as HEAD
    /// with the message, two files, and a clean tree after; the first commit of a new
    /// repository too.
    #[tokio::test]
    async fn a_commit_takes_every_change_and_names_its_commit() {
        let dir = committable("commit");
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        std::fs::write(dir.join("new.txt"), "n\n").unwrap();
        let CommitOutcome::Committed {
            sha,
            files,
            message,
        } = commit_in(&dir, "Fix the thing").await
        else {
            panic!("not committed");
        };
        assert_eq!(message, "Fix the thing");
        assert_eq!(sha, head_of(&dir));
        assert_eq!(files, 2);
        assert_eq!(
            git_out(&dir, &["log", "-1", "--format=%s"]).trim(),
            "Fix the thing"
        );
        assert_eq!(
            git_out(&dir, &["status", "--porcelain"]),
            "",
            "nothing left"
        );
        let _ = std::fs::remove_dir_all(&dir);

        let fresh = temp_dir("commit-unborn");
        git_sync(&fresh, &["init", "-q", "-b", "main"]);
        git_sync(&fresh, &["config", "user.name", "t"]);
        git_sync(&fresh, &["config", "user.email", "t@example.com"]);
        std::fs::write(fresh.join("a.txt"), "1\n").unwrap();
        assert!(matches!(
            commit_in(&fresh, "first").await,
            CommitOutcome::Committed { files: 1, .. }
        ));
        let _ = std::fs::remove_dir_all(&fresh);
    }

    /// **What git commits is what is told**: a change outside the run's subfolder counts with
    /// `diff.relative` set, a `commit-msg` hook's rewritten message is the one named, a commit
    /// of more files than a listing holds is counted, and a `post-commit` hook's background
    /// job holding the output does not hold the answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commit_tells_what_git_committed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = committable("commit-told");
        std::fs::create_dir(dir.join("sub")).unwrap();
        git_sync(&dir, &["config", "diff.relative", "true"]);
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        let sub = dir.join("sub");
        assert!(
            matches!(
                commit_in(&sub, "outside the subfolder").await,
                CommitOutcome::Committed { files: 1, .. }
            ),
            "a relative diff hides nothing that was staged"
        );

        let hook = dir.join(".git/hooks/commit-msg");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf '[T-1] %s' \"$(cat \"$1\")\" > \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "3\n").unwrap();
        let CommitOutcome::Committed { message, .. } = commit_in(&dir, "Fix it").await else {
            panic!("not committed");
        };
        assert_eq!(message, "[T-1] Fix it");
        std::fs::remove_file(&hook).unwrap();

        let many = dir.join("node_modules/a-package-with-a-rather-long-name/lib");
        std::fs::create_dir_all(&many).unwrap();
        for at in 0..16_000 {
            std::fs::write(many.join(format!("index-file-number-{at:05}.js")), "x\n").unwrap();
        }
        assert!(
            matches!(
                commit_in(&dir, "many").await,
                CommitOutcome::Committed { files: 16_000, .. }
            ),
            "counted past what one listing holds"
        );

        let post = dir.join(".git/hooks/post-commit");
        std::fs::write(&post, "#!/bin/sh\n( sleep 5 ) &\nexit 0\n").unwrap();
        std::fs::set_permissions(&post, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "4\n").unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            commit_in(&dir, "bg").await,
            CommitOutcome::Committed { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A commit's hooks find what the person's login shell finds**: a pre-commit hook
    /// running a tool that only the person's `~/.profile` puts on `PATH` (as `cargo` or
    /// `npm` are in a terminal, not in the service's environment) lets it commit.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commits_hooks_find_the_login_shells_tools() {
        use std::os::unix::fs::PermissionsExt;
        let dir = committable("commit-path");
        let tools = dir.join(".git/test-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let tool = tools.join("fmtcheck-only-in-a-terminal");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nfmtcheck-only-in-a-terminal\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        let environment = login_environment(&dir, "/usr/bin:/bin");
        // A login shell's own profile may set PATH outright (Debian's does); the person's
        // `~/.profile` is read after it.
        std::fs::write(
            Path::new(&environment[std::ffi::OsStr::new("HOME")]).join(".profile"),
            format!("PATH=\"{}:$PATH\"\nexport PATH\n", tools.display()),
        )
        .unwrap();
        let outcome = commit(
            dir.to_str().unwrap(),
            None,
            "with the tool",
            Duration::from_secs(30),
            &environment,
        )
        .await;
        assert!(
            matches!(outcome, CommitOutcome::Committed { .. }),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A hook's words are its own under an interactive bash**: with bash as the login shell
    /// (Linux's usual one) a refusing hook's line is what the commit says, not bash's own
    /// "no job control" lines from starting interactive with no terminal; and `~/.bashrc` still
    /// runs past Debian's guard for interactive shells, so the hook finds a tool it puts on `PATH`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hooks_words_are_its_own_under_an_interactive_bash() {
        use std::os::unix::fs::PermissionsExt;
        let dir = committable("commit-bash");
        let tools = dir.join(".git/test-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let tool = tools.join("lint-only-in-bashrc");
        std::fs::write(
            &tool,
            "#!/bin/sh
echo 'lint: 3 problems' >&2
exit 1
",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh
lint-only-in-bashrc
",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        let mut environment = login_environment(&dir, "/usr/bin:/bin");
        environment.insert("SHELL".into(), "/bin/bash".into());
        let home = Path::new(&environment[std::ffi::OsStr::new("HOME")]).to_path_buf();
        std::fs::write(
            home.join(".bashrc"),
            format!(
                "case $- in *i*) ;; *) return;; esac\nPATH=\"{}:$PATH\"\nexport PATH\n",
                tools.display()
            ),
        )
        .unwrap();
        std::fs::write(home.join(".bash_profile"), ". ~/.bashrc\n").unwrap();
        assert_eq!(
            commit(
                dir.to_str().unwrap(),
                None,
                "m",
                Duration::from_secs(30),
                &environment
            )
            .await,
            CommitOutcome::Refused {
                code: protocol::ws::git_refusal::GIT_FAILED,
                detail: Some("lint: 3 problems".into())
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A signed commit is named with `log.showSignature` set**: git's check of the signature
    /// is not read as the sha.
    #[tokio::test]
    async fn a_signed_commit_is_named_whatever_the_log_shows() {
        let dir = committable("commit-signed");
        let key = dir.join(".git/test-signing-key");
        let made = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "t@example.com", "-f"])
            .arg(&key)
            .status();
        if !made.is_ok_and(|status| status.success()) {
            eprintln!("no ssh-keygen here: a signed commit is not made");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let signers = dir.join(".git/allowed-signers");
        std::fs::write(&signers, format!("t@example.com {public}")).unwrap();
        for (name, value) in [
            ("gpg.format", "ssh"),
            (
                "user.signingkey",
                key.with_extension("pub").to_str().unwrap(),
            ),
            ("commit.gpgsign", "true"),
            ("gpg.ssh.allowedSignersFile", signers.to_str().unwrap()),
            ("log.showSignature", "true"),
        ] {
            git_sync(&dir, &["config", name, value]);
        }
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        let outcome = commit_in(&dir, "signed").await;
        assert_eq!(
            outcome,
            CommitOutcome::Committed {
                sha: head_of(&dir),
                files: 1,
                message: "signed".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Refused without staging anything**: nothing changed, a merge with unresolved files
    /// (its index left as it was), and a detached head.
    #[tokio::test]
    async fn a_commit_is_refused_before_it_stages_anything() {
        use protocol::ws::git_refusal as code;
        let dir = committable("commit-refused");
        assert_eq!(
            commit_in(&dir, "m").await,
            CommitOutcome::Refused {
                code: code::NOTHING_TO_COMMIT,
                detail: None
            }
        );

        git_sync(&dir, &["switch", "-q", "-c", "other"]);
        std::fs::write(dir.join("a.txt"), "other\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "other"]);
        git_sync(&dir, &["switch", "-q", "main"]);
        std::fs::write(dir.join("a.txt"), "main\n").unwrap();
        git_sync(&dir, &["commit", "-qam", "main"]);
        let _ = std::process::Command::new(git_bin(None).unwrap())
            .arg("-C")
            .arg(&dir)
            .args(["merge", "-q", "other"])
            .output();
        let unresolved = git_out(&dir, &["ls-files", "--unmerged"]);
        assert!(!unresolved.is_empty(), "the merge conflicts");
        std::fs::write(dir.join("loose.txt"), "x\n").unwrap();
        assert_eq!(
            commit_in(&dir, "m").await,
            CommitOutcome::Refused {
                code: code::UNMERGED,
                detail: None
            }
        );
        assert_eq!(
            git_out(&dir, &["ls-files", "--unmerged"]),
            unresolved,
            "the index is as it was"
        );
        assert_eq!(
            git_out(&dir, &["ls-files", "--", "loose.txt"]),
            "",
            "nothing staged"
        );
        git_sync(&dir, &["merge", "--abort"]);

        git_sync(&dir, &["checkout", "-q", "--detach"]);
        assert_eq!(
            commit_in(&dir, "m").await,
            CommitOutcome::Refused {
                code: code::DETACHED,
                detail: None
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The repository's hooks run as for the person's own commit**: a failing pre-commit
    /// hook refuses it with the hook's words, and one printing far past the runner's cap on
    /// both streams (git sends a hook's output to its stderr) lets it commit: every byte is
    /// read, so the hook, stopping at its first failure as hooks do, is never cut short.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commits_hooks_run_and_a_verbose_one_is_never_cut_short() {
        use std::os::unix::fs::PermissionsExt;
        let dir = committable("commit-hooks");
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'lint: 3 problems' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "2\n").unwrap();
        assert_eq!(
            commit_in(&dir, "m").await,
            CommitOutcome::Refused {
                code: protocol::ws::git_refusal::GIT_FAILED,
                detail: Some("lint: 3 problems".into())
            }
        );

        let mark = dir.join("hook-finished");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nset -e\nhead -c 2000000 /dev/zero | tr '\\0' o\nhead -c 2000000 /dev/zero | tr '\\0' e >&2\ntouch '{}'\n",
                mark.display()
            ),
        )
        .unwrap();
        let outcome = commit_in(&dir, "m").await;
        assert!(
            matches!(outcome, CommitOutcome::Committed { files: 1, .. }),
            "{outcome:?}"
        );
        assert!(mark.exists(), "the hook ran to its end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `git status --porcelain=v2 --branch -z`, read as git writes it: a rename's original
    /// path that looks like an entry is not one.
    #[test]
    fn status_reads_as_git_writes_it() {
        assert_eq!(
            parse_status(
                "# branch.oid abc\0# branch.head feat/x\0# branch.upstream origin/feat/x\0\
                 # branch.ab +0 -0\x002 R. N... 100644 100644 100644 1 1 R100 new\0u fake\0"
            ),
            StatusHead {
                oid: Some("abc".into()),
                branch: Some("feat/x".into()),
                unmerged: false,
            }
        );
        assert_eq!(
            parse_status("# branch.oid (initial)\0# branch.head (detached)\0# branch.ab +1 -0\0u UU N... 1 2 3 4 a b c a.txt\0"),
            StatusHead {
                oid: None,
                branch: None,
                unmerged: true,
            }
        );
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
        assert!(run_git_of(&dir).await.is_ok());
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

    /// **A remote is one key however its URL spells it**: https, ssh and scp forms, a trailing
    /// `.git` or `/`, and any case give one `github.com/<owner>/<name>`; another host gives none.
    #[test]
    fn a_remote_is_one_key_however_it_is_spelled() {
        for url in [
            "https://github.com/Faisal/CodeConnect.git",
            "https://github.com/faisal/codeconnect",
            "https://github.com/faisal/codeconnect/",
            "ssh://git@github.com/Faisal/CodeConnect.git",
            "git@GitHub.com:FAISAL/codeconnect.git",
        ] {
            assert_eq!(
                remote_key(url).as_deref(),
                Some("github.com/faisal/codeconnect"),
                "{url}"
            );
        }
        for url in [
            "https://gitlab.com/faisal/codeconnect.git",
            "git@ghe.example.com:faisal/codeconnect.git",
            "/srv/git/codeconnect.git",
            "",
        ] {
            assert_eq!(remote_key(url), None, "{url}");
        }
    }

    async fn repo_of(dir: &Path) -> Option<protocol::event::RepoFact> {
        repo_fact(dir.to_str().unwrap(), None, Duration::from_secs(10)).await
    }

    fn real(dir: &Path) -> String {
        dir.canonicalize().unwrap().to_str().unwrap().to_string()
    }

    /// The repository a checkout with no remote is read as.
    fn local_repo(dir: &Path) -> protocol::event::RepoFact {
        protocol::event::RepoFact {
            name: dir.file_name().unwrap().to_str().unwrap().into(),
            remote: None,
            main_path: real(dir),
        }
    }

    /// **A run's repository**: its main checkout and folder name, and its GitHub `origin` as one
    /// key: the same for two checkouts of one repository, another for a fork; none with no
    /// `origin`, with only another remote, or on another host; nothing outside git.
    #[tokio::test]
    async fn a_run_is_read_into_its_repository() {
        let one = committable("repo-one");
        git_sync(
            &one,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Faisal/App.git",
            ],
        );
        let two = committable("repo-two");
        git_sync(
            &two,
            &["remote", "add", "origin", "git@github.com:faisal/app"],
        );
        let fork = committable("repo-fork");
        git_sync(
            &fork,
            &["remote", "add", "origin", "https://github.com/someone/app"],
        );
        git_sync(
            &fork,
            &["remote", "add", "upstream", "https://github.com/faisal/app"],
        );
        let local = committable("repo-local");
        let other = committable("repo-other");
        git_sync(
            &other,
            &["remote", "add", "upstream", "https://github.com/faisal/app"],
        );
        let hosted = committable("repo-hosted");
        git_sync(
            &hosted,
            &["remote", "add", "origin", "https://gitlab.com/faisal/app"],
        );

        let fact = repo_of(&one).await.expect("a repository");
        assert_eq!(fact.main_path, real(&one));
        assert_eq!(fact.name, one.file_name().unwrap().to_str().unwrap());
        assert_eq!(fact.remote.as_deref(), Some("github.com/faisal/app"));
        let second = repo_of(&two).await.unwrap();
        assert_eq!(second.remote, fact.remote, "two checkouts, one repository");
        assert_ne!(second.main_path, fact.main_path);
        assert_eq!(
            repo_of(&fork).await.unwrap().remote.as_deref(),
            Some("github.com/someone/app")
        );
        for dir in [&local, &other, &hosted] {
            let fact = repo_of(dir).await.unwrap();
            assert_eq!((fact.main_path, fact.remote), (real(dir), None), "{dir:?}");
        }
        assert_eq!(repo_of(&temp_dir("repo-plain")).await, None);
    }

    /// **A folder in a repository reads as its main checkout**: a subfolder, a
    /// `.claude/worktrees` worktree and one elsewhere all name the main checkout; a submodule
    /// names its own folder; a repository with no commit yet is read too, and the same after
    /// its first.
    #[tokio::test]
    async fn a_folder_in_a_repository_reads_as_its_main_checkout() {
        let main = committable("repo-main");
        let top = real(&main);
        std::fs::create_dir_all(main.join("src/deep")).unwrap();
        git_sync(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "worktree-x",
                ".claude/worktrees/x",
            ],
        );
        let elsewhere = temp_dir("repo-elsewhere").join("wt");
        git_sync(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "far",
                elsewhere.to_str().unwrap(),
            ],
        );
        for dir in [
            main.join("src/deep"),
            main.join(".claude/worktrees/x"),
            elsewhere.clone(),
        ] {
            let fact = repo_of(&dir).await.unwrap();
            assert_eq!(fact.main_path, top, "{dir:?}");
            assert_eq!(fact.name, main.file_name().unwrap().to_str().unwrap());
        }

        let child = committable("repo-child");
        git_sync(
            &main,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.to_str().unwrap(),
                "vendor/child",
            ],
        );
        assert_eq!(
            repo_of(&main.join("vendor/child")).await.unwrap().main_path,
            real(&main.join("vendor/child"))
        );

        let empty = temp_dir("repo-empty");
        git_sync(&empty, &["init", "-q", "-b", "main"]);
        let before = repo_of(&empty).await.expect("a repository with no commit");
        assert_eq!(before.main_path, real(&empty));
        std::fs::write(empty.join("a.txt"), "1\n").unwrap();
        git_sync(&empty, &["add", "."]);
        git_sync(&empty, &["commit", "-qm", "first"]);
        assert_eq!(repo_of(&empty).await, Some(before));
    }
}
