import SwiftUI

/// One shared left edge for the command beside a tool name.
///
/// **The problem this solves.** A tool row is `Bash  git push …` — a prose tool
/// name at full contrast, then the command in monospace. Set inline, the command
/// begins wherever the name happens to end, so a *column* of tool rows has a
/// ragged left edge. Measured on the fleet: `Edit`, `Bash` and `Read` are 23.67,
/// 30.00 and 30.67 wide, so their commands began at 83.67, 90.00 and 90.67.
///
/// It is not cosmetic on these screens. The fleet's own band header carries the
/// note that the screen "gets exactly two" left edges, and the timeline already
/// pins its glyph to a fixed gutter *"because a column of tool rows has to line
/// up whatever symbols it happens to hold"* — the same argument, applied to the
/// icon and not yet to the name beside it.
///
/// **Why a measured column and not a constant.** Tool names run from `Edit` to
/// `MultiEdit` to whatever an MCP server calls itself. A fixed width either
/// truncates the long ones or reserves dead space for the short ones. Taking the
/// widest label actually on screen costs nothing when they are all the same
/// width and fixes the edge when they are not.
///
/// **Why it settles.** The label is measured at its intrinsic size, *before* the
/// column frame widens it, so the maximum cannot chase itself. Tool names do not
/// change with scroll, so this converges on the first pass and then stops — the
/// hazard that a scroll-varying measurement would create does not arise here.
struct CCToolLabelWidthKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        let next = nextValue()
        guard next.isFinite else { return }
        value = max(value, next)
    }
}

private struct CCToolColumnKey: EnvironmentKey {
    static let defaultValue: CGFloat = 0
}

extension EnvironmentValues {
    /// The width every tool label on this screen is drawn into. Zero until the
    /// first pass has measured, which reads as "no column yet" at every call
    /// site rather than as a zero-width frame.
    var ccToolColumn: CGFloat {
        get { self[CCToolColumnKey.self] }
        set { self[CCToolColumnKey.self] = newValue }
    }
}

extension View {
    /// Report this view's intrinsic width as a candidate for the tool column,
    /// then draw it into the column once one exists.
    ///
    /// `disabled` is how a caller opts out at accessibility sizes, where these
    /// rows stack and a width would only be dead space.
    func ccToolLabelColumn(_ width: CGFloat, disabled: Bool = false) -> some View {
        fixedSize()
            .background {
                GeometryReader { proxy in
                    Color.clear.preference(
                        key: CCToolLabelWidthKey.self, value: proxy.size.width)
                }
            }
            .frame(width: disabled || width <= 0 ? nil : width, alignment: .leading)
    }

    /// Collect the widest tool label beneath this view and publish it back down.
    ///
    /// Applied once per screen. Two screens each get their own column, which is
    /// correct: they are different lists and have no reason to agree.
    func ccCollectsToolColumn(into width: Binding<CGFloat>) -> some View {
        onPreferenceChange(CCToolLabelWidthKey.self) { measured in
            if measured > width.wrappedValue { width.wrappedValue = measured }
        }
        .environment(\.ccToolColumn, width.wrappedValue)
    }
}
