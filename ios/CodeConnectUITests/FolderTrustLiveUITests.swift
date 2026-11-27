import XCTest

/// **The folder-trust question against a real folder-trust ccd (protocol 2.34).**
///
/// Drives the phone against an isolated rig daemon built from 1c3d1d76, paired by
/// token. Proves the acceptance facts: a start in a never-seen folder asks (the
/// must-fail was a start that counted needs-trust as answered and waited for
/// first words forever); an answer starts the run and is not re-asked; Back from
/// the question returns to the form with the message and choices intact; Settings
/// › Folders lists the answers and Forget removes one.
///
///   xcodebuild test-without-building -xctestrun <path> \
///     -destination 'platform=iOS Simulator,id=<sim>' \
///     -only-testing:CodeConnectUITests/FolderTrustLiveUITests
/// with CC_HOST/CC_TOKEN injected into the runner's EnvironmentVariables.
final class FolderTrustLiveUITests: XCTestCase {

    private var host: String { ProcessInfo.processInfo.environment["CC_HOST"] ?? "" }
    private var token: String { ProcessInfo.processInfo.environment["CC_TOKEN"] ?? "" }

    override func setUp() { continueAfterFailure = false }

    private func launch() throws -> XCUIApplication {
        try XCTSkipIf(host.isEmpty || token.isEmpty, "CC_HOST/CC_TOKEN not provided")
        let app = XCUIApplication()
        app.launchArguments = ["-CC_HOST", host, "-CC_TOKEN", token, "-CC_RESET_CACHE", "YES"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Projects"].waitForExistence(timeout: 30), "reached the fleet (protocol-2 pairing accepted)")
        return app
    }

    private func shot(_ app: XCUIApplication, _ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }

    /// Back up the navigation stack until Projects is showing again.
    private func backToProjects(_ app: XCUIApplication) {
        for _ in 0..<5 {
            if app.navigationBars["Projects"].waitForExistence(timeout: 3) { return }
            let back = app.navigationBars.buttons.firstMatch
            if back.exists { back.tap() } else { break }
        }
        _ = app.navigationBars["Projects"].waitForExistence(timeout: 6)
    }

    private func messageField(_ app: XCUIApplication) -> XCUIElement {
        let tv = app.textViews["start-message"].firstMatch
        if tv.waitForExistence(timeout: 6) { return tv }
        return app.textFields["start-message"].firstMatch
    }

    /// Open the start sheet and reach the Ready step on a browsed folder under the
    /// rig home, typing `message` first when given. Leaves the sheet on Ready.
    private func reachReady(_ app: XCUIApplication, folder: String, message: String?) {
        app.buttons["start-entry"].tap()
        let computer = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-computer-'")).firstMatch
        XCTAssertTrue(computer.waitForExistence(timeout: 15), "a paired computer to start on")
        computer.tap()
        let browse = app.buttons["start-browse"]
        XCTAssertTrue(browse.waitForExistence(timeout: 15), "Browse folders")
        browse.tap()
        let row = app.buttons["start-folder-\(folder)"]
        XCTAssertTrue(row.waitForExistence(timeout: 15), "the folder \(folder) in the browser")
        row.tap()
        app.buttons["start-use-folder"].tap()
        let claude = app.buttons["start-agent-claude"]
        XCTAssertTrue(claude.waitForExistence(timeout: 15), "the agent step")
        // Availability settles a beat after the step appears (the computer finishes
        // probing the agent), so wait for Claude to enable rather than race it.
        let enabled = XCTNSPredicateExpectation(predicate: NSPredicate(format: "isEnabled == true"), object: claude)
        XCTAssertEqual(XCTWaiter().wait(for: [enabled], timeout: 25), .completed, "Claude becomes available on the rig")
        claude.tap()
        if let message {
            let field = messageField(app)
            if field.waitForExistence(timeout: 8) { field.tap(); field.typeText(message) }
        }
        // The Ready step is reached: its Start is the bar's start-go, or — at the
        // accessibility sizes where a focused message pill hides the bar — the
        // pill's own send (start-message-button).
        XCTAssertTrue(
            app.buttons["start-go"].waitForExistence(timeout: 20) || app.buttons["start-message-button"].exists,
            "the Ready step (Start bar, or the pill's send when a focused message hides it)")
    }

    /// The fleet header ("N runs · …", or "Nothing running") counts live sessions
    /// from the model and is always drawn — unlike the session rows, which lazy
    /// rendering leaves off-screen once the fleet holds more than a couple (seen: the
    /// header read "2 runs" while only one `session-` row was in the tree). So the
    /// count of started runs is read from the header, not by counting rows. -1 until
    /// the header is up.
    private func agentCount(_ app: XCUIApplication) -> Int {
        if app.staticTexts["Nothing running"].exists { return 0 }
        let h = app.staticTexts.matching(NSPredicate(format: "label MATCHES %@", "[0-9]+ session.*")).firstMatch
        guard h.exists else { return -1 }
        return Int(h.label.prefix(while: { $0.isNumber })) ?? -1
    }

    /// Wait for the fleet to finish loading (a session row realized, or none running),
    /// then its live-agent count from the header.
    private func settledAgentCount(_ app: XCUIApplication) -> Int {
        let loaded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'session-'")).firstMatch.exists
                || app.staticTexts["Nothing running"].exists
        }, object: nil)
        _ = XCTWaiter().wait(for: [loaded], timeout: 20)
        return self.agentCount(app)
    }

    /// Scenarios 1 + 2: a never-seen folder asks, an answer starts, no second ask.
    func test1_questionAsksAndAnswerStartsOnce() throws {
        let app = try launch()
        reachReady(app, folder: "proj-a", message: "work on proj-a")
        app.buttons["start-go"].tap()

        // 1. The must-fail: the question shows, not "Waiting for first words".
        let yes = app.buttons["trust-yes"]
        XCTAssertTrue(yes.waitForExistence(timeout: 25) && app.buttons["trust-no"].exists, "the folder-trust question shows")
        XCTAssertFalse(
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'Waiting for'")).firstMatch.exists,
            "must NOT be the forever-Waiting start screen")
        shot(app, "1-folder-question")

        // 2. Answer → the run starts (question gone), not re-asked.
        yes.tap()
        XCTAssertTrue(
            app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH 'Starting'")).firstMatch.waitForExistence(timeout: 25)
                || app.otherElements.matching(NSPredicate(format: "identifier BEGINSWITH 'session-'")).firstMatch.waitForExistence(timeout: 10),
            "the start was accepted (Starting…/live), not the question again")
        XCTAssertFalse(app.buttons["trust-yes"].exists, "no second question after answering")
        shot(app, "2-started")

        // No second ask: start again in the now-trusted folder → no question.
        backToProjects(app)
        reachReady(app, folder: "proj-a", message: "work on proj-a")
        app.buttons["start-go"].tap()
        XCTAssertFalse(app.buttons["trust-yes"].waitForExistence(timeout: 12), "a trusted folder is not asked again")
        shot(app, "2b-no-second-ask")
    }

    /// Live re-render of the sheet-step question at whatever size the simulator is
    /// set to. Starts with a first message (a message-less start is refused, so the
    /// question would never come), then starts via the control the size presents —
    /// the Ready bar's start-go, or, at the accessibility sizes where a focused
    /// message pill hides the bar, the pill's own send — and shoots the question
    /// with its round Back and both answers.
    func testRenderQuestion() throws {
        let app = try launch()
        // proj-e is this test's own folder — no other test trusts it, so it is
        // always never-seen here and the question shows regardless of suite order.
        reachReady(app, folder: "proj-e", message: "render the folder question")
        // Start via the control the size presents. At non-accessibility sizes the
        // Ready bar's start-go sends and the message pill's button is dictate, so
        // prefer start-go; only when it is hidden (the accessibility sizes where the
        // focused pill covers the bar) does the pill's own send start the run.
        if app.buttons["start-go"].exists {
            app.buttons["start-go"].tap()
        } else {
            app.buttons["start-message-button"].tap()
        }
        XCTAssertTrue(app.buttons["trust-yes"].waitForExistence(timeout: 25), "the folder question")
        XCTAssertTrue(app.buttons["start-back"].exists, "the sheet's round Back on the question")
        XCTAssertTrue(app.buttons["trust-no"].exists, "both answers present")
        shot(app, "render-question")
    }

    /// Scenario 3: Back from the question returns to the form with the message kept.
    func test2_backKeepsMessageAndChoices() throws {
        let app = try launch()
        let typed = "fix the checkout crash"
        reachReady(app, folder: "proj-b", message: typed)
        app.buttons["start-go"].tap()
        // The question is now a step of the start sheet: its Back is the sheet's
        // own round control (`start-back`), a pop to the Ready form behind it.
        XCTAssertTrue(app.buttons["trust-yes"].waitForExistence(timeout: 25), "the folder question")
        XCTAssertTrue(app.buttons["start-back"].exists, "the sheet's Back on the question")
        shot(app, "3-question-before-back")
        app.buttons["start-back"].tap()
        XCTAssertTrue(app.buttons["start-go"].waitForExistence(timeout: 20), "returned to the Ready form")
        let keptMessage =
            app.textViews.matching(NSPredicate(format: "value CONTAINS %@", typed)).firstMatch.exists
            || app.textFields.matching(NSPredicate(format: "value CONTAINS %@", typed)).firstMatch.exists
            || app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", typed)).firstMatch.exists
        XCTAssertTrue(keptMessage, "the typed first message survived Back")
        XCTAssertTrue(
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'proj-b'")).firstMatch.exists,
            "the chosen folder survived Back")
        shot(app, "3-reseeded-form")
    }

    /// Scenario 4: Settings › Folders lists the kept answers and Forget removes one.
    func test3_settingsListsAndForgets() throws {
        let app = try launch()
        // test3 owns proj-c — distinct from proj-a/proj-b — and trusts it itself, so
        // the kept answer this checks never depends on another test having trusted a
        // folder first. The rig is fresh per run, so proj-c starts untrusted: the
        // question comes and is answered; if a prior run left it trusted the guard
        // skips and it is already in the store.
        reachReady(app, folder: "proj-c", message: "work on proj-c")
        app.buttons["start-go"].tap()
        if app.buttons["trust-yes"].waitForExistence(timeout: 20) { app.buttons["trust-yes"].tap() }
        backToProjects(app)

        app.buttons["Settings"].tap()
        let folders = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Folders'")).firstMatch
        XCTAssertTrue(folders.waitForExistence(timeout: 15), "the Folders row in Settings")
        folders.tap()
        let listed = app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'proj-c'")).firstMatch
        XCTAssertTrue(listed.waitForExistence(timeout: 20), "proj-c listed in Settings › Folders")
        shot(app, "4-folders-listed")
        // Forget proj-c by its OWN row's control. Each row's Forget now names its
        // folder for VoiceOver ("Forget proj-c"), which is also how this reaches the
        // one row rather than a bare "Forget" shared by every row. That name is the
        // must-fail: on the old screen every Forget read the bare word and this query
        // found nothing.
        let forget = app.buttons["Forget proj-c"]
        XCTAssertTrue(forget.waitForExistence(timeout: 15), "proj-c's Forget names its folder (VoiceOver)")
        forget.tap()
        let gone = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in !app.buttons["Forget proj-c"].exists }, object: nil)
        XCTAssertEqual(XCTWaiter().wait(for: [gone], timeout: 15), .completed, "proj-c's row left after Forget")
        shot(app, "4-forgotten")
    }

    /// Two taps on an answer start ONE session, not two. The answer button carries
    /// no explicit double-tap guard (nor does the Ready bar's start-go); the sheet
    /// dismissing on the first answer is what keeps the second tap from starting a
    /// second run, and this holds it to that. test4 owns proj-d so the question is
    /// always asked here regardless of what another test trusted.
    func test4_doubleTapStartsOneSession() throws {
        let app = try launch()
        // Read the live-agent count from the fleet header (see agentCount): counting
        // session rows is unreliable once the fleet is non-empty. test4 owns proj-d so
        // the question is always asked here, whatever another test trusted.
        let before = settledAgentCount(app)
        reachReady(app, folder: "proj-d", message: "work on proj-d")
        app.buttons["start-go"].tap()
        let yes = app.buttons["trust-yes"]
        XCTAssertTrue(yes.waitForExistence(timeout: 25), "the folder-trust question")
        yes.doubleTap()
        backToProjects(app)
        // One new live agent: the header goes before → before+1. A double start would
        // make it before+2 (the throwaway two-starts control proves the guard catches it).
        let grew = XCTNSPredicateExpectation(
            predicate: NSPredicate { [self] _, _ in agentCount(app) == before + 1 }, object: nil)
        let r = XCTWaiter().wait(for: [grew], timeout: 45)
        XCTAssertEqual(r, .completed,
                       "the double-tap started exactly one session (header agents \(before) → \(agentCount(app)), expected \(before + 1))")
        shot(app, "4a-double-tap-one-session")
    }
}
