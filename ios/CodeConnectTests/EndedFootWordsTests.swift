import XCTest

@testable import CodeConnectKit

/// An ended run's foot says only that it has ended: the conversation above it is plainly there to read.
final class EndedFootWordsTests: XCTestCase {
    func testTheFootSaysOnlyThatTheRunHasEnded() {
        XCTAssertEqual(SessionWords.runEnded, "This session has ended")
    }
}
