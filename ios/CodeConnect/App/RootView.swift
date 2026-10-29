import SwiftUI

/// One root (Fleet), one detail stack (Session), sheets for everything else — a
/// notification-led information architecture. No tab bar.
///
/// Everything below is handed the model of the machine the app-wide surfaces
/// speak for (`Machines.linkModel`); the screens under a session are handed
/// that session's own.
struct RootView: View {
    @Environment(Machines.self) private var machines

    var body: some View {
        Group {
            if !machines.showsFleet {
                PairingView(isOnboarding: true)
            } else if machines.launchSettled {
                FleetView()
            } else {
                LaunchLoader()
            }
        }
        .environment(machines.linkModel)
    }
}

/// What the app shows until it has caught up with the Mac. How long it waits is
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
