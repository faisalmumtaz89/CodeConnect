//! Secret generation.
//!
//! One source of randomness for the whole daemon: `/dev/urandom`. It is the
//! kernel CSPRNG, it never blocks after boot on macOS, and reading it needs no
//! crate. A failed read is a hard error rather than a fallback to something
//! weaker — a "random" pairing code that is not random is worse than no
//! pairing at all, and there is no situation where guessing is the right
//! recovery from "the kernel will not give me entropy".

use std::io::Read;

use anyhow::{Context, Result};

/// Fill `N` bytes from the kernel CSPRNG.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)
        .context("reading /dev/urandom")?;
    Ok(bytes)
}

/// An 8-symbol pairing code over `protocol::pairing::PAIRING_ALPHABET`.
///
/// The alphabet is exactly 32 symbols, so masking to 5 bits is a uniform
/// selection. That is not a coincidence to be rediscovered later: with any
/// other size, `byte % len` would bias the low symbols and quietly cost the
/// code some of the entropy its safety argument depends on.
pub fn pairing_code() -> Result<String> {
    let alphabet = protocol::pairing::PAIRING_ALPHABET;
    debug_assert_eq!(
        alphabet.len(),
        32,
        "masking assumes a power-of-two alphabet"
    );
    let bytes = random_bytes::<{ protocol::pairing::PAIRING_CODE_LEN }>()?;
    Ok(bytes
        .iter()
        .map(|byte| alphabet[(byte & 0x1f) as usize] as char)
        .collect())
}

/// 256 bits, hex. What the phone stores in its Keychain and presents forever.
pub fn device_token() -> Result<String> {
    Ok(hex(&random_bytes::<32>()?))
}

/// A short, stable handle for a device. 48 bits: unguessable is not a
/// requirement (it is not a credential), uniqueness across a handful of
/// devices is, and it has to be short enough to type as a prefix.
pub fn device_id() -> Result<String> {
    Ok(hex(&random_bytes::<6>()?))
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn pairing_codes_are_well_formed_and_unique() {
        let mut seen = HashSet::new();
        for _ in 0..500 {
            let code = pairing_code().unwrap();
            assert!(
                protocol::pairing::is_well_formed(&code),
                "generated an unusable code: {code}"
            );
            assert!(seen.insert(code), "500 draws from 2^40 must not repeat");
        }
    }

    #[test]
    fn every_symbol_of_the_alphabet_is_reachable() {
        // A masking bug would silently shrink the alphabet and the entropy with
        // it. 4000 symbols over 32 slots makes a missing one conclusive.
        let mut seen = HashSet::new();
        for _ in 0..500 {
            seen.extend(pairing_code().unwrap().bytes());
        }
        assert_eq!(
            seen.len(),
            protocol::pairing::PAIRING_ALPHABET.len(),
            "some symbols are never generated"
        );
    }

    #[test]
    fn device_tokens_are_256_bits_of_hex() {
        let token = device_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, device_token().unwrap());
    }

    #[test]
    fn device_ids_are_short_hex_and_prefix_matchable() {
        let id = device_id().unwrap();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(device_id().unwrap()));
        }
    }

    #[test]
    fn hex_encodes_every_byte_value() {
        assert_eq!(hex(&[0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
        assert_eq!(hex(&[]), "");
    }
}
