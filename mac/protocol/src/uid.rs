//! `session_uid` — the stable, unique identity of one agent run.
//!
//! ## Why this exists
//!
//! `cc-1` is a *tmux session name*. `cc claude` picks the lowest free one, so
//! when a session dies the next one is called `cc-1` again. That name was
//! originally the event log's primary key too, which meant a new session
//! inherited the dead one's log and continued its `seq` numbering. Two concrete
//! failures came out of that:
//!
//!   * **History integrity.** One `cc-1` timeline on the phone was actually two
//!     unrelated agent runs spliced together, with no marker between them.
//!   * **Approval safety.** A `request_id` answered in the first run stayed in
//!     the answers ledger under the same session key, so a card from the second
//!     run that happened to reuse an id would be reported as an already-applied
//!     duplicate — an answer nobody gave.
//!
//! The fix is to separate the two jobs the name was doing. `session_id` stays
//! the human-facing tmux name (display, `cc attach`, `send-keys` targeting);
//! `session_uid` is minted once at spawn, never reused, and is what the log,
//! the tail cursors and the answers ledger are keyed by.
//!
//! ## Why ULID rather than a UUID
//!
//! A ULID is 48 bits of millisecond timestamp followed by 80 bits of entropy,
//! rendered in Crockford Base32. Two properties earn it the choice:
//!
//!   * **It sorts by creation time as a plain string**, so "the newest `cc-1`"
//!     — the resolution a phone that only knows the legacy name needs — is a
//!     string comparison rather than a join against `created_at`.
//!   * **It is one case-insensitive token with no separators**, so it survives
//!     being a filename, a tmux argument and a JSON key without quoting.
//!
//! 80 bits of entropy per millisecond is the collision argument: two sessions
//! minted in the same millisecond collide with probability 2^-80.
//!
//! Implemented here rather than pulled in as a crate: the whole of it is the
//! forty lines below, it needs no randomness source beyond the one the daemon
//! already uses for tokens, and a dependency for this would arrive with its own
//! RNG stack.

/// Crockford Base32. Excludes `I`, `L`, `O` and `U` so a uid read aloud or
/// re-typed from a log cannot become a different, equally valid uid.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 26 Base32 symbols carry 130 bits; a ULID is 128, so the leading symbol holds
/// only 3 bits and can never exceed `7`.
pub const UID_LEN: usize = 26;

/// Mint a new uid from the current time and kernel entropy.
pub fn new() -> std::io::Result<String> {
    at(crate::time::now_unix_ms())
}

/// Mint a uid that sorts as though it were created at `unix_ms`.
///
/// Used when an upgraded daemon synthesises identities for sessions that
/// predate them, so the migrated rows keep their real ordering instead of all
/// appearing to have been created at migration time.
pub fn at(unix_ms: i64) -> std::io::Result<String> {
    Ok(encode(compose(
        unix_ms,
        crate::secret::random_bytes::<10>()?,
    )))
}

/// Is this a syntactically valid uid? Accepts either case; [`new`] always emits
/// upper case.
pub fn is_well_formed(value: &str) -> bool {
    value.len() == UID_LEN
        && value
            .bytes()
            .all(|byte| symbol_value(byte).is_some())
        // The leading symbol carries 3 bits. Anything above 7 is 129 bits of
        // "ULID" and decodes to a different value than it displays.
        && symbol_value(value.as_bytes()[0]).is_some_and(|v| v < 8)
}

/// The millisecond the uid was minted, or `None` if it is not a uid.
pub fn timestamp_ms(value: &str) -> Option<i64> {
    decode(value).map(|bits| (bits >> 80) as i64)
}

fn compose(unix_ms: i64, entropy: [u8; 10]) -> u128 {
    // A negative or absurd clock must not silently wrap into another session's
    // range; clamping keeps the value inside the 48 bits the format allows.
    let ms = unix_ms.clamp(0, (1i64 << 48) - 1) as u128;
    let mut bits = ms << 80;
    for (index, byte) in entropy.iter().enumerate() {
        bits |= (*byte as u128) << (72 - index * 8);
    }
    bits
}

fn encode(bits: u128) -> String {
    let mut out = String::with_capacity(UID_LEN);
    for position in (0..UID_LEN).rev() {
        let shift = position * 5;
        let index = ((bits >> shift) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

fn decode(value: &str) -> Option<u128> {
    if value.len() != UID_LEN {
        return None;
    }
    let mut bits: u128 = 0;
    for byte in value.bytes() {
        bits = bits
            .checked_mul(32)?
            .checked_add(symbol_value(byte)? as u128)?;
    }
    Some(bits)
}

fn symbol_value(byte: u8) -> Option<u8> {
    let upper = byte.to_ascii_uppercase();
    ALPHABET
        .iter()
        .position(|symbol| *symbol == upper)
        .map(|index| index as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_minted_uid_is_well_formed() {
        let uid = new().unwrap();
        assert_eq!(uid.len(), UID_LEN);
        assert!(is_well_formed(&uid), "{uid}");
        assert!(uid
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn uids_are_unique_across_a_burst() {
        // The collision this whole module exists to prevent. A tight loop mints
        // many uids inside one millisecond, so this exercises the entropy half
        // rather than the clock half.
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            assert!(seen.insert(new().unwrap()), "uids must never repeat");
        }
    }

    #[test]
    fn uids_sort_by_creation_time_as_plain_strings() {
        let early = at(1_600_000_000_000).unwrap();
        let late = at(1_700_000_000_000).unwrap();
        assert!(early < late, "{early} should sort before {late}");
        // And within one millisecond the ordering is arbitrary but total, which
        // is all "newest session with this name" needs.
        let a = at(1_700_000_000_000).unwrap();
        let b = at(1_700_000_000_000).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_timestamp_survives_the_round_trip() {
        for ms in [0i64, 1, 1_700_000_000_000, (1i64 << 48) - 1] {
            let uid = at(ms).unwrap();
            assert_eq!(timestamp_ms(&uid), Some(ms), "{uid}");
        }
    }

    #[test]
    fn an_impossible_clock_is_clamped_rather_than_wrapped() {
        // A machine with a broken clock must not mint a uid that sorts into
        // another session's range or overflows into the entropy bits.
        let negative = at(-1).unwrap();
        assert_eq!(timestamp_ms(&negative), Some(0));
        let far_future = at(i64::MAX).unwrap();
        assert_eq!(timestamp_ms(&far_future), Some((1i64 << 48) - 1));
        assert!(is_well_formed(&far_future));
    }

    #[test]
    fn malformed_uids_are_rejected() {
        for bad in [
            "",
            "cc-1",
            // One symbol short, one symbol long.
            "01ARZ3NDEKTSV4RRFFQ69G5FA",
            "01ARZ3NDEKTSV4RRFFQ69G5FAVX",
            // `I`, `L`, `O` and `U` are not in the alphabet.
            "01ARZ3NDEKTSV4RRFFQ69G5FIV",
            "01ARZ3NDEKTSV4RRFFQ69G5FOV",
            // 129 bits: displays as a uid, decodes as something else.
            "81ARZ3NDEKTSV4RRFFQ69G5FAV",
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZ",
        ] {
            assert!(!is_well_formed(bad), "{bad:?} must not pass as a uid");
        }
        // Overflowing the 128 bits is rejected by the decoder too, not only by
        // the shape check — so a caller that skips validation still cannot get a
        // wrong timestamp out of a wrong uid.
        assert_eq!(timestamp_ms("ZZZZZZZZZZZZZZZZZZZZZZZZZZ"), None);
        assert_eq!(timestamp_ms("cc-1"), None);
    }

    #[test]
    fn decoding_accepts_either_case() {
        let uid = new().unwrap();
        assert!(is_well_formed(&uid.to_lowercase()));
        assert_eq!(
            timestamp_ms(&uid.to_lowercase()),
            timestamp_ms(&uid),
            "case must not change the value"
        );
    }

    #[test]
    fn the_encoding_is_the_documented_crockford_one() {
        // Pinned against the ULID specification's own example, so a rewrite of
        // the bit-twiddling cannot silently change the format.
        assert_eq!(encode(0), "00000000000000000000000000");
        assert_eq!(encode(u128::MAX), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
        assert_eq!(decode("7ZZZZZZZZZZZZZZZZZZZZZZZZZ"), Some(u128::MAX));
        assert_eq!(
            decode(&encode(12345678901234567890)),
            Some(12345678901234567890)
        );
        assert!(!ALPHABET.contains(&b'I'));
        assert!(!ALPHABET.contains(&b'L'));
        assert!(!ALPHABET.contains(&b'O'));
        assert!(!ALPHABET.contains(&b'U'));
    }
}
