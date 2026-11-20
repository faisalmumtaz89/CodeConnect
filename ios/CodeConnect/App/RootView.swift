import CodeConnectKit
import SwiftUI

/// One root (Fleet), one detail stack (Session), sheets for everything else — a
/// notification-led information architecture. No tab bar.
///
/// Everything below is handed the model of the machine the app-wide surfaces
/// speak for (`Machines.linkModel`); the screens under a session are handed
/// that session's own.
struct RootView: View {
    @Environment(Machines.self) private var machines
    /// The cold launch's splash, laid over the first screen until its exit has
    /// run. A return from the background keeps this view, so it never replays.
    @State private var splash: SplashState? = Splash.playsAtLaunch ? SplashState() : nil

    var body: some View {
        Group {
            if !machines.showsFleet {
                PairingView(isOnboarding: true)
            } else if machines.launchSettled || splash != nil {
                // Under the splash the fleet is laid out while it catches up,
                // so the splash leaves to a complete first screen.
                FleetView()
            } else {
                LaunchLoader()
            }
        }
        .environment(machines.linkModel)
        // Every ask on the root screen is drawn over all of it; each sheet
        // that asks hosts its own.
        .ccConfirmationHost()
        .overlay {
            if let splash {
                // No computer set up, as the screen under it says: read live,
                // since the computers this phone keeps arrive after the first frame.
                Splash(
                    started: splash.started, settledAt: splash.settledAt, unpaired: !machines.showsFleet,
                    onFinished: { self.splash = nil })
            }
        }
        .task {
            #if DEBUG
                if let folder = UserDefaults.standard.string(forKey: "CC_LAUNCH_FRAMES") {
                    // The computer's name arrives with its hello.
                    for _ in 0..<100 where machines.models.first?.machineName == nil {
                        try? await Task.sleep(for: .milliseconds(100))
                    }
                    Splash.writeFrames(to: folder, computer: machines.models.first?.machineName ?? "your computer")
                }
            #endif
            guard splash != nil else { return }
            await machines.waitOutLaunch()
        }
        .onChange(of: machines.launchSettled, initial: true) { _, settled in
            guard settled, let started = splash?.started, splash?.settledAt == nil else { return }
            // A still is drawn at the moments it is given, the catch-up's too.
            splash?.settledAt = Splash.frozenT == nil ? Date().timeIntervalSince(started) : Splash.frozenSettledAt
        }
    }
}

/// One cold launch's splash: when it began, and when the fleet caught up.
struct SplashState {
    let started = Date()
    var settledAt: Double?
}

/// What the app shows until it has caught up with the Mac when no splash
/// covers the wait: after the first pairing. How long it waits is
/// `AppModel.waitOutLaunch`.
private struct LaunchLoader: View {
    @Environment(Machines.self) private var machines

    var body: some View {
        VStack(spacing: CC.space.sm) {
            CCProgressRing(.md)
            Text("Connecting to your computer…")
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(CC.color.bg)
        .accessibilityElement(children: .combine)
        .task { await machines.waitOutLaunch() }
    }
}

#if DEBUG

    /// **What content-size category this process actually resolved to**, so a
    /// render pass can prove it rendered at the size it claims.
    ///
    /// The whole reason it exists: `xcodebuild test` plus
    /// `XCUIApplication.launch()` silently resets the simulator's content-size
    /// category, and `TEST_RUNNER_*` never reaches the runner — so an AX5 pass
    /// that was quietly running at `L` looks exactly like an AX5 pass that
    /// worked. Several earlier "findings" were that, and they were believed. A
    /// harness that cannot check its own premise is an instrument with no
    /// calibration mark.
    ///
    /// Attached at the **app** root rather than to `RootView`, because
    /// `CC_GALLERY_PAGE` swaps `RootView` out for `CCGallery` and half the
    /// catalogue is gallery pages — a calibration mark that is absent from
    /// exactly the renders it is meant to certify is worse than none.
    ///
    /// Present **only** when `-CC_RENDER_PROBE` is on the command line, so it
    /// cannot add an element to a tree a product UI test is counting, and
    /// `#if DEBUG` so it cannot exist in a shipped build at all. One point
    /// square, transparent, not hit-testable: read by identifier, never seen and
    /// never touched.
    struct CCRenderProbe: View {
        @Environment(\.dynamicTypeSize) private var typeSize

        private var enabled: Bool {
            UserDefaults.standard.bool(forKey: "CC_RENDER_PROBE")
        }

        var body: some View {
            if enabled {
                // The size SwiftUI is *actually laying out with* — the one every
                // `@ScaledMetric` in the kit reads — not the one UIKit was asked
                // for. Those two are exactly the pair that can disagree.
                Color.clear
                    .frame(width: 1, height: 1)
                    .accessibilityElement()
                    .accessibilityIdentifier("cc-render-probe")
                    .accessibilityLabel(Self.name(for: typeSize))
                    .allowsHitTesting(false)
            }
        }

        /// The `UIContentSizeCategory` spelling, because that is what
        /// `xcrun simctl ui <device> content_size` speaks and what a render
        /// filename has to be comparable against.
        static func name(for size: DynamicTypeSize) -> String {
            switch size {
            case .xSmall: return "UICTContentSizeCategoryXS"
            case .small: return "UICTContentSizeCategoryS"
            case .medium: return "UICTContentSizeCategoryM"
            case .large: return "UICTContentSizeCategoryL"
            case .xLarge: return "UICTContentSizeCategoryXL"
            case .xxLarge: return "UICTContentSizeCategoryXXL"
            case .xxxLarge: return "UICTContentSizeCategoryXXXL"
            case .accessibility1: return "UICTContentSizeCategoryAccessibilityM"
            case .accessibility2: return "UICTContentSizeCategoryAccessibilityL"
            case .accessibility3: return "UICTContentSizeCategoryAccessibilityXL"
            case .accessibility4: return "UICTContentSizeCategoryAccessibilityXXL"
            case .accessibility5: return "UICTContentSizeCategoryAccessibilityXXXL"
            @unknown default: return "unknown"
            }
        }
    }

#endif
