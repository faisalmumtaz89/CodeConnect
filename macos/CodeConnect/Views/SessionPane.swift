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

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Place(summary: row.summary, computer: row.machineName) {
                // A Codex run or a Claude run started from the app, whenever Kit says it can be stopped; only on the
                // conversation, where the composer's note says what became of the stop.
                if tab == .conversation, model.stopUnavailable(for: row.id) == nil {
                    StopTurn(model: model, key: row.id)
                }
            }
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
        .onAppear { model.markReviewed(row.id) }
        .onDisappear { model.markReviewed(row.id) }
    }
}

/// Where the run is: its computer, its folder as a shell prints it, and the branch checked out there when git
/// names one (none outside a repository or on a detached head); what can be done to the run sits at its end.
private struct Place<Actions: View>: View {
    let summary: SessionSummary
    let computer: String?
    @ViewBuilder let actions: Actions

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
            actions
        }
        .font(Theme.ui(12))
        .foregroundStyle(Theme.text2)
        .padding(.horizontal, 20)
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
                    Text(agentLine)
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
                    TabButton(
                        tab: each, selected: tab == each,
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
    /// model when there is one, so the line and the settings list spell it alike (Kit's `modelName`).
    private var agentLine: String {
        [
            row.summary.agent.displayName,
            model.confirmedModel(for: row.id).map { RunSettings.modelName(confirmed: $0.name, menus: row.summary.menus) },
        ]
            .compactMap { $0 }
            .joined(separator: " · ")
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

/// One tab: its word, and a count beside it when it has one, underlined while it is the one shown. Every tab is a
/// button; only the shown one reads selected, and each keeps the header trait the single Conversation tab had.
private struct TabButton: View {
    let tab: SessionTab
    let selected: Bool
    let count: Int?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 7) {
                HStack(spacing: 5) {
                    Text(tab.title).foregroundStyle(selected ? Theme.text : Theme.text3)
                    if let count { Text(verbatim: "\(count)").foregroundStyle(Theme.text3) }
                }
                .font(Theme.ui(12.5, .medium))
                Rectangle().fill(selected ? Theme.text : Color.clear).frame(height: 1.5)
            }
            .fixedSize()
        }
        .buttonStyle(.hover(over: true, id: tab.id))
        .accessibilityAddTraits(selected ? [.isHeader, .isSelected] : [.isHeader])
        .accessibilityIdentifier(tab.id)
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

/// Stops the turn the run is running, a Codex run's over Codex's own link and a Claude run's through the host it was
/// started on: no keyboard involved. Greyed for a moment after a turn changes, as Kit rules.
private struct StopTurn: View {
    let model: AppModel
    let key: String

    var body: some View {
        let stopping = model.codexStop(for: key) == .inFlight
        let greyed = model.codexControls(for: key).isStopGreyed(now: model.now)
        ActionButton(title: stopping ? SessionWords.stopping : SessionWords.stopThisTurn, key: nil, prominent: false, id: "session.stop") {
            Task { await model.stopCodexTurn(sessionKey: key) }
        }
        .disabled(stopping || greyed)
        .help(greyed ? SessionWords.tryAgainInAMoment : SessionWords.stopHelp)
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
