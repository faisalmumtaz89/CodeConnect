//! A Codex run's app-server, from ccd's side (P3 L1-L3, R1, R5).
//!
//! ccd writes the run's row first, with the app's start request id, so a repeated
//! request finds the run instead of starting another (H1, as for a Claude run). Then it
//! makes the run's folder `<root>/runs/<uid>/` (0700) and launches the codex the start's
//! check found as `codex -c projects=… app-server --listen unix://<run dir>/as.sock`,
//! through the user's login shell, in a session of its own so it outlives ccd. A
//! `/bin/sh` line in front of codex claims the run first: from the run's folder it writes
//! its own pid to `as.pid` with noclobber, so only one process ever claims a run, then
//! becomes codex. The pid ccd spawned is that one process throughout, the login shell,
//! the claim and codex in turn: the run's identity is that pid and the birth time the
//! kernel gives it, recorded at once, while ccd still holds the child. Its process group,
//! whose id is that pid, holds the app-server whether codex runs it itself or as the
//! npm launcher's child. ccd then connects over `as.sock` and initializes.
//!
//! A start that fails once the app-server is launched marks the run `failed` and stops
//! its process group with the ladder ([`crate::claude_host::stop_group`]): an app-server
//! never ends by itself.
//!
//! codex is told each folder from the repository's top down to the run's own, or the
//! run's folder alone outside git, as untrusted (`-c projects=`), so a folder's trust is
//! CodeConnect's answer and not one codex kept from elsewhere.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::agent::AgentKind;
use protocol::event::Lifecycle;
use protocol::proc_identity::{read_birth_identity, BootIdentity, ProcessIdentity};
use protocol::ws::{StartFailure, StartSessionResult};
use tokio::time::Instant;

use crate::codex_client::Client;
use crate::state::Daemon;
use crate::store::SessionRow;

/// How often the start looks for the app-server's socket.
const POLL: Duration = Duration::from_millis(50);

/// The line in front of codex, run by `/bin/sh` as `$0` = its name, `$1` = the run's
/// folder, and codex's own argv after it: claim the run with this shell's pid, which
/// noclobber lets only one writer do, then become codex.
const CLAIM: &str = r#"cd "$1" || exit 1; set -C; echo $$ > as.pid || exit 1; shift; exec "$@""#;

/// One Codex start, as the app asked for it and the checks before the row resolved it.
pub(crate) struct Run<'a> {
    /// This daemon's state folder, which holds `runs/`.
    pub root: &'a Path,
    pub request_id: &'a str,
    /// The folder the run starts in.
    pub cwd: &'a str,
    /// The codex the start's check found.
    pub codex: &'a Path,
    /// The git the trust list's repository read runs.
    pub git_bin: Option<&'a str>,
    /// The start's environment, the one its codex check read: codex gets it as it is,
    /// with only `CODECONNECT_SESSION_UID` added.
    pub environment: &'a HashMap<OsString, OsString>,
}

/// Start `run`: its row, its folder, its app-server, the connection and `initialize`.
/// The connection is then let go: until a thread is started on it, it carries nothing.
pub(crate) async fn start(daemon: &Arc<Daemon>, run: Run<'_>) -> StartSessionResult {
    start_within(daemon, run, crate::start_session::START_TIMEOUT).await
}

/// [`start`], with `timeout` from the launch to `initialize`'s answer: the tests' way to
/// a deadline shorter than START_TIMEOUT.
async fn start_within(daemon: &Arc<Daemon>, run: Run<'_>, timeout: Duration) -> StartSessionResult {
    let projects = match projects(run.cwd, run.git_bin).await {
        Ok(projects) => projects,
        Err(why) => return failed(why),
    };
    let uid = match protocol::uid::new() {
        Ok(uid) => uid,
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
        agent: AgentKind::Codex,
        codex_thread_id: None,
        codex_socket: None,
    };
    match daemon.db.begin_run(row, run.request_id.to_string()).await {
        Ok(Some(existing)) => return crate::claude_host::answer_for(existing),
        Ok(None) => {}
        Err(err) => return failed(format!("could not record the run: {err:#}")),
    }
    let deadline = Instant::now() + timeout;
    let mut launched = None;
    match bring_up(daemon, &uid, &run, &projects, deadline, &mut launched).await {
        Ok(client) => {
            drop(client);
            StartSessionResult::Started {
                session_uid: uid,
                cwd: Some(run.cwd.to_string()),
            }
        }
        Err(detail) => {
            if let Err(err) = daemon
                .db
                .set_lifecycle(uid.clone(), Lifecycle::Failed)
                .await
            {
                crate::log_error!("could not mark {uid} failed: {err:#}");
            }
            if let Some((leader, boot)) = launched {
                stop(&uid, leader, boot).await;
            }
            failed(detail)
        }
    }
}

fn failed(detail: String) -> StartSessionResult {
    StartSessionResult::Failed {
        reason: StartFailure::LaunchFailed,
        detail,
    }
}

/// The ladder on a failed start's process group, under the warrant of its recorded leader.
async fn stop(uid: &str, leader: ProcessIdentity, boot: Option<BootIdentity>) {
    use crate::claude_host::Ladder;
    let Some(boot) = boot else {
        crate::log_info!(
            "{uid}: no boot was recorded with its app-server, so its group was not signalled"
        );
        return;
    };
    match crate::claude_host::stop_group(leader, boot).await {
        Ladder::Done => {}
        Ladder::Refused(why) => {
            crate::log_info!("{uid}: its app-server's group was not signalled: {why}")
        }
        Ladder::Deferred => {
            crate::log_info!("{uid}: its app-server is not yet reaped; the liveness sweep takes it")
        }
    }
}

/// The run's folder, its app-server launched and recorded in `launched`, connected and
/// initialized, by `deadline`.
async fn bring_up(
    daemon: &Arc<Daemon>,
    uid: &str,
    run: &Run<'_>,
    projects: &str,
    deadline: Instant,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<Client, String> {
    let dir = crate::claude_host::run_dir(run.root, uid);
    let log = crate::start_session::make_run_dir(run.root, &dir)
        .map_err(|err| format!("could not make the run's folder {}: {err}", dir.display()))?;
    let socket = dir.join("as.sock");
    let mut listen = OsString::from("unix://");
    listen.push(&socket);
    let projects = format!("projects={projects}");
    let mut child = crate::start_session::spawn_detached(
        run.environment,
        &[
            OsStr::new("/bin/sh"),
            OsStr::new("-c"),
            OsStr::new(CLAIM),
            OsStr::new("codeconnect-codex"),
            dir.as_os_str(),
            run.codex.as_os_str(),
            OsStr::new("-c"),
            OsStr::new(&projects),
            OsStr::new("app-server"),
            OsStr::new("--listen"),
            &listen,
        ],
        Path::new(run.cwd),
        uid,
        log,
    )
    .map_err(|err| format!("could not run the login shell: {err}"))?;
    let attached = attach(daemon, uid, &dir, &socket, &mut child, deadline, launched).await;
    crate::start_session::reap(child);
    attached
}

async fn attach(
    daemon: &Arc<Daemon>,
    uid: &str,
    dir: &Path,
    socket: &Path,
    child: &mut tokio::process::Child,
    deadline: Instant,
    launched: &mut Option<(ProcessIdentity, Option<BootIdentity>)>,
) -> Result<Client, String> {
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("the login shell ended as it started")?;
    let leader = ProcessIdentity {
        pid,
        birth: read_birth_identity(pid).ok_or("could not read the app-server's start time")?,
    };
    let boot = protocol::proc_identity::boot_identity();
    *launched = Some((leader, boot));
    daemon
        .db
        .record_host(uid.to_string(), leader, boot)
        .await
        .map_err(|err| format!("could not record the app-server: {err:#}"))?;

    let client = loop {
        match tokio::time::timeout_at(deadline, crate::codex_client::connect(socket)).await {
            Ok(Ok(client)) => break client,
            Ok(Err(err)) if not_listening(&err) => {
                crate::start_session::give_up_if_ended(
                    dir,
                    child,
                    deadline,
                    "Codex's app-server did not listen",
                )?;
                tokio::time::sleep(POLL).await;
            }
            Ok(Err(err)) => return Err(format!("could not reach Codex's app-server: {err:#}")),
            Err(_) => {
                return Err(format!(
                    "Codex's app-server did not listen within {} s",
                    crate::start_session::START_TIMEOUT.as_secs()
                ))
            }
        }
    };
    match tokio::time::timeout_at(deadline, client.initialize()).await {
        Ok(Ok(_)) => Ok(client),
        Ok(Err(err)) => Err(format!("Codex's app-server did not initialize: {err}")),
        Err(_) => Err(format!(
            "Codex's app-server did not answer initialize within {} s",
            crate::start_session::START_TIMEOUT.as_secs()
        )),
    }
}

/// Whether a dial failed only because nothing listens on the socket yet.
fn not_listening(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>().is_some_and(|err| {
        matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    })
}

/// The folders codex is told the trust of, as the TOML inline table `-c projects=` takes:
/// by their real paths, every folder from the repository's top down to `cwd`, or `cwd`
/// alone outside git, each untrusted. Each path is a TOML basic string, written as JSON
/// writes a string.
async fn projects(cwd: &str, git_bin: Option<&str>) -> Result<String, String> {
    let folder = std::fs::canonicalize(cwd).map_err(|err| format!("{cwd}: {err}"))?;
    let top = crate::git::toplevel(cwd, git_bin, crate::start_session::GIT_READ_TIMEOUT)
        .await
        .and_then(|top| std::fs::canonicalize(top).ok());
    let mut folders: Vec<PathBuf> = match &top {
        Some(top) if folder.starts_with(top) => folder
            .ancestors()
            .take_while(|at| at.starts_with(top))
            .map(Path::to_path_buf)
            .collect(),
        _ => vec![folder],
    };
    folders.reverse();
    let entries: Vec<String> = folders
        .iter()
        .map(|folder| {
            let key =
                serde_json::to_string(&folder.to_string_lossy()).expect("a string is always JSON");
            format!(r#"{key} = {{ trust_level = "untrusted" }}"#)
        })
        .collect();
    Ok(format!("{{ {} }}", entries.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Endpoint;
    use crate::store::Store;
    use futures_util::{SinkExt, StreamExt};
    use protocol::config::Config;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::protocol::Message;

    /// A state folder of its own, short and under `/tmp` so `runs/<uid>/as.sock` fits
    /// `sun_path`, with a daemon on a store in it. Removed when dropped.
    struct Rig {
        root: PathBuf,
        daemon: Arc<Daemon>,
        /// The folder the rig's runs start in.
        project: String,
        /// What a start gives codex: a home of the rig's own and the system shell.
        environment: HashMap<OsString, OsString>,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rig() -> Rig {
        let uid = protocol::uid::new().unwrap();
        let root = PathBuf::from(format!("/tmp/ccx2.{}", &uid[16..]));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let environment = HashMap::from([
            (OsString::from("HOME"), root.join("home").into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        Rig {
            daemon: daemon_on(&root),
            project: root.join("project").display().to_string(),
            root,
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

    /// A stand-in codex running `script`, launched as codex is: in the run's folder, after
    /// the claim, with codex's arguments. It keeps its arguments in the run's `argv` and
    /// counts its launches in `<root>/launches`.
    fn stand_in(rig: &Rig, script: &str) -> PathBuf {
        let path = rig.root.join("stand-in-codex");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > argv\necho launch >> '{}'\n{script}\n",
                rig.root.join("launches").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run<'a>(rig: &'a Rig, request_id: &'a str, codex: &'a Path) -> Run<'a> {
        Run {
            root: &rig.root,
            request_id,
            cwd: &rig.project,
            codex,
            git_bin: None,
            environment: &rig.environment,
        }
    }

    fn launches(rig: &Rig) -> usize {
        std::fs::read_to_string(rig.root.join("launches"))
            .map(|launches| launches.lines().count())
            .unwrap_or(0)
    }

    /// The run's folder, once the start has made one.
    async fn run_folder(rig: &Rig) -> PathBuf {
        loop {
            if let Some(dir) = std::fs::read_dir(rig.root.join("runs"))
                .ok()
                .and_then(|mut entries| entries.next())
            {
                return dir.unwrap().path();
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The test's side of `as.sock`: it waits for the stand-in to be launched, listens
    /// where the app-server would, takes ccd's connection, and answers `initialize` with
    /// `answer`. It gives back the request it answered and the frame after it.
    async fn play_app_server(
        rig: &Rig,
        answer: serde_json::Value,
    ) -> (serde_json::Value, serde_json::Value) {
        let dir = run_folder(rig).await;
        while launches(rig) == 0 {
            tokio::time::sleep(POLL).await;
        }
        let listener = UnixListener::bind(dir.join("as.sock")).unwrap();
        let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("ccd connected")
            .unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let asked = next_frame(&mut ws).await;
        let mut reply = answer;
        reply["id"] = asked["id"].clone();
        ws.send(Message::Text(reply.to_string())).await.unwrap();
        (asked, next_frame(&mut ws).await)
    }

    async fn next_frame(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    ) -> serde_json::Value {
        loop {
            if let Message::Text(text) = ws.next().await.unwrap().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Whether the process group `pgid` has a member.
    fn group_alive(pgid: i32) -> bool {
        protocol::proc_identity::group_has_members(pgid)
    }

    /// The ladder on the run's recorded group, so a test leaves no process behind.
    async fn end_run(rig: &Rig, uid: &str) {
        let watch = rig
            .daemon
            .db
            .watched_host(uid.to_string())
            .await
            .unwrap()
            .unwrap();
        crate::claude_host::stop_group(watch.host, watch.boot.unwrap()).await;
    }

    /// **A Codex start writes its row, launches codex in a session of its own after the
    /// claim, records the launch's leader, connects and initializes**, then answers
    /// `started`. The leader is the spawned pid, which the claim wrote and which leads
    /// the group and the session. A repeat of the request, on this daemon or on a
    /// restarted one, answers the same run and launches nothing.
    #[tokio::test]
    async fn a_codex_start_launches_after_the_claim_records_its_leader_and_initializes() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (result, (asked, then)) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-1", &codex)),
            play_app_server(
                &rig,
                serde_json::json!({"result": {"userAgent": "stand-in"}})
            )
        );
        let project = rig.project.clone();
        let uid = match &result {
            StartSessionResult::Started { session_uid, cwd } => {
                assert_eq!(cwd.as_deref(), Some(project.as_str()));
                session_uid.clone()
            }
            other => panic!("expected a start, got {other:?}"),
        };
        let dir = crate::claude_host::run_dir(&rig.root, &uid);
        let watch = rig
            .daemon
            .db
            .watched_host(uid.clone())
            .await
            .unwrap()
            .unwrap();
        end_run(&rig, &uid).await;

        assert_eq!(asked["method"], "initialize");
        assert_eq!(asked["params"]["capabilities"]["experimentalApi"], true);
        assert_eq!(then, serde_json::json!({"method": "initialized"}));
        assert_eq!(watch.lifecycle, Lifecycle::Starting);
        let claimed: i32 = std::fs::read_to_string(dir.join("as.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(watch.host.pid, claimed, "the claim is the recorded leader");
        assert!(watch.boot.is_some());
        assert_eq!((mode(&dir), mode(&dir.join("host.log"))), (0o700, 0o600));
        let argv = std::fs::read(dir.join("argv")).unwrap();
        let argv: Vec<String> = argv
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        let real = std::fs::canonicalize(&project)
            .unwrap()
            .display()
            .to_string();
        assert_eq!(
            argv,
            [
                "-c".to_string(),
                format!(r#"projects={{ "{real}" = {{ trust_level = "untrusted" }} }}"#),
                "app-server".into(),
                "--listen".into(),
                format!("unix://{}", dir.join("as.sock").display()),
            ]
        );

        let again = start(&rig.daemon, run(&rig, "req-1", &codex)).await;
        let restarted = start(&daemon_on(&rig.root), run(&rig, "req-1", &codex)).await;
        assert_eq!(
            again,
            StartSessionResult::Started {
                session_uid: uid.clone(),
                cwd: Some(project.clone())
            }
        );
        assert_eq!(restarted, again);
        assert_eq!(launches(&rig), 1, "a repeat launched another app-server");
    }

    /// The leader leads its own group and session: `setsid` ran before codex.
    #[tokio::test]
    async fn the_leader_leads_its_own_group_and_session() {
        let rig = rig();
        let codex = stand_in(&rig, "exec sleep 30");
        let (result, _) = tokio::join!(
            start(&rig.daemon, run(&rig, "req-group", &codex)),
            play_app_server(&rig, serde_json::json!({"result": {}}))
        );
        let StartSessionResult::Started { session_uid, .. } = result else {
            panic!("expected a start, got {result:?}");
        };
        let watch = rig
            .daemon
            .db
            .watched_host(session_uid.clone())
            .await
            .unwrap()
            .unwrap();
        let pid = watch.host.pid;
        // SAFETY: getpgid(2) and getsid(2) only read.
        let (group, session) = unsafe { (libc::getpgid(pid), libc::getsid(pid)) };
        end_run(&rig, &session_uid).await;
        assert_eq!((group, session), (pid, pid));
        assert_ne!(group, unsafe { libc::getpgid(0) });
    }

    /// **A codex that never listens is stopped, and its run failed**: at the deadline the
    /// start answers why, the run is `failed`, and the ladder empties the run's group, a
    /// child that ignores SIGTERM included.
    #[tokio::test]
    async fn a_codex_that_never_listens_is_stopped_and_its_run_failed() {
        let rig = rig();
        let codex = stand_in(&rig, "(trap '' TERM; exec sleep 60) &\nexec sleep 60");
        let result = start_within(
            &rig.daemon,
            run(&rig, "req-silent", &codex),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("did not listen")),
            "{result:?}"
        );
        let dir = run_folder(&rig).await;
        let leader: i32 = std::fs::read_to_string(dir.join("as.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let run = rig
            .daemon
            .db
            .hosted_run_for_request("req-silent".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.lifecycle, Lifecycle::Failed);
        assert!(
            !group_alive(leader),
            "the run's group outlived its failed start"
        );
    }

    /// **A codex that ends at once fails the start with what it said last**, and the run
    /// is `failed`.
    #[tokio::test]
    async fn a_codex_that_ends_at_once_fails_its_start_with_its_last_line() {
        let rig = rig();
        let codex = stand_in(&rig, "echo 'error: unexpected argument' >&2; exit 2");
        let result = start(&rig.daemon, run(&rig, "req-ends", &codex)).await;
        assert_eq!(
            result,
            StartSessionResult::Failed {
                reason: StartFailure::LaunchFailed,
                detail: "error: unexpected argument".into(),
            }
        );
        let run = rig
            .daemon
            .db
            .hosted_run_for_request("req-ends".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.lifecycle, Lifecycle::Failed);
    }

    /// `git -C dir <args>`, quietly.
    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// **The trust list is every folder from the repository's top down to the run's, by
    /// their real paths, each untrusted, or the run's folder alone outside git**; a path
    /// is one TOML string however it is spelled.
    #[tokio::test]
    async fn the_trust_list_is_the_real_folders_from_the_top_down() {
        let rig = rig();
        let top = rig.root.join("repo");
        let deep = top.join("a b").join("q\"uo\\te");
        std::fs::create_dir_all(&deep).unwrap();
        git(&top, &["init", "-q", "-b", "main"]);
        let real = |path: &Path| {
            serde_json::to_string(&std::fs::canonicalize(path).unwrap().display().to_string())
                .unwrap()
        };
        let entry = |path: &Path| format!(r#"{} = {{ trust_level = "untrusted" }}"#, real(path));
        assert_eq!(
            projects(deep.to_str().unwrap(), None).await.unwrap(),
            format!(
                "{{ {}, {}, {} }}",
                entry(&top),
                entry(&top.join("a b")),
                entry(&deep)
            )
        );
        let plain = rig.root.join("project");
        assert_eq!(
            projects(plain.to_str().unwrap(), None).await.unwrap(),
            format!("{{ {} }}", entry(&plain))
        );
        assert!(
            real(&plain).starts_with("\"/private/tmp/") || !cfg!(target_os = "macos"),
            "the folder is listed by its real path: {}",
            real(&plain)
        );
        assert!(projects("/nonexistent/folder", None).await.is_err());
    }
}
