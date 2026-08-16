import XCTest

@testable import CodeConnect

// =============================================================================
//  AppModel orchestration of the relay path, driven through the real handshake
//  seam: direct mode never contacts the relay, a static connection never
//  enrolls, relay mode enrolls and registers, and a credential that arrives for
//  a token that has since changed is not registered over the current one.
// =============================================================================

/// A scriptable transport as an `actor` — its state is serialized by isolation,
/// and awaiting the enroll gate suspends it reentrantly, which is exactly the
/// window a second token needs to start its own enrollment.
private actor LockedTransport: RelayTransport {
    private var challengeQueue: [RelayHTTPResponse]
    private var enrollQueue: [RelayHTTPResponse]
    private var statusQueue: [RelayHTTPResponse]
    private var recordedPosts: [String] = []
    private var recordedGets: [String] = []
    private let gate: (@Sendable () async -> Void)?
    private let statusGate: (@Sendable () async -> Void)?
    /// Thrown by `get` after its gate — the status transport-exception path.
    private let statusError: Error?

    init(
        challenges: [RelayHTTPResponse] = [], enrolls: [RelayHTTPResponse] = [],
        statuses: [RelayHTTPResponse] = [], gate: (@Sendable () async -> Void)? = nil,
        statusGate: (@Sendable () async -> Void)? = nil, statusError: Error? = nil
    ) {
        challengeQueue = challenges
        enrollQueue = enrolls
        statusQueue = statuses
        self.gate = gate
        self.statusGate = statusGate
        self.statusError = statusError
    }

    func post(path: String, json: Data) async throws -> RelayHTTPResponse {
        recordedPosts.append(path)
        switch path {
        case RelayEndpoint.challenge:
            return challengeQueue.isEmpty
                ? RelayHTTPResponse(status: 404, body: Data()) : challengeQueue.removeFirst()
        case RelayEndpoint.enroll:
            // Bind the response at post time, before the gate parks the call —
            // otherwise two parked enrolls receive their credentials in resume
            // order, not caller order, and the surviving request's credential
            // becomes nondeterministic.
            let response =
                enrollQueue.isEmpty
                ? RelayHTTPResponse(status: 404, body: Data()) : enrollQueue.removeFirst()
            if let gate { await gate() }
            return response
        default:
            return RelayHTTPResponse(status: 404, body: Data())
        }
    }
    func get(path: String, bearer: String) async throws -> RelayHTTPResponse {
        recordedGets.append(path)
        let response =
            statusQueue.isEmpty
            ? RelayHTTPResponse(status: 404, body: Data()) : statusQueue.removeFirst()
        if let statusGate { await statusGate() }
        if let statusError { throw statusError }
        return response
    }

    var posts: [String] { recordedPosts }
    var statusGets: Int { recordedGets.filter { $0 == RelayEndpoint.status }.count }
    func count(_ path: String) -> Int { recordedPosts.filter { $0 == path }.count }
}

private final class StoreBox: RelayCredentialStoring, @unchecked Sendable {
    private let lock = NSLock()
    private var value: RelayCredential?
    init(_ seed: RelayCredential? = nil) { value = seed }
    func load() -> RelayCredential? {
        lock.lock(); defer { lock.unlock() }
        return value
    }
    func save(_ credential: RelayCredential) {
        lock.lock(); value = credential; lock.unlock()
    }
    func clear() {
        lock.lock(); value = nil; lock.unlock()
    }
}

private actor GateActor {
    private var open = false
    func release() { open = true }
    func wait() async {
        while !open { await Task.yield() }
    }
}

/// Records every registration's intent — synchronously, the moment
/// `sendRegistration` is called — and every registration Task's completion, so a
/// test can wait for ALL registration work to settle (`quiescent` is true only
/// when nothing is still in the air) and then inspect exactly what was sent. This
/// is the barrier `onRelayOutcomeApplied` is not: that fires while a registration
/// the outcome just scheduled is still pending.
@MainActor
private final class RegistrationLog {
    private(set) var scheduled: [(token: String, environment: String, credential: String?)] = []
    private(set) var settled = 0
    func attach(to model: AppModel) {
        model.onRegisterPushScheduledForTesting = { [self] in scheduled.append(($0, $1, $2)) }
        model.onRegisterPushSettledForTesting = { [self] in settled += 1 }
    }
    /// No registration is still in flight: every scheduled one has settled.
    var quiescent: Bool { scheduled.count == settled }
}

private func challengeResponse() -> RelayHTTPResponse {
    let body = try! JSONSerialization.data(withJSONObject: [
        "challenge": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        "expires_in_seconds": 600,
    ])
    return RelayHTTPResponse(status: 200, body: body)
}
private func credentialResponse(_ bearer: String) -> RelayHTTPResponse {
    let body = try! JSONSerialization.data(withJSONObject: [
        "credential": bearer, "environment": "production", "generation": 1,
    ])
    return RelayHTTPResponse(status: 200, body: body)
}
private func statusResponse(_ status: String) -> RelayHTTPResponse {
    let body = try! JSONSerialization.data(withJSONObject: [
        "status": status, "environment": "production",
    ])
    return RelayHTTPResponse(status: 200, body: body)
}

@MainActor
final class RelayOrchestrationTests: XCTestCase {
    private func makeModel(_ enrollment: RelayEnrollment) -> AppModel {
        AppModel(
            pairing: PairingStore(),
            cache: EventCache(
                root: URL(fileURLWithPath: NSTemporaryDirectory())
                    .appendingPathComponent(UUID().uuidString)),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!),
            relayEnrollment: enrollment)
    }

    private func ack(
        push: Bool = false, pushRelay: Bool = false, deviceID: String? = "device-1",
        pushEnvironment: String? = nil
    ) -> ServerMessage {
        .helloAck(
            HelloAck(
                protocolVersion: 1, protocolMinor: 14, serverTime: "",
                capabilities: Capabilities(push: push, pushRelay: pushRelay),
                deviceToken: nil, deviceID: deviceID, deviceName: nil,
                pushEnvironment: pushEnvironment))
    }

    private func waitUntil(
        _ condition: @escaping () async -> Bool, _ message: String, file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<400 {
            if await condition() { return }
            try? await Task.sleep(nanoseconds: 5_000_000)
        }
        XCTFail("timed out waiting for \(message)", file: file, line: line)
    }

    private let tokenA = String(repeating: "a", count: 64)
    private let tokenB = String(repeating: "b", count: 64)

    /// Direct mode registers the token and never once contacts the relay.
    func testDirectModeSendsNoRelayRequest() async {
        let transport = LockedTransport()
        let model = makeModel(RelayEnrollment(attest: alwaysSupported(), transport: transport, store: StoreBox()))
        var delivered: [String] = []
        model.onPushDeliveryAttempted = { delivered.append($0) }

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(push: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")

        await waitUntil({ delivered.contains(self.tokenA) }, "the direct registration")
        let posts = await transport.posts
        XCTAssertTrue(posts.isEmpty, "direct mode never reaches the relay")
    }

    /// A bootstrap/static connection has no device row: no prompt, no enrollment.
    func testStaticConnectionNeverEnrolls() async {
        let transport = LockedTransport(challenges: [challengeResponse()], enrolls: [credentialResponse("X")])
        let model = makeModel(RelayEnrollment(attest: alwaysSupported(), transport: transport, store: StoreBox()))
        var delivered: [String] = []
        model.onPushDeliveryAttempted = { delivered.append($0) }

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true, deviceID: nil))
        model.simulatePushTokenForTesting(tokenA, environment: "production")

        // Give the flow a chance to (not) run.
        try? await Task.sleep(nanoseconds: 200_000_000)
        let posts = await transport.posts
        XCTAssertTrue(posts.isEmpty, "static: no challenge, no enrollment")
        XCTAssertFalse(delivered.contains(tokenA), "static: no registration attempt")
    }

    /// Relay mode enrolls, then registers the token it enrolled.
    func testRelayModeEnrollsThenRegisters() async {
        let transport = LockedTransport(
            challenges: [challengeResponse()], enrolls: [credentialResponse("BEARER")])
        let model = makeModel(RelayEnrollment(attest: alwaysSupported(), transport: transport, store: StoreBox()))
        var delivered: [String] = []
        model.onPushDeliveryAttempted = { delivered.append($0) }

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")

        await waitUntil({ delivered.contains(self.tokenA) }, "the relay registration")
        let enrollCount = await transport.count(RelayEndpoint.enroll)
        XCTAssertEqual(enrollCount, 1)
        XCTAssertEqual(model.relayPushState, .ready)
    }

    /// A late credential for a token that has since changed is not registered
    /// over the current token — the token-tuple recheck, distinct from the
    /// connection generation.
    func testLateResultForOldTokenIsNotRegistered() async {
        let gate = GateActor()
        let transport = LockedTransport(
            challenges: [challengeResponse(), challengeResponse()],
            enrolls: [credentialResponse("FOR-A"), credentialResponse("FOR-B")],
            gate: { await gate.wait() })
        let model = makeModel(RelayEnrollment(attest: alwaysSupported(), transport: transport, store: StoreBox()))
        var delivered: [String] = []
        model.onPushDeliveryAttempted = { delivered.append($0) }

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))

        // Token A begins enrolling and blocks at the gate.
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ await transport.count(RelayEndpoint.enroll) >= 1 }, "token A reaches the gate")

        // The APNs token rotates to B before A's credential comes back.
        model.simulatePushTokenForTesting(tokenB, environment: "production")
        await waitUntil({ await transport.count(RelayEndpoint.enroll) >= 2 }, "token B reaches the gate")

        await gate.release()
        await waitUntil({ delivered.contains(self.tokenB) }, "token B registers")
        XCTAssertFalse(
            delivered.contains(tokenA),
            "A's credential arrived after A was superseded and must not register")
    }

    /// A handshake whose `push_environment` disagrees with the cached tuple
    /// persists the corrected value rather than resending the stale one — the
    /// daemon→app propagation path that a second Mac's correction rides. The
    /// registration then carries the corrected environment.
    func testEnvironmentCorrectionIsPersistedNotResent() async {
        // A binding already held for token A, enrolled as "production".
        let store = StoreBox(
            RelayCredential(
                keyID: "K", token: tokenA, environment: "production", credential: "BEARER",
                generation: 1, installID: DeviceIdentity.installationID))
        let transport = LockedTransport()
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: store))

        model.connection.simulateConnectedForTesting()
        // The daemon now reports the authoritative environment as "sandbox".
        model.connection.injectForTesting(ack(pushRelay: true, pushEnvironment: "sandbox"))
        model.simulatePushTokenForTesting(tokenA, environment: "production")

        await waitUntil(
            { store.load()?.environment == "sandbox" },
            "the corrected environment is persisted")
        XCTAssertEqual(store.load()?.credential, "BEARER", "same credential, corrected environment")
        let posts = await transport.posts
        XCTAssertTrue(posts.isEmpty, "a correction is not a re-enrollment")
    }

    /// A direct handshake, then a relay handshake: the relay path enrolls from the
    /// APNs token already cached, without waiting for a new one.
    func testDirectToRelayEnrollsFromCachedToken() async {
        let transport = LockedTransport(
            challenges: [challengeResponse()], enrolls: [credentialResponse("BEARER")])
        let model = makeModel(RelayEnrollment(attest: alwaysSupported(), transport: transport, store: StoreBox()))
        var delivered: [String] = []
        model.onPushDeliveryAttempted = { delivered.append($0) }

        // Direct first: token cached, no relay contact.
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(push: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ delivered.contains(self.tokenA) }, "direct registration")
        let posts = await transport.posts
        XCTAssertTrue(posts.isEmpty)

        // Now a relay daemon: enrollment starts from the cached token, no new
        // APNs token injected.
        model.connection.injectForTesting(ack(pushRelay: true))
        await waitUntil(
            { await transport.count(RelayEndpoint.enroll) >= 1 },
            "enrollment from the cached token")
    }

    /// A relay→direct switch while enrollment is in flight: the connection sends
    /// the token directly, and the late relay result is suppressed — the token is
    /// registered exactly once (the direct send), never a second time by the
    /// stale relay credential.
    func testRelayToDirectDuringEnrollmentSendsDirectSuppressesRelay() async {
        let gate = GateActor()
        let transport = LockedTransport(
            challenges: [challengeResponse()], enrolls: [credentialResponse("BEARER")],
            gate: { await gate.wait() })
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: StoreBox()))
        let reg = RegistrationLog()
        reg.attach(to: model)
        var relayApplied = false
        model.onRelayOutcomeApplied = { relayApplied = true }

        // Relay handshake; enrollment parks at the gate.
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ await transport.count(RelayEndpoint.enroll) >= 1 }, "relay enroll in flight")

        // The daemon becomes a direct-key daemon (new handshake, new generation).
        model.connection.injectForTesting(ack(push: true))
        await waitUntil({ reg.scheduled.contains { $0.token == self.tokenA } }, "the direct registration")

        // Release the stale relay enrollment. Barrier on its outcome being applied
        // AND on ALL registration work settling — so a registration the (buggy)
        // relay path might have scheduled would have run and been recorded, rather
        // than slipping past an assertion that fired while it was still in the air.
        await gate.release()
        await waitUntil(
            { relayApplied && reg.quiescent }, "the stale relay outcome and all registrations settled")

        let forA = reg.scheduled.filter { $0.token == tokenA }
        XCTAssertEqual(forA.count, 1, "the token registered exactly once")
        XCTAssertNil(forA.first?.credential, "the one registration was the direct, credential-less send")
        XCTAssertFalse(
            reg.scheduled.contains { $0.token == tokenA && $0.credential != nil },
            "the stale relay credential was never registered")
    }

    /// The in-flight latch, isolated from the daily debounce, plus the debounce and
    /// token-keying. The latch is the hard one: the production code sets the 24-hour
    /// deadline *before* awaiting, so a naive overlap test proves nothing — the
    /// deadline alone would block the second GET. Here the debounce is deliberately
    /// CLEARED (a new token) while check #1 is in flight, so the *only* thing that
    /// can stop the overlapping check is the in-flight latch.
    func testForegroundRefreshInFlightLatchAndDebounce() async {
        let store = StoreBox(
            RelayCredential(
                keyID: "K", token: tokenA, environment: "production", credential: "BEARER",
                generation: 1, installID: DeviceIdentity.installationID))
        let statusGate = GateActor()
        let transport = LockedTransport(
            statuses: [statusResponse("active"), statusResponse("active"), statusResponse("active")],
            statusGate: { await statusGate.wait() })
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: store))
        var refreshes = 0
        model.onRelayRefreshComplete = { refreshes += 1 }

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")

        // Check #1 parks at its status GET, claiming the in-flight latch.
        model.foregroundPushCheck()
        await waitUntil({ await transport.statusGets == 1 }, "first status GET parked")

        // Clear the DAILY debounce without releasing the latch: a new token resets
        // nextRelayRefreshAllowed to distantPast. Token B's binding is already
        // stored, so its enrollment path returns `.ready` with no status GET.
        store.save(
            RelayCredential(
                keyID: "K", token: tokenB, environment: "production", credential: "BEARER-B",
                generation: 1, installID: DeviceIdentity.installationID))
        model.simulatePushTokenForTesting(tokenB, environment: "production")

        // A second foreground now — the debounce is cleared, so ONLY the in-flight
        // latch can stop it. It must not start a second GET.
        model.foregroundPushCheck()
        let duringLatch = await transport.statusGets
        XCTAssertEqual(duringLatch, 1, "the in-flight latch blocks the overlap even with the debounce cleared")

        // Release #1 and barrier on its completion.
        await statusGate.release()
        await waitUntil({ refreshes == 1 }, "first refresh completed")

        // Latch released and token-B debounce still clear: the next foreground runs
        // token B's own check.
        model.foregroundPushCheck()
        await waitUntil({ await transport.statusGets == 2 }, "token B gets its own check")
        await waitUntil({ refreshes == 2 }, "token B's refresh completed")

        // Daily debounce: a further check for the SAME token B is skipped.
        model.foregroundPushCheck()
        let afterDebounce = await transport.statusGets
        XCTAssertEqual(afterDebounce, 2, "daily debounce: no third GET within the window")
    }

    /// The complete environment-correction sequence, observed at the wire: a
    /// reconnect (a second Mac) reports `sandbox`; the app persists it and
    /// registers the corrected environment, and a later reconnect with no
    /// correction still sends `sandbox` — the cached `production` is never resent.
    func testEnvironmentCorrectionSurvivesReconnectAndSecondMac() async {
        let store = StoreBox(
            RelayCredential(
                keyID: "K", token: tokenA, environment: "production", credential: "BEARER",
                generation: 1, installID: DeviceIdentity.installationID))
        let transport = LockedTransport()
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: store))
        let reg = RegistrationLog()
        reg.attach(to: model)

        model.connection.simulateConnectedForTesting()
        // A second Mac's push made the relay correct the environment to sandbox;
        // the daemon reports that authoritative value on this handshake — the
        // documented daemon→app propagation path, the only form in which a second
        // Mac's correction reaches iOS.
        model.connection.injectForTesting(ack(pushRelay: true, pushEnvironment: "sandbox"))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ reg.quiescent && reg.settled >= 1 }, "the first registration settled")
        XCTAssertEqual(
            reg.scheduled.last?.environment, "sandbox",
            "registered the corrected environment, not the cached production")
        XCTAssertEqual(store.load()?.environment, "sandbox")

        // A later reconnect carries no correction. The app must keep sending the
        // corrected value, never resend its cached production one.
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        // Barrier on ALL registration work settling, so no later registration
        // resending the cached production value could still be in the air.
        await waitUntil({ reg.quiescent && reg.settled >= 2 }, "all registration work settled")
        XCTAssertEqual(reg.scheduled.last?.environment, "sandbox", "still sends the corrected env")
        XCTAssertFalse(
            reg.scheduled.contains { $0.environment == "production" },
            "the cached production environment was never resent, in any registration")
        let posts = await transport.posts
        XCTAssertTrue(posts.isEmpty, "no re-enrollment was needed")
    }

    /// DEF-1(b) at the orchestration boundary: a foreground status check that is
    /// superseded by a same-token reset and whose transport then THROWS must be
    /// inert. Because the token (and generation and mode) are unchanged across the
    /// reset, `applyRelayOutcome`'s own guards accept the result — only the actor's
    /// `.superseded` verdict keeps the thrown status from clobbering the fresh
    /// `.ready` the reset's re-enrollment set.
    func testSupersededStatusThrowDoesNotClobberReady() async {
        let store = StoreBox(
            RelayCredential(
                keyID: "K", token: tokenA, environment: "production", credential: "BEARER",
                generation: 1, installID: DeviceIdentity.installationID))
        let statusGate = GateActor()
        let transport = LockedTransport(
            challenges: [challengeResponse()], enrolls: [credentialResponse("FRESH")],
            statuses: [statusResponse("active")],
            statusGate: { await statusGate.wait() }, statusError: URLError(.timedOut))
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: store))
        var refreshDone = false
        model.onRelayRefreshComplete = { refreshDone = true }

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ model.relayPushState == .ready }, "the stored binding is ready")

        // A foreground status check parks at its GET (which is primed to throw).
        model.foregroundPushCheck()
        await waitUntil({ await transport.statusGets == 1 }, "the status GET parked")

        // A same-token reset supersedes it and re-enrolls fresh → ready again.
        model.resetNotificationRegistration()
        await waitUntil(
            { store.load()?.credential == "FRESH" && model.relayPushState == .ready },
            "the reset re-enrolled and is ready")

        // Release the parked GET; it throws. The superseded refresh must be inert.
        await statusGate.release()
        await waitUntil({ refreshDone }, "the superseded refresh settled")
        XCTAssertEqual(
            model.relayPushState, .ready,
            "a superseded status throw did not clobber the fresh ready state")
    }
}

/// A fake attester that always succeeds — orchestration tests care about routing,
/// not attestation bytes (those are proven in `RelayClientDataTests`).
private func alwaysSupported() -> AppAttesting { StubAttest() }
private final class StubAttest: AppAttesting, @unchecked Sendable {
    var isSupported: Bool { true }
    func generateKey() async throws -> String { "KEY" }
    func attestKey(_ keyID: String, clientDataHash: Data) async throws -> Data {
        Data("a".utf8)
    }
    func generateAssertion(_ keyID: String, clientDataHash: Data) async throws -> Data {
        Data("s".utf8)
    }
}
