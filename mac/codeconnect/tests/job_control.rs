//! `codeconnect internal-job` in a real pane of a private tmux server: an agent that
//! stops itself is really stopped, held while a viewer at the Mac is attached, continued
//! by the viewer's SIGUSR1, hung up when the last viewer or the session goes, ended with
//! the session when it dies stopped, and let go when no viewer at the Mac holds it. The
//! phone's terminal client (`ignore-size`) is never a viewer.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A private tmux server whose one pane runs `sh -c SCRIPT` under `internal-job`.
struct Pane {
    dir: PathBuf,
    socket: PathBuf,
    tmux: PathBuf,
}

impl Pane {
    fn start(script: &str) -> Pane {
        Pane::start_with(&[], script)
    }

    /// [`Pane::start`], with `options` ahead of the job's program; `DIR` in either
    /// is the pane's directory.
    fn start_with(options: &[&str], script: &str) -> Pane {
        let tmux = protocol::tmux::tmux_bin().expect("job control tests require tmux");
        // Short: a unix socket path is capped near 104 bytes.
        let dir = Path::new("/tmp").join(format!("ccj-{}", protocol::uid::new().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let pane = Pane {
            socket: dir.join("s"),
            dir,
            tmux,
        };
        let dir = pane.dir.display().to_string();
        let script = script.replace("DIR", &dir);
        let started = pane
            .command()
            .args(["new-session", "-d", "-s", "j", "-x", "80", "-y", "24", "--"])
            .arg(env!("CARGO_BIN_EXE_codeconnect"))
            .arg("internal-job")
            .args(options.iter().map(|option| option.replace("DIR", &dir)))
            .args(["/bin/sh", "-c", &script])
            .status()
            .unwrap();
        assert!(started.success(), "new-session");
        pane
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.tmux);
        command
            .arg("-S")
            .arg(&self.socket)
            .args(["-f", "/dev/null"]);
        command
    }

    fn ask(&self, format: &str) -> String {
        let out = self
            .command()
            .args(["display-message", "-p", "-t", "j:", format])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn pane_pid(&self) -> i32 {
        self.ask("#{pane_pid}").parse().unwrap()
    }

    /// The agent: the pane process's one child.
    fn agent(&self) -> i32 {
        let mut agent = None;
        wait_for("the agent to start", || {
            let out = Command::new("/usr/bin/pgrep")
                .args(["-P", &self.pane_pid().to_string()])
                .output()
                .unwrap();
            agent = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .and_then(|pid| pid.parse().ok());
            agent.is_some()
        });
        agent.unwrap()
    }

    fn stopped_flag(&self) -> String {
        self.ask("#{@codeconnect-stopped}")
    }

    fn alive(&self) -> bool {
        self.command()
            .args(["has-session", "-t", "=j"])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// A control-mode client, as the viewer is; its output is collected until it ends.
    fn viewer(&self) -> Viewer {
        self.client(&["-C", "attach", "-t", "j"])
    }

    /// The client ccd attaches for the phone's terminal tab.
    fn phone(&self) -> Viewer {
        self.client(&[
            "-N",
            "-C",
            "attach-session",
            "-E",
            "-f",
            "ignore-size",
            "-t",
            "j",
        ])
    }

    fn client(&self, args: &[&str]) -> Viewer {
        let before = self.ask("#{session_attached}");
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = child.stdout.take().unwrap();
        let output = std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = out.read_to_end(&mut all);
            all
        });
        wait_for("the client to attach", || {
            self.ask("#{session_attached}") != before
        });
        Viewer { child, output }
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.dir.join(name), "").unwrap();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = self
            .command()
            .arg("kill-server")
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Viewer {
    child: Child,
    output: std::thread::JoinHandle<Vec<u8>>,
}

impl Viewer {
    /// Detach as a closed tab does, and return everything tmux sent.
    fn leave(mut self) -> String {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        self.child.wait().unwrap();
        String::from_utf8_lossy(&self.output.join().unwrap()).into_owned()
    }
}

fn state(pid: i32) -> String {
    let out = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stop once `DIR/go` exists, then say so and stay.
const STOPS_ON_GO: &str = "while [ ! -e DIR/go ]; do sleep 0.02; done; kill -TSTP $$; \
                           echo resumed > DIR/after; exec sleep 30";

/// [`STOPS_ON_GO`], but on SIGHUP it puts its terminal modes back before it exits.
const RESTORES_ON_HANGUP: &str = "exec python3 -c 'import os, signal, termios, time\n\
def hangup(*_):\n    termios.tcsetattr(0, termios.TCSANOW, termios.tcgetattr(0))\n    os._exit(0)\n\
signal.signal(signal.SIGHUP, hangup)\n\
while not os.path.exists(\"DIR/go\"): time.sleep(0.02)\n\
os.kill(os.getpid(), signal.SIGTSTP)\n\
time.sleep(30)'";

#[test]
fn a_viewer_holds_the_stop_and_its_resume_continues_the_agent() {
    let pane = Pane::start(STOPS_ON_GO);
    let viewer = pane.viewer();
    let agent = pane.agent();
    pane.touch("go");
    wait_for("the stop to be held", || pane.stopped_flag() == "1");
    assert!(
        state(agent).starts_with('T'),
        "really stopped: {}",
        state(agent)
    );
    assert_eq!(pane.file("after"), "", "nothing ran past the stop");

    unsafe { libc::kill(pane.pane_pid(), libc::SIGUSR1) };
    wait_for("the agent to continue", || {
        pane.file("after") == "resumed\n"
    });
    assert_eq!(pane.stopped_flag(), "");
    assert!(!state(agent).starts_with('T'));
    let output = viewer.leave();
    assert!(
        output.contains("\\033P=codeconnect-stopped\\033\\134"),
        "the viewer was told: {output}"
    );
}

#[test]
fn the_last_viewer_leaving_a_stopped_agent_hangs_it_up() {
    let pane = Pane::start(&format!(
        "trap 'echo hangup > DIR/hup; exit 0' HUP; {STOPS_ON_GO}"
    ));
    // The phone's terminal stays; it does not hold the stop.
    let phone = pane.phone();
    let viewer = pane.viewer();
    pane.touch("go");
    wait_for("the stop to be held", || pane.stopped_flag() == "1");
    viewer.leave();
    wait_for("the hangup", || pane.file("hup") == "hangup\n");
    wait_for("the session to end", || !pane.alive());
    assert_eq!(pane.file("after"), "", "it never went on");
    phone.leave();
}

#[test]
fn a_stop_nobody_holds_is_dropped() {
    let pane = Pane::start(STOPS_ON_GO);
    // The phone's terminal is attached; it cannot `fg`, so it holds nothing.
    let phone = pane.phone();
    let agent = pane.agent();
    pane.touch("go");
    wait_for("the agent to go on", || pane.file("after") == "resumed\n");
    assert!(!state(agent).starts_with('T'));
    assert_eq!(pane.stopped_flag(), "");
    phone.leave();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(pane.alive(), "the phone's client leaving ends nothing");
}

#[test]
fn the_session_ending_while_the_agent_is_stopped_ends_the_job() {
    for end in ["kill-session", "kill-server"] {
        // On hangup it puts its terminal back, as a TUI does: while the pane's process
        // still holds the terminal, that stops it again (SIGTTOU).
        let pane = Pane::start(RESTORES_ON_HANGUP);
        let viewer = pane.viewer();
        let job = pane.pane_pid();
        let agent = pane.agent();
        pane.touch("go");
        wait_for("the stop to be held", || pane.stopped_flag() == "1");
        let args: &[&str] = if end == "kill-session" {
            &["kill-session", "-t", "=j"]
        } else {
            &["kill-server"]
        };
        pane.command().args(args).status().unwrap();
        wait_for(end, || state(job).is_empty() && state(agent).is_empty());
        viewer.leave();
    }
}

/// A new server started on the socket as soon as the old one exits is another one: the
/// job ends, and the new server's pane, `%0` as the old one was, is left alone.
#[test]
fn a_server_replaced_while_the_agent_is_stopped_is_left_alone() {
    let pane = Pane::start(STOPS_ON_GO);
    let viewer = pane.viewer();
    let job = pane.pane_pid();
    let agent = pane.agent();
    pane.touch("go");
    wait_for("the stop to be held", || pane.stopped_flag() == "1");
    let server: i32 = pane.ask("#{pid}").parse().unwrap();
    pane.command().arg("kill-server").status().unwrap();
    wait_for("the old server to exit", || state(server).is_empty());
    let replaced = pane
        .command()
        .args(["new-session", "-d", "-s", "j", "sleep", "60", ";"])
        .args([
            "set-option",
            "-p",
            "-t",
            "j:",
            "@codeconnect-stopped",
            "new",
        ])
        .status()
        .unwrap();
    assert!(replaced.success(), "the new server");
    wait_for("the job to end", || {
        state(job).is_empty() && state(agent).is_empty()
    });
    assert_eq!(pane.ask("#{pane_id}"), "%0");
    assert_eq!(
        pane.stopped_flag(),
        "new",
        "the new server's pane untouched"
    );
    viewer.leave();
}

#[test]
fn an_agent_killed_while_stopped_ends_the_session() {
    let pane = Pane::start(STOPS_ON_GO);
    let viewer = pane.viewer();
    let agent = pane.agent();
    pane.touch("go");
    wait_for("the stop to be held", || pane.stopped_flag() == "1");
    unsafe { libc::kill(agent, libc::SIGKILL) };
    wait_for("the session to end", || !pane.alive());
    viewer.leave();
}

/// A program reading the terminal in its ordinary line mode, stopped by the terminal's
/// own Ctrl+Z, reads on once continued, as it does as a shell's job.
#[test]
fn a_reader_stopped_by_the_terminal_reads_on_after_it_is_continued() {
    let pane = Pane::start("exec python3 -c 'import os,sys\nwhile True:\n d = os.read(0, 64)\n open(sys.argv[1], \"ab\").write(d)' DIR/read");
    let viewer = pane.viewer();
    let agent = pane.agent();
    let keys = |hex: &str| {
        pane.command()
            .args(["send-keys", "-t", "j:", "-H", hex])
            .status()
            .unwrap()
    };
    keys("1a");
    wait_for("the stop to be held", || pane.stopped_flag() == "1");
    assert!(state(agent).starts_with('T'), "{}", state(agent));
    unsafe { libc::kill(pane.pane_pid(), libc::SIGUSR1) };
    wait_for("the reader to continue", || pane.stopped_flag().is_empty());
    keys("61");
    keys("0d");
    wait_for("the reader to read", || pane.file("read") == "a\n");
    viewer.leave();
}

#[test]
fn the_job_ends_as_its_agent_ends() {
    let run = |script: &str| {
        Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(["internal-job", "/bin/sh", "-c", script])
            .stdin(Stdio::null())
            .status()
            .unwrap()
    };
    assert_eq!(run("exit 7").code(), Some(7));
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(run("kill -TERM $$").signal(), Some(libc::SIGTERM));
}

/// **An OpenCode pane's job**: it starts the agent only while the pinned binary
/// still hashes to its digest, publishes the agent's own pid in the session
/// directory, and takes the session's files with it however the agent ends,
/// leaving the rest of the directory alone.
#[test]
fn an_opencode_job_publishes_its_agent_and_takes_its_files_with_it() {
    use std::os::unix::process::ExitStatusExt;
    let dir = std::env::temp_dir().join(format!("ccj-oc-{}", protocol::uid::new().unwrap()));
    protocol::fsperm::private_dir(&dir).unwrap();
    let binary = Path::new("/bin/sh");
    let pinned = protocol::hash::sha256_file(binary).unwrap();
    let session_files = ["codeconnect-opencode.js", "tui.json"];
    let run = |digest: &str, script: &str| {
        for name in session_files.iter().chain(&["environment"]) {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let script = script.replace("DIR", &dir.display().to_string());
        Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .arg("internal-job")
            .arg("--opencode-dir")
            .arg(&dir)
            .arg("--verify")
            .arg(binary)
            .arg(digest)
            .args(["/bin/sh", "-c", &script])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
    };
    let left = || {
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };

    let ended = run(
        &pinned,
        "while [ ! -e DIR/agent.json ]; do sleep 0.05; done; \
         cp DIR/agent.json DIR/seen; echo $$ > DIR/pid; exit 3",
    );
    assert_eq!(ended.code(), Some(3));
    let seen: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("seen")).unwrap()).unwrap();
    let pid: i64 = std::fs::read_to_string(dir.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(seen["pid"], pid, "the published pid is the agent's own");
    assert!(seen["start"]["sec"].is_i64() && seen["start"]["usec"].is_i64());
    assert_eq!(left(), ["environment", "pid", "seen"]);

    let ended = run(&pinned, "kill -TERM $$");
    assert_eq!(ended.signal(), Some(libc::SIGTERM));
    assert_eq!(left(), ["environment", "pid", "seen"]);

    let other = "0".repeat(64);
    let ended = run(&other, "touch DIR/ran");
    assert_eq!(ended.code(), Some(1), "a binary that changed does not run");
    assert_eq!(left(), ["environment", "pid", "seen"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// **An OpenCode agent that cannot start leaves its reason on screen**: a binary
/// that changed, or a program that cannot be run. The pane stays, saying why, until
/// Return closes it.
#[test]
fn an_opencode_job_that_cannot_start_shows_why_until_return() {
    let other = "0".repeat(64);
    for (options, reason) in [
        (
            &["--opencode-dir", "DIR", "--verify", "/bin/sh", &other][..],
            "is not the one this launch pinned",
        ),
        (
            &["--opencode-dir", "DIR", "DIR/missing"][..],
            "starting /tmp/",
        ),
    ] {
        let pane = Pane::start_with(options, "touch DIR/ran");
        let screen = || {
            let out = pane
                .command()
                .args(["capture-pane", "-p", "-J", "-t", "j:"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        wait_for("the reason on screen", || screen().contains(reason));
        std::thread::sleep(Duration::from_millis(300));
        assert!(pane.alive(), "the pane stays while its reason is shown");
        assert!(!pane.dir.join("ran").exists());
        pane.command()
            .args(["send-keys", "-t", "j:", "Enter"])
            .status()
            .unwrap();
        wait_for("Return to close the session", || !pane.alive());
    }
}

/// **A signal while the pinned binary is being hashed takes the session's files**,
/// as it does once the agent runs. The binary is a FIFO, so the hash waits on it.
#[test]
fn a_signal_during_the_hash_removes_the_session_files() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::ExitStatusExt;
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        let dir = std::env::temp_dir().join(format!("ccj-sig-{}", protocol::uid::new().unwrap()));
        protocol::fsperm::private_dir(&dir).unwrap();
        for name in ["codeconnect-opencode.js", "tui.json", "environment"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let binary = dir.join("opencode");
        let path = std::ffi::CString::new(binary.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut job = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .arg("internal-job")
            .arg("--opencode-dir")
            .arg(&dir)
            .arg("--verify")
            .arg(&binary)
            .arg("0".repeat(64))
            .args(["/bin/sh", "-c", "exit 0"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // A writer opens only once the job is reading the FIFO.
        let mut writer = None;
        wait_for("the job to open the binary", || {
            writer = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&binary)
                .ok();
            writer.is_some()
        });
        unsafe { libc::kill(job.id() as i32, signal) };
        let ended = job.wait().unwrap();
        drop(writer);
        assert_eq!(ended.signal(), Some(signal), "signal {signal}");
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["environment", "opencode"], "signal {signal}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
