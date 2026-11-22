import XCTest

/// What `BulkRemoveUITests` and `BulkRemoveTimedUITests` share: how they launch the app and find their way to the state each tests.
class BulkRemoveUITestsBase: XCTestCase {
    let first = "session-01K1B3XQ8ZC0DE5FGH7JKMNP00"

    let second = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func launchSelecting(_ extra: [String] = [], signals: FixtureSignals? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "ended", "-CC_BIOMETRICS", "allow"] + extra
        signals?.arm(app)
        app.launch()
        XCTAssertTrue(app.buttons["Show"].waitForExistence(timeout: 20))
        app.buttons["Show"].scrollClearOfTheBar(in: app)
        app.buttons["Show"].tap()
        // The ended rows start under the bottom bar; a tap there lands on the bar.
        let live = row(app, "session-fx-1").coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        live.press(forDuration: 0.05, thenDragTo: live.withOffset(CGVector(dx: 0, dy: -260)))
        let select = app.buttons["Select"]
        XCTAssertTrue(select.waitForExistence(timeout: 5), "the opened Ended band offers Select")
        select.scrollClearOfTheBar(in: app)
        select.tap()
        return app
    }

    func row(_ app: XCUIApplication, _ identifier: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: identifier).firstMatch
    }
}

/// Selecting several ended sessions and removing them with one question.
///
/// Runs under `-CC_FIXTURE ended`, whose eight exited runs are removable. The
/// sample fleet has no Mac, so every removal it is asked for comes back
/// unanswered — which is exactly what lets these tests watch a refused batch.
final class BulkRemoveUITests: BulkRemoveUITestsBase {
    func testRowsAreSelectedNotOpened() {
        let app = launchSelecting()
        XCTAssertTrue(app.buttons["Done"].exists, "selection is left with Done")
        XCTAssertFalse(app.buttons["Hide"].exists, "nothing else on the header while selecting")
        XCTAssertFalse(app.buttons["Review"].exists, "the selection bar replaces the deck bar")
        let remove = app.buttons["Remove (0)"]
        XCTAssertTrue(remove.waitForExistence(timeout: 5))
        XCTAssertFalse(remove.isEnabled, "nothing to remove yet")

        row(app, first).tap()
        row(app, second).tap()
        XCTAssertFalse(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 2),
            "a tap selects; it does not open the session")
        XCTAssertTrue(row(app, first).isSelected)
        XCTAssertTrue(row(app, second).isSelected)
        XCTAssertTrue(app.buttons["Remove (2)"].isEnabled)

        row(app, second).tap()
        XCTAssertFalse(row(app, second).isSelected, "a second tap clears it")
        XCTAssertTrue(app.buttons["Remove (1)"].exists)
    }

    func testSelectAllCoversEveryEndedRow() {
        let app = launchSelecting()
        app.buttons["Select all"].tap()
        XCTAssertTrue(app.buttons["Remove (8)"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["Deselect all"].exists)
        app.buttons["Deselect all"].tap()
        XCTAssertTrue(app.buttons["Remove (0)"].waitForExistence(timeout: 5))
    }

    func testOneQuestionCoversTheBatch() {
        let app = launchSelecting()
        row(app, first).tap()
        row(app, second).tap()

        app.buttons["Remove (2)"].tap()
        XCTAssertTrue(
            app.staticTexts["Remove 2 runs?"].waitForExistence(timeout: 5), "one question for all")
        app.buttons["remove.cancel"].tap()
        XCTAssertTrue(row(app, first).isSelected, "Cancel leaves the selection as it was")
        XCTAssertFalse(
            app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS[c] 'not removed'"))
                .firstMatch.exists, "and removes nothing")

        app.buttons["Remove (2)"].tap()
        app.buttons["remove.confirm"].tap()
        // No Mac answers the sample fleet: the batch stops, both stay selected,
        // and the screen says so.
        XCTAssertTrue(
            app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS[c] '2 not removed'"))
                .firstMatch.waitForExistence(timeout: 10))
        XCTAssertTrue(row(app, first).isSelected)
        XCTAssertTrue(row(app, second).isSelected)
    }

    func testDoneLeavesSelection() {
        let app = launchSelecting()
        row(app, first).tap()
        app.buttons["Done"].tap()
        XCTAssertTrue(app.buttons["Review"].waitForExistence(timeout: 5), "the deck bar is back")
        XCTAssertTrue(app.buttons["Select"].exists)
        XCTAssertFalse(row(app, first).isSelected)
    }
}
