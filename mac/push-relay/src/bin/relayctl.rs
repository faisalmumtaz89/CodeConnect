//! `relayctl seed-sandbox-binding` — a credential for a development phone,
//! without an attestation.
//!
//! **Why this is allowed to exist.** The whole path — daemon to relay to APNs
//! to a physical phone — has to be provable before the iOS App Attest flow
//! exists. Until it does, no device can enrol, so a binding has to come from
//! somewhere, and it comes from a person at a shell with the database in front
//! of them.
//!
//! **Why it cannot be turned on in production.** Decision 1 is precise: local
//! only, absent from the production image, and impossible to enable through a
//! network request. Three things make that true, and each is enough alone.
//!
//! - It is a `[[bin]]` with `required-features = ["seed"]`, and the feature is
//!   off by default. The image's build command is `cargo build -p push-relay`,
//!   which never compiles this file — there is no binary in the container to
//!   run, to disable, or to find.
//! - It is a binary that opens a database file. It has no HTTP route and
//!   nothing in the router refers to it, so no request reaches it however the
//!   service is configured.
//! - **It refuses a database that holds a production record.** The refusal is a
//!   fact about the file it was pointed at and not about the shell it was run
//!   from: an environment variable is unset for anybody who has just logged in,
//!   and a guard whose safe state is the one nobody has to set is a guard that
//!   is off. `RELAY_ATTEST_ENVIRONMENT` is still consulted, because reading a
//!   variable is cheaper than opening a database, but the database is what
//!   decides.
//!
//! Neither is the environment word `sandbox` on the binding it writes a
//! mitigation on its own: `push::correction` moves a sandbox binding to
//! production the first time Apple answers `BadDeviceToken`, so a seeded row is
//! one refusal away from being a production one.
//!
//! The credential it writes is an ordinary one — same table, same generation
//! floor, same rate bucket, same revocation path — so nothing downstream has a
//! seeded case to handle.

use anyhow::{bail, Context, Result};
use push_relay::config::{AttestEnvironment, RelayConfig};
use push_relay::db;
use push_relay::secret::{bearer_hash, new_bearer, normalize_device_token, sha256_hex, token_hash};
use rusqlite::{Connection, TransactionBehavior};

const USAGE: &str = "\
relayctl seed-sandbox-binding --token <apns device token> [--db <path>]

Writes a sandbox binding for a development phone and prints its bearer once.
The database is RELAY_DB_PATH unless --db says otherwise.";

/// The environment a seeded binding is always on.
///
/// Sandbox is the point: a development build's token is valid only there, and a
/// tool that could write a production binding would be a tool that could mint a
/// credential for somebody's real phone.
const ENVIRONMENT: &str = "sandbox";

/// What a seeded installation's `key_id_hash` is derived from, so the rows this
/// tool wrote are identifiable in a database an operator is inspecting — and so
/// two runs for one token update one installation rather than accumulating.
const SEED_CONTEXT: &str = "relayctl-seed/1/";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("seed-sandbox-binding") => seed(&args.collect::<Vec<_>>()),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn seed(args: &[String]) -> Result<()> {
    let mut token = None;
    let mut db_path = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        match flag {
            "--token" => token = Some(value.clone()),
            "--db" => db_path = Some(std::path::PathBuf::from(value)),
            other => bail!("{other:?} is not an option\n\n{USAGE}"),
        }
        index += 2;
    }

    let token = token.with_context(|| format!("--token is required\n\n{USAGE}"))?;
    let token = normalize_device_token(&token).context("the device token")?;

    let config = RelayConfig::from_env()?;
    // The cheaper of the two refusals, and the weaker one: it is true only when
    // somebody set the variable, so it stops a mistake and not a database.
    if config.attest_environment == AttestEnvironment::Production {
        bail!(
            "RELAY_ATTEST_ENVIRONMENT is production; this tool writes bindings nobody attested \
             for and refuses to touch a production namespace"
        );
    }
    let path = db_path.unwrap_or(config.db_path);
    let mut conn = db::open(&path)?;

    let bearer = new_bearer()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_millis() as i64;
    write(&mut conn, &token, &bearer_hash(&bearer), now)?;

    // The bearer goes to stdout on its own line and everything else to stderr,
    // so a person can pipe it into the daemon's configuration without also
    // piping a sentence into it. It is printed once and never written down: the
    // relay keeps only its hash, and a second copy would have to be a second
    // seeding.
    eprintln!(
        "seeded a {ENVIRONMENT} binding in {} for a token ending {}",
        path.display(),
        &token[token.len() - 6..]
    );
    eprintln!("the bearer is printed once and is not recoverable from the database:");
    println!("{}", bearer.expose());
    Ok(())
}

/// One installation and one active binding, in a transaction.
///
/// **The previous credential for this token dies in the same transaction**, the
/// way enrollment does it — `bindings_one_active_per_token` is what makes that
/// atomic, and a seeded row that skipped the revocation would leave two live
/// bearers for one phone, which is the state the index exists to prevent.
fn write(conn: &mut Connection, token: &str, bearer_hash: &str, now: i64) -> Result<()> {
    let token_hash = token_hash(token);
    let key_id_hash = sha256_hex(format!("{SEED_CONTEXT}{token_hash}").as_bytes());
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("beginning the seed")?;

    refuse_a_production_database(&tx)?;

    // **An installation that can never authorise anything.** No public key, so
    // `verify_assertion` cannot succeed against it; an untrusted counter, so it
    // claims nothing about replay. A seeded phone rebinds or rotates by being
    // seeded again, which is a person at a shell, and that is the intent.
    tx.execute(
        "INSERT INTO installations
            (key_id_hash, public_key, receipt, attest_environment, counter, counter_trusted,
             bundle_version, validation_category, created_ms, updated_ms)
         VALUES (?1, X'', NULL, 'development', 0, 0, NULL, NULL, ?2, ?2)
         ON CONFLICT (key_id_hash) DO UPDATE SET updated_ms = excluded.updated_ms",
        rusqlite::params![key_id_hash, now],
    )
    .context("recording the seeded installation")?;
    let installation_id: i64 = tx
        .query_row(
            "SELECT id FROM installations WHERE key_id_hash = ?1",
            [&key_id_hash],
            |row| row.get(0),
        )
        .context("reading the seeded installation")?;

    tx.execute(
        "UPDATE bindings SET status = 'revoked', terminal_reason = 'superseded', updated_ms = ?2
         WHERE token_hash = ?1 AND status = 'active'",
        rusqlite::params![token_hash, now],
    )
    .context("revoking the previous credential for this token")?;

    // Generation 0, which is the floor a relay with no incident behind it
    // reports — so a seeded credential is refused by a floor bump exactly like
    // an attested one.
    tx.execute(
        "INSERT INTO bindings
            (installation_id, token_hash, environment, bearer_hash, generation,
             status, terminal_reason, created_ms, updated_ms)
         VALUES (?1, ?2, ?3, ?4, 0, 'active', NULL, ?5, ?5)",
        rusqlite::params![installation_id, token_hash, ENVIRONMENT, bearer_hash, now],
    )
    .context("writing the seeded binding")?;

    tx.commit().context("committing the seed")
}

/// Stop if this file belongs to phones that attested for real.
///
/// **Inside the write transaction and before any statement that changes
/// anything**, so the revocation this tool performs — `UPDATE bindings ... WHERE
/// token_hash = ? AND status = 'active'`, which retires whatever credential that
/// phone is currently using — cannot happen and then be regretted. `Immediate`
/// means the read below is under the write lock, so nothing enrols between the
/// question and the answer.
///
/// Two questions, because a production database can be either shape: an
/// installation attested against Apple's production namespace, or a binding
/// addressed at Apple's production host. Neither is something a development
/// database has, and either is enough to make the file somebody's real phone.
fn refuse_a_production_database(tx: &Connection) -> Result<()> {
    let production: i64 = tx
        .query_row(
            "SELECT (SELECT COUNT(*) FROM installations WHERE attest_environment = 'production')
                  + (SELECT COUNT(*) FROM bindings WHERE environment = 'production')",
            [],
            |row| row.get(0),
        )
        .context("looking for production records")?;
    if production > 0 {
        bail!(
            "this database holds {production} production record(s); this tool writes bindings \
             nobody attested for and refuses to touch a database real phones are enrolled in"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const NOW: i64 = 1_700_000_000_000;

    /// A directory under the OS temp dir. The process id is in the name because
    /// two `cargo test` invocations can overlap on one machine, and a fixed path
    /// means one of them deleting the other's fixture halfway through.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("push-relay-relayctl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn installation(conn: &Connection, key_id_hash: &str, attest_environment: &str) -> i64 {
        conn.execute(
            "INSERT INTO installations
                (key_id_hash, public_key, receipt, attest_environment, counter, counter_trusted,
                 bundle_version, validation_category, created_ms, updated_ms)
             VALUES (?1, X'0102', NULL, ?2, 0, 1, '1.0', 1, ?3, ?3)",
            rusqlite::params![key_id_hash, attest_environment, NOW],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn binding(conn: &Connection, installation_id: i64, environment: &str, bearer_hash: &str) {
        conn.execute(
            "INSERT INTO bindings
                (installation_id, token_hash, environment, bearer_hash, generation,
                 status, terminal_reason, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, 0, 'active', NULL, ?5, ?5)",
            rusqlite::params![
                installation_id,
                token_hash(TOKEN),
                environment,
                bearer_hash,
                NOW
            ],
        )
        .unwrap();
    }

    fn active(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT bearer_hash FROM bindings WHERE status = 'active' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// **The guard is about the file and not about the shell.** A person who has
    /// just logged in has no `RELAY_ATTEST_ENVIRONMENT` set at all, so a
    /// database holding real phones has to be what stops this — before the
    /// revocation that would otherwise retire the credential one of them is
    /// using.
    #[test]
    fn a_database_holding_a_production_record_is_refused_before_anything_is_written() {
        let dir = scratch("production");

        for (name, attest_environment, environment) in [
            ("attested", "production", "sandbox"),
            ("bound", "development", "production"),
        ] {
            let path = dir.join(format!("{name}.sqlite"));
            let mut conn = db::open(&path).unwrap();
            let id = installation(&conn, "a-real-phone", attest_environment);
            binding(&conn, id, environment, "a-real-credential");

            let refused = write(&mut conn, TOKEN, "a-seeded-credential", NOW)
                .expect_err("a production database must be refused");
            let why = format!("{refused:#}");
            assert!(why.contains("production record"), "{why}");

            // And the refusal is a refusal to touch it: the phone's credential
            // is still the live one.
            assert_eq!(active(&conn), ["a-real-credential"], "{name}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The database the tool exists for still works: a clean file, and one that
    /// already holds nothing but sandbox development records.
    #[test]
    fn a_clean_or_sandbox_only_database_is_seeded() {
        let dir = scratch("sandbox");

        let path = dir.join("clean.sqlite");
        let mut conn = db::open(&path).unwrap();
        write(&mut conn, TOKEN, "a-seeded-credential", NOW).expect("a clean database");
        assert_eq!(active(&conn), ["a-seeded-credential"]);

        // Seeding again replaces the credential rather than leaving two live.
        write(&mut conn, TOKEN, "a-second-credential", NOW + 1).expect("a sandbox database");
        assert_eq!(active(&conn), ["a-second-credential"]);

        // And one seeded by hand for another phone, which is the ordinary
        // development state.
        let path = dir.join("sandbox.sqlite");
        let mut conn = db::open(&path).unwrap();
        let id = installation(&conn, "another-development-phone", "development");
        binding(&conn, id, "sandbox", "another-seeded-credential");
        conn.execute(
            "UPDATE bindings SET token_hash = 'another-token-hash' WHERE bearer_hash = ?1",
            ["another-seeded-credential"],
        )
        .unwrap();
        write(&mut conn, TOKEN, "a-seeded-credential", NOW).expect("a sandbox database");
        assert_eq!(
            active(&conn),
            ["another-seeded-credential", "a-seeded-credential"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
