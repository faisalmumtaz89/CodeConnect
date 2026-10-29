import XCTest

/// End-to-end against **two real** `ccd`s that this phone has both paired with:
/// one fleet and one Deck made of both, and a removal reaching each machine for
/// its own runs. Each test does what a person does; what each daemon received is
/// checked on the daemons, by whoever staged them.
///
/// Opt-in, because it needs two daemons each running a `cc-1` (with uids
/// `CC_MULTI_UID_A` and `CC_MULTI_UID_B`), the simulator paired with both, and
/// for the approval test one Read approval held on each (`approve-on-A` on A,
/// `deny-on-B` on B):
///
///     TEST_RUNNER_CC_MULTI=1 TEST_RUNNER_CC_MULTI_UID_A=<uid> \
///     TEST_RUNNER_CC_MULTI_UID_B=<uid> \
///     xcodebuild test … -only-testing:CodeConnectUITests/MultiMachineLiveUITests/<test>
final class MultiMachineLiveUITests: XCTestCase {

    private var environment: [String: String] { ProcessInfo.processInfo.environment }
    private var uidA: String { environment["CC_MULTI_UID_A"] ?? "" }
    private var uidB: String { environment["CC_MULTI_UID_B"] ?? "" }

    override func setUp() {
        continueAfterFailure = false
    }

    private func launchApp() throws -> XCUIApplication {
        try XCTSkipUnless(
            environment["CC_MULTI"] == "1" && !uidA.isEmpty && !uidB.isEmpty,
            "needs two paired daemons: set TEST_RUNNER_CC_MULTI=1, TEST_RUNNER_CC_MULTI_UID_A "
                + "and TEST_RUNNER_CC_MULTI_UID_B (see this file's header)")
        let app = XCUIApplication()
        // The stored pairings, both of them: no pairing arguments.
        app.launchArguments = ["-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryM"]
        app.launch()
        return app
    }

    private func session(_ app: XCUIApplication, _ uid: String) -> XCUIElement {
        app.buttons["session-\(uid)"].firstMatch
    }

    /// Both machines' `cc-1` are in the one fleet.
    func testBothMachinesSessionsAreInOneFleet() throws {
        let app = try launchApp()
        XCTAssertTrue(session(app, uidB).waitForExistence(timeout: 45), "machine B's cc-1")
        XCTAssertTrue(session(app, uidA).waitForExistence(timeout: 15), "machine A's cc-1")
        attachScreenshot(app, name: "one-fleet")
    }

    /// One approval held on each machine: the bar and the Deck count both, and
    /// the Deck shows each machine's card.
    func testBothMachinesApprovalsAreInOneDeck() throws {
        let app = try launchApp()
        let review = app.buttons["deck-bar"].firstMatch
        XCTAssertTrue(review.waitForExistence(timeout: 45), "the bar counts the held approvals")
        attachScreenshot(app, name: "two-waiting")
        review.tap()
        XCTAssertTrue(app.navigationBars["Waiting for you"].waitForExistence(timeout: 15))
        let counter = app.descendants(matching: .any)["deck-count"].firstMatch
        XCTAssertTrue(counter.waitForExistence(timeout: 5))
        XCTAssertTrue(counter.label.hasPrefix("2"), "both machines' cards: \(counter.label)")
        let card = { (marker: String) in
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", marker)).firstMatch
        }
        XCTAssertTrue(
            card("approve-on-A").waitForExistence(timeout: 10)
                || card("deny-on-B").waitForExistence(timeout: 5), "a held card is on top")
        attachScreenshot(app, name: "deck")
    }

    /// Every ended run, removed together: each from its own machine.
    func testEndedRunsAreRemovedEachFromItsOwnMachine() throws {
        let app = try launchApp()
        let show = app.buttons["Show"]
        XCTAssertTrue(show.waitForExistence(timeout: 45), "both machines have ended runs")
        show.tap()
        let select = app.buttons["Select"]
        XCTAssertTrue(select.waitForExistence(timeout: 10))
        select.tap()
        app.buttons["Select All"].tap()
        let remove = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Remove ('"))
            .firstMatch
        XCTAssertTrue(remove.waitForExistence(timeout: 5))
        attachScreenshot(app, name: "selected")
        remove.tap()
        app.alerts.buttons["Remove"].tap()
        XCTAssertTrue(
            app.buttons["Show"].waitForNonExistence(timeout: 30), "no ended run is left on either")
        attachScreenshot(app, name: "removed")
    }

    private func attachScreenshot(_ app: XCUIApplication, name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
