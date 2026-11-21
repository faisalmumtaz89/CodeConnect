import XCTest

/// `CardReorderUITests`'s tests that rest on time: a fixture clock set at launch, an elapsed-time budget or a long press.
/// They run on this Mac before every release and are left off the CI runner, which is slower than they allow.
final class CardReorderTimedUITests: CardReorderUITestsBase {
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
}
