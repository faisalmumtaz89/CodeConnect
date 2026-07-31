import SwiftUI

// =============================================================================
//  CCDisclosure — the replacement for `DisclosureGroup`.
// =============================================================================

/// A 44pt row, a `micro` `textTertiary` label, a 12pt chevron that rotates 90°
/// on expand, and a 1pt `border` top rule.
///
/// `DisclosureGroup` is banned for the usual reason — its label type, its
/// chevron size, its insets and its animation are the system's — but also for a
/// specific one: it indents its content, and the content here is a `CCMonoBlock`
/// holding text that must be read character for character against the Mac. An
/// indent that is not on the grid moves a hash off the content column.
///
/// The label is deliberately `micro`: what is behind a disclosure is provenance,
/// not prose, and it should read as a section label rather than as an offer.
struct CCDisclosure<Content: View>: View {
    let label: String
    /// Rendered right-aligned before the chevron — a count, a size, a time.
    var note: String?
    /// Draws the 1pt rule above the row. Off for the first disclosure in a
    /// stack, where the block above has already drawn its own edge.
    var showsTopRule: Bool = true

    /// Set when the caller needs to drive or observe the state — the timeline's
    /// failed tool rows auto-expand once on arrival and must stay collapsed if
    /// the reader closes them.
    private let external: Binding<Bool>?
    @State private var local: Bool
    private let content: () -> Content

    private var isExpanded: Binding<Bool> { external ?? $local }

    @Environment(\.ccColumnInset) private var columnInset
    /// The same ramp `CCSectionHeader` and `CCFactRow` use, so a disclosure's
    /// heading and the headings around it move together at accessibility sizes.
    @ScaledMetric(relativeTo: .footnote) private var scaledDot: CGFloat = CC.size.dot

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised memberwise
    /// one, and the two collide over the private stored properties.
    init(
        _ label: String,
        note: String? = nil,
        showsTopRule: Bool = true,
        isExpanded: Binding<Bool>,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.label = label
        self.note = note
        self.showsTopRule = showsTopRule
        self.external = isExpanded
        self._local = State(initialValue: isExpanded.wrappedValue)
        self.content = content
    }

    /// Owns its own state, for the common case where nothing outside needs to
    /// know whether the drawer is open.
    init(
        _ label: String,
        note: String? = nil,
        showsTopRule: Bool = true,
        initiallyExpanded: Bool = false,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.label = label
        self.note = note
        self.showsTopRule = showsTopRule
        self.external = nil
        self._local = State(initialValue: initiallyExpanded)
        self.content = content
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if showsTopRule { CCHairline() }

            Button {
                // No haptic, deliberately: opening a drawer is not a decision
                // and does not buzz.
                withAnimation(CC.motion.small) { isExpanded.wrappedValue.toggle() }
            } label: {
                HStack(spacing: CC.space.sm) {
                    Text(label.uppercased())
                        .ccType(CC.type.micro)
                        .foregroundStyle(CC.text.tertiary)
                        .multilineTextAlignment(.leading)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: CC.space.xs)
                    if let note {
                        Text(note)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(CC.text.tertiary)
                            .lineLimit(1)
                    }
                    CCIcon("chevron.right", size: 12, weight: .semibold, relativeTo: .caption)
                        .foregroundStyle(CC.text.tertiary)
                        .rotationEffect(.degrees(isExpanded.wrappedValue ? 90 : 0))
                        // Rotation is geometry, so Reduce Motion gets the state
                        // change without the sweep — the chevron still ends up
                        // pointing down, it just gets there instantly.
                        .ccAnimation(CC.motion.small, value: isExpanded.wrappedValue)
                }
                // **The label is a section label, so it goes on 52** — the same
                // text edge every other heading holds. It is literally the
                // `micro` token — the word that names a group — and it sat on
                // the container's own edge while the verbatim block underneath
                // it now lands on the content column: measured on the gallery's
                // `terminal` page, label 16.00 against block text 52.00, a third
                // edge inside one component. The *content* still gets inset 0
                // and the full measure; it is only the heading that moves.
                .padding(.leading, CCColumn.step(from: columnInset, scaledDot: scaledDot))
                .frame(minHeight: CC.size.hitTarget)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(CCDisclosureStyle())
            .accessibilityLabel(label)
            .accessibilityValue(isExpanded.wrappedValue ? "Expanded" : "Collapsed")
            .accessibilityHint(
                isExpanded.wrappedValue ? "Double tap to collapse" : "Double tap to expand")

            if isExpanded.wrappedValue {
                content()
                    // Inset 0 — full content width. What is inside is
                    // verbatim text, and verbatim text belongs on the same left
                    // edge as everything else that has to be checked.
                    .padding(.vertical, CC.space.sm)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .transition(.opacity)
            }
        }
        .accessibilityElement(children: .contain)
    }
}

private struct CCDisclosureStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            // A row lightens, it does not scale.
            .background(configuration.isPressed ? CC.color.surfaceRaised : Color.clear)
            .ccAnimation(CC.motion.micro, value: configuration.isPressed)
    }
}
