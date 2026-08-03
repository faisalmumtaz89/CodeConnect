import Foundation
import Network
import os
import Observation

enum ConnectionError: LocalizedError, Sendable {
    case notConnected
    case timedOut
    case unauthorized
    /// A pairing code the daemon would not accept.
    ///
    /// Separate from `unauthorized` only for its noun. The daemon deliberately
    /// collapses "unknown", "expired" and "already used" into one opaque refusal —
    /// telling an unauthenticated caller which applies would be a free oracle — so
    /// this cannot say *why*, and must not pretend to. What it can do is name the
    /// thing the reader is holding, which is a code, not a token.
    case rejectedPairingCode
    /// The daemon speaks a protocol this app does not. Terminal: retrying cannot
    /// change it, and the fix is at the Mac.
    case incompatible(String)
    case invalidEndpoint
    case server(code: String, message: String)
    case transport(String)
    /// DNS could not resolve the daemon's name — classified at the throw site
    /// because both transport wrappers otherwise reduce a `URLError` to its
    /// sentence, and the sentence cannot be matched honestly.
    case hostUnresolvable

    var errorDescription: String? {
        switch self {
        case .notConnected: return "Not connected to the daemon"
        case .timedOut: return "The daemon did not answer in time"
        case .unauthorized: return "The daemon rejected this token"
        case .rejectedPairingCode:
            return
                "The Mac rejected that pairing code. It may have expired, already been used, or been mistyped, run `codeconnect pair` again for a fresh one."
        case .incompatible(let detail): return detail
        case .invalidEndpoint: return "That address could not be turned into a URL"
        case .server(let code, let message): return "\(message) (\(code))"
        case .transport(let message): return message
        case .hostUnresolvable: return "The daemon's address is not resolving"
        }
    }
}

/// One WebSocket to one `ccd`, kept alive across drops.
///
/// The state machine is deliberately visible: every screen renders link health
/// from `phase` and `lastContactAt` rather than inferring liveness from the
/// existence of a socket, which is the mistake this exists to prevent.
@MainActor
@Observable
final class DaemonConnection {
    enum Phase: Equatable, Sendable {
        case idle
        case connecting
        case connected(since: Date)
        /// Backing off after a drop. Recoverable without the user doing anything.
        case waiting(until: Date, reason: String)
        /// Needs the user: a rejected token will be rejected again forever.
        case failed(reason: String)

        var isConnected: Bool {
            if case .connected = self { return true }
            return false
        }
    }

    // MARK: Observable state

    private(set) var phase: Phase = .idle {
        didSet {
            // Maintained here rather than at any caller, so every present and
            // future writer of `phase` keeps it true by construction. The
            // earliest entry into `.connecting` is kept: re-entering while
            // already dialling is the same attempt, not a fresh grace.
            switch phase {
            case .connecting: connectingSince = connectingSince ?? Date()
            default: connectingSince = nil
            }
        }
    }
    /// When the current dial attempt began, or nil when not dialling. The
    /// banners' grace clock: a banner that flashes during the seconds an
    /// ordinary connect is allowed to take is noise, and this is the fact that
    /// says those seconds are still running.
    private(set) var connectingSince: Date?
    private(set) var capabilities: Capabilities?
    /// The `hello_ack` this connection got, or nil before the handshake.
    private(set) var helloAck: HelloAck?
    /// Everything the app needs to know about which daemon build this is.
    var profile: DaemonProfile {
        guard let helloAck else { return .unknown }
        return DaemonProfile(
            protocolVersion: helloAck.protocolVersion,
            protocolMinor: helloAck.protocolMinor,
            capabilities: helloAck.capabilities,
            deviceName: helloAck.deviceName,
            sshKeyInstalled: helloAck.sshKeyInstalled)
    }
    /// When a frame last arrived from the daemon. The only honest basis for a
    /// freshness claim — a socket that is open but silent is not fresh.
    private(set) var lastContactAt: Date?
    private(set) var serverTime: String?
    private(set) var reconnectCount = 0
    private(set) var lastErrorMessage: String?
    /// When `lastErrorMessage` was set. A request that times out *after* the
    /// daemon complained about something can quote the complaint instead of
    /// guessing — without ever routing an uncorrelated frame to a waiter.
    private(set) var lastErrorAt: Date?
    /// The last complaint that **the daemon** put on the wire, kept apart from
    /// `lastErrorMessage`.
    ///
    /// The two are not the same thing and treating them as one is how the diff
    /// sheet came to print `Connecting to the daemon…` under the words *the
    /// daemon's own reason follows, verbatim*. `lastErrorMessage` also carries
    /// the app's own account of the link — `The daemon closed the connection`,
    /// a protocol-version mismatch, an unreadable frame — which are true, and
    /// are not quotations. Only an `error` frame lands here, so only this may
    /// be shown as something the Mac said.
    private(set) var lastDaemonErrorMessage: String?
    private(set) var lastDaemonErrorAt: Date?
    /// Increments on every successful handshake. Work started for one
    /// connection must not be applied after a later one has replaced it.
    private(set) var generation = 0

    /// Delivered on the main actor, in arrival order.
    var onMessage: ((ServerMessage) -> Void)?
    /// Fired after every successful `hello_ack`, which is where subscriptions
    /// must be re-established.
    var onConnected: (() -> Void)?
    /// Fired when a `hello_ack` carries a device token — the durable credential
    /// a pairing code is exchanged for. Exactly once per successful pairing.
    var onDeviceToken: ((String) -> Void)?
    /// Fired when the transport that actually worked is not the one that was
    /// stored, so the pairing can remember it.
    var onTransportSettled: ((Bool) -> Void)?

    /// Whether the *current* attempt is `wss://`. Reported, never assumed: the
    /// pairing's stored preference is only a starting point.
    private(set) var usingTLS = false

    /// Why the last dial of this `start` failed, typed — the stable fact the
    /// banner renders while retries cycle underneath it. Set by `supervise`,
    /// cleared on `start`, `stop` and a successful `hello_ack`, and it names
    /// its host so a stale classification can never dress a replacement
    /// endpoint.
    enum DialFailure: Equatable, Sendable {
        /// DNS cannot resolve the daemon's name. On a `*.ts.net` pairing this
        /// almost always means Tailscale is off on this phone — but it is
        /// recovery guidance, not an observed fact, and the copy built from it
        /// must not claim more than DNS reported.
        case hostUnresolvable(host: String)
        case other(message: String)
    }
    private(set) var dialFailure: DialFailure?
    /// Whether this `start` has already completed at least one failed dial —
    /// what separates the launch dial (whose quiet grace is earned) from a
    /// redial (whose blank-out was the flicker).
    private(set) var isRedial = false

    // MARK: Private

    private var endpoint: DaemonEndpoint?

    /// Whether this connection still holds a credential that can only be spent
    /// once. False from the moment a pairing exchange has upgraded it to the
    /// durable token, which is the invariant `PairingUpgradeTests` pins.
    var holdsPairingCode: Bool { endpoint?.isPairingCode ?? false }
    /// Offered on every hello. Only a daemon whose operator ran `codeconnect pair --ssh`
    /// does anything with it.
    private var sshPublicKey: String?
    /// True while the ladder is trying the scheme the pairing does *not* prefer.
    private var schemeAlternate = false
    private var supervisor: Task<Void, Never>?
    private var socket: URLSessionWebSocketTask?
    private var wakeTask: Task<Void, Never>?
    private var session: URLSession?

    /// A pending reply, tagged so a timeout can only ever cancel *its own*
    /// request. Without the ticket, answering the same `request_id` twice — the
    /// duplicate-answer path the ledger exists for — would let the first
    /// request's expired timer cancel the second.
    ///
    /// `continuation` becomes nil when the request is abandoned (timed out or
    /// failed to send) while its reply may still be in flight. The entry stays
    /// in the queue as a tombstone so the late reply is *consumed and dropped*
    /// rather than handed to the next request for the same key. Replies carry no
    /// ticket on the wire (`send_text_result` and `capture_result` are keyed
    /// only by session), so position is the only correlation available and it
    /// has to be kept exact.
    private struct Waiter<T> {
        let ticket: UInt64
        var continuation: CheckedContinuation<T, Error>?
    }

    private var answerWaiters: [String: [Waiter<AnswerResult>]] = [:]
    private var sendTextWaiters: [String: [Waiter<SendTextResult>]] = [:]
    private var captureWaiters: [String: [Waiter<String>]] = [:]
    private var deleteWaiters: [String: [Waiter<DeleteSessionResult>]] = [:]
    private var testPushWaiters: [String: [Waiter<TestPushResult>]] = [:]
    private var diffWaiters: [String: [Waiter<SessionDiff>]] = [:]
    private var nextTicket: UInt64 = 0

    /// Comfortably inside the 30s URLSession idle window, so an idle link still
    /// produces traffic — and a fresh `lastContactAt` — every few seconds.
    private static let pingInterval: Duration = .seconds(10)
    private static let requestTimeout: Duration = .seconds(20)
    /// Comfortably longer than the daemon's own 10s handshake window.
    private static let handshakeTimeout: Duration = .seconds(15)
    nonisolated private static let maxBackoff: Double = 30

    // MARK: - Lifecycle

    func start(endpoint: DaemonEndpoint, sshPublicKey: String? = nil) {
        if self.endpoint == endpoint, self.sshPublicKey == sshPublicKey, supervisor != nil,
            !isFailed
        {
            return
        }
        stop()
        self.endpoint = endpoint
        self.sshPublicKey = sshPublicKey
        schemeAlternate = false
        lastErrorMessage = nil
        lastErrorAt = nil
        dialFailure = nil
        isRedial = false
        ledger.reset()
        supervisor = Task { [weak self] in await self?.supervise() }
        startPathMonitor()
    }

    func stop() {
        supervisor?.cancel()
        supervisor = nil
        wakeTask?.cancel()
        wakeTask = nil
        pathMonitor?.cancel()
        pathMonitor = nil
        dialFailure = nil
        isRedial = false
        ledger.reset()
        closeCurrentSocket()
        failAllWaiters(with: ConnectionError.notConnected)
        phase = .idle
        capabilities = nil
        helloAck = nil
    }

    /// Called when the app returns to the foreground: waiting out a backoff
    /// while the user is looking at the screen is the wrong trade.
    func retryNow() {
        guard endpoint != nil else { return }
        if isFailed {
            lastErrorMessage = nil
            if let endpoint { start(endpoint: endpoint) }
            return
        }
        wakeTask?.cancel()
    }

    private var isFailed: Bool {
        if case .failed = phase { return true }
        return false
    }

    // MARK: - Supervision

    private func supervise() async {
        while !Task.isCancelled {
            guard let endpoint else {
                phase = .failed(reason: ConnectionError.invalidEndpoint.localizedDescription)
                return
            }
            let tls = resolveTLS(for: endpoint)
            guard let url = endpoint.url(useTLS: tls) else {
                phase = .failed(reason: ConnectionError.invalidEndpoint.localizedDescription)
                return
            }
            usingTLS = tls

            phase = .connecting
            do {
                try await runConnection(url: url, credential: endpoint.credential)
                // A clean close is still a disconnect; reconnect promptly.
                ledger.reset()
                dialFailure = .other(message: "The daemon closed the connection")
                lastErrorMessage = "The daemon closed the connection"
            } catch let error as ConnectionError {
                // A dial that had completed its handshake before dying ends
                // the failure run: what follows is a fresh outage, not
                // attempt N+1 of the old one.
                if helloAck != nil { ledger.reset() }
                // Terminal refusals: the same attempt will be refused forever, so
                // backing off would only hide the answer behind a spinner.
                switch error {
                case .unauthorized, .rejectedPairingCode, .incompatible:
                    phase = .failed(reason: error.localizedDescription)
                    return
                case .hostUnresolvable:
                    dialFailure = .hostUnresolvable(host: endpoint.host)
                    ledger.dnsFailure()
                default:
                    dialFailure = .other(message: error.localizedDescription)
                    ledger.failure()
                }
                lastErrorMessage = error.localizedDescription
            } catch is CancellationError {
                return
            } catch {
                if helloAck != nil { ledger.reset() }
                dialFailure = .other(message: error.localizedDescription)
                ledger.failure()
                lastErrorMessage = error.localizedDescription
            }
            isRedial = true

            // **A pairing code is not worth retrying forever.**
            //
            // A device token is durable: the Mac may be asleep, the tunnel may be
            // down, and backing off until either changes is exactly right. A
            // pairing code is single-use and dies after five minutes, so the same
            // patience becomes a lie — the app sat on `.waiting`, `pairingError`
            // reads only `.failed`, and the pairing screen therefore showed a
            // progress ring and "Exchanging the code for a device token…"
            // indefinitely, for a code that had already expired. No error, ever.
            //
            // Both schemes are still tried, because the ws/wss alternation above
            // is how a TLS mismatch corrects itself and giving up before it has
            // swapped once would turn a recoverable setup into a dead end.
            if case .pairingCode = endpoint.credential, ledger.attempts >= Self.pairingAttemptLimit {
                phase = .failed(
                    reason: lastErrorMessage ?? ConnectionError.timedOut.localizedDescription)
                return
            }

            if Task.isCancelled { return }
            capabilities = nil
            helloAck = nil
            reconnectCount += 1

            // Two consecutive *TLS-eligible* failures on one scheme, then try
            // the other.
            //
            // A daemon that turns TLS on stops speaking `ws://` on that port
            // altogether — nothing arrives to *tell* the app to switch, because
            // the handshake never completes. Guessing from URLSession error
            // codes is guesswork; alternating is not, and it self-corrects in
            // both directions for the price of at most one extra attempt.
            //
            // DNS failures are counted separately: no TCP ever happened, so the
            // scheme is not implicated — alternating on them would both narrate
            // a switch that cannot help and let a stack of stale DNS attempts
            // flip the scheme on the first real failure after DNS recovers.
            if ledger.takeAlternation() {
                schemeAlternate.toggle()
            }

            let delay = ledger.delay()
            phase = .waiting(
                until: Date().addingTimeInterval(delay),
                reason: lastErrorMessage ?? "Reconnecting")
            await sleepInterruptibly(seconds: delay)
        }
    }

    /// How many failed attempts end a pairing exchange.
    ///
    /// Four: enough for the `ws`/`wss` alternation to have tried both schemes
    /// when the failures are scheme-eligible (DNS failures never advance the
    /// alternation — no TCP happened, so the scheme is not implicated), and
    /// short enough to finish well inside the code's five-minute life so the
    /// reader is told while the code they are holding is still the one that
    /// failed.
    static let pairingAttemptLimit = 4

    /// Which scheme this attempt should use.
    ///
    /// A `tailscale cert` certificate carries a DNS SAN, so TLS against a
    /// literal address can never validate no matter how willing both ends are.
    /// Spending attempts on it would only produce a confusing certificate
    /// error, so an IP-literal pairing stays on `ws://` (the tailnet still
    /// carries the encryption) and Settings explains how to get the encrypted
    /// one.
    private func resolveTLS(for endpoint: DaemonEndpoint) -> Bool {
        if endpoint.hostIsIPLiteral { return false }
        return schemeAlternate ? !endpoint.useTLS : endpoint.useTLS
    }

    /// The retry counters, owned together because their invariant is shared:
    /// **every one of them is about the current unbroken run of failures**,
    /// and a successful handshake ends that run. Kept as raw properties they
    /// drifted — a scheme-eligible failure survived a successful `hello_ack`,
    /// so a later unrelated disconnect became "failure two" and switched away
    /// from a scheme that demonstrably worked; a fresh `start` inherited the
    /// previous outage's DNS streak and began at the 20–30s cadence.
    struct RetryLedger: Equatable {
        /// Failed dials since the last handshake (or start).
        private(set) var attempts = 0
        /// Consecutive unresolvable-host failures, driving the DNS cadence.
        private(set) var dnsStreak = 0
        /// Consecutive failures a scheme switch could plausibly fix. DNS
        /// failures never count: no TCP happened, the scheme is not
        /// implicated, and stale DNS attempts must not flip the scheme on
        /// the first real failure after DNS recovers.
        private(set) var schemeEligibleFailures = 0

        /// A dial failed with DNS unable to resolve the host.
        mutating func dnsFailure() {
            attempts += 1
            dnsStreak += 1
        }

        /// A dial failed for any scheme-eligible reason.
        mutating func failure() {
            attempts += 1
            dnsStreak = 0
            schemeEligibleFailures += 1
        }

        /// The connection completed a handshake, or a new `start` began:
        /// whatever run of failures was accumulating is over.
        mutating func reset() {
            self = RetryLedger()
        }

        /// Whether the ladder should switch scheme now — true on every second
        /// eligible failure, consuming the pair.
        mutating func takeAlternation() -> Bool {
            guard schemeEligibleFailures > 0, schemeEligibleFailures.isMultiple(of: 2) else {
                return false
            }
            schemeEligibleFailures = 0
            return true
        }

        /// The next backoff, from whichever cadence the failure run is in.
        func delay(unit: Double = Double.random(in: 0...1)) -> Double {
            dnsStreak > 0
                ? DaemonConnection.dnsBackoff(streak: dnsStreak, unit: unit)
                : DaemonConnection.backoff(attempt: attempts, unit: unit)
        }
    }

    private var ledger = RetryLedger()

    /// Exponential with full jitter, so a daemon restart does not meet a
    /// thundering herd of retries from every client at the same instant.
    nonisolated static func backoff(attempt: Int, unit: Double = Double.random(in: 0...1)) -> Double {
        guard attempt > 0 else { return 0.5 }
        let ceiling = min(maxBackoff, 0.5 * pow(2, Double(attempt - 1)))
        return ceiling / 2 + unit * (ceiling / 2)
    }

    /// The cadence for a host that does not resolve. DNS answers instantly and
    /// costs nothing, but every attempt cycles the connection's state — so two
    /// fast tries absorb a transient blip, then the pace drops to one the
    /// screen can be calm over. Recovery does not wait on it: the path monitor
    /// and foregrounding both wake the backoff early.
    nonisolated static func dnsBackoff(streak: Int, unit: Double = Double.random(in: 0...1)) -> Double {
        switch streak {
        case ..<3: return backoff(attempt: streak, unit: unit)
        case 3: return 10 + unit * 5
        default: return 20 + unit * 10
        }
    }

    /// Whether this error, anywhere down its underlying chain, is DNS failing
    /// to resolve the host — `.cannotFindHost` or `.dnsLookupFailed`.
    static func isUnresolvableHost(_ error: Error) -> Bool {
        var current: NSError? = error as NSError
        while let e = current {
            if e.domain == NSURLErrorDomain,
                e.code == NSURLErrorCannotFindHost || e.code == NSURLErrorDNSLookupFailed
            {
                return true
            }
            current = e.userInfo[NSUnderlyingErrorKey] as? NSError
        }
        return false
    }

    /// Wakes a recoverable backoff the moment the network path changes —
    /// Tailscale coming up is an interface appearing, and waiting out a 30s
    /// timer after the user just fixed the network is the wrong trade.
    ///
    /// Deliberately narrower than `retryNow()`: only `.waiting` is woken. A
    /// `.failed` phase is a terminal refusal (rejected token, incompatible
    /// protocol) that a path change cannot cure, and `retryNow` restarting it
    /// is a *user* gesture.
    private var pathMonitor: NWPathMonitor?

    #if DEBUG
        /// Test seam: place the connection in a phase directly, so the path
        /// monitor's guard can be tested without arranging a real dial.
        func simulatePhaseForTesting(_ value: Phase) { phase = value }
    #endif

    /// The narrow wake: only a recoverable backoff. Factored out of the
    /// monitor closure so the guard is testable — `.failed` is a terminal
    /// refusal a path change cannot cure, and waking it would retry a
    /// rejected token forever.
    func pathDidChange() {
        guard case .waiting = phase else { return }
        wakeTask?.cancel()
    }

    private func startPathMonitor() {
        pathMonitor?.cancel()
        let monitor = NWPathMonitor()
        // The monitor always reports once on start; that delivery describes
        // the present, not a change, and must not wake anything. The lock is
        // how a Sendable closure is allowed to remember it happened — the
        // handler runs on the monitor's own queue.
        let sawInitial = OSAllocatedUnfairLock(initialState: false)
        monitor.pathUpdateHandler = { [weak self] _ in
            let isChange = sawInitial.withLock { seen -> Bool in
                defer { seen = true }
                return seen
            }
            guard isChange else { return }
            Task { @MainActor [weak self] in self?.pathDidChange() }
        }
        monitor.start(queue: DispatchQueue(label: "codeconnect.pathmonitor"))
        pathMonitor = monitor
    }

    private func sleepInterruptibly(seconds: Double) async {
        let task = Task<Void, Never> { try? await Task.sleep(for: .seconds(seconds)) }
        wakeTask = task
        await task.value
        wakeTask = nil
    }

    /// Returns when the socket closes cleanly; throws on any failure.
    private func runConnection(url: URL, credential: HelloCredential) async throws {
        let configuration = URLSessionConfiguration.ephemeral
        // Bounds how long a dead tailnet host takes to report itself dead; the
        // 10s app-level ping keeps a live link well inside it.
        configuration.timeoutIntervalForRequest = 30
        // Failing fast beats a spinner that waits for connectivity in silence:
        // the UI can only be honest about a link it is told about.
        configuration.waitsForConnectivity = false
        let session = URLSession(configuration: configuration)
        self.session = session

        let socket = session.webSocketTask(with: url)
        socket.maximumMessageSize = 4 * Wire.maxClientMessageBytes
        self.socket = socket
        socket.resume()

        // Closes *these* objects, not whatever is current. A cancelled
        // connection unwinds asynchronously, so its cleanup can land after a
        // replacement has already been installed; without the identity check it
        // would nil out the new socket and fail the new connection's waiters.
        defer { close(socket: socket, session: session) }

        try await send(
            .hello(
                credential: credential,
                clientID: Self.installationID,
                clientName: Self.clientName,
                sshPublicKey: sshPublicKey))

        let pinger = Task { [weak self] in await self?.pingLoop() }
        defer { pinger.cancel() }

        // The daemon closes a client that never says hello (ws_server.rs
        // HANDSHAKE_TIMEOUT); nothing symmetric protects us from a daemon that
        // accepts the socket and then goes silent. Without this the app sits in
        // `.connecting` forever and even foregrounding cannot shift it, because
        // the backoff sleep it would interrupt never started.
        let handshake = Task { [weak self] in
            try? await Task.sleep(for: Self.handshakeTimeout)
            guard let self, !Task.isCancelled, self.socket === socket, !self.phase.isConnected
            else { return }
            self.lastErrorMessage = "The daemon accepted the connection but never answered hello"
            // Cancelling the socket makes the receive below throw, which drops
            // into the normal backoff path.
            socket.cancel(with: .goingAway, reason: nil)
        }
        defer { handshake.cancel() }

        while !Task.isCancelled {
            let message: URLSessionWebSocketTask.Message
            do {
                message = try await socket.receive()
            } catch {
                if Task.isCancelled { throw CancellationError() }
                if Self.isUnresolvableHost(error) { throw ConnectionError.hostUnresolvable }
                throw ConnectionError.transport((error as NSError).localizedDescription)
            }

            let text: String
            switch message {
            case .string(let value):
                text = value
            case .data(let value):
                text = String(decoding: value, as: UTF8.self)
            @unknown default:
                continue
            }

            lastContactAt = Date()
            guard let decoded = try? JSONDecoder().decode(ServerMessage.self, from: Data(text.utf8))
            else {
                // An undecodable frame is a bug worth seeing, never a reason to
                // drop a working connection.
                lastErrorMessage = "Ignored an unreadable frame from the daemon"
                continue
            }

            if case .error(let code, let message) = decoded {
                switch code {
                case "unauthorized":
                    // The same wire code covers a rejected device token and a
                    // rejected pairing code; only this side knows which was sent.
                    if case .pairingCode = credential {
                        throw ConnectionError.rejectedPairingCode
                    }
                    throw ConnectionError.unauthorized
                case "protocol_mismatch":
                    // Terminal, and it used to be neither: it fell through to the
                    // generic reconnect, so an app and a Mac helper that could
                    // never speak to each other retried forever instead of saying
                    // so once. The daemon's own words are kept — this is one of
                    // the few refusals it explains.
                    throw ConnectionError.incompatible(
                        message.isEmpty
                            ? "This Mac helper speaks a protocol this app does not. Update it and try again."
                            : message)
                default:
                    break
                }
            }
            handle(decoded)
        }
        throw CancellationError()
    }

    private func close(socket: URLSessionWebSocketTask?, session: URLSession?) {
        socket?.cancel(with: .goingAway, reason: nil)
        session?.invalidateAndCancel()

        // Only disturb shared state if what we just closed is still the current
        // connection.
        guard socket == nil || self.socket === socket else { return }
        self.socket = nil
        self.session = nil
        if phase.isConnected { phase = .connecting }
        failAllWaiters(with: ConnectionError.notConnected)
    }

    private func closeCurrentSocket() {
        close(socket: socket, session: session)
    }

    private func pingLoop() async {
        while !Task.isCancelled {
            try? await Task.sleep(for: Self.pingInterval)
            if Task.isCancelled { return }
            // A ping that cannot be written is a dead socket; the receive loop
            // will discover the same thing and drive the reconnect.
            try? await send(.ping)
        }
    }

    // MARK: - Sending

    func send(_ message: ClientMessage) async throws {
        guard let socket else { throw ConnectionError.notConnected }
        let data = try JSONEncoder().encode(message)
        guard data.count <= Wire.maxClientMessageBytes else {
            throw ConnectionError.transport("Message too large for the daemon to accept")
        }
        do {
            try await socket.send(.string(String(decoding: data, as: UTF8.self)))
        } catch {
            if Self.isUnresolvableHost(error) { throw ConnectionError.hostUnresolvable }
            throw ConnectionError.transport((error as NSError).localizedDescription)
        }
    }

    /// Fire-and-forget for messages whose failure the caller cannot act on.
    func sendIgnoringFailure(_ message: ClientMessage) {
        Task { [weak self] in try? await self?.send(message) }
    }

    // MARK: - Request/response

    /// Idempotent by `(session, requestID)` on the daemon side, so a retry after
    /// a timeout is always safe and returns the original outcome.
    ///
    /// `session` scopes the answer to the run the card came from. Correlation
    /// stays on `requestID` alone, because that is all `answer_result` carries —
    /// which also means two runs holding one request id cannot have concurrent
    /// answers in flight, and the one-per-key rule below is what enforces it.
    func answer(
        requestID: String, payloadHash: String, decision: AnswerDecision, session: String?
    ) async throws -> AnswerResult {
        #if DEBUG
            if fixtureAnswers {
                return .applied(
                    outcome: AnswerOutcome(
                        requestID: requestID, sessionID: session ?? "fixture", decision: decision,
                        resolvedBy: .phone, appliedVia: .sendKeys,
                        resolvedAt: Date().formatted(
                            Date.ISO8601FormatStyle(includingFractionalSeconds: true)),
                        detail: "fixture", inferred: false))
            }
        #endif
        return try await request(
            key: requestID,
            store: \.answerWaiters,
            send: .answer(
                requestID: requestID, payloadHash: payloadHash, decision: decision,
                session: session))
    }

    /// The reply to each of these echoes back the reference *as sent*
    /// (`ccd/src/ws_server.rs` moves `session_id` straight into the result), so
    /// correlating on the string this app chose is exact whether it is a uid or
    /// a name.
    func sendText(
        session: String, text: String, require: PromptPresence?, submit: Bool
    ) async throws -> SendTextResult {
        try await request(
            key: session,
            store: \.sendTextWaiters,
            send: .sendText(session: session, text: text, require: require, submit: submit))
    }

    /// Ask the Mac to forget one ended run. Keyed by uid, which is also the
    /// waiter key, so two rows swiped at once cannot collect each other's answer.
    func deleteSession(sessionUID: String) async throws -> DeleteSessionResult {
        #if DEBUG
            deleteRequests.append(sessionUID)
            if let deleteStub { return try await deleteStub(sessionUID) }
        #endif
        return try await request(
            key: sessionUID,
            store: \.deleteWaiters,
            send: .deleteSession(sessionUID: sessionUID))
    }

    /// One real push to this device, with the daemon's typed answer.
    func testPush() async throws -> TestPushResult {
        let requestID = "tp-" + UUID().uuidString
        #if DEBUG
            if let testPushStub { return try await testPushStub(requestID) }
        #endif
        return try await request(
            key: requestID,
            store: \.testPushWaiters,
            send: .testPush(requestID: requestID))
    }

        func capture(session: String, lines: UInt32?) async throws -> String {
        try await request(
            key: session,
            store: \.captureWaiters,
            send: .capture(session: session, lines: lines))
    }

    /// On-demand `git diff` for one run. Correlated by the reference like the
    /// other single-keyed replies, so the same one-in-flight-per-key rule holds.
    func diff(session: String) async throws -> SessionDiff {
        try await request(
            key: session,
            store: \.diffWaiters,
            send: .getDiff(session: session))
    }

    private typealias WaiterTable<T> = ReferenceWritableKeyPath<
        DaemonConnection, [String: [Waiter<T>]]
    >

    /// FIFO correlation. `answer_result` carries a `request_id` and
    /// `send_text_result`/`capture_result` carry only a `session_id`, so in both
    /// cases replies for one key arrive in the order the requests were sent.
    private func request<T: Sendable>(
        key: String, store: WaiterTable<T>, send message: ClientMessage
    ) async throws -> T {
        guard phase.isConnected else { throw ConnectionError.notConnected }
        // One live request per key, enforced here rather than trusted to every
        // call site. Replies are correlated by position, so two concurrent
        // requests for one key could otherwise have their results swapped — and
        // a card that reports "Denied" for an answer the daemon applied as
        // "Allow" is the worst lie this app could tell.
        // Sequential re-answers are unaffected, which is what the idempotent
        // duplicate-answer path depends on.
        if self[keyPath: store][key]?.contains(where: { $0.continuation != nil }) == true {
            throw ConnectionError.transport("A request for this is already in flight")
        }
        nextTicket += 1
        let ticket = nextTicket

        return try await withCheckedThrowingContinuation { continuation in
            self[keyPath: store][key, default: []].append(
                Waiter(ticket: ticket, continuation: continuation))
            Task { [weak self] in
                guard let self else { return }
                do {
                    try await self.send(message)
                } catch {
                    // Never sent, so no reply is coming: drop it entirely.
                    self.abandon(
                        store: store, key: key, ticket: ticket, with: error, keepTombstone: false)
                    return
                }
                try? await Task.sleep(for: Self.requestTimeout)
                // Sent but unanswered: a late reply must not be misread as the
                // next request's.
                self.abandon(
                    store: store, key: key, ticket: ticket, with: ConnectionError.timedOut,
                    keepTombstone: true)
            }
        }
    }

    /// Deliver to the oldest outstanding waiter for this key. A tombstone at the
    /// head means this reply belongs to an abandoned request; it is consumed and
    /// discarded so everything behind it stays correctly aligned.
    private func deliver<T: Sendable>(store: WaiterTable<T>, key: String, value: T) {
        guard var queue = self[keyPath: store][key], !queue.isEmpty else { return }
        let waiter = queue.removeFirst()
        self[keyPath: store][key] = queue.isEmpty ? nil : queue
        waiter.continuation?.resume(returning: value)
    }

    /// Give up on one specific in-flight request; a no-op once it has been
    /// answered.
    private func abandon<T: Sendable>(
        store: WaiterTable<T>, key: String, ticket: UInt64, with error: Error, keepTombstone: Bool
    ) {
        guard var queue = self[keyPath: store][key],
            let index = queue.firstIndex(where: { $0.ticket == ticket }),
            let continuation = queue[index].continuation
        else { return }
        if keepTombstone {
            queue[index].continuation = nil
        } else {
            queue.remove(at: index)
        }
        self[keyPath: store][key] = queue.isEmpty ? nil : queue
        continuation.resume(throwing: error)
    }

    /// The socket is gone, so no reply is coming for anything — tombstones
    /// included. Clearing them here is what stops them accumulating forever.
    private func failAllWaiters(with error: Error) {
        let answers = answerWaiters
        answerWaiters = [:]
        for queue in answers.values { for w in queue { w.continuation?.resume(throwing: error) } }

        let texts = sendTextWaiters
        sendTextWaiters = [:]
        for queue in texts.values { for w in queue { w.continuation?.resume(throwing: error) } }

        let captures = captureWaiters
        captureWaiters = [:]
        for queue in captures.values { for w in queue { w.continuation?.resume(throwing: error) } }

        let diffs = diffWaiters
        diffWaiters = [:]
        for queue in diffs.values { for w in queue { w.continuation?.resume(throwing: error) } }

        // Every waiter store belongs in here. One left out does not just leak: it
        // becomes a tombstone after its timeout, and the *next* good answer for
        // that key is handed to the dead waiter and dropped, so the request that
        // is actually in flight fails too. Once per reconnect, forever.
        let deletes = deleteWaiters
        deleteWaiters = [:]
        for queue in deletes.values { for w in queue { w.continuation?.resume(throwing: error) } }

        let tests = testPushWaiters
        testPushWaiters = [:]
        for queue in tests.values { for w in queue { w.continuation?.resume(throwing: error) } }
    }

    // MARK: - Inbound

    private func handle(_ message: ServerMessage) {
        switch message {
        case .helloAck(let ack):
            let caps = ack.capabilities
            helloAck = ack
            capabilities = caps
            serverTime = ack.serverTime
            generation &+= 1
            phase = .connected(since: Date())
            // Only here: a socket that opened is not yet a daemon that
            // answered. The handshake is the first proof the link works —
            // and the end of whatever failure run was accumulating.
            dialFailure = nil
            isRedial = false
            ledger.reset()
            lastErrorMessage =
                ack.protocolVersion == Wire.protocolVersion
                ? nil
                : "Daemon speaks protocol \(ack.protocolVersion); this app speaks \(Wire.protocolVersion)"
            lastErrorAt = lastErrorMessage == nil ? nil : Date()
            // This scheme worked, so it is no longer "the alternate" — it is the
            // answer. Persisting it means the next launch starts on the right
            // one instead of paying for the ladder again.
            schemeAlternate = false
            if endpoint?.useTLS != usingTLS {
                endpoint?.useTLS = usingTLS
                onTransportSettled?(usingTLS)
            }
            // **A pairing code is dead the instant the daemon answers with a
            // token, so this connection stops holding one.**
            //
            // Banking it in the Keychain through `onDeviceToken` was never enough:
            // `endpoint` is what `retryNow()` and the supervise loop re-dial, and
            // it still carried the spent code. Backgrounding the app right after
            // pairing and returning was sufficient to replay it, and the daemon
            // answers `unauthorized`, which is terminal, so a phone that had
            // paired perfectly landed in a permanent failure. Measured: paired at
            // 10:31:45, "pairing code already used" at 10:32:28.
            //
            // Upgrading here rather than at the call site is what makes it
            // structural: this is the one moment the code is provably spent, and
            // afterwards no dial path can reach one, because none exists.
            // `useTLS` above is mutated from the same ack for the same reason.
            if let deviceToken = ack.deviceToken, !deviceToken.isEmpty {
                endpoint?.credential = .token(deviceToken)
                onDeviceToken?(deviceToken)
            }
            onConnected?()
            // The daemon has a certificate but this connection is in the clear.
            // Almost always means it just turned TLS on; a reconnect gets the
            // encrypted transport now rather than at the next cold start. The
            // ladder above puts things right if `wss://` turns out not to work.
            if caps.tls, !usingTLS, endpoint?.hostIsIPLiteral == false {
                lastErrorMessage = "The daemon offers TLS; reconnecting over wss://"
                lastErrorAt = Date()
                endpoint?.useTLS = true
                onTransportSettled?(true)
                socket?.cancel(with: .goingAway, reason: nil)
            }
        case .answerResult(let requestID, let result):
            deliver(store: \.answerWaiters, key: requestID, value: result)
        case .sendTextResult(let sessionID, let result):
            deliver(store: \.sendTextWaiters, key: sessionID, value: result)
        case .captureResult(let sessionID, let text):
            deliver(store: \.captureWaiters, key: sessionID, value: text)
        case .deleteSessionResult(let sessionUID, let result):
            deliver(store: \.deleteWaiters, key: sessionUID, value: result)
        case .testPushResult(let requestID, let result):
            deliver(store: \.testPushWaiters, key: requestID, value: result)
        case .diff(let diff):
            deliver(store: \.diffWaiters, key: diff.sessionID, value: diff)
        case .error(let code, let message):
            lastErrorMessage = "\(message) (\(code))"
            lastErrorAt = Date()
            // The one place a string arrives having been written by the daemon.
            lastDaemonErrorMessage = lastErrorMessage
            lastDaemonErrorAt = lastErrorAt
        case .pong, .sessions, .event, .unknown:
            break
        }
        onMessage?(message)
    }

    /// The best account of a failure available after `since`, whoever wrote it.
    /// Used to explain a request that timed out rather than inventing a reason
    /// for it — but never as a quotation. See `daemonErrorReported(since:)`.
    func errorReported(since: Date) -> String? {
        guard let lastErrorAt, lastErrorAt >= since else { return nil }
        return lastErrorMessage
    }

    /// What the **daemon** said after `since`, verbatim, or nil if it said
    /// nothing. The only string in this class that may be quoted to the reader
    /// as the Mac's own words.
    func daemonErrorReported(since: Date) -> String? {
        guard let lastDaemonErrorAt, lastDaemonErrorAt >= since else { return nil }
        return lastDaemonErrorMessage
    }

    #if DEBUG
        /// Test seam: resolve answers locally instead of over the socket.
        ///
        /// Exists for exactly one reason: the Deck's *advance* behaviour — the
        /// stack moving on, the count ticking down, the fleet-clear state — can
        /// only be exercised when answers resolve, and arranging three agents
        /// blocked at three risk classes on a live Mac on demand is not
        /// something a test can rely on. The real answer path, with a real
        /// daemon and a real ledger, is covered by `ApprovalFlowUITests`; this
        /// seam never runs in a release build and never runs without the
        /// `-CC_FIXTURE` launch argument.
        var fixtureAnswers = false

        /// Test seam: feed a frame through the real inbound path.
        ///
        /// Debug builds only. It is the *decoded* message that is injected, so
        /// everything downstream — ingest, the timeline builder, the fleet
        /// ordering — runs exactly as it does on the wire.
        func injectForTesting(_ message: ServerMessage) {
            lastContactAt = Date()
            handle(message)
        }

        /// Test seam: report a live link without one.
        ///
        /// Needed because link health gates every action, so a fixture run with
        /// an idle connection could only ever demonstrate disabled buttons.
        func simulateConnectedForTesting() {
            phase = .connected(since: Date())
            lastContactAt = Date()
        }

        /// Test seam: answer `deleteSession` locally, and record what was asked.
        ///
        /// **Without this, the removal tests were unfalsifiable.** An `AppModel`
        /// built in a test has no socket, so `deleteSession` throws
        /// `notConnected` and `removeSession`'s `try?` turns that into the same
        /// `nil` its guards produce. Every gate test therefore passed whether or
        /// not the gate existed — measured by deleting the guards, which changed
        /// nothing. The count is what makes the difference observable: a request
        /// that was never sent is a different fact from one that was sent and
        /// refused, and only that distinction tests a gate.
        var deleteStub: ((String) async throws -> DeleteSessionResult)?
        /// Test seam: answer `testPush` locally.
        var testPushStub: ((String) async throws -> TestPushResult)?
        /// Uids passed to `deleteSession`, in order, whether stubbed or not.
        private(set) var deleteRequests: [String] = []

        /// Test seam: claim a set of capabilities without a handshake.
        ///
        /// `DaemonProfile` is derived from the ack, so a test that wants to
        /// exercise a capability gate has to be able to set both sides of it —
        /// otherwise "the daemon cannot delete" is indistinguishable from "there
        /// is no daemon".
        func simulateCapabilitiesForTesting(_ capabilities: Capabilities, minor: UInt32) {
            helloAck = HelloAck(
                protocolVersion: Wire.protocolVersion, protocolMinor: minor,
                serverTime: "", capabilities: capabilities, deviceToken: nil,
                deviceID: nil, deviceName: nil, sshKeyInstalled: nil)
            self.capabilities = capabilities
        }
    #endif


    // MARK: - Identity

    /// Stable per-install id so the daemon's logs can tell two phones apart.
    /// Shared with the SSH key comment, so one device is one identity wherever
    /// it shows up on the Mac.
    private static var installationID: String { DeviceIdentity.installationID }

    private static let clientName = "CodeConnect iPhone"
}
