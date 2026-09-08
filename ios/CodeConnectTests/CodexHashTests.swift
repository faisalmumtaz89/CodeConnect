import XCTest

@testable import CodeConnect

/// **Tier 1.3 — the two mutation hashes, against pinned cross-language vectors.**
///
/// `mac/protocol/src/hash.rs` builds each preimage as
///
/// ```
/// material = "<domain tag>"
/// for field in [...]:
///     material += "\n" + decimal(field.len()) + ":" + field
/// digest   = lowercase_hex(SHA256(utf8(material)))
/// ```
///
/// and Rust's `str::len()` is a **UTF-8 byte count**. Swift's `String.count` is
/// grapheme clusters. That single word is the most likely silent bug in this
/// phase: a `.count` implementation passes every ASCII vector below and fails
/// only the two that carry an accent, a CJK character or a ZWJ sequence — which
/// is exactly why those two are here.
///
/// **Where the vectors came from.** `hash.rs` pins no hex literal of its own
/// (its tests assert inequality, not value), so the expected digests below were
/// produced by a third, independent implementation of the documented preimage —
/// a short Python script written from `hash.rs` — rather than by this Swift.
/// Pinning this file's own output would prove only that it agrees with itself.
/// T4.2 remains the only thing that proves the real daemon computes the same:
/// one live `interrupt` and one live `compose` that do not come back
/// `stale payload_hash`.
final class CodexHashTests: XCTestCase {

    /// A uid-shaped reference, per decision D4: the phone hashes over
    /// `session_uid`, never a display name.
    private let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"
    private let turn = "01a073f6-2004-7750-a697-a6c12004ca48"

    func testInterruptHashMatchesThePinnedVector() {
        XCTAssertEqual(
            CodexHash.interrupt(sessionRef: uid, turnID: turn),
            "7639dcf329ca2192ea738385619bcc7b91e13dfe965da1ba2a421cdb0b2ff4ef")
        XCTAssertEqual(
            CodexHash.interrupt(sessionRef: "cc-1", turnID: "turn-7"),
            "11b04062970d5ab513005bd3219ba23836b6ba48afd9e0a7a6e56c587f2c3e92")
    }

    func testComposeHashMatchesThePinnedVector() {
        XCTAssertEqual(
            CodexHash.compose(sessionRef: uid, text: "Run the tests"),
            "dbccce916f5274f3ea879582cd5cf353c6907a7eb25f54d71647f9e7a93b43ee")
        XCTAssertEqual(
            CodexHash.compose(sessionRef: "cc-1", text: "turn-7"),
            "749d625c2a2a2cd0ece8eae4380b937fcc796fd2f873fd531d6f8a8adeb1da94")
    }

    /// **The byte-versus-character test, and the reason this file exists.**
    ///
    /// `/work/ünïcodé/文件.txt` is 27 UTF-8 bytes and 20 Swift characters; the
    /// ZWJ family is 26 bytes and 9 Swift characters. An implementation using
    /// `String.count` produces a different preimage for both and the same one
    /// for every ASCII vector above.
    func testTheHashUsesByteLengthNotCharacterCount() {
        let path = "/work/ünïcodé/文件.txt"
        XCTAssertEqual(path.utf8.count, 27)
        XCTAssertNotEqual(path.count, path.utf8.count, "the discriminator must actually differ")
        XCTAssertEqual(
            CodexHash.compose(sessionRef: uid, text: path),
            "323ef09926a8c37ab887712e6d939010cbeab7956d745f1e084d051997513fa2")

        let emoji = "ship it \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"
        XCTAssertEqual(emoji.utf8.count, 26)
        XCTAssertNotEqual(emoji.count, emoji.utf8.count)
        XCTAssertEqual(
            CodexHash.compose(sessionRef: uid, text: emoji),
            "191329562852d346026ba291df98913c3b0f2a311a9786be223c93fdbcbc6c5b")
    }

    /// Two domain tags, so a stop can never be mistaken for a message.
    func testTheDomainTagsAreDistinct() {
        XCTAssertNotEqual(
            CodexHash.interrupt(sessionRef: uid, turnID: "x"),
            CodexHash.compose(sessionRef: uid, text: "x"))
    }

    /// The length prefix is what makes the fields unambiguous: without it
    /// `("cc-1\na", "b")` and `("cc-1", "a\nb")` would share a preimage.
    func testTheLengthPrefixSeparatesTheFields() {
        XCTAssertNotEqual(
            CodexHash.interrupt(sessionRef: "cc-1", turnID: "a\nb"),
            CodexHash.interrupt(sessionRef: "cc-1\na", turnID: "b"))
    }

    func testTheHashIsLowercaseHexSixtyFourChars() {
        for hex in [
            CodexHash.interrupt(sessionRef: uid, turnID: turn),
            CodexHash.compose(sessionRef: uid, text: ""),
        ] {
            XCTAssertEqual(hex.count, 64)
            XCTAssertTrue(
                hex.allSatisfy { $0.isHexDigit && !$0.isUppercase }, "lowercase hex only: \(hex)")
        }
    }

    // MARK: The compose ceiling

    /// `MAX_COMPOSE_BYTES` is 8192 **bytes**, enforced at the Mac on
    /// `text.len()`. The composer must count the same way and pre-refuse, or a
    /// message of 8192 accented characters (16384 bytes) leaves the phone and
    /// comes back refused.
    func testTheComposeCeilingIsBytes() {
        let ascii = String(repeating: "a", count: Wire.maxComposeBytes)
        XCTAssertEqual(ascii.utf8.count, 8192)
        XCTAssertNil(ComposeDraft(text: ascii).blockedReason, "8192 bytes is allowed")

        let overByOne = ascii + "a"
        XCTAssertEqual(
            ComposeDraft(text: overByOne).blockedReason,
            "This message is 8193 bytes. The ceiling is 8192.")

        // 8192 *characters* of é is 16384 bytes — the case a `String.count`
        // ceiling would wave through.
        let accented = String(repeating: "é", count: Wire.maxComposeBytes)
        XCTAssertEqual(accented.count, 8192)
        XCTAssertEqual(accented.utf8.count, 16384)
        XCTAssertEqual(
            ComposeDraft(text: accented).blockedReason,
            "This message is 16384 bytes. The ceiling is 8192.")
    }

    /// Empty is refused before it is sent, in the phone's own words — the
    /// daemon would refuse it too, but a round trip to learn that is a round
    /// trip nobody needed.
    ///
    /// **Only the empty string.** `state.rs` rejects `text.is_empty()` and
    /// nothing more; this used to trim first, so a message of newlines — bytes
    /// the Mac would have taken — was refused by the phone in the daemon's name.
    /// A stricter client policy may be worth having, but it would be the app's
    /// own and would have to say so.
    func testOnlyAnEmptyComposeIsRefusedBeforeItIsSent() {
        XCTAssertNotNil(ComposeDraft(text: "").blockedReason)
        XCTAssertNil(
            ComposeDraft(text: "   \n ").blockedReason,
            "whitespace is bytes; the daemon accepts them and so does the phone")
        XCTAssertNil(ComposeDraft(text: "go").blockedReason)
    }

    /// What the byte counter shows, and when. It appears only as the ceiling
    /// comes into view — a counter on every message is noise, and a counter that
    /// only appears once you are over is a scolding.
    func testTheByteCounterAppearsBeforeTheCeilingNotAfterIt() {
        XCTAssertNil(ComposeDraft(text: "hello").counterText)
        let near = String(repeating: "a", count: 7_500)
        XCTAssertEqual(ComposeDraft(text: near).counterText, "7,500 / 8,192 bytes")
        let over = String(repeating: "a", count: 8_300)
        XCTAssertEqual(ComposeDraft(text: over).counterText, "8,300 / 8,192 bytes")
    }
}
