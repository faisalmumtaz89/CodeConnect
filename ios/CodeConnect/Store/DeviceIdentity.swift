import Foundation

/// Who this install is, to the Mac.
///
/// Stable per install and independent of any credential: the daemon's logs and
/// `codeconnect devices` use it to tell two phones apart, so it has to outlive
/// pairing, re-pairing, and whatever transport the terminal happens to use.
enum DeviceIdentity {
    private static let key = "codeconnect.installationID"

    static let installationID: String = {
        if let existing = UserDefaults.standard.string(forKey: key) { return existing }
        let fresh = UUID().uuidString
        UserDefaults.standard.set(fresh, forKey: key)
        return fresh
    }()
}
