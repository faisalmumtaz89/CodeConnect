import XCTest

@testable import CodeConnect

/// A Claude turn the person interrupted at the Mac.
///
/// `transcript-interrupts-wire.json` is emitted, not written: three runs Claude
/// Code 2.1.304 recorded against a fake model — Esc while the reply streams,
/// Esc while a tool runs, "No" at a tool's permission prompt — each followed by
/// a later prompt answered in full, through the daemon's production tailer and
/// store as the events a phone is sent (`ccd/src/fixture_replay.rs` re-derives
/// it). An Esc writes Claude's interrupt marker and no turn end at all, ever;
/// the refusal writes the same marker and a `turn_duration` straight after it.
@MainActor
final class InterruptedTurnTests: XCTestCase {

    private func run(_ name: String) throws -> [[String: Any]] {
        try WireRecording.objects("transcript-interrupts-wire", run: name)
    }

    /// The run's events up to and including the one at `seq`.
    private func events(_ name: String, through seq: Int) throws -> [Event] {
        try WireRecording.events("transcript-interrupts-wire", run: name, through: seq)
    }

    private func status(_ events: [Event], reviewedSeq: UInt64 = 0) async throws -> FleetStatus {
        try WireRecording.status(await WireRecording.state(events), reviewedSeq: reviewedSeq)
    }

    private func spoken(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .userMessage(let text, _) = $0.content { return text }
            return nil
        }
    }

    private func notices(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .notice(let notice) = $0.content { return notice.title }
            return nil
        }
    }

    // MARK: The turn ends where the person stopped it

    /// Esc while the reply streams: the partial reply, then the Mac's own row,
    /// and the session is no longer working.
    func testAnEscWhileTheReplyStreamsEndsTheTurn() async throws {
        let interrupted = try events("esc-mid-reply", through: 18)
        let items = TimelineBuilder.build(interrupted)
        XCTAssertEqual(spoken(items), ["LONGREPLY please write a long answer"])
        XCTAssertEqual(notices(items), ["Interrupted · What should Claude do instead?"])
        let result = try await status(interrupted)
        XCTAssertEqual(result, .doneUnreviewed)
        let reviewed = try await status(interrupted, reviewedSeq: 18)
        XCTAssertEqual(reviewed, .idle)
    }

    /// Esc while a tool runs: the same. Claude writes the call's rejection
    /// before the marker (every one of 138 real interrupts did), so the call
    /// reads as failed rather than running.
    func testAnEscWhileAToolRunsEndsTheTurn() async throws {
        let interrupted = try events("esc-during-tool", through: 21)
        let items = TimelineBuilder.build(interrupted)
        XCTAssertEqual(spoken(items), ["SLEEPTOOL please run the sleep"])
        XCTAssertEqual(notices(items), ["Interrupted · What should Claude do instead?"])
        let tools = items.compactMap { item -> ToolItem? in
            if case .tool(let tool) = item.content { return tool }
            return nil
        }
        XCTAssertEqual(tools.first?.name, "Bash")
        XCTAssertEqual(tools.first?.status, .failed)
        XCTAssertFalse(tools.contains { $0.status == .running })
        let result = try await status(interrupted)
        XCTAssertEqual(result, .doneUnreviewed)
    }

    /// "No" at the prompt: the Mac draws the interrupt and then its "done" line,
    /// because Claude wrote a turn end too — so the phone draws both.
    func testARefusedToolEndsTheTurnAsTheMacDrawsIt() async throws {
        let refused = try events("refuse-tool", through: 22)
        let items = TimelineBuilder.build(refused)
        XCTAssertEqual(spoken(items), ["REFUSETOOL please create the marker"])
        XCTAssertEqual(
            notices(items), ["Interrupted · What should Claude do instead?", "Turn complete"])
        let result = try await status(refused, reviewedSeq: 22)
        XCTAssertEqual(result, .idle)
    }

    /// Nothing after the interrupt revives it: a minute later Claude Code's idle
    /// reminder arrives, and the session is still not working.
    func testTheIdleReminderAfterAnInterruptIsNotWork() async throws {
        var objects = try run("esc-mid-reply").filter { ($0["seq"] as? Int ?? .max) <= 18 }
        objects.append([
            "seq": 19, "session_uid": WireRecording.uid, "session_id": "cc-1",
            "ts": "2026-10-20T11:51:00.000Z", "kind": "notification", "source": "hook",
            "payload": [
                "hook_event_name": "Notification", "notification_type": "idle_prompt",
                "message": "Claude is waiting for your input",
            ],
        ])
        let result = try await status(try WireRecording.decode(objects), reviewedSeq: 18)
        XCTAssertEqual(result, .idle)
    }

    // MARK: The next prompt is work again

    func testTheNextPromptIsWorkAgainAndEndsAsUsual() async throws {
        for (name, prompt, end) in [
            ("esc-mid-reply", 20, 24), ("esc-during-tool", 24, 27), ("refuse-tool", 25, 28),
        ] {
            let asked = try await status(try events(name, through: prompt), reviewedSeq: UInt64(prompt))
            XCTAssertEqual(asked, .running, "\(name): the next prompt is work")
            let answered = try await status(try events(name, through: end), reviewedSeq: UInt64(end))
            XCTAssertEqual(answered, .idle, "\(name): its turn ends as usual")
            XCTAssertEqual(
                spoken(TimelineBuilder.build(try events(name, through: end))).last,
                "FOLLOWUP please reply briefly")
        }
    }

    /// Resumed after an Esc, Claude writes its placeholder "No response
    /// requested." with the next prompt. The Mac never draws it; neither does
    /// the phone, and it is never taken for the agent working.
    func testClaudesResumePlaceholderIsNotAReply() async throws {
        let run = try events("resume-after-esc", through: 29)
        var replies: [String] = []
        for item in TimelineBuilder.build(run) {
            if case .agentMessage(let text, _) = item.content { replies.append(text) }
        }
        XCTAssertEqual(
            replies, ["word1 word2 word3 word4 word5 word6 word7", "Short reply: done."])
        let placeholderOnly = try events("resume-after-esc", through: 24)
        let result = try await status(placeholderOnly, reviewedSeq: 24)
        XCTAssertEqual(result, .idle)
        let answered = try await status(run, reviewedSeq: 29)
        XCTAssertEqual(answered, .idle)
    }

    // MARK: Only Claude's own marker

    /// The person's words stay theirs: the marker's text typed as a prompt (a
    /// typed prompt is stamped `origin.kind: "human"`), the text inside a longer
    /// message, and the marker beside anything else in the same line.
    func testOnlyClaudesOwnMarkerIsAnInterrupt() throws {
        var marker = try XCTUnwrap(try run("esc-mid-reply").first { $0["seq"] as? Int == 18 })
        var payload = try XCTUnwrap(marker["payload"] as? [String: Any])
        func withPayload(_ edit: (inout [String: Any]) -> Void) throws -> Event {
            var changed = payload
            edit(&changed)
            marker["payload"] = changed
            return try WireRecording.decode([marker])[0]
        }
        let typed = try withPayload { $0["origin"] = ["kind": "human"] }
        XCTAssertEqual(spoken(TimelineBuilder.build([typed])), ["[Request interrupted by user]"])

        let quoted = try withPayload {
            $0["message"] = [
                "role": "user",
                "content": [["type": "text", "text": "why did I see [Request interrupted by user]?"]],
            ]
        }
        XCTAssertEqual(
            spoken(TimelineBuilder.build([quoted])), ["why did I see [Request interrupted by user]?"])

        let besideMore = try withPayload {
            $0["message"] = [
                "role": "user",
                "content": [
                    ["type": "text", "text": "[Request interrupted by user]"],
                    ["type": "text", "text": "and then this"],
                ],
            ]
        }
        XCTAssertEqual(
            spoken(TimelineBuilder.build([besideMore])), ["[Request interrupted by user]\nand then this"])

        payload["message"] = ["role": "user", "content": "[Request interrupted by user]"]
        marker["payload"] = payload
        XCTAssertEqual(
            spoken(TimelineBuilder.build(try WireRecording.decode([marker]))),
            ["[Request interrupted by user]"],
            "Claude writes the marker as a text block, never as a bare string")
    }
}
