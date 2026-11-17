import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **"Not sent: stopped": a send a Stop cancelled marks the person's own row, agent-neutral.**
///
/// The daemon files `send_not_sent {request_id, reason:"stopped"}` for a message a Stop dropped — Claude's
/// B7 part 2, and the same for a Codex follow-up (X5). The Kit joins it to the person's `user_message` row
/// by `request_id`: the row is marked (`TimelineItem.notSent`, so the view reads "Not sent: stopped") and the
/// notice draws no row of its own. The join keys on the notice and the row, not the agent. Proved on the
/// daemon's own `person_row` and `send_not_sent` from `fixtures/claude/minor-37-wire.json`.
final class SendNotSentRowTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let url = repoRoot.appendingPathComponent("fixtures/claude/minor-37-wire.json")
        guard let data = try? Data(contentsOf: url) else {
            throw XCTSkip("fixtures/claude/minor-37-wire.json is not reachable from this bundle")
        }
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
    }

    /// A person's message carrying its send's `request_id` in the payload, as the daemon files it.
    private func userMessage(seq: UInt64, requestID: String, text: String) throws -> Event {
        let idJSON = JSONValue.string(requestID).canonicalJSONString
        let textJSON = JSONValue.string(text).canonicalJSONString
        let json = """
            {"seq":\(seq),"session_id":"cc-1","ts":"2026-08-04T20:00:0\(seq).000Z",
             "kind":"user_message","source":"hook",
             "payload":{"type":"user","request_id":\(idJSON),"message":{"role":"user","content":\(textJSON)}}}
            """
        return try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    /// The daemon's own `send_not_sent` payload, wrapped in a notification event.
    private func notificationEvent(seq: UInt64, payload: [String: JSONValue]) throws -> Event {
        let payloadJSON = JSONValue.object(payload).canonicalJSONString
        let json = """
            {"seq":\(seq),"session_id":"cc-1","ts":"2026-08-04T20:00:09.000Z",
             "kind":"notification","source":"daemon","payload":\(payloadJSON)}
            """
        return try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    func testASendAStopCancelledMarksItsRowAndDrawsNoNoticeRow() throws {
        let fixture = try wire()
        let personRow = try XCTUnwrap(fixture["person_row"]?.objectValue)
        let stoppedID = try XCTUnwrap(personRow["request_id"]?.stringValue)
        let stoppedText = try XCTUnwrap(personRow["text"]?.stringValue)
        let notice = try XCTUnwrap(fixture["send_not_sent"]?.objectValue)

        let cancelled = try userMessage(seq: 1, requestID: stoppedID, text: stoppedText)
        let ran = try userMessage(seq: 2, requestID: "ran-req-1", text: "this message ran")
        let sendNotSent = try notificationEvent(seq: 3, payload: notice)

        let items = TimelineBuilder.build([cancelled, ran, sendNotSent])

        let userRows = items.filter {
            if case .userMessage = $0.content { return true } else { return false }
        }
        XCTAssertEqual(userRows.count, 2, "both person messages render")
        XCTAssertEqual(userRows.filter(\.notSent).count, 1, "exactly the cancelled send is marked")
        XCTAssertEqual(
            try XCTUnwrap(userRows.first(where: \.notSent)).content, .userMessage(stoppedText),
            "the marked row is the one the Stop cancelled")
        XCTAssertFalse(
            items.contains { if case .notice = $0.content { return true } else { return false } },
            "the send_not_sent notice is consumed into the row, not drawn as its own")
    }
}
