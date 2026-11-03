import XCTest

/// **A copied diff line is the file's line, not the drawn one.** The grid draws
/// a tab to the next 4-column stop, so a selection of the drawn text would paste
/// spaces where a Makefile recipe needs its tab. A long press on a row offers
/// Copy line, which copies the line exactly as it is in the file.
final class DiffCopyUITests: XCTestCase {

    override func setUp() { continueAfterFailure = false }

    func testCopyLineKeepsAMakefileRecipesTab() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "deck", "-cc.debug.diff", "sample", "-cc.debug.lastCopied", "YES",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        XCTAssertTrue(app.navigationBars["Projects"].waitForExistence(timeout: 30))
        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 30), "the fixture fleet has rows")
        session.tap()
        let more = app.buttons["session-more"]
        XCTAssertTrue(more.waitForExistence(timeout: 20), "the session bar's menu")
        more.tap()
        let diff = app.buttons.matching(NSPredicate(format: "label == 'Changes'")).firstMatch
        XCTAssertTrue(diff.waitForExistence(timeout: 10), "the menu offers the changes")
        diff.tap()

        // The sample's Makefile hunk is last; scroll it into reach.
        let recipe = app.descendants(matching: .any).matching(
            NSPredicate(format: "label ENDSWITH 'swift test --parallel'")
        ).firstMatch
        XCTAssertTrue(recipe.waitForExistence(timeout: 15), "the Makefile's added recipe line")
        for _ in 0..<8 where !recipe.isHittable { app.swipeUp() }
        XCTAssertTrue(recipe.isHittable)

        recipe.press(forDuration: 1.0)
        let copy = app.buttons["Copy line"]
        XCTAssertTrue(copy.waitForExistence(timeout: 5), "a long press offers Copy line")
        copy.tap()

        // Read back inside the app (`-cc.debug.lastCopied`): the test runner is
        // not authorised to read another app's pasteboard.
        let copied = app.descendants(matching: .any)["debug-last-copied"]
        XCTAssertTrue(copied.waitForExistence(timeout: 5))
        XCTAssertEqual(copied.label, "\tswift test --parallel")
    }
}
