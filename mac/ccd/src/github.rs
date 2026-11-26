//! A run's GitHub state for its Changes panel: whether this computer's `gh` can read the
//! repository on github.com, and the branch's pull request. Read only, through `gh api`, and at
//! most once per [`WINDOW`] per branch and commit, whatever asks.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use protocol::ws::{GhState, PrState, PullRequest};
use tokio::sync::Mutex;

/// How long one read of a branch's GitHub state answers every read of it at the same commit.
pub const WINDOW: Duration = Duration::from_secs(30);
/// How long a host's reads stop after GitHub refused one for its rate limit.
const PAUSE: Duration = Duration::from_secs(300);
/// A pulls call answers in well under a second; on a network dropping packets the Changes panel
/// waits at most two of these.
const API_TIMEOUT: Duration = Duration::from_secs(5);
const API_CAP: usize = 4 * 1024 * 1024;

/// A repository on github.com, as a remote's URL names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// The github.com repository a remote's URL names, read as data: `https://github.com/owner/name`,
/// `ssh://[user@]github.com[:port]/owner/name` or `user@github.com:owner/name`, a trailing `.git`
/// or `/` dropped. `None` for any other host (GitHub Enterprise, `ssh.github.com`, an SSH host
/// alias among them), a local path, or a path that is not exactly owner and name.
pub fn parse_remote(url: &str) -> Option<Repo> {
    let url = url.trim();
    let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        rest.split_once('/')?
    } else {
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority, path)
    };
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let fine = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    (host == "github.com" && fine(owner) && fine(name)).then(|| Repo {
        host,
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// What one read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub gh: GhState,
    pub pr: Option<PullRequest>,
    /// When GitHub was asked; `None` when it was not: no gh, no account in it, or the host's
    /// reads paused.
    pub read_at: Option<String>,
}

/// Every branch's last read, and the hosts whose reads are paused, for the daemon's life.
#[derive(Default)]
pub struct Reads {
    state: Mutex<State>,
    /// Held across a read of GitHub: reads asked together wait for the first and share it.
    one_at_a_time: Mutex<()>,
}

#[derive(Default)]
struct State {
    cached: HashMap<String, (Instant, Read)>,
    paused: HashMap<String, Instant>,
}

impl Reads {
    /// The GitHub state of `branch` from `head` at `head_oid`, proposed to `target`: the last
    /// read when it is under [`WINDOW`] old, else a new one (counted, failed or not). A host
    /// GitHub asked to wait reads `unknown` until [`PAUSE`] has passed. `gh` is looked for only
    /// when GitHub is asked: config `gh_bin`, else the person's login shell's `PATH`.
    pub async fn read(
        &self,
        gh_bin: Option<&str>,
        head: &Repo,
        target: &Repo,
        branch: &str,
        head_oid: Option<&str>,
    ) -> Read {
        let key = format!(
            "{}{}",
            branch_key(head, target, branch),
            head_oid.unwrap_or_default()
        );
        if let Some(read) = self.answered(&key, &target.host).await {
            return read;
        }
        let _one_at_a_time = self.one_at_a_time.lock().await;
        if let Some(read) = self.answered(&key, &target.host).await {
            return read;
        }
        let (read, rate_limited) = read_github(gh_bin, head, target, branch, head_oid).await;
        let mut state = self.state.lock().await;
        if rate_limited {
            state
                .paused
                .insert(target.host.clone(), Instant::now() + PAUSE);
        }
        state.cached.retain(|_, (at, _)| at.elapsed() < WINDOW);
        state.cached.insert(key, (Instant::now(), read.clone()));
        read
    }

    /// Drop `branch`'s reads at every commit: the next read asks GitHub (after an action on it).
    pub async fn forget(&self, head: &Repo, target: &Repo, branch: &str) {
        // After the read in flight, so its answer from before the action is not kept.
        let _one_at_a_time = self.one_at_a_time.lock().await;
        let prefix = branch_key(head, target, branch);
        self.state
            .lock()
            .await
            .cached
            .retain(|key, _| !key.starts_with(&prefix));
    }

    /// The answer for `key` without asking GitHub: its read under [`WINDOW`] old, or `unknown`
    /// while `host` is paused.
    async fn answered(&self, key: &str, host: &str) -> Option<Read> {
        let state = self.state.lock().await;
        if let Some((at, read)) = state.cached.get(key) {
            if at.elapsed() < WINDOW {
                return Some(read.clone());
            }
        }
        state
            .paused
            .get(host)
            .is_some_and(|until| Instant::now() < *until)
            .then_some(Read {
                gh: GhState::Unknown,
                pr: None,
                read_at: None,
            })
    }
}

/// The start of a branch's cache keys; the commit follows.
fn branch_key(head: &Repo, target: &Repo, branch: &str) -> String {
    format!(
        "{}/{}/{} {}:{branch} ",
        target.host, target.owner, target.name, head.owner
    )
}

/// Why GitHub gave no answer to [`open_pr`].
#[derive(Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// No `gh` on this computer.
    Missing,
    /// gh has no account for the host, or GitHub refused its token.
    SignedOut,
    /// GitHub could not be reached, asked to wait, or gave no pulls list.
    Down,
}

/// The open pull request from `branch` of `head` into `target`, asked of GitHub now (no
/// cache); `None` when there is none, or two.
pub async fn open_pr(
    gh_bin: Option<&str>,
    head: &Repo,
    target: &Repo,
    branch: &str,
) -> Result<Option<PullRequest>, Unavailable> {
    let environment = &crate::start_session::phone_environment();
    let Some(gh) = crate::start_session::gh_path(environment, gh_bin).await else {
        return Err(Unavailable::Missing);
    };
    let from = format!("head={}:{branch}", head.owner);
    match pulls(
        &gh,
        environment,
        target,
        &["state=open", &from, "per_page=2"],
    )
    .await
    {
        Ok(open) => Ok(open_from(&open, head, branch).flatten()),
        Err(Failed::NoAccount | Failed::TokenRefused) => Err(Unavailable::SignedOut),
        Err(Failed::RateLimited | Failed::Other) => Err(Unavailable::Down),
    }
}

/// How long `gh pr create` may take.
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);

/// What [`create_pr`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Created {
    Opened(protocol::ws::PrLink),
    /// gh refused: its first line.
    Failed(String),
    /// gh gave no answer in time; GitHub may have it.
    Unknown,
}

/// `gh pr create` from `branch` of `head` into `target` onto `base`, its title and body gh's
/// own from the branch's commits, which it reads with git in `cwd`, the run's folder.
pub async fn create_pr(
    cwd: &str,
    gh_bin: Option<&str>,
    head: &Repo,
    target: &Repo,
    branch: &str,
    base: &str,
) -> Created {
    let environment = &crate::start_session::phone_environment();
    let Some(gh) = crate::start_session::gh_path(environment, gh_bin).await else {
        return Created::Failed("gh is not installed".into());
    };
    let repo = format!("{}/{}", target.owner, target.name);
    let same = head.owner.eq_ignore_ascii_case(&target.owner)
        && head.name.eq_ignore_ascii_case(&target.name);
    let from = if same {
        branch.to_string()
    } else {
        format!("{}:{branch}", head.owner)
    };
    let args = [
        "pr",
        "create",
        "--repo",
        repo.as_str(),
        "--head",
        from.as_str(),
        "--base",
        base,
        "--fill",
    ];
    let mut command = gh_command(&gh, environment, &args);
    command.current_dir(cwd);
    match crate::git::run_program(command, &args, CREATE_TIMEOUT, CREATE_CAP).await {
        Ok(out) if out.ok => match created(&out.stdout, target) {
            Some(link) => Created::Opened(link),
            None => Created::Failed(format!("gh answered no pull request of {repo}")),
        },
        Ok(out) => Created::Failed(crate::git::first_words(&out).unwrap_or_default()),
        Err(err) if format!("{err:#}").contains("timed out") => Created::Unknown,
        Err(err) => Created::Failed(format!("{err:#}")),
    }
}

/// The pull request of `target` whose URL `gh pr create` printed last.
fn created(stdout: &str, target: &Repo) -> Option<protocol::ws::PrLink> {
    let url = stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())?;
    let prefix = format!(
        "https://{}/{}/{}/pull/",
        target.host, target.owner, target.name
    );
    let rest = url
        .get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(&prefix))
        .and(url.get(prefix.len()..))?;
    Some(protocol::ws::PrLink {
        number: rest.parse().ok()?,
        url: url.to_string(),
    })
}

/// `gh pr create`'s output is a URL and a few lines of progress.
const CREATE_CAP: usize = 64 * 1024;

/// `gh` run as ccd runs it: the person's environment, no prompt, no update notice, no colour,
/// no pager.
fn gh_command(
    gh: &Path,
    environment: &HashMap<OsString, OsString>,
    args: &[&str],
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(gh);
    command
        .env_clear()
        .envs(environment)
        .envs([
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
            ("NO_COLOR", "1"),
            ("GH_PAGER", "cat"),
        ])
        .args(args);
    command
}

/// One read, and whether GitHub refused a call for its rate limit. The sign-in state is the
/// pulls call's own answer: no `gh auth status`, whose flags differ across gh releases and
/// which reads a network failure as a bad token.
async fn read_github(
    gh_bin: Option<&str>,
    head: &Repo,
    target: &Repo,
    branch: &str,
    head_oid: Option<&str>,
) -> (Read, bool) {
    let read = |gh, pr| Read {
        gh,
        pr,
        read_at: Some(protocol::time::now_rfc3339()),
    };
    let environment = &crate::start_session::phone_environment();
    let Some(gh) = crate::start_session::gh_path(environment, gh_bin).await else {
        let missing = Read {
            gh: GhState::Missing,
            pr: None,
            read_at: None,
        };
        return (missing, false);
    };
    match pull_request(&gh, environment, head, target, branch, head_oid).await {
        Ok(Some(mut pr)) if pr.state == PrState::Open => {
            match checks(&gh, environment, target, &pr).await {
                Ok(Some((list, complete))) => {
                    pr.checks = Some(list);
                    pr.checks_complete = Some(complete);
                }
                Err(Failed::RateLimited) => return (read(GhState::SignedIn, Some(pr)), true),
                // Not read: the pull request stands without them.
                Ok(None) | Err(_) => {}
            }
            (read(GhState::SignedIn, Some(pr)), false)
        }
        Ok(pr) => (read(GhState::SignedIn, pr), false),
        Err(Failed::TokenRefused) => (read(GhState::SignedOut, None), false),
        Err(Failed::NoAccount) => {
            let signed_out = Read {
                gh: GhState::SignedOut,
                pr: None,
                read_at: None,
            };
            (signed_out, false)
        }
        Err(Failed::RateLimited) => (read(GhState::Unknown, None), true),
        Err(Failed::Other) => (read(GhState::Unknown, None), false),
    }
}

fn said(out: &crate::git::Output, words: &[&str]) -> bool {
    let text = format!("{}\n{}", out.stdout, out.stderr).to_ascii_lowercase();
    words.iter().any(|word| text.contains(word))
}

/// Why a pulls call gave no answer.
enum Failed {
    /// gh has no account for the host: it says to run `gh auth login` without asking GitHub.
    NoAccount,
    /// GitHub refused gh's token (HTTP 401).
    TokenRefused,
    RateLimited,
    /// Anything else: GitHub unreachable, slow, or an answer that is not the pulls list.
    Other,
}

/// The branch's pull request on `target`, looked up by its owner (so a fork's PR on its
/// parent is found, and a branch named like a number is a branch): the one open from it; else
/// the newest closed from it, only when its head is the commit checked out. Two open: none.
async fn pull_request(
    gh: &Path,
    environment: &HashMap<OsString, OsString>,
    head: &Repo,
    target: &Repo,
    branch: &str,
    head_oid: Option<&str>,
) -> Result<Option<PullRequest>, Failed> {
    let from = format!("head={}:{branch}", head.owner);
    let open = pulls(
        gh,
        environment,
        target,
        &["state=open", &from, "per_page=2"],
    )
    .await?;
    if let Some(found) = open_from(&open, head, branch) {
        return Ok(found);
    }
    let closed = pulls(
        gh,
        environment,
        target,
        &[
            "state=closed",
            &from,
            "sort=updated",
            "direction=desc",
            "per_page=1",
        ],
    )
    .await?;
    Ok(closed_at(&closed, head, branch, head_oid))
}

/// Of the open pulls, the one from `branch` of `head`; `Some(None)` when two are (none is
/// shown); `None` when none is, so the closed ones are asked.
fn open_from(open: &[serde_json::Value], head: &Repo, branch: &str) -> Option<Option<PullRequest>> {
    let mut from = open.iter().filter(|pr| from_branch(pr, head, branch));
    let first = from.next()?;
    Some(if from.next().is_some() {
        None
    } else {
        pull(first)
    })
}

/// Of the closed pulls, the one from `branch` of `head` whose head is the commit checked out.
fn closed_at(
    closed: &[serde_json::Value],
    head: &Repo,
    branch: &str,
    head_oid: Option<&str>,
) -> Option<PullRequest> {
    closed
        .iter()
        .filter(|pr| from_branch(pr, head, branch))
        .find(|pr| head_oid.is_some() && pr["head"]["sha"].as_str() == head_oid)
        .and_then(pull)
}

async fn pulls(
    gh: &Path,
    environment: &HashMap<OsString, OsString>,
    target: &Repo,
    query: &[&str],
) -> Result<Vec<serde_json::Value>, Failed> {
    let path = format!("repos/{}/{}/pulls", target.owner, target.name);
    let mut args = vec![
        "api",
        "--hostname",
        target.host.as_str(),
        "--method",
        "GET",
        path.as_str(),
    ];
    for field in query {
        args.extend(["-f", field]);
    }
    let out = crate::git::run_program(
        gh_command(gh, environment, &args),
        &args,
        API_TIMEOUT,
        API_CAP,
    )
    .await
    .map_err(|_| Failed::Other)?;
    if out.ok && !out.stdout_truncated {
        return serde_json::from_str(&out.stdout).map_err(|_| Failed::Other);
    }
    Err(if said(&out, &["rate limit"]) {
        Failed::RateLimited
    } else if said(&out, &["http 401"]) {
        Failed::TokenRefused
    } else if said(&out, &["gh auth login"]) {
        Failed::NoAccount
    } else {
        Failed::Other
    })
}

/// A pulls entry from `branch` of `head`.
fn from_branch(pr: &serde_json::Value, head: &Repo, branch: &str) -> bool {
    pr["head"]["ref"].as_str() == Some(branch)
        && pr["head"]["repo"]["full_name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(&format!("{}/{}", head.owner, head.name)))
}

/// The checks query: an open pull request's last commit and its checks, each check run with its
/// first annotations, in one request.
const CHECKS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { \
    pullRequest(number: $number) { commits(last: 1) { nodes { commit { oid statusCheckRollup { contexts(first: 100) { \
    pageInfo { hasNextPage } nodes { __typename \
    ... on CheckRun { name status conclusion startedAt completedAt detailsUrl annotations(first: 10) { nodes { annotationLevel message } } } \
    ... on StatusContext { context state targetUrl } } } } } } } } } }";

/// `pr`'s checks at its head, asked of GitHub with [`CHECKS_QUERY`]; `None` when the answer is
/// not one this daemon reads.
async fn checks(
    gh: &Path,
    environment: &HashMap<OsString, OsString>,
    target: &Repo,
    pr: &PullRequest,
) -> Result<Option<(Vec<protocol::ws::Check>, bool)>, Failed> {
    let query = format!("query={CHECKS_QUERY}");
    // Owner and name as strings (`-f`): `-F` would read an all-digit name as a number.
    let owner = format!("owner={}", target.owner);
    let name = format!("name={}", target.name);
    let number = format!("number={}", pr.number);
    let args = [
        "api",
        "graphql",
        "--hostname",
        target.host.as_str(),
        "-f",
        query.as_str(),
        "-f",
        owner.as_str(),
        "-f",
        name.as_str(),
        "-F",
        number.as_str(),
    ];
    let out = crate::git::run_program(
        gh_command(gh, environment, &args),
        &args,
        API_TIMEOUT,
        API_CAP,
    )
    .await
    .map_err(|_| Failed::Other)?;
    if !out.ok || out.stdout_truncated {
        return Err(if said(&out, &["rate limit"]) {
            Failed::RateLimited
        } else {
            Failed::Other
        });
    }
    let answer: serde_json::Value = serde_json::from_str(&out.stdout).map_err(|_| Failed::Other)?;
    Ok(checks_from(&answer, &pr.head_oid))
}

/// An open pull request's checks from ccd's checks query's answer, when its commit is
/// `head_oid`: sorted, and whether that is all of them.
fn checks_from(
    answer: &serde_json::Value,
    head_oid: &str,
) -> Option<(Vec<protocol::ws::Check>, bool)> {
    let commit = &answer["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"];
    let oid = commit["oid"].as_str()?;
    if oid != head_oid {
        return Some((Vec::new(), false));
    }
    let contexts = &commit["statusCheckRollup"]["contexts"];
    if contexts.is_null() {
        return Some((Vec::new(), true));
    }
    let complete = contexts["pageInfo"]["hasNextPage"].as_bool() == Some(false);
    let text = |value: &serde_json::Value| value.as_str().map(str::to_string);
    let mut checks: Vec<protocol::ws::Check> = contexts["nodes"]
        .as_array()?
        .iter()
        .filter_map(|node| {
            Some(match node["__typename"].as_str()? {
                "CheckRun" => {
                    let state = run_state(node["status"].as_str(), node["conclusion"].as_str());
                    protocol::ws::Check {
                        name: node["name"].as_str()?.to_string(),
                        state,
                        started_at: text(&node["startedAt"]),
                        completed_at: text(&node["completedAt"]),
                        url: text(&node["detailsUrl"]),
                        failure_line: (state == protocol::ws::CheckState::Fail)
                            .then(|| failure_line(&node["annotations"]["nodes"]))
                            .flatten(),
                    }
                }
                "StatusContext" => protocol::ws::Check {
                    name: node["context"].as_str()?.to_string(),
                    state: status_state(node["state"].as_str()),
                    started_at: None,
                    completed_at: None,
                    url: text(&node["targetUrl"]),
                    failure_line: None,
                },
                _ => return None,
            })
        })
        .collect();
    checks.sort_by(|a, b| (a.state, &a.name).cmp(&(b.state, &b.name)));
    Some((checks, complete))
}

/// A check run's state from its `status` and `conclusion`.
fn run_state(status: Option<&str>, conclusion: Option<&str>) -> protocol::ws::CheckState {
    use protocol::ws::CheckState::*;
    if status != Some("COMPLETED") {
        return Pending;
    }
    match conclusion {
        Some("SUCCESS") => Pass,
        Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED") => Fail,
        Some("CANCELLED") => Cancelled,
        Some("SKIPPED" | "NEUTRAL" | "STALE") => Skipped,
        _ => Unknown,
    }
}

/// A commit status's state.
fn status_state(state: Option<&str>) -> protocol::ws::CheckState {
    use protocol::ws::CheckState::*;
    match state {
        Some("SUCCESS") => Pass,
        Some("FAILURE" | "ERROR") => Fail,
        Some("PENDING" | "EXPECTED") => Pending,
        _ => Unknown,
    }
}

/// The first line of a check run's first failure annotation that says more than the runner's
/// "Process completed with exit code N.", at most 300 characters.
fn failure_line(annotations: &serde_json::Value) -> Option<String> {
    annotations
        .as_array()?
        .iter()
        .filter(|annotation| annotation["annotationLevel"].as_str() == Some("FAILURE"))
        .filter_map(|annotation| annotation["message"].as_str())
        .map(|message| message.lines().next().unwrap_or_default().trim())
        .find(|line| !line.is_empty() && !line.starts_with("Process completed with exit code"))
        .map(|line| line.chars().take(300).collect())
}

/// A pulls entry as the wire carries it.
fn pull(pr: &serde_json::Value) -> Option<PullRequest> {
    let merged_at = pr["merged_at"].as_str().map(str::to_string);
    Some(PullRequest {
        number: pr["number"].as_u64()?,
        url: pr["html_url"].as_str()?.to_string(),
        state: match (pr["state"].as_str()?, &merged_at) {
            (_, Some(_)) => PrState::Merged,
            ("open", None) => PrState::Open,
            _ => PrState::Closed,
        },
        base: pr["base"]["ref"].as_str()?.to_string(),
        head_oid: pr["head"]["sha"].as_str()?.to_string(),
        merged_at,
        checks: None,
        checks_complete: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: &str = include_str!("../../../fixtures/github/pulls-open.json");
    const CLOSED: &str = include_str!("../../../fixtures/github/pulls-closed.json");

    fn repo(owner: &str, name: &str) -> Repo {
        Repo {
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// A remote's URL in each form a clone has, and the ones that name no repository.
    #[test]
    fn a_remote_names_its_repository() {
        for url in [
            "https://github.com/cli/cli.git",
            "https://github.com/cli/cli",
            "https://github.com/cli/cli/",
            "git@github.com:cli/cli.git",
            "ssh://git@github.com:22/cli/cli.git",
            "https://user@GitHub.com/cli/cli.git",
        ] {
            assert_eq!(parse_remote(url), Some(repo("cli", "cli")), "{url}");
        }
        for url in [
            "git@ghe.example.com:team/app.git",
            "https://gitlab.com/team/app.git",
            "ssh://git@ssh.github.com:443/cli/cli.git",
            "git@github-work:cli/cli.git",
            "/srv/git/app.git",
            "file:///srv/git/app.git",
            "https://gitlab.com/group/sub/app.git",
            "https://github.com/cli",
            "https://github.com/cli/cli/pulls",
            "git@github.com:-x/app.git",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    /// **The pull request is found by its branch's owner**, in GitHub's own answer for
    /// cli/cli: the open one from the repository's own branch, the open one from a fork's,
    /// none for another branch, and none when two open are from it.
    #[test]
    fn the_open_pull_request_is_the_one_from_the_branch() {
        let open: Vec<serde_json::Value> = serde_json::from_str(OPEN).unwrap();
        let ours = open_from(
            &open,
            &repo("cli", "cli"),
            "dependabot/go_modules/github.com/sigstore/sigstore-go-1.4.1",
        );
        assert!(
            matches!(
                ours,
                Some(Some(PullRequest {
                    number: 15173,
                    state: PrState::Open,
                    ..
                }))
            ),
            "{ours:?}"
        );
        let fork = open_from(
            &open,
            &repo("lazerg", "cli-2"),
            "fix/issue-14946-existing-linked-branch",
        );
        assert!(
            matches!(fork, Some(Some(PullRequest { number: 15111, .. }))),
            "{fork:?}"
        );
        assert_eq!(
            open_from(
                &open,
                &repo("cli", "cli"),
                "fix/issue-14946-existing-linked-branch"
            ),
            None,
            "the same branch name from another owner is not ours"
        );
        let twice = [open[0].clone(), open[0].clone()];
        assert_eq!(
            open_from(
                &twice,
                &repo("cli", "cli"),
                "dependabot/go_modules/github.com/sigstore/sigstore-go-1.4.1"
            ),
            Some(None)
        );
    }

    /// **gh's answer is the target's pull request, or none**: its URL printed last, of the target
    /// repository (any case), else nothing.
    #[test]
    fn a_created_pull_request_is_the_targets() {
        let target = repo("cli", "cli");
        let link = created("Creating pull request for w2 into trunk in cli/cli\n\nhttps://github.com/cli/cli/pull/15200\n", &target);
        assert_eq!(link.map(|link| link.number), Some(15200));
        assert_eq!(
            created("https://github.com/CLI/Cli/pull/7\n", &target).map(|link| link.number),
            Some(7)
        );
        for out in [
            "https://github.com/someone/cli/pull/7\n",
            "https://github.com/abc/xyz/pull/7\n",
            "https://github.com/cli/cli/pull/x\n",
            "",
            "done\n",
        ] {
            assert_eq!(created(out, &target), None, "{out:?}");
        }
    }

    const CHECKS: &str = include_str!("../../../fixtures/github/checks-graphql.json");

    /// **An open pull request's checks, as the panel shows them**, from the checks query's
    /// answer (written from GitHub's schema): each state named, sorted failed first, a
    /// check run's times and log, a status's log alone, and a failure's line when it says
    /// more than the runner's exit code.
    #[test]
    fn a_pull_requests_checks_are_read_and_sorted() {
        use protocol::ws::{Check, CheckState::*};
        let answer: serde_json::Value = serde_json::from_str(CHECKS).unwrap();
        let (checks, complete) =
            checks_from(&answer, "8a93995195c4e5a299e227a00f236bf88199674a").unwrap();
        assert!(complete);
        let named: Vec<_> = checks
            .iter()
            .map(|check| (check.name.as_str(), check.state))
            .collect();
        assert_eq!(
            named,
            vec![
                ("lint", Fail),
                ("test (macos)", Fail),
                ("ci/circleci", Pending),
                ("deploy preview", Pending),
                ("flaky e2e", Cancelled),
                ("a new kind", Unknown),
                ("build (ubuntu)", Pass),
                ("license/cla", Pass),
                ("docs", Skipped),
            ]
        );
        let by = |name: &str| {
            checks
                .iter()
                .find(|check| check.name == name)
                .unwrap()
                .clone()
        };
        assert_eq!(
            by("test (macos)"),
            Check {
                name: "test (macos)".into(),
                state: Fail,
                started_at: Some("2026-11-26T09:00:05Z".into()),
                completed_at: Some("2026-11-26T09:07:00Z".into()),
                url: Some("https://github.com/cli/cli/actions/runs/1/job/12".into()),
                failure_line: Some("pkg/cmd/pr/create_test.go:212: expected 2 calls, got 3".into()),
            }
        );
        assert_eq!(
            by("lint").failure_line,
            None,
            "the runner's exit code says nothing more"
        );
        assert_eq!(by("deploy preview").completed_at, None);
        let status = by("ci/circleci");
        assert_eq!(
            (status.started_at, status.url.as_deref()),
            (None, Some("https://circleci.com/gh/cli/cli/1"))
        );
        assert_eq!(by("a new kind").url, None);
    }

    /// **Checks of another commit are not shown, and a list past 100 says so**: the head moved
    /// between the reads (none, incomplete), another page (incomplete), and no checks at all
    /// (empty, complete).
    #[test]
    fn checks_of_another_head_or_past_a_page_are_incomplete() {
        let answer: serde_json::Value = serde_json::from_str(CHECKS).unwrap();
        assert_eq!(checks_from(&answer, "0000000"), Some((Vec::new(), false)));
        let mut more = answer.clone();
        let rollup = &mut more["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]
            ["commit"]["statusCheckRollup"];
        rollup["contexts"]["pageInfo"]["hasNextPage"] = serde_json::Value::Bool(true);
        let (checks, complete) =
            checks_from(&more, "8a93995195c4e5a299e227a00f236bf88199674a").unwrap();
        assert_eq!((checks.len(), complete), (9, false));
        let mut none = answer;
        none["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
            ["statusCheckRollup"] = serde_json::Value::Null;
        assert_eq!(
            checks_from(&none, "8a93995195c4e5a299e227a00f236bf88199674a"),
            Some((Vec::new(), true))
        );
        assert_eq!(checks_from(&serde_json::json!({"errors": []}), "x"), None);
    }

    /// **A closed pull request is the branch's only at the commit checked out**: GitHub's
    /// merged one reads merged at its head, and nothing at another commit.
    #[test]
    fn a_merged_pull_request_is_shown_at_its_head_only() {
        let closed: Vec<serde_json::Value> = serde_json::from_str(CLOSED).unwrap();
        let pr = &closed[0];
        let branch = pr["head"]["ref"].as_str().unwrap();
        let sha = pr["head"]["sha"].as_str().unwrap();
        let merged = closed_at(&closed, &repo("cli", "cli"), branch, Some(sha)).unwrap();
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.head_oid, sha);
        assert!(merged.merged_at.is_some());
        assert_eq!(
            closed_at(&closed, &repo("cli", "cli"), branch, Some("0000000")),
            None
        );
        assert_eq!(closed_at(&closed, &repo("cli", "cli"), branch, None), None);
    }
}
