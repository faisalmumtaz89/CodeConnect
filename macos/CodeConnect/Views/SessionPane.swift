import CodeConnectKit
import SwiftUI

/// One run, opened beside the list: where it runs, what it is called, how it stands, and its conversation or its
/// changes. Opening and closing it mark the run reviewed, as the iPhone does.
struct SessionPane: View {
    let row: FleetRow
    let model: AppModel
    /// Which tab is shown: the window's, which keeps it for the run it was chosen on.
    @Binding var tab: SessionTab
    @State private var height: CGFloat = 0
    /// A request holds the reply box's place.
    @State private var boxTaken = false
    /// How tall the reply box stands (none while a request holds its place): a request shares the half pane with it.
    @State private var replyHeight: CGFloat = 0
    /// The reply field (or its command list) holds the keyboard.
    @FocusedValue(\.composerHoldsEscape) private var composerHolds
    /// The window's Add sheet, and its setup checklist: either covers the window, and then a request's answers take no
    /// keys, as the window's own Esc stands down for the Add sheet.
    @FocusedValue(\.adding) private var adding
    @Environment(Setup.self) private var setup
    @Environment(\.agentsShown) private var agentsShown

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Place(summary: row.summary, computer: row.machineName)
            Rectangle().fill(Theme.hairline).frame(height: 1)
            Heading(row: row, model: model, tab: $tab)
            ZStack(alignment: .topLeading) {
                if tab == .conversation {
                    VStack(spacing: 0) {
                        Conversation(model: model, key: row.id)
                        // What waits on you sits at the foot, outside the conversation's scroll, in at most half the
                        // pane with the reply box when that shows, so the window holds them both at its smallest; its
                        // answers take the reply box's place. Not while the reply field holds the keyboard: the box
                        // stays the writer's until it lets go. Once a request has the box, the field can't take the
                        // keyboard back, so what holds it then is the request's own field.
                        PendingRequests(
                            model: model, key: row.id, maxHeight: max(0, height / 2 - replyHeight),
                            held: composerHolds == true && !boxTaken,
                            sheetUp: adding.map { $0.wrappedValue != nil } ?? false || setup.shown)
                        // Standing aside, the reply box stays mounted: its words, its last outcome and a send on its
                        // way are kept. It takes no room, no click, no keyboard (Tab can't reach it) and no reading.
                        // Not clipped: its command list opens above it.
                        ComposerBar(key: row.id)
                            .frame(height: boxTaken ? 0 : nil, alignment: .top)
                            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { replyHeight = $0 }
                            .opacity(boxTaken ? 0 : 1)
                            .allowsHitTesting(!boxTaken)
                            .disabled(boxTaken)
                            .accessibilityHidden(boxTaken)
                    }
                    .onPreferenceChange(HoldsReplyBox.self) { boxTaken = $0 }
                }
                if tab == .changes { ChangesTab(model: model, key: row.id) }
            }
        }
        #if DEBUG
            .task { if let shown = RenderWindow.sessionTab { tab = shown } }
        #endif
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        .background(Theme.window)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("session.pane")
        // Seen only while on screen: under Settings nothing is marked, and coming back from it is seeing the run.
        .onAppear { if agentsShown { model.markReviewed(row.id) } }
        .onDisappear { if agentsShown { model.markReviewed(row.id) } }
        .onChange(of: agentsShown) { _, shown in if shown { model.markReviewed(row.id) } }
    }
}

/// Where the run is: its computer, its folder as a shell prints it, and the branch checked out there when git
/// names one (none outside a repository or on a detached head).
private struct Place: View {
    let summary: SessionSummary
    let computer: String?
    @Environment(\.headerClearance) private var headerClearance

    var body: some View {
        HStack(spacing: 6) {
            Text(computer ?? "Unknown computer").lineLimit(1)
            separator
            Text(FleetGroup.shellPath(summary.shownCwd)).font(Theme.mono(11.5)).lineLimit(1).truncationMode(.head)
            if let branch = summary.gitBranch?.displaySafe {
                separator
                Text(branch).font(Theme.mono(11.5)).lineLimit(1).accessibilityIdentifier("session.branch")
            }
            Spacer(minLength: 8)
        }
        .font(Theme.ui(12))
        .foregroundStyle(Theme.text2)
        .padding(.horizontal, 20)
        .padding(.leading, max(0, headerClearance.leading - 20))
        .padding(.trailing, max(0, headerClearance.trailing - 20))
        .frame(height: 52)
    }

    private var separator: some View {
        Image(systemName: "chevron.right").font(.system(size: 8, weight: .semibold)).foregroundStyle(Theme.text3)
            .accessibilityHidden(true)
    }
}

/// What the run is called, how it stands, which agent and model it is, the lines it has changed, how long ago it
/// last said anything, and its two tabs.
private struct Heading: View {
    let row: FleetRow
    let model: AppModel
    @Binding var tab: SessionTab

    var body: some View {
        let since = row.lastActive
        VStack(alignment: .leading, spacing: 10) {
            Text(row.label.inline)
                .font(Theme.ui(18, .semibold))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .truncationMode(.middle)
                .accessibilityIdentifier("session.title")
            HStack(spacing: 12) {
                HStack(spacing: 6) {
                    StateIcon(status: row.status)
                    Text(row.status.label)
                }
                // The mark and the word say one thing; read once.
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(row.status.label)
                .accessibilityIdentifier(row.status == .ended ? "session.ended" : "session.state")
                HStack(spacing: 6) {
                    AgentMark(agent: row.summary.agent)
                    agentLine
                }
                if let diff = row.summary.diffStat {
                    LineDelta(added: diff.added, removed: diff.removed).accessibilityIdentifier("session.diffStat")
                }
                AgeText(since: since) { Format.ago(since: since, now: $0) }
                    .foregroundStyle(Theme.text3)
                    .monospacedDigit()
            }
            .font(Theme.ui(12))
            .foregroundStyle(Theme.text2)
            HStack(spacing: 20) {
                ForEach(SessionTab.allCases, id: \.self) { each in
                    UnderlineTab(
                        title: each.title, id: each.id, selected: tab == each,
                        count: each == .changes ? ChangesTab.fileCount(model: model, key: row.id) : nil
                    ) { tab = each }
                }
            }
            .padding(.top, 6)
        }
        .padding(.horizontal, 28)
        .padding(.top, 18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }

    /// The agent, and the model it says it runs when it has said: by the label of the run's menu entry for that
    /// model when there is one, so the line and the settings list spell it alike, else as the agent reported it, in
    /// code type (Kit's `modelName`). A run that takes no reply names, after the model, the effort it ended with (Kit's
    /// `headerEffort`); any other run names its effort on its reply row.
    private var agentLine: Text {
        let agent = Text(row.summary.agent.productName)
        guard let confirmed = model.confirmedModel(for: row.id) else { return agent }
        let name = RunSettings.modelName(
            confirmed: confirmed.name, menus: row.summary.menus, label: row.summary.modelLabel)
        let line = Text("\(agent) · \(name.reported ? Text(name.text).font(Theme.mono(11.5)) : Text(name.text))")
        guard let effort = RunSettings.headerEffort(summary: row.summary) else { return line }
        return Text("\(line) · \(effort)")
    }
}

/// What the pane shows below its heading.
enum SessionTab: CaseIterable {
    case conversation
    case changes

    var title: String {
        switch self {
        case .conversation: return "Conversation"
        case .changes: return "Changes"
        }
    }

    /// What a driver finds the tab by.
    var id: String {
        switch self {
        case .conversation: return "session.tab.conversation"
        case .changes: return "session.tab.changes"
        }
    }
}

/// Lines added and removed, green and red: the one place those two colours mean something.
struct LineDelta: View {
    let added: UInt64
    let removed: UInt64

    var body: some View {
        HStack(spacing: 5) {
            Text(verbatim: "+\(added)").foregroundStyle(Theme.green)
            Text(verbatim: "−\(removed)").foregroundStyle(Theme.red)
        }
        .font(Theme.mono(11))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("+\(added) −\(removed)")
    }
}

extension CodexProse.CodexTone {
    /// A Kit outcome's tone on the Mac: its colour only where it means something, and one symbol per tone.
    var color: Color {
        switch self {
        case .info, .success: Theme.text3
        case .warning: Theme.amber
        case .danger: Theme.red
        }
    }

    var symbol: String {
        switch self {
        case .info: "info.circle"
        case .success: "checkmark.circle"
        case .warning: "exclamationmark.circle"
        case .danger: "xmark.circle"
        }
    }
}
