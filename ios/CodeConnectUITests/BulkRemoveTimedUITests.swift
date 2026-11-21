import XCTest

/// `BulkRemoveUITests`'s tests that rest on time: a fixture clock set at launch, an elapsed-time budget or a long press.
/// They run on this Mac before every release and are left off the CI runner, which is slower than they allow.
final class BulkRemoveTimedUITests: BulkRemoveUITestsBase {
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
        app.buttons["remove.confirm"].tap()
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
}
