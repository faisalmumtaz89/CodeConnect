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
        #if DEBUG
            // **Never ask Apple for a token from a test-host process.** The unit
            // tests run *inside* this app, and Apple answers this call by way of
            // `NotificationCenter.default`, which every `PushRegistration` in the
            // process observes — including the ones a test's own `AppModel` owns.
            //
            // On a simulator whose notification authorization was granted by an
            // earlier run of the app, `requestAndRegister` is therefore answered
            // without a prompt, a *real* token arrives part-way through the suite,
            // and it supersedes the synthetic token the test injected: the
            // credential that was enrolled for the test's token is dropped by the
            // token recheck, and the real token starts an enrollment of its own
            // that drains the scripted transport's queue. Measured on a simulator
            // that had run the app: 9 failures across 6 `RelayOrchestrationTests`,
            // 0 on an erased one, and 0 here once this call is not made.
            guard !PushWire.isTestHostProcess else { return }
        #endif
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
