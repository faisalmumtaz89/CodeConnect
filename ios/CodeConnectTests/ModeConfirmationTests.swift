import XCTest

@testable import CodeConnectKit

/// Decisions 214/215: the four strings of the "work without asking" confirmation, per agent. The strings are
/// the agreed draft (macosapp's WORDS.md); they are final when the owner answers. The Codex message's
/// folder-and-network clause awaits X6's fixture before it is frozen.
final class ModeConfirmationTests: XCTestCase {

    func testClaudeBypassPermissionsWords() {
        let w = ModeConfirmation.words(for: .claude)
        XCTAssertEqual(w.title, "Let Claude work without asking?")
        XCTAssertEqual(
            w.message,
            "Claude will run commands and edit files in this run without asking you first, until you change "
                + "the mode back.")
        XCTAssertEqual(w.confirm, "Bypass Permissions")
        XCTAssertEqual(w.cancel, "Keep Asking")
    }

    func testCodexFullAccessWords() {
        let w = ModeConfirmation.words(for: .codex)
        XCTAssertEqual(w.title, "Give Codex full access?")
        XCTAssertEqual(
            w.message,
            "Codex will run commands and edit files in this run without asking you first, outside this "
                + "project’s folder and with network access, until you change the mode back.")
        XCTAssertEqual(w.confirm, "Give Full Access")
        XCTAssertEqual(w.cancel, "Keep Asking")
    }

    // Only what is true (97, decision 221): no safety claim, bounded to this run and until the mode is changed
    // back. The restart sentence was dropped for the shortest form; the exact-string tests pin its absence.
    func testTheWordsClaimNoSafetyAndAreBoundedToThisRun() {
        for agent in [AgentKind.claude, .codex] {
            let w = ModeConfirmation.words(for: agent)
            XCTAssertFalse(w.message.lowercased().contains("safe"), w.message)
            XCTAssertTrue(w.message.contains("in this run"), w.message)
            XCTAssertTrue(w.message.contains("until you change the mode back"), w.message)
        }
    }
}
