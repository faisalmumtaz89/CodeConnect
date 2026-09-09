import XCTest

// =============================================================================
//  CodexBehaviourTests — the things a screenshot cannot see.
//
//  The render pass photographs a state at rest. These drive the app and assert
//  what it *does*, in the only place the whole stack is real: a signed-out
//  runner tapping a running app, through the same fixtures the renders use.
//
//  They live beside `SheetPresentationTests` and run in the same
//  `behaviour_gate`, at `L` only — every property here is about behaviour, and
//  none of it changes with type size.
// =============================================================================

final class CodexBehaviourTests: XCTestCase {

    private func launch(_ state: String, deepLink: String? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        var arguments = ["-CC_CODEX", state]
        if let deepLink { arguments += ["-CC_DEEPLINK", deepLink] }
        app.launchArguments = arguments
        app.terminate()
        app.activate()
        return app
    }

    private func driver(_ timeout: TimeInterval = 30) -> RenderDriver {
        RenderDriver(test: self, timeout: timeout)
    }

    /// **A Codex card offers its own options and neither Allow nor Deny.**
    ///
    /// Both are refused *by name* at the Mac — *"a Codex card is answered by
    /// naming one of the options it offered; this decision names none of them,
    /// so nothing was sent"* — so a bar carrying them would be two controls
    /// whose only behaviour is a refusal.
    ///
    /// Driven on the **two-option** card, which is the shape that was
    /// unanswerable before this phase: Claude's `> 2` rule suppressed the option
    /// list and left exactly the pair Codex will not accept.
    func testACodexCardOffersItsOwnOptionsAndNoAllowDeny() throws {
        let app = launch(
            "card-two-options",
            deepLink: "codeconnect://session/\(RenderCatalog.codexSessionKey)")
        let driver = driver()
        if !app.staticTexts["Decision"].waitForExistence(timeout: 5) {
            try driver.tapRow(app, "Review")
        }
        try driver.require(driver.text(containing: "Decision", in: app), "the decision sheet")

        try driver.require(
            driver.element(containing: "Yes, proceed", in: app), "Codex's first option")
        try driver.require(
            driver.element(containing: "tell Codex what to do differently", in: app),
            "Codex's second option — a two-option card is still answerable")

        XCTAssertFalse(app.buttons["Allow"].exists, "a Codex card must not offer Allow")
        XCTAssertFalse(app.buttons["Deny"].exists, "a Codex card must not offer Deny")
        app.terminate()
    }

    /// **The G1 regression, end to end.**
    ///
    /// Before this phase a Codex `approval_resolved` never reached its card:
    /// its payload is a bare `CodexResolution` carrying none of `AnswerOutcome`'s
    /// fields, so the decode returned nil, nothing was recorded, and the card
    /// stayed live and tappable on the phone **after it had already been
    /// answered at the Mac**.
    func testAResolvedCodexCardIsNotTappable() throws {
        // Deep-linked by name: once a card is resolved the Codex run is an
        // ordinary row and does not reliably sort first — see `openResolvedCard`.
        let app = launch(
            "resolved-at-mac", deepLink: "codeconnect://session/\(RenderCatalog.codexSessionKey)")
        let driver = driver()

        // The row itself has already changed: no `Review`, no wait clock.
        XCTAssertFalse(
            app.buttons["Review"].exists,
            "a resolved card is not still asking to be reviewed")

        // And the card behind `View` is readable but not answerable.
        let view = app.buttons["View"].firstMatch
        try driver.require(view, "the resolved card's View button")
        try driver.scrollUntil(app, "the View button on screen") { view.isHittable }
        view.tap()
        try driver.require(
            driver.element(containing: "Answered at the Mac", in: app), "the ending's sentence")
        XCTAssertFalse(app.buttons["Allow"].exists)
        XCTAssertFalse(app.buttons["Deny"].exists)
        XCTAssertFalse(
            app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Option '")).firstMatch
                .exists,
            "a resolved card exposes no answer surface at all")
        app.terminate()
    }

    /// **The ordering trap (test plan S3).** A `turn_complete` arrives and *then*
    /// the resolution. Neither may leave a live card on a dead turn, and no
    /// frame in between may show one.
    ///
    /// Staged as `cleared-turn-aborted`, whose fixture delivers exactly that
    /// order through the real ingest path.
    func testTheOrderingTrapLeavesNoLiveCardOnADeadTurn() throws {
        let app = launch(
            "cleared-turn-aborted",
            deepLink: "codeconnect://session/\(RenderCatalog.codexSessionKey)")
        let driver = driver()
        XCTAssertFalse(app.buttons["Review"].exists)
        let view = app.buttons["View"].firstMatch
        try driver.require(view, "the resolved card's View button")
        try driver.scrollUntil(app, "the View button on screen") { view.isHittable }
        view.tap()
        try driver.require(
            driver.element(containing: "The turn was stopped", in: app),
            "the turn-stopped sentence")
        XCTAssertFalse(app.buttons["Allow"].exists)
        // And Stop is gone with the turn: there is nothing left to name.
        XCTAssertFalse(
            app.buttons["stop-\(RenderCatalog.codexSessionKey)"].exists,
            "a turn that has been stopped is not a turn that can be stopped")
        app.terminate()
    }

    /// **The `deeplink-request-race` defect, named rather than quietly omitted.**
    ///
    /// `codeconnect://session/<id>?request=<rid>` should open a session with its
    /// decision card already showing. It opens it at `L` and **not** at AX5,
    /// because the route is consumed once and never retried: `consumeDeepLink`
    /// spends the link before `state.pendingApprovals` is read, so a slower
    /// launch resolves the route before the approval has arrived and the request
    /// is spent on nothing — and `.onChange(of: model.pendingDeepLink)` cannot
    /// re-fire, because the value was just cleared.
    ///
    /// It is **pre-existing**, it is not a Codex defect, and Phase 5 Step 2 was
    /// told explicitly not to fix it. So this skips, with the defect named in
    /// the skip reason — the test plan's own rule, because a test quietly
    /// omitted is a defect quietly forgotten. When somebody fixes
    /// `SessionDetailView`'s `case .session(let reference, let requestID)` arm,
    /// delete the skip and this is the proof.
    func testTheDeepLinkOpensTheCardWhenTheApprovalArrivesLate() throws {
        throw XCTSkip(
            """
            deeplink-request-race, open and unfixed: `?request=` is consumed \
            before `pendingApprovals` is read, so a slower launch spends the \
            request id on nothing and nothing retries when the approval lands. \
            Filed, pre-existing, and out of scope for this phase — the Codex \
            card scenarios reach the sheet through its own Review button, which \
            is why they render at both sizes.
            """)
    }
}
