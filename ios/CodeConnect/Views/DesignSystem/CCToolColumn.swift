import SwiftUI

/// One shared left edge for the command beside a tool name.
///
/// **The problem this solves.** A tool row is `Bash  git push …` — a prose tool
/// name at full contrast, then the command in monospace. Set inline, the command
/// begins wherever the name happens to end, so a *column* of tool rows has a
/// ragged left edge. Measured on the fleet: `Edit`, `Bash` and `Read` are 23.67,
/// 30.00 and 30.67 wide, so their commands began at 83.67, 90.00 and 90.67.
///
/// It is not cosmetic on these screens. The fleet's own band header carries the
/// note that the screen "gets exactly two" left edges, and the timeline already
/// pins its glyph to a fixed gutter *"because a column of tool rows has to line
/// up whatever symbols it happens to hold"* — the same argument, applied to the
/// icon and not yet to the name beside it.
///
/// **Why a measured column and not a constant.** Tool names run from `Edit` to
/// `MultiEdit` to whatever an MCP server calls itself. A fixed width either
/// truncates the long ones or reserves dead space for the short ones. Taking the
/// widest label actually on screen costs nothing when they are all the same
/// width and fixes the edge when they are not.
///
/// **Why the column is the widest label that fits in half a row.** An MCP tool
/// is named `mcp__<server>__<tool>`, wider than a phone row. Set by the widest
/// label on screen, one such name took every row's width: on the `wideTool`
/// fleet the `Bash`, `Edit` and `Read` rows drew no command at all, and the MCP
/// row's card measured 393pt wide from x=16 on a 402pt screen. Capped at half a
/// row, it still pushed the short rows' commands 131pt past their labels and
/// cut `git push --force origin main` to `…--force origin main`. So a label
/// wider than half its row is an outlier (`CCToolPairLayout`): it takes half of
/// its own row, truncated in the middle so the name keeps how it starts and how
/// it ends, and it sets the column only of rows it fits in half of. Rows offered
/// the same width still share one edge.
///
/// **Why it settles.** Each label is measured from a hidden copy at its
/// intrinsic size, never from the slot it is drawn into, so the widths cannot
/// chase themselves. Widths are only ever added, so the column can move only
/// when a row with a wider label is first drawn — in a chat's `List`, when it
/// scrolls into view — and never back.
enum CCToolColumn {
    /// The one gap between a tool label and the argument beside it, on every
    /// row that draws the pair — a timeline tool row, an approval preview, a
    /// fleet row's activity line. The tool row used its glyph spacing, 12, and
    /// the other two 8, so one screen's commands started 4pt apart after the
    /// same measured column.
    static let argumentGap: CGFloat = CC.space.xs
}

/// The horizontal form of a tool label and the argument beside it: the label
/// first, the argument, if any, second.
///
/// Half of what the pair is offered, after the gap, is the most a label may
/// take. A label that fits is drawn into the column: the widest label on the
/// screen that also fits, so every such row's argument starts on one edge. A
/// label that does not fit takes the half and no more. The argument gets the
/// rest. A label with no argument takes the row. Offered a width, the pair
/// takes all of it, so offered it again it splits it the same way.
struct CCToolPairLayout: Layout {
    /// Every tool label's width on this screen. Empty until measured.
    let labels: Set<CGFloat>
    var alignment: VerticalAlignment = .center

    /// The label's slot, the gap after it (`CCToolColumn.argumentGap`) and the
    /// width offered to the argument, for a pair offered `width` whose own
    /// label is `label` wide. Offered less than the gap, the gap is what there
    /// is, so the argument never starts outside the pair. Offered no width, or
    /// an infinite one, nothing is an outlier and the argument is offered the
    /// same as the pair.
    static func split(
        width: CGFloat?, labels: Set<CGFloat>, label: CGFloat
    ) -> (label: CGFloat, gap: CGFloat, argument: CGFloat?) {
        let spacing = CCToolColumn.argumentGap
        let gap = width.map { min(spacing, max(0, $0)) } ?? spacing
        let half = width.map { max(0, ($0 - gap) / 2) } ?? .infinity
        let column = labels.filter { $0 <= half }.max() ?? 0
        let slot = min(max(label, column), half)
        guard let width, width.isFinite else { return (slot, gap, width) }
        return (slot, gap, max(0, width - slot - gap))
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let placed = measure(proposal.width, subviews)
        let width: CGFloat
        if let offered = proposal.width, offered.isFinite {
            width = offered
        } else {
            width = placed.map { $0.x + $0.dimensions.width }.max() ?? 0
        }
        return CGSize(width: width, height: height(placed))
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        let placed = measure(proposal.width, subviews)
        let baseline = placed.map { $0.dimensions[alignment] }.max() ?? 0
        for (subview, slot) in zip(subviews, placed) {
            subview.place(
                at: CGPoint(
                    x: bounds.minX + slot.x,
                    y: bounds.minY + baseline - slot.dimensions[alignment]),
                proposal: slot.proposal)
        }
    }

    private struct Slot {
        let x: CGFloat
        let proposal: ProposedViewSize
        let dimensions: ViewDimensions
    }

    private func measure(_ width: CGFloat?, _ subviews: Subviews) -> [Slot] {
        guard let label = subviews.first else { return [] }
        let ideal = label.sizeThatFits(.unspecified).width
        // With no argument there is nothing to leave room for, nor to line up:
        // the label takes the whole row. Held to half, a call that names no
        // command (`AskUserQuestion`, an MCP lookup) was cut beside empty space.
        guard subviews.count > 1 else {
            let proposal = ProposedViewSize(width: width.map { min(ideal, $0) } ?? ideal, height: nil)
            return [Slot(x: 0, proposal: proposal, dimensions: label.dimensions(in: proposal))]
        }
        let split = Self.split(width: width, labels: labels, label: ideal)
        let labelProposal = ProposedViewSize(width: split.label, height: nil)
        let argumentProposal = ProposedViewSize(width: split.argument, height: nil)
        return [
            Slot(x: 0, proposal: labelProposal, dimensions: label.dimensions(in: labelProposal)),
            Slot(
                x: split.label + split.gap, proposal: argumentProposal,
                dimensions: subviews[1].dimensions(in: argumentProposal)),
        ]
    }

    /// Tall enough for every subview hung on the shared alignment guide.
    private func height(_ placed: [Slot]) -> CGFloat {
        let above = placed.map { $0.dimensions[alignment] }.max() ?? 0
        let below = placed.map { $0.dimensions.height - $0.dimensions[alignment] }.max() ?? 0
        return above + below
    }
}

struct CCToolLabelWidthKey: PreferenceKey {
    static let defaultValue: Set<CGFloat> = []
    static func reduce(value: inout Set<CGFloat>, nextValue: () -> Set<CGFloat>) {
        value.formUnion(nextValue().filter(\.isFinite))
    }
}

private struct CCToolLabelWidthsKey: EnvironmentKey {
    static let defaultValue: Set<CGFloat> = []
}

extension EnvironmentValues {
    /// Every tool label's width on this screen, from which each row takes its
    /// column. Empty until the first pass has measured, which reads as "no
    /// column yet": each label takes its own width.
    var ccToolLabelWidths: Set<CGFloat> {
        get { self[CCToolLabelWidthsKey.self] }
        set { self[CCToolLabelWidthsKey.self] = newValue }
    }
}

extension View {
    /// Draw this tool label on one line, truncating in the middle, and report
    /// its intrinsic width as a candidate for the tool column. The slot it is
    /// drawn into is `CCToolPairLayout`'s.
    ///
    /// The width is read off a hidden copy held at its intrinsic size, so it is
    /// the whole name however much of it the slot shows. The copy sits in the
    /// background, where it takes no width from the row, and `hidden()` keeps
    /// it from being drawn and from VoiceOver, which reads the label itself.
    ///
    /// `disabled` is how a caller opts out at accessibility sizes, where these
    /// rows stack and a width would only be dead space. Opted out, the label is
    /// left alone entirely.
    @ViewBuilder
    func ccToolLabelColumn(disabled: Bool = false) -> some View {
        if disabled {
            self
        } else {
            lineLimit(1)
                .truncationMode(.middle)
                .background {
                    fixedSize()
                        .hidden()
                        .background {
                            GeometryReader { proxy in
                                Color.clear.preference(
                                    key: CCToolLabelWidthKey.self, value: [proxy.size.width])
                            }
                        }
                }
        }
    }

    /// Collect the width of every tool label beneath this view and publish
    /// them back down. Widths are only ever added, so a row scrolled out of a
    /// `List` does not take its label's width with it.
    ///
    /// Applied once per screen. Two screens each get their own column, which is
    /// correct: they are different lists and have no reason to agree.
    func ccCollectsToolColumn(into widths: Binding<Set<CGFloat>>) -> some View {
        onPreferenceChange(CCToolLabelWidthKey.self) { measured in
            if !measured.isSubset(of: widths.wrappedValue) {
                widths.wrappedValue.formUnion(measured)
            }
        }
        .environment(\.ccToolLabelWidths, widths.wrappedValue)
    }
}
