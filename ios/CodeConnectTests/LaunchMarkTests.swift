import XCTest

@testable import CodeConnectKit

/// The launch's mark is a pure function of time: these are the moments both apps rely on.
final class LaunchMarkTests: XCTestCase {
    /// Before the surface appears there is only the ground; once the intro ends the mark is assembled, its bands on
    /// their own offsets.
    func testTheIntroStartsFromTheGroundAndEndsOnTheAssembledMark() {
        XCTAssertFalse(LaunchMark.frame(t: 0, exitAt: nil).drawn)
        XCTAssertTrue(LaunchMark.frame(t: LaunchMark.cutAt / 2, exitAt: nil).whole, "a whole disc before its cuts")
        let assembled = LaunchMark.frame(t: LaunchMark.introEnd, exitAt: nil)
        XCTAssertTrue(assembled.drawn)
        XCTAssertFalse(assembled.whole)
        XCTAssertEqual(assembled.scale, 1, accuracy: 0.001)
        for slide in assembled.along { XCTAssertEqual(slide, 0, accuracy: 0.5) }
    }

    /// The exit leaves only from a hold-loop boundary, never mid-breath, and never before the intro is done.
    func testTheExitStartsOnlyOnALoopBoundary() {
        XCTAssertEqual(LaunchMark.exitStart(settledAt: 0), LaunchMark.introEnd)
        for settled in [0.8, 1.9, 3.15, 3.16, 9.99, 15] {
            let start = LaunchMark.exitStart(settledAt: settled)
            XCTAssertGreaterThanOrEqual(start, settled)
            let loops = (start - LaunchMark.introEnd) / LaunchMark.holdPeriod
            XCTAssertEqual(loops, loops.rounded(), accuracy: 1e-9, "settled at \(settled)")
            for slide in LaunchMark.frame(t: start - 1e-6, exitAt: nil).along {
                XCTAssertEqual(slide, 0, accuracy: 0.01, "the hold is at rest where the exit begins")
            }
        }
    }

    /// The exit's last frame draws nothing, and the surface is finished from then on.
    func testTheExitEndsWithNothingDrawn() {
        let exit = LaunchMark.exitStart(settledAt: 2)
        XCTAssertTrue(LaunchMark.frame(t: exit + LaunchMark.exitLength * 0.5, exitAt: exit).drawn)
        XCTAssertFalse(LaunchMark.frame(t: exit + LaunchMark.exitLength, exitAt: exit).drawn)
        XCTAssertFalse(LaunchMark.finished(t: exit + LaunchMark.exitLength - 0.01, exitAt: exit))
        XCTAssertTrue(LaunchMark.finished(t: exit + LaunchMark.exitLength, exitAt: exit))
        XCTAssertFalse(LaunchMark.finished(t: 60, exitAt: nil), "while the fleet is catching up the surface stays")
    }
}
