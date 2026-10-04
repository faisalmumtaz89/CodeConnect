//! The hook Claude holds while it asks its own question, driven against a fake
//! daemon socket: the question reaches the daemon as one it may hold, and the
//! hook waits on it with no deadline of its own and without closing its side.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use protocol::hook::HookDecision;
use protocol::ipc::{ClientFrame, DaemonFrame};

/// A directory removed when the test ends, passed or failed.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_question_is_held_for_as_long_as_the_daemon_takes() {
    let dir = Scratch(std::env::temp_dir().join(format!("cc-hook-q-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let socket = dir.0.join("d.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();

    let mut hook = Command::new(env!("CARGO_BIN_EXE_cc-hook"))
        .args(["--session", "cc-1", "--gate", "--gate-timeout-ms", "300"])
        .arg("--socket")
        .arg(&socket)
        .env_remove("CODECONNECT_DEBUG")
        .env_remove(protocol::ENV_SESSION_UID)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion",
                 "tool_input":{"questions":[{"question":"Q","options":[{"label":"A"}]}]}}"#,
        )
        .unwrap();

    let (stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let ClientFrame::Hook(post) = serde_json::from_str(line.trim()).unwrap() else {
        panic!("not a hook frame: {line}");
    };
    assert!(post.wait && post.holds_questions, "{line}");

    // Well past the 300 ms gate timeout: the hook neither closes its side nor
    // gives up waiting.
    let mut stream = reader.into_inner();
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .unwrap();
    match stream.read(&mut [0; 1]) {
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        other => panic!("the hook let the question go: {other:?}"),
    }
    assert!(
        hook.try_wait().unwrap().is_none(),
        "the hook stopped waiting"
    );

    let reply = DaemonFrame::HookReply {
        decision: HookDecision::ask("answered"),
    };
    stream
        .write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
        .unwrap();
    let started = Instant::now();
    let status = hook.wait().unwrap();
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(5));
}
