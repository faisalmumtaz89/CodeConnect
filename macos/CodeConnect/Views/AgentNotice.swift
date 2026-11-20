import AppKit
import CodeConnectKit
import SwiftUI

/// Why Claude Code can't start on the form's computer, under its folders: the computer's own sentence and, when it is
/// signed out on a computer other than this Mac, how to sign in there (the owner's 2A). Nothing while it can start.
struct AgentNotice: View {
    let context: StartContext
    /// The form's computer is not this Mac.
    let elsewhere: Bool
    @State private var copied = false

    var body: some View {
        switch context.models(.claude) {
        case .missing(let sentence): notice(sentence, signIn: false)
        case .signedOut(let sentence): notice(sentence, signIn: elsewhere)
        case .checking, .choices: EmptyView()
        }
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
                    HStack(spacing: 8) {
                        Text(StartContext.claudeSignIn).font(Theme.mono(12)).foregroundStyle(Theme.text)
                            .textSelection(.enabled)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 6)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                            .accessibilityIdentifier("agent.notice.command")
                        ActionButton(title: copied ? "Copied" : "Copy", key: nil, prominent: false, id: "agent.notice.copy") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(StartContext.claudeSignIn, forType: .string)
                            copied = true
                        }
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
