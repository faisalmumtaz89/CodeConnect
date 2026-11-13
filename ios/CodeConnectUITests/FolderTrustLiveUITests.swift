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
        XCTAssertTrue(app.buttons["start-go"].waitForExistence(timeout: 20), "the Ready step with Start")
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

    /// Live re-render of the sheet-step question at whatever size the simulator
    /// is set to. No first message — so the Ready step's Start is always present
    /// (at the accessibility sizes a focused message pill replaces it) — then
    /// shoot the question with its round Back and both answers.
    func testRenderQuestion() throws {
        let app = try launch()
        reachReady(app, folder: "proj-a", message: nil)
        app.buttons["start-go"].tap()
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
        // Ensure a kept answer: trust proj-a if the question comes.
        reachReady(app, folder: "proj-a", message: "work on proj-a")
        app.buttons["start-go"].tap()
        if app.buttons["trust-yes"].waitForExistence(timeout: 20) { app.buttons["trust-yes"].tap() }
        backToProjects(app)

        app.buttons["Settings"].tap()
        let folders = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Folders'")).firstMatch
        XCTAssertTrue(folders.waitForExistence(timeout: 15), "the Folders row in Settings")
        folders.tap()
        let listed = app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'proj-a'")).firstMatch
        XCTAssertTrue(listed.waitForExistence(timeout: 20), "proj-a listed in Settings › Folders")
        shot(app, "4-folders-listed")
        let forget = app.buttons.matching(NSPredicate(format: "label == 'Forget'")).firstMatch
        XCTAssertTrue(forget.waitForExistence(timeout: 10), "a Forget control")
        forget.tap()
        // proj-a was the only kept answer, so its Forget (and its row) leaves the
        // pane. ('proj-a' still shows in the fleet's session rows behind Settings —
        // the stub runs that started — so this is scoped to the Forget control.)
        let forgotten = XCTNSPredicateExpectation(
            predicate: NSPredicate { _, _ in
                !app.buttons.matching(NSPredicate(format: "label == 'Forget'")).firstMatch.exists
            }, object: nil)
        XCTAssertEqual(XCTWaiter().wait(for: [forgotten], timeout: 15), .completed, "proj-a was forgotten (its Folders row is gone)")
        shot(app, "4-forgotten")
    }
}
