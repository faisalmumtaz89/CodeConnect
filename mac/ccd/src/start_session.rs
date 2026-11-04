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
    AgentAvailability, ClientMessage, Project, ServerMessage, StartFailure, StartSessionResult,
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
        ClientMessage::StartSession {
            request_id,
            path,
            agent,
            prompt,
        } => {
            let state_dir = protocol::root_dir();
            let launch = launch(
                &daemon.db,
                crate::state::codex_launcher(),
                &state_dir,
                &path,
                &agent,
                prompt.as_deref(),
            );
            ServerMessage::StartSessionResult {
                result: daemon.starts.once(&request_id, launch).await,
                request_id,
            }
        }
        _ => unreachable!("handle_client passes only list_projects and start_session here"),
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

/// Run `codeconnect <agent> --no-attach [-- <prompt>]` in `path` through the
/// user's login shell, and read what it started from its result line, a file in
/// `state_dir`.
pub(crate) async fn launch(
    db: &crate::db::Db,
    launcher: Result<PathBuf, String>,
    state_dir: &Path,
    path: &str,
    agent: &AgentKind,
    prompt: Option<&str>,
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
    match db.is_project(path.to_string()).await {
        Ok(true) => {}
        Ok(false) => {
            return failed(
                StartFailure::LaunchFailed,
                format!("{path} is not one of this computer's projects"),
            )
        }
        Err(err) => {
            return failed(
                StartFailure::LaunchFailed,
                format!("could not read the project list: {err}"),
            )
        }
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
    if let Some(prompt) = prompt {
        args.extend([OsStr::new("--"), OsStr::new(prompt)]);
    }
    let mut command = login_shell_command(&args);
    command
        .current_dir(path)
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
        (Ok(LaunchResult::Started { uid, .. }), _) => {
            StartSessionResult::Started { session_uid: uid }
        }
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
                Ok(self.launcher.clone()),
                &self.root,
                path,
                &AgentKind::Claude,
                prompt,
            )
            .await
        }

        fn project(&self) -> String {
            self.project.display().to_string()
        }
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
                session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into()
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
}
