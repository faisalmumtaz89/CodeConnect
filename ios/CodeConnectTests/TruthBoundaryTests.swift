import XCTest

@testable import CodeConnect

/// What the reader acts on is checked against the truth, and what the reader is
/// credited with having seen is what was drawn — never the other way round.
@MainActor
final class TruthBoundaryTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    private func message(_ key: String, seq: Int) throws -> Event {
        try event(
            """
            {"seq":\(seq),"session_uid":"\(key)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:0\(seq)Z","kind":"user_message","source":"transcript",\
            "payload":{"message":{"content":"step \(seq)"}}}
            """)
    }

    /// An event that has arrived and not been drawn is not something the
    /// reader has seen, so opening or leaving the chat does not mark it.
    func testReviewingMarksOnlyWhatIsDrawn() async throws {
        let key = "01K1B3XQ8ZC0DE5FGH7JKMNPQR-review-\(UUID().uuidString)"
        defer { ReviewMarks.forget(sessionKey: key) }
        let state = SessionState(sessionKey: key)
        for seq in 1...3 { state.ingest(try message(key, seq: seq)) }
        await state.settleForTesting()
        state.ingest(try message(key, seq: 4))

        state.markReviewed()
        XCTAssertEqual(ReviewMarks.reviewedSeq(for: key), 3, "seq 4 is not on screen yet")
    }

    private let requestEvent = """
        {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:00.000Z",
         "kind":"approval_request","source":"hook","payload":{"card":{
           "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
           "tool_input":{"command":"ls"},"display_text":"Bash"}}}
        """
    private let resolvedEvent = """
        {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:01.000Z",
         "kind":"approval_resolved","source":"daemon","payload":{
           "request_id":"toolu_1","session_id":"cc-1","decision":{"type":"deny"},
           "resolved_by":"local","applied_via":"send_keys",
           "resolved_at":"2026-08-18T09:00:01.000Z"}}
        """

    /// The Mac resolved the card and the event is in, but not yet drawn: the
    /// reader's tap on the card still on screen sends nothing, and says what
    /// became of it.
    func testAnAnswerToACardTheTruthHasResolvedSendsNothing() async throws {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["hook_only_approvals": .bool(true)]), minor: 22)
        var answers = 0
        model.connection.sendStub = { message in
            if case .answer = message { answers += 1 }
            throw ConnectionError.notConnected
        }
        model.connection.injectForTesting(.event(try event(requestEvent)))
        model.connection.injectForTesting(
            .sessions([
                try JSONDecoder().decode(
                    SessionSummary.self,
                    from: Data(
                        """
                        {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1",
                         "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                         "last_seq":1,"created_at":"2026-08-18T09:00:00.000Z",
                         "updated_at":"2026-08-18T09:00:00.000Z","blocked_on":[],"agent":"claude"}
                        """.utf8))
            ]))
        let state = try XCTUnwrap(model.statesForTesting["u-1"])
        await state.settleForTesting()
        let card = try XCTUnwrap(state.pendingApprovals.first)

        model.connection.injectForTesting(.event(try event(resolvedEvent)))
        let attempt = await model.answer(item: card, decision: .allow)

        XCTAssertEqual(answers, 0, "nothing goes to the Mac for a card it has resolved")
        guard case .duplicate(let outcome, _) = attempt else {
            return XCTFail("the card says what became of it, got \(attempt)")
        }
        XCTAssertEqual(outcome.decision, .deny)
    }
}
