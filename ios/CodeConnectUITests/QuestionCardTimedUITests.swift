import XCTest

/// `QuestionCardUITests`'s tests that rest on time: a fixture clock set at launch, an elapsed-time budget or a long press.
/// They run on this Mac before every release and are left off the CI runner, which is slower than they allow.
final class QuestionCardTimedUITests: QuestionCardUITestsBase {
    /// The card sends what it held when the answer was given, so nothing on
    /// it can change while that answer is on its way: the step's choices are
    /// disabled until it lands.
    func testTheChoicesAreLockedWhileAnAnswerIsSent() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "held",
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "-CC_FIXTURE_ANSWER_MS", "20000",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let answer = app.buttons["Answer"].firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 30), "the question's row on the timeline")
        answer.tap()
        let option = app.buttons["question-0-option-0"]
        XCTAssertTrue(option.waitForExistence(timeout: 15))
        XCTAssertTrue(option.isEnabled, "a choice is live before anything is sent")

        app.buttons["question-decline"].tap()
        let locked = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "isEnabled == false"), object: option)
        XCTAssertEqual(
            XCTWaiter.wait(for: [locked], timeout: 5), .completed,
            "the choices are locked while the answer is sent")
        XCTAssertFalse(app.buttons["question-next"].isEnabled, "and so is the step's navigation")
    }
}
