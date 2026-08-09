import SwiftUI

@main
struct CodeConnectApp: App {
    /// Apple delivers the APNs token — and a notification tap — to a
    /// `UIApplicationDelegate`, and SwiftUI's `App` has none, so one is adopted.
    @UIApplicationDelegateAdaptor(PushAppDelegate.self) private var pushDelegate
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            #if DEBUG
                surface
                    // The render harness's calibration mark — see `CCRenderProbe`.
                    // Here rather than inside `RootView` because `CC_GALLERY_PAGE`
                    // replaces `RootView` entirely, and half the render catalogue
                    // is gallery pages.
                    .overlay(alignment: .topLeading) { CCRenderProbe() }
            #else
                root
            #endif
        }
        .onChange(of: scenePhase) { _, phase in
            model.scenePhaseChanged(to: phase)
        }
    }

    #if DEBUG
        /// The design system's gallery harness, wired the way the README has
        /// always claimed it was: set `CC_GALLERY_PAGE` and the app boots into
        /// `CCGallery` instead of the product. It used to require editing this
        /// file by hand before every render, which is how a gallery goes a long
        /// time without being looked at.
        ///
        ///   xcrun simctl launch --console <dev> <bundle-id>
        ///     with SIMCTL_CHILD_CC_GALLERY_PAGE=indicators
        ///
        /// DEBUG-only and unreachable in a release build: the product has no way
        /// to reach this and neither does a user.
        @ViewBuilder
        private var surface: some View {
            if ProcessInfo.processInfo.environment["CC_GALLERY_PAGE"] != nil {
                CCGallery()
            } else {
                root
            }
        }
    #endif

    private var root: some View {
        RootView()
            // Dark-only, applied once at the root. See DesignKit.swift.
            .ccAppearance()
            .environment(model)
            // Drains a tap that launched the app, which can arrive before this
            // view — and therefore the observer below — exists.
            .task {
                #if DEBUG
                    // Test seam: `-CC_TAP approval|input|done|idle` seeds the
                    // buffer a cold-started tap lands in, so a UI test drives
                    // the same path a notification does — buffer, drain, route,
                    // view — without SpringBoard, which XCUITest cannot reach.
                    // A *warm* tap arrives by URL instead; see `onOpenURL`.
                    if let kind = UserDefaults.standard.string(forKey: "CC_TAP") {
                        PushWire.seedTapForTesting(kind: kind)
                    }
                #endif
                model.consumePendingTap()
                model.bootstrap()
            }
            // Deep links land on the model, not on a view: the target has to
            // survive the app being cold-started by the link, and a tapped
            // notification uses the same entry point for the same reason.
            .onOpenURL { url in
                #if DEBUG
                    // Test seam for a *warm* tap: the test opens this URL only
                    // after it can see the screen the tap has to replace, so
                    // the ordering is something the test observed rather than
                    // something it waited out. What follows is production —
                    // `recordTap` broadcasts, and the observer below routes.
                    if let kind = PushWire.testTapKind(from: url) {
                        // Through the same entry point Apple's callback uses,
                        // and with the payload the daemon writes — so what the
                        // UI tests drive is the production path from the
                        // payload onwards, not a shortcut past it.
                        PushWire.deliverTap(userInfo: ["codeconnect": ["kind": kind]])
                        return
                    }
                #endif
                _ = model.open(url: url)
            }
            // A tapped notification lands the same way a link does, and for the
            // same reason: it may have cold-started the app, so the destination
            // has to survive there being no view yet.
            .onReceive(NotificationCenter.default.publisher(for: PushWire.tapNotification)) { _ in
                model.consumePendingTap()
            }
    }
}
