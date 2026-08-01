import Foundation
import UIKit
import UserNotifications

/// Getting this phone an APNs token, and nothing more.
///
/// **Permission is asked for once and never assumed.** iOS answers
/// `registerForRemoteNotifications` with a token whether or not the user has
/// granted permission, so a token in hand does not mean a notification will ever
/// be shown. Authorization is therefore checked and reported separately, and the
/// app says which of the two states it is in rather than implying the good one.
///
/// **The token is not a credential.** It identifies a device to Apple, changes
/// on reinstall and on restore-from-backup, and is useless without the
/// provider key on the Mac. It is sent over the same authenticated tailnet
/// connection as everything else and stored against the paired device row, so
/// revoking the device stops its notifications at the same instant it stops the
/// device connecting.
@MainActor
final class PushRegistration: NSObject {
    /// Called with the token, lowercase hex, once Apple issues one.
    var onToken: ((String, String) -> Void)?
    /// Called with what the user actually decided, so the UI can say so.
    var onAuthorization: ((Bool) -> Void)?

    private var delegate: TokenDelegate?

    /// Ask, then register.
    ///
    /// Registering *before* asking would produce a token the app cannot use and
    /// a lock screen that stays silent, which is exactly the state that is hard
    /// to tell from "push is broken".
    func requestAndRegister() {
        Task {
            let granted = await requestAuthorization()
            onAuthorization?(granted)
            guard granted else { return }
            let delegate = TokenDelegate { [weak self] token, environment in
                self?.onToken?(token, environment)
            }
            self.delegate = delegate
            UIApplication.shared.registerForRemoteNotifications()
        }
    }

    /// The current setting, re-read rather than remembered: the user can change
    /// it in Settings at any time and the app has no callback for that.
    func authorizationStatus() async -> UNAuthorizationStatus {
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    private func requestAuthorization() async -> Bool {
        do {
            return try await UNUserNotificationCenter.current()
                .requestAuthorization(options: [.alert, .sound, .badge])
        } catch {
            return false
        }
    }

    /// Apple hands the token to the app delegate, and SwiftUI's `App` has none —
    /// so one exists for this single purpose.
    private final class TokenDelegate: NSObject {
        private let onToken: (String, String) -> Void
        init(onToken: @escaping (String, String) -> Void) {
            self.onToken = onToken
            super.init()
            NotificationCenter.default.addObserver(
                self, selector: #selector(received(_:)),
                name: PushWire.tokenNotification, object: nil)
        }

        @objc private func received(_ note: Notification) {
            guard let data = note.object as? Data else { return }
            onToken(PushWire.hex(data), PushWire.environment)
        }
    }

}

/// The app delegate Apple requires for the token callback.
///
/// It does one thing and holds no state: the token is republished as a
/// notification so the SwiftUI side can stay the owner of what happens next.
final class PushAppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        NotificationCenter.default.post(
            name: PushWire.tokenNotification, object: deviceToken)
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        // Reported, never swallowed. The usual cause is an App ID without the
        // Push Notifications capability, and the symptom without this line is a
        // phone that simply never buzzes.
        NotificationCenter.default.post(
            name: PushWire.failureNotification, object: error.localizedDescription)
    }
}

/// The parts of push registration that are not main-actor work.
///
/// Apple's token callback arrives on the app delegate outside any actor, so the
/// pure helpers it needs live here rather than on the `@MainActor` type.
enum PushWire {
    static let tokenNotification = Notification.Name("cc.push.token")
    static let failureNotification = Notification.Name("cc.push.failed")

    /// Lowercase hex, which is the only form Apple's `/3/device/<token>` path
    /// accepts. `Data.description` produces `<a1b2 …>` and would 400.
    static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    /// Which APNs world this build's token belongs to.
    ///
    /// Read from the signed provisioning profile rather than from `#if DEBUG`:
    /// what decides the token's validity is how the binary was *signed*, and a
    /// debug build signed for distribution — or a release build signed for
    /// development — would take the wrong branch every time.
    static var environment: String {
        #if targetEnvironment(simulator)
            // A simulator has no APNs token worth sending anywhere.
            return "sandbox"
        #else
            guard
                let url = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
                let raw = try? Data(contentsOf: url),
                let text = String(data: raw, encoding: .isoLatin1),
                let range = text.range(of: "<key>aps-environment</key>")
            else {
                // **No profile means App Store or TestFlight, which is
                // production.** Apple re-signs on the way through and strips
                // `embedded.mobileprovision`, so its absence is not "unknown" —
                // it is the one case that is certainly not a development build.
                //
                // This defaulted to `sandbox`, which is exactly backwards: the
                // first TestFlight install registered a production token,
                // reported `sandbox`, and every push came back
                // `400 BadDeviceToken` from the sandbox host.
                return "production"
            }
            // The value is the next `<string>` after the key, read to its close
            // rather than from a fixed-width window, so plist whitespace cannot
            // change the answer.
            let tail = text[range.upperBound...]
            guard
                let open = tail.range(of: "<string>"),
                let close = tail.range(of: "</string>", range: open.upperBound..<tail.endIndex)
            else { return "production" }
            return tail[open.upperBound..<close.lowerBound] == "development"
                ? "sandbox" : "production"
        #endif
    }
}
