import Security
import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// A phone upgraded from an earlier build keeps its paired computers. The item is written straight
/// through Security, under the app's Keychain service and a test-prefixed account, with the bytes an
/// earlier build stored for two computers, and read back through `PairingStore`.
@MainActor
final class PairingKeychainTests: XCTestCase {
    /// What the build before the connection code moved into CodeConnectKit stored, byte for byte.
    private let storedByEarlierBuild = #"[{"endpoint":{"token":"studio-upgrade-token","host":"studio.upgrade.ts.net","useTLS":false,"port":8787},"id":"upgrade-studio"},{"endpoint":{"host":"laptop.upgrade.ts.net","port":8788,"useTLS":true,"token":"laptop-upgrade-token"},"id":"upgrade-laptop"}]"#

    func testComputersAnEarlierBuildPairedAreStillListed() {
        let prefix = "upgrade-\(UUID().uuidString)-"
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "com.codeconnect.pairing",
            kSecAttrAccount as String: prefix + "machines",
        ]
        addTeardownBlock { SecItemDelete(item as CFDictionary) }
        let added = SecItemAdd(
            item.merging([kSecValueData as String: Data(storedByEarlierBuild.utf8)]) { _, new in new }
                as CFDictionary, nil)
        XCTAssertEqual(added, errSecSuccess)

        let store = PairingStore(keychainPrefix: prefix)
        XCTAssertEqual(store.machines.map(\.id), ["upgrade-studio", "upgrade-laptop"], "every computer is still paired")
        XCTAssertEqual(
            store.machines.map(\.endpoint.host), ["studio.upgrade.ts.net", "laptop.upgrade.ts.net"])
        XCTAssertEqual(store.machines.map(\.endpoint.port), [8787, 8788])
        XCTAssertEqual(store.machines.map(\.endpoint.useTLS), [false, true])
        XCTAssertEqual(store.machines.map(\.endpoint.token), ["studio-upgrade-token", "laptop-upgrade-token"])
    }
}

/// A run's keychain: `Keychain` reaches the service it is given and no other, and with none it
/// reaches nothing, not even the app's own service.
@MainActor
final class KeychainServiceTests: XCTestCase {
    func testTheKeychainReachesOnlyTheServiceItIsGiven() {
        let account = "run-\(UUID().uuidString)"
        let runService = "com.codeconnect.pairing.run-\(UUID().uuidString)"
        let previous = Keychain.service
        func item(_ service: String) -> [String: Any] {
            [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: service,
                kSecAttrAccount as String: account,
            ]
        }
        addTeardownBlock {
            Keychain.service = previous
            SecItemDelete(item(runService) as CFDictionary)
            SecItemDelete(item("com.codeconnect.pairing") as CFDictionary)
        }
        for (service, value) in [(runService, "run"), ("com.codeconnect.pairing", "app")] {
            let added = SecItemAdd(
                item(service).merging([kSecValueData as String: Data(value.utf8)]) { _, new in new } as CFDictionary, nil)
            XCTAssertEqual(added, errSecSuccess)
        }

        Keychain.service = runService
        XCTAssertEqual(Keychain.load(account: account), Data("run".utf8), "the run's service is the one read")

        Keychain.service = nil
        XCTAssertNil(Keychain.load(account: account), "with no service, nothing is read, the app's own included")
        XCTAssertThrowsError(try Keychain.save(Data("kept".utf8), account: account + "-new"))
        XCTAssertTrue(Keychain.delete(account: account))

        for service in [runService, "com.codeconnect.pairing"] {
            Keychain.service = service
            XCTAssertNotNil(Keychain.load(account: account), "nothing was deleted from \(service)")
            XCTAssertNil(Keychain.load(account: account + "-new"), "nothing was written to \(service)")
        }
    }
}
