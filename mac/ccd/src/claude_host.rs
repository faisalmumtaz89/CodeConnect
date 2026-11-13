//! A Claude run's agent host, from ccd's side (P0 §2.1 H1-H3).
//!
//! ccd writes the run's row first, with the app's start request id, so a repeated
//! request finds the run instead of starting another. Then it makes the run's folder
//! `<root>/runs/<uid>/` (0700) and spawns `cc-agent-host <run dir>` through the login
//! shell, in a session of its own so it outlives ccd. The host writes `host.json`; ccd
//! admits the pid in it only if it is the child ccd spawned, and records it with the
//! birth time the kernel gives it. ccd then attaches over `host.sock`, commits the
//! host's first `state` before acknowledging it, and sends `start`. The connection
//! stays with a link task that commits and acknowledges each message in sequence
//! order and stops at the first one it does not apply, which the host keeps, so
//! nothing is acknowledged that was not stored. The start's outcome, committed by the
//! link, makes the run `live` or `failed`, and the app's answer waits for it.
//!
//! The lines are [`protocol::host`]'s; the host is mac/agent-host.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::config::Config;
use protocol::event::Lifecycle;
use protocol::host::{
    FromHost, HostFile, McpCommand, OutcomeStatus, StartOptions, ToHost, Trust,
    HOST_PROTOCOL_VERSION,
};
use protocol::proc_identity::{read_birth_identity, ProcessIdentity};
use protocol::ws::{StartFailure, StartSessionResult};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::state::Daemon;
use crate::store::{HostedRun, SessionRow};

/// The agent host's file name, beside ccd's own executable.
const HOST_BIN: &str = "cc-agent-host";

/// How often the start looks for `host.json` and the socket.
const POLL: Duration = Duration::from_millis(50);

/// One Claude start, as the app asked for it and the checks before the row resolved it.
pub(crate) struct Run<'a> {
    /// This daemon's state folder, which holds `runs/`.
    pub root: &'a Path,
    pub request_id: &'a str,
    /// The folder the run starts in.
    pub cwd: &'a str,
    pub prompt: &'a str,
    /// The app's model. Claude Code's own `default` is sent as none, so the run starts
    /// with the model its settings name.
    pub model: Option<&'a str>,
    /// A new worktree for claude to make and start in, when the app asked for one.
    pub worktree: Option<&'a str>,
    /// The claude the start's check found.
    pub claude_path: &'a str,
    /// The agent host to spawn.
    pub host_bin: &'a Path,
    /// The `codeconnect` binary, which runs the run's mail server.
    pub codeconnect: &'a Path,
    /// The start's environment, the one its claude check read: the host and claude
    /// get it as it is, with only `CODECONNECT_SESSION_UID` added.
    pub environment: &'a HashMap<OsString, OsString>,
}

/// The run's folder: `<root>/runs/<uid>/`.
pub(crate) fn run_dir(root: &Path, uid: &str) -> PathBuf {
    root.join("runs").join(uid)
}

/// The agent host ccd spawns: `agent_host_bin` when the config names one, else the one
/// beside ccd's own executable, where the Mac app installs it.
pub(crate) fn host_binary(config: &Config) -> Result<PathBuf, String> {
    if let Some(bin) = &config.agent_host_bin {
        return Ok(PathBuf::from(bin));
    }
    std::env::current_exe()
        .map(|exe| exe.with_file_name(HOST_BIN))
        .map_err(|err| format!("could not find this daemon's own executable: {err}"))
}

/// The answer for a start whose request id already has a run (H1): that run.
pub(crate) fn answer_for(run: HostedRun) -> StartSessionResult {
    match run.lifecycle {
        Lifecycle::Failed => failed("did not start".into()),
        _ => StartSessionResult::Started {
            session_uid: run.session_uid,
            cwd: Some(run.cwd),
        },
    }
}

/// What became of a run's `start`, as its link committed it: `Ok` once the start is
/// done, or why it did not start (the host's refusal, or the link ending first).
type Begun = Result<(), String>;

/// Start `run`: its row, its folder, its host, the first attach and `start`, then the
/// answer from the start's outcome.
pub(crate) async fn start(daemon: &Arc<Daemon>, run: Run<'_>) -> StartSessionResult {
    start_within(daemon, run, crate::start_session::START_TIMEOUT).await
}

/// [`start`], with `timeout` for all of it, from the row to the outcome: the tests'
/// way to a deadline shorter than START_TIMEOUT.
async fn start_within(daemon: &Arc<Daemon>, run: Run<'_>, timeout: Duration) -> StartSessionResult {
    // The run and its first message are named together, before the row: the start's
    // uuid is the one claude stamps on that message.
    let (uid, start_uuid) =
        match protocol::uid::new().and_then(|uid| Ok((uid, protocol::uid::uuid_v4()?))) {
            Ok(names) => names,
            Err(err) => return failed(format!("could not name the run: {err}")),
        };
    let now = protocol::time::now_rfc3339();
    let row = SessionRow {
        session_uid: uid.clone(),
        session_id: uid.clone(),
        tmux_session: String::new(),
        tmux_socket: String::new(),
        cwd: run.cwd.to_string(),
        claude_session_id: None,
        transcript_path: None,
        lifecycle: Lifecycle::Starting,
        created_at: now.clone(),
        updated_at: now,
        agent: AgentKind::Claude,
        codex_thread_id: None,
        codex_socket: None,
    };
    match daemon.db.begin_run(row, run.request_id.to_string()).await {
        Ok(Some(existing)) => return answer_for(existing),
        Ok(None) => {}
        Err(err) => return failed(format!("could not record the run: {err:#}")),
    }
    let deadline = Instant::now() + timeout;
    let begun = match bring_up(daemon, &uid, &start_uuid, &run, deadline).await {
        Ok(begun) => begun,
        // Before `start` no claude can exist (the host exits by itself at 30 s), so the
        // run has failed.
        Err(detail) => {
            if let Err(err) = daemon
                .db
                .set_lifecycle(uid.clone(), Lifecycle::Failed)
                .await
            {
                crate::log_error!("could not mark {uid} failed: {err:#}");
            }
            return failed(detail);
        }
    };
    // From `start` on, claude may be running, so only the outcome the link commits ends
    // the run: a link that ends first leaves it `starting`, protected from deletion.
    let started = StartSessionResult::Started {
        session_uid: uid,
        // A start in a new worktree does not know its folder until claude reports it.
        cwd: run.worktree.is_none().then(|| run.cwd.to_string()),
    };
    match tokio::time::timeout_at(deadline, begun).await {
        Ok(Ok(Ok(()))) => started,
        Ok(Ok(Err(detail))) => failed(detail),
        Ok(Err(_)) => failed("the agent host's link ended".into()),
        // No outcome yet. The run exists and is starting, and its link keeps applying
        // what comes, so a late outcome still makes it `live` or `failed`.
        Err(_) => started,
    }
}

fn failed(detail: String) -> StartSessionResult {
    StartSessionResult::Failed {
        reason: StartFailure::LaunchFailed,
        detail,
    }
}

/// The run's folder, its host spawned and admitted, attached, and `start` sent, by
/// `deadline`: then what the link will commit of the start's outcome.
async fn bring_up(
    daemon: &Arc<Daemon>,
    uid: &str,
    start_uuid: &str,
    run: &Run<'_>,
    deadline: Instant,
) -> Result<oneshot::Receiver<Begun>, String> {
    let dir = run_dir(run.root, uid);
    let log = make_run_dir(run.root, &dir)
        .map_err(|err| format!("could not make the run's folder {}: {err}", dir.display()))?;
    let mut child = crate::start_session::spawn_detached(
        run.environment,
        &[run.host_bin.as_os_str(), dir.as_os_str()],
        Path::new(run.cwd),
        uid,
        log,
    )
    .map_err(|err| format!("could not run the login shell: {err}"))?;
    let spawned = child.id();
    let attached = attach(
        daemon, uid, start_uuid, run, &dir, spawned, &mut child, deadline,
    )
    .await;
    crate::start_session::reap(child);
    attached
}

/// `runs/` and the run's own folder, both 0700, the folder made here and only here,
/// and its `host.log` (0600), which the host's output goes to.
fn make_run_dir(root: &Path, dir: &Path) -> io::Result<std::fs::File> {
    protocol::fsperm::private_dir(&root.join("runs"))?;
    std::fs::DirBuilder::new()
        .mode(protocol::fsperm::DIR_MODE)
        .create(dir)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(protocol::fsperm::FILE_MODE)
        .open(dir.join("host.log"))
}

#[allow(clippy::too_many_arguments)]
async fn attach(
    daemon: &Arc<Daemon>,
    uid: &str,
    start_uuid: &str,
    run: &Run<'_>,
    dir: &Path,
    spawned: Option<u32>,
    child: &mut tokio::process::Child,
    deadline: Instant,
) -> Result<oneshot::Receiver<Begun>, String> {
    let record = loop {
        if let Some(record) = read_host_file(&dir.join("host.json")) {
            break record;
        }
        give_up_if_ended(
            dir,
            child,
            deadline,
            "the agent host did not write host.json",
        )?;
        tokio::time::sleep(POLL).await;
    };
    if record.protocol_version != HOST_PROTOCOL_VERSION {
        return Err(format!(
            "the agent host speaks version {} of its protocol, and this CodeConnect speaks {}",
            record.protocol_version, HOST_PROTOCOL_VERSION
        ));
    }
    if u32::try_from(record.host_pid).ok() != spawned {
        return Err("host.json names a process this daemon did not start".into());
    }
    let host = ProcessIdentity {
        pid: record.host_pid,
        birth: read_birth_identity(record.host_pid)
            .ok_or("could not read the agent host's start time")?,
    };
    daemon
        .db
        .record_host(uid.to_string(), host)
        .await
        .map_err(|err| format!("could not record the agent host: {err:#}"))?;

    let socket = dir.join("host.sock");
    let stream = loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => break stream,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                give_up_if_ended(dir, child, deadline, "the agent host did not listen")?;
                tokio::time::sleep(POLL).await;
            }
            Err(err) => return Err(format!("could not reach the agent host: {err}")),
        }
    };
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    send(&mut write, &ToHost::Attach { after: 0 }).await?;
    // A new host keeps nothing yet, so the first line it sends is its state.
    let first = tokio::time::timeout_at(deadline, lines.next_line())
        .await
        .map_err(|_| "the agent host sent nothing after the attach".to_string())?
        .map_err(|err| format!("could not read the agent host: {err}"))?
        .ok_or("the agent host closed the connection")?;
    match serde_json::from_str::<FromHost>(&first) {
        Ok(FromHost::State { seq, .. }) => commit_and_ack(daemon, uid, host, seq, &mut write)
            .await
            .map_err(|why| format!("could not commit the agent host's state: {why}"))?,
        _ => return Err("the agent host's first message was not its state".into()),
    }
    let start = ToHost::Start {
        uuid: start_uuid.to_string(),
        options: start_options(uid, run),
        prompt: run.prompt.to_string(),
    };
    send(&mut write, &start).await?;
    daemon.host_link_attached(uid).await;
    // The link is a task of its own, so the start's answer timing out never ends it.
    let (begun, outcome) = oneshot::channel();
    tokio::spawn(link(
        Arc::clone(daemon),
        uid.to_string(),
        host,
        start_uuid.to_string(),
        begun,
        lines,
        write,
    ));
    Ok(outcome)
}

/// The run's `query()` options.
fn start_options(uid: &str, run: &Run<'_>) -> StartOptions {
    StartOptions {
        cwd: run.cwd.to_string(),
        claude_path: run.claude_path.to_string(),
        // The app chooses no mode at a start, so claude picks its own: the person's
        // `defaultMode`, else its default.
        permission_mode: None,
        // Not trusted, whatever the folder's answer: that answer decides only whether
        // the start goes on (start_session's folder question).
        trust: Trust::NotTrusted,
        resume: None,
        model: run
            .model
            .filter(|model| *model != "default")
            .map(str::to_string),
        worktree: run.worktree.map(str::to_string),
        mail: McpCommand {
            command: run.codeconnect.to_string_lossy().into_owned(),
            args: protocol::ipc::mail_server_args(uid, &protocol::socket_path()),
        },
    }
}

/// `host.json` once it is whole: the host creates it before it writes it.
fn read_host_file(path: &Path) -> Option<HostFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// An error once the host has ended (with what it last said) or the start's time is up.
fn give_up_if_ended(
    dir: &Path,
    child: &mut tokio::process::Child,
    deadline: Instant,
    waiting: &str,
) -> Result<(), String> {
    if let Ok(Some(status)) = child.try_wait() {
        let said = std::fs::read(dir.join("host.log"))
            .ok()
            .and_then(|log| crate::start_session::last_line(&log));
        return Err(said.unwrap_or_else(|| format!("the agent host ended ({status})")));
    }
    if Instant::now() >= deadline {
        return Err(format!(
            "{waiting} within {} s",
            crate::start_session::START_TIMEOUT.as_secs()
        ));
    }
    Ok(())
}

async fn send(write: &mut OwnedWriteHalf, message: &ToHost) -> Result<(), String> {
    let mut line = serde_json::to_string(message).map_err(|err| err.to_string())?;
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .map_err(|err| format!("could not write to the agent host: {err}"))
}

/// Commit `seq` as the run's cursor, then acknowledge it: never the other way round,
/// so the host drops only what is stored (H3).
async fn commit_and_ack(
    daemon: &Daemon,
    uid: &str,
    host: ProcessIdentity,
    seq: u64,
    write: &mut OwnedWriteHalf,
) -> Result<(), String> {
    match daemon.db.commit_cursor(uid.to_string(), host, seq).await {
        Ok(true) => send(write, &ToHost::Ack { seq }).await,
        Ok(false) => Err(format!(
            "message {seq} is not past the run's cursor for this host"
        )),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// The run's connection after `start`: each message committed, then acknowledged, in
/// order. It applies the run's `state` and the outcome of `start_uuid`, which makes the
/// run `live` or `failed` and is told to `begun` once committed; `begun` hears instead
/// why the link ended if it ends first. It ends at the first message this daemon does
/// not apply, which stays in the host with everything after it, or when the connection
/// or a commit fails.
async fn link(
    daemon: Arc<Daemon>,
    uid: String,
    host: ProcessIdentity,
    start_uuid: String,
    begun: oneshot::Sender<Begun>,
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
) {
    let mut begun = Some(begun);
    let ended = loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break "the agent host closed the connection".to_string(),
            Err(err) => break format!("could not read the agent host: {err}"),
        };
        match serde_json::from_str::<FromHost>(&line) {
            Ok(FromHost::State { seq, .. }) => {
                if let Err(why) = commit_and_ack(&daemon, &uid, host, seq, &mut write).await {
                    break why;
                }
            }
            Ok(FromHost::Outcome {
                seq,
                uuid,
                status,
                detail,
                initialize,
                ..
            }) if uuid == start_uuid => {
                let (lifecycle, said) = match status {
                    OutcomeStatus::Done => (Lifecycle::Live, Ok(())),
                    OutcomeStatus::Refused => (
                        Lifecycle::Failed,
                        Err(detail.unwrap_or_else(|| "the agent host refused the start".into())),
                    ),
                };
                let initialize = match initialize.as_ref().map(serde_json::to_string).transpose() {
                    Ok(initialize) => initialize,
                    Err(err) => {
                        break format!("could not keep the start's initialize answer: {err}")
                    }
                };
                match daemon
                    .db
                    .commit_outcome(uid.clone(), host, seq, lifecycle, initialize)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        break format!("message {seq} is not past the run's cursor for this host")
                    }
                    Err(err) => break format!("{err:#}"),
                }
                if let Err(detail) = &said {
                    crate::log_info!("{uid}: the agent host refused the start: {detail}");
                }
                // Told once committed, before the ack: a failed ack keeps what is stored.
                if let Some(begun) = begun.take() {
                    let _ = begun.send(said);
                }
                if let Err(why) = send(&mut write, &ToHost::Ack { seq }).await {
                    break why;
                }
            }
            Ok(_) => break "a message this daemon does not apply yet; the host keeps it".into(),
            Err(err) => break format!("a line that is not a host message: {err}"),
        }
    };
    crate::log_info!("{uid}: the agent host link ended: {ended}");
    if let Some(begun) = begun.take() {
        let _ = begun.send(Err(ended));
    }
    daemon.host_link_detached(&uid).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Endpoint;
    use crate::store::Store;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    /// A state folder of its own, short and under `/tmp` so `runs/<uid>/host.sock` fits
    /// `sun_path`, with a daemon on a store in it. Removed when dropped.
    struct Rig {
        root: PathBuf,
        daemon: Arc<Daemon>,
        /// What a start gives its host: a home of the rig's own and the system shell.
        environment: HashMap<OsString, OsString>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rig() -> Rig {
        let uid = protocol::uid::new().unwrap();
        let root = PathBuf::from(format!("/tmp/cch2.{}", &uid[16..]));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        let daemon = daemon_on(&root);
        let environment = HashMap::from([
            (OsString::from("HOME"), root.join("home").into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        Rig {
            root,
            daemon,
            environment,
        }
    }

    /// A daemon on the store in `root`, as a restarted ccd opens it again.
    fn daemon_on(root: &Path) -> Arc<Daemon> {
        let store = Store::open(&root.join("events.db")).unwrap();
        let (transcript_tx, transcript_rx) = tokio::sync::mpsc::unbounded_channel();
        Box::leak(Box::new(transcript_rx));
        Daemon::new(
            Config::default(),
            Arc::new(store),
            Arc::new(crate::apns::LoggingPushSender::new()),
            Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            transcript_tx,
        )
    }

    /// A stand-in host: it writes `host.json` with `version` and `pid` (its own when
    /// `None`) into the run's folder, then waits, so the test can play the host's side
    /// of the socket.
    fn stub_host(rig: &Rig, version: u32, pid: Option<i32>) -> PathBuf {
        let path = rig.root.join("stub-host");
        let pid = pid.map_or("$$".to_string(), |pid| pid.to_string());
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho $$ > \"$1/stub.pid\"\nprintf '{{\"protocol_version\":{version},\"host_pid\":%s}}' \
                 \"{pid}\" > \"$1/host.json\"\nexec sleep 20\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run<'a>(rig: &'a Rig, request_id: &'a str, cwd: &'a str, host_bin: &'a Path) -> Run<'a> {
        Run {
            root: &rig.root,
            request_id,
            cwd,
            prompt: "fix the tests",
            model: Some("default"),
            worktree: None,
            claude_path: "/nonexistent/claude",
            host_bin,
            codeconnect: Path::new("/nonexistent/codeconnect"),
            environment: &rig.environment,
        }
    }

    fn runs(rig: &Rig) -> Vec<PathBuf> {
        match std::fs::read_dir(rig.root.join("runs")) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The test's side of the run's socket: it waits for the start to write host.json,
    /// listens where the host would, and takes ccd's connection.
    async fn play_host(dir: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf, HostFile) {
        let record = loop {
            if let Some(record) = read_host_file(&dir.join("host.json")) {
                break record;
            }
            tokio::time::sleep(POLL).await;
        };
        let listener = UnixListener::bind(dir.join("host.sock")).unwrap();
        let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("ccd connected")
            .unwrap();
        let (read, write) = stream.into_split();
        (BufReader::new(read).lines(), write, record)
    }

    async fn next(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Option<ToHost> {
        tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .ok()?
            .ok()?
            .map(|line| serde_json::from_str(&line).unwrap())
    }

    async fn say(write: &mut OwnedWriteHalf, message: serde_json::Value) {
        write
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    /// Stop the stand-in host in `dir` by the pid it recorded for itself.
    fn stop(dir: &Path) {
        let pid: i32 = std::fs::read_to_string(dir.join("stub.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: a plain kill(2) of the stand-in host this test started, by its pid.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }

    fn cursor(rig: &Rig, uid: &str) -> i64 {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT cursor FROM run_hosts WHERE session_uid = ?1",
                [uid],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// The run's folder once the start has spawned its stand-in host, which has written
    /// host.json there.
    async fn spawned_dir(rig: &Rig) -> PathBuf {
        loop {
            if let Some(dir) = runs(rig)
                .into_iter()
                .find(|dir| dir.join("host.json").exists())
            {
                return dir;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The host's side of a start, up to and including `start`: ccd's attach, the first
    /// state and its ack, then the `start` line.
    struct Played {
        dir: PathBuf,
        uid: String,
        lines: Lines<BufReader<OwnedReadHalf>>,
        write: OwnedWriteHalf,
        record: HostFile,
        start: ToHost,
    }

    impl Played {
        fn start_uuid(&self) -> String {
            match &self.start {
                ToHost::Start { uuid, .. } => uuid.clone(),
                other => panic!("not a start: {other:?}"),
            }
        }
    }

    async fn play_to_start(rig: &Rig) -> Played {
        let dir = spawned_dir(rig).await;
        let uid = dir.file_name().unwrap().to_str().unwrap().to_string();
        let (mut lines, mut write, record) = play_host(&dir).await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 0 }));
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 1, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, Some(ToHost::Ack { seq: 1 }));
        let start = next(&mut lines).await.expect("ccd sent start");
        Played {
            dir,
            uid,
            lines,
            write,
            record,
            start,
        }
    }

    /// An initialize answer as the host sends it in a done outcome.
    fn initialize_answer() -> serde_json::Value {
        serde_json::json!({
            "commands": [{"name": "compact"}],
            "models": [{"value": "opus"}],
            "account": {"api_provider": "firstParty"}
        })
    }

    /// What the store holds for run `uid`: its lifecycle and the initialize answer kept.
    fn kept(rig: &Rig, uid: &str) -> (String, Option<serde_json::Value>) {
        rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT s.lifecycle, h.initialize
                   FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE session_uid = ?1",
                [uid],
                |row| {
                    let initialize: Option<String> = row.get(1)?;
                    Ok((
                        row.get(0)?,
                        initialize.map(|json| serde_json::from_str(&json).unwrap()),
                    ))
                },
            )
            .unwrap()
    }

    fn started_uid(result: &StartSessionResult) -> String {
        match result {
            StartSessionResult::Started { session_uid, .. } => session_uid.clone(),
            other => panic!("not started: {other:?}"),
        }
    }

    /// H1: the row is written before the host is spawned, so a ccd that died while its
    /// host starts finds the run when it starts again (H8) instead of a host nobody
    /// knows about. A write lock held from a second connection makes the row's insert
    /// wait (the store's busy timeout): while it is held, the host must not start.
    #[tokio::test]
    async fn the_row_is_there_while_the_host_starts() {
        let rig = rig();
        // A host that says it is up, then ends without writing host.json.
        let host = rig.root.join("slow-host");
        std::fs::write(&host, "#!/bin/sh\ntouch \"$1/started\"\nexec sleep 1\n").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = rig.root.to_str().unwrap().to_string();
        let lock = rusqlite::Connection::open(rig.root.join("events.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = || runs(&rig).iter().any(|dir| dir.join("started").exists());
        let (result, (started_under_lock, seen)) =
            tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let started_under_lock = started();
                lock.execute_batch("ROLLBACK").unwrap();
                while !started() {
                    tokio::time::sleep(POLL).await;
                }
                let seen = rig
                    .daemon
                    .db
                    .hosted_run_for_request("req-1".into())
                    .await
                    .unwrap()
                    .map(|run| run.lifecycle);
                (started_under_lock, seen)
            });
        assert!(
            !started_under_lock,
            "the host started before its row could be written"
        );
        assert_eq!(seen, Some(Lifecycle::Starting));
        assert!(
            matches!(result, StartSessionResult::Failed { .. }),
            "{result:?}"
        );
    }

    /// A start whose spawn cannot even begin (its folder is gone) leaves its row, failed,
    /// with its request id, and its folder private: 0700, with a 0600 log.
    #[tokio::test]
    async fn a_start_whose_spawn_fails_leaves_its_row_failed_and_its_folder_private() {
        let rig = rig();
        let missing = rig.root.join("gone");
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let result = start(
            &rig.daemon,
            run(&rig, "req-1", missing.to_str().unwrap(), &host),
        )
        .await;
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("login shell")),
            "{result:?}"
        );
        let found = rig
            .daemon
            .db
            .hosted_run_for_request("req-1".into())
            .await
            .unwrap()
            .expect("the row is there");
        assert_eq!(found.lifecycle, Lifecycle::Failed);
        let dirs = runs(&rig);
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        assert_eq!(dirs[0].file_name().unwrap(), found.session_uid.as_str());
        assert_eq!(mode(&rig.root.join("runs")), 0o700);
        assert_eq!(mode(&dirs[0]), 0o700);
        assert_eq!(mode(&dirs[0].join("host.log")), 0o600);
    }

    /// H1: a repeated request id, after the first finished and after a restart, gets
    /// the first run and starts nothing; so does one that overlaps the first.
    #[tokio::test]
    async fn a_repeated_start_gets_the_first_run_and_starts_nothing() {
        let rig = rig();
        let missing = rig.root.join("gone");
        let missing = missing.to_str().unwrap();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let first = start(&rig.daemon, run(&rig, "req-1", missing, &host)).await;
        let again = start(&rig.daemon, run(&rig, "req-1", missing, &host)).await;
        let restarted = daemon_on(&rig.root);
        let after_restart = start(&restarted, run(&rig, "req-1", missing, &host)).await;
        let did_not_start = failed("did not start".into());
        assert!(
            matches!(first, StartSessionResult::Failed { .. }),
            "{first:?}"
        );
        assert_eq!(again, did_not_start);
        assert_eq!(after_restart, did_not_start);
        assert_eq!(runs(&rig).len(), 1);

        // Overlapping: the second asks while the first is still bringing its host up, and
        // gets the first start's own answer when it comes, here a refusal, rather than
        // "started" for a run that is about to fail.
        let cwd = rig.root.to_str().unwrap();
        let refused = stub_host(&rig, 999, None);
        let (one, two) = tokio::join!(
            rig.daemon.starts.once(
                "req-2",
                start(&rig.daemon, run(&rig, "req-2", cwd, &refused))
            ),
            rig.daemon.starts.once(
                "req-2",
                start(&rig.daemon, run(&rig, "req-2", cwd, &refused))
            ),
        );
        assert!(
            matches!(&one, StartSessionResult::Failed { detail, .. } if detail.contains("version 999")),
            "{one:?}"
        );
        assert_eq!(
            one, two,
            "the repeat waited for the first start's own answer"
        );
        let dirs = runs(&rig);
        assert_eq!(dirs.len(), 2);
        for dir in dirs.iter().filter(|dir| dir.join("stub.pid").exists()) {
            stop(dir);
        }
    }

    /// The handshake against a stand-in host: the pid ccd spawned, its kernel birth,
    /// the first state committed before its ack, `start` with the run's options, its
    /// done outcome committed (the run `live`, its initialize answer kept) before its
    /// ack and before the answer, and the run's link attached until the connection ends.
    #[tokio::test]
    async fn a_new_host_is_admitted_attached_and_started() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let daemon = Arc::clone(&rig.daemon);
        let (result, played) = tokio::join!(
            start(
                &daemon,
                Run {
                    worktree: Some("feature-x"),
                    ..run(&rig, "req-1", &cwd, &host)
                }
            ),
            async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                       "status": "done", "initialize": initialize_answer()}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                // Acknowledged only once stored: the run is live, its answer kept.
                assert_eq!(
                    kept(&rig, &played.uid),
                    ("live".to_string(), Some(initialize_answer()))
                );
                played
            }
        );
        let Played {
            dir,
            uid,
            mut lines,
            mut write,
            record,
            start,
        } = played;
        assert_eq!(started_uid(&result), uid);
        // A start in a new worktree does not know its folder until claude reports it.
        assert!(
            matches!(&result, StartSessionResult::Started { cwd: None, .. }),
            "{result:?}"
        );
        assert_eq!(cursor(&rig, &uid), 2);
        match start {
            ToHost::Start {
                uuid,
                options,
                prompt,
            } => {
                assert_eq!(uuid.len(), 36);
                assert_eq!(prompt, "fix the tests");
                assert_eq!(options.cwd, cwd);
                assert_eq!(options.claude_path, "/nonexistent/claude");
                assert_eq!(options.permission_mode, None, "the app chose no mode");
                assert_eq!(options.worktree.as_deref(), Some("feature-x"));
                assert_eq!(options.trust, Trust::NotTrusted);
                assert_eq!(options.model, None, "Claude Code's own default is no model");
                assert_eq!(
                    options.mail.args[..3],
                    ["mcp", "--session-uid", uid.as_str()]
                );
            }
            other => panic!("no start: {other:?}"),
        }
        // The host is a session leader of its own, so it outlives this ccd.
        // SAFETY: getsid(2) reads one process's session id.
        assert_eq!(unsafe { libc::getsid(record.host_pid) }, record.host_pid);
        let link = || async {
            daemon
                .sessions()
                .await
                .unwrap()
                .into_iter()
                .find(|session| session.session_uid == uid)
                .map(|session| session.link)
        };
        assert_eq!(link().await, Some(protocol::event::Link::Attached));

        // The host sends something this daemon does not apply yet, then a state: neither
        // is acknowledged and the cursor stays, so the host keeps both.
        say(
            &mut write,
            serde_json::json!({"type": "sdk", "seq": 3, "message": {}}),
        )
        .await;
        say(
            &mut write,
            serde_json::json!({"type": "state", "seq": 4, "pending": []}),
        )
        .await;
        assert_eq!(next(&mut lines).await, None);
        assert_eq!(cursor(&rig, &uid), 2);
        let detached = async {
            while link().await != Some(protocol::event::Link::Detached) {
                tokio::time::sleep(POLL).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), detached)
            .await
            .expect("the link reads detached once its connection ends");
        stop(&dir);
    }

    /// A start the host refuses: its outcome makes the run `failed`, and the app is
    /// answered with the host's reason.
    #[tokio::test]
    async fn a_refused_start_fails_with_the_hosts_reason() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (result, played) =
            tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                let mut played = play_to_start(&rig).await;
                let uuid = played.start_uuid();
                say(
                    &mut played.write,
                    serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                       "status": "refused", "detail": "claude would not start"}),
                )
                .await;
                assert_eq!(next(&mut played.lines).await, Some(ToHost::Ack { seq: 2 }));
                played
            });
        assert_eq!(result, failed("claude would not start".into()));
        assert_eq!(kept(&rig, &played.uid), ("failed".to_string(), None));
        stop(&played.dir);
    }

    /// Once `start` is sent, claude may be running, so a run whose outcome is not
    /// committed stays `starting`, and a delete is refused: the app is answered with the
    /// reason, but the run is not marked failed. Three ways: the outcome's commit changes
    /// nothing (the run's recorded host is another), the outcome is for another command,
    /// and the connection ends first. None is acknowledged.
    #[tokio::test]
    async fn a_start_with_no_committed_outcome_leaves_its_run_starting() {
        for (case, why) in [
            ("another host", "not past the run's cursor"),
            ("another command", "does not apply"),
            ("closed", "closed the connection"),
        ] {
            let rig = rig();
            let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (result, (dir, uid)) =
                tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                    let Played {
                        dir,
                        uid,
                        mut lines,
                        mut write,
                        start,
                        ..
                    } = play_to_start(&rig).await;
                    let uuid = match &start {
                        ToHost::Start { uuid, .. } => uuid.clone(),
                        other => panic!("{other:?}"),
                    };
                    let outcome = |uuid: &str| {
                        serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                           "status": "done", "initialize": initialize_answer()})
                    };
                    match case {
                        "another host" => {
                            rusqlite::Connection::open(rig.root.join("events.db"))
                                .unwrap()
                                .execute(
                                    "UPDATE run_hosts SET host_start_usec = host_start_usec + 1
                                      WHERE session_uid = ?1",
                                    [&uid],
                                )
                                .unwrap();
                            say(&mut write, outcome(&uuid)).await;
                        }
                        "another command" => say(&mut write, outcome("not-the-start")).await,
                        _ => drop(write),
                    }
                    assert_eq!(next(&mut lines).await, None, "{case}: acknowledged");
                    (dir, uid)
                });
            assert!(
                matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains(why)),
                "{case}: {result:?}"
            );
            assert_eq!(kept(&rig, &uid), ("starting".to_string(), None), "{case}");
            assert_eq!(cursor(&rig, &uid), 1, "{case}");
            let deleted = rig.daemon.db.delete_exited_session(uid).await.unwrap();
            assert!(
                matches!(deleted, crate::store::DeleteOutcome::NotExited { .. }),
                "{case}: {deleted:?}"
            );
            stop(&dir);
        }
    }

    /// 97's condition on the start's deadline: the app is answered `Started` once it
    /// passes with no outcome, and the run's link keeps applying what comes, so a late
    /// outcome still makes the run `live` (with its answer kept) or `failed`.
    #[tokio::test]
    async fn a_late_outcome_still_ends_the_run() {
        for (status, lifecycle) in [("done", "live"), ("refused", "failed")] {
            let rig = rig();
            let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (result, played) = tokio::join!(
                start_within(
                    &rig.daemon,
                    run(&rig, "req-1", &cwd, &host),
                    Duration::from_secs(3)
                ),
                async {
                    let mut played = play_to_start(&rig).await;
                    let uuid = played.start_uuid();
                    // Past the start's deadline: the answer has gone.
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    assert_eq!(kept(&rig, &played.uid).0, "starting");
                    say(
                        &mut played.write,
                        serde_json::json!({"type": "outcome", "seq": 2, "uuid": uuid,
                                           "status": status, "detail": "late",
                                           "initialize": (status == "done").then(initialize_answer)}),
                    )
                    .await;
                    assert_eq!(
                        next(&mut played.lines).await,
                        Some(ToHost::Ack { seq: 2 }),
                        "{status}"
                    );
                    played
                }
            );
            assert_eq!(started_uid(&result), played.uid, "{status}");
            assert_eq!(kept(&rig, &played.uid).0, lifecycle, "{status}");
            stop(&played.dir);
        }
    }

    /// The host gets the run's environment as it was given, with only the run's uid
    /// added: a sign-in variable arrives unchanged, and nothing sets a Claude config
    /// folder. (That the claude check read this same map is the arm's test,
    /// `start_session::tests::the_check_and_the_host_see_one_environment`.)
    #[tokio::test]
    async fn the_host_gets_its_runs_environment_as_given() {
        let mut rig = rig();
        rig.environment.insert(
            OsString::from("ANTHROPIC_API_KEY"),
            OsString::from("sk-test-marker"),
        );
        let host = rig.root.join("env-host");
        std::fs::write(&host, "#!/bin/sh\nenv > \"$1/env.txt\"\n").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = rig.root.to_str().unwrap().to_string();
        let result = start(&rig.daemon, run(&rig, "req-1", &cwd, &host)).await;
        assert!(
            matches!(result, StartSessionResult::Failed { .. }),
            "{result:?}"
        );
        let dir = runs(&rig).pop().unwrap();
        let uid = dir.file_name().unwrap().to_str().unwrap().to_string();
        let seen = std::fs::read_to_string(dir.join("env.txt")).unwrap();
        let lines: Vec<&str> = seen.lines().collect();
        assert!(
            lines.contains(&"ANTHROPIC_API_KEY=sk-test-marker"),
            "{seen}"
        );
        assert!(
            lines.contains(&format!("CODECONNECT_SESSION_UID={uid}").as_str()),
            "{seen}"
        );
        assert!(!seen.contains("CLAUDE_CONFIG_DIR"), "{seen}");
        let home = format!("HOME={}", rig.root.join("home").display());
        assert!(lines.contains(&home.as_str()), "{seen}");
    }

    /// A state whose commit changes nothing (the run's recorded host is another) is
    /// not acknowledged.
    #[tokio::test]
    async fn a_message_whose_commit_changes_nothing_is_not_acknowledged() {
        let rig = rig();
        let host = stub_host(&rig, HOST_PROTOCOL_VERSION, None);
        let cwd = rig.root.to_str().unwrap().to_string();
        let (result, ()) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-1", &cwd, &host)),
            async {
                let dir = loop {
                    if let Some(dir) = runs(&rig)
                        .into_iter()
                        .find(|dir| dir.join("host.json").exists())
                    {
                        break dir;
                    }
                    tokio::time::sleep(POLL).await;
                };
                let (mut lines, mut write, _) = play_host(&dir).await;
                assert_eq!(next(&mut lines).await, Some(ToHost::Attach { after: 0 }));
                let uid = dir.file_name().unwrap().to_str().unwrap().to_string();
                rusqlite::Connection::open(rig.root.join("events.db"))
                .unwrap()
                .execute(
                    "UPDATE run_hosts SET host_start_usec = host_start_usec + 1 WHERE session_uid = ?1",
                    [&uid],
                )
                .unwrap();
                say(
                    &mut write,
                    serde_json::json!({"type": "state", "seq": 1, "pending": []}),
                )
                .await;
                assert_eq!(
                    next(&mut lines).await,
                    None,
                    "nothing after an uncommitted state"
                );
                stop(&dir);
            }
        );
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("commit")),
            "{result:?}"
        );
    }

    /// A host.json naming a process ccd did not spawn, or a protocol version it does
    /// not speak, is refused before ccd attaches: nothing is sent and the run fails.
    #[tokio::test]
    async fn a_host_file_ccd_cannot_admit_gets_no_start() {
        for (version, pid, why) in [
            (
                HOST_PROTOCOL_VERSION,
                Some(std::process::id() as i32),
                "did not start",
            ),
            (999, None, "version 999"),
        ] {
            let rig = rig();
            let host = stub_host(&rig, version, pid);
            let cwd = rig.root.to_str().unwrap().to_string();
            let (result, connected) =
                tokio::join!(start(&rig.daemon, run(&rig, "req-1", &cwd, &host)), async {
                    let dir = loop {
                        if let Some(dir) = runs(&rig)
                            .into_iter()
                            .find(|dir| dir.join("host.json").exists())
                        {
                            break dir;
                        }
                        tokio::time::sleep(POLL).await;
                    };
                    tokio::time::sleep(POLL).await;
                    let listener = UnixListener::bind(dir.join("host.sock")).unwrap();
                    let connected = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                        .await
                        .is_ok();
                    stop(&dir);
                    connected
                });
            assert!(
                matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains(why)),
                "{result:?}"
            );
            assert!(!connected, "ccd attached to a host it should have refused");
        }
    }

    /// The agent host's one stand-in claude (mac/agent-host/test/stub-claude.ts), as its
    /// own tests write it (rig.ts `writeStub`): a sh wrapper running it on the built
    /// host as Bun. It answers initialize with a recorded claude's answer and, after the
    /// first message, writes the recorded session's `system/init`.
    fn stand_in_claude(rig: &Rig, host: &Path) -> (PathBuf, serde_json::Value) {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let init =
            std::fs::read_to_string(repo.join("fixtures/claude/initialize-api-key-2.1.319.jsonl"))
                .unwrap();
        let init = init.lines().next().unwrap().to_string();
        let session =
            std::fs::read_to_string(repo.join("fixtures/claude/sdk-session-0.3.322.jsonl"))
                .unwrap();
        let session_init = session
            .lines()
            .find(|line| {
                line.contains(r#""type":"system""#) && line.contains(r#""subtype":"init""#)
            })
            .unwrap();
        let script = rig.root.join("stub.json");
        std::fs::write(
            &script,
            serde_json::json!({"init": init, "after_user": [session_init],
                               "log": rig.root.join("stub.log")})
            .to_string(),
        )
        .unwrap();
        let claude = rig.root.join("claude");
        std::fs::write(
            &claude,
            format!(
                "#!/bin/sh\nBUN_BE_BUN=1 exec '{}' '{}' '{}' \"$@\"\n",
                host.display(),
                repo.join("mac/agent-host/test/stub-claude.ts").display(),
                script.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let answer: serde_json::Value = serde_json::from_str(&init).unwrap();
        (claude, answer["response"]["response"]["models"].clone())
    }

    /// Against the real agent host (mac/agent-host/build.sh's `cc-agent-host`), through
    /// zsh's login shell (`-lic`), as a zsh user's start runs it, with the host's
    /// stand-in claude: the recorded pid and birth are the host's own, it leads a session
    /// of its own, the start's outcome makes the run `live` with the initialize answer's
    /// models kept, and the host is still running after its 30 s no-start exit would
    /// have ended it (claude's init is unacknowledged, so the host keeps it). A start
    /// whose claude is missing is refused, naming it, and its run is `failed`. Ignored
    /// and gated: `CODECONNECT_TEST_AGENT_HOST` names the built host.
    #[tokio::test]
    #[ignore = "needs the built agent host: CODECONNECT_TEST_AGENT_HOST=<path>"]
    async fn the_real_host_is_admitted_attached_and_kept_by_start() {
        let host = PathBuf::from(
            std::env::var("CODECONNECT_TEST_AGENT_HOST")
                .expect("CODECONNECT_TEST_AGENT_HOST names the built agent host"),
        );
        let mut rig = rig();
        rig.environment
            .insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let cwd = rig.root.to_str().unwrap().to_string();
        let (claude, models) = stand_in_claude(&rig, &host);
        let missing = start(&rig.daemon, run(&rig, "req-0", &cwd, &host)).await;
        assert!(
            matches!(&missing, StartSessionResult::Failed { detail, .. } if detail.contains("/nonexistent/claude")),
            "{missing:?}"
        );
        let refused = rig
            .daemon
            .db
            .hosted_run_for_request("req-0".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refused.lifecycle, Lifecycle::Failed);
        let result = start(
            &rig.daemon,
            Run {
                claude_path: claude.to_str().unwrap(),
                ..run(&rig, "req-1", &cwd, &host)
            },
        )
        .await;
        let uid = started_uid(&result);
        assert!(
            matches!(&result, StartSessionResult::Started { cwd: Some(folder), .. } if *folder == cwd),
            "{result:?}"
        );
        let (lifecycle, initialize) = kept(&rig, &uid);
        assert_eq!(lifecycle, "live");
        assert_eq!(initialize.unwrap()["models"], models);
        let record = read_host_file(&run_dir(&rig.root, &uid).join("host.json")).unwrap();
        let pid = record.host_pid;
        let stored: (i32, i64, i64) = rusqlite::Connection::open(rig.root.join("events.db"))
            .unwrap()
            .query_row(
                "SELECT host_pid, host_start_sec, host_start_usec FROM run_hosts
                  WHERE session_uid = ?1",
                [&uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let birth = read_birth_identity(pid).unwrap();
        assert_eq!(stored, (pid, birth.start_sec, birth.start_usec));
        assert_eq!(
            cursor(&rig, &uid),
            2,
            "the state and the outcome are committed"
        );
        // SAFETY: getsid(2) and kill(2) with signal 0 only read one process's state.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        tokio::time::sleep(Duration::from_secs(31)).await;
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        // SAFETY: kill(2) of the process group this test's host leads (its recorded pid),
        // which holds the host and its stand-in claude.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        assert!(alive, "the host ended within 31 s: start never reached it");
    }
}
