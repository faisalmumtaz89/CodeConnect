import CodeConnectKit
import SwiftUI

/// The model button at the composer's bottom left, and its menu: a section an agent, opening upward over the form.
/// What each section holds and every word the app adds are Kit's (`StartContext.models`); a model's label is the
/// agent's own name for it, shown as sent.
struct ModelButton: View {
    let context: StartContext
    let pick: StartModelChoice?
    @Binding var open: Bool
    let choose: (StartModelChoice) -> Void

    @Environment(\.startBox) private var startBox
    /// The button's top leading corner, in `StartForm.space`.
    @State private var minX: CGFloat = 0
    @State private var minY: CGFloat = 0
    private var placed: (x: CGFloat, width: CGFloat) {
        StartPanel.place(widest: 440, leading: minX, box: startBox)
    }

    private var checking: Bool { StartAgent.allCases.contains { context.models($0) == .checking } }

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                if let pick { AgentMark(agent: pick.agent == .claude ? .claude : .codex) }
                Text(label).font(InputBoxChip.font).lineLimit(1).truncationMode(.middle)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(pick == nil ? Theme.text3 : Theme.text2)
            .inputBoxChip(open: open, bordered: true)
        }
        .buttonStyle(.hover(radius: 8, id: "start.model"))
        .help(label)
        .disabled(checking)
        .accessibilityIdentifier("start.model")
        .accessibilityValue(open ? "open" : "closed")
        // Above the button, clear of it by the panels' one gap, inside the box by the start form's one rule
        // (`StartPanel`).
        .onGeometryChange(for: CGPoint.self) { $0.frame(in: .named(StartForm.space)).origin } action: {
            minX = $0.x
            minY = $0.y
        }
        .overlay(alignment: .bottomLeading) {
            if open { menu.openPanel().offset(x: placed.x - minX, y: -(InputBoxChip.height + Theme.panelGap)) }
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
        StartAgent.allCases.flatMap { agent -> [StartModelChoice] in
            guard case .choices = context.models(agent) else { return [] }
            return context.modelChoices.filter { $0.agent == agent }
        }
    }

    /// As tall as its entries, up to what fits above the composer; longer, it scrolls.
    private var menu: some View {
        let order = rows
        return PanelScroll {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(StartAgent.allCases.enumerated()), id: \.element) { index, agent in
                    if index > 0 { divider }
                    header(agent.displayName)
                    switch context.models(agent) {
                    case .checking: note(StartContext.checkingTitle)
                    case .missing(let reason), .signedOut(let reason): note(reason)
                    case .choices:
                        ForEach(context.modelChoices.filter { $0.agent == agent }, id: \.self) { choice in
                            row(choice, index: order.firstIndex(of: choice) ?? 0)
                        }
                    }
                }
            }
            .padding(.vertical, 5)
        }
        .frame(width: placed.width)
        // Its top 8 pt inside the pane (before the button is measured, its tallest).
        .frame(maxHeight: minY > 0 ? min(360, minY - Theme.panelGap - StartPanel.margin) : 360)
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
