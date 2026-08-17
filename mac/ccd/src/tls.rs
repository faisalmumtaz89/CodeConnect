//! `wss://` via `tailscale cert`.
//!
//! On a tailnet with HTTPS Certificates enabled, Tailscale issues a real,
//! publicly-trusted certificate for a node's MagicDNS name, which is the only
//! reason TLS here is worth having: the phone validates it with the system
//! trust store and no custom CA, no pinning and no "accept this certificate?"
//! dialog is involved anywhere.
//!
//! Three consequences follow, and they are not negotiable:
//!
//! 1. **The client must connect by MagicDNS name.** The certificate's SAN is a
//!    DNS name; `wss://100.x.y.z:8787` cannot validate against it no matter
//!    what the daemon does. This is why the QR's `host` is the MagicDNS name
//!    whenever one exists (`protocol::pairing::QrPayload`).
//! 2. **TLS is best-effort, never a startup requirement.** A tailnet with HTTPS
//!    disabled, an expired node key, or a `tailscale` binary that is not there
//!    all end the same way: log it, serve `ws://`, and report `tls: false` in
//!    `hello_ack` so the phone knows exactly what it is on.
//! 3. **Both schemes share one port.** The listener sniffs the first byte
//!    (`ws_server::is_tls_hello`) rather than demanding a flag day, because the
//!    iOS client and this daemon ship independently and a hard cutover would
//!    mean a phone that cannot connect at all.
//!
//! Expiry is read out of the certificate rather than tracked in a sidecar file,
//! because the file could be replaced by anything (a manual `tailscale cert`, a
//! restore from backup) and a cached claim about someone else's bytes is how a
//! daemon ends up serving an expired certificate while believing it is fine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use protocol::pairing::TAILSCALE_CANDIDATES;

/// ACME issuance is not fast, and the first call on a tailnet may do a full
/// order. Generous, but bounded: a wedged subprocess must not wedge startup.
const CERT_TIMEOUT: Duration = Duration::from_secs(90);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// The platform trust store, for the connections this daemon *makes*.
///
/// Both push transports reach a public endpoint — Apple's, or the relay's —
/// whose certificate chains to a public root, so the system store is exactly
/// right and vendoring a root set would be a second thing to keep current.
fn platform_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// An outbound TLS client that will negotiate HTTP/2, or the reason it cannot.
///
/// **ALPN is required here, not an optimisation.** Both endpoints this daemon
/// dials serve HTTP/2 and decide the protocol from ALPN. Without the token the
/// handshake completes, the server offers HTTP/1.1, and the h2 handshake then
/// fails on a connection that looked perfectly healthy — a failure that reads
/// as a network fault and is not one.
///
/// `what` names the peer, so a machine with no usable trust store says which
/// connection it cannot make rather than that something went wrong.
pub fn outbound_h2(what: &str) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(platform_roots());
    if added == 0 {
        bail!("no trust anchors available for the {what} connection");
    }
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

pub fn tailscale_bin() -> Option<PathBuf> {
    TAILSCALE_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// The node's own MagicDNS name, without the trailing dot.
///
/// `Self.DNSName` is authoritative and already fully qualified; deriving it
/// from `HostName` + `MagicDNSSuffix` would guess wrong the moment a hostname
/// contains a character Tailscale rewrites — a Mac named "Ada's Mac Studio"
/// answers to `adas-mac-studio`, which no naive join of the two fields produces.
pub async fn magic_dns_name() -> Option<String> {
    let bin = tailscale_bin()?;
    let output = tokio::time::timeout(
        STATUS_TIMEOUT,
        tokio::process::Command::new(bin)
            .args(["status", "--json"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let name = status
        .get("Self")?
        .get("DNSName")?
        .as_str()?
        .trim_end_matches('.')
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// A certificate on disk that we have actually looked at.
pub struct Material {
    pub hostname: String,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// From the leaf's `notAfter`, so the number describes the bytes we hold.
    pub not_after_ms: i64,
}

impl Material {
    pub fn days_remaining(&self, now_ms: i64) -> i64 {
        (self.not_after_ms - now_ms) / 86_400_000
    }
}

/// Return usable certificate material for `hostname`, issuing or renewing when
/// what is on disk is missing, unreadable, or too close to expiry.
pub async fn ensure(hostname: &str, refresh_days: u64) -> Result<Material> {
    let dir = protocol::tls_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    restrict(&dir, 0o700)?;

    // The hostname reaches this as a filename. It comes from `tailscale status`
    // or from config, but a path separator in either would write the key
    // somewhere unintended, so it is sanitised rather than trusted.
    let stem = sanitise_hostname(hostname)?;
    let cert_path = dir.join(format!("{stem}.crt"));
    let key_path = dir.join(format!("{stem}.key"));

    let now = protocol::time::now_unix_ms();
    if let Some(material) = inspect(hostname, &cert_path, &key_path) {
        if material.days_remaining(now) >= refresh_days as i64 {
            crate::log_info!(
                "tls: using cached certificate for {hostname} ({} days remaining)",
                material.days_remaining(now)
            );
            return Ok(material);
        }
        crate::log_info!(
            "tls: certificate for {hostname} has {} days left; renewing",
            material.days_remaining(now)
        );
    }

    issue(hostname, &cert_path, &key_path).await?;
    restrict(&key_path, 0o600)?;
    inspect(hostname, &cert_path, &key_path)
        .ok_or_else(|| anyhow::anyhow!("tailscale wrote a certificate we cannot parse"))
}

/// Read what is on disk. `None` for anything we cannot fully understand —
/// missing, empty, or a leaf whose validity does not parse — because every one
/// of those means "get a fresh certificate", never "assume it is fine".
fn inspect(hostname: &str, cert_path: &Path, key_path: &Path) -> Option<Material> {
    let cert_pem = std::fs::read(cert_path).ok()?;
    if !key_path.is_file() {
        return None;
    }
    let leaf = first_certificate(&cert_pem)?;
    let not_after_ms = der::not_after_unix_ms(&leaf)?;
    Some(Material {
        hostname: hostname.to_string(),
        cert_path: cert_path.to_path_buf(),
        key_path: key_path.to_path_buf(),
        not_after_ms,
    })
}

async fn issue(hostname: &str, cert_path: &Path, key_path: &Path) -> Result<()> {
    let bin = tailscale_bin().context("tailscale is not installed at a known location")?;
    let output = tokio::time::timeout(
        CERT_TIMEOUT,
        tokio::process::Command::new(&bin)
            .arg("cert")
            .arg("--cert-file")
            .arg(cert_path)
            .arg("--key-file")
            .arg(key_path)
            .arg(hostname)
            .output(),
    )
    .await
    .context("tailscale cert timed out")?
    .context("running tailscale cert")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tailscale cert failed: {}", stderr.trim());
    }
    crate::log_info!("tls: issued a certificate for {hostname}");
    Ok(())
}

/// Build the acceptor. Reads the key on every call rather than caching it in
/// memory, which keeps the private key's lifetime as short as the operation
/// that needs it.
pub fn acceptor(material: &Material) -> Result<TlsAcceptor> {
    let cert_pem = std::fs::read(&material.cert_path)
        .with_context(|| format!("reading {}", material.cert_path.display()))?;
    let key_pem = std::fs::read(&material.key_path)
        .with_context(|| format!("reading {}", material.key_path.display()))?;

    let mut cert_reader = cert_pem.as_slice();
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing the certificate chain")?;
    if certs.is_empty() {
        bail!("{} contains no certificates", material.cert_path.display());
    }
    let mut key_reader = key_pem.as_slice();
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .context("parsing the private key")?
        .ok_or_else(|| {
            anyhow::anyhow!("{} contains no private key", material.key_path.display())
        })?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building the TLS configuration")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Choose the crypto provider explicitly.
///
/// rustls can infer one from enabled features, but inference that changes with
/// a feature flag is not something a daemon should depend on. Being called
/// twice is not an error: the second install losing the race is the correct
/// outcome, since the first already set the provider we want.
pub fn install_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

fn sanitise_hostname(hostname: &str) -> Result<String> {
    let clean: String = hostname
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect();
    if clean.is_empty() || clean.contains("..") {
        bail!("{hostname:?} is not a usable DNS name");
    }
    Ok(clean)
}

fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// The first `CERTIFICATE` block of a PEM bundle, as DER.
fn first_certificate(pem: &[u8]) -> Option<Vec<u8>> {
    let mut reader = pem;
    // Bound to a local: the iterator borrows `reader`, so returning straight
    // out of the chain would outlive it.
    let first = rustls_pemfile::certs(&mut reader).next()?.ok()?;
    Some(first.to_vec())
}

/// Just enough DER to read a certificate's expiry.
///
/// A full X.509 parser is a dependency; `notAfter` is four nested reads.
mod der {
    /// One tag-length-value triple.
    struct Tlv<'a> {
        tag: u8,
        value: &'a [u8],
        /// Bytes consumed, so a caller can walk a sequence.
        total: usize,
    }

    /// Read one TLV. Returns `None` on any malformed length, rather than
    /// panicking or reading past the end.
    fn read(bytes: &[u8]) -> Option<Tlv<'_>> {
        let tag = *bytes.first()?;
        let first_len = *bytes.get(1)? as usize;
        let (len, header) = if first_len < 0x80 {
            (first_len, 2)
        } else {
            let count = first_len & 0x7f;
            // 0x80 is the indefinite form (not legal in DER) and more than four
            // length bytes is a certificate larger than any we will ever serve.
            if count == 0 || count > 4 {
                return None;
            }
            let mut len = 0usize;
            for index in 0..count {
                len = (len << 8) | *bytes.get(2 + index)? as usize;
            }
            (len, 2 + count)
        };
        let value = bytes.get(header..header.checked_add(len)?)?;
        Some(Tlv {
            tag,
            value,
            total: header + len,
        })
    }

    const SEQUENCE: u8 = 0x30;
    const INTEGER: u8 = 0x02;
    const CONTEXT_0: u8 = 0xa0;
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;

    /// `Certificate.tbsCertificate.validity.notAfter`, in epoch milliseconds.
    ///
    /// ```text
    /// Certificate    ::= SEQUENCE { tbsCertificate TBSCertificate, ... }
    /// TBSCertificate ::= SEQUENCE { [0] version OPTIONAL, serialNumber INTEGER,
    ///                               signature SEQUENCE, issuer SEQUENCE,
    ///                               validity SEQUENCE { notBefore, notAfter }, ... }
    /// ```
    pub fn not_after_unix_ms(der: &[u8]) -> Option<i64> {
        let certificate = read(der)?;
        if certificate.tag != SEQUENCE {
            return None;
        }
        let tbs = read(certificate.value)?;
        if tbs.tag != SEQUENCE {
            return None;
        }

        let mut rest = tbs.value;
        // `version` is `[0] EXPLICIT ... DEFAULT v1`, so DER omits it on a v1
        // certificate. Peek rather than assume.
        if read(rest)?.tag == CONTEXT_0 {
            rest = skip_field(rest, CONTEXT_0)?;
        }
        rest = skip_field(rest, INTEGER)?; // serialNumber
        rest = skip_field(rest, SEQUENCE)?; // signature
        rest = skip_field(rest, SEQUENCE)?; // issuer

        let validity = read(rest)?;
        if validity.tag != SEQUENCE {
            return None;
        }
        let not_before = read(validity.value)?;
        let not_after = read(validity.value.get(not_before.total..)?)?;
        parse_time(not_after.tag, not_after.value)
    }

    /// Consume one field of `expected` tag, returning what follows it.
    fn skip_field(bytes: &[u8], expected: u8) -> Option<&[u8]> {
        let field = read(bytes)?;
        if field.tag != expected {
            return None;
        }
        bytes.get(field.total..)
    }

    /// `YYMMDDHHMMSSZ` (UTCTime) or `YYYYMMDDHHMMSSZ` (GeneralizedTime).
    fn parse_time(tag: u8, value: &[u8]) -> Option<i64> {
        let text = std::str::from_utf8(value).ok()?;
        let digits = |text: &str, at: usize, len: usize| -> Option<i64> {
            text.get(at..at + len)?.parse::<i64>().ok()
        };
        let (year, offset) = match tag {
            UTC_TIME => {
                if text.len() < 13 {
                    return None;
                }
                let short = digits(text, 0, 2)?;
                // RFC 5280, section 4.1.2.5.1: 00-49 is 20xx, 50-99 is 19xx.
                (
                    if short < 50 {
                        2000 + short
                    } else {
                        1900 + short
                    },
                    2,
                )
            }
            GENERALIZED_TIME => {
                if text.len() < 15 {
                    return None;
                }
                (digits(text, 0, 4)?, 4)
            }
            _ => return None,
        };
        // Only the `Z` form is legal in a certificate (RFC 5280 requires UTC).
        if !text.ends_with('Z') {
            return None;
        }
        Some(protocol::time::unix_ms_from_civil(
            year,
            digits(text, offset, 2)? as u32,
            digits(text, offset + 2, 2)? as u32,
            digits(text, offset + 4, 2)? as u32,
            digits(text, offset + 6, 2)? as u32,
            digits(text, offset + 8, 2)? as u32,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn utc_time_applies_the_rfc5280_century_rule() {
            assert_eq!(
                parse_time(UTC_TIME, b"261029120000Z"),
                Some(protocol::time::unix_ms_from_civil(2026, 10, 29, 12, 0, 0))
            );
            assert_eq!(
                parse_time(UTC_TIME, b"990101000000Z"),
                Some(protocol::time::unix_ms_from_civil(1999, 1, 1, 0, 0, 0))
            );
            assert_eq!(
                parse_time(UTC_TIME, b"490101000000Z"),
                Some(protocol::time::unix_ms_from_civil(2049, 1, 1, 0, 0, 0))
            );
        }

        #[test]
        fn generalized_time_carries_its_own_century() {
            assert_eq!(
                parse_time(GENERALIZED_TIME, b"20261029120000Z"),
                Some(protocol::time::unix_ms_from_civil(2026, 10, 29, 12, 0, 0))
            );
        }

        #[test]
        fn malformed_times_are_none_not_a_panic() {
            assert_eq!(parse_time(UTC_TIME, b"26102912"), None);
            assert_eq!(parse_time(UTC_TIME, b"2610291200+0"), None);
            assert_eq!(parse_time(UTC_TIME, b"xx1029120000Z"), None);
            assert_eq!(parse_time(0x05, b"20261029120000Z"), None);
            assert_eq!(parse_time(UTC_TIME, &[0xff, 0xfe]), None);
        }

        #[test]
        fn truncated_der_is_rejected_without_reading_past_the_end() {
            // Every prefix of a plausible header must fail cleanly. This is the
            // whole safety argument for a hand-rolled parser.
            let body = [0x30u8, 0x82, 0x01, 0x00];
            for len in 0..body.len() {
                assert!(not_after_unix_ms(&body[..len]).is_none());
            }
            assert!(not_after_unix_ms(&[]).is_none());
            assert!(
                not_after_unix_ms(&[0x30, 0x80]).is_none(),
                "indefinite length"
            );
            assert!(not_after_unix_ms(&[0x30, 0x88, 1, 1, 1, 1, 1, 1, 1, 1]).is_none());
            assert!(
                not_after_unix_ms(&[0x02, 0x01, 0x00]).is_none(),
                "not a SEQUENCE"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_that_would_escape_the_tls_directory_are_refused() {
        // The name reaches this as a filename; a separator in it would write a
        // private key outside ~/.codeconnect/tls.
        assert_eq!(
            sanitise_hostname("your-mac.tailnet-name.ts.net").unwrap(),
            "your-mac.tailnet-name.ts.net"
        );
        // Separators are stripped, and what is left still contains `..`, so it
        // is refused outright rather than written under a mangled name.
        assert!(sanitise_hostname("a/../../etc/passwd").is_err());
        assert!(sanitise_hostname("..").is_err());
        assert!(sanitise_hostname("../../key").is_err());
        assert!(sanitise_hostname("/").is_err());
        assert!(sanitise_hostname("").is_err());
        // A plain traversal-free name survives with its separators removed.
        assert_eq!(sanitise_hostname("a/b/c").unwrap(), "abc");
    }

    #[test]
    fn days_remaining_is_computed_from_the_certificate_not_the_clock_we_wish_for() {
        let material = Material {
            hostname: "h".into(),
            cert_path: PathBuf::new(),
            key_path: PathBuf::new(),
            not_after_ms: 1_000 * 86_400_000,
        };
        assert_eq!(material.days_remaining(900 * 86_400_000), 100);
        // Already expired: negative, so it can never satisfy a refresh floor.
        assert!(material.days_remaining(1_100 * 86_400_000) < 0);
    }

    #[test]
    fn a_missing_certificate_reads_as_absent_rather_than_erroring() {
        let missing = std::env::temp_dir().join("ccd-tls-does-not-exist");
        assert!(inspect(
            "h",
            &missing.with_extension("crt"),
            &missing.with_extension("key")
        )
        .is_none());
    }

    #[test]
    fn a_certificate_without_its_key_is_not_usable_material() {
        let dir = std::env::temp_dir().join(format!("ccd-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("half.crt");
        std::fs::write(&cert, "-----BEGIN CERTIFICATE-----\nnot base64\n").unwrap();
        assert!(inspect("h", &cert, &dir.join("half.key")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
