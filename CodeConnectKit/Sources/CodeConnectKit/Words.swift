/// The words both apps show for the same thing, said once.
///
/// A sentence the Mac and the iPhone both draw lives here, so the two cannot
/// drift apart; a view keeps only the words its own platform alone shows.

/// The fleet list: its title, the way to start a run, the empty states and the
/// questions asked before a run ends or goes.
public enum FleetWords {
    public static let projects = "Projects"
    public static let newAgent = "New agent"
    public static let startAnAgent = "Start an agent"
    public static let agentsAppearHere = "Agents you start with CodeConnect appear here."
    public static let noEndedRuns = "No ended runs."
    public static let noComputersYet = "No computers yet"

    public static let endTitle = "End this run?"
    public static func endMessage(_ agent: String) -> String {
        "\(agent) stops, and the run moves to \(FleetStatus.ended.label)."
    }

    public static func removeTitle(_ project: String) -> String { "Remove \(project)?" }
    public static func removeKeeps(_ agent: String) -> String { "\(agent)’s own transcript stays on the computer." }
    public static func removeManyTitle(_ count: Int) -> String { "Remove \(count) runs?" }
    public static let removeManyKeeps = "Their own transcripts stay on the computer."
}

/// The folder question asked before an agent first starts in a folder, and
/// Settings › Folders, where its answers are kept.
public enum FolderWords {
    public static let title = "Do you trust the authors of the files in this folder?"
    public static func body(_ computer: String) -> String {
        "If you do, the agent loads the folder’s own scripts, tools and instructions, and its scripts can run on "
            + "\(computer) without asking you. If you don’t, the agent starts with nothing from the folder."
    }
    public static let trust = "Yes, I trust the authors"
    public static let trustDetail = "Load the folder’s scripts, tools and instructions"
    public static let distrust = "No, I don’t trust the authors"
    public static let distrustDetail = "Start with nothing from the folder"
    public static let footnote = "Asked once for this folder. You can change it in Settings › Folders."
    public static func then(_ agent: String, _ folder: String) -> String { "Then \(agent) starts in \(folder)." }

    public static let paneTitle = "Folders"
    public static let paneIntro =
        "Agents load the scripts, tools and instructions of folders you trust. In other folders they start with nothing from the folder."
    public static let trusted = "Trusted"
    public static let notTrusted = "Not trusted"
    public static let forget = "Forget"
    public static let forgetHelp = "Forgetting a folder asks again the next time an agent starts there."
    public static let empty = "No folders yet. CodeConnect asks the first time an agent starts in a folder."
    public static func reading(_ computer: String) -> String { "Reading \(computer)’s folders…" }
    public static func unreachable(_ computer: String) -> String {
        "\(computer)’s CodeConnect isn’t answering, so its folders can’t be shown."
    }
}

/// A run's own screen: its reply box, its turn control and what its
/// conversation says about itself.
public enum SessionWords {
    public static func replyPrompt(_ agent: String) -> String { "Reply to \(agent)…" }
    public static let interrupted = "Interrupted"
    public static let interruptedSpoken = "Interrupted — this reply was cut short"
    public static let loadingEarlier = "Loading earlier history…"
    public static let stopThisTurn = "Stop this turn"
    public static let stopping = "Stopping…"
    public static let stopHelp = "Stop the turn this run is running"
    public static let tryAgainInAMoment = "Try again in a moment."
    /// A tool call or its result the computer cut whole before sending it.
    public static var cutShort: String {
        "Cut short by \(HostDevice.current.computer("your computer")). This is not the whole output."
    }
    public static let nothingLiveYet = "Nothing live has arrived for this run yet."
}

/// A run's settings, as both apps name them.
public enum RunSettingWords {
    public static let permissionMode = "Permission mode"
    public static let compactOrClear = "Compact or clear the context"
}

/// An approval card: who asks for what, its answers, and what a Codex edit
/// leaves off the card.
public enum CardWords {
    public static func asks(_ agent: String, _ tool: String) -> String { "\(agent) asks to use \(tool)" }
    public static let cantBeVerified = "Can’t be verified"
    public static let allow = "Allow"
    public static let allowForSession = "Allow for this session"
    public static let deny = "Deny"
    public static let denyAndSend = "Deny and send"
    public static func denyPrompt(_ agent: String) -> String { "What should \(agent) do instead?" }
    public static func denyTells(_ agent: String) -> String { "This call does not run, and \(agent) is told you said no." }
    public static func denyWithReasonTells(_ agent: String) -> String {
        "Denies the call, and \(agent) gets your reason with the denial."
    }
    public static func allowingGrants(_ rule: String) -> String { "Allowing for this session grants \(rule)" }
    public static let chooseOne = "Choose one"
    public static func showMoreFiles(_ count: Int) -> String { "Show \(count) more file\(count == 1 ? "" : "s")" }
    public static func linesNotDrawn(_ count: Int) -> String {
        count == 1 ? "1 more line of this patch is not drawn here." : "\(count) more lines of this patch are not drawn here."
    }
    public static func filesNotShown(_ count: Int) -> String {
        count == 1 ? "1 more file would change and is not shown here."
            : "\(count) more files would change and are not shown here."
    }
    public static let approvesThoseToo = "Approving this approves those too. Review them at the computer."
}

/// A question card's answers, as both apps word them.
public enum QuestionWords {
    public static let other = "Other"
    public static let typeYourAnswer = "Type your answer"
    public static let notes = "Notes"
    public static func notesPrompt(_ agent: String) -> String { "Optional notes for \(agent)" }
    public static let decline = "Decline"
    public static let submit = "Submit"
    public static func submitNeeds(questions: Int) -> String {
        questions == 1 ? "Choose an answer to submit." : "Answer every question to submit."
    }
}
