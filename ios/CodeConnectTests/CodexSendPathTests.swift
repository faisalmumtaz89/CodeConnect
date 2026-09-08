import XCTest

@testable import CodeConnect

/// **The send path, counted at the socket.**
///
/// Every assertion here is on frames that actually left, recorded by
/// `DaemonConnection.sendStub`. That is the only kind of evidence that
/// distinguishes a real gate from a hidden control — and the round-1 review
/// found two blockers precisely because the existing tests asked the *offering*
/// path ("would the button be shown?") rather than the *transmission* path
/// ("did a frame leave?").
///
/// Both blockers are pinned here, and each of them was measured leaving the
/// socket before the fix:
///
///   * **F1** — `codex_link` was checked when deciding whether to draw a
///     control and not when deciding whether to transmit, so an `interrupt`
///     and a `compose` both left for a session the phone already knew was
///     `bound` / `offline` / `none`.
///   * **F2** — an `indeterminate` outcome retired its request id, so tapping
///     Send again on the *same words* minted a new id and said the same thing
///     to Codex a second time.
@MainActor
final class CodexSendPathTests: XCTestCase {

    // MARK: Harness

    /// Records what reached the socket, and answers with a staged result.
    private final class Wire {
        var frames: [ClientMessage] = []
        /// What to answer each request with, by frame type. Absent ⇒ the request
        /// is left hanging, which is what a lost result frame looks like.
        var interruptResult: InterruptResult?
        var composeResult: ComposeResult?
        /// Set when the stub should refuse to transmit at all — the
        /// `failedBeforeSend` shape.
        var throwOnSend: Error?

        var types: [String] {
            frames.compactMap { message in
                guard let data = try? JSONEncoder().encode(message),
                    let object = try? JSONDecoder().decode(JSONValue.self, from: data)
                else { return nil }
                return object["type"]?.stringValue
            }
        }

        /// Only the two Codex mutations. `subscribe` rides the same stub and is
        /// never what these tests are counting.
        var codexTypes: [String] { types.filter { $0 == "interrupt" || $0 == "compose" } }
    }

    private func model(
        link: String, agent: String = "codex", uid: String = "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
        interrupt: Bool = true, compose: Bool = true, minor: UInt32 = 19
    ) -> (AppModel, Wire) {
        // Its own defaults suite: the spent-material ledger is durable by
        // design, so a shared one would let the first test's uncertainty refuse
        // the second test's first tap.
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(
                canApproveReliably: true, sendText: true,
                extra: [
                    "codex_interrupt": .bool(interrupt), "codex_compose": .bool(compose),
                ]),
            minor: minor)
        model.connection.injectForTesting(
            .sessions([Self.summary(uid: uid, agent: agent, link: link)]))
        // A running turn on the envelope, so Stop has something to name.
        model.connection.injectForTesting(
            .event(Self.turnEvent(uid: uid.isEmpty ? "cc-1" : uid)))

        let wire = Wire()
        model.connection.sendStub = { [weak connection = model.connection] message in
            if let error = wire.throwOnSend { throw error }
            wire.frames.append(message)
            switch message {
            case .interrupt(let session, let requestID, _, _):
                guard let result = wire.interruptResult else { return }
                connection?.injectForTesting(
                    .interruptResult(sessionID: session, requestID: requestID, result: result))
            case .compose(let session, let requestID, _, _):
                guard let result = wire.composeResult else { return }
                connection?.injectForTesting(
                    .composeResult(sessionID: session, requestID: requestID, result: result))
            default:
                break
            }
        }
        return (model, wire)
    }

    private static func summary(uid: String, agent: String, link: String) -> SessionSummary {
        let uidClause = uid.isEmpty ? "" : #""session_uid":"\#(uid)","#
        return try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {\(uidClause)"session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-09-04T21:31:59.000Z",
                 "updated_at":"2026-09-04T21:31:59.000Z","blocked_on":[],
                 "agent":"\(agent)","codex_link":"\(link)"}
                """.utf8))
    }

    private static func turnEvent(uid: String) -> Event {
        try! JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":1,"session_uid":"\(uid)","session_id":"cc-1",
                 "ts":"2026-09-04T21:31:59.000Z","kind":"tool_call","source":"codex",
                 "turn_id":"turn-9","payload":{}}
                """.utf8))
    }

    private func key(_ uid: String = "01K1B3XQ8ZC0DE5FGH7JKMNPQR") -> String { uid }

    // MARK: F1 — the link state is enforced on the SEND path

    /// **Measured leaving the socket before this fix**, for all three states:
    /// `framesSent=["compose","subscribe"]`.
    func testComposeIsNotTransmittedUnlessTheLinkIsSubscribed() async {
        for link in ["bound", "offline", "none"] {
            let (model, wire) = model(link: link)
            _ = await model.composeToCodex(sessionKey: key(), text: "hello")
            XCTAssertEqual(
                wire.codexTypes, [],
                "link=\(link): no compose may leave for a session the phone knows cannot take it")
        }
    }

    /// The same, for Stop. Measured before the fix as
    /// `framesSent=["interrupt","subscribe"]` at `link=offline`.
    func testInterruptIsNotTransmittedUnlessTheLinkIsSubscribed() async {
        for link in ["bound", "offline", "none"] {
            let (model, wire) = model(link: link)
            _ = await model.stopCodexTurn(sessionKey: key())
            XCTAssertEqual(wire.codexTypes, [], "link=\(link): no interrupt may leave")
        }
    }

    /// And `subscribed` transmits exactly one of each — the gate must not have
    /// been closed by shutting everything.
    func testASubscribedLinkTransmitsExactlyOneFrame() async {
        let (composeModel, composeWire) = model(link: "subscribed")
        composeWire.composeResult = .started(turnID: "turn-9")
        _ = await composeModel.composeToCodex(sessionKey: key(), text: "hello")
        XCTAssertEqual(composeWire.codexTypes, ["compose"])

        let (stopModel, stopWire) = model(link: "subscribed")
        stopWire.interruptResult = .aborted(turnID: "turn-9")
        _ = await stopModel.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(stopWire.codexTypes, ["interrupt"])
    }

    /// The refusal the reader sees names the link, in the phone's own words —
    /// the daemon has said nothing, because nothing was sent.
    func testTheLinkRefusalIsTheAppsOwnSentence() async {
        let (model, _) = model(link: "offline")
        _ = await model.composeToCodex(sessionKey: key(), text: "hello")
        guard case .notSent(let reason) = model.codexControls(for: key()).compose else {
            return XCTFail("expected a not-sent outcome")
        }
        XCTAssertEqual(reason, CodexLinkState.offline.blockedReason)
    }

    // MARK: F2 — an indeterminate mutation is never re-sent

    /// **The blocker, as a two-call wire count.** Send `deploy`, receive
    /// `indeterminate`, tap Send again without editing: the second call must put
    /// nothing on the socket.
    func testAnUnchangedComposeIsNotResentAfterIndeterminate() async {
        let (model, wire) = model(link: "subscribed")
        wire.composeResult = .indeterminate(reason: "written, outcome unknown")

        _ = await model.composeToCodex(sessionKey: key(), text: "deploy")
        XCTAssertEqual(wire.codexTypes, ["compose"], "the first attempt is sent")

        _ = await model.composeToCodex(sessionKey: key(), text: "deploy")
        XCTAssertEqual(
            wire.codexTypes, ["compose"],
            "the same words after an indeterminate outcome must never be said twice")
    }

    /// **Changed words are a new message**, and must still go. The rule is
    /// "never resend *this*", not "never speak again".
    func testChangedTextIsStillSentAfterIndeterminate() async {
        let (model, wire) = model(link: "subscribed")
        wire.composeResult = .indeterminate(reason: "written, outcome unknown")

        _ = await model.composeToCodex(sessionKey: key(), text: "deploy")
        _ = await model.composeToCodex(sessionKey: key(), text: "deploy now")
        XCTAssertEqual(wire.codexTypes, ["compose", "compose"])
    }

    /// A stop whose outcome is unknown is not sent again either.
    func testAnUnchangedStopIsNotResentAfterIndeterminate() async {
        let (model, wire) = model(link: "subscribed")
        wire.interruptResult = .indeterminate(reason: "already sent; it will not be sent again")

        _ = await model.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(wire.codexTypes, ["interrupt"])

        _ = await model.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(
            wire.codexTypes, ["interrupt"],
            "the same turn after an indeterminate stop must never be aborted twice")
    }

    /// **`.aborted` retires the turn locally, at once** — before any
    /// `turn_complete` arrives. Otherwise the turn stays "running" in the event
    /// log the phone holds, Stop stays offered, and a second tap sends a second
    /// interrupt at a turn that is already gone.
    func testAStoppedTurnIsNotStoppedAgain() async {
        let (model, wire) = model(link: "subscribed")
        wire.interruptResult = .aborted(turnID: "turn-9")

        _ = await model.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(wire.codexTypes, ["interrupt"])

        XCTAssertNil(
            model.runningTurn(for: key()),
            "an aborted turn is retired locally the moment the daemon says so")
        XCTAssertNotNil(
            model.stopUnavailable(for: key()), "and Stop stops being offered for it")

        _ = await model.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(wire.codexTypes, ["interrupt"], "no second interrupt for a dead turn")
    }

    /// A `duplicate` says the same thing about that turn: it is gone.
    func testADuplicateStopAlsoRetiresTheTurn() async {
        let (model, wire) = model(link: "subscribed")
        wire.interruptResult = .duplicate(turnID: "turn-9")
        _ = await model.stopCodexTurn(sessionKey: key())
        XCTAssertNil(model.runningTurn(for: key()))
        _ = await model.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(wire.codexTypes, ["interrupt"])
    }

    // MARK: F3 — sent-but-unanswered is not "nothing was sent"

    /// **The daemon took the frame and the answer never came back.** Recorded as
    /// not-sent, that is a promise the phone cannot keep: the words may well
    /// have reached Codex. It must read as *sent, no answer heard* — and it must
    /// block an unchanged retry for the same reason `indeterminate` does.
    func testATimedOutComposeReadsAsSentNotAsNothingWasSent() async {
        let (model, wire) = model(link: "subscribed")
        wire.composeResult = nil  // accepted, and nothing ever comes back

        _ = await model.composeToCodex(sessionKey: key(), text: "deploy")
        XCTAssertEqual(wire.codexTypes, ["compose"], "the frame did leave")

        // **Its own activity, not a wire status.** The wire enums mirror
        // `ws.rs` and must stay exactly what the daemon can say; "the daemon
        // took this and I never heard back" is the *app's* observation, so it
        // lives on the activity beside `notSent` rather than being smuggled into
        // `ComposeResult`.
        guard case .sentNoAnswer = model.codexControls(for: key()).compose else {
            return XCTFail(
                "a sent-but-unanswered compose is its own outcome, got "
                    + "\(model.codexControls(for: key()).compose)")
        }

        _ = await model.composeToCodex(sessionKey: key(), text: "deploy")
        XCTAssertEqual(
            wire.codexTypes, ["compose"],
            "words that may already have landed are not said again")
    }

    /// A failure **before** the send is still honestly not-sent.
    func testAFailureBeforeTheSendIsStillNotSent() async {
        let (model, wire) = model(link: "subscribed")
        wire.throwOnSend = ConnectionError.notConnected

        _ = await model.composeToCodex(sessionKey: key(), text: "deploy")
        XCTAssertEqual(wire.codexTypes, [], "nothing left")
        guard case .notSent = model.codexControls(for: key()).compose else {
            return XCTFail("a frame that never left is not-sent")
        }
    }

    // MARK: F7 — D4, the uid or nothing

    /// **Fail closed on an empty uid.** The tmux name is handed to the next run,
    /// so hashing and sending it can aim a mutation at a session the reader
    /// never saw.
    func testAnEmptyUidSendsNothing() async {
        let (model, wire) = model(link: "subscribed", uid: "")
        _ = await model.stopCodexTurn(sessionKey: "cc-1")
        guard case .notSent = model.codexControls(for: "cc-1").stop else {
            return XCTFail("expected a typed not-sent for the stop")
        }
        // **Then the compose**, and in that order: one slot, one question, so
        // the second attempt's answer replaces the first's (G9). Asserting both
        // at once would be asserting the bug the ordered rule removed.
        _ = await model.composeToCodex(sessionKey: "cc-1", text: "hello")
        XCTAssertEqual(wire.codexTypes, [], "no uid, no mutation")
        guard case .notSent(let reason) = model.codexControls(for: "cc-1").compose else {
            return XCTFail("expected a typed not-sent")
        }
        XCTAssertTrue(
            reason.contains("id of its own"),
            "the refusal names the missing fact in the reader's terms: \(reason)")
    }

    /// And what does go carries the uid, never the name.
    func testTheFrameCarriesTheUid() async throws {
        let (model, wire) = model(link: "subscribed")
        wire.composeResult = .started(turnID: "turn-9")
        _ = await model.composeToCodex(sessionKey: key(), text: "hello")
        guard case .compose(let session, _, _, let hash) = wire.frames.first(where: {
            if case .compose = $0 { return true }
            return false
        }) else { return XCTFail("no compose frame") }
        XCTAssertEqual(session, "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        XCTAssertEqual(hash, CodexHash.compose(sessionRef: session, text: "hello"))
    }

    // MARK: F5 — the answer vocabulary follows the session, not the card

    /// A Claude session never transmits `option_id`, whatever `tool_input`
    /// happens to contain — `tool_input` is agent-authored content, not an
    /// agent discriminator.
    func testAClaudeSessionNeverTransmitsAnOptionId() async {
        let (model, wire) = model(link: "none", agent: "claude")
        let item = Self.approvalItem(sessionKey: key(), withOptions: true)
        _ = await model.answer(item: item, decision: .optionId("accept"))
        XCTAssertFalse(
            wire.types.contains("answer"),
            "an option_id aimed at a Claude session is refused by the Mac by name")
    }

    /// And a Codex session never transmits allow/deny.
    func testACodexSessionNeverTransmitsAllowOrDeny() async {
        let (model, wire) = model(link: "subscribed")
        let item = Self.approvalItem(sessionKey: key(), withOptions: true)
        _ = await model.answer(item: item, decision: .allow)
        _ = await model.answer(item: item, decision: .deny)
        XCTAssertFalse(wire.types.contains("answer"))
    }

    // MARK: G5 — the interlock outlives the process

    /// **Quitting the app is not evidence about what the Mac did.**
    ///
    /// `indeterminate` and `sentNoAnswer` both mean the mutation may have
    /// happened and nobody can account for it, so the material must never go
    /// again. It was kept only in `CodexControls`, which dies with the process:
    /// a relaunch forgot it, minted a fresh `request_id`, and the same words
    /// went to Codex a second time — under a new id, so the daemon's own replay
    /// ledger could not catch it either.
    ///
    /// This is the relaunch, done honestly: a second `CodexControls` built from
    /// the persisted store, exactly as `AppModel` builds one on a cold start.
    func testSpentMaterialSurvivesARelaunch() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)"))
        defer { defaults.removePersistentDomain(forName: defaults.description) }
        let ledger = CodexSpentLedger(defaults: defaults)
        let session = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"
        let stopMaterial = CodexHash.interrupt(sessionRef: session, turnID: "turn-9")
        let composeMaterial = CodexHash.compose(sessionRef: session, text: "hello")

        let before = CodexControls(sessionKey: session, ledger: ledger)
        before.beginStop()
        before.settleStop(
            .indeterminate(reason: "already sent"), material: stopMaterial, turnID: "turn-9")
        before.beginCompose()
        before.settleCompose(.indeterminate(reason: "already written"), material: composeMaterial)
        XCTAssertTrue(before.stopIsSpent(material: stopMaterial))

        // The process ends. A new launch reads the store from disk.
        let reloaded = CodexSpentLedger(defaults: defaults)
        let after = CodexControls(sessionKey: session, ledger: reloaded)
        XCTAssertTrue(
            after.stopIsSpent(material: stopMaterial),
            "a relaunch must not be able to abort that turn again")
        XCTAssertTrue(
            after.composeIsSpent(material: composeMaterial),
            "a relaunch must not be able to say those words again")

        // Scoped: another run's identical material is untouched, and a
        // departed run takes its own entries with it.
        let other = CodexControls(sessionKey: "01OTHEROTHEROTHEROTHEROTHE", ledger: reloaded)
        XCTAssertFalse(other.stopIsSpent(material: stopMaterial))
        reloaded.forget(session: session)
        XCTAssertFalse(
            CodexControls(sessionKey: session, ledger: reloaded).stopIsSpent(material: stopMaterial))
    }

    /// And the store is bounded: a safety interlock, not a history nobody can
    /// sweep. The newest entries are the ones that survive.
    func testTheSpentStoreIsBounded() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)"))
        defer { defaults.removePersistentDomain(forName: defaults.description) }
        let ledger = CodexSpentLedger(defaults: defaults)
        for index in 0..<(CodexSpentLedger.capacity + 10) {
            ledger.markSpent(.stop, session: "s-\(index)", material: "m-\(index)")
        }
        let stored = defaults.stringArray(forKey: CodexSpentLedger.defaultsKey) ?? []
        XCTAssertEqual(stored.count, CodexSpentLedger.capacity)
        XCTAssertTrue(ledger.isSpent(.stop, session: "s-57", material: "m-57"), "the newest stay")
        XCTAssertFalse(ledger.isSpent(.stop, session: "s-0", material: "m-0"), "the oldest go")
    }

    // MARK: G3 — an unreadable status is not an actuation claim

    /// **`.unknown` must not spend the material.**
    ///
    /// The arm exists precisely so the phone claims neither "it happened" nor
    /// "it did not". Spending the material made the next tap say *"This was
    /// already sent and what became of it is not known"* — an actuation claim,
    /// in the app's own voice, about a word this build cannot read. A status it
    /// cannot interpret licenses nothing, including a refusal.
    func testAnUnknownStatusDoesNotSpendTheMaterial() async {
        let (stopModel, stopWire) = model(link: "subscribed")
        stopWire.interruptResult = .unknown(status: "quiesced")
        _ = await stopModel.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(stopWire.codexTypes, ["interrupt"])

        _ = await stopModel.stopCodexTurn(sessionKey: key())
        XCTAssertEqual(
            stopWire.codexTypes, ["interrupt", "interrupt"],
            "an unreadable status is not a reason to refuse the next tap")

        let (composeModel, composeWire) = model(link: "subscribed")
        composeWire.composeResult = .unknown(status: "quiesced")
        _ = await composeModel.composeToCodex(sessionKey: key(), text: "hello")
        _ = await composeModel.composeToCodex(sessionKey: key(), text: "hello")
        XCTAssertEqual(composeWire.codexTypes, ["compose", "compose"])
    }

    /// **And the retry goes under the SAME request id.**
    ///
    /// Not spending the material was right; clearing the open id with it was
    /// not. A fresh id is a brand-new mutation as far as the Mac is concerned —
    /// it cannot match it against the first one, so its replay ledger cannot
    /// dedupe it, and if the unreadable status happened to mean *the interrupt
    /// landed*, the second tap actuates a second time. Retaining the id is what
    /// makes the retry safe: same id, same material, so the daemon either
    /// replays its first answer or refuses the duplicate — its choice, on facts
    /// it holds and this phone does not.
    func testAnUnknownStatusKeepsItsRequestId() async throws {
        let (stopModel, stopWire) = model(link: "subscribed")
        stopWire.interruptResult = .unknown(status: "quiesced")
        _ = await stopModel.stopCodexTurn(sessionKey: key())
        _ = await stopModel.stopCodexTurn(sessionKey: key())

        let ids = stopWire.frames.compactMap { message -> String? in
            if case .interrupt(_, let requestID, _, _) = message { return requestID }
            return nil
        }
        XCTAssertEqual(ids.count, 2, "the unchanged retry is allowed")
        XCTAssertEqual(
            ids[0], ids[1],
            "an unchanged retry of an unreadable answer must reuse the id the daemon already saw")

        let (composeModel, composeWire) = model(link: "subscribed")
        composeWire.composeResult = .unknown(status: "quiesced")
        _ = await composeModel.composeToCodex(sessionKey: key(), text: "hello")
        _ = await composeModel.composeToCodex(sessionKey: key(), text: "hello")
        let composeIDs = composeWire.frames.compactMap { message -> String? in
            if case .compose(_, let requestID, _, _) = message { return requestID }
            return nil
        }
        XCTAssertEqual(composeIDs.count, 2)
        XCTAssertEqual(composeIDs[0], composeIDs[1], "the same words keep the same id")

        // And **changed** material still mints a new one: the id is the
        // identity of what is being said, not of the tap.
        _ = await composeModel.composeToCodex(sessionKey: key(), text: "different words")
        let afterChange = composeWire.frames.compactMap { message -> String? in
            if case .compose(_, let requestID, _, _) = message { return requestID }
            return nil
        }
        XCTAssertEqual(afterChange.count, 3)
        XCTAssertNotEqual(
            afterChange[2], afterChange[1],
            "different words are a different thing to say, so a different id")
    }

    /// And the sentence stays neutral rather than becoming "already sent".
    func testTheUnknownSentenceNeverClaimsActuation() {
        let sentence = CodexProse.interrupt(.unknown(status: "quiesced")).message
        XCTAssertTrue(sentence.contains("quiesced"), "the word is named verbatim")
        for claim in ["already sent", "will not be sent again", "was sent"] {
            XCTAssertFalse(
                sentence.localizedCaseInsensitiveContains(claim),
                "\(claim): an unreadable status claims nothing either way")
        }
    }

    /// **A card whose run nobody can describe is not Claude's.**
    ///
    /// `summary(for:) ?? .claude` was the default, and `.claude` is the one
    /// agent whose vocabulary transmits: an `allow` for a run that has left the
    /// fleet, or one the daemon has not described yet, went out on the wire.
    /// Unknown fails closed — every decision, not just the Codex ones.
    func testAnUnknownAgentAnswersNothing() async {
        let (model, wire) = model(link: "subscribed")
        // A card for a run the fleet has never heard of.
        let item = Self.approvalItem(sessionKey: "01ZZZZZZZZZZZZZZZZZZZZZZZZ", withOptions: true)
        for decision in [
            AnswerDecision.allow, .deny, .optionId("accept"), .text("no thanks"),
        ] {
            _ = await model.answer(item: item, decision: decision)
        }
        XCTAssertFalse(
            wire.types.contains("answer"),
            "no decision may be sent for a run whose agent is unknown")
    }

    /// **`.text` is Claude's deny-with-a-reason path**, which types free text
    /// into a composer Codex does not have. It was permitted only because the
    /// control is not drawn on a Codex card — a view fact standing in for a
    /// send-path guarantee.
    func testACodexSessionNeverTransmitsFreeText() async {
        let (model, wire) = model(link: "subscribed")
        let item = Self.approvalItem(sessionKey: key(), withOptions: true)
        _ = await model.answer(item: item, decision: .text("do it differently"))
        XCTAssertFalse(wire.types.contains("answer"))
    }

    /// **F4 on the send path.** Below minor 19 a Codex resolution carries no
    /// `request_id`, so an answered card can never be retired: it stays live and
    /// tappable for ever. The view already refuses to draw the options; this is
    /// the path itself refusing, so a deep link or a stale sheet cannot answer.
    func testAnOldDaemonAnswersNoCodexCard() async {
        for minor in [UInt32(16), 17, 18] {
            let (model, wire) = model(link: "subscribed", minor: minor)
            let item = Self.approvalItem(sessionKey: key(), withOptions: true)
            _ = await model.answer(item: item, decision: .optionId("accept"))
            XCTAssertFalse(
                wire.types.contains("answer"),
                "minor \(minor) cannot retire a Codex card, so it must not answer one")
        }
    }

    private static func approvalItem(sessionKey: String, withOptions: Bool) -> ApprovalItem {
        let input =
            withOptions
            ? #"{"command":"ls","options":[{"id":"accept","label":"Yes"}]}"#
            : #"{"command":"ls"}"#
        let display = "command\n\(input)"
        let card = ApprovalCard(
            requestID: "rid", payloadHash: "h", toolName: "command",
            toolInput: try! JSONDecoder().decode(JSONValue.self, from: Data(input.utf8)),
            displayText: display, permissionSuggestions: nil, promptID: nil,
            permissionMode: nil, risk: nil)
        return ApprovalItem(
            card: card, requestedAt: Date(), sessionKey: sessionKey, outcome: nil,
            codexResolution: nil, paneSnapshot: nil, risk: nil)
    }
}
