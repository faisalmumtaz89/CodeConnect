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
use crate::store::SessionRow;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
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

/// **Wake the Codex run `run_uid`**, a run CodeConnect started on its engine
/// ([`wake_engine_run`]). `Ok` means Codex accepted the nudge into the thread's queue,
/// not that a turn has run. An `Err` (no thread yet, the CLI refusing) leaves the wake to
/// the next try.
pub(crate) async fn wake_codex(daemon: &Arc<Daemon>, run_uid: &str) -> Result<()> {
    let row = daemon
        .db
        .get_session(run_uid.to_string())
        .await?
        .context("no such run")?;
    wake_engine_run(daemon, run_uid, &row).await
}

/// **Wake a run CodeConnect started on its engine**: the run's own app-server
/// (`<run-dir>/as.sock`), the thread on its row, and the `codex` a start's resolver finds
/// now ([`crate::start_session::codex_path`]; a start also reads its version, a wake does
/// not), run as a start runs it, through the login shell with the environment a start
/// reads ([`crate::start_session::login_shell_command`]), so a `codex` whose
/// `#!/usr/bin/env node` needs the shell's own `PATH` runs here too. The deadline covers
/// running the queue, not finding the `codex`. A written limit: that `codex` can be another
/// install than the one the run was started with, or one too old to queue; a queue it
/// refuses leaves the wake to the next try.
async fn wake_engine_run(daemon: &Arc<Daemon>, run_uid: &str, row: &SessionRow) -> Result<()> {
    let thread = row
        .codex_thread_id
        .clone()
        .context("the run has no thread yet")?;
    let as_sock = crate::claude_host::run_dir(&protocol::root_dir(), run_uid).join("as.sock");
    let environment = crate::start_session::phone_environment();
    let codex = crate::start_session::codex_path(&environment, daemon.config.codex_bin.as_deref())
        .await
        .context("no codex to run the wake with")?;
    let command = through_login_shell(
        &environment,
        &codex,
        &as_sock,
        &thread,
        crate::mail::nudge(),
    );
    run(command, &codex, WAKE_DEADLINE).await
}

/// `codex queue …` for an engine run, run by the login shell with `environment`.
fn through_login_shell(
    environment: &HashMap<OsString, OsString>,
    codex: &Path,
    as_sock: &Path,
    thread_id: &str,
    nudge: &str,
) -> tokio::process::Command {
    let args = queue_args(as_sock, thread_id, nudge);
    let mut argv: Vec<&OsStr> = vec![codex.as_os_str()];
    argv.extend(args.iter().map(OsString::as_os_str));
    crate::start_session::agent_command(environment, None, &argv)
}

/// The arguments of `codex queue` that put `nudge` on `thread_id` through `as_sock`.
fn queue_args(as_sock: &Path, thread_id: &str, nudge: &str) -> Vec<OsString> {
    [
        OsString::from("queue"),
        OsString::from("--remote"),
        OsString::from(format!("unix://{}", as_sock.display())),
        OsString::from("--thread"),
        OsString::from(thread_id),
        OsString::from("--message"),
        OsString::from(nudge),
    ]
    .into()
}

/// Run a `codex queue` `command` within `deadline`; its stderr's tail is the error.
async fn run(mut command: tokio::process::Command, codex: &Path, deadline: Duration) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Past the deadline its group is ended, what it started with it.
    let Some(output) = crate::start_session::agent_output(command, deadline).await else {
        bail!(
            "{} queue did not answer within {deadline:?}",
            codex.display()
        );
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
        let mut command = tokio::process::Command::new(&bin);
        command.args(queue_args(
            Path::new("/tmp/cch.x/as.sock"),
            "01a197a7-7ac3-7ed1-ac9e-888bf77b95a4",
            protocol::ipc::MAIL_NUDGE_ACT,
        ));
        run(command, &bin, WAKE_DEADLINE).await.unwrap();
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
        let mut command = tokio::process::Command::new(&bin);
        command.args(queue_args(
            Path::new("/tmp/cch.x/as.sock"),
            "01a1",
            protocol::ipc::MAIL_NUDGE_SHOW,
        ));
        let err = run(command, &bin, WAKE_DEADLINE).await.unwrap_err();
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
        let mut command = tokio::process::Command::new(&bin);
        command.args(queue_args(
            Path::new("/tmp/cch.x/as.sock"),
            "01a1",
            protocol::ipc::MAIL_NUDGE_SHOW,
        ));
        let err = run(command, &bin, Duration::from_millis(500))
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

    /// **An engine run's wake runs `codex queue` through the person's login shell**, as a start
    /// runs codex: the program is the environment's `SHELL`, zsh here and so `-lic`; its
    /// arguments carry the codex and `queue --remote unix://… --thread … --message …`; and its
    /// environment is the one given. Run directly, a `codex` whose `#!/usr/bin/env node` needs
    /// the shell's own `PATH` would not start.
    #[test]
    fn an_engine_runs_wake_runs_codex_queue_through_the_login_shell() {
        let environment = HashMap::from([
            (OsString::from("SHELL"), OsString::from("/bin/zsh")),
            (OsString::from("HOME"), OsString::from("/tmp/cch.x/home")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let command = through_login_shell(
            &environment,
            Path::new("/tmp/cch.x/home/.local/bin/codex"),
            Path::new("/tmp/cch.x/runs/01K1B3XQ8ZC0DE5FGH7JKMNPQR/as.sock"),
            "01a197a7-7ac3-7ed1-ac9e-888bf77b95a4",
            protocol::ipc::MAIL_NUDGE_ACT,
        );
        let command = command.as_std();
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(command.get_program(), "/bin/zsh");
        assert_eq!(args.first(), Some(&OsStr::new("-lic")), "{args:?}");
        assert!(
            args.ends_with(&[
                OsStr::new("/tmp/cch.x/home/.local/bin/codex"),
                OsStr::new("queue"),
                OsStr::new("--remote"),
                OsStr::new("unix:///tmp/cch.x/runs/01K1B3XQ8ZC0DE5FGH7JKMNPQR/as.sock"),
                OsStr::new("--thread"),
                OsStr::new("01a197a7-7ac3-7ed1-ac9e-888bf77b95a4"),
                OsStr::new("--message"),
                OsStr::new(protocol::ipc::MAIL_NUDGE_ACT),
            ]),
            "{args:?}"
        );
        let given: HashMap<OsString, OsString> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_os_string(),
                    value.expect("set, not removed").to_os_string(),
                )
            })
            .collect();
        assert_eq!(given, environment);
    }
}
