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

/// An ask before an act that cannot be taken back, in the words both apps show: its title naming what it acts on, one
/// line for each thing the act does (a mark and plain words, never a path), one line that reassures or says until when,
/// and its two answers, the safe one first.
public struct AskWords: Sendable, Hashable {
    public struct Item: Sendable, Hashable {
        /// The line's mark, an SF Symbol's name.
        public let mark: String
        public let words: String
    }

    public let title: String
    public let items: [Item]
    public let note: String
    /// The safe answer, which Escape also gives.
    public let cancel: String
    /// The answer that acts, drawn as destructive.
    public let confirm: String

    /// The ask as one paragraph, for a card that draws a sentence (the iPhone's): each line, then the note.
    public var message: String {
        (items.map { "\($0.words)." } + [note]).joined(separator: " ")
    }
}

/// The fleet list: its title, the way to start a run, the empty states and the
/// questions asked before a run ends or goes.
public enum FleetWords {
    public static let projects = "Projects"
    public static let newSession = "New session"
    public static let startAnAgent = "Start an agent"
    /// Under the launch's mark once it has waited a second for its computers (`LaunchMark`).
    public static func connecting(to computers: [String]) -> String {
        computers.count == 1 ? "Connecting to \(computers[0])…" : "Connecting to your computers…"
    }
    public static let agentsAppearHere = "Agents you start with CodeConnect appear here."
    /// The Mac's run list with nothing in it: its title, then where runs come from.
    public static let noRunsYet = "No sessions yet"
    public static let runsComeFrom = "Start one here or from your iPhone. Sessions started at a computer appear on their own."
    public static let noComputersYet = "No computers yet"

    /// Ending run `name` (its whole `RunLabel.inline`, which tells it from another run in the same project), whose agent
    /// is `agent`: it stops, goes where `then` says (the iPhone's Ended; on the Mac, `joinsEarlier`), and its folder is
    /// left as it is.
    public static func endAsk(
        _ name: String, agent: String, then: String = "The session moves to \(FleetStatus.ended.label)"
    ) -> AskWords {
        AskWords(
            title: "End “\(name)”?",
            items: [
                .init(mark: "stop", words: "\(agent) stops"),
                .init(mark: "arrow.right", words: then),
            ],
            note: "Its folder stays as it is.", cancel: "Cancel", confirm: "End Session")
    }

    /// Where an ended session goes on the Mac, whose list has no Ended section: under its project's "N earlier ›".
    public static let joinsEarlier = "It joins its project’s earlier sessions"

    /// Archiving run `name` (artboard n-11): it removes the worktree, deletes the local branch and ends `agent`’s
    /// session, and says first that files git ignores there go too, so a person keeping secrets in it is told before
    /// pressing.
    public static func archiveAsk(_ name: String, agent: String) -> AskWords {
        AskWords(
            title: "Archive “\(name)”?",
            items: [
                .init(mark: "folder", words: "Removes the worktree"),
                .init(mark: "arrow.triangle.branch", words: "Deletes the local branch"),
                .init(mark: "terminal", words: "Ends the \(agent) session"),
            ],
            note: "Files git ignores in it, like .env, are deleted too.", cancel: "Cancel", confirm: "Archive")
    }
    /// A project's menu's way to archive its ended sessions in worktrees of their own, `count` of them (artboard 48).
    public static func archiveEnded(_ count: Int) -> String {
        count == 1 ? "Archive 1 Ended Session…" : "Archive \(count) Ended Sessions…"
    }

    /// Archiving `count` of `project`'s ended sessions, on `computers` (by name), one by one as Archive Session does;
    /// the note names each that stays (`staying`) and why.
    public static func archiveEndedAsk(project: String, count: Int, computers: [String], staying: [String]) -> AskWords {
        let worktrees = count == 1 ? "1 worktree is" : "\(count) worktrees are"
        let from = computers.count == 1 ? computers[0] : "their computers"
        return AskWords(
            title: "Archive \(count) ended \(count == 1 ? "session" : "sessions") in \(project)?",
            items: [
                .init(mark: "archivebox", words: "They leave the list; Search still finds them"),
                .init(mark: "folder", words: "\(worktrees) removed from \(from)"),
                .init(mark: "arrow.triangle.branch", words: "Their branches are deleted; their commits are already on the base"),
            ],
            note: staying.joined(separator: " "), cancel: "Cancel", confirm: "Archive \(count)")
    }

    /// When none of them can go: the notice's title; its message is what stays, and why.
    public static let archiveEndedNone = "Nothing to archive"

    /// Why one of them stays.
    public static func staysUncommitted(_ title: String) -> String { "“\(title)” has uncommitted changes, so it stays." }
    public static func staysOffBase(_ title: String) -> String { "“\(title)” has commits not on its base, so it stays." }
    /// One going that was resumed between the card and its turn: not archived.
    public static let archiveSkippedResumed = "It was resumed, so it stays."
    public static func staysNoBase(_ title: String) -> String { "“\(title)” has no base branch known, so it stays." }
    public static func staysUnread(_ title: String) -> String { "“\(title)”’s repository couldn’t be read, so it stays." }

    /// An archived run's conversation ends with this (its `archived` notice).
    public static let archived = "Archived · worktree and branch removed"

    public static func removeTitle(_ project: String) -> String { "Remove \(project)?" }
    public static func removeKeeps(_ agent: String) -> String { "\(agent)’s own transcript stays on the computer." }
    public static func removeManyTitle(_ count: Int) -> String { "Remove \(count) sessions?" }
    public static let removeManyKeeps = "Their own transcripts stay on the computer."

    /// The notices after an end or a removal that did not happen: what happened, then why, in a sentence.
    public static let endFailedTitle = "Couldn’t end the session"
    public static let removeFailedTitle = "Couldn’t remove the session"
    /// An answer this build does not know: refused, with nothing quoted.
    public static var refusedPlainly: String { "\(HostDevice.current.computer("The computer")) refused." }
    /// Why a removal did not happen, for each answer ccd gives, or nil when it did (or the run was already gone).
    public static func removalRefused(_ result: DeleteSessionResult?) -> String? {
        let computer = HostDevice.current.computer("The computer")
        switch result {
        case .deleted, .notFound: return nil
        case .stillRunning: return "It’s still running. End it first, then remove it."
        case .notExited: return "\(computer) doesn’t know whether it has stopped, so it keeps the session."
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
    /// The Mac's card above the start form, which names the folder (screen 35).
    public static func question(_ folder: String) -> String { "Do you trust the authors of the files in \(folder)?" }
    public static func body(_ computer: String) -> String {
        "If you do, the agent loads the folder’s own scripts, tools and instructions, and its scripts can run on "
            + "\(computer) without asking you. If you don’t, the agent starts with nothing from the folder."
    }
    public static let trust = "Yes, I trust the authors"
    public static let trustDetail = "Load the folder’s scripts, tools and instructions"
    public static let distrust = "No, I don’t trust the authors"
    public static let distrustDetail = "Start with nothing from the folder"
    public static let footnote = "Asked once for this folder. You can change it in Settings › Folders."

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
    /// The way back to the conversation's end for a reader who left it: how many rows arrived since
    /// (`TimelineEnd.rowsAfter`), or, with none, "Latest".
    public static func latest(new count: Int) -> String { count > 0 ? "\(count) new" : "Latest" }
    public static let jumpToLatest = "Jump to the latest event"
    public static let stopThisTurn = "Stop this turn"
    public static let stopping = "Stopping…"
    public static let stopHelp = "Stop the turn this session is running"
    public static let tryAgainInAMoment = "Try again in a moment."
    /// A tool call or its result the computer cut whole before sending it.
    public static var cutShort: String {
        "Cut short by \(HostDevice.current.computer("your computer")). This is not the whole output."
    }
    public static let nothingLiveYet = "Nothing live has arrived for this session yet."
    /// In a run's place while a catch-up holds it: the computer's list names the run and its conversation has not
    /// arrived yet.
    public static func catchingUp(with computer: String?) -> String {
        "Catching up with \(computer ?? "your computer")…"
    }
    public static let catchingUpMessage = "This session’s conversation appears here once it has arrived."
    /// In the reply box's place on a run that takes no reply (`SessionSummary.takesNoReply`).
    public static let runEnded = "This session has ended"
    /// The conversation's end of a run the person ended (End Session), and, in a worktree of its own, that its worktree
    /// and branch are kept; else `runEnded`.
    public static func ended(byPerson: Bool, inWorktree: Bool) -> String {
        guard byPerson else { return runEnded }
        return inWorktree ? "You ended this session · its worktree and branch are kept" : "You ended this session"
    }
    /// A conversation with nothing in it: "yet" only while the run can still record something, not on one that has
    /// ended and takes no reply (`SessionSummary.takesNoReply`).
    public static func nothingRecorded(endedRun: Bool) -> String {
        endedRun ? "Nothing recorded" : "Nothing recorded yet"
    }
    /// Under `nothingRecorded`, while the run can still record something: not on one that takes no reply.
    public static let eventsAppearHere = "Events appear here as the agent works."
    /// Over an ended run's `SessionSummary.resumeCommand`.
    public static let pickUpAtComputer = "Pick this conversation up at the computer"
    /// The reply line while the run's computer is offline: why a reply cannot go, and when it can.
    public static func computerOffline(_ computer: String) -> String {
        "\(computer) is offline. You can reply once it’s back."
    }
}

/// A run's settings, as both apps name them.
public enum RunSettingWords {
    public static let permissionMode = "Permission mode"
    public static let compactOrClear = "Compact or clear the context"
    /// An Antigravity run's "…": agy has no clear.
    public static let compactOnly = "Compact the context"
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
    /// Beside the Mac's Decline and Submit: how far the answers have come.
    public static func answered(_ count: Int, of questions: Int) -> String { "\(count) of \(questions) answered" }
}

/// A run's Changes: when they were read, how they are asked for again, and
/// what is said when there are none or they could not be read.
public enum ChangesWords {
    public static func checked(since date: Date?, now: Date) -> String { "Checked \(Format.ago(since: date, now: now))" }
    /// While the run's computer is offline: what the panel says, over the diff it last read, if any, and its age.
    public static func cantReach(_ computer: String) -> String { "Can’t reach \(computer)" }
    public static func lastSaid(since date: Date?, now: Date) -> String {
        "Showing what it last said, \(Format.ago(since: date, now: now))"
    }
    public static let checkAgain = "Check again"
    public static let reading = "Reading this session’s changes on its computer…"
    public static let stillReading = "Still reading this session’s changes…"
    public static let couldNotRead = "Your computer could not read the changes"
    public static let itsReason = "Its reason, word for word:"
    public static let noUncommitted = "No uncommitted changes"
    /// A session whose diff is against its base (`RunGit.fromBase`) and is empty: nothing, committed or not, since
    /// `base`.
    public static func noChanges(from base: String) -> String { "No changes from \(base)" }
    public static let clean = "This session’s folder was clean when it was read."
    public static let noDiff = "No diff to show"
    public static let computersReason = "Your computer’s reason, word for word:"
    public static func files(_ count: Int) -> String { count == 1 ? "1 file" : "\(count) files" }
    /// A commit made from the panel, as the run's conversation shows it (its `committed` notice): how many files and
    /// the commit's short sha.
    public static func committed(files count: Int, sha: String) -> String {
        "You committed \(files(count)) · \(sha.prefix(7))"
    }
    /// The Changes panel's line under its state while it can commit, or why it cannot yet.
    public static let commitReady = "The session has stopped, so you can commit what it left"
    public static let commitWaitsWorking = "Agent is working · actions wait until it stops"
    public static let commitWaitsCard = "Waiting on you · actions wait until the agent stops"
    public static let commitWaitsMerge = "A merge has unresolved files · resolve them to commit"
    public static let commitWaitsBranch = "No branch is checked out · check one out to commit"
    /// Where Create PR opens its pull request: the base branch, when the computer knows it, on the repository it opens
    /// it in (a fork's parent when there is one).
    public static func pullRequestTarget(repo: String, base: String?) -> String {
        base.map { "Pushes the branch, then opens a PR into \($0) on \(repo)" }
            ?? "Pushes the branch, then opens a PR into \(repo)"
    }
    /// Why the computer did not do `step` (commit, open a pull request, merge it, archive the run), by its code
    /// (`git_refusal`), with git's or gh's own words after where it gave them.
    public static func actionRefused(code: String, detail: String?, step: GitStep) -> String {
        let reason: String
        switch (code, step) {
        case ("busy", .pullRequest): reason = "A session in this folder is working or waiting for you, so nothing was pushed."
        case ("unmerged", .pullRequest): reason = "A merge has unresolved files, so nothing was pushed."
        case ("git_failed", .pullRequest): reason = "Git could not push the branch, so no PR was opened."
        case ("timeout", .pullRequest): reason = "Git took too long; the branch may or may not have been pushed, and no PR was opened."
        case ("busy", .merge): reason = "A session in this folder is working or waiting for you, so nothing was merged."
        case ("head_moved", .merge):
            reason = "The pull request or its commit changed since the panel showed it, so nothing was merged."
        case ("method_not_allowed", .merge):
            reason = "This repository doesn’t allow that merge method, so nothing was merged."
        case ("no_pull_request", .merge): reason = "This branch has no open pull request, so nothing was merged."
        case ("merge_failed", .merge): reason = "GitHub did not merge it."
        case ("timeout", .merge): reason = "GitHub took too long; the pull request may or may not have been merged."
        case ("gh_missing", .merge): reason = "This computer has no gh, so nothing was merged."
        case ("gh_signed_out", .merge): reason = "This computer’s gh isn’t signed in to GitHub, so nothing was merged."
        case ("github_unavailable", .merge): reason = "GitHub could not be reached, so nothing was merged."
        case ("busy", .archive): reason = "A session in this folder is working or waiting for you, so nothing was archived."
        case ("head_moved", .archive): reason = "The branch or its commit changed since the panel showed it, so nothing was archived."
        case ("on_base_branch", .archive): reason = "The branch checked out is the base branch itself, so nothing was archived."
        case ("base_unknown", .archive): reason = "No base branch is known for this repository, so nothing was archived."
        case ("github_unavailable", .archive): reason = "GitHub could not be reached to check the merge, so nothing was archived."
        case ("timeout", .archive): reason = "Git took too long, so the session was not archived."
        // Its detail says how far it got ("ended, worktree removed; <branch> kept: …"), so it leads.
        case ("git_failed", .archive):
            return detail.map { "Git did not finish archiving: \($0)" } ?? "Git did not archive the session."
        default: return actionRefused(code: code, detail: detail)
        }
        guard let detail, !detail.isEmpty else { return reason }
        return "\(reason) \(detail)"
    }

    /// Why the computer did not act, by its code, in the words every step shares.
    static func actionRefused(code: String, detail: String?) -> String {
        let reason: String
        switch code {
        case "busy": reason = "A session in this folder is working or waiting for you, so nothing was committed."
        case "unmerged": reason = "A merge has unresolved files, so nothing was committed."
        case "nothing_to_commit": reason = "Nothing has changed since the last commit."
        case "detached": reason = "No branch is checked out, so nothing was done."
        case "not_a_repository": reason = "This session’s folder is not in a git repository."
        case "unknown_session": reason = "This session is no longer on its computer."
        case "bad_request": reason = "The computer could not take this request, so nothing was done."
        case "git_failed": reason = "Git did not commit."
        case "timeout": reason = "Git took too long, so nothing was committed."
        case "gh_missing": reason = "This computer has no gh, so no PR was opened."
        case "gh_signed_out": reason = "This computer’s gh isn’t signed in to GitHub, so no PR was opened."
        case "github_unavailable": reason = "GitHub could not be reached, so nothing was pushed."
        case "no_github_remote": reason = "No remote of this repository is on GitHub, so no PR was opened."
        case "on_base_branch": reason = "The branch checked out is the base branch itself, so no PR was opened."
        case "base_unknown": reason = "No base branch is known for this repository, so nothing was pushed."
        case "nothing_to_propose": reason = "The branch has no commits its base lacks, so no PR was opened."
        case "create_failed": reason = "GitHub did not open the PR."
        case "main_worktree": reason = "This session is in the repository’s main folder, not a worktree, so nothing was archived."
        case "shared_worktree":
            reason = "Another session works in this worktree, or another worktree has its branch, so nothing was archived."
        case "has_changes":
            reason = "The worktree has changes git may not show: uncommitted, untracked, or in files git is told not to check, so nothing was archived."
        case "branch_has_commits": reason = "The branch has commits that are not merged, so nothing was archived."
        case "locked": reason = "Git has this worktree locked, so nothing was archived."
        case "still_running": reason = "The session did not end, so nothing was archived."
        default: return actionUnknown
        }
        guard let detail, !detail.isEmpty else { return reason }
        return "\(reason) \(detail)"
    }
    /// Why Create PR waits, before it is pressed, for what the computer would refuse it for.
    public static let pullRequestWaitsBase = "This is the base branch · a PR comes from another branch"
    public static let pullRequestWaitsSignIn = "gh isn’t signed in to GitHub on this computer · sign in to open a PR"
    public static let pullRequestWaitsGh = "This computer has no gh · install it to open a PR"
    /// An action whose outcome is not known: the panel's repository, read again, shows what git and GitHub have now.
    public static let actionUnknown = "It is not known whether it was done; the panel below shows what git and GitHub have now."
    /// A Changes panel's state line from its run's repository: an unresolved merge, the files changed (at least so
    /// many when the computer could not count them all), and the commits not on the base, pushed or not, or nothing
    /// committed yet. Only what the computer read; nothing from an agent's words.
    public static func state(_ git: RunGit, conflicts: RunConflicts? = nil) -> String {
        var parts: [String] = []
        let conflicted = conflicting(git, conflicts)
        // A branch's pull request speaks for its commits and its push.
        if let pr = git.pullRequest {
            switch pr.state {
            case .open: parts.append("PR #\(pr.number) · \(openLead(pr, conflictsWith: conflicted))")
            case .merged: parts.append("PR #\(pr.number) · Merged into \(pr.base)")
            case .closed: parts.append("PR #\(pr.number) · Closed")
            case .unknown: parts.append("PR #\(pr.number)")
            }
        }
        if git.pullRequest == nil, let base = conflicted { parts.append("Conflicts with \(base)") }
        if git.unmerged { parts.append("Merge conflicts") }
        let changed = Int(git.changed.files)
        if changed > 0 {
            parts.append(git.changed.complete ? "\(files(changed)) changed" : "At least \(files(changed)) changed")
        } else if !git.changed.complete {
            parts.append("Changes not all counted")
        }
        if let pr = git.pullRequest {
            // Commits made since an open pull request was opened, not yet on its branch (a merged one's branch is
            // often deleted, which is no unpushed work).
            if pr.state == .open, !git.pushed, git.branch != nil { parts.append("not pushed") }
            return parts.joined(separator: " · ")
        }
        if let ahead = git.ahead, ahead > 0 {
            parts.append(ahead == 1 ? "1 commit" : "\(ahead) commits")
            if git.branch != nil { parts.append(git.pushed ? "pushed" : "not pushed") }
        } else if git.ahead == 0 || git.headOID == nil, changed > 0 {
            parts.append("nothing committed")
        }
        // Clean, with no base to count commits against: nothing more is known than that nothing is uncommitted.
        if parts.isEmpty { return git.ahead == nil && git.headOID != nil ? "No uncommitted changes" : "No changes" }
        return parts.joined(separator: " · ")
    }
    /// What leads an open pull request's state line: queued to merge, else what its checks say, else its merge state
    /// (artboards 07, 10), else Open.
    private static func openLead(_ pr: RunGit.PullRequest, conflictsWith base: String?) -> String {
        if pr.mergeQueued == true { return "Queued to merge" }
        return checksLead(pr) ?? base.map { "Conflicts with \($0)" } ?? mergeLead(pr) ?? "Open"
    }
    /// The base a run's commit conflicts with, when the computer compared that very commit (artboard 17); nil when it
    /// merges cleanly, was not compared, or was compared at another commit.
    private static func conflicting(_ git: RunGit, _ conflicts: RunConflicts?) -> String? {
        guard let conflicts, !conflicts.clean, conflicts.headOID == git.headOID else { return nil }
        return conflicts.base.name
    }
    /// Under a state line led by conflicts (artboard 17): the files they are in, by name, three at most.
    public static func conflictsNote(_ conflicts: RunConflicts) -> String? {
        guard !conflicts.clean else { return nil }
        guard !conflicts.files.isEmpty else { return conflictsUnnamed }
        let names = conflicts.files.map { ($0 as NSString).lastPathComponent.displaySafe }.sorted()
        let count = conflicts.filesComplete ? files(names.count) : "at least \(files(names.count))"
        let shown = names.prefix(3).joined(separator: ", ")
        return "In \(count): \(names.count > 3 ? "\(shown) and \(names.count - 3) more" : shown)"
    }
    /// The two sides of a conflicting hunk: the branch checked out, then the base by its name.
    public static let conflictThisBranch = "this branch"
    /// A conflicting file with no lines to show.
    public static let conflictNoLines = "No lines to show: binary, renamed, or deleted on one side."
    /// Under a file's hunks when it has more than were read.
    public static let conflictMore = "More conflicts in this file than are shown."
    /// A conflict git names no file for.
    public static let conflictsUnnamed = "Git names no file for them"
    /// A conflicting file's heading over its hunks (artboard 17).
    public static func conflictTitle(_ path: String) -> String {
        "Both changed in \((path as NSString).lastPathComponent.displaySafe)"
    }
    /// An open pull request's merge state as GitHub reports it, in words; nil while GitHub works it out.
    private static func mergeLead(_ pr: RunGit.PullRequest) -> String? {
        switch pr.mergeState {
        case .clean, .unstable, .hasHooks: "Ready to merge"
        case .behind: "Behind \(pr.base)"
        case .dirty: "Conflicts with \(pr.base)"
        case .draft: "Draft"
        case .blocked: "Blocked by \(pr.base)’s rules"
        case .unknown, nil: nil
        }
    }
    /// Whether the merge button may merge an open pull request: GitHub would merge it as it stands (a check it doesn't
    /// require may fail), and no queue holds it already.
    public static func mergeable(_ pr: RunGit.PullRequest, conflicts: RunConflicts? = nil) -> Bool {
        guard pr.state == .open, pr.mergeQueued != true else { return false }
        // The computer found this commit conflicting with its base: GitHub would refuse it too.
        if let conflicts, !conflicts.clean, conflicts.headOID == pr.headOID { return false }
        return [.clean, .unstable, .hasHooks].contains(pr.mergeState)
    }
    /// A merge method as the merge button and its menu name it (artboard 10).
    public static func mergeMethod(_ method: MergeMethod) -> String {
        switch method {
        case .squash: "Squash and merge"
        case .merge: "Create a merge commit"
        case .rebase: "Rebase and merge"
        }
    }
    /// Whether Archive Session is offered (artboard 11), by the computer's own gate as the run's repository shows it: a run
    /// in a worktree of its own, on a branch other than its base, nothing uncommitted, its commits on the base already
    /// or merged by its pull request at the commit checked out. What the panel can't see (a worktree another run
    /// shares, a lock) the computer refuses on the press.
    public static func archivable(_ git: RunGit, inWorktree: Bool) -> Bool {
        guard inWorktree, let branch = git.branch, let head = git.headOID, let base = git.base?.name, branch != base,
            git.changed.files == 0, git.changed.complete
        else { return false }
        if git.ahead == 0 { return true }
        guard let pr = git.pullRequest else { return false }
        return pr.state == .merged && pr.headOID == head
    }
    /// The merge button's chevron, as a screen reader names it.
    public static let mergeMethods = "Merge methods"
    /// A merge method the repository turns off, as its menu row says it.
    public static func mergeMethodOff(_ method: MergeMethod) -> String { "\(mergeMethod(method)) (off in this repo)" }
    /// The method the merge button starts on: squash where the repository allows it, else the first it allows.
    public static func defaultMergeMethod(_ allowed: [MergeMethod]) -> MergeMethod? {
        allowed.contains(.squash) ? .squash : allowed.first
    }
    /// What an open pull request's checks lead its state line with: a failure, else what still runs; nil when every
    /// check passed or none was read, and the line says Open.
    private static func checksLead(_ pr: RunGit.PullRequest) -> String? {
        guard let checks = pr.checks else { return nil }
        let failed = checks.filter { $0.state == .fail }.count
        if failed > 0 { return failed == 1 ? "1 check failed" : "\(failed) checks failed" }
        let running = checks.filter { $0.state == .pending }.count
        if running > 0 { return running == 1 ? "1 check running" : "\(running) checks running" }
        return nil
    }
    /// The line under an open pull request's state (artboard 07): how many of its checks passed, a skipped one with
    /// them, then which failed and how many still run. Nil when it has none read, or is not open.
    public static func checksNote(_ git: RunGit, conflicts: RunConflicts? = nil) -> String? {
        guard let pr = git.pullRequest, pr.state == .open, let checks = pr.checks, !checks.isEmpty else { return nil }
        var parts = ["\(passed(checks)) of \(checks.count) checks passed"]
        let failed = checks.filter { $0.state == .fail }.map(\.name.displaySafe)
        if !failed.isEmpty { parts.append("\(failed.joined(separator: ", ")) failed") }
        let running = checks.filter { $0.state == .pending }.count
        if running > 0 { parts.append("\(running) running") }
        // A comparison of this very commit that merges cleanly says so (artboard 10), unless a check failed (07).
        if failed.isEmpty, let conflicts, conflicts.clean, conflicts.headOID == git.headOID {
            parts.append("no conflicts")
        }
        return parts.joined(separator: " · ")
    }
    /// The Checks tab's label: how many of the checks passed, of how many.
    public static func checksTab(_ checks: [RunGit.PullRequest.Check]) -> String {
        "Checks \(passed(checks))/\(checks.count)"
    }
    private static func passed(_ checks: [RunGit.PullRequest.Check]) -> Int {
        checks.filter { $0.state == .pass || $0.state == .skipped }.count
    }
    /// Where a running check's duration goes.
    public static let checkRunning = "running"
    /// Under the checks when the computer could not read them all (more than it reads, or the head moved meanwhile).
    public static let checksNotAll = "Not every check is shown"
    /// How long a check ran (artboard 07): "22s", "4m 12s", "5m 03s", "1h 02m".
    public static func checkDuration(_ seconds: Int) -> String {
        let seconds = max(0, seconds)
        if seconds < 60 { return "\(seconds)s" }
        if seconds < 3600 { return "\(seconds / 60)m \(String(format: "%02d", seconds % 60))s" }
        return "\(seconds / 3600)h \(String(format: "%02d", seconds % 3600 / 60))m"
    }
    /// A check's duration from its start to its end, or to `now` while it runs; nil for one with no start (a status).
    public static func checkDuration(_ check: RunGit.PullRequest.Check, now: Date) -> String? {
        guard let start = check.startedAt.flatMap(ISO8601.parse) else { return nil }
        let end = check.completedAt.flatMap(ISO8601.parse) ?? now
        return checkDuration(Int(end.timeIntervalSince(start)))
    }
    public static func showMoreLines(_ count: Int) -> String { "Show \(count) more line\(count == 1 ? "" : "s")" }
    public static func unchangedLines(_ count: Int) -> String {
        count == 1 ? "1 unchanged line" : "\(count) unchanged lines"
    }
    /// A folded stretch of a diff, where its code starts: a click or tap opens it.
    public static func foldedLines(_ count: Int) -> String { "⋯ \(unchangedLines(count))" }
    /// The folded stretch as a screen reader says it, and what opening it does.
    public static func foldedLinesSpoken(_ count: Int) -> String { "\(unchangedLines(count)) hidden" }
    public static let foldedLinesHint = "Shows them"
    /// What a comment on a hunk asks for, in its empty field: the run's agent by name.
    public static func commentPrompt(_ agent: String) -> String { "Comment to \(agent) about these lines" }
    /// Under a hunk for a few seconds once its comment reached the agent, on the Mac, where the field closes.
    public static let commentSent = "Sent"
    /// Which file and lines a comment on a hunk is about, so the agent reads it against the code: the lowest and
    /// highest line numbers in the hunk, new ones where a line has one.
    public static func commentAnchor(file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk) -> String {
        guard let lines = commentLines(hunk) else { return "In `\(file.displayPath)`" }
        return "In `\(file.displayPath)` \(lines)"
    }
    /// The same place as the comment field shows it, over the field: the path, then its lines.
    public static func commentPlace(file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk) -> String {
        guard let lines = commentLines(hunk) else { return file.displayPath }
        return "\(file.displayPath) · \(lines)"
    }
    /// "line 7" or "lines 12–14": the lowest and highest line numbers in the hunk, new ones where a line has one; nil
    /// when it numbers none.
    private static func commentLines(_ hunk: UnifiedDiff.Hunk) -> String? {
        let numbers = hunk.lines.compactMap { $0.newNumber ?? $0.oldNumber }
        guard let first = numbers.min(), let last = numbers.max() else { return nil }
        return first == last ? "line \(first)" : "lines \(first)–\(last)"
    }
    /// A comment on a hunk as it is sent to the agent: its anchor, the words, then the hunk itself, so the agent reads
    /// the comment against the lines it is about. At most `Wire.maxComposeBytes`: only the hunk is cut.
    public static func comment(_ text: String, file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk) -> String {
        fitted(
            head: "\(commentAnchor(file: file, hunk: hunk)): \(text)\n\n",
            excerpts: [Excerpt(label: "", text: hunk.text, language: "diff")], tail: "")
    }

    /// A failed check sent to the agent: its name, its first failure line and its log, then the person's words, if any.
    public static func sendCheck(_ words: String?, check: RunGit.PullRequest.Check) -> String {
        var head = "The check `\(check.name)` failed"
        if let line = check.failureLine, !line.isEmpty { head += ": \(line)" }
        if let url = check.url, !url.isEmpty { head += "\nIts log: \(url)" }
        return fitted(head: head, excerpts: [], tail: following(words))
    }

    /// A file's conflict with the base sent to the agent: each conflicting stretch, this branch's lines then the
    /// base's, then the person's words, if any. At most `Wire.maxComposeBytes`: only the stretches are cut.
    public static func sendConflict(_ words: String?, path: String, base: String, hunks: [ConflictHunk]) -> String {
        fitted(
            head: "`\(path)` conflicts with \(base):\n\n",
            excerpts: hunks.flatMap { hunk in
                [
                    Excerpt(label: "This branch:\n", text: hunk.ours, language: ""),
                    Excerpt(label: "\(base):\n", text: hunk.theirs, language: ""),
                ]
            },
            tail: following(words))
    }

    /// The person's words after an excerpt, a blank line apart; nothing when there are none.
    private static func following(_ words: String?) -> String {
        guard let words = words?.trimmingCharacters(in: .whitespacesAndNewlines), !words.isEmpty else { return "" }
        return "\n\n\(words)"
    }

    /// Lines sent as code: a label over its fenced block, the block's language.
    private struct Excerpt {
        let label: String
        let text: String
        let language: String
    }

    /// `head`, each excerpt under its label in a fenced block, then `tail`, in at most `Wire.maxComposeBytes` of UTF-8:
    /// `head` and `tail` whole, the excerpts cut at a character where the rest does not fit, ending "… (cut)". A fence is
    /// longer than any run of backticks in its text.
    private static func fitted(head: String, excerpts: [Excerpt], tail: String) -> String {
        let cut = "… (cut)"
        var body = ""
        let room = { Wire.maxComposeBytes - (head + body + tail).utf8.count }
        for (index, excerpt) in excerpts.enumerated() {
            let fence = String(repeating: "`", count: max(3, longestBackticks(excerpt.text) + 1))
            let open = (index == 0 ? "" : "\n") + excerpt.label + fence + excerpt.language + "\n"
            let close = (excerpt.text.hasSuffix("\n") ? "" : "\n") + fence
            if (open + excerpt.text + close).utf8.count <= room() {
                body += open + excerpt.text + close
                continue
            }
            // Cut here: what fits of this excerpt, its fence closed, and the mark.
            let suffix = "\n" + fence + "\n" + cut
            var kept = ""
            var left = room() - (open + suffix).utf8.count
            for character in excerpt.text {
                let size = String(character).utf8.count
                if size > left { break }
                kept.append(character)
                left -= size
            }
            if kept.isEmpty {
                let mark = (body.isEmpty ? "" : "\n") + cut
                if mark.utf8.count <= room() { body += mark }
            } else {
                body += open + kept + suffix
            }
            break
        }
        return head + body + tail
    }

    /// The longest run of backticks in `text`.
    private static func longestBackticks(_ text: String) -> Int {
        var longest = 0
        var run = 0
        for character in text {
            run = character == "`" ? run + 1 : 0
            longest = max(longest, run)
        }
        return longest
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

    // Where in its repository a run starts: the folder as it is, a worktree it has, or a new one.
    public static let whichBranch = "Which branch?"
    public static let branch = "Branch"
    public static func readingBranches(_ folder: String) -> String { "Reading \(folder)’s branches…" }
    public static let thisFolder = "This folder"
    public static let worktrees = "Worktrees"
    public static let newWorktree = "New worktree"
    public static let olderBranches = "Older branches aren’t listed."
    public static let newBranch = "New branch…"
    public static let newBranchDetail = "A worktree of its own, by name"
    /// The Mac's branch menu (screen 34): the folder's checkout, the default new worktree, and the branches to start one
    /// from, named by the first two and how many more.
    public static let ownCheckout = "The folder’s own checkout"
    public static let namedFromTask = "Branch named from the task"
    public static let fromABranch = "From a branch…"
    public static func branchList(_ branches: [String]) -> String {
        branches.count <= 2
            ? branches.joined(separator: ", ")
            : branches.prefix(2).joined(separator: ", ") + ", \(branches.count - 2) more"
    }
    public static let worktreeName = "Name"
    public static let worktreeNameExample = "fix-login"
    public static let worktreeNameRule = "Letters, digits, . _ and -, with / between parts. Up to 64."
    public static let worktreeNameBroken = "Letters, digits, . _ and - only, with / between parts."
    public static let notNamedYet = "Not named yet"
    public static let from = "From"
    public static let changes = "Changes"
    public static let changesStay = "Uncommitted changes stay where they are"
    /// Start waits for the first message: no start goes without one (decision 169).
    public static let writeTheFirstMessage = "Write the first message."
    public static let nameTheWorktree = "Name the worktree."
    public static let nameBreaksTheRule = "The name breaks the rule above."

    // Browsing a computer's folders for one to start in. The Mac's buttons read these in title style.
    public static let browseFolders = "Browse folders"
    public static let showHiddenFolders = "Show hidden folders"
    public static let newFolderAction = "New folder…"
    public static let useThisFolder = "Use this folder"
    /// The card that names a new folder: its title, where it goes, the field's placeholder.
    public static let newFolder = "New folder"
    public static func newFolderIn(_ path: String) -> String { "In \(path)" }
    public static let folderName = "Name"
    public static func couldNotMake(_ name: String) -> String { "Couldn’t make “\(name)”" }
    /// While the computer lists a folder; nil is its home folder, before the computer has named it.
    public static func readingFolder(_ path: String?, on computer: String) -> String {
        path.map { "Reading \($0)…" } ?? "Reading \(computer)’s home folder…"
    }
    /// What a listing cannot show by itself.
    public static let firstFolders = "The first 2,000 folders, in name order."
    public static func noFolders(showingHidden: Bool) -> String {
        showingHidden ? "No folders in here." : "No folders in here, hidden ones aside."
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
