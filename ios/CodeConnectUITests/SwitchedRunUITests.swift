import XCTest

/// A run moved to another account on the Mac, as the phone shows it, from the daemon's own recordings of a move
/// (`-CC_SWITCH`, `SwitchFixtures`): a run its message is moving stays in the live list exactly as before, with its
/// reply box and its settings, and draws no row for the move; a move that could not start reads Ended with one warning
/// row and keeps its box; End run on a run due to move ends it, with no box; a move onto a plan without the run's model
/// says, in one quiet row, which model it is now on.
final class SwitchedRunUITests: XCTestCase {

    private static let claude = "01M89G4WF7VPP746TMY6Y404JB"
    private static let codex = "01M89T5Z50QTD00VMHHYNBVR4C"
    private static let failed = "01M89T5Z4ZAC3JWB35FH06S29P"
    private static let ended = "01M89G4WF8XNPTVJRGNRVE69DY"
    private static let planLacksModel = "01M89T5Z51SXVEP19X4QRKVKFK"

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func open(_ recording: String, step: Int? = nil, session: String? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            ["-CC_SWITCH", recording]
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

    func testAMovingRunStaysInTheLiveList() {
        let moving = open("switch-claude", step: 2)
        XCTAssertFalse(row(moving, Self.claude).label.contains("Ended"), "moving: live")
        moving.terminate()
        let moved = open("switch-claude")
        XCTAssertFalse(row(moved, Self.claude).label.contains("Ended"), "moved: live")
    }

    func testAMovingClaudeRunDrawsNothingAndKeepsItsReplyBoxAndSettings() {
        movingRun(open("switch-claude", step: 2, session: Self.claude))
    }

    func testAMovingCodexRunDrawsNothingAndKeepsItsReplyBoxAndSettings() {
        movingRun(open("switch-codex", step: 2, session: Self.codex))
    }

    private func movingRun(_ app: XCUIApplication, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertTrue(
            app.buttons["session.composer.settings"].waitForExistence(timeout: 25), "its settings, to change",
            file: file, line: line)
        XCTAssertTrue(replyBox(app), "its reply box", file: file, line: line)
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 0, "no row for the move", file: file, line: line)
        XCTAssertFalse(texts(app, beginningWith: "Run ended").firstMatch.exists, "no Run ended", file: file, line: line)
    }

    /// With the account book's listing (ccd's, recorded with the move), the conversation still draws no row for it:
    /// the message that moved the run is its one row.
    func testAMovedRunDrawsNoRowWhenTheBookKnowsTheAccount() {
        let app = open("switch-with-listing-claude", session: Self.claude)
        XCTAssertTrue(texts(app, beginningWith: "on B now?").firstMatch.waitForExistence(timeout: 25), "the message")
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 0, "no row for the move")
    }

    func testAMoveThatCouldNotStartReadsEndedWithOneWarningAndKeepsItsReplyBox() {
        let app = open("switch-failed-then-recovered-claude", step: 6, session: Self.failed)
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
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 0, "no row for the move")
        XCTAssertFalse(texts(app, beginningWith: "Run resumed").firstMatch.exists, "a move's relaunch draws nothing")
        XCTAssertTrue(replyBox(app), "its reply box")
    }

    func testEndRunOnADueRunEndsTheRunWithNoReplyBox() {
        let app = open("switch-then-end-claude", session: Self.ended)
        XCTAssertTrue(
            app.descendants(matching: .any).matching(identifier: "session-ended-line").firstMatch
                .waitForExistence(timeout: 25), "the ended line")
        XCTAssertEqual(rows(app, beginningWith: "Switched to"), 0, "no move")
        XCTAssertTrue(texts(app, beginningWith: "Run ended").firstMatch.exists, "Run ended")
        XCTAssertFalse(replyBox(app), "no reply box")
    }
}
