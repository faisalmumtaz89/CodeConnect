import XCTest

@testable import CodeConnect

/// The Swift half of `protocol/src/hash.rs::send_text_hash`. One pinned
/// literal on each side is what proves the two implementations are the same
/// function rather than two functions that agree on easy inputs.
final class SendTextIdentityTests: XCTestCase {

    func testTheCrossLanguageVectorMatchesTheDaemon() {
        XCTAssertEqual(
            SendTextIdentity.payloadHash(session: "cc-1", text: "hi", submit: true),
            "832d56d28203c01645209f9b61d192de468301d1c2bdb6090a607b15ab8026a9",
            "the same literal is asserted in hash.rs; a drift on either side fails one of them")
    }

    func testEveryPartOfTheMutationMoves() {
        let base = SendTextIdentity.payloadHash(session: "cc-1", text: "hi", submit: true)
        XCTAssertNotEqual(
            base, SendTextIdentity.payloadHash(session: "cc-2", text: "hi", submit: true))
        XCTAssertNotEqual(
            base, SendTextIdentity.payloadHash(session: "cc-1", text: "ho", submit: true))
        XCTAssertNotEqual(
            base, SendTextIdentity.payloadHash(session: "cc-1", text: "hi", submit: false))
    }

    /// The length prefixes are load-bearing: with plain separators these two
    /// mutations would hash identically, and one could answer for the other.
    func testAMovedNewlineIsADifferentMutation() {
        XCTAssertNotEqual(
            SendTextIdentity.payloadHash(session: "cc-1", text: "true\nx", submit: true),
            SendTextIdentity.payloadHash(session: "cc-1\ntrue", text: "x", submit: true))
    }
}

/// The retry ledger's client half: an identity travels with every send, and
/// after "nobody knows" the *same* identity travels again — that is what lets
/// the daemon recognise a retry instead of typing twice.
@MainActor
final class SendIdempotencyTests: XCTestCase {

    private func connectedModel(
        listing keys: [String] = ["u-1"],
        answering result: @escaping @Sendable (String, String) async throws -> SendTextResult
    ) -> AppModel {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.sendTextStub = result
        list(keys, in: model)
        return model
    }

    /// Unsettled identities are retained only for sessions the daemon still
    /// lists — so every retention test needs its sessions on the fleet, the
    /// way a real session always is while its screen is open.
    private func list(_ keys: [String], in model: AppModel) {
        let rows = keys.map { key in
            #"{"session_uid":"\#(key)","session_id":"cc-1","tmux_session":"cc-1","cwd":"/tmp","lifecycle":"live","link":"attached","last_seq":0,"created_at":"2026-08-04T20:00:00.000Z","updated_at":"2026-08-04T20:00:00.000Z"}"#
        }
        let frame = #"{"type":"sessions","sessions":[\#(rows.joined(separator: ","))]}"#
        model.connection.injectForTesting(
            try! JSONDecoder().decode(ServerMessage.self, from: Data(frame.utf8)))
    }

    private func identities(_ model: AppModel) -> [(requestID: String?, payloadHash: String?)] {
        model.connection.sendTextIdentities
    }

    func testEverySendCarriesAFullIdentity() async {
        let model = connectedModel { _, _ in .sent(matched: "foragents") }
        _ = await model.send(text: "deploy", to: "u-1")
        let sent = identities(model)
        XCTAssertEqual(sent.count, 1)
        XCTAssertNotNil(sent[0].requestID)
        XCTAssertEqual(
            sent[0].payloadHash,
            SendTextIdentity.payloadHash(session: "u-1", text: "deploy", submit: true),
            "the hash is the mutation the daemon will verify")
    }

    func testAnIndeterminateOutcomeKeepsTheIdentityForTheRetry() async {
        let model = connectedModel { _, _ in .indeterminate(reason: "supervisor vanished") }
        let first = await model.send(text: "deploy", to: "u-1")
        guard case .indeterminate = first else { return XCTFail("\(first)") }
        _ = await model.send(text: "deploy", to: "u-1")
        let sent = identities(model)
        XCTAssertEqual(sent.count, 2)
        XCTAssertEqual(
            sent[0].requestID, sent[1].requestID,
            "the retry must be recognisable as a retry")
    }

    func testASettledRetryReleasesTheIdentity() async {
        var outcomes: [SendTextResult] = [
            .indeterminate(reason: "unanswered"),
            .sent(matched: "foragents"),
            .sent(matched: "foragents"),
        ]
        let model = connectedModel { @MainActor _, _ in outcomes.removeFirst() }

        _ = await model.send(text: "deploy", to: "u-1")
        _ = await model.send(text: "deploy", to: "u-1")
        _ = await model.send(text: "deploy", to: "u-1")
        let sent = model.connection.sendTextIdentities
        XCTAssertEqual(sent[0].requestID, sent[1].requestID, "the retry reused the id")
        XCTAssertNotEqual(
            sent[1].requestID, sent[2].requestID,
            "after the retry settled, the same text is a new mutation")
    }

    func testDifferentTextAfterIndeterminateIsAFreshMutation() async {
        let model = connectedModel { _, _ in .indeterminate(reason: "unanswered") }
        _ = await model.send(text: "deploy", to: "u-1")
        _ = await model.send(text: "roll back", to: "u-1")
        let sent = identities(model)
        XCTAssertNotEqual(
            sent[0].requestID, sent[1].requestID,
            "only the same text is a retry; new text is a new mutation")
    }

    func testARefusalReleasesTheIdentity() async {
        let model = connectedModel { _, _ in .refused(reason: "composer not on screen") }
        _ = await model.send(text: "deploy", to: "u-1")
        _ = await model.send(text: "deploy", to: "u-1")
        let sent = identities(model)
        XCTAssertNotEqual(
            sent[0].requestID, sent[1].requestID,
            "a refusal promises nothing was typed; the next attempt is fresh")
    }

    /// The single-slot regression: an indeterminate mutation in one session
    /// must survive settled sends in another, or the promised retry
    /// recognition silently stops holding exactly when two sessions are busy.
    func testAnUnsettledMutationSurvivesOtherSessionsSettling() async {
        let model = connectedModel(listing: ["u-a", "u-b"]) { session, _ in
            session == "u-a" ? .indeterminate(reason: "unanswered") : .sent(matched: "foragents")
        }
        _ = await model.send(text: "deploy", to: "u-a")
        _ = await model.send(text: "unrelated", to: "u-b")
        _ = await model.send(text: "deploy", to: "u-a")
        let sent = model.connection.sendTextIdentities
        XCTAssertEqual(sent.count, 3)
        XCTAssertEqual(
            sent[0].requestID, sent[2].requestID,
            "session B settling must not evict session A's unsettled identity")
        XCTAssertNotEqual(sent[0].requestID, sent[1].requestID)
    }

    /// The side-door guard: every caller of `send` — including a denial
    /// reason — answers to the policy, and a blocked command never reaches
    /// the wire at all.
    func testABlockedCommandNeverTouchesTheWireFromAnyCaller() async {
        let model = connectedModel { _, _ in .sent(matched: "foragents") }
        let attempt = await model.send(text: "/config", to: "u-1")
        guard case .refused(let reason) = attempt else { return XCTFail("\(attempt)") }
        XCTAssertTrue(reason.contains("locks this composer"), reason)
        XCTAssertTrue(
            model.connection.sendTextIdentities.isEmpty,
            "a policy refusal is client-side; nothing may reach the wire")

        let modelAttempt = await model.send(text: "/model sonnet", to: "u-1")
        guard case .refused = modelAttempt else { return XCTFail("\(modelAttempt)") }
        XCTAssertTrue(model.connection.sendTextIdentities.isEmpty)
    }

    /// The counterpart: a session the daemon no longer lists retains
    /// nothing — a late indeterminate must not park a dead identity under a
    /// name the next run may reuse.
    func testADepartedSessionRetainsNoIdentity() async {
        let model = connectedModel(listing: []) { _, _ in
            .indeterminate(reason: "unanswered")
        }
        _ = await model.send(text: "deploy", to: "u-gone")
        _ = await model.send(text: "deploy", to: "u-gone")
        let sent = identities(model)
        XCTAssertEqual(sent.count, 2)
        XCTAssertNotEqual(
            sent[0].requestID, sent[1].requestID,
            "an unlisted session's identity dies with the attempt")
    }

    /// The tri-state handshake: no capabilities is a wait (no verdict may be
    /// recorded), capabilities without the flag is the honest "unsupported".
    func testCatalogFetchDistinguishesWaitingFromUnsupported() async {
        let model = AppModel(cache: EventCache())
        // Handshake in flight: no capabilities at all.
        await model.fetchCommandCatalog(for: "u-1")
        XCTAssertNil(model.commandCatalogs["u-1"])
        XCTAssertNil(
            model.commandCatalogFailures["u-1"],
            "a handshake in flight is a wait, not a verdict")

        // An older daemon answers — without the capability.
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            try! JSONDecoder().decode(
                Capabilities.self, from: Data(#"{"send_text":true}"#.utf8)),
            minor: 7)
        await model.fetchCommandCatalog(for: "u-1")
        XCTAssertNotNil(
            model.commandCatalogFailures["u-1"],
            "capabilities that lack the flag are the honest unsupported answer")
        XCTAssertTrue(
            model.commandCatalogFailures["u-1"]?.contains("predates") == true)
    }

    /// The one sanctioned bypass: the Model sheet's own injection.
    func testTheModelSheetPathBypassesThePolicy() async {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        var wired: [String] = []
        model.connection.sendTextStub = { @MainActor _, text in
            wired.append(text)
            return .sent(matched: "foragents")
        }
        let attempt = await model.sendModelCommand("sonnet", to: "u-1")
        guard case .sent = attempt else { return XCTFail("\(attempt)") }
        XCTAssertEqual(wired, ["/model sonnet"])
    }

    func testADuplicateReadsAsAlreadyApplied() async {
        let model = connectedModel { _, _ in
            .duplicate(matched: "foragents", appliedAt: "2026-08-04T20:00:00Z")
        }
        let attempt = await model.send(text: "deploy", to: "u-1")
        XCTAssertEqual(attempt, .alreadyApplied(appliedAt: "2026-08-04T20:00:00Z"))
    }
}
