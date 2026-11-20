import XCTest

/// **A pushed screen goes back by a swipe from the screen's left edge**, as
/// every iPhone screen does. The app draws its own back button, and hiding
/// the platform's switches that swipe off unless the navigation controller
/// turns it on again; these drive the swipe on each pushed screen and check
/// it lands on the screen before, and that the back button speaks as the
/// platform's does.
final class BackSwipeTests: XCTestCase {

    private func launch(_ arguments: [String]) -> (XCUIApplication, RenderDriver) {
        let app = XCUIApplication()
        app.launchArguments = arguments
        app.terminate()
        app.activate()
        return (app, RenderDriver(test: self, timeout: 30))
    }

    /// The back button, spoken "Back" and then the screen it returns to.
    private func assertBackButton(_ app: XCUIApplication, returnsTo previous: String) {
        let back = app.buttons["nav-back"]
        XCTAssertTrue(back.waitForExistence(timeout: 10), "the back button is drawn")
        XCTAssertEqual(back.label, "Back")
        XCTAssertEqual(back.value as? String, previous)
    }

    /// A slow drag from the very left edge to most of the way across.
    private func swipeFromLeftEdge(_ app: XCUIApplication) {
        let edge = app.coordinate(withNormalizedOffset: CGVector(dx: 0.0, dy: 0.5))
        edge.press(
            forDuration: 0.05, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.5)),
            withVelocity: .slow, thenHoldForDuration: 0.1)
    }

    func testASessionGoesBackToProjectsByAnEdgeSwipe() throws {
        let (app, driver) = launch(["-CC_FIXTURE", "stacked"])
        try driver.openFirstSession(app)
        try driver.require(app.buttons["session-more"], "the session screen")
        assertBackButton(app, returnsTo: "Projects")
        swipeFromLeftEdge(app)
        XCTAssertTrue(app.buttons["session-more"].waitForNonExistence(timeout: 5), "the session screen left")
        try driver.fleet(app)
        app.terminate()
    }

    func testAComputerGoesBackToSettingsByAnEdgeSwipe() throws {
        let (app, driver) = launch(RenderCatalog.secondMachineArguments)
        try driver.openSettings(app)
        try driver.require(RenderCatalog.secondMachineRow(app), "the second computer's row").tap()
        let remove = try driver.require(app.buttons["Remove this computer"], "the computer's screen")
        assertBackButton(app, returnsTo: "Settings")
        swipeFromLeftEdge(app)
        XCTAssertTrue(remove.waitForNonExistence(timeout: 5), "the computer's screen left")
        try driver.require(app.navigationBars["Settings"], "Settings")
        app.terminate()
    }

    func testFoldersGoBackToSettingsByAnEdgeSwipe() throws {
        let (app, driver) = launch(["-CC_FIXTURE", "deck"])
        try driver.openSettings(app)
        try driver.require(app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Folders'")).firstMatch,
            "the Folders row").tap()
        let folders = try driver.require(app.navigationBars["Folders"], "the Folders screen")
        assertBackButton(app, returnsTo: "Settings")
        swipeFromLeftEdge(app)
        XCTAssertTrue(folders.waitForNonExistence(timeout: 5), "the Folders screen left")
        try driver.require(app.navigationBars["Settings"], "Settings")
        app.terminate()
    }
}
