import SwiftUI

// =============================================================================
//  CCCard / CCSectionHeader — the two containers everything sits inside.
// =============================================================================

/// `surface` on `bg`, hairline, radius `lg`. Optional header and footer slots,
/// divided from the body by full-bleed hairlines.
///
/// **Every card in the product is `surface`, and there is no parameter that
/// says otherwise.** Three fills shipped for one idea — `#000000` on the Deck,
/// `#0A0A0A` on Fleet, `#131313` on Session Detail's NEEDS YOU — and the Deck's
/// was flush with the display bezel, so a 1pt hairline was drawn on the phone's
/// own edge and the "card" had no surface identity at all. The luminance ladder
/// only means something if each rung means one thing: `surface` is a card,
/// `surfaceRaised` is a block *nested inside* one (a mono block, a count chip,
/// an accessory bar), which is exactly how the diff grid already uses it.
///
/// The card does not cast a shadow, ever. "Hairlines, not fills": depth in this
/// product is a one-step luminance change plus an edge, and a shadow on a
/// near-black surface is just a smudge.
struct CCCard<Header: View, Content: View, Footer: View>: View {
    /// The inset on the trailing and vertical edges — and the *gutter* on the
    /// leading one, which `contentColumn` then steps out of.
    ///
    /// `0` hands the edges to the content, which is what a card full of `CCRow`s
    /// wants: the row draws both columns itself and the separator between two of
    /// them runs the full card width.
    var padding: CGFloat = CC.space.md
    /// Whether the leading edge steps out to the **content column**.
    ///
    /// On by default, because the default content of a card is something to
    /// read. A card padded 16 puts its text on 32 — the gutter, where the dots
    /// and section labels of every card around it live — so one screen shipped
    /// four different text edges and two screens carried a hand-written shim at
    /// every call site to fix it. With this on, a paragraph in a card begins on
    /// the same 52 as the title of a `CCRow` in the card beside it.
    ///
    /// Off for a card whose content is **centred**: a centred block wants four
    /// even edges, not a spine, and a 20pt step on one side of it is a 10pt
    /// error in the middle.
    var contentColumn: Bool = true
    var radius: CGFloat = CC.radius.lg
    var border: Color? = CC.color.border

    private let header: () -> Header
    private let content: () -> Content
    private let footer: () -> Footer

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised
    /// memberwise one, and the two collide over the private closure properties.
    init(
        padding: CGFloat = CC.space.md,
        contentColumn: Bool = true,
        radius: CGFloat = CC.radius.lg,
        border: Color? = CC.color.border,
        @ViewBuilder header: @escaping () -> Header,
        @ViewBuilder content: @escaping () -> Content,
        @ViewBuilder footer: @escaping () -> Footer
    ) {
        self.padding = padding
        self.contentColumn = contentColumn
        self.radius = radius
        self.border = border
        self.header = header
        self.content = content
        self.footer = footer
    }

    /// A card that pads nothing is not running a column either: its content owns
    /// both edges, and adding 20 to zero would put a row's separator 20pt short
    /// of the card it is dividing.
    private var leadingPadding: CGFloat {
        padding > 0 && contentColumn ? padding + CCColumn.gap : padding
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if Header.self != EmptyView.self {
                header()
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.leading, leadingPadding)
                    .padding(.trailing, padding)
                    .padding(.vertical, CC.space.sm)
                CCHairline()
            }

            content()
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.leading, leadingPadding)
                .padding(.trailing, padding)
                .padding(.vertical, padding)

            if Footer.self != EmptyView.self {
                CCHairline()
                footer()
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.leading, leadingPadding)
                    .padding(.trailing, padding)
                    .padding(.vertical, CC.space.sm)
            }
        }
        // Says how far the card has already stepped its content out, so a
        // component that owns its own column — `CCSectionHeader`, `CCFactRow` —
        // adds the remainder rather than the whole step. Without this, a section
        // header in a card's header slot would land on 72 the day the header
        // started placing itself, and the kit would have shipped the exact
        // collision it exists to prevent.
        .ccColumnInset(leadingPadding)
        // A card that is *not* running a column is running a centred block, and
        // a centred block has no gutter for a nested surface to hang its border
        // into. See `CCColumn.hang(from:)`.
        .ccCentredContent(!contentColumn)
        .ccSurface(.surface, radius: radius, border: border)
    }
}

extension CCCard where Header == EmptyView, Footer == EmptyView {
    init(
        padding: CGFloat = CC.space.md,
        contentColumn: Bool = true,
        radius: CGFloat = CC.radius.lg,
        border: Color? = CC.color.border,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.padding = padding
        self.contentColumn = contentColumn
        self.radius = radius
        self.border = border
        self.header = { EmptyView() }
        self.content = content
        self.footer = { EmptyView() }
    }
}

extension CCCard where Footer == EmptyView {
    init(
        padding: CGFloat = CC.space.md,
        contentColumn: Bool = true,
        radius: CGFloat = CC.radius.lg,
        border: Color? = CC.color.border,
        @ViewBuilder header: @escaping () -> Header,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.padding = padding
        self.contentColumn = contentColumn
        self.radius = radius
        self.border = border
        self.header = header
        self.content = content
        self.footer = { EmptyView() }
    }
}

// MARK: - Section header

/// `micro` label, optional count, optional action. Every screen uses one — a
/// bare heading in body type is what the type scale exists to prevent.
///
/// **It puts its own label on the content column, and there is no parameter that
/// says otherwise.** A screen gets two left edges and never a third: x=32 is a
/// non-text gutter for marks, glyphs and index badges; every text run —
/// section-header labels included — starts on x=52. That rule was settled after
/// this component shipped, and eleven headers
/// were measured off the column: ten at x=32.00 and one at x=16.00, against the
/// 52.00 that every row they name held perfectly. Six sites carried
/// `.padding(.leading, CCColumn.gutter)`, two carried `.padding(.horizontal,
/// CC.space.md)` inside an already-16pt page, and one carried nothing at all —
/// three ways to be wrong and no way for a call site to be *told* it was.
///
/// So the number moved into the component. **Place a section header flush** —
/// no leading inset, no horizontal inset. It owns 36 on the leading edge (52 on
/// screen) and `CC.space.md` on the trailing one, which is what puts a trailing
/// note on 370 rather than 386. A header nested in a container that has already
/// stepped out to the content column — a `CCCard` header slot — adds nothing and
/// still lands on 52, because the container declares its inset through
/// `.ccColumnInset(_:)` and the header asks for the remainder rather than
/// assuming it starts at the card's edge.
struct CCSectionHeader: View {
    let title: String
    var count: Int?
    /// The **leading dot**: the Blocked band, and nothing else so far. The
    /// only section header in the product that takes colour, and it takes it as
    /// a dot — never by colouring the word itself.
    var dotColor: Color?
    /// Only `blocked` and `connecting` pulse.
    var dotPulses: Bool = false
    /// The **trailing note** — `OBSERVE ONLY` on a band where every row is
    /// observe-only. `micro` `textTertiary`: it qualifies the label, so it must
    /// not out-weigh it.
    var note: String?
    /// Makes the note tappable, so a four-character claim can be expanded into
    /// the sentence behind it. 44pt of finger, a press state, and no glyph.
    var noteAction: (() -> Void)?
    var actionTitle: String?
    var action: (() -> Void)?

    init(
        _ title: String,
        count: Int? = nil,
        dotColor: Color? = nil,
        dotPulses: Bool = false,
        note: String? = nil,
        noteAction: (() -> Void)? = nil,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil
    ) {
        self.title = title
        self.count = count
        self.dotColor = dotColor
        self.dotPulses = dotPulses
        self.note = note
        self.noteAction = noteAction
        self.actionTitle = actionTitle
        self.action = action
    }

    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.ccColumnInset) private var columnInset
    /// Mirrors `CCStatusDot`'s own ramp and ceiling, so the offset that hangs
    /// the dot in the gutter tracks the dot it is hanging.
    @ScaledMetric(relativeTo: .footnote) private var scaledDot: CGFloat = CC.size.dot

    /// How far back from the label's leading edge the dot sits, so that it lands
    /// **centred on the 8pt gutter slot** — 32→40 on screen.
    ///
    /// Measured back rather than forward: `content − gutter − dot/2` is the slot
    /// centre (16 inside the card), and half the dot's own width puts its centre
    /// there. A mark that outgrows the 8pt slot therefore bleeds *symmetrically*
    /// about the gutter axis instead of pushing the label, which is what keeps
    /// the content column on 52 at AX5 as well as at L. The previous form —
    /// `dot + 8` — pinned the gap instead of the column, which was correct only
    /// while the label itself sat in the gutter.
    private var dotOffset: CGFloat {
        let width = min(scaledDot, CC.size.dot * CC.size.dotMaxScale)
        return -(CCColumn.content - CCColumn.gutter - CC.size.dot / 2 + width / 2)
    }

    var body: some View {
        // Written out rather than composed from `CCAdaptiveStack`, because the
        // horizontal form needs a `Spacer` and the vertical form must not have
        // one — a `Spacer` that survives the switch pushes the action to the
        // bottom of the screen at AX sizes.
        Group {
            if typeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    label
                    noteLabel
                    actionButton
                }
            } else {
                HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                    label
                    Spacer(minLength: CC.space.xs)
                    noteLabel
                    actionButton
                }
            }
        }
        // The component's own column, and the only place it is expressed.
        // `step(from:)` rather than `CCColumn.content` so that a header inside a
        // container which has already stepped out does not step out twice.
        //
        // **Scaled**, because the column it must agree with is scaled. `CCRow`
        // builds its content edge from `16 + scaled dot + 12`, so at AX5 the rows
        // sit at 59.33 while a constant 36 left their headers at 54.33 — Δ 5.00,
        // a fifth text edge appearing only at accessibility sizes. Mirrors the
        // dot's own ramp and ceiling, the same pair `dotOffset` uses, so header,
        // dot and row all move together or not at all.
        // **Only when there is something in the gutter to clear.**
        //
        // Stepping out unconditionally reserved room for a dot that no screen
        // draws — 17 production call sites, none passing `dotColor` — so every
        // section label in the app sat 36pt right of the text it labelled, with
        // nothing in the gap. Measured on the decision card: body on 16.00,
        // `EXACT COMMAND` on 52.00, inside one card.
        //
        // The rule itself was never wrong, only its premise. A header that hangs
        // a mark in the gutter still has to clear it, and still has to land on the
        // same column as the marked rows beneath it. A header with no mark belongs
        // on its container's own text edge, like everything else in the container.
        .padding(
            .leading,
            dotColor == nil ? 0 : CCColumn.step(from: columnInset, scaledDot: scaledDot))
        .padding(.trailing, CC.space.md)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(count.map { "\(title), \($0)" } ?? title)
        .accessibilityAddTraits(.isHeader)
    }

    private var label: some View {
        HStack(spacing: CC.space.xs) {
            // Uppercasing happens here, not in the type token: a token that
            // rewrites strings cannot be trusted with a session name.
            Text(title.uppercased())
                .ccType(CC.type.micro)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)

            if let count {
                CCCountChip(count)
            }
        }
        // The dot is a *graphic in the gutter*, not a member of the label row.
        // Drawn as an overlay and offset out of the label's leading edge so it
        // takes no layout width at all: the label holds its column and the dot
        // hangs back in the gutter beside it, so a band that gains or loses its
        // dot never moves its own text. It rides the
        // *label's* height rather than the header's, so an action button
        // wrapping below at AX5 cannot drag it down.
        .overlay(alignment: .leading) {
            if let dotColor {
                CCStatusDot(color: dotColor, size: CC.size.dot, pulses: dotPulses)
                    .offset(x: dotOffset)
                    .accessibilityHidden(true)
            }
        }
    }

    /// `micro` `textTertiary`.
    ///
    /// Deliberately not a `CCBadge` and not the ghost-button action slot. Both
    /// were tried on the fleet's band headers: the ghost button renders a
    /// `footnote`-semibold label that measured *louder* than the band name it
    /// qualifies, and an outlined badge draws a second bordered container onto
    /// a header whose whole job is to be quiet. A note that shouts over its own
    /// heading has inverted the hierarchy it exists to serve.
    @ViewBuilder
    private var noteLabel: some View {
        if let note {
            if let noteAction {
                Button(action: {
                    CCHaptic.light.fire()
                    noteAction()
                }) {
                    noteText
                        // 44pt of finger around an 11pt label, with no width
                        // padding — the note must not push the count off the
                        // trailing edge.
                        .ccHitTarget(minWidth: 0)
                }
                .buttonStyle(CCSectionNoteStyle())
                .accessibilityLabel(note)
                .accessibilityAddTraits(.isButton)
            } else {
                noteText
            }
        }
    }

    private var noteText: some View {
        Text(note?.uppercased() ?? "")
            .ccType(CC.type.micro)
            .foregroundStyle(CC.text.tertiary)
            .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
            .fixedSize(horizontal: false, vertical: true)
    }

    @ViewBuilder
    private var actionButton: some View {
        if let actionTitle, let action {
            // No negative trailing padding any more. It existed to pull a
            // borderless label's own inset back off the edge so the *label*
            // aligned with the content below it; a ghost button now draws a 1pt
            // edge of its own, and the thing that has to align is the edge.
            CCButton(actionTitle, variant: .ghost, size: .sm, action: action)
        }
    }
}

/// The note's press state: the label lifts a step rather than the header
/// growing a background. A section header has no surface of its own to lighten.
private struct CCSectionNoteStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(configuration.isPressed ? CC.text.primary : CC.text.tertiary)
            .ccAnimation(CC.motion.micro, value: configuration.isPressed)
    }
}
