import XCTest

/// The splice, against a **real** `ccd` at protocol minor 2.
///
/// Everything here needs a daemon that has more than one run under one tmux
/// name, which is the state a Mac reaches on its own the moment a `cc-1` exits
/// and the next `cc claude` takes the name back:
///
///   cc claude                      # in some directory; note the uid from `cc ls`
///   tmux -L codeconnect kill-session -t cc-1
///   cc claude                      # same directory, same name, new uid
///
///   xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
///     -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
///     -only-testing:CodeConnectUITests/SessionIdentityLiveUITests \
///     TEST_RUNNER_CC_HOST=127.0.0.1 TEST_RUNNER_CC_TOKEN="$(cc token)"
final class SessionIdentityLiveUITests: XCTestCase {

    private var host: String { ProcessInfo.processInfo.environment["CC_HOST"] ?? "" }
    private var token: String { ProcessInfo.processInfo.environment["CC_TOKEN"] ?? "" }

    override func setUp() { continueAfterFailure = false }

    private func launchApp() throws -> XCUIApplication {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        // The cache is reset so this measures what the daemon says, not what a
        // previous run of the suite left on disk.
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 25))
        return app
    }

    /// Rows are identified by `session_uid`, not by the tmux name. This is the
    /// whole change in one assertion: on the build before it, every run called
    /// `cc-1` shared one row, one store and one timeline.
    func testFleetRowsAreIdentifiedByRunAndNotByName() throws {
        let app = try launchApp()
        let anyRow = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        try XCTSkipUnless(anyRow.waitForExistence(timeout: 30), "the daemon lists no sessions")

        // Crockford Base32, 26 symbols, no I/L/O/U — `protocol/src/uid.rs`.
        let uidRows = app.buttons.matching(
            NSPredicate(format: "identifier MATCHES 'session-[0-9A-HJKMNP-TV-Z]{26}'"))
        attach(app, "live-uid-fleet")
        XCTAssertGreaterThan(
            uidRows.count, 0,
            "a minor-2 daemon mints a uid per run and the app must key rows by it")
    }

    /// Two runs that shared a name are two rows, and each says which one it is.
    ///
    /// Skips rather than fails when the daemon has no reused name to show: the
    /// state is real but it has to be arranged (see the header), and a suite
    /// that fails for want of a fixture stops being run.
    func testTwoRunsOfOneNameRenderAsTwoDistinctSessions() throws {
        let app = try launchApp()
        let rows = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'session-'"))
        try XCTSkipUnless(rows.firstMatch.waitForExistence(timeout: 30), "no sessions")

        // The disambiguating tail is only printed where a name is shared, so its
        // presence *is* the evidence that the fleet holds two runs of one name.
        let shared = app.buttons.matching(NSPredicate(format: "label CONTAINS 'cc-1 · '"))
        attach(app, "live-two-runs-one-name")
        try XCTSkipUnless(
            shared.count > 0, "no tmux name is currently held by more than one run")
        XCTAssertGreaterThanOrEqual(
            shared.count, 2,
            "a shared name must produce two rows; one row would be the splice")

        let identifiers = Set(
            (0..<shared.count).map { shared.element(boundBy: $0).identifier })
        XCTAssertEqual(
            identifiers.count, shared.count,
            "two rows that share a name must not share an identity")
    }

    /// Opening one of them shows that run's timeline, and the app can still talk
    /// to the daemon about it — which it now does by uid.
    func testASharedNameOpensTheRunThatWasTapped() throws {
        let app = try launchApp()
        let shared = app.buttons.matching(NSPredicate(format: "label CONTAINS 'cc-1 · '"))
        try XCTSkipUnless(
            shared.firstMatch.waitForExistence(timeout: 30),
            "no tmux name is currently held by more than one run")

        let first = shared.element(boundBy: 0)
        let identifier = first.identifier
        first.tap()
        XCTAssertFalse(app.navigationBars["Fleet"].exists, "tapping a row pushes the detail stack")

        // The diff is a round trip that names the session on the wire; a request
        // scoped to a uid the daemon does not know would come back as an error
        // rather than as a diff or a note.
        app.buttons["Diff"].tap()
        let settled = app.staticTexts.matching(
            NSPredicate(
                format:
                    "label CONTAINS 'Captured on the Mac' OR label CONTAINS 'No diff' OR label CONTAINS 'not a git'"
            )
        ).firstMatch
        XCTAssertTrue(
            settled.waitForExistence(timeout: 30),
            "the daemon answered a request scoped to \(identifier)")
        attach(app, "live-run-scoped-diff")
        XCTAssertFalse(
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'unknown_session'"))
                .firstMatch.exists,
            "the uid the app sent must be one the daemon recognises")
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
