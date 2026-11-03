//! Starting a session from the phone: the folders it can pick from, and the start.
//!
//! Everything the daemon runs on the phone's behalf goes through the user's own
//! login shell ([`login_shell_command`]), so the launcher and the agent it starts
//! see the PATH, version managers and credentials a new terminal would — not the
//! daemon's service environment.

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use protocol::agent::AgentKind;
use protocol::ws::{AgentAvailability, ClientMessage, Project, ServerMessage};

use crate::state::Daemon;

/// How long one folder's git branch may take.
const BRANCH_TIMEOUT: Duration = Duration::from_secs(2);

/// How many folders' branches are read at once.
const BRANCH_READS_AT_ONCE: usize = 8;

/// How long the login shell and the launcher may take to say an agent is there.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The agents the phone can start.
const AGENTS: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

/// The answer to a `list_projects` or `start_session`.
pub async fn answer(daemon: &Daemon, message: ClientMessage) -> ServerMessage {
    match message {
        ClientMessage::ListProjects => list_projects(daemon).await,
        _ => ServerMessage::Error {
            code: "bad_request".into(),
            message: "this daemon does not start sessions yet".into(),
        },
    }
}

/// The answer to `list_projects`.
async fn list_projects(daemon: &Daemon) -> ServerMessage {
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
    let agents = futures_util::future::join_all(AGENTS.iter().map(agent_availability));
    let (projects, agents) = tokio::join!(projects, agents);
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    unavailable(
        stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("the agent could not be found")
            .trim()
            .to_string(),
    )
}

/// `args` run through the user's login shell, as one argv each, never one string.
///
/// zsh and bash run interactive and login (`-lic`), which is what reads the files
/// a new terminal reads. Any other shell, or none in `$SHELL`, runs `/bin/sh -lc`:
/// a written miss, so a fish user gets the system shell's environment.
pub fn login_shell_command(args: &[&std::ffi::OsStr]) -> tokio::process::Command {
    let shell = std::env::var_os("SHELL").unwrap_or_default();
    let name = Path::new(&shell).file_name().unwrap_or_default();
    let (program, flags) = if name == "zsh" || name == "bash" {
        (std::path::PathBuf::from(&shell), "-lic")
    } else {
        (std::path::PathBuf::from("/bin/sh"), "-lc")
    };
    let mut command = tokio::process::Command::new(program);
    command
        .arg(flags)
        .arg("exec \"$@\"")
        .arg("codeconnect-start")
        .args(args);
    command
}
