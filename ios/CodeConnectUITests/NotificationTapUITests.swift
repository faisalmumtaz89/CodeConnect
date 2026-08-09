import XCTest

/// Where a tapped notification actually lands, on screen.
///
/// The model tests prove which route is chosen; this proves the route produces
/// the navigation. Between the two sits `FleetView.consumeDeepLink`, which
/// dismisses whatever is presented and asks for a full-screen cover in the same
/// SwiftUI update — the part no model test can establish.
///
/// SpringBoard is out of reach for XCUITest, so the tap itself is delivered
/// another way. `-CC_TAP` seeds the buffer a cold-started tap lands in, which
/// is the one thing a running app cannot demonstrate. A warm tap arrives by
/// URL once the test can see the screen it has to act on, and goes through
/// `PushWire.deliverTap` — the whole of what Apple's callback does. From there
/// on both are the production path: drain, route, view.
final class NotificationTapUITests: XCTestCase {

    /// A cold launch that already has a tap waiting in the buffer.
    private func launch(tap kind: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "deck", "-CC_TAP", kind]
        app.launch()
        return app
    }

    /// A launch that navigates somewhere first and takes no tap.
    private func launch(showing deepLink: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "deck", "-CC_DEEPLINK", deepLink]
        app.launch()
        return app
    }

    /// Deliver a tap to the running app.
    ///
    /// Called only after the test has *seen* the screen the tap must act on, so
    /// what orders the two is an observation, not a delay.
    private func tap(_ kind: String) {
        XCUIDevice.shared.system.open(
            URL(string: "codeconnect://test-notification-tap/\(kind)")!)
    }

    /// An approval tap has to *show* the decision list, not merely aim at it.
    func testAnApprovalTapLandsOnTheVisibleDecisionList() {
        let app = launch(tap: "approval")
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 25),
            "the tap has to arrive at the list, not just set a route nobody consumed")
    }

    /// Every other kind has no card there, so it must land on the fleet — and
    /// the decision list must not be what the reader is looking at.
    func testAFinishedTurnTapLandsOnTheFleetRatherThanAnEmptyList() {
        let app = launch(tap: "done")
        XCTAssertTrue(
            app.staticTexts["Fleet"].waitForExistence(timeout: 25),
            "the fleet is where a run that finished a turn lives")
        XCTAssertFalse(
            app.navigationBars["Needs you"].exists,
            "there is no card to answer, so the decision list is the wrong place")
    }

    /// **The warm path, from something already on screen.** A cold launch lands
    /// on the fleet anyway, so it cannot tell whether `.fleet` clears anything.
    /// This starts on the decision list and taps a notification about a run that
    /// merely finished a turn: the list has to go.
    func testATapAboutAFinishedTurnDismissesTheDecisionListItLandedOn() {
        let app = launch(showing: "codeconnect://deck")
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 25),
            "the run-up: the decision list is what the reader is looking at")

        tap("done")
        let gone = NSPredicate(format: "exists == false")
        expectation(for: gone, evaluatedWith: app.navigationBars["Needs you"])
        waitForExpectations(timeout: 25)
        XCTAssertTrue(app.staticTexts["Fleet"].waitForExistence(timeout: 10))
    }

    /// The same warm path, the other way, and the harder direction: the reader
    /// is inside a session when a decision arrives. The Deck has to *replace*
    /// what is on screen — a Deck presented underneath the session it covers is
    /// a Deck nobody can see.
    func testAWarmApprovalTapReplacesTheSessionOnScreen() {
        let app = launch(showing: "codeconnect://session/fx-1")
        XCTAssertTrue(
            app.buttons["Terminal"].waitForExistence(timeout: 25),
            "the run-up: a session is what the reader is looking at")

        tap("approval")
        XCTAssertTrue(
            app.navigationBars["Needs you"].waitForExistence(timeout: 25),
            "the tap has to take them to the decision")
        XCTAssertFalse(
            app.buttons["Terminal"].exists,
            "and leave the session behind, not sit under it")
    }
}
