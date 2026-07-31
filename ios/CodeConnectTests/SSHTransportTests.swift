import Network
import XCTest

@testable import CodeConnect

/// The SSH transport's local half — everything that can be proven without a
/// server to talk to.
///
/// The reachability probe is tested against real sockets, because its whole job
/// is to tell "nothing is listening" apart from "authentication failed", and a
/// mocked socket cannot demonstrate that distinction.
final class SSHTransportTests: XCTestCase {

    /// A listener that accepts and immediately drops. Enough to make a TCP
    /// connect succeed, which is all the probe asks.
    private func startListener() throws -> (listener: NWListener, port: Int) {
        let listener = try NWListener(using: .tcp, on: .any)
        listener.newConnectionHandler = { connection in
            connection.start(queue: .global())
            connection.cancel()
        }
        let ready = expectation(description: "listener ready")
        listener.stateUpdateHandler = { state in
            if case .ready = state { ready.fulfill() }
        }
        listener.start(queue: .global())
        wait(for: [ready], timeout: 5)
        guard let port = listener.port?.rawValue else {
            throw XCTSkip("the listener did not report a port")
        }
        return (listener, Int(port))
    }

    func testProbeFindsAListeningPort() async throws {
        let (listener, port) = try startListener()
        defer { listener.cancel() }
        let outcome = await SSHReachability.probe(host: "127.0.0.1", port: port)
        XCTAssertTrue(outcome.isReachable, "got \(outcome)")
        XCTAssertNil(
            SSHSetupGuidance.forOutcome(outcome, host: "127.0.0.1", port: port),
            "a reachable server needs no setup card")
    }

    /// The case that actually matters on this machine: SSH is switched off.
    func testProbeReportsAClosedPortAndOffersSetup() async throws {
        // A port that was just released is reliably closed and reliably local.
        let (listener, port) = try startListener()
        listener.cancel()
        try await Task.sleep(for: .milliseconds(200))

        let outcome = await SSHReachability.probe(host: "127.0.0.1", port: port, timeout: .seconds(3))
        XCTAssertFalse(outcome.isReachable, "got \(outcome)")

        let guidance = try XCTUnwrap(
            SSHSetupGuidance.forOutcome(outcome, host: "127.0.0.1", port: port))
        XCTAssertTrue(guidance.steps.contains { $0.command == "tailscale up --ssh" })
        XCTAssertTrue(guidance.steps.contains { $0.command == "cc pair --ssh" })
        XCTAssertTrue(
            guidance.steps.contains { $0.title.contains("Remote Login") },
            "the macOS path is offered too, and neither is performed by the app")
        XCTAssertTrue(
            guidance.steps.allSatisfy { ($0.command ?? "").isEmpty || !$0.body.isEmpty },
            "every command comes with an explanation of what it does")
    }

    func testProbeRefusesAnImpossibleAddress() async {
        let outcome = await SSHReachability.probe(host: "", port: 22)
        if case .failed = outcome {} else { XCTFail("expected a failure, got \(outcome)") }
    }

    /// Timing out must not be reported as reachable — the whole ladder depends
    /// on this being conservative.
    func testProbeTimesOutOnABlackHole() async {
        // 198.51.100.0/24 is TEST-NET-2: guaranteed not to be routed anywhere.
        let outcome = await SSHReachability.probe(
            host: "198.51.100.7", port: 22, timeout: .milliseconds(600))
        XCTAssertFalse(outcome.isReachable, "got \(outcome)")
        XCTAssertNotNil(SSHSetupGuidance.forOutcome(outcome, host: "198.51.100.7", port: 22))
    }

    // MARK: Host key pinning

    /// Trust-on-first-use, and a changed key is a mismatch rather than a
    /// silent overwrite.
    @MainActor
    func testHostKeyPinningRoundTrip() {
        let host = "pin-test-\(UUID().uuidString.prefix(8))"
        defer { KnownHostKeys.forget(host: host, port: 22) }

        XCTAssertNil(KnownHostKeys.pin(host: host, port: 22), "nothing is trusted to begin with")
        KnownHostKeys.remember(fingerprint: "SHA256:aaa", host: host, port: 22)
        XCTAssertEqual(KnownHostKeys.pin(host: host, port: 22)?.fingerprint, "SHA256:aaa")

        // Same name, different port, is a different server.
        XCTAssertNil(KnownHostKeys.pin(host: host, port: 2222))

        KnownHostKeys.forget(host: host, port: 22)
        XCTAssertNil(KnownHostKeys.pin(host: host, port: 22), "forgetting a pin really forgets it")
    }
}
