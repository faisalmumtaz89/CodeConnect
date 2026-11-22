import XCTest

/// `CardReorderUITests`'s test that measures time: a long press into a drag. It has a class of its own, the
/// serial rule's (ios/README.md).
final class CardReorderSerialUITests: CardReorderUITestsBase {
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
