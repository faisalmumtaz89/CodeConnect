//! The one source of randomness in the workspace.
//!
//! `/dev/urandom` is the kernel CSPRNG, it never blocks after boot on macOS, and
//! reading it needs no crate. A failed read is a hard error rather than a
//! fallback to something weaker: a "random" session uid that is not random would
//! silently reintroduce the identity collision this whole mechanism exists to
//! remove, and there is no situation where guessing is the right recovery from
//! "the kernel will not give me entropy".
//!
//! It lives in `protocol` rather than in `ccd` because both the daemon (device
//! tokens, pairing codes) and the shim (session uids, minted at spawn before any
//! daemon is involved) need it, and two readers of the same device is exactly the
//! duplication that eventually drifts.

//! `std::io::Result` rather than `anyhow`: `protocol` is the crate every other
//! one depends on, and an error type is not worth a dependency when the only
//! failure is a read.
//!
//! It is also where a secret that arrives *from* the wire is given a type that
//! will not print it — see [`Redacted`].

use std::io::Read;

use serde::{Deserialize, Serialize};

/// Fill `N` bytes from the kernel CSPRNG.
pub fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// What a value renders as when it is a secret. Fixed text rather than an
/// elision of the real one: a prefix is a head start, and a length is a fact
/// about the value.
const REDACTED: &str = "<redacted>";

/// A string from the wire that must not print itself.
///
/// **The hazard is the derive, not the log line.** Every message this crate
/// defines derives `Debug`, and a bearer credential sitting in one as a bare
/// `String` reaches a log the first time anybody writes `{message:?}` — in a
/// parse-error branch, a panic message, or an `anyhow` context three layers
/// away that nobody was thinking about secrets while writing. A type that
/// cannot render itself removes the possibility rather than relying on every
/// future author noticing.
///
/// `serde(transparent)`, so it is exactly the string it was on the wire and no
/// peer can tell it apart from one.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Redacted(String);

impl Redacted {
    /// The value itself, at a call site that has had to name what it is asking
    /// for.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Redacted {
    fn from(value: String) -> Self {
        Redacted(value)
    }
}

impl From<&str> for Redacted {
    fn from(value: &str) -> Self {
        Redacted(value.to_string())
    }
}

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl std::fmt::Display for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn draws_are_the_requested_length_and_do_not_repeat() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let bytes = random_bytes::<16>().expect("the kernel must give us entropy");
            assert_eq!(bytes.len(), 16);
            assert!(seen.insert(bytes), "200 draws from 2^128 must not repeat");
        }
    }

    #[test]
    fn a_zero_length_draw_is_legal_and_empty() {
        assert_eq!(random_bytes::<0>().unwrap(), [0u8; 0]);
    }

    /// **The whole point of the type.** A `Debug` that leaked even a prefix
    /// would leak it into every message dump and every error chain that ever
    /// contained one.
    #[test]
    fn a_redacted_value_never_prints_itself_or_any_part_of_it() {
        let secret = Redacted::from("s3cret-bearer-value-with-entropy");
        for rendered in [
            format!("{secret:?}"),
            format!("{secret}"),
            format!("{:?}", Some(secret.clone())),
            format!("{:?}", vec![secret.clone()]),
        ] {
            assert!(!rendered.contains("s3cret"), "{rendered}");
            assert!(rendered.contains(REDACTED), "{rendered}");
        }
        assert_eq!(secret.expose(), "s3cret-bearer-value-with-entropy");
    }

    /// Transparent on the wire: a peer cannot tell it from the string it was,
    /// which is what lets the type be introduced without a protocol change.
    #[test]
    fn the_wrapper_is_invisible_on_the_wire() {
        let secret = Redacted::from("opaque");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"opaque\"");
        assert_eq!(
            serde_json::from_str::<Redacted>("\"opaque\"").unwrap(),
            secret
        );
    }
}
