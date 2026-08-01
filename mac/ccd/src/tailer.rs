//! JSONL transcript tailer — content and backfill.
//!
//! The cursor is `(file identity, byte offset, hash of the last line consumed)`.
//! File identity alone is not enough: an editor or a crash can leave the same
//! inode with different bytes at that offset. Re-hashing the last consumed line
//! turns "resume where I left off" into a checked claim; when the check fails we
//! rescan from zero and let `(source, source_event_id)` dedup absorb it, which
//! is why a rescan cannot duplicate or renumber the log.
//!
//! An incomplete trailing line (no newline yet) is never consumed. Claude
//! appends whole JSON objects, but a poll can land mid-write, and half a line
//! parsed as a fact would be a permanent lie in an append-only log.
//!
//! **FSEvents on top of the poll, never instead of it.** A watcher turns a
//! 250ms average latency into a few milliseconds, which is what the phone
//! feels. But FSEvents coalesces, can drop notifications under load, and says
//! nothing at all if the watch fails to register — so the poll stays running as
//! the correctness floor. The watcher's only power is to make a poll happen
//! *sooner*; it can never be the reason one happens at all. That asymmetry is
//! why adding it cannot introduce a missed transcript line.
//!
//! Directories are watched rather than files: the transcript does not exist yet
//! when a session starts, and a rewrite that swaps the inode would leave a
//! file watch pointing at bytes nobody will write to again.
//!
//! Cursors are keyed by `session_uid`, so `codeconnect claude --resume` in a reused name
//! is a *new* run that reads the transcript into its own log rather than one
//! that finds a cursor claiming the file is already consumed. It costs one
//! backfill; the alternative was a timeline with its first half missing.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use protocol::event::{EventKind, PendingEvent, SessionKey, Source};
use tokio::sync::mpsc;

use crate::state::Daemon;
use crate::store::TailCursor;

/// Bytes consumed per session per tick. Bounds memory during a cold backfill of
/// a 20MB transcript; the remainder arrives on the next tick.
const MAX_BYTES_PER_POLL: u64 = 4 * 1024 * 1024;

/// Coalescing window for filesystem notifications. Claude writes a transcript
/// line as several small writes; without this, one line would cost several
/// scans. Short enough to stay imperceptible, long enough to batch a line.
const FSEVENT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(15);

/// How much of an unreadable line is carried as evidence.
///
/// Enough to recognise what was lost, small enough that a hundred of them
/// cannot themselves become the problem. The line's hash is recorded in full
/// alongside, so the fact identifies the exact bytes even though it does not
/// contain them.
const EVIDENCE_BYTES: usize = 512;

/// Parse-error facts emitted from a single scan.
///
/// A transcript is JSONL, so an unreadable line is genuinely anomalous and
/// worth a fact. A *corrupt* transcript is not anomalous line by line, and
/// without a ceiling one damaged file would write a fact per line into the
/// event log — turning a reporting mechanism into the outage.
const MAX_PARSE_ERRORS_PER_SCAN: usize = 32;

/// Transcripts being tailed, keyed by `session_uid`.
type Tails = HashMap<String, Tail>;

/// One session's transcript. The display name rides along so every ingested
/// line carries it without a lookup per line.
struct Tail {
    session: SessionKey,
    path: String,
}

/// What the daemon asks the tailer to do.
///
/// This used to be a bare `(session_uid, path)` — follow this, and never
/// anything else, because there was no way to say a run had finished. The cost
/// was measured on the owner's machine: 1,282 `fsevents could not watch` lines
/// in one error log, every one of them a *deleted* working directory belonging
/// to a session that had ended days earlier, re-armed and re-failed on every
/// single daemon start. A tail nobody can end is a `stat` per session per poll,
/// for ever, on behalf of agents that are gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailCommand {
    /// This run's transcript is at `path`; follow it.
    Follow { session_uid: String, path: String },
    /// This run has ended. Read whatever it wrote last, then let it go.
    Stop { session_uid: String },
}

pub async fn run(daemon: Arc<Daemon>, mut rx: mpsc::UnboundedReceiver<TailCommand>) {
    let mut tails: Tails = HashMap::new();

    // Restart path: everything we already knew is re-tailed from its cursor, so
    // a `kill -9` costs at most the events written while we were dead.
    //
    // Everything *still running*, that is. A run that has ended has nothing
    // left to write, and re-arming a watch on its working directory — usually
    // deleted by now — is filesystem work and a warning per session on behalf
    // of an agent that is gone. The liveness sweep runs before this task is
    // even spawned, so by the time this list is read the ended runs are
    // already marked as such.
    let mut final_reads = Vec::new();
    match daemon.db.list_sessions().await {
        Ok(rows) => {
            let (resumed, ended) = tails_to_resume(rows);
            tails = resumed;
            final_reads = ended;
        }
        Err(err) => crate::log_error!("tailer could not list sessions: {err:#}"),
    }
    if !tails.is_empty() || !final_reads.is_empty() {
        crate::log_info!(
            "tailer resuming {} transcript(s); {} ended run(s) read once and released",
            tails.len(),
            final_reads.len()
        );
    }
    // Read once, then let go. A run that ended while this daemon was down can
    // still have transcript bytes nobody consumed, and they are only reachable
    // from here — nothing else will ever look at that file again.
    for tail in final_reads {
        if let Err(err) = poll_once(&daemon, &tail.session, &tail.path).await {
            crate::log_debug!("tail {}: final read failed: {err:#}", tail.session.name);
        }
    }

    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = daemon
        .config
        .fsevents
        .then(|| DirWatcher::new(nudge_tx))
        .flatten();
    if let Some(watcher) = watcher.as_mut() {
        for tail in tails.values() {
            watcher.watch_parent_of(&tail.path);
        }
    }

    let interval = std::time::Duration::from_millis(daemon.config.tail_poll_ms);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = rx.recv() => {
                match command {
                    Some(TailCommand::Follow { session_uid, path }) => {
                        if tails.get(&session_uid).map(|t| t.path.as_str()) == Some(path.as_str()) {
                            continue;
                        }
                        // The name is looked up rather than passed in: the
                        // channel carries an identity, and the identity is what
                        // the name has to be derived from — never the reverse.
                        let session = match daemon.db.get_session(session_uid.clone()).await {
                            Ok(Some(row)) => row.key(),
                            Ok(None) => {
                                crate::log_warn!(
                                    "tailer asked to follow {path} for unknown session {session_uid}"
                                );
                                continue;
                            }
                            Err(err) => {
                                crate::log_error!("tailer could not read {session_uid}: {err:#}");
                                continue;
                            }
                        };
                        crate::log_info!("tailing {path} for {} ({session_uid})", session.name);
                        if let Some(watcher) = watcher.as_mut() {
                            watcher.watch_parent_of(&path);
                        }
                        tails.insert(session_uid, Tail { session, path });
                    }
                    Some(TailCommand::Stop { session_uid }) => {
                        let Some(tail) = tails.remove(&session_uid) else {
                            continue;
                        };
                        // One last read *before* letting go. A run can write its
                        // final transcript lines between the previous poll and
                        // the moment its end is established, and dropping the
                        // tail without reading them would lose the tail-end of
                        // the session permanently — the log's whole promise is
                        // that ending costs nothing. A transcript whose file is
                        // already gone scans to nothing, silently, so this is
                        // free for a run that ended days ago.
                        if let Err(err) = poll_once(&daemon, &tail.session, &tail.path).await {
                            crate::log_debug!(
                                "tail {}: final read failed: {err:#}", tail.session.name
                            );
                        }
                        if let Some(watcher) = watcher.as_mut() {
                            watcher.release_parent_of(&tail.path);
                        }
                        crate::log_debug!(
                            "stopped tailing {} for {} ({session_uid})",
                            tail.path,
                            tail.session.name
                        );
                    }
                    None => return,
                }
            }
            Some(()) = nudge_rx.recv() => {
                // Drain the burst: several writes to one line must cost one scan.
                tokio::time::sleep(FSEVENT_DEBOUNCE).await;
                while nudge_rx.try_recv().is_ok() {}
                poll_all(&daemon, &tails).await;
            }
            _ = ticker.tick() => {
                poll_all(&daemon, &tails).await;
            }
        }
    }
}

/// What a starting tailer should follow, and what it should read once and drop.
///
/// Split out of `run` because the decision is the whole point and the loop
/// around it is not testable. Two rules, and the second was learned the hard
/// way:
///
/// 1. **A run that has ended is not *followed*.** Without this, every session
///    the machine had ever recorded a transcript path for was re-tailed on
///    every start — polled on every tick, and its working directory re-watched.
///    Measured on the owner's Mac: 1,282 `could not watch` failures in one error
///    log, against directories deleted days earlier.
///
/// 2. **…but it is still *read*, once.** The first rule alone was a data-loss
///    bug, and the soak gauntlet caught it: a run can end with transcript bytes
///    the tailer had not yet consumed, and skipping it outright meant those
///    lines were never ingested and never would be. The event log's promise is
///    that a `kill -9` costs nothing but a reconnect; silently abandoning the
///    tail-end of a session's transcript is not nothing. One final scan costs a
///    `stat` — free for a run whose file is already deleted, and free again for
///    one whose cursor is already at the end — and then the run is let go for
///    good, which is what keeps rule 1's benefit.
fn tails_to_resume(rows: Vec<crate::store::SessionRow>) -> (Tails, Vec<Tail>) {
    let mut tails = Tails::new();
    let mut final_reads = Vec::new();
    for row in rows {
        let Some(path) = row.transcript_path.clone() else {
            continue;
        };
        let tail = Tail {
            session: row.key(),
            path,
        };
        if row.lifecycle == protocol::event::Lifecycle::Exited {
            final_reads.push(tail);
            continue;
        }
        tails.insert(row.session_uid.clone(), tail);
    }
    (tails, final_reads)
}

async fn poll_all(daemon: &Arc<Daemon>, tails: &Tails) {
    for tail in tails.values() {
        if let Err(err) = poll_once(daemon, &tail.session, &tail.path).await {
            crate::log_warn!("tail {}: {err:#}", tail.session.name);
        }
    }
}

/// Watches the directories holding transcripts and nudges the tailer.
///
/// It carries no state about *what* changed. Deciding that is the scanner's
/// job, and the cursor already makes a redundant scan free — so the watcher can
/// be as imprecise as FSEvents wants to be without costing correctness.
struct DirWatcher {
    watcher: notify::RecommendedWatcher,
    /// Watched directories, and how many tails still need each.
    ///
    /// Counted rather than a set, because a directory is shared: Claude keeps
    /// every project's transcripts in one folder, so several live sessions
    /// routinely watch the same path. Unwatching when the *first* of them ends
    /// would silently drop the others back to the 250ms poll — a latency
    /// regression with no symptom anyone could trace. Counting is what makes
    /// releasing safe, and releasing is what stops a long-lived daemon
    /// accumulating one kernel watch per session it has ever seen.
    watched: HashMap<PathBuf, usize>,
}

impl DirWatcher {
    fn new(nudge_tx: mpsc::UnboundedSender<()>) -> Option<DirWatcher> {
        // `UnboundedSender::send` is not async and is `Send`, so the notify
        // callback thread can hand work to the runtime without a bridge.
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                let _ = nudge_tx.send(());
            }
        });
        match watcher {
            Ok(watcher) => Some(DirWatcher {
                watcher,
                watched: HashMap::new(),
            }),
            Err(err) => {
                // The poll alone is still correct, so this is a lost
                // optimisation rather than a lost feature.
                crate::log_warn!("fsevents unavailable ({err}); polling only");
                None
            }
        }
    }

    fn watch_parent_of(&mut self, path: &str) {
        let Some(dir) = Path::new(path).parent().map(Path::to_path_buf) else {
            return;
        };
        if let Some(holders) = self.watched.get_mut(&dir) {
            *holders += 1;
            return;
        }
        match self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.watched.insert(dir.clone(), 1);
                crate::log_debug!("fsevents watching {}", dir.display());
            }
            // Logged at debug, not warning. The commonest cause by far is a
            // working directory that no longer exists, which is an ordinary
            // fact about a finished session rather than a fault — and at
            // warning level it produced 1,282 lines in one error log, drowning
            // the entries that did need reading. The poll is still the
            // correctness floor either way, so a lost watch costs latency and
            // nothing else.
            Err(err) => crate::log_debug!("fsevents could not watch {}: {err}", dir.display()),
        }
    }

    /// Give up one hold on a directory, and unwatch it when the last one goes.
    fn release_parent_of(&mut self, path: &str) {
        let Some(dir) = Path::new(path).parent().map(Path::to_path_buf) else {
            return;
        };
        let Some(holders) = self.watched.get_mut(&dir) else {
            return;
        };
        *holders -= 1;
        if *holders > 0 {
            return;
        }
        self.watched.remove(&dir);
        // A failure here means the watch is already gone, which is the state we
        // were asking for.
        let _ = self.watcher.unwatch(&dir);
        crate::log_debug!("fsevents released {}", dir.display());
    }
}

async fn poll_once(daemon: &Arc<Daemon>, session: &SessionKey, path: &str) -> Result<()> {
    let store = Arc::clone(&daemon.store);
    let owned = session.clone();
    let file_path = path.to_string();

    // File IO and the cursor read are blocking; keep them off the runtime.
    let scan = tokio::task::spawn_blocking(move || scan_file(&store, &owned, &file_path))
        .await
        .context("tail task panicked")??;

    let Some(scan) = scan else { return Ok(()) };
    let count = scan.events.len();
    // The events and the cursor that claims them are written together, and the
    // publish happens only after that commit. Any other order can lose a fact
    // permanently: a cursor saved first says "these lines are in the log" while
    // they are not, and nothing ever re-reads those bytes.
    daemon
        .ingest_scan(&session.uid, scan.events, scan.cursor)
        .await?;
    if count > 0 {
        crate::log_debug!("tail {}: ingested {count} transcript line(s)", session.name);
    }
    Ok(())
}

pub(crate) struct Scan {
    pub(crate) events: Vec<PendingEvent>,
    /// Where the scan stopped. Committed with the events above, never before.
    pub(crate) cursor: TailCursor,
}

pub(crate) fn scan_file(
    store: &crate::store::Store,
    session: &SessionKey,
    path: &str,
) -> Result<Option<Scan>> {
    let path = Path::new(path);
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        // Not an error: Claude writes the transcript lazily, and a session with
        // persistence disabled never writes one at all.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("stat transcript"),
    };
    let (dev, ino, len) = (metadata.dev() as i64, metadata.ino() as i64, metadata.len());

    let cursor = store.load_cursor(&session.uid)?;
    let mut file = std::fs::File::open(path).context("open transcript")?;

    let start = match &cursor {
        Some(cursor)
            if cursor.path == path.to_string_lossy()
                && cursor.dev == dev
                && cursor.ino == ino
                && cursor.offset <= len
                && verify_last_line(&mut file, cursor).unwrap_or(false) =>
        {
            cursor.offset
        }
        Some(_) => {
            crate::log_warn!(
                "tail {}: cursor did not verify, rescanning from 0 (dedup absorbs it)",
                session.name
            );
            0
        }
        None => 0,
    };

    if start >= len {
        return Ok(None);
    }

    let to_read = (len - start).min(MAX_BYTES_PER_POLL);
    let mut buffer = vec![0u8; to_read as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buffer)?;

    let mut events = Vec::new();
    let mut consumed: u64 = 0;
    let mut last_line_start = cursor.as_ref().map(|c| c.last_line_start).unwrap_or(0);
    let mut last_line_sha = cursor.map(|c| c.last_line_sha).unwrap_or_default();

    let mut unparsed = 0usize;
    for line in buffer.split_inclusive(|&byte| byte == b'\n') {
        if !line.ends_with(b"\n") {
            // Incomplete trailing line: leave it for the next poll.
            break;
        }
        let line_start = start + consumed;
        consumed += line.len() as u64;
        let text = &line[..line.len() - 1];
        // Recorded for *every* complete line, blank ones included: the cursor's
        // integrity check re-reads exactly `[last_line_start, offset)`, so a
        // skipped blank line would make that range span two lines and fail to
        // verify on the next poll, rescanning the file forever.
        last_line_start = line_start;
        last_line_sha = protocol::hash::sha256_hex(text);
        if text.is_empty() {
            continue;
        }
        match serde_json::from_slice::<serde_json::Value>(text) {
            // Decoded. Either it maps to a fact or it is an entry shape this
            // build does not know about, and the second is an ordinary skip —
            // the transcript format grows, and an unknown entry is not damage.
            Ok(value) => {
                if let Some(event) = transcript_event(session, &value, &last_line_sha) {
                    events.push(event);
                }
            }
            // A line the transcript format promises is JSON and which is not.
            // It used to be dropped in silence *after the cursor advanced past
            // it*, so the bytes were gone and nothing anywhere recorded that
            // anything had been lost. The log's rule is that it never claims
            // what it does not know, and a silent discard is the log claiming
            // there was nothing there.
            Err(_) => {
                unparsed += 1;
                if unparsed <= MAX_PARSE_ERRORS_PER_SCAN {
                    events.push(parse_error_event(session, text, &last_line_sha, line_start));
                }
            }
        }
    }
    if unparsed > MAX_PARSE_ERRORS_PER_SCAN {
        crate::log_warn!(
            "tail {}: {unparsed} unreadable transcript line(s) in one scan; only the first \
             {MAX_PARSE_ERRORS_PER_SCAN} were logged as facts",
            session.name
        );
    }

    if consumed == 0 {
        // Nothing in this window ended with a newline.
        //
        // Two very different situations produce that, and treating them the
        // same is what made a single oversized line stall a session's tail
        // *forever*: `consumed == 0` returned early, the cursor never moved,
        // and every subsequent poll read the same bytes and made the same
        // decision. No transcript fact from that session was ever ingested
        // again — silently, because the code path looks exactly like an idle
        // file.
        //
        //   * The window is short of the cap, so this really is the incomplete
        //     tail of a line still being written. Waiting is correct.
        //   * The window is the full cap and the file has more, so there is a
        //     line at least `MAX_BYTES_PER_POLL` long with no terminator in
        //     sight. Waiting is not correct: it is a stall.
        if to_read < MAX_BYTES_PER_POLL {
            return Ok(None);
        }
        return Ok(Some(oversized_line_scan(
            session, path, dev, ino, start, &buffer,
        )));
    }

    // Returned, not saved. Saving here and ingesting afterwards is two writes
    // with a gap between them, and a crash in that gap makes the cursor's claim
    // — "everything up to this byte is in the log" — permanently false: the
    // lines are skipped on every later poll because the cursor says they are
    // done. The caller commits both halves in one transaction.
    Ok(Some(Scan {
        events,
        cursor: TailCursor {
            path: path.to_string_lossy().to_string(),
            dev,
            ino,
            offset: start + consumed,
            last_line_start,
            last_line_sha,
        },
    }))
}

/// Consume one window of a line too long to hold, and say so in the log.
///
/// The cursor advances by exactly the bytes we read, and `last_line_start` /
/// `last_line_sha` describe *that window* rather than a line — which is what
/// keeps [`verify_last_line`] working, since it re-reads exactly
/// `[last_line_start, offset)` and hashes what it finds. So the next poll
/// resumes mid-line, consumes the next window, and progress is guaranteed:
/// a line of any length is consumed in `ceil(length / MAX_BYTES_PER_POLL)`
/// polls instead of never.
///
/// The fact is `EventKind::Error` and carries the head of the window as
/// evidence. A truncated observation the client can see beats a complete one it
/// never receives — and beats a silent gap, which is the only outcome the event
/// log is not allowed to produce.
fn oversized_line_scan(
    session: &SessionKey,
    path: &Path,
    dev: i64,
    ino: i64,
    start: u64,
    window: &[u8],
) -> Scan {
    let window_sha = protocol::hash::sha256_hex(window);
    let head = String::from_utf8_lossy(&window[..EVIDENCE_BYTES.min(window.len())]).into_owned();
    crate::log_error!(
        "tail {}: a transcript line exceeds {} bytes with no newline at offset {start}; \
         consuming it in windows and recording the loss rather than stalling",
        session.name,
        MAX_BYTES_PER_POLL,
    );
    let event = PendingEvent::new(
        session,
        EventKind::Error,
        serde_json::json!({
            "error": "transcript_line_too_long",
            "detail": "a transcript line is longer than the tailer will hold in memory; \
                       this window of it could not be parsed and its content is not in the log",
            "offset": start,
            "window_bytes": window.len(),
            "window_sha256": window_sha,
            "head": head,
        }),
        Source::Daemon,
    )
    // Stable for these exact bytes at this exact offset, so a rescan after a
    // restart deduplicates against the original rather than logging it twice.
    .with_source_event_id(format!("transcript_overlong:{start}:{window_sha}"));

    Scan {
        events: vec![event],
        cursor: TailCursor {
            path: path.to_string_lossy().to_string(),
            dev,
            ino,
            offset: start + window.len() as u64,
            last_line_start: start,
            last_line_sha: window_sha,
        },
    }
}

/// A line the transcript format promises is JSON, and which is not.
fn parse_error_event(
    session: &SessionKey,
    text: &[u8],
    line_sha: &str,
    offset: u64,
) -> PendingEvent {
    let head = String::from_utf8_lossy(&text[..EVIDENCE_BYTES.min(text.len())]).into_owned();
    PendingEvent::new(
        session,
        EventKind::Error,
        serde_json::json!({
            "error": "transcript_line_unreadable",
            "detail": "this transcript line is not decodable JSON; it was consumed and its \
                       content is not in the log",
            "offset": offset,
            "line_bytes": text.len(),
            "line_sha256": line_sha,
            "head": head,
        }),
        Source::Daemon,
    )
    // The line's own content hash, so the same bad line is one fact however
    // many times the file is rescanned.
    .with_source_event_id(format!("transcript_unreadable:{line_sha}"))
}

/// Re-read the last line we claimed to consume and compare its hash.
fn verify_last_line(file: &mut std::fs::File, cursor: &TailCursor) -> Result<bool> {
    if cursor.offset == 0 {
        return Ok(true);
    }
    if cursor.last_line_start >= cursor.offset {
        return Ok(false);
    }
    let length = (cursor.offset - cursor.last_line_start) as usize;
    let mut buffer = vec![0u8; length];
    file.seek(SeekFrom::Start(cursor.last_line_start))?;
    file.read_exact(&mut buffer)?;
    // The stored hash covers the line without its terminator.
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    Ok(protocol::hash::sha256_hex(&buffer) == cursor.last_line_sha)
}

/// Map one transcript line to a fact.
///
/// `uuid` is Claude's own per-entry identity and makes replay idempotent; a line
/// without one falls back to its content hash, which is stable for the same
/// bytes and therefore serves the same purpose.
/// Takes the *decoded* value rather than the bytes, because the caller has to
/// distinguish two things this function used to collapse into one `None`: a
/// line that could not be decoded at all (an anomaly worth a fact) and a line
/// that decoded perfectly into a shape we deliberately do not map (an ordinary
/// skip). Reporting the second as an error would put a fact in the log for
/// every entry the transcript format grows that we have not taught it yet.
fn transcript_event(
    session: &SessionKey,
    value: &serde_json::Value,
    line_sha: &str,
) -> Option<PendingEvent> {
    let object = value.as_object()?;

    let entry_type = object.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let kind = match entry_type {
        "user" => {
            if object.contains_key("toolUseResult") {
                EventKind::ToolResult
            } else {
                EventKind::UserMessage
            }
        }
        "assistant" => EventKind::AgentMessage,
        "" => EventKind::Other("transcript_entry".to_string()),
        other => EventKind::Other(format!("transcript_{other}")),
    };

    let source_event_id = object
        .get("uuid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("sha:{line_sha}"));

    let turn_id = object
        .get("requestId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let item_id = object
        .get("uuid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(
        PendingEvent::new(session, kind, value.clone(), Source::Transcript)
            .with_source_event_id(source_event_id)
            .with_turn_id(turn_id)
            .with_item_id(item_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A run to file the scanned lines under. The tailer never invents one —
    /// it is handed a key by the session registry — so the tests do the same.
    fn session() -> SessionKey {
        SessionKey::new(protocol::uid::new().unwrap(), "cc-1")
    }

    fn temp_paths() -> (std::path::PathBuf, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "ccd-tail-{}-{}-{}",
            std::process::id(),
            n,
            protocol::time::now_unix_ms()
        ));
        (base.with_extension("db"), base.with_extension("jsonl"))
    }

    /// A watcher with nowhere to send nudges, for exercising the bookkeeping.
    fn dir_watcher() -> DirWatcher {
        let (tx, rx) = mpsc::unbounded_channel();
        // Kept alive: a closed receiver would make every send fail, which is
        // not what these assertions are about.
        Box::leak(Box::new(rx));
        DirWatcher::new(tx).expect("a watcher must be constructible")
    }

    #[test]
    fn a_directory_is_watched_once_and_released_only_by_its_last_holder() {
        // Claude keeps every project's transcripts in one folder, so several
        // live sessions routinely watch the same path. Unwatching when the
        // first of them ends would silently drop the others back to the 250ms
        // poll — a latency regression with no symptom anyone could trace back
        // to this.
        let dir = std::env::temp_dir().join(format!(
            "ccd-tail-watch-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("a.jsonl");
        let second = dir.join("b.jsonl");

        let mut watcher = dir_watcher();
        watcher.watch_parent_of(first.to_str().unwrap());
        watcher.watch_parent_of(second.to_str().unwrap());
        assert_eq!(
            watcher.watched.get(&dir).copied(),
            Some(2),
            "two sessions, one directory, one watch"
        );

        watcher.release_parent_of(first.to_str().unwrap());
        assert_eq!(
            watcher.watched.get(&dir).copied(),
            Some(1),
            "the surviving session must keep its watch"
        );
        watcher.release_parent_of(second.to_str().unwrap());
        assert!(
            !watcher.watched.contains_key(&dir),
            "the last holder releasing must unwatch, or a long-lived daemon \
             accumulates a kernel watch per session it has ever seen"
        );

        // Releasing something never held, or twice, is a no-op rather than an
        // underflow — a `usize` going below zero here would panic the tailer.
        watcher.release_parent_of(second.to_str().unwrap());
        watcher.release_parent_of("/nonexistent/never/watched.jsonl");
        assert!(watcher.watched.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_watch_that_cannot_be_armed_is_not_recorded_as_held() {
        // The owner's machine: 1,282 attempts to watch working directories
        // that had been deleted, re-armed on every daemon start. A failure must
        // leave no hold behind, or a later release would decrement a
        // directory that was never watched.
        let mut watcher = dir_watcher();
        watcher.watch_parent_of("/nonexistent/codeconnect/deleted/transcript.jsonl");
        assert!(
            watcher.watched.is_empty(),
            "a failed watch must not be recorded as held"
        );
    }

    #[test]
    fn a_starting_tailer_resumes_the_living_and_leaves_the_ended_alone() {
        // The regression. Every session with a transcript path was re-tailed at
        // startup regardless of whether the run had finished, so a machine with
        // a history polled hundreds of files nobody would ever write to again
        // and re-armed a filesystem watch on each of their working directories
        // — usually deleted, so each one failed and logged. On the owner's Mac
        // that was 1,282 warning lines per error log, recurring on every start.
        let row = |uid: &str, lifecycle, transcript: Option<&str>| crate::store::SessionRow {
            session_uid: uid.into(),
            session_id: "cc-1".into(),
            tmux_session: "cc-1".into(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: transcript.map(str::to_string),
            lifecycle,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        use protocol::event::Lifecycle;
        let (tails, final_reads) = tails_to_resume(vec![
            row("live-uid", Lifecycle::Live, Some("/tmp/live.jsonl")),
            row("dead-uid", Lifecycle::Exited, Some("/tmp/dead.jsonl")),
            row("unsure-uid", Lifecycle::Unknown, Some("/tmp/unsure.jsonl")),
            row("spawning-uid", Lifecycle::Spawning, Some("/tmp/new.jsonl")),
            row("nopath-uid", Lifecycle::Live, None),
        ]);

        assert!(tails.contains_key("live-uid"));
        assert!(
            !tails.contains_key("dead-uid"),
            "a run that has ended writes nothing more and must not be followed"
        );
        // `unknown` is emphatically *not* the same as ended: the daemon could
        // not establish what happened, so the transcript may still be growing.
        assert!(tails.contains_key("unsure-uid"));
        assert!(tails.contains_key("spawning-uid"));
        assert!(!tails.contains_key("nopath-uid"));
        assert_eq!(tails.len(), 3);

        // …but the ended run is *read once*, not dropped on the floor. Skipping
        // it outright was a data-loss bug — a run can end with transcript bytes
        // nobody consumed, and nothing else will ever look at that file again.
        // The soak gauntlet caught it; this is the unit test that should have.
        assert_eq!(final_reads.len(), 1);
        assert_eq!(final_reads[0].session.uid, "dead-uid");
        assert_eq!(final_reads[0].path, "/tmp/dead.jsonl");
    }

    #[test]
    fn a_run_that_ended_with_unread_transcript_still_gets_its_last_lines() {
        // The property the split above exists for, asserted against a real
        // file: a session ends, its lifecycle is already terminal by the time a
        // new daemon starts, and the bytes it wrote before dying are still in
        // the transcript with the cursor behind them. Those lines are reachable
        // from exactly one place — the final read — and the event log's promise
        // that a `kill -9` costs nothing depends on it happening.
        let (db_path, transcript) = temp_paths();
        let store = Store::open(&db_path).unwrap();
        let run = session();
        append(
            &transcript,
            &[
                r#"{"type":"user","uuid":"tail-end-1"}"#,
                r#"{"type":"assistant","uuid":"tail-end-2"}"#,
            ],
        );

        let ended = Tail {
            session: run.clone(),
            path: transcript.to_string_lossy().into_owned(),
        };
        // Exactly what the startup path does with a `final_reads` entry.
        let scan = scan_file(&store, &ended.session, &ended.path)
            .unwrap()
            .expect("an ended run with unread bytes must still have a scan to do");
        assert_eq!(scan.events.len(), 2, "both lines must be recovered");
        commit(&store, &run, &scan);
        assert_eq!(store.count_events(&run.uid).unwrap(), 2);

        // And a second start finds nothing left, so the read is idempotent
        // rather than a way to re-ingest a transcript on every restart.
        assert!(scan_file(&store, &run, &ended.path).unwrap().is_none());

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn a_final_read_of_a_deleted_transcript_costs_nothing_and_says_nothing() {
        // The common case on a machine with a history: the working directory is
        // long gone. It has to be silent — 41 ended runs on the owner's Mac,
        // and a warning each would be the log spam this whole change removes,
        // reintroduced from the other side.
        let (db_path, transcript) = temp_paths();
        let store = Store::open(&db_path).unwrap();
        let run = session();
        assert!(!transcript.exists());
        assert!(scan_file(&store, &run, transcript.to_str().unwrap())
            .unwrap()
            .is_none());
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn the_tail_commands_are_distinguishable_and_name_a_run_by_identity() {
        // Both variants carry a `session_uid` and never a tmux name: `cc-1` is
        // whichever run currently holds the number, and stopping the wrong
        // one's tail would leave a live agent's transcript unread.
        let follow = TailCommand::Follow {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            path: "/tmp/t.jsonl".into(),
        };
        let stop = TailCommand::Stop {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
        };
        assert_ne!(follow, stop);
    }

    fn append(path: &Path, lines: &[&str]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    /// Commit a scan the way the tailer does: events and cursor together.
    ///
    /// The tests go through this rather than `append_batch` so they exercise
    /// the *only* path that is allowed to advance a cursor — if a future change
    /// reintroduces a bare `save_cursor` before the ingest, nothing here would
    /// be testing it.
    fn commit(store: &Store, run: &SessionKey, scan: &Scan) -> Vec<protocol::event::Event> {
        store
            .append_batch_with_cursor(&run.uid, &scan.events, &scan.cursor)
            .unwrap()
    }

    /// Decode a line the way the scan loop does, then map it.
    ///
    /// The two steps are separate in production because they answer different
    /// questions — "could this be read at all?" and "is this a shape we map?" —
    /// and only the first is an anomaly worth a fact.
    fn map_line(line: &[u8], sha: &str) -> Option<PendingEvent> {
        let value = serde_json::from_slice::<serde_json::Value>(line).ok()?;
        transcript_event(&session(), &value, sha)
    }

    #[test]
    fn maps_entry_types_to_kinds() {
        let user = map_line(br#"{"type":"user","uuid":"u1"}"#, "x").unwrap();
        assert_eq!(user.kind, EventKind::UserMessage);
        assert_eq!(user.source_event_id.as_deref(), Some("u1"));

        let result = map_line(
            br#"{"type":"user","uuid":"u2","toolUseResult":{"stdout":"hi"}}"#,
            "x",
        )
        .unwrap();
        assert_eq!(result.kind, EventKind::ToolResult);

        let assistant = map_line(br#"{"type":"assistant","uuid":"a1"}"#, "x").unwrap();
        assert_eq!(assistant.kind, EventKind::AgentMessage);

        let unknown = map_line(br#"{"type":"bridge-session"}"#, "abc").unwrap();
        assert_eq!(
            unknown.kind,
            EventKind::Other("transcript_bridge-session".into())
        );
        // No uuid: identity falls back to the content hash.
        assert_eq!(unknown.source_event_id.as_deref(), Some("sha:abc"));
    }

    #[test]
    fn json_in_a_shape_we_do_not_map_is_skipped_not_fatal() {
        // Well-formed JSON that is simply not an entry we know how to turn into
        // a fact. Skipped in silence, deliberately: the transcript format grows,
        // and an unrecognised entry is not damage.
        //
        // This test used to also cover `b"not json at all"`. That case is no
        // longer silent — an undecodable line is now an explicit
        // `transcript_line_unreadable` fact, asserted in
        // `an_unreadable_line_is_reported_rather_than_vanishing` — so it moved
        // rather than being dropped from the suite.
        assert!(map_line(b"[1,2,3]", "x").is_none());
        assert!(map_line(b"\"a string\"", "x").is_none());
        assert!(map_line(b"42", "x").is_none());
        // And an undecodable line never reaches the mapper at all.
        assert!(serde_json::from_slice::<serde_json::Value>(b"not json at all").is_err());
    }

    #[test]
    fn incremental_tail_consumes_each_line_once() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(
            &jsonl,
            &[
                r#"{"type":"user","uuid":"u1"}"#,
                r#"{"type":"assistant","uuid":"a1"}"#,
            ],
        );

        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scan.events.len(), 2);
        commit(&store, &run, &scan);

        // Nothing new yet.
        assert!(scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .is_none());

        append(&jsonl, &[r#"{"type":"user","uuid":"u2"}"#]);
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].source_event_id.as_deref(), Some("u2"));
    }

    #[test]
    fn incomplete_trailing_line_is_not_consumed() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        {
            let mut file = std::fs::File::create(&jsonl).unwrap();
            // Second line has no terminator: a poll landed mid-write.
            write!(
                file,
                "{{\"type\":\"user\",\"uuid\":\"u1\"}}\n{{\"type\":\"user\",\"uu"
            )
            .unwrap();
        }
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scan.events.len(), 1, "partial line must be left alone");
        commit(&store, &run, &scan);

        // Now the writer finishes the line.
        append(&jsonl, &[r#"id":"u2"}"#]);
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].source_event_id.as_deref(), Some("u2"));
    }

    #[test]
    fn a_scan_that_is_never_committed_leaves_the_cursor_where_it_was() {
        // The cursor used to be saved inside `scan_file`, before the
        // events reached the log — so a crash between the two skipped those
        // lines permanently, and no later poll ever revisited the bytes.
        //
        // This is that crash: the scan is produced and then *dropped* on the
        // floor. Nothing was ingested, so nothing may have been marked
        // consumed, and the next poll has to offer the same lines again.
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(
            &jsonl,
            &[
                r#"{"type":"user","uuid":"u1"}"#,
                r#"{"type":"assistant","uuid":"a1"}"#,
            ],
        );

        let lost = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(lost.events.len(), 2);
        drop(lost); // the daemon dies here

        assert!(
            store.load_cursor(&run.uid).unwrap().is_none(),
            "an uncommitted scan must not have moved the cursor"
        );
        let again = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .expect("the same lines must still be offered");
        assert_eq!(
            again
                .events
                .iter()
                .filter_map(|e| e.source_event_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["u1", "a1"],
            "no transcript line may be skipped by a crash"
        );

        // And once it *is* committed, both halves land together.
        let ingested = commit(&store, &run, &again);
        assert_eq!(ingested.len(), 2);
        assert_eq!(
            store.load_cursor(&run.uid).unwrap().unwrap().offset,
            std::fs::metadata(&jsonl).unwrap().len(),
            "the committed cursor must claim exactly the bytes that were ingested"
        );
        assert!(scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn restart_resumes_without_duplicates() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(
            &jsonl,
            &[
                r#"{"type":"user","uuid":"u1"}"#,
                r#"{"type":"assistant","uuid":"a1"}"#,
            ],
        );
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        commit(&store, &run, &scan);
        drop(store);

        // ccd was killed; it comes back and the agent wrote more meanwhile.
        append(&jsonl, &[r#"{"type":"user","uuid":"u2"}"#]);
        let store = Store::open(&db).unwrap();
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        let ingested = commit(&store, &run, &scan);
        assert_eq!(ingested.len(), 1, "only the new line is new");
        assert_eq!(store.max_seq(&run.uid).unwrap(), 3);
    }

    #[test]
    fn rewritten_file_rescans_and_dedup_absorbs_it() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(&jsonl, &[r#"{"type":"user","uuid":"u1"}"#]);
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        commit(&store, &run, &scan);

        // Same length, different bytes at the same offset: identity checks pass
        // but the content hash must not.
        std::fs::write(&jsonl, format!("{}\n", r#"{"type":"user","uuid":"uX"}"#)).unwrap();
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].source_event_id.as_deref(), Some("uX"));
        let ingested = commit(&store, &run, &scan);
        assert_eq!(ingested.len(), 1);
        assert_eq!(store.max_seq(&run.uid).unwrap(), 2);
    }

    #[test]
    fn truncated_file_is_rescanned_from_zero() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(
            &jsonl,
            &[
                r#"{"type":"user","uuid":"u1"}"#,
                r#"{"type":"user","uuid":"u2"}"#,
            ],
        );
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        commit(&store, &run, &scan);

        std::fs::write(&jsonl, format!("{}\n", r#"{"type":"user","uuid":"u1"}"#)).unwrap();
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        // Rescan from 0 re-emits u1, which dedup then drops.
        assert_eq!(scan.events.len(), 1);
        assert_eq!(commit(&store, &run, &scan).len(), 0);
        assert_eq!(store.max_seq(&run.uid).unwrap(), 2);
    }

    #[test]
    fn missing_transcript_is_not_an_error() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        assert!(scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn blank_lines_are_ignored_but_still_advance_the_cursor() {
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(&jsonl, &["", r#"{"type":"user","uuid":"u1"}"#, ""]);
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scan.events.len(), 1);
        commit(&store, &run, &scan);
        assert!(scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_batch_with_no_events_still_moves_the_cursor_past_the_bytes() {
        // Blank lines and lines we do not map to facts consume bytes and produce
        // nothing. Since the cursor now moves only inside the ingest, an ingest
        // that declines to write any event still has to record that those bytes
        // were read — otherwise the tailer re-reads them on every poll forever.
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        // Blank lines and well-formed JSON in a shape this build does not map.
        // `"not json at all"` used to be in this fixture; it moved to
        // `an_unreadable_line_is_reported_rather_than_vanishing`, because a line
        // that cannot be decoded is now a fact rather than a silent discard.
        // The property under test here — bytes that produce no fact still move
        // the cursor — is unchanged, and is what these lines still exercise.
        append(&jsonl, &["", "[1,2,3]", "\"a string\"", "42"]);

        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(scan.events.is_empty(), "none of these are facts");
        assert_eq!(commit(&store, &run, &scan).len(), 0);
        assert!(
            scan_file(&store, &run, jsonl.to_str().unwrap())
                .unwrap()
                .is_none(),
            "the tailer must not re-read bytes it has already consumed"
        );
    }

    #[test]
    fn an_unreadable_line_is_reported_rather_than_vanishing() {
        // A line that is not decodable JSON used to be dropped in silence
        // *after the cursor had advanced past it*: the bytes were gone and
        // nothing anywhere recorded that anything had been lost. A log that
        // silently discards is a log claiming there was nothing there.
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        append(
            &jsonl,
            &[
                r#"{"type":"user","uuid":"u1"}"#,
                "{ this is not json",
                r#"{"type":"assistant","uuid":"a1"}"#,
            ],
        );

        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        let events = commit(&store, &run, &scan);
        assert_eq!(events.len(), 3, "the good lines and the bad one");
        let bad = events
            .iter()
            .find(|event| event.kind == EventKind::Error)
            .expect("the unreadable line must be a fact");
        assert_eq!(bad.payload["error"], "transcript_line_unreadable");
        assert!(bad.payload["head"].as_str().unwrap().contains("not json"));
        // Position and identity, so the operator can find the exact bytes.
        assert!(bad.payload["line_sha256"].as_str().unwrap().len() == 64);
        // The surrounding facts are unaffected: one bad line does not cost the
        // lines around it.
        assert!(events.iter().any(|e| e.kind == EventKind::UserMessage));
        assert!(events.iter().any(|e| e.kind == EventKind::AgentMessage));

        // And rescanning does not log it a second time.
        let events = std::iter::once(())
            .filter_map(|()| scan_file(&store, &run, jsonl.to_str().unwrap()).unwrap())
            .count();
        assert_eq!(events, 0, "the cursor advanced past it");
    }

    #[test]
    fn an_enormous_line_advances_the_cursor_instead_of_stalling_forever() {
        // The stall, exactly: a line with no newline inside the read window
        // left `consumed == 0`, the scan returned `None`, and the cursor never
        // moved. Every later poll read the same bytes and made the same
        // decision, so *no transcript fact from that session was ever ingested
        // again* — silently, because the code path is indistinguishable from an
        // idle file.
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();

        // 5MiB with no newline, then a real line behind it. Larger than
        // MAX_BYTES_PER_POLL, so the first window contains no terminator at all.
        let giant = "x".repeat(5 * 1024 * 1024);
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&jsonl)
                .unwrap();
            writeln!(file, "{giant}").unwrap();
            writeln!(file, r#"{{"type":"user","uuid":"after-the-giant"}}"#).unwrap();
        }

        let first = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .expect("the scan must not return None on an oversized line");
        assert_eq!(
            first.cursor.offset, MAX_BYTES_PER_POLL,
            "the cursor must advance by exactly the window that was read"
        );
        let events = commit(&store, &run, &first);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Error);
        assert_eq!(events[0].payload["error"], "transcript_line_too_long");
        assert_eq!(events[0].payload["offset"], 0);
        assert_eq!(events[0].payload["window_bytes"], MAX_BYTES_PER_POLL);

        // Progress is the property: poll until the file is consumed, and the
        // line *behind* the giant one must arrive. Bounded so a regression is a
        // failure rather than a hang.
        let mut seen_after = false;
        for _ in 0..16 {
            let Some(scan) = scan_file(&store, &run, jsonl.to_str().unwrap()).unwrap() else {
                break;
            };
            for event in commit(&store, &run, &scan) {
                if event.source_event_id.as_deref() == Some("after-the-giant") {
                    seen_after = true;
                }
            }
        }
        assert!(
            seen_after,
            "a session's tail must recover from an oversized line, not stop at it"
        );
        assert!(
            scan_file(&store, &run, jsonl.to_str().unwrap())
                .unwrap()
                .is_none(),
            "and the file must end up fully consumed"
        );
    }

    #[test]
    fn an_incomplete_final_line_is_still_waited_for() {
        // The other side of the same condition, and the reason it cannot simply
        // always advance: a short trailing fragment is a line Claude is *still
        // writing*. Consuming it would lose the fact it is about to become.
        let (db, jsonl) = temp_paths();
        let run = session();
        let store = Store::open(&db).unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&jsonl).unwrap();
            write!(file, r#"{{"type":"user","uuid":"partial"#).unwrap();
        }
        assert!(
            scan_file(&store, &run, jsonl.to_str().unwrap())
                .unwrap()
                .is_none(),
            "a line still being written must not be consumed"
        );

        // Finished, and now it is a fact.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl)
                .unwrap();
            writeln!(file, r#""}}"#).unwrap();
        }
        let scan = scan_file(&store, &run, jsonl.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(commit(&store, &run, &scan).len(), 1);
    }
}
