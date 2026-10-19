import XCTest

@testable import CodeConnect

/// Which `type:"user"` lines are the person's words, and how a background
/// task's report draws.
///
/// `transcript-user-lines-wire.json` is emitted, not written: every user line of
/// a recorded Claude Code 2.1.290 transcript, through the daemon's production
/// tailer and store, as the events a phone is sent (`ccd/src/fixture_replay.rs`
/// re-derives it). The variations below start from one of those events and
/// change only the fields named, taking each shape from the owner's own
/// transcripts as Claude Code 2.1.304 wrote them.
final class TaskNotificationTests: XCTestCase {

    private func wire() throws -> [[String: Any]] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(
                forResource: "transcript-user-lines-wire", withExtension: "json"),
            "transcript-user-lines-wire.json is not in the test bundle (checked in at "
                + "ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    }

    private func decode(_ objects: [[String: Any]]) throws -> [Event] {
        try objects.map {
            try JSONDecoder().decode(
                Event.self, from: JSONSerialization.data(withJSONObject: $0))
        }
    }

    /// The emitted event at `seq`, its payload changed by `edit`.
    private func event(
        seq: Int, _ edit: (inout [String: Any]) -> Void = { _ in }
    ) throws -> Event {
        var object = try XCTUnwrap(try wire().first { $0["seq"] as? Int == seq })
        var payload = try XCTUnwrap(object["payload"] as? [String: Any])
        edit(&payload)
        object["payload"] = payload
        return try decode([object])[0]
    }

    private func setContent(_ content: Any) -> (inout [String: Any]) -> Void {
        { payload in payload["message"] = ["role": "user", "content": content] }
    }

    private func spoken(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .userMessage(let text, _) = $0.content { return text }
            return nil
        }
    }

    private func notices(_ items: [TimelineItem]) -> [NoticeItem] {
        items.compactMap {
            if case .notice(let notice) = $0.content { return notice }
            return nil
        }
    }

    // MARK: The recording

    /// The ten typed prompts are the only YOU rows; the agent's report is one
    /// notice reading as the Mac's row does; the three lines written for the
    /// model draw nothing.
    func testTheRecordedUserLinesDrawAsTheMacDrawsThem() throws {
        let items = TimelineBuilder.build(try decode(try wire()))
        XCTAssertEqual(
            spoken(items),
            [
                "SCEN:p1 goSCEN:p1 go", "SCEN:tool go", "SCEN:q go", "SCEN:mend go",
                "SCEN:blk go", "SCEN:blkmcp go", "SCEN:err go", "SCEN:slow go", "SCEN:bga go",
                "SCEN:p1 go",
            ])
        XCTAssertEqual(
            notices(items),
            [
                NoticeItem(
                    kind: .taskReport, symbol: "circle.fill",
                    title: "Agent \"Background helper\" finished · 4s", detail: nil,
                    severity: .success)
            ])
    }

    /// Older stamping cannot be relied on, and the TUI does not rely on it:
    /// unstamped, the report is still read by its shape.
    func testAnUnstampedReportIsStillAReport() throws {
        let items = TimelineBuilder.build([
            try event(seq: 91) { $0["origin"] = nil; $0["promptSource"] = nil }
        ])
        XCTAssertEqual(spoken(items), [])
        XCTAssertEqual(
            notices(items).map(\.title), ["Agent \"Background helper\" finished · 4s"])
    }

    func testAReportWithoutASummaryDrawsNothing() throws {
        let items = TimelineBuilder.build([
            try event(
                seq: 91,
                setContent(
                    "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n"
                        + "</task-notification>"))
        ])
        XCTAssertEqual(items, [])
    }

    /// A report wakes the agent: after a finished turn the session is working
    /// again, and the fleet must say Running, not Done or Idle, until the agent
    /// first answers — measured, 6.1s at the median and over 30s for 845 of
    /// 13,885 real report-started turns.
    @MainActor
    func testAReportAfterAFinishedTurnIsWork() async throws {
        // The emitted prompt and report, renumbered to follow one another.
        let renumbered = { (seq: Int, newSeq: Int) -> Event in
            var object = try XCTUnwrap(try self.wire().first { $0["seq"] as? Int == seq })
            object["seq"] = newSeq
            return try self.decode([object])[0]
        }
        let turnEnd = try JSONDecoder().decode(
            Event.self,
            from: Data(
                #"""
                {"seq":2,"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "ts":"2026-10-06T20:30:00.000Z","kind":"turn_complete","payload":{"hook_event_name":"Stop"},
                 "source":"hook"}
                """#.utf8))
        let state = SessionState(sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        for event in [try renumbered(81, 1), turnEnd, try renumbered(91, 3)] {
            state.ingest(event)
        }
        await state.settleForTesting()
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                #"""
                {"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "tmux_session":"cc-1","cwd":"/scratch/work","lifecycle":"live",
                 "link":"attached","last_seq":3,"created_at":"2026-10-06T20:29:00Z",
                 "updated_at":"2026-10-06T20:30:00Z","blocked_on":[]}
                """#.utf8))
        XCTAssertEqual(
            FleetStatusRule.status(summary: summary, shown: state.shown, reviewedSeq: 2),
            .running)

        // Recorded without being handed to the agent, the same report is history:
        // drawn, and the session stays idle (measured: 6h 11m idle after one).
        let recorded = SessionState(sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        var report = try XCTUnwrap(try wire().first { $0["seq"] as? Int == 91 })
        report["seq"] = 3
        var payload = try XCTUnwrap(report["payload"] as? [String: Any])
        payload["queueTranscriptOnly"] = true
        report["payload"] = payload
        for event in [try renumbered(81, 1), turnEnd, try decode([report])[0]] {
            recorded.ingest(event)
        }
        await recorded.settleForTesting()
        XCTAssertEqual(
            notices(recorded.timeline).last?.title,
            "Agent \"Background helper\" finished · 4s")
        XCTAssertEqual(
            FleetStatusRule.status(summary: summary, shown: recorded.shown, reviewedSeq: 2),
            .idle)
    }

    // MARK: What is not a report

    /// The literal tag mid-text is not a report: 486 `/loop` prompts carry it.
    /// Written for the model, such a prompt draws nothing; typed by the person,
    /// it is theirs, verbatim — and so is one that starts with the tag, because
    /// the TUI reads a line stamped human as human whatever it says.
    func testOnlyAWholeReportIsRead() throws {
        let quoted = "When a <task-notification> arrives, check the result."
        let scheduled = try event(seq: 91) {
            $0["origin"] = nil
            $0["isMeta"] = true
            self.setContent(quoted)(&$0)
        }
        XCTAssertEqual(TimelineBuilder.build([scheduled]), [])

        let typed = try event(seq: 5, setContent(quoted))
        XCTAssertEqual(spoken(TimelineBuilder.build([typed])), [quoted])

        let report = "<task-notification><summary>x</summary></task-notification>"
        let pasted = try event(seq: 5, setContent(report))
        XCTAssertEqual(spoken(TimelineBuilder.build([pasted])), [report])

        let trailing = try event(
            seq: 91, setContent("<task-notification><summary>x</summary></task-notification> and"))
        XCTAssertEqual(TimelineBuilder.build([trailing]), [], "not a report, and not the person's")
    }

    /// Lines Claude Code writes for the model and the Mac never draws as the
    /// person's words: another session's message, a resumed usage limit, the
    /// summary that continues a compacted conversation.
    func testLinesWrittenForTheModelAreNotThePersons() throws {
        let peer = try event(seq: 5) {
            $0["isMeta"] = true
            $0["origin"] = ["kind": "peer"]
            $0["promptSource"] = "system"
            self.setContent("Another Claude session sent a message:\n<cross-session-message>")(&$0)
        }
        let continued = try event(seq: 5) {
            $0["isMeta"] = true
            $0["origin"] = ["kind": "auto-continuation"]
            self.setContent("Your claude.ai usage limit has reset. Continue the task.")(&$0)
        }
        let compacted = try event(seq: 5) {
            $0["origin"] = nil
            $0["isCompactSummary"] = true
            $0["isVisibleInTranscriptOnly"] = true
            self.setContent("This session is being continued from a previous conversation.")(&$0)
        }
        XCTAssertEqual(TimelineBuilder.build([peer, continued, compacted]), [])
    }

    /// Every way the person's own words arrive stays theirs.
    func testThePersonsWordsStayTheirs() throws {
        let queued = try event(seq: 5) { $0["promptSource"] = "queued" }
        let suggestion = try event(seq: 5) { $0["promptSource"] = "suggestion_accepted" }
        let sdk = try event(seq: 5) {
            $0["origin"] = nil
            $0["promptSource"] = "sdk"
            self.setContent("Reply with exactly the word PINEAPPLE.")(&$0)
        }
        let blocks = try event(
            seq: 5, setContent([["type": "text", "text": "what is this?"]]))
        XCTAssertEqual(
            spoken(TimelineBuilder.build([queued, suggestion, sdk, blocks])),
            ["SCEN:p1 goSCEN:p1 go", "SCEN:p1 goSCEN:p1 go",
             "Reply with exactly the word PINEAPPLE.", "what is this?"])

        // Codex's flat shape (`ccd/src/codex_adapter.rs` `message_payload`).
        let codex = try event(seq: 5) { $0 = ["text": "run the tests", "interrupted": false] }
        XCTAssertEqual(spoken(TimelineBuilder.build([codex])), ["run the tests"])
    }

    /// Local commands are read before anything else, exactly as before.
    func testLocalCommandsAreUnchanged() throws {
        let clear = try event(
            seq: 5,
            setContent(
                "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n"
                    + "<command-args></command-args>"))
        XCTAssertEqual(notices(TimelineBuilder.build([clear])).map(\.title), ["Conversation cleared."])
        let caveat = try event(seq: 5) {
            $0["isMeta"] = true
            self.setContent("<local-command-caveat>Caveat: generated locally.</local-command-caveat>")(&$0)
        }
        XCTAssertEqual(TimelineBuilder.build([caveat]), [])
    }

    // MARK: The row

    /// Claude Code 2.1.304's own format, rounding and carries included.
    func testTheTimeReadsAsTheMacWritesIt() {
        let cases: [(Int, String)] = [
            (4079, "4s"), (57000, "57s"), (59_999, "59s"), (60000, "1m 0s"),
            (119_500, "2m 0s"), (243_112, "4m 3s"), (3_599_500, "1h 0m 0s"),
            (3_723_000, "1h 2m 3s"), (90_061_000, "1d 1h 1m"),
        ]
        for (ms, expected) in cases {
            XCTAssertEqual(TaskNotification.elapsed(ms: ms), expected, "\(ms)ms")
        }
    }

    func testTheRowCarriesTheTimeOnlyWhenThereIsOne() {
        let agent = TaskNotification(summary: "Agent \"a\" finished", status: "completed", durationMS: 243_112)
        XCTAssertEqual(agent.line, "Agent \"a\" finished · 4m 3s")
        let command = TaskNotification(
            summary: "Background command \"b\" completed (exit code 0)", status: "completed",
            durationMS: nil)
        XCTAssertEqual(command.line, "Background command \"b\" completed (exit code 0)")
        XCTAssertEqual(
            TaskNotification(summary: "s", status: nil, durationMS: 0).line, "s")
        XCTAssertNil(TaskNotification(summary: "", status: nil, durationMS: 5).line)
    }

    /// Decoded as the TUI decodes: the three entities, once.
    func testTheSummaryIsDecodedAsTheMacDecodesIt() {
        func summary(_ raw: String) -> String? {
            TaskNotification.parse(
                "<task-notification><summary>\(raw)</summary></task-notification>")?.summary
        }
        XCTAssertEqual(summary("ls 2&gt;/dev/null &amp;&amp; echo &lt;ok&gt;"), "ls 2>/dev/null && echo <ok>")
        XCTAssertEqual(summary("&amp;gt;"), "&gt;", "one pass, not two")
        XCTAssertEqual(summary("a & b &quot;c&quot;"), "a & b &quot;c&quot;")
        XCTAssertEqual(summary("Agent \"日本語 ☕\" finished"), "Agent \"日本語 ☕\" finished")
    }

    func testTheDotTakesTheStatusColour() {
        func severity(_ status: String?) -> NoticeSeverity {
            TaskNotification(summary: "s", status: status, durationMS: nil).severity
        }
        XCTAssertEqual(severity("completed"), .success)
        XCTAssertEqual(severity("failed"), .failure)
        XCTAssertEqual(severity("killed"), .warning)
        XCTAssertEqual(severity("stopped"), .info)
        XCTAssertEqual(severity(nil), .info)
    }
}
