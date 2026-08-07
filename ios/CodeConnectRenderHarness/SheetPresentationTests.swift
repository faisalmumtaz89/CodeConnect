import XCTest

// =============================================================================
//  SheetPresentationTests — the one thing a screenshot cannot see.
//
//  The render pass photographs a sheet at rest. This measures what happens
//  *between* two rests, because that is where the defect lived: a sheet that
//  looks right at both detents can still rescale on its way from one to the
//  other, and no still frame of either end shows it.
// =============================================================================

final class SheetPresentationTests: XCTestCase {

    /// **Expanding a sheet moves it and does not resize it.**
    ///
    /// iOS 26 draws a sheet at a partial detent as an inset floating card and
    /// `.large` edge-to-edge. Offering both put every element of this sheet
    /// 1.04145× larger once expanded — width and height — so the drag ended in
    /// a visible flinch. `CCSheetDetents` keeps both detents out of `.large`;
    /// this is the assertion that says so, in the only terms that can be
    /// checked: the same element, measured at both ends of the same gesture.
    ///
    /// The y assertion is not decoration. Without it a drag that failed to
    /// expand anything would satisfy every equality below and pass green.
    func testExpandingASheetTranslatesItWithoutResizingIt() throws {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
        ]
        app.terminate()
        app.activate()

        let driver = RenderDriver(test: self, timeout: 30)
        try driver.openPaletteRow(app, fragment: "/m", row: "/model")
        try driver.require(driver.text(containing: "Choose model", in: app), "the chooser section")

        // The sheet arrives animating; its frame before the animation settles
        // is not the frame of the detent it is heading for.
        Thread.sleep(forTimeInterval: 1.0)

        let title = try driver.require(app.staticTexts["Model"].firstMatch, "the sheet title")
        let row = try driver.require(app.staticTexts["Opus 5"].firstMatch, "a chooser row")
        let titleBefore = title.frame
        let rowBefore = row.frame

        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.53))
            .press(
                forDuration: 0.3,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.08)),
                withVelocity: .slow,
                thenHoldForDuration: 0.5)
        Thread.sleep(forTimeInterval: 1.0)

        let titleAfter = title.frame
        let rowAfter = row.frame

        XCTAssertLessThan(
            titleAfter.minY, titleBefore.minY - 100,
            "the drag did not expand the sheet, so nothing below was actually tested")

        // A point of tolerance: these are rendered frames, not layout constants.
        // The defect this guards was 4%, which on the widest of these is ~15pt.
        for (name, before, after) in [
            ("title", titleBefore, titleAfter), ("row", rowBefore, rowAfter),
        ] {
            XCTAssertEqual(
                after.minX, before.minX, accuracy: 1.0,
                "\(name) moved horizontally: the sheet changed appearance class")
            XCTAssertEqual(
                after.width, before.width, accuracy: 1.0,
                "\(name) changed width between detents — the sheet rescaled")
            XCTAssertEqual(
                after.height, before.height, accuracy: 1.0,
                "\(name) changed height between detents — the sheet rescaled")
        }

        app.terminate()
    }
}
