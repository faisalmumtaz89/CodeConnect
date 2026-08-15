//! App Attest: the ceremony that decides who is allowed to hold a credential.
//!
//! CodeConnect has no accounts, so there is no other moment at which the relay
//! learns that a caller is a real installation of the real app rather than
//! somebody who read the API documentation. Everything downstream — the bearer,
//! the token binding, the rate budget — is issued on the strength of what this
//! module returns, which makes it the security boundary of the service. It is
//! also the only place that parses bytes chosen by an attacker, so the parsing
//! is borrowed: `ciborium` for CBOR, `x509-parser` for the certificate,
//! `rustls-webpki` for the path, and `ring` for the signature.
//!
//! **The root is compiled in, never fetched.** Apple publishes the App Attest
//! root once and it is valid until 2045; a relay that downloaded it at startup
//! would have made its trust anchor a network-controlled value. A test asserts
//! the fingerprint of the copy in this directory, so replacing it is a failing
//! build rather than a silent change of who can vouch for a device.
//!
//! **Verification time is a parameter, and this is not fussiness.** Apple's
//! credCert is valid for about three days. An attestation is therefore verified
//! once, at enrolment, and the *result* is stored; re-running verification a
//! week later against the same object would refuse an installation that is
//! perfectly legitimate. Passing the clock in is what lets the acceptance suite
//! use Apple's own published attestation object, whose leaf expired in April
//! 2026 and cannot be reissued.
//!
//! **The client data hash is a parameter too.** The device signs whatever bytes
//! the app handed it; `SHA256(challenge)` is a convention the app and the relay
//! agree on, not something the protocol enforces. A verifier that hashed the
//! challenge internally would be untestable against the only real attestation
//! object in existence, which was made over a raw challenge.

use std::sync::OnceLock;

use ciborium::value::Value;
use ring::digest::{Context, SHA256};
use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1};
use rustls_pki_types::{CertificateDer, SignatureVerificationAlgorithm, UnixTime};
use webpki::{anchor_from_trusted_cert, EndEntityCert, KeyUsage};
use x509_parser::prelude::FromDer;

/// Apple's App Attest root, pinned in the binary.
const APPLE_ROOT_CA_PEM: &str = include_str!("../apple-app-attest-root-ca.pem");

/// The algorithms a genuine Apple chain is allowed to be signed with.
///
/// **`ECDSA_P384_SHA256` is mandatory and is missing from most default sets.**
/// Apple's intermediate holds a P-384 key but signs the credCert with SHA-256,
/// and webpki matches the pair rather than either half: drop this entry and
/// every real chain fails with `UnsupportedSignatureAlgorithmForPublicKey`,
/// which reads like a malformed attestation rather than a misconfigured relay.
/// No synthetic fixture reproduces the pairing — `rcgen` signs with SHA-384
/// whenever the issuer key is P-384 — so only the real object catches this,
/// which is why one is checked in.
const CHAIN_ALGORITHMS: &[&dyn SignatureVerificationAlgorithm] = &[
    webpki::ring::ECDSA_P256_SHA256,
    webpki::ring::ECDSA_P384_SHA384,
    webpki::ring::ECDSA_P384_SHA256,
];

/// Apple's App Attest extended key usage, 1.2.840.113635.100.4.18, as the DER
/// content bytes webpki compares.
const APP_ATTEST_EKU: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 0x04, 0x18];

/// The credCert extension carrying the nonce, 1.2.840.113635.100.8.2.
const NONCE_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 0x08, 0x02];

/// The DER that precedes the nonce inside that extension.
///
/// **A context-specific \[1\] sits between the SEQUENCE and the OCTET STRING**,
/// which Apple's prose does not mention. A reader that expects
/// `SEQUENCE { OCTET STRING }` finds no nonce at all and rejects every genuine
/// attestation. The lengths are fixed because the payload is always a SHA-256
/// digest, so an exact prefix is as strict as a parser would be and cannot be
/// talked into reading somewhere else.
const NONCE_PREFIX: &[u8] = &[0x30, 0x24, 0xa1, 0x22, 0x04, 0x20];

const RP_ID_HASH: std::ops::Range<usize> = 0..32;
const COUNTER: std::ops::Range<usize> = 33..37;
const AAGUID: std::ops::Range<usize> = 37..53;
const CREDENTIAL_ID_LEN: std::ops::Range<usize> = 53..55;
const CREDENTIAL_ID_START: usize = 55;

/// The shortest `authenticatorData` an assertion can carry: hash, flags,
/// counter, and nothing else. Assertions never repeat the attested credential.
///
/// **A minimum, and never an equality.** From iOS 27 a device appends the same
/// extensions map an attestation carries, so an assertion is these 37 bytes on
/// every phone shipping today and these 37 bytes followed by a CBOR map on the
/// ones that are not. A length check would refuse the newer phone.
const ASSERTION_AUTH_DATA_LEN: usize = 37;

/// The names a device may write the validation category under.
///
/// **Two spellings, because Apple documents two.** The attestation steps name it
/// `apple_validation_category_01` and the assertion steps name the same value
/// `validationCategory`; which one a given iOS writes is not something a server
/// gets to assume, and reading only one of them is a policy that silently stops
/// applying.
const CATEGORY_KEYS: &[&str] = &["apple_validation_category_01", "validationCategory"];

/// The names a device may write the bundle version under, for the same reason.
const BUNDLE_VERSION_KEYS: &[&str] = &["apple_bundle_version_01", "bundleVersion"];

/// Which App Attest world a key was minted in.
///
/// **These are separate namespaces, and the check against one is an equality
/// check.** A development key can be produced by anybody who can build the app
/// with a development profile, so a relay that accepted either value in
/// production would be accepting attestations that prove nothing about the
/// build the customer is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aaguid(pub [u8; 16]);

impl Aaguid {
    pub const PRODUCTION: Self = Self(*b"appattest\0\0\0\0\0\0\0");
    pub const DEVELOPMENT: Self = Self(*b"appattestdevelop");
}

impl std::fmt::Display for Aaguid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// What an accepted attestation established, and everything the relay needs to
/// keep about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAttestation {
    /// The credCert's public key as an uncompressed EC point — the value later
    /// assertions are checked against.
    pub public_key: Vec<u8>,
    /// The attested credential id, which is also Apple's key id for this key.
    pub key_id: Vec<u8>,
    /// Apple's receipt, stored byte for byte. It is CMS rooted at a different
    /// Apple CA, no verification step needs it, and parsing it here would be
    /// attacker-supplied input read for no reason.
    pub receipt: Vec<u8>,
    /// Zero, by the time this value exists. Kept because assertions are
    /// measured against it.
    pub counter: u32,
    pub aaguid: Aaguid,
    /// **Optional because the extension is, not because it is unimportant.**
    /// Older iOS omits it, and the relay's policy — not its parser — decides
    /// what an absent distribution category means.
    pub validation_category: Option<u32>,
    pub bundle_version: Option<String>,
}

/// What an accepted assertion established.
///
/// **The two policy inputs are optional here for a different reason than they
/// are on an attestation.** iOS 27 appends them to an assertion and no earlier
/// release does, so an assertion that carries them is a phone describing the
/// build it is running *now* — later than, and authoritative over, whatever it
/// was running when it attested — and an assertion without them is not a phone
/// declining to say, it is a phone that has no way to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAssertion {
    /// The counter this assertion reached. The caller stores it; the next
    /// assertion must exceed it.
    pub counter: u32,
    pub validation_category: Option<u32>,
    pub bundle_version: Option<String>,
}

/// Why an attestation or assertion was refused.
///
/// One variant per rule, because "attestation invalid" tells an operator
/// nothing and tells a caller exactly as much as it tells an attacker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestError {
    Cbor(String),
    Format(String),
    Shape(&'static str),
    Certificate(String),
    Chain(webpki::Error),
    NonceMissing,
    NonceMalformed,
    NonceMismatch,
    AppIdMismatch,
    AaguidMismatch { expected: Aaguid, found: Aaguid },
    PublicKey(&'static str),
    CredentialIdNotKeyHash,
    KeyIdMismatch,
    CounterNotZero(u32),
    Signature,
    CounterNotAdvanced { stored: u32, offered: u32 },
}

impl std::fmt::Display for AttestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttestError::Cbor(why) => write!(f, "the object is not the CBOR it claims: {why}"),
            AttestError::Format(found) => {
                write!(
                    f,
                    "the attestation format is {found:?}, not apple-appattest"
                )
            }
            AttestError::Shape(what) => write!(f, "{what}"),
            AttestError::Certificate(why) => write!(f, "the credential certificate: {why}"),
            AttestError::Chain(err) => {
                write!(f, "the chain does not lead to Apple's root: {err}")
            }
            AttestError::NonceMissing => {
                write!(f, "the credential certificate carries no nonce extension")
            }
            AttestError::NonceMalformed => {
                write!(f, "the nonce extension is not a 32-byte nonce")
            }
            AttestError::NonceMismatch => write!(
                f,
                "the nonce does not match this authenticator data and challenge"
            ),
            AttestError::AppIdMismatch => write!(f, "the attestation is for another app"),
            AttestError::AaguidMismatch { expected, found } => {
                write!(f, "the key was minted as {found}, not {expected}")
            }
            AttestError::PublicKey(why) => write!(f, "the attested public key {why}"),
            AttestError::CredentialIdNotKeyHash => write!(
                f,
                "the credential id is not the hash of the attested public key"
            ),
            AttestError::KeyIdMismatch => {
                write!(f, "the attested credential is not the key id claimed")
            }
            AttestError::CounterNotZero(found) => {
                write!(f, "a freshly attested key has counter {found}, not 0")
            }
            AttestError::Signature => write!(f, "the assertion signature does not verify"),
            AttestError::CounterNotAdvanced { stored, offered } => write!(
                f,
                "the assertion counter is {offered} and {stored} was already seen"
            ),
        }
    }
}

impl std::error::Error for AttestError {}

/// Everything the verifier is allowed to differ about between production and a
/// test.
///
/// The seam exists because no test can mint a certificate under Apple's root:
/// the negative paths are exercised against a chain built at test time, and the
/// one chain Apple actually signed is checked against the pinned root. Neither
/// substitutes for the other, so both are reachable.
struct Trust<'a> {
    root: &'a CertificateDer<'a>,
    algorithms: &'a [&'a dyn SignatureVerificationAlgorithm],
}

/// Apple's root, parsed once.
///
/// Panicking here would be a broken binary rather than a bad request: the PEM is
/// compiled in and its fingerprint is asserted by a test.
fn apple_root() -> &'static CertificateDer<'static> {
    static ROOT: OnceLock<CertificateDer<'static>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut pem = APPLE_ROOT_CA_PEM.as_bytes();
        let mut roots = rustls_pemfile::certs(&mut pem)
            .collect::<Result<Vec<_>, _>>()
            .expect("the pinned Apple App Attest root parses");
        // One anchor, not a store: a second certificate appended to that file
        // would be a second party allowed to vouch for a device.
        assert_eq!(roots.len(), 1, "the pinned root file holds one certificate");
        roots.remove(0)
    })
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut context = Context::new(&SHA256);
    context.update(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(context.finish().as_ref());
    out
}

/// Apple's nonce construction: one digest over the authenticator data followed
/// by whatever the app called its client data hash.
fn nonce(auth_data: &[u8], client_data_hash: &[u8]) -> [u8; 32] {
    let mut context = Context::new(&SHA256);
    context.update(auth_data);
    context.update(client_data_hash);
    let mut out = [0u8; 32];
    out.copy_from_slice(context.finish().as_ref());
    out
}

fn entry<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn entry_any<'a>(map: &'a [(Value, Value)], keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| entry(map, key))
}

fn bytes(value: &Value, what: &'static str) -> Result<Vec<u8>, AttestError> {
    value
        .as_bytes()
        .map(|b| b.to_vec())
        .ok_or(AttestError::Shape(what))
}

/// The fixed-offset part of `authData`, sliced but not yet judged.
struct AuthData<'a> {
    rp_id_hash: &'a [u8],
    counter: u32,
    aaguid: Aaguid,
    credential_id: &'a [u8],
    /// The COSE key and, when present, the extensions map. Left undecoded until
    /// the cheap equality checks have run.
    tail: &'a [u8],
}

fn slice_auth_data(auth_data: &[u8]) -> Result<AuthData<'_>, AttestError> {
    if auth_data.len() < CREDENTIAL_ID_START {
        return Err(AttestError::Shape(
            "the authenticator data is too short to carry an attested credential",
        ));
    }
    let mut aaguid = [0u8; 16];
    aaguid.copy_from_slice(&auth_data[AAGUID]);
    let mut counter = [0u8; 4];
    counter.copy_from_slice(&auth_data[COUNTER]);
    let mut length = [0u8; 2];
    length.copy_from_slice(&auth_data[CREDENTIAL_ID_LEN]);
    let end = CREDENTIAL_ID_START + usize::from(u16::from_be_bytes(length));
    if end > auth_data.len() {
        return Err(AttestError::Shape(
            "the credential id runs past the end of the authenticator data",
        ));
    }
    Ok(AuthData {
        rp_id_hash: &auth_data[RP_ID_HASH],
        counter: u32::from_be_bytes(counter),
        aaguid: Aaguid(aaguid),
        credential_id: &auth_data[CREDENTIAL_ID_START..end],
        tail: &auth_data[end..],
    })
}

/// Read the two policy inputs out of an `authData` extensions map, wherever the
/// map begins.
///
/// **Absence is not an error.** The extensions are appended by iOS 27 and by no
/// earlier release, so an empty tail is what almost every phone in the world
/// produces today and the relay's policy — not its parser — decides what that
/// means.
fn read_extension_map(mut rest: &[u8]) -> Result<(Option<u32>, Option<String>), AttestError> {
    if rest.is_empty() {
        return Ok((None, None));
    }
    let extensions: Value =
        ciborium::de::from_reader(&mut rest).map_err(|e| AttestError::Cbor(e.to_string()))?;
    let entries = extensions
        .as_map()
        .ok_or(AttestError::Shape("the extensions are not a CBOR map"))?;

    // The category is a four-byte CBOR byte string holding a little-endian
    // integer, not a CBOR integer. Reading it as one finds nothing, and reading
    // the four bytes the other way round turns a 1 into 16777216.
    let category = match entry_any(entries, CATEGORY_KEYS) {
        Some(value) => {
            let raw = bytes(value, "the validation category is not a byte string")?;
            let raw = <[u8; 4]>::try_from(raw.as_slice())
                .map_err(|_| AttestError::Shape("the validation category is not four bytes"))?;
            Some(u32::from_le_bytes(raw))
        }
        None => None,
    };
    let version = match entry_any(entries, BUNDLE_VERSION_KEYS) {
        Some(value) => Some(
            value
                .as_text()
                .ok_or(AttestError::Shape("the bundle version is not text"))?
                .to_string(),
        ),
        None => None,
    };
    Ok((category, version))
}

/// The same two values out of an attestation, where a COSE key sits in front of
/// them.
///
/// **The ED flag is not consulted.** Apple's own attestation sets flags to
/// `0x40` — the attested-credential bit alone — while carrying a full
/// extensions map, so a reader that waited for `0x80` would return `None` for
/// every real device and quietly disable the distribution-category policy.
fn read_extensions(tail: &[u8]) -> Result<(Option<u32>, Option<String>), AttestError> {
    let mut rest = tail;
    let _cose_key: Value =
        ciborium::de::from_reader(&mut rest).map_err(|e| AttestError::Cbor(e.to_string()))?;
    read_extension_map(rest)
}

/// The nonce Apple put in the credCert, or the reason there is none to read.
fn certificate_nonce(
    certificate: &x509_parser::certificate::X509Certificate<'_>,
) -> Result<[u8; 32], AttestError> {
    let extension = certificate
        .extensions()
        .iter()
        .find(|extension| extension.oid.as_bytes() == NONCE_OID)
        .ok_or(AttestError::NonceMissing)?;
    if extension.value.len() != NONCE_PREFIX.len() + 32
        || !extension.value.starts_with(NONCE_PREFIX)
    {
        return Err(AttestError::NonceMalformed);
    }
    let mut found = [0u8; 32];
    found.copy_from_slice(&extension.value[NONCE_PREFIX.len()..]);
    Ok(found)
}

/// The credCert's public key as an uncompressed EC point.
///
/// App Attest keys are P-256, so anything else is either a different curve or a
/// point format `ring` will not verify against later — better refused here,
/// where the reason is legible.
fn public_key(
    certificate: &x509_parser::certificate::X509Certificate<'_>,
) -> Result<Vec<u8>, AttestError> {
    let point = certificate.public_key().subject_public_key.data.as_ref();
    if point.len() != 65 || point[0] != 0x04 {
        return Err(AttestError::PublicKey("is not an uncompressed P-256 point"));
    }
    Ok(point.to_vec())
}

fn verify_chain<'a>(
    leaf: &'a CertificateDer<'a>,
    intermediates: &'a [CertificateDer<'a>],
    trust: &Trust<'a>,
    now: UnixTime,
) -> Result<(), AttestError> {
    let anchor = anchor_from_trusted_cert(trust.root).map_err(AttestError::Chain)?;
    let end_entity = EndEntityCert::try_from(leaf).map_err(AttestError::Chain)?;
    end_entity
        .verify_for_usage(
            trust.algorithms,
            &[anchor],
            intermediates,
            now,
            // **`required_if_present`, never `required`.** Apple's intermediate
            // carries no extended key usage extension at all, and webpki applies
            // the usage rule at every node in the path — so demanding the OID
            // outright rejects the genuine chain with `RequiredEkuNotFound`.
            KeyUsage::required_if_present(APP_ATTEST_EKU),
            None,
            None,
        )
        .map(|_| ())
        .map_err(AttestError::Chain)
}

/// Verify an App Attest attestation object against Apple's pinned root.
///
/// `client_data_hash` is the exact bytes the app passed to `attestKey`, and
/// `now` is the instant the chain is judged at — see the module note on why
/// neither is computed here.
pub fn verify_attestation(
    attestation_cbor: &[u8],
    client_data_hash: &[u8],
    expected_app_id: &str,
    expected_aaguid: Aaguid,
    expected_key_id: &[u8],
    now: UnixTime,
) -> Result<VerifiedAttestation, AttestError> {
    verify_attestation_with(
        attestation_cbor,
        client_data_hash,
        expected_app_id,
        expected_aaguid,
        expected_key_id,
        now,
        &Trust {
            root: apple_root(),
            algorithms: CHAIN_ALGORITHMS,
        },
    )
}

fn verify_attestation_with(
    attestation_cbor: &[u8],
    client_data_hash: &[u8],
    expected_app_id: &str,
    expected_aaguid: Aaguid,
    expected_key_id: &[u8],
    now: UnixTime,
    trust: &Trust<'_>,
) -> Result<VerifiedAttestation, AttestError> {
    let object: Value = ciborium::de::from_reader(attestation_cbor)
        .map_err(|e| AttestError::Cbor(e.to_string()))?;
    let object = object
        .as_map()
        .ok_or(AttestError::Shape("the attestation object is not a map"))?;

    let format = entry(object, "fmt")
        .and_then(Value::as_text)
        .ok_or(AttestError::Shape("the object declares no format"))?;
    if format != "apple-appattest" {
        return Err(AttestError::Format(format.to_string()));
    }

    let statement = entry(object, "attStmt")
        .and_then(Value::as_map)
        .ok_or(AttestError::Shape("the object carries no attStmt map"))?;
    let chain = entry(statement, "x5c")
        .and_then(Value::as_array)
        .ok_or(AttestError::Shape("the attStmt carries no x5c array"))?;
    let [leaf, intermediate] = chain.as_slice() else {
        return Err(AttestError::Shape(
            "x5c is not a credential certificate and one intermediate",
        ));
    };
    let leaf = CertificateDer::from(bytes(leaf, "the credential certificate is not bytes")?);
    let intermediates = [CertificateDer::from(bytes(
        intermediate,
        "the intermediate certificate is not bytes",
    )?)];
    let receipt = bytes(
        entry(statement, "receipt").ok_or(AttestError::Shape("the attStmt carries no receipt"))?,
        "the receipt is not bytes",
    )?;
    let auth_data = bytes(
        entry(object, "authData").ok_or(AttestError::Shape("the object carries no authData"))?,
        "the authenticator data is not bytes",
    )?;

    verify_chain(&leaf, &intermediates, trust, now)?;

    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(&leaf)
        .map_err(|e| AttestError::Certificate(e.to_string()))?;

    if certificate_nonce(&certificate)? != nonce(&auth_data, client_data_hash) {
        return Err(AttestError::NonceMismatch);
    }

    let attested = slice_auth_data(&auth_data)?;

    if attested.rp_id_hash != sha256(expected_app_id.as_bytes()) {
        return Err(AttestError::AppIdMismatch);
    }
    if attested.aaguid != expected_aaguid {
        return Err(AttestError::AaguidMismatch {
            expected: expected_aaguid,
            found: attested.aaguid,
        });
    }

    let key = public_key(&certificate)?;
    if attested.credential_id != sha256(&key) {
        return Err(AttestError::CredentialIdNotKeyHash);
    }
    if attested.credential_id != expected_key_id {
        return Err(AttestError::KeyIdMismatch);
    }
    if attested.counter != 0 {
        return Err(AttestError::CounterNotZero(attested.counter));
    }

    let (validation_category, bundle_version) = read_extensions(attested.tail)?;

    Ok(VerifiedAttestation {
        public_key: key,
        key_id: attested.credential_id.to_vec(),
        receipt,
        counter: attested.counter,
        aaguid: attested.aaguid,
        validation_category,
        bundle_version,
    })
}

/// Verify an App Attest assertion and return everything it established.
pub fn verify_assertion(
    assertion_cbor: &[u8],
    client_data_hash: &[u8],
    stored_public_key: &[u8],
    expected_app_id: &str,
    stored_counter: u32,
) -> Result<VerifiedAssertion, AttestError> {
    let object: Value =
        ciborium::de::from_reader(assertion_cbor).map_err(|e| AttestError::Cbor(e.to_string()))?;
    let object = object
        .as_map()
        .ok_or(AttestError::Shape("the assertion is not a map"))?;
    let signature = bytes(
        entry(object, "signature")
            .ok_or(AttestError::Shape("the assertion carries no signature"))?,
        "the signature is not bytes",
    )?;
    let auth_data = bytes(
        entry(object, "authenticatorData").ok_or(AttestError::Shape(
            "the assertion carries no authenticatorData",
        ))?,
        "the authenticator data is not bytes",
    )?;
    if auth_data.len() < ASSERTION_AUTH_DATA_LEN {
        return Err(AttestError::Shape(
            "the authenticator data is too short to carry a counter",
        ));
    }

    if auth_data[RP_ID_HASH] != sha256(expected_app_id.as_bytes()) {
        return Err(AttestError::AppIdMismatch);
    }

    // **ASN.1 DER, not the fixed-width pair.** Apple's assertions carry a DER
    // `SEQUENCE { r, s }`; verifying them with `ECDSA_P256_SHA256_FIXED` refuses
    // every genuine assertion. The message is the nonce, which ECDSA hashes
    // again — that second hash is the algorithm's, not a mistake here.
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, stored_public_key)
        .verify(&nonce(&auth_data, client_data_hash), &signature)
        .map_err(|_| AttestError::Signature)?;

    let mut counter = [0u8; 4];
    counter.copy_from_slice(&auth_data[COUNTER]);
    let counter = u32::from_be_bytes(counter);
    // Strictly greater: a repeated counter is a replayed assertion, and the
    // whole point of the counter is that replaying one is not possible.
    if counter <= stored_counter {
        return Err(AttestError::CounterNotAdvanced {
            stored: stored_counter,
            offered: counter,
        });
    }

    // Read last, because everything before it is what makes these bytes the
    // device's own: the whole buffer is inside the signature, so what the map
    // says is as trustworthy as the counter beside it.
    let (validation_category, bundle_version) =
        read_extension_map(&auth_data[ASSERTION_AUTH_DATA_LEN..])?;
    Ok(VerifiedAssertion {
        counter,
        validation_category,
        bundle_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use rcgen::{
        date_time_ymd, BasicConstraints, CertificateParams, CustomExtension, DnType, IsCa, KeyPair,
        PKCS_ECDSA_P256_SHA256,
    };
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair as _, ECDSA_P256_SHA256_ASN1_SIGNING};

    /// Apple's own published attestation object, base64. Its leaf expired three
    /// days after it was minted, which is why every test that touches it pins a
    /// clock.
    const REAL_OBJECT: &str = include_str!("../../../fixtures/appattest/attestation-object.b64");
    const REAL_APP_ID: &str = "1234567890.com.example.myapp";
    const REAL_CHALLENGE: &[u8] = b"example_server_challenge";
    const REAL_KEY_ID: &str = "zgSY9YSD+7TaDXssY6WlOPVS1K3Lmk+pFhlcSWE+ZV0=";
    /// Inside the fixture leaf's window of 1776708792..1776967992.
    const REAL_CLOCK: u64 = 1_776_800_000;

    fn base64(text: &str) -> Vec<u8> {
        let joined: String = text.split_whitespace().collect();
        base64::engine::general_purpose::STANDARD
            .decode(joined)
            .expect("the fixture is base64")
    }

    fn at(seconds: u64) -> UnixTime {
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(seconds))
    }

    fn real_object() -> Vec<u8> {
        base64(REAL_OBJECT)
    }

    // ---------------------------------------------------------------------
    // The pinned root.
    // ---------------------------------------------------------------------

    /// A swapped root is a change of who may vouch for a device, which is the
    /// most consequential edit anyone can make to this crate. It should fail
    /// here rather than pass review.
    #[test]
    fn the_pinned_root_is_apples_published_root() {
        let fingerprint: String = sha256(apple_root())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            fingerprint,
            "1cb9823ba28ba6ad2d33a006941de2ae4f513ef1d4e831b9f7e0fa7b6242c932"
        );
    }

    // ---------------------------------------------------------------------
    // Apple's real attestation object.
    //
    // These are the only tests that exercise a chain Apple actually signed, and
    // they are not redundant with the forged ones below: the forge cannot
    // reproduce Apple's P-384-key/SHA-256-signature pairing, because rcgen
    // always signs with SHA-384 when the issuer key is P-384.
    // ---------------------------------------------------------------------

    #[test]
    fn apples_published_attestation_object_verifies() {
        let verified = verify_attestation(
            &real_object(),
            REAL_CHALLENGE,
            REAL_APP_ID,
            Aaguid::PRODUCTION,
            &base64(REAL_KEY_ID),
            at(REAL_CLOCK),
        )
        .expect("Apple's own object must verify");

        assert_eq!(verified.key_id, base64(REAL_KEY_ID));
        assert_eq!(verified.counter, 0);
        assert_eq!(verified.aaguid, Aaguid::PRODUCTION);
        assert_eq!(verified.validation_category, Some(1));
        assert_eq!(verified.bundle_version.as_deref(), Some("1"));
        assert_eq!(verified.public_key.len(), 65);
        assert_eq!(verified.public_key[0], 0x04);
        assert_eq!(verified.receipt.len(), 3977);
    }

    /// Finding 2 as an executable fact: narrow the algorithm list by the one
    /// entry most default sets omit, and the genuine chain stops verifying.
    #[test]
    fn a_narrow_algorithm_list_rejects_apples_real_chain() {
        let narrow: &[&dyn SignatureVerificationAlgorithm] = &[
            webpki::ring::ECDSA_P256_SHA256,
            webpki::ring::ECDSA_P384_SHA384,
        ];
        let err = verify_attestation_with(
            &real_object(),
            REAL_CHALLENGE,
            REAL_APP_ID,
            Aaguid::PRODUCTION,
            &base64(REAL_KEY_ID),
            at(REAL_CLOCK),
            &Trust {
                root: apple_root(),
                algorithms: narrow,
            },
        )
        .expect_err("without ECDSA_P384_SHA256 the real chain cannot verify");
        assert!(
            matches!(
                err,
                AttestError::Chain(
                    webpki::Error::UnsupportedSignatureAlgorithmForPublicKeyContext(_)
                )
            ),
            "{err}"
        );
    }

    /// The same object, judged a year later. Real leaves live about three days,
    /// so an attestation is verified once and its result stored.
    #[test]
    fn apples_real_chain_is_refused_by_a_clock_past_its_window() {
        let err = verify_attestation(
            &real_object(),
            REAL_CHALLENGE,
            REAL_APP_ID,
            Aaguid::PRODUCTION,
            &base64(REAL_KEY_ID),
            at(REAL_CLOCK + 60 * 60 * 24 * 365),
        )
        .expect_err("an expired leaf must not verify");
        assert!(
            matches!(err, AttestError::Chain(webpki::Error::CertExpired { .. })),
            "{err}"
        );
    }

    /// Apple's fixture was made over the raw challenge, ours will be made over
    /// its digest — which is exactly why the hash is an argument.
    #[test]
    fn the_client_data_hash_is_taken_literally() {
        let err = verify_attestation(
            &real_object(),
            &sha256(REAL_CHALLENGE),
            REAL_APP_ID,
            Aaguid::PRODUCTION,
            &base64(REAL_KEY_ID),
            at(REAL_CLOCK),
        )
        .expect_err("hashing the challenge again is a different nonce");
        assert_eq!(err, AttestError::NonceMismatch);
    }

    // ---------------------------------------------------------------------
    // Forged chains, minted at test time.
    //
    // **What these prove and what they do not.** They exercise the whole
    // parser and every negative path, against a root/intermediate/leaf shaped
    // like Apple's — the intermediate deliberately carries no extended key
    // usage, so the `required_if_present` policy is under test. They do *not*
    // prove the algorithm list: rcgen signs with SHA-384 whenever the issuer
    // key is P-384, so a forged chain verifies even with the narrow list that
    // rejects every real Apple chain. That single fact is why the real object
    // above is checked in and why neither set of tests can be deleted in favour
    // of the other. The keys here are generated in-process and never written
    // down; a checked-in chain would be a private key in the repository.
    // ---------------------------------------------------------------------

    const FORGE_CLOCK: u64 = 1_800_000_000;

    struct Forge {
        app_id: String,
        aaguid: Aaguid,
        counter: u32,
        client_data_hash: Vec<u8>,
        credential_id: Option<[u8; 32]>,
        validation_category: Option<u32>,
        bundle_version: Option<String>,
        not_after: (i32, u8, u8),
        corrupt_credential_id_after_signing: bool,
        corrupt_leaf_signature: bool,
        /// A credential-id length field that disagrees with the bytes that
        /// follow it. Written **before** the nonce is computed, so the object
        /// is self-consistent and the parser reaches the bounds check instead
        /// of stopping at the nonce.
        credential_id_length: Option<u16>,
        /// `authData` cut short, for the same reason and in the same place.
        truncate_auth_data: Option<usize>,
        /// The extensions written verbatim instead of built from the two
        /// fields above, for the shapes a device would never produce.
        raw_extensions: Option<Value>,
    }

    impl Default for Forge {
        fn default() -> Self {
            Self {
                app_id: "1234567890.com.example.myapp".to_string(),
                aaguid: Aaguid::PRODUCTION,
                counter: 0,
                client_data_hash: sha256(b"a challenge").to_vec(),
                credential_id: None,
                validation_category: Some(1),
                bundle_version: Some("1".to_string()),
                not_after: (2100, 1, 1),
                corrupt_credential_id_after_signing: false,
                corrupt_leaf_signature: false,
                credential_id_length: None,
                truncate_auth_data: None,
                raw_extensions: None,
            }
        }
    }

    struct Forged {
        root: CertificateDer<'static>,
        object: Vec<u8>,
        key_id: Vec<u8>,
    }

    impl Forged {
        fn verify(
            &self,
            forge: &Forge,
            expected_key_id: &[u8],
        ) -> Result<VerifiedAttestation, AttestError> {
            self.verify_at(forge, expected_key_id, &self.root, FORGE_CLOCK)
        }

        fn verify_at(
            &self,
            forge: &Forge,
            expected_key_id: &[u8],
            root: &CertificateDer<'_>,
            clock: u64,
        ) -> Result<VerifiedAttestation, AttestError> {
            verify_attestation_with(
                &self.object,
                &forge.client_data_hash,
                &forge.app_id,
                forge.aaguid,
                expected_key_id,
                at(clock),
                &Trust {
                    root,
                    algorithms: CHAIN_ALGORITHMS,
                },
            )
        }
    }

    fn authority(name: &str) -> CertificateParams {
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2100, 1, 1);
        params
    }

    /// A COSE_Key for the attested P-256 key, written the way a real one is so
    /// the extensions that follow it start at a realistic offset.
    fn cose_key(point: &[u8]) -> Value {
        Value::Map(vec![
            (Value::Integer(1.into()), Value::Integer(2.into())),
            (Value::Integer(3.into()), Value::Integer((-7).into())),
            (Value::Integer((-1).into()), Value::Integer(1.into())),
            (
                Value::Integer((-2).into()),
                Value::Bytes(point[1..33].to_vec()),
            ),
            (
                Value::Integer((-3).into()),
                Value::Bytes(point[33..65].to_vec()),
            ),
        ])
    }

    fn cbor(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(value, &mut out).expect("CBOR into a Vec cannot fail");
        out
    }

    fn forge(forge: &Forge) -> Forged {
        let root_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let root = authority("Forged App Attest Root")
            .self_signed(&root_key)
            .unwrap();

        // No extended key usage, exactly like Apple's intermediate. This is what
        // makes `required_if_present` load-bearing rather than decorative.
        let intermediate_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let intermediate = authority("Forged App Attest CA")
            .signed_by(&intermediate_key, &root, &root_key)
            .unwrap();

        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let point = leaf_key.public_key_raw().to_vec();
        let credential_id = forge.credential_id.unwrap_or_else(|| sha256(&point));

        let mut auth_data = Vec::new();
        auth_data.extend_from_slice(&sha256(forge.app_id.as_bytes()));
        // 0x40 and not 0x80 | 0x40, matching Apple: the extensions map is there
        // whether or not the ED bit says so.
        auth_data.push(0x40);
        auth_data.extend_from_slice(&forge.counter.to_be_bytes());
        auth_data.extend_from_slice(&forge.aaguid.0);
        auth_data.extend_from_slice(&(credential_id.len() as u16).to_be_bytes());
        auth_data.extend_from_slice(&credential_id);
        auth_data.extend_from_slice(&cbor(&cose_key(&point)));
        match &forge.raw_extensions {
            Some(extensions) => auth_data.extend_from_slice(&cbor(extensions)),
            None => {
                let mut extensions = Vec::new();
                if let Some(version) = &forge.bundle_version {
                    extensions.push((
                        Value::Text("apple_bundle_version_01".to_string()),
                        Value::Text(version.clone()),
                    ));
                }
                if let Some(category) = forge.validation_category {
                    extensions.push((
                        Value::Text("apple_validation_category_01".to_string()),
                        Value::Bytes(category.to_le_bytes().to_vec()),
                    ));
                }
                if !extensions.is_empty() {
                    auth_data.extend_from_slice(&cbor(&Value::Map(extensions)));
                }
            }
        }

        // Before the nonce, so that a lying length and a cut-short buffer are
        // signed over and the parser has to refuse them itself.
        if let Some(length) = forge.credential_id_length {
            auth_data[CREDENTIAL_ID_LEN].copy_from_slice(&length.to_be_bytes());
        }
        if let Some(length) = forge.truncate_auth_data {
            auth_data.truncate(length);
        }

        let mut leaf_params = CertificateParams::default();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "Forged credential certificate");
        leaf_params.not_before = date_time_ymd(2020, 1, 1);
        leaf_params.not_after =
            date_time_ymd(forge.not_after.0, forge.not_after.1, forge.not_after.2);
        let mut nonce_der = NONCE_PREFIX.to_vec();
        nonce_der.extend_from_slice(&nonce(&auth_data, &forge.client_data_hash));
        leaf_params
            .custom_extensions
            .push(CustomExtension::from_oid_content(
                &[1, 2, 840, 113635, 100, 8, 2],
                nonce_der,
            ));
        let leaf = leaf_params
            .signed_by(&leaf_key, &intermediate, &intermediate_key)
            .unwrap();

        if forge.corrupt_credential_id_after_signing {
            auth_data[CREDENTIAL_ID_START + 5] ^= 0xff;
        }
        let mut leaf_der = leaf.der().to_vec();
        if forge.corrupt_leaf_signature {
            // The final byte of a certificate is inside the signature value.
            let last = leaf_der.len() - 1;
            leaf_der[last] ^= 0xff;
        }

        let object = Value::Map(vec![
            (
                Value::Text("fmt".to_string()),
                Value::Text("apple-appattest".to_string()),
            ),
            (
                Value::Text("attStmt".to_string()),
                Value::Map(vec![
                    (
                        Value::Text("x5c".to_string()),
                        Value::Array(vec![
                            Value::Bytes(leaf_der),
                            Value::Bytes(intermediate.der().to_vec()),
                        ]),
                    ),
                    (
                        Value::Text("receipt".to_string()),
                        Value::Bytes(b"an opaque receipt".to_vec()),
                    ),
                ]),
            ),
            (Value::Text("authData".to_string()), Value::Bytes(auth_data)),
        ]);

        Forged {
            root: root.der().clone(),
            object: cbor(&object),
            key_id: credential_id.to_vec(),
        }
    }

    #[test]
    fn a_forged_chain_verifies_against_the_root_that_signed_it() {
        let spec = Forge::default();
        let forged = forge(&spec);
        let verified = forged
            .verify(&spec, &forged.key_id)
            .expect("the forge must produce a verifiable object");
        assert_eq!(verified.key_id, forged.key_id);
        assert_eq!(verified.receipt, b"an opaque receipt");
        assert_eq!(verified.aaguid, Aaguid::PRODUCTION);
    }

    #[test]
    fn a_leaf_from_another_root_is_refused() {
        let spec = Forge::default();
        let forged = forge(&spec);
        let stranger = forge(&Forge::default());
        let err = forged
            .verify_at(&spec, &forged.key_id, &stranger.root, FORGE_CLOCK)
            .expect_err("a chain to an unpinned root must not verify");
        assert!(matches!(err, AttestError::Chain(_)), "{err}");
    }

    #[test]
    fn tampered_authenticator_data_breaks_the_nonce() {
        let spec = Forge {
            corrupt_credential_id_after_signing: true,
            ..Forge::default()
        };
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &forged.key_id)
            .expect_err("authenticator data changed after signing must not verify");
        assert_eq!(err, AttestError::NonceMismatch);
    }

    #[test]
    fn a_different_app_id_fails_the_rp_id_hash() {
        let spec = Forge::default();
        let forged = forge(&spec);
        let other = Forge {
            app_id: "1234567890.com.example.otherapp".to_string(),
            ..Forge::default()
        };
        let err = forged
            .verify(&other, &forged.key_id)
            .expect_err("an attestation for another app must not verify");
        assert_eq!(err, AttestError::AppIdMismatch);
    }

    #[test]
    fn development_and_production_keys_are_not_interchangeable() {
        let development = Forge {
            aaguid: Aaguid::DEVELOPMENT,
            ..Forge::default()
        };
        let forged = forge(&development);
        let err = forged
            .verify(&Forge::default(), &forged.key_id)
            .expect_err("a development key offered to production must not verify");
        assert_eq!(
            err,
            AttestError::AaguidMismatch {
                expected: Aaguid::PRODUCTION,
                found: Aaguid::DEVELOPMENT,
            }
        );

        let production = Forge::default();
        let forged = forge(&production);
        let err = forged
            .verify(&development, &forged.key_id)
            .expect_err("a production key offered to development must not verify");
        assert_eq!(
            err,
            AttestError::AaguidMismatch {
                expected: Aaguid::DEVELOPMENT,
                found: Aaguid::PRODUCTION,
            }
        );
    }

    #[test]
    fn a_credential_id_that_is_not_the_key_hash_is_refused() {
        let spec = Forge {
            credential_id: Some([0x11; 32]),
            ..Forge::default()
        };
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &forged.key_id)
            .expect_err("a credential id unrelated to the attested key must not verify");
        assert_eq!(err, AttestError::CredentialIdNotKeyHash);
    }

    #[test]
    fn a_key_id_the_caller_did_not_ask_for_is_refused() {
        let spec = Forge::default();
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &[0x22; 32])
            .expect_err("a credential other than the one claimed must not verify");
        assert_eq!(err, AttestError::KeyIdMismatch);
    }

    #[test]
    fn a_freshly_attested_key_with_a_used_counter_is_refused() {
        let spec = Forge {
            counter: 7,
            ..Forge::default()
        };
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &forged.key_id)
            .expect_err("an attestation counter above zero must not verify");
        assert_eq!(err, AttestError::CounterNotZero(7));
    }

    #[test]
    fn a_tampered_certificate_signature_is_refused() {
        let spec = Forge {
            corrupt_leaf_signature: true,
            ..Forge::default()
        };
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &forged.key_id)
            .expect_err("a leaf whose signature was edited must not verify");
        assert!(matches!(err, AttestError::Chain(_)), "{err}");
    }

    #[test]
    fn the_validation_category_and_bundle_version_are_read() {
        let spec = Forge {
            validation_category: Some(2),
            bundle_version: Some("41.3".to_string()),
            ..Forge::default()
        };
        let forged = forge(&spec);
        let verified = forged.verify(&spec, &forged.key_id).unwrap();
        assert_eq!(verified.validation_category, Some(2));
        assert_eq!(verified.bundle_version.as_deref(), Some("41.3"));

        let absent = Forge {
            validation_category: None,
            bundle_version: None,
            ..Forge::default()
        };
        let forged = forge(&absent);
        let verified = forged.verify(&absent, &forged.key_id).unwrap();
        assert_eq!(verified.validation_category, None);
        assert_eq!(verified.bundle_version, None);
    }

    #[test]
    fn a_clock_outside_the_leafs_validity_is_refused() {
        let spec = Forge {
            not_after: (2026, 1, 1),
            ..Forge::default()
        };
        let forged = forge(&spec);
        let root = forged.root.clone();
        let err = forged
            .verify_at(&spec, &forged.key_id, &root, FORGE_CLOCK)
            .expect_err("a leaf that stopped being valid must not verify");
        assert!(
            matches!(err, AttestError::Chain(webpki::Error::CertExpired { .. })),
            "{err}"
        );
    }

    #[test]
    fn only_apples_attestation_format_is_read() {
        let object = Value::Map(vec![
            (
                Value::Text("fmt".to_string()),
                Value::Text("packed".to_string()),
            ),
            (Value::Text("attStmt".to_string()), Value::Map(vec![])),
            (Value::Text("authData".to_string()), Value::Bytes(vec![])),
        ]);
        let err = verify_attestation(
            &cbor(&object),
            b"",
            REAL_APP_ID,
            Aaguid::PRODUCTION,
            &[0u8; 32],
            at(FORGE_CLOCK),
        )
        .expect_err("another attestation format must not verify");
        assert_eq!(err, AttestError::Format("packed".to_string()));
    }

    // ---------------------------------------------------------------------
    // Bytes chosen to be wrong.
    //
    // The buffer these read is posted by the caller, and every offset in it is
    // fixed — so each bound below is the only thing between a lying length and
    // a slice past the end of a `Vec`. The forge signs each of these the way a
    // device would, because an inconsistent object is refused by the nonce long
    // before it reaches the code under test.
    // ---------------------------------------------------------------------

    #[test]
    fn authenticator_data_too_short_for_an_attested_credential_is_refused_rather_than_sliced() {
        let spec = Forge {
            truncate_auth_data: Some(40),
            ..Forge::default()
        };
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &forged.key_id)
            .expect_err("a buffer that ends inside the AAGUID must not be sliced");
        assert_eq!(
            err,
            AttestError::Shape(
                "the authenticator data is too short to carry an attested credential"
            )
        );
    }

    #[test]
    fn a_credential_id_longer_than_the_buffer_it_sits_in_is_refused_rather_than_sliced() {
        let spec = Forge {
            credential_id_length: Some(u16::MAX),
            ..Forge::default()
        };
        let forged = forge(&spec);
        let err = forged
            .verify(&spec, &forged.key_id)
            .expect_err("a credential id that claims 65535 bytes must not be sliced");
        assert_eq!(
            err,
            AttestError::Shape("the credential id runs past the end of the authenticator data")
        );
    }

    /// The extensions are two values of two shapes. Anything else is refused by
    /// the rule it broke rather than read as whichever type is convenient.
    #[test]
    fn an_extensions_map_of_the_wrong_shape_is_refused_by_the_rule_it_broke() {
        let cases = [
            (
                Value::Text("extensions".to_string()),
                "the extensions are not a CBOR map",
            ),
            (
                Value::Map(vec![(
                    Value::Text("apple_validation_category_01".to_string()),
                    Value::Integer(1.into()),
                )]),
                "the validation category is not a byte string",
            ),
            (
                Value::Map(vec![(
                    Value::Text("apple_validation_category_01".to_string()),
                    Value::Bytes(vec![1, 0]),
                )]),
                "the validation category is not four bytes",
            ),
            (
                Value::Map(vec![(
                    Value::Text("apple_bundle_version_01".to_string()),
                    Value::Integer(41.into()),
                )]),
                "the bundle version is not text",
            ),
        ];
        for (extensions, expected) in cases {
            let spec = Forge {
                raw_extensions: Some(extensions),
                ..Forge::default()
            };
            let forged = forge(&spec);
            let err = forged.verify(&spec, &forged.key_id).expect_err(expected);
            assert_eq!(err, AttestError::Shape(expected));
        }
    }

    // ---------------------------------------------------------------------
    // Assertions.
    // ---------------------------------------------------------------------

    const ASSERTION_APP_ID: &str = "1234567890.com.example.myapp";

    /// The counter on its own, for the tests whose subject is not the map that
    /// may follow it.
    fn counter_of(result: Result<VerifiedAssertion, AttestError>) -> Result<u32, AttestError> {
        result.map(|assertion| assertion.counter)
    }

    fn assertion_key() -> EcdsaKeyPair {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    }

    fn assertion(
        key: &EcdsaKeyPair,
        app_id: &str,
        counter: u32,
        client_data_hash: &[u8],
    ) -> Vec<u8> {
        assertion_with(key, app_id, counter, client_data_hash, None)
    }

    /// The same, with whatever an iOS 27 device would append after the counter.
    fn assertion_with(
        key: &EcdsaKeyPair,
        app_id: &str,
        counter: u32,
        client_data_hash: &[u8],
        extensions: Option<Value>,
    ) -> Vec<u8> {
        let mut auth_data = Vec::new();
        auth_data.extend_from_slice(&sha256(app_id.as_bytes()));
        auth_data.push(0x00);
        auth_data.extend_from_slice(&counter.to_be_bytes());
        if let Some(extensions) = &extensions {
            auth_data.extend_from_slice(&cbor(extensions));
        }
        let signature = key
            .sign(&SystemRandom::new(), &nonce(&auth_data, client_data_hash))
            .unwrap();
        cbor(&Value::Map(vec![
            (
                Value::Text("signature".to_string()),
                Value::Bytes(signature.as_ref().to_vec()),
            ),
            (
                Value::Text("authenticatorData".to_string()),
                Value::Bytes(auth_data),
            ),
        ]))
    }

    #[test]
    fn an_assertion_verifies_and_reports_the_counter_it_reached() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let object = assertion(&key, ASSERTION_APP_ID, 4, &hash);
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            )),
            Ok(4)
        );
    }

    /// **Signed, and still too short.** The counter sits at the end of a buffer
    /// whose length nothing else checks, so a signature the key really made is
    /// what makes this reach the bound rather than the signature check.
    #[test]
    fn an_assertion_too_short_to_carry_a_counter_is_refused_rather_than_sliced() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let mut auth_data = sha256(ASSERTION_APP_ID.as_bytes()).to_vec();
        auth_data.push(0x00);
        auth_data.extend_from_slice(&[0u8; 3]);
        assert_eq!(auth_data.len(), ASSERTION_AUTH_DATA_LEN - 1);

        let signature = key
            .sign(&SystemRandom::new(), &nonce(&auth_data, &hash))
            .unwrap();
        let object = cbor(&Value::Map(vec![
            (
                Value::Text("signature".to_string()),
                Value::Bytes(signature.as_ref().to_vec()),
            ),
            (
                Value::Text("authenticatorData".to_string()),
                Value::Bytes(auth_data),
            ),
        ]));

        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            )),
            Err(AttestError::Shape(
                "the authenticator data is too short to carry a counter"
            ))
        );
    }

    #[test]
    fn an_assertion_that_repeats_the_stored_counter_is_a_replay() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let object = assertion(&key, ASSERTION_APP_ID, 4, &hash);
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                4
            )),
            Err(AttestError::CounterNotAdvanced {
                stored: 4,
                offered: 4
            })
        );
    }

    #[test]
    fn an_assertion_that_moves_the_counter_backwards_is_refused() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let object = assertion(&key, ASSERTION_APP_ID, 2, &hash);
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                9
            )),
            Err(AttestError::CounterNotAdvanced {
                stored: 9,
                offered: 2
            })
        );
    }

    #[test]
    fn a_tampered_assertion_signature_is_refused() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let mut object = assertion(&key, ASSERTION_APP_ID, 4, &hash);
        let last = object.len() - 1;
        object[last] ^= 0xff;
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            )),
            Err(AttestError::Signature)
        );
    }

    #[test]
    fn an_assertion_for_another_app_is_refused() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let object = assertion(&key, "1234567890.com.example.otherapp", 4, &hash);
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            )),
            Err(AttestError::AppIdMismatch)
        );
    }

    #[test]
    fn an_assertion_signed_by_another_key_is_refused() {
        let key = assertion_key();
        let stranger = assertion_key();
        let hash = sha256(b"a challenge");
        let object = assertion(&stranger, ASSERTION_APP_ID, 4, &hash);
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            )),
            Err(AttestError::Signature)
        );
    }

    /// **An assertion says what the phone is running now, when it can.** iOS 27
    /// appends the same two values an attestation carries, under the camel-case
    /// spelling Apple's assertion steps use — and the byte string is read the
    /// little-endian way round, because the other way turns a 4 into 67108864.
    #[test]
    fn an_assertion_from_a_phone_that_appends_extensions_reports_the_build_it_is_running() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let extensions = Value::Map(vec![
            (
                Value::Text("validationCategory".to_string()),
                Value::Bytes(4u32.to_le_bytes().to_vec()),
            ),
            (
                Value::Text("bundleVersion".to_string()),
                Value::Text("41.3".to_string()),
            ),
        ]);
        let object = assertion_with(&key, ASSERTION_APP_ID, 4, &hash, Some(extensions));
        assert_eq!(
            verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            ),
            Ok(VerifiedAssertion {
                counter: 4,
                validation_category: Some(4),
                bundle_version: Some("41.3".to_string()),
            })
        );
    }

    /// The other spelling, which is the one Apple's attestation steps document —
    /// and the shape of every phone before iOS 27, which appends nothing and
    /// must not be refused for it.
    #[test]
    fn an_assertion_is_read_under_either_spelling_and_is_accepted_carrying_neither() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let snake = Value::Map(vec![
            (
                Value::Text("apple_validation_category_01".to_string()),
                Value::Bytes(2u32.to_le_bytes().to_vec()),
            ),
            (
                Value::Text("apple_bundle_version_01".to_string()),
                Value::Text("7".to_string()),
            ),
        ]);
        let object = assertion_with(&key, ASSERTION_APP_ID, 4, &hash, Some(snake));
        assert_eq!(
            verify_assertion(
                &object,
                &hash,
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            ),
            Ok(VerifiedAssertion {
                counter: 4,
                validation_category: Some(2),
                bundle_version: Some("7".to_string()),
            })
        );

        let bare = assertion(&key, ASSERTION_APP_ID, 4, &hash);
        assert_eq!(
            verify_assertion(&bare, &hash, key.public_key().as_ref(), ASSERTION_APP_ID, 3),
            Ok(VerifiedAssertion {
                counter: 4,
                validation_category: None,
                bundle_version: None,
            })
        );
    }

    /// The map is inside the signature, so a map of the wrong shape is a device
    /// producing bytes no device produces — refused by the rule it broke rather
    /// than read as whichever type is convenient.
    #[test]
    fn an_assertion_extensions_map_of_the_wrong_shape_is_refused_by_the_rule_it_broke() {
        let key = assertion_key();
        let hash = sha256(b"a challenge");
        let cases = [
            (
                Value::Text("extensions".to_string()),
                "the extensions are not a CBOR map",
            ),
            (
                Value::Map(vec![(
                    Value::Text("validationCategory".to_string()),
                    Value::Integer(4.into()),
                )]),
                "the validation category is not a byte string",
            ),
            (
                Value::Map(vec![(
                    Value::Text("validationCategory".to_string()),
                    Value::Bytes(vec![4, 0]),
                )]),
                "the validation category is not four bytes",
            ),
            (
                Value::Map(vec![(
                    Value::Text("bundleVersion".to_string()),
                    Value::Integer(41.into()),
                )]),
                "the bundle version is not text",
            ),
        ];
        for (extensions, expected) in cases {
            let object = assertion_with(&key, ASSERTION_APP_ID, 4, &hash, Some(extensions));
            assert_eq!(
                verify_assertion(
                    &object,
                    &hash,
                    key.public_key().as_ref(),
                    ASSERTION_APP_ID,
                    3
                ),
                Err(AttestError::Shape(expected))
            );
        }
    }

    /// A challenge the relay did not issue produces a different nonce, so the
    /// signature over it does not verify.
    #[test]
    fn an_assertion_over_another_challenge_is_refused() {
        let key = assertion_key();
        let object = assertion(&key, ASSERTION_APP_ID, 4, &sha256(b"a challenge"));
        assert_eq!(
            counter_of(verify_assertion(
                &object,
                &sha256(b"another challenge"),
                key.public_key().as_ref(),
                ASSERTION_APP_ID,
                3
            )),
            Err(AttestError::Signature)
        );
    }
}
