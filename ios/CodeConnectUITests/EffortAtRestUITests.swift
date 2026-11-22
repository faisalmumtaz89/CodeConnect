import XCTest

/// The settings line above the reply box names the effort in use after the model, at rest, by Kit's one rule
/// (`RunSettings.effortName`): the effort the agent named, or "Default" while it has named none. The model's own words
/// are `RunSettings`' to choose, so only what follows them is asserted.
final class EffortAtRestUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func settingsLine(_ session: String) -> XCUIElement {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings", "-CC_BIOMETRICS", "allow",
            "-CC_DEEPLINK", "codeconnect://session/\(session)",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let line = app.buttons["session.composer.settings"]
        XCTAssertTrue(line.waitForExistence(timeout: 25), "\(session): the settings line")
        return line
    }

    func testTheEffortInUseIsNamedAfterTheModel() {
        // fx-1: Claude on Sonnet, its effort `high`, in Plan.
        let label = settingsLine("fx-1").label
        XCTAssertTrue(label.hasSuffix(", High effort, Plan"), label)
    }

    func testARunWhoseAgentNamedNoEffortReadsDefault() {
        // cx-1: a Codex run whose model offers efforts and whose agent has named none, in its default mode.
        let label = settingsLine("cx-1").label
        XCTAssertTrue(label.hasSuffix(", Default effort"), label)
    }
}
