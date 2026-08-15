//! The daily encrypted backup, and the one property that makes restoring it
//! safe.
//!
//! **SQLite's own online backup, not a disk snapshot.** The plan is explicit
//! that a platform snapshot is never the restore path: it copies a file that a
//! running process is in the middle of writing, and restoring one destructively
//! overwrites the disk. `rusqlite::backup::Backup` reads a consistent copy from
//! the live connection with no downtime, which is the only kind of copy worth
//! keeping.
//!
//! **A restore fails closed** (§7). The backup is a database that believes in
//! credentials the operator may have revoked since, and no query against it can
//! know that. What makes the restore safe is the generation floor — a number in
//! a file *outside* the database — which every bearer records and the relay
//! refuses below. Restoring resurrects a revoked bearer; raising the floor
//! kills every restored bearer at once and sends every phone through fresh
//! attestation. The drill in the tests below is that sequence end to end.
//!
//! **The nonce is fresh for every backup, and this is the footgun.** AES-GCM
//! under a repeated `(key, nonce)` pair is not weakened, it is broken: two
//! ciphertexts under one nonce leak the XOR of their plaintexts and hand an
//! attacker the authentication subkey, which forges any message. This key is
//! long-lived *by design* — it is mounted once and every night's backup uses it
//! — so a fixed or counter-derived nonce would repeat on the first restart or
//! rollback. Twelve random bytes come from the system CSPRNG on every run and
//! travel in front of the ciphertext, because a nonce is not secret and the
//! reader has to have it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::Connection;

use crate::api::{BackupState, Relay};
use crate::config::RelayConfig;

/// How often the job runs. Daily, as the plan states.
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// Bound into every ciphertext as additional data.
///
/// Not secrecy — it is public — but a ciphertext produced by some other system
/// that happened to share this key will not open here, and neither will one of
/// these against some other reader.
const CONTEXT: &[u8] = b"codeconnect-relay/backup/1";

/// The key is 32 bytes written as hex, so a mounted secret file is a line of
/// text an operator can generate, paste and compare without a binary editor.
const KEY_HEX_CHARS: usize = 64;

/// Pages copied per step of the online backup.
///
/// There is no pause between steps: the pause exists to yield to another
/// process writing the source, and this database has exactly one connection,
/// held by the caller for the length of the copy.
const PAGES_PER_STEP: i32 = 1_024;

const NAME_PREFIX: &str = "relay-";
const NAME_SUFFIX: &str = ".sqlite.enc";

/// Thirteen digits of milliseconds, which orders lexically and stays that way
/// until the year 2286 — so a listing sorts by name and a restore does not
/// depend on a file's mtime, which copying a backup would change.
const NAME_DIGITS: usize = 13;

/// Where sealed backups are kept.
///
/// **A trait with the target in configuration, not a hardcoded directory.** The
/// deployment writes to a mounted disk today and to object storage later; the
/// plan says thirty-day retention either way. Written as a trait, that later
/// sink is an added implementation and a changed environment variable. Written
/// as `std::fs` calls inside the job, it is a rewrite of the job.
pub trait BackupSink: Send + Sync {
    fn put(&self, name: &str, sealed: &[u8]) -> Result<()>;
    fn list(&self) -> Result<Vec<String>>;
    fn get(&self, name: &str) -> Result<Vec<u8>>;
    fn delete(&self, name: &str) -> Result<()>;
    /// What the startup log says this sink is, without naming a secret.
    fn describe(&self) -> String;
}

/// A directory on the instance's persistent disk.
struct Directory(PathBuf);

impl BackupSink for Directory {
    fn put(&self, name: &str, sealed: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.0)
            .with_context(|| format!("creating the backup directory {}", self.0.display()))?;
        // Written under a temporary name and renamed, so a process killed
        // mid-write leaves no half-file that a restore would read as a backup.
        let partial = self.0.join(format!("{name}.partial"));
        std::fs::write(&partial, sealed)
            .with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, self.0.join(name))
            .with_context(|| format!("publishing the backup {name}"))
    }

    fn list(&self) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.0) {
            Ok(entries) => entries,
            // A directory that does not exist yet holds no backups, which is
            // the correct answer on the first run rather than an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading the backup directory {}", self.0.display()))
            }
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| taken_at(name).is_some())
            .collect();
        names.sort();
        Ok(names)
    }

    fn get(&self, name: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(name)).with_context(|| format!("reading the backup {name}"))
    }

    fn delete(&self, name: &str) -> Result<()> {
        std::fs::remove_file(self.0.join(name))
            .with_context(|| format!("deleting the backup {name}"))
    }

    fn describe(&self) -> String {
        format!("directory {}", self.0.display())
    }
}

/// The sink a target names, or a refusal that says which schemes exist.
///
/// A scheme this build cannot serve is an error at startup rather than a
/// backup that silently never happens — which is a state nobody discovers
/// until the morning they need a restore.
pub fn sink(target: &str) -> Result<Box<dyn BackupSink>> {
    match target.split_once("://") {
        Some(("file", path)) if !path.is_empty() => Ok(Box::new(Directory(PathBuf::from(path)))),
        _ => {
            bail!("the backup target {target:?} is not supported; this build writes `file:///path`")
        }
    }
}

/// Everything one run needs, assembled once at startup.
pub struct Job {
    key: LessSafeKey,
    sink: Box<dyn BackupSink>,
    db_path: PathBuf,
    retention_days: u32,
}

/// What one run did, for the log line and for a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub name: String,
    pub sealed_bytes: usize,
    pub pruned: usize,
}

impl Job {
    pub fn from_config(config: &RelayConfig) -> Result<Job> {
        Ok(Job {
            key: read_key(&config.backup_key_file)?,
            sink: sink(&config.backup_target)?,
            db_path: config.db_path.clone(),
            retention_days: config.backup_retention_days,
        })
    }

    pub fn describe(&self) -> String {
        self.sink.describe()
    }

    /// Copy, seal, store, prune.
    ///
    /// Takes the live connection rather than opening its own: a second
    /// connection to a WAL database sees a consistent snapshot too, but it
    /// would be a second writer for SQLite to arbitrate and the relay's whole
    /// design is that there is one.
    /// **The prune runs whether or not the store worked.** The failure this job
    /// most has to survive is a full disk, and the store is what fails on one
    /// while the prune is what frees the space — so a `?` between them would
    /// make the job disable its own recovery on the night it is needed, and
    /// every night after. The store's error is still what the run reports,
    /// because it is the one an operator has to act on.
    pub fn run(&self, live: &Connection, now_ms: i64) -> Result<Completed> {
        let name = format!("{NAME_PREFIX}{now_ms:0NAME_DIGITS$}{NAME_SUFFIX}");
        let plain = self.snapshot(live)?;
        let sealed = seal(&self.key, plain)?;
        let stored = self.sink.put(&name, &sealed);
        let pruned = self.prune(now_ms);
        stored?;
        Ok(Completed {
            name,
            sealed_bytes: sealed.len(),
            pruned: pruned?,
        })
    }

    /// A consistent copy of the live database, as bytes.
    ///
    /// **The scratch file lives beside the database it copies.** The plaintext
    /// is on disk for the moment between the copy and the sealing, and the only
    /// directory whose permissions have already been decided for exactly this
    /// content is the one the database is in. A temp directory would put an
    /// unencrypted copy of every binding somewhere nobody reviewed.
    fn snapshot(&self, live: &Connection) -> Result<Vec<u8>> {
        let scratch = self.db_path.with_extension("backup-partial");
        let _ = std::fs::remove_file(&scratch);
        let copied = (|| -> Result<Vec<u8>> {
            let mut destination = Connection::open(&scratch)
                .with_context(|| format!("opening {}", scratch.display()))?;
            {
                let backup = rusqlite::backup::Backup::new(live, &mut destination)
                    .context("starting the online backup")?;
                backup
                    .run_to_completion(PAGES_PER_STEP, Duration::ZERO, None)
                    .context("running the online backup")?;
            }
            drop(destination);
            std::fs::read(&scratch).with_context(|| format!("reading {}", scratch.display()))
        })();
        // Removed whether or not the copy worked: a failed run must not leave
        // an unencrypted database lying next to the live one.
        let _ = std::fs::remove_file(&scratch);
        copied
    }

    /// Delete every backup older than the retention window.
    fn prune(&self, now_ms: i64) -> Result<usize> {
        let cutoff = now_ms - i64::from(self.retention_days) * DAY_MS;
        let mut pruned = 0;
        for name in self.sink.list()? {
            if taken_at(&name).is_some_and(|taken| taken < cutoff) {
                self.sink.delete(&name)?;
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    /// The most recent backup, decrypted — the first half of §7's restore.
    pub fn latest(&self) -> Result<Option<Vec<u8>>> {
        let Some(name) = self.sink.list()?.pop() else {
            return Ok(None);
        };
        let sealed = self.sink.get(&name)?;
        open_sealed(&self.key, &sealed).map(Some)
    }
}

/// Why backups cannot run, when they cannot.
///
/// **Absent or unreadable is a reported state, not a crash.** A relay that
/// refused to start because its backup key had not been mounted would turn a
/// recoverable gap into an outage of the push path, which is the one thing
/// customers see. It starts, `/readyz` says backups are off, and the startup
/// log says why in the words that name the path.
pub fn absence(config: &RelayConfig) -> Option<String> {
    Job::from_config(config).err().map(|e| format!("{e:#}"))
}

/// Run the job now, and then once a day for the life of the process.
///
/// The first run is immediate on purpose: an instance that has just been
/// restored or redeployed should have a backup of the state it is actually
/// serving, rather than of whatever it is serving twenty-four hours later.
pub fn spawn_daily(relay: &Relay) {
    let job = match Job::from_config(&relay.config) {
        Ok(job) => Arc::new(job),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "backups are disabled");
            return;
        }
    };
    tracing::info!(
        sink = job.describe(),
        retention_days = relay.config.backup_retention_days,
        "backups enabled"
    );
    let db = Arc::clone(&relay.db);
    let state = Arc::clone(&relay.backup);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        loop {
            ticker.tick().await;
            let job = Arc::clone(&job);
            let db = Arc::clone(&db);
            let now = crate::enroll::now_ms();
            // **`spawn_blocking`, because the copy is disk work.** Run on a
            // runtime worker it would stall every request the process is
            // serving for as long as the database takes to read.
            let done = tokio::task::spawn_blocking(move || {
                let live = db.lock().unwrap_or_else(|e| e.into_inner());
                job.run(&live, now)
            })
            .await;
            match done {
                Ok(outcome) => record(&state, now, outcome),
                Err(e) => {
                    state.failed();
                    tracing::error!(error = %e, "the backup task did not finish");
                }
            }
        }
    });
}

/// Say what one run did, and leave it on the shared state.
///
/// **The log line is not the record.** A failure that only ever prints is a
/// failure nobody is alerted on and `/readyz` keeps calling backups enabled;
/// what makes it visible is that the next scrape of either reads this.
fn record(state: &BackupState, now_ms: i64, outcome: Result<Completed>) {
    match outcome {
        Ok(completed) => {
            state.succeeded(now_ms);
            tracing::info!(
                name = completed.name,
                sealed_bytes = completed.sealed_bytes,
                pruned = completed.pruned,
                "backup stored"
            );
        }
        Err(e) => {
            state.failed();
            tracing::error!(error = %format!("{e:#}"), "the backup failed");
        }
    }
}

/// The 32-byte key, read as hex from a mounted secret file.
fn read_key(path: &Path) -> Result<LessSafeKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading the backup key at {}", path.display()))?;
    let hex = raw.trim();
    if hex.len() != KEY_HEX_CHARS || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!(
            "the backup key at {} is {} characters; it is {KEY_HEX_CHARS} hex characters, \
             which is a 32-byte AES-256 key",
            path.display(),
            hex.len()
        );
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .expect("every pair was checked to be hex digits");
    }
    let unbound = UnboundKey::new(&AES_256_GCM, &bytes)
        .map_err(|_| anyhow::anyhow!("the backup key is not a valid AES-256 key"))?;
    Ok(LessSafeKey::new(unbound))
}

/// Encrypt one backup: a fresh random nonce, then the sealed bytes.
///
/// See the module note. `Nonce::assume_unique_for_key` is the name `ring` gives
/// this obligation, and the twelve bytes below are what discharges it — drawn
/// from the system CSPRNG on every single call, never derived from the clock,
/// a counter, or the file name, all of which repeat across a restart or a
/// rollback while the key does not.
fn seal(key: &LessSafeKey, plain: Vec<u8>) -> Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("the system random number generator refused"))?;
    let mut buffer = plain;
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(CONTEXT),
        &mut buffer,
    )
    .map_err(|_| anyhow::anyhow!("sealing the backup"))?;
    let mut sealed = Vec::with_capacity(NONCE_LEN + buffer.len());
    sealed.extend_from_slice(&nonce);
    sealed.append(&mut buffer);
    Ok(sealed)
}

/// Decrypt one backup — the restore primitive §7 depends on.
pub fn open_sealed(key: &LessSafeKey, sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() <= NONCE_LEN {
        bail!(
            "the sealed backup is {} bytes, which is not one",
            sealed.len()
        );
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(nonce);
    let mut buffer = body.to_vec();
    let plain = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(CONTEXT),
            &mut buffer,
        )
        .map_err(|_| anyhow::anyhow!("the sealed backup does not open with this key"))?;
    Ok(plain.to_vec())
}

/// When a backup was taken, read out of its name.
fn taken_at(name: &str) -> Option<i64> {
    let digits = name.strip_prefix(NAME_PREFIX)?.strip_suffix(NAME_SUFFIX)?;
    if digits.len() != NAME_DIGITS {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::api::{router, Relay};
    use crate::secret::{bearer_hash, new_bearer, token_hash, Secret};

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const KEY: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const NOW: i64 = 1_700_000_000_000;

    /// A directory under the OS temp dir. The process id is in the name because
    /// two `cargo test` invocations can overlap on one machine, and a fixed path
    /// means one of them deleting the other's fixture halfway through.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("push-relay-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config(dir: &Path, extra: &[(&str, &str)]) -> RelayConfig {
        let key_file = dir.join("backup-key");
        if !key_file.exists() {
            std::fs::write(&key_file, format!("{KEY}\n")).unwrap();
        }
        let mut pairs: Vec<(String, String)> = vec![
            (
                "RELAY_DB_PATH".into(),
                dir.join("relay.sqlite").to_string_lossy().into_owned(),
            ),
            (
                "RELAY_BACKUP_KEY_FILE".into(),
                key_file.to_string_lossy().into_owned(),
            ),
            (
                "RELAY_BACKUP_TARGET".into(),
                format!("file://{}", dir.join("backups").display()),
            ),
            (
                "RELAY_GENERATION_FLOOR_FILE".into(),
                dir.join("generation-floor").to_string_lossy().into_owned(),
            ),
        ];
        pairs.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        let map: std::collections::HashMap<String, String> = pairs.into_iter().collect();
        RelayConfig::read(move |key| map.get(key).cloned()).unwrap()
    }

    /// A live database with one attested installation and one active binding,
    /// returning the bearer that was minted for it.
    fn seeded(path: &Path) -> (Connection, Secret) {
        let conn = crate::db::open(path).unwrap();
        let bearer = new_bearer().unwrap();
        conn.execute(
            "INSERT INTO installations
                (key_id_hash, public_key, receipt, attest_environment, counter, counter_trusted,
                 bundle_version, validation_category, created_ms, updated_ms)
             VALUES ('a-key-id-hash', X'0102', NULL, 'production', 0, 1, '1.0', 1, ?1, ?1)",
            [NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO bindings
                (installation_id, token_hash, environment, bearer_hash, generation,
                 status, terminal_reason, created_ms, updated_ms)
             VALUES (1, ?1, 'production', ?2, 0, 'active', NULL, ?3, ?3)",
            rusqlite::params![token_hash(TOKEN), bearer_hash(&bearer), NOW],
        )
        .unwrap();
        (conn, bearer)
    }

    fn active_bearers(conn: &Connection, bearer: &Secret) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM bindings WHERE bearer_hash = ?1 AND status = 'active'",
            [bearer_hash(bearer)],
            |row| row.get(0),
        )
        .unwrap()
    }

    async fn credential_status(relay: Relay, bearer: &Secret) -> serde_json::Value {
        let response = router(relay)
            .oneshot(
                Request::builder()
                    .uri("/v1/credential/status")
                    .header("authorization", format!("Bearer {}", bearer.expose()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// **The round trip, and the reason there is a backup at all.** A sealed
    /// backup restores into a database a clean instance can open and query.
    #[test]
    fn a_backup_restores_into_a_clean_instance() {
        let dir = scratch("round-trip");
        let config = config(&dir, &[]);
        let (live, bearer) = seeded(&config.db_path);

        let job = Job::from_config(&config).unwrap();
        let done = job.run(&live, NOW).unwrap();
        assert!(done.sealed_bytes > NONCE_LEN);
        assert_eq!(done.pruned, 0);

        let restored_path = dir.join("restored.sqlite");
        std::fs::write(&restored_path, job.latest().unwrap().unwrap()).unwrap();
        let restored = crate::db::open(&restored_path).unwrap();
        assert_eq!(active_bearers(&restored, &bearer), 1);
        assert_eq!(
            crate::db::schema_version(&restored).unwrap(),
            crate::db::schema_version(&live).unwrap()
        );

        // The plaintext copy the job makes on the way through is not left
        // behind for anyone to read.
        assert!(!config.db_path.with_extension("backup-partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The sealed bytes are the thing an attacker reads.** The assertion is
    /// about them and not about the code that wrote them.
    #[test]
    fn the_sealed_bytes_carry_no_token_and_no_bearer() {
        let dir = scratch("sealed-opaque");
        let config = config(&dir, &[]);
        let (live, bearer) = seeded(&config.db_path);
        let job = Job::from_config(&config).unwrap();
        let done = job.run(&live, NOW).unwrap();

        let sealed = std::fs::read(dir.join("backups").join(&done.name)).unwrap();
        for secret in [TOKEN, bearer.expose()] {
            assert!(
                !contains(&sealed, secret.as_bytes()),
                "the sealed backup carries {secret:?}"
            );
        }
        // And it is not merely that the values are absent from the plaintext:
        // the hashes that *are* stored do not appear either, because the whole
        // file is ciphertext.
        assert!(!contains(&sealed, token_hash(TOKEN).as_bytes()));
        assert!(!contains(&sealed, b"SQLite format 3"));

        // The plaintext behind it does contain the hashes, which is what makes
        // the assertion above a statement about the encryption.
        let plain = job.latest().unwrap().unwrap();
        assert!(contains(&plain, b"SQLite format 3"));
        assert!(contains(&plain, token_hash(TOKEN).as_bytes()));
        assert!(!contains(&plain, TOKEN.as_bytes()));
        assert!(!contains(&plain, bearer.expose().as_bytes()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The footgun, asserted.** Two backups under one long-lived key must not
    /// share a nonce; AES-GCM under a repeated pair leaks the XOR of the two
    /// plaintexts and the authentication subkey with it.
    #[test]
    fn two_backups_never_share_a_nonce() {
        let dir = scratch("nonces");
        let config = config(&dir, &[]);
        let (live, _bearer) = seeded(&config.db_path);
        let job = Job::from_config(&config).unwrap();

        let mut nonces = std::collections::HashSet::new();
        for index in 0..8 {
            let done = job.run(&live, NOW + index).unwrap();
            let sealed = std::fs::read(dir.join("backups").join(&done.name)).unwrap();
            assert!(
                nonces.insert(sealed[..NONCE_LEN].to_vec()),
                "run {index} reused a nonce"
            );
        }
        // Same input, same key, different ciphertext — which is what a fresh
        // nonce is *for*, and what a fixed one would destroy.
        let sealed: Vec<Vec<u8>> = job
            .sink
            .list()
            .unwrap()
            .iter()
            .map(|name| job.sink.get(name).unwrap())
            .collect();
        assert_ne!(sealed[0], sealed[1]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_sealed_under_another_key_does_not_open() {
        let key = read_key_from("11".repeat(32).as_str());
        let other = read_key_from("22".repeat(32).as_str());
        let sealed = seal(&key, b"the relay database".to_vec()).unwrap();

        assert_eq!(open_sealed(&key, &sealed).unwrap(), b"the relay database");
        assert!(open_sealed(&other, &sealed).is_err());

        // A single flipped byte anywhere is a refusal rather than a corrupt
        // database that opens.
        for index in [0, NONCE_LEN, sealed.len() - 1] {
            let mut tampered = sealed.clone();
            tampered[index] ^= 0x01;
            assert!(open_sealed(&key, &tampered).is_err(), "byte {index}");
        }
        assert!(open_sealed(&key, &sealed[..NONCE_LEN]).is_err());
    }

    fn read_key_from(hex: &str) -> LessSafeKey {
        let dir = scratch(&format!("key-{}", &hex[..4]));
        let path = dir.join("backup-key");
        std::fs::write(&path, hex).unwrap();
        let key = read_key(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        key
    }

    /// An absent or malformed key disables backups and says so; it does not
    /// stop the relay.
    #[test]
    fn a_missing_or_malformed_key_disables_backups_and_names_the_file() {
        let dir = scratch("key-absent");
        let missing = config(&dir, &[]);
        let missing_path = dir.join("no-such-key");
        std::fs::remove_file(dir.join("backup-key")).unwrap();
        let why = absence(&missing).expect("no key file");
        assert!(why.contains("backup-key"), "{why}");

        std::fs::write(&missing_path, "not hex at all").unwrap();
        let malformed = config(
            &dir,
            &[("RELAY_BACKUP_KEY_FILE", missing_path.to_str().unwrap())],
        );
        let why = absence(&malformed).expect("a key that is not a key");
        assert!(why.contains("hex characters"), "{why}");

        // And a key that is there is no absence at all.
        std::fs::write(&missing_path, KEY).unwrap();
        assert_eq!(absence(&malformed), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsupported_target_scheme_is_refused_with_the_ones_that_work() {
        for target in [
            "s3://codeconnect-relay-backups",
            "https://example.invalid/backups",
            "/var/data/backups",
            "file://",
            "",
        ] {
            let err = sink(target)
                .err()
                .unwrap_or_else(|| panic!("accepted {target:?}"))
                .to_string();
            assert!(err.contains("file:///path"), "{err}");
        }
        assert!(sink("file:///var/data/backups").is_ok());
    }

    /// Retention deletes what is past the window and nothing that is inside it.
    #[test]
    fn retention_deletes_only_what_is_old_enough() {
        let dir = scratch("retention");
        let config = config(&dir, &[("RELAY_BACKUP_RETENTION_DAYS", "30")]);
        let (live, _bearer) = seeded(&config.db_path);
        let job = Job::from_config(&config).unwrap();

        // One a day for forty days, each run pruning as it goes.
        for day in 0..40 {
            job.run(&live, NOW + day * DAY_MS).unwrap();
        }
        let kept = job.sink.list().unwrap();
        assert_eq!(kept.len(), 31, "thirty days plus today: {kept:?}");
        let oldest = taken_at(&kept[0]).unwrap();
        assert_eq!(oldest, NOW + 9 * DAY_MS);

        // The boundary is on the keeping side: a backup exactly thirty days old
        // is still inside the window the policy promises, and the one taken a
        // day before it is not.
        let newest = kept.last().unwrap().clone();
        assert_eq!(job.prune(NOW + 69 * DAY_MS).unwrap(), 30);
        assert_eq!(job.sink.list().unwrap(), vec![newest]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that is not one of ours is left alone, so a directory shared with
    /// anything else is not a directory this job deletes from.
    #[test]
    fn a_file_that_is_not_a_backup_is_neither_listed_nor_pruned() {
        let dir = scratch("foreign");
        let config = config(&dir, &[("RELAY_BACKUP_RETENTION_DAYS", "0")]);
        let (live, _bearer) = seeded(&config.db_path);
        let job = Job::from_config(&config).unwrap();
        job.run(&live, NOW).unwrap();

        let foreign = dir.join("backups").join("notes.txt");
        std::fs::write(&foreign, b"someone else's file").unwrap();
        job.run(&live, NOW + DAY_MS).unwrap();

        assert!(foreign.exists(), "a foreign file was deleted");
        assert!(job
            .sink
            .list()
            .unwrap()
            .iter()
            .all(|name| taken_at(name).is_some()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **§7's restore drill, end to end.**
    ///
    /// Revoke a credential, restore a backup from before the revocation, and
    /// the revoked bearer is alive again — which is the whole danger, and no
    /// query against the restored database can see it. Raising the generation
    /// floor is what closes it: every restored bearer is refused at once and
    /// the phone is told to attest again.
    #[tokio::test]
    async fn the_restore_drill_fails_closed_once_the_generation_floor_is_raised() {
        let dir = scratch("restore-drill");
        let config = config(&dir, &[]);
        let floor_file = config.generation_floor_file.clone();
        let (live, bearer) = seeded(&config.db_path);

        // A backup taken while the credential is good.
        let job = Job::from_config(&config).unwrap();
        job.run(&live, NOW).unwrap();
        assert_eq!(active_bearers(&live, &bearer), 1);

        // The revocation the incident called for.
        live.execute(
            "UPDATE bindings SET status = 'revoked', terminal_reason = 'rotated' WHERE bearer_hash = ?1",
            [bearer_hash(&bearer)],
        )
        .unwrap();
        assert_eq!(active_bearers(&live, &bearer), 0);

        // The restore, into a clean instance.
        let restored_path = dir.join("restored.sqlite");
        std::fs::write(&restored_path, job.latest().unwrap().unwrap()).unwrap();
        let restored = crate::db::open(&restored_path).unwrap();
        assert_eq!(
            active_bearers(&restored, &bearer),
            1,
            "the danger §7 exists for: the revoked bearer is live again"
        );

        // And it is genuinely usable — the relay honours it, because nothing in
        // the database records that it was revoked.
        let relay = Relay::new(config, restored);
        assert_eq!(
            credential_status(relay.clone(), &bearer).await["status"],
            "active"
        );
        assert_eq!(
            crate::push::push(
                &relay,
                Some(&format!("Bearer {}", bearer.expose())),
                None,
                push_body().as_bytes(),
                NOW,
            )
            .await
            .status,
            // Refused only because this relay has no sandbox or production key,
            // which is a fact about the transport and not about the credential.
            StatusCode::SERVICE_UNAVAILABLE
        );

        // The floor, which lives outside the database precisely so a restore
        // cannot bring it back.
        std::fs::write(&floor_file, "1\n").unwrap();
        assert_eq!(
            credential_status(relay.clone(), &bearer).await["status"],
            "reenroll",
            "the phone has to be told to attest again"
        );
        let refused = crate::push::push(
            &relay,
            Some(&format!("Bearer {}", bearer.expose())),
            None,
            push_body().as_bytes(),
            NOW,
        )
        .await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused.body["error"], "credential_invalid");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn push_body() -> String {
        format!(
            r#"{{"schema":1,"token":"{TOKEN}","environment":"production","notification":{{"type":"test"}}}}"#
        )
    }

    /// **The full-disk case, which is the one the job has to survive.** The
    /// store fails, the run reports that failure — and the prune still runs, so
    /// the space the next attempt needs has been freed rather than held by
    /// backups the policy expired weeks ago.
    #[test]
    fn a_store_that_fails_still_reclaims_the_space_the_next_one_needs() {
        let dir = scratch("full-disk");
        let config = config(&dir, &[("RELAY_BACKUP_RETENTION_DAYS", "30")]);
        let (live, _bearer) = seeded(&config.db_path);
        let sink = Arc::new(Refusing::default());
        let job = Job {
            key: read_key(&config.backup_key_file).unwrap(),
            sink: Box::new(Arc::clone(&sink)),
            db_path: config.db_path.clone(),
            retention_days: 30,
        };

        for day in 0..40 {
            job.run(&live, NOW + day * DAY_MS).unwrap();
        }
        assert_eq!(job.sink.list().unwrap().len(), 31);

        // The disk fills. The store fails and says so.
        sink.refuse.store(true, Ordering::SeqCst);
        let failed = job
            .run(&live, NOW + 60 * DAY_MS)
            .expect_err("the store failed");
        assert!(format!("{failed:#}").contains("no space"), "{failed:#}");

        // And the backups the policy expired are gone, which is the only way
        // the next run can succeed. Thirty days back from the run that failed,
        // which is days thirty to thirty-nine of the forty.
        let kept = job.sink.list().unwrap();
        assert_eq!(
            kept.len(),
            10,
            "a full disk kept its own recovery from running: {kept:?}"
        );
        assert!(
            kept.iter()
                .all(|name| taken_at(name).unwrap() >= NOW + 30 * DAY_MS),
            "{kept:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sink that stores until it is told the disk is full.
    #[derive(Default)]
    struct Refusing {
        stored: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
        refuse: std::sync::atomic::AtomicBool,
    }

    impl BackupSink for Arc<Refusing> {
        fn put(&self, name: &str, sealed: &[u8]) -> Result<()> {
            if self.refuse.load(Ordering::SeqCst) {
                bail!("writing {name}: no space left on device");
            }
            self.stored
                .lock()
                .unwrap()
                .push((name.to_string(), sealed.to_vec()));
            Ok(())
        }
        fn list(&self) -> Result<Vec<String>> {
            let mut names: Vec<String> = self
                .stored
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            names.sort();
            Ok(names)
        }
        fn get(&self, name: &str) -> Result<Vec<u8>> {
            self.stored
                .lock()
                .unwrap()
                .iter()
                .find(|(stored, _)| stored == name)
                .map(|(_, bytes)| bytes.clone())
                .context("no such backup")
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.stored.lock().unwrap().retain(|(n, _)| n != name);
            Ok(())
        }
        fn describe(&self) -> String {
            "refusing".into()
        }
    }

    /// **`backups_enabled` is a fact about the last run and not about the key
    /// file.** A relay whose configuration parses and whose disk is full has to
    /// stop saying its backups are on, on the endpoint and in the metric an
    /// operator alerts from.
    #[tokio::test]
    async fn a_failed_run_turns_backups_off_on_readyz_and_in_the_metric() {
        let dir = scratch("state");
        let config = config(&dir, &[]);
        let relay = Relay::new(config, crate::db::open_in_memory().unwrap());

        // Configured, nothing has failed, and no run has succeeded yet.
        assert_eq!(readyz(relay.clone()).await["backups_enabled"], true);
        assert!(
            metrics_text(relay.clone())
                .await
                .contains("codeconnect_relay_backup_age_seconds -1"),
            "a relay that has never taken one must not report an age"
        );

        record(&relay.backup, NOW, Ok(completed()));
        assert_eq!(readyz(relay.clone()).await["backups_enabled"], true);

        record(&relay.backup, NOW, Err(anyhow::anyhow!("no space left")));
        assert_eq!(
            readyz(relay.clone()).await["backups_enabled"],
            false,
            "a failed run has to be visible where the configuration is"
        );
        assert!(metrics_text(relay.clone())
            .await
            .contains("codeconnect_relay_backups_enabled 0"));

        // And a run that works again says so, rather than needing a restart.
        record(
            &relay.backup,
            crate::enroll::now_ms() - 3_600_000,
            Ok(completed()),
        );
        assert_eq!(readyz(relay.clone()).await["backups_enabled"], true);
        let age = metrics_text(relay.clone())
            .await
            .lines()
            .find_map(|line| line.strip_prefix("codeconnect_relay_backup_age_seconds "))
            .and_then(|value| value.parse::<i64>().ok())
            .expect("the age gauge");
        assert!((3_500..=3_700).contains(&age), "{age}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relay that cannot back up at all still reports it, whatever the last
    /// run did — the two halves are `and`, not `or`.
    #[tokio::test]
    async fn an_unconfigured_backup_is_off_however_well_the_last_run_went() {
        let relay = Relay::new(
            RelayConfig::read(|_| None).unwrap(),
            crate::db::open_in_memory().unwrap(),
        );
        assert!(relay.backup_absence.is_some(), "no key file is mounted");
        record(&relay.backup, NOW, Ok(completed()));
        assert_eq!(readyz(relay).await["backups_enabled"], false);
    }

    fn completed() -> Completed {
        Completed {
            name: "relay-1700000000000.sqlite.enc".into(),
            sealed_bytes: 4_096,
            pruned: 0,
        }
    }

    async fn readyz(relay: Relay) -> serde_json::Value {
        serde_json::from_str(&get(relay, "/readyz").await).unwrap()
    }

    async fn metrics_text(relay: Relay) -> String {
        get(relay, "/metrics").await
    }

    async fn get(relay: Relay, path: &str) -> String {
        let response = router(relay)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The sink is a trait so a later object store is an added implementation.
    /// This is that implementation, written in a test to prove the job needs
    /// nothing from a directory.
    #[test]
    fn a_second_sink_needs_no_change_to_the_job() {
        #[derive(Default)]
        struct InMemory {
            stored: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
            deletes: AtomicUsize,
        }
        impl BackupSink for InMemory {
            fn put(&self, name: &str, sealed: &[u8]) -> Result<()> {
                self.stored
                    .lock()
                    .unwrap()
                    .push((name.to_string(), sealed.to_vec()));
                Ok(())
            }
            fn list(&self) -> Result<Vec<String>> {
                let mut names: Vec<String> = self
                    .stored
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect();
                names.sort();
                Ok(names)
            }
            fn get(&self, name: &str) -> Result<Vec<u8>> {
                self.stored
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(stored, _)| stored == name)
                    .map(|(_, bytes)| bytes.clone())
                    .context("no such backup")
            }
            fn delete(&self, name: &str) -> Result<()> {
                self.deletes.fetch_add(1, Ordering::SeqCst);
                self.stored.lock().unwrap().retain(|(n, _)| n != name);
                Ok(())
            }
            fn describe(&self) -> String {
                "memory".into()
            }
        }

        let dir = scratch("second-sink");
        let config = config(&dir, &[]);
        let (live, _bearer) = seeded(&config.db_path);
        let job = Job {
            key: read_key(&config.backup_key_file).unwrap(),
            sink: Box::new(InMemory::default()),
            db_path: config.db_path.clone(),
            retention_days: 1,
        };

        job.run(&live, NOW).unwrap();
        job.run(&live, NOW + 5 * DAY_MS).unwrap();
        assert_eq!(job.sink.list().unwrap().len(), 1, "the old one was pruned");
        assert!(contains(
            &job.latest().unwrap().unwrap(),
            b"SQLite format 3"
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
