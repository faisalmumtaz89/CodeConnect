import CryptoKit
import Foundation

// `@preconcurrency`: `DCAppAttestService` predates `Sendable` annotations, and its
// only use here is `.shared` plus three completion-handler calls bridged to
// `async`. The import keeps the actor below free of Swift-6 Sendable noise
// without weakening any of this file's own isolation.
@preconcurrency import DeviceCheck

// =============================================================================
//  Relay enrollment — App Attest, kept apart from APNs permission/token duties.
//
//  `PushRegistration` still owns the notification prompt and the APNs token.
//  This actor owns the *other* half of the relay path: proving to the relay,
//  once per install, that a legitimate app instance is asking for a credential
//  bound to this phone's `(token, environment)`, and then keeping that
//  credential alive across token changes, restores, and revocations.
//
//  The one thing this file must get exactly right is the byte layout the relay
//  verifier hashes. Both constructions live in `RelayClientData` as pure
//  functions so a unit test proves the match without a device or a network.
// =============================================================================

/// The relay's App Attest routes. The host mirrors the daemon's baked constant
/// (`mac/ccd/src/relay_sender.rs` `RELAY_HOST`) — the single relay every install
/// reaches, deliberately not configurable (§9). The app talks to the same host;
/// the environment is a request field, not a second hostname.
enum RelayEndpoint {
    static let host = "codeconnect-push-relay.onrender.com"
    static let schema = 1

    static let challenge = "/v1/attest/challenge"
    static let enroll = "/v1/attest/enroll"
    static let assert = "/v1/attest/assert"
    static let status = "/v1/credential/status"
}

/// The exact bytes the relay verifier hashes. Getting either of these wrong is a
/// silent enrollment failure on real hardware, so they are pure and tested
/// against independently-computed golden vectors.
enum RelayClientData {
    /// **Attestation.** The relay does not hash the challenge — it treats the raw
    /// 32 decoded challenge bytes as the `clientDataHash` and recomputes
    /// `nonce = SHA256(authData ‖ clientDataHash)` itself. So the client must pass
    /// those raw bytes to `attestKey`, never `SHA256(challenge)`.
    static func attestation(challengeBytes: Data) -> Data { challengeBytes }

    /// The domain prefix, trailing NUL included — it is one `update` in the
    /// verifier, so the NUL is part of the constant, not a separator.
    static let assertionDomain = Data("codeconnect-relay/1/assert\u{0}".utf8)

    /// **Assertion.** `SHA256` over a domain-separated tuple the relay rebuilds
    /// from the same inputs. Order and separators are load-bearing: domain, then
    /// operation, NUL, challenge string, NUL, raw challenge bytes, NUL, token
    /// string, NUL, environment word.
    static func assertion(
        operation: String, challenge: String, challengeBytes: Data,
        token: String, environment: String
    ) -> Data {
        var input = assertionDomain
        input.append(Data(operation.utf8))
        input.append(0)
        input.append(Data(challenge.utf8))
        input.append(0)
        input.append(challengeBytes)
        input.append(0)
        input.append(Data(token.utf8))
        input.append(0)
        input.append(Data(environment.utf8))
        return Data(SHA256.hash(data: input))
    }
}

/// base64url without padding, which is how the relay issues the challenge and the
/// bearer. Only decoding is needed here — the challenge round-trips as its string
/// and only its raw bytes feed the two hashes above.
enum Base64URL {
    static func decode(_ value: String) -> Data? {
        var s = value.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        switch s.count % 4 {
        case 2: s += "=="
        case 3: s += "="
        case 1: return nil
        default: break
        }
        return Data(base64Encoded: s)
    }
}

// MARK: - App Attest, behind a seam

/// The slice of `DCAppAttestService` this flow uses. A protocol because the real
/// service returns `isSupported == false` on the simulator and refuses every
/// call, so every unit test injects a deterministic double instead.
protocol AppAttesting: Sendable {
    var isSupported: Bool { get }
    func generateKey() async throws -> String
    func attestKey(_ keyID: String, clientDataHash: Data) async throws -> Data
    func generateAssertion(_ keyID: String, clientDataHash: Data) async throws -> Data
}

/// The production adapter over Apple's service. It holds no state; the key
/// material lives in Apple's hardware-backed store, addressed by key id.
struct DeviceAppAttest: AppAttesting {
    /// Computed, not stored: `DCAppAttestService` is a non-`Sendable` reference
    /// type, and storing it in this `Sendable` struct would be the only thing
    /// that made the struct unsound. `.shared` is a process singleton, so
    /// re-reading it costs nothing.
    private var service: DCAppAttestService { .shared }

    var isSupported: Bool { service.isSupported }

    func generateKey() async throws -> String {
        try await withCheckedThrowingContinuation { continuation in
            service.generateKey { keyID, error in
                switch (keyID, error) {
                case (let keyID?, _): continuation.resume(returning: keyID)
                case (_, let error?): continuation.resume(throwing: error)
                default: continuation.resume(throwing: RelayEnrollmentError.attestUnavailable)
                }
            }
        }
    }

    func attestKey(_ keyID: String, clientDataHash: Data) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            service.attestKey(keyID, clientDataHash: clientDataHash) { attestation, error in
                switch (attestation, error) {
                case (let attestation?, _): continuation.resume(returning: attestation)
                case (_, let error?): continuation.resume(throwing: error)
                default: continuation.resume(throwing: RelayEnrollmentError.attestUnavailable)
                }
            }
        }
    }

    func generateAssertion(_ keyID: String, clientDataHash: Data) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            service.generateAssertion(keyID, clientDataHash: clientDataHash) { assertion, error in
                switch (assertion, error) {
                case (let assertion?, _): continuation.resume(returning: assertion)
                case (_, let error?): continuation.resume(throwing: error)
                default: continuation.resume(throwing: RelayEnrollmentError.attestUnavailable)
                }
            }
        }
    }
}

// MARK: - Transport, behind a seam

struct RelayHTTPResponse: Sendable {
    let status: Int
    let body: Data
}

/// One relay request. A protocol so the enrollment state machine can be proven
/// against a scripted relay that answers instantly and exactly — no real service
/// can produce a consumed challenge or a revoked credential on demand.
protocol RelayTransport: Sendable {
    func post(path: String, json: Data) async throws -> RelayHTTPResponse
    func get(path: String, bearer: String) async throws -> RelayHTTPResponse
}

/// The production transport: HTTPS to the one baked host, bearer in
/// `Authorization` and nowhere else.
struct URLSessionRelayTransport: RelayTransport {
    private let session: URLSession = .shared

    private func url(_ path: String) -> URL {
        var components = URLComponents()
        components.scheme = "https"
        components.host = RelayEndpoint.host
        components.path = path
        // A baked host and a fixed path set cannot fail to form a URL, but the
        // API is non-throwing here rather than force-unwrapping in a network path.
        return components.url ?? URL(string: "https://\(RelayEndpoint.host)\(path)")!
    }

    func post(path: String, json: Data) async throws -> RelayHTTPResponse {
        var request = URLRequest(url: url(path))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = json
        let (data, response) = try await session.data(for: request)
        return RelayHTTPResponse(status: (response as? HTTPURLResponse)?.statusCode ?? 0, body: data)
    }

    func get(path: String, bearer: String) async throws -> RelayHTTPResponse {
        var request = URLRequest(url: url(path))
        request.httpMethod = "GET"
        request.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization")
        let (data, response) = try await session.data(for: request)
        return RelayHTTPResponse(status: (response as? HTTPURLResponse)?.statusCode ?? 0, body: data)
    }
}

// MARK: - Errors and wire DTOs

enum RelayEnrollmentError: Error {
    case attestUnavailable
    case badChallenge
    case http(Int)
    case malformedResponse
}

private struct ChallengeRequestBody: Encodable {
    let schema: Int
}

private struct ChallengeResponseBody: Decodable {
    let challenge: String
}

private struct EnrollRequestBody: Encodable {
    let schema: Int
    let keyID: String
    let attestation: String
    let challenge: String
    let token: String
    let environment: String

    enum CodingKeys: String, CodingKey {
        case schema
        case keyID = "key_id"
        case attestation
        case challenge
        case token
        case environment
    }
}

private struct AssertRequestBody: Encodable {
    let schema: Int
    let keyID: String
    let assertion: String
    let challenge: String
    let operation: String
    let token: String
    let environment: String

    enum CodingKeys: String, CodingKey {
        case schema
        case keyID = "key_id"
        case assertion
        case challenge
        case operation
        case token
        case environment
    }
}

/// The relay's credential body, returned by enroll and by a rebind/rotate assert.
private struct CredentialBody: Decodable {
    let credential: String
    let environment: String
    let generation: Int
}

private struct StatusBody: Decodable {
    let status: String
    let environment: String?
}

// MARK: - The actor

/// Owns the relay binding and every operation that mints, rebinds, refreshes, or
/// discards it. An `actor` so single-flight and the stored binding are serialized
/// without a lock; the caller is `AppModel` on the main actor.
actor RelayEnrollment {
    /// What the register path should do with the result.
    enum Outcome: Equatable {
        /// Register the token with this credential. Its `token` is the binding it
        /// was minted for — the caller rechecks it against the live token before
        /// sending, so a result that arrived for a superseded token is dropped.
        case ready(RelayCredential)
        /// This device has no App Attest; there is no relay push and, by design,
        /// no insecure fallback. All non-relay function stays.
        case unsupported
        /// The relay says this APNs token is dead (`410`-derived). The caller
        /// discards the tuple and asks APNs to register again.
        case tokenInvalid
        /// A transient failure — challenge fetch, attestation, network, a relay
        /// refusal that is not terminal. Retried on the next foreground.
        case failed(String)
        /// The token this work was for stopped being the current one before it
        /// finished (a rotation, or a reset). The result is dropped: not stored,
        /// not registered. The superseding request carries its own credential.
        case superseded
    }

    private let attest: AppAttesting
    private let transport: RelayTransport
    private let store: RelayCredentialStoring
    /// The install this actor is running in. Stamped into every minted binding
    /// and compared on reuse, so a credential that survived a reinstall (Keychain
    /// can; the App Attest key and `UserDefaults` cannot) is detected and freshly
    /// attested rather than reused with a key that no longer exists.
    private let installID: String

    /// In-flight enrollments keyed by `(token hash, environment)`, so two
    /// handshakes racing to enroll the same tuple share one attestation instead
    /// of minting two credentials and revoking the first.
    private var inFlight: [String: Task<Outcome, Never>] = [:]

    /// The token the most recent request was for. A completing flight persists
    /// its credential only while this still matches, so a late result for a
    /// superseded token cannot clobber the current binding in the Keychain, and a
    /// reset (which nils this) cannot be undone by an enrollment already in
    /// flight.
    private var latestToken: String?

    init(
        attest: AppAttesting = DeviceAppAttest(),
        transport: RelayTransport = URLSessionRelayTransport(),
        store: RelayCredentialStoring = KeychainRelayCredentialStore(),
        installID: String = DeviceIdentity.installationID
    ) {
        self.attest = attest
        self.transport = transport
        self.store = store
        self.installID = installID
    }

    // MARK: Public operations

    /// The register path's single entry. Returns a credential bound to
    /// `(token, environment)`, doing the least work that yields one: reuse a
    /// stored binding for this token, rebind an existing key to a changed token,
    /// or enroll fresh. Single-flight per tuple.
    ///
    /// `daemonEnvironment` is `hello_ack.push_environment`. Applying it here,
    /// under the actor, before the stored binding is read makes the correction
    /// atomic with the credential the caller is about to register — so the app
    /// registers the corrected environment and never resends its stale one.
    func credential(
        token: String, environment: String, daemonEnvironment: String? = nil
    ) async -> Outcome {
        latestToken = token
        if let daemonEnvironment {
            adoptEnvironment(daemonEnvironment, token: token)
        }
        if let stored = store.load(), stored.token == token, stored.installID == installID {
            return .ready(stored)
        }
        return await singleFlight(token: token, environment: environment) {
            // A key from *this* install exists for a different (old) token:
            // rebind it rather than burning a new App Attest key. A key from a
            // previous install (surviving Keychain, lost key) fails this check
            // and takes the fresh-enroll path, which is what a reinstall needs.
            if let stored = self.store.load(), stored.installID == self.installID {
                return await self.runAssert(
                    operation: "rebind", keyID: stored.keyID, token: token,
                    environment: environment)
            }
            return await self.runEnroll(token: token, environment: environment)
        }
    }

    /// Foreground recovery. Asks the read-only status endpoint what became of the
    /// stored binding and acts on the answer: keep it, rotate via assertion,
    /// re-enroll with a fresh key, or surrender the token to APNs.
    func refresh(token: String, environment: String) async -> Outcome {
        latestToken = token
        guard let stored = store.load(), stored.token == token, stored.installID == installID
        else {
            return await credential(token: token, environment: environment)
        }
        let response: RelayHTTPResponse
        do {
            response = try await transport.get(path: RelayEndpoint.status, bearer: stored.credential)
        } catch {
            return .failed("status check failed: \(error.localizedDescription)")
        }
        guard response.status == 200,
            let body = try? JSONDecoder().decode(StatusBody.self, from: response.body)
        else { return .failed("status unavailable (\(response.status))") }

        switch body.status {
        case "active":
            if let corrected = body.environment {
                let updated = stored.adoptingEnvironment(corrected)
                store.save(updated)
                return .ready(updated)
            }
            return .ready(stored)
        case "reissue":
            // Sign against the caller's environment, not the stored one. The
            // caller passes the build's App Attest namespace environment, and the
            // relay verifies the assertion under that namespace; a stored value
            // the relay had corrected within the namespace stays consistent, and
            // a value that somehow diverged cannot make the assertion unverifiable.
            return await singleFlight(token: token, environment: environment) {
                await self.runAssert(
                    operation: "rotate", keyID: stored.keyID, token: token,
                    environment: environment)
            }
        case "reenroll":
            store.clear()
            return await singleFlight(token: token, environment: environment) {
                await self.runEnroll(token: token, environment: environment)
            }
        case "token_invalid":
            store.clear()
            return .tokenInvalid
        default:
            return .failed("unknown status: \(body.status)")
        }
    }

    /// "Reset notification registration." Forgets the local binding and
    /// invalidates any enrollment already in flight — nils `latestToken` so a
    /// completion cannot re-save the credential this just cleared, and drops the
    /// flight handles so the caller's re-drive starts a fresh attestation rather
    /// than joining the one being discarded.
    ///
    /// A lost-Mac incident is not a separate control: the relay raises its
    /// generation floor, the next foreground `refresh` reads `reissue`, and the
    /// assertion-authorized rotation runs there — then redistributes on each
    /// Mac's next handshake.
    func reset() {
        latestToken = nil
        inFlight.removeAll()
        store.clear()
    }

    /// Persist a daemon-corrected environment without re-enrolling — the
    /// `hello_ack.push_environment` propagation path. No-op unless a binding for
    /// this exact token exists and the value actually differs.
    func adoptEnvironment(_ environment: String, token: String) {
        guard let stored = store.load(), stored.token == token,
            stored.environment != environment
        else { return }
        store.save(stored.adoptingEnvironment(environment))
    }

    /// The binding the caller should register with, if any — used to attach the
    /// credential to a `RegisterPush` without forcing an enrollment.
    func storedCredential(forToken token: String) -> RelayCredential? {
        guard let stored = store.load(), stored.token == token else { return nil }
        return stored
    }

    // MARK: Flow core

    private func runEnroll(token: String, environment: String) async -> Outcome {
        guard attest.isSupported else { return .unsupported }
        let challenge: (string: String, bytes: Data)
        switch await fetchChallenge() {
        case .ok(let string, let bytes): challenge = (string, bytes)
        case .failed(let reason): return .failed(reason)
        }

        let keyID: String
        let attestation: Data
        do {
            keyID = try await attest.generateKey()
            attestation = try await attest.attestKey(
                keyID, clientDataHash: RelayClientData.attestation(challengeBytes: challenge.bytes))
        } catch {
            return .failed("attestation failed: \(error.localizedDescription)")
        }

        let body = EnrollRequestBody(
            schema: RelayEndpoint.schema, keyID: keyID,
            attestation: attestation.base64EncodedString(), challenge: challenge.string,
            token: token, environment: environment)
        return await postCredential(path: RelayEndpoint.enroll, body: body, keyID: keyID, token: token)
    }

    private func runAssert(
        operation: String, keyID: String, token: String, environment: String
    ) async -> Outcome {
        guard attest.isSupported else { return .unsupported }
        let challenge: (string: String, bytes: Data)
        switch await fetchChallenge() {
        case .ok(let string, let bytes): challenge = (string, bytes)
        case .failed(let reason): return .failed(reason)
        }

        let clientDataHash = RelayClientData.assertion(
            operation: operation, challenge: challenge.string, challengeBytes: challenge.bytes,
            token: token, environment: environment)
        let assertion: Data
        do {
            assertion = try await attest.generateAssertion(keyID, clientDataHash: clientDataHash)
        } catch {
            // A key the hardware no longer has cannot assert; fall back to a fresh
            // enrollment so a changed or restored device still recovers.
            return await runEnroll(token: token, environment: environment)
        }

        let body = AssertRequestBody(
            schema: RelayEndpoint.schema, keyID: keyID,
            assertion: assertion.base64EncodedString(), challenge: challenge.string,
            operation: operation, token: token, environment: environment)
        return await postCredential(path: RelayEndpoint.assert, body: body, keyID: keyID, token: token)
    }

    private func postCredential(
        path: String, body: Encodable, keyID: String, token: String
    ) async -> Outcome {
        let json: Data
        do {
            json = try JSONEncoder().encode(AnyEncodable(body))
        } catch {
            return .failed("could not encode request")
        }
        let response: RelayHTTPResponse
        do {
            response = try await transport.post(path: path, json: json)
        } catch {
            return .failed("relay unreachable: \(error.localizedDescription)")
        }
        guard response.status == 200,
            let credential = try? JSONDecoder().decode(CredentialBody.self, from: response.body)
        else { return .failed("relay refused enrollment (\(response.status))") }

        let record = RelayCredential(
            keyID: keyID, token: token, environment: credential.environment,
            credential: credential.credential, generation: credential.generation,
            installID: installID)
        // Only persist while this is still the current token. A rotation or a
        // reset moved `latestToken` on while this flight was in the air, so
        // saving now would resurrect a superseded binding in the Keychain.
        guard token == latestToken else { return .superseded }
        store.save(record)
        return .ready(record)
    }

    /// The outcome of fetching and decoding one challenge — a challenge and its
    /// raw bytes, or a reader-facing reason. Its own type rather than a `Result`
    /// because the failure is a plain reason string, not an `Error`.
    private enum Challenge {
        case ok(string: String, bytes: Data)
        case failed(String)
    }

    private func fetchChallenge() async -> Challenge {
        let json: Data
        do {
            json = try JSONEncoder().encode(ChallengeRequestBody(schema: RelayEndpoint.schema))
        } catch {
            return .failed("could not encode challenge request")
        }
        let response: RelayHTTPResponse
        do {
            response = try await transport.post(path: RelayEndpoint.challenge, json: json)
        } catch {
            return .failed("relay unreachable: \(error.localizedDescription)")
        }
        guard response.status == 200,
            let body = try? JSONDecoder().decode(ChallengeResponseBody.self, from: response.body),
            let bytes = Base64URL.decode(body.challenge), bytes.count == 32
        else { return .failed("bad challenge (\(response.status))") }
        return .ok(string: body.challenge, bytes: bytes)
    }

    private func singleFlight(
        token: String, environment: String, _ work: @escaping @Sendable () async -> Outcome
    ) async -> Outcome {
        let key = Self.flightKey(token: token, environment: environment)
        if let existing = inFlight[key] { return await existing.value }
        let task = Task { await work() }
        inFlight[key] = task
        let outcome = await task.value
        inFlight[key] = nil
        return outcome
    }

    /// The single-flight key hashes the token — the plan keys by `(token hash,
    /// environment)`, and a hash keeps the raw token out of a dictionary that
    /// outlives the request.
    static func flightKey(token: String, environment: String) -> String {
        let digest = SHA256.hash(data: Data(token.utf8))
        let hex = digest.map { String(format: "%02x", $0) }.joined()
        return "\(hex):\(environment)"
    }
}

/// Erases `Encodable` so one `postCredential` handles both request bodies. The
/// concrete types are the only things ever wrapped, so encoding cannot fail for a
/// reason the call site could have prevented.
private struct AnyEncodable: Encodable {
    private let encode: (Encoder) throws -> Void
    init(_ wrapped: Encodable) { encode = wrapped.encode }
    func encode(to encoder: Encoder) throws { try encode(encoder) }
}
