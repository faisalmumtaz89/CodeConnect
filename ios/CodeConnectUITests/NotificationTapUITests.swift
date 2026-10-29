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
            app.navigationBars["Waiting for you"].waitForExistence(timeout: 25),
            "the tap has to arrive at the list, not just set a route nobody consumed")
    }

    /// Every other kind has no card there, so it must land on the fleet — and
    /// the decision list must not be what the reader is looking at.
    func testAFinishedTurnTapLandsOnTheFleetRatherThanAnEmptyList() {
        let app = launch(tap: "done")
        XCTAssertTrue(
            app.staticTexts["Projects"].waitForExistence(timeout: 25),
            "the fleet is where a run that finished a turn lives")
        XCTAssertFalse(
            app.navigationBars["Waiting for you"].exists,
            "there is no card to answer, so the decision list is the wrong place")
    }

    /// **The warm path, from something already on screen.** A cold launch lands
    /// on the fleet anyway, so it cannot tell whether `.fleet` clears anything.
    /// This starts on the decision list and taps a notification about a run that
    /// merely finished a turn: the list has to go.
    func testATapAboutAFinishedTurnDismissesTheDecisionListItLandedOn() {
        let app = launch(showing: "codeconnect://deck")
        XCTAssertTrue(
            app.navigationBars["Waiting for you"].waitForExistence(timeout: 25),
            "the run-up: the decision list is what the reader is looking at")

        tap("done")
        let gone = NSPredicate(format: "exists == false")
        expectation(for: gone, evaluatedWith: app.navigationBars["Waiting for you"])
        waitForExpectations(timeout: 25)
        XCTAssertTrue(app.staticTexts["Projects"].waitForExistence(timeout: 10))
    }

    /// **From a chat opened on the fleet**, which the fleet stops drawing for
    /// while it is out of view. A tap about a finished turn has to bring the
    /// fleet back, current; a tap about a decision from a chat has to open the
    /// decision list over it.
    func testTapsFromAChatOpenedOnTheFleetLandWhereTheySay() {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "deck"]
        app.launch()
        let row = app.descendants(matching: .any).matching(identifier: "session-fx-1").firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 25), "the fleet")
        row.tap()
        XCTAssertTrue(app.buttons["Terminal"].waitForExistence(timeout: 20), "fx-1's chat")

        tap("done")
        XCTAssertTrue(
            app.staticTexts["3 waiting for you"].waitForExistence(timeout: 15),
            "the fleet is back, with its count under the title")
        XCTAssertEqual(
            app.buttons["deck-bar"].label, "3 waiting for you", "and the Deck bar under it")
        XCTAssertFalse(app.buttons["Terminal"].exists, "and the chat is gone")

        row.tap()
        XCTAssertTrue(app.buttons["Terminal"].waitForExistence(timeout: 20), "fx-1's chat again")
        tap("approval")
        XCTAssertTrue(
            app.navigationBars["Waiting for you"].waitForExistence(timeout: 25),
            "the tap takes them to the decision")
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
            app.navigationBars["Waiting for you"].waitForExistence(timeout: 25),
            "the tap has to take them to the decision")
        XCTAssertFalse(
            app.buttons["Terminal"].exists,
            "and leave the session behind, not sit under it")
    }

    /// A link to one session, followed from inside another, replaces the chat
    /// on screen — and is then done with. Back from the chat it opened lands on
    /// the fleet and stays there; a link still waiting to be followed would
    /// take the reader straight back into the chat they just left.
    func testBackFromAChatALinkSwappedInStaysOnTheFleet() {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "deck"]
        app.launch()
        let row = app.descendants(matching: .any)["session-fx-1"]
        XCTAssertTrue(row.waitForExistence(timeout: 25), "fx-1's row")
        row.tap()
        let first = composer(app, for: "app-1")
        XCTAssertTrue(
            first.waitForExistence(timeout: 25),
            "the run-up: fx-1's chat, opened by a tap, is what the reader is looking at")

        XCUIDevice.shared.system.open(URL(string: "codeconnect://session/fx-2")!)
        XCTAssertTrue(
            composer(app, for: "app-2").waitForExistence(timeout: 25),
            "the link replaces the chat on screen with fx-2's")
        expectation(for: NSPredicate(format: "exists == false"), evaluatedWith: first)
        waitForExpectations(timeout: 10)

        app.navigationBars.buttons.firstMatch.tap()
        XCTAssertTrue(app.staticTexts["Projects"].waitForExistence(timeout: 10), "Back lands on the fleet")
        XCTAssertFalse(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 3),
            "and stays there: no chat is pushed again")
    }

    /// The chat a link swaps in is that session's own: words typed into the chat
    /// it replaced do not come with it, where Send would deliver them to a
    /// session they were never written for.
    func testAChatALinkSwappedInDoesNotInheritTheDraft() {
        let app = launch(showing: "codeconnect://session/fx-1")
        let first = composer(app, for: "app-1")
        XCTAssertTrue(first.waitForExistence(timeout: 25), "the run-up: fx-1's chat")
        first.tap()
        first.typeText("meant for app-1")

        XCUIDevice.shared.system.open(URL(string: "codeconnect://session/fx-2")!)
        let second = composer(app, for: "app-2")
        XCTAssertTrue(second.waitForExistence(timeout: 25), "the link swaps in fx-2's chat")
        XCTAssertFalse(
            (second.value as? String ?? "").contains("meant for app-1"),
            "fx-2's compose field holds none of fx-1's draft: \(String(describing: second.value))")
    }

    /// A link that names a decision, followed from inside another session's
    /// chat, opens that decision in the chat it swaps in.
    func testALinkSwappingTheChatOpensTheDecisionItNames() {
        let app = launch(showing: "codeconnect://session/fx-1")
        XCTAssertTrue(
            composer(app, for: "app-1").waitForExistence(timeout: 25), "the run-up: fx-1's chat")

        XCUIDevice.shared.system.open(
            URL(string: "codeconnect://session/fx-2?request=toolu_fixture_medium")!)
        XCTAssertTrue(
            app.staticTexts["Verified against the daemon's hash."]
                .waitForExistence(timeout: 25),
            "a decision card opens")
        XCTAssertTrue(
            text(app, containing: "Feature.swift").exists,
            "it is fx-2's: the write to Sources/Feature.swift")
        XCTAssertFalse(
            text(app, containing: "git push --force origin main").exists,
            "not fx-1's force-push, from the chat the link replaced")
    }

    /// A link to the chat already on screen keeps that chat as it is — the
    /// reader's draft included — and is done with: Back stays on the fleet.
    func testALinkToTheChatOnScreenKeepsItsDraft() {
        let app = launch(showing: "codeconnect://session/fx-1")
        let field = composer(app, for: "app-1")
        XCTAssertTrue(field.waitForExistence(timeout: 25), "the run-up: fx-1's chat")
        field.tap()
        field.typeText("still mine")

        XCUIDevice.shared.system.open(URL(string: "codeconnect://session/fx-1")!)
        XCTAssertTrue(field.waitForExistence(timeout: 10), "fx-1's chat is still on screen")
        XCTAssertTrue(
            (field.value as? String ?? "").contains("still mine"),
            "and still holds the draft: \(String(describing: field.value))")

        app.navigationBars.buttons.firstMatch.tap()
        XCTAssertTrue(app.staticTexts["Projects"].waitForExistence(timeout: 10), "Back lands on the fleet")
        XCTAssertFalse(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 3),
            "and stays there: the link was followed once")
    }

    /// Any element whose label holds `fragment`.
    private func text(_ app: XCUIApplication, containing fragment: String) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@", fragment))
            .firstMatch
    }

    /// A session's compose field, named after the run it types into.
    private func composer(_ app: XCUIApplication, for runLabel: String) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", "Message for \(runLabel)"))
            .firstMatch
    }
}
