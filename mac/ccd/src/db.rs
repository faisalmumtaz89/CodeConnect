//! The async handle to the event log.
//!
//! [`crate::store::Store`] is synchronous and does real disk I/O. Calling it
//! from an `async fn` therefore blocks a Tokio worker for the whole of a
//! `fsync`, a `BEGIN IMMEDIATE` that has to wait, or a `SELECT` that misses the
//! page cache. With `rt-multi-thread` there are as many workers as cores, so a
//! handful of concurrent slow queries stops *everything* — the WebSocket
//! listener, the hook path, the timers — not just the caller. The store's own
//! `busy_timeout` is 5 seconds, which is the worst case: five seconds of a dead
//! runtime, from a database another process happened to be holding.
//!
//! This type is the boundary. Every method hands the work to `spawn_blocking`,
//! where waiting is what the thread is for, and awaits the result. The store
//! stays synchronous because that is what a blocking pool wants to call, and
//! because the tests and the sync `cc`-facing paths have no runtime to defer to.
//!
//! **What this does not change.** The store still allocates `seq` inside one
//! `BEGIN IMMEDIATE` against a single writer connection, and the caller still
//! holds its per-session publish gate across the await. Moving *where* the work
//! runs does not move *when* it is ordered: a `spawn_blocking` call is awaited
//! to completion before the caller proceeds, so append-then-publish is the same
//! sequence it was, one thread further out.
//!
//! **Cancellation.** A `spawn_blocking` task cannot be cancelled — dropping the
//! future lets the closure run to completion and discards the result. That is
//! the correct behaviour here and not a compromise: a commit that has begun must
//! finish, and a client disconnecting mid-request must not leave a half-written
//! transaction. The one consequence worth stating is that the *result* of such a
//! call is lost, which is why nothing here treats "the future was dropped" as
//! evidence that the write did not happen.

use std::sync::Arc;

use anyhow::{Context, Result};
use protocol::event::{Event, EventKind, Lifecycle, PendingEvent};
use protocol::pairing::DeviceSummary;
use protocol::ws::AnswerOutcome;

use crate::store::{
    AnswerClaim, DeviceLookup, DeviceRow, LedgerWrite, PairingConsume, PendingApprovalRow,
    PrunedSession, SessionRow, Store, TailCursor, TextClaim,
};

#[derive(Clone)]
pub struct Db {
    store: Arc<Store>,
}

impl Db {
    pub fn new(store: Arc<Store>) -> Db {
        Db { store }
    }

    /// The synchronous store underneath.
    ///
    /// For the paths that genuinely have no runtime — the startup integrity
    /// check before any task exists, and the tests. Deliberately named so that
    /// reaching for it from an `async fn` reads as the mistake it would be.
    #[cfg(test)]
    pub fn blocking(&self) -> &Arc<Store> {
        &self.store
    }

    /// Run one store operation on the blocking pool.
    ///
    /// The closure takes `&Store` rather than capturing it, so no call site can
    /// accidentally hold a `MutexGuard` past the end of the operation.
    async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(&Store) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || operation(&store))
            .await
            // A panic inside a store operation is a bug, and the honest report
            // is an error the caller can act on rather than a poisoned lock
            // discovered somewhere unrelated later.
            .context("a database operation panicked")?
    }
}

/// Generate the async twins.
///
/// A macro rather than forty hand-written wrappers, because forty
/// hand-written wrappers is forty chances to await the wrong method. Arguments
/// are owned (`String`, not `&str`) because the closure outlives the call.
macro_rules! db_ops {
    ($(
        $(#[$meta:meta])*
        fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty;
    )*) => {
        impl Db {
            $(
                $(#[$meta])*
                pub async fn $name(&self $(, $arg: $ty)*) -> Result<$ret> {
                    self.run(move |store| store.$name($(db_ops!(@pass $arg)),*)).await
                }
            )*
        }
    };
    // Owned values are passed by reference to the store, which takes `&str` and
    // `&T` throughout. One `&` here rather than a borrow at every call site.
    (@pass $arg:ident) => { &$arg };
}

db_ops! {
    /// Append one fact. `None` means it was a duplicate.
    fn append_event(pending: PendingEvent) -> Option<Event>;
    fn max_seq(session_uid: String) -> u64;
    fn count_events_of_kind(session_uid: String, kind: EventKind) -> u64;
    fn upsert_session(row: SessionRow) -> crate::store::SessionUpsert;
    fn get_session(session_uid: String) -> Option<SessionRow>;
    fn find_session(reference: String) -> Option<SessionRow>;
    fn list_sessions() -> Vec<SessionRow>;
    fn list_pending_approvals() -> Vec<PendingApprovalRow>;
    fn upsert_pending_approval(row: PendingApprovalRow) -> bool;
    fn name_is_tombstoned(session_id: String) -> bool;
    fn clear_name_tombstone(session_id: String) -> ();
    fn claim_answer(claim: AnswerClaim) -> ();
    fn unresolved_answer_claims() -> Vec<AnswerClaim>;
    fn find_device(needle: String) -> DeviceLookup;
    fn device_by_token_hash(token_hash: String) -> Option<DeviceRow>;
    fn device_is_active(device_id: String) -> bool;
    /// Where this device's token is registered, or `None` when it has none.
    fn push_environment_for(device_id: String) -> Option<String>;
    fn recover_text_mutations(at: String) -> usize;
    fn orphan_event_count() -> u64;
}

// The operations whose signatures do not fit the macro's one shape: more than
// one reference kind, a non-`Result` return, or a borrow the store takes by
// value. Written out rather than bent into the macro, because a macro contorted
// to fit every case is harder to read than the six functions it saves.
impl Db {
    /// Append a transcript batch **and** advance its cursor, atomically.
    ///
    /// The batch is the reason this module exists: a cold backfill hands over
    /// megabytes of transcript, and doing that on a runtime worker stopped the
    /// WebSocket listener for its duration.
    pub async fn append_batch_with_cursor(
        &self,
        session_uid: String,
        pendings: Vec<PendingEvent>,
        cursor: TailCursor,
    ) -> Result<Vec<Event>> {
        self.run(move |store| store.append_batch_with_cursor(&session_uid, &pendings, &cursor))
            .await
    }

    pub async fn events_after(
        &self,
        session_uid: String,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<Event>> {
        self.run(move |store| store.events_after(&session_uid, after_seq, limit))
            .await
    }

    pub async fn set_lifecycle(&self, session_uid: String, lifecycle: Lifecycle) -> Result<()> {
        self.run(move |store| store.set_lifecycle(&session_uid, lifecycle))
            .await
    }

    /// Remove one ended run. Small next to a prune, and off the runtime for the
    /// same reason: it is still a multi-table transaction on the write
    /// connection, and the phone is waiting on the answer.
    pub async fn delete_exited_session(
        &self,
        session_uid: String,
    ) -> Result<crate::store::DeleteOutcome> {
        self.run(move |store| store.delete_exited_session(&session_uid))
            .await
    }

    /// Remove ended runs. Potentially thousands of row deletions across seven
    /// tables in one transaction, which is precisely the shape of work that
    /// must not run on a runtime worker.
    pub async fn prune_exited_sessions(
        &self,
        protect: Vec<String>,
        dry_run: bool,
    ) -> Result<Vec<PrunedSession>> {
        self.run(move |store| store.prune_exited_sessions(&protect, dry_run))
            .await
    }

    pub async fn record_answer(
        &self,
        session_uid: String,
        request_id: String,
        payload_hash: String,
        outcome: AnswerOutcome,
    ) -> Result<LedgerWrite> {
        self.run(move |store| {
            store.record_answer(&session_uid, &request_id, &payload_hash, &outcome)
        })
        .await
    }

    pub async fn get_answer(
        &self,
        session_uid: String,
        request_id: String,
    ) -> Result<Option<(String, AnswerOutcome)>> {
        self.run(move |store| store.get_answer(&session_uid, &request_id))
            .await
    }

    pub async fn find_answer_by_request(
        &self,
        request_id: String,
    ) -> Result<Option<(String, String, AnswerOutcome)>> {
        self.run(move |store| store.find_answer_by_request(&request_id))
            .await
    }

    pub async fn consume_pairing_code(
        &self,
        code_hash: String,
        now_ms: i64,
    ) -> Result<PairingConsume> {
        self.run(move |store| store.consume_pairing_code(&code_hash, now_ms))
            .await
    }

    pub async fn answer_claim(
        &self,
        session_uid: String,
        request_id: String,
    ) -> Result<Option<AnswerClaim>> {
        self.run(move |store| store.answer_claim(&session_uid, &request_id))
            .await
    }

    pub async fn release_answer_claim(
        &self,
        session_uid: String,
        request_id: String,
    ) -> Result<()> {
        self.run(move |store| store.release_answer_claim(&session_uid, &request_id))
            .await
    }

    pub async fn delete_pending_approval(
        &self,
        session_uid: String,
        request_id: String,
    ) -> Result<()> {
        self.run(move |store| store.delete_pending_approval(&session_uid, &request_id))
            .await
    }

    pub async fn claim_text_mutation(
        &self,
        session_uid: String,
        request_id: String,
        payload_hash: String,
        now: String,
    ) -> Result<TextClaim> {
        self.run(move |store| {
            store.claim_text_mutation(&session_uid, &request_id, &payload_hash, &now)
        })
        .await
    }

    pub async fn settle_text_mutation(
        &self,
        session_uid: String,
        request_id: String,
        matched: String,
        settled_at: String,
    ) -> Result<()> {
        self.run(move |store| {
            store.settle_text_mutation(&session_uid, &request_id, &matched, &settled_at)
        })
        .await
    }

    pub async fn release_text_mutation(
        &self,
        session_uid: String,
        request_id: String,
    ) -> Result<()> {
        self.run(move |store| store.release_text_mutation(&session_uid, &request_id))
            .await
    }

    pub async fn create_pairing_code(
        &self,
        code_hash: String,
        expires_at: String,
        expires_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        self.run(move |store| {
            store.create_pairing_code(&code_hash, &expires_at, expires_ms, now_ms)
        })
        .await
    }

    pub async fn insert_device(
        &self,
        device_id: String,
        name: String,
        token_hash: String,
        at: String,
    ) -> Result<()> {
        self.run(move |store| store.insert_device(&device_id, &name, &token_hash, &at))
            .await
    }

    pub async fn unique_device_name(&self, requested: String) -> Result<String> {
        self.run(move |store| store.unique_device_name(&requested))
            .await
    }

    /// Returns the device rows the token was taken from — see the store.
    ///
    /// The credential is owned rather than borrowed, like every other argument
    /// here, and travels with the token in one call: there is no wrapper that
    /// registers a token now and a credential afterwards, because a window
    /// between the two is a window in which the row names one and not the other.
    pub async fn set_push_token(
        &self,
        device_id: String,
        token: String,
        environment: String,
        credential: Option<protocol::secret::Redacted>,
    ) -> Result<Vec<String>> {
        self.run(move |store| {
            // Exposed only here, at the store's SQL bind — the one boundary a
            // bearer must cross to be persisted. It arrived wrapped and is
            // wrapped again the instant it is read back.
            store.set_push_token(
                &device_id,
                &token,
                &environment,
                credential.as_ref().map(protocol::secret::Redacted::expose),
            )
        })
        .await
    }

    pub async fn touch_device(&self, device_id: String, at: String) -> Result<()> {
        self.run(move |store| store.touch_device(&device_id, &at))
            .await
    }

    pub async fn set_device_features(
        &self,
        device_id: String,
        features_json: Option<String>,
        epoch: String,
    ) -> Result<()> {
        self.run(move |store| {
            store.set_device_features(&device_id, features_json.as_deref(), &epoch)
        })
        .await
    }

    pub async fn invalidate_device_features(&self, current_epoch: String) -> Result<u64> {
        self.run(move |store| store.invalidate_device_features(&current_epoch))
            .await
    }

    pub async fn revoke_device(&self, device_id: String, at: String) -> Result<bool> {
        self.run(move |store| store.revoke_device(&device_id, &at))
            .await
    }

    pub async fn device_summaries(&self) -> Result<Vec<DeviceSummary>> {
        self.run(|store| {
            Ok(store
                .list_devices()?
                .iter()
                .map(DeviceRow::to_summary)
                .collect())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::event::{SessionKey, Source};

    fn temp_db() -> Db {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-db-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        Db::new(Arc::new(Store::open(&path).unwrap()))
    }

    fn seed(db: &Db) -> SessionKey {
        let key = SessionKey::new("01K1B3XQ8ZC0DE5FGH7JKMNPQR", "cc-1");
        let now = protocol::time::now_rfc3339();
        db.blocking()
            .upsert_session(&SessionRow {
                session_uid: key.uid.clone(),
                session_id: key.name.clone(),
                tmux_session: key.name.clone(),
                tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                cwd: "/tmp".into(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: Lifecycle::Live,
                created_at: now.clone(),
                updated_at: now,
                agent: protocol::agent::AgentKind::Claude,
                codex_thread_id: None,
                codex_socket: None,
            })
            .unwrap()
            .assert_present();
        key
    }

    #[tokio::test]
    async fn the_sequence_is_still_gap_free_under_concurrent_appends() {
        // The property the split must not cost. `seq` is allocated inside one
        // `BEGIN IMMEDIATE` against a single writer connection; moving the call
        // onto a blocking pool changes which thread waits, never the order in
        // which the transaction commits. If a reader connection could ever take
        // a write, this is the test that would find it.
        let db = temp_db();
        let key = seed(&db);

        let mut tasks = Vec::new();
        for i in 0..64u32 {
            let db = db.clone();
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                db.append_event(
                    PendingEvent::new(
                        &key,
                        EventKind::ToolCall,
                        serde_json::json!({ "i": i }),
                        Source::Hook,
                    )
                    .with_source_event_id(format!("e{i}")),
                )
                .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().unwrap().is_some());
        }

        let max = db.max_seq(key.uid.clone()).await.unwrap();
        let count = db.blocking().count_events(&key.uid).unwrap();
        assert_eq!(max, 64);
        assert_eq!(
            max, count,
            "max_seq must equal the event count, or the log has a hole or a burnt seq"
        );
        let events = db.events_after(key.uid.clone(), 0, 100).await.unwrap();
        let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, (1..=64).collect::<Vec<u64>>());
    }

    #[tokio::test]
    async fn a_duplicate_still_consumes_no_sequence() {
        let db = temp_db();
        let key = seed(&db);
        let pending = PendingEvent::new(
            &key,
            EventKind::ToolCall,
            serde_json::json!({}),
            Source::Hook,
        )
        .with_source_event_id("same");

        assert!(db.append_event(pending.clone()).await.unwrap().is_some());
        assert!(
            db.append_event(pending).await.unwrap().is_none(),
            "a fact that arrives twice must consume one seq"
        );
        assert_eq!(db.max_seq(key.uid).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn reads_are_served_while_a_write_is_in_flight() {
        // What the reader pool buys, stated as a test: a read must not be
        // serialised behind the writer's mutex. Before the split there was one
        // `Mutex<Connection>`, so a 4MB transcript batch stopped every replay,
        // every session listing and every revocation check on the machine for
        // its whole duration.
        let db = temp_db();
        let key = seed(&db);
        // Enough rows that the batch is not instantaneous.
        let batch: Vec<PendingEvent> = (0..5_000)
            .map(|i| {
                PendingEvent::new(
                    &key,
                    EventKind::ToolCall,
                    serde_json::json!({ "i": i }),
                    Source::Hook,
                )
                .with_source_event_id(format!("b{i}"))
            })
            .collect();
        let cursor = TailCursor {
            path: "/tmp/t.jsonl".into(),
            dev: 1,
            ino: 1,
            offset: 1,
            last_line_start: 0,
            last_line_sha: "x".into(),
        };

        let writing = {
            let db = db.clone();
            let uid = key.uid.clone();
            tokio::spawn(async move { db.append_batch_with_cursor(uid, batch, cursor).await })
        };
        // Concurrent reads. The assertion is that they *complete* — with one
        // shared mutex they would simply queue behind the batch, and with a
        // reader that could not be obtained they would deadlock.
        for _ in 0..16 {
            db.list_sessions().await.unwrap();
            db.device_is_active("nobody".into()).await.unwrap();
        }
        assert_eq!(writing.await.unwrap().unwrap().len(), 5_000);
        assert_eq!(db.max_seq(key.uid).await.unwrap(), 5_000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_long_write_does_not_stall_the_runtime() {
        // The defect, made deterministic. On a single-threaded runtime there is
        // exactly *one* worker, so a synchronous store call from an `async fn`
        // owns it for the whole commit: timers do not fire, sockets are not
        // polled, nothing else runs. On the real multi-threaded runtime the same
        // thing happens to one worker per concurrent call, and with a 5-second
        // `busy_timeout` the worst case is a five-second dead daemon.
        //
        // Routing through `spawn_blocking` moves the waiting to a pool thread.
        // The assertion is that a 5ms sleep scheduled *during* a much longer
        // commit still completes on time — which is only possible if the worker
        // was free the whole while.
        use std::time::{Duration, Instant};

        let db = temp_db();
        let key = seed(&db);
        let batch: Vec<PendingEvent> = (0..20_000)
            .map(|i| {
                PendingEvent::new(
                    &key,
                    EventKind::ToolCall,
                    serde_json::json!({ "i": i, "pad": "----------------------------------------" }),
                    Source::Hook,
                )
                .with_source_event_id(format!("slow{i}"))
            })
            .collect();
        let cursor = TailCursor {
            path: "/tmp/t.jsonl".into(),
            dev: 1,
            ino: 1,
            offset: 1,
            last_line_start: 0,
            last_line_sha: "x".into(),
        };

        // Ordering is the whole experiment, so it is spelled out. `tokio::spawn`
        // on a current-thread runtime only *queues* the task; it runs when this
        // one next yields. The `sleep` below is that yield, so the write and the
        // timer are handed to the scheduler together — and whether the timer
        // fires on time is exactly the question. Measuring after a
        // `yield_now()` would prove nothing, because the yield would not return
        // until the blocking write had already finished.
        let started = Instant::now();
        let writing = {
            let db = db.clone();
            let uid = key.uid.clone();
            tokio::spawn(async move { db.append_batch_with_cursor(uid, batch, cursor).await })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        let tick_latency = started.elapsed();

        assert_eq!(writing.await.unwrap().unwrap().len(), 20_000);
        let write_time = started.elapsed();

        // If the fixture is not slow enough the comparison proves nothing, so
        // say so rather than passing vacuously.
        assert!(
            write_time > Duration::from_millis(50),
            "the fixture commit finished in {write_time:?}; too fast to demonstrate anything"
        );
        assert!(
            tick_latency < write_time / 2,
            "a 5ms timer took {tick_latency:?} while a {write_time:?} commit ran; the runtime \
             was blocked by the database"
        );
    }

    #[tokio::test]
    async fn a_store_error_surfaces_as_an_error_rather_than_a_panic() {
        let db = temp_db();
        db.blocking().break_device_lookups_for_tests();
        assert!(db.device_is_active("abcd1234".into()).await.is_err());
    }
}
