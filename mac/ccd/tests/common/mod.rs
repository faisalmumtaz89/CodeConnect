//! A `ccd` run as a real process in a home of its own under `/tmp`, with `HOME`,
//! `CODECONNECT_HOME` and `TMUX_TMPDIR` all pointed there and a loopback listener
//! on a port the kernel chose, so nothing reaches the operator's own daemon, state
//! or tmux server.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Home(pub PathBuf);

impl Home {
    pub fn new(tag: &str) -> Home {
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

    pub fn socket(&self) -> PathBuf {
        self.0.join("ccd.sock")
    }

    pub fn start(&self, name: &str) -> Daemon {
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

pub struct Daemon {
    pub child: Child,
    output: PathBuf,
}

impl Daemon {
    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.output).unwrap_or_default()
    }

    pub fn wait_until_serving(&self) {
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

    pub fn wait_for_exit(&mut self) -> std::process::ExitStatus {
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
