import XCTest

/// The tail-following contract, proven on the running app.
///
/// Two behaviours only a runtime can settle — both shipped and were reported
/// from a device:
/// - Scrolling back to the bottom by hand must clear the "Latest" pill without
///   tapping it. The pill is a statement about where the reader is, not a mode
///   only its own tap can leave.
/// - "Show less" on a message taller than the screen must bring the collapsed
///   row back under the eyes, not leave the viewport stranded in the blank the
///   collapse just made.
///
/// **Scrolling here is coordinate drags on a cached point, never element
/// swipes, and queries wait for the motion to settle.** Every `swipeUp` and
/// every `exists` poll takes an accessibility snapshot, and a snapshot needs a
/// quiescent main thread — over a screen holding an expanded, selectable,
/// multi-segment message, snapshotting *during* scroll deceleration starves:
/// measured as "main thread busy for 30.0s" on every polling variant of this
/// file, in builds with all follow logic removed. Cached coordinates take no
/// snapshots at all.
final class SessionFollowUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launchSession() -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
            "-CC_DEEPLINK", "codeconnect://session/fx-5",
        ]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        return app
    }

    /// A drag between two cached points in the timeline's own frame, with a
    /// hold before release so the scroll lands dead rather than decelerating
    /// into the next query.
    ///
    /// **In the trailing margin, above the compose bar.** Three claimed
    /// regions bracket the safe surface, and the probe caught each one
    /// freezing the scroll: a press on the expanded message starts a
    /// text-*selection* drag; the left ~13pt belong to the navigation
    /// stack's edge-pop gesture; and the scroll view's frame runs on behind
    /// the compose bar, so a press in its bottom fifth lands on the text
    /// field. The trailing padding above dy 0.65 is scrollable surface that
    /// nothing else wants.
    private func drag(_ timeline: XCUIElement, fromY: CGFloat, toY: CGFloat) {
        let start = timeline.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: fromY))
        let end = timeline.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: toY))
        start.press(
            forDuration: 0.05, thenDragTo: end,
            withVelocity: .default, thenHoldForDuration: 0.25)
    }

    /// Bring "Show more" on screen and tap it — really on screen. The
    /// timeline opens at its tail with the long message a screen and a half
    /// above; the button *exists* to a query long before it is under the
    /// glass (an accessibility snapshot materializes the whole lazy stack),
    /// and a tap at that stale frame lands on whatever is actually there.
    private func expandTheLongMessage(_ app: XCUIApplication, _ timeline: XCUIElement) {
        let showMore = app.buttons["Show more"]
        XCTAssertTrue(showMore.waitForExistence(timeout: 10))
        let window = app.windows.firstMatch.frame
        var attempts = 0
        while attempts < 8 {
            if showMore.isHittable && showMore.frame.minY >= window.minY
                && showMore.frame.maxY <= window.maxY
            {
                break
            }
            drag(timeline, fromY: 0.25, toY: 0.7)
            attempts += 1
        }
        XCTAssertTrue(showMore.isHittable, "the collapsed message must be reachable")
        showMore.tap()
    }

    func testScrollingBackToTheBottomClearsTheLatestPillWithoutATap() {
        let app = launchSession()
        let timeline = app.scrollViews["session-timeline"]
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        // Expand the long message so the timeline is decisively taller than
        // the screen — the only geometry in which the tail can be left.
        expandTheLongMessage(app, timeline)

        let pill = app.buttons["Jump to the latest event"]

        // Leave the tail: a dead-stop drag upward through the history. (The
        // expansion already put the tail well below the viewport.)
        drag(timeline, fromY: 0.3, toY: 0.8)
        XCTAssertTrue(
            waitUntil(timeout: 5) { pill.exists && pill.isHittable },
            "leaving the tail must offer the way back")

        // Return by hand — never tapping the pill. All the drags first, then
        // one settled assertion: a snapshot query between drags asks for
        // main-thread quiescence while the scroll machinery is still warm
        // over a deep timeline, and that starved at 30s a query. Eight drags
        // out-travel the whole scrollable range, and extras rubber-band
        // harmlessly at the bottom.
        for _ in 0..<8 {
            drag(timeline, fromY: 0.65, toY: 0.15)
        }
        XCTAssertTrue(
            waitUntil(timeout: 6) { !(pill.exists && pill.isHittable) },
            "reaching the bottom by hand must clear the pill; a pill only its "
                + "own tap can dismiss is a mode, not a statement of place")
    }

    func testShowLessBringsTheCollapsedMessageBackUnderTheEyes() {
        let app = launchSession()
        let timeline = app.scrollViews["session-timeline"]
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        expandTheLongMessage(app, timeline)

        // Read down to the control the way a person does: dead-stop drags,
        // stopping when a settled frame shows "Show less" hittable — mid-
        // content, the position where a collapse strands the viewport.
        //
        // **`isHittable` alone is not trusted.** Straight after the expansion
        // relayout, the accessibility snapshot can report a below-the-fold
        // element hittable at a content-space frame — measured: the "tap" on
        // that frame landed on nothing and the message never collapsed. A
        // frame is believed only when it also lies inside the window.
        let showLess = app.buttons["Show less"]
        let window = app.windows.firstMatch.frame
        func settledOnScreen(_ element: XCUIElement) -> Bool {
            element.exists && element.isHittable
                && element.frame.minY >= window.minY
                && element.frame.maxY <= window.maxY
        }
        var attempts = 0
        while attempts < 16 {
            if settledOnScreen(showLess) { break }
            drag(timeline, fromY: 0.65, toY: 0.1)
            attempts += 1
        }
        XCTAssertTrue(
            settledOnScreen(showLess),
            "the fixture's expanded message must be reachable by scrolling")
        showLess.tap()

        // The collapsed row — recognisable by its own "Show more" — must come
        // back **under the reader's eyes**: hittable, and in the upper half
        // of the screen. Position is the contract, not mere visibility — when
        // the scroll view clamps a shrunken timeline on its own, the row ends
        // up pinned at the screen's foot, which is exactly the disorientation
        // the re-anchor exists to prevent.
        let showMore = app.buttons["Show more"]
        XCTAssertTrue(
            waitUntil(timeout: 4) {
                showMore.exists && showMore.isHittable
                    && showMore.frame.midY < timeline.frame.midY
            },
            "collapse must bring the message's opening back under the eyes, "
                + "not leave it wherever the clamp dropped it")
    }

    /// **Reported from a device, and the case the test above does not
    /// cover.** That one stops as soon as "Show less" is on screen — part
    /// way down the expanded message. A reader who scrolls to the *very
    /// bottom* first collapses from a different place: the offset the
    /// collapse strands is past the end of the shrunken content, and the
    /// lazy stack has nothing loaded there to re-anchor to.
    func testShowLessFromTheVeryBottomDoesNotStrandTheReaderInBlank() {
        let app = launchSession()
        let timeline = app.scrollViews["session-timeline"]
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))
        expandTheLongMessage(app, timeline)

        // All the way down, the way the report describes it.
        for _ in 0..<10 {
            drag(timeline, fromY: 0.65, toY: 0.15)
        }
        let showLess = app.buttons["Show less"]
        let window = app.windows.firstMatch.frame
        var attempts = 0
        while attempts < 10 {
            if showLess.exists && showLess.isHittable && showLess.frame.minY >= window.minY
                && showLess.frame.maxY <= window.maxY
            {
                break
            }
            drag(timeline, fromY: 0.25, toY: 0.7)
            attempts += 1
        }
        XCTAssertTrue(showLess.isHittable, "the control must be reachable from the bottom")
        showLess.tap()

        // The contract is that *something* of the timeline is under the
        // reader's eyes. A blank viewport is the failure being reproduced.
        XCTAssertTrue(
            waitUntil(timeout: 6) {
                let more = app.buttons["Show more"]
                return more.exists && more.isHittable
                    && more.frame.maxY > timeline.frame.minY
                    && more.frame.minY < timeline.frame.maxY
            },
            "collapsing from the bottom must leave the timeline on screen, "
                + "not a viewport stranded past the end of the content")
    }

    /// **Reported from a device.** After a send lands, the composer must be
    /// empty. The send button turns back into the mic — which is read off
    /// the bound string — so the string *is* cleared; what stays behind is
    /// the keyboard's own uncommitted text, and a field still showing the
    /// message that was just sent invites sending it twice.
    func testTheComposerIsEmptyAfterASendLands() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
            "-cc.debug.sendText", "sent",
            "-CC_DEEPLINK", "codeconnect://session/fx-5",
        ]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))

        let field = app.textViews.firstMatch.exists
            ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 20))
        field.tap()
        // A misspelling on purpose: it is what leaves an autocorrection
        // session open on the field, which is the state the report was
        // taken in and the state a clear has to survive.
        field.typeText("Tell me abput this project")

        let send = app.buttons["Send"]
        XCTAssertTrue(send.waitForExistence(timeout: 5))
        try? XCTSkipUnless(send.isEnabled, "send is disabled — link or capability says so")
        send.tap()

        XCTAssertTrue(
            waitUntil(timeout: 8) {
                let shown = (field.value as? String) ?? ""
                return shown.isEmpty || shown == "Say something to this agent"
            },
            "the composer still shows the message it just sent: "
                + "\((field.value as? String) ?? "nil")")
    }

    private func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return condition()
    }
}
