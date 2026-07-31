//! SQLite event log — the source of truth.
//!
//! Invariants this module owns:
//!   * `seq` is monotonic and gap-free **per `session_uid`**, assigned here and
//!     nowhere else, under `BEGIN IMMEDIATE` so it holds even if a second writer
//!     exists.
//!   * A fact that arrives twice consumes **one** seq. Dedup is checked inside
//!     the same transaction that allocates, so a restart that re-scans a
//!     transcript cannot inflate the log or renumber it.
//!   * The answers ledger is insert-once *per run*: a duplicate answer returns
//!     the original outcome instead of applying a second decision, and an answer
//!     recorded against one run can never be replayed as another's.
//!
//! Everything is keyed by `session_uid`, never by the tmux name. The name is
//! reused — `cc-1` is whichever session currently holds the number — so keying
//! by it made a new run inherit the dead one's log and continue its `seq`.
//! `session_id` is still stored on every row, as the display name that was true
//! when the fact happened.
//!
//! ## Concurrency: one writer, a pool of readers
//!
//! There used to be a single `Mutex<Connection>` for everything, which made
//! every read wait behind every write on the same lock. That is the opposite of
//! what WAL buys: in WAL mode a reader and the writer genuinely do not block
//! each other *at the SQLite level*, and the one mutex was reintroducing the
//! contention the journal mode had just removed. A 4MB transcript batch —
//! `append_batch_with_cursor` during a cold backfill — held that lock for its
//! entire duration, so every WebSocket replay, every session listing and every
//! revocation check on the machine stopped until it finished.
//!
//! So:
//!
//!   * **One writer connection.** Serialised by a mutex, which is also what
//!     makes the `BEGIN IMMEDIATE` sequence allocation cheap: with a single
//!     in-process writer there is no `SQLITE_BUSY` to wait out, and the
//!     `busy_timeout` is left only for a *second process* (the soak harness,
//!     `sqlite3` at a prompt) touching the same file.
//!   * **A small pool of read-only connections**, taken by whichever is free.
//!     A read never waits for a write, and at most `READERS` reads run at once.
//!
//! `std::sync::MutexGuard` is `!Send`, so the compiler still mechanically
//! prevents holding either across an await.
//!
//! ## Staying off the runtime's worker threads
//!
//! Every method here is synchronous and does real disk I/O, so calling one
//! directly from an `async fn` blocks a Tokio worker. [`crate::db::Db`] is the
//! async handle the daemon uses: it hands each call to `spawn_blocking`, which
//! is where the waiting is allowed to happen. This type stays synchronous
//! because that is what a blocking pool wants to call, and because the tests
//! below — and the sync `cc`-facing paths — have no runtime to defer to.

use std::path::Path;
use std::sync::Mutex;

/// Read-only connections. Small on purpose: the readers are a phone replaying
/// a backlog, the fleet listing, and the revocation check, and each is bounded
/// and short. More connections would buy nothing but page cache.
const READERS: usize = 4;

use anyhow::{Context, Result};
use protocol::event::{Event, EventKind, Lifecycle, PendingEvent, SessionKey, Source};
use protocol::pairing::DeviceSummary;
use protocol::ws::AnswerOutcome;
use rusqlite::{params, Connection, OptionalExtension};

/// Schema generation. Bumped when a migration is added; stored in SQLite's own
/// `user_version` so the check costs nothing and cannot itself be missing.
///
///   * `1` — session uids: `sessions` is keyed by uid, `events`, `answers` and
///     `tail_cursors` follow, and the tmux name becomes an ordinary column.
///   * `2` — durability for things that were only ever in memory:
///     `pending_approvals` (a card survives a restart), `answer_claims` and
///     `text_mutations` (a mutation is claimed durably *before* anything is
///     typed, so a daemon killed mid-injection can say "I do not know" instead
///     of typing again). Additive tables only — nothing existing is rewritten,
///     so a downgrade still reads the log.
const SCHEMA_VERSION: i64 = 2;

pub struct Store {
    /// The only connection that writes. One, so `BEGIN IMMEDIATE` never has to
    /// wait out another in-process writer.
    writer: Mutex<Connection>,
    /// Read-only connections, taken by whichever is free.
    readers: Vec<Mutex<Connection>>,
    /// Round-robin start point, so `read()` does not always probe reader 0 and
    /// serialise everything behind it under light load.
    next_reader: std::sync::atomic::AtomicUsize,
}

/// A session as persisted. Presentation-level fields (`link`, `blocked_on`) live
/// in memory because they describe the daemon's current observation, not a fact
/// about the past.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// The run's identity, minted at spawn. Primary key.
    pub session_uid: String,
    /// The tmux session name. Reused across runs, so never an identity.
    pub session_id: String,
    pub tmux_session: String,
    pub tmux_socket: String,
    pub cwd: String,
    pub claude_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub lifecycle: Lifecycle,
    pub created_at: String,
    pub updated_at: String,
}

impl SessionRow {
    pub fn key(&self) -> SessionKey {
        SessionKey::new(&self.session_uid, &self.session_id)
    }
}

#[derive(Debug, Clone)]
pub struct TailCursor {
    pub path: String,
    pub dev: i64,
    pub ino: i64,
    pub offset: u64,
    pub last_line_start: u64,
    pub last_line_sha: String,
}

/// A paired device as stored. The token hash never leaves this module, so no
/// caller can accidentally log or return it.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub device_id: String,
    pub name: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub revoked_at: Option<String>,
    pub ssh_key_installed: bool,
    pub ssh_fingerprint: Option<String>,
}

impl DeviceRow {
    pub fn to_summary(&self) -> DeviceSummary {
        DeviceSummary {
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            created_at: self.created_at.clone(),
            last_seen_at: self.last_seen_at.clone(),
            revoked_at: self.revoked_at.clone(),
            ssh_key_installed: self.ssh_key_installed,
            ssh_fingerprint: self.ssh_fingerprint.clone(),
        }
    }
}

/// Why a pairing attempt failed, in the daemon's own words.
///
/// The distinctions exist for the *log*, not for the peer: every failure is
/// reported over the wire as the same opaque refusal, because telling an
/// unauthenticated caller "that code existed but expired" is a free oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingConsume {
    Consumed { allow_ssh: bool },
    NotFound,
    Expired,
    AlreadyUsed,
}

/// Resolving a user-typed device reference. Ambiguity is an outcome, not an
/// arbitrary pick: revoking the wrong device is not a recoverable mistake.
#[derive(Debug, Clone)]
pub enum DeviceLookup {
    Found(Box<DeviceRow>),
    NotFound,
    Ambiguous(Vec<String>),
}

/// Outcome of writing to the answers ledger.
#[derive(Debug, Clone)]
pub enum LedgerWrite {
    /// This answer won the race and is now the record.
    Recorded,
    /// Already resolved; `outcome` is the original and must be returned as-is.
    Existing {
        outcome: AnswerOutcome,
        payload_hash: String,
    },
}

/// An approval card as persisted, so a restart does not answer "unknown" to a
/// question a human is still looking at.
///
/// The card itself is stored verbatim rather than rebuilt from the event log:
/// the log has the fact, but rebuilding a projection from it at startup would
/// be a second implementation of card-building that has to stay in step with
/// the first one forever.
#[derive(Debug, Clone)]
pub struct PendingApprovalRow {
    pub session_uid: String,
    pub session_id: String,
    pub request_id: String,
    /// Serialised [`protocol::ws::ApprovalCard`].
    pub card: String,
    pub generation: u64,
    pub created_ms: i64,
}

/// A durable claim on an answer, written **before** anything is typed.
///
/// It exists for one question a restart would otherwise be unable to answer:
/// "did the keystroke land?". A claim with no matching ledger row means the
/// daemon died between the two, and the honest recovery is to say so — never to
/// type again, because a second injection into a live TTY cannot be undone
/// while an unanswered prompt is still sitting in front of a human.
#[derive(Debug, Clone)]
pub struct AnswerClaim {
    pub session_uid: String,
    pub session_id: String,
    pub request_id: String,
    pub payload_hash: String,
    /// Serialised [`protocol::ws::AnswerDecision`].
    pub decision: String,
    pub started_at: String,
}

/// What claiming a `send_text` mutation found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextClaim {
    /// Nothing under this id: the caller now owns it and may type.
    Claimed,
    /// Already applied. Replay the original outcome; type nothing.
    Applied { matched: String, settled_at: String },
    /// A claim nobody settled. Never retried automatically.
    Indeterminate { started_at: String },
    /// The same request id carrying different material. Refused rather than
    /// conflated: an id is a retry key, not a licence to type something else.
    Conflict,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            protocol::fsperm::private_dir(parent)
                .with_context(|| format!("creating {} as 0700", parent.display()))?;
        }
        // Owner-only *before* SQLite ever sees the path, for two reasons. The
        // obvious one: `Connection::open` creates the file under the umask, so
        // chmod'ing afterwards leaves a window in which the event log — a
        // verbatim copy of every transcript line an agent produced — is
        // world-readable. The load-bearing one: SQLite copies the main
        // database's mode onto the `-wal` and `-shm` files it creates
        // (`findCreateFileMode` in os_unix.c), and the `journal_mode=WAL`
        // pragma below is what creates them. Getting there first is what makes
        // the whole set owner-only rather than only the file we can name.
        protocol::fsperm::touch_private(path)
            .with_context(|| format!("securing {} as 0600", path.display()))?;
        let writer = open_connection(path)?;
        // A sidecar left behind by a daemon that predates the boundary keeps
        // whatever mode the umask gave it, and it holds committed pages that
        // have not been checkpointed yet — the newest facts in the log.
        harden_sidecars(path);
        // Opened *after* the writer, so the WAL exists and every reader attaches
        // to it rather than to a rollback journal.
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            readers.push(Mutex::new(open_connection(path)?));
        }
        let store = Store {
            writer: Mutex::new(writer),
            readers,
            next_reader: std::sync::atomic::AtomicUsize::new(0),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.write();
        // The rebuild has to happen before `create_schema`: `CREATE TABLE IF NOT
        // EXISTS` is a no-op against a legacy table, so it would leave the old
        // shape in place and every later statement would fail on a missing
        // column.
        if needs_session_uid_migration(&conn)? {
            migrate_to_session_uids(&mut conn)?;
        }
        create_schema(&conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// The writer. Every mutation goes through here and nowhere else.
    ///
    /// A poisoned mutex means a previous holder panicked mid-statement. SQLite
    /// state is still consistent (the transaction rolled back), so recovering is
    /// strictly better than taking the daemon down.
    fn write(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A free reader, or a wait on the least-recently-probed one.
    ///
    /// `try_lock` around the ring first so an idle reader is found without
    /// blocking; only when every one is busy does this wait, and then on a
    /// deterministic choice rather than on whichever the loop happened to end
    /// at. Never returns the writer: a read that took the writer's lock would
    /// reintroduce exactly the convoy this split exists to remove.
    fn read(&self) -> std::sync::MutexGuard<'_, Connection> {
        let start = self
            .next_reader
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for offset in 0..self.readers.len() {
            let index = (start + offset) % self.readers.len();
            match self.readers[index].try_lock() {
                Ok(guard) => return guard,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => continue,
            }
        }
        let index = start % self.readers.len();
        self.readers[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Append one fact. Returns `None` when it was a duplicate.
    pub fn append_event(&self, pending: &PendingEvent) -> Result<Option<Event>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let event = append_in_tx(&tx, pending)?;
        tx.commit()?;
        Ok(event)
    }

    /// Append many facts in one transaction. Duplicates are skipped silently.
    ///
    /// Deliberately *not* used by the transcript tailer, which must advance its
    /// cursor in the same transaction — see
    /// [`Store::append_batch_with_cursor`]. This is the plain form, for callers
    /// with no cursor to keep honest (the fixture replay harness).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn append_batch(&self, pendings: &[PendingEvent]) -> Result<Vec<Event>> {
        if pendings.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut out = Vec::with_capacity(pendings.len());
        for pending in pendings {
            if let Some(event) = append_in_tx(&tx, pending)? {
                out.push(event);
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// Append a transcript batch **and** advance its cursor, atomically.
    ///
    /// The cursor is a claim: "everything up to byte N of this file is in the
    /// log". Saving it in a separate transaction from the events makes that
    /// claim briefly false, and a crash inside that window makes it *permanently*
    /// false — the lines are skipped on every subsequent poll, and no rescan
    /// ever revisits them because the cursor says they were consumed. One
    /// `BEGIN IMMEDIATE` is what turns the claim back into a fact: either both
    /// halves land or neither does, and neither leaves the log with a hole.
    ///
    /// An empty batch still advances the cursor. Blank lines and lines that are
    /// not facts (malformed JSON, entries we do not map) legitimately produce no
    /// events, and refusing to move past them would re-read the same bytes
    /// forever.
    pub fn append_batch_with_cursor(
        &self,
        session_uid: &str,
        pendings: &[PendingEvent],
        cursor: &TailCursor,
    ) -> Result<Vec<Event>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut out = Vec::with_capacity(pendings.len());
        for pending in pendings {
            if let Some(event) = append_in_tx(&tx, pending)? {
                out.push(event);
            }
        }
        save_cursor_in_tx(&tx, session_uid, cursor)?;
        tx.commit()?;
        Ok(out)
    }

    /// Delete one event, leaving `MAX(seq) != COUNT(*)`.
    ///
    /// Test-only, and it exists because the integrity check has no other way to
    /// be exercised: nothing in the daemon can produce a hole, which is exactly
    /// the property being asserted — so the hole has to be manufactured.
    #[cfg(test)]
    pub fn punch_hole_for_tests(&self, session_uid: &str, seq: u64) {
        let conn = self.write();
        conn.execute(
            "DELETE FROM events WHERE session_uid = ?1 AND seq = ?2",
            params![session_uid, seq as i64],
        )
        .expect("test fixture");
    }

    /// Make every device query fail.
    ///
    /// Test-only, and it exists for the same reason `punch_hole_for_tests` does:
    /// the behaviour under test is *which way an unanswerable authorisation
    /// question resolves*, and nothing the daemon does can produce a failing
    /// lookup on demand. Dropping the table is the smallest faithful stand-in
    /// for the class — a corrupt page, a missing file, an I/O error — because
    /// every one of them reaches the caller as the same `Err`.
    #[cfg(test)]
    pub fn break_device_lookups_for_tests(&self) {
        let conn = self.write();
        conn.execute_batch("DROP TABLE devices")
            .expect("test fixture");
    }

    pub fn max_seq(&self, session_uid: &str) -> Result<u64> {
        let conn = self.read();
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_uid = ?1",
            params![session_uid],
            |row| row.get(0),
        )?;
        Ok(seq as u64)
    }

    /// How many events of one kind this run has logged.
    ///
    /// Used to derive the prompt generation. Counting logged facts rather than
    /// keeping a counter is what makes the generation idempotent for free: a
    /// replayed hook produces no new event, so it produces no new generation,
    /// and there is no in-memory number to roll back when a duplicate is
    /// discovered — or to lose across a restart.
    pub fn count_events_of_kind(&self, session_uid: &str, kind: &EventKind) -> Result<u64> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_uid = ?1 AND kind = ?2",
            params![session_uid, kind.as_str()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// How many events this run actually has.
    ///
    /// Exists to be compared against [`Store::max_seq`]: the log's central
    /// promise is that they are equal, and a soak run that finds them apart has
    /// found either a lost event or a burnt seq.
    pub fn count_events(&self, session_uid: &str) -> Result<u64> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_uid = ?1",
            params![session_uid],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn events_after(
        &self,
        session_uid: &str,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<Event>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT seq, session_id, ts, kind, payload, source, source_event_id, turn_id, item_id
               FROM events
              WHERE session_uid = ?1 AND seq > ?2
              ORDER BY seq ASC
              LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![session_uid, after_seq as i64, limit as i64],
            |row| {
                let payload: String = row.get(4)?;
                let source: String = row.get(5)?;
                Ok(Event {
                    seq: row.get::<_, i64>(0)? as u64,
                    session_uid: session_uid.to_string(),
                    session_id: row.get(1)?,
                    ts: row.get(2)?,
                    kind: EventKind::from_str_lossy(&row.get::<_, String>(3)?),
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                    source: parse_source(&source),
                    source_event_id: row.get(6)?,
                    turn_id: row.get(7)?,
                    item_id: row.get(8)?,
                })
            },
        )?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    pub fn upsert_session(&self, row: &SessionRow) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                  claude_session_id, transcript_path, lifecycle,
                                  created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_uid) DO UPDATE SET
                session_id        = excluded.session_id,
                tmux_session      = excluded.tmux_session,
                tmux_socket       = excluded.tmux_socket,
                cwd               = excluded.cwd,
                -- COALESCE keeps a known value when a later update has none:
                -- SessionStart learns the transcript path, a heartbeat does not.
                claude_session_id = COALESCE(excluded.claude_session_id, sessions.claude_session_id),
                transcript_path   = COALESCE(excluded.transcript_path, sessions.transcript_path),
                lifecycle         = excluded.lifecycle,
                updated_at        = excluded.updated_at",
            params![
                row.session_uid,
                row.session_id,
                row.tmux_session,
                row.tmux_socket,
                row.cwd,
                row.claude_session_id,
                row.transcript_path,
                lifecycle_str(row.lifecycle),
                row.created_at,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_uid: &str) -> Result<Option<SessionRow>> {
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                        claude_session_id, transcript_path, lifecycle, created_at, updated_at
                   FROM sessions WHERE session_uid = ?1",
                params![session_uid],
                session_row_from,
            )
            .optional()?;
        Ok(row)
    }

    /// Resolve whatever a caller named: a `session_uid` (exact) or a tmux name.
    ///
    /// A name no longer identifies one run, so it has to be resolved by policy.
    /// The policy is exactly one rule — **the newest run under that name** —
    /// and `lifecycle` deliberately does not enter into it. `lifecycle` is not
    /// reliable enough to rank on: a session that ended while the daemon was
    /// down was never observed exiting, so it stays `Live` forever, and
    /// preferring "not exited" would rank that ghost above the run that
    /// genuinely happened last. Which run is *there* is a question about
    /// supervisors, and the daemon answers it (see `Daemon::resolve`).
    ///
    /// `session_uid DESC` is the tie-break because a ULID sorts by mint time,
    /// so it is the same ordering as `created_at` without depending on two
    /// clocks agreeing.
    ///
    /// Ambiguity is resolved rather than reported on purpose: a phone on
    /// protocol minor 1 can only ever say `cc-1`, and refusing it would break
    /// every client that has not been rebuilt.
    pub fn find_session(&self, reference: &str) -> Result<Option<SessionRow>> {
        if reference.is_empty() {
            return Ok(None);
        }
        if protocol::uid::is_well_formed(reference) {
            if let Some(row) = self.get_session(reference)? {
                return Ok(Some(row));
            }
        }
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                        claude_session_id, transcript_path, lifecycle, created_at, updated_at
                   FROM sessions
                  WHERE session_id = ?1
                  ORDER BY created_at DESC, session_uid DESC
                  LIMIT 1",
                params![reference],
                session_row_from,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                    claude_session_id, transcript_path, lifecycle, created_at, updated_at
               FROM sessions ORDER BY created_at ASC, session_uid ASC",
        )?;
        let rows = stmt.query_map([], session_row_from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_lifecycle(&self, session_uid: &str, lifecycle: Lifecycle) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE sessions SET lifecycle = ?2, updated_at = ?3 WHERE session_uid = ?1",
            params![
                session_uid,
                lifecycle_str(lifecycle),
                protocol::time::now_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Insert-once ledger write. Returns the original outcome if one exists.
    ///
    /// The read and the insert run inside a single immediate transaction, so two
    /// answers racing from two phones cannot both observe "not yet answered".
    ///
    /// Keyed by `(session_uid, request_id)`: a request id is unique within a run
    /// but nothing guarantees it across runs, and an answer given in a dead
    /// `cc-1` must not come back as the answer to a card the live `cc-1` is
    /// showing.
    pub fn record_answer(
        &self,
        session_uid: &str,
        request_id: &str,
        payload_hash: &str,
        outcome: &AnswerOutcome,
    ) -> Result<LedgerWrite> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT payload_hash, outcome FROM answers
                  WHERE session_uid = ?1 AND request_id = ?2",
                params![session_uid, request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((hash, encoded)) = existing {
            tx.commit()?;
            let outcome = serde_json::from_str(&encoded)
                .with_context(|| format!("decoding stored outcome for {request_id}"))?;
            return Ok(LedgerWrite::Existing {
                outcome,
                payload_hash: hash,
            });
        }
        tx.execute(
            "INSERT INTO answers(session_uid, request_id, payload_hash, outcome, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                session_uid,
                request_id,
                payload_hash,
                serde_json::to_string(outcome)?,
                protocol::time::now_rfc3339(),
            ],
        )?;
        // The claim and the outcome that settles it are written together. Any
        // other order leaves a window where the request looks both claimed and
        // answered (harmless) or neither (a request the recovery path would
        // never notice was in flight).
        tx.execute(
            "DELETE FROM answer_claims WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id],
        )?;
        tx.commit()?;
        Ok(LedgerWrite::Recorded)
    }

    pub fn get_answer(
        &self,
        session_uid: &str,
        request_id: &str,
    ) -> Result<Option<(String, AnswerOutcome)>> {
        let conn = self.read();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT payload_hash, outcome FROM answers
                  WHERE session_uid = ?1 AND request_id = ?2",
                params![session_uid, request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            Some((hash, encoded)) => Ok(Some((hash, serde_json::from_str(&encoded)?))),
            None => Ok(None),
        }
    }

    /// Find an answer when the caller could not say which run it belongs to.
    ///
    /// A client on protocol minor 1 sends `answer{request_id}` with no session,
    /// and a request id it is retrying may belong to a run that has since ended
    /// — so there is no in-memory entry left to learn the uid from. Returning
    /// the most recent match is the only honest reading of "the card I tapped":
    /// it is the one the phone was most plausibly showing, and it is
    /// deterministic rather than whichever row SQLite happened to visit first.
    pub fn find_answer_by_request(
        &self,
        request_id: &str,
    ) -> Result<Option<(String, String, AnswerOutcome)>> {
        let conn = self.read();
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT session_uid, payload_hash, outcome FROM answers
                  WHERE request_id = ?1
                  ORDER BY created_at DESC, session_uid DESC
                  LIMIT 1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            Some((uid, hash, encoded)) => Ok(Some((uid, hash, serde_json::from_str(&encoded)?))),
            None => Ok(None),
        }
    }

    // --------------------------------------------------------------- pairing

    /// Record a freshly minted code. Expired codes are swept in the same
    /// statement batch so the table cannot grow without bound on a machine
    /// where the operator repeatedly runs `cc pair` and never scans.
    pub fn create_pairing_code(
        &self,
        code_hash: &str,
        allow_ssh: bool,
        expires_at: &str,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "DELETE FROM pairing_codes WHERE expires_at_ms < ?1",
            params![now_ms],
        )?;
        conn.execute(
            "INSERT INTO pairing_codes(code_hash, allow_ssh, created_at, expires_at,
                                       expires_at_ms, consumed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL)",
            params![
                code_hash,
                allow_ssh as i64,
                protocol::time::rfc3339_from_unix_ms(now_ms),
                expires_at,
                expires_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Redeem a code exactly once.
    ///
    /// The read and the mark-consumed share one immediate transaction, so two
    /// phones scanning the same screen at the same moment cannot both pair:
    /// single-use is enforced by the database, not by the order in which two
    /// tasks happen to be scheduled.
    pub fn consume_pairing_code(&self, code_hash: &str, now_ms: i64) -> Result<PairingConsume> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(i64, i64, Option<String>)> = tx
            .query_row(
                "SELECT allow_ssh, expires_at_ms, consumed_at
                   FROM pairing_codes WHERE code_hash = ?1",
                params![code_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let outcome = match row {
            None => PairingConsume::NotFound,
            Some((_, _, Some(_))) => PairingConsume::AlreadyUsed,
            Some((_, expires_at_ms, None)) if expires_at_ms < now_ms => PairingConsume::Expired,
            Some((allow_ssh, _, None)) => {
                let changed = tx.execute(
                    "UPDATE pairing_codes SET consumed_at = ?2
                      WHERE code_hash = ?1 AND consumed_at IS NULL",
                    params![code_hash, protocol::time::rfc3339_from_unix_ms(now_ms)],
                )?;
                if changed == 1 {
                    PairingConsume::Consumed {
                        allow_ssh: allow_ssh != 0,
                    }
                } else {
                    PairingConsume::AlreadyUsed
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    // --------------------------------------------------------------- devices

    /// Pick a free name close to what the phone asked for.
    ///
    /// Names are how a human revokes a device, so they must be unique — but a
    /// collision is not worth failing a pairing over. Two iPhones become
    /// "iPhone" and "iPhone-2".
    pub fn unique_device_name(&self, requested: &str) -> Result<String> {
        let base = {
            let trimmed = requested.trim();
            if trimmed.is_empty() {
                "device"
            } else {
                trimmed
            }
        };
        let conn = self.read();
        for suffix in 1..=9999u32 {
            let candidate = if suffix == 1 {
                base.to_string()
            } else {
                format!("{base}-{suffix}")
            };
            let taken: i64 = conn.query_row(
                "SELECT COUNT(*) FROM devices WHERE name = ?1 COLLATE NOCASE",
                params![candidate],
                |row| row.get(0),
            )?;
            if taken == 0 {
                return Ok(candidate);
            }
        }
        anyhow::bail!("no free device name based on {base:?}")
    }

    pub fn insert_device(
        &self,
        device_id: &str,
        name: &str,
        token_hash: &str,
        created_at: &str,
    ) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "INSERT INTO devices(device_id, name, token_hash, created_at,
                                 last_seen_at, revoked_at, ssh_key_installed, ssh_fingerprint)
             VALUES(?1, ?2, ?3, ?4, NULL, NULL, 0, NULL)",
            params![device_id, name, token_hash, created_at],
        )?;
        Ok(())
    }

    /// Authenticate a presented token. Revoked devices are excluded in SQL, so
    /// there is no path where a caller forgets to check.
    pub fn device_by_token_hash(&self, token_hash: &str) -> Result<Option<DeviceRow>> {
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT device_id, name, created_at, last_seen_at, revoked_at,
                        ssh_key_installed, ssh_fingerprint
                   FROM devices WHERE token_hash = ?1 AND revoked_at IS NULL",
                params![token_hash],
                device_row_from,
            )
            .optional()?;
        Ok(row)
    }

    /// Is this device still allowed to act?
    ///
    /// Separate from [`Store::device_by_token_hash`] because it is asked on a
    /// live connection that authenticated some time ago: revocation has to take
    /// effect on sockets that are *already open*, not only on the next hello.
    pub fn device_is_active(&self, device_id: &str) -> Result<bool> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM devices WHERE device_id = ?1 AND revoked_at IS NULL",
            params![device_id],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    }

    pub fn touch_device(&self, device_id: &str, at: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE devices SET last_seen_at = ?2 WHERE device_id = ?1",
            params![device_id, at],
        )?;
        Ok(())
    }

    pub fn list_devices(&self) -> Result<Vec<DeviceRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT device_id, name, created_at, last_seen_at, revoked_at,
                    ssh_key_installed, ssh_fingerprint
               FROM devices ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], device_row_from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Resolve what the operator typed: an exact device id, an exact name
    /// (case-insensitively — names come from a phone's settings, not from us),
    /// or an unambiguous device-id prefix.
    pub fn find_device(&self, needle: &str) -> Result<DeviceLookup> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Ok(DeviceLookup::NotFound);
        }
        let devices = self.list_devices()?;

        if let Some(row) = devices.iter().find(|d| d.device_id == needle) {
            return Ok(DeviceLookup::Found(Box::new(row.clone())));
        }

        let by_name: Vec<&DeviceRow> = devices
            .iter()
            .filter(|d| d.name.eq_ignore_ascii_case(needle))
            .collect();
        match by_name.len() {
            1 => return Ok(DeviceLookup::Found(Box::new(by_name[0].clone()))),
            n if n > 1 => {
                return Ok(DeviceLookup::Ambiguous(
                    by_name.iter().map(|d| d.device_id.clone()).collect(),
                ))
            }
            _ => {}
        }

        // A prefix shorter than this is a typo, not an abbreviation.
        if needle.len() >= 4 {
            let by_prefix: Vec<&DeviceRow> = devices
                .iter()
                .filter(|d| d.device_id.starts_with(needle))
                .collect();
            match by_prefix.len() {
                1 => return Ok(DeviceLookup::Found(Box::new(by_prefix[0].clone()))),
                n if n > 1 => {
                    return Ok(DeviceLookup::Ambiguous(
                        by_prefix.iter().map(|d| d.device_id.clone()).collect(),
                    ))
                }
                _ => {}
            }
        }
        Ok(DeviceLookup::NotFound)
    }

    /// Returns false when the device was already revoked, so the caller can
    /// report "nothing to do" instead of pretending it acted.
    pub fn revoke_device(&self, device_id: &str, at: &str) -> Result<bool> {
        let conn = self.write();
        let changed = conn.execute(
            "UPDATE devices SET revoked_at = ?2
              WHERE device_id = ?1 AND revoked_at IS NULL",
            params![device_id, at],
        )?;
        Ok(changed == 1)
    }

    pub fn set_ssh_installed(
        &self,
        device_id: &str,
        installed: bool,
        fingerprint: Option<&str>,
    ) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE devices SET ssh_key_installed = ?2, ssh_fingerprint = ?3
              WHERE device_id = ?1",
            params![device_id, installed as i64, fingerprint],
        )?;
        Ok(())
    }

    // ----------------------------------------------------- pending approvals

    /// Persist a card so a restart can still answer "what is this agent waiting
    /// for?" with the truth rather than with silence.
    pub fn upsert_pending_approval(&self, row: &PendingApprovalRow) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "INSERT INTO pending_approvals(session_uid, session_id, request_id, card,
                                           generation, created_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_uid, request_id) DO UPDATE SET
                session_id = excluded.session_id,
                card       = excluded.card,
                generation = excluded.generation,
                created_ms = excluded.created_ms",
            params![
                row.session_uid,
                row.session_id,
                row.request_id,
                row.card,
                row.generation as i64,
                row.created_ms,
            ],
        )?;
        Ok(())
    }

    pub fn delete_pending_approval(&self, session_uid: &str, request_id: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "DELETE FROM pending_approvals WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id],
        )?;
        Ok(())
    }

    /// Every card that was open when the daemon stopped.
    ///
    /// Rows whose approval has since been answered are excluded in SQL: the
    /// ledger is the terminal record, and a card that outlived its own answer
    /// is not something the recovery path should have to reason about.
    pub fn list_pending_approvals(&self) -> Result<Vec<PendingApprovalRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT p.session_uid, p.session_id, p.request_id, p.card, p.generation, p.created_ms
               FROM pending_approvals p
              WHERE NOT EXISTS (SELECT 1 FROM answers a
                                 WHERE a.session_uid = p.session_uid
                                   AND a.request_id = p.request_id)
              ORDER BY p.created_ms ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingApprovalRow {
                session_uid: row.get(0)?,
                session_id: row.get(1)?,
                request_id: row.get(2)?,
                card: row.get(3)?,
                generation: row.get::<_, i64>(4)? as u64,
                created_ms: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --------------------------------------------------------- answer claims

    /// Claim an answer durably, before a single key is sent.
    ///
    /// `INSERT OR REPLACE`: the in-memory gate already serialises answers for
    /// one request, so the only way a row is here already is a claim this
    /// process did not settle — and [`Store::unresolved_answer_claims`] has
    /// already turned those into terminal indeterminate outcomes at startup.
    pub fn claim_answer(&self, claim: &AnswerClaim) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "INSERT OR REPLACE INTO answer_claims(session_uid, session_id, request_id,
                                                  payload_hash, decision, started_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                claim.session_uid,
                claim.session_id,
                claim.request_id,
                claim.payload_hash,
                claim.decision,
                claim.started_at,
            ],
        )?;
        Ok(())
    }

    /// Drop a claim that never actuated, so the card can be answered again.
    ///
    /// Only ever called when the supervisor *refused* — that is, when we know
    /// nothing was typed. A claim whose fate is unknown is left where it is.
    pub fn release_answer_claim(&self, session_uid: &str, request_id: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "DELETE FROM answer_claims WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id],
        )?;
        Ok(())
    }

    pub fn answer_claim(&self, session_uid: &str, request_id: &str) -> Result<Option<AnswerClaim>> {
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT session_uid, session_id, request_id, payload_hash, decision, started_at
                   FROM answer_claims WHERE session_uid = ?1 AND request_id = ?2",
                params![session_uid, request_id],
                answer_claim_from,
            )
            .optional()?;
        Ok(row)
    }

    /// Claims with no terminal answer: the daemon died mid-injection.
    pub fn unresolved_answer_claims(&self) -> Result<Vec<AnswerClaim>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT c.session_uid, c.session_id, c.request_id, c.payload_hash,
                    c.decision, c.started_at
               FROM answer_claims c
              WHERE NOT EXISTS (SELECT 1 FROM answers a
                                 WHERE a.session_uid = c.session_uid
                                   AND a.request_id = c.request_id)
              ORDER BY c.started_at ASC",
        )?;
        let rows = stmt.query_map([], answer_claim_from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // -------------------------------------------------------- text mutations

    /// Take durable ownership of one `send_text`, or find out who already has.
    ///
    /// Read and insert share one immediate transaction, so two sockets replaying
    /// the same `request_id` at the same instant cannot both come back
    /// [`TextClaim::Claimed`] and both type.
    pub fn claim_text_mutation(
        &self,
        session_uid: &str,
        request_id: &str,
        payload_hash: &str,
        now: &str,
    ) -> Result<TextClaim> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<TextMutationRow> = tx
            .query_row(
                "SELECT payload_hash, status, matched, started_at, settled_at
                   FROM text_mutations WHERE session_uid = ?1 AND request_id = ?2",
                params![session_uid, request_id],
                |row| {
                    Ok(TextMutationRow {
                        payload_hash: row.get(0)?,
                        status: row.get(1)?,
                        matched: row.get(2)?,
                        started_at: row.get(3)?,
                        settled_at: row.get(4)?,
                    })
                },
            )
            .optional()?;

        let claim = match existing {
            None => {
                tx.execute(
                    "INSERT INTO text_mutations(session_uid, request_id, payload_hash,
                                                status, matched, started_at, settled_at)
                     VALUES(?1, ?2, ?3, 'applying', NULL, ?4, NULL)",
                    params![session_uid, request_id, payload_hash, now],
                )?;
                TextClaim::Claimed
            }
            // Same id, different material. Two different mutations, and only one
            // of them can own the id — so neither is typed on a guess.
            Some(row) if row.payload_hash != payload_hash => TextClaim::Conflict,
            Some(row) => match row.status.as_str() {
                "sent" => TextClaim::Applied {
                    matched: row.matched.unwrap_or_default(),
                    settled_at: row.settled_at.unwrap_or(row.started_at),
                },
                _ => TextClaim::Indeterminate {
                    started_at: row.started_at,
                },
            },
        };
        tx.commit()?;
        Ok(claim)
    }

    pub fn settle_text_mutation(
        &self,
        session_uid: &str,
        request_id: &str,
        matched: &str,
        settled_at: &str,
    ) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE text_mutations SET status = 'sent', matched = ?3, settled_at = ?4
              WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id, matched, settled_at],
        )?;
        Ok(())
    }

    /// Drop a claim whose injection was refused, so a later retry is a fresh
    /// attempt rather than a permanent "I do not know".
    pub fn release_text_mutation(&self, session_uid: &str, request_id: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "DELETE FROM text_mutations WHERE session_uid = ?1 AND request_id = ?2
               AND status = 'applying'",
            params![session_uid, request_id],
        )?;
        Ok(())
    }

    /// Turn every claim this process did not settle into a durable "unknown".
    ///
    /// Returns how many were found, so startup can *say* it rather than leaving
    /// the operator to infer it from a quiet log.
    pub fn recover_text_mutations(&self, at: &str) -> Result<usize> {
        let conn = self.write();
        let changed = conn.execute(
            "UPDATE text_mutations SET status = 'indeterminate', settled_at = ?1
              WHERE status = 'applying'",
            params![at],
        )?;
        Ok(changed)
    }

    pub fn load_cursor(&self, session_uid: &str) -> Result<Option<TailCursor>> {
        let conn = self.read();
        let cursor = conn
            .query_row(
                "SELECT path, dev, ino, offset, last_line_start, last_line_sha
                   FROM tail_cursors WHERE session_uid = ?1",
                params![session_uid],
                |row| {
                    Ok(TailCursor {
                        path: row.get(0)?,
                        dev: row.get(1)?,
                        ino: row.get(2)?,
                        offset: row.get::<_, i64>(3)? as u64,
                        last_line_start: row.get::<_, i64>(4)? as u64,
                        last_line_sha: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(cursor)
    }
}

// There is deliberately no public `save_cursor`.
//
// A cursor may only move inside the transaction that appends the events it is
// claiming — see `Store::append_batch_with_cursor`. A standalone save is what
// once let a cursor advance past lines that had not reached the log yet: a
// crash between the two writes skipped those lines permanently, and no later
// poll ever revisited the bytes, because the cursor said they were consumed.
// Removing the entry point makes the invariant structural rather than a rule
// somebody has to remember.

/// Open one connection with the pragmas every connection needs.
///
/// The pragmas are per-*connection*, not per-database: `busy_timeout` in
/// particular is a property of the handle, so a reader opened without it would
/// return `SQLITE_BUSY` immediately where the writer would have waited.
fn open_connection(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("opening sqlite at {}", path.display()))?;
    // WAL keeps readers (WSS replay) from blocking the hook write path, which
    // is the one place added latency is visible to the user.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Only ever waited out for a *second process* on the same file — the soak
    // harness, or `sqlite3` at a prompt. In-process there is one writer and one
    // mutex, so this is not the daemon waiting for itself. It is short because
    // the call now happens on a blocking thread with a deadline above it, not
    // because contention is expected.
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(conn)
}

/// Bring the `-wal` and `-shm` companions to `0600`.
///
/// Best-effort and never fatal: a database that opened is a database the daemon
/// can serve, and refusing to start because one chmod failed would trade a
/// readable sidecar for no daemon at all. The failure is reported so it is not
/// silent, which is the whole difference between a degraded boundary and an
/// imagined one.
fn harden_sidecars(path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        if let Err(err) = protocol::fsperm::harden_file(&sidecar) {
            crate::log_error!(
                "could not make {} owner-only ({err}); recent events may be readable by other \
                 accounts on this Mac",
                sidecar.display()
            );
        }
    }
}

fn save_cursor_in_tx(conn: &Connection, session_uid: &str, cursor: &TailCursor) -> Result<()> {
    conn.execute(
        "INSERT INTO tail_cursors(session_uid, path, dev, ino, offset,
                                  last_line_start, last_line_sha, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(session_uid) DO UPDATE SET
            path = excluded.path, dev = excluded.dev, ino = excluded.ino,
            offset = excluded.offset, last_line_start = excluded.last_line_start,
            last_line_sha = excluded.last_line_sha, updated_at = excluded.updated_at",
        params![
            session_uid,
            cursor.path,
            cursor.dev,
            cursor.ino,
            cursor.offset as i64,
            cursor.last_line_start as i64,
            cursor.last_line_sha,
            protocol::time::now_rfc3339(),
        ],
    )?;
    Ok(())
}

/// One `text_mutations` row, as read. A named struct rather than a tuple: five
/// same-shaped columns, three of them optional, is exactly where a positional
/// read starts silently returning the wrong field.
struct TextMutationRow {
    payload_hash: String,
    status: String,
    matched: Option<String>,
    started_at: String,
    settled_at: Option<String>,
}

fn answer_claim_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnswerClaim> {
    Ok(AnswerClaim {
        session_uid: row.get(0)?,
        session_id: row.get(1)?,
        request_id: row.get(2)?,
        payload_hash: row.get(3)?,
        decision: row.get(4)?,
        started_at: row.get(5)?,
    })
}

/// Create anything that is missing. Idempotent, and the whole schema for a
/// database that has never been opened before.
fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions(
            -- The run's identity: a ULID minted by `cc claude` at spawn, or
            -- synthesised by the daemon for a session it adopts. Never reused.
            session_uid       TEXT PRIMARY KEY,
            -- The tmux session name. NOT unique: `cc-1` belongs to whichever
            -- run currently holds the number, and the ones before it keep
            -- their rows.
            session_id        TEXT NOT NULL,
            tmux_session      TEXT NOT NULL,
            tmux_socket       TEXT NOT NULL,
            cwd               TEXT NOT NULL,
            claude_session_id TEXT,
            transcript_path   TEXT,
            lifecycle         TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL
        );

        -- Resolving a legacy `cc-1` to the newest run under that name.
        CREATE INDEX IF NOT EXISTS sessions_name ON sessions(session_id);

        CREATE TABLE IF NOT EXISTS events(
            session_uid     TEXT    NOT NULL,
            -- The display name as it was when the fact happened. Stored rather
            -- than joined: a fact is not allowed to change retroactively
            -- because a session row was rewritten.
            session_id      TEXT    NOT NULL,
            seq             INTEGER NOT NULL,
            ts              TEXT    NOT NULL,
            kind            TEXT    NOT NULL,
            payload         TEXT    NOT NULL,
            source          TEXT    NOT NULL,
            source_event_id TEXT,
            turn_id         TEXT,
            item_id         TEXT,
            PRIMARY KEY(session_uid, seq)
        );

        -- The dedup key. Partial so facts without a natural id (daemon
        -- markers) are never collapsed together.
        CREATE UNIQUE INDEX IF NOT EXISTS events_dedup
            ON events(session_uid, source, source_event_id)
            WHERE source_event_id IS NOT NULL;

        -- Keyed per run, not per request id alone: an approval answered in one
        -- `cc-1` must never be replayed as the answer to a card the *next*
        -- `cc-1` is showing.
        CREATE TABLE IF NOT EXISTS answers(
            session_uid  TEXT NOT NULL,
            request_id   TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            outcome      TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            PRIMARY KEY(session_uid, request_id)
        );

        -- A client on protocol minor 1 sends an answer with no session, so the
        -- request id alone still has to be lookupable.
        CREATE INDEX IF NOT EXISTS answers_request ON answers(request_id);

        -- Cards that are still waiting for a human. A projection, not a fact:
        -- the fact is the `approval_request` event, and this exists so a
        -- restart can answer "what is this agent blocked on?" instead of
        -- answering "unknown" to a question somebody is looking at right now.
        CREATE TABLE IF NOT EXISTS pending_approvals(
            session_uid TEXT    NOT NULL,
            session_id  TEXT    NOT NULL,
            request_id  TEXT    NOT NULL,
            card        TEXT    NOT NULL,
            -- The prompt generation this card belongs to, so a recovered card
            -- can still be told apart from the prompt that replaced it.
            generation  INTEGER NOT NULL,
            created_ms  INTEGER NOT NULL,
            PRIMARY KEY(session_uid, request_id)
        );

        -- Written before anything is typed, deleted in the same transaction as
        -- the terminal answer. A row surviving a restart means exactly one
        -- thing: we do not know whether the keystroke landed.
        CREATE TABLE IF NOT EXISTS answer_claims(
            session_uid  TEXT NOT NULL,
            session_id   TEXT NOT NULL,
            request_id   TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            decision     TEXT NOT NULL,
            started_at   TEXT NOT NULL,
            PRIMARY KEY(session_uid, request_id)
        );

        -- The same two-phase claim for `send_text`, which is a mutation with
        -- exactly the same "did it land?" problem and, until minor 3, no
        -- identity at all to ask the question with.
        CREATE TABLE IF NOT EXISTS text_mutations(
            session_uid  TEXT NOT NULL,
            request_id   TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            -- applying | sent | indeterminate
            status       TEXT NOT NULL,
            matched      TEXT,
            started_at   TEXT NOT NULL,
            settled_at   TEXT,
            PRIMARY KEY(session_uid, request_id)
        );

        CREATE TABLE IF NOT EXISTS tail_cursors(
            session_uid     TEXT PRIMARY KEY,
            path            TEXT    NOT NULL,
            dev             INTEGER NOT NULL,
            ino             INTEGER NOT NULL,
            offset          INTEGER NOT NULL,
            last_line_start INTEGER NOT NULL,
            last_line_sha   TEXT    NOT NULL,
            updated_at      TEXT    NOT NULL
        );

        -- Pairing codes are stored hashed. The code is a bearer capability
        -- for five minutes; a database file or a backup that leaked one in
        -- the clear would hand over that capability, and hashing costs
        -- nothing because the lookup is by exact hash anyway.
        CREATE TABLE IF NOT EXISTS pairing_codes(
            code_hash     TEXT PRIMARY KEY,
            allow_ssh     INTEGER NOT NULL,
            created_at    TEXT    NOT NULL,
            expires_at    TEXT    NOT NULL,
            expires_at_ms INTEGER NOT NULL,
            consumed_at   TEXT
        );

        -- Device tokens are stored hashed for the same reason, and for the
        -- stronger one: these do not expire.
        CREATE TABLE IF NOT EXISTS devices(
            device_id         TEXT PRIMARY KEY,
            name              TEXT NOT NULL,
            token_hash        TEXT NOT NULL UNIQUE,
            created_at        TEXT NOT NULL,
            last_seen_at      TEXT,
            revoked_at        TEXT,
            ssh_key_installed INTEGER NOT NULL DEFAULT 0,
            ssh_fingerprint   TEXT
        );

        CREATE UNIQUE INDEX IF NOT EXISTS devices_name ON devices(name);
        "#,
    )?;
    Ok(())
}

/// Does this database predate session uids?
///
/// Asked of the schema itself rather than of `user_version`, because a database
/// written before versioning existed reports version 0 and so does a brand-new
/// empty file. The presence of a `sessions` table without a `session_uid`
/// column is the unambiguous signal.
fn needs_session_uid_migration(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "sessions")? {
        return Ok(false);
    }
    Ok(!column_exists(conn, "sessions", "session_uid")?)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Give every pre-existing session a synthetic identity and re-key the log.
///
/// Facts recorded before uids existed are still facts, so nothing is discarded:
/// each distinct legacy `session_id` becomes one run, gets one synthesised uid,
/// and keeps its events, its answers and its tail cursor with their `seq`
/// numbering untouched. That the numbering was *shared* by two runs of the same
/// name is not recoverable after the fact — the log does not record where one
/// stopped and the next began — so the honest migration is to treat the history
/// under a name as one run and to guarantee the property only from here on.
///
/// The synthesised uid takes its timestamp from the session's own `created_at`
/// (or its earliest event) so the migrated rows sort in their real order rather
/// than all appearing to have started at migration time.
///
/// All of it runs in one immediate transaction: a `kill -9` in the middle leaves
/// the old schema completely intact and the migration simply runs again.
fn migrate_to_session_uids(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    // Asked again, now that we hold the write lock.
    //
    // The check in `migrate()` is made against a database no lock is held on,
    // and `Store::open` is not process-local: a second `ccd` starting at the
    // same moment can pass that check, block here on the first one's
    // transaction, and arrive with the work already done. Re-asking under the
    // lock turns that into a clean no-op. Without it the copy below runs against
    // the migrated schema and fails on a column that no longer exists — safe,
    // because the whole thing is one transaction, but it presents as a daemon
    // that will not start and an error naming a column nobody wrote.
    if !needs_session_uid_migration(&tx)? {
        crate::log_debug!("another process migrated the event log first; nothing to do");
        return Ok(());
    }

    // Every name that appears anywhere, not only those with a session row: the
    // daemon adopts sessions from hooks, so a name can own events, answers and
    // a cursor while never having been registered by a supervisor.
    let mut names: Vec<String> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT session_id FROM sessions
             UNION SELECT session_id FROM events
             UNION SELECT session_id FROM answers
             UNION SELECT session_id FROM tail_cursors",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            names.push(row.get(0)?);
        }
    }

    tx.execute_batch(
        "CREATE TEMP TABLE _uid_map(session_id TEXT PRIMARY KEY, session_uid TEXT NOT NULL);",
    )?;
    let now = protocol::time::now_unix_ms();
    for name in &names {
        let created: Option<String> = tx
            .query_row(
                "SELECT COALESCE(
                     (SELECT created_at FROM sessions WHERE session_id = ?1),
                     (SELECT MIN(ts) FROM events WHERE session_id = ?1))",
                params![name],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let minted_at = created
            .as_deref()
            .and_then(protocol::time::unix_ms_from_rfc3339)
            .unwrap_or(now);
        let uid = protocol::uid::at(minted_at)
            .with_context(|| format!("minting a session uid for {name}"))?;
        tx.execute(
            "INSERT INTO _uid_map(session_id, session_uid) VALUES(?1, ?2)",
            params![name, uid],
        )?;
    }

    tx.execute_batch(
        r#"
        ALTER TABLE sessions     RENAME TO sessions_v0;
        ALTER TABLE events       RENAME TO events_v0;
        ALTER TABLE answers      RENAME TO answers_v0;
        ALTER TABLE tail_cursors RENAME TO tail_cursors_v0;
        DROP INDEX IF EXISTS events_dedup;
        "#,
    )?;
    create_schema(&tx)?;
    tx.execute_batch(
        r#"
        INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                             claude_session_id, transcript_path, lifecycle,
                             created_at, updated_at)
            SELECT m.session_uid, s.session_id, s.tmux_session, s.tmux_socket, s.cwd,
                   s.claude_session_id, s.transcript_path, s.lifecycle,
                   s.created_at, s.updated_at
              FROM sessions_v0 s JOIN _uid_map m ON m.session_id = s.session_id;
        "#,
    )?;

    // A name can own events and a tail cursor without ever having had a session
    // row — the daemon adopts sessions from hooks, and a registration that never
    // arrived (or a database rebuilt under a running supervisor) leaves exactly
    // that. Dropping the identity would orphan real history behind a uid nothing
    // lists, so the run is given a placeholder row instead; the next hook or
    // supervisor registration fills in what it actually is.
    tx.execute(
        "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                              claude_session_id, transcript_path, lifecycle,
                              created_at, updated_at)
            SELECT m.session_uid, m.session_id, m.session_id, ?1, '',
                   NULL,
                   (SELECT c.path FROM tail_cursors_v0 c WHERE c.session_id = m.session_id),
                   'unknown',
                   COALESCE((SELECT MIN(e.ts) FROM events_v0 e WHERE e.session_id = m.session_id), ?2),
                   COALESCE((SELECT MAX(e.ts) FROM events_v0 e WHERE e.session_id = m.session_id), ?2)
              FROM _uid_map m
             WHERE m.session_id NOT IN (SELECT session_id FROM sessions_v0)",
        params![
            protocol::TMUX_SOCKET_NAME,
            protocol::time::rfc3339_from_unix_ms(now)
        ],
    )?;

    tx.execute_batch(
        r#"
        INSERT INTO events(session_uid, session_id, seq, ts, kind, payload, source,
                           source_event_id, turn_id, item_id)
            SELECT m.session_uid, e.session_id, e.seq, e.ts, e.kind, e.payload, e.source,
                   e.source_event_id, e.turn_id, e.item_id
              FROM events_v0 e JOIN _uid_map m ON m.session_id = e.session_id;

        INSERT INTO answers(session_uid, request_id, payload_hash, outcome, created_at)
            SELECT m.session_uid, a.request_id, a.payload_hash, a.outcome, a.created_at
              FROM answers_v0 a JOIN _uid_map m ON m.session_id = a.session_id;

        INSERT INTO tail_cursors(session_uid, path, dev, ino, offset,
                                 last_line_start, last_line_sha, updated_at)
            SELECT m.session_uid, c.path, c.dev, c.ino, c.offset,
                   c.last_line_start, c.last_line_sha, c.updated_at
              FROM tail_cursors_v0 c JOIN _uid_map m ON m.session_id = c.session_id;

        DROP TABLE sessions_v0;
        DROP TABLE events_v0;
        DROP TABLE answers_v0;
        DROP TABLE tail_cursors_v0;
        DROP TABLE _uid_map;
        "#,
    )?;

    tx.commit()?;
    crate::log_info!(
        "migrated the event log to session uids: {} session(s) given a stable identity",
        names.len()
    );
    Ok(())
}

fn append_in_tx(tx: &rusqlite::Transaction<'_>, pending: &PendingEvent) -> Result<Option<Event>> {
    // A fact with no identity to file it under would land in a shared bucket and
    // recreate the exact collision session uids exist to remove. Every ingest
    // path fills this in; failing loudly here is how we find out if one stops.
    anyhow::ensure!(
        !pending.session_uid.is_empty(),
        "refusing to append an event with no session_uid ({} {:?})",
        pending.session_id,
        pending.kind.as_str()
    );

    if let Some(source_event_id) = &pending.source_event_id {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT seq FROM events
                  WHERE session_uid = ?1 AND source = ?2 AND source_event_id = ?3",
                params![
                    pending.session_uid,
                    pending.source.as_str(),
                    source_event_id
                ],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(None);
        }
    }

    let next: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_uid = ?1",
        params![pending.session_uid],
        |row| row.get(0),
    )?;

    let payload = serde_json::to_string(&pending.payload)?;
    tx.execute(
        "INSERT INTO events(session_uid, session_id, seq, ts, kind, payload, source,
                            source_event_id, turn_id, item_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            pending.session_uid,
            pending.session_id,
            next,
            pending.ts,
            pending.kind.as_str(),
            payload,
            pending.source.as_str(),
            pending.source_event_id,
            pending.turn_id,
            pending.item_id,
        ],
    )?;

    Ok(Some(Event {
        seq: next as u64,
        session_uid: pending.session_uid.clone(),
        session_id: pending.session_id.clone(),
        ts: pending.ts.clone(),
        kind: pending.kind.clone(),
        payload: pending.payload.clone(),
        source: pending.source,
        source_event_id: pending.source_event_id.clone(),
        turn_id: pending.turn_id.clone(),
        item_id: pending.item_id.clone(),
    }))
}

fn session_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_uid: row.get(0)?,
        session_id: row.get(1)?,
        tmux_session: row.get(2)?,
        tmux_socket: row.get(3)?,
        cwd: row.get(4)?,
        claude_session_id: row.get(5)?,
        transcript_path: row.get(6)?,
        lifecycle: parse_lifecycle(&row.get::<_, String>(7)?),
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn device_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        device_id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        last_seen_at: row.get(3)?,
        revoked_at: row.get(4)?,
        ssh_key_installed: row.get::<_, i64>(5)? != 0,
        ssh_fingerprint: row.get(6)?,
    })
}

fn lifecycle_str(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Spawning => "spawning",
        Lifecycle::Live => "live",
        Lifecycle::Exited => "exited",
        Lifecycle::Unknown => "unknown",
    }
}

fn parse_lifecycle(value: &str) -> Lifecycle {
    match value {
        "spawning" => Lifecycle::Spawning,
        "live" => Lifecycle::Live,
        "exited" => Lifecycle::Exited,
        _ => Lifecycle::Unknown,
    }
}

fn parse_source(value: &str) -> Source {
    match value {
        "hook" => Source::Hook,
        "transcript" => Source::Transcript,
        "pty" => Source::Pty,
        _ => Source::Daemon,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ws::{AnswerDecision, AnswerPath, ResolvedBy};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_store() -> (Store, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ccd-test-{}-{}-{}.db",
            std::process::id(),
            n,
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        (Store::open(&path).unwrap(), path)
    }

    /// A uid that reads like one in a failure message. Real ones come from
    /// `protocol::uid`, but a test that hard-codes them is easier to debug.
    fn uid(tag: &str) -> String {
        let mut value = format!("01K1B3XQ8ZC0DE5FGH7JKMNP{tag}");
        value.truncate(protocol::uid::UID_LEN);
        assert!(protocol::uid::is_well_formed(&value), "{value}");
        value
    }

    fn key(tag: &str, name: &str) -> SessionKey {
        SessionKey::new(uid(tag), name)
    }

    fn pending(session: &SessionKey, kind: EventKind, id: Option<&str>) -> PendingEvent {
        let mut event = PendingEvent::new(session, kind, json!({"x": 1}), Source::Hook);
        event.source_event_id = id.map(|s| s.to_string());
        event
    }

    fn session_row(session: &SessionKey) -> SessionRow {
        let now = protocol::time::now_rfc3339();
        SessionRow {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            tmux_session: session.name.clone(),
            tmux_socket: "codeconnect".into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[test]
    fn seq_is_monotonic_and_per_run() {
        let (store, _path) = temp_store();
        let first = key("AA", "cc-1");
        for expected in 1..=3u64 {
            let event = store
                .append_event(&pending(&first, EventKind::ToolCall, None))
                .unwrap()
                .unwrap();
            assert_eq!(event.seq, expected);
        }
        // A second session starts its own numbering.
        let second = key("AB", "cc-2");
        let other = store
            .append_event(&pending(&second, EventKind::ToolCall, None))
            .unwrap()
            .unwrap();
        assert_eq!(other.seq, 1);
        assert_eq!(store.max_seq(&first.uid).unwrap(), 3);
        assert_eq!(store.max_seq(&second.uid).unwrap(), 1);
    }

    #[test]
    fn a_reused_name_does_not_inherit_the_dead_run_s_log() {
        // Name reuse, as a storage property. `cc-1` exits, the number is freed,
        // and the next session is called `cc-1` too — it must start at seq 1
        // with an empty log, not continue somebody else's numbering.
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        let live = key("AB", "cc-1");
        assert_ne!(dead.uid, live.uid);

        for i in 0..5 {
            store
                .append_event(&pending(&dead, EventKind::ToolCall, Some(&format!("d{i}"))))
                .unwrap();
        }
        let first_of_the_new_run = store
            .append_event(&pending(&live, EventKind::SessionStart, Some("n0")))
            .unwrap()
            .unwrap();

        assert_eq!(first_of_the_new_run.seq, 1, "a new run starts at 1");
        assert_eq!(store.max_seq(&dead.uid).unwrap(), 5);
        assert_eq!(store.max_seq(&live.uid).unwrap(), 1);
        assert_eq!(store.events_after(&live.uid, 0, 100).unwrap().len(), 1);
        assert_eq!(store.events_after(&dead.uid, 0, 100).unwrap().len(), 5);

        // And the same natural id in both runs is two facts, not a duplicate:
        // dedup is scoped to the run, so a replayed tool call in the new session
        // is not silently swallowed because the old one saw that id.
        store
            .append_event(&pending(&dead, EventKind::ToolCall, Some("shared")))
            .unwrap()
            .expect("first run records it");
        store
            .append_event(&pending(&live, EventKind::ToolCall, Some("shared")))
            .unwrap()
            .expect("second run records it independently");
    }

    #[test]
    fn a_fact_with_no_identity_is_refused_rather_than_filed_anywhere() {
        let (store, _path) = temp_store();
        let mut orphan = pending(&key("AA", "cc-1"), EventKind::ToolCall, None);
        orphan.session_uid = String::new();
        let err = store.append_event(&orphan).unwrap_err();
        assert!(format!("{err:#}").contains("session_uid"), "{err:#}");
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn a_name_resolves_to_the_newest_run_regardless_of_lifecycle() {
        let (store, _path) = temp_store();
        let old = key("AA", "cc-1");
        let new = key("AB", "cc-1");

        // The older run is a *ghost*: it ended while the daemon was down, so it
        // was never observed exiting and is still recorded live. Ranking on
        // lifecycle would put it above the run that actually happened last,
        // which is the failure this ordering exists to avoid.
        let mut row = session_row(&old);
        row.created_at = "2026-07-01T00:00:00.000Z".into();
        row.lifecycle = Lifecycle::Live;
        store.upsert_session(&row).unwrap();

        let mut row = session_row(&new);
        row.created_at = "2026-07-02T00:00:00.000Z".into();
        row.lifecycle = Lifecycle::Exited;
        store.upsert_session(&row).unwrap();

        assert_eq!(
            store.find_session("cc-1").unwrap().unwrap().session_uid,
            new.uid,
            "the newest run wins, ghost or not"
        );
        // A uid is exact, dead or alive.
        assert_eq!(
            store.find_session(&old.uid).unwrap().unwrap().session_uid,
            old.uid
        );
        assert!(store.find_session("cc-9").unwrap().is_none());
        assert!(store.find_session("").unwrap().is_none());
        // A well-formed uid nobody minted must not fall through to a name match.
        assert!(store.find_session(&uid("ZZ")).unwrap().is_none());
    }

    #[test]
    fn count_and_max_seq_agree_for_a_healthy_log() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        for i in 0..7 {
            store
                .append_event(&pending(
                    &session,
                    EventKind::ToolCall,
                    Some(&format!("t{i}")),
                ))
                .unwrap();
        }
        // A duplicate must move neither, which is what makes the two numbers a
        // usable integrity check rather than two ways of counting inserts.
        store
            .append_event(&pending(&session, EventKind::ToolCall, Some("t3")))
            .unwrap();
        assert_eq!(store.max_seq(&session.uid).unwrap(), 7);
        assert_eq!(store.count_events(&session.uid).unwrap(), 7);
    }

    #[test]
    fn duplicate_source_event_id_consumes_no_seq() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        let first = store
            .append_event(&pending(&session, EventKind::ToolCall, Some("toolu_1")))
            .unwrap();
        assert_eq!(first.unwrap().seq, 1);

        let dup = store
            .append_event(&pending(&session, EventKind::ToolCall, Some("toolu_1")))
            .unwrap();
        assert!(dup.is_none(), "duplicate must be dropped");

        let next = store
            .append_event(&pending(&session, EventKind::ToolCall, Some("toolu_2")))
            .unwrap()
            .unwrap();
        assert_eq!(next.seq, 2, "a dropped duplicate must not burn a seq");
    }

    #[test]
    fn same_id_from_a_different_source_is_a_different_fact() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        let mut hook = pending(&session, EventKind::ToolCall, Some("uuid-1"));
        hook.source = Source::Hook;
        let mut transcript = pending(&session, EventKind::ToolCall, Some("uuid-1"));
        transcript.source = Source::Transcript;
        assert!(store.append_event(&hook).unwrap().is_some());
        assert!(store.append_event(&transcript).unwrap().is_some());
    }

    #[test]
    fn events_without_ids_are_never_collapsed() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        assert!(store
            .append_event(&pending(&session, EventKind::Notification, None))
            .unwrap()
            .is_some());
        assert!(store
            .append_event(&pending(&session, EventKind::Notification, None))
            .unwrap()
            .is_some());
        assert_eq!(store.max_seq(&session.uid).unwrap(), 2);
    }

    #[test]
    fn restart_backfill_does_not_duplicate_or_renumber() {
        let (store, path) = temp_store();
        let session = key("AA", "cc-1");
        let batch: Vec<_> = (0..5)
            .map(|i| pending(&session, EventKind::UserMessage, Some(&format!("u{i}"))))
            .collect();
        assert_eq!(store.append_batch(&batch).unwrap().len(), 5);
        drop(store);

        // Reopen, as ccd would after `kill -9`, and re-scan the same lines.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.append_batch(&batch).unwrap().len(), 0);
        assert_eq!(store.max_seq(&session.uid).unwrap(), 5);

        let seqs: Vec<u64> = store
            .events_after(&session.uid, 0, 100)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5], "replay must stay gap-free");
    }

    #[test]
    fn events_after_replays_only_the_tail() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        for i in 0..5 {
            store
                .append_event(&pending(
                    &session,
                    EventKind::ToolCall,
                    Some(&format!("t{i}")),
                ))
                .unwrap();
        }
        let tail = store.events_after(&session.uid, 3, 100).unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
        // The display name travels with every replayed event, so a client can
        // render it without a second lookup that might now say something else.
        assert!(tail.iter().all(|e| e.session_id == "cc-1"));
        assert!(tail.iter().all(|e| e.session_uid == session.uid));
        assert!(store
            .events_after(&session.uid, 99, 100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unknown_kind_survives_a_database_round_trip() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        let mut event = pending(&session, EventKind::Other("future_kind".into()), None);
        event.payload = json!({"a": [1, 2, 3]});
        store.append_event(&event).unwrap();
        let back = &store.events_after(&session.uid, 0, 10).unwrap()[0];
        assert_eq!(back.kind, EventKind::Other("future_kind".into()));
        assert_eq!(back.payload, json!({"a": [1, 2, 3]}));
    }

    fn outcome(request: &str, decision: AnswerDecision) -> AnswerOutcome {
        AnswerOutcome {
            request_id: request.to_string(),
            session_id: "cc-1".into(),
            decision,
            resolved_by: ResolvedBy::Phone,
            applied_via: AnswerPath::SendKeys,
            resolved_at: protocol::time::now_rfc3339(),
            detail: None,
            inferred: false,
            indeterminate: false,
        }
    }

    #[test]
    fn ledger_is_insert_once_and_returns_the_original() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        let first = outcome("toolu_1", AnswerDecision::Allow);
        assert!(matches!(
            store
                .record_answer(&session.uid, "toolu_1", "hash-a", &first)
                .unwrap(),
            LedgerWrite::Recorded
        ));

        // A second, *different* decision must not overwrite the first.
        let second = outcome("toolu_1", AnswerDecision::Deny);
        match store
            .record_answer(&session.uid, "toolu_1", "hash-a", &second)
            .unwrap()
        {
            LedgerWrite::Existing {
                outcome,
                payload_hash,
            } => {
                assert_eq!(outcome.decision, AnswerDecision::Allow);
                assert_eq!(payload_hash, "hash-a");
            }
            LedgerWrite::Recorded => panic!("duplicate must not be recorded"),
        }
    }

    #[test]
    fn an_answer_in_one_run_is_not_an_answer_in_the_next() {
        // The approval-safety half of the session uid change. Both runs are
        // called `cc-1`; an approval answered in the first must not make the
        // second's card come back as an already-applied duplicate.
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        let live = key("AB", "cc-1");

        store
            .record_answer(
                &dead.uid,
                "toolu_1",
                "h",
                &outcome("toolu_1", AnswerDecision::Allow),
            )
            .unwrap();

        assert!(
            store.get_answer(&live.uid, "toolu_1").unwrap().is_none(),
            "the new run has answered nothing"
        );
        assert!(matches!(
            store
                .record_answer(
                    &live.uid,
                    "toolu_1",
                    "h",
                    &outcome("toolu_1", AnswerDecision::Deny)
                )
                .unwrap(),
            LedgerWrite::Recorded
        ));
        // Both records survive, each under its own run.
        assert_eq!(
            store
                .get_answer(&dead.uid, "toolu_1")
                .unwrap()
                .unwrap()
                .1
                .decision,
            AnswerDecision::Allow
        );
        assert_eq!(
            store
                .get_answer(&live.uid, "toolu_1")
                .unwrap()
                .unwrap()
                .1
                .decision,
            AnswerDecision::Deny
        );
    }

    #[test]
    fn a_request_id_alone_finds_the_most_recent_answer() {
        // What a protocol-minor-1 client's retry resolves to. Deterministic
        // rather than "whichever row SQLite reached first".
        let (store, _path) = temp_store();
        let old = key("AA", "cc-1");
        let new = key("AB", "cc-1");
        store
            .record_answer(
                &old.uid,
                "toolu_1",
                "h",
                &outcome("toolu_1", AnswerDecision::Allow),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        store
            .record_answer(
                &new.uid,
                "toolu_1",
                "h",
                &outcome("toolu_1", AnswerDecision::Deny),
            )
            .unwrap();

        let (uid, _hash, outcome) = store.find_answer_by_request("toolu_1").unwrap().unwrap();
        assert_eq!(uid, new.uid);
        assert_eq!(outcome.decision, AnswerDecision::Deny);
        assert!(store
            .find_answer_by_request("never-asked")
            .unwrap()
            .is_none());
    }

    #[test]
    fn ledger_survives_restart() {
        let (store, path) = temp_store();
        let session = key("AA", "cc-1");
        store
            .record_answer(
                &session.uid,
                "toolu_9",
                "h",
                &outcome("toolu_9", AnswerDecision::Allow),
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let (hash, stored) = store.get_answer(&session.uid, "toolu_9").unwrap().unwrap();
        assert_eq!(hash, "h");
        assert_eq!(stored.decision, AnswerDecision::Allow);
    }

    #[test]
    fn session_upsert_preserves_learned_fields() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        let mut row = session_row(&session);
        row.lifecycle = Lifecycle::Spawning;
        store.upsert_session(&row).unwrap();

        // SessionStart teaches us the transcript path.
        row.transcript_path = Some("/tmp/x.jsonl".into());
        row.claude_session_id = Some("uuid-1".into());
        row.lifecycle = Lifecycle::Live;
        store.upsert_session(&row).unwrap();

        // A later heartbeat knows neither; they must not be erased.
        row.transcript_path = None;
        row.claude_session_id = None;
        store.upsert_session(&row).unwrap();

        let stored = store.get_session(&session.uid).unwrap().unwrap();
        assert_eq!(stored.transcript_path.as_deref(), Some("/tmp/x.jsonl"));
        assert_eq!(stored.claude_session_id.as_deref(), Some("uuid-1"));
        assert_eq!(stored.lifecycle, Lifecycle::Live);
        assert_eq!(stored.session_id, "cc-1");
    }

    #[test]
    fn two_runs_of_one_name_are_two_rows_not_an_overwrite() {
        let (store, _path) = temp_store();
        let first = key("AA", "cc-1");
        let second = key("AB", "cc-1");
        store.upsert_session(&session_row(&first)).unwrap();
        store.upsert_session(&session_row(&second)).unwrap();
        let rows = store.list_sessions().unwrap();
        assert_eq!(rows.len(), 2, "the second run must not replace the first");
        assert!(rows.iter().all(|r| r.session_id == "cc-1"));
    }

    fn mint(store: &Store, code_hash: &str, allow_ssh: bool, ttl_ms: i64) -> i64 {
        let now = protocol::time::now_unix_ms();
        let expires = now + ttl_ms;
        store
            .create_pairing_code(
                code_hash,
                allow_ssh,
                &protocol::time::rfc3339_from_unix_ms(expires),
                expires,
                now,
            )
            .unwrap();
        now
    }

    #[test]
    fn a_pairing_code_works_exactly_once() {
        let (store, _path) = temp_store();
        let now = mint(&store, "hash-a", false, 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-a", now).unwrap(),
            PairingConsume::Consumed { allow_ssh: false }
        );
        // The second scan of the same screen must not pair a second device.
        assert_eq!(
            store.consume_pairing_code("hash-a", now).unwrap(),
            PairingConsume::AlreadyUsed
        );
    }

    #[test]
    fn an_expired_code_is_refused_even_though_it_exists() {
        let (store, _path) = temp_store();
        let now = mint(&store, "hash-b", false, 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-b", now + 300_001).unwrap(),
            PairingConsume::Expired
        );
    }

    #[test]
    fn an_unknown_code_is_not_found() {
        let (store, _path) = temp_store();
        assert_eq!(
            store.consume_pairing_code("never-issued", 0).unwrap(),
            PairingConsume::NotFound
        );
    }

    #[test]
    fn ssh_consent_rides_the_code_not_the_daemon() {
        let (store, _path) = temp_store();
        let now = mint(&store, "hash-ssh", true, 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-ssh", now).unwrap(),
            PairingConsume::Consumed { allow_ssh: true }
        );
        // A second code minted without --ssh must not inherit the consent.
        let now = mint(&store, "hash-plain", false, 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-plain", now).unwrap(),
            PairingConsume::Consumed { allow_ssh: false }
        );
    }

    #[test]
    fn minting_sweeps_codes_that_can_never_be_used_again() {
        let (store, _path) = temp_store();
        let now = protocol::time::now_unix_ms();
        store
            .create_pairing_code("old", false, "t", now - 10_000, now)
            .unwrap();
        // A later mint sweeps anything already past its expiry.
        mint(&store, "fresh", false, 300_000);
        assert_eq!(
            store.consume_pairing_code("old", now).unwrap(),
            PairingConsume::NotFound
        );
    }

    fn add_device(store: &Store, id: &str, name: &str, token_hash: &str) {
        let name = store.unique_device_name(name).unwrap();
        store
            .insert_device(id, &name, token_hash, &protocol::time::now_rfc3339())
            .unwrap();
    }

    #[test]
    fn a_device_token_authenticates_until_it_is_revoked() {
        let (store, _path) = temp_store();
        add_device(&store, "d1a2b3c4", "iPhone", "token-hash-1");
        let found = store.device_by_token_hash("token-hash-1").unwrap().unwrap();
        assert_eq!(found.name, "iPhone");

        assert!(store
            .revoke_device("d1a2b3c4", &protocol::time::now_rfc3339())
            .unwrap());
        assert!(
            store
                .device_by_token_hash("token-hash-1")
                .unwrap()
                .is_none(),
            "a revoked token must not authenticate"
        );
        // Revoking twice is a no-op, not an error.
        assert!(!store
            .revoke_device("d1a2b3c4", &protocol::time::now_rfc3339())
            .unwrap());
    }

    #[test]
    fn an_unknown_token_authenticates_nothing() {
        let (store, _path) = temp_store();
        add_device(&store, "d1", "iPhone", "token-hash-1");
        assert!(store.device_by_token_hash("wrong").unwrap().is_none());
        assert!(store.device_by_token_hash("").unwrap().is_none());
    }

    #[test]
    fn colliding_device_names_are_made_unique_rather_than_rejected() {
        let (store, _path) = temp_store();
        add_device(&store, "d1", "iPhone", "t1");
        add_device(&store, "d2", "iPhone", "t2");
        add_device(&store, "d3", "iPhone", "t3");
        let names: Vec<String> = store
            .list_devices()
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["iPhone", "iPhone-2", "iPhone-3"]);
        assert_eq!(store.unique_device_name("  ").unwrap(), "device");
    }

    #[test]
    fn devices_resolve_by_id_name_or_unambiguous_prefix() {
        let (store, _path) = temp_store();
        add_device(&store, "aaaa1111", "iPhone", "t1");
        add_device(&store, "aaaa2222", "iPad", "t2");

        assert!(matches!(
            store.find_device("aaaa1111").unwrap(),
            DeviceLookup::Found(row) if row.name == "iPhone"
        ));
        assert!(matches!(
            store.find_device("ipad").unwrap(),
            DeviceLookup::Found(row) if row.device_id == "aaaa2222"
        ));
        assert!(matches!(
            store.find_device("aaaa2").unwrap(),
            DeviceLookup::Found(row) if row.device_id == "aaaa2222"
        ));
        // A prefix matching two devices must never pick one at random.
        assert!(matches!(
            store.find_device("aaaa").unwrap(),
            DeviceLookup::Ambiguous(ids) if ids.len() == 2
        ));
        assert!(matches!(
            store.find_device("nope").unwrap(),
            DeviceLookup::NotFound
        ));
        // Too short to be an abbreviation.
        assert!(matches!(
            store.find_device("aa").unwrap(),
            DeviceLookup::NotFound
        ));
    }

    #[test]
    fn revoked_devices_stay_listed_with_their_history() {
        let (store, _path) = temp_store();
        add_device(&store, "d1", "iPhone", "t1");
        store
            .set_ssh_installed("d1", true, Some("SHA256:abc"))
            .unwrap();
        store
            .revoke_device("d1", "2026-07-31T10:00:00.000Z")
            .unwrap();

        let listed = &store.list_devices().unwrap()[0];
        assert_eq!(
            listed.revoked_at.as_deref(),
            Some("2026-07-31T10:00:00.000Z")
        );
        assert!(listed.ssh_key_installed);
        assert_eq!(listed.ssh_fingerprint.as_deref(), Some("SHA256:abc"));
        assert!(!listed.to_summary().is_active());
    }

    #[test]
    fn devices_and_pairing_survive_a_restart() {
        let (store, path) = temp_store();
        add_device(&store, "d1", "iPhone", "t1");
        let now = mint(&store, "hash-r", true, 300_000);
        drop(store);

        let store = Store::open(&path).unwrap();
        assert!(store.device_by_token_hash("t1").unwrap().is_some());
        assert_eq!(
            store.consume_pairing_code("hash-r", now).unwrap(),
            PairingConsume::Consumed { allow_ssh: true }
        );
    }

    #[test]
    fn cursor_round_trip() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        assert!(store.load_cursor(&session.uid).unwrap().is_none());
        let cursor = TailCursor {
            path: "/tmp/x.jsonl".into(),
            dev: 16777232,
            ino: 12345,
            offset: 4096,
            last_line_start: 4000,
            last_line_sha: "abc".into(),
        };
        // Through the production path: a cursor only ever moves alongside the
        // events it claims, so there is nothing else to save it with.
        store
            .append_batch_with_cursor(&session.uid, &[], &cursor)
            .unwrap();
        let back = store.load_cursor(&session.uid).unwrap().unwrap();
        assert_eq!(back.offset, 4096);
        assert_eq!(back.ino, 12345);
        assert_eq!(back.last_line_sha, "abc");
    }

    #[test]
    fn a_new_run_of_an_old_name_starts_with_no_cursor() {
        // `cc claude --resume` in a reused name points a *new* run at the same
        // transcript. It must re-read that transcript into its own log rather
        // than inheriting a cursor that says "already consumed".
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        let live = key("AB", "cc-1");
        store
            .append_batch_with_cursor(
                &dead.uid,
                &[],
                &TailCursor {
                    path: "/tmp/x.jsonl".into(),
                    dev: 1,
                    ino: 2,
                    offset: 900,
                    last_line_start: 800,
                    last_line_sha: "abc".into(),
                },
            )
            .unwrap();
        assert!(store.load_cursor(&live.uid).unwrap().is_none());
    }

    // ------------------------------------------------------- schema migration

    /// Build a database in the exact pre-`session_uid` shape, including its
    /// index names — a copy of the old `migrate()`, frozen here because reading
    /// that shape is the migration's whole job.
    fn legacy_database(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions(
                session_id        TEXT PRIMARY KEY,
                tmux_session      TEXT NOT NULL,
                tmux_socket       TEXT NOT NULL,
                cwd               TEXT NOT NULL,
                claude_session_id TEXT,
                transcript_path   TEXT,
                lifecycle         TEXT NOT NULL,
                created_at        TEXT NOT NULL,
                updated_at        TEXT NOT NULL
            );
            CREATE TABLE events(
                session_id      TEXT    NOT NULL,
                seq             INTEGER NOT NULL,
                ts              TEXT    NOT NULL,
                kind            TEXT    NOT NULL,
                payload         TEXT    NOT NULL,
                source          TEXT    NOT NULL,
                source_event_id TEXT,
                turn_id         TEXT,
                item_id         TEXT,
                PRIMARY KEY(session_id, seq)
            );
            CREATE UNIQUE INDEX events_dedup
                ON events(session_id, source, source_event_id)
                WHERE source_event_id IS NOT NULL;
            CREATE TABLE answers(
                request_id   TEXT PRIMARY KEY,
                session_id   TEXT NOT NULL,
                payload_hash TEXT NOT NULL,
                outcome      TEXT NOT NULL,
                created_at   TEXT NOT NULL
            );
            CREATE TABLE tail_cursors(
                session_id      TEXT PRIMARY KEY,
                path            TEXT    NOT NULL,
                dev             INTEGER NOT NULL,
                ino             INTEGER NOT NULL,
                offset          INTEGER NOT NULL,
                last_line_start INTEGER NOT NULL,
                last_line_sha   TEXT    NOT NULL,
                updated_at      TEXT    NOT NULL
            );
            CREATE TABLE pairing_codes(
                code_hash     TEXT PRIMARY KEY,
                allow_ssh     INTEGER NOT NULL,
                created_at    TEXT    NOT NULL,
                expires_at    TEXT    NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                consumed_at   TEXT
            );
            CREATE TABLE devices(
                device_id         TEXT PRIMARY KEY,
                name              TEXT NOT NULL,
                token_hash        TEXT NOT NULL UNIQUE,
                created_at        TEXT NOT NULL,
                last_seen_at      TEXT,
                revoked_at        TEXT,
                ssh_key_installed INTEGER NOT NULL DEFAULT 0,
                ssh_fingerprint   TEXT
            );
            CREATE UNIQUE INDEX devices_name ON devices(name);
            "#,
        )
        .unwrap();
        conn
    }

    fn legacy_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ccd-legacy-{}-{}-{}.db",
            std::process::id(),
            n,
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn an_older_database_is_migrated_without_losing_anything() {
        let path = legacy_path();
        {
            let conn = legacy_database(&path);
            conn.execute(
                "INSERT INTO sessions VALUES('cc-1','cc-1','codeconnect','/tmp/one',
                                             'claude-uuid','/tmp/one.jsonl','live',
                                             '2026-07-30T10:00:00.000Z','2026-07-30T11:00:00.000Z')",
                [],
            )
            .unwrap();
            for seq in 1..=4 {
                conn.execute(
                    "INSERT INTO events VALUES('cc-1', ?1, '2026-07-30T10:00:0?.000Z',
                                               'tool_call', '{\"a\":1}', 'hook', ?2, NULL, NULL)",
                    params![seq, format!("pre:toolu_{seq}")],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO answers VALUES('toolu_2','cc-1','hash-a','{\"request_id\":\"toolu_2\",
                     \"session_id\":\"cc-1\",\"decision\":{\"type\":\"allow\"},
                     \"resolved_by\":\"phone\",\"applied_via\":\"send_keys\",
                     \"resolved_at\":\"t\"}','2026-07-30T10:30:00.000Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tail_cursors VALUES('cc-1','/tmp/one.jsonl',1,2,4096,4000,'sha','t')",
                [],
            )
            .unwrap();
            // A name with history but no session row: the daemon adopts sessions
            // from hooks, and this is what a database rebuilt under a running
            // supervisor actually looks like (observed in practice).
            conn.execute(
                "INSERT INTO events VALUES('cc-2', 1, '2026-07-30T12:00:00.000Z',
                                           'session_start', '{}', 'hook', 'orphan-1', NULL, NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tail_cursors VALUES('cc-2','/tmp/two.jsonl',3,4,10,0,'sha2','t')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2, "both names became runs: {sessions:?}");

        let one = store.find_session("cc-1").unwrap().unwrap();
        assert!(protocol::uid::is_well_formed(&one.session_uid));
        assert_eq!(one.cwd, "/tmp/one");
        assert_eq!(one.transcript_path.as_deref(), Some("/tmp/one.jsonl"));
        assert_eq!(one.lifecycle, Lifecycle::Live);
        // The synthesised uid carries the session's real creation time, so the
        // migrated rows keep their order instead of all sorting as "now".
        assert_eq!(
            protocol::uid::timestamp_ms(&one.session_uid),
            protocol::time::unix_ms_from_rfc3339("2026-07-30T10:00:00.000Z")
        );

        // Events, numbering and dedup all survive, keyed by the new identity.
        assert_eq!(store.max_seq(&one.session_uid).unwrap(), 4);
        assert_eq!(store.count_events(&one.session_uid).unwrap(), 4);
        let seqs: Vec<u64> = store
            .events_after(&one.session_uid, 0, 100)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);

        // The ledger followed, so a retried tap still reads as a duplicate.
        let (hash, outcome) = store
            .get_answer(&one.session_uid, "toolu_2")
            .unwrap()
            .unwrap();
        assert_eq!(hash, "hash-a");
        assert_eq!(outcome.decision, AnswerDecision::Allow);

        // So did the cursor, so the tailer resumes instead of re-reading.
        assert_eq!(
            store.load_cursor(&one.session_uid).unwrap().unwrap().offset,
            4096
        );

        // The orphan got a real row with the transcript its cursor named, so
        // its history is reachable rather than stranded behind a uid nothing
        // lists.
        let two = store.find_session("cc-2").unwrap().unwrap();
        assert_eq!(two.lifecycle, Lifecycle::Unknown);
        assert_eq!(two.transcript_path.as_deref(), Some("/tmp/two.jsonl"));
        assert_eq!(store.max_seq(&two.session_uid).unwrap(), 1);
        assert_ne!(two.session_uid, one.session_uid);
    }

    #[test]
    fn migrating_is_idempotent_and_dedup_still_works_afterwards() {
        let path = legacy_path();
        {
            let conn = legacy_database(&path);
            conn.execute(
                "INSERT INTO sessions VALUES('cc-1','cc-1','codeconnect','/tmp','u','/tmp/x.jsonl',
                                             'live','2026-07-30T10:00:00.000Z','t')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events VALUES('cc-1',1,'t','tool_call','{}','hook','pre:a',NULL,NULL)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let uid = store.find_session("cc-1").unwrap().unwrap().session_uid;
        drop(store);

        // Re-opening must not migrate a second time and mint a second identity.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        assert_eq!(
            store.find_session("cc-1").unwrap().unwrap().session_uid,
            uid
        );

        // The old `events_dedup` index was attached to the renamed table; if it
        // had survived, `CREATE UNIQUE INDEX IF NOT EXISTS` would have silently
        // skipped building the new one.
        //
        // Asserted against the schema itself, not only through behaviour: the
        // steady-state dedup is an explicit SELECT under the connection mutex,
        // so an append would still be deduplicated with no index at all. Only
        // this catches the index going missing — and with it the last defence
        // if a second writer ever appears.
        let session = SessionKey::new(&uid, "cc-1");
        let mut replay = pending(&session, EventKind::ToolCall, Some("pre:a"));
        replay.session_uid = uid.clone();
        assert!(
            store.append_event(&replay).unwrap().is_none(),
            "dedup must still hold after the migration"
        );
        assert_eq!(store.max_seq(&uid).unwrap(), 1);
        drop(store);

        let conn = Connection::open(&path).unwrap();
        let indexed_on: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_list('events')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(
            indexed_on.iter().any(|name| name == "events_dedup"),
            "the dedup index did not survive the migration: {indexed_on:?}"
        );
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_info('events_dedup')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(
            columns,
            vec!["session_uid", "source", "source_event_id"],
            "the rebuilt index must be keyed by the run, not the name"
        );
    }

    #[test]
    fn a_second_process_arriving_mid_migration_is_a_no_op() {
        // `Store::open` checks the schema shape before taking a write lock, and
        // that check is not process-local: a second `ccd` starting at the same
        // moment can pass it, block on the first one's transaction, and arrive
        // with the work already done. Re-minting every identity there would
        // orphan the log this feature exists to protect.
        //
        // Simulated by calling the migration a second time against an
        // already-migrated database, which is exactly the state that racer sees.
        let path = legacy_path();
        {
            let conn = legacy_database(&path);
            conn.execute(
                "INSERT INTO sessions VALUES('cc-1','cc-1','codeconnect','/tmp','u','/tmp/x.jsonl',
                                             'live','2026-07-30T10:00:00.000Z','t')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events VALUES('cc-1',1,'t','tool_call','{}','hook','pre:a',NULL,NULL)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let uid = store.find_session("cc-1").unwrap().unwrap().session_uid;
        drop(store);

        let mut conn = Connection::open(&path).unwrap();
        assert!(
            !needs_session_uid_migration(&conn).unwrap(),
            "the database is already migrated"
        );
        migrate_to_session_uids(&mut conn).expect("a redundant migration must not error");

        // The identity is intact: no re-mint, no orphaned events.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        assert_eq!(
            store.find_session("cc-1").unwrap().unwrap().session_uid,
            uid
        );
        assert_eq!(store.max_seq(&uid).unwrap(), 1);
        assert_eq!(store.count_events(&uid).unwrap(), 1);
    }

    #[test]
    fn a_fresh_database_is_created_at_the_current_schema() {
        let (store, path) = temp_store();
        drop(store);
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(column_exists(&conn, "events", "session_uid").unwrap());
        assert!(!needs_session_uid_migration(&conn).unwrap());
    }
}
