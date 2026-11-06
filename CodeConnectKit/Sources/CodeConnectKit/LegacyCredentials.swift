import Foundation

/// Credentials earlier releases stored, deleted at startup.
///
/// This app holds one secret: the paired daemon and its device token, under the
/// `daemon` account of `Keychain`'s service. Earlier releases also stored a
/// transport credential under two Keychain accounts and the Mac's SSH
/// coordinates under three `UserDefaults` keys; nothing reads any of them, so
/// they are deleted. The defaults keys are why this reaches past the Keychain:
/// they are gone from the current tree but still sit in an upgraded user's
/// Preferences plist — the host and the Mac username among them — and ride into
/// every backup until removed.
enum LegacyCredentials {
    private static let retiredAccounts = ["ssh-ed25519", "ssh-known-hosts"]
    private static let retiredDefaultsKeys = [
        "codeconnect.ssh.username", "codeconnect.ssh.host", "codeconnect.ssh.port",
    ]

    /// Idempotent and silent. `Keychain.delete` reports an absent item as
    /// success and `removeObject` no-ops on an absent key, so every launch after
    /// the first finds nothing and changes nothing.
    ///
    /// `defaults` is injectable only so the removal can be checked against an
    /// isolated suite; production passes the standard database.
    static func purge(defaults: UserDefaults = .standard) {
        for account in retiredAccounts {
            Keychain.delete(account: account)
        }
        for key in retiredDefaultsKeys {
            defaults.removeObject(forKey: key)
        }
    }
}
