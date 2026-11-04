//! Starting a session from the phone: the folders it can pick from, and the start.
//!
//! Everything the daemon runs on the phone's behalf goes through the user's own
//! login shell ([`login_shell_command`]), so the launcher and the agent it starts
//! see the PATH, version managers and credentials a new terminal would — not the
//! daemon's service environment.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use protocol::agent::AgentKind;
use protocol::launch_result::{LaunchResult, ENV_LAUNCH_RESULT};
use protocol::ws::{
    AgentAvailability, ClientMessage, Project, ProjectGit, ServerMessage, StartFailure,
    StartSessionResult, StartWorktree, Worktree,
};
use tokio::sync::watch;

use crate::state::Daemon;

/// How long the launcher may run before it is taken to be hung and killed.
///
/// The launcher gives up on its own after 75 s (a Codex start can take eleven
/// seconds when nothing is wrong), so while it runs its answer is the one that
/// counts; this is only the backstop for one that never ends.
const START_TIMEOUT: Duration = Duration::from_secs(80);

/// How long one folder's git branch may take.
const BRANCH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one git read for a project's worktrees and branches may take.
const GIT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest worktree name, as Claude Code's `--worktree` takes it.
const MAX_WORKTREE_NAME_BYTES: usize = 64;

/// How many folders' branches are read at once.
const BRANCH_READS_AT_ONCE: usize = 8;

/// How long the login shell and the launcher may take to say an agent is there.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The agents the phone can start.
const AGENTS: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

/// The answer to a `list_projects` or `start_session`.
pub async fn answer(daemon: &Arc<Daemon>, message: ClientMessage) -> ServerMessage {
    match message {
        ClientMessage::ListProjects => list_projects(daemon).await,
        ClientMessage::GetProjectGit { path } => project_git(daemon, path).await,
        ClientMessage::StartSession {
            request_id,
            path,
            agent,
            prompt,
            worktree,
        } => {
            let state_dir = protocol::root_dir();
            let launch = launch(
                &daemon.db,
                daemon.config.git_bin.as_deref(),
                crate::state::codex_launcher(),
                &state_dir,
                &path,
                &agent,
                prompt.as_deref(),
                worktree.as_ref(),
            );
            ServerMessage::StartSessionResult {
                result: daemon.starts.once(&request_id, launch).await,
                request_id,
            }
        }
        _ => unreachable!(
            "handle_client passes only list_projects, get_project_git and start_session here"
        ),
    }
}

/// Which agents could start, as the last check found: a project list answers
/// from it rather than wait seconds on login shells. Empty until the first list
/// after the daemon starts, which waits for the check.
#[derive(Default)]
pub struct KnownAgents(Mutex<Option<Vec<AgentAvailability>>>);

impl KnownAgents {
    fn get(&self) -> Option<Vec<AgentAvailability>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(crate) fn set(&self, agents: Vec<AgentAvailability>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(agents);
    }
}

/// The agents for a project list: the last check's answer, with a fresh check
/// started for the next list; or, the first time, a check waited for.
async fn agents(daemon: &Arc<Daemon>) -> Vec<AgentAvailability> {
    if let Some(known) = daemon.agents.get() {
        let daemon = Arc::clone(daemon);
        tokio::spawn(async move { daemon.agents.set(check_agents().await) });
        return known;
    }
    let checked = check_agents().await;
    daemon.agents.set(checked.clone());
    checked
}

async fn check_agents() -> Vec<AgentAvailability> {
    futures_util::future::join_all(AGENTS.iter().map(agent_availability)).await
}

/// Starts already asked for, by `request_id`: in memory, so a repeat on this
/// connection or after a reconnect gets the first outcome and starts nothing.
/// A daemon restart forgets them.
#[derive(Default)]
pub struct Starts(Mutex<HashMap<String, watch::Receiver<Option<StartSessionResult>>>>);

impl Starts {
    /// The outcome for `request_id`: `launch`'s, run once however often it is asked.
    pub(crate) async fn once(
        &self,
        request_id: &str,
        launch: impl std::future::Future<Output = StartSessionResult>,
    ) -> StartSessionResult {
        let (first, mut outcome) = {
            let mut asked = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match asked.get(request_id) {
                Some(outcome) => (None, outcome.clone()),
                None => {
                    let (first, outcome) = watch::channel(None);
                    asked.insert(request_id.to_string(), outcome.clone());
                    (Some(first), outcome)
                }
            }
        };
        if let Some(first) = first {
            first.send_replace(Some(launch.await));
        }
        let result = match outcome.wait_for(Option::is_some).await {
            Ok(result) => result.clone().expect("waited until there is one"),
            Err(_) => failed(
                StartFailure::LaunchFailed,
                "the start was interrupted".into(),
            ),
        };
        result
    }
}

fn failed(reason: StartFailure, detail: String) -> StartSessionResult {
    StartSessionResult::Failed { reason, detail }
}

/// Run `codeconnect <agent> --no-attach [--worktree=<name>] [-- <prompt>]`
/// through the user's login shell, in `path` or the worktree `worktree` names, and
/// read what it started from its result line, a file in `state_dir`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn launch(
    db: &crate::db::Db,
    git_bin: Option<&str>,
    launcher: Result<PathBuf, String>,
    state_dir: &Path,
    path: &str,
    agent: &AgentKind,
    prompt: Option<&str>,
    worktree: Option<&StartWorktree>,
) -> StartSessionResult {
    if !matches!(agent, AgentKind::Claude | AgentKind::Codex) {
        return failed(
            StartFailure::LaunchFailed,
            format!("{} cannot be started from the phone", agent.as_str()),
        );
    }
    if prompt.is_some_and(|prompt| prompt.len() > protocol::ws::MAX_COMPOSE_BYTES) {
        return failed(
            StartFailure::LaunchFailed,
            "the first message is longer than 8 KiB".into(),
        );
    }
    if let Err((_, why)) = may_name(db, path).await {
        return failed(StartFailure::LaunchFailed, why);
    }
    if !Path::new(path).is_dir() {
        return failed(
            StartFailure::FolderMissing,
            format!("{path} no longer exists"),
        );
    }
    let launcher = match launcher {
        Ok(launcher) => launcher,
        Err(why) => return failed(StartFailure::LaunchFailed, why),
    };
    let place = match worktree {
        None => Place {
            cwd: path.to_string(),
            known: true,
            flag: None,
        },
        Some(worktree) => match place(git_bin, path, agent, worktree).await {
            Ok(place) => place,
            Err(why) => return failed(StartFailure::LaunchFailed, why),
        },
    };
    let result_path = match protocol::uid::new() {
        Ok(uid) => state_dir.join(format!("launch-{uid}.json")),
        Err(err) => {
            return failed(
                StartFailure::LaunchFailed,
                format!("could not name the launch: {err}"),
            )
        }
    };
    let mut args: Vec<&OsStr> = vec![
        launcher.as_os_str(),
        OsStr::new(agent.as_str()),
        OsStr::new("--no-attach"),
    ];
    if let Some(flag) = &place.flag {
        args.push(OsStr::new(flag));
    }
    if let Some(prompt) = prompt {
        args.extend([OsStr::new("--"), OsStr::new(prompt)]);
    }
    let mut command = login_shell_command(&args);
    command
        .current_dir(&place.cwd)
        .env(ENV_LAUNCH_RESULT, &result_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // No terminal ever told this session its type; this is the one it gets.
    if std::env::var_os("TERM").is_none() {
        command.env("TERM", "xterm-256color");
    }
    let output = tokio::time::timeout(START_TIMEOUT, command.output()).await;
    let written = LaunchResult::read_from(&result_path);
    let _ = std::fs::remove_file(&result_path);
    match (written, output) {
        (Ok(LaunchResult::Started { uid, .. }), _) => StartSessionResult::Started {
            session_uid: uid,
            cwd: place.known.then_some(place.cwd),
        },
        (Ok(LaunchResult::Failed { error, detail }), _) => failed(error, detail),
        (Err(_), Err(_)) => failed(
            StartFailure::LaunchFailed,
            format!(
                "the session did not finish starting within {} s",
                START_TIMEOUT.as_secs()
            ),
        ),
        (Err(_), Ok(Err(err))) => failed(
            StartFailure::LaunchFailed,
            format!("could not run the login shell: {err}"),
        ),
        (Err(_), Ok(Ok(output))) => failed(
            StartFailure::LaunchFailed,
            last_line(&output.stderr)
                .or_else(|| last_line(&output.stdout))
                .unwrap_or_else(|| {
                    format!(
                        "the launcher ended ({}) without saying what it started",
                        output.status
                    )
                }),
        ),
    }
}

/// Whether the phone may name `path`, for a start or a git read alike: one of this
/// computer's projects. Refused, it gives the error code a request answers with and
/// the reason.
async fn may_name(db: &crate::db::Db, path: &str) -> Result<(), (&'static str, String)> {
    match db.is_project(path.to_string()).await {
        Ok(true) => Ok(()),
        Ok(false) => Err((
            "not_a_project",
            format!("{path} is not one of this computer's projects"),
        )),
        Err(err) => Err((
            "projects_unavailable",
            format!("could not read the project list: {err}"),
        )),
    }
}

/// Where a start runs.
struct Place {
    /// The folder the launcher runs in.
    cwd: String,
    /// The session runs in `cwd`; false when the agent makes its own folder.
    known: bool,
    /// The one flag the agent is handed before the prompt.
    flag: Option<String>,
}

/// The folder for a start in `worktree`, making the worktree first when it is
/// new. Every check reads the repository afresh, and every refusal says why.
async fn place(
    git_bin: Option<&str>,
    path: &str,
    agent: &AgentKind,
    worktree: &StartWorktree,
) -> Result<Place, String> {
    let entries = crate::git::worktrees(path, git_bin, GIT_READ_TIMEOUT)
        .await
        .ok_or_else(|| format!("{path} is not in a git repository"))?;
    let main = entries
        .first()
        .map(|entry| entry.path.clone())
        .ok_or_else(|| format!("git listed no worktrees for {path}"))?;
    let made = |cwd: PathBuf| Place {
        cwd: cwd.display().to_string(),
        known: true,
        flag: None,
    };
    match worktree {
        StartWorktree::Existing { path: wanted } => {
            match entries.iter().find(|entry| &entry.path == wanted) {
                Some(entry) if entry.prunable => {
                    Err(format!("the worktree {wanted} no longer exists"))
                }
                Some(_) => Ok(made(PathBuf::from(wanted))),
                None => Err(format!("{wanted} is not a worktree of this repository")),
            }
        }
        StartWorktree::Branch { branch } => {
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.branch.as_deref() == Some(branch.as_str()))
            {
                return Err(format!("{branch} is already checked out in {}", entry.path));
            }
            let branches = crate::git::local_branches(path, git_bin, GIT_READ_TIMEOUT).await;
            if !branches.is_some_and(|branches| branches.contains(branch)) {
                return Err(format!("{branch} is not a local branch of this repository"));
            }
            let folder = worktree_folder(&main, branch);
            add_worktree(
                path,
                &[OsStr::new("--"), folder.as_os_str(), OsStr::new(branch)],
            )
            .await?;
            Ok(made(folder))
        }
        StartWorktree::New { name } => {
            check_worktree_name(name)?;
            if matches!(agent, AgentKind::Claude) {
                return Ok(Place {
                    cwd: path.to_string(),
                    known: false,
                    flag: Some(format!("--worktree={name}")),
                });
            }
            // The branch is the name as typed. Claude's own `worktree-` prefix marks a
            // branch Claude deletes on a clean exit, commits and all, should a later
            // `claude --worktree <name>` reopen this one.
            let folder = worktree_folder(&main, name);
            add_worktree(
                path,
                &[
                    OsStr::new("-b"),
                    OsStr::new(name),
                    OsStr::new("--"),
                    folder.as_os_str(),
                    OsStr::new("HEAD"),
                ],
            )
            .await?;
            Ok(made(folder))
        }
    }
}

/// Where a worktree named `name` goes: Claude Code's own layout, with each `/`
/// written `+` as Claude writes it.
fn worktree_folder(main: &str, name: &str) -> PathBuf {
    Path::new(main)
        .join(".claude")
        .join("worktrees")
        .join(name.replace('/', "+"))
}

/// Claude Code's rule for a `--worktree` name (2.1.317), held to for both agents.
fn check_worktree_name(name: &str) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("\"{name}\" is not a valid worktree name: {why}"));
    if name.len() > MAX_WORKTREE_NAME_BYTES {
        return refuse("it is longer than 64 characters");
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return refuse("each \"/\"-separated part must be non-empty and not \".\" or \"..\"");
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return refuse("use only letters, digits, dots, underscores and dashes");
        }
    }
    Ok(())
}

/// `git worktree add <args>` in `path`, through the user's login shell as the
/// launcher runs: a checkout runs the repository's filters and hooks (Git LFS
/// among them), and they need the PATH a terminal has. Git's own last line is
/// the reason when it refuses.
async fn add_worktree(path: &str, args: &[&OsStr]) -> Result<(), String> {
    let mut argv: Vec<&OsStr> = vec![
        OsStr::new("git"),
        OsStr::new("-C"),
        OsStr::new(path),
        OsStr::new("worktree"),
        OsStr::new("add"),
    ];
    argv.extend_from_slice(args);
    let mut command = login_shell_command(&argv);
    command
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(START_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(last_line(&output.stderr)
            .unwrap_or_else(|| format!("git worktree add ended ({})", output.status))),
        Ok(Err(err)) => Err(format!("could not run the login shell: {err}")),
        Err(_) => Err(format!(
            "git worktree add did not finish within {} s",
            START_TIMEOUT.as_secs()
        )),
    }
}

/// The answer to `get_project_git`.
async fn project_git(daemon: &Arc<Daemon>, path: String) -> ServerMessage {
    if let Err((code, message)) = may_name(&daemon.db, &path).await {
        return ServerMessage::Error {
            code: code.into(),
            message,
        };
    }
    let git_bin = daemon.config.git_bin.as_deref();
    let Some(entries) = crate::git::worktrees(&path, git_bin, GIT_READ_TIMEOUT).await else {
        return ServerMessage::ProjectGit { path, git: None };
    };
    let mut worktrees = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        worktrees.push(Worktree {
            main: index == 0,
            branch: entry.branch.clone(),
            dirty: !entry.prunable
                && crate::git::dirty(&entry.path, git_bin, GIT_READ_TIMEOUT).await,
            missing: entry.prunable,
            path: entry.path.clone(),
        });
    }
    let mut branches: Vec<String> = crate::git::local_branches(&path, git_bin, GIT_READ_TIMEOUT)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|branch| {
            !entries
                .iter()
                .any(|entry| entry.branch.as_deref() == Some(branch.as_str()))
        })
        .collect();
    let branches_truncated = branches.len() > protocol::ws::MAX_PROJECT_BRANCHES;
    branches.truncate(protocol::ws::MAX_PROJECT_BRANCHES);
    ServerMessage::ProjectGit {
        path,
        git: Some(ProjectGit {
            worktrees,
            branches,
            branches_truncated,
        }),
    }
}

/// The last line of `bytes` with anything on it: where a failing program says why,
/// without the `Error: ` the launcher prints before it. The user's shell startup
/// speaks first, so it is never the last line.
fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.strip_prefix("Error: ").unwrap_or(line).to_string())
}

/// The answer to `list_projects`.
async fn list_projects(daemon: &Arc<Daemon>) -> ServerMessage {
    let rows = match daemon.db.projects().await {
        Ok(rows) => rows,
        Err(err) => {
            crate::log_error!("could not list projects: {err:#}");
            return ServerMessage::Error {
                code: "projects_unavailable".into(),
                message: format!("could not read the project list: {err}"),
            };
        }
    };
    let git_bin = daemon.config.git_bin.as_deref();
    // A few at a time: every git child holds pipes, and two hundred at once
    // would spend the daemon's descriptors.
    let projects = futures_util::stream::iter(rows)
        .map(|row| async move {
            let exists = Path::new(&row.path).is_dir();
            let git_branch = if exists {
                crate::git::branch(&row.path, git_bin, BRANCH_TIMEOUT).await
            } else {
                None
            };
            Project {
                name: crate::project_label::project_label(&row.path),
                path: row.path,
                last_used: row.last_used,
                last_agent: row.last_agent,
                git_branch,
                exists,
            }
        })
        .buffered(BRANCH_READS_AT_ONCE)
        .collect::<Vec<_>>();
    let (projects, agents) = tokio::join!(projects, agents(daemon));
    ServerMessage::Projects { projects, agents }
}

/// Whether `codeconnect <agent> --version` succeeds from the user's login shell:
/// the launcher resolves the agent exactly as a start would.
async fn agent_availability(agent: &AgentKind) -> AgentAvailability {
    let unavailable = |detail: String| AgentAvailability {
        agent: agent.clone(),
        available: false,
        detail: Some(detail),
    };
    let launcher = match crate::state::codex_launcher() {
        Ok(launcher) => launcher,
        Err(why) => return unavailable(why),
    };
    let home = protocol::home_dir();
    let mut command = login_shell_command(&[
        launcher.as_os_str(),
        agent.as_str().as_ref(),
        "--version".as_ref(),
    ]);
    command
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return unavailable(format!("could not run the login shell: {err}")),
        Err(_) => {
            return unavailable(format!(
                "the login shell did not answer within {} s",
                PROBE_TIMEOUT.as_secs()
            ))
        }
    };
    if output.status.success() {
        return AgentAvailability {
            agent: agent.clone(),
            available: true,
            detail: None,
        };
    }
    unavailable(last_line(&output.stderr).unwrap_or_else(|| "the agent could not be found".into()))
}

/// `args` run through the user's login shell, as one argv each, never one string.
///
/// zsh and bash run interactive and login (`-lic`), which is what reads the files
/// a new terminal reads. Any other shell, or none in `$SHELL`, runs `/bin/sh -lc`:
/// a written miss, so a fish user gets the system shell's environment.
fn login_shell_command(args: &[&OsStr]) -> tokio::process::Command {
    let shell = std::env::var_os("SHELL").unwrap_or_default();
    let name = Path::new(&shell).file_name().unwrap_or_default();
    let (program, flags) = if name == "zsh" || name == "bash" {
        (PathBuf::from(&shell), "-lic")
    } else {
        (PathBuf::from("/bin/sh"), "-lc")
    };
    let mut command = tokio::process::Command::new(program);
    command
        .arg(flags)
        .arg("exec \"$@\"")
        .arg("codeconnect-start")
        .args(args);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A folder for one test: its store, a listed project folder, and a stub
    /// launcher whose `--no-attach` runs `body`.
    struct Rig {
        root: PathBuf,
        db: crate::db::Db,
        project: PathBuf,
        launcher: PathBuf,
    }

    impl Rig {
        fn new(tag: &str, body: &str) -> Rig {
            let root = std::env::temp_dir().join(format!(
                "ccd-start-{tag}-{}-{}",
                std::process::id(),
                protocol::time::now_unix_ms()
            ));
            let project = root.join("my app");
            std::fs::create_dir_all(&project).unwrap();
            let store = Arc::new(crate::store::Store::open(&root.join("events.db")).unwrap());
            let at = protocol::time::now_rfc3339();
            let filed = store
                .upsert_session(&crate::store::SessionRow {
                    session_uid: protocol::uid::new().unwrap(),
                    session_id: "cc-1".into(),
                    tmux_session: "cc-1".into(),
                    tmux_socket: "codeconnect".into(),
                    cwd: project.display().to_string(),
                    claude_session_id: None,
                    transcript_path: None,
                    lifecycle: protocol::event::Lifecycle::Exited,
                    created_at: at.clone(),
                    updated_at: at,
                    agent: AgentKind::Claude,
                    codex_thread_id: None,
                    codex_socket: None,
                })
                .unwrap();
            assert!(matches!(filed, crate::store::SessionUpsert::Present));
            let launcher = root.join("codeconnect");
            std::fs::write(&launcher, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::process::Command::new("chmod")
                .arg("755")
                .arg(&launcher)
                .status()
                .unwrap();
            Rig {
                db: crate::db::Db::new(store),
                root,
                project,
                launcher,
            }
        }

        async fn start(&self, path: &str, prompt: Option<&str>) -> StartSessionResult {
            launch(
                &self.db,
                None,
                Ok(self.launcher.clone()),
                &self.root,
                path,
                &AgentKind::Claude,
                prompt,
                None,
            )
            .await
        }

        async fn start_in(
            &self,
            path: &str,
            agent: AgentKind,
            worktree: StartWorktree,
        ) -> StartSessionResult {
            launch(
                &self.db,
                None,
                Ok(self.launcher.clone()),
                &self.root,
                path,
                &agent,
                None,
                Some(&worktree),
            )
            .await
        }

        /// List `cwd` as a project, as a session that ran there would.
        async fn list(&self, cwd: &str) {
            let at = protocol::time::now_rfc3339();
            let filed = self
                .db
                .upsert_session(crate::store::SessionRow {
                    session_uid: protocol::uid::new().unwrap(),
                    session_id: "cc-2".into(),
                    tmux_session: "cc-2".into(),
                    tmux_socket: "codeconnect".into(),
                    cwd: cwd.into(),
                    claude_session_id: None,
                    transcript_path: None,
                    lifecycle: protocol::event::Lifecycle::Exited,
                    created_at: at.clone(),
                    updated_at: at,
                    agent: AgentKind::Codex,
                    codex_thread_id: None,
                    codex_socket: None,
                })
                .await
                .unwrap();
            assert!(matches!(filed, crate::store::SessionUpsert::Present));
        }

        /// The worktree on `branch`, as git lists it.
        async fn worktree_on(&self, branch: &str) -> crate::git::WorktreeEntry {
            crate::git::worktrees(&self.project(), None, GIT_READ_TIMEOUT)
                .await
                .unwrap()
                .into_iter()
                .find(|entry| entry.branch.as_deref() == Some(branch))
                .unwrap()
        }

        /// The project as a repository: `main` at commit A, a linked worktree on
        /// `wt/existing` one commit ahead at B, a worktree whose folder is gone, and
        /// branches whose names a shell would read as code.
        fn make_repository(&self) {
            git(&self.project, &["init", "-q", "-b", "main"]);
            git(&self.project, &["commit", "-q", "--allow-empty", "-m", "A"]);
            for branch in BRANCHES {
                git(&self.project, &["branch", branch]);
            }
            let existing = self.existing().display().to_string();
            git(
                &self.project,
                &["worktree", "add", "-q", "-b", "wt/existing", &existing],
            );
            git(
                &self.existing(),
                &["commit", "-q", "--allow-empty", "-m", "B"],
            );
            let gone = self.root.join("gone");
            let gone_path = gone.display().to_string();
            git(
                &self.project,
                &["worktree", "add", "-q", "-b", "wt/gone", &gone_path],
            );
            std::fs::remove_dir_all(&gone).unwrap();
        }

        fn existing(&self) -> PathBuf {
            self.root.join("wt existing")
        }

        /// What `git` itself calls this repository's main folder.
        fn main(&self) -> String {
            git(
                &self.project,
                &["rev-parse", "--path-format=absolute", "--show-toplevel"],
            )
        }

        /// Everything about the main folder a start must leave alone.
        fn main_state(&self) -> String {
            [
                git(&self.project, &["rev-parse", "HEAD"]),
                git(&self.project, &["symbolic-ref", "HEAD"]),
                git(&self.project, &["status", "--porcelain=v2", "--branch"])
                    .lines()
                    .filter(|line| *line != "? .claude/")
                    .collect::<Vec<_>>()
                    .join("\n"),
            ]
            .join("\n")
        }

        fn launcher_runs(&self) -> usize {
            std::fs::read_to_string(self.root.join("codeconnect.runs"))
                .map(|runs| runs.lines().count())
                .unwrap_or(0)
        }

        fn launcher_cwd(&self) -> PathBuf {
            let cwd = std::fs::read_to_string(self.root.join("codeconnect.cwd")).unwrap();
            PathBuf::from(cwd.trim())
        }

        fn launcher_argv(&self) -> Vec<String> {
            let argv = std::fs::read(self.root.join("codeconnect.argv")).unwrap();
            argv.split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
                .collect()
        }

        fn project(&self) -> String {
            self.project.display().to_string()
        }
    }

    /// Branches a start must hand git as they are.
    const BRANCHES: [&str; 3] = [
        "unicode-ünï-☕-分支",
        "sub$(touch${IFS}PWNED)",
        "feature/search",
    ];

    /// `git -C dir <args>` as a person would run it, its output trimmed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const STARTED: &str = r#"printf '{"uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_name":"cc-7","agent":"claude"}\n' > "$CODECONNECT_LAUNCH_RESULT""#;

    /// **The prompt reaches the launcher as one literal argument, in the folder
    /// asked for**, and nothing in it runs.
    #[tokio::test]
    async fn the_launcher_gets_the_folder_and_a_literal_prompt() {
        let rig = Rig::new(
            "argv",
            &format!("pwd -P > \"$0.cwd\"; printf '%s\\0' \"$@\" > \"$0.argv\"; {STARTED}"),
        );
        let marker = rig.root.join("ran");
        let prompt = format!(
            "-p \"it's\" $(touch {m}) `touch {m}`; echo hi;\nline two;",
            m = marker.display()
        );
        let result = rig.start(&rig.project(), Some(&prompt)).await;
        assert_eq!(
            result,
            StartSessionResult::Started {
                session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
                cwd: Some(rig.project()),
            }
        );
        let argv = std::fs::read(rig.root.join("codeconnect.argv")).unwrap();
        let argv: Vec<&[u8]> = argv.split(|byte| *byte == 0).collect();
        assert_eq!(
            argv,
            [
                b"claude".as_slice(),
                b"--no-attach",
                b"--",
                prompt.as_bytes(),
                b""
            ]
        );
        let cwd = std::fs::read_to_string(rig.root.join("codeconnect.cwd")).unwrap();
        assert_eq!(Path::new(cwd.trim()), rig.project.canonicalize().unwrap());
        assert!(!marker.exists(), "nothing in the prompt may run");
        let left: Vec<_> = std::fs::read_dir(&rig.root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("launch-"))
            .collect();
        assert!(left.is_empty(), "the result file is removed once read");
    }

    /// **The reason a launcher gives reads the same wherever it is shown**: its
    /// last line, after the shell's noise, without the `Error: ` it prints first.
    #[test]
    fn a_launcher_reason_is_its_last_line_without_the_error_prefix() {
        let stderr =
            b"bash: cannot set terminal process group (1): Inappropriate ioctl for device\n\
            bash: no job control in this shell\n\
            Error: could not find the claude binary: install Claude Code\n\n";
        assert_eq!(
            last_line(stderr).as_deref(),
            Some("could not find the claude binary: install Claude Code")
        );
        assert_eq!(last_line(b"  \n\n"), None);
    }

    /// **A double tap starts one session**: the second ask waits for the first
    /// and gets its answer.
    #[tokio::test]
    async fn the_same_request_id_starts_one_session() {
        let rig = Rig::new(
            "once",
            &format!("echo run >> \"$0.runs\"; sleep 1; {STARTED}"),
        );
        let starts = Starts::default();
        let path = rig.project();
        let (first, second) = tokio::join!(
            starts.once("r1", rig.start(&path, None)),
            starts.once("r1", rig.start(&path, None)),
        );
        let again = starts.once("r1", rig.start(&path, None)).await;
        assert_eq!(first, second);
        assert_eq!(first, again);
        assert!(matches!(first, StartSessionResult::Started { .. }));
        let runs = std::fs::read_to_string(rig.root.join("codeconnect.runs")).unwrap();
        assert_eq!(runs.lines().count(), 1, "{runs}");
    }

    /// **Each failure says which one it is**, and none of them runs a launcher
    /// that was not needed.
    #[tokio::test]
    async fn a_failed_start_says_why() {
        let reason = |result: StartSessionResult| match result {
            StartSessionResult::Failed { reason, detail } => (reason, detail),
            other => panic!("expected a failure, got {other:?}"),
        };

        let rig = Rig::new("unlisted", STARTED);
        let elsewhere = rig.root.display().to_string();
        let (why, detail) = reason(rig.start(&elsewhere, None).await);
        assert_eq!(why, StartFailure::LaunchFailed);
        assert!(
            detail.contains("not one of this computer's projects"),
            "{detail}"
        );

        std::fs::remove_dir(&rig.project).unwrap();
        let (why, _) = reason(rig.start(&rig.project(), None).await);
        assert_eq!(why, StartFailure::FolderMissing);

        let rig = Rig::new(
            "missing",
            r#"printf '{"error":"agent_missing","detail":"could not find the claude binary"}\n' > "$CODECONNECT_LAUNCH_RESULT"; exit 1"#,
        );
        assert_eq!(
            reason(rig.start(&rig.project(), None).await),
            (
                StartFailure::AgentMissing,
                "could not find the claude binary".into()
            )
        );

        let rig = Rig::new(
            "broken",
            "echo 'rc noise' >&2; echo 'Error: tmux refused' >&2; exit 1",
        );
        assert_eq!(
            reason(rig.start(&rig.project(), None).await),
            (StartFailure::LaunchFailed, "tmux refused".into())
        );

        let rig = Rig::new("long", STARTED);
        let long = "x".repeat(protocol::ws::MAX_COMPOSE_BYTES + 1);
        assert_eq!(
            reason(rig.start(&rig.project(), Some(&long)).await).0,
            StartFailure::LaunchFailed
        );
    }

    /// The stub launcher, keeping what it was run with, then starting.
    fn recording(tag: &str) -> Rig {
        let rig = Rig::new(
            tag,
            &format!(
                "echo run >> \"$0.runs\"; pwd -P > \"$0.cwd\"; printf '%s\\0' \"$@\" > \"$0.argv\"; {STARTED}"
            ),
        );
        rig.make_repository();
        rig
    }

    fn started_in(result: &StartSessionResult) -> Option<String> {
        match result {
            StartSessionResult::Started { cwd, .. } => cwd.clone(),
            other => panic!("expected a start, got {other:?}"),
        }
    }

    fn refusal(result: StartSessionResult) -> String {
        match result {
            StartSessionResult::Failed {
                reason: StartFailure::LaunchFailed,
                detail,
            } => detail,
            other => panic!("expected launch_failed, got {other:?}"),
        }
    }

    fn canonical(path: &str) -> PathBuf {
        Path::new(path).canonicalize().unwrap()
    }

    /// **An existing worktree is started in as it is**: the agent runs there, the
    /// answer names it, and git is not written to.
    #[tokio::test]
    async fn an_existing_worktree_is_started_in_as_it_is() {
        let rig = recording("existing");
        let existing = rig.worktree_on("wt/existing").await.path;
        let listed = git(&rig.project, &["worktree", "list", "--porcelain"]);
        let before = rig.main_state();

        let result = rig
            .start_in(
                &rig.project(),
                AgentKind::Codex,
                StartWorktree::Existing {
                    path: existing.clone(),
                },
            )
            .await;
        assert_eq!(started_in(&result).as_deref(), Some(existing.as_str()));
        assert_eq!(rig.launcher_cwd(), canonical(&existing));
        assert_eq!(
            git(&rig.project, &["worktree", "list", "--porcelain"]),
            listed
        );
        assert_eq!(rig.main_state(), before);

        let gone = rig.worktree_on("wt/gone").await.path;
        let elsewhere = rig.root.display().to_string();
        for (path, says) in [
            (gone, "no longer exists"),
            (elsewhere, "is not a worktree of this repository"),
        ] {
            let detail = refusal(
                rig.start_in(
                    &rig.project(),
                    AgentKind::Claude,
                    StartWorktree::Existing { path },
                )
                .await,
            );
            assert!(detail.contains(says), "{detail}");
        }
        assert_eq!(rig.launcher_runs(), 1, "a refused start runs nothing");
    }

    /// **A branch gets a worktree of its own, on that branch, in Claude Code's
    /// layout**, its name reaching git as it is, and the main folder is untouched.
    #[tokio::test]
    async fn a_branch_gets_its_own_worktree_and_the_main_folder_stays_as_it_was() {
        let rig = recording("branch");
        let main = rig.main();
        let before = rig.main_state();

        for branch in BRANCHES {
            let result = rig
                .start_in(
                    &rig.project(),
                    AgentKind::Claude,
                    StartWorktree::Branch {
                        branch: branch.into(),
                    },
                )
                .await;
            let folder = Path::new(&main)
                .join(".claude/worktrees")
                .join(branch.replace('/', "+"));
            assert_eq!(started_in(&result), Some(folder.display().to_string()));
            assert_eq!(git(&folder, &["symbolic-ref", "--short", "HEAD"]), branch);
            assert_eq!(rig.launcher_cwd(), folder.canonicalize().unwrap());
        }
        let pwned = std::process::Command::new("find")
            .arg(&rig.root)
            .args(["-name", "PWNED"])
            .output()
            .unwrap();
        assert!(pwned.stdout.is_empty(), "nothing in a branch name may run");
        assert_eq!(rig.main_state(), before);

        for (branch, says) in [
            ("wt/existing", "already checked out"),
            ("wt/gone", "already checked out"),
            ("main", "already checked out"),
            ("no-such-branch", "is not a local branch"),
        ] {
            let detail = refusal(
                rig.start_in(
                    &rig.project(),
                    AgentKind::Codex,
                    StartWorktree::Branch {
                        branch: branch.into(),
                    },
                )
                .await,
            );
            assert!(detail.contains(says), "{branch}: {detail}");
        }
        assert_eq!(rig.launcher_runs(), BRANCHES.len());
    }

    /// **A new Codex worktree is made where Claude Code makes one, on a branch named
    /// as typed**, from the commit of the folder it was asked in: here a linked
    /// worktree one commit ahead of main. A second one by the same name is git's to
    /// refuse.
    #[tokio::test]
    async fn a_new_codex_worktree_starts_from_the_folder_it_is_asked_in() {
        let rig = recording("new-codex");
        let existing = rig.worktree_on("wt/existing").await.path;
        rig.list(&existing).await;
        let main = rig.main();
        let ahead = git(Path::new(&existing), &["rev-parse", "HEAD"]);
        assert_ne!(ahead, git(&rig.project, &["rev-parse", "main"]));

        let new = || StartWorktree::New {
            name: "fix/login".into(),
        };
        let result = rig.start_in(&existing, AgentKind::Codex, new()).await;
        let folder = Path::new(&main).join(".claude/worktrees/fix+login");
        assert_eq!(started_in(&result), Some(folder.display().to_string()));
        assert_eq!(
            git(&folder, &["symbolic-ref", "--short", "HEAD"]),
            "fix/login"
        );
        assert_eq!(git(&folder, &["rev-parse", "HEAD"]), ahead);
        assert_eq!(rig.launcher_cwd(), folder.canonicalize().unwrap());

        let detail = refusal(rig.start_in(&existing, AgentKind::Codex, new()).await);
        assert!(detail.contains("already exists"), "{detail}");
        assert_eq!(rig.launcher_runs(), 1);
    }

    /// **A new Claude worktree is Claude's own to make**: the launcher runs in the
    /// folder with `--worktree=<name>` as one argument, and the daemon makes nothing.
    #[tokio::test]
    async fn a_new_claude_worktree_is_left_to_claude() {
        let rig = recording("new-claude");
        let before = rig.main_state();
        let result = rig
            .start_in(
                &rig.project(),
                AgentKind::Claude,
                StartWorktree::New {
                    name: "-fix/login".into(),
                },
            )
            .await;
        assert_eq!(started_in(&result), None, "Claude chooses the folder");
        assert_eq!(
            rig.launcher_argv(),
            ["claude", "--no-attach", "--worktree=-fix/login"]
        );
        assert_eq!(rig.launcher_cwd(), rig.project.canonicalize().unwrap());
        assert!(!Path::new(&rig.main()).join(".claude").exists());
        assert_eq!(rig.main_state(), before);
    }

    /// **A name Claude Code would refuse is refused before anything runs**, for
    /// both agents.
    #[tokio::test]
    async fn a_worktree_name_claude_would_refuse_is_refused_before_anything_runs() {
        let rig = recording("names");
        let branches = git(&rig.project, &["branch", "--list"]);
        let too_long = "x".repeat(MAX_WORKTREE_NAME_BYTES + 1);
        for name in [
            "semi;",
            "has space",
            "ünï",
            "..",
            "a//b",
            "a/./b",
            "",
            "trailing/",
            too_long.as_str(),
        ] {
            for agent in [AgentKind::Claude, AgentKind::Codex] {
                let detail = refusal(
                    rig.start_in(
                        &rig.project(),
                        agent,
                        StartWorktree::New { name: name.into() },
                    )
                    .await,
                );
                assert!(
                    detail.contains("is not a valid worktree name"),
                    "{name:?}: {detail}"
                );
            }
        }
        assert!(check_worktree_name(&"x".repeat(MAX_WORKTREE_NAME_BYTES)).is_ok());
        assert_eq!(rig.launcher_runs(), 0);
        assert!(!Path::new(&rig.main()).join(".claude").exists());
        assert_eq!(git(&rig.project, &["branch", "--list"]), branches);
    }

    /// **Outside a repository there is no worktree to pick**, and it says so.
    #[tokio::test]
    async fn a_folder_outside_a_repository_has_no_worktrees() {
        let rig = Rig::new("plain", STARTED);
        let detail = refusal(
            rig.start_in(
                &rig.project(),
                AgentKind::Claude,
                StartWorktree::Branch {
                    branch: "main".into(),
                },
            )
            .await,
        );
        assert!(detail.contains("is not in a git repository"), "{detail}");
    }
}
