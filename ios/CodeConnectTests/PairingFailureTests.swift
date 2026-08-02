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
