import XCTest

@testable import CodeConnect

/// The wire format at feature level 1, tested against JSON written by hand
/// from `mac/protocol/src`.
///
/// Hand-written rather than round-tripped through this app's own encoders on
/// purpose: a round-trip test passes just as happily when both sides are wrong
/// together, which is the exact failure a two-language protocol has to be
/// defended against.
final class ProtocolTests: XCTestCase {

    private func decodeServer(_ json: String) throws -> ServerMessage {
        try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
    }

    private func encodeClient(_ message: ClientMessage) throws -> [String: JSONValue] {
        let data = try JSONEncoder().encode(message)
        let value = try JSONDecoder().decode(JSONValue.self, from: data)
        return try XCTUnwrap(value.objectValue)
    }

    // MARK: hello

    /// The pairing hello must carry **no** `token` at all.
    ///
    /// `protocol/src/ws.rs`: "one carrying both prefers the token". An empty
    /// string would therefore be taken as the credential to check, the pairing
    /// code would never be looked at, and pairing would fail with an
    /// authentication error that had nothing to do with the code.
    func testPairingHelloOmitsTokenEntirely() throws {
        let fields = try encodeClient(
            .hello(
                credential: .pairingCode("ABCD2345"), clientID: "id", clientName: "iPhone",
                sshPublicKey: "ssh-ed25519 AAAAC3Nz phone"))
        XCTAssertEqual(fields["type"]?.stringValue, "hello")
        XCTAssertEqual(fields["pairing_code"]?.stringValue, "ABCD2345")
        XCTAssertNil(fields["token"], "a pairing hello that carries a token is authenticated as one")
        XCTAssertEqual(fields["ssh_pubkey"]?.stringValue, "ssh-ed25519 AAAAC3Nz phone")
        XCTAssertEqual(fields["protocol_version"]?.intValue, Int(Wire.protocolVersion))
    }

    func testTokenHelloOmitsPairingCode() throws {
        let fields = try encodeClient(
            .hello(credential: .token("deadbeef"), clientID: nil, clientName: nil, sshPublicKey: nil))
        XCTAssertEqual(fields["token"]?.stringValue, "deadbeef")
        XCTAssertNil(fields["pairing_code"])
        XCTAssertNil(fields["ssh_pubkey"], "a key that was not offered must not appear as null")
    }

    // MARK: hello_ack

    func testHelloAckCarriesTheAdditiveFields() throws {
        let message = try decodeServer(
            """
            {"type":"hello_ack","protocol_version":1,"protocol_minor":1,
             "server_time":"2026-07-31T09:14:00.000Z",
             "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
               "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,
               "push":false,"tls":true,"tls_active":true,"diff":true,"risk_class":true},
             "device_token":"tok_123","device_id":"dev_1","device_name":"iPhone 2",
             "ssh_key_installed":true}
            """)
        guard case .helloAck(let ack) = message else { return XCTFail("wrong message") }
        XCTAssertEqual(ack.protocolMinor, 1)
        XCTAssertEqual(ack.deviceToken, "tok_123")
        XCTAssertEqual(ack.deviceName, "iPhone 2")
        XCTAssertEqual(ack.sshKeyInstalled, true)
        XCTAssertTrue(ack.capabilities.servesDiff)
        XCTAssertTrue(ack.capabilities.classifiesRisk)
        XCTAssertTrue(ack.capabilities.tlsActive)
    }

    /// An older ack has none of the additive keys and must still decode — the alternative
    /// is an app that reports a working daemon as unreachable.
    func testOlderHelloAckStillDecodes() throws {
        let message = try decodeServer(
            """
            {"type":"hello_ack","protocol_version":1,"server_time":"2026-07-30T16:00:00.000Z",
             "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
               "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,
               "push":false,"tls":false}}
            """)
        guard case .helloAck(let ack) = message else { return XCTFail("wrong message") }
        XCTAssertEqual(ack.protocolMinor, 0)
        XCTAssertFalse(ack.capabilities.servesDiff)
        let profile = DaemonProfile(
            protocolVersion: ack.protocolVersion, protocolMinor: ack.protocolMinor,
            capabilities: ack.capabilities)
        XCTAssertFalse(profile.speaksMinor1OrLater)
        XCTAssertFalse(profile.trustsTurnCompleteKind)
    }

    /// An unreadable or renamed capability must degrade one affordance, never
    /// fail the whole handshake.
    func testCapabilitiesSurviveAnUnknownShape() throws {
        let message = try decodeServer(
            """
            {"type":"hello_ack","protocol_version":1,"server_time":"t",
             "capabilities":{"can_approve_reliably":true,"future_thing":"yes","hold_secs":9}}
            """)
        guard case .helloAck(let ack) = message else { return XCTFail("wrong message") }
        XCTAssertTrue(ack.capabilities.canApproveReliably)
        XCTAssertEqual(ack.capabilities.holdSecs, 9)
        XCTAssertFalse(ack.capabilities.sendText, "an absent capability is not an available one")
        XCTAssertEqual(ack.capabilities.advertised["future_thing"]?.stringValue, "yes")
        XCTAssertTrue(
            ack.capabilities.advertisedRows.contains { $0.name == "future_thing" },
            "the trust screen must be able to list capabilities this build cannot name")
    }

    // MARK: diff

    func testDiffFrameDecodes() throws {
        let message = try decodeServer(
            """
            {"type":"diff","session_id":"cc-1","unified":"diff --git a/x b/x\\n",
             "truncated":true,"captured_at":"2026-07-31T09:14:02.104Z","note":"not a git repository"}
            """)
        guard case .diff(let diff) = message else { return XCTFail("wrong message") }
        XCTAssertEqual(diff.sessionID, "cc-1")
        XCTAssertTrue(diff.truncated)
        XCTAssertEqual(diff.note, "not a git repository")
        XCTAssertNotNil(diff.capturedDate)
    }

    func testGetDiffEncoding() throws {
        let fields = try encodeClient(.getDiff(session: "cc-2"))
        XCTAssertEqual(fields["type"]?.stringValue, "get_diff")
        XCTAssertEqual(fields["session_id"]?.stringValue, "cc-2")
    }

    // MARK: turn_complete

    func testTurnCompleteIsItsOwnKind() throws {
        let message = try decodeServer(
            """
            {"type":"event","event":{"seq":4,"session_id":"cc-1","ts":"2026-07-31T09:00:00.000Z",
             "kind":"turn_complete","source":"hook",
             "payload":{"hook_event_name":"Stop","last_assistant_message":"done"}}}
            """)
        guard case .event(let event) = message else { return XCTFail("wrong message") }
        XCTAssertEqual(event.kind, .turnComplete)
        XCTAssertTrue(event.isTurnComplete)
        XCTAssertFalse(event.isSessionExit, "a finished turn is not a finished session")
    }

    /// The older shape still reads as a turn boundary, and a real session exit
    /// still reads as one — which is what makes the legacy clause safe to keep.
    func testLegacyStopShapeStillReadsAsTurnComplete() throws {
        let legacy = try decodeServer(
            """
            {"type":"event","event":{"seq":4,"session_id":"cc-1","ts":"t","kind":"session_end",
             "source":"hook","payload":{"hook_event_name":"Stop"}}}
            """)
        guard case .event(let event) = legacy else { return XCTFail("wrong message") }
        XCTAssertTrue(event.isTurnComplete)

        let exit = try decodeServer(
            """
            {"type":"event","event":{"seq":9,"session_id":"cc-1","ts":"t","kind":"session_end",
             "source":"daemon","payload":{"exit_code":0}}}
            """)
        guard case .event(let exitEvent) = exit else { return XCTFail("wrong message") }
        XCTAssertFalse(exitEvent.isTurnComplete)
        XCTAssertTrue(exitEvent.isSessionExit)
    }

    // MARK: risk

    func testApprovalCardCarriesNestedRiskBlock() throws {
        let message = try decodeServer(
            """
            {"type":"event","event":{"seq":2,"session_id":"cc-1","ts":"t",
             "kind":"approval_request","source":"hook","payload":{"card":{
               "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
               "tool_input":{"command":"rm -rf /tmp/x"},"display_text":"Bash\\n{}",
               "risk":{"class":"high","matched_pattern":"rm -rf"}}}}}
            """)
        guard case .event(let event) = message, let card = event.approvalCard else {
            return XCTFail("no card")
        }
        XCTAssertEqual(card.risk?.cls, "high")
        XCTAssertEqual(card.risk?.matchedPattern, "rm -rf")
    }

    // MARK: outcomes

    /// An inferred decision is the daemon noticing the prompt is gone. It must
    /// never be rendered as an observed answer.
    func testInferredOutcomeIsLabelledAsSuch() throws {
        let message = try decodeServer(
            """
            {"type":"answer_result","request_id":"toolu_1","result":{"status":"applied",
             "outcome":{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
               "resolved_by":"local","applied_via":"send_keys",
               "resolved_at":"2026-07-31T09:00:00.000Z","inferred":true}}}
            """)
        guard case .answerResult(_, .applied(let outcome)) = message else {
            return XCTFail("wrong result")
        }
        XCTAssertTrue(outcome.inferred)
        XCTAssertEqual(outcome.decisionLabel, "Answered at the keyboard")
        XCTAssertEqual(outcome.decision.label, "Allowed", "the raw decision is still available")
    }

    func testOutcomeWithoutInferredDefaultsToObserved() throws {
        let outcome = try JSONDecoder().decode(
            AnswerOutcome.self,
            from: Data(
                """
                {"request_id":"r","session_id":"s","decision":{"type":"deny"},
                 "resolved_by":"phone","applied_via":"send_keys","resolved_at":"t"}
                """.utf8))
        XCTAssertFalse(outcome.inferred)
        XCTAssertEqual(outcome.decisionLabel, "Denied")
    }
}


/// The push-test wire, pinned like the delete wire: every status the daemon can
/// answer, and the rule that an unknown one stays inert.
@MainActor
final class TestPushWireTests: XCTestCase {
    private func decode(_ json: String) throws -> TestPushResult {
        try JSONDecoder().decode(TestPushResult.self, from: Data(json.utf8))
    }

    func testEveryStatusDecodesToItsOwnMeaning() throws {
        XCTAssertEqual(
            try decode(#"{"status":"accepted","apns_id":"A1"}"#), .accepted(apnsID: "A1"))
        XCTAssertEqual(try decode(#"{"status":"accepted"}"#), .accepted(apnsID: nil))
        XCTAssertEqual(try decode(#"{"status":"push_unconfigured"}"#), .pushUnconfigured)
        XCTAssertEqual(try decode(#"{"status":"not_paired_device"}"#), .notPairedDevice)
        XCTAssertEqual(try decode(#"{"status":"no_registered_token"}"#), .noRegisteredToken)
        XCTAssertEqual(
            try decode(#"{"status":"rate_limited","retry_after_secs":12}"#),
            .rateLimited(retryAfterSecs: 12))
        XCTAssertEqual(
            try decode(#"{"status":"failed","reason":"apns 500"}"#), .failed(reason: "apns 500"))
        XCTAssertEqual(try decode(#"{"status":"queued"}"#), .unknown(status: "queued"))
    }

    func testTheRequestCarriesItsCorrelationId() throws {
        let encoded = try JSONEncoder().encode(ClientMessage.testPush(requestID: "tp-9"))
        let text = String(decoding: encoded, as: UTF8.self)
        XCTAssertTrue(text.contains(#""test_push""#), text)
        XCTAssertTrue(text.contains(#""tp-9""#), text)
    }

    func testTheReplyRoutesByItsId() throws {
        let decoded = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                #"{"type":"test_push_result","request_id":"tp-9","result":{"status":"accepted"}}"#
                    .utf8))
        guard case .testPushResult("tp-9", .accepted(nil)) = decoded else {
            return XCTFail("got \(decoded)")
        }
    }
}


/// The agent-prose renderer's model half: what becomes a code block, what
/// stays prose, and the rule that malformed fences lose nothing.
final class AgentProseTests: XCTestCase {

    func testFencesBecomeCodeAndProseStaysProse() {
        let text = "Look:\n```swift\nlet x = 1\n```\nDone."
        XCTAssertEqual(
            AgentProse.segments(text),
            [.prose("Look:"), .code("let x = 1"), .prose("Done.")],
            "the fence lines and the language tag are wrapper, not content")
    }

    func testMultipleFencesKeepTheirOrder() {
        let text = "```\na\n```\nmiddle\n```\nb\n```"
        XCTAssertEqual(
            AgentProse.segments(text), [.code("a"), .prose("middle"), .code("b")])
    }

    /// An unclosed fence is not a fence: guessing at what an unterminated
    /// block meant is how content vanishes, so the text renders literally.
    func testAnUnclosedFenceFallsBackToVerbatimProse() {
        let text = "before\n```swift\nnever closed"
        let segments = AgentProse.segments(text)
        XCTAssertEqual(segments.count, 1)
        guard case .prose(let prose) = segments[0] else { return XCTFail("\(segments)") }
        XCTAssertTrue(prose.contains("```"), "the fence line itself must survive")
        XCTAssertTrue(prose.contains("never closed"))
    }

    func testInlineMarkdownRendersInsteadOfShowingArtifacts() {
        let rendered = AgentProse.inline("**CodeConnect** lets you *monitor*")
        let plain = String(rendered.characters)
        XCTAssertFalse(plain.contains("*"), "asterisks are formatting, not content: \(plain)")
        XCTAssertTrue(plain.contains("CodeConnect lets you monitor"))
    }

    // MARK: Tildes

    private func hasStrikethrough(_ a: AttributedString) -> Bool {
        // The markdown parser records strikethrough as an *inline presentation
        // intent*, not as `strikethroughStyle` — asserted on the wrong
        // attribute, the first version of these tests passed vacuously.
        // Probed, not assumed.
        a.runs.contains { $0.inlinePresentationIntent?.contains(.strikethrough) == true }
    }

    /// The real defect: lone "approximately" tildes that Apple's parser pairs
    /// into a deletion. `a ~single~ pair` provably pairs (probed) — after
    /// neutralization it must not, and the tildes must survive as text.
    func testLoneTildesDoNotStrikeTheTextBetweenThem() {
        let unguarded = try? AttributedString(
            markdown: "a ~single~ pair",
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
        XCTAssertTrue(
            hasStrikethrough(unguarded ?? AttributedString()),
            "the premise: without the neutralizer, Apple pairs lone tildes")

        let rendered = AgentProse.inline("a ~single~ pair")
        XCTAssertFalse(hasStrikethrough(rendered), "a lone ~ is a word, not a deletion")
        XCTAssertTrue(String(rendered.characters).contains("~single~"))

        // And the real message's shape survives whole.
        let real = AgentProse.inline("after ~60 seconds, work stalls (~$25 one-time).")
        XCTAssertFalse(hasStrikethrough(real))
        XCTAssertTrue(String(real.characters).contains("~60 seconds"))
    }

    func testIntentionalDoubleTildeStillStrikes() {
        XCTAssertTrue(
            hasStrikethrough(AgentProse.inline("keep ~~this struck~~ though")),
            "~~...~~ is the strikethrough an agent occasionally means")
    }

    /// The trap the neutralizer must never fall into: tildes inside backtick
    /// spans are paths, and an escape there renders as a literal backslash.
    func testTildesInsideInlineCodeAreUntouched() {
        let rendered = AgentProse.inline("config lives in `~/.codeconnect/config.json`")
        let plain = String(rendered.characters)
        XCTAssertTrue(plain.contains("~/.codeconnect/config.json"))
        XCTAssertFalse(plain.contains("\\~"), "no backslash may reach a path: \(plain)")
    }

    func testAnAlreadyEscapedTildeIsNotDoubleEscaped() {
        XCTAssertEqual(
            AgentProse.neutralizeLoneTildes("about \\~60"), "about \\~60",
            "singly escaped stays singly escaped")
    }

    func testDoubleBacktickSpansShieldTheirTildes() {
        XCTAssertEqual(
            AgentProse.neutralizeLoneTildes("run ``x ~ y`` then ~5s"),
            "run ``x ~ y`` then \\~5s",
            "span matching is by backtick-run length, per CommonMark")
    }

    // MARK: Headings

    func testHeadingLevelsSegmentAsHeadingsAmongProseAndCode() {
        let text = "# One\nbody\n###### Six\n```\n# not a heading\n```"
        XCTAssertEqual(
            AgentProse.segments(text),
            [
                .heading("One"), .prose("body"), .heading("Six"),
                .code("# not a heading"),
            ],
            "markers strip outside fences and never inside them")
    }

    func testNonHeadingsStayLiteral() {
        XCTAssertEqual(AgentProse.segments("#nospace"), [.prose("#nospace")])
        XCTAssertEqual(AgentProse.segments("####### seven"), [.prose("####### seven")])
    }

    func testThePreviewSourceStripsHeadingMarkers() {
        let source = AgentProse.previewSource("## How it's useful\nThe core value is x.")
        XCTAssertEqual(source, "How it's useful\nThe core value is x.")
    }

    func testTickedFenceMarkerInProseIsNotADelimiter() {
        // A line *mentioning* backticks inline is prose; only a delimiter line
        // opens a fence.
        let text = "use ```code``` fences"
        XCTAssertEqual(AgentProse.segments(text), [.prose("use ```code``` fences")])
    }
}

/// The composer chips: exact labels, exact order, and the reason "Stop" is
/// absent is a product fact — injection cannot interrupt a running turn.
final class ComposerTemplateTests: XCTestCase {
    func testTheTemplatesAreTheAgreedSetInOrder() {
        XCTAssertEqual(
            ComposerTemplates.all,
            ["Continue", "Fix it", "Run the tests", "Commit & push", "Explain this",
             "Use a simpler approach"])
    }
}
