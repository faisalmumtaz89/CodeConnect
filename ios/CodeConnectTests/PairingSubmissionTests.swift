import XCTest

@testable import CodeConnectKit

/// The typed-in pairing form, as the iPhone and the Mac both submit it: the words the user reads
/// when the form is not ready or not right, and what a ready form pairs with.
final class PairingSubmissionTests: XCTestCase {
    private let token = "8ef368f146c2c387c55310a00cab548a52717a20a40e85ca3f6a515fb70eff39"

    func testAnEmptyAddressBlocksTheFormFirst() {
        XCTAssertEqual(
            PairingCredentialInput.blockedReason(address: "  ", code: ""),
            "Enter the address shown with the code.")
    }

    func testAnEmptyCodeBlocksTheForm() {
        XCTAssertEqual(
            PairingCredentialInput.blockedReason(address: "100.64.0.1", code: " "),
            "Enter the code your computer shows.")
        XCTAssertNil(PairingCredentialInput.blockedReason(address: "100.64.0.1", code: "abc"))
    }

    func testACodeOfNeitherShapeIsRefused() {
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "100.64.0.1", code: "abc"),
            .refused("That isn't a pairing code: it's eight letters and digits."))
    }

    func testAnAddressThatDoesNotParseIsRefused() {
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "ws://", code: "35AG-AE55"),
            .refused("That address isn't valid. Enter it exactly as your computer shows it."))
    }

    func testAReadyFormPairsByCodeOrConnectsByToken() {
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "100.64.0.1", code: "35ag-ae55"),
            .pairingCode(PairingQRPayload(host: "100.64.0.1", port: DaemonEndpoint.defaultPort, code: "35AGAE55")))
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "ws://box:9000", code: token),
            .token(DaemonEndpoint(host: "box", port: 9000, credential: .token(token), useTLS: false)))
    }

    /// A paired computer's address is shown as its host alone, on its own screen
    /// and in Link health: the scheme and the port are how the phone dials it.
    func testAPairedAddressShowsTheHostAlone() {
        let plain = DaemonEndpoint(host: "100.64.0.2", port: 8787, credential: .token(token), useTLS: false)
        let encrypted = DaemonEndpoint(
            host: "studio.tail1234.ts.net", port: 8788, credential: .token(token), useTLS: true)
        XCTAssertEqual(plain.displayAddress, "100.64.0.2")
        XCTAssertEqual(encrypted.displayAddress, "studio.tail1234.ts.net")
    }
}
