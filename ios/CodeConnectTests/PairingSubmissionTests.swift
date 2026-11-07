import XCTest

@testable import CodeConnectKit

/// The typed-in pairing form, as the iPhone and the Mac both submit it: the words the user reads
/// when the form is not ready or not right, and what a ready form pairs with.
final class PairingSubmissionTests: XCTestCase {
    private let token = "8ef368f146c2c387c55310a00cab548a52717a20a40e85ca3f6a515fb70eff39"

    func testAnEmptyAddressBlocksTheFormFirst() {
        XCTAssertEqual(
            PairingCredentialInput.blockedReason(address: "  ", code: ""),
            "An address is needed - the one `codeconnect pair` printed.")
    }

    func testAnEmptyCodeBlocksTheForm() {
        XCTAssertEqual(
            PairingCredentialInput.blockedReason(address: "100.64.0.1", code: " "),
            "A pairing code or a `codeconnect token` is needed.")
        XCTAssertNil(PairingCredentialInput.blockedReason(address: "100.64.0.1", code: "abc"))
    }

    func testACodeOfNeitherShapeIsRefused() {
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "100.64.0.1", code: "abc"),
            .refused("That is neither an eight-character pairing code nor a `codeconnect token`."))
    }

    func testAnAddressThatDoesNotParseIsRefused() {
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "ws://", code: "35AG-AE55"),
            .refused("That address could not be parsed. Try 100.x.y.z or ws://host:8787."))
    }

    func testAReadyFormPairsByCodeOrConnectsByToken() {
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "100.64.0.1", code: "35ag-ae55"),
            .pairingCode(PairingQRPayload(host: "100.64.0.1", port: DaemonEndpoint.defaultPort, code: "35AGAE55")))
        XCTAssertEqual(
            PairingCredentialInput.submission(address: "ws://box:9000", code: token),
            .token(DaemonEndpoint(host: "box", port: 9000, credential: .token(token), useTLS: false)))
    }
}
