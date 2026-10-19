import XCTest

/// The fleet scrolls at the accessibility type sizes.
///
/// Over a `LazyVStack` it did not: at AX1 and above a band of rows is taller
/// than the screen, and one of the first few drags left the lazy stack placing
/// and re-placing its bands for good — the main thread at 100% for minutes,
/// one drag taking 61s before the runner gave up on the app. Each drag here
/// takes about two seconds of the runner's own waiting, so a drag that takes
/// ten is the app not coming back.
///
/// Runs under `-CC_FIXTURE ended`, whose Blocked band at these sizes is
/// several screens tall, and under `-CC_RENDER_PROBE`, so each pass proves it
/// laid out at the size it asked for: a launch can silently fall back to `L`,
/// where nothing here can fail.
final class FleetLargeTypeScrollUITests: XCTestCase {

    /// A blocked agent's row, at the top of the fleet.
    private let firstRow = "session-fx-1"
    /// The runner's own wait for idle, plus a drag's travel, is about 2s.
    private let dragCeiling: TimeInterval = 10

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testTheFleetScrollsAtAX1() { scrollsBothWays(at: "UICTContentSizeCategoryAccessibilityM") }

    func testTheFleetScrollsAtAX5() { scrollsBothWays(at: "UICTContentSizeCategoryAccessibilityXXXL") }

    private func scrollsBothWays(at size: String) {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "ended", "-CC_BIOMETRICS", "allow",
            "-CC_RENDER_PROBE", "YES",
            "-UIPreferredContentSizeCategoryName", size,
        ]
        app.launch()
        let probe = app.otherElements["cc-render-probe"]
        XCTAssertTrue(probe.waitForExistence(timeout: 30), "no render probe")
        XCTAssertEqual(probe.label, size, "the app did not lay out at the size asked for")

        let row = app.descendants(matching: .any).matching(identifier: firstRow).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 20), "no fleet")
        let top = row.frame.minY

        let middle = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        for (step, travel) in (Array(repeating: -300, count: 6) + Array(repeating: 300, count: 6))
            .enumerated()
        {
            let start = Date()
            middle.press(
                forDuration: 0.1, thenDragTo: middle.withOffset(CGVector(dx: 0, dy: travel)),
                withVelocity: .default, thenHoldForDuration: 0)
            let took = Date().timeIntervalSince(start)
            XCTAssertLessThan(took, dragCeiling, "drag \(step + 1) took \(took)s: the app stopped")
            if step == 0 {
                XCTAssertLessThan(
                    settledTop(of: row), top - 100, "the first drag did not scroll the fleet")
            }
            if step == 5 {
                _ = settledTop(of: row)
                XCTAssertTrue(app.buttons["Show"].isHittable, "six drags did not reach the fleet's end")
            }
        }
        XCTAssertEqual(settledTop(of: row), top, accuracy: 1, "six drags back did not return to the top")
    }

    /// The row's top once two reads a quarter-second apart agree to half a
    /// point. These drags are released without a hold — the gesture that froze
    /// the lazy stack — so the list flings, and a frame read mid-deceleration is
    /// still travelling. A list still moving after five seconds fails the test.
    private func settledTop(
        of row: XCUIElement, file: StaticString = #filePath, line: UInt = #line
    ) -> CGFloat {
        var last = row.frame.minY
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline {
            Thread.sleep(forTimeInterval: 0.25)
            let now = row.frame.minY
            if abs(now - last) < 0.5 { return now }
            last = now
        }
        XCTFail("the fleet was still moving 5s after a drag", file: file, line: line)
        return last
    }
}
