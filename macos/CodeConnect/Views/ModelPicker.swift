import CodeConnectKit
import SwiftUI

/// The model button at the bar's end, and its menu: a section an agent, opening upward over the form with its trailing
/// edge on the button's (screen 31), as a menu at the window's edge does.
/// What each section holds and every word the app adds are Kit's (`StartContext.models`); a model's label is the
/// agent's own name for it, shown as sent.
struct ModelButton: View {
    let context: StartContext
    let pick: StartModelChoice?
    @Binding var open: Bool
    let choose: (StartModelChoice) -> Void

    @Environment(\.startBox) private var startBox
    /// The button, in `StartForm.space`.
    @State private var chip: CGRect = .zero
    /// The menu, 340 pt or the box's width, its trailing edge on the button's and inside the box.
    private var placed: (x: CGFloat, width: CGFloat) {
        let width = startBox.width > 0 ? min(340, startBox.width) : 340
        return (max(startBox.minX, chip.maxX - width), width)
    }

    private var checking: Bool { context.startAgents.contains { context.models($0) == .checking } }

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                if let pick { AgentMark(agent: AgentKind(pick.agent)) }
                Text(label).font(InputBoxChip.font).lineLimit(1).truncationMode(.tail)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(pick == nil ? Theme.text3 : Theme.text2)
            .inputBoxChip(open: open, bordered: true)
        }
        .buttonStyle(.hover(radius: 8, id: "start.model"))
        .help(label)
        // Nothing to choose while the computer is asked, or when no agent can start here (the form says why).
        .disabled(checking || context.modelChoices.isEmpty)
        .accessibilityIdentifier("start.model")
        .accessibilityValue(open ? "open" : "closed")
        // Above the button, clear of it by the panels' one gap.
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(StartForm.space)) } action: { chip = $0 }
        .overlay(alignment: .bottomLeading) {
            if open { menu.openPanel().offset(x: placed.x - chip.minX, y: -(InputBoxChip.height + Theme.panelGap)) }
        }
        .closesPanel($open)
    }

    private var label: String {
        if checking { return StartContext.checkingTitle }
        guard let pick else { return StartContext.noAgentTitle }
        return context.title(pick)
    }

    /// The rows a choice can be, in the menu's order: each agent's that has its models.
    private var rows: [StartModelChoice] {
        context.startAgents.flatMap { agent -> [StartModelChoice] in
            guard case .choices = context.models(agent) else { return [] }
            return context.modelChoices.filter { $0.agent == agent }
        }
    }

    /// The agents the menu has a section for: each that offers its models, or hasn't answered yet.
    private var shownAgents: [StartAgent] {
        context.startAgents.filter {
            switch context.models($0) {
            case .checking, .choices: true
            case .missing, .signedOut: false
            }
        }
    }

    /// As tall as its entries, up to what fits above the composer; longer, it scrolls.
    private var menu: some View {
        let order = rows
        return PanelScroll {
            VStack(alignment: .leading, spacing: 0) {
                // An agent that can't start here is left out (screen 31); the form says why under the folder where it
                // concerns the person (`AgentNotice`).
                ForEach(Array(shownAgents.enumerated()), id: \.element) { index, agent in
                    if index > 0 { divider }
                    header(agent.displayName)
                    if context.models(agent) == .checking {
                        note(StartContext.checkingTitle)
                    } else {
                        ForEach(context.modelChoices.filter { $0.agent == agent }, id: \.self) { choice in
                            row(choice, index: order.firstIndex(of: choice) ?? 0)
                        }
                    }
                }
            }
        }
        // The card's own inset each side within its width, as every panel's.
        .frame(width: placed.width - 2 * MenuMetrics.inset)
        // Its top 8 pt inside the pane (before the button is measured, its tallest).
        .frame(maxHeight: chip.minY > 0 ? min(360, chip.minY - Theme.panelGap - PanelPlace.margin) : 360)
        .fixedSize(horizontal: false, vertical: true)
        .menuCard()
    }

    private func header(_ text: String) -> some View {
        MenuHeader(text: text)
    }

    private func note(_ text: String) -> some View {
        Text(text).font(Theme.ui(12)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.vertical, 4)
            .fixedSize(horizontal: false, vertical: true)
    }

    private var divider: some View {
        MenuDivider().padding(.vertical, 5)
    }

    /// The model's name, and its description beside it: the menu's row `index`.
    private func row(_ choice: StartModelChoice, index: Int) -> some View {
        let id = "start.model.\(choice.agent.rawValue).\(choice.isDefault ? "default" : choice.model ?? "default")"
        let select = {
            choose(choice)
            open = false
        }
        return Button(action: select) {
            MenuRow(chosen: choice == pick) {
                Text(choice.label).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1).fixedSize()
                if let detail = choice.detail {
                    ModelDetail(text: detail)
                }
            }
        }
        .buttonStyle(.panelRow)
        .panelRow(index, id: id, choose: select)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(choice == pick ? .isSelected : [])
    }
}
