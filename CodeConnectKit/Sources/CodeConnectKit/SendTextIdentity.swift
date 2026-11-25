import CryptoKit
import Foundation

/// The identity of one `send_text` mutation, hashed exactly as the daemon
/// hashes it (`protocol/src/hash.rs::send_text_hash`).
///
/// The daemon recomputes this from the request's own fields and refuses a
/// mismatch, so the recipe is a wire contract: version tag, then each field
/// as `\n{utf8-byte-length}:{field}`. Length prefixes rather than separators,
/// because `"{session}\n{submit}\n{text}"` is ambiguous the moment text
/// contains a newline — and an ambiguity in a hash that authorises typing is
/// a way to make one mutation answer for another. The middle field is the
/// word `submit` or `stage`, never a boolean rendering, for the same
/// cross-language reason.
public enum SendTextIdentity {
    /// `attachments`, the upload ids in order (minor 50), are further fields of the same material, so a send with none
    /// hashes exactly as before (`hash.rs::send_text_hash_with`).
    public static func payloadHash(session: String, text: String, submit: Bool, attachments: [String] = []) -> String {
        var material = "codeconnect.send_text.v1"
        for field in [session, submit ? "submit" : "stage", text] + attachments {
            material += "\n\(field.utf8.count):\(field)"
        }
        let digest = SHA256.hash(data: Data(material.utf8))
        return digest.map { String(format: "%02x", $0) }.joined()
    }
}
