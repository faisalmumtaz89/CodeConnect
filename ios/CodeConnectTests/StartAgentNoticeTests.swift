import XCTest

@testable import CodeConnectKit

/// **What the start form says about an agent that can't start** (screen 31, one rule for every agent): the model menu
/// leaves it out; the form names, in the computer's own sentence, each one that is installed but signed out, and when
/// none can start, every one. One the computer lacks is otherwise not mentioned: one agent is enough.
final class StartAgentNoticeTests: XCTestCase {
    private let choices = StartModels.choices([StartModelChoice(agent: .claude, model: nil, label: "Default", detail: nil)])

    private func notices(_ states: [(StartAgent, StartModels)]) -> [StartContext.AgentNotice] {
        StartContext.notices(states)
    }

    func testASignedOutAgentIsNamedWhileAnotherCanStart() {
        XCTAssertEqual(
            notices([(.claude, choices), (.codex, .signedOut("Codex on Studio isn’t signed in.")), (.antigravity, choices)]),
            [.init(agent: .codex, sentence: "Codex on Studio isn’t signed in.")])
        XCTAssertEqual(
            notices([(.claude, .signedOut("Claude Code on Studio isn’t signed in.")), (.codex, choices)]),
            [.init(agent: .claude, sentence: "Claude Code on Studio isn’t signed in.")])
    }

    /// An agent the computer can't start for another reason (not installed, or below the floor: its answer doesn't say
    /// which) is not mentioned while another can start.
    func testAnAgentThatCantStartIsSilentWhileAnotherCan() {
        XCTAssertEqual(notices([(.claude, choices), (.codex, .missing("Codex isn’t installed on Studio."))]), [])
        XCTAssertEqual(notices([(.claude, .missing("Claude Code isn’t installed on Studio.")), (.codex, choices)]), [])
    }

    func testWhenNoneCanStartEveryOneSaysWhy() {
        XCTAssertEqual(
            notices([
                (.claude, .missing("Claude Code isn’t installed on Studio.")),
                (.codex, .signedOut("Codex on Studio isn’t signed in.")),
            ]),
            [
                .init(agent: .claude, sentence: "Claude Code isn’t installed on Studio."),
                .init(agent: .codex, sentence: "Codex on Studio isn’t signed in."),
            ])
    }

    /// While the computer hasn't answered, nothing is said yet.
    func testNothingIsSaidWhileChecking() {
        XCTAssertEqual(notices([(.claude, .checking), (.codex, .checking)]), [])
        XCTAssertEqual(notices([(.claude, .checking), (.codex, .signedOut("Codex on Studio isn’t signed in."))]), [])
    }
}
