import XCTest

/// The Deck — the product's signature interaction — driven end to end.
///
/// Fixture-backed rather than daemon-backed, and that is the point: the Deck
/// only means anything when *several* agents are blocked at once at *different*
/// risk classes, and that is not a state a test can arrange on a live Mac on
/// demand. The frames it replays are the daemon's own JSON, decoded by the app's
/// real decoders, with real `payload_hash` values — so a card that failed
/// verification here would fail on a phone too.
///
/// `ApprovalFlowUITests` covers the same cards against a real `ccd`. This file
/// covers the things that need a fleet.
///
/// **Every launch pins its own Dynamic Type size.** The suite used to run at
/// whatever the simulator had been left at, which meant a render pass at AX5
/// left four tests failing for a reason that had nothing to do with the code.
/// The read-gate tests below need both sizes anyway, and a size the test states
/// is a size the test can reason about.
///
/// **The waits are long on purpose.** A cold launch of this app on a busy
/// machine has been measured at over twenty seconds; a five-second wait turns
/// that into a failure that reads like a regression and is not one. The
/// assertions are what this suite is for, and none of them are about speed.
final class DeckUITests: XCTestCase {

    /// The two sizes the design is verified at.
    private enum TypeSize: String {
        case medium = "UICTContentSizeCategoryM"
        case ax5 = "UICTContentSizeCategoryAccessibilityXXXL"
    }

    /// The gate's own sentences, drawn by `ccDisabled` under the control they
    /// block. Not accessibility hints: a disabled control renders its reason as
    /// visible text.
    private static let readGateReason = "Scroll the command into view before deciding."
    /// What HIGH says once the command has been seen but the class it was given
    /// has not. MEDIUM never shows this one.
    private static let readGateHighReason =
        "Scroll to the end of the command block before deciding."

    override func setUp() {
        continueAfterFailure = false
    }

    /// The fixture's MEDIUM card — a `Write` to a path outside the worktree.
    private static let mediumRequestID = "toolu_fixture_medium"
    /// Its LOW card — a `Read` of a 24-character path.
    private static let lowRequestID = "toolu_fixture_low"

    private func launch(
        biometrics: String = "allow", size: TypeSize = .medium,
        fixture: String = "deck", openingCard requestID: String? = nil
    ) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", fixture,
            // The system biometric sheet cannot be driven by XCUITest, so the
            // *outcome* is injected. What is being tested is that the app asks
            // and honours the answer, which is exactly what this exercises.
            "-CC_BIOMETRICS", biometrics,
            "-UIPreferredContentSizeCategoryName", size.rawValue,
        ]
        if let requestID {
            // A URL can name one card; a tapped notification cannot — it
            // carries no identifier — so this is a deep link, not a stand-in
            // for a push. It is the only way to assert the gate on a *named*
            // card without three drags and a postpone in the way.
            app.launchArguments += ["-CC_DEEPLINK", "codeconnect://deck/\(requestID)"]
        }
        app.launch()
        return app
    }

    // MARK: The bar

    /// The accessory bar rises when something is pending, counts what is
    /// waiting, and is the only way into the queue.
    func testAccessoryBarRisesWithPendingCount() {
        let app = launch()
        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25))
        XCTAssertEqual(bar.label, "3 decisions need you")
        attach(app, "deck-bar")
    }

    /// **The headline and the bar count the same noun, 631pt apart.**
    ///
    /// They did not. The fleet's display line counted *sessions* in the Blocked
    /// band and the bar counted *cards* in the Deck, in identical words — so two
    /// agents holding three approvals rendered `2 need you` in the largest type
    /// on the screen and `3 need you` in the second largest, both true and only
    /// one of them answerable.
    ///
    /// It could not be caught in the `deck` fixture, where three agents hold one
    /// card each and both counts read 3 by coincidence. `stacked` is that fleet
    /// with a second decision on `fx-2`: three blocked rows, four decisions.
    func testHeadlineAndAccessoryBarCountTheSameThing() {
        let app = launch(fixture: "stacked")
        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25))

        XCTAssertEqual(bar.label, "4 decisions need you", "three agents, four decisions")
        let headline = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'decisions need you'")
        ).firstMatch
        XCTAssertTrue(headline.waitForExistence(timeout: 20))
        XCTAssertTrue(
            headline.label.contains(bar.label),
            "the two loudest strings on the screen must not disagree: "
                + "\(headline.label) against \(bar.label)")

        // …and the row that is holding two says so, so the arithmetic closes for
        // a reader: three rows, one of them `+1 more`.
        XCTAssertTrue(
            app.buttons["session-fx-2"].label.contains("2 pending decisions"),
            "the stacked agent has to account for the difference")
        attach(app, "counts-agree")
    }

    /// The bar advertises the card `Review` actually opens. Under the old
    /// age-first order it named `app-3 · Read` — the least consequential item in
    /// the queue — while a `git push --force` sat above it in the stack.
    func testAccessoryBarAdvertisesTheCardReviewOpens() {
        let app = launch()
        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25))
        XCTAssertTrue(
            bar.value as? String == nil || (bar.value as! String).contains("git push --force"),
            "the bar's preview must name the riskiest card, not the oldest: \(bar.value ?? "nil")")
    }

    /// **The door still opens where the door is drawn.**
    ///
    /// At accessibility sizes the decorative `Review` button is replaced by a
    /// chevron on the aggregate's own line — `Review` never carried the hit area
    /// (the whole bar is the target) so nothing was lost, but a control
    /// that measures in the accessibility tree and misses under a thumb passes
    /// every assertion that asks the tree. So this taps a **coordinate**, at the
    /// chevron's corner of the bar, rather than asking XCUITest for an element.
    func testTheBarOpensTheDeckFromWhereTheChevronIsDrawnAtAX5() {
        let app = launch(size: .ax5)
        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25))
        bar.coordinate(withNormalizedOffset: CGVector(dx: 0.88, dy: 0.28)).tap()
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 20),
            "the bar is the button; a tap where the chevron is drawn must open the deck")
        attach(app, "deck-opened-from-chevron-ax5")
    }

    // MARK: Ordering

    /// **Urgency first, then age.** The fixture's HIGH card is the *newest* of
    /// the three (60s against 180s and 300s) and its LOW `Read` is the oldest,
    /// so seeing `Bash` on top proves the ranking rule rather than a
    /// coincidence — and proves it is not the old oldest-first rule.
    func testDeckOpensOnTheRiskiestCardAcrossTheFleet() {
        let app = launch()
        openDeck(app)
        XCTAssertTrue(
            app.staticTexts["Bash"].firstMatch.waitForExistence(timeout: 20),
            "a force-push must not sit behind a Read because the Read is older")
        // The badge's label spells the class out; "HIGH" alone is decoration.
        XCTAssertTrue(
            app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'Risk HIGH'"))
                .firstMatch.exists)
        attach(app, "deck-top-card")
    }

    /// Nothing here is a swipe. A stray horizontal drag across the card must
    /// leave the decision exactly where it was.
    func testSwipingTheCardDecidesNothing() {
        let app = launch()
        openDeck(app)
        XCTAssertTrue(app.staticTexts["Bash"].firstMatch.waitForExistence(timeout: 20))

        app.staticTexts["Bash"].firstMatch.swipeLeft()
        app.staticTexts["Bash"].firstMatch.swipeRight()

        XCTAssertTrue(
            app.staticTexts["Bash"].firstMatch.exists,
            "an accidental swipe approving a command is the one bug that ends this product")
        XCTAssertFalse(app.staticTexts["Fleet clear"].firstMatch.exists)
    }

    // MARK: The read-before-you-decide gate

    /// **The gate is a viewport test.**
    ///
    /// It used to arm on `onAppear` of a marker nested inside an
    /// already-materialised `LazyVStack` child, which fires on creation and not
    /// on scroll. A HIGH card at AX5 then showed `Hold to allow`
    /// fully armed with the words `EXACT COMMAND` as the last thing on screen
    /// and **zero characters of the command rendered** — a face authenticating a
    /// command the product never showed.
    ///
    /// Nothing in either test target referenced `hasSeenCommand`, `armGate` or
    /// this sentence before now.
    func testHighCardIsGatedWhenTheCommandIsBelowTheFoldAtAX5() {
        let app = launch(size: .ax5)
        openDeck(app)

        let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
        XCTAssertTrue(hold.waitForExistence(timeout: 25))
        attach(app, "gate-high-ax5-shut")
        XCTAssertFalse(
            hold.isEnabled,
            "the command is below the fold at AX5; the hold must not be armed")
        XCTAssertTrue(
            app.staticTexts[Self.readGateReason].firstMatch.exists,
            "a disabled control states its reason as visible text")
    }

    /// The transition the screenshots never reached: scroll, and the gate opens.
    ///
    /// HIGH asks for two things and says so in order — first the command, then
    /// the class the command was given — because a face is about to authorise
    /// both. The intermediate sentence is the proof that the two halves are
    /// measured separately rather than one implying the other.
    func testHighCardArmsAfterScrollingToTheCommandAtAX5() {
        let app = launch(size: .ax5)
        openDeck(app)

        let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
        XCTAssertTrue(hold.waitForExistence(timeout: 25))
        XCTAssertFalse(hold.isEnabled)
        XCTAssertTrue(app.staticTexts[Self.readGateReason].exists)

        scrollUntil(app, "the command clears the fold") {
            app.staticTexts[Self.readGateHighReason].exists
                || app.buttons.matching(identifier: "Hold to allow").firstMatch.isEnabled
        }
        scrollUntil(app, "the read gate arms") {
            app.buttons.matching(identifier: "Hold to allow").firstMatch.isEnabled
        }

        attach(app, "gate-high-ax5-armed")
        XCTAssertFalse(app.staticTexts[Self.readGateReason].exists)
        XCTAssertFalse(app.staticTexts[Self.readGateHighReason].exists)
    }

    /// At a reading size the whole command block fits above the action bar, so
    /// the gate is satisfied by the first frame. The positive control: a gate
    /// that never opens is as broken as one that never shuts.
    func testHighCardIsArmedWhenTheWholeBlockFitsAtMedium() {
        let app = launch(size: .medium)
        openDeck(app)

        let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
        XCTAssertTrue(hold.waitForExistence(timeout: 25))
        XCTAssertTrue(hold.isEnabled, "the command and its class are both on screen")
        XCTAssertFalse(app.staticTexts[Self.readGateReason].exists)
    }

    /// **MEDIUM is where this matters most.** There is no hold and no Face ID at
    /// MEDIUM, so the read gate is the only thing between one tap on a 52pt
    /// white button and a write to an arbitrary path outside the worktree —
    /// which is exactly what a MEDIUM card at AX5 showed, with thirteen
    /// characters of that path on screen.
    ///
    /// Deep-linked straight to the card: it opens with the command below the
    /// fold, and `Allow` must not be the enabled white primary it was in the
    /// render.
    func testMediumCardIsGatedWhenTheCommandIsBelowTheFoldAtAX5() {
        let app = launch(size: .ax5, openingCard: Self.mediumRequestID)

        let allow = allowButton(app, tool: "Write")
        XCTAssertTrue(allow.waitForExistence(timeout: 25), "the Write card opened")
        attach(app, "gate-medium-ax5-shut")
        XCTAssertFalse(allow.isEnabled, "one tap, no hold, no Face ID — this is the whole gate")
        XCTAssertTrue(
            app.staticTexts[Self.readGateReason].firstMatch.exists,
            "a disabled control states its reason as visible text")

        // …and it opens on the same scroll the HIGH card needs. MEDIUM asks for
        // the command and nothing more, so one arming is the whole gate.
        scrollUntil(app, "the read gate arms") {
            allowButton(app, tool: "Write").isEnabled
        }
        attach(app, "gate-medium-ax5-armed")
        XCTAssertFalse(
            app.staticTexts[Self.readGateHighReason].exists,
            "the second sentence belongs to HIGH; MEDIUM never shows it")
    }

    /// **A LOW card is gated too, and this is the case that says why.**
    ///
    /// `ReadGate.isOpen(at: .low)` returned `true` unconditionally, on the
    /// argument that a `Read` costing a scroll is a gate people learn to defeat.
    /// Measured at AX5, what that bought was `Allow this Read call` **enabled**,
    /// as the filled white `accent` primary at x=16 y=734.67 — the lowest,
    /// brightest control on the screen, in the thumb zone — with the mono block
    /// sliced horizontally through the descenders of `ap` and **13 of the 24
    /// characters** of `/Users/dev/app/README.md` on screen. No fade, no `…`, no
    /// `↳`: nothing anywhere on the card said the string continued.
    ///
    /// Risk decides friction, never visibility. The tier is why this card is one
    /// tap rather than a hold and a face; it is not a reason to answer something
    /// the product declined to show.
    func testLowCardIsGatedWhenTheCommandIsBelowTheFoldAtAX5() {
        let app = launch(size: .ax5, openingCard: Self.lowRequestID)

        let allow = allowButton(app, tool: "Read")
        XCTAssertTrue(allow.waitForExistence(timeout: 25), "the Read card opened")
        attach(app, "gate-low-ax5-shut")
        XCTAssertFalse(
            allow.isEnabled,
            "half a path is not a command, whatever tier it was classified at")
        XCTAssertTrue(
            app.staticTexts[Self.readGateReason].firstMatch.exists,
            "a disabled control states its reason as visible text")

        scrollUntil(app, "the read gate arms") { allowButton(app, tool: "Read").isEnabled }
        attach(app, "gate-low-ax5-armed")
        XCTAssertFalse(
            app.staticTexts[Self.readGateHighReason].exists,
            "the second sentence belongs to HIGH; LOW asks for the command and stops")
    }

    /// **And it costs nothing at a reading size.** The whole point of the LOW
    /// exemption was that a `Read` should not need a scroll; it does not. The
    /// command block clears the action bar on the first frame, so the gate is
    /// open before the card is drawn and `Allow` takes the first tap.
    func testLowCardIsArmedOnSightAtMedium() {
        let app = launch(size: .medium, openingCard: Self.lowRequestID)

        let allow = allowButton(app, tool: "Read")
        XCTAssertTrue(allow.waitForExistence(timeout: 25))
        XCTAssertTrue(allow.isEnabled, "a LOW command that fits is answerable on sight")
        XCTAssertFalse(
            app.staticTexts[Self.readGateReason].exists,
            "and the gate says nothing, because it is not in the way")
    }

    /// The same card at a reading size, where the command does fit.
    func testMediumCardIsArmedWhenTheCommandFitsAtMedium() {
        let app = launch(size: .medium, openingCard: Self.mediumRequestID)

        let allow = allowButton(app, tool: "Write")
        XCTAssertTrue(allow.waitForExistence(timeout: 25))
        XCTAssertTrue(allow.isEnabled, "the command block is on screen, so the gate is armed")
        XCTAssertFalse(app.staticTexts[Self.readGateReason].exists)
    }

    /// A deep link opens the card it names, not the top of the queue — which is
    /// also what makes the two tests above assertions about MEDIUM rather than
    /// about whatever urgency ranking happened to put on top.
    func testDeepLinkOpensTheCardItNames() {
        let app = launch(openingCard: Self.mediumRequestID)
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 25),
            "a deep link lands in the Deck, past the fleet")
        XCTAssertTrue(app.staticTexts["Write"].firstMatch.waitForExistence(timeout: 20))
        XCTAssertFalse(
            allowButton(app, tool: "Bash").exists,
            "the named card is on top, not the riskiest one")
    }

    // MARK: Risk gates

    /// The queue, emptied in its documented order: HIGH first because urgency
    /// ranks the stack, then MEDIUM, then LOW; a hold and a biometric check at
    /// the top, a single tap at the bottom; and the pass ends on fleet-clear.
    func testTapToAdvanceThroughTheQueueToFleetClear() {
        let app = launch()
        openDeck(app)

        // 1 — HIGH: a hold, not a tap, plus the biometric check.
        let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
        XCTAssertTrue(hold.waitForExistence(timeout: 20), "a HIGH card swaps the tap for a hold")
        XCTAssertFalse(
            allowButton(app, tool: "Bash").exists, "a HIGH card must not offer a plain Allow")
        attach(app, "deck-high-card")
        hold.press(forDuration: 1.6)

        // 2 — MEDIUM: the command has to be seen first, and at this type size it
        // already has been.
        let allowWrite = allowButton(app, tool: "Write")
        XCTAssertTrue(allowWrite.waitForExistence(timeout: 20), "the stack advanced")
        XCTAssertTrue(allowWrite.isEnabled, "the command block is on screen, so the gate is armed")
        allowWrite.tap()

        // 3 — LOW: a single tap. The gate applies here too, and at this type
        // size the command fits above the bar, so it opened on the first frame
        // and the reader never met it.
        let allowRead = allowButton(app, tool: "Read")
        XCTAssertTrue(allowRead.waitForExistence(timeout: 20))
        XCTAssertTrue(allowRead.isEnabled, "a LOW command that fits is a single tap")
        allowRead.tap()

        let clear = app.staticTexts.matching(identifier: "Fleet clear").firstMatch
        XCTAssertTrue(clear.waitForExistence(timeout: 25), "the queue ends on fleet clear")
        attach(app, "deck-fleet-clear")
    }

    /// **`THIS PASS` is past tense, so it stops when the pass does.**
    ///
    /// It was wired to the model's one-second tick. Four captures of a single
    /// `Fleet clear` read `28s` → `55s` → `56s` → `57s`, monotonic and rising
    /// (2,550 → 3,203 differing pixels in the stat's own crop): the queue
    /// emptied once, the pass took 28 seconds, and the screen kept counting.
    /// Left open on a table it would have claimed the pass took five minutes —
    /// on the line people actually screenshot, and the only number on
    /// that screen with no age beside it for a reader to falsify it against.
    ///
    /// Read twice, seconds apart, which is exactly how it was caught.
    func testThisPassStopsCountingWhenThePassEnds() {
        let app = launch()
        openDeck(app)

        app.buttons.matching(identifier: "Hold to allow").firstMatch.press(forDuration: 1.6)
        let allowWrite = allowButton(app, tool: "Write")
        XCTAssertTrue(allowWrite.waitForExistence(timeout: 20))
        allowWrite.tap()
        let allowRead = allowButton(app, tool: "Read")
        XCTAssertTrue(allowRead.waitForExistence(timeout: 20))
        allowRead.tap()

        let clear = app.staticTexts.matching(identifier: "Fleet clear").firstMatch
        XCTAssertTrue(clear.waitForExistence(timeout: 25))

        let stat = app.descendants(matching: .any).matching(
            NSPredicate(format: "label == 'This pass'")
        ).firstMatch
        XCTAssertTrue(stat.waitForExistence(timeout: 20), "the pass reports how long it took")

        let first = stat.value as? String ?? ""
        XCTAssertFalse(first.isEmpty, "and it reports a value, not a blank")
        attach(app, "this-pass-frozen-t0")

        // Long enough that a live clock could not have stayed still: the
        // captures that caught this were seconds apart and moved every time.
        Thread.sleep(forTimeInterval: 6)

        let second = stat.value as? String ?? ""
        attach(app, "this-pass-frozen-t6")
        XCTAssertEqual(
            second, first,
            "a completed measurement must not tick: read \(first), then \(second)")
    }

    /// The HIGH gate is the trust promise made physical: refuse the biometric
    /// check and nothing is sent.
    func testHighRiskApprovalIsRefusedWhenBiometricsFail() {
        let app = launch(biometrics: "deny")
        openDeck(app)

        // Urgency-ranked, so the HIGH card is already on top.
        let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
        XCTAssertTrue(hold.waitForExistence(timeout: 20))
        hold.press(forDuration: 1.6)

        let refusal = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Not approved'")
        ).firstMatch
        XCTAssertTrue(
            refusal.waitForExistence(timeout: 25),
            "a failed biometric check must say so, not silently do nothing")
        XCTAssertTrue(hold.exists, "and the card must still be unanswered")
        attach(app, "deck-biometric-refusal")
    }

    /// **A refused Face ID has to be readable, and it was 100% behind the bar.**
    ///
    /// The banner was the seventh child of the card's scroll content — a
    /// document the reader has already scrolled to the end of to reach the hold
    /// — so the instant it appeared it was drawn *underneath* the bar that had
    /// just refused them. Measured: banner box y=613.33 h=53.00 against an
    /// action bar whose top hairline is y=631.00, so 66.6% of the banner was
    /// hidden; and its **message** — the sentence, the only part that says what
    /// went wrong — at y=640.33 h=14.00, entirely behind it. What the reader saw
    /// was a 17.67pt amber sliver with half a category label in it, which is
    /// indistinguishable from a tap that did nothing.
    ///
    /// The assertion is geometric on purpose. "The element exists" was already
    /// true in the frame that shipped; existing is not the property that failed.
    func testRefusedFaceIDIsReadableAndNotBehindTheActionBar() {
        let app = launch(biometrics: "deny")
        openDeck(app)

        let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
        XCTAssertTrue(hold.waitForExistence(timeout: 20))
        hold.press(forDuration: 1.6)

        let refusal = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Not approved'")
        ).firstMatch
        XCTAssertTrue(refusal.waitForExistence(timeout: 25))
        attach(app, "face-id-banner-pinned")

        // The bar is drawn *over* the scroll view rather than shortening it, so
        // the edge that matters is the pinned control's own top — the same edge
        // the read gate tests against.
        let deny = app.buttons.matching(identifier: "Deny this Bash call").firstMatch
        XCTAssertTrue(deny.exists)
        XCTAssertTrue(
            refusal.frame.maxY <= deny.frame.minY,
            "the refusal's sentence must clear the action bar: "
                + "banner ends \(refusal.frame.maxY), bar starts \(deny.frame.minY)")
        XCTAssertTrue(refusal.isHittable, "and be on screen, not merely in the tree")
    }

    /// The daemon's own rule is shown, because a risk badge whose reason is
    /// invisible is a badge that gets ignored.
    func testMatchedPatternIsShownOnTheCard() {
        let app = launch()
        openDeck(app)
        let matched = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'git push --force'")
        ).firstMatch
        XCTAssertTrue(matched.waitForExistence(timeout: 20))
    }

    // MARK: Postponing

    /// Postponing is not deciding. The card goes to the back and stays pending,
    /// and the count does not move.
    func testPostponingKeepsTheCountAndTheCard() {
        let app = launch()
        openDeck(app)
        XCTAssertTrue(app.staticTexts["Bash"].firstMatch.waitForExistence(timeout: 20))

        postponeTopCard(app)
        XCTAssertTrue(
            app.staticTexts["Write"].firstMatch.waitForExistence(timeout: 20),
            "the next card came forward")

        let counter = app.descendants(matching: .any).matching(identifier: "deck-count")
            .firstMatch
        XCTAssertTrue(counter.waitForExistence(timeout: 20))
        XCTAssertTrue(
            counter.label.contains("3"), "postponing decides nothing, so nothing was cleared")
    }

    // MARK: The fleet row

    /// The 4-second glance. A fleet row has to say what the agent wants to run,
    /// or the screen can only report that triage is required without enabling
    /// any of it.
    func testFleetRowCarriesTheCommand() {
        let app = launch()
        let row = app.buttons["session-fx-1"]
        XCTAssertTrue(row.waitForExistence(timeout: 25))
        XCTAssertTrue(
            row.label.contains("git push --force origin main"),
            "the row must name the command: \(row.label)")
        XCTAssertTrue(
            row.label.lowercased().contains("risk high"),
            "and its class, or a blind reader cannot tell a Read from a force-push")
        XCTAssertFalse(row.label.contains("HELD FOR YOU"))
        attach(app, "fleet-row")
    }

    // MARK: Diff

    /// The diff surface, reached from the session it belongs to.
    func testDiffRendersFilesAndFoldedContext() {
        let app = launch()
        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 25))
        session.tap()

        let diff = app.buttons["Diff"]
        XCTAssertTrue(diff.waitForExistence(timeout: 20))
        diff.tap()

        XCTAssertTrue(
            app.navigationBars.element(boundBy: 0).waitForExistence(timeout: 25))
        let chip = app.buttons.matching(
            NSPredicate(format: "label CONTAINS 'Feature.swift'")
        ).firstMatch
        XCTAssertTrue(chip.waitForExistence(timeout: 25), "file chips name what changed")
        attach(app, "diff")
    }

    // MARK: Helpers

    private func openDeck(_ app: XCUIApplication) {
        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25), "the deck bar is the way in")
        bar.tap()
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 20), "the deck opened")
    }

    /// Sends the top card to the back of the queue.
    ///
    /// At accessibility sizes `Come back to this` leaves the pinned action bar
    /// and lands in the card's own footer — four stacked controls in the bar
    /// measured 440pt at AX5 and left the document a four-line sliver — so it
    /// has to be scrolled to. That scroll is on the card being *postponed*, and
    /// the card that comes forward gets a fresh gate.
    private func postponeTopCard(_ app: XCUIApplication) {
        let later = app.buttons.matching(identifier: "deck-later").firstMatch
        XCTAssertTrue(later.waitForExistence(timeout: 25))
        scrollUntil(app, "`Come back to this` is reachable") { later.isHittable }
        later.tap()
    }

    /// Scrolls the document until `condition` holds, or fails saying what it was
    /// waiting for. A test that gives up silently is worse than no test.
    private func scrollUntil(
        _ app: XCUIApplication, _ what: String, attempts: Int = 12,
        condition: () -> Bool
    ) {
        for _ in 0..<attempts {
            if condition() { return }
            dragUp(app)
        }
        XCTAssertTrue(condition(), "gave up scrolling before \(what)")
    }

    /// A controlled press-and-drag inside the readable strip.
    ///
    /// **Not `swipeUp()`.** Measured on the Deck at AX5: five consecutive
    /// `app.swipeUp()` calls moved the card's scroll view exactly zero points —
    /// the flick's end point lands in the navigation bar and the gesture is
    /// never delivered to the scroll view. A press, a drag between two points
    /// that are both inside the document, and a release scrolls every time.
    private func dragUp(_ app: XCUIApplication) {
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.45))
            .press(
                forDuration: 0.05,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.15)))
    }

    /// `firstMatch` throughout: SwiftUI's `safeAreaInset` renders the action bar
    /// into the hierarchy more than once, so an exact query is ambiguous even
    /// when only one is on screen.
    private func allowButton(_ app: XCUIApplication, tool: String) -> XCUIElement {
        app.buttons.matching(identifier: "Allow this \(tool) call").firstMatch
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
