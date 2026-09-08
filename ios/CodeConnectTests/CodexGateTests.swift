import XCTest

@testable import CodeConnect

/// **Tier 1.4 and 1.5 — the capability gate, and idempotence at the client.**
///
/// The contract's own normative rule, and the one thing this phase most owes a
/// test for:
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
    private func connected(interrupt: Bool, compose: Bool) -> DaemonConnection {
        let connection = DaemonConnection()
        connection.simulateConnectedForTesting()
        connection.simulateCapabilitiesForTesting(
            Capabilities(
                canApproveReliably: true, failMode: "fail_open", answerPath: .codexResponse,
                sendText: true,
                extra: [
                    "codex_interrupt": .bool(interrupt),
                    "codex_compose": .bool(compose),
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
                session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h")
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
                session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h")
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
            session: "u-1", requestID: "stop-1", turnID: "t-1", payloadHash: "h")
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

    /// And the link state gates it too (decision D3): only `subscribed` can
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
                Self.summary(uid: "u-bound", agent: "codex", link: "bound"),
                Self.summary(uid: "u-off", agent: "codex", link: "offline"),
                Self.summary(uid: "u-none", agent: "codex", link: "none"),
            ]))
        XCTAssertNil(model.composeUnavailable(for: "u-sub"))
        for key in ["u-bound", "u-off", "u-none"] {
            XCTAssertNotNil(model.composeUnavailable(for: key), key)
        }
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
        controls.settleCompose(.rejected(reason: "this Mac has lost its control link"), material: hash)
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

    // MARK: The bounded grey (decision D3/D7)

    /// A link-state refusal greys Stop for a **bounded** window, then gives it
    /// back — four of the daemon's own sentences end in "try again shortly", so
    /// a permanent grey would contradict the sentence printed above it.
    func testStopGreysAndRecovers() {
        let controls = CodexControls()
        let now = Date()
        controls.settleStop(
            .rejected(
                reason:
                    "this Mac has lost its control link to the Codex session and is reconnecting, "
                    + "so nothing was sent; try again shortly, or stop the turn at the Mac"),
            material: "m", turnID: "t-1", now: now)
        XCTAssertTrue(controls.isStopGreyed(now: now.addingTimeInterval(1)))
        XCTAssertTrue(controls.isStopGreyed(now: now.addingTimeInterval(9)))
        XCTAssertFalse(
            controls.isStopGreyed(now: now.addingTimeInterval(11)),
            "the grey is a cooldown, not a verdict")
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
                    + "it will not be sent again. Check the Mac."),
            material: "m", turnID: "t-1", now: now)
        XCTAssertFalse(controls.isStopGreyed(now: now.addingTimeInterval(1)))
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
            .rejected(reason: "this Mac has lost its control link"), material: "m", turnID: "t-1")
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

    // MARK: Helpers

    private static func summary(uid: String, agent: String, link: String) -> SessionSummary {
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"\(uid)","tmux_session":"\(uid)",
                 "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-09-04T21:31:59.000Z",
                 "updated_at":"2026-09-04T21:31:59.000Z","blocked_on":[],
                 "agent":"\(agent)","codex_link":"\(link)"}
                """.utf8))
    }
}
