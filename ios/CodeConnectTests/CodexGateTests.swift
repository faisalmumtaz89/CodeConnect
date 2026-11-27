import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **The capability gate, and idempotence at the client.**
///
/// The normative rule, and the one thing the Codex controls most owe a test
/// for:
///
/// > The phone **MUST NOT TRANSMIT** `compose` unless
/// > `capabilities.codex_compose == true`. **Hiding the button is not the same
/// > thing.** A phone that sends one to a minor-17 daemon waits for a
/// > `ComposeResult` that is never coming — that daemon cannot decode the
/// > message and answers `Error{code:"bad_request"}` instead.
///
/// So every assertion below is on **frames leaving the socket**, counted through
/// the existing `sendStub` seam, and the send API is driven directly rather than
/// through any view. A test that tapped a button and found nothing happened
/// would pass just as happily against a phone that hid the control and still
/// transmitted from a deep link, a keyboard shortcut, or the next layout.
@MainActor
final class CodexGateTests: XCTestCase {

    /// A connection that has shaken hands, advertising exactly these Codex flags.
    private func connected(interrupt: Bool, compose: Bool, claudeInterrupt: Bool = false)
        -> DaemonConnection
    {
        let connection = DaemonConnection()
        connection.simulateConnectedForTesting()
        connection.simulateCapabilitiesForTesting(
            Capabilities(
                canApproveReliably: true, sendText: true,
                extra: [
                    "codex_interrupt": .bool(interrupt),
                    "codex_compose": .bool(compose),
                    "claude_interrupt": .bool(claudeInterrupt),
                ]),
            minor: 18)
        return connection
    }

    /// Counts what actually reached the socket.
    private final class Wire {
        var frames: [ClientMessage] = []
        var types: [String] {
            frames.compactMap { message in
                guard let data = try? JSONEncoder().encode(message),
                    let object = try? JSONDecoder().decode(JSONValue.self, from: data)
                else { return nil }
                return object["type"]?.stringValue
            }
        }
    }

    private func wired(_ connection: DaemonConnection) -> Wire {
        let wire = Wire()
        connection.sendStub = { message in wire.frames.append(message) }
        return wire
    }

    // MARK: The gate

    /// **Zero frames**, and a typed refusal — not a thrown transport error,
    /// which would say the frame may have gone.
    func testComposeIsNotTransmittedWithoutTheCapability() async {
        let connection = connected(interrupt: true, compose: false)
        let wire = wired(connection)

        do {
            _ = try await connection.compose(
                session: "u-1", requestID: "say-1", text: "hello", payloadHash: "h")
            XCTFail("a compose must not be transmitted to a daemon that cannot decode it")
        } catch let refusal as DaemonConnection.CodexRefusal {
            XCTAssertEqual(refusal, .notAdvertised("codex_compose"))
        } catch {
            XCTFail("expected a typed refusal, got \(error)")
        }
        XCTAssertTrue(wire.frames.isEmpty, "sent \(wire.types)")
    }

    func testInterruptIsNotTransmittedWithoutTheCapability() async {
        let connection = connected(interrupt: false, compose: true)
        let wire = wired(connection)

        do {
            _ = try await connection.interrupt(
                session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h",
                requires: .codex)
            XCTFail("an interrupt must not be transmitted to a daemon that would always refuse it")
        } catch let refusal as DaemonConnection.CodexRefusal {
            XCTAssertEqual(refusal, .notAdvertised("codex_interrupt"))
        } catch {
            XCTFail("expected a typed refusal, got \(error)")
        }
        XCTAssertTrue(wire.frames.isEmpty, "sent \(wire.types)")
    }

    /// With the flag, exactly one frame, carrying all four required fields.
    func testComposeIsTransmittedWithTheCapability() async throws {
        let connection = connected(interrupt: true, compose: true)
        let wire = wired(connection)

        let sent = expectation(description: "one compose left the socket")
        connection.sendStub = { message in
            wire.frames.append(message)
            sent.fulfill()
        }
        let pending = Task { @MainActor in
            try await connection.compose(
                session: "u-1", requestID: "say-1", text: "hello", payloadHash: "h")
        }
        await fulfillment(of: [sent], timeout: 2)

        XCTAssertEqual(wire.types, ["compose"])
        let object = try XCTUnwrap(
            try JSONDecoder()
                .decode(JSONValue.self, from: JSONEncoder().encode(wire.frames[0])).objectValue)
        XCTAssertEqual(
            Set(object.keys), ["type", "session_id", "request_id", "text", "payload_hash"])

        connection.injectForTesting(
            .composeResult(
                sessionID: "u-1", requestID: "say-1", result: .started(turnID: "t-1")))
        let result = try await pending.value
        XCTAssertEqual(result, .started(turnID: "t-1"))
    }

    func testInterruptIsTransmittedWithTheCapability() async throws {
        let connection = connected(interrupt: true, compose: true)
        let wire = Wire()
        let sent = expectation(description: "one interrupt left the socket")
        connection.sendStub = { message in
            wire.frames.append(message)
            sent.fulfill()
        }
        let pending = Task { @MainActor in
            try await connection.interrupt(
                session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h",
                requires: .codex)
        }
        await fulfillment(of: [sent], timeout: 2)
        XCTAssertEqual(wire.types, ["interrupt"])

        connection.injectForTesting(
            .interruptResult(
                sessionID: "u-1", requestID: "stop-1", result: .aborted(turnID: "t-1")))
        let result = try await pending.value
        XCTAssertEqual(result, .aborted(turnID: "t-1"))
    }

    /// A hosted Claude Stop is gated on `claude_interrupt`, not `codex_interrupt`:
    /// a daemon that answers Codex but not Claude must not let a Claude frame leave.
    func testClaudeInterruptIsNotTransmittedWithoutTheCapability() async {
        let connection = connected(interrupt: true, compose: true, claudeInterrupt: false)
        let wire = wired(connection)

        do {
            _ = try await connection.interrupt(
                session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h",
                requires: .claude)
            XCTFail("a Claude interrupt must not be transmitted without claude_interrupt")
        } catch let refusal as DaemonConnection.CodexRefusal {
            XCTAssertEqual(refusal, .notAdvertised("claude_interrupt"))
        } catch {
            XCTFail("expected a typed refusal, got \(error)")
        }
        XCTAssertTrue(wire.frames.isEmpty, "sent \(wire.types)")
    }

    func testClaudeInterruptIsTransmittedWithTheCapability() async throws {
        let connection = connected(interrupt: false, compose: false, claudeInterrupt: true)
        let wire = Wire()
        let sent = expectation(description: "one Claude interrupt left the socket")
        connection.sendStub = { message in
            wire.frames.append(message)
            sent.fulfill()
        }
        let pending = Task { @MainActor in
            try await connection.interrupt(
                session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h",
                requires: .claude)
        }
        await fulfillment(of: [sent], timeout: 2)
        XCTAssertEqual(wire.types, ["interrupt"])

        connection.injectForTesting(
            .interruptResult(
                sessionID: "u-1", requestID: "stop-1", result: .aborted(turnID: "t-1")))
        let result = try await pending.value
        XCTAssertEqual(result, .aborted(turnID: "t-1"))
    }

    /// **The gate is the send path, not the control.** This drives the API with
    /// no view anywhere in the picture, which is precisely what distinguishes a
    /// real gate from a hidden button.
    func testHidingTheButtonIsNotTheGate() async {
        let connection = connected(interrupt: false, compose: false)
        let wire = wired(connection)
        _ = try? await connection.compose(
            session: "u-1", requestID: "say-1", text: "x", payloadHash: "h")
        _ = try? await connection.interrupt(
            session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h",
            requires: .codex)
        XCTAssertTrue(
            wire.frames.isEmpty,
            "no view was involved and nothing may leave: sent \(wire.types)")
    }

    /// The `Capabilities` rule this inherits: every default is "not available",
    /// so an ack with no Codex keys can only ever hide something.
    func testAnAbsentFlagIsFalse() throws {
        let ack = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                """
                {"type":"hello_ack","protocol_version":1,"protocol_minor":17,
                 "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
                   "answer_path":"send_keys","send_text":true}}
                """.utf8))
        guard case .helloAck(let helloAck) = ack else { return XCTFail("expected an ack") }
        XCTAssertFalse(helloAck.capabilities.codexInterrupt)
        XCTAssertFalse(helloAck.capabilities.codexCompose)
        // And `supported_agents` is omitted entirely while it would only name
        // Claude, so empty reads as `["claude"]` rather than as "no agents".
        XCTAssertEqual(helloAck.capabilities.supportedAgents, ["claude"])
        XCTAssertFalse(helloAck.capabilities.hostsCodex)
    }

    // MARK: The gate is per session too

    /// A Claude session never transmits either message **even when both flags
    /// are true**: the flags are connection-global and build-shaped, and say
    /// nothing about one run. The Mac refuses both by name, so a phone that sent
    /// one would be spending a round trip to be told something it already knew.
    func testTheGateIsPerSessionToo() async throws {
        let model = AppModel(
            cache: EventCache(),
            // Its own defaults suite: the spent-material ledger is durable by
            // design, so a shared one carries one run's uncertainty into the
            // next one's first tap. See `CodexSpentLedger`.
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]),
            minor: 18)
        model.connection.injectForTesting(
            .sessions([
                Self.summary(uid: "u-claude", agent: "claude", link: "none"),
                Self.summary(uid: "u-codex", agent: "codex", link: "subscribed"),
            ]))
        // Installed AFTER the fleet lands, so the subscriptions it triggers are
        // not counted as Codex traffic.
        let wire = wired(model.connection)

        XCTAssertNotNil(
            model.composeUnavailable(for: "u-claude"),
            "a Claude session is not spoken to this way")
        _ = await model.composeToCodex(sessionKey: "u-claude", text: "hello")
        _ = await model.stopCodexTurn(sessionKey: "u-claude")
        XCTAssertFalse(
            wire.types.contains { $0 == "compose" || $0 == "interrupt" },
            "no Codex mutation may leave for a Claude session: sent \(wire.types)")
    }

    /// And the link state gates it too: only `subscribed` can
    /// actuate, and the other three are told before the tap rather than after.
    func testOnlyASubscribedLinkIsOffered() {
        let model = AppModel(
            cache: EventCache(),
            // Its own defaults suite: the spent-material ledger is durable by
            // design, so a shared one carries one run's uncertainty into the
            // next one's first tap. See `CodexSpentLedger`.
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]),
            minor: 18)
        model.connection.injectForTesting(
            .sessions([
                Self.summary(uid: "u-sub", agent: "codex", link: "subscribed"),
                Self.summary(uid: "u-none", agent: "codex", link: "none"),
            ]))
        XCTAssertNil(model.composeUnavailable(for: "u-sub"))
        XCTAssertNotNil(model.composeUnavailable(for: "u-none"))
    }

    // MARK: 1.5 — idempotence at the client

    /// **A retry of an unacknowledged compose reuses its id and its hash.**
    /// That is exactly what makes the daemon replay the recorded outcome rather
    /// than say the same thing to Codex twice.
    func testOneRequestIdPerComposeAttempt() {
        let controls = CodexControls()
        let hash = CodexHash.compose(sessionRef: "u-1", text: "run the tests")
        let first = controls.composeRequestID(material: hash)
        let retry = controls.composeRequestID(material: hash)
        XCTAssertEqual(first, retry, "an unedited retry is a replay, not a second message")
    }

    /// **Editing the text mints a new id**, because the daemon refuses a reused
    /// id carrying different material — and it is right to: those are two
    /// different things to say.
    func testChangedTextEarnsANewRequestId() {
        let controls = CodexControls()
        let first = controls.composeRequestID(
            material: CodexHash.compose(sessionRef: "u-1", text: "run the tests"))
        let edited = controls.composeRequestID(
            material: CodexHash.compose(sessionRef: "u-1", text: "run the tests now"))
        XCTAssertNotEqual(first, edited)
    }

    /// The same rule for a stop, keyed on the turn: *"this request id was used
    /// to stop a different turn, so nothing was sent; ask again under a new one"*.
    func testANewTurnEarnsANewStopRequestId() {
        let controls = CodexControls()
        let first = controls.stopRequestID(
            material: CodexHash.interrupt(sessionRef: "u-1", turnID: "t-1"))
        XCTAssertEqual(
            controls.stopRequestID(
                material: CodexHash.interrupt(sessionRef: "u-1", turnID: "t-1")),
            first)
        XCTAssertNotEqual(
            controls.stopRequestID(
                material: CodexHash.interrupt(sessionRef: "u-1", turnID: "t-2")),
            first)
    }

    /// A settled compose retires its id, so the *next* message is a new one
    /// rather than a replay of the last.
    func testASettledComposeRetiresItsId() {
        let controls = CodexControls()
        let hash = CodexHash.compose(sessionRef: "u-1", text: "same words")
        let first = controls.composeRequestID(material: hash)
        controls.settleCompose(.started(turnID: "t-1"), material: hash)
        XCTAssertNotEqual(
            controls.composeRequestID(material: hash), first,
            "saying the same thing twice on purpose is a new message")
    }

    /// A **refusal** does not retire it: nothing was sent, so an unedited retry
    /// is still the same first attempt.
    func testARefusedComposeKeepsItsId() {
        let controls = CodexControls()
        let hash = CodexHash.compose(sessionRef: "u-1", text: "words")
        let first = controls.composeRequestID(material: hash)
        controls.settleCompose(.rejected(reason: "the computer has lost its control link"), material: hash)
        XCTAssertEqual(controls.composeRequestID(material: hash), first)
    }

    /// **`indeterminate` is never retried automatically**, and the phone must
    /// not offer a retry that resends. The id is retired precisely so a later,
    /// deliberate message is a new one rather than a silent replay of a write
    /// whose outcome nobody knows.
    /// **The material is retained, not the id.** Retiring the id was the bug:
    /// the next attempt minted a fresh one and looked brand new to the daemon.
    /// The wire-level proof is `CodexSendPathTests`; this is the unit.
    func testIndeterminateSpendsTheMaterial() {
        let controls = CodexControls()
        let hash = CodexHash.compose(sessionRef: "u-1", text: "deploy")
        _ = controls.composeRequestID(material: hash)
        controls.settleCompose(.indeterminate(reason: "written, outcome unknown"), material: hash)
        XCTAssertTrue(controls.composeIsSpent(material: hash))
        XCTAssertFalse(
            controls.composeIsSpent(
                material: CodexHash.compose(sessionRef: "u-1", text: "deploy now")),
            "different words are a different message")
    }

    // MARK: The bounded grey

    /// A link-state refusal greys Stop for a **bounded** window, then gives it
    /// back: the same ask may work shortly, so a permanent grey would contradict
    /// the sentence printed above it.
    func testStopGreysAndRecovers() {
        let controls = CodexControls()
        let now = Date()
        controls.settleStop(
            .rejected(reason: "CodeConnect has no connection to this session right now, so nothing was sent."),
            material: "m", turnID: "t-1", now: now)
        XCTAssertTrue(controls.isStopGreyed(now: now.addingTimeInterval(1)))
        XCTAssertTrue(controls.isStopGreyed(now: now.addingTimeInterval(9)))
        XCTAssertFalse(
            controls.isStopGreyed(now: now.addingTimeInterval(10)), "ten seconds, not eleven")
        XCTAssertFalse(
            controls.isStopGreyed(now: now.addingTimeInterval(11)),
            "the grey is a cooldown, not a verdict")
    }

    /// **The clock is read only while a grey stands.** The fleet asks this for
    /// every row on every pass, and `model.now` ticks every second: read whether
    /// or not anything was greyed, it redrew every row of an idle fleet once a
    /// second.
    func testTheClockIsReadOnlyWhileAGreyStands() {
        let controls = CodexControls()
        let now = Date()
        var reads = 0
        let clock = { () -> Date in
            reads += 1
            return now.addingTimeInterval(1)
        }
        XCTAssertFalse(controls.isStopGreyed(now: clock()))
        XCTAssertEqual(reads, 0, "nothing greyed, so nothing to time")
        controls.settleStop(
            .rejected(reason: "CodeConnect has no connection to this session right now, so nothing was sent."),
            material: "m", turnID: "t-1", now: now)
        XCTAssertTrue(controls.isStopGreyed(now: clock()))
        XCTAssertEqual(reads, 1, "a standing grey is timed")

        controls.expireStopGrey(now: now.addingTimeInterval(9))
        XCTAssertTrue(controls.isStopGreyed(now: now.addingTimeInterval(9)), "not yet up")
        controls.expireStopGrey(now: now.addingTimeInterval(10))
        reads = 0
        XCTAssertFalse(controls.isStopGreyed(now: clock()))
        XCTAssertEqual(reads, 0, "a grey whose time is up is no longer timed")

        controls.settleStop(
            .rejected(reason: "CodeConnect has no connection to this session right now, so nothing was sent."),
            material: "m", turnID: "t-1", now: now)
        controls.beginCompose()
        XCTAssertFalse(controls.isStopGreyed(now: clock()))
        XCTAssertEqual(reads, 0, "a compose that left clears the grey, and its clock")
    }

    /// A refusal the reader can fix **now** does not grey: a stale hash, an
    /// empty turn or a reused id are all fixed by a fresh attempt, and ten
    /// seconds of dead control would only make the fix take longer.
    func testANonLinkRefusalDoesNotGrey() {
        let controls = CodexControls()
        let now = Date()
        controls.settleStop(
            .rejected(reason: "this request names no turn, so there is nothing to stop"),
            material: "m", turnID: "t-1", now: now)
        XCTAssertFalse(controls.isStopGreyed(now: now.addingTimeInterval(1)))
    }

    /// **`indeterminate` never greys.** Nothing about it says to wait, and a
    /// grey would read as "try again in a moment" for an outcome the phone has
    /// been told explicitly not to retry.
    func testIndeterminateNeverGreys() {
        let controls = CodexControls()
        let now = Date()
        controls.settleStop(
            .indeterminate(
                reason:
                    "this interrupt was already sent and what became of it is not known; "
                    + "it will not be sent again. Check the computer."),
            material: "m", turnID: "t-1", now: now)
        XCTAssertFalse(controls.isStopGreyed(now: now.addingTimeInterval(1)))
    }

    /// **An outcome belongs to the turn it answered.** Whatever became of the
    /// last stop, a turn that started since is offered Stop; the turn it was
    /// aimed at keeps the answer, and so does a session with no turn running.
    func testAStopsOutcomeBelongsToTheTurnItAnswered() {
        let outcomes: [(String, (CodexControls) -> Void)] = [
            ("stopped", { $0.beginStop(turn: "t-1"); $0.settleStop(.aborted(turnID: "t-1"), material: "m1", turnID: "t-1") }),
            ("already stopped", { $0.beginStop(turn: "t-1"); $0.settleStop(.duplicate(turnID: "t-1"), material: "m2", turnID: "t-1") }),
            ("refused", { $0.beginStop(turn: "t-1"); $0.settleStop(.rejected(reason: "the computer has lost its control link"), material: "m3", turnID: "t-1") }),
            ("outcome unknown", { $0.beginStop(turn: "t-1"); $0.settleStop(.indeterminate(reason: "the link's connection to Codex ended"), material: "m4", turnID: "t-1") }),
            ("unclear", { $0.beginStop(turn: "t-1"); $0.settleStop(.unknown(status: "paused"), material: "m5", turnID: "t-1") }),
            ("not sent", { $0.stopNotSent("This was already sent and what became of it is not known.", turn: "t-1") }),
            ("no answer", { $0.beginStop(turn: "t-1"); $0.stopSentNoAnswer("The computer never answered.", material: "m6") }),
        ]
        for (name, answer) in outcomes {
            let controls = CodexControls()
            answer(controls)
            XCTAssertNotEqual(controls.stop(forRunningTurn: "t-1"), .idle, "\(name): the turn it answered keeps it")
            XCTAssertNotEqual(controls.stop(forRunningTurn: nil), .idle, "\(name): with no turn running it stays")
            XCTAssertEqual(controls.stop(forRunningTurn: "t-2"), .idle, "\(name): a newer turn is offered Stop")
        }
        // Refused before any turn could be named: a turn that runs is offered Stop.
        let early = CodexControls()
        early.stopNotSent("Nothing is running to stop.")
        XCTAssertEqual(early.stop(forRunningTurn: "t-1"), .idle)
        XCTAssertNotEqual(early.stop(forRunningTurn: nil), .idle)
    }

    /// **A stop the phone refuses answers the turn it was pressed on.** The
    /// link went down between what the row drew and the tap, so nothing left;
    /// that "nothing was sent" stands in Stop's place while the turn runs on,
    /// through the same send path and the same accessor the row reads.
    func testAStopRefusedOnThePhoneAnswersTheRunningTurn() async throws {
        let refusal = try XCTUnwrap(CodexLinkState.none.blockedReason(for: .stop))
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]),
            minor: 19)
        // The fixture's frames without its hello: an ack opens a catch-up, and
        // a catch-up holds the screen this test reads.
        for frame in CodexFixtures.frames(state: .stopLinkDown) {
            if case .helloAck = frame { continue }
            model.connection.injectForTesting(frame)
        }
        // What arrived reaches the screen in each run's coalesced rebuild.
        for state in model.statesForTesting.values { await state.settleForTesting() }
        let key = CodexFixtures.sessionKey
        XCTAssertEqual(model.shown(for: key)?.runningTurn, CodexFixtures.turnID, "a turn is running")

        _ = await model.stopCodexTurn(sessionKey: key)
        XCTAssertEqual(model.codexStop(for: key), .notSent(refusal))
    }

    /// **One slot, one question, no exceptions.**
    ///
    /// `beginStop`/`beginCompose` cleared the other outcome, so a mutation that
    /// actually left the phone replaced the previous banner. A mutation refused
    /// *before* that — the link is down, the words are empty, the material is
    /// spent — never reached `begin`, so its "nothing was sent" was written
    /// underneath a stale Stop refusal and the reader saw the older, louder one.
    /// Every outcome is the latest answer to the same question.
    func testTheLatestOutcomeIsTheOnlyOutcome() {
        let controls = CodexControls()
        controls.beginStop()
        controls.settleStop(
            .rejected(reason: "the computer has lost its control link"), material: "m", turnID: "t-1")
        guard case .settled = controls.stop else { return XCTFail("the stop was answered") }

        // A compose refused before it could begin: the stop's banner must go.
        controls.composeNotSent("There is no live link to this Codex session.")
        guard case .idle = controls.stop else {
            return XCTFail("a newer outcome must not be hidden behind an older one")
        }
        guard case .notSent = controls.compose else { return XCTFail("the compose said why") }

        // And the same in the other direction.
        controls.beginCompose()
        controls.settleCompose(.started(turnID: "t-2"), material: "m2")
        controls.stopNotSent("Nothing is running to stop.")
        guard case .idle = controls.compose else {
            return XCTFail("the compose's outcome is not the latest one any more")
        }
    }

    // MARK: R2T3-F1 — a message to a Codex run while a card waits

    private static let cardWaitLine = "Answer the card first. Your draft will stay here."

    private func codexFleet(agent: String = "codex", link: String = "subscribed", pending: Bool)
        -> AppModel
    {
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]),
            minor: 18)
        model.connection.injectForTesting(
            .sessions([Self.summary(uid: "u", agent: agent, link: link, pending: pending)]))
        return model
    }

    /// The live-event order: the card arrives as an `approval_request` event and is built into the
    /// session's shown `pendingApprovals` (on a fresh model, so it keeps its shown state) BEFORE the
    /// fleet summary that would name it in `summary.pending` arrives — and that summary, when it does,
    /// still has `pending` empty. This is the window app-qa first reproduced.
    private func codexFleetWithShownCard(agent: String = "codex", link: String = "subscribed")
        -> AppModel
    {
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]),
            minor: 18)
        // The card arrives first as a live event (catchUp nil, so the shown state keeps it). Force the
        // coalesced rebuild synchronously (`rebuildNow`, as the submit path does) rather than awaiting the
        // scheduler, which starves under a full suite's load. The fleet summary that would name it in
        // `pending` arrives after, still empty — the live-event window.
        model.connection.injectForTesting(Self.cardEvent(uid: "u", seq: 2, id: "u-card"))
        model.statesForTesting["u"]?.rebuildNow()
        model.connection.injectForTesting(
            .sessions([Self.summary(uid: "u", agent: agent, link: link, pending: false)]))
        model.statesForTesting["u"]?.rebuildNow()
        return model
    }

    /// (gate 1) A card waiting on a Codex run blocks compose: the line is shown and no
    /// frame leaves; the draft is the view's to keep (it clears only on a landed result).
    func testACardWaitingOnACodexRunBlocksCompose() async {
        let model = codexFleet(pending: true)
        XCTAssertEqual(model.composeUnavailable(for: "u"), Self.cardWaitLine)
        let wire = wired(model.connection)
        let result = await model.composeToCodex(sessionKey: "u", text: "steer me")
        XCTAssertNil(result, "a blocked compose does not send")
        XCTAssertFalse(wire.types.contains { $0 == "compose" }, "no compose frame left: \(wire.types)")
    }

    /// (gate 2) The submit race: the card is named in `summary.pending` but no
    /// approval_request event has been built into `pendingApprovals`, yet the submit is
    /// still refused and no frame leaves.
    func testTheSubmitIsRefusedWhenTheCardIsKnownButNotYetBuilt() async {
        let model = codexFleet(pending: true)
        XCTAssertTrue(
            model.shown(for: "u")?.pendingApprovals.isEmpty ?? true,
            "no card is built from an event — only the summary names it")
        let wire = wired(model.connection)
        let result = await model.composeToCodex(sessionKey: "u", text: "steer me")
        XCTAssertNil(result, "the submit refuses on the summary's pending ref alone")
        XCTAssertFalse(wire.types.contains { $0 == "compose" }, "no compose frame left: \(wire.types)")
    }

    /// (gate 3) Claude is left alone: a waiting card does not raise the Codex card-wait line.
    func testACardWaitingOnAClaudeRunKeepsClaudeUnchanged() {
        let model = codexFleet(agent: "claude", link: "none", pending: true)
        XCTAssertNotEqual(
            model.composeUnavailable(for: "u"), Self.cardWaitLine, "the card-wait rule is Codex-only")
    }

    /// (gate 4 source) The composer's standing reason carries the card-wait line, so the
    /// note slot can show it (the view prefers it over a stale outcome).
    func testTheCardWaitLineIsTheStandingReason() {
        let model = codexFleet(pending: true)
        XCTAssertEqual(model.sendBlockedReason(for: "u", draft: "hi"), Self.cardWaitLine)
    }

    /// (gate 5) The live-event order — the one app-qa first reproduced: the card arrives as an
    /// `approval_request` event and is built into `pendingApprovals` a fleet round trip BEFORE
    /// `summary.pending` catches up. For that window the gate must still hold on the shown card, or
    /// Send/Return steers the turn the No then aborts and the words are lost.
    func testACardShownFromAnEventBlocksComposeBeforeTheSummaryCatchesUp() async {
        let model = codexFleetWithShownCard()
        XCTAssertFalse(
            model.shown(for: "u")?.pendingApprovals.isEmpty ?? true,
            "the approval_request event built the card into pendingApprovals")
        XCTAssertTrue(
            model.shownSummary(for: "u")?.pending.isEmpty ?? false,
            "the fleet summary has not caught up — summary.pending is still empty")
        XCTAssertEqual(
            model.composeUnavailable(for: "u"), Self.cardWaitLine,
            "the shown card blocks compose even before summary.pending catches up")
        let wire = wired(model.connection)
        let result = await model.composeToCodex(sessionKey: "u", text: "steer me")
        XCTAssertNil(result, "the submit refuses on the shown card")
        XCTAssertFalse(wire.types.contains { $0 == "compose" }, "no compose frame left: \(wire.types)")
    }

    // Claude-unchanged for the shown-card half is covered structurally: the `cardShown` branch sits
    // inside the same `summary.isCodex` guard that `testACardWaitingOnAClaudeRunKeepsClaudeUnchanged`
    // exercises (a mutant dropping `isCodex` fails that gate). A dedicated Claude+shown-card test was
    // order-dependent — a Claude summary injected after the card event reshaped the session's window
    // and lost the projection — so it is omitted rather than left flaky.

    /// **A just-taken Stop's outcome outranks the "answer the card first" block.** On a Codex run with a
    /// card waiting, the compose slot shows the block — but right after the reader taps Stop, the slot
    /// must show the Stop's own result, not the standing block about a different thing. Red before the fix
    /// (the block won unconditionally); the slot's precedence is `composeOutcomeOrBlock`.
    func testAJustTakenStopOutranksTheCardBlockInTheComposeSlot() async {
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]), minor: 19)
        model.connection.injectForTesting(
            .sessions([Self.summary(uid: "u", agent: "codex", link: "subscribed", pending: true)]))
        // A running turn, so Stop has one to name.
        model.connection.injectForTesting(
            .event(try! JSONDecoder().decode(Event.self, from: Data(
                #"{"seq":1,"session_uid":"u","session_id":"u","ts":"2026-09-04T21:31:59.000Z","kind":"tool_call","source":"codex","turn_id":"t1","payload":{}}"#.utf8))))
        // The settled Stop lands when the interrupt is sent.
        model.connection.sendStub = { [weak connection = model.connection] message in
            if case .interrupt(let session, let requestID, _, _) = message {
                connection?.injectForTesting(
                    .interruptResult(sessionID: session, requestID: requestID, result: .aborted(turnID: "t1")))
            }
        }
        // Before any action, the card block is in the slot.
        XCTAssertEqual(
            model.composeOutcomeOrBlock(for: "u", draft: "", lastSend: nil)?.text,
            "Answer the card first. Your draft will stay here.", "the card block shows before any action")
        // The reader taps Stop; its outcome settles.
        _ = await model.stopCodexTurn(sessionKey: "u")
        let note = model.composeOutcomeOrBlock(for: "u", draft: "", lastSend: nil)
        XCTAssertNotEqual(
            note?.text, "Answer the card first. Your draft will stay here.",
            "the just-taken Stop outranks the card block")
        XCTAssertEqual(
            note?.text, model.outcomeNote(for: "u", lastSend: nil)?.text,
            "the slot shows the Stop's just-taken outcome")
    }

    // MARK: Helpers

    /// An `approval_request` event (daemon shape, as `CatchUpTests`), so the card is built into
    /// `pendingApprovals` on ingest — the live-event order, before any fleet summary names it.
    private static func cardEvent(uid: String, seq: Int, id: String) -> ServerMessage {
        try! JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                """
                {"type":"event","event":{"seq":\(seq),"session_uid":"\(uid)","session_id":"\(uid)",
                 "ts":"2026-09-04T21:32:00.000Z","kind":"approval_request","source":"hook",
                 "payload":{"card":{"request_id":"\(id)","payload_hash":"h","tool_name":"Bash",
                 "tool_input":{"command":"ls"},"display_text":"Bash"}}}}
                """.utf8))
    }

    private static func summary(uid: String, agent: String, link: String, pending: Bool = false)
        -> SessionSummary
    {
        // A waiting card is named by a `pending` ref — present even before its
        // approval_request event is ingested and built into `pendingApprovals`.
        let pendingJSON = pending
            ? ",\"pending\":[{\"request_id\":\"\(uid)-card\",\"seq\":2,\"turn_id\":\"t1\"}]" : ""
        return try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"\(uid)",
                 "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-09-04T21:31:59.000Z",
                 "updated_at":"2026-09-04T21:31:59.000Z","blocked_on":[]\(pendingJSON),
                 "agent":"\(agent)","codex_link":"\(link)"}
                """.utf8))
    }
}
