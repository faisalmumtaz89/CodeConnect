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
            Text(FleetWords.startAnAgent).font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
            Text(FleetWords.agentsAppearHere)
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

/// A computer's list while that computer runs a different version of CodeConnect: what the sidebar's "Different
/// version" means and what cures it, in the refusal's own sentence, and Try Again once it is cured: a refused
/// connection does not dial again by itself.
struct DifferentVersion: View {
    let sentence: String
    let tryAgain: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.system(size: 26, weight: .light))
                .foregroundStyle(Theme.text3)
                .padding(.bottom, 4)
                .accessibilityHidden(true)
            Text("Different version").font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
            Text(sentence)
                .font(Theme.ui(12.5))
                .foregroundStyle(Theme.text2)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityIdentifier("fleet.differentVersion.sentence")
            ActionButton(title: "Try Again", key: nil, prominent: false, id: "fleet.differentVersion.tryAgain", action: tryAgain)
                .padding(.top, 6)
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("fleet.differentVersion")
    }
}
