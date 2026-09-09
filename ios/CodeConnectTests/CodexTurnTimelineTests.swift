import XCTest

@testable import CodeConnect

/// **The P0 the operator found on a real phone: a Codex turn ran and the
/// timeline showed only "Turn complete".**
///
/// The words were on the wire the whole time. `ccd` logged `user_message` and
/// `agent_message`; the phone drew neither, because
/// `PayloadViews.userText`/`agentText` were written against **Claude's
/// transcript shape** — `payload.message.content` as a string or a block array
/// — while the Codex adapter emits a flat message:
///
/// ```
/// {"interrupted":false,"text":"Hey"}
/// ```
///
/// (`ccd/src/codex_adapter.rs` `message_payload`, which builds exactly
/// `{"text":…, "interrupted":…}` for `userMessage` and `agentMessage` items.)
///
/// **Why every render and every test stayed green through it.** The Codex
/// fixtures in `Fixtures.swift` were hand-written in the Claude shape
/// (`"message":{"content":[…]}`), so the app was tested against a wire the
/// daemon never sends. That is the defect underneath the defect, and it is why
/// the payloads in this file are copied from the operator's own session (build
/// 73) with nothing changed but the name in the reply.
///
/// Every payload below is a **measured** shape. Nothing here accepts a third
/// spelling on the theory that some daemon might send one.
@MainActor
final class CodexTurnTimelineTests: XCTestCase {

    // MARK: The operator's turn, verbatim

    /// The thread and turn ids from the capture's id grammar:
    /// `<thread>:item:<id>` and `<thread>:turn:<id>` (`codex_adapter.rs` `sid`).
    private static let thread = "01a0127a-c6f4-70d1-b3a3-0742f8fd0d86"
    private static let turn = "01a0127a-d9cd-7461-84d7-6eea6d0b98a5"

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    /// One event of the phone-started turn, in the daemon's envelope: source
    /// `codex`, a `turn_id`, and the `<thread>:…` id grammar.
    private func codexEvent(
        seq: UInt64, kind: String, item: String? = nil, sid: String, payload: String
    ) throws -> Event {
        let itemClause = item.map { #","item_id":"\#($0)""# } ?? ""
        return try event(
            """
            {"seq":\(seq),"session_uid":"u-73","session_id":"cc-1",
             "ts":"2026-09-08T09:14:0\(seq).000Z","kind":"\(kind)","source":"codex",
             "turn_id":"\(Self.turn)","source_event_id":"\(Self.thread):\(sid)"\(itemClause),
             "payload":\(payload)}
            """)
    }

    private func userMessage(text: String = "Hey", interrupted: Bool = false) throws -> Event {
        try codexEvent(
            seq: 1, kind: "user_message", item: "item-user-1", sid: "item:item-user-1",
            payload: #"{"interrupted":\#(interrupted),"text":"\#(text)"}"#)
    }

    private func agentMessage(
        text: String = "Hey Sam! What are we working on today?", interrupted: Bool = false
    ) throws -> Event {
        try codexEvent(
            seq: 3, kind: "agent_message", item: "item-agent-1", sid: "item:item-agent-1",
            payload: #"{"interrupted":\#(interrupted),"text":"\#(text)"}"#)
    }

    /// Empty in the capture — both arrays — which is the whole reason the phone
    /// draws nothing for it. See `testReasoningCarriesNothingToDraw`.
    private func reasoning() throws -> Event {
        try codexEvent(
            seq: 2, kind: "reasoning", item: "item-reason-1", sid: "item:item-reason-1",
            payload: #"{"content":[],"interrupted":false,"summary":[]}"#)
    }

    private func usage() throws -> Event {
        try codexEvent(
            seq: 4, kind: "usage", sid: "usage:\(Self.turn)",
            payload: """
                {"last":{"cacheWriteInputTokens":0,"cachedInputTokens":12928,\
                "inputTokens":23100,"outputTokens":15,"reasoningOutputTokens":0,\
                "totalTokens":23115}}
                """)
    }

    private func turnComplete() throws -> Event {
        try codexEvent(
            seq: 5, kind: "turn_complete", sid: "turn:\(Self.turn)",
            payload: """
                {"completed_at":"2026-09-08T09:14:05.000Z","duration_ms":6385,"error":null,\
                "started_at":"2026-09-08T09:13:58.000Z","status":"completed"}
                """)
    }

    /// `tool_call_payload` in the adapter: `tool`, `command`, `cwd` — not the
    /// hook's `tool_name`/`tool_input`.
    private func toolCall() throws -> Event {
        try codexEvent(
            seq: 6, kind: "tool_call", item: "exec-1", sid: "pre:exec-1",
            payload: """
                {"tool":"command_execution","command":"/bin/zsh -lc 'touch marker.txt'",\
                "cwd":"/work/proj","command_actions":null}
                """)
    }

    // MARK: Readers

    private func userTexts(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .userMessage(let text, _) = $0.content { return text }
            return nil
        }
    }

    private func agentTexts(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .agentMessage(let text, _) = $0.content { return text }
            return nil
        }
    }

    private func noticeTitles(_ items: [TimelineItem]) -> [String] {
        items.compactMap {
            if case .notice(let notice) = $0.content { return notice.title }
            return nil
        }
    }

    private func tools(_ items: [TimelineItem]) -> [ToolItem] {
        items.compactMap {
            if case .tool(let tool) = $0.content { return tool }
            return nil
        }
    }

    // MARK: The regression

    /// **The bug, in one assertion.** Before the fix this timeline held exactly
    /// one row — "Turn complete" — which is what the operator photographed.
    func testAPhoneStartedCodexTurnShowsWhatWasSaid() throws {
        let items = TimelineBuilder.build([
            try userMessage(), try reasoning(), try agentMessage(), try usage(),
            try turnComplete(),
        ])

        XCTAssertEqual(userTexts(items), ["Hey"], "the phone must show what the operator typed")
        XCTAssertEqual(
            agentTexts(items), ["Hey Sam! What are we working on today?"],
            "the phone must show what Codex replied")
        XCTAssertEqual(noticeTitles(items), ["Turn complete"])
    }

    /// The accessors themselves, so a failure says which half broke.
    func testTheFlatCodexMessageShapeReadsAsProse() throws {
        XCTAssertEqual(try userMessage().userText, "Hey")
        XCTAssertEqual(
            try agentMessage().agentText, "Hey Sam! What are we working on today?")
    }

    /// **Claude's shape is untouched.** The fix is a second arm, not a
    /// replacement: a transcript message still reads through `message.content`
    /// as a bare string and as a block array.
    func testClaudesTranscriptShapeStillReads() throws {
        let bare = try event(
            """
            {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-08T09:14:01.000Z",
             "kind":"user_message","source":"transcript","source_event_id":"t:1",
             "payload":{"message":{"content":"what does this repo do?"}}}
            """)
        XCTAssertEqual(bare.userText, "what does this repo do?")

        let blocks = try event(
            """
            {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-08T09:14:02.000Z",
             "kind":"agent_message","source":"transcript","source_event_id":"t:2",
             "payload":{"message":{"content":[{"type":"text","text":"It pairs a phone."}]}}}
            """)
        XCTAssertEqual(blocks.agentText, "It pairs a phone.")
    }

    /// **`interrupted` is a fact the wire states, so the row states it.** The
    /// adapter synthesises a terminal item with `interrupted: true` when a turn
    /// is aborted mid-reply; drawing that reply as if it had finished is the
    /// phone telling the operator something the Mac never said.
    func testAnInterruptedCodexReplyIsMarkedInterrupted() throws {
        let items = TimelineBuilder.build([
            try agentMessage(text: "I'm creating marker.txt in the", interrupted: true)
        ])
        guard case .agentMessage(let text, let isInterrupted) = try XCTUnwrap(items.first).content
        else {
            return XCTFail("the interrupted reply should still be an agent message row")
        }
        XCTAssertEqual(text, "I'm creating marker.txt in the")
        XCTAssertTrue(isInterrupted, "the wire said interrupted; the row must say so too")
    }

    func testAnUninterruptedCodexReplyClaimsNothing() throws {
        let items = TimelineBuilder.build([try agentMessage()])
        guard case .agentMessage(_, let isInterrupted) = try XCTUnwrap(items.first).content else {
            return XCTFail("expected an agent message row")
        }
        XCTAssertFalse(isInterrupted)
    }

    /// A Codex `tool_call` names its tool through `tool`, and carries the
    /// command at the top level. Read through the hook's `tool_name` /
    /// `tool_input` it drew a row labelled "tool" with no argument at all —
    /// visible, and empty.
    func testACodexToolCallCarriesItsCommand() throws {
        let items = TimelineBuilder.build([try toolCall()])
        let tool = try XCTUnwrap(tools(items).first)
        // The card's word for the same family, not the adapter's —
        // `codex_approval.rs` `Family::tool_name`. A row that says
        // `command_execution` over a card that says `command` is the app
        // disagreeing with itself about one call.
        XCTAssertEqual(tool.name, "command")
        XCTAssertEqual(tool.argument, "/bin/zsh -lc 'touch marker.txt'")
    }

    // MARK: Every kind the daemon emits for a Codex turn

    /// **The coverage the fixtures were supposed to give and did not.** One
    /// assertion per event kind `codex_adapter.rs` can emit for a turn, against
    /// the payload it really emits, so no kind can go silently blank again.
    func testEveryKindOfACodexTurnIsAccountedFor() throws {
        let items = TimelineBuilder.build([
            try userMessage(), try reasoning(), try toolCall(), try agentMessage(),
            try usage(), try turnComplete(),
        ])

        XCTAssertEqual(userTexts(items), ["Hey"], "user_message")
        XCTAssertEqual(
            agentTexts(items), ["Hey Sam! What are we working on today?"], "agent_message")
        XCTAssertEqual(tools(items).map(\.name), ["command"], "tool_call")
        XCTAssertEqual(noticeTitles(items), ["Turn complete"], "turn_complete")

        // Four rows and no more: `reasoning` and `usage` are storage, not
        // screen — see the next test for why that is a fact and not a shrug.
        XCTAssertEqual(items.count, 4)
    }

    /// **Reasoning draws nothing because it *carries* nothing.**
    ///
    /// Both arrays are empty in every capture, and `codex_adapter.rs` says so
    /// itself: "Reasoning is stored, not rendered (feature matrix) … the phone
    /// drops it." A collapsed row over an empty summary would be a disclosure
    /// triangle hiding nothing — the app inventing a fact the wire withheld.
    /// Pinned here so the emptiness is a decision on the record rather than an
    /// oversight, and so the day a summary arrives this test is the one that
    /// fails.
    func testReasoningCarriesNothingToDraw() throws {
        let reasoning = try reasoning()
        XCTAssertEqual(reasoning.payload["summary"]?.arrayValue?.isEmpty, true)
        XCTAssertEqual(reasoning.payload["content"]?.arrayValue?.isEmpty, true)
        XCTAssertTrue(TimelineBuilder.build([reasoning]).isEmpty)
    }

    /// `usage` is the token ledger the Deck reads; it has never been a timeline
    /// row and this pins that it still is not, now that the real payload is
    /// under test.
    func testUsageIsALedgerNotARow() throws {
        XCTAssertTrue(TimelineBuilder.build([try usage()]).isEmpty)
    }

    // MARK: The daemon's own capture of a phone-started turn

    /// **`fixtures/codex/phone-turn-stream-0.153.4.json`, decoded whole.**
    ///
    /// The file ccd's own tests byte-compare against: one `compose_result`
    /// followed by nineteen `{"type":"event","event":{…}}` frames exactly as
    /// `ws_server` wraps them — one session over two threads, a message turn
    /// and then a command approval answered by the phone and a file change
    /// answered at the keyboard. It is the only place the two languages meet on
    /// the same bytes for a *turn*, which is the thing that was broken.
    ///
    /// Its `agent_message` carries `text: ""` — the only 0.153 capture there
    /// is — so the rendering assertions above keep using the operator's own
    /// rows. Emptiness is asserted here rather than papered over.
    private func capture() throws -> (compose: JSONValue, events: [Event]) {
        guard
            let url = Bundle(for: type(of: self))
                .url(forResource: "phone-turn-stream-0.153.4", withExtension: "json"),
            let data = try? Data(contentsOf: url)
        else {
            XCTFail(
                "phone-turn-stream-0.153.4.json is not in the test bundle (checked in at "
                    + "ios/CodeConnectTests/Resources/, copied from fixtures/codex/). This is the "
                    + "daemon's own capture of a phone-started turn; it must not be skipped.")
            throw CocoaError(.fileNoSuchFile)
        }
        let root = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
        let frames = try XCTUnwrap(root["frames"]?.arrayValue)
        let compose = try XCTUnwrap(frames.first)
        let events = try frames.dropFirst().map { frame -> Event in
            try XCTUnwrap(frame["event"]?.decoded(Event.self), "every event frame must decode")
        }
        return (compose, Array(events))
    }

    func testTheDaemonsOwnPhoneTurnCaptureDecodesWhole() throws {
        let (compose, events) = try capture()

        // Frame 0 through the app's real `ServerMessage` decoder, not a
        // bespoke struct: what the phone would do with it on the socket.
        XCTAssertEqual(compose["type"]?.stringValue, "compose_result")
        guard case .composeResult(_, let requestID, let result) =
            try XCTUnwrap(compose.decoded(ServerMessage.self))
        else {
            return XCTFail("frame 0 is the compose_result that started the turn")
        }
        XCTAssertEqual(requestID, "say-1")
        XCTAssertEqual(result, .started(turnID: "01a07c5c-6911-7973-aafd-4d5509e27e80"))

        XCTAssertEqual(events.count, 19, "nineteen event frames, all decoded")
        XCTAssertEqual(
            events.map(\.seq), Array(1...19).map(UInt64.init),
            "contiguous seq, as the phone's resync banner demands")
        for event in events where event.kind != .approvalRequest && event.kind != .approvalResolved
        {
            XCTAssertEqual(event.source, .codex, "seq \(event.seq)")
        }
        // Every kind in the file, so this test fails the day the capture grows
        // one the phone has never been shown.
        XCTAssertEqual(
            Set(events.map(\.kind.rawValue)),
            [
                "user_message", "agent_message", "reasoning", "tool_call", "tool_result",
                "approval_request", "approval_resolved", "turn_complete",
            ])
    }

    /// **(b) Every kind in the daemon's capture reaches the screen** — the
    /// assertion the hand-made fixtures could not make, because they were
    /// written in a shape the daemon does not send.
    ///
    /// The tool rows are the half that had not been looked at: a Codex
    /// `tool_call` is not a Claude `tool_use` block, and a Codex `tool_result`
    /// is not a PostToolUse hook. Read through those, the rows did not vanish
    /// the way the messages did — they rendered *empty*, which is the same lie
    /// with a border around it.
    func testEveryKindInTheDaemonsCaptureRendersARow() throws {
        let (_, events) = try capture()
        let items = TimelineBuilder.build(events)

        // The words. The capture's own agent_message is empty, so the user's
        // line is the message row it can prove; `testAPhoneStartedCodexTurn…`
        // above proves the reply with the operator's real text.
        XCTAssertEqual(
            userTexts(items),
            [
                "Run the shell command `touch /work/marker.txt` now. Do not explain, just run it.",
                "Use apply_patch to edit /work/hello.txt, replacing the word hello with goodbye. "
                    + "Do not explain, just do it.",
            ])
        XCTAssertTrue(
            agentTexts(items).isEmpty,
            "the captured agent_message is text:\"\" — an empty row would be worse than none")

        // The tools: three calls, each naming its family and its argument.
        let tools = tools(items)
        XCTAssertEqual(tools.map(\.name), ["command", "command", "file change"])
        XCTAssertEqual(
            tools.map(\.argument),
            [
                "/bin/zsh -lc \'touch /work/marker.txt\'",
                "/bin/zsh -lc \"sed -n \'1,20p\' /work/hello.txt\"",
                "/work/hello.txt",
            ],
            "the command for a command, the path for a file change — never blank")
        XCTAssertEqual(
            tools.map(\.status), [.succeeded, .succeeded, .succeeded],
            "each call's own tool_result, read through the Codex shape")

        // The cards and the endings.
        XCTAssertEqual(
            items.compactMap { item -> Bool? in
                if case .approval(let approval) = item.content { return approval.isPending }
                return nil
            },
            [false, false],
            "both captured cards are resolved — one by the phone, one at the Mac")
        XCTAssertEqual(noticeTitles(items), ["Turn complete", "Turn complete"])

        // Nothing in the capture reaches the screen as nothing: 2 messages +
        // 3 tools + 2 cards + 2 turn endings = 9 rows, and the 5 reasoning
        // items are the documented, empty remainder.
        XCTAssertEqual(items.count, 9)
    }

    /// A non-zero exit is a **failure**, whatever the item's lifecycle status
    /// says. The capture only carries `exit_code: 0`, so the failing half is
    /// pinned here against the same shape.
    func testANonZeroExitCodeFailsTheToolRow() throws {
        let failed = try codexEvent(
            seq: 7, kind: "tool_result", item: "exec-1", sid: "post:exec-1",
            payload: """
                {"tool":"command_execution","status":"completed","interrupted":false,\
                "command":"/bin/zsh -lc 'false'","exit_code":1,"aggregated_output":"boom",\
                "duration_ms":12}
                """)
        let items = TimelineBuilder.build([try toolCall(), failed])
        let tool = try XCTUnwrap(tools(items).first)
        XCTAssertEqual(tool.status, .failed, "exit 1 is not `done`")
        XCTAssertEqual(tool.output, "boom")
        XCTAssertEqual(tool.durationMS, 12)
    }
}
