//! `codeconnect claude|codex --no-attach`, run as the daemon runs it: no terminal at
//! all, stdin from nowhere, the outcome in the file `CODECONNECT_LAUNCH_RESULT`
//! names. It hosts the session a terminal start hosts, at 120x40, with the first
//! prompt after `--` exactly as given; a missing agent or a refused argument is said
//! in the result and leaves no session.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use protocol::launch_result::{LaunchResult, ENV_LAUNCH_RESULT};

/// A short root under `/tmp`: tmux's socket lives in `TMUX_TMPDIR`, and a unix
/// socket path must fit `sun_path`.
struct Rig {
    root: PathBuf,
    /// The sessions it started, whose processes it waits out before it goes.
    started: std::cell::RefCell<Vec<String>>,
}

impl Rig {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cc-noattach-{}",
            protocol::uid::new().expect("a unique test directory")
        ));
        std::fs::create_dir_all(root.join("cc")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        Rig {
            root,
            started: Default::default(),
        }
    }

    /// A claude that writes its arguments, `TERM` and whether it saw the result
    /// variable, then stays up as a session's agent does.
    fn claude(&self) -> PathBuf {
        let claude = self.root.join("claude");
        std::fs::write(
            &claude,
            format!(
                "#!/bin/sh\nprintf '%s\\000' \"$@\" > {seen}/argv\n\
                 printf '%s|%s' \"$TERM\" \"${{{ENV_LAUNCH_RESULT}+set}}\" > {seen}/env\nexec sleep 30\n",
                seen = self.root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        claude
    }

    /// A codex that answers `--version`, serves an app-server socket, and as the
    /// TUI writes its arguments and size, then stays up.
    fn codex(&self) -> PathBuf {
        let python = protocol::tmux::search_path("python3").expect("python3 for the fake codex");
        let codex = self.root.join("codex");
        std::fs::write(
            &codex,
            format!(
                r#"#!{python}
import os, socket, sys, time
argv = sys.argv[1:]
if argv == ["--version"]:
    print("codex-cli 0.160.0"); sys.exit(0)
if argv and argv[0] == "app-server":
    path = argv[argv.index("--listen") + 1][len("unix://"):]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.bind(path); os.chmod(path, 0o600); s.listen(16)
else:
    open("{seen}/size", "w").write("%d %d" % tuple(os.get_terminal_size(0)))
    open("{seen}/argv", "wb").write(b"".join(a.encode() + b"\0" for a in argv))
while True:
    time.sleep(3600)
"#,
                python = python.display(),
                seen = self.root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        codex
    }

    fn config(&self, agent: &str, binary: &Path) {
        std::fs::write(
            self.root.join("cc/config.json"),
            format!(r#"{{"{agent}_bin": {binary:?}, "update_check": false}}"#),
        )
        .unwrap();
    }

    /// Run the launch with no terminal and no `TERM`, as a daemon's child.
    fn launch(&self, agent: &str, args: &[&str]) -> (Option<i32>, String, Option<LaunchResult>) {
        let result = self.root.join("result");
        let _ = std::fs::remove_file(&result);
        let mut command = Command::new(env!("CARGO_BIN_EXE_codeconnect"));
        command
            .arg(agent)
            .args(args)
            .current_dir(self.root.join("project"))
            .env_remove("TERM")
            .env_remove("CODECONNECT_CLAUDE_BIN")
            .env_remove("CODECONNECT_CODEX_BIN")
            // No agent on it but the configured one, so a missing one is missing.
            .env("PATH", "/usr/bin:/bin")
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env("HOME", &self.root)
            .env("CODECONNECT_HOME", self.root.join("cc"))
            .env("TMUX_TMPDIR", &self.root)
            .env(ENV_LAUNCH_RESULT, &result)
            .stdin(Stdio::null());
        if let Some(tmux) = protocol::tmux::tmux_bin() {
            command.env("CODECONNECT_TMUX", tmux);
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let run = command
            .output()
            .expect("running codeconnect claude --no-attach");
        let printed = String::from_utf8_lossy(&run.stdout).into_owned()
            + &String::from_utf8_lossy(&run.stderr);
        let result = LaunchResult::read_from(&result).ok();
        if let Some(LaunchResult::Started { uid, .. }) = &result {
            self.started.borrow_mut().push(uid.clone());
        }
        (run.status.code(), printed, result)
    }

    fn tmux(&self, args: &[&str]) -> String {
        let tmux = protocol::tmux::tmux_bin().expect("a tmux");
        let socket = self
            .root
            .join(format!("tmux-{}/codeconnect", unsafe { libc::getuid() }));
        let output = Command::new(tmux)
            .arg("-S")
            .arg(socket)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The file the stand-in claude writes, once it has.
    fn seen(&self, name: &str) -> Vec<u8> {
        let path = self.root.join(name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::read(&path).unwrap_or_default()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if protocol::tmux::tmux_bin().is_some() {
            self.tmux(&["kill-server"]);
        }
        // A session's supervisor, and a Codex session's custodian, clean up after
        // its server has gone; the folder they work in goes once none is left.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        while std::time::Instant::now() < deadline {
            let table = Command::new("ps")
                .args(["-axo", "command"])
                .output()
                .unwrap();
            let table = String::from_utf8_lossy(&table.stdout);
            if !self
                .started
                .borrow()
                .iter()
                .any(|uid| table.contains(uid.as_str()))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn with_no_terminal_it_hosts_the_session_and_says_which() {
    if protocol::tmux::tmux_bin().is_none() {
        return;
    }
    let rig = Rig::new();
    let claude = rig.claude();
    rig.config("claude", &claude);
    let prompt = "-p fix \"it\" 'now'; $(touch pwned) `id`\nsecond line;";

    let (status, printed, result) = rig.launch("claude", &["--no-attach", "--", prompt]);

    assert_eq!(status, Some(0), "{printed}");
    let Some(LaunchResult::Started {
        uid,
        session_name,
        agent,
    }) = result
    else {
        panic!("no started result: {result:?} {printed}");
    };
    assert_eq!(session_name, "cc-1");
    assert_eq!(agent, protocol::agent::AgentKind::Claude);
    assert!(protocol::uid::is_well_formed(&uid), "{uid}");
    let argv = rig.seen("argv");
    let argv: Vec<&[u8]> = argv.split(|&byte| byte == 0).collect();
    assert_eq!(argv[0], b"--mcp-config", "{argv:?}");
    assert_eq!(argv[2], b"--settings");
    assert_eq!(
        &argv[4..],
        [&b"--"[..], prompt.as_bytes(), b""],
        "the prompt, exactly"
    );
    assert_eq!(rig.seen("env"), b"xterm-256color|");
    assert!(!rig.root.join("project/pwned").exists());
    // Nobody saw the mail question, so it is neither asked nor answered here.
    assert!(!rig.root.join("cc/mail-trust").exists());
    assert_eq!(
        rig.tmux(&[
            "display-message",
            "-p",
            "-t",
            "=cc-1:",
            "#{window_width}x#{window_height} #{session_attached} #{CODECONNECT_SESSION_UID}"
        ]),
        format!("120x40 0 {uid}")
    );
}

#[test]
fn a_missing_claude_or_a_refused_argument_is_said_and_hosts_nothing() {
    let rig = Rig::new();
    rig.config("claude", &rig.root.join("no-claude-here"));
    let (status, printed, result) = rig.launch("claude", &["--no-attach"]);
    assert_ne!(status, Some(0), "{printed}");
    assert!(
        matches!(
            &result,
            Some(LaunchResult::Failed { error: protocol::ws::StartFailure::AgentMissing, detail })
                if detail.contains("could not find the claude binary")
        ),
        "{result:?}"
    );

    let claude = rig.claude();
    rig.config("claude", &claude);
    for args in [
        &["--no-attach", "--model", "x"][..],
        &["--no-attach", "prompt"],
    ] {
        let (status, printed, result) = rig.launch("claude", args);
        assert_ne!(status, Some(0), "{args:?}: {printed}");
        assert!(
            matches!(
                &result,
                Some(LaunchResult::Failed {
                    error: protocol::ws::StartFailure::LaunchFailed,
                    ..
                })
            ),
            "{args:?}: {result:?}"
        );
    }
    assert!(
        !rig.root.join("cc/sessions").exists(),
        "a session was created"
    );
}

#[test]
fn codex_with_no_terminal_hosts_the_session_and_says_which() {
    if protocol::tmux::tmux_bin().is_none() {
        return;
    }
    let rig = Rig::new();
    let codex = rig.codex();
    rig.config("codex", &codex);
    let prompt = "-p fix \"it\" 'now'; $(touch pwned)\nsecond line;";

    let (status, printed, result) = rig.launch("codex", &["--no-attach", "--", prompt]);

    assert_eq!(status, Some(0), "{printed}");
    let Some(LaunchResult::Started {
        uid,
        session_name,
        agent,
    }) = result
    else {
        panic!("no started result: {result:?} {printed}");
    };
    assert_eq!(session_name, "cc-1");
    assert_eq!(agent, protocol::agent::AgentKind::Codex);
    let argv = rig.seen("argv");
    let argv: Vec<&[u8]> = argv.split(|&byte| byte == 0).collect();
    assert_eq!(argv[0], b"--remote", "{argv:?}");
    assert_eq!(
        &argv[2..],
        [&b"--"[..], prompt.as_bytes(), b""],
        "the prompt, exactly"
    );
    assert_eq!(rig.seen("size"), b"120 40");
    assert!(!rig.root.join("project/pwned").exists());
    assert_eq!(
        rig.tmux(&[
            "display-message",
            "-p",
            "-t",
            "=cc-1:",
            "#{session_attached} #{CODECONNECT_SESSION_UID}"
        ]),
        format!("0 {uid}")
    );
}
