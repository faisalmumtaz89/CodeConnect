import XCTest

extension XCUIElement {
    /// Scrolls the fleet until this element's middle, where a tap lands, sits
    /// above the Deck bar at the bottom of the screen. XCUITest scrolls an
    /// element into view only when it is off the screen; one resting under the
    /// bar is on it, and a tap there lands on the bar's Review.
    func scrollClearOfTheBar(in app: XCUIApplication) {
        // The bar speaks "N waiting for you", as the title's count does above
        // the cards; the bar is the lower of the two, and with one there is no
        // bar to clear.
        let counts = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label ENDSWITH %@", "waiting for you"))
        func barTop() -> CGFloat? {
            let tops = counts.allElementsBoundByIndex.filter(\.exists).map(\.frame.minY)
            return tops.count >= 2 ? tops.max() : nil
        }
        for _ in 0..<8 {
            guard let top = barTop(), frame.midY > top - 12 else { break }
            app.swipeUp()
        }
    }
}
