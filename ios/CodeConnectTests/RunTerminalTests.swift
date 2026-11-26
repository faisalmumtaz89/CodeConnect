import XCTest

@testable import CodeConnectKit

/// **A run's shell as one computer's link carries it** (minor 55), counted at the socket: the open and what it asks
/// for, output fed in order with its credit given back, typing held for the open and for credit, a fold and a resume
/// from the offset fed, a stopped view opened again by a keystroke, and a reconnect that resumes without taking over.
@MainActor
final class RunTerminalTests: XCTestCase {

    private func model(serves: Bool = true) -> (AppModel, Recorder) {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(canApproveReliably: true, sendText: true, extra: ["run_terminal": .bool(serves)]), minor: 55)
        let recorder = Recorder()
        model.connection.sendStub = { recorder.frames.append($0) }
        model.terminal.output = { data, reset in recorder.fed.append((data, reset, model.terminal.replaying)) }
        return (model, recorder)
    }

    private final class Recorder {
        var frames: [ClientMessage] = []
        var fed: [(data: Data, reset: Bool, replaying: Bool)] = []
        var opens: [(id: String, resumeAfter: UInt64?, takeover: Bool)] {
            frames.compactMap {
                guard case .terminalOpen(let id, _, _, _, _, let resume, let takeover) = $0 else { return nil }
                return (id, resume, takeover)
            }
        }
        var typed: [String] {
            frames.compactMap {
                guard case .terminalInput(_, let data) = $0 else { return nil }
                return Data(base64Encoded: data).map { String(decoding: $0, as: UTF8.self) }
            }
        }
        var credits: [Int] {
            frames.compactMap {
                guard case .terminalCredit(_, let bytes) = $0 else { return nil }
                return bytes
            }
        }
    }

    private func opened(_ model: AppModel, _ id: String, credit: Int = 32_768, replayEnd: UInt64 = 0) {
        model.connection.injectForTesting(
            .terminalOpened(
                attachmentID: id, inputCredit: credit, maxChunkBytes: 16_384, folder: "/r/.claude/worktrees/x",
                replayEnd: replayEnd))
    }

    private func output(_ model: AppModel, _ id: String, _ text: String, reset: Bool = false, offset: UInt64) {
        model.connection.injectForTesting(
            .terminalOutput(attachmentID: id, data: Data(text.utf8), reset: reset, offset: offset))
    }

    /// **The person's open takes the shell, at the view's size, from no offset; a computer that serves no shells is
    /// sent nothing.**
    func testShowOpensTheRunsShellAndTakesIt() throws {
        let (model, recorder) = model()
        model.terminal.show(session: "u-1", cols: 100, rows: 30)
        guard case .terminalOpen(_, "u-1", 100, 30, RunTerminal.outputCredit, nil, true)? = recorder.frames.first else {
            return XCTFail("not the open asked for: \(recorder.frames)")
        }
        XCTAssertEqual(model.terminal.phase, .opening)
        opened(model, try XCTUnwrap(recorder.opens.first?.id))
        XCTAssertEqual(model.terminal.phase, .open(folder: "/r/.claude/worktrees/x"))

        let (older, olderRecorder) = self.model(serves: false)
        older.terminal.show(session: "u-1", cols: 80, rows: 24)
        XCTAssertTrue(olderRecorder.frames.isEmpty, "\(olderRecorder.frames)")
    }

    /// **Output is fed in order, each chunk's credit given back; what replays the shell's past is marked so; and a
    /// frame for an attachment that is no longer this view's is dropped.**
    func testOutputIsFedInOrderAndCreditedBack() throws {
        let (model, recorder) = model()
        model.terminal.show(session: "u-1", cols: 80, rows: 24)
        let id = try XCTUnwrap(recorder.opens.first?.id)
        opened(model, id, replayEnd: 6)
        output(model, id, "past\r\n", reset: true, offset: 6)
        output(model, id, "now", offset: 9)
        output(model, "someone-else", "not mine", offset: 99)
        XCTAssertEqual(recorder.fed.map { String(decoding: $0.data, as: UTF8.self) }, ["past\r\n", "now"])
        XCTAssertEqual(recorder.fed.map(\.reset), [true, false])
        XCTAssertEqual(recorder.fed.map(\.replaying), [true, false], "the replay is marked, live output is not")
        XCTAssertEqual(recorder.credits, [6, 3])
    }

    /// **Typing waits for the open and for credit, goes in chunks no larger than the computer takes, and the rest
    /// follows as credit comes back.**
    func testTypingWaitsForTheOpenAndForCredit() throws {
        let (model, recorder) = model()
        model.terminal.show(session: "u-1", cols: 80, rows: 24)
        model.terminal.type(Array("ls\r".utf8))
        XCTAssertTrue(recorder.typed.isEmpty, "nothing is sent before the shell is open")
        let id = try XCTUnwrap(recorder.opens.first?.id)
        opened(model, id, credit: 4)
        XCTAssertEqual(recorder.typed, ["ls\r"])
        model.terminal.type(Array("abcdef".utf8))
        XCTAssertEqual(recorder.typed, ["ls\r", "a"], "one byte of credit was left")
        model.connection.injectForTesting(.terminalCredit(attachmentID: id, bytes: 8))
        XCTAssertEqual(recorder.typed, ["ls\r", "a", "bcdef"])
    }

    /// **Folding lets the shell go; showing it again resumes after the offset fed, taking it back.**
    func testAFoldResumesFromTheOffsetFed() throws {
        let (model, recorder) = model()
        model.terminal.show(session: "u-1", cols: 80, rows: 24)
        let first = try XCTUnwrap(recorder.opens.first?.id)
        opened(model, first)
        output(model, first, "hello", reset: true, offset: 1_099_511_627_781)
        model.terminal.hide()
        guard case .terminalDetach(first)? = recorder.frames.last else { return XCTFail("\(recorder.frames)") }
        XCTAssertEqual(model.terminal.phase, .idle)
        model.terminal.show(session: "u-1", cols: 80, rows: 24)
        XCTAssertEqual(recorder.opens.last?.resumeAfter, 1_099_511_627_781)
        XCTAssertEqual(recorder.opens.last?.takeover, true)
    }

    /// **A shell that exited keeps its screen until a keystroke, which opens it again and goes to it.**
    func testAKeystrokeOpensAStoppedShellAgain() throws {
        let (model, recorder) = model()
        model.terminal.show(session: "u-1", cols: 80, rows: 24)
        let first = try XCTUnwrap(recorder.opens.first?.id)
        opened(model, first)
        model.connection.injectForTesting(.terminalClosed(attachmentID: first, reason: "exited"))
        XCTAssertEqual(model.terminal.phase, .stopped(reason: "exited"))
        XCTAssertEqual(recorder.opens.count, 1, "nothing opens until the person types")
        model.terminal.type(Array("x".utf8))
        XCTAssertEqual(recorder.opens.count, 2)
        XCTAssertEqual(recorder.opens.last?.takeover, true)
        XCTAssertTrue(recorder.typed.isEmpty)
        opened(model, try XCTUnwrap(recorder.opens.last?.id))
        XCTAssertEqual(recorder.typed, ["x"], "the keystroke that opened it goes to the fresh shell")
    }

    /// **A link that drops and comes back resumes the view without taking the shell from anyone; a stopped view is
    /// left stopped.**
    func testAReconnectResumesWithoutTakingOver() throws {
        let (model, recorder) = model()
        model.terminal.show(session: "u-1", cols: 80, rows: 24)
        let first = try XCTUnwrap(recorder.opens.first?.id)
        opened(model, first)
        output(model, first, "x", reset: true, offset: 1)
        model.terminal.linkDown()
        XCTAssertEqual(model.terminal.phase, .opening)
        model.terminal.linkUp()
        XCTAssertEqual(recorder.opens.count, 2)
        XCTAssertEqual(recorder.opens.last?.takeover, false)
        XCTAssertEqual(recorder.opens.last?.resumeAfter, 1)
        model.connection.injectForTesting(
            .terminalClosed(attachmentID: try XCTUnwrap(recorder.opens.last?.id), reason: "superseded"))
        model.terminal.linkDown()
        model.terminal.linkUp()
        XCTAssertEqual(recorder.opens.count, 2, "a view another device took waits for a keystroke")
    }

    /// **The frames, as the daemon writes and reads them** (`ws.rs`'s own wire test).
    func testTheTerminalFramesMatchTheDaemons() throws {
        let decoder = JSONDecoder()
        let output = try decoder.decode(
            ServerMessage.self,
            from: Data(#"{"type":"terminal_output","attachment_id":"t1","data":"aGk=","offset":14}"#.utf8))
        guard case .terminalOutput("t1", let data, false, 14) = output else { return XCTFail("\(output)") }
        XCTAssertEqual(String(decoding: data, as: UTF8.self), "hi")
        let opened = try decoder.decode(
            ServerMessage.self,
            from: Data(
                #"{"type":"terminal_opened","attachment_id":"t1","input_credit":32768,"max_chunk_bytes":16384,"max_outstanding_credit":262144,"folder":"/r/.claude/worktrees/x","replay_end":12}"#
                    .utf8))
        guard case .terminalOpened("t1", 32_768, 16_384, "/r/.claude/worktrees/x", 12) = opened else {
            return XCTFail("\(opened)")
        }
        let closed = try decoder.decode(
            ServerMessage.self, from: Data(#"{"type":"terminal_closed","attachment_id":"t1","reason":"run_ended"}"#.utf8))
        guard case .terminalClosed("t1", "run_ended") = closed else { return XCTFail("\(closed)") }
        let open = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(
                ClientMessage.terminalOpen(
                    attachmentID: "t1", session: "01RUN", cols: 80, rows: 24, outputCredit: 65_536, resumeAfter: nil,
                    takeover: true))) as? [String: Any]
        XCTAssertEqual(open?["type"] as? String, "terminal_open")
        XCTAssertEqual(open?["session_id"] as? String, "01RUN")
        XCTAssertEqual(open?["output_credit"] as? Int, 65_536)
        XCTAssertNil(open?["resume_after"], "no offset is left out, as the daemon reads it")
        XCTAssertEqual(open?["takeover"] as? Bool, true)
    }
}
