import XCTest

@testable import CodeConnect

/// Which computer the phone is paired with (protocol minor 25).
///
/// `minor-25-wire.json` is captured, not written: the `hello_ack` a minor-25
/// `ccd` sent from a sandbox home on a Mac, and the one a minor-25
/// `demo-daemon` sent, each read off its socket. One value is not as captured:
/// the Mac's computer name, replaced by a stand-in of the same macOS shape
/// (`<name>’s <model>`), because a public repository does not carry its owner's.
/// The older daemon is the captured minor-23 ack in `minor-23-wire.json`, which
/// predates both fields.
@MainActor
final class MachineNameTests: XCTestCase {

    private func ack(_ resource: String, _ name: String) throws -> ServerMessage {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: resource, withExtension: "json"),
            "\(resource).json is not in the test bundle")
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        return try XCTUnwrap(wire[name]?.decoded(ServerMessage.self), "\(name) decodes")
    }

    private func pairedModel() -> AppModel {
        let model = AppModel(
            machineID: "automation", pairing: PairingStore(ephemeral: true),
            cache: EventCache(
                root: URL(fileURLWithPath: NSTemporaryDirectory())
                    .appendingPathComponent(UUID().uuidString)),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
        model.pairing.saveEphemeral(
            DaemonEndpoint(
                host: "studio.tail1234.ts.net", port: 8787, credential: .token("t"),
                useTLS: false))
        return model
    }

    func testAMacNamesItselfAndItsSystem() throws {
        let model = pairedModel()
        model.connection.injectForTesting(try ack("minor-25-wire", "ccd_hello_ack"))

        XCTAssertEqual(model.machineName, "Jordan’s Mac Studio")
        XCTAssertEqual(model.daemonProfile.machineSystem, "macOS")
    }

    func testTheDemoNamesItselfAndClaimsNoSystem() throws {
        let model = pairedModel()
        model.connection.injectForTesting(try ack("minor-25-wire", "demo_hello_ack"))

        XCTAssertEqual(model.machineName, "Review demo")
        XCTAssertNil(model.daemonProfile.machineSystem)
    }

    func testAnOlderDaemonIsNamedByTheHostItPairedWith() throws {
        let model = pairedModel()
        model.connection.injectForTesting(try ack("minor-23-wire", "hello_ack"))

        XCTAssertEqual(model.machineName, "studio.tail1234.ts.net")
        XCTAssertNil(model.daemonProfile.machineSystem)
    }

    func testASystemThisBuildDoesNotKnowIsNotShown() {
        let profile = DaemonProfile(
            protocolVersion: 1, protocolMinor: 25, capabilities: Capabilities(),
            machineOS: "freebsd")
        XCTAssertNil(profile.machineSystem)
        XCTAssertEqual(
            DaemonProfile(
                protocolVersion: 1, protocolMinor: 25, capabilities: Capabilities(),
                machineOS: "linux"
            ).machineSystem, "Linux")
    }
}
