import XCTest

/// The sample fleet, driven the way somebody who has no Mac will drive it.
///
/// **No launch arguments at all.** `-CC_FIXTURE` is the test seam and is debug
/// only; a suite that used it would prove nothing about the build that ships.
/// The app is launched exactly as it arrives, and everything below happens by
/// tapping.
final class SampleModeUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    /// The banner's title, however the design system chooses to case it.
    private func sampleBanner(_ app: XCUIApplication) -> XCUIElement {
        app.staticTexts.containing(
            NSPredicate(format: "label CONTAINS[c] %@", "sample fleet")
        ).firstMatch
    }

    private func launchUnpaired() -> XCUIApplication {
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        return app
    }

    func testAReviewerWithNoMacCanReachTheFleetAndGetBack() {
        let app = launchUnpaired()

        let enter = app.buttons["pairing-sample-mode"]
        XCTAssertTrue(
            enter.waitForExistence(timeout: 15),
            "an unpaired launch must offer a way in for somebody with no Mac")
        enter.tap()

        // The fleet, with agents on it. Matched case-insensitively because the
        // banner style uppercases its title.
        XCTAssertTrue(
            sampleBanner(app).waitForExistence(timeout: 15),
            "the sample fleet must say what it is, on the fleet itself")

        // And the way back out, because a reader who cannot leave has been shown
        // something they are trapped in.
        let leave = app.buttons["Leave"]
        XCTAssertTrue(leave.waitForExistence(timeout: 10), "the sample fleet must be escapable")
        leave.tap()

        XCTAssertTrue(
            enter.waitForExistence(timeout: 15),
            "leaving must return to pairing, not to a blank screen")
    }

    /// The claim on the way in is that nothing is connected. This checks the app
    /// never presents itself as linked to a Mac while showing sample data — the
    /// one way this feature could actually mislead somebody.
    func testSampleModeNeverClaimsToBePaired() {
        let app = launchUnpaired()
        let enter = app.buttons["pairing-sample-mode"]
        XCTAssertTrue(enter.waitForExistence(timeout: 15))
        enter.tap()
        XCTAssertTrue(sampleBanner(app).waitForExistence(timeout: 15))

        // A live link is reported as `Live`; the sample fleet must not borrow
        // that word anywhere on the screen.
        XCTAssertFalse(
            app.staticTexts["Live"].exists,
            "sample data must never be presented as a live link to a Mac")

        // Nor the control that reports one: link health is a statement about a
        // connection, and there is none to make a statement about.
        XCTAssertFalse(
            app.descendants(matching: .any)["Link health"].exists,
            "the sample fleet must not offer a link-health reading")
    }
}
