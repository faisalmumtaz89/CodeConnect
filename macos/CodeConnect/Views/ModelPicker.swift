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

    @Environment(\.startWidth) private var startWidth
    /// The button's leading edge, in `StartForm.space`.
    @State private var minX: CGFloat = 0
    private static let menuWidth: CGFloat = 440

    private var checking: Bool { StartAgent.allCases.contains { context.models($0) == .checking } }

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                Text(label).font(Theme.ui(12, .medium))
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(pick == nil ? Theme.text3 : Theme.text2)
            .padding(.horizontal, 7)
            .frame(height: 22)
            .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: "start.model"))
        .disabled(checking)
        .accessibilityIdentifier("start.model")
        .accessibilityValue(open ? "open" : "closed")
        // Above the button, clear of it by the panels' one gap, its leading edge on the composer box's (the button
        // stands 3 pt in from that edge: the box's 10 pt inset, less the button's own 7), moved left as far as it must
        // to end 8 pt inside the pane, as the branch menu keeps from the form's edges.
        .onGeometryChange(for: CGFloat.self) { $0.frame(in: .named(StartForm.space)).minX } action: { minX = $0 }
        .overlay(alignment: .bottomLeading) {
            if open {
                menu.openPanel()
                    .offset(x: -3 + min(0, startWidth - 8 - (minX - 3 + Self.menuWidth)), y: -(22 + Theme.panelGap))
            }
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
        .frame(width: Self.menuWidth)
        .frame(maxHeight: 360)
        .fixedSize(horizontal: false, vertical: true)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
    }

    private func header(_ text: String) -> some View {
        Text(text).font(Theme.ui(11, .semibold)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.top, 5)
            .padding(.bottom, 3)
    }

    private func note(_ text: String) -> some View {
        Text(text).font(Theme.ui(12)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.vertical, 4)
            .fixedSize(horizontal: false, vertical: true)
    }

    private var divider: some View {
        Rectangle().fill(Theme.hairline).frame(height: 1).padding(.vertical, 5)
    }

    /// The model's name, and its description beside it: the menu's row `index`.
    private func row(_ choice: StartModelChoice, index: Int) -> some View {
        let id = "start.model.\(choice.agent.rawValue).\(choice.isDefault ? "default" : choice.model ?? "default")"
        let select = {
            choose(choice)
            open = false
        }
        return Button(action: select) {
            HStack(spacing: 8) {
                Image(systemName: "checkmark")
                    .font(.system(size: 9.5, weight: .bold))
                    .opacity(choice == pick ? 1 : 0)
                    .frame(width: 12)
                    .accessibilityHidden(true)
                Text(choice.label).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1).fixedSize()
                if let detail = choice.detail {
                    ModelDetail(text: detail)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 24)
            .contentShape(Rectangle())
        }
        .buttonStyle(.panelRow)
        .panelRow(index, id: id, choose: select)
        .padding(.horizontal, 4)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(choice == pick ? .isSelected : [])
    }
}
