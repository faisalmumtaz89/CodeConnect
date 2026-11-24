import XCTest

/// A list narrowed to one computer holds that computer's runs and nothing else: none of the other computer's cards,
/// and no "ended" pill for the runs the filter left out. The list's held cards are drawn from the narrowed rows, the
/// same rows as its summary.
final class FleetFilterUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private let second = "build-server-eu-west-1"

    /// A project's card, by the label its header starts with.
    private func card(_ name: String, in app: XCUIApplication) -> XCUIElement {
        app.descendants(matching: .any).matching(NSPredicate(format: "label BEGINSWITH %@", "\(name),")).firstMatch
    }

    func testANarrowedListHoldsOnlyItsComputersRuns() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "deck",
            "-CC_FIXTURE_SECOND_MACHINE", second, "-CC_FIXTURE_SECOND_LIVE", "YES",
            "-CC_FILTER", second,
        ]
        app.launch()

        let summary = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "on \(second)")).firstMatch
        XCTAssertTrue(summary.waitForExistence(timeout: 20), "the summary narrowed to the second computer")
        XCTAssertTrue(app.buttons["fleet-filter-clear"].exists, "the chip that shows every run again")
        XCTAssertTrue(card("ml-eval", in: app).exists, "the second computer's project")
        XCTAssertFalse(card("app-1", in: app).exists, "the first computer's project is not in the list")
        XCTAssertFalse(app.buttons["fleet-held-changes"].exists, "nothing is held back as ended")

        app.buttons["fleet-filter-clear"].tap()
        XCTAssertTrue(card("app-1", in: app).waitForExistence(timeout: 5), "every run again")
    }

    /// A computer with no runs, here one that is not answering: its empty line under the chip, and still none of the
    /// other computer's cards.
    func testANarrowedListOfNoRunsShowsNoOtherCards() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "deck",
            "-CC_FIXTURE_SECOND_MACHINE", second, "-CC_FILTER", second,
        ]
        app.launch()

        let empty = app.staticTexts["Nothing running on \(second)"]
        XCTAssertTrue(empty.waitForExistence(timeout: 20), "the narrowed list's empty line")
        XCTAssertTrue(app.buttons["fleet-filter-clear"].exists, "the chip that shows every run again")
        XCTAssertFalse(card("app-1", in: app).exists, "the first computer's project is not in the list")
        XCTAssertFalse(app.buttons["fleet-held-changes"].exists, "nothing is held back as ended")
    }
}
