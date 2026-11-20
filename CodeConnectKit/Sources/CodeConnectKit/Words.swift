import Foundation

extension String {
    /// The words as a Mac button reads them, in title style: each word starts with a capital except a short article,
    /// conjunction or preposition after the first. A word that already holds a capital ("iPhone") is kept as it is.
    public var titleStyle: String {
        let minor: Set<String> = ["a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "at", "by", "from"]
        return split(separator: " ", omittingEmptySubsequences: false).enumerated().map { index, word in
            let word = String(word)
            guard let first = word.first, first.isLowercase, index == 0 || !minor.contains(word) else { return word }
            return first.uppercased() + word.dropFirst()
        }.joined(separator: " ")
    }
}

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
    /// The list while its computers are still answering, or waited out: never "Start an agent" before then.
    public static let connecting = "Connecting to your computer…"
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

    /// The notices after an end or a removal that did not happen: what happened, then why, in a sentence.
    public static let endFailedTitle = "Couldn’t end the run"
    public static let removeFailedTitle = "Couldn’t remove the run"
    /// An answer this build does not know: refused, with nothing quoted.
    public static var refusedPlainly: String { "\(HostDevice.current.computer("The computer")) refused." }
    /// Why a removal did not happen, for each answer ccd gives, or nil when it did (or the run was already gone).
    public static func removalRefused(_ result: DeleteSessionResult?) -> String? {
        let computer = HostDevice.current.computer("The computer")
        switch result {
        case .deleted, .notFound: return nil
        case .stillRunning: return "It’s still running. End it first, then remove it."
        case .notExited: return "\(computer) doesn’t know whether it has stopped, so it keeps the run."
        case .failed: return "\(computer) couldn’t remove it."
        case .unknown: return refusedPlainly
        case .none: return "\(computer) didn’t answer."
        }
    }
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
    public static let interruptedSpoken = "Interrupted: this reply was cut short"
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
    /// In the reply box's place on a run that takes no reply (`SessionSummary.takesNoReply`).
    public static let runEnded = "This run has ended. Its conversation stays here to read."
}

/// A run's settings, as both apps name them.
public enum RunSettingWords {
    public static let permissionMode = "Permission mode"
    public static let compactOrClear = "Compact or clear the context"
}

/// An approval card: who asks for what, its answers, and what a Codex edit
/// leaves off the card.
public enum CardWords {
    /// The card's heading: a named tool is used ("Claude asks to use Bash"); a Codex request that is an act, not a
    /// tool's name, says the act. Any other kind keeps the named form.
    public static func asks(_ agent: String, _ tool: String) -> String {
        switch tool {
        case "command": "\(agent) asks to run a command"
        case "file change": "\(agent) asks to change files"
        default: "\(agent) asks to use \(tool)"
        }
    }
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
    public static func allowingGrants(_ rule: String) -> String { "\(DecisionCard.sessionGrantLead) \(rule)" }
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

/// A run's Changes: when they were read, how they are asked for again, and
/// what is said when there are none or they could not be read.
public enum ChangesWords {
    public static func checked(since date: Date?, now: Date) -> String { "Checked \(Format.ago(since: date, now: now))" }
    public static let checkAgain = "Check again"
    public static let reading = "Reading this run’s changes on its computer…"
    public static let stillReading = "Still reading this run’s changes…"
    public static let couldNotRead = "Your computer could not read the changes"
    public static let itsReason = "Its reason, word for word:"
    public static let noUncommitted = "No uncommitted changes"
    public static let clean = "This run’s folder was clean when it was read."
    public static let noDiff = "No diff to show"
    public static let computersReason = "Your computer’s reason, word for word:"
    public static func files(_ count: Int) -> String { count == 1 ? "1 file" : "\(count) files" }
    public static func showMoreLines(_ count: Int) -> String { "Show \(count) more line\(count == 1 ? "" : "s")" }
    public static func unchangedLines(_ count: Int) -> String {
        count == 1 ? "1 unchanged line" : "\(count) unchanged lines"
    }
}

/// Starting a run: a computer's recent folders, and what the start says while
/// it is under way.
public enum StartWords {
    public static func recentFolders(_ count: Int, lastUsed: Date?, now: Date) -> String {
        let used = lastUsed.map { " · last used \(Format.ago(since: $0, now: now))" } ?? ""
        return "\(count) recent folder\(count == 1 ? "" : "s")\(used)"
    }
    public static func starting(_ agent: String) -> String { "Starting \(agent)…" }
    public static func started(_ computer: String) -> String { "\(computer) started it" }
    public static func asking(_ computer: String) -> String { "Asking \(computer) to start it" }
    public static func waitingForFirstWords(_ agent: String) -> String { "Waiting for \(agent)’s first words · " }
    public static let stopRetrying = "Stop retrying"
    public static func noMoreSends(_ computer: String) -> String {
        "No more sends from this \(HostDevice.current.noun); \(computer) may already have started it."
    }
}

/// Adding a computer by its address and pairing code.
public enum PairingWords {
    public static let address = "Address"
    public static let code = "Code"
    public static let pairingCode = "Pairing code"
    public static let connect = "Connect"
    public static let connecting = "Connecting…"
    public static let codeWorksOnce = "Each code works once and expires after five minutes."

    /// The Mac's Add an iPhone while no iPhone could reach this Mac yet, one sentence for each thing the Tailscale
    /// note under it can say: with no Tailscale (or none said yet), …
    public static let iPhoneCantReach = "Your iPhone can’t reach this Mac until both are on one network, such as Tailscale."
    /// … with Tailscale on but not connected, …
    public static let iPhoneReachesOnceConnected = "Your iPhone can reach this Mac once Tailscale is connected."
    /// … and with Tailscale connected, while this Mac's CodeConnect moves onto it.
    public static let iPhoneReachesOnceMoved = "Your iPhone can reach this Mac once CodeConnect is on Tailscale."
}

/// A computer the owner's tailnet lists that has not joined this device's list, and why, as both apps say it.
public enum PeerWords {
    /// Its CodeConnect answered and would not let this device in: why, for each refusal a join can get.
    public static func refused(_ refusal: ConnectionRefusal?) -> String {
        switch refusal {
        case .notAdmitted: "Won’t let this \(HostDevice.current.name) in without a pairing code"
        case .differentVersion: "Runs a different version of CodeConnect"
        case .credential, .pairingCode, nil: "Didn’t let this \(HostDevice.current.name) in"
        }
    }
    public static let tryAgain = "Try again"
    /// Add a Computer's section of the computers his Tailscale account lists that he removed, each added by one
    /// action with no code.
    public static let onYourTailscale = "On your Tailscale account"
    /// Add a Computer's opening sentence, true beside that section: a computer on the account joins by itself
    /// unless he removed it, and a removed one is then listed below.
    public static func addIntro(removedListed: Bool) -> String {
        "Install CodeConnect on it and sign in to the same Tailscale account. "
            + (removedListed ? "It appears here by itself, unless you removed it: then it’s listed below." : "It appears here by itself.")
    }
    public static let add = "Add"
}
