//! Talking to the live daemon: its socket, its database, its process.
//!
//! Everything here reads the *production* installation — `~/.codeconnect` — on
//! purpose. A soak against a private fixture would prove the code compiles;
//! this one is meant to prove the thing that is actually running survives being
//! attacked, so it uses the same socket the hooks use and the same database the
//! phone reads.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use protocol::ipc::{ClientFrame, DaemonFrame, DaemonInfo, HookPost};
use rusqlite::{Connection, OpenFlags};

/// A dead daemon that launchd is restarting reappears within a second or two;
/// this is the ceiling before the run is called a failure.
pub const RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// One request over the unix socket.
pub fn request(frame: &ClientFrame) -> Result<DaemonFrame> {
    let socket = protocol::socket_path();
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()?;

    let mut response = String::new();
    if BufReader::new(stream).read_line(&mut response)? == 0 {
        bail!("ccd closed the connection without replying");
    }
    Ok(serde_json::from_str(response.trim())?)
}

/// Fire a hook the way `cc-hook` does, and do not wait for a decision.
///
/// Returns `false` when the daemon could not be reached. That is not an error
/// during a kill storm — it is the fail-open path working — so the caller counts
/// it rather than aborting.
pub fn post_hook(post: &HookPost) -> bool {
    let Ok(mut stream) = UnixStream::connect(protocol::socket_path()) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Ok(mut line) = serde_json::to_vec(&ClientFrame::Hook(post.clone())) else {
        return false;
    };
    line.push(b'\n');
    stream.write_all(&line).is_ok() && stream.flush().is_ok()
}

pub fn daemon_info() -> Result<DaemonInfo> {
    match request(&ClientFrame::DaemonInfo)? {
        DaemonFrame::Daemon(info) => Ok(info),
        other => bail!("unexpected reply to daemon_info: {other:?}"),
    }
}

pub fn sessions() -> Result<Vec<protocol::event::SessionSummary>> {
    match request(&ClientFrame::ListSessions)? {
        DaemonFrame::Sessions { sessions } => Ok(sessions),
        other => bail!("unexpected reply to list_sessions: {other:?}"),
    }
}

/// Wait for the daemon to answer again, reporting how long it took.
pub fn wait_for_daemon(timeout: Duration) -> Result<(DaemonInfo, Duration)> {
    let start = Instant::now();
    loop {
        if let Ok(info) = daemon_info() {
            return Ok((info, start.elapsed()));
        }
        if start.elapsed() > timeout {
            bail!(
                "ccd did not come back within {}s — is the LaunchAgent installed? \
                 (`codeconnect daemon status`)",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Mint a pairing code, exactly as `codeconnect pair` does.
///
/// Over the unix socket, which is the operator's own authority: a process that
/// can reach `~/.codeconnect/ccd.sock` can already do everything the CLI can.
/// The terminal scenarios need this because a live terminal is refused to the
/// static token, and a code is the only thing that buys a device credential.
pub fn create_pairing() -> Result<String> {
    match request(&ClientFrame::CreatePairing {
        ttl_secs: 0,
        allow_ssh: false,
    })? {
        DaemonFrame::Pairing { code, .. } => Ok(code),
        DaemonFrame::Error { message } => {
            bail!("the daemon refused to mint a pairing code: {message}")
        }
        other => bail!("unexpected reply to create_pairing: {other:?}"),
    }
}

/// Every device the daemon has a row for, revoked ones included.
///
/// The terminal scenarios read this immediately before and immediately after
/// the one pairing they perform, because the difference is the only
/// *authoritative* answer to "what did this run create". The pairing ack is
/// not: the code is redeemed and the row written before the ack is composed, so
/// a lost ack orphans a grant the run then cannot name. See
/// [`crate::scenarios::release_device`].
pub fn devices() -> Result<Vec<protocol::pairing::DeviceSummary>> {
    match request(&ClientFrame::ListDevices)? {
        DaemonFrame::Devices { devices } => Ok(devices),
        DaemonFrame::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply to list_devices: {other:?}"),
    }
}

/// Revoke a device by name, as `codeconnect revoke` does. `false` means it was
/// already revoked, which is not an error.
///
/// `device` is whatever `RevokeDevice` accepts — a device id, an id prefix or
/// an exact name. The gauntlet passes the **id**, because the daemon uniquifies
/// a requested name against every row it has including revoked ones, so the
/// name this run asked for is routinely not the name it was given.
pub fn revoke_device(device: &str) -> Result<bool> {
    match request(&ClientFrame::RevokeDevice {
        device: device.to_string(),
        ssh_only: false,
    })? {
        DaemonFrame::Revoked { token_revoked, .. } => Ok(token_revoked),
        DaemonFrame::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply to revoke_device: {other:?}"),
    }
}

pub fn token() -> Result<String> {
    let path = protocol::token_path();
    Ok(std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_string())
}

// ------------------------------------------------------------------ database

/// A read-only view of the live event log.
///
/// Read-write flags without ever writing: SQLite in WAL mode needs to map the
/// shared-memory index, and a strictly read-only connection to a database with
/// an active writer is the one configuration that fails for reasons unrelated to
/// what is being measured.
pub fn open_db() -> Result<Connection> {
    let path = protocol::db_path();
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("opening {}", path.display()))
}

/// The two integrity claims the event log makes, asked of every run at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    /// Runs whose `MAX(seq)` and `COUNT(*)` disagree — a hole or a burnt number.
    pub gaps: Vec<String>,
    /// `(session_uid, source, source_event_id)` triples appearing more than
    /// once. The unique index should make this impossible; it is checked anyway
    /// because "impossible" is what the index asserts, not what the data proves.
    pub duplicates: Vec<String>,
    pub runs: usize,
    pub events: u64,
}

impl Integrity {
    pub fn is_clean(&self) -> bool {
        self.gaps.is_empty() && self.duplicates.is_empty()
    }
}

pub fn check_integrity(conn: &Connection) -> Result<Integrity> {
    let mut gaps = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT session_uid, MAX(seq), COUNT(*) FROM events
              GROUP BY session_uid HAVING MAX(seq) != COUNT(*)",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            gaps.push(format!(
                "{}: max_seq={} count={}",
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?
            ));
        }
    }

    let mut duplicates = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT session_uid, source, source_event_id, COUNT(*) FROM events
              WHERE source_event_id IS NOT NULL
              GROUP BY session_uid, source, source_event_id
              HAVING COUNT(*) > 1",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            duplicates.push(format!(
                "{}/{}/{} x{}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?
            ));
        }
    }

    let runs: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    let events: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    Ok(Integrity {
        gaps,
        duplicates,
        runs: runs as usize,
        events: events as u64,
    })
}

pub fn count_events(conn: &Connection, session_uid: &str) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE session_uid = ?1",
        [session_uid],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

pub fn max_seq(conn: &Connection, session_uid: &str) -> Result<u64> {
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_uid = ?1",
        [session_uid],
        |row| row.get(0),
    )?;
    Ok(seq as u64)
}

/// How many events in this run carry exactly this natural id.
pub fn count_by_source_event_id(
    conn: &Connection,
    session_uid: &str,
    source_event_id: &str,
) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE session_uid = ?1 AND source_event_id = ?2",
        [session_uid, source_event_id],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// Wait until `predicate` holds against a freshly read database, or give up.
pub fn wait_until<F>(timeout: Duration, mut predicate: F) -> Result<Duration>
where
    F: FnMut(&Connection) -> Result<bool>,
{
    let start = Instant::now();
    loop {
        // Reopened each time: a long-lived connection in WAL mode holds a read
        // snapshot, and this is a poll for *new* writes by definition.
        if let Ok(conn) = open_db() {
            if predicate(&conn)? {
                return Ok(start.elapsed());
            }
        }
        if start.elapsed() > timeout {
            return Err(anyhow!("condition not reached within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ----------------------------------------------------------------------- tmux

/// How long one tmux command gets. A shared server under load answers in
/// milliseconds; a stopped one never answers at all, which is what the deadline
/// is for and what [`TmuxAnswer::Unknown`] then reports.
const TMUX_DEADLINE: Duration = Duration::from_secs(5);

/// What one tmux command answered — and, before anything else, *whether it
/// answered at all*.
///
/// The three outcomes [`protocol::proc::run_deadlined`] distinguishes are kept
/// distinct here because every assertion downstream turns on the difference.
/// tmux exiting non-zero is a **real answer**: `has-session` exiting 1 means
/// the session is gone, `list-clients` exiting 1 means there is no session to
/// count clients on. tmux not answering — no binary, or a deadline blown —
/// means **nothing was learned**, and the one thing a gauntlet must never do is
/// let nothing-learned take the value that passes.
///
/// This existed as `Option<String>` and collapsed all three into `None`.
/// Measured consequence, reachable in every `ccsoak all` run: the `tmuxfreeze`
/// scenario SIGSTOPs this same shared server a few scenarios earlier, and a
/// loaded server blows the five-second deadline routinely — after which
/// "0 tmux clients are attached" and "the scratch session is verifiably gone"
/// were both being reported off a command that never ran to completion. The
/// second of those disarmed the only watchdog left over a live session on the
/// operator's server.
#[derive(Debug)]
pub enum TmuxAnswer {
    /// tmux ran and exited zero; its stdout.
    Ok(String),
    /// tmux ran and exited non-zero. An answer, and often the interesting one.
    Failed {
        /// `None` when the child was signalled rather than exiting.
        status: Option<i32>,
        stderr: String,
    },
    /// Nothing was learned, and why not.
    Unknown(String),
}

impl TmuxAnswer {
    /// The stdout of a command that succeeded, for callers that genuinely
    /// cannot act on the difference between the other two.
    pub fn ok(self) -> Option<String> {
        match self {
            TmuxAnswer::Ok(out) => Some(out),
            _ => None,
        }
    }

    /// One sentence naming what happened, for an assertion message that must
    /// never read as a measurement. `doing` is the present-tense thing being
    /// attempted, e.g. "counting the session's tmux clients".
    pub fn complaint(&self, doing: &str) -> String {
        match self {
            TmuxAnswer::Ok(_) => format!("{doing}: tmux answered"),
            TmuxAnswer::Failed { status, stderr } => {
                let said = stderr.trim();
                match status {
                    Some(code) => format!("{doing}: tmux exited {code} ({said})"),
                    None => format!("{doing}: tmux was signalled ({said})"),
                }
            }
            TmuxAnswer::Unknown(why) => format!("{doing}: {why}"),
        }
    }
}

/// One bounded command against the **shared** `codeconnect` server.
///
/// Note what is absent: nothing here ever issues `kill-server`. The operator's
/// agents live on this socket, and a harness that took the whole server down to
/// clean up after itself would be the worst failure in this file.
pub fn tmux_answer(args: &[&str]) -> TmuxAnswer {
    let Some(bin) = protocol::tmux::tmux_bin() else {
        return TmuxAnswer::Unknown("tmux is not installed, so nothing was asked".to_string());
    };
    let mut command = std::process::Command::new(bin);
    command
        .args(["-L", protocol::TMUX_SOCKET_NAME])
        .args(args)
        .env_remove("TMUX");
    classify(protocol::proc::run_deadlined(&mut command, TMUX_DEADLINE))
}

/// The outcome-to-answer mapping, free of the spawn so it can be exercised.
///
/// Split out because it is the whole of the fix and none of it needs tmux: as
/// an inline `match` it could only ever be checked by freezing the operator's
/// server, which is to say never under `cargo test`.
fn classify(outcome: std::io::Result<protocol::proc::RunOutcome>) -> TmuxAnswer {
    match outcome {
        Ok(protocol::proc::RunOutcome::Completed {
            status,
            stdout,
            stderr,
        }) => {
            if status.success() {
                TmuxAnswer::Ok(String::from_utf8_lossy(&stdout).into_owned())
            } else {
                TmuxAnswer::Failed {
                    status: status.code(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                }
            }
        }
        // Indeterminate on purpose: `run_deadlined` kills and reaps a child that
        // overran, and whether it *acted* first is unknowable. A `kill-session`
        // that times out may or may not have killed the session.
        Ok(protocol::proc::RunOutcome::TimedOut { waited }) => TmuxAnswer::Unknown(format!(
            "tmux did not answer within {waited:?}, so nothing was learned — the shared server \
             may be stopped or loaded"
        )),
        Err(err) => TmuxAnswer::Unknown(format!("tmux could not be started: {err}")),
    }
}

/// The stdout of a successful tmux command, for the callers that do not care
/// why an unsuccessful one failed.
///
/// Kept beside [`tmux_answer`] rather than migrating every caller: for a
/// `send-keys` that makes a pane print, or a best-effort `kill-session` whose
/// result is verified separately, "it did not work" is the whole of the useful
/// information and a tri-state at the call site would be noise. Every caller
/// whose *assertion* turns on the difference uses [`tmux_answer`].
pub fn tmux(args: &[&str]) -> Option<String> {
    tmux_answer(args).ok()
}

// -------------------------------------------------------------------- process

/// `kill -9` by pid, through `/bin/kill` so no libc dependency is needed.
pub fn kill_9(pid: u32) -> Result<()> {
    let status = std::process::Command::new("/bin/kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .context("running /bin/kill -9")?;
    if !status.success() {
        bail!("kill -9 {pid} failed ({status})");
    }
    Ok(())
}

pub fn scratch_dir(tag: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "ccsoak-{tag}-{}-{}",
        std::process::id(),
        protocol::time::now_unix_ms()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::proc::RunOutcome;
    use std::os::unix::process::ExitStatusExt;

    fn completed(code: i32, stdout: &str, stderr: &str) -> std::io::Result<RunOutcome> {
        Ok(RunOutcome::Completed {
            // The wait(2) encoding: the exit code lives in the high byte.
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn a_refusal_and_a_silence_are_not_the_same_answer() {
        // The distinction the tri-state exists for. `has-session` exiting 1 is
        // tmux saying the session is gone; a blown deadline is tmux saying
        // nothing at all, and a soak that read the second as the first would
        // disarm a live session's last rescuer.
        assert!(matches!(
            classify(completed(1, "", "can't find session: nosuch\n")),
            TmuxAnswer::Failed {
                status: Some(1),
                ..
            }
        ));
        assert!(matches!(
            classify(Ok(RunOutcome::TimedOut {
                waited: Duration::from_secs(5)
            })),
            TmuxAnswer::Unknown(_)
        ));
        assert!(matches!(
            classify(Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
            TmuxAnswer::Unknown(_)
        ));
    }

    #[test]
    fn a_successful_command_yields_its_stdout_and_nothing_else_does() {
        assert!(matches!(
            classify(completed(0, "12345\n", "")),
            TmuxAnswer::Ok(out) if out == "12345\n"
        ));
        // The `ok()` shim collapses the two non-answers, which is exactly why
        // it may not be used where an assertion turns on them.
        assert_eq!(
            classify(completed(0, "12345\n", "")).ok().as_deref(),
            Some("12345\n")
        );
        assert_eq!(classify(completed(1, "", "boom")).ok(), None);
    }

    #[test]
    fn a_complaint_never_reads_as_a_measurement() {
        // The failure mode this replaces printed "0 tmux client(s) attached"
        // for a command that never completed. Whatever the reason, the sentence
        // has to name the reason.
        for answer in [
            classify(completed(1, "", "can't find session: nosuch\n")),
            classify(Ok(RunOutcome::TimedOut {
                waited: Duration::from_secs(5),
            })),
        ] {
            let said = answer.complaint("counting the session's tmux clients");
            assert!(
                said.starts_with("counting the session's tmux clients: "),
                "{said}"
            );
            assert!(
                said.len() > "counting the session's tmux clients: ".len(),
                "{said}"
            );
        }
    }
}
