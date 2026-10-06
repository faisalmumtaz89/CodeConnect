import XCTest

@testable import CodeConnect

/// How one Claude turn reads on the timeline: the reply's text, the notices
/// around it, and what the session's status says about it. The events are in the
/// shapes ccd logs for a 2.1.289 run that asked one `AskUserQuestion`.
@MainActor
final class TimelineTurnTests: XCTestCase {

    private func event(_ seq: UInt64, _ kind: String, _ payload: String, source: String = "hook")
        -> Event
    {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "ts":"2026-08-02T10:00:\(String(format: "%02d", seq))Z","kind":"\(kind)",
                 "payload":\(payload),"source":"\(source)"}
                """.utf8))
    }

    /// A transcript text block, written as JSON so the escapes are the wire's.
    private func reply(_ seq: UInt64, _ text: String) -> Event {
        // swiftlint:disable:next force_try
        let data = try! JSONEncoder().encode(["type": "text", "text": text])
        let block = String(decoding: data, as: UTF8.self)
        return event(
            seq, "agent_message", #"{"message":{"content":[\#(block)]}}"#, source: "transcript")
    }

    private func replies(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .agentMessage(let text, _) = $0.content { return text }
            return nil
        }
    }

    // MARK: The reply's text

    func testAReplyLosesTheBlankLinesClaudePutsAroundIt() {
        let items = TimelineBuilder.build([reply(1, "\n\nYou picked **Blue**. 🟦\n")])
        XCTAssertEqual(replies(items), ["You picked **Blue**. 🟦"])
    }

    func testAWhitespaceOnlyBlockDrawsNoRow() {
        XCTAssertTrue(TimelineBuilder.build([reply(1, "\n\n"), reply(2, " \n\t\n")]).isEmpty)
    }

    func testInsideTheReplyNothingChanges() {
        let text = "    indented first line\n\nsecond paragraph\n\n```\n  code\n\n  more\n```"
        XCTAssertEqual(replies(TimelineBuilder.build([reply(1, "\n\n" + text + "\n\n")])), [text])
    }

    func testACodexFlatReplyIsTrimmedTheSameWay() {
        let items = TimelineBuilder.build([
            event(1, "agent_message", #"{"text":"\n\nDone.\n"}"#, source: "codex"),
            event(2, "agent_message", #"{"text":"\n"}"#, source: "codex"),
        ])
        XCTAssertEqual(replies(items), ["Done."])
    }

    // MARK: A turn's events

    private func user(_ seq: UInt64, _ text: String = "Ask me my favourite colour") -> Event {
        event(
            seq, "user_message", #"{"message":{"role":"user","content":"\#(text)"}}"#,
            source: "transcript")
    }

    /// The `Stop` hook, naming the reply the turn ended on, as Claude Code's
    /// hook does.
    private func stop(_ seq: UInt64) -> Event {
        event(
            seq, "turn_complete",
            #"{"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"Done."}"#)
    }

    private func idle(_ seq: UInt64) -> Event {
        event(
            seq, "notification",
            #"{"hook_event_name":"Notification","message":"Claude is waiting for your input","notification_type":"idle_prompt"}"#)
    }

    /// One label per row, in order, so a wrong order reads as a wrong list.
    private func shape(_ items: [TimelineItem]) -> [String] {
        items.map { item in
            switch item.content {
            case .userMessage: return "you"
            case .agentMessage(let text, _): return "reply:\(text)"
            case .tool(let tool): return "tool:\(tool.name)"
            case .approval: return "card"
            case .notice(let notice): return "notice:\(notice.title)"
            }
        }
    }

    // MARK: The idle reminder

    func testTheIdleReminderIsNeutralAndSaysItOnce() throws {
        let items = TimelineBuilder.build([user(1), reply(2, "Done."), stop(3), idle(4)])
        guard case .notice(let notice)? = items.last?.content else {
            return XCTFail("\(shape(items))")
        }
        XCTAssertEqual("\(notice.kind)", "agentIdle", "its own kind, so status can tell it from a request")
        XCTAssertEqual(notice.title, "Claude is waiting for your input")
        XCTAssertNil(notice.detail, "the title already says it")
        XCTAssertEqual(notice.severity, .info, "amber means it needs you; a finished turn does not")
    }

    /// The next prompt answers it. It is replaced, not added to, so the
    /// "N new" pill counts the prompt and nothing else.
    func testTheNextPromptRetiresTheIdleReminder() {
        let before = TimelineBuilder.build([user(1), reply(2, "Done."), stop(3), idle(4)])
        let after = TimelineBuilder.build([user(1), reply(2, "Done."), stop(3), idle(4), user(5)])
        XCTAssertEqual(
            shape(after), ["you", "reply:Done.", "notice:Turn complete", "you"])
        XCTAssertEqual(after.count, before.count, "the prompt replaces the reminder in the count")
    }

    /// A real request for input is not routine and stays a warning.
    func testARequestForInputStaysAWarning() {
        let items = TimelineBuilder.build([
            event(
                1, "notification",
                #"{"message":"Claude needs your input","notification_type":"agent_needs_input"}"#),
            user(2),
        ])
        guard case .notice(let notice)? = items.first?.content else {
            return XCTFail("\(shape(items))")
        }
        XCTAssertEqual(notice.kind, .agentWaiting)
        XCTAssertEqual(notice.severity, .warning)
    }

    // MARK: Rows that told the reader nothing

    func testASessionsStartAndItsFirstAttachmentDrawNoRow() {
        let items = TimelineBuilder.build([
            event(1, "link_state", #"{"link":"attached","reason":"supervisor registered"}"#, source: "daemon"),
            event(2, "session_start", #"{"hook_event_name":"SessionStart","model":"claude-opus-5[1m]"}"#),
            user(3),
        ])
        XCTAssertEqual(shape(items), ["you"])
    }

    /// The first start is the top of the timeline; a later one is the run
    /// starting again under the reader, and says so, with the model it is on.
    func testAStartAfterTheFirstIsShown() async {
        let restart = event(
            5, "session_start",
            #"{"hook_event_name":"SessionStart","source":"resume","model":"claude-opus-5[1m]"}"#)
        let events = [
            event(1, "session_start", #"{"hook_event_name":"SessionStart","source":"startup"}"#),
            user(2), reply(3, "Done."), stop(4), restart,
        ]
        let items = TimelineBuilder.build(events)
        XCTAssertEqual(
            shape(items), ["you", "reply:Done.", "notice:Turn complete", "notice:Session resumed"])
        guard case .notice(let notice)? = items.last?.content else { return XCTFail("\(shape(items))") }
        XCTAssertEqual(notice.detail, "Opus 5 · 1M context")
        let result = await status(events)
        XCTAssertEqual(result, .idle, "a start is not work: the turn before it ended")
    }

    /// A cold open loads only the newest events, so the first start in view
    /// is not necessarily the session's own: here it restarts under work the
    /// reader can see, and then sits at the top of history not yet loaded.
    func testAStartIsHiddenOnlyAtTheTopOfTheWholeHistory() {
        let restart = event(44, "session_start", #"{"hook_event_name":"SessionStart","source":"startup"}"#)
        XCTAssertEqual(
            shape(TimelineBuilder.build([user(41), reply(42, "Done."), stop(43), restart])),
            ["you", "reply:Done.", "notice:Turn complete", "notice:Session started"])
        XCTAssertEqual(
            shape(TimelineBuilder.build([restart, user(45)])), ["notice:Session started", "you"])
    }

    /// Whether a start is the session's own is decided by the events before
    /// it, not by the rows: the idle reminder above it is retired by the next
    /// prompt, and the start row must not go with it. Anything of the
    /// conversation before it makes it a restart.
    func testTheSessionsOwnStartIsDecidedByTheEventsBeforeIt() {
        let resume = event(
            2, "session_start", #"{"hook_event_name":"SessionStart","source":"resume"}"#)
        XCTAssertEqual(
            shape(TimelineBuilder.build([idle(1), resume])),
            ["notice:Claude is waiting for your input"])
        XCTAssertEqual(shape(TimelineBuilder.build([idle(1), resume, user(3)])), ["you"])
        let late = event(
            4, "session_start", #"{"hook_event_name":"SessionStart","source":"resume"}"#)
        XCTAssertEqual(
            shape(TimelineBuilder.build([user(1), reply(2, "Done."), stop(3), late])),
            ["you", "reply:Done.", "notice:Turn complete", "notice:Session resumed"])
    }

    /// Claude Code says what started it (2.1.289: startup, resume, clear,
    /// compact, fork). A compaction or a resume started nothing new, and
    /// `/clear` already has its own row.
    func testALaterStartSaysWhatStartedIt() {
        func start(_ seq: UInt64, _ source: String) -> Event {
            event(seq, "session_start", #"{"hook_event_name":"SessionStart","source":"\#(source)"}"#)
        }
        let clear = user(
            6,
            "<command-name>/clear</command-name>\\n            <command-message>clear</command-message>\\n            <command-args></command-args>")
        let items = TimelineBuilder.build([
            start(1, "startup"), user(2), start(3, "compact"), start(4, "resume"), start(5, "startup"),
            clear, start(7, "clear"),
        ])
        XCTAssertEqual(
            shape(items),
            [
                "you", "notice:Conversation compacted", "notice:Session resumed",
                "notice:Session started", "notice:Conversation cleared.",
            ])
    }

    /// `/clear` takes an optional name (2.1.289: `argumentHint:"[name]"`), and
    /// its aliases `/new` and `/reset` are recorded under the command's own
    /// name, so every one of them is a `/clear` invocation.
    func testAClearWithANameStillSaysTheConversationWasCleared() {
        let clear = user(
            2,
            "<command-name>/clear</command-name>\\n            <command-message>clear</command-message>\\n            <command-args>next-task</command-args>")
        let items = TimelineBuilder.build([
            user(1), clear,
            event(3, "session_start", #"{"hook_event_name":"SessionStart","source":"clear"}"#),
        ])
        XCTAssertEqual(shape(items), ["you", "notice:Conversation cleared."])
    }

    func testALinkThatDropsStillSaysSoAndSaysWhenItCameBack() {
        let items = TimelineBuilder.build([
            event(1, "link_state", #"{"link":"attached","reason":"supervisor registered"}"#, source: "daemon"),
            event(2, "link_state", #"{"link":"detached","reason":"supervisor gone"}"#, source: "daemon"),
            event(3, "link_state", #"{"link":"attached","reason":"supervisor registered"}"#, source: "daemon"),
        ])
        XCTAssertEqual(shape(items), ["notice:Link detached", "notice:Link attached"])
    }

    // MARK: What the session's status says

    private func summary() -> SessionSummary {
        // swiftlint:disable:next force_try
        return try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "tmux_session":"cc-1","cwd":"/tmp/x","lifecycle":"live",
                 "link":"attached","last_seq":9,"created_at":"2026-08-02T10:00:00Z",
                 "updated_at":"2026-08-02T10:00:00Z","blocked_on":[]}
                """.utf8))
    }

    private func status(_ events: [Event], reviewedSeq: UInt64 = 99) async -> FleetStatus {
        let state = SessionState(sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        for event in events { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return FleetStatusRule.status(summary: summary(), state: state, reviewedSeq: reviewedSeq)
    }

    /// ccd records the turn's reply before its `Stop`, so the turn's end is
    /// the last row and the session is not running.
    func testAFinishedTurnIsNotRunning() async {
        let events = [user(1), reply(2, "Done."), stop(3)]
        XCTAssertEqual(
            shape(TimelineBuilder.build(events)), ["you", "reply:Done.", "notice:Turn complete"])
        let reviewed = await status(events)
        XCTAssertEqual(reviewed, .idle)
        let unreviewed = await status(events, reviewedSeq: 0)
        XCTAssertEqual(unreviewed, .doneUnreviewed)
    }

    func testTheIdleReminderDoesNotMakeAFinishedTurnRunning() async {
        let result = await status([user(1), reply(2, "Done."), stop(3), idle(4)])
        XCTAssertEqual(result, .idle)
        let unreviewed = await status([user(1), reply(2, "Done."), stop(3), idle(4)], reviewedSeq: 0)
        XCTAssertEqual(unreviewed, .doneUnreviewed)
    }
}
