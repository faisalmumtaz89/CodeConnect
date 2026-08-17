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
/// revoking the device stops it being offered a notification from the next
/// send onward, exactly as it stops the device connecting. A doorbell already
/// on its way to Apple cannot be recalled; it carries no session, request or
/// device identifier, and the app it would reach is no longer paired.
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
            registerForRemoteNotifications()
        }
    }

    /// Register for a token **without** prompting — for the case where the user
    /// denied in-app and later switched notifications on in Settings. Settings
    /// gives no callback, so the app re-reads authorization on foreground and,
    /// finding it granted with no token in hand, asks Apple for one directly. A
    /// second `requestAuthorization` here would either no-op or re-prompt; this
    /// path does neither.
    func registerForRemoteNotifications() {
        if delegate == nil {
            delegate = TokenDelegate { [weak self] token, environment in
                self?.onToken?(token, environment)
            }
        }
        UIApplication.shared.registerForRemoteNotifications()
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

/// The app delegate Apple requires: the token callbacks, foreground
/// presentation, and a tap.
///
/// It owns no product state. A tap is recorded in the buffer below and
/// broadcast; everything else is republished as a notification, so the SwiftUI
/// side stays the owner of what happens next.
///
/// **The tap handler is installed here, before launch finishes**, which is what
/// Apple asks for: a delegate set later may miss a launch-time response. A
/// delegate assigned by a view or a model would be later, and the tap it could
/// miss is the one that started the app.
///
/// The notification-centre callbacks are `nonisolated`: Apple calls them from
/// its own context, and everything they touch — the tap buffer's lock, the
/// payload dictionary — is safe there. The one main-actor hop that is needed
/// happens inside `recordTap`, where the broadcast is posted.
final class PushAppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        return true
    }

    /// While the app is foregrounded, iOS shows no banners unless the delegate
    /// says so — and for the ordinary doorbell that silence is right: the app
    /// itself is already showing the state the push would announce. The one
    /// exception is a **test** the user just asked for, whose entire point is
    /// the banner; the daemon marks those with `codeconnect_test`.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        let info = notification.request.content.userInfo
        return info["codeconnect_test"] != nil ? [.banner, .sound] : []
    }

    /// Where a tap goes: the decision list for an approval, the fleet otherwise.
    ///
    /// **The payload names no session and no decision, deliberately.** One that
    /// pointed at a single card would have to keep being right about it — after
    /// it is answered, superseded, already run, or the daemon restarts — and
    /// would have to carry an identifier through Apple to do it. The *kind* is
    /// stable in a way a target never is, and it is all a tap needs: only an
    /// approval has a card in that list, so only an approval opens it.
    ///
    /// The alert's badge is deliberately *not* consulted: it is a count taken
    /// before the send, so it could send a reader to the fleet moments after a
    /// new decision arrived, and it counts blocked sessions rather than waiting
    /// decisions.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        PushWire.deliverTap(userInfo: response.notification.request.content.userInfo)
        completionHandler()
    }

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
    /// A notification was tapped.
    static let tapNotification = Notification.Name("cc.push.tapped")

    /// **A buffer, not just a broadcast.** A cold-started tap can arrive during
    /// scene connection, before any SwiftUI view — and therefore any observer —
    /// exists. Posting alone could drop the tap that launched the app, so it is
    /// recorded here first; the post is the fast path for a warm one.
    /// One tap, waiting to be drained. `kind` is which doorbell rang — absent
    /// when an older daemon sent no such word.
    struct Tap: Equatable {
        let kind: String?
    }

    private static let pending = NSLock()
    nonisolated(unsafe) private static var pendingTap: Tap?

    /// The routing word out of a real APNs payload.
    ///
    /// A named function because it is a **wire contract** with the daemon, not
    /// a line of glue: the daemon writes `codeconnect.kind` and this is the only
    /// thing that reads it. `UNNotificationResponse` cannot be constructed in a
    /// test, so this is the seam where the production shape is checked.
    static func kind(fromUserInfo userInfo: [AnyHashable: Any]) -> String? {
        (userInfo["codeconnect"] as? [String: Any])?["kind"] as? String
    }

    /// A tapped notification, from its payload to the broadcast.
    ///
    /// **The whole of what the delegate does.** Apple's callback cannot be
    /// driven from a test — `UNNotificationResponse` has no public initialiser
    /// — so the work lives here, where the tests that matter can reach it: the
    /// UI tests deliver a warm tap through this exact function, so emptying it
    /// stops a tapped notification navigating anywhere and they say so.
    static func deliverTap(userInfo: [AnyHashable: Any]) {
        recordTap(kind: kind(fromUserInfo: userInfo))
    }

    static func recordTap(kind: String?) {
        pending.lock()
        pendingTap = Tap(kind: kind)
        pending.unlock()
        // **On the main actor.** The delegate callback arrives on whatever
        // queue Apple chose, and the subscriber mutates `@MainActor` state the
        // instant it fires.
        DispatchQueue.main.async {
            NotificationCenter.default.post(name: tapNotification, object: nil)
        }
    }

    #if DEBUG
        /// Test seam: record without broadcasting.
        ///
        /// The broadcast is what a *running* app reacts to, and the host app is
        /// running during unit tests — its own observer would drain the buffer
        /// before the test could. This seeds the buffer alone, which is the half
        /// that has to survive a cold launch.
        static func seedTapForTesting(kind: String?) {
            pending.lock()
            pendingTap = Tap(kind: kind)
            pending.unlock()
        }

        /// The kind carried by the UI tests' warm-tap URL, or `nil` for every
        /// other link.
        ///
        /// `codeconnect://test-notification-tap/<kind>`. A URL rather than a
        /// launch argument because a launch argument can only fire on a timer,
        /// and a timer cannot know whether the app has finished navigating.
        static func testTapKind(from url: URL) -> String? {
            guard url.scheme == "codeconnect", url.host == "test-notification-tap" else {
                return nil
            }
            let kind = url.path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            return kind.isEmpty ? nil : kind
        }
    #endif

    /// The tap that arrived before anything was listening, if there was one.
    /// Drained once: a tap is an event, not a state to be replayed.
    static func consumeTap() -> Tap? {
        pending.lock()
        defer {
            pendingTap = nil
            pending.unlock()
        }
        return pendingTap
    }

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
