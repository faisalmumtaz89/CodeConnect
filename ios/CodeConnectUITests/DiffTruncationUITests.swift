import XCTest

/// **The truncated diff, which shipped as a blank black rectangle.**
///
/// Against a real 1.5MB working tree the daemon answered in 124ms with
/// `truncated=true, bytes=524323`, and the sheet drew its chrome and then 99.8%
/// `#000000` below it for 56 seconds and counting: no stamp, no truncation
/// banner, no file chips, no rows, no skeleton. The cause was not a missing
/// branch — every string was already written — but that the document handed its
/// `LazyVStack` **one element per file**, so SwiftUI had to build and lay out
/// all 6,935 rows of a single-hunk capture before it could present a frame.
///
/// Fixture-backed rather than daemon-backed, and deliberately: arranging a
/// >512KB diff on a live Mac took a generated 13,000-line file and a real
/// worktree, which is why this state was reached exactly once, by hand,
/// after the code had shipped. `-cc.debug.diff truncated` builds a capture that
/// is over the grid's budget, and `-cc.debug.diffRows` lowers that budget so the
/// held-back marker is on screen without dragging through 400 rows. Neither
/// argument exists in a release build.
///
/// Every launch pins its own Dynamic Type size, per `DeckUITests`.
final class DiffTruncationUITests: XCTestCase {

    private enum TypeSize: String {
        case large = "UICTContentSizeCategoryL"
        case ax5 = "UICTContentSizeCategoryAccessibilityXXXL"
    }

    override func setUp() { continueAfterFailure = false }

    private func launch(rows: Int = 12, size: TypeSize = .large) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "deck",
            "-cc.debug.diff", "truncated",
            "-cc.debug.diffRows", "\(rows)",
            "-UIPreferredContentSizeCategoryName", size.rawValue,
        ]
        app.launch()
        return app
    }

    /// Fixture fleet → the first session → its diff.
    private func openDiff(_ app: XCUIApplication) throws {
        XCTAssertTrue(app.navigationBars["Fleet"].waitForExistence(timeout: 30))
        let session = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 30), "the fixture fleet has rows")
        session.tap()
        let diff = app.buttons["open-diff"]
        XCTAssertTrue(diff.waitForExistence(timeout: 20))
        diff.tap()
    }

    /// The one that failed: a truncated capture draws the diff **and** says it
    /// is not all of it.
    func testATruncatedDiffDrawsItselfAndSaysSo() throws {
        let app = launch()
        try openDiff(app)

        // The daemon's cap, in the daemon's terms, above the fold.
        let banner = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS '512KB cap'")
        ).firstMatch
        XCTAssertTrue(
            banner.waitForExistence(timeout: 20),
            "a partial diff shown without a loud statement is a lie about state — "
                + "and this one used to show nothing at all")

        // The stamp, which is what makes this a capture rather than a live read.
        let stamp = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'Captured on the Mac'")
        ).firstMatch
        XCTAssertTrue(stamp.exists, "a rendered diff always carries its capture time")

        // And the grid itself. The failure being guarded is 302,388 points of
        // black, so it is not enough that the chrome came back.
        let firstLine = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'import Foundation'")
        ).firstMatch
        XCTAssertTrue(firstLine.exists, "the rows have to be on screen, not just the banner")

        attach(app, "truncated-diff")
    }

    /// The grid draws a bounded number of rows, and the hunk that is holding
    /// lines back says how many, where it stopped.
    func testAHeldBackHunkStatesTheCountAndDrawsMoreWhenAsked() throws {
        let app = launch(rows: 12)
        try openDiff(app)

        let marker = app.buttons.matching(
            NSPredicate(format: "label CONTAINS 'more lines'")
        ).firstMatch
        XCTAssertTrue(
            marker.waitForExistence(timeout: 20),
            "the grid stopped, so it says so at the point it stopped")
        let before = marker.label

        // The banner counts the same thing the marker does.
        let coverage = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS '12 of 126 lines drawn'")
        ).firstMatch
        XCTAssertTrue(coverage.exists, "the banner states the coverage it is actually drawing")

        marker.tap()

        let grown = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS '24 of 126 lines drawn'")
        ).firstMatch
        XCTAssertTrue(
            grown.waitForExistence(timeout: 10),
            "asking for more draws more, and the banner keeps up")
        XCTAssertNotEqual(
            app.buttons.matching(NSPredicate(format: "label CONTAINS 'more lines'"))
                .firstMatch.label,
            before,
            "the count of what is still held back comes down")

        attach(app, "held-back-marker")
    }

    /// The same at AX5, where the budget matters most: the rows are taller, so
    /// the same number of them is more screen, and the banner is longer.
    func testTheTruncatedDiffStillStatesItselfAtAX5() throws {
        let app = launch(rows: 12, size: .ax5)
        try openDiff(app)

        let banner = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS '512KB cap'")
        ).firstMatch
        XCTAssertTrue(banner.waitForExistence(timeout: 25))
        attach(app, "truncated-diff-ax5")
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
