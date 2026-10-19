import XCTest

/// A change to a row reaches that row while the fleet is on screen.
///
/// A fleet row is redrawn only when what it draws has changed (`FleetRowCell`),
/// so a fact the row draws but its comparison misses would freeze the row until
/// something else about it changed. Every case here changes a row **after** it
/// first drew and reads what the row then says.
final class FleetRowChangeUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launch(_ arguments: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            arguments + [
                "-CC_BIOMETRICS", "allow",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ]
        app.launch()
        return app
    }

    private func row(_ app: XCUIApplication, _ key: String) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: "session-\(key)").firstMatch
    }

    /// The cards, `fx-4`'s finished turn and `fx-5`'s running one arrive after
    /// the rows have drawn: the blocked rows gain their card and their clock
    /// moves to it, and two rows change band. Before that, a blocked row's own
    /// clock ticks.
    func testWhatArrivesLaterReachesItsRows() {
        let app = launch(["-CC_FIXTURE", "stacked", "-CC_FIXTURE_LATE_APPROVALS_MS", "12000"])
        let high = row(app, "fx-1")
        XCTAssertTrue(high.waitForExistence(timeout: 25), "the fleet")
        XCTAssertFalse(high.label.contains("git push"), "no card yet: \(high.label)")
        XCTAssertFalse(row(app, "fx-4").label.contains("Done"), row(app, "fx-4").label)
        XCTAssertFalse(row(app, "fx-5").label.contains("Running"), row(app, "fx-5").label)

        let ticked = high.label
        XCTAssertTrue(
            waitUntil(timeout: 5) { high.label != ticked && !high.label.contains("git push") },
            "the wait clock ticks on its own row: \(ticked)")

        XCTAssertTrue(
            waitUntil(timeout: 25) { high.label.contains("git push --force origin main") },
            "the card reaches its row: \(high.label)")
        XCTAssertTrue(high.label.contains("risk HIGH"), high.label)
        XCTAssertTrue(
            high.label.contains("waiting 1 minute"), "the clock counts from the card: \(high.label)")
        XCTAssertTrue(
            waitUntil(timeout: 5) { self.row(app, "fx-4").label.contains("Done") },
            "fx-4's finished turn moves it to Done: \(row(app, "fx-4").label)")
        XCTAssertTrue(
            waitUntil(timeout: 5) {
                let label = self.row(app, "fx-5").label
                return label.contains("Running") && label.contains("--pass 8")
            },
            "fx-5's turn moves it to Running, on its last command: \(row(app, "fx-5").label)")
    }

    /// A refused Stop greys for its cooldown and comes back, on its row, with
    /// the daemon's sentence beneath it.
    func testAGreyedStopComesBackOnItsRow() {
        let app = launch(["-CC_CODEX", "stop-refused-late"])
        let stop = app.buttons["stop-cx-1"]
        let reason = app.descendants(matching: .any).matching(
            NSPredicate(format: "label CONTAINS %@", "Try again in a moment")
        ).firstMatch
        XCTAssertTrue(stop.waitForExistence(timeout: 30), "Stop is offered on the Codex row")
        XCTAssertTrue(
            waitUntil(timeout: 15) { !reason.exists }, "the launch's own refusal cools down")

        stop.tap()
        XCTAssertTrue(
            reason.waitForExistence(timeout: 10), "the refusal greys Stop and says why, on the row")
        XCTAssertTrue(
            app.descendants(matching: .any).matching(
                NSPredicate(format: "label CONTAINS %@", "lost its control link")
            ).firstMatch.exists,
            "the daemon's sentence is under the row")
        XCTAssertTrue(waitUntil(timeout: 15) { !reason.exists }, "and Stop comes back")
    }

    /// The link drops and returns: the run's capability is unknown while it is
    /// away and known again after, on the row.
    func testCapabilityFollowsTheLinkOnItsRow() {
        let app = launch(["-CC_FIXTURE", "ended", "-CC_FIXTURE_LINK_DROP_MS", "6000"])
        let live = row(app, "fx-1")
        XCTAssertTrue(live.waitForExistence(timeout: 25))
        XCTAssertTrue(
            waitUntil(timeout: 5) { live.label.hasSuffix("control") },
            "a known capability is spoken: \(live.label)")
        XCTAssertTrue(
            waitUntil(timeout: 15) { !live.label.hasSuffix("control") },
            "while the link is down nothing is claimed: \(live.label)")
        XCTAssertTrue(
            waitUntil(timeout: 15) { live.label.hasSuffix("control") },
            "and it is claimed again when the link returns: \(live.label)")
    }

    private func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return condition()
    }
}
