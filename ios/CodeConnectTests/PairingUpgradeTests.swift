import XCTest

@testable import CodeConnect

/// A pairing code must be unreachable the moment it is spent.
///
/// **The defect this pins.** `DaemonConnection` re-dials `endpoint` from two
/// places: `retryNow()`, which the app calls every time it comes to the
/// foreground, and the supervise loop's own reconnect. After a successful pairing
/// the durable token was banked in the Keychain, but `endpoint` still carried the
/// single-use code, so either path replayed it. The daemon answers `unauthorized`,
/// which is terminal, so a phone that had paired *correctly* landed in a permanent
/// failure with a live device row on the Mac.
///
/// Observed on the owner's device: paired at 10:31:45 (`newly paired device
/// ffa612044496`), then `pairing code already used` at 10:32:28, after
/// backgrounding the app to start a session and returning.
///
/// The rule is not "call something after pairing". It is that a spent credential
/// stops existing, so no present or future dial path has to remember.
@MainActor
final class PairingUpgradeTests: XCTestCase {

    private func helloAck(deviceToken: String?) throws -> ServerMessage {
        let token = deviceToken.map { "\"device_token\":\"\($0)\"," } ?? ""
        return try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                """
                {"type":"hello_ack",\(token)"protocol_version":1,"protocol_minor":6,
                 "capabilities":{"answer_path":"hook_return","tls":false}}
                """.utf8))
    }

    func testARedeemedPairingCodeIsNoLongerHeld() throws {
        let connection = DaemonConnection()
        connection.start(
            endpoint: DaemonEndpoint(
                host: "mac.example.ts.net", port: 8787,
                credential: .pairingCode("ABCD2345"), useTLS: false))
        defer { connection.stop() }

        XCTAssertTrue(
            connection.holdsPairingCode,
            "the exchange has not happened yet, so the code is still the credential")

        connection.injectForTesting(try helloAck(deviceToken: "a-durable-device-token"))

        XCTAssertFalse(
            connection.holdsPairingCode,
            """
            The daemon has answered with a durable token, so the code is spent. \
            Holding it means the next foreground or reconnect replays it and is \
            refused for good.
            """)
    }

    /// The other half: an ack without a token must change nothing. A daemon
    /// re-acknowledging a durable connection does not hand out a new token, and
    /// treating that as an upgrade would let a missing field silently blank a
    /// working credential.
    func testAnAckWithoutATokenLeavesTheCredentialAlone() throws {
        let connection = DaemonConnection()
        connection.start(
            endpoint: DaemonEndpoint(
                host: "mac.example.ts.net", port: 8787,
                credential: .pairingCode("ABCD2345"), useTLS: false))
        defer { connection.stop() }

        connection.injectForTesting(try helloAck(deviceToken: nil))

        XCTAssertTrue(
            connection.holdsPairingCode,
            "nothing was exchanged, so nothing should have been upgraded")
    }
}
