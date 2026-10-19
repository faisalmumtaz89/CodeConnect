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

    private func launchFleet(_ extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "ended", "-CC_BIOMETRICS", "allow"] + extra
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

    /// The revealed button is a full-height rectangle: every corner red. A middle
    /// row, so no corner of it meets the card's own rounded outline.
    func testTheRevealedButtonHasSquareCorners() {
        let app = launchFleet()
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        let middle = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

        drag(app, from: middle, dx: -180, dy: 0)
        let content = row(app, middle).frame
        XCTAssertLessThan(content.minX, 16 - revealWidth + 2, "the row is open")

        // The button sits just past the row's content and spans its height.
        let button = CGRect(
            x: content.maxX, y: content.minY, width: revealWidth, height: content.height)
        let shot = app.screenshot().image
        let inset: CGFloat = 1.5
        let corners = [
            CGPoint(x: button.minX + inset, y: button.minY + inset),
            CGPoint(x: button.maxX - inset, y: button.minY + inset),
            CGPoint(x: button.minX + inset, y: button.maxY - inset),
            CGPoint(x: button.maxX - inset, y: button.maxY - inset),
        ]
        for corner in corners {
            let (red, green, blue) = pixel(shot, at: corner)
            XCTAssertTrue(
                red > 200 && green < 120 && blue < 120,
                "corner \(corner) is not the button's red: rgb(\(red), \(green), \(blue))")
        }
    }

    /// The revealed button asks before anything is removed, and Cancel leaves the
    /// row exactly as it was. Confirming hands the removal to the row; the sample
    /// fleet has no Mac to grant it, so the row stays, showing the refusal.
    func testRemoveAsksFirst() {
        let app = launchFleet()
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        let middle = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

        drag(app, from: middle, dx: -180, dy: 0)
        let open = row(app, middle).frame
        XCTAssertLessThan(open.minX, 16 - revealWidth + 2, "the row is open")
        // The button is hidden from accessibility (VoiceOver has the row's own
        // action), so it is reached where it is drawn.
        let button = app.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: open.maxX + revealWidth / 2, dy: open.midY))

        // What the button draws is the instrument: the sample fleet refuses at
        // once, so a removal that ran replaces "Remove" with the refusal.
        let face = CGRect(x: open.maxX, y: open.minY, width: revealWidth, height: open.height)
        let resting = pixels(app.screenshot().image, in: face)
        let redrawn = { (expected: Bool) in
            XCTNSPredicateExpectation(
                predicate: NSPredicate { _, _ in
                    (self.pixels(app.screenshot().image, in: face) != resting) == expected
                }, object: nil)
        }

        button.tap()
        let confirm = app.buttons["Remove"]
        XCTAssertTrue(confirm.waitForExistence(timeout: 5), "Remove must ask first")
        XCTAssertTrue(
            app.alerts["Remove app-6?"].exists, "the question names what is being removed")
        app.buttons["Cancel"].tap()
        XCTAssertFalse(confirm.waitForExistence(timeout: 2), "Cancel dismisses the question")
        XCTAssertEqual(
            row(app, middle).frame.minX, open.minX, accuracy: 2,
            "Cancel leaves the row exactly as it was")
        wait(for: [redrawn(false)], timeout: 5)

        button.tap()
        XCTAssertTrue(app.buttons["Remove"].waitForExistence(timeout: 5))
        app.buttons["Remove"].tap()
        XCTAssertFalse(app.alerts.firstMatch.waitForExistence(timeout: 2), "confirming answers it")
        wait(for: [redrawn(true)], timeout: 5)
        XCTAssertEqual(
            row(app, middle).frame.minX, open.minX, accuracy: 2,
            "a refused removal leaves the row open, showing why")

        // A nudge that leaves the row open keeps the reason on screen.
        drag(app, from: middle, dx: -40, dy: 0)
        XCTAssertEqual(row(app, middle).frame.minX, open.minX, accuracy: 2, "still open")
        wait(for: [redrawn(true)], timeout: 2)
    }

    // MARK: Across a link drop

    /// When the fixture drops the link, for the tests below: late enough for
    /// each one's setup to finish first, which each of them checks. Counted
    /// here from before the launch and in the app from its fixture loading,
    /// so the check is stricter than the truth and can only fail safe.
    private let linkDropMS = 30_000

    /// The link dropping takes the Mac's removal capability with it, which
    /// switches every row's swipe off; its return switches them back on, shut.
    /// Waited out through the one control that says so on screen: Select is
    /// offered only while the Mac can remove.
    private func waitForTheLinkToDropAndReturn(_ app: XCUIApplication, launchedAt: Date) {
        XCTAssertLessThan(
            Date().timeIntervalSince(launchedAt), Double(linkDropMS) / 1000,
            "the setup must finish before the link drops, or nothing below is tested")
        let select = app.buttons["Select"]
        XCTAssertTrue(select.exists, "Select is offered while the Mac can remove")
        XCTAssertTrue(select.waitForNonExistence(timeout: 30), "the link never dropped")
        XCTAssertTrue(select.waitForExistence(timeout: 15), "the link never came back")
    }

    /// A refusal belongs to the opening that showed it. After the link drops
    /// and returns the row comes back shut, and opening it again offers
    /// "Remove", not the old reason.
    func testARefusedRowReopensOfferingRemoveAfterTheLinkReturns() {
        let launched = Date()
        let app = launchFleet(["-CC_FIXTURE_LINK_DROP_MS", "\(linkDropMS)"])
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        let middle = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

        drag(app, from: middle, dx: -180, dy: 0)
        let open = row(app, middle).frame
        XCTAssertLessThan(open.minX, 16 - revealWidth + 2, "the row is open")
        let face = CGRect(x: open.maxX, y: open.minY, width: revealWidth, height: open.height)
        let resting = pixels(app.screenshot().image, in: face)
        let showsResting = { (expected: Bool) in
            let shows = XCTNSPredicateExpectation(
                predicate: NSPredicate { _, _ in
                    (self.pixels(app.screenshot().image, in: face) == resting) == expected
                }, object: nil)
            shows.expectationDescription =
                expected ? "the reopened row offers Remove, not the old refusal" : "the refusal shows"
            return shows
        }
        app.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: open.maxX + revealWidth / 2, dy: open.midY)).tap()
        XCTAssertTrue(app.buttons["Remove"].waitForExistence(timeout: 5), "Remove asks first")
        app.buttons["Remove"].tap()
        wait(for: [showsResting(false)], timeout: 5)

        waitForTheLinkToDropAndReturn(app, launchedAt: launched)
        XCTAssertEqual(row(app, middle).frame.minX, 16, accuracy: 2, "the row comes back shut")

        drag(app, from: middle, dx: -180, dy: 0)
        XCTAssertEqual(row(app, middle).frame.minX, open.minX, accuracy: 2, "the row opens again")
        wait(for: [showsResting(true)], timeout: 3)
    }

    /// A row left open when the link dropped comes back shut, and a tap on it
    /// opens its session, as a tap on any shut row does.
    func testARowOpenAcrossALinkDropOpensItsSessionOnATap() {
        let launched = Date()
        let app = launchFleet(["-CC_FIXTURE_LINK_DROP_MS", "\(linkDropMS)"])
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)

        drag(app, from: endedRow, dx: -180, dy: 0)
        XCTAssertLessThan(row(app, endedRow).frame.minX, 16 - revealWidth + 2, "the row is open")

        waitForTheLinkToDropAndReturn(app, launchedAt: launched)
        XCTAssertEqual(row(app, endedRow).frame.minX, 16, accuracy: 2, "the row comes back shut")

        row(app, endedRow).tap()
        XCTAssertTrue(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 10),
            "a tap on the shut row must open its session")
    }

    /// A removal confirmed in a question asked before the link dropped, and
    /// refused after it returned, still shows its reason on the row as it is
    /// now: the answer belongs to the request, not to the swipe that asked.
    func testARefusalAfterTheLinkReturnsShowsOnTheReopenedRow() {
        let launched = Date()
        let app = launchFleet([
            "-CC_FIXTURE_LINK_DROP_MS", "\(linkDropMS)", "-CC_FIXTURE_REMOVE_MS", "10000",
        ])
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        let middle = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

        drag(app, from: middle, dx: -180, dy: 0)
        let open = row(app, middle).frame
        XCTAssertLessThan(open.minX, 16 - revealWidth + 2, "the row is open")
        let face = CGRect(x: open.maxX, y: open.minY, width: revealWidth, height: open.height)
        let faceNow = { self.pixels(app.screenshot().image, in: face) }
        let resting = faceNow()
        app.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: open.maxX + revealWidth / 2, dy: open.midY)).tap()
        XCTAssertTrue(app.buttons["Remove"].waitForExistence(timeout: 5), "Remove asks first")

        waitForTheLinkToDropAndReturn(app, launchedAt: launched)
        app.buttons["Remove"].tap()
        drag(app, from: middle, dx: -180, dy: 0)
        XCTAssertEqual(row(app, middle).frame.minX, open.minX, accuracy: 2, "the row opens again")
        let removing = faceNow()
        XCTAssertNotEqual(removing, resting, "the reopened row reads Removing… while the answer is on its way")

        let refused = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in
                let now = faceNow()
                return now != removing && now != resting
            }, object: nil)
        refused.expectationDescription = "the refusal shows on the reopened row"
        wait(for: [refused], timeout: 20)
        XCTAssertEqual(
            row(app, middle).frame.minX, open.minX, accuracy: 2,
            "the row stays open showing why, so a closed row cannot pass for a refusal")
    }

    /// One pixel of a screenshot, at a point in screen coordinates.
    private func pixel(_ image: UIImage, at point: CGPoint) -> (UInt8, UInt8, UInt8) {
        let one = pixels(image, in: CGRect(origin: point, size: CGSize(width: 1, height: 1)))
        return (one[0], one[1], one[2])
    }

    /// The RGBA bytes of a screenshot region, given in screen coordinates.
    private func pixels(_ image: UIImage, in rect: CGRect) -> [UInt8] {
        let scale = image.scale
        let width = Int(rect.width * scale)
        let height = Int(rect.height * scale)
        var rgba = [UInt8](repeating: 0, count: width * height * 4)
        let context = CGContext(
            data: &rgba, width: width, height: height, bitsPerComponent: 8,
            bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        context.draw(
            image.cgImage!,
            in: CGRect(
                x: -rect.minX * scale, y: rect.maxY * scale - image.size.height * scale,
                width: image.size.width * scale, height: image.size.height * scale))
        return rgba
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

    /// An opened Ended band folds back from its own header, without a relaunch.
    func testTheEndedBandHidesAgain() {
        let app = launchFleet()
        revealEndedRows(app)

        let hide = app.buttons["Hide"]
        XCTAssertTrue(hide.waitForExistence(timeout: 5), "the opened band offers Hide")
        hide.tap()

        XCTAssertTrue(app.buttons["Show"].waitForExistence(timeout: 5), "back to the footer")
        XCTAssertFalse(row(app, endedRow).exists, "the ended rows are folded away")
        XCTAssertFalse(app.buttons["Hide"].exists)
    }

    /// A chat opened from an Ended row and closed again leaves the fleet as it
    /// was: the band still open, scrolled where it was.
    func testAChatKeepsTheEndedBandOpenAndTheScroll() {
        let app = launchFleet()
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        let before = row(app, endedRow).frame

        row(app, endedRow).tap()
        XCTAssertTrue(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 10),
            "the row opens its session")
        app.navigationBars.buttons.firstMatch.tap()

        XCTAssertTrue(app.buttons["Hide"].waitForExistence(timeout: 10), "the band is still open")
        XCTAssertEqual(
            row(app, endedRow).frame.minY, before.minY, accuracy: 2,
            "the fleet is scrolled where it was")
    }

    /// A header button is a bordered control, so its border sits on the card's
    /// edge below it, not on the text column inside the card.
    func testHeaderButtonsSitOnTheCardEdge() {
        let app = launchFleet()
        revealEndedRows(app)
        let hide = app.buttons["Hide"]
        XCTAssertTrue(hide.waitForExistence(timeout: 5))
        XCTAssertEqual(
            hide.frame.maxX, row(app, endedRow).frame.maxX, accuracy: 1,
            "Hide's right edge is the card's right edge")
    }

    /// A row left open when the band folds must not come back armed: Hide stands
    /// it down, like any other way of leaving the rows.
    func testHidingStandsAnOpenRowDown() {
        let app = launchFleet()
        revealEndedRows(app)
        liftRowsClearOfTheBar(app)
        drag(app, from: endedRow, dx: -180, dy: 0)
        XCTAssertLessThan(row(app, endedRow).frame.minX, 16 - revealWidth + 2, "the row is open")

        let hide = app.buttons["Hide"]
        XCTAssertTrue(hide.waitForExistence(timeout: 5))
        hide.tap()
        app.buttons["Show"].tap()

        XCTAssertTrue(row(app, endedRow).waitForExistence(timeout: 5))
        XCTAssertEqual(
            row(app, endedRow).frame.minX, 16, accuracy: 2, "the row comes back shut")
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
