//! Live end to end for `codeconnect opencode`: a real `ccd`, the real launcher, a
//! real OpenCode 1.18.34 and the plugin's scripted mock model, in a private home.
//!
//! 1. The launch registers an OpenCode run under the folder it opened, and the
//!    terminal attaches; `codeconnect ls` lists it.
//! 2. The plugin inside the pane is admitted: the run's timeline says attached.
//! 3. A typed prompt runs a turn that lands as facts, ending in a TurnComplete.
//! 4. Ending the session ends the run, and the session's OpenCode files go.
//! 5. With `ccd` stopped, OpenCode draws its prompt within max(5 s, OpenCode alone
//!    + 1 s), counted from the agent's own start.
//!
//! `#[ignore]` and gated: it runs only with `CC_OPENCODE_LIVE=1`, and then a missing
//! `CC_OPENCODE_BIN` (the native `opencode` executable) is a failure, not a skip.
//! The mock model is run by OpenCode's own runtime (`BUN_BE_BUN=1`).
//!
//! ```text
//! CC_OPENCODE_LIVE=1 CC_OPENCODE_BIN=/path/to/opencode \
//!     cargo test -p ccd --test opencode_live -- --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::ipc::{ClientFrame, DaemonFrame};

const OPENCODE_VERSION: &str = "1.18.34";

/// What OpenCode's TUI draws once its prompt takes keys.
const READY: &str = "ctrl+p commands";

/// A private home under `/tmp`, short enough for every socket in it, with the
/// mock model and the daemon it started. Dropped, it ends everything it started.
struct Rig {
    root: PathBuf,
    opencode: PathBuf,
    codeconnect: PathBuf,
    proj: PathBuf,
    _mock: Mock,
    ccd: Option<Child>,
    launchers: Vec<Child>,
}

/// The mock model, ended when dropped, so a rig that fails while it is being built
/// leaves nothing running.
struct Mock(Child);

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Rig {
    fn new(opencode: PathBuf) -> Rig {
        let root = PathBuf::from(format!("/tmp/ccoc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home/.config/opencode", "cc", "tmux", "tmp", "proj"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let proj = std::fs::canonicalize(root.join("proj")).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .status()
            .unwrap();
        let mock_model = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../codeconnect/opencode-plugin/live/mock-model.js");
        let mut mock = Mock(
            Command::new(&opencode)
                .env("BUN_BE_BUN", "1")
                .arg(mock_model)
                .arg("0")
                .stdout(Stdio::piped())
                .spawn()
                .expect("start the mock model"),
        );
        let stdout = mock.0.stdout.take().unwrap();
        let (said, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = said.send(line);
        });
        let line = heard
            .recv_timeout(Duration::from_secs(30))
            .expect("the mock model never said it was listening");
        let port: u16 = line
            .trim()
            .strip_prefix("listening ")
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("the mock model said {line:?}"));
        let config = serde_json::json!({
            "autoupdate": false,
            "share": "disabled",
            "model": "mock/mock-model",
            "small_model": "mock/mock-model",
            "provider": {"mock": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Mock",
                "options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "x"},
                "models": {"mock-model": {"name": "Mock Model", "tool_call": true,
                    "limit": {"context": 1000000, "output": 100000}}},
            }},
            "permission": {"*": "allow", "bash": "allow"},
        });
        std::fs::write(
            root.join("home/.config/opencode/opencode.json"),
            config.to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("cc/config.json"),
            r#"{"ws_port":0,"ws_bind":"127.0.0.1","ws_loopback":false,"push_enabled":false,"update_check":false}"#,
        )
        .unwrap();
        Rig {
            codeconnect: build_codeconnect(),
            root,
            opencode,
            proj,
            _mock: mock,
            ccd: None,
            launchers: Vec::new(),
        }
    }

    /// Every process here gets this environment and nothing else.
    fn env(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("TERM", "xterm-256color")
            .env("CODECONNECT_HOME", self.root.join("cc"))
            .env("TMUX_TMPDIR", self.root.join("tmux"))
            .env("CODECONNECT_OPENCODE_BIN", &self.opencode)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("NO_PROXY", "127.0.0.1,localhost");
    }

    fn start_ccd(&mut self) {
        let log = std::fs::File::create(self.root.join("ccd.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccd"));
        self.env(&mut command);
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.ccd = Some(child);
        assert!(
            wait_until(Duration::from_secs(30), || UnixStream::connect(
                self.root.join("cc/ccd.sock")
            )
            .is_ok()),
            "ccd never served:\n{}",
            self.read("ccd.log")
        );
    }

    fn signal_ccd(&self, signal: libc::c_int) {
        let pid = self.ccd.as_ref().expect("ccd runs").id() as libc::pid_t;
        unsafe { libc::kill(pid, signal) };
    }

    /// `codeconnect opencode` in a terminal of its own, its input held open so the
    /// attach it ends with stays.
    fn launch(&mut self, tag: &str) {
        let out = std::fs::File::create(self.root.join(format!("{tag}.out"))).unwrap();
        let mut command = Command::new("/usr/bin/script");
        self.env(&mut command);
        let launcher = command
            .args(["-q", "/dev/null"])
            .arg(&self.codeconnect)
            .arg("opencode")
            .current_dir(&self.proj)
            .env("PWD", &self.proj)
            .stdin(Stdio::piped())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .expect("run codeconnect opencode");
        self.launchers.push(launcher);
    }

    fn sessions(&self) -> Vec<protocol::event::SessionSummary> {
        let mut stream = UnixStream::connect(self.root.join("cc/ccd.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut line = serde_json::to_vec(&ClientFrame::ListSessions).unwrap();
        line.push(b'\n');
        stream.write_all(&line).unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        match serde_json::from_str(&reply).unwrap() {
            DaemonFrame::Sessions { sessions } => sessions,
            other => panic!("unexpected reply {other:?}"),
        }
    }

    /// The payloads of `uid`'s facts of `kind`, in order.
    fn facts(&self, uid: &str, kind: &str) -> Vec<String> {
        let Ok(db) = rusqlite::Connection::open_with_flags(
            self.root.join("cc/events.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) else {
            return Vec::new();
        };
        let Ok(mut query) = db.prepare(
            "SELECT payload FROM events WHERE session_uid = ?1 AND kind = ?2 ORDER BY seq",
        ) else {
            return Vec::new();
        };
        query
            .query_map([uid, kind], |row| row.get(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    fn tmux(&self, server: &str, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .env("TMUX_TMPDIR", self.root.join("tmux"))
            .env_remove("TMUX")
            .args(["-L", server, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn pane(&self, server: &str, name: &str) -> String {
        self.tmux(server, &["capture-pane", "-p", "-t", &format!("={name}:")])
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    /// How long OpenCode alone takes to draw its prompt here, in a pane of its own
    /// with no plugin.
    fn opencode_alone(&self) -> Duration {
        let mut command = Command::new("tmux");
        self.env(&mut command);
        let started = Instant::now();
        let status = command
            .args(["-L", "plain", "-f", "/dev/null", "new-session", "-d"])
            .args(["-s", "plain", "-x", "160", "-y", "50", "--"])
            .arg(&self.opencode)
            .current_dir(&self.proj)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            wait_until(Duration::from_secs(60), || self
                .pane("plain", "plain")
                .contains(READY)),
            "OpenCode alone never drew its prompt:\n{}",
            self.pane("plain", "plain")
        );
        let took = started.elapsed();
        self.tmux("plain", &["kill-server"]);
        took
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.tmux("codeconnect", &["kill-server"]);
        self.tmux("plain", &["kill-server"]);
        if let Some(mut ccd) = self.ccd.take() {
            let _ = ccd.kill();
            let _ = ccd.wait();
        }
        for child in &mut self.launchers {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Supervisors are detached and name the session folder in their argv.
        let _ = Command::new("pkill")
            .args(["-KILL", "-f"])
            .arg(self.root.file_name().unwrap())
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Builds the launcher from this tree, in the profile this test's `ccd` was built
/// with, and returns the executable cargo reports for it.
fn build_codeconnect() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    // `target/<profile dir>/ccd`, where the `dev` profile's directory is `debug`.
    let profile = match Path::new(env!("CARGO_BIN_EXE_ccd"))
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    {
        Some("debug") => "dev",
        Some(name) => name,
        None => panic!("no profile directory above {}", env!("CARGO_BIN_EXE_ccd")),
    };
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "codeconnect", "--bin", "codeconnect"])
        .args(["--profile", profile, "--message-format=json"])
        .current_dir(workspace)
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(out.status.success(), "cargo build -p codeconnect");
    let bin = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["target"]["name"] == "codeconnect")
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .expect("cargo reported no codeconnect executable");
    assert!(bin.is_file(), "no codeconnect at {}", bin.display());
    bin
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The OpenCode under test, or `None` when the live run was not asked for.
fn live_opencode() -> Option<PathBuf> {
    if std::env::var("CC_OPENCODE_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set CC_OPENCODE_LIVE=1 and CC_OPENCODE_BIN to run");
        return None;
    }
    let bin = PathBuf::from(
        std::env::var_os("CC_OPENCODE_BIN").expect("CC_OPENCODE_LIVE=1 needs CC_OPENCODE_BIN"),
    );
    let version = Command::new(&bin).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        OPENCODE_VERSION,
        "{} is not OpenCode {OPENCODE_VERSION}",
        bin.display()
    );
    Some(bin)
}

#[test]
#[ignore = "live: needs a real OpenCode and tmux; run with CC_OPENCODE_LIVE=1 -- --ignored"]
fn codeconnect_opencode_is_observed_end_to_end() {
    let Some(opencode) = live_opencode() else {
        return;
    };
    let mut rig = Rig::new(opencode);
    let alone = rig.opencode_alone();
    rig.start_ccd();

    // 1. Registered under the folder it opened, attached, listed.
    rig.launch("first");
    let mut run = None;
    assert!(
        wait_until(Duration::from_secs(60), || {
            run = rig
                .sessions()
                .into_iter()
                .find(|s| s.agent == protocol::agent::AgentKind::Opencode);
            run.is_some()
        }),
        "no OpenCode run registered. launcher:\n{}\nccd.log:\n{}",
        rig.read("first.out"),
        rig.read("ccd.log")
    );
    let run = run.unwrap();
    let (uid, name) = (run.session_uid.clone(), run.session_id.clone());
    assert_eq!(run.cwd, rig.proj.to_str().unwrap());
    assert_eq!(run.lifecycle, protocol::event::Lifecycle::Live);
    assert!(
        wait_until(Duration::from_secs(30), || !rig
            .tmux("codeconnect", &["list-clients", "-t", &format!("={name}")])
            .trim()
            .is_empty()),
        "the launcher did not attach:\n{}",
        rig.read("first.out")
    );
    let mut ls = Command::new(&rig.codeconnect);
    rig.env(&mut ls);
    let ls = String::from_utf8(ls.arg("ls").output().unwrap().stdout).unwrap();
    assert!(
        ls.lines().any(|line| line.starts_with(&name)
            && line.contains(&uid)
            && line.contains(rig.proj.to_str().unwrap())),
        "`codeconnect ls` does not list the run:\n{ls}"
    );

    // 2. The plugin is admitted.
    assert!(
        wait_until(Duration::from_secs(30), || rig
            .facts(&uid, "link_state")
            .iter()
            .any(|payload| payload.contains(r#""link":"attached""#))),
        "the plugin was never admitted: {:?}\nccd.log:\n{}",
        rig.facts(&uid, "link_state"),
        rig.read("ccd.log")
    );

    // 3. A turn, typed at the keyboard, lands as facts.
    assert!(
        wait_until(Duration::from_secs(60), || rig
            .pane("codeconnect", &name)
            .contains(READY)),
        "OpenCode never drew its prompt:\n{}",
        rig.pane("codeconnect", &name)
    );
    let target = format!("={name}:");
    rig.tmux(
        "codeconnect",
        &["send-keys", "-t", &target, "-l", "cc:fast"],
    );
    std::thread::sleep(Duration::from_millis(200));
    rig.tmux("codeconnect", &["send-keys", "-t", &target, "Enter"]);
    assert!(
        wait_until(Duration::from_secs(60), || !rig
            .facts(&uid, "turn_complete")
            .is_empty()),
        "no TurnComplete. pane:\n{}\nccd.log:\n{}",
        rig.pane("codeconnect", &name),
        rig.read("ccd.log")
    );
    assert!(rig
        .facts(&uid, "user_message")
        .iter()
        .any(|payload| payload.contains("cc:fast")));
    assert!(rig
        .facts(&uid, "agent_message")
        .iter()
        .any(|payload| payload.contains("fast reply done")));

    // 4. The session ends, and so does the run; its OpenCode files go with it.
    let dir = rig.root.join(format!("cc/sessions/{name}-{uid}"));
    assert!(dir.join("tui.json").is_file());
    rig.tmux("codeconnect", &["kill-session", "-t", &format!("={name}")]);
    assert!(
        wait_until(Duration::from_secs(30), || !rig
            .facts(&uid, "session_end")
            .is_empty()),
        "the run never ended. ccd.log:\n{}",
        rig.read("ccd.log")
    );
    assert!(
        wait_until(Duration::from_secs(10), || [
            "codeconnect-opencode.js",
            "tui.json",
            "agent.json"
        ]
        .iter()
        .all(|file| !dir.join(file).exists())),
        "the session's files outlived it"
    );

    // 5. A stopped daemon does not hold the keyboard up.
    rig.signal_ccd(libc::SIGSTOP);
    rig.launch("second");
    let mut agent = None;
    assert!(
        wait_until(Duration::from_secs(30), || {
            agent = std::fs::read_dir(rig.root.join("cc/sessions"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| *path != dir && path.join("agent.json").is_file());
            agent.is_some()
        }),
        "the second launch never published its agent:\n{}",
        rig.read("second.out")
    );
    let second = agent.unwrap();
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(second.join("agent.json")).unwrap()).unwrap();
    let born = UNIX_EPOCH
        + Duration::from_secs(record["start"]["sec"].as_u64().unwrap())
        + Duration::from_micros(record["start"]["usec"].as_u64().unwrap());
    let dir_name = second.file_name().unwrap().to_str().unwrap();
    let second_name = &dir_name[..dir_name.len() - protocol::uid::UID_LEN - 1];
    let budget = (alone + Duration::from_secs(1)).max(Duration::from_secs(5));
    assert!(
        wait_until(budget + Duration::from_secs(5), || rig
            .pane("codeconnect", second_name)
            .contains(READY)),
        "OpenCode never drew its prompt with ccd stopped:\n{}",
        rig.pane("codeconnect", second_name)
    );
    let took = SystemTime::now().duration_since(born).unwrap();
    assert!(
        took <= budget,
        "with ccd stopped the prompt took {took:?}; OpenCode alone took {alone:?}"
    );
    rig.tmux(
        "codeconnect",
        &["kill-session", "-t", &format!("={second_name}")],
    );
    rig.signal_ccd(libc::SIGCONT);
}
