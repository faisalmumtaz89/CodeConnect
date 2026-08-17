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
//! below — and the sync `codeconnect`-facing paths — have no runtime to defer to.

use std::path::Path;
use std::sync::Mutex;

/// Read-only connections. Small on purpose: the readers are a phone replaying
/// a backlog, the fleet listing, and the revocation check, and each is bounded
/// and short. More connections would buy nothing but page cache.
const READERS: usize = 4;

use anyhow::{Context, Result};
use protocol::event::{Event, EventKind, Lifecycle, PendingEvent, SessionKey, Source};
use protocol::pairing::DeviceSummary;
use protocol::secret::Redacted;
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
///   * `3` — the push tuple `(token, environment, credential)` is normalized
///     once on the way up (see [`normalize_push_tuples`]). **A version gate, not
///     an idempotent normalizer.** The GLOB fix below runs every start because
///     the value it clears is always wrong; this one clears a *credential*,
///     which usually is not — so running it every start would wipe every relay
///     bearer on every restart and leave relay push permanently re-registering.
///     It can therefore only fire on the transition, which is exactly what a
///     version bump buys. The push_credential *column* was added under version 2
///     without a bump (it is an additive `ALTER`, and the GLOB normalizer proves
///     a column can arrive that way); what needs the bump is the one-shot data
///     repair, not the column.
const SCHEMA_VERSION: i64 = 3;

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
}

impl DeviceRow {
    pub fn to_summary(&self) -> DeviceSummary {
        DeviceSummary {
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            created_at: self.created_at.clone(),
            last_seen_at: self.last_seen_at.clone(),
            revoked_at: self.revoked_at.clone(),
        }
    }
}

/// One device's push registration as the store holds it.
///
/// The three push values travel together because they are only meaningful
/// together: the credential authorizes the relay to address *that* token, and
/// the environment says which Apple host the token lives on. Splitting them
/// across reads is how a caller ends up pairing a rotated token with the
/// credential minted for its predecessor.
///
/// `credential` is `None` for the direct path — a daemon holding its own Apple
/// key talks to APNs itself and is never issued one.
#[derive(Clone)]
pub struct PushRegistration {
    pub device_id: String,
    pub token: String,
    pub environment: String,
    /// The bearer as a [`Redacted`], so it survives no `Debug` on the way to a
    /// sender — not this struct's, not a caller's, not an `anyhow` chain. The
    /// store is the one place a raw bearer touches disk; everywhere else it
    /// stays wrapped, and the type is what keeps it so rather than a hand-written
    /// `Debug` a later field could break.
    pub credential: Option<Redacted>,
}

/// **The token is abbreviated.** The credential renders itself safely — it is a
/// [`Redacted`] — but the token is a bare `String`, and a derived `Debug` would
/// print it whole. It addresses a phone but authorizes nothing on its own, and a
/// stable few characters are what lets two log lines about one registration be
/// recognised as one.
impl std::fmt::Debug for PushRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushRegistration")
            .field("device_id", &self.device_id)
            .field("token", &abbreviated(&self.token))
            .field("environment", &self.environment)
            .field("credential", &self.credential)
            .finish()
    }
}

/// Enough of a value to recognise it again, never enough to use it.
///
/// Crate-visible because a device token is rendered in two places — a stored
/// row and a live delivery target — and one policy rendered two ways is a
/// policy only until somebody widens the looser one.
pub(crate) fn abbreviated(value: &str) -> String {
    const KEPT: usize = 8;
    let head: String = value.chars().take(KEPT).collect();
    if head.chars().count() < value.chars().count() {
        format!("{head}…")
    } else {
        head
    }
}

/// Why a pairing attempt failed, in the daemon's own words.
///
/// The distinctions exist for the *log*, not for the peer: every failure is
/// reported over the wire as the same opaque refusal, because telling an
/// unauthenticated caller "that code existed but expired" is a free oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingConsume {
    Consumed,
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

/// What `upsert_session` did.
#[must_use = "a tombstoned upsert wrote nothing; the session must not be set up"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionUpsert {
    /// The row exists (written or updated).
    Present,
    /// The uid was deliberately deleted; nothing was written. The caller must
    /// abandon whatever it was setting up for this session.
    Tombstoned,
}

#[cfg(test)]
impl SessionUpsert {
    /// For seeding in tests, where the uid is fresh and a tombstone would mean
    /// the fixture itself is wrong. Compiled only for tests: production callers
    /// must handle `Tombstoned`, not assert it away.
    #[track_caller]
    pub fn assert_present(self) {
        assert!(
            matches!(self, SessionUpsert::Present),
            "seeding a session that has a tombstone; the fixture is wrong"
        );
    }
}

/// What `delete_exited_session` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted {
        events: u64,
    },
    /// Refused: the row is not `Exited`. Carries the daemon's own word for what
    /// it is, so the refusal can be stated rather than guessed at.
    NotExited {
        lifecycle: String,
    },
    NotFound,
}

/// Everything a run owns, besides its own row in `sessions`.
///
/// One list, in one place, because removing a session has to remove all of it:
/// a table left off leaves rows keyed to a `session_uid` that no longer exists,
/// and the ones here are exactly the tables that decide whether an answer may
/// be typed. An orphaned `answer_claims` row would outlive its run and be
/// recovered as an indeterminate answer for a session nobody can name.
/// [`Store::prune_exited_sessions`] and [`Store::delete_exited_session`] both
/// delete from every one of them — sharing this list is what stops the two paths
/// drifting — and `every_session_scoped_table_is_named_in_the_prune_list` reads
/// the schema back to prove the list has not fallen behind it.
const SESSION_SCOPED_TABLES: &[&str] = &[
    "events",
    "answers",
    "pending_approvals",
    "answer_claims",
    "text_mutations",
    "tail_cursors",
];

/// One run removed by `codeconnect sessions prune`, and what went with it.
///
/// Returned rather than counted, because deleting an operator's history is
/// exactly the operation that has to be able to say what it did — a bare
/// "removed 18 sessions" is not something anybody can check afterwards, and by
/// then the evidence is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedSession {
    pub session_uid: String,
    pub session_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub events: u64,
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
        // Read *before* anything below writes it: this is the version the
        // database was last left at, and the only thing that can tell a
        // one-shot repair from a restart. `0` on a database this build has never
        // opened — a fresh file, or one from before `user_version` was used —
        // and every one-shot gate below reads that as "everything is new here".
        let from_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // The rebuild has to happen before `create_schema`: `CREATE TABLE IF NOT
        // EXISTS` is a no-op against a legacy table, so it would leave the old
        // shape in place and every later statement would fail on a missing
        // column.
        if needs_session_uid_migration(&conn)? {
            migrate_to_session_uids(&mut conn)?;
        }
        create_schema(&conn)?;
        // Additive, and applied after `create_schema` for the reason the note
        // above gives in reverse: `CREATE TABLE IF NOT EXISTS` will not widen a
        // table that already exists, so a database written before push existed
        // keeps its old `devices` shape and every push statement fails on a
        // missing column.
        if needs_column_additions(&conn)? {
            add_missing_columns(&mut conn)?;
        }
        // Ordered after `create_schema` so a database that never had these
        // tables gets them in the current shape and finds nothing to do here.
        // Never fatal: a daemon that refuses to open its own database is
        // strictly worse than one carrying a column it does not write, and
        // launchd would restart it into the same refusal for ever.
        if let Err(err) = drop_retired_columns(&mut conn) {
            crate::log_error!(
                "schema: the retired SSH columns are still on this database ({err:#}); every \
                 statement this build writes works around them, so the daemon serves normally \
                 and the next start tries the removal again"
            );
        }
        // A normalizer that runs every start, not a version-gated repair: the
        // value it clears is always wrong, so repeating it costs nothing and
        // needs no gate — unlike [`normalize_push_tuples`] below, which clears a
        // credential that usually is not wrong and therefore fires only on the
        // transition. Adopted rows (`claude:*`, the prefix
        // cc-hook mints for sessions CodeConnect did not launch) were written
        // with a fabricated tmux location — a name in a server they never lived
        // in — and the liveness sweep read the inevitable "no such session" as
        // proof of death for runs that were alive. Empty is the honest value:
        // the daemon has no idea where, or whether, this process runs. GLOB,
        // not LIKE: LIKE is case-insensitive and the prefix is an exact
        // contract. Ordered after `migrate_to_session_uids`, whose synthesized
        // rows this must also catch on a legacy database.
        conn.execute(
            "UPDATE sessions SET tmux_session = '', tmux_socket = ''
              WHERE session_id GLOB 'claude:*'
                AND (tmux_session != '' OR tmux_socket != '')",
            [],
        )?;
        // The one-shot push-tuple repair — see [`SCHEMA_VERSION`] on why it is
        // gated where the GLOB fix above is not. Ordered after the column
        // additions so the columns it names are present on a legacy database.
        if from_version < 3 {
            normalize_push_tuples(&conn)?;
        }
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
    ///
    /// **A batch for a session that is gone is dropped, cursor and all.** Ending
    /// a run stops its tail by *queueing* a `TailCommand::Stop`, so one more
    /// poll can already be in flight when the run's rows are deleted. Landing it
    /// afterwards would file events and a cursor under a `session_uid` with no
    /// row — the schema has no foreign key to prevent that, and the resulting
    /// orphans are the exact shape `SESSION_SCOPED_TABLES` exists to avoid.
    /// Checked inside the same `BEGIN IMMEDIATE` as the writes, so the delete
    /// either happened before this and the batch is refused, or after it and the
    /// delete takes these rows with the rest. There is no third ordering: both
    /// transactions are `Immediate` on the one write connection.
    pub fn append_batch_with_cursor(
        &self,
        session_uid: &str,
        pendings: &[PendingEvent],
        cursor: &TailCursor,
    ) -> Result<Vec<Event>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let known: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_uid = ?1)",
            params![session_uid],
            |row| row.get(0),
        )?;
        if !known {
            // Rolled back rather than committed empty: the cursor must not move
            // for a session whose log no longer exists.
            tx.rollback()?;
            return Ok(Vec::new());
        }
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

    /// Write or update one session row — unless its uid has been deliberately
    /// deleted, in which case nothing is written and the caller is told.
    ///
    /// **The tombstone check is inside the statement, and that is the fix.** A
    /// hook or registration looks a row up, awaits, and writes; a phone's
    /// delete can commit between those two steps, and the write would re-insert
    /// the row the user just removed. No ordering at the caller can close that
    /// window — only the write itself refusing can, because every write goes
    /// through the one connection whose transactions are serial.
    ///
    /// Callers must not treat `Tombstoned` as success: whatever they were about
    /// to set up for this session — a tail, a push, an in-memory supervisor —
    /// must not happen.
    pub fn upsert_session(&self, row: &SessionRow) -> Result<SessionUpsert> {
        let conn = self.write();
        let changed = conn.execute(
            "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                  claude_session_id, transcript_path, lifecycle,
                                  created_at, updated_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             WHERE NOT EXISTS (SELECT 1 FROM deleted_sessions WHERE session_uid = ?1)
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
        // `INSERT ... SELECT ... WHERE NOT EXISTS` writes zero rows exactly when
        // the tombstone matched; `ON CONFLICT` paths always write one.
        Ok(if changed == 0 {
            SessionUpsert::Tombstoned
        } else {
            SessionUpsert::Present
        })
    }

    /// Whether this name was deliberately removed — see `deleted_names`.
    pub fn name_is_tombstoned(&self, session_id: &str) -> Result<bool> {
        let conn = self.read();
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deleted_names WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    /// A SessionStart is a resume announcing itself: observation was asked for
    /// again, so the removal stops applying.
    pub fn clear_name_tombstone(&self, session_id: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "DELETE FROM deleted_names WHERE session_id = ?1",
            params![session_id],
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

    /// Remove exactly one ended run, by uid.
    ///
    /// Shares `SESSION_SCOPED_TABLES` and the `lifecycle = 'exited'` predicate with
    /// `prune_exited_sessions` rather than restating them: a second delete path
    /// that forgot a table would leave orphaned answers and cursors pointing at a
    /// session nobody can see, and the two would drift the first time a table was
    /// added.
    ///
    /// The lifecycle predicate is the safety property and it is in the SQL, not in
    /// the caller. A phone asking to delete a running session is refused by the
    /// statement itself, whatever the phone believed.
    pub fn delete_exited_session(&self, session_uid: &str) -> Result<DeleteOutcome> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT lifecycle, tmux_socket, session_id FROM sessions WHERE session_uid = ?1",
                params![session_uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((lifecycle, tmux_socket, session_id_of_row)) = row else {
            return Ok(DeleteOutcome::NotFound);
        };
        // **A run the daemon never hosted is deletable at any lifecycle.** An
        // empty `tmux_socket` means adoption: the hooks arrived but nothing here
        // spawned or supervises the process, so no probe can ever prove it
        // ended, and holding the row hostage to a proof that cannot exist would
        // make it immortal. "The daemon cannot say — you may remove it" is the
        // honest contract; Claude Code's own transcript, the authoritative
        // record, is untouched either way. Hosted rows keep the strict rule.
        let unhosted = tmux_socket.is_empty();
        if !unhosted && lifecycle != lifecycle_str(Lifecycle::Exited) {
            // **Which non-ended state it is, because they do not mean the same
            // thing.** `Live` and `Spawning` say the agent is there. `Unknown`
            // says the opposite of a claim: the daemon could not establish what
            // happened to this run — which `prune_exited_sessions` treats as the
            // sharpest reason of all not to delete, and which is precisely not
            // "still running". Both refuse; only one of them may say so.
            return Ok(DeleteOutcome::NotExited { lifecycle });
        }

        let events: u64 = tx.query_row(
            "SELECT COUNT(*) FROM events WHERE session_uid = ?1",
            params![session_uid],
            |row| row.get::<_, i64>(0),
        )? as u64;

        for table in SESSION_SCOPED_TABLES {
            // The table names are a compile-time list in this file; nothing a
            // caller supplies reaches the statement text.
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_uid = ?1"),
                params![session_uid],
            )?;
        }
        let gone = tx.execute(
            "DELETE FROM sessions
              WHERE session_uid = ?1 AND (lifecycle = ?2 OR tmux_socket = '')",
            params![session_uid, lifecycle_str(Lifecycle::Exited)],
        )?;
        if gone != 1 {
            // The row changed underneath the transaction. Roll back rather than
            // report a deletion that did not happen. Unreachable as things
            // stand — one `BEGIN IMMEDIATE` on the one write connection — and
            // kept as the same destructive-transaction assertion
            // `prune_exited_sessions` makes: a deletion that removed a number of
            // rows nobody predicted must abort, not be reported as success.
            tx.rollback()?;
            return Ok(DeleteOutcome::NotExited {
                lifecycle: lifecycle_str(Lifecycle::Unknown).into(),
            });
        }
        // The decision, recorded with the deed: from this commit on, nothing may
        // file anything under this uid again — see `deleted_sessions`.
        tx.execute(
            "INSERT OR IGNORE INTO deleted_sessions(session_uid, deleted_at) VALUES(?1, ?2)",
            params![session_uid, protocol::time::now_rfc3339()],
        )?;
        if unhosted {
            // And under this *name*: an adopted run's hooks carry no uid, so
            // without this the very next one would mint a fresh identity and
            // put the row straight back — the resurrection wearing a new uid.
            // See `deleted_names`; a SessionStart clears it.
            tx.execute(
                "INSERT OR IGNORE INTO deleted_names(session_id, deleted_at) VALUES(?1, ?2)",
                params![session_id_of_row, protocol::time::now_rfc3339()],
            )?;
        }
        tx.commit()?;
        Ok(DeleteOutcome::Deleted { events })
    }

    /// Delete every run that has **ended**, and everything filed under it.
    ///
    /// The event log is the source of truth, so nothing here happens on a timer
    /// or a size threshold: a machine that quietly discarded an operator's
    /// history to save disk would be deciding, on their behalf, which of their
    /// agents' work was worth keeping. This runs when a human asks and never
    /// otherwise.
    ///
    /// What makes it safe to run at all is that it is defined *only* over
    /// `Exited`. `Live` is left alone because the agent may be working, and —
    /// more sharply — `Unknown` is left alone because "we could not establish
    /// what happened to this" is the one state where deletion destroys the
    /// evidence somebody would need to find out. `protect` is the caller's
    /// additional veto, for runs the daemon can see are still in use whatever
    /// the row says.
    ///
    /// Three things make this hard to get wrong:
    ///   * One `BEGIN IMMEDIATE` around the whole sweep, so a session cannot
    ///     come back to life between being chosen and being deleted.
    ///   * The final `DELETE` re-states `lifecycle = 'exited'` in its own
    ///     `WHERE` and insists it removed exactly one row. A bug in the
    ///     candidate query above therefore aborts the transaction rather than
    ///     removing a live agent's history.
    ///   * Every table keyed by `session_uid` is named in one list, and a test
    ///     reads the schema back to prove the list is complete — so a table
    ///     added later leaves orphans loudly rather than silently.
    pub fn prune_exited_sessions(
        &self,
        protect: &[String],
        dry_run: bool,
    ) -> Result<Vec<PrunedSession>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let candidates: Vec<(String, String, String, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT session_uid, session_id, cwd, created_at, updated_at
                   FROM sessions WHERE lifecycle = ?1
                  ORDER BY created_at ASC, session_uid ASC",
            )?;
            let rows = stmt.query_map(params![lifecycle_str(Lifecycle::Exited)], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut removed = Vec::new();
        for (session_uid, session_id, cwd, created_at, updated_at) in candidates {
            if protect.iter().any(|held| held == &session_uid) {
                continue;
            }
            let events: u64 = tx.query_row(
                "SELECT COUNT(*) FROM events WHERE session_uid = ?1",
                params![session_uid],
                |row| row.get::<_, i64>(0),
            )? as u64;

            if !dry_run {
                for table in SESSION_SCOPED_TABLES {
                    tx.execute(
                        // The table names are a compile-time list in this file;
                        // nothing a caller supplies reaches the statement text.
                        &format!("DELETE FROM {table} WHERE session_uid = ?1"),
                        params![session_uid],
                    )?;
                }
                let gone = tx.execute(
                    "DELETE FROM sessions WHERE session_uid = ?1 AND lifecycle = ?2",
                    params![session_uid, lifecycle_str(Lifecycle::Exited)],
                )?;
                // Belt and braces over the query above. If this ever removes
                // anything other than exactly the one ended run it named, the
                // whole transaction is abandoned rather than half-applied.
                anyhow::ensure!(
                    gone == 1,
                    "refusing to prune {session_uid}: the delete matched {gone} rows rather than \
                     the one ended session it named"
                );
                // Same decision, same record as the phone's single delete: a
                // pruned uid is deliberately erased, and a hook in flight when
                // this commits must not be able to resurrect it. Dry runs
                // decide nothing and so record nothing.
                tx.execute(
                    "INSERT OR IGNORE INTO deleted_sessions(session_uid, deleted_at) \
                     VALUES(?1, ?2)",
                    params![session_uid, protocol::time::now_rfc3339()],
                )?;
            }

            removed.push(PrunedSession {
                session_uid,
                session_id,
                cwd,
                created_at,
                updated_at,
                events,
            });
        }

        if dry_run {
            // Nothing was written, and dropping the transaction rather than
            // committing it is what says so.
            return Ok(removed);
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Events filed under a `session_uid` that has no row in `sessions`.
    ///
    /// Found while verifying the prune against the owner's real database: 24
    /// events, one answer and one tail cursor belonging to three runs
    /// (`cc-audit`, `cc-clean`, `cc-gone`) with no session row at all. They
    /// predate any prune — the same counts are in a backup taken beforehand —
    /// and nothing reaches them: they are absent from the fleet, from
    /// `codeconnect sessions`, and from the prune itself, which is defined over
    /// `sessions` rows and correctly does not invent one.
    ///
    /// There is no foreign key to prevent this (SQLite does not enforce one
    /// unless asked, and adding it to an existing log is a migration with real
    /// risk), so this counts them instead. **Counting, not deleting**: data
    /// whose provenance is not understood is exactly the data an automatic
    /// cleanup must not touch. Reported so the operator can see that it exists,
    /// which is more than was true before.
    pub fn orphan_event_count(&self) -> Result<u64> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events
              WHERE session_uid NOT IN (SELECT session_uid FROM sessions)",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
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
            // `WHERE EXISTS(sessions)`: an answer settling just as its session
            // is deleted must vanish with the session rather than survive it as
            // a row nothing can ever name again.
            "INSERT INTO answers(session_uid, request_id, payload_hash, outcome, created_at)
             SELECT ?1, ?2, ?3, ?4, ?5
              WHERE EXISTS (SELECT 1 FROM sessions WHERE session_uid = ?1)",
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
    /// transaction so the table cannot grow without bound on a machine where the
    /// operator repeatedly runs `codeconnect pair` and never scans.
    ///
    /// The statement is chosen against the table's actual columns because of one
    /// of them: `allow_ssh` is retired (see [`RETIRED_COLUMNS`]) but is `NOT
    /// NULL` with no default, so on the one database where SQLite refused to drop
    /// it an `INSERT` that does not name it fails and no phone can pair. Named
    /// with a literal `0` while it is there, that refusal costs a column of dead
    /// weight and nothing else. `0` is the value the build that declared the
    /// column wrote for "this code grants no SSH access", which is the truth
    /// about every code this build mints.
    ///
    /// Asked per mint rather than remembered from startup: mints are human-paced
    /// — one `codeconnect pair` — so a `PRAGMA` costs nothing measurable, and an
    /// operator who clears the obstacle and restarts a *second* daemon takes the
    /// column out from under a remembered answer.
    ///
    /// The sweep, that probe and the `INSERT` are one immediate transaction, for
    /// the same reason [`drop_retired_columns`] is: `BEGIN IMMEDIATE` takes the
    /// write lock up front, so the second daemon that clears the obstacle cannot
    /// land its `ALTER TABLE … DROP COLUMN` *between* the probe and the statement
    /// the probe chose. Without it the answer is stale by the width of two
    /// statements, and a mint that read the column as present names one that is
    /// gone by the time it inserts — a `codeconnect pair` failing on "no such
    /// column: allow_ssh". Serialised this way the probe is not merely fresh at
    /// the moment it is asked, it is still true at the moment it is acted on:
    /// either the drop lands before this transaction begins and the mint sees a
    /// table without the column, or it waits until after the commit and the mint
    /// fills the column it saw.
    pub fn create_pairing_code(
        &self,
        code_hash: &str,
        expires_at: &str,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM pairing_codes WHERE expires_at_ms < ?1",
            params![now_ms],
        )?;
        // Matched the way SQLite matches a column name, on the same terms as
        // `retired_columns_present`: `ALLOW_SSH` is the same `NOT NULL` column.
        let leftover_allow_ssh = table_columns(&tx, "pairing_codes")?
            .iter()
            .any(|actual| actual.eq_ignore_ascii_case("allow_ssh"));
        // The instant the paragraph above is about, and the only place a test
        // can stand a second daemon in. Compiled out of the daemon entirely.
        #[cfg(test)]
        test_second_daemon_drops_allow_ssh(&tx);
        let insert = if leftover_allow_ssh {
            "INSERT INTO pairing_codes(code_hash, created_at, expires_at,
                                       expires_at_ms, consumed_at, allow_ssh)
             VALUES(?1, ?2, ?3, ?4, NULL, 0)"
        } else {
            "INSERT INTO pairing_codes(code_hash, created_at, expires_at,
                                       expires_at_ms, consumed_at)
             VALUES(?1, ?2, ?3, ?4, NULL)"
        };
        tx.execute(
            insert,
            params![
                code_hash,
                protocol::time::rfc3339_from_unix_ms(now_ms),
                expires_at,
                expires_at_ms,
            ],
        )?;
        tx.commit()?;
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
        let row: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT expires_at_ms, consumed_at FROM pairing_codes WHERE code_hash = ?1",
                params![code_hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let outcome = match row {
            None => PairingConsume::NotFound,
            Some((_, Some(_))) => PairingConsume::AlreadyUsed,
            Some((expires_at_ms, None)) if expires_at_ms < now_ms => PairingConsume::Expired,
            Some((_, None)) => {
                let changed = tx.execute(
                    "UPDATE pairing_codes SET consumed_at = ?2
                      WHERE code_hash = ?1 AND consumed_at IS NULL",
                    params![code_hash, protocol::time::rfc3339_from_unix_ms(now_ms)],
                )?;
                if changed == 1 {
                    PairingConsume::Consumed
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
                                 last_seen_at, revoked_at)
             VALUES(?1, ?2, ?3, ?4, NULL, NULL)",
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
                "SELECT device_id, name, created_at, last_seen_at, revoked_at
                   FROM devices
                  WHERE token_hash = ?1 AND revoked_at IS NULL",
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

    /// Record where a device wants its pushes sent.
    ///
    /// Keyed on the device, so re-registering replaces rather than accumulates:
    /// APNs reissues a token on reinstall and on restore-from-backup, and a
    /// stale one left beside it would be pushed to forever.
    /// A push token names one physical phone, so registering it here strips
    /// it from every other device row first — in the same transaction, so no
    /// interleaving can observe two rows holding it. Without this, a re-pair
    /// left the old row's copy in place and every doorbell rang the same
    /// phone twice: measured live, one production token on two rows, twin
    /// notifications at the same instant.
    /// The strip only ever commits alongside a successful claim: if the
    /// claiming row is missing or was revoked after the caller's own check,
    /// the transaction rolls back whole rather than leaving the token owned
    /// by no active row.
    /// Returns the device rows this token was taken *from*, so their senders
    /// can be retired: a phone that re-pairs arrives under a new device id, and
    /// the old row's queue would otherwise wait on a token it no longer holds.
    ///
    /// **The tuple is written whole.** `credential` of `None` writes SQL NULL
    /// rather than leaving whatever was there: the relay checks the credential
    /// against the token it accompanies, so a rotation that kept the previous
    /// credential would be refused on every push while the row still reads as
    /// registered — and refused as a bad *credential*, which is not the same
    /// fact as a phone that has gone away. For the same reason the strip clears
    /// all three columns: a row that has lost the token has no business keeping
    /// the credential minted to address it.
    pub fn set_push_token(
        &self,
        device_id: &str,
        token: &str,
        environment: &str,
        credential: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut conn = self.write();
        let tx = conn.transaction()?;
        let displaced: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT device_id FROM devices WHERE push_token = ?1 AND device_id <> ?2",
            )?;
            let rows = stmt.query_map(params![token, device_id], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        tx.execute(
            "UPDATE devices SET push_token = NULL, push_environment = NULL, push_credential = NULL
              WHERE push_token = ?1 AND device_id <> ?2",
            params![token, device_id],
        )?;
        let claimed = tx.execute(
            "UPDATE devices SET push_token = ?2, push_environment = ?3, push_credential = ?4
              WHERE device_id = ?1 AND revoked_at IS NULL",
            params![device_id, token, environment, credential],
        )?;
        if claimed != 1 {
            anyhow::bail!("push registration for unknown or revoked device {device_id}");
        }
        tx.commit()?;
        Ok(displaced)
    }

    /// Record the environment Apple actually accepted, leaving the token and the
    /// credential alone.
    /// **Only for the exact `(token, credential)` tuple that was corrected.** A
    /// late answer about a tuple the phone has already rotated must not move the
    /// current one to the wrong host, which would make every push to it fail.
    /// The token can be rotated by a reinstall and the credential by a relay
    /// reissue, and either happening between the attempt and its answer makes
    /// this answer stale — so both are compared and neither is written.
    /// `push_credential IS ?3` rather than `= ?3`: `IS` is SQLite's null-safe
    /// equality, so a direct-mode row (credential `NULL`) is matched by a
    /// correction that carries `None`, and a relay row only by the exact bearer
    /// — with no separate branch for the two.
    pub fn set_push_environment(
        &self,
        device_id: &str,
        token: &str,
        credential: Option<&str>,
        environment: &str,
    ) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE devices SET push_environment = ?4
              WHERE device_id = ?1 AND push_token = ?2 AND push_credential IS ?3",
            params![device_id, token, credential, environment],
        )?;
        Ok(())
    }

    /// Apple has said this token is dead. Cleared rather than remembered: the
    /// device row itself stays, because the pairing is still valid and the
    /// phone may register again on next launch.
    /// **Only the token Apple refused.** A device id outlives the token behind
    /// it: a phone that reinstalls registers a new one under the same row, and
    /// a late `410` for the old token would otherwise erase the new one and
    /// leave a paired phone silently unable to receive anything.
    /// The relay credential goes with it, on the same terms as the strip in
    /// [`Store::set_push_token`]: it names a token Apple has disowned, and the
    /// phone mints a fresh one when it registers again.
    /// Returns whether the refused tuple was the one registered — `false` means
    /// the phone has since rotated to another and nothing was cleared.
    /// **The whole tuple is the key.** A `410` snapshotted under `(T, C1)` must
    /// leave a row now reading `(T, C2)` untouched, exactly as it leaves one
    /// reading `(T2, …)` untouched: a relay reissue rotates the credential
    /// under a token Apple still knows, and a departure about the old bearer is
    /// not a departure about the phone. `push_credential IS ?3` is the same
    /// null-safe compare `set_push_environment` uses, so a direct-mode row is
    /// matched by a clear carrying `None` and a relay row only by its bearer.
    pub fn clear_push_token(
        &self,
        device_id: &str,
        refused_token: &str,
        refused_credential: Option<&str>,
    ) -> Result<bool> {
        let conn = self.write();
        let cleared = conn.execute(
            "UPDATE devices SET push_token = NULL, push_environment = NULL, push_credential = NULL \
             WHERE device_id = ?1 AND push_token = ?2 AND push_credential IS ?3",
            params![device_id, refused_token, refused_credential],
        )?;
        Ok(cleared > 0)
    }

    /// Every device that can currently receive a push.
    ///
    /// **This read is the authorization point for a push.** Revoked devices are
    /// excluded here rather than at the call site, so a revoked phone stops
    /// being offered from the next read onward — otherwise revocation would
    /// leak the fact that an agent is waiting to a device that is no longer
    /// trusted. A doorbell already snapshotted from an earlier read may still
    /// go out; nothing can recall a request handed to Apple.
    /// The credential comes back in the same row as the token it authorizes, so
    /// a sender cannot assemble a request from two reads taken either side of a
    /// rotation.
    pub fn push_targets(&self) -> Result<Vec<PushRegistration>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT device_id, push_token, COALESCE(push_environment, 'sandbox'), push_credential
               FROM devices
              WHERE push_token IS NOT NULL AND push_token <> '' AND revoked_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PushRegistration {
                device_id: row.get(0)?,
                token: row.get(1)?,
                environment: row.get(2)?,
                // Wrapped the instant it leaves the database, so the raw bearer
                // exists as a bare `String` only inside this closure.
                credential: row.get::<_, Option<String>>(3)?.map(Redacted::from),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The environment this device's registered token lives at, or `None` when
    /// it has no token. Feeds `hello_ack.push_environment`.
    ///
    /// A row with no token and no row at all are one answer, because they are
    /// one fact to the phone asking: there is nothing registered here. A device
    /// id it does not recognise is not something the handshake can act on
    /// differently.
    ///
    /// **Deliberately not filtered on `revoked_at`.** A revoked device cannot
    /// complete the handshake this feeds, so the filter would be unreachable
    /// code standing in for a check that belongs — and already happens — at
    /// authentication. [`Store::push_targets`] is the read that authorizes a
    /// push, and it does filter.
    ///
    /// `NULL` reads as `sandbox`, exactly as it does in `push_targets`, so the
    /// phone is told the host the daemon would actually push at rather than a
    /// second opinion about the same row.
    pub fn push_environment_for(&self, device_id: &str) -> Result<Option<String>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                "SELECT COALESCE(push_environment, 'sandbox')
                   FROM devices
                  WHERE device_id = ?1 AND push_token IS NOT NULL AND push_token <> ''",
                params![device_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
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
            "SELECT device_id, name, created_at, last_seen_at, revoked_at
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
    ///
    /// **The push tuple goes with the trust.** Filtering revoked rows out of
    /// `push_targets` is what stops the notifications; erasing the tuple here is
    /// what stops the *secrets* outliving the decision. A revoked row keeps its
    /// APNs token and its relay bearer for ever otherwise: nothing reads them,
    /// so nothing ever replaces them, and no path clears them — `clear_push_token`
    /// is only reached from a delivery, and a revoked row is never delivered to.
    /// That is a live credential pair sitting on disk after the operator said
    /// this phone is no longer trusted, which is the opposite of what they asked
    /// for. Written in the same statement as the revocation so there is no
    /// instant in which one holds without the other.
    pub fn revoke_device(&self, device_id: &str, at: &str) -> Result<bool> {
        let conn = self.write();
        let changed = conn.execute(
            "UPDATE devices
                SET revoked_at = ?2,
                    push_token = NULL,
                    push_environment = NULL,
                    push_credential = NULL
              WHERE device_id = ?1 AND revoked_at IS NULL",
            params![device_id, at],
        )?;
        Ok(changed == 1)
    }

    // ----------------------------------------------------- pending approvals

    /// Persist a card so a restart can still answer "what is this agent waiting
    /// for?" with the truth rather than with silence.
    /// False when the session was deleted while the approval was in flight:
    /// nothing was written, and the caller must retire its in-memory half too.
    pub fn upsert_pending_approval(&self, row: &PendingApprovalRow) -> Result<bool> {
        let conn = self.write();
        let changed = conn.execute(
            // `SELECT ... WHERE EXISTS(sessions)` rather than VALUES: an
            // approval for a session that was deleted mid-flight must not leave
            // an orphan row that is later recovered as an indeterminate answer
            // for a run nobody can name.
            "INSERT INTO pending_approvals(session_uid, session_id, request_id, card,
                                           generation, created_ms)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
              WHERE EXISTS (SELECT 1 FROM sessions WHERE session_uid = ?1)
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
        Ok(changed > 0)
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
                    // Same rule as `answers`: no new claim for a session that
                    // was deleted while the request was in flight.
                    "INSERT INTO text_mutations(session_uid, request_id, payload_hash,
                                                status, matched, started_at, settled_at)
                     SELECT ?1, ?2, ?3, 'applying', NULL, ?4, NULL
                      WHERE EXISTS (SELECT 1 FROM sessions WHERE session_uid = ?1)",
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

/// The database a second daemon takes `pairing_codes.allow_ssh` off at the
/// instant [`Store::create_pairing_code`] has probed for that column and has not
/// yet inserted. `None` (the default) means no second daemon, which is every
/// mint outside the one test that arms this.
///
/// The interleaving has no other seam: the two processes are real processes in
/// production, and a test that raced a thread against the mint would be asking
/// the scheduler for the one nanosecond that matters.
#[cfg(test)]
static TEST_MIGRATION_MID_MINT: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Arms that migration for the duration of one test and disarms on drop, so a
/// test that panics part-way cannot leave every mint after it racing a drop —
/// the value is process-wide, on the same terms as `terminal`'s test hooks.
///
/// Disarming is not the only thing keeping tests apart: the hook below acts only
/// on the database named here, so a mint in a test running beside this one — on
/// its own temporary file — is not touched even while this is armed.
#[cfg(test)]
struct MigrationMidMint;

#[cfg(test)]
impl MigrationMidMint {
    fn armed_on(path: &Path) -> MigrationMidMint {
        *TEST_MIGRATION_MID_MINT.lock().unwrap() = Some(path.to_path_buf());
        MigrationMidMint
    }
}

#[cfg(test)]
impl Drop for MigrationMidMint {
    fn drop(&mut self) {
        *TEST_MIGRATION_MID_MINT.lock().unwrap() = None;
    }
}

/// Stand in for the second daemon reaching `ALTER TABLE … DROP COLUMN` between
/// the mint's probe and the `INSERT` the probe chose.
///
/// A fresh connection, because that is what a second process is, and with the
/// busy timeout at zero so the refusal is instant rather than five seconds of
/// waiting on a lock the caller itself holds. What SQLite said is deliberately
/// discarded: whether the drop lands is the property under test, not a fact this
/// hook is entitled to assert.
#[cfg(test)]
fn test_second_daemon_drops_allow_ssh(conn: &Connection) {
    let Some(armed) = TEST_MIGRATION_MID_MINT.lock().unwrap().clone() else {
        return;
    };
    // Through `canonicalize`, because SQLite reports the name it resolved the
    // file to and a test hands over the name it opened: on macOS those are
    // `/private/var/folders/…` and `/var/folders/…`, one file spelled two ways.
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if conn.path().map(|open| real(Path::new(open))) != Some(real(&armed)) {
        return;
    }
    let second = Connection::open(&armed).unwrap();
    second.busy_timeout(std::time::Duration::ZERO).unwrap();
    let _ = second.execute_batch("ALTER TABLE pairing_codes DROP COLUMN allow_ssh;");
}

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
            -- The run's identity: a ULID minted by `codeconnect claude` at spawn, or
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

        -- A deletion is a decision, and this is its record. A uid here was
        -- deliberately erased by an operator; nothing may file anything under
        -- it again. Without this, a hook or registration in flight when the
        -- delete committed would re-insert the row a heartbeat later, and the
        -- user's removal would silently undo itself. Uids are ULIDs and are
        -- never reused, so the table only ever grows by one small row per
        -- deliberate deletion and entries never need expiry.
        CREATE TABLE IF NOT EXISTS deleted_sessions(
            session_uid TEXT PRIMARY KEY,
            deleted_at  TEXT NOT NULL
        );

        -- The same decision at the name level, for runs that have no uid to
        -- tombstone. An adopted session's hooks carry no CodeConnect uid, so a
        -- uid tombstone cannot stop the next hook minting a fresh identity and
        -- putting the row straight back. Removing an adopted run means "stop
        -- observing this conversation": its name is recorded here, ordinary
        -- hooks for it are dropped, and an explicit SessionStart — a resume
        -- announcing itself — clears the entry and re-adopts.
        CREATE TABLE IF NOT EXISTS deleted_names(
            session_id TEXT PRIMARY KEY,
            deleted_at TEXT NOT NULL
        );

        -- Pairing codes are stored hashed. The code is a bearer capability
        -- for five minutes; a database file or a backup that leaked one in
        -- the clear would hand over that capability, and hashing costs
        -- nothing because the lookup is by exact hash anyway.
        CREATE TABLE IF NOT EXISTS pairing_codes(
            code_hash     TEXT PRIMARY KEY,
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
            -- APNs. Null until the phone has been granted notification
            -- permission *and* Apple has issued a token; "registered for push"
            -- and "asked and refused" are both absent here, deliberately, so
            -- nothing infers consent from a row that merely exists.
            push_token        TEXT,
            push_environment  TEXT,
            -- The relay's bearer credential, for a daemon that has no Apple
            -- key of its own and asks CodeConnect's relay to address APNs for
            -- it. Null for the direct path, which is never issued one, and
            -- null for every phone that has not been through the relay's
            -- attestation. **Written and cleared only alongside the token it
            -- was minted against**: the relay checks the credential against
            -- the token in the same request, so a credential left behind by a
            -- rotation is refused for every push while the row still looks
            -- registered.
            push_credential   TEXT
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

/// Columns added to an existing table after the fact.
///
/// Every entry must be nullable or carry a default: SQLite cannot add a `NOT
/// NULL` column without one, and a migration that fails leaves a daemon that
/// cannot open its own database.
const COLUMN_ADDITIONS: &[(&str, &str, &str)] = &[
    ("devices", "push_token", "TEXT"),
    ("devices", "push_environment", "TEXT"),
    ("devices", "push_credential", "TEXT"),
];

/// Repair push tuples a rollback or a pre-tuple build could have left incoherent.
///
/// **Two defects, one transition, and the same safe rule for both.** A tuple is
/// three columns that must move together, and two histories can leave them out
/// of step:
///
///   * A build that carried the credential column wrote `(T1, C1)`. A rollback
///     to a build that predates it updated the token in place — it knows only
///     `push_token` — and left `(T2, C1)`. On the way back up that reads as a
///     live relay registration, but `C1` was minted for `T1` and the relay
///     refuses it on every push. The credential cannot be proven current
///     against its token from inside the database — it is opaque, and there is
///     nothing here to check it against — so the only safe rule is to clear it
///     wherever one is present and let the phone re-register the whole tuple.
///     The token stays: it is Apple's and still valid, and a direct-mode row
///     (no credential) was never at risk and is left alone.
///   * A build that predates [`Store::revoke_device`]'s tuple-clear revoked a
///     row and kept its token, environment and credential. The row never
///     delivers — `push_targets` filters it — but the routing tuple lingers on
///     a device the operator has withdrawn trust from, which is the opposite of
///     what revocation means. The whole tuple goes.
///
/// **Fires once, by the version gate in [`Store::migrate`].** Clearing a live
/// credential is the price of not being able to prove it stale; paying it on
/// every restart would make relay push re-register for ever, so this is not the
/// GLOB fix's always-on shape. At this point in the rollout no phone can obtain
/// a credential yet — the attestation flow is a later phase — so the one-time
/// clear costs nothing real and buys coherence against both rollback paths.
fn normalize_push_tuples(conn: &Connection) -> Result<()> {
    let mixed = conn.execute(
        "UPDATE devices SET push_credential = NULL, push_environment = NULL
          WHERE revoked_at IS NULL AND push_credential IS NOT NULL",
        [],
    )?;
    if mixed > 0 {
        crate::log_info!(
            "schema: cleared {mixed} relay credential(s) whose token pairing could not be \
             proven current; the phone re-registers the tuple on next contact"
        );
    }
    let revoked = conn.execute(
        "UPDATE devices SET push_token = NULL, push_environment = NULL, push_credential = NULL
          WHERE revoked_at IS NOT NULL
            AND (push_token IS NOT NULL OR push_credential IS NOT NULL)",
        [],
    )?;
    if revoked > 0 {
        crate::log_info!(
            "schema: cleared the push tuple on {revoked} revoked device row(s) a prior build left \
             addressable"
        );
    }
    Ok(())
}

/// The entries of [`COLUMN_ADDITIONS`] this database is still missing.
///
/// A table that does not exist contributes nothing: `create_schema` builds it
/// at the current shape, so there is no column to add afterwards.
fn missing_columns(conn: &Connection) -> Result<Vec<(&'static str, &'static str, &'static str)>> {
    let mut out = Vec::new();
    for (table, column, kind) in COLUMN_ADDITIONS {
        if table_exists(conn, table)? && !column_exists(conn, table, column)? {
            out.push((*table, *column, *kind));
        }
    }
    Ok(out)
}

fn needs_column_additions(conn: &Connection) -> Result<bool> {
    Ok(!missing_columns(conn)?.is_empty())
}

/// Widen the tables that predate a column, in one immediate transaction.
///
/// `ALTER TABLE … ADD COLUMN` has no `IF NOT EXISTS`, so a second attempt at a
/// column that is already there fails with a duplicate-column error — which,
/// raised from here, is a daemon that will not open its own database.
fn add_missing_columns(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    // Asked again under the write lock, for the reason spelled out in
    // `migrate_to_session_uids`: two daemons starting at once can both pass the
    // unlocked check, and the one that arrives second must find nothing left to
    // do rather than repeat an `ALTER` the first one already committed.
    let additions = missing_columns(&tx)?;
    if additions.is_empty() {
        crate::log_debug!("another process widened these tables first; nothing to do");
        return Ok(());
    }

    for (table, column, kind) in additions {
        tx.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
            [],
        )?;
        crate::log_info!("schema: added {table}.{column}");
    }

    tx.commit()?;
    Ok(())
}

/// Is there a table by this name?
///
/// Matched without regard to case, like every other name this file reads back
/// out of `sqlite_master`, because `CREATE TABLE IF NOT EXISTS devices` already
/// finds a table called `Devices` and no-ops against it. A binary match would
/// answer "no table" about one SQLite will not create, which is how a table ends
/// up never being widened by [`missing_columns`] and never repaired.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1 COLLATE NOCASE",
        params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Is this name already taken on this table, by a column of any kind?
///
/// Read through [`table_columns_including_generated`], because every caller is
/// asking whether the name is free rather than whether it can be written.
/// `ALTER TABLE … ADD COLUMN` fails with a bare `duplicate column name` against
/// a generated column of that name, and `PRAGMA table_info` cannot see one, so a
/// reader that skipped generated columns would answer "free" about a name that
/// is taken and turn a daemon's startup into an error naming a column the
/// operator can plainly see in the schema.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns_including_generated(conn, table)?
        .iter()
        .any(|name| name == column))
}

/// The columns of a table an `INSERT` can name, in declaration order.
///
/// For an ordinary table those are exactly the columns `PRAGMA table_info`
/// reports: a generated column is an expression over the others and cannot be
/// written, and the pragma leaves it out.
/// [`table_columns_including_generated`] is the reader for the other question,
/// "what does this table hold".
///
/// A table that does not exist has no columns rather than being an error, which
/// is what lets the callers below ask about a legacy database without knowing
/// its shape.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get::<_, String>(1)?);
    }
    Ok(out)
}

/// Every column a table has, in declaration order, generated columns included.
///
/// `PRAGMA table_xinfo` is the pragma that reports them and `table_info` is not,
/// which is the whole reason this reader exists: [`column_exists`] is asking
/// whether a name is free, and a name standing over a generated column is taken
/// however invisible it is to `table_info`.
///
/// A table that does not exist has no columns rather than being an error, on the
/// same terms as [`table_columns`].
fn table_columns_including_generated(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_xinfo({table})"))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get::<_, String>(1)?);
    }
    Ok(out)
}

/// Columns earlier releases of this project declared and this one does not.
///
/// All three are dead weight rather than an obstacle, and a build that never
/// manages to drop one still pairs phones and authenticates them.
/// `devices.ssh_key_installed` carries a default and `devices.ssh_fingerprint`
/// is nullable, so every statement here writes around them.
/// `pairing_codes.allow_ssh` is `NOT NULL` with no default and would break the
/// mint, which is why [`Store::create_pairing_code`] names it explicitly while
/// it is present — a column that cannot be dropped is a cosmetic problem, never
/// a pairing outage.
///
/// Named one by one, never "every column this build does not declare". Someone
/// who runs a newer build and comes back to this one arrives carrying columns
/// this build has never heard of, and reading those as legacy would destroy the
/// newer build's data on the way down and hand back an empty column on the way
/// up. A retired name is a fact this project owns; an unrecognised one is a fact
/// about this build's age.
const RETIRED_COLUMNS: &[(&str, &str)] = &[
    ("devices", "ssh_key_installed"),
    ("devices", "ssh_fingerprint"),
    ("pairing_codes", "allow_ssh"),
];

/// The entries of [`RETIRED_COLUMNS`] this database still carries.
///
/// Matched without regard to ASCII case, the way SQLite matches a column name:
/// `ALLOW_SSH` and `allow_ssh` are one column to every statement that reads the
/// table, so a binary match would leave a column this project retired in place.
/// A table that does not exist reports no columns and contributes nothing.
fn retired_columns_present(conn: &Connection) -> Result<Vec<(&'static str, &'static str)>> {
    let mut present = Vec::new();
    for (table, column) in RETIRED_COLUMNS {
        if table_columns(conn, table)?
            .iter()
            .any(|actual| actual.eq_ignore_ascii_case(column))
        {
            present.push((*table, *column));
        }
    }
    Ok(present)
}

/// One `ALTER TABLE … DROP COLUMN`, answered by the table rather than by the
/// error text.
///
/// Both identifiers are literals from [`RETIRED_COLUMNS`], so there is nothing
/// in the formatted statement for a name to escape out of. What the statement
/// says when it fails cannot be read, though: a drop an index blocks reports
/// `error in index … after drop column: no such column: allow_ssh`, in the same
/// words a drop of an already-absent column reports, and reading one as the
/// other either leaves a `NOT NULL` column in place or takes a table apart that
/// did not need it. Asking the table afterwards is the reading that cannot be
/// confused: gone is done, however it went.
fn drop_column(conn: &Connection, table: &str, column: &str) -> Result<()> {
    if let Err(err) = conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column};")) {
        if table_columns(conn, table)?
            .iter()
            .any(|actual| actual.eq_ignore_ascii_case(column))
        {
            return Err(err.into());
        }
    }
    Ok(())
}

/// Take the retired columns off the tables that still carry them.
///
/// `ALTER TABLE … DROP COLUMN` is the whole mechanism. It removes one column and
/// reproduces nothing else, so every other column keeps the type and the
/// constraints it was declared with, every row keeps its values, every index,
/// trigger and view that does not depend on the column stays, and a column a
/// newer build added is never so much as looked at. Nothing is written when
/// nothing is retired — the common case, which is every boot after the first.
///
/// SQLite refuses the drop when the column is a `PRIMARY KEY`, is `UNIQUE`, is
/// indexed, or is named by a generated column, a `CHECK`, a trigger or a view.
/// Every one of those is a fact about the schema, never about the rows, so there
/// is no second attempt worth making: the column is left where it is and a
/// warning names the table, the column and what SQLite said. That is survivable
/// on all three — the two on `devices` because they are nullable or defaulted,
/// `pairing_codes.allow_ssh` because [`Store::create_pairing_code`] fills it
/// while it is there. Nothing is deleted and no table is rebuilt to force a
/// removal through: a column left behind costs an operator one `DROP INDEX` and
/// a restart, and there is no cost that a build is entitled to pay with somebody
/// else's rows or with a newer build's columns.
///
/// One immediate transaction, so two daemons starting at once serialize and the
/// second finds nothing left to do, and a `kill -9` in the middle leaves the old
/// schema intact for the next start to try again. Returns one message per column
/// that survived, already logged, so a caller can assert on what an operator was
/// told.
fn drop_retired_columns(conn: &mut Connection) -> Result<Vec<String>> {
    if retired_columns_present(conn)?.is_empty() {
        return Ok(Vec::new());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut warnings = Vec::new();
    // Asked again under the write lock, so the daemon that lost a startup race
    // reports the work as done rather than reporting it twice. What makes that
    // race *safe* is [`drop_column`], which reads a column that is already gone
    // as gone whoever took it.
    for (table, column) in retired_columns_present(&tx)? {
        let Err(err) = drop_column(&tx, table, column) else {
            crate::log_info!("schema: dropped the retired column {table}.{column}");
            continue;
        };
        let warning = format!(
            "schema: SQLite will not drop the retired column {table}.{column} ({err}), so it \
             stays. The daemon serves normally with it in place — nothing this build reads names \
             it, and minting a pairing code fills it explicitly while it is there. To be rid of \
             it, drop whatever depends on {table}.{column} (an index, a view, a trigger, a CHECK \
             or a generated column — the error above names it) and restart."
        );
        crate::log_warn!("{warning}");
        warnings.push(warning);
    }
    tx.commit()?;
    Ok(warnings)
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
            SELECT m.session_uid, m.session_id,
                   -- An adopted name (`claude:*`, cc-hook's prefix for sessions
                   -- CodeConnect did not launch) gets no tmux location: nothing
                   -- here ever put it in tmux, and a fabricated one is what let
                   -- the liveness sweep mark live adopted runs dead. The
                   -- startup normalizer enforces the same rule for rows written
                   -- before it existed; this keeps a legacy rebuild from
                   -- re-fabricating what it just cleaned.
                   CASE WHEN m.session_id GLOB 'claude:*' THEN '' ELSE m.session_id END,
                   CASE WHEN m.session_id GLOB 'claude:*' THEN '' ELSE ?1 END,
                   '',
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

    // A fact for a session that is gone is dropped, not filed. A hook or an
    // approval already in flight when the phone's delete commits would
    // otherwise write rows keyed to a uid nothing can name again — the same
    // orphan shape `append_batch_with_cursor` refuses, enforced here so every
    // single-event path shares the rule. `None` is the shape callers already
    // tolerate for a dedup miss.
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_uid = ?1)",
        params![pending.session_uid],
        |row| row.get(0),
    )?;
    if !known {
        return Ok(None);
    }

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
    })
}

pub(crate) fn lifecycle_str(lifecycle: Lifecycle) -> &'static str {
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

    // ----------------------------------------------------------- pruning

    /// Seed one run in a given state, with `events` facts filed under it.
    fn seed_run(store: &Store, session: &SessionKey, lifecycle: Lifecycle, events: u32) {
        let mut row = session_row(session);
        row.lifecycle = lifecycle;
        store.upsert_session(&row).unwrap().assert_present();
        for i in 0..events {
            store
                .append_event(&pending(
                    session,
                    EventKind::ToolCall,
                    Some(&format!("{}-{i}", session.uid)),
                ))
                .unwrap();
        }
    }

    #[test]
    fn pruning_removes_ended_runs_and_everything_filed_under_them() {
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        seed_run(&store, &dead, Lifecycle::Exited, 5);

        let removed = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].session_uid, dead.uid);
        assert_eq!(
            removed[0].events, 5,
            "the report has to say what went, because afterwards there is nothing to check"
        );
        assert!(store.get_session(&dead.uid).unwrap().is_none());
        assert_eq!(store.count_events(&dead.uid).unwrap(), 0);
    }

    #[test]
    fn pruning_refuses_to_touch_anything_that_has_not_ended() {
        // The safety property. `live` is obvious; `unknown` is the sharp one —
        // it means the daemon could not establish what happened to this run,
        // and that is precisely when the record is the only evidence there is.
        let (store, _path) = temp_store();
        let live = key("AA", "cc-1");
        let unsure = key("BB", "cc-2");
        let spawning = key("CC", "cc-3");
        let dead = key("DD", "cc-4");
        seed_run(&store, &live, Lifecycle::Live, 2);
        seed_run(&store, &unsure, Lifecycle::Unknown, 2);
        seed_run(&store, &spawning, Lifecycle::Spawning, 2);
        seed_run(&store, &dead, Lifecycle::Exited, 2);

        let removed = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].session_uid, dead.uid);
        for survivor in [&live, &unsure, &spawning] {
            assert!(
                store.get_session(&survivor.uid).unwrap().is_some(),
                "{} was removed and should not have been",
                survivor.uid
            );
            assert_eq!(store.count_events(&survivor.uid).unwrap(), 2);
        }
    }

    // ------------------------------------------------ deleting exactly one

    /// The safety property of the phone's swipe, stated at the level that
    /// enforces it. The daemon checks the lifecycle too, but this is the check
    /// that cannot be forgotten by a caller: it is in the `WHERE` clause.
    #[test]
    fn a_live_run_cannot_be_deleted_however_it_is_asked_for() {
        let (store, _path) = temp_store();
        let live = key("AA", "cc-1");
        let unsure = key("BB", "cc-2");
        seed_run(&store, &live, Lifecycle::Live, 2);
        seed_run(&store, &unsure, Lifecycle::Unknown, 2);

        // The two are asserted separately because the *word* differs, and that
        // is the point: `Unknown` means the daemon could not establish what
        // happened to this run, which is not "still running" and must not be
        // reported as it.
        for (session, expected) in [(&live, Lifecycle::Live), (&unsure, Lifecycle::Unknown)] {
            assert_eq!(
                store.delete_exited_session(&session.uid).unwrap(),
                DeleteOutcome::NotExited {
                    lifecycle: lifecycle_str(expected).into()
                },
                "{} is not exited and must survive being asked for by uid",
                session.uid
            );
            assert!(store.get_session(&session.uid).unwrap().is_some());
            assert_eq!(store.count_events(&session.uid).unwrap(), 2);
        }
    }

    #[test]
    fn deleting_one_ended_run_takes_its_rows_and_leaves_its_neighbours() {
        let (store, _path) = temp_store();
        let doomed = key("AA", "cc-1");
        let neighbour = key("BB", "cc-2");
        seed_run(&store, &doomed, Lifecycle::Exited, 5);
        seed_run(&store, &neighbour, Lifecycle::Exited, 4);

        assert_eq!(
            store.delete_exited_session(&doomed.uid).unwrap(),
            DeleteOutcome::Deleted { events: 5 },
            "the count is reported because afterwards there is nothing left to count"
        );
        assert!(store.get_session(&doomed.uid).unwrap().is_none());
        assert_eq!(store.count_events(&doomed.uid).unwrap(), 0);

        // The other ended run is the point: this deletes one, not a class.
        assert!(store.get_session(&neighbour.uid).unwrap().is_some());
        assert_eq!(store.count_events(&neighbour.uid).unwrap(), 4);
    }

    /// **The write that arrives after the delete.** Ending a run stops its tail
    /// by queueing a command, so one more transcript poll can already be under
    /// way when the phone removes the run. Landing it afterwards would file
    /// events and a cursor under a `session_uid` with no row — orphans of
    /// exactly the kind `SESSION_SCOPED_TABLES` exists to prevent, and which
    /// nothing would ever clean up because no session names them.
    #[test]
    fn a_transcript_batch_for_a_deleted_run_is_refused_entirely() {
        let (store, _path) = temp_store();
        let gone = key("AA", "cc-1");
        seed_run(&store, &gone, Lifecycle::Exited, 2);
        let cursor = TailCursor {
            path: "/tmp/x.jsonl".into(),
            dev: 1,
            ino: 2,
            offset: 4096,
            last_line_start: 4000,
            last_line_sha: "abc".into(),
        };

        assert_eq!(
            store.delete_exited_session(&gone.uid).unwrap(),
            DeleteOutcome::Deleted { events: 2 }
        );

        let late = [pending(&gone, EventKind::ToolCall, Some("late-1"))];
        let written = store
            .append_batch_with_cursor(&gone.uid, &late, &cursor)
            .unwrap();

        assert!(
            written.is_empty(),
            "nothing may be written for a run that is gone"
        );
        assert_eq!(store.count_events(&gone.uid).unwrap(), 0);
        assert!(
            store.load_cursor(&gone.uid).unwrap().is_none(),
            "and the cursor must not come back either — it is a session-scoped row like any other"
        );
    }

    /// The daemon's own delete rule, stated as a matrix. Hosted rows need the
    /// proof; unhosted rows (empty socket — nothing here ever put them in tmux)
    /// are removable at any lifecycle, because the proof cannot exist.
    #[test]
    fn an_unhosted_row_is_deletable_at_any_lifecycle_and_a_hosted_one_is_not() {
        let (store, _path) = temp_store();
        for (i, lifecycle) in [Lifecycle::Live, Lifecycle::Unknown, Lifecycle::Exited]
            .into_iter()
            .enumerate()
        {
            let unhosted = key(&format!("A{i}"), &format!("claude:conv-{i}"));
            let mut row = session_row(&unhosted);
            row.lifecycle = lifecycle;
            row.tmux_session = String::new();
            row.tmux_socket = String::new();
            store.upsert_session(&row).unwrap().assert_present();
            assert_eq!(
                store.delete_exited_session(&unhosted.uid).unwrap(),
                DeleteOutcome::Deleted { events: 0 },
                "an unhosted {lifecycle:?} row must be removable"
            );
        }
    }

    /// Deleted means deleted: the tombstone outlives the row, the upsert that
    /// raced the delete writes nothing, and no fact can be filed under the uid.
    #[test]
    fn a_deleted_uid_can_never_be_recreated_or_written_to() {
        let (store, _path) = temp_store();
        let doomed = key("AA", "cc-1");
        seed_run(&store, &doomed, Lifecycle::Exited, 1);
        assert_eq!(
            store.delete_exited_session(&doomed.uid).unwrap(),
            DeleteOutcome::Deleted { events: 1 }
        );

        // The exact shape of `ensure_session`'s second half after losing the race.
        assert_eq!(
            store.upsert_session(&session_row(&doomed)).unwrap(),
            SessionUpsert::Tombstoned,
            "an upsert that raced the delete must write nothing"
        );
        assert!(store.get_session(&doomed.uid).unwrap().is_none());
        assert!(
            store
                .append_event(&pending(&doomed, EventKind::ToolCall, Some("late")))
                .unwrap()
                .is_none(),
            "no fact may be filed under a deleted uid"
        );
    }

    /// Prune is the same decision at bulk, so it leaves the same record.
    #[test]
    fn a_pruned_uid_is_tombstoned_like_a_swiped_one() {
        let (store, _path) = temp_store();
        let doomed = key("AA", "cc-1");
        seed_run(&store, &doomed, Lifecycle::Exited, 1);
        assert_eq!(store.prune_exited_sessions(&[], false).unwrap().len(), 1);
        assert_eq!(
            store.upsert_session(&session_row(&doomed)).unwrap(),
            SessionUpsert::Tombstoned
        );
        // And a dry run decides nothing, so it records nothing.
        let spared = key("BB", "cc-2");
        seed_run(&store, &spared, Lifecycle::Exited, 1);
        assert_eq!(store.prune_exited_sessions(&[], true).unwrap().len(), 1);
        store
            .upsert_session(&session_row(&spared))
            .unwrap()
            .assert_present();
    }

    /// The startup normalizer: adopted rows lose their fabricated location,
    /// exactly once each, case-sensitively, and nothing else moves.
    #[test]
    fn the_normalizer_clears_adopted_locations_and_only_those() {
        let (store, path) = temp_store();
        let adopted = key("AA", "claude:8f37b678");
        let hosted = key("BB", "cc-1");
        let decoy = key("CC", "Claude:shout");
        for k in [&adopted, &hosted, &decoy] {
            store
                .upsert_session(&session_row(k))
                .unwrap()
                .assert_present();
        }
        drop(store);
        // A second open runs the normalizer against the rows above.
        let store = Store::open(&path).unwrap();
        let cleared = store.get_session(&adopted.uid).unwrap().unwrap();
        assert_eq!(cleared.tmux_session, "");
        assert_eq!(cleared.tmux_socket, "");
        assert_eq!(
            cleared.lifecycle,
            Lifecycle::Live,
            "lifecycle is not its business"
        );
        let kept = store.get_session(&hosted.uid).unwrap().unwrap();
        assert_eq!(kept.tmux_session, "cc-1", "hosted rows keep their location");
        let case = store.get_session(&decoy.uid).unwrap().unwrap();
        assert_eq!(
            case.tmux_session, "Claude:shout",
            "the prefix is an exact contract; GLOB must not match a different case"
        );
    }

    /// Two phones swiping the same row is a race, not an error. The second one
    /// must get an answer it can act on rather than a failure it has to explain.
    #[test]
    fn deleting_something_already_gone_is_not_an_error() {
        let (store, _path) = temp_store();
        let gone = key("AA", "cc-1");
        seed_run(&store, &gone, Lifecycle::Exited, 1);

        assert_eq!(
            store.delete_exited_session(&gone.uid).unwrap(),
            DeleteOutcome::Deleted { events: 1 }
        );
        assert_eq!(
            store.delete_exited_session(&gone.uid).unwrap(),
            DeleteOutcome::NotFound
        );
        assert_eq!(
            store
                .delete_exited_session("01NOSUCHRUNATALLXXXXXXXXXX")
                .unwrap(),
            DeleteOutcome::NotFound
        );
    }

    #[test]
    fn a_protected_run_survives_even_though_its_row_says_it_ended() {
        // The daemon's veto, for runs it still holds live state for — a
        // supervisor attached, an approval open. Both should be impossible for
        // a row marked `Exited`, which is exactly why it is checked: if one
        // ever happens it is a bug, and deleting the evidence is the worst
        // available response to a bug.
        let (store, _path) = temp_store();
        let held = key("AA", "cc-1");
        let free = key("BB", "cc-2");
        seed_run(&store, &held, Lifecycle::Exited, 3);
        seed_run(&store, &free, Lifecycle::Exited, 3);

        let removed = store
            .prune_exited_sessions(std::slice::from_ref(&held.uid), false)
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].session_uid, free.uid);
        assert!(store.get_session(&held.uid).unwrap().is_some());
        assert_eq!(store.count_events(&held.uid).unwrap(), 3);
    }

    #[test]
    fn a_dry_run_reports_exactly_what_would_go_and_removes_none_of_it() {
        // The rehearsal has to be trustworthy in both directions: it must
        // report the same set the real thing would remove, and it must leave
        // every byte of it in place.
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        let live = key("BB", "cc-2");
        seed_run(&store, &dead, Lifecycle::Exited, 4);
        seed_run(&store, &live, Lifecycle::Live, 4);

        let rehearsal = store.prune_exited_sessions(&[], true).unwrap();
        assert_eq!(rehearsal.len(), 1);
        assert_eq!(rehearsal[0].events, 4);
        assert!(store.get_session(&dead.uid).unwrap().is_some());
        assert_eq!(store.count_events(&dead.uid).unwrap(), 4);

        let real = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(real, rehearsal, "the rehearsal must predict the deletion");
        assert!(store.get_session(&dead.uid).unwrap().is_none());
    }

    #[test]
    fn pruning_leaves_no_row_behind_in_any_table_keyed_by_a_run() {
        // An orphan here is not tidiness. `answer_claims` outliving its run
        // would be recovered at the next startup as an indeterminate answer for
        // a session nobody can name, and `pending_approvals` would restore a
        // card for a run that no longer exists.
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        seed_run(&store, &dead, Lifecycle::Exited, 2);

        let outcome = AnswerOutcome {
            request_id: "toolu_1".into(),
            session_id: dead.name.clone(),
            decision: AnswerDecision::Allow,
            resolved_by: ResolvedBy::Phone,
            applied_via: AnswerPath::SendKeys,
            resolved_at: protocol::time::now_rfc3339(),
            detail: None,
            inferred: false,
            indeterminate: false,
        };
        store
            .record_answer(&dead.uid, "toolu_1", "hash", &outcome)
            .unwrap();
        store
            .upsert_pending_approval(&PendingApprovalRow {
                session_uid: dead.uid.clone(),
                session_id: dead.name.clone(),
                request_id: "toolu_2".into(),
                card: "{}".into(),
                generation: 1,
                created_ms: 0,
            })
            .unwrap();
        store
            .claim_answer(&AnswerClaim {
                session_uid: dead.uid.clone(),
                session_id: dead.name.clone(),
                request_id: "toolu_3".into(),
                payload_hash: "hash".into(),
                decision: "\"allow\"".into(),
                started_at: protocol::time::now_rfc3339(),
            })
            .unwrap();
        store
            .claim_text_mutation(&dead.uid, "toolu_4", "hash", &protocol::time::now_rfc3339())
            .unwrap();
        // The cursor is only written alongside a transcript batch, which is the
        // invariant that keeps consuming lines and recording that they were
        // consumed in one transaction.
        store
            .append_batch_with_cursor(
                &dead.uid,
                &[pending(&dead, EventKind::AgentMessage, Some("tail-1"))],
                &TailCursor {
                    path: "/tmp/t.jsonl".into(),
                    dev: 1,
                    ino: 1,
                    offset: 1,
                    last_line_start: 0,
                    last_line_sha: "x".into(),
                },
            )
            .unwrap();
        assert!(store.load_cursor(&dead.uid).unwrap().is_some());

        store.prune_exited_sessions(&[], false).unwrap();

        let conn = store.read();
        for table in SESSION_SCOPED_TABLES {
            let left: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_uid = ?1"),
                    params![dead.uid],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{table} kept rows for a run that was removed");
        }
    }

    #[test]
    fn every_session_scoped_table_is_named_in_the_prune_list() {
        // Read back off the schema, so a table added later cannot silently
        // start leaving orphans. The failure mode this prevents is invisible by
        // nature: nothing breaks at the moment of the prune, and the stale rows
        // surface much later as a recovered claim for a session that is gone.
        let (store, _path) = temp_store();
        let conn = store.read();
        let mut tables = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let names: Vec<String> = tables
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        drop(tables);

        for name in names {
            if name == "sessions" || name.starts_with("sqlite_") {
                continue;
            }
            // The one deliberate exception: tombstones are the record that a
            // deletion happened, so they are precisely the rows that must
            // SURVIVE the deletion they describe.
            if name == "deleted_sessions" || name == "deleted_names" {
                continue;
            }
            let mut info = conn.prepare(&format!("PRAGMA table_info({name})")).unwrap();
            let has_uid = info
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .any(|column| column.as_deref() == Ok("session_uid"));
            drop(info);
            if has_uid {
                assert!(
                    SESSION_SCOPED_TABLES.contains(&name.as_str()),
                    "{name} is keyed by session_uid but is not pruned with its session"
                );
            }
        }
    }

    #[test]
    fn events_with_no_session_row_are_counted_and_never_swept_up() {
        // Found on the owner's real database while verifying the prune: 24
        // events, one answer and one tail cursor belonging to three runs with
        // no `sessions` row. They predate any prune — the same counts appear in
        // a backup taken beforehand — and nothing in the product reaches them.
        // The prune must neither miss that they exist nor take it upon itself
        // to delete data whose provenance nobody understands.
        let (store, _path) = temp_store();
        let ghost = key("AA", "cc-gone");
        // Raw SQL, deliberately: `append_event` now refuses to create exactly
        // this shape (a fact with no session row), which is the fix — so the
        // legacy orphans this test is about have to be manufactured the way
        // they actually arose, by writes that predate the guard.
        {
            let conn = store.write();
            conn.execute(
                "INSERT INTO events(session_uid, session_id, seq, ts, kind, payload, source)
                 VALUES(?1, ?2, 1, ?3, 'tool_call', '{}', 'hook')",
                params![ghost.uid, ghost.name, protocol::time::now_rfc3339()],
            )
            .unwrap();
        }
        let dead = key("BB", "cc-1");
        seed_run(&store, &dead, Lifecycle::Exited, 2);

        assert_eq!(store.orphan_event_count().unwrap(), 1);
        let removed = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(removed.len(), 1, "only the run with a row is a candidate");
        assert_eq!(
            store.orphan_event_count().unwrap(),
            1,
            "an orphan must survive a prune rather than be quietly swept up"
        );
        assert_eq!(store.count_events(&ghost.uid).unwrap(), 1);
    }

    #[test]
    fn pruning_an_empty_or_all_live_log_is_a_no_op_rather_than_an_error() {
        let (store, _path) = temp_store();
        assert!(store.prune_exited_sessions(&[], false).unwrap().is_empty());
        seed_run(&store, &key("AA", "cc-1"), Lifecycle::Live, 1);
        assert!(store.prune_exited_sessions(&[], false).unwrap().is_empty());
    }

    #[test]
    fn a_run_that_ends_later_is_prunable_and_the_survivors_keep_their_own_log() {
        // Name reuse, which is the case a machine with a long history is full
        // of: several dead `cc-1`s and one that is still going. Removing the
        // dead ones must not disturb the live one's sequence.
        let (store, _path) = temp_store();
        let live = key("AA", "cc-1");
        seed_run(&store, &live, Lifecycle::Live, 3);
        for tag in ["BB", "CC", "DD"] {
            seed_run(&store, &key(tag, "cc-1"), Lifecycle::Exited, 2);
        }

        let removed = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(removed.len(), 3);
        assert_eq!(store.max_seq(&live.uid).unwrap(), 3);
        assert_eq!(store.count_events(&live.uid).unwrap(), 3);
        assert_eq!(store.list_sessions().unwrap().len(), 1);
    }

    #[test]
    fn seq_is_monotonic_and_per_run() {
        let (store, _path) = temp_store();
        let first = key("AA", "cc-1");
        store
            .upsert_session(&session_row(&first))
            .unwrap()
            .assert_present();
        for expected in 1..=3u64 {
            let event = store
                .append_event(&pending(&first, EventKind::ToolCall, None))
                .unwrap()
                .unwrap();
            assert_eq!(event.seq, expected);
        }
        // A second session starts its own numbering.
        let second = key("AB", "cc-2");
        store
            .upsert_session(&session_row(&second))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&dead))
            .unwrap()
            .assert_present();
        let live = key("AB", "cc-1");
        store
            .upsert_session(&session_row(&live))
            .unwrap()
            .assert_present();
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
        store.upsert_session(&row).unwrap().assert_present();

        let mut row = session_row(&new);
        row.created_at = "2026-07-02T00:00:00.000Z".into();
        row.lifecycle = Lifecycle::Exited;
        store.upsert_session(&row).unwrap().assert_present();

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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&dead))
            .unwrap()
            .assert_present();
        let live = key("AB", "cc-1");
        store
            .upsert_session(&session_row(&live))
            .unwrap()
            .assert_present();

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
        store
            .upsert_session(&session_row(&old))
            .unwrap()
            .assert_present();
        let new = key("AB", "cc-1");
        store
            .upsert_session(&session_row(&new))
            .unwrap()
            .assert_present();
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
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store.upsert_session(&row).unwrap().assert_present();

        // SessionStart teaches us the transcript path.
        row.transcript_path = Some("/tmp/x.jsonl".into());
        row.claude_session_id = Some("uuid-1".into());
        row.lifecycle = Lifecycle::Live;
        store.upsert_session(&row).unwrap().assert_present();

        // A later heartbeat knows neither; they must not be erased.
        row.transcript_path = None;
        row.claude_session_id = None;
        store.upsert_session(&row).unwrap().assert_present();

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
        store
            .upsert_session(&session_row(&first))
            .unwrap()
            .assert_present();
        store
            .upsert_session(&session_row(&second))
            .unwrap()
            .assert_present();
        let rows = store.list_sessions().unwrap();
        assert_eq!(rows.len(), 2, "the second run must not replace the first");
        assert!(rows.iter().all(|r| r.session_id == "cc-1"));
    }

    fn mint(store: &Store, code_hash: &str, ttl_ms: i64) -> i64 {
        let now = protocol::time::now_unix_ms();
        let expires = now + ttl_ms;
        store
            .create_pairing_code(
                code_hash,
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
        let now = mint(&store, "hash-a", 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-a", now).unwrap(),
            PairingConsume::Consumed
        );
        // The second scan of the same screen must not pair a second device.
        assert_eq!(
            store.consume_pairing_code("hash-a", now).unwrap(),
            PairingConsume::AlreadyUsed
        );
        // And once the same code is also past its expiry, it is still
        // "already used". The two answers are ordered on purpose: having been
        // used is the fact that changed something, and it does not stop being
        // true when the clock passes. Answering "expired" here would report a
        // code that quietly expired over one that paired a device.
        assert_eq!(
            store.consume_pairing_code("hash-a", now + 600_000).unwrap(),
            PairingConsume::AlreadyUsed
        );
    }

    #[test]
    fn an_expired_code_is_refused_even_though_it_exists() {
        let (store, _path) = temp_store();
        let now = mint(&store, "hash-b", 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-b", now + 300_001).unwrap(),
            PairingConsume::Expired
        );
        // The boundary itself, which is the whole of what the comparison
        // decides: a code is live through the millisecond it expires on, and
        // one millisecond later it is not. Asked in this order because only the
        // living one changes anything.
        assert_eq!(
            store.consume_pairing_code("hash-b", now + 300_000).unwrap(),
            PairingConsume::Consumed,
            "the millisecond a code expires on is still inside its life"
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
    fn minting_sweeps_codes_that_can_never_be_used_again() {
        let (store, _path) = temp_store();
        let now = protocol::time::now_unix_ms();
        store
            .create_pairing_code("old", "t", now - 10_000, now)
            .unwrap();
        // A later mint sweeps anything already past its expiry.
        mint(&store, "fresh", 300_000);
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
            .revoke_device("d1", "2026-07-31T10:00:00.000Z")
            .unwrap();

        let listed = &store.list_devices().unwrap()[0];
        assert_eq!(
            listed.revoked_at.as_deref(),
            Some("2026-07-31T10:00:00.000Z")
        );
        assert!(!listed.to_summary().is_active());
    }

    #[test]
    fn devices_and_pairing_survive_a_restart() {
        let (store, path) = temp_store();
        add_device(&store, "d1", "iPhone", "t1");
        let now = mint(&store, "hash-r", 300_000);
        drop(store);

        let store = Store::open(&path).unwrap();
        assert!(store.device_by_token_hash("t1").unwrap().is_some());
        assert_eq!(
            store.consume_pairing_code("hash-r", now).unwrap(),
            PairingConsume::Consumed
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
        // The row first: a batch for a session that is not in `sessions` is
        // refused, so that a tail poll still in flight when a run is deleted
        // cannot file events — or a cursor — under a uid nobody can name.
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        // `codeconnect claude --resume` in a reused name points a *new* run at the same
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
    /// index names, frozen here because reading that shape is the migration's
    /// whole job.
    ///
    /// `devices` and `pairing_codes` carry the real SSH columns an earlier
    /// schema declares and this one does not, so these tests exercise the
    /// migration a user's own database gets rather than a generic one.
    /// `pairing_codes.allow_ssh` is `NOT NULL` with no default, so an `INSERT`
    /// that neither drops it nor names it fails — the constraint the mint has to
    /// cope with, reproduced here exactly.
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

    /// The retired list names nothing this schema declares.
    ///
    /// [`RETIRED_COLUMNS`] is a list of names to destroy, held apart from the
    /// DDL that builds the tables. A name on both lists would take a live column
    /// off every database this build opens, and nobody would find out until a
    /// phone could not authenticate.
    ///
    /// Held against `create_schema`'s own output rather than against a database
    /// `Store::open` produced: opening runs the removal, so a live name on the
    /// list would already be gone by the time such a database was asked, and the
    /// assertion would agree with any answer.
    #[test]
    fn the_retired_list_names_nothing_this_schema_declares() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        for (table, column) in RETIRED_COLUMNS {
            assert!(
                !table_columns(&conn, table)
                    .unwrap()
                    .iter()
                    .any(|actual| actual.eq_ignore_ascii_case(column)),
                "{table}.{column} is both declared and retired"
            );
        }
        assert!(retired_columns_present(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_legacy_credential_table_gives_up_its_retired_columns_without_losing_a_device() {
        // A device row *is* a phone's pairing: the token hash is the only copy
        // of that credential the Mac holds, and dropping it un-pairs a phone
        // that has no way to find out until it next tries to connect.
        let path = legacy_path();
        {
            let conn = legacy_database(&path);
            conn.execute(
                "INSERT INTO devices(device_id, name, token_hash, created_at, last_seen_at,
                                     revoked_at, ssh_key_installed, ssh_fingerprint)
                 VALUES('d1','iPhone','hash-a','2026-08-01T00:00:00.000Z',
                        '2026-08-02T00:00:00.000Z', NULL, 1, 'SHA256:abc')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pairing_codes(code_hash, allow_ssh, created_at, expires_at,
                                           expires_at_ms, consumed_at)
                 VALUES('hash-c', 1, 'c', 'e', 9_000_000_000_000, NULL)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();

        let device = store
            .device_by_token_hash("hash-a")
            .unwrap()
            .expect("the device token must still authenticate");
        assert_eq!(device.device_id, "d1");
        assert_eq!(device.name, "iPhone");
        assert_eq!(device.created_at, "2026-08-01T00:00:00.000Z");
        assert_eq!(
            device.last_seen_at.as_deref(),
            Some("2026-08-02T00:00:00.000Z")
        );
        assert!(device.revoked_at.is_none());

        // The in-flight code came across and is still redeemable exactly once.
        assert_eq!(
            store.consume_pairing_code("hash-c", 0).unwrap(),
            PairingConsume::Consumed
        );

        // And the migrated tables take this build's own writes, which name
        // neither retired column.
        mint(&store, "hash-new", 300_000);
        store
            .insert_device("d2", "iPad", "hash-b", "2026-08-03T00:00:00.000Z")
            .unwrap();
        assert_eq!(store.list_devices().unwrap().len(), 2);

        drop(store);
        let conn = Connection::open(&path).unwrap();
        assert!(retired_columns_present(&conn).unwrap().is_empty());
        // The declared index is on the table afterwards. `DROP COLUMN` touches
        // nothing but the column it names, and nothing here keys on one that is
        // going, so the index is never disturbed.
        assert!(index_names(&conn, "devices").contains(&"devices_name".to_string()));
    }

    #[test]
    fn widening_a_legacy_table_twice_is_a_no_op() {
        let path = legacy_path();
        {
            let conn = legacy_database(&path);
            conn.execute(
                "INSERT INTO devices(device_id, name, token_hash, created_at, last_seen_at,
                                     revoked_at, ssh_key_installed, ssh_fingerprint)
                 VALUES('d1','iPhone','hash-a','2026-08-01T00:00:00.000Z', NULL, NULL, 0, NULL)",
                [],
            )
            .unwrap();
            assert!(
                needs_column_additions(&conn).unwrap(),
                "a database written before push has none of the push columns"
            );
        }
        drop(Store::open(&path).unwrap());

        let mut conn = Connection::open(&path).unwrap();
        assert!(!needs_column_additions(&conn).unwrap());
        // The state a second `ccd` sees when it passes the unlocked check and
        // then reaches the transaction after the first one committed. `ALTER
        // TABLE … ADD COLUMN` has no `IF NOT EXISTS`, so the recheck taken
        // under the write lock is the whole difference between this and a
        // duplicate-column error that stops the daemon from opening.
        add_missing_columns(&mut conn).expect("a redundant widening must not error");
        assert!(column_exists(&conn, "devices", "push_token").unwrap());
        assert!(column_exists(&conn, "devices", "push_environment").unwrap());
        assert!(column_exists(&conn, "devices", "push_credential").unwrap());
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_devices().unwrap().len(), 1);
    }

    /// The second start finds nothing retired, and does not open a transaction
    /// to establish it.
    ///
    /// Another connection holds the write lock throughout, so a pass that
    /// reached `BEGIN IMMEDIATE` would come back `database is locked` rather
    /// than come back empty. Being invisible when there is nothing to do is the
    /// point: the common case is every boot after the first.
    #[test]
    fn a_second_pass_over_a_clean_database_writes_nothing() {
        let path = legacy_path();
        {
            let conn = legacy_database(&path);
            conn.execute(
                "INSERT INTO devices(device_id, name, token_hash, created_at, last_seen_at,
                                     revoked_at, ssh_key_installed, ssh_fingerprint)
                 VALUES('d1','iPhone','hash-a','2026-08-01T00:00:00.000Z', NULL, NULL, 0, NULL)",
                [],
            )
            .unwrap();
        }
        drop(Store::open(&path).unwrap());

        let mut conn = Connection::open(&path).unwrap();
        assert!(retired_columns_present(&conn).unwrap().is_empty());
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert_eq!(
            drop_retired_columns(&mut conn)
                .expect("a pass with nothing to do must not reach for the write lock"),
            Vec::<String>::new()
        );
        holder.execute_batch("ROLLBACK;").unwrap();
        drop(holder);
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_devices().unwrap().len(), 1);
        assert!(store.device_by_token_hash("hash-a").unwrap().is_some());
    }

    #[test]
    fn a_column_a_future_build_added_survives_a_rollback() {
        // The reason the removal is driven by a named retired list instead of
        // by "a column I do not declare". A user who runs a newer build and
        // comes back to this one arrives with a column this build has never
        // heard of. Reading it as legacy would destroy the newer build's data
        // on the way down and hand back an empty column on the way up, with
        // nothing but a cheerful log line to show for it.
        let (store, path) = temp_store();
        store
            .insert_device("d1", "iPhone", "hash-a", "2026-08-01T00:00:00.000Z")
            .unwrap();
        drop(store);

        {
            // What a newer build's own `add_missing_columns` does: widen each
            // credential table in place, then write through it.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE devices ADD COLUMN attested_at TEXT;
                 ALTER TABLE pairing_codes ADD COLUMN issued_by TEXT;",
            )
            .unwrap();
            conn.execute(
                "UPDATE devices SET attested_at = '2026-09-01T00:00:00.000Z'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pairing_codes(code_hash, created_at, expires_at, expires_at_ms,
                                           consumed_at, issued_by)
                 VALUES('hash-c','c','e', 9_000_000_000_000, NULL, 'the newer build')",
                [],
            )
            .unwrap();
        }

        // The rollback itself.
        let store = Store::open(&path).unwrap();

        let device = store
            .device_by_token_hash("hash-a")
            .unwrap()
            .expect("the device token must still authenticate");
        assert_eq!(device.device_id, "d1");
        assert_eq!(device.name, "iPhone");
        // This build writes through the wider tables without knowing they are
        // wider, because every column it does not name is nullable to it.
        let now = mint(&store, "hash-new", 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-new", now).unwrap(),
            PairingConsume::Consumed
        );
        drop(store);

        let conn = Connection::open(&path).unwrap();
        assert!(
            column_exists(&conn, "devices", "attested_at").unwrap(),
            "a column this build does not know must not be dropped"
        );
        assert!(
            column_exists(&conn, "pairing_codes", "issued_by").unwrap(),
            "a column this build does not know must not be dropped"
        );
        // Present is not enough: a removal that recreated the table and then
        // let the newer build widen it again would leave both names behind with
        // the values gone, which a user only notices on the way up.
        let attested: Option<String> = conn
            .query_row(
                "SELECT attested_at FROM devices WHERE device_id = 'd1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attested.as_deref(), Some("2026-09-01T00:00:00.000Z"));
        let issued_by: Option<String> = conn
            .query_row(
                "SELECT issued_by FROM pairing_codes WHERE code_hash = 'hash-c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(issued_by.as_deref(), Some("the newer build"));
        // Untouched tables keep the index they already carry.
        assert!(index_names(&conn, "devices").contains(&"devices_name".to_string()));
    }

    #[test]
    fn a_table_carrying_a_retired_column_gives_it_up_and_keeps_the_unknown_one() {
        // The mixed case: one retired column and one a newer build added, on the
        // same table. The retired one goes on its own, leaving the column beside
        // it, its type and its value where they were. `devices` shows the other
        // half of the rule: it carries no retired column, so nothing is done to
        // it on its sibling's account.
        let (store, path) = temp_store();
        drop(store);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE devices ADD COLUMN attested_at TEXT;
                 DROP TABLE pairing_codes;
                 CREATE TABLE pairing_codes(
                     code_hash     TEXT PRIMARY KEY,
                     allow_ssh     INTEGER NOT NULL,
                     created_at    TEXT    NOT NULL,
                     expires_at    TEXT    NOT NULL,
                     expires_at_ms INTEGER NOT NULL,
                     consumed_at   TEXT,
                     issued_by     TEXT
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO devices(device_id, name, token_hash, created_at, attested_at)
                 VALUES('d1','iPhone','hash-a','2026-08-01T00:00:00.000Z',
                        '2026-09-01T00:00:00.000Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pairing_codes(code_hash, allow_ssh, created_at, expires_at,
                                           expires_at_ms, consumed_at, issued_by)
                 VALUES('hash-c', 1, 'c', 'e', 9_000_000_000_000, NULL, 'the newer build')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        // Pairing works again, which is what removing the column is for.
        let now = mint(&store, "hash-new", 300_000);
        assert_eq!(
            store.consume_pairing_code("hash-new", now).unwrap(),
            PairingConsume::Consumed
        );
        // Every row came across, including the one that outlived the column it
        // was inserted beside.
        assert_eq!(
            store.consume_pairing_code("hash-c", now).unwrap(),
            PairingConsume::Consumed
        );
        assert!(store.device_by_token_hash("hash-a").unwrap().is_some());
        drop(store);

        let conn = Connection::open(&path).unwrap();
        assert!(!column_exists(&conn, "pairing_codes", "allow_ssh").unwrap());
        assert!(
            column_exists(&conn, "pairing_codes", "issued_by").unwrap(),
            "the column a newer build wrote stays when the retired one leaves"
        );
        let issued_by: Option<String> = conn
            .query_row(
                "SELECT issued_by FROM pairing_codes WHERE code_hash = 'hash-c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(issued_by.as_deref(), Some("the newer build"));
        let attested: Option<String> = conn
            .query_row(
                "SELECT attested_at FROM devices WHERE device_id = 'd1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            attested.as_deref(),
            Some("2026-09-01T00:00:00.000Z"),
            "a table with no retired column is not touched on a sibling's account"
        );
    }

    #[test]
    fn a_fresh_database_is_created_at_the_current_schema() {
        let (store, path) = temp_store();
        drop(store);
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        // Written out rather than compared against the constant that produced
        // it: a version on disk is a fact other builds read, and a test that
        // asks the schema what the schema said would agree with any answer.
        assert_eq!(version, 3, "the schema version other builds will read");
        assert_eq!(version, SCHEMA_VERSION);
        assert!(column_exists(&conn, "events", "session_uid").unwrap());
        assert!(!needs_session_uid_migration(&conn).unwrap());
        // No retired column is ever built, so a first start has nothing to
        // remove and every start after it has nothing either.
        assert!(retired_columns_present(&conn).unwrap().is_empty());
    }

    /// Every push target as `(device_id, token, environment, credential)`, in
    /// device order, so a test can state a whole registration in one assertion.
    ///
    /// A helper rather than a `PartialEq` on [`PushRegistration`]: the type
    /// carries a bearer secret, and handing every caller a `==` over one is not
    /// a convenience worth offering.
    fn registrations(store: &Store) -> Vec<(String, String, String, Option<String>)> {
        let mut rows: Vec<_> = store
            .push_targets()
            .unwrap()
            .into_iter()
            .map(|target| {
                (
                    target.device_id,
                    target.token,
                    target.environment,
                    // Exposed for the comparison the tests make; the wrapper is
                    // the on-the-wire and in-memory guard, not a barrier to a
                    // test reading its own fixture back.
                    target.credential.map(|c| c.expose().to_string()),
                )
            })
            .collect();
        rows.sort();
        rows
    }

    /// The credential column as SQLite holds it, for the rows no accessor
    /// reaches: a row stripped of its token is not a push target, so
    /// `push_targets` cannot say whether its credential went with it.
    fn stored_credential(path: &Path, device_id: &str) -> Option<String> {
        stored_column(path, device_id, "push_credential")
    }

    fn stored_token(path: &Path, device_id: &str) -> Option<String> {
        stored_column(path, device_id, "push_token")
    }

    /// Read one push column straight out of the file, past every filter the
    /// store's own readers apply — the only way to ask what a row still holds
    /// rather than what a caller is allowed to see.
    fn stored_column(path: &Path, device_id: &str, column: &str) -> Option<String> {
        Connection::open(path)
            .unwrap()
            .query_row(
                &format!("SELECT {column} FROM devices WHERE device_id = ?1"),
                params![device_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// The measured twin-notification defect: one phone re-paired, its token
    /// registered under the new device row while the old row kept a copy,
    /// and every doorbell fanned out to both. A token is one phone;
    /// registering it anywhere strips it everywhere else, atomically.
    #[test]
    fn a_push_token_lives_on_exactly_one_device_row() {
        let (store, _path) = temp_store();
        store
            .insert_device("dev-old", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            // The same physical phone re-pairing arrives as a *new* device
            // row with a fresh name — names are unique.
            .insert_device("dev-new", "iPhone 2", "hash-b", "2026-08-02T00:00:00Z")
            .unwrap();

        store
            .set_push_token("dev-old", "tok-same", "production", None)
            .unwrap();
        store
            .set_push_token("dev-new", "tok-same", "production", None)
            .unwrap();

        let targets = store.push_targets().unwrap();
        assert_eq!(
            targets.len(),
            1,
            "one physical phone must be one push target: {targets:?}"
        );
        assert_eq!(
            targets[0].device_id, "dev-new",
            "the latest registration owns the token"
        );

        // A different phone's different token is untouched.
        store
            .set_push_token("dev-old", "tok-other", "production", None)
            .unwrap();
        assert_eq!(store.push_targets().unwrap().len(), 2);
    }

    /// **A late environment correction cannot move a token it is not about.**
    ///
    /// The correction says "this token belongs on the other host". Applied by
    /// device id alone, an answer about a replaced token would move the *new*
    /// one to the wrong host, and every push to it would fail.
    #[test]
    fn an_environment_correction_only_moves_the_token_it_is_about() {
        let (store, _path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "sandbox", None)
            .unwrap();
        store
            .set_push_token("dev-1", "tok-b", "sandbox", None)
            .unwrap();

        store
            .set_push_environment("dev-1", "tok-a", None, "production")
            .unwrap();
        assert_eq!(
            registrations(&store),
            vec![(
                "dev-1".to_string(),
                "tok-b".to_string(),
                "sandbox".to_string(),
                None
            )],
            "the live token keeps the host it registered on"
        );

        store
            .set_push_environment("dev-1", "tok-b", None, "production")
            .unwrap();
        assert_eq!(
            store.push_targets().unwrap()[0].environment,
            "production",
            "a correction about the live token still applies"
        );
    }

    /// **A late refusal cannot erase a token registered since.**
    ///
    /// A device id outlives the token behind it: a phone that reinstalls
    /// registers a new one under the same row. Apple's `410` for the old token
    /// can arrive after that, and clearing by device id alone would leave a
    /// paired phone silently unable to receive anything.
    #[test]
    fn a_dead_token_is_cleared_only_if_it_is_still_the_one_registered() {
        let (store, _path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "production", None)
            .unwrap();

        // The phone reinstalls and registers again before Apple answers.
        store
            .set_push_token("dev-1", "tok-b", "production", None)
            .unwrap();
        assert!(
            !store.clear_push_token("dev-1", "tok-a", None).unwrap(),
            "and says it cleared nothing, so the caller can tell the device is still there"
        );

        assert_eq!(
            registrations(&store),
            vec![(
                "dev-1".to_string(),
                "tok-b".to_string(),
                "production".to_string(),
                None
            )],
            "the token the phone is actually using survives its predecessor's refusal"
        );

        // And the refusal still works when it names the current token — and
        // says so, which is what `forget` reports and the delivery path types
        // its refusal on.
        assert!(
            store.clear_push_token("dev-1", "tok-b", None).unwrap(),
            "clearing the registered token reports that it did"
        );
        assert!(
            store.push_targets().unwrap().is_empty(),
            "a device Apple has disowned stops being a target"
        );
    }

    /// The strip must never commit without its claim: registering against a
    /// device row that does not exist (or was revoked in the race window after
    /// the caller's check) errors, and the current owner keeps the token — the
    /// transaction rolled back whole.
    #[test]
    fn a_failed_claim_rolls_back_the_strip() {
        let (store, _path) = temp_store();
        store
            .insert_device("dev-live", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-live", "tok-live", "production", Some("cred-live"))
            .unwrap();

        assert!(
            store
                .set_push_token("dev-ghost", "tok-live", "production", Some("cred-ghost"))
                .is_err(),
            "an unknown device cannot claim a token"
        );
        let targets = store.push_targets().unwrap();
        assert_eq!(targets.len(), 1, "the owner survived: {targets:?}");
        assert_eq!(targets[0].device_id, "dev-live");
        assert_eq!(
            targets[0].credential.as_ref().map(|c| c.expose()),
            Some("cred-live"),
            "the rollback restored the whole tuple, not the token alone"
        );

        // A revoked device is no better than an unknown one — the claim's
        // `revoked_at IS NULL` is load-bearing, not decoration.
        store
            .insert_device("dev-revoked", "iPhone 2", "hash-b", "2026-08-02T00:00:00Z")
            .unwrap();
        store
            .revoke_device("dev-revoked", "2026-08-03T00:00:00Z")
            .unwrap();
        assert!(
            store
                .set_push_token(
                    "dev-revoked",
                    "tok-live",
                    "production",
                    Some("cred-revoked")
                )
                .is_err(),
            "a revoked device cannot claim a token"
        );
        assert_eq!(store.push_targets().unwrap()[0].device_id, "dev-live");
    }

    /// **A token never arrives carrying its predecessor's credential.**
    ///
    /// The relay checks the credential against the token in the same request.
    /// A registration that wrote the new token over the old one and left the
    /// credential where it was would be refused for every push afterwards — and
    /// refused as a bad credential, which is not the fact "this phone is gone"
    /// and must not be acted on as one. `None` is therefore a value that is
    /// written, not an argument that is skipped.
    #[test]
    fn a_registration_replaces_the_whole_push_tuple() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();

        store
            .set_push_token("dev-1", "tok-a", "sandbox", Some("cred-a"))
            .unwrap();
        assert_eq!(
            registrations(&store),
            vec![(
                "dev-1".to_string(),
                "tok-a".to_string(),
                "sandbox".to_string(),
                Some("cred-a".to_string())
            )],
            "the credential is read back beside the token it authorizes"
        );

        // The phone rotates its token and registers against a daemon that now
        // holds an Apple key of its own, so there is no relay credential.
        store
            .set_push_token("dev-1", "tok-b", "production", None)
            .unwrap();
        assert_eq!(
            registrations(&store),
            vec![(
                "dev-1".to_string(),
                "tok-b".to_string(),
                "production".to_string(),
                None
            )]
        );
        assert_eq!(
            stored_credential(&path, "dev-1"),
            None,
            "the previous credential is gone from the row, not merely unreported"
        );
    }

    /// **A displaced row keeps nothing about the token it lost.**
    ///
    /// The strip that stops one phone being two push targets clears all three
    /// columns in the claim's own transaction. A row that kept the credential
    /// for a token another row now holds is a bearer secret sitting under an
    /// identity that cannot use it, waiting to be paired with whatever token is
    /// registered there next.
    #[test]
    fn a_displaced_row_loses_its_credential_with_its_token() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-old", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .insert_device("dev-new", "iPhone 2", "hash-b", "2026-08-02T00:00:00Z")
            .unwrap();

        store
            .set_push_token("dev-old", "tok-same", "production", Some("cred-old"))
            .unwrap();
        let displaced = store
            .set_push_token("dev-new", "tok-same", "production", Some("cred-new"))
            .unwrap();

        assert_eq!(displaced, vec!["dev-old".to_string()]);
        assert_eq!(
            registrations(&store),
            vec![(
                "dev-new".to_string(),
                "tok-same".to_string(),
                "production".to_string(),
                Some("cred-new".to_string())
            )]
        );
        assert_eq!(
            stored_credential(&path, "dev-old"),
            None,
            "the row the token was taken from kept a credential for it"
        );
    }

    /// **A late refusal disarms the token it names, credential and all.**
    ///
    /// The same window as the token rule it extends: Apple's `410` can arrive
    /// after the phone has registered again, and a clear applied by device id
    /// alone would strip the credential out from under a live registration and
    /// leave every push refused for a reason that reads like a stolen bearer.
    #[test]
    fn a_dead_token_takes_its_credential_only_if_it_is_still_the_one_registered() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "production", Some("cred-a"))
            .unwrap();
        store
            .set_push_token("dev-1", "tok-b", "production", Some("cred-b"))
            .unwrap();

        assert!(!store
            .clear_push_token("dev-1", "tok-a", Some("cred-a"))
            .unwrap());
        assert_eq!(
            stored_credential(&path, "dev-1"),
            Some("cred-b".to_string()),
            "a refusal about a replaced token must leave the live credential alone"
        );

        assert!(store
            .clear_push_token("dev-1", "tok-b", Some("cred-b"))
            .unwrap());
        assert_eq!(
            stored_credential(&path, "dev-1"),
            None,
            "the credential for a token Apple has disowned does not outlive it"
        );
        assert!(store.push_targets().unwrap().is_empty());
    }

    /// **A late correction for a superseded *credential* is a no-op**, the
    /// credential half of the tuple-CAS the token half already had.
    ///
    /// The race: an attempt snapshots `(T, C1)`; the relay reissues, so the row
    /// now reads `(T, C2)` under the *same token*; then `C1`'s accepted answer
    /// arrives carrying an environment correction. Scoped to the token alone it
    /// would move `C2`'s environment to whatever `C1` was told — a live
    /// registration corrupted by a stale answer about a bearer nobody holds.
    ///
    /// **The negative check:** drop `AND push_credential IS ?3` from
    /// `set_push_environment` and this fails — the token still matches, so the
    /// stale correction fires. That predicate is the whole fix.
    #[test]
    fn a_late_environment_correction_for_a_superseded_credential_is_a_no_op() {
        let (store, _path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        // Same token, rotated credential — the relay reissued a bearer.
        store
            .set_push_token("dev-1", "tok-a", "sandbox", Some("cred-1"))
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "sandbox", Some("cred-2"))
            .unwrap();

        // C1's late correction, for a bearer the row no longer holds.
        store
            .set_push_environment("dev-1", "tok-a", Some("cred-1"), "production")
            .unwrap();
        assert_eq!(
            store.push_targets().unwrap()[0].environment,
            "sandbox",
            "a correction under the superseded credential must not move the live one"
        );

        // C2's correction, for the bearer the row does hold, still applies.
        store
            .set_push_environment("dev-1", "tok-a", Some("cred-2"), "production")
            .unwrap();
        assert_eq!(
            store.push_targets().unwrap()[0].environment,
            "production",
            "a correction under the current credential is the one that lands"
        );
    }

    /// **A late `410` for a superseded credential clears nothing**, the
    /// credential half of the same tuple-CAS.
    ///
    /// A relay reissue rotates the bearer under a token Apple still knows. A
    /// `410` snapshotted under `(T, C1)` is a departure about the old bearer,
    /// not about the phone — and clearing by token alone would strip a live
    /// `(T, C2)` and leave every push refused as if the phone were gone.
    ///
    /// **The negative check:** drop `AND push_credential IS ?3` from
    /// `clear_push_token` and this fails — the token matches, so the stale
    /// departure wipes the live tuple.
    #[test]
    fn a_late_410_for_a_superseded_credential_clears_nothing() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "production", Some("cred-1"))
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "production", Some("cred-2"))
            .unwrap();

        assert!(
            !store
                .clear_push_token("dev-1", "tok-a", Some("cred-1"))
                .unwrap(),
            "a 410 about the superseded credential reports it cleared nothing"
        );
        assert_eq!(
            stored_credential(&path, "dev-1"),
            Some("cred-2".to_string()),
            "the live tuple survives a departure about the bearer it replaced"
        );

        assert!(
            store
                .clear_push_token("dev-1", "tok-a", Some("cred-2"))
                .unwrap(),
            "a 410 about the current credential does clear it"
        );
        assert!(store.push_targets().unwrap().is_empty());
    }

    /// **An accepted push never invalidates the credential that carried it.**
    ///
    /// The environment correction is the relay reporting where the token
    /// actually lives. It says nothing about the bearer, and the bearer is the
    /// same one either side of the answer: rewriting it here would mean a push
    /// that worked cost the phone its authorization to receive the next one.
    #[test]
    fn an_environment_correction_leaves_the_credential_alone() {
        let (store, _path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "sandbox", Some("cred-a"))
            .unwrap();

        store
            .set_push_environment("dev-1", "tok-a", Some("cred-a"), "production")
            .unwrap();
        assert_eq!(
            registrations(&store),
            vec![(
                "dev-1".to_string(),
                "tok-a".to_string(),
                "production".to_string(),
                Some("cred-a".to_string())
            )],
            "only the host moved"
        );
    }

    /// Reopen a database that a build at `user_version` 2 last wrote, so the
    /// one-shot normalizer in [`Store::migrate`] sees the upgrade it gates on.
    /// The raw connection is how a legacy state is manufactured that no current
    /// accessor could write — a mixed tuple, or a revoked row that kept its
    /// token — exactly what a rollback or a pre-tuple build left behind.
    fn reopen_from_v2(path: &Path, craft: impl FnOnce(&Connection)) -> Store {
        let raw = Connection::open(path).unwrap();
        craft(&raw);
        raw.pragma_update(None, "user_version", 2i64).unwrap();
        drop(raw);
        Store::open(path).unwrap()
    }

    /// **A rollback-mixed tuple is repaired on the way back up.** (D2)
    ///
    /// A build carrying the credential column wrote `(T1, C1)`. A rollback to a
    /// build that predates it updated the token in place — it knows only
    /// `push_token` — leaving `(T2, C1)`, which reads as a live relay
    /// registration the relay will refuse on every push because `C1` was minted
    /// for `T1`. The credential is opaque and cannot be proven current from
    /// inside the database, so the upgrade clears it and keeps the token, and
    /// the phone re-registers the whole tuple.
    ///
    /// **The negative check:** remove the `from_version < 3` call to
    /// `normalize_push_tuples` (or the SCHEMA bump that makes the gate fire) and
    /// this fails — the mixed credential survives and reads as live.
    #[test]
    fn a_rollback_mixed_tuple_is_cleared_on_the_next_upgrade() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-1", "production", Some("cred-1"))
            .unwrap();
        drop(store);

        // The rollback's token-only update: a new token beside the old bearer.
        let store = reopen_from_v2(&path, |raw| {
            raw.execute(
                "UPDATE devices SET push_token = 'tok-2' WHERE device_id = 'dev-1'",
                [],
            )
            .unwrap();
        });

        assert_eq!(
            stored_credential(&path, "dev-1"),
            None,
            "the credential that cannot be proven current is cleared"
        );
        assert_eq!(
            stored_column(&path, "dev-1", "push_environment"),
            None,
            "and its environment with it, so the phone re-establishes the tuple"
        );
        assert_eq!(
            stored_token(&path, "dev-1"),
            Some("tok-2".to_string()),
            "the token is Apple's and still valid; it stays"
        );
        // The row is still a target — it kept its token — but now credential-less,
        // which the relay path answers as a missing tuple and the phone repairs
        // by re-registering. That is the whole intent: coherent, not deleted.
        let targets = store.push_targets().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].token, "tok-2");
        assert_eq!(
            targets[0].credential.as_ref().map(|c| c.expose()),
            None,
            "no bearer the relay would refuse rides on the surviving token"
        );
    }

    /// **A historically revoked row loses its lingering push tuple.** (D3)
    ///
    /// A build that predated [`Store::revoke_device`]'s tuple-clear revoked a
    /// row and kept its token and credential. It never delivers — `push_targets`
    /// filters it — but the routing tuple sits on a device the operator withdrew
    /// trust from, which revocation is supposed to have ended. The upgrade
    /// clears the whole tuple.
    ///
    /// **The negative check:** remove the revoked-row `UPDATE` from
    /// `normalize_push_tuples` and this fails — the tuple lingers on the revoked
    /// row.
    #[test]
    fn a_historically_revoked_row_loses_its_push_tuple_on_upgrade() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        drop(store);

        // A pre-tuple-clear revocation: revoked, but token/env/credential kept.
        let _store = reopen_from_v2(&path, |raw| {
            raw.execute(
                "UPDATE devices
                    SET revoked_at = '2026-08-02T00:00:00Z',
                        push_token = 'tok-a',
                        push_environment = 'production',
                        push_credential = 'cred-a'
                  WHERE device_id = 'dev-1'",
                [],
            )
            .unwrap();
        });

        assert_eq!(stored_token(&path, "dev-1"), None, "the token goes");
        assert_eq!(
            stored_column(&path, "dev-1", "push_environment"),
            None,
            "the environment goes"
        );
        assert_eq!(
            stored_credential(&path, "dev-1"),
            None,
            "and the bearer goes — nothing addressable outlives the revocation"
        );
    }

    /// **The gate is what makes clearing a credential safe.** An ordinary
    /// restart is not an upgrade, so a live credential must survive it — the
    /// property that separates this one-shot repair from the always-on GLOB
    /// normalizer, which clears a value that is always wrong.
    ///
    /// **The negative check:** drop the `from_version < 3` gate so the
    /// normalizer runs every start, and this fails — the credential is wiped on
    /// the reopen and relay push would re-register for ever.
    #[test]
    fn a_current_credential_survives_a_restart_that_is_not_an_upgrade() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "production", Some("cred-a"))
            .unwrap();
        // `temp_store` already migrated to the current version, so this reopen
        // is a plain restart: `from_version` equals `SCHEMA_VERSION`, the gate
        // is closed, and the normalizer does not run.
        drop(store);
        let store = Store::open(&path).unwrap();

        assert_eq!(
            store.push_targets().unwrap()[0]
                .credential
                .as_ref()
                .map(|c| c.expose()),
            Some("cred-a"),
            "a restart is not an upgrade; a live bearer is not touched"
        );
    }

    /// The authorization read: a revoked phone is not a target, and the
    /// registrations that are carry exactly the credential they were given —
    /// one for the relay path, none for the direct one.
    #[test]
    fn a_revoked_device_is_never_a_push_target() {
        let (store, path) = temp_store();
        for (device_id, name, hash) in [
            ("dev-relay", "iPhone", "hash-a"),
            ("dev-direct", "iPad", "hash-b"),
            ("dev-gone", "retired iPhone", "hash-c"),
        ] {
            store
                .insert_device(device_id, name, hash, "2026-08-01T00:00:00Z")
                .unwrap();
        }
        store
            .set_push_token("dev-relay", "tok-relay", "production", Some("cred-relay"))
            .unwrap();
        store
            .set_push_token("dev-direct", "tok-direct", "sandbox", None)
            .unwrap();
        store
            .set_push_token("dev-gone", "tok-gone", "production", Some("cred-gone"))
            .unwrap();

        assert!(store
            .revoke_device("dev-gone", "2026-08-03T00:00:00Z")
            .unwrap());
        assert_eq!(
            registrations(&store),
            vec![
                (
                    "dev-direct".to_string(),
                    "tok-direct".to_string(),
                    "sandbox".to_string(),
                    None
                ),
                (
                    "dev-relay".to_string(),
                    "tok-relay".to_string(),
                    "production".to_string(),
                    Some("cred-relay".to_string())
                ),
            ],
            "a revoked phone must stop being offered"
        );
        assert_eq!(
            stored_credential(&path, "dev-gone"),
            None,
            "revoking a phone takes its bearer with it — nothing else ever would, \
             because no delivery is attempted for a revoked row and only a delivery \
             clears a token"
        );
        assert_eq!(
            stored_token(&path, "dev-gone"),
            None,
            "and its APNs token, for the same reason"
        );
        assert_eq!(
            stored_credential(&path, "dev-relay"),
            Some("cred-relay".to_string()),
            "revoking one phone touches no other"
        );
    }

    /// What the handshake tells a phone about where its token lives.
    ///
    /// "No token registered" and "no such device" are one answer, because they
    /// are one fact to the phone asking. A row with an empty token is the same
    /// answer again: it is the value `push_targets` refuses to push to, and the
    /// two reads must not disagree about whether a registration exists.
    #[test]
    fn only_a_device_with_a_registered_token_reports_an_environment() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z")
            .unwrap();
        store
            .insert_device("dev-2", "iPad", "hash-b", "2026-08-02T00:00:00Z")
            .unwrap();
        store
            .set_push_token("dev-1", "tok-a", "production", Some("cred-a"))
            .unwrap();

        assert_eq!(
            store.push_environment_for("dev-1").unwrap().as_deref(),
            Some("production")
        );
        assert_eq!(
            store.push_environment_for("dev-2").unwrap(),
            None,
            "a paired phone that never registered has no environment to be told"
        );
        assert_eq!(
            store.push_environment_for("dev-nobody").unwrap(),
            None,
            "and a device id nothing was ever paired under is the same answer"
        );

        // Apple disowns the token, and the answer goes with it.
        assert!(store
            .clear_push_token("dev-1", "tok-a", Some("cred-a"))
            .unwrap());
        assert_eq!(store.push_environment_for("dev-1").unwrap(), None);

        // The empty token, which only a hand-written row or an older build
        // produces, is the value `push_targets` already refuses.
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE devices SET push_token = '' WHERE device_id = 'dev-2'",
                [],
            )
            .unwrap();
        assert_eq!(
            store.push_environment_for("dev-2").unwrap(),
            None,
            "an empty token is not a registration to either read"
        );
        assert!(store.push_targets().unwrap().is_empty());
    }

    /// **The credential is a bearer secret and does not render.**
    ///
    /// A `{targets:?}` in a log line, a failed `assert_eq!`, a panic on the push
    /// path — every one of them formats a registration, and a derived `Debug`
    /// would put a value someone can push with into all of them. Whether there
    /// is a credential still renders: that is the difference between the relay
    /// path and the direct path, and an operator reading a failure needs it.
    #[test]
    fn a_credential_never_appears_in_a_debug_rendering() {
        const CREDENTIAL: &str = "cred-live-bearer-nobody-may-log";
        const TOKEN: &str = "0123456789abcdef0123456789abcdef";
        let relay = PushRegistration {
            device_id: "dev-1".to_string(),
            token: TOKEN.to_string(),
            environment: "production".to_string(),
            credential: Some(Redacted::from(CREDENTIAL)),
        };

        let rendered = format!("{relay:?}");
        assert!(
            !rendered.contains(CREDENTIAL),
            "a bearer secret reached a rendering: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "having one must still be legible: {rendered}"
        );
        assert!(rendered.contains("dev-1"), "{rendered}");
        assert!(rendered.contains("production"), "{rendered}");
        assert!(
            !rendered.contains(TOKEN),
            "the token is abbreviated to what correlates two log lines: {rendered}"
        );
        assert!(rendered.contains("01234567"), "{rendered}");

        // The whole list a sender formats carries the same guarantee.
        assert!(!format!("{:?}", vec![relay.clone()]).contains(CREDENTIAL));

        // Absence renders as absence, so the direct path is not mistaken for a
        // relay registration whose credential merely did not print.
        let direct = PushRegistration {
            credential: None,
            ..relay
        };
        let rendered = format!("{direct:?}");
        assert!(rendered.contains("None"), "{rendered}");
        assert!(!rendered.contains("<redacted>"), "{rendered}");
    }

    // --------------------------------------- adversarial: the retired columns
    //
    // Independent cover for `drop_retired_columns`, written against the claims
    // it makes rather than against how it makes them, and not reusing the
    // fixture that came with the session-uid migration: `legacy_database` above
    // brings that migration along with it, and the builder here changes nothing
    // but the two credential tables.

    /// A database at this build's schema everywhere except `devices` and
    /// `pairing_codes`, which carry the exact shape the last shipped build
    /// declares: the SSH columns, `allow_ssh INTEGER NOT NULL` with no default,
    /// the push columns, and the unique name index.
    fn retired_credential_database(path: &Path) -> Connection {
        drop(Store::open(path).unwrap());
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            DROP TABLE devices;
            DROP TABLE pairing_codes;
            CREATE TABLE devices(
                device_id         TEXT PRIMARY KEY,
                name              TEXT NOT NULL,
                token_hash        TEXT NOT NULL UNIQUE,
                created_at        TEXT NOT NULL,
                last_seen_at      TEXT,
                revoked_at        TEXT,
                ssh_key_installed INTEGER NOT NULL DEFAULT 0,
                ssh_fingerprint   TEXT,
                push_token        TEXT,
                push_environment  TEXT,
                push_credential   TEXT
            );
            CREATE UNIQUE INDEX devices_name ON devices(name);
            CREATE TABLE pairing_codes(
                code_hash     TEXT PRIMARY KEY,
                allow_ssh     INTEGER NOT NULL,
                created_at    TEXT    NOT NULL,
                expires_at    TEXT    NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                consumed_at   TEXT
            );
            "#,
        )
        .unwrap();
        conn
    }

    /// `(device_id, name, token_hash, last_seen_at, revoked_at, push_token,
    /// push_credential)`. One row per value the removal could plausibly mangle:
    /// a quote in a name that a string-built statement would break on,
    /// non-ASCII, an empty name, a revoked row that must survive as a row while
    /// refusing to authenticate, and both shapes of live push registration —
    /// the relay's, which carries a credential, and the direct one, which never
    /// has one. Losing either half of that tuple silences a phone.
    #[allow(clippy::type_complexity)]
    const HOSTILE_DEVICES: &[(
        &str,
        &str,
        &str,
        Option<&str>,
        Option<&str>,
        Option<&str>,
        Option<&str>,
    )] = &[
        (
            "d-plain",
            "iPhone",
            "hash-plain",
            Some("2026-08-02T00:00:00.000Z"),
            None,
            None,
            None,
        ),
        (
            "d-quote",
            "O'Brien's iPad",
            "hash-quote",
            None,
            None,
            None,
            None,
        ),
        (
            "d-unicode",
            "Ünïcodé 📱",
            "hash-ünïcodé-🔑",
            None,
            None,
            None,
            None,
        ),
        ("d-empty-name", "", "", None, None, None, None),
        (
            "d-revoked",
            "retired iPhone",
            "hash-revoked",
            Some("2026-08-02T00:00:00.000Z"),
            Some("2026-08-03T00:00:00.000Z"),
            None,
            None,
        ),
        (
            "d-push",
            "iPad Pro",
            "hash-push",
            None,
            None,
            Some("apns-token-1"),
            None,
        ),
        (
            "d-push-relay",
            "iPhone mini",
            "hash-push-relay",
            None,
            None,
            Some("apns-token-2"),
            Some("relay-credential-1"),
        ),
    ];

    /// Seed the retired-shape `devices` table the way the build that declared
    /// that shape would have.
    #[allow(clippy::type_complexity)]
    fn seed_retired_device(
        conn: &Connection,
        row: &(
            &str,
            &str,
            &str,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
        ),
    ) {
        conn.execute(
            "INSERT INTO devices(device_id, name, token_hash, created_at, last_seen_at,
                                 revoked_at, ssh_key_installed, ssh_fingerprint,
                                 push_token, push_environment, push_credential)
             VALUES(?1, ?2, ?3, '2026-08-01T00:00:00.000Z', ?4, ?5, 1, 'SHA256:abc', ?6, ?7, ?8)",
            params![
                row.0,
                row.1,
                row.2,
                row.3,
                row.4,
                row.5,
                row.5.map(|_| "production"),
                row.6
            ],
        )
        .unwrap();
    }

    /// Every value of every row a query returns, typed as SQLite stores it, so a
    /// blob that came back as text or a NULL that became an empty string fails
    /// here instead of being formatted away.
    fn rows_of(conn: &Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut stmt = conn.prepare(sql).unwrap();
        let width = stmt.column_count();
        let rows = stmt
            .query_map([], |row| {
                (0..width)
                    .map(|i| row.get::<usize, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    /// The schema as SQLite itself records it, minus the root pages a rebuild is
    /// entitled to move. Byte equality across an open is the difference between
    /// "left alone" and "rebuilt to something that merely looks the same".
    fn schema_dump(conn: &Connection) -> Vec<(String, String, Option<String>)> {
        conn.prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    /// One named object, and everything named after it, as SQLite records it.
    fn schema_dump_of(conn: &Connection, prefix: &str) -> Vec<(String, String, Option<String>)> {
        schema_dump(conn)
            .into_iter()
            .filter(|(_, name, _)| name.starts_with(prefix))
            .collect()
    }

    /// The names of the indexes on a table that have a `CREATE INDEX` of their
    /// own. The implicit `sqlite_autoindex_*` behind a `PRIMARY KEY` or a
    /// `UNIQUE` declaration has no `sql` and is left out.
    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(
            "SELECT name FROM sqlite_master
              WHERE type = 'index' AND tbl_name = ?1 COLLATE NOCASE AND sql IS NOT NULL
              ORDER BY name",
        )
        .unwrap()
        .query_map(params![table], |row| row.get(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    }

    /// Everything `devices` holds that outlives the removal, including the push
    /// columns no accessor exposes. Named columns rather than `*` so the same
    /// query reads both the retired shape and the current one — and named in
    /// full, because a column left out of this list is a column the removal
    /// could empty with every test here still green.
    fn devices_dump(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        rows_of(
            conn,
            "SELECT device_id, name, token_hash, created_at, last_seen_at,
                    revoked_at, push_token, push_environment, push_credential
               FROM devices ORDER BY device_id",
        )
    }

    fn codes_dump(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        rows_of(
            conn,
            "SELECT code_hash, created_at, expires_at, expires_at_ms, consumed_at
               FROM pairing_codes ORDER BY code_hash",
        )
    }

    /// Every row is still there afterwards, value for value.
    ///
    /// `DROP COLUMN` preserves rows by construction, which is exactly why it is
    /// worth pinning: a device row *is* a phone's pairing, and losing one
    /// silently un-pairs a phone that has no way to find out until it next tries
    /// to connect. The values are the hostile ones — a quote, non-ASCII, an
    /// empty string, a NULL, a revoked row — read back as SQLite stores them.
    #[test]
    fn every_row_survives_the_removal() {
        let path = legacy_path();
        let (devices_before, codes_before) = {
            let conn = retired_credential_database(&path);
            for row in HOSTILE_DEVICES {
                seed_retired_device(&conn, row);
            }
            conn.execute(
                "INSERT INTO pairing_codes(code_hash, allow_ssh, created_at, expires_at,
                                           expires_at_ms, consumed_at)
                 VALUES('code-inflight', 1, 'c', 'e', 9000000000000, NULL)",
                [],
            )
            .unwrap();
            (devices_dump(&conn), codes_dump(&conn))
        };

        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_devices().unwrap().len(), HOSTILE_DEVICES.len());
        // The revoked row is still a row and still refuses to authenticate.
        assert!(store.device_by_token_hash("hash-plain").unwrap().is_some());
        assert!(!store.device_is_active("d-revoked").unwrap());
        // The code that was in flight when the column went is still redeemable.
        assert_eq!(
            store.consume_pairing_code("code-inflight", 0).unwrap(),
            PairingConsume::Consumed
        );
        drop(store);

        let conn = Connection::open(&path).unwrap();
        assert_eq!(devices_dump(&conn), devices_before, "a device row changed");
        assert_eq!(
            codes_dump(&conn)
                .into_iter()
                .map(|row| row[0].clone())
                .collect::<Vec<_>>(),
            codes_before
                .into_iter()
                .map(|row| row[0].clone())
                .collect::<Vec<_>>(),
            "a pairing code was lost"
        );
        assert!(retired_columns_present(&conn).unwrap().is_empty());
    }

    /// The tables the removal has no business touching come through it
    /// byte-identical, rows and schema.
    ///
    /// A regression test with a history: the engine this replaced reached the
    /// same three columns through `ALTER TABLE … RENAME`, which rewrites every
    /// other object in the schema that names the renamed table, and one wrong
    /// step there silently emptied a neighbour. `DROP COLUMN` names one column
    /// on one table and cannot reach past it — this is the assertion that says
    /// so out loud.
    #[test]
    fn the_neighbour_tables_come_through_the_removal_untouched() {
        let path = legacy_path();
        let (schema_before, sessions_before, events_before) = {
            let conn = retired_credential_database(&path);
            for row in HOSTILE_DEVICES {
                seed_retired_device(&conn, row);
            }
            conn.execute_batch(
                "INSERT INTO sessions VALUES('uid-1','cc-1','codeconnect','/tmp/sock','/tmp/one',
                                             'claude-uuid','/tmp/one.jsonl','live',
                                             '2026-07-30T10:00:00.000Z','2026-07-30T11:00:00.000Z');
                 INSERT INTO events VALUES('uid-1','cc-1',1,'2026-07-30T10:00:01.000Z','output',
                                           '{\"text\":\"hello\"}','hook','ev-1',NULL,NULL);",
            )
            .unwrap();
            (
                [
                    schema_dump_of(&conn, "sessions"),
                    schema_dump_of(&conn, "events"),
                ],
                rows_of(&conn, "SELECT * FROM sessions ORDER BY session_uid"),
                rows_of(&conn, "SELECT * FROM events ORDER BY session_uid, seq"),
            )
        };

        drop(Store::open(&path).unwrap());

        let conn = Connection::open(&path).unwrap();
        assert!(retired_columns_present(&conn).unwrap().is_empty());
        assert_eq!(
            [
                schema_dump_of(&conn, "sessions"),
                schema_dump_of(&conn, "events"),
            ],
            schema_before,
            "a neighbour table was redefined"
        );
        assert_eq!(
            rows_of(&conn, "SELECT * FROM sessions ORDER BY session_uid"),
            sessions_before,
            "a neighbour table lost or changed a row"
        );
        assert_eq!(
            rows_of(&conn, "SELECT * FROM events ORDER BY session_uid, seq"),
            events_before,
            "a neighbour table lost or changed a row"
        );
    }

    /// A `devices` column SQLite will not drop is left where it is, and the
    /// daemon serves normally around it.
    ///
    /// Both columns on that table are nullable or defaulted, so every statement
    /// this build writes keeps working with one of them still present — which is
    /// why leaving it is the right answer and stopping the daemon is not. An
    /// index on the column is what SQLite refuses the drop for.
    #[test]
    fn a_devices_column_that_cannot_be_dropped_is_left_and_the_daemon_carries_on() {
        let path = legacy_path();
        let before = {
            let conn = retired_credential_database(&path);
            for row in HOSTILE_DEVICES {
                seed_retired_device(&conn, row);
            }
            conn.execute_batch("CREATE INDEX devices_ssh ON devices(ssh_fingerprint);")
                .unwrap();
            devices_dump(&conn)
        };

        let store = Store::open(&path).unwrap();
        // Pairing a phone and authenticating one both still work.
        let now = mint(&store, "code-new", 300_000);
        assert_eq!(
            store.consume_pairing_code("code-new", now).unwrap(),
            PairingConsume::Consumed
        );
        assert!(store.device_by_token_hash("hash-plain").unwrap().is_some());
        drop(store);

        let mut conn = Connection::open(&path).unwrap();
        assert!(
            column_exists(&conn, "devices", "ssh_fingerprint").unwrap(),
            "the column its index blocks stays, rather than taking the daemon down with it"
        );
        assert!(
            index_names(&conn, "devices").contains(&"devices_ssh".to_string()),
            "and so does the index that blocked it"
        );
        // The column beside it has no such obstacle and is gone, and so is the
        // one on the other table: a refusal is about one column.
        assert!(!column_exists(&conn, "devices", "ssh_key_installed").unwrap());
        assert!(!column_exists(&conn, "pairing_codes", "allow_ssh").unwrap());
        assert_eq!(devices_dump(&conn), before, "the refusal cost a row");

        // What an operator is told, in the message that names the column.
        let warnings = drop_retired_columns(&mut conn).unwrap();
        assert_eq!(warnings.len(), 1, "one refusal, one message: {warnings:?}");
        assert!(
            warnings[0].contains("devices.ssh_fingerprint"),
            "the message must name the column: {}",
            warnings[0]
        );
        drop(conn);

        // And a phone that pairs afterwards is written to the table the column
        // is still on.
        let store = Store::open(&path).unwrap();
        store
            .insert_device("d-new", "iPad mini", "hash-new", "2026-08-04T00:00:00.000Z")
            .unwrap();
        assert_eq!(
            store.list_devices().unwrap().len(),
            HOSTILE_DEVICES.len() + 1
        );
    }

    /// A `pairing_codes.allow_ssh` SQLite will not drop is left where it is, and
    /// costs nothing but its own presence.
    ///
    /// The scenario the removal exists to be safe in: a newer build added
    /// `issued_by` and an index that names `allow_ssh`, and an operator came back
    /// down to this one. A rebuild of the table would take `issued_by`, the value
    /// under it and the index with it — so there is no rebuild. The column stays,
    /// every row and every newer column stays, and pairing keeps working because
    /// the mint names the leftover `NOT NULL` column itself.
    #[test]
    fn a_pairing_codes_column_that_cannot_be_dropped_costs_nothing_but_its_own_presence() {
        let path = legacy_path();
        let devices_before = {
            let conn = retired_credential_database(&path);
            for row in HOSTILE_DEVICES {
                seed_retired_device(&conn, row);
            }
            conn.execute_batch(
                "ALTER TABLE pairing_codes ADD COLUMN issued_by TEXT;
                 CREATE INDEX codes_ssh ON pairing_codes(allow_ssh);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pairing_codes(code_hash, allow_ssh, created_at, expires_at,
                                           expires_at_ms, consumed_at, issued_by)
                 VALUES('code-inflight', 1, 'c', 'e', 9000000000000, NULL, 'the newer build')",
                [],
            )
            .unwrap();
            devices_dump(&conn)
        };

        let store = Store::open(&path).unwrap();
        // Minting still works with the column in place, which is what makes
        // leaving it an option at all.
        let now = mint(&store, "code-new", 300_000);
        assert_eq!(
            store.consume_pairing_code("code-new", now).unwrap(),
            PairingConsume::Consumed
        );
        // And the row that was in flight when the drop was refused is still
        // redeemable exactly once.
        assert_eq!(
            store.consume_pairing_code("code-inflight", now).unwrap(),
            PairingConsume::Consumed
        );
        drop(store);

        let mut conn = Connection::open(&path).unwrap();
        assert!(
            column_exists(&conn, "pairing_codes", "allow_ssh").unwrap(),
            "the column its index blocks stays, rather than taking the table down with it"
        );
        assert!(
            index_names(&conn, "pairing_codes").contains(&"codes_ssh".to_string()),
            "and so does the index that blocked it"
        );
        assert!(
            column_exists(&conn, "pairing_codes", "issued_by").unwrap(),
            "a column a newer build added must survive a drop this build could not make"
        );
        // Present is not enough: a rebuild that let the newer build widen the
        // table again would leave the name behind with the value gone.
        let issued_by: Option<String> = conn
            .query_row(
                "SELECT issued_by FROM pairing_codes WHERE code_hash = 'code-inflight'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(issued_by.as_deref(), Some("the newer build"));
        // The value the mint wrote into the leftover column: 0, "no SSH", which
        // is the truth about every code this build issues.
        let minted: i64 = conn
            .query_row(
                "SELECT allow_ssh FROM pairing_codes WHERE code_hash = 'code-new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(minted, 0);

        // What an operator is told, in the message that names the column.
        let warnings = drop_retired_columns(&mut conn).unwrap();
        assert_eq!(warnings.len(), 1, "one refusal, one message: {warnings:?}");
        assert!(
            warnings[0].contains("pairing_codes.allow_ssh"),
            "the message must name the column: {}",
            warnings[0]
        );
        assert_eq!(
            retired_columns_present(&conn).unwrap(),
            vec![("pairing_codes", "allow_ssh")],
            "the refusal is about one column and leaves the others gone"
        );
        // `devices` has no such obstacle and is not held back by its sibling.
        assert_eq!(devices_dump(&conn), devices_before, "a device row changed");
        assert!(!column_exists(&conn, "devices", "ssh_fingerprint").unwrap());
        assert!(!column_exists(&conn, "devices", "ssh_key_installed").unwrap());
    }

    /// The mint fills a leftover `allow_ssh` however the column is spelled.
    ///
    /// `ALLOW_SSH` and `allow_ssh` are one `NOT NULL` column to the `INSERT`, so
    /// a mint that compared the name byte for byte would pick the statement that
    /// omits it and fail every `codeconnect pair` on a database where the drop
    /// was refused. The index is what SQLite refuses the drop for.
    #[test]
    fn a_mint_fills_a_leftover_allow_ssh_however_it_is_spelled() {
        let path = legacy_path();
        {
            let conn = retired_credential_database(&path);
            conn.execute_batch(
                "DROP TABLE pairing_codes;
                 CREATE TABLE pairing_codes(
                     code_hash     TEXT PRIMARY KEY,
                     ALLOW_SSH     INTEGER NOT NULL,
                     created_at    TEXT    NOT NULL,
                     expires_at    TEXT    NOT NULL,
                     expires_at_ms INTEGER NOT NULL,
                     consumed_at   TEXT
                 );
                 CREATE INDEX codes_ssh ON pairing_codes(ALLOW_SSH);",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let now = mint(&store, "code-new", 300_000);
        assert_eq!(
            store.consume_pairing_code("code-new", now).unwrap(),
            PairingConsume::Consumed
        );
        drop(store);

        let conn = Connection::open(&path).unwrap();
        assert!(
            column_exists(&conn, "pairing_codes", "ALLOW_SSH").unwrap(),
            "the drop its index blocks leaves the column, whatever its spelling"
        );
        let minted: i64 = conn
            .query_row(
                "SELECT ALLOW_SSH FROM pairing_codes WHERE code_hash = 'code-new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(minted, 0);
    }

    /// A second daemon dropping `allow_ssh` cannot land between the mint's probe
    /// and the mint's `INSERT`.
    ///
    /// The probe exists because the answer can change between daemon starts, and
    /// it was asked per mint precisely so a remembered answer could not go stale.
    /// It was still stale by two statements: the drop below, fired at the one
    /// instant that matters, used to succeed and leave the `INSERT` naming a
    /// column that no longer existed — a `codeconnect pair` failing with "no such
    /// column: allow_ssh". Under one immediate transaction the write lock is
    /// already taken, so the drop is refused and the mint fills the column it
    /// saw. The property asserted is not *which* of the two schemas the mint
    /// works against, but that it works against the one it looked at.
    #[test]
    fn a_migration_cannot_land_between_the_mints_probe_and_its_insert() {
        let (store, path) = temp_store();
        // The column back as the one database SQLite refused it on carries it:
        // `NOT NULL` with no default. No index over it, so the only thing that
        // can refuse the drop below is the lock — an index would refuse it on a
        // database with the bug too, and prove nothing.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE pairing_codes;
                 CREATE TABLE pairing_codes(
                     code_hash     TEXT PRIMARY KEY,
                     allow_ssh     INTEGER NOT NULL,
                     created_at    TEXT    NOT NULL,
                     expires_at    TEXT    NOT NULL,
                     expires_at_ms INTEGER NOT NULL,
                     consumed_at   TEXT
                 );",
            )
            .unwrap();
        }

        let now = protocol::time::now_unix_ms();
        let expires = now + 300_000;
        {
            let _second_daemon = MigrationMidMint::armed_on(&path);
            store
                .create_pairing_code(
                    "code-raced",
                    &protocol::time::rfc3339_from_unix_ms(expires),
                    expires,
                    now,
                )
                .unwrap_or_else(|err| {
                    panic!("a migration landing mid-mint failed the mint: {err:#}")
                });
        }

        // And the code it minted is a real one: redeemable exactly once.
        assert_eq!(
            store.consume_pairing_code("code-raced", now).unwrap(),
            PairingConsume::Consumed
        );
        assert_eq!(
            store.consume_pairing_code("code-raced", now).unwrap(),
            PairingConsume::AlreadyUsed
        );

        let conn = Connection::open(&path).unwrap();
        assert!(
            column_exists(&conn, "pairing_codes", "allow_ssh").unwrap(),
            "the drop was taken while the mint held the write lock"
        );
        let minted: i64 = conn
            .query_row(
                "SELECT allow_ssh FROM pairing_codes WHERE code_hash = 'code-raced'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(minted, 0);
    }

    /// A retired column spelled in another case is the same column to SQLite,
    /// and is retired here too. A binary comparison would leave `ALLOW_SSH` on
    /// the table for ever, dead weight no start would ever look at again.
    #[test]
    fn a_retired_column_spelled_in_another_case_is_still_retired() {
        let path = legacy_path();
        {
            drop(Store::open(&path).unwrap());
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE pairing_codes;
                 CREATE TABLE pairing_codes(
                     code_hash     TEXT PRIMARY KEY,
                     ALLOW_SSH     INTEGER NOT NULL,
                     created_at    TEXT    NOT NULL,
                     expires_at    TEXT    NOT NULL,
                     expires_at_ms INTEGER NOT NULL,
                     consumed_at   TEXT
                 );
                 ALTER TABLE devices ADD COLUMN SSH_Fingerprint TEXT;",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let now = mint(&store, "code-new", 300_000);
        assert_eq!(
            store.consume_pairing_code("code-new", now).unwrap(),
            PairingConsume::Consumed
        );
        drop(store);

        let conn = Connection::open(&path).unwrap();
        assert!(retired_columns_present(&conn).unwrap().is_empty());
        assert!(!column_exists(&conn, "pairing_codes", "ALLOW_SSH").unwrap());
        assert!(!column_exists(&conn, "devices", "SSH_Fingerprint").unwrap());
    }

    /// Four daemons opening the same retired database at once all start, and the
    /// database they leave behind has lost nothing.
    ///
    /// `ALTER TABLE … DROP COLUMN` has no `IF NOT EXISTS`, so the recheck taken
    /// under the write lock is the whole difference between this and three
    /// daemons failing on a column the first one already removed.
    #[test]
    fn four_daemons_opening_the_same_retired_database_at_once_all_start() {
        let path = legacy_path();
        let before = {
            let conn = retired_credential_database(&path);
            for row in HOSTILE_DEVICES {
                seed_retired_device(&conn, row);
            }
            devices_dump(&conn)
        };

        let gate = std::sync::Arc::new(std::sync::Barrier::new(4));
        let opened: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    Store::open(&path).map(|store| store.list_devices().unwrap().len())
                })
            })
            .collect();
        for (i, handle) in opened.into_iter().enumerate() {
            let count = handle
                .join()
                .unwrap()
                .unwrap_or_else(|err| panic!("racing open {i} failed: {err:#}"));
            assert_eq!(
                count,
                HOSTILE_DEVICES.len(),
                "racing open {i} lost a device"
            );
        }

        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            devices_dump(&conn),
            before,
            "the race lost or changed a row"
        );
        assert!(retired_columns_present(&conn).unwrap().is_empty());
        assert!(index_names(&conn, "devices").contains(&"devices_name".to_string()));
    }

    /// Pairing works end to end afterwards: mint a code, redeem it once, refuse
    /// it the second time, refuse an expired one, and hand the phone a device
    /// row whose token authenticates.
    #[test]
    fn pairing_works_end_to_end_after_the_retired_columns_go() {
        let path = legacy_path();
        {
            let conn = retired_credential_database(&path);
            seed_retired_device(&conn, &HOSTILE_DEVICES[0]);
        }

        let store = Store::open(&path).unwrap();
        let now = mint(&store, "code-live", 300_000);
        // A minute in the past, not a millisecond: `mint` stamps each code from
        // its own clock reading, and the redemptions below are judged against
        // the *first* one. A code stamped one millisecond before the second
        // reading is not expired against the first unless both landed in the
        // same millisecond, which is a coin toss rather than an assertion.
        mint(&store, "code-stale", -60_000);

        assert_eq!(
            store.consume_pairing_code("code-live", now).unwrap(),
            PairingConsume::Consumed
        );
        assert_eq!(
            store.consume_pairing_code("code-live", now).unwrap(),
            PairingConsume::AlreadyUsed
        );
        assert_eq!(
            store.consume_pairing_code("code-stale", now).unwrap(),
            PairingConsume::Expired
        );

        store
            .insert_device("d-new", "iPad mini", "hash-new", "2026-08-04T00:00:00.000Z")
            .unwrap();
        assert_eq!(
            store
                .device_by_token_hash("hash-new")
                .unwrap()
                .expect("the phone that just paired must authenticate")
                .device_id,
            "d-new"
        );
        assert!(store.device_by_token_hash("hash-plain").unwrap().is_some());
    }

    /// The refusal reaches an operator at startup, and the daemon starts anyway.
    ///
    /// A child process, because the log goes to stderr and there is nothing in
    /// front of it a test could stand in. The child goes through `Store::open`
    /// rather than calling the removal, so the wiring is under test too: a
    /// `migrate()` that stopped calling it would fail here and nowhere else.
    #[test]
    fn a_refusal_reaches_an_operator_at_startup_without_stopping_the_daemon() {
        const CHILD: &str = "CCD_STARTUP_LOG_CHILD";
        if let Ok(database) = std::env::var(CHILD) {
            drop(Store::open(std::path::Path::new(&database)).unwrap());
            return;
        }

        let path = legacy_path();
        {
            let conn = retired_credential_database(&path);
            seed_retired_device(&conn, &HOSTILE_DEVICES[0]);
            conn.execute_batch("CREATE INDEX devices_ssh ON devices(ssh_fingerprint);")
                .unwrap();
        }

        let started = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "store::tests::a_refusal_reaches_an_operator_at_startup_without_stopping_the_daemon",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .output()
            .unwrap();
        let told = String::from_utf8_lossy(&started.stderr);
        assert!(
            started.status.success(),
            "the child daemon did not open: {told}"
        );
        assert!(
            told.contains(
                "WARN  schema: SQLite will not drop the retired column devices.ssh_fingerprint"
            ),
            "nothing an operator can read named the column: {told}"
        );
    }
}
