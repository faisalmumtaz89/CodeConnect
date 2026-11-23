import XCTest

/// The run's usage on its settings line is the button's value, never its label: VoiceOver reads the line's words as
/// they were, then the Kit's sentence for the usage the app holds (`ReplyBar.spoken`), as the Mac speaks it.
final class UsageRowUITests: XCTestCase {

    /// Each agent's Default as you@example.com: its 5-hour window full, its week at 41%.
    private let accounts = """
        {"type":"accounts","request_id":"fx-usage","accounts":[],"active":[],"defaults":[\
        {"agent":"claude","facts":{"email":"you@example.com","plan":"max","read_at":"2026-11-24T09:00:00Z","windows":[\
        {"used_percent":100,"window_minutes":300,"resets_at":4102444800},\
        {"used_percent":41,"window_minutes":10080,"resets_at":4102444800}]}}]}
        """

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func settingsLine(usage: Bool) -> XCUIElement {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings", "-CC_BIOMETRICS", "allow",
            "-CC_DEEPLINK", "codeconnect://session/fx-2",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ] + (usage ? ["-CC_FIXTURE_USAGE", "YES", "-CC_ACCOUNTS_WIRE", accounts] : [])
        app.launch()
        let line = app.buttons["session.composer.settings"]
        XCTAssertTrue(line.waitForExistence(timeout: 25), "the settings line")
        return line
    }

    func testTheUsageIsTheLinesValueAndItsLabelIsUnchanged() {
        let plain = settingsLine(usage: false).label
        XCUIApplication().terminate()
        let line = settingsLine(usage: true)
        XCTAssertEqual(line.label, plain, "the label reads as it did")
        let value = line.value as? String ?? ""
        XCTAssertTrue(value.hasPrefix("Context 390k of 1M; 5-hour 100%"), value)
        XCTAssertTrue(value.contains("; Week 41%"), value)
        XCTAssertTrue(value.hasSuffix("; Account you@example.com"), value)
    }
}
