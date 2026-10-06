import XCTest

@testable import CodeConnect

/// What the session screen's header and composer say about the run.
final class SessionHeaderTests: XCTestCase {

    /// The composer names the agent whenever the app knows it — Claude too,
    /// which used to be "this agent" on a screen that says CLAUDE CODE above it.
    func testTheComposerNamesTheAgentItKnows() {
        XCTAssertEqual(ComposerTemplates.placeholder(for: .claude), "Ask Claude to do anything")
        XCTAssertEqual(ComposerTemplates.placeholder(for: .codex), "Ask Codex to do anything")
        XCTAssertEqual(ComposerTemplates.placeholder(for: nil), "Say something to this agent")
        XCTAssertEqual(
            ComposerTemplates.placeholder(for: .unsupported("gemini")), "Say something to this agent")
    }

    /// The diff control's count: a clean tree is a counted zero, not the same
    /// bare `±` as a diff nobody has asked for.
    func testACleanTreeIsACountedZero() {
        func loaded(_ unified: String, note: String? = nil, truncated: Bool = false) -> DiffState {
            .loaded(
                SessionDiff(
                    sessionID: "cc-1", unified: unified, truncated: truncated,
                    capturedAt: "2026-10-05T10:00:00Z", note: note),
                parsed: UnifiedDiff.parse(unified), fetchedAt: Date())
        }
        XCTAssertEqual(loaded("").changedFileCount, 0)
        XCTAssertNil(loaded("", note: "not a git repository").changedFileCount)
        XCTAssertNil(loaded("", truncated: true).changedFileCount)
        // `ccd`'s own listing (mac/ccd/src/git.rs) when only untracked files changed:
        // the tree is not clean, and this app counts no files from it.
        XCTAssertNil(
            loaded("\n# 1 untracked file(s), not shown as a diff:\n#   notes.txt\n").changedFileCount,
            "an untracked-only tree is not a counted zero")
        XCTAssertEqual(
            loaded("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n").changedFileCount, 1)
        XCTAssertNil(DiffState.idle.changedFileCount)
        XCTAssertNil(DiffState.loading.changedFileCount)
    }
}
