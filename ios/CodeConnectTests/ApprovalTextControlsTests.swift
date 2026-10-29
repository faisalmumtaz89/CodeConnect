import CryptoKit
import SwiftUI
import XCTest

@testable import CodeConnect

/// An approval's agent text is drawn with every character that makes a screen
/// show something other than what runs replaced by U+FFFD.
///
/// A right-to-left override reverses what follows it, so `git push origin
/// <U+202E>niam ecrof--` was drawn `git push origin --force main` under
/// "Verified", and a NUL is drawn as nothing at all.
/// Claude Code's own renderer draws U+061C and the bidi embeddings, overrides
/// and isolates as U+FFFD (2.1.308), and the phone does the same at the four
/// places an approval's text comes from: `ToolSummary.principalArgument`,
/// `ToolSummary.intent`, `ApprovalCard.primaryText`'s exact-text fallback and
/// `ApprovalCard.shownToolInput`. The wire, the hash and the answer keep the
/// bytes as they are.
@MainActor
final class ApprovalTextControlsTests: XCTestCase {

    /// Every character drawn as U+FFFD, in code point order.
    private static let controls: [Character] =
        ([0x00, 0x061C] + Array(0x202A...0x202E) + Array(0x2066...0x2069))
        .map { Character(Unicode.Scalar($0)!) }

    private static func named(_ character: Character) -> String {
        character.unicodeScalars.map { $0.escaped(asASCII: true) }.joined()
    }

    // MARK: Cards as the daemon sends them

    private func heldRequest() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-22-wire", withExtension: "json"))
        let root = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
        return try XCTUnwrap(root["held_request"]?.objectValue)
    }

    /// minor 22's held card asking about `input` instead, with the text and the
    /// hash the daemon computes for it: `"{tool}\n{input}"` and its SHA-256
    /// (`protocol/src/hash.rs`), unless the daemon's own are given.
    private func approval(
        tool: String = "Bash", input: JSONValue,
        displayText: String? = nil, payloadHash: String? = nil
    ) throws -> ApprovalItem {
        let text = displayText ?? "\(tool)\n\(input.canonicalJSONString)"
        let hash =
            payloadHash
            ?? SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined()
        var request = try heldRequest()
        var payload = try XCTUnwrap(request["payload"]?.objectValue)
        var card = try XCTUnwrap(payload["card"]?.objectValue)
        card["tool_name"] = .string(tool)
        card["tool_input"] = input
        card["display_text"] = .string(text)
        card["payload_hash"] = .string(hash)
        payload["card"] = .object(card)
        request["payload"] = .object(payload)
        let event = try XCTUnwrap(JSONValue.object(request).decoded(Event.self))
        var built: ApprovalItem?
        for item in TimelineBuilder.build([event]) {
            if case .approval(let approval) = item.content { built = approval }
        }
        return try XCTUnwrap(built, "no approval row was built")
    }

    // MARK: Drawn and spoken

    /// Each control in the spoofed command is drawn and spoken as U+FFFD on the
    /// chat's row, on the decision card's command block, and in what the fleet
    /// row is built from. `SpoofedCommandUITests` reads the fleet row, the chat
    /// row and the card off the screen.
    func testEachControlInACommandIsDrawnAndSpokenAsAReplacementCharacter() throws {
        for control in Self.controls {
            let input = JSONValue.object(["command": .string("git push origin \(control)niam ecrof--")])
            let shown = "git push origin \u{FFFD}niam ecrof--"
            let item = try approval(input: input)
            let name = Self.named(control)
            XCTAssertTrue(item.card.verification.renderMatchesDisplayText, name)
            XCTAssertEqual(
                item.card.primaryText(verification: item.card.verification, agent: .claude), shown,
                "the decision card's command, \(name)")
            XCTAssertEqual(
                ToolSummary.principalArgument(tool: "Bash", input: input), shown,
                "the chat row and the fleet row draw this, \(name)")
            XCTAssertTrue(
                ApprovalRow(approval: item, risk: .medium, onOpen: {}).accessibilityLabel
                    .hasPrefix("Pending approval, risk MEDIUM, Bash, \(shown), waiting "),
                "the chat row's label, \(name)")
        }
    }

    /// A NUL in a Bash description is drawn, as U+FFFD, and so is the text
    /// after it: the description is as wide as the same words with U+FFFD in
    /// the NUL's place. Drawn as it arrives the NUL took no width (216.7pt
    /// against 232pt, iOS 26.3) and the words after it were drawn.
    func testANULInADescriptionDrawsTheTextAfterIt() throws {
        func width(_ text: String) -> CGFloat {
            UIHostingController(rootView: Text(text).ccType(CC.type.callout).fixedSize())
                .sizeThatFits(in: CGSize(width: 10_000, height: 10_000)).width
        }
        let input = JSONValue.object([
            "command": .string("git push"),
            "description": .string("Force-push\u{0} the rebased branch"),
        ])
        let intent = try XCTUnwrap(ToolSummary.intent(tool: "Bash", input: input))
        XCTAssertEqual(width(intent), width("Force-push\u{FFFD} the rebased branch"), intent.debugDescription)
        XCTAssertGreaterThan(width(intent), width("Force-push") * 2, intent.debugDescription)
    }

    // MARK: The bytes stay the daemon's

    /// The command and description as the daemon emits them for this card
    /// (`protocol::hash::approval_payload_text` and `approval_payload_hash`,
    /// run on this input): the override travels raw, the NUL as `\u0000`.
    private static let daemonDisplayText =
        "Bash\n{\"command\":\"git push origin \u{202E}niam ecrof--\","
        + "\"description\":\"Force-push\\u0000 the rebased branch\"}"
    private static let daemonPayloadHash =
        "523b5d1c870311f36d1a48ca6618846f9dc80d4cdc97c5888afe95c9e38a134f"

    /// The phone verifies the card on the bytes that run — its hash check and
    /// its render check both hold — and answers it with the daemon's request id
    /// and hash. Only what is drawn changes.
    func testTheHashTheRenderAndTheAnswerKeepTheDaemonsBytes() throws {
        let input = JSONValue.object([
            "command": .string("git push origin \u{202E}niam ecrof--"),
            "description": .string("Force-push\u{0} the rebased branch"),
        ])
        let card = try approval(
            input: input, displayText: Self.daemonDisplayText,
            payloadHash: Self.daemonPayloadHash
        ).card
        XCTAssertEqual(card.displayText, Self.daemonDisplayText)
        XCTAssertEqual(card.toolInput, input)
        XCTAssertEqual(
            card.verification,
            ApprovalCard.Verification(hashMatchesDisplayText: true, renderMatchesDisplayText: true))
        let answer = try JSONDecoder().decode(
            JSONValue.self,
            from: JSONEncoder().encode(
                ClientMessage.answer(
                    requestID: card.requestID, payloadHash: card.payloadHash, decision: .allow,
                    session: WireRecording.uid)))
        XCTAssertEqual(answer["request_id"]?.stringValue, "toolu_allowed")
        XCTAssertEqual(answer["payload_hash"]?.stringValue, Self.daemonPayloadHash)
    }

    // MARK: Everything else is drawn as it arrives

    /// Text without those characters is drawn exactly as it arrives — line
    /// breaks other than `\n` (`\r`, form feed), tabs, the marks that only
    /// steer neutral characters, letters of every direction, emoji sequences
    /// and a U+FFFD the agent wrote itself.
    func testTextWithoutTheControlsIsDrawnAsItArrives() throws {
        let plain = [
            "git push --force origin main",
            "printf 'a\rb\u{0C}c\u{0B}d\te'\r\necho done\u{2028}\u{85}",
            "echo \u{200E}left\u{200F}right \u{1B}[1mbold",
            "echo שלום مرحبا नमस्ते 👩‍👩‍👧 e\u{301}",
            "already \u{FFFD} here",
        ]
        for text in plain {
            let input = JSONValue.object([
                "command": .string(text), "description": .string(text),
            ])
            XCTAssertEqual(ToolSummary.principalArgument(tool: "Bash", input: input), text)
            XCTAssertEqual(ToolSummary.intent(tool: "Bash", input: input), text)
            let card = try approval(input: input).card
            XCTAssertEqual(card.shownToolInput, input.prettyJSONString)
        }
    }

    // MARK: The census

    /// **Every place an approval's agent text is drawn from.** A view draws an
    /// approval's words through one of these four and nothing else:
    ///
    ///   * `ToolSummary.principalArgument` — the chat's approval row (drawn
    ///     and spoken), the fleet row's activity line and its spoken sentence,
    ///     the fleet's line for a blocked run, and the
    ///     decision card's command block (through `primaryText`);
    ///   * `ToolSummary.intent` — the description under the card's command;
    ///   * `ApprovalCard.primaryText` — the card's command block, which falls
    ///     back to `display_text` itself when the render check fails;
    ///   * `ApprovalCard.shownToolInput` — the card's "Full tool input".
    ///
    /// Each one draws every control as U+FFFD, for every tool family whose
    /// argument `principalArgument` reads.
    func testEverySourceOfAnApprovalsTextDrawsTheControlsAsReplacementCharacters() throws {
        for control in Self.controls {
            let name = Self.named(control)
            let words = "a\(control)b"
            let shown = "a\u{FFFD}b"
            let families: [(tool: String, input: JSONValue, expected: String)] = [
                ("Bash", .object(["command": .string(words)]), shown),
                ("BashOutput", .object(["command": .string(words)]), shown),
                ("Read", .object(["file_path": .string(words)]), shown),
                ("Write", .object(["file_path": .string(words)]), shown),
                ("Edit", .object(["file_path": .string(words)]), shown),
                ("NotebookEdit", .object(["notebook_path": .string(words)]), shown),
                ("Glob", .object(["pattern": .string(words)]), shown),
                (
                    "Grep", .object(["pattern": .string(words), "path": .string(words)]),
                    "\(shown)  in \(shown)"
                ),
                ("WebFetch", .object(["url": .string(words)]), shown),
                ("WebSearch", .object(["query": .string(words)]), shown),
                ("Task", .object(["description": .string(words)]), shown),
                ("Agent", .object(["prompt": .string(words)]), shown),
                (
                    "file change", .object(["changes": .array([.object(["path": .string(words)])])]),
                    shown
                ),
                (
                    "AskUserQuestion",
                    .object(["questions": .array([.object(["question": .string(words)])])]), shown
                ),
                ("mcp__server__tool", .object(["path": .string(words)]), shown),
            ]
            for family in families {
                XCTAssertEqual(
                    ToolSummary.principalArgument(tool: family.tool, input: family.input),
                    family.expected, "principalArgument, \(family.tool), \(name)")
            }

            let described = JSONValue.object([
                "command": .string("ls"), "description": .string(words),
            ])
            XCTAssertEqual(
                ToolSummary.intent(tool: "Bash", input: described), shown, "intent, \(name)")

            // A card whose `display_text` is not the phone's rendering of its
            // input: the command block shows `display_text` itself.
            let unrendered = try approval(
                input: .object(["command": .string("ls")]), displayText: "Bash\n\(words)")
            XCTAssertFalse(unrendered.card.verification.renderMatchesDisplayText)
            XCTAssertEqual(
                unrendered.card.primaryText(
                    verification: unrendered.card.verification, agent: .claude),
                "Bash\n\(shown)", "primaryText's fallback, \(name)")

            // JSON already writes a NUL as `\u0000`, which draws.
            let full = try approval(input: described).card.shownToolInput
            let inJSON = control == "\u{0}" ? "a\\u0000b" : shown
            XCTAssertFalse(full.contains(control), "shownToolInput, \(name)")
            XCTAssertTrue(
                full.contains("\"description\": \"\(inJSON)\""), "shownToolInput, \(name): \(full)")
        }
    }

    /// The card vouches for the bytes the daemon hashed, never for the
    /// characters on screen, which draw some of them as U+FFFD.
    func testTheVerificationLineClaimsTheHashNotTheScreen() {
        for rendered in [true, false] {
            XCTAssertEqual(
                ApprovalCard.Verification(hashMatchesDisplayText: true, renderMatchesDisplayText: rendered)
                    .summary,
                "Verified against the daemon's hash.", "render matches: \(rendered)")
        }
    }
}
