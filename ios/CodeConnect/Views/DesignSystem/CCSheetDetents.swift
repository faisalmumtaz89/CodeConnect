import SwiftUI

// =============================================================================
//  CCSheetDetents — one sheet height policy, everywhere.
// =============================================================================

/// The heights a sheet may take, and the reason none of them is `.large`.
///
/// **iOS 26 draws a sheet at a partial detent as an inset floating card and
/// `.large` edge-to-edge, and those are two different sizes.** Measured on
/// iOS 26.3 through element frames rather than pixels: every element of the
/// `/model` sheet is exactly 1.04145× larger at `.large` than at `.medium`, in
/// width *and* height, and the content origin moves from x=23.363 to x=16.0 —
/// a 0.9602 scale about the screen's centre. So dragging a sheet open rescaled
/// the whole thing mid-gesture, which reads as a flinch rather than a move.
///
/// A bare SwiftUI sheet carrying none of this app's chrome reproduces it
/// exactly, so it is the platform's presentation and not this app's layout.
/// Neither `UISheetPresentationController` nor SwiftUI exposes a switch for it.
/// What they do expose is which *class* a detent belongs to: every detent
/// except `.large` floats. Keeping all of them out of `.large` keeps a sheet in
/// one class for its whole life, so expanding it translates and never resizes.
///
/// `.fraction(1.0)` is not a substitute — it resolves to `.large` and the
/// rescale returns. The ~3% of height this gives up buys every sheet in the app
/// the same geometry, which is why tall-only sheets pay it too: a reader who
/// opens two different sheets should not be shown two different sizes of the
/// same design system.
enum CCSheetDetents {
    /// A sheet's full height: the whole height less a fixed gap. A fraction
    /// gives a gap that shrinks with the screen, and `.fraction(0.97)`, which
    /// floats on an iPhone 17 Pro, falls into `.large`'s class on an iPhone SE
    /// and rescales there.
    static let expanded: PresentationDetent = .custom(Expanded.self)

    private struct Expanded: CustomPresentationDetent {
        static func height(in context: Context) -> CGFloat? { context.maxDetentValue - 24 }
    }
}

extension View {
    /// Opens half height, drags up to full. The default for a sheet whose
    /// content a reader may want to weigh against the screen behind it.
    func ccResizableSheet() -> some View {
        presentationDetents([.medium, CCSheetDetents.expanded])
    }

    /// The same policy, for a sheet that drives its own detent.
    func ccResizableSheet(selection: Binding<PresentationDetent>) -> some View {
        presentationDetents([.medium, CCSheetDetents.expanded], selection: selection)
    }

    /// One height, and it is the tall one — for a sheet that has to be resolved
    /// rather than browsed, or whose content wants every point it can get.
    func ccTallSheet() -> some View {
        presentationDetents([CCSheetDetents.expanded])
    }

    /// As tall as its content and no taller, for a sheet whose content is fixed:
    /// one height, so nothing to drag toward and no grabber. Its parts report
    /// their heights (`ccSheetHeightPart`): the chrome above the content, and
    /// the content inside its scroll view. Content taller than the screen gets
    /// the screen, and scrolls.
    func ccFittedSheet() -> some View {
        modifier(CCFittedSheet())
    }

    /// Counts this view's height into its fitted sheet's.
    func ccSheetHeightPart() -> some View {
        background {
            GeometryReader { proxy in
                Color.clear.preference(key: CCSheetHeightKey.self, value: proxy.size.height)
            }
        }
    }
}

private struct CCSheetHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value += nextValue() }
}

private struct CCFittedSheet: ViewModifier {
    @State private var height: CGFloat = 0

    func body(content: Content) -> some View {
        content
            .environment(\.ccSheetIsFitted, true)
            .onPreferenceChange(CCSheetHeightKey.self) { height = $0 }
            .presentationDetents(height > 0 ? [.height(height)] : [CCSheetDetents.expanded])
    }
}

private struct CCSheetIsFittedKey: EnvironmentKey {
    static let defaultValue = false
}

extension EnvironmentValues {
    /// Set inside a fitted sheet, which has one height and so no grabber.
    var ccSheetIsFitted: Bool {
        get { self[CCSheetIsFittedKey.self] }
        set { self[CCSheetIsFittedKey.self] = newValue }
    }
}
