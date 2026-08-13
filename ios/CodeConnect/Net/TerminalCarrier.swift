import Foundation

/// The live terminal half of the product: the phone's end of a tmux pane
/// streamed over the connection it is *already* paired on.
///
/// There is no second transport, no second credential, and nothing for the user
/// to switch on: if the timeline is live, the terminal can open. The daemon
/// attaches a disposable tmux control-mode client to the exact hosted session
/// and forwards that pane's bytes; this class turns them back into bytes for the
/// emulator, and keystrokes into `terminal_input`.
///
/// This is deliberately *not* the semantic surface. Everything the app knows
/// about state comes from the daemon's event log; this is the escape hatch where
/// the phone becomes the Mac's keyboard. Bytes are never parsed for meaning —
/// they go to the terminal emulator and nowhere else, so nothing scraped off
/// this screen can ever become a fact the rest of the app believes.
///
/// Four properties are worth not breaking:
///
///   * **Flow control is a contract, not a suggestion.** Each direction has a
///     credit window; exceeding it is a protocol error the daemon closes the
///     terminal for. Output credit is returned only after bytes have been fed to
///     the emulator, and input is refused rather than sent when the window is
///     spent, so neither side can be made to buffer without bound.
///   * **One attachment at a time, identified.** Every frame carries the
///     attachment id, and anything naming a different one is ignored: a late
///     frame from a terminal that has already closed can never be drawn into its
///     successor.
///   * **Outbound frames are one ordered stream, a frame that fails to send
///     ends the terminal, and nothing is written for a terminal that has
///     ended.** All three are the outbound chain. Order, because these frames
///     have rules about their sequence — an attach that overtakes its detach is
///     read by the daemon as a second attach, which closes the terminal that
///     exists and answers the new one with nothing. Failure, because a frame
///     that never reaches the wire desynchronises the two ends in silence. And
///     nothing after the ending, because the frames already queued behind a
///     failed one are the rest of a paste: written anyway, they are keystrokes
///     going into a shell this phone has stopped showing.
///   * **Disconnected is always visible.** A terminal showing the last thing it
///     received is indistinguishable from a live one, which is the exact lie
///     this rule exists to forbid — so the view gets an explicit not-live state,
///     and a dropped socket — or a single dropped frame — ends the terminal
///     rather than freezing it.
@MainActor
@Observable
final class TerminalCarrier {
    enum Phase: Sendable, Equatable {
        case idle
        /// `terminal_attach` is out; the daemon has not answered yet.
        case attaching
        case attached(since: Date)
        /// The terminal ended. Carries why, because "it stopped" is not a reason
        /// anyone can act on, and whether it had ever been live — a failure to
        /// open reads differently from a session that ended under you.
        case ended(reason: String, wasAttached: Bool)

        var isAttached: Bool {
            if case .attached = self { return true }
            return false
        }

        var isBusy: Bool {
            if case .attaching = self { return true }
            return false
        }
    }

    /// Whose terminal this carrier is holding, seen from a screen for one run.
    ///
    /// One carrier serves the whole app — the terminal has to survive SwiftUI
    /// rebuilding the tab — so *which run's keyboard it is* cannot be read off
    /// the phase. A screen that renders a phase without asking this shows
    /// another run's live pane under its own label and sends its keystrokes
    /// there.
    enum Standing: Sendable, Equatable {
        /// The carrier is this run's: its phase is this screen's to render.
        case mine
        /// No terminal is open. This run may open one without ending another.
        case free
        /// Another run's terminal is open. Opening this one ends that one,
        /// which is a thing to be asked rather than done.
        case heldByAnotherRun(sessionUID: String)

        /// Whether the carrier's own state is this run's to read. A screen that
        /// reasons about the last close has to ask first: another run's close
        /// says nothing about this one.
        var isMine: Bool {
            if case .mine = self { return true }
            return false
        }
    }

    /// What the daemon said when it closed the terminal. The wire carries a
    /// stable code plus human text; the code is what the UI reasons about, so a
    /// reworded reason never changes behaviour.
    enum CloseCode: String, Sendable, CaseIterable {
        case sessionNotHosted = "session_not_hosted"
        case identityMismatch = "identity_mismatch"
        case tmuxUnavailable = "tmux_unavailable"
        /// The Mac-wide cap on open terminals, and **only** that: a second
        /// terminal on the same session is no longer refused, it supersedes.
        case attachmentLimit = "attachment_limit"
        /// A newer attach took this session's terminal over. Sent to the holder
        /// that lost it, which may well be this phone's own dead one — a socket
        /// that died without a FIN leaves a lease the daemon cannot tell from a
        /// live reader, and taking it back is exactly what the reconnecting
        /// phone is entitled to do.
        case superseded
        /// An attach arrived while the session's previous terminal was still
        /// closing. Nothing is wrong; the close settles on its own.
        case sessionBusy = "session_busy"
        case notAuthorised = "not_authorised"
        case protocolError = "protocol_error"
        case slowConsumer = "slow_consumer"
        case windowChanged = "window_changed"
        case detached
        case sessionExited = "session_exited"

        /// Whether reattaching could plausibly work. A session that ended, or a
        /// connection that may not open one at all, is not worth a retry button.
        ///
        /// `superseded` and `sessionBusy` are both retryable, and neither is a
        /// courtesy: taking a terminal back from a holder that took it from you
        /// is the whole point of the supersede rule, and an attach refused
        /// because a close had not settled yet succeeds as soon as it has.
        var isRetryable: Bool {
            switch self {
            case .tmuxUnavailable, .attachmentLimit, .protocolError, .slowConsumer,
                .windowChanged, .identityMismatch, .superseded, .sessionBusy:
                return true
            case .sessionNotHosted, .sessionExited, .notAuthorised, .detached:
                return false
            }
        }
    }

    // MARK: Observable state

    private(set) var phase: Phase = .idle
    /// Rows and columns the emulator last reported, so a reattach starts at the
    /// right size instead of at 80×24.
    private(set) var lastSize: (cols: Int, rows: Int) = (80, 24)
    /// The tail of everything received, so a view rebuild can replay what is
    /// already on screen instead of showing an empty terminal.
    private(set) var transcript: [UInt8] = []
    /// Whether this buffer has lost a head to the cap.
    ///
    /// The whole reason it is here: a rebuilt emulator seeded from a buffer
    /// that has been trimmed is looking at output that begins in the middle of
    /// the session, and nothing else on the pane says so.
    ///
    /// **Read at the moment a fresh emulator is seeded, and nowhere else.**
    /// It is a fact about *this buffer*, not about any pane: a pane that
    /// survived a rebuild holds every byte in its own scrollback while this is
    /// true, and a pane the daemon has repainted holds a whole screen while it
    /// is false. Only the one site that hands the buffer to a new emulator can
    /// turn it into a claim about what that emulator is showing — see
    /// `TerminalTabView.seededFromTrimmedBuffer`.
    private(set) var transcriptIsTruncated = false
    /// The session this terminal is for, while there is one.
    private(set) var sessionUID: String?
    /// The code the daemon closed with, for the view's explanation and retry.
    private(set) var lastClose: CloseCode?
    /// Bytes this carrier has ever received, across every attachment.
    ///
    /// Monotonic on purpose. `transcript` is capped, so past the cap its count
    /// stops changing while output is still arriving — and anything watching
    /// that count for freshness freezes at the moment the buffer filled, which
    /// makes a dead terminal claim it was last live then.
    ///
    /// **Deliberately not observable.** It moves on every chunk — a hundred
    /// times a second on a busy pane — and a SwiftUI body that read it would be
    /// re-run that often, on the main thread, competing with the emulator's own
    /// drawing. `lastOutputAt` below is the observable form of the same fact,
    /// sampled at the resolution anything actually renders it at. A view that
    /// wants this number for a *count* may still read it; what it must not do is
    /// depend on being told when it changes.
    @ObservationIgnored private(set) var totalOutputBytes: Int = 0
    /// When output last arrived, sampled at most once a second.
    ///
    /// The freshness fact every liveness surface renders, and the reason it is
    /// sampled *here* rather than at the view: the strip shows a clock time to
    /// the second, so a stamp written per chunk would invalidate every observer
    /// of this carrier at chunk rate to redraw the same string.
    ///
    /// Sampled without a timer. The first chunk of an attachment always sets it
    /// — a terminal that has drawn something and claims no output is worse than
    /// one second of staleness — and afterwards a chunk moves it only once the
    /// stored value is a second old. So it is at most one second behind, which
    /// is below what a whole-seconds clock can show, and it never ticks on its
    /// own: a quiet pane's stamp is the last time bytes really arrived.
    private(set) var lastOutputAt: Date?

    // MARK: Private

    private let connection: any PairedConnection
    /// The tail of the outbound chain: the write this carrier submitted last.
    /// Every frame awaits it before writing its own, which is what makes
    /// submission order wire order. See `submit`.
    private var outbound: Task<Void, Never>?
    /// Where output goes, and who asked for it. Owner-stamped so a terminal
    /// view being torn down cannot silence the one that replaced it.
    private var onOutput: ((ArraySlice<UInt8>) -> Void)?
    private var outputOwner: ObjectIdentifier?
    /// This terminal's id on the wire. New for every attach, so a frame from a
    /// previous attachment can be told apart from this one's.
    private var attachmentID: String?
    /// Decoded input bytes this phone may still send. Spent on send, replenished
    /// by `terminal_credit`.
    private var inputCredit: UInt32 = 0
    /// Decoded output bytes the daemon may still send us. Spent as output
    /// arrives, replenished once it has been fed to the emulator.
    private var outputCredit: UInt32 = 0
    /// Keystrokes waiting for input credit, oldest first. Bounded: past this a
    /// terminal that cannot take input is not one worth queueing more for.
    private var pendingInput: [UInt8] = []
    /// The largest decoded chunk one `terminal_input` may carry, **as this
    /// attachment's daemon stated it** in `terminal_attached`.
    ///
    /// Per-attachment rather than global: it is the answer of the daemon that
    /// opened this terminal, and it is reset to the mirrored default at every
    /// attach so a ceiling learned from one Mac can never bound a frame sent to
    /// another. A daemon that omits the field leaves the default standing, which
    /// is what this app enforced before the field existed.
    private var maxChunkBytes = Wire.Terminal.maxChunkBytes
    /// The ceiling outstanding credit may reach in either direction, from the
    /// same frame and reset the same way.
    private var maxOutstandingCredit = Wire.Terminal.maxOutstandingCredit
    /// The wait for an answer to the attach that is out, cancelled the moment
    /// one arrives. See `startAttachDeadline`.
    private var attachDeadline: Task<Void, Never>?

    /// The most unsent input to hold while waiting for credit. One window's
    /// worth: if a whole credit window has been granted and spent without this
    /// draining, the pane is not reading and dropping is honest.
    private static let maxPendingInput = Int(Wire.Terminal.maxOutstandingCredit)
    /// Bounded so a chatty agent cannot grow the transcript without limit.
    ///
    /// **The emulator holds the scrollback that matters, for as long as the
    /// screen is on the stack.** The pane is one view identity through live and
    /// ended both, and through the Timeline/Terminal picker — both were once a
    /// `switch` that dismantled it, and both are now slots of their own, with a
    /// test on each. So while the reader stays on the session, what this holds
    /// is only what a rebuilt view would need to look continuous.
    ///
    /// Leaving the screen for the fleet and coming back is still a rebuild —
    /// the whole detail view goes with the navigation stack — and on that one
    /// path this buffer is the entire ceiling on what is replayed.
    ///
    /// **And a reattach is not that path.** A confirmed `terminal_attached`
    /// empties this buffer, because the daemon opens every attachment by
    /// capturing the pane's whole current screen and cursor and sending it as
    /// one repaint. That repaint is what makes a reattached terminal correct;
    /// this buffer only makes a *rebuilt* one look continuous.
    private static let maxTranscriptBytes = 256 * 1024
    /// What a trim leaves behind, deliberately below the cap rather than at it.
    ///
    /// Trimming back to the cap exactly means trimming on *every* chunk once
    /// the buffer is full, so a one-byte keystroke echo would move a quarter of
    /// a megabyte. Headroom makes it one move per 32 KiB of output instead.
    private static let trimmedTranscriptBytes = maxTranscriptBytes - 32 * 1024

    init(connection: any PairedConnection) {
        self.connection = connection
        connection.onTerminal = { [weak self] message in self?.handle(message) }
        connection.onDisconnected = { [weak self] in
            self?.end(reason: "The connection to the Mac dropped.", code: nil)
        }
    }

    // MARK: - Lifecycle

    /// This carrier's standing for a screen showing `run` — the first thing
    /// such a screen asks, before it renders anything of the carrier at all.
    func standing(forRun run: String) -> Standing {
        Self.standing(phase: phase, serving: sessionUID, forRun: run)
    }

    /// The phase a screen for `run` may render, which is `idle` for every run
    /// but the one whose terminal this is.
    ///
    /// The reason it is not `phase`: a screen that reads the phase directly
    /// draws another run's live pane under its own label, hands that run's
    /// keystrokes to it, and — because the carrier is then busy — can never
    /// open the terminal it is actually for.
    func phase(forRun run: String) -> Phase {
        guard case .mine = standing(forRun: run) else { return .idle }
        return phase
    }

    /// The standing of a carrier in `phase` serving `serving`, from a screen
    /// for `run`.
    ///
    /// A terminal that has *ended* holds nothing, whichever run it belonged to:
    /// the next run to ask gets it without ending anybody's.
    nonisolated static func standing(
        phase: Phase, serving: String?, forRun run: String
    ) -> Standing {
        if serving == run { return .mine }
        guard let serving, phase.isAttached || phase.isBusy else { return .free }
        return .heldByAnotherRun(sessionUID: serving)
    }

    /// Whether `attach` would do anything: used by foreground-reattach so it
    /// cannot stack attempts.
    var canAttach: Bool {
        switch phase {
        case .idle, .ended: return true
        case .attaching, .attached: return false
        }
    }

    /// Open a terminal on `sessionUID` at the emulator's current size.
    ///
    /// Refused before it reaches the wire when the connection is not up or does
    /// not offer a terminal: an attach the daemon would only close is a worse
    /// answer than saying so.
    func attach(sessionUID: String, cols: Int, rows: Int) {
        guard canAttach else { return }
        guard connection.isConnected else {
            end(reason: "Not connected to the Mac.", code: nil)
            return
        }
        guard connection.servesTerminal else {
            end(
                reason:
                    "This connection may not open a terminal. Pair this device, then try again.",
                code: .notAuthorised)
            return
        }
        let size = Self.clampSize(cols: cols, rows: rows)
        lastSize = size
        // A change of run empties the transcript *here*, because one run's
        // bytes are never another's to hold. The same run's are kept until the
        // daemon answers: an attach that is refused — the foreground reattach
        // into a session that has since exited — must leave the reader the
        // snapshot they were looking at rather than replacing it with an empty
        // screen. What a successful attach replaces it with arrives with the
        // answer, in `terminal_attached`.
        if self.sessionUID != sessionUID { clearTranscript() }
        self.sessionUID = sessionUID
        lastClose = nil
        pendingInput.removeAll(keepingCapacity: true)

        let id = UUID().uuidString
        attachmentID = id
        inputCredit = 0
        outputCredit = Wire.Terminal.initialOutputCredit
        // Back to the mirrored numbers until this attachment's daemon states
        // its own. The attach itself is bounded by them, because it is sent
        // before there is anything else to be bounded by.
        maxChunkBytes = Wire.Terminal.maxChunkBytes
        maxOutstandingCredit = Wire.Terminal.maxOutstandingCredit
        phase = .attaching
        submit(
            .terminalAttach(
                attachmentID: id, sessionUID: sessionUID, cols: size.cols, rows: size.rows,
                outputCredit: Wire.Terminal.initialOutputCredit),
            for: id)
        startAttachDeadline(for: id)
    }

    /// Open `sessionUID`'s terminal, ending whatever terminal is already open.
    ///
    /// The daemon allows one terminal per connection, so opening a second run's
    /// *is* ending the first one's. That fact is one call rather than a caller's
    /// two, because a caller that forgets the detach gets an attach the carrier
    /// silently refuses — a button that does nothing.
    /// The connection is checked *before* anything is ended, not left to
    /// `attach`: a link that dropped between the screen offering this and the
    /// tap arriving would otherwise cost the other run its terminal and open
    /// nothing in its place. Nothing is taken unless it can be given.
    ///
    /// **The detach reaches the daemon first, and that is a guarantee rather
    /// than a hope.** Both frames go through `submit`, which writes them in the
    /// order they were submitted.
    ///
    /// What the order no longer decides is whether this works at all. The daemon
    /// used to answer an attach arriving while it held a terminal by closing the
    /// one it had and replying to the new attachment with nothing, which stranded
    /// the carrier on a reply that never came; it now supersedes — the held
    /// terminal is closed as `superseded` and the new one is opened and answered
    /// (see [`CloseCode.superseded`]) — and a detach naming a terminal it no
    /// longer holds is ignored rather than refused. So the reversed order would
    /// reach the same place.
    ///
    /// It is still this order, for what the order is actually worth: the other
    /// run's terminal is released on a frame that says so rather than as a
    /// side effect of taking it, and this carrier's own state is cleared before
    /// a new attachment id exists to be confused with the old one.
    func takeOver(sessionUID: String, cols: Int, rows: Int) {
        guard connection.isConnected, connection.servesTerminal else { return }
        if !canAttach { detach(reason: "Another run took the terminal.") }
        attach(sessionUID: sessionUID, cols: cols, rows: rows)
    }

    /// Close the terminal and say why. Safe to call from any state; tells the
    /// daemon so its disposable client goes away now rather than at the next
    /// timeout.
    func detach(reason: String = "Disconnected.") {
        if let id = attachmentID, phase.isAttached || phase.isBusy {
            submitDetach(of: id)
        }
        end(reason: reason, code: .detached)
    }

    // MARK: - Terminal I/O

    /// What a screen for `run` may replay into a fresh emulator: this run's
    /// bytes, and never another run's. The carrier is shared, so handing back
    /// `transcript` unasked is how one run's output gets drawn into another
    /// run's terminal under that run's label.
    func transcript(forRun run: String) -> ArraySlice<UInt8> {
        guard sessionUID == run else { return [] }
        return ArraySlice(transcript)
    }

    /// Send output to `owner`'s emulator, replacing whatever was receiving it.
    func deliverOutput(to owner: AnyObject, sink: @escaping (ArraySlice<UInt8>) -> Void) {
        outputOwner = ObjectIdentifier(owner)
        onOutput = sink
    }

    /// Stop sending output to `owner`, and only to `owner`. A terminal view is
    /// commonly torn down *after* the one replacing it is already receiving —
    /// leaving a live terminal that draws nothing, which looks exactly like a
    /// hung agent.
    func stopDeliveringOutput(to owner: AnyObject) {
        guard outputOwner == ObjectIdentifier(owner) else { return }
        outputOwner = nil
        onOutput = nil
    }

    /// Keystrokes from the emulator.
    ///
    /// Split to the wire's maximum chunk and spent against input credit. What
    /// does not fit waits for the daemon to grant more, so this never sends past
    /// the window the daemon agreed to — doing so would close the terminal.
    func send(_ bytes: ArraySlice<UInt8>) {
        // Refused, not queued, before the attach is answered — which is what
        // makes the queue provably empty at that point. Nothing reaches this
        // while a terminal is opening: the screen for that state has no
        // emulator on it. Were that to change, keystrokes typed during the
        // wait would need flushing where the attach is confirmed.
        guard phase.isAttached, !bytes.isEmpty else { return }
        if pendingInput.count + bytes.count > Self.maxPendingInput {
            // A whole window is already waiting: the pane is not reading, and
            // silently growing this queue would be the unbounded buffer the
            // credit protocol exists to prevent.
            return
        }
        pendingInput.append(contentsOf: bytes)
        flushInput()
    }

    func send(text: String) {
        send(ArraySlice(Array(text.utf8)))
    }

    /// The emulator resized. Tell the daemon, or the agent's TUI will draw for
    /// the wrong window. Applied to the daemon's own client only, so a human at
    /// the Mac is never resized by this phone.
    func resize(cols: Int, rows: Int) {
        let size = Self.clampSize(cols: cols, rows: rows)
        guard lastSize != size else { return }
        // The phase is checked *before* the size is recorded, because
        // `lastSize` is not scratch: it is the geometry the next attach opens
        // at. Recording it from an emulator that is not attached lets a
        // transient layout — a rotation part-way through, a view sized before
        // it is on screen — become what the daemon is told to draw for.
        guard phase.isAttached, let id = attachmentID else { return }
        lastSize = size
        submit(.terminalResize(attachmentID: id, cols: size.cols, rows: size.rows), for: id)
    }

    // MARK: - Incoming

    private func handle(_ message: ServerMessage) {
        switch message {
        case .terminalAttached(let id, let inputCredit, let chunkBound, let creditCeiling):
            guard id == attachmentID, case .attaching = phase else { return }
            // Answered: the wait is over. `end` cancels it on every other
            // ending, and this is the one path that does not go through `end`.
            attachDeadline?.cancel()
            attachDeadline = nil
            // The advertised ceilings take effect before the first thing they
            // bound — the credit clamp on the next line — so this attachment is
            // governed end to end by what its own daemon said.
            //
            // **A ceiling this side cannot flow-control against is treated as
            // absent**, on both fields and for one reason: an unusable bound
            // stalls the terminal silently, and a stall no reader can act on is
            // worse than enforcing the number this build shipped with.
            //
            // Zero is the obvious case — a zero chunk cap leaves `flushInput`
            // unable to move a single byte, and a zero ceiling makes every grant
            // nothing. A credit ceiling *below the output credit this phone
            // already granted in its attach* is the same thing arriving by a
            // longer route: `grantOutputCredit` computes its headroom against
            // this number, so a ceiling under 64 KiB is zero headroom for as
            // long as the attach's own grant is outstanding — which is forever,
            // because returning it is what the headroom was for. The pane would
            // draw 64 KiB and then go quiet with nothing on screen to say why.
            //
            // The daemon guarantees `max_outstanding_credit >=
            // TERMINAL_INITIAL_OUTPUT_CREDIT`, so this is defence against a Mac
            // that breaks its own contract rather than a case in it — and the
            // fallback is the number that was enforced before either field
            // existed, which is the conservative answer in both directions.
            if let chunkBound, chunkBound > 0 { maxChunkBytes = Int(chunkBound) }
            if let creditCeiling, creditCeiling >= Wire.Terminal.initialOutputCredit {
                maxOutstandingCredit = creditCeiling
            }
            self.inputCredit = min(inputCredit, maxOutstandingCredit)
            // The snapshot `attach` kept is spent: what this attachment sends
            // is the pane as it is now, and the old bytes would be replayed
            // above it as though they were part of the same screen.
            clearTranscript()
            // And the freshness stamp with it: this attachment has drawn
            // nothing yet, and the previous one's stamp under a fresh screen is
            // a claim about bytes that are no longer on it. A *refused* attach
            // keeps the old stamp, because it keeps the old screen — see
            // `attach`.
            lastOutputAt = nil
            phase = .attached(since: Date())
            // Nothing is flushed here. There can be nothing to flush: `send`
            // is the only thing that queues input and it refuses everything
            // until this line has run, so a flush would be a call that cannot
            // do anything and a comment claiming it can.
        case .terminalOutput(let id, let base64):
            guard id == attachmentID, phase.isAttached else { return }
            guard let data = Data(base64Encoded: base64) else {
                // Undecodable output is not something to draw. Ending is the
                // honest answer: the stream's framing is not what it claims.
                detachAfterProtocolFault("The Mac sent output this app could not decode.")
                return
            }
            let bytes = [UInt8](data)
            guard !bytes.isEmpty else { return }
            // **Two bounds, and this end is the one that has to check them.**
            // The wire documents the chunk cap as receiver-enforced, and the credit
            // ceiling is only a bound if the end that granted the credit checks
            // what comes back against it. A daemon that keeps its side of both
            // never reaches either line; one that does not is either broken or
            // is not the Mac it claims to be, and in both cases the bytes are
            // not something to draw.
            //
            // **Checked before the ledger moves, and in this order.** The chunk
            // bound is what keeps the byte count inside `UInt32` for the credit
            // arithmetic below, so it cannot be the second test.
            guard bytes.count <= maxChunkBytes else {
                detachAfterProtocolFault(
                    "The Mac sent a larger burst of terminal output than the protocol allows.")
                return
            }
            // Absorbing this was the silent failure: clamping the ledger to
            // zero and feeding the bytes anyway grants the overdraft straight
            // back, so a daemon that ignores the window is handed an unbounded
            // one — which is the whole thing the window is for.
            guard UInt32(bytes.count) <= outputCredit else {
                detachAfterProtocolFault(
                    "The Mac sent more terminal output than this app had asked for.")
                return
            }
            totalOutputBytes += bytes.count
            noteOutputArrived()
            outputCredit -= UInt32(bytes.count)
            appendTranscript(bytes)
            onOutput?(ArraySlice(bytes))
            // Consumed: return exactly what was drawn, so the daemon may send
            // that much more. Credit returns *after* the feed, never before.
            grantOutputCredit(UInt32(bytes.count))
        case .terminalCredit(let id, let bytes):
            guard id == attachmentID else { return }
            // The same ceiling, in the other direction — and the same reason to
            // fault rather than clamp. A grant past the ceiling is a request to
            // buffer more than either end agreed to hold, and the Mac closes
            // this phone's terminal for the mirror-image frame (`output credit
            // overflow`). Clamping it made this side the only one that let it
            // pass. Summed in 64 bits so a grant near `UInt32.max` cannot wrap
            // its way under the ceiling.
            let granted = UInt64(inputCredit) + UInt64(bytes)
            guard granted <= UInt64(maxOutstandingCredit) else {
                detachAfterProtocolFault(
                    "The Mac granted more terminal input than the protocol allows.")
                return
            }
            inputCredit = UInt32(granted)
            flushInput()
        case .terminalClosed(let id, let code, let reason):
            guard id == attachmentID else { return }
            let parsed = CloseCode(rawValue: code)
            end(reason: Self.describe(code: parsed, reason: reason), code: parsed)
        default:
            break
        }
    }

    /// Move the freshness stamp, at most once a second.
    ///
    /// **No timer, and it must stay that way.** A timer would keep the stamp
    /// moving after the last byte — a clock that ticks on its own is a terminal
    /// claiming output it never received, which is the one lie the liveness
    /// strip exists to forbid. Sampling on arrival means the stamp can only ever
    /// be *behind*, and never by more than the second below.
    ///
    /// The first chunk of an attachment always lands, because `lastOutputAt` is
    /// cleared when the terminal attaches: a pane that has visibly drawn while
    /// the strip says nothing has arrived is a worse answer than a stamp a
    /// second stale.
    private func noteOutputArrived() {
        let now = Date()
        guard let last = lastOutputAt else {
            lastOutputAt = now
            return
        }
        guard now.timeIntervalSince(last) >= Self.outputStampInterval else { return }
        lastOutputAt = now
    }

    /// How coarse the freshness stamp is, in seconds. One, because every
    /// surface renders it as a whole-second clock time — a finer sample would
    /// invalidate this carrier's observers to redraw an identical string.
    private static let outputStampInterval: TimeInterval = 1

    // MARK: - Flow control

    // Both ledgers below move when a frame is *submitted*, not when its write
    // is confirmed, and neither is rolled back when a write fails.
    //
    // **Because the alternative breaks the protocol.** Waiting for confirmation
    // leaves a window in which this carrier still counts credit it has already
    // promised away, and a second grant computed against that stale figure
    // pushes the daemon's outstanding window past `maxOutstandingCredit` —
    // which the daemon reads as `output credit overflow` and closes the
    // terminal for. Input has the same shape: a second `flushInput` before the
    // first was confirmed would send the same bytes twice against one grant.
    //
    // **And a rollback would be unobservable.** A submitted frame misses the
    // wire two ways, and both end at the same place. Its write fails, and
    // `submit` ends the terminal for it. Or `enqueue` drops it as stale — which
    // it does on exactly one condition, that `attachmentID` has changed, and
    // the only two places that change it are `end` and `attach`. Both zero
    // these ledgers in the same synchronous block. So a figure moved by a frame
    // that never reached the daemon has already been reset by the time anything
    // could read it, and writing a rollback would be ceremony rather than
    // correctness.

    /// Send as much queued input as credit allows, in wire-sized chunks.
    private func flushInput() {
        guard phase.isAttached, let id = attachmentID else { return }
        while !pendingInput.isEmpty, inputCredit > 0 {
            let take = min(
                pendingInput.count, maxChunkBytes, Int(inputCredit))
            let chunk = Array(pendingInput.prefix(take))
            pendingInput.removeFirst(take)
            inputCredit -= UInt32(take)
            submit(
                .terminalInput(attachmentID: id, base64: Data(chunk).base64EncodedString()),
                for: id)
        }
    }

    /// Return output credit for bytes already drawn, never letting the daemon's
    /// outstanding window exceed the ceiling *this attachment's daemon named*.
    private func grantOutputCredit(_ bytes: UInt32) {
        guard phase.isAttached, let id = attachmentID, bytes > 0 else { return }
        let headroom = maxOutstandingCredit - min(outputCredit, maxOutstandingCredit)
        let grant = min(bytes, headroom)
        guard grant > 0 else { return }
        outputCredit += grant
        submit(.terminalCredit(attachmentID: id, bytes: grant), for: id)
    }

    // MARK: - The outbound chain

    /// Write one terminal frame for `attachment` after every frame already
    /// submitted — unless that attachment is over by the time its turn comes.
    ///
    /// Everything a live terminal sends goes through here. The detach that ends
    /// one does not: see `submitDetach`.
    private func submit(_ message: ClientMessage, for attachment: String) {
        enqueue(message, revalidating: attachment)
    }

    /// Write the detach that lets the daemon go, after every frame already
    /// submitted.
    ///
    /// **The one frame written whatever has happened since**, because it exists
    /// *for* an attachment that is over: `end` clears the id before this reaches
    /// the front of the chain, so the staleness rule below would drop the very
    /// frame that says so. Dropped, the daemon keeps the attachment until its
    /// own timeout — and the reader's next Connect is then a second attach on a
    /// connection that already holds a terminal, which the daemon answers by
    /// closing the one it has and never replying to the new one.
    ///
    /// Its own failure ends nothing. There is nothing left to end, and a detach
    /// sent because a detach failed is a loop.
    ///
    /// **It can name an attachment the daemon never saw**, and that is newly
    /// true: the attach ahead of it in the chain is dropped if the terminal
    /// ended before its turn came, so this arrives for an id the far end has
    /// never heard of. Harmless, and only because the daemon answers a detach
    /// for an unknown id with nothing at all rather than with a protocol error
    /// — which is cheaper than the alternative it replaces, an attach and a
    /// detach that make the daemon open a tmux client and tear it down again.
    private func submitDetach(of attachment: String) {
        enqueue(.terminalDetach(attachmentID: attachment), revalidating: nil)
    }

    /// The chain itself: append one frame, write it when its turn comes, and
    /// end the terminal if it cannot be written.
    ///
    /// **Order.** `URLSessionWebSocketTask` does not serialise concurrent
    /// `send` calls, so the connection's fire-and-forget send — one unstructured
    /// task per frame — hands frames to the wire in whatever order the transport
    /// finishes them, whatever order this actor started them in. Measured
    /// against a real socket and a real server at this protocol's frame sizes,
    /// six to ten takeovers in every hundred put the attach on the wire ahead of
    /// its own detach. That is not a tolerance the terminal has: a paste larger
    /// than one chunk is keystrokes that have to arrive in the order they were
    /// typed, and there is no reading of them out of order that is merely
    /// untidy. (`takeOver`'s own two frames are the weaker case — see it for why
    /// the daemon now reaches the same place either way — but they are carried
    /// by the same guarantee because everything here is.) Awaiting the previous
    /// write is the whole mechanism — frame *n* cannot reach the socket before
    /// frame *n-1* has returned from it.
    ///
    /// **Failure.** `sendIgnoringFailure` is named for callers whose frame will
    /// come round again — a fleet refresh, a resubscribe. Nothing the terminal
    /// sends comes round again. A lost `terminal_credit` leaves the daemon
    /// unable to send while this phone still draws Live, which is the precise
    /// lie the liveness strip exists to forbid, so a write that throws ends the
    /// terminal instead of being swallowed.
    ///
    /// **Staleness.** A frame's turn can come long after it was submitted — the
    /// chain is FIFO and a write parks on the socket for as long as the
    /// transport takes — and by then this carrier may have let its attachment
    /// go: the frame ahead of it failed, the daemon closed the terminal, the
    /// reader took the terminal for another run. So the attachment is asked for
    /// again immediately before the write, and a frame for one this carrier no
    /// longer holds is dropped rather than sent. Without that, the followers of
    /// a failed frame still reach the daemon: the first chunk of a paste fails,
    /// the terminal ends, and chunks two and three are typed into a shell this
    /// phone has stopped showing — with the detach, appended behind them,
    /// arriving last of all. Dropping them is also what puts that detach next on
    /// the wire rather than last, because the frames ahead of it write nothing.
    ///
    /// What it costs is the tail of a paste when the terminal ends mid-write,
    /// and that is the right way round: once the terminal is over this phone has
    /// stopped being the Mac's keyboard, so a keystroke landing after it is one
    /// nobody is watching for.
    ///
    /// **Nothing here runs on the main actor for long.** Both `await`s are
    /// suspensions, not waits: the actor is released at each, so the emulator
    /// keeps drawing and the timeline keeps arriving while a write is in
    /// flight. What is serialised is this carrier's writes, and only those.
    private func enqueue(_ message: ClientMessage, revalidating attachment: String?) {
        let previous = outbound
        outbound = Task { @MainActor [weak self] in
            await previous?.value
            // The connection, not the carrier. Binding `self` here would hold
            // it strongly across the write, so a carrier let go while a frame
            // is in flight could not be released until the socket answered.
            guard let connection = self?.connection else { return }
            if let attachment, self?.attachmentID != attachment { return }
            do {
                try await connection.send(message)
            } catch {
                if let attachment { self?.endAfterWriteFailure(of: attachment) }
            }
        }
    }

    /// End the terminal after a write did not land, and tell the daemon so.
    ///
    /// **Only the attachment the frame belonged to.** The chain is FIFO, so a
    /// frame can still be waiting for the socket after the carrier has moved
    /// on: a `terminal_credit` queued for X fails while Y is attaching, and
    /// ending "the terminal" there closes a terminal that was never the one at
    /// fault — the reader's tab for run B says `Terminal closed` about an
    /// attachment that was never tried, and the detach lands behind Y's attach
    /// so the daemon opens a tmux client and tears it down again. A frame for
    /// an attachment this carrier has already let go is nothing to act on: it
    /// failed to reach a terminal that is over.
    ///
    /// Most such frames never get as far as failing — `enqueue` drops them
    /// before the write. This is still the reachable case rather than belt and
    /// braces, because the write is a *suspension*: a frame that passed that
    /// check can be inside `send` while the daemon's close, or the reader's
    /// takeover, moves the carrier on underneath it.
    ///
    /// **The detach is the rest of it, and it is the next thing written.** One
    /// frame failing does not mean the socket is gone, and if it is not, the
    /// daemon is still holding this attachment. Left holding it, the reader's
    /// next Connect is a *second* attach on that connection — which the daemon
    /// answers by closing the terminal it has and never replying to the new
    /// one, stranding this carrier in `attaching` with nothing left to answer
    /// it. It is appended behind whatever was already queued, but `end` has
    /// just let that attachment go, so those frames write nothing and the
    /// detach is what reaches the socket next.
    private func endAfterWriteFailure(of attachment: String) {
        guard attachment == attachmentID else { return }
        submitDetach(of: attachment)
        end(reason: Self.writeFailureReason, code: nil)
    }

    // MARK: - The wait for an answer

    /// Stop waiting for an answer to `attachment` after the deadline, and say
    /// so rather than sitting in `attaching`.
    ///
    /// **The state it exists for is unreachable from the screen.** Every other
    /// ending has something that drives it — the daemon closes, a write fails,
    /// the socket drops. An attach whose answer is simply lost has none: the
    /// connection is up, so nothing fires; the carrier is busy, so `canAttach`
    /// refuses the reader's next attempt; and the tab shows "Opening a
    /// terminal…" with no control on it. The only way out was leaving the
    /// session and coming back, which is not a thing a screen should require.
    ///
    /// **Comfortably longer than the daemon's own deadline, and that is the
    /// whole sizing rule.** The Mac bounds its attach at `ATTACH_DEADLINE`, 20
    /// seconds, and answers with a close when it expires — so any deadline at
    /// or under that number races an answer that is still coming, and the phone
    /// would call a slow-but-honest open a failure while the daemon is
    /// mid-`tmux`. Worse than a wrong word on screen: the detach below would
    /// then chase an attach the daemon is still opening, which is the ordering
    /// the outbound chain exists to prevent. 30 seconds sits past the daemon's
    /// bound with margin for the round trip, so this can only fire for an answer
    /// that is genuinely lost — which is the only case it is for.
    ///
    /// A `Task.sleep`, not a `Timer`: the carrier is a `@MainActor` object with
    /// a cancellable handle already in every other position like this, and a
    /// timer would need a runloop mode nobody would remember to check.
    private func startAttachDeadline(for attachment: String) {
        attachDeadline?.cancel()
        let wait = attachDeadlineDuration
        attachDeadline = Task { @MainActor [weak self] in
            try? await Task.sleep(for: wait)
            guard !Task.isCancelled, let self else { return }
            // The id and the phase, like every other handler here — and both
            // are belt to the cancellation's braces rather than the mechanism.
            // What actually keeps a previous attachment's deadline from ending
            // the terminal that replaced it is `end` and `attach` cancelling
            // this task, and there is no suspension between the check above and
            // the guard below for a cancellation to slip through. They are kept
            // because the cost is two comparisons on a path that runs once per
            // terminal, and because this class's rule is that a frame — or a
            // deadline — is acted on only after its attachment has been asked
            // for again. `testAPreviousAttachsDeadlineCannotEndTheTerminalThat-
            // ReplacedIt` holds the invariant; it takes removing the
            // cancellation *and* this line to break it.
            guard self.attachmentID == attachment, case .attaching = self.phase else { return }
            self.endAfterUnansweredAttach(of: attachment)
        }
    }

    /// End an attach that was never answered, and let the daemon go.
    ///
    /// **The detach is not optional.** The answer being lost does not mean the
    /// attach was: the daemon may well be holding an attachment for a phone that
    /// has stopped waiting for it, and the reader's next Connect would then be a
    /// second attach against a session that already has one. Telling it now is
    /// what makes the retry this leaves offered actually work.
    ///
    /// **No close code, deliberately.** `CloseCode` is the daemon's vocabulary —
    /// every case is a string the Mac really sends — and nothing was received
    /// here, so there is no code to state. Borrowing one would put a claim about
    /// the daemon in `lastClose` that this side cannot support: `tmuxUnavailable`
    /// says the Mac could not open a terminal, and what actually happened is that
    /// it never said anything at all. `nil` is also what leaves `Reconnect`
    /// offered — see `canOfferReconnect` — which is the control this ending
    /// exists to produce, and the same choice a failed write already makes.
    private func endAfterUnansweredAttach(of attachment: String) {
        submitDetach(of: attachment)
        end(reason: Self.unansweredAttachReason, code: nil)
    }

    /// What an unanswered attach says. Actionable without inventing a cause: the
    /// Mac's silence is the only fact this side has, and naming a reason for it
    /// would be a guess the reader cannot check.
    private static let unansweredAttachReason =
        "The Mac did not answer the terminal request. Try again."

    /// How long to wait, and the seam a test drives it through.
    ///
    /// Injected per carrier rather than through a shared static: the deadline is
    /// this terminal's, tests run in parallel, and a global would make one test's
    /// impatience another's flake.
    private var attachDeadlineDuration: Duration {
        #if DEBUG
            return attachDeadlineForTesting ?? Self.attachDeadline
        #else
            return Self.attachDeadline
        #endif
    }

    /// Past the daemon's own `ATTACH_DEADLINE` (20s) with margin for the round
    /// trip. See `startAttachDeadline` for why it may not be shorter.
    private static let attachDeadline: Duration = .seconds(30)

    #if DEBUG
        /// Test seam: shorten the wait for an answer.
        ///
        /// A test for this deadline is a test about *time*, and the alternatives
        /// are both worse than a seam: half a minute of wall clock in the suite,
        /// or a fake clock threaded through a class whose every other deadline
        /// is the transport's. Set before `attach`; it applies to the attach it
        /// precedes.
        var attachDeadlineForTesting: Duration?
    #endif

    /// What a failed write says. Deliberately not the transport's own words: a
    /// `URLError` code is not a sentence, and every one of them means the same
    /// actionable thing here. No close code, because there is no daemon in this
    /// answer — and because `nil` is what leaves `Reconnect` offered, which is
    /// the right control for a link that may well come back.
    private static let writeFailureReason = "This terminal lost its connection to the Mac."

    #if DEBUG
        /// Waits until the outbound chain is quiet. Tests await this fact
        /// rather than sleeping for it: the chain runs when the scheduler gets
        /// to it, so any fixed sleep is a bet a loaded machine eventually
        /// loses.
        ///
        /// The loop is not decoration. Awaiting the tail once covers only the
        /// frames submitted before the call, and the chain submits frames of
        /// its own — a failed write sends a detach — so a single await returns
        /// against a wire that is still filling.
        ///
        /// **Never call this from inside a send.** While a write is running the
        /// tail *is* the task doing the writing, so awaiting it waits on
        /// itself: no trap, no diagnostic, just a process that stops.
        func settleForTesting() async {
            while let tail = outbound {
                await tail.value
                if outbound == tail { return }
            }
        }
    #endif

    // MARK: - Endings

    private func detachAfterProtocolFault(_ reason: String) {
        if let id = attachmentID {
            submitDetach(of: id)
        }
        end(reason: reason, code: .protocolError)
    }

    /// The single place the terminal stops. Clears the attachment id first, so
    /// any frame still in flight for it is ignored rather than drawn into
    /// whatever comes next.
    private func end(reason: String, code: CloseCode?) {
        let wasAttached = phase.isAttached
        guard wasAttached || phase.isBusy else { return }
        // Every ending passes through here — the daemon's close, the reader's
        // detach, a dropped socket, a failed write — so cancelling the wait for
        // an answer once, here, covers all of them. A deadline outliving its
        // terminal would end the *next* one, which is what the id guard in
        // `startAttachDeadline` catches even so.
        attachDeadline?.cancel()
        attachDeadline = nil
        attachmentID = nil
        inputCredit = 0
        outputCredit = 0
        pendingInput.removeAll(keepingCapacity: false)
        lastClose = code
        phase = .ended(reason: reason, wasAttached: wasAttached)
    }

    // MARK: - Helpers

    private func clearTranscript() {
        transcript.removeAll(keepingCapacity: true)
        transcriptIsTruncated = false
    }

    /// Keep the tail, and remember that a head was thrown away.
    ///
    /// A plain byte cut, which is the honest shape for what this buffer is. It
    /// used to be cut at an offset a mirror of SwiftTerm's parser said was not
    /// inside an escape sequence — and a syntactically clean cut is still not a
    /// self-contained stream: the alternate screen, the scroll region, the SGR
    /// colours and the cursor were all established before it. Replayed into a
    /// fresh emulator the tail comes back mis-coloured or blank whatever byte it
    /// starts on, so the scan bought a smaller class of wrongness at the price
    /// of tracking another library's parser forever. What actually makes a
    /// reattached pane correct is the daemon's repaint: every attach captures
    /// the pane's whole current screen and cursor and sends it as one chunk.
    private func appendTranscript(_ bytes: [UInt8]) {
        transcript.append(contentsOf: bytes)
        guard transcript.count > Self.maxTranscriptBytes else { return }
        transcript.removeFirst(transcript.count - Self.trimmedTranscriptBytes)
        transcriptIsTruncated = true
    }

    /// Keep geometry inside what the daemon accepts. A phone in a strange layout
    /// state can report a zero or enormous size; sending it would be refused as
    /// a protocol error, and losing the terminal over a transient layout is a
    /// worse answer than drawing at the nearest usable size.
    nonisolated static func clampSize(cols: Int, rows: Int) -> (cols: Int, rows: Int) {
        (
            cols: min(max(cols, Wire.Terminal.minCols), Wire.Terminal.maxCols),
            rows: min(max(rows, Wire.Terminal.minRows), Wire.Terminal.maxRows)
        )
    }

    /// The sentence the view shows. The daemon's own text is preferred — it is
    /// the end that knows — with a fallback per code so an older or newer daemon
    /// that sends a bare code still says something actionable.
    nonisolated static func describe(code: CloseCode?, reason: String) -> String {
        let trimmed = reason.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        switch code {
        case .sessionNotHosted: return "That session is not running on the Mac."
        case .sessionExited: return "The session ended."
        case .identityMismatch: return "The session changed under the terminal."
        case .tmuxUnavailable: return "The Mac could not open a terminal for that session."
        case .attachmentLimit: return "The Mac already has as many terminals open as it allows."
        case .superseded: return "Another terminal took over this session."
        case .sessionBusy: return "The session's previous terminal is still closing; try again."
        case .notAuthorised: return "This connection may not open a terminal."
        case .protocolError: return "The terminal was closed after a protocol error."
        case .slowConsumer: return "The terminal stalled and was closed."
        case .windowChanged: return "The session's window changed."
        case .detached: return "Disconnected."
        case nil: return "The terminal closed."
        }
    }
}

// MARK: - The connection underneath

/// The paired connection, narrowed to the five members the terminal touches.
///
/// **Not a second transport.** It is the same socket the timeline rides — see
/// this file's opening note — described by what the carrier asks of it rather
/// than by what it is. Narrowing it is what lets the carrier be driven whole in
/// a test: attach, output, credit, close, drop. The flow-control and identity
/// rules above are the ones most worth a test and the least reachable without
/// one, since exercising them otherwise needs a real Mac at the far end.
@MainActor
protocol PairedConnection: AnyObject {
    var isConnected: Bool { get }
    /// Whether the daemon granted *this* connection shell-equivalent authority.
    /// Never assumed: absent means absent.
    var servesTerminal: Bool { get }
    var onTerminal: ((ServerMessage) -> Void)? { get set }
    var onDisconnected: (() -> Void)? { get set }
    /// Throwing, where the rest of the app sends and forgets. The terminal is
    /// the one caller that cannot forget: a frame it loses is not a refresh
    /// that will come round again, it is two ends that have silently stopped
    /// agreeing — so the failure has to be something this carrier can see.
    func send(_ message: ClientMessage) async throws
}

extension DaemonConnection: PairedConnection {
    var isConnected: Bool { phase.isConnected }
    var servesTerminal: Bool { capabilities?.servesTerminal == true }
}
