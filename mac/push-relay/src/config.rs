//! Everything the relay is told at startup, and where the secrets are not.
//!
//! Two rules shape this module.
//!
//! **A secret is a file, never a variable.** The APNs signing keys, the
//! generation floor, the backup key and the IP pepper are read from paths that
//! are themselves configurable — so a local run can point at a directory it
//! owns, and a deployment can mount them where its platform puts secret files
//! without anything being baked into an image or printed by an environment
//! dump.
//!
//! **Blank means absent.** A deployment sets several of these to the empty
//! string as a visible placeholder for a value that has not been issued yet. An
//! empty topic treated as a literal topic would produce a `403` from Apple that
//! names neither the variable nor the reason, so an empty value is exactly an
//! unset one here.

use std::path::PathBuf;

use anyhow::{Context, Result};
use push_core::{ApnsEnvironment, ApnsIdentity, ProviderToken};

const DEFAULT_PORT: u16 = 10_000;
const DEFAULT_DB_PATH: &str = "/var/data/relay.sqlite";
const DEFAULT_SANDBOX_KEY_FILE: &str = "/etc/secrets/apns-sandbox.p8";
const DEFAULT_PRODUCTION_KEY_FILE: &str = "/etc/secrets/apns-production.p8";
const DEFAULT_GENERATION_FLOOR_FILE: &str = "/etc/secrets/generation-floor";
const DEFAULT_BACKUP_KEY_FILE: &str = "/etc/secrets/backup-key";
const DEFAULT_IP_PEPPER_FILE: &str = "/etc/secrets/ip-pepper";
const DEFAULT_BACKUP_S3_SECRET_KEY_FILE: &str = "/etc/secrets/backup-s3-secret-key";
const DEFAULT_BACKUP_TARGET: &str = "file:///var/data/backups";
const DEFAULT_BACKUP_S3_REGION: &str = "us-east-1";
const DEFAULT_BACKUP_RETENTION_DAYS: u32 = 30;

/// Which App Attest namespace an attestation is checked against.
///
/// Apple issues a different AAGUID for a development build than for one
/// distributed through the store, and the two are separate namespaces rather
/// than a strict/lenient pair: accepting a development attestation in
/// production would accept any build anyone can sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestEnvironment {
    Development,
    Production,
}

impl AttestEnvironment {
    pub fn as_str(self) -> &'static str {
        match self {
            AttestEnvironment::Development => "development",
            AttestEnvironment::Production => "production",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "development" => Ok(AttestEnvironment::Development),
            "production" => Ok(AttestEnvironment::Production),
            other => anyhow::bail!(
                "RELAY_ATTEST_ENVIRONMENT is {other:?}; it is `development` or `production`"
            ),
        }
    }
}

/// What one APNs environment needs in order to sign a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApnsKey {
    pub identity: ApnsIdentity,
    pub key_file: PathBuf,
}

/// One APNs environment's credentials, or the reason there are none.
///
/// **Absence is a running state, not a startup failure.** A relay that exited
/// because a `.p8` had not been issued yet could not serve the enrollment
/// endpoints that have to work *before* the first push is ever sent, and a
/// deployment whose key file is briefly unreadable would restart in a loop
/// instead of reporting the one thing that is wrong. So the relay boots,
/// `/readyz` says which environment cannot send, and a push for that
/// environment answers `unavailable` rather than falling back to the other
/// environment's key — which would sign for the wrong topic and be refused by
/// Apple with a message about the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApnsSlot {
    Ready(ApnsKey),
    Absent(String),
}

impl ApnsSlot {
    /// The credentials, when there are any.
    pub fn key(&self) -> Option<&ApnsKey> {
        match self {
            ApnsSlot::Ready(key) => Some(key),
            ApnsSlot::Absent(_) => None,
        }
    }

    /// Why this environment cannot send, in the words `/readyz` reports and the
    /// startup log prints once.
    pub fn absence(&self) -> Option<&str> {
        match self {
            ApnsSlot::Ready(_) => None,
            ApnsSlot::Absent(why) => Some(why),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub port: u16,
    pub db_path: PathBuf,
    /// `<teamID>.<bundle id>`, whose SHA-256 is the `rpIdHash` an App Attest
    /// attestation has to match. Absent when it has not been set, because an
    /// empty string here would be hashed and compared like any other value and
    /// would fail every attestation with a message about a hash.
    pub app_id: Option<String>,
    pub attest_environment: AttestEnvironment,
    pub min_bundle_version: Option<String>,
    pub sandbox: ApnsSlot,
    pub production: ApnsSlot,
    pub send_enabled: bool,
    pub enrollment_enabled: bool,
    pub generation_floor_file: PathBuf,
    pub backup_key_file: PathBuf,
    pub ip_pepper_file: PathBuf,
    /// Where sealed backups go: `file:///path` or `s3://bucket/optional/prefix`.
    ///
    /// **The default is a directory because a developer machine has no bucket**,
    /// and the deployment overrides it — a relay whose database and whose
    /// backups are on one disk loses both to one disk, which is the state this
    /// setting exists to move a deployment out of.
    pub backup_target: String,
    /// The S3 API address, when it is not AWS's own.
    ///
    /// Present so that Cloudflare R2, Backblaze B2 and MinIO are reachable and
    /// not only AWS: they speak the same API at a different host. Absent means
    /// AWS, whose host this build derives from the bucket and the region.
    pub backup_s3_endpoint: Option<String>,
    /// The region every signature's credential scope names. A signature scoped
    /// to the wrong region is refused by the store, so this is not cosmetic.
    pub backup_s3_region: String,
    pub backup_s3_access_key_id: Option<String>,
    /// **The secret access key is a file, like every other secret here.** An
    /// environment variable is printed by a process dump, inherited by every
    /// child the relay spawns and shown by the deployment dashboard to anyone
    /// who can read the service — and this one key can read, overwrite and
    /// delete every backup the relay has ever taken, which is the whole of the
    /// state after a disk loss.
    pub backup_s3_secret_key_file: PathBuf,
    pub backup_retention_days: u32,
    /// The deployed commit, reported so an operator reading a metric knows
    /// which code produced it.
    pub git_sha: Option<String>,
}

impl RelayConfig {
    pub fn from_env() -> Result<Self> {
        Self::read(|key| std::env::var(key).ok())
    }

    /// The same load against an arbitrary lookup, so the rules are testable
    /// without a process-wide environment two tests could race over.
    pub fn read(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str| present(lookup(key));

        let port = match get("PORT") {
            None => DEFAULT_PORT,
            Some(value) => value.parse().with_context(|| {
                format!("PORT is {value:?}; it is a TCP port number, for example {DEFAULT_PORT}")
            })?,
        };
        let backup_retention_days = match get("RELAY_BACKUP_RETENTION_DAYS") {
            None => DEFAULT_BACKUP_RETENTION_DAYS,
            Some(value) => value.parse().with_context(|| {
                format!(
                    "RELAY_BACKUP_RETENTION_DAYS is {value:?}; it is a whole number of days, \
                     for example {DEFAULT_BACKUP_RETENTION_DAYS}"
                )
            })?,
        };
        let attest_environment = match get("RELAY_ATTEST_ENVIRONMENT") {
            None => AttestEnvironment::Development,
            Some(value) => AttestEnvironment::parse(&value)?,
        };

        Ok(RelayConfig {
            port,
            db_path: get("RELAY_DB_PATH").map_or_else(|| DEFAULT_DB_PATH.into(), PathBuf::from),
            app_id: get("RELAY_APP_ID"),
            attest_environment,
            min_bundle_version: get("RELAY_MIN_BUNDLE_VERSION"),
            sandbox: slot(&get, ApnsEnvironment::Sandbox),
            production: slot(&get, ApnsEnvironment::Production),
            send_enabled: flag(&get, "RELAY_SEND_ENABLED")?,
            enrollment_enabled: flag(&get, "RELAY_ENROLLMENT_ENABLED")?,
            generation_floor_file: get("RELAY_GENERATION_FLOOR_FILE")
                .map_or_else(|| DEFAULT_GENERATION_FLOOR_FILE.into(), PathBuf::from),
            backup_key_file: get("RELAY_BACKUP_KEY_FILE")
                .map_or_else(|| DEFAULT_BACKUP_KEY_FILE.into(), PathBuf::from),
            ip_pepper_file: get("RELAY_IP_PEPPER_FILE")
                .map_or_else(|| DEFAULT_IP_PEPPER_FILE.into(), PathBuf::from),
            backup_target: get("RELAY_BACKUP_TARGET")
                .unwrap_or_else(|| DEFAULT_BACKUP_TARGET.to_string()),
            backup_s3_endpoint: get("RELAY_BACKUP_S3_ENDPOINT"),
            backup_s3_region: get("RELAY_BACKUP_S3_REGION")
                .unwrap_or_else(|| DEFAULT_BACKUP_S3_REGION.to_string()),
            backup_s3_access_key_id: get("RELAY_BACKUP_S3_ACCESS_KEY_ID"),
            backup_s3_secret_key_file: get("RELAY_BACKUP_S3_SECRET_KEY_FILE")
                .map_or_else(|| DEFAULT_BACKUP_S3_SECRET_KEY_FILE.into(), PathBuf::from),
            backup_retention_days,
            git_sha: get("RELAY_GIT_SHA"),
        })
    }

    pub fn slot(&self, environment: ApnsEnvironment) -> &ApnsSlot {
        match environment {
            ApnsEnvironment::Sandbox => &self.sandbox,
            ApnsEnvironment::Production => &self.production,
        }
    }

    /// The lowest credential generation this relay will honour, read fresh.
    ///
    /// Read on every call rather than cached at startup so that raising the
    /// floor — the step that stops a restored backup from resurrecting a
    /// revoked bearer — takes effect on the next request instead of on the next
    /// restart. A restart is the one thing an operator handling a database
    /// incident should not have to do.
    ///
    /// A file that is not there is floor 0: no incident has happened. A file
    /// that is there and unreadable is an **error**, because answering 0 to a
    /// permissions problem would quietly re-admit every credential the floor
    /// was raised to refuse.
    pub fn generation_floor(&self) -> Result<u64> {
        let raw = match std::fs::read_to_string(&self.generation_floor_file) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "reading the generation floor at {}",
                        self.generation_floor_file.display()
                    )
                })
            }
        };
        let value = raw.trim();
        value.parse().with_context(|| {
            format!(
                "the generation floor at {} is {} bytes and {}; it is a decimal whole number, \
                 for example 0",
                self.generation_floor_file.display(),
                value.len(),
                unreadable(value)
            )
        })
    }
}

/// Why a value is not a generation, said without saying what the value was.
///
/// **The contents never reach the message.** Every path this module reads is
/// under a secret mount, so an operator who transposes two filenames would
/// otherwise put a signing key or a backup key into a structured log that leaves
/// the machine. A length and the first character that is not a digit are enough
/// to recognise which file was mounted where.
fn unreadable(value: &str) -> String {
    if value.is_empty() {
        return "empty".to_string();
    }
    match value.chars().find(|c| !c.is_ascii_digit()) {
        Some(found) => format!("carries {found:?} where a digit belongs"),
        None => "a number larger than this relay can count to".to_string(),
    }
}

/// An empty or whitespace-only value is an unset one, and a value that survives
/// arrives trimmed — a trailing newline in a mounted variable is otherwise part
/// of a topic.
fn present(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn flag(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<bool> {
    match get(key) {
        None => Ok(true),
        Some(value) => match value.as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            other => anyhow::bail!("{key} is {other:?}; it is `true` or `false`"),
        },
    }
}

/// Whether one environment can sign, and if not, exactly what is missing.
///
/// **The key is parsed here, because this slot is what readiness is read from.**
/// A `.p8` that opens and is not a key would otherwise be reported as loaded by
/// the startup log, by `/readyz` and by the metric, while every push for that
/// environment answered `unavailable` — an operator watching the one place that
/// should have told them would see nothing wrong. Signing still parses it again
/// where it signs; what happens here is a check, and the parsed key is
/// deliberately not carried on a struct that is `Debug` and `Clone`.
///
/// The three failures stay three sentences. "Not set", "cannot be read" and "not
/// a key" are three different things for an operator to go and do, and a log
/// that says only that the key is unusable sends them to the wrong one.
fn slot(get: &impl Fn(&str) -> Option<String>, environment: ApnsEnvironment) -> ApnsSlot {
    let (prefix, default_file) = match environment {
        ApnsEnvironment::Sandbox => ("RELAY_APNS_SANDBOX", DEFAULT_SANDBOX_KEY_FILE),
        ApnsEnvironment::Production => ("RELAY_APNS_PRODUCTION", DEFAULT_PRODUCTION_KEY_FILE),
    };
    let key_file = get(&format!("{prefix}_KEY_FILE"))
        .map_or_else(|| PathBuf::from(default_file), PathBuf::from);

    let mut missing = Vec::new();
    let mut field = |name: &str| match get(&format!("{prefix}_{name}")) {
        Some(value) => value,
        None => {
            missing.push(format!("{prefix}_{name}"));
            String::new()
        }
    };
    let key_id = field("KEY_ID");
    let team_id = field("TEAM_ID");
    let topic = field("TOPIC");
    if !missing.is_empty() {
        return ApnsSlot::Absent(format!("{} is not set", missing.join(", ")));
    }

    if let Err(e) = std::fs::File::open(&key_file) {
        return ApnsSlot::Absent(format!(
            "{} cannot be read: {}",
            key_file.display(),
            e.kind()
        ));
    }

    let identity = ApnsIdentity {
        key_id,
        team_id,
        topic,
    };
    // **The parser's own message is not repeated.** It can quote the line it
    // choked on, and every path this module reads is under a secret mount — so
    // an operator who transposes two filenames would put key material into a
    // structured log that leaves the machine. The file and the shape it should
    // have are enough to fix it.
    if ProviderToken::load(&key_file, identity.clone()).is_err() {
        return ApnsSlot::Absent(format!(
            "{} is not a usable APNs signing key; it is the PKCS#8 PEM Apple hands you, \
             unconverted",
            key_file.display()
        ));
    }

    ApnsSlot::Ready(ApnsKey { identity, key_file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn from(pairs: &[(&str, &str)]) -> Result<RelayConfig> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        RelayConfig::read(move |key| map.get(key).cloned())
    }

    /// A directory under the OS temp dir, cleaned up by the caller.
    ///
    /// The process id is in the name because two `cargo test` invocations can
    /// overlap on one machine, and a fixed path means one of them deleting the
    /// other's fixture halfway through.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("push-relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_environment_yields_the_documented_defaults() {
        let config = from(&[]).unwrap();
        assert_eq!(config.port, 10_000);
        assert_eq!(config.db_path, PathBuf::from("/var/data/relay.sqlite"));
        assert_eq!(config.app_id, None);
        assert_eq!(config.attest_environment, AttestEnvironment::Development);
        assert_eq!(config.min_bundle_version, None);
        assert!(config.send_enabled);
        assert!(config.enrollment_enabled);
        assert_eq!(
            config.generation_floor_file,
            PathBuf::from("/etc/secrets/generation-floor")
        );
        assert_eq!(
            config.backup_key_file,
            PathBuf::from("/etc/secrets/backup-key")
        );
        assert_eq!(
            config.ip_pepper_file,
            PathBuf::from("/etc/secrets/ip-pepper")
        );
        assert_eq!(config.backup_target, "file:///var/data/backups");
        assert_eq!(config.backup_s3_endpoint, None);
        assert_eq!(config.backup_s3_region, "us-east-1");
        assert_eq!(config.backup_s3_access_key_id, None);
        assert_eq!(
            config.backup_s3_secret_key_file,
            PathBuf::from("/etc/secrets/backup-s3-secret-key")
        );
        assert_eq!(config.backup_retention_days, 30);
        assert_eq!(config.git_sha, None);
    }

    /// **The object-store credential follows the module's two rules.** The
    /// address, the region and the key id are ordinary settings; the secret
    /// access key is a path, because a variable holding it is printed by a
    /// process dump and shown by the deployment dashboard — and that one key can
    /// delete every backup the relay has.
    #[test]
    fn an_object_store_target_reads_its_address_from_variables_and_its_secret_from_a_file() {
        let config = from(&[
            ("RELAY_BACKUP_TARGET", "s3://relay-backups/nightly"),
            (
                "RELAY_BACKUP_S3_ENDPOINT",
                "https://accountid.r2.cloudflarestorage.com",
            ),
            ("RELAY_BACKUP_S3_REGION", "auto"),
            ("RELAY_BACKUP_S3_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
            (
                "RELAY_BACKUP_S3_SECRET_KEY_FILE",
                "/run/secrets/backup-s3-secret-key",
            ),
        ])
        .unwrap();
        assert_eq!(config.backup_target, "s3://relay-backups/nightly");
        assert_eq!(
            config.backup_s3_endpoint.as_deref(),
            Some("https://accountid.r2.cloudflarestorage.com")
        );
        assert_eq!(config.backup_s3_region, "auto");
        assert_eq!(
            config.backup_s3_access_key_id.as_deref(),
            Some("AKIAIOSFODNN7EXAMPLE")
        );
        assert_eq!(
            config.backup_s3_secret_key_file,
            PathBuf::from("/run/secrets/backup-s3-secret-key")
        );

        // And the blank placeholders a deployment writes before the bucket
        // exists are unset values, not an endpoint named "" and a region named
        // "" that every signature would be scoped to.
        let blank = from(&[
            ("RELAY_BACKUP_S3_ENDPOINT", "  "),
            ("RELAY_BACKUP_S3_REGION", ""),
            ("RELAY_BACKUP_S3_ACCESS_KEY_ID", ""),
        ])
        .unwrap();
        assert_eq!(blank.backup_s3_endpoint, None);
        assert_eq!(blank.backup_s3_region, "us-east-1");
        assert_eq!(blank.backup_s3_access_key_id, None);
    }

    /// **The placeholder case.** A deployment writes these as empty strings
    /// before the values exist, and an empty topic is not a topic.
    #[test]
    fn a_blank_value_is_an_unset_value() {
        let config = from(&[
            ("PORT", "   "),
            ("RELAY_APP_ID", ""),
            ("RELAY_MIN_BUNDLE_VERSION", ""),
            ("RELAY_GIT_SHA", ""),
            ("RELAY_BACKUP_TARGET", ""),
            ("RELAY_APNS_PRODUCTION_TOPIC", ""),
        ])
        .unwrap();
        assert_eq!(config.port, 10_000);
        assert_eq!(config.app_id, None);
        assert_eq!(config.min_bundle_version, None);
        assert_eq!(config.git_sha, None);
        assert_eq!(config.backup_target, "file:///var/data/backups");
        assert!(config
            .production
            .absence()
            .unwrap()
            .contains("RELAY_APNS_PRODUCTION_TOPIC"));

        // And a value that is merely padded is still that value.
        let padded = from(&[("RELAY_GIT_SHA", " abc123 \n")]).unwrap();
        assert_eq!(padded.git_sha.as_deref(), Some("abc123"));
    }

    #[test]
    fn an_unparseable_number_names_the_variable_and_a_valid_value() {
        let err = from(&[("PORT", "http")]).unwrap_err().to_string();
        assert!(err.contains("PORT"), "{err}");
        assert!(
            err.contains("10000") || err.contains("port number"),
            "{err}"
        );

        let err = from(&[("RELAY_BACKUP_RETENTION_DAYS", "a month")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("RELAY_BACKUP_RETENTION_DAYS"), "{err}");
        assert!(err.contains("days"), "{err}");
    }

    #[test]
    fn an_unknown_environment_name_is_refused_rather_than_guessed() {
        let err = from(&[("RELAY_ATTEST_ENVIRONMENT", "staging")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("RELAY_ATTEST_ENVIRONMENT"), "{err}");
        assert!(err.contains("development"), "{err}");
        assert!(err.contains("production"), "{err}");

        assert_eq!(
            from(&[("RELAY_ATTEST_ENVIRONMENT", "production")])
                .unwrap()
                .attest_environment,
            AttestEnvironment::Production
        );
    }

    #[test]
    fn a_kill_switch_is_a_word_and_not_anything_truthy() {
        assert!(
            !from(&[("RELAY_SEND_ENABLED", "false")])
                .unwrap()
                .send_enabled
        );
        assert!(
            !from(&[("RELAY_ENROLLMENT_ENABLED", "0")])
                .unwrap()
                .enrollment_enabled
        );
        let err = from(&[("RELAY_SEND_ENABLED", "no")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("RELAY_SEND_ENABLED"), "{err}");
        assert!(err.contains("true"), "{err}");
    }

    /// **The keys-absent state, which is a state and not a failure.** Every
    /// road to it is here: no identity, a partial identity, and an identity
    /// whose key file is not on disk.
    #[test]
    fn a_missing_key_leaves_the_relay_running_and_says_which_environment_cannot_send() {
        let dir = scratch("keys-absent");
        let sandbox_key = dir.join("apns-sandbox.p8");
        std::fs::write(&sandbox_key, crate::apns::fake_apple::signing_key_file()).unwrap();

        let config = from(&[
            ("RELAY_APNS_SANDBOX_KEY_ID", "SANDKEYID1"),
            ("RELAY_APNS_SANDBOX_TEAM_ID", "TEAMID1234"),
            ("RELAY_APNS_SANDBOX_TOPIC", "com.example.app"),
            ("RELAY_APNS_SANDBOX_KEY_FILE", sandbox_key.to_str().unwrap()),
            ("RELAY_APNS_PRODUCTION_KEY_ID", "PRODKEYID1"),
            ("RELAY_APNS_PRODUCTION_TEAM_ID", "TEAMID1234"),
            ("RELAY_APNS_PRODUCTION_TOPIC", "com.example.app"),
            (
                "RELAY_APNS_PRODUCTION_KEY_FILE",
                dir.join("apns-production.p8").to_str().unwrap(),
            ),
        ])
        .expect("a missing key file must not stop the relay from starting");

        let ready = config.sandbox.key().expect("the sandbox key is readable");
        assert_eq!(ready.identity.key_id, "SANDKEYID1");
        assert_eq!(ready.identity.topic, "com.example.app");
        assert_eq!(ready.key_file, sandbox_key);
        assert_eq!(config.sandbox.absence(), None);

        let absent = config
            .production
            .absence()
            .expect("the production key file is not there");
        assert!(absent.contains("apns-production.p8"), "{absent}");
        assert!(config.production.key().is_none());

        // A partial identity is the same state, named by variable.
        let partial = from(&[
            ("RELAY_APNS_SANDBOX_KEY_ID", "SANDKEYID1"),
            ("RELAY_APNS_SANDBOX_TOPIC", "com.example.app"),
        ])
        .unwrap();
        let why = partial.sandbox.absence().unwrap();
        assert!(why.contains("RELAY_APNS_SANDBOX_TEAM_ID"), "{why}");
        assert!(!why.contains("RELAY_APNS_SANDBOX_KEY_ID"), "{why}");

        // And no configuration at all names all three.
        let bare = from(&[]).unwrap();
        let why = bare.production.absence().unwrap();
        for name in ["KEY_ID", "TEAM_ID", "TOPIC"] {
            assert!(why.contains(name), "{why}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A file that is there and is not a key is its own state, and it is not
    /// "ready".** Readiness is read from this slot, so a slot that called a
    /// malformed `.p8` loaded would put "the key is fine" on `/readyz`, in the
    /// metric and in the startup log while every push for that environment
    /// answered `unavailable`.
    ///
    /// The three sentences stay three sentences: not set, cannot be read, and
    /// not a key are three different things to go and do about it.
    #[test]
    fn a_key_file_that_is_not_a_key_is_absent_and_says_so_in_its_own_words() {
        let dir = scratch("keys-malformed");
        let key_file = dir.join("apns-sandbox.p8");
        let with_key = |file: &std::path::Path| {
            from(&[
                ("RELAY_APNS_SANDBOX_KEY_ID", "SANDKEYID1"),
                ("RELAY_APNS_SANDBOX_TEAM_ID", "TEAMID1234"),
                ("RELAY_APNS_SANDBOX_TOPIC", "com.example.app"),
                ("RELAY_APNS_SANDBOX_KEY_FILE", file.to_str().unwrap()),
            ])
            .unwrap()
        };

        // The mistake this catches: a `.p8` converted to something else, or a
        // file that is simply not one.
        std::fs::write(&key_file, b"a file that exists").unwrap();
        let malformed = with_key(&key_file);
        assert!(malformed.sandbox.key().is_none());
        let why = malformed.sandbox.absence().expect("the key does not parse");
        assert!(why.contains("apns-sandbox.p8"), "{why}");
        assert!(why.contains("not a usable APNs signing key"), "{why}");
        assert!(
            !why.contains("cannot be read"),
            "a malformed key must not read as a missing one: {why}"
        );

        // And the file that is not there says the other thing.
        let missing = with_key(&dir.join("never-mounted.p8"));
        let why = missing.sandbox.absence().expect("the key is not there");
        assert!(why.contains("cannot be read"), "{why}");
        assert!(!why.contains("not a usable"), "{why}");

        // Neither message carries what was in the file.
        std::fs::write(&key_file, "wRoNgFiLe-32-bytes-of-key-materia").unwrap();
        let leaked = with_key(&key_file);
        let why = leaked.sandbox.absence().unwrap();
        assert!(!why.contains("wRoNgFiLe"), "{why}");

        // The key Apple hands you, unconverted, is the one that is ready.
        std::fs::write(&key_file, crate::apns::fake_apple::signing_key_file()).unwrap();
        assert_eq!(with_key(&key_file).sandbox.absence(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_generation_floor_is_zero_only_when_there_is_no_file() {
        let dir = scratch("generation-floor");
        let floor_file = dir.join("generation-floor");

        let config =
            from(&[("RELAY_GENERATION_FLOOR_FILE", floor_file.to_str().unwrap())]).unwrap();
        assert_eq!(config.generation_floor().unwrap(), 0);

        std::fs::write(&floor_file, "7\n").unwrap();
        assert_eq!(config.generation_floor().unwrap(), 7);

        std::fs::write(&floor_file, "not a number").unwrap();
        let err = config.generation_floor().unwrap_err().to_string();
        assert!(err.contains("generation-floor"), "{err}");
        assert!(err.contains("decimal whole number"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Every path this module reads is a secret file.** An operator who
    /// mounts the backup key where the floor belongs gets a message that says
    /// which file is wrong and does not put its contents in a log.
    #[test]
    fn an_unreadable_floor_is_described_by_its_shape_and_never_by_its_contents() {
        let dir = scratch("generation-floor-contents");
        let floor_file = dir.join("generation-floor");
        let config =
            from(&[("RELAY_GENERATION_FLOOR_FILE", floor_file.to_str().unwrap())]).unwrap();

        let mounted = "wRoNgFiLe-32-bytes-of-key-materia";
        std::fs::write(&floor_file, mounted).unwrap();
        let err = config.generation_floor().unwrap_err().to_string();
        assert!(!err.contains(mounted), "{err}");
        for len in 2..=mounted.len() {
            assert!(
                !err.contains(&mounted[..len]),
                "the message leaked the {len}-character prefix: {err}"
            );
        }
        // What it does say is which file, how big it is, and the first
        // character that is not a digit.
        assert!(err.contains("generation-floor"), "{err}");
        assert!(err.contains("33 bytes"), "{err}");
        assert!(err.contains(r"'w'"), "{err}");
        assert!(err.contains("decimal whole number"), "{err}");

        std::fs::write(&floor_file, "   \n").unwrap();
        let empty = config.generation_floor().unwrap_err().to_string();
        assert!(empty.contains("0 bytes"), "{empty}");
        assert!(empty.contains("empty"), "{empty}");

        // A number no `u64` can hold is a mounted file too, and is not printed
        // either.
        std::fs::write(&floor_file, "9".repeat(30)).unwrap();
        let huge = config.generation_floor().unwrap_err().to_string();
        assert!(huge.contains("30 bytes"), "{huge}");
        assert!(huge.contains("count to"), "{huge}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slot_is_looked_up_by_the_environment_a_binding_names() {
        let config = from(&[
            ("RELAY_APNS_SANDBOX_KEY_ID", "SANDKEYID1"),
            ("RELAY_APNS_SANDBOX_TEAM_ID", "TEAMID1234"),
            ("RELAY_APNS_SANDBOX_TOPIC", "com.example.app"),
        ])
        .unwrap();
        assert_eq!(
            config.slot(ApnsEnvironment::Sandbox),
            &config.sandbox,
            "the sandbox binding must not be answered with the production key"
        );
        assert_eq!(config.slot(ApnsEnvironment::Production), &config.production);
    }
}
