import XCTest

/// **A send whose answer is lost on a link that went silent** (B18/B19), against a
/// real `ccd` behind a proxy that forwards the phone's send and then drops everything
/// both ways while keeping the connection open: what a vanished network leaves.
/// The daemon types the message; the phone must notice the dead link, reconnect, and
/// learn the send landed, so the composer empties without the person sending again.
/// Needs a scratch `ccd` with one Claude session, and the proxy in front of it:
///
///   ios/scripts/halfopen_proxy.py <proxy port> <ccd port> <log>
///   TEST_RUNNER_CC_HOST=127.0.0.1:<proxy port> TEST_RUNNER_CC_TOKEN=<token> \
///     xcodebuild test … -only-testing:CodeConnectUITests/HalfOpenSendUITests
///
/// The proxy goes silent once, on the first send it sees; restart it between runs.
final class HalfOpenSendUITests: XCTestCase {

    private var host: String { ProcessInfo.processInfo.environment["CC_HOST"] ?? "" }
    private var token: String { ProcessInfo.processInfo.environment["CC_TOKEN"] ?? "" }

    override func setUp() { continueAfterFailure = false }

    func testASendWhoseAnswerIsLostStillClearsTheComposer() throws {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()

        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40), "the scratch session is listed")
        session.tap()

        let field = app.textViews.firstMatch.exists
            ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 20))
        field.tap()
        field.typeText("B18 probe: a message whose answer the network loses")
        let send = app.buttons["Send"]
        XCTAssertTrue(send.waitForExistence(timeout: 5) && send.isEnabled)
        send.tap()
        let sentAt = Date()
        print("B18 SENT at \(sentAt)")

        var cleared: TimeInterval?
        while Date().timeIntervalSince(sentAt) < 75 {
            let shown = (field.value as? String) ?? ""
            let elapsed = Date().timeIntervalSince(sentAt)
            print(String(format: "B18 t=%5.1fs composer=%@", elapsed, shown))
            if shown.isEmpty || shown == "Ask anything" {
                cleared = elapsed
                break
            }
            if Int(elapsed) % 25 < 5 { attach(app, String(format: "b18-%02.0fs", elapsed)) }
            Thread.sleep(forTimeInterval: 5)
        }
        attach(app, "b18-end")
        XCTAssertNotNil(
            cleared,
            "75 s after the send the composer still shows what the computer already typed: "
                + "\((field.value as? String) ?? "nil")")
    }

    /// **The same, with the person on the terminal while the link comes back.** The
    /// reconnect is noticed wherever the session screen is, so returning to the
    /// timeline finds the box already empty.
    func testALostAnswerClearsTheComposerAfterAReconnectSeenFromTheTerminal() throws {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()

        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40), "the scratch session is listed")
        session.tap()
        let field = app.textViews.firstMatch.exists
            ? app.textViews.firstMatch : app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 20))
        field.tap()
        field.typeText("B18 probe: sent, then the terminal while the link is down")
        app.buttons["Send"].tap()
        let sentAt = Date()
        // The picker stands aside while the composer has focus; a tap on the
        // timeline's upper background gives focus up (the timeline by name: with the
        // keyboard up, a bare coordinate can land on the keyboard's own bar).
        app.collectionViews["session-timeline"].coordinate(
            withNormalizedOffset: CGVector(dx: 0.5, dy: 0.3)
        ).tap()
        app.buttons["session-more"].tap()
        let terminal = app.buttons["Terminal"]
        XCTAssertTrue(terminal.waitForExistence(timeout: 10))
        terminal.tap()
        print("B18 TERMINAL shown at t=\(Date().timeIntervalSince(sentAt))s")
        // Past the 30 s silence deadline and the redial, on the terminal.
        Thread.sleep(forTimeInterval: max(0, 45 - Date().timeIntervalSince(sentAt)))
        app.buttons["session-more"].tap()
        app.buttons["Timeline"].tap()
        let cleared = (0..<20).contains { _ in
            let shown = (field.value as? String) ?? ""
            if shown.isEmpty || shown == "Ask anything" { return true }
            Thread.sleep(forTimeInterval: 0.5)
            return false
        }
        attach(app, "b18-terminal-end")
        XCTAssertTrue(
            cleared,
            "back on the timeline the composer still shows what the computer already typed: "
                + "\((field.value as? String) ?? "nil")")
    }

    /// **A slow answer is not a dead link.** The daemon answers a link's requests one
    /// at a time, so a diff it takes 18 s to read holds every pong back for that
    /// long. The rig's `git` sleeps 18 s; the link must stay up and the diff arrive.
    func testASlowAnswerKeepsTheLink() throws {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()

        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40), "the scratch session is listed")
        session.tap()
        let more = app.buttons["session-more"]
        XCTAssertTrue(more.waitForExistence(timeout: 20), "the session bar's menu")
        more.tap()
        let diff = app.buttons.matching(NSPredicate(format: "label == 'Changes'")).firstMatch
        XCTAssertTrue(diff.waitForExistence(timeout: 10), "the menu offers the changes")
        diff.tap()
        let asked = Date()
        print("B19 SLOW asked at \(asked)")
        let answered = app.staticTexts.matching(
            NSPredicate(
                format: "label CONTAINS 'Captured on the computer' OR label CONTAINS 'not a git'"
                    + " OR label CONTAINS 'No diff' OR label CONTAINS 'No uncommitted changes'"
                    + " OR label CONTAINS 'could not read the diff'")
        ).firstMatch
        XCTAssertTrue(answered.waitForExistence(timeout: 40), "the slow diff's answer arrived")
        print(String(format: "B19 SLOW answered after %.1fs: %@", Date().timeIntervalSince(asked), answered.label))
        // Past the silence deadline, so a link wrongly dropped would have shown it.
        Thread.sleep(forTimeInterval: 20)
        attach(app, "b19-slow-end")
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
