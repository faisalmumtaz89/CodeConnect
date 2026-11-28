import XCTest

/// **A session stopped at a usage limit says so over its reply box, as the Mac's band does**: which limit and, under it,
/// the account's windows; the box stays live, its prompt saying when a message goes through. On `cx-1`, a live Codex
/// session whose account's 5-hour window is full, stopped at that limit (`-CC_FIXTURE_LIMIT`).
final class LimitBandUITests: XCTestCase {

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

    func testTheBandSaysWhichLimitAndTheBoxWhenToSend() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/cx-1",
            "-CC_FIXTURE_USAGE", "YES", "-CC_ACCOUNTS_WIRE", accounts,
            // cx-1 by its key, its session_uid.
            "-CC_OPEN", "01CX000000000000000000DEF1", "-CC_FIXTURE_LIMIT", "five_hour",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let band = app.descendants(matching: .any)["session.limit"]
        XCTAssertTrue(band.waitForExistence(timeout: 25), "the band over the box")
        XCTAssertTrue(band.label.hasPrefix("5-hour limit reached"), band.label)
        XCTAssertTrue(band.label.contains("5-hour 100%"), band.label)
        let field = app.textViews.firstMatch.exists ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertEqual(field.placeholderValue, "Send after the reset to carry on")
    }
}
