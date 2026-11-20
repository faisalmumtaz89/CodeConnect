import XCTest

@testable import CodeConnectKit

/// **A hosted Claude run draws its start, its clear, its commands and its turn ends as a terminal run does.**
///
/// The agent host files a `session_start` for every init claude sends. ccd stamps a launch's first `startup` and a
/// clear's `clear`; a later turn's start carries no source and draws no row. A Codex start never carries a source (ccd's
/// Codex adapter files the thread's folder, rollout and CLI version only), so it keeps its row. ccd's own
/// `conversation_cleared` notice on a hosted clear draws the row a terminal run's `/clear` draws, once. A command an app
/// sends draws a command row, and its own turn end, like the end after "Interrupted", draws none.
///
/// Pinned to `fixtures/claude/mail-wake-wire.json` (a later turn's start, seq 7), `hosted-clear-wire.json` (a clear),
/// `sdk-session-events.json` (the daemon's events for a real session), `denied-call-wire.json` (an allowed call, then a
/// denied one) and `transcript-interrupts-wire.json` (a terminal run's refused tool), each copied byte for byte.
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

    private func event(_ seq: UInt64, _ kind: String, _ payload: String, source: String) throws -> Event {
        try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "ts":"2026-08-02T10:00:\(String(format: "%02d", seq))Z","kind":"\(kind)",
                 "payload":\(payload),"source":"\(source)"}
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
            try event(1, "user_message", #"{"text":"Fix the build"}"#, source: "codex"),
            try event(
                2, "session_start",
                #"{"thread_id":"t-2","cwd":"/tmp/p","rollout_path":"/tmp/r.jsonl","cli_version":"0.164.0"}"#,
                source: "codex"),
        ]
        XCTAssertNil(events[1].payload["source"])
        XCTAssertEqual(shape(TimelineBuilder.build(events)), ["you:Fix the build", "notice:Session started"])
    }

    /// **Decision 227: a hosted command whose end carries no `local_command` draws no stray "Turn complete".**
    /// A declined `/help` (seq 8, a plain person-row since ccd marks no command) draws claude's "Command output"
    /// (seq 10), then a turn end with no `local_command` (seq 11). The output row already ends the turn on
    /// screen, so no "Turn complete" follows — two rows, not three. `/cost` (16) and `/config` (21) carry
    /// `local_command` and were already silent. From the daemon's `hosted-clear-wire.json`. Must-fail: today the
    /// stray "Turn complete" shows.
    func testADeclinedCommandsTurnEndDrawsNoTurnCompleteRow() async throws {
        let session = await state(try events("hosted-clear-wire"))
        // `drawn` is the display transform; the item stays in `timeline` for fleet status.
        let helpRows = shape(TimelineBuilder.drawn(session.timeline).filter { (8...12).contains($0.seq) })
        XCTAssertEqual(
            helpRows, ["you:/help", "notice:Command output"],
            "a declined /help draws its message and its Command output, no stray Turn complete")
        XCTAssertFalse(
            helpRows.contains("notice:Turn complete"),
            "a turn end directly after a Command output row draws none")
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

    /// A hosted compaction (`fixtures/claude/sdk-session-events.json`, the daemon's events for a real session, seq 84)
    /// draws the row a terminal compaction's start draws, without the model the notice does not carry: by its type it
    /// read "Compacted" over "Conversation compacted".
    func testAHostedCompactionDrawsTheTerminalCompactionsRow() async throws {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "sdk-session-events", withExtension: "json"))
        let all = try JSONDecoder().decode([Event].self, from: Data(contentsOf: url))
        let notice = try XCTUnwrap(all.first { $0.kind == .notification && $0.notificationType == "compacted" })
        let drawn = await state(all)
        guard case .notice(let row)? = drawn.timeline.first(where: { $0.seq == notice.seq })?.content else {
            return XCTFail("the compaction drew no row: \(shape(drawn.timeline))")
        }
        let terminal = TimelineBuilder.build([
            try event(1, "user_message", #"{"text":"Fix the build"}"#, source: "hook"),
            try event(2, "session_start", #"{"hook_event_name":"SessionStart","source":"compact"}"#, source: "hook"),
        ])
        guard case .notice(let compacted)? = terminal.last?.content else { return XCTFail("\(shape(terminal))") }
        XCTAssertEqual([row.title, row.symbol], [compacted.title, compacted.symbol])
        XCTAssertEqual(
            row,
            NoticeItem(
                kind: .sessionStart, symbol: "play-circle", title: "Conversation compacted", detail: nil,
                severity: .info))
    }

    /// An effort an app set (`fixtures/claude/minor-39-wire.json`'s `effort_notice`, ccd's payload) draws in the effort
    /// menu's words: by its type it read "Model Set" over "Effort set to high".
    func testAnEffortAnAppSetDrawsItsMessage() throws {
        let payload = try XCTUnwrap(try wire("minor-39-wire")["effort_notice"], "no effort_notice in minor-39-wire.json")
        let notice = try event(2, "notification", payload.canonicalJSONString, source: "daemon")
        XCTAssertEqual(notice.notificationType, "model_set")
        XCTAssertEqual(
            TimelineBuilder.build([notice]).map(\.content),
            [.notice(NoticeItem(kind: .other, symbol: "gauge", title: "Effort set to High", detail: nil, severity: .info))])
    }

    // MARK: A command's row and its own end; a turn's end after "Interrupted"

    /// The events of a recording kept as a plain list.
    private func recorded(_ name: String) throws -> [Event] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try JSONDecoder().decode([Event].self, from: Data(contentsOf: url))
    }

    private var liveRun: SessionSummary {
        get throws { try decoded(SessionSummary.self, "session_hosted_live", in: "hosted-clear-wire") }
    }

    /// `hosted-clear-wire`'s s3, the person's `/clear` as ccd files it, made `/compact`'s, on `run`'s session: ccd names
    /// any of claude's commands the same way (`command`, the first word's name).
    private func compactSent(_ seq: UInt64, on run: Event) throws -> Event {
        let clear = try XCTUnwrap(try events("hosted-clear-wire").first { $0.seq == 3 })
        var payload = try XCTUnwrap(clear.payload.objectValue)
        XCTAssertEqual(payload["command"], .string("clear"))
        payload["command"] = .string("compact")
        payload["text"] = .string("/compact")
        return try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_uid":"\(run.sessionUID)","session_id":"\(run.sessionID)",
                 "ts":"\(run.ts)","kind":"user_message",
                 "payload":\(JSONValue.object(payload).canonicalJSONString),"source":"daemon"}
                """.utf8))
    }

    /// A `/clear` an app sends (s3) draws no row of its own: ccd's `conversation_cleared` (s4) is the one row, as a
    /// terminal clear draws the notice alone. It read "you: /clear" above it.
    func testAClearAnAppSentDrawsOnlyTheClearedRow() async throws {
        let shown = await state(try events("hosted-clear-wire"))
        XCTAssertEqual(shape(shown.timeline.filter { $0.seq == 3 }), [], "the person's /clear draws no row")
        XCTAssertEqual(
            shape(TimelineBuilder.drawn(shown.timeline).filter { (3...7).contains($0.seq) }), ["notice:Conversation cleared."])
    }

    /// Any other command an app sends draws as a terminal run's invocation does, a command row, and starts its turn:
    /// sent after a finished turn, the run reads Running until the command's own end (the test above). It was the
    /// person's row.
    func testAnyOtherCommandAnAppSentIsACommandRowAndStartsItsTurn() async throws {
        let finished = try recorded("denied-call-wire").filter { $0.seq <= 8 }
        let run = try XCTUnwrap(finished.first)
        let shown = await state(finished + [try compactSent(9, on: run)]).shown
        XCTAssertEqual(shown.timeline.last?.content, .userMessage("/compact", isCommand: true))
        XCTAssertEqual(FleetStatusRule.status(summary: try liveRun, shown: shown, reviewedSeq: 8), .running)
        XCTAssertEqual(shape(shown.timeline.suffix(1)), ["you:/compact"], "the preview")
    }

    /// A command's own turn end (`local_command`: the recorded session's `/compact` s86, `/effort low` s98, `/clear`
    /// s102) draws no row, as a terminal run files none. The item stays: the run reads done until seen, then idle, and
    /// the preview is the turn's end, as after any turn. Each drew "Turn complete".
    func testACommandsOwnTurnEndDrawsNoRowButEndsTheTurnForTheFleet() async throws {
        let all = try recorded("sdk-session-events")
        for end in [UInt64(86), 98, 102] {
            XCTAssertNotNil(try XCTUnwrap(all.first { $0.seq == end }).payload["local_command"], "s\(end)")
            let shown = await state(all.filter { $0.seq <= end }).shown
            XCTAssertEqual(shape(shown.timeline.filter { $0.seq == end }), ["notice:Turn complete"], "s\(end)'s item")
            XCTAssertEqual(TimelineBuilder.drawn(shown.timeline).filter { $0.seq == end }, [], "s\(end) draws no row")
            XCTAssertEqual(
                FleetStatusRule.status(summary: try liveRun, shown: shown, reviewedSeq: 0), .doneUnreviewed, "s\(end)")
            XCTAssertEqual(FleetStatusRule.status(summary: try liveRun, shown: shown, reviewedSeq: end), .done, "s\(end)")
            XCTAssertEqual(shape(shown.timeline.suffix(1)), ["notice:Turn complete"], "the preview through s\(end)")
        }
    }

    /// A turn's end directly after "Interrupted" draws no row: the recorded session's Stop (s64-65) and its denied call
    /// (s70-71: "rejected before it ran", s75) and `denied-call-wire`'s denied call (s36-37) end on "Interrupted · What
    /// should Claude do instead?", as an Esc does at a terminal. The item stays: Running while the call runs, done once
    /// it ends. Each drew "Turn complete" under it.
    func testATurnsEndAfterInterruptedDrawsNoRow() async throws {
        let session = try recorded("sdk-session-events")
        let denied = try recorded("denied-call-wire")
        for (events, interrupt, end) in [(session, UInt64(64), UInt64(65)), (session, 70, 71), (denied, 36, 37)] {
            let running = await state(events.filter { $0.seq < interrupt }).shown
            XCTAssertEqual(
                FleetStatusRule.status(summary: try liveRun, shown: running, reviewedSeq: 0), .running, "s\(end)")
            let shown = await state(events.filter { $0.seq <= end }).shown
            let rows = TimelineBuilder.drawn(shown.timeline)
            XCTAssertEqual(
                shape(rows.filter { $0.seq == interrupt }), ["notice:Interrupted · What should Claude do instead?"])
            XCTAssertEqual(rows.filter { $0.seq == end }, [], "s\(end) draws no row")
            XCTAssertEqual(shape(shown.timeline.filter { $0.seq == end }), ["notice:Turn complete"], "s\(end)'s item")
            XCTAssertEqual(
                FleetStatusRule.status(summary: try liveRun, shown: shown, reviewedSeq: 0), .doneUnreviewed, "s\(end)")
            XCTAssertEqual(shape(shown.timeline.suffix(1)), ["notice:Turn complete"], "the preview through s\(end)")
        }
        // The allowed call's turn ends on Claude's reply, and that end draws no row, as before.
        let allowed = await state(denied.filter { $0.seq <= 8 })
        XCTAssertEqual(shape(TimelineBuilder.drawn(allowed.timeline).suffix(1)), ["reply:done"])
    }

    /// At a terminal a refused tool's end follows the same "Interrupted" row (`transcript-interrupts-wire`'s
    /// `refuse-tool`, s21-22), so it draws none either; it drew "Turn complete".
    func testATerminalRefusedToolsEndDrawsNoRow() async throws {
        let refused = try decoded([Event].self, "refuse-tool", in: "transcript-interrupts-wire")
        let shown = await state(refused)
        XCTAssertEqual(
            shape(TimelineBuilder.drawn(shown.timeline).filter { (21...22).contains($0.seq) }),
            ["notice:Interrupted · What should Claude do instead?"])
        XCTAssertEqual(shape(shown.timeline.filter { $0.seq == 22 }), ["notice:Turn complete"], "the item stays")
    }
}
