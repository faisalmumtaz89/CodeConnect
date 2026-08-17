//! What a device token is, decided once for both ends of the wire.
//!
//! **Two copies of this rule is an outage nobody can see.** The daemon decides
//! whether to store a token; the relay decides whether to act on one, and keys
//! a binding and a rate-limit budget by it. A daemon that accepted a token the
//! relay refuses would file a registration that fails on every push for ever,
//! reported to the user as an opaque `400` — and no test on either side would
//! go red, because each would be testing its own rule. So the rule lives here,
//! where neither can hold a different one.

use anyhow::{bail, Result};

/// A device token shorter than this is not a token. Sixteen bytes is well under
/// anything Apple has issued and is here only to refuse the empty-ish input a
/// caller sends when it means "none".
pub const MIN_TOKEN_HEX: usize = 32;

/// And an upper bound that is deliberately **not** Apple's current length.
///
/// Apple has changed device-token length before and documents no maximum, so a
/// sender that hard-coded 64 hex characters would start refusing every phone on
/// the day it changed — a total outage, arriving without a deploy. 256 hex
/// characters is four times anything issued and still small enough that a
/// request-body limit is the real bound.
pub const MAX_TOKEN_HEX: usize = 256;

/// Lowercase hex, even length, bounded — or a refusal naming the rule it broke.
///
/// **Normalising rather than merely checking matters** because the token is the
/// binding key and the rate-limit key: the same phone sending uppercase from one
/// Mac and lowercase from another would otherwise be two bindings with two
/// budgets, and revoking one would leave the other sending. Even length because
/// a token is whole bytes; a value with an odd count is not a truncated token,
/// it is not a token.
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

    const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    /// Case is normalised away rather than refused: a phone's token is the same
    /// token whichever Mac reports it, and two spellings would be two bindings.
    #[test]
    fn the_same_token_in_any_case_is_one_token() {
        assert_eq!(
            normalize_device_token(&TOKEN.to_ascii_uppercase()).unwrap(),
            TOKEN
        );
        assert_eq!(normalize_device_token(TOKEN).unwrap(), TOKEN);
    }

    #[test]
    fn every_rule_refuses_with_the_reason_it_refused_for() {
        for (bad, because) in [
            ("", "empty"),
            ("aabb", "characters"),
            ("aabbccddeeff001122334455667788zz", "hex digit"),
        ] {
            let err = normalize_device_token(bad).expect_err("accepted {bad:?}");
            assert!(err.to_string().contains(because), "{bad:?}: {err}");
        }

        let odd = "a".repeat(MIN_TOKEN_HEX + 1);
        assert!(normalize_device_token(&odd)
            .expect_err("an odd length is not whole bytes")
            .to_string()
            .contains("whole bytes"));
    }

    /// The boundaries themselves, on the accepting side — so a change to either
    /// bound is a deliberate act rather than a silently moved edge.
    #[test]
    fn the_bounds_are_inclusive_at_both_ends() {
        assert!(normalize_device_token(&"a".repeat(MIN_TOKEN_HEX)).is_ok());
        assert!(normalize_device_token(&"a".repeat(MIN_TOKEN_HEX - 2)).is_err());
        assert!(normalize_device_token(&"a".repeat(MAX_TOKEN_HEX)).is_ok());
        assert!(normalize_device_token(&"a".repeat(MAX_TOKEN_HEX + 2)).is_err());
    }
}
