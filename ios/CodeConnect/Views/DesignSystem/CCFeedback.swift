import SwiftUI

// =============================================================================
//  CCEmptyState / CCBanner — the two ways the product admits to a situation.
// =============================================================================

/// Glyph, title, body, optional action.
///
/// `ContentUnavailableView` is banned: it is styled by the system, which means
/// it is styled for a light-mode grouped list, and it puts prose in the wrong
/// place in the type scale.
struct CCEmptyState<Detail: View>: View {
    let glyph: String
    let title: String
    var message: String?
    /// `neutral` for a genuinely empty list; `warning` / `danger` when the
    /// emptiness is a *failure* rather than a starting point.
    var tone: CCTone = .neutral
    var actionTitle: String?
    var action: (() -> Void)?
    /// Why the way out is not available.
    ///
    /// An empty state whose single action is dead is the worst version of a
    /// control that will not work and will not say why: there is nothing else
    /// on the screen to try. Routed through `ccDisabled`, so the reason is
    /// *drawn*, not just
    /// hinted — a tap that silently does nothing is indistinguishable from an
    /// app that has stopped.
    var actionDisabledReason: CCDisabledReason?
    var secondaryActionTitle: String?
    var secondaryAction: (() -> Void)?

    private let detail: () -> Detail

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised
    /// memberwise one, and the two collide over the private closure property.
    init(
        glyph: String,
        title: String,
        message: String? = nil,
        tone: CCTone = .neutral,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil,
        actionDisabledReason: CCDisabledReason? = nil,
        secondaryActionTitle: String? = nil,
        secondaryAction: (() -> Void)? = nil,
        @ViewBuilder detail: @escaping () -> Detail
    ) {
        self.glyph = glyph
        self.title = title
        self.message = message
        self.tone = tone
        self.actionTitle = actionTitle
        self.action = action
        self.actionDisabledReason = actionDisabledReason
        self.secondaryActionTitle = secondaryActionTitle
        self.secondaryAction = secondaryAction
        self.detail = detail
    }

    var body: some View {
        VStack(spacing: CC.space.sm) {
            mark
                .padding(.bottom, CC.space.xxs)

            Text(title)
                .ccType(CC.type.title)
                .foregroundStyle(CC.text.primary)
                .multilineTextAlignment(.center)

            if let message {
                // `CCProse`: an empty state is where the product explains how to
                // leave it, and the way out is usually a command — `cc token`,
                // `cc pair --ssh`. Set as plain text those arrived wearing their
                // own backticks on the scanner-unsupported screen.
                CCProse(message, style: CC.type.callout, color: CC.text.secondary)
                    .multilineTextAlignment(.center)
                    // 50–75 characters is the readable measure; on a phone that
                    // is roughly this, and it stops the copy running edge to
                    // edge in landscape.
                    .frame(maxWidth: 320)
                    // Without this, a height-constrained container compresses the
                    // message to one line and truncates it rather than wrapping.
                    // Measured on the ended terminal at AX5: 307.7 × 51.3pt
                    // rendering `tmux detached....`, eating the daemon's second
                    // sentence — "The agent is still running on the Mac." A
                    // message that explains how to leave an empty state is the
                    // last string that can afford to be cut.
                    .fixedSize(horizontal: false, vertical: true)
            }

            detail()

            if actionTitle != nil || secondaryActionTitle != nil {
                VStack(spacing: CC.space.xs) {
                    if let actionTitle, let action {
                        // **`primary`.** `Open Settings`, `Pair by hand`, `Try
                        // again`, `Reconnect` are all this shape.
                        // It shipped `.secondary`, which drew the only way out
                        // of a dead end as a bordered transparent control, the
                        // same weight as the `ghost` beneath it. An empty state
                        // has exactly one action worth taking; that is the
                        // definition of a primary.
                        // `.lg` for the same reason it is `.primary`: primary
                        // actions are 52pt, and this is the one action on the
                        // screen. At `.md` it measured
                        // 206.3 × 44.0 with accessibility type inside it, while
                        // every other primary on the same screens is 52 at `L`
                        // and grows past 70 at AX5 — so the way out of a dead end
                        // was the smallest control on it.
                        CCButton(
                            actionTitle, variant: .primary, size: .lg,
                            disabledReason: actionDisabledReason, action: action)
                    }
                    if let secondaryActionTitle, let secondaryAction {
                        CCButton(
                            secondaryActionTitle, variant: .ghost, size: .md,
                            action: secondaryAction)
                    }
                }
                .padding(.top, CC.space.xs)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, CC.space.xl)
        .padding(.vertical, CC.space.xxl)
        // An empty state is a **centred** composition — mark, title, measure-
        // bounded message — so it has no leading gutter, and a nested surface in
        // the `detail` slot must not step out to a column or hang its border
        // into one. `cc claude` inside a 280pt block sits on the screen's centre
        // line; 12pt of hang would put it 6pt off it.
        .ccCentredContent()
        .accessibilityElement(children: .contain)
    }

    private var mark: some View {
        CCScreenMark(glyph: glyph, tone: tone)
    }
}

// MARK: - The screen's mark

/// The screen's mark: a 32pt glyph inside a 64pt `surfaceRaised` circle with a
/// 1pt border — not a bare glyph floating in the middle of the screen.
///
/// The ring is what makes the state read as a *placed object* rather than as a
/// rendering that failed halfway. It scales with the glyph
/// (`ccGlyphContainer`), so at AX5 it is still a ring around a mark and not a
/// mark that has climbed out of its ring.
///
/// The border takes the tone at 45% when there is one — the same 45% a
/// `CCBanner` uses — so a failure state's mark agrees with the banner that will
/// usually be beside it, and a neutral empty list stays monochrome.
///
/// **It is a component and not a modifier because the ramp is the whole
/// point.** `ccGlyphContainer` takes `relativeTo:` as a free parameter, and two
/// marks meaning *here is the thing this screen is about* were built with two
/// different answers: `CCEmptyState` on `.largeTitle`, the Terminal tab's
/// connect card on `.title2`. `.title2` grows without bound at accessibility
/// sizes and `.largeTitle` does not, so the two circles measured **58.67 / 61.66
/// at M, 63.33 / 63.33 at the default L, and 150.33 / 108.67 at AX5** — a 41.66pt
/// divergence, with one of them 37% of the screen's width, hiding behind the one
/// type size where the two ramps happen to cross. A shared *token* was never
/// enough: `CC.size.emptyGlyphCircle` was already on both sides. The ramp had to
/// be welded to it, which a component can do and a modifier's parameter cannot.
struct CCScreenMark: View {
    let glyph: String
    var tone: CCTone = .neutral
    /// The one ramp. Not a parameter — see the type's note.
    private static let ramp: Font.TextStyle = .largeTitle

    var body: some View {
        CCIcon(glyph, size: CC.size.emptyGlyph, weight: .light, relativeTo: Self.ramp)
            .foregroundStyle(tone == .neutral ? CC.text.tertiary : tone.color)
            .ccGlyphContainer(
                CC.size.emptyGlyphCircle,
                border: tone == .neutral ? CC.color.border : tone.color.opacity(0.45),
                relativeTo: Self.ramp)
            .accessibilityHidden(true)
    }
}

extension CCEmptyState where Detail == EmptyView {
    init(
        glyph: String,
        title: String,
        message: String? = nil,
        tone: CCTone = .neutral,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil,
        actionDisabledReason: CCDisabledReason? = nil,
        secondaryActionTitle: String? = nil,
        secondaryAction: (() -> Void)? = nil
    ) {
        self.init(
            glyph: glyph, title: title, message: message, tone: tone,
            actionTitle: actionTitle, action: action,
            actionDisabledReason: actionDisabledReason,
            secondaryActionTitle: secondaryActionTitle, secondaryAction: secondaryAction,
            detail: { EmptyView() })
    }
}

// MARK: - Banner

/// An ADDITION to the original component list.
///
/// The design language did not name a banner, but the product has eight call
/// sites for one — stale caches, gone-quiet links, truncated history, "already
/// resolved". The standing rule is that if a screen needs a new visual pattern,
/// it becomes a DesignKit component first. Eight screens needed it, so here it
/// is, rather than eight bespoke ones scattered across the app.
///
/// It is the component that admits to a limit of the app's own knowledge, so it
/// is deliberately not dismissible-by-default and never uses a full-saturation
/// fill that could be mistaken for a decorative highlight.
struct CCBanner: View {
    let title: String
    var message: String?
    var tone: CCTone = .warning
    var icon: String?
    var actionTitle: String?
    var action: (() -> Void)?

    init(
        _ title: String,
        message: String? = nil,
        tone: CCTone = .warning,
        icon: String? = nil,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil
    ) {
        self.title = title
        self.message = message
        self.tone = tone
        self.icon = icon
        self.actionTitle = actionTitle
        self.action = action
    }

    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.ccColumnInset) private var columnInset

    /// What the glyph row adds inside the banner's own 12pt inset to put the
    /// glyph on the **gutter** and the words on the **content column** — 32 and
    /// 52, the two edges every row and header in the product holds.
    ///
    /// Measured before this: glyph ink at **30.33** and text at **62.33**, so a
    /// banner sitting between two cards agreed with neither of them. The banner's
    /// own border is chrome and stays on the container's edge with an even 12pt
    /// inset all round — the two-edge rule governs text, not chrome; only what
    /// is written inside it moves.
    private var gutterStep: CGFloat {
        max(0, CCColumn.gutter - CC.space.sm - columnInset)
    }

    var body: some View {
        CCAdaptiveStack(
            horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.sm,
            horizontalAlignment: .leading, verticalAlignment: .top
        ) {
            HStack(alignment: .top, spacing: CC.space.sm) {
                CCIcon(icon ?? tone.defaultGlyph, size: CC.size.icon, weight: .semibold)
                    .foregroundStyle(tone.color)
                    // Optically centres the glyph on the title's cap height
                    // rather than on its line box, which sits it a shade low.
                    .padding(.top, 1)
                    // The gutter slot: **8pt of layout, whatever the glyph's own
                    // width**, centred — the construction `CCRow`, `CCStepRow`
                    // and `CCSectionHeader`'s dot overlay all use, so a banner's
                    // mark sits on the same 32–40 axis as a row's. At
                    // accessibility sizes the slot is abandoned rather than
                    // defended, exactly as those three do it: a glyph scaled past
                    // 40pt centred on an 8pt column reaches outside the banner's
                    // own border.
                    .frame(width: typeSize.isAccessibilitySize ? nil : CC.size.dot)

                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    // `badgeLabel` title + `footnote` detail. The title is
                    // the classification — the same role a badge's label plays,
                    // and now the same token, so it cannot read as a section
                    // heading for the sentence underneath it.
                    Text(title.uppercased())
                        .ccType(CC.type.badgeLabel)
                        .foregroundStyle(tone == .neutral ? CC.text.secondary : tone.color)
                        .fixedSize(horizontal: false, vertical: true)
                    if let message {
                        // `CCProse`: a banner's sentence is the product's main
                        // channel for "run this at the Mac", and every one of
                        // those strings names a command.
                        CCProse(message, style: CC.type.footnote, color: CC.text.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(.leading, gutterStep)

            if let actionTitle, let action {
                // `md` — 44pt, not the 36pt `sm`. A banner's action is often the
                // only way out of the state it is describing, so it gets a full
                // 44pt target.
                CCButton(actionTitle, variant: .secondary, size: .md, action: action)
                    .fixedSize()
            }
        }
        .padding(CC.space.sm)
        .frame(maxWidth: .infinity, alignment: .leading)
        // `surfaceRaised` fill with a 45% semantic border — NOT a 12% colour
        // wash. The wash reads as a highlight; a banner is not a highlight, it
        // is the app admitting to a limit of its own knowledge.
        .ccSurface(
            fill: CC.color.surfaceRaised, radius: CC.radius.md,
            border: tone == .neutral ? CC.color.border : tone.color.opacity(0.45))
        .accessibilityElement(children: .contain)
        .accessibilityLabel(
            CCInlineCode.plain([title, message].compactMap { $0 }.joined(separator: ". ")))
    }
}

// MARK: - The banner ladder

/// Where a banner sits in the one-banner priority ladder.
///
/// Lower raw value wins. The order is not arbitrary: it runs from "nothing
/// works and it is not coming back" down to "some history is missing".
enum CCBannerPriority: Int, Comparable, CaseIterable {
    case rejected = 0
    case offline = 1
    case stale = 2
    case cached = 3
    case gap = 4
    case truncated = 5

    static func < (lhs: CCBannerPriority, rhs: CCBannerPriority) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

/// One candidate for the screen's single banner slot.
struct CCBannerItem: Identifiable {
    let priority: CCBannerPriority
    let title: String
    var message: String?
    var tone: CCTone = .warning
    var icon: String?
    var actionTitle: String?
    var action: (() -> Void)?

    var id: Int { priority.rawValue }

    init(
        _ priority: CCBannerPriority,
        title: String,
        message: String? = nil,
        tone: CCTone = .warning,
        icon: String? = nil,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil
    ) {
        self.priority = priority
        self.title = title
        self.message = message
        self.tone = tone
        self.icon = icon
        self.actionTitle = actionTitle
        self.action = action
    }
}

/// **One banner. Ever.**
///
/// Screens hand this every fact that *could* warrant a banner and it renders
/// the highest-priority one, in silence. Making the slot do the choosing is
/// what makes "two stacked banners is a bug" structurally impossible rather
/// than a rule somebody has to remember on every screen.
///
/// Facts that lose the ladder are not lost — they still appear in their
/// per-element treatment: hollow dots for cached data, inline `CCGapMarker`s
/// for sequence gaps.
struct CCBannerSlot: View {
    let candidates: [CCBannerItem]

    init(_ candidates: [CCBannerItem?]) {
        self.candidates = candidates.compactMap { $0 }
    }

    var body: some View {
        if let winner = candidates.min(by: { $0.priority < $1.priority }) {
            CCBanner(
                winner.title, message: winner.message, tone: winner.tone,
                icon: winner.icon, actionTitle: winner.actionTitle, action: winner.action)
        }
    }
}

extension CCTone {
    /// The glyph a tone reaches for when a caller does not name one.
    var defaultGlyph: String {
        switch self {
        case .neutral: return "info.circle"
        case .success: return "checkmark.circle"
        case .info: return "info.circle"
        case .warning: return "exclamationmark.triangle"
        case .danger: return "exclamationmark.octagon"
        }
    }
}
