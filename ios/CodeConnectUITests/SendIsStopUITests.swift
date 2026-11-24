import XCTest

/// While a turn can be stopped, the reply box's circle is its Stop (`stop-<key>`, "Stop"); the session's header has
/// none. Text that can be sent now makes the circle Send; an empty box makes it Stop again; a draft that must wait
/// behind a card keeps Stop, since the circle cannot send it.
final class SendIsStopUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func open(_ state: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_CODEX", state,
            "-CC_DEEPLINK", "codeconnect://session/cx-1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        return app
    }

    /// A vertical-axis TextField is backed by a text view, not a text field.
    private func field(_ app: XCUIApplication) -> XCUIElement {
        app.textViews.firstMatch.exists ? app.textViews.firstMatch : app.textFields.firstMatch
    }

    func testTheCircleIsTheRunningTurnsStopAndTypedTextMakesItSend() {
        // compose-steered: a running turn with no card waiting, so a typed message can be sent now.
        let app = open("compose-steered")
        let stop = app.buttons["stop-cx-1"]
        XCTAssertTrue(stop.waitForExistence(timeout: 25), "the running turn's Stop")
        XCTAssertEqual(stop.label, "Stop")
        XCTAssertEqual(app.buttons.matching(identifier: "stop-cx-1").count, 1, "one Stop, the circle; none in the header")

        let field = field(app)
        field.tap()
        field.typeText("x")
        let send = app.buttons["session.composer.send"]
        XCTAssertTrue(send.waitForExistence(timeout: 5), "typed text makes the circle Send")
        XCTAssertEqual(send.label, "Send")
        XCTAssertFalse(stop.exists, "and the turn's Stop gives way to it")

        field.typeText(XCUIKeyboardKey.delete.rawValue)
        XCTAssertTrue(stop.waitForExistence(timeout: 5), "an empty box makes it the turn's Stop again")
    }

    func testADraftThatMustWaitBehindACardKeepsStop() {
        // stop-offered: the running turn waits on a card, so a message cannot be sent until it is answered.
        let app = open("stop-offered")
        let stop = app.buttons["stop-cx-1"]
        XCTAssertTrue(stop.waitForExistence(timeout: 25), "the running turn's Stop")

        let field = field(app)
        field.tap()
        field.typeText("x")
        XCTAssertTrue(
            app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'Answer the card first'")).firstMatch
                .waitForExistence(timeout: 5),
            "the note says why the draft waits")
        XCTAssertTrue(stop.exists, "the circle stays the turn's Stop")
        XCTAssertEqual(stop.label, "Stop")
        XCTAssertFalse(app.buttons["session.composer.send"].exists, "and offers no Send it could not keep")
    }
}
