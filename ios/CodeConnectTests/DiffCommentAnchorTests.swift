import XCTest

@testable import CodeConnectKit

/// A comment on a hunk reaches the agent with the file and lines it is about, worded the same from both apps.
final class DiffCommentAnchorTests: XCTestCase {
    private func hunk(_ lines: [UnifiedDiff.Line]) -> UnifiedDiff.Hunk {
        UnifiedDiff.Hunk(
            id: "h", header: "@@ -12,3 +12,4 @@ func send()", oldStart: 12, newStart: 12, lines: lines,
            segments: [.shown(id: 0, lines: lines)])
    }

    private let file = UnifiedDiff.FileDiff(
        id: "a/b.swift", newPath: "a/b.swift", status: .modified, hunks: [], notes: [])

    func testAHunkOfSeveralLinesNamesTheirRange() {
        let lines = [
            UnifiedDiff.Line(id: 0, kind: .context, text: "let a = 1", oldNumber: 12, newNumber: 12),
            UnifiedDiff.Line(id: 1, kind: .deletion, text: "let b = 2", oldNumber: 13),
            UnifiedDiff.Line(id: 2, kind: .addition, text: "let b = 3", newNumber: 13),
            UnifiedDiff.Line(id: 3, kind: .addition, text: "let c = 4", newNumber: 14),
        ]
        XCTAssertEqual(
            ChangesWords.comment("fix", file: file, hunk: hunk(lines)),
            "In `a/b.swift` lines 12–14: fix\n\n```diff\n@@ -12,3 +12,4 @@ func send()\n let a = 1\n-let b = 2\n+let b = 3\n+let c = 4\n```")
    }

    func testAHunkOfOneLineNamesIt() {
        let lines = [UnifiedDiff.Line(id: 0, kind: .addition, text: "x", newNumber: 7)]
        XCTAssertEqual(ChangesWords.commentAnchor(file: file, hunk: hunk(lines)), "In `a/b.swift` line 7")
    }

    func testAHunkWithNoNumbersNamesOnlyTheFile() {
        let lines = [UnifiedDiff.Line(id: 0, kind: .note, text: "\\ No newline at end of file")]
        XCTAssertTrue(ChangesWords.comment("why?", file: file, hunk: hunk(lines)).hasPrefix("In `a/b.swift`: why?\n\n"))
    }

    func testTheCommentFieldShowsItsPlaceAsPathAndLines() {
        let several = [
            UnifiedDiff.Line(id: 0, kind: .context, text: "a", oldNumber: 12, newNumber: 12),
            UnifiedDiff.Line(id: 1, kind: .addition, text: "b", newNumber: 13),
        ]
        XCTAssertEqual(ChangesWords.commentPlace(file: file, hunk: hunk(several)), "a/b.swift · lines 12–13")
        let one = [UnifiedDiff.Line(id: 0, kind: .addition, text: "x", newNumber: 7)]
        XCTAssertEqual(ChangesWords.commentPlace(file: file, hunk: hunk(one)), "a/b.swift · line 7")
        let none = [UnifiedDiff.Line(id: 0, kind: .note, text: "\\ No newline at end of file")]
        XCTAssertEqual(ChangesWords.commentPlace(file: file, hunk: hunk(none)), "a/b.swift")
        XCTAssertTrue(
            ChangesWords.comment("fix", file: file, hunk: hunk(several)).hasPrefix("In `a/b.swift` lines 12–13: fix\n\n"),
            "what reaches the agent keeps its backticked anchor")
    }

    func testTheCommentPromptNamesTheAgent() {
        XCTAssertEqual(ChangesWords.commentPrompt("Codex"), "Comment to Codex about these lines")
    }

    func testCopyHunkCopiesTheHeaderAndEveryLineUnderItsMark() {
        let lines = [
            UnifiedDiff.Line(id: 0, kind: .context, text: "a", oldNumber: 12, newNumber: 12),
            UnifiedDiff.Line(id: 1, kind: .deletion, text: "b", oldNumber: 13),
            UnifiedDiff.Line(id: 2, kind: .addition, text: "c", newNumber: 13),
            UnifiedDiff.Line(id: 3, kind: .note, text: "\\ No newline at end of file"),
        ]
        XCTAssertEqual(hunk(lines).text, "@@ -12,3 +12,4 @@ func send()\n a\n-b\n+c\n\\ No newline at end of file")
    }
}
