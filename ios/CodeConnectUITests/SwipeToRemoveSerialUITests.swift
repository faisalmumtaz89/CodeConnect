import XCTest

/// `SwipeToRemoveUITests`'s tests that measure time: the band's expansion against a ceiling, and its calibration.
/// They have a class of their own, the serial rule's (ios/README.md).
final class SwipeToRemoveSerialUITests: SwipeToRemoveUITestsBase {
    /// The worst real fleet on record: 45 ended runs, every one materialized
    /// the moment the band expands (its VStack is deliberately eager). A smoke
    /// ceiling, not a benchmark — XCUITest wall-clock includes quiescence waits
    /// and the 180ms expansion animation, and Apple's own guidance is that
    /// responsiveness is proven with on-device profiling, not a simulator. What
    /// this catches is the failure worth catching in CI: expansion degrading
    /// from "an animation" to "a stall".
    func testExpandingFortyFiveEndedRowsIsNotAStall() {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "ended45", "-CC_BIOMETRICS", "allow"]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))

        let show = app.buttons["Show"]
        XCTAssertTrue(show.waitForExistence(timeout: 20))
        let last = "session-01K1B3XQ8ZC0DE5FGH7JKMNP44"

        show.scrollClearOfTheBar(in: app)
        let started = Date()
        show.tap()
        XCTAssertTrue(
            row(app, last).waitForExistence(timeout: 5),
            "45 ended rows must all exist once the band expands; the band is eager")
        let elapsed = Date().timeIntervalSince(started)
        print("MEASURE ended45 expansion wall-clock: \(Int(elapsed * 1000))ms")
        XCTAssertLessThan(elapsed, 3.0, "expansion degraded from an animation to a stall")
    }

    /// The control for the measurement above: identical instrument, 8 rows.
    /// The difference between the two isolates the marginal cost of 37 extra
    /// rows from the harness's own overhead (existence queries, quiescence,
    /// the fixed 180ms animation).
    func testExpandingEightRowsCalibratesTheInstrument() {
        let app = launchFleet()
        let show = app.buttons["Show"]
        XCTAssertTrue(show.waitForExistence(timeout: 20))
        let last = "session-01K1B3XQ8ZC0DE5FGH7JKMNP07"

        show.scrollClearOfTheBar(in: app)
        let started = Date()
        show.tap()
        XCTAssertTrue(row(app, last).waitForExistence(timeout: 5))
        let elapsed = Date().timeIntervalSince(started)
        print("MEASURE ended8 expansion wall-clock: \(Int(elapsed * 1000))ms")
    }
}
