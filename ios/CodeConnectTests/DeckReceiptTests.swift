import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The finish line counts a card the person answered even when the computer confirms it only later.
///
/// An answer sent during a link stall lands `.sentUnconfirmed`: the card stays in the queue, not settled,
/// and the computer's later `approval_resolved` drops it from the pending set. Before this, the pass never
/// counted it, so the finish line read "Nothing was waiting" though the person had just answered a card.
/// The pass records such a card (`recordSent`) and counts it once the confirmation removes it (`reconcile`).
final class DeckReceiptTests: XCTestCase {

    func testAnAnswerConfirmedAfterItWasSentCountsOnTheFinishLine() {
        var pass = DeckPass()
        pass.recordSent("run-a:req-1")
        XCTAssertEqual(pass.settledCount, 0, "not counted while the computer has not confirmed it")
        // The computer confirms: the card is no longer pending in the log.
        pass.reconcile(pending: [])
        XCTAssertEqual(pass.settledCount, 1, "the person answered it, so a confirmed answer counts")
    }

    func testAnUnconfirmedCardStillPendingIsNotCountedYet() {
        var pass = DeckPass()
        pass.recordSent("run-a:req-1")
        pass.reconcile(pending: ["run-a:req-1"])
        XCTAssertEqual(pass.settledCount, 0, "still waiting on the computer: nothing to count yet")
    }

    func testACardThePersonNeverAnsweredIsNotCounted() {
        var pass = DeckPass()
        // No `recordSent`: the person did not answer this card on this phone (it was answered at the
        // Mac, or it ended on its own). It leaves the queue, but the pass does not claim it.
        pass.reconcile(pending: [])
        XCTAssertEqual(pass.settledCount, 0)
    }

    func testSettlingAnAwaitingCardDoesNotDoubleCountIt() {
        var pass = DeckPass()
        pass.recordSent("run-a:req-1")
        pass.settle("run-a:req-1")
        pass.reconcile(pending: [])
        XCTAssertEqual(pass.settledCount, 1, "settle moves it out of awaiting, so reconcile cannot re-add it")
    }

    /// The card was resolved (answered at the computer) and left the pending set BEFORE the phone's send
    /// timed out, so `recordSent` runs after the card is already gone. Reconciling at that moment still
    /// counts it — the receipt must not depend on which arrived first. (The view reconciles right after
    /// `recordSent` for exactly this ordering.)
    func testACardConfirmedBeforeItWasRecordedIsStillCounted() {
        var pass = DeckPass()
        pass.reconcile(pending: [])        // the card is already gone, nothing recorded yet
        pass.recordSent("run-a:req-1")     // the phone times out as sentUnconfirmed, late
        pass.reconcile(pending: [])        // reconcile at that moment counts it
        XCTAssertEqual(pass.settledCount, 1)
    }
}
