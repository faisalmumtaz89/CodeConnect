import Foundation
import Observation

/// **A run's shell, as one computer's link carries it** (minor 56, `Capabilities.servesRunTerminal`).
///
/// The shell is the run's and lives on the computer: this end only views it. One view at a time per link: showing
/// another run's shell lets the first go. Folding the view lets the shell go too (it runs on), and showing it again
/// resumes from the last output offset this end fed, so the screen the view kept is continued, not repainted. A
/// shell that exited, or that another device took, keeps its last screen until the person types: that keystroke
/// opens the shell again (a fresh one, after an exit) and is sent to it. A link that drops and comes back resumes the
/// view by itself, without taking the shell from anyone.
@MainActor
@Observable
public final class RunTerminal {
    public enum Phase: Equatable, Sendable {
        /// Not showing a shell.
        case idle
        /// An open is on its way.
        case opening
        /// Showing the run's shell, in `folder`.
        case open(folder: String)
        /// Its last screen stays; why it stopped (`exited`, `superseded`, `run_ended`, …). A keystroke opens it again.
        case stopped(reason: String)
    }

    public private(set) var phase: Phase = .idle
    /// The run whose shell this view shows, while it shows one.
    public private(set) var session: String?

    /// Where the shell's output goes, in order: the bytes, and whether the screen is cleared first. Set by the view.
    @ObservationIgnored public var output: ((Data, Bool) -> Void)?
    /// Whether output being fed now replays what the shell printed before this view took it: a terminal fed a replay
    /// must not answer the queries in it, since the program that asked is long past them.
    @ObservationIgnored public private(set) var replaying = false

    @ObservationIgnored private let connection: DaemonConnection
    @ObservationIgnored private var attachmentID: String?
    @ObservationIgnored private var size = (cols: 80, rows: 24)
    /// The size the open in flight asked for: the shell's, until the open answers.
    @ObservationIgnored private var openedSize = (cols: 80, rows: 24)
    /// The output offset fed so far, per run: what an open resumes after.
    @ObservationIgnored private var fed: [String: UInt64] = [:]
    @ObservationIgnored private var replayEnd: UInt64 = 0
    @ObservationIgnored private var inputCredit = 0
    @ObservationIgnored private var chunk = 16 * 1024
    /// Typed bytes waiting for input credit, or for the open a keystroke started.
    @ObservationIgnored private var pending: [UInt8] = []

    /// The most output this end takes ahead of feeding it: the daemon's own ceiling.
    static let outputCredit = 256 * 1024
    /// The most typed input kept waiting: a paste larger than this is cut there.
    static let pendingLimit = 1024 * 1024

    init(connection: DaemonConnection) {
        self.connection = connection
    }

    /// Show run `session`'s shell at `cols` by `rows`: the person opened it, so it is taken from any other viewer.
    public func show(session: String, cols: Int, rows: Int) {
        size = (cols, rows)
        if self.session == session, phase == .opening || isOpen { return }
        if self.session != session { letGo() }
        self.session = session
        open(takeover: true)
    }

    /// Stop showing the shell (the view folded or closed): it runs on.
    public func hide() {
        letGo()
        session = nil
        phase = .idle
        pending.removeAll()
    }

    /// The view showing run `session` is a new one, with none of the screen fed before: its next open asks for the
    /// output the computer kept rather than resuming after what the old view had.
    public func forgetScreen(session: String) {
        fed[session] = nil
    }

    /// The view's size changed.
    public func resize(cols: Int, rows: Int) {
        guard (cols, rows) != size else { return }
        size = (cols, rows)
        if let attachmentID, isOpen {
            _ = connection.sendInOrder(.terminalResize(attachmentID: attachmentID, cols: cols, rows: rows))
        }
    }

    /// Bytes the person typed or pasted. Into a stopped view they open the shell again and go to it.
    public func type(_ bytes: [UInt8]) {
        guard session != nil, !bytes.isEmpty else { return }
        pending.append(contentsOf: bytes.prefix(max(0, Self.pendingLimit - pending.count)))
        switch phase {
        case .open: flush()
        case .stopped: open(takeover: true)
        case .idle, .opening: break
        }
    }

    /// The link handshook again: a view that was showing a shell takes it up again, from where it left off, without
    /// taking it from another viewer.
    func linkUp() {
        guard session != nil, phase != .idle else { return }
        if case .stopped = phase { return }
        open(takeover: false)
    }

    /// The link dropped: the attachment went with it, and the shell runs on.
    func linkDown() {
        attachmentID = nil
        if phase != .idle, !isStopped { phase = .opening }
    }

    /// A terminal frame from the daemon.
    func receive(_ message: ServerMessage) {
        switch message {
        case .terminalOpened(let id, let credit, let maxChunk, let folder, let end) where id == attachmentID:
            inputCredit = credit
            chunk = max(1, maxChunk)
            replayEnd = end
            phase = .open(folder: folder)
            // The view took its size while the open was on its way.
            if size != openedSize {
                _ = connection.sendInOrder(.terminalResize(attachmentID: id, cols: size.cols, rows: size.rows))
            }
            flush()
        case .terminalOutput(let id, let data, let reset, let offset) where id == attachmentID:
            guard let session else { return }
            replaying = offset <= replayEnd
            output?(data, reset)
            replaying = false
            fed[session] = offset
            if !data.isEmpty { _ = connection.sendInOrder(.terminalCredit(attachmentID: id, bytes: data.count)) }
        case .terminalCredit(let id, let bytes) where id == attachmentID:
            inputCredit += bytes
            flush()
        case .terminalClosed(let id, let reason) where id == attachmentID:
            attachmentID = nil
            inputCredit = 0
            phase = .stopped(reason: reason)
        default:
            break
        }
    }

    /// Whether the shell is open in this view.
    public var isShowing: Bool { isOpen }

    private var isOpen: Bool {
        if case .open = phase { return true }
        return false
    }

    private var isStopped: Bool {
        if case .stopped = phase { return true }
        return false
    }

    /// An open the link cannot carry yet still reads `.opening`: the handshake's `linkUp` sends it.
    private func open(takeover: Bool) {
        guard let session else { return }
        attachmentID = nil
        inputCredit = 0
        phase = .opening
        guard connection.capabilities?.servesRunTerminal == true else { return }
        let id = UUID().uuidString
        attachmentID = id
        openedSize = size
        let sent = connection.sendInOrder(
            .terminalOpen(
                attachmentID: id, session: session, cols: size.cols, rows: size.rows, outputCredit: Self.outputCredit,
                resumeAfter: fed[session], takeover: takeover))
        if !sent { attachmentID = nil }
    }

    private func letGo() {
        if let attachmentID { _ = connection.sendInOrder(.terminalDetach(attachmentID: attachmentID)) }
        attachmentID = nil
        inputCredit = 0
    }

    private func flush() {
        guard let attachmentID, isOpen else { return }
        while !pending.isEmpty, inputCredit > 0 {
            let count = min(pending.count, inputCredit, chunk)
            let bytes = Data(pending.prefix(count))
            guard connection.sendInOrder(.terminalInput(attachmentID: attachmentID, data: bytes.base64EncodedString()))
            else { return }
            pending.removeFirst(count)
            inputCredit -= count
        }
    }
}
