import XCTest

/// End run from the iPhone, against a **real** `ccd` hosting a run it started (a stand-in claude behind ccd's agent
/// host; no real model, no prompt). Decision 185's iPhone half: the session's "…" menu ends with End run, only on a live
/// run CodeConnect started; it asks once; the row reads Stopping… and the run then sits with the ended ones.
///
///   TEST_RUNNER_CC_HOST=127.0.0.1:<port> TEST_RUNNER_CC_TOKEN=<device token> \
///   TEST_RUNNER_CC_END_RUN=<session uid of a live run ccd started> \
///   xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
///     -destination 'platform=iOS Simulator,id=<sim>' -only-testing:CodeConnectUITests/EndRunLiveUITests
///
/// The run's stand-in should hold its exit for a few seconds after the stop, so Stopping… can be read before Ended.
/// Each step's screen is kept as an attachment (the owner's frames): the menu, the question, the row stopping.
final class EndRunLiveUITests: XCTestCase {

    private var env: [String: String] { ProcessInfo.processInfo.environment }

    override func setUp() { continueAfterFailure = false }

    private func launchApp() throws -> XCUIApplication {
        let host = env["CC_HOST"] ?? "", token = env["CC_TOKEN"] ?? ""
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Projects"].waitForExistence(timeout: 20))
        return app
    }

    private func keep(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }

    private func openRun(_ app: XCUIApplication, uid: String) {
        let row = app.buttons["session-\(uid)"]
        XCTAssertTrue(row.waitForExistence(timeout: 30), "the run's row in the fleet")
        row.tap()
        let more = app.buttons["session-more"]
        XCTAssertTrue(more.waitForExistence(timeout: 20), "the session bar's menu")
        more.tap()
    }

    func testEndingALiveRunCodeConnectStartedFromItsMenu() throws {
        let uid = env["CC_END_RUN"] ?? ""
        try XCTSkipIf(uid.isEmpty, "CC_END_RUN not provided")
        let app = try launchApp()
        openRun(app, uid: uid)

        let end = app.buttons["End run"]
        XCTAssertTrue(end.waitForExistence(timeout: 10), "the menu offers End run on a live run CodeConnect started")
        let others = ["Changes", "Link health", "Terminal", "Timeline"].map { app.buttons[$0] }.filter(\.exists)
        XCTAssertFalse(others.isEmpty, "the menu's other items are there")
        XCTAssertTrue(others.allSatisfy { $0.frame.minY < end.frame.minY }, "End run is the menu's last item")
        keep(app, "1 the menu, End run last")

        end.tap()
        XCTAssertTrue(
            app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'End “' AND label ENDSWITH '”?'")).firstMatch
                .waitForExistence(timeout: 10), "End run asks first, naming the run")
        XCTAssertTrue(
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'stops. The run moves to Ended.'"))
                .firstMatch.exists, "the question says what ending does")
        keep(app, "2 the question")
        app.buttons["endRun.confirm"].tap()

        app.navigationBars.buttons.element(boundBy: 0).tap()
        let row = app.buttons["session-\(uid)"]
        XCTAssertTrue(row.waitForExistence(timeout: 20), "back on the fleet")
        // The row is one element; its label carries the status's word, "Stopping" (the drawn line reads "Stopping…").
        XCTAssertTrue(wait(10) { row.label.contains("Stopping") }, "the row reads Stopping once the computer took the stop")
        keep(app, "3 the row, stopping")
        XCTAssertTrue(wait(30) { !row.exists || !row.label.contains("Stopping") }, "the run then ends: no longer Stopping")
    }

    /// Polls `condition` until it holds or `seconds` pass.
    private func wait(_ seconds: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return condition()
    }
}
