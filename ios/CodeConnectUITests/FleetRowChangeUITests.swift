import XCTest

/// What `FleetRowChangeUITests` and `FleetRowChangeTimedUITests` share: how they launch the app and find their way to the state each tests.
class FleetRowChangeUITestsBase: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func launch(_ arguments: [String], signals: FixtureSignals? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            arguments + [
                "-CC_BIOMETRICS", "allow",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ]
        signals?.arm(app)
        app.launch()
        return app
    }

    func row(_ app: XCUIApplication, _ key: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: "session-\(key)").firstMatch
    }

    func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return condition()
    }
}

/// A change to a row reaches that row while the fleet is on screen.
///
/// A fleet row is redrawn only when what it draws has changed (`FleetRowCell`),
/// so a fact the row draws but its comparison misses would freeze the row until
/// something else about it changed. Every case here changes a row **after** it
/// first drew and reads what the row then says.
final class FleetRowChangeUITests: FleetRowChangeUITestsBase {
    /// A refused Stop says so in Stop's own place on its row, the daemon's
    /// sentence a tap away and Stop offered beside it: greyed for the
    /// refusal's cooldown, saying why, and then back.
    func testARefusedStopIsOfferedAgainBesideItsSentence() {
        let app = launch(["-CC_CODEX", "stop-refused-late"])
        let outcome = app.buttons["stop-outcome-cx-1"]
        let again = app.buttons["stop-again-cx-1"]
        let reason = app.descendants(matching: .any).matching(
            NSPredicate(format: "label CONTAINS %@", "Try again in a moment")
        ).firstMatch
        XCTAssertTrue(outcome.waitForExistence(timeout: 30), "the launch's refusal is in Stop's place")
        XCTAssertTrue(
            outcome.label.contains("no connection to this run"), "and speaks the daemon's sentence: \(outcome.label)")

        outcome.tap()
        XCTAssertTrue(again.waitForExistence(timeout: 10), "Stop is offered beside the sentence")
        XCTAssertTrue(waitUntil(timeout: 15) { again.isEnabled }, "once the launch's refusal cools down")
        again.tap()

        XCTAssertTrue(outcome.waitForExistence(timeout: 10), "the new refusal is in Stop's place")
        outcome.tap()
        XCTAssertTrue(again.waitForExistence(timeout: 10))
        XCTAssertFalse(again.isEnabled, "the refusal greys Stop")
        XCTAssertTrue(reason.exists, "and says why")
        XCTAssertTrue(waitUntil(timeout: 15) { again.isEnabled }, "and Stop comes back")
    }
}
