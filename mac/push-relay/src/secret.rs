//! The two values the relay is trusted with and never keeps: a device token and
//! a bearer credential.
//!
//! Both arrive on every push and both leave with the response. What survives is
//! a SHA-256 of each, which is enough to recognise a binding and not enough to
//! use one — an attacker holding the whole database still cannot address a
//! phone, because a 32-byte random bearer and a device token are not guessable
//! from their digests.
//!
//! The redaction is a type rather than a convention. A convention is a thing
//! every future call site has to remember at the moment it is writing a log
//! line about a failure, which is the moment nobody is careful.

use anyhow::{bail, Result};
use base64::Engine;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};

/// What every redaction prints, everywhere, so a log line reads the same
/// whether the value was present or not — an empty field would say which.
const REDACTED: &str = "<redacted>";

/// A device token shorter than this is not a token. Sixteen bytes is well under
/// anything Apple has issued and is here only to refuse the empty-ish input a
/// caller sends when it means "none".
const MIN_TOKEN_HEX: usize = 32;

/// And an upper bound that is deliberately **not** Apple's current 32 bytes.
///
/// Apple has changed device-token length before and documents no maximum, so a
/// relay that hard-coded 64 hex characters would start refusing every phone on
/// the day it changed — a total outage, arriving without a deploy. 256 hex
/// characters is four times anything issued and still small enough that the
/// request-body limit is the real bound.
const MAX_TOKEN_HEX: usize = 256;

/// The domain each digest is taken in.
///
/// `token_hash` and `bearer_hash` land in two columns of one table, and a bare
/// SHA-256 of each would make "is this value a token or a bearer?" a question
/// about lengths rather than about domains — today the answer is only that a
/// 43-character bearer has odd length and [`normalize_device_token`] refuses
/// odd lengths, which is an accident. The label goes in first, so one value
/// cannot produce both digests.
const TOKEN_DOMAIN: &[u8] = b"cc-token:";
const BEARER_DOMAIN: &[u8] = b"cc-bearer:";

/// A value that must not reach a log, a `Debug` dump, or an error message.
///
/// The inner string is reachable only through [`Secret::expose`], which is
/// named so that a reviewer can find every place it happens by searching for
/// one word.
///
/// **No `PartialEq`.** A derived one is a short-circuiting byte comparison over
/// a 32-byte credential, offered to every future call site by autocomplete.
/// Comparison here goes through [`bearer_hash`] and a database lookup, and a
/// test that needs two values to be the same compares [`Secret::expose`] so
/// that the intent is written down.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The raw value, for the one caller that has to put it on a wire.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// SHA-256, lowercase hex. The one shape anything persisted takes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// SHA-256 over a domain label and then the value.
///
/// **The label is part of the stored row.** Changing one — or adding a third
/// domain that reuses an existing label — makes every digest already in the
/// database unrecognisable, which is a migration and not an edit.
fn domain_hash(domain: &[u8], value: &[u8]) -> String {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(domain);
    context.update(value);
    hex(context.finish().as_ref())
}

/// A fresh bearer credential: 32 bytes from the system CSPRNG, base64url.
///
/// Unpadded because the value travels in an `Authorization` header and a `=` in
/// a header value is a thing some proxy eventually decides to interpret.
pub fn new_bearer() -> Result<Secret> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("the system random number generator refused"))?;
    Ok(Secret(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
    ))
}

/// The only form of a device token that is ever written down.
pub fn token_hash(token: &str) -> String {
    domain_hash(TOKEN_DOMAIN, token.as_bytes())
}

/// The only form of a bearer that is ever written down.
///
/// Takes a [`Secret`] rather than a `&str` so that a call site holding a raw
/// bearer has already had to name it as one.
pub fn bearer_hash(bearer: &Secret) -> String {
    domain_hash(BEARER_DOMAIN, bearer.0.as_bytes())
}

/// Lowercase hex, even length, bounded — or a refusal that says which rule
/// failed.
///
/// Normalising rather than merely checking matters because the token is the
/// rate-limit key and the binding key: the same phone sending uppercase from
/// one Mac and lowercase from another would otherwise be two bindings with two
/// budgets, and the second one would not be revoked when the first was.
pub fn normalize_device_token(raw: &str) -> Result<String> {
    if raw.is_empty() {
        bail!("the device token is empty");
    }
    if raw.len() < MIN_TOKEN_HEX || raw.len() > MAX_TOKEN_HEX {
        bail!(
            "the device token is {} characters; a token is between {MIN_TOKEN_HEX} and \
             {MAX_TOKEN_HEX} hex characters",
            raw.len()
        );
    }
    if raw.len() % 2 != 0 {
        bail!("the device token has an odd number of characters, so it is not whole bytes");
    }
    if !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the device token contains a character that is not a hex digit");
    }
    Ok(raw.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The whole point of the type.** A `Debug` that leaked even a prefix
    /// would leak it into every `anyhow` chain and every struct dump that ever
    /// contains one.
    #[test]
    fn a_secret_never_prints_its_value_or_any_prefix_of_it() {
        let value = "s3cret-bearer-value-with-entropy";
        let secret = Secret::new(value);

        let debug = format!("{secret:?}");
        let display = format!("{secret}");
        assert_eq!(debug, REDACTED);
        assert_eq!(display, REDACTED);
        assert!(!debug.contains(value));
        for len in 1..=value.len() {
            let prefix = &value[..len];
            assert!(
                !debug.contains(prefix),
                "the debug rendering leaked the {len}-character prefix {prefix:?}"
            );
            assert!(!display.contains(prefix));
        }
        // And inside something else, which is how it will actually appear.
        assert!(!format!("{:?}", Some(secret.clone())).contains(value));
        assert_eq!(secret.expose(), value);
    }

    #[test]
    fn sha256_matches_the_published_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[test]
    fn a_bearer_is_unpadded_base64url_and_never_the_same_twice() {
        let first = new_bearer().unwrap();
        let second = new_bearer().unwrap();
        assert_ne!(first.expose(), second.expose());
        // 32 bytes is 43 unpadded base64 characters.
        assert_eq!(first.expose().len(), 43);
        assert!(!first.expose().contains('='), "{:?}", first.expose().len());
        assert!(
            first
                .expose()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "a bearer travels in a header, so `+` and `/` are not available"
        );
        // The hash is what is stored, and it is not the bearer.
        let hash = bearer_hash(&first);
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, first.expose());
    }

    /// **One value, two columns, two different digests.** Without the domain
    /// label the only thing keeping a bearer from being a token digest is that
    /// the lengths happen not to overlap.
    #[test]
    fn the_same_value_hashed_as_a_token_and_as_a_bearer_is_two_different_digests() {
        let value = "aabbccddeeff00112233445566778899";
        let as_token = token_hash(value);
        let as_bearer = bearer_hash(&Secret::new(value));
        assert_ne!(as_token, as_bearer);
        // And neither is the undomained digest a reader would try first.
        let bare = sha256_hex(value.as_bytes());
        assert_ne!(as_token, bare);
        assert_ne!(as_bearer, bare);
        assert_eq!(as_token.len(), 64);
        assert_eq!(as_bearer.len(), 64);

        // The domain is a prefix of the input and not of the output, so a
        // digest is still 32 bytes of nothing legible.
        assert!(!as_token.contains("cc-token"));
        assert!(!as_bearer.contains("cc-bearer"));
    }

    #[test]
    fn an_uppercase_token_normalises_rather_than_becoming_a_second_binding() {
        let upper = "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF0011";
        let lower = upper.to_ascii_lowercase();
        assert_eq!(normalize_device_token(upper).unwrap(), lower);
        assert_eq!(normalize_device_token(&lower).unwrap(), lower);
        assert_eq!(
            token_hash(&normalize_device_token(upper).unwrap()),
            token_hash(&normalize_device_token(&lower).unwrap())
        );
    }

    #[test]
    fn anything_that_is_not_a_whole_hex_token_is_refused_by_name() {
        let cases = [
            ("", "empty"),
            ("aabbccddeeff00112233445566778899aabbccddeeff001", "odd"),
            ("aabbccddeeff00112233445566778899aabbccddeeff00zz", "hex"),
            ("aabbcc", "characters"),
            ("aabbccddeeff001122334455667788 9aabbccddeeff0011", "hex"),
        ];
        for (input, expected) in cases {
            let err = normalize_device_token(input)
                .err()
                .unwrap_or_else(|| panic!("{input:?} must be refused"))
                .to_string();
            assert!(err.contains(expected), "{input:?} refused with {err:?}");
        }
        let absurd = "ab".repeat(MAX_TOKEN_HEX);
        assert!(normalize_device_token(&absurd).is_err());
    }

    /// The bound must not be Apple's current length: a relay pinned to 64 hex
    /// characters stops delivering to every phone the day Apple changes it.
    #[test]
    fn a_token_longer_than_apples_current_one_is_still_accepted() {
        let long = "ab".repeat(64);
        assert_eq!(long.len(), 128);
        assert!(normalize_device_token(&long).is_ok());
    }
}
