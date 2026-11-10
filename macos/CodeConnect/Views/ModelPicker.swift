import CodeConnectKit
import SwiftUI

/// The model button at the composer's bottom left, and its menu: a section an agent, opening upward over the form.
/// What each section holds and every word the app adds are Kit's (`StartContext.models`); a model's label and detail
/// are the agent's own, shown as sent.
struct ModelButton: View {
    let context: StartContext
    let pick: StartModelChoice?
    @Binding var open: Bool
    let choose: (StartModelChoice) -> Void

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
        // Above the button, clear of it: the menu's bottom 6pt over the button's top.
        .overlay(alignment: .bottomLeading) {
            if open { menu.offset(y: -28) }
        }
    }

    private var label: String {
        if checking { return StartContext.checkingTitle }
        guard let pick else { return StartContext.noAgentTitle }
        return context.title(pick)
    }

    /// As tall as its entries, up to what fits above the composer; longer, it scrolls.
    private var menu: some View {
        let agentOnly = StartAgent.allCases.allSatisfy { context.models($0) == .agentOnly }
        return ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(StartAgent.allCases.enumerated()), id: \.element) { index, agent in
                    if index > 0, !agentOnly { divider }
                    if !agentOnly { header(agent.displayName) }
                    switch context.models(agent) {
                    case .checking: note(StartContext.checkingTitle)
                    case .missing(let reason), .signedOut(let reason): note(reason)
                    case .choices, .agentOnly:
                        ForEach(context.modelChoices.filter { $0.agent == agent }, id: \.self) { row($0) }
                    }
                }
                if agentOnly {
                    divider
                    Text(StartContext.updateForModels)
                        .font(Theme.ui(11))
                        .foregroundStyle(Theme.text3)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 4)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(.vertical, 5)
        }
        .frame(width: 440)
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

    /// The agent's label, and its detail beside it, cut short with the whole of it on hover.
    private func row(_ choice: StartModelChoice) -> some View {
        Button {
            choose(choice)
            open = false
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "checkmark")
                    .font(.system(size: 9.5, weight: .bold))
                    .opacity(choice == pick ? 1 : 0)
                    .frame(width: 12)
                    .accessibilityHidden(true)
                Text(choice.label).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1).fixedSize()
                if let detail = choice.detail {
                    Text(detail).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.tail)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 24)
            .background(choice == pick ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: "start.model.\(choice.agent.rawValue).\(choice.isDefault ? "default" : choice.model ?? "default")"))
        .padding(.horizontal, 4)
        .help(choice.detail ?? "")
        .accessibilityIdentifier("start.model.\(choice.agent.rawValue).\(choice.isDefault ? "default" : choice.model ?? "default")")
        .accessibilityAddTraits(choice == pick ? .isSelected : [])
    }
}
