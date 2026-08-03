import XCTest

/// Proves the removal gesture on the running app, because nothing else can.
///
/// **The defect this exists for was invisible in the source.** `CCSwipeToRemove`
/// was first built on a `DragGesture` with a horizontal-intent gate — ignore the
/// drag until it is clearly sideways, then move the row. Every line of it reads
/// as correct. It is not: a `DragGesture` has no direction, so it recognises at
/// `minimumDistance` whichever way the finger went, and the gate runs in
/// `onChanged`, *after* recognition. Declining there cannot hand the touch back,
/// and the fleet stopped scrolling over precisely the rows that offer removal.
/// Two independent reviews read that code and only a real drag settled it.
///
/// **The control is the point of this file.** Each scroll assertion is paired
/// with the identical drag on a *live* row, which is not wrapped in the
/// component at all. Without it, "the list did not scroll" cannot be told apart
/// from "the test cannot scroll a list", and the first version of these tests
/// failed for that second reason.
///
/// Runs under `-CC_FIXTURE ended`, the one fixture with exited runs: the swipe
/// is offered on `lifecycle == .exited` alone, so under the other fixtures the
/// component is not installed and a green test would mean nothing.
final class SwipeToRemoveUITests: XCTestCase {

    /// The first uid `Fixtures.Variant.ended` reports.
    private let endedRow = "session-01K1B3XQ8ZC0DE5FGH7JKMNP00"
    /// A blocked, running agent. Never removable.
    private let liveRow = "session-fx-1"

    /// `CCSwipeToRemove.revealWidth` at a non-accessibility type size.
    private let revealWidth: CGFloat = 112

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launchFleet() -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "ended", "-CC_BIOMETRICS", "allow"]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        return app
    }

    private func row(_ app: XCUIApplication, _ identifier: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: identifier).firstMatch
    }

    /// The ended band is collapsed to one footer row by default, which is the
    /// whole point of it. Nothing below can happen until it is opened.
    private func revealEndedRows(_ app: XCUIApplication) {
        let show = app.buttons["Show"]
        XCTAssertTrue(show.waitForExistence(timeout: 20), "no collapsed ended band to open")
        show.tap()
        XCTAssertTrue(
            row(app, endedRow).waitForExistence(timeout: 10), "the ended band did not open")
    }

    /// A press-and-drag from the middle of a row.
    ///
    /// **Not `element.swipeUp()`.** That swipe is scaled to the element, and a
    /// 76pt row yields a stroke too short to scroll anything — it reported "did
    /// not scroll" for every row including ones with no gesture on them. This
    /// travels a fixed distance regardless of what it starts on, which is what
    /// makes the two rows comparable.
    private func drag(_ app: XCUIApplication, from identifier: String, dx: CGFloat, dy: CGFloat) {
        let start = row(app, identifier).coordinate(
            withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        start.press(forDuration: 0.05, thenDragTo: start.withOffset(CGVector(dx: dx, dy: dy)))
    }

    // MARK: Scrolling must survive the gesture

    func testTheListStillScrollsWhenTheDragStartsOnARemovableRow() {
        let app = launchFleet()
        revealEndedRows(app)
        let before = row(app, endedRow).frame.minY

        drag(app, from: endedRow, dx: 0, dy: -250)

        XCTAssertLessThan(
            row(app, endedRow).frame.minY, before - 100,
            """
            The fleet did not scroll under a vertical drag that began on a \
            removable row, and the control below proves the drag itself works. \
            The row's own gesture took the touch and never gave it back — see the \
            note at the top of `CCSwipeToRemove`.
            """)
    }

    /// The control. A live row is not wrapped in the component, so this is the
    /// same drag with nothing in its way. If it fails, the instrument is broken
    /// and the test above proves nothing either way.
    func testTheControlRowScrollsTheSameWay() {
        let app = launchFleet()
        revealEndedRows(app)
        let before = row(app, liveRow).frame.minY

        drag(app, from: liveRow, dx: 0, dy: -250)

        XCTAssertLessThan(
            row(app, liveRow).frame.minY, before - 100,
            "a row with no gesture on it did not scroll either; the instrument is wrong")
    }

    // MARK: The gesture itself

    func testASidewaysDragSlidesTheRowOpen() {
        let app = launchFleet()
        revealEndedRows(app)
        let before = row(app, endedRow).frame.minX

        drag(app, from: endedRow, dx: -180, dy: 0)

        // Comes to rest exactly open, never part-way: `RevealBehavior` snaps a
        // released swipe to one of two offsets.
        XCTAssertEqual(
            row(app, endedRow).frame.minX, before - revealWidth, accuracy: 2,
            "a sideways drag must slide the row aside by exactly one button")
    }

    /// Scrolls the ended rows clear of the bottom accessory bar. A drag that
    /// starts under the bar lands on the bar — measured: the first ended row's
    /// own lower edge starts life beneath it — and a swipe that never touched
    /// the row proves nothing about the row.
    private func liftRowsClearOfTheBar(_ app: XCUIApplication) {
        drag(app, from: liveRow, dx: 0, dy: -260)
    }

    /// `List`'s `.swipeActions` closes an open row when the list scrolls; this
    /// hand-built swipe must match it. Measured, not assumed: the row's own
    /// frame is the instrument for both axes.
    func testScrollingTheFleetClosesAnOpenRow() {
        let app = launchFleet()
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)

        drag(app, from: endedRow, dx: -180, dy: 0)
        XCTAssertLessThan(
            row(app, endedRow).frame.minX, 16 - revealWidth + 2, "the row must be open first")

        // From a neighbouring wrapped row, not the live one: the lift above
        // scrolled the live row off-screen, and a drag from a stale element
        // moves nothing. Vertical drags from wrapped rows are exactly what the
        // scroll-guard test at the top of this file proves work.
        drag(app, from: "session-01K1B3XQ8ZC0DE5FGH7JKMNP01", dx: 0, dy: -120)

        XCTAssertEqual(
            row(app, endedRow).frame.minX, 16, accuracy: 2,
            "scrolling the fleet must stand the open row down")
    }

    /// And opening a second row closes the first — one armed destructive
    /// control at a time, exactly as `.swipeActions` behaves.
    func testOpeningASecondRowClosesTheFirst() {
        let app = launchFleet()
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        let second = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

        drag(app, from: endedRow, dx: -180, dy: 0)
        XCTAssertLessThan(row(app, endedRow).frame.minX, 16 - revealWidth + 2)

        drag(app, from: second, dx: -180, dy: 0)

        XCTAssertLessThan(
            row(app, second).frame.minX, 16 - revealWidth + 2, "the second row is open")
        XCTAssertEqual(
            row(app, endedRow).frame.minX, 16, accuracy: 2,
            "the first must have stood down when the second opened")
    }



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

        let started = Date()
        show.tap()
        XCTAssertTrue(row(app, last).waitForExistence(timeout: 5))
        let elapsed = Date().timeIntervalSince(started)
        print("MEASURE ended8 expansion wall-clock: \(Int(elapsed * 1000))ms")
    }

    // MARK: What is deliberately not tested here
    //
    // **"A live row does not slide open" has no honest UI test.** The drag that
    // proves it would have to start and end inside that row, and a finger that
    // never leaves a button's bounds is a tap however far it travelled — so the
    // row activates and opens the session, which is correct behaviour that looks
    // identical to the failure it is meant to rule out. Measured, not assumed:
    // the attempt navigated away every time.
    //
    // The claim is carried where it can be proven. `FleetView` passes
    // `isEnabled: lifecycle == .exited && daemonProfile.removesSessions`, and
    // `CCSwipeToRemove` returns `content()` unwrapped when that is false, so a
    // live row has no component around it to slide. `SessionRemovalTests` covers
    // the half that matters most — that the model refuses to ask the daemon at
    // all — in `testALiveRunIsNeverEvenAskedAbout`.
}
