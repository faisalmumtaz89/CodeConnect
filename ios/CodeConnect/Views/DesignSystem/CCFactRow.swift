import SwiftUI

// =============================================================================
//  CCFactRow — label left, value right, 48pt.
// =============================================================================

/// How a fact row sets its label.
enum CCFactLabelStyle {
    /// `body` `textSecondary`. The label *names* a fact and the value answers
    /// it: `Round trip` / `18ms`. The common case.
    case prose
    /// `mono` `text`. The row **is** the identifier — the daemon's host name —
    /// and whatever sits at the trailing edge is its age.
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
/// Link Health and in Settings, and hand-written copies of one row are that
/// many chances for one of them to print a measurement in the wrong colour.
/// Three rules it enforces that a hand-written row forgets:
///
///  * **A value nobody measured renders `—`, never `0`.** `0` is a measurement.
///    That is what `isUnmeasured` is for, and it is the one permitted use of
///    `textDisabled` here — an em dash beside a full-contrast label is the
///    "inactive component" case exactly.
///  * **A state takes its semantic colour; a measurement stays `text`.**
///  * **Every fact carries its age**, in `monoSmall`, at the trailing edge.
///
/// **It lands on its container's edge** — 16 inside a zero-padding card, 32 on
/// screen — because a fact row carries no mark, and the kit's law (written at
/// `CCRow`'s gutter) is that *a row with no mark keeps its edge rather than
/// reserving a column for nothing*. This row has been at both numbers: an
/// early fix moved it out to the dot column to match the *marked* rows of one
/// Settings card, which read as harmony there and as a phantom indent on every
/// screen where no mark exists — the settings sheet, the link sheet, the
/// terminal settings, which is all twenty call sites. Marked `CCRow`s keep 52
/// because their mark earns it; this row never has one.
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
                    // Where this row's own text edge ended up, so a nested
                    // surface in this slot — the daemon's verbatim note — hangs
                    // from the label above it instead of adding a second text
                    // edge under it. See `CCColumn.hang(from:)`.
                    .ccColumnInset(max(columnInset, CC.space.md))
            }
            // **The container's edge, not the content column.** A fact row is a
            // key–value line with no mark in front of it, and text without a
            // mark sits on its container's edge — the rule `CCSectionHeader`
            // (dotless) and the fleet's ended footer already follow. This used
            // to step every label out to the dot column, 52 on screen, and
            // since no fact row anywhere in the app sits beside a mark
            // (verified across all twenty call sites), that read as a phantom
            // indent against the header above it — on the settings sheet, the
            // link sheet, and the terminal settings alike. The remainder logic
            // stays for the container that already padded itself.
            .padding(.leading, max(0, CC.space.md - columnInset))
            .padding(.trailing, CC.space.md)
            .padding(.vertical, CC.space.sm)
            .frame(minHeight: CC.size.factRow)
            .frame(maxWidth: .infinity, alignment: .leading)

            if separator { CCHairline() }
        }
        // A row whose detail is a `CCMonoBlock` keeps it as its own element:
        // the block has a copy button and its own spoken form, and `.combine`
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
    /// A fact whose body is underneath it: a daemon's verbatim error, a command
    /// as it will be run. The trailing edge carries the age and nothing else.
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
