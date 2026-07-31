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
                 (`cc daemon status`)",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
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
