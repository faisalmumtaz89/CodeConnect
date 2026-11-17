import CodeConnectKit
import SwiftUI

// =============================================================================
//  CCField
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
    /// Where the label sits: on the content column (the default, for a field
    /// among rows that keep a gutter), or on the field's own leading edge, for a
    /// document with no gutter — a question sheet, where every line starts at
    /// the margin and a label stepped out to the column was a third edge.
    var labelOnContentColumn: Bool = true
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
                    .padding(
                        .leading,
                        labelOnContentColumn
                            ? CCColumn.step(from: columnInset, scaledDot: scaledDot) : 0)
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
                footnote(error, tone: .danger, glyph: "warning-circle-fill")
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
                    .ccMark(beside: CC.type.footnote)
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
