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

    /// The exit starts once the intro is done and the fleet has caught up, never later, from wherever the hold had
    /// the bands.
    func testTheExitStartsWhenTheIntroIsDoneAndTheFleetHasCaughtUp() {
        XCTAssertEqual(LaunchMark.exitStart(settledAt: 0.2), LaunchMark.introEnd, accuracy: 1e-9)
        XCTAssertEqual(LaunchMark.exitStart(settledAt: 1.0), 1.0, accuracy: 1e-9)
        XCTAssertEqual(LaunchMark.exitStart(settledAt: 3.0), 3.0, accuracy: 1e-9)
        XCTAssertEqual(LaunchMark.exitStart(settledAt: 3.2), 3.2, accuracy: 1e-9)
        for settled in [0.2, 1.0, 1.9, 3.0, 3.2, 9.99, 15] {
            let start = LaunchMark.exitStart(settledAt: settled)
            // The exit's first frame is the hold's frame at that moment: no jump (to a twentieth of a unit of the
            // 1024 box, under a hundredth of a point; the intro's spring is that close to rest when it ends).
            let before = LaunchMark.frame(t: start - 1e-6, exitAt: nil).along
            let after = LaunchMark.frame(t: start, exitAt: start).along
            for (held, leaving) in zip(before, after) {
                XCTAssertEqual(held, leaving, accuracy: 0.05, "settled at \(settled)")
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
