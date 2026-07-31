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

use std::io::Read;

/// Fill `N` bytes from the kernel CSPRNG.
pub fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
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
}
