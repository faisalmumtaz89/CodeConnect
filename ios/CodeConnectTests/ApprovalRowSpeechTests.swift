import SwiftUI
import XCTest

@testable import CodeConnect

/// What VoiceOver says on the chat's approval row: the label of the one
/// combined element the row is, read straight off the row, and once from the
/// row drawn on a screen.
///
/// VoiceOver stops on the row once and reads that label — never the tool name
/// and command drawn inside it. A label built from the tool name alone said
/// `Bash` on a row that draws `git push --force origin main`, while the fleet
/// row and the Deck spoke the command.
///
/// The cards are the daemon's own bytes (`minor-22-wire.json`,
/// `minor-19-wire.json`), run through the real `TimelineBuilder`, so the
/// resolved rows carry the outcome the wire decodes to.
@MainActor
final class ApprovalRowSpeechTests: XCTestCase {

    private func spokenRow(_ approval: ApprovalItem, risk: RiskClass = .medium) -> String {
        ApprovalRow(approval: approval, risk: risk, onOpen: {}).accessibilityLabel
    }

    // MARK: On the screen

    private typealias SetAccessibilityEnabled = @convention(c) (Int32) -> Void

    /// Switches the app's accessibility runtime, as an assistive technology
    /// does: without it SwiftUI builds no accessibility elements to read.
    private func setAccessibility(_ enabled: Bool) throws {
        let library = try XCTUnwrap(dlopen("/usr/lib/libAccessibility.dylib", RTLD_NOW))
        let symbol = try XCTUnwrap(dlsym(library, "_AXSApplicationAccessibilitySetEnabled"))
        unsafeBitCast(symbol, to: SetAccessibilityEnabled.self)(enabled ? 1 : 0)
    }

    /// Every element VoiceOver would stop on in `object`: an element is read
    /// and not entered, a hidden one is skipped, anything else is walked.
    private func stops(in object: NSObject) -> [String] {
        if object.accessibilityElementsHidden { return [] }
        if object.isAccessibilityElement { return [object.accessibilityLabel ?? ""] }
        let count = object.accessibilityElementCount()
        let children: [NSObject]
        if let elements = object.accessibilityElements as? [NSObject], !elements.isEmpty {
            children = elements
        } else if count > 0, count != NSNotFound {
            children = (0..<count).compactMap { object.accessibilityElement(at: $0) as? NSObject }
        } else if let view = object as? UIView {
            children = view.subviews.filter { !$0.isHidden }
        } else {
            children = []
        }
        return children.flatMap { stops(in: $0) }
    }

    /// The label is the one the drawn row carries: rendered on an iPhone 17
    /// Pro's screen, the row is one stop that speaks the command, waiting and
    /// answered. The tests below read the label's wording directly.
    func testTheDrawnRowSpeaksItsCommand() throws {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let root = try wire("minor-22-wire")
        let pending = try approval([try event(root["held_request"])])
        let answered = try approval([
            try event(root["held_request"]), try event(root["answered_allow"]),
        ])
        for (row, spoken) in [
            (pending, "Pending approval, risk MEDIUM, Bash, make zz-target, waiting "),
            (answered, "Approval. Sent to Claude from a phone. Bash, make zz-target."),
        ] {
            let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 402, height: 874))
            window.rootViewController = UIHostingController(
                rootView: ApprovalRow(approval: row, risk: .medium, onOpen: {}))
            window.isHidden = false
            window.rootViewController?.view.layoutIfNeeded()
            RunLoop.main.run(until: Date().addingTimeInterval(1))
            let found = stops(in: window)
            window.isHidden = true
            XCTAssertEqual(found.count, 1, "the row is one stop: \(found)")
            XCTAssertTrue(found.first?.hasPrefix(spoken) == true, "\(found)")
        }
    }

    // MARK: The daemon's cards

    private func wire(_ name: String) throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"),
            "\(name).json is not in the test bundle (checked in at ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func event(_ value: JSONValue?) throws -> Event {
        try XCTUnwrap(value?.decoded(Event.self))
    }

    /// minor 22's held card, with the call it asks about replaced.
    private func claudeRequest(tool: String, input: JSONValue) throws -> Event {
        var request = try XCTUnwrap(try wire("minor-22-wire")["held_request"]?.objectValue)
        var payload = try XCTUnwrap(request["payload"]?.objectValue)
        var card = try XCTUnwrap(payload["card"]?.objectValue)
        card["tool_name"] = .string(tool)
        card["tool_input"] = input
        payload["card"] = .object(card)
        request["payload"] = .object(payload)
        return try event(.object(request))
    }

    private func approval(_ events: [Event]) throws -> ApprovalItem {
        var built: ApprovalItem?
        for item in TimelineBuilder.build(events) {
            if case .approval(let approval) = item.content { built = approval }
        }
        return try XCTUnwrap(built, "no approval row was built")
    }

    // MARK: Pending

    func testAPendingCommandIsSpokenAfterItsTool() throws {
        let pending = try approval([try event(try wire("minor-22-wire")["held_request"])])
        XCTAssertTrue(pending.isPending)
        let spoken = spokenRow(pending)
        XCTAssertTrue(
            spoken.hasPrefix("Pending approval, risk MEDIUM, Bash, make zz-target, waiting "), spoken)
    }

    /// An MCP tool's argument is found the way the row finds the one it draws:
    /// the call `mcpApproval` puts on the chat.
    func testAPendingMCPCallIsSpokenWithItsArgument() throws {
        let pending = try approval([
            try claudeRequest(tool: Fixtures.mcpCard.tool, input: Fixtures.mcpCard.input)
        ])
        let spoken = spokenRow(pending)
        XCTAssertTrue(
            spoken.hasPrefix(
                "Pending approval, risk MEDIUM, mcp__filesystem__list_directory_with_sizes, "
                    + "/Users/dev/app, waiting "),
            spoken)
    }

    /// The command's first line is spoken in full and nothing after it is.
    /// The drawn preview may cut that line short — one head-truncated line,
    /// or a few at accessibility sizes — and speech is not cut with it: the
    /// policy `CCMonoBlock` keeps for its own inline text, which it speaks in
    /// full even where the line is not drawn in full.
    func testALongCommandIsSpokenAsTheRowDrawsIt() throws {
        let first =
            "git push --force-with-lease origin release/2026-10-22 && ./scripts/deploy.sh "
            + "--env production --region eu-west-1 --skip-smoke-tests --verbose"
        let pending = try approval([
            try claudeRequest(tool: "Bash", input: .object(["command": .string(first + "\necho done")]))
        ])
        let spoken = spokenRow(pending)
        XCTAssertTrue(spoken.contains("Bash, \(first), waiting "), spoken)
        XCTAssertFalse(spoken.contains("echo done"), spoken)
    }

    /// Windows line ends and a lone carriage return end the first line too.
    /// Swift reads `\r\n` as one `Character`, so a split on `"\n"` never saw
    /// it, and the row spoke a line it did not draw.
    func testACommandWithOtherLineEndsIsSpokenToItsFirstLine() throws {
        for end in ["\r\n", "\r"] {
            let command = "git status\(end)git push --force origin main"
            let pending = try approval([
                try claudeRequest(tool: "Bash", input: .object(["command": .string(command)]))
            ])
            let spoken = spokenRow(pending)
            XCTAssertTrue(
                spoken.hasPrefix("Pending approval, risk MEDIUM, Bash, git status, waiting "),
                "\(end.unicodeScalars.map { $0.escaped(asASCII: true) }.joined()): \(spoken)")
        }
    }

    /// A vertical tab is not a line break on screen — the row draws the whole
    /// command through it — so it is spoken whole too, the part after it
    /// included.
    func testACommandWithAVerticalTabIsSpokenWhole() throws {
        let command = "printf 'a\u{0B}b' | rm -rf ~/work"
        let pending = try approval([
            try claudeRequest(tool: "Bash", input: .object(["command": .string(command)]))
        ])
        let spoken = spokenRow(pending)
        XCTAssertTrue(
            spoken.hasPrefix("Pending approval, risk MEDIUM, Bash, \(command), waiting "),
            spoken.debugDescription)
    }

    /// A first line with nothing on it is no argument: the call is spoken as
    /// one with none, not as a tool name followed by an empty clause.
    func testACommandWithABlankFirstLineIsSpokenAsTheToolAlone() throws {
        for command in ["   ", "\ngit push --force origin main", "\t\ngit push --force origin main"] {
            let pending = try approval([
                try claudeRequest(tool: "Bash", input: .object(["command": .string(command)]))
            ])
            let spoken = spokenRow(pending)
            XCTAssertTrue(
                spoken.hasPrefix("Pending approval, risk MEDIUM, Bash, waiting "),
                "\(command.debugDescription): \(spoken)")
        }
    }

    // MARK: Where a first line ends

    /// Every preview that shows one line of something — this row, the Deck,
    /// the fleet's subtitle, a tool row — cuts at the first line break the
    /// screen draws, and nowhere else.
    func testTheFirstLineEndsAtAnyLineBreak() {
        for end in ["\n", "\r\n", "\r", "\u{0C}", "\u{2028}", "\u{2029}", "\u{85}"] {
            XCTAssertEqual(
                "git status\(end)git push --force origin main".firstLine, "git status",
                end.unicodeScalars.map { $0.escaped(asASCII: true) }.joined())
        }
        let verticalTab = "printf 'a\u{0B}b' | rm -rf ~/work"
        XCTAssertEqual(verticalTab.firstLine, verticalTab)
        XCTAssertEqual("  ls -la  ".firstLine, "ls -la")
        XCTAssertEqual("\r\nls".firstLine, "")
    }

    /// The line the previews cut is the line the screen draws: a character
    /// ends the first line exactly when `Text` breaks a line at it. A cut where
    /// the screen draws on hides the rest of the command — `printf 'a<VT>b' |
    /// rm -rf ~/work` drawn and spoken as `printf 'a` — and a screen break the
    /// cut misses speaks a line nobody can see.
    func testTheFirstLineEndsWhereTheScreenBreaksALine() {
        func height(_ text: String) -> CGFloat {
            UIHostingController(rootView: Text(text).ccType(CC.type.monoSmall).fixedSize())
                .sizeThatFits(in: CGSize(width: 10_000, height: 10_000)).height
        }
        let oneLine = height("ab")
        for end in ["\n", "\r\n", "\r", "\u{0B}", "\u{0C}", "\u{85}", "\u{2028}", "\u{2029}"] {
            let text = "a\(end)b"
            let drawn = height(text)
            XCTAssertEqual(
                text.firstLine == "a", drawn > oneLine * 1.5,
                "\(end.unicodeScalars.map { $0.escaped(asASCII: true) }.joined()): firstLine "
                    + "\(text.firstLine.debugDescription), drawn \(drawn)pt against \(oneLine)pt")
        }
    }

    // MARK: Resolved

    func testAnAnsweredCommandIsStillSpoken() throws {
        let root = try wire("minor-22-wire")
        let answered = try approval([
            try event(root["held_request"]), try event(root["answered_allow"]),
        ])
        let outcome = try XCTUnwrap(answered.outcome, "the wire's answer is on the row")
        XCTAssertTrue(outcome.wasSentToClaude)
        XCTAssertEqual(
            spokenRow(answered), "Approval. Sent to Claude from a phone. Bash, make zz-target.")
    }

    func testACodexCommandItEndedIsStillSpoken() throws {
        let root = try wire("minor-19-wire")
        let ended = try approval([
            try event(root["approval_request"]), try event(root["approval_resolved"]),
        ])
        XCTAssertNil(ended.outcome)
        XCTAssertEqual(
            try XCTUnwrap(ended.codexResolution, "the wire's ending is on the row"),
            .answered(by: .phone, decision: .optionId("accept")))
        XCTAssertEqual(
            spokenRow(ended),
            "Answered from a phone. Codex has the answer. "
                + "command, /bin/zsh -lc 'touch /tmp/marker.txt'.")
    }
}
