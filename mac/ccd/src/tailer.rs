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
//! Cursors are keyed by `session_uid`, so `cc claude --resume` in a reused name
//! is a *new* run that reads the transcript into its own log rather than one
//! that finds a cursor claiming the file is already consumed. It costs one
//! backfill; the alternative was a timeline with its first half missing.

use std::collections::{HashMap, HashSet};
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

pub async fn run(daemon: Arc<Daemon>, mut rx: mpsc::UnboundedReceiver<(String, String)>) {
    let mut tails: Tails = HashMap::new();

    // Restart path: everything we already knew is re-tailed from its cursor, so
    // a `kill -9` costs at most the events written while we were dead.
    match daemon.db.list_sessions().await {
        Ok(rows) => {
            for row in rows {
                if let Some(path) = row.transcript_path.clone() {
                    tails.insert(
                        row.session_uid.clone(),
                        Tail {
                            session: row.key(),
                            path,
                        },
                    );
                }
            }
        }
        Err(err) => crate::log_error!("tailer could not list sessions: {err:#}"),
    }
    if !tails.is_empty() {
        crate::log_info!("tailer resuming {} transcript(s)", tails.len());
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
            registration = rx.recv() => {
                match registration {
                    Some((session_uid, path)) => {
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
    watched: HashSet<PathBuf>,
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
                watched: HashSet::new(),
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
        if !self.watched.insert(dir.clone()) {
            return;
        }
        match self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => crate::log_debug!("fsevents watching {}", dir.display()),
            Err(err) => {
                self.watched.remove(&dir);
                crate::log_warn!("fsevents could not watch {}: {err}", dir.display());
            }
        }
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
