import XCTest

/// How a conversation reads on the session screen, on the running app: your
/// words and Claude's on one edge, a "Turn complete" line only where nothing
/// else ends the turn, and the computer named in the header.
///
/// `-CC_FIXTURE timeline`: `fx-6` is "hi" and Claude's one-line answer, `fx-7`
/// a longer exchange whose second turn ends on a tool call.
final class ConversationLayoutUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func open(_ session: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "timeline", "-CC_DEEPLINK", "codeconnect://session/\(session)",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        XCTAssertTrue(app.buttons["open-diff"].waitForExistence(timeout: 20), "the session screen")
        return app
    }

    /// The screen x, in points, where an element's bright text starts: the first
    /// pixel column of its own screenshot holding primary-coloured ink.
    private func inkStart(of element: XCUIElement) throws -> CGFloat? {
        let image = try XCTUnwrap(element.screenshot().image.cgImage)
        let width = image.width, height = image.height
        var pixels = [UInt8](repeating: 0, count: width * height * 4)
        let context = try XCTUnwrap(
            CGContext(
                data: &pixels, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        let scale = CGFloat(width) / element.frame.width
        for x in 0..<width {
            for y in 0..<height {
                let i = (y * width + x) * 4
                // Primary text is #EDEDED; the surfaces and tertiary labels stay
                // well under this.
                if Int(pixels[i]) + Int(pixels[i + 1]) + Int(pixels[i + 2]) > 3 * 200 {
                    return element.frame.minX + CGFloat(x) / scale
                }
            }
        }
        return nil
    }

    /// Your words start on the edge Claude's reply starts on, with nothing in
    /// the gutter: the reading lane, 28pt from the screen's edge.
    func testYourWordsAndClaudesStartOnOneEdge() throws {
        let app = open("fx-6")
        let yours = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", "You said: hi")).firstMatch
        let claudes = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label BEGINSWITH %@", "Agent said: Hi!")).firstMatch
        XCTAssertTrue(yours.waitForExistence(timeout: 10))
        XCTAssertTrue(claudes.exists)
        let you = try XCTUnwrap(try inkStart(of: yours), "no text drawn in your row")
        let claude = try XCTUnwrap(try inkStart(of: claudes), "no text drawn in Claude's row")
        XCTAssertEqual(you, claude, accuracy: 1.5, "your words at \(you)pt, Claude's at \(claude)pt")
    }

    /// A turn Claude's reply ends draws no "Turn complete"; one that ends on a
    /// tool call does.
    func testTurnCompleteIsDrawnOnlyWhereNothingElseEndsTheTurn() {
        let turnComplete = NSPredicate(format: "label CONTAINS 'Turn complete'")

        let short = open("fx-6")
        XCTAssertTrue(short.staticTexts["Claude is waiting for your input"].waitForExistence(timeout: 10))
        XCTAssertEqual(
            short.staticTexts.matching(turnComplete).count, 0,
            "a turn its reply ended still drew a Turn complete line")
        short.terminate()

        let long = open("fx-7")
        XCTAssertTrue(long.staticTexts["Claude is waiting for your input"].waitForExistence(timeout: 10))
        XCTAssertEqual(
            long.staticTexts.matching(turnComplete).count, 1,
            "the turn that ended on `npm run lint` must keep its line")
    }

    /// The header names the computer the session is on — the home screen's
    /// label, as the button that opens the link's details. (Which word it adds
    /// when the link is not live is the shared table's, pinned in
    /// `TimelineTurnTests`.)
    func testTheHeaderNamesTheComputer() {
        let app = open("fx-6")
        let link = app.buttons["Link health"].firstMatch
        XCTAssertTrue(link.waitForExistence(timeout: 10))
        let value = link.value as? String ?? ""
        XCTAssertTrue(value.hasPrefix("Jordan’s Mac Studio"), "the header's link says: \(value)")
    }
}
