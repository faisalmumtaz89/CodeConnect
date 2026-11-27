/// The words of the confirmation a run's mode asks for before it works without asking (decisions 214/215).
///
/// Claude's "Bypass permissions" and Codex's "Full Access" each let the agent run commands and edit files
/// without asking first, for this run until the mode is changed back. The confirmation is shown from a live
/// run's mode list for the entry the daemon marks `confirm`, and nothing is sent until the confirm button —
/// Escape and cancel send nothing.
///
/// Read by both apps so the words are identical. Only what is true is stated (97): no claim about safety, and the mode
/// only turns off the agent's prompts, for this run until the mode is changed back.
public enum ModeConfirmation {
    /// The ask for the agent whose mode is being confirmed.
    public static func words(for agent: AgentKind) -> AskWords {
        let withoutAsking = AskWords.Item(mark: "apple.terminal", words: "Runs commands and edits files without asking")
        // The mode is the run's own: the note says so, and until when (decision 221).
        let inThisRun = "In this session, until you change the mode back."
        switch agent {
        case .codex:
            // The folder and network lines are true: the confirmed mode is Codex's own full-access preset — sandbox
            // `danger-full-access` and approval `never` — set through X6.
            return AskWords(
                title: "Give Codex full access?",
                items: [
                    withoutAsking,
                    .init(mark: "folder", words: "Works outside this project’s folder"),
                    .init(mark: "globe", words: "Uses the network"),
                ],
                note: inThisRun, cancel: "Keep Asking", confirm: "Give Full Access")
        case .claude, .antigravity, .unsupported:
            // Claude's lines promise nothing about a sandbox: the mode only turns off claude's prompts. None of an
            // Antigravity run's modes lets agy work without asking (decision 214), so it never asks this.
            return AskWords(
                title: "Let Claude work without asking?", items: [withoutAsking],
                note: inThisRun, cancel: "Keep Asking", confirm: "Bypass Permissions")
        }
    }
}
