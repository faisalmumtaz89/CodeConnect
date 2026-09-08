import XCTest

@testable import CodeConnect

/// **Tier 1.2 — a Codex `approval_resolved` must resolve its card.**
///
/// The highest-value test in the phase, and the one that stands in front of the
/// worst defect the gap list found: before it, a Codex card stayed **live and
/// tappable on the phone** after it had been answered at the Mac, cleared by an
/// interrupt, or timed out. `Timeline` wrote resolutions into
/// `outcomesByRequest` via `AnswerOutcome`, which requires `request_id`; a Codex
/// resolution payload is a bare `CodexResolution`, which carried none; so the
/// decode returned nil, nothing was recorded, and `actionBarAvailable` stayed
/// true forever.
///
/// **Decision D1, and where the fixtures stand.** The daemon adds `request_id`
/// to the Codex `approval_resolved` payload — additively, in the same spelling
/// and position as Claude's. The phone correlates on that field **only**; it
/// never prefix-parses `source_event_id`, because parsing an id-bearing string
/// is exactly the thing that fails silently.
///
/// Every resolution frame below was written by hand against that decision, and
/// has since been **cross-checked against the Rust that landed it**:
/// `ws.rs`'s `CodexResolutionPayload` is `{ request_id, #[serde(flatten)]
/// resolution }`, which puts `request_id` beside the status tag rather than
/// inside a nested object — exactly the flat shape these read. No capture in
/// `fixtures/codex/` carries it yet (every one predates the change), so the
/// hand-written frames here are the only exercise it gets on this side until
/// T4 runs against a live daemon.
///
/// `aResolutionWithoutARequestIdCorrelatesToNothing` keeps the fallback honest:
/// it pins what a pre-D1 frame does, which is nothing at all.
@MainActor
final class CodexResolutionTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    /// The Codex approval card as the daemon really raises it:
    /// `source: "daemon"` (never `"hook"`), `source_event_id: "perm:<rid>"`,
    /// **no** `turn_id`, **no** `item_id`, and a payload with one key.
    private func codexCard(requestID: String = "rid-1", seq: UInt64 = 1) throws -> Event {
        let input = """
            {"command":"/bin/zsh -lc 'touch marker.txt'","cwd":"/work",\
            "options":[{"id":"accept","label":"Yes, proceed"},\
            {"id":"cancel","label":"No, and tell Codex what to do differently"}]}
            """
        let display = "command\n\(input)"
        return try event(
            """
            {"seq":\(seq),"session_uid":"u-1","session_id":"cc-1",
             "ts":"2026-09-04T21:31:59.000Z","kind":"approval_request","source":"daemon",
             "source_event_id":"perm:\(requestID)",
             "payload":{"card":{"request_id":"\(requestID)",
               "payload_hash":"\(Self.hash(of: display))","tool_name":"command",
               "tool_input":\(input),"display_text":\(Self.quoted(display)),
               "generation":1,"identity_bound":false,"risk":{"class":"medium"}}}}
            """)
    }

    /// A Codex resolution: a **bare `CodexResolution`** as the payload, with
    /// D1's additive `request_id` beside the status. Not wrapped in an
    /// `AnswerOutcome`, and carrying none of its fields.
    private func codexResolution(
        requestID: String = "rid-1", seq: UInt64 = 2, body: String
    ) throws -> Event {
        try event(
            """
            {"seq":\(seq),"session_uid":"u-1","session_id":"cc-1",
             "ts":"2026-09-04T21:32:09.000Z","kind":"approval_resolved","source":"daemon",
             "source_event_id":"resolved:\(requestID)",
             "payload":{"request_id":"\(requestID)",\(body)}}
            """)
    }

    private func approval(in timeline: [TimelineItem]) -> ApprovalItem? {
        for item in timeline {
            if case .approval(let approval) = item.content { return approval }
        }
        return nil
    }

    // MARK: The fact that makes the rest necessary

    /// A Codex resolution payload is not an `AnswerOutcome` and never will be:
    /// it carries no `session_id`, no `decision`, no `resolved_by`, no
    /// `applied_via`, no `resolved_at`. The Claude path reads it as nil, which
    /// is why a second arm has to exist at all.
    func testACodexResolutionIsNotAnAnswerOutcome() throws {
        let resolved = try codexResolution(body: #""status":"timeout""#)
        XCTAssertNil(
            resolved.approvalOutcome,
            "the Claude path cannot read a Codex resolution, and must not pretend to")
        XCTAssertNotNil(resolved.codexResolution)
    }

    /// **The regression, end to end, through the real `TimelineBuilder`.**
    func testACodexResolutionResolvesItsCard() throws {
        let timeline = TimelineBuilder.build([
            try codexCard(),
            try codexResolution(
                body: #""status":"answered","by":"local""#),
        ])
        let card = try XCTUnwrap(approval(in: timeline))
        XCTAssertEqual(card.codexResolution, .answered(by: .local, decision: nil))
        XCTAssertFalse(card.isPending, "an answered card is not still waiting on a human")
        XCTAssertFalse(
            DecisionCardView.actionBarAvailable(
                outcome: card.outcome, codex: card.codexResolution, attempt: nil, isBacked: true),
            "a resolved Codex card must not offer an answer")
    }

    /// Every terminal the wire can deliver retires the card. Written as a sweep
    /// because the arms are what a reader distinguishes, and a builder that
    /// handled one of them would look correct in a spot-check.
    func testEveryCodexTerminalRetiresTheCard() throws {
        let bodies = [
            #""status":"answered","by":"phone","decision":{"type":"option_id","option_id":"accept"}"#,
            #""status":"answered","by":"local""#,
            #""status":"cleared","cause":"turn_aborted""#,
            #""status":"cleared","cause":"turn_completed""#,
            #""status":"cleared","cause":"superseded""#,
            #""status":"cleared","cause":"item_completed""#,
            #""status":"timeout""#,
            #""status":"unknown","attempted_by":"phone","write_stage":"upstream_write_unconfirmed","cause":"the wire died before the ack"#
                + #"""#,
        ]
        // **One pass over the eight**, asserting everything the builder decides
        // about each: the ending is recorded, the card stops waiting on a human,
        // and the row says which kind of ending it was. It was two sweeps of the
        // same eight bodies through the same builder, in two files.
        for (body, header) in zip(
            bodies, ["RESOLVED", "RESOLVED", "RETIRED", "RETIRED", "RETIRED", "RETIRED",
                     "RETIRED", "UNCONFIRMED"])
        {
            let timeline = TimelineBuilder.build([
                try codexCard(),
                try codexResolution(body: body),
            ])
            let card = try XCTUnwrap(approval(in: timeline), body)
            XCTAssertNotNil(card.codexResolution, body)
            XCTAssertFalse(card.isPending, body)
            XCTAssertEqual(ApprovalRow.headerTitle(for: card), header, body)
        }
    }

    /// **`source_event_id` is never parsed.** The contract's open question 4.1
    /// offered a `"resolved:"` prefix parse as the alternative; D1 chose the
    /// field. This pins the choice: a resolution whose `source_event_id` says
    /// `resolved:rid-1` but whose payload carries no `request_id` correlates to
    /// nothing, and the card stays live rather than being retired by a string
    /// that happened to start with the right eight characters.
    func testAResolutionWithoutARequestIdCorrelatesToNothing() throws {
        let unaugmented = try event(
            """
            {"seq":2,"session_uid":"u-1","session_id":"cc-1",
             "ts":"2026-09-04T21:32:09.000Z","kind":"approval_resolved","source":"daemon",
             "source_event_id":"resolved:rid-1",
             "payload":{"status":"timeout"}}
            """)
        let timeline = TimelineBuilder.build([try codexCard(), unaugmented])
        let card = try XCTUnwrap(approval(in: timeline))
        XCTAssertNil(
            card.codexResolution,
            "no request_id is no correlation — the prefix is not a substitute for the field")
        XCTAssertTrue(card.isPending)
    }

    /// A resolution for a different card leaves this one alone. The trap a
    /// prefix parse would have walked into is the same one a sloppy field read
    /// would: correlation is **exact-string equality**, on ids that are 169
    /// characters of base64url and share long prefixes across visit generations.
    func testAResolutionForAnotherCardDoesNotRetireThisOne() throws {
        let timeline = TimelineBuilder.build([
            try codexCard(requestID: "rid-1"),
            try codexResolution(requestID: "rid-2", body: #""status":"timeout""#),
        ])
        let card = try XCTUnwrap(approval(in: timeline))
        XCTAssertNil(card.codexResolution)
        XCTAssertTrue(card.isPending, "somebody else's resolution is not an answer to this")
    }

    /// **This phase must not regress Claude.** The `AnswerOutcome` path is
    /// byte-identical and still wins where both could apply.
    func testAClaudeResolutionIsUnaffected() throws {
        let request = try event(
            """
            {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-04T21:31:59.000Z",
             "kind":"approval_request","source":"hook","payload":{"card":{
               "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
               "tool_input":{"command":"ls"},"display_text":"Bash"}}}
            """)
        let resolved = try event(
            """
            {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-04T21:32:09.000Z",
             "kind":"approval_resolved","source":"hook","payload":{
               "request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
               "resolved_by":"phone","applied_via":"send_keys",
               "resolved_at":"2026-09-04T21:32:09.000Z","inferred":false}}
            """)
        let timeline = TimelineBuilder.build([request, resolved])
        let card = try XCTUnwrap(approval(in: timeline))
        XCTAssertEqual(card.outcome?.decision, .allow)
        XCTAssertEqual(card.outcome?.appliedVia, .sendKeys)
        XCTAssertNil(card.codexResolution, "a Claude resolution is not a Codex one")
        XCTAssertFalse(card.isPending)
    }

    /// The ordering trap the test plan names S3: a `turn_complete` arrives
    /// **before** the resolution. Neither event may leave a live card on a dead
    /// turn, and the resolution is what retires it either way.
    func testATurnCompleteBeforeTheResolutionStillRetiresTheCard() throws {
        let turnComplete = try event(
            """
            {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-04T21:32:05.000Z",
             "kind":"turn_complete","source":"codex","turn_id":"t-1",
             "payload":{"status":"interrupted","duration_ms":4200}}
            """)
        let timeline = TimelineBuilder.build([
            try codexCard(),
            turnComplete,
            try codexResolution(seq: 3, body: #""status":"cleared","cause":"turn_aborted""#),
        ])
        let card = try XCTUnwrap(approval(in: timeline))
        XCTAssertEqual(card.codexResolution, .cleared(cause: .turnAborted))
        XCTAssertFalse(card.isPending)
    }

    // MARK: The daemon's own fixture

    /// **`fixtures/codex/minor-19-wire.json`, decoded whole.**
    ///
    /// The daemon's tests byte-compare against this file, so it is the one place
    /// the two languages meet on the same bytes rather than on two readings of a
    /// decisions document. Everything the phone needs from minor 19 is in it —
    /// D1's `request_id` on the resolution payload, D2's `turn_id` on the
    /// approval's *envelope*, D3's `codex_link` and `agent` on the summaries —
    /// and this decodes all of it through the app's own types and drives the
    /// real `TimelineBuilder` over it.
    ///
    /// The hand-written frames elsewhere in this file are kept, and they are not
    /// redundant: this fixture carries one card, one ending and one link state,
    /// and the eight terminals, the four clear causes and the four link words
    /// each need their own row.
    func testTheDaemonsOwnMinor19FixtureDecodesWhole() throws {
        // **From the test bundle.** `#filePath` baked this machine's checkout
        // path into the binary, so on CI or a fresh clone the daemon's own bytes
        // were simply absent — and a fixture that cannot be found is a fixture
        // that stops binding the two languages, silently. The file is checked in
        // at `ios/CodeConnectTests/Resources/` and ships with the tests.
        guard
            let url = Bundle(for: type(of: self))
                .url(forResource: "minor-19-wire", withExtension: "json"),
            let data = try? Data(contentsOf: url)
        else {
            XCTFail(
                "minor-19-wire.json is not in the test bundle (checked in at "
                    + "ios/CodeConnectTests/Resources/). This is the only test that decodes the "
                    + "daemon's own bytes through the app's types; it must not be skipped.")
            return
        }
        let root = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: data).objectValue)

        // ---- D3: the fleet ------------------------------------------------
        let sessions = try XCTUnwrap(root["sessions"]?.arrayValue)
            .compactMap { $0.decoded(SessionSummary.self) }
        XCTAssertEqual(sessions.count, 2, "both summaries decode")
        let codex = try XCTUnwrap(sessions.first { $0.agent == .codex })
        XCTAssertEqual(codex.codexLink, .subscribed)
        XCTAssertTrue(codex.codexLink.canActuate(.compose))
        XCTAssertNotNil(codex.codexThreadID)
        let claude = try XCTUnwrap(sessions.first { $0.agent == .claude })
        XCTAssertEqual(
            claude.codexLink, CodexLinkState.none,
            "a Claude row says `none` explicitly — the field is never skipped")
        XCTAssertFalse(claude.codexLink.canActuate(.compose))

        // ---- D2: the turn is on the approval's ENVELOPE --------------------
        let request = try XCTUnwrap(root["approval_request"]?.decoded(Event.self))
        XCTAssertEqual(request.kind, .approvalRequest)
        XCTAssertEqual(request.source, .daemon, "a Codex card is `daemon`, never `hook`")
        XCTAssertEqual(
            request.turnID, "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee",
            "D2: the card's own turn, so Stop needs no inference at all")
        let card = try XCTUnwrap(request.approvalCard)
        XCTAssertTrue(
            card.verification.hashMatchesDisplayText, "the real card's hash gate holds")
        XCTAssertEqual(
            CodexCard.options(in: card.toolInput).map(\.id),
            ["accept", "acceptWithExecpolicyAmendment", "cancel"])

        // ---- D1: the resolution correlates on `request_id` -----------------
        let resolved = try XCTUnwrap(root["approval_resolved"]?.decoded(Event.self))
        XCTAssertNil(
            resolved.approvalOutcome, "still not an AnswerOutcome, and never will be")
        XCTAssertEqual(resolved.codexResolvedRequestID, card.requestID)
        XCTAssertEqual(
            resolved.codexResolution,
            .answered(by: .phone, decision: .optionId("accept")))

        // ---- and the two together, through the real builder ----------------
        let timeline = TimelineBuilder.build([request, resolved])
        let item = try XCTUnwrap(approval(in: timeline))
        XCTAssertFalse(item.isPending, "the daemon's own bytes retire the card")
        XCTAssertEqual(ApprovalRow.headerTitle(for: item), "RESOLVED")

        // ---- D2 again, through the tracker the Stop control reads ----------
        XCTAssertEqual(
            CodexTurnTracker.runningTurn(in: [request]),
            "01a06db1-2223-7ed0-8be5-1d7e3ccdeaee",
            "the card's envelope turn is picked up with no inference")
    }

    // MARK: The row that shows it

    // `testTheRowHeaderReflectsACodexEnding` lived here and walked the same
    // eight terminals through the same builder a second time. Its one real
    // assertion — the row's header word — is now made in
    // `testEveryCodexTerminalRetiresTheCard`, in the same pass that already had
    // the card in hand. The defect it was written for (the row said `NEEDS YOU`
    // on a card answered at the Mac, because `ApprovalRow` read `outcome`
    // alone) is covered there.

    /// The Claude header words are byte-identical. This phase adds a branch; it
    /// does not move the existing one.
    func testTheClaudeRowHeaderIsUnchanged() throws {
        func item(indeterminate: Bool) throws -> ApprovalItem {
            let request = try event(
                """
                {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-04T21:31:59.000Z",
                 "kind":"approval_request","source":"hook","payload":{"card":{
                   "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
                   "tool_input":{"command":"ls"},"display_text":"Bash"}}}
                """)
            let resolved = try event(
                """
                {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-04T21:32:09.000Z",
                 "kind":"approval_resolved","source":"hook","payload":{
                   "request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
                   "resolved_by":"phone","applied_via":"send_keys",
                   "resolved_at":"2026-09-04T21:32:09.000Z","inferred":false,
                   "indeterminate":\(indeterminate)}}
                """)
            return try XCTUnwrap(approval(in: TimelineBuilder.build([request, resolved])))
        }
        XCTAssertEqual(ApprovalRow.headerTitle(for: try item(indeterminate: false)), "RESOLVED")
        XCTAssertEqual(ApprovalRow.headerTitle(for: try item(indeterminate: true)), "UNCONFIRMED")
    }

    // MARK: The running turn (gap G11)

    /// **The one honest hide.** `interrupt` requires a `turn_id`; the approval
    /// card carries none, and `turn/started` maps to no event at all — so the
    /// phone's only source is the envelope of a preceding item event. Stop is
    /// hidden when there is no such turn, because that is a fact the phone
    /// genuinely holds rather than a guess about the Mac.
    func testTheRunningTurnIsTakenFromTheEventEnvelope() throws {
        func item(seq: UInt64, kind: String, turn: String?) throws -> Event {
            let turnClause = turn.map { #","turn_id":"\#($0)""# } ?? ""
            return try event(
                """
                {"seq":\(seq),"session_uid":"u-1","session_id":"cc-1",
                 "ts":"2026-09-04T21:3\(seq):00.000Z","kind":"\(kind)","source":"codex"\
                \(turnClause),"payload":{}}
                """)
        }

        XCTAssertNil(
            CodexTurnTracker.runningTurn(in: [try item(seq: 1, kind: "user_message", turn: nil)]),
            "an event with no turn on its envelope names no turn")

        let started = [
            try item(seq: 1, kind: "user_message", turn: "t-1"),
            try item(seq: 2, kind: "tool_call", turn: "t-1"),
        ]
        XCTAssertEqual(CodexTurnTracker.runningTurn(in: started), "t-1")

        // A matching `turn_complete` ends it — and only a *matching* one.
        let completed =
            started + [try item(seq: 3, kind: "turn_complete", turn: "t-1")]
        XCTAssertNil(CodexTurnTracker.runningTurn(in: completed))

        let otherCompleted =
            started + [try item(seq: 3, kind: "turn_complete", turn: "t-other")]
        XCTAssertEqual(
            CodexTurnTracker.runningTurn(in: otherCompleted), "t-1",
            "another turn's terminal says nothing about this one")

        // A second turn after the first completed is the running one.
        let second =
            completed + [try item(seq: 4, kind: "reasoning", turn: "t-2")]
        XCTAssertEqual(CodexTurnTracker.runningTurn(in: second), "t-2")
    }

    /// **A turn the events ended cannot be brought back by what a mutation
    /// once observed.**
    ///
    /// The observation exists because a compose is handed a turn id seconds
    /// before the first item event for it arrives, so `observed` fills a real
    /// gap. But it was consulted whenever the event scan ended with no
    /// candidate — and "no candidate" is also what a matching `turn_complete`
    /// produces. So the sequence compose→`Started{T1}`→`turn_complete(T1)`
    /// re-offered Stop for a turn the Mac had already finished, and a late
    /// `Steered{T1}` arriving after the terminal did the same.
    ///
    /// The fix is to say which turns ended, not merely that nothing is running:
    /// terminals are collected, and an observation naming one of them is not an
    /// observation of anything that can still be stopped.
    func testAnEndedTurnIsNotResurrectedByAnObservation() throws {
        func item(seq: UInt64, kind: String, turn: String?) throws -> Event {
            let turnClause = turn.map { #","turn_id":"\#($0)""# } ?? ""
            return try event(
                """
                {"seq":\(seq),"session_uid":"u-1","session_id":"cc-1",
                 "ts":"2026-09-04T21:3\(seq):00.000Z","kind":"\(kind)","source":"codex"\
                \(turnClause),"payload":{}}
                """)
        }

        // (a) The compose observed T1; then the events ended T1.
        let ended = [
            try item(seq: 1, kind: "user_message", turn: "t-1"),
            try item(seq: 2, kind: "turn_complete", turn: "t-1"),
        ]
        XCTAssertNil(
            CodexTurnTracker.runningTurn(in: ended, observed: ["t-1"]),
            "the compose's turn was completed by the events; there is nothing to stop")

        // (b) The terminal arrived first and the compose answer was late. Order
        // of arrival must not decide whether a finished turn looks alive.
        XCTAssertNil(
            CodexTurnTracker.runningTurn(in: ended, observed: ["t-1", "t-1"]),
            "a late Steered{T1} after T1's terminal names an ended turn")

        // (c) A genuinely later turn still wins: the exclusion is per turn id,
        // not a blanket refusal to trust observations.
        let thenT2 = ended + [try item(seq: 3, kind: "reasoning", turn: "t-2")]
        XCTAssertEqual(
            CodexTurnTracker.runningTurn(in: thenT2, observed: ["t-1"]), "t-2")
        XCTAssertEqual(
            CodexTurnTracker.runningTurn(in: ended, observed: ["t-1", "t-2"]), "t-2",
            "an observation of a turn no terminal names is still the best fact there is")
    }

    /// The tracker reads the envelope, never a payload. A Claude session's
    /// events carry `turn_id` too (`item_id` correlation), and its
    /// `turn_complete` is a `session_end` carrying `hook_event_name: "Stop"` —
    /// but nothing in the app offers Stop for a Claude session, and this is
    /// scoped by `agent` at every call site rather than by the tracker.
    func testTheTrackerIsSourceAgnosticAndTheAgentScopeLivesAtTheCallSite() throws {
        let claudeish = try event(
            """
            {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-09-04T21:31:00.000Z",
             "kind":"tool_call","source":"hook","turn_id":"t-9","payload":{}}
            """)
        XCTAssertEqual(
            CodexTurnTracker.runningTurn(in: [claudeish]), "t-9",
            "the tracker reports what the envelope says; who may act on it is decided elsewhere")
    }

    // MARK: Helpers

    private static func quoted(_ value: String) -> String {
        JSONValue.string(value).canonicalJSONString
    }

    /// The card's own gate, run forwards.
    ///
    /// This used to call a `CodexTestHash` helper that recomputed
    /// `CardVerification`'s exact expression — so a card it built was guaranteed
    /// to pass the gate the test was meant to exercise. Asking the production
    /// type is the only version that can fail.
    private static func hash(of text: String) -> String {
        var probe = ApprovalCard(
            requestID: "", payloadHash: "", toolName: "", toolInput: .null,
            displayText: text, permissionSuggestions: nil, promptID: nil,
            permissionMode: nil, risk: nil)
        // `verification` compares SHA-256(displayText) to `payloadHash`; binary
        // search is absurd, so the hash is read out of the type that computes it.
        probe = ApprovalCard(
            requestID: "", payloadHash: probe.sha256OfDisplayText, toolName: "",
            toolInput: .null, displayText: text, permissionSuggestions: nil,
            promptID: nil, permissionMode: nil, risk: nil)
        precondition(probe.verification.hashMatchesDisplayText)
        return probe.payloadHash
    }
}
