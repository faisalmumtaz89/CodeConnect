import SwiftUI

// =============================================================================
//  CCRow — the full-width tappable list row.
// =============================================================================

/// How much air a row has around its text.
///
/// Two values, not a number, so the two row rhythms in the product cannot drift
/// into five.
enum CCRowDensity {
    /// 12pt of padding on a 64pt floor. Settings rows, sheet lists — chrome
    /// you read once, at arm's reach, having gone looking for it.
    case compact
    /// 16pt on a 76pt floor: the fleet row, exactly
    /// `16 + 22 + 4 + 18 + 16`. Three lines is normal here and the row is read
    /// at 2am without having gone looking for anything.
    case comfortable

    var verticalPadding: CGFloat {
        self == .compact ? CC.space.sm : CC.space.md
    }

    var minHeight: CGFloat {
        self == .compact ? CC.size.rowMin : CC.size.rowRoomy
    }
}

/// 64pt minimum, the entire row is the target, and the pressed state is a
/// one-step luminance change rather than a highlight colour.
///
/// "Nothing important is a small chevron": the chevron here is a hint about
/// where the row goes, never the thing you have to hit.
///
/// **Three text lines, not two.** A fleet row is `place` / `activity` /
/// `identity · risk · capability`, and the third line is a *composition* — a
/// `CCIdentity`, badges, a right-aligned note — not a string. Before the `meta`
/// slot existed, the fleet row could not be a `CCRow` at all and was rebuilt
/// from primitives, which is how one product ends up with two rows that lighten
/// differently under a finger.
///
/// **`meta` gets the whole row.** It shipped as the third child of the title's
/// own column, so the trailing accessory took its width too: on the fleet, a
/// `HIGH` badge and a `1m02s` clock cost the line below them ~90pt and
/// `git push --force origin ma…` lost the word that says *which branch*, with
/// the row visibly empty to its right. `main` and `master` differing by five
/// characters nobody was shown is the exact failure this product exists to
/// prevent, so line three now spans from the content column to the row's own
/// trailing edge, under the accessory rather than beside it.
struct CCRow<Leading: View, Trailing: View, Meta: View>: View {
    let title: String
    /// Monospace title — for a title that is an *identifier* rather than a
    /// name: an unrecognized model id, a token, a path. The same rule
    /// `CCField.isMono` encodes for input, applied to display: machine
    /// strings wear machine type, byte-for-byte.
    var titleIsMono: Bool = false
    var subtitle: String?
    /// **Which end of the title identifies it.** `.middle` is mandatory on the
    /// fleet: generated folder names like
    /// `ccsoak-tail-54568-1785466205` are identical for forty characters and
    /// differ only in the tail.
    ///
    /// Head and middle truncation are statements that the string's *ends*
    /// identify it, which is only true on one line — so either mode drops the
    /// title to a single line below the accessibility sizes. At and above them
    /// the title wraps instead and nothing is truncated at all.
    var titleTruncation: Text.TruncationMode = .tail
    /// Two lines, not one: a truncated project name at AX3 is the difference
    /// between the right agent and the wrong one. `nil` never truncates.
    var subtitleLineLimit: Int? = 2
    /// Monospace metadata — a uid tail, an age, a duration. Always monospace,
    /// always present where a fact is shown. Use the `meta` **slot** when line
    /// three carries components rather than a string.
    var showsChevron: Bool = true
    var separator: Bool = true
    var density: CCRowDensity = .compact
    /// An inactive row — the `ENDED` band. The title drops to `textTertiary`
    /// rather than the row being dimmed as a whole, which is forbidden.
    var isDimmed: Bool = false
    var disabledReason: CCDisabledReason?
    /// What VoiceOver reads instead of `title, subtitle`. Needed whenever the
    /// `meta` slot carries facts the row cannot read back out of it.
    var accessibilityLabelText: String?
    /// A row with no action is a display row: no press state, no chevron, no
    /// button trait.
    var action: (() -> Void)?

    private let leading: () -> Leading
    private let trailing: () -> Trailing
    private let meta: () -> Meta

    @Environment(\.dynamicTypeSize) private var typeSize
    /// The height of the title's first line, scaled. Everything in the gutter
    /// and the trailing edge centres on *this*, not on the row — see
    /// `content(pressed:)`.
    @ScaledMetric(relativeTo: .headline) private var titleLine: CGFloat = 21

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised
    /// memberwise one, and the two collide over the private closure properties.
    init(
        _ title: String,
        titleIsMono: Bool = false,
        subtitle: String? = nil,
        titleTruncation: Text.TruncationMode = .tail,
        subtitleLineLimit: Int? = 2,
        showsChevron: Bool = true,
        separator: Bool = true,
        density: CCRowDensity = .compact,
        isDimmed: Bool = false,
        disabledReason: CCDisabledReason? = nil,
        accessibilityLabelText: String? = nil,
        action: (() -> Void)? = nil,
        @ViewBuilder leading: @escaping () -> Leading,
        @ViewBuilder trailing: @escaping () -> Trailing,
        @ViewBuilder meta: @escaping () -> Meta
    ) {
        self.title = title
        self.titleIsMono = titleIsMono
        self.subtitle = subtitle
        self.titleTruncation = titleTruncation
        self.subtitleLineLimit = subtitleLineLimit
        self.showsChevron = showsChevron
        self.separator = separator
        self.density = density
        self.isDimmed = isDimmed
        self.disabledReason = disabledReason
        self.accessibilityLabelText = accessibilityLabelText
        self.action = action
        self.leading = leading
        self.trailing = trailing
        self.meta = meta
    }

    var body: some View {
        Group {
            if let action {
                Button {
                    // No haptic — row press-in and release are visual only.
                    // A fleet of rows that each buzz on the way past is
                    // the fairground the calm palette exists to avoid.
                    action()
                } label: {
                    content(pressed: false)
                }
                .buttonStyle(CCRowButtonStyle(separator: separator))
                .disabled(disabledReason != nil)
                .accessibilityHint(disabledReason?.text ?? "")
            } else {
                content(pressed: false)
                    .background(CC.color.surface)
                    .overlay(alignment: .bottom) {
                        if separator { CCHairline() }
                    }
            }
        }
        // One element per row, not eight. A row is a single thing to a reader —
        // its dot, title, activity, identity and badges are one sentence — and
        // merging them is what stops VoiceOver making you swipe eight times to
        // pass a session you did not want. It is also what keeps the
        // accessibility tree small enough to answer a snapshot request on a
        // fleet of two dozen.
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityAddTraits(action != nil ? .isButton : [])
    }

    private func content(pressed: Bool) -> some View {
        Group {
            if typeSize.isAccessibilitySize {
                // At AX sizes the trailing accessory drops below the text.
                //
                // Measured failure this replaces: a `CCBadge` is `fixedSize`,
                // so at AX5 "BLOCKED ×3" claims ~250pt of a 329pt content
                // width and the flexible title column collapses to about ten
                // points — one character per line, straight down the row. The
                // badge has to leave the line entirely; there is no width to
                // negotiate over.
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    HStack(alignment: .top, spacing: CC.space.sm) {
                        leading().frame(minHeight: titleLine)
                        textBlock
                    }
                    if Trailing.self != EmptyView.self || (showsChevron && action != nil) {
                        HStack(spacing: CC.space.sm) {
                            trailing()
                            Spacer(minLength: 0)
                            chevron
                        }
                    }
                }
            } else {
                horizontalContent
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, density.verticalPadding)
        .frame(minHeight: density.minHeight)
        .frame(maxWidth: .infinity, alignment: .leading)
        // The whole rectangle, including the gaps between its children — a row
        // that only responds where there happens to be a glyph is a row that
        // feels broken at 2am.
        .contentShape(Rectangle())
    }

    /// `.top`, with every satellite centred inside one title-line's height.
    ///
    /// Centring the whole HStack instead — the obvious version — floats the
    /// status dot down beside the *subtitle* on any row that wraps to three
    /// lines, so a column of rows has its dots at three different heights. The
    /// dot belongs to the title; it aligns to the title.
    ///
    /// The accessory sits on the **title's** line, not on the text block's, so
    /// `meta` below it runs the full measure. See the type's own note: a badge
    /// and a clock stealing 90pt from line three is how a command lost its
    /// branch name.
    private var horizontalContent: some View {
        HStack(alignment: .top, spacing: CC.space.sm) {
            leadingSlot
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                HStack(alignment: .top, spacing: CC.space.sm) {
                    titleBlock
                    trailing()
                        .frame(minHeight: titleLine)
                    chevron
                        .frame(minHeight: titleLine)
                }
                metaBlock
            }
        }
    }

    /// The gutter: **8pt of layout, whatever the mark's own width**, centred.
    ///
    /// The slot took its width from the mark, so the content column moved
    /// depending on which mark a row happened to carry — measured inside one
    /// Settings card, a row led by a `CCStatusDot` put its title on **52.00** and
    /// a row led by a `CCIcon` put its title on **65.33**. Two text edges, 13.33pt
    /// apart, in a single card — precisely the third edge the two-edge rule
    /// forbids. Reserving the dot's width and letting anything larger bleed
    /// symmetrically about the same axis pins the column: the title is on 52
    /// whether the row is led by a dot, a glyph or an index badge. This is the
    /// construction `CCStepRow` already documents for its index badge and
    /// `CCSectionHeader` for its dot.
    ///
    /// Horizontal form only. At accessibility sizes the row switches to the
    /// stacked layout, where the gutter is abandoned rather than defended — a
    /// mark scaled to ~38pt centred on an 8pt column reaches the first letter of
    /// the title, which is the failure `CCStepRow` measured and describes.
    ///
    /// A row with no mark keeps its old edge rather than reserving a column for
    /// nothing; moving every leading-less row in the product is a separate
    /// change with its own render pass, and it is not what was measured.
    @ViewBuilder
    private var leadingSlot: some View {
        if Leading.self != EmptyView.self {
            leading()
                .frame(width: CC.size.dot)
                .frame(minHeight: titleLine)
        }
    }

    @ViewBuilder
    private var chevron: some View {
        if showsChevron, action != nil {
            CCIcon("chevron.right", size: CC.size.iconSm, weight: .semibold)
                .foregroundStyle(CC.text.tertiary)
        }
    }

    /// The whole text column, for the accessibility layout — where the accessory
    /// has already left the line and there is nothing to share the width with.
    private var textBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            titleBlock
            metaBlock
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// Lines one and two — the only ones that share their line with an
    /// accessory.
    private var titleBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            Text(title)
                .ccType(titleIsMono ? CC.type.monoSmall : CC.type.headline)
                .foregroundStyle(isDimmed ? CC.text.tertiary : CC.text.primary)
                .lineLimit(titleLineLimit)
                .truncationMode(titleTruncation)
                .fixedSize(horizontal: false, vertical: true)

            if let subtitle {
                // `footnote` — the token for row subtitles and helper text.
                // `callout` here made the activity line compete with the place
                // name it qualifies, and put a 2-line row at 80pt rather than
                // the 76pt a comfortable row is built on.
                Text(subtitle)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .lineLimit(typeSize.isAccessibilitySize ? nil : subtitleLineLimit)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// Line three and the disabled reason: the two things that get the row's
    /// full measure, because neither of them is ever beside an accessory.
    @ViewBuilder
    private var metaBlock: some View {
        // Deliberately no `.frame(maxWidth: .infinity)`: the `meta` slot is
        // usually absent, and a frame around an absent view is a real
        // zero-height child that the stack still puts 4pt of spacing around.
        // Every row in the product would have grown 4pt for a line it does not
        // draw. The slot's own content claims the width — a `Text` takes the
        // measure it is proposed, and a composed line ends in a `Spacer`.
        meta()

        // Drawn here rather than by `.ccDisabled` so it sits inside the
        // row's own 16pt padding. The generic modifier has no idea what
        // its container's inset is, and in a `padding: 0` card its
        // reason ends up flush against the card's border.
        if let disabledReason {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                CCIcon(
                    "exclamationmark.circle.fill", size: 11, weight: .semibold,
                    relativeTo: .caption
                )
                .foregroundStyle(CC.color.warning)
                Text(disabledReason.text)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.color.warning)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.top, CC.space.xxs)
            // The row's own hint already carries this string; leaving
            // it visible to VoiceOver too makes `.combine` read the
            // reason twice.
            .accessibilityHidden(true)
        }
    }

    private var titleLineLimit: Int? {
        // Never truncate at accessibility sizes — wrap instead.
        guard !typeSize.isAccessibilitySize else { return nil }
        return titleTruncation == .tail ? 2 : 1
    }

    private var accessibilityLabel: String {
        if let accessibilityLabelText { return accessibilityLabelText }
        return [title, subtitle].compactMap { $0 }.joined(separator: ", ")
    }
}

// MARK: - The string form of line three

/// `meta` as a plain string — a uid tail, an age, a duration.
///
/// A view rather than an `if let` inside `CCRow` so that the string form and
/// the composed form are the *same slot*, and a row cannot end up with two
/// third lines.
struct CCRowMeta: View {
    let text: String?

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        if let text {
            Text(text)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                // Never truncate an identifier. At AX sizes `cc-1 · K76F46
                // · 12s` needs two lines and gets them.
                .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

// MARK: - Convenience initialisers

extension CCRow where Meta == CCRowMeta {
    init(
        _ title: String,
        titleIsMono: Bool = false,
        subtitle: String? = nil,
        meta: String? = nil,
        titleTruncation: Text.TruncationMode = .tail,
        subtitleLineLimit: Int? = 2,
        showsChevron: Bool = true,
        separator: Bool = true,
        density: CCRowDensity = .compact,
        isDimmed: Bool = false,
        disabledReason: CCDisabledReason? = nil,
        accessibilityLabelText: String? = nil,
        action: (() -> Void)? = nil,
        @ViewBuilder leading: @escaping () -> Leading,
        @ViewBuilder trailing: @escaping () -> Trailing
    ) {
        self.init(
            title, titleIsMono: titleIsMono, subtitle: subtitle,
            titleTruncation: titleTruncation,
            subtitleLineLimit: subtitleLineLimit, showsChevron: showsChevron,
            separator: separator, density: density,
            isDimmed: isDimmed, disabledReason: disabledReason,
            accessibilityLabelText: accessibilityLabelText
                ?? [title, subtitle, meta].compactMap { $0 }.joined(separator: ", "),
            action: action,
            leading: leading, trailing: trailing, meta: { CCRowMeta(text: meta) })
    }
}

extension CCRow where Trailing == EmptyView, Meta == CCRowMeta {
    init(
        _ title: String,
        subtitle: String? = nil,
        meta: String? = nil,
        titleTruncation: Text.TruncationMode = .tail,
        subtitleLineLimit: Int? = 2,
        showsChevron: Bool = true,
        separator: Bool = true,
        density: CCRowDensity = .compact,
        isDimmed: Bool = false,
        disabledReason: CCDisabledReason? = nil,
        accessibilityLabelText: String? = nil,
        action: (() -> Void)? = nil,
        @ViewBuilder leading: @escaping () -> Leading
    ) {
        self.init(
            title, subtitle: subtitle, meta: meta, titleTruncation: titleTruncation,
            subtitleLineLimit: subtitleLineLimit, showsChevron: showsChevron,
            separator: separator, density: density,
            isDimmed: isDimmed, disabledReason: disabledReason,
            accessibilityLabelText: accessibilityLabelText, action: action,
            leading: leading, trailing: { EmptyView() })
    }
}

extension CCRow where Leading == EmptyView, Trailing == EmptyView, Meta == CCRowMeta {
    init(
        _ title: String,
        subtitle: String? = nil,
        meta: String? = nil,
        titleTruncation: Text.TruncationMode = .tail,
        subtitleLineLimit: Int? = 2,
        showsChevron: Bool = true,
        separator: Bool = true,
        density: CCRowDensity = .compact,
        isDimmed: Bool = false,
        disabledReason: CCDisabledReason? = nil,
        accessibilityLabelText: String? = nil,
        action: (() -> Void)? = nil
    ) {
        self.init(
            title, subtitle: subtitle, meta: meta, titleTruncation: titleTruncation,
            subtitleLineLimit: subtitleLineLimit, showsChevron: showsChevron,
            separator: separator, density: density,
            isDimmed: isDimmed, disabledReason: disabledReason,
            accessibilityLabelText: accessibilityLabelText, action: action,
            leading: { EmptyView() })
    }
}

// MARK: - Style

private struct CCRowButtonStyle: ButtonStyle {
    let separator: Bool

    func makeBody(configuration: Configuration) -> some View {
        CCRowSurface(
            isPressed: configuration.isPressed, separator: separator,
            label: configuration.label)
    }
}

private struct CCRowSurface: View {
    let isPressed: Bool
    let separator: Bool
    let label: ButtonStyleConfiguration.Label

    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        label
            // Deliberately NOT dimmed. Never dim a disabled control with
            // `.opacity(0.5)` — dimmed text fails contrast and reads as a
            // render bug. The reason drawn underneath by `ccDisabled` is what
            // communicates the state.
            .background(
                isPressed && isEnabled ? CCSurfaceLevel.surface.pressed : CC.color.surface)
            .overlay(alignment: .bottom) {
                if separator { CCHairline() }
            }
            // Rows do not scale. A 64pt block contracting under the finger
            // reads as the list itself flinching; the fill change is enough.
            .ccAnimation(CC.motion.micro, value: isPressed)
    }
}
