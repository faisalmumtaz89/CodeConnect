import CodeConnectKit
import SwiftUI

/// The Active list with nothing in it: where runs come from, in plain words at the list's top, and New Run.
struct NoAgentsYet: View {
    let newAgent: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(FleetWords.noRunsYet).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
            Text(FleetWords.runsComeFrom)
                .font(Theme.ui(12))
                .foregroundStyle(Theme.text3)
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
            ActionButton(
                title: AddKind.agent.title, key: AddKind.agent.shortcut, prominent: false, id: "noAgents.newAgent",
                action: newAgent)
                .padding(.top, 6)
        }
        .accessibilityElement(children: .contain)
    }
}
