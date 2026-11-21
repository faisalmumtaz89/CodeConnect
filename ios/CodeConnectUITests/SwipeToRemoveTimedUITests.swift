import XCTest

/// `SwipeToRemoveUITests`'s tests that rest on time: a fixture clock set at launch, an elapsed-time budget or a long press.
/// They run on this Mac before every release and are left off the CI runner, which is slower than they allow.
final class SwipeToRemoveTimedUITests: SwipeToRemoveUITestsBase {
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
