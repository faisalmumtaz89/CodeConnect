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

    init(
        challenges: [RelayHTTPResponse] = [], enrolls: [RelayHTTPResponse] = [],
        statuses: [RelayHTTPResponse] = [], gate: (@Sendable () async -> Void)? = nil
    ) {
        challengeQueue = challenges
        enrollQueue = enrolls
        statusQueue = statuses
        self.gate = gate
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
        return statusQueue.isEmpty
            ? RelayHTTPResponse(status: 404, body: Data()) : statusQueue.removeFirst()
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
        var delivered: [String] = []
        model.onPushDeliveryAttempted = { delivered.append($0) }

        // Relay handshake; enrollment parks at the gate.
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ await transport.count(RelayEndpoint.enroll) >= 1 }, "relay enroll in flight")

        // The daemon becomes a direct-key daemon (new handshake, new generation).
        model.connection.injectForTesting(ack(push: true))
        await waitUntil({ delivered.contains(self.tokenA) }, "the direct registration")

        // Release the stale relay enrollment; it must not register a second time.
        await gate.release()
        try? await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(
            delivered.filter { $0 == tokenA }.count, 1,
            "only the direct send registered; the stale relay result was suppressed")
    }

    /// Foreground status checks are debounced (one GET, not many) and keyed to the
    /// token — a new token clears the debounce so its own credential is checked.
    func testForegroundRefreshIsDebouncedAndTokenKeyed() async {
        let store = StoreBox(
            RelayCredential(
                keyID: "K", token: tokenA, environment: "production", credential: "BEARER",
                generation: 1, installID: DeviceIdentity.installationID))
        let transport = LockedTransport(
            statuses: [
                statusResponse("active"), statusResponse("active"),
            ])
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: store))

        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")

        model.foregroundPushCheck()
        await waitUntil({ await transport.statusGets == 1 }, "first status check")
        model.foregroundPushCheck()
        try? await Task.sleep(nanoseconds: 150_000_000)
        let afterSecond = await transport.statusGets
        XCTAssertEqual(afterSecond, 1, "the second foreground is debounced")

        // A new token clears the debounce. Seed B so the check reuses it (a status
        // GET), rather than diverting to a rebind.
        store.save(
            RelayCredential(
                keyID: "K", token: tokenB, environment: "production", credential: "BEARER-B",
                generation: 1, installID: DeviceIdentity.installationID))
        model.simulatePushTokenForTesting(tokenB, environment: "production")
        model.foregroundPushCheck()
        await waitUntil({ await transport.statusGets == 2 }, "a new token is checked")
    }

    /// The complete environment-correction sequence: enroll production, a reconnect
    /// carrying a `sandbox` correction persists it, and a further reconnect that no
    /// longer carries a correction keeps the corrected value — the app never
    /// resends its stale one.
    func testEnvironmentCorrectionSurvivesReconnectAndSecondMac() async {
        let store = StoreBox(
            RelayCredential(
                keyID: "K", token: tokenA, environment: "production", credential: "BEARER",
                generation: 1, installID: DeviceIdentity.installationID))
        let transport = LockedTransport()
        let model = makeModel(
            RelayEnrollment(attest: alwaysSupported(), transport: transport, store: store))

        model.connection.simulateConnectedForTesting()
        // First reconnect (or a second Mac) reports the authoritative env as sandbox.
        model.connection.injectForTesting(ack(pushRelay: true, pushEnvironment: "sandbox"))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        await waitUntil({ store.load()?.environment == "sandbox" }, "correction persisted")

        // A later reconnect no longer carries a correction; the corrected value
        // must survive rather than being overwritten by the cached production one.
        model.connection.injectForTesting(ack(pushRelay: true))
        model.simulatePushTokenForTesting(tokenA, environment: "production")
        try? await Task.sleep(nanoseconds: 150_000_000)
        XCTAssertEqual(store.load()?.environment, "sandbox", "the corrected env survived reconnect")
        XCTAssertEqual(store.load()?.credential, "BEARER")
        let posts = await transport.posts
        XCTAssertTrue(posts.isEmpty, "no re-enrollment was needed")
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
