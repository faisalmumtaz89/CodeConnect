import SwiftUI
import UIKit

// =============================================================================
//  CCKeyCap — the keys a phone keyboard does not have and a terminal needs.
// =============================================================================

/// One key in the terminal accessory row.
///
/// 40pt of visible cap, 44pt of finger, `surfaceOverlay` fill and a hairline —
/// the same surface ladder as everything else, rather than `tertiarySystemFill`,
/// which is UIKit's grey and lightens under a trait this app forces off but
/// cannot control inside every hosted subview.
///
/// **Latched is the Vercel signature, not a tint.** A held `ctrl` fills solid
/// `#EDEDED` with a `#000` label, exactly like a selected file chip and a
/// primary button. `accentColor.opacity(0.30)` — what this replaced — was a
/// wash that read as "slightly emphasised" on a surface that is already dark,
/// and a modifier you cannot tell is on is a modifier that types the wrong
/// character.
struct CCKeyCap: View {
    /// The printed legend: `esc`, `tab`, `ctrl`, `^C`. Monospace, because it is
    /// a key.
    var label: String?
    /// An SF Symbol instead of a legend, for the arrows.
    var symbol: String?
    /// `ctrl` is held down. The only latching key in the row.
    var isLatched: Bool = false
    /// What VoiceOver says. Required: `^C` read aloud is not a control anyone
    /// can identify.
    let spokenLabel: String
    let action: () -> Void

    init(
        _ label: String,
        spokenLabel: String,
        isLatched: Bool = false,
        action: @escaping () -> Void
    ) {
        self.label = label
        self.symbol = nil
        self.isLatched = isLatched
        self.spokenLabel = spokenLabel
        self.action = action
    }

    init(symbol: String, spokenLabel: String, action: @escaping () -> Void) {
        self.label = nil
        self.symbol = symbol
        self.isLatched = false
        self.spokenLabel = spokenLabel
        self.action = action
    }

    var body: some View {
        Button {
            // `playInputClick` rather than a `CCHaptic`: it honours the user's
            // own keyboard-click setting, which a raw impact generator does not,
            // and these are keys. The `ctrl` latch adds `.impact(.light)` at the
            // call site because a *mode change* is not a keystroke.
            UIDevice.current.playInputClick()
            action()
        } label: {
            cap
        }
        .buttonStyle(CCKeyCapStyle(isLatched: isLatched))
        .accessibilityLabel(spokenLabel)
        .accessibilityAddTraits(isLatched ? [.isButton, .isSelected] : .isButton)
    }

    private var cap: some View {
        Group {
            if let symbol {
                CCIcon(symbol, size: CC.size.iconSm, weight: .semibold, relativeTo: .footnote)
            } else if let label {
                Text(label)
                    .ccType(CC.type.mono.weight(.medium))
                    .lineLimit(1)
            }
        }
        .foregroundStyle(isLatched ? CC.text.onAccent : CC.text.primary)
        .padding(.horizontal, CC.space.sm)
        // 40pt of cap inside 44pt of hit area. `minHeight` rather than
        // a fixed height so a legend at AX5 grows the key instead of clipping.
        .frame(minWidth: CC.size.hitTarget, minHeight: capHeight)
        .ccSurface(
            fill: isLatched ? CC.color.accent : CC.color.surfaceOverlay,
            radius: CC.radius.sm,
            border: isLatched ? nil : CC.color.border)
        .frame(minHeight: CC.size.hitTarget)
        .contentShape(Rectangle())
        .ccAnimation(CC.motion.micro, value: isLatched)
    }

    @ScaledMetric(relativeTo: .footnote) private var capHeight: CGFloat = 40
}

private struct CCKeyCapStyle: ButtonStyle {
    let isLatched: Bool

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            // A key that does not move under the finger reads as unpressed on a
            // surface this dark; the fill change alone is invisible at 40pt.
            .ccPressScale(configuration.isPressed, scale: 0.94)
            .opacity(configuration.isPressed && !isLatched ? 0.85 : 1)
            .ccAnimation(CC.motion.micro, value: configuration.isPressed)
    }
}

// MARK: - Divider

/// The 1pt rule that separates the control keys from the arrows.
///
/// Its own type rather than a `Divider()`: `Divider` in an `HStack` takes the
/// system separator colour and stretches to the tallest sibling, which in a key
/// row is 44pt of hit area rather than the 24pt of rule it draws.
struct CCKeyCapDivider: View {
    var body: some View {
        Rectangle()
            .fill(CC.color.border)
            .frame(width: CC.stroke.hairline, height: CC.space.xl)
            .padding(.horizontal, CC.space.xxs)
            .accessibilityHidden(true)
    }
}

// MARK: - Preview

#Preview("CCKeyCap") {
    VStack(spacing: CC.space.md) {
        HStack(spacing: CC.space.xs) {
            CCKeyCap("esc", spokenLabel: "Escape") {}
            CCKeyCap("tab", spokenLabel: "Tab") {}
            CCKeyCap("ctrl", spokenLabel: "Control, on", isLatched: true) {}
            CCKeyCap("^C", spokenLabel: "Control C, interrupt") {}
            CCKeyCapDivider()
            CCKeyCap(symbol: "arrow.up", spokenLabel: "Up arrow") {}
            CCKeyCap(symbol: "arrow.down", spokenLabel: "Down arrow") {}
        }
        .padding(.horizontal, CC.space.sm)
        .padding(.vertical, CC.space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(CC.color.surfaceRaised)
        .overlay(alignment: .top) { CCHairline() }
    }
    .frame(maxHeight: .infinity, alignment: .bottom)
    .background(CC.color.bg)
    .ccAppearance()
}
