import AppKit
import CodeConnectKit
import SwiftUI

/// The agent's words as Kit splits them: prose held to a reading width, headings, lists, fenced code kept exactly, and
/// tables across the pane.
struct Prose: View {
    let text: String
    let isInterrupted: Bool

    /// The longest a line of prose runs (about 85 characters), so a wide pane still reads as a column. Tables and code
    /// take the pane.
    static let measure: CGFloat = 680
    static let size: CGFloat = 13.5
    static let leading: CGFloat = 5
    /// Between blocks: paragraphs, a heading and what follows it, a list, a table, a code block.
    static let gap: CGFloat = 14

    var body: some View {
        VStack(alignment: .leading, spacing: Self.gap) {
            ForEach(Array(AgentProse.segments(text).enumerated()), id: \.offset) { _, segment in
                switch segment {
                case .prose(let words):
                    // One text for the run, its paragraphs apart by a blank line: a drag selects across them.
                    Text(Self.inline(Self.trimmingBlankEdges(words), size: Self.size))
                        .font(Theme.ui(Self.size))
                        .foregroundStyle(Theme.text)
                        .lineSpacing(Self.leading)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: Self.measure, alignment: .leading)
                case .code(let code):
                    CodeBlock(text: code)
                case .heading(let words):
                    Text(Self.inline(words, size: 15))
                        .font(Theme.ui(15, .semibold))
                        .foregroundStyle(Theme.text)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: Self.measure, alignment: .leading)
                        .padding(.top, 4)
                case .table(let table):
                    TableBlock(table: table)
                case .list(let list):
                    ListBlock(list: list).frame(maxWidth: Self.measure, alignment: .leading)
                }
            }
            if isInterrupted {
                Text(SessionWords.interrupted).font(Theme.ui(12)).foregroundStyle(Theme.text3)
            }
        }
        .textSelection(.enabled)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// A prose run without the blank lines at its two ends: the gap between blocks is the stack's, never a blank line
    /// drawn on top of it. Kit never emits a run that is only whitespace.
    static func trimmingBlankEdges(_ words: String) -> String {
        var lines = words.split(separator: "\n", omittingEmptySubsequences: false)
        while let first = lines.first, first.trimmingCharacters(in: .whitespaces).isEmpty { lines.removeFirst() }
        while let last = lines.last, last.trimmingCharacters(in: .whitespaces).isEmpty { lines.removeLast() }
        return lines.joined(separator: "\n")
    }

    /// Kit's inline Markdown as the Mac sets it beside text of `size`: `code` in the code face a step smaller on a
    /// faint ground, so it sits in the line; a link underlined in the accent colour.
    static func inline(_ words: String, size: CGFloat) -> AttributedString {
        var string = AgentProse.inline(words)
        for run in string.runs {
            if run[AttributeScopes.FoundationAttributes.InlinePresentationIntentAttribute.self]?.contains(.code) == true {
                string[run.range][AttributeScopes.SwiftUIAttributes.FontAttribute.self] = Theme.mono(size - 1.5)
                string[run.range][AttributeScopes.SwiftUIAttributes.BackgroundColorAttribute.self] = Theme.codeGround
            }
            if run[AttributeScopes.FoundationAttributes.LinkAttribute.self] != nil {
                string[run.range][AttributeScopes.SwiftUIAttributes.ForegroundColorAttribute.self] = CC.color.accent
                string[run.range][AttributeScopes.SwiftUIAttributes.UnderlineStyleAttribute.self] = .single
            }
        }
        return string
    }
}

/// Text that must keep its lines: code, a command's output, a tool's input. Drawn `displaySafe`, as the prose around
/// it is: the characters that reorder a line would otherwise draw different code from what the bytes say. Its Copy
/// takes the text as it came, not as drawn.
struct CodeBlock: View {
    let text: String

    var body: some View {
        Text(text.displaySafe)
            .font(Theme.mono(12))
            .foregroundStyle(Theme.text)
            .lineSpacing(2)
            .textSelection(.enabled)
            .fixedSize(horizontal: false, vertical: true)
            .padding([.vertical, .leading], 12)
            .padding(.trailing, CopyButton.slot)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.code, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
            .overlay(alignment: .trailing) { CopyButton(text: text, id: "conversation.code.copy") }
    }
}

/// A list: one line per item, its words hanging clear of Kit's marker, each level indented one step.
private struct ListBlock: View {
    let list: AgentList

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            ForEach(Array(list.items.enumerated()), id: \.offset) { _, item in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Group {
                        if let glyph = item.glyph {
                            Text(glyph).font(Theme.ui(Prose.size))
                        } else if case .task(let done) = item.marker {
                            Image(systemName: done ? "checkmark.square" : "square")
                                .font(.system(size: Prose.size - 1))
                                .accessibilityLabel(done ? "Done" : "Not done")
                        }
                    }
                    .foregroundStyle(Theme.text3)
                    .frame(minWidth: 14, alignment: .leading)
                    Text(Prose.inline(item.text, size: Prose.size))
                        .font(Theme.ui(Prose.size))
                        .foregroundStyle(Theme.text)
                        .lineSpacing(Prose.leading)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.leading, CGFloat(item.depth) * 20)
            }
        }
    }
}

/// A table across the pane: a bold header, a rule under it and a quiet one under every row, each cell at its row's
/// first line. A column whose cells are all short is set at its own width and the long ones share the rest; a table
/// that cannot fit the pane that way pans sideways inside itself, each long cell capped (Kit's `WidthCap`), with its
/// scroll bar shown. The wheel's vertical turns over it scroll the conversation (`PanningTable`).
private struct TableBlock: View {
    let table: MarkdownTable

    static let size: CGFloat = 12.5
    /// A column whose longest cell is this many characters or fewer is set at its own width.
    static let short = 24
    /// The least a long column needs before the table pans instead: about 25 characters a line.
    static let longMinimum: CGFloat = 160
    /// The widest a long cell grows in a table that pans.
    static let cellCap: CGFloat = 360
    static let padding: CGFloat = 12

    var body: some View {
        // The first that fits: short columns at their own widths, the long ones sharing the pane; else the table at
        // its own width, panning.
        ViewThatFits(in: .horizontal) {
            grid(sized: true)
                .frame(minWidth: needed, idealWidth: needed, maxWidth: .infinity, alignment: .leading)
            PanningTable(content: grid(sized: false).fixedSize())
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(AgentProse.tableSummary(table))
    }

    private func column(_ index: Int) -> [String] {
        [table.headers[index]] + table.rows.map { $0[index] }
    }

    private func plain(_ cell: String) -> String { String(AgentProse.inline(cell).characters) }

    private var shortColumns: Set<Int> {
        Set(table.headers.indices.filter { column($0).allSatisfy { plain($0).count <= Self.short } })
    }

    /// The width the table needs to fit without panning: short columns at their widest cell, long ones at their
    /// minimum, and the padding between.
    private var needed: CGFloat {
        let short = shortColumns
        return table.headers.indices.reduce(0) { sum, index in
            let width = short.contains(index) ? column(index).map(Self.width).max() ?? 0 : Self.longMinimum
            return sum + width + (index == 0 ? 0 : Self.padding) + Self.padding
        }
    }

    /// A cell's width on one line, each run in the face it draws in (`code` in the code face), at the header's weight
    /// so the estimate errs wide.
    private static func width(_ cell: String) -> CGFloat {
        let text = Prose.inline(cell, size: size)
        let words = Theme.uiFont(size, .semibold)
        let code = Theme.monoFont(size - 1.5)
        return ceil(
            text.runs.reduce(0) { sum, run in
                let isCode = run[AttributeScopes.FoundationAttributes.InlinePresentationIntentAttribute.self]?
                    .contains(.code) == true
                let piece = String(text[run.range].characters) as NSString
                return sum + piece.size(withAttributes: [.font: isCode ? code : words]).width
            })
    }

    private func grid(sized: Bool) -> some View {
        let short = shortColumns
        return Grid(alignment: .topLeading, horizontalSpacing: 0, verticalSpacing: 0) {
            GridRow {
                ForEach(table.headers.indices, id: \.self) { index in
                    cell(table.headers[index], column: index, header: true, short: short, sized: sized)
                        .gridColumnAlignment(Self.alignment(table.alignments[index]))
                }
            }
            rule(strong: true)
            ForEach(table.rows.indices, id: \.self) { row in
                if row > 0 { rule(strong: false) }
                GridRow {
                    ForEach(table.rows[row].indices, id: \.self) { index in
                        cell(table.rows[row][index], column: index, header: false, short: short, sized: sized)
                    }
                }
            }
        }
    }

    private func rule(strong: Bool) -> some View {
        Rectangle().fill(Color.white.opacity(strong ? 0.14 : 0.07)).frame(height: 1).gridCellUnsizedAxes(.horizontal)
    }

    @ViewBuilder private func cell(
        _ words: String, column: Int, header: Bool, short: Set<Int>, sized: Bool
    ) -> some View {
        let text = Text(Prose.inline(words, size: Self.size))
            .font(Theme.ui(Self.size, header ? .semibold : .regular))
            .foregroundStyle(Theme.text)
            .lineSpacing(3.5)
            .multilineTextAlignment(Self.textAlignment(table.alignments[column]))
        Group {
            if short.contains(column) {
                text.fixedSize(horizontal: true, vertical: false)
            } else if sized {
                text.fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: Alignment(horizontal: Self.alignment(table.alignments[column]), vertical: .top))
            } else {
                WidthCap(cap: Self.cellCap) { text.fixedSize(horizontal: false, vertical: true) }
            }
        }
        .padding(.vertical, 7)
        .padding(.leading, column == 0 ? 0 : Self.padding)
        .padding(.trailing, Self.padding)
    }

    static func alignment(_ alignment: MarkdownTable.Alignment) -> HorizontalAlignment {
        switch alignment {
        case .leading: .leading
        case .center: .center
        case .trailing: .trailing
        }
    }

    static func textAlignment(_ alignment: MarkdownTable.Alignment) -> TextAlignment {
        switch alignment {
        case .leading: .leading
        case .center: .center
        case .trailing: .trailing
        }
    }
}

/// A table too wide for the pane, panning inside an AppKit scroll view with its horizontal scroll bar always shown.
/// Its height is the table's own (the hosting view's fitting height at the table's width) plus the bar's, so the
/// conversation measures the row from the grid, never from the scroll view's frame.
private struct PanningTable<Content: View>: NSViewRepresentable {
    let content: Content

    func makeNSView(context: Context) -> WheelPassingScrollView {
        let scroll = WheelPassingScrollView()
        scroll.hasHorizontalScroller = true
        scroll.hasVerticalScroller = false
        scroll.scrollerStyle = .legacy
        scroll.autohidesScrollers = false
        scroll.verticalScrollElasticity = .none
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.documentView = NSHostingView(rootView: ThemeRoot(content: content))
        return scroll
    }

    func updateNSView(_ scroll: WheelPassingScrollView, context: Context) {
        guard let host = scroll.documentView as? NSHostingView<ThemeRoot<Content>> else { return }
        host.rootView = ThemeRoot(content: content)
        host.setFrameSize(host.fittingSize)
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView scroll: WheelPassingScrollView, context: Context) -> CGSize? {
        guard let host = scroll.documentView as? NSHostingView<ThemeRoot<Content>> else { return nil }
        let table = host.fittingSize
        let bar = NSScroller.scrollerWidth(for: .regular, scrollerStyle: .legacy)
        return CGSize(width: min(proposal.width ?? table.width, table.width), height: table.height + bar)
    }
}

/// A scroll view that pans sideways and hands a wheel's mostly-vertical turns to the views around it, so the
/// conversation under the pointer keeps scrolling. A trackpad gesture's direction is decided once, on its first event
/// that moves, and the gesture and its momentum all go that way, so neither scroll view sees half a gesture; a wheel
/// click, which has no phase, is judged on its own.
final class WheelPassingScrollView: NSScrollView {
    /// Whether the gesture under way goes to the conversation; nil until its first event that moves.
    private var passesGesture: Bool?

    override func scrollWheel(with event: NSEvent) {
        let dx = abs(event.scrollingDeltaX)
        let dy = abs(event.scrollingDeltaY)
        let phased = event.phase != [] || event.momentumPhase != []
        if event.phase == .mayBegin || event.phase == .began { passesGesture = nil }
        if phased, passesGesture == nil, dx > 0 || dy > 0 {
            passesGesture = dy > dx
        }
        let passes = phased ? passesGesture ?? (dy > dx) : dy > dx
        if passes {
            nextResponder?.scrollWheel(with: event)
        } else {
            super.scrollWheel(with: event)
        }
    }
}
