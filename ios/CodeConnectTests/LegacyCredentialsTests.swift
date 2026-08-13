import XCTest

@testable import CodeConnect

final class LegacyCredentialsTests: XCTestCase {
    private func isolatedDefaults() -> (UserDefaults, String) {
        let suite = "codeconnect.legacy.\(UUID().uuidString)"
        return (UserDefaults(suiteName: suite)!, suite)
    }

    func testThePurgeRemovesTheRetiredSSHDefaultsKeys() {
        let (defaults, suite) = isolatedDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        let keys = ["codeconnect.ssh.username", "codeconnect.ssh.host", "codeconnect.ssh.port"]
        for key in keys { defaults.set("retired", forKey: key) }

        LegacyCredentials.purge(defaults: defaults)

        for key in keys {
            XCTAssertNil(defaults.object(forKey: key), "\(key) survived the purge")
        }
    }

    func testThePurgeLeavesUnrelatedDefaultsUntouched() {
        let (defaults, suite) = isolatedDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        defaults.set("retired", forKey: "codeconnect.ssh.host")
        defaults.set(12, forKey: "codeconnect.terminal.fontSize")

        LegacyCredentials.purge(defaults: defaults)

        XCTAssertNil(defaults.object(forKey: "codeconnect.ssh.host"))
        XCTAssertEqual(defaults.integer(forKey: "codeconnect.terminal.fontSize"), 12)
    }

    /// The other half of the purge: the two Keychain accounts an earlier
    /// release's transport credential lived under.
    ///
    /// Both halves are asserted in one place because they are one promise —
    /// "nothing this app used to store is still on the device" — and a purge
    /// that lost either half would leave a private key or a Mac's hostname
    /// riding into every backup indefinitely.
    ///
    /// Skipped rather than failed where the Keychain is unavailable: the
    /// assertion needs an item to have really been written, and a
    /// `-34018`-shaped environment would otherwise report a defect in the purge
    /// that is not there.
    func testThePurgeRemovesTheRetiredKeychainAccounts() throws {
        let accounts = ["ssh-ed25519", "ssh-known-hosts"]
        let seeded = Data("retired".utf8)
        for account in accounts {
            do {
                try Keychain.save(seeded, account: account)
            } catch {
                throw XCTSkip("this environment has no writable Keychain: \(error)")
            }
        }
        // The premise, stated where it is measured.
        for account in accounts {
            XCTAssertEqual(Keychain.load(account: account), seeded, "\(account) was never stored")
        }

        let (defaults, suite) = isolatedDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        LegacyCredentials.purge(defaults: defaults)

        for account in accounts {
            XCTAssertNil(Keychain.load(account: account), "\(account) survived the purge")
        }
    }

    func testThePurgeIsSilentWhenTheKeysAreAbsent() {
        let (defaults, suite) = isolatedDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        // A second launch: nothing to remove, and nothing left behind.
        LegacyCredentials.purge(defaults: defaults)

        for key in ["codeconnect.ssh.username", "codeconnect.ssh.host", "codeconnect.ssh.port"] {
            XCTAssertNil(defaults.object(forKey: key))
        }
    }
}
