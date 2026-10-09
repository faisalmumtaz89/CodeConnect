//! A held hook outlives nothing: when the process that ran it (Claude) dies,
//! the hook exits and closes its connection, which the daemon reads as Claude
//! ending the hook.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_held_hook_exits_when_its_parent_dies() {
    let dir = std::env::temp_dir().join(format!("cc-hook-parent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("ccd.sock");
    let payload = dir.join("payload.json");
    std::fs::write(
        &payload,
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
    )
    .unwrap();
    // A daemon that takes the request and never answers.
    let listener = UnixListener::bind(&socket).unwrap();

    // The fake parent: a shell that runs the hook, says its pid, and waits on it.
    let mut parent = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#""$0" --gate --socket "$1" < "$2" > /dev/null & echo $!; wait"#)
        .arg(env!("CARGO_BIN_EXE_cc-hook"))
        .arg(&socket)
        .arg(&payload)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut hook_pid = String::new();
    BufReader::new(parent.stdout.take().unwrap())
        .read_line(&mut hook_pid)
        .unwrap();
    let mut held = listener.accept().unwrap().0;

    parent.kill().unwrap();
    parent.wait().unwrap();
    held.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let killed = Instant::now();
    let closed = held.read_to_end(&mut Vec::new()).is_ok();
    let took = killed.elapsed();

    // Never leave an orphan behind, whatever the outcome.
    let _ = Command::new("/bin/kill")
        .args(["-9", hook_pid.trim()])
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        closed,
        "the hook still held its connection 5 s after its parent died"
    );
    assert!(took < Duration::from_secs(2), "it took {took:?}");
}
