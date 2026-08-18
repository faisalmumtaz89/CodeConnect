//! Payload hashing for the stale-approval race.
//!
//! `payload_hash` is a hash of the *exact text the phone displayed*. The daemon
//! recomputes it from its own record and rejects a mismatch, so an approval
//! tapped against a stale card can never be applied to a different command.

use sha2::{Digest, Sha256};

/// Raw digest, for callers that need the bytes rather than the hex text.
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256_bytes(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Hand-rolled hex keeps the dependency list at one crate.
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Canonical text for an approval card: the tool name plus its input rendered
/// as sorted-key JSON. `serde_json::Value` maps are BTreeMap-backed only with
/// the `preserve_order` feature *off*, which is the default — so key order is
/// already deterministic. We still round-trip through `to_string` on the Value
/// rather than the raw stdin bytes, because whitespace in the hook's stdin is
/// not guaranteed stable across Claude Code versions.
pub fn approval_payload_text(tool_name: &str, tool_input: &serde_json::Value) -> String {
    format!("{tool_name}\n{tool_input}")
}

pub fn approval_payload_hash(tool_name: &str, tool_input: &serde_json::Value) -> String {
    sha256_hex(approval_payload_text(tool_name, tool_input).as_bytes())
}

/// Identity of one `send_text` mutation: what is typed, where, and whether it
/// is submitted.
///
/// Not the text alone. A `request_id` makes a retry idempotent, but only if the
/// daemon can also tell a *retry* from a *different* mutation reusing the id —
/// otherwise a captured frame could be replayed with new text under an id the
/// ledger already trusts. Binding the target and the submit flag as well means
/// the ledger's answer to "is this the same mutation?" cannot be forged by
/// changing any part of what would actually be typed.
///
/// The session is hashed exactly as the client named it (a uid or a tmux name),
/// because that is the only string both sides can agree on before the daemon
/// has resolved it.
///
/// Fields are length-prefixed rather than joined by a separator. A plain
/// `"{session}\n{submit}\n{text}"` is ambiguous the moment `text` contains a
/// newline — `("cc-1", "true\nx")` and `("cc-1\ntrue", "x")` produce identical
/// material — and an ambiguity in a hash that authorises typing is a way to
/// make one mutation answer for another.
pub fn send_text_hash(session_ref: &str, text: &str, submit: bool) -> String {
    let mut material = String::from("codeconnect.send_text.v1");
    for field in [session_ref, if submit { "submit" } else { "stage" }, text] {
        material.push('\n');
        material.push_str(&field.len().to_string());
        material.push(':');
        material.push_str(field);
    }
    sha256_hex(material.as_bytes())
}

/// Identity of one `interrupt` mutation: which session's which turn is aborted.
///
/// The same length-prefixed, domain-tagged shape as [`send_text_hash`], for the
/// same reason: a retry with the same `request_id` but a different target turn
/// must be recognised as a *different* mutation and refused, not silently
/// treated as a replay that aborts the wrong turn. There is no submit flag —
/// aborting is not staged.
pub fn interrupt_hash(session_ref: &str, turn_id: &str) -> String {
    let mut material = String::from("codeconnect.interrupt.v1");
    for field in [session_ref, turn_id] {
        material.push('\n');
        material.push_str(&field.len().to_string());
        material.push(':');
        material.push_str(field);
    }
    sha256_hex(material.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_vector() {
        // NIST/RFC-6234 canonical vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn payload_hash_is_key_order_independent() {
        let a = json!({"command": "touch /tmp/x", "description": "d"});
        let b: serde_json::Value =
            serde_json::from_str(r#"{"description":"d","command":"touch /tmp/x"}"#).unwrap();
        assert_eq!(
            approval_payload_hash("Bash", &a),
            approval_payload_hash("Bash", &b)
        );
    }

    #[test]
    fn payload_hash_detects_command_change() {
        let a = json!({"command": "touch /tmp/x"});
        let b = json!({"command": "rm -rf /tmp/x"});
        assert_ne!(
            approval_payload_hash("Bash", &a),
            approval_payload_hash("Bash", &b)
        );
    }

    #[test]
    fn payload_hash_detects_tool_change() {
        let input = json!({"command": "ls"});
        assert_ne!(
            approval_payload_hash("Bash", &input),
            approval_payload_hash("Write", &input)
        );
    }

    /// The exact vector the iOS client pins too (SendTextIdentityTests): one
    /// literal on each side is what proves the two implementations are the
    /// same function rather than two functions that agree on easy inputs.
    #[test]
    fn send_text_hash_matches_the_cross_language_vector() {
        assert_eq!(
            send_text_hash("cc-1", "hi", true),
            "832d56d28203c01645209f9b61d192de468301d1c2bdb6090a607b15ab8026a9"
        );
    }

    #[test]
    fn send_text_hash_covers_every_part_of_the_mutation() {
        let base = send_text_hash("cc-1", "deploy to prod", true);
        assert_eq!(base, send_text_hash("cc-1", "deploy to prod", true));
        // Changing the text, the target or the submit flag is a *different*
        // mutation and must not be able to ride an already-trusted request id.
        assert_ne!(base, send_text_hash("cc-1", "rm -rf /", true));
        assert_ne!(base, send_text_hash("cc-2", "deploy to prod", true));
        assert_ne!(base, send_text_hash("cc-1", "deploy to prod", false));
    }

    #[test]
    fn interrupt_hash_binds_session_and_turn() {
        let base = interrupt_hash("cc-1", "turn-7");
        assert_eq!(base, interrupt_hash("cc-1", "turn-7"));
        // A different turn or session is a different mutation, never a replay.
        assert_ne!(base, interrupt_hash("cc-1", "turn-8"));
        assert_ne!(base, interrupt_hash("cc-2", "turn-7"));
        // Distinct domain tag: it can never equal a send_text hash.
        assert_ne!(base, send_text_hash("cc-1", "turn-7", true));
        // Separator-safe: a turn id containing a newline cannot impersonate a
        // different (session, turn).
        assert_ne!(
            interrupt_hash("cc-1", "a\nb"),
            interrupt_hash("cc-1\na", "b")
        );
    }

    #[test]
    fn send_text_hash_is_not_confusable_by_moving_the_separator() {
        // The fields are newline-joined, so a text that *contains* newlines
        // must not be able to impersonate a different (session, submit, text).
        assert_ne!(
            send_text_hash("cc-1", "true\nx", true),
            send_text_hash("cc-1\ntrue", "x", true)
        );
    }
}
