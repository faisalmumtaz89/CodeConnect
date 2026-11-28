import CodeConnectKit
import SwiftUI

/// Which agent a run is, as the Mac draws it (`AgentKind.mark`): Claude's asterisk, Codex's code chevron,
/// Antigravity's sparkle, a question mark for an agent this build does not know; a grey system symbol in an 18 pt
/// circle, named by its product to VoiceOver. `warn`: in the alert's red, for a run in a mode that lets the agent work
/// without asking.
struct CCAgentMark: View {
    let agent: AgentKind
    var warn = false

    var body: some View {
        Image(systemName: agent.mark.symbol).font(.system(size: agent.mark.size, weight: .bold))
            .foregroundStyle(warn ? CC.color.danger : CC.text.secondary)
            .frame(width: 18, height: 18)
            .background(Circle().fill(warn ? CC.color.danger.opacity(0.18) : Color.white.opacity(0.08)))
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(agent.productName)
    }
}
