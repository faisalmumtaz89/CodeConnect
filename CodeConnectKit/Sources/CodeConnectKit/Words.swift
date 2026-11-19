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
