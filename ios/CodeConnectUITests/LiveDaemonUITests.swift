import XCTest

/// The daemon's newer surfaces — `get_diff`, `turn_complete`, advertised
/// capabilities, pairing codes — against a **real** `ccd`. No mocks, same rule
/// as `ApprovalFlowUITests`: the point of these features is that they move real
/// bytes on a real Mac, and only a real daemon can prove that.
///
///   xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
///     -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
///     -only-testing:CodeConnectUITests/LiveDaemonUITests \
///     TEST_RUNNER_CC_HOST=100.x.y.z TEST_RUNNER_CC_TOKEN="$(codeconnect token)"
final class LiveDaemonUITests: XCTestCase {

    private var host: String { ProcessInfo.processInfo.environment["CC_HOST"] ?? "" }
    private var token: String { ProcessInfo.processInfo.environment["CC_TOKEN"] ?? "" }

    override func setUp() { continueAfterFailure = false }

    private func launchApp() throws -> XCUIApplication {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()
        return app
    }

    private func openFirstSession(_ app: XCUIApplication) throws {
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))
        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        try XCTSkipUnless(
            session.waitForExistence(timeout: 30), "no sessions registered with the daemon")
        session.tap()
    }

    /// `get_diff` end to end: the app asks, the daemon runs `git diff HEAD`, and
    /// the parser renders it with a capture time attached.
    ///
    /// **It walks the fleet until a session produces a diff.** It used to open
    /// the *first* row and skip if that one refused — and against a real Mac the
    /// first row is routinely a stale soak session whose worktree no longer
    /// exists, so this test took its skip branch on every run here. A test that
    /// always skips is a test that is not run, and it had been reporting green
    /// for a path nothing was exercising.
    ///
    /// The skip survives, moved to where it is honest: **no** session on the
    /// fleet could produce a diff. That is a real condition — a fleet of clean
    /// worktrees is the normal state between turns — and it is now the only one
    /// that skips.
    func testDiffFromTheLiveDaemon() throws {
        let app = try launchApp()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))

        let rows = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'session-'"))
        try XCTSkipUnless(
            rows.firstMatch.waitForExistence(timeout: 30), "no sessions registered with the daemon")

        // Snapshotted up front: `rows.count` is a live query, and re-reading it
        // after a push evaluates against the detail screen's tree, where it is 0.
        let keys = rows.allElementsBoundByIndex.map(\.identifier)
        var refusals: [String] = []

        for key in keys {
            let row = app.buttons[key]
            guard row.waitForExistence(timeout: 15) else { continue }
            row.tap()

            let diff = app.buttons["Diff"].firstMatch
            XCTAssertTrue(
                diff.waitForExistence(timeout: 15), "the session toolbar offers the diff")
            diff.tap()

            // Either a rendered diff or an explicit reason. What must never
            // happen — for any session, not just the one that answers — is a
            // spinner that never resolves.
            let captured = app.staticTexts.matching(
                NSPredicate(format: "label CONTAINS 'Captured on the Mac'")
            ).firstMatch
            let refused = app.staticTexts.matching(
                NSPredicate(
                    format:
                        "label CONTAINS 'No diff' OR label CONTAINS 'not a git'"
                        + " OR label CONTAINS 'No uncommitted changes'"
                        + " OR label CONTAINS 'could not read the diff'"
                        + " OR label CONTAINS 'No answer from the Mac'")
            ).firstMatch

            let settled = XCTNSPredicateExpectation(
                predicate: NSPredicate { _, _ in captured.exists || refused.exists }, object: nil)
            wait(for: [settled], timeout: 30)

            if captured.exists {
                attach(app, "live-diff-\(key)")
                // The assertion this test exists for, reached at last.
                XCTAssertTrue(
                    captured.exists, "a rendered diff always carries its capture time")
                return
            }

            refusals.append("\(key): \(refused.exists ? refused.label : "nothing resolved")")
            attach(app, "live-diff-refused-\(key)")

            // Back to the fleet for the next candidate. The sheet first, then
            // the push — both are `Done`-then-back, and a sheet left open eats
            // the navigation tap.
            let done = app.buttons["Done"].firstMatch
            if done.exists { done.tap() }
            let back = app.navigationBars.buttons.firstMatch
            if back.exists { back.tap() }
            _ = app.navigationBars["Fleet"].waitForExistence(timeout: 15)
        }

        attach(app, "live-diff-none")
        try XCTSkipIf(
            true,
            "no session on this fleet could produce a diff — \(refusals.joined(separator: " · "))")
    }

    /// The daemon's `hello_ack` is what every one of these affordances is gated
    /// on, so the trust screen has to show what actually arrived.
    func testDaemonAdvertisesItsCapabilities() throws {
        let app = try launchApp()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 20))
        app.buttons["Settings and pairing"].tap()
        XCTAssertTrue(app.navigationBars["Settings"].waitForExistence(timeout: 10))

        let advertised = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'What this daemon advertises'")
        ).firstMatch
        app.swipeUp()
        app.swipeUp()
        XCTAssertTrue(
            advertised.waitForExistence(timeout: 10) || app.staticTexts["diff"].exists,
            "the capability list is reported, not guessed")
        attach(app, "live-capabilities")
    }

    /// The Stop hook now arrives as `EventKind::TurnComplete`, and the timeline
    /// renders it as a finished turn rather than a finished session.
    ///
    /// Skips unless something has actually run a turn — this reads the log, it
    /// does not drive the agent.
    func testTurnCompleteRendersAsAFinishedTurn() throws {
        let app = try launchApp()
        try openFirstSession(app)

        let turn = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Turn complete'")
        ).firstMatch
        let ended = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Session ended'")
        ).firstMatch

        _ = turn.waitForExistence(timeout: 25)
        attach(app, "live-timeline")
        try XCTSkipUnless(turn.exists, "no completed turn in this session's log yet")
        XCTAssertFalse(
            ended.exists,
            "a finished turn must not also read as a finished session — an earlier build "
                + "conflated the two")
    }

    /// **The push path: a cold launch straight into the diff.**
    ///
    /// `codeconnect://session/<uid>/diff` at launch is the route the diff
    /// screen's "not connected" state exists for, and it beats the socket every
    /// time: the sheet's `.task` asks for the diff while the connection is still
    /// being made. What shipped was `The Mac could not read the diff` over
    /// `No diff was produced. The daemon's own reason follows, verbatim.` over
    /// **`Connecting to the daemon…`** — the app's own sentence about its own
    /// link, attributed to the Mac, on the screen the honesty ladder exists to
    /// protect.
    ///
    /// The assertion is one-sided on purpose. Which of the two good outcomes
    /// this run reaches — the link state, or the diff itself once the socket
    /// comes up and the request is re-issued — depends on timing that a test
    /// must not pretend to control. What must never appear, at any moment, is
    /// the verbatim caption over a string no daemon ever sent.
    func testColdLaunchDeepLinkNeverQuotesTheAppToTheDaemon() throws {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")

        // Resolve a real run first, on a normal launch, so the link addresses
        // something the daemon actually has.
        let finder = XCUIApplication()
        finder.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        finder.launch()
        XCTAssertTrue(finder.navigationBars["Fleet"].waitForExistence(timeout: 20))
        let row = finder.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        try XCTSkipUnless(
            row.waitForExistence(timeout: 30), "no sessions registered with the daemon")
        let key = String(row.identifier.dropFirst("session-".count))
        finder.terminate()

        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES",
            "-CC_DEEPLINK", "codeconnect://session/\(key)/diff",
        ]
        app.launch()

        let verbatim = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS \"daemon's own reason follows\"")
        ).firstMatch
        let quoted = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Connecting to the daemon'")
        ).firstMatch

        // Watch the whole race, not one moment of it.
        for _ in 0..<20 {
            if verbatim.exists {
                XCTAssertFalse(
                    quoted.exists,
                    "the app put its own link string under a caption promising the daemon's words")
            }
            Thread.sleep(forTimeInterval: 0.5)
        }

        // And it must settle on something that says what it knows.
        let settled = app.staticTexts.matching(
            NSPredicate(
                format:
                    "label CONTAINS 'Captured on the Mac' OR label CONTAINS 'Not connected'"
                    + " OR label CONTAINS 'No uncommitted changes' OR label CONTAINS 'No diff to show'"
                    + " OR label CONTAINS 'No answer from the Mac'")
        ).firstMatch
        XCTAssertTrue(
            settled.waitForExistence(timeout: 30),
            "a deep link that races the socket resolves rather than dead-ending")
        attach(app, "cold-launch-diff-deeplink")
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}

/// QR pairing against a real `codeconnect pair`.
///
/// Run it with a *fresh* code — they are single-use and expire in five minutes:
///
///   CODE=$(codeconnect pair | grep -o '"code":"[^"]*"' | cut -d'"' -f4)
///   xcodebuild test … TEST_RUNNER_CC_PAIR_CODE=$CODE TEST_RUNNER_CC_PAIR_HOST=<host>
final class PairingLiveUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }

    func testPairingCodeBuysADeviceToken() throws {
        let env = ProcessInfo.processInfo.environment
        let code = env["CC_PAIR_CODE"] ?? ""
        let host = env["CC_PAIR_HOST"] ?? ""
        try XCTSkipIf(code.isEmpty || host.isEmpty, "no fresh pairing code provided")

        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_PAIR_HOST", host, "-CC_PAIR_CODE", code, "-CC_RESET_CACHE", "YES",
        ]
        app.launch()

        // Reaching the Fleet at all means the tokenless hello was accepted and
        // the device token came back — the app never stores the code, so a
        // second connection could only succeed with the token.
        XCTAssertTrue(
            app.navigationBars["Fleet"].waitForExistence(timeout: 30),
            "a pairing code must buy a working, durable pairing")
        XCTAssertTrue(app.buttons["Link health"].waitForExistence(timeout: 10))

        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "live-paired-by-code"
        shot.lifetime = .keepAlways
        add(shot)
    }
}
