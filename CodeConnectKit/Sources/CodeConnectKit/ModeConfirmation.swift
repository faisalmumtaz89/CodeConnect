/// The words of the confirmation a run's mode asks for before it works without asking (decisions 214/215).
///
/// Claude's "Bypass permissions" and Codex's "Full Access" each let the agent run commands and edit files
/// without asking first, for this run until the mode is changed back. The confirmation is shown from a live
/// run's mode list for the entry the daemon marks `confirm`, and nothing is sent until the confirm button —
/// Escape and cancel send nothing.
///
/// Read by both apps so the words are identical: the iPhone and the Mac show the same four strings. Only
/// what is true is stated (97): no claim about safety, and the mode only turns off the agent's prompts.
public enum ModeConfirmation {
    public struct Words: Sendable, Hashable {
        public let title: String
        public let message: String
        /// The destructive confirm button.
        public let confirm: String
        /// The cancel button, which Escape also triggers.
        public let cancel: String
    }

    /// The four strings for the agent whose mode is being confirmed.
    public static func words(for agent: AgentKind) -> Words {
        switch agent {
        case .codex:
            // The folder-and-network clause is true: the confirmed mode is Codex's own full-access preset
            // — sandbox `danger-full-access` and approval `never` — set through X6.
            return Words(
                title: "Give Codex full access?",
                message: "Codex will run commands and edit files in this run without asking you first, "
                    + "outside this project’s folder and with network access, until you change the mode back.",
                confirm: "Give Full Access",
                cancel: "Keep Asking")
        case .claude, .antigravity, .unsupported:
            // Claude's message promises nothing about a sandbox: the mode only turns off claude's prompts. None of an
            // Antigravity run's modes lets agy work without asking (decision 214), so it never asks this.
            return Words(
                title: "Let Claude work without asking?",
                message: "Claude will run commands and edit files in this run without asking you first, "
                    + "until you change the mode back.",
                confirm: "Bypass Permissions",
                cancel: "Keep Asking")
        }
    }
}
