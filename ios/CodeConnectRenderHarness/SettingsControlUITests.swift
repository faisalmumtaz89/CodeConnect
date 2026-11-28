import XCTest

/// **The composer's run-settings control (placement B, decision 238), driven on the device.** A render
/// shows the control drawn; these show it *works*: it is hittable and tapping it opens the settings sheet
/// for a live Codex run and a hosted Claude run; its hit area is the platform's 44pt even at the smallest
/// text size, where the label is smallest; and it is disabled, with its reason on screen, while a card
/// waits. Raw `XCUIApplication`, like `DeepLinkTests` — the harness `RenderDriver` is for render passes.
final class SettingsControlUITests: XCTestCase {

    private func launch(_ arguments: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = arguments
        app.terminate()
        app.activate()
        return app
    }

    private func control(_ app: XCUIApplication) -> XCUIElement {
        app.buttons["session.composer.settings"]
    }

    /// Hittable, then one tap opens the session's settings sheet (its "Permission" section is the mark).
    private func assertTapOpensSheet(_ app: XCUIApplication, _ who: String) {
        let button = control(app)
        XCTAssertTrue(button.waitForExistence(timeout: 10), "\(who): the control is drawn")
        XCTAssertTrue(button.isEnabled, "\(who): the control is hittable")
        button.tap()
        XCTAssertTrue(
            app.staticTexts["Permission"].waitForExistence(timeout: 10),
            "\(who): tapping the control opens the settings sheet")
        app.terminate()
    }

    func testTappingTheControlOpensTheSheetForAHostedClaudeRun() {
        assertTapOpensSheet(
            launch(["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"]), "Claude")
    }

    func testTappingTheControlOpensTheSheetForALiveCodexRun() {
        assertTapOpensSheet(
            launch(["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/cx-1"]), "Codex")
    }

    /// **The 44pt hit area at the smallest text size.** The label is at its smallest there, so most of the
    /// frame is transparent padding over the composer; a tap low in the frame must still land on the control
    /// and open the sheet, not fall through to the compose bar below it.
    func testTheControlIsTappableByItsFrameAtTheSmallestTextSize() {
        let app = launch([
            "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryXS",
        ])
        let button = control(app)
        XCTAssertTrue(button.waitForExistence(timeout: 10), "the control is drawn")
        button.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.9)).tap()
        XCTAssertTrue(
            app.staticTexts["Permission"].waitForExistence(timeout: 10),
            "a tap low in the 44pt frame opens the sheet")
        app.terminate()
    }

    /// **Disabled, with its reason, while a card waits.** A Codex run with a pending card refuses a send
    /// ("Answer the card first…"), so the control is dead and the reason is on screen. This is a Codex case:
    /// a Claude run with a card is not blocked — typed text is not a steer — so its control stays live.
    func testTheControlIsDisabledWithItsReasonWhileACardWaits() {
        let app = launch(["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/cx-blocked"])
        let button = control(app)
        XCTAssertTrue(button.waitForExistence(timeout: 10), "the control is drawn")
        XCTAssertFalse(button.isEnabled, "the control is disabled while a card waits")
        XCTAssertTrue(
            app.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "Answer the card first"))
                .firstMatch.waitForExistence(timeout: 10),
            "its reason is on screen")
        app.terminate()
    }
}
