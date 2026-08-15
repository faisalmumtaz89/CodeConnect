//! The relay's state: which installations exist, which tokens they own, and
//! nothing that could be used to reach one.
//!
//! **No column here holds a raw device token or a raw bearer.** Both arrive on
//! every request and both are hashed before anything is written, so a copy of
//! this file — a backup, a disk snapshot, a stolen volume — is a record of who
//! exists and not a means of pushing to them.
//!
//! Migrations are an ordered list applied inside one transaction, and the list
//! is append-only. Editing an entry that has already run changes the schema of
//! a fresh database and not of the deployed one, which is the failure that ends
//! with two production shapes and one set of queries.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// Every schema change this relay has ever made, in order.
///
/// The index of an entry is its version, so entries are appended and never
/// rewritten or reordered.
const MIGRATIONS: &[&str] = &[
    // One attested app installation: the App Attest public key it proved, the
    // receipt Apple issued for it, and the assertion counter that makes a
    // replayed assertion visible.
    //
    // `key_id_hash` rather than the key id itself: the id is not secret, but it
    // is a stable per-installation identifier and there is no query that needs
    // to read it back — only to recognise it.
    //
    // `counter_trusted` because a restored backup's counters describe a moment
    // that has passed. A restore marks them untrusted rather than deleting the
    // installation, so the next successful assertion re-establishes the value
    // instead of forcing every phone through a fresh attestation.
    "CREATE TABLE installations (
         id                 INTEGER PRIMARY KEY,
         key_id_hash        TEXT NOT NULL UNIQUE,
         public_key         BLOB NOT NULL,
         receipt            BLOB,
         attest_environment TEXT NOT NULL,
         counter            INTEGER NOT NULL,
         counter_trusted    INTEGER NOT NULL,
         bundle_version     TEXT,
         validation_category INTEGER,
         created_ms         INTEGER NOT NULL,
         updated_ms         INTEGER NOT NULL
     )",
    // One `(token, environment)` a bearer may push to.
    //
    // `generation` is what a database restore invalidates: every bearer records
    // the generation it was minted under, and raising the floor held outside
    // this file refuses all of them at once. Without it, restoring a snapshot
    // taken before a revocation brings the revoked bearer back to life.
    "CREATE TABLE bindings (
         id              INTEGER PRIMARY KEY,
         installation_id INTEGER NOT NULL REFERENCES installations(id),
         token_hash      TEXT NOT NULL,
         environment     TEXT NOT NULL,
         bearer_hash     TEXT NOT NULL UNIQUE,
         generation      INTEGER NOT NULL,
         status          TEXT NOT NULL,
         terminal_reason TEXT,
         created_ms      INTEGER NOT NULL,
         updated_ms      INTEGER NOT NULL
     )",
    // **At most one live bearer per token, enforced by the database.**
    //
    // Enrollment replacing a credential is two statements — revoke the old row,
    // insert the new one — and a partial index is what makes the pair atomic in
    // the sense that matters: an interleaving that left both active cannot
    // commit. Checked in application code instead, two concurrent enrollments
    // for one phone would both read "none active" and both insert.
    //
    // Partial, so the revoked rows the retention policy keeps for thirty days
    // do not collide with anything.
    "CREATE UNIQUE INDEX bindings_one_active_per_token
         ON bindings (token_hash) WHERE status = 'active'",
    // Outstanding enrollment challenges. The hash rather than the challenge for
    // the same reason as everything else here: a leaked table must not let
    // anyone answer a challenge they were not issued.
    "CREATE TABLE challenges (
         id             INTEGER PRIMARY KEY,
         challenge_hash TEXT NOT NULL UNIQUE,
         issued_ms      INTEGER NOT NULL,
         expires_ms     INTEGER NOT NULL,
         consumed_ms    INTEGER
     )",
    // Abuse counters, keyed by an opaque string the caller derives. Nothing
    // here is joinable to a binding or an address by anyone reading the file.
    "CREATE TABLE rate_buckets (
         bucket_key    TEXT PRIMARY KEY,
         day_start_ms  INTEGER NOT NULL,
         day_count     INTEGER NOT NULL
     )",
    // The three lookups that are not already an index.
    //
    // A lifecycle operation retires everything one installation still holds, a
    // challenge is issued far more often than it is spent and the expired ones
    // are swept on that path, and every rotated address bucket has to be found
    // by the day it belonged to. Each of those is a full scan otherwise, and the
    // last two are scans of the tables that grow fastest.
    "CREATE INDEX bindings_by_installation ON bindings (installation_id);
     CREATE INDEX challenges_by_expiry ON challenges (expires_ms);
     CREATE INDEX rate_buckets_by_day ON rate_buckets (day_start_ms)",
];

/// How long a revoked binding is kept before it is deleted.
///
/// Thirty days, which is the retention the privacy statement publishes. What
/// the row is worth for those thirty days is an answer to "why did this phone
/// stop receiving pushes"; after them it is a record of who existed and nothing
/// anyone needs.
pub const REVOKED_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;

/// What one sweep removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub bindings: usize,
    pub installations: usize,
}

/// Delete every terminal record whose thirty days are up.
///
/// Called where a credential is issued rather than on a timer, for the reason
/// `challenge::prune` is: a retention promise kept by a background task is a
/// promise about whether the task ran, and this one is about the file.
///
/// Measured from `updated_ms`, which is when the row became terminal — a
/// binding revoked today is kept for thirty days from today and not from the
/// day it was created.
///
/// **An installation outlives its last binding by the same thirty days and no
/// longer.** Nothing in the relay references an installation that holds no
/// binding: its public key can authorise nothing, its counter measures nothing,
/// and the retention schedule promises the App Attest record is kept until
/// lifecycle termination rather than forever. The timestamp is part of the rule
/// and not merely of the binding's, because an enrollment writes the
/// installation row before the binding it is about to issue — an orphan test
/// without one would delete the row the very statement after it was inserted.
pub fn prune_terminal_records(conn: &Connection, now_ms: i64) -> rusqlite::Result<Pruned> {
    let cutoff = now_ms - REVOKED_RETENTION_MS;
    let bindings = conn.execute(
        "DELETE FROM bindings WHERE status <> 'active' AND updated_ms <= ?1",
        [cutoff],
    )?;
    let installations = conn.execute(
        "DELETE FROM installations
         WHERE updated_ms <= ?1
           AND NOT EXISTS (SELECT 1 FROM bindings WHERE bindings.installation_id = installations.id)",
        [cutoff],
    )?;
    Ok(Pruned {
        bindings,
        installations,
    })
}

/// How long a statement waits for a writer before giving up.
///
/// SQLite's default is zero: a second connection meeting a held write lock
/// returns `SQLITE_BUSY` immediately rather than waiting the few milliseconds
/// the writer needs. Under WAL that is the difference between a checkpoint
/// being invisible and a checkpoint being an error the caller sees.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Open the database at `path`, applying any migrations it has not seen.
pub fn open(path: &std::path::Path) -> Result<Connection> {
    let mut conn = Connection::open(path)
        .with_context(|| format!("opening the relay database at {}", path.display()))?;
    prepare(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// The same database with no file behind it.
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory().context("opening an in-memory relay database")?;
    prepare(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn prepare(conn: &Connection) -> Result<()> {
    // WAL so a reader never blocks the writer. The pragma answers with the
    // mode it settled on, which is why it is a query rather than an execute —
    // and an in-memory database answers `memory`, which is correct for it.
    conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
        row.get::<_, String>(0)
    })
    .context("setting the journal mode")?;
    // Off by default in SQLite, so `bindings.installation_id` would reference
    // nothing and a deleted installation would leave bindings pointing at a row
    // that is gone.
    conn.execute_batch("PRAGMA foreign_keys = ON")
        .context("enabling foreign keys")?;
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))
        .context("setting the busy timeout")?;
    Ok(())
}

/// The schema version this database is at: the number of migrations applied.
pub fn schema_version(conn: &Connection) -> Result<u32> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
             id      INTEGER PRIMARY KEY CHECK (id = 1),
             version INTEGER NOT NULL
         )",
    )
    .context("creating the schema version table")?;
    let version: Option<u32> = conn
        .query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()
        .context("reading the schema version")?;
    Ok(version.unwrap_or(0))
}

/// Apply every migration this database has not seen, in one transaction.
///
/// One transaction for the whole run rather than one each: a process killed
/// halfway through a multi-statement upgrade would otherwise leave a schema
/// that is at no version at all, and the next start would try to apply a
/// migration to a table it had already created.
fn migrate(conn: &mut Connection) -> Result<()> {
    let applied = schema_version(conn)? as usize;
    if applied >= MIGRATIONS.len() {
        return Ok(());
    }
    let tx = conn.transaction().context("beginning the migration")?;
    for (index, statement) in MIGRATIONS.iter().enumerate().skip(applied) {
        tx.execute_batch(statement)
            .with_context(|| format!("applying schema migration {}", index + 1))?;
    }
    tx.execute(
        "INSERT INTO schema_version (id, version) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET version = excluded.version",
        [MIGRATIONS.len() as i64],
    )
    .context("recording the schema version")?;
    tx.commit().context("committing the migration")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installation(conn: &Connection) -> i64 {
        keyed_installation(conn, "0f0e0d0c")
    }

    fn keyed_installation(conn: &Connection, key_id_hash: &str) -> i64 {
        conn.execute(
            "INSERT INTO installations
                (key_id_hash, public_key, receipt, attest_environment, counter,
                 counter_trusted, bundle_version, validation_category, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                key_id_hash,
                vec![1u8, 2, 3],
                Option::<Vec<u8>>::None,
                "development",
                0i64,
                1i64,
                "1.2.3",
                0i64,
                1_700_000_000_000i64,
                1_700_000_000_000i64,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn bind(
        conn: &Connection,
        installation_id: i64,
        token_hash: &str,
        bearer_hash: &str,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO bindings
                (installation_id, token_hash, environment, bearer_hash, generation,
                 status, terminal_reason, created_ms, updated_ms)
             VALUES (?1, ?2, 'production', ?3, 1, 'active', NULL, ?4, ?4)",
            rusqlite::params![
                installation_id,
                token_hash,
                bearer_hash,
                1_700_000_000_000i64
            ],
        )
    }

    #[test]
    fn a_fresh_database_reaches_the_current_version_and_stays_there() {
        let conn = open_in_memory().unwrap();
        assert_eq!(schema_version(&conn).unwrap() as usize, MIGRATIONS.len());

        // Re-running is what every restart does.
        let mut conn = conn;
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap() as usize, MIGRATIONS.len());

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in [
            "bindings",
            "challenges",
            "installations",
            "rate_buckets",
            "schema_version",
        ] {
            assert!(tables.contains(&expected.to_string()), "{tables:?}");
        }
    }

    /// **Two live bearers for one phone is the failure this index exists for.**
    /// The second one would keep working after the first was revoked.
    #[test]
    fn one_token_cannot_hold_two_active_bindings_but_may_be_rebound_after_a_revocation() {
        let conn = open_in_memory().unwrap();
        let id = installation(&conn);

        bind(&conn, id, "token-hash", "bearer-one").expect("the first binding");
        let clash = bind(&conn, id, "token-hash", "bearer-two")
            .expect_err("a second active binding for the same token");
        assert!(clash.to_string().contains("UNIQUE"), "{clash}");

        conn.execute(
            "UPDATE bindings SET status = 'revoked', terminal_reason = 'rotated' WHERE bearer_hash = 'bearer-one'",
            [],
        )
        .unwrap();
        bind(&conn, id, "token-hash", "bearer-two").expect("a replacement after revocation");

        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bindings WHERE token_hash = 'token-hash' AND status = 'active'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 1);
    }

    #[test]
    fn an_installation_and_its_binding_survive_a_round_trip() {
        let conn = open_in_memory().unwrap();
        let id = installation(&conn);
        bind(&conn, id, "token-hash", "bearer-one").unwrap();

        let (key_id_hash, environment, counter, trusted): (String, String, i64, i64) = conn
            .query_row(
                "SELECT key_id_hash, attest_environment, counter, counter_trusted
                 FROM installations WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(key_id_hash, "0f0e0d0c");
        assert_eq!(environment, "development");
        assert_eq!(counter, 0);
        assert_eq!(trusted, 1);

        let (token_hash, status, generation, owner): (String, String, i64, i64) = conn
            .query_row(
                "SELECT token_hash, status, generation, installation_id
                 FROM bindings WHERE bearer_hash = 'bearer-one'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(token_hash, "token-hash");
        assert_eq!(status, "active");
        assert_eq!(generation, 1);
        assert_eq!(owner, id);
    }

    /// **The retention promise is about the file**, so it is asserted about the
    /// file: a binding revoked yesterday is still there and one revoked a month
    /// ago is not — and the installation nothing references any longer goes with
    /// it, because a public key and a receipt nobody can authorise anything with
    /// are exactly what the schedule promises not to keep.
    #[test]
    fn a_revoked_binding_lives_thirty_days_and_takes_its_last_installation_with_it() {
        let conn = open_in_memory().unwrap();
        let id = installation(&conn);
        let lone = keyed_installation(&conn, "0a0b0c0d");
        let revoked_ms = 1_700_000_000_000i64;

        bind(&conn, id, "token-one", "bearer-one").unwrap();
        bind(&conn, id, "token-two", "bearer-two").unwrap();
        bind(&conn, lone, "token-three", "bearer-three").unwrap();
        conn.execute(
            "UPDATE bindings SET status = 'revoked', terminal_reason = 'rotated', updated_ms = ?1
             WHERE bearer_hash IN ('bearer-one', 'bearer-three')",
            [revoked_ms],
        )
        .unwrap();

        let survivors = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT bearer_hash FROM bindings ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let installations = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT key_id_hash FROM installations ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        assert_eq!(
            prune_terminal_records(&conn, revoked_ms + REVOKED_RETENTION_MS - 1).unwrap(),
            Pruned {
                bindings: 0,
                installations: 0
            }
        );
        assert_eq!(
            survivors(&conn),
            ["bearer-one", "bearer-two", "bearer-three"]
        );
        assert_eq!(installations(&conn), ["0f0e0d0c", "0a0b0c0d"]);

        assert_eq!(
            prune_terminal_records(&conn, revoked_ms + REVOKED_RETENTION_MS).unwrap(),
            Pruned {
                bindings: 2,
                installations: 1
            }
        );
        // The active row is untouched however long it has been there: it is
        // retained until an explicit lifecycle termination and not before — and
        // so is the installation that still holds it.
        assert_eq!(survivors(&conn), ["bearer-two"]);
        assert_eq!(installations(&conn), ["0f0e0d0c"]);
        assert_eq!(
            prune_terminal_records(&conn, revoked_ms + REVOKED_RETENTION_MS * 12).unwrap(),
            Pruned {
                bindings: 0,
                installations: 0
            }
        );
        assert_eq!(survivors(&conn), ["bearer-two"]);
        assert_eq!(installations(&conn), ["0f0e0d0c"]);
    }

    /// **An installation is never deleted out from under a binding it still
    /// holds**, and never within its thirty days — the sweep runs where an
    /// enrollment writes the installation row a statement before its first
    /// binding, and a rule without the timestamp would delete that row.
    #[test]
    fn an_installation_written_this_moment_survives_a_sweep_that_finds_no_binding() {
        let conn = open_in_memory().unwrap();
        let now = 1_700_000_000_000i64;
        let id = keyed_installation(&conn, "0f0e0d0c");
        conn.execute("UPDATE installations SET updated_ms = ?1", [now])
            .unwrap();

        assert_eq!(
            prune_terminal_records(&conn, now).unwrap(),
            Pruned {
                bindings: 0,
                installations: 0
            }
        );
        bind(&conn, id, "token-one", "bearer-one").expect("the row it was written for");
    }

    /// A binding for an installation that does not exist is a row nothing can
    /// ever authorise, and it is refused rather than written.
    #[test]
    fn a_binding_cannot_point_at_an_installation_that_is_not_there() {
        let conn = open_in_memory().unwrap();
        let orphan = bind(&conn, 4242, "token-hash", "bearer-one")
            .expect_err("a foreign key that references nothing");
        assert!(orphan.to_string().contains("FOREIGN KEY"), "{orphan}");
    }

    /// The file on disk is the thing an attacker would read, so the assertion
    /// is about its bytes rather than about the code that wrote them.
    #[test]
    fn no_column_can_hold_a_raw_token_or_a_raw_bearer() {
        let conn = open_in_memory().unwrap();
        let columns: Vec<String> = conn
            .prepare(
                "SELECT name FROM pragma_table_info('bindings')
                 UNION ALL SELECT name FROM pragma_table_info('installations')
                 UNION ALL SELECT name FROM pragma_table_info('challenges')",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for column in ["token", "bearer", "credential", "challenge"] {
            assert!(
                !columns.iter().any(|name| name == column),
                "{column:?} would be a raw value; the schema stores hashes"
            );
        }
        for column in ["token_hash", "bearer_hash", "challenge_hash", "key_id_hash"] {
            assert!(columns.iter().any(|name| name == column), "{columns:?}");
        }
    }
}
