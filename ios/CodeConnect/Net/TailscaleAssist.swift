import UIKit

/// The one place the app knows how to hand the user to Tailscale.
///
/// `canOpenURL` is the honest capability check Apple provides: true means a
/// registered handler exists and a subsequent `open` will succeed. False means
/// only that no handler is registered — which is why the fallback says "Set
/// up", never "Get": the app cannot know whether Tailscale is absent or merely
/// unqueryable. The scheme must be declared in `LSApplicationQueriesSchemes`
/// for the check to be answered at all.
@MainActor
enum TailscaleAssist {
    private static let appURL = URL(string: "tailscale://")!
    /// The vendor's own install page — deliberately not a hardcoded App Store
    /// ID, which would break silently if the listing ever moved.
    private static let setupURL = URL(string: "https://tailscale.com/download/ios")!

    /// Whether the banner should read "Set up Tailscale" instead of "Open
    /// Tailscale".
    static var isInstallHintNeeded: Bool {
        !UIApplication.shared.canOpenURL(appURL)
    }

    /// Open the Tailscale app when it answers, its install page when it does
    /// not.
    static func open() {
        let app = UIApplication.shared
        if app.canOpenURL(appURL) {
            app.open(appURL)
        } else {
            app.open(setupURL)
        }
    }
}
