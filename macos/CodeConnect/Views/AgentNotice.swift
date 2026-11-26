import AppKit
import CodeConnectKit
import SwiftUI

/// Why an agent can't start on the form's computer, under its folders, by the Kit's one rule for every agent
/// (`StartContext.agentNotices`): the computer's own sentence for each, and for Claude Code signed out on a computer
/// other than this Mac, how to sign in there (the owner's 2A). Nothing while each agent it would name can start.
struct AgentNotice: View {
    let context: StartContext
    /// The form's computer is not this Mac.
    let elsewhere: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(context.agentNotices, id: \.agent) { notice in
                self.notice(notice.sentence, signIn: elsewhere && notice.agent == .claude && claudeSignedOut)
            }
        }
    }

    private var claudeSignedOut: Bool {
        if case .signedOut = context.models(.claude) { return true }
        return false
    }

    private func notice(_ sentence: String, signIn: Bool) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 9) {
            Image(systemName: "exclamationmark.circle").foregroundStyle(Theme.amber).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 8) {
                Text(sentence).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("agent.notice.sentence")
                if signIn {
                    Text(context.signInThere).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("agent.notice.there")
                        // "CodeConnect sees it when you're done": see `StartContext.watchSignIn`.
                        .task(id: context.id) { await context.watchSignIn() }
                    Text(StartContext.claudeSignIn).font(Theme.mono(12)).foregroundStyle(Theme.text)
                        .textSelection(.enabled)
                        .padding(.leading, 10)
                        .padding(.trailing, CopyButton.slot)
                        .padding(.vertical, 6)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                        .accessibilityIdentifier("agent.notice.command")
                        .overlay(alignment: .trailing) {
                            CopyButton(text: StartContext.claudeSignIn, id: "agent.notice.copy")
                        }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.code.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("agent.notice")
    }
}
