import CodeConnectKit
import SwiftUI

/// A run's conversation as a reading column, oldest at the top, opened at the newest: your words, the agent's prose
/// and code, each tool call folded to one line with its output under it, a decision where it was made, and the run's
/// notices; a request still waiting on you keeps its place as one line, and is answered at the pane's foot. A List,
/// which measures the rows it shows and builds only those near the screen. It opens at the newest row and follows new
/// rows until the reader scrolls away from the end (`Tail`); a reader who went up stays where they are as rows land
/// below, and is followed again once they come back to the end. When a page of earlier history lands above, the row
/// they were at is put back where it was (`PageAnchor`). Reaching the top pages back on its own, as the iPhone does.
struct Conversation: View {
    let model: AppModel
    let key: String
    @State private var tail = Tail()

    var body: some View {
        let items = model.shown(for: key)?.timeline ?? []
        let places = TimelineItem.places(items)
        ScrollViewReader { proxy in
            List {
                Earlier(model: model, key: key).columnRow()
                ForEach(Array(zip(places, items)), id: \.0) { _, item in
                    ConversationRow(item: item, model: model).columnRow()
                }
                // The tail: the follow's target, and the row whose probe finds the scroll view.
                Color.clear.frame(height: 1).background(TailProbe(tail: tail)).id(Self.end).columnRow()
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .environment(\.defaultMinListRowHeight, 0)
            .environment(\.conversationTail, tail)
            // The follow: any change of the last item, a new row or a grown one, scrolls to the end for a reader who
            // has not left it, as the iPhone's does. Nothing else scrolls for it but `Tail`'s re-pin, so the List's
            // own corrections can start no scroll. While a row is held for a page, the hold's placement is the move.
            .onChange(of: items.last) {
                guard !tail.isAway, !tail.anchor.isPlacing else { return }
                noteFollow("FOLLOW")
                proxy.scrollTo(Self.end, anchor: .bottom)
            }
            .onChange(of: model.isReadyToPage(key)) { _, ready in
                if ready { tail.wake() }
            }
            .onAppear {
                proxy.scrollTo(Self.end, anchor: .bottom)
                tail.reachedTop = { [weak model, key] in model?.reachedTop(key: key) }
                tail.anchor.timeline = { [weak model, key] in model?.shown(for: key)?.timeline ?? [] }
                tail.anchor.pageBackOnItsOwn = { [weak model, key] in model?.pageBackOnItsOwn(key: key) }
                model.onWindowChange(of: key) { [weak tail] shown, next, change in
                    tail?.willChange(shown: shown, next: next, change: change)
                }
            }
            .onDisappear {
                model.onWindowChange(of: key, nil)
                tail.detach()
            }
        }
        #if DEBUG
            .modifier(ShownOut(model: model, key: key))
        #endif
    }

    private static let end = "session.conversation.end"
}

private extension View {
    /// A row of the column: no separator, no background of its own, the column's side margins, and half the gap
    /// between rows above and below it.
    func columnRow() -> some View {
        listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 7, leading: 28, bottom: 7, trailing: 28))
            .listRowBackground(Color.clear)
    }
}

/// The row above the first: while a page of earlier history is on its way it says so, at one height either way.
/// Reaching the top is what asks for the page (`Tail`).
private struct Earlier: View {
    let model: AppModel
    let key: String

    var body: some View {
        let loading = model.isLoadingEarlier(key)
        Text("Loading earlier history")
            .font(Theme.ui(12))
            .foregroundStyle(Theme.text3)
            .frame(maxWidth: .infinity)
            .opacity(loading ? 1 : 0)
            .accessibilityHidden(!loading)
            .accessibilityIdentifier("session.earlier")
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
            Prompt(text: text, isCommand: isCommand, notSent: item.notSent)
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

/// What you said: one raised block, a slash command in the code face; under it, when a Stop cancelled the send, the
/// composer's own note shape saying so.
private struct Prompt: View {
    let text: String
    let isCommand: Bool
    let notSent: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            block
            if notSent {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: "exclamationmark.circle").foregroundStyle(Theme.amber).accessibilityHidden(true)
                    Text("Not sent: stopped").foregroundStyle(Theme.text2)
                }
                .font(Theme.ui(12))
            }
        }
    }

    private var block: some View {
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

/// Text that must keep its lines: code, a command's output. Drawn `displaySafe`, as the prose around it is: the
/// characters that reorder a line would otherwise draw different code from what the bytes say.
private struct CodeBlock: View {
    let text: String

    var body: some View {
        Text(text.displaySafe)
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

/// A tool call folded to one line: how it ended, its name, what it was given and how long it took. One with what it was
/// sent or what came back opens to show them, in that order, as the iPhone does.
private struct ToolLine: View {
    let tool: ToolItem
    let id: String
    @State private var open = false
    @Environment(\.conversationTail) private var tail

    /// What it was sent, what came back, or that the computer cut it short: something to open.
    private var opens: Bool { tool.input?.isNull == false || tool.output?.isEmpty == false || tool.isTruncated }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if opens {
                Button { toggle() } label: { line(opens: true) }
                    .buttonStyle(.hover(outset: 4, id: "session.tool.\(id)"))
                    .accessibilityLabel(spoken)
                    .accessibilityIdentifier("session.tool.\(id)")
                    .accessibilityValue(open ? "open" : "closed")
                if open {
                    if let input = tool.input, !input.isNull { CodeBlock(text: input.prettyJSONString) }
                    if let output = tool.output, !output.isEmpty { CodeBlock(text: output) }
                    // Kit marks the call or its result cut whole by the computer: which one, the row cannot tell.
                    if tool.isTruncated {
                        Text("Too large to send whole: \(HostDevice.current.computer("the computer")) cut it short")
                            .font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
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

    /// Opening the row is choosing to read it: the follow hears first, before the row grows.
    private func toggle() {
        if !open { tail?.leave() }
        open.toggle()
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
