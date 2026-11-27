import XCTest

/// The run's usage is its own control in the reply box's bar, beside the model chip: its value is the Kit's sentence
/// for the usage the app holds (`ReplyBar.spoken`), as the Mac speaks it, and the chip's label reads as it did without
/// it. On `cx-1`, a live Codex run whose short name leaves room for all the figures.
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
        XCTAssertTrue(app.buttons["session.composer.settings"].waitForExistence(timeout: 25), "the model chip")
        return app
    }

    func testTheUsageIsItsOwnControlAndTheChipsLabelIsUnchanged() {
        let plain = launch(usage: false)
        let chip = plain.buttons["session.composer.settings"].label
        XCTAssertFalse(plain.buttons["session.composer.usage"].exists, "no usage held, no figures")
        plain.terminate()
        let app = launch(usage: true)
        XCTAssertEqual(app.buttons["session.composer.settings"].label, chip, "the chip reads as it did")
        let usage = app.buttons["session.composer.usage"]
        XCTAssertTrue(usage.waitForExistence(timeout: 5), "the figures, a control of their own")
        let value = usage.value as? String ?? ""
        XCTAssertTrue(value.hasPrefix("Context 390k of 1M; 5-hour 100%"), value)
        XCTAssertTrue(value.contains("; Weekly 41%"), value)
        XCTAssertTrue(value.hasSuffix("; Account you@example.com · Plus"), value)
    }
}
