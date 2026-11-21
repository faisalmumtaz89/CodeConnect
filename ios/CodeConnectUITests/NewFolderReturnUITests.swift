import XCTest

/// A card with a name to type is answered from the keyboard: the field's return key gives the card's acting answer,
/// as Return does on the Mac's card. Here, New folder: a name and return make the folder, with no tap on Create.
final class NewFolderReturnUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testReturnInTheNameMakesTheFolder() {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", "deck", "-CC_FIXTURE_REAL_NAMES", "YES", "-CC_BIOMETRICS", "allow"]
        app.launch()

        let entry = app.buttons["start-entry"]
        XCTAssertTrue(entry.waitForExistence(timeout: 20), "New agent")
        entry.tap()
        let computer = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-computer-'")).firstMatch
        XCTAssertTrue(computer.waitForExistence(timeout: 10), "a computer")
        computer.tap()
        let browse = app.buttons["start-browse"]
        XCTAssertTrue(browse.waitForExistence(timeout: 10), "Browse folders")
        browse.tap()
        let newFolder = app.buttons["start-new-folder"]
        XCTAssertTrue(newFolder.waitForExistence(timeout: 10), "New folder")
        newFolder.tap()

        let create = app.buttons["folder.new.create"]
        XCTAssertTrue(create.waitForExistence(timeout: 5), "the New folder card")
        let field = app.textFields.firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5), "the name field")
        field.typeText("x\n")

        let made = app.staticTexts.matching(NSPredicate(format: "label ENDSWITH %@", "jordan/x")).firstMatch
        XCTAssertTrue(made.waitForExistence(timeout: 10), "return made the folder and the browser went into it")
        XCTAssertFalse(create.exists, "the card is answered")
    }
}
