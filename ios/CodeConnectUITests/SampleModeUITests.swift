import XCTest

/// Sample mode, driven the way the only person who needs it will drive it.
///
/// **No `-CC_FIXTURE`.** That launch argument is the test seam and is still debug
/// only; if these tests used it they would prove nothing about the build an App
/// Store reviewer opens. The app is launched clean, exactly as it arrives from the
/// store, and everything below happens by tapping.
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
        // Only the cache reset, so a previous run's pairing cannot make this pass
        // by showing a real fleet. Nothing here loads sample data.
        app.launchArguments = ["-CC_RESET_CACHE", "YES"]
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
            "sample mode must say what it is, on the fleet itself")

        // And the way back out, because a reviewer who cannot leave has been
        // shown a demo they are trapped in.
        let leave = app.buttons["Leave"]
        XCTAssertTrue(leave.waitForExistence(timeout: 10), "sample mode must be escapable")
        leave.tap()

        XCTAssertTrue(
            enter.waitForExistence(timeout: 15),
            "leaving must return to pairing, not to a blank screen")
    }

    /// The claim on the button is that nothing is connected. This checks the app
    /// never presents itself as paired while showing sample data — the one way
    /// this feature could actually mislead somebody.
    func testSampleModeNeverClaimsToBePaired() {
        let app = launchUnpaired()
        let enter = app.buttons["pairing-sample-mode"]
        XCTAssertTrue(enter.waitForExistence(timeout: 15))
        enter.tap()
        XCTAssertTrue(sampleBanner(app).waitForExistence(timeout: 15))

        // A live link is reported as `Live`; sample mode must not borrow that word.
        XCTAssertFalse(
            app.staticTexts["Live"].exists,
            "sample data must never be presented as a live link to a Mac")
    }
}
