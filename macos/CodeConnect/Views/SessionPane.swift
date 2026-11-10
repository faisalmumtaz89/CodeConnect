import CodeConnectKit
import SwiftUI

/// One run, opened beside the list: where it runs, what it is called, how it stands, and its conversation. It reads
/// what Kit holds for the run and changes nothing; opening and closing it mark the run reviewed, as the iPhone does.
struct SessionPane: View {
    let row: FleetRow
    let model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Place(summary: row.summary, computer: row.machineName) {
                if row.summary.isCodex, model.stopUnavailable(for: row.id) == nil { StopTurn(model: model, key: row.id) }
            }
            Rectangle().fill(Theme.hairline).frame(height: 1)
            Heading(row: row, model: model)
            Conversation(model: model, key: row.id)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
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
/// last said anything, and its one tab.
private struct Heading: View {
    let row: FleetRow
    let model: AppModel

    var body: some View {
        let since = row.lastEventAt ?? row.summary.updatedDate
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
            if row.summary.isCodex { StopOutcome(activity: model.codexStop(for: row.id)) }
            VStack(spacing: 7) {
                Text("Conversation").font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                Rectangle().fill(Theme.text).frame(height: 1.5)
            }
            .fixedSize()
            .padding(.top, 6)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Conversation")
            .accessibilityAddTraits([.isHeader, .isSelected])
            .accessibilityIdentifier("session.tab.conversation")
        }
        .padding(.horizontal, 28)
        .padding(.top, 18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }

    /// The agent, and the model it says it runs when it has said.
    private var agentLine: String {
        [row.summary.agent.displayName, model.confirmedModel(for: row.id).map { ModelDisplay.from($0.name).name }]
            .compactMap { $0 }
            .joined(separator: " · ")
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

/// Stops the turn a Codex run is running, over Codex's own link: no keyboard involved. Greyed for a moment after a
/// turn changes, as Kit rules.
private struct StopTurn: View {
    let model: AppModel
    let key: String

    var body: some View {
        let stopping = model.codexStop(for: key) == .inFlight
        let greyed = model.codexControls(for: key).isStopGreyed(now: model.now)
        ActionButton(title: stopping ? "Stopping…" : "Stop this turn", key: nil, prominent: false, id: "session.stop") {
            Task { await model.stopCodexTurn(sessionKey: key) }
        }
        .disabled(stopping || greyed)
        .help(greyed ? "Try again in a moment." : "Stop the turn this run is running")
    }
}

/// What became of the last stop, while it answers the turn running now: Kit's sentence for a settled stop, or why
/// it was not sent, or that no answer came.
private struct StopOutcome: View {
    let activity: CodexControls.Activity<InterruptResult>

    var body: some View {
        switch activity {
        case .settled(let result):
            let banner = CodexProse.interrupt(result)
            line(banner.oneLine, tone: banner.tone)
        case .notSent(let reason), .sentNoAnswer(let reason):
            line(reason, tone: .warning)
        case .idle, .inFlight:
            EmptyView()
        }
    }

    private func line(_ text: String, tone: CodexProse.CodexTone) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: tone.symbol).foregroundStyle(tone.color).accessibilityHidden(true)
            Text(text).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.ui(12))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.stop.outcome")
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
