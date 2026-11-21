import XCTest

@testable import CodeConnectKit

/// Where both apps draw the missed-events notice in a drawn timeline (`GapNotice.index(in:)`): after the last row
/// dated at or before the moment the hole was seen, or after the last row when every row is later.
final class GapPlacementTests: XCTestCase {
    private let start = Date(timeIntervalSince1970: 1_800_000_000)

    private func rows(at seconds: [Double]) -> [TimelineItem] {
        seconds.enumerated().map { index, second in
            TimelineItem(
                id: "row-\(index)", seq: UInt64(index + 1), date: start.addingTimeInterval(second),
                content: .agentMessage("Row \(index)"))
        }
    }

    private func notice(at second: Double) -> GapNotice {
        GapNotice(cause: .sequenceJump(missing: 12), at: start.addingTimeInterval(second))
    }

    func testTheNoticeFollowsTheLastRowBeforeIt() {
        XCTAssertEqual(notice(at: 25).index(in: rows(at: [10, 20, 30])), 1)
    }

    func testARowDatedAtTheNoticeComesBeforeIt() {
        XCTAssertEqual(notice(at: 20).index(in: rows(at: [10, 20, 30])), 1)
    }

    func testANoticeAfterEveryRowFollowsTheLast() {
        XCTAssertEqual(notice(at: 40).index(in: rows(at: [10, 20, 30])), 2)
    }

    func testANoticeBeforeEveryRowFollowsTheLast() {
        XCTAssertEqual(notice(at: 5).index(in: rows(at: [10, 20, 30])), 2)
    }

    func testNoRowsIsTheFirstPlace() {
        XCTAssertEqual(notice(at: 5).index(in: []), 0)
    }

    func testTheSentenceForTwelveMissedEvents() {
        XCTAssertEqual(notice(at: 0).message, "Resynced, 12 events not shown.")
    }
}
