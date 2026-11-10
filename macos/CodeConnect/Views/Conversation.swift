import CodeConnectKit
import SwiftUI

/// A run's conversation as a reading column, oldest at the top, opened at the newest: your words, the agent's prose
/// and code, each tool call folded to one line with its output under it, a decision where it was made, and the run's
/// notices; a request still waiting on you keeps its place as one line, and is answered at the pane's foot. Lazy, so
/// a run thousands of rows long builds only what is near the screen. It opens at the newest row and moves for one
/// reason only: new rows are followed while the newest is on screen. The row at the top of what is on screen stays
/// where it is as rows or a page of earlier history land around it. Reaching the top pages back on its own, as the
/// iPhone does.
struct Conversation: View {
    let model: AppModel
    let key: String
    /// The row at the top of what is on screen, kept in place when a page of earlier history lands above it.
    @State private var topRow: String?

    var body: some View {
        let items = model.shown(for: key)?.timeline ?? []
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    Earlier(model: model, key: key)
                    LazyVStack(alignment: .leading, spacing: 14) {
                        ForEach(items) { item in
                            ConversationRow(item: item, model: model)
                        }
                    }
                    .scrollTargetLayout()
                }
                .padding(.horizontal, 28)
            }
            .scrollPosition(id: $topRow, anchor: .top)
            // The column's top and bottom margin, kept by the scroll view, so a row scrolled to the bottom shows the end.
            .contentMargins(.vertical, 18, for: .scrollContent)
            // At the bottom from the first frame, so nothing above the newest row is passed over on the way.
            .defaultScrollAnchor(.bottom, for: .initialOffset)
            .defaultScrollAnchor(.bottom, for: .alignment)
            // A reader at the end is kept there as the column grows or the pane's foot takes more of it. Each time is
            // one scroll, which, unlike a bottom anchor, does not come back as rows change, so a reader who went up
            // stays where they are.
            .onScrollGeometryChange(for: ScrollGeometry.self) { $0 } action: { old, new in
                let endMoved = new.contentSize.height > old.contentSize.height
                    || new.containerSize.height < old.containerSize.height
                if Self.atEnd(old), endMoved, let last = items.last {
                    proxy.scrollTo(last.id, anchor: .bottom)
                }
            }
            .accessibilityIdentifier("session.conversation")
        }
        #if DEBUG
            .modifier(ShownOut(model: model, key: key))
        #endif
    }

    /// The end of the column is on screen. From the scroll view's own geometry, whose top inset is the column's margin or
    /// the space above a column shorter than the pane: a lazy row that scrolled far away is gone, and never says so.
    private static func atEnd(_ geometry: ScrollGeometry) -> Bool {
        geometry.contentOffset.y + geometry.contentInsets.top + geometry.containerSize.height
            >= geometry.contentSize.height - 2
    }
}

/// The row above the first: once it is really on screen and Kit can page, it asks for the page before; while that
/// page is on its way it says so. Kit decides whether there is anything earlier and guards a page already in flight.
private struct Earlier: View {
    let model: AppModel
    let key: String
    @State private var onScreen = false

    var body: some View {
        Group {
            if model.isLoadingEarlier(key) {
                Text("Loading earlier history")
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.text3)
                    .frame(maxWidth: .infinity)
            } else {
                Color.clear.frame(height: 1)
            }
        }
        .accessibilityIdentifier("session.earlier")
        .onScrollVisibilityChange { onScreen = $0 }
        .task(id: [onScreen, model.isReadyToPage(key)]) {
            if onScreen, model.isReadyToPage(key) { model.reachedTop(key: key) }
        }
    }
}

private struct ConversationRow: View {
    let item: TimelineItem
    let model: AppModel

    var body: some View {
        if case .approval(let approval) = item.content, approval.isPending {
            // One element, named by its words.
            WaitingLine().accessibilityIdentifier("session.row.\(item.id)")
        } else {
            content
                // One element per row: an identifier on a row of several texts otherwise lands on each.
                .accessibilityElement(children: .contain)
                .accessibilityIdentifier("session.row.\(item.id)")
        }
    }

    @ViewBuilder private var content: some View {
        switch item.content {
        case .userMessage(let text, let isCommand):
            Prompt(text: text, isCommand: isCommand)
        case .agentMessage(let text, let isInterrupted):
            Prose(text: text, isInterrupted: isInterrupted)
        case .tool(let tool):
            ToolLine(tool: tool, id: item.id)
        case .approval(let approval):
            RequestBlock(approval: approval, model: model)
        case .notice(let notice):
            NoticeLine(notice: notice)
        }
    }
}

/// Where a request still waiting on you was asked: one line in its place, pointing at the pane's foot, where the
/// request itself is read and answered.
private struct WaitingLine: View {
    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.down.circle").font(.system(size: 11)).foregroundStyle(Theme.amber)
                .accessibilityHidden(true)
            Text("Waiting on you below").font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Waiting on you below")
    }
}

/// What you said: one raised block, a slash command in the code face.
private struct Prompt: View {
    let text: String
    let isCommand: Bool

    var body: some View {
        Text(text)
            .font(isCommand ? Theme.mono(12.5) : Theme.ui(13))
            .foregroundStyle(Theme.text)
            .lineSpacing(3)
            .textSelection(.enabled)
            .padding(.horizontal, 12)
            .padding(.vertical, 9)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }
}

/// The agent's words as Kit splits them: prose with its inline marks, fenced code kept exactly, headings, tables.
private struct Prose: View {
    let text: String
    let isInterrupted: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(AgentProse.segments(text).enumerated()), id: \.offset) { _, segment in
                switch segment {
                case .prose(let words):
                    Text(Self.inline(words))
                        .font(Theme.ui(13))
                        .foregroundStyle(Theme.text)
                        .lineSpacing(3.5)
                        .fixedSize(horizontal: false, vertical: true)
                case .code(let code):
                    CodeBlock(text: code)
                case .heading(let words):
                    Text(Self.inline(words)).font(Theme.ui(13.5, .semibold)).foregroundStyle(Theme.text)
                case .table(let table):
                    TableBlock(table: table)
                }
            }
            if isInterrupted {
                Text("Stopped before it finished").font(Theme.ui(12)).foregroundStyle(Theme.text3)
            }
        }
        .textSelection(.enabled)
    }

    /// Kit's inline Markdown, with `code` in the code face on a raised ground.
    static func inline(_ words: String) -> AttributedString {
        var string = AgentProse.inline(words)
        for run in string.runs where run.inlinePresentationIntent?.contains(.code) == true {
            string[run.range].font = Theme.mono(12)
            string[run.range].backgroundColor = Theme.raised
        }
        return string
    }
}

/// Text that must keep its lines: code, a command's output.
private struct CodeBlock: View {
    let text: String

    var body: some View {
        Text(text)
            .font(Theme.mono(12))
            .foregroundStyle(Theme.text)
            .lineSpacing(2)
            .textSelection(.enabled)
            .fixedSize(horizontal: false, vertical: true)
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.code, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }
}

private struct TableBlock: View {
    let table: MarkdownTable

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 6) {
            GridRow {
                ForEach(Array(table.headers.enumerated()), id: \.offset) { _, header in
                    Text(Prose.inline(header)).font(Theme.ui(12, .medium)).foregroundStyle(Theme.text2)
                }
            }
            ForEach(Array(table.rows.enumerated()), id: \.offset) { _, row in
                GridRow {
                    ForEach(Array(row.enumerated()), id: \.offset) { _, cell in
                        Text(Prose.inline(cell)).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                    }
                }
            }
        }
        .padding(12)
        .background(Theme.code, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }
}

/// A tool call folded to one line: how it ended, its name, what it was given and how long it took. One with output
/// opens to show it.
private struct ToolLine: View {
    let tool: ToolItem
    let id: String
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let output = tool.output, !output.isEmpty {
                Button { open.toggle() } label: { line(opens: true) }
                    .buttonStyle(.hover(outset: 4, id: "session.tool.\(id)"))
                    .accessibilityLabel(spoken)
                    .accessibilityIdentifier("session.tool.\(id)")
                    .accessibilityValue(open ? "open" : "closed")
                if open {
                    CodeBlock(text: output)
                    if tool.isTruncated {
                        Text("The rest of the output was cut").font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                    }
                }
            } else {
                // One element either way, found by the same id; its name starts with how the call stands.
                line(opens: false)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(spoken)
                    .accessibilityIdentifier("session.tool.\(id)")
            }
        }
    }

    /// How the call reads aloud: how it stands, the tool, what it was given and how long it took.
    private var spoken: String {
        [tool.status.label, tool.name, tool.argument ?? "", tool.durationMS.map { Format.duration(ms: $0) } ?? ""]
            .filter { !$0.isEmpty }.joined(separator: ", ")
    }

    private func line(opens: Bool) -> some View {
        HStack(spacing: 7) {
            Image(systemName: Self.symbol(tool.status))
                .font(.system(size: 11))
                .foregroundStyle(tool.status == .failed ? Theme.red : Theme.text3)
                .frame(width: 14)
                // Hidden: a checkmark symbol in a button's label marks the button selected; the name carries the word.
                .accessibilityHidden(true)
            Text(tool.name).font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
            if let argument = tool.argument, !argument.isEmpty {
                Text(argument).font(Theme.mono(11.5)).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.middle)
            }
            if let ms = tool.durationMS {
                Text(Format.duration(ms: ms)).font(Theme.mono(11)).foregroundStyle(Theme.text3)
            }
            if opens {
                Image(systemName: open ? "chevron.down" : "chevron.right")
                    .font(.system(size: 8.5, weight: .semibold))
                    .foregroundStyle(Theme.text3)
                    .accessibilityHidden(true)
            }
            Spacer(minLength: 0)
        }
    }

    static func symbol(_ status: ToolStatus) -> String {
        switch status {
        case .running: "circle.dotted"
        case .succeeded: "checkmark.circle"
        case .failed: "xmark.circle"
        case .interrupted: "stop.circle"
        case .denied: "hand.raised"
        case .unresolved: "questionmark.circle"
        }
    }
}

/// Something the run reported about itself, quiet unless it went wrong.
private struct NoticeLine: View {
    let notice: NoticeItem

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            Image(systemName: notice.severity == .failure || notice.severity == .warning ? "exclamationmark.circle" : "info.circle")
                .font(.system(size: 11))
                .foregroundStyle(notice.severity == .failure ? Theme.red : notice.severity == .warning ? Theme.amber : Theme.text3)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(notice.title).font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
                if let detail = notice.detail, !detail.isEmpty {
                    Text(detail).font(Theme.ui(12)).foregroundStyle(Theme.text3).textSelection(.enabled)
                }
            }
        }
    }
}

#if DEBUG
    /// `-CC_SHOWN_OUT <path>`: each time the open run's rows change, the ids Kit holds for it (`shown(for:)`), one per
    /// line, then whether its head is cut, so a test compares the screen with them from outside the app.
    private struct ShownOut: ViewModifier {
        let model: AppModel
        let key: String

        func body(content: Content) -> some View {
            let shown = model.shown(for: key)
            let lines = (shown?.timeline.map(\.id) ?? []) + ["headTruncated=\(shown?.headTruncated ?? false)"]
            content.task(id: lines) {
                guard let path = UserDefaults.standard.string(forKey: "CC_SHOWN_OUT") else { return }
                try? (lines.joined(separator: "\n") + "\n").write(toFile: path, atomically: true, encoding: .utf8)
            }
        }
    }
#endif
