import SwiftUI
import UIKit

// =============================================================================
//  Primitives — the parts every CC component is assembled from.
//
//  Nothing here is a screen-level component. If two components need to look
//  identical in some respect, the reason lives in this file.
// =============================================================================

// MARK: - The two left edges

/// A screen gets exactly two vertical edges, and this is where they are written
/// down — **measured from a card's own leading edge**, because a card is what
/// every column in this product is drawn inside.
///
/// A card sits 16 from the screen. A `CCRow` inside it pads 16, so the gutter —
/// dots, glyphs, section labels — starts on **32**. The dot is 8 wide and the
/// row's own gap is 12, so text starts on **52**. Anything that is not a row and
/// still has to land on those numbers — a fact row, a card full of prose, a
/// band footer, a loading skeleton — reads them from here rather than guessing,
/// which is how one screen ended up with four content columns.
enum CCColumn {
    /// What the gutter costs: the dot, plus the gap after it. 20.
    ///
    /// The number a component adds to its container's inset to step from the
    /// gutter column out to the content column.
    static let gap: CGFloat = CC.size.dot + CC.space.sm
    /// Where a glyph or a dot sits inside a card: 16, which is 32 on screen.
    static let gutter: CGFloat = CC.space.md
    /// Where text sits inside a card: 36, which is 52 on screen.
    static let content: CGFloat = CC.space.md + gap

    /// What a component must **add** to land its own text on the content column,
    /// given how far its container has already inset it.
    ///
    /// This exists because "the component owns its column" and "the container
    /// owns its padding" are both true, and stacking them silently is how a
    /// label lands on 72. `CCCard` reports its leading inset through
    /// `.ccColumnInset(_:)`; a component that places text asks this for the
    /// remainder. A component nested in a container that has already stepped out
    /// adds nothing; the same component free-standing on a screen adds the whole
    /// 36. Neither call site is told which case it is in.
    static func step(from applied: CGFloat) -> CGFloat {
        max(0, content - applied)
    }

    /// The content column when the mark in the gutter has grown.
    ///
    /// `CCRow` composes its text edge as `gutter + scaled mark + gap`, so the
    /// column genuinely moves at accessibility sizes — it steps right only far
    /// enough to clear the mark. A component that places text against those rows
    /// has to move with them: a constant 36 left section headers at 54.33 while
    /// their own rows sat at 59.33, a fifth text edge that exists only at AX5.
    ///
    /// Pass the caller's own `@ScaledMetric` dot so the ramp is the one the mark
    /// is actually drawn on, and the ceiling matches `CCStatusDot`'s.
    static func content(scaledDot: CGFloat) -> CGFloat {
        let mark = min(scaledDot, CC.size.dot * CC.size.dotMaxScale)
        return CC.space.md + max(CC.size.dot, mark) + CC.space.sm
    }

    /// `step(from:)`, measured against the scaled column.
    static func step(from applied: CGFloat, scaledDot: CGFloat) -> CGFloat {
        max(0, content(scaledDot: scaledDot) - applied)
    }

    /// **Where a nested surface's own border sits** so that the text inside it
    /// lands on the container's text column — the corollary of the two-edge
    /// rule, and the number `CCMonoBlock` is built on.
    ///
    /// The two-edge rule governs *text*, not chrome. A nested surface (a mono
    /// block, a tinted band, an alarm card) pads its contents by `CC.space.sm`
    /// before it draws them, so its leading edge belongs one inner padding
    /// **left** of
    /// the column, hanging into the gutter: border 40, text 52. Monospace is
    /// never set hard against a border, and a container never pushes what is
    /// inside it onto a third text edge.
    ///
    /// - Parameter applied: the container's declared text column
    ///   (`.ccColumnInset(_:)`). `0` means nothing upstream has stepped out yet,
    ///   so the surface finds `content` for itself.
    /// - Returns: what the surface must **add** to its own leading edge. Usually
    ///   negative, which is the hang.
    static func hang(from applied: CGFloat) -> CGFloat {
        (applied > 0 ? 0 : content) - CC.space.sm
    }
}

/// A container whose content is **centred** rather than run down a column — an
/// empty state, a `CCCard(contentColumn: false)`.
///
/// There is no gutter on its leading edge to hang chrome into, and a 12pt step
/// on one side of a centred block is a 6pt error in the middle of it. A nested
/// surface inside one draws flush and keeps its own inner padding.
private struct CCCentredContentKey: EnvironmentKey {
    static let defaultValue: Bool = false
}

extension EnvironmentValues {
    var ccCentredContent: Bool {
        get { self[CCCentredContentKey.self] }
        set { self[CCCentredContentKey.self] = newValue }
    }
}

/// How far the enclosing container has already inset its content from the
/// leading edge a column is measured against.
///
/// Zero — the default — means "nobody has stepped out yet", which is correct for
/// a component sitting directly in a screen's 16pt margin.
private struct CCColumnInsetKey: EnvironmentKey {
    static let defaultValue: CGFloat = 0
}

extension EnvironmentValues {
    var ccColumnInset: CGFloat {
        get { self[CCColumnInsetKey.self] }
        set { self[CCColumnInsetKey.self] = newValue }
    }
}

extension View {
    /// Declares how far this container has already inset the content inside it,
    /// so a component that owns its own column does not add the step twice.
    func ccColumnInset(_ inset: CGFloat) -> some View {
        environment(\.ccColumnInset, inset)
    }

    /// Declares that this container centres its content, so a nested surface
    /// inside it draws flush instead of stepping out to a column and hanging its
    /// border back into a gutter that is not there.
    func ccCentredContent(_ centred: Bool = true) -> some View {
        environment(\.ccCentredContent, centred)
    }
}

// MARK: - Measured, and not measured

/// Whether a number was measured — and what an unmeasured one looks like.
///
/// **One rule, one em dash, every component that prints a value.** It exists
/// because the rule was written twice and the two copies disagreed: `CCFactRow`
/// drew an unmeasured `—` at `textDisabled` (2.53:1, correctly reading as
/// "nothing here"), while `CCStat` had no notion of the state at all and drew
/// the same em dash at `text` — 16.91:1, *pixel-identical in weight to the two
/// real measurements beside it*, on the strip whose whole job is to be trusted.
/// A reader scanning `MISSED DECISIONS —  ·  SEQ GAPS 0  ·  RECONNECTS 0` saw
/// three equally confident figures, one of which was a placeholder.
///
/// The fix is not a second `isUnmeasured` flag; it is one function both
/// components ask. Two components cannot render one state two ways if neither
/// of them owns the answer.
enum CCMeasured {
    /// What a value nobody measured renders as. **Never `0`** — `0` is a
    /// measurement, and printing it is a lie about state.
    static let mark = "—"

    /// A value is unmeasured when the caller says so **or when it is already
    /// the mark**.
    ///
    /// The second half is what stops the two components drifting apart again. A
    /// component that has to be *told* a visible em dash means "not measured"
    /// has a second, forgettable source of truth for one fact — and the call
    /// site that forgets is exactly the one that shipped. `CCStat("MISSED
    /// DECISIONS", value: "—")` is now self-evidently unmeasured whether or not
    /// anybody remembered the flag.
    static func isUnmeasured(_ value: String, flagged: Bool = false) -> Bool {
        flagged || value.trimmingCharacters(in: .whitespaces) == mark
    }

    /// The colour of a value. The **only** implementation of that rule.
    ///
    /// `textDisabled` here is the one position it is permitted in on a row: an
    /// em dash beside a full-contrast label is the "inactive component" case
    /// exactly, and the contrast is the message.
    static func color(_ value: String, tone: CCTone = .neutral, flagged: Bool = false) -> Color {
        if isUnmeasured(value, flagged: flagged) { return CC.text.disabled }
        return tone == .neutral ? CC.text.primary : tone.color
    }

    /// What VoiceOver reads. An em dash is not a word, and "dash" is not the
    /// fact.
    static func spoken(_ value: String, flagged: Bool = false) -> String {
        isUnmeasured(value, flagged: flagged) ? "not measured" : value
    }
}

// MARK: - Prose that names an identifier

/// Backtick markup, resolved.
///
/// **Backticks are markup, and markup never renders.** Strings arrive from the
/// wire, from `CCDisabledReason`s and from screen copy carrying `` `cc pair
/// --ssh` `` or `` `ios/CodeConnect/Net/Sender.swift` `` — and those spans are
/// commands and paths, which this product sets in monospace *always*, in prose
/// as much as anywhere else. Drawn as plain text they were doubly wrong: an
/// identifier in SF Pro, wearing two literal grave accents that the writer
/// meant as markup and VoiceOver reads aloud.
enum CCInlineCode {
    /// One run of a string, and whether it is code.
    struct Run: Equatable {
        let text: String
        let isCode: Bool
    }

    /// Splits on backticks: even segments are prose, odd segments are code.
    ///
    /// An **odd** number of backticks is not markup — it is a string that
    /// happens to contain one — so it is returned whole and renders literally.
    /// Silently eating a lone backtick would corrupt a value; showing it is at
    /// worst untidy.
    static func runs(_ text: String) -> [Run] {
        let parts = text.components(separatedBy: "`")
        guard parts.count > 2, parts.count.isMultiple(of: 2) == false else {
            return [Run(text: text, isCode: false)]
        }
        return parts.enumerated().compactMap { index, part in
            part.isEmpty ? nil : Run(text: part, isCode: !index.isMultiple(of: 2))
        }
    }

    /// The string as it will be read — markup removed. What an accessibility
    /// label wants.
    static func plain(_ text: String) -> String {
        runs(text).map(\.text).joined()
    }
}

/// Prose that may name a command, a path or an identifier.
///
/// Renders a `CCTextStyle` exactly as `.ccType` does — the same scaled size,
/// tracking and leading — except that a backtick-delimited span inside it is set
/// at the monospaced design and the backticks themselves are consumed.
///
/// **The colour does not change, only the typeface.** Lifting a code span a step
/// brighter was tried and rejected: the rule being enforced is "identifiers are
/// monospace", and a component that also repaints the span invents a second
/// rule nobody asked for — one that would fight every toned surface a message
/// can land on (a `warning` banner's own colour is the message).
///
/// The code run keeps the prose run's **size and weight**, so the two share a
/// baseline and the paragraph's line height is the one the token specifies. A
/// span set a point smaller "to compensate for SF Mono's width" is a second
/// typeface in the middle of a sentence.
struct CCProse: View {
    let text: String
    var style: CCTextStyle
    /// `nil` **inherits** the surrounding foreground and sets no colour at all —
    /// the same contract `.ccType(_:)` keeps, and for the same reason: a
    /// `CCButton`'s label takes its colour from the variant, so prose that
    /// insisted on the style's `documentedColor` here would paint a primary
    /// button's label `text` on a `text` fill. Pass a colour to set one.
    var color: Color?

    @ScaledMetric private var scaledSize: CGFloat

    init(_ text: String, style: CCTextStyle = CC.type.body, color: Color? = nil) {
        self.text = text
        self.style = style
        self.color = color
        _scaledSize = ScaledMetric(wrappedValue: style.size, relativeTo: style.relativeTo)
    }

    var body: some View {
        let size = min(scaledSize, style.maxSize ?? .greatestFiniteMagnitude)
        let ratio = size / style.size
        let composed = composed(at: size, ratio: ratio)
            .lineSpacing(leading(at: size, target: style.lineHeight * ratio))
        // No `.accessibilityLabel`: the backticks are already gone from the
        // runs, so the composed `Text` reads correctly on its own. Adding one
        // would turn every piece of prose in the kit into its own accessibility
        // element and fragment the rows that deliberately `.combine`.
        return Group {
            if let color {
                composed.foregroundStyle(color)
            } else {
                composed
            }
        }
    }

    private func composed(at size: CGFloat, ratio: CGFloat) -> Text {
        CCInlineCode.runs(text).reduce(Text(verbatim: "")) { accumulated, run in
            accumulated
                + Text(verbatim: run.text)
                .font(font(for: run, at: size))
                // Mono is drawn on its own advance; the prose token's tracking
                // would push it off the grid that makes it readable as code.
                .tracking(run.isCode ? 0 : style.tracking * ratio)
        }
    }

    private func font(for run: CCInlineCode.Run, at size: CGFloat) -> Font {
        let design: Font.Design = run.isCode ? .monospaced : style.design
        let base = Font.system(size: size, weight: style.weight, design: design)
        return run.isCode || style.monospacedDigits ? base.monospacedDigit() : base
    }

    /// The same leading `.ccType` computes, measured against the **taller** of
    /// the two fonts on the line. SF Mono's line box is a shade deeper than SF
    /// Pro's at the same size, and measuring against the prose font alone would
    /// let a line containing code grow past the token's line height.
    private func leading(at size: CGFloat, target: CGFloat) -> CGFloat {
        let weight = style.weight.uiKit
        let prose = UIFont.systemFont(ofSize: size, weight: weight)
        let code = UIFont.monospacedSystemFont(ofSize: size, weight: weight)
        let tallest = CCInlineCode.runs(text).contains(where: \.isCode)
            ? max(prose.lineHeight, code.lineHeight)
            : prose.lineHeight
        return max(0, target - tallest)
    }
}

// MARK: - Hairline

/// The system's only separator. "Hairlines, not fills": separation comes from a
/// 1pt border and space around it, never from a heavier grey block.
///
/// **It has no inset, and that is the whole point.** A separator runs the full
/// width of whatever contains it; if you want it narrower, inset the container.
/// Two insets shipped — full card width on Fleet and Link Health, left-inset 16
/// with a flush right edge in Settings — which measured as one asymmetric,
/// unique rule in a system whose credibility rests on there being no such
/// thing. One inset, and asymmetry is now unconstructible.
struct CCHairline: View {
    var color: Color = CC.color.border

    init(color: Color = CC.color.border) {
        self.color = color
    }

    var body: some View {
        Rectangle()
            .fill(color)
            .frame(height: CC.stroke.hairline)
            .accessibilityHidden(true)
    }
}

// MARK: - Icon

/// An SF Symbol at a size that tracks Dynamic Type.
///
/// Symbols are `.monochrome` throughout: a hierarchical or multicolour symbol
/// introduces colour that is not carrying information, and in this product
/// colour is information, never decoration.
struct CCIcon: View {
    let name: String
    var size: CGFloat = CC.size.icon
    var weight: Font.Weight = .medium
    var relativeTo: Font.TextStyle = .body

    @ScaledMetric private var scaled: CGFloat

    init(
        _ name: String,
        size: CGFloat = CC.size.icon,
        weight: Font.Weight = .medium,
        relativeTo: Font.TextStyle = .body
    ) {
        self.name = name
        self.size = size
        self.weight = weight
        self.relativeTo = relativeTo
        _scaled = ScaledMetric(wrappedValue: size, relativeTo: relativeTo)
    }

    var body: some View {
        Image(systemName: name)
            .font(.system(size: scaled, weight: weight))
            .symbolRenderingMode(.monochrome)
            // Symbols vary in optical width; a fixed frame keeps a column of
            // rows aligned no matter which glyph each one uses.
            .frame(width: scaled * 1.35, alignment: .center)
            .accessibilityHidden(true)
    }
}

// MARK: - Glyph containers

/// A bordered circle or rounded square around a glyph, sized so the container
/// grows with the glyph it holds.
///
/// **The bug this exists to make impossible.** `CCIcon` scales with Dynamic
/// Type; a `.frame(width: 28, height: 28)` around it does not. At
/// `accessibility-extra-large` the sheet's close cross measured 34pt wide
/// inside its 28pt circle — the glyph escaped the ring, and the ring read as a
/// smudge behind a cross rather than as a button. The same shape appeared three
/// times in the kit (`CCSheetChrome`'s close, `CCMonoBlock`'s copy,
/// `CCEmptyState`'s mark) and would have appeared again in every screen that
/// needed one, so it is a single scaled container rather than three
/// `@ScaledMetric`s that have to agree.
///
/// `relativeTo` must be the **same** text style the glyph inside was built
/// with, or the two ramps diverge and the fix only half works.
private struct CCGlyphContainer: ViewModifier {
    let radius: CGFloat?
    let fill: Color
    let border: Color?
    @ScaledMetric private var diameter: CGFloat

    init(diameter: CGFloat, radius: CGFloat?, fill: Color, border: Color?, relativeTo: Font.TextStyle)
    {
        self.radius = radius
        self.fill = fill
        self.border = border
        _diameter = ScaledMetric(wrappedValue: diameter, relativeTo: relativeTo)
    }

    func body(content: Content) -> some View {
        content
            .frame(width: diameter, height: diameter)
            .background(fill, in: shape)
            .overlay {
                if let border {
                    // `strokeBorder`, never `stroke`: `stroke` straddles the
                    // path and leaves half a point outside the radius, which
                    // reads as a soft edge at exactly these sizes.
                    shape.strokeBorder(border, lineWidth: CC.stroke.hairline)
                }
            }
    }

    /// One concrete shape rather than a type-erased pair. A square
    /// `RoundedRectangle` whose radius is half its side, in the `.circular`
    /// style, **is** a circle — where `.continuous` at the same radius would be
    /// a squircle. That equivalence is what lets the circle and the rounded
    /// square share a modifier without an `AnyShape` in between.
    private var shape: RoundedRectangle {
        RoundedRectangle(
            cornerRadius: radius ?? diameter / 2,
            style: radius == nil ? .circular : .continuous)
    }
}

extension View {
    /// Wraps a scalable glyph in a container that scales with it.
    ///
    /// - Parameters:
    ///   - diameter: the container's size at the nominal type size.
    ///   - radius: `nil` — the default — draws a circle.
    ///   - relativeTo: the Dynamic Type ramp the *glyph inside* rides.
    func ccGlyphContainer(
        _ diameter: CGFloat,
        radius: CGFloat? = nil,
        level: CCSurfaceLevel = .raised,
        border: Color? = CC.color.border,
        relativeTo: Font.TextStyle = .body
    ) -> some View {
        modifier(
            CCGlyphContainer(
                diameter: diameter, radius: radius, fill: level.fill, border: border,
                relativeTo: relativeTo))
    }
}

// MARK: - Spinner

/// **The only spinner in the app.** `ProgressView` is banned: its styling
/// is the system's, not ours, and it cannot be sized honestly against a 13pt
/// label.
///
/// 1.5–2pt arc, 0.9s linear rotation, sizes 16 / 24 / 32.
struct CCProgressRing: View {
    enum Size: CGFloat {
        case sm = 16
        case md = 24
        case lg = 32

        var lineWidth: CGFloat { self == .sm ? 1.5 : 2 }
    }

    var size: CGFloat = Size.sm.rawValue
    /// Defaults to the inherited foreground style, so a ring inside a primary
    /// button is black-on-white and the same ring inside a destructive button
    /// is `danger` — without either caller being told. Standalone, it defaults
    /// to `textSecondary`.
    var style: AnyShapeStyle = AnyShapeStyle(.foreground)
    var lineWidth: CGFloat = Size.sm.lineWidth

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var spinning = false

    init(_ size: Size = .sm) {
        self.size = size.rawValue
        self.lineWidth = size.lineWidth
        self.style = AnyShapeStyle(CC.text.secondary)
    }

    /// Inherits the surrounding foreground style. For use inside buttons.
    init(inheritingSize size: CGFloat, lineWidth: CGFloat = 1.5) {
        self.size = size
        self.lineWidth = lineWidth
    }

    init(size: CGFloat, color: Color, lineWidth: CGFloat = 1.5) {
        self.size = size
        self.style = AnyShapeStyle(color)
        self.lineWidth = lineWidth
    }

    var body: some View {
        Circle()
            // A 3/4 arc reads as motion even at 12pt, where a shorter trim
            // just looks like a chipped ring.
            .trim(from: 0, to: 0.75)
            .stroke(style, style: StrokeStyle(lineWidth: lineWidth, lineCap: .round))
            .frame(width: size, height: size)
            .rotationEffect(.degrees(spinning ? 360 : 0))
            .animation(
                reduceMotion ? nil : .linear(duration: 0.9).repeatForever(autoreverses: false),
                value: spinning
            )
            // Under Reduce Motion the ring sits still at reduced weight: a
            // static mark that still reads as "not finished".
            .opacity(reduceMotion ? 0.55 : 1)
            .onAppear { spinning = true }
            .accessibilityHidden(true)
    }
}

// MARK: - Press feedback

extension View {
    /// The press signature for *buttons*: a 1.5% contraction on a 120ms
    /// ease-out. Rows and cards do not scale — they lighten.
    ///
    /// Becomes a no-op under Reduce Motion, which forbids scale outright; the
    /// fill change carries the feedback on its own.
    func ccPressScale(_ pressed: Bool, scale: CGFloat = 0.985) -> some View {
        ccScaleEffect(pressed ? scale : 1)
            .ccAnimation(CC.motion.micro, value: pressed)
    }

    /// The focus ring — 2pt `borderFocus`, outside the shape so it never eats
    /// into the control's own border.
    func ccFocusRing(_ focused: Bool, radius: CGFloat) -> some View {
        overlay {
            RoundedRectangle(cornerRadius: radius + CC.stroke.focus, style: .continuous)
                .strokeBorder(CC.color.borderFocus, lineWidth: CC.stroke.focus)
                .padding(-CC.stroke.focus)
                .opacity(focused ? 1 : 0)
        }
        .ccAnimation(CC.motion.micro, value: focused)
    }
}

/// A `ButtonStyle` that hands `isPressed` back to the caller and does nothing
/// else, so a component can render its own pressed appearance without
/// reimplementing gesture handling.
struct CCPressReporter<Content: View>: ButtonStyle {
    let content: (_ label: ButtonStyleConfiguration.Label, _ isPressed: Bool) -> Content

    func makeBody(configuration: Configuration) -> some View {
        content(configuration.label, configuration.isPressed)
    }
}

// MARK: - Disabled reasons

/// The kit's contract for a dead control: it is never *just* dead.
///
/// A disabled button with no explanation is the failure mode the product's
/// first principle exists to prevent, so the reason is required at the call
/// site.
struct CCDisabledReason: Equatable {
    let text: String
    init(_ text: String) { self.text = text }
}

private struct CCDisabledModifier: ViewModifier {
    let reason: CCDisabledReason?

    func body(content: Content) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            gated(content)

            if let reason {
                HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                    CCIcon(
                        "exclamationmark.circle.fill", size: 11, weight: .semibold,
                        relativeTo: .caption
                    )
                    .foregroundStyle(CC.color.warning)
                    // `CCProse`, not `Text`: a blocked reason is the one string
                    // in the kit most likely to name the command that would
                    // unblock it — `cc pair --ssh`, `cc token` — and those
                    // arrive backticked from the call site.
                    CCProse(reason.text, style: CC.type.footnote, color: CC.color.warning)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                // **Not hidden**, even though the control above carries the
                // same string as its hint.
                //
                // It used to be, on the reasoning that exposing both makes the
                // reason read twice. That trade is wrong in one direction: a
                // hint is a *setting* — VoiceOver's Speak Hints can be off, and
                // hints are spoken after a pause the user can interrupt — so
                // with it hidden, a blind user of a card whose Allow is dead
                // could be told nothing at all. The requirement is that the
                // reason is rendered, not that it is rendered for people who
                // can see it. Hearing it twice costs a second; not hearing it
                // costs the decision.
                //
                // It also makes the requirement testable: the read-gate suite
                // asserts this sentence is *in the tree*, which is the only way
                // an automated test can check "it says why".
                .accessibilityElement(children: .combine)
                .accessibilityLabel(CCInlineCode.plain(reason.text))
                .transition(.opacity)
            }
        }
        .ccAnimation(CC.motion.small, value: reason)
    }

    /// Branches rather than applying `.accessibilityHint(reason?.text ?? "")`
    /// unconditionally.
    ///
    /// An outer `.accessibilityHint("")` *replaces* whatever hint the control
    /// set for itself, so the enabled path was silently deleting it — which
    /// cost `CCHoldButton` its "Hold for 1.2 seconds to confirm" the moment it
    /// started routing through here. When there is no reason, this modifier has
    /// nothing to say and says nothing.
    @ViewBuilder
    private func gated(_ content: Content) -> some View {
        if let reason {
            content
                .disabled(true)
                // Kept alongside the visible text so a VoiceOver user who
                // focuses the control itself hears why without having to go
                // looking for the sentence underneath it. Spoken without its
                // markup — nobody needs to hear "grave accent see see token".
                .accessibilityHint(CCInlineCode.plain(reason.text))
        } else {
            content
        }
    }
}

extension View {
    /// Disables a control *and draws why*. A disabled control MUST render its
    /// reason as visible text in `footnote` `warning` adjacent to the control —
    /// not only as an accessibility hint.
    ///
    /// The reason is deliberately not dimmed and the control is not faded:
    /// `.opacity(0.5)` on a disabled control is forbidden, because dimmed text
    /// fails contrast and reads as a render bug.
    func ccDisabled(_ reason: CCDisabledReason?) -> some View {
        modifier(CCDisabledModifier(reason: reason))
    }
}

// MARK: - Copy to pasteboard

/// Copying is a "meaningful action", so it gets a haptic and a visible
/// confirmation. Shared because three components offer it and they must not
/// each invent their own timing.
@MainActor
enum CCPasteboard {
    static func copy(_ string: String) {
        UIPasteboard.general.string = string
        // `.impact(.light)` plus a 1.4s toast, per the haptics table.
        // Not a success notification — copying is not an outcome.
        CCHaptic.light.fire()
    }
}

// MARK: - Height cap

/// The viewport a capped block measures itself against.
///
/// Defaults to 0, which means "nobody told me" — `ccScrollCap` then asks the
/// window. A sheet, a card or a screen that knows its own height should say so
/// with `.ccViewport(height:)`, because a sheet at a medium detent is not the
/// window and a block that caps itself at 45% of the *screen* inside it is
/// taller than the sheet.
private struct CCViewportHeightKey: EnvironmentKey {
    static let defaultValue: CGFloat = 0
}

extension EnvironmentValues {
    var ccViewportHeight: CGFloat {
        get { self[CCViewportHeightKey.self] }
        set { self[CCViewportHeightKey.self] = newValue }
    }
}

@MainActor
enum CCViewport {
    /// The key window's height. The fallback when nothing upstream measured a
    /// viewport — correct for a full-height screen, which is where every
    /// current caller lives.
    static var height: CGFloat {
        let measured = UIApplication.shared.connectedScenes
            .lazy
            .compactMap { ($0 as? UIWindowScene)?.keyWindow?.bounds.height }
            .first
        // iPhone 17 Pro. Only reachable before a window exists — a preview
        // snapshot — and a wrong cap there is a layout note, not a defect.
        return measured ?? 874
    }
}

/// Caps content at a fraction of the viewport and scrolls the overflow.
///
/// **The alternative is what AX5 shipped**: Link Health's bars, the Deck's
/// action bar and the accessory bar each grew until they owned 45% of the
/// screen and then kept going, pushing the content they were describing off the
/// bottom — on a HIGH card rendered at AX5, *zero characters of the command* were
/// on screen while `Hold to allow` was fully armed. A bar that has run out of
/// room must give the room back and scroll, not annex the viewport.
///
/// The cap is a height, not a `maxHeight`: a `ScrollView` takes every point it
/// is offered, so bounding it needs the content's own ideal height measured and
/// compared. Below the cap the block is exactly as tall as its content and does
/// not scroll at all.
private struct CCScrollCap: ViewModifier {
    let fraction: CGFloat
    let minimum: CGFloat
    @Environment(\.ccViewportHeight) private var viewport
    @State private var contentHeight: CGFloat = 0

    private var cap: CGFloat {
        let available = viewport.isFinite && viewport > 0 ? viewport : CCViewport.height
        return max(minimum, available * fraction)
    }

    func body(content: Content) -> some View {
        let capped = contentHeight > cap
        return ScrollView(.vertical) {
            content
                .background {
                    GeometryReader { proxy in
                        Color.clear
                            .onChange(of: proxy.size.height, initial: true) { _, height in
                                // A scroll view proposes an unbounded height to
                                // its content, so a nested one can report `inf`
                                // — which would pin the cap open forever.
                                contentHeight = height.isFinite && height > 0 ? height : 0
                            }
                    }
                }
        }
        .frame(height: min(max(contentHeight, 1), cap))
        .scrollBounceBehavior(.basedOnSize)
        // The only affordance a capped block gets. Not a fade: this product
        // fades nothing, and the one place it did — a command running off the
        // edge of `CCMonoBlock` — is the defect that produced this modifier.
        .scrollIndicators(capped ? .visible : .hidden)
        .accessibilityElement(children: .contain)
    }
}

extension View {
    /// Caps this view at `fraction` of the viewport, scrolling the rest.
    ///
    /// - Parameters:
    ///   - fraction: 0.45 by default — the most of the viewport a block may
    ///     claim before it starts crowding the action bar beneath it.
    ///   - minimum: the floor the cap will not go below, so a short viewport
    ///     cannot collapse the block to nothing.
    func ccScrollCap(_ fraction: CGFloat = 0.45, minimum: CGFloat = 120) -> some View {
        modifier(CCScrollCap(fraction: fraction, minimum: minimum))
    }

    /// Tells everything inside how tall the viewport actually is. Apply where a
    /// sheet, card or screen knows its own bounds.
    func ccViewport(height: CGFloat) -> some View {
        environment(\.ccViewportHeight, height)
    }
}

// MARK: - Conditional layout

/// Stacks horizontally at normal type sizes and vertically once Dynamic Type
/// reaches the accessibility range.
///
/// This is the single mechanism the kit uses to survive AX1–AX5 without
/// clipping: at those sizes a label and its trailing accessory cannot share a
/// line, and `ViewThatFits` guesses wrong inside a `List` row whose width is
/// not yet known.
struct CCAdaptiveStack<Content: View>: View {
    var horizontalSpacing: CGFloat = CC.space.sm
    var verticalSpacing: CGFloat = CC.space.xs
    var horizontalAlignment: HorizontalAlignment = .leading
    var verticalAlignment: VerticalAlignment = .center
    @ViewBuilder var content: () -> Content

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        if typeSize.isAccessibilitySize {
            VStack(alignment: horizontalAlignment, spacing: verticalSpacing, content: content)
        } else {
            HStack(alignment: verticalAlignment, spacing: horizontalSpacing, content: content)
        }
    }
}
