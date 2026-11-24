import XCTest

/// A start whose computer answers, and lists the new run, before the starting screen has appeared still opens the
/// run: the starting screen hands over on a run it finds already listed, not only on one listed while it watches.
final class StartHandoverUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testAStartListedBeforeItsScreenAppearsOpensTheSession() throws {
        // The catalog's own start to an empty live session, its answer and listing made instant.
        let scene = try XCTUnwrap(RenderCatalog.startSession.first { $0.name == "start2-h-empty" })
        let app = XCUIApplication()
        app.launchArguments =
            scene.arguments + [
                "-CC_START_OUTCOME", "instant",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ]
        app.launch()
        try scene.reach(app, RenderDriver(test: self, timeout: 20))
    }
}
