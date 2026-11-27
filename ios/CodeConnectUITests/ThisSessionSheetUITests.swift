import XCTest

/// **This session's sheet lists every agent's models, and another agent's starts a new session.** On `fx-1`, a hosted
/// Claude session, the computer also lists Codex with one model: choosing it closes the sheet and opens the Start
/// sheet on Ready, on this computer and folder, with Codex and that model, for the person to send.
final class ThisSessionSheetUITests: XCTestCase {

    private let agents = #"[{"agent":"claude","available":true,"models":[{"value":"default","label":"Default (recommended)"},{"value":"sonnet","label":"Sonnet"}],"signed_in":true},{"agent":"codex","available":true,"models":[{"value":"gpt-6.1-sol","label":"GPT-6.1-Sol","detail":"Strong all-round coding model"}],"signed_in":true}]"#

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    /// The sheet opens tall, its models in view as the board draws it, and the choices in force are drawn live: a
    /// disabled button is faded by iOS, which drew the current effort, mode and model as the dimmest rows.
    func testTheSheetOpensTallWithTheCurrentChoicesLive() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let chip = app.buttons["session.composer.settings"]
        XCTAssertTrue(chip.waitForExistence(timeout: 25), "the model chip")
        chip.tap()
        let sonnet = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Sonnet")).firstMatch
        XCTAssertTrue(sonnet.waitForExistence(timeout: 10), "the model rows")
        XCTAssertTrue(sonnet.isHittable, "the models in view without a drag")
        let current = app.buttons.matching(NSPredicate(format: "isSelected == true")).allElementsBoundByIndex
        XCTAssertGreaterThanOrEqual(current.count, 3, "the effort, the mode and the model in force")
        for choice in current { XCTAssertTrue(choice.isEnabled, "\(choice.label) is drawn live") }
    }

    func testAnotherAgentsModelOpensTheStartSheetOnIt() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1",
            "-CC_START_AGENTS_WIRE", agents,
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        let chip = app.buttons["session.composer.settings"]
        XCTAssertTrue(chip.waitForExistence(timeout: 25), "the model chip")
        chip.tap()
        let codex = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "GPT-6.1-Sol")).firstMatch
        XCTAssertTrue(codex.waitForExistence(timeout: 10), "Codex's model, in the sheet")
        // Down the list to it: a press and a drag, as the app's other lists are scrolled.
        for _ in 0..<6 where !codex.isHittable {
            let start = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.8))
            start.press(forDuration: 0.05, thenDragTo: start.withOffset(CGVector(dx: 0, dy: -300)))
        }
        codex.tap()
        let model = app.buttons["start-summary-Model"]
        XCTAssertTrue(model.waitForExistence(timeout: 10), "the Start sheet, on Ready")
        XCTAssertTrue(model.label.contains("GPT-6.1-Sol"), model.label)
        XCTAssertTrue(app.buttons["start-summary-Agent"].label.contains("Codex"), app.buttons["start-summary-Agent"].label)
    }
}
