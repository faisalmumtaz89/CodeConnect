//! Exercise the shipping attach command through a real terminal and private tmux server.

use std::process::Command;
use std::time::Duration;
use std::{fs, path::Path};

#[test]
fn local_attach_carries_bytes_queries_resize_reattach_and_exit() {
    let tmux = protocol::tmux::tmux_bin().expect("terminal input tests require tmux");
    let root = Path::new("/tmp").join(format!(
        "cc-input-{}",
        protocol::uid::new().expect("unique test directory")
    ));
    fs::create_dir(&root).expect("create private terminal test directory");
    let mut command = Command::new("python3");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/terminal_input.py"
        ))
        .arg(env!("CARGO_BIN_EXE_codeconnect"))
        .arg(&tmux)
        .arg(&root);
    let outcome = protocol::proc::run_deadlined(&mut command, Duration::from_secs(45));
    // The parent owns cleanup even if the terminal fixture is killed at its
    // deadline. A failed cleanup leaves the socket reachable for recovery.
    let cleanup = protocol::proc::run_deadlined(
        Command::new(tmux)
            .env("LC_ALL", "C")
            .arg("-S")
            .arg(root.join("s"))
            .arg("kill-server"),
        Duration::from_secs(3),
    );
    let cleaned = matches!(
        &cleanup,
        Ok(protocol::proc::RunOutcome::Completed { status, stderr, .. })
            if status.success()
                || String::from_utf8_lossy(stderr).starts_with("no server running on ")
                || (!root.join("s").exists()
                    && String::from_utf8_lossy(stderr).contains("No such file or directory"))
    );
    assert!(
        cleaned,
        "private server cleanup failed; retained {}: {cleanup:?}",
        root.display()
    );
    fs::remove_dir_all(&root).expect("remove terminal test directory");
    let outcome = outcome.expect("terminal input tests require python3");
    match outcome {
        protocol::proc::RunOutcome::Completed {
            status,
            stdout,
            stderr,
            ..
        } => {
            assert!(
                status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        other => panic!("terminal input test did not complete: {other:?}"),
    }
}
