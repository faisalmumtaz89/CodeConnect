import XCTest

/// "Show all" on a tool's output at the tail keeps the reader where they were.
///
/// The output's top is above the screen when its "Show all" is tapped at the
/// tail, and the timeline's list keeps what is below a growing row in place:
/// it moved the offset down by everything the output grew, so the reader
/// landed on its last line and went on following. Opening is choosing to
/// read, so line 1 has to stay where it was, the "Latest" pill has to offer
/// the way back, and an event arriving afterwards is counted, not followed.
///
/// Scrolling is coordinate drags in the trailing margin, for the reasons
/// `SessionFollowUITests` gives.
final class SessionShowAllUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    /// The fixture holds its last event back this long, so the tap comes
    /// first and that event arrives while the reader is reading.
    private let lateEventMS = 30_000

    func testShowAllAtTheTailKeepsLineOneWhereItWas() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "longOutput", "-CC_BIOMETRICS", "allow",
            "-CC_DEEPLINK", "codeconnect://session/fx-5",
            "-CC_FIXTURE_LATE_EVENT_MS", "\(lateEventMS)",
        ]
        app.launch()
        let timeline = app.collectionViews["session-timeline"]
        XCTAssertTrue(timeline.waitForExistence(timeout: 20))

        // Open the command's output, then come back to the tail by hand.
        let row = app.buttons.matching(
            NSPredicate(format: "label CONTAINS %@", "cargo test soak")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the fixture's command is the tail row")
        row.tap()
        for _ in 0..<8 {
            drag(timeline, fromY: 0.65, toY: 0.15)
        }

        let showAll = app.buttons.matching(
            NSPredicate(format: "label CONTAINS[c] %@", "show all 60 lines")).firstMatch
        let lineOne = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS %@", "pass_01 ")).firstMatch
        let later = app.buttons.matching(
            NSPredicate(format: "label CONTAINS %@", "./soak/report.sh")).firstMatch
        XCTAssertTrue(
            waitUntil(timeout: 5) {
                showAll.exists && showAll.isHittable
                    && showAll.frame.maxY <= timeline.frame.maxY
            },
            "at the tail, the output's \"Show all\" is on screen")
        XCTAssertTrue(lineOne.exists, "the output's first line is laid out")
        let before = lineOne.frame.minY
        XCTAssertLessThan(
            before, timeline.frame.minY,
            "the output's top is above the screen, the geometry this test is about")
        XCTAssertFalse(later.exists, "the tap comes before the held-back event")

        showAll.tap()

        let pill = app.buttons["Jump to the latest event"]
        XCTAssertTrue(
            waitUntil(timeout: 5) { pill.exists && pill.isHittable },
            "opening every line is reading: the way back is offered")
        XCTAssertTrue(app.staticTexts["Latest"].exists, "nothing has arrived yet")
        XCTAssertTrue(lineOne.exists, "line 1 is still laid out")
        XCTAssertEqual(
            lineOne.frame.minY, before, accuracy: 1,
            "line 1 stays where the reader saw it, not carried off to the last line")

        XCTAssertTrue(
            waitUntil(timeout: Double(lateEventMS) / 1000 + 15) {
                app.staticTexts["1 new"].exists
            },
            "an event arriving while reading is counted, not followed")
        XCTAssertEqual(
            lineOne.frame.minY, before, accuracy: 1, "and the reader was not moved by it")
    }

    private func drag(_ timeline: XCUIElement, fromY: CGFloat, toY: CGFloat) {
        let start = timeline.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: fromY))
        let end = timeline.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: toY))
        start.press(
            forDuration: 0.05, thenDragTo: end,
            withVelocity: .default, thenHoldForDuration: 0.25)
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
