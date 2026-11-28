import XCTest

/// What a person types on the phone and has not sent outlives leaving the screen it was typed on, as on the Mac: the
/// reply, a Deny's reason and a question's answer stay with the run (`RunDrafts`), and the first message stays with the
/// start until a start is sent.
final class DraftsKeptUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launch(_ arguments: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            arguments + [
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ]
        app.launch()
        return app
    }

    /// A vertical-axis TextField is backed by a text view, not a text field.
    private func field(_ app: XCUIApplication, _ id: String) -> XCUIElement {
        app.textViews[id].exists ? app.textViews[id] : app.textFields[id]
    }

    private func value(_ element: XCUIElement) -> String { element.value as? String ?? "" }

    func testTheReplyOutlivesLeavingTheSession() {
        let app = launch(["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"])
        let reply = app.textViews.firstMatch.waitForExistence(timeout: 25) ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertTrue(reply.exists, "the reply box")
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "listed-run"
        shot.lifetime = .keepAlways
        add(shot)
        reply.tap()
        reply.typeText("kept reply")

        app.navigationBars.buttons.firstMatch.tap()
        // A row is keyed by the run's uid (`SessionSummary.sessionKey`).
        let row = app.buttons["session-01CL000000000000000000DEF1"]
        XCTAssertTrue(row.waitForExistence(timeout: 10), "back on the fleet")
        row.tap()

        let again = app.textViews.firstMatch.waitForExistence(timeout: 10) ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertEqual(value(again), "kept reply")
    }

    func testADenyReasonOutlivesClosingTheDeck() {
        let app = launch(["-CC_FIXTURE", "deck"])
        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25), "the Deck's bar")
        bar.tap()
        let open = app.buttons["Deny with a reason"].firstMatch
        XCTAssertTrue(open.waitForExistence(timeout: 15), "the card's Deny with a reason")
        open.tap()
        let reason = field(app, "Reason for denying")
        XCTAssertTrue(reason.waitForExistence(timeout: 5), "the reason field")
        reason.tap()
        reason.typeText("kept reason")

        app.buttons["deck-done"].tap()
        XCTAssertTrue(bar.waitForExistence(timeout: 10), "back on the fleet")
        bar.tap()
        // A kept reason keeps its box open.
        let again = field(app, "Reason for denying")
        XCTAssertTrue(again.waitForExistence(timeout: 15), "the same card, its reason box open")
        XCTAssertEqual(value(again), "kept reason")
    }

    func testAQuestionsTypedAnswerOutlivesClosingTheCard() {
        let app = launch(["-CC_QUESTION", "held", "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR"])
        let answer = app.buttons["Answer"].firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 30), "the question's row")
        answer.tap()
        let next = app.buttons["question-next"].firstMatch
        XCTAssertTrue(app.buttons["question-0-option-0"].waitForExistence(timeout: 15), "the question card")
        app.buttons["question-0-option-0"].tap()
        next.tap()
        XCTAssertTrue(app.buttons["question-1-option-0"].waitForExistence(timeout: 5))
        app.buttons["question-1-option-0"].tap()
        next.tap()
        XCTAssertTrue(app.buttons["question-2-other"].waitForExistence(timeout: 5))
        app.buttons["question-2-other"].tap()
        let other = app.textFields["Other"]
        XCTAssertTrue(other.waitForExistence(timeout: 5))
        other.tap()
        app.typeText("kept answer")

        app.buttons["Close"].firstMatch.tap()
        XCTAssertTrue(answer.waitForExistence(timeout: 10), "back on the session")
        answer.tap()
        XCTAssertTrue(app.buttons["question-0-option-0"].waitForExistence(timeout: 15), "the card again")
        XCTAssertTrue(app.buttons["question-0-option-0"].isSelected, "the first pick is still made")
        next.tap()
        XCTAssertTrue(app.buttons["question-1-option-0"].waitForExistence(timeout: 5))
        next.tap()
        let again = app.textFields["Other"]
        XCTAssertTrue(again.waitForExistence(timeout: 5), "Other is still chosen")
        XCTAssertEqual(value(again), "kept answer")
    }

    /// New session, through to the Ready step in a recent folder.
    private func toReady(_ app: XCUIApplication) {
        let entry = app.buttons["start-entry"]
        XCTAssertTrue(entry.waitForExistence(timeout: 20), "New session")
        entry.tap()
        let computer = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-computer-'")).firstMatch
        XCTAssertTrue(computer.waitForExistence(timeout: 10), "a computer")
        computer.tap()
        let project = app.buttons["start-project-query-parser"]
        XCTAssertTrue(project.waitForExistence(timeout: 10), "a recent folder")
        project.tap()
        let claude = app.buttons["start-agent-claude"]
        XCTAssertTrue(claude.waitForExistence(timeout: 10), "the agent step")
        claude.tap()
        let main = app.buttons["start-place-main"]
        XCTAssertTrue(main.waitForExistence(timeout: 10), "the branch step")
        main.tap()
        XCTAssertTrue(app.buttons["start-go"].waitForExistence(timeout: 10), "the Ready step")
    }

    func testTheFirstMessageOutlivesClosingTheStartSheet() {
        let app = launch(["-CC_FIXTURE", "deck", "-CC_FIXTURE_REAL_NAMES", "YES"])
        toReady(app)
        let message = field(app, "start-message")
        XCTAssertTrue(message.waitForExistence(timeout: 5), "the first message")
        message.tap()
        message.typeText("kept first message")

        app.buttons["Close"].firstMatch.tap()
        toReady(app)
        XCTAssertEqual(value(field(app, "start-message")), "kept first message")
    }

    /// The reply box writes into the run's drafts, so it is drawn while the screen lists the run, as on the Mac: a
    /// screen for a run the phone does not list (here a link to a run that is not there) has none.
    func testAScreenForARunThePhoneDoesNotListHasNoReplyBox() {
        let app = launch(["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/no-such-run"])
        let empty = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'Not on your computer'")).firstMatch
        XCTAssertTrue(empty.waitForExistence(timeout: 25), "the screen for a run that is not listed")
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "unlisted-run"
        shot.lifetime = .keepAlways
        add(shot)
        XCTAssertFalse(app.textViews.firstMatch.exists || app.textFields.firstMatch.exists, "no reply box")
    }
}
