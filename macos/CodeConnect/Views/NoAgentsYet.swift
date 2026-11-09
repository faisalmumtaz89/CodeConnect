import SwiftUI

/// The Active list with nothing in it: where agents come from, in plain words, and New agent.
struct NoAgentsYet: View {
    let newAgent: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "terminal")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(Theme.text3)
                .padding(.bottom, 4)
                .accessibilityHidden(true)
            Text("Start an agent").font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
            Text("Agents you start with CodeConnect appear here.")
                .font(Theme.ui(12.5))
                .foregroundStyle(Theme.text2)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
            ActionButton(
                title: AddKind.agent.title, key: AddKind.agent.shortcut, prominent: true, id: "noAgents.newAgent",
                action: newAgent)
                .padding(.top, 6)
        }
    }
}
