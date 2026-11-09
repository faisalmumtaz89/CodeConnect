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

    /// Blocked, then allowed again: refused by its token and then without one,
    /// the device keeps its token, so the next try (a return to the app, or
    /// Retry) asks with it, and the computer that restored its pairing lets it
    /// in as it was.
    func testABlockedDeviceAllowedAgainIsBackWithItsOwnToken() async throws {
        let blocked = OSAllocatedUnfairLock(initialState: true)
        let daemon = try StubDaemon { credential in
            credential == "token kept" && !blocked.withLock { $0 }
                ? .admit(token: "kept", machineID: "daemon-1") : .refuse
        }
        let connection = DaemonConnection()
        connection.start(
            endpoint: DaemonEndpoint(host: "127.0.0.1", port: daemon.port, credential: .token("kept"), useTLS: false))
        try await until { if case .failed = connection.phase { true } else { false } }
        blocked.withLock { $0 = false }
        connection.retryNow()
        try await until { connection.phase.isConnected }
        XCTAssertEqual(daemon.hellos, ["token kept", "none", "token kept"])
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
