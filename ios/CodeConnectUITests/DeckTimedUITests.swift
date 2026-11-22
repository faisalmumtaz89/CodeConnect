import XCTest

/// `DeckUITests`'s tests that rest on time: a fixture clock set at launch, an elapsed-time budget or a long press.
/// They run on this Mac before every release and are left off the CI runner, which is slower than they allow.
final class DeckTimedUITests: DeckUITestsBase {
    /// An answer the Mac has not confirmed says how long it has waited, and
    /// VoiceOver hears that as a duration — "4 seconds" — never as "4s",
    /// which it reads "four ess".
    func testAnUnconfirmedAnswerSpeaksHowLongItHasWaited() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "deck", "-CC_BIOMETRICS", "allow",
            "-UIPreferredContentSizeCategoryName", TypeSize.medium.rawValue,
            // Held until the test sends `answer`, which it never does: the answer stays on its way.
            "-CC_FIXTURE_ANSWER_HELD", "YES",
            "-CC_DEEPLINK", "codeconnect://deck/\(Self.lowRequestID)",
        ]
        FixtureSignals().arm(app)
        app.launch()
        let allow = allowButton(app, tool: "Read")
        XCTAssertTrue(allow.waitForExistence(timeout: 25))
        allow.tap()
        let notice = app.staticTexts.matching(
            NSPredicate(format: "label BEGINSWITH 'Still waiting on the computer'")
        ).firstMatch
        XCTAssertTrue(notice.waitForExistence(timeout: 15), "the unconfirmed notice")
        XCTAssertNotNil(
            notice.label.range(
                of: #"^Still waiting on the computer, \d+ seconds?\.$"#, options: .regularExpression),
            notice.label)
    }
}
