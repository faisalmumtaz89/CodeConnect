import XCTest

@testable import CodeConnectKit

/// What Send to <agent> and a hunk's comment carry (slice 8): the excerpt in a fenced block, the person's words whole, and
/// never more than a message may be (`Wire.maxComposeBytes`), cut at a character with "… (cut)".
final class SendToTests: XCTestCase {
    private func check(_ json: String) throws -> RunGit.PullRequest.Check {
        try JSONDecoder().decode(RunGit.PullRequest.Check.self, from: Data(json.utf8))
    }

    private let file = UnifiedDiff.FileDiff(
        id: "a/b.swift", newPath: "a/b.swift", status: .modified, hunks: [], notes: [])

    private func hunk(_ texts: [String]) -> UnifiedDiff.Hunk {
        let lines = texts.enumerated().map { index, text in
            UnifiedDiff.Line(id: index, kind: .addition, text: text, newNumber: index + 1)
        }
        return UnifiedDiff.Hunk(
            id: "h", header: "@@ -0,0 +1,\(texts.count) @@", oldStart: 0, newStart: 1, lines: lines,
            segments: [.shown(id: 0, lines: lines)])
    }

    /// A failed check: its name, its failure line and its log, then the words when there are any.
    func testAFailedCheckSaysWhatFailedThenTheWords() throws {
        let failed = try check(
            #"{"name":"test","state":"fail","url":"https://ci/run/1","failure_line":"expected 2, got 3"}"#)
        XCTAssertEqual(
            ChangesWords.sendCheck("please fix", check: failed),
            "The check `test` failed: expected 2, got 3\nIts log: https://ci/run/1\n\nplease fix")
        XCTAssertEqual(ChangesWords.sendCheck("  ", check: failed), "The check `test` failed: expected 2, got 3\nIts log: https://ci/run/1")
        let bare = try check(#"{"name":"lint","state":"fail"}"#)
        XCTAssertEqual(ChangesWords.sendCheck(nil, check: bare), "The check `lint` failed")
    }

    /// A conflict: each stretch, this branch's lines then the base's, each in its own block, then the words.
    func testAConflictSendsBothSidesOfEachStretch() {
        let hunks = [ConflictHunk(ours: "FEAT\n", theirs: "MAIN\n")]
        XCTAssertEqual(
            ChangesWords.sendConflict("keep mine", path: "a.txt", base: "main", hunks: hunks),
            "`a.txt` conflicts with main:\n\nThis branch:\n```\nFEAT\n```\nmain:\n```\nMAIN\n```\n\nkeep mine")
    }

    /// Lines that hold a fence of their own are fenced with one longer than any run of backticks in them.
    func testAFenceIsLongerThanAnyInItsLines() {
        let sent = ChangesWords.comment("look", file: file, hunk: hunk(["```swift", "x"]))
        XCTAssertTrue(sent.contains("\n````diff\n"), sent)
        XCTAssertTrue(sent.hasSuffix("\n````"), sent)
    }

    /// Past the limit, the excerpt is cut at a character, its block closed and marked; the anchor and words go whole.
    func testALongExcerptIsCutToFitAtACharacter() {
        let long = hunk((0..<2000).map { "é line \($0) 🚀" })
        let sent = ChangesWords.comment("why?", file: file, hunk: long)
        XCTAssertLessThanOrEqual(sent.utf8.count, Wire.maxComposeBytes)
        XCTAssertTrue(sent.hasPrefix("In `a/b.swift` lines 1–2000: why?\n\n```diff\n"), String(sent.prefix(80)))
        XCTAssertTrue(sent.hasSuffix("\n```\n… (cut)"), String(sent.suffix(40)))
        XCTAssertNotNil(String(data: Data(sent.utf8), encoding: .utf8), "cut at a character, never inside one")

        let stretch = String(repeating: "x", count: 9000)
        let conflict = ChangesWords.sendConflict(
            "keep both", path: "a.txt", base: "main", hunks: [ConflictHunk(ours: stretch, theirs: "MAIN\n")])
        XCTAssertLessThanOrEqual(conflict.utf8.count, Wire.maxComposeBytes)
        XCTAssertTrue(conflict.hasSuffix("… (cut)\n\nkeep both"), String(conflict.suffix(40)))
        XCTAssertFalse(conflict.contains("MAIN"), "nothing after the cut")
    }
}
