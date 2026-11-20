import CodeConnectKit
import SwiftUI
import UIKit

// =============================================================================
//  CCBackButton — a pushed screen's way back.
// =============================================================================

extension View {
    /// A pushed screen's back control: the 36pt circle every other bar button
    /// in the app is (`ccSheetButton`), in place of the platform's back button,
    /// which iOS 26 draws as a 44pt glass disc no other item in the bar uses.
    /// `previous` is the title of the screen it returns to, spoken after "Back"
    /// as the platform's own button speaks it.
    func ccBackButton(to previous: String) -> some View {
        modifier(CCBackButton(previous: previous))
    }
}

private struct CCBackButton: ViewModifier {
    let previous: String
    @Environment(\.dismiss) private var dismiss

    func body(content: Content) -> some View {
        content
            .navigationBarBackButtonHidden(true)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button {
                        dismiss()
                    } label: {
                        CCIcon("arrow-left", size: 16, weight: .semibold)
                            .foregroundStyle(CC.text.primary)
                            .ccSheetButtonSizing()
                            .ccSheetButton()
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Back")
                    .accessibilityValue(previous)
                    .accessibilityIdentifier("nav-back")
                }
                .ccPlainToolbarItem()
            }
    }
}

/// Hiding the platform's back button also switches off the swipe from the
/// screen's left edge that goes back. This turns it on again: the gesture
/// begins wherever there is a screen to go back to.
extension UINavigationController: @retroactive UIGestureRecognizerDelegate {
    override open func viewDidLoad() {
        super.viewDidLoad()
        interactivePopGestureRecognizer?.delegate = self
    }

    public func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        viewControllers.count > 1
    }
}
