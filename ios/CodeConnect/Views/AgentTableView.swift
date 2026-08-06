import SwiftUI

/// A recognized pipe table, rendered as the two-dimensional artifact it is.
///
/// The columns are real: a native `Grid` measures every cell and sizes each
/// column to its widest, which is the entire reason this is not a space-padded
/// monospace string — character counting has no idea how wide a CJK glyph, an
/// emoji, or a combining mark actually draws, and SwiftUI does. Cells render
/// through the same inline parser as prose, so `**bold**`, links, and `\|`
/// behave identically inside and outside a table.
///
/// **This is the documented exception to `CCMonoBlock`'s no-horizontal-scroll
/// rule.** A command wraps because its characters survive wrapping; a table's
/// meaning is its geometry, and reflowing columns is how rows stop lining up.
/// So a wide table stays wide and pans, indicators visible, no edge fade —
/// with one bound: a single cell caps at 240pt and wraps vertically, so one
/// pasted URL cannot drag the grid a few thousand points wide. Nothing is
/// truncated, tightened, or ellipsised.
struct AgentTableView: View {
    let table: MarkdownTable

    @Environment(\.layoutDirection) private var layoutDirection
    @Environment(\.ccColumnInset) private var columnInset

    /// One paragraph-of-text width. Wider, and a lone prose cell makes every
    /// other column unreachable without a scroll expedition; narrower, and
    /// ordinary cells wrap that never needed to.
    private static let cellCap: CGFloat = 240

    var body: some View {
        ScrollView(.horizontal) {
            grid
                .padding(CC.space.sm)
        }
        .scrollBounceBehavior(.basedOnSize, axes: [.horizontal])
        .ccSurface(.raised, radius: CC.radius.md)
        // The kit's one hang rule, not private arithmetic: the container
        // declared its column, and the border steps back from it so the
        // first cell's text stands where the prose does.
        .padding(.leading, CCColumn.hang(from: columnInset))
        // VoiceOver reads structure, not the grid's cells in visual order
        // stripped of context: the container states the shape, then one stop
        // for the columns and one per row, every cell paired with its header.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(AgentProse.tableSummary(table))
        .accessibilityChildren {
            Text(AgentProse.tableColumnsLabel(table))
            ForEach(table.rows.indices, id: \.self) { row in
                Text(AgentProse.tableRowLabel(table, row: row))
            }
        }
    }

    private var grid: some View {
        Grid(
            alignment: .topLeading,
            horizontalSpacing: CC.space.sm,
            verticalSpacing: CC.space.xs
        ) {
            GridRow {
                ForEach(table.headers.indices, id: \.self) { column in
                    cell(table.headers[column], column: column, header: true)
                        .gridColumnAlignment(gridAlignment(for: column))
                }
            }
            hairline
            ForEach(table.rows.indices, id: \.self) { row in
                if row > 0 { hairline }
                GridRow {
                    ForEach(table.rows[row].indices, id: \.self) { column in
                        cell(table.rows[row][column], column: column, header: false)
                    }
                }
            }
        }
        .textSelection(.enabled)
    }

    /// Spans every column; unsized horizontally so the rule follows the
    /// grid's width instead of stretching the grid to the viewport's.
    private var hairline: some View {
        CCHairline()
            .gridCellUnsizedAxes(.horizontal)
    }

    private func cell(_ text: String, column: Int, header: Bool) -> some View {
        WidthCap(cap: Self.cellCap) {
            Text(AgentProse.inline(text))
                .ccType(header ? CC.type.callout.weight(.semibold) : CC.type.callout)
                .foregroundStyle(CC.text.primary)
                .multilineTextAlignment(textAlignment(for: column))
        }
    }

    /// The cap as a real layout, because `frame(maxWidth:)` cannot do this
    /// job: it passes the grid's unspecified measurement proposal through
    /// untouched, reports the capped width anyway, and the row then clips
    /// every line the measurement never counted — a 500-character URL cell
    /// measured 20pt tall and drew ~300 (caught by the layout tests). This
    /// clamps the *proposal*, so a capped cell's reported height is its
    /// honest wrapped height and nothing truncates.
    private struct WidthCap: Layout {
        let cap: CGFloat

        func sizeThatFits(
            proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
        ) -> CGSize {
            guard let cell = subviews.first else { return .zero }
            switch proposal.width {
            case .some(let width) where width > 0:
                // Height stays unconstrained on purpose: forwarding the
                // grid's height probe let a (240 × one-line) proposal report
                // one line, and the URL cell rendered `…` — caught on a
                // rendered fixture, forbidden by the no-truncation rule.
                return cell.sizeThatFits(
                    ProposedViewSize(width: min(width, cap), height: nil))
            case .some:
                // The zero-width minimum probe is not ours to shape.
                return cell.sizeThatFits(proposal)
            case .none:
                let ideal = cell.sizeThatFits(.unspecified)
                if ideal.width <= cap { return ideal }
                return cell.sizeThatFits(ProposedViewSize(width: cap, height: nil))
            }
        }

        func placeSubviews(
            in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews,
            cache: inout ()
        ) {
            // Height-free again: the bounds came from the honest measurement
            // above, and a concrete height here is an invitation to ellipsise
            // if the two ever disagree by a rounding error.
            subviews.first?.place(
                at: bounds.origin, anchor: .topLeading,
                proposal: ProposedViewSize(width: bounds.width, height: nil))
        }
    }

    // The delimiter row's colons, honored twice over: `gridColumnAlignment`
    // places each cell in its column, `multilineTextAlignment` places lines
    // within a wrapped cell. A cell is always exactly its content's size —
    // the cap layout leaves no slack inside it to align.

    private func gridAlignment(for column: Int) -> HorizontalAlignment {
        Self.gridAlignment(
            for: table.alignments[column],
            rightToLeft: layoutDirection == .rightToLeft)
    }

    private func textAlignment(for column: Int) -> TextAlignment {
        Self.textAlignment(
            for: table.alignments[column],
            rightToLeft: layoutDirection == .rightToLeft)
    }

    // GFM's colons are physical: `:---` pins the LEFT edge on GitHub in any
    // UI language. SwiftUI's leading/trailing flip under right-to-left, so
    // the mapping compensates and a colon keeps its physical promise.

    static func gridAlignment(
        for alignment: MarkdownTable.Alignment, rightToLeft: Bool
    ) -> HorizontalAlignment {
        switch alignment {
        case .center: return .center
        case .leading: return rightToLeft ? .trailing : .leading
        case .trailing: return rightToLeft ? .leading : .trailing
        }
    }

    static func textAlignment(
        for alignment: MarkdownTable.Alignment, rightToLeft: Bool
    ) -> TextAlignment {
        switch alignment {
        case .center: return .center
        case .leading: return rightToLeft ? .trailing : .leading
        case .trailing: return rightToLeft ? .leading : .trailing
        }
    }
}

#Preview("Tables") {
    let fixtures: [(String, String)] = [
        (
            "Typical",
            """
            | Command | Description | Status |
            |---|---|:---:|
            | `git status` | Lists changed files | **available** |
            | `git push` | Publishes commits | blocked |
            """
        ),
        (
            "Wide + long cell",
            """
            | Check | Where | Result | Notes | Link |
            |---|---|---:|---|---|
            | build | CI | 412 | A deliberately long prose cell that should wrap at the cap instead of dragging the whole grid sideways forever | https://example.com/some/very/long/path/to/a/build/artifact |
            | tests | CI | 9 | ok | — |
            """
        ),
        (
            "CJK, emoji, empty",
            """
            | 項目 | 状態 |
            |---|---|
            | ビルド | ✅ 成功 |
            | テスト |  |
            """
        ),
        (
            "Header only",
            """
            | Field | Value |
            |---|---|
            """
        ),
    ]
    return ScrollView {
        VStack(alignment: .leading, spacing: CC.space.lg) {
            ForEach(fixtures, id: \.0) { fixture in
                Text(fixture.0).ccType(CC.type.headline)
                ForEach(
                    Array(AgentProse.segments(fixture.1).enumerated()), id: \.offset
                ) { _, segment in
                    if case .table(let table) = segment {
                        AgentTableView(table: table)
                    }
                }
            }
        }
        .padding(CC.space.xl)
    }
    .background(CC.color.bg)
}
