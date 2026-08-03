import XCTest

/// The keyboard's claim on the session screen, proven on the running app.
///
/// The rule: while the composer is focused, the chrome that is not part of
/// typing — the full identity block, the Timeline/Terminal picker — stands
/// aside so the conversation keeps the screen; drag or a background tap gives
/// the keyboard back. None of that is provable from source: focus, keyboard
/// animation and gesture arbitration are runtime facts.
final class SessionKeyboardUITests: XCTestCase {

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

    func testFocusCollapsesChromeAndBackgroundTapRestoresIt() {
        let app = launchSession()
        let terminalTab = app.buttons["Terminal"]
        XCTAssertTrue(terminalTab.waitForExistence(timeout: 20), "picker visible before focus")

        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.tap()

        // Focused: the picker steps aside and the keyboard is up.
        XCTAssertTrue(
            waitUntil(timeout: 5) { !terminalTab.exists },
            "the surface picker must stand aside while typing")
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 5))

        // A tap on the timeline's background resigns focus. Coordinates, not
        // an element — the point is that nothing interactive is there — and
        // placed in the scroll view's *upper* half: with the keyboard up, its
        // lower half is under the keyboard, and a tap there presses keys.
        // The app's own timeline, by name: `scrollViews.firstMatch` with the
        // keyboard up is the keyboard's input-assistant bar — also a scroll
        // view — and a coordinate tap on it presses keys. Measured.
        let scroll = app.scrollViews["session-timeline"]
        XCTAssertTrue(scroll.waitForExistence(timeout: 5))
        scroll.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.3)).tap()

        XCTAssertTrue(
            waitUntil(timeout: 5) { terminalTab.exists },
            "dismissing the keyboard must bring the chrome back")
        XCTAssertTrue(
            waitUntil(timeout: 5) { !app.keyboards.firstMatch.exists },
            "the background tap must dismiss the keyboard")
    }

    /// Expanding the agent's message renders its fenced code as a mono block —
    /// the copyable kind — rather than reflowing it into prose.
    func testShowMoreRevealsTheFencedCodeAsAMonoBlock() {
        let app = launchSession()
        let showMore = app.buttons["Show more"]
        XCTAssertTrue(showMore.waitForExistence(timeout: 20))
        showMore.tap()

        XCTAssertTrue(
            app.staticTexts["let seed = 0x5eed"].waitForExistence(timeout: 5),
            "the fence body must render, verbatim, in the expansion")
        XCTAssertFalse(
            app.staticTexts.matching(
                NSPredicate(format: "label CONTAINS '**'")
            ).firstMatch.exists,
            "no markdown artifact may survive rendering")
    }

    private func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        }
        return condition()
    }
}
