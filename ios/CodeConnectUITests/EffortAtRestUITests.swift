import XCTest

/// The reply box's model chip names the effort in use after the model to VoiceOver, at rest, by Kit's one rule
/// (`RunSettings.effortName`): the effort the agent named, or while it has named none its model's own default, by name
/// (decision 313); on screen the chip draws the model alone. The model's own words are `RunSettings`' to choose, so only
/// what follows them is asserted.
final class EffortAtRestUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func settingsLine(_ session: String) -> XCUIElement {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings",
            "-CC_DEEPLINK", "codeconnect://session/\(session)",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let line = app.buttons["session.composer.settings"]
        XCTAssertTrue(line.waitForExistence(timeout: 25), "\(session): the model chip")
        return line
    }

    func testTheEffortInUseIsNamedAfterTheModel() {
        // fx-1: Claude on Sonnet, its effort `high`, in Plan.
        let label = settingsLine("fx-1").label
        XCTAssertTrue(label.hasSuffix(", High effort, Plan"), label)
    }

    func testARunWhoseAgentNamedNoEffortReadsItsModelsDefault() {
        // cx-1: a Codex run on GPT-6.1-Sol, whose default effort is Low, whose agent has named none, in its default mode.
        let label = settingsLine("cx-1").label
        XCTAssertTrue(label.hasSuffix(", Low effort"), label)
    }
}
