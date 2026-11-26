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
//! Everything is keyed by `session_uid`, never by the name. A name can be
//! reused, so keying by it made a new run inherit the dead one's log and
//! continue its `seq`.
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
use protocol::agent::AgentKind;
use protocol::event::{Event, EventKind, Lifecycle, PendingEvent, Source};
use protocol::host::Trust;
use protocol::pairing::DeviceSummary;
use protocol::proc_identity::{BootIdentity, ProcessIdentity};
use protocol::secret::Redacted;
use protocol::ws::{AnswerOutcome, FolderAnswer};
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
///     an idempotent normalizer**: it clears a *credential*, which usually is not
///     wrong — so running it every start would wipe every relay
///     bearer on every restart and leave relay push permanently re-registering.
///     It can therefore only fire on the transition, which is exactly what a
///     version bump buys. The push_credential *column* was added under version 2
///     without a bump (it is an additive `ALTER`); what needs the bump is the
///     one-shot data repair, not the column.
///   * `4` — agent-scoped session storage: Codex runs live in `codex_sessions`,
///     a second physical table with the same shape, and `sessions` goes back to
///     being Claude-only. See [`create_schema`] for why a second table rather
///     than the `agent` column that is already there.
///     The `sessions_refuse_codex_shadow` trigger ships with them, and it is the
///     part of this version a rolled-back binary keeps: a trigger lives in the
///     schema, so it goes on refusing v0.6.0's own upsert while v0.6.0 is the
///     one running.
///
///     **The bump is honest but load-bearing for nothing.** It is here because
///     the schema genuinely changed, and it makes a real v3-binary-opens-a-v4-
///     database downgrade happen for the first time. That downgrade is harmless
///     for a *measured* reason, not a hoped-for one: v0.6.0 reads
///     `user_version`, ignores what it finds, and unconditionally writes `3`
///     back — so a version fence could never have protected anything, and the
///     isolation cannot rest on one. It rests on the table name instead: v0.6.0
///     contains no statement that names `codex_sessions`.
///   * `5` — agent-scoped approval cards: a Codex run's open cards live in
///     `codex_pending_approvals`, and `pending_approvals` goes back to being
///     Claude-only. The same reasoning as `4` one table down: the old daemon
///     reads `pending_approvals` GLOBALLY, without walking a session row, so
///     `4`'s session split did not hide the cards. This split waited only
///     because nothing produced a Codex card; the approval observer is
///     that producer. Ships with `pending_approvals_refuse_codex_card`, which a
///     rollback keeps for the same reason the session trigger is kept, and with
///     the `all_pending_approvals` view for the reads that answer for both
///     agents. `answer_claims` and `text_mutations` are deliberately NOT split:
///     nothing writes a Codex row into either, and a table split ahead of its
///     producer is the speculative half-surface this design refuses.
///   * `6` — an ended run's transcript is forgotten, so no start reads a file a
///     later run has resumed; on the way up the rows that ended before are
///     forgotten once. A version gate, as `3` is. No schema changes, so an
///     older daemon reads everything; a rollback writes its own number back,
///     and the way back up forgets the rows it ended, once more.
///   * `7` — a Claude run's background tasks are filed ([`protocol::sdk_map::BACKGROUND_TASK`]); an earlier build
///     acknowledged them unfiled, so on the way up each Claude run whose host may still be running is filed one task
///     open whose id names none, once ([`file_unknown_tasks`]): it moves to another account only once that host has
///     ended. A version gate, as `6` is. No schema changes.
const SCHEMA_VERSION: i64 = 7;

pub struct Store {
    /// The only connection that writes. One, so `BEGIN IMMEDIATE` never has to
    /// wait out another in-process writer.
    writer: Mutex<Connection>,
    /// Read-only connections, taken by whichever is free.
    readers: Vec<Mutex<Connection>>,
    /// Round-robin start point, so `read()` does not always probe reader 0 and
    /// serialise everything behind it under light load.
    next_reader: std::sync::atomic::AtomicUsize,
    /// A test's database, set by [`Store::open_temp`]. Last, so the connections
    /// close before it goes.
    #[cfg(test)]
    temp: Option<TempDb>,
}

/// A test database that removes itself, siblings and all, once nothing holds it:
/// not the [`Store`] opened on it, and no clone a test kept to open it again.
///
/// SQLite leaves a `-wal` and a `-shm` beside the file it is given, so "delete the
/// db" has to mean all three. Not fussiness: a suite that leaves its files behind
/// on every run is how a machine ends up with gigabytes of them.
#[cfg(test)]
#[derive(Clone)]
pub struct TempDb(std::sync::Arc<TempDbFile>);

#[cfg(test)]
struct TempDbFile {
    db: std::path::PathBuf,
}

#[cfg(test)]
impl TempDb {
    pub fn new(stem: &str) -> TempDb {
        let file = TempDbFile {
            db: std::env::temp_dir().join(format!("{stem}.db")),
        };
        file.remove();
        TempDb(std::sync::Arc::new(file))
    }

    /// `<prefix>-<pid>-<n>-<ms>.db`: the counter is load-bearing, because tests
    /// run in parallel threads and a millisecond alone lets two share a file.
    pub fn unique(prefix: &str) -> TempDb {
        TempDb::new(&unique_stem(prefix))
    }

    pub fn path(&self) -> &Path {
        &self.0.db
    }
}

#[cfg(test)]
fn unique_stem(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        protocol::time::now_unix_ms()
    )
}

#[cfg(test)]
impl std::ops::Deref for TempDb {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.path()
    }
}

#[cfg(test)]
impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

#[cfg(test)]
impl TempDbFile {
    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.db.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(path));
        }
    }
}

#[cfg(test)]
impl Drop for TempDbFile {
    fn drop(&mut self) {
        self.remove();
    }
}

/// A folder sessions have started in, from the `projects` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub path: String,
    /// The newest start there.
    pub last_used: String,
    /// The agent of that start.
    pub last_agent: AgentKind,
}

/// A session as persisted. Presentation-level fields (`link`, `blocked_on`) live
/// in memory because they describe the daemon's current observation, not a fact
/// about the past.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// The run's identity, minted at spawn. Primary key.
    pub session_uid: String,
    /// The run's name. Reused across runs, so never an identity.
    pub session_id: String,
    pub cwd: String,
    pub claude_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub lifecycle: Lifecycle,
    pub created_at: String,
    pub updated_at: String,
    /// Which agent hosts this run. `Claude` for every legacy row and every row
    /// this build writes today; the column defaults to `'claude'` in SQLite, so
    /// a backfilled legacy row is Claude without a data repair. An unrecognised
    /// stored value decodes to [`AgentKind::Unsupported`], never Claude.
    pub agent: AgentKind,
    /// Codex thread identity, `None` for Claude and for a pre-seam row.
    pub codex_thread_id: Option<String>,
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
    pub tailnet_node: Option<String>,
    pub paired_before_nodes: bool,
}

impl DeviceRow {
    pub fn to_summary(&self) -> DeviceSummary {
        DeviceSummary {
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            created_at: self.created_at.clone(),
            last_seen_at: self.last_seen_at.clone(),
            revoked_at: self.revoked_at.clone(),
            tailnet_node: self.tailnet_node.clone(),
            paired_before_nodes: self.paired_before_nodes,
        }
    }
}

/// What a sign-in with neither a token nor a code came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    /// A revoked device carries this Tailscale device.
    Blocked,
    /// A device paired before devices were recorded was revoked with none, so
    /// this daemon cannot tell which Tailscale device to keep out.
    Paused,
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

/// The columns a [`PushRegistration`] is decoded from, in the order
/// [`push_registration`] reads them.
///
/// Shared by the fleet read and the per-device lookup so the two cannot drift:
/// a column added to one and not the other would be a decode that reads a
/// different row than it thinks it does.
const PUSH_TARGET_COLUMNS: &str = "SELECT device_id, push_token, \
     COALESCE(push_environment, 'sandbox'), push_credential \
     FROM devices";

/// Who may be pushed to at all — the authorization predicate, shared for the
/// same reason as the columns. A revoked device is excluded here rather than at
/// a call site, so revocation cannot leak through whichever read forgot it.
const PUSH_TARGET_ELIGIBLE: &str =
    "WHERE push_token IS NOT NULL AND push_token <> '' AND revoked_at IS NULL";

/// One row of [`PUSH_TARGET_COLUMNS`].
fn push_registration(row: &rusqlite::Row<'_>) -> rusqlite::Result<PushRegistration> {
    Ok(PushRegistration {
        device_id: row.get(0)?,
        token: row.get(1)?,
        environment: row.get(2)?,
        // Wrapped the instant it leaves the database, so the raw bearer
        // exists as a bare `String` only inside this function.
        credential: row.get::<_, Option<String>>(3)?.map(Redacted::from),
    })
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

/// The handshake's verdict on a presented token, from one read of the devices table.
#[derive(Debug)]
pub enum TokenAuth {
    /// A live pairing: this token admits this device.
    Active(Box<DeviceRow>),
    /// A pairing the computer has blocked. The token is kept; `unblock_device` readmits it.
    Revoked,
    /// No device carries this token.
    Missing,
}

/// Outcome of writing to the answers ledger.
#[derive(Debug, Clone)]
pub enum LedgerWrite {
    /// This answer won the race and is now the record.
    Recorded,
    /// Already resolved: the original stands and nothing was written.
    Existing,
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
}

/// The same card for a Codex run, plus the four columns that make one Codex
/// approval identifiable.
///
/// The six shared columns keep their names, types and meaning — the same
/// `ApprovalCard` in `card`, the same `(session_uid, request_id)` key the
/// in-memory `pending` map is already keyed by. Only the storage moved, and it
/// moved because a rolled-back v0.6.0 daemon reads `pending_approvals`
/// globally. See `create_schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPendingApprovalRow {
    pub session_uid: String,
    pub session_id: String,
    /// Derived from `(thread_id, item_id)`, never read off the wire — a Codex
    /// `serverRequest` id identifies nothing across the reconnect it would most
    /// need to. See [`Store::raise_codex_pending_approval`].
    pub request_id: String,
    /// Serialised [`protocol::ws::ApprovalCard`].
    pub card: String,
    pub generation: u64,
    pub created_ms: i64,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    /// `commandExecution` | `fileChange` | `mcpToolCall` | `requestUserInput`
    /// ([`crate::codex_approval::Family::as_str`]). The observe-only families never
    /// reach this table, because no card is built for them.
    pub family: String,
}

/// What one commit boundary decided about a Codex card.
///
/// There are only two answers because there is only one transaction: either the
/// row and the fact it stands behind both landed, or neither did.
#[derive(Debug)]
pub struct CodexCardRaise {
    /// What the commit decided. Only [`CodexCardOutcome::Filed`] and
    /// [`CodexCardOutcome::Rebound`] mean a card is open for this request; the
    /// other two rolled the transaction back and left nothing at all.
    pub outcome: CodexCardOutcome,
    /// The `ApprovalRequest` this raise appended, or `None` when an earlier
    /// sighting of the same item already filed one under the same `perm:` source
    /// id — a re-delivery, not a second question. Always `None` on an outcome
    /// that rolled back, because the same rollback took it.
    pub event: Option<Event>,
}

impl CodexCardRaise {
    /// Whether a card is now open for this request.
    pub fn is_open(&self) -> bool {
        matches!(
            self.outcome,
            CodexCardOutcome::Filed | CodexCardOutcome::Rebound
        )
    }
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

/// What `delete_exited_session` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted {
        events: u64,
    },
    /// Refused: the row has not ended (`Exited` or `Failed`). Carries the daemon's own word for what
    /// it is, so the refusal can be stated rather than guessed at.
    NotExited {
        lifecycle: String,
    },
    NotFound,
}

/// A run ccd started, of either agent, as its start request finds it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedRun {
    pub session_uid: String,
    pub cwd: String,
    pub lifecycle: Lifecycle,
}

/// A hosted run's recorded host, as ccd checks whether it is still there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedHost {
    pub session_uid: String,
    pub lifecycle: Lifecycle,
    pub host: ProcessIdentity,
    /// The boot the host was recorded in; `None` when it could not be read then.
    pub boot: Option<BootIdentity>,
}

/// A Codex run ccd hosts, as a restarted ccd connects to it again while it is `live`
/// ([`Store::codex_live_runs`]), or as a message relaunches it once it has ended
/// ([`Store::begin_relaunch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCodexRun {
    pub session_uid: String,
    /// Its recorded app-server: for a relaunch, the one that ended; none for a run its switch left with no app-server
    /// recorded ([`Store::fail_codex_starts`]).
    pub host: Option<WatchedHost>,
    /// The folder it runs in.
    pub cwd: String,
    /// The thread its first message started.
    pub thread: String,
    /// The folder answer it started with, kept in its `start` record; a record that is
    /// missing or unreadable reads not trusted.
    pub trust: protocol::host::Trust,
}

/// What a link to a recorded host starts from ([`Store::hosted_link`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedLink {
    pub lifecycle: Lifecycle,
    /// The highest host message whose effects are committed.
    pub cursor: u64,
    /// The run's `start` line, as ccd sent or would send it.
    pub start: Option<String>,
    /// The uuid of the run's last SDK init, which its turn's later events carry.
    pub turn: Option<String>,
    /// The account its newest relaunch was launched on (inside, `None` for Default); `None` when not recorded.
    pub launched: Option<Option<String>>,
}

/// What one host message tells a hosted run's row besides its events
/// ([`Store::append_host_message`]): from an init, the run's Claude session id, the
/// folder claude runs in and the commands it offers on its own screen only; from a
/// `commands_changed`, claude's command list.
#[derive(Debug, Default)]
pub struct RunNotes {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub screen_only: Option<Vec<String>>,
    pub commands: Option<Vec<serde_json::Value>>,
}

/// What a hosted run's command menu is read from ([`Store::hosted_commands`]).
#[derive(Debug, Clone, PartialEq)]
pub struct HostedCommands {
    /// claude's command list as it last reported it, each command as it gives it;
    /// `None` before the start is done.
    pub commands: Option<Vec<serde_json::Value>>,
    /// When ccd took that list.
    pub commands_at: Option<String>,
    /// The commands the run's latest init names as its screen's only; `None` before
    /// the first init.
    pub screen_only: Option<Vec<String>>,
    /// The version of the claude that answered the start.
    pub claude_version: Option<String>,
}

fn watched_host_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<WatchedHost> {
    let boot = match (row.get::<_, Option<i64>>(5)?, row.get::<_, Option<i64>>(6)?) {
        (Some(boot_sec), Some(boot_usec)) => Some(BootIdentity {
            boot_sec,
            boot_usec,
        }),
        _ => None,
    };
    Ok(WatchedHost {
        session_uid: row.get(0)?,
        lifecycle: parse_lifecycle(&row.get::<_, String>(1)?),
        host: ProcessIdentity {
            pid: row.get(2)?,
            birth: protocol::proc_identity::BirthIdentity {
                start_sec: row.get(3)?,
                start_usec: row.get(4)?,
            },
        },
        boot,
    })
}

/// Whether `host` is the host recorded for the hosted run `session_uid`: the launch a caller
/// inspected is still the run's.
fn recorded_host_is(conn: &Connection, session_uid: &str, host: &ProcessIdentity) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_hosts WHERE session_uid = ?1 AND host_pid = ?2
                          AND host_start_sec = ?3 AND host_start_usec = ?4)",
        params![
            session_uid,
            host.pid,
            host.birth.start_sec,
            host.birth.start_usec
        ],
        |row| row.get(0),
    )?)
}

/// A [`LiveCodexRun`] from a row of [`WatchedHost`]'s seven columns, then the run's folder, its
/// thread and its `start` record.
fn live_codex_run_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<LiveCodexRun> {
    let start: Option<String> = row.get(9)?;
    Ok(LiveCodexRun {
        session_uid: row.get(0)?,
        host: match row.get::<_, Option<i32>>(2)? {
            Some(_) => Some(watched_host_from(row)?),
            None => None,
        },
        cwd: row.get(7)?,
        thread: row.get(8)?,
        trust: start
            .and_then(|start| serde_json::from_str(&start).ok())
            .unwrap_or(protocol::host::Trust::NotTrusted),
    })
}

/// The hosted Codex run `session_uid` when the person's next message relaunches it (P3 R4): it
/// is `exited`, the last end it filed reads "Stopped: its process ended"
/// ([`crate::claude_host::PROCESS_ENDED`]), so its app-server ended without ccd stopping it, or "Switched to …"
/// ([`crate::switch::SWITCHED`]), so ccd stopped it to move it to another account, and
/// it has a thread and a recorded app-server, or a switch, whose relaunch a restart may have left with none. A run
/// ended by End run ("Stopped") or a start that failed is not one. No column says how a run ended; its end's
/// `reason` does.
fn relaunchable_in(conn: &Connection, session_uid: &str) -> Result<Option<LiveCodexRun>> {
    let (table, _) = session_tables_for(&AgentKind::Codex);
    Ok(conn
        .query_row(
            &format!(
                "SELECT h.session_uid, s.lifecycle, h.host_pid, h.host_start_sec, h.host_start_usec,
                        h.boot_sec, h.boot_usec, s.cwd, s.codex_thread_id, h.start
                   FROM run_hosts AS h JOIN {table} AS s USING(session_uid)
                  WHERE h.session_uid = ?1 AND s.lifecycle = ?2
                    AND (h.host_pid IS NOT NULL OR h.switching IS NOT NULL)
                    AND s.codex_thread_id IS NOT NULL
                    AND EXISTS (SELECT 1 FROM (SELECT json_extract(payload, '$.reason') AS reason FROM events
                                                WHERE session_uid = ?1 AND kind = ?3
                                                ORDER BY seq DESC LIMIT 1)
                                 WHERE reason = ?4 OR reason GLOB ?5)"
            ),
            params![
                session_uid,
                lifecycle_str(Lifecycle::Exited),
                EventKind::SessionEnd.as_str(),
                crate::claude_host::PROCESS_ENDED,
                format!("{}*", crate::switch::SWITCHED)
            ],
            live_codex_run_from,
        )
        .optional()?)
}

/// What a message's relaunch of an ended hosted Claude run starts from ([`Store::claude_relaunch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeRelaunch {
    /// The run's kept `start` line: its folder answer, its model and its mail server.
    pub start: String,
    /// The session claude named, which the relaunch resumes.
    pub claude_session_id: String,
    /// The folder claude ran in, as its init named it.
    pub cwd: String,
}

/// The hosted Claude run `session_uid` when the person's next message relaunches it (decision
/// 210): it is `exited`, the last end it filed reads "Stopped: its process ended"
/// ([`crate::claude_host::PROCESS_ENDED`]), so its claude ended without ccd stopping it, or "Switched to …"
/// ([`crate::switch::SWITCHED`]), so ccd stopped it to move it to another account, and
/// claude named its session, which `--resume` needs. A recorded host is not required: one an
/// earlier relaunch never recorded left no claude. A run ended by End run ("Stopped") or a start
/// that failed is not one.
fn claude_relaunchable_in(conn: &Connection, session_uid: &str) -> Result<Option<ClaudeRelaunch>> {
    let (table, _) = session_tables_for(&AgentKind::Claude);
    Ok(conn
        .query_row(
            &format!(
                "SELECT h.start, s.claude_session_id, s.cwd
                   FROM run_hosts AS h JOIN {table} AS s USING(session_uid)
                  WHERE h.session_uid = ?1 AND s.lifecycle = ?2 AND h.start IS NOT NULL
                    AND s.claude_session_id IS NOT NULL
                    AND EXISTS (SELECT 1 FROM (SELECT json_extract(payload, '$.reason') AS reason FROM events
                                                WHERE session_uid = ?1 AND kind = ?3
                                                ORDER BY seq DESC LIMIT 1)
                                 WHERE reason = ?4 OR reason GLOB ?5)"
            ),
            params![
                session_uid,
                lifecycle_str(Lifecycle::Exited),
                EventKind::SessionEnd.as_str(),
                crate::claude_host::PROCESS_ENDED,
                format!("{}*", crate::switch::SWITCHED)
            ],
            |row| {
                Ok(ClaudeRelaunch {
                    start: row.get(0)?,
                    claude_session_id: row.get(1)?,
                    cwd: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn hosted_run_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostedRun> {
    Ok(HostedRun {
        session_uid: row.get(0)?,
        cwd: row.get(1)?,
        lifecycle: parse_lifecycle(&row.get::<_, String>(2)?),
    })
}

/// Where a run of this agent is stored, and where it therefore must not be.
///
/// Returns `(table, other)`: the table a run of `agent` lives in, and the other one.
///
/// **Everything that is not a hosted run goes to `codex_sessions`**, including an
/// [`AgentKind::Unsupported`] run. That is deliberate: `sessions` is the table a
/// rolled-back v0.6.0 daemon sweeps, and an agent this build understands even less than
/// Codex is not the one to leave in it. The table is named for the agent it was
/// built for, not for the only agent it can ever hold.
///
/// An Antigravity run is a hosted run, its agent host's as a Claude run's is, so it lives
/// in `sessions`: the hosts this ccd attaches again are read from there, and every path
/// over `codex_sessions` takes a recorded host as a Codex app-server.
fn session_tables_for(agent: &AgentKind) -> (&'static str, &'static str) {
    if matches!(agent, AgentKind::Claude | AgentKind::Antigravity) {
        ("sessions", "codex_sessions")
    } else {
        ("codex_sessions", "sessions")
    }
}

/// Both tables a session row can live in, for the paths that must reach a run
/// without first knowing which agent it belongs to.
///
/// `sessions` first, so a Claude run — still the overwhelming majority — is
/// found by the first statement and the second is a no-op on an indexed miss.
const SESSION_TABLES: &[&str] = &["sessions", "codex_sessions"];

/// Everything a run owns, besides its own identity row in one of
/// [`SESSION_TABLES`].
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
///
/// These stay **shared between the two agents**, and that is measured rather
/// than assumed: a rolled-back v0.6.0 daemon reaches `events`, `tail_cursors`
/// and `mutation_ledger` only by a uid it walked from a `sessions` row, and
/// after the move there is no such row for a Codex run. The other four —
/// `answers`, `pending_approvals`, `answer_claims`, `text_mutations` — it reads
/// *globally*, so they carry the open obligation described on [`create_schema`].
const SESSION_SCOPED_TABLES: &[&str] = &[
    "events",
    "answers",
    "pending_approvals",
    // Agent-scoped, and still swept by THIS daemon: isolation from a rolled-back
    // v0.6.0 is not a licence to leak rows here. A deleted Codex run takes its
    // open cards with it exactly as a Claude one does.
    "codex_pending_approvals",
    "answer_claims",
    "text_mutations",
    "mutation_ledger",
    "mail",
    "mail_sent",
    "mail_wake",
    "mail_runs",
    "run_hosts",
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

/// One message for a session on this computer, as its receiver's daemon takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailIntake {
    pub message_id: String,
    pub context_id: String,
    pub in_reply_to: Option<String>,
    /// The receiving run.
    pub session_uid: String,
    /// The sending computer's tailnet name, or empty for this computer.
    pub sender_computer: String,
    pub sender_uid: String,
    /// `<run uid>@<computer>`.
    pub sender: String,
    /// `cc-N@<computer>`.
    pub sender_label: String,
    pub sender_agent: String,
    pub sender_project: String,
    pub text: String,
}

/// What taking a message found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailTaken {
    /// Kept, unread.
    Kept,
    /// A message with this id was taken before: where it stands now. Nothing was kept.
    Known(protocol::ipc::MailState),
    /// The sender has sent this receiver [`MailLimits::per_pair_per_hour`] messages
    /// in the last hour.
    RateLimited,
    /// The receiving run has no session row.
    NoSession,
}

/// The bounds the daemon puts on mail, passed in so tests can name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailLimits {
    /// How long an unread message is kept.
    pub unread_ms: i64,
    /// How many messages one sender may leave one receiver in an hour.
    pub per_pair_per_hour: u32,
    /// How long a settled message is kept before it is dropped.
    pub settled_ms: i64,
}

/// A batch offered to a reader: its messages, and the changes in where its own
/// sent messages stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailOffer {
    pub messages: Vec<protocol::ipc::MailEnvelope>,
    pub updates: Vec<protocol::ipc::MailUpdate>,
    pub more_pending: bool,
}

/// One message this computer's agent sent, kept before it goes anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMail {
    pub message_id: String,
    pub context_id: String,
    pub in_reply_to: Option<String>,
    /// The sending run.
    pub session_uid: String,
    /// The address it was sent to, `cc-N@<computer>`.
    pub recipient: String,
    /// The receiving computer's tailnet name, or empty for this computer.
    pub recipient_computer: String,
    pub text: String,
}

/// A state change a sender on another computer has not been told yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntoldMail {
    pub message_id: String,
    pub sender_computer: String,
    pub state: protocol::ipc::MailState,
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

/// The immutable material a mutation was claimed with — enough to replay the
/// **original** route on a retry, not just recognise it. Written once at claim
/// and returned verbatim to a duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedMaterial {
    pub thread_id: String,
    pub generation: u64,
    /// **The snapshotted route, in the operation kind's own vocabulary.** A
    /// compose is `"turn_start"` or `"turn_steer"`; an answer is the id of the
    /// option the phone named, which is the only thing "which way did this
    /// actuation go?" can mean for a decision, and an answer to a Codex question
    /// card is `"answers"` or `"decline"`.
    pub route: String,
    pub target_turn_id: Option<String>,
    pub claimed_hash: String,
    /// **An answer to a Codex question card, kept whole**: the `Answers` or `Decline`
    /// the app sent, so the resolution filed and the reply sent again after a restart
    /// are built from the person's own answers rather than from `route`. `None` for
    /// every other claim.
    pub question_decision: Option<protocol::ws::AnswerDecision>,
}

/// **How a phone answer's ledger row is closed in the commit that retires its
/// card.**
///
/// "Nothing was written, and that is provable" never reaches a durable row at
/// all: the claim is taken only once the live connection has accepted the ask,
/// so an answer that cannot be addressed leaves the ledger untouched and the
/// card answerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerTerminal {
    /// The upstream write is proven. A duplicate replays this outcome rather than
    /// writing a second response to a request the app-server answers once.
    Settled(&'static str),
}

/// **Where one phone answer's claim stands, durably.**
///
/// The ledger's own three states, named rather than spelled: a gate that has to read
/// a terminal back — because the card it belonged to is no longer there to read —
/// must not do it by string-matching a column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerStatus {
    /// Claimed, and nothing has settled it. A daemon that stops here leaves the row
    /// recovery makes terminal.
    Applying,
    /// Settled with a proven outcome, which a duplicate replays.
    Settled(String),
    /// Terminal, and nothing can say what it did. Never retried.
    Indeterminate,
}

/// **Where one claim stands, together with the material it was claimed with.**
///
/// The two are read as one because a replay needs both and reading them apart is
/// where the law goes wrong: the status alone says an id has been used before, and
/// only the material says whether it was used for *this*. An id reused for a
/// different turn wears the same status as an honest retry, and answering from the
/// status alone tells an operator the turn in front of them has already been stopped.
///
/// The same all-field comparison [`Store::claim_mutation`] makes before it will call
/// a second ask a duplicate, made available to the readers that decide before a claim
/// is attempted at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationState {
    pub status: AnswerStatus,
    /// The material the row was claimed with, verbatim.
    pub claimed: ClaimedMaterial,
}

/// One unsettled claim under any operation kind, for recovery to make terminal.
///
/// Named for the ledger rather than for one of its operations: the row is the same
/// row whichever kind wrote it, and a type called after the first caller would have
/// to be either renamed or lied about by the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationClaimRow {
    pub session_uid: String,
    /// The durable id the phone retried under. For an answer it is the card's
    /// item-derived id; for an interrupt it is the id the ask carried.
    pub client_request_id: String,
    /// The material the mutation was claimed with. `route` is the operation kind's
    /// own word for which way the actuation went, which is what lets a recovered
    /// terminal say what was attempted.
    pub claimed: ClaimedMaterial,
    pub started_at: String,
}

/// A `mutation_ledger` row's `question_decision` column at `index`, read back as the
/// decision it was written from ([`ClaimedMaterial::question_decision`]).
fn question_decision_column(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<protocol::ws::AnswerDecision>> {
    row.get::<_, Option<String>>(index)?
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(err),
            )
        })
}

/// **The two outcomes a settled answer claim records.**
///
/// Spelled once so a duplicate replays the same word the first attempt wrote, and so a
/// reader that has to tell them apart cannot do it with a literal that drifts.
pub const ANSWER_DELIVERED: &str = "delivered";
pub const ANSWER_LOST: &str = "lost";

/// **What a settled answer claim is replayed as, in the operator's words.**
///
/// `Settled` is the ledger's word for *terminal*, and it carries two facts that are each
/// other's opposite. `delivered` is an answer the broker confirmed reached the
/// app-server; `lost` is an answer that forwarded ZERO bytes because something else
/// settled the request first, and whose card was deliberately left standing for that
/// other terminal to retire. Both are replayed here, because both mean "this card will
/// not be answered again" — but telling an operator their tap was applied when the
/// record says it was not is the one thing this sentence must never do.
///
/// Neither names the device that answered: the claim stores none, and the answer may
/// have come from a phone, the Mac app, or a client on the static token.
///
/// An outcome this build has no words for still refuses, naming the word rather than
/// guessing at it: the terminal is what governs, and the vocabulary is not.
pub fn replayed_answer_sentence(outcome: &str) -> String {
    match outcome {
        ANSWER_DELIVERED => "this card was already answered; nothing was sent again".to_string(),
        ANSWER_LOST => "an earlier answer to this card lost the race — something else \
                        answered it first — so nothing was sent again"
            .to_string(),
        other => format!("this card's answer is already settled ({other}); nothing was sent again"),
    }
}

/// **The one route an interrupt can take.**
///
/// `route` is the ledger's field for "which way did this actuation go?", and for an
/// interrupt there is exactly one way: stop the turn. Written anyway rather than left
/// empty, because the column is part of the claimed material a duplicate is compared
/// against, and an empty string is a value a bug can also produce.
pub const INTERRUPT_ROUTE: &str = "stop";

/// **The three outcomes a settled interrupt claim records.**
///
/// Spelled once so a duplicate replays the same word the first attempt wrote, and so
/// a reader that has to tell them apart cannot do it with a literal that drifts.
/// They are the three things that can become of a written interrupt, and no fourth:
/// the turn stopped, the turn ended for its own reasons first, or the write was
/// refused with a reason.
///
/// The turn reached its terminal with `interrupted`, which is this ask taking effect
/// and the only outcome that stopped anything.
pub const INTERRUPT_ABORTED: &str = "aborted";
/// The turn ended for its own reasons while the ask was in flight. Nothing was
/// stopped from here, and the record must not let anyone say otherwise.
pub const INTERRUPT_TURN_ENDED: &str = "turn_ended";
/// The write itself was refused. Written by earlier builds for every refusal from the
/// wire, and read back from their rows: replayed, never re-sent. This build gives back a
/// refusal that proves nothing reached Codex ([`Store::release_mutation`]) and records any
/// other answer as `indeterminate`.
pub const INTERRUPT_REFUSED: &str = "refused";

/// **What a settled interrupt claim is replayed as, in the operator's words.**
///
/// The same rule [`replayed_answer_sentence`] keeps, and for the same reason: telling
/// somebody their tap stopped a turn when the record says it did not is the one thing
/// this sentence must never do. An outcome this build has no words for names itself
/// rather than being guessed at — the terminal is what governs, not the vocabulary.
pub fn replayed_interrupt_sentence(outcome: &str) -> String {
    match outcome {
        INTERRUPT_ABORTED => crate::codex_refusals::REPLAY_INTERRUPT_ABORTED.to_string(),
        INTERRUPT_TURN_ENDED => crate::codex_refusals::REPLAY_INTERRUPT_TURN_ENDED.to_string(),
        INTERRUPT_REFUSED => crate::codex_refusals::REPLAY_INTERRUPT_REFUSED.to_string(),
        other => crate::codex_refusals::replay_interrupt_unknown_word(other),
    }
}

/// **The one `operation_kind` a phone answer is claimed under.**
///
/// Spelled once so a typo is a compile error rather than a claim nothing can find
/// again — the same reason every other wire vocabulary in this file is a constant.
pub const OPERATION_ANSWER: &str = "answer";

/// **The `operation_kind` a phone interrupt is claimed under.**
///
/// The second producer for the generalized ledger, and the reason the ledger was
/// generalized: an interrupt is a mutation with the same idempotency law as an
/// answer — claimed before the write, settled by the outcome, replayed rather than
/// re-actuated — differing only in what it writes and what tells it what happened.
///
/// Spelled once for the same reason [`OPERATION_ANSWER`] is: a typo would be a claim
/// nothing can find again rather than a compile error. No schema change goes with
/// it — the column has always been a string and the ledger has always been keyed by
/// it.
pub const OPERATION_INTERRUPT: &str = "interrupt";

/// **The `operation_kind` a phone compose is claimed under.**
///
/// ONE kind for both routes, not two, and the schema said so before the subject existed:
/// the DDL names "answer, compose, interrupt" as the three kinds, and
/// [`ClaimedMaterial::route`] reserves `"turn_start"` and `"turn_steer"` as a compose's
/// two routes. That split is what the retry law needs. A compose's identity is the WORDS —
/// the phone composed a message and is retrying the same message — while whether those
/// words start a turn or join one is a fact about the session at the instant of the write.
/// Two kinds would make one message under one id two different mutations, so an honest
/// retry of an unacknowledged send would be claimed afresh under the other kind and the
/// words would be said twice. One kind with the route in the immutable material is the
/// shape where a retry replays what actually happened.
pub const OPERATION_COMPOSE: &str = "compose";

/// A compose that began a turn on an idle thread.
pub const COMPOSE_ROUTE_START: &str = "turn_start";
/// A compose that joined the turn the session was already running.
pub const COMPOSE_ROUTE_STEER: &str = "turn_steer";
/// A compose the wire refused. Written by earlier builds for every refusal from the wire,
/// and read back from their rows: terminal, and replayed as a refusal rather than re-sent.
/// This build gives back a refusal that proves nothing reached Codex
/// ([`Store::release_mutation`]) and records any other answer as `indeterminate`.
pub const COMPOSE_REFUSED: &str = "refused";
/// A compose of an engine run that joined a running turn and was dropped when that turn
/// was stopped: Codex recorded no message carrying its id before the turn's interrupted
/// end. Settled with its `send_not_sent` notice, and replayed as a refusal.
pub const COMPOSE_STOPPED: &str = "stopped";

/// **The recorded outcome of a compose that reached the model**, which is the route it
/// took and the turn that heard it.
///
/// The turn is in the OUTCOME rather than in the claimed material because for a start it
/// is not known at claim time — the app-server mints it and reports it in `turn/started`.
/// (For a steer it is known, and is also in `target_turn_id`; recording it both ways keeps
/// one reader for both routes.)
pub fn compose_outcome(route: &str, turn_id: &str) -> String {
    format!("{route} {turn_id}")
}

/// The inverse of [`compose_outcome`]: `(route, turn_id)`, or `None` for an outcome that
/// named no turn (a refusal).
///
/// `split_once` rather than a whitespace split, so a turn id that somehow contained a
/// space round-trips whole rather than being silently truncated to its first word.
pub fn parse_compose_outcome(outcome: &str) -> Option<(&str, &str)> {
    let (route, turn) = outcome.split_once(' ')?;
    if turn.is_empty() {
        return None;
    }
    // **The route is one of the two, or this row is unreadable.**
    //
    // It used to return whatever the first token was, and `replayed_compose_report` then
    // asked `route == COMPOSE_ROUTE_START` — so any other word replayed to the phone as
    // `Duplicate{started:false}`, which reads as "your words joined a running turn". A
    // corrupt row, or a third route a future build writes and this one does not know,
    // would say that about words it cannot account for. Refusing to read it is the only
    // honest answer, and the caller already has one: an outcome that names no turn
    // replays as a plain refusal.
    if !matches!(route, COMPOSE_ROUTE_START | COMPOSE_ROUTE_STEER) {
        return None;
    }
    Some((route, turn))
}

/// The sentence a replayed compose is told, for an outcome that named no turn.
pub fn replayed_compose_sentence(outcome: &str) -> String {
    match outcome {
        COMPOSE_REFUSED => crate::codex_refusals::REPLAY_COMPOSE_REFUSED.to_string(),
        COMPOSE_STOPPED => crate::codex_refusals::REPLAY_COMPOSE_STOPPED.to_string(),
        // **The outcome is not interpolated.** A row this build cannot read carries a
        // turn id the phone never sent — `parse_compose_outcome` refuses an unknown route
        // and lands here — and there is nothing in the stored word a caller can act on
        // beyond the fact that this id is spent.
        _ => crate::codex_refusals::REPLAY_COMPOSE_SETTLED.to_string(),
    }
}

/// Close one answer claim inside a caller's transaction, **or fail the whole
/// transaction**.
///
/// **First terminal wins**, and that is what the `status = 'applying'` guard is:
/// a claim recovery already made `indeterminate` is the record that this card
/// must never be answered again, and a late disposition arriving afterwards would
/// otherwise overwrite it with a cheerful outcome for an answer nobody can prove
/// was sent.
///
/// **Exactly one row, and the count is the whole point of checking it.** Sharing a
/// transaction with the card's deletion proves that the two statements ran together; it
/// does not prove the second one did anything. A guarded `UPDATE` that matched nothing
/// succeeds, so discarding the count let
/// [`Store::retire_codex_pending_approval`] delete the card and file a resolution naming
/// the PHONE while settling zero applying claims — the exact state a recovery that
/// already made the claim `indeterminate` leaves behind, and the state a claim that was
/// never taken is in.
///
/// Every caller that passes an [`AnswerTerminal`] holds a live `applying` claim by
/// construction (the link takes it at the one moment it knows the answer is about to be
/// written), so anything but one row is a contradiction rather than a case. It returns
/// `Err`, which rolls the caller's transaction back: the card stays, the resolution is
/// not filed, and recovery makes the terminal on the next start.
fn settle_answer_in_tx(
    tx: &rusqlite::Transaction<'_>,
    session_uid: &str,
    client_request_id: &str,
    answer: AnswerTerminal,
    at: &str,
) -> Result<()> {
    let AnswerTerminal::Settled(outcome) = answer;
    let settled = tx.execute(
        "UPDATE mutation_ledger SET status = 'done', outcome = ?4, settled_at = ?5
          WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3
            AND status = 'applying'",
        params![
            OPERATION_ANSWER,
            session_uid,
            client_request_id,
            outcome,
            at
        ],
    )?;
    if settled != 1 {
        return Err(anyhow::anyhow!(
            "settling the answer claim for {client_request_id} in {session_uid} matched \
             {settled} applying row(s), not one: the card must not be retired against a \
             claim this commit did not settle"
        ));
    }
    Ok(())
}

/// What claiming a generalized [`mutation_ledger`](Store::claim_mutation) row
/// found — the same taxonomy as [`TextClaim`], widened to any operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationClaim {
    /// Nothing under this key: the caller now owns it and may actuate.
    Claimed,
    /// Already done. Replay the recorded outcome against the **claimed** route;
    /// actuate nothing new.
    Applied {
        outcome: String,
        claimed: ClaimedMaterial,
    },
    /// A claim nobody settled. Never retried automatically; the claimed material
    /// is returned so recovery can reason about the original route.
    Indeterminate {
        started_at: String,
        claimed: ClaimedMaterial,
    },
    /// The same `(operation_kind, session_uid, client_request_id)` carrying
    /// different claimed material. Refused, never a second actuation.
    Conflict,
    /// The session was deleted while the request was in flight, so nothing was
    /// claimed. Distinct from `Claimed`: there is nothing to actuate against.
    NoSession,
}

/// The account a run records ([`Store::run_accounts`]): `None` for its agent's Default, else its id and its label
/// (`None` once the account is removed).
pub type RecordedAccount = Option<(String, Option<String>)>;

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
            #[cfg(test)]
            temp: None,
        };
        store.migrate()?;
        Ok(store)
    }

    /// A store on a fresh test database named after `prefix` (see
    /// [`TempDb::unique`]), removed when the store and every clone of
    /// [`Store::temp_db`] are dropped.
    #[cfg(test)]
    pub fn open_temp(prefix: &str) -> Store {
        let db = TempDb::unique(prefix);
        let mut store = Store::open(db.path()).unwrap();
        store.temp = Some(db);
        store
    }

    /// The database [`Store::open_temp`] made, for a test that opens it again.
    #[cfg(test)]
    pub fn temp_db(&self) -> TempDb {
        self.temp.clone().expect("a store from Store::open_temp")
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.write();
        // Read *before* anything below writes it: this is the version the
        // database was last left at, and the only thing that can tell a
        // one-shot repair from a restart. `0` on a database this build has never
        // opened — a fresh file, or one from before `user_version` was used —
        // and every one-shot gate below reads that as "everything is new here".
        let from_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // Before the transaction below, so that `create_schema` recreates in this same
        // open the `all_sessions` view it drops; and fatal, because a session table that
        // keeps one of its retired columns refuses every run this build begins. See
        // [`drop_retired_columns`].
        drop_retired_columns(&mut conn)?;
        // **One `BEGIN IMMEDIATE` over the whole migration.** Every statement below is
        // transactional in SQLite — `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW` and
        // `ALTER TABLE … ADD COLUMN` all roll back cleanly, measured rather than
        // assumed. `create_schema` is one `execute_batch` of pure DDL with no
        // transaction control of its own, so it nests here without a second `BEGIN`.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Asked before anything widens `devices`: a table that already exists
        // without `tailnet_node` holds pairings made before devices were recorded.
        // Asked of the schema, not of `user_version`, which a rollback rewrites.
        let pairings_predate_nodes =
            table_exists(&tx, "devices")? && !column_exists(&tx, "devices", "tailnet_node")?;
        create_schema(&tx)?;
        // **Every run without an engine host record goes, with everything filed under
        // it**: the runs the tmux-era engine made, which nothing in this build can host,
        // end or answer. Every start, so a crash between the drops above and this commit
        // is finished by the next open. The children first, then the identity rows.
        let mut removed = 0usize;
        for table in SESSION_SCOPED_TABLES.iter().chain(SESSION_TABLES) {
            // A compile-time list in this file; nothing a caller supplies reaches the
            // statement text.
            let gone = tx.execute(
                &format!(
                    "DELETE FROM {table}
                      WHERE session_uid NOT IN (SELECT session_uid FROM run_hosts)"
                ),
                [],
            )?;
            if SESSION_TABLES.contains(table) {
                removed += gone;
            }
        }
        if removed > 0 {
            crate::log_info!(
                "schema: removed {removed} run(s) the tmux-era engine made, with their history"
            );
        }
        // Additive, and applied after `create_schema` for the reason the note
        // above gives in reverse: `CREATE TABLE IF NOT EXISTS` will not widen a
        // table that already exists, so a database written before push existed
        // keeps its old `devices` shape and every push statement fails on a
        // missing column.
        if needs_column_additions(&tx)? {
            add_missing_columns(&tx)?;
        }
        // Every row, revoked or not: a device paired before this release and
        // revoked after it, before it ever connected through Tailscale, has no
        // node either, and must keep out what it was just as a revocation from
        // before the upgrade does.
        if pairings_predate_nodes {
            let marked = tx.execute("UPDATE devices SET paired_before_nodes = 1", [])?;
            crate::log_info!(
                "schema: marked {marked} device(s) as paired before Tailscale sign-in"
            );
        }
        // The predicate asks whether a card's uid is a Codex run, and the only
        // witness of that is a `codex_sessions` row.
        //
        // Not gated on `from_version`: a rollback resets the number, and the rows
        // are the only honest question.
        if needs_codex_card_move(&tx)? {
            let moved = move_codex_pending_approvals(&tx)?;
            crate::log_info!(
                "schema: took {moved} approval card(s) out of the shared pending_approvals \
                 table, where a rolled-back v0.6 daemon enumerates and deletes them without \
                 going through a sessions row"
            );
        }
        // Every start, not once: a run filed by a build without the projects
        // table, before an upgrade or after a rollback, is folded in here. It
        // follows `add_missing_columns` because it reads `agent`.
        tx.execute(
            &format!(
                "{RECORD_PROJECTS} SELECT cwd, created_at, agent FROM all_sessions
                  WHERE true {RECORD_PROJECTS_NEWEST}"
            ),
            [],
        )?;
        // The one-shot for `7`: see [`SCHEMA_VERSION`].
        if from_version < 7 {
            let filed = file_unknown_tasks(&tx)?;
            if filed > 0 {
                crate::log_info!(
                    "schema: {filed} running Claude run(s) keep their account until their process ends: what \
                     they run in the background was not recorded before"
                );
            }
        }
        tx.commit()?;
        // An earlier build's transcript cursors, name tombstones and run tombstones:
        // nothing reads them, and a cursor names a run's transcript, so they do not
        // outlive the code that kept them. Every start.
        conn.execute_batch(
            "DROP TABLE IF EXISTS tail_cursors; DROP TABLE IF EXISTS deleted_names; \
             DROP TABLE IF EXISTS deleted_sessions;",
        )?;
        // The one-shot push-tuple repair — see [`SCHEMA_VERSION`] on why it is
        // gated. Ordered after the column additions so the columns it names are
        // present on a legacy database.
        if from_version < 3 {
            normalize_push_tuples(&conn)?;
        }
        // The one-shot for `6`: see [`SCHEMA_VERSION`].
        if from_version < 6 {
            conn.execute(
                "UPDATE sessions SET transcript_path = NULL WHERE lifecycle = 'exited'",
                [],
            )?;
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

    /// The writer, held by a test to stand for a store busy with another write.
    #[cfg(test)]
    pub(crate) fn hold_writer(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.write()
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

    /// Wait until a write that is about to take the writer, or has taken it, has
    /// finished.
    ///
    /// Test-only, for a test that holds another connection's write lock so that a
    /// commit already handed to the blocking pool fails: that lock has to be held
    /// until the commit has given up. A fixed hold cannot promise it, because
    /// `busy_timeout` totals the sleeps SQLite asks for, not the time they take,
    /// and on a loaded machine they take longer.
    #[cfg(test)]
    pub fn wait_out_the_writer_for_tests(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self.writer.try_lock().is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "no write took the writer"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(self.write());
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

    /// Make every Codex card write fail, on the same terms and for the same
    /// reason.
    ///
    /// What it buys is the arm neither a constraint refusal nor a deleted
    /// session can reach: an *ordinary* write failure in the middle of raising
    /// or retiring a card. The behaviour under test is whether that failure
    /// leaves half a card behind, and nothing the daemon does can produce one on
    /// demand.
    #[cfg(test)]
    pub fn break_codex_card_writes_for_tests(&self) {
        let conn = self.write();
        conn.execute_batch("DROP TABLE codex_pending_approvals")
            .expect("test fixture");
    }

    /// Make every Codex card query fail, **reversibly**.
    ///
    /// The dropped-table fixtures above stand in for a permanent failure; this
    /// one stands in for the transient class — `database is locked`, a
    /// contended write — which is the only class a *retry* can be a correct
    /// answer to, and therefore the only one that can prove a retry happens.
    /// A rename rather than a drop because SQLite rewrites the references in
    /// `all_pending_approvals` both ways, so the view is whole again afterwards.
    #[cfg(test)]
    pub fn hide_codex_cards_for_tests(&self, hidden: bool) {
        let conn = self.write();
        let (from, to) = if hidden {
            ("codex_pending_approvals", "codex_pending_approvals_hidden")
        } else {
            ("codex_pending_approvals_hidden", "codex_pending_approvals")
        };
        conn.execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
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

    /// What the log holds of the Codex turn `turn_id` of this run: whether its end is
    /// filed, and how many of its user messages are a mail wake
    /// ([`protocol::ipc::is_mail_nudge`]).
    pub fn codex_turn_messages(&self, session_uid: &str, turn_id: &str) -> Result<(bool, usize)> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT kind, payload FROM events
              WHERE session_uid = ?1 AND source = ?2 AND turn_id = ?3 AND kind IN (?4, ?5)",
        )?;
        let rows = stmt
            .query_map(
                params![
                    session_uid,
                    protocol::event::Source::Codex.as_str(),
                    turn_id,
                    protocol::event::EventKind::TurnComplete.as_str(),
                    protocol::event::EventKind::UserMessage.as_str()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let ended = rows
            .iter()
            .any(|(kind, _)| kind == protocol::event::EventKind::TurnComplete.as_str());
        let matching = rows
            .iter()
            .filter(|(kind, payload)| {
                kind == protocol::event::EventKind::UserMessage.as_str()
                    && serde_json::from_str::<serde_json::Value>(payload).is_ok_and(|payload| {
                        payload["text"]
                            .as_str()
                            .is_some_and(protocol::ipc::is_mail_nudge)
                    })
            })
            .count();
        Ok((ended, matching))
    }

    /// The seq and turn of the event `source` filed under `source_event_id` in
    /// this run, read on the dedup index.
    pub fn filed_event(
        &self,
        session_uid: &str,
        source: &Source,
        source_event_id: &str,
    ) -> Result<Option<(u64, Option<String>)>> {
        let conn = self.read();
        let filed: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT seq, turn_id FROM events
                  WHERE session_uid = ?1 AND source = ?2 AND source_event_id = ?3",
                params![session_uid, source.as_str(), source_event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(filed.map(|(seq, turn_id)| (seq as u64, turn_id)))
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
            |row| event_from_row(session_uid, row),
        )?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// The newest `limit` events below `before_seq`, oldest first — one earlier
    /// page. Read backwards on the `(session_uid, seq)` primary key.
    pub fn events_before(
        &self,
        session_uid: &str,
        before_seq: u64,
        limit: u32,
    ) -> Result<Vec<Event>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT seq, session_id, ts, kind, payload, source, source_event_id, turn_id, item_id
               FROM events
              WHERE session_uid = ?1 AND seq < ?2
              ORDER BY seq DESC
              LIMIT ?3",
        )?;
        // Clamped, because SQLite integers are signed and a mark past `i64::MAX`
        // is still past the tail rather than below the head.
        let before = i64::try_from(before_seq).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![session_uid, before, limit as i64], |row| {
            event_from_row(session_uid, row)
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        events.reverse();
        Ok(events)
    }

    /// A run's facts, rebuilt as the live path keeps them: every event that can decide one, oldest
    /// first, folded by [`protocol::facts::SessionFacts::observe`]. One read of the run, narrowed in
    /// SQL to those events under [`protocol::facts`] — a `session_start`, a user line, a Codex or an
    /// Antigravity run's settings ([`protocol::facts::CODEX_SETTINGS`], [`protocol::facts::ACP_SETTINGS`]), Claude's permission-mode line, a notification,
    /// a `session_end`, or any hook's — so the facts a restart reads back are the ones the live fold
    /// would hold, by the same rule.
    pub fn session_facts(&self, session_uid: &str) -> Result<protocol::facts::SessionFacts> {
        let conn = self.read();
        let mut stmt = conn.prepare_cached(
            "SELECT seq, session_id, ts, kind, payload, source, source_event_id, turn_id, item_id
               FROM events
              WHERE session_uid = ?1 AND (kind IN (?2, ?3, ?4, ?5, ?6, ?7, ?9) OR source = ?8)
              ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query(params![
            session_uid,
            EventKind::SessionStart.as_str(),
            EventKind::UserMessage.as_str(),
            protocol::facts::CODEX_SETTINGS,
            protocol::facts::TRANSCRIPT_PERMISSION_MODE,
            EventKind::Notification.as_str(),
            EventKind::SessionEnd.as_str(),
            Source::Hook.as_str(),
            protocol::facts::ACP_SETTINGS,
        ])?;
        let mut facts = protocol::facts::SessionFacts::default();
        while let Some(row) = rows.next()? {
            facts.observe(&event_from_row(session_uid, row)?);
        }
        Ok(facts)
    }

    pub fn get_session(&self, session_uid: &str) -> Result<Option<SessionRow>> {
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT session_uid, session_id, cwd,
                        claude_session_id, transcript_path, lifecycle, created_at, updated_at,
                        agent, codex_thread_id
                   FROM all_sessions WHERE session_uid = ?1",
                params![session_uid],
                session_row_from,
            )
            .optional()?;
        Ok(row)
    }

    /// Resolve whatever a caller named: a `session_uid` (exact) or a name.
    ///
    /// A name no longer identifies one run, so it has to be resolved by policy.
    /// The policy is exactly one rule — **the newest run under that name** —
    /// and `lifecycle` deliberately does not enter into it. `lifecycle` is not
    /// reliable enough to rank on: a session that ended while the daemon was
    /// down was never observed exiting, so it stays `Live` forever, and
    /// preferring "not exited" would rank that ghost above the run that
    /// genuinely happened last.
    ///
    /// `session_uid DESC` is the tie-break because a ULID sorts by mint time,
    /// so it is the same ordering as `created_at` without depending on two
    /// clocks agreeing.
    ///
    /// Ambiguity is resolved rather than reported on purpose: a name a later run
    /// carries again names the newest of them.
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
                "SELECT session_uid, session_id, cwd,
                        claude_session_id, transcript_path, lifecycle, created_at, updated_at,
                        agent, codex_thread_id
                   FROM all_sessions
                  WHERE session_id = ?1
                  ORDER BY created_at DESC, session_uid DESC
                  LIMIT 1",
                params![reference],
                session_row_from,
            )
            .optional()?;
        Ok(row)
    }

    /// The folders sessions have started in, most recent first, at most
    /// [`MAX_PROJECTS`].
    pub fn projects(&self) -> Result<Vec<ProjectRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT path, last_used, last_agent FROM projects
              WHERE path != ''
              ORDER BY last_used DESC, path
              LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![MAX_PROJECTS], |row| {
                Ok(ProjectRow {
                    path: row.get(0)?,
                    last_used: row.get(1)?,
                    last_agent: AgentKind::from_str_lossy(&row.get::<_, String>(2)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The run ccd started, for either agent, for the app's start request `request_id`,
    /// if any.
    pub fn hosted_run_for_request(&self, request_id: &str) -> Result<Option<HostedRun>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                "SELECT h.session_uid, s.cwd, s.lifecycle
                   FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE h.request_id = ?1",
                params![request_id],
                hosted_run_from,
            )
            .optional()?)
    }

    /// Write a new hosted run's row, in its agent's table, and its `run_hosts` row with
    /// its `start` line, when it has one, in one transaction, before anything for it is
    /// spawned (H1). A Claude run's host is sent its start again after a restart; a Codex
    /// start is never sent again, and its record is the folder answer it started with, which
    /// its reconnect resumes with. If `request_id` already has a run, of
    /// either agent, nothing is written and that run is returned, so a repeated start never
    /// makes a second one.
    pub fn begin_run(
        &self,
        row: &SessionRow,
        request_id: &str,
        start: Option<&str>,
        account: Option<&str>,
    ) -> Result<Option<HostedRun>> {
        let (table, _) = session_tables_for(&row.agent);
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT h.session_uid, s.cwd, s.lifecycle
                   FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE h.request_id = ?1",
                params![request_id],
                hosted_run_from,
            )
            .optional()?;
        if existing.is_some() {
            return Ok(existing);
        }
        tx.execute(
            // The table name is a compile-time literal from `session_tables_for`.
            &format!(
                "INSERT INTO {table}(session_uid, session_id, cwd,
                                     claude_session_id, transcript_path, lifecycle,
                                     created_at, updated_at, agent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            ),
            params![
                row.session_uid,
                row.session_id,
                row.cwd,
                row.claude_session_id,
                row.transcript_path,
                lifecycle_str(row.lifecycle),
                row.created_at,
                row.updated_at,
                row.agent.as_str(),
            ],
        )?;
        tx.execute(
            "INSERT INTO run_hosts(session_uid, request_id, start, account_id) VALUES (?1, ?2, ?3, ?4)",
            params![row.session_uid, request_id, start, account],
        )?;
        tx.execute(
            &format!("{RECORD_PROJECTS} VALUES (?1, ?2, ?3) {RECORD_PROJECTS_NEWEST}"),
            params![row.cwd, row.created_at, row.agent.as_str()],
        )?;
        tx.commit()?;
        Ok(None)
    }

    /// Record the agent host's identity, as ccd read it from the kernel, and the boot it
    /// runs in, which a signal to its group is warranted against.
    pub fn record_host(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        boot: Option<BootIdentity>,
    ) -> Result<()> {
        let conn = self.write();
        let changed = conn.execute(
            "UPDATE run_hosts SET host_pid = ?2, host_start_sec = ?3, host_start_usec = ?4,
                                  boot_sec = ?5, boot_usec = ?6
              WHERE session_uid = ?1",
            params![
                session_uid,
                host.pid,
                host.birth.start_sec,
                host.birth.start_usec,
                boot.map(|boot| boot.boot_sec),
                boot.map(|boot| boot.boot_usec)
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "{session_uid} has no hosted run to record a host for"
        );
        Ok(())
    }

    /// Advance the run's cursor to `seq`: everything up to it from the host
    /// `host` is committed. `false` when nothing changed, because the run's
    /// recorded host is another one or the cursor is already there, and then
    /// the caller must not acknowledge `seq` (H3).
    pub fn commit_cursor(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        seq: u64,
    ) -> Result<bool> {
        let seq = i64::try_from(seq).context("a host sequence number past i64")?;
        let conn = self.write();
        let changed = conn.execute(
            "UPDATE run_hosts SET cursor = ?5
              WHERE session_uid = ?1 AND host_pid = ?2
                AND host_start_sec = ?3 AND host_start_usec = ?4 AND cursor < ?5",
            params![
                session_uid,
                host.pid,
                host.birth.start_sec,
                host.birth.start_usec,
                seq
            ],
        )?;
        Ok(changed == 1)
    }

    /// Commit the start's outcome, message `seq` from the host `host`, in one
    /// transaction: the cursor, as [`Store::commit_cursor`] advances it, with the
    /// SDK's initialize answer (`initialize`, as JSON) on a done start, whose
    /// commands are then claude's list as last reported; then the
    /// run's lifecycle, `starting` to `lifecycle` (`Live` or `Failed`; `Exited` for a
    /// relaunch claude refused), only while it is `starting`, so a run already ended keeps
    /// its end; and, when that made it `live`, `events` (the person's first words). A
    /// relaunch's start is the person's message `send`: its claim is settled `sent` with
    /// the run live, or released when the relaunch was refused. `None`, with nothing
    /// written, when the cursor does not move, and then the caller must not acknowledge
    /// `seq` (H3); otherwise the events newly stored.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_outcome(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        seq: u64,
        lifecycle: Lifecycle,
        initialize: Option<&str>,
        events: &[PendingEvent],
        send: Option<&str>,
    ) -> Result<Option<Vec<Event>>> {
        let seq = i64::try_from(seq).context("a host sequence number past i64")?;
        let now = protocol::time::now_rfc3339();
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE run_hosts SET cursor = ?5, initialize = ?6,
                    commands = json_extract(?6, '$.commands'),
                    commands_at = CASE WHEN ?6 IS NULL THEN NULL ELSE ?7 END
              WHERE session_uid = ?1 AND host_pid = ?2
                AND host_start_sec = ?3 AND host_start_usec = ?4 AND cursor < ?5",
            params![
                session_uid,
                host.pid,
                host.birth.start_sec,
                host.birth.start_usec,
                seq,
                initialize,
                now
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let mut moved = 0;
        for table in SESSION_TABLES {
            moved += tx.execute(
                // A compile-time list in this file; nothing a caller supplies
                // reaches the statement text.
                &format!(
                    "UPDATE {table} SET lifecycle = ?2, updated_at = ?3
                      WHERE session_uid = ?1 AND lifecycle = ?4"
                ),
                params![
                    session_uid,
                    lifecycle_str(lifecycle),
                    now,
                    lifecycle_str(Lifecycle::Starting)
                ],
            )?;
        }
        let mut filed = Vec::new();
        if moved == 1 && lifecycle == Lifecycle::Live {
            for event in events {
                filed.extend(append_in_tx(&tx, event)?);
            }
        }
        if let (1, Some(request_id)) = (moved, send) {
            let settle = if lifecycle == Lifecycle::Live {
                "UPDATE text_mutations SET status = 'sent', settled_at = ?3
                  WHERE session_uid = ?1 AND request_id = ?2 AND status = 'applying'"
            } else {
                "DELETE FROM text_mutations
                  WHERE session_uid = ?1 AND request_id = ?2 AND status = 'applying' AND ?3 IS NOT NULL"
            };
            tx.execute(settle, params![session_uid, request_id, now])?;
        }
        tx.commit()?;
        Ok(Some(filed))
    }

    /// Commit the host's outcome for a message ccd sent it, message `seq` from the host
    /// `host`, in one transaction: the cursor, as [`Store::commit_cursor`] advances it;
    /// then the send `request_id`'s claim, settled `sent` with `row` (the person's words)
    /// filed when the host took the message, or released when it refused it (`row`
    /// `None`), either only while it is still claimed. `None`, with nothing written,
    /// when the cursor does not move, and then the caller must not acknowledge `seq`
    /// (H3); otherwise the events newly stored.
    pub fn commit_host_send(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        seq: u64,
        request_id: &str,
        row: Option<&PendingEvent>,
    ) -> Result<Option<Vec<Event>>> {
        let seq = i64::try_from(seq).context("a host sequence number past i64")?;
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let moved = tx.execute(
            "UPDATE run_hosts SET cursor = ?5
              WHERE session_uid = ?1 AND host_pid = ?2
                AND host_start_sec = ?3 AND host_start_usec = ?4 AND cursor < ?5",
            params![
                session_uid,
                host.pid,
                host.birth.start_sec,
                host.birth.start_usec,
                seq
            ],
        )?;
        if moved != 1 {
            return Ok(None);
        }
        let mut filed = Vec::new();
        match row {
            Some(row) => {
                let settled = tx.execute(
                    "UPDATE text_mutations SET status = 'sent', settled_at = ?3
                      WHERE session_uid = ?1 AND request_id = ?2 AND status = 'applying'",
                    params![session_uid, request_id, protocol::time::now_rfc3339()],
                )?;
                if settled == 1 {
                    filed.extend(append_in_tx(&tx, row)?);
                }
            }
            None => {
                tx.execute(
                    "DELETE FROM text_mutations
                      WHERE session_uid = ?1 AND request_id = ?2 AND status = 'applying'",
                    params![session_uid, request_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(Some(filed))
    }

    /// A host's line about one of its run's cards, message `seq` from the host `host`,
    /// in one transaction: the cursor, as [`Store::commit_cursor`] advances it; then
    /// `events`; then the card's `pending_approvals` row written (`card`, a request
    /// claude asked) or removed (`None`, a request the host withdrew). `None`, with
    /// nothing written, when the cursor does not move, and then the caller must not
    /// acknowledge `seq` (H3); otherwise the events newly stored.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_host_card(
        &self,
        session_uid: &str,
        session_id: &str,
        host: &ProcessIdentity,
        seq: u64,
        request_id: &str,
        card: Option<(&str, i64)>,
        events: &[PendingEvent],
    ) -> Result<Option<Vec<Event>>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !advance_host_cursor(&tx, session_uid, host, seq)? {
            return Ok(None);
        }
        let mut filed = Vec::new();
        for event in events {
            filed.extend(append_in_tx(&tx, event)?);
        }
        match card {
            Some((card, created_ms)) => {
                tx.execute(
                    "INSERT INTO pending_approvals(session_uid, session_id, request_id, card,
                                                   generation, created_ms)
                     SELECT ?1, ?2, ?3, ?4, 0, ?5
                      WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)
                     ON CONFLICT(session_uid, request_id) DO UPDATE SET card = excluded.card",
                    params![session_uid, session_id, request_id, card, created_ms],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM pending_approvals WHERE session_uid = ?1 AND request_id = ?2",
                    params![session_uid, request_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(Some(filed))
    }

    /// The host's outcome for an answer ccd sent it, message `seq` from the host
    /// `host`, in one transaction: the cursor; then, when the answer reached claude
    /// (`outcome`, with the card's `payload_hash`), the answer recorded unless one
    /// already is, the card's row removed and `events` (its resolution) filed; and in
    /// either case the answer's claim removed, so a refused answer leaves the card
    /// answerable again. `None`, with nothing written, when the cursor does not move
    /// (H3); otherwise the events newly stored.
    pub fn commit_host_answer(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        seq: u64,
        request_id: &str,
        outcome: Option<(&str, &AnswerOutcome)>,
        events: &[PendingEvent],
    ) -> Result<Option<Vec<Event>>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !advance_host_cursor(&tx, session_uid, host, seq)? {
            return Ok(None);
        }
        let mut filed = Vec::new();
        if let Some((payload_hash, outcome)) = outcome {
            tx.execute(
                "INSERT INTO answers(session_uid, request_id, payload_hash, outcome, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5
                  WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)
                 ON CONFLICT DO NOTHING",
                params![
                    session_uid,
                    request_id,
                    payload_hash,
                    serde_json::to_string(outcome)?,
                    protocol::time::now_rfc3339(),
                ],
            )?;
            tx.execute(
                "DELETE FROM pending_approvals WHERE session_uid = ?1 AND request_id = ?2",
                params![session_uid, request_id],
            )?;
            for event in events {
                filed.extend(append_in_tx(&tx, event)?);
            }
        }
        tx.execute(
            "DELETE FROM answer_claims WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id],
        )?;
        tx.commit()?;
        Ok(Some(filed))
    }

    /// The run `session_uid`'s answers claimed and not yet settled.
    pub fn answer_claims_of(&self, session_uid: &str) -> Result<Vec<AnswerClaim>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, session_id, request_id, payload_hash, decision, started_at
               FROM answer_claims WHERE session_uid = ?1",
        )?;
        let rows = stmt
            .query_map(params![session_uid], answer_claim_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// File one SDK message from the host `host`, message `seq`, in one transaction:
    /// the cursor, as [`Store::commit_cursor`] advances it, then the message's
    /// `events`, then what it tells the run's row (`notes`). `None`, with nothing
    /// written, when the cursor does not move, and then the caller must not acknowledge
    /// `seq` (H3); otherwise the events newly stored, which a replay leaves empty.
    pub fn append_host_message(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        seq: u64,
        events: &[PendingEvent],
        notes: &RunNotes,
    ) -> Result<Option<Vec<Event>>> {
        let seq = i64::try_from(seq).context("a host sequence number past i64")?;
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let moved = tx.execute(
            "UPDATE run_hosts SET cursor = ?5
              WHERE session_uid = ?1 AND host_pid = ?2
                AND host_start_sec = ?3 AND host_start_usec = ?4 AND cursor < ?5",
            params![
                session_uid,
                host.pid,
                host.birth.start_sec,
                host.birth.start_usec,
                seq
            ],
        )?;
        if moved != 1 {
            return Ok(None);
        }
        let mut filed = Vec::with_capacity(events.len());
        for pending in events {
            if let Some(event) = append_in_tx(&tx, pending)? {
                filed.push(event);
            }
        }
        // A hosted run, Claude's or Antigravity's, has its row in `sessions`.
        let (table, _) = session_tables_for(&AgentKind::Claude);
        if let Some(id) = &notes.session_id {
            tx.execute(
                &format!(
                    "UPDATE {table} SET claude_session_id = ?2
                      WHERE session_uid = ?1 AND claude_session_id IS NOT ?2"
                ),
                params![session_uid, id],
            )?;
        }
        if let Some(cwd) = &notes.cwd {
            tx.execute(
                &format!("UPDATE {table} SET cwd = ?2 WHERE session_uid = ?1 AND cwd != ?2"),
                params![session_uid, cwd],
            )?;
        }
        if let Some(names) = &notes.screen_only {
            tx.execute(
                "UPDATE run_hosts SET screen_only = ?2 WHERE session_uid = ?1",
                params![session_uid, serde_json::to_string(names)?],
            )?;
        }
        if let Some(commands) = &notes.commands {
            tx.execute(
                "UPDATE run_hosts SET commands = ?2, commands_at = ?3 WHERE session_uid = ?1",
                params![
                    session_uid,
                    serde_json::to_string(commands)?,
                    protocol::time::now_rfc3339()
                ],
            )?;
        }
        tx.commit()?;
        Ok(Some(filed))
    }

    /// The newest `codex_settings` fact of the run `session_uid` as it was filed: the settings
    /// Codex last named for its thread ([`protocol::facts::CODEX_SETTINGS`]), `None` before any.
    pub fn newest_codex_settings(&self, session_uid: &str) -> Result<Option<serde_json::Value>> {
        let conn = self.read();
        newest_decided(
            &conn,
            session_uid,
            "kind = ?2",
            &[protocol::facts::CODEX_SETTINGS],
            |event| Some(event.payload.clone()),
        )
    }

    /// Keep `models` (`{"models": [...]}`) as the Codex run `session_uid`'s models, where a
    /// Claude run's start's answer keeps its own ([`Store::hosted_models`] reads either).
    pub fn keep_hosted_models(&self, session_uid: &str, models: &serde_json::Value) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE run_hosts SET initialize = ?2 WHERE session_uid = ?1",
            params![session_uid, serde_json::to_string(models)?],
        )?;
        Ok(())
    }

    /// The models of the hosted run `session_uid`: a Claude run's start's answer's, each as
    /// claude gives it, or a Codex run's start check's, each as Codex's `model/list` gives
    /// it, and the model its thread started on first if the check did not list it; `None`
    /// when the run has no `run_hosts` row or none are kept.
    pub fn hosted_models(&self, session_uid: &str) -> Result<Option<Vec<serde_json::Value>>> {
        let conn = self.read();
        let models = conn
            .query_row(
                "SELECT json_extract(initialize, '$.models') FROM run_hosts WHERE session_uid = ?1",
                [session_uid],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        models
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .context("a hosted run's models")
    }

    /// The modes of the hosted Antigravity run `session_uid`, as its start's outcome gave them (`{value, label}`);
    /// empty for any other run and when none are kept.
    pub fn hosted_modes(&self, session_uid: &str) -> Result<Vec<serde_json::Value>> {
        let conn = self.read();
        let modes = conn
            .query_row(
                "SELECT json_extract(initialize, '$.modes') FROM run_hosts WHERE session_uid = ?1",
                [session_uid],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(modes
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .context("a hosted run's modes")?
            .unwrap_or_default())
    }

    /// Whether the run `session_uid` is in a turn: its newest `session_start` has no
    /// `turn_complete` after it.
    pub fn turn_running(&self, session_uid: &str) -> Result<bool> {
        let conn = self.read();
        let newest = conn
            .query_row(
                "SELECT kind FROM events WHERE session_uid = ?1 AND kind IN (?2, ?3)
                  ORDER BY seq DESC LIMIT 1",
                params![
                    session_uid,
                    EventKind::SessionStart.as_str(),
                    EventKind::TurnComplete.as_str()
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(newest.as_deref() == Some(EventKind::SessionStart.as_str()))
    }

    /// **Whether the run `session_uid` is certainly quiet, by what ccd has filed of it** ([`crate::switch::moves`]):
    /// a turn of its process has completed and none has begun since; no message of the person's sent it waits to be
    /// taken or answered (a claim still `applying`, or one filed after its last turn began, which may run as a turn
    /// of its own); its last turn ran no command, subagent or task (Codex: no command, and no subagent in any turn), and its process
    /// scheduled no prompt; no background task it started (a command, a subagent, [`protocol::sdk_map::BACKGROUND_TASK`])
    /// is still open; and no mail wake is outstanding. `sending`, the message being sent
    /// now, is no message waiting. Anything not known reads not quiet.
    pub fn certainly_quiet(&self, session_uid: &str, sending: &str) -> Result<bool> {
        let conn = self.read();
        let agent = conn
            .query_row(
                "SELECT agent FROM all_sessions WHERE session_uid = ?1",
                params![session_uid],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let codex = agent.as_deref() == Some(AgentKind::Codex.as_str());
        let waiting: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM text_mutations
                            WHERE session_uid = ?1 AND status = 'applying' AND request_id IS NOT ?3)
                 OR EXISTS(SELECT 1 FROM mutation_ledger
                            WHERE session_uid = ?1 AND operation_kind = ?2 AND status = 'applying'
                              AND client_request_id IS NOT ?3)
                 OR EXISTS(SELECT 1 FROM mail_wake WHERE session_uid = ?1 AND outstanding = 1)",
            params![session_uid, OPERATION_COMPOSE, sending],
            |row| row.get(0),
        )?;
        if waiting {
            return Ok(false);
        }
        if agent.as_deref() == Some(AgentKind::Antigravity.as_str()) {
            return acp_quiet(&conn, session_uid);
        }
        // What its process has filed: everything since the run's newest end, whose process this one followed.
        let mut stmt = conn.prepare(
            "SELECT kind, source, payload FROM events
              WHERE session_uid = ?1
                AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE session_uid = ?1 AND kind = ?2), 0)
              ORDER BY seq",
        )?;
        let events = stmt
            .query_map(
                params![session_uid, EventKind::SessionEnd.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let kind_at = |i: usize| events[i].0.as_str();
        let Some(ended) = (0..events.len())
            .rev()
            .find(|&i| kind_at(i) == EventKind::TurnComplete.as_str())
        else {
            return Ok(false);
        };
        // Where its last turn began: a Claude turn's init; a Codex turn's first user message after the turn before,
        // whose tools are read from that turn's end on.
        let before = (0..ended)
            .rev()
            .find(|&i| kind_at(i) == EventKind::TurnComplete.as_str())
            .map_or(0, |i| i + 1);
        let began = if codex {
            (before..ended).find(|&i| kind_at(i) == EventKind::UserMessage.as_str())
        } else {
            (0..ended)
                .rev()
                .find(|&i| kind_at(i) == EventKind::SessionStart.as_str())
        };
        let Some(began) = began else {
            return Ok(false);
        };
        let turn_start = if codex {
            EventKind::UserMessage.as_str()
        } else {
            EventKind::SessionStart.as_str()
        };
        let blocks = |payload: &str| -> Vec<serde_json::Value> {
            serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|payload| payload["message"]["content"].as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(|block| block["type"] == "tool_use")
                .collect()
        };
        // A background task claude runs, from its start until its end, whichever turn began it.
        let mut open = std::collections::HashSet::new();
        for (kind, _, payload) in &events {
            if kind != protocol::sdk_map::BACKGROUND_TASK {
                continue;
            }
            let task = serde_json::from_str::<serde_json::Value>(payload).unwrap_or_default();
            let Some(id) = task["task_id"].as_str() else {
                continue;
            };
            if task["ended"] == true {
                open.remove(id);
            } else {
                open.insert(id.to_string());
            }
        }
        if !open.is_empty() {
            return Ok(false);
        }
        for (i, (kind, source, payload)) in events.iter().enumerate() {
            let in_last_turn = i > began;
            let quiet = match kind.as_str() {
                // A turn begun after the last one ended.
                kind if i > ended && kind == turn_start => false,
                // The person's message after the last turn began, which may run as a turn of its own.
                kind if in_last_turn
                    && kind == EventKind::UserMessage.as_str()
                    && (codex || source == Source::Daemon.as_str()) =>
                {
                    false
                }
                // A Codex command in its last turn; a file change is no command.
                kind if codex
                    && i >= before
                    && (kind == EventKind::ToolCall.as_str()
                        || kind == EventKind::ToolResult.as_str()) =>
                {
                    serde_json::from_str::<serde_json::Value>(payload)
                        .is_ok_and(|payload| payload["tool"] == "file_change")
                }
                // A Codex subagent it spawned in any turn of this app-server, which may work on after that turn: Codex
                // files no end of a subagent that ccd reads, so the run keeps its account until the app-server ends.
                kind if codex && kind == "codex_collabAgentToolCall" => false,
                // A prompt it scheduled may still come, whichever turn scheduled it.
                kind if kind == EventKind::AgentMessage.as_str() => {
                    blocks(payload).iter().all(|block| {
                        !(block["name"] == "CronCreate"
                            || in_last_turn
                                && ["Bash", "Task", "Agent"]
                                    .iter()
                                    .any(|name| block["name"] == *name))
                    })
                }
                _ => true,
            };
            if !quiet {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the Codex run `session_uid` is in a turn: its newest user message, the
    /// person's or a mail wake's, has no `turn_complete` after it.
    pub fn codex_turn_running(&self, session_uid: &str) -> Result<bool> {
        let conn = self.read();
        let newest = conn
            .query_row(
                "SELECT kind FROM events WHERE session_uid = ?1 AND kind IN (?2, ?3)
                  ORDER BY seq DESC LIMIT 1",
                params![
                    session_uid,
                    EventKind::UserMessage.as_str(),
                    EventKind::TurnComplete.as_str()
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(newest.as_deref() == Some(EventKind::UserMessage.as_str()))
    }

    /// What the hosted run `session_uid`'s command menu is read from; `None` when the
    /// run has no `run_hosts` row.
    pub fn hosted_commands(&self, session_uid: &str) -> Result<Option<HostedCommands>> {
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT commands, commands_at, screen_only,
                        json_extract(initialize, '$.claude_code_version')
                   FROM run_hosts WHERE session_uid = ?1",
                [session_uid],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((commands, commands_at, screen_only, claude_version)) = row else {
            return Ok(None);
        };
        Ok(Some(HostedCommands {
            commands: commands
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .context("a hosted run's command list")?,
            commands_at,
            screen_only: screen_only
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .context("a hosted run's screen-only commands")?,
            claude_version,
        }))
    }

    /// End a hosted run's launch whose host is `host` in one transaction: only while `host`
    /// is still the host recorded for the run, so a caller that inspected another launch (an
    /// end found for an app-server a message has since replaced) writes nothing; with
    /// `cursor`, the run's cursor, as [`Store::commit_cursor`] advances it; then the row from
    /// `from` to `to`, only while it is still `from`; then, if the row changed, the run's `end`
    /// event. `None`, with nothing written, when `host` is not the recorded host or the cursor
    /// does not move, and then the caller must not acknowledge the message (H3). Otherwise
    /// the events newly stored: none when the row was no longer `from`.
    pub fn end_hosted_run(
        &self,
        session_uid: &str,
        host: &ProcessIdentity,
        cursor: Option<u64>,
        from: Lifecycle,
        to: Lifecycle,
        end: &PendingEvent,
    ) -> Result<Option<Vec<Event>>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !recorded_host_is(&tx, session_uid, host)? {
            return Ok(None);
        }
        if let Some(seq) = cursor {
            let seq = i64::try_from(seq).context("a host sequence number past i64")?;
            let moved = tx.execute(
                "UPDATE run_hosts SET cursor = ?2 WHERE session_uid = ?1 AND cursor < ?2",
                params![session_uid, seq],
            )?;
            if moved != 1 {
                return Ok(None);
            }
        }
        // The row is in its agent's table, the one of the two that holds it.
        let now = protocol::time::now_rfc3339();
        let mut ended = 0;
        for table in SESSION_TABLES {
            ended += tx.execute(
                &format!(
                    "UPDATE {table} SET lifecycle = ?3, updated_at = ?4
                      WHERE session_uid = ?1 AND lifecycle = ?2"
                ),
                params![session_uid, lifecycle_str(from), lifecycle_str(to), now],
            )?;
        }
        let mut filed = Vec::new();
        if ended == 1 {
            filed.extend(append_in_tx(&tx, end)?);
        }
        tx.commit()?;
        Ok(Some(filed))
    }

    /// The recorded host of every hosted run that has not ended (`starting`, `live` or
    /// `stopping`).
    pub fn watched_hosts(&self) -> Result<Vec<WatchedHost>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT h.session_uid, s.lifecycle, h.host_pid, h.host_start_sec, h.host_start_usec,
                    h.boot_sec, h.boot_usec
               FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
              WHERE h.host_pid IS NOT NULL AND s.lifecycle IN (?1, ?2, ?3)",
        )?;
        let rows = stmt
            .query_map(
                params![
                    lifecycle_str(Lifecycle::Starting),
                    lifecycle_str(Lifecycle::Live),
                    lifecycle_str(Lifecycle::Stopping)
                ],
                watched_host_from,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The recorded host of the hosted run `session_uid`, whatever its lifecycle; `None`
    /// before its host is recorded.
    pub fn watched_host(&self, session_uid: &str) -> Result<Option<WatchedHost>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                "SELECT h.session_uid, s.lifecycle, h.host_pid, h.host_start_sec,
                        h.host_start_usec, h.boot_sec, h.boot_usec
                   FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE h.session_uid = ?1 AND h.host_pid IS NOT NULL",
                params![session_uid],
                watched_host_from,
            )
            .optional()?)
    }

    /// The recorded host of every hosted run of `agent`, whatever its lifecycle: a Claude
    /// run's agent host, which a restarted ccd attaches again while it is alive, or a Codex
    /// run's app-server leader.
    pub fn recorded_hosts(&self, agent: &AgentKind) -> Result<Vec<WatchedHost>> {
        let (table, _) = session_tables_for(agent);
        let conn = self.read();
        let mut stmt = conn.prepare(&format!(
            "SELECT h.session_uid, s.lifecycle, h.host_pid, h.host_start_sec, h.host_start_usec,
                    h.boot_sec, h.boot_usec
               FROM run_hosts AS h JOIN {table} AS s USING(session_uid)
              WHERE h.host_pid IS NOT NULL"
        ))?;
        let rows = stmt
            .query_map([], watched_host_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every `starting` hosted Claude run with no recorded host becomes `failed`: the ccd
    /// that began it died before it recorded a host, so no `start` was ever sent; one its switch was relaunching
    /// goes back to `exited`, still switching, for its next message. Only before this ccd starts any run of its own.
    /// The number of runs failed or put back.
    pub fn fail_unhosted_starts(&self) -> Result<usize> {
        let (table, _) = session_tables_for(&AgentKind::Claude);
        Ok(self.write().execute(
            &format!(
                "UPDATE {table} SET updated_at = ?2,
                        lifecycle = CASE WHEN session_uid IN
                                         (SELECT session_uid FROM run_hosts WHERE switching IS NOT NULL)
                                    THEN ?4 ELSE ?1 END
                  WHERE lifecycle = ?3 AND session_uid IN
                        (SELECT session_uid FROM run_hosts WHERE host_pid IS NULL)"
            ),
            params![
                lifecycle_str(Lifecycle::Failed),
                protocol::time::now_rfc3339(),
                lifecycle_str(Lifecycle::Starting),
                lifecycle_str(Lifecycle::Exited)
            ],
        )?)
    }

    /// A restarted ccd's moves (306): a run its switch was ending stays so, its process stopped again once found
    /// alive ([`crate::claude_host::attach_run`], [`crate::state::Daemon::recover_codex_runs`]), so the end it
    /// reports is the switch's. The message that was moving a run that has not started again since, or retrying a
    /// failed move, never reached a process, and is released, so its retry relaunches it. Only before this ccd starts
    /// any run of its own, once [`Store::fail_unhosted_starts`] has put a relaunch that never got a host back to
    /// `exited`.
    pub fn recover_moves(&self) -> Result<()> {
        self.write().execute(
            "DELETE FROM text_mutations WHERE status = 'applying' AND session_uid IN
                    (SELECT h.session_uid FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                      WHERE (h.switching = ?1 AND s.lifecycle IN (?3, ?4))
                         OR (h.switching = ?2 AND s.lifecycle = ?4))",
            params![
                Store::SWITCH_MOVED,
                Store::SWITCH_FAILED,
                lifecycle_str(Lifecycle::Live),
                lifecycle_str(Lifecycle::Exited)
            ],
        )?;
        Ok(())
    }

    /// Every `starting` Codex run ccd hosts becomes `failed`: a restarted ccd never goes on
    /// with a start that a ccd before it began; one its switch was relaunching goes back to `exited`, still
    /// switching, for its next message. The number of runs failed or put back, and the recorded app-server of each
    /// that has one, for the ladder: nothing ccd did not record is signalled. Only before this ccd starts any run of
    /// its own.
    pub fn fail_codex_starts(&self) -> Result<(usize, Vec<WatchedHost>)> {
        let (table, _) = session_tables_for(&AgentKind::Codex);
        let mut conn = self.write();
        let tx = conn.transaction()?;
        let hosts = tx
            .prepare(&format!(
                "SELECT h.session_uid, s.lifecycle, h.host_pid, h.host_start_sec, h.host_start_usec,
                        h.boot_sec, h.boot_usec
                   FROM run_hosts AS h JOIN {table} AS s USING(session_uid)
                  WHERE s.lifecycle = ?1 AND h.host_pid IS NOT NULL"
            ))?
            .query_map(
                params![lifecycle_str(Lifecycle::Starting)],
                watched_host_from,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let failed = tx.execute(
            &format!(
                "UPDATE {table} SET updated_at = ?2,
                        lifecycle = CASE WHEN session_uid IN
                                         (SELECT session_uid FROM run_hosts WHERE switching IS NOT NULL)
                                    THEN ?4 ELSE ?1 END
                  WHERE lifecycle = ?3 AND session_uid IN (SELECT session_uid FROM run_hosts)"
            ),
            params![
                lifecycle_str(Lifecycle::Failed),
                protocol::time::now_rfc3339(),
                lifecycle_str(Lifecycle::Starting),
                lifecycle_str(Lifecycle::Exited)
            ],
        )?;
        tx.commit()?;
        Ok((failed, hosts))
    }

    /// Every `live` Codex run ccd hosts with a recorded app-server and a thread: what a
    /// restarted ccd connects to again while its app-server is alive.
    pub fn codex_live_runs(&self) -> Result<Vec<LiveCodexRun>> {
        let (table, _) = session_tables_for(&AgentKind::Codex);
        let conn = self.read();
        let mut stmt = conn.prepare(&format!(
            "SELECT h.session_uid, s.lifecycle, h.host_pid, h.host_start_sec, h.host_start_usec,
                    h.boot_sec, h.boot_usec, s.cwd, s.codex_thread_id, h.start
               FROM run_hosts AS h JOIN {table} AS s USING(session_uid)
              WHERE s.lifecycle = ?1 AND h.host_pid IS NOT NULL
                AND s.codex_thread_id IS NOT NULL"
        ))?;
        let rows = stmt
            .query_map(params![lifecycle_str(Lifecycle::Live)], live_codex_run_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Whether the hosted run `session_uid`, of either agent, is one the person's next message
    /// relaunches ([`relaunchable_in`] for Codex, [`claude_relaunchable_in`] for Claude; a uid is
    /// in one table only): what the fleet reports `relaunchable` for, and a Codex run's
    /// `bound_not_started`.
    pub fn relaunchable(&self, session_uid: &str) -> Result<bool> {
        let conn = self.read();
        Ok(relaunchable_in(&conn, session_uid)?.is_some()
            || claude_relaunchable_in(&conn, session_uid)?.is_some())
    }

    /// The hosted Claude run `session_uid` as a message relaunches it, when it is one to
    /// relaunch ([`claude_relaunchable_in`]); `None` otherwise.
    pub fn claude_relaunch(&self, session_uid: &str) -> Result<Option<ClaudeRelaunch>> {
        claude_relaunchable_in(&self.read(), session_uid)
    }

    /// Begin a message's relaunch of the hosted Claude run `session_uid` (decision 210), in one
    /// transaction: only while it is one to relaunch ([`claude_relaunchable_in`]), the run moves
    /// `exited` → `starting`; its recorded host and boot are cleared, as a first start's row
    /// stands before its spawn, so the watch never reads the old host as this launch's; its
    /// cursor goes back to 0, since a new host numbers its messages from 1; and its start line
    /// becomes `start`, which a restarted ccd sends again, with `account`, the account it is launched on (`None` its
    /// Default), which a restarted ccd records once it goes live ([`HostedLink::launched`]). Whether it moved: `false`,
    /// with nothing written, when it is no longer one to relaunch (a second message met it `starting`).
    pub fn begin_claude_relaunch(
        &self,
        session_uid: &str,
        start: &str,
        account: Option<&str>,
    ) -> Result<bool> {
        let (table, _) = session_tables_for(&AgentKind::Claude);
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if claude_relaunchable_in(&tx, session_uid)?.is_none() {
            return Ok(false);
        }
        tx.execute(
            &format!(
                "UPDATE {table} SET lifecycle = ?2, updated_at = ?3
                  WHERE session_uid = ?1 AND lifecycle = ?4"
            ),
            params![
                session_uid,
                lifecycle_str(Lifecycle::Starting),
                protocol::time::now_rfc3339(),
                lifecycle_str(Lifecycle::Exited)
            ],
        )?;
        tx.execute(
            "UPDATE run_hosts SET host_pid = NULL, host_start_sec = NULL, host_start_usec = NULL,
                                  boot_sec = NULL, boot_usec = NULL, cursor = 0, start = ?2,
                                  launch_account = ?3
              WHERE session_uid = ?1",
            params![session_uid, start, serde_json::json!(account).to_string()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The messages to the run `session_uid` claimed before `before` (an earlier ccd's) and still `applying` once its
    /// host has replayed what it holds: none of them reached it, so each is released, and a retry is a fresh attempt.
    pub fn release_unsent(&self, session_uid: &str, before: &str) -> Result<()> {
        self.write().execute(
            "DELETE FROM text_mutations
              WHERE session_uid = ?1 AND status = 'applying' AND started_at < ?2",
            params![session_uid, before],
        )?;
        Ok(())
    }

    /// Begin a message's relaunch of the hosted Codex run `session_uid` (P3 R4), in one
    /// transaction: only while it is one to relaunch ([`relaunchable_in`]), the run moves
    /// `exited` → `starting` and its recorded app-server is cleared, as a first start's row stands
    /// before its launch, so a second message meets `starting` and the watch never reads the old
    /// app-server as the relaunch's. The run as it was, its old app-server for the warrant; `None`,
    /// with nothing written, when it is not one to relaunch.
    pub fn begin_relaunch(&self, session_uid: &str) -> Result<Option<LiveCodexRun>> {
        let (table, _) = session_tables_for(&AgentKind::Codex);
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(run) = relaunchable_in(&tx, session_uid)? else {
            return Ok(None);
        };
        tx.execute(
            &format!(
                "UPDATE {table} SET lifecycle = ?2, updated_at = ?3
                  WHERE session_uid = ?1 AND lifecycle = ?4"
            ),
            params![
                session_uid,
                lifecycle_str(Lifecycle::Starting),
                protocol::time::now_rfc3339(),
                lifecycle_str(Lifecycle::Exited)
            ],
        )?;
        tx.execute(
            "UPDATE run_hosts SET host_pid = NULL, host_start_sec = NULL, host_start_usec = NULL,
                                  boot_sec = NULL, boot_usec = NULL
              WHERE session_uid = ?1",
            params![session_uid],
        )?;
        tx.commit()?;
        Ok(Some(run))
    }

    /// A hosted run's start whose host was found gone, failed in one transaction: `starting` →
    /// `failed`, or `exited` for a relaunch by its switch, which its next message relaunches again; only while it is
    /// still `starting` and `host`, the host the caller found gone, is still the one recorded for it. The message a
    /// relaunch started with, which the gone host may or may not have given claude, is settled indeterminate, so its
    /// retry is told so and no later move waits for it. Whether it moved.
    pub fn fail_hosted_start(&self, session_uid: &str, host: &ProcessIdentity) -> Result<bool> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !recorded_host_is(&tx, session_uid, host)? {
            return Ok(false);
        }
        let now = protocol::time::now_rfc3339();
        let mut moved = 0;
        for table in SESSION_TABLES {
            moved += tx.execute(
                &format!(
                    "UPDATE {table} SET updated_at = ?3,
                            lifecycle = CASE WHEN session_uid IN
                                             (SELECT session_uid FROM run_hosts WHERE switching IS NOT NULL)
                                        THEN ?5 ELSE ?2 END
                      WHERE session_uid = ?1 AND lifecycle = ?4"
                ),
                params![
                    session_uid,
                    lifecycle_str(Lifecycle::Failed),
                    now,
                    lifecycle_str(Lifecycle::Starting),
                    lifecycle_str(Lifecycle::Exited)
                ],
            )?;
        }
        if moved == 1 {
            tx.execute(
                "UPDATE text_mutations SET status = 'indeterminate', settled_at = ?2
                  WHERE session_uid = ?1 AND status = 'applying'",
                params![session_uid, now],
            )?;
        }
        tx.commit()?;
        Ok(moved == 1)
    }

    /// A relaunch of either agent that did not reach its link, put back in one transaction: the
    /// run `exited` from `starting`, or from `failed` when the watch failed it for the
    /// relaunch's own `launched` host, still the recorded one; and its old host `old`, when it
    /// had one, recorded again unless the relaunch recorded its own, so the next message's
    /// warrant reads the launch that ran last. Whether the run moved back.
    pub fn undo_relaunch(
        &self,
        session_uid: &str,
        old: Option<&WatchedHost>,
        launched: Option<&ProcessIdentity>,
    ) -> Result<bool> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let failed_by_its_launch = match launched {
            Some(host) => recorded_host_is(&tx, session_uid, host)?,
            None => false,
        };
        if let Some(old) = old {
            tx.execute(
                "UPDATE run_hosts SET host_pid = ?2, host_start_sec = ?3, host_start_usec = ?4,
                                      boot_sec = ?5, boot_usec = ?6
                  WHERE session_uid = ?1 AND host_pid IS NULL",
                params![
                    session_uid,
                    old.host.pid,
                    old.host.birth.start_sec,
                    old.host.birth.start_usec,
                    old.boot.map(|boot| boot.boot_sec),
                    old.boot.map(|boot| boot.boot_usec)
                ],
            )?;
        }
        let now = protocol::time::now_rfc3339();
        let mut moved = 0;
        // A uid is in one table only.
        for table in SESSION_TABLES {
            moved += tx.execute(
                &format!(
                    "UPDATE {table} SET lifecycle = ?2, updated_at = ?3
                      WHERE session_uid = ?1 AND (lifecycle = ?4 OR (?5 AND lifecycle = ?6))"
                ),
                params![
                    session_uid,
                    lifecycle_str(Lifecycle::Exited),
                    now,
                    lifecycle_str(Lifecycle::Starting),
                    failed_by_its_launch,
                    lifecycle_str(Lifecycle::Failed)
                ],
            )?;
        }
        tx.commit()?;
        Ok(moved == 1)
    }

    /// What a link to the hosted run `session_uid`'s host starts from: the run's
    /// lifecycle, its cursor, its stored `start`, and its turn, the uuid its last SDK
    /// init stamped on its `SessionStart`. `None` when the run has no `run_hosts` row.
    pub fn hosted_link(&self, session_uid: &str) -> Result<Option<HostedLink>> {
        let conn = self.read();
        let Some((lifecycle, cursor, start, launched)) = conn
            .query_row(
                "SELECT s.lifecycle, h.cursor, h.start, h.launch_account
                   FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE h.session_uid = ?1",
                params![session_uid],
                |row| {
                    Ok((
                        parse_lifecycle(&row.get::<_, String>(0)?),
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let turn = conn
            .query_row(
                "SELECT turn_id FROM events
                  WHERE session_uid = ?1 AND source = ?2 AND kind = ?3
                  ORDER BY seq DESC LIMIT 1",
                params![
                    session_uid,
                    protocol::event::Source::Sdk.as_str(),
                    protocol::event::EventKind::SessionStart.as_str()
                ],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(Some(HostedLink {
            lifecycle,
            cursor: cursor as u64,
            start,
            turn,
            launched: launched.and_then(|json| serde_json::from_str(&json).ok()),
        }))
    }

    /// The answer kept for the folder whose real path is `path`.
    pub fn folder_trust(&self, path: &str) -> Result<Option<Trust>> {
        let trusted = self
            .read()
            .query_row(
                "SELECT trusted FROM folder_trust WHERE path = ?1",
                params![path],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        Ok(trusted.map(trust_from))
    }

    /// Keeps `trust` as the folder's answer, replacing any earlier one.
    pub fn set_folder_trust(&self, path: &str, trust: &Trust) -> Result<()> {
        self.write().execute(
            "INSERT INTO folder_trust(path, trusted, answered_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET
                 trusted = excluded.trusted, answered_at = excluded.answered_at",
            params![
                path,
                *trust == Trust::Trusted,
                protocol::time::now_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Forgets the folder's answer; whether there was one.
    pub fn forget_folder_trust(&self, path: &str) -> Result<bool> {
        let removed = self
            .write()
            .execute("DELETE FROM folder_trust WHERE path = ?1", params![path])?;
        Ok(removed > 0)
    }

    /// Every folder's answer, newest first.
    pub fn folder_trusts(&self) -> Result<Vec<FolderAnswer>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT path, trusted, answered_at FROM folder_trust
              ORDER BY answered_at DESC, path",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(FolderAnswer {
                    path: row.get(0)?,
                    trust: trust_from(row.get(1)?),
                    answered_at: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A new account for `agent` (`claude` or `codex`), labelled `label`, with an id of ccd's own making: the only
    /// name its folder is given. Not yet the active one.
    pub fn add_account(&self, agent: &str, label: &str) -> Result<Account> {
        let conn = self.write();
        // One label per agent: the same label again (a retry after a lost reply) is the account it made.
        if let Some(account) = conn
            .query_row(
                "SELECT id, agent, label FROM accounts WHERE agent = ?1 AND label = ?2",
                params![agent, label],
                account_from,
            )
            .optional()?
        {
            return Ok(account);
        }
        let account = Account {
            id: protocol::uid::new()?,
            agent: agent.to_owned(),
            label: label.to_owned(),
        };
        conn.execute(
            "INSERT INTO accounts(id, agent, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                account.id,
                account.agent,
                account.label,
                protocol::time::now_rfc3339()
            ],
        )?;
        Ok(account)
    }

    /// Every account, both agents, oldest first.
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let conn = self.read();
        let mut stmt =
            conn.prepare("SELECT id, agent, label FROM accounts ORDER BY created_at, id")?;
        let rows = stmt
            .query_map([], account_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The account `agent`'s starts use now; `None` is its Default, the agent as it is on this computer.
    pub fn active_account(&self, agent: &str) -> Result<Option<Account>> {
        Ok(self
            .read()
            .query_row(
                "SELECT a.id, a.agent, a.label
                   FROM account_selection AS s JOIN accounts AS a ON a.id = s.account_id
                  WHERE s.agent = ?1 AND a.agent = ?1",
                params![agent],
                account_from,
            )
            .optional()?)
    }

    /// Makes `id` the account `agent`'s starts use, or its Default for `None`, and marks `agent`'s live runs on another
    /// account due to move to it ([`crate::switch`]), in one transaction. False, and nothing changed, for an id that is
    /// not one of `agent`'s accounts.
    pub fn activate_account(&self, agent: &str, id: Option<&str>) -> Result<bool> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match id {
            Some(id) => {
                let known = tx
                    .query_row(
                        "SELECT 1 FROM accounts WHERE id = ?1 AND agent = ?2",
                        params![id, agent],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !known {
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO account_selection(agent, account_id) VALUES (?1, ?2)
                     ON CONFLICT(agent) DO UPDATE SET account_id = excluded.account_id",
                    params![agent, id],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM account_selection WHERE agent = ?1",
                    params![agent],
                )?;
            }
        }
        // Its live runs are due to move to it, each at its next message ([`crate::switch::moves`]), but a run already
        // on it; a run already moving goes on moving, and starts on the account active then.
        tx.execute(
            "UPDATE run_hosts SET switching = CASE WHEN account_id IS ?4 THEN NULL ELSE ?2 END
              WHERE switching IS NOT ?3 AND session_uid IN
                    (SELECT session_uid FROM all_sessions WHERE agent = ?1 AND lifecycle = 'live')",
            params![agent, Store::SWITCH_WAITING, Store::SWITCH_MOVED, id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The account a run started on, as `begin_run` recorded it; `None` for its Default, and for every run recorded
    /// before accounts.
    /// Removes the account `id` and any agent's choice of it (that agent goes back to its Default). The runs that
    /// recorded it keep the id, which then names no account. The removed account, or None when there was none.
    pub fn remove_account(&self, id: &str) -> Result<Option<Account>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let removed = tx
            .query_row(
                "SELECT id, agent, label FROM accounts WHERE id = ?1",
                params![id],
                account_from,
            )
            .optional()?;
        tx.execute(
            "DELETE FROM account_selection WHERE account_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed)
    }

    /// A switch's states ([`Store::switch_state`]): due, to move at the run's next message; its process being ended
    /// by that message, which then starts it on the new account; that start not gone through.
    pub const SWITCH_WAITING: i64 = 1;
    pub const SWITCH_FAILED: i64 = 2;
    pub const SWITCH_MOVED: i64 = 3;

    /// Where the run `session_uid`'s switch of account stands: [`Store::SWITCH_WAITING`],
    /// [`Store::SWITCH_MOVED`], [`Store::SWITCH_FAILED`], or `None`.
    pub fn switch_state(&self, session_uid: &str) -> Result<Option<i64>> {
        Ok(self
            .read()
            .query_row(
                "SELECT switching FROM run_hosts WHERE session_uid = ?1",
                params![session_uid],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The run `session_uid`'s switch now stands at `state` (`None`: it has none).
    pub fn set_switch_state(&self, session_uid: &str, state: Option<i64>) -> Result<()> {
        self.write().execute(
            "UPDATE run_hosts SET switching = ?2 WHERE session_uid = ?1",
            params![session_uid, state],
        )?;
        Ok(())
    }

    /// Every run with a switch of account, and where it stands.
    pub fn switching_runs(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.read();
        let mut stmt = conn
            .prepare("SELECT session_uid, switching FROM run_hosts WHERE switching IS NOT NULL")?;
        let runs = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(runs)
    }

    /// Whether a run still going (starting, live or stopping) records the account `id`.
    pub fn account_in_use(&self, id: &str) -> Result<bool> {
        Ok(self
            .read()
            .query_row(
                "SELECT 1 FROM run_hosts AS h JOIN all_sessions AS s USING(session_uid)
                  WHERE h.account_id = ?1 AND s.lifecycle IN ('starting', 'live', 'stopping') LIMIT 1",
                params![id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Every run CodeConnect started, with the account it records: `None` for its agent's Default, `Some(id, label)`
    /// for an account, and a label of `None` for an account since removed.
    pub fn run_accounts(&self) -> Result<Vec<(String, RecordedAccount)>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT h.session_uid, h.account_id, a.label
               FROM run_hosts AS h LEFT JOIN accounts AS a ON a.id = h.account_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id: Option<String> = row.get(1)?;
                let label: Option<String> = row.get(2)?;
                Ok((row.get::<_, String>(0)?, id.map(|id| (id, label))))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The run `session_uid` is live on `account` (`None`: its agent's Default), recorded once its relaunch went live,
    /// so a relaunch that did not leaves the account its process ran on.
    pub fn set_run_account(&self, session_uid: &str, account: Option<&str>) -> Result<()> {
        self.write().execute(
            "UPDATE run_hosts SET account_id = ?2 WHERE session_uid = ?1",
            params![session_uid, account],
        )?;
        Ok(())
    }

    pub fn run_account(&self, session_uid: &str) -> Result<Option<Account>> {
        Ok(self
            .read()
            .query_row(
                "SELECT a.id, a.agent, a.label
                   FROM run_hosts AS h JOIN accounts AS a ON a.id = h.account_id
                  WHERE h.session_uid = ?1",
                params![session_uid],
                account_from,
            )
            .optional()?)
    }

    /// The whole fleet, both agents, oldest first.
    ///
    /// Reads `all_sessions` — see [`create_schema`]. This daemon owns Codex runs
    /// as well as Claude ones, so every read projection answers for both; only
    /// the *old* daemon is meant to be blind to half of them, and it is blind by
    /// not knowing the name of the second table, not by anything written here.
    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, session_id, cwd,
                    claude_session_id, transcript_path, lifecycle, created_at, updated_at,
                    agent, codex_thread_id
               FROM all_sessions ORDER BY created_at ASC, session_uid ASC",
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
    /// Shares `SESSION_SCOPED_TABLES` and the ended predicate (`exited` or `failed`) with
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

        // **The least deletable copy is the one that answers, and the `ORDER BY`
        // is what makes that true.** A uid is supposed to have exactly one
        // identity row across the two physical tables, and
        // `sessions_refuse_codex_shadow` is what keeps that so. If one ever
        // survives anyway, `all_sessions` returns the run twice and an
        // unordered `query_row` takes whichever the planner reached first —
        // which, with a rollback shadow reading `exited` beside a live isolated
        // run, is a coin toss that decides whether the guard below fires. The
        // children are deleted *before* the identity rows, and the exit
        // predicate the identity delete restates matches only the shadow, so
        // losing that toss commits: the live run's events and ledgers are gone
        // and its row is still standing. Sorting the undeletable copies first
        // makes the guard read the copy that refuses, so the predicate has to
        // hold for **every** identity row of the uid rather than for one of
        // them.
        //
        // The plan is unchanged where it matters — still two covering-index
        // seeks, one per table — with a temp b-tree over the at most two rows
        // they return. Measured over 2,000 sessions: 0.0023ms to 0.0040ms per
        // lookup.
        let row: Option<String> = tx
            .query_row(
                "SELECT lifecycle FROM all_sessions
                  WHERE session_uid = ?1
                  ORDER BY (lifecycle IN (?2, ?3)) ASC
                  LIMIT 1",
                params![
                    session_uid,
                    lifecycle_str(Lifecycle::Exited),
                    lifecycle_str(Lifecycle::Failed)
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(lifecycle) = row else {
            return Ok(DeleteOutcome::NotFound);
        };
        // A run due to move, or moving, reads live until a message has started it; one whose move failed is ended.
        let switching: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_hosts WHERE session_uid = ?1 AND switching IN (?2, ?3))",
            params![session_uid, Store::SWITCH_WAITING, Store::SWITCH_MOVED],
            |row| row.get(0),
        )?;
        if switching {
            return Ok(DeleteOutcome::NotExited {
                lifecycle: lifecycle_str(Lifecycle::Live).to_string(),
            });
        }
        let ended = lifecycle == lifecycle_str(Lifecycle::Exited)
            || lifecycle == lifecycle_str(Lifecycle::Failed);
        if !ended {
            // **Which non-ended state it is, because they do not mean the same
            // thing.** `Live` and `Starting` say the agent is there. `Unknown`
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
        // Both tables, and the *total* is what must be one. The run is in
        // exactly one of them, so the other statement matches nothing; summing
        // rather than dispatching keeps the guarded predicate — and the
        // assertion below that it removed precisely one row — as the single
        // safety rule it has always been, instead of making it conditional on
        // having guessed the agent right.
        let mut gone = 0usize;
        for table in SESSION_TABLES {
            gone += tx.execute(
                // A compile-time list in this file; nothing a caller supplies
                // reaches the statement text.
                &format!(
                    "DELETE FROM {table}
                      WHERE session_uid = ?1
                        AND lifecycle IN (?2, ?3)"
                ),
                params![
                    session_uid,
                    lifecycle_str(Lifecycle::Exited),
                    lifecycle_str(Lifecycle::Failed)
                ],
            )?;
        }
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
    /// What makes it safe to run at all is that it is defined *only* over ended
    /// runs, `Exited` and `Failed`. `Live` is left alone because the agent may be working, and —
    /// more sharply — `Unknown` is left alone because "we could not establish
    /// what happened to this" is the one state where deletion destroys the
    /// evidence somebody would need to find out. `protect` is the caller's
    /// additional veto, for runs the daemon can see are still in use whatever
    /// the row says.
    ///
    /// Three things make this hard to get wrong:
    ///   * One `BEGIN IMMEDIATE` around the whole sweep, so a session cannot
    ///     come back to life between being chosen and being deleted.
    ///   * The final `DELETE` re-states the ended lifecycles in its own
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
            // **Ended has to be true of every copy of the uid, not of one.** The
            // same duplicate `delete_exited_session` guards against: a rollback
            // shadow reading `exited` in `sessions` beside the live isolated run
            // in `codex_sessions`. Without the `NOT EXISTS` the shadow is a
            // candidate, the loop below deletes the uid's children before it
            // touches either identity row, and the identity delete then restates
            // the ended lifecycles — matching the shadow and not the live copy,
            // so `gone == 1`, the sweep commits, and a running agent's events and
            // ledgers are gone. `sessions_refuse_codex_shadow` is what stops the
            // duplicate existing; this is what stops it being destructive if one
            // ever does.
            //
            // The `NOT EXISTS` is two index seeks per candidate, not a second
            // scan: measured, SQLite pushes it into both branches of the view
            // the same way the outer query is pushed. Over 2,000 sessions the
            // candidate sweep goes from 0.76ms to 1.18ms, for a command a human
            // runs by hand.
            let mut stmt = tx.prepare(
                "SELECT session_uid, session_id, cwd, created_at, updated_at
                   FROM all_sessions AS a
                  WHERE a.lifecycle IN (?1, ?2)
                    AND NOT EXISTS(SELECT 1 FROM all_sessions AS b
                                    WHERE b.session_uid = a.session_uid
                                      AND b.lifecycle NOT IN (?1, ?2))
                    AND NOT EXISTS(SELECT 1 FROM run_hosts AS h
                                    WHERE h.session_uid = a.session_uid AND h.switching IS NOT NULL)
                  ORDER BY created_at ASC, session_uid ASC",
            )?;
            let ended = params![
                lifecycle_str(Lifecycle::Exited),
                lifecycle_str(Lifecycle::Failed)
            ];
            let rows = stmt.query_map(ended, |row| {
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
                // Both tables, total must be one — see `delete_exited_session`.
                // **The prune reaches Codex runs on purpose.** The isolation
                // this schema buys is from the *old* daemon, which cannot name
                // the second table; it is not isolation from ourselves. An
                // operator who asks this daemon to remove ended sessions is
                // asking about their whole fleet, and a prune that quietly kept
                // half of it would be the same kind of lie in the other
                // direction.
                let mut gone = 0usize;
                for table in SESSION_TABLES {
                    gone += tx.execute(
                        // A compile-time list in this file; nothing a caller
                        // supplies reaches the statement text.
                        &format!(
                            "DELETE FROM {table} WHERE session_uid = ?1 AND lifecycle IN (?2, ?3)"
                        ),
                        params![
                            session_uid,
                            lifecycle_str(Lifecycle::Exited),
                            lifecycle_str(Lifecycle::Failed)
                        ],
                    )?;
                }
                // Belt and braces over the query above. If this ever removes
                // anything other than exactly the one ended run it named, the
                // whole transaction is abandoned rather than half-applied.
                anyhow::ensure!(
                    gone == 1,
                    "refusing to prune {session_uid}: the delete matched {gone} rows rather than \
                     the one ended session it named"
                );
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
    /// risk), so this counts them. [`Store::migrate`] deletes them at every open,
    /// as it deletes every run without an engine host record, so what this counts
    /// was filed since. Reported so the operator can see that it exists.
    ///
    /// Reads `all_sessions`, so a Codex run's events are not counted as
    /// orphans. Left on the physical `sessions` table this would report every
    /// event of every live Codex session as history whose provenance nobody
    /// understands — a number an operator would read as corruption.
    pub fn orphan_event_count(&self) -> Result<u64> {
        let conn = self.read();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events
              WHERE session_uid NOT IN (SELECT session_uid FROM all_sessions)",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Move one run to a new lifecycle, whichever table holds it.
    ///
    /// Both tables, not a lookup then a dispatch: the uid is a primary key in
    /// each and lives in exactly one, so the statement that does not match it
    /// updates nothing. Doing it this way removes the failure the dispatch
    /// version would have — a wrong guess about the agent leaves a run that can
    /// never be marked `Exited`, and therefore can never be deleted or pruned,
    /// while `set_lifecycle` still returns `Ok(())`. That is exactly the
    /// silence this method has no way to report.
    ///
    /// **One stamp for both statements, read before the loop rather than inside
    /// it.** One lifecycle transition happened, at one time, and reading the
    /// clock per table would say it happened twice — a millisecond apart if the
    /// two reads land in different milliseconds.
    pub fn set_lifecycle(&self, session_uid: &str, lifecycle: Lifecycle) -> Result<()> {
        let conn = self.write();
        let now = protocol::time::now_rfc3339();
        for table in SESSION_TABLES {
            conn.execute(
                // A compile-time list in this file; nothing a caller supplies
                // reaches the statement text.
                &format!(
                    "UPDATE {table} SET lifecycle = ?2, updated_at = ?3 WHERE session_uid = ?1"
                ),
                params![session_uid, lifecycle_str(lifecycle), now],
            )?;
        }
        Ok(())
    }

    /// Move the run `session_uid` from `from` to `to`, only while it is still `from`, in
    /// whichever table holds it. Whether it moved.
    pub fn advance_lifecycle(
        &self,
        session_uid: &str,
        from: Lifecycle,
        to: Lifecycle,
    ) -> Result<bool> {
        let conn = self.write();
        let now = protocol::time::now_rfc3339();
        let mut moved = 0;
        for table in SESSION_TABLES {
            moved += conn.execute(
                &format!(
                    "UPDATE {table} SET lifecycle = ?3, updated_at = ?4
                      WHERE session_uid = ?1 AND lifecycle = ?2"
                ),
                params![session_uid, lifecycle_str(from), lifecycle_str(to), now],
            )?;
        }
        Ok(moved == 1)
    }

    /// Record the thread a ccd-hosted Codex run's first `thread/start` made, only while the
    /// run is still `starting`. Whether it was recorded.
    pub fn record_hosted_thread(&self, session_uid: &str, thread_id: &str) -> Result<bool> {
        let (table, _) = session_tables_for(&AgentKind::Codex);
        let recorded = self.write().execute(
            &format!(
                "UPDATE {table} SET codex_thread_id = ?2
                  WHERE session_uid = ?1 AND lifecycle = ?3
                    AND EXISTS (SELECT 1 FROM run_hosts WHERE session_uid = ?1)"
            ),
            params![session_uid, thread_id, lifecycle_str(Lifecycle::Starting)],
        )?;
        Ok(recorded == 1)
    }

    /// Record `thread_id` as a ccd-hosted Codex run's thread in place of the one it has, only
    /// while the run has no message: a thread that never ran a turn holds nothing to lose.
    /// Whether it was recorded.
    pub fn replace_unsaid_thread(&self, session_uid: &str, thread_id: &str) -> Result<bool> {
        let (table, _) = session_tables_for(&AgentKind::Codex);
        let recorded = self.write().execute(
            &format!(
                "UPDATE {table} SET codex_thread_id = ?2
                  WHERE session_uid = ?1
                    AND NOT EXISTS (SELECT 1 FROM events WHERE session_uid = ?1 AND kind = ?3)"
            ),
            params![session_uid, thread_id, EventKind::UserMessage.as_str()],
        )?;
        Ok(recorded == 1)
    }

    /// Insert-once ledger write: [`LedgerWrite::Existing`], writing nothing, if an
    /// outcome is already recorded.
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
        let existing: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM answers WHERE session_uid = ?1 AND request_id = ?2",
                params![session_uid, request_id],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            tx.commit()?;
            return Ok(LedgerWrite::Existing);
        }
        tx.execute(
            // `WHERE EXISTS(sessions)`: an answer settling just as its session
            // is deleted must vanish with the session rather than survive it as
            // a row nothing can ever name again.
            "INSERT INTO answers(session_uid, request_id, payload_hash, outcome, created_at)
             SELECT ?1, ?2, ?3, ?4, ?5
              WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)",
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
        Self::free_device_name(&self.read(), requested, None)
    }

    /// Name `device_id` as it now asks to be named, made unique against the other devices only, so a device that
    /// keeps its name keeps it exactly and two devices asking for one name stay `name` and `name-2`. Returns the name
    /// it is listed under now.
    pub fn rename_device(&self, device_id: &str, requested: &str) -> Result<String> {
        let conn = self.write();
        let name = Self::free_device_name(&conn, requested, Some(device_id))?;
        conn.execute(
            "UPDATE devices SET name = ?2 WHERE device_id = ?1 AND name <> ?2",
            params![device_id, name],
        )?;
        Ok(name)
    }

    /// The first of `requested`, `requested-2`, … that no device holds, `except` this one: names are how a human
    /// revokes a device, so they are unique, compared without case.
    fn free_device_name(
        conn: &Connection,
        requested: &str,
        except: Option<&str>,
    ) -> Result<String> {
        let base = {
            let trimmed = requested.trim();
            if trimmed.is_empty() {
                "device"
            } else {
                trimmed
            }
        };
        for suffix in 1..=9999u32 {
            let candidate = if suffix == 1 {
                base.to_string()
            } else {
                format!("{base}-{suffix}")
            };
            let taken: i64 = conn.query_row(
                "SELECT COUNT(*) FROM devices WHERE name = ?1 COLLATE NOCASE AND device_id IS NOT ?2",
                params![candidate, except],
                |row| row.get(0),
            )?;
            if taken == 0 {
                return Ok(candidate);
            }
        }
        anyhow::bail!("no free device name based on {base:?}")
    }

    /// `tailnet_node` is the Tailscale device it paired from, when Tailscale
    /// vouched for that address.
    pub fn insert_device(
        &self,
        device_id: &str,
        name: &str,
        token_hash: &str,
        created_at: &str,
        tailnet_node: Option<&str>,
    ) -> Result<()> {
        let conn = self.write();
        insert_device_row(&conn, device_id, name, token_hash, created_at, tailnet_node)
    }

    /// **Pair a device Tailscale vouched for, unless that device is kept out.**
    ///
    /// The checks and the insert are one `BEGIN IMMEDIATE` on the one write
    /// connection, which [`Store::revoke_device`] also writes through, so a revoke
    /// lands wholly before this (and the check refuses) or wholly after it (and
    /// revokes the row this inserts).
    pub fn admit_tailnet_device(
        &self,
        device_id: &str,
        name: &str,
        token_hash: &str,
        created_at: &str,
        tailnet_node: &str,
    ) -> Result<Admission> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let blocked: i64 = tx.query_row(
            "SELECT COUNT(*) FROM devices WHERE tailnet_node = ?1 AND revoked_at IS NOT NULL",
            params![tailnet_node],
            |row| row.get(0),
        )?;
        if blocked > 0 {
            return Ok(Admission::Blocked);
        }
        let paused: i64 = tx.query_row(
            "SELECT COUNT(*) FROM devices
              WHERE paired_before_nodes = 1 AND tailnet_node IS NULL AND revoked_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        if paused > 0 {
            return Ok(Admission::Paused);
        }
        insert_device_row(
            &tx,
            device_id,
            name,
            token_hash,
            created_at,
            Some(tailnet_node),
        )?;
        tx.commit()?;
        Ok(Admission::Admitted)
    }

    /// Record the Tailscale device a token was presented from, on the row that
    /// token belongs to, revoked or not, when that row has none yet.
    pub fn stamp_tailnet_node(&self, token_hash: &str, tailnet_node: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE devices SET tailnet_node = ?2
              WHERE token_hash = ?1 AND tailnet_node IS NULL",
            params![token_hash, tailnet_node],
        )?;
        Ok(())
    }

    /// The Tailscale devices kept out: those a revoked device carries.
    pub fn blocked_tailnet_nodes(&self) -> Result<Vec<String>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT tailnet_node FROM devices
              WHERE revoked_at IS NOT NULL AND tailnet_node IS NOT NULL",
        )?;
        let nodes = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(nodes)
    }

    /// Let a revoked device back in, and every revoked device on the same
    /// Tailscale device. Each keeps its token, so the device signs in again with
    /// the pairing it already holds. Returns how many rows were restored; zero
    /// when the device is not revoked, which is refused rather than restored.
    pub fn unblock_device(&self, device_id: &str) -> Result<usize> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let node: Option<Option<String>> = tx
            .query_row(
                "SELECT tailnet_node FROM devices WHERE device_id = ?1 AND revoked_at IS NOT NULL",
                params![device_id],
                |row| row.get(0),
            )
            .optional()?;
        let restored = match node {
            None => 0,
            Some(None) => tx.execute(
                "UPDATE devices SET revoked_at = NULL WHERE device_id = ?1",
                params![device_id],
            )?,
            Some(Some(node)) => tx.execute(
                "UPDATE devices SET revoked_at = NULL
                  WHERE tailnet_node = ?1 AND revoked_at IS NOT NULL",
                params![node],
            )?,
        };
        tx.commit()?;
        Ok(restored)
    }

    /// Authenticate a presented token in one read: the active device it names, a pairing the
    /// computer has blocked (the device keeps its token and `unblock_device` readmits it), or
    /// no device. One read, so an unblock landing between two reads cannot mislabel a
    /// now-valid token, and no caller can forget to check `revoked_at`.
    pub fn device_by_token_hash(&self, token_hash: &str) -> Result<TokenAuth> {
        let conn = self.read();
        let row = conn
            .query_row(
                "SELECT device_id, name, created_at, last_seen_at, revoked_at,
                        tailnet_node, paired_before_nodes
                   FROM devices
                  WHERE token_hash = ?1",
                params![token_hash],
                device_row_from,
            )
            .optional()?;
        Ok(match row {
            Some(device) if device.revoked_at.is_some() => TokenAuth::Revoked,
            Some(device) => TokenAuth::Active(Box::new(device)),
            None => TokenAuth::Missing,
        })
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
        let mut stmt = conn.prepare(&format!("{PUSH_TARGET_COLUMNS} {PUSH_TARGET_ELIGIBLE}"))?;
        let rows = stmt.query_map([], push_registration)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One device's push registration, or `None` when it has none to offer.
    ///
    /// **The same question [`Store::push_targets`] answers, asked about one row.**
    /// The predicate is the same and so is the decode — both come from the
    /// constants above, so the fan-out's list and this lookup cannot come to
    /// disagree about who may be pushed to. A device that is revoked, has no token, or does not exist answers
    /// `None`, which is the same "nothing to vouch for" the fan-out reads as an
    /// absence from its list.
    ///
    /// **Why a second read exists at all.** The authorization re-check before a
    /// transport attempt is about ONE device id. Answering it out of the fleet
    /// list would make the cost of a single push proportional to the number of
    /// registered phones — a whole-table scan per attempt, on the blocking pool,
    /// with as many attempts in flight as there are devices with work. This is
    /// the indexed lookup that question deserves. See
    /// [`crate::push_queue::still_authorized`].
    pub fn push_target_for(&self, device_id: &str) -> Result<Option<PushRegistration>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                &format!("{PUSH_TARGET_COLUMNS} {PUSH_TARGET_ELIGIBLE} AND device_id = ?1"),
                params![device_id],
                push_registration,
            )
            .optional()?)
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
            "SELECT device_id, name, created_at, last_seen_at, revoked_at,
                    tailnet_node, paired_before_nodes
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

    /// Returns every device it revoked: this one, unless it was already revoked,
    /// and every active device on the same Tailscale device, which is the same
    /// phone or computer. Empty when there was nothing to do, so the caller can
    /// report that instead of pretending it acted.
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
    pub fn revoke_device(&self, device_id: &str, at: &str) -> Result<Vec<String>> {
        let conn = self.write();
        let mut stmt = conn.prepare(
            "UPDATE devices
                SET revoked_at = ?2,
                    push_token = NULL,
                    push_environment = NULL,
                    push_credential = NULL
              WHERE revoked_at IS NULL
                AND (device_id = ?1
                     OR tailnet_node = (SELECT tailnet_node FROM devices WHERE device_id = ?1))
              RETURNING device_id",
        )?;
        let revoked = stmt
            .query_map(params![device_id, at], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(revoked)
    }

    // ----------------------------------------------------- pending approvals

    pub fn delete_pending_approval(&self, session_uid: &str, request_id: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "DELETE FROM pending_approvals WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id],
        )?;
        Ok(())
    }

    /// **File one Codex approval card and the fact it stands behind, in one
    /// commit.**
    ///
    /// The two halves were two writes, and the gap between them was where the
    /// durable-card guarantee leaked. The `ApprovalRequest` event went first,
    /// so a row the schema then refused — the unique index catching a broken
    /// derivation — left a filed request event with no card anywhere and no
    /// terminal that could ever retire it: a permanently unresolved fact in the
    /// log. And an ordinary write failure left the reverse, an in-memory card
    /// the phone was rung about and a restart forgot.
    ///
    /// One `BEGIN IMMEDIATE` removes both orderings: a projection and the fact it
    /// projects cannot be two commits.
    ///
    /// The row is written **first inside the transaction**, so a constraint
    /// refusal aborts before any event is appended rather than after.
    pub fn raise_codex_pending_approval(
        &self,
        row: &CodexPendingApprovalRow,
        pending: &PendingEvent,
    ) -> Result<CodexCardRaise> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = upsert_codex_card_in_tx(&tx, row)?;
        if !matches!(outcome, CodexCardOutcome::Filed | CodexCardOutcome::Rebound) {
            // The run was deleted mid-flight, or the re-delivery carried a
            // different question. Rolled back rather than committed empty, so
            // the event does not land either and the stored card is untouched.
            tx.rollback()?;
            return Ok(CodexCardRaise {
                outcome,
                event: None,
            });
        }
        let event = append_in_tx(&tx, pending)?;
        tx.commit()?;
        Ok(CodexCardRaise { outcome, event })
    }

    /// **Retire one Codex card: delete the row and file its terminal, in one
    /// commit.**
    ///
    /// The mirror of [`Store::raise_codex_pending_approval`], and it closes the
    /// mirror failure. A failed delete followed by a successful resolution event
    /// left a row that recovery restores — an already-answered question back on
    /// the phone after a restart, because nothing writes a Codex row into
    /// `answers` and the recovery read's terminal check is asked of that table.
    /// A successful delete followed by a failed append lost the only terminal
    /// this card will ever have.
    ///
    /// Returns the resolution event, or `None` when it was a duplicate — the
    /// `resolved:{request_id}` source id is the third of the three
    /// first-terminal-wins guards and the only one that survives a reconnect.
    /// **`answer` joins the same commit.** A phone answer's ledger row is the
    /// record that this card must never be answered again; the card's deletion is
    /// the record that it is no longer being asked. Settling one without the other
    /// is the failure either way round — a settled ledger over a standing card is
    /// a question the operator can never answer again, and a retired card over a
    /// live claim is a claim recovery will make terminal for a card that already
    /// has a terminal. `None` for every terminal that is not a phone answer.
    pub fn retire_codex_pending_approval(
        &self,
        session_uid: &str,
        request_id: &str,
        pending: &PendingEvent,
        answer: Option<AnswerTerminal>,
    ) -> Result<Option<Event>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM codex_pending_approvals WHERE session_uid = ?1 AND request_id = ?2",
            params![session_uid, request_id],
        )?;
        if let Some(answer) = answer {
            settle_answer_in_tx(&tx, session_uid, request_id, answer, &pending.ts)?;
        }
        let event = append_in_tx(&tx, pending)?;
        tx.commit()?;
        Ok(event)
    }

    /// Every open Codex card for one run, with the Codex identity the fleet
    /// view deliberately does not project.
    ///
    /// Retirement is what needs this and it is why the four identity columns
    /// exist: a turn terminal clears the cards bound to that turn, a thread
    /// switch clears the ones bound to the retired thread, and an item's own
    /// terminal clears the one bound to that item. Asked of the whole run in one
    /// read and filtered in Rust rather than three narrower queries — a run
    /// holds a handful of open cards at most, and one statement is one thing to
    /// keep true.
    ///
    /// Unlike [`Store::list_pending_approvals`] this does **not** exclude rows
    /// with a matching `answers` entry: nothing writes a Codex row into that
    /// shared table, so the predicate would be a filter that can never fire
    /// standing in front of the sweep that retires these cards.
    pub fn codex_pending_approvals(
        &self,
        session_uid: &str,
    ) -> Result<Vec<CodexPendingApprovalRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, session_id, request_id, card, generation, created_ms,
                    thread_id, turn_id, item_id, family
               FROM codex_pending_approvals
              WHERE session_uid = ?1
              ORDER BY created_ms ASC",
        )?;
        let rows = stmt.query_map(params![session_uid], |row| {
            Ok(CodexPendingApprovalRow {
                session_uid: row.get(0)?,
                session_id: row.get(1)?,
                request_id: row.get(2)?,
                card: row.get(3)?,
                generation: row.get::<_, i64>(4)? as u64,
                created_ms: row.get(5)?,
                thread_id: row.get(6)?,
                turn_id: row.get(7)?,
                item_id: row.get(8)?,
                family: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every card that was open when the daemon stopped.
    ///
    /// Rows whose approval has since been answered are excluded in SQL: the
    /// ledger is the terminal record, and a card that outlived its own answer
    /// is not something the recovery path should have to reason about.
    pub fn list_pending_approvals(&self) -> Result<Vec<PendingApprovalRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            // `all_pending_approvals`, not `pending_approvals`: recovery answers
            // "what is this fleet blocked on?" for both agents, and a Codex card
            // that survived a restart is exactly as real as a Claude one. Only
            // the OLD daemon is meant to be blind to half of them, and it is
            // blind by not knowing the second table's name — see `create_schema`.
            "SELECT p.session_uid, p.session_id, p.request_id, p.card
               FROM all_pending_approvals p
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
    /// Only ever called when the answer did not reach the agent — the run's host
    /// refused it or never took it — that is, when we know nothing was sent. A
    /// claim whose fate is unknown is left where it is.
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
    /// [`TextClaim::Claimed`] and both type. `text` is kept with the claim for a
    /// message to a run's agent host, so a restarted ccd can file the person's words
    /// when the host's replay says the message was taken ([`Store::host_sends`]).
    pub fn claim_text_mutation(
        &self,
        session_uid: &str,
        request_id: &str,
        payload_hash: &str,
        text: Option<&str>,
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
                    //
                    // The same bound obligation as `pending_approvals`, and the
                    // sharper one: a rolled-back v0.6.0 daemon does not merely
                    // read this table globally, its `recover_text_mutations`
                    // WRITES to it — every `applying` row becomes
                    // `indeterminate`, whoever it belongs to.
                    //
                    // And this row is written EARLY: `Daemon::send_text` claims
                    // here before the run's host has taken anything, so a crash
                    // between the two used to leave an `applying` row for the
                    // old daemon to rewrite. What stops
                    // that is `send_text` refusing a non-Claude session before
                    // it reaches this call — scaffolding that lifts once the
                    // table is split per agent. See [`create_schema`].
                    "INSERT INTO text_mutations(session_uid, request_id, payload_hash,
                                                status, matched, started_at, settled_at)
                     SELECT ?1, ?2, ?3, 'applying', ?5, ?4, NULL
                      WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)",
                    params![session_uid, request_id, payload_hash, now, text],
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

    /// The request id of every message sent to the hosted run `session_uid`, whatever became
    /// of it: what a Stop's receipt names a cancelled message by.
    pub fn send_request_ids(&self, session_uid: &str) -> Result<Vec<String>> {
        let conn = self.read();
        let mut stmt =
            conn.prepare("SELECT request_id FROM text_mutations WHERE session_uid = ?1")?;
        let rows = stmt
            .query_map(params![session_uid], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The run `session_uid`'s messages to its agent host still claimed, each by its
    /// request id with its text.
    pub fn host_sends(&self, session_uid: &str) -> Result<Vec<(String, String)>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT request_id, matched FROM text_mutations
              WHERE session_uid = ?1 AND status = 'applying' AND matched IS NOT NULL",
        )?;
        let rows = stmt
            .query_map(params![session_uid], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------------- mail
    //
    // `mail` is the receiver's side: one row per message for a session on this
    // computer, keyed by the sender's `message_id`. `mail_sent` is the sender's
    // side: one row per message this computer's agents sent, written before it goes
    // anywhere, and what the sender has been told of it. A message between two
    // sessions on this computer has a row in each.

    /// **Take one message for a session on this computer.** A `message_id` taken
    /// before keeps nothing new and says where that one stands, so a sender's
    /// retries leave one copy.
    pub fn take_mail(
        &self,
        mail: &MailIntake,
        now_ms: i64,
        limits: MailLimits,
    ) -> Result<MailTaken> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let known: Option<String> = tx
            .query_row(
                "SELECT state FROM mail WHERE message_id = ?1",
                params![mail.message_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(state) = known {
            return Ok(MailTaken::Known(mail_state(&state)));
        }
        let exists = tx
            .query_row(
                "SELECT 1 FROM all_sessions WHERE session_uid = ?1",
                params![mail.session_uid],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Ok(MailTaken::NoSession);
        }
        let last_hour: u32 = tx.query_row(
            "SELECT COUNT(*) FROM mail
              WHERE session_uid = ?1 AND sender_computer = ?2 AND sender_uid = ?3
                AND accepted_ms > ?4",
            params![
                mail.session_uid,
                mail.sender_computer,
                mail.sender_uid,
                now_ms - HOUR_MS
            ],
            |row| row.get(0),
        )?;
        if last_hour >= limits.per_pair_per_hour {
            return Ok(MailTaken::RateLimited);
        }
        // New mail resumes wakes that had stopped.
        tx.execute(
            "UPDATE mail_wake SET degraded = 0, misses = 0 WHERE session_uid = ?1",
            params![mail.session_uid],
        )?;
        tx.execute(
            "INSERT INTO mail(message_id, context_id, in_reply_to, session_uid, sender_computer,
                              sender_uid, sender, sender_label, sender_agent, sender_project,
                              text, accepted_ms, expires_ms, state, sender_told)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'waiting', 1)",
            params![
                mail.message_id,
                mail.context_id,
                mail.in_reply_to,
                mail.session_uid,
                mail.sender_computer,
                mail.sender_uid,
                mail.sender,
                mail.sender_label,
                mail.sender_agent,
                mail.sender_project,
                mail.text,
                now_ms,
                now_ms + limits.unread_ms
            ],
        )?;
        tx.commit()?;
        Ok(MailTaken::Kept)
    }

    /// **The wake rule's one decision: wake `session_uid` now?** Yes — and from here
    /// the wake is outstanding — when unread mail waits for it, no wake is
    /// outstanding, wakes have not stopped, and, for an agent that is woken only
    /// `between_turns`, no turn of it is running: its newest turn start is not later
    /// than its newest turn end. One transaction, so two callers never both wake one
    /// run.
    pub fn claim_mail_wake(
        &self,
        session_uid: &str,
        now_ms: i64,
        between_turns: bool,
    ) -> Result<bool> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !unread_mail(&tx, session_uid, now_ms)? {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR IGNORE INTO mail_wake(session_uid) VALUES (?1)",
            params![session_uid],
        )?;
        let claimed = tx.execute(
            "UPDATE mail_wake SET outstanding = 1
              WHERE session_uid = ?1 AND outstanding = 0 AND degraded = 0
                AND (?2 = 0 OR started_ms <= ended_ms)",
            params![session_uid, between_turns],
        )? == 1;
        tx.commit()?;
        Ok(claimed)
    }

    /// The claimed wake could not be issued: none is outstanding.
    pub fn release_mail_wake(&self, session_uid: &str) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE mail_wake SET outstanding = 0 WHERE session_uid = ?1",
            params![session_uid],
        )?;
        Ok(())
    }

    /// **A turn of `session_uid` ended.** The wake it answered, if any, is no longer
    /// outstanding; if mail is still unread after it, that is one more miss, and at
    /// `misses_allowed` misses wakes stop until a human turn or new mail.
    pub fn mail_turn_ended(
        &self,
        session_uid: &str,
        now_ms: i64,
        misses_allowed: u32,
    ) -> Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let unread = unread_mail(&tx, session_uid, now_ms)?;
        tx.execute(
            "UPDATE mail_wake
                SET misses = CASE WHEN NOT ?2 THEN 0
                                  WHEN outstanding = 1 THEN misses + 1
                                  ELSE misses END,
                    outstanding = 0
              WHERE session_uid = ?1",
            params![session_uid, unread],
        )?;
        tx.execute(
            "UPDATE mail_wake SET degraded = 1 WHERE session_uid = ?1 AND misses >= ?2",
            params![session_uid, misses_allowed],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// **A prompt of `session_uid` was submitted**, which starts a turn or joins the
    /// one running: a turn start. With a wake outstanding it is that wake's own
    /// turn. Otherwise it is the person's, which resumes wakes that had stopped.
    /// Returns whether it is the person's, with no turn running and unread mail
    /// waiting, which its turn is then told of. A prompt that joins a running turn,
    /// or is queued behind it, is not: told, it would read the mail inside a turn,
    /// and the wake after that turn reads it.
    pub fn mail_prompt(&self, session_uid: &str, now_ms: i64) -> Result<bool> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO mail_wake(session_uid) VALUES (?1)",
            params![session_uid],
        )?;
        let (woken, running): (bool, bool) = tx.query_row(
            "SELECT outstanding = 1, started_ms > ended_ms FROM mail_wake WHERE session_uid = ?1",
            params![session_uid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        tx.execute(
            "UPDATE mail_wake
                SET started_ms = MAX(started_ms, ?3),
                    misses = CASE WHEN ?2 THEN misses ELSE 0 END,
                    degraded = CASE WHEN ?2 THEN degraded ELSE 0 END
              WHERE session_uid = ?1",
            params![session_uid, woken, now_ms],
        )?;
        let tell = !woken && !running && unread_mail(&tx, session_uid, now_ms)?;
        tx.commit()?;
        Ok(tell)
    }

    /// **A turn of `session_uid` started, or ended, at these times**, by its
    /// transcript's own clock. Each mark only ever moves forward, so reading a line
    /// twice changes nothing.
    pub fn mail_turn_marks(
        &self,
        session_uid: &str,
        started_ms: Option<i64>,
        ended_ms: Option<i64>,
    ) -> Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO mail_wake(session_uid) VALUES (?1)",
            params![session_uid],
        )?;
        tx.execute(
            "UPDATE mail_wake SET started_ms = MAX(started_ms, COALESCE(?2, 0)),
                                  ended_ms = MAX(ended_ms, COALESCE(?3, 0))
              WHERE session_uid = ?1",
            params![session_uid, started_ms, ended_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Keep the protocol minor of the build that launched the run: its row here is what
    /// makes the run one mail can be for ([`Store::mail_runs`]).
    pub fn note_mail_minor(&self, session_uid: &str, minor: u32) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "INSERT INTO mail_runs(session_uid, minor) VALUES (?1, ?2)
             ON CONFLICT(session_uid) DO UPDATE SET minor = excluded.minor",
            params![session_uid, minor],
        )?;
        Ok(())
    }

    /// **Every run that has not ended**, with the protocol minor kept for it
    /// ([`Store::note_mail_minor`]): the runs mail can be for, whether or not their
    /// link is attached at this moment. A run its switch is moving reads live, its process ended or not.
    pub fn mail_runs(&self) -> Result<Vec<(SessionRow, u32)>> {
        let minors: std::collections::HashMap<String, u32> = {
            let conn = self.read();
            let mut stmt = conn.prepare("SELECT session_uid, minor FROM mail_runs")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let moving: std::collections::HashSet<String> = self
            .switching_runs()?
            .into_iter()
            .filter(|(_, state)| *state != Store::SWITCH_FAILED)
            .map(|(uid, _)| uid)
            .collect();
        Ok(self
            .list_sessions()?
            .into_iter()
            .filter(|row| row.lifecycle != Lifecycle::Exited || moving.contains(&row.session_uid))
            .filter_map(|row| {
                let minor = *minors.get(&row.session_uid)?;
                Some((row, minor))
            })
            .collect())
    }

    /// Every run with unread mail.
    pub fn runs_with_unread_mail(&self, now_ms: i64) -> Result<Vec<String>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT session_uid FROM mail
              WHERE state = 'waiting' AND expires_ms > ?1 ORDER BY session_uid",
        )?;
        let rows = stmt.query_map(params![now_ms], |row| row.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The conversation of a message `session_uid` was sent, to reply in.
    pub fn mail_context(&self, session_uid: &str, message_id: &str) -> Result<Option<String>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                "SELECT context_id FROM mail WHERE session_uid = ?1 AND message_id = ?2",
                params![session_uid, message_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// **Offer `session_uid` its unread messages**, oldest first and within the
    /// batch bounds, and the changes in where its own sent messages stand, under
    /// `offer`. Nothing offered is read until [`Store::read_mail`]; another reader
    /// is offered none of it meanwhile.
    pub fn offer_mail(&self, session_uid: &str, offer: &str, now_ms: i64) -> Result<MailOffer> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut unread = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT seq, message_id, context_id, in_reply_to, sender, sender_label,
                        sender_agent, sender_project, text
                   FROM mail
                  WHERE session_uid = ?1 AND state = 'waiting' AND expires_ms > ?2
                  ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![session_uid, now_ms], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    protocol::ipc::MailEnvelope {
                        message_id: row.get(1)?,
                        context_id: row.get(2)?,
                        in_reply_to: row.get(3)?,
                        from: row.get(4)?,
                        label: row.get(5)?,
                        agent: row.get(6)?,
                        project: row.get(7)?,
                        text: row.get(8)?,
                    },
                ))
            })?;
            for row in rows {
                unread.push(row?);
            }
        }
        let mut messages = Vec::new();
        let mut bytes = 0;
        for (seq, envelope) in &unread {
            let full = messages.len() == protocol::ipc::MAIL_BATCH_MESSAGES
                || (!messages.is_empty()
                    && bytes + envelope.text.len() > protocol::ipc::MAIL_BATCH_BYTES);
            if full {
                break;
            }
            bytes += envelope.text.len();
            tx.execute(
                "UPDATE mail SET state = 'offered', offer = ?2 WHERE seq = ?1",
                params![seq, offer],
            )?;
            messages.push(envelope.clone());
        }
        let more_pending = unread.len() > messages.len();
        tx.execute(
            "UPDATE mail_sent SET offer = ?2, offer_state = state
              WHERE session_uid = ?1 AND offer IS NULL AND state <> told_state",
            params![session_uid, offer],
        )?;
        let mut updates = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT message_id, recipient, offer_state, reason FROM mail_sent
                  WHERE offer = ?1 ORDER BY created_ms ASC",
            )?;
            let rows = stmt.query_map(params![offer], |row| {
                Ok(protocol::ipc::MailUpdate {
                    message_id: row.get(0)?,
                    to: row.get(1)?,
                    state: mail_state(&row.get::<_, String>(2)?),
                    reason: row.get(3)?,
                })
            })?;
            for row in rows {
                updates.push(row?);
            }
        }
        tx.commit()?;
        Ok(MailOffer {
            messages,
            updates,
            more_pending,
        })
    }

    /// **The offer reached its agent:** its messages are read, and the sender of
    /// each is told — here at once, on another computer through
    /// [`Store::untold_mail`] — and the updates it carried are given. Returns how many
    /// messages were read.
    pub fn read_mail(&self, offer: &str) -> Result<usize> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let read = settle_mail(
            &tx,
            "offer = ?1 AND state = 'offered'",
            params![offer],
            "read",
        )?;
        tx.execute(
            "UPDATE mail_sent SET told_state = offer_state, offer = NULL, offer_state = NULL
              WHERE offer = ?1",
            params![offer],
        )?;
        tx.commit()?;
        Ok(read)
    }

    /// The offer did not reach its agent: everything in it is unread again.
    pub fn release_mail(&self, offer: &str) -> Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE mail SET state = 'waiting', offer = NULL WHERE offer = ?1 AND state = 'offered'",
            params![offer],
        )?;
        tx.execute(
            "UPDATE mail_sent SET offer = NULL, offer_state = NULL WHERE offer = ?1",
            params![offer],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// At start-up, no offer is in flight: every one is unread again; and no Claude
    /// wake is outstanding.
    pub fn recover_mail(&self) -> Result<usize> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let released = tx.execute(
            "UPDATE mail SET state = 'waiting', offer = NULL WHERE state = 'offered'",
            [],
        )?;
        tx.execute(
            "UPDATE mail_sent SET offer = NULL, offer_state = NULL WHERE offer IS NOT NULL",
            [],
        )?;
        // A Claude wake is not outstanding across a restart. A Codex
        // wake stays outstanding: the link files the turn that carried it when it
        // attaches again, and that ends it, while a wake still held in Codex's queue
        // must not be queued twice.
        tx.execute(
            "UPDATE mail_wake SET outstanding = 0
              WHERE session_uid NOT IN (SELECT session_uid FROM codex_sessions)",
            [],
        )?;
        tx.commit()?;
        Ok(released)
    }

    /// **Settle what time and ended sessions decide:** unread messages past their
    /// expiry are expired, those for a run that has ended are ended (a run its switch of
    /// account ended keeps its waiting mail for its next process until it is ended for good), a sent
    /// message no computer took within the unread time is expired, one taken whose
    /// answer never came is expired once the receiver's unread time for it has
    /// surely run out, and settled rows older than [`MailLimits::settled_ms`] are
    /// dropped.
    pub fn sweep_mail(&self, now_ms: i64, limits: MailLimits) -> Result<()> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        settle_mail(
            &tx,
            "state = 'waiting' AND expires_ms <= ?1",
            params![now_ms],
            "expired",
        )?;
        settle_mail(
            &tx,
            "state IN ('waiting', 'offered')
             AND session_uid IN (SELECT session_uid FROM all_sessions WHERE lifecycle = ?1)
             AND NOT (state = 'waiting'
                      AND session_uid IN (SELECT session_uid FROM run_hosts WHERE switching IS NOT NULL))",
            params![lifecycle_str(Lifecycle::Exited)],
            "ended",
        )?;
        // A receiver takes a message up to one unread time after it was written, and
        // keeps it unread for one more.
        tx.execute(
            "UPDATE mail_sent SET state = 'expired'
              WHERE (state = 'sent' AND created_ms <= ?1)
                 OR (state = 'waiting' AND created_ms <= ?2)",
            params![now_ms - limits.unread_ms, now_ms - 2 * limits.unread_ms],
        )?;
        let settled = now_ms - limits.settled_ms;
        tx.execute(
            "DELETE FROM mail WHERE state IN ('read', 'expired', 'ended')
                AND accepted_ms <= ?1 AND sender_told = 1",
            params![settled],
        )?;
        tx.execute(
            "DELETE FROM mail_sent WHERE state <> 'sent' AND state = told_state AND created_ms <= ?1",
            params![settled],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A run ended: its unread messages are ended now. A run its switch of account ended goes on under its uid, so
    /// what waits for it is kept for its next process while its switch stands, moved or failed.
    pub fn end_mail(&self, session_uid: &str) -> Result<usize> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ended = settle_mail(
            &tx,
            "session_uid = ?1 AND state IN ('waiting', 'offered')
             AND NOT (state = 'waiting'
                      AND session_uid IN (SELECT session_uid FROM run_hosts WHERE switching IS NOT NULL))",
            params![session_uid],
            "ended",
        )?;
        tx.commit()?;
        Ok(ended)
    }

    /// Keep a message this computer's agent sent, before it goes anywhere.
    pub fn file_sent_mail(&self, mail: &SentMail, now_ms: i64) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "INSERT INTO mail_sent(message_id, context_id, in_reply_to, session_uid, recipient,
                                   recipient_computer, text, created_ms, state, told_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'sent', 'sent')",
            params![
                mail.message_id,
                mail.context_id,
                mail.in_reply_to,
                mail.session_uid,
                mail.recipient,
                mail.recipient_computer,
                mail.text,
                now_ms
            ],
        )?;
        Ok(())
    }

    /// **Where a sent message stands now**, from the receiver. A state only moves
    /// forward: an answer that arrives after a later one changes nothing. `told`
    /// says the sender was just told this state itself.
    pub fn note_sent_mail(
        &self,
        message_id: &str,
        state: protocol::ipc::MailState,
        reason: Option<&str>,
        told: bool,
    ) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE mail_sent
                SET state = ?2, reason = ?3, told_state = CASE WHEN ?4 THEN ?2 ELSE told_state END
              WHERE message_id = ?1 AND state IN ('sent', 'waiting') AND state <> ?2",
            params![message_id, state.as_str(), reason, told],
        )?;
        Ok(())
    }

    /// The computer a sent message went to, empty for this one, or `None` when
    /// this computer sent no such message.
    pub fn sent_mail_computer(&self, message_id: &str) -> Result<Option<String>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                "SELECT recipient_computer FROM mail_sent WHERE message_id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Sent messages no receiver has taken yet, here or on another computer, oldest
    /// first.
    pub fn unsent_mail(&self) -> Result<Vec<SentMail>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT message_id, context_id, in_reply_to, session_uid, recipient,
                    recipient_computer, text
               FROM mail_sent WHERE state = 'sent' ORDER BY created_ms ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SentMail {
                message_id: row.get(0)?,
                context_id: row.get(1)?,
                in_reply_to: row.get(2)?,
                session_uid: row.get(3)?,
                recipient: row.get(4)?,
                recipient_computer: row.get(5)?,
                text: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// State changes the senders on other computers have not been told yet.
    pub fn untold_mail(&self) -> Result<Vec<UntoldMail>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT message_id, sender_computer, state FROM mail WHERE sender_told = 0
              ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(UntoldMail {
                message_id: row.get(0)?,
                sender_computer: row.get(1)?,
                state: mail_state(&row.get::<_, String>(2)?),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The sender of `message_id` was told it stands at `state`.
    pub fn told_mail(&self, message_id: &str, state: protocol::ipc::MailState) -> Result<()> {
        let conn = self.write();
        conn.execute(
            "UPDATE mail SET sender_told = 1 WHERE message_id = ?1 AND state = ?2",
            params![message_id, state.as_str()],
        )?;
        Ok(())
    }

    // ---------------------------------------------------- generalized ledger    // ---------------------------------------------------- generalized ledger
    //
    // The generalized mutation ledger, keyed `(operation_kind, session_uid,
    // client_request_id)`. It is the same claim-before-write, lookup-then-
    // conflict primitive as `claim_text_mutation` above,
    // widened so any Codex mutation (answer, compose, interrupt) shares one
    // idempotency law. A phone answer is its first producer — it claims here
    // under `operation_kind = "answer"` — so the code is live and carries no
    // `cfg_attr(not(test), allow(dead_code))`.

    /// Take durable ownership of one mutation, or find out who already has.
    ///
    /// `claimed_hash` is the immutable claimed material — a hash over the full
    /// authorization surface (route, target turn, the exact displayed option
    /// set, cwd). A retry under the same key with a **different** hash is a
    /// [`MutationClaim::Conflict`]: an id is a retry key, never a licence to
    /// actuate something else. Read and insert share one immediate transaction,
    /// so two deliveries replaying one id cannot both come back `Claimed`.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_mutation(
        &self,
        operation_kind: &str,
        session_uid: &str,
        client_request_id: &str,
        claimed: &ClaimedMaterial,
        now: &str,
    ) -> Result<MutationClaim> {
        let question_decision = claimed
            .question_decision
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, Option<String>, String, ClaimedMaterial)> = tx
            .query_row(
                "SELECT claimed_hash, status, outcome, started_at,
                        thread_id, generation, route, target_turn_id, question_decision
                   FROM mutation_ledger
                  WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3",
                params![operation_kind, session_uid, client_request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        ClaimedMaterial {
                            thread_id: row.get(4)?,
                            generation: row.get::<_, i64>(5)? as u64,
                            route: row.get(6)?,
                            target_turn_id: row.get(7)?,
                            claimed_hash: row.get(0)?,
                            question_decision: question_decision_column(row, 8)?,
                        },
                    ))
                },
            )
            .optional()?;

        let claim = match existing {
            None => {
                let inserted = tx.execute(
                    // Same rule as `answers`/`text_mutations`: no new claim for a
                    // session deleted while the request was in flight. The claimed
                    // material is written once, here, and never updated.
                    "INSERT INTO mutation_ledger(operation_kind, session_uid, client_request_id,
                                                 claimed_hash, thread_id, generation, route,
                                                 target_turn_id, status, outcome, started_at,
                                                 settled_at, question_decision)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'applying', NULL, ?9, NULL, ?10
                      WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?2)",
                    params![
                        operation_kind,
                        session_uid,
                        client_request_id,
                        claimed.claimed_hash,
                        claimed.thread_id,
                        claimed.generation as i64,
                        claimed.route,
                        claimed.target_turn_id,
                        now,
                        question_decision,
                    ],
                )?;
                if inserted == 1 {
                    MutationClaim::Claimed
                } else {
                    MutationClaim::NoSession
                }
            }
            // Same key, different material: two different mutations, one id.
            // Neither is actuated on a guess. The comparison is over the
            // **explicit immutable fields** (thread_id, generation, route,
            // target_turn_id) *and* the hash — belt and suspenders, so a changed
            // route or target that somehow shared a hash is still a conflict, not
            // a duplicate. `ClaimedMaterial`'s `Eq` covers every field including
            // the hash.
            Some((_, _, _, _, material)) if material != *claimed => MutationClaim::Conflict,
            Some((_, status, outcome, started_at, material)) => match status.as_str() {
                "done" => MutationClaim::Applied {
                    outcome: outcome.unwrap_or_default(),
                    claimed: material,
                },
                _ => MutationClaim::Indeterminate {
                    started_at,
                    claimed: material,
                },
            },
        };
        tx.commit()?;
        Ok(claim)
    }

    /// Record a mutation's terminal outcome, so a later retry replays it verbatim
    /// rather than actuating again.
    ///
    /// **First-terminal-wins over *any* terminal state.** Settlement is allowed
    /// only from the non-terminal `'applying'` state, so it can never overwrite a
    /// claim that already reached a terminal outcome — `'done'` **or**
    /// `'indeterminate'` (which recovery writes for a claim it could not prove
    /// landed). A second settle of a terminal claim writes nothing and reports it
    /// did not win. Returns whether this call was the one that settled it.
    pub fn settle_mutation(
        &self,
        operation_kind: &str,
        session_uid: &str,
        client_request_id: &str,
        outcome: &str,
        settled_at: &str,
    ) -> Result<bool> {
        let conn = self.write();
        let updated = conn.execute(
            "UPDATE mutation_ledger SET status = 'done', outcome = ?4, settled_at = ?5
              WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3
                AND status = 'applying'",
            params![
                operation_kind,
                session_uid,
                client_request_id,
                outcome,
                settled_at
            ],
        )?;
        Ok(updated == 1)
    }

    /// [`Store::settle_mutation`] with `event` filed in the same transaction, only by the
    /// settle that wins, so the outcome and the fact it is told by cannot land apart. The
    /// events newly stored: none when the claim was already terminal.
    pub fn settle_mutation_with_event(
        &self,
        operation_kind: &str,
        session_uid: &str,
        client_request_id: &str,
        outcome: &str,
        settled_at: &str,
        event: &PendingEvent,
    ) -> Result<Vec<Event>> {
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            "UPDATE mutation_ledger SET status = 'done', outcome = ?4, settled_at = ?5
              WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3
                AND status = 'applying'",
            params![
                operation_kind,
                session_uid,
                client_request_id,
                outcome,
                settled_at
            ],
        )?;
        let mut filed = Vec::new();
        if updated == 1 {
            filed.extend(append_in_tx(&tx, event)?);
        }
        tx.commit()?;
        Ok(filed)
    }

    /// **Give back a claim whose write was refused before it did anything**, so the same
    /// id is a first attempt again: [`Store::release_text_mutation`]'s rule for the
    /// generalized ledger. Only a claim still `applying` is removed; one already terminal
    /// — `indeterminate` above all, which forbids a resend — is left as it is. Returns
    /// whether this call removed it.
    pub fn release_mutation(
        &self,
        operation_kind: &str,
        session_uid: &str,
        client_request_id: &str,
    ) -> Result<bool> {
        let conn = self.write();
        let deleted = conn.execute(
            "DELETE FROM mutation_ledger
              WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3
                AND status = 'applying'",
            params![operation_kind, session_uid, client_request_id],
        )?;
        Ok(deleted == 1)
    }

    /// **Make one claim terminal without being able to say what it did.**
    ///
    /// Recovery's word for a claim whose write may or may not have reached the
    /// socket. The same first-terminal-wins guard as [`Store::settle_mutation`],
    /// and reached only for a claim whose card is already gone — a claim that
    /// still has a card is settled inside
    /// [`Store::retire_codex_pending_approval`]'s transaction instead, so the two
    /// halves of one terminal cannot land apart.
    pub fn settle_mutation_indeterminate(
        &self,
        operation_kind: &str,
        session_uid: &str,
        client_request_id: &str,
        settled_at: &str,
    ) -> Result<bool> {
        let conn = self.write();
        let updated = conn.execute(
            "UPDATE mutation_ledger SET status = 'indeterminate', settled_at = ?4
              WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3
                AND status = 'applying'",
            params![operation_kind, session_uid, client_request_id, settled_at],
        )?;
        Ok(updated == 1)
    }

    /// **One answer claim's durable status.**
    ///
    /// Read by the answer path before it asks the link, so a card whose claim is
    /// already terminal is refused with the reason rather than with whatever the
    /// link happens to say. A terminal claim outlives its card in two shapes that
    /// both leave the question standing: a lost race, and a request the app-server
    /// re-delivered after a bounce.
    pub fn answer_status(
        &self,
        session_uid: &str,
        client_request_id: &str,
    ) -> Result<Option<AnswerStatus>> {
        Ok(self
            .mutation_status(OPERATION_ANSWER, session_uid, client_request_id)?
            .map(|state| state.status))
    }

    /// **Where one claim of any kind stands, durably.**
    ///
    /// The general form of [`Store::answer_status`], read by every path that must
    /// refuse a mutation whose claim is already terminal — and refuse it with the
    /// reason the record gives rather than with whatever the link happens to say.
    /// **The claimed material comes back with the status**, because a caller that
    /// replays a terminal has to apply the ledger's own law: a duplicate is a second
    /// ask carrying the SAME material, and an id reused for different material is two
    /// mutations under one key. A reader given only the status cannot tell them apart
    /// and answers the second as though it were the first.
    pub fn mutation_status(
        &self,
        operation_kind: &str,
        session_uid: &str,
        client_request_id: &str,
    ) -> Result<Option<MutationState>> {
        let row = self
            .read()
            .query_row(
                "SELECT status, outcome, claimed_hash, thread_id, generation, route,
                        target_turn_id, question_decision
                   FROM mutation_ledger
                  WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3",
                params![operation_kind, session_uid, client_request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        ClaimedMaterial {
                            thread_id: row.get(3)?,
                            generation: row.get::<_, i64>(4)? as u64,
                            route: row.get(5)?,
                            target_turn_id: row.get(6)?,
                            claimed_hash: row.get(2)?,
                            question_decision: question_decision_column(row, 7)?,
                        },
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(status, outcome, claimed)| MutationState {
            status: match status.as_str() {
                "done" => AnswerStatus::Settled(outcome.unwrap_or_default()),
                "applying" => AnswerStatus::Applying,
                _ => AnswerStatus::Indeterminate,
            },
            claimed,
        }))
    }

    /// **Every mutation of one kind this daemon was in the middle of when it
    /// stopped.**
    ///
    /// The claimed material comes back with the rows, so a recovered terminal can
    /// name what was attempted rather than only the key it was attempted under —
    /// which for an answer is the decision and for an interrupt is the turn.
    ///
    /// Taking the kind as an argument rather than having one function per operation
    /// is what keeps the two recoveries reading the same rows by the same rule: the
    /// ledger is one table with one law, and a second copy of this query would be a
    /// second chance for the law to drift.
    pub fn unsettled_claims(&self, operation_kind: &str) -> Result<Vec<MutationClaimRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, client_request_id, claimed_hash, thread_id, generation,
                    route, target_turn_id, started_at, question_decision
               FROM mutation_ledger
              WHERE operation_kind = ?1 AND status = 'applying'
              ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map(params![operation_kind], |row| {
            Ok(MutationClaimRow {
                session_uid: row.get(0)?,
                client_request_id: row.get(1)?,
                claimed: ClaimedMaterial {
                    thread_id: row.get(3)?,
                    generation: row.get::<_, i64>(4)? as u64,
                    route: row.get(5)?,
                    target_turn_id: row.get(6)?,
                    claimed_hash: row.get(2)?,
                    question_decision: question_decision_column(row, 8)?,
                },
                started_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// **The applying claims of one kind that belong to one session.**
    ///
    /// The scoped twin of [`Store::unsettled_claims`], for settling the claims of a
    /// single outgoing session at an in-process abort — a handover, a registration, a
    /// disconnect — rather than the whole store at a restart. The claim row is the
    /// authoritative record of a mutation in flight —
    /// it is committed before the mutation enters any in-memory ledger, and the
    /// write that commits it cannot be cancelled — so a handover that has to abandon
    /// a session reads its claims from here and makes each terminal, catching one
    /// that was committed but never reached the daemon's own ledger.
    pub fn unsettled_claims_for(
        &self,
        operation_kind: &str,
        session_uid: &str,
    ) -> Result<Vec<MutationClaimRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, client_request_id, claimed_hash, thread_id, generation,
                    route, target_turn_id, started_at, question_decision
               FROM mutation_ledger
              WHERE operation_kind = ?1 AND session_uid = ?2 AND status = 'applying'
              ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map(params![operation_kind, session_uid], |row| {
            Ok(MutationClaimRow {
                session_uid: row.get(0)?,
                client_request_id: row.get(1)?,
                claimed: ClaimedMaterial {
                    thread_id: row.get(3)?,
                    generation: row.get::<_, i64>(4)? as u64,
                    route: row.get(5)?,
                    target_turn_id: row.get(6)?,
                    claimed_hash: row.get(2)?,
                    question_decision: question_decision_column(row, 8)?,
                },
                started_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

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
                 accounts on this computer",
                sidecar.display()
            );
        }
    }
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
///
/// **Rolling back across the retirement of the session tables' tmux-era columns is
/// one-way:** an earlier build's start-up names the columns [`Store::migrate`] drops,
/// and fails at its first statement that does, so it cannot use this database.
///
/// ## Why sessions live in two tables
///
/// `sessions` holds Claude runs. `codex_sessions` holds Codex runs, in exactly
/// the same columns. That looks like the duplication the `agent` column was
/// added to avoid, and it is deliberate, because the thing it has to survive is
/// not a query this build writes — it is a **binary this build cannot change**.
///
/// Measured against the real v0.6.0 daemon, with a Codex row sitting in
/// `sessions`:
///
///   * its liveness sweep enumerated the row through an agent-agnostic
///     positional `SELECT`, looked for the tmux session on its own socket,
///     found nothing there — it never can, a Codex session is not on it — and
///     marked a live run `exited`;
///   * `codeconnect sessions prune` then enumerated that same row and **deleted
///     it and all four of its events**.
///
/// Neither is incidental. Both follow from v0.6.0 reading `sessions` by
/// position and knowing nothing about agents, and both are total data loss for
/// the Codex half of the fleet after a rollback.
///
/// Two smaller shapes were tried and refuted by measurement, and are recorded
/// here so they are not re-proposed:
///
///   * **A `user_version` fence.** v0.6.0 reads `user_version` and then ignores
///     it: its `migrate()` writes `3` back unconditionally. A released binary
///     cannot be taught to refuse a schema it does not know.
///   * **A view named `sessions` shadowing a renamed table.** `CREATE TABLE IF
///     NOT EXISTS sessions` against a view is a silent no-op, so v0.6.0 would
///     open — but `UPDATE sessions …` fails with "cannot modify sessions
///     because it is a view", and v0.6.0 must `INSERT`, `UPDATE` and `DELETE`
///     it. `INSTEAD OF` triggers for all three would then have to reproduce
///     `changes()` exactly, because v0.6.0's prune asserts it removed one row.
///     That puts the old daemon's *Claude* path at risk to protect the Codex
///     one, which is the trade this gate forbids.
///
/// What is left is the table name. v0.6.0 contains no statement that names
/// `codex_sessions`, so every one of its sessions projections — enumerate,
/// update, delete, prune — is structurally unable to reach a Codex run. Not
/// "unlikely to": there is no code path.
///
/// The children v0.6.0 reaches **only** by a uid it walked from a `sessions`
/// row — `events`, `tail_cursors`, `mutation_ledger` — stay shared, because
/// after the move there is no such row for a Codex run to be walked from.
///
/// `answers` is **not** one of them and is deliberately not listed here: v0.6.0
/// queries it by `request_id`, with no `sessions` row anywhere in the path. It
/// belongs to the four-table obligation below, and the census there is the one
/// that governs.
///
/// ### The one residual, measured rather than assumed
///
/// v0.6.0's orphan count is defined over `sessions`, so after a rollback it
/// reports every Codex event as belonging to "runs with no session row". Its
/// own prune output, verbatim against the real binary: *"note: 2 event(s)
/// belong to runs with no session row and are reachable by nothing here; they
/// were left untouched"*. That is a cosmetic over-count in one line of one
/// report, and it is safe for the reason the note itself gives — that path
/// counts and never deletes, deliberately, because data whose provenance is not
/// understood is exactly what an automatic cleanup must not touch. This build's
/// [`Store::orphan_event_count`] reads `all_sessions` and does not over-count.
///
/// ### The obligation this leaves open
///
/// Four tables — `pending_approvals`, `answer_claims`, `text_mutations` and
/// `answers` — are reached by v0.6.0 **globally**, without going through a
/// `sessions` row at all: it reads the first three by no key but their own, it
/// queries `answers` by `request_id`, and its `recover_text_mutations` *writes*
/// `text_mutations`, turning every `applying` row into `indeterminate` whoever
/// it belongs to. Its recovery can delete `answer_claims` rows and the pending
/// cards that match them. The table-name isolation the rest of this schema
/// rests on does not cover any of that.
///
/// They are still not split, and the reason has changed shape since it was
/// first written down. It was "no producer exists". That was **wrong**: daemon
/// paths reach these tables agent-agnostically — `send_text`, which claims
/// `text_mutations`, and `answer`, which claims `answer_claims` before every one
/// of its refusals. Both would have written for a Codex run.
///
/// So the real reason is a decision: **each of those producers refuses
/// a non-Claude session outright, before any durable write.** That is
/// scaffolding, deliberately — it makes Codex text mutations and Codex claims in
/// these tables impossible rather than merely unbuilt, and it is what splitting these tables
/// per agent removes. It is cheap and reversible where splitting four tables now
/// would be machinery built ahead of the wire that will shape it. The refusals live in
/// `Daemon::send_text` and `Daemon::answer`;
/// `send_text_to_a_codex_session_is_refused_before_it_claims_anything` and
/// `answering_a_codex_card_is_refused_before_the_claim` drive those real paths
/// against a Codex row and read all four tables back off the daemon's own
/// database file, and
/// `no_codex_row_reaches_a_table_a_rolled_back_daemon_sweeps_globally` holds the
/// floor underneath them.
///
/// **That day came, and it did not go the way this paragraph predicted.** What
/// stood here was: they all name `AgentKind::Codex` outright, so the day Codex
/// joins `Daemon::supported_agents` and the refusals stop firing, they go red —
/// which is the day the split stops being speculative. Codex joined that list
/// when its coordinator became able to register a session, and the three tests
/// stayed green, because the refusals did not stop firing. `shared_ledgers_admit`
/// was rewritten in the same breath to ask `AgentKind::is_claude` instead of
/// asking the supported list, so the refusal moved off the list rather than
/// lifting with it — see that method for the decision and why it was taken.
///
/// The prediction was wrong in its mechanism and right in its substance: a Codex
/// run really can be hosted now, and these four tables really are still closed to
/// it. The three tests were repointed rather than deleted — they now put their
/// refusals in front of a Codex run begun as the engine begins one instead of one
/// staged into the store, which is a stronger claim than the one they were making — and they
/// are what a per-agent split has to change on purpose. The day the split stops
/// being speculative is therefore the day somebody edits those refusals, not a
/// day a test goes red on its own.
fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions(
            -- The run's identity: a ULID minted when the run begins. Never reused.
            session_uid       TEXT PRIMARY KEY,
            -- The run's name. NOT unique: a name belongs to whichever run
            -- currently holds it, and the ones before it keep their rows.
            session_id        TEXT NOT NULL,
            cwd               TEXT NOT NULL,
            claude_session_id TEXT,
            transcript_path   TEXT,
            lifecycle         TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL,
            -- The agent hosting this run. NOT NULL with a 'claude' default so a
            -- legacy row a migration backfills, and every row written before the
            -- agent seam, reads as Claude — which is exactly what it is. Decoded
            -- through `AgentKind::from_str_lossy`, so an unrecognised value fails
            -- closed rather than posing as Claude.
            --
            -- Every row in THIS table is 'claude': `begin_run` routes by this
            -- value, so nothing else lands here. Decoded through
            -- `AgentKind::from_str_lossy`, so an unrecognised value fails closed
            -- rather than posing as Claude.
            agent             TEXT NOT NULL DEFAULT 'claude',
            -- Codex thread identity. NULL for Claude and for any row predating
            -- the seam. Carried on this table too, so the two tables have one
            -- shape.
            codex_thread_id   TEXT
        );

        -- Resolving a name to the newest run under it.
        CREATE INDEX IF NOT EXISTS sessions_name ON sessions(session_id);

        -- Codex runs. The same ten columns in the same order as `sessions`
        -- — see the note on `create_schema` for why this is a second table and
        -- not a `WHERE agent = 'codex'`. `both_session_tables_have_one_shape`
        -- reads both back off the schema, so a column added to one and not the
        -- other fails a test instead of failing a query.
        CREATE TABLE IF NOT EXISTS codex_sessions(
            session_uid       TEXT PRIMARY KEY,
            session_id        TEXT NOT NULL,
            cwd               TEXT NOT NULL,
            claude_session_id TEXT,
            transcript_path   TEXT,
            lifecycle         TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL,
            -- Declared identically to `sessions`, `DEFAULT 'claude'` and all, so
            -- the two declarations can be read side by side and seen to be the
            -- same. Every insert here names `agent` explicitly, so the default is
            -- never reached.
            agent             TEXT NOT NULL DEFAULT 'claude',
            codex_thread_id   TEXT
        );

        -- The twin of `sessions_name`: without it, resolving a name would
        -- index-seek Claude's half and full-scan Codex's.
        CREATE INDEX IF NOT EXISTS codex_sessions_name ON codex_sessions(session_id);

        -- **The shadow is refused, not repaired afterwards.**
        --
        -- A rolled-back v0.6.0 daemon has no `agent` column of its own: its
        -- `upsert_session` names ten columns, so the eleventh takes the
        -- `DEFAULT 'claude'` above and a reconnecting Codex run is re-filed into
        -- the shared table beside its real row in `codex_sessions`. One uid, two
        -- tables — and from there its liveness sweep ends the shadow, its prune
        -- deletes the events keyed by the uid underneath it, and it records a
        -- tombstone the surviving isolated row contradicts.
        --
        -- Repairing that *after the fact* needs a timing argument, and there is
        -- no sound one: `BEGIN IMMEDIATE` defers a v0.6.0 statement rather than
        -- neutralising it, so a re-ask after the commit has to guess how long
        -- the old daemon's busy handler will sleep — and on a database with
        -- nothing to repair there is no evidence to know the re-ask is even
        -- needed. This trigger closes it at the schema layer instead, where no
        -- scheduling argument exists to be wrong: the shadow is never written.
        --
        -- **It fires for v0.6.0 because it lives in the schema, not in a
        -- binary.** A rollback swaps the daemon; it does not drop the trigger.
        -- Measured against v0.6.0's verbatim ten-column upsert — the
        -- `INSERT … SELECT … WHERE NOT EXISTS … ON CONFLICT DO UPDATE` shape —
        -- on a uid already in `codex_sessions`: `SQLITE_CONSTRAINT`, zero rows
        -- written, on the fresh-insert path and on the conflict path alike.
        -- `NEW.agent` carries the column default there, which is what makes the
        -- `WHEN` clause able to see the old daemon at all.
        --
        -- **What it costs the hot path, measured rather than waved at.** Every
        -- insert into `sessions` pays one indexed seek into `codex_sessions`.
        -- Over 3,000 committed Claude upserts against 200 Codex rows: 23.3us to
        -- 42.1us each. That is a real 1.8x on the statement and it is nothing on
        -- the daemon — a run is inserted once, when it begins. The `EXISTS` seeks
        -- the `codex_sessions` primary key; there is no cheaper shape that still
        -- answers the question.
        --
        -- **And it does not catch this build's own writes**: `begin_run` files a
        -- Codex run in `codex_sessions` and never in `sessions`.
        CREATE TRIGGER IF NOT EXISTS sessions_refuse_codex_shadow
        BEFORE INSERT ON sessions
        WHEN NEW.agent = 'claude'
         AND EXISTS(SELECT 1 FROM codex_sessions AS c
                     WHERE c.session_uid = NEW.session_uid)
        BEGIN
            SELECT RAISE(ABORT, 'this session_uid is a Codex run and lives in codex_sessions; \
refusing to file a second copy of it in the shared sessions table');
        END;

        -- The whole fleet, for the reads that must see all of it.
        --
        -- Splitting the write side does not mean splitting the read side: this
        -- daemon owns both agents and every projection it shows a human — the
        -- listing, the resolver, the prune's candidate sweep — has to answer for
        -- both. So the reads changed their FROM clause and nothing else.
        --
        -- A view, sitting *beside* the real `sessions` table rather than
        -- shadowing it.
        --
        -- `UNION ALL`, not `UNION`: a uid lives in exactly one of the two tables
        -- (`begin_run` files a run in one), so there is nothing to deduplicate and
        -- `UNION` would only buy a sort.
        --
        -- Measured, not assumed, on the owner's real store: SQLite pushes the
        -- predicate into both branches rather than materialising the union
        -- first. `WHERE session_uid = ?` becomes two covering-index seeks,
        -- `WHERE session_id = ?` two seeks on `sessions_name` /
        -- `codex_sessions_name`, and the prune's `WHERE lifecycle = ?` two
        -- scans — which is exactly what it was against the one table, because
        -- there is no index on `lifecycle` and never was.
        CREATE VIEW IF NOT EXISTS all_sessions AS
            SELECT session_uid, session_id, cwd,
                   claude_session_id, transcript_path, lifecycle,
                   created_at, updated_at, agent, codex_thread_id
              FROM sessions
            UNION ALL
            SELECT session_uid, session_id, cwd,
                   claude_session_id, transcript_path, lifecycle,
                   created_at, updated_at, agent, codex_thread_id
              FROM codex_sessions;

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

        -- The same cards for Codex runs, in a table a rolled-back v0.6.0 daemon
        -- has never heard of.
        --
        -- `pending_approvals` above is one of the four tables the old daemon
        -- reads GLOBALLY rather than by walking a session row, so a Codex card
        -- sitting there is a card its recovery can enumerate and delete. That
        -- was a bound obligation while nothing produced one; the Codex approval
        -- observer is the producer, so the split lands with it.
        --
        -- **The six shared columns keep their names, types and their meaning**,
        -- because everything above the store is reused verbatim: the same
        -- `ApprovalCard` in `card`, the same generation, the same
        -- `(session_uid, request_id)` primary key the in-memory `pending` map
        -- and the answer path are already keyed by. Only the storage moved.
        --
        -- **The four added columns are the Codex identity, and `request_id` is
        -- not part of it.** A Codex `serverRequest` id is a per-CONNECTION
        -- integer from zero, shared across families: measured on 0.153, the
        -- very same approval re-delivered to a reconnecting link arrives as
        -- `id: 0` again, and two unrelated approvals on two connections would
        -- both call themselves zero. What is stable is `itemId` — a uuid,
        -- measured byte-identical across the re-delivery — so identity is
        -- `(thread_id, item_id)` and `request_id` is derived from it rather
        -- than read off the wire. That is what makes a daemon bounce rebind
        -- one card instead of minting a second.
        --
        -- `turn_id` is a column rather than a detail inside `card` because
        -- retirement queries it: a `turn/completed{interrupted}` clears every
        -- card bound to that turn, and reading it back out of serialised JSON
        -- to do so would be a second parser of the same field.
        CREATE TABLE IF NOT EXISTS codex_pending_approvals(
            session_uid TEXT    NOT NULL,
            session_id  TEXT    NOT NULL,
            request_id  TEXT    NOT NULL,
            card        TEXT    NOT NULL,
            generation  INTEGER NOT NULL,
            created_ms  INTEGER NOT NULL,
            -- The Codex identity. `family` is the request method's family
            -- (`commandExecution` | `fileChange`); the observe-only families
            -- never reach this table because no card is built for them.
            thread_id   TEXT    NOT NULL,
            turn_id     TEXT    NOT NULL,
            item_id     TEXT    NOT NULL,
            family      TEXT    NOT NULL,
            PRIMARY KEY(session_uid, request_id)
        );

        -- Retirement reads by turn (a turn terminal clears its cards) and the
        -- dedup that makes a rebind idempotent reads by item.
        CREATE INDEX IF NOT EXISTS codex_pending_approvals_turn
            ON codex_pending_approvals(session_uid, turn_id);
        -- The measured identity, enforced rather than merely intended: two rows
        -- for one wire item is the "second card" this whole split is here to
        -- make impossible.
        CREATE UNIQUE INDEX IF NOT EXISTS codex_pending_approvals_item
            ON codex_pending_approvals(session_uid, thread_id, item_id);

        -- Both agents' open cards, for the reads that answer for the fleet.
        --
        -- The same shape as `all_sessions` and for the same reason: splitting
        -- the write side does not split the read side. Every projection this
        -- daemon shows a human — the pending list, recovery — has to answer for
        -- both agents, so those reads changed their FROM clause and nothing
        -- else. Named something v0.6.0 has never heard of, and sitting BESIDE
        -- `pending_approvals` rather than shadowing it, so the old daemon still
        -- finds a table it can INSERT and DELETE.
        --
        -- The four Codex columns are not projected: a reader of this view is by
        -- definition agent-agnostic, and a NULL-padded identity would invite
        -- exactly the agent-specific branch the view exists to avoid. The
        -- Codex-only reads go to the physical table.
        CREATE VIEW IF NOT EXISTS all_pending_approvals AS
            SELECT session_uid, session_id, request_id, card, generation, created_ms
              FROM pending_approvals
            UNION ALL
            SELECT session_uid, session_id, request_id, card, generation, created_ms
              FROM codex_pending_approvals;

        -- A Codex card in the shared table is refused, not repaired afterwards.
        --
        -- The mirror of `sessions_refuse_codex_shadow`, and it earns its place
        -- the same way: it lives in the SCHEMA, so a rollback that swaps the
        -- daemon does not drop it, and it goes on refusing while v0.6.0 is the
        -- one running. `pending_approvals` has no `agent` column to read, so
        -- the `WHEN` clause asks the question the uid can answer — is this uid
        -- a Codex run? — at the cost of one indexed primary-key seek per
        -- approval insert, the same shape and the same cost the session
        -- trigger's note measures.
        --
        -- It does not stand in front of this build's own writes: the Codex
        -- observer inserts into `codex_pending_approvals`, which this trigger
        -- does not watch.
        CREATE TRIGGER IF NOT EXISTS pending_approvals_refuse_codex_card
        BEFORE INSERT ON pending_approvals
        WHEN EXISTS(SELECT 1 FROM codex_sessions AS c
                     WHERE c.session_uid = NEW.session_uid)
        BEGIN
            SELECT RAISE(ABORT, 'this session_uid is a Codex run and its cards live in \
codex_pending_approvals; refusing to file one in the shared pending_approvals table');
        END;

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

        -- Messages from other agents for a session on this computer (the receiver's
        -- side), one per sender's `message_id`, in the order they were taken (`seq`).
        -- `offered` while a `read_messages` answer is on its way to the agent; any
        -- found at start-up are `waiting` again. `sender_told` is 0 while a sender on
        -- another computer has not been told the state.
        CREATE TABLE IF NOT EXISTS mail(
            seq             INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id      TEXT NOT NULL UNIQUE,
            context_id      TEXT NOT NULL,
            in_reply_to     TEXT,
            session_uid     TEXT NOT NULL,
            sender_computer TEXT NOT NULL,
            sender_uid      TEXT NOT NULL,
            sender          TEXT NOT NULL,
            sender_label    TEXT NOT NULL,
            sender_agent    TEXT NOT NULL,
            sender_project  TEXT NOT NULL,
            text            TEXT NOT NULL,
            accepted_ms     INTEGER NOT NULL,
            expires_ms      INTEGER NOT NULL,
            -- waiting | offered | read | expired | ended
            state           TEXT NOT NULL,
            offer           TEXT,
            sender_told     INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS mail_by_session ON mail(session_uid, state, seq);

        -- The protocol minor each run that takes mail was launched with (this
        -- build's), kept so a restarted daemon knows which runs take mail.
        CREATE TABLE IF NOT EXISTS mail_runs(
            session_uid TEXT PRIMARY KEY,
            minor       INTEGER NOT NULL
        );

        -- Where waking each run for its mail stands (`crate::mail`'s wake rule): a
        -- wake issued whose turn has not ended, the turn ends since with mail still
        -- unread, wakes stopped until a human turn or new mail, and the newest start
        -- and end of a Claude turn (a turn runs while the start is the later).
        CREATE TABLE IF NOT EXISTS mail_wake(
            session_uid TEXT PRIMARY KEY,
            outstanding INTEGER NOT NULL DEFAULT 0,
            misses      INTEGER NOT NULL DEFAULT 0,
            degraded    INTEGER NOT NULL DEFAULT 0,
            started_ms  INTEGER NOT NULL DEFAULT 0,
            ended_ms    INTEGER NOT NULL DEFAULT 0
        );

        -- Messages this computer's agents sent (the sender's side), written before
        -- they go anywhere. `told_state` is the state the sender last read; an offer
        -- of the change in flight is `offer` / `offer_state`.
        CREATE TABLE IF NOT EXISTS mail_sent(
            message_id         TEXT PRIMARY KEY,
            context_id         TEXT NOT NULL,
            in_reply_to        TEXT,
            session_uid        TEXT NOT NULL,
            recipient          TEXT NOT NULL,
            recipient_computer TEXT NOT NULL,
            text               TEXT NOT NULL,
            created_ms         INTEGER NOT NULL,
            -- sent | waiting | read | refused | expired | ended
            state              TEXT NOT NULL,
            reason             TEXT,
            told_state         TEXT NOT NULL,
            offer              TEXT,
            offer_state        TEXT
        );

        -- The generalized mutation ledger. `text_mutations` above is the proven
        -- shape for one operation (send_text); this is the same lookup-then-
        -- conflict pattern widened to any Codex mutation (answer, compose,
        -- interrupt) by keying on `operation_kind` as well. A retry is recognised
        -- by `(operation_kind, session_uid, client_request_id)` and replays its
        -- recorded outcome; a retry that reuses the id with *different* claimed
        -- material is a conflict, never a second actuation. The claimed material
        -- is a single hash over the full authorization surface (route, target
        -- turn, the exact displayed option set, cwd) and is immutable once
        -- written. Additive: a Codex run's asks and the Stops of a Claude run
        -- CodeConnect started are written here, so a rolled-back daemon that does not
        -- know the table neither reads nor mutates it.
        CREATE TABLE IF NOT EXISTS mutation_ledger(
            operation_kind    TEXT NOT NULL,
            session_uid       TEXT NOT NULL,
            client_request_id TEXT NOT NULL,
            -- **Immutable claimed material.** Written once at claim and never
            -- updated: on a retry the ledger replays *these*, so a compose that
            -- was a turn/start is re-issued as a turn/start even if current state
            -- would now route it as a steer. The hash covers the full
            -- authorization surface; the columns are the material needed to
            -- actually replay the original route.
            claimed_hash      TEXT NOT NULL,
            thread_id         TEXT NOT NULL,
            generation        INTEGER NOT NULL,
            -- The snapshotted route, in the operation kind's own vocabulary:
            -- 'turn_start' | 'turn_steer' for a compose, and the chosen option's
            -- id for an answer.
            route             TEXT NOT NULL,
            target_turn_id    TEXT,
            -- applying | done | indeterminate
            status            TEXT NOT NULL,
            -- The recorded terminal outcome, replayed verbatim on a duplicate.
            outcome           TEXT,
            started_at        TEXT NOT NULL,
            settled_at        TEXT,
            -- An answer to a Codex question card, kept whole: the app's `Answers`
            -- or `Decline` as JSON. NULL for every other claim.
            question_decision TEXT,
            PRIMARY KEY(operation_kind, session_uid, client_request_id)
        );

        -- A run ccd hosts, of either agent: the app's start request it answers,
        -- its host's kernel identity (null until the host has written host.json),
        -- the highest host message whose effects are committed, and the SDK's
        -- initialize answer from the start's outcome (JSON, null until a start
        -- is done): the run's facts before its first turn's init. `commands` is
        -- claude's command list as it last reported it (JSON: the answer's, then
        -- each `commands_changed`'s), taken at `commands_at`; `screen_only` the
        -- names its latest init offers on claude's own screen only (JSON, null
        -- until the first init). The row is
        -- written with the run's own session row, before anything is spawned, so
        -- a repeated request finds the run instead of starting another. `start`
        -- is the run's `start` line, as ccd sends it, kept so a restarted ccd can
        -- send it again to a host it never reached.
        CREATE TABLE IF NOT EXISTS run_hosts(
            session_uid     TEXT PRIMARY KEY,
            request_id      TEXT    NOT NULL UNIQUE,
            host_pid        INTEGER,
            host_start_sec  INTEGER,
            host_start_usec INTEGER,
            cursor          INTEGER NOT NULL DEFAULT 0,
            initialize      TEXT,
            boot_sec        INTEGER,
            boot_usec       INTEGER,
            start           TEXT,
            commands        TEXT,
            commands_at     TEXT,
            screen_only     TEXT,
            account_id      TEXT,
            switching       INTEGER,
            launch_account  TEXT
        );

        -- An agent's accounts on this computer: each a folder of ccd's own,
        -- `<root>/accounts/<agent>/<id>/`, which a start on it is given (Claude
        -- Code's sign-in store, Codex's whole home). An agent's Default, its
        -- own setup as it is, has no row. `account_selection` holds the one
        -- account each agent's starts use now; no row is its Default.
        CREATE TABLE IF NOT EXISTS accounts(
            id         TEXT PRIMARY KEY,
            agent      TEXT NOT NULL,
            label      TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS account_selection(
            agent      TEXT PRIMARY KEY,
            account_id TEXT NOT NULL
        );

        -- Every folder a session has started in, with its newest start: the
        -- phone's project list. Its own table because session rows go when a
        -- run is removed or pruned, and the folder must not go with them.
        CREATE TABLE IF NOT EXISTS projects(
            path       TEXT PRIMARY KEY,
            last_used  TEXT NOT NULL,
            last_agent TEXT NOT NULL
        );

        -- Each folder's answer to "trust this folder?", by the folder's real
        -- path. Its own table for the same reason as `projects`: removing a run
        -- keeps the answer, and only Forget removes it.
        CREATE TABLE IF NOT EXISTS folder_trust(
            path        TEXT PRIMARY KEY,
            trusted     INTEGER NOT NULL,
            answered_at TEXT NOT NULL
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
            push_credential   TEXT,
            -- An agent list a phone could once advertise, and the daemon run
            -- that confirmed it. **Read and written by nothing**: no shipping
            -- phone ever sent one, and a push is not scoped by agent. Declared
            -- so a new database has the shape every release since v0.7.0 made;
            -- the bundled SQLite cannot drop `features_epoch` from those tables.
            features          TEXT,
            features_epoch    TEXT,
            -- The Tailscale device (stable node id) it was last seen connecting
            -- from. A revoked row that carries one keeps that device out.
            tailnet_node      TEXT,
            -- 1 on every row that existed when `tailnet_node` was added: paired
            -- before devices were recorded. Revoked with no node, such a row
            -- pauses every sign-in without a token or a code.
            paired_before_nodes INTEGER NOT NULL DEFAULT 0
        );

        CREATE UNIQUE INDEX IF NOT EXISTS devices_name ON devices(name);
        "#,
    )?;
    Ok(())
}

/// A card in the shared table whose run is a Codex run.
///
/// Written against the alias `p`, and named once so the predicate in
/// [`needs_codex_card_move`] and the statement in
/// [`move_codex_pending_approvals`] cannot answer it differently.
///
/// **Asked of the data, never of `user_version`**: a rollback resets the number,
/// so it is the wrong question.
const MISFILED_CARD: &str = "EXISTS(SELECT 1 FROM codex_sessions AS c
                                    WHERE c.session_uid = p.session_uid)";

fn needs_codex_card_move(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "pending_approvals")? || !table_exists(conn, "codex_sessions")? {
        return Ok(false);
    }
    Ok(conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pending_approvals AS p WHERE {MISFILED_CARD})"),
        [],
        |row| row.get(0),
    )?)
}

/// Take Codex cards out of the shared table — by **deleting** them, and that is
/// the honest action rather than a lossy one.
///
/// A card is a projection, not a fact: the fact is the `approval_request` event,
/// and the card exists so a restart can answer "what is this agent blocked on?".
/// Moving one into `codex_pending_approvals` would need the Codex identity that
/// table is keyed by — `thread_id`, `turn_id`, `item_id` — and a row in the
/// shared table has none of them and no way to reconstruct them: the wire that
/// could name the item is a `serverRequest` on an app-server connection that no
/// longer exists. A synthesised identity would be a fabricated one, and it would
/// occupy the unique index that stops a real re-delivery from rebinding.
///
/// **Nothing this build ships can produce such a row**, which is the reason this
/// is a delete and not a migration worth more code: the three shared-table
/// producers refuse a non-Claude run before any durable write, and the observer that
/// replaces one of those refusals writes `codex_pending_approvals` directly. A
/// row here therefore means an intermediate build or a hand-edited database, and
/// leaving it is the one thing that is definitely wrong — it is precisely the
/// card a rolled-back v0.6.0 enumerates and deletes on its own, without the log
/// line below.
fn move_codex_pending_approvals(tx: &Connection) -> Result<usize> {
    let removed = tx.execute(
        &format!("DELETE FROM pending_approvals AS p WHERE {MISFILED_CARD}"),
        [],
    )?;
    if removed > 0 {
        crate::log_error!(
            "schema: {removed} Codex approval card(s) were in the shared pending_approvals \
             table, which no shipping build can write; they carry no Codex item identity and \
             cannot be re-keyed, so they were removed rather than left where a rolled-back \
             v0.6 daemon sweeps them. The runs themselves are untouched, and a live approval \
             is re-delivered by the app-server on the next resume"
        );
    }
    Ok(removed)
}

/// The most folders [`Store::projects`] returns.
const MAX_PROJECTS: u32 = 200;

/// One of an agent's accounts on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    /// `claude` or `codex`.
    pub agent: String,
    pub label: String,
}

fn account_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        agent: row.get(1)?,
        label: row.get(2)?,
    })
}

/// `folder_trust.trusted` as the answer it stands for.
fn trust_from(trusted: bool) -> Trust {
    if trusted {
        Trust::Trusted
    } else {
        Trust::NotTrusted
    }
}

/// Files `(path, last_used, last_agent)` rows into `projects`, each row followed
/// by [`RECORD_PROJECTS_NEWEST`]: a folder keeps the newest start it has seen.
const RECORD_PROJECTS: &str = "INSERT INTO projects(path, last_used, last_agent)";
const RECORD_PROJECTS_NEWEST: &str = "ON CONFLICT(path) DO UPDATE SET
    last_used = excluded.last_used, last_agent = excluded.last_agent
    WHERE excluded.last_used > projects.last_used";

/// Columns added to an existing table after the fact.
///
/// Every entry must be nullable or carry a default: SQLite cannot add a `NOT
/// NULL` column without one, and a migration that fails leaves a daemon that
/// cannot open its own database.
const COLUMN_ADDITIONS: &[(&str, &str, &str)] = &[
    ("devices", "push_token", "TEXT"),
    ("devices", "push_environment", "TEXT"),
    ("devices", "push_credential", "TEXT"),
    // The agent seam (minor 15). Additive on both fresh (the CREATE TABLE above)
    // and existing databases (these ALTERs). `agent` carries a default so a
    // legacy row is backfilled to 'claude' by SQLite itself; the Codex thread
    // column is nullable.
    ("sessions", "agent", "TEXT NOT NULL DEFAULT 'claude'"),
    ("sessions", "codex_thread_id", "TEXT"),
    // Read and written by nothing; declared for one shape across every database.
    // See the `devices` table above.
    ("devices", "features", "TEXT"),
    ("devices", "features_epoch", "TEXT"),
    // Tailscale sign-in (minor 32). Adding `tailnet_node` is also when every
    // existing row is marked `paired_before_nodes`: see [`Store::migrate`].
    ("devices", "tailnet_node", "TEXT"),
    (
        "devices",
        "paired_before_nodes",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    // A Codex question card's answer, kept whole (X7b).
    ("mutation_ledger", "question_decision", "TEXT"),
    // The account a run started on (minor 48); null is its agent's Default, and every run recorded before accounts.
    ("run_hosts", "account_id", "TEXT"),
    ("run_hosts", "switching", "INTEGER"),
    ("run_hosts", "launch_account", "TEXT"),
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
/// every restart would make relay push re-register for ever, so this is not an
/// always-on normalizer. At this point in the rollout no phone can obtain
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

/// Widen the tables that predate a column.
///
/// `ALTER TABLE … ADD COLUMN` has no `IF NOT EXISTS`, so a second attempt at a
/// column that is already there fails with a duplicate-column error — which,
/// raised from here, is a daemon that will not open its own database.
///
/// Takes the caller's connection rather than opening a transaction of its own,
/// because [`Store::migrate`] runs this inside its one `BEGIN IMMEDIATE`. The
/// write lock is taken there, earlier, and held across the whole migration.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    // Asked again under the write lock: two daemons starting at once can both pass
    // the unlocked check, and the one that arrives second must find nothing left to
    // do rather than repeat an `ALTER` the first one already committed.
    let additions = missing_columns(conn)?;
    if additions.is_empty() {
        crate::log_debug!("another process widened these tables first; nothing to do");
        return Ok(());
    }

    for (table, column, kind) in additions {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
            [],
        )?;
        crate::log_info!("schema: added {table}.{column}");
    }

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
/// The three SSH columns are dead weight rather than an obstacle, and a build
/// that never manages to drop one still pairs phones and authenticates them.
/// `devices.ssh_key_installed` carries a default and `devices.ssh_fingerprint`
/// is nullable, so every statement here writes around them.
/// `pairing_codes.allow_ssh` is `NOT NULL` with no default and would break the
/// mint, which is why [`Store::create_pairing_code`] names it explicitly while
/// it is present — a column that cannot be dropped is a cosmetic problem, never
/// a pairing outage.
///
/// The eight on the two session tables are the opposite: `tmux_session` and
/// `tmux_socket` are `NOT NULL` with no default, so a session table still
/// carrying one refuses every run [`Store::begin_run`] files. A refused drop of
/// any of them fails the open (see [`drop_retired_columns`]).
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
    ("sessions", "tmux_session"),
    ("sessions", "tmux_socket"),
    ("sessions", "codex_socket"),
    ("sessions", "codex_generation"),
    ("codex_sessions", "tmux_session"),
    ("codex_sessions", "tmux_socket"),
    ("codex_sessions", "codex_socket"),
    ("codex_sessions", "codex_generation"),
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
/// is no second attempt worth making. For the three SSH columns the column is
/// left where it is and a warning names the table, the column and what SQLite
/// said. That is survivable on all three — the two on `devices` because they
/// are nullable or defaulted, `pairing_codes.allow_ssh` because
/// [`Store::create_pairing_code`] fills it while it is there. Nothing is deleted
/// and no table is rebuilt to force a removal through: a column left behind costs
/// an operator one `DROP INDEX` and a restart, and there is no cost that a build
/// is entitled to pay with somebody else's rows or with a newer build's columns.
///
/// **A session table's retired column is not survivable**, so a refused drop of
/// one returns the error before the commit: the whole transaction, the view drop
/// with it, rolls back, and the open fails as any other migration error does.
///
/// The `all_sessions` view is the one schema object that names the session
/// tables' retired columns, and SQLite re-checks views after a drop, so it is
/// dropped first, inside the same transaction. [`create_schema`] recreates it
/// without them, in the same open; `CREATE VIEW IF NOT EXISTS` never replaces a
/// view, so this drop is the only thing that removes the old definition.
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
    tx.execute_batch("DROP VIEW IF EXISTS all_sessions;")?;
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
        if SESSION_TABLES.contains(&table) {
            return Err(err.context(format!(
                "schema: SQLite will not drop the retired column {table}.{column}"
            )));
        }
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

/// What filing one Codex approval card decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexCardOutcome {
    /// A new card. Nothing held this derived id before.
    Filed,
    /// The same question again, byte for byte — a re-delivery to a reconnecting
    /// link. The stored row already says exactly this, so nothing is written and
    /// the card the phone holds is still the right one.
    Rebound,
    /// The run was deleted while the request was in flight.
    SessionGone,
    /// A re-delivery under the same derived id carrying a **different question**.
    /// Refused; the stored card stands. See [`upsert_codex_card_in_tx`].
    ContentChanged,
}

/// File one Codex approval card, in the table a rolled-back v0.6.0 daemon
/// has never heard of.
///
/// **Idempotent on the item, which is the only stable identity.** A Codex
/// `serverRequest` id is a per-connection integer from zero, so the same
/// approval re-delivered to a reconnecting link arrives calling itself `0`
/// again; `request_id` is therefore derived from `(thread_id, item_id)` by
/// [`crate::codex_approval::Approval::request_id`] and a re-delivery lands on
/// the row that id already holds. The unique index on
/// `(session_uid, thread_id, item_id)` is the second half of the same rule and
/// catches the case the primary key cannot — a *broken* derivation, which
/// produces a different key for one item and would otherwise put two cards on
/// the phone for one question.
///
/// The `WHERE EXISTS` guard: an approval for a run deleted mid-flight must not
/// leave an orphan row that recovery later raises for a session nobody can name.
///
/// # A re-delivery is compared, not merged
///
/// This used to be an `ON CONFLICT … DO UPDATE SET card, turn_id`, and that
/// silently split the card in half. The row and the in-memory card would take
/// the new content while the already-filed `ApprovalRequest` — and every
/// connected client holding it — kept the old one, with no event and no ring to
/// say the question had changed. A phone would then be showing, and answering,
/// a question the app-server had replaced.
///
/// So the stored row is read and compared instead. **Byte equality of `card`,
/// not hash equality**, because it is both simpler and strictly stronger: the
/// hash is a field *inside* the card, so equal bytes imply an equal hash and
/// also catch drift in `display_text` or `tool_input` that a hash comparison
/// alone would not. Nothing in the card varies non-semantically — `request_id`
/// is derived, `generation` is part of that derivation so it cannot differ
/// within one conflict, and `risk` is a pure function of the content — so equal
/// bytes are exactly "the same question again". `turn_id` is compared for the
/// same reason: an item belongs to its turn, so a re-delivery naming a different
/// one is not a re-delivery of the same thing.
///
/// A difference is **refused**, not reconciled. Every capture of a re-delivery
/// shows a byte-identical question, so a content-changing one is a shape the
/// wire has never produced; inventing a reconciliation for it would be building
/// machinery for an input nothing can currently generate, and guessing at which
/// half of the representation to move. The stored card stands and the mismatch
/// is logged loudly.
///
/// Takes the transaction rather than the store, because it is never one
/// write on its own: see [`Store::raise_codex_pending_approval`].
fn upsert_codex_card_in_tx(
    tx: &rusqlite::Transaction<'_>,
    row: &CodexPendingApprovalRow,
) -> Result<CodexCardOutcome> {
    let held: Option<(String, String)> = tx
        .query_row(
            "SELECT card, turn_id FROM codex_pending_approvals
              WHERE session_uid = ?1 AND request_id = ?2",
            params![row.session_uid, row.request_id],
            |stored| Ok((stored.get(0)?, stored.get(1)?)),
        )
        .optional()?;
    if let Some((card, turn_id)) = held {
        if card == row.card && turn_id == row.turn_id {
            return Ok(CodexCardOutcome::Rebound);
        }
        return Ok(CodexCardOutcome::ContentChanged);
    }

    let changed = tx.execute(
        "INSERT INTO codex_pending_approvals(session_uid, session_id, request_id, card,
                                             generation, created_ms, thread_id, turn_id,
                                             item_id, family)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
          WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)",
        params![
            row.session_uid,
            row.session_id,
            row.request_id,
            row.card,
            row.generation as i64,
            row.created_ms,
            row.thread_id,
            row.turn_id,
            row.item_id,
            row.family,
        ],
    )?;
    Ok(if changed > 0 {
        CodexCardOutcome::Filed
    } else {
        CodexCardOutcome::SessionGone
    })
}

/// Move the run `session_uid`'s host cursor to `seq` inside `tx`, only for that host
/// and only forward. False when it did not move.
fn advance_host_cursor(
    tx: &rusqlite::Transaction<'_>,
    session_uid: &str,
    host: &ProcessIdentity,
    seq: u64,
) -> Result<bool> {
    let seq = i64::try_from(seq).context("a host sequence number past i64")?;
    let moved = tx.execute(
        "UPDATE run_hosts SET cursor = ?5
          WHERE session_uid = ?1 AND host_pid = ?2
            AND host_start_sec = ?3 AND host_start_usec = ?4 AND cursor < ?5",
        params![
            session_uid,
            host.pid,
            host.birth.start_sec,
            host.birth.start_usec,
            seq
        ],
    )?;
    Ok(moved == 1)
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

    // A fact for a session that is gone is dropped, not filed. An event or an
    // approval already in flight when the phone's delete commits would
    // otherwise write rows keyed to a uid nothing can name again — an orphan,
    // refused here so every single-event path shares the rule. `None` is the shape callers already
    // tolerate for a dedup miss.
    //
    // `all_sessions`, and this is the single highest-consequence line of the
    // split: every fact either agent records passes through here. Pointed at
    // the Claude table alone it reads a live Codex run as gone and drops the
    // event as if it were a dedup miss — silently, because `Ok(None)` is
    // exactly what a duplicate returns.
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM all_sessions WHERE session_uid = ?1)",
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

/// The task id [`file_unknown_tasks`] files: no task claude names.
pub(crate) const UNKNOWN_TASK: &str = "unknown";

/// Each Claude run whose host may still be running (`starting`, `live` or `stopping`), launched by a build that filed
/// none of its claude's background tasks: one task filed open under [`UNKNOWN_TASK`], which no end closes, so the run
/// is never certainly quiet while that host runs ([`Store::certainly_quiet`]). The runs filed.
fn file_unknown_tasks(tx: &rusqlite::Transaction<'_>) -> Result<usize> {
    let uids = tx
        .prepare(
            "SELECT s.session_uid FROM sessions s JOIN run_hosts h ON h.session_uid = s.session_uid
              WHERE s.agent = ?1 AND s.lifecycle IN ('starting', 'live', 'stopping')",
        )?
        .query_map(params![AgentKind::Claude.as_str()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for uid in &uids {
        append_in_tx(
            tx,
            &PendingEvent::new(
                &protocol::event::SessionKey::new(uid, uid),
                EventKind::Other(protocol::sdk_map::BACKGROUND_TASK.into()),
                serde_json::json!({"task_id": UNKNOWN_TASK, "ended": false}),
                Source::Daemon,
            ),
        )?;
    }
    Ok(uids.len())
}

fn session_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_uid: row.get(0)?,
        session_id: row.get(1)?,
        cwd: row.get(2)?,
        claude_session_id: row.get(3)?,
        transcript_path: row.get(4)?,
        lifecycle: parse_lifecycle(&row.get::<_, String>(5)?),
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        // Positions 8 and 9 are the agent-seam columns, always selected last. A NULL
        // `agent` is true absence — a value neither the CREATE default nor a
        // backfill produced — and decodes as Claude; a *present* value goes
        // through `from_str_lossy`, so a present unrecognised string fails closed
        // rather than being read as Claude.
        agent: match row.get::<_, Option<String>>(8)? {
            None => AgentKind::Claude,
            Some(value) => AgentKind::from_str_lossy(&value),
        },
        codex_thread_id: row.get(9)?,
    })
}

fn device_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        device_id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        last_seen_at: row.get(3)?,
        revoked_at: row.get(4)?,
        tailnet_node: row.get(5)?,
        paired_before_nodes: row.get(6)?,
    })
}

fn insert_device_row(
    conn: &Connection,
    device_id: &str,
    name: &str,
    token_hash: &str,
    created_at: &str,
    tailnet_node: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO devices(device_id, name, token_hash, created_at,
                             last_seen_at, revoked_at, tailnet_node)
         VALUES(?1, ?2, ?3, ?4, ?4, NULL, ?5)",
        params![device_id, name, token_hash, created_at, tailnet_node],
    )?;
    Ok(())
}

pub(crate) fn lifecycle_str(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Starting => "starting",
        Lifecycle::Live => "live",
        Lifecycle::Stopping => "stopping",
        Lifecycle::Exited => "exited",
        Lifecycle::Failed => "failed",
        Lifecycle::Unknown => "unknown",
    }
}

fn parse_lifecycle(value: &str) -> Lifecycle {
    match value {
        "starting" => Lifecycle::Starting,
        "live" => Lifecycle::Live,
        "stopping" => Lifecycle::Stopping,
        "exited" => Lifecycle::Exited,
        "failed" => Lifecycle::Failed,
        _ => Lifecycle::Unknown,
    }
}

fn parse_source(value: &str) -> Source {
    match value {
        "hook" => Source::Hook,
        "transcript" => Source::Transcript,
        "daemon" => Source::Daemon,
        "codex" => Source::Codex,
        "sdk" => Source::Sdk,
        "acp" => Source::Acp,
        "pty" => Source::Pty,
        // An unrecognised persisted source — a fact written by a newer daemon,
        // read back after a rollback — is the lowest trust there is, never the
        // trusted `Daemon` it used to become. A source it does not understand
        // must not out-rank a real hook or transcript fact in dedup.
        _ => Source::Unknown,
    }
}

const HOUR_MS: i64 = 60 * 60 * 1000;

/// Whether unread mail waits for `session_uid`.
fn unread_mail(tx: &rusqlite::Transaction<'_>, session_uid: &str, now_ms: i64) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM mail WHERE session_uid = ?1 AND state = 'waiting' AND expires_ms > ?2
              LIMIT 1",
            params![session_uid, now_ms],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A `mail` or `mail_sent` state as stored. Only this file writes them.
fn mail_state(text: &str) -> protocol::ipc::MailState {
    protocol::ipc::MailState::parse(text).unwrap_or(protocol::ipc::MailState::Waiting)
}

/// **Settle the `mail` rows `filter` picks at `state`**, and tell each sender: one
/// on this computer at once, in its `mail_sent` row, and one on another computer
/// by marking the row untold. `filter` binds `?1` onwards to `bind`. Returns how
/// many rows were settled.
fn settle_mail(
    tx: &rusqlite::Transaction<'_>,
    filter: &str,
    bind: impl rusqlite::Params,
    state: &str,
) -> Result<usize> {
    let ids: Vec<(String, bool)> = {
        // `filter` is a literal in this file; nothing a caller supplies reaches it.
        let mut stmt = tx.prepare(&format!(
            "SELECT message_id, sender_computer = '' FROM mail WHERE {filter}"
        ))?;
        let rows = stmt.query_map(bind, |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        ids
    };
    for (message_id, here) in &ids {
        tx.execute(
            "UPDATE mail SET state = ?2, offer = NULL, sender_told = ?3 WHERE message_id = ?1",
            params![message_id, state, *here],
        )?;
        if *here {
            tx.execute(
                "UPDATE mail_sent SET state = ?2
                  WHERE message_id = ?1 AND state IN ('sent', 'waiting')",
                params![message_id, state],
            )?;
        }
    }
    Ok(ids.len())
}

/// What the newest of a run's events matching `filter` decides, walking them
/// newest first and stopping at the first that decides anything. `filter` binds
/// `?2` onwards to `bind`.
fn newest_decided<T>(
    conn: &Connection,
    session_uid: &str,
    filter: &str,
    bind: &[&str],
    decide: fn(&Event) -> Option<T>,
) -> Result<Option<T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT seq, session_id, ts, kind, payload, source, source_event_id, turn_id, item_id
           FROM events
          WHERE session_uid = ?1 AND {filter}
          ORDER BY seq DESC"
    ))?;
    let mut rows = stmt.query(rusqlite::params_from_iter(
        std::iter::once(session_uid).chain(bind.iter().copied()),
    ))?;
    while let Some(row) = rows.next()? {
        if let Some(fact) = decide(&event_from_row(session_uid, row)?) {
            return Ok(Some(fact));
        }
    }
    Ok(None)
}

/// One event, from a row selected as `seq, session_id, ts, kind, payload, source,
/// source_event_id, turn_id, item_id`.
fn event_from_row(session_uid: &str, row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
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
}

/// Whether the Antigravity run `session_uid` is certainly quiet by what its process has filed since the run's newest
/// end: agy takes each message at once and queues it behind the turn running, then ends that message's turn with the
/// reply to its prompt, filed under its uuid. So every message and mail wake has its turn's end, no tool it began is
/// still running, and its last turn ran no command, which may still run in the background.
fn acp_quiet(conn: &Connection, session_uid: &str) -> Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT kind, source_event_id, turn_id, payload FROM events
          WHERE session_uid = ?1
            AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE session_uid = ?1 AND kind = ?2), 0)
          ORDER BY seq",
    )?;
    let events = stmt
        .query_map(
            params![session_uid, EventKind::SessionEnd.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, String>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let ends: Vec<&str> = events
        .iter()
        .filter(|(kind, ..)| kind == EventKind::TurnComplete.as_str())
        .map(|(_, _, turn, _)| turn.as_str())
        .collect();
    let Some(&last) = ends.last() else {
        return Ok(false);
    };
    let mut running = std::collections::HashSet::new();
    for (kind, id, turn, payload) in &events {
        let asked = id.strip_prefix("say:").or_else(|| id.strip_prefix("wake:"));
        if asked.is_some_and(|asked| !ends.contains(&asked)) {
            return Ok(false);
        }
        let tool = serde_json::from_str::<serde_json::Value>(payload).unwrap_or_default();
        let call = tool["tool_use_id"].as_str().unwrap_or_default().to_string();
        if kind == EventKind::ToolCall.as_str() {
            if turn == last && tool["tool_name"] == "Run" {
                return Ok(false);
            }
            running.insert(call);
        } else if kind == EventKind::ToolResult.as_str() {
            running.remove(&call);
        }
    }
    Ok(running.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::event::SessionKey;
    use protocol::ws::{AnswerDecision, AnswerPath, ResolvedBy};
    use serde_json::json;

    fn temp_store() -> (Store, TempDb) {
        let store = Store::open_temp("ccd-test");
        let path = store.temp_db();
        (store, path)
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

    /// **The generalized ledger's conflict law**: a retry under
    /// the same `(operation_kind, session_uid, client_request_id)` with *changed*
    /// claimed material is a conflict, never a second actuation; an unsettled
    /// replay is indeterminate; a settled one replays its outcome; the operation
    /// kind is part of the key.
    #[test]
    fn the_mutation_ledger_conflicts_on_changed_material_and_never_actuates_twice() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let now = protocol::time::now_rfc3339();

        // The immutable material a compose was claimed with: it was a `turn_start`
        // against thread T at generation 3, targeting no existing turn.
        let material = |hash: &str| ClaimedMaterial {
            thread_id: "th_T".into(),
            generation: 3,
            route: "turn_start".into(),
            target_turn_id: None,
            claimed_hash: hash.into(),
            question_decision: None,
        };

        // First claim owns the id.
        assert_eq!(
            store
                .claim_mutation("compose", &session.uid, "req-1", &material("hashA"), &now)
                .unwrap(),
            MutationClaim::Claimed
        );
        // Same id, same material, still in flight: an unsettled replay is
        // indeterminate — recognised as the same mutation, never re-run — and it
        // returns the **claimed** material so recovery replays the original route.
        match store
            .claim_mutation("compose", &session.uid, "req-1", &material("hashA"), &now)
            .unwrap()
        {
            MutationClaim::Indeterminate { claimed, .. } => {
                assert_eq!(claimed, material("hashA"));
                assert_eq!(claimed.route, "turn_start");
            }
            other => panic!("expected indeterminate, got {other:?}"),
        }
        // **Same id, different material ⇒ Conflict.** This is the actuation
        // guard: a captured frame replayed with new claimed material can never
        // ride an id the ledger already trusts.
        assert_eq!(
            store
                .claim_mutation("compose", &session.uid, "req-1", &material("hashB"), &now)
                .unwrap(),
            MutationClaim::Conflict
        );

        // **First-terminal-wins settle.** The first settle records the outcome
        // and reports it won; a second settle of the now-terminal claim writes
        // nothing and reports it did not win — the recorded outcome is immutable.
        assert!(
            store
                .settle_mutation("compose", &session.uid, "req-1", "turn_started", &now)
                .unwrap(),
            "the first settle wins"
        );
        assert!(
            !store
                .settle_mutation("compose", &session.uid, "req-1", "OVERWRITE", &now)
                .unwrap(),
            "a second settle of a terminal claim must not win"
        );

        // A same-material retry now replays the recorded outcome **and** the
        // claimed route material — never actuating anew.
        match store
            .claim_mutation("compose", &session.uid, "req-1", &material("hashA"), &now)
            .unwrap()
        {
            MutationClaim::Applied { outcome, claimed } => {
                assert_eq!(outcome, "turn_started", "the first outcome is immutable");
                assert_eq!(claimed, material("hashA"), "the original route replays");
            }
            other => panic!("expected applied, got {other:?}"),
        }

        // The operation kind is part of the key: the same session and id under a
        // different kind is an independent claim, not a conflict.
        assert_eq!(
            store
                .claim_mutation("interrupt", &session.uid, "req-1", &material("hashZ"), &now)
                .unwrap(),
            MutationClaim::Claimed
        );

        // A claim for a session that does not exist writes nothing to actuate
        // against — distinct from an owned claim.
        assert_eq!(
            store
                .claim_mutation("interrupt", &uid("ZZ"), "req-9", &material("h"), &now)
                .unwrap(),
            MutationClaim::NoSession
        );

        // **A changed immutable field is a conflict even with the SAME hash.**
        // A fresh claim, then a retry that keeps the hash but changes the route:
        // the explicit-field comparison catches it — never a duplicate.
        let steer = ClaimedMaterial {
            route: "turn_steer".into(),
            ..material("hashSame")
        };
        assert_eq!(
            store
                .claim_mutation(
                    "compose",
                    &session.uid,
                    "req-2",
                    &material("hashSame"),
                    &now
                )
                .unwrap(),
            MutationClaim::Claimed
        );
        assert_eq!(
            store
                .claim_mutation("compose", &session.uid, "req-2", &steer, &now)
                .unwrap(),
            MutationClaim::Conflict,
            "same hash, changed route ⇒ conflict, never a duplicate"
        );

        // **Settle is refused over ANY terminal state, not just `done`.** Force a
        // claim into the terminal `indeterminate` state (what recovery writes)
        // and prove a settle over it does not win and does not overwrite it.
        store
            .claim_mutation("interrupt", &session.uid, "req-3", &material("hashI"), &now)
            .unwrap();
        store
            .write()
            .execute(
                "UPDATE mutation_ledger SET status = 'indeterminate'
                  WHERE operation_kind='interrupt' AND session_uid=?1 AND client_request_id='req-3'",
                params![session.uid],
            )
            .unwrap();
        assert!(
            !store
                .settle_mutation("interrupt", &session.uid, "req-3", "aborted", &now)
                .unwrap(),
            "settle over a terminal `indeterminate` must not win"
        );
        let status: String = store
            .read()
            .query_row(
                "SELECT status FROM mutation_ledger
                  WHERE operation_kind='interrupt' AND session_uid=?1 AND client_request_id='req-3'",
                params![session.uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "indeterminate", "the terminal outcome is immutable");
    }

    /// **One phone answer, claimed and settled in the one generalized ledger.**
    ///
    /// The generalized ledger says it covers "answer, compose, interrupt", and
    /// this is the answer half arriving. What it must NOT arrive as is a second
    /// claim-before-write table for the operation the first one was built for:
    /// two idempotency laws for one question drift the moment only one of them
    /// learns something, and the cross-operation tests could then never exercise
    /// the production answer path.
    ///
    /// The claimed material is the whole authorization surface of an answer. For
    /// `operation_kind = "answer"` the `route` column carries the option the
    /// phone named — that is what "the snapshotted route" means for an answer,
    /// and it is what recovery reads back to say which decision was attempted —
    /// while `claimed_hash` covers the card's payload hash and the wire decision
    /// as well, so a replay under the same id against a refreshed card is a
    /// conflict rather than a second write.
    ///
    /// **Mutation:** claim answers under a second, answer-only ledger table of
    /// their own and the `Applied` replay below goes red, because the generalized
    /// ledger — the one every later Codex mutation will use — would know nothing
    /// about the answer. (`a_fresh_database_is_created_at_the_current_schema`
    /// asserts that second table does not exist.)
    #[test]
    fn an_answer_is_claimed_and_settled_in_the_one_generalized_ledger() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let now = protocol::time::now_rfc3339();
        insert_codex_card(&store, &session, "derived-1", "item-1", "turn-1", 10, "{}").unwrap();

        let material = |hash: &str| ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: hash.into(),
            question_decision: None,
        };
        assert_eq!(
            store
                .claim_mutation(
                    "answer",
                    &session.uid,
                    "derived-1",
                    &material("hashA"),
                    &now
                )
                .unwrap(),
            MutationClaim::Claimed,
            "an answer takes its claim in the ledger every Codex mutation shares"
        );

        // The card, its terminal and the ledger settle are one commit.
        let event = store
            .retire_codex_pending_approval(
                &session.uid,
                "derived-1",
                &pending(
                    &session,
                    EventKind::ApprovalResolved,
                    Some("resolved:derived-1"),
                ),
                Some(AnswerTerminal::Settled("delivered")),
            )
            .unwrap();
        assert!(event.is_some(), "the resolution rides the same transaction");
        assert!(store
            .codex_pending_approvals(&session.uid)
            .unwrap()
            .is_empty());

        // A second tap replays the recorded outcome instead of writing again.
        match store
            .claim_mutation(
                "answer",
                &session.uid,
                "derived-1",
                &material("hashA"),
                &now,
            )
            .unwrap()
        {
            MutationClaim::Applied { outcome, claimed } => {
                assert_eq!(outcome, "delivered");
                assert_eq!(claimed.route, "accept", "the option the phone named");
            }
            other => panic!("a settled answer must replay, not re-actuate: {other:?}"),
        }

        // The same id against a card that has since been refreshed is a different
        // mutation, and is refused rather than conflated with the one above.
        assert_eq!(
            store
                .claim_mutation(
                    "answer",
                    &session.uid,
                    "derived-1",
                    &material("hashB"),
                    &now
                )
                .unwrap(),
            MutationClaim::Conflict
        );
    }

    /// **The ledger settle, the card delete and the resolution are one commit or
    /// none of them.**
    ///
    /// Three separate writes left a window at every boundary, and the worst of
    /// them is silent: a settled ledger with the card still standing is an answer
    /// the operator can never make again against a question that is still being
    /// asked. The failure is injected reversibly, so what is asserted is that a
    /// commit which cannot delete the card leaves the claim exactly as live as it
    /// was — and the next terminal can still finish it.
    ///
    /// **Mutation:** settle the ledger in a commit of its own *before* the card's
    /// transaction and the `Indeterminate` assertion below goes red — the row
    /// would read `done` for a card nobody retired, which is the silent shape: the
    /// operator is refused by the ledger while the question is still on screen.
    ///
    /// The mirror ordering — settling *after* the card's commit — is deliberately
    /// not claimed here, because this test cannot tell it apart: the injected
    /// failure aborts the card's transaction first, so a settle placed after it is
    /// never reached either. Distinguishing that one needs a failure injected
    /// between two commits, which is a fixture this store does not have and which
    /// would only exist to test the arrangement the fix removes.
    #[test]
    fn a_retirement_that_cannot_commit_leaves_the_answer_claim_live() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let now = protocol::time::now_rfc3339();
        insert_codex_card(&store, &session, "derived-1", "item-1", "turn-1", 10, "{}").unwrap();
        let material = ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: "hashA".into(),
            question_decision: None,
        };
        assert_eq!(
            store
                .claim_mutation("answer", &session.uid, "derived-1", &material, &now)
                .unwrap(),
            MutationClaim::Claimed
        );

        store.hide_codex_cards_for_tests(true);
        assert!(
            store
                .retire_codex_pending_approval(
                    &session.uid,
                    "derived-1",
                    &pending(
                        &session,
                        EventKind::ApprovalResolved,
                        Some("resolved:derived-1")
                    ),
                    Some(AnswerTerminal::Settled("delivered")),
                )
                .is_err(),
            "a commit that cannot reach the card must fail rather than settle half of it"
        );
        store.hide_codex_cards_for_tests(false);

        match store
            .claim_mutation("answer", &session.uid, "derived-1", &material, &now)
            .unwrap()
        {
            MutationClaim::Indeterminate { .. } => {}
            other => panic!("nothing may have settled: {other:?}"),
        }
        assert_eq!(
            store.codex_pending_approvals(&session.uid).unwrap().len(),
            1,
            "the question still stands, so the next terminal can retire it"
        );
    }

    /// **The one commit must prove it settled the claim it was given, or roll back.**
    ///
    /// `settle_answer_in_tx` discarded the guarded `UPDATE`'s affected
    /// row count, so a retirement carrying an `AnswerTerminal` could delete the card and
    /// append `ApprovalResolved{Phone}` while settling **zero** applying claims — the
    /// sharing of one transaction proved only that the two statements ran together, not
    /// that the second one did anything.
    ///
    /// That is not a hypothetical shape: a recovery that already made the claim
    /// `indeterminate` leaves exactly this state, and so does a claim that was never
    /// taken. In both, the card would go, a resolution naming the PHONE would be filed,
    /// and the ledger would keep saying something else entirely.
    ///
    /// Every caller that passes an `AnswerTerminal` holds a live `applying` claim by
    /// construction, so "exactly one row" is the invariant and not a preference. A
    /// violation rolls back, the card stays, `Retirement::Failed` puts it back in memory
    /// and recovery makes the same terminal on the next start — the fail-closed
    /// direction.
    ///
    /// **Mutation:** ignore the affected-row count in `settle_answer_in_tx` (return
    /// `Ok(())` from the `execute`) and both halves below go red: the card disappears and
    /// a resolution is filed for a claim that was already terminal.
    #[test]
    fn an_answer_terminal_that_settles_no_claim_rolls_the_whole_commit_back() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let now = protocol::time::now_rfc3339();

        // --- no claim at all under this key ---
        insert_codex_card(&store, &session, "derived-1", "item-1", "turn-1", 10, "{}").unwrap();
        assert!(
            store
                .retire_codex_pending_approval(
                    &session.uid,
                    "derived-1",
                    &pending(
                        &session,
                        EventKind::ApprovalResolved,
                        Some("resolved:derived-1")
                    ),
                    Some(AnswerTerminal::Settled(ANSWER_DELIVERED)),
                )
                .is_err(),
            "a phone terminal for a claim that does not exist must not commit"
        );
        assert_eq!(
            store.codex_pending_approvals(&session.uid).unwrap().len(),
            1,
            "the card stays, so the next terminal can still retire it"
        );
        assert!(
            store
                .events_after(&session.uid, 0, 100)
                .unwrap()
                .iter()
                .all(|e| e.kind != EventKind::ApprovalResolved),
            "and no resolution was filed for a settle that settled nothing"
        );

        // --- a claim that is already terminal: first-terminal-wins, and the guard is
        //     what makes the second one fail loudly instead of quietly doing nothing ---
        let material = ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: "hashA".into(),
            question_decision: None,
        };
        assert_eq!(
            store
                .claim_mutation("answer", &session.uid, "derived-1", &material, &now)
                .unwrap(),
            MutationClaim::Claimed
        );
        assert!(store
            .settle_mutation("answer", &session.uid, "derived-1", ANSWER_LOST, &now)
            .unwrap());
        assert!(
            store
                .retire_codex_pending_approval(
                    &session.uid,
                    "derived-1",
                    &pending(
                        &session,
                        EventKind::ApprovalResolved,
                        Some("resolved:derived-1")
                    ),
                    Some(AnswerTerminal::Settled(ANSWER_DELIVERED)),
                )
                .is_err(),
            "a claim another terminal already settled may not be overwritten by this one"
        );
        assert_eq!(
            store.codex_pending_approvals(&session.uid).unwrap().len(),
            1,
            "and nothing about the card changed either"
        );
        match store
            .claim_mutation("answer", &session.uid, "derived-1", &material, &now)
            .unwrap()
        {
            MutationClaim::Applied { outcome, .. } => assert_eq!(
                outcome, ANSWER_LOST,
                "the first terminal still stands, byte for byte"
            ),
            other => panic!("the settled claim must replay: {other:?}"),
        }
    }

    /// **Recovery can name every answer this daemon left mid-flight.**
    ///
    /// An `applying` row under `operation_kind = "answer"` is a claim whose write
    /// may or may not have reached the socket, and nothing that survives the
    /// restart can say which — so recovery has to find it in order to make it
    /// terminal rather than leave it live for a second tap to inherit.
    ///
    /// **Mutations:** drop the `operation_kind` filter and a `compose` claim comes
    /// back here, which would retire an approval card on the strength of an
    /// unrelated mutation. Drop the `status = 'applying'` guard from either
    /// settle and one of the two first-terminal assertions below goes red.
    #[test]
    fn recovery_reads_every_unsettled_answer_claim_and_no_other_operation() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let now = protocol::time::now_rfc3339();
        let material = |route: &str| ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: route.into(),
            target_turn_id: None,
            claimed_hash: "hashA".into(),
            question_decision: None,
        };
        store
            .claim_mutation("answer", &session.uid, "req-1", &material("accept"), &now)
            .unwrap();
        store
            .claim_mutation(
                "compose",
                &session.uid,
                "req-2",
                &material("turn_start"),
                &now,
            )
            .unwrap();
        store
            .claim_mutation("answer", &session.uid, "req-3", &material("cancel"), &now)
            .unwrap();
        assert!(store
            .settle_mutation("answer", &session.uid, "req-3", "delivered", &now)
            .unwrap());

        let open = store.unsettled_claims(OPERATION_ANSWER).unwrap();
        assert_eq!(
            open.iter()
                .map(|claim| (
                    claim.client_request_id.as_str(),
                    claim.claimed.route.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("req-1", "accept")],
            "only the answers, and only the ones nothing has settled"
        );

        // **First terminal wins, and it has to hold in both directions.** Recovery
        // writing `indeterminate` over a settled answer would forget an outcome
        // that was proven, and a late disposition writing `delivered` over
        // recovery's `indeterminate` would claim proof for an answer nobody can
        // account for. Both are asserted, because only asserting one leaves the
        // other's guard free to be deleted.
        assert!(store
            .settle_mutation_indeterminate("answer", &session.uid, "req-1", &now)
            .unwrap());
        assert!(
            !store
                .settle_mutation("answer", &session.uid, "req-1", "delivered", &now)
                .unwrap(),
            "a late disposition may never overwrite the terminal recovery wrote"
        );
        assert!(
            !store
                .settle_mutation_indeterminate("answer", &session.uid, "req-3", &now)
                .unwrap(),
            "recovery may never overwrite an outcome that was proven"
        );
        assert_eq!(
            store.answer_status(&session.uid, "req-3").unwrap(),
            Some(AnswerStatus::Settled("delivered".into())),
            "req-3 settled `delivered` before recovery ran, and stays that way"
        );
        assert!(store.unsettled_claims(OPERATION_ANSWER).unwrap().is_empty());
    }

    /// **Additive-column compatibility (new → old → new).**
    ///
    /// The invariant this asserts: the agent-seam columns are purely additive,
    /// so a positional reader or writer that names only the columns before them
    /// works unchanged and no data is lost across a reopen. **Only Claude rows
    /// are written here**, because the
    /// shared `sessions` table holds no other kind.
    ///
    /// > **Codex durable state lives in agent-scoped storage** (`codex_sessions`,
    /// > `codex_pending_approvals`, `mutation_ledger`) that a legacy daemon never
    /// > enumerates or mutates. A Codex row in the *shared* `sessions` table
    /// > would be enumerated by a rolled-back old daemon (its positional `SELECT`
    /// > returns every row, agent-agnostically). This test deliberately does
    /// > **not** place a Codex row in the shared table, because in a correct build
    /// > one cannot be there.
    #[test]
    fn additive_columns_are_transparent_to_a_legacy_reader_and_lose_no_data() {
        let (_p, path) = {
            let (store, path) = temp_store();
            // NEW daemon writes only Claude rows (the only kind the shared table holds).
            begin(&store, &key("AA", "cc-1"));
            drop(store);
            ((), path)
        };

        // A reader and a writer that name only the columns before the seam.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            // (a) It reads sessions with the eight columns before the seam, never
            // the agent-seam ones — the additive columns are invisible to it.
            let mut stmt = conn
                .prepare(
                    "SELECT session_uid, session_id, cwd, claude_session_id, transcript_path,
                            lifecycle, created_at, updated_at
                       FROM sessions ORDER BY session_id",
                )
                .unwrap();
            let names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(
                names,
                vec!["cc-1"],
                "a legacy reader enumerates its own columns"
            );
            drop(stmt);

            // (b) It writes a session with only those eight columns, and the run's
            // `run_hosts` row; SQLite applies the 'claude' default, so its row is a
            // Claude row exactly as a pre-seam one would have been.
            conn.execute(
                "INSERT INTO sessions(session_uid, session_id, cwd, claude_session_id,
                                      transcript_path, lifecycle, created_at, updated_at)
                 VALUES(?1,'cc-2','/tmp',NULL,NULL,'live','t','t')",
                params![uid("CC")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO run_hosts(session_uid, request_id) VALUES(?1, ?1)",
                params![uid("CC")],
            )
            .unwrap();
        }

        // NEW daemon reopens: migration re-runs idempotently and every fact
        // survives, with the legacy-written row backfilled to Claude.
        let store = Store::open(&path).unwrap();
        let by_name = |name: &str| {
            store
                .list_sessions()
                .unwrap()
                .into_iter()
                .find(|r| r.session_id == name)
                .unwrap()
        };
        assert_eq!(by_name("cc-1").agent, AgentKind::Claude);
        assert_eq!(
            by_name("cc-2").agent,
            AgentKind::Claude,
            "a legacy write backfills to Claude, never NULL/unknown"
        );
    }

    /// One SDK message files several events: a `result` files `turn_complete`
    /// and `usage`. Under the message's uuid alone the dedup key keeps only the
    /// first; under `sdk_event_id` both are kept, and a replay of the same
    /// message files nothing new.
    #[test]
    fn both_events_of_one_sdk_result_are_kept_and_a_replay_files_none() {
        use protocol::event::sdk_event_id;
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let filed = |kind: EventKind, id: String| {
            let mut event = PendingEvent::new(&session, kind, json!({"x": 1}), Source::Sdk);
            event.source_event_id = Some(id);
            event
        };
        let raw = store
            .append_batch(&[
                filed(EventKind::TurnComplete, "raw-uuid".into()),
                filed(EventKind::Usage, "raw-uuid".into()),
            ])
            .unwrap();
        assert_eq!(
            raw.len(),
            1,
            "the message's uuid alone keeps one of the two"
        );

        let result = || {
            [
                filed(
                    EventKind::TurnComplete,
                    sdk_event_id("msg-1", &EventKind::TurnComplete, 0),
                ),
                filed(
                    EventKind::Usage,
                    sdk_event_id("msg-1", &EventKind::Usage, 0),
                ),
            ]
        };
        let kept = store.append_batch(&result()).unwrap();
        assert_eq!(kept.len(), 2, "both events of one result are kept");
        assert!(
            store.append_batch(&result()).unwrap().is_empty(),
            "a replay of the same message files nothing new"
        );
    }

    #[test]
    fn a_persisted_sdk_source_is_read_back_as_itself() {
        assert_eq!(parse_source("sdk").as_str(), "sdk");
        assert_eq!(parse_source("sdk").trust(), 3);
    }

    /// An unrecognised persisted source never decodes to the trusted `Daemon`
    /// (store.rs's former `_ => Source::Daemon`): it becomes the lowest-trust
    /// `Unknown`, both at the decode boundary and end-to-end through a real read.
    #[test]
    fn an_unknown_persisted_source_never_decodes_as_trusted_daemon() {
        assert_eq!(parse_source("future_source"), Source::Unknown);
        assert_ne!(parse_source("future_source"), Source::Daemon);
        assert_eq!(parse_source("future_source").trust(), 0);
        // The known ones still decode as themselves.
        assert_eq!(parse_source("hook"), Source::Hook);
        assert_eq!(parse_source("daemon"), Source::Daemon);

        // End-to-end: a raw event row with a source string this build does not
        // know is read back as `Unknown`, not `Daemon`.
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        store
            .write()
            .execute(
                "INSERT INTO events(session_uid, seq, session_id, ts, kind, payload, source)
                 VALUES(?1, 1, ?2, 't', 'agent_message', '{}', 'future_source')",
                params![session.uid, session.name],
            )
            .unwrap();
        let events = store.events_after(&session.uid, 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, Source::Unknown);
        assert_ne!(events[0].source, Source::Daemon);
    }

    fn session_row(session: &SessionKey) -> SessionRow {
        let now = protocol::time::now_rfc3339();
        SessionRow {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: AgentKind::Claude,
            codex_thread_id: None,
        }
    }

    /// `session`'s run, begun as the engine begins one, its uid standing for the start
    /// request.
    fn begin(store: &Store, session: &SessionKey) {
        assert_eq!(
            store
                .begin_run(&session_row(session), &session.uid, None, None)
                .unwrap(),
            None
        );
    }

    /// The host a test records for a run whose cards it files. Nothing here signals it.
    const HOST: ProcessIdentity = ProcessIdentity {
        pid: 4242,
        birth: protocol::proc_identity::BirthIdentity {
            start_sec: 1,
            start_usec: 0,
        },
    };

    /// **A folder stays on the project list after its runs are removed and the
    /// daemon restarts**, it covers both agents' tables, keeps the newest start,
    /// and a run filed by a build without the table is folded in on open.
    #[test]
    fn projects_outlive_their_sessions_and_a_restart() {
        let (store, path) = temp_store();
        let file = |cwd: &str, at: &str, agent: AgentKind, lifecycle: Lifecycle| {
            let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-1");
            let mut row = session_row(&session);
            row.cwd = cwd.into();
            row.created_at = at.into();
            row.agent = agent;
            row.lifecycle = lifecycle;
            assert_eq!(
                store.begin_run(&row, &session.uid, None, None).unwrap(),
                None
            );
            session.uid
        };
        let old = file(
            "/w/app",
            "2026-11-01T00:00:00.000Z",
            AgentKind::Claude,
            Lifecycle::Exited,
        );
        file(
            "/w/api",
            "2026-11-02T00:00:00.000Z",
            AgentKind::Codex,
            Lifecycle::Live,
        );
        file(
            "/w/app",
            "2026-11-03T00:00:00.000Z",
            AgentKind::Codex,
            Lifecycle::Live,
        );
        // An older start written later must not move the folder back in time.
        file(
            "/w/app",
            "2026-10-01T00:00:00.000Z",
            AgentKind::Claude,
            Lifecycle::Live,
        );
        file(
            "",
            "2026-11-04T00:00:00.000Z",
            AgentKind::Claude,
            Lifecycle::Live,
        );

        let expected = vec![
            ProjectRow {
                path: "/w/app".into(),
                last_used: "2026-11-03T00:00:00.000Z".into(),
                last_agent: AgentKind::Codex,
            },
            ProjectRow {
                path: "/w/api".into(),
                last_used: "2026-11-02T00:00:00.000Z".into(),
                last_agent: AgentKind::Codex,
            },
        ];
        assert_eq!(store.projects().unwrap(), expected);

        assert!(matches!(
            store.delete_exited_session(&old).unwrap(),
            DeleteOutcome::Deleted { .. }
        ));
        drop(store);
        // A build without the table filed this run.
        let raw = Connection::open(&path).unwrap();
        let new = protocol::uid::new().unwrap();
        raw.execute(
            "INSERT INTO sessions(session_uid, session_id, cwd, lifecycle, created_at,
                                  updated_at, agent)
             VALUES (?1, 'cc-9', '/w/new', 'live', ?2, ?2, 'claude')",
            params![new, "2026-11-05T00:00:00.000Z"],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO run_hosts(session_uid, request_id) VALUES (?1, ?1)",
            params![new],
        )
        .unwrap();
        drop(raw);

        let store = Store::open(&path).unwrap();
        let paths: Vec<String> = store
            .projects()
            .unwrap()
            .into_iter()
            .map(|project| project.path)
            .collect();
        assert_eq!(paths, ["/w/new", "/w/app", "/w/api"]);
    }

    // ----------------------------------------------------------- pruning

    /// Seed one run in a given state, with `events` facts filed under it.
    fn seed_run(store: &Store, session: &SessionKey, lifecycle: Lifecycle, events: u32) {
        let mut row = session_row(session);
        row.lifecycle = lifecycle;
        assert_eq!(
            store.begin_run(&row, &session.uid, None, None).unwrap(),
            None
        );
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
        seed_run(&store, &spawning, Lifecycle::Starting, 2);
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

    // ------------------------------------------- agent-scoped session storage

    /// A Codex run.
    /// **An account a live run is on is not removed** (306 Slice D, minor 2): with a live Codex run recorded on A,
    /// removing A is refused with the sentence a person reads, and nothing is removed: while A is chosen, to choose
    /// another; once another is chosen, that its sessions move with their next message. Once the run has exited, the
    /// account is no longer in use.
    #[tokio::test]
    async fn an_account_a_live_run_is_on_is_not_removed() {
        let (store, _path) = temp_store();
        let a = store.add_account("codex", "A").unwrap();
        let run = key("CC", "cx-9");
        assert_eq!(
            store
                .begin_run(&codex_session_row(&run), &run.uid, None, Some(&a.id))
                .unwrap(),
            None
        );
        store.set_lifecycle(&run.uid, Lifecycle::Live).unwrap();
        assert!(store.account_in_use(&a.id).unwrap());
        for (chosen, words) in [
            (
                Some(a.id.as_str()),
                "Sessions are running on this account. Choose another account first.",
            ),
            (
                None,
                "Open sessions started on A move with their next message; end them or send to them first.",
            ),
        ] {
            assert!(store.activate_account("codex", chosen).unwrap());
            let refused = serde_json::to_value(
                crate::accounts::remove_with(
                    &store,
                    &crate::accounts::SignIns::default(),
                    &std::collections::HashMap::new(),
                    crate::accounts::Agents::default(),
                    "r1".into(),
                    a.id.clone(),
                )
                .await,
            )
            .unwrap();
            assert_eq!(
                (
                    refused["type"].as_str(),
                    refused["code"].as_str(),
                    refused["message"].as_str()
                ),
                (Some("accounts_refused"), Some("accounts_in_use"), Some(words)),
                "{refused}"
            );
            assert_eq!(store.accounts().unwrap().len(), 1, "nothing removed");
        }
        store.set_lifecycle(&run.uid, Lifecycle::Exited).unwrap();
        assert!(
            !store.account_in_use(&a.id).unwrap(),
            "an exited run uses it no longer"
        );
    }

    fn codex_session_row(session: &SessionKey) -> SessionRow {
        let mut row = session_row(session);
        row.agent = AgentKind::Codex;
        row.codex_thread_id = Some("th_ABC123".into());
        row
    }

    /// How many rows each physical table holds, so an assertion can say *where*
    /// a run is rather than only that the daemon can still find it.
    fn table_counts(store: &Store) -> (i64, i64) {
        let conn = store.read();
        let claude: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let codex: i64 = conn
            .query_row("SELECT COUNT(*) FROM codex_sessions", [], |row| row.get(0))
            .unwrap();
        (claude, codex)
    }

    /// **The whole point of the split.** A Codex run is written to
    /// `codex_sessions` and is not in `sessions` at all, while every read this
    /// daemon performs still finds it.
    #[test]
    fn a_codex_run_is_written_to_its_own_table_and_never_to_the_shared_one() {
        let (store, _path) = temp_store();
        let claude = key("AA", "cc-1");
        let codex = key("CX", "cx-1");
        begin(&store, &claude);
        // Begun as a start begins it, its thread recorded while it starts.
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Starting;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);
        assert!(store.record_hosted_thread(&codex.uid, "th_ABC123").unwrap());

        assert_eq!(
            table_counts(&store),
            (1, 1),
            "each agent's run belongs to exactly one table"
        );

        // And every read still answers for the whole fleet.
        let listed: Vec<String> = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|row| row.session_uid)
            .collect();
        assert!(listed.contains(&claude.uid) && listed.contains(&codex.uid));
        let found = store.get_session(&codex.uid).unwrap().expect("by uid");
        assert_eq!(found.agent, AgentKind::Codex);
        assert_eq!(found.codex_thread_id.as_deref(), Some("th_ABC123"));
        assert_eq!(found.cwd, "/tmp", "the row round-trips whole, not partly");
        assert_eq!(
            store
                .find_session("cx-1")
                .unwrap()
                .map(|row| row.session_uid),
            Some(codex.uid.clone()),
            "resolving by name reaches the Codex half too"
        );
    }

    /// A run begun naming its transcript, in `lifecycle`.
    fn with_transcript(store: &Store, session: &SessionKey, lifecycle: Lifecycle) {
        let mut row = session_row(session);
        row.lifecycle = lifecycle;
        row.transcript_path = Some(format!("/tmp/{}.jsonl", session.uid));
        assert_eq!(
            store.begin_run(&row, &session.uid, None, None).unwrap(),
            None
        );
    }

    fn transcript_of(store: &Store, session: &SessionKey) -> Option<String> {
        store
            .get_session(&session.uid)
            .unwrap()
            .unwrap()
            .transcript_path
    }

    /// **A database from before accounts takes them on, and keeps them** (N4 G2). Built as the last release left it
    /// (no `accounts`, no `account_selection`, no `run_hosts.account_id`) with a run in it, then opened by this
    /// build: accounts are added and chosen, and survive two more opens; the run from before resolves to its
    /// agent's Default; a run recorded on an account keeps it.
    #[test]
    fn a_database_from_before_accounts_takes_them_on_and_keeps_them() {
        let (store, path) = temp_store();
        let legacy = key("AA", "cc-1");
        assert_eq!(
            store
                .begin_run(&session_row(&legacy), &legacy.uid, None, None)
                .unwrap(),
            None
        );
        drop(store);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TABLE accounts; DROP TABLE account_selection; ALTER TABLE run_hosts DROP COLUMN account_id;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.run_account(&legacy.uid).unwrap(),
            None,
            "a run from before is on its Default"
        );
        let work = store.add_account("codex", "Work").unwrap();
        let home = store.add_account("claude", "Home").unwrap();
        assert!(store.activate_account("codex", Some(&work.id)).unwrap());
        assert!(
            !store.activate_account("claude", Some(&work.id)).unwrap(),
            "codex's account is not claude's"
        );
        assert!(store.activate_account("claude", Some(&home.id)).unwrap());
        let on_work = key("BB", "cx-1");
        assert_eq!(
            store
                .begin_run(
                    &codex_session_row(&on_work),
                    &on_work.uid,
                    None,
                    Some(&work.id)
                )
                .unwrap(),
            None
        );
        drop(store);
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.accounts().unwrap(), vec![work.clone(), home.clone()]);
            assert_eq!(store.active_account("codex").unwrap(), Some(work.clone()));
            assert_eq!(store.active_account("claude").unwrap(), Some(home.clone()));
            assert_eq!(store.run_account(&on_work.uid).unwrap(), Some(work.clone()));
            assert_eq!(store.run_account(&legacy.uid).unwrap(), None);
        }
        let store = Store::open(&path).unwrap();
        assert!(store.activate_account("codex", None).unwrap());
        assert_eq!(
            store.active_account("codex").unwrap(),
            None,
            "Default again"
        );
        assert_eq!(
            store.run_account(&on_work.uid).unwrap(),
            Some(work),
            "a choice alone changes no run's account"
        );
    }

    /// On the way up to `6`, every run that had ended forgets its transcript, once,
    /// and a running one keeps its own. Past the transition the gate is shut: a
    /// run whose end a later start has still to read keeps its transcript.
    #[test]
    fn runs_that_ended_before_version_6_forget_their_transcripts_once() {
        let (store, path) = temp_store();
        let (ended, live) = (key("EE", "cc-1"), key("BB", "cc-2"));
        with_transcript(&store, &ended, Lifecycle::Exited);
        with_transcript(&store, &live, Lifecycle::Live);
        drop(store);
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 5i64).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(transcript_of(&store, &ended), None);
        assert!(transcript_of(&store, &live).is_some());

        let later = key("FF", "cc-3");
        with_transcript(&store, &later, Lifecycle::Exited);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(transcript_of(&store, &later).is_some());
    }

    /// The refusal is narrow enough to leave Claude alone.
    ///
    /// A trigger written on the uid alone would refuse a write it must not: an
    /// ordinary Claude run. Its uid is not in `codex_sessions` and never will be,
    /// so the `EXISTS` is false and the statement is not examined — but that is
    /// the claim, and it is worth pinning.
    #[test]
    fn the_refusal_is_narrow_enough_to_leave_claude_alone() {
        let (store, _path) = temp_store();

        let claude = key("AA", "cc-1");
        begin(&store, &claude);
        assert_eq!(table_counts(&store), (1, 0));

        let codex = key("CX", "cx-1");
        assert_eq!(
            store
                .begin_run(&codex_session_row(&codex), &codex.uid, None, None)
                .unwrap(),
            None
        );
        assert_eq!(table_counts(&store), (1, 1));
    }

    /// Ending, deleting and pruning all reach a Codex run.
    ///
    /// The isolation is from the *old* daemon. This one owns both agents, and an
    /// operator who asks it to clean up is asking about their whole fleet. A
    /// Codex run that could never be marked `Exited` — the shape a dispatch that
    /// guessed the wrong table would produce — would be immortal and silent.
    #[test]
    fn this_daemon_still_ends_deletes_and_prunes_a_codex_run() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let doomed = key("CY", "cx-2");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);
        let mut second = codex_session_row(&doomed);
        second.lifecycle = Lifecycle::Live;
        assert_eq!(
            store.begin_run(&second, &doomed.uid, None, None).unwrap(),
            None
        );
        store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("one")))
            .unwrap();

        store.set_lifecycle(&codex.uid, Lifecycle::Exited).unwrap();
        assert_eq!(
            store.get_session(&codex.uid).unwrap().unwrap().lifecycle,
            Lifecycle::Exited,
            "a Codex run can be ended, or it can never be removed"
        );

        assert_eq!(
            store.delete_exited_session(&codex.uid).unwrap(),
            DeleteOutcome::Deleted { events: 1 }
        );
        assert_eq!(table_counts(&store), (0, 1));
        assert_eq!(store.count_events(&codex.uid).unwrap(), 0);

        // And the prune reaches the other one.
        store.set_lifecycle(&doomed.uid, Lifecycle::Exited).unwrap();
        let removed = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(
            removed
                .iter()
                .map(|p| p.session_uid.clone())
                .collect::<Vec<_>>(),
            vec![doomed.uid.clone()],
            "the prune this daemon runs is over the whole fleet"
        );
        assert_eq!(table_counts(&store), (0, 0));
    }

    /// The two tables must not drift apart.
    ///
    /// `all_sessions` selects the same columns from each, so a column added to
    /// one and forgotten on the other turns every session read into a prepare
    /// error at runtime. Read off the schema rather than restated, so a future
    /// `COLUMN_ADDITIONS` entry that names only `sessions` fails here.
    ///
    /// The view projects exactly the columns [`SessionRow`] carries, because
    /// `session_row_from` decodes it positionally.
    #[test]
    fn both_session_tables_have_one_shape() {
        let (store, _path) = temp_store();
        let conn = store.read();
        let columns = |table: &str| -> Vec<(String, String, i64)> {
            let mut info = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let rows = info
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows
        };
        assert_eq!(
            columns("sessions"),
            columns("codex_sessions"),
            "the two session tables disagree on name, type, order or primary key"
        );
        // And the view really does read both halves through the `SessionRow`
        // shape — the ten columns `session_row_from` decodes by position.
        let stmt = conn.prepare("SELECT * FROM all_sessions").unwrap();
        let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
        let projected = [
            "session_uid",
            "session_id",
            "cwd",
            "claude_session_id",
            "transcript_path",
            "lifecycle",
            "created_at",
            "updated_at",
            "agent",
            "codex_thread_id",
        ];
        assert_eq!(
            names, projected,
            "all_sessions does not project the SessionRow shape"
        );
        // The projection must be the physical shape, in its order, or the view
        // is naming columns the tables no longer declare in that order and
        // `session_row_from` decodes the wrong field.
        let declared: Vec<String> = columns("sessions").into_iter().map(|c| c.0).collect();
        assert_eq!(
            declared, projected,
            "the projected columns are not the columns of the table"
        );
    }

    /// Every "does this run still exist?" guard asks about the fleet.
    ///
    /// Six statements gate a write on the session being there, and each one
    /// answers a *question*, not a table. Pointed at Claude's half alone every
    /// one of them reads a live Codex run as gone, and five of the six say so by
    /// doing nothing and reporting success — an event that looks like a dedup
    /// miss, an answer never filed, a card retired, a `send_text` claim reported
    /// as taken without being taken. This asserts the store's own contract; it
    /// says nothing about which of these the daemon can reach today, which is
    /// the separate question
    /// `no_codex_row_reaches_a_table_a_rolled_back_daemon_sweeps_globally`
    /// answers.
    #[test]
    fn every_existence_guard_asks_about_the_whole_fleet() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);
        store.record_host(&codex.uid, &HOST, None).unwrap();
        let now = protocol::time::now_rfc3339();

        // `append_in_tx` — every fact either agent records.
        assert!(store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("one")))
            .unwrap()
            .is_some());

        // `orphan_event_count` — a Codex run's log is history with an owner, not
        // history whose provenance nobody understands.
        assert_eq!(store.orphan_event_count().unwrap(), 0);

        // `record_answer` — and the replay that proves the first one landed
        // rather than being silently dropped by a guard that said "no session".
        let answer = outcome("req-1", AnswerDecision::Allow);
        assert!(matches!(
            store
                .record_answer(&codex.uid, "req-1", "hash", &answer)
                .unwrap(),
            LedgerWrite::Recorded
        ));
        assert!(matches!(
            store
                .record_answer(&codex.uid, "req-1", "hash", &answer)
                .unwrap(),
            LedgerWrite::Existing
        ));

        // The shared `pending_approvals` no longer accepts a Codex run, and that is
        // the one guard in the six that changed. It used to, on purpose, while
        // the daemon's own refusal was the thing keeping the shared table clean
        // — scaffolding that stood only because nothing produced a Codex card.
        // The approval observer is that producer, so the table split landed and
        // the refusal moved into the schema, where a rolled-back binary keeps
        // it. Refused rather than silently dropped: a store guard that reported
        // success and lost the card is the failure this whole seam exists to
        // prevent in the other direction.
        let refused = store
            .commit_host_card(
                &codex.uid,
                &codex.name,
                &HOST,
                1,
                "req-2",
                Some(("{}", 0)),
                &[],
            )
            .expect_err("a Codex card must not land in the shared table");
        assert!(
            format!("{refused:#}").contains("codex_pending_approvals"),
            "the refusal must name where the card belongs: {refused:#}"
        );

        // And the table it moved to takes the same run, asking the same fleet
        // question the other five guards ask.
        assert_eq!(
            insert_codex_card(&store, &codex, "req-2", "exec-1", "tu-1", 0, "{}").unwrap(),
            CodexCardOutcome::Filed
        );

        // `claim_text_mutation` and `claim_mutation` still accept a Codex run on
        // purpose: their tables are deliberately NOT split, because nothing
        // writes a Codex row into either. Splitting a table ahead of its
        // producer is the speculative half-surface this design refuses.
        assert_eq!(
            store
                .claim_text_mutation(&codex.uid, "req-3", "hash", None, &now)
                .unwrap(),
            TextClaim::Claimed
        );
        // Asked a second time, because `Claimed` is what this returns when the
        // guarded `INSERT` writes nothing as well as when it writes the row —
        // the one place in the six where the return value cannot tell a claim
        // that was durably taken from one that silently was not. Only the replay
        // can: a claim that landed comes back `Indeterminate`, an unwritten one
        // comes back `Claimed` again and the caller types twice.
        assert_eq!(
            store
                .claim_text_mutation(&codex.uid, "req-3", "hash", None, &now)
                .unwrap(),
            TextClaim::Indeterminate {
                started_at: now.clone()
            },
            "the claim was reported taken but nothing was written"
        );
        assert_eq!(
            store
                .claim_mutation(
                    "compose",
                    &codex.uid,
                    "req-4",
                    &ClaimedMaterial {
                        thread_id: "th_ABC123".into(),
                        generation: 1,
                        route: "turn_start".into(),
                        target_turn_id: None,
                        claimed_hash: "hash".into(),
                        question_decision: None,
                    },
                    &now,
                )
                .unwrap(),
            MutationClaim::Claimed
        );
    }

    /// Four tables a rolled-back v0.6.0 daemon reaches **without walking a
    /// `sessions` row**, named once so no reader has to rediscover the list.
    ///
    /// `pending_approvals`, `answer_claims` and `text_mutations` it reads
    /// globally, and `recover_text_mutations` *writes* one of them — every
    /// `applying` row becomes `indeterminate`, whoever it belongs to. `answers`
    /// it queries by `request_id` alone. So the table-name isolation the rest of
    /// this schema rests on does not cover any of them.
    const GLOBALLY_SWEPT_TABLES: [&str; 4] = [
        "pending_approvals",
        "answer_claims",
        "text_mutations",
        "answers",
    ];

    /// **The store's half of the tripwire for the obligation the shared tables
    /// leave open**, and it is the weaker half on purpose.
    ///
    /// The four tables in [`GLOBALLY_SWEPT_TABLES`] are not split. What keeps
    /// them empty of Codex rows is not the store — the store's own API will
    /// happily write all four for a Codex run, and
    /// `every_existence_guard_asks_about_the_whole_fleet` proves it does, which
    /// is deliberate: a guard that silently dropped a Codex card would leave a
    /// test like this green for ever and ship the bug. What keeps them empty is
    /// an explicit refusal at the daemon's producers: `Daemon::send_text` and
    /// `Daemon::answer` each refuse a non-Claude row before any durable write,
    /// and `send_text_to_a_codex_session_is_refused_before_it_claims_anything` and
    /// `answering_a_codex_card_is_refused_before_the_claim` drive those real
    /// paths and read these same four tables back off the daemon's own database
    /// file. Those are the tests that can see a producer; this one cannot, and
    /// saying so is the point of this comment.
    ///
    /// What this one still adds is the floor underneath them: the paths a Codex
    /// run *does* travel in this build — facts — must not reach these tables by
    /// some route that has nothing to do with the gated producers.
    #[test]
    fn no_codex_row_reaches_a_table_a_rolled_back_daemon_sweeps_globally() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);

        // Everything a Codex run reaches in the store as things stand: facts,
        // and the generalized mutation ledger built for it.
        store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("one")))
            .unwrap();
        store
            .append_event(&pending(&codex, EventKind::TurnComplete, None))
            .unwrap();
        assert_eq!(store.count_events(&codex.uid).unwrap(), 2);

        let conn = store.read();
        for table in GLOBALLY_SWEPT_TABLES {
            let leaked: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table}
                          WHERE session_uid IN (SELECT session_uid FROM codex_sessions)"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                leaked, 0,
                "a Codex run now has a row in {table}, which a rolled-back v0.6 daemon reaches \
                 without going through a sessions row — and, for text_mutations, rewrites. That \
                 table has to become agent-scoped before the producer that put this row here can \
                 ship. See `create_schema`."
            );
        }
    }

    /// A Codex card lands in the agent-scoped table, and the shared one refuses
    /// it — in the SCHEMA, so a rollback keeps the refusal.
    ///
    /// This is the half of the shared-table obligation that `create_schema` said
    /// would come due "the day somebody edits those refusals". The daemon-side gate
    /// could only ever protect a daemon that still had it; a trigger protects
    /// the database from the binary, which is the direction a rollback runs in.
    ///
    /// **Mutation:** drop `pending_approvals_refuse_codex_card` from
    /// `create_schema` and the `expect_err` goes green-then-red — the shared
    /// insert succeeds and the card sits exactly where a v0.6.0 sweep finds it.
    #[test]
    fn a_codex_card_is_refused_by_the_shared_table_and_kept_by_the_scoped_one() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);
        store.record_host(&codex.uid, &HOST, None).unwrap();

        let shared = store.commit_host_card(
            &codex.uid,
            &codex.name,
            &HOST,
            1,
            "derived-1",
            Some(("{}", 10)),
            &[],
        );
        let err = shared.expect_err("the shared table must refuse a Codex card");
        assert!(
            matches!(
                err.downcast_ref::<rusqlite::Error>()
                    .and_then(|e| e.sqlite_error_code()),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            ),
            "refused, but not by the constraint that is supposed to do it: {err:#}"
        );

        insert_codex_card(&store, &codex, "derived-1", "exec-1", "tu-1", 10, "{}").unwrap();

        // A Claude run is untouched by the refusal: it is the uid that decides,
        // and a trigger that over-refused would take the Claude path with it.
        let claude = key("AA", "cc-1");
        begin(&store, &claude);
        store.record_host(&claude.uid, &HOST, None).unwrap();
        store
            .commit_host_card(
                &claude.uid,
                &claude.name,
                &HOST,
                1,
                "req-1",
                Some(("{}", 11)),
                &[],
            )
            .unwrap()
            .expect("the cursor moved");
        assert!(store
            .list_pending_approvals()
            .unwrap()
            .iter()
            .any(|card| card.session_uid == claude.uid));
    }

    /// **A re-delivered approval rebinds its card instead of minting a second.**
    ///
    /// Measured on codex 0.153: when a link reconnects and resumes, the
    /// app-server re-sends the outstanding `requestApproval` with a
    /// byte-identical `itemId` — and with its per-connection `id` back at `0`,
    /// which is why the wire id identifies nothing and `request_id` is derived
    /// from `(thread_id, item_id)` instead. That derivation is what makes the
    /// second sighting a conflict rather than a new row.
    ///
    /// **A re-delivery rebinds one card; a re-delivery that changed the question
    /// is refused.**
    ///
    /// The derived request id follows `(thread_id, item_id)`, so the same
    /// approval re-delivered to a reconnecting link lands on the row it already
    /// has. What that lands *as* is the whole of this test.
    ///
    /// It used to be `ON CONFLICT … DO UPDATE SET card, turn_id` — a silent
    /// refresh — and that split the card in half: the row took the new content
    /// while the already-filed `ApprovalRequest` and every connected phone kept
    /// the old one, with no event and no ring to say the question had changed.
    /// So the stored row is compared instead: byte-identical is a rebind and
    /// writes nothing, and anything else is refused with the stored card left
    /// standing. A content-changing re-delivery is a shape no capture has ever
    /// produced, so there is nothing measured to reconcile it against.
    ///
    /// **Mutations:** put the `DO UPDATE SET` back and the stored-card assertion
    /// goes red; compare only `card` and the `turn_id` assertion goes red.
    #[test]
    fn a_redelivered_codex_approval_rebinds_one_card() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);

        assert_eq!(
            insert_codex_card(
                &store,
                &codex,
                "derived-1",
                "exec-1",
                "tu-1",
                10,
                r#"{"v":1}"#
            )
            .unwrap(),
            CodexCardOutcome::Filed
        );
        // The same item, seen again after a reconnect: same derived id, same
        // question, byte for byte. The measured case.
        assert_eq!(
            insert_codex_card(
                &store,
                &codex,
                "derived-1",
                "exec-1",
                "tu-1",
                99,
                r#"{"v":1}"#
            )
            .unwrap(),
            CodexCardOutcome::Rebound,
            "an identical re-delivery is a rebind, and writes nothing"
        );

        let open = store.list_pending_approvals().unwrap();
        assert_eq!(open.len(), 1, "a re-delivery must not mint a second card");
        assert_eq!(open[0].card, r#"{"v":1}"#);
        assert_eq!(
            store.codex_pending_approvals(&codex.uid).unwrap()[0].created_ms,
            10,
            "and it is the same question, not a newer one"
        );

        // A re-delivery whose CONTENT changed. Refused, and the stored card is
        // untouched — the phone is holding it and the filed request event
        // describes it.
        assert_eq!(
            insert_codex_card(
                &store,
                &codex,
                "derived-1",
                "exec-1",
                "tu-1",
                10,
                r#"{"v":2}"#
            )
            .unwrap(),
            CodexCardOutcome::ContentChanged
        );
        // A re-delivery naming a different turn is the same refusal: an item
        // belongs to its turn, so this is not the same thing arriving twice.
        assert_eq!(
            insert_codex_card(
                &store,
                &codex,
                "derived-1",
                "exec-1",
                "tu-9",
                10,
                r#"{"v":1}"#
            )
            .unwrap(),
            CodexCardOutcome::ContentChanged
        );
        let after = store.list_pending_approvals().unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].card, r#"{"v":1}"#,
            "the refused re-delivery must not have moved the stored card"
        );

        // A genuinely different item is a genuinely different card.
        assert_eq!(
            insert_codex_card(&store, &codex, "derived-2", "exec-2", "tu-2", 30, "{}").unwrap(),
            CodexCardOutcome::Filed
        );
        assert_eq!(store.list_pending_approvals().unwrap().len(), 2);

        // **And the one-item-one-card rule is enforced in SQL, not merely
        // implied by the derivation above it.** `request_id` is derived from
        // `(thread_id, item_id)`, so a second id for one item can only mean the
        // derivation broke — the exact failure that would put two cards on the
        // phone for one question, and the one thing the comparison above cannot
        // catch, because a broken derivation finds no row to compare against.
        let err = insert_codex_card(
            &store,
            &codex,
            "derived-1-forked",
            "exec-1",
            "tu-2",
            40,
            "{}",
        )
        .expect_err("one wire item must not become two cards");
        assert!(
            matches!(
                err.downcast_ref::<rusqlite::Error>()
                    .and_then(|e| e.sqlite_error_code()),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            ),
            "refused, but not by the uniqueness that is supposed to do it: {err:#}"
        );
        assert_eq!(store.list_pending_approvals().unwrap().len(), 2);
    }

    /// Recovery answers for both agents, and deleting a run takes its Codex
    /// cards with it.
    ///
    /// Isolation from a rolled-back binary is not a licence to leak rows here:
    /// `codex_pending_approvals` is in `SESSION_SCOPED_TABLES`, so THIS daemon
    /// sweeps it exactly as it sweeps the Claude table.
    ///
    /// **Mutation:** point `list_pending_approvals` back at `pending_approvals`
    /// and the fleet assertion drops to one; remove `codex_pending_approvals`
    /// from `SESSION_SCOPED_TABLES` and the post-delete assertion finds the
    /// orphan.
    #[test]
    fn the_fleet_read_sees_both_agents_and_a_delete_takes_the_codex_cards() {
        let (store, _path) = temp_store();
        let claude = key("AA", "cc-1");
        begin(&store, &claude);
        store.record_host(&claude.uid, &HOST, None).unwrap();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);

        store
            .commit_host_card(
                &claude.uid,
                &claude.name,
                &HOST,
                1,
                "req-1",
                Some(("{}", 1)),
                &[],
            )
            .unwrap()
            .expect("the cursor moved");
        insert_codex_card(&store, &codex, "derived-1", "exec-1", "tu-1", 2, "{}").unwrap();

        let open = store.list_pending_approvals().unwrap();
        assert_eq!(open.len(), 2, "recovery must answer for the whole fleet");

        // The lifecycle predicate that guards deletion lives in the SQL, so the
        // run has to have actually ended before it can be deleted.
        store.set_lifecycle(&codex.uid, Lifecycle::Exited).unwrap();
        store.delete_exited_session(&codex.uid).unwrap();
        let after = store.list_pending_approvals().unwrap();
        assert_eq!(after.len(), 1, "a deleted Codex run leaves no orphan card");
        assert_eq!(after[0].session_uid, claude.uid);
    }

    /// A Codex card written by a build that had no scoped table is taken out of
    /// the shared one on the way up.
    ///
    /// It cannot be re-keyed — the shared row carries no `itemId`, and the
    /// connection that could name one is gone — so it is removed rather than
    /// left where a rolled-back v0.6.0 enumerates and deletes it anyway. The
    /// run and its events are untouched.
    ///
    /// **Mutation:** make `needs_codex_card_move` return `false` and the
    /// post-migration count stays at one, in the shared table.
    #[test]
    fn a_codex_card_left_in_the_shared_table_is_taken_out_on_the_way_up() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        assert_eq!(store.begin_run(&row, &codex.uid, None, None).unwrap(), None);
        store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("one")))
            .unwrap();
        drop(store);

        // Staged with the object that would refuse it removed, so the row is
        // the one a build without this schema really could have written.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TRIGGER pending_approvals_refuse_codex_card")
            .unwrap();
        conn.execute(
            "INSERT INTO pending_approvals(session_uid, session_id, request_id, card,
                                           generation, created_ms)
             VALUES(?1, 'cx-1', 'stale', '{}', 1, 0)",
            params![codex.uid],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&path).unwrap();
        let conn = store.read();
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_approvals WHERE session_uid = ?1",
                params![codex.uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "the misfiled card must not survive the migration");
        drop(conn);
        assert_eq!(
            store.count_events(&codex.uid).unwrap(),
            1,
            "the run's own facts are untouched"
        );
    }

    /// Insert one Codex card exactly as the approval observer will: a direct
    /// write to the agent-scoped table, with the identity the wire supplies.
    ///
    /// SQL rather than a `Store` method on purpose — the Rust writer lands with
    /// the observer that calls it, and a store API with no producer is the
    /// speculative half-surface this design refuses. What these tests are about
    /// is the SCHEMA: the table, the view over both agents, the uniqueness that
    /// makes a re-delivery rebind, and the trigger that keeps the shared table
    /// clean while a rolled-back binary is the one running.
    fn insert_codex_card(
        store: &Store,
        session: &SessionKey,
        request_id: &str,
        item_id: &str,
        turn_id: &str,
        created_ms: i64,
        card: &str,
    ) -> Result<CodexCardOutcome> {
        // Through the production statement, not a copy of it. A hand-written
        // duplicate here would let the two drift, and the drift would be
        // invisible: this helper is the only thing asserting that a re-delivery
        // *refreshes* a card rather than being refused, so it has to be asserting
        // it about the statement the daemon actually runs.
        let mut conn = store.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = upsert_codex_card_in_tx(
            &tx,
            &CodexPendingApprovalRow {
                session_uid: session.uid.clone(),
                session_id: session.name.clone(),
                request_id: request_id.to_string(),
                card: card.to_string(),
                generation: 1,
                created_ms,
                thread_id: "th-1".to_string(),
                turn_id: turn_id.to_string(),
                item_id: item_id.to_string(),
                family: "commandExecution".to_string(),
            },
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Deleted means deleted: no fact can be filed under the uid.
    #[test]
    fn a_deleted_uid_is_never_written_to() {
        let (store, _path) = temp_store();
        let doomed = key("AA", "cc-1");
        seed_run(&store, &doomed, Lifecycle::Exited, 1);
        assert_eq!(
            store.delete_exited_session(&doomed.uid).unwrap(),
            DeleteOutcome::Deleted { events: 1 }
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
        // The daemon's veto, for runs it still holds live state for — an
        // approval open. That should be impossible for a row marked `Exited`,
        // which is exactly why it is checked: if it ever happens it is a bug,
        // and deleting the evidence is the worst available response to a bug.
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
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: protocol::time::now_rfc3339(),
            detail: None,
            indeterminate: false,
        };
        store
            .record_answer(&dead.uid, "toolu_1", "hash", &outcome)
            .unwrap();
        store.record_host(&dead.uid, &HOST, None).unwrap();
        store
            .commit_host_card(
                &dead.uid,
                &dead.name,
                &HOST,
                1,
                "toolu_2",
                Some(("{}", 0)),
                &[],
            )
            .unwrap()
            .expect("the cursor moved");
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
            .claim_text_mutation(
                &dead.uid,
                "toolu_4",
                "hash",
                None,
                &protocol::time::now_rfc3339(),
            )
            .unwrap();

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

    const MAIL_LIMITS: MailLimits = MailLimits {
        unread_ms: 24 * HOUR_MS,
        per_pair_per_hour: 3,
        settled_ms: 7 * 24 * HOUR_MS,
    };

    /// A store with two runs, `R` (cc-1) and `S` (cc-2), on this computer.
    fn mail_store() -> (Store, TempDb, SessionKey, SessionKey) {
        let (store, path) = temp_store();
        let (receiver, sender) = (key("R1", "cc-1"), key("S1", "cc-2"));
        for session in [&receiver, &sender] {
            begin(&store, session);
        }
        (store, path, receiver, sender)
    }

    /// One message `id` from `sender_uid` on `computer` (empty: this one) to `to`.
    fn intake(
        id: &str,
        to: &SessionKey,
        computer: &str,
        sender_uid: &str,
        text: &str,
    ) -> MailIntake {
        MailIntake {
            message_id: id.into(),
            context_id: format!("ctx-{id}"),
            in_reply_to: None,
            session_uid: to.uid.clone(),
            sender_computer: computer.into(),
            sender_uid: sender_uid.into(),
            sender: format!("{sender_uid}@studio"),
            sender_label: "cc-2@studio".into(),
            sender_agent: "Codex".into(),
            sender_project: "api".into(),
            text: text.into(),
        }
    }

    /// The `mail_sent` row of a message `sender` sent to cc-1 on this computer.
    fn sent(id: &str, sender: &SessionKey) -> SentMail {
        SentMail {
            message_id: id.into(),
            context_id: format!("ctx-{id}"),
            in_reply_to: None,
            session_uid: sender.uid.clone(),
            recipient: "cc-1@studio".into(),
            recipient_computer: String::new(),
            text: "hi".into(),
        }
    }

    fn offered_ids(offer: &MailOffer) -> Vec<String> {
        offer
            .messages
            .iter()
            .map(|m| m.message_id.clone())
            .collect()
    }

    /// **One copy per message id, and a bounded number per sender and receiver.**
    /// A retried id keeps nothing new and says where the first one stands; the
    /// fourth message in an hour from one sender to one receiver is refused, while
    /// another sender, another receiver and the same pair an hour later are not.
    #[test]
    fn mail_is_kept_once_per_id_and_limited_per_pair() {
        let (store, _path, receiver, sender) = mail_store();
        let other = key("R2", "cc-3");
        begin(&store, &other);
        let t0 = 1_000_000_000;
        let take = |id: &str, to: &SessionKey, computer: &str, from: &str, at: i64| {
            store
                .take_mail(&intake(id, to, computer, from, "hi"), at, MAIL_LIMITS)
                .unwrap()
        };
        for id in ["m1", "m2", "m3"] {
            assert_eq!(take(id, &receiver, "", &sender.uid, t0), MailTaken::Kept);
        }
        assert_eq!(
            take("m1", &receiver, "", &sender.uid, t0),
            MailTaken::Known(protocol::ipc::MailState::Waiting),
            "a retried id keeps nothing new"
        );
        assert_eq!(
            take("m4", &receiver, "", &sender.uid, t0 + 1),
            MailTaken::RateLimited
        );
        assert_eq!(
            take("m5", &receiver, "desktop", &sender.uid, t0 + 1),
            MailTaken::Kept,
            "the same uid on another computer is another sender"
        );
        assert_eq!(take("m6", &other, "", &sender.uid, t0 + 1), MailTaken::Kept);
        assert_eq!(
            take("m7", &receiver, "", &sender.uid, t0 + HOUR_MS),
            MailTaken::Kept,
            "the limit frees up an hour on"
        );
        assert_eq!(
            take("m8", &key("ZZ", "cc-9"), "", &sender.uid, t0),
            MailTaken::NoSession
        );
    }

    /// **Offered is not read.** Messages offered to one call are offered to no
    /// other; an offer that never reached its agent is unread again, and so is
    /// every offer found at start-up; a confirmed offer is read, and its local
    /// sender sees that once, in the next offer it is given.
    #[test]
    fn offered_mail_is_read_only_once_its_offer_is_confirmed() {
        let (store, path, receiver, sender) = mail_store();
        let now = 1_000_000_000;
        for id in ["m1", "m2"] {
            store.file_sent_mail(&sent(id, &sender), now).unwrap();
            store
                .take_mail(
                    &intake(id, &receiver, "", &sender.uid, "hi"),
                    now,
                    MAIL_LIMITS,
                )
                .unwrap();
            store
                .note_sent_mail(id, protocol::ipc::MailState::Waiting, None, true)
                .unwrap();
        }
        let first = store.offer_mail(&receiver.uid, "o1", now).unwrap();
        assert_eq!(offered_ids(&first), ["m1", "m2"], "oldest first");
        assert!(!first.more_pending);
        assert!(
            store
                .offer_mail(&receiver.uid, "o2", now)
                .unwrap()
                .messages
                .is_empty(),
            "a second call at the same time is offered none of them"
        );
        store.release_mail("o1").unwrap();
        assert_eq!(
            offered_ids(&store.offer_mail(&receiver.uid, "o3", now).unwrap()),
            ["m1", "m2"],
            "an offer that never reached its agent is unread again"
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.recover_mail().unwrap(), 2);
        assert_eq!(
            store.read_mail("o3").unwrap(),
            0,
            "a recovered offer reads nothing"
        );
        assert_eq!(
            offered_ids(&store.offer_mail(&receiver.uid, "o4", now).unwrap()),
            ["m1", "m2"]
        );
        assert_eq!(store.read_mail("o4").unwrap(), 2);
        assert!(store
            .offer_mail(&receiver.uid, "o5", now)
            .unwrap()
            .messages
            .is_empty());

        let told = store.offer_mail(&sender.uid, "o6", now).unwrap();
        let states: Vec<_> = told
            .updates
            .iter()
            .map(|u| (u.message_id.as_str(), u.state))
            .collect();
        assert_eq!(
            states,
            [
                ("m1", protocol::ipc::MailState::Read),
                ("m2", protocol::ipc::MailState::Read)
            ]
        );
        store.release_mail("o6").unwrap();
        assert_eq!(
            store
                .offer_mail(&sender.uid, "o7", now)
                .unwrap()
                .updates
                .len(),
            2,
            "an update offered but not confirmed is given again"
        );
        store.read_mail("o7").unwrap();
        assert!(
            store
                .offer_mail(&sender.uid, "o8", now)
                .unwrap()
                .updates
                .is_empty(),
            "each change is given once"
        );
    }

    /// **A batch holds at most 16 messages and 32 KiB of text**, and says when
    /// more wait.
    #[test]
    fn an_offer_is_bounded_and_says_more_wait() {
        let (store, _path, receiver, _sender) = mail_store();
        let now = 1_000_000_000;
        let limits = MailLimits {
            per_pair_per_hour: 100,
            ..MAIL_LIMITS
        };
        for n in 0..17 {
            store
                .take_mail(
                    &intake(
                        &format!("s{n:02}"),
                        &receiver,
                        "",
                        &format!("sender{n}"),
                        "hi",
                    ),
                    now,
                    limits,
                )
                .unwrap();
        }
        let offer = store.offer_mail(&receiver.uid, "o1", now).unwrap();
        assert_eq!(offer.messages.len(), protocol::ipc::MAIL_BATCH_MESSAGES);
        assert!(offer.more_pending);
        store.read_mail("o1").unwrap();
        let rest = store.offer_mail(&receiver.uid, "o2", now).unwrap();
        assert_eq!(offered_ids(&rest), ["s16"]);
        assert!(!rest.more_pending);
        store.read_mail("o2").unwrap();

        let full = "x".repeat(protocol::ipc::MAIL_TEXT_BYTES);
        for n in 0..5 {
            store
                .take_mail(
                    &intake(&format!("b{n}"), &receiver, "", "big", &full),
                    now,
                    limits,
                )
                .unwrap();
        }
        let offer = store.offer_mail(&receiver.uid, "o3", now).unwrap();
        assert_eq!(
            offer.messages.len(),
            protocol::ipc::MAIL_BATCH_BYTES / protocol::ipc::MAIL_TEXT_BYTES
        );
        assert!(offer.more_pending);
    }

    /// **Unread mail expires a day after it was taken, and not before**, and is
    /// never offered once expired; mail for a run that ended is ended. The sender
    /// on this computer sees both.
    #[test]
    fn unread_mail_expires_after_a_day_and_ends_with_its_run() {
        let (store, _path, receiver, sender) = mail_store();
        let t0 = 1_000_000_000;
        for id in ["m1", "m2"] {
            store.file_sent_mail(&sent(id, &sender), t0).unwrap();
            store
                .take_mail(
                    &intake(id, &receiver, "", &sender.uid, "hi"),
                    t0,
                    MAIL_LIMITS,
                )
                .unwrap();
        }
        let day = MAIL_LIMITS.unread_ms;
        assert_eq!(
            store
                .offer_mail(&receiver.uid, "o1", t0 + day - 1)
                .unwrap()
                .messages
                .len(),
            2
        );
        store.release_mail("o1").unwrap();
        assert!(
            store
                .offer_mail(&receiver.uid, "o2", t0 + day)
                .unwrap()
                .messages
                .is_empty(),
            "not offered at the day itself"
        );
        store.sweep_mail(t0 + day - 1, MAIL_LIMITS).unwrap();
        assert_eq!(
            store
                .offer_mail(&receiver.uid, "o3", t0 + day - 1)
                .unwrap()
                .messages
                .len(),
            2,
            "a second before the day, still unread"
        );
        store.release_mail("o3").unwrap();
        let other = key("R3", "cc-4");
        begin(&store, &other);
        store
            .take_mail(
                &intake("m3", &other, "", &sender.uid, "hi"),
                t0,
                MAIL_LIMITS,
            )
            .unwrap();
        store.file_sent_mail(&sent("m3", &sender), t0).unwrap();
        assert_eq!(store.end_mail(&other.uid).unwrap(), 1);
        store.sweep_mail(t0 + day, MAIL_LIMITS).unwrap();
        let states: Vec<_> = store
            .offer_mail(&sender.uid, "o4", t0 + day)
            .unwrap()
            .updates
            .into_iter()
            .map(|u| (u.message_id, u.state))
            .collect();
        assert_eq!(
            states,
            [
                ("m1".to_string(), protocol::ipc::MailState::Expired),
                ("m2".to_string(), protocol::ipc::MailState::Expired),
                ("m3".to_string(), protocol::ipc::MailState::Ended),
            ]
        );
        store
            .set_lifecycle(&receiver.uid, Lifecycle::Exited)
            .unwrap();
        store
            .take_mail(
                &intake("m4", &receiver, "", "late", "hi"),
                t0 + day,
                MAIL_LIMITS,
            )
            .unwrap();
        store.sweep_mail(t0 + day, MAIL_LIMITS).unwrap();
        assert!(
            store.untold_mail().unwrap().is_empty(),
            "a sender on this computer is told at once"
        );
        assert_eq!(
            store
                .offer_mail(&receiver.uid, "o5", t0 + day)
                .unwrap()
                .messages
                .len(),
            0,
            "an ended run's mail is never offered"
        );
    }

    /// **A sender on another computer is told each change until it takes it**, and
    /// a sent message's state only moves forward.
    #[test]
    fn a_remote_sender_is_told_each_change_and_states_only_move_forward() {
        let (store, _path, receiver, sender) = mail_store();
        let now = 1_000_000_000;
        store
            .take_mail(
                &intake("m1", &receiver, "desktop", "remote-uid", "hi"),
                now,
                MAIL_LIMITS,
            )
            .unwrap();
        assert!(
            store.untold_mail().unwrap().is_empty(),
            "waiting was its answer"
        );
        store.offer_mail(&receiver.uid, "o1", now).unwrap();
        store.read_mail("o1").unwrap();
        let untold = store.untold_mail().unwrap();
        assert_eq!(
            untold,
            [UntoldMail {
                message_id: "m1".into(),
                sender_computer: "desktop".into(),
                state: protocol::ipc::MailState::Read,
            }]
        );
        store
            .told_mail("m1", protocol::ipc::MailState::Read)
            .unwrap();
        assert!(store.untold_mail().unwrap().is_empty());

        store.file_sent_mail(&sent("m2", &sender), now).unwrap();
        store
            .note_sent_mail("m2", protocol::ipc::MailState::Read, None, false)
            .unwrap();
        store
            .note_sent_mail("m2", protocol::ipc::MailState::Waiting, None, false)
            .unwrap();
        assert_eq!(
            store.offer_mail(&sender.uid, "o2", now).unwrap().updates[0].state,
            protocol::ipc::MailState::Read,
            "a late answer does not move it back"
        );
    }

    /// **At most one wake at a time, and only for unread mail** (M-07). More mail
    /// while a wake is outstanding adds none; a turn end clears it; a wake that could
    /// not be issued is released; read mail asks for none.
    #[test]
    fn one_wake_is_outstanding_at_a_time_and_only_for_unread_mail() {
        let (store, _path, receiver, sender) = mail_store();
        let now = 1_000_000_000;
        let uid = receiver.uid.as_str();
        assert!(
            !store.claim_mail_wake(uid, now, false).unwrap(),
            "no mail, no wake"
        );
        let take = |id: &str| {
            store
                .take_mail(
                    &intake(id, &receiver, "", &sender.uid, "hi"),
                    now,
                    MAIL_LIMITS,
                )
                .unwrap()
        };
        take("m1");
        assert!(store.claim_mail_wake(uid, now, false).unwrap());
        take("m2");
        assert!(
            !store.claim_mail_wake(uid, now, false).unwrap(),
            "one is outstanding"
        );
        store.release_mail_wake(uid).unwrap();
        assert!(
            store.claim_mail_wake(uid, now, false).unwrap(),
            "a released wake is gone"
        );
        store.offer_mail(uid, "o1", now).unwrap();
        assert!(
            !store.claim_mail_wake(uid, now, false).unwrap(),
            "mail being read is not unread"
        );
        store.read_mail("o1").unwrap();
        store.mail_turn_ended(uid, now, 3).unwrap();
        assert!(!store.claim_mail_wake(uid, now, false).unwrap(), "all read");
        assert!(store.runs_with_unread_mail(now).unwrap().is_empty());
        take("m3");
        assert_eq!(store.runs_with_unread_mail(now).unwrap(), [uid]);
        assert!(store.claim_mail_wake(uid, now, false).unwrap());
    }

    /// **Three turn ends that leave woken mail unread stop the wakes** (M-08), and a
    /// person's turn or new mail starts them again. A turn the person starts while
    /// mail waits is told of it.
    #[test]
    fn wakes_stop_after_three_unanswered_turns_until_a_person_or_new_mail() {
        let (store, _path, receiver, sender) = mail_store();
        let now = 1_000_000_000;
        let uid = receiver.uid.as_str();
        let limits = MailLimits {
            per_pair_per_hour: 100,
            ..MAIL_LIMITS
        };
        store
            .take_mail(&intake("m1", &receiver, "", &sender.uid, "hi"), now, limits)
            .unwrap();
        for _ in 0..3 {
            assert!(store.claim_mail_wake(uid, now, false).unwrap());
            store.mail_turn_ended(uid, now, 3).unwrap();
        }
        assert!(
            !store.claim_mail_wake(uid, now, false).unwrap(),
            "no fourth wake for the same unread mail"
        );
        assert!(
            store.mail_prompt(uid, now).unwrap(),
            "the person's turn is told of it"
        );
        store.mail_turn_ended(uid, now, 3).unwrap();
        assert!(
            store.claim_mail_wake(uid, now, false).unwrap(),
            "a person's turn resumes them"
        );
        for _ in 0..3 {
            store.mail_turn_ended(uid, now, 3).unwrap();
            store.claim_mail_wake(uid, now, false).unwrap();
        }
        store.release_mail_wake(uid).unwrap();
        assert!(
            !store.claim_mail_wake(uid, now, false).unwrap(),
            "stopped again"
        );
        store
            .take_mail(&intake("m2", &receiver, "", &sender.uid, "hi"), now, limits)
            .unwrap();
        assert!(
            store.claim_mail_wake(uid, now, false).unwrap(),
            "new mail resumes them"
        );
    }

    /// **A Claude turn holds its wake until it ends** (the T5 rule): mail taken while
    /// a turn runs wakes no one until the turn's end is read. The turn a wake starts
    /// is that wake's own: it resumes nothing and is not told again.
    #[test]
    fn a_running_claude_turn_holds_its_wake_until_the_turn_ends() {
        let (store, _path, receiver, sender) = mail_store();
        let now = 1_000_000_000;
        let uid = receiver.uid.as_str();
        let end = |at: i64| {
            store.mail_turn_ended(uid, at, 3).unwrap();
            store.mail_turn_marks(uid, None, Some(at)).unwrap();
        };
        assert!(
            !store.mail_prompt(uid, now).unwrap(),
            "no mail to be told of"
        );
        store
            .take_mail(
                &intake("m1", &receiver, "", &sender.uid, "hi"),
                now,
                MAIL_LIMITS,
            )
            .unwrap();
        assert!(
            !store.claim_mail_wake(uid, now, true).unwrap(),
            "the turn is running"
        );
        end(now + 1);
        assert!(
            store.claim_mail_wake(uid, now + 1, true).unwrap(),
            "woken at its end"
        );
        assert!(
            !store.mail_prompt(uid, now + 2).unwrap(),
            "the wake's own turn is not told again"
        );
        end(now + 3);
        assert!(
            store.claim_mail_wake(uid, now + 3, true).unwrap(),
            "unread after it: woken again"
        );
    }

    /// **Start-up recovery gives back only what was mid-read** (B38-a): a message
    /// already read stays read.
    #[test]
    fn recovery_never_makes_read_mail_unread() {
        let (store, path, receiver, sender) = mail_store();
        let now = 1_000_000_000;
        for id in ["m1", "m2"] {
            store
                .take_mail(
                    &intake(id, &receiver, "", &sender.uid, "hi"),
                    now,
                    MAIL_LIMITS,
                )
                .unwrap();
            store
                .offer_mail(&receiver.uid, &format!("o-{id}"), now)
                .unwrap();
        }
        store.read_mail("o-m1").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.recover_mail().unwrap(), 1);
        assert_eq!(
            offered_ids(&store.offer_mail(&receiver.uid, "o3", now).unwrap()),
            ["m2"]
        );
    }

    /// **The receiver's own expiry tells the sender** (M23-b, M23-e), for a
    /// sender told `waiting` when it sent, as a real local send is; and at the day
    /// itself, not after it.
    #[test]
    fn receiver_side_expiry_tells_a_sender_that_was_told_waiting() {
        let (store, _path, receiver, sender) = mail_store();
        let t0 = 1_000_000_000;
        store.file_sent_mail(&sent("m1", &sender), t0).unwrap();
        store
            .take_mail(
                &intake("m1", &receiver, "", &sender.uid, "hi"),
                t0,
                MAIL_LIMITS,
            )
            .unwrap();
        store
            .note_sent_mail("m1", protocol::ipc::MailState::Waiting, None, true)
            .unwrap();
        store
            .sweep_mail(t0 + MAIL_LIMITS.unread_ms, MAIL_LIMITS)
            .unwrap();
        let updates = store
            .offer_mail(&sender.uid, "o1", t0 + MAIL_LIMITS.unread_ms)
            .unwrap()
            .updates;
        assert_eq!(
            updates
                .iter()
                .map(|u| (u.message_id.as_str(), u.state))
                .collect::<Vec<_>>(),
            [("m1", protocol::ipc::MailState::Expired)]
        );
    }

    /// **A message taken whose answer never comes is expired in its sender's
    /// outbox** (final round): the receiver's run pruned while the sending computer
    /// was away drops the word that it ended. Once the receiver's own unread time has
    /// surely run out — it may have taken the message up to one unread time after it
    /// was written — the sender reads it expired, and not before.
    #[test]
    fn a_taken_message_whose_answer_never_comes_expires_for_its_sender() {
        let (store, _path, _receiver, sender) = mail_store();
        let t0 = 1_000_000_000;
        store.file_sent_mail(&sent("m1", &sender), t0).unwrap();
        store
            .note_sent_mail("m1", protocol::ipc::MailState::Waiting, None, true)
            .unwrap();
        let updates = |at: i64| {
            store.sweep_mail(at, MAIL_LIMITS).unwrap();
            store
                .offer_mail(&sender.uid, &format!("o{at}"), at)
                .unwrap()
                .updates
                .iter()
                .map(|u| (u.message_id.clone(), u.state))
                .collect::<Vec<_>>()
        };
        assert_eq!(updates(t0 + 2 * MAIL_LIMITS.unread_ms - 1), []);
        assert_eq!(
            updates(t0 + 2 * MAIL_LIMITS.unread_ms),
            [("m1".to_string(), protocol::ipc::MailState::Expired)]
        );
    }

    /// **Settled mail is dropped a week on, and not before** (B20-a), on both sides.
    #[test]
    fn settled_mail_is_dropped_a_week_on() {
        let (store, _path, receiver, sender) = mail_store();
        let t0 = 1_000_000_000;
        store.file_sent_mail(&sent("m1", &sender), t0).unwrap();
        store
            .take_mail(
                &intake("m1", &receiver, "", &sender.uid, "hi"),
                t0,
                MAIL_LIMITS,
            )
            .unwrap();
        store.offer_mail(&receiver.uid, "o1", t0).unwrap();
        store.read_mail("o1").unwrap();
        store.offer_mail(&sender.uid, "o2", t0).unwrap();
        store.read_mail("o2").unwrap();
        let kept = |store: &Store| {
            (
                store.mail_context(&receiver.uid, "m1").unwrap().is_some(),
                store.sent_mail_computer("m1").unwrap().is_some(),
            )
        };
        store
            .sweep_mail(t0 + MAIL_LIMITS.settled_ms - 1, MAIL_LIMITS)
            .unwrap();
        assert_eq!(kept(&store), (true, true), "kept until the week is up");
        store
            .sweep_mail(t0 + MAIL_LIMITS.settled_ms, MAIL_LIMITS)
            .unwrap();
        assert_eq!(kept(&store), (false, false), "dropped once it is");
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
            // Both session-identity tables, and `codex_sessions` for a reason
            // worth stating rather than pattern-matching: it is keyed by
            // `session_uid` like the child tables are, but it is not filed
            // *under* a session — it IS the session. Adding it to
            // `SESSION_SCOPED_TABLES` would make both removal paths delete the
            // identity row in their unguarded loop, ahead of and instead of the
            // guarded `DELETE` that restates `lifecycle = 'exited'` and insists
            // it matched exactly one row. That guard is the whole safety
            // property of the prune.
            if name == "sessions" || name == "codex_sessions" || name.starts_with("sqlite_") {
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
        begin(&store, &first);
        for expected in 1..=3u64 {
            let event = store
                .append_event(&pending(&first, EventKind::ToolCall, None))
                .unwrap()
                .unwrap();
            assert_eq!(event.seq, expected);
        }
        // A second session starts its own numbering.
        let second = key("AB", "cc-2");
        begin(&store, &second);
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
        begin(&store, &dead);
        let live = key("AB", "cc-1");
        begin(&store, &live);
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
        assert_eq!(store.begin_run(&row, &old.uid, None, None).unwrap(), None);

        let mut row = session_row(&new);
        row.created_at = "2026-07-02T00:00:00.000Z".into();
        row.lifecycle = Lifecycle::Exited;
        assert_eq!(store.begin_run(&row, &new.uid, None, None).unwrap(), None);

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
        begin(&store, &session);
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
        begin(&store, &session);
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
        begin(&store, &session);
        let mut hook = pending(&session, EventKind::ToolCall, Some("uuid-1"));
        hook.source = Source::Hook;
        let mut transcript = pending(&session, EventKind::ToolCall, Some("uuid-1"));
        transcript.source = Source::Transcript;
        assert!(store.append_event(&hook).unwrap().is_some());
        assert!(store.append_event(&transcript).unwrap().is_some());
    }

    /// **The effort ends with the run's process, read back after a restart** (decision 210):
    /// an effort filed before the run's `session_end` is not the run's effort, one after it is.
    #[test]
    fn a_runs_effort_is_read_back_only_from_after_its_end() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let effort = |level: &str| {
            let mut notice = pending(&session, EventKind::Notification, None);
            notice.payload = serde_json::json!({"notification_type": "model_set", "effort": level});
            notice
        };
        store.append_event(&effort("high")).unwrap();
        let mut end = pending(&session, EventKind::SessionEnd, None);
        end.payload = serde_json::json!({"exit_code": 1, "reason": "Stopped: its process ended"});
        store.append_event(&end).unwrap();
        let facts = store.session_facts(&session.uid).unwrap();
        assert_eq!(facts.effort, None);
        assert_eq!(facts.ended, 2);
        store.append_event(&effort("low")).unwrap();
        assert_eq!(
            store
                .session_facts(&session.uid)
                .unwrap()
                .effort
                .map(|fact| fact.value),
            Some("low".to_string())
        );
    }

    /// **The effort a run's process ended with is read back as it was folded live**: the effort before the newest
    /// end and after the end before it; none when that process named none.
    #[test]
    fn the_effort_a_run_ended_with_is_read_back_from_its_own_process() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let effort = |level: &str| {
            let mut notice = pending(&session, EventKind::Notification, None);
            notice.payload = serde_json::json!({"notification_type": "model_set", "effort": level});
            notice
        };
        let end = || {
            let mut end = pending(&session, EventKind::SessionEnd, None);
            end.payload =
                serde_json::json!({"exit_code": 1, "reason": "Stopped: its process ended"});
            end
        };
        let mut live = protocol::facts::SessionFacts::default();
        for event in [effort("high"), end()] {
            live.observe(&store.append_event(&event).unwrap().unwrap());
        }
        let read = store.session_facts(&session.uid).unwrap();
        assert_eq!(
            read.ended_with.map(|fact| fact.value).as_deref(),
            Some("high")
        );
        assert_eq!(store.session_facts(&session.uid).unwrap(), live);
        live.observe(&store.append_event(&end()).unwrap().unwrap());
        let read = store.session_facts(&session.uid).unwrap();
        assert_eq!(read.ended_with, None, "the second process named none");
        assert_eq!(read, live);
    }

    /// **A run's newest model receipt is read back after a restart as it was folded live**: the
    /// newest `Set model to …` or `Kept model as …` line claude gave, colour codes and all, whatever
    /// inits and other messages were filed since; and its model and effort with it, read from local
    /// commands' output only, never from a user's own words that look like one.
    #[test]
    fn a_runs_model_receipt_is_read_back_as_it_was_folded() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let said = |content: &str| {
            let mut message = pending(&session, EventKind::UserMessage, None);
            message.payload = json!({"message": {"role": "user", "content": content}});
            message
        };
        let init = |model: &str| {
            let mut start = pending(&session, EventKind::SessionStart, None);
            start.payload = json!({"model": model});
            start
        };
        let mut live = protocol::facts::SessionFacts::default();
        let mut receipts = Vec::new();
        for event in [
            said("hello"),
            init("claude-opus-5-5"),
            said("<local-command-stdout>\u{1b}[31mSet model to\u{1b}[0m sonnet</local-command-stdout>"),
            said("thanks"),
            init("claude-sonnet-5-5"),
            said("<local-command-caveat>Caveat: Set model to opus</local-command-caveat>"),
            said("<local-command-stdout>Kept model as opus</local-command-stdout>"),
            said("<local-command-stdout>Set effort level to low (this session only)</local-command-stdout>"),
            init("claude-opus-5-5"),
            said("Set effort level to high and Set model to haiku, please"),
            said("go on"),
        ] {
            live.observe(&store.append_event(&event).unwrap().unwrap());
            let read = store.session_facts(&session.uid).unwrap();
            assert_eq!(read, live);
            receipts.push(read.model_receipt.map(|fact| fact.value));
        }
        let sonnet = Some("sonnet".to_string());
        let opus = Some("opus".to_string());
        assert_eq!(
            receipts,
            [
                None,
                None,
                sonnet.clone(),
                sonnet.clone(),
                sonnet.clone(),
                sonnet,
                opus.clone(),
                opus.clone(),
                opus.clone(),
                opus.clone(),
                opus
            ]
        );
        let read = store.session_facts(&session.uid).unwrap();
        assert_eq!(
            (
                read.model.map(|fact| fact.value),
                read.effort.map(|fact| fact.value)
            ),
            (Some("claude-opus-5-5".into()), Some("low".into()))
        );
    }

    /// **A Codex run's settings are read back after a restart as they were folded live**: its
    /// model, effort and mode from its newest `codex_settings` fact, a start's answer and a
    /// later report alike.
    #[test]
    fn a_codex_runs_settings_are_read_back_as_they_were_folded() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let settings = |model: &str, effort: &str, approval: &str, sandbox: &str| {
            let mut report = pending(
                &session,
                EventKind::Other(protocol::facts::CODEX_SETTINGS.into()),
                None,
            );
            report.source = Source::Codex;
            report.payload = serde_json::json!({"model": model, "effort": effort,
                "approval_policy": approval, "sandbox": sandbox, "collaboration_mode": "default"});
            report
        };
        let mut live = protocol::facts::SessionFacts::default();
        for report in [
            settings("gpt-6.1-sol", "low", "on-request", "workspaceWrite"),
            settings("gpt-6-astra", "high", "never", "dangerFullAccess"),
        ] {
            live.observe(&store.append_event(&report).unwrap().unwrap());
        }
        let read = store.session_facts(&session.uid).unwrap();
        assert_eq!(read, live);
        assert_eq!(
            (
                read.model.map(|fact| fact.value),
                read.effort.map(|fact| fact.value),
                read.permission_mode.map(|fact| fact.value),
            ),
            (
                Some("gpt-6-astra".into()),
                Some("high".into()),
                Some("full-access".into())
            )
        );
    }

    /// **An Antigravity run's settings are read back after a restart as they were folded live**: its model, effort
    /// and mode from its newest `acp_settings` naming each, its opening's and a later change of the mode alone, and
    /// the effort its process held at its end.
    #[test]
    fn an_antigravity_runs_settings_are_read_back_as_they_were_folded() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let settings = |payload: serde_json::Value| {
            let mut report = pending(
                &session,
                EventKind::Other(protocol::facts::ACP_SETTINGS.into()),
                None,
            );
            report.source = Source::Acp;
            report.payload = payload;
            report
        };
        let mut live = protocol::facts::SessionFacts::default();
        for report in [
            settings(
                serde_json::json!({"model": "gemini-4-neon", "effort": "high", "mode": "default"}),
            ),
            settings(serde_json::json!({"mode": "plan"})),
        ] {
            live.observe(&store.append_event(&report).unwrap().unwrap());
        }
        let read = store.session_facts(&session.uid).unwrap();
        assert_eq!(read, live);
        assert_eq!(
            (
                read.model.map(|fact| fact.value),
                read.effort.map(|fact| fact.value),
                read.permission_mode.map(|fact| fact.value),
            ),
            (
                Some("gemini-4-neon".into()),
                Some("high".into()),
                Some("plan".into())
            )
        );
        live.observe(
            &store
                .append_event(&pending(&session, EventKind::SessionEnd, None))
                .unwrap()
                .unwrap(),
        );
        let read = store.session_facts(&session.uid).unwrap();
        assert_eq!(
            read.ended_with.map(|fact| fact.value).as_deref(),
            Some("high")
        );
        assert_eq!(store.session_facts(&session.uid).unwrap(), live);
    }

    #[test]
    fn events_without_ids_are_never_collapsed() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
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
        begin(&store, &session);
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
        begin(&store, &session);
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
        begin(&store, &session);
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
            answered_on: None,
            applied_via: AnswerPath::HookReturn,
            resolved_at: protocol::time::now_rfc3339(),
            detail: None,
            indeterminate: false,
        }
    }

    #[test]
    fn ledger_is_insert_once_and_keeps_the_original() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        begin(&store, &session);
        let first = outcome("toolu_1", AnswerDecision::Allow);
        assert!(matches!(
            store
                .record_answer(&session.uid, "toolu_1", "hash-a", &first)
                .unwrap(),
            LedgerWrite::Recorded
        ));

        // A second, *different* decision must not overwrite the first.
        let second = outcome("toolu_1", AnswerDecision::Deny);
        assert!(matches!(
            store
                .record_answer(&session.uid, "toolu_1", "hash-a", &second)
                .unwrap(),
            LedgerWrite::Existing
        ));
        let (payload_hash, stored) = store.get_answer(&session.uid, "toolu_1").unwrap().unwrap();
        assert_eq!(stored.decision, AnswerDecision::Allow);
        assert_eq!(payload_hash, "hash-a");
    }

    #[test]
    fn an_answer_in_one_run_is_not_an_answer_in_the_next() {
        // The approval-safety half of the session uid change. Both runs are
        // called `cc-1`; an approval answered in the first must not make the
        // second's card come back as an already-applied duplicate.
        let (store, _path) = temp_store();
        let dead = key("AA", "cc-1");
        begin(&store, &dead);
        let live = key("AB", "cc-1");
        begin(&store, &live);

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
        begin(&store, &old);
        let new = key("AB", "cc-1");
        begin(&store, &new);
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
        begin(&store, &session);
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

    /// A hosted Claude run: written with its start request, before anything spawns.
    fn begun(store: &Store, tag: &str, request_id: &str) -> SessionKey {
        let session = key(tag, &format!("run-{tag}"));
        let mut row = session_row(&session);
        row.lifecycle = Lifecycle::Starting;
        assert_eq!(
            store
                .begin_run(&row, request_id, Some(r#"{"type":"start"}"#), None)
                .unwrap(),
            None
        );
        session
    }

    /// A hosted Codex run, in Codex's table: written with its start request and no start.
    fn begun_codex(store: &Store, tag: &str, request_id: &str) -> SessionKey {
        let session = key(tag, &format!("run-{tag}"));
        let mut row = session_row(&session);
        row.lifecycle = Lifecycle::Starting;
        row.agent = AgentKind::Codex;
        assert_eq!(store.begin_run(&row, request_id, None, None).unwrap(), None);
        session
    }

    /// **A hosted Codex run's thread and its `live` are written only while it is still
    /// starting**, so its first send never revives a run that has ended meanwhile.
    #[test]
    fn a_hosted_codex_runs_thread_and_live_are_written_only_while_it_is_starting() {
        let (store, _path) = temp_store();
        let run = begun_codex(&store, "AA", "req-1");
        assert!(store.record_hosted_thread(&run.uid, "thr-1").unwrap());
        assert!(store
            .advance_lifecycle(&run.uid, Lifecycle::Starting, Lifecycle::Live)
            .unwrap());
        assert!(!store.record_hosted_thread(&run.uid, "thr-2").unwrap());
        let row = store.get_session(&run.uid).unwrap().unwrap();
        assert_eq!(
            (row.lifecycle, row.codex_thread_id.as_deref()),
            (Lifecycle::Live, Some("thr-1"))
        );

        let ended = begun_codex(&store, "BB", "req-2");
        store.set_lifecycle(&ended.uid, Lifecycle::Failed).unwrap();
        assert!(!store
            .advance_lifecycle(&ended.uid, Lifecycle::Starting, Lifecycle::Live)
            .unwrap());
        assert!(!store.record_hosted_thread(&ended.uid, "thr-3").unwrap());
        let row = store.get_session(&ended.uid).unwrap().unwrap();
        assert_eq!(
            (row.lifecycle, row.codex_thread_id),
            (Lifecycle::Failed, None)
        );
    }

    #[test]
    fn a_second_start_with_the_same_request_gets_the_first_run() {
        let (store, _path) = temp_store();
        let first = begun(&store, "AA", "req-1");
        let mut second = session_row(&key("BB", "run-BB"));
        second.lifecycle = Lifecycle::Starting;
        let existing = store
            .begin_run(&second, "req-1", Some(r#"{"type":"start"}"#), None)
            .unwrap()
            .expect("the first run");
        assert_eq!(existing.session_uid, first.uid);
        assert_eq!(existing.lifecycle, Lifecycle::Starting);
        assert!(store.get_session(&second.session_uid).unwrap().is_none());
    }

    /// **B5:** a hosted run's send still claimed when ccd stopped is left claimed with
    /// its text, for its host's replay to settle and file.
    #[test]
    fn a_hosted_runs_claimed_send_is_kept_for_its_host() {
        let (store, _path) = temp_store();
        let hosted = begun(&store, "BB", "req-1");
        let now = protocol::time::now_rfc3339();
        assert!(matches!(
            store
                .claim_text_mutation(&hosted.uid, "r-1", "hash", Some("kept"), &now)
                .unwrap(),
            TextClaim::Claimed
        ));
        let status: String = store
            .read()
            .query_row(
                "SELECT status FROM text_mutations WHERE session_uid = ?1",
                [&hosted.uid],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "applying");
        assert_eq!(
            store.host_sends(&hosted.uid).unwrap(),
            [("r-1".to_string(), "kept".to_string())]
        );
    }

    /// A run ccd hosts is not removable while it runs: only once it has ended,
    /// and then its hosted row goes with it.
    #[test]
    fn a_hosted_run_is_removed_only_once_it_has_ended() {
        let (store, _path) = temp_store();
        let session = begun(&store, "AA", "req-1");
        for lifecycle in [Lifecycle::Starting, Lifecycle::Live] {
            store.set_lifecycle(&session.uid, lifecycle).unwrap();
            assert_eq!(
                store.delete_exited_session(&session.uid).unwrap(),
                DeleteOutcome::NotExited {
                    lifecycle: lifecycle_str(lifecycle).into()
                }
            );
        }
        store
            .set_lifecycle(&session.uid, Lifecycle::Failed)
            .unwrap();
        assert!(matches!(
            store.delete_exited_session(&session.uid).unwrap(),
            DeleteOutcome::Deleted { .. }
        ));
        assert_eq!(store.hosted_run_for_request("req-1").unwrap(), None);
    }

    /// **A run with a switch of account is never pruned, and deleted only once its move failed** (306, decision 311
    /// v4; gate 9): due or moving, it reads live to every device until a message has started it; one whose move
    /// failed reads ended, and the person may delete it.
    #[test]
    fn a_run_with_a_switch_is_pruned_never_and_deleted_once_its_move_failed() {
        let (store, _path) = temp_store();
        let moving = begun(&store, "AA", "req-1");
        let failed = begun(&store, "AB", "req-2");
        for (run, state) in [
            (&moving, Store::SWITCH_MOVED),
            (&failed, Store::SWITCH_FAILED),
        ] {
            store.set_lifecycle(&run.uid, Lifecycle::Exited).unwrap();
            store.set_switch_state(&run.uid, Some(state)).unwrap();
        }
        assert_eq!(store.prune_exited_sessions(&[], false).unwrap(), []);
        for state in [Store::SWITCH_WAITING, Store::SWITCH_MOVED] {
            store.set_switch_state(&moving.uid, Some(state)).unwrap();
            assert!(matches!(
                store.delete_exited_session(&moving.uid).unwrap(),
                DeleteOutcome::NotExited { .. }
            ));
        }
        assert!(matches!(
            store.delete_exited_session(&failed.uid).unwrap(),
            DeleteOutcome::Deleted { .. }
        ));
        store.set_switch_state(&moving.uid, None).unwrap();
        assert_eq!(store.prune_exited_sessions(&[], false).unwrap().len(), 1);
    }

    /// **A switch's relaunch a restart finds unfinished goes back to ended, still switching** (306, decision 311 v4;
    /// gate 8): one with no host recorded, and one whose host was found gone, are `exited`, for their next message,
    /// not `failed`; any other start is failed as before.
    #[test]
    fn a_switchs_relaunch_left_unfinished_goes_back_to_ended() {
        let (store, _path) = temp_store();
        let unhosted = begun(&store, "AA", "req-1");
        let hosted = begun(&store, "AB", "req-2");
        let other = begun(&store, "AC", "req-3");
        for run in [&unhosted, &hosted] {
            store
                .set_switch_state(&run.uid, Some(Store::SWITCH_MOVED))
                .unwrap();
        }
        store.record_host(&hosted.uid, &HOST, None).unwrap();
        assert!(store.fail_hosted_start(&hosted.uid, &HOST).unwrap());
        assert_eq!(store.fail_unhosted_starts().unwrap(), 2);
        let lifecycle = |run: &SessionKey| store.get_session(&run.uid).unwrap().unwrap().lifecycle;
        assert_eq!(
            (lifecycle(&unhosted), lifecycle(&hosted), lifecycle(&other)),
            (Lifecycle::Exited, Lifecycle::Exited, Lifecycle::Failed)
        );
        assert_eq!(
            store.switch_state(&unhosted.uid).unwrap(),
            Some(Store::SWITCH_MOVED)
        );
    }

    #[test]
    fn prune_removes_a_failed_run_and_keeps_a_starting_one() {
        let (store, _path) = temp_store();
        let failed = begun(&store, "AA", "req-1");
        let starting = begun(&store, "BB", "req-2");
        store.set_lifecycle(&failed.uid, Lifecycle::Failed).unwrap();
        let removed = store.prune_exited_sessions(&[], false).unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].session_uid, failed.uid);
        assert_eq!(store.hosted_run_for_request("req-1").unwrap(), None);
        assert!(store.get_session(&starting.uid).unwrap().is_some());
    }

    #[test]
    fn two_runs_of_one_name_are_two_rows_not_an_overwrite() {
        let (store, _path) = temp_store();
        let first = key("AA", "cc-1");
        let second = key("AB", "cc-1");
        begin(&store, &first);
        begin(&store, &second);
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
            .insert_device(id, &name, token_hash, &protocol::time::now_rfc3339(), None)
            .unwrap();
    }

    /// The active device a token authenticates as, or `None` when it is revoked or unknown:
    /// the shape the handshake's `device_by_token_hash` once returned, for tests that only
    /// care whether the token still admits a device. Tests that distinguish revoked from
    /// unknown match `TokenAuth` directly.
    fn auth(store: &Store, token_hash: &str) -> Option<DeviceRow> {
        match store.device_by_token_hash(token_hash).unwrap() {
            TokenAuth::Active(device) => Some(*device),
            TokenAuth::Revoked | TokenAuth::Missing => None,
        }
    }

    #[test]
    fn a_device_token_authenticates_until_it_is_revoked() {
        let (store, _path) = temp_store();
        add_device(&store, "d1a2b3c4", "iPhone", "token-hash-1");
        assert_eq!(auth(&store, "token-hash-1").unwrap().name, "iPhone");

        assert_eq!(
            store
                .revoke_device("d1a2b3c4", &protocol::time::now_rfc3339())
                .unwrap(),
            ["d1a2b3c4"]
        );
        // A revoked token is recognised as the blocked pairing it still holds, not as a
        // missing one: the device keeps its token so Unblock can readmit it, and the
        // handshake tells it "revoked" (wait) rather than "unauthorized" (pair anew).
        assert!(
            matches!(
                store.device_by_token_hash("token-hash-1").unwrap(),
                TokenAuth::Revoked
            ),
            "a revoked token is recognised as revoked, and does not authenticate"
        );
        // Revoking twice is a no-op, not an error.
        assert!(store
            .revoke_device("d1a2b3c4", &protocol::time::now_rfc3339())
            .unwrap()
            .is_empty());
        // Unblock readmits the same token — the pairing it already held, no new code.
        store.unblock_device("d1a2b3c4").unwrap();
        assert_eq!(
            auth(&store, "token-hash-1").unwrap().name,
            "iPhone",
            "an unblocked token authenticates again on its held pairing"
        );
    }

    // ------------------------------------------------- Tailscale sign-in

    const NOW: &str = "2026-11-08T00:00:00.000Z";

    /// A Tailscale sign-in on `node`, under a name nothing else holds.
    fn admit(store: &Store, id: &str, node: &str) -> Admission {
        store
            .admit_tailnet_device(id, &format!("dev {id}"), &format!("hash-{id}"), NOW, node)
            .unwrap()
    }

    fn row(store: &Store, id: &str) -> DeviceRow {
        store
            .list_devices()
            .unwrap()
            .into_iter()
            .find(|row| row.device_id == id)
            .unwrap()
    }

    /// The database a build from before Tailscale sign-in left (its `devices` as
    /// shipped, [`SHIPPED_DEVICES_DDL`]), holding one row for each `(id, revoked)`
    /// in `rows`, reopened by this build.
    fn upgraded_with(rows: &[(&str, bool)]) -> (Store, TempDb) {
        let (store, path) = temp_store();
        drop(store);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("DROP INDEX devices_name; DROP TABLE devices;")
                .unwrap();
            conn.execute_batch(SHIPPED_DEVICES_DDL).unwrap();
            for (id, revoked) in rows {
                conn.execute(
                    "INSERT INTO devices(device_id, name, token_hash, created_at, revoked_at)
                     VALUES(?1, 'dev ' || ?1, 'hash-' || ?1, ?2, ?3)",
                    params![id, NOW, revoked.then_some(NOW)],
                )
                .unwrap();
            }
        }
        (Store::open(&path).unwrap(), path)
    }

    /// **Revoking one device revokes every device on the same Tailscale device, in
    /// one statement, and clears each one's push tuple.** A device on no Tailscale
    /// device is revoked alone.
    #[test]
    fn a_revoke_takes_every_device_on_the_same_tailscale_device_and_its_push_tuple() {
        let (store, _path) = temp_store();
        assert_eq!(admit(&store, "a", "n1"), Admission::Admitted);
        assert_eq!(admit(&store, "b", "n1"), Admission::Admitted);
        assert_eq!(admit(&store, "c", "n2"), Admission::Admitted);
        store
            .insert_device("d", "dev d", "hash-d", NOW, None)
            .unwrap();
        for id in ["a", "b", "c", "d"] {
            store
                .set_push_token(id, &format!("tok-{id}"), "production", Some("cred"))
                .unwrap();
        }

        let mut revoked = store.revoke_device("a", NOW).unwrap();
        revoked.sort();
        assert_eq!(revoked, ["a", "b"]);
        let mut pushed: Vec<String> = store
            .push_targets()
            .unwrap()
            .into_iter()
            .map(|target| target.device_id)
            .collect();
        pushed.sort();
        assert_eq!(pushed, ["c", "d"]);
        let raw = store.read();
        let tuples: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM devices WHERE device_id IN ('a', 'b')
                   AND (push_token IS NOT NULL OR push_environment IS NOT NULL
                        OR push_credential IS NOT NULL)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(raw);
        assert_eq!(tuples, 0, "no push tuple outlives the revoke");

        assert_eq!(store.revoke_device("d", NOW).unwrap(), ["d"]);
        assert!(row(&store, "c").revoked_at.is_none());
        assert_eq!(admit(&store, "e", "n1"), Admission::Blocked);
        assert_eq!(admit(&store, "f", "n2"), Admission::Admitted);
    }

    /// The token stamp reaches revoked rows as well as active ones, and never
    /// overwrites a node already recorded.
    #[test]
    fn a_token_records_its_tailscale_device_once_revoked_or_not() {
        let (store, _path) = temp_store();
        store
            .insert_device("a", "dev a", "hash-a", NOW, None)
            .unwrap();
        store
            .insert_device("b", "dev b", "hash-b", NOW, None)
            .unwrap();
        store.revoke_device("b", NOW).unwrap();
        store.stamp_tailnet_node("hash-a", "n1").unwrap();
        store.stamp_tailnet_node("hash-b", "n2").unwrap();
        store.stamp_tailnet_node("hash-a", "n9").unwrap();
        assert_eq!(row(&store, "a").tailnet_node.as_deref(), Some("n1"));
        assert_eq!(row(&store, "b").tailnet_node.as_deref(), Some("n2"));
        assert_eq!(store.blocked_tailnet_nodes().unwrap(), ["n2"]);
    }

    /// **A revocation from before the upgrade pauses Tailscale sign-in.** The
    /// upgrade marks every row it finds, and a revoked marked row with no node is
    /// one this daemon cannot place. Unblocking it, or its own old token arriving
    /// from a Tailscale device, lifts the pause.
    #[test]
    fn a_device_revoked_before_the_upgrade_pauses_sign_in_until_it_is_placed_or_unblocked() {
        let (store, _path) = upgraded_with(&[("old", true), ("lost", true)]);
        assert!(row(&store, "old").paired_before_nodes);
        assert_eq!(admit(&store, "a", "n1"), Admission::Paused);

        store.stamp_tailnet_node("hash-old", "n7").unwrap();
        assert_eq!(
            admit(&store, "a", "n1"),
            Admission::Paused,
            "one is still unplaced"
        );
        assert_eq!(store.unblock_device("lost").unwrap(), 1);
        assert_eq!(admit(&store, "a", "n1"), Admission::Admitted);
        assert_eq!(admit(&store, "b", "n7"), Admission::Blocked);
    }

    /// **A device paired after the upgrade is never marked**, so revoking it
    /// while it has no node blocks its token and nothing else: here, the Mac app's
    /// own loopback pairing.
    #[test]
    fn a_device_paired_after_the_upgrade_revoked_without_a_node_pauses_nothing() {
        let (store, _path) = upgraded_with(&[]);
        store
            .insert_device("local", "dev local", "hash-local", NOW, None)
            .unwrap();
        store.revoke_device("local", NOW).unwrap();
        assert!(!row(&store, "local").paired_before_nodes);
        assert_eq!(admit(&store, "a", "n1"), Admission::Admitted);
    }

    /// **A device paired before the upgrade and revoked after it, before it
    /// ever connected through Tailscale, pauses sign-in too**, and still does
    /// after a restart.
    #[test]
    fn a_device_paired_before_the_upgrade_and_revoked_after_it_pauses_sign_in() {
        let (store, path) = upgraded_with(&[("phone", false)]);
        assert!(row(&store, "phone").paired_before_nodes);
        assert_eq!(admit(&store, "a", "n1"), Admission::Admitted);
        store.revoke_device("phone", NOW).unwrap();
        assert_eq!(admit(&store, "b", "n2"), Admission::Paused);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            admit(&store, "b", "n2"),
            Admission::Paused,
            "after a restart"
        );
        assert!(
            row(&store, "phone").paired_before_nodes,
            "marked once, not re-marked away"
        );
        assert!(
            !row(&store, "a").paired_before_nodes,
            "rows after the upgrade stay unmarked"
        );
    }

    /// **Unblock restores revoked devices only, and the whole Tailscale device
    /// at once**, each with the token it held.
    #[test]
    fn unblock_restores_every_revoked_device_on_the_tailscale_device_and_no_other() {
        let (store, _path) = temp_store();
        admit(&store, "a", "n1");
        admit(&store, "b", "n1");
        store
            .insert_device("d", "dev d", "hash-d", NOW, None)
            .unwrap();
        store.revoke_device("d", NOW).unwrap();
        assert_eq!(
            store.unblock_device("a").unwrap(),
            0,
            "an active device is not blocked"
        );
        store.revoke_device("a", NOW).unwrap();
        assert_eq!(store.unblock_device("a").unwrap(), 2);
        for id in ["a", "b"] {
            let device = auth(&store, &format!("hash-{id}"));
            assert_eq!(device.map(|d| d.device_id).as_deref(), Some(id));
        }
        assert!(
            row(&store, "d").revoked_at.is_some(),
            "another device stays out"
        );
        assert_eq!(admit(&store, "c", "n1"), Admission::Admitted);
    }

    /// **A Tailscale sign-in and a revoke of the same device never leave it
    /// signed in.** Raced from two threads, 200 times: the revoke lands wholly
    /// before the sign-in, which is refused, or wholly after it, which revokes
    /// the row the sign-in made.
    #[test]
    fn a_sign_in_racing_a_revoke_of_its_device_never_survives_it() {
        let (store, _path) = temp_store();
        let store = std::sync::Arc::new(store);
        for round in 0..200 {
            let node = format!("n{round}");
            let first = format!("first-{round}");
            store
                .insert_device(&first, &first, &format!("hash-{first}"), NOW, Some(&node))
                .unwrap();
            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
            let sign_in = {
                let (store, start, node) = (
                    std::sync::Arc::clone(&store),
                    std::sync::Arc::clone(&start),
                    node.clone(),
                );
                std::thread::spawn(move || {
                    start.wait();
                    admit(&store, &format!("second-{round}"), &node)
                })
            };
            let revoke = {
                let (store, start) = (std::sync::Arc::clone(&store), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    store.revoke_device(&first, NOW).unwrap()
                })
            };
            sign_in.join().unwrap();
            revoke.join().unwrap();
            let active = store
                .list_devices()
                .unwrap()
                .into_iter()
                .filter(|row| {
                    row.tailnet_node.as_deref() == Some(&node) && row.revoked_at.is_none()
                })
                .count();
            assert_eq!(active, 0, "round {round}: a device survived its revoke");
        }
    }

    #[test]
    fn an_unknown_token_authenticates_nothing() {
        let (store, _path) = temp_store();
        add_device(&store, "d1", "iPhone", "token-hash-1");
        assert!(matches!(
            store.device_by_token_hash("wrong").unwrap(),
            TokenAuth::Missing
        ));
        assert!(matches!(
            store.device_by_token_hash("").unwrap(),
            TokenAuth::Missing
        ));
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

    /// A paired device takes the name it now calls itself, made unique against the others only: a device that
    /// keeps its name keeps it exactly (no drift to `-2` on each connect), two devices asking for one name stay
    /// `name` and `name-2` whichever reconnects, and the comparison ignores case as pairing's does.
    #[test]
    fn a_paired_device_is_renamed_to_what_it_calls_itself_without_drifting() {
        let (store, _path) = temp_store();
        add_device(&store, "studio", "CodeConnect Mac", "t1");
        add_device(&store, "air", "CodeConnect Mac", "t2");
        let names = |store: &Store| -> Vec<String> {
            store
                .list_devices()
                .unwrap()
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert_eq!(names(&store), vec!["CodeConnect Mac", "CodeConnect Mac-2"]);

        assert_eq!(
            store
                .rename_device("studio", "Jordan’s Mac Studio")
                .unwrap(),
            "Jordan’s Mac Studio"
        );
        assert_eq!(
            store.rename_device("air", "Jordan’s MacBook Air").unwrap(),
            "Jordan’s MacBook Air"
        );
        assert_eq!(
            names(&store),
            vec!["Jordan’s Mac Studio", "Jordan’s MacBook Air"]
        );
        // The next connect, same name: unchanged, not "-2".
        assert_eq!(
            store
                .rename_device("studio", "Jordan’s Mac Studio")
                .unwrap(),
            "Jordan’s Mac Studio"
        );
        // Two devices calling themselves one name: the second is made unique, and stays so on each connect.
        assert_eq!(
            store.rename_device("air", "jordan’s mac studio").unwrap(),
            "jordan’s mac studio-2"
        );
        assert_eq!(
            store.rename_device("air", "jordan’s mac studio").unwrap(),
            "jordan’s mac studio-2"
        );
        assert_eq!(
            names(&store),
            vec!["Jordan’s Mac Studio", "jordan’s mac studio-2"]
        );
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
        assert!(auth(&store, "t1").is_some());
        assert_eq!(
            store.consume_pairing_code("hash-r", now).unwrap(),
            PairingConsume::Consumed
        );
    }

    // ------------------------------------------------------- schema migration

    fn legacy_path() -> TempDb {
        TempDb::unique("ccd-legacy")
    }

    /// **What an earlier build kept for its transcript tailer and its hook path
    /// goes at open.** `tail_cursors` names each run's transcript and the hash of
    /// its last line read, `deleted_names` the names of deleted runs, and nothing
    /// reads either now: an upgraded store opens without them, so a deleted run
    /// leaves neither behind.
    #[test]
    fn an_earlier_builds_cursors_and_name_tombstones_are_dropped_at_open() {
        let path = legacy_path();
        drop(Store::open(path.path()).unwrap());
        rusqlite::Connection::open(path.path())
            .unwrap()
            .execute_batch(
                "CREATE TABLE tail_cursors(session_uid TEXT PRIMARY KEY, path TEXT NOT NULL);
                 INSERT INTO tail_cursors VALUES('u-1', '/tmp/one.jsonl');
                 CREATE TABLE deleted_names(session_id TEXT PRIMARY KEY);
                 INSERT INTO deleted_names VALUES('cc-1');",
            )
            .unwrap();

        let reopened = Store::open(path.path()).unwrap();
        let conn = rusqlite::Connection::open(path.path()).unwrap();
        for table in ["tail_cursors", "deleted_names"] {
            let left: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{table} outlived the open");
        }
        drop(reopened);
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

    /// The `devices` table every release from v0.10.0 to v0.14.0 created, copied
    /// from `git show v0.14.0:mac/ccd/src/store.rs` byte for byte — the comment
    /// lines included, because they are what SQLite 3.46 trips over when asked to
    /// drop `features_epoch` from this table.
    const SHIPPED_DEVICES_DDL: &str = r#"
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
            push_credential   TEXT,
            -- The device's advertised feature set (its agent list) as JSON, and
            -- the feature epoch of the daemon run that confirmed it. **Null on
            -- every row today, and nothing in production writes either one**: no
            -- shipping client can send a `features` field, so the daemon has no
            -- input to record and does not pretend otherwise. Null is the Claude
            -- floor, which is also what every row predating the agent seam holds.
            -- The columns are declared now because the read (`push_targets`) is
            -- already fail-closed against them — a set stamped with another run's
            -- epoch, or bytes that will not decode, authorize nothing — and a later build
            -- can turn the write on without a migration.
            features          TEXT,
            features_epoch    TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS devices_name ON devices(name);
"#;

    /// **A database a shipping build made keeps its phone, keeps its two unused
    /// feature columns, and opens without a warning.**
    ///
    /// Every release from v0.7.0 to v0.14.0 declared `devices.features` and
    /// `devices.features_epoch`; this one reads and writes neither. They stay
    /// where they are: the bundled SQLite cannot drop `features_epoch` from the
    /// table those builds created, so retiring them left a column behind and a
    /// warning at every start naming a remedy that does not exist.
    ///
    /// **Mutation:** put the two columns back on [`RETIRED_COLUMNS`] and
    /// `features` is dropped while `features_epoch` stays behind with a warning.
    #[test]
    fn a_shipping_database_keeps_its_phone_and_its_feature_columns_without_a_warning() {
        let (store, path) = temp_store();
        drop(store);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("DROP INDEX devices_name; DROP TABLE devices;")
                .unwrap();
            conn.execute_batch(SHIPPED_DEVICES_DDL).unwrap();
            conn.execute(
                "INSERT INTO devices(device_id, name, token_hash, created_at, push_token,
                                     push_environment, push_credential, features,
                                     features_epoch)
                 VALUES('phone', 'iPhone', 'hash-phone', '2026-10-01T00:00:00Z', 'tok-phone',
                        'production', 'bearer-phone', '{\"agents\":[\"codex\"]}',
                        'a-previous-daemon-run')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(
            auth(&store, "hash-phone")
                .expect("the phone is still paired")
                .device_id,
            "phone"
        );
        let targets = store.push_targets().unwrap();
        assert_eq!(targets.len(), 1, "the phone is still registered for push");
        assert_eq!(
            (
                targets[0].token.as_str(),
                targets[0].environment.as_str(),
                targets[0].credential.as_ref().map(|c| c.expose())
            ),
            ("tok-phone", "production", Some("bearer-phone"))
        );
        drop(store);

        let mut conn = Connection::open(&path).unwrap();
        let columns = table_columns(&conn, "devices").unwrap();
        assert!(
            columns.iter().any(|c| c == "features")
                && columns.iter().any(|c| c == "features_epoch"),
            "both columns stay where the shipping build put them: {columns:?}"
        );
        assert_eq!(
            drop_retired_columns(&mut conn).unwrap(),
            Vec::<String>::new(),
            "and a start has nothing to warn about"
        );
    }

    #[test]
    fn a_legacy_credential_table_gives_up_its_retired_columns_without_losing_a_device() {
        // A device row *is* a phone's pairing: the token hash is the only copy
        // of that credential the Mac holds, and dropping it un-pairs a phone
        // that has no way to find out until it next tries to connect.
        let path = legacy_path();
        {
            let conn = retired_credential_database(&path);
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

        let device = auth(&store, "hash-a").expect("the device token must still authenticate");
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
            .insert_device("d2", "iPad", "hash-b", "2026-08-03T00:00:00.000Z", None)
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
            let conn = retired_credential_database(&path);
            conn.execute(
                "INSERT INTO devices(device_id, name, token_hash, created_at, last_seen_at,
                                     revoked_at, ssh_key_installed, ssh_fingerprint)
                 VALUES('d1','iPhone','hash-a','2026-08-01T00:00:00.000Z', NULL, NULL, 0, NULL)",
                [],
            )
            .unwrap();
            assert!(
                needs_column_additions(&conn).unwrap(),
                "a table written before the columns this build adds has none of them"
            );
        }
        drop(Store::open(&path).unwrap());

        let conn = Connection::open(&path).unwrap();
        assert!(!needs_column_additions(&conn).unwrap());
        // The state a second `ccd` sees when it passes the unlocked check and
        // then reaches the write lock after the first one committed. `ALTER
        // TABLE … ADD COLUMN` has no `IF NOT EXISTS`, so the recheck this makes
        // for itself, once it holds the lock, is the whole difference between
        // this and a duplicate-column error that stops the daemon from opening.
        // The lock now belongs to [`Store::migrate`]'s single transaction; the
        // recheck is still this function's own, which is what this exercises.
        add_missing_columns(&conn).expect("a redundant widening must not error");
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
            let conn = retired_credential_database(&path);
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
        assert!(auth(&store, "hash-a").is_some());
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
            .insert_device("d1", "iPhone", "hash-a", "2026-08-01T00:00:00.000Z", None)
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

        let device = auth(&store, "hash-a").expect("the device token must still authenticate");
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
        assert!(auth(&store, "hash-a").is_some());
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
        assert_eq!(version, 7, "the schema version other builds will read");
        assert_eq!(version, SCHEMA_VERSION);
        // **One answer ledger, and it is the generalized one.** `mutation_ledger`
        // was built for "answer, compose, interrupt" and its conflict law is
        // proven; a second claim-before-write table for the very operation it was
        // built for would be two idempotency laws for one question. The negative
        // is asserted here because it is the thing that stays true only if
        // somebody keeps checking it.
        assert!(table_exists(&conn, "mutation_ledger").unwrap());
        assert!(!table_exists(&conn, "codex_answers").unwrap());
        assert!(column_exists(&conn, "events", "session_uid").unwrap());
        // Both halves of the fleet and the view that reads them, built by the
        // first open.
        assert!(table_exists(&conn, "codex_sessions").unwrap());
        let view: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'view' AND name = 'all_sessions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            view, 1,
            "the fleet-wide read surface is a view, and it exists"
        );
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
            .insert_device("dev-old", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
            .unwrap();
        store
            // The same physical phone re-pairing arrives as a *new* device
            // row with a fresh name — names are unique.
            .insert_device(
                "dev-new",
                "iPhone 2",
                "hash-b",
                "2026-08-02T00:00:00Z",
                None,
            )
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device("dev-live", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device(
                "dev-revoked",
                "iPhone 2",
                "hash-b",
                "2026-08-02T00:00:00Z",
                None,
            )
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device("dev-old", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
            .unwrap();
        store
            .insert_device(
                "dev-new",
                "iPhone 2",
                "hash-b",
                "2026-08-02T00:00:00Z",
                None,
            )
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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

    /// **A rollback-mixed tuple is repaired on the way back up.**
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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

    /// **A historically revoked row loses its lingering push tuple.**
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
    /// property that makes this a one-shot repair rather than an always-on
    /// normalizer.
    ///
    /// **The negative check:** drop the `from_version < 3` gate so the
    /// normalizer runs every start, and this fails — the credential is wiped on
    /// the reopen and relay push would re-register for ever.
    #[test]
    fn a_current_credential_survives_a_restart_that_is_not_an_upgrade() {
        let (store, path) = temp_store();
        store
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
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
                .insert_device(device_id, name, hash, "2026-08-01T00:00:00Z", None)
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

        assert_eq!(
            store
                .revoke_device("dev-gone", "2026-08-03T00:00:00Z")
                .unwrap(),
            ["dev-gone"]
        );
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
            .insert_device("dev-1", "iPhone", "hash-a", "2026-08-01T00:00:00Z", None)
            .unwrap();
        store
            .insert_device("dev-2", "iPad", "hash-b", "2026-08-02T00:00:00Z", None)
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

    /// **The fleet read and the per-device lookup are one rule.**
    ///
    /// The lookup exists so a push worker need not scan the fleet before each
    /// transport attempt, so a predicate that drifted between the two would be a
    /// device authorized differently depending on which question was asked.
    ///
    /// **Mutation:** drop `PUSH_TARGET_ELIGIBLE` from `push_target_for`'s
    /// statement and the revoked and tokenless devices are offered by the lookup.
    #[test]
    fn the_per_device_lookup_refuses_what_the_fleet_read_refuses() {
        let (store, _path) = temp_store();
        for device in ["silent", "confirmed"] {
            store
                .insert_device(
                    device,
                    // Device names are unique, so each phone needs its own.
                    &format!("iPhone ({device})"),
                    &format!("hash-{device}"),
                    "2026-08-01T00:00:00Z",
                    None,
                )
                .unwrap();
            store
                .set_push_token(device, &format!("tok-{device}"), "production", None)
                .unwrap();
            let from_fleet = store
                .push_targets()
                .unwrap()
                .into_iter()
                .find(|t| t.device_id == device)
                .expect("the device is registered for push");
            let from_lookup = store
                .push_target_for(device)
                .unwrap()
                .expect("the per-device lookup finds the same registration");
            assert_eq!(
                (from_fleet.token, from_fleet.environment),
                (from_lookup.token, from_lookup.environment),
                "{device}: the fleet read and the per-device lookup disagree"
            );
        }

        // **And the per-device lookup carries the same ELIGIBILITY predicate, not
        // only the same decode.** A row the fleet read refuses to offer must be
        // absent from the lookup too — otherwise the re-check before a transport
        // attempt would vouch for a phone the fan-out would never have rung.
        // A device that does not exist is the same answer, for the same reason:
        // nothing to vouch for.
        assert!(
            store.push_target_for("never-paired").unwrap().is_none(),
            "a device that does not exist has no registration to offer"
        );
        store
            .revoke_device("confirmed", "2026-08-04T00:00:00Z")
            .unwrap();
        assert!(
            store.push_target_for("confirmed").unwrap().is_none(),
            "a revoked device is refused by the lookup exactly as the fleet read \
             refuses it; revocation must not survive only in the list"
        );
        assert!(
            store
                .push_targets()
                .unwrap()
                .iter()
                .all(|t| t.device_id != "confirmed"),
            "the premise: the fleet read refuses it too"
        );
        store
            .clear_push_token("silent", "tok-silent", None)
            .unwrap();
        assert!(
            store.push_target_for("silent").unwrap().is_none(),
            "a device with no token has no registration to offer either"
        );
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
    // it makes rather than against how it makes them. The builder here changes
    // nothing but the two credential tables.

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
        assert!(auth(&store, "hash-plain").is_some());
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
            // Named columns, the way a legacy writer inserts: the agent-seam
            // columns added at open take their defaults ('claude', NULL), so this
            // row is a Claude row exactly as a pre-seam one would have been.
            conn.execute_batch(
                "INSERT INTO sessions(session_uid, session_id, cwd, claude_session_id,
                                      transcript_path, lifecycle, created_at, updated_at)
                 VALUES('uid-1','cc-1','/tmp/one','claude-uuid','/tmp/one.jsonl','live',
                        '2026-07-30T10:00:00.000Z','2026-07-30T11:00:00.000Z');
                 INSERT INTO run_hosts(session_uid, request_id) VALUES('uid-1','uid-1');
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

    /// **The tmux-era engine's runs and columns go at open, and nothing else
    /// does.** A store as the last build left it — the eight columns on both
    /// session tables, the view naming them, the tombstone and cursor tables —
    /// opens with all of those gone, the run with no engine host record gone with
    /// its history, both engine runs whole, and pairing, trust and projects as
    /// they were.
    #[test]
    fn the_tmux_era_runs_and_columns_are_retired_at_open() {
        let path = legacy_path();
        let (tmux, claude, codex) = (uid("TM"), uid("CA"), uid("CX"));
        let kept = |conn: &Connection| {
            [
                "SELECT * FROM devices ORDER BY device_id",
                "SELECT * FROM folder_trust",
                "SELECT * FROM projects",
            ]
            .map(|sql| rows_of(conn, sql))
        };
        let before = {
            drop(Store::open(&path).unwrap());
            let conn = Connection::open(&path).unwrap();
            let columns = "session_uid TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                tmux_session TEXT NOT NULL, tmux_socket TEXT NOT NULL, cwd TEXT NOT NULL,
                claude_session_id TEXT, transcript_path TEXT, lifecycle TEXT NOT NULL,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                agent TEXT NOT NULL DEFAULT 'claude', codex_thread_id TEXT, codex_socket TEXT,
                codex_generation INTEGER";
            let shown = "session_uid, session_id, tmux_session, tmux_socket, cwd,
                claude_session_id, transcript_path, lifecycle, created_at, updated_at, agent,
                codex_thread_id, codex_socket";
            conn.execute_batch(&format!(
                "DROP VIEW all_sessions; DROP TABLE sessions; DROP TABLE codex_sessions;
                 CREATE TABLE sessions({columns}); CREATE TABLE codex_sessions({columns});
                 CREATE VIEW all_sessions AS SELECT {shown} FROM sessions
                     UNION ALL SELECT {shown} FROM codex_sessions;
                 CREATE TABLE deleted_sessions(session_uid TEXT PRIMARY KEY, deleted_at TEXT);
                 CREATE TABLE deleted_names(session_id TEXT PRIMARY KEY);
                 CREATE TABLE tail_cursors(session_uid TEXT PRIMARY KEY, path TEXT);
                 INSERT INTO sessions VALUES('{tmux}', 'cc-1', 'cc-1', 'codeconnect',
                     '/w/tmux', NULL, NULL, 'live', 't', 't', 'claude', NULL, NULL, NULL),
                   ('{claude}', '{claude}', '', '', '/w/app', NULL, NULL, 'live',
                     '2026-11-01T00:00:00.000Z', 't', 'claude', NULL, NULL, NULL);
                 INSERT INTO codex_sessions VALUES('{codex}', '{codex}', '', '', '/w/app', NULL,
                     NULL, 'live', '2026-11-02T00:00:00.000Z', 't', 'codex', 'th-1', NULL, NULL);
                 INSERT INTO run_hosts(session_uid, request_id)
                     VALUES('{claude}', 'r-1'), ('{codex}', 'r-2');
                 INSERT INTO events(session_uid, session_id, seq, ts, kind, payload, source)
                     VALUES('{tmux}', 'cc-1', 1, 't', 'tool_call', '{{}}', 'hook'),
                           ('{claude}', '{claude}', 1, 't', 'tool_call', '{{}}', 'sdk');
                 INSERT INTO codex_pending_approvals VALUES('{codex}', '{codex}', 'rq-1', '{{}}',
                     1, 0, 'th-1', 'tu-1', 'it-1', 'commandExecution');
                 INSERT INTO mutation_ledger(operation_kind, session_uid, client_request_id,
                         claimed_hash, thread_id, generation, route, status, started_at)
                     VALUES('answer', '{codex}', 'rq-1', 'h', 'th-1', 1, 'accept', 'applying', 't');
                 INSERT INTO devices(device_id, name, token_hash, created_at, revoked_at,
                         tailnet_node)
                     VALUES('paired', 'iPhone', 'hash-p', 't', NULL, NULL),
                           ('revoked', 'iPad', 'hash-r', 't', 't', 'node-1');
                 INSERT INTO folder_trust VALUES('/w/app', 1, 't');
                 INSERT INTO projects VALUES('/w/app', '2026-11-10T00:00:00.000Z', 'codex');"
            ))
            .unwrap();
            kept(&conn)
        };

        let store = Store::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert!(retired_columns_present(&conn).unwrap().is_empty());
        assert_eq!(kept(&conn), before, "pairing, trust or projects changed");
        for table in ["deleted_sessions", "deleted_names", "tail_cursors"] {
            assert!(
                !table_exists(&conn, table).unwrap(),
                "{table} outlived the open"
            );
        }
        assert!(
            store.get_session(&tmux).unwrap().is_none(),
            "the tmux-era run stayed"
        );
        assert_eq!(store.count_events(&tmux).unwrap(), 0, "its history stayed");
        assert!(store.get_session(&claude).unwrap().is_some());
        assert_eq!(store.count_events(&claude).unwrap(), 1);
        assert!(store.get_session(&codex).unwrap().is_some());
        assert_eq!(store.codex_pending_approvals(&codex).unwrap().len(), 1);
        assert_eq!(store.unsettled_claims(OPERATION_ANSWER).unwrap().len(), 1);
        assert_eq!(store.blocked_tailnet_nodes().unwrap(), ["node-1"]);
        let new = key("NW", "cc-2");
        begin(&store, &new);
        assert!(
            store.get_session(&new.uid).unwrap().is_some(),
            "a new run is refused"
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
        assert!(auth(&store, "hash-plain").is_some());
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
            .insert_device(
                "d-new",
                "iPad mini",
                "hash-new",
                "2026-08-04T00:00:00.000Z",
                None,
            )
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
            .insert_device(
                "d-new",
                "iPad mini",
                "hash-new",
                "2026-08-04T00:00:00.000Z",
                None,
            )
            .unwrap();
        assert_eq!(
            auth(&store, "hash-new")
                .expect("the phone that just paired must authenticate")
                .device_id,
            "d-new"
        );
        assert!(auth(&store, "hash-plain").is_some());
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
            .env(CHILD, path.path())
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

    /// **An Antigravity run is certainly quiet once every message it was given has its turn's end** (306 G3): agy takes
    /// a message at once and queues it behind the turn running, and ends each turn with the reply to that message's
    /// prompt, filed under its uuid. Quiet: each message and mail wake since its process started answered, no tool still
    /// running, and no command run in its last turn (one may still run in the background). A turn's tools are read by
    /// its id, so a finished Read leaves it quiet.
    #[test]
    fn an_antigravity_run_is_quiet_once_every_message_has_its_turns_end() {
        let filed = |steps: &[(&str, &str)]| {
            let (store, _path) = temp_store();
            let session = key("AG", "ag-1");
            let mut row = session_row(&session);
            row.agent = AgentKind::Antigravity;
            assert_eq!(
                store.begin_run(&row, &session.uid, None, None).unwrap(),
                None
            );
            for (seq, (step, turn)) in steps.iter().enumerate() {
                let acp = |message: serde_json::Value| match protocol::acp_map::read(
                    &session,
                    "start",
                    seq as u64,
                    Some(turn),
                    &message,
                ) {
                    protocol::acp_map::Read::Events(events) => events,
                    protocol::acp_map::Read::Chunk { .. } => unreachable!(),
                };
                let tool = |kind: &str, status: &str| {
                    acp(
                        json!({"method": "session/update", "params": {"update": {"sessionUpdate": "tool_call",
                        "toolCallId": format!("call-{seq}"), "kind": kind, "status": status, "rawInput": {}}}}),
                    )
                };
                let events = match *step {
                    "say" => vec![crate::claude_host::person_row(&session.uid, turn, "hello", None, None)],
                    "wake" => vec![PendingEvent::new(
                        &session,
                        EventKind::Notification,
                        json!({"notification_type": protocol::event::notice::MAIL_NUDGE, "message": "mail"}),
                        Source::Daemon,
                    )
                    .with_source_event_id(format!("wake:{turn}"))],
                    "read" => tool("read", "completed"),
                    "run" => tool("execute", "completed"),
                    "running" => tool("read", "in_progress"),
                    "end" => acp(json!({"result": {"stopReason": "end_turn"}})),
                    other => unreachable!("{other}"),
                };
                for event in &events {
                    store.append_event(event).unwrap();
                }
            }
            store.certainly_quiet(&session.uid, "sending").unwrap()
        };
        let quiet = [
            ("answered", vec![("say", "m1"), ("end", "m1")]),
            (
                "a finished read",
                vec![("say", "m1"), ("read", "m1"), ("end", "m1")],
            ),
            (
                "two answered",
                vec![("say", "m1"), ("say", "m2"), ("end", "m1"), ("end", "m2")],
            ),
            (
                "a wake answered",
                vec![("say", "m1"), ("end", "m1"), ("wake", "w1"), ("end", "w1")],
            ),
            (
                "a command before its last turn",
                vec![
                    ("say", "m1"),
                    ("run", "m1"),
                    ("end", "m1"),
                    ("say", "m2"),
                    ("end", "m2"),
                ],
            ),
        ];
        let busy = [
            ("nothing answered", vec![("say", "m1")]),
            (
                "a queued message",
                vec![("say", "m1"), ("say", "m2"), ("end", "m1")],
            ),
            (
                "a wake running",
                vec![("say", "m1"), ("end", "m1"), ("wake", "w1")],
            ),
            (
                "a tool running",
                vec![("say", "m1"), ("running", "m1"), ("end", "m1")],
            ),
            (
                "a command in its last turn",
                vec![("say", "m1"), ("run", "m1"), ("end", "m1")],
            ),
        ];
        for (case, steps) in quiet {
            assert!(filed(&steps), "{case}: quiet");
        }
        for (case, steps) in busy {
            assert!(!filed(&steps), "{case}: not quiet");
        }
    }
}
