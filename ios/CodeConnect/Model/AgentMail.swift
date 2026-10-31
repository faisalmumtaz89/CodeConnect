import Foundation

/// **Another agent's message**, as CodeConnect hands it to a session: a block headed
/// `[CodeConnect] Message from cc-2@studio (Codex · project api). Reply with send_message
/// to cc-2@studio.`, a blank line, then what the agent wrote (`ccd/src/agent_messages.rs`,
/// `take_agent_message`).
///
/// It reaches a working Claude as Stop hook feedback, several in one block with a blank
/// line between them, and an idle Claude or Codex as a prompt typed into its box. Either
/// way the Mac shows it as text addressed to the agent, and the phone shows who sent it.
struct AgentMail: Equatable {
    static let header = "[CodeConnect] Message from "

    /// `cc-2@studio`.
    let from: String
    /// What the agent wrote.
    let text: String

    /// The messages in one block of text, or none when the block is not CodeConnect's:
    /// it must start with the header, as every block the daemon writes does.
    static func parse(_ block: String) -> [AgentMail] {
        guard block.hasPrefix(header) else { return [] }
        let parts = String(block.dropFirst(header.count))
            .components(separatedBy: "\n\n" + header)
        return parts.compactMap { part in
            guard let from = part.split(separator: " ", maxSplits: 1).first,
                let body = part.range(of: "\n\n")
            else { return nil }
            return AgentMail(from: String(from), text: String(part[body.upperBound...]))
        }
    }
}
