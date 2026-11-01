//! **The Codex wake: one content-free nudge into a hosted session's own queue.**
//!
//! Mail never travels through here. It waits in the daemon's mailbox, and the agent
//! takes it with its own `read_messages` call. This only tells an idle (or busy) Codex
//! that there is something to take, and it does that through the documented, stable
//! `codex queue` CLI against the session's upstream app-server (`<run-dir>/as.sock`).
//! The daemon never writes the queue's wire method itself.
//!
//! The words are the line the mail switch picks ([`crate::mail::nudge`]); the phone
//! shows the turn they start as it is.
//! Codex's own queue then decides when the nudge runs, exactly as it does for a person:
//! - at once on an idle thread;
//! - after the running turn ends on a busy one;
//! - held after Esc until the person's next turn ends.
//!
//! The session's `CODEX_HOME` is not needed: the queue lives in the app-server, and a
//! `codex queue --remote` run from an empty `CODEX_HOME` queues the same way (measured,
//! codex 0.164.0).

use crate::state::Daemon;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// How long one `codex queue` may take. It answers in well under a second when the
/// app-server is up; past this the server is wedged, and a wake must not hold the
/// caller for longer.
const WAKE_DEADLINE: Duration = Duration::from_secs(10);

/// How much of `codex queue`'s stderr an error carries.
const STDERR_TAIL: usize = 400;

/// **Wake the Codex run `run_uid`**: queue the mail switch's line on the thread its link
/// adopted, through its app-server (`as.sock`, beside the run's control socket), with
/// the `codex` it was launched with. `Ok` means Codex accepted the nudge into the
/// thread's queue, not that a turn has run. An `Err` (no thread yet, no live
/// supervisor, the CLI refusing) leaves the wake to the next try.
pub(crate) async fn wake_codex(daemon: &Arc<Daemon>, run_uid: &str) -> Result<()> {
    let row = daemon
        .db
        .get_session(run_uid.to_string())
        .await?
        .context("no such run")?;
    let thread = crate::codex_link::real_thread_id(row.codex_thread_id.as_deref())
        .context("the run has no thread yet")?
        .to_string();
    let control = row.codex_socket.context("the run has no control socket")?;
    let codex = daemon
        .agent_bin(run_uid)
        .await
        .context("the run has no live supervisor")?;
    let as_sock = Path::new(&control).with_file_name("as.sock");
    wake(Path::new(&codex), &as_sock, &thread, crate::mail::nudge()).await
}

async fn wake(codex: &Path, as_sock: &Path, thread_id: &str, nudge: &str) -> Result<()> {
    queue(codex, as_sock, thread_id, nudge, WAKE_DEADLINE).await
}

async fn queue(
    codex: &Path,
    as_sock: &Path,
    thread_id: &str,
    nudge: &str,
    deadline: Duration,
) -> Result<()> {
    let output = tokio::process::Command::new(codex)
        .arg("queue")
        .arg("--remote")
        .arg(format!("unix://{}", as_sock.display()))
        .arg("--thread")
        .arg(thread_id)
        .arg("--message")
        .arg(nudge)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // The deadline drops the future; this makes that a kill rather than a leak.
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(deadline, output).await {
        Ok(output) => output.with_context(|| format!("running {} queue", codex.display()))?,
        Err(_) => bail!("codex queue did not answer within {deadline:?}"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let start = stderr
            .char_indices()
            .rev()
            .nth(STDERR_TAIL - 1)
            .map_or(0, |(at, _)| at);
        bail!(
            "codex queue exited with {}: {}",
            output.status,
            &stderr[start..]
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A stand-in `codex`: a shell script with `body`, in its own scratch directory.
    fn fake_codex(tag: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ccd-codex-wake-{tag}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir(&dir).unwrap();
        let bin = dir.join("codex");
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (dir, bin)
    }

    #[tokio::test]
    async fn the_nudge_goes_to_the_named_thread_through_codex_queue() {
        let (dir, bin) = fake_codex(
            "argv",
            r#"for a in "$@"; do printf '%s\n' "$a"; done > "$(dirname "$0")/argv""#,
        );
        wake(
            &bin,
            Path::new("/tmp/cch.x/as.sock"),
            "01a197a7-7ac3-7ed1-ac9e-888bf77b95a4",
            protocol::ipc::MAIL_NUDGE_ACT,
        )
        .await
        .unwrap();
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            [
                "queue",
                "--remote",
                "unix:///tmp/cch.x/as.sock",
                "--thread",
                "01a197a7-7ac3-7ed1-ac9e-888bf77b95a4",
                "--message",
                protocol::ipc::MAIL_NUDGE_ACT,
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_refused_queue_is_an_error_carrying_codexs_words() {
        let (dir, bin) = fake_codex(
            "refused",
            "echo 'Error: thread not found: 01a1' >&2\nexit 1",
        );
        let err = wake(
            &bin,
            Path::new("/tmp/cch.x/as.sock"),
            "01a1",
            protocol::ipc::MAIL_NUDGE_SHOW,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("thread not found: 01a1"),
            "{err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_queue_that_never_answers_is_killed_at_the_deadline() {
        // Hung for 30 s at most, so a build without the kill leaves nothing behind for long.
        // It is found by its own path, which also finds it if the deadline came before it
        // ever ran.
        let (dir, bin) = fake_codex(
            "hang",
            "i=0; while [ $i -lt 30 ]; do sleep 1; i=$((i + 1)); done",
        );
        let started = std::time::Instant::now();
        let err = queue(
            &bin,
            Path::new("/tmp/cch.x/as.sock"),
            "01a1",
            protocol::ipc::MAIL_NUDGE_SHOW,
            Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // Polled on the runtime, so its child reaper runs and a killed child does not
        // linger as a zombie.
        let alive = || {
            std::process::Command::new("pgrep")
                .arg("-f")
                .arg(&bin)
                .output()
                .unwrap()
                .status
                .success()
        };
        let mut gone = false;
        for _ in 0..250 {
            if !alive() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the hung codex outlived the deadline");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
