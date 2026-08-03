import SwiftUI

// =============================================================================
//  CCField / CCSegmented
// =============================================================================

// MARK: - Field

/// Label, input, hint, error. 52pt minimum.
///
/// Not `.textFieldStyle(.roundedBorder)` — that control is the system's, it is
/// drawn for light mode, and its focus ring is the system blue this product
/// does not use.
struct CCField: View {
    /// `nil` draws no label row at all.
    ///
    /// It has to be optional rather than `""`: an empty `Text` still occupies
    /// its line, which measured as ~22pt of dead space above the compose bar's
    /// field — a gap that pushed the send button off the thumb's natural arc
    /// and looked like a missing string. A field with no visible label takes
    /// its accessibility label from the placeholder, so it is never unlabelled.
    var label: String?
    @Binding var text: String
    var placeholder: String = ""
    /// Quiet guidance, shown while the field is valid.
    var hint: String?
    /// Replaces the hint when present, and turns the border `danger`.
    var error: String?
    var axis: Axis = .horizontal
    var lineLimit: ClosedRange<Int> = 1...1
    var isSecure: Bool = false
    var submitLabel: SubmitLabel = .return
    var keyboardType: UIKeyboardType = .default
    var autocapitalization: TextInputAutocapitalization = .sentences
    var disableAutocorrection: Bool = false
    /// Monospace input — endpoints, tokens, paths.
    var isMono: Bool = false
    var onSubmit: (() -> Void)?

    @FocusState private var focused: Bool
    @Environment(\.ccColumnInset) private var columnInset
    /// Same ramp and ceiling as `CCSectionHeader` and `CCFactRow`, so a field
    /// label and the headers above it move together at accessibility sizes.
    @ScaledMetric(relativeTo: .footnote) private var scaledDot: CGFloat = CC.size.dot

    private var isInvalid: Bool { error != nil }

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            if let label {
                // `fieldLabel`, not `micro`. A field label names the value
                // beside it; a `micro` presides over a group of rows. Shipped
                // as one token, `ACCOUNT NAME`, `BLOCKED` and `MEDIUM` measured
                // as the same 11pt uppercase grey on one screen.
                Text(label.uppercased())
                    .ccType(CC.type.fieldLabel, color: nil)
                    .fixedSize(horizontal: false, vertical: true)
                    // The label is text, so it goes on the content column — and
                    // it is the *only* part of a field that does. The input
                    // below draws its own bordered surface, whose leading edge is
                    // chrome and belongs on the container's own inset (the same
                    // corollary that lets `CCMonoBlock` hang its border left).
                    // Measured in the comment sheet before this: `THE HUNK`
                    // 52.67 against `YOUR COMMENT` 16.67, Δ −36.00 — and the
                    // `CCSectionHeader` fix could never reach it, because this
                    // label is not a section header.
                    .padding(.leading, CCColumn.step(from: columnInset, scaledDot: scaledDot))
            }

            input
                .padding(.horizontal, CC.space.sm)
                .padding(.vertical, CC.space.sm)
                .frame(minHeight: CC.size.controlLg)
                .ccSurface(
                    fill: CC.color.surface, radius: CC.radius.md,
                    border: borderColor, lineWidth: borderWidth)
                .ccAnimation(CC.motion.micro, value: focused)
                .ccAnimation(CC.motion.micro, value: isInvalid)
                .contentShape(Rectangle())
                // Tapping the field's padding focuses it, not just the 17pt
                // strip of text in the middle.
                .onTapGesture { focused = true }

            if let error {
                footnote(error, tone: .danger, glyph: "exclamationmark.circle.fill")
            } else if let hint {
                footnote(hint, tone: .neutral, glyph: nil)
            }
        }
        .accessibilityElement(children: .contain)
    }

    @ViewBuilder
    private var input: some View {
        ZStack(alignment: .topLeading) {
            // Our own placeholder, because the system prompt's colour is not
            // ours to set and its default fails contrast on `surface`.
            if text.isEmpty {
                Text(placeholder)
                    .ccType(isMono ? CC.type.mono : CC.type.body)
                    .foregroundStyle(CC.text.tertiary)
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
            }

            field
                .ccType(isMono ? CC.type.mono : CC.type.body)
                .foregroundStyle(CC.text.primary)
                // The caret and the selection highlight are the one place the
                // accent is allowed to appear inside a text control.
                .tint(CC.color.accent)
                .focused($focused)
                .submitLabel(submitLabel)
                .keyboardType(keyboardType)
                .textInputAutocapitalization(autocapitalization)
                .autocorrectionDisabled(disableAutocorrection)
                .onSubmit { onSubmit?() }
                // A field with no visible label is still a labelled field: the
                // placeholder is what a sighted reader is using to identify it,
                // so it is what VoiceOver gets too. A call site with a better
                // name overrides this from outside.
                .accessibilityLabel(label ?? placeholder)
                .accessibilityHint(error ?? hint ?? "")
        }
    }

    @ViewBuilder
    private var field: some View {
        if isSecure {
            SecureField("", text: $text)
                .textFieldStyle(.plain)
        } else {
            TextField("", text: $text, axis: axis)
                .textFieldStyle(.plain)
                .lineLimit(lineLimit)
        }
    }

    private func footnote(_ string: String, tone: CCTone, glyph: String?) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
            if let glyph {
                CCIcon(glyph, size: 11, weight: .semibold, relativeTo: .caption)
                    .foregroundStyle(tone.color)
            }
            // `CCProse`: a field's error and its hint are where the app names
            // the command that produces a valid value — `codeconnect pair`, `codeconnect token` —
            // and those arrive backticked from the call site.
            CCProse(
                string, style: CC.type.footnote,
                color: tone == .neutral ? CC.text.tertiary : tone.color
            )
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var borderColor: Color {
        if isInvalid { return CC.color.danger }
        return focused ? CC.color.borderFocus : CC.color.border
    }

    private var borderWidth: CGFloat {
        (focused || isInvalid) ? CC.stroke.focus : CC.stroke.hairline
    }
}

// MARK: - Segmented

struct CCSegmentedOption<Value: Hashable>: Identifiable {
    let value: Value
    let title: String
    var icon: String?

    var id: Value { value }

    init(_ value: Value, title: String, icon: String? = nil) {
        self.value = value
        self.title = title
        self.icon = icon
    }
}

/// The Timeline/Terminal switch, and any other two-or-three-way surface
/// choice.
///
/// `.pickerStyle(.segmented)` is banned: it is UIKit-drawn, it ignores the
/// palette, and it clips outright at accessibility type sizes — which is
/// exactly the bug the session screen already worked around once.
struct CCSegmented<Value: Hashable>: View {
    @Binding var selection: Value
    let options: [CCSegmentedOption<Value>]
    var accessibilityLabelText: String?

    @Namespace private var thumb
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    init(
        selection: Binding<Value>,
        options: [CCSegmentedOption<Value>],
        accessibilityLabel: String? = nil
    ) {
        self._selection = selection
        self.options = options
        self.accessibilityLabelText = accessibilityLabel
    }

    /// **The drawn track is the finger's 44 — visible target and hit target are
    /// one object.**
    ///
    /// This control has been wrong in both directions. It shipped drawing
    /// 49.67pt against an intended 36 (frame *plus* padding *plus* border); the
    /// repair inset the chrome to a 36 track inside the 44 button, keeping the
    /// full hit-tested height — and that read as the opposite failure: a
    /// control that *looks* 36 invites a 36-sized press, and the 8pt of honest
    /// but invisible target bought nothing a reader could see. The kit's rule
    /// still forbids a `contentShape` larger than the frame (reported to the
    /// accessibility tree without being hit-tested), so the resolution is the
    /// only one with no gap anywhere: draw the track over the whole 44.

    /// Track edge to thumb edge.
    private var thumbInset: CGFloat { CC.space.xxs - 1 }

    var body: some View {
        HStack(spacing: 0) {
            ForEach(options) { option in
                segment(option)
            }
        }
        .padding(.horizontal, thumbInset)
        .background { track }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(accessibilityLabelText ?? "")
    }

    /// Drawn behind the buttons, edge to edge with their hit area: what the
    /// reader sees is exactly what they can press.
    ///
    /// `surfaceRaised`, not `surface`: a segmented control is a block nested
    /// inside a screen, which is the rung of the luminance ladder that means
    /// exactly that. Sampled `#0A0A0A` where it should have been `#131313`.
    private var track: some View {
        let shape = RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
        return shape
            .fill(CC.color.surfaceRaised)
            .overlay { shape.strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
    }

    private func segment(_ option: CCSegmentedOption<Value>) -> some View {
        let isSelected = option.value == selection
        return Button {
            guard !isSelected else { return }
            // `.light`, not a selection haptic: the haptics table is closed and
            // has no selection entry. Switching surface is a light
            // confirmation, not a decision.
            CCHaptic.light.fire()
            if reduceMotion {
                selection = option.value
            } else {
                withAnimation(CC.motion.standard) { selection = option.value }
            }
        } label: {
            HStack(spacing: CC.space.xxs + 2) {
                if let icon = option.icon {
                    CCIcon(icon, size: CC.size.iconSm, weight: .semibold, relativeTo: .callout)
                }
                Text(option.title)
                    // Two lines rather than a shrunk font: `minimumScaleFactor`
                    // is how a segmented control ends up with 9pt text at AX5.
                    .lineLimit(2)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .ccType(CC.type.callout.weight(.semibold))
            // `textTertiary` unselected, not `textSecondary`. Sampled
            // `#A1A1A1`: the selected/unselected pair read 15.87 : 7.19, which
            // is barely a step. At `#828282` it is 14.87 : 5.15 — the intended
            // separation, and still AA on every surface a track can sit on.
            .foregroundStyle(isSelected ? CC.text.primary : CC.text.tertiary)
            .frame(maxWidth: .infinity)
            // 4, not 8. The thumb's floor already reserves the height; 8pt on
            // top of a 20pt `callout` line made the *content* 36 and pushed the
            // drawn track to 42, which is a different wrong number from the
            // 49.67 this replaced. What the padding is still for is
            // accessibility sizes, where the label outgrows the floor and would
            // otherwise sit hard against the thumb's own edge.
            .padding(.vertical, CC.space.xxs)
            // The **thumb**: 38pt, inside the 44pt track/button — the 3pt
            // `thumbInset` on each side is the whole of the remaining chrome.
            .frame(minHeight: CC.size.controlMd - thumbInset * 2)
            .background {
                if isSelected {
                    RoundedRectangle(cornerRadius: CC.radius.sm, style: .continuous)
                        .fill(CC.color.surfaceOverlay)
                        .overlay {
                            RoundedRectangle(cornerRadius: CC.radius.sm, style: .continuous)
                                .strokeBorder(CC.color.borderStrong, lineWidth: CC.stroke.hairline)
                        }
                        // The thumb slides between segments as one object
                        // rather than cross-fading in place.
                        .matchedGeometryEffect(id: "cc.segment.thumb", in: thumb)
                }
            }
            // Only the thumb's own inset from the track edge: the track fills
            // the button, so the pressable region and the drawn one are the
            // same rectangle and `.contentShape` hit-tests exactly what the
            // reader sees.
            .padding(.vertical, thumbInset)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(option.title)
        .accessibilityAddTraits(isSelected ? [.isButton, .isSelected] : .isButton)
    }
}
