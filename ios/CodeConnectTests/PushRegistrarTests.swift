import UserNotifications
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// Each paired computer's model gets a push registrar of its own from the
/// app's factory, asks it once, and hears only its own answers: the
/// permission it reports and the token it delivers go to that computer.
@MainActor
final class PushRegistrarTests: XCTestCase {

    private final class RecordingRegistrar: PushRegistrar {
        var onToken: ((String, String) -> Void)?
        var onAuthorization: ((Bool) -> Void)?
        private(set) var requests = 0
        func requestAndRegister() { requests += 1 }
        func registerForRemoteNotifications() {}
        func authorizationStatus() async -> UNAuthorizationStatus { .notDetermined }
    }

    private func pushCapabilities() -> Capabilities {
        let json = """
            {"can_approve_reliably":true,"fail_mode":"fail_open",\
            "answer_path":"send_keys","hold_secs":0,"send_text":true,\
            "capture":true,"push":true,"tls":false}
            """
        // swiftlint:disable:next force_try
        return try! JSONDecoder().decode(Capabilities.self, from: Data(json.utf8))
    }

    /// Two computers paired, in memory, each past an eligible handshake: a
    /// daemon that advertises push and a device row for this phone.
    private func twoMachines(makePushRegistrar: (@MainActor () -> any PushRegistrar)?) -> Machines {
        let pairing = PairingStore(ephemeral: true)
        for host in ["studio.tail1234.ts.net", "laptop.tail1234.ts.net"] {
            pairing.save(DaemonEndpoint(host: host, port: 8787, credential: .token(host), useTLS: false))
        }
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("push-registrar-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let machines = Machines(
            pairing: pairing, cache: EventCache(root: root),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!),
            makePushRegistrar: makePushRegistrar)
        for (index, model) in machines.models.enumerated() {
            model.connection.simulateConnectedForTesting()
            model.connection.simulateCapabilitiesForTesting(
                pushCapabilities(), minor: 7, deviceID: "device-\(index)")
        }
        return machines
    }

    private func drainDeliveries() async {
        for _ in 0..<4 { await Task.yield() }
    }

    func testEachComputerHasItsOwnRegistrarAndHearsOnlyItsOwnAnswers() async {
        var made: [RecordingRegistrar] = []
        let machines = twoMachines(makePushRegistrar: {
            let registrar = RecordingRegistrar()
            made.append(registrar)
            return registrar
        })
        let models = machines.models
        XCTAssertEqual(models.count, 2)
        XCTAssertEqual(made.count, 2, "one registrar per computer")
        guard models.count == 2, made.count == 2 else { return }
        XCTAssertFalse(made[0] === made[1], "two computers never share a registrar")

        var attempts: [[String]] = [[], []]
        for (index, model) in models.enumerated() {
            model.onPushDeliveryAttempted = { attempts[index].append($0) }
            model.enablePush()  // first handshake
            model.enablePush()  // reconnect
        }
        XCTAssertEqual(made.map(\.requests), [1, 1], "each computer asks once")

        made[0].onAuthorization?(true)
        XCTAssertEqual(models[0].pushAuthorized, true)
        XCTAssertNil(models[1].pushAuthorized, "one computer's answer is not another's")
        made[1].onAuthorization?(false)
        XCTAssertEqual(models[1].pushAuthorized, false)

        made[0].onToken?("token-studio", "production")
        await drainDeliveries()
        XCTAssertEqual(attempts, [["token-studio"], []], "a token reaches its own computer only")
        made[1].onToken?("token-laptop", "production")
        await drainDeliveries()
        XCTAssertEqual(attempts, [["token-studio"], ["token-laptop"]])
    }

    /// With no factory, no model registers for push, and the registration hook
    /// still fires, as `UpdateCompatibilityTests` counts.
    func testWithoutAFactoryAModelAsksNothingAndStillWorks() async {
        let machines = twoMachines(makePushRegistrar: nil)
        for model in machines.models {
            var requests = 0
            model.onPushRegistrationRequested = { requests += 1 }
            model.enablePush()
            XCTAssertEqual(requests, 1)
            let status = await model.pushAuthorizationStatus()
            XCTAssertEqual(status, .notDetermined)
            XCTAssertNil(model.pushAuthorized)
        }
    }
}
