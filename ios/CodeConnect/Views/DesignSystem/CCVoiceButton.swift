import SwiftUI

// =============================================================================
//  CCVoiceButton — the compose bar's one control.
// =============================================================================

/// What the circle's face means right now.
enum CCVoiceButtonPhase: Equatable {
    /// Empty field. Tapping starts dictation.
    case dictate
    /// Text staged. Tapping sends it.
    case send
    /// The mic is hot. Tapping ends the recording — and *only* that: while
    /// recording, a stray tap can stop a transcription but can never send one.
    case stop
    /// The daemon is typing the text. A spinner, **not** the disabled palette —
    /// a busy button is not an unavailable one.
    case sending
    /// The daemon confirmed the echo. The checkmark holds for a beat, then the
    /// face returns to rest.
    case sent
}

/// A 44pt circle whose face *is* the state: mic, arrow, stop, spinner, check.
///
/// One control in one position doing the compose bar's whole job, so the row
/// never grows a second button for the thumb to arbitrate at 2am. 44pt of
/// *visible* cap — the hit-target floor as the drawn control, not as an
/// invisible halo around something smaller — because this is the most-used
/// button in the product.
///
/// The face is `accent` in every live phase, the kit's primary signature.
/// Blocked (`blockedReason` set, and the phase is one that acts on the daemon)
/// wears the compose bar's disabled recipe: `surfaceRaised` fill,
/// `textDisabled` glyph, hairline edge. The *visible* reason is the note the
/// compose bar already shows above the field; the string here is what
/// VoiceOver reads.
struct CCVoiceButton: View {
    let phase: CCVoiceButtonPhase
    /// Why `dictate` and `send` cannot act right now (stale link, a daemon
    /// that refuses typed text). Never blocks `stop`: a recording can always
    /// be ended.
    var blockedReason: String?
    let action: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var ringDimmed = false

    private var isBlocked: Bool {
        blockedReason != nil && (phase == .dictate || phase == .send)
    }

    var body: some View {
        Button {
            guard !isBlocked, phase != .sending, phase != .sent else { return }
            action()
        } label: {
            face
        }
        .buttonStyle(CCVoiceButtonStyle())
        .accessibilityLabel(accessibilityLabel)
        .accessibilityHint(accessibilityHint)
        // "Busy", not a second label: the control has not changed identity.
        .accessibilityValue(phase == .sending ? "Busy" : "")
    }

    private var face: some View {
        ZStack {
            Circle().fill(isBlocked ? CC.color.surfaceRaised : CC.color.accent)
            if isBlocked {
                Circle().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
            }
            glyph
                .foregroundStyle(isBlocked ? CC.text.disabled : CC.text.onAccent)
        }
        // Fixed like every control height in the kit: the circle grows only
        // if its content demands it, which a glyph never does.
        .frame(width: CC.size.hitTarget, height: CC.size.hitTarget)
        .overlay {
            if phase == .stop {
                recordingRing
            }
        }
        .contentShape(Circle())
        .ccAnimation(CC.motion.micro, value: phase)
        .ccAnimation(CC.motion.micro, value: isBlocked)
    }

    @ViewBuilder
    private var glyph: some View {
        switch phase {
        case .dictate:
            CCIcon("mic.fill", size: CC.size.iconLg, weight: .semibold, relativeTo: .body)
        case .send:
            CCIcon("arrow.up", size: CC.size.iconLg, weight: .semibold, relativeTo: .body)
        case .stop:
            CCIcon("stop.fill", size: CC.size.icon, weight: .semibold, relativeTo: .body)
        case .sending:
            // Inherits `onAccent`, so the ring is black on the white face
            // without being told — same contract as CCButton's spinner.
            CCProgressRing(inheritingSize: 16)
        case .sent:
            CCIcon("checkmark", size: CC.size.iconLg, weight: .semibold, relativeTo: .body)
        }
    }

    /// The one universal meaning of a red ring around a control: the mic is
    /// hot. Same breathing rhythm as a live `CCStatusDot` (1.6s, ease-in-out),
    /// collapsing to a steady ring under Reduce Motion — the *presence* of the
    /// ring carries the fact; the breathing is only emphasis.
    private var recordingRing: some View {
        Circle()
            .stroke(
                CC.color.danger.opacity(ringDimmed ? 0.16 : CC.opacity.pulse),
                lineWidth: 4
            )
            .padding(-4)
            .onAppear {
                guard !reduceMotion else { return }
                withAnimation(.easeInOut(duration: 1.6).repeatForever(autoreverses: true)) {
                    ringDimmed = true
                }
            }
            .onDisappear { ringDimmed = false }
            .accessibilityHidden(true)
    }

    private var accessibilityLabel: String {
        switch phase {
        case .dictate: return "Start dictation"
        case .send: return "Send"
        case .stop: return "Stop dictation"
        case .sending: return "Send"
        case .sent: return "Sent"
        }
    }

    private var accessibilityHint: String {
        if let blockedReason, isBlocked { return blockedReason }
        switch phase {
        case .dictate: return "Transcribes speech on this phone into the message field."
        case .send: return "Types this into the agent's prompt."
        case .stop: return "Keeps the transcript in the field for review."
        case .sending, .sent: return ""
        }
    }
}

/// The keycap's press feel, not the row's: a 44pt disc needs real travel to
/// read as pressed on a surface this dark.
private struct CCVoiceButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .ccPressScale(configuration.isPressed, scale: 0.94)
            .opacity(configuration.isPressed ? 0.85 : 1)
    }
}

// MARK: - Preview

#Preview("CCVoiceButton") {
    VStack(spacing: CC.space.lg) {
        HStack(spacing: CC.space.md) {
            CCVoiceButton(phase: .dictate) {}
            CCVoiceButton(phase: .send) {}
            CCVoiceButton(phase: .stop) {}
            CCVoiceButton(phase: .sending) {}
            CCVoiceButton(phase: .sent) {}
        }
        HStack(spacing: CC.space.md) {
            CCVoiceButton(phase: .dictate, blockedReason: "Link stale.") {}
            CCVoiceButton(phase: .send, blockedReason: "Link stale.") {}
        }
    }
    .padding()
    .background(CC.color.surfaceRaised)
    .ccAppearance()
}
