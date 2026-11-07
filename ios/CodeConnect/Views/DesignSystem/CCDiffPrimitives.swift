import CodeConnectKit
import SwiftUI
import UIKit

// =============================================================================
//  CCDiffRow / CCHunkHeader / CCFoldRow — the diff grid.
//
//  "Beating GitHub-mobile-web at reading an agent diff is a top-three v1
//  objective." Everything in this file exists to make a monospace grid readable
//  on a 393pt screen at 2am, so the rules are tighter than elsewhere:
//
//    * The change signal is two parts, as in VS Code: a `+` / `−` marker
//      glyph, so the kind of change never rests on red and green alone, and a
//      **12%** tint across the whole row.
//    * The *background* carries the word-level diff, never the foreground. Code
//      must not become harder to read in the act of being highlighted.
//    * One measurement, handed down. A monospaced font has a single advance, so
//      the wrap column count is arithmetic rather than a guess — and because
//      every row shares one `CCDiffMetrics`, the text and its hanging
//      continuations can never be laid out against two different widths.
// =============================================================================

// MARK: - Metrics

/// Everything about the diff's typography that depends on the width it got.
///
/// The type size here is **not** a design token, and that is deliberate: diff
/// size is a pinch-scaled user preference over 9…24pt, persisted in
/// `AppSettings.diffFontSize`. A token cannot express a value the user sets with
/// their fingers. Every *other* size in the grid — gutter, marker, wrap column —
/// is derived from it, so there is still exactly one number in play.
struct CCDiffMetrics: Equatable {
    /// How much of the width the line numbers are worth.
    enum Gutter: Equatable {
        /// Old and new, side by side. The default.
        case dual
        /// New only. Buys back four advances at `xLarge` and above.
        case single
        /// No gutter column at all; each line is prefixed with its number in
        /// `textDisabled`. `accessibility1` and above, where four advances is a
        /// quarter of the visible line.
        case inline
    }

    var fontSize: Double
    var advance: CGFloat
    var gutterWidth: CGFloat
    var markerWidth: CGFloat
    var columns: Int
    var gutter: Gutter

    init(fontSize: Double, availableWidth: CGFloat, gutter: Gutter = .dual) {
        self.fontSize = fontSize
        self.gutter = gutter
        let font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        let advance = max(("0" as NSString).size(withAttributes: [.font: font]).width, 1)
        self.advance = advance
        switch gutter {
        // Two four-digit line numbers and a separator.
        case .dual: gutterWidth = advance * 9
        case .single: gutterWidth = advance * 5
        case .inline: gutterWidth = 0
        }
        markerWidth = advance * 2
        let text = availableWidth - gutterWidth - markerWidth - 8
        columns = max(8, Int(text / advance))
    }

    /// The gutter mode for a Dynamic Type size. Mono grid content scales
    /// with type up to `xxLarge` and then **changes layout instead of size** —
    /// clipping a line of code or truncating a command is never an option.
    static func gutter(for typeSize: DynamicTypeSize) -> Gutter {
        if typeSize.isAccessibilitySize { return .inline }
        return typeSize >= .xLarge ? .single : .dual
    }

    /// How much Dynamic Type multiplies the user's chosen diff size by, capped
    /// at `xxLarge`. Past that the layout changes and the glyphs do not
    /// keep growing, or a 24pt line of code at AX5 would be four characters wide.
    static func typeScale(for typeSize: DynamicTypeSize) -> Double {
        switch typeSize {
        case .xSmall: return 0.86
        case .small: return 0.90
        case .medium: return 0.95
        case .large: return 1.0
        case .xLarge: return 1.08
        default: return 1.16
        }
    }
}

// MARK: - Word-level highlighting

/// Which characters actually changed between a deletion and the addition that
/// replaced it.
///
/// The single biggest readability win in the surface: on a line where one
/// argument moved, a whole-line tint says "something here" and this says
/// *what*. Computed with a character-level LCS, skipped when either side is over
/// 400 characters — the quadratic cost is invisible on code and unbounded on a
/// minified bundle.
enum CCDiffWordHighlight {
    static let maximumLength = 400

    /// Character ranges (as offsets into each string) that differ.
    ///
    /// Returns empty ranges when the two lines have too little in common to be
    /// a rewrite of each other: highlighting 90% of both lines is not
    /// highlighting, it is a second tint that carries no information.
    static func ranges(deletion: String, addition: String) -> (
        deletion: [Range<Int>], addition: [Range<Int>]
    ) {
        let old = Array(deletion)
        let new = Array(addition)
        guard !old.isEmpty, !new.isEmpty,
            old.count <= maximumLength, new.count <= maximumLength
        else { return ([], []) }

        let table = lcsTable(old, new)
        let common = table[old.count][new.count]
        // Below a quarter in common these are two different lines that happen to
        // sit next to each other, not an edit.
        guard common * 4 >= max(old.count, new.count) else { return ([], []) }

        var oldChanged = [Bool](repeating: true, count: old.count)
        var newChanged = [Bool](repeating: true, count: new.count)
        var i = old.count
        var j = new.count
        while i > 0 && j > 0 {
            if old[i - 1] == new[j - 1], table[i][j] == table[i - 1][j - 1] + 1 {
                oldChanged[i - 1] = false
                newChanged[j - 1] = false
                i -= 1
                j -= 1
            } else if table[i - 1][j] >= table[i][j - 1] {
                i -= 1
            } else {
                j -= 1
            }
        }
        return (runs(of: oldChanged), runs(of: newChanged))
    }

    private static func lcsTable(_ old: [Character], _ new: [Character]) -> [[Int]] {
        var table = [[Int]](
            repeating: [Int](repeating: 0, count: new.count + 1), count: old.count + 1)
        for i in 1...old.count {
            for j in 1...new.count {
                table[i][j] =
                    old[i - 1] == new[j - 1]
                    ? table[i - 1][j - 1] + 1
                    : max(table[i - 1][j], table[i][j - 1])
            }
        }
        return table
    }

    /// Collapses a per-character flag array into ranges, and drops a lone
    /// changed character sitting between two unchanged ones — a one-glyph
    /// highlight is confetti, not information.
    private static func runs(of flags: [Bool]) -> [Range<Int>] {
        var ranges: [Range<Int>] = []
        var start: Int?
        for (index, changed) in flags.enumerated() {
            if changed, start == nil { start = index }
            if !changed, let open = start {
                ranges.append(open..<index)
                start = nil
            }
        }
        if let open = start { ranges.append(open..<flags.count) }
        return ranges
    }

    /// Pairs each deletion with the addition that replaced it, for a whole hunk.
    ///
    /// Keyed by `UnifiedDiff.Line.id`, which the parser mints once per line
    /// across the entire document, so one map serves every file.
    static func map(for lines: [UnifiedDiff.Line]) -> [Int: [Range<Int>]] {
        var result: [Int: [Range<Int>]] = [:]
        var index = 0
        while index < lines.count {
            guard lines[index].kind == .deletion else {
                index += 1
                continue
            }
            var deletions: [UnifiedDiff.Line] = []
            while index < lines.count, lines[index].kind == .deletion {
                deletions.append(lines[index])
                index += 1
            }
            var additions: [UnifiedDiff.Line] = []
            while index < lines.count, lines[index].kind == .addition {
                additions.append(lines[index])
                index += 1
            }
            for offset in 0..<min(deletions.count, additions.count) {
                // On the text as the row draws it, so a range lands on the
                // characters it names in a tab-indented file too.
                let pair = ranges(
                    deletion: DiffLineWrap.expandingTabs(deletions[offset].text),
                    addition: DiffLineWrap.expandingTabs(additions[offset].text))
                if !pair.deletion.isEmpty { result[deletions[offset].id] = pair.deletion }
                if !pair.addition.isEmpty { result[additions[offset].id] = pair.addition }
            }
        }
        return result
    }
}

// MARK: - Line

/// One diff line: gutter numbers, a marker, a 12% tint, word-level
/// highlighting, and soft wrapping that hangs at the line's own indent.
struct CCDiffRow: View {
    let line: UnifiedDiff.Line
    let metrics: CCDiffMetrics
    /// Character offsets that changed relative to the paired line.
    var wordRanges: [Range<Int>] = []

    private var fontSize: Double { metrics.fontSize }

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            if metrics.gutter != .inline {
                Text(gutterText)
                    .font(.system(size: fontSize - 1, design: .monospaced).monospacedDigit())
                    // One of the few positions `textDisabled` is permitted in:
                    // a line number is not a fact you have to read, it is a
                    // coordinate you look up.
                    .foregroundStyle(CC.text.disabled)
                    .frame(width: metrics.gutterWidth, alignment: .trailing)
                    .padding(.trailing, CC.space.xxs)
            }

            marker

            VStack(alignment: .leading, spacing: 0) {
                ForEach(segments) { segment in
                    segmentView(segment)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.leading, CC.space.xxs)
        }
        .fixedSize(horizontal: false, vertical: true)
        // In a rectangle, said out loud: a bare colour fills the container's
        // shape, and inside the hunk's rounded card that rounded every row's
        // ends, a notch at each seam down both edges.
        .background(rowTint, in: Rectangle())
        // The line as it is in the file, real tabs and every wrapped piece,
        // not as drawn: selecting the drawn text would copy expanded tabs and
        // only the piece that was pressed.
        .contextMenu {
            Button { CCPasteboard.copy(line.text) } label: {
                Label { Text("Copy line") } icon: { Image("copy") }
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityAction(named: "Copy line") { CCPasteboard.copy(line.text) }
    }

    // MARK: Marker column

    private var marker: some View {
        // At the top, not centred: a centred glyph on a wrapped row drifts down
        // beside the *continuation*, measured on a two-line addition where the
        // `+` landed next to `load, deadline:` and the first line read as
        // context. The marker belongs to the line, and the line starts at the top.
        Text(markerGlyph)
            .font(.system(size: fontSize, design: .monospaced))
            .foregroundStyle(changeTone ?? CC.text.disabled)
            .frame(width: metrics.markerWidth, alignment: .top)
            .accessibilityHidden(true)
    }

    // MARK: Text

    /// What is actually laid out: in `inline` gutter mode the line number is
    /// prefixed into the string so it wraps with it, rather than sitting in a
    /// column that mode exists to delete.
    private var renderedText: String { numberPrefix + drawnText }

    /// The line as drawn: tabs at 4-column stops, so the wrap column, the
    /// hanging indent and the word highlights count the columns the reader
    /// sees. Copying a line or a hunk keeps its real tabs.
    private var drawnText: String { DiffLineWrap.expandingTabs(line.text) }

    private var numberPrefix: String {
        guard metrics.gutter == .inline else { return "" }
        guard let number = line.newNumber ?? line.oldNumber else { return "" }
        return "\(number) "
    }

    /// One wrapped run, split into the parts that are drawn in different inks.
    private struct Segment: Identifiable {
        /// Wrap index. `0` is the first line; every other one hangs.
        let id: Int
        /// The inline line number, when there is one and this is where it fell.
        let prefix: String
        let code: String
        /// Where `code` starts inside `drawnText`, so a word-level highlight
        /// computed against the whole line can be placed on this run.
        let codeStart: Int
    }

    private var segments: [Segment] {
        let wrapped = DiffLineWrap.wrap(
            renderedText, columns: metrics.columns, continuationIndent: continuationIndent)
        var out: [Segment] = []
        var consumed = 0
        for (index, piece) in wrapped.enumerated() {
            let characters = Array(piece)
            let prefixCount = max(0, min(numberPrefix.count - consumed, characters.count))
            out.append(
                Segment(
                    id: index,
                    prefix: String(characters[0..<prefixCount]),
                    code: String(characters[prefixCount...]),
                    codeStart: max(0, consumed + prefixCount - numberPrefix.count)))
            consumed += characters.count
        }
        return out
    }

    /// Columns a continuation gives up: it hangs where the line's code starts,
    /// past an inline number and the line's own leading whitespace, up to half
    /// the line, so a wrapped line reads as one statement.
    private var continuationIndent: Int {
        let leading = drawnText.prefix { $0 == " " }.count
        return min(numberPrefix.count + leading, metrics.columns / 2)
    }

    private var gutterText: String {
        let new = (line.newNumber.map(String.init) ?? " ").padded(4)
        guard metrics.gutter == .dual else { return new }
        let old = (line.oldNumber.map(String.init) ?? " ").padded(4)
        return "\(old) \(new)"
    }

    private var markerGlyph: String {
        switch line.kind {
        case .addition: return "+"
        case .deletion: return "−"
        case .context: return " "
        case .note: return "\\"
        }
    }

    private var changeTone: Color? {
        switch line.kind {
        case .addition: return CC.color.success
        case .deletion: return CC.color.danger
        case .context, .note: return nil
        }
    }

    /// 12% over `bg`, pinned as a literal in DesignKit so a row tint cannot drift
    /// with an opacity change somewhere.
    private var rowTint: Color {
        switch line.kind {
        case .addition: return CC.color.successMuted
        case .deletion: return CC.color.dangerMuted
        case .context, .note: return CC.color.bg
        }
    }

    private var wordTint: Color? {
        switch line.kind {
        case .addition: return CC.color.successWord
        case .deletion: return CC.color.dangerWord
        case .context, .note: return nil
        }
    }

    private var mono: Font { .system(size: fontSize, design: .monospaced) }

    /// One wrapped run: the continuation glyph, the inline line number, the code,
    /// and the word-level highlight drawn **behind** it.
    ///
    /// The highlight is a rectangle placed by arithmetic rather than a text
    /// attribute. Two reasons, and the second is the real one: `AttributedString`
    /// background runs cost a rope splice per run per layout pass on a surface
    /// that has to scroll a thousand rows, and a monospaced font already gives
    /// this view an exact advance width — the same number the wrap column count
    /// is derived from — so `offset × advance` lands on the glyph every time.
    private func segmentView(_ segment: Segment) -> some View {
        HStack(alignment: .top, spacing: 0) {
            if segment.id > 0 {
                Color.clear
                    .frame(width: metrics.advance * CGFloat(continuationIndent), height: 0)
            }
            if !segment.prefix.isEmpty {
                Text(segment.prefix)
                    .font(mono)
                    .foregroundStyle(CC.text.disabled)
                    .fixedSize()
                    .frame(
                        width: metrics.advance * CGFloat(segment.prefix.count),
                        alignment: .leading)
            }
            Text(segment.code)
                .font(mono)
                .foregroundStyle(CC.text.primary)
                .background(alignment: .topLeading) { highlight(segment) }
        }
    }

    @ViewBuilder
    private func highlight(_ segment: Segment) -> some View {
        if let wordTint, !wordRanges.isEmpty {
            let runs = highlightRuns(in: segment)
            if !runs.isEmpty {
                ZStack(alignment: .topLeading) {
                    ForEach(Array(runs.enumerated()), id: \.offset) { _, run in
                        Rectangle()
                            .fill(wordTint)
                            .frame(width: CGFloat(run.count) * metrics.advance)
                            .offset(x: CGFloat(run.lowerBound) * metrics.advance)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    /// The changed runs, clipped to this segment and expressed in *its* own
    /// character offsets.
    private func highlightRuns(in segment: Segment) -> [Range<Int>] {
        let start = segment.codeStart
        let end = start + segment.code.count
        return wordRanges.compactMap { range in
            let lower = max(range.lowerBound, start)
            let upper = min(range.upperBound, end)
            guard lower < upper else { return nil }
            return (lower - start)..<(upper - start)
        }
    }

    private var accessibilityLabel: String {
        let kind: String
        switch line.kind {
        case .addition: kind = "added"
        case .deletion: kind = "removed"
        case .context: kind = "unchanged"
        case .note: kind = "note"
        }
        let number = line.newNumber ?? line.oldNumber
        let prefix = number.map { "line \($0), \(kind)" } ?? kind
        return "\(prefix): \(line.text.isEmpty ? "blank" : line.text)"
    }
}

// MARK: - Hunk actions

/// The hunk's actions, as **data rather than as a pre-built menu**.
///
/// Handing the header a finished `Menu` was how the product ended up with one
/// route to these three actions and it 13pt wide. As values, the kit can build
/// the menu, the glyph, the target and the gesture once — and the list itself
/// stops being something a call site can get short.
///
/// Every action is optional so a header can offer a subset without the caller
/// assembling a conditional `ViewBuilder`; a header given none of them draws no
/// control and is not a button.
struct CCHunkActions {
    var comment: (() -> Void)?
    var copyHunk: (() -> Void)?
    var copyPath: (() -> Void)?

    init(
        comment: (() -> Void)? = nil,
        copyHunk: (() -> Void)? = nil,
        copyPath: (() -> Void)? = nil
    ) {
        self.comment = comment
        self.copyHunk = copyHunk
        self.copyPath = copyPath
    }

    var isEmpty: Bool { comment == nil && copyHunk == nil && copyPath == nil }
}

// MARK: - Hunk header

/// `@@ -12,7 +12,9 @@ func send(_ text: String)` — 28pt, a quiet separator
/// between two hairlines, and the machine part separated from the human part.
///
/// **The header is the hunk's control, and the gesture lives here because here
/// is the only place it can survive.** The intended gesture was a 0.4s
/// long-press anywhere in a hunk — the thesis of the whole Diff surface;
/// measured, 1.2s presses at three positions inside three hunks at both type
/// sizes produced no menu and no lift. A `.contextMenu` on the hunk container
/// cannot win — every `CCDiffRow` inside it owns its long press, for Copy line.
/// That is not a bug to be tuned; it is two features asking for one gesture on
/// one rectangle.
///
/// So the gesture moved to the one strip of the hunk that is not a row. The header carries the actions as a menu — tap or press-and-hold, both
/// open it — which also hands Switch Control and VoiceOver a real button where
/// before they had a long-press they could not perform.
///
/// **A header that carries actions is 44pt; a header that is only a label is
/// 28.** The 28 was chosen when this band was a caption. It is now the hunk's
/// control, and this product's rule for a control is `CC.size.hitTarget` — the
/// same call `CCFoldRow` in this file already made and documented ("44pt, not
/// the ~26pt it replaced: this is a control, and a control is 44pt"). Density
/// is paid only where a control exists.
///
/// The alternative was measured and rejected. Drawing 28 and expressing the
/// remaining 16 as `.contentShape(Rectangle().inset(by: -8).offset(y: 8))` —
/// the shape reaching down into the hunk — **reports** a 44pt frame to the
/// accessibility tree and is not hit-tested: a coordinate tap 8pt below the
/// drawn band did not open the menu, while the same build's long-press on the
/// band did. A target that measures 44 in the tree and 28 under a thumb is worse
/// than an honest 28: it claims a reachability it does not have, so nothing
/// downstream — a measurement, a screenshot, an accessibility inspector — can
/// see the miss.
///
/// The height is also no longer the caller's to set. It shipped at **43.67pt**
/// because the trailing control carried its own `.ccHitTarget()`, and a
/// component whose band height is decided by whatever a screen wrapped its glyph
/// in has no height at all.
struct CCHunkHeader<Actions: View>: View {
    let header: String
    var fontSize: Double
    /// The kit-owned form. When present, this component draws the control.
    private var hunkActions: CCHunkActions?
    private let menu: () -> Actions

    /// **Legacy.** Takes a finished control for the trailing slot.
    ///
    /// Kept only so the diff screen keeps compiling while its call sites move
    /// over to `actions:`. It cannot carry the long-press — the component has
    /// no idea what the control does — so a header built this way is menu-only.
    /// The slot is clamped to the header's own height so that whatever the
    /// caller wrapped it in can no longer inflate the band.
    init(header: String, fontSize: Double, @ViewBuilder menu: @escaping () -> Actions) {
        self.header = header
        self.fontSize = fontSize
        self.hunkActions = nil
        self.menu = menu
    }

    fileprivate init(header: String, fontSize: Double, hunkActions: CCHunkActions)
    where Actions == EmptyView {
        self.header = header
        self.fontSize = fontSize
        self.hunkActions = hunkActions
        self.menu = { EmptyView() }
    }

    /// 28 for a caption; `CC.size.hitTarget` once the band is a control.
    private var bandHeight: CGFloat {
        isControl ? CC.size.hitTarget : CC.space.xl + CC.space.xxs
    }

    private var isControl: Bool {
        if let hunkActions { return !hunkActions.isEmpty }
        return false
    }

    var body: some View {
        if let hunkActions, !hunkActions.isEmpty {
            Menu {
                menuItems(hunkActions)
            } label: {
                band { ellipsis }
            }
            .buttonStyle(.plain)
            // The whole strip, and all of it real: the shape is the band, and
            // the band is 44. See the type's note for the 28-plus-content-shape
            // version and the coordinate tap that disproved it.
            .contentShape(Rectangle())
            .accessibilityLabel("Actions for this hunk")
            .accessibilityHint(header)
        } else {
            band {
                menu()
                    // Clamped, so a caller's own hit target cannot decide this
                    // band's height. See the type's note: this is the 43.67.
                    .frame(height: bandHeight)
            }
            .accessibilityElement(children: .contain)
        }
    }

    private func band<Trailing: View>(@ViewBuilder trailing: () -> Trailing) -> some View {
        HStack(spacing: CC.space.xs) {
            Text(machinePart)
                .font(.system(size: fontSize - 1, design: .monospaced).monospacedDigit())
                .foregroundStyle(CC.text.tertiary)
                .lineLimit(1)
            if let context = contextPart {
                Text(context)
                    .font(.system(size: fontSize - 1, design: .monospaced))
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: CC.space.xxs)
            trailing()
        }
        .padding(.horizontal, CC.space.xs)
        .frame(minHeight: bandHeight, alignment: .leading)
        .frame(maxWidth: .infinity, alignment: .leading)
        // A separator, not a band: the page's own ground between two hairlines.
        .overlay(alignment: .top) { CCHairline() }
        .overlay(alignment: .bottom) { CCHairline() }
    }

    /// The affordance, and nothing more — the target is the band.
    ///
    /// Sized off `fontSize` rather than through `CCIcon`, which is the one place
    /// in the kit that is right: **the diff grid does not ride Dynamic Type.**
    /// Diff size is a pinch-scaled user preference and every dimension
    /// in this file derives from it, so a glyph on a `@ScaledMetric` ramp grows
    /// while the header text beside it stays put — measured at AX5, where the
    /// three dots stood about twice the cap height of the `@@ -12,7 +12,9 @@`
    /// they were sitting next to. Reachability is not what the glyph was
    /// carrying anyway; the 44pt band is.
    private var ellipsis: some View {
        Image(CCIcon.asset("dots-three", weight: .semibold))
            .resizable()
            .scaledToFit()
            .frame(width: fontSize * CCIcon.artScale, height: fontSize * CCIcon.artScale)
            .foregroundStyle(CC.text.tertiary)
            .accessibilityHidden(true)
    }

    @ViewBuilder
    private func menuItems(_ actions: CCHunkActions) -> some View {
        if let comment = actions.comment {
            Button(action: comment) {
                Label { Text("Comment to agent") } icon: { Image("chat-text") }
            }
        }
        if let copyHunk = actions.copyHunk {
            Button(action: copyHunk) {
                Label { Text("Copy hunk") } icon: { Image("copy") }
            }
        }
        if let copyPath = actions.copyPath {
            Button(action: copyPath) {
                Label { Text("Copy file path") } icon: { Image("copy") }
            }
        }
    }

    /// The `@@ … @@` prefix, verbatim.
    private var machinePart: String {
        guard let close = header.range(of: " @@") else { return header }
        return String(header[..<close.upperBound])
    }

    /// Whatever git appended after it — usually the enclosing function.
    private var contextPart: String? {
        guard let close = header.range(of: " @@") else { return nil }
        let rest = header[close.upperBound...].trimmingCharacters(in: .whitespaces)
        return rest.isEmpty ? nil : rest
    }
}

extension CCHunkHeader where Actions == EmptyView {
    init(header: String, fontSize: Double) {
        self.init(header: header, fontSize: fontSize, menu: { EmptyView() })
    }

    /// **The form to use.** The kit draws the control, owns the 44pt target and
    /// carries the press-and-hold gesture.
    ///
    /// Deliberately not a `@ViewBuilder`: a value argument cannot be confused
    /// with the legacy `menu:` trailing closure, so both initialisers can exist
    /// while the diff screen migrates without a single call site becoming
    /// ambiguous.
    init(header: String, fontSize: Double, actions: CCHunkActions) {
        self.init(header: header, fontSize: fontSize, hunkActions: actions)
    }
}

// MARK: - File chip

/// One file in the horizontal strip above the diff.
///
/// Not a `CCBadge`: a badge uppercases its label, and a file name is an
/// identifier where casing is part of the identity — `Sender.swift` and
/// `SENDER.SWIFT` are not the same path on a case-sensitive checkout, and this
/// product's rule is that identifiers are monospace, never SF Pro small-caps.
/// **There is no selected state, and that is a decision.**
///
/// The intended one — a Vercel `#EDEDED` fill marking "the file the reader is
/// currently looking at" — shipped as an `isSelected` flag that
/// **no call site ever set**: the strip is a jump list, and the document does not
/// tell anybody which file is under the fold. Wiring it to the last chip *tapped*
/// would mark the wrong file the moment the reader scrolled, which on this
/// product's terms is worse than marking none; and the only honest source — the
/// pinned header's own position — is scroll-varying state on the one screen where
/// a `GeometryReader`-plus-`PreferenceKey` loop has already cost this app its
/// idle state mid-drag (see `DecisionCardView.probe`). So the flag is gone rather
/// than left looking implemented. `CCBadge(isSelected:)` still carries the
/// signature for controls whose state the app actually knows.
struct CCDiffFileChip: View {
    let file: UnifiedDiff.FileDiff
    /// The widest the chip may be: the strip's own width, so the counts stay on
    /// screen and a long name gives way, from its middle, instead.
    var maxWidth: CGFloat? = nil
    let action: () -> Void

    var body: some View {
        Button {
            // No haptic: scrolling somewhere is not a commitment.
            action()
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 2) {
                CCIcon(file.status.symbol, size: 10, weight: .semibold, relativeTo: .caption)
                    .foregroundStyle(CC.text.tertiary)
                    .ccMark(beside: CC.type.monoSmall)
                Text(file.shortName)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("+\(file.additions)")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.color.success)
                    .fixedSize()
                Text("−\(file.deletions)")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.color.danger)
                    .fixedSize()
            }
            .padding(.horizontal, CC.space.sm)
            .frame(maxWidth: maxWidth)
            // The floor is capped (`chipMaxScale`); the content still grows the
            // chip wherever it actually needs to.
            .frame(minHeight: min(chipHeight, CC.size.chip * CC.size.chipMaxScale))
            .background(CC.color.surfaceRaised, in: Capsule())
            .overlay {
                Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
            }
            // 32pt of chip, 44pt of finger.
            .frame(minHeight: CC.size.hitTarget)
            .contentShape(Rectangle())
        }
        .buttonStyle(CCDiffChipStyle())
        .accessibilityLabel(
            "\(file.shortName), \(file.status.label), \(file.additions) added, \(file.deletions) removed"
        )
        .accessibilityHint("Scrolls to this file")
        .accessibilityAddTraits(.isButton)
    }

    /// The kit's chip height, shared with `CCBadge`'s control form rather than
    /// written twice — a file chip and a tappable badge are the same object.
    @ScaledMetric(relativeTo: .caption) private var chipHeight: CGFloat = CC.size.chip
}

private struct CCDiffChipStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .ccPressScale(configuration.isPressed, scale: 0.96)
    }
}

// MARK: - File header

/// The sticky 40pt band that names the file you are reading.
///
/// The path truncates from the **head**: the tail is what you need. A renamed
/// file puts its emphasis on the half that answers the question — the old path
/// is `textTertiary` because it is context, the new one is `text`.
struct CCDiffFileHeader: View {
    let file: UnifiedDiff.FileDiff

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
            CCIcon(file.status.symbol, size: 14, weight: .medium, relativeTo: .footnote)
                .foregroundStyle(CC.text.tertiary)
                .ccMark(beside: CC.type.monoSmall)

            path
                .ccType(CC.type.monoSmall)
                .lineLimit(1)
                // Keep: the tail of a path is what identifies it.
                .truncationMode(.head)

            Spacer(minLength: CC.space.xs)

            // **The counts leave the sticky band at accessibility sizes.**
            //
            // They are `fixedSize`, so at AX5 `+5 −2` claims ~150 of a 402pt row
            // and the path — the band's *entire* job, "which file am I in" — was
            // left six characters and rendered `….swift`. The same two numbers
            // are on the file chip directly above and in the stamp above that;
            // the path is stated nowhere else. VoiceOver keeps all of it: the
            // label below is built from `displayPath` and both counts.
            if !typeSize.isAccessibilitySize {
                HStack(spacing: CC.space.xs) {
                    Text("+\(file.additions)")
                        .foregroundStyle(CC.color.success)
                    Text("−\(file.deletions)")
                        .foregroundStyle(CC.color.danger)
                }
                .ccType(CC.type.monoSmall)
                .lineLimit(1)
                .fixedSize()
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.xs)
        // **40 is a floor, not a height, and it does not scale.**
        //
        // It rode `.footnote` as a scaled fixed height, which at AX5 reserved
        // ~135pt for one line of ~36pt `monoSmall` — the single largest item in
        // the ~570pt of chrome measured above the first line of code on an
        // 874pt screen. The band still grows for its content, because the
        // content sets its own scaled height and this is a `minHeight`; it just
        // no longer reserves three times what the content asks for.
        .frame(minHeight: CC.size.controlSm + CC.space.xxs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(CC.color.surface)
        .overlay(alignment: .bottom) { CCHairline() }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "\(file.displayPath), \(file.status.label), \(file.additions) added, \(file.deletions) removed"
        )
        .accessibilityAddTraits(.isHeader)
    }

    /// **Head truncation is right until there is nothing left of the head.**
    ///
    /// The tail identifies a path, so the sticky header cuts from the front —
    /// correct at reading sizes, and measured at AX5 as `….swift`: the one
    /// element whose entire job is *which file am I in* naming only the
    /// extension. At accessibility sizes it draws the **basename** instead, which
    /// is a shorter true string rather than a longer truncated one. VoiceOver
    /// keeps the full path either way; the label above is built from
    /// `displayPath`.
    private var path: Text {
        guard file.status == .renamed, let old = file.oldPath, let new = file.newPath else {
            return Text(compact(file.displayPath)).foregroundColor(CC.text.primary)
        }
        return Text(compact(old)).foregroundColor(CC.text.tertiary)
            // `textTertiary`: the arrow is what says this file *moved*, which is
            // the whole content of a rename row, so it is not inactive chrome
            // and does not take `textDisabled`.
            + Text(" → ").foregroundColor(CC.text.tertiary)
            + Text(compact(new)).foregroundColor(CC.text.primary)
    }

    private func compact(_ path: String) -> String {
        guard typeSize.isAccessibilitySize else { return path }
        return path.split(separator: "/").last.map(String.init) ?? path
    }
}

// MARK: - Fold row

/// `⋯ 6 unchanged lines`, one quiet row where the code starts. A tap
/// anywhere on it shows them. 44pt, not the ~26pt it replaced: this is a
/// control, and a control is 44pt.
struct CCFoldRow: View {
    let count: Int
    let metrics: CCDiffMetrics
    let expand: () -> Void

    var body: some View {
        Button(action: expand) {
            // The grid's own size, as the hunk header is: the diff does not
            // ride Dynamic Type, and a label that did stood over the code it
            // folds, broken into fragments at AX5.
            Text(label)
                .font(.system(size: metrics.fontSize - 1, design: .monospaced))
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.leading, codeInset)
                .padding(.trailing, CC.space.xs)
                .padding(.vertical, CC.space.xxs)
                .frame(minHeight: CC.size.hitTarget)
                .contentShape(Rectangle())
        }
        .buttonStyle(CCFoldRowStyle())
        .accessibilityLabel("\(count) unchanged line\(count == 1 ? "" : "s") hidden")
        .accessibilityHint("Shows them")
    }

    /// Where a row's code starts, so the label reads as a line of the file.
    private var codeInset: CGFloat {
        let gutter = metrics.gutter == .inline ? 0 : metrics.gutterWidth + CC.space.xxs
        return gutter + metrics.markerWidth + CC.space.xxs
    }

    private var label: String {
        count == 1 ? "⋯ 1 unchanged line" : "⋯ \(count) unchanged lines"
    }
}

private struct CCFoldRowStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            // Rows lighten one step, they do not scale.
            .background(
                configuration.isPressed ? CC.color.surfaceRaised : CC.color.surface, in: Rectangle())
            .ccAnimation(CC.motion.micro, value: configuration.isPressed)
    }
}

// MARK: - Wrapping

/// Wrapping done here rather than by the layout engine, so a continuation's
/// indent, or `CCMonoBlock`'s `↳`, can be put exactly where the break is. Only correct for a monospaced
/// font, which is the only font the grid uses.
enum DiffLineWrap {
    /// Tabs replaced by spaces to the next 4-column stop, VS Code's default.
    static func expandingTabs(_ text: String) -> String {
        guard text.contains("\t") else { return text }
        var out = ""
        for character in text {
            if character == "\t" {
                out += String(repeating: " ", count: 4 - out.count % 4)
            } else {
                out.append(character)
            }
        }
        return out
    }

    static func wrap(_ text: String, columns: Int, continuationIndent: Int = 1) -> [String] {
        guard columns > 1 else { return [text] }
        guard text.count > columns else { return [text.isEmpty ? " " : text] }
        var segments: [String] = []
        var remainder = Substring(text)
        // The first line gets the full width; continuations lose the columns
        // their `↳` or hanging indent takes.
        var width = columns
        while !remainder.isEmpty {
            let take = min(width, remainder.count)
            let index = remainder.index(remainder.startIndex, offsetBy: take)
            segments.append(String(remainder[..<index]))
            remainder = remainder[index...]
            width = max(1, columns - continuationIndent)
        }
        return segments
    }
}

extension String {
    /// Right-aligns a gutter number without a formatter.
    fileprivate func padded(_ width: Int) -> String {
        count >= width ? self : String(repeating: " ", count: width - count) + self
    }
}

// MARK: - Preview

#Preview("CCDiffPrimitives") {
    let lines: [UnifiedDiff.Line] = [
        .init(id: 0, kind: .context, text: "    let payload = encode(text)", oldNumber: 12, newNumber: 12),
        .init(id: 1, kind: .context, text: "    var attempt = 0", oldNumber: 13, newNumber: 13),
        .init(id: 2, kind: .deletion, text: "    try await transport.write(payload)", oldNumber: 14, newNumber: nil),
        .init(id: 3, kind: .addition, text: "    try await transport.writeAll(payload)", oldNumber: nil, newNumber: 14),
        .init(id: 4, kind: .context, text: "    return", oldNumber: 15, newNumber: 16),
    ]
    let map = CCDiffWordHighlight.map(for: lines)
    return GeometryReader { proxy in
        let metrics = CCDiffMetrics(fontSize: 12, availableWidth: proxy.size.width - 32)
        VStack(spacing: 0) {
            CCHunkHeader(header: "@@ -12,7 +12,9 @@ func send(_ text: String)", fontSize: 12)
            ForEach(lines) { line in
                CCDiffRow(line: line, metrics: metrics, wordRanges: map[line.id] ?? [])
            }
            CCFoldRow(count: 6, metrics: metrics) {}
        }
        .ccSurface(.surface, radius: CC.radius.md)
        .padding(CC.space.md)
    }
    .background(CC.color.bg)
    .ccAppearance()
}
