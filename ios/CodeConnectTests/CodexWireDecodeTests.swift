import CryptoKit
import XCTest

@testable import CodeConnect

/// **Tier 1.1 — the Codex wire, decoded through the app's own `Codable` types.**
///
/// Hand-written JSON where `mac/protocol/src/ws.rs` pins a string, and the real
/// captured bytes where `fixtures/codex/*` holds them. Never a round trip
/// through this app's own encoder: a round trip passes just as happily when both
/// sides are wrong together, which is the exact failure a two-language protocol
/// has to be defended against.
///
/// **What the fixture corpus actually is**, stated once so nobody re-derives it:
/// `fixtures/codex/*.jsonl` is overwhelmingly the *upstream app-server's*
/// JSON-RPC — frames tagged `"conn":"ccd-…"` carrying a `method` like
/// `item/commandExecution/requestApproval`. Those are the Mac's business; they
/// never reach a phone and they are not `ServerMessage`s. Exactly one file
/// carries CodeConnect's own phone wire: `compose-0.153.4.jsonl`, whose four
/// `"conn":"phone"` lines are a real `compose` / `compose_result` exchange.
/// `theCorpusStatesWhichFramesArePhoneWire` proves that split rather than
/// asserting it in prose.
final class CodexWireDecodeTests: XCTestCase {

    // MARK: Fixture access

    /// **From the test bundle, not from the source tree.**
    ///
    /// These were read through `#filePath`, which bakes this machine's checkout
    /// path into the binary: on CI, a fresh clone elsewhere, or a relocated
    /// build, the file is simply not there. The captures are checked in under
    /// `CodeConnectTests/Resources/`, so the synchronized group ships them
    /// inside the test bundle and the bytes travel with the tests that read
    /// them.
    private func fixture(_ name: String) throws -> Data {
        let bundle = Bundle(for: type(of: self))
        let base = (name as NSString).deletingPathExtension
        let ext = (name as NSString).pathExtension
        guard let url = bundle.url(forResource: base, withExtension: ext),
            let data = try? Data(contentsOf: url)
        else {
            XCTFail(
                "\(name) is not in the test bundle. It is checked in at "
                    + "ios/CodeConnectTests/Resources/\(name); these tests decode the daemon's "
                    + "own bytes and must never be skipped.")
            throw CocoaError(.fileNoSuchFile)
        }
        return data
    }

    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        try JSONDecoder().decode(type, from: Data(json.utf8))
    }

    // MARK: The captured cards

    /// Both real cards, straight out of the capture, through `ApprovalCard`.
    private func capturedCards() throws -> [(key: String, card: ApprovalCard)] {
        let object = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: fixture("approval-card-0.153.json"))
                .objectValue)
        return try object.keys.sorted().map { key in
            let card = try XCTUnwrap(
                object[key]?.decoded(ApprovalCard.self), "\(key) did not decode as an ApprovalCard")
            return (key, card)
        }
    }

    func testEveryCapturedCardDecodes() throws {
        let cards = try capturedCards()
        XCTAssertEqual(cards.map(\.key), ["command", "file_change"])

        let byName = Dictionary(uniqueKeysWithValues: cards.map { ($0.key, $0.card) })
        // `Family::tool_name`, not `Family::as_str`: the wire says `command` and
        // `file change`, never `commandExecution` / `fileChange`.
        XCTAssertEqual(byName["command"]?.toolName, "command")
        XCTAssertEqual(byName["file_change"]?.toolName, "file change")

        for (key, card) in cards {
            XCTAssertNil(card.promptID, "\(key): a Codex card carries no prompt_id")
            XCTAssertNil(card.permissionMode, "\(key): a Codex card carries no permission_mode")
            XCTAssertNil(
                card.permissionSuggestions,
                "\(key): a Codex card carries no permission_suggestions")
            XCTAssertEqual(card.risk?.cls, "medium", "\(key): the daemon classified it")
            XCTAssertEqual(card.payloadHash.count, 64, "\(key): payload_hash is 64 hex chars")
        }
    }

    /// The gate the app already runs on every card, against the real bytes.
    func testTheCardHashGateHoldsOnRealCards() throws {
        for (key, card) in try capturedCards() {
            XCTAssertTrue(
                card.verification.hashMatchesDisplayText,
                "\(key): SHA-256(display_text) must equal payload_hash")
        }
    }

    /// `"{tool_name}\n{tool_input}"` reproduces `display_text`, so the card
    /// renders structured rather than falling back to the daemon's raw text.
    func testTheDisplayTextIsToolNamePlusToolInput() throws {
        for (key, card) in try capturedCards() {
            XCTAssertTrue(
                card.verification.renderMatchesDisplayText,
                "\(key): the structured render must reproduce display_text")
        }
    }

    /// The option table is where a Codex card's answer comes from, so it has to
    /// survive the decode with its ids intact and opaque.
    func testTheCapturedCardsCarryTheirOptionIds() throws {
        let byName = Dictionary(
            uniqueKeysWithValues: try capturedCards().map { ($0.key, $0.card) })

        XCTAssertEqual(
            CodexCard.options(in: byName["command"]?.toolInput).map(\.id),
            ["accept", "acceptWithExecpolicyAmendment", "cancel"])
        XCTAssertEqual(
            CodexCard.options(in: byName["file_change"]?.toolInput).map(\.id),
            ["accept", "acceptForSession", "cancel"])
    }

    // MARK: The five wire strings (gap G2)

    /// **Fails before this phase.** `ClearCause` had no `itemCompleted`, so the
    /// one cause that means *the turn is still running* fell into the unknown
    /// bucket beside a cause that means the opposite.
    func testItemCompletedIsNotTurnCompleted() throws {
        let resolution = try decode(
            CodexResolution.self, #"{"status":"cleared","cause":"item_completed"}"#)
        guard case .cleared(let cause) = resolution else {
            return XCTFail("expected .cleared, got \(resolution)")
        }
        XCTAssertEqual(cause, .itemCompleted)
        XCTAssertNotEqual(cause, .turnCompleted, "the turn is still running")
        if case .unknown = cause { XCTFail("item_completed must be a named case") }
    }

    /// All four `ClearCause` strings ws.rs pins by hand, because the phone
    /// matches on them.
    func testEveryClearCauseDecodes() throws {
        let expected: [(String, ClearCause)] = [
            ("turn_aborted", .turnAborted),
            ("turn_completed", .turnCompleted),
            ("superseded", .superseded),
            ("item_completed", .itemCompleted),
        ]
        for (wire, cause) in expected {
            let resolution = try decode(
                CodexResolution.self, #"{"status":"cleared","cause":"\#(wire)"}"#)
            XCTAssertEqual(resolution, .cleared(cause: cause), wire)
        }
    }

    /// **Fails before this phase** (D5). Without the named case every successful
    /// Codex answer rendered through `AnswerPath.unknown`, whose whole contract
    /// is that it never claims an actuation — so a confirmed answer read as
    /// unvouchable.
    func testCodexResponseIsANamedAnswerPath() throws {
        XCTAssertEqual(try decode(AnswerPath.self, #""codex_response""#), .codexResponse)
        XCTAssertEqual(AnswerPath(wire: "codex_response"), .codexResponse)
        XCTAssertEqual(AnswerPath.codexResponse.rawValue, "codex_response")
    }

    /// D5's other half: an `applied_via` this build has never heard of still
    /// decodes, into `.unknown`, and never fails the frame.
    func testAnUnknownAnswerPathIsRetainedNotAFrameFailure() throws {
        XCTAssertEqual(try decode(AnswerPath.self, #""teleported""#), .unknown("teleported"))
    }

    /// The whole `answer_result` a Codex answer really produces
    /// (`state.rs` `answer_codex`): `resolved_by: phone`,
    /// `applied_via: codex_response`.
    func testACodexAnswerResultDecodesWholeAndNamesItsPath() throws {
        let message = try decode(
            ServerMessage.self,
            """
            {"type":"answer_result","request_id":"rid","result":{"status":"applied",
             "outcome":{"request_id":"rid","session_id":"cc-1",
               "decision":{"type":"option_id","option_id":"accept"},
               "resolved_by":"phone","applied_via":"codex_response",
               "resolved_at":"2026-09-04T21:31:59.000Z","inferred":false}}}
            """)
        guard case .answerResult(let requestID, .applied(let outcome)) = message else {
            return XCTFail("expected an applied answer_result, got \(message)")
        }
        XCTAssertEqual(requestID, "rid")
        XCTAssertEqual(outcome.appliedVia, .codexResponse)
        XCTAssertEqual(outcome.decision, .optionId("accept"))
    }

    /// `source: "codex"` is what every Codex timeline event carries
    /// (`event.rs`). It must be a named provenance, never the trusted `.daemon`
    /// and never an unknown the timeline discards.
    func testCodexIsANamedEventSource() throws {
        XCTAssertEqual(try decode(EventSource.self, #""codex""#), .codex)
        XCTAssertEqual(EventSource.codex.rawValue, "codex")
    }

    /// An item type this build has never seen arrives as `codex_<type>`. It must
    /// round-trip as itself.
    func testUnknownCodexItemKindsSurvive() throws {
        let kind = try decode(EventKind.self, #""codex_webSearch""#)
        XCTAssertEqual(kind, .other("codex_webSearch"))
        XCTAssertEqual(kind.rawValue, "codex_webSearch")
    }

    /// **Fails before this phase.** `SessionSummary` had no `agent`, and
    /// `ProtocolTests.testSessionSummaryIgnoresAnUnexpectedAgentKey` pinned the
    /// ignoring. `agent.rs`: absent ⇒ `claude`; an unknown *present* string is
    /// retained, never coerced to Claude.
    func testAnUnknownAgentIsNeverClaude() throws {
        func summary(_ agentClause: String) throws -> SessionSummary {
            try decode(
                SessionSummary.self,
                """
                {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1","cwd":"/x",
                 "lifecycle":"live","link":"attached","last_seq":3,
                 "created_at":"t","updated_at":"t"\(agentClause)}
                """)
        }
        XCTAssertEqual(try summary("").agent, .claude, "an absent agent is claude")
        XCTAssertEqual(try summary(#","agent":"claude""#).agent, .claude)
        XCTAssertEqual(try summary(#","agent":"codex""#).agent, .codex)
        XCTAssertEqual(
            try summary(#","agent":"gemini""#).agent, .unsupported("gemini"),
            "an unknown agent is retained, never read as Claude")
    }

    /// **The unsupported arm, on `SessionSummary` itself.**
    ///
    /// The suite covered `"codex"` and `"claude"` and never an unknown string on
    /// the summary — the exact case the rewritten
    /// `testSessionSummaryDecodesTheAgentAndStillIgnoresKeysItDoesNotKnow`
    /// stopped being about. Reading `"gemini"` as Claude would offer a Claude
    /// session's whole vocabulary to an agent that answers none of it.
    @MainActor func testAnUnknownAgentOnASummaryIsUnsupportedAndNotCodex() throws {
        let summary = try decode(
            SessionSummary.self,
            """
            {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1","cwd":"/x",
             "lifecycle":"live","link":"attached","last_seq":3,
             "created_at":"t","updated_at":"t","agent":"gemini","codex_link":"subscribed"}
            """)
        XCTAssertEqual(summary.agent, .unsupported("gemini"))
        XCTAssertFalse(summary.isCodex, "an agent this build cannot drive is not Codex")
        XCTAssertEqual(summary.agent.displayName, "gemini", "named in the daemon's own word")
        // And it gets no answer surface at all — never Claude's by default.
        guard case .noneAnswerable = DecisionCardView.answerSurface(
            card: try XCTUnwrap(capturedCards().first?.card), agent: summary.agent,
            paneSnapshot: nil)
        else { return XCTFail("an unknown agent must not inherit a vocabulary") }
    }

    /// D3: `codex_link` is the addressee state, defaulted so an older daemon
    /// still parses; `codex_thread_id` is opaque and only ever carried.
    ///
    /// **Two minors' worth of vocabulary, decoded by one code path.** Four of the
    /// words are minor 19, which build 72 shipped; `bound_not_started` is minor
    /// 20. That split is why the field is decoded as an opaque string with an
    /// `unknown` arm rather than as a closed enum — a daemon older than the word
    /// never sends it, and a daemon newer than this build may send one nobody
    /// here has heard of. Both directions land somewhere safe, and the last row
    /// is the proof of the second.
    func testTheSummaryCarriesTheCodexLinkAndThreadId() throws {
        func summary(_ extra: String) throws -> SessionSummary {
            try decode(
                SessionSummary.self,
                """
                {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1","cwd":"/x",
                 "lifecycle":"live","link":"attached","last_seq":3,
                 "created_at":"t","updated_at":"t"\(extra)}
                """)
        }
        XCTAssertEqual(
            try summary("").codexLink, CodexLinkState.none,
            "an older daemon sends no codex_link, and `none` is the safe default")
        XCTAssertNil(try summary("").codexThreadID)

        // Minor 19's four words, then minor 20's fifth.
        for (wire, state) in [
            ("subscribed", CodexLinkState.subscribed), ("bound", .bound),
            ("offline", .offline), ("none", CodexLinkState.none),
            ("bound_not_started", .boundNotStarted),
        ] {
            XCTAssertEqual(try summary(#","codex_link":"\#(wire)""#).codexLink, state, wire)
        }
        XCTAssertEqual(
            try summary(#","codex_link":"teleported""#).codexLink, .unknown("teleported"),
            "an unrecognised link word is retained, never read as subscribed")
        XCTAssertEqual(
            try summary(#","codex_thread_id":"01a073f6-09c8-7c10-8212-4d369b80140b""#)
                .codexThreadID,
            "01a073f6-09c8-7c10-8212-4d369b80140b")
    }

    // MARK: The result envelopes

    func testEveryInterruptArmDecodes() throws {
        let aborted = try decode(
            InterruptResult.self, #"{"status":"aborted","turn_id":"t-1"}"#)
        XCTAssertEqual(aborted, .aborted(turnID: "t-1"))

        let duplicate = try decode(
            InterruptResult.self, #"{"status":"duplicate","turn_id":"t-1"}"#)
        XCTAssertEqual(duplicate, .duplicate(turnID: "t-1"))

        let rejected = try decode(
            InterruptResult.self, #"{"status":"rejected","reason":"unknown session cc-9"}"#)
        XCTAssertEqual(rejected, .rejected(reason: "unknown session cc-9"))

        let indeterminate = try decode(
            InterruptResult.self, #"{"status":"indeterminate","reason":"the wire died"}"#)
        XCTAssertEqual(indeterminate, .indeterminate(reason: "the wire died"))
    }

    /// §3 of the contract, as a decode fact: a refusal carries no id the phone
    /// did not send.
    func testARefusedInterruptCarriesNoTurnId() throws {
        let rejected = try decode(
            InterruptResult.self,
            #"{"status":"rejected","reason":"this request names no turn, so there is nothing to stop"}"#)
        XCTAssertNil(rejected.turnID, "a refusal never discloses a turn")
        XCTAssertEqual(
            InterruptResult.aborted(turnID: "t-1").turnID, "t-1",
            "success returns the turn the phone itself named")
    }

    func testEveryComposeArmDecodes() throws {
        XCTAssertEqual(
            try decode(ComposeResult.self, #"{"status":"started","turn_id":"t-1"}"#),
            .started(turnID: "t-1"))
        XCTAssertEqual(
            try decode(ComposeResult.self, #"{"status":"steered","turn_id":"t-1"}"#),
            .steered(turnID: "t-1"))
        XCTAssertEqual(
            try decode(
                ComposeResult.self, #"{"status":"duplicate","turn_id":"t-1","started":true}"#),
            .duplicate(turnID: "t-1", started: true))
        XCTAssertEqual(
            try decode(
                ComposeResult.self, #"{"status":"duplicate","turn_id":"t-1","started":false}"#),
            .duplicate(turnID: "t-1", started: false))
        XCTAssertEqual(
            try decode(ComposeResult.self, #"{"status":"rejected","reason":"empty"}"#),
            .rejected(reason: "empty"))
        XCTAssertEqual(
            try decode(ComposeResult.self, #"{"status":"indeterminate","reason":"unknown"}"#),
            .indeterminate(reason: "unknown"))
    }

    /// `started` is **required** on `duplicate` (ws.rs:1131-1137). A default
    /// would render a replay with the wrong verb — "started a turn" for words
    /// that steered one — so an absent one must fail the decode rather than be
    /// guessed at.
    func testDuplicateWithoutStartedIsADecodeFailure() {
        XCTAssertThrowsError(
            try decode(ComposeResult.self, #"{"status":"duplicate","turn_id":"t-1"}"#))
    }

    /// The single easiest mistake in the phase: every result enum discriminates
    /// on **`status`**, never on `type`.
    func testResultsDiscriminateOnStatusNotType() {
        XCTAssertThrowsError(
            try decode(InterruptResult.self, #"{"type":"aborted","turn_id":"t-1"}"#))
        XCTAssertThrowsError(
            try decode(ComposeResult.self, #"{"type":"started","turn_id":"t-1"}"#))
    }

    /// The whole frames, through `ServerMessage`, so the decoder's own routing
    /// is covered and not just the payload types.
    func testTheTwoNewServerFramesRouteByType() throws {
        let interrupt = try decode(
            ServerMessage.self,
            """
            {"type":"interrupt_result","session_id":"u-1","request_id":"stop-1",
             "result":{"status":"aborted","turn_id":"t-1"}}
            """)
        guard case .interruptResult(let session, let request, let result) = interrupt else {
            return XCTFail("expected .interruptResult, got \(interrupt)")
        }
        XCTAssertEqual(session, "u-1")
        XCTAssertEqual(request, "stop-1")
        XCTAssertEqual(result, .aborted(turnID: "t-1"))

        let compose = try decode(
            ServerMessage.self,
            """
            {"type":"compose_result","session_id":"u-1","request_id":"say-1",
             "result":{"status":"steered","turn_id":"t-1"}}
            """)
        guard case .composeResult(let s2, let r2, let result2) = compose else {
            return XCTFail("expected .composeResult, got \(compose)")
        }
        XCTAssertEqual(s2, "u-1")
        XCTAssertEqual(r2, "say-1")
        XCTAssertEqual(result2, .steered(turnID: "t-1"))
    }

    // MARK: The corpus

    // `testTheCorpusStatesWhichFramesArePhoneWire` lived here: a census of a
    // checked-in directory that walked 454 lines of fixture and asserted how
    // many were notes, how many were upstream JSON-RPC and how many were phone
    // wire. It exercised no app code — it tested the fixture folder against
    // itself, and would have gone red the day somebody added a capture. The one
    // thing worth keeping from it is the *statement*, which is in this class's
    // own documentation: only `compose-0.153.4.jsonl` carries phone frames, and
    // the test below is what reads them.

    /// **The compose capture's phone lines are ABBREVIATED, and this says so.**
    ///
    /// Measured, not assumed: `compose-0.153.4.jsonl`'s four `"conn":"phone"`
    /// frames carry no `session_id` in either direction and no `payload_hash` on
    /// the request, while `ws.rs` makes all three **required**
    /// (`ClientMessage::Compose`, `ServerMessage::ComposeResult`). So those
    /// lines record the *sequence* a real exchange had, not its bytes, and a
    /// test that decoded them whole through `ServerMessage` would fail on the
    /// fixture rather than on the app.
    ///
    /// What is byte-accurate in them is the `result` object, which is the part
    /// this phase's decoder is actually responsible for — so that is what is
    /// decoded, and the envelope's shortfall is named rather than worked around
    /// silently.
    func testTheComposeCaptureIsAbbreviatedAndItsResultsStillDecode() throws {
        let text = String(decoding: try fixture("compose-0.153.4.jsonl"), as: UTF8.self)
        var results: [ComposeResult] = []
        var requests = 0

        for line in text.split(separator: "\n") {
            let envelope = try JSONDecoder().decode(JSONValue.self, from: Data(line.utf8))
            guard envelope["conn"]?.stringValue == "phone", let frame = envelope["frame"]
            else { continue }

            // The abbreviation, pinned. If a future capture fills these in, this
            // fails and the sweep above can start decoding the whole envelope.
            XCTAssertNil(frame["session_id"], "the capture omits session_id, which ws.rs requires")

            switch envelope["dir"]?.stringValue {
            case "c2s":
                XCTAssertEqual(frame["type"]?.stringValue, "compose")
                XCTAssertNil(
                    frame["payload_hash"], "the capture omits payload_hash, which ws.rs requires")
                XCTAssertNotNil(frame["request_id"]?.stringValue)
                XCTAssertNotNil(frame["text"]?.stringValue)
                requests += 1
            case "s2c":
                XCTAssertEqual(frame["type"]?.stringValue, "compose_result")
                let result = try XCTUnwrap(frame["result"]?.decoded(ComposeResult.self))
                results.append(result)
            default:
                XCTFail("a phone frame with no direction")
            }
        }

        XCTAssertEqual(requests, 2)
        XCTAssertEqual(results.count, 2)
        // The fact the capture exists to record: idle then mid-turn, one turn.
        guard case .started(let first) = results.first,
            case .steered(let second) = results.last
        else { return XCTFail("expected started then steered, got \(results)") }
        XCTAssertEqual(
            first, second, "a steer joins the running turn; it does not start a new one")
    }

    /// And the frame the phone must actually produce, whole — the fields the
    /// capture left out included. Encode-only: nothing decodes a client message.
    func testTheComposeAndInterruptFramesThePhoneSends() throws {
        func encoded(_ message: ClientMessage) throws -> [String: JSONValue] {
            let data = try JSONEncoder().encode(message)
            return try XCTUnwrap(
                try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
        }

        let compose = try encoded(
            .compose(session: "u-1", requestID: "say-1", text: "hi", payloadHash: "h"))
        XCTAssertEqual(
            Set(compose.keys), ["type", "session_id", "request_id", "text", "payload_hash"],
            "all four fields, and no others")
        XCTAssertEqual(compose["type"], .string("compose"))
        XCTAssertEqual(compose["session_id"], .string("u-1"))

        let interrupt = try encoded(
            .interrupt(session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h"))
        XCTAssertEqual(
            Set(interrupt.keys), ["type", "session_id", "request_id", "turn_id", "payload_hash"])
        XCTAssertEqual(interrupt["type"], .string("interrupt"))
        XCTAssertEqual(interrupt["turn_id"], .string("t-1"))
    }
}
