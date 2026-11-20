import CodeConnectKit
import XCTest

/// A folded stretch of unchanged lines reads, is spoken and is opened in the Kit's words, the same in both apps.
final class DiffFoldWordsTests: XCTestCase {
    func testAFoldedStretchReadsTheSameInBothApps() {
        XCTAssertEqual(ChangesWords.foldedLines(6), "⋯ 6 unchanged lines")
        XCTAssertEqual(ChangesWords.foldedLines(1), "⋯ 1 unchanged line")
        XCTAssertEqual(ChangesWords.foldedLinesSpoken(6), "6 unchanged lines hidden")
        XCTAssertEqual(ChangesWords.foldedLinesSpoken(1), "1 unchanged line hidden")
        XCTAssertEqual(ChangesWords.foldedLinesHint, "Shows them")
    }
}
