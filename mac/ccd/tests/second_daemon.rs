//! Two `ccd` processes in one home: the one already running keeps it.
//!
//! Real processes, because what is at stake is what survives one of them
//! exiting. Every daemon here runs in a home of its own under `/tmp`, with
//! `HOME`, `CODECONNECT_HOME` and `TMUX_TMPDIR` all pointed there and a
//! loopback listener on a port the kernel chose, so nothing reaches the
//! operator's own daemon, state or tmux server.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Home {
        // Short and under `/tmp`: the socket path must fit `sun_path`.
        let dir = PathBuf::from(format!("/tmp/ccd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tmux")).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            dir.join("config.json"),
            format!(
                r#"{{"ws_port":{port},"ws_bind":"127.0.0.1","ws_loopback":false,"push_enabled":false}}"#
            ),
        )
        .unwrap();
        Home(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("ccd.sock")
    }

    fn start(&self, name: &str) -> Daemon {
        let out = std::fs::File::create(self.0.join(name)).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ccd"))
            .env_remove("XPC_SERVICE_NAME")
            .env_remove("TMUX")
            .env("HOME", &self.0)
            .env("CODECONNECT_HOME", &self.0)
            .env("TMUX_TMPDIR", self.0.join("tmux"))
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        Daemon {
            child,
            output: self.0.join(name),
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon {
    child: Child,
    output: PathBuf,
}

impl Daemon {
    fn output(&self) -> String {
        std::fs::read_to_string(&self.output).unwrap_or_default()
    }

    fn wait_until_serving(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.output().contains("ipc listening") {
            assert!(
                Instant::now() < deadline,
                "ccd never started serving:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "ccd did not exit:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn answers(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

#[test]
fn a_second_daemon_leaves_the_running_one_reachable_and_its_state_untouched() {
    let home = Home::new("second");
    let first = home.start("first.out");
    first.wait_until_serving();
    assert!(answers(&home.socket()));

    let mut second = home.start("second.out");
    let status = second.wait_for_exit();
    let said = second.output();

    assert!(
        answers(&home.socket()),
        "the running daemon's socket is gone:\n{said}"
    );
    assert!(
        !said.contains("recovery:"),
        "the second daemon ran recovery against the running one's database:\n{said}"
    );
    assert!(!status.success(), "the second daemon must fail:\n{said}");
    assert!(
        said.contains("another ccd is already running"),
        "the second daemon must say why:\n{said}"
    );
}

#[test]
fn a_daemon_killed_outright_leaves_nothing_that_stops_the_next_one() {
    let home = Home::new("stale");
    let mut first = home.start("first.out");
    first.wait_until_serving();
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    assert!(home.socket().exists(), "SIGKILL leaves the socket behind");

    let next = home.start("next.out");
    next.wait_until_serving();
    assert!(answers(&home.socket()), "{}", next.output());
}
