import SwiftUI

// =============================================================================
//  CCFingerprint — lighting only the characters that differ.
// =============================================================================

// MARK: - Fingerprint comparison

extension CCFingerprint {
    /// Lights only the characters that *differ* from a reference string,
    /// dimming everything that matches.
    ///
    /// Built for the SSH host-key-changed screen, where the whole question is
    /// which characters moved. Reading two 47-character fingerprints at equal
    /// brightness is a task nobody performs correctly at 2am, so the component
    /// performs it for them and shows its work.
    ///
    /// - Parameter name: what this fingerprint *is*. See `CCFingerprint.name`;
    ///   pass it wherever a screen shows more than one.
    /// - Parameter referenceName: what it is being compared *against*. Pass it
    ///   whenever the reference is not the pinned key — a screen that diffs both
    ///   directions has one block whose reference is the other block.
    static func fingerprint(
        _ value: String,
        comparedTo reference: String?,
        name: String? = nil,
        referenceName: String = "the pinned key"
    ) -> CCFingerprint {
        CCFingerprint(
            value: value, reference: reference, name: name, referenceName: referenceName)
    }
}

/// The rendered form of a fingerprint diff. Differing characters are `text` on
/// a `dangerWord` ground; matching characters stay `textTertiary`.
struct CCFingerprint: View {
    let value: String
    let reference: String?
    /// **What is being spelled.** Prefixed to the spoken label — `"The key you
    /// pinned: S, H, A, 2, 5, 6, colon, …"`.
    ///
    /// It has to live in here, and it cannot be an `.accessibilityLabel` at the
    /// call site. This view's whole accessibility contribution is that it spells
    /// its value out one character at a time, which is the only form a
    /// fingerprint can be checked in against what the Mac prints; an outer label
    /// *replaces* that, so naming the row from outside silently deletes the
    /// spelling. Without a name, though, a screen showing two of these reads two
    /// identical-sounding streams of letters with nothing to say which is the
    /// pinned key and which is the one being offered now — the one distinction
    /// the screen exists to draw.
    var name: String?
    /// **What the differing characters differ *from*.**
    ///
    /// Spoken, never drawn. The host-key screen diffs both lines against each
    /// other, so the pinned block's reference is the key being offered now — and
    /// with this hard-coded, that block read `"The key you pinned: … 1 character
    /// differs from the pinned key"`, naming itself as its own reference on the
    /// one screen where which-key-is-which is the entire question.
    var referenceName: String = "the pinned key"
    var style: CCTextStyle = CC.type.mono

    var body: some View {
        // A single concatenated `Text` rather than an HStack, so the string
        // wraps and selects as one run of text instead of as N boxes.
        segments
            .ccType(style)
            .lineLimit(nil)
            .fixedSize(horizontal: false, vertical: true)
            .textSelection(.enabled)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(spokenLabel)
    }

    private var segments: Text {
        guard let reference else {
            // `textSecondary`, not `text`. With no reference there is nothing
            // to scrutinise — this is the line you are comparing *against*.
            // Rendering it at full brightness would out-shout the diffed line
            // below it and invert the emphasis rule the component exists for.
            return Text(value).foregroundColor(CC.text.secondary)
        }
        let referenceChars = Array(reference)
        return value.enumerated().reduce(Text("")) { partial, pair in
            let (index, character) = pair
            let matches = index < referenceChars.count && referenceChars[index] == character
            let piece =
                Text(String(character))
                .foregroundColor(matches ? CC.text.tertiary : CC.text.primary)
            return partial + (matches ? piece : piece.bold())
        }
    }

    private var spokenLabel: String {
        // The name goes in front of the spelling, never around it: VoiceOver
        // reads a label left to right and stops for nothing, so a reader who
        // hears forty-seven letters before being told what they belong to has
        // already lost them.
        let prefix = name.map { "\($0): " } ?? ""
        let spelled = value.map(String.init).joined(separator: ", ")
        guard let reference else { return prefix + spelled }
        let referenceChars = Array(reference)
        let differing = value.enumerated()
            .filter { index, character in
                index >= referenceChars.count || referenceChars[index] != character
            }
            .map { String($0.element) }
        guard !differing.isEmpty else {
            return "\(prefix)\(spelled). Identical to \(referenceName)."
        }
        // The **verb** agrees too. It read "1 character differ" — and n = 1 is
        // both the commonest case and the most dangerous one on a screen about a
        // host key that changed, so it is the sentence least able to afford
        // sounding like a template.
        let count = differing.count
        let noun = count == 1 ? "character" : "characters"
        let verb = count == 1 ? "differs" : "differ"
        return "\(prefix)\(spelled). \(count) \(noun) \(verb) from \(referenceName)."
    }
}

// MARK: - Gap marker

/// A seq discontinuity, drawn **at its position in time**.
///
/// A gap is not a top-of-screen banner. It happened between two events, and
/// putting it anywhere other than between those two events misrepresents when
/// the app stopped knowing what was going on.
struct CCGapMarker: View {
    let label: String
    var actionLabel: String?
    var action: (() -> Void)?

    var body: some View {
        Group {
            if let action {
                Button {
                    CCHaptic.light.fire()
                    action()
                } label: { marker }
                .buttonStyle(CCGapMarkerStyle())
            } else {
                marker
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
        .accessibilityHint(actionLabel ?? "")
        .accessibilityAddTraits(action != nil ? .isButton : [])
    }

    private var marker: some View {
        HStack(spacing: CC.space.sm) {
            dashes
            HStack(spacing: CC.space.xxs) {
                // The pill between the dashes is a badge label, not a
                // section heading — same role as `CCBadge`'s text, same token.
                Text(label.uppercased())
                    .ccType(CC.type.badgeLabel)
                    .foregroundStyle(CC.color.warning)
                    .lineLimit(2)
                    .multilineTextAlignment(.center)
                if action != nil {
                    CCIcon("chevron.right", size: 9, weight: .bold, relativeTo: .caption)
                        .foregroundStyle(CC.color.warning)
                }
            }
            .fixedSize(horizontal: false, vertical: true)
            // The dashes are decoration; the label is the content. Both dashes
            // claim `maxWidth: .infinity`, so without this the three flexible
            // views split the row in thirds and the label gets ~90pt — about 13
            // characters a line, 26 over its two. Measured: `114 more lines ·
            // Draw more` broke mid-phrase and then truncated. A marker whose
            // rule is short is fine; a marker that truncates the sentence
            // explaining the gap is a defect, not a cosmetic one.
            //
            // **On this stack, not on the `Text` inside it.** Priority is
            // resolved among the *children of one stack*: applied to the label
            // it only decided label-versus-chevron, and the outer row went on
            // splitting itself three ways — measured unchanged at 97pt, still
            // breaking as `114 MORE LINES` / `· DRAW MORE`.
            .layoutPriority(1)
            dashes
        }
        .padding(.vertical, CC.space.xs)
        .frame(minHeight: CC.size.hitTarget)
        .frame(maxWidth: .infinity)
        .contentShape(Rectangle())
    }

    private var dashes: some View {
        Rectangle()
            .fill(.clear)
            .frame(height: 1)
            .overlay {
                Line()
                    .stroke(
                        CC.color.warning.opacity(0.45),
                        style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
            }
            // A floor so the rule never vanishes entirely, and no claim on the
            // label's width beyond the remainder — see `layoutPriority` above.
            .frame(minWidth: CC.space.md, maxWidth: .infinity)
            .accessibilityHidden(true)
    }
}

private struct Line: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.minX, y: rect.midY))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.midY))
        return path
    }
}

private struct CCGapMarkerStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .opacity(configuration.isPressed ? 0.6 : 1)
            .ccAnimation(CC.motion.micro, value: configuration.isPressed)
    }
}
