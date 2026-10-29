import XCTest

/// A fleet row opens its session, whatever the row's status.
///
/// A Done row is the case that matters: its turn may have been watched at the
/// Mac, or changed nothing, and opening its diff over the session lands the
/// reader on "No uncommitted changes" instead of the conversation.
final class FleetTapUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testTappingADoneRowOpensTheSessionNotTheDiff() {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "deck", "-CC_BIOMETRICS", "allow"]
        app.launch()

        let row = app.descendants(matching: .any).matching(identifier: "session-fx-4").firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 30), "the fixture fleet has fx-4")
        XCTAssertTrue(
            row.label.contains("Done"), "fx-4 must read Done, or this proves nothing: \(row.label)")
        // Below the waiting projects' cards, under the Deck bar: scrolled clear
        // of it, as a reader would, so the tap lands on the row.
        let bar = app.buttons["deck-bar"]
        var swipes = 0
        while bar.exists, row.frame.maxY > bar.frame.minY, swipes < 6 {
            app.swipeUp(velocity: .slow)
            swipes += 1
        }
        row.tap()

        XCTAssertTrue(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 20),
            "the tap opens the session")
        let diff = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'Diff ·'"))
            .firstMatch
        XCTAssertFalse(diff.waitForExistence(timeout: 3), "the tap must not open the diff")
    }
}
