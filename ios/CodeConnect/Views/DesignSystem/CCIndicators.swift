import SwiftUI

// =============================================================================
//  CCBadge / CCStatusDot / CCFreshnessPill — how state gets on screen.
//
//  "Status is one dot + one word. Never a coloured pill *and* a rail *and* an
//  icon." These three are the entire vocabulary; a screen that needs a fourth
//  needs a conversation, not a one-off.
// =============================================================================

// MARK: - Badge

/// Status, risk and capability all render through here, so a MEDIUM risk badge
/// and a "Blocked" status badge cannot end up with different paddings.
///
/// **One construction, three tints, no exceptions.** Four shipped — saturated
/// fill, muted fill + border, neutral fill + border, transparent + coloured
/// border — which is why three risk badges measured as three unrelated objects
/// rather than as one scale with three stops. There is no `style:` parameter to
/// pick between them, because there is nothing left to pick.
/// ```
/// fill   = tint @ 12% over surface     HIGH → #271212   MEDIUM → #261D0D
/// border = tint @ 40%, 1pt             HIGH → #6C2526   MEDIUM → #684914
/// label  = tint at full strength       HIGH → #FF4D4F   MEDIUM → #F5A623
/// radius = 6 (sm)     height = 20, scaled by Dynamic Type
/// ```
/// `neutral` substitutes `surfaceRaised` / `border` / `textSecondary`, so a
/// stateless badge spends no colour at all — which is why `LOW` carries none.
///
/// Severity is expressed by the **label and the border**, never by mass: a
/// `danger` badge takes a 1.5pt edge (`CC.stroke.emphasis`) and, at risk, a
/// leading warning glyph. A saturated fill is reserved for
/// `CCButton(.destructive)` at final confirmation, which is the only place in
/// the product colour is allowed to dominate a whole control.
struct CCBadge: View {
    let text: String
    var icon: String?
    var tone: CCTone = .neutral
    /// Rendered as `×3`. Monospace, because it is a number.
    var count: Int?
    /// What VoiceOver says instead of the visible text, when the visible text
    /// is an abbreviation.
    var accessibilityText: String?
    /// Makes the badge a control: 44pt hit area and a press state. For file
    /// chips, template chips and the `SINCE YOU LOOKED` toggle.
    var action: (() -> Void)?
    /// A tappable badge that is currently on.
    var isSelected: Bool = false

    @Environment(\.dynamicTypeSize) private var typeSize
    /// 20pt at nominal, on the label's own ramp. Fixed at 21.33 it inflated a
    /// blocked row to 100.33 against an intended 96 — and 96 with a 21.33pt
    /// badge is not achievable on a 4pt grid, which is why the row is now 100
    /// and the badge 20.
    @ScaledMetric(relativeTo: .caption) private var chipHeight: CGFloat = CC.size.badge
    /// 20 = 14 (the label's line box) + 3 above + 3 below. Optical, so it is
    /// written as the number it is rather than dressed up as a space token.
    @ScaledMetric(relativeTo: .caption) private var chipPadding: CGFloat = 3
    /// Scaled too, or a chip keeps 8pt of side padding while its label triples
    /// and a two-letter badge renders as a tall portrait box with a word
    /// wedged into it. Measured at AX5 on `LOW` and on the count chip's `3`.
    @ScaledMetric(relativeTo: .caption) private var chipInset: CGFloat = CC.space.xs
    /// **A chip that is pressed is a control, not a label.** A control of that
    /// kind is 32; it shipped at the badge's own 20 (measured 19.67), so the
    /// compose templates and the file strip read as captions with an invisible
    /// 44pt target around them. The inset steps with the height, or a 32pt chip
    /// is a portrait box with a word wedged into it.
    @ScaledMetric(relativeTo: .caption) private var controlHeight: CGFloat = CC.size.chip
    @ScaledMetric(relativeTo: .caption) private var controlInset: CGFloat = CC.space.sm

    init(
        _ text: String,
        icon: String? = nil,
        tone: CCTone = .neutral,
        count: Int? = nil,
        accessibilityText: String? = nil,
        isSelected: Bool = false,
        action: (() -> Void)? = nil
    ) {
        self.text = text
        self.icon = icon
        self.tone = tone
        self.count = count
        self.accessibilityText = accessibilityText
        self.isSelected = isSelected
        self.action = action
    }

    var body: some View {
        Group {
            if let action {
                Button {
                    CCHaptic.light.fire()
                    action()
                } label: {
                    chip.ccHitTarget(minWidth: 0)
                }
                .buttonStyle(CCBadgePressStyle())
            } else {
                chip
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityAddTraits(traits)
    }

    private var chip: some View {
        HStack(spacing: CC.space.xxs) {
            if let icon {
                CCIcon(icon, size: 9, weight: .bold, relativeTo: .caption)
            }
            Text(text.uppercased())
                .ccType(CC.type.badgeLabel)
                .lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
            if let count, count > 1 {
                Text("×\(count)")
                    .ccType(CC.type.badgeLabel)
                    .lineLimit(1)
            }
        }
        .foregroundStyle(foreground)
        // The digit cannot be squeezed out of the fill: the label reports its
        // full ideal height and the chip is a `minHeight` around it, so the
        // container grows with the type rather than clipping it.
        .fixedSize(horizontal: false, vertical: true)
        // A control's drawn box, or a label's. See `controlHeight`.
        .padding(.horizontal, action == nil ? chipInset : controlInset)
        .padding(.vertical, chipPadding)
        .frame(
            minHeight: action == nil
                ? chipHeight
                : min(controlHeight, CC.size.chip * CC.size.chipMaxScale))
        .ccSurface(
            fill: fill, radius: CC.radius.sm, border: border, lineWidth: borderWidth)
        // Horizontally fixed at normal sizes so a badge is never squeezed by a
        // greedy neighbour. At AX sizes it must be allowed to wrap instead —
        // a `fixedSize` badge there is wide enough to starve the row it sits
        // in, which is the AX5 row-collapse bug.
        .fixedSize(
            horizontal: !typeSize.isAccessibilitySize,
            vertical: true)
    }

    private var traits: AccessibilityTraits {
        guard action != nil else { return [] }
        return isSelected ? [.isButton, .isSelected] : .isButton
    }

    /// The tint at full strength — 5.41:1 for `danger` on its own 12% fill,
    /// 8.27:1 for `warning`. Selection raises the label to `text` rather than
    /// changing the recipe.
    private var foreground: Color {
        if isSelected { return CC.text.primary }
        return tone == .neutral ? CC.text.secondary : tone.color
    }

    private var fill: Color {
        if isSelected && tone == .neutral { return CC.color.surfaceOverlay }
        return tone == .neutral ? CC.color.surfaceRaised : tone.muted
    }

    private var border: Color? {
        if isSelected { return tone == .neutral ? CC.color.borderStrong : tone.color.opacity(0.6) }
        return tone.border
    }

    /// The one place the scale escalates, and it escalates by weight.
    private var borderWidth: CGFloat {
        tone == .danger ? CC.stroke.emphasis : CC.stroke.hairline
    }

    private var accessibilityLabel: String {
        let base = accessibilityText ?? text
        guard let count, count > 1 else { return base }
        return "\(base), \(count)"
    }
}

// MARK: - Count chip

/// A bare number in a chip — a section header's count, and nothing else.
///
/// Built from `CCBadge`'s construction rather than beside it: same radius, same
/// scaled 20pt floor, same padding pair, same neutral fill *and* hairline. It
/// shipped as a hand-rolled label with 1pt of vertical padding and a
/// `radius.sm − 2` corner, so at AX5 the digit broke out of the fill top and
/// bottom — and it sat in the same header row as a bordered, unfilled note,
/// two chips of one size class built by opposite rules.
struct CCCountChip: View {
    let count: Int

    @ScaledMetric(relativeTo: .caption) private var chipHeight: CGFloat = CC.size.badge
    @ScaledMetric(relativeTo: .caption) private var chipPadding: CGFloat = 3
    @ScaledMetric(relativeTo: .caption) private var chipInset: CGFloat = CC.space.xs

    init(_ count: Int) {
        self.count = count
    }

    var body: some View {
        Text("\(count)")
            .ccType(CC.type.monoSmall)
            .foregroundStyle(CC.text.secondary)
            .lineLimit(1)
            // The digit reports its full height, so the chip grows around it
            // instead of clipping it.
            .fixedSize()
            .padding(.horizontal, chipInset)
            .padding(.vertical, chipPadding)
            .frame(minHeight: chipHeight)
            .ccSurface(
                fill: CC.color.surfaceRaised, radius: CC.radius.sm, border: CC.color.border)
            // The header's own accessibility label already carries the count;
            // a second element would read it twice.
            .accessibilityHidden(true)
    }
}

private struct CCBadgePressStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .ccPressScale(configuration.isPressed, scale: 0.96)
    }
}

// MARK: - Status dot

/// One 8pt component carrying **three independent bits**.
///
///  * **Colour** — semantic. `running` is `text` (white), *not* a colour:
///    running is the healthy default, and colour is reserved for things that
///    need you. This is the single largest calm win in the redesign.
///  * **Fill** — filled means live data; hollow (1.5pt ring, clear centre)
///    means it came from the cache. Cached and stale are different facts and
///    never share a treatment.
///  * **Pulse** — only `blocked` and `connecting`. Never `running`: a fleet of
///    eight pulsing dots is a fairground.
struct CCStatusDot: View {
    /// The three sizes: 8pt in rows, 10pt in card headers, 12pt in the Link
    /// Health hero.
    enum Size: CGFloat {
        case row = 8
        case cardHeader = 10
        case hero = 12
    }

    var color: Color = CC.text.tertiary
    var size: CGFloat = Size.row.rawValue
    /// From cache. A hollow dot is the *only* per-element cached treatment;
    /// the word "cached" belongs in the banner, never on a row.
    var isHollow: Bool = false
    /// 1.6s ease-in-out, opacity 1.0 ↔ 0.45.
    var pulses: Bool = false
    /// VoiceOver text. A dot with no label is decoration; pass one whenever the
    /// dot is the only thing carrying the state.
    var accessibilityText: String?

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var dimmed = false

    /// **The dot scales.** It used to be the one indicator dimension with no
    /// `@ScaledMetric` in the kit: at AX5 an 8pt speck sat beside 40pt type and
    /// read as dust, while the fact it was carrying — blocked, cached, live —
    /// was load-bearing. It rides `.footnote`, the same ramp as the `mono` it
    /// usually sits next to, and stops at `CC.size.dotMaxScale` so a growing
    /// disc cannot burst the 32–40pt gutter column the spine is built on.
    @ScaledMetric private var scaledSize: CGFloat

    init(
        color: Color = CC.text.tertiary,
        size: CGFloat = Size.row.rawValue,
        isHollow: Bool = false,
        pulses: Bool = false,
        accessibilityText: String? = nil
    ) {
        self.color = color
        self.size = size
        self.isHollow = isHollow
        self.pulses = pulses
        self.accessibilityText = accessibilityText
        _scaledSize = ScaledMetric(wrappedValue: size, relativeTo: .footnote)
    }

    /// Tone-based convenience, for the non-fleet uses (banners, legends).
    init(
        tone: CCTone,
        size: CGFloat = Size.row.rawValue,
        isHollow: Bool = false,
        pulses: Bool = false,
        accessibilityText: String? = nil
    ) {
        self.init(
            color: tone.color, size: size, isHollow: isHollow, pulses: pulses,
            accessibilityText: accessibilityText)
    }

    /// 8 → 14 at AX5, 10 → 17.5, 12 → 21.
    private var diameter: CGFloat {
        min(scaledSize, size * CC.size.dotMaxScale)
    }

    var body: some View {
        shape
            .frame(width: diameter, height: diameter)
            .opacity(dimmed ? 0.45 : 1)
            .animation(pulseAnimation, value: dimmed)
            .onAppear { if pulses && !reduceMotion { dimmed = true } }
            .accessibilityHidden(accessibilityText == nil)
            .accessibilityLabel(accessibilityText ?? "")
    }

    @ViewBuilder
    private var shape: some View {
        if isHollow {
            // The ring thickens with the dot, or a scaled hollow dot reads as a
            // filled one that lost its centre.
            Circle().strokeBorder(color, lineWidth: max(1.5, diameter * 3 / 16))
        } else {
            Circle().fill(color)
        }
    }

    /// Opacity only — no scale, no translation — so it stays within what
    /// Reduce Motion permits. It is still switched off there, because a
    /// perpetually breathing element is exactly what that setting is for.
    private var pulseAnimation: Animation? {
        guard pulses, !reduceMotion else { return nil }
        return .easeInOut(duration: 1.6).repeatForever(autoreverses: true)
    }
}

// MARK: - Freshness pill

/// Dot plus a monospace age. Always visible, always both halves: a state
/// without an age is a claim the app cannot back up.
///
/// Driven by a single freshness table — see `CCFreshnessPill.init(health:action:)`
/// in CCDomain.swift, which is the only place that table is written down.
struct CCFreshnessPill: View {
    /// Already formatted — `0.4s`, `18s`, `offline`. Monospace, so a ticking
    /// number does not shuffle the layout on every tick.
    let age: String
    var dotColor: Color = CC.color.success
    var labelColor: Color = CC.text.tertiary
    var isHollow: Bool = false
    var pulses: Bool = false
    var accessibilityLabelText: String = "Link health"
    var accessibilityValueText: String?
    var accessibilityHintText: String?
    var action: (() -> Void)?

    var body: some View {
        Group {
            if let action {
                Button {
                    CCHaptic.light.fire()
                    action()
                } label: { pill }
                .buttonStyle(CCFreshnessPillStyle())
            } else {
                pill
            }
        }
        // Toolbars compress custom content to its minimum width, which silently
        // collapsed the age to nothing — and an age you cannot read is the one
        // thing this pill exists for. Measured; do not remove.
        .fixedSize()
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(accessibilityLabelText)
        .accessibilityValue(accessibilityValueText ?? age)
        .accessibilityHint(accessibilityHintText ?? "")
        .accessibilityAddTraits(action != nil ? .isButton : [])
    }

    private var pill: some View {
        HStack(spacing: CC.space.xxs + 1) {
            CCStatusDot(
                color: dotColor, size: CC.size.dotSm, isHollow: isHollow, pulses: pulses)
            Text(age)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(labelColor)
                .fixedSize()
        }
        .padding(.horizontal, CC.space.sm)
        .padding(.vertical, CC.space.xxs + 1)
        // **36pt of drawn pill**, the toolbar row's height, applied before the
        // capsule so the capsule takes it. It shipped as the height of its own
        // content — measured **22.33pt** beside a 36pt bordered circle on the
        // same baseline, which read as two controls from two different systems
        // rather than as one toolbar.
        .frame(minHeight: CC.size.controlSm)
        .background(CC.color.surfaceRaised, in: Capsule())
        .overlay { Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
        // 36pt of chrome, 44pt of finger — and the 44 is the button's real
        // frame, not a `contentShape` reaching outside one.
        .frame(minHeight: CC.size.hitTarget)
        .contentShape(Rectangle())
    }
}

private struct CCFreshnessPillStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .ccPressScale(configuration.isPressed, scale: 0.96)
    }
}
