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
                credential: .pairingCode("ABCD2345"), clientID: "id", clientName: "iPhone"))
        XCTAssertEqual(fields["type"]?.stringValue, "hello")
        XCTAssertEqual(fields["pairing_code"]?.stringValue, "ABCD2345")
        XCTAssertNil(fields["token"], "a pairing hello that carries a token is authenticated as one")
        XCTAssertEqual(fields["protocol_version"]?.intValue, Int(Wire.protocolVersion))
    }

    func testTokenHelloOmitsPairingCode() throws {
        let fields = try encodeClient(
            .hello(credential: .token("deadbeef"), clientID: nil, clientName: nil))
        XCTAssertEqual(fields["token"]?.stringValue, "deadbeef")
        XCTAssertNil(fields["pairing_code"])
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
             "device_token":"tok_123","device_id":"dev_1","device_name":"iPhone 2"}
            """)
        guard case .helloAck(let ack) = message else { return XCTFail("wrong message") }
        XCTAssertEqual(ack.protocolMinor, 1)
        XCTAssertEqual(ack.deviceToken, "tok_123")
        XCTAssertEqual(ack.deviceName, "iPhone 2")
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


/// The send_text and catalog wire shapes, pinned byte-for-byte against what
/// `protocol/src/ws.rs` ships.
final class SendTextWireTests: XCTestCase {

    private func encoded(_ message: ClientMessage) -> String {
        String(data: try! JSONEncoder().encode(message), encoding: .utf8)!
    }

    func testSendTextCarriesItsIdentityAndOmitsAnAbsentOne() {
        let with = encoded(
            .sendText(
                session: "u-1", text: "hi", require: nil, submit: true,
                requestID: "st-1", payloadHash: "abc123",
                completeNativeConfirmation: false))
        XCTAssertTrue(with.contains(#""request_id":"st-1""#), with)
        XCTAssertTrue(with.contains(#""payload_hash":"abc123""#), with)

        let without = encoded(
            .sendText(
                session: "u-1", text: "hi", require: nil, submit: true,
                requestID: nil, payloadHash: nil, completeNativeConfirmation: false))
        XCTAssertFalse(without.contains("request_id"), "omitted, never null: \(without)")
        XCTAssertFalse(without.contains("payload_hash"), without)
    }

    /// The permission that lets the daemon *complete* Claude Code's
    /// confirmation instead of dismissing it. Always present and explicit — a
    /// daemon that defaults it to false must be told `true` by a client that
    /// showed the human what the command costs.
    func testSendTextCarriesTheNativeConfirmationPermission() {
        let allowed = encoded(
            .sendText(
                session: "u-1", text: "/model sonnet", require: nil, submit: true,
                requestID: nil, payloadHash: nil, completeNativeConfirmation: true))
        XCTAssertTrue(allowed.contains(#""complete_native_confirmation":true"#), allowed)

        let ordinary = encoded(
            .sendText(
                session: "u-1", text: "hi", require: nil, submit: true,
                requestID: nil, payloadHash: nil, completeNativeConfirmation: false))
        XCTAssertTrue(ordinary.contains(#""complete_native_confirmation":false"#), ordinary)
    }

    func testGetCommandCatalogEncodesItsType() {
        let json = encoded(.getCommandCatalog(session: "u-1"))
        XCTAssertTrue(json.contains(#""type":"get_command_catalog""#), json)
        XCTAssertTrue(json.contains(#""session_id":"u-1""#), json)
    }

    private func decodeResult(_ json: String) -> SendTextResult {
        try! JSONDecoder().decode(SendTextResult.self, from: Data(json.utf8))
    }

    func testTheFourStatusesDecode() {
        XCTAssertEqual(
            decodeResult(#"{"status":"sent","matched":"foragents"}"#),
            .sent(matched: "foragents"))
        XCTAssertEqual(
            decodeResult(#"{"status":"refused","reason":"no composer"}"#),
            .refused(reason: "no composer"))
        XCTAssertEqual(
            decodeResult(
                #"{"status":"duplicate","matched":"foragents","applied_at":"2026-08-04T20:00:00Z"}"#
            ),
            .duplicate(matched: "foragents", appliedAt: "2026-08-04T20:00:00Z"))
        XCTAssertEqual(
            decodeResult(#"{"status":"indeterminate","reason":"never confirmed"}"#),
            .indeterminate(reason: "never confirmed"))
    }

    /// A status this build has never seen is a mutation result it cannot vouch
    /// for. "Refused" would promise nothing was typed — a promise made on the
    /// daemon's behalf — so unknown decodes as the case that promises nothing.
    func testAnUnknownStatusPromisesNothing() {
        guard case .indeterminate(let reason) = decodeResult(#"{"status":"teleported"}"#) else {
            return XCTFail("unknown must decode as indeterminate")
        }
        XCTAssertTrue(reason.contains("teleported"), reason)
    }

    func testTheCommandCatalogDecodesBothAnswers() throws {
        let payload = #"""
            {"type":"command_catalog","session_id":"u-1","result":{"status":"available",
            "commands":["model","clear"],"claude_version":"2.1.221",
            "probed_at":"2026-08-04T20:00:00Z"}}
            """#
        guard
            case .commandCatalog(let session, .available(let commands, let version, let probedAt)) =
                try JSONDecoder().decode(ServerMessage.self, from: Data(payload.utf8))
        else { return XCTFail("wrong decode") }
        XCTAssertEqual(session, "u-1")
        XCTAssertEqual(commands, ["model", "clear"])
        XCTAssertEqual(version, "2.1.221")
        XCTAssertEqual(probedAt, "2026-08-04T20:00:00Z")

        let unavailable = #"{"type":"command_catalog","session_id":"u-1","result":{"status":"unavailable","reason":"no binary"}}"#
        guard
            case .commandCatalog(_, .unavailable(let reason)) = try JSONDecoder().decode(
                ServerMessage.self, from: Data(unavailable.utf8))
        else { return XCTFail("wrong decode") }
        XCTAssertEqual(reason, "no binary")
    }

    func testTheNewCapabilitiesRead() throws {
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(
                #"{"send_text":true,"send_text_idempotent":true,"command_catalog":true}"#.utf8))
        XCTAssertTrue(caps.sendTextIdempotent)
        XCTAssertTrue(caps.servesCommandCatalog)
        let bare = try JSONDecoder().decode(
            Capabilities.self, from: Data(#"{"send_text":true}"#.utf8))
        XCTAssertFalse(bare.sendTextIdempotent, "unknown is always false")
        XCTAssertFalse(bare.servesCommandCatalog)
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

    /// The preview drops blank lines from **prose only**. Its four lines are
    /// worth more spent on words, and a preview that ends on a blank one
    /// draws a line of empty space above the expander that the expanded state
    /// does not have. Code is untouched: its blank lines separate statements
    /// and its leading whitespace is the structure.
    func testThePreviewDropsBlankProseLinesAndLeavesCodeAlone() {
        let source = AgentProse.previewSource(
            "First line.\n\n\nSecond line.\n\n```swift\nif x {\n\n    work()\n}\n```")
        XCTAssertTrue(
            source.contains("First line.\nSecond line."),
            "blank prose lines spend the preview's four lines on nothing: \(source)")
        XCTAssertTrue(
            source.contains("if x {\n\n    work()\n}"),
            "code keeps its own blank lines and its indentation: \(source)")
    }

    /// Runs of blank lines between paragraphs — including whitespace-only
    /// ones — leave nothing behind in the preview. Asserted end to end
    /// rather than at the layer that happens to achieve it: `segments`
    /// discards a whitespace-only run and the preview drops the rest, and
    /// the contract is the same whichever of them does the work.
    func testBlankRunsBetweenParagraphsLeaveNothingInThePreview() {
        let source = AgentProse.previewSource("Alpha.\n\n   \n\nBeta.")
        XCTAssertEqual(source, "Alpha.\nBeta.", source)
    }

    func testTickedFenceMarkerInProseIsNotADelimiter() {
        // A line *mentioning* backticks inline is prose; only a delimiter line
        // opens a fence.
        let text = "use ```code``` fences"
        XCTAssertEqual(AgentProse.segments(text), [.prose("use ```code``` fences")])
    }
}

/// The table grammar: a conservative GFM subset that recognizes the shape
/// agents actually emit without ever swallowing prose — and never silently
/// discards a cell.
final class AgentTableTests: XCTestCase {

    private func onlyTable(
        _ text: String, file: StaticString = #filePath, line: UInt = #line
    ) -> MarkdownTable? {
        for segment in AgentProse.segments(text) {
            if case .table(let table) = segment { return table }
        }
        XCTFail("no table in \(AgentProse.segments(text))", file: file, line: line)
        return nil
    }

    private func hasTable(_ text: String) -> Bool {
        AgentProse.segments(text).contains {
            if case .table = $0 { return true } else { return false }
        }
    }

    func testStandardTableParsesWithAlignmentsAndVerbatimRaw() {
        let text = """
            Results:

            | Command | Description | Status |
            | --- | :---: | ---: |
            | `git status` | lists files | ok |
            | push | publishes | blocked |

            Done.
            """
        let segments = AgentProse.segments(text)
        guard case .table(let table)? = segments.dropFirst().first else {
            return XCTFail("\(segments)")
        }
        XCTAssertEqual(table.headers, ["Command", "Description", "Status"])
        XCTAssertEqual(table.alignments, [.leading, .center, .trailing])
        XCTAssertEqual(
            table.rows,
            [["`git status`", "lists files", "ok"], ["push", "publishes", "blocked"]])
        XCTAssertEqual(
            table.raw,
            "| Command | Description | Status |\n| --- | :---: | ---: |\n"
                + "| `git status` | lists files | ok |\n| push | publishes | blocked |",
            "raw is the source, verbatim")
        guard case .prose(let after)? = segments.last else { return XCTFail("\(segments)") }
        XCTAssertTrue(after.contains("Done."), "prose after the table survives")
    }

    func testOuterPipesAreIndependentlyOptionalPerRow() {
        let table = onlyTable("Name | Role\n--- | ---\n| Ada | eng |\nBo | ops")
        XCTAssertEqual(table?.headers, ["Name", "Role"])
        XCTAssertEqual(table?.rows, [["Ada", "eng"], ["Bo", "ops"]])
    }

    /// `:---` and bare `---` are both leading — there is no distinct
    /// "default" rendering to preserve — and a header + delimiter with no
    /// body rows is still a table.
    func testLeadingColonAndNoColonBothReadLeadingAndHeaderOnlyParses() {
        let table = onlyTable("| a | b |\n|:---|----|")
        XCTAssertEqual(table?.alignments, [.leading, .leading])
        XCTAssertEqual(table?.rows, [])
    }

    func testTwoHyphenDelimitersStayProse() {
        XCTAssertFalse(hasTable("| a | b |\n| -- | -- |"), "three hyphens minimum")
    }

    func testHeaderDelimiterWidthMismatchStaysProse() {
        XCTAssertFalse(hasTable("| a | b | c |\n| --- | --- |"))
    }

    func testAPipeLineWithoutADelimiterIsProse() {
        XCTAssertEqual(AgentProse.segments("a | b"), [.prose("a | b")])
    }

    /// The false positive the boundary rule exists to exclude: pipes inside
    /// hard-wrapped prose never become a table.
    func testACandidateInsideHardWrappedProseStaysProse() {
        let text = "wrapped prose line\n| a | b |\n| --- | --- |\n| c | d |"
        XCTAssertEqual(AgentProse.segments(text), [.prose(text)])
    }

    /// A heading is a block edge, not prose — `### Results` directly over a
    /// table is how agents write them.
    func testAHeadingDirectlyAboveIsABoundary() {
        let segments = AgentProse.segments("## Results\n| a | b |\n| --- | --- |\n| c | d |")
        XCTAssertEqual(segments.count, 2)
        XCTAssertEqual(segments.first, .heading("Results"))
        guard case .table(let table)? = segments.last else { return XCTFail("\(segments)") }
        XCTAssertEqual(table.rows, [["c", "d"]])
    }

    func testAClosedFenceDirectlyAboveIsABoundary() {
        let segments = AgentProse.segments("```\nx\n```\n| a | b |\n| --- | --- |")
        XCTAssertEqual(segments.first, .code("x"))
        guard case .table? = segments.last else { return XCTFail("\(segments)") }
    }

    func testOneColumnStaysProse() {
        XCTAssertFalse(hasTable("| a |\n| --- |"), "a table is at least two columns")
    }

    func testHeadersMustNotAllBeEmptyButOneEmptyCornerIsFine() {
        XCTAssertFalse(hasTable("|  |  |\n| --- | --- |"))
        let table = onlyTable("|  | Lang |\n| --- | --- |\n| app | Swift |")
        XCTAssertEqual(table?.headers, ["", "Lang"])
    }

    /// Escaped pipes stay inside their cell — including in code spans, where
    /// GFM likewise requires `\|` — and an even backslash run does not escape.
    func testEscapedPipesStayInTheirCells() {
        let table = onlyTable(#"| a \| b | c |"# + "\n| --- | --- |\n" + #"| `x \| y` | z |"#)
        XCTAssertEqual(table?.headers, [#"a \| b"#, "c"])
        XCTAssertEqual(table?.rows, [[#"`x \| y`"#, "z"]])

        let even = onlyTable(#"p \\| q"# + "\n--- | ---")
        XCTAssertEqual(
            even?.headers, [#"p \\"#, "q"],
            "two backslashes escape each other, not the pipe")
    }

    func testTableShapedContentInsideAClosedFenceStaysCode() {
        XCTAssertEqual(
            AgentProse.segments("```\n| a | b |\n| --- | --- |\n```"),
            [.code("| a | b |\n| --- | --- |")])
    }

    func testTableShapedContentInsideAnUnclosedFenceStaysLiteralProse() {
        let segments = AgentProse.segments("```swift\n| a | b |\n| --- | --- |")
        XCTAssertEqual(segments.count, 1)
        guard case .prose(let prose)? = segments.first else { return XCTFail("\(segments)") }
        XCTAssertTrue(prose.contains("```swift"), "an unclosed fence renders literally")
        XCTAssertTrue(prose.contains("| a | b |"))
    }

    /// A fence opener whose info string carries a pipe is exactly wide enough
    /// to impersonate a body row — block starts outrank rows inside the body
    /// scan too, or the code after the fence loses its rendering.
    func testAFenceOpenerEndsTheTableAndStillOpensItsFence() {
        let segments = AgentProse.segments(
            "| A | B |\n|---|---|\n| 1 | 2 |\n```swift | metadata\nlet x = 1\n```")
        guard case .table(let table)? = segments.first else { return XCTFail("\(segments)") }
        XCTAssertEqual(table.rows, [["1", "2"]], "the fence opener is not a row")
        XCTAssertEqual(segments.last, .code("let x = 1"), "the fence still renders as code")
    }

    func testAHeadingShapedLineEndsTheTableAndStaysAHeading() {
        let segments = AgentProse.segments(
            "| a | b |\n|---|---|\n| 1 | 2 |\n# Result | Detail\ntail")
        guard case .table(let table)? = segments.first else { return XCTFail("\(segments)") }
        XCTAssertEqual(table.rows, [["1", "2"]])
        XCTAssertEqual(segments.dropFirst().first, .heading("Result | Detail"))
        XCTAssertEqual(segments.last, .prose("tail"))
    }

    /// Stricter than GFM, which pads and discards: a wrong-width row ends the
    /// table and is *not consumed* — every cell the agent wrote stays visible.
    func testAWrongWidthBodyRowEndsTheTableUnconsumed() {
        let segments = AgentProse.segments(
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 1 | 2 | 3 |\ntail")
        guard case .table(let table)? = segments.first else { return XCTFail("\(segments)") }
        XCTAssertEqual(table.rows, [["1", "2"]])
        XCTAssertEqual(
            segments.last, .prose("| 1 | 2 | 3 |\ntail"),
            "the offending row and everything after it segment normally")
    }

    func testABlankLineEndsTheTableAndFollowingProseSurvives() {
        let segments = AgentProse.segments("| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter")
        guard case .table(let table)? = segments.first else { return XCTFail("\(segments)") }
        XCTAssertEqual(table.rows, [["1", "2"]])
        XCTAssertEqual(segments.last, .prose("\nafter"))
    }

    func testCRLFInputParsesClean() {
        let table = onlyTable("intro\r\n\r\n| a | b |\r\n| --- | --- |\r\n| 1 | 2 |\r")
        XCTAssertEqual(table?.headers, ["a", "b"])
        XCTAssertEqual(table?.rows, [["1", "2"]], "no carriage return reaches a cell")
    }

    /// GFM's indented-code threshold: three leading spaces are a row, four —
    /// or a tab — are not.
    func testIndentationRules() {
        XCTAssertNotNil(onlyTable("   | a | b |\n   | --- | --- |"))
        XCTAssertFalse(hasTable("    | a | b |\n| --- | --- |"))
        XCTAssertFalse(hasTable("\t| a | b |\n| --- | --- |"))
    }

    // MARK: Preview and speech

    /// The collapsed preview never shows pipe art: one semantic line, headers
    /// rendered plain, row count honest down to its plural.
    func testThePreviewLineReplacesPipeArt() {
        let source = AgentProse.previewSource(
            "| **Command** | Status |\n| --- | --- |\n| build | ok |\n| test | ok |")
        XCTAssertEqual(source, "Table: Command, Status — 2 rows")

        let one = onlyTable("| a | b |\n| --- | --- |\n| 1 | 2 |")!
        XCTAssertEqual(AgentProse.tablePreviewLine(one), "Table: a, b — 1 row")
        let none = onlyTable("| a | b |\n| --- | --- |")!
        XCTAssertEqual(AgentProse.tablePreviewLine(none), "Table: a, b — 0 rows")
    }

    /// VoiceOver's three stops: shape, columns, then one label per row with
    /// every cell paired to its header — markdown resolved, escapes unescaped,
    /// and an empty cell said aloud instead of skipped.
    func testSpokenLabelsPairCellsWithHeaders() {
        let table = onlyTable(
            "| **Ready** | `cmd` |\n| --- | --- |\n"
                + #"| a \| b | run |"# + "\n|  | stop |")!
        XCTAssertEqual(AgentProse.tableSummary(table), "Table, 2 columns, 2 rows.")
        XCTAssertEqual(AgentProse.tableColumnsLabel(table), "Columns: Ready, cmd.")
        XCTAssertEqual(
            AgentProse.tableRowLabel(table, row: 0), "Row 1. Ready: a | b. cmd: run.")
        XCTAssertEqual(
            AgentProse.tableRowLabel(table, row: 1), "Row 2. Ready: empty. cmd: stop.")
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


/// The agent-seam decode-safety wire (PROTOCOL_MINOR 15): the single rule under
/// all of it is that an unknown wire value never becomes an *actuation claim* —
/// a build ahead of this one must never be read as "we typed it", "the daemon
/// said so", or any real outcome. Pinned against JSON written by hand from the
/// Rust protocol crate, never round-tripped through this app's own encoders.
@MainActor
final class AgentSeamDecodeSafetyTests: XCTestCase {

    private func decodeServer(_ json: String) throws -> ServerMessage {
        try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
    }

    private func decodeOutcome(_ json: String) throws -> AnswerOutcome {
        try JSONDecoder().decode(AnswerOutcome.self, from: Data(json.utf8))
    }

    private func decodeResolution(_ json: String) throws -> CodexResolution {
        try JSONDecoder().decode(CodexResolution.self, from: Data(json.utf8))
    }

    // MARK: applied_via (AnswerPath)

    /// The bug this whole phase exists to close: an `applied_via` a newer daemon
    /// invented used to decode to `.sendKeys`, so the app claimed it had typed
    /// keystrokes it never sent. It must now land in its own `.unknown` case and
    /// read as *not* `send_keys`.
    func testUnknownAppliedViaIsNotClaimedAsSendKeys() throws {
        let message = try decodeServer(
            """
            {"type":"answer_result","request_id":"toolu_1","result":{"status":"applied",
             "outcome":{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
               "resolved_by":"local","applied_via":"future_path",
               "resolved_at":"2026-08-18T09:00:00.000Z"}}}
            """)
        guard case .answerResult(_, .applied(let outcome)) = message else {
            return XCTFail("wrong result")
        }
        XCTAssertNotEqual(
            outcome.appliedVia, .sendKeys,
            "an unknown path must never be read as keystrokes this build sent")
        guard case .unknown(let raw) = outcome.appliedVia else {
            return XCTFail("unknown applied_via must land in its own case, got \(outcome.appliedVia)")
        }
        XCTAssertEqual(raw, "future_path")
    }

    func testKnownAppliedViaValuesStillDecode() throws {
        let hook = try decodeOutcome(
            """
            {"request_id":"r","session_id":"s","decision":{"type":"allow"},
             "resolved_by":"phone","applied_via":"hook_return","resolved_at":"t"}
            """)
        XCTAssertEqual(hook.appliedVia, .hookReturn)

        let keys = try decodeOutcome(
            """
            {"request_id":"r","session_id":"s","decision":{"type":"allow"},
             "resolved_by":"phone","applied_via":"send_keys","resolved_at":"t"}
            """)
        XCTAssertEqual(keys.appliedVia, .sendKeys)
    }

    // MARK: source (EventSource)

    /// An unrecognised `source` must not inherit the trusted `daemon`
    /// provenance — it lands in its own `.unknown` case.
    func testUnknownEventSourceIsNotClaimedAsDaemon() throws {
        let message = try decodeServer(
            """
            {"type":"event","event":{"seq":7,"session_id":"cc-1","ts":"t","kind":"notification",
             "source":"agent_bridge","payload":{}}}
            """)
        guard case .event(let event) = message else { return XCTFail("wrong message") }
        XCTAssertNotEqual(event.source, .daemon, "an unknown source must not claim daemon trust")
        guard case .unknown(let raw) = event.source else {
            return XCTFail("unknown source must land in its own case, got \(event.source)")
        }
        XCTAssertEqual(raw, "agent_bridge")
    }

    func testKnownEventSourcesStillDecode() throws {
        for (word, expected): (String, EventSource) in [
            ("hook", .hook), ("transcript", .transcript), ("daemon", .daemon), ("pty", .pty),
        ] {
            let message = try decodeServer(
                """
                {"type":"event","event":{"seq":1,"session_id":"cc-1","ts":"t","kind":"notification",
                 "source":"\(word)","payload":{}}}
                """)
            guard case .event(let event) = message else { return XCTFail("wrong message") }
            XCTAssertEqual(event.source, expected)
        }
    }

    // MARK: indeterminate (AnswerOutcome)

    func testIndeterminateDecodesAndDefaultsToFalseWhenAbsent() throws {
        let present = try decodeOutcome(
            """
            {"request_id":"r","session_id":"s","decision":{"type":"allow"},
             "resolved_by":"local","applied_via":"send_keys","resolved_at":"t","indeterminate":true}
            """)
        XCTAssertTrue(present.indeterminate)

        let absent = try decodeOutcome(
            """
            {"request_id":"r","session_id":"s","decision":{"type":"allow"},
             "resolved_by":"local","applied_via":"send_keys","resolved_at":"t"}
            """)
        XCTAssertFalse(absent.indeterminate, "an absent indeterminate is false, never present")
    }

    // MARK: option_id (AnswerDecision)

    /// The agent-seam decision shape: an option named by id, not ordinal. It
    /// decodes to its own case and round-trips back to the same bytes.
    func testOptionIdDecodesAndRoundTrips() throws {
        let json = #"{"type":"option_id","option_id":"acceptWithExecpolicyAmendment"}"#
        let decision = try JSONDecoder().decode(AnswerDecision.self, from: Data(json.utf8))
        guard case .optionId(let id) = decision else {
            return XCTFail("expected .optionId, got \(decision)")
        }
        XCTAssertEqual(id, "acceptWithExecpolicyAmendment")

        let reencoded = try JSONDecoder().decode(
            JSONValue.self, from: try JSONEncoder().encode(decision))
        let expected = try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8))
        XCTAssertEqual(reencoded, expected, "option_id must encode back to the same JSON")
    }

    /// The pre-existing decision shapes keep working byte-identically.
    func testKnownDecisionShapesStillDecode() throws {
        func decode(_ json: String) throws -> AnswerDecision {
            try JSONDecoder().decode(AnswerDecision.self, from: Data(json.utf8))
        }
        XCTAssertEqual(try decode(#"{"type":"allow"}"#), .allow)
        XCTAssertEqual(try decode(#"{"type":"deny"}"#), .deny)
        XCTAssertEqual(try decode(#"{"type":"option","index":2}"#), .option(index: 2))
        XCTAssertEqual(try decode(#"{"type":"text","text":"hi"}"#), .text("hi"))
        XCTAssertEqual(try decode(#"{"type":"future"}"#), .unrecognised("future"))
    }

    // MARK: CodexResolution envelope

    func testAnsweredByPhoneWithADecision() throws {
        let resolution = try decodeResolution(
            #"{"status":"answered","by":"phone","decision":{"type":"allow"}}"#)
        guard case .answered(let by, let decision) = resolution else {
            return XCTFail("expected .answered, got \(resolution)")
        }
        XCTAssertEqual(by, .phone)
        XCTAssertEqual(decision, .allow)
    }

    func testAnsweredByLocalWithoutADecision() throws {
        let resolution = try decodeResolution(#"{"status":"answered","by":"local"}"#)
        guard case .answered(let by, let decision) = resolution else {
            return XCTFail("expected .answered, got \(resolution)")
        }
        XCTAssertEqual(by, .local)
        XCTAssertNil(decision, "an omitted decision stays nil")
    }

    /// `turn_aborted` is load-bearing and must decode to its own retained case,
    /// never the unknown fallback.
    func testClearedTurnAbortedIsRetained() throws {
        let resolution = try decodeResolution(#"{"status":"cleared","cause":"turn_aborted"}"#)
        guard case .cleared(let cause) = resolution else {
            return XCTFail("expected .cleared, got \(resolution)")
        }
        XCTAssertEqual(cause, .turnAborted, "turn_aborted must be retained, not folded into unknown")
    }

    func testClearedSuperseded() throws {
        let resolution = try decodeResolution(#"{"status":"cleared","cause":"superseded"}"#)
        guard case .cleared(.superseded) = resolution else {
            return XCTFail("expected .cleared(.superseded), got \(resolution)")
        }
    }

    func testTimeout() throws {
        XCTAssertEqual(try decodeResolution(#"{"status":"timeout"}"#), .timeout)
    }

    func testUnknownWriteStageEnvelope() throws {
        let resolution = try decodeResolution(
            """
            {"status":"unknown","attempted_by":"phone",
             "attempted_decision":{"type":"deny"},
             "write_stage":"upstream_write_unconfirmed","cause":"broker timed out"}
            """)
        guard
            case .unknown(let attemptedBy, let attemptedDecision, let writeStage, let cause) =
                resolution
        else { return XCTFail("expected .unknown, got \(resolution)") }
        XCTAssertEqual(attemptedBy, .phone)
        XCTAssertEqual(attemptedDecision, .deny)
        XCTAssertEqual(writeStage, .upstreamWriteUnconfirmed)
        XCTAssertEqual(cause, "broker timed out")
    }

    /// A `status` word this build has never seen stays in its own case — it is
    /// never read as `timeout` or any other real outcome.
    func testUnrecognisedStatusIsRetainedApartFromEveryOutcome() throws {
        let resolution = try decodeResolution(#"{"status":"teleported"}"#)
        guard case .unrecognisedStatus(let raw) = resolution else {
            return XCTFail("expected .unrecognisedStatus, got \(resolution)")
        }
        XCTAssertEqual(raw, "teleported")
        XCTAssertNotEqual(resolution, .timeout, "an unknown status is not a timeout")
    }

    // MARK: forward-compat

    /// **Rewritten, not deleted.** This used to read
    /// `testSessionSummaryIgnoresAnUnexpectedAgentKey`, and it pinned the
    /// forward-compatibility fact that was true at the time: an old client
    /// decoding a summary from a newer daemon survives, because `JSONDecoder`
    /// ignores keys it was not told about.
    ///
    /// That fact is still true and still worth holding — the second half below
    /// is the same assertion, on a key this build genuinely does not know. What
    /// changed is that `agent` stopped being one of those keys: Phase 5 gave it
    /// a decoded field, because *ignoring* it is exactly what let a Codex
    /// session inherit Claude's whole vocabulary. So the first half now asserts
    /// the opposite of what the old name promised, which is why the name had to
    /// go with it.
    func testSessionSummaryDecodesTheAgentAndStillIgnoresKeysItDoesNotKnow() throws {
        func summary(_ extra: String) throws -> SessionSummary {
            try JSONDecoder().decode(
                SessionSummary.self,
                from: Data(
                    """
                    {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1","cwd":"/x",
                     "lifecycle":"live","link":"attached","last_seq":3,
                     "created_at":"t","updated_at":"t"\(extra)}
                    """.utf8))
        }

        let codex = try summary(#","agent":"codex""#)
        XCTAssertEqual(codex.sessionUID, "u-1")
        XCTAssertEqual(codex.lifecycle, .live)
        XCTAssertEqual(codex.link, .attached)
        XCTAssertEqual(codex.agent, .codex, "agent is decoded now, not ignored")
        XCTAssertTrue(codex.isCodex)

        // The forward-compatibility fact itself, on a key this build really has
        // never heard of: it is ignored, and the summary still decodes whole.
        let future = try summary(#","agent":"claude","telemetry_budget":{"tokens":42}"#)
        XCTAssertEqual(future.agent, .claude)
        XCTAssertEqual(future.sessionUID, "u-1")
    }
}
