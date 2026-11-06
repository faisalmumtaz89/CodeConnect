import Foundation

/// The parts of push registration that are not main-actor work.
///
/// Apple's token callback arrives on the app delegate outside any actor, so the
/// pure helpers it needs live here rather than on the `@MainActor` type.
public enum PushWire {
    #if DEBUG
        /// Whether XCTest launched this process to host a test bundle.
        ///
        /// `XCTestConfigurationFilePath` is placed in the **host application's**
        /// environment by XCTest and nowhere else: a build started from the Home
        /// screen, TestFlight or the App Store is launched by the system, which
        /// sets no such variable, and it appears in no Info.plist or entitlement
        /// of this app. A UI test does not set it here either — the variable
        /// belongs to the *runner* process, and the app under test is launched
        /// separately — so the UI tests and the render harness continue to drive
        /// a shipping-identical app. The whole property is `DEBUG`-only, so in a
        /// Release build the branch it guards is not merely false but absent.
        public static var isTestHostProcess: Bool {
            ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
        }
    #endif

    public static let tokenNotification = Notification.Name("cc.push.token")
    public static let failureNotification = Notification.Name("cc.push.failed")
    /// A notification was tapped.
    public static let tapNotification = Notification.Name("cc.push.tapped")

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
    public static func deliverTap(userInfo: [AnyHashable: Any]) {
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
        public static func seedTapForTesting(kind: String?) {
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
        public static func testTapKind(from url: URL) -> String? {
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
    public static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    /// Which APNs world this build's token belongs to.
    ///
    /// Read from the signed provisioning profile rather than from `#if DEBUG`:
    /// what decides the token's validity is how the binary was *signed*, and a
    /// debug build signed for distribution — or a release build signed for
    /// development — would take the wrong branch every time.
    public static var environment: String {
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
