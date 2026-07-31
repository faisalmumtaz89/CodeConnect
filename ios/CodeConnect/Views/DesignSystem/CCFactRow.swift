import SwiftUI

// =============================================================================
//  CCFactRow — label left, value right, 48pt.
// =============================================================================

/// How a fact row sets its label.
enum CCFactLabelStyle {
    /// `body` `textSecondary`. The label *names* a fact and the value answers
    /// it: `Round trip` / `18ms`. The common case.
    case prose
    /// `mono` `text`. The row **is** the identifier — a pinned host, a file
    /// path — and whatever sits at the trailing edge is its age.
    case identifier
    /// `mono` `textSecondary`. A machine-written key the app did not choose the
    /// spelling of, whose *value* is the point: `answer_path` / `hook`.
    case key
}

/// A fact and its value: `body` `textSecondary` label on the left, the value
/// on the right in `mono`, 48pt tall, 1pt `border` between.
///
/// Explicitly not `LabeledContent` — that control's type scale, its insets and
/// its colours are the system's, and it is drawn for a light-mode grouped list.
///
/// It exists because the same fifteen lines were being written out by hand on
/// Link Health, in Settings and beside every pinned host key, and three
/// hand-written copies of one row is three chances for one of them to print a
/// measurement in the wrong colour. Three rules it enforces that a hand-written
/// row forgets:
///
///  * **A value nobody measured renders `—`, never `0`.** `0` is a measurement.
///    That is what `isUnmeasured` is for, and it is the one permitted use of
///    `textDisabled` here — an em dash beside a full-contrast label is the
///    "inactive component" case exactly.
///  * **A state takes its semantic colour; a measurement stays `text`.**
///  * **Every fact carries its age**, in `monoSmall`, at the trailing edge.
///
/// **It lands on the content column itself** (`CCColumn.content`). It used to
/// inset its text 16 from the card edge, which is 32 on screen — the *gutter*,
/// where the `CCRow`s in the card above put their status dots, not their titles.
/// Link Health and Settings therefore ran two text edges 20pt apart on one
/// screen, and two screens carried a hand-written 20pt shim at every
/// call site to close it. The number belongs to the component: a fact row and a
/// list row now begin in the same place without anybody adding anything.
struct CCFactRow<Value: View, Detail: View>: View {
    let label: String
    var labelStyle: CCFactLabelStyle = .prose
    /// How old this fact is. `monoSmall` `textTertiary`, hard against the
    /// trailing edge, after the value.
    var age: String?
    var separator: Bool = true
    /// What VoiceOver reads for the value, when the visible form is an
    /// abbreviation or a glyph.
    var accessibilityValueText: String?

    private let value: () -> Value
    private let detail: () -> Detail

    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.ccColumnInset) private var columnInset
    /// Same ramp and ceiling as `CCStatusDot` and `CCSectionHeader`: the content
    /// column moves at accessibility sizes to clear a grown mark, and a fact row
    /// that stayed on a constant 36 would part company with the rows above it.
    @ScaledMetric(relativeTo: .footnote) private var scaledDot: CGFloat = CC.size.dot

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised memberwise
    /// one, and the two collide over the private closure properties.
    init(
        _ label: String,
        labelStyle: CCFactLabelStyle = .prose,
        age: String? = nil,
        separator: Bool = true,
        accessibilityValueText: String? = nil,
        @ViewBuilder value: @escaping () -> Value,
        @ViewBuilder detail: @escaping () -> Detail
    ) {
        self.label = label
        self.labelStyle = labelStyle
        self.age = age
        self.separator = separator
        self.accessibilityValueText = accessibilityValueText
        self.value = value
        self.detail = detail
    }

    var body: some View {
        VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: CC.space.xs) {
                CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xxs) {
                    labelText
                    Spacer(minLength: CC.space.xs)
                    value()
                    ageText
                }
                detail()
                    // The row has just stepped its own content onto the content
                    // column, so it says where that column ended up. A nested
                    // surface in this slot — the daemon's verbatim note, a
                    // fingerprint — then hangs its border 12pt left of the
                    // label above it instead of adding a third text edge under
                    // it — the two-edge rule's corollary; see
                    // `CCColumn.hang(from:)`.
                    .ccColumnInset(
                        max(columnInset, CCColumn.content(scaledDot: scaledDot)))
            }
            // The *remainder* of the content column, not the whole of it: a fact
            // row inside a `CCCard` that has already stepped out would otherwise
            // land its label on 72. `CCCard` reports its own inset; this asks
            // for what is left. Free-standing — the common case, a
            // `CCCard(padding: 0)` full of rows — the remainder is all 36 of it.
            .padding(.leading, CCColumn.step(from: columnInset, scaledDot: scaledDot))
            .padding(.trailing, CC.space.md)
            .padding(.vertical, CC.space.sm)
            .frame(minHeight: CC.size.factRow)
            .frame(maxWidth: .infinity, alignment: .leading)

            if separator { CCHairline() }
        }
        // A row whose detail is a `CCMonoBlock` or a `CCFingerprint` keeps those
        // as their own elements: the block has a copy button and the
        // fingerprint spells itself character by character, and `.combine`
        // would flatten both into one unreadable run.
        .accessibilityElement(children: Detail.self == EmptyView.self ? .combine : .contain)
        .accessibilityLabel(spokenLabel)
    }

    @ViewBuilder
    private var labelText: some View {
        switch labelStyle {
        case .prose:
            Text(label)
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
        case .identifier, .key:
            Text(label)
                .ccType(CC.type.mono)
                .foregroundStyle(labelStyle == .identifier ? CC.text.primary : CC.text.secondary)
                // A hostname's *tail* identifies it, so the head is what goes.
                // At accessibility sizes it wraps instead: an identifier is
                // never truncated when there is room to wrap it.
                .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                .truncationMode(.head)
                .fixedSize(horizontal: false, vertical: typeSize.isAccessibilitySize)
        }
    }

    @ViewBuilder
    private var ageText: some View {
        if let age {
            Text(age)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize()
        }
    }

    private var spokenLabel: String {
        [label, accessibilityValueText, age].compactMap { $0 }.joined(separator: ", ")
    }
}

// MARK: - The string form of a value

/// A fact's value as a plain string.
///
/// Its own view rather than an `if let` inside `CCFactRow` so the string form
/// and the composed form are the same slot, and a row cannot grow two values.
struct CCFactValue: View {
    let text: String
    var tone: CCTone = .neutral
    /// Nothing measured this. Renders in `textDisabled` — the one permitted
    /// use of that token in a row, and what makes "not measured" look
    /// different from "zero".
    var isUnmeasured: Bool = false

    var body: some View {
        Text(text)
            .ccType(CC.type.mono)
            .foregroundStyle(colour)
            .multilineTextAlignment(.trailing)
            .fixedSize(horizontal: false, vertical: true)
    }

    /// Asked of `CCMeasured` rather than decided here, so this row and a
    /// `CCStat` column cannot render one state two ways. This was the half that
    /// was already right; it is routed through the shared rule anyway, because a
    /// rule with one correct copy and one missing copy is how the defect
    /// happened.
    private var colour: Color {
        CCMeasured.color(text, tone: tone, flagged: isUnmeasured)
    }
}

// MARK: - Convenience initialisers

extension CCFactRow where Value == CCFactValue, Detail == EmptyView {
    init(
        _ label: String,
        value: String,
        labelStyle: CCFactLabelStyle = .prose,
        tone: CCTone = .neutral,
        isUnmeasured: Bool = false,
        age: String? = nil,
        separator: Bool = true,
        accessibilityValueText: String? = nil
    ) {
        self.init(
            label, labelStyle: labelStyle, age: age, separator: separator,
            accessibilityValueText: accessibilityValueText
                ?? CCMeasured.spoken(value, flagged: isUnmeasured),
            value: { CCFactValue(text: value, tone: tone, isUnmeasured: isUnmeasured) },
            detail: { EmptyView() })
    }
}

extension CCFactRow where Detail == EmptyView {
    init(
        _ label: String,
        labelStyle: CCFactLabelStyle = .prose,
        age: String? = nil,
        separator: Bool = true,
        accessibilityValueText: String? = nil,
        @ViewBuilder value: @escaping () -> Value
    ) {
        self.init(
            label, labelStyle: labelStyle, age: age, separator: separator,
            accessibilityValueText: accessibilityValueText, value: value,
            detail: { EmptyView() })
    }
}

extension CCFactRow where Value == EmptyView {
    /// A fact whose body is underneath it: a pinned key's fingerprint, a
    /// daemon's verbatim error. The trailing edge carries the age and nothing
    /// else.
    init(
        _ label: String,
        labelStyle: CCFactLabelStyle = .prose,
        age: String? = nil,
        separator: Bool = true,
        accessibilityValueText: String? = nil,
        @ViewBuilder detail: @escaping () -> Detail
    ) {
        self.init(
            label, labelStyle: labelStyle, age: age, separator: separator,
            accessibilityValueText: accessibilityValueText, value: { EmptyView() },
            detail: detail)
    }
}
