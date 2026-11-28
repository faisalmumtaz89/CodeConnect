import XCTest

@testable import CodeConnectKit

/// **One mark per agent, the same in both apps**: the Mac's mark (an asterisk for Claude, a code chevron for Codex, a
/// sparkle for Antigravity, a question mark for an agent this build does not know), each at the size it is drawn in its
/// 18 pt circle.
final class AgentMarkTests: XCTestCase {
    func testEachAgentHasTheMacsMark() {
        XCTAssertEqual(AgentKind.claude.mark, AgentKind.Mark(symbol: "asterisk", size: 9))
        XCTAssertEqual(AgentKind.codex.mark, AgentKind.Mark(symbol: "chevron.left.forwardslash.chevron.right", size: 7.5))
        XCTAssertEqual(AgentKind.antigravity.mark, AgentKind.Mark(symbol: "sparkle", size: 8.5))
        XCTAssertEqual(AgentKind.unsupported("newagent").mark, AgentKind.Mark(symbol: "questionmark", size: 8))
    }
}
