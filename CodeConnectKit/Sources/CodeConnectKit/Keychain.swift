import Foundation
import Security

/// Minimal generic-password wrapper. The pairing token is the only secret the
/// app holds, and it never leaves the device: `ThisDeviceOnly` accessibility
/// keeps it out of encrypted backups and iCloud Keychain.
public enum Keychain {
    enum Failure: LocalizedError {
        case status(OSStatus)

        var errorDescription: String? {
            switch self {
            case .status(let code):
                let message = SecCopyErrorMessageString(code, nil) as String?
                return message ?? "Keychain error \(code)"
            }
        }
    }

    #if DEBUG
        /// Test seam: the service every item is kept under. A Mac run sets it before anything reads
        /// the keychain: to a run's own service, or to nil, which makes every call find nothing and
        /// keep nothing without reaching the keychain, so a run never reads or writes the pairing
        /// the computer holds.
        nonisolated(unsafe) public static var service: String? = "com.codeconnect.pairing"
    #else
        private static let service: String? = "com.codeconnect.pairing"
    #endif

    static func save(_ data: Data, account: String) throws {
        guard let service else { throw Failure.status(errSecNotAvailable) }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]

        let updateStatus = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        if updateStatus == errSecSuccess { return }
        guard updateStatus == errSecItemNotFound else { throw Failure.status(updateStatus) }

        let addStatus = SecItemAdd(query.merging(attributes) { _, new in new } as CFDictionary, nil)
        guard addStatus == errSecSuccess else { throw Failure.status(addStatus) }
    }

    static func load(account: String) -> Data? {
        guard let service else { return nil }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess else { return nil }
        return item as? Data
    }

    @discardableResult
    public static func delete(account: String) -> Bool {
        guard let service else { return true }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let status = SecItemDelete(query as CFDictionary)
        return status == errSecSuccess || status == errSecItemNotFound
    }
}
