import XCTest

/// Claude's four-question card, answered end to end from the phone.
///
/// Staged by `-CC_QUESTION held`: one session holding the 2.1.286 question card
/// in the shape a minor-21 daemon sends it, decoded through the real decoders,
/// with answers resolved locally as under `-CC_FIXTURE`. What the daemon does
/// with the answer is ccd's; this is the reader's half — every step, the Review,
/// and a card that never offers Allow.
///
/// Pins its own Dynamic Type size, as every UI test in this target does.
final class QuestionCardUITests: XCTestCase {

    override func setUp() {
        continueAfterFailure = false
    }

    func testAnsweringTheFourQuestionCardEndToEnd() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "held",
            "-CC_DEEPLINK", "codeconnect://session/qx-1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()

        let review = app.buttons["Review"].firstMatch
        XCTAssertTrue(review.waitForExistence(timeout: 30), "the question's row on the timeline")
        review.tap()
        XCTAssertTrue(
            app.staticTexts["Claude has 4 questions"].waitForExistence(timeout: 15),
            "the question card, not the approval layout")
        XCTAssertFalse(app.buttons["Allow"].exists, "a question card never offers Allow")

        let next = app.buttons["question-next"].firstMatch
        let submit = app.buttons["question-submit"].firstMatch

        // 1 — a single choice.
        app.buttons["question-0-option-0"].tap()
        next.tap()
        // 2 — several choices, in the order tapped.
        XCTAssertTrue(app.buttons["question-1-option-0"].waitForExistence(timeout: 5))
        app.buttons["question-1-option-0"].tap()
        app.buttons["question-1-option-2"].tap()
        next.tap()
        // 3 — "Other", typed. Into whatever has focus: a focused SwiftUI field
        // can change its automation type under the query that found it.
        XCTAssertTrue(app.buttons["question-2-other"].waitForExistence(timeout: 5))
        app.buttons["question-2-other"].tap()
        let other = app.textFields["Other"]
        XCTAssertTrue(other.waitForExistence(timeout: 5))
        other.tap()
        app.typeText("Archive it under docs/plans, café ☕\n")
        next.tap()
        // 4 — a preview, and its notes.
        XCTAssertTrue(app.buttons["question-3-option-1"].waitForExistence(timeout: 5))
        app.buttons["question-3-option-1"].tap()
        XCTAssertTrue(
            app.descendants(matching: .any)["question-3-preview"].firstMatch.exists,
            "the chosen option's preview")
        // A vertical field is a text view or a text field depending on the OS.
        let notes = app.descendants(matching: .any).matching(
            NSPredicate(
                format: "label == 'Notes' AND (elementType == %d OR elementType == %d)",
                XCUIElement.ElementType.textField.rawValue,
                XCUIElement.ElementType.textView.rawValue)
        ).firstMatch
        XCTAssertTrue(notes.waitForExistence(timeout: 5))
        notes.tap()
        app.typeText("wider screens only")
        XCTAssertEqual(next.label, "Review")
        next.tap()

        // Review: every answer, before anything is sent.
        XCTAssertTrue(submit.waitForExistence(timeout: 5))
        for line in [
            "Dedupe with hardlinks across every worktree that shares them (Recommended)",
            "Unit tests, Soak test against a real Mac for eight hours",
            "Archive it under docs/plans, café ☕",
            "Side by side",
            "Notes: wider screens only",
        ] {
            XCTAssertTrue(app.staticTexts[line].exists, "Review lists \(line)")
        }
        XCTAssertTrue(submit.isEnabled)
        submit.tap()

        XCTAssertTrue(
            app.staticTexts["Claude has your answers."].waitForExistence(timeout: 10),
            "the confirmed answer")
        XCTAssertTrue(app.staticTexts["Your answers"].exists || app.staticTexts["YOUR ANSWERS"].exists)
        XCTAssertFalse(submit.exists, "an answered card offers nothing more")
        XCTAssertFalse(app.buttons["Allow"].exists)
    }

    /// Submit is never live while any question is unanswered.
    func testSubmitWaitsForEveryAnswer() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "held",
            "-CC_DEEPLINK", "codeconnect://session/qx-1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let review = app.buttons["Review"].firstMatch
        XCTAssertTrue(review.waitForExistence(timeout: 30))
        review.tap()
        let next = app.buttons["question-next"].firstMatch
        XCTAssertTrue(next.waitForExistence(timeout: 15))
        for _ in 0..<4 { next.tap() }
        let submit = app.buttons["question-submit"].firstMatch
        XCTAssertTrue(submit.waitForExistence(timeout: 5))
        XCTAssertFalse(submit.isEnabled, "four questions unanswered")
        XCTAssertTrue(app.staticTexts["Answer every question to submit."].exists)
        XCTAssertEqual(
            app.staticTexts.matching(NSPredicate(format: "label == 'Not answered yet'")).count, 4)
    }
}
