import XCTest

/// End-to-end against a **real** `ccd`. There is no mock daemon anywhere in this
/// target on purpose: the whole point is that a tap on this phone moves a
/// real agent on a real Mac, and only a real daemon can prove that.
///
///   xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
///     -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
///     TEST_RUNNER_CC_HOST=100.x.y.z TEST_RUNNER_CC_TOKEN="$(cc token)"
///
/// `testAnswerBlockedApproval` needs an approval already pending on the daemon;
/// it skips (rather than fails) when the fleet is clear, so the suite stays
/// meaningful when run casually.
final class ApprovalFlowUITests: XCTestCase {

    private var host: String {
        ProcessInfo.processInfo.environment["CC_HOST"] ?? ""
    }
    private var token: String {
        ProcessInfo.processInfo.environment["CC_TOKEN"] ?? ""
    }

    override func setUp() {
        continueAfterFailure = false
    }

    private func launchApp(resetCache: Bool = false) throws -> XCUIApplication {
        let visible = ProcessInfo.processInfo.environment.keys.filter {
            $0.hasPrefix("CC") || $0.hasPrefix("TEST")
        }.sorted()
        try XCTSkipIf(
            host.isEmpty || token.isEmpty,
            "CC_HOST/CC_TOKEN not provided; environment offered: \(visible)")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token]
        if resetCache { app.launchArguments += ["-CC_RESET_CACHE", "YES"] }
        app.launch()
        return app
    }

    /// Pairing, the fleet list, and the freshness pill all come up against a
    /// live daemon.
    func testFleetConnectsToLiveDaemon() throws {
        let app = try launchApp(resetCache: true)

        XCTAssertTrue(
            app.navigationBars["Fleet"].waitForExistence(timeout: 20),
            "Fleet should be the root screen once a token is present")

        let pill = app.buttons["Link health"]
        XCTAssertTrue(pill.waitForExistence(timeout: 20), "the freshness pill is always visible")

        // A live link is the only state that produces session rows.
        let anySession = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(
            anySession.waitForExistence(timeout: 30),
            "the daemon should report at least one session")
        attachScreenshot(app, name: "fleet")
    }

    /// The money path: a real pending approval, answered by a real tap.
    func testAnswerBlockedApproval() throws {
        let app = try launchApp()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))

        let review = deckBar(app)
        try XCTSkipUnless(
            review.waitForExistence(timeout: 45),
            "no approval is pending on the daemon; nothing to answer")
        attachScreenshot(app, name: "blocked-fleet")

        review.tap()

        // The bar opens the Deck, which lands on the riskiest card. The title is
        // the Deck's own — the per-session sheet is the other home of the same
        // card view, and asserting on *its* title made this test fail after the
        // Deck became the way in.
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 15),
            "the Deck should land on the decision card")
        attachScreenshot(app, name: "decision-card")

        // The card must say, in so many words, that the text is the hashed text.
        let verified = app.staticTexts["Verified — this is the exact text the daemon hashed."]
        XCTAssertTrue(
            verified.waitForExistence(timeout: 5),
            "payload_hash verification must pass for a card straight off the wire")

        let counter = app.descendants(matching: .any)["deck-count"].firstMatch
        let countBefore = counter.exists ? counter.label : ""

        let allow = app.buttons.matching(
            NSPredicate(format: "label BEGINSWITH 'Allow this'")
        ).firstMatch
        XCTAssertTrue(allow.waitForExistence(timeout: 5), "Allow must be offered")

        // **The read gate applies at every tier now, including LOW**, so a real
        // card whose command runs past the action bar arrives with `Allow` shut
        // and a sentence under it saying to scroll. That is the fix working, not
        // a failure — the previous rule let a LOW card be approved with half its
        // path off screen. Scroll to it the way a person would, then assert.
        for _ in 0..<10 where !allow.isEnabled {
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.45))
                .press(
                    forDuration: 0.05,
                    thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.15)))
        }
        XCTAssertTrue(
            allow.isEnabled,
            "a live link, an attached session and a command that has been on screen must "
                + "enable Allow")
        allow.tap()

        // No optimistic UI: every one of these three states is reached only
        // *after* `answer_result` comes back. Which one you get depends on what
        // else is in the queue — the Deck advances a settled card off the stack,
        // so the banner on it is not something the test can insist on seeing.
        let confirmed = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'confirmed by the daemon'")
        ).firstMatch
        let cleared = app.staticTexts["Fleet clear"]
        let settled = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in
                confirmed.exists || cleared.exists
                    || (counter.exists && counter.label != countBefore)
            }, object: nil)
        wait(for: [settled], timeout: 30)
        attachScreenshot(app, name: "resolution")
        XCTAssertTrue(
            confirmed.exists || cleared.exists
                || (counter.exists && counter.label != countBefore),
            "the resolution must be daemon-confirmed, not assumed")
    }

    /// The ledger guarantee, seen from the phone: answering a request that was
    /// already resolved elsewhere returns the *original* outcome instead of
    /// applying anything a second time.
    ///
    /// Orchestrated, so it is opt-in via `-only-testing`. Something outside the
    /// app must answer the open card during `externalAnswerWindow` — in the run
    /// this was written against that was a harness answering at the daemon, which
    /// is exactly the "you answered at the desk while the card was open on your
    /// phone" case this has to survive.
    func testDuplicateAnswerShowsOriginalOutcome() throws {
        let app = try launchApp()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))

        let review = deckBar(app)
        try XCTSkipUnless(review.waitForExistence(timeout: 45), "no approval is pending")
        review.tap()
        XCTAssertTrue(app.navigationBars["Needs you"].waitForExistence(timeout: 15))

        // The sheet holds the card it was opened with, so it stays answerable
        // while the outside world resolves it underneath.
        Thread.sleep(forTimeInterval: externalAnswerWindow)

        let allow = app.buttons.matching(
            NSPredicate(format: "label BEGINSWITH 'Allow this'")
        ).firstMatch
        XCTAssertTrue(allow.waitForExistence(timeout: 5))
        allow.tap()

        let duplicate = app.staticTexts["Already answered"]
        XCTAssertTrue(
            duplicate.waitForExistence(timeout: 30),
            "a second answer must report the original outcome, never re-apply")

        let original = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Original outcome:'")
        ).firstMatch
        XCTAssertTrue(original.exists, "the original outcome must be shown, not just the fact of it")
        attachScreenshot(app, name: "duplicate-answer")
    }

    /// Deny is Escape, and the reason is typed afterwards — two daemon round
    /// trips that both have to be confirmed before the card claims success.
    func testDenyWithReason() throws {
        let app = try launchApp()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))

        let review = deckBar(app)
        try XCTSkipUnless(review.waitForExistence(timeout: 45), "no approval is pending")
        review.tap()
        XCTAssertTrue(app.navigationBars["Needs you"].waitForExistence(timeout: 15))

        let openReasonField = app.buttons["Deny with a reason"]
        XCTAssertTrue(openReasonField.waitForExistence(timeout: 5))
        openReasonField.tap()

        // Query by label: the compose bar's field is still in the hierarchy
        // behind the sheet, and `firstMatch` finds that one.
        let field = app.textViews["Reason for denying"].exists
            ? app.textViews["Reason for denying"] : app.textFields["Reason for denying"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap()
        field.typeText("not now, use the scratch dir instead")

        let denyAndSend = app.buttons["Deny and send"]
        XCTAssertTrue(denyAndSend.waitForExistence(timeout: 5))
        denyAndSend.tap()

        let denied = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Denied — confirmed by the daemon'")
        ).firstMatch
        XCTAssertTrue(
            denied.waitForExistence(timeout: 30), "the denial must be daemon-confirmed")

        let typed = app.staticTexts["Your reason was typed into the session."]
        XCTAssertTrue(
            typed.waitForExistence(timeout: 30),
            "the reason has to reach the session, or the card must say it did not")
        attachScreenshot(app, name: "deny-with-reason")
    }

    private var externalAnswerWindow: TimeInterval {
        Double(ProcessInfo.processInfo.environment["CC_EXTERNAL_ANSWER_WINDOW"] ?? "") ?? 20
    }

    /// Typing into the agent's composer from the phone.
    func testComposeSendsText() throws {
        let app = try launchApp()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))

        // `cells.firstMatch` is the section header, not a session — rows carry
        // an explicit identifier so the query cannot drift.
        let firstSession = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        try XCTSkipUnless(firstSession.waitForExistence(timeout: 30), "no sessions")
        firstSession.tap()
        XCTAssertFalse(
            app.navigationBars["Fleet"].exists, "tapping a session must push the detail stack")

        // A vertical-axis TextField is backed by a text view, not a text field.
        let field = app.textViews.firstMatch.exists
            ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 15), "the compose bar is always present")
        field.tap()
        field.typeText("hi from the ui test")
        attachScreenshot(app, name: "session-detail")

        let send = app.buttons["Send"]
        XCTAssertTrue(send.waitForExistence(timeout: 5))
        try XCTSkipUnless(send.isEnabled, "send is disabled — link or capability says so")
        send.tap()

        let sent = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Typed into the session'")
        ).firstMatch
        let refused = app.staticTexts.matching(
            NSPredicate(format: "label BEGINSWITH 'Not typed:'")
        ).firstMatch

        // Text only lands when Claude's composer is actually on screen. If a
        // permission prompt is up, a refusal is the *correct* outcome — the
        // daemon must never type a message into a yes/no prompt — so that is a
        // skip, not a failure. What must never happen is silence.
        let outcome = expectation(description: "the daemon reports what happened to the text")
        let poll = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { _ in
            if sent.exists || refused.exists { outcome.fulfill() }
        }
        defer { poll.invalidate() }
        wait(for: [outcome], timeout: 30)

        attachScreenshot(app, name: "compose")
        try XCTSkipIf(
            refused.exists, "the session is not at its composer: \(refused.label)")
        XCTAssertTrue(sent.exists, "send_text must be confirmed by the daemon")
    }

    /// The bar that rises when something is pending — found by the identifier
    /// the view deliberately exposes, not by the word on the pill. Querying for
    /// "Review" matched nothing once the bar took an explicit accessibility
    /// label, and these three tests skipped instead of failing: a live suite
    /// that quietly stops exercising the money path is worse than no suite.
    private func deckBar(_ app: XCUIApplication) -> XCUIElement {
        app.buttons["deck-bar"].firstMatch
    }

    private func attachScreenshot(_ app: XCUIApplication, name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
