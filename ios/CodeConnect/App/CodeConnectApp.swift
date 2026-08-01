import SwiftUI

@main
struct CodeConnectApp: App {
    /// Apple delivers the APNs token to a `UIApplicationDelegate` and SwiftUI's
    /// `App` has none, so one is adopted for that single callback.
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
            .task { model.bootstrap() }
            // Deep links land on the model, not on a view: the target has to
            // survive the app being cold-started by the link, and once push
            // notifications land the same entry point serves those too.
            .onOpenURL { url in _ = model.open(url: url) }
    }
}
