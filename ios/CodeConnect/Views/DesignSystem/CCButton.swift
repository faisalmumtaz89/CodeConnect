import SwiftUI

// =============================================================================
//  CCButton — the only button in the product.
// =============================================================================

/// Four variants, and no fifth.
///
/// The primary action is white-on-black: `accent` fill, `onAccent` label,
/// 17.94:1. Everything else recedes from it.
enum CCButtonVariant: String, CaseIterable, Hashable {
    /// The one action this screen exists for. At most one per view.
    case primary
    /// Transparent plus a hairline. The alternative, not the afterthought.
    case secondary
    /// The quietest control that is still a control: transparent, a `border`
    /// hairline, and a `textSecondary` label. Dismissals, "not now", inline
    /// affordances.
    ///
    /// **It has an edge, and it always did need one.** Drawn without
    /// one, `Reconnect now` and `Refresh sessions` measured as bare centred
    /// white labels with ~70pt of black around each — visually identical to the
    /// body copy above them, and the two most consequential recovery controls in
    /// the product. Meanwhile the *same class* of action got a bordered
    /// container on the diff screen and a `danger`-bordered one in Settings, so
    /// the rule was not even consistently absent.
    ///
    /// What still separates it from `secondary` is weight, not chrome:
    /// `secondary` carries a full-strength `text` label, `ghost` a
    /// `textSecondary` one that lifts to full only under the finger.
    case ghost
    /// `danger` border and label. Deliberately *not* filled: a filled red
    /// button is the last confirmation, and in this product that is a
    /// `CCHoldButton`, whose fill is the hold's own progress.
    case destructive
}

/// **Three heights, and no fourth.**
///
/// Measured on the shipped build: `Allow`/`Deny` 52, Fleet's `Done` 52, the
/// accessory bar's `Review` 44, and Session Detail's `Review` **36** — one
/// primary action below the touch minimum, on the most consequential card of
/// that screen. The ladder itself was never the problem; a primary wearing the
/// bottom rung of it was. See `CCButton.init`, which no longer allows that.
enum CCButtonSize: String, CaseIterable, Hashable {
    /// 36pt visual, 44pt hit area. Ghost and inline actions **only** — never a
    /// primary.
    case sm
    /// 44pt. The default. Bars and in-card actions.
    case md
    /// 52pt. Primary actions, action bars, sheets.
    case lg

    var minHeight: CGFloat {
        switch self {
        case .sm: return CC.size.controlSm
        case .md: return CC.size.controlMd
        case .lg: return CC.size.controlLg
        }
    }

    /// Radius grows with the control so the corner stays optically constant
    /// against the height, rather than looking tighter as the button gets big.
    var radius: CGFloat {
        switch self {
        case .sm: return CC.radius.sm
        case .md: return CC.radius.md
        case .lg: return CC.radius.lg
        }
    }

    var horizontalPadding: CGFloat {
        switch self {
        case .sm: return CC.space.sm
        case .md: return CC.space.md
        case .lg: return CC.space.lg
        }
    }

    var label: CCTextStyle {
        switch self {
        case .sm: return CC.type.footnote.weight(.semibold)
        case .md: return CC.type.callout.weight(.semibold)
        case .lg: return CC.type.headline
        }
    }

    var iconSize: CGFloat {
        switch self {
        case .sm: return CC.size.iconSm
        case .md: return CC.size.icon
        case .lg: return CC.size.icon
        }
    }

    var spinner: CGFloat {
        switch self {
        case .sm: return 12
        case .md: return 14
        case .lg: return 16
        }
    }
}

struct CCButton: View {
    let title: String
    var icon: String?
    var variant: CCButtonVariant = .primary
    var size: CCButtonSize = .md
    /// Stretches to the container. Action bars want this; a row of two buttons
    /// wants it on both so they split the width evenly.
    var fullWidth: Bool = false
    /// Shows a spinner and stops accepting taps, *without* adopting the
    /// disabled palette — a busy button is not an unavailable one.
    var isLoading: Bool = false
    /// Why this button cannot be pressed. Required to disable it: a dead
    /// control with no explanation is the failure mode this product exists to
    /// prevent, and this string is what VoiceOver reads.
    var disabledReason: CCDisabledReason?
    /// Defaults to the variant's own signature; pass `nil` for silence.
    var haptic: CCHaptic?
    let action: () -> Void

    init(
        _ title: String,
        icon: String? = nil,
        variant: CCButtonVariant = .primary,
        size: CCButtonSize = .md,
        fullWidth: Bool = false,
        isLoading: Bool = false,
        disabledReason: CCDisabledReason? = nil,
        haptic: CCHaptic? = .decision,
        action: @escaping () -> Void
    ) {
        self.title = title
        self.icon = icon
        self.variant = variant
        // **A primary action is never 36pt.** The 44×44 minimum is a rule about
        // the *visible* control, and `sm` is 36 with a 44pt hit area — an honest
        // bargain for a ghost `Show more`, and the wrong one for the button that
        // commits the decision. Enforced here rather than written down, because
        // a rule that lives in prose is a rule that ships at 36.
        self.size = (variant == .primary && size == .sm) ? .md : size
        self.fullWidth = fullWidth
        self.isLoading = isLoading
        self.disabledReason = disabledReason
        // Secondary and ghost are the quiet options; buzzing on every "Cancel"
        // trains the user to ignore the haptic that matters.
        self.haptic = (variant == .secondary || variant == .ghost) ? nil : haptic
        self.action = action
    }

    var body: some View {
        Button {
            guard !isLoading else { return }
            haptic?.fire()
            action()
        } label: {
            CCButtonLabel(title: title, icon: icon, size: size, isLoading: isLoading)
        }
        .buttonStyle(CCButtonStyle(variant: variant, size: size, fullWidth: fullWidth))
        .ccDisabled(disabledReason)
        .allowsHitTesting(!isLoading)
        // Without its markup: a label that names a run reads as the run, not as
        // "grave accent see see dash tests grave accent".
        .accessibilityLabel(CCInlineCode.plain(title))
        // "Busy" rather than a second label: the control has not changed
        // identity, only state, and VoiceOver users need the difference.
        .accessibilityValue(isLoading ? "Busy" : "")
    }
}

// MARK: - Label

private struct CCButtonLabel: View {
    let title: String
    let icon: String?
    let size: CCButtonSize
    let isLoading: Bool

    var body: some View {
        HStack(spacing: CC.space.xs) {
            if isLoading {
                // No colour passed: the ring inherits the variant's own
                // foreground, so it is black on a primary fill and `danger` on
                // a destructive outline without either being restated here.
                CCProgressRing(inheritingSize: size.spinner)
                    .transition(.opacity)
            } else if let icon {
                CCIcon(icon, size: size.iconSize, weight: .semibold, relativeTo: .body)
            }
            // `CCProse`, so a label that names a run — `Send to CodeConnect ·
            // \`cc-tests\`` — sets that half in mono. A button label is the last
            // place an identifier should be guessed at, and it was the third
            // live sibling of the proportional-type defect.
            CCProse(title, style: size.label, color: nil)
                // Wraps rather than truncates: at AX5 a two-word label needs
                // two lines, and a clipped verb is a control nobody can trust.
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
        .ccAnimation(CC.motion.micro, value: isLoading)
    }
}

// MARK: - Style

private struct CCButtonStyle: ButtonStyle {
    let variant: CCButtonVariant
    let size: CCButtonSize
    let fullWidth: Bool

    func makeBody(configuration: Configuration) -> some View {
        CCButtonSurface(
            variant: variant, size: size, fullWidth: fullWidth,
            isPressed: configuration.isPressed, label: configuration.label)
    }
}

private struct CCButtonSurface: View {
    let variant: CCButtonVariant
    let size: CCButtonSize
    let fullWidth: Bool
    let isPressed: Bool
    let label: ButtonStyleConfiguration.Label

    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        label
            .foregroundStyle(foreground)
            .padding(.horizontal, size.horizontalPadding)
            .padding(.vertical, CC.space.xs)
            .frame(maxWidth: fullWidth ? .infinity : nil)
            // `minHeight`, never a fixed height: the control grows with
            // Dynamic Type instead of clipping its own label.
            .frame(minHeight: size.minHeight)
            .ccSurface(fill: fill, radius: size.radius, border: border)
            // A 36pt `sm` button still answers to a 44pt finger.
            .frame(minHeight: CC.size.hitTarget)
            .contentShape(Rectangle())
            .ccPressScale(isPressed)
            .ccAnimation(CC.motion.micro, value: isPressed)
            .ccAnimation(CC.motion.micro, value: isEnabled)
    }

    // Every variant converges on the same disabled appearance. A disabled
    // control has no hierarchy left to express — only unavailability.
    private var foreground: Color {
        guard isEnabled else { return CC.text.disabled }
        switch variant {
        case .primary: return CC.text.onAccent
        case .secondary: return CC.text.primary
        // Ghost lights up under the finger instead of moving or filling
        // heavily — the quietest press signature in the kit.
        case .ghost: return isPressed ? CC.text.primary : CC.text.secondary
        case .destructive: return CC.color.danger
        }
    }

    private var fill: Color {
        guard isEnabled else { return CC.color.surface }
        // Secondary and ghost both go `clear` → `surfaceRaised`. They
        // are the same press, because they are the same weight of action.
        switch variant {
        case .primary: return isPressed ? CC.color.accentPressed : CC.color.accent
        case .secondary, .ghost: return isPressed ? CC.color.surfaceRaised : .clear
        case .destructive: return isPressed ? CC.color.muted(.danger) : .clear
        }
    }

    private var border: Color? {
        guard isEnabled else { return CC.color.border }
        switch variant {
        // The only variant with no edge, because it does not need one: a solid
        // `#EDEDED` field on `#000` is the strongest boundary in the palette.
        case .primary: return nil
        case .secondary, .ghost: return isPressed ? CC.color.borderStrong : CC.color.border
        case .destructive:
            return CC.color.danger.opacity(isPressed ? 0.70 : 0.45)
        }
    }
}

extension View {
    /// A button that lives in a navigation bar, and must not wrap.
    ///
    /// `CCButton` lets its label wrap on purpose: at AX5 a two-word verb needs two
    /// lines, and a clipped verb is a control nobody can trust. A toolbar breaks
    /// that bargain — it hands its items a *narrow proposal* rather than the width
    /// they ask for, and the label dutifully accepts it. Measured on device with a
    /// leading counter and an inline title competing for the same bar: `Done`
    /// rendered as `Do` / `ne`.
    ///
    /// So the fix is opt-in and local to the bar. `fixedSize(horizontal:)` refuses
    /// the compression rather than absorbing it, which is the honest answer: a
    /// toolbar with more items than fit should drop one, not fold a word in half.
    /// Deliberately *not* applied to `CCButton` generally — ordinary controls keep
    /// their wrapping.
    func ccToolbarButton() -> some View {
        lineLimit(1).fixedSize(horizontal: true, vertical: false)
    }
}
