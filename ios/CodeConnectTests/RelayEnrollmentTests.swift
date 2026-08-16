import CryptoKit
import XCTest

@testable import CodeConnect

// =============================================================================
//  The relay enrollment actor and the two client-data hashes it must match.
//
//  The match proof is the point: on real hardware a wrong byte in either hash is
//  a silent enrollment failure, so the assertion hash is checked against a vector
//  computed outside Swift (see the golden constants — reproduced by hashlib in
//  the same byte order the Rust verifier uses), and the attestation hash is
//  checked to be the raw challenge bytes, unhashed.
// =============================================================================

// MARK: - Doubles

private struct FakeAttestError: Error {}

private final class FakeAttest: AppAttesting, @unchecked Sendable {
    var supported = true
    var keyID = "FAKE-KEY-ID"
    /// Number of `attestKey` calls that throw (an availability failure) before
    /// succeeding — for the key-reuse-on-retry case.
    var attestFailuresRemaining = 0
    private(set) var generatedKeys = 0
    private(set) var attestKeyIDs: [String] = []
    private(set) var attestHashes: [Data] = []
    private(set) var assertHashes: [Data] = []

    var isSupported: Bool { supported }
    func generateKey() async throws -> String {
        generatedKeys += 1
        return generatedKeys == 1 ? keyID : "\(keyID)-\(generatedKeys)"
    }
    func attestKey(_ keyID: String, clientDataHash: Data) async throws -> Data {
        if attestFailuresRemaining > 0 {
            attestFailuresRemaining -= 1
            throw FakeAttestError()
        }
        attestKeyIDs.append(keyID)
        attestHashes.append(clientDataHash)
        return Data("attestation-object".utf8)
    }
    func generateAssertion(_ keyID: String, clientDataHash: Data) async throws -> Data {
        assertHashes.append(clientDataHash)
        return Data("assertion-object".utf8)
    }
}

private final class FakeTransport: RelayTransport, @unchecked Sendable {
    var challengeQueue: [RelayHTTPResponse] = []
    var enrollQueue: [RelayHTTPResponse] = []
    var assertQueue: [RelayHTTPResponse] = []
    var statusQueue: [RelayHTTPResponse] = []
    private(set) var posts: [(path: String, body: Data)] = []
    private(set) var gets: [(path: String, bearer: String)] = []
    /// Opened by the test to release a blocked enroll/assert/status — for
    /// parked-flight races. The response is captured before the gate, so which
    /// credential a parked request receives never depends on resume order.
    var enrollGate: (() async -> Void)?
    var assertGate: (() async -> Void)?
    var statusGate: (() async -> Void)?

    func post(path: String, json: Data) async throws -> RelayHTTPResponse {
        posts.append((path, json))
        switch path {
        case RelayEndpoint.challenge:
            return challengeQueue.isEmpty ? Self.notFound : challengeQueue.removeFirst()
        case RelayEndpoint.enroll:
            // Bind the response to the request at *post* time, not on resume.
            // The gate parks the call, and dequeuing after it would hand the
            // response to whichever parked task the runtime resumes first — so
            // two parked enrolls could swap credentials. Post order is the caller
            // order the test controls; that is what the response must follow.
            let response = enrollQueue.isEmpty ? Self.notFound : enrollQueue.removeFirst()
            if let gate = enrollGate { await gate() }
            return response
        case RelayEndpoint.assert:
            let response = assertQueue.isEmpty ? Self.notFound : assertQueue.removeFirst()
            if let gate = assertGate { await gate() }
            return response
        default:
            return Self.notFound
        }
    }
    func get(path: String, bearer: String) async throws -> RelayHTTPResponse {
        gets.append((path, bearer))
        let response = statusQueue.isEmpty ? Self.notFound : statusQueue.removeFirst()
        if let gate = statusGate { await gate() }
        return response
    }

    var statusGets: Int { gets.filter { $0.path == RelayEndpoint.status }.count }

    var challengePosts: Int { posts.filter { $0.path == RelayEndpoint.challenge }.count }
    var enrollPosts: Int { posts.filter { $0.path == RelayEndpoint.enroll }.count }
    var assertPosts: Int { posts.filter { $0.path == RelayEndpoint.assert }.count }

    static let notFound = RelayHTTPResponse(status: 404, body: Data())
}

private final class InMemoryStore: RelayCredentialStoring, @unchecked Sendable {
    private var value: RelayCredential?
    init(_ seed: RelayCredential? = nil) { value = seed }
    func load() -> RelayCredential? { value }
    func save(_ credential: RelayCredential) { value = credential }
    func clear() { value = nil }
}

/// Releases blocked enrollments on the test's command, so overlap and late
/// results can be arranged deterministically.
private actor Gate {
    private var open = false
    func release() { open = true }
    func wait() async { while !open { await Task.yield() } }
}

/// Gates only the *first* caller and lets every later one through — so a stale
/// operation can be parked while its replacement runs to completion, giving a
/// deterministic reproduction of "the late result overwrites the new one".
private actor OneShotGate {
    private var seen = 0
    private var open = false
    func release() { open = true }
    func wait() async {
        seen += 1
        if seen > 1 { return }
        while !open { await Task.yield() }
    }
}

/// Spin until a condition holds — the transport is mutated on the actor's
/// executor, so a test waits for the parked state rather than guessing at it.
private func spin(
    _ what: String, file: StaticString = #filePath, line: UInt = #line,
    _ condition: @escaping () -> Bool
) async {
    for _ in 0..<400 {
        if condition() { return }
        try? await Task.sleep(nanoseconds: 5_000_000)
    }
    XCTFail("timed out waiting for \(what)", file: file, line: line)
}

/// Lifecycle-aware handling of the relay's typed refusals, and Apple's
/// retry-the-same-key guidance.
final class RelayLifecycleTests: XCTestCase {
    /// A dead APNs token surfaced by enroll becomes `.tokenInvalid`, not a
    /// generic failure — there is no bearer to repair via status.
    func testEnrollTokenUnregisteredBecomesTokenInvalid() async {
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [jsonResponse(["error": "token_unregistered"], status: 409)]
        let store = InMemoryStore()
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        XCTAssertEqual(outcome, .tokenInvalid)
        XCTAssertNil(store.load())
    }

    /// A rebind the relay answers with `binding_unknown` (a raised floor, a
    /// restore) re-enrolls with a fresh key rather than repeating forever.
    func testBindingUnknownReenrollsWithFreshKey() async {
        let store = InMemoryStore(seeded(keyID: "OLD-KEY", token: "oldtoken", credential: "OLD"))
        let attest = FakeAttest()
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.assertQueue = [jsonResponse(["error": "binding_unknown"], status: 403)]
        transport.enrollQueue = [credentialResponse(credential: "REBOUND-FRESH")]
        let enrollment = RelayEnrollment(
            attest: attest, transport: transport, store: store, installID: installID)

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.credential, "REBOUND-FRESH")
        XCTAssertEqual(credential.token, sampleToken)
        XCTAssertEqual(attest.generatedKeys, 1, "binding_unknown drove one fresh attestation")
    }

    /// An `attestKey` availability failure is retried with the *same* key id, not
    /// a newly generated one.
    func testKeyIDReusedAcrossAttestFailure() async {
        let attest = FakeAttest()
        attest.attestFailuresRemaining = 1
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "OK")]
        let store = InMemoryStore()
        let enrollment = RelayEnrollment(
            attest: attest, transport: transport, store: store, installID: installID)

        let first = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .failed = first else { return XCTFail("first attempt should fail: \(first)") }
        XCTAssertEqual(attest.generatedKeys, 1)

        let second = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready = second else { return XCTFail("second attempt should succeed: \(second)") }
        XCTAssertEqual(attest.generatedKeys, 1, "the pending key was reused, not regenerated")
        XCTAssertEqual(attest.attestKeyIDs, ["FAKE-KEY-ID"], "attested with the reused key id")
    }
}

/// Reentrancy: an actor await lets a newer operation become current, so every
/// post-await mutation must re-check identity, not just the token. Each test here
/// parks a stale operation, lets a newer one win, then releases the stale one and
/// proves it changed nothing.
final class RelayReentrancyTests: XCTestCase {
    private let tokenB = String(repeating: "b", count: 64)

    /// A stale `active`-status refresh for token A, resumed after token B became
    /// current, must not overwrite B's binding.
    func testLateActiveRefreshDoesNotOverwriteNewerToken() async {
        let store = InMemoryStore(seeded(credential: "A-CRED"))
        let transport = FakeTransport()
        let gate = Gate()
        transport.statusGate = { await gate.wait() }
        transport.statusQueue = [jsonResponse(["status": "active", "environment": "sandbox"])]
        transport.challengeQueue = [challengeResponse()]
        transport.assertQueue = [credentialResponse(credential: "B-CRED")]
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        async let refreshA = enrollment.refresh(token: sampleToken, environment: "production")
        await spin("A parked at status") { transport.statusGets == 1 }

        let outcomeB = await enrollment.credential(token: tokenB, environment: "production")
        guard case .ready = outcomeB else { return XCTFail("B: \(outcomeB)") }
        XCTAssertEqual(store.load()?.token, tokenB)

        await gate.release()
        _ = await refreshA
        XCTAssertEqual(store.load()?.token, tokenB, "the late active refresh must not overwrite B")
        XCTAssertEqual(store.load()?.credential, "B-CRED")
    }

    /// A stale `reenroll`-status refresh for A, resumed after B is current, must
    /// not clear B's binding.
    func testLateReenrollRefreshDoesNotClearNewerToken() async {
        let store = InMemoryStore(seeded(credential: "A-CRED"))
        let transport = FakeTransport()
        let gate = Gate()
        transport.statusGate = { await gate.wait() }
        transport.statusQueue = [jsonResponse(["status": "reenroll"])]
        transport.challengeQueue = [challengeResponse()]
        transport.assertQueue = [credentialResponse(credential: "B-CRED")]
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        async let refreshA = enrollment.refresh(token: sampleToken, environment: "production")
        await spin("A parked at status") { transport.statusGets == 1 }
        _ = await enrollment.credential(token: tokenB, environment: "production")
        XCTAssertEqual(store.load()?.token, tokenB)

        await gate.release()
        _ = await refreshA
        XCTAssertEqual(store.load()?.token, tokenB, "the late reenroll must not clear B")
    }

    /// A stale `token_invalid`-status refresh for A, resumed after B is current,
    /// must not clear B's binding.
    func testLateTokenInvalidRefreshDoesNotClearNewerToken() async {
        let store = InMemoryStore(seeded(credential: "A-CRED"))
        let transport = FakeTransport()
        let gate = Gate()
        transport.statusGate = { await gate.wait() }
        transport.statusQueue = [jsonResponse(["status": "token_invalid"])]
        transport.challengeQueue = [challengeResponse()]
        transport.assertQueue = [credentialResponse(credential: "B-CRED")]
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        async let refreshA = enrollment.refresh(token: sampleToken, environment: "production")
        await spin("A parked at status") { transport.statusGets == 1 }
        _ = await enrollment.credential(token: tokenB, environment: "production")
        XCTAssertEqual(store.load()?.token, tokenB)

        await gate.release()
        _ = await refreshA
        XCTAssertEqual(store.load()?.token, tokenB, "the late token_invalid must not clear B")
    }

    /// Reset then re-drive the same token: a pre-reset enrollment that lands after
    /// the fresh one must not resurrect the revoked bearer.
    func testResetThenRedriveDoesNotResurrectRevokedBearer() async {
        let store = InMemoryStore()
        let transport = FakeTransport()
        let gate = OneShotGate()
        transport.enrollGate = { await gate.wait() }
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.enrollQueue = [
            credentialResponse(credential: "STALE"), credentialResponse(credential: "FRESH"),
        ]
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        // The pre-reset enrollment parks at the gate holding "STALE".
        async let stale = enrollment.credential(token: sampleToken, environment: "production")
        await spin("stale parked") { transport.enrollPosts == 1 }

        // Reset revokes it; the re-drive enrolls "FRESH" (not gated — OneShotGate
        // only holds the first) and stores it.
        await enrollment.reset()
        let fresh = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready(let freshCred) = fresh else { return XCTFail("fresh: \(fresh)") }
        XCTAssertEqual(freshCred.credential, "FRESH")

        // Release the pre-reset enrollment; it must not overwrite FRESH.
        await gate.release()
        let staleOutcome = await stale
        XCTAssertEqual(staleOutcome, .superseded)
        XCTAssertEqual(store.load()?.credential, "FRESH", "the revoked bearer was not resurrected")
    }
}

// MARK: - Fixtures

/// 32 raw challenge bytes 0x00…0x1f and their base64url string, the same pair the
/// golden vectors were computed from.
private let challengeBytes = Data(0..<32)
private let challengeB64URL = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
private let sampleToken = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
/// The install every seeded credential and enrollment shares, so a reuse check
/// passes unless a test deliberately mismatches it (the reinstall case).
private let installID = "INSTALL-1"

private func seeded(
    keyID: String = "K", token: String = sampleToken, environment: String = "production",
    credential: String = "C", generation: Int = 1, install: String = installID
) -> RelayCredential {
    RelayCredential(
        keyID: keyID, token: token, environment: environment, credential: credential,
        generation: generation, installID: install)
}

private func challengeResponse() -> RelayHTTPResponse {
    jsonResponse(["challenge": challengeB64URL, "expires_in_seconds": 600])
}
private func credentialResponse(
    credential: String = "BEARER-abc", environment: String = "production", generation: Int = 1
) -> RelayHTTPResponse {
    jsonResponse([
        "credential": credential, "environment": environment, "generation": generation,
    ])
}
private func jsonResponse(_ object: [String: Any], status: Int = 200) -> RelayHTTPResponse {
    let body = try! JSONSerialization.data(withJSONObject: object)
    return RelayHTTPResponse(status: status, body: body)
}

private func makeEnrollment(
    attest: FakeAttest = FakeAttest(), transport: FakeTransport = FakeTransport(),
    store: InMemoryStore = InMemoryStore(), install: String = installID
) -> RelayEnrollment {
    RelayEnrollment(attest: attest, transport: transport, store: store, installID: install)
}

// MARK: - Match proof

final class RelayClientDataTests: XCTestCase {
    /// The attestation client-data hash is the raw challenge bytes, never a hash
    /// of them — the relay recomputes the nonce from these exact bytes.
    func testAttestationHashIsRawChallengeBytes() {
        XCTAssertEqual(RelayClientData.attestation(challengeBytes: challengeBytes), challengeBytes)
        XCTAssertNotEqual(
            RelayClientData.attestation(challengeBytes: challengeBytes),
            Data(SHA256.hash(data: challengeBytes)),
            "hashing the challenge is exactly the mistake the relay rejects")
    }

    /// The assertion client-data hash equals the value the relay's
    /// `assertion_client_data` produces, computed here by an independent tool.
    func testAssertionHashMatchesRelayGolden() {
        let rebind = RelayClientData.assertion(
            operation: "rebind", challenge: challengeB64URL, challengeBytes: challengeBytes,
            token: sampleToken, environment: "production")
        XCTAssertEqual(
            rebind.map { String(format: "%02x", $0) }.joined(),
            "d593b10153e20d20a85ce5c5c8dbe847770106831e2f18052797ce460e02ca66")

        let rotate = RelayClientData.assertion(
            operation: "rotate", challenge: challengeB64URL, challengeBytes: challengeBytes,
            token: sampleToken, environment: "sandbox")
        XCTAssertEqual(
            rotate.map { String(format: "%02x", $0) }.joined(),
            "74d6ecbe2dfdeceb06c000af4cc89eb23ed78317df17fb4dda080680a3d3cf0c")
    }

    /// A different operation, token, or environment must change the hash — the
    /// domain separation is load-bearing, not decoration.
    func testAssertionHashIsSensitiveToEveryField() {
        let base = RelayClientData.assertion(
            operation: "rebind", challenge: challengeB64URL, challengeBytes: challengeBytes,
            token: sampleToken, environment: "production")
        let otherOp = RelayClientData.assertion(
            operation: "rotate", challenge: challengeB64URL, challengeBytes: challengeBytes,
            token: sampleToken, environment: "production")
        let otherEnv = RelayClientData.assertion(
            operation: "rebind", challenge: challengeB64URL, challengeBytes: challengeBytes,
            token: sampleToken, environment: "sandbox")
        XCTAssertNotEqual(base, otherOp)
        XCTAssertNotEqual(base, otherEnv)
    }

    func testBase64URLDecodesTheChallenge() {
        XCTAssertEqual(Base64URL.decode(challengeB64URL), challengeBytes)
        XCTAssertNil(Base64URL.decode("A"), "a one-char remainder cannot be base64")
    }
}

// MARK: - Enrollment flow

final class RelayEnrollmentTests: XCTestCase {
    /// Spin until a condition holds — the transport is mutated on the actor's
    /// executor, so the test waits for the parked state rather than guessing.
    private func poll(
        _ what: String, _ condition: @escaping () -> Bool, file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        for _ in 0..<400 {
            if condition() { return }
            try? await Task.sleep(nanoseconds: 5_000_000)
        }
        XCTFail("timed out waiting for \(what)", file: file, line: line)
    }

    func testEnrollStoresAndReturnsCredential() async {
        let attest = FakeAttest()
        let transport = FakeTransport()
        let store = InMemoryStore()
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "BEARER-1")]
        let enrollment = RelayEnrollment(attest: attest, transport: transport, store: store, installID: installID)

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.credential, "BEARER-1")
        XCTAssertEqual(credential.token, sampleToken)
        XCTAssertEqual(store.load()?.credential, "BEARER-1")
        // The attestation carried the raw challenge bytes, unhashed.
        XCTAssertEqual(attest.attestHashes, [challengeBytes])
    }

    /// The enroll request is exactly the shape the relay's `EnrollRequest`
    /// deserializes: schema 1, standard-base64 attestation, the challenge string,
    /// the token, and the environment.
    func testEnrollRequestFieldsMatchTheRelay() async throws {
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [credentialResponse()]
        let enrollment = makeEnrollment(transport: transport)

        _ = await enrollment.credential(token: sampleToken, environment: "production")
        let enroll = try XCTUnwrap(transport.posts.first { $0.path == RelayEndpoint.enroll })
        let body = try XCTUnwrap(
            JSONSerialization.jsonObject(with: enroll.body) as? [String: Any])
        XCTAssertEqual(body["schema"] as? Int, 1)
        XCTAssertEqual(body["key_id"] as? String, "FAKE-KEY-ID")
        XCTAssertEqual(body["challenge"] as? String, challengeB64URL)
        XCTAssertEqual(body["token"] as? String, sampleToken)
        XCTAssertEqual(body["environment"] as? String, "production")
        XCTAssertEqual(
            body["attestation"] as? String, Data("attestation-object".utf8).base64EncodedString())
    }

    func testUnsupportedDeviceNeverTouchesTheRelay() async {
        let attest = FakeAttest()
        attest.supported = false
        let transport = FakeTransport()
        let enrollment = makeEnrollment(attest: attest, transport: transport)

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        XCTAssertEqual(outcome, .unsupported)
        XCTAssertTrue(transport.posts.isEmpty, "no challenge, no attestation, no network")
    }

    /// Two concurrent requests for the same tuple share one enrollment. The gate
    /// proves the overlap: while the single in-flight enrollment is parked, both
    /// callers are already waiting on it, and only one challenge was fetched.
    func testSingleFlightSharesOneEnrollment() async {
        let attest = FakeAttest()
        let transport = FakeTransport()
        let gate = Gate()
        transport.enrollGate = { await gate.wait() }
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.enrollQueue = [credentialResponse(), credentialResponse()]
        let enrollment = RelayEnrollment(
            attest: attest, transport: transport, store: InMemoryStore(), installID: installID)

        async let first = enrollment.credential(token: sampleToken, environment: "production")
        async let second = enrollment.credential(token: sampleToken, environment: "production")

        await poll("one enrollment is parked at the gate") { transport.enrollPosts == 1 }
        XCTAssertEqual(transport.challengePosts, 1, "both callers shared one challenge")
        await gate.release()
        _ = await (first, second)
        XCTAssertEqual(transport.enrollPosts, 1, "one attestation, not two credentials")
        XCTAssertEqual(attest.generatedKeys, 1)
    }

    /// A credential that survived a reinstall (Keychain can; the App Attest key
    /// and the install id cannot) is not reused — it is freshly attested.
    func testReinstallWithSurvivingCredentialReenrolls() async {
        let survivor = seeded(credential: "STALE-BEARER", install: "OLD-INSTALL")
        let attest = FakeAttest()
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "FRESH")]
        let store = InMemoryStore(survivor)
        let enrollment = RelayEnrollment(
            attest: attest, transport: transport, store: store, installID: "NEW-INSTALL")

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.credential, "FRESH", "the surviving bearer was not reused")
        XCTAssertEqual(attest.generatedKeys, 1, "a fresh App Attest key was minted")
        XCTAssertEqual(store.load()?.installID, "NEW-INSTALL")
    }

    /// A late credential for a superseded token does not clobber the current
    /// binding in the store — the persistence guard, not just the register guard.
    func testLateResultDoesNotClobberTheStore() async {
        let gate = Gate()
        let transport = FakeTransport()
        transport.enrollGate = { await gate.wait() }
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "FOR-A"), credentialResponse(credential: "FOR-B")]
        let store = InMemoryStore()
        let tokenB = String(repeating: "b", count: 64)
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        async let a = enrollment.credential(token: sampleToken, environment: "production")
        await poll("A parked") { transport.enrollPosts == 1 }
        async let b = enrollment.credential(token: tokenB, environment: "production")
        await poll("B parked") { transport.enrollPosts == 2 }

        await gate.release()
        let outcomeA = await a
        _ = await b
        XCTAssertEqual(outcomeA, .superseded, "A finished after being superseded by B")
        XCTAssertEqual(store.load()?.token, tokenB, "the store holds B, not the late A")
        XCTAssertEqual(store.load()?.credential, "FOR-B")
    }

    /// Two consuming operations fetch two challenges — a challenge is never
    /// replayed across a rebind following an enroll.
    func testTwoConsumingOperationsFetchTwoChallenges() async {
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "FIRST")]
        transport.assertQueue = [credentialResponse(credential: "SECOND")]
        let store = InMemoryStore()
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        _ = await enrollment.credential(token: sampleToken, environment: "production")
        let otherToken = String(repeating: "c", count: 64)
        _ = await enrollment.credential(token: otherToken, environment: "production")
        XCTAssertEqual(transport.challengePosts, 2, "enroll and the rebind each fetched their own")
    }

    /// A stored credential for the current token is reused with no network at all.
    func testStoredCredentialIsReusedWithoutEnrolling() async {
        let existing = seeded(credential: "OLD")
        let transport = FakeTransport()
        let enrollment = makeEnrollment(transport: transport, store: InMemoryStore(existing))

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        XCTAssertEqual(outcome, .ready(existing))
        XCTAssertTrue(transport.posts.isEmpty)
    }

    /// A changed token rebinds the existing key by assertion rather than burning a
    /// new one, and stores the new binding.
    func testTokenChangeRebindsViaAssertion() async {
        let old = seeded(keyID: "KEEP-ME", token: "oldtoken", credential: "OLD")
        let attest = FakeAttest()
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse()]
        transport.assertQueue = [credentialResponse(credential: "NEW")]
        let store = InMemoryStore(old)
        let enrollment = RelayEnrollment(attest: attest, transport: transport, store: store, installID: installID)

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.token, sampleToken)
        XCTAssertEqual(credential.credential, "NEW")
        XCTAssertEqual(attest.generatedKeys, 0, "the existing key was reused")
        XCTAssertEqual(transport.assertPosts, 1)
        // The assertion signed a rebind for the new token.
        let assert = transport.posts.first { $0.path == RelayEndpoint.assert }!
        let body = try! JSONSerialization.jsonObject(with: assert.body) as! [String: Any]
        XCTAssertEqual(body["operation"] as? String, "rebind")
        XCTAssertEqual(body["token"] as? String, sampleToken)
    }

    // MARK: Foreground status recovery

    func testStatusActiveKeepsAndAdoptsEnvironment() async {
        let stored = seeded(environment: "sandbox")
        let transport = FakeTransport()
        transport.statusQueue = [jsonResponse(["status": "active", "environment": "production"])]
        let store = InMemoryStore(stored)
        let enrollment = RelayEnrollment(attest: FakeAttest(), transport: transport, store: store, installID: installID)

        let outcome = await enrollment.refresh(token: sampleToken, environment: "sandbox")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.environment, "production", "the relay's environment is adopted")
        XCTAssertEqual(store.load()?.environment, "production")
    }

    func testStatusReissueRotates() async {
        let stored = seeded(credential: "OLD")
        let transport = FakeTransport()
        transport.statusQueue = [jsonResponse(["status": "reissue"])]
        transport.challengeQueue = [challengeResponse()]
        transport.assertQueue = [credentialResponse(credential: "ROTATED")]
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: InMemoryStore(stored),
            installID: installID)

        let outcome = await enrollment.refresh(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.credential, "ROTATED")
        let assert = transport.posts.first { $0.path == RelayEndpoint.assert }!
        let body = try! JSONSerialization.jsonObject(with: assert.body) as! [String: Any]
        XCTAssertEqual(body["operation"] as? String, "rotate")
    }

    func testStatusReenrollMakesAFreshKey() async {
        let stored = seeded(keyID: "STALE", credential: "OLD")
        let attest = FakeAttest()
        attest.keyID = "BRAND-NEW"
        let transport = FakeTransport()
        transport.statusQueue = [jsonResponse(["status": "reenroll"])]
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "REENROLLED")]
        let store = InMemoryStore(stored)
        let enrollment = RelayEnrollment(attest: attest, transport: transport, store: store, installID: installID)

        let outcome = await enrollment.refresh(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.credential, "REENROLLED")
        XCTAssertEqual(attest.generatedKeys, 1, "a fresh key, not the stale one")
        XCTAssertEqual(store.load()?.keyID, "BRAND-NEW")
    }

    func testStatusTokenInvalidClearsAndSignals() async {
        let stored = seeded()
        let transport = FakeTransport()
        transport.statusQueue = [jsonResponse(["status": "token_invalid"])]
        let store = InMemoryStore(stored)
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        let outcome = await enrollment.refresh(token: sampleToken, environment: "production")
        XCTAssertEqual(outcome, .tokenInvalid)
        XCTAssertNil(store.load(), "a retired token's binding is discarded")
    }

    /// Keychain loss: nothing stored, so the register path enrolls fresh.
    func testKeychainLossReenrolls() async {
        let attest = FakeAttest()
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [credentialResponse(credential: "AFTER-LOSS")]
        let enrollment = RelayEnrollment(
            attest: attest, transport: transport, store: InMemoryStore(nil), installID: installID)

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.credential, "AFTER-LOSS")
        XCTAssertEqual(attest.generatedKeys, 1)
    }

    /// Every operation fetches its own challenge; a challenge is never reused
    /// across two calls (the relay consumes it once, and the app must not replay).
    func testEachOperationFetchesAFreshChallenge() async {
        let attest = FakeAttest()
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse(), challengeResponse()]
        transport.enrollQueue = [credentialResponse()]
        transport.statusQueue = [jsonResponse(["status": "active", "environment": "production"])]
        let store = InMemoryStore()
        let enrollment = RelayEnrollment(attest: attest, transport: transport, store: store, installID: installID)

        _ = await enrollment.credential(token: sampleToken, environment: "production")
        _ = await enrollment.refresh(token: sampleToken, environment: "production")
        // Enroll fetched one; the active-status refresh needs none — so exactly
        // one challenge was consumed, and the second remains unused.
        XCTAssertEqual(transport.challengePosts, 1)
    }

    /// A consumed or refused challenge surfaces as a retryable failure, not a
    /// crash and not a false success.
    func testRelayRefusalIsAFailure() async {
        let transport = FakeTransport()
        transport.challengeQueue = [challengeResponse()]
        transport.enrollQueue = [jsonResponse(["error": "challenge_consumed"], status: 409)]
        let enrollment = makeEnrollment(transport: transport, store: InMemoryStore())

        let outcome = await enrollment.credential(token: sampleToken, environment: "production")
        guard case .failed = outcome else { return XCTFail("\(outcome)") }
    }

    // MARK: Environment correction

    func testAdoptEnvironmentPersistsCorrectionWithoutReenrolling() async {
        let stored = seeded(environment: "sandbox")
        let transport = FakeTransport()
        let store = InMemoryStore(stored)
        let enrollment = RelayEnrollment(
            attest: FakeAttest(), transport: transport, store: store, installID: installID)

        let outcome = await enrollment.credential(
            token: sampleToken, environment: "sandbox", daemonEnvironment: "production")
        guard case .ready(let credential) = outcome else { return XCTFail("\(outcome)") }
        XCTAssertEqual(credential.environment, "production")
        XCTAssertEqual(credential.credential, "C", "the credential itself is unchanged")
        XCTAssertEqual(store.load()?.environment, "production")
        XCTAssertTrue(transport.posts.isEmpty, "a correction is persistence, not a round trip")
    }

    // MARK: Single-flight key

    func testFlightKeyHashesTokenAndSplitsByEnvironment() {
        let a = RelayEnrollment.flightKey(token: sampleToken, environment: "production")
        let b = RelayEnrollment.flightKey(token: sampleToken, environment: "production")
        let c = RelayEnrollment.flightKey(token: sampleToken, environment: "sandbox")
        let d = RelayEnrollment.flightKey(token: "other", environment: "production")
        XCTAssertEqual(a, b)
        XCTAssertNotEqual(a, c, "environment splits the key")
        XCTAssertNotEqual(a, d, "a different token is a different flight")
        XCTAssertFalse(a.contains(sampleToken), "the raw token is not in the key")
    }
}
