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
            "{}/{}/{} {}:{branch} {}",
            target.host,
            target.owner,
            target.name,
            head.owner,
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
