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
use protocol::agent::AgentKind;
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
///   * `4` — agent-scoped session storage: Codex runs live in `codex_sessions`,
///     a second physical table with the same shape, and `sessions` goes back to
///     being Claude-only. See [`create_schema`] for why a second table rather
///     than the `agent` column that is already there, and
///     [`needs_codex_session_move`] for the migration that moves existing rows.
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
///     contains no statement that names `codex_sessions`. The migration below
///     is deliberately **not** gated on this number for the mirror-image reason
///     — a rollback resets `user_version` to 3, and the move still has to
///     re-run correctly on the way back up.
///   * `5` — agent-scoped approval cards: a Codex run's open cards live in
///     `codex_pending_approvals`, and `pending_approvals` goes back to being
///     Claude-only. The same reasoning as `4` one table down: the old daemon
///     reads `pending_approvals` GLOBALLY, without walking a session row, so
///     `4`'s session split did not hide the cards. `2e-7a` deferred this split
///     only because nothing produced a Codex card; the approval observer is
///     that producer. Ships with `pending_approvals_refuse_codex_card`, which a
///     rollback keeps for the same reason the session trigger is kept, and with
///     the `all_pending_approvals` view for the reads that answer for both
///     agents. `answer_claims` and `text_mutations` are deliberately NOT split:
///     nothing writes a Codex row into either, and a table split ahead of its
///     producer is the speculative half-surface this plan refuses.
const SCHEMA_VERSION: i64 = 5;

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
    /// Which agent hosts this run. `Claude` for every legacy row and every row
    /// this build writes today; the column defaults to `'claude'` in SQLite, so
    /// a backfilled legacy row is Claude without a data repair. An unrecognised
    /// stored value decodes to [`AgentKind::Unsupported`], never Claude.
    pub agent: AgentKind,
    /// Codex thread identity, `None` for Claude and for a pre-seam row.
    pub codex_thread_id: Option<String>,
    /// The Codex broker socket ccd reconnects to, `None` for Claude.
    pub codex_socket: Option<String>,
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
    /// **What this device said it can render, as far as this run can confirm
    /// it** — the read side of `devices.features`, and the reason the push read
    /// is the authorization point rather than merely the address book. See
    /// [`DeviceFeatures`] for the two shapes and why they are not one.
    pub features: DeviceFeatures,
}

/// The columns a [`PushRegistration`] is decoded from, in the order
/// [`push_registration`] reads them.
///
/// Shared by the fleet read and the per-device lookup so the two cannot drift:
/// a column added to one and not the other would be a decode that reads a
/// different row than it thinks it does.
const PUSH_TARGET_COLUMNS: &str = "SELECT device_id, push_token, \
     COALESCE(push_environment, 'sandbox'), push_credential, features, features_epoch \
     FROM devices";

/// Who may be pushed to at all — the authorization predicate, shared for the
/// same reason as the columns. A revoked device is excluded here rather than at
/// a call site, so revocation cannot leak through whichever read forgot it.
const PUSH_TARGET_ELIGIBLE: &str =
    "WHERE push_token IS NOT NULL AND push_token <> '' AND revoked_at IS NULL";

/// One row of [`PUSH_TARGET_COLUMNS`], decoded under this run's feature epoch.
fn push_registration(row: &rusqlite::Row<'_>, epoch: &str) -> rusqlite::Result<PushRegistration> {
    let features: Option<String> = row.get(4)?;
    let features_epoch: Option<String> = row.get(5)?;
    Ok(PushRegistration {
        device_id: row.get(0)?,
        token: row.get(1)?,
        environment: row.get(2)?,
        // Wrapped the instant it leaves the database, so the raw bearer
        // exists as a bare `String` only inside this function.
        credential: row.get::<_, Option<String>>(3)?.map(Redacted::from),
        // **The column's three states, kept as the two answers they
        // are** — see [`DeviceFeatures`]. `NULL` is the genuine floor: a
        // device that has advertised nothing gets Claude, which is what
        // every phone predating the field has always got. A set that IS
        // stored and this run cannot vouch for — another process's epoch,
        // or bytes that will not decode — is not a quieter version of
        // that; it is a claim this run cannot read, and reading it as
        // "advertised nothing" would hand a `[Codex]`-only phone back the
        // Claude doorbells its own claim excluded.
        features: match features {
            None => DeviceFeatures::Advertised(Default::default()),
            Some(json) if features_epoch.as_deref() == Some(epoch) => serde_json::from_str(&json)
                .map(DeviceFeatures::Advertised)
                .unwrap_or(DeviceFeatures::Unconfirmable),
            Some(_) => DeviceFeatures::Unconfirmable,
        },
    })
}

/// What a device's stored feature set authorizes **this run** — the decode of
/// `devices.features` beside the token it rides with.
///
/// # Two shapes, and the broadening that collapsing them causes
///
/// The column has three states, not two, and the third is the one that matters:
/// no set at all, a set this run can confirm, and a set this run *cannot*. The
/// first two are one variant here because they mean the same thing to a push —
/// a phone that has advertised nothing (a legacy build, or one that named no
/// agents) gets the Claude floor from [`protocol::ws::ClientFeatures::supports`],
/// and a confirmed set gets exactly what it names.
///
/// The third is [`DeviceFeatures::Unconfirmable`], and it was once folded into
/// the floor along with the others. That fold is a **broadening**: a phone that
/// explicitly advertised `[Codex]` has said it cannot render a Claude alert, and
/// decoding its stored set as "advertised nothing" after a restart hands it back
/// the Claude doorbells its own claim excluded — a write that broadens
/// authorization, arriving through the read.
///
/// So the rule the whole projection now holds, on both sides: **`NULL` is the
/// genuine Claude floor; a stored set this run cannot vouch for means the device
/// hears nothing.** It is the only direction that cannot grant a device something
/// it never claimed.
///
/// **Nothing writes this column in this phase, so today the third state cannot
/// arise and every row is `NULL`.** The advertisement write side was deleted —
/// nothing that ships can fill `RegisterPush::features`, so it was a write path
/// with no input — and the daemon discards an advertised set rather than
/// persisting it. The `Unconfirmable` branch is therefore a fail-closed decode
/// kept for the phase that lands the writes, not a state incident response will
/// meet: until then, silence here is not repaired by a later advertisement,
/// because there is no later advertisement to repair it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceFeatures {
    /// The device's own word: the set it advertised and this run confirmed, or
    /// the empty set left by a device that advertised nothing (`NULL`), which is
    /// the Claude floor.
    Advertised(protocol::ws::ClientFeatures),
    /// A set is stored and this run cannot vouch for it: stamped with another
    /// process's [`crate::state::feature_epoch`], or bytes that will not decode.
    /// Whatever the device last said, this is not it.
    Unconfirmable,
}

/// **`NULL`'s answer**, because that is the row a device with no history has and
/// the value every test fleet means by "an ordinary phone". A derived `Default`
/// cannot name a variant with a payload, so it is written out here.
impl Default for DeviceFeatures {
    fn default() -> Self {
        DeviceFeatures::Advertised(protocol::ws::ClientFeatures::default())
    }
}

impl DeviceFeatures {
    /// Whether a doorbell for `agent` may be sent to this device.
    ///
    /// The one predicate, so the two senders and every test ask the question the
    /// same way. `Unconfirmable` answers `false` for every agent, Claude included:
    /// Claude is the floor of what an *advertisement* grants, and a set this run
    /// cannot read is not one.
    pub fn supports(&self, agent: &protocol::agent::AgentKind) -> bool {
        match self {
            DeviceFeatures::Advertised(features) => features.supports(agent),
            DeviceFeatures::Unconfirmable => false,
        }
    }
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
    /// `commandExecution` | `fileChange`. The observe-only families never reach
    /// this table, because no card is built for them.
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

/// Where a run of this agent is stored, and where it therefore must not be.
///
/// Returns `(table, other)`. The pair rather than just the table, because every
/// caller that writes one has to be able to say something about the other: the
/// upsert **moves** a uid it finds in the other rather than adding a second copy
/// — see [`Store::upsert_session`] for why moving and not refusing — and the two
/// removal paths delete from both and check the total.
///
/// **Everything that is not Claude goes to `codex_sessions`**, including an
/// [`AgentKind::Unsupported`] run. That is deliberate and it is the same rule
/// [`needs_codex_session_move`] uses: `sessions` is the table a rolled-back
/// v0.6.0 daemon sweeps, and an agent this build understands even less than
/// Codex is not the one to leave in it. The table is named for the agent it was
/// built for, not for the only agent it can ever hold.
fn session_tables_for(agent: &AgentKind) -> (&'static str, &'static str) {
    if agent.is_claude() {
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
    /// actuation go?" can mean for a decision.
    pub route: String,
    pub target_turn_id: Option<String>,
    pub claimed_hash: String,
}

/// **How a phone answer's ledger row is closed in the commit that retires its
/// card.**
///
/// Two endings and not three, because the third — "nothing was written, and that
/// is provable" — never reaches a durable row at all: the claim is taken only
/// once the live connection has accepted the ask, so an answer that cannot be
/// addressed leaves the ledger untouched and the card answerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerTerminal {
    /// The upstream write is proven. A duplicate replays this outcome rather than
    /// writing a second response to a request the app-server answers once.
    Settled(&'static str),
    /// The answer may have actuated and nothing that survives can say. Terminal
    /// precisely so that it is never retried.
    Indeterminate,
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

/// One SQL string literal, quotes doubled. Test fixture only — every production
/// statement in this file is parameterised.
#[cfg(test)]
fn escape_sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
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
/// other's opposite. `delivered` is a phone answer the broker confirmed reached the
/// app-server; `lost` is a phone answer that forwarded ZERO bytes because something else
/// settled the request first, and whose card was deliberately left standing for that
/// other terminal to retire. Both are replayed here, because both mean "this card will
/// not be answered from a phone again" — but telling an operator their tap was applied
/// when the record says it was not is the one thing this sentence must never do.
///
/// An outcome this build has no words for still refuses, naming the word rather than
/// guessing at it: the terminal is what governs, and the vocabulary is not.
pub fn replayed_answer_sentence(outcome: &str) -> String {
    match outcome {
        ANSWER_DELIVERED => "this card was already answered from a phone; nothing was sent \
                             again"
            .to_string(),
        ANSWER_LOST => "a phone answer to this card lost the race — something else answered \
                        it first — so nothing was sent again"
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
/// The write itself was refused — by the broker before a byte left, or by the
/// app-server with an error. Proven not to have actuated.
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
/// A compose the wire refused. Terminal, and replayed as a refusal rather than re-sent.
pub const COMPOSE_REFUSED: &str = "refused";

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
        // **The outcome is not interpolated.** A row this build cannot read carries a
        // turn id the phone never sent — `parse_compose_outcome` refuses an unknown route
        // and lands here — and there is nothing in the stored word a caller can act on
        // beyond the fact that this id is spent.
        _ => crate::codex_refusals::REPLAY_COMPOSE_SETTLED.to_string(),
    }
}

/// **Every `operation_kind` the ledger has, in one list.**
///
/// The kinds are still named individually by the paths that *write* them, because each
/// writes something different: an answer retires an approval card beside its row, an
/// interrupt has only the row, and one of the two recoveries is rightly gated on the
/// cards being back while the other must not be.
///
/// What they must NOT differ about is being made **terminal**. A claim of any kind left
/// `applying` by a process — or by a link — that stopped is one no phone can ever
/// re-ask under the same id, and the second kind spent a while with half a recovery
/// precisely because "which kinds are covered" lived only in a reader's head.
///
/// **Production reads it now, and that is the change.** Every in-process abort path
/// sweeps the outgoing session's claims by iterating this list — see
/// [`crate::state::Daemon::sweep_stranded_claims`] — so a third kind added here is
/// swept by every abort path the day it exists, rather than by whichever ones somebody
/// remembered. That is worth the one `match` on the kind inside the loop, which is the
/// price of the two closers genuinely differing.
pub const OPERATION_KINDS: [&str; 3] = [OPERATION_ANSWER, OPERATION_INTERRUPT, OPERATION_COMPOSE];

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
    let (status, outcome) = match answer {
        AnswerTerminal::Settled(outcome) => ("done", Some(outcome)),
        AnswerTerminal::Indeterminate => ("indeterminate", None),
    };
    let settled = tx.execute(
        "UPDATE mutation_ledger SET status = ?4, outcome = ?5, settled_at = ?6
          WHERE operation_kind = ?1 AND session_uid = ?2 AND client_request_id = ?3
            AND status = 'applying'",
        params![
            OPERATION_ANSWER,
            session_uid,
            client_request_id,
            status,
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
        // **One `BEGIN IMMEDIATE` over all three, and the transaction is the
        // isolation.** Creating `codex_sessions` publishes a schema that
        // *promises* Codex runs are out of the swept table; the move is what
        // makes that true. Run as three autocommitting steps there is a window
        // between them, and a v0.6.0 daemon that starts inside it finds the
        // promise made and unkept — measured, with the old daemon's own two
        // statements against a database mid-migration:
        //
        // ```text
        // three steps:  old enumerated=[AA, CX]  update_rows=2
        // one txn:      old enumerated=[AA, CX]  update_rows=None  database is locked
        //               (after COMMIT) enumerated=[AA]  update_rows=1
        // ```
        //
        // Under WAL the old daemon's *reads* mid-transaction see the snapshot as
        // it was before any of this ran — the old schema, with the Codex row
        // still in `sessions` and no `codex_sessions` to be seen — and its
        // *writes* are refused until the commit lands. So it finds either the
        // state before the migration or the state after it, never the half
        // state in between; and because v0.6.0 sets `busy_timeout` to 5000ms
        // (its `open_connection`, unchanged from ours), what it actually does is
        // wait a few milliseconds and then proceed against the isolated schema.
        //
        // Every statement below is transactional in SQLite — `CREATE TABLE`,
        // `CREATE INDEX`, `CREATE VIEW` and `ALTER TABLE … ADD COLUMN` all roll
        // back cleanly, measured rather than assumed. `create_schema` is one
        // `execute_batch` of pure DDL with no transaction control of its own, so
        // it nests here without a second `BEGIN`.
        //
        // `migrate_to_session_uids` above stays outside on purpose: it is a
        // whole-table rebuild for a database that predates session uids, and
        // such a database predates the `agent` column too, so there is no Codex
        // row for it to expose. `drop_retired_columns` below stays outside for
        // the opposite reason — it is deliberately never fatal, and a failure
        // inside this transaction would take the schema down with it.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        create_schema(&tx)?;
        // Additive, and applied after `create_schema` for the reason the note
        // above gives in reverse: `CREATE TABLE IF NOT EXISTS` will not widen a
        // table that already exists, so a database written before push existed
        // keeps its old `devices` shape and every push statement fails on a
        // missing column.
        if needs_column_additions(&tx)? {
            add_missing_columns(&tx)?;
        }
        // **Third, and it has to be here.** The move reads `sessions.agent`, so
        // it must follow `add_missing_columns` on a database written before the
        // agent seam; it writes `codex_sessions`, so it must follow
        // `create_schema`.
        //
        // Not gated on `from_version`: see [`needs_codex_session_move`]. A
        // rollback resets the number and the rows are the only honest question.
        //
        // **This repairs; it does not have to also race.** An earlier shape
        // followed the commit with a timed re-ask, because `BEGIN IMMEDIATE`
        // defers a v0.6.0 statement rather than refusing it, and a deferred
        // upsert lands after the commit and re-files the uid this just moved.
        // That window is now closed where it cannot be missed — the
        // `sessions_refuse_codex_shadow` trigger in `create_schema` refuses the
        // deferred statement outright — so what is left here is the one job a
        // trigger cannot do: rows that were already misfiled before this build
        // ever opened the database.
        if needs_codex_session_move(&tx)? {
            let done = move_codex_sessions(&tx)?;
            crate::log_info!(
                "schema: took {} misfiled session row(s) out of the shared sessions table, \
                 where a rolled-back v0.6 daemon cannot enumerate, update, delete or prune \
                 them: {} carried whole into codex_sessions, {} merged into an isolated row \
                 that was already there",
                done.moved + done.reconciled,
                done.moved,
                done.reconciled
            );
        }
        // **Fourth, and it has to follow the session move.** The predicate asks
        // whether a card's uid is a Codex run, and the only witness of that is a
        // `codex_sessions` row — which the move above is what puts there. Run
        // before it, a card belonging to a still-misfiled run would look Claude
        // and be left in the swept table.
        //
        // Not gated on `from_version`, for the reason version 4 gives: a
        // rollback resets the number, and the rows are the only honest question.
        if needs_codex_card_move(&tx)? {
            let moved = move_codex_pending_approvals(&tx)?;
            crate::log_info!(
                "schema: took {moved} approval card(s) out of the shared pending_approvals \
                 table, where a rolled-back v0.6 daemon enumerates and deletes them without \
                 going through a sessions row"
            );
        }
        tx.commit()?;
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
        //
        // Deliberately still the physical `sessions` table, not `all_sessions`:
        // `claude:` is the prefix cc-hook mints, so every row this can match is
        // Claude's by construction, and a view is not updatable anyway.
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
            // `all_sessions`: "does this run still exist" is a question about
            // the fleet, not about one table. Asked of Claude's half alone it
            // answers "no" for a live Codex run, rolls the batch back, and — the
            // part that does not recover — leaves the cursor where it was.
            "SELECT EXISTS(SELECT 1 FROM all_sessions WHERE session_uid = ?1)",
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

    /// Make every session query fail, on the same terms and for the same reason.
    ///
    /// Dropping the Claude table alone is enough, and that is not an accident of
    /// this fixture: every session read goes through `all_sessions`, and a view
    /// whose base table is gone fails to prepare. One `DROP` therefore breaks
    /// both halves of the fleet, which is what "every session query" means.
    #[cfg(test)]
    pub fn break_session_lookups_for_tests(&self) {
        let conn = self.write();
        conn.execute_batch("DROP TABLE sessions")
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

    /// Make every **event append** fail, reversibly, while every other read and
    /// write keeps working.
    ///
    /// The narrower failure, and the only one that reaches the arm that matters:
    /// a retirement is a store *read* (which open cards does this terminal
    /// name?) followed by a *transaction* (delete the row, append the
    /// resolution). Breaking the cards table breaks the read, so the loop that
    /// decides what to do with a failed retirement is never entered at all —
    /// which is how a test can look like it covers that arm and cover nothing.
    /// Breaking only the append lets the read succeed and the transaction fail,
    /// which is the shape of a contended or corrupt log.
    ///
    /// No view or trigger names `events`, so the rename is symmetric.
    #[cfg(test)]
    pub fn break_event_appends_for_tests(&self, broken: bool) {
        let conn = self.write();
        let (from, to) = if broken {
            ("events", "events_hidden")
        } else {
            ("events_hidden", "events")
        };
        conn.execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
            .expect("test fixture");
    }

    /// Make the recovery's **pending-card read** fail, reversibly, while every other read
    /// and write keeps working.
    ///
    /// Renaming the Codex card table is not this: SQLite rewrites the view's reference
    /// along with it, so `all_pending_approvals` stays whole and the read succeeds (see
    /// [`Store::hide_codex_cards_for_tests`], which relies on exactly that). The view
    /// itself is what has to go.
    ///
    /// The behaviour under test is what a recovery DECIDES when it cannot see the cards,
    /// and nothing the daemon does can produce a failing read on demand. Reversible, so
    /// the same store can then be recovered by a healthy start — which is the second half
    /// of the claim.
    /// A view cannot be renamed, so it is dropped and rebuilt from its own recorded
    /// definition — `sqlite_master` holds the exact `CREATE VIEW` the migration wrote, so
    /// the restored view is that statement and not a copy of it kept here to drift.
    #[cfg(test)]
    pub fn break_pending_card_reads_for_tests(&self, broken: bool) {
        let conn = self.write();
        if broken {
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = \
                     'all_pending_approvals'",
                    [],
                    |row| row.get(0),
                )
                .expect("test fixture: the recovery view exists");
            conn.execute_batch(&format!(
                "CREATE TABLE all_pending_approvals_saved (sql TEXT);
                 INSERT INTO all_pending_approvals_saved (sql) VALUES ({});
                 DROP VIEW all_pending_approvals;",
                escape_sql_literal(&sql)
            ))
            .expect("test fixture");
        } else {
            let sql: String = conn
                .query_row("SELECT sql FROM all_pending_approvals_saved", [], |row| {
                    row.get(0)
                })
                .expect("test fixture: the view definition was saved");
            conn.execute_batch(&format!("{sql};\nDROP TABLE all_pending_approvals_saved;"))
                .expect("test fixture");
        }
    }

    /// Make every **session read** fail, reversibly, while every other read and write
    /// keeps working.
    ///
    /// `get_session` reads the `all_sessions` view, and the distinction under test is
    /// `Err` (this read failed and says nothing about the run) against `Ok(None)` (the
    /// run is genuinely gone). Only a failing read can tell them apart, and nothing the
    /// daemon does produces one on demand. Dropped and rebuilt from its own recorded
    /// definition, for [`Store::break_pending_card_reads_for_tests`]'s reason.
    #[cfg(test)]
    pub fn break_session_reads_for_tests(&self, broken: bool) {
        let conn = self.write();
        if broken {
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'all_sessions'",
                    [],
                    |row| row.get(0),
                )
                .expect("test fixture: the fleet view exists");
            conn.execute_batch(&format!(
                "CREATE TABLE all_sessions_saved (sql TEXT);
                 INSERT INTO all_sessions_saved (sql) VALUES ({});
                 DROP VIEW all_sessions;",
                escape_sql_literal(&sql)
            ))
            .expect("test fixture");
        } else {
            let sql: String = conn
                .query_row("SELECT sql FROM all_sessions_saved", [], |row| row.get(0))
                .expect("test fixture: the view definition was saved");
            conn.execute_batch(&format!("{sql};\nDROP TABLE all_sessions_saved;"))
                .expect("test fixture");
        }
    }

    /// Make every **mutation-ledger** read and write fail, reversibly, while every other
    /// read and write keeps working.
    ///
    /// The narrowest seam that reaches the one arm that was once unguarded: an answer
    /// whose card is not this call's to retire settles its claim ON ITS OWN, and that
    /// standalone settle can fail while the card path is perfectly healthy. Breaking the
    /// cards table or the event log breaks the retirement instead, so the standalone
    /// settle is never reached at all — which is how a test can look like it covers the
    /// arm and cover nothing.
    ///
    /// Reversible (a rename, like [`Store::break_event_appends_for_tests`]) because a
    /// test has to arm it *between* the claim and the settle: the claim is taken while
    /// the ledger is whole, and only then does the fault appear. No view or trigger names
    /// `mutation_ledger`, so the rename is symmetric.
    #[cfg(test)]
    pub fn break_answer_ledger_for_tests(&self, broken: bool) {
        let conn = self.write();
        let (from, to) = if broken {
            ("mutation_ledger", "mutation_ledger_hidden")
        } else {
            ("mutation_ledger_hidden", "mutation_ledger")
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

    /// **Has this run already filed the terminal named by
    /// `terminal_source_event_id`?**
    ///
    /// The log is what the daemon *knows* about a turn, and it is the only
    /// memory of one that outlives a control-link connection. A resume answer
    /// describing a turn as still running is news exactly when the log holds no
    /// terminal for it; an answer that regresses a turn this daemon already
    /// watched finish is a stale snapshot, and stale is not news — see
    /// `crate::codex_link::Connection::attach_from_seed`, which cancels a
    /// pending doorbell on the strength of that distinction.
    ///
    /// Asked of the log rather than of the connection because the connection is
    /// the wrong scope twice over: a turn observed completing live never records
    /// anything connection-local, and whatever it did record would be gone at
    /// the next reconnect — which is precisely when a resume answer arrives.
    ///
    /// **Asked by the terminal's own identity, not by `turn_id`, because a turn
    /// id names a turn only WITHIN a thread.** The link has held that rule since
    /// it was written — its debt map is keyed `(thread, turn)` — and a session
    /// spans as many threads as the user makes. A query on `turn_id` alone reads
    /// thread A's settled turn as thread B's genuinely running one and declines
    /// to cancel a doorbell that has already stopped being true. The id is built
    /// by [`crate::codex_adapter::turn_terminal_source_event_id`], the same
    /// function that stamps the fact, so the ask and the write cannot drift.
    ///
    /// The predicate is exactly the dedup key `append_event` files under
    /// (`session_uid`, `source`, `source_event_id`), which is what makes "has it
    /// been filed?" and "would filing it be a duplicate?" the same question, and
    /// what puts this read on that tuple's unique index.
    pub fn turn_terminal_filed(
        &self,
        session_uid: &str,
        terminal_source_event_id: &str,
    ) -> Result<bool> {
        let conn = self.read();
        let filed: bool = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM events
                  WHERE session_uid = ?1 AND source = ?2 AND source_event_id = ?3
                    AND kind = ?4
             )",
            params![
                session_uid,
                protocol::event::Source::Codex.as_str(),
                terminal_source_event_id,
                protocol::event::EventKind::TurnComplete.as_str()
            ],
            |row| row.get(0),
        )?;
        Ok(filed)
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
    ///
    /// **Which table a run is written to is decided by its `agent`, here and
    /// nowhere else.** Claude goes to `sessions`, everything else to
    /// `codex_sessions`; that single choice is what keeps `sessions` Claude-only
    /// and therefore keeps a rolled-back v0.6.0 daemon's sweeps off the rest of
    /// the fleet. See [`create_schema`].
    ///
    /// The cross-table check is the invariant `all_sessions` depends on: one uid
    /// in both tables would be returned twice by the view, and `get_session`
    /// would answer with whichever the planner reached first. So a write whose
    /// agent disagrees with where the row currently lives **moves the row**
    /// rather than adding a second copy.
    ///
    /// **Moving, not refusing, and that choice is deliberate.** A re-registration
    /// really can arrive under a different agent — `register_supervisor` treats
    /// the registration as the authority on which agent a run is, and
    /// `the_resolver_never_pairs_one_registrations_row_with_anothers_addressee`
    /// exercises exactly that path. Whether a run may change agents at all is a
    /// question about registration adoption (plan A5.1, generation-aware
    /// adoption), not about storage, and answering it here would be this layer
    /// inventing a policy the layer that owns it has not adopted. So the store
    /// does the one thing it can do without deciding anything: it carries the
    /// whole row across first, and then applies the ordinary upsert on top of
    /// it. The `COALESCE`d fields survive the move because the row that arrives
    /// in the new table is the row that left the old one — a delete-and-insert
    /// would silently drop the transcript path and the Codex identity a later
    /// heartbeat does not carry.
    ///
    /// The carry is `SELECT *`, which is only correct because the two tables
    /// have one column order — `both_session_tables_have_one_shape` is what
    /// keeps that true.
    ///
    /// **Writes no Codex generation.** Every caller but one is a writer that
    /// knows nothing about visits — a hook, a heartbeat, a tailer, a fixture —
    /// and a generation is a fact about a *registration*. `None` is
    /// `COALESCE`d away by the statement below, so none of them can blank a
    /// high-water they were never told about. The registration path uses
    /// [`Store::upsert_session_at_generation`].
    pub fn upsert_session(&self, row: &SessionRow) -> Result<SessionUpsert> {
        self.upsert_session_at_generation(row, None)
    }

    /// The same write, carrying the **Codex generation this registration was
    /// accepted at** (plan A5.1, clause "durable high-water evidence").
    ///
    /// A second method rather than a fourteenth field on [`SessionRow`], and the
    /// reason is what the column is *for*. `SessionRow` is the fleet projection
    /// — the shape `all_sessions` returns, the shape every reader in the daemon
    /// decodes, the shape twenty-one call sites construct. The generation is
    /// none of those things: it is written by one caller, read by one guard, and
    /// projected to nobody. Putting it on the row would have twenty-one writers
    /// declaring a fact only one of them can know, and would put a
    /// registration's high-water in reach of every heartbeat that builds a row
    /// literal.
    ///
    /// It is nonetheless **one statement**, not a second write chased after the
    /// first, because the ordering clause A5.1 turns on is that a refused frame
    /// mutates nothing and an accepted one leaves the row and its generation
    /// agreeing. Two statements would leave a crash window where the row names a
    /// registration whose generation was never recorded — and the next daemon
    /// would read a high-water older than the session it is looking at.
    ///
    /// `None` means "this write knows nothing about generations", and the
    /// `COALESCE` below keeps whatever is there — the same rule the Codex
    /// identity columns already follow, for the same reason: a later write with
    /// no news must not blank a known value.
    pub fn upsert_session_at_generation(
        &self,
        row: &SessionRow,
        codex_generation: Option<u64>,
    ) -> Result<SessionUpsert> {
        // SQLite stores integers as `i64` and rusqlite binds them as such, so a
        // `u64` above `i64::MAX` has no representation here. This used to
        // *saturate* — pin to `i64::MAX` — on the argument that no run can reach
        // that number anyway. The argument was sound and the fallback was not:
        // saturating writes a high-water that disagrees with the one the caller
        // holds in memory, which makes the durable/incoming equality the adoption
        // guard turns on (plan A5.1) read false for a generation that was in fact
        // adopted, and makes a restart reload a *lower* high-water than the daemon
        // had. Silently storing a different number than the caller asked for is
        // the failure mode, and the size of the number is not what makes it one.
        //
        // So the write refuses instead. Not the first line of defence — a
        // registration is refused far earlier, before any mutation at all, by
        // `ControlLink::from_registration`, which is where the argument about what
        // a legitimate producer can mint is written out — but the last one, and
        // the one that makes "what is in this column is what a caller passed" a
        // property of the column rather than of its callers.
        let codex_generation = codex_generation
            .map(i64::try_from)
            .transpose()
            .with_context(|| {
                format!(
                    "refusing to record a Codex generation above {} for {}: SQLite would store a \
                     different number than the registration claimed",
                    i64::MAX,
                    row.session_uid
                )
            })?;
        let (table, other) = session_tables_for(&row.agent);
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Inside the transaction that writes, so the row cannot move, or be
        // deleted, between the carry and the upsert that expects it.
        //
        // **The same tombstone guard as the upsert below, and it is not
        // redundant.** The argument for leaving it off was that a tombstone is
        // only ever written in the same transaction that deletes the run from
        // *both* tables, so a tombstoned uid has no row anywhere for this to
        // find. That is true of every delete **this** build performs — and the
        // build this schema exists to survive performs a different one. v0.6.0's
        // prune deletes from `sessions` and writes `deleted_sessions`; it has no
        // statement that names `codex_sessions` and cannot touch it. So a uid can
        // end up tombstoned with an isolated row still standing, and without this
        // clause the next registration under the other agent would carry that row
        // into `sessions`, watch the guarded upsert below write zero, commit the
        // carry anyway, and return `Tombstoned` for a run it had just
        // resurrected. One indexed lookup makes the invariant structural instead
        // of an argument about another binary's delete.
        let carried = tx.execute(
            // Both table names are compile-time literals from
            // `session_tables_for`; nothing a caller supplies reaches the text.
            &format!(
                "INSERT INTO {table} SELECT * FROM {other}
                  WHERE session_uid = ?1
                    AND NOT EXISTS (SELECT 1 FROM deleted_sessions WHERE session_uid = ?1)"
            ),
            params![row.session_uid],
        )?;
        if carried == 1 {
            tx.execute(
                &format!("DELETE FROM {other} WHERE session_uid = ?1"),
                params![row.session_uid],
            )?;
        }
        let changed = tx.execute(
            &format!(
                "INSERT INTO {table}(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                     claude_session_id, transcript_path, lifecycle,
                                     created_at, updated_at,
                                     agent, codex_thread_id, codex_socket, codex_generation)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
                 WHERE NOT EXISTS (SELECT 1 FROM deleted_sessions WHERE session_uid = ?1)
                 ON CONFLICT(session_uid) DO UPDATE SET
                    session_id        = excluded.session_id,
                    tmux_session      = excluded.tmux_session,
                    tmux_socket       = excluded.tmux_socket,
                    cwd               = excluded.cwd,
                    -- COALESCE keeps a known value when a later update has none:
                    -- SessionStart learns the transcript path, a heartbeat does not.
                    claude_session_id = COALESCE(excluded.claude_session_id, {table}.claude_session_id),
                    transcript_path   = COALESCE(excluded.transcript_path, {table}.transcript_path),
                    lifecycle         = excluded.lifecycle,
                    updated_at        = excluded.updated_at,
                    -- The agent is set by whoever introduces the run and is the
                    -- authority; the Codex identity columns are COALESCE-preserved so
                    -- a later heartbeat that does not carry them cannot blank them —
                    -- with the one exception the thread's own note below argues for.
                    agent             = excluded.agent,
                    -- **The thread is a fact about the visit named beside it,
                    -- so it does not outlive that visit.** `COALESCE` alone
                    -- kept the previous visit's thread while the generation
                    -- moved on, and the pair it left behind — a generation
                    -- carrying a thread no registration at that generation ever
                    -- named — is read as a BINDING by the adoption guard
                    -- (`Daemon::register_supervisor`, plan A5.1). Traced:
                    -- `(G1,A)` then `(G2,none)` then `(G2,B)` refused B, on the
                    -- evidence of a `(G2,A)` that was never an acceptance. The
                    -- same `COALESCE` also carried a pre-A5.1 row's thread into
                    -- the first generation ever recorded for it.
                    --
                    -- So a write that ADVANCES the generation writes the thread
                    -- it actually carries, absence included, and only a write at
                    -- or below the standing generation `COALESCE`s. That makes
                    -- true of the row the one thing the guard already assumes:
                    -- **the pair reads: the registration recorded at this
                    -- `codex_generation` is currently on this
                    -- `codex_thread_id`.** Note what that does and does not
                    -- say. It does NOT say a registration at this generation
                    -- named this thread — on a healthy run none ever does, and
                    -- after a `/new` the thread here is the session's second or
                    -- third while the generation is still the registration's
                    -- first. What it says is that the two are one live fact
                    -- about one registration, which is exactly what the
                    -- adoption guard needs and all it reads them for. No second
                    -- column is needed to record when the thread was bound,
                    -- because the generation beside it names the registration
                    -- that owns it — and a second column would have to be added
                    -- to both tables, kept in the `INSERT … SELECT *` carry,
                    -- kept out of `move_codex_sessions`' freshest-wins family
                    -- and proven rollback-invisible, all to store a number this
                    -- one already holds.
                    --
                    -- **The thread has a second writer, and it is the reason
                    -- the sentence above is phrased about the REGISTRATION
                    -- rather than about a registration having named a thread.**
                    -- A registration is written
                    -- before the app-server has announced a thread, so on a
                    -- healthy run it names none and the column stayed empty for
                    -- the whole life of the session. The control link is what
                    -- learns the thread, and it writes it through
                    -- [`Store::bind_codex_thread`] — scoped to the generation
                    -- its own REGISTRATION was accepted at, which is this one
                    -- and stays this one for as long as the link lives. The
                    -- link's live visit generation moves with every thread it
                    -- adopts and is deliberately not what the write is keyed
                    -- by: a `/new` is a new visit, the row is still the same
                    -- registration's, and a write scoped to the visit would
                    -- match nothing at exactly the moment the row most needs
                    -- updating. The ELSE arm here
                    -- is what preserves that write across every later
                    -- registration at the same generation: a restart
                    -- re-registers with no thread, `COALESCE` keeps the one the
                    -- link recorded, and a relaunch — a strictly later
                    -- generation — blanks it, which is right, because its
                    -- thread is a new one nobody has adopted yet.
                    --
                    -- Every other writer is untouched, and structurally rather
                    -- than by convention: a hook, a heartbeat or a tailer reaches
                    -- this through [`Store::upsert_session`], whose `None` makes
                    -- `excluded.codex_generation` NULL, fails the first test, and
                    -- takes the ELSE arm — the `COALESCE` those writers have
                    -- always had. `-1` is a sentinel below every generation a
                    -- producer mints (the coordinator starts at 1) and is there
                    -- only so a row predating the column compares as lower
                    -- instead of as NULL.
                    codex_thread_id   = CASE
                                          WHEN excluded.codex_generation IS NOT NULL
                                           AND excluded.codex_generation >
                                               COALESCE({table}.codex_generation, -1)
                                          THEN excluded.codex_thread_id
                                          ELSE COALESCE(excluded.codex_thread_id,
                                                        {table}.codex_thread_id)
                                        END,
                    codex_socket      = COALESCE(excluded.codex_socket, {table}.codex_socket),
                    -- The high-water moves with the row that carries it, and is
                    -- `COALESCE`d for the same reason the two columns above are:
                    -- a heartbeat or a hook re-writing this row knows nothing
                    -- about visits and must not blank the generation a
                    -- registration recorded. Nothing here refuses a *lower*
                    -- generation — that refusal belongs to registration
                    -- adoption, which decides it before it ever calls this
                    -- (`Daemon::register_supervisor`, plan A5.1) — and putting
                    -- a MAX() here instead would be this layer inventing an
                    -- adoption policy, which is the mistake the note above on
                    -- agent changes already declines to make.
                    codex_generation  = COALESCE(excluded.codex_generation, {table}.codex_generation)"
            ),
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
                row.agent.as_str(),
                row.codex_thread_id,
                row.codex_socket,
                codex_generation,
            ],
        )?;
        tx.commit()?;
        // `INSERT ... SELECT ... WHERE NOT EXISTS` writes zero rows exactly when
        // the tombstone matched; `ON CONFLICT` paths always write one.
        Ok(if changed == 0 {
            SessionUpsert::Tombstoned
        } else {
            SessionUpsert::Present
        })
    }

    /// The **durable Codex generation high-water** for one uid (plan A5.1).
    ///
    /// `None` for a uid with no row, for a Claude run, and for a Codex row
    /// written before this column existed — all three mean the same thing to the
    /// caller ("nothing has been adopted here that I can prove"), so they are
    /// deliberately not distinguished.
    ///
    /// **Not projected through `all_sessions`, and asked of both physical tables
    /// instead.** The view's shape is [`SessionRow`]'s shape — `session_row_from`
    /// decodes it positionally — and widening it would mean recreating a view
    /// that every database already has under `CREATE VIEW IF NOT EXISTS`. So this
    /// asks the two tables directly. It is still a question about the *fleet*
    /// rather than about a table, which is the rule
    /// `every_existence_guard_asks_about_the_whole_fleet` states: a Codex run's
    /// row lives in `codex_sessions` today, but a rollback can leave a uid with a
    /// row in each, and answering out of one table would then answer about half
    /// the evidence.
    ///
    /// `MAX` over the union is what makes that safe: two rows for one uid is the
    /// transient `move_codex_sessions` repairs at open, and while it lasts the
    /// **higher** generation is the one a stale-generation refusal must be made
    /// against. `MAX` over an empty set is `NULL`, which is the same `None` as a
    /// row with no generation — see above for why that conflation is intended.
    pub fn codex_generation(&self, session_uid: &str) -> Result<Option<u64>> {
        let conn = self.read();
        let generation: Option<i64> = conn.query_row(
            "SELECT MAX(g) FROM (
                 SELECT codex_generation AS g FROM sessions       WHERE session_uid = ?1
                 UNION ALL
                 SELECT codex_generation AS g FROM codex_sessions WHERE session_uid = ?1
             )",
            params![session_uid],
            |row| row.get(0),
        )?;
        // Only this build writes the column and it writes a non-negative
        // `i64`, so a negative value is a corrupt or hand-edited database.
        // Read as "no provable high-water" rather than cast into a colossal
        // `u64` that would refuse every registration this session ever makes.
        Ok(generation.and_then(|g| u64::try_from(g).ok()))
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
                        claude_session_id, transcript_path, lifecycle, created_at, updated_at,
                        agent, codex_thread_id, codex_socket
                   FROM all_sessions WHERE session_uid = ?1",
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
                        claude_session_id, transcript_path, lifecycle, created_at, updated_at,
                        agent, codex_thread_id, codex_socket
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

    /// The whole fleet, both agents, oldest first.
    ///
    /// Reads `all_sessions` — see [`create_schema`]. This daemon owns Codex runs
    /// as well as Claude ones, so every read projection answers for both; only
    /// the *old* daemon is meant to be blind to half of them, and it is blind by
    /// not knowing the name of the second table, not by anything written here.
    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.read();
        let mut stmt = conn.prepare(
            "SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                    claude_session_id, transcript_path, lifecycle, created_at, updated_at,
                    agent, codex_thread_id, codex_socket
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
        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT lifecycle, tmux_socket, session_id FROM all_sessions
                  WHERE session_uid = ?1
                  ORDER BY (lifecycle = ?2 OR tmux_socket = '') ASC
                  LIMIT 1",
                params![session_uid, lifecycle_str(Lifecycle::Exited)],
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
                      WHERE session_uid = ?1 AND (lifecycle = ?2 OR tmux_socket = '')"
                ),
                params![session_uid, lifecycle_str(Lifecycle::Exited)],
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
            // **Ended has to be true of every copy of the uid, not of one.** The
            // same duplicate `delete_exited_session` guards against: a rollback
            // shadow reading `exited` in `sessions` beside the live isolated run
            // in `codex_sessions`. Without the `NOT EXISTS` the shadow is a
            // candidate, the loop below deletes the uid's children before it
            // touches either identity row, and the identity delete then restates
            // `lifecycle = 'exited'` — matching the shadow and not the live copy,
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
                  WHERE a.lifecycle = ?1
                    AND NOT EXISTS(SELECT 1 FROM all_sessions AS b
                                    WHERE b.session_uid = a.session_uid
                                      AND b.lifecycle <> ?1)
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
                        &format!("DELETE FROM {table} WHERE session_uid = ?1 AND lifecycle = ?2"),
                        params![session_uid, lifecycle_str(Lifecycle::Exited)],
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
    /// two reads land in different milliseconds. That is invisible while the uid
    /// is in exactly one table, which is the invariant, and it is exactly the
    /// wrong thing to have when it is not: [`move_codex_sessions`] decides a
    /// whole column family by comparing the two copies' `updated_at`, so two
    /// stamps for one transition are what would let a merge keep one row's
    /// `lifecycle` and discard the other's newer `cwd`. Hoisting the read costs
    /// nothing and means the tear has no mechanism, rather than no reachable
    /// caller.
    ///
    /// **One stamp is not the whole answer, and this is only half of it.** A
    /// single stamp written to two divergent copies *levels* them, and an equal
    /// `updated_at` is exactly what the merge reads as "the shared side is at
    /// least as fresh". So the other half lives in [`move_codex_sessions`],
    /// which compares per family: `lifecycle` on `>=`, and every location column
    /// on strict `>`, because this write advances a stamp without touching a
    /// location and its equality is therefore not evidence about one.
    ///
    /// Scoping this statement to the rows whose `lifecycle` actually changed was
    /// the other candidate and it was **measured not to be a fix**: when both
    /// copies genuinely change — the ordinary case, two `live` rows being ended
    /// — both are still stamped, still levelled, and the merge still hands the
    /// stale rollback copy the whole family.
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

    /// **Write down the thread a control link has actually adopted**, against the
    /// generation that link belongs to.
    ///
    /// The column has one other writer — a registration, through
    /// [`Store::upsert_session_at_generation`] — and a registration happens before
    /// the app-server has said which thread this session is on. So on a healthy run
    /// nothing ever named one, and the row said a Codex session was on no thread
    /// while the live link knew exactly which. Everything that reads the row rather
    /// than the link saw the emptier answer, and a restarted daemon saw only that.
    ///
    /// **Scoped to the generation, and that is the fence rather than a filter.** The
    /// row's thread and its generation are one fact: a generation is one visit, one
    /// codex process, one thread, which is what lets the adoption guard read the
    /// pair as a binding. A link whose registration has been superseded is still
    /// running for as long as it takes to notice, and an unscoped write would let it
    /// stamp its thread onto the visit that replaced it — the exact confusion the
    /// guard exists to refuse, arriving from underneath instead.
    ///
    /// **`updated_at` is deliberately left alone.** That stamp arbitrates the
    /// rollback merge in [`move_codex_sessions`], which decides the location family
    /// by comparing the two copies; `codex_thread_id` is not in that family at all,
    /// and it is written here without touching a location column. Bumping the stamp
    /// would be this write casting a vote in an argument it is not part of.
    ///
    /// Both tables, for the reason [`Store::set_lifecycle`] gives: the uid is a
    /// primary key in each and lives in exactly one, so the statement that does not
    /// match writes nothing, and no wrong guess about the agent can silently drop
    /// the write.
    ///
    /// Returns whether a row actually changed — `false` for a row already carrying
    /// this thread, and for a generation this link does not speak for.
    pub fn bind_codex_thread(
        &self,
        session_uid: &str,
        generation: u64,
        thread_id: &str,
    ) -> Result<bool> {
        let conn = self.write();
        let generation = i64::try_from(generation).unwrap_or(i64::MAX);
        let mut changed = 0usize;
        for table in SESSION_TABLES {
            changed += conn.execute(
                // A compile-time list in this file; nothing a caller supplies
                // reaches the statement text.
                &format!(
                    "UPDATE {table} SET codex_thread_id = ?3
                       WHERE session_uid = ?1
                         AND codex_generation = ?2
                         AND (codex_thread_id IS NULL OR codex_thread_id <> ?3)"
                ),
                params![session_uid, generation, thread_id],
            )?;
        }
        Ok(changed > 0)
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
    ///
    /// **`features` and `features_epoch` are deliberately not in that tuple, and
    /// this statement does not touch them.** It used to write the device's
    /// advertised agent list beside the token, so two overlapping registrations
    /// could not commit their tokens A→B and their sets B→A. No shipping client can
    /// send that field, so the write was acting on an input the wire cannot
    /// produce and it is gone. The columns stay and are left exactly as they are —
    /// which today is `NULL` on every row, the Claude floor
    /// [`Store::push_targets`] reads. Phase 5 brings the write back into this
    /// transaction, with the phone that exercises it, for the reason it was here.
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
    ///
    /// **The feature set rides the same row, for the same reason.** Which agents a
    /// device can render decides whether it may be rung at all, and reading it
    /// separately would be a second read to pair with the first — the very thing
    /// the credential note above exists to forbid. `epoch` is this run's
    /// [`crate::state::feature_epoch`]; a stored set stamped with any other one is
    /// [`DeviceFeatures::Unconfirmable`], and that device hears nothing until it
    /// re-advertises to *this* run — which is what makes a restart start from the
    /// honest floor rather than from whatever the last process was told.
    ///
    /// **Nothing writes that column in this phase**, so in a live database it is
    /// `NULL` on every row and this decode always answers the Claude floor. The
    /// other branches are kept rather than deferred because they cost one string
    /// comparison, because they are what the column *means*, and because the write
    /// Phase 5 turns on must arrive at a read that already refuses everything it
    /// cannot vouch for — not at one that has to be taught to.
    pub fn push_targets(&self, epoch: &str) -> Result<Vec<PushRegistration>> {
        let conn = self.read();
        let mut stmt = conn.prepare(&format!("{PUSH_TARGET_COLUMNS} {PUSH_TARGET_ELIGIBLE}"))?;
        let rows = stmt.query_map([], |row| push_registration(row, epoch))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One device's push registration, or `None` when it has none to offer.
    ///
    /// **The same question [`Store::push_targets`] answers, asked about one row.**
    /// The predicate is the same and so is the decode — both come from the
    /// constants below, so the fan-out's list and this lookup cannot come to
    /// disagree about who may be pushed to or about what a stored feature set
    /// means. A device that is revoked, has no token, or does not exist answers
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
    pub fn push_target_for(
        &self,
        device_id: &str,
        epoch: &str,
    ) -> Result<Option<PushRegistration>> {
        let conn = self.read();
        Ok(conn
            .query_row(
                &format!("{PUSH_TARGET_COLUMNS} {PUSH_TARGET_ELIGIBLE} AND device_id = ?1"),
                params![device_id],
                |row| push_registration(row, epoch),
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

    /// Replace a device's advertised feature set (its agent list, as JSON) under
    /// the current daemon feature epoch. `Some(json)` records the set; `None`
    /// clears the column, which is the row a device that advertised **nothing**
    /// leaves — so absence *overwrites* any prior set rather than leaving stale
    /// Codex eligibility standing.
    ///
    /// **Nothing in the daemon calls this.** No shipping client can send a
    /// `features` field — `hello` and `register_push` both carry the wire-legal
    /// `Option<ClientFeatures>` and both ignore it — so the handlers that used to
    /// call this were machinery for an input the wire cannot produce, and they are
    /// gone. Every `devices.features` cell is `NULL`, which is the Claude floor.
    ///
    /// What it survives as is the **fixture writer for the read side**: the tests
    /// that assert [`Store::push_targets`] tells a confirmed set from another run's
    /// stamp, from bytes that will not decode, and from an empty column need rows in
    /// all four shapes, and this is the only thing that can produce them. Kept
    /// rather than re-derived from raw SQL in each test, because it is also the
    /// write Phase 5 turns on when a phone that advertises arrives with the wire
    /// that exercises it.
    ///
    /// **`None` means the device said nothing, and nothing else.** It is not a
    /// repair for a write that failed: clearing writes the empty legacy set, which
    /// grants Claude ([`protocol::ws::ClientFeatures::supports`]), so using it to
    /// tidy up after a failure would hand a `[Codex]`-only phone doorbells its own
    /// claim excluded. See [`DeviceFeatures`] for the read that keeps the same rule.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_device_features(
        &self,
        device_id: &str,
        features_json: Option<&str>,
        epoch: &str,
    ) -> Result<()> {
        let conn = self.write();
        match features_json {
            Some(json) => conn.execute(
                "UPDATE devices SET features = ?2, features_epoch = ?3 WHERE device_id = ?1",
                params![device_id, json, epoch],
            )?,
            None => conn.execute(
                "UPDATE devices SET features = NULL, features_epoch = NULL WHERE device_id = ?1",
                params![device_id],
            )?,
        };
        Ok(())
    }

    // The read side — projecting a device's stored feature set into a push
    // authorization decision — is `push_targets`. It decodes this column beside
    // the token the same row authorizes, and fails closed on every input that is
    // not a set confirmed under the running daemon's own epoch.
    //
    // **There is deliberately no startup sweep of stale sets.** There was one:
    // it cleared every set not stamped with this run's epoch, on the reasoning
    // that a rollback must not resurrect Codex eligibility. The read already
    // refuses those rows, so the sweep never made a device less eligible — and
    // once the read learned to tell `NULL` from an unconfirmable set
    // ([`DeviceFeatures`]), the sweep could only make one MORE eligible: it
    // rewrote a stored `[Codex]` claim into the empty legacy set, which is the
    // Claude floor. A tidy-up that broadens authorization is not a tidy-up, and
    // the untouched bytes are what carry the distinction the read depends on.
    //
    // In this phase there is nothing to sweep either way: no handler writes this
    // column, so every row holds `NULL` and reads as the floor. The read's other
    // branches are what the write Phase 5 lands will meet.

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
            // `SELECT ... WHERE EXISTS(all_sessions)` rather than VALUES: an
            // approval for a session that was deleted mid-flight must not leave
            // an orphan row that is later recovered as an indeterminate answer
            // for a run nobody can name.
            //
            // **`pending_approvals` is not agent-scoped, and that is a bound
            // obligation, not an oversight.** A v0.6.0 daemon reads this table
            // GLOBALLY — `list_pending_approvals` there does not go through a
            // `sessions` row — so a Codex card sitting here after a rollback is
            // visible to it, and its recovery can delete one.
            //
            // The guard above deliberately does not stop that: it asks
            // `all_sessions`, so it accepts a Codex uid. What stops it is
            // upstream, in the daemon — `Daemon::handle_permission_request`
            // refuses a non-Claude session before a card is built at all, which
            // is scaffolding that lifts when Phase 3 splits this table. The
            // refusal is up there rather than down here on purpose: a store
            // guard that quietly dropped a Codex card would report success and
            // lose the card, which is the failure this whole chunk exists to
            // prevent in the other direction. See [`create_schema`].
            "INSERT INTO pending_approvals(session_uid, session_id, request_id, card,
                                           generation, created_ms)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
              WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)
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
    /// One `BEGIN IMMEDIATE` removes both orderings. `append_batch_with_cursor`
    /// took the same shape for the same reason: a projection and the fact it
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
            "SELECT p.session_uid, p.session_id, p.request_id, p.card, p.generation, p.created_ms
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
                    //
                    // The same bound obligation as `pending_approvals`, and the
                    // sharper one: a rolled-back v0.6.0 daemon does not merely
                    // read this table globally, its `recover_text_mutations`
                    // WRITES to it — every `applying` row becomes
                    // `indeterminate`, whoever it belongs to.
                    //
                    // And this row is written EARLY: `Daemon::send_text` claims
                    // here before it has any idea whether a supervisor is
                    // attached, so a crash between the two used to leave an
                    // `applying` row for the old daemon to rewrite. What stops
                    // that is `send_text` refusing a non-Claude session before
                    // it reaches this call — scaffolding that lifts when Phase 3
                    // splits the table. See [`create_schema`].
                    "INSERT INTO text_mutations(session_uid, request_id, payload_hash,
                                                status, matched, started_at, settled_at)
                     SELECT ?1, ?2, ?3, 'applying', NULL, ?4, NULL
                      WHERE EXISTS (SELECT 1 FROM all_sessions WHERE session_uid = ?1)",
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

    // ---------------------------------------------------- generalized ledger
    //
    // The generalized mutation ledger, keyed `(operation_kind, session_uid,
    // client_request_id)`. It is the same claim-before-write, lookup-then-
    // conflict primitive as `claim_text_mutation`/`settle_text_mutation` above,
    // widened so any Codex mutation (answer, compose, interrupt) shares one
    // idempotency law. Phase 1 shipped the schema and this primitive with its
    // conflict semantics proven and no caller; Phase 3b wired the first producer
    // to it — a phone answer claims here under `operation_kind = "answer"` — so
    // the `cfg_attr(not(test), allow(dead_code))` this used to carry is gone,
    // because the code is live.

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
        let mut conn = self.write();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, Option<String>, String, ClaimedMaterial)> = tx
            .query_row(
                "SELECT claimed_hash, status, outcome, started_at,
                        thread_id, generation, route, target_turn_id
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
                                                 settled_at)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'applying', NULL, ?9, NULL
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
                        target_turn_id
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
                    route, target_turn_id, started_at
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
                    route, target_turn_id, started_at
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
/// ### The obligation this leaves open, named so Phase 3 cannot miss it
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
/// first written down. It was "no producer exists". That was **wrong**: three
/// daemon paths reach these tables agent-agnostically today — `send_text`,
/// which claims `text_mutations` before it knows whether a supervisor is even
/// attached; the `PermissionRequest` hook, whose `ensure_session` *preserves* an
/// existing row's agent rather than filtering on it; and `answer`, which claims
/// `answer_claims` before every one of its refusals. All three would have
/// written for a Codex run.
///
/// So the real reason is a decision: **each of those three producers now refuses
/// a non-Claude session outright, before any durable write.** That is
/// scaffolding, deliberately — it makes Codex approvals and Codex text mutations
/// impossible rather than merely unbuilt, and it is what a Phase-3 split
/// removes. It is cheap and reversible where splitting four tables now would be
/// machinery built ahead of the wire that will shape it. The refusals live in
/// `Daemon::send_text`, `Daemon::handle_permission_request` and `Daemon::answer`;
/// `send_text_to_a_codex_session_is_refused_before_it_claims_anything`,
/// `a_permission_request_for_a_codex_session_raises_no_card` and
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
/// refusals in front of a **registered** Codex session instead of one staged into
/// the store, which is a stronger claim than the one they were making — and they
/// are what a Phase-3 split has to change on purpose. The day the split stops
/// being speculative is therefore the day somebody edits those refusals, not a
/// day a test goes red on its own.
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
            updated_at        TEXT NOT NULL,
            -- The agent hosting this run. NOT NULL with a 'claude' default so a
            -- legacy row a migration backfills, and every row written before the
            -- agent seam, reads as Claude — which is exactly what it is. Decoded
            -- through `AgentKind::from_str_lossy`, so an unrecognised value fails
            -- closed rather than posing as Claude.
            --
            -- Every row in THIS table is 'claude' — the move below keeps it that
            -- way, and `upsert_session` routes by this value so nothing else can
            -- land here. The column stays because it is what the move is defined
            -- over: a rollback writes rows here again, and the way back up has to
            -- be able to ask which of them are not Claude's. Decoded through
            -- `AgentKind::from_str_lossy`, so an unrecognised value fails closed
            -- rather than posing as Claude.
            agent             TEXT NOT NULL DEFAULT 'claude',
            -- Codex thread identity and broker socket. NULL for Claude and for
            -- any row predating the seam. Carried on this table too, so the two
            -- tables have one shape and a row can be moved between them by
            -- copying columns rather than by mapping them.
            codex_thread_id   TEXT,
            codex_socket      TEXT,
            -- The **durable Codex generation high-water** (plan A5.1). NULL for
            -- Claude and for every row predating it; carried on this table for
            -- the same one-shape reason as the two columns above, and for no
            -- other — no row that lives here can ever have a value, because
            -- `upsert_session` routes a Codex run to `codex_sessions` and only a
            -- Codex registration writes this column. See `codex_sessions` below
            -- for what it means and why it is rollback-isolated in practice.
            codex_generation  INTEGER
        );

        -- Resolving a legacy `cc-1` to the newest run under that name.
        CREATE INDEX IF NOT EXISTS sessions_name ON sessions(session_id);

        -- Codex runs. The same fourteen columns in the same order as `sessions`
        -- — see the note on `create_schema` for why this is a second table and
        -- not a `WHERE agent = 'codex'`. `both_session_tables_have_one_shape`
        -- reads both back off the schema, so a column added to one and not the
        -- other fails a test instead of failing a query.
        CREATE TABLE IF NOT EXISTS codex_sessions(
            session_uid       TEXT PRIMARY KEY,
            session_id        TEXT NOT NULL,
            tmux_session      TEXT NOT NULL,
            tmux_socket       TEXT NOT NULL,
            cwd               TEXT NOT NULL,
            claude_session_id TEXT,
            transcript_path   TEXT,
            lifecycle         TEXT NOT NULL,
            created_at        TEXT NOT NULL,
            updated_at        TEXT NOT NULL,
            -- Declared identically to `sessions`, `DEFAULT 'claude'` and all,
            -- because "identical" is the property that matters: `upsert_session`
            -- moves a run between the two tables with `INSERT … SELECT *`, which
            -- is correct only while the column ORDER matches. Every insert here
            -- names `agent` explicitly, so the default is never reached; it is
            -- carried so the two declarations can be read side by side and seen
            -- to be the same.
            agent             TEXT NOT NULL DEFAULT 'claude',
            codex_thread_id   TEXT,
            codex_socket      TEXT,
            -- **The durable Codex generation high-water** (plan A5.1, clause
            -- "durable high-water evidence in rollback-isolated storage").
            --
            -- The generation of the last registration this daemon ACCEPTED for
            -- this uid — the same quantity `SupervisorHandle::codex_generation`
            -- holds in memory, and nothing else. Measured, not assumed: that
            -- field has exactly one writer (`register_supervisor`, from
            -- `info.codex_generation`), and the link's own `visit.generation`
            -- bumps live on a connection-local `Visit` that is never written
            -- back to the handle. So a column written by the same acceptance
            -- restores the identical number after a restart, and the
            -- stale-generation refusal cannot be walked around by killing ccd.
            -- A durable value can never strand a live session, because the only
            -- thing it refuses is a generation the daemon has already adopted.
            --
            -- **Rollback-isolated in the sense the clause means.** Only a Codex
            -- registration ever writes a non-NULL value, and `upsert_session`
            -- files a Codex run here — in the table a v0.6.0 daemon does not
            -- know the name of, and cannot re-file into `sessions` because the
            -- shadow trigger below refuses the write. The twin column on
            -- `sessions` exists for column-order parity alone (`INSERT … SELECT *`
            -- carries a row between the tables) and holds NULL on every row a
            -- v0.6.0 daemon can reach.
            --
            -- `INTEGER`, nullable, and read back as `Option<u64>`: NULL is "no
            -- generation has been adopted for this uid", which is what a Claude
            -- row, a pre-A5.1 row and an unregistered uid all are.
            codex_generation  INTEGER
        );

        -- The twin of `sessions_name`: without it, resolving a tmux name would
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
        -- **What it costs the old daemon is one refused registration, and that
        -- was measured against the real binary rather than reasoned about.**
        -- The error surfaces from `upsert_session` through
        -- `register_supervisor` into v0.6.0's `read_loop`, which ends *that
        -- connection* (logging it at debug) while the accept loop releases the
        -- permit and carries on. Driven against `8e5b172` on a v4 database
        -- holding one live Codex run with two events under it:
        --
        -- ```text
        -- old daemon opens it:      user_version 4 -> 3, serves normally
        -- register(codex uid):      connection closed, no ack; daemon ALIVE
        -- register(claude uid):     {"type":"ack"}
        -- list_sessions:            the Claude run, as normal
        -- register(codex) again:    closed again; daemon still ALIVE
        -- afterwards:               0 shadow rows, codex row byte-identical,
        --                           2 events intact, 0 tombstones
        -- ```
        --
        -- So a Codex run stays unregistered on a rolled-back daemon — which is
        -- the outcome the supervisor's own `withhold_unless_hosted` already
        -- produces, and the only alternative on offer is the deleted history
        -- above. Claude is untouched: its uid is never in `codex_sessions`, so
        -- the `WHEN` clause is false and the statement is not even examined.
        --
        -- **What it costs the hot path, measured rather than waved at.** Every
        -- insert into `sessions` pays one indexed seek into `codex_sessions`.
        -- Over 3,000 committed Claude upserts against 200 Codex rows: 23.3us to
        -- 42.1us each. That is a real 1.8x on the statement and it is nothing on
        -- the daemon — a session is upserted on registration and on hooks, not
        -- per heartbeat, so a fleet writing a hundred times a second spends
        -- under two milliseconds of that second on it. The `EXISTS` seeks the
        -- `codex_sessions` primary key; there is no cheaper shape that still
        -- answers the question.
        --
        -- **And it does not catch this build's own writes.** The one statement
        -- here that inserts into `sessions` for a uid living in
        -- `codex_sessions` is `upsert_session`'s carry, which is
        -- `INSERT INTO sessions SELECT * FROM codex_sessions` — so `NEW.agent`
        -- is that row's real agent, not `'claude'`, and the guard is a
        -- registration re-filing a run under a different agent rather than a
        -- daemon that cannot see the second table. The guarded upsert that
        -- follows it runs after the source row has been deleted.
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
        -- A view, and named something v0.6.0 has never heard of, on purpose. It
        -- sits *beside* the real `sessions` table rather than shadowing it, so
        -- the old daemon still finds a table it can INSERT, UPDATE and DELETE —
        -- which the shadowing shape could not offer without triggers.
        --
        -- `UNION ALL`, not `UNION`: a uid lives in exactly one of the two tables
        -- (`upsert_session` moves a run rather than copying it), so there is
        -- nothing to deduplicate and `UNION` would only buy a sort.
        --
        -- Measured, not assumed, on the owner's real store: SQLite pushes the
        -- predicate into both branches rather than materialising the union
        -- first. `WHERE session_uid = ?` becomes two covering-index seeks,
        -- `WHERE session_id = ?` two seeks on `sessions_name` /
        -- `codex_sessions_name`, and the prune's `WHERE lifecycle = ?` two
        -- scans — which is exactly what it was against the one table, because
        -- there is no index on `lifecycle` and never was.
        CREATE VIEW IF NOT EXISTS all_sessions AS
            SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                   claude_session_id, transcript_path, lifecycle,
                   created_at, updated_at, agent, codex_thread_id, codex_socket
              FROM sessions
            UNION ALL
            SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                   claude_session_id, transcript_path, lifecycle,
                   created_at, updated_at, agent, codex_thread_id, codex_socket
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

        -- The generalized mutation ledger. `text_mutations` above is the proven
        -- shape for one operation (send_text); this is the same lookup-then-
        -- conflict pattern widened to any Codex mutation (answer, compose,
        -- interrupt) by keying on `operation_kind` as well. A retry is recognised
        -- by `(operation_kind, session_uid, client_request_id)` and replays its
        -- recorded outcome; a retry that reuses the id with *different* claimed
        -- material is a conflict, never a second actuation. The claimed material
        -- is a single hash over the full authorization surface (route, target
        -- turn, the exact displayed option set, cwd) and is immutable once
        -- written. Additive and agent-scoped by construction: only a Codex writer
        -- ever inserts here, so a rolled-back daemon that does not know the table
        -- neither reads nor mutates it.
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
            PRIMARY KEY(operation_kind, session_uid, client_request_id)
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
            push_credential   TEXT,
            -- The device's advertised feature set (its agent list) as JSON, and
            -- the feature epoch of the daemon run that confirmed it. **Null on
            -- every row today, and nothing in this phase writes either one**: no
            -- shipping client can send a `features` field, so the daemon has no
            -- input to record and does not pretend otherwise. Null is the Claude
            -- floor, which is also what every row predating the agent seam holds.
            -- The columns are declared now because the read (`push_targets`) is
            -- already fail-closed against them — a set stamped with another run's
            -- epoch, or bytes that will not decode, authorize nothing — and Phase 5
            -- turns the write on without a migration.
            features          TEXT,
            features_epoch    TEXT
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

/// Is there a non-Claude run still sitting in the shared `sessions` table?
///
/// **Asked of the data, never of `user_version`**, and that is the whole design
/// of this migration rather than a stylistic echo of
/// [`needs_session_uid_migration`]. A version gate would fire exactly once, on
/// the way up from 3 to 4. But the scenario this exists for is a *rollback*: a
/// v0.6.0 daemon opens the database, writes `user_version = 3` back
/// unconditionally, and may create session rows of its own while it runs. On
/// the way back up a version gate would either re-fire on a database that has
/// nothing to do (harmless) or — if it were remembered some other way — not
/// fire on one that does (a Codex row left where the *next* rollback deletes
/// it). Asking the rows removes the question: the move runs when there is
/// something to move, every start, whatever the number on the file says.
///
/// `agent <> 'claude'` rather than `agent = 'codex'`: an
/// [`AgentKind::Unsupported`] row is not Claude's either, and leaving it in a
/// table the old daemon prunes would be the same data loss for an agent we
/// understand even less.
///
/// ## Why the `agent` column is not the whole predicate
///
/// **A rollback can put a Codex run back in `sessions` wearing `claude`.**
/// v0.6.0 has no `agent` column of its own: its `upsert_session` names ten
/// columns, so the eleventh takes this schema's `DEFAULT 'claude'`. A Codex
/// supervisor that reconnects while the old daemon is running is therefore
/// re-filed into the swept table under the wrong agent, while its real row is
/// still sitting in `codex_sessions`. One uid, two tables, and every downstream
/// invariant broken at once: `all_sessions` is a `UNION ALL` and returns the run
/// twice, the old daemon's prune can walk the `sessions` copy and delete the
/// shared events underneath it, and the next agent flip through
/// [`Store::upsert_session`] carries a row onto a uid that is already a primary
/// key in the destination and fails.
///
/// An `agent`-only predicate never sees that row and the damage is permanent.
/// So the question this asks is not "is this row's agent Codex" but **"does this
/// row belong in `sessions` at all"**, and a uid that already exists in
/// `codex_sessions` answers no whatever the `agent` column says. Nothing this
/// build writes can make that pair — [`Store::upsert_session`] moves a run
/// rather than copying it — so the only thing that produces one is a rollback,
/// and treating it as Codex-owned is what makes a rollback round trip
/// self-healing rather than terminal. [`move_codex_sessions`] does the
/// reconciling.
///
/// ## The producer, and where it is actually closed
///
/// This repair is the *third* lock, and saying which ones come first matters,
/// because a repair that runs after the damage is not a substitute for not doing
/// the damage. Census of everything at `8e5b172` that can put a row in
/// `sessions` under a uid an external peer supplies: `ClientFrame::Register` →
/// `register_supervisor`, and `ClientFrame::Hook` → `ensure_session`. Nothing
/// else — `Heartbeat` reads and touches an in-memory map, `SessionExited` can
/// only `UPDATE` a row that already exists, and every other frame is read-only
/// or writes another table. The hook path is Claude's by construction: it is
/// driven by `cc-hook`, which a Codex run neither installs nor invokes.
///
/// So the producer is exactly one frame, and it is refused twice over:
///
///   1. **At the peer.** `codeconnect`'s supervisor withholds the frame — before
///      registering a non-Claude run it asks the daemon, on the same connection,
///      whether that agent is hosted, and a daemon that predates the agent seam
///      cannot even decode the question.
///   2. **At the database**, for the statement the withhold cannot reach: one
///      already in flight when this migration takes the write lock, or one from
///      a peer that is not this supervisor at all. `sessions_refuse_codex_shadow`
///      lives in the schema, so it fires for the old binary's own upsert
///      whenever it lands, and no argument about scheduling is needed anywhere.
///
/// What is left for this migration is the one thing neither can do: rows that
/// were already misfiled before the refusal existed. `sessions` carried the
/// `agent` column for a release before `codex_sessions` and the trigger did.
fn needs_codex_session_move(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "sessions")? || !column_exists(conn, "sessions", "agent")? {
        return Ok(false);
    }
    Ok(conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM sessions AS s WHERE {MISFILED})"),
        [],
        |row| row.get(0),
    )?)
}

/// A `sessions` row that does not belong in `sessions`.
///
/// One predicate, named once, so the question "which rows leave" cannot be
/// answered differently by the three statements that ask it: the check in
/// [`needs_codex_session_move`], and the insert and the delete in
/// [`move_codex_sessions`]. Written against the alias `s`.
///
/// The reconcile between those two does not name it and does not need to: its
/// `WHERE s.session_uid = c.session_uid` is the second disjunct written as a
/// join, so it matches exactly the rows this matches for that reason.
///
/// See [`needs_codex_session_move`] for why the second disjunct is there.
const MISFILED: &str = "s.agent <> 'claude'
     OR EXISTS(SELECT 1 FROM codex_sessions AS c WHERE c.session_uid = s.session_uid)";

/// What one run of [`move_codex_sessions`] did, split by which arm took it.
struct CodexSessionMove {
    /// Rows carried whole into an empty destination.
    moved: usize,
    /// Rows whose uid was already in `codex_sessions` and were merged into it.
    reconciled: usize,
}

/// Move every misfiled run out of `sessions` and into `codex_sessions`.
///
/// Runs inside [`Store::migrate`]'s `BEGIN IMMEDIATE`, so a run is never in both
/// tables and never in neither, and the `codex_sessions` table it writes into
/// becomes visible to another process in the same commit that empties `sessions`
/// of Codex rows. Interrupting the daemon mid-move — a `SIGKILL`, a power cut —
/// leaves the transaction unfinished, SQLite rolls it back out of the WAL on the
/// next open, and [`needs_codex_session_move`] answers `true` again because the
/// rows are still where they were. There is no half-moved state to detect and no
/// partial state to repair.
///
/// The events, answers and cursors filed under these uids are **not** touched:
/// they are keyed by uid, the uid does not change, and the whole point is that
/// nothing reaches them without a `sessions` row to walk from.
///
/// ## The collision arm, which is a repair and not a discard
///
/// [`needs_codex_session_move`] explains where a uid in both tables comes from:
/// a rollback, re-filing a reconnecting Codex run into `sessions` under the
/// `DEFAULT 'claude'` v0.6.0 cannot override. Both rows are then real history —
/// the isolated one holds the run's identity, the shared one holds whatever the
/// old daemon learned while it was in charge — so keeping either and dropping
/// the other loses something. They are **merged**, by one rule with two families:
///
///   * **Identity never regresses.** `created_at`, `agent`, `codex_thread_id`,
///     `codex_socket` and `codex_generation` are simply not in the `SET` list.
///     v0.6.0 cannot write any of them — it names ten columns and the other four
///     are not among them — so anything the shared copy holds for these is a
///     default or a `NULL`, never news. `codex_generation` belongs to this
///     family and not to freshest-wins for a reason beyond "v0.6.0 cannot write
///     it": it is the high-water a registration is refused against (plan A5.1),
///     so letting a rollback-era copy decide it would let a daemon downgrade
///     rather than only ever fail to advance.
///   * **Everything else is freshest-wins, decided together by `updated_at`.**
///     Census of v0.6.0's own writes to `sessions`: `upsert_session` sets
///     `session_id`, `tmux_session`, `tmux_socket`, `cwd`, `lifecycle`,
///     `updated_at` and `COALESCE`s `claude_session_id` / `transcript_path`;
///     `set_lifecycle` sets `lifecycle` and `updated_at`. Both advance
///     `updated_at` in the same statement, so one comparison decides the whole
///     family and cannot tear it. (Its third writer, the `claude:*` GLOB
///     normalizer, blanks a tmux location *without* touching `updated_at` — and
///     is the one v0.6.0 write this can never see, because it matches on a
///     `session_id` prefix `cc-hook` mints and a Codex run does not carry.)
///     `updated_at` is RFC3339 UTC at millisecond precision from a formatter
///     that is byte-identical in both builds, so the lexicographic comparison
///     SQLite performs is the chronological one.
///
/// The two `COALESCE`d columns keep the store's existing rule on top of that: a
/// known value is not blanked by a later write that has none, whichever side is
/// fresher.
///
/// ## The tie-break is per family, because an equal stamp means two things
///
/// An equal `updated_at` is not one situation. It is two, and they want opposite
/// answers:
///
///   * **A genuine tie** — two writes landing in the same millisecond. Here the
///     shared copy is the causally later one by construction: it exists only
///     because a rolled-back daemon was in charge and a run reconnected to it,
///     so whatever it holds was written after the isolated row stopped being
///     maintained.
///   * **A manufactured tie** — [`Store::set_lifecycle`] writes *both* physical
///     tables under one stamp, because the uid is a primary key in each and is
///     supposed to live in exactly one. When it does not, that one write levels
///     the two stamps while changing nothing but `lifecycle`. The equality is
///     then an artefact of our own write and says nothing about location at all.
///
/// So the comparison is split rather than picked:
///
///   * `lifecycle` keeps `>=`. On a genuine tie the shared side is the one that
///     was live most recently. On a manufactured tie the two sides already hold
///     the same value — `set_lifecycle` wrote it to both — so the direction
///     cannot decide anything, and the ratified reading of the genuine case is
///     kept intact.
///   * Everything else — `session_id`, `tmux_session`, `tmux_socket`, `cwd`, and
///     the two `COALESCE`d columns — takes strict `>`. A shared-side *location*
///     is news only if a v0.6.0 `upsert_session` actually wrote it, and that
///     write is precisely what makes `s.updated_at` strictly greater. An equal
///     stamp is therefore not evidence of a location write, and moving location
///     on it is what tears the family: the manufactured tie would hand the whole
///     of a stale rollback row over a `cwd` this build had since refreshed.
///
/// What strictness costs in the genuine case is nothing reachable. For the two
/// stamps to be equal there, the last isolated write and the first shared write
/// must fall in the same millisecond — with a daemon shutdown, a v0.6.0 start,
/// and a supervisor reconnect in between, whose backoff floor alone
/// (`RECONNECT_MIN`) is 500ms. And the `COALESCE`d pair loses nothing in either
/// direction: a tie takes the isolated value and falls back to the shared one,
/// so no known value is discarded, only ordered differently.
///
/// The identity columns are not in the `SET` list at all, so neither branch of
/// this can regress them.
///
/// **The bound this leaves, stated rather than papered over.** `updated_at` is a
/// wall stamp, and this repository's own [`protocol::time`] notes that wall time
/// can step backwards. A backward step *smaller* than the rollback window is
/// absorbed by the tie-break above, because the shared write still lands at or
/// after the isolated one. A backward step *larger* than the window would stamp
/// the causally-later shared write with an earlier time, and freshest-wins would
/// then keep the isolated row's `cwd` and `lifecycle`. That is the residual: it
/// costs the fields one old-daemon session advanced, never identity and never
/// history — the events are keyed by uid and are not part of this merge at all —
/// and closing it would need a monotonic write counter on a table v0.6.0 writes
/// without one. Named here so it is a known bound and not a surprise.
///
/// The alternative — fail closed, quarantine the shared copy, and let the
/// operator sort it out — was rejected as the larger change for a strictly worse
/// outcome: it needs a table to quarantine into, a reader for it, and something
/// to tell the operator; and it still leaves `all_sessions` returning a
/// duplicate until a human acts. Merging repairs the invariant at the one moment
/// the new binary is in a position to act on it.
fn move_codex_sessions(tx: &Connection) -> Result<CodexSessionMove> {
    // First, while both copies still exist. Freshest-wins over the family
    // v0.6.0 can advance; the identity columns are absent from the `SET` list
    // and therefore cannot regress.
    let reconciled = tx.execute(
        "UPDATE codex_sessions AS c
            SET session_id        = CASE WHEN s.updated_at > c.updated_at
                                         THEN s.session_id   ELSE c.session_id   END,
                tmux_session      = CASE WHEN s.updated_at > c.updated_at
                                         THEN s.tmux_session ELSE c.tmux_session END,
                tmux_socket       = CASE WHEN s.updated_at > c.updated_at
                                         THEN s.tmux_socket  ELSE c.tmux_socket  END,
                cwd               = CASE WHEN s.updated_at > c.updated_at
                                         THEN s.cwd          ELSE c.cwd          END,
                lifecycle         = CASE WHEN s.updated_at >= c.updated_at
                                         THEN s.lifecycle    ELSE c.lifecycle    END,
                claude_session_id = CASE WHEN s.updated_at > c.updated_at
                                         THEN COALESCE(s.claude_session_id, c.claude_session_id)
                                         ELSE COALESCE(c.claude_session_id, s.claude_session_id)
                                    END,
                transcript_path   = CASE WHEN s.updated_at > c.updated_at
                                         THEN COALESCE(s.transcript_path, c.transcript_path)
                                         ELSE COALESCE(c.transcript_path, s.transcript_path)
                                    END,
                updated_at        = MAX(s.updated_at, c.updated_at)
           FROM sessions AS s
          WHERE s.session_uid = c.session_uid",
        [],
    )?;
    // Then the ordinary arm: a misfiled row with nothing at its uid in the
    // destination. `NOT EXISTS` rather than `INSERT OR IGNORE`, so the rows the
    // reconcile just handled are excluded by the statement instead of being
    // swallowed by a conflict clause that would look identical if the reconcile
    // had never run.
    let moved = tx.execute(
        &format!(
            "INSERT INTO codex_sessions(session_uid, session_id, tmux_session, tmux_socket,
                                        cwd, claude_session_id, transcript_path, lifecycle,
                                        created_at, updated_at, agent, codex_thread_id,
                                        codex_socket, codex_generation)
             SELECT s.session_uid, s.session_id, s.tmux_session, s.tmux_socket, s.cwd,
                    s.claude_session_id, s.transcript_path, s.lifecycle, s.created_at,
                    s.updated_at, s.agent, s.codex_thread_id, s.codex_socket,
                    s.codex_generation
               FROM sessions AS s
              WHERE ({MISFILED})
                AND NOT EXISTS(SELECT 1 FROM codex_sessions AS c
                                WHERE c.session_uid = s.session_uid)"
        ),
        [],
    )?;
    // And the same predicate one last time. Nothing is lost by it: every row it
    // matches is either one the reconcile merged — the `UPDATE`'s own `WHERE`
    // is the second disjunct of [`MISFILED`], so the two sets are the same set
    // — or one the `INSERT` above copied whole. Sharing the predicate is what
    // makes that true by construction rather than by an assertion that could
    // never fire.
    tx.execute(&format!("DELETE FROM sessions AS s WHERE {MISFILED}"), [])?;
    if reconciled > 0 {
        crate::log_error!(
            "schema: {reconciled} Codex run(s) had been re-filed into the shared sessions table \
             under the default agent, which only a rolled-back v0.6 daemon can do; the two copies \
             were merged — identity from the isolated row, freshest-wins on everything the old \
             daemon can advance — and the shared copy removed"
        );
    }
    Ok(CodexSessionMove { moved, reconciled })
}

/// A card in the shared table whose run is a Codex run.
///
/// Written against the alias `p`, and named once so the predicate in
/// [`needs_codex_card_move`] and the statement in
/// [`move_codex_pending_approvals`] cannot answer it differently.
///
/// **Asked of the data, never of `user_version`** — see
/// [`needs_codex_session_move`] for why a rollback makes the number the wrong
/// question.
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
/// is a delete and not a migration worth more code: the three 2e-7a producers
/// refuse a non-Claude run before any durable write, and the observer that
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
    // legacy row is backfilled to 'claude' by SQLite itself; the Codex identity
    // columns are nullable. A rolled-back build simply never names these columns
    // in its positional SELECTs, so they are invisible to it.
    ("sessions", "agent", "TEXT NOT NULL DEFAULT 'claude'"),
    ("sessions", "codex_thread_id", "TEXT"),
    ("sessions", "codex_socket", "TEXT"),
    // The durable Codex generation high-water (plan A5.1). **Both** tables are
    // named, unlike the three entries above, and the asymmetry is not an
    // oversight: those predate `codex_sessions`, so on every database that could
    // be missing them the Codex table is created fresh at the current shape and
    // `missing_columns` skips it. This column arrives *after* `codex_sessions`
    // shipped, so a database written by the build before this one has the table
    // at thirteen columns and needs the `ALTER` too. Naming only `sessions`
    // would widen one half, leave the other, and fail
    // `both_session_tables_have_one_shape` — which is exactly the drift that
    // test exists to catch, and exactly what would break the `INSERT … SELECT *`
    // carry in `upsert_session` at runtime.
    //
    // Appended last on both, which is where `ALTER TABLE … ADD COLUMN` puts it
    // and where the `CREATE TABLE`s above declare it, so the two orders agree
    // on a fresh database and on an upgraded one alike.
    ("sessions", "codex_generation", "INTEGER"),
    ("codex_sessions", "codex_generation", "INTEGER"),
    // Per-device feature set and the daemon-version epoch it was last confirmed
    // under. Both nullable, and in this phase both are `NULL` for every row:
    // **nothing writes them.** No shipping client can advertise a feature set —
    // the phone encodes no such field — so the write side was deleted rather than
    // carried, and an inbound `features` is ignored. `NULL` is the Claude-only
    // floor, which is exactly what every device is entitled to today.
    //
    // The columns stay because the READ side is live and fail-closed: a set
    // stamped with another run's epoch, or bytes that will not decode, is
    // [`DeviceFeatures::Unconfirmable`] and authorizes nothing. That is what makes
    // a Codex doorbell reach no device at all until a phone genuinely advertises
    // one, and it is why Phase 5 lands a write here rather than a migration.
    ("devices", "features", "TEXT"),
    ("devices", "features_epoch", "TEXT"),
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

/// Widen the tables that predate a column.
///
/// `ALTER TABLE … ADD COLUMN` has no `IF NOT EXISTS`, so a second attempt at a
/// column that is already there fails with a duplicate-column error — which,
/// raised from here, is a daemon that will not open its own database.
///
/// Takes the caller's connection rather than opening a transaction of its own,
/// because [`Store::migrate`] now runs this inside the one `BEGIN IMMEDIATE`
/// that also creates `codex_sessions` and moves the Codex rows into it. The
/// write lock that used to be taken here is taken there, earlier, and held
/// across all three.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    // Asked again under the write lock, for the reason spelled out in
    // `migrate_to_session_uids`: two daemons starting at once can both pass the
    // unlocked check, and the one that arrives second must find nothing left to
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
/// The `WHERE EXISTS` guard is `upsert_pending_approval`'s, for the same
/// reason: an approval for a run deleted mid-flight must not leave an orphan
/// row that recovery later raises for a session nobody can name.
///
/// # A re-delivery is compared, not merged
///
/// This used to be an `ON CONFLICT … DO UPDATE SET card, turn_id`, and that
/// silently split the card in half. The row and the in-memory card would take
/// the new content while the already-filed `ApprovalRequest` — and every
/// connected client holding it — kept the old one, with no event and no ring to
/// say the question had changed. A phone would then be showing, and Phase 3b
/// answering, a question the app-server had replaced.
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
        // Positions 10..13 are the agent-seam columns, always selected last so an
        // older build's 10-column positional SELECT never touches them. A NULL
        // `agent` is true absence — a value neither the CREATE default nor a
        // backfill produced — and decodes as Claude; a *present* value goes
        // through `from_str_lossy`, so a present unrecognised string fails closed
        // rather than being read as Claude.
        agent: match row.get::<_, Option<String>>(10)? {
            None => AgentKind::Claude,
            Some(value) => AgentKind::from_str_lossy(&value),
        },
        codex_thread_id: row.get(11)?,
        codex_socket: row.get(12)?,
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
        "daemon" => Source::Daemon,
        "codex" => Source::Codex,
        "pty" => Source::Pty,
        // An unrecognised persisted source — a fact written by a newer daemon,
        // read back after a rollback — is the lowest trust there is, never the
        // trusted `Daemon` it used to become. A source it does not understand
        // must not out-rank a real hook or transcript fact in dedup.
        _ => Source::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ws::{AnswerDecision, AnswerPath, ResolvedBy};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// The epoch these tests write feature sets under and read them back with.
    ///
    /// A fixed string rather than `crate::state::feature_epoch()`: the tests that
    /// care about the epoch are about the *mismatch*, and a value that changes per
    /// process cannot be written into a fixture and then disagreed with on purpose.
    const TEST_FEATURE_EPOCH: &str = "test-epoch";

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

    /// **The generalized ledger's conflict law** (Phase-1 gate): a retry under
    /// the same `(operation_kind, session_uid, client_request_id)` with *changed*
    /// claimed material is a conflict, never a second actuation; an unsettled
    /// replay is indeterminate; a settled one replays its outcome; the operation
    /// kind is part of the key.
    #[test]
    fn the_mutation_ledger_conflicts_on_changed_material_and_never_actuates_twice() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
        let now = protocol::time::now_rfc3339();

        // The immutable material a compose was claimed with: it was a `turn_start`
        // against thread T at generation 3, targeting no existing turn.
        let material = |hash: &str| ClaimedMaterial {
            thread_id: "th_T".into(),
            generation: 3,
            route: "turn_start".into(),
            target_turn_id: None,
            claimed_hash: hash.into(),
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
    /// The ledger Phase 1 built says it covers "answer, compose, interrupt", and
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
    /// about the answer. (A25 forbids that second table, and
    /// `a_fresh_database_is_created_at_the_current_schema` asserts it does not exist.)
    #[test]
    fn an_answer_is_claimed_and_settled_in_the_one_generalized_ledger() {
        let (store, _p) = temp_store();
        let session = key("AA", "cc-1");
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
        let now = protocol::time::now_rfc3339();
        insert_codex_card(&store, &session, "derived-1", "item-1", "turn-1", 10, "{}").unwrap();

        let material = |hash: &str| ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: hash.into(),
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
        let now = protocol::time::now_rfc3339();
        insert_codex_card(&store, &session, "derived-1", "item-1", "turn-1", 10, "{}").unwrap();
        let material = ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: "accept".into(),
            target_turn_id: Some("turn-1".into()),
            claimed_hash: "hashA".into(),
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
        let now = protocol::time::now_rfc3339();
        let material = |route: &str| ClaimedMaterial {
            thread_id: "th-1".into(),
            generation: 1,
            route: route.into(),
            target_turn_id: None,
            claimed_hash: "hashA".into(),
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

    /// **What the column holds, and what each shape of it authorizes.**
    ///
    /// The read side is what is under test, and it is the only side that ships in
    /// this phase: nothing writes this column, so a live row is always `NULL` —
    /// the Claude floor, and the row every phone predating the field has.
    /// [`Store::set_device_features`] is here as the fixture writer, because the
    /// other three shapes have to exist for the decode to be asked about at all.
    ///
    /// The epoch is spent on the read: only a set stamped with the asking run's own
    /// epoch is the device's word, and the two ways of not being one — another
    /// run's stamp, and a `features_epoch IS NULL` row — are the same answer, which
    /// is not the floor.
    ///
    /// **Mutation:** decode a mismatched or null-epoch set as
    /// `DeviceFeatures::Advertised(Default::default())` (what the old chain's
    /// `unwrap_or_default` did) and the two `Unconfirmable` assertions fail.
    #[test]
    fn a_stored_feature_set_is_only_this_runs_word_if_this_run_stamped_it() {
        let (store, _p) = temp_store();
        let now = protocol::time::now_rfc3339();
        store
            .insert_device("dev-1", "iPhone", "tok-hash", &now)
            .unwrap();
        store
            .set_push_token("dev-1", "token", "sandbox", None)
            .unwrap();

        let read_features = |store: &Store| -> (Option<String>, Option<String>) {
            store
                .read()
                .query_row(
                    "SELECT features, features_epoch FROM devices WHERE device_id = ?1",
                    params!["dev-1"],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        let decoded =
            |store: &Store, epoch: &str| store.push_targets(epoch).unwrap()[0].features.clone();

        // A device that has never advertised: the column is empty, and empty is
        // the floor rather than a refusal.
        assert_eq!(read_features(&store), (None, None));
        assert_eq!(decoded(&store, "epoch-A"), DeviceFeatures::default());

        // Persist a Codex feature set under epoch A, and prove it was written.
        store
            .set_device_features("dev-1", Some(r#"{"agents":["claude","codex"]}"#), "epoch-A")
            .unwrap();
        assert_eq!(
            read_features(&store).0.as_deref(),
            Some(r#"{"agents":["claude","codex"]}"#)
        );
        assert_eq!(read_features(&store).1.as_deref(), Some("epoch-A"));
        assert!(decoded(&store, "epoch-A").supports(&AgentKind::Codex));

        // **The same bytes, asked about by a different run.** Nothing was swept
        // and nothing needs to be: the stamp is what the answer turns on.
        assert_eq!(decoded(&store, "epoch-B"), DeviceFeatures::Unconfirmable);

        // **Absence clears.** `None` writes the empty row rather than leaving what
        // is there — and *that* row is the floor, because a device that names no
        // features is exactly a device that can render Claude and nothing else.
        store.set_device_features("dev-1", None, "epoch-A").unwrap();
        assert_eq!(read_features(&store), (None, None));
        assert_eq!(decoded(&store, "epoch-A"), DeviceFeatures::default());

        // **The half-written row**: `features` present, `features_epoch` NULL. It
        // is not this run's word either, and it is not the floor.
        store
            .write()
            .execute(
                "UPDATE devices SET features = '{\"agents\":[\"codex\"]}', features_epoch = NULL
                  WHERE device_id = 'dev-1'",
                [],
            )
            .unwrap();
        assert_eq!(decoded(&store, "epoch-A"), DeviceFeatures::Unconfirmable);
    }

    /// **The read is the whole defense, and nothing has to have run first.**
    ///
    /// The same bytes, asked about by the run that wrote them and by any other
    /// run: confirmation to the first, and to the second a set it cannot vouch for,
    /// which authorizes nothing at all. No sweep, no flag, no startup step — and
    /// deliberately so, because every one of those is a defense that can fail to
    /// run, while this one is the read that authorizes the push.
    ///
    /// This is also why nothing latches a process-wide "trust no device" flag
    /// anywhere: it could only repeat what this assertion already shows, and —
    /// being process-wide — nothing a phone did afterwards could clear it.
    #[test]
    fn a_feature_set_from_another_run_never_authorizes_this_one() {
        let (store, _p) = temp_store();
        let now = protocol::time::now_rfc3339();
        store
            .insert_device("dev-1", "iPhone", "tok-hash", &now)
            .unwrap();
        store
            .set_push_token("dev-1", "token", "sandbox", None)
            .unwrap();
        store
            .set_device_features("dev-1", Some(r#"{"agents":["codex"]}"#), "epoch-OLD")
            .unwrap();

        assert!(
            store.push_targets("epoch-OLD").unwrap()[0]
                .features
                .supports(&AgentKind::Codex),
            "the run that confirmed it reads it back as confirmation"
        );
        assert_eq!(
            store.push_targets("epoch-NEW").unwrap()[0].features,
            DeviceFeatures::Unconfirmable,
            "and to any other run the same bytes are a claim it cannot read — not \
             the floor, which would hand this phone Claude doorbells it said it \
             cannot render"
        );
    }

    /// **Additive-column compatibility (new → old → new).**
    ///
    /// The invariant this asserts for Phase 1: the agent-seam columns are purely
    /// additive, so a pre-seam positional reader/writer works unchanged and no
    /// data is lost across a reopen. **Only Claude rows exist**, because the
    /// daemon now fails closed on any non-Claude registration (see
    /// `a_registration_for_an_unsupported_agent_is_refused_with_no_trace`) — a
    /// Codex `sessions` row cannot be written in this phase at all.
    ///
    /// > **HARD PHASE-2 GATE — must land before the `codex` command is exposed.**
    /// > The moment a real Codex writer exists, Codex durable state (sessions,
    /// > pending, claims, mutations, cursors) must move into agent-scoped storage
    /// > a legacy daemon never enumerates or mutates. A Codex row in the *shared*
    /// > `sessions` table would be enumerated by a rolled-back old daemon (its
    /// > positional `SELECT` returns every row, agent-agnostically). That is
    /// > acceptable ONLY while no such row can exist. This test deliberately does
    /// > **not** place a Codex row in the shared table, because in a correct build
    /// > one cannot be there.
    ///
    /// The real old-binary run — which drives the ACTUAL v0.6.0 `ccd` through a
    /// new → old → new rollback — is the harness at
    /// `mac/ccd/tests/new-old-new-real.sh` (a shell script, not a `cargo test`,
    /// because it builds a historical binary). This in-process test proves the
    /// additive-column contract deterministically for CI; that harness proves it
    /// against the real old binary.
    #[test]
    fn additive_columns_are_transparent_to_a_legacy_reader_and_lose_no_data() {
        let (_p, path) = {
            let (store, path) = temp_store();
            // NEW daemon writes only Claude rows (the only kind Phase 1 permits)
            // plus a device with a feature set.
            store
                .upsert_session(&session_row(&key("AA", "cc-1")))
                .unwrap()
                .assert_present();
            let now = protocol::time::now_rfc3339();
            store.insert_device("dev-1", "iPhone", "tok", &now).unwrap();
            store
                .set_device_features(
                    "dev-1",
                    Some(r#"{"agents":["claude","codex"]}"#),
                    "epoch-NEW",
                )
                .unwrap();
            drop(store);
            ((), path)
        };

        // OLD daemon: the observable contract of a pre-seam binary.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            // (a) It reads sessions with the exact 10 legacy columns, never the
            // agent-seam ones — the additive columns are invisible to it.
            let mut stmt = conn
                .prepare(
                    "SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                            claude_session_id, transcript_path, lifecycle, created_at, updated_at
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

            // (b) It writes a session with only the 10 legacy columns; SQLite
            // applies the 'claude' default, so a legacy writer's row is a Claude
            // row exactly as a pre-seam one would have been.
            conn.execute(
                "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                      claude_session_id, transcript_path, lifecycle,
                                      created_at, updated_at)
                 VALUES(?1,'cc-2','cc-2','codeconnect','/tmp',NULL,NULL,'live','t','t')",
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

        // **The device's set survives the reopen byte-for-byte**, which is the
        // additive-column contract this test is about — a legacy binary neither
        // reads nor rewrites the column. What a *fresh* run may do with those
        // bytes is the read's business and not a rollback's: they carry
        // `epoch-NEW`, so any other run finds them
        // [`DeviceFeatures::Unconfirmable`] and rings that phone about nothing.
        // Pinned by `a_stored_feature_set_is_only_this_runs_word_if_this_run_stamped_it`.
        let features: Option<String> = store
            .read()
            .query_row(
                "SELECT features FROM devices WHERE device_id = 'dev-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            features.as_deref(),
            Some(r#"{"agents":["claude","codex"]}"#),
            "a legacy round trip leaves the column exactly as it found it"
        );
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
        store
            .upsert_session(&session_row(&session))
            .unwrap()
            .assert_present();
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
            tmux_session: session.name.clone(),
            tmux_socket: "codeconnect".into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: AgentKind::Claude,
            codex_thread_id: None,
            codex_socket: None,
        }
    }

    /// **The link's thread survives a daemon restart, and a relaunch clears it.**
    ///
    /// The row is the only answer that outlives the process, so a re-registration
    /// after a restart — which carries no thread, because a registration happens
    /// before the app-server announces one — must not blank what the link recorded.
    /// A relaunch must, because its generation is a different visit and its thread is
    /// one nobody has adopted yet.
    #[test]
    fn a_link_recorded_thread_survives_a_restart_and_not_a_relaunch() {
        let (store, _path) = temp_store();
        let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-1");
        let mut row = session_row(&session);
        row.agent = AgentKind::Codex;
        store
            .upsert_session_at_generation(&row, Some(7))
            .unwrap()
            .assert_present();
        let thread = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86";

        assert!(
            store.bind_codex_thread(&session.uid, 7, thread).unwrap(),
            "the link's first write is a change"
        );
        assert!(
            !store.bind_codex_thread(&session.uid, 7, thread).unwrap(),
            "the same thread again owes nothing"
        );
        assert!(
            !store
                .bind_codex_thread(&session.uid, 6, "somebody-elses")
                .unwrap(),
            "a superseded link speaks for no generation but its own"
        );
        assert_eq!(
            store
                .get_session(&session.uid)
                .unwrap()
                .unwrap()
                .codex_thread_id,
            Some(thread.to_string())
        );

        // **The restart, with the store actually closed and reopened.** The claim is
        // that the link's write survives the daemon dying, and a test that kept one
        // open handle proved only that the row survived a second statement. Dropping
        // the `Store` closes its connections; reopening reads the same file back off
        // the disk the durable answer is supposed to be on.
        drop(store);
        let store = Store::open(&_path).unwrap();
        assert_eq!(
            store
                .get_session(&session.uid)
                .unwrap()
                .unwrap()
                .codex_thread_id,
            Some(thread.to_string()),
            "the thread the link recorded must still be there after the file is \
             reopened, or it was never durable"
        );

        // Then the supervisor re-registers at the same generation, naming no thread,
        // exactly as the producer does.
        store
            .upsert_session_at_generation(&row, Some(7))
            .unwrap()
            .assert_present();
        assert_eq!(
            store
                .get_session(&session.uid)
                .unwrap()
                .unwrap()
                .codex_thread_id,
            Some(thread.to_string()),
            "a restart re-registers with no thread and must keep the one the link adopted"
        );

        // The relaunch: a later generation is a different visit.
        store
            .upsert_session_at_generation(&row, Some(8))
            .unwrap()
            .assert_present();
        assert_eq!(
            store
                .get_session(&session.uid)
                .unwrap()
                .unwrap()
                .codex_thread_id,
            None,
            "a new visit starts on no adopted thread"
        );
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

    // ------------------------------------------- agent-scoped session storage

    /// A Codex run, in the shape a registration will write once one exists.
    fn codex_session_row(session: &SessionKey) -> SessionRow {
        let mut row = session_row(session);
        row.agent = AgentKind::Codex;
        row.codex_thread_id = Some("th_ABC123".into());
        row.codex_socket = Some("/tmp/cch.test/ccd.sock".into());
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

    /// **The durable Codex generation high-water, at the layer that stores it**
    /// (plan A5.1, clause "durable high-water evidence in rollback-isolated
    /// storage").
    ///
    /// Four claims, and each of them is a different way the evidence could stop
    /// being evidence:
    ///
    ///   * it is **written by the same statement as the row**, so there is no
    ///     window in which a row exists at a generation nothing recorded;
    ///   * it lands in `codex_sessions` and **not** in `sessions`, which is what
    ///     "rollback-isolated" means here — asked of SQLite directly rather than
    ///     through `get_session`, which reads both tables and so cannot tell a
    ///     correctly filed value from a misfiled one;
    ///   * an ordinary [`Store::upsert_session`] — every hook, heartbeat and
    ///     tailer write in the daemon — **cannot blank it**, because it passes
    ///     `None` into the `COALESCE`. A blanked high-water is a session a stale
    ///     supervisor could re-adopt at any generation it liked;
    ///   * it **survives the cross-table carry**, which is the one path that
    ///     moves a row wholesale (`INSERT … SELECT *`) and would silently drop a
    ///     column the two tables disagreed about.
    ///
    /// **Mutation:** drop `codex_generation` from the `ON CONFLICT DO UPDATE`
    /// list and the second leg reads back `None` — a re-registration would then
    /// erase the high-water it was supposed to advance.
    #[test]
    fn the_codex_generation_is_written_with_the_row_and_never_blanked_by_a_later_one() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        store
            .upsert_session_at_generation(&codex_session_row(&codex), Some(7))
            .unwrap()
            .assert_present();
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(7),
            "the generation the registration was accepted at must be readable back"
        );

        // Where it is, asked of the tables and not of the fleet view.
        let conn = store.read();
        let stored = |table: &str| -> Option<i64> {
            conn.query_row(
                &format!("SELECT codex_generation FROM {table} WHERE session_uid = ?1"),
                params![codex.uid],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
        };
        assert_eq!(
            stored("codex_sessions"),
            Some(7),
            "the high-water lives in the table a rolled-back v0.6.0 daemon cannot name"
        );
        assert_eq!(
            stored("sessions"),
            None,
            "and never in the one it rewrites and prunes globally"
        );
        drop(conn);

        // **A later registration advances it**, which is the `ON CONFLICT` arm
        // and not the insert arm above. Asserted separately because the two arms
        // are different SQL: a statement that recorded the generation on insert
        // and dropped it on conflict would satisfy every other leg here, and
        // would then freeze the high-water at whatever the first registration
        // said for the rest of the session's life.
        store
            .upsert_session_at_generation(&codex_session_row(&codex), Some(9))
            .unwrap()
            .assert_present();
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(9),
            "a re-registration at a later visit must move the high-water with it"
        );

        // A later write that knows nothing about visits must not erase it.
        let mut heartbeat = codex_session_row(&codex);
        heartbeat.cwd = "/elsewhere".into();
        store.upsert_session(&heartbeat).unwrap().assert_present();
        assert_eq!(
            store.get_session(&codex.uid).unwrap().unwrap().cwd,
            "/elsewhere",
            "the premise: the later write really did land"
        );
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(9),
            "a writer with no news about generations must leave the high-water alone"
        );

        // And it survives the carry between tables, which moves a row wholesale.
        let mut as_claude = session_row(&codex);
        as_claude.agent = AgentKind::Claude;
        store.upsert_session(&as_claude).unwrap().assert_present();
        assert_eq!(
            table_counts(&store),
            (1, 0),
            "the premise: the row really did move to the shared table"
        );
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(9),
            "the carry is INSERT … SELECT *, so a column the two tables disagreed \
             about would have been dropped on the way across"
        );
    }

    /// **The thread on a row was named by a registration at that row's exact
    /// generation** (round-C F2) — the invariant the adoption guard states and
    /// `COALESCE` alone did not provide.
    ///
    /// Under the old statement a generation could advance while the previous
    /// visit's thread stayed put, and the pair left behind — `(G2, A)` for a
    /// registration at G2 that named no thread — is read by
    /// `Daemon::register_supervisor` as G2's *binding*. This asserts the three
    /// shapes that pair can be reached through and the one the ordinary reconnect
    /// still relies on:
    ///
    ///   * a generation that ADVANCES writes the thread it carries, absence
    ///     included, so an inherited thread cannot pose as this visit's;
    ///   * the same is true when the row predates the column entirely (a thread,
    ///     no generation), which is the pre-A5.1 misattribution;
    ///   * a write at or BELOW the standing generation still `COALESCE`s, which is
    ///     the reconnect that names no thread and must change nothing;
    ///   * and a writer with no generation at all — every hook, heartbeat and
    ///     tailer in the daemon — is on the `COALESCE` arm regardless.
    ///
    /// **Mutation:** replace the `CASE` with the bare
    /// `COALESCE(excluded.codex_thread_id, {table}.codex_thread_id)` and the first
    /// two legs read back `th-first`, which is a thread generation 2 never named.
    #[test]
    fn a_generation_that_advances_does_not_inherit_the_previous_visits_thread() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let at = |thread: Option<&str>, generation: Option<u64>| {
            let mut row = codex_session_row(&codex);
            row.codex_thread_id = thread.map(str::to_string);
            store
                .upsert_session_at_generation(&row, generation)
                .unwrap()
                .assert_present();
            store
                .get_session(&codex.uid)
                .unwrap()
                .unwrap()
                .codex_thread_id
        };

        assert_eq!(at(Some("th-first"), Some(1)), Some("th-first".into()));
        assert_eq!(
            at(None, Some(2)),
            None,
            "generation 2 named no thread, so the row must not claim generation 1's"
        );
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(2),
            "and the high-water still advanced — the thread is what does not carry, \
             not the visit count"
        );
        assert_eq!(
            at(Some("th-second"), Some(2)),
            Some("th-second".into()),
            "so generation 2's own first thread binds"
        );
        assert_eq!(
            at(None, Some(2)),
            Some("th-second".into()),
            "and a write at the standing generation with no thread COALESCEs, which \
             is the ordinary reconnect"
        );
        assert_eq!(
            at(Some("th-heartbeat"), None),
            Some("th-heartbeat".into()),
            "the premise for the next leg: a generation-less writer takes the \
             COALESCE arm and its thread lands"
        );
        assert_eq!(
            at(None, None),
            Some("th-heartbeat".into()),
            "and cannot blank one, which is the rule every hook and heartbeat \
             has always had"
        );

        // The pre-A5.1 row: a thread and no generation at all. The first
        // generation ever recorded for it must not adopt that thread as its own.
        let legacy = key("CY", "cx-2");
        let mut row = codex_session_row(&legacy);
        row.codex_thread_id = Some("th-from-the-seam".into());
        store.upsert_session(&row).unwrap().assert_present();
        assert_eq!(store.codex_generation(&legacy.uid).unwrap(), None);
        row.codex_thread_id = None;
        store
            .upsert_session_at_generation(&row, Some(9))
            .unwrap()
            .assert_present();
        assert_eq!(
            store
                .get_session(&legacy.uid)
                .unwrap()
                .unwrap()
                .codex_thread_id,
            None,
            "a NULL generation compares below every real one, so the seam-era \
             thread is not misattributed to generation 9"
        );
    }

    /// **A generation SQLite cannot represent is refused, never rounded**
    /// (round-C F3).
    ///
    /// The column is `INTEGER`, which is `i64`; the caller's type is `u64`. This
    /// used to saturate to `i64::MAX`, so the value read back was a different
    /// number from the one written — and the adoption guard compares the two for
    /// equality. Writing a number nobody asked for is the defect; that no honest
    /// producer mints one is the reason it is refused rather than stored wider.
    ///
    /// The largest representable value is asserted too, so the refusal is a
    /// boundary and not a range.
    ///
    /// **Mutation:** restore `.unwrap_or(i64::MAX)` and the second leg writes,
    /// reading back `Some(9223372036854775807)` for a registration that claimed
    /// 18446744073709551615.
    #[test]
    fn a_codex_generation_sqlite_cannot_represent_is_refused_not_saturated() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        store
            .upsert_session_at_generation(&codex_session_row(&codex), Some(i64::MAX as u64))
            .unwrap()
            .assert_present();
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(i64::MAX as u64),
            "the largest representable generation round-trips"
        );

        let refused = store
            .upsert_session_at_generation(&codex_session_row(&codex), Some(u64::MAX))
            .expect_err("a generation the column cannot hold must not be written");
        assert!(
            format!("{refused:#}").contains(&codex.uid),
            "the refusal names the run it refused: {refused:#}"
        );
        assert_eq!(
            store.codex_generation(&codex.uid).unwrap(),
            Some(i64::MAX as u64),
            "and left the high-water exactly where it was"
        );
    }

    /// A uid nothing has registered has no high-water, and neither does a Claude
    /// run — the two cases the adoption guard must read as "nothing has been
    /// adopted here", not as generation zero.
    #[test]
    fn an_unregistered_uid_and_a_claude_run_have_no_codex_generation() {
        let (store, _path) = temp_store();
        let claude = key("AA", "cc-1");
        assert_eq!(
            store.codex_generation(&claude.uid).unwrap(),
            None,
            "a uid with no row has no high-water"
        );
        store
            .upsert_session(&session_row(&claude))
            .unwrap()
            .assert_present();
        assert_eq!(
            store.codex_generation(&claude.uid).unwrap(),
            None,
            "and a Claude run, whose sessions have no visits, has none either"
        );
    }

    /// **The whole point of the split.** A Codex run is written to
    /// `codex_sessions` and is not in `sessions` at all, while every read this
    /// daemon performs still finds it.
    #[test]
    fn a_codex_run_is_written_to_its_own_table_and_never_to_the_shared_one() {
        let (store, _path) = temp_store();
        let claude = key("AA", "cc-1");
        let codex = key("CX", "cx-1");
        store
            .upsert_session(&session_row(&claude))
            .unwrap()
            .assert_present();
        store
            .upsert_session(&codex_session_row(&codex))
            .unwrap()
            .assert_present();

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
            "resolving by tmux name reaches the Codex half too"
        );
    }

    /// The isolation itself, stated as the old daemon's own SQL.
    ///
    /// v0.6.0 cannot be imported, so its statements are reproduced here verbatim
    /// — the agent-agnostic positional listing its liveness sweep enumerates
    /// from, the `UPDATE` that sweep issues when tmux says a session is gone,
    /// and the `DELETE` its prune runs. All three name `sessions`, because that
    /// is the only session table that build has ever heard of. Every one of them
    /// must come away with nothing.
    ///
    /// Measured against the real binary before this change, with the Codex row
    /// in the shared table: the sweep marked a live Codex session `exited` and
    /// the prune deleted the row and all its events.
    #[test]
    fn a_rolled_back_daemons_own_statements_cannot_reach_a_codex_run() {
        let (store, _path) = temp_store();
        let claude = key("AA", "cc-1");
        let codex = key("CX", "cx-1");
        seed_run(&store, &claude, Lifecycle::Live, 2);
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        store.upsert_session(&row).unwrap().assert_present();
        for i in 0..2 {
            store
                .append_event(&pending(
                    &codex,
                    EventKind::ToolCall,
                    Some(&format!("cx-{i}")),
                ))
                .unwrap();
        }

        let conn = store.write();

        // 1. Enumeration — v0.6.0's `list_sessions`, ten columns by position.
        let enumerated: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT session_uid, session_id, tmux_session, tmux_socket, cwd,
                            claude_session_id, transcript_path, lifecycle, created_at, updated_at
                       FROM sessions ORDER BY created_at ASC, session_uid ASC",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows
        };
        assert_eq!(
            enumerated,
            vec![claude.uid.clone()],
            "the old daemon's listing sees Claude's run and nothing else"
        );

        // 2. Mutation — the liveness sweep's write, applied to every uid it
        //    could possibly have reached.
        let marked = conn
            .execute(
                "UPDATE sessions SET lifecycle = 'exited', updated_at = 'then'",
                [],
            )
            .unwrap();
        assert_eq!(marked, 1, "only Claude's run was reachable to be marked");

        // 3. Destruction — the prune, over every ended run the old build knows.
        let pruned = conn
            .execute("DELETE FROM sessions WHERE lifecycle = 'exited'", [])
            .unwrap();
        assert_eq!(pruned, 1, "the prune took Claude's run and only Claude's");
        drop(conn);

        // The Codex run and its whole log are untouched.
        let survivor = store.get_session(&codex.uid).unwrap().expect("codex row");
        assert_eq!(survivor.lifecycle, Lifecycle::Live);
        assert_eq!(survivor.updated_at, row.updated_at);
        assert_eq!(store.count_events(&codex.uid).unwrap(), 2);
    }

    /// The move is defined over the rows, not over `user_version`.
    ///
    /// Seeded the way a rollback leaves it: a Codex row sitting in the shared
    /// table, written there by a build that had no other place to put it. The
    /// first open moves it. The second finds nothing to do. And a third, with
    /// `user_version` forced back to 3 the way a v0.6.0 daemon leaves it, still
    /// finds nothing to do — which is the property a version gate could not
    /// have, because the version it would gate on is not ours to keep.
    #[test]
    fn the_codex_move_is_driven_by_the_rows_and_survives_a_reset_version() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let claude = key("AA", "cc-1");
        seed_run(&store, &claude, Lifecycle::Live, 1);
        drop(store);

        // A v0.6-shaped write: straight into `sessions`, agent and all.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                      claude_session_id, transcript_path, lifecycle,
                                      created_at, updated_at, agent, codex_thread_id, codex_socket)
                 VALUES(?1, 'cx-1', 'cx-1', 'codeconnect', '/work', NULL, NULL, 'live',
                        't', 't', 'codex', 'th_ABC123', '/tmp/s.sock')",
                params![codex.uid],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(
            table_counts(&store),
            (1, 1),
            "the move put the Codex run where the old daemon cannot reach it"
        );
        let moved = store.get_session(&codex.uid).unwrap().expect("moved row");
        assert_eq!(moved.agent, AgentKind::Codex);
        assert_eq!(moved.codex_thread_id.as_deref(), Some("th_ABC123"));
        assert_eq!(
            moved.cwd, "/work",
            "every column came across, not just the key"
        );
        drop(store);

        // Idempotent: a second open has nothing to move and changes nothing.
        let store = Store::open(&path).unwrap();
        assert_eq!(table_counts(&store), (1, 1));
        drop(store);

        // And the rollback's parting gift — `user_version` back at 3 — does not
        // make it re-run, because it never asked.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 3i64).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(table_counts(&store), (1, 1));
        assert!(store.get_session(&codex.uid).unwrap().is_some());
        assert!(store.get_session(&claude.uid).unwrap().is_some());
        assert_eq!(store.count_events(&claude.uid).unwrap(), 1);
    }

    /// The ten-column `INSERT … ON CONFLICT DO UPDATE` v0.6.0's `upsert_session`
    /// issues, character for character from `git show 8e5b172:…/store.rs:727`.
    ///
    /// Written out rather than approximated because the whole question is what
    /// *that build's own statement* does to *this* schema: the eleventh column
    /// it does not name is what takes `DEFAULT 'claude'`, and an approximation
    /// that named `agent` would be testing something else entirely.
    const V060_UPSERT: &str = "INSERT INTO sessions(session_uid, session_id, tmux_session,
                                                    tmux_socket, cwd, claude_session_id,
                                                    transcript_path, lifecycle,
                                                    created_at, updated_at)
                               SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
                                WHERE NOT EXISTS (SELECT 1 FROM deleted_sessions
                                                   WHERE session_uid = ?1)
                                  ON CONFLICT(session_uid) DO UPDATE SET
                                     session_id        = excluded.session_id,
                                     tmux_session      = excluded.tmux_session,
                                     tmux_socket       = excluded.tmux_socket,
                                     cwd               = excluded.cwd,
                                     claude_session_id = COALESCE(excluded.claude_session_id,
                                                                  sessions.claude_session_id),
                                     transcript_path   = COALESCE(excluded.transcript_path,
                                                                  sessions.transcript_path),
                                     lifecycle         = excluded.lifecycle,
                                     updated_at        = excluded.updated_at";

    /// Run that statement for one uid, exactly as the old daemon would.
    fn v060_upsert(
        conn: &Connection,
        uid: &str,
        cwd: &str,
        stamp: &str,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            V060_UPSERT,
            params![
                uid,
                "cx-1",
                "cx-1",
                "codeconnect",
                cwd,
                None::<String>,
                None::<String>,
                "live",
                stamp,
                stamp
            ],
        )
    }

    /// Run a seeding block against the schema as it was **before** the shadow
    /// became unwritable.
    ///
    /// `sessions_refuse_codex_shadow` is what stops a second copy of a Codex run
    /// reaching the shared table, which is the point of it — and which means
    /// every test *about* a duplicate has to stage one from before the refusal
    /// existed. That state is real rather than contrived: `sessions` carried the
    /// `agent` column for a release before `codex_sessions` and the trigger did,
    /// so a first upgrade can genuinely find one on disk.
    ///
    /// Taking the trigger away, writing the row, and putting the trigger back is
    /// what that history looks like. The trigger returns through
    /// [`create_schema`] rather than through a copy of its text, so the staged
    /// database ends at the schema this build actually publishes.
    fn from_before_the_refusal<T>(conn: &Connection, seed: impl FnOnce(&Connection) -> T) -> T {
        conn.execute_batch("DROP TRIGGER sessions_refuse_codex_shadow")
            .unwrap();
        let seeded = seed(conn);
        create_schema(conn).unwrap();
        seeded
    }

    /// The common case of [`from_before_the_refusal`]: v0.6.0's own statement.
    fn stage_a_shadow_from_before_the_refusal(
        conn: &Connection,
        uid: &str,
        cwd: &str,
        stamp: &str,
    ) {
        from_before_the_refusal(conn, |conn| {
            v060_upsert(conn, uid, cwd, stamp).unwrap();
        });
    }

    /// Put the database into the shape the *first* upgrade finds: `sessions`
    /// carrying the agent column and some non-Claude rows, and no
    /// `codex_sessions` or `all_sessions` yet.
    ///
    /// Built by opening a store and then taking the two v4 objects away again,
    /// rather than by hand-writing a v3 schema, so it cannot drift from what
    /// `create_schema` actually builds.
    fn wind_back_to_the_shape_before_the_split(path: &std::path::Path, misfiled: u32) {
        let conn = Connection::open(path).unwrap();
        // The trigger goes with them, because it ships with them: it names
        // `codex_sessions` in its `WHEN` clause and is created by the same
        // `create_schema` batch. Leaving it behind would be a shape no build
        // ever wrote, and would refuse the seeding below on a table that is not
        // there to be consulted.
        conn.execute_batch(
            "DROP TRIGGER sessions_refuse_codex_shadow;
             DROP VIEW all_sessions;
             DROP TABLE codex_sessions;",
        )
        .unwrap();
        for i in 0..misfiled {
            conn.execute(
                "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                      claude_session_id, transcript_path, lifecycle,
                                      created_at, updated_at, agent, codex_thread_id,
                                      codex_socket)
                 VALUES(?1, 'cx-1', 'cx-1', 'codeconnect', '/work', NULL, NULL, 'live',
                        't', 't', 'codex', 'th_ABC123', '/tmp/s.sock')",
                params![format!("CXFILL{i:020}")],
            )
            .unwrap();
        }
    }

    /// **The schema and the rows it is about must become true in one commit.**
    ///
    /// Creating `codex_sessions` publishes the claim that Codex runs are out of
    /// the table a rolled-back v0.6.0 daemon sweeps; the move is what makes the
    /// claim true. Committed separately there is a state on disk where the claim
    /// is published and unkept — the new table exists and the Codex rows are
    /// still in `sessions` — and an old daemon starting inside it does exactly
    /// what it did before the split.
    ///
    /// Measured against the old daemon's own two statements, with a database
    /// mid-migration:
    ///
    /// ```text
    /// three autocommitting steps:  enumerated=[AA, CX]  update_rows=2
    /// one BEGIN IMMEDIATE:         enumerated=[AA, CX]  update_rows=None (database is locked)
    ///                              after COMMIT: enumerated=[AA]  update_rows=1
    /// ```
    ///
    /// The observer here is the reading half of that, run continuously for the
    /// whole of `Store::open`. Each sample is one transaction, so under WAL it
    /// reads a snapshot of a single *committed* state and cannot smear two
    /// together: seeing the half state means the half state was committed. A
    /// deferred read transaction rather than the old daemon's `BEGIN IMMEDIATE`,
    /// because a writer sampling in a tight loop would spend the test fighting
    /// the migration for the write lock and prove nothing extra — the question
    /// is what is *visible*, and WAL readers never block.
    ///
    /// The rows are seeded in bulk because the window this hunts for is exactly
    /// as wide as the move is long: it opens when `create_schema` commits and
    /// closes when the move commits, so more rows to move is a wider window and
    /// a test with more power, not less.
    #[test]
    fn the_new_tables_and_the_moved_rows_become_visible_in_the_same_commit() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let (store, path) = temp_store();
        drop(store);
        wind_back_to_the_shape_before_the_split(&path, 20_000);

        let stop = Arc::new(AtomicBool::new(false));
        let (ready, sampling) = std::sync::mpsc::channel();
        let observer = {
            let path = path.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                let (mut samples, mut half_states) = (0u64, 0u64);
                let mut ready = Some(ready);
                while !stop.load(Ordering::Relaxed) {
                    let tx = conn.transaction().unwrap();
                    // `EXISTS`, not `COUNT`, on both halves: the question is
                    // whether the half state is visible at all, and a count over
                    // twenty thousand rows would make each sample a full scan
                    // and the sampling too coarse to see the window it is
                    // hunting for.
                    let published: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM sqlite_master
                                            WHERE name = 'codex_sessions')",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let still_shared: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM sessions WHERE agent <> 'claude')",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    tx.commit().unwrap();
                    samples += 1;
                    if published && still_shared {
                        half_states += 1;
                    }
                    // Only once the first sample has actually been taken, so the
                    // measured window is the migration and not this thread's
                    // startup.
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
                (samples, half_states)
            })
        };
        sampling.recv().unwrap();

        let store = Store::open(&path).unwrap();
        stop.store(true, Ordering::Relaxed);
        let (samples, half_states) = observer.join().unwrap();

        assert_eq!(
            half_states, 0,
            "{half_states} of {samples} samples caught a committed state in which \
             codex_sessions had been published while sessions still held Codex runs — \
             a v0.6.0 daemon starting there sweeps and prunes them"
        );
        assert!(
            samples > 500,
            "the observer only managed {samples} samples, which is too few for its \
             silence to mean anything"
        );
        assert_eq!(table_counts(&store), (0, 20_000));
    }

    /// How many times an old-shaped statement had to wait for this build's write
    /// lock, counted by the statement's own busy handler.
    ///
    /// A `static` because SQLite's busy handler is a bare `fn` pointer with
    /// nothing to capture into — and the witness has to be the *real* statement's
    /// waiting, not a sleep the test guessed at.
    static OLD_WRITE_WAITS: AtomicUsize = AtomicUsize::new(0);

    fn count_a_wait(tries: i32) -> bool {
        OLD_WRITE_WAITS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(1));
        // v0.6.0's own bound: `busy_timeout(5000)`, an instruction to wait for
        // the lock rather than fail on it.
        tries < 5_000
    }

    /// **An old-shaped write never lands a shadow, on any schedule.**
    ///
    /// The observer above proves nobody *sees* a half state. It says nothing
    /// about a writer, and `BEGIN IMMEDIATE` does not neutralise one — it defers
    /// it. v0.6.0's `open_connection` sets `busy_timeout` to 5000ms, so its
    /// ten-column upsert resumes *after* our commit, against the schema we have
    /// just published, and would file the uid we just moved straight back into
    /// `sessions` under `DEFAULT 'claude'`.
    ///
    /// A repair after the fact cannot answer that, and the review that found this
    /// said why: on a database with nothing misfiled there is no evidence a
    /// re-ask is needed at all, and even where there is, no interval has a
    /// happens-before relationship with another process's scheduling. So the
    /// answer is not a repair, it is a refusal — `sessions_refuse_codex_shadow`,
    /// which lives in the schema and therefore fires for the old binary's own
    /// statement. This drives that across **all three schedules**, with the
    /// verbatim v0.6.0 statement on its own connection:
    ///
    ///   * `Before` — the write lands before `Store::open` is called at all.
    ///   * `Deferred` — the write is held until it has *observed* the migration
    ///     holding the write lock, then blocks behind it and resumes after the
    ///     commit. Nothing sleeps to arrange that: the writer probes for
    ///     `SQLITE_BUSY` itself, and [`OLD_WRITE_WAITS`] counts the real
    ///     statement's retries, so the round asserts it was blocked rather than
    ///     assuming a sleep was long enough. That was the masking the review
    ///     found — a 40ms sleep and an assertion that ran before `Store::open`.
    ///   * `After` — on a **clean** database, with nothing misfiled and nothing
    ///     for the migration to move, the write lands strictly after
    ///     `Store::open` has returned. This is the schedule the review named, and
    ///     it is deterministic: no timing argument is involved anywhere in it.
    ///
    /// In every one the write is **refused** rather than repaired, and the
    /// refusal is what is asserted. A shadow count of zero would also be
    /// satisfied by a repair that ran afterwards, which is precisely the claim
    /// this can no longer make.
    #[test]
    fn an_old_shaped_write_racing_the_migration_never_leaves_a_shadow() {
        #[derive(Clone, Copy, Debug)]
        enum Schedule {
            Before,
            Deferred,
            After,
        }

        const ROUNDS: u32 = 4;
        for round in 0..ROUNDS {
            for schedule in [Schedule::Before, Schedule::Deferred, Schedule::After] {
                let (store, path) = temp_store();
                let codex = key("CX", "cx-1");
                let mut row = codex_session_row(&codex);
                row.lifecycle = Lifecycle::Live;
                store.upsert_session(&row).unwrap().assert_present();
                assert_eq!(table_counts(&store), (0, 1));
                store
                    .append_event(&pending(&codex, EventKind::ToolCall, Some("e0")))
                    .unwrap();
                drop(store);

                // Rows for the move to chew through, so the migration's
                // transaction is wide enough for the deferred arm to catch it
                // holding the lock *and* still have work left when the old
                // statement fires. The `After` arm gets none on purpose: a
                // database with nothing to repair is exactly the case a
                // post-commit re-ask has no evidence to run for, and it is the
                // schedule the review named.
                let fill = match schedule {
                    Schedule::After => 0,
                    Schedule::Before => 8_000,
                    Schedule::Deferred => 40_000,
                };
                {
                    let conn = Connection::open(&path).unwrap();
                    for i in 0..fill {
                        conn.execute(
                            "INSERT INTO sessions(session_uid, session_id, tmux_session,
                                                  tmux_socket, cwd, claude_session_id,
                                                  transcript_path, lifecycle, created_at,
                                                  updated_at, agent, codex_thread_id,
                                                  codex_socket)
                             VALUES(?1, 'cx-f', 'cx-f', 'codeconnect', '/work', NULL, NULL,
                                    'live', 't', 't', 'codex', NULL, NULL)",
                            params![format!("CXFILL{i:020}")],
                        )
                        .unwrap();
                    }
                }

                OLD_WRITE_WAITS.store(0, Ordering::SeqCst);
                let old_write = {
                    let path = path.clone();
                    let uid = codex.uid.clone();
                    move |wait_for_the_lock: bool| -> rusqlite::Result<usize> {
                        let conn = Connection::open(&path).unwrap();
                        conn.busy_handler(Some(count_a_wait)).unwrap();
                        if wait_for_the_lock {
                            // Held back until the migration is *certainly*
                            // inside its transaction, established by two
                            // observations together rather than by a sleep:
                            //
                            //   * the write lock is held — asked for without
                            //     waiting, and refused;
                            //   * and the move has not committed — a WAL read,
                            //     which never blocks, so it is answerable while
                            //     the lock is held.
                            //
                            // Both are needed. `Store::open` takes the write
                            // lock more than once (`drop_retired_columns` runs
                            // its own short transaction after the commit), so
                            // "somebody holds it" alone would let this fire on
                            // the far side of the migration and race an empty
                            // window. The second condition is what makes the
                            // holder the migration.
                            //
                            // Separate connections, so the counter above stays a
                            // record of the real statement's waiting and nothing
                            // else.
                            // Two of them, because they want opposite settings:
                            // the lock ask must *not* wait, or it would sit in
                            // the busy handler instead of reporting what it
                            // found, while the read must wait like any other
                            // reader — a WAL read never blocks on a writer, but
                            // it can still meet a moment of WAL recovery.
                            let probe = Connection::open(&path).unwrap();
                            probe.busy_timeout(std::time::Duration::ZERO).unwrap();
                            let watch = Connection::open(&path).unwrap();
                            let last_fill = format!("CXFILL{:020}", fill - 1);
                            let moved = |watch: &Connection| -> bool {
                                watch
                                    .query_row(
                                        "SELECT EXISTS(SELECT 1 FROM codex_sessions
                                                        WHERE session_uid = ?1)",
                                        params![last_fill],
                                        |r| r.get::<_, bool>(0),
                                    )
                                    .unwrap()
                            };
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(30);
                            loop {
                                assert!(
                                    !moved(&watch),
                                    "the migration committed before this could catch it holding \
                                     the lock, so the deferred schedule was never driven"
                                );
                                if probe.execute_batch("BEGIN IMMEDIATE").is_err() {
                                    break;
                                }
                                probe.execute_batch("ROLLBACK").unwrap();
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "the migration never held the write lock where this could \
                                     see it, so the deferred schedule was never driven"
                                );
                                // Room for the migration to take the lock. Only
                                // on the path where it does not hold it — the
                                // refusal is what ends this loop, and nothing
                                // sleeps between seeing it and issuing the
                                // statement.
                                std::thread::sleep(std::time::Duration::from_micros(200));
                            }
                        }
                        v060_upsert(&conn, &uid, "/work/moved", "2099-01-01T00:00:00.000Z")
                    }
                };

                let refusal = match schedule {
                    Schedule::Before => {
                        let refusal = old_write(false);
                        Store::open(&path).unwrap();
                        refusal
                    }
                    Schedule::Deferred => {
                        let writer = std::thread::spawn(move || old_write(true));
                        Store::open(&path).unwrap();
                        writer.join().unwrap()
                    }
                    Schedule::After => {
                        Store::open(&path).unwrap();
                        old_write(false)
                    }
                };

                let err = refusal.expect_err(&format!(
                    "round {round} {schedule:?}: the shared table accepted a second copy of a \
                     Codex run. Whether a later repair would have removed it is beside the \
                     point — a rolled-back daemon acts on the row it has just written."
                ));
                assert!(
                    matches!(
                        err.sqlite_error_code(),
                        Some(rusqlite::ErrorCode::ConstraintViolation)
                    ),
                    "round {round} {schedule:?}: refused, but not by the constraint that is \
                     supposed to be doing it: {err}"
                );
                if matches!(schedule, Schedule::Deferred) {
                    assert!(
                        OLD_WRITE_WAITS.load(Ordering::SeqCst) > 0,
                        "round {round}: the statement was supposed to block behind the \
                         migration's write lock and never did, so this round proved nothing \
                         about the deferred schedule"
                    );
                }

                let store = Store::open(&path).unwrap();
                let shadows: i64 = {
                    let conn = store.read();
                    conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE session_uid = ?1",
                        params![codex.uid],
                        |r| r.get(0),
                    )
                    .unwrap()
                };
                assert_eq!(
                    shadows, 0,
                    "round {round} {schedule:?}: a default-Claude shadow of a Codex run is in \
                     the shared table"
                );
                let isolated = store.get_session(&codex.uid).unwrap().expect("one row");
                assert_eq!(
                    isolated.agent,
                    AgentKind::Codex,
                    "round {round} {schedule:?}: identity must survive"
                );
                assert_eq!(isolated.codex_thread_id.as_deref(), Some("th_ABC123"));
                assert_eq!(
                    isolated.cwd, row.cwd,
                    "round {round} {schedule:?}: the refused write must not have advanced the \
                     surviving row either"
                );
                assert_eq!(
                    store.count_events(&codex.uid).unwrap(),
                    1,
                    "round {round} {schedule:?}: the children are keyed by uid and are never \
                     part of this"
                );
                let listed = store
                    .list_sessions()
                    .unwrap()
                    .iter()
                    .filter(|s| s.session_uid == codex.uid)
                    .count();
                assert_eq!(
                    listed, 1,
                    "round {round} {schedule:?}: the UNION ALL is returning the run twice"
                );
            }
        }
    }

    /// The refusal is narrow enough to leave Claude and the agent flip alone.
    ///
    /// A trigger written on the uid alone would refuse two writes it must not.
    ///
    ///   * An ordinary Claude registration. Its uid is not in `codex_sessions`
    ///     and never will be, so the `EXISTS` is false and the statement is not
    ///     examined — but that is the claim, and it is worth pinning.
    ///   * A re-registration that moves a run *from* Codex *to* Claude. That
    ///     genuinely does insert into `sessions` for a uid living in
    ///     `codex_sessions`: [`Store::upsert_session`]'s carry is
    ///     `INSERT INTO sessions SELECT * FROM codex_sessions`. `NEW.agent` is
    ///     that row's real agent rather than the default, which is exactly what
    ///     the `WHEN` clause distinguishes, so the flip goes through and every
    ///     `COALESCE`d field survives it.
    #[test]
    fn the_refusal_is_narrow_enough_to_leave_claude_and_the_agent_flip_alone() {
        let (store, _path) = temp_store();

        let claude = key("AA", "cc-1");
        store
            .upsert_session(&session_row(&claude))
            .unwrap()
            .assert_present();
        assert_eq!(table_counts(&store), (1, 0));

        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.transcript_path = Some("/tmp/codex.jsonl".into());
        store.upsert_session(&row).unwrap().assert_present();
        assert_eq!(table_counts(&store), (1, 1));

        // The flip: same uid, now announcing itself as Claude.
        let mut flipped = session_row(&codex);
        flipped.transcript_path = None;
        store.upsert_session(&flipped).unwrap().assert_present();
        assert_eq!(
            table_counts(&store),
            (2, 0),
            "the carry is what moves the row, and the trigger must not stand in front of it"
        );
        let moved = store.get_session(&codex.uid).unwrap().expect("one row");
        assert_eq!(moved.agent, AgentKind::Claude);
        assert_eq!(
            moved.transcript_path.as_deref(),
            Some("/tmp/codex.jsonl"),
            "the carry moves the row rather than rebuilding it, so a COALESCEd field survives"
        );
    }

    /// A shadow written before this schema shipped is still repaired at open.
    ///
    /// `sessions_refuse_codex_shadow` stops the shadow being *written*; it says
    /// nothing about one already on disk when the trigger arrives. That is the
    /// migration's remaining job, and it is not hypothetical: `sessions` carried
    /// the `agent` column for a release before `codex_sessions` existed, so a
    /// non-Claude run really can be sitting in the shared table on first upgrade.
    ///
    /// Staged by taking the trigger away and writing the row the old binary
    /// writes — the only honest way to produce a shadow no trigger was there to
    /// refuse. Both shapes at once: a uid that is also in `codex_sessions`
    /// (merged) and one that is not (carried whole).
    #[test]
    fn a_shadow_written_before_this_schema_shipped_is_repaired_at_open() {
        let (store, path) = temp_store();
        let merged = key("CX", "cx-1");
        let carried = key("CY", "cx-2");
        let mut row = codex_session_row(&merged);
        row.lifecycle = Lifecycle::Live;
        row.cwd = "/work/before".into();
        row.updated_at = "2020-01-01T00:00:00.000Z".into();
        store.upsert_session(&row).unwrap().assert_present();
        assert_eq!(table_counts(&store), (0, 1));
        drop(store);

        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TRIGGER sessions_refuse_codex_shadow")
            .unwrap();
        // Exactly what a v0.6.0 daemon writes: ten columns, `agent` taking this
        // schema's default, and nothing present to refuse it.
        v060_upsert(
            &conn,
            &merged.uid,
            "/work/after",
            "2099-01-01T00:00:00.000Z",
        )
        .unwrap();
        // And the other shape, for a uid the old daemon knew and this build does
        // not have isolated: still not Claude's, still must leave.
        conn.execute(
            "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                  claude_session_id, transcript_path, lifecycle,
                                  created_at, updated_at, agent, codex_thread_id, codex_socket)
             VALUES(?1, 'cx-2', 'cx-2', 'codeconnect', '/work', NULL, NULL, 'live',
                    't', 't', 'codex', 'th_XYZ', NULL)",
            params![carried.uid],
        )
        .unwrap();
        assert!(
            needs_codex_session_move(&conn).unwrap(),
            "the seeded state is the one the repair exists for"
        );
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert!(
            !needs_codex_session_move(&store.read()).unwrap(),
            "the repair left the shared table holding a run that does not belong in it"
        );
        let healed = store.get_session(&merged.uid).unwrap().expect("one row");
        assert_eq!(healed.agent, AgentKind::Codex, "identity is not regressed");
        assert_eq!(healed.codex_thread_id.as_deref(), Some("th_ABC123"));
        assert_eq!(
            healed.cwd, "/work/after",
            "and the strictly fresher of the two copies still wins on the family the old \
             daemon advances"
        );
        assert_eq!(
            store.get_session(&carried.uid).unwrap().unwrap().agent,
            AgentKind::Codex
        );
        assert_eq!(table_counts(&store), (0, 2));
        // And the trigger is back, so the next old-shaped write is refused
        // rather than repaired at the start after that.
        let conn = Connection::open(&path).unwrap();
        assert!(
            v060_upsert(
                &conn,
                &merged.uid,
                "/work/again",
                "2099-01-02T00:00:00.000Z"
            )
            .is_err(),
            "the repair must leave the refusal in place behind it"
        );
    }

    /// **An ended shadow cannot take a live run's history with it.**
    ///
    /// The destructive shape, and it is not a near miss. With
    /// `sessions(CX) = exited` beside `codex_sessions(CX) = live`, both the
    /// single delete and the prune used to:
    ///
    ///   1. accept the uid, because the guard read *an* identity row and the
    ///      unordered `all_sessions` lookup could hand it the ended one;
    ///   2. delete every `SESSION_SCOPED_TABLES` row under the uid — the live
    ///      run's events, answers and ledgers — **before** touching either
    ///      identity row;
    ///   3. delete identity rows under the exit predicate, which matched the
    ///      shadow and not the live copy, so the "exactly one row" assertion saw
    ///      `1` and was satisfied;
    ///   4. commit, tombstone the uid, and leave the live identity standing with
    ///      its whole history gone and a `deleted_sessions` row now forbidding
    ///      anything from being filed under it again.
    ///
    /// `sessions_refuse_codex_shadow` is what stops that state existing, so this
    /// stages it from before the refusal. The guard is kept anyway and is not
    /// redundant belt: the trigger stops the duplicate being *written*, and this
    /// is what stops a duplicate already on disk — one that arrived on an older
    /// schema and has not yet met `Store::open` — being *destructive*. The exit
    /// predicate now has to hold for every identity row of the uid, not for one
    /// of them.
    ///
    /// Both callers, because they are two statements and only shared a comment.
    #[test]
    fn an_ended_shadow_cannot_take_a_live_runs_history_with_it() {
        for prune in [false, true] {
            let (store, path) = temp_store();
            let codex = key("CX", "cx-1");
            let mut row = codex_session_row(&codex);
            row.lifecycle = Lifecycle::Live;
            store.upsert_session(&row).unwrap().assert_present();
            store
                .append_event(&pending(&codex, EventKind::ToolCall, Some("e0")))
                .unwrap();
            store
                .append_event(&pending(&codex, EventKind::ToolCall, Some("e1")))
                .unwrap();
            store
                .record_answer(
                    &codex.uid,
                    "req-1",
                    "hash-1",
                    &outcome("req-1", AnswerDecision::Allow),
                )
                .unwrap();

            // The shadow, staged on a second connection while the store above is
            // still open — `Store::open` repairs one, so a reopen here would
            // remove the very state under test.
            {
                let conn = Connection::open(&path).unwrap();
                stage_a_shadow_from_before_the_refusal(
                    &conn,
                    &codex.uid,
                    "/work/shadow",
                    "2099-01-01T00:00:00.000Z",
                );
                conn.execute(
                    "UPDATE sessions SET lifecycle = 'exited' WHERE session_uid = ?1",
                    params![codex.uid],
                )
                .unwrap();
            }
            assert_eq!(
                table_counts(&store),
                (1, 1),
                "prune={prune}: the staged state is one uid in both tables"
            );

            if prune {
                let removed = store.prune_exited_sessions(&[], false).unwrap();
                assert!(
                    !removed.iter().any(|r| r.session_uid == codex.uid),
                    "prune={prune}: the sweep reported removing a uid whose Codex run is live"
                );
            } else {
                assert_eq!(
                    store.delete_exited_session(&codex.uid).unwrap(),
                    DeleteOutcome::NotExited {
                        lifecycle: lifecycle_str(Lifecycle::Live).into()
                    },
                    "prune={prune}: the delete accepted a uid whose Codex run is live"
                );
            }

            // The live run, and everything filed under it, is exactly as it was.
            assert_eq!(
                store.count_events(&codex.uid).unwrap(),
                2,
                "prune={prune}: the live run's events were deleted through its ended shadow"
            );
            let conn = store.read();
            for table in SESSION_SCOPED_TABLES {
                let left: i64 = conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE session_uid = ?1"),
                        params![codex.uid],
                        |r| r.get(0),
                    )
                    .unwrap();
                let expected = match *table {
                    "events" => 2,
                    "answers" => 1,
                    _ => 0,
                };
                assert_eq!(
                    left, expected,
                    "prune={prune}: {table} lost the live run's rows"
                );
            }
            let tombstoned: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM deleted_sessions WHERE session_uid = ?1)",
                    params![codex.uid],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                !tombstoned,
                "prune={prune}: a uid whose run is still live was tombstoned, so nothing may be \
                 filed under it again"
            );
            let live: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM codex_sessions WHERE session_uid = ?1",
                    params![codex.uid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(live, 1, "prune={prune}: the live identity row is gone");
        }
    }

    /// An equal `updated_at` splits: lifecycle to the shared copy, location to
    /// the isolated one.
    ///
    /// The two branches of [`move_codex_sessions`]'s tie-break, pinned together
    /// because they are one rule and the reason they differ is the whole point.
    /// `lifecycle` takes the shared side on a tie, which is the causal reading of
    /// a genuine same-millisecond collision. The location family takes the
    /// isolated side, because a shared-side location is news only if a v0.6.0
    /// `upsert_session` wrote it — and that write is exactly what makes
    /// `s.updated_at` strictly greater. An equal stamp is therefore not evidence
    /// of a location write, and the next test shows what moving on it costs.
    #[test]
    fn an_equal_updated_at_gives_lifecycle_to_the_shared_copy_and_location_to_the_isolated_one() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Exited;
        row.cwd = "/work/before".into();
        row.updated_at = "2026-08-28T12:00:00.000Z".into();
        let born = row.created_at.clone();
        store.upsert_session(&row).unwrap().assert_present();
        drop(store);

        {
            let conn = Connection::open(&path).unwrap();
            // The same stamp, to the millisecond. `v060_upsert` writes
            // `lifecycle = 'live'`, so the two sides genuinely disagree on both
            // families and each one has something to decide.
            stage_a_shadow_from_before_the_refusal(
                &conn,
                &codex.uid,
                "/work/after",
                "2026-08-28T12:00:00.000Z",
            );
        }

        let store = Store::open(&path).unwrap();
        let healed = store.get_session(&codex.uid).unwrap().expect("one row");
        assert_eq!(
            healed.lifecycle,
            Lifecycle::Live,
            "on a tie the rollback-written copy is the one that was live most recently, and \
             lifecycle is the family that reading is about"
        );
        assert_eq!(
            healed.cwd, "/work/before",
            "an equal stamp is not evidence that a location was written, so location does not \
             move on one"
        );
        // And the tie decides only the family v0.6.0 can advance. Identity is
        // absent from the SET list, so it cannot regress in either direction.
        assert_eq!(healed.agent, AgentKind::Codex);
        assert_eq!(healed.codex_thread_id.as_deref(), Some("th_ABC123"));
        assert_eq!(healed.created_at, born);
        assert_eq!(table_counts(&store), (0, 1));
    }

    /// A strictly later shared write still takes the whole family.
    ///
    /// The counterpart to the test above, and the reason the split is a split
    /// rather than a reversal: an old daemon that really did re-register a run
    /// wrote its location, that write advanced `updated_at` past the isolated
    /// copy's, and the merge must keep it. Only the *tie* is ambiguous.
    #[test]
    fn a_strictly_later_shared_write_still_wins_the_whole_family() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Exited;
        row.cwd = "/work/before".into();
        row.updated_at = "2026-08-28T12:00:00.000Z".into();
        store.upsert_session(&row).unwrap().assert_present();
        drop(store);

        {
            let conn = Connection::open(&path).unwrap();
            stage_a_shadow_from_before_the_refusal(
                &conn,
                &codex.uid,
                "/work/after",
                "2026-08-28T12:00:00.001Z",
            );
        }

        let store = Store::open(&path).unwrap();
        let healed = store.get_session(&codex.uid).unwrap().expect("one row");
        assert_eq!(healed.cwd, "/work/after");
        assert_eq!(healed.lifecycle, Lifecycle::Live);
        assert_eq!(healed.agent, AgentKind::Codex);
        assert_eq!(table_counts(&store), (0, 1));
    }

    /// One lifecycle transition stamps one time, and that stamp does not decide
    /// a location.
    ///
    /// [`Store::set_lifecycle`] writes both physical tables, because the uid is a
    /// primary key in each and is supposed to live in exactly one. Two things
    /// follow when it does not, and this covers both:
    ///
    ///   * **One transition, one time.** Reading the clock per statement would
    ///     let one transition claim two different `updated_at` values.
    ///   * **And that one time must not tear the merge.** The stamp levels the
    ///     two copies while changing nothing but `lifecycle`, so the equality it
    ///     creates says nothing about location. This runs the reconciliation
    ///     afterwards and asserts *which family survives*: the fresher isolated
    ///     `cwd` must still be there. Checking only that the stamps matched — the
    ///     shape the review found — passes just as well while the merge hands the
    ///     whole of a stale rollback row over the top of it.
    ///
    /// **Repeated, because once proves nothing about the first claim.** Two clock
    /// reads a few microseconds apart usually land in the same millisecond, so a
    /// single transition passes whether the read is hoisted or not — that version
    /// of this test was written, run against the un-hoisted code, and *passed*.
    /// What separates them is a read pair straddling a millisecond boundary,
    /// which is a few percent of attempts; several hundred transitions make it a
    /// certainty. Measured: the un-hoisted shape fails within the first hundred.
    #[test]
    fn ending_a_run_stamps_one_time_across_both_tables() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        row.cwd = "/work/isolated".into();
        store.upsert_session(&row).unwrap().assert_present();
        // A duplicate, staged from before the refusal, because the point is what
        // happens when the invariant does not hold.
        {
            let conn = Connection::open(&path).unwrap();
            stage_a_shadow_from_before_the_refusal(
                &conn,
                &codex.uid,
                "/work/shared",
                "2020-01-01T00:00:00.000Z",
            );
        }

        for attempt in 0..600 {
            let to = if attempt % 2 == 0 {
                Lifecycle::Exited
            } else {
                Lifecycle::Live
            };
            store.set_lifecycle(&codex.uid, to).unwrap();

            let conn = store.read();
            let stamps: Vec<String> = SESSION_TABLES
                .iter()
                .map(|table| {
                    conn.query_row(
                        &format!("SELECT updated_at FROM {table} WHERE session_uid = ?1"),
                        params![codex.uid],
                        |r| r.get::<_, String>(0),
                    )
                    .unwrap()
                })
                .collect();
            assert_eq!(
                stamps[0], stamps[1],
                "attempt {attempt}: one transition wrote two different times, and a later merge \
                 compares exactly these two values to decide a whole column family"
            );
        }

        // And now the merge that comparison feeds. The isolated copy's `cwd` was
        // written by this build and is the fresher of the two by construction;
        // the shared copy's is from a rollback in 2020. The levelled stamp must
        // not be what hands the stale one the whole family.
        store.set_lifecycle(&codex.uid, Lifecycle::Exited).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let healed = store.get_session(&codex.uid).unwrap().expect("one row");
        assert_eq!(
            healed.cwd, "/work/isolated",
            "the lifecycle write levelled the two stamps without touching either location, and \
             the merge took the stale rollback copy's whole family on the strength of it"
        );
        assert_eq!(healed.agent, AgentKind::Codex);
        assert_eq!(healed.codex_thread_id.as_deref(), Some("th_ABC123"));
        assert_eq!(table_counts(&store), (0, 1));
    }

    /// **The rollback round trip, and it heals itself.**
    ///
    /// v0.6.0 has no `agent` column of its own: its `upsert_session` names ten
    /// columns, so the eleventh takes this schema's `DEFAULT 'claude'`. A Codex
    /// run that reconnects while a rolled-back daemon is in charge is therefore
    /// re-filed into the swept table wearing the wrong agent, while its real row
    /// is still in `codex_sessions`. One uid, two tables — and an `agent`-only
    /// move predicate never sees it, so the damage would be permanent: the
    /// `UNION ALL` view returns the run twice, the old daemon's prune can walk
    /// the shared copy and take the events with it, and the next agent flip
    /// carries a row onto a uid that is already a primary key in the
    /// destination and fails outright.
    ///
    /// Driven as the whole cycle rather than as a seeded end state — v4, a
    /// v0.6.0-shaped reconnect with `user_version` put back to 3, then v4 again
    /// — because the claim is about the round trip and not about one row.
    #[test]
    fn a_rollback_that_refiles_a_codex_run_under_the_default_agent_is_repaired_on_the_way_up() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        row.transcript_path = Some("/tmp/codex.jsonl".into());
        let born = row.created_at.clone();
        store.upsert_session(&row).unwrap().assert_present();
        for i in 0..2 {
            store
                .append_event(&pending(
                    &codex,
                    EventKind::ToolCall,
                    Some(&format!("e{i}")),
                ))
                .unwrap();
        }
        assert_eq!(table_counts(&store), (0, 1));
        drop(store);

        // The rollback. Ten columns, exactly as v0.6.0's own upsert names them,
        // and the version number it writes back unconditionally. The run has
        // moved on while the old daemon was in charge: a new cwd, and ended.
        {
            let conn = Connection::open(&path).unwrap();
            from_before_the_refusal(&conn, |conn| {
                conn.execute(
                    "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                          claude_session_id, transcript_path, lifecycle,
                                          created_at, updated_at)
                     VALUES(?1, 'cx-1', 'cx-1', 'codeconnect', '/work/moved', NULL, NULL,
                            'exited', '2099-01-01T00:00:00.000Z', '2099-01-01T00:00:00.000Z')",
                    params![codex.uid],
                )
                .unwrap();
            });
            conn.pragma_update(None, "user_version", 3i64).unwrap();
            // The shape this test exists for, stated rather than assumed.
            let agent: String = conn
                .query_row(
                    "SELECT agent FROM sessions WHERE session_uid = ?1",
                    params![codex.uid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                agent, "claude",
                "the old build cannot write any other value"
            );
            let duplicated: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM all_sessions WHERE session_uid = ?1",
                    params![codex.uid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(duplicated, 2, "one uid, both tables — the damage to repair");
        }

        let store = Store::open(&path).unwrap();

        assert_eq!(
            table_counts(&store),
            (0, 1),
            "the shared copy is gone and the run is back to living in one table"
        );
        assert_eq!(
            store.list_sessions().unwrap().len(),
            1,
            "the fleet listing sees one run, not the two the UNION ALL was returning"
        );
        assert_eq!(
            store.count_events(&codex.uid).unwrap(),
            2,
            "the shared children were never keyed on which table the identity sat in"
        );

        let healed = store.get_session(&codex.uid).unwrap().expect("one row");
        // Identity, which v0.6.0 cannot write and therefore can only have lost.
        assert_eq!(healed.agent, AgentKind::Codex);
        assert_eq!(healed.codex_thread_id.as_deref(), Some("th_ABC123"));
        assert_eq!(
            healed.codex_socket.as_deref(),
            Some("/tmp/cch.test/ccd.sock")
        );
        assert_eq!(
            healed.created_at, born,
            "a run is not born again by a rollback"
        );
        // History, which it can legitimately have advanced — and did.
        assert_eq!(healed.lifecycle, Lifecycle::Exited);
        assert_eq!(healed.cwd, "/work/moved");
        assert_eq!(healed.updated_at, "2099-01-01T00:00:00.000Z");
        // And the `COALESCE` rule on top: a known value the fresher write does
        // not carry is still not blanked.
        assert_eq!(healed.transcript_path.as_deref(), Some("/tmp/codex.jsonl"));

        // The failure that was waiting downstream: an agent flip now carries the
        // row instead of colliding with a second copy of it.
        store
            .upsert_session(&session_row(&codex))
            .unwrap()
            .assert_present();
        assert_eq!(table_counts(&store), (1, 0));
    }

    /// The other half of the merge rule: the isolated row is the fresher one.
    ///
    /// Freshest-wins has to be a comparison and not a preference for whichever
    /// side the code happens to read first, or a rollback that touched nothing
    /// would still roll a live run backwards to whatever it looked like when the
    /// old daemon adopted it.
    #[test]
    fn reconciling_keeps_the_isolated_rows_history_when_it_is_the_fresher_one() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        row.cwd = "/work/current".into();
        row.updated_at = "2099-01-01T00:00:00.000Z".into();
        row.transcript_path = None;
        store.upsert_session(&row).unwrap().assert_present();
        drop(store);

        {
            let conn = Connection::open(&path).unwrap();
            from_before_the_refusal(&conn, |conn| {
                conn.execute(
                    "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                          claude_session_id, transcript_path, lifecycle,
                                          created_at, updated_at)
                     VALUES(?1, 'cx-1', 'cx-1', 'codeconnect', '/work/stale', NULL,
                            '/tmp/learned.jsonl', 'exited',
                            '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:00.000Z')",
                    params![codex.uid],
                )
                .unwrap();
            });
        }

        let store = Store::open(&path).unwrap();
        let healed = store.get_session(&codex.uid).unwrap().expect("one row");
        assert_eq!(table_counts(&store), (0, 1));
        assert_eq!(
            healed.lifecycle,
            Lifecycle::Live,
            "a stale copy cannot end a live run"
        );
        assert_eq!(healed.cwd, "/work/current");
        assert_eq!(healed.updated_at, "2099-01-01T00:00:00.000Z");
        assert_eq!(
            healed.transcript_path.as_deref(),
            Some("/tmp/learned.jsonl"),
            "but a value only the stale copy holds is still not thrown away"
        );
    }

    /// An agent this build does not understand is moved out too.
    ///
    /// `sessions` is the table the old daemon sweeps. A `gemini` row left in it
    /// is the same total loss as a Codex one, for an agent we can say even less
    /// about — so the move is defined as "not Claude's", not as "Codex's".
    #[test]
    fn an_unsupported_agents_run_is_moved_out_of_the_swept_table_as_well() {
        let (store, path) = temp_store();
        let stranger = key("ZZ", "gx-1");
        drop(store);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                      claude_session_id, transcript_path, lifecycle,
                                      created_at, updated_at, agent, codex_thread_id, codex_socket)
                 VALUES(?1, 'gx-1', 'gx-1', 'codeconnect', '/work', NULL, NULL, 'live',
                        't', 't', 'gemini', NULL, NULL)",
                params![stranger.uid],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(table_counts(&store), (0, 1));
        assert_eq!(
            store.get_session(&stranger.uid).unwrap().unwrap().agent,
            AgentKind::Unsupported("gemini".into()),
            "moved without being reinterpreted as an agent we do know"
        );
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
        store.upsert_session(&row).unwrap().assert_present();
        let mut second = codex_session_row(&doomed);
        second.lifecycle = Lifecycle::Live;
        store.upsert_session(&second).unwrap().assert_present();
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

    /// A run is in one table or the other, never both — including when its agent
    /// changes under it.
    ///
    /// `all_sessions` is a `UNION ALL`, so a uid in both halves would be listed
    /// twice and resolved arbitrarily. A re-registration really can arrive under
    /// a different agent (`register_supervisor` treats it as the authority), so
    /// this is a shape the store must handle rather than one it may declare
    /// impossible. It moves the row, and — the half a delete-and-insert would
    /// silently lose — it moves the `COALESCE`-preserved fields with it: the
    /// transcript path and the Codex identity that a later write does not carry
    /// are properties of the run, not of the table it is filed in.
    #[test]
    fn changing_a_runs_agent_moves_its_row_rather_than_duplicating_it() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        let mut learned = session_row(&session);
        learned.transcript_path = Some("/tmp/x.jsonl".into());
        store.upsert_session(&learned).unwrap().assert_present();
        assert_eq!(table_counts(&store), (1, 0));

        // Claude -> Codex. A heartbeat-shaped write: it knows the new agent and
        // carries no transcript path.
        let mut flipped = codex_session_row(&session);
        flipped.transcript_path = None;
        store.upsert_session(&flipped).unwrap().assert_present();
        assert_eq!(
            table_counts(&store),
            (0, 1),
            "the run moved; it was not copied — the shared table is empty again"
        );
        let moved = store.get_session(&session.uid).unwrap().expect("one row");
        assert_eq!(moved.agent, AgentKind::Codex);
        assert_eq!(
            moved.transcript_path.as_deref(),
            Some("/tmp/x.jsonl"),
            "a learned field survived the move, as it survives every other update"
        );
        assert_eq!(moved.codex_thread_id.as_deref(), Some("th_ABC123"));

        // And back again, still one row, still carrying what it learned.
        let mut back = session_row(&session);
        back.transcript_path = None;
        store.upsert_session(&back).unwrap().assert_present();
        assert_eq!(table_counts(&store), (1, 0));
        let returned = store.get_session(&session.uid).unwrap().expect("one row");
        assert_eq!(returned.agent, AgentKind::Claude);
        assert_eq!(returned.transcript_path.as_deref(), Some("/tmp/x.jsonl"));
        assert_eq!(
            returned.codex_thread_id.as_deref(),
            Some("th_ABC123"),
            "COALESCE preserves it here exactly as it does within one table"
        );
        assert_eq!(
            store.list_sessions().unwrap().len(),
            1,
            "the fleet listing sees one run, not two"
        );
    }

    /// A tombstoned uid stays deleted whichever agent asks for it back.
    ///
    /// The pre-existing tombstone test writes the same agent the run had; this
    /// one comes back as the *other* one, which is the write that now also
    /// carries rows between tables. Nothing may be recreated and nothing may be
    /// relocated — the run was removed from both tables and the tombstone is
    /// what keeps it that way.
    #[test]
    fn a_deleted_uid_is_not_recreated_by_a_write_that_names_a_different_agent() {
        let (store, _path) = temp_store();
        let session = key("AA", "cc-1");
        seed_run(&store, &session, Lifecycle::Exited, 1);
        assert_eq!(
            store.delete_exited_session(&session.uid).unwrap(),
            DeleteOutcome::Deleted { events: 1 }
        );

        assert_eq!(
            store.upsert_session(&codex_session_row(&session)).unwrap(),
            SessionUpsert::Tombstoned
        );
        assert_eq!(
            table_counts(&store),
            (0, 0),
            "the deletion stands, in both tables"
        );
    }

    /// **The old binary's delete, not this one's, and it must not resurrect the
    /// run.**
    ///
    /// `a_deleted_uid_is_not_recreated_by_a_write_that_names_a_different_agent`
    /// above deletes through [`Store::delete_exited_session`], which removes the
    /// run from *both* tables — so the tombstone it leaves has nothing standing
    /// behind it and the claim it proves is the easy one. v0.6.0 deletes
    /// differently, and that difference is the whole point of this schema: its
    /// prune removes the `sessions` row and writes `deleted_sessions`, and it has
    /// no statement that names `codex_sessions`. A uid can therefore be
    /// tombstoned with an isolated row still standing.
    ///
    /// So this stages exactly that — the old binary's two statements, verbatim in
    /// shape — and then does the thing that used to make it worse: a
    /// registration under the other agent, whose carry moves rows between tables
    /// before the guarded upsert ever runs. Nothing may come back. The run stays
    /// deleted, in both tables, and the caller is told so.
    ///
    /// (The shadow this needs is itself only producible by a supervisor that
    /// registers a Codex run with a daemon that cannot host one — which
    /// `codeconnect`'s pre-`Register` negotiation now withholds. This is the
    /// second lock, on the store side, for the databases that already went
    /// through it.)
    #[test]
    fn the_old_binarys_delete_cannot_resurrect_a_run_through_the_carry() {
        let (store, path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Exited;
        store.upsert_session(&row).unwrap().assert_present();
        store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("e0")))
            .unwrap();
        assert_eq!(table_counts(&store), (0, 1));
        drop(store);

        {
            let conn = Connection::open(&path).unwrap();
            // The shadow a rolled-back daemon filed when a Codex supervisor
            // registered with it, back when nothing refused one: ten columns,
            // `agent` taking the default.
            stage_a_shadow_from_before_the_refusal(
                &conn,
                &codex.uid,
                "/work",
                "2099-01-01T00:00:00.000Z",
            );
            conn.execute(
                "UPDATE sessions SET lifecycle = 'exited' WHERE session_uid = ?1",
                params![codex.uid],
            )
            .unwrap();
            // And v0.6.0's prune, in its own two statements. `codex_sessions` is
            // not named because that build contains no statement that names it.
            let removed = conn
                .execute(
                    "DELETE FROM sessions WHERE session_uid = ?1 AND lifecycle = 'exited'",
                    params![codex.uid],
                )
                .unwrap();
            assert_eq!(removed, 1, "the old prune removes the shared copy");
            conn.execute(
                "INSERT OR IGNORE INTO deleted_sessions(session_uid, deleted_at) VALUES(?1, ?2)",
                params![codex.uid, "2099-01-01T00:00:00.000Z"],
            )
            .unwrap();
            // The state this test exists for, stated rather than assumed: a
            // tombstone with an isolated row still standing behind it.
            let isolated: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM codex_sessions WHERE session_uid = ?1",
                    params![codex.uid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(isolated, 1, "the old binary cannot reach codex_sessions");
        }

        let store = Store::open(&path).unwrap();
        // A registration under the *other* agent, which is the write whose carry
        // moves a row between tables before the guarded upsert runs.
        assert_eq!(
            store.upsert_session(&session_row(&codex)).unwrap(),
            SessionUpsert::Tombstoned,
            "a tombstoned uid is refused whichever agent asks for it back"
        );
        assert_eq!(
            table_counts(&store),
            (0, 1),
            "the carry must not have moved the isolated row into the swept table"
        );
        let conn = store.read();
        let shadows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_uid = ?1",
                params![codex.uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            shadows, 0,
            "a run came back into the shared table under a uid its own tombstone says is gone"
        );
    }

    /// The two tables must not drift apart.
    ///
    /// `all_sessions` selects the same columns from each, and `upsert_session`
    /// carries a row between them with `INSERT … SELECT *`, so a column added to
    /// one and forgotten on the other turns every session read into a prepare
    /// error and every agent change into a column-count error at runtime. Read
    /// off the schema rather than restated, so a future `COLUMN_ADDITIONS` entry
    /// that names only `sessions` fails here.
    ///
    /// **The view is a narrower claim than the tables, and that is deliberate.**
    /// It projects exactly the columns [`SessionRow`] carries, because
    /// `session_row_from` decodes it positionally; `codex_generation` is a
    /// fourteenth column on both tables and is deliberately *not* in the
    /// projection (see [`Store::codex_generation`], which asks the two tables
    /// directly). So the second assertion names the projection explicitly rather
    /// than deriving it from the table shape — a derived assertion would have
    /// forced this column into the view, and widening a `CREATE VIEW IF NOT
    /// EXISTS` means recreating a view every existing database already has.
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
        // shape — the thirteen columns `session_row_from` decodes by position.
        let stmt = conn.prepare("SELECT * FROM all_sessions").unwrap();
        let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
        let projected = [
            "session_uid",
            "session_id",
            "tmux_session",
            "tmux_socket",
            "cwd",
            "claude_session_id",
            "transcript_path",
            "lifecycle",
            "created_at",
            "updated_at",
            "agent",
            "codex_thread_id",
            "codex_socket",
        ];
        assert_eq!(
            names, projected,
            "all_sessions does not project the SessionRow shape"
        );
        // The projection must still be a leading slice of the physical shape,
        // or the view is naming columns the tables no longer declare in that
        // order and `session_row_from` decodes the wrong field.
        let declared: Vec<String> = columns("sessions").into_iter().map(|c| c.0).collect();
        assert_eq!(
            declared[..projected.len()],
            projected[..],
            "the projected columns are not the leading columns of the table"
        );
        assert_eq!(
            declared[projected.len()..],
            ["codex_generation"],
            "the only column outside the fleet projection is the A5.1 high-water"
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
        store.upsert_session(&row).unwrap().assert_present();
        let now = protocol::time::now_rfc3339();

        // `append_in_tx` — every fact either agent records.
        assert!(store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("one")))
            .unwrap()
            .is_some());

        // `append_batch_with_cursor` — the batch AND its cursor.
        let cursor = TailCursor {
            path: "/tmp/x.jsonl".into(),
            dev: 1,
            ino: 2,
            offset: 4096,
            last_line_start: 4000,
            last_line_sha: "abc".into(),
        };
        let batch = [pending(&codex, EventKind::ToolCall, Some("two"))];
        assert_eq!(
            store
                .append_batch_with_cursor(&codex.uid, &batch, &cursor)
                .unwrap()
                .len(),
            1
        );
        assert!(store.load_cursor(&codex.uid).unwrap().is_some());

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
            LedgerWrite::Existing { .. }
        ));

        // `upsert_pending_approval` no longer accepts a Codex run, and that is
        // the one guard in the six that changed. It used to, on purpose, while
        // the daemon's own refusal was the thing keeping the shared table clean
        // — scaffolding that stood only because nothing produced a Codex card.
        // The approval observer is that producer, so the table split landed and
        // the refusal moved into the schema, where a rolled-back binary keeps
        // it. Refused rather than silently dropped: a store guard that reported
        // success and lost the card is the failure this whole seam exists to
        // prevent in the other direction.
        let refused = store
            .upsert_pending_approval(&PendingApprovalRow {
                session_uid: codex.uid.clone(),
                session_id: codex.name.clone(),
                request_id: "req-2".into(),
                card: "{}".into(),
                generation: 1,
                created_ms: 0,
            })
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
        // producer is the speculative half-surface this plan refuses.
        assert_eq!(
            store
                .claim_text_mutation(&codex.uid, "req-3", "hash", &now)
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
                .claim_text_mutation(&codex.uid, "req-3", "hash", &now)
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

    /// **The store's half of the tripwire for the obligation this chunk leaves
    /// open**, and it is the weaker half on purpose.
    ///
    /// The four tables in [`GLOBALLY_SWEPT_TABLES`] are not split. What keeps
    /// them empty of Codex rows is not the store — the store's own API will
    /// happily write all four for a Codex run, and
    /// `every_existence_guard_asks_about_the_whole_fleet` proves it does, which
    /// is deliberate: a guard that silently dropped a Codex card would leave a
    /// test like this green for ever and ship the bug. What keeps them empty is
    /// an explicit refusal at each of the three producers that can reach them,
    /// in the daemon: `Daemon::send_text`, `Daemon::handle_permission_request`
    /// and `Daemon::answer` each refuse a non-Claude row before any durable
    /// write, and `send_text_to_a_codex_session_is_refused_before_it_claims_anything`,
    /// `a_permission_request_for_a_codex_session_raises_no_card` and
    /// `answering_a_codex_card_is_refused_before_the_claim` drive those real
    /// paths and read these same four tables back off the daemon's own database
    /// file. Those are the tests that can see a producer; this one cannot, and
    /// saying so is the point of this comment.
    ///
    /// What this one still adds is the floor underneath them: the paths a Codex
    /// run *does* travel in this build — facts — must not reach these tables by
    /// some route that has nothing to do with the three gated producers.
    #[test]
    fn no_codex_row_reaches_a_table_a_rolled_back_daemon_sweeps_globally() {
        let (store, _path) = temp_store();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        store.upsert_session(&row).unwrap().assert_present();

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
    /// This is the half of the 2e-7a obligation that `create_schema` said would
    /// come due "the day somebody edits those refusals". The daemon-side gate
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
        store.upsert_session(&row).unwrap().assert_present();

        let shared = store.upsert_pending_approval(&PendingApprovalRow {
            session_uid: codex.uid.clone(),
            session_id: codex.name.clone(),
            request_id: "derived-1".into(),
            card: "{}".into(),
            generation: 1,
            created_ms: 10,
        });
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
        store
            .upsert_session(&session_row(&claude))
            .unwrap()
            .assert_present();
        assert!(store
            .upsert_pending_approval(&PendingApprovalRow {
                session_uid: claude.uid.clone(),
                session_id: claude.name.clone(),
                request_id: "req-1".into(),
                card: "{}".into(),
                generation: 1,
                created_ms: 11,
            })
            .unwrap());
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
        store.upsert_session(&row).unwrap().assert_present();

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
            open[0].created_ms, 10,
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
        store
            .upsert_session(&session_row(&claude))
            .unwrap()
            .assert_present();
        let codex = key("CX", "cx-1");
        let mut row = codex_session_row(&codex);
        row.lifecycle = Lifecycle::Live;
        store.upsert_session(&row).unwrap().assert_present();

        store
            .upsert_pending_approval(&PendingApprovalRow {
                session_uid: claude.uid.clone(),
                session_id: claude.name.clone(),
                request_id: "req-1".into(),
                card: "{}".into(),
                generation: 1,
                created_ms: 1,
            })
            .unwrap();
        insert_codex_card(&store, &codex, "derived-1", "exec-1", "tu-1", 2, "{}").unwrap();

        let open = store.list_pending_approvals().unwrap();
        assert_eq!(open.len(), 2, "recovery must answer for the whole fleet");

        // The lifecycle predicate that guards deletion lives in the SQL, so the
        // run has to have actually ended before it can be deleted.
        let mut ended = codex_session_row(&codex);
        ended.lifecycle = Lifecycle::Exited;
        store.upsert_session(&ended).unwrap().assert_present();
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
        store.upsert_session(&row).unwrap().assert_present();
        store
            .append_event(&pending(&codex, EventKind::ToolCall, Some("one")))
            .unwrap();
        drop(store);

        // Staged the way `from_before_the_refusal` stages one: with the object
        // that would refuse it removed, so the row is the one a build without
        // this schema really could have written.
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
    /// speculative half-surface this plan refuses. What these tests are about
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

    /// **The A5.1 high-water column is added to BOTH session tables on a
    /// database the previous build wrote**, and the two shapes still match
    /// afterwards.
    ///
    /// The three agent-seam columns needed only a `sessions` entry in
    /// [`COLUMN_ADDITIONS`], because every database that could be missing them
    /// predates `codex_sessions` and gets that table created fresh at the
    /// current shape. `codex_generation` is the first column to arrive *after*
    /// `codex_sessions` shipped, so it is the first one that needs the `ALTER`
    /// on both halves — and a one-sided entry would not fail at open. It would
    /// fail later and elsewhere: `all_sessions` would still prepare (it names
    /// neither), and the break would surface as a column-count error inside
    /// `upsert_session`'s `INSERT … SELECT *` the first time a run changed
    /// agent.
    ///
    /// **The predecessor's two tables are written out rather than derived from
    /// this build's**, and that is not the duplication it looks like. The
    /// thirteen-column shape is *finished* — it is what a released build wrote
    /// and will never write again — so a copy of it cannot drift out of step
    /// with anything; deriving it instead would mean taking the column back off
    /// a current database, and SQLite's `DROP COLUMN` reparses the stored
    /// `CREATE TABLE` text and fails on the comments `create_schema` documents
    /// these columns with (measured: `error in table sessions after drop
    /// column: incomplete input`).
    ///
    /// Only these two tables are pre-created. `Store::open` builds the rest with
    /// `CREATE TABLE IF NOT EXISTS`, finds these two already there — which is
    /// precisely the situation that makes [`COLUMN_ADDITIONS`] the only thing
    /// that can widen them — and then runs the additions.
    ///
    /// **Mutation:** delete the `("codex_sessions", "codex_generation", …)`
    /// entry from [`COLUMN_ADDITIONS`] and this fails at the parity assertion
    /// with `codex_sessions` one column short.
    #[test]
    fn the_generation_high_water_is_added_to_both_session_tables_on_an_upgrade() {
        let path = legacy_path();
        {
            let conn = Connection::open(&path).unwrap();
            for table in ["sessions", "codex_sessions"] {
                conn.execute_batch(&format!(
                    "CREATE TABLE {table}(
                         session_uid       TEXT PRIMARY KEY,
                         session_id        TEXT NOT NULL,
                         tmux_session      TEXT NOT NULL,
                         tmux_socket       TEXT NOT NULL,
                         cwd               TEXT NOT NULL,
                         claude_session_id TEXT,
                         transcript_path   TEXT,
                         lifecycle         TEXT NOT NULL,
                         created_at        TEXT NOT NULL,
                         updated_at        TEXT NOT NULL,
                         agent             TEXT NOT NULL DEFAULT 'claude',
                         codex_thread_id   TEXT,
                         codex_socket      TEXT
                     );"
                ))
                .unwrap();
            }
            conn.execute(
                "INSERT INTO codex_sessions(session_uid, session_id, tmux_session, tmux_socket,
                                            cwd, claude_session_id, transcript_path, lifecycle,
                                            created_at, updated_at, agent, codex_thread_id,
                                            codex_socket)
                 VALUES(?1, 'cx-1', 'cx-1', 'codeconnect', '/tmp', NULL, NULL, 'live',
                        '2026-08-01T00:00:00.000Z', '2026-08-01T00:00:00.000Z',
                        'codex', 'th_ABC123', '/tmp/cch.test/ccd.sock')",
                params!["01K1B3XQ8ZC0DE5FGH7JKMNPQR"],
            )
            .unwrap();
            assert!(
                needs_column_additions(&conn).unwrap(),
                "the premise: this database is missing the A5.1 column"
            );
        }

        let store = Store::open(&path).unwrap();
        let conn = store.read();
        let columns = |table: &str| -> Vec<String> {
            conn.prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            columns("sessions"),
            columns("codex_sessions"),
            "the upgrade widened one table and left the other"
        );
        assert!(columns("codex_sessions").contains(&"codex_generation".to_string()));
        drop(conn);

        let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";
        assert_eq!(
            store.codex_generation(uid).unwrap(),
            None,
            "a row that predates the column has no provable high-water, and reads \
             as one — not as generation zero, which would refuse nothing"
        );
        let row = store.get_session(uid).unwrap().expect("the pre-A5.1 run");
        assert_eq!(
            row.agent,
            AgentKind::Codex,
            "and the run itself came through the widening intact"
        );
        assert_eq!(row.codex_thread_id.as_deref(), Some("th_ABC123"));

        // And the widened database really does take a generation now, which is
        // what "the upgrade path leaves a usable high-water" means.
        store
            .upsert_session_at_generation(&row, Some(2))
            .unwrap()
            .assert_present();
        assert_eq!(store.codex_generation(uid).unwrap(), Some(2));
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
        assert_eq!(version, 5, "the schema version other builds will read");
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
        assert!(!needs_session_uid_migration(&conn).unwrap());
        // Both halves of the fleet and the view that reads them, built by the
        // first open rather than by a migration a fresh file would never run.
        assert!(table_exists(&conn, "codex_sessions").unwrap());
        assert!(!needs_codex_session_move(&conn).unwrap());
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
            .push_targets(TEST_FEATURE_EPOCH)
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

        let targets = store.push_targets(TEST_FEATURE_EPOCH).unwrap();
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
        assert_eq!(store.push_targets(TEST_FEATURE_EPOCH).unwrap().len(), 2);
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
            store.push_targets(TEST_FEATURE_EPOCH).unwrap()[0].environment,
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
            store.push_targets(TEST_FEATURE_EPOCH).unwrap().is_empty(),
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
        let targets = store.push_targets(TEST_FEATURE_EPOCH).unwrap();
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
        assert_eq!(
            store.push_targets(TEST_FEATURE_EPOCH).unwrap()[0].device_id,
            "dev-live"
        );
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
        assert!(store.push_targets(TEST_FEATURE_EPOCH).unwrap().is_empty());
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
            store.push_targets(TEST_FEATURE_EPOCH).unwrap()[0].environment,
            "sandbox",
            "a correction under the superseded credential must not move the live one"
        );

        // C2's correction, for the bearer the row does hold, still applies.
        store
            .set_push_environment("dev-1", "tok-a", Some("cred-2"), "production")
            .unwrap();
        assert_eq!(
            store.push_targets(TEST_FEATURE_EPOCH).unwrap()[0].environment,
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
        assert!(store.push_targets(TEST_FEATURE_EPOCH).unwrap().is_empty());
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
        let targets = store.push_targets(TEST_FEATURE_EPOCH).unwrap();
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
            store.push_targets(TEST_FEATURE_EPOCH).unwrap()[0]
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
        assert!(store.push_targets(TEST_FEATURE_EPOCH).unwrap().is_empty());
    }

    /// **What a push target is allowed to say it can render, and every way of not
    /// saying it.**
    ///
    /// The read that authorizes a push decodes the device's advertised agent set,
    /// and four of the five inputs it can meet are *not* a confirmed set. They do
    /// **not** all land on the same answer, and the split is the whole point:
    ///
    ///   * never advertised (a phone that predates the field) — the genuine
    ///     Claude floor, and the row every legacy phone in the fleet has;
    ///   * advertised, then cleared — the same floor, because advertising nothing
    ///     is itself a claim and the device made it;
    ///   * advertised under a **different daemon run's** epoch — a claim this run
    ///     cannot vouch for, so the device hears nothing;
    ///   * stored bytes that no longer decode — the same.
    ///
    /// **The fixture advertises `[Codex]` and not Claude on purpose.** With a
    /// `[Claude, Codex]` set, every one of these inputs supports Claude either way
    /// and the last two cases cannot be told from the first two — the test would
    /// pass while a restart quietly broadened a Codex-only phone into a
    /// Claude-eligible one (round-3 P1/P7). A set that names Codex *without*
    /// Claude is the shape that makes the difference observable.
    ///
    /// **Mutation:** decode the unconfirmable shapes as
    /// `DeviceFeatures::Advertised(Default::default())` and the Claude assertions
    /// on `other-epoch`/`corrupt` fail.
    #[test]
    fn only_a_set_this_run_confirmed_authorizes_anything_beyond_claude() {
        let (store, _path) = temp_store();
        let codex = serde_json::to_string(&protocol::ws::ClientFeatures {
            agents: vec![protocol::agent::AgentKind::Codex],
        })
        .unwrap();
        // **Both reads, every time.** The fleet list and the per-device lookup
        // are two statements of one rule, and the second exists so a push
        // worker need not scan the fleet — so a decode that drifted between
        // them would be a device authorized differently depending on which
        // question was asked. Asserting they agree HERE, over the four shapes
        // below, is what keeps them one rule.
        let features_of = |device: &str| {
            let from_fleet = store
                .push_targets(TEST_FEATURE_EPOCH)
                .unwrap()
                .into_iter()
                .find(|t| t.device_id == device)
                .expect("the device is registered for push")
                .features;
            let from_lookup = store
                .push_target_for(device, TEST_FEATURE_EPOCH)
                .unwrap()
                .expect("the per-device lookup finds the same registration")
                .features;
            assert_eq!(
                from_fleet, from_lookup,
                "{device}: the fleet read and the per-device lookup disagree"
            );
            from_fleet
        };
        let codex_kind = protocol::agent::AgentKind::Codex;

        for device in ["silent", "confirmed", "other-epoch", "corrupt", "cleared"] {
            store
                .insert_device(
                    device,
                    // Device names are unique, so each phone needs its own.
                    &format!("iPhone ({device})"),
                    &format!("hash-{device}"),
                    "2026-08-01T00:00:00Z",
                )
                .unwrap();
            store
                .set_push_token(device, &format!("tok-{device}"), "production", None)
                .unwrap();
        }
        store
            .set_device_features("confirmed", Some(&codex), TEST_FEATURE_EPOCH)
            .unwrap();
        store
            .set_device_features("other-epoch", Some(&codex), "a-previous-daemon-run")
            .unwrap();
        store
            .set_device_features("corrupt", Some("{not json"), TEST_FEATURE_EPOCH)
            .unwrap();
        store
            .set_device_features("cleared", Some(&codex), TEST_FEATURE_EPOCH)
            .unwrap();
        store
            .set_device_features("cleared", None, TEST_FEATURE_EPOCH)
            .unwrap();

        assert!(
            features_of("confirmed").supports(&codex_kind),
            "a set written under this run's epoch is the one thing that grants Codex"
        );
        assert!(
            !features_of("confirmed").supports(&protocol::agent::AgentKind::Claude),
            "and it grants exactly what it names: this phone said it cannot render Claude"
        );
        for device in ["silent", "other-epoch", "corrupt", "cleared"] {
            assert!(
                !features_of(device).supports(&codex_kind),
                "{device}: this is not a confirmed advertisement, so it must not \
                 authorize a Codex push"
            );
        }
        for device in ["silent", "cleared"] {
            assert!(
                features_of(device).supports(&protocol::agent::AgentKind::Claude),
                "{device}: an empty column is the floor a device gets by saying nothing, \
                 and failing closed must not take it away"
            );
        }
        for device in ["other-epoch", "corrupt"] {
            assert_eq!(
                features_of(device),
                DeviceFeatures::Unconfirmable,
                "{device}: a set is stored and this run cannot read it as the device's \
                 word, so the device hears nothing until it re-advertises"
            );
            assert!(
                !features_of(device).supports(&protocol::agent::AgentKind::Claude),
                "{device}: reading an unreadable [Codex] claim as the Claude floor is \
                 the broadening this split exists to prevent"
            );
        }

        // **And the per-device lookup carries the same ELIGIBILITY predicate, not
        // only the same decode.** A row the fleet read refuses to offer must be
        // absent from the lookup too — otherwise the re-check before a transport
        // attempt would vouch for a phone the fan-out would never have rung.
        // A device that does not exist is the same answer, for the same reason:
        // nothing to vouch for.
        assert!(
            store
                .push_target_for("never-paired", TEST_FEATURE_EPOCH)
                .unwrap()
                .is_none(),
            "a device that does not exist has no registration to offer"
        );
        store
            .revoke_device("confirmed", "2026-08-04T00:00:00Z")
            .unwrap();
        assert!(
            store
                .push_target_for("confirmed", TEST_FEATURE_EPOCH)
                .unwrap()
                .is_none(),
            "a revoked device is refused by the lookup exactly as the fleet read \
             refuses it; revocation must not survive only in the list"
        );
        assert!(
            store
                .push_targets(TEST_FEATURE_EPOCH)
                .unwrap()
                .iter()
                .all(|t| t.device_id != "confirmed"),
            "the premise: the fleet read refuses it too"
        );
        store
            .clear_push_token("silent", "tok-silent", None)
            .unwrap();
        assert!(
            store
                .push_target_for("silent", TEST_FEATURE_EPOCH)
                .unwrap()
                .is_none(),
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
            features: Default::default(),
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
            // Named columns, the way a legacy writer inserts: the agent-seam
            // columns added at open take their defaults ('claude', NULL), so this
            // row is a Claude row exactly as a pre-seam one would have been.
            conn.execute_batch(
                "INSERT INTO sessions(session_uid, session_id, tmux_session, tmux_socket, cwd,
                                      claude_session_id, transcript_path, lifecycle,
                                      created_at, updated_at)
                 VALUES('uid-1','cc-1','codeconnect','/tmp/sock','/tmp/one',
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
