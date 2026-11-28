import XCTest

/// An ended session names the model it ran on, which the Mac names in its pane heading: one that takes no reply names
/// it, with its effort, on the line that says it has ended; one the next message relaunches names it in its reply box's
/// chip, read only.
final class EndedRunModelUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    /// The `ended` fixture's runs with a model and the effort they ended with, opened on the one `uid` names.
    private func open(_ uid: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "ended",
            "-CC_ENDED_MODEL", "claude-sonnet-5-5", "-CC_ENDED_MODEL_LABEL", "Sonnet 5.5", "-CC_ENDED_EFFORT", "high",
            "-CC_DEEPLINK", "codeconnect://session/\(uid)",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        return app
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }

    func testARunThatTakesNoReplyNamesItsModelOnTheEndedLine() {
        let app = open("01K1B3XQ8ZC0DE5FGH7JKMNP00")

        let ended = app.descendants(matching: .any).matching(identifier: "session-ended-line").firstMatch
        XCTAssertTrue(ended.waitForExistence(timeout: 25), "a run that takes no reply: the ended line")
        attach(app, "ended-not-relaunchable")
        let line = app.staticTexts.matching(identifier: "session.composer.model").firstMatch
        XCTAssertTrue(line.waitForExistence(timeout: 5), "its model, read only")
        XCTAssertEqual(line.label, "Sonnet 5.5 · High")
        XCTAssertFalse(app.buttons["session.composer.settings"].exists, "nothing to change on a run that has ended")
        XCTAssertTrue(ended.frame.contains(CGPoint(x: line.frame.midX, y: line.frame.midY)), "on the ended line")
    }

    /// An ended session the next message relaunches keeps its reply box, its model in the box's chip.
    func testARelaunchableRunNamesItsModelOverItsReplyBox() {
        let app = open("01K1B3XQ8ZC0DE5FGH7JKMNP07")
        let line = app.staticTexts.matching(identifier: "session.composer.model").firstMatch
        XCTAssertTrue(line.waitForExistence(timeout: 25), "its model, read only")
        XCTAssertTrue(line.label.hasPrefix("Sonnet 5.5"), line.label)
        attach(app, "ended-relaunchable")
    }
}
