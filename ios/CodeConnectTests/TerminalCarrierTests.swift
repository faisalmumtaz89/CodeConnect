import SwiftTerm
import XCTest

@testable import CodeConnect

/// The phone's half of the live terminal: the wire shape the daemon parses, and
/// the rules that keep a terminal from lying about being live.
///
/// The credit protocol is the part worth testing without a Mac. Exceeding a
/// window is a protocol error the daemon closes the terminal for, so a frame
/// this side gets wrong is not a cosmetic bug — it is a terminal that dies
/// mid-keystroke, and the numbers below are the ones `protocol/src/ws.rs`
/// enforces.
final class TerminalCarrierTests: XCTestCase {

    private func encoded(_ message: ClientMessage) throws -> [String: Any] {
        let data = try JSONEncoder().encode(message)
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

    private func decoded(_ json: String) throws -> ServerMessage {
        try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
    }

    // MARK: The wire the daemon parses

    func testAttachCarriesUIDAndGeometryTheDaemonAccepts() throws {
        let json = try encoded(
            .terminalAttach(
                attachmentID: "att-1", sessionUID: "01KZ", cols: 100, rows: 40,
                outputCredit: Wire.Terminal.initialOutputCredit))
        XCTAssertEqual(json["type"] as? String, "terminal_attach")
        XCTAssertEqual(json["attachment_id"] as? String, "att-1")
        // **By uid, never by tmux name.** A name is reused; a uid is not, and
        // attaching by name is how a terminal ends up on a different agent.
        XCTAssertEqual(json["session_uid"] as? String, "01KZ")
        XCTAssertEqual(json["cols"] as? Int, 100)
        XCTAssertEqual(json["rows"] as? Int, 40)
        XCTAssertEqual(json["output_credit"] as? Int, 65536)

        // "The daemon accepts" is the claim in the name, so it is checked
        // against the daemon's own bounds rather than asserted. Geometry
        // outside them is answered with `bad geometry` and the terminal never
        // opens, and every size this app sends comes through `clampSize`.
        let daemon = try daemonTerminalConstants()
        for (cols, rows) in [(0, 0), (1, 1), (9_999, 9_999), (100, 40), (2, 2)] {
            let clamped = TerminalCarrier.clampSize(cols: cols, rows: rows)
            XCTAssertGreaterThanOrEqual(clamped.cols, try XCTUnwrap(daemon["TERMINAL_MIN_COLS"]))
            XCTAssertLessThanOrEqual(clamped.cols, try XCTUnwrap(daemon["TERMINAL_MAX_COLS"]))
            XCTAssertGreaterThanOrEqual(clamped.rows, try XCTUnwrap(daemon["TERMINAL_MIN_ROWS"]))
            XCTAssertLessThanOrEqual(clamped.rows, try XCTUnwrap(daemon["TERMINAL_MAX_ROWS"]))
        }
    }

    func testInputIsBase64AndCreditIsAnUnsignedByteCount() throws {
        let input = try encoded(.terminalInput(attachmentID: "att-1", base64: "aGk="))
        XCTAssertEqual(input["type"] as? String, "terminal_input")
        XCTAssertEqual(input["data"] as? String, "aGk=")

        let credit = try encoded(.terminalCredit(attachmentID: "att-1", bytes: 4096))
        XCTAssertEqual(credit["type"] as? String, "terminal_credit")
        XCTAssertEqual(credit["bytes"] as? Int, 4096)

        let detach = try encoded(.terminalDetach(attachmentID: "att-1"))
        XCTAssertEqual(detach["type"] as? String, "terminal_detach")
        XCTAssertEqual(detach["attachment_id"] as? String, "att-1")

        let resize = try encoded(.terminalResize(attachmentID: "att-1", cols: 80, rows: 24))
        XCTAssertEqual(resize["type"] as? String, "terminal_resize")
        XCTAssertEqual(resize["cols"] as? Int, 80)
        XCTAssertEqual(resize["rows"] as? Int, 24)
    }

    // MARK: What the daemon says back

    func testServerTerminalFramesDecode() throws {
        guard
            case .terminalAttached(let id, let credit, let chunkBound, let creditCeiling) =
                try decoded(
                #"{"type":"terminal_attached","attachment_id":"att-1","input_credit":32768}"#)
        else { return XCTFail("expected terminal_attached") }
        XCTAssertEqual(id, "att-1")
        XCTAssertEqual(credit, 32768)
        // A daemon that predates the advertised ceilings says nothing about
        // them, and *nothing* is what has to arrive here: a decoder that
        // substituted this build's constants would make "the Mac said 16 KiB"
        // and "the Mac has never heard of the field" the same fact, and the
        // fallback would then be unfalsifiable.
        XCTAssertNil(chunkBound)
        XCTAssertNil(creditCeiling)

        guard
            case .terminalAttached(_, _, let advertisedChunk, let advertisedCeiling) =
                try decoded(
                    #"{"type":"terminal_attached","attachment_id":"att-1","input_credit":32768,"max_chunk_bytes":16384,"max_outstanding_credit":262144}"#
                )
        else { return XCTFail("expected terminal_attached") }
        XCTAssertEqual(advertisedChunk, 16_384)
        XCTAssertEqual(advertisedCeiling, 262_144)

        guard
            case .terminalOutput(_, let base64) = try decoded(
                #"{"type":"terminal_output","attachment_id":"att-1","data":"aGk="}"#)
        else { return XCTFail("expected terminal_output") }
        XCTAssertEqual(String(data: Data(base64Encoded: base64)!, encoding: .utf8), "hi")

        guard
            case .terminalClosed(_, let code, let reason) = try decoded(
                #"{"type":"terminal_closed","attachment_id":"att-1","code":"session_exited","reason":"the session ended"}"#
            )
        else { return XCTFail("expected terminal_closed") }
        XCTAssertEqual(code, "session_exited")
        XCTAssertEqual(reason, "the session ended")
    }

    /// A close from a daemon this build has never heard of must still end the
    /// terminal, with the daemon's own words — the alternative is a dead
    /// terminal that looks live because its code did not parse.
    ///
    /// Driven through a carrier, because that is where the failure lives. The
    /// decoding below says the frame parses; only this says the terminal ends.
    @MainActor
    func testAnUnknownCloseCodeStillEndsTheTerminal() async throws {
        let (carrier, connection) = try await attached(to: "A")
        let id = try XCTUnwrap(connection.attachmentID)
        connection.deliver(
            .terminalClosed(
                attachmentID: id, code: "from_the_future", reason: "something new"))

        XCTAssertFalse(carrier.phase.isAttached)
        guard case .ended(let reason, let wasAttached) = carrier.phase else {
            return XCTFail("an unparsed close code left the carrier in \(carrier.phase)")
        }
        XCTAssertEqual(reason, "something new")
        XCTAssertTrue(wasAttached)
        // Unknown to this build, so nothing is known about whether a retry
        // would work — and `nil` is what leaves the control offered.
        XCTAssertNil(carrier.lastClose)
    }

    /// And the frame itself decodes, with the daemon's text preserved.
    func testAnUnknownCloseCodeStillCarriesTheDaemonsReason() throws {
        guard case .terminalClosed(_, let code, let reason) = try decoded(
            #"{"type":"terminal_closed","attachment_id":"a","code":"from_the_future","reason":"something new"}"#
        ) else { return XCTFail("expected terminal_closed") }
        XCTAssertNil(TerminalCarrier.CloseCode(rawValue: code))
        XCTAssertEqual(
            TerminalCarrier.describe(code: TerminalCarrier.CloseCode(rawValue: code), reason: reason),
            "something new")
    }

    /// And a bare code with no text still says something a reader can act on.
    ///
    /// Over `allCases`, not over a list written here: a hand-kept list makes
    /// "every" mean "every one somebody remembered", and the code added without
    /// a sentence is exactly the one nobody would think to add to it.
    func testEveryCloseCodeHasASentence() {
        for code in TerminalCarrier.CloseCode.allCases {
            let sentence = TerminalCarrier.describe(code: code, reason: "   ")
            XCTAssertFalse(sentence.isEmpty, "\(code) has no sentence")
        }
        XCTAssertFalse(TerminalCarrier.describe(code: nil, reason: "").isEmpty)
        XCTAssertGreaterThanOrEqual(TerminalCarrier.CloseCode.allCases.count, 10)
    }

    /// Retry is offered only where it could work. A session that ended does not
    /// come back by asking again, and a connection that may not open a terminal
    /// will not change its mind — a Reconnect button on those is a dead control.
    func testOnlyRecoverableClosesOfferRetry() {
        XCTAssertTrue(TerminalCarrier.CloseCode.tmuxUnavailable.isRetryable)
        XCTAssertTrue(TerminalCarrier.CloseCode.attachmentLimit.isRetryable)
        XCTAssertTrue(TerminalCarrier.CloseCode.superseded.isRetryable)
        XCTAssertTrue(TerminalCarrier.CloseCode.sessionBusy.isRetryable)
        XCTAssertTrue(TerminalCarrier.CloseCode.slowConsumer.isRetryable)
        XCTAssertTrue(TerminalCarrier.CloseCode.windowChanged.isRetryable)
        XCTAssertFalse(TerminalCarrier.CloseCode.sessionExited.isRetryable)
        XCTAssertFalse(TerminalCarrier.CloseCode.sessionNotHosted.isRetryable)
        XCTAssertFalse(TerminalCarrier.CloseCode.notAuthorised.isRetryable)
        XCTAssertFalse(TerminalCarrier.CloseCode.detached.isRetryable)
    }

    /// The two closes the supersede rule introduced, from the wire through to
    /// the control the reader is left with.
    ///
    /// **Both are retryable, and that is the whole point of them.** A terminal
    /// this phone lost because its own reconnecting self took it back closes as
    /// `superseded`; an attach that arrived while the previous terminal was
    /// still closing is refused as `session_busy` and succeeds a moment later.
    /// Either one landing on a screen with no Reconnect is a terminal the reader
    /// cannot reopen without leaving the session and coming back.
    func testTheSupersedeClosesParseAndLeaveARetryOffered() throws {
        for (raw, code) in [
            ("superseded", TerminalCarrier.CloseCode.superseded),
            ("session_busy", TerminalCarrier.CloseCode.sessionBusy),
        ] {
            guard case .terminalClosed(_, let wire, let reason) = try decoded(
                #"{"type":"terminal_closed","attachment_id":"att-1","code":"\#(raw)","reason":""}"#
            ) else { return XCTFail("expected terminal_closed") }
            XCTAssertEqual(TerminalCarrier.CloseCode(rawValue: wire), code)
            XCTAssertTrue(code.isRetryable, "\(raw) left the reader no way back in")
            // The daemon may send these with no text at all — it is a code, not
            // a sentence — so the fallback is what the reader actually gets.
            XCTAssertEqual(
                TerminalCarrier.describe(code: code, reason: reason),
                raw == "superseded"
                    ? "Another terminal took over this session."
                    : "The session's previous terminal is still closing; try again.")
        }

        // And the code that used to cover the supersede case now means only the
        // Mac-wide cap, so its sentence may no longer name the session.
        XCTAssertEqual(
            TerminalCarrier.describe(code: .attachmentLimit, reason: ""),
            "The Mac already has as many terminals open as it allows.")
    }

    // MARK: The ceilings the daemon advertises

    /// A terminal is bounded by the numbers **its own daemon stated**, not by
    /// the ones this build was compiled with.
    ///
    /// Both bounds are enforced as protocol errors at the Mac, so a hand-
    /// mirrored pair was a trap: raising either there would have killed the
    /// terminal of every phone still carrying the old number. `terminal_attached`
    /// carries them, and this is what carrying them has to mean — the chunk cap
    /// bounds what `terminal_input` may hold, and the ceiling clamps every
    /// credit figure this side keeps.
    @MainActor
    func testTheAdvertisedCeilingsBoundInputChunksAndCredit() async throws {
        let (carrier, connection) = try await attached(
            to: "A", inputCredit: 8_192, maxChunkBytes: 1_024, maxOutstandingCredit: 65_536)

        carrier.send(ArraySlice([UInt8](repeating: UInt8(ascii: "x"), count: 3_000)))
        await carrier.settleForTesting()

        // 1024 + 1024 + 952: the advertised cap, not this build's 16 KiB, which
        // would have put the whole paste in one frame.
        XCTAssertEqual(connection.writtenInputChunkSizes, [1_024, 1_024, 952])
        XCTAssertEqual(connection.writtenInput.count, 3_000)
    }

    /// And the credit ceiling is the daemon's too — on the grant it opens the
    /// terminal with, and on every one after it.
    ///
    /// Observable through what the carrier is willing to *send*: credit it
    /// clamped away is credit it will not spend, so a ceiling that was ignored
    /// shows up as a paste going out in one go that should have waited.
    ///
    /// The attach's own figure is clamped rather than faulted, and that is not
    /// an inconsistency with the grants below. A `terminal_attached` naming a
    /// window wider than its own ceiling is a Mac contradicting itself about
    /// numbers it is offering; the conservative reading costs nothing and keeps
    /// the terminal. A *grant* past the ceiling is different — it is a request
    /// to buffer past what both ends agreed to hold, which is the thing the
    /// ceiling exists to refuse.
    @MainActor
    func testTheAdvertisedCreditCeilingClampsWhatMayBeSent() async throws {
        // The daemon offers far more than its own ceiling — a Mac whose ceiling
        // is lower than the figure it granted, which is exactly the skew the
        // advertisement exists for. Still at or above the output credit this
        // phone granted in its attach, because below that the ceiling is
        // unusable and falls back instead — see the test after next.
        let ceiling = Wire.Terminal.initialOutputCredit
        let (carrier, connection) = try await attached(
            to: "A", inputCredit: 250_000, maxChunkBytes: nil, maxOutstandingCredit: ceiling)

        carrier.send(ArraySlice([UInt8](repeating: UInt8(ascii: "y"), count: 2 * Int(ceiling))))
        await carrier.settleForTesting()
        // Clamped at the attach: only the ceiling's worth may go, and the rest
        // waits. Unclamped, the whole paste would already be on the wire.
        XCTAssertEqual(connection.writtenInput.count, Int(ceiling))

        // And the ceiling is a ceiling rather than a fresh budget on every
        // grant: the window is empty again, so the whole of it may be granted
        // back, and the rest of the paste goes on exactly that.
        let id = try XCTUnwrap(connection.attachmentID)
        connection.deliver(.terminalCredit(attachmentID: id, bytes: ceiling))
        await carrier.settleForTesting()
        XCTAssertEqual(connection.writtenInput.count, 2 * Int(ceiling))
        XCTAssertTrue(carrier.phase.isAttached, "a grant inside the ceiling ended the terminal")
    }

    // MARK: The bounds this end is the one that enforces

    // `MAX_TERMINAL_CHUNK_BYTES` is documented on the wire as enforced by the
    // *receiver*, and a credit ceiling is only a ceiling if the end that opened
    // the window checks what comes back against it. Both were documented and
    // neither was enforced here: an oversized frame was drawn, and an overdraft
    // clamped the ledger to zero and was then granted straight back — so a
    // daemon that ignored the window was handed an unbounded one.
    //
    // The honest daemon keeps both, so none of this is reachable from a Mac
    // that is working. It is reachable from one that is broken, and from one
    // that is not a Mac at all, which is the case receiver-enforcement is for.

    /// A burst larger than the chunk bound ends the terminal instead of being
    /// drawn.
    @MainActor
    func testAnOversizedOutputBurstEndsTheTerminalAsAFault() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try connection.deliverOutput(
            [UInt8](repeating: UInt8(ascii: "a"), count: Wire.Terminal.maxChunkBytes + 1))
        await carrier.settleForTesting()

        XCTAssertEqual(carrier.lastClose, .protocolError)
        XCTAssertTrue(
            try XCTUnwrap(carrier.lastClose).isRetryable,
            "a fault the reader cannot retry out of leaves them with a dead tab")
        XCTAssertFalse(carrier.phase.isAttached)
        XCTAssertTrue(
            connection.writtenKinds.contains("terminal_detach"),
            "the Mac was never told to let the terminal go")
        // None of it was drawn or counted: an oversized frame is refused, not
        // truncated.
        XCTAssertTrue(carrier.transcript.isEmpty)
        XCTAssertEqual(carrier.totalOutputBytes, 0)
        guard case .ended(let reason, _) = carrier.phase else {
            return XCTFail("the terminal did not end")
        }
        XCTAssertFalse(reason.isEmpty, "the fault gave the reader nothing to act on")
    }

    /// And the bound checked is **the one this attachment's daemon named**, not
    /// the number this build was compiled with — the same rule the input side
    /// already followed.
    @MainActor
    func testTheAdvertisedChunkBoundIsWhatOutputIsCheckedAgainst() async throws {
        let (carrier, connection) = try await attached(to: "A", maxChunkBytes: 1_024)
        // Far under the mirrored 16 KiB, and one byte over what this daemon
        // said it would send.
        try connection.deliverOutput([UInt8](repeating: UInt8(ascii: "a"), count: 1_025))
        await carrier.settleForTesting()
        XCTAssertEqual(carrier.lastClose, .protocolError)
    }

    /// In both directions: a daemon that advertises a larger bound may use it,
    /// and a phone that refused would kill a healthy terminal on its first busy
    /// screen.
    @MainActor
    func testAChunkTheAdvertisedBoundAllowsStillStreams() async throws {
        let (carrier, connection) = try await attached(to: "A", maxChunkBytes: 32 * 1024)
        let burst = [UInt8](repeating: UInt8(ascii: "a"), count: 20 * 1024)
        try connection.deliverOutput(burst)
        await carrier.settleForTesting()
        XCTAssertTrue(carrier.phase.isAttached)
        XCTAssertEqual(carrier.transcript.count, burst.count)
    }

    /// Output past the window this phone granted ends the terminal.
    ///
    /// The chunk bound cannot catch this one, which is why it is a second
    /// check rather than the same check: a daemon may advertise room for a
    /// frame far larger than the credit it has been given, and the credit is
    /// the bound that says how much this side has agreed to hold.
    @MainActor
    func testOutputBeyondTheGrantedCreditEndsTheTerminalAsAFault() async throws {
        let (carrier, connection) = try await attached(to: "A", maxChunkBytes: 256 * 1024)
        try connection.deliverOutput(
            [UInt8](
                repeating: UInt8(ascii: "a"),
                count: Int(Wire.Terminal.initialOutputCredit) + 1))
        await carrier.settleForTesting()

        XCTAssertEqual(carrier.lastClose, .protocolError)
        XCTAssertTrue(connection.writtenKinds.contains("terminal_detach"))
        // The old behaviour is the thing being refused: the ledger was clamped
        // to zero, the bytes were fed anyway, and the overdraft was granted
        // straight back — so nothing was ever bounded.
        XCTAssertTrue(carrier.transcript.isEmpty, "the overdraft was drawn anyway")
        XCTAssertEqual(carrier.totalOutputBytes, 0)
    }

    /// And output that fills the window exactly is not an overdraft. Off by one
    /// here would close a terminal the daemon ran perfectly.
    @MainActor
    func testOutputExactlyFillingTheGrantedCreditStillStreams() async throws {
        let (carrier, connection) = try await attached(to: "A", maxChunkBytes: 256 * 1024)
        let full = Int(Wire.Terminal.initialOutputCredit)
        try connection.deliverOutput([UInt8](repeating: UInt8(ascii: "a"), count: full))
        await carrier.settleForTesting()
        XCTAssertTrue(carrier.phase.isAttached)
        XCTAssertEqual(carrier.totalOutputBytes, full)
        // And the window reopened, which is what lets the pane keep drawing.
        XCTAssertTrue(connection.writtenKinds.contains("terminal_credit"))
    }

    /// A credit grant that would push the input window past the ceiling ends
    /// the terminal.
    ///
    /// The Mac closes this phone's terminal for the mirror-image frame — an
    /// `output credit overflow` — so clamping it here made this side the only
    /// one that let it pass.
    @MainActor
    func testACreditGrantPastTheCeilingEndsTheTerminalAsAFault() async throws {
        let (carrier, connection) = try await attached(to: "A")
        let id = try XCTUnwrap(connection.attachmentID)
        // One byte more than the window still has room for.
        let over = Wire.Terminal.maxOutstandingCredit - Wire.Terminal.initialInputCredit + 1
        connection.deliver(.terminalCredit(attachmentID: id, bytes: over))
        await carrier.settleForTesting()

        XCTAssertEqual(carrier.lastClose, .protocolError)
        XCTAssertTrue(try XCTUnwrap(carrier.lastClose).isRetryable)
        XCTAssertTrue(connection.writtenKinds.contains("terminal_detach"))
    }

    /// A grant that fills the ceiling exactly is legitimate, and spendable.
    @MainActor
    func testACreditGrantThatFillsTheCeilingStillStreams() async throws {
        let (carrier, connection) = try await attached(to: "A")
        let id = try XCTUnwrap(connection.attachmentID)
        let exact = Wire.Terminal.maxOutstandingCredit - Wire.Terminal.initialInputCredit
        connection.deliver(.terminalCredit(attachmentID: id, bytes: exact))
        await carrier.settleForTesting()
        XCTAssertTrue(carrier.phase.isAttached)

        carrier.send(ArraySlice([UInt8](repeating: UInt8(ascii: "x"), count: 200_000)))
        await carrier.settleForTesting()
        XCTAssertEqual(connection.writtenInput.count, 200_000)
    }

    /// **A grant near `UInt32.max` is a fault, not a small number.** The clamp
    /// this replaced summed with `addingReportingOverflow(_:).partialValue`,
    /// so a grant chosen to wrap arrived as whatever was left over and was
    /// then clamped to the ceiling — which is to say the largest possible
    /// overdraft read as a perfectly ordinary one.
    @MainActor
    func testACreditGrantThatWouldWrapIsAFaultRatherThanASmallNumber() async throws {
        let (carrier, connection) = try await attached(to: "A")
        let id = try XCTUnwrap(connection.attachmentID)
        connection.deliver(.terminalCredit(attachmentID: id, bytes: .max))
        await carrier.settleForTesting()
        XCTAssertEqual(carrier.lastClose, .protocolError)
    }

    /// And the ceiling a grant is checked against is **this attachment's
    /// daemon's**, like every other bound here.
    @MainActor
    func testTheAdvertisedCreditCeilingIsWhatAGrantIsCheckedAgainst() async throws {
        let ceiling = Wire.Terminal.initialOutputCredit
        let (carrier, connection) = try await attached(
            to: "A", inputCredit: 0, maxChunkBytes: nil, maxOutstandingCredit: ceiling)
        let id = try XCTUnwrap(connection.attachmentID)
        // Comfortably inside the mirrored 256 KiB, and one byte outside what
        // this daemon said its own ceiling was.
        connection.deliver(.terminalCredit(attachmentID: id, bytes: ceiling + 1))
        await carrier.settleForTesting()
        XCTAssertEqual(carrier.lastClose, .protocolError)
    }

    /// A daemon that says nothing about either bound is a daemon this app
    /// treats exactly as it did before they existed. The mirrored constants are
    /// the fallback, and this is what pins them as still being one.
    @MainActor
    func testAbsentCeilingsFallBackToTheMirroredConstants() async throws {
        let (carrier, connection) = try await attached(
            to: "A", inputCredit: 200_000, maxChunkBytes: nil, maxOutstandingCredit: nil)

        carrier.send(ArraySlice([UInt8](repeating: UInt8(ascii: "z"), count: 20_000)))
        await carrier.settleForTesting()

        XCTAssertEqual(
            connection.writtenInputChunkSizes,
            [Wire.Terminal.maxChunkBytes, 20_000 - Wire.Terminal.maxChunkBytes])
    }

    /// A credit ceiling **below the output credit this phone already granted**
    /// is not a bound it can work under, and is treated as absent.
    ///
    /// The phone's attach grants 64 KiB before it can know any ceiling — the
    /// ceiling arrives in the answer. `grantOutputCredit` measures its headroom
    /// against the ceiling, so one below that grant is zero headroom while the
    /// grant is outstanding, and returning the grant is precisely what the
    /// headroom would have been for. The pane draws 64 KiB and goes quiet
    /// forever, with nothing on screen to say why.
    ///
    /// The daemon promises never to send such a number. This is what happens
    /// when it does anyway.
    @MainActor
    func testACreditCeilingBelowThePhonesOwnGrantIsTreatedAsAbsent() async throws {
        let unusable = Wire.Terminal.initialOutputCredit / 2
        let (carrier, connection) = try await attached(
            to: "A", inputCredit: 200_000, maxChunkBytes: nil, maxOutstandingCredit: unusable)

        // Honoured, the attach's own grant would have been clamped to 32 KiB
        // and the paste would stop there. Fallen back, the mirrored 256 KiB
        // stands and the whole paste goes.
        carrier.send(ArraySlice([UInt8](repeating: UInt8(ascii: "v"), count: 200_000)))
        await carrier.settleForTesting()
        XCTAssertEqual(connection.writtenInput.count, 200_000)

        // And output keeps being acknowledged, which is the failure this rule
        // is really about: a ceiling under the outstanding grant makes every
        // credit frame zero, and a daemon that is never told what was drawn
        // stops sending.
        try connection.deliverOutput("drawn")
        await carrier.settleForTesting()
        XCTAssertTrue(
            connection.writtenKinds.contains("terminal_credit"),
            "no credit was returned, so the pane would have gone quiet")
    }

    // MARK: The wait for an answer

    /// An attach the Mac never answers has to end itself.
    ///
    /// Nothing else can end it: the connection is up so no drop fires, the
    /// carrier is busy so the reader's next attempt is refused, and the tab
    /// shows "Opening a terminal…" with no control on it. Before the deadline
    /// the only way out was leaving the session and coming back.
    @MainActor
    func testAnAttachTheMacNeverAnswersEndsItselfWithARetryOffered() async throws {
        let connection = StubConnection()
        let carrier = TerminalCarrier(connection: connection)
        carrier.attachDeadlineForTesting = .milliseconds(50)
        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        // The premise: the daemon says nothing at all, ever.
        XCTAssertTrue(carrier.phase.isBusy)

        try await Task.sleep(for: .milliseconds(300))

        guard case .ended(let reason, let wasAttached) = carrier.phase else {
            return XCTFail("an unanswered attach left the carrier in \(carrier.phase)")
        }
        XCTAssertEqual(reason, "The Mac did not answer the terminal request. Try again.")
        // Never live, so the tab shows the connect card rather than a snapshot
        // of a pane that never drew anything.
        XCTAssertFalse(wasAttached)
        // No close code: nothing was received, so there is nothing to state —
        // and nil is what leaves Reconnect offered.
        XCTAssertNil(carrier.lastClose)
        XCTAssertTrue(carrier.canAttach, "the reader cannot try again")

        // The daemon may be holding the attachment it never answered for. Left
        // holding it, the reader's next Connect is a second attach on a
        // connection that already has one.
        await carrier.settleForTesting()
        XCTAssertEqual(connection.writtenKinds.last, "terminal_detach")
    }

    /// And an attach that *is* answered never hears from its deadline again.
    @MainActor
    func testAnAnsweredAttachOutlivesItsDeadline() async throws {
        let connection = StubConnection()
        let carrier = TerminalCarrier(connection: connection)
        carrier.attachDeadlineForTesting = .milliseconds(50)
        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let id = try XCTUnwrap(connection.attachmentID)
        connection.deliver(
            .terminalAttached(
                attachmentID: id, inputCredit: Wire.Terminal.initialInputCredit,
                maxChunkBytes: nil, maxOutstandingCredit: nil))

        try await Task.sleep(for: .milliseconds(300))

        XCTAssertTrue(
            carrier.phase.isAttached, "the deadline ended a terminal that had already opened")
        try connection.deliverOutput("still live")
        XCTAssertEqual(Array(carrier.transcript(forRun: "A")), Array("still live".utf8))
    }

    /// A deadline may never end the terminal that replaced the attach it was
    /// set for.
    ///
    /// The carrier outlives every attachment, and a reader who gives up on one
    /// run and opens another does it inside a second. If the first attach's
    /// deadline could still speak, the terminal it kills is the one the reader
    /// is looking at — attributed to a run they have already left.
    @MainActor
    func testAPreviousAttachsDeadlineCannotEndTheTerminalThatReplacedIt() async throws {
        let connection = StubConnection()
        let carrier = TerminalCarrier(connection: connection)
        carrier.attachDeadlineForTesting = .milliseconds(50)
        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()

        // Before A's deadline: the reader gives up on A and opens B, whose own
        // wait is long and has barely started. B is deliberately left
        // *unanswered*, because that is the only state A's deadline could do
        // damage in — and the state the id guard is the sole defence of. Were B
        // already attached, the phase check alone would turn A's deadline away
        // and this would be testing that instead.
        carrier.attachDeadlineForTesting = .seconds(30)
        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        await carrier.settleForTesting()
        XCTAssertEqual(carrier.standing(forRun: "B"), .mine)

        try await Task.sleep(for: .milliseconds(300))

        // Still waiting on B's own answer, with B's own deadline to end it. A's
        // deadline has long since expired and has nothing to say about it.
        XCTAssertTrue(carrier.phase.isBusy, "A's deadline ended B's attach: \(carrier.phase)")
        XCTAssertEqual(carrier.standing(forRun: "B"), .mine)
    }

    /// A ceiling learned from one attachment may not bound the next one's
    /// frames. The carrier outlives every terminal it opens — it is the app's,
    /// not the tab's — so a number left standing from a Mac that is no longer
    /// on the other end is a frame sized for the wrong daemon.
    @MainActor
    func testEachAttachStartsFromTheMirroredConstantsAgain() async throws {
        let (carrier, connection) = try await attached(
            to: "A", inputCredit: 8_192, maxChunkBytes: 1_024, maxOutstandingCredit: 65_536)
        carrier.detach(reason: "Done.")
        await carrier.settleForTesting()

        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let retry = try XCTUnwrap(connection.attachmentID)
        connection.deliver(
            .terminalAttached(
                attachmentID: retry, inputCredit: 200_000, maxChunkBytes: nil,
                maxOutstandingCredit: nil))

        let before = connection.writtenInput.count
        carrier.send(ArraySlice([UInt8](repeating: UInt8(ascii: "w"), count: 4_000)))
        await carrier.settleForTesting()
        // One frame, at the default cap — not four at the previous
        // attachment's 1 KiB.
        XCTAssertEqual(connection.writtenInput.count - before, 4_000)
        XCTAssertEqual(connection.writtenInputChunkSizes.suffix(1), [4_000])
    }

    // MARK: Geometry

    /// A phone mid-rotation can report a zero or enormous grid. Sending it would
    /// be refused as a protocol error and cost the terminal; drawing at the
    /// nearest usable size is the honest answer.
    func testGeometryIsClampedToWhatTheDaemonAccepts() {
        XCTAssertEqual(TerminalCarrier.clampSize(cols: 0, rows: 0).cols, Wire.Terminal.minCols)
        XCTAssertEqual(TerminalCarrier.clampSize(cols: 0, rows: 0).rows, Wire.Terminal.minRows)
        XCTAssertEqual(
            TerminalCarrier.clampSize(cols: 9_999, rows: 9_999).cols, Wire.Terminal.maxCols)
        XCTAssertEqual(
            TerminalCarrier.clampSize(cols: 9_999, rows: 9_999).rows, Wire.Terminal.maxRows)
        let ordinary = TerminalCarrier.clampSize(cols: 100, rows: 40)
        XCTAssertEqual(ordinary.cols, 100)
        XCTAssertEqual(ordinary.rows, 40)
    }

    // MARK: Capability

    /// The terminal is shell-equivalent authority, so the daemon answers per
    /// connection. Absent means absent: a build that cannot read the flag must
    /// hide the affordance, never invent it.
    func testTerminalCapabilityIsOffByDefault() {
        XCTAssertFalse(Capabilities().servesTerminal)
        XCTAssertTrue(Capabilities(extra: ["terminal_pty": .bool(true)]).servesTerminal)
        XCTAssertFalse(Capabilities(extra: ["terminal_pty": .bool(false)]).servesTerminal)
    }

    /// The flow-control numbers are a contract with `protocol/src/ws.rs`, and
    /// this reads that file rather than restating it.
    ///
    /// A test that compares a Swift constant to a literal spelled out beside it
    /// cannot fail for the one reason it exists: if the Mac's number changes
    /// alone, both sides of the assertion stay equal and terminals start dying
    /// mid-keystroke against a green suite. The daemon's source is in this
    /// repository, so the contract can be checked against the other party to
    /// it.
    func testFlowControlConstantsMatchTheDaemon() throws {
        let daemon = try daemonTerminalConstants()
        XCTAssertEqual(Wire.Terminal.maxChunkBytes, daemon["MAX_TERMINAL_CHUNK_BYTES"])
        XCTAssertEqual(
            Int(Wire.Terminal.initialOutputCredit), daemon["TERMINAL_INITIAL_OUTPUT_CREDIT"])
        XCTAssertEqual(
            Int(Wire.Terminal.maxOutstandingCredit), daemon["TERMINAL_MAX_OUTSTANDING_CREDIT"])
        XCTAssertEqual(Wire.Terminal.minCols, daemon["TERMINAL_MIN_COLS"])
        XCTAssertEqual(Wire.Terminal.maxCols, daemon["TERMINAL_MAX_COLS"])
        XCTAssertEqual(Wire.Terminal.minRows, daemon["TERMINAL_MIN_ROWS"])
        XCTAssertEqual(Wire.Terminal.maxRows, daemon["TERMINAL_MAX_ROWS"])
        // Granted by the daemon rather than by this phone, so nothing here
        // would notice it moving — but it is what the carrier is handed at
        // attach, and it bounds the first keystrokes that may be sent.
        XCTAssertEqual(
            Int(Wire.Terminal.initialInputCredit), daemon["TERMINAL_INITIAL_INPUT_CREDIT"])
        // The attachment id goes on the wire as a UUID string. A longer one is
        // refused outright, which would close every terminal this app opens.
        let identifier = UUID().uuidString
        XCTAssertLessThanOrEqual(
            identifier.utf8.count, try XCTUnwrap(daemon["MAX_ATTACHMENT_ID_BYTES"]))
    }

    /// The daemon's terminal constants, read from its own source.
    ///
    /// Out of the test bundle, not off the checkout. `daemon-ws.rs` beside this
    /// file is a symlink to `mac/protocol/src/ws.rs`, so the build copies in
    /// whatever that file says today and the two can never be separately
    /// edited — a copy would drift, which is the failure this replaces. Reading
    /// the working tree directly instead is what the simulator cannot do: the
    /// checkout here lives under `~/Documents`, and a process on the far side
    /// of that privacy boundary does not get an error, it gets no answer at
    /// all — measured, the test hung rather than failed.
    private func daemonTerminalConstants() throws -> [String: Int] {
        let source = try XCTUnwrap(
            Bundle(for: Self.self).url(forResource: "daemon-ws", withExtension: "rs"),
            "the daemon's protocol source is not in the test bundle")
        let text = try String(contentsOf: source, encoding: .utf8)
        var constants: [String: Int] = [:]
        for line in text.split(separator: "\n") {
            let parts = line.split(separator: " ", omittingEmptySubsequences: true)
            guard parts.count >= 5, parts[0] == "pub", parts[1] == "const" else { continue }
            let name = parts[2].split(separator: ":").first.map(String.init) ?? ""
            guard name.hasPrefix("TERMINAL_") || name.hasPrefix("MAX_TERMINAL_")
                || name == "MAX_ATTACHMENT_ID_BYTES"
            else { continue }
            let value = line.split(separator: "=").dropFirst().joined(separator: "=")
                .replacingOccurrences(of: ";", with: "")
            let factors = value.split(separator: "*").compactMap {
                Int($0.trimmingCharacters(in: .whitespaces))
            }
            guard !factors.isEmpty else { continue }
            constants[name] = factors.reduce(1, *)
        }
        // A parse that quietly found nothing is a test that quietly stopped
        // checking, which is the failure this whole rewrite is about.
        XCTAssertGreaterThanOrEqual(
            constants.count, 9, "the daemon's constants did not parse: \(constants)")
        return constants
    }

    // MARK: Whose terminal it is

    /// One carrier serves the whole app, because the terminal has to survive
    /// the tab being rebuilt. So a screen has to ask whose terminal it is
    /// holding before rendering anything of it: a tab that reads the phase
    /// alone shows another run's live pane under its own label, sends its
    /// keystrokes there, and — the carrier being busy — can never open the
    /// terminal it is actually for.
    func testALiveTerminalBelongsOnlyToTheRunItWasOpenedFor() {
        let live = TerminalCarrier.Phase.attached(since: Date())
        XCTAssertEqual(TerminalCarrier.standing(phase: live, serving: "A", forRun: "A"), .mine)
        XCTAssertEqual(
            TerminalCarrier.standing(phase: live, serving: "A", forRun: "B"),
            .heldByAnotherRun(sessionUID: "A"))
        // An attach in flight is just as taken: the answer cannot wait for it.
        XCTAssertEqual(
            TerminalCarrier.standing(phase: .attaching, serving: "A", forRun: "B"),
            .heldByAnotherRun(sessionUID: "A"))
    }

    /// A terminal that has ended holds nothing, whichever run it belonged to.
    /// The next run to ask gets it without ending anybody's — and without
    /// inheriting the snapshot or the close reason of the run that finished.
    func testAClosedTerminalLeavesTheCarrierFreeForAnyRun() {
        let closed = TerminalCarrier.Phase.ended(reason: "The session ended.", wasAttached: true)
        XCTAssertEqual(TerminalCarrier.standing(phase: .idle, serving: nil, forRun: "B"), .free)
        XCTAssertEqual(TerminalCarrier.standing(phase: closed, serving: "A", forRun: "B"), .free)
        XCTAssertEqual(TerminalCarrier.standing(phase: closed, serving: "A", forRun: "A"), .mine)
    }

    /// The liveness strip renders from the phase, and it is the one element on
    /// that screen that must never be wrong. A screen for another run is told
    /// `idle` — not live, nothing to draw — however live the carrier is.
    @MainActor
    func testAScreenForAnotherRunIsNeverToldTheTerminalIsLive() async throws {
        let (carrier, _) = try await attached(to: "A")
        XCTAssertTrue(carrier.phase(forRun: "A").isAttached)
        XCTAssertEqual(carrier.phase(forRun: "B"), .idle)
    }

    /// The transcript is replayed verbatim into a fresh emulator every time the
    /// tab is re-entered or the phone is rotated. Handed over unasked, it
    /// replays one run's output into another run's terminal under that run's
    /// label — output that never came from the agent named on the screen.
    @MainActor
    func testATranscriptIsOnlyEverReplayedIntoItsOwnRun() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try connection.deliverOutput("hello")
        XCTAssertEqual(Array(carrier.transcript(forRun: "A")), Array("hello".utf8))
        XCTAssertTrue(carrier.transcript(forRun: "B").isEmpty)
    }

    /// SwiftUI routinely builds a replacement view before dismantling the one
    /// it replaces. An unstamped teardown clears whichever sink is current, so
    /// the terminal that is now live draws nothing — which reads as a hung
    /// agent rather than as a bug on this side.
    @MainActor
    func testATornDownTerminalCannotSilenceTheOneThatReplacedIt() async throws {
        let (carrier, connection) = try await attached(to: "A")
        let replaced = NSObject()
        let current = NSObject()
        var drawn: [UInt8] = []
        var reachedReplaced = false
        carrier.deliverOutput(to: replaced) { _ in reachedReplaced = true }
        carrier.deliverOutput(to: current) { drawn.append(contentsOf: $0) }
        carrier.stopDeliveringOutput(to: replaced)
        try connection.deliverOutput("still here")
        XCTAssertEqual(drawn, Array("still here".utf8))
        XCTAssertFalse(reachedReplaced)
    }

    /// The daemon allows one terminal per connection, so opening this run's
    /// ends the other run's. Both halves happen or neither does.
    @MainActor
    func testATakeoverEndsTheOtherRunsTerminalAndOpensThisOne() async throws {
        let (carrier, connection) = try await attached(to: "A")
        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        // Standing and phase are this carrier's own state, and change on the
        // call. What reaches the daemon is awaited: the frames are written by
        // the outbound chain, which is the whole point of it.
        XCTAssertEqual(carrier.standing(forRun: "B"), .mine)
        XCTAssertTrue(carrier.phase.isBusy)
        await carrier.settleForTesting()
        XCTAssertTrue(
            connection.sent.contains { if case .terminalDetach = $0 { return true } else { return false } })
    }

    /// A link that drops between the screen offering the takeover and the tap
    /// arriving must not cost the other run its terminal and open nothing in
    /// its place. Nothing is taken unless it can be given.
    @MainActor
    func testATakeoverEndsNothingWhenTheLinkIsDown() async throws {
        let (carrier, connection) = try await attached(to: "A")
        connection.isConnected = false
        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        XCTAssertTrue(carrier.phase.isAttached)
        XCTAssertEqual(carrier.standing(forRun: "B"), .heldByAnotherRun(sessionUID: "A"))
    }

    // MARK: One ordered stream out

    /// The daemon allows one terminal per connection and reads a second attach
    /// as a protocol violation: it closes the terminal it already holds and
    /// answers the new attachment with nothing at all — no `terminal_attached`,
    /// no `terminal_closed`, ever. So a takeover's detach has to be *written*
    /// before its attach, not merely submitted first. Reversed, the reader
    /// loses the terminal they had and this carrier waits on a reply that is
    /// never coming.
    @MainActor
    func testATakeoversDetachIsWrittenBeforeItsAttach() async throws {
        let (carrier, connection) = try await attached(to: "A")
        // The detach is made the slow frame. What this stands in for is not
        // quite what a socket does: the real reorder happens *below* the send,
        // in a `URLSessionWebSocketTask` that does not serialise concurrent
        // writes, which nothing in this process can reproduce. Perturbing
        // completion order here reaches the same verdict — frames started
        // independently come out in the order they finish — and it is the
        // regression this file can actually hold.
        connection.writeDelay = { message in
            if case .terminalDetach = message { return .milliseconds(50) }
            return .zero
        }
        let written = expectation(description: "the takeover's two frames land")
        written.expectedFulfillmentCount = 2
        connection.onWrite = { _ in written.fulfill() }

        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        await fulfillment(of: [written], timeout: 5)
        XCTAssertEqual(
            connection.writtenKinds,
            ["terminal_attach", "terminal_detach", "terminal_attach"])
    }

    /// A paste longer than one chunk is split, and the chunks are keystrokes.
    /// Delivered out of order they are not slow, they are a different thing
    /// typed — into a shell.
    @MainActor
    func testAPasteReachesTheDaemonInTheOrderItWasTyped() async throws {
        let (carrier, connection) = try await attached(to: "A")
        // The earlier the chunk, the slower it is written, so any path that
        // starts writes independently delivers this backwards.
        connection.writeDelay = { message in
            guard case .terminalInput(_, let base64) = message,
                let first = Data(base64Encoded: base64)?.first
            else { return .zero }
            return .milliseconds(Int(UInt8(ascii: "c") - first) * 20)
        }

        // Deliberately larger than the window the daemon opens, so the last
        // chunk waits for credit and is sent from a *second* flush. Ordering
        // that only held within one flush would be no ordering at all: a paste
        // does not stop at the window.
        var typed = [UInt8](repeating: UInt8(ascii: "a"), count: Wire.Terminal.maxChunkBytes)
        typed += [UInt8](repeating: UInt8(ascii: "b"), count: Wire.Terminal.maxChunkBytes)
        typed += [UInt8](repeating: UInt8(ascii: "c"), count: Wire.Terminal.maxChunkBytes)
        XCTAssertGreaterThan(typed.count, Int(Wire.Terminal.initialInputCredit))

        carrier.send(ArraySlice(typed))
        await carrier.settleForTesting()
        XCTAssertEqual(
            connection.writtenInput.count, Int(Wire.Terminal.initialInputCredit),
            "the window the daemon opened was exceeded")

        let id = try XCTUnwrap(connection.attachmentID)
        connection.deliver(
            .terminalCredit(attachmentID: id, bytes: UInt32(Wire.Terminal.maxChunkBytes)))
        await carrier.settleForTesting()

        XCTAssertEqual(connection.writtenInput.count, typed.count)
        XCTAssertEqual(connection.writtenInput, typed)
    }

    /// A frame that never reaches the daemon desynchronises the two ends in
    /// silence: the daemon waits on credit it was not granted while this phone
    /// goes on drawing Live. That is the one lie the liveness strip exists to
    /// forbid, so a failed write ends the terminal.
    @MainActor
    func testAFailedWriteEndsTheTerminalInsteadOfLeavingItLive() async throws {
        let (carrier, connection) = try await attached(to: "A")
        connection.writeFailure = StubConnection.WriteFailed()
        // Output arrives and is drawn, so the credit for it is returned — and
        // that grant is the frame whose loss would strand the daemon.
        try connection.deliverOutput("hello")
        await carrier.settleForTesting()

        XCTAssertFalse(carrier.phase.isAttached)
        guard case .ended(let reason, let wasAttached) = carrier.phase else {
            return XCTFail("a dropped frame left the carrier in \(carrier.phase)")
        }
        XCTAssertTrue(wasAttached, "it had been live, and the ending has to say so")
        XCTAssertFalse(reason.isEmpty)
        // No close code: no daemon spoke here. That is also what leaves
        // `Reconnect` offered, which is the right control for a link that may
        // well come back.
        XCTAssertNil(carrier.lastClose)
    }

    /// A lost frame does not mean a lost socket. If the link carries on, the
    /// daemon is still holding this attachment — and the reader's next Connect
    /// would be a *second* attach on that connection, which the daemon answers
    /// by closing the terminal it holds and never replying to the new one. So
    /// the ending has to reach the daemon too, not only the screen.
    @MainActor
    func testAFailedWriteStillTellsTheDaemonToLetTheTerminalGo() async throws {
        let (carrier, connection) = try await attached(to: "A")
        connection.failNextWrite = true
        try connection.deliverOutput("hello")
        await carrier.settleForTesting()

        XCTAssertFalse(carrier.phase.isAttached)
        XCTAssertTrue(
            connection.writtenKinds.contains("terminal_detach"),
            "the daemon was never told, so it is still holding the attachment")
    }

    /// The same for keystrokes. Input that was spent against the window but
    /// never sent leaves the daemon expecting bytes that are not coming, and
    /// the phone showing a prompt that will not answer.
    @MainActor
    func testAFailedInputWriteEndsTheTerminal() async throws {
        let (carrier, connection) = try await attached(to: "A")
        connection.writeFailure = StubConnection.WriteFailed()
        carrier.send(text: "ls\n")
        await carrier.settleForTesting()
        XCTAssertFalse(carrier.phase.isAttached)
    }

    /// And nothing follows a terminal that has ended: no further credit is
    /// granted for output that arrives after, because output for an attachment
    /// this carrier no longer holds is not drawn at all.
    @MainActor
    func testNothingIsGrantedAfterAFailedWriteEndsTheTerminal() async throws {
        let (carrier, connection) = try await attached(to: "A")
        connection.writeFailure = StubConnection.WriteFailed()
        try connection.deliverOutput("hello")
        await carrier.settleForTesting()

        connection.writeFailure = nil
        let before = connection.sent.count
        try connection.deliverOutput("more")
        await carrier.settleForTesting()
        XCTAssertEqual(connection.sent.count, before)
        XCTAssertEqual(carrier.totalOutputBytes, Array("hello".utf8).count)
    }

    /// A frame that fails takes the frames already queued behind it with it.
    ///
    /// The chain is FIFO, so by the time the first frame of a paste reaches the
    /// socket the rest are already behind it. If that one fails the terminal is
    /// over — and the followers are the remainder of what was typed, which
    /// reaches the pane anyway unless something stops it, with the cleanup
    /// detach appended behind *them* and arriving last of all. Half a pasted
    /// command, run against a prompt this phone has stopped showing, is the
    /// thing this forbids.
    ///
    /// Asserted on what was *attempted*, not on what landed: the failed frame
    /// never lands either, so a test that reads only the daemon's side cannot
    /// tell a frame that was refused from one that was tried and lost.
    @MainActor
    func testFramesQueuedBehindAFailedOneAreDroppedAndTheDetachGoesNext() async throws {
        let (carrier, connection) = try await attached(to: "A")
        // One frame is lost and the socket carries on, which is the case where
        // the followers would otherwise still reach the daemon.
        connection.failNextWrite = true

        // Two frames, submitted before either is written — no `await` between
        // them, so the chain is two deep when the first one fails. This is the
        // shape of a paste split across chunks.
        carrier.send(text: "rm -rf ")
        carrier.send(text: "/important\n")
        await carrier.settleForTesting()

        XCTAssertEqual(
            connection.attemptedKinds,
            ["terminal_attach", "terminal_input", "terminal_detach"],
            "a keystroke frame was written for a terminal that had already ended")
        XCTAssertEqual(
            connection.writtenInput, [],
            "the failed frame's followers were typed into the shell after the failure")
        XCTAssertFalse(carrier.phase.isAttached)
    }

    /// And the rule is about the attachment, not about failure. Keystrokes
    /// queued for A are not written once the reader has taken the terminal for
    /// B, however healthy the socket is: the phone stopped being A's keyboard
    /// the moment that terminal ended, and a frame landing in A's pane after
    /// that is typing into a terminal nobody is looking at.
    ///
    /// No delay is needed to arrange this. `send` and `takeOver` both run to
    /// completion on the main actor, and a submitted frame is not written until
    /// the actor is released — so the takeover always happens with A's
    /// keystrokes queued and unwritten.
    @MainActor
    func testKeystrokesQueuedForATerminalTheReaderLeftAreNeverWritten() async throws {
        let (carrier, connection) = try await attached(to: "A")
        carrier.send(text: "rm -rf /important\n")
        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        await carrier.settleForTesting()

        XCTAssertEqual(
            connection.writtenInput, [],
            "A's keystrokes reached the Mac after the reader left A's terminal")
        XCTAssertEqual(
            connection.attemptedKinds,
            ["terminal_attach", "terminal_detach", "terminal_attach"])
    }

    /// Serialising this carrier's writes must not serialise the phone.
    ///
    /// Two claims, and neither is about the transport. **Inbound is not gated
    /// by outbound**: output that lands while a write is parked is drawn,
    /// counted and credited then, not after the wire clears — a terminal that
    /// stopped painting until the last keystroke was acknowledged would stutter
    /// on every key. And **the main actor keeps running**: the loop below is
    /// main-actor work interleaved with an outstanding write, so an
    /// implementation that waited rather than suspended could not reach the
    /// assertions at all. That half fails by hanging rather than by asserting,
    /// which is worth saying out loud — it guards a shape, and no line of the
    /// carrier as written reverts to it.
    @MainActor
    func testAWriteInFlightStopsNeitherTheEmulatorNorThePhone() async throws {
        let (carrier, connection) = try await attached(to: "A")
        connection.writeDelay = { _ in .milliseconds(400) }
        let began = expectation(description: "the input write is under way")
        connection.onWriteBegan = { message in
            if case .terminalInput = message { began.fulfill() }
        }

        var drawn: [UInt8] = []
        let emulator = NSObject()
        carrier.deliverOutput(to: emulator) { drawn.append(contentsOf: $0) }

        carrier.send(text: "x")
        await fulfillment(of: [began], timeout: 5)
        XCTAssertTrue(connection.writtenInput.isEmpty, "the write has not landed yet")

        // Main-actor work, interleaved with the parked write.
        var turns = 0
        for _ in 0..<200 {
            await Task.yield()
            turns += 1
        }
        XCTAssertEqual(turns, 200)

        // And the inbound path ran throughout, while that same write was still
        // outstanding.
        try connection.deliverOutput("still painting")
        XCTAssertEqual(drawn, Array("still painting".utf8))
        XCTAssertEqual(carrier.totalOutputBytes, Array("still painting".utf8).count)
        XCTAssertTrue(connection.writtenInput.isEmpty, "the write is still outstanding")
        await carrier.settleForTesting()
    }

    /// A frame can still be *inside its write* after the carrier has moved on,
    /// and its failure belongs to the attachment it was written for. Charged to
    /// whichever attachment happens to be current, a stale credit frame closes
    /// a terminal that was never at fault: run B's tab reads `Terminal closed`
    /// about an attach it never got an answer to, and the detach lands behind
    /// B's attach, so the daemon opens a tmux client and tears it down again.
    ///
    /// **Inside the write, not merely queued.** A frame still queued when the
    /// reader takes the terminal is dropped before it can fail at all — that is
    /// `testKeystrokesQueuedForATerminalTheReaderLeftAreNeverWritten`. The
    /// window this covers is the one after that check and before the socket
    /// answers: the write is a suspension, so the carrier can move on
    /// underneath a frame that has already passed it.
    @MainActor
    func testAStaleFramesFailureDoesNotEndTheTerminalThatReplacedIt() async throws {
        let (carrier, connection) = try await attached(to: "A")
        let first = try XCTUnwrap(connection.attachmentID)
        // A's credit frame is parked on the wire, and fails there.
        connection.writeDelay = { message in
            if case .terminalCredit = message { return .milliseconds(150) }
            return .zero
        }
        connection.failNextWrite = true
        let onTheWire = expectation(description: "A's credit frame is inside its write")
        connection.onWriteBegan = { message in
            if case .terminalCredit = message { onTheWire.fulfill() }
        }
        try connection.deliverOutput("A's output")
        await fulfillment(of: [onTheWire], timeout: 5)

        // B takes the terminal while A's frame is still in flight.
        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let second = try XCTUnwrap(connection.attachmentID)
        XCTAssertNotEqual(first, second)

        // B is untouched: still its own, still waiting on its own attach.
        XCTAssertEqual(carrier.standing(forRun: "B"), .mine)
        XCTAssertTrue(carrier.phase.isBusy, "B was ended by a frame belonging to A")
        // A's attach, the takeover's detach of A, B's attach — and nothing
        // else. A second detach here is one written for B on A's behalf, which
        // is the daemon opening a tmux client and tearing it down again.
        XCTAssertEqual(
            connection.writtenKinds,
            ["terminal_attach", "terminal_detach", "terminal_attach"])
        let detached = connection.sent.compactMap { message -> String? in
            guard case .terminalDetach(let id) = message else { return nil }
            return id
        }
        XCTAssertEqual(detached, [first], "the detach named an attachment that never failed")
    }

    /// Geometry from an emulator this carrier is not attached to is not
    /// recorded. `lastSize` is what the next attach opens at, so a size taken
    /// from a view that is being torn down — or one measured before it is on
    /// screen — would become the window the agent's TUI is told to draw for.
    @MainActor
    func testGeometryIsNotRememberedFromATerminalThatIsNotAttached() async throws {
        let (carrier, connection) = try await attached(to: "A")
        carrier.resize(cols: 120, rows: 50)
        await carrier.settleForTesting()
        XCTAssertEqual(carrier.lastSize.cols, 120)

        carrier.detach(reason: "Backgrounded.")
        await carrier.settleForTesting()
        carrier.resize(cols: 2, rows: 2)
        XCTAssertEqual(carrier.lastSize.cols, 120, "a detached emulator set the next attach's size")
        XCTAssertEqual(carrier.lastSize.rows, 50)

        carrier.attach(sessionUID: "A", cols: carrier.lastSize.cols, rows: carrier.lastSize.rows)
        await carrier.settleForTesting()
        guard
            case .terminalAttach(_, _, let cols, let rows, _) = try XCTUnwrap(connection.sent.last)
        else { return XCTFail("expected an attach") }
        XCTAssertEqual(cols, 120)
        XCTAssertEqual(rows, 50)
    }

    // MARK: What an attach may throw away

    /// A reattach the daemon refuses must not cost the reader the screen they
    /// were looking at. The snapshot frame is drawn from the transcript, so
    /// emptying it when the attach is *sent* replaces a readable dead terminal
    /// with an empty one every time a foreground reattach lands on a session
    /// that has since exited.
    @MainActor
    func testARefusedReattachKeepsTheSnapshotItWasShowing() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try connection.deliverOutput("the last thing it said")
        carrier.detach(reason: "Backgrounded.")
        await carrier.settleForTesting()

        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let retry = try XCTUnwrap(connection.attachmentID, "the carrier sent no second attach")
        connection.deliver(
            .terminalClosed(
                attachmentID: retry, code: "session_exited", reason: "the session ended"))

        XCTAssertEqual(
            Array(carrier.transcript(forRun: "A")), Array("the last thing it said".utf8))
    }

    /// And a *successful* attach spends it. What the new attachment sends is
    /// the pane as it stands now, and bytes kept from the last one would be
    /// replayed above it as though the two were one screen.
    @MainActor
    func testAConfirmedReattachStartsFromTheDaemonsFreshScreen() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try connection.deliverOutput("stale")
        carrier.detach(reason: "Backgrounded.")
        await carrier.settleForTesting()

        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let retry = try XCTUnwrap(connection.attachmentID, "the carrier sent no second attach")
        connection.deliver(
            .terminalAttached(
                attachmentID: retry, inputCredit: Wire.Terminal.initialOutputCredit,
                maxChunkBytes: nil, maxOutstandingCredit: nil))
        XCTAssertTrue(carrier.transcript(forRun: "A").isEmpty)

        try connection.deliverOutput("fresh")
        XCTAssertEqual(Array(carrier.transcript(forRun: "A")), Array("fresh".utf8))
    }

    /// Keeping a transcript across a refused attach must never become keeping
    /// it across runs. One run's output under another run's label is the thing
    /// this whole carrier is arranged to prevent, so a change of run empties it
    /// at once rather than waiting for an answer.
    @MainActor
    func testAttachingADifferentRunEmptiesTheTranscriptAtOnce() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try connection.deliverOutput("A's output")
        carrier.takeOver(sessionUID: "B", cols: 80, rows: 24)
        XCTAssertTrue(carrier.transcript.isEmpty)
        XCTAssertTrue(carrier.transcript(forRun: "B").isEmpty)
        await carrier.settleForTesting()
    }

    // MARK: Freshness that cannot freeze

    /// The transcript is capped, so past the cap its own count is a constant
    /// while output is still arriving. A clock driven from that count stops at
    /// the minute the buffer filled, and an hour later the strip and the
    /// snapshot stamp both state a dead terminal was last live then — the exact
    /// lie the strip exists to prevent.
    @MainActor
    func testTheOutputCountKeepsRisingAfterTheTranscriptStopsGrowing() async throws {
        let (carrier, connection) = try await attached(to: "A")
        // Past the 256 KiB cap, in frames the chunk bound allows.
        let batch = [UInt8](repeating: 0x61, count: 320 * 1024)
        try deliver(batch, to: connection)
        let filled = carrier.transcript.count
        let counted = carrier.totalOutputBytes
        XCTAssertEqual(counted, batch.count)
        XCTAssertLessThan(
            filled, counted, "the transcript is not capped, so this test measures nothing")

        try deliver(batch, to: connection)
        // Not equality: what the trim leaves behind is a policy, and where in
        // its cycle a batch happens to end is arithmetic rather than fact. The
        // fact is that this count stays under the cap while the byte count
        // doubles, so nothing may read freshness off it.
        XCTAssertLessThanOrEqual(carrier.transcript.count, 256 * 1024)
        XCTAssertEqual(carrier.totalOutputBytes, counted + batch.count)
    }

    /// The freshness fact is sampled **at the carrier**, and the byte counter it
    /// is derived from is not observable at all.
    ///
    /// A screen that observed the counter would have its body re-run on every
    /// chunk — up to a hundred times a second on a busy pane, on the main
    /// thread, against the emulator's own drawing — to redraw a clock that shows
    /// whole seconds. So the counter is `@ObservationIgnored` and the stamp is
    /// what a view watches. Asserted by tracking rather than by reading the
    /// attribute, because the attribute is not the fact: what matters is that
    /// output does not invalidate a reader of the count, and does invalidate a
    /// reader of the stamp.
    @MainActor
    func testTheByteCounterIsNotObservableAndTheStampIs() async throws {
        let (carrier, connection) = try await attached(to: "A")

        let counted = Invalidation()
        withObservationTracking { _ = carrier.totalOutputBytes } onChange: { counted.fire() }
        let stamped = Invalidation()
        withObservationTracking { _ = carrier.lastOutputAt } onChange: { stamped.fire() }

        try connection.deliverOutput("hello")

        XCTAssertGreaterThan(carrier.totalOutputBytes, 0, "no output arrived, so this tests nothing")
        XCTAssertFalse(
            counted.fired, "the byte counter invalidated its observers on a chunk of output")
        // The positive control. Without it the assertion above would pass just
        // as well against a carrier that never told anybody anything.
        XCTAssertTrue(stamped.fired, "nothing was told that output had arrived")
    }

    /// The stamp lands on the first chunk of an attachment and then moves at
    /// most once a second.
    ///
    /// Both halves matter. A terminal that has visibly drawn while the strip
    /// says nothing has arrived is the lie the strip exists to prevent, so the
    /// first chunk cannot wait for a tick; and every chunk after it moving the
    /// stamp is the per-chunk invalidation this sampling exists to remove.
    @MainActor
    func testTheStampLandsOnTheFirstChunkAndThenOnceASecond() async throws {
        let (carrier, connection) = try await attached(to: "A")
        // Nothing has been drawn yet, so there is nothing to claim.
        XCTAssertNil(carrier.lastOutputAt)

        try connection.deliverOutput("first")
        let first = try XCTUnwrap(carrier.lastOutputAt, "the first chunk left no stamp")

        for _ in 0..<20 { try connection.deliverOutput("more") }
        XCTAssertEqual(
            carrier.lastOutputAt, first, "the stamp moved inside its own sampling interval")

        // The one real second this file spends. The throttle is a wall-clock
        // fact with no seam to drive it, and a sample interval that could be
        // faked is not the one shipping.
        try await Task.sleep(for: .seconds(1.05))
        try connection.deliverOutput("later")
        let second = try XCTUnwrap(carrier.lastOutputAt)
        XCTAssertGreaterThan(
            second, first, "the stamp never moved again, so a live pane reads as stale")
    }

    /// And a fresh attachment starts with no stamp at all. `terminal_attached`
    /// replaces the screen with the pane as it stands now, and the previous
    /// attachment's stamp under it is a claim about bytes that are no longer
    /// there.
    @MainActor
    func testAConfirmedAttachClearsTheStampWithTheScreen() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try connection.deliverOutput("old")
        XCTAssertNotNil(carrier.lastOutputAt)

        carrier.detach(reason: "Done.")
        await carrier.settleForTesting()
        // A *refused* reattach keeps it, for the same reason it keeps the
        // transcript: the reader is still looking at that screen.
        XCTAssertNotNil(carrier.lastOutputAt)

        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let retry = try XCTUnwrap(connection.attachmentID)
        connection.deliver(
            .terminalAttached(
                attachmentID: retry, inputCredit: Wire.Terminal.initialInputCredit,
                maxChunkBytes: nil, maxOutstandingCredit: nil))
        XCTAssertNil(carrier.lastOutputAt, "a fresh screen kept the old screen's stamp")
    }

    // MARK: What the cap leaves behind

    /// The cap holds, whatever shape the output arrives in.
    ///
    /// This is the whole of what the trim promises now. It used to promise
    /// more — that the cut landed at an offset SwiftTerm's parser would be in
    /// `ground` at — and that promise was not worth its machinery: a
    /// syntactically clean cut is still not a self-contained stream, because
    /// the alternate screen, the scroll region, the colours and the cursor were
    /// all established before it. What makes a reattached pane correct is the
    /// daemon's repaint, and no cut this side chooses can substitute for one.
    @MainActor
    func testTheTranscriptStaysUnderItsCap() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try deliver(Array(String(repeating: "build output line\n", count: 40_000).utf8), to: connection)
        XCTAssertLessThanOrEqual(carrier.transcript.count, 256 * 1024)
        XCTAssertFalse(carrier.transcript.isEmpty, "the cap emptied the buffer instead of trimming it")
    }

    /// And it is a *byte* cap, so multi-byte content is bounded exactly as
    /// ASCII is — no scalar boundary is sought and none is claimed.
    @MainActor
    func testATrimmedTranscriptIsBoundedOnMultiByteContent() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try deliver(Array(String(repeating: "はい", count: 64 * 1024).utf8), to: connection)
        XCTAssertLessThanOrEqual(carrier.transcript.count, 256 * 1024)
        XCTAssertFalse(carrier.transcript.isEmpty)
    }

    /// A pane can emit bytes that are not text at all. Nothing here reads them,
    /// so a run of them is bounded and kept like anything else.
    @MainActor
    func testABinaryStreamStillLeavesATranscriptToReplay() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try deliver([UInt8](repeating: 0x80, count: 320 * 1024), to: connection)
        XCTAssertFalse(carrier.transcript.isEmpty)
        XCTAssertLessThanOrEqual(carrier.transcript.count, 256 * 1024)
    }

    /// **A trimmed buffer says that it is trimmed**, because the pane rebuilt
    /// from it begins in the middle of the session and nothing else on screen
    /// says so. Silent truncation is a partial tail presented as the whole of
    /// what was printed.
    @MainActor
    func testATrimmedTranscriptIsMarkedAsTruncated() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try deliver([UInt8](repeating: UInt8(ascii: "a"), count: 320 * 1024), to: connection)
        XCTAssertTrue(carrier.transcriptIsTruncated)
    }

    /// And one that fits does not, which is the half that keeps the mark
    /// meaningful: a marker on every pane is a marker nobody reads.
    @MainActor
    func testATranscriptUnderTheCapIsNotMarkedTruncated() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try deliver(Array("a short session\n".utf8), to: connection)
        XCTAssertFalse(carrier.transcript.isEmpty)
        XCTAssertFalse(carrier.transcriptIsTruncated)
    }

    /// A confirmed attach replaces the screen with the daemon's repaint, so the
    /// mark goes with the bytes it was about. Left standing, the next rebuilt
    /// pane would claim a gap above a screen the Mac had just painted whole.
    @MainActor
    func testAConfirmedAttachClearsTheTruncationMarkWithTheScreen() async throws {
        let (carrier, connection) = try await attached(to: "A")
        try deliver([UInt8](repeating: UInt8(ascii: "a"), count: 320 * 1024), to: connection)
        XCTAssertTrue(carrier.transcriptIsTruncated, "nothing was trimmed, so this tests nothing")

        carrier.detach(reason: "Done.")
        await carrier.settleForTesting()
        carrier.attach(sessionUID: "A", cols: 80, rows: 24)
        await carrier.settleForTesting()
        let retry = try XCTUnwrap(connection.attachmentID)
        connection.deliver(
            .terminalAttached(
                attachmentID: retry, inputCredit: Wire.Terminal.initialInputCredit,
                maxChunkBytes: nil, maxOutstandingCredit: nil))
        XCTAssertFalse(carrier.transcriptIsTruncated, "a repainted screen still claims a gap above it")
    }

    // MARK: The modifier SwiftTerm resets

    /// SwiftTerm clears `controlModifier` after applying it to one character
    /// and posts this as it does; the ctrl cap is re-read from that. A rename
    /// upstream would leave the cap lit after `^C`, so the next `d` typed for
    /// `^D` would reach the agent's pane as a literal `d` — silently, which is
    /// why the name is asserted rather than trusted.
    func testTheControlModifierResetKeepsTheNameSwiftTermPosts() {
        XCTAssertEqual(
            Notification.Name.terminalViewControlModifierReset.rawValue,
            "SwiftTerm.TerminalView.controlModifierReset")
    }

    // MARK: Driving a carrier without a Mac

    /// A carrier attached to `run` with the daemon's side stubbed, ready to be
    /// fed output as a real one would be.
    ///
    /// Awaited rather than immediate, because a frame reaches the wire through
    /// the carrier's outbound chain: the attach has not been written when
    /// `attach` returns, and the id the daemon has to answer is not knowable
    /// until it has.
    /// `maxChunkBytes` and `maxOutstandingCredit` default to absent, which is
    /// what a daemon that predates the advertised ceilings sends — so every test
    /// that does not care about them drives the fallback path, which is the one
    /// most installs are on.
    @MainActor
    private func attached(
        to run: String,
        inputCredit: UInt32 = Wire.Terminal.initialInputCredit,
        maxChunkBytes: UInt32? = nil,
        maxOutstandingCredit: UInt32? = nil
    ) async throws -> (TerminalCarrier, StubConnection) {
        let connection = StubConnection()
        let carrier = TerminalCarrier(connection: connection)
        carrier.attach(sessionUID: run, cols: 80, rows: 24)
        await carrier.settleForTesting()
        let id = try XCTUnwrap(connection.attachmentID, "the carrier sent no attach")
        connection.deliver(
            .terminalAttached(
                attachmentID: id, inputCredit: inputCredit, maxChunkBytes: maxChunkBytes,
                maxOutstandingCredit: maxOutstandingCredit))
        XCTAssertTrue(carrier.phase.isAttached)
        return (carrier, connection)
    }

    /// Feed `bytes` as a daemon that keeps the protocol would: split into
    /// frames no larger than the chunk bound.
    ///
    /// Needed because the carrier now *enforces* that bound, so a test about
    /// the transcript cap written as one 300 KiB frame would no longer be
    /// testing the cap — it would be testing the enforcement, and passing for
    /// the wrong reason.
    @MainActor
    private func deliver(_ bytes: [UInt8], to connection: StubConnection) throws {
        var start = 0
        while start < bytes.count {
            let end = min(start + Wire.Terminal.maxChunkBytes, bytes.count)
            try connection.deliverOutput(Array(bytes[start..<end]))
            start = end
        }
    }
    // MARK: - Offering a retry

    /// **The rule the liveness strip and the "Terminal closed" card both ask.**
    ///
    /// They used to decide separately, and on a close a retry cannot change they
    /// disagreed: the strip hid its `Reconnect` while the card, on the same
    /// screen, still offered one — and tapping it re-sent an attach into the same
    /// refusal. Both now read `mayOfferReconnect`, so the disagreement is not a
    /// bug to be re-fixed but a shape that cannot be written.
    ///
    /// This pins the rule. It does not pin that a caller asks it: SwiftUI draws
    /// that card into a single layer with no control and no text to inspect, so
    /// three separate attempts to assert on the rendered screen all passed with
    /// the defect restored — once measuring the reason sentence, once the
    /// liveness clock. They were deleted rather than kept as cover. The two call
    /// sites are one line each and read together.
    @MainActor
    func testOnlyACloseARetryCouldChangeMayOfferAReconnect() {
        let may = TerminalTabView.mayOfferReconnect

        // The defect, in one line: nothing blocking, this run's own terminal,
        // and a close the daemon will give again however many times it is asked.
        XCTAssertFalse(
            may(true, false, true, .sessionNotHosted),
            "a run the Mac is not hosting cannot be reconnected to by asking again")
        XCTAssertFalse(may(true, false, true, .sessionExited), "an ended session does not come back")
        XCTAssertFalse(may(true, false, true, .notAuthorised), "a refusal is not a retry")
        XCTAssertFalse(may(true, false, true, .detached), "the reader closed this one themselves")

        // And the cases a retry can change, so this cannot pass by refusing
        // everything.
        for close in [
            TerminalCarrier.CloseCode.tmuxUnavailable, .attachmentLimit, .protocolError,
            .slowConsumer, .windowChanged, .identityMismatch, .superseded, .sessionBusy,
        ] {
            XCTAssertTrue(
                may(true, false, true, close), "\(close.rawValue) is retryable and must be offered")
        }
        XCTAssertTrue(may(true, false, true, nil), "nothing has closed yet")

        // Another run's close says nothing about this screen: letting it speak
        // would hide this screen's only control because a different tab exited.
        XCTAssertTrue(
            may(true, false, false, .sessionExited),
            "another run's close must not hide this run's control")

        // The structural refusals, unchanged.
        XCTAssertFalse(may(false, false, true, nil), "a carrier that cannot attach offers nothing")
        XCTAssertFalse(may(true, true, true, nil), "a block hands over its own route")
    }

}

/// Whether an observation fired, in a box `withObservationTracking`'s
/// `@Sendable` handler may write to. The handler runs synchronously inside the
/// `willSet` that triggered it, and every writer and reader here is on the main
/// actor, so the unchecked conformance is a formality rather than a race.
private final class Invalidation: @unchecked Sendable {
    private(set) var fired = false
    func fire() { fired = true }
}

/// The paired connection with no socket under it.
///
/// The carrier asks five things of the connection, so a stub that records what
/// it was sent and hands back frames on demand drives the real attach, output,
/// credit and close paths — the ones that otherwise need a Mac at the far end
/// to reach at all.
///
/// Two things it does that a socket does: it can take its time over a write,
/// and it can refuse one. Both are needed to state anything about ordering or
/// about what a lost frame costs, and a transport that always answers instantly
/// and always succeeds can express neither.
@MainActor
private final class StubConnection: PairedConnection {
    /// A write that did not reach the daemon.
    struct WriteFailed: Error {}

    var isConnected = true
    var servesTerminal = true
    var onTerminal: ((ServerMessage) -> Void)?
    var onDisconnected: (() -> Void)?
    private(set) var sent: [ClientMessage] = []
    /// Thrown by every write once set, standing in for a socket that has gone
    /// away underneath a terminal that still believes it is live.
    var writeFailure: Error?
    /// Thrown by the next write only, standing in for the other case: one frame
    /// is lost and the socket carries on, so the daemon is still holding the
    /// attachment afterwards.
    var failNextWrite = false
    /// How long a write takes to land. Ordering is only observable when writes
    /// can finish out of the order they were started.
    var writeDelay: ((ClientMessage) -> Duration)?
    /// Called as each frame lands, so a test can await a count instead of a
    /// clock.
    var onWrite: ((ClientMessage) -> Void)?
    /// Called as a write *begins*, before its delay. What a test needs to know
    /// a write is genuinely outstanding rather than not yet started.
    var onWriteBegan: ((ClientMessage) -> Void)?

    func send(_ message: ClientMessage) async throws {
        attemptedKinds.append(Self.kind(of: message))
        onWriteBegan?(message)
        if let writeDelay { try? await Task.sleep(for: writeDelay(message)) }
        if failNextWrite {
            failNextWrite = false
            throw WriteFailed()
        }
        if let writeFailure { throw writeFailure }
        sent.append(message)
        onWrite?(message)
    }

    /// The wire `type` of each frame the carrier *asked* for, in the order it
    /// asked, whether or not the frame landed.
    ///
    /// `writtenKinds` is what reached the daemon; this is what was tried. Once
    /// a write has failed the difference between the two is the whole question:
    /// a frame the carrier declined to attempt never appears here, and one it
    /// attempted and lost appears here and nowhere else.
    private(set) var attemptedKinds: [String] = []

    /// The wire `type` of each frame written, in the order it was written.
    var writtenKinds: [String] { sent.map(Self.kind(of:)) }

    private static func kind(of message: ClientMessage) -> String {
        let data = try? JSONEncoder().encode(message)
        let json = data.flatMap { try? JSONSerialization.jsonObject(with: $0) }
        return (json as? [String: Any])?["type"] as? String ?? "unencodable"
    }

    /// Every keystroke written, reassembled in the order the frames landed. A
    /// paste is split across frames, so this is what the agent's pane actually
    /// receives — and what a reordering turns into a different thing typed.
    var writtenInput: [UInt8] {
        sent.reduce(into: [UInt8]()) { bytes, message in
            guard case .terminalInput(_, let base64) = message,
                let decoded = Data(base64Encoded: base64)
            else { return }
            bytes += decoded
        }
    }

    /// The decoded size of every `terminal_input` frame written, in order. What
    /// the chunk cap is actually about: the daemon closes the terminal for a
    /// frame over its bound, so the sizes are the fact and the reassembled bytes
    /// are not.
    var writtenInputChunkSizes: [Int] {
        sent.compactMap { message in
            guard case .terminalInput(_, let base64) = message else { return nil }
            return Data(base64Encoded: base64)?.count
        }
    }

    /// The id of the most recent attach this carrier sent. The most recent
    /// rather than the first, because a reattach is a new attachment and only
    /// frames carrying *its* id are ones the carrier will act on.
    var attachmentID: String? {
        for message in sent.reversed() {
            if case .terminalAttach(let id, _, _, _, _) = message { return id }
        }
        return nil
    }

    func deliver(_ message: ServerMessage) { onTerminal?(message) }

    func deliverOutput(_ bytes: [UInt8]) throws {
        let id = try XCTUnwrap(attachmentID, "the carrier sent no attach")
        deliver(.terminalOutput(attachmentID: id, base64: Data(bytes).base64EncodedString()))
    }

    func deliverOutput(_ text: String) throws {
        try deliverOutput(Array(text.utf8))
    }
}
