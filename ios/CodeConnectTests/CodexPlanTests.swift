import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Codex's plan in Plan mode is the turn's reply.** ccd files Codex's `plan` item as
/// `EventKind.other("codex_plan")`, and the timeline drew nothing for it: after the question card was
/// answered the turn read "Turn complete", and the person never saw the plan. `plan-item-wire.json`
/// (copied byte for byte from `fixtures/codex/`) is the run as ccd sends it, fed a real 0.170 plan item.
final class CodexPlanTests: XCTestCase {

    private func frames() throws -> [[String: Any]] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: "plan-item-wire", withExtension: "json"))
        let root = try XCTUnwrap(try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let completed = try XCTUnwrap(root["completed"] as? [String: Any])
        return try XCTUnwrap(completed["frames"] as? [[String: Any]])
    }

    private func event(_ json: Any) throws -> Event {
        try JSONDecoder().decode(Event.self, from: JSONSerialization.data(withJSONObject: json))
    }

    private func events() throws -> [Event] {
        try frames().map { try event(try XCTUnwrap($0["event"])) }
    }

    /// The plan draws once, as the agent's prose, after the answered question and where the turn ended: the
    /// turn's "Turn complete" folds under it as under any reply.
    func testAPlanDrawsAsTheAgentsReplyWhereItHappened() throws {
        let drawn = TimelineBuilder.drawn(TimelineBuilder.build(try events()))
        let replies = drawn.enumerated().compactMap { index, item -> (Int, String)? in
            if case .agentMessage(let text, let isInterrupted) = item.content, !isInterrupted { return (index, text) }
            return nil
        }
        XCTAssertEqual(replies.map(\.1), ["Once `notes.txt` exists, rename it to `a.txt`."], "the plan, once")
        let plan = try XCTUnwrap(replies.first).0
        let question = try XCTUnwrap(
            drawn.firstIndex {
                if case .approval = $0.content { return true }
                return false
            }, "the answered question")
        XCTAssertLessThan(question, plan, "the plan follows the answer")
        XCTAssertEqual(plan, drawn.count - 1, "the plan ends the turn; its Turn complete folds under it")
    }

    /// A plan with no visible text draws no row, as an empty reply does.
    func testABlankPlanDrawsNoRow() throws {
        var json = try XCTUnwrap(
            try frames().compactMap { $0["event"] as? [String: Any] }.first { $0["kind"] as? String == "codex_plan" })
        var payload = try XCTUnwrap(json["payload"] as? [String: Any])
        payload["text"] = " \n\n"
        json["payload"] = payload
        let blank = try event(json)
        XCTAssertNil(blank.codexPlanText)
        XCTAssertTrue(
            TimelineBuilder.build([blank]).allSatisfy {
                if case .agentMessage = $0.content { return false }
                return true
            })
    }
}
