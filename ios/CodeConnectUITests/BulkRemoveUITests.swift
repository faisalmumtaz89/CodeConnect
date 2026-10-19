import XCTest

/// Selecting several ended sessions and removing them with one question.
///
/// Runs under `-CC_FIXTURE ended`, whose eight exited runs are removable. The
/// sample fleet has no Mac, so every removal it is asked for comes back
/// unanswered — which is exactly what lets these tests watch a refused batch.
final class BulkRemoveUITests: XCTestCase {

    private let first = "session-01K1B3XQ8ZC0DE5FGH7JKMNP00"
    private let second = "session-01K1B3XQ8ZC0DE5FGH7JKMNP01"

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launchSelecting(_ extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "ended", "-CC_BIOMETRICS", "allow"] + extra
        app.launch()
        XCTAssertTrue(app.buttons["Show"].waitForExistence(timeout: 20))
        app.buttons["Show"].tap()
        // The ended rows start under the bottom bar; a tap there lands on the bar.
        let live = row(app, "session-fx-1").coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        live.press(forDuration: 0.05, thenDragTo: live.withOffset(CGVector(dx: 0, dy: -260)))
        let select = app.buttons["Select"]
        XCTAssertTrue(select.waitForExistence(timeout: 5), "the opened Ended band offers Select")
        select.tap()
        return app
    }

    private func row(_ app: XCUIApplication, _ identifier: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: identifier).firstMatch
    }

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
        app.buttons["Select All"].tap()
        XCTAssertTrue(app.buttons["Remove (8)"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["Deselect All"].exists)
        app.buttons["Deselect All"].tap()
        XCTAssertTrue(app.buttons["Remove (0)"].waitForExistence(timeout: 5))
    }

    func testOneQuestionCoversTheBatch() {
        let app = launchSelecting()
        row(app, first).tap()
        row(app, second).tap()

        app.buttons["Remove (2)"].tap()
        XCTAssertTrue(
            app.alerts["Remove 2 sessions?"].waitForExistence(timeout: 5), "one question for all")
        app.alerts.buttons["Cancel"].tap()
        XCTAssertTrue(row(app, first).isSelected, "Cancel leaves the selection as it was")
        XCTAssertFalse(
            app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS[c] 'not removed'"))
                .firstMatch.exists, "and removes nothing")

        app.buttons["Remove (2)"].tap()
        app.alerts.buttons["Remove"].tap()
        // No Mac answers the sample fleet: the batch stops, both stay selected,
        // and the screen says so.
        XCTAssertTrue(
            app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS[c] '2 not removed'"))
                .firstMatch.waitForExistence(timeout: 10))
        XCTAssertTrue(row(app, first).isSelected)
        XCTAssertTrue(row(app, second).isSelected)
    }

    /// A batch confirmed and then left for a chat answers while the fleet is
    /// out of view: back on the fleet, its note and the selection it kept are
    /// what the fleet shows.
    func testABatchAnsweredBehindAChatShowsOnReturn() {
        let app = launchSelecting(["-CC_FIXTURE_REMOVE_MS", "4000"])
        row(app, first).tap()
        row(app, second).tap()
        app.buttons["Remove (2)"].tap()
        // Before the tap: the hold may start while the tap is still returning.
        let confirmed = Date()
        app.alerts.buttons["Remove"].tap()
        XCTAssertTrue(app.buttons["Removing…"].waitForExistence(timeout: 5), "the batch is held")

        XCUIDevice.shared.system.open(URL(string: "codeconnect://session/fx-1")!)
        XCTAssertTrue(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 10),
            "a chat opens over the fleet while the batch is held")
        // The answer is held four seconds from the confirmation; a chat that
        // opened after that would have watched it land on the fleet instead.
        XCTAssertLessThan(
            Date().timeIntervalSince(confirmed), 4,
            "the chat is open before the answer is due, so it lands behind the chat")
        // The hold, and then some: the refusal arrives while the chat is open.
        RunLoop.current.run(until: Date().addingTimeInterval(6))

        app.navigationBars.buttons.firstMatch.tap()
        XCTAssertTrue(
            app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS[c] '2 not removed'"))
                .firstMatch.waitForExistence(timeout: 10),
            "the note says what the batch did not remove")
        XCTAssertTrue(row(app, first).isSelected)
        XCTAssertTrue(row(app, second).isSelected)
        XCTAssertTrue(app.buttons["Remove (2)"].isEnabled, "the batch is over")
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
