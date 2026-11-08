import XCTest

/// Held and dragged, a project card moves, and it is still there after the app
/// is closed and opened again; swiped, the list scrolls, whether the swipe
/// starts on a card or not. `-CC_FIXTURE_KEEP_ORDER` lets this fixture run keep
/// its order, which every other fixture run starts without, so the tests read
/// the order they are given rather than assuming one.
final class CardReorderUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launch(_ fixture: String = "deck") -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-CC_FIXTURE", fixture, "-CC_FIXTURE_KEEP_ORDER", "YES"]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        return app
    }

    /// Each card's header, top to bottom, by its folder (`~/app-N`). A header
    /// is the topmost element naming its folder: the folder's own line inside
    /// the card names it too.
    private func cards(_ app: XCUIApplication) -> [(folder: String, header: XCUIElement)] {
        let named = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@", "~/app-"))
        XCTAssertTrue(named.firstMatch.waitForExistence(timeout: 30), "no project card")
        var seen = Set<String>()
        return named.allElementsBoundByIndex
            .filter { $0.exists && $0.isHittable }
            .sorted { $0.frame.minY < $1.frame.minY }
            .compactMap { element in
                let folder = element.label.components(separatedBy: ", ").first { $0.hasPrefix("~/app-") }
                guard let folder, seen.insert(folder).inserted else { return nil }
                return (folder, element)
            }
    }

    /// The folders once their order has held still for three seconds: a fleet
    /// still arriving re-sorts itself on a build that keeps no order.
    private func settled(_ app: XCUIApplication) -> [String] {
        var last = cards(app).map(\.folder)
        var still = 0
        for _ in 0..<30 where still < 3 {
            sleep(1)
            let now = cards(app).map(\.folder)
            still = now == last ? still + 1 : 0
            last = now
        }
        return last
    }

    func testADraggedCardKeepsItsPlaceAcrossALaunch() throws {
        let app = launch()
        let order = settled(app)
        XCTAssertGreaterThanOrEqual(order.count, 2, "\(order)")
        let (first, second) = (order[0], order[1])
        XCTAssertNotEqual(first, second)
        let headers = cards(app)

        headers[1].header.press(forDuration: 1.0, thenDragTo: headers[0].header)
        let moved = settled(app)
        XCTAssertEqual(Array(moved.prefix(2)), [second, first], "the drag did not move the card: \(moved)")

        app.terminate()
        let again = launch()
        let kept = settled(again)
        XCTAssertEqual(Array(kept.prefix(2)), [second, first], "the order was not kept: \(kept)")
    }

    /// A quick swipe that starts on a card's run scrolls the list, as on any
    /// list, and moves no card: only a deliberate hold picks one up.
    func testASwipeStartingOnACardScrollsTheList() throws {
        let app = launch("stacked")
        let order = settled(app)
        let top = cards(app)[0].header
        let before = top.frame.minY
        let run = app.descendants(matching: .any).matching(identifier: "session-fx-2").firstMatch
        XCTAssertTrue(run.waitForExistence(timeout: 10) && run.isHittable, "run fx-2 is not on screen")

        run.swipeUp()
        sleep(2)
        XCTAssertLessThan(
            top.frame.minY, before - 60, "a swipe starting on a card did not scroll the list")
        app.swipeDown()
        XCTAssertEqual(settled(app), order, "a swipe moved a card")
    }
}
