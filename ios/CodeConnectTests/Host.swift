import SwiftUI
import UIKit

/// A view in a real, visible window, which is what makes SwiftUI run its
/// lifecycle at all: `onAppear`, `onReceive` and `onChange` belong to a view
/// that is being rendered, and a hosting controller left off screen is not.
///
/// A window off the scene rather than a bare frame: the lifecycle these tests
/// measure is the app's, and the app's runs in a scene.
@MainActor
final class Host {
    let view: UIView
    private let window: UIWindow
    private let previousKey: UIWindow?

    init<Root: View>(_ root: Root, scene: UIWindowScene, size: CGSize) {
        let controller = UIHostingController(rootView: root)
        view = controller.view
        window = UIWindow(windowScene: scene)
        window.frame = CGRect(origin: .zero, size: size)
        previousKey = scene.windows.first { $0.isKeyWindow }
        window.rootViewController = controller
        window.makeKeyAndVisible()
        view.layoutIfNeeded()
    }

    func dismantle() {
        window.isHidden = true
        window.rootViewController = nil
        previousKey?.makeKeyAndVisible()
    }
}
