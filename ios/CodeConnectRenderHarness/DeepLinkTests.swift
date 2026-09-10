import XCTest

/// **A `?request=` deep link opens the decision card, at every size and
/// whenever the approval arrives.**
///
/// `codeconnect://session/<id>?request=<rid>` is what a tapped notification
/// resolves to. Its whole promise is that the reader lands on the decision,
/// not on the timeline above it.
final class DeepLinkTests: XCTestCase {

    private static let link = "codeconnect://session/fx-1?request=toolu_fixture_high"

    private func launch(_ arguments: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = arguments
        app.terminate()
        app.activate()
        return app
    }

    /// The approval is already in the fleet when the link is consumed: the
    /// `deck` fixture is injected before the automation deep link is applied.
    /// The sheet must open without the reader tapping anything.
    func testTheDeepLinkOpensTheCardWhenTheApprovalIsAlreadyThere() throws {
        let app = launch(["-CC_FIXTURE", "deck", "-CC_DEEPLINK", Self.link])
        XCTAssertTrue(
            app.staticTexts["Decision"].waitForExistence(timeout: 10),
            "the deep link must open the decision sheet on its own — no Review tap")
        XCTAssertTrue(
            app.staticTexts.containing(
                NSPredicate(format: "label CONTAINS %@", "git push --force")).firstMatch.exists,
            "and it is the card the link named")
        app.terminate()
    }

    /// The approval lands AFTER the link has been consumed — a cold launch from
    /// a notification tap, with the socket still catching up. The request id
    /// must be kept until its card exists, and spent only when the sheet opens.
    func testTheDeepLinkOpensTheCardWhenTheApprovalArrivesLate() throws {
        let app = launch([
            "-CC_FIXTURE", "deck", "-CC_DEEPLINK", Self.link,
            "-CC_FIXTURE_LATE_APPROVALS_MS", "2500",
        ])
        XCTAssertFalse(
            app.staticTexts["Decision"].waitForExistence(timeout: 1),
            "nothing to open yet: the approval has been held back")
        XCTAssertTrue(
            app.staticTexts["Decision"].waitForExistence(timeout: 10),
            "the sheet opens when the approval the link named finally arrives")
        app.terminate()
    }
}
