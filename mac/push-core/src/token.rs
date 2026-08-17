//! The APNs provider token: an ES256 JWT signed with Apple's `.p8` key.
//!
//! Apple authenticates a push sender one of two ways, and this is the one that
//! does not expire annually: a JWT signed with a P-256 key downloaded once from
//! the developer portal. Three facts are baked in and none of them is guessable,
//! so all three are configuration rather than constants:
//!
//!   * **`kid`** — the key's own ten-character id, which is also in its filename.
//!   * **`iss`** — the team id. Apple rejects a token whose issuer is not the
//!     team the key belongs to, with a `403 InvalidProviderToken` that says
//!     nothing about which of the two was wrong.
//!   * **`iat`** — issued-at. Apple refuses a token older than **one hour** and
//!     also refuses one minted more than once every **20 minutes**, so the token
//!     is cached and reused until it is genuinely stale. Both limits are Apple's,
//!     and violating the second earns a `429 TooManyProviderTokenUpdates` that
//!     looks exactly like a rate limit on the pushes themselves.
//!
//! The signature is **fixed-width `r||s`**, not DER. `ring`'s
//! `ECDSA_P256_SHA256_FIXED_SIGNING` produces exactly that; the ASN.1 variant
//! sitting next to it in the same module produces a token Apple rejects without
//! explanation.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

/// Apple refuses a provider token older than an hour. Renewed well inside that,
/// and still far outside the 20-minute floor on how often a new one may be
/// minted.
const RENEW_AFTER: Duration = Duration::from_secs(40 * 60);

/// What identifies this sender to Apple. Every field comes from the developer
/// account; none has a sensible default, so there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApnsIdentity {
    /// The `.p8`'s key id — the ten characters between `AuthKey_` and `.p8`
    /// in the filename Apple gives you.
    pub key_id: String,
    /// The Apple Developer team id.
    pub team_id: String,
    /// The app's bundle id, sent as the `apns-topic` header.
    pub topic: String,
}

pub struct ProviderToken {
    identity: ApnsIdentity,
    key: EcdsaKeyPair,
    rng: SystemRandom,
    cached: Mutex<Option<(String, SystemTime)>>,
}

impl ProviderToken {
    /// Load a `.p8` from disk.
    ///
    /// The file is PKCS#8 PEM, which `rustls-pemfile` already parses for the
    /// `wss://` path — so the armour is handled by a crate that is here anyway
    /// rather than by a hand-rolled base64 decoder.
    pub fn load(path: &std::path::Path, identity: ApnsIdentity) -> Result<Self> {
        let pem = std::fs::read(path)
            .with_context(|| format!("reading the APNs key at {}", path.display()))?;
        let mut cursor = std::io::Cursor::new(pem);
        let der = rustls_pemfile::pkcs8_private_keys(&mut cursor)
            .next()
            .transpose()
            .context("parsing the APNs key as PKCS#8 PEM")?
            .with_context(|| {
                format!(
                    "{} contains no PKCS#8 private key; an APNs key is the PEM \
                     file Apple hands you, unconverted",
                    path.display()
                )
            })?;
        Self::from_pkcs8(der.secret_pkcs8_der(), identity)
    }

    pub fn from_pkcs8(der: &[u8], identity: ApnsIdentity) -> Result<Self> {
        if identity.key_id.is_empty() || identity.team_id.is_empty() || identity.topic.is_empty() {
            bail!("APNs needs a key id, a team id and a topic; none of them has a default");
        }
        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &rng)
            .map_err(|_| anyhow::anyhow!("the APNs key is not a P-256 PKCS#8 key"))?;
        Ok(Self {
            identity,
            key,
            rng,
            cached: Mutex::new(None),
        })
    }

    pub fn topic(&self) -> &str {
        &self.identity.topic
    }

    /// The current bearer token, minting a new one only once the old one is
    /// approaching Apple's one-hour limit.
    pub fn bearer(&self) -> Result<String> {
        self.bearer_at(SystemTime::now())
    }

    fn bearer_at(&self, now: SystemTime) -> Result<String> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((token, minted)) = cached.as_ref() {
            if now.duration_since(*minted).unwrap_or(RENEW_AFTER) < RENEW_AFTER {
                return Ok(token.clone());
            }
        }
        let token = self.mint(now)?;
        *cached = Some((token.clone(), now));
        Ok(token)
    }

    fn mint(&self, now: SystemTime) -> Result<String> {
        let issued = now
            .duration_since(UNIX_EPOCH)
            .context("the system clock is before 1970")?
            .as_secs();
        let header = format!(
            r#"{{"alg":"ES256","kid":"{}"}}"#,
            escape(&self.identity.key_id)
        );
        let claims = format!(
            r#"{{"iss":"{}","iat":{issued}}}"#,
            escape(&self.identity.team_id)
        );
        let signing_input = format!(
            "{}.{}",
            b64url(header.as_bytes()),
            b64url(claims.as_bytes())
        );
        let signature = self
            .key
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| anyhow::anyhow!("signing the APNs provider token failed"))?;
        Ok(format!("{signing_input}.{}", b64url(signature.as_ref())))
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url **without padding**, which is what JWT requires. Padded output is
/// accepted by some verifiers and rejected by Apple's.
fn b64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let quad = [
            ALPHABET[(n >> 18) as usize & 63],
            ALPHABET[(n >> 12) as usize & 63],
            ALPHABET[(n >> 6) as usize & 63],
            ALPHABET[n as usize & 63],
        ];
        // 1 input byte yields 2 output characters, 2 yields 3, 3 yields 4.
        out.push_str(std::str::from_utf8(&quad[..chunk.len() + 1]).unwrap_or_default());
    }
    out
}

/// The three identity fields are operator-supplied and land inside a JSON
/// string, so a stray quote would produce a malformed token rather than a
/// signed one.
fn escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider token over a key minted in this process. Checking one in
    /// would be a private key in the repository, and Apple's `.p8` is the one
    /// secret this crate exists to hold.
    fn provider() -> ProviderToken {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        ProviderToken::from_pkcs8(
            pkcs8.as_ref(),
            ApnsIdentity {
                key_id: "KEYIDABCDE".to_string(),
                team_id: "TEAMIDWXYZ".to_string(),
                topic: "com.example.app".to_string(),
            },
        )
        .unwrap()
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    /// Base64url back to bytes, so the assertions below are about the documents
    /// Apple parses rather than about the strings this module printed.
    fn decode(text: &str) -> Vec<u8> {
        let mut bits = 0u32;
        let mut held = 0u32;
        let mut out = Vec::new();
        for character in text.bytes() {
            let value = ALPHABET
                .iter()
                .position(|entry| *entry == character)
                .unwrap_or_else(|| panic!("{text} is not base64url"));
            bits = (bits << 6) | value as u32;
            held += 6;
            if held >= 8 {
                held -= 8;
                out.push((bits >> held) as u8);
            }
        }
        out
    }

    /// **The cache is what this module is for.** Apple refuses a provider token
    /// minted more often than once every twenty minutes with a `429
    /// TooManyProviderTokenUpdates`, which arrives looking exactly like a rate
    /// limit on the pushes — so minting per push fails under the load the relay
    /// exists to carry, and only under that load.
    #[test]
    fn one_token_serves_every_push_until_it_approaches_apples_hour() {
        let token = provider();
        let minted = 1_800_000_000;

        let first = token.bearer_at(at(minted)).unwrap();
        assert_eq!(
            token.bearer_at(at(minted + 39 * 60)).unwrap(),
            first,
            "a token minted 39 minutes ago is the token Apple still accepts"
        );

        let renewed = token.bearer_at(at(minted + 41 * 60)).unwrap();
        assert_ne!(
            renewed, first,
            "a token approaching the hour is replaced before Apple refuses it"
        );
        // And the replacement is now the one being reused.
        assert_eq!(token.bearer_at(at(minted + 41 * 60 + 60)).unwrap(), renewed);
    }

    /// The three things Apple checks and answers about with a `403` that names
    /// none of them.
    #[test]
    fn the_token_is_an_es256_jwt_whose_signature_is_the_fixed_width_pair() {
        let issued = 1_800_000_000;
        let token = provider().bearer_at(at(issued)).unwrap();

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "{token}");
        assert_eq!(
            decode(parts[0]),
            br#"{"alg":"ES256","kid":"KEYIDABCDE"}"#.to_vec()
        );
        // `iat` is seconds since the epoch, not an age: Apple measures it
        // against its own clock and refuses anything over an hour old.
        assert_eq!(
            decode(parts[1]),
            format!(r#"{{"iss":"TEAMIDWXYZ","iat":{issued}}}"#).into_bytes()
        );
        // 64 bytes of `r||s`. The ASN.1 form of the same signature is about
        // seventy bytes and is refused by Apple without explanation.
        assert_eq!(decode(parts[2]).len(), 64);
    }

    #[test]
    fn base64url_is_unpadded_and_uses_the_url_alphabet() {
        // RFC 4648 vectors, with padding removed.
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
        // `+` and `/` never appear; `-` and `_` do.
        let bytes = [0xfb, 0xff, 0xfe];
        let encoded = b64url(&bytes);
        assert!(
            !encoded.contains('+') && !encoded.contains('/'),
            "{encoded}"
        );
        assert!(encoded.contains('-') || encoded.contains('_'), "{encoded}");
        assert!(
            !encoded.contains('='),
            "JWT base64url is unpadded: {encoded}"
        );
    }

    #[test]
    fn an_identity_field_cannot_break_out_of_the_json() {
        assert_eq!(escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape(r"a\b"), r"a\\b");
    }

    #[test]
    fn a_missing_identity_field_is_refused_rather_than_defaulted() {
        // A zeroed key would fail later anyway; what is asserted here is that
        // the *identity* is checked first, because an empty team id produces a
        // 403 from Apple that names neither field.
        let err = ProviderToken::from_pkcs8(
            &[0u8; 8],
            ApnsIdentity {
                key_id: String::new(),
                team_id: "T".into(),
                topic: "com.example".into(),
            },
        )
        .err()
        .expect("an empty key id must be refused");
        assert!(err.to_string().contains("key id"), "{err}");
    }
}
