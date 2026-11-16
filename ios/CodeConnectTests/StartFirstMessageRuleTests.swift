import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Q1: a start needs a first message, and the rule lives once in the Kit.** Both apps' Start surfaces read
/// `StartContext.firstMessageReason` — the iPhone's Start button as its `disabledReason`, the Mac form inside
/// `blocked` — so neither lets an empty start reach ccd, which refuses it anyway (`start_session.rs`). The
/// placeholder drops "(optional)". Must-fail: a rule that admits an empty message would let the start fail at ccd.
@MainActor
final class StartFirstMessageRuleTests: XCTestCase {

    func testAnEmptyFirstMessageIsRefusedAndARealOnePasses() {
        XCTAssertEqual(
            StartContext.firstMessageReason(""), "Write the first message.", "an empty message blocks Start")
        XCTAssertEqual(
            StartContext.firstMessageReason("   \n\t "), "Write the first message.",
            "whitespace-only is empty")
        XCTAssertNil(StartContext.firstMessageReason("Run the tests."), "a real first message passes")
        XCTAssertNil(StartContext.firstMessageReason("  hi  "), "a message with surrounding space still passes")
    }
}
