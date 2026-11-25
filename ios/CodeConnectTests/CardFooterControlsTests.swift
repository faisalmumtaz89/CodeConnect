import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **At accessibility sizes a card's quieter controls move from the bar to its footer**, for every agent whose bar holds
/// them at the other sizes: Claude, Antigravity, an agent the app doesn't know. Not Codex, whose bar keeps its own
/// `Come back to this` at every size.
@MainActor
final class CardFooterControlsTests: XCTestCase {

    func testAnAntigravityCardKeepsItsQuieterControlsAtAccessibilitySizes() {
        XCTAssertTrue(DecisionCardView.footerHoldsSubordinates(agent: .antigravity, isAccessibilitySize: true, isActionable: true))
    }

    func testClaudeKeepsThemAndCodexKeepsItsOwn() {
        XCTAssertTrue(DecisionCardView.footerHoldsSubordinates(agent: .claude, isAccessibilitySize: true, isActionable: true))
        XCTAssertFalse(DecisionCardView.footerHoldsSubordinates(agent: .codex, isAccessibilitySize: true, isActionable: true))
    }

    func testTheFooterHoldsNoneAtOtherSizesOrOnACardThatCannotBeAnswered() {
        XCTAssertFalse(DecisionCardView.footerHoldsSubordinates(agent: .antigravity, isAccessibilitySize: false, isActionable: true))
        XCTAssertFalse(DecisionCardView.footerHoldsSubordinates(agent: .antigravity, isAccessibilitySize: true, isActionable: false))
    }
}
