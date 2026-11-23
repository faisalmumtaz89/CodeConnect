import XCTest

/// A run moved to another account on the Mac, as the phone shows it, from the daemon's own recordings of the switch
/// (`-CC_SWITCH`, `SwitchFixtures`): a waiting or moved run stays in the live list with its reply box and reads one
/// quiet "Switched to" row, never "Run ended"; a move that could not start reads Ended with one warning row and keeps
/// its box; End run after a move ends the run, with no box; a moved run's settings line is read only; a move onto a
/// plan without the run's model says, in one quiet row, which model it is now on.
final class SwitchedRunUITests: XCTestCase {

    private static let claude = "01M8299ZGSRFFX29Z8KF3Q5Q2V"
    private static let codex = "01M8299ZJQXMP43MZH8JY9VYMD"
    private static let failed = "01M82B4K7A2XN1X22R98H2XB80"
    private static let ended = "01M82B4K7A2XN1X22R98H2XB7Z"
    private static let planLacksModel = "01M830N4NHCWHA55C31CE5QNXF"
    private static let withBook = "01M82X1ASERF7DPA32E16243H9"

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func open(_ recording: String, step: Int? = nil, session: String? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            ["-CC_SWITCH", recording, "-CC_BIOMETRICS", "allow"]
            + (step.map { ["-CC_SWITCH_STEP", "\($0)"] } ?? [])
            + (session.map { ["-CC_DEEPLINK", "codeconnect://session/\($0)"] } ?? [])
            + ["-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL"]
        app.launch()
        return app
    }

    private func texts(_ app: XCUIApplication, beginningWith prefix: String) -> XCUIElementQuery {
        app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", prefix))
    }

    /// How many rows draw a phrase: a row read as one and its title carry the same words, so a match inside another
    /// match is the same row.
    private func rows(_ app: XCUIApplication, beginningWith prefix: String) -> Int {
        let frames = texts(app, beginningWith: prefix).allElementsBoundByIndex.map(\.frame)
        return frames.enumerated().filter { index, frame in
            !frames.enumerated().contains { other, outer in other != index && outer != frame && outer.contains(frame) }
        }.count
    }

    private var replyBox: (XCUIApplication) -> Bool {
        { $0.textViews.firstMatch.exists || $0.textFields.firstMatch.exists }
    }

    /// The row's status words; "Ended" is the Ended band's.
    private func row(_ app: XCUIApplication, _ uid: String) -> XCUIElement {
        let row = app.buttons["session-\(uid)"]
        XCTAssertTrue(row.waitForExistence(timeout: 25), "the run's row")
        return row
    }

    func testWaitingAndMovedRunsStayInTheLiveList() {
        let waiting = open("switch-claude", step: 2)
        XCTAssertFalse(row(waiting, Self.claude).label.contains("Ended"), "waiting: live")
        waiting.terminate()
        let moved = open("switch-claude", step: 6)
        XCTAssertFalse(row(moved, Self.claude).label.contains("Ended"), "moved: live")
    }

    func testAMovedClaudeRunReadsSwitchedToAndKeepsItsReplyBox() {
        movedRun(open("switch-claude", step: 6, session: Self.claude))
    }

    func testAMovedCodexRunReadsSwitchedToAndKeepsItsReplyBox() {
        movedRun(open("switch-codex", step: 6, session: Self.codex))
    }

    private func movedRun(_ app: XCUIApplication, file: StaticString = #filePath, line: UInt = #line) {
        let switched = texts(app, beginningWith: "Switched to")
        XCTAssertTrue(switched.firstMatch.waitForExistence(timeout: 25), "the Switched to row", file: file, line: line)
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 1, "one row", file: file, line: line)
        XCTAssertFalse(texts(app, beginningWith: "Run ended").firstMatch.exists, "no Run ended", file: file, line: line)
        XCTAssertTrue(replyBox(app), "its reply box", file: file, line: line)
        XCTAssertTrue(
            app.staticTexts.matching(identifier: "session.composer.model").firstMatch.exists,
            "its model, read only", file: file, line: line)
        XCTAssertFalse(app.buttons["session.composer.settings"].exists, "no settings to change", file: file, line: line)
    }

    /// With the account book's listing (ccd's, recorded with the switch), the row names the account by its email.
    func testAMovedRunNamesTheAccountByItsEmailWhenTheBookKnowsIt() {
        let app = open("switch-with-listing-claude", session: Self.withBook)
        let row = texts(app, beginningWith: "Switched to b@example.test")
        XCTAssertTrue(row.firstMatch.waitForExistence(timeout: 25), "the row by its email")
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 1, "one row")
    }

    func testAMoveThatCouldNotStartReadsEndedWithOneWarningAndKeepsItsReplyBox() {
        let app = open("switch-failed-then-recovered-claude", step: 4, session: Self.failed)
        let warning = texts(app, beginningWith: "Couldn’t move to")
        XCTAssertTrue(warning.firstMatch.waitForExistence(timeout: 25), "the warning row")
        XCTAssertEqual(rows(app, beginningWith: "Couldn’t move to"), 1, "one warning row")
        XCTAssertTrue(replyBox(app), "its reply box: the next message starts it")
        app.navigationBars.buttons.firstMatch.tap()
        let ended = app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS 'ended'")).firstMatch
        XCTAssertTrue(ended.waitForExistence(timeout: 10), "on the fleet, it reads Ended")
    }

    func testAMoveOntoAPlanWithoutTheModelNamesTheModelItIsNowOn() {
        let app = open("relaunch-plan-default-codex", session: Self.planLacksModel)
        let row = texts(app, beginningWith: "Now on")
        XCTAssertTrue(row.firstMatch.waitForExistence(timeout: 25), "the model-changed row")
        XCTAssertEqual(rows(app, beginningWith: "Now on"), 1, "one row")
        XCTAssertTrue(row.firstMatch.label.contains("isn’t in"), row.firstMatch.label)
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 1, "the move")
        XCTAssertFalse(texts(app, beginningWith: "Run resumed").firstMatch.exists, "a switch's relaunch draws nothing")
        XCTAssertTrue(replyBox(app), "its reply box")
    }

    func testEndRunAfterAMoveEndsTheRunWithNoReplyBox() {
        let app = open("switch-then-end-claude", session: Self.ended)
        XCTAssertTrue(
            app.descendants(matching: .any).matching(identifier: "session-ended-line").firstMatch
                .waitForExistence(timeout: 25), "the ended line")
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 1, "the move")
        XCTAssertTrue(texts(app, beginningWith: "Run ended").firstMatch.exists, "then Run ended")
        XCTAssertFalse(replyBox(app), "no reply box")
    }
}
