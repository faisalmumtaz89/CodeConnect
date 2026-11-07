import XCTest

/// A pairing link opened from outside the app lands on its sheet, and pairs
/// nothing until Pair is tapped.
///
/// `-CC_OPEN_LINK` hands the link to the handler `onOpenURL` calls: a universal
/// link itself needs codeconnect.sh to vouch for this build, which a simulator
/// run cannot arrange. The link is the one `codeconnect pair` prints, from
/// `fixtures/wire/qr-link.txt`.
final class PairingLinkUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testAPairingLinkOpensItsSheetAndWaitsForPair() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_OPEN_LINK", "https://codeconnect.sh/p#v=1&h=studio.tail0000.ts.net&p=8787&c=ABCD2345",
        ]
        app.launch()

        // The computer's name, not the sheet's title: the first screen has a
        // step called "Pair this iPhone" too.
        XCTAssertTrue(
            app.staticTexts["studio.tail0000.ts.net"].waitForExistence(timeout: 20),
            "the link has to open the pairing sheet, naming the computer")
        XCTAssertTrue(app.buttons["Pair"].exists)

        // Closed without a tap, the first screen is still inviting a pairing
        // rather than showing one under way.
        app.buttons["Close"].tap()
        XCTAssertTrue(
            app.buttons["pairing-sample-mode"].waitForExistence(timeout: 10),
            "opening a link must not start pairing by itself")
    }
}
