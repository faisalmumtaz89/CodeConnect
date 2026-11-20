import Network
import os
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Joining without a code, against a daemon on this device's loopback** that
/// answers each hello from a script and keeps what each one carried. The daemon's
/// own admission is tested in ccd; this is the app's half: what it sends, and
/// what it does with each answer.
@MainActor
final class PeerJoinTests: XCTestCase {

    /// A saved token the computer no longer takes is asked once more with no
    /// credential, and a second refusal is the end: the reader is asked for the
    /// code, and neither credential is sent again.
    func testARefusedTokenIsAskedOnceMoreWithNoneThenStops() async throws {
        let daemon = try StubDaemon { _ in .refuse }
        let connection = DaemonConnection()
        connection.start(
            endpoint: DaemonEndpoint(host: "127.0.0.1", port: daemon.port, credential: .token("dead"), useTLS: false))
        try await until { if case .failed = connection.phase { true } else { false } }
        XCTAssertEqual(daemon.hellos, ["token dead", "none"])
        XCTAssertEqual(connection.phase, .failed(reason: ConnectionError.notAdmitted.localizedDescription))
        connection.stop()
    }

    /// Blocked, then allowed again (T5-F1): the computer answers the held token with
    /// "revoked", not "unauthorized", so the link reads blocked and keeps dialling with
    /// that same token — it never asks for a code, never sends "none", and never reaches
    /// the terminal refused state. The instant the pairing is restored the next dial is
    /// admitted, as the same device, with no new code.
    func testABlockedDeviceAllowedAgainIsBackWithItsOwnToken() async throws {
        let blocked = OSAllocatedUnfairLock(initialState: true)
        let daemon = try StubDaemon { credential in
            guard credential == "token kept" else { return .refuse }
            return blocked.withLock { $0 } ? .revoked : .admit(token: "kept", machineID: "daemon-1")
        }
        let connection = DaemonConnection()
        connection.start(
            endpoint: DaemonEndpoint(host: "127.0.0.1", port: daemon.port, credential: .token("kept"), useTLS: false))
        // A block is retryable, not terminal: the link reads blocked and never fails.
        try await until { connection.dialFailure == .blocked }
        XCTAssertNotEqual(connection.phase, .failed(reason: ConnectionError.revoked.localizedDescription))
        // Allowed again: the loop reconnects on its own, on the token it already held.
        blocked.withLock { $0 = false }
        try await until { connection.phase.isConnected }
        XCTAssertTrue(daemon.hellos.allSatisfy { $0 == "token kept" }, "no code, no none: \(daemon.hellos)")
        connection.stop()
    }

    /// A typed token (a pairing dial) for a blocked pairing is not a shape a real person
    /// produces, but if it happens it must not promise a reconnect it cannot make: a pairing
    /// dial takes the plain refused-token path — terminal, with the refused-token words — not
    /// the retryable blocked state a saved-token dial gets.
    func testATypedTokenForABlockedPairingTakesTheRefusedTokenPath() async throws {
        let daemon = try StubDaemon { _ in .revoked }
        let connection = DaemonConnection()
        connection.start(
            endpoint: DaemonEndpoint(
                host: "127.0.0.1", port: daemon.port, credential: .token("blocked"), useTLS: false),
            forPairing: true)
        try await until { if case .failed = connection.phase { true } else { false } }
        XCTAssertEqual(connection.phase, .failed(reason: ConnectionError.unauthorized.localizedDescription))
        XCTAssertNotEqual(
            connection.dialFailure, .blocked, "a pairing dial does not enter the retryable blocked state")
        connection.stop()
    }

    /// A token the computer no longer knows: the computer lets the device in by
    /// its Tailscale identity, and the token it answers with replaces the dead one.
    func testAnUnknownTokenIsReplacedThroughTailscale() async throws {
        let daemon = try StubDaemon { $0 == "none" ? .admit(token: "fresh", machineID: "daemon-1") : .refuse }
        let pairing = PairingStore(ephemeral: true)
        pairing.save(
            DaemonEndpoint(host: "127.0.0.1", port: daemon.port, credential: .token("dead"), useTLS: false), id: "studio")
        let machines = Machines(pairing: pairing, cache: EventCache(root: temporaryCache()))
        machines.bootstrap()
        try await until { pairing.machines.first?.endpoint.token == "fresh" }
        XCTAssertEqual(daemon.hellos, ["token dead", "none"])
        XCTAssertEqual(pairing.machines.map(\.id), ["studio"])
        for model in machines.models { model.leave() }
    }

    /// A removed computer listed under an address it was not removed by leaves
    /// as soon as its daemon says which it is, and that address is kept out too.
    func testARemovedComputerUnderANewAddressLeavesAtItsHello() async throws {
        let daemon = try StubDaemon { _ in .admit(token: "t", machineID: "daemon-1") }
        let pairing = PairingStore(ephemeral: true)
        pairing.decline(daemonID: "daemon-1", host: "studio.tail0000.ts.net")
        let machines = Machines(pairing: pairing, cache: EventCache(root: temporaryCache()))
        machines.join(directory(port: daemon.port))
        try await until { pairing.isDeclined(host: "127.0.0.1") }
        XCTAssertEqual(pairing.machines, [])
        XCTAssertFalse(machines.models.contains { $0.machineID != nil })
        XCTAssertEqual(pairing.declined.count, 1)
        // Out of the list, as removed, and offered where a computer is added; adding it there joins it, no code.
        XCTAssertEqual(machines.unjoinedComputers, [])
        let removed = try XCTUnwrap(machines.removedComputers.first)
        XCTAssertEqual(machines.removedComputers.count, 1)
        machines.addAgain(removed)
        try await until { pairing.machines.first?.endpoint.token == "t" }
        XCTAssertEqual(pairing.declined, [])
        XCTAssertEqual(machines.removedComputers, [])
        for model in machines.models { model.leave() }
    }

    /// A listed computer whose daemon refuses this device is shown with the daemon's reason, and the next list
    /// naming it asks again, rather than leaving the refused join to sit unseen for the life of the app.
    func testARefusedJoinIsShownWithItsReasonAndAskedAgainOnTheNextList() async throws {
        let daemon = try StubDaemon { _ in .refuse }
        let pairing = PairingStore(ephemeral: true)
        let machines = Machines(pairing: pairing, cache: EventCache(root: temporaryCache()))
        machines.join(directory(port: daemon.port))
        try await until { !machines.unjoinedComputers.isEmpty }
        let refused = try XCTUnwrap(machines.unjoinedComputers.first)
        XCTAssertEqual(refused.reason, "Won’t let this iPhone in without a pairing code")
        XCTAssertEqual(machines.removedComputers, [], "refused is not removed")
        let asked = daemon.hellos.count
        machines.join(directory(port: daemon.port))
        try await until { daemon.hellos.count > asked }
        for (_, model) in machines.joining { model.leave() }
    }

    /// A listed computer that has never answered is no evidence of a CodeConnect computer (Tailscale lists every Mac
    /// and Linux node), so it shows in no row while it is dialled. Must-fail: show a waiting join as unjoined.
    func testAListedComputerThatNeverAnsweredShowsNoRow() async throws {
        let daemon = try StubDaemon(dropping: 1_000) { _ in .refuse }
        let pairing = PairingStore(ephemeral: true)
        let machines = Machines(pairing: pairing, cache: EventCache(root: temporaryCache()))
        machines.join(directory(port: daemon.port))
        try await until {
            machines.joining.contains { if case .waiting = $0.model.connection.phase { true } else { false } }
        }
        XCTAssertEqual(machines.unjoinedComputers, [])
        for (_, model) in machines.joining { model.leave() }
    }

    /// A computer listed before its daemon answers joins once it does, with no
    /// new list: it is dialled with a saved link's patience, not a pairing's.
    func testAListedComputerJoinsOnceItsDaemonAnswers() async throws {
        let daemon = try StubDaemon(dropping: DaemonConnection.pairingAttemptLimit + 1) { _ in
            .admit(token: "t", machineID: "daemon-2")
        }
        let pairing = PairingStore(ephemeral: true)
        let machines = Machines(pairing: pairing, cache: EventCache(root: temporaryCache()))
        machines.join(directory(port: daemon.port))
        try await until(seconds: 60) { pairing.machines.first?.endpoint.token == "t" }
        XCTAssertEqual(machines.joining.count, 0)
        for model in machines.models { model.leave() }
    }

    private func directory(port: Int) -> PeerDirectory {
        PeerDirectory(
            tailnet: "running", login: "owner@example.com", tagged: false,
            computers: [PeerComputer(name: "studio", host: "127.0.0.1", port: port)])
    }

    private func until(seconds: Double = 15, _ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(seconds)
        while !condition() {
            guard Date() < deadline else { return XCTFail("not within \(seconds) s") }
            try await Task.sleep(for: .milliseconds(50))
        }
    }

    private func temporaryCache() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(
            "join-\(UUID().uuidString)", isDirectory: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}

/// A daemon on loopback: each connection's hello is answered from `answer`, by
/// what credential it carried — `token <t>`, `code <c>` or `none`. The first
/// `dropping` connections are closed before anything is read, as a computer
/// whose daemon is not yet running would.
private final class StubDaemon: @unchecked Sendable {
    enum Answer {
        case refuse
        /// The handshake refused a token whose pairing the computer has blocked: the
        /// daemon's "revoked" answer, distinct from "unauthorized".
        case revoked
        case admit(token: String, machineID: String)
    }

    private(set) var port = 0
    private let listener: NWListener
    private let queue = DispatchQueue(label: "stub-daemon")
    private let answer: @Sendable (String) -> Answer
    private var toDrop: Int
    private var received: [String] = []
    private let lock = NSLock()

    var hellos: [String] { lock.withLock { received } }

    init(dropping: Int = 0, answer: @escaping @Sendable (String) -> Answer) throws {
        self.answer = answer
        toDrop = dropping
        let parameters = NWParameters.tcp
        parameters.defaultProtocolStack.applicationProtocols.insert(NWProtocolWebSocket.Options(), at: 0)
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
        listener = try NWListener(using: parameters)
        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { if case .ready = $0 { ready.signal() } }
        listener.newConnectionHandler = { [unowned self] connection in self.accept(connection) }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 5) == .success, let port = listener.port else {
            throw CancellationError()
        }
        self.port = Int(port.rawValue)
    }

    deinit { listener.cancel() }

    private func accept(_ connection: NWConnection) {
        connection.start(queue: queue)
        let drop = lock.withLock {
            defer { toDrop = max(0, toDrop - 1) }
            return toDrop > 0
        }
        if drop { return connection.cancel() }
        receive(on: connection)
    }

    private func receive(on connection: NWConnection) {
        connection.receiveMessage { [weak self] data, _, _, error in
            guard let self, error == nil, let data else { return connection.cancel() }
            if let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                frame["type"] as? String == "hello"
            {
                let credential =
                    (frame["token"] as? String).map { "token \($0)" }
                    ?? (frame["pairing_code"] as? String).map { "code \($0)" } ?? "none"
                lock.withLock { self.received.append(credential) }
                switch answer(credential) {
                case .refuse:
                    send(["type": "error", "code": "unauthorized", "message": "refused"], on: connection) {
                        connection.cancel()
                    }
                    return
                case .revoked:
                    send(
                        ["type": "error", "code": "revoked", "message": "this device has been revoked"],
                        on: connection
                    ) {
                        connection.cancel()
                    }
                    return
                case .admit(let token, let machineID):
                    send(
                        [
                            "type": "hello_ack", "protocol_version": Int(Wire.protocolVersion),
                            "device_token": token, "machine_id": machineID,
                        ], on: connection) {}
                }
            }
            receive(on: connection)
        }
    }

    private func send(_ frame: [String: Any], on connection: NWConnection, then: @escaping () -> Void) {
        let metadata = NWProtocolWebSocket.Metadata(opcode: .text)
        connection.send(
            content: try? JSONSerialization.data(withJSONObject: frame),
            contentContext: NWConnection.ContentContext(identifier: "frame", metadata: [metadata]),
            isComplete: true, completion: .contentProcessed { _ in then() })
    }
}
