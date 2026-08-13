import SwiftUI

// =============================================================================
//  CCGapMarker — a seq discontinuity, drawn at its position in time.
// =============================================================================

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
