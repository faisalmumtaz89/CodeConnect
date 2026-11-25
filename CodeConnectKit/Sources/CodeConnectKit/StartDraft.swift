import Foundation
import Observation

/// **One window's start of an agent**, held above the form that draws it: what is chosen on each computer, the first
/// message, and the start sent with what it asked. The form owns none of it, so leaving for a run and coming back, a
/// window closed and opened again, a computer switched and switched back, or a link dropped and returned all show the
/// same start. The answer to a sent start stays the computer's (`SessionStarter.starts`); the draft reads it.
///
/// A start the person withdraws (`withdraw`, with the computer's `stopRetrying`) is remembered with what it asked.
/// Start pressed again on exactly that ask sends it again under its own id (`StartContext.resume`): the computer answers
/// a start it already has as it did, so one ask never runs twice. Any change to the ask makes a new start, and the
/// withdrawn one is let go: if it begins after all, it is a run in the list like any other.
@MainActor
@Observable
public final class StartDraft {
    /// What is chosen on one computer.
    public struct Choices: Hashable, Sendable {
        public var folderPath: String?
        public var query = ""
        public var model: StartModelChoice?
        public var place = StartPlace.folder
        /// A new worktree's name while one is being named.
        public var newName: String?

        public init() {}
    }

    /// What a start asks: the computer, the folder, where in it, the agent and model, and the first message.
    public struct Ask: Hashable, Sendable {
        public let computerID: String
        public let folderPath: String
        public let place: StartPlace
        public let agent: StartAgent
        public let model: String?
        public let message: String
        /// The draft's files, by their tiles' ids: other files are another ask.
        public let files: [String]

        public init(
            computerID: String, folderPath: String, place: StartPlace, agent: StartAgent, model: String?, message: String,
            files: [String] = []
        ) {
            self.computerID = computerID
            self.folderPath = folderPath
            self.place = place
            self.agent = agent
            self.model = model
            self.message = message.trimmingCharacters(in: .whitespacesAndNewlines)
            self.files = files
        }
    }

    /// The start this draft sent, and whether the person withdrew it.
    public struct Sent: Hashable, Sendable {
        public let requestID: String
        public let ask: Ask
        public fileprivate(set) var withdrawn = false
    }

    /// Which of the form's pages the draft is on, from the sent start's answer.
    public enum Phase: Hashable, Sendable {
        case composing
        case sending
        case trust(path: String)
        case failed(StartFailure, detail: String)
        case started(sessionUID: String)
    }

    /// What Start does with an ask: send the withdrawn start again, or a new one.
    public enum Send: Hashable, Sendable {
        case resume(requestID: String)
        case new
    }

    /// The computer chosen, by id; nil leaves it to the form (the one used last).
    public var computerID: String?
    public var message = ""
    /// The files going with the first message, in order. They go to the computer with the start, and again with every
    /// send of it (`AppModel.startSession`): a start's files are spent once it is sent.
    public private(set) var files: [Attachment] = []
    public private(set) var sent: Sent?
    public private(set) var choices: [String: Choices] = [:]
    /// New Agent asked for the first message to have the keyboard, and the form has not given it yet.
    public private(set) var wantsKeyboard = false

    public init() {}

    /// Adds a file to the first message, refused on its name and size alone when over a limit (`read` is then never
    /// called).
    public func attach(name: String, size: Int, read: () throws -> Data) {
        var file = Attachment.taking(name: name, size: size, count: files.count, read: read)
        // Nothing is sent until Start: a file within the limits waits as one that arrived.
        if case .uploading = file.state { file.state = .arrived }
        files.append(file)
    }

    public func removeFile(_ id: String) { files.removeAll { $0.id == id } }

    /// Why the files hold the start back: a file over a limit, in its own words.
    public var filesBlockedReason: String? {
        for file in files { if case .refused(let reason) = file.state { return reason } }
        return nil
    }

    public func choices(on computer: String) -> Choices { choices[computer] ?? Choices() }

    /// A change to one computer's choices; the others are untouched.
    public func edit(on computer: String, _ change: (inout Choices) -> Void) {
        var each = choices(on: computer)
        change(&each)
        choices[computer] = each
    }

    /// A folder the person picks starts in the folder as it is, until they choose a place in it. The same folder again
    /// changes nothing.
    public func chooseFolder(_ path: String?, on computer: String) {
        guard choices(on: computer).folderPath != path else { return }
        edit(on: computer) {
            $0.folderPath = path
            $0.query = ""
            $0.place = .folder
            $0.newName = nil
        }
    }

    /// The page for the sent start's answer (`outcome`, the computer's for `sent`), given what the form asks now
    /// (`asking`) and whether the start's computer is still one of the app's (`computerKnown`). A start whose computer
    /// was removed is let go. A withdrawn start shows the form again while it has no answer, and after an answer that
    /// started nothing; it is let go the moment the form asks anything else, so it never takes the form over again.
    /// One that started after all while the form still asks exactly it is opened.
    public func phase(outcome: StartOutcome?, asking: Ask?, computerKnown: Bool) -> Phase {
        guard let sent, computerKnown else { return .composing }
        if sent.withdrawn, asking != sent.ask { return .composing }
        switch outcome {
        case .started(let uid, _)?: return .started(sessionUID: uid)
        case nil: return sent.withdrawn ? .composing : .sending
        case .needsTrust(let path)?: return sent.withdrawn ? .composing : .trust(path: path)
        case .failed(let reason, let detail)?: return sent.withdrawn ? .composing : .failed(reason, detail: detail)
        }
    }

    /// What Start sends for `ask`: the withdrawn start again when it asks exactly this and has no answer yet, else a
    /// new start.
    public func send(for ask: Ask, outcome: StartOutcome?, computerKnown: Bool) -> Send {
        guard let sent, computerKnown, sent.withdrawn, outcome == nil, sent.ask == ask else { return .new }
        return .resume(requestID: sent.requestID)
    }

    /// A start was sent; any start sent before is let go.
    public func didSend(_ requestID: String, asking ask: Ask) { sent = Sent(requestID: requestID, ask: ask) }

    /// The withdrawn start was sent again under its own id.
    public func didResume() { sent?.withdrawn = false }

    /// The person stopped the start being sent (with the computer's `stopRetrying`); the form comes back.
    public func withdraw() { sent?.withdrawn = true }

    /// Back to the form from a page (Back, Close, Start Again): the start is let go, its message put back when the
    /// form has none.
    public func compose(restoring prompt: String?) {
        if message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { message = prompt ?? "" }
        sent = nil
    }

    /// The started run is open: the message and the place it started in are spent; the folder and model stay chosen.
    public func didOpen() {
        if let sent {
            edit(on: sent.ask.computerID) {
                $0.place = .folder
                $0.newName = nil
            }
        }
        message = ""
        sent = nil
    }

    /// A start this window sent that the computer has not answered, found again at launch.
    public func adopt(_ requestID: String, asking ask: Ask) {
        guard sent == nil else { return }
        sent = Sent(requestID: requestID, ask: ask)
    }

    /// New Agent: the form gives the first message the keyboard when it next draws.
    public func requestKeyboard() { wantsKeyboard = true }

    /// The form takes the request once; true when there was one.
    public func takeKeyboardRequest() -> Bool {
        defer { wantsKeyboard = false }
        return wantsKeyboard
    }

    /// When an opening last asked the form's computers (`asksAgain`).
    @ObservationIgnored private var askedAt: Date?

    /// Whether New Agent's opening asks the form's computers again what they offer. A form this press draws does not:
    /// its first appearance asks them, and that counts. A form already on screen does, at most once every 2 s from the
    /// last ask, so a held ⌘N, which repeats, asks once.
    public func asksAgain(formShown: Bool, now: Date = .now) -> Bool {
        guard formShown else {
            askedAt = now
            return false
        }
        if let askedAt, now.timeIntervalSince(askedAt) < 2 { return false }
        askedAt = now
        return true
    }
}
