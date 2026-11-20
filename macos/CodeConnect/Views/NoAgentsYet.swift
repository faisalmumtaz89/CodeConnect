import CodeConnectKit
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

/// A computer's list while its link is not working: what the sidebar's word means and what to do, in the link's own
/// title and sentence (Kit's `LinkNotice`), and its one action when it has one.
struct ComputerNotice: View {
    let notice: LinkNotice
    let act: (title: String, run: () -> Void)?

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.system(size: 26, weight: .light))
                .foregroundStyle(Theme.text3)
                .padding(.bottom, 4)
                .accessibilityHidden(true)
            Text(notice.title).font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
            // A host in the sentence is a code span, as the iPhone's banner draws it.
            Text((try? AttributedString(markdown: notice.sentence)) ?? AttributedString(notice.sentence))
                .font(Theme.ui(12.5))
                .foregroundStyle(Theme.text2)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityIdentifier("fleet.notice.sentence")
            if let act {
                ActionButton(title: act.title, key: nil, prominent: false, id: "fleet.notice.act", action: act.run)
                    .padding(.top, 6)
            }
        }
        .frame(maxWidth: 420)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("fleet.notice")
    }
}
