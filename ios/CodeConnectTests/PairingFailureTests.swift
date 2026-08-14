import XCTest

@testable import CodeConnect

/// A pairing attempt that cannot succeed has to say so.
///
/// **The defect this covers was the real new-user experience.** Only
/// `unauthorized` ever reached `.failed`; every other failure became `.waiting`,
/// and `AppModel.pairingError` reads `.failed` alone — so the pairing screen showed
/// a progress ring and "Exchanging the code for a device token…" indefinitely,
/// while the single-use code expired underneath it after five minutes. No error was
/// ever shown, at any point.
///
/// Driven against a closed local port, so the refusal is immediate and the test
/// measures the policy rather than a network. Loopback is legitimate here: the QR
/// decoder refuses loopback *input* (`PairingReachabilityTests`), while the
/// connection layer does not second-guess an endpoint it was handed.
@MainActor
final class PairingFailureTests: XCTestCase {

    /// Port 1: nothing listens, and the kernel refuses immediately.
    private func unreachable(credential: HelloCredential) -> DaemonEndpoint {
        DaemonEndpoint(host: "127.0.0.1", port: 1, credential: credential, useTLS: false)
    }

    private func waitForFailure(_ connection: DaemonConnection, timeout: TimeInterval) async -> Bool
    {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if case .failed = connection.phase { return true }
            try? await Task.sleep(for: .milliseconds(100))
        }
        return false
    }

    func testAPairingCodeStopsInsteadOfSpinningForever() async {
        let connection = DaemonConnection()
        connection.start(endpoint: unreachable(credential: .pairingCode("ABCD2345")))
        defer { connection.stop() }

        let failed = await waitForFailure(connection, timeout: 30)
        XCTAssertTrue(
            failed,
            """
            A pairing code that cannot be delivered must reach a terminal failure. \
            Staying in `.waiting` is what left the pairing screen spinning on a code \
            that had already expired.
            """)
    }

    /// A token typed at the pairing screen is still a pairing: nothing is saved
    /// until the daemon's `hello_ack` proves the credential, and a human is
    /// watching for a verdict. An address that refuses every dial must become a
    /// visible failure, exactly as an undeliverable pairing code does.
    func testATypedTokenAtPairingStopsInsteadOfSpinningForever() async {
        let connection = DaemonConnection()
        connection.start(
            endpoint: unreachable(credential: .token("device-token")), forPairing: true)
        defer { connection.stop() }

        let failed = await waitForFailure(connection, timeout: 30)
        XCTAssertTrue(
            failed,
            """
            A typed token that no daemon answers must reach a terminal failure while \
            the person who typed it is still looking at the pairing screen. Staying \
            in `.waiting` shows a progress ring forever and no error, ever.
            """)
    }

    /// The cap must not outlive the exchange it bounds. A dial that began as a
    /// pairing becomes a paired link the moment the daemon acknowledges it, and
    /// from then on its reconnects deserve the durable token's endless patience —
    /// otherwise a Mac that sleeps four backoffs after onboarding shows the new
    /// user a dead, mislabeled link.
    func testAPairedLinkKeepsRetryingAfterItsPairingDialSucceeded() async throws {
        let connection = DaemonConnection()
        connection.start(endpoint: unreachable(credential: .pairingCode("ABCD2345")))
        defer { connection.stop() }

        let ack = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                """
                {"type":"hello_ack","device_token":"a-durable-device-token",
                 "protocol_version":1,"protocol_minor":6,
                 "capabilities":{"answer_path":"hook_return","tls":false}}
                """.utf8))
        connection.injectForTesting(ack)

        let failed = await waitForFailure(connection, timeout: 12)
        XCTAssertFalse(
            failed,
            """
            The daemon answered this dial, so the pairing has its verdict and the \
            link is a saved pairing now. Reaching `.failed` after a handful of \
            dropped redials means the pairing cap outlived the pairing.
            """)
    }

    /// Re-pairing is what people do when the link is already broken, so the
    /// pairing screen's bounded verdict must survive meeting a dial that is
    /// already mid-retry on the same endpoint. Absorbing the pairing into the
    /// patient saved-profile dial shows the spinner this whole class exists to
    /// forbid.
    func testRepairingAnEndpointAlreadyRetryingGetsAVerdict() async {
        let connection = DaemonConnection()
        let endpoint = unreachable(credential: .token("device-token"))
        connection.start(endpoint: endpoint)
        defer { connection.stop() }

        try? await Task.sleep(for: .milliseconds(600))
        connection.start(endpoint: endpoint, forPairing: true)

        let failed = await waitForFailure(connection, timeout: 30)
        XCTAssertTrue(
            failed,
            """
            Typing the same address and token at the pairing screen while the saved \
            dial is struggling must still end in a verdict. Deduplicating the start \
            keeps the unbounded dial and the pairing screen spins forever.
            """)
    }

    /// The other half of the rule, and the reason this is not simply "give up
    /// sooner". A device token is durable: the Mac may be asleep or off the
    /// tailnet, and backing off until that changes is exactly right. Making *this*
    /// terminal would strand a paired phone that was only ever briefly out of
    /// range.
    func testADurableTokenKeepsRetrying() async {
        let connection = DaemonConnection()
        connection.start(endpoint: unreachable(credential: .token("device-token")))
        defer { connection.stop() }

        let failed = await waitForFailure(connection, timeout: 12)
        XCTAssertFalse(
            failed,
            "a saved pairing must keep retrying rather than give up while the Mac is away")
    }
}
