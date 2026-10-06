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
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()

        let answer = app.buttons["Answer"].firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 30), "the question's row on the timeline")
        answer.tap()
        XCTAssertTrue(
            app.staticTexts["4 questions"].waitForExistence(timeout: 15),
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
            "Unit tests",
            "Soak test against a real Mac for eight hours",
            "Archive it under docs/plans, café ☕",
            "Side by side",
            "Notes: wider screens only",
        ] {
            XCTAssertTrue(app.staticTexts[line].exists, "Review lists \(line)")
        }
        XCTAssertTrue(submit.isEnabled)
        submit.tap()

        XCTAssertTrue(
            app.staticTexts["Claude has the answers sent from the phone."].waitForExistence(timeout: 10),
            "the confirmed answer")
        XCTAssertFalse(
            app.staticTexts["Answers sent"].exists || app.staticTexts["ANSWERS SENT"].exists,
            "the banner says it once")
        XCTAssertTrue(app.staticTexts["Side by side"].exists, "with what was sent")
        XCTAssertFalse(submit.exists, "an answered card offers nothing more")
        XCTAssertFalse(app.buttons["Allow"].exists)
    }

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

    /// Choosing the answer that completes the card does not move the bar at
    /// AX5, where every line is tallest: Submit's note is cleared, not removed
    /// (removed, Submit moved 22.7pt at L and 105pt here).
    func testTheBarStaysPutWhenTheLastAnswerIsChosenAtAX5() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "held",
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "-UIPreferredContentSizeCategoryName",
            "UICTContentSizeCategoryAccessibilityXXXL",
        ]
        app.launch()
        let answer = app.buttons["Answer"].firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 30))
        answer.tap()
        let next = app.buttons["question-next"].firstMatch
        for index in 0..<3 {
            let option = app.buttons["question-\(index)-option-0"]
            XCTAssertTrue(option.waitForExistence(timeout: 10))
            tapAboveTheBar(option, in: app)
            next.tap()
        }
        next.tap()  // the fourth left unanswered
        let submit = app.buttons["question-submit"].firstMatch
        XCTAssertTrue(submit.waitForExistence(timeout: 5))
        XCTAssertFalse(submit.isEnabled, "one question unanswered")
        let before = submit.frame

        app.buttons["question-3-change"].tap()
        let last = app.buttons["question-3-option-0"]
        XCTAssertTrue(last.waitForExistence(timeout: 5))
        tapAboveTheBar(last, in: app)
        next.tap()
        XCTAssertTrue(submit.waitForExistence(timeout: 5))
        XCTAssertTrue(submit.isEnabled, "every question answered")
        XCTAssertEqual(submit.frame.minY, before.minY, accuracy: 0.5, "the bar did not move")
        XCTAssertEqual(submit.frame.height, before.height, accuracy: 0.5)
    }

    /// At AX5 the pinned bar covers the lower part of the sheet, and a plain
    /// `tap()` on an option behind it presses the bar's Next instead. Taps the
    /// option's top, after dragging the sheet up while the bar hides it.
    private func tapAboveTheBar(_ element: XCUIElement, in app: XCUIApplication) {
        let window = app.windows.firstMatch
        // The bar's highest control: Back stacks above Next at AX5.
        func barTop() -> CGFloat {
            ["question-back", "question-next", "question-submit"]
                .map { app.buttons[$0].firstMatch }
                .filter(\.exists)
                .map(\.frame.minY)
                .min() ?? window.frame.maxY
        }
        for _ in 0..<6 where element.frame.minY + 24 > barTop() - 16 {
            window.coordinate(withNormalizedOffset: CGVector(dx: 0.05, dy: 0.6)).press(
                forDuration: 0.05,
                thenDragTo: window.coordinate(withNormalizedOffset: CGVector(dx: 0.05, dy: 0.35)),
                withVelocity: .slow, thenHoldForDuration: 0.25)
        }
        XCTAssertLessThan(element.frame.minY + 24, barTop() - 16, "the option is above the bar")
        window.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: element.frame.midX, dy: element.frame.minY + 12)).tap()
        let chosen = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "isSelected == true"), object: element)
        XCTAssertEqual(XCTWaiter.wait(for: [chosen], timeout: 3), .completed, "the option was chosen")
    }

    /// A question only the Mac can answer is still shown whole, its previews
    /// included, so the reader knows what is being asked there.
    func testAQuestionAtTheMacShowsItsPreviews() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "at-mac",
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        // Only the Mac can answer it, so its row offers View, not Answer.
        XCTAssertTrue(
            app.staticTexts["Asking at the Mac"].waitForExistence(timeout: 30),
            "the row says where the question is")
        XCTAssertFalse(app.buttons["Answer"].exists)
        app.buttons["View"].firstMatch.tap()
        for option in 0..<2 {
            XCTAssertTrue(
                app.descendants(matching: .any)["question-3-option-\(option)-preview"]
                    .waitForExistence(timeout: 15),
                "the Layout question's preview \(option)")
        }
    }

    /// At the largest accessibility size Decline sits below the question in the
    /// card's scroll rather than in the pinned bar; scrolled to, it is a real
    /// control that declines the question.
    func testDeclineIsReachableAtTheLargestTextSize() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "held",
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryAccessibilityXXXL",
        ]
        app.launch()
        let answer = app.buttons["Answer"].firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 30))
        answer.tap()
        XCTAssertTrue(app.buttons["question-next"].firstMatch.waitForExistence(timeout: 15))

        let decline = app.buttons["question-decline"].firstMatch
        let next = app.buttons["question-next"].firstMatch
        let window = app.windows.firstMatch
        // Clear of the pinned bar, not merely on screen: a control whose middle
        // is under the bar or the home indicator is not one a reader can tap.
        func clear() -> Bool { decline.exists && decline.frame.maxY <= next.frame.minY }
        // Slow drags held at their end, so the content is at rest when Decline
        // is tapped: a tap on a list still coasting only stops it.
        for _ in 0..<12 where !clear() {
            window.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.6)).press(
                forDuration: 0.05,
                thenDragTo: window.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.25)),
                withVelocity: .slow, thenHoldForDuration: 0.2)
        }
        XCTAssertTrue(clear(), "Decline scrolls clear of the pinned bar")
        XCTAssertTrue(decline.isHittable, "and is on screen to be tapped")
        decline.tap()
        XCTAssertTrue(
            app.staticTexts["Declined from the phone, as Escape does at the Mac."]
                .waitForExistence(timeout: 10),
            "the decline was sent")
        XCTAssertFalse(app.buttons["question-decline"].exists)
    }

    /// Submit is never live while any question is unanswered.
    func testSubmitWaitsForEveryAnswer() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_QUESTION", "held",
            "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let answer = app.buttons["Answer"].firstMatch
        XCTAssertTrue(answer.waitForExistence(timeout: 30))
        answer.tap()
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
