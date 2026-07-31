import SwiftUI

// =============================================================================
//  CCHoldButton — hold-to-approve, with a progress ring.
// =============================================================================

/// The gate in front of a HIGH-risk approval.
///
/// A hold, never a swipe. An accidental swipe approving `rm -rf` is the one bug
/// that ends the product, so there is no swipe gesture anywhere in this app and
/// this is the component that makes the alternative feel deliberate rather than
/// tedious.
///
/// Two things make the hold legible:
///
///  * a **ring** stroked around the button's own rounded rect, so progress is
///    read at the edge of the thing being pressed rather than in a separate
///    indicator. Explicitly a ring and **not** a fill sweep: a sweep at
///    `danger` reads as the button already having committed.
///  * **escalating haptics** — a soft tick at 25/50/75%, a rigid thump on
///    commit — so the hold can be completed without looking, which at 2am is
///    how it will actually be used.
///
/// Fill is transparent, border is `danger`. Releasing early rewinds. Nothing is
/// submitted on a partial hold and nothing is remembered between attempts.
struct CCHoldButton: View {
    let title: String
    var icon: String? = "exclamationmark.triangle.fill"
    var tone: CCTone = .danger
    var duration: Double = CC.duration.hold
    var isLoading: Bool = false
    var fullWidth: Bool = true
    /// Why this button cannot be held. **Drawn**, not merely hinted — see the
    /// note on `body`.
    var disabledReason: CCDisabledReason?
    let action: () -> Void

    @State private var progress: Double = 0
    @State private var isHolding = false
    @State private var tickTask: Task<Void, Never>?
    @State private var pressedAt: Date?
    @State private var showsAbortHint = false
    @State private var abortTask: Task<Void, Never>?

    private var isEnabled: Bool { disabledReason == nil && !isLoading }

    /// What an unfinished hold says. Explains the mechanism rather than
    /// reporting a failure: nothing went wrong, the gesture simply is not
    /// finished, and an alarming string here would teach people to fear the
    /// control that guards the most consequential action in the product.
    private static let abortHint = "Keep holding until the ring closes."

    init(
        _ title: String,
        icon: String? = "exclamationmark.triangle.fill",
        tone: CCTone = .danger,
        duration: Double = CC.duration.hold,
        isLoading: Bool = false,
        fullWidth: Bool = true,
        disabledReason: CCDisabledReason? = nil,
        action: @escaping () -> Void
    ) {
        self.title = title
        self.icon = icon
        self.tone = tone
        self.duration = duration
        self.isLoading = isLoading
        self.fullWidth = fullWidth
        self.disabledReason = disabledReason
        self.action = action
    }

    /// **The reason is drawn, not whispered.**
    ///
    /// This control used to route `disabledReason` to `.accessibilityHint` and
    /// nowhere else — the only control in the kit that did not go through
    /// `.ccDisabled`. The failure that exposes: on a HIGH-risk card the link
    /// goes stale, the hold button greys out and says nothing, and the Deny
    /// button beside it deliberately omits its own reason on the premise that
    /// "Allow is where the sentence lives" — which is true at LOW and MEDIUM,
    /// where Allow is a `CCButton`, and false at HIGH, where Allow *is* this
    /// button. Both controls went silent at the moment of maximum consequence.
    ///
    /// `.ccDisabled` also actually disables the subtree, so the long-press
    /// gesture stops being recognised rather than being caught by a guard —
    /// a control that swallows a two-second hold and then does nothing is
    /// indistinguishable from a broken one.
    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            control
                .ccDisabled(disabledReason)
                // A busy button is not an unavailable one, so loading
                // blocks the gesture without adopting the disabled treatment or
                // claiming a reason it does not have.
                .allowsHitTesting(!isLoading)

            if showsAbortHint {
                Text(Self.abortHint)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .transition(.opacity)
                    // Spoken through the button's own value instead, so the
                    // hint is not a second element to swipe past.
                    .accessibilityHidden(true)
            }
        }
        .ccAnimation(CC.motion.small, value: showsAbortHint)
        .onDisappear {
            tickTask?.cancel()
            abortTask?.cancel()
        }
    }

    private var control: some View {
        label
            .frame(maxWidth: fullWidth ? .infinity : nil)
            .frame(minHeight: CC.size.controlLg)
            .ccSurface(fill: fill, radius: CC.radius.lg, border: border)
            .overlay { ring }
            .contentShape(Rectangle())
            .ccPressScale(isHolding, scale: 0.985)
            .onLongPressGesture(
                minimumDuration: duration,
                maximumDistance: 40,
                perform: complete,
                onPressingChanged: pressingChanged
            )
            .accessibilityElement(children: .combine)
            .accessibilityLabel(title)
            .accessibilityAddTraits(.isButton)
            .accessibilityValue(isLoading ? "Busy" : (showsAbortHint ? Self.abortHint : ""))
            .accessibilityHint(
                disabledReason?.text
                    ?? "Hold for \(formattedDuration) seconds to confirm. With VoiceOver, double tap to confirm."
            )
            // VoiceOver cannot express a hold, so it gets a plain activation.
            // Refusing it would make the highest-stakes control in the product
            // the one blind users cannot operate.
            .accessibilityAction {
                guard isEnabled else { return }
                CCHaptic.commit.fire()
                action()
            }
    }

    // MARK: Appearance

    private var label: some View {
        HStack(spacing: CC.space.xs) {
            if isLoading {
                CCProgressRing(inheritingSize: 16)
            } else if let icon {
                CCIcon(icon, size: CC.size.icon, weight: .semibold)
            }
            Text(title)
                .ccType(CC.type.headline)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
        .foregroundStyle(isEnabled ? tone.color : CC.text.disabled)
        .padding(.horizontal, CC.space.lg)
        .padding(.vertical, CC.space.xs)
    }

    @ViewBuilder
    private var ring: some View {
        if isEnabled {
            RoundedRectangle(cornerRadius: CC.radius.lg, style: .continuous)
                // Half the stroke, so the ring sits *on* the border rather than
                // straddling it and softening the corner.
                .inset(by: CC.stroke.ring / 2)
                .trim(from: 0, to: progress)
                .stroke(
                    tone.color,
                    style: StrokeStyle(lineWidth: CC.stroke.ring, lineCap: .round)
                )
                .allowsHitTesting(false)
        }
    }

    /// Transparent, deliberately. The ring is the only progress signal; a filling
    /// background would read as the decision already having been taken.
    private var fill: Color {
        isEnabled ? .clear : CC.color.surface
    }

    private var border: Color {
        isEnabled ? tone.color.opacity(isHolding ? 0.75 : 0.45) : CC.color.border
    }

    private var formattedDuration: String {
        String(format: "%.1f", duration)
    }

    // MARK: Gesture

    private func pressingChanged(_ pressing: Bool) {
        guard isEnabled else { return }
        isHolding = pressing

        // The sweep is *information* — elapsed time against a deadline — so it
        // animates even under Reduce Motion. A linear curve is mandatory: any
        // easing here would misreport how much of the hold is left.
        withAnimation(.linear(duration: pressing ? duration : CC.duration.exit)) {
            progress = pressing ? 1 : 0
        }

        tickTask?.cancel()
        guard pressing else {
            reportAbortIfNeeded()
            return
        }

        abortTask?.cancel()
        showsAbortHint = false
        pressedAt = Date()
        // Ticks at 25 / 50 / 75%. Three even slices of the hold, so
        // the escalation itself reports how much is left.
        tickTask = Task { @MainActor in
            for _ in 0..<3 {
                try? await Task.sleep(for: .seconds(duration * 0.25))
                guard !Task.isCancelled else { return }
                CCHaptic.holdTick.fire()
            }
        }
    }

    /// **A failed gesture must never look like nothing happened.**
    ///
    /// A short tap used to draw about 8% of the ring, rewind it in 240ms and
    /// say nothing at all — no haptic, no text. On the app's most consequential
    /// control that is the worst possible reading, because "nothing happened"
    /// and "it did not take" are indistinguishable, and the second one is the
    /// true one. The kit's own standard is already higher: a refused second tap
    /// on the decision card answers with a sentence.
    ///
    /// Elapsed time decides, not a flag, because `perform` and
    /// `onPressingChanged(false)` are not ordered against each other: a press
    /// that ran the full duration completed, and anything shorter did not.
    /// Under 80ms is a brush past the control rather than a gesture, and stays
    /// silent — there, "nothing happened" is the honest reading.
    private func reportAbortIfNeeded() {
        guard let started = pressedAt else { return }
        pressedAt = nil
        let held = Date().timeIntervalSince(started)
        // The 50ms tolerance keeps a release that races `perform` on the last
        // frame from being reported as an abort *and* completing.
        guard held >= 0.08, held < duration - 0.05 else { return }

        // The softest entry in the closed haptics table — the same tick
        // the hold itself uses. Nothing new is invented for this.
        CCHaptic.holdTick.fire()
        abortTask?.cancel()
        showsAbortHint = true
        abortTask = Task { @MainActor in
            try? await Task.sleep(for: .seconds(CC.duration.toast))
            guard !Task.isCancelled else { return }
            showsAbortHint = false
        }
    }

    private func complete() {
        guard isEnabled else { return }
        tickTask?.cancel()
        abortTask?.cancel()
        // Consumed, so the release that follows a completed hold cannot be read
        // as an abort.
        pressedAt = nil
        showsAbortHint = false
        isHolding = false
        // Reset without animation: the gesture has already fired, and rewinding
        // the ring would read as the hold being undone.
        progress = 0
        CCHaptic.commit.fire()
        action()
    }
}
