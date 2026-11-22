import XCTest

/// An ended run names the model it ran on, which the Mac names in its pane heading: one that takes no reply shows its
/// settings line, read only, over the line that says it has ended, as one the next message relaunches shows it over its
/// reply box.
final class EndedRunModelUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testARunThatTakesNoReplyNamesItsModelOverTheEndedLine() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "ended", "-CC_BIOMETRICS", "allow",
            "-CC_ENDED_MODEL", "claude-sonnet-5-5", "-CC_ENDED_MODEL_LABEL", "Sonnet 5.5", "-CC_ENDED_EFFORT", "high",
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNP00",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()

        let ended = app.descendants(matching: .any).matching(identifier: "session-ended-line").firstMatch
        XCTAssertTrue(ended.waitForExistence(timeout: 25), "a run that takes no reply: the ended line")
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "ended-not-relaunchable"
        shot.lifetime = .keepAlways
        add(shot)
        // The line's words; its glyph carries the same identifier.
        let line = app.staticTexts.matching(identifier: "session.composer.model").firstMatch
        XCTAssertTrue(line.waitForExistence(timeout: 5), "its model, read only")
        XCTAssertTrue(line.label.hasPrefix("Sonnet 5.5"), line.label)
        XCTAssertFalse(app.buttons["session.composer.settings"].exists, "nothing to change on a run that has ended")
        XCTAssertLessThan(line.frame.maxY, ended.frame.minY + 1, "over the ended line")
    }
}
