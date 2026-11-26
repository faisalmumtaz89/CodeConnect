import XCTest

@testable import CodeConnectKit

/// Decisions 214/215: the "work without asking" ask, per agent, as both apps show it (W0-6): the title, a line for each
/// thing the mode does, the note, the two answers, and the one sentence the iPhone draws from them.
final class ModeConfirmationTests: XCTestCase {

    func testClaudeBypassPermissionsWords() {
        let w = ModeConfirmation.words(for: .claude)
        XCTAssertEqual(w.title, "Let Claude work without asking?")
        XCTAssertEqual(w.items.map(\.words), ["Runs commands and edits files without asking"])
        XCTAssertEqual(w.note, "Until you change the mode back.")
        XCTAssertEqual(w.confirm, "Bypass Permissions")
        XCTAssertEqual(w.cancel, "Keep Asking")
        XCTAssertEqual(w.message, "Runs commands and edits files without asking. Until you change the mode back.")
    }

    func testCodexFullAccessWords() {
        let w = ModeConfirmation.words(for: .codex)
        XCTAssertEqual(w.title, "Give Codex full access?")
        XCTAssertEqual(
            w.items.map(\.words),
            ["Runs commands and edits files without asking", "Works outside this project’s folder", "Uses the network"])
        XCTAssertEqual(w.note, "Until you change the mode back.")
        XCTAssertEqual(w.confirm, "Give Full Access")
        XCTAssertEqual(w.cancel, "Keep Asking")
    }

    // Only what is true (97, decision 221): no safety claim, and bounded to until the mode is changed back.
    func testTheWordsClaimNoSafetyAndAreBoundedUntilTheModeChangesBack() {
        for agent in [AgentKind.claude, .codex] {
            let w = ModeConfirmation.words(for: agent)
            XCTAssertFalse(w.message.lowercased().contains("safe"), w.message)
            XCTAssertEqual(w.note, "Until you change the mode back.")
        }
    }
}
