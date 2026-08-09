import SwiftUI

// =============================================================================
//  CCSheetChrome — one sheet header, everywhere.
// =============================================================================

/// Grabber, title, close affordance, hairline, content.
///
/// "Sheets share `CCSheetChrome`." The system's `.presentationDragIndicator`
/// draws a grabber in the system's own grey, and a `NavigationStack` inside a
/// sheet brings a navigation bar with it — both are how five sheets end up
/// with five different headers.
struct CCSheetChrome<Content: View, Trailing: View>: View {
    let title: String
    var subtitle: String?
    /// Nil hides the close button — for a sheet that must be resolved rather
    /// than dismissed.
    var onClose: (() -> Void)?
    var closeLabel: String = "Close"

    private let trailing: () -> Trailing
    private let content: () -> Content

    @Environment(\.dynamicTypeSize) private var typeSize

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised
    /// memberwise one, and the two collide over the private closure properties.
    init(
        _ title: String,
        subtitle: String? = nil,
        onClose: (() -> Void)? = nil,
        closeLabel: String = "Close",
        @ViewBuilder trailing: @escaping () -> Trailing,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.title = title
        self.subtitle = subtitle
        self.onClose = onClose
        self.closeLabel = closeLabel
        self.trailing = trailing
        self.content = content
    }

    var body: some View {
        VStack(spacing: 0) {
            grabber

            header
                .padding(.horizontal, CC.space.md)
                .padding(.top, CC.space.xs)
                .padding(.bottom, CC.space.sm)

            CCHairline()

            content()
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .background(CC.color.bg)
        // The sheet's own backdrop, so the corners of the presentation do not
        // reveal the system's material underneath.
        .presentationBackground(CC.color.bg)
        // Ours is drawn above; the system's would be a second one.
        .presentationDragIndicator(.hidden)
    }

    /// **Two circles leave the title's line at accessibility sizes**, exactly as
    /// `CCRow` drops its trailing accessory and `CCMonoBlock` drops its copy
    /// button. One mechanism, used everywhere, rather than three components each
    /// surviving AX5 its own way.
    ///
    /// Measured failure this replaces: at AX5 a refresh circle and a close
    /// circle are ~60pt each and `fixedSize`, so on a 402pt sheet they left the
    /// title about 150pt and `Diff · fx-1` wrapped to `Diff ·` / `fx-1` — a
    /// heading broken across a separator glyph, on the sheet that says which
    /// session's changes you are looking at.
    ///
    /// **Two** is the condition, not "accessibility sizes", because two is what
    /// was measured. One circle leaves a title ~260pt, which is a line and a
    /// half of AX5 headline and no squeeze at all; moving it down anyway costs
    /// ~90pt of empty black at the top of every sheet in the product, on the
    /// screen where the least room is left for content. The rule is the same
    /// shape as the kit's other AX switches and it fires where the damage is.
    ///
    /// The title stays *first*: the controls move below it rather than above so
    /// VoiceOver still reads what the sheet is before how to leave it.
    private var stacksControls: Bool {
        typeSize.isAccessibilitySize && Trailing.self != EmptyView.self && onClose != nil
    }

    @ViewBuilder
    private var header: some View {
        if stacksControls {
            VStack(alignment: .leading, spacing: CC.space.sm) {
                titleBlock
                HStack(spacing: CC.space.sm) {
                    Spacer(minLength: 0)
                    trailing()
                    if let onClose { closeButton(onClose) }
                }
            }
        } else {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
                titleBlock
                trailing()
                if let onClose { closeButton(onClose) }
            }
        }
    }

    /// **The subtitle resolves backtick markup** (`CCProse`); the title does
    /// not.
    ///
    /// The subtitle is a sheet's *anchor* — the comment sheet's is
    /// ``In `ios/CodeConnect/Net/Sender.swift` lines 12–28`` — and a slot that
    /// rendered it as prose would set the one element whose entire job is to
    /// say *which file and which lines* in proportional type, wearing two
    /// literal grave accents the caller meant as markup. The rule is
    /// unconditional: identifiers, commands, diffs and paths are monospace,
    /// always.
    ///
    /// **A title is a sentence, and it now contains a project name.** A project
    /// is a directory the user named, and a directory may legitimately contain
    /// a backtick, an asterisk or an underscore. Passing that through a markup
    /// renderer would silently eat the characters, or worse, italicise part of
    /// somebody's folder — so the title is set verbatim, and a caller with an
    /// identifier to mark puts it in the subtitle where the monospace rule
    /// already lives.
    private var titleBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            Text(verbatim: title)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                // **Bounded, because a title can now carry a project.** A
                // forty-character name is what the daemon allows, and at the
                // largest accessibility sizes an unbounded header eats the
                // sheet it is labelling. From the tail, as a project is drawn
                // everywhere else.
                .lineLimit(2)
                .truncationMode(.tail)
                .fixedSize(horizontal: false, vertical: true)
            if let subtitle {
                CCProse(subtitle, style: CC.type.footnote, color: CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityAddTraits(.isHeader)
    }

    private var grabber: some View {
        Capsule()
            .fill(CC.color.borderStrong)
            .frame(width: 36, height: 5)
            .padding(.top, CC.space.xs)
            .padding(.bottom, CC.space.xs)
            .frame(maxWidth: .infinity)
            .accessibilityHidden(true)
    }

    private func closeButton(_ action: @escaping () -> Void) -> some View {
        Button {
            CCHaptic.light.fire()
            action()
        } label: {
            CCIcon("xmark", size: CC.size.iconSm, weight: .bold, relativeTo: .footnote)
                .foregroundStyle(CC.text.secondary)
                // The circle scales on the *same* ramp as the cross inside it.
                // Measured failure this replaces: at `accessibility-extra-large`
                // the 13pt cross grew past the fixed 28pt ring and the button
                // read as a stray glyph on a smudge. See `ccGlyphContainer`.
                .ccGlyphContainer(CC.size.glyph, relativeTo: .footnote)
                .ccHitTarget()
        }
        .buttonStyle(CCCloseButtonStyle())
        .accessibilityLabel(closeLabel)
    }
}

private struct CCCloseButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .ccPressScale(configuration.isPressed, scale: 0.92)
    }
}

// MARK: - Convenience initialisers

extension CCSheetChrome where Trailing == EmptyView {
    init(
        _ title: String,
        subtitle: String? = nil,
        onClose: (() -> Void)? = nil,
        closeLabel: String = "Close",
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.title = title
        self.subtitle = subtitle
        self.onClose = onClose
        self.closeLabel = closeLabel
        self.trailing = { EmptyView() }
        self.content = content
    }
}

// MARK: - Action bar

/// The bar that sits at the bottom of a sheet or a scroll view and holds the
/// decision. Hairline on top, `surfaceRaised` fill — never `.bar`, whose
/// material is the system's and lightens under a light-mode trait that this app
/// forces off but cannot rely on inside every UIKit-hosted subview.
///
/// **`surfaceRaised`, with 16 / 20 / 12 padding.** It shipped `surface`
/// #0A0A0A with 12 / 16 / 8 — the same fill as the scroll content it is pinned
/// over, so the bar carrying the irreversible action separated from the document
/// by a hairline and nothing else, and every one of its three paddings was 4pt
/// short. The luminance ladder is the whole separation mechanism here: a bar is a
/// block *nested over* a screen, which is exactly what `surfaceRaised` means.
struct CCActionBar<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(spacing: 0) {
            CCHairline()
            VStack(spacing: CC.space.sm) {
                content()
            }
            .padding(.horizontal, CC.space.lg)
            .padding(.top, CC.space.md)
            .padding(.bottom, CC.space.sm)
        }
        .background(CC.color.surfaceRaised)
    }
}

// MARK: - The decision pair

/// The two-button decision row: **40 : 60, 12pt gap**, and no parameter
/// that says otherwise.
///
/// `Deny` is narrower than `Allow` because the two are not equals. has HIGH
/// invert the hierarchy — Deny becomes the filled white primary, Allow the
/// bordered hold-target — and *area* is one of the three variables specified to
/// carry that inversion. Shipped equal, the one place the design deliberately
/// says "this side is safer" said it in a third of the language it specified.
///
/// **Why this is a `Layout` and not a measured width.** It shipped as a
/// `GeometryReader` in the action bar's `.background` writing a `PreferenceKey`
/// into `@State`, with `denyWidth` returning `nil` until the measurement landed
/// so that "the first frame is an even split rather than a zero-width button".
/// The arithmetic was right — `max(0, content * 0.4)` — and both buttons still
/// measured exactly **179.00pt = 358/2** four seconds after launch, at both
/// sizes, on the card and in the sheet: the signature of `denyWidth == nil`. The
/// first frame was the only frame. A ratio does not need to be measured to be
/// known, so this asks SwiftUI for the width it is *already* being given and
/// divides it during layout. There is no `@State`, no preference, no first-frame
/// flash and nothing to fail to propagate — on a 370pt bar the split is
/// **143.20 / 214.80** on frame one, forever.
///
/// At accessibility sizes it stacks, because two 52pt buttons and an AX5 label
/// cannot share 361pt — the same switch `CCAdaptiveStack` makes everywhere else.
struct CCActionPair<Deny: View, Allow: View>: View {
    private let deny: () -> Deny
    private let allow: () -> Allow

    @Environment(\.dynamicTypeSize) private var typeSize

    init(@ViewBuilder deny: @escaping () -> Deny, @ViewBuilder allow: @escaping () -> Allow) {
        self.deny = deny
        self.allow = allow
    }

    var body: some View {
        if typeSize.isAccessibilitySize {
            VStack(spacing: CC.space.sm) {
                deny()
                allow()
            }
        } else {
            CCSplitLayout(leadingFraction: 0.4, spacing: CC.space.sm) {
                deny()
                allow()
            }
        }
    }
}

/// Divides the width it is given between exactly two subviews, gap first.
///
/// The gap comes out before the split so the ratio describes the *buttons*
/// rather than the buttons-plus-air: 143.20 / 214.80 on a 370pt bar.
private struct CCSplitLayout: Layout {
    let leadingFraction: CGFloat
    let spacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) -> CGSize {
        let width = resolvedWidth(proposal: proposal, subviews: subviews)
        let height =
            zip(subviews, widths(in: width, count: subviews.count))
            .map { subview, width in
                subview.sizeThatFits(ProposedViewSize(width: width, height: proposal.height)).height
            }
            .max() ?? 0
        return CGSize(width: width, height: height)
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Void
    ) {
        var x = bounds.minX
        for (subview, width) in zip(subviews, widths(in: bounds.width, count: subviews.count)) {
            subview.place(
                at: CGPoint(x: x, y: bounds.minY),
                anchor: .topLeading,
                proposal: ProposedViewSize(width: width, height: bounds.height))
            x += width + spacing
        }
    }

    /// Two subviews split by the ratio; any other count splits evenly, because a
    /// ratio that names a *leading* share has no meaning for three.
    private func widths(in total: CGFloat, count: Int) -> [CGFloat] {
        guard count > 0 else { return [] }
        let available = max(0, total - spacing * CGFloat(count - 1))
        guard count == 2 else {
            return Array(repeating: available / CGFloat(count), count: count)
        }
        let leading = available * leadingFraction
        // The trailing share is the *remainder*, never `available * (1 - f)`:
        // subtracting keeps the two widths and the gap summing to the bar
        // exactly, with no rounding crumb left on the trailing edge.
        return [leading, available - leading]
    }

    /// An ideal-size query proposes no width. Answering with the subviews' own
    /// widths keeps the pair honest inside a `fixedSize` or a sizing pass,
    /// rather than collapsing to SwiftUI's 10pt fallback.
    private func resolvedWidth(proposal: ProposedViewSize, subviews: Subviews) -> CGFloat {
        if let width = proposal.width, width.isFinite, width > 0 { return width }
        let intrinsic = subviews.reduce(into: CGFloat.zero) { total, subview in
            total += subview.sizeThatFits(.unspecified).width
        }
        return intrinsic + spacing * CGFloat(max(0, subviews.count - 1))
    }
}
