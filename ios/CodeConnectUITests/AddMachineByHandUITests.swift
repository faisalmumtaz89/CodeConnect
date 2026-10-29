import XCTest

/// Adding a machine by hand starts from an empty form.
///
/// Found by a first-run review: with one computer paired, Settings → Add a
/// machine → Pair by hand opened with that computer's address and its stored
/// device token already in the fields, so the new computer's details had to be
/// typed over the old one's secret.
///
/// The pairing seam pairs the app with an unresolvable tailnet name, which is
/// all this needs: a paired machine whose details the form must not reuse.
final class AddMachineByHandUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testPairingByHandAfterAnotherMachineStartsEmpty() {
        // The paired boot path requests notification permission; the system
        // alert must be answered or it sits over every query.
        addUIInterruptionMonitor(withDescription: "notification permission") { alert in
            let allow = alert.buttons["Allow"]
            guard allow.exists else { return false }
            allow.tap()
            return true
        }
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_HOST", "bogus-mac.tail9999.ts.net",
            "-CC_TOKEN", "junktoken",
            "-CC_RESET_CACHE", "YES",
        ]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        // Interruption monitors only fire on interaction; give them one.
        app.tap()

        let settings = app.buttons["Settings"].firstMatch
        XCTAssertTrue(settings.waitForExistence(timeout: 15))
        settings.tap()

        let add = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Add a machine"))
            .firstMatch
        XCTAssertTrue(add.waitForExistence(timeout: 10))
        add.tap()

        // The Simulator has no scanner, so the scan sheet offers the hand form
        // directly — the same form a phone with its camera off is sent to.
        let byHand = app.buttons["Pair by hand"]
        XCTAssertTrue(byHand.waitForExistence(timeout: 10))
        byHand.tap()

        let address = app.textFields["Address"]
        let code = app.textFields["Code or token"]
        XCTAssertTrue(address.waitForExistence(timeout: 10))
        XCTAssertTrue(code.exists)
        XCTAssertFalse(
            (address.value as? String ?? "").contains("bogus-mac"),
            "the new machine's address field holds the paired machine's address")
        XCTAssertFalse(
            (code.value as? String ?? "").contains("junktoken"),
            "the new machine's code field holds the paired machine's token")
    }
}
