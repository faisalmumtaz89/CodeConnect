import XCTest

/// The run's usage on its settings line is the button's value, never its label: VoiceOver reads the line's words as
/// they were, then the Kit's sentence for the usage the app holds (`ReplyBar.spoken`), as the Mac speaks it. On `cx-1`,
/// a live Codex run whose short line leaves room for all the figures, so the row asserted is one that draws them.
final class UsageRowUITests: XCTestCase {

    /// Codex's Default as you@example.com: its 5-hour window full, its week at 41%.
    private let accounts = """
        {"type":"accounts","request_id":"fx-usage","accounts":[],"active":[],"defaults":[\
        {"agent":"codex","facts":{"email":"you@example.com","plan":"plus","read_at":"2026-11-24T09:00:00Z","windows":[\
        {"used_percent":100,"window_minutes":300,"resets_at":4102444800},\
        {"used_percent":41,"window_minutes":10080,"resets_at":4102444800}]}}]}
        """

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launch(usage: Bool) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings",
            "-CC_DEEPLINK", "codeconnect://session/cx-1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ] + (usage ? ["-CC_FIXTURE_USAGE", "YES", "-CC_ACCOUNTS_WIRE", accounts] : [])
        app.launch()
        XCTAssertTrue(app.buttons["session.composer.settings"].waitForExistence(timeout: 25), "the settings line")
        return app
    }

    func testTheUsageIsTheLinesValueAndItsLabelIsUnchanged() {
        let plain = launch(usage: false).buttons["session.composer.settings"].label
        XCUIApplication().terminate()
        let app = launch(usage: true)
        let lines = app.buttons.matching(identifier: "session.composer.settings")
        XCTAssertEqual(lines.count, 1, "one settings control, the figures inside it")
        let line = lines.firstMatch
        XCTAssertEqual(line.label, plain, "the label reads as it did")
        let value = line.value as? String ?? ""
        XCTAssertTrue(value.hasPrefix("Context 390k of 1M; 5-hour 100%"), value)
        XCTAssertTrue(value.contains("; Weekly 41%"), value)
        XCTAssertTrue(value.hasSuffix("; Account you@example.com · Plus"), value)
    }
}
