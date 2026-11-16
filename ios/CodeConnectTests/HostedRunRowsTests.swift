import XCTest

@testable import CodeConnectKit

/// **A hosted Claude run draws its start and its clear once, as a terminal run does.**
///
/// The agent host files a `session_start` for every init claude sends. ccd stamps a launch's first `startup` and a
/// clear's `clear`; a later turn's start carries no source and draws no row. A Codex start never carries a source (ccd's
/// Codex adapter files the thread's folder, rollout and CLI version only), so it keeps its row. ccd's own
/// `conversation_cleared` notice on a hosted clear draws the row a terminal run's `/clear` draws, once.
///
/// Pinned to `fixtures/claude/mail-wake-wire.json` (a later turn's start, seq 7) and
/// `fixtures/claude/hosted-clear-wire.json` (a clear), each copied byte for byte.
@MainActor
final class HostedRunRowsTests: XCTestCase {

    private func wire(_ name: String) throws -> [String: JSONValue] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func decoded<T: Decodable>(_ type: T.Type, _ key: String, in name: String) throws -> T {
        let value = try XCTUnwrap(try wire(name)[key], "no \(key) in \(name).json")
        return try JSONDecoder().decode(T.self, from: Data(value.canonicalJSONString.utf8))
    }

    private func events(_ name: String) throws -> [Event] {
        try decoded([Event].self, "session_hosted_live_events", in: name)
    }

    private func state(_ events: [Event]) async -> SessionState {
        let state = SessionState(sessionKey: events.first?.sessionKey ?? "")
        for event in events { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return state
    }

    private func shape(_ items: [TimelineItem]) -> [String] {
        items.map { item in
            switch item.content {
            case .userMessage(let text, _): return "you:\(text)"
            case .agentMessage(let text, _): return "reply:\(text)"
            case .tool(let tool): return "tool:\(tool.name)"
            case .approval: return "card"
            case .notice(let notice): return "notice:\(notice.title)"
            }
        }
    }

    /// The row a terminal run's `/clear` invocation draws (Timeline.swift), written out.
    private let terminalClearRow = TimelineItem.Content.notice(
        NoticeItem(kind: .other, symbol: "eraser", title: "Conversation cleared.", detail: nil, severity: .info))

    private func codex(_ seq: UInt64, _ kind: String, _ payload: String) throws -> Event {
        try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "ts":"2026-08-02T10:00:\(String(format: "%02d", seq))Z","kind":"\(kind)",
                 "payload":\(payload),"source":"codex"}
                """.utf8))
    }

    /// The woken turn's start (seq 7) is the per-turn init: no source, from the agent host. It draws nothing; the
    /// launch's own start (seq 3, `startup`) keeps its row.
    func testAHostedTurnsStartDrawsNoRow() async throws {
        let all = try events("mail-wake-wire")
        let turn = try XCTUnwrap(all.first { $0.seq == 7 })
        XCTAssertEqual(turn.kind, .sessionStart)
        XCTAssertEqual(turn.source, .sdk)
        XCTAssertNil(turn.payload["source"], "the fixture's later start carries no source")
        let drawn = await state(all)
        XCTAssertEqual(shape(drawn.timeline.filter { $0.seq == 7 }), [], "a hosted turn's start draws no row")
        XCTAssertEqual(
            shape(drawn.timeline.filter { $0.seq == 3 }), ["notice:Session started"],
            "the launch's start keeps its row")
    }

    /// A Codex thread started mid-run files a start with no source too; it is a new conversation and keeps its row.
    func testACodexStartMidRunKeepsItsRow() throws {
        let events = [
            try codex(1, "user_message", #"{"text":"Fix the build"}"#),
            try codex(
                2, "session_start",
                #"{"thread_id":"t-2","cwd":"/tmp/p","rollout_path":"/tmp/r.jsonl","cli_version":"0.164.0"}"#),
        ]
        XCTAssertNil(events[1].payload["source"])
        XCTAssertEqual(shape(TimelineBuilder.build(events)), ["you:Fix the build", "notice:Session started"])
    }

    /// The fleet reads the run as it did with the row drawn: Running through the woken turn, done when it ends. The
    /// preview after the woken turn's start is the mail notice, where it was "Session started".
    func testAHostedRunsStatusAndPreviewThroughAWokenTurn() async throws {
        let all = try events("mail-wake-wire")
        let summary = try decoded(SessionSummary.self, "session_hosted_live", in: "mail-wake-wire")
        let expected: [(through: UInt64, status: FleetStatus, preview: String)] = [
            (3, .running, "notice:Session started"),
            (4, .doneUnreviewed, "notice:Turn complete"),
            (7, .running, "notice:Reading CodeConnect mail"),
            (8, .running, "reply:I read my mail."),
            (9, .doneUnreviewed, "notice:Turn complete"),
        ]
        for (through, status, preview) in expected {
            let shown = await state(all.filter { $0.seq <= through }).shown
            XCTAssertEqual(
                FleetStatusRule.status(summary: summary, shown: shown, reviewedSeq: 0), status, "through seq \(through)")
            XCTAssertEqual(shape(shown.timeline.suffix(1)), [preview], "the preview through seq \(through)")
        }
    }

    /// ccd's notice draws the terminal clear's row, once: by its type it read "Conversation Cleared" over
    /// "Conversation cleared".
    func testAHostedClearDrawsTheTerminalClearsRowOnce() async throws {
        let all = try events("hosted-clear-wire")
        let notice = try XCTUnwrap(all.first { $0.notificationType == EngineNotice.conversationCleared })
        let drawn = await state(all)
        XCTAssertEqual(drawn.timeline.filter { $0.seq == notice.seq }.map(\.content), [terminalClearRow])
        XCTAssertEqual(
            shape(drawn.timeline).filter { $0.lowercased().contains("cleared") }, ["notice:Conversation cleared."])
    }
}
