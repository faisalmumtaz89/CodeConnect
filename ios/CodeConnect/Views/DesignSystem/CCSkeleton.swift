import SwiftUI

// =============================================================================
//  CCSkeleton — the shape of a wait.
// =============================================================================

/// A rounded `surfaceRaised` bar standing in for a line that has not arrived.
///
/// **No shimmer, ever.** A shimmer is an animation that implies
/// progress, and the state this component describes is precisely the one where
/// there may be none: on a link that has genuinely stalled, a travelling
/// highlight is a lie told sixty times a second. A still bar plus the ticking
/// elapsed counter beside it tells the truth — *nothing has arrived, and here is
/// how long that has been true.*
struct CCSkeleton: View {
    /// `nil` fills the available width.
    var width: CGFloat?
    var height: CGFloat = 17
    var radius: CGFloat = CC.radius.md
    /// The Dynamic Type ramp the line it stands in for rides, so the placeholder
    /// is the height of the text that will land in it.
    var relativeTo: Font.TextStyle = .headline

    @ScaledMetric private var scaledHeight: CGFloat

    init(
        width: CGFloat? = nil,
        height: CGFloat = 17,
        radius: CGFloat = CC.radius.md,
        relativeTo: Font.TextStyle = .headline
    ) {
        self.width = width
        self.height = height
        self.radius = radius
        self.relativeTo = relativeTo
        _scaledHeight = ScaledMetric(wrappedValue: height, relativeTo: relativeTo)
    }

    var body: some View {
        RoundedRectangle(cornerRadius: radius, style: .continuous)
            .fill(CC.color.surfaceRaised)
            // Widths stay in points while heights scale: the bar is a
            // placeholder, not a measurement, and a 200pt bar grown by 2× at AX5
            // would simply run out of column.
            .frame(width: width, height: scaledHeight)
            .frame(maxWidth: width == nil ? .infinity : nil, alignment: .leading)
            .accessibilityHidden(true)
    }
}

// MARK: - Row shapes

/// The two skeleton silhouettes the decision-path screens wait in.
///
/// They are shapes, not content: a reader who sees a two-bar row knows a fleet
/// row is coming and a reader who sees a glyph-plus-two-runs row knows a tool
/// call is. Guessing the shape wrong is worse than a blank space, so there are
/// only the shapes the screens actually produce.
struct CCSkeletonRow: View {
    enum Shape {
        /// A fleet row — 140×17 over 200×13, with the gutter dot in place.
        case fleetRow
        /// A tool call: glyph, name, argument, duration.
        case toolRow
        /// Agent prose: two full-measure runs.
        case proseRow
    }

    let shape: Shape

    var body: some View {
        switch shape {
        case .fleetRow:
            HStack(alignment: .top, spacing: CC.space.sm) {
                Circle()
                    .fill(CC.color.surfaceRaised)
                    .frame(width: CC.size.dot, height: CC.size.dot)
                    // Optically on the first line's cap height, exactly where a
                    // real `CCStatusDot` will appear.
                    .padding(.top, CC.space.xxs + 2)
                VStack(alignment: .leading, spacing: CC.space.xxs + 1) {
                    CCSkeleton(width: 140, height: 17, relativeTo: .headline)
                    CCSkeleton(width: 200, height: 13, relativeTo: .footnote)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.md)

        case .toolRow:
            HStack(spacing: CC.space.sm) {
                CCSkeleton(width: 16, height: 16, radius: CC.radius.sm, relativeTo: .callout)
                CCSkeleton(width: 64, height: 15, relativeTo: .callout)
                CCSkeleton(width: 132, height: 12, relativeTo: .caption)
                Spacer(minLength: CC.space.xs)
                CCSkeleton(width: 34, height: 12, relativeTo: .caption)
            }
            .frame(minHeight: CC.size.controlSm)

        case .proseRow:
            VStack(alignment: .leading, spacing: CC.space.xs) {
                CCSkeleton(height: 16, relativeTo: .body)
                CCSkeleton(height: 16, relativeTo: .body)
                    .frame(maxWidth: 220, alignment: .leading)
            }
        }
    }
}

// MARK: - Waiting notice

/// The sentence and the ticking counter that must accompany every skeleton —
/// *every wait has a shape, a sentence, and a ticking elapsed counter* — plus
/// the escape hatch every wait over 20s owes the reader.
///
/// The escalation is the honesty: `waiting for the daemon · 3s` becomes
/// `still waiting · 8s` and grows a way out, because a wait that has stopped
/// being normal should stop *looking* normal.
struct CCWaitingNotice: View {
    let elapsed: TimeInterval
    var label: String = "waiting for the daemon"
    var lateLabel: String = "still waiting"
    /// When the notice starts calling itself late, and offers `actionTitle`.
    var lateAfter: TimeInterval = 8
    var actionTitle: String = "Check link"
    var action: (() -> Void)?

    private var isLate: Bool { elapsed >= lateAfter }

    var body: some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
            Text("\(isLate ? lateLabel : label) · \(Format.age(elapsed))")
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
            if isLate, let action {
                CCButton(actionTitle, variant: .ghost, size: .sm, action: action)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
    }
}
