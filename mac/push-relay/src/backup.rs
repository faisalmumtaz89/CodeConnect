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
//! **A backup on the database's own disk is not a backup.** The plan asks for
//! the sealed copy to go to object storage, and the reason is the failure it is
//! for: the disk the relay's SQLite file lives on is the disk a platform
//! incident takes away, and a copy written beside it goes with it. So the sink
//! below is either a directory — right for a developer machine, where the
//! failure being rehearsed is a mistake and not a lost volume — or an
//! S3-compatible bucket, which is the one interface AWS S3, Cloudflare R2,
//! Backblaze B2 and MinIO all answer, and Render provides none of its own.
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{digest, hmac};
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
/// **A trait with the target in configuration, not a hardcoded directory.** A
/// developer machine keeps them in a directory and the deployment keeps them in
/// a bucket somewhere the relay's disk cannot take with it; the plan says
/// thirty-day retention either way. Written as a trait, the second sink is an
/// added implementation and a changed environment variable. Written as
/// `std::fs` calls inside the job, it would be a rewrite of the job.
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

/// The service name every S3 signature's credential scope carries.
const S3_SERVICE: &str = "s3";

/// The only signing algorithm S3 accepts on a header-signed request.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The last element of a credential scope, and the last step of the key
/// derivation. A literal in both places, because AWS defines it as one.
const TERMINATOR: &str = "aws4_request";

/// The credential every request to the object store is signed with.
///
/// **Not `Debug`, not `Clone` and not on any struct that is.** The secret
/// access key reads, overwrites and deletes every backup the relay has taken,
/// and a derived `Debug` is what puts it in the first structured log line
/// somebody adds to this module.
struct Signer {
    access_key_id: String,
    secret_access_key: String,
    region: String,
    /// `s3` in the relay. A field rather than the constant because AWS's own
    /// published test vectors sign for a service literally named `service`, and
    /// a signer that could not be pointed at it could not be checked against
    /// them.
    service: String,
}

/// **Signature Version 4, written here rather than taken from the AWS SDK.**
///
/// The relay needs four operations against one bucket. The AWS SDK for Rust is
/// a tree of crates carrying credential providers, a retry policy, an endpoint
/// resolver and a runtime of its own — none of which this job wants, and all of
/// which would have to be reviewed and patched on the relay's cadence. What is
/// actually needed is an HMAC-SHA256 chain over a canonical string, and `ring`
/// is already here computing the AES key above.
///
/// What that trades away is that a signing mistake is **silent**: a wrong
/// signature is a `403`, and a `403` every night is backups that never
/// happened. That is why the tests below sign AWS's own published examples and
/// compare the hex, rather than checking this implementation against itself.
impl Signer {
    /// The `Authorization` header for one request.
    ///
    /// The four steps are AWS's: a canonical request, a string to sign that
    /// binds it to a scope, a signing key derived from the secret and that same
    /// scope, and the HMAC of the two.
    fn authorization(
        &self,
        method: &str,
        canonical_uri: &str,
        canonical_query: &str,
        headers: &[(String, String)],
        payload_sha256: &str,
        stamp: &Stamp,
    ) -> String {
        let (canonical_headers, signed_headers) = canonical_headers(headers);
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n\
             {signed_headers}\n{payload_sha256}"
        );
        let scope = format!(
            "{}/{}/{}/{TERMINATOR}",
            stamp.date, self.region, self.service
        );
        let string_to_sign = format!(
            "{ALGORITHM}\n{}\n{scope}\n{}",
            stamp.instant,
            hex(sha256(canonical_request.as_bytes()).as_ref())
        );
        let signature = hex(self
            .signature(&stamp.date, string_to_sign.as_bytes())
            .as_ref());
        format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, \
             Signature={signature}",
            self.access_key_id
        )
    }

    /// The signing key, and the signature under it.
    ///
    /// **The key is derived per request and scoped to date, region and
    /// service.** That scoping is what makes a captured signature useless
    /// anywhere else: it is not the secret that signs, it is a key that only
    /// exists for one day, one region and one service.
    fn signature(&self, date: &str, string_to_sign: &[u8]) -> hmac::Tag {
        let seed = format!("AWS4{}", self.secret_access_key);
        let date_key = hmac_sha256(seed.as_bytes(), date.as_bytes());
        let region_key = hmac_sha256(date_key.as_ref(), self.region.as_bytes());
        let service_key = hmac_sha256(region_key.as_ref(), self.service.as_bytes());
        let signing_key = hmac_sha256(service_key.as_ref(), TERMINATOR.as_bytes());
        hmac_sha256(signing_key.as_ref(), string_to_sign)
    }
}

/// The one clock reading a request is signed under, in both forms the signature
/// uses: `20130524` for the credential scope and `20130524T000000Z` for
/// `x-amz-date` and the string to sign. The two must come from a single
/// reading — taken separately, a request signed across midnight carries a scope
/// for one day and a timestamp for the next, and the store refuses it.
struct Stamp {
    date: String,
    instant: String,
}

impl Stamp {
    fn now() -> Result<Stamp> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("the system clock is before 1970")?
            .as_secs();
        Ok(Stamp::at(secs as i64))
    }

    fn at(epoch_secs: i64) -> Stamp {
        let days = epoch_secs.div_euclid(86_400);
        let seconds = epoch_secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Stamp {
            date: format!("{year:04}{month:02}{day:02}"),
            instant: format!(
                "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
                seconds / 3_600,
                (seconds / 60) % 60,
                seconds % 60
            ),
        }
    }
}

/// The civil date a count of days since 1970-01-01 names.
///
/// Written out rather than pulled from a date library: `x-amz-date` is the only
/// calendar this relay has, and the vectors below check the arithmetic against
/// two dates AWS published.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The signed headers, in the two forms the canonical request needs.
///
/// Lower-cased, sorted by name, and every value trimmed with runs of spaces
/// collapsed — which is AWS's `Trimall`, and is the difference between a
/// signature the store recomputes and a `403`.
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), trim_all(value)))
        .collect();
    sorted.sort();
    let canonical = sorted
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed = sorted
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (canonical, signed)
}

/// AWS's `Trimall`: no leading or trailing space, and no run of spaces longer
/// than one.
fn trim_all(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut spaced = false;
    for character in value.trim().chars() {
        if character == ' ' {
            spaced = true;
            continue;
        }
        if spaced && !out.is_empty() {
            out.push(' ');
        }
        spaced = false;
        out.push(character);
    }
    out
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_LOWER[usize::from(byte >> 4)] as char);
        out.push(HEX_LOWER[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn sha256(bytes: &[u8]) -> digest::Digest {
    digest::digest(&digest::SHA256, bytes)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
}

/// Percent-encoding as the signature defines it: unreserved characters as
/// themselves, everything else as upper-case `%XY`.
///
/// **`/` is kept in a path and encoded in a query value.** A prefix is part of
/// the object's key and the store reads the slashes in it; the same slashes in
/// a continuation token are data, and one left literal there is a canonical
/// query string the store does not recompute.
///
/// S3 is the service AWS exempts from path normalization, so the key goes on
/// the wire as it is written and is encoded exactly once.
fn encode(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if keep_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push(HEX_UPPER[usize::from(byte >> 4)] as char);
                out.push(HEX_UPPER[usize::from(byte & 0x0f)] as char);
            }
        }
    }
    out
}

/// How much of a listing is read. One page of keys is a few kilobytes; the
/// bound is here so that a store answering with something endless cannot be
/// read into memory by a job that only ever wanted file names.
const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;

/// How much of one object is read. A backup is the whole database, so this is
/// sized by what a relay could plausibly hold rather than by a protocol.
const MAX_OBJECT_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// How many listing pages are followed before the walk is a fault. Each page
/// carries up to a thousand keys and retention holds about thirty backups, so
/// one page is the whole of a legitimate listing — the bound exists because a
/// store answering `IsTruncated` for ever, each page inside its own deadline,
/// would otherwise keep the job from ever returning, and with it every later
/// backup and the failure line that would have said so.
const MAX_LIST_PAGES: usize = 32;

/// How long one exchange with the object store may take, from the first packet
/// of the connection to the last byte of the answer.
///
/// **A bound is the difference between a failed backup and a stopped one.** A
/// store that accepts a connection and then says nothing — a half-open NAT, a
/// vendor incident, a security appliance holding the flow — leaves a blocking
/// read that no byte will ever complete, and the run behind it never returns:
/// nothing marks the state failed, `/readyz` keeps reporting backups on, and the
/// daily ticker never comes round again. Two minutes is far longer than a
/// working store needs for a database this size, and a store that has not
/// answered in two minutes is not about to.
const EXCHANGE_DEADLINE: Duration = Duration::from_secs(120);

/// How long the one exchange that carries a whole database may take.
///
/// **The restore is the one place where waiting beats giving up.** Every other
/// request is a few kilobytes of listing or the nightly upload, and a store that
/// is slow on those is a store to hear about; a `GET` of the sealed database is
/// bounded by [`MAX_OBJECT_BYTES`] rather than by anything about the protocol,
/// and it is run by a person restoring an instance who would far rather wait
/// than start again. This is that person's bound, not the job's.
const RESTORE_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// How long the TCP connection alone may take. [`EXCHANGE_DEADLINE`] already
/// covers it; this is the shorter bound for the one failure that is only ever a
/// dead address, so a store whose port is not answering is reported in ten
/// seconds rather than in two minutes. Name resolution is outside it — that one
/// is the exchange deadline's.
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);

/// A bucket at an S3-compatible store.
///
/// **Written against S3's API rather than one vendor's.** It is the interface
/// AWS S3, Cloudflare R2, Backblaze B2 and MinIO all answer, and Render — where
/// this relay runs — offers no object storage of its own, so the deployment's
/// store is chosen after the code is written and not before.
struct ObjectStore {
    /// The verified trust anchors, parsed once at startup.
    ///
    /// **The client is built per request and this is not.** Each request runs
    /// on a runtime of its own (see [`ObjectStore::send`]), and a pooled client
    /// outliving the runtime that opened its connections is a pool holding
    /// handles to a dead reactor. Reading and validating the platform trust
    /// store is the expensive half and it happens once; building a connector
    /// around an `Arc` of the result costs nothing.
    tls: Arc<rustls::ClientConfig>,
    address: Address,
    bucket: String,
    /// The key prefix, empty or ending in `/`.
    prefix: String,
    signer: Signer,
    /// A field rather than the constant so a test can prove the bound in
    /// milliseconds instead of waiting the two minutes a real one allows.
    deadline: Duration,
}

/// Where one bucket is addressed, and in which of S3's two URL styles.
///
/// **AWS is addressed by virtual host and everything else by path.** AWS has
/// been retiring path-style addressing since 2020 and new buckets may not
/// answer it at all, while R2, B2 and MinIO all serve path-style at a host that
/// says nothing about the bucket. So the presence of a configured endpoint is
/// what decides: no endpoint means AWS and `bucket.s3.region.amazonaws.com`, an
/// endpoint means that host with the bucket as the first path segment.
struct Address {
    scheme: String,
    /// The `host[:port]` that is signed as `host` and dialled.
    host: String,
    /// What every key hangs under: empty for a virtual-hosted bucket, `/bucket`
    /// for a path-style endpoint.
    root: String,
}

impl Address {
    fn resolve(config: &RelayConfig, bucket: &str) -> Result<Address> {
        let Some(endpoint) = &config.backup_s3_endpoint else {
            return Ok(Address {
                scheme: "https".to_string(),
                host: format!("{bucket}.s3.{}.amazonaws.com", config.backup_s3_region),
                root: String::new(),
            });
        };
        let uri: http::Uri = endpoint.parse().with_context(|| {
            format!("RELAY_BACKUP_S3_ENDPOINT is {endpoint:?}, which is not a URL")
        })?;
        let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
            bail!(
                "RELAY_BACKUP_S3_ENDPOINT is {endpoint:?}; it is a scheme and a host, \
                 for example https://s3.us-east-1.amazonaws.com"
            );
        };
        if !matches!(uri.path(), "" | "/") {
            bail!(
                "RELAY_BACKUP_S3_ENDPOINT is {endpoint:?}; it is the store's address and the \
                 bucket comes from RELAY_BACKUP_TARGET, so it carries no path"
            );
        }
        // **Plaintext is refused here and not left to the connector.** The
        // sealed bytes are already ciphertext, but the request around them
        // carries the access key id and a signature that anyone on the path can
        // replay for as long as the store's clock skew allows — and that
        // credential deletes every backup this relay has. The test build allows
        // `http` because the fake store below is a plaintext socket on
        // localhost, which is the only way to read the request the store
        // actually receives.
        if !cfg!(test) && scheme != "https" {
            bail!(
                "RELAY_BACKUP_S3_ENDPOINT is {endpoint:?}; it is `https`, because the request \
                 carries a credential that can delete every backup this relay has taken"
            );
        }
        Ok(Address {
            scheme: scheme.to_string(),
            host: authority.to_string(),
            root: format!("/{bucket}"),
        })
    }
}

impl ObjectStore {
    fn new(config: &RelayConfig, location: &str) -> Result<ObjectStore> {
        let (bucket, prefix) = split_location(location)?;
        let access_key_id = config.backup_s3_access_key_id.clone().context(
            "RELAY_BACKUP_S3_ACCESS_KEY_ID is not set, and an `s3://` backup target cannot be \
             written without it",
        )?;
        let secret_access_key = read_secret_access_key(&config.backup_s3_secret_key_file)?;
        Ok(ObjectStore {
            tls: Arc::new(trusted_tls()?),
            address: Address::resolve(config, &bucket)?,
            bucket,
            prefix,
            signer: Signer {
                access_key_id,
                secret_access_key,
                region: config.backup_s3_region.clone(),
                service: S3_SERVICE.to_string(),
            },
            deadline: EXCHANGE_DEADLINE,
        })
    }

    #[cfg(test)]
    fn with_deadline(mut self, deadline: Duration) -> ObjectStore {
        self.deadline = deadline;
        self
    }

    /// The full object key one backup name lands under.
    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// One signed request, and the store's answer.
    ///
    /// **A thread and a runtime of its own, because [`BackupSink`] is
    /// synchronous.** The job that calls it is already off the reactor on
    /// `spawn_blocking` — an upload of a whole database must not sit on a
    /// worker that is meant to be answering pushes — and a restore is run from
    /// a context with no runtime at all. A runtime created and dropped on a
    /// thread that is inside neither is the one arrangement that is correct in
    /// both places; a runtime owned by this struct would panic when a caller
    /// inside an async context dropped it.
    fn send(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
        limit: usize,
        deadline: Duration,
    ) -> Result<Vec<u8>> {
        let path = if key.is_empty() {
            if self.address.root.is_empty() {
                "/".to_string()
            } else {
                self.address.root.clone()
            }
        } else {
            format!("{}/{}", self.address.root, encode(key, true))
        };
        let mut pairs: Vec<(String, String)> = query
            .iter()
            .map(|(name, value)| (encode(name, false), encode(value, false)))
            .collect();
        pairs.sort();
        let canonical_query = pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");

        let payload_sha256 = hex(sha256(&body).as_ref());
        let stamp = Stamp::now()?;
        // Exactly the three headers that are signed, and the ones the store
        // checks: `host` is what stops a signature for one bucket being
        // replayed against another, and `x-amz-content-sha256` is what stops
        // the body being swapped under a signature that covers only the
        // headers.
        let headers = [
            ("host".to_string(), self.address.host.clone()),
            ("x-amz-content-sha256".to_string(), payload_sha256.clone()),
            ("x-amz-date".to_string(), stamp.instant.clone()),
        ];
        let authorization = self.signer.authorization(
            method,
            &path,
            &canonical_query,
            &headers,
            &payload_sha256,
            &stamp,
        );
        let uri = if canonical_query.is_empty() {
            format!("{}://{}{path}", self.address.scheme, self.address.host)
        } else {
            format!(
                "{}://{}{path}?{canonical_query}",
                self.address.scheme, self.address.host
            )
        };

        let request = http::Request::builder()
            .method(method)
            .uri(&uri)
            .header("host", &self.address.host)
            .header("x-amz-content-sha256", &payload_sha256)
            .header("x-amz-date", &stamp.instant)
            .header("authorization", authorization)
            .body(Full::new(Bytes::from(body)))
            .context("building the object store request")?;

        let tls = Arc::clone(&self.tls);
        let request_path = path.clone();
        let exchange = std::thread::scope(|scope| {
            scope
                .spawn(move || -> Result<(http::StatusCode, Vec<u8>)> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .context("the object store's runtime")?;
                    let answered = runtime.block_on(async move {
                        let exchange = async {
                            let response = client(tls)
                                .request(request)
                                .await
                                .with_context(|| format!("{method} {request_path}"))?;
                            let status = response.status();
                            let body = Limited::new(response.into_body(), limit)
                                .collect()
                                .await
                                .map_err(|e| {
                                    anyhow::anyhow!("reading the object store's answer: {e}")
                                })?
                                .to_bytes();
                            Ok((status, body.to_vec()))
                        };
                        // **Inside the runtime, so the thread ends.** A deadline
                        // waited on outside this `block_on` would be a second
                        // thread learning that the first one is stuck, and the
                        // stuck one would still be holding the scope open.
                        match tokio::time::timeout(deadline, exchange).await {
                            Ok(answer) => answer,
                            Err(_) => bail!(
                                "the object store did not answer {method} {request_path} \
                                 within {deadline:?}"
                            ),
                        }
                    });
                    // **Abandoned rather than dropped**, because dropping a
                    // runtime waits for its blocking pool and name resolution
                    // runs there: a `getaddrinfo` that never returns would hold
                    // this thread open long after the deadline had given up on
                    // it, which is the hang the deadline was for. The resolver
                    // thread itself is not cancellable and lives on in a
                    // detached pool until the OS gives up on it — one per timed
                    // out request on a daily job — but nothing waits for it.
                    runtime.shutdown_timeout(Duration::ZERO);
                    answered
                })
                .join()
        })
        .map_err(|_| anyhow::anyhow!("the object store request did not finish"))?;

        let (status, body) = exchange?;
        if !status.is_success() {
            // **The store's own message is not repeated.** An S3 error document
            // quotes the request back, which includes the access key id and the
            // canonical request; the fault code is what an operator acts on and
            // is the whole of what a log needs.
            bail!(
                "the object store answered {} to {method} {path}{}",
                status.as_u16(),
                fault(&body)
            );
        }
        Ok(body)
    }
}

impl BackupSink for ObjectStore {
    /// **No temporary name and no rename.** The directory sink writes a partial
    /// file and renames it because a process killed mid-write would otherwise
    /// leave half a backup that a restore would read; an object appears at its
    /// key only once the store has the whole body, so an interrupted upload is
    /// an object that never existed.
    fn put(&self, name: &str, sealed: &[u8]) -> Result<()> {
        self.send(
            "PUT",
            &self.key(name),
            &[],
            sealed.to_vec(),
            MAX_LISTING_BYTES,
            self.deadline,
        )?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut resume: Option<String> = None;
        let mut complete = false;
        for _page in 0..MAX_LIST_PAGES {
            let mut query = vec![("list-type", "2"), ("prefix", self.prefix.as_str())];
            if let Some(token) = resume.as_deref() {
                query.push(("continuation-token", token));
            }
            let body = self.send(
                "GET",
                "",
                &query,
                Vec::new(),
                MAX_LISTING_BYTES,
                self.deadline,
            )?;
            let listing = String::from_utf8_lossy(&body).into_owned();
            names.extend(
                elements(&listing, "Key")
                    .into_iter()
                    .filter_map(|key| key.strip_prefix(self.prefix.as_str()).map(str::to_string))
                    .filter(|name| taken_at(name).is_some()),
            );
            // **A listing is paged, and the prune reads every page.** A store
            // that answered the first page only would leave everything past it
            // undeleted, so the thirty-day window would quietly become "the
            // thirty days that fit in one page".
            if elements(&listing, "IsTruncated")
                .first()
                .map(String::as_str)
                != Some("true")
            {
                complete = true;
                break;
            }
            let Some(token) = elements(&listing, "NextContinuationToken")
                .into_iter()
                .next()
            else {
                complete = true;
                break;
            };
            resume = Some(token);
        }
        // A walk that never ends is a fault, never a longer listing: acting on
        // what was gathered would delete against a list the store never
        // finished telling, and returning quietly would hide the fault the
        // failure line exists to record.
        if !complete {
            bail!(
                "the listing was still truncated after {MAX_LIST_PAGES} pages; \
                 retention holds about thirty backups, so this walk is a fault \
                 in the store, not a longer listing"
            );
        }
        names.sort();
        Ok(names)
    }

    fn get(&self, name: &str) -> Result<Vec<u8>> {
        self.send(
            "GET",
            &self.key(name),
            &[],
            Vec::new(),
            MAX_OBJECT_BYTES,
            RESTORE_DEADLINE,
        )
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.send(
            "DELETE",
            &self.key(name),
            &[],
            Vec::new(),
            MAX_LISTING_BYTES,
            self.deadline,
        )?;
        Ok(())
    }

    fn describe(&self) -> String {
        format!(
            "bucket {}/{} at {}",
            self.bucket, self.prefix, self.address.host
        )
    }
}

/// The bucket and the key prefix a target's `s3://` body names.
fn split_location(location: &str) -> Result<(String, String)> {
    let (bucket, prefix) = location.split_once('/').unwrap_or((location, ""));
    if bucket.is_empty() {
        bail!("the backup target names no bucket; it is `s3://bucket/optional/prefix`");
    }
    let prefix = prefix.trim_matches('/');
    Ok((
        bucket.to_string(),
        if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        },
    ))
}

/// The secret access key, read as one line from a mounted secret file.
///
/// **A file and never a variable**, for the reason the module header gives for
/// every other secret here: a variable is printed by a process dump, inherited
/// by every child, and shown by the deployment dashboard to anyone who can read
/// the service. This one is the credential that can delete every backup the
/// relay has taken, which after a disk loss is the entire state.
///
/// Absent or empty is an error, not a store that signs with nothing: a relay
/// that started with no credential would answer `403` every night and the
/// operator would find out on the morning of a restore.
fn read_secret_access_key(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading the backup object store's secret access key at {}",
            path.display()
        )
    })?;
    let key = raw.trim().to_string();
    if key.is_empty() {
        bail!(
            "the backup object store's secret access key at {} is empty",
            path.display()
        );
    }
    Ok(key)
}

/// The fault code out of an S3 error document, if it says one.
fn fault(body: &[u8]) -> String {
    match elements(&String::from_utf8_lossy(body), "Code")
        .into_iter()
        .next()
    {
        Some(code) => format!(" ({code})"),
        None => String::new(),
    }
}

/// Every value of one element in the store's XML answer.
///
/// **A scan and not a parser.** The listing is the only XML this relay ever
/// reads, and the three elements it needs — a key, a truncation flag, a
/// continuation token — are element text with no attributes and no namespaces.
/// Everything the scan cannot understand is a name [`taken_at`] rejects, so a
/// key this relay did not write is skipped rather than acted on.
fn elements(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        found.push(after[..end].to_string());
        rest = &after[end + close.len()..];
    }
    found
}

/// The TLS configuration every object-store request is made under.
///
/// The platform trust store, exactly as [`crate::apns`] does it and for the
/// same reason: every store this can address presents a certificate chaining to
/// a public root, and vendoring a root set would be a second thing to keep
/// current. A trust store that will not load is a broken image, and it is
/// reported here rather than at the first upload.
fn trusted_tls() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    let (added, _ignored) =
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if added == 0 {
        bail!("no trust anchors available for the backup object store connection");
    }
    // Named rather than taken from the process default, which is installed by
    // whichever crate got there first.
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("the backup object store's TLS configuration")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    // **HTTP/1.1 in ALPN, and not `h2`.** S3's REST API is HTTP/1.1 and AWS
    // offers no `h2` for it; a client that announced only `h2` would meet a
    // server that selects nothing and, on the strict ones, an aborted
    // handshake. Announcing the version that is actually spoken costs one
    // round trip a day and works at every store this can address.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// A client for one request.
///
/// `enforce_http(false)` because the connector is handed an `https` address;
/// the scheme itself was decided at startup by [`Address::resolve`], which is
/// the only place a target can name one.
fn client(
    tls: Arc<rustls::ClientConfig>,
) -> Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(CONNECT_DEADLINE));
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(hyper_rustls::HttpsConnector::from((http, tls)))
}

/// The sink a target names, or a refusal that says which schemes exist.
///
/// **Everything a sink needs is decided here, at startup.** A scheme this build
/// cannot serve, a bucket with no credential, a secret file that is not
/// mounted, an endpoint that is not a URL — each of them is an error the relay
/// reports the moment it boots rather than a backup that silently never
/// happens, which is a state nobody discovers until the morning they need a
/// restore.
pub fn sink(config: &RelayConfig) -> Result<Box<dyn BackupSink>> {
    match config.backup_target.split_once("://") {
        Some(("file", path)) if !path.is_empty() => Ok(Box::new(Directory(PathBuf::from(path)))),
        Some(("s3", location)) if !location.is_empty() => {
            Ok(Box::new(ObjectStore::new(config, location)?))
        }
        _ => bail!(
            "the backup target {:?} is not supported; it is `s3://bucket/optional/prefix` \
             or `file:///path`",
            config.backup_target
        ),
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
            sink: sink(config)?,
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
    ///
    /// **The two halves are separate functions because the daily job has to
    /// hold the database for one of them and must not hold it for the other** —
    /// see `run_once`, which is the only caller in the service. This
    /// composition is the one-shot form, for a caller that already owns the
    /// connection and has nobody waiting behind it.
    pub fn run(&self, live: &Connection, now_ms: i64) -> Result<Completed> {
        self.store(self.snapshot(live)?, now_ms)
    }

    /// Seal the copy, put it, and prune what the window has expired — none of
    /// which reads the database.
    ///
    /// **The prune runs whether or not the store worked.** The failure this job
    /// most has to survive is a full disk, and the store is what fails on one
    /// while the prune is what frees the space — so a `?` between them would
    /// make the job disable its own recovery on the night it is needed, and
    /// every night after. The store's error is still what the run reports,
    /// because it is the one an operator has to act on.
    fn store(&self, plain: Vec<u8>, now_ms: i64) -> Result<Completed> {
        let name = format!("{NAME_PREFIX}{now_ms:0NAME_DIGITS$}{NAME_SUFFIX}");
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
            let now = crate::enroll::now_ms();
            let outcome = run_once(Arc::clone(&job), Arc::clone(&db), now).await;
            record(&state, now, outcome);
        }
    });
}

/// One run, with the one database lock this process has held for the copy and
/// for nothing else.
///
/// **The lock ends where the network begins.** The relay has a single
/// `Connection` behind a single mutex, so whoever holds it holds every push,
/// every enrollment and every status request in the process. The copy needs it;
/// sealing a `Vec<u8>`, uploading it, listing a bucket and deleting last month's
/// objects do not — and those are the steps that talk to somebody else's
/// service. Held across them, one stalled store is a total outage of the push
/// path for as long as the stall lasts, which is a backup taking down the thing
/// it exists to protect.
///
/// So the copy is one `spawn_blocking` whose guard dies with it, and the rest is
/// another that never sees the connection at all.
async fn run_once(
    job: Arc<Job>,
    db: Arc<std::sync::Mutex<Connection>>,
    now_ms: i64,
) -> Result<Completed> {
    // **`spawn_blocking`, because the copy is disk work.** Run on a runtime
    // worker it would stall every request the process is serving for as long as
    // the database takes to read.
    let copied = {
        let job = Arc::clone(&job);
        tokio::task::spawn_blocking(move || {
            let live = db.lock().unwrap_or_else(|e| e.into_inner());
            job.snapshot(&live)
        })
        .await
        .context("the backup copy did not finish")??
    };
    // Blocking again rather than inline: the upload is a synchronous sink, and
    // the point of the whole arrangement is that no reactor worker waits on it.
    tokio::task::spawn_blocking(move || job.store(copied, now_ms))
        .await
        .context("the backup upload did not finish")?
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
            // The namespace the seeded installation was attested in. A relay
            // configured for the other one refuses its bearer, which is the
            // point of the namespace and not the subject of these tests.
            ("RELAY_ATTEST_ENVIRONMENT".into(), "production".into()),
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

    fn authorization(bearer: &Secret) -> String {
        format!("Bearer {}", bearer.expose())
    }

    async fn credential_status(relay: Relay, authorization: String) -> serde_json::Value {
        let response = router(relay)
            .oneshot(
                Request::builder()
                    .uri("/v1/credential/status")
                    .header("authorization", authorization)
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
        let dir = scratch("sink-schemes");
        // A blank target is not here: blank-means-absent turns it into the
        // default before `sink` ever sees it.
        for target in [
            "https://example.invalid/backups",
            "/var/data/backups",
            "file://",
        ] {
            let err = sink(&config(&dir, &[("RELAY_BACKUP_TARGET", target)]))
                .err()
                .unwrap_or_else(|| panic!("accepted {target:?}"))
                .to_string();
            assert!(err.contains("file:///path"), "{err}");
        }
        // An `s3://` target is supported, so it is refused for what is
        // actually missing — the credentials — not as an unknown scheme.
        let err = sink(&config(
            &dir,
            &[("RELAY_BACKUP_TARGET", "s3://codeconnect-relay-backups")],
        ))
        .err()
        .expect("an s3 target without credentials must be refused")
        .to_string();
        assert!(err.contains("RELAY_BACKUP_S3_ACCESS_KEY_ID"), "{err}");
        assert!(sink(&config(
            &dir,
            &[("RELAY_BACKUP_TARGET", "file:///var/data/backups")]
        ))
        .is_ok());
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
            credential_status(relay.clone(), authorization(&bearer)).await["status"],
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
            credential_status(relay.clone(), authorization(&bearer)).await["status"],
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

    // -----------------------------------------------------------------------
    // The database lock, and what a stalled store may not take with it.
    // -----------------------------------------------------------------------

    /// **A store that stops answering must not stop the relay.** The process has
    /// one SQLite connection behind one mutex, so anything holding it holds
    /// every push and every enrollment; the upload is a request to somebody
    /// else's service and can hang for as long as that service is unwell. Held
    /// across the upload, the nightly backup would be a scheduled outage of the
    /// push path whenever the store had a bad night.
    ///
    /// Two worker threads because the failure this proves is a *blocked* one: if
    /// the lock were still held, the request below would block the thread it is
    /// polled on, and a deadline sharing that thread would never fire — the test
    /// would hang instead of failing. The deadline gets a thread of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_store_does_not_hold_the_database_lock() {
        let dir = scratch("stalled-store");
        let config = config(&dir, &[]);
        let (live, bearer) = seeded(&config.db_path);
        let sink = Arc::new(Stalling::default());
        let job = Arc::new(Job {
            key: read_key(&config.backup_key_file).unwrap(),
            sink: Box::new(Arc::clone(&sink)),
            db_path: config.db_path.clone(),
            retention_days: 30,
        });
        let relay = Relay::new(config, live);

        let running = tokio::spawn(run_once(job, Arc::clone(&relay.db), NOW));
        // The upload has been entered, which is only true once the copy has
        // finished — and the copy is the only step that holds the connection.
        sink.entered().await;

        // So the relay is still serving, on the very connection the copy read.
        let answering = tokio::spawn(credential_status(relay.clone(), authorization(&bearer)));
        let answered = tokio::time::timeout(Duration::from_secs(10), answering).await;
        // Let go before asserting: a runtime is dropped only once its blocking
        // tasks return, so a failure that left the sink stalled would wedge the
        // teardown and hide itself behind a hung test.
        sink.release();
        let answered = answered
            .expect("the stalled store was still holding the database lock")
            .unwrap();
        assert_eq!(answered["status"], "active");

        let completed = running.await.unwrap().expect("the run finished");
        assert_eq!(completed.name, format!("relay-{NOW}.sqlite.enc"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sink whose `put` arrives and does not come back until it is let go.
    #[derive(Default)]
    struct Stalling {
        arrived: tokio::sync::Notify,
        released: std::sync::Mutex<bool>,
        wake: std::sync::Condvar,
        stored: std::sync::Mutex<Vec<String>>,
    }

    impl Stalling {
        async fn entered(&self) {
            self.arrived.notified().await;
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    impl BackupSink for Arc<Stalling> {
        fn put(&self, name: &str, _sealed: &[u8]) -> Result<()> {
            self.stored.lock().unwrap().push(name.to_string());
            self.arrived.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
            Ok(())
        }

        fn list(&self) -> Result<Vec<String>> {
            Ok(self.stored.lock().unwrap().clone())
        }

        fn get(&self, _name: &str) -> Result<Vec<u8>> {
            bail!("a stalling sink is never read")
        }

        fn delete(&self, _name: &str) -> Result<()> {
            Ok(())
        }

        fn describe(&self) -> String {
            "stalling".into()
        }
    }

    // -----------------------------------------------------------------------
    // The wire, against a store on localhost.
    // -----------------------------------------------------------------------

    /// The credentials AWS publishes its signing test suite under. They are
    /// examples, they authorize nothing, and they are here so the arithmetic is
    /// checked against somebody else's answer.
    const EXAMPLE_KEY_ID: &str = "AKIDEXAMPLE";
    const EXAMPLE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// **A signing mistake is silent.** A wrong signature is a `403`, and a
    /// `403` every night is backups that never happened — discovered on the one
    /// morning a restore is needed. So the signer is checked against the
    /// signature AWS publishes for its own `get-vanilla` example rather than
    /// against itself, and the clock that produces the credential scope is
    /// checked with it: a signature scoped to the wrong day is refused exactly
    /// like a signature computed wrongly.
    #[test]
    fn the_signer_reproduces_the_signature_aws_publishes_for_its_own_example() {
        let stamp = Stamp::at(1_440_938_160);
        assert_eq!(stamp.date, "20150830");
        assert_eq!(stamp.instant, "20150830T123600Z");

        let signer = Signer {
            access_key_id: EXAMPLE_KEY_ID.to_string(),
            secret_access_key: EXAMPLE_SECRET.to_string(),
            region: "us-east-1".to_string(),
            // The suite signs for a service literally named `service`, which is
            // the whole reason this field is not the `s3` constant.
            service: "service".to_string(),
        };
        let headers = [
            ("host".to_string(), "example.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), stamp.instant.clone()),
        ];

        assert_eq!(
            signer.authorization("GET", "/", "", &headers, EMPTY_SHA256, &stamp),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// **What is signed has to be what is sent.** The vector above proves the
    /// arithmetic; this proves the inputs, by rebuilding the canonical request
    /// out of the bytes the store actually received and re-deriving the
    /// signature from them. A key signed under one path and sent to another, a
    /// body swapped after signing, a header left out of the signed set — each of
    /// them is a `403` from a real store and a failure here.
    #[tokio::test]
    async fn the_store_receives_a_signed_put_for_the_key_the_backup_names() {
        let dir = scratch("s3-put");
        let (endpoint, ledger) = store(vec![Answer::Ok(String::new())]).await;
        let store = object_store(&dir, &endpoint);
        let sealed = b"sealed backup bytes".to_vec();

        blocking(move || store.put("relay-1700000000000.sqlite.enc", &sealed))
            .await
            .expect("the store accepted the object");

        let seen = ledger.taken();
        let [request] = &seen[..] else {
            panic!("one request, not {}", seen.len())
        };
        assert_eq!(request.method, "PUT");
        assert_eq!(
            request.target,
            "/codeconnect-relay-backups/relay/relay-1700000000000.sqlite.enc"
        );
        assert_eq!(request.body, b"sealed backup bytes");
        assert_eq!(
            request.header("x-amz-content-sha256"),
            hex(sha256(b"sealed backup bytes").as_ref()),
            "the body hash is what stops the bytes being swapped under the signature"
        );
        verify(request);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A walk that never ends is a fault, not a longer listing.** Every page
    /// answers inside its own deadline, so without a bound on the pages a store
    /// answering `IsTruncated` for ever — quickly, politely, each page well
    /// formed — would keep the job from ever returning, and with it every
    /// later backup and the failure line that would have said so.
    #[tokio::test]
    async fn a_listing_that_never_stops_being_truncated_is_a_fault() {
        let dir = scratch("s3-endless");
        let (endpoint, ledger) = store(
            (0..MAX_LIST_PAGES)
                .map(|_| Answer::Ok(listing(&["relay-1700000000000.sqlite.enc"], Some("t"))))
                .collect(),
        )
        .await;
        let store = object_store(&dir, &endpoint);

        let err = blocking(move || store.list())
            .await
            .expect_err("an endless listing must be refused")
            .to_string();
        assert!(err.contains("still truncated"), "{err}");
        assert_eq!(
            ledger.taken().len(),
            MAX_LIST_PAGES,
            "one request per page, then the fault"
        );
    }

    /// **The prune reads every page or the window is a lie.** A store that
    /// answers the first page only would leave everything past it undeleted, and
    /// thirty-day retention would quietly become "the thirty days that fit in
    /// one page". The token is carried in the query, so it is also inside the
    /// signature — a token appended after signing is a `403`.
    #[tokio::test]
    async fn a_truncated_listing_is_followed_to_its_last_page() {
        let dir = scratch("s3-pages");
        // A token with the three characters S3 actually puts in one and SigV4
        // requires encoded, so the encoding is exercised rather than assumed.
        let token = "1/abc+def=";
        let (endpoint, ledger) = store(vec![
            Answer::Ok(listing(&["relay-1700000000000.sqlite.enc"], Some(token))),
            Answer::Ok(listing(&["relay-1700086400000.sqlite.enc"], None)),
        ])
        .await;
        let store = object_store(&dir, &endpoint);

        let names = blocking(move || store.list()).await.expect("the listing");
        assert_eq!(
            names,
            [
                "relay-1700000000000.sqlite.enc",
                "relay-1700086400000.sqlite.enc"
            ],
            "the second page was dropped"
        );

        let seen = ledger.taken();
        assert_eq!(seen.len(), 2, "one request per page");
        assert!(
            seen[0].target.starts_with("/codeconnect-relay-backups?"),
            "{}",
            seen[0].target
        );
        assert!(
            !seen[0].target.contains("continuation-token"),
            "the first page asks for no continuation: {}",
            seen[0].target
        );
        assert!(
            seen[1]
                .target
                .contains("continuation-token=1%2Fabc%2Bdef%3D"),
            "the second page must resume from the token, encoded: {}",
            seen[1].target
        );
        for request in &seen {
            assert!(request.target.contains("list-type=2"), "{}", request.target);
            assert!(
                request.target.contains("prefix=relay%2F"),
                "{}",
                request.target
            );
            verify(request);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Retention deletes what the window expired and nothing else.** The
    /// listing here carries one object from inside the window, two from outside
    /// it, and one name this relay never wrote — and the store must be asked to
    /// delete exactly the two.
    #[tokio::test]
    async fn retention_deletes_only_the_objects_past_the_window() {
        let dir = scratch("s3-retention");
        let kept = format!("relay-{}.sqlite.enc", NOW - 29 * DAY_MS);
        let expired = [
            format!("relay-{}.sqlite.enc", NOW - 31 * DAY_MS),
            format!("relay-{}.sqlite.enc", NOW - 400 * DAY_MS),
        ];
        let names = [
            expired[1].as_str(),
            expired[0].as_str(),
            kept.as_str(),
            "notes.txt",
        ];
        let (endpoint, ledger) = store(vec![
            Answer::Ok(listing(&names, None)),
            Answer::Ok(String::new()),
            Answer::Ok(String::new()),
        ])
        .await;
        let config = config(&dir, &[]);
        let job = Job {
            key: read_key(&config.backup_key_file).unwrap(),
            sink: Box::new(object_store(&dir, &endpoint)),
            db_path: config.db_path.clone(),
            retention_days: 30,
        };

        let pruned = blocking(move || job.prune(NOW)).await.expect("the prune");
        assert_eq!(pruned, 2);

        let deleted: Vec<String> = ledger
            .taken()
            .iter()
            .filter(|request| request.method == "DELETE")
            .map(|request| request.target.clone())
            .collect();
        assert_eq!(
            deleted,
            [
                format!("/codeconnect-relay-backups/relay/{}", expired[1]),
                format!("/codeconnect-relay-backups/relay/{}", expired[0]),
            ],
            "only the objects the window expired, and the one it kept is not here"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A store that never answers is a failed backup, not a stopped relay.**
    /// Unbounded, this is the worst failure the job has: the run never returns,
    /// so nothing marks the state failed, `/readyz` goes on reporting backups
    /// enabled, and the daily ticker behind it never comes round again. Bounded,
    /// it is one bad night that says so.
    #[tokio::test]
    async fn a_store_that_never_answers_is_a_failed_backup_and_not_a_hang() {
        let dir = scratch("s3-silent");
        let (endpoint, _ledger) = store(vec![Answer::Silence]).await;
        let store = object_store(&dir, &endpoint).with_deadline(Duration::from_millis(300));

        let started = std::time::Instant::now();
        let failed = blocking(move || store.put("relay-1700000000000.sqlite.enc", b"sealed"))
            .await
            .expect_err("a store that says nothing cannot have stored anything");
        let elapsed = started.elapsed();

        assert!(
            format!("{failed:#}").contains("did not answer"),
            "{failed:#}"
        );
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");

        // And that failure is what `/readyz` and the metric are reading.
        let relay = Relay::new(config(&dir, &[]), crate::db::open_in_memory().unwrap());
        record(&relay.backup, NOW, Err(failed));
        assert_eq!(readyz(relay.clone()).await["backups_enabled"], false);
        assert!(metrics_text(relay)
            .await
            .contains("codeconnect_relay_backups_enabled 0"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The synchronous sink, called where the job calls it: off the reactor, so
    /// the store answering on this runtime is not waiting behind it.
    async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(work).await.unwrap()
    }

    /// A store pointed at a fake, under the example credentials.
    fn object_store(dir: &Path, endpoint: &str) -> ObjectStore {
        let secret_file = dir.join("backup-s3-secret-key");
        std::fs::write(&secret_file, format!("{EXAMPLE_SECRET}\n")).unwrap();
        let config = config(
            dir,
            &[
                (
                    "RELAY_BACKUP_TARGET",
                    "s3://codeconnect-relay-backups/relay",
                ),
                ("RELAY_BACKUP_S3_ENDPOINT", endpoint),
                ("RELAY_BACKUP_S3_ACCESS_KEY_ID", EXAMPLE_KEY_ID),
                (
                    "RELAY_BACKUP_S3_SECRET_KEY_FILE",
                    &secret_file.to_string_lossy(),
                ),
            ],
        );
        ObjectStore::new(&config, "codeconnect-relay-backups/relay").unwrap()
    }

    /// One `ListObjectsV2` answer, in the shape S3 sends it.
    fn listing(names: &[&str], next: Option<&str>) -> String {
        let contents: String = names
            .iter()
            .map(|name| format!("<Contents><Key>relay/{name}</Key></Contents>"))
            .collect();
        let (truncated, token) = match next {
            Some(token) => (
                "true",
                format!("<NextContinuationToken>{token}</NextContinuationToken>"),
            ),
            None => ("false", String::new()),
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>\
             <Name>codeconnect-relay-backups</Name><Prefix>relay/</Prefix>\
             <IsTruncated>{truncated}</IsTruncated>{contents}{token}</ListBucketResult>"
        )
    }

    /// **The signature, re-derived here from the request as it arrived.**
    ///
    /// Written out rather than handed to [`Signer`], which would only prove that
    /// the signer agrees with itself. What it checks is that the canonical
    /// request the store would build from these bytes — this method, this path,
    /// this query, these headers, this body — is the one that was signed.
    fn verify(request: &Seen) {
        let authorization = request.header("authorization");
        let date = &request.header("x-amz-date")[..8];
        let scope = format!("{date}/us-east-1/s3/aws4_request");
        let signed = "host;x-amz-content-sha256;x-amz-date";
        let prefix = format!(
            "AWS4-HMAC-SHA256 Credential={EXAMPLE_KEY_ID}/{scope}, \
             SignedHeaders={signed}, Signature="
        );
        let signature = authorization
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("{authorization:?} does not begin {prefix:?}"));
        assert_eq!(signature.len(), 64, "{signature}");
        assert!(
            signature
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{signature}"
        );

        let (path, query) = request
            .target
            .split_once('?')
            .unwrap_or((request.target.as_str(), ""));
        let canonical = format!(
            "{}\n{path}\n{query}\nhost:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n\n{signed}\n{}",
            request.method,
            request.header("host"),
            request.header("x-amz-content-sha256"),
            request.header("x-amz-date"),
            hex(sha256(&request.body).as_ref()),
        );
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
            request.header("x-amz-date"),
            hex(sha256(canonical.as_bytes()).as_ref())
        );
        let mut key = hmac_sha256(format!("AWS4{EXAMPLE_SECRET}").as_bytes(), date.as_bytes());
        for part in ["us-east-1", "s3", "aws4_request"] {
            key = hmac_sha256(key.as_ref(), part.as_bytes());
        }
        assert_eq!(
            hex(hmac_sha256(key.as_ref(), to_sign.as_bytes()).as_ref()),
            signature,
            "the signature does not cover the request that was sent:\n{canonical}"
        );
    }

    /// One request, exactly as it arrived.
    struct Seen {
        method: String,
        /// Path and query as written on the request line.
        target: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> &str {
            self.headers
                .iter()
                .find(|(found, _)| found == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("no {name} header in {:?}", self.headers))
        }
    }

    /// What the fake store answers, one per request in order.
    enum Answer {
        Ok(String),
        /// Nothing, ever — the failure a deadline is for.
        Silence,
    }

    #[derive(Default)]
    struct Ledger {
        requests: std::sync::Mutex<Vec<Seen>>,
    }

    impl Ledger {
        fn taken(&self) -> Vec<Seen> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }
    }

    /// **A plaintext S3 on localhost, written to the socket rather than to a
    /// server crate.** This workspace has no HTTP/1.1 server in it — the APNs
    /// fake is `h2` — and what these tests have to read is the exact request
    /// line, the exact headers and the exact body, because that is precisely
    /// what the signature covers. A framework that normalized any of them would
    /// be checking the framework.
    ///
    /// The answers are handed out in the order connections arrive, which is the
    /// order the requests were made only because every caller here is
    /// sequential. A test that issued two at once would have to match them on
    /// something in the request instead.
    async fn store(answers: Vec<Answer>) -> (String, Arc<Ledger>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let ledger = Arc::new(Ledger::default());
        let seen = Arc::clone(&ledger);
        let answers = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            answers,
        )));
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                let answers = Arc::clone(&answers);
                tokio::spawn(async move {
                    serve(socket, &seen, &answers).await;
                });
            }
        });
        (format!("http://{address}"), ledger)
    }

    async fn serve(
        mut socket: tokio::net::TcpStream,
        ledger: &Ledger,
        answers: &std::sync::Mutex<std::collections::VecDeque<Answer>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // The head, then exactly the body the sender declared. One request per
        // connection: the client under test pools nothing.
        let mut buffer = Vec::new();
        let head = loop {
            let mut chunk = [0u8; 4096];
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(end) = buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|at| at + 4)
            {
                break end;
            }
        };
        let text = String::from_utf8_lossy(&buffer[..head]).into_owned();
        let mut lines = text.lines();
        let mut request_line = lines.next().unwrap_or_default().split(' ');
        let method = request_line.next().unwrap_or_default().to_string();
        let target = request_line.next().unwrap_or_default().to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let length: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let mut body = buffer[head..].to_vec();
        while body.len() < length {
            let mut chunk = [0u8; 4096];
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        ledger.requests.lock().unwrap().push(Seen {
            method,
            target,
            headers,
            body,
        });

        let answer = answers.lock().unwrap().pop_front();
        let payload = match answer {
            Some(Answer::Silence) => {
                // Accepted, read, and then nothing at all — the connection stays
                // open and no byte ever comes back.
                std::future::pending::<()>().await;
                unreachable!()
            }
            Some(Answer::Ok(body)) => body,
            None => String::new(),
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    }
}
