import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **End run, against the daemon's own bytes**: which runs offer it, which can be removed,
/// what a run being ended reads, and what the person is told.
///
/// `minor-37-wire.json` carries the summaries ccd emits for a live and an ended run CodeConnect
/// started; `minor-35-wire.json` a run that is stopping and each `stop_run` answer. Both are copied byte for byte from `fixtures/claude/`.
@MainActor
final class EndRunTests: XCTestCase {

    /// Ending asks in W0-6's words, naming the run: what ending does, a line each, and that its folder stays.
    func testEndingAsksNamingTheRunAndSaysWhatEndingDoes() {
        let end = FleetWords.endAsk("query-parser", agent: "Codex")
        XCTAssertEqual(end.title, "End “query-parser”?")
        XCTAssertEqual(end.items.map(\.words), ["Codex stops", "The session moves to Ended"])
        XCTAssertEqual(end.note, "Its folder stays as it is.")
        XCTAssertEqual([end.cancel, end.confirm], ["Cancel", "End Session"])
        XCTAssertEqual(end.message, "Codex stops. The session moves to Ended. Its folder stays as it is.")
    }

    /// The notice after an end or a removal that did not happen says why in a whole sentence of its own, one for
    /// each answer ccd can give, and quotes no wire word: an answer this build does not know is "refused", plainly.
    func testAFailedEndOrRemovalSaysWhyInASentenceAndQuotesNoWireWord() {
        XCTAssertEqual(FleetWords.endFailedTitle, "Couldn’t end the session")
        XCTAssertEqual(FleetWords.removeFailedTitle, "Couldn’t remove the session")
        XCTAssertEqual(FleetWords.removalRefused(.stillRunning), "It’s still running. End it first, then remove it.")
        XCTAssertEqual(
            FleetWords.removalRefused(.notExited(lifecycle: "unknown")),
            "The computer doesn’t know whether it has stopped, so it keeps the session.")
        XCTAssertEqual(
            FleetWords.removalRefused(.failed(message: "the daemon could not remove that session")),
            "The computer couldn’t remove it.")
        XCTAssertEqual(FleetWords.removalRefused(.unknown(status: "busy")), "The computer refused.")
        XCTAssertEqual(FleetWords.removalRefused(nil), "The computer didn’t answer.")
        XCTAssertNil(FleetWords.removalRefused(.deleted(events: 3)))
        XCTAssertNil(FleetWords.removalRefused(.notFound))
        XCTAssertEqual(FleetWords.refusedPlainly, "The computer refused.")
    }

    private func wire(_ minor: Int) throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-\(minor)-wire", withExtension: "json"))
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func decoded<T: Decodable>(_ type: T.Type, _ key: String, minor: Int) throws -> T {
        let value = try XCTUnwrap(try wire(minor)[key], "no \(key) in minor-\(minor)-wire.json")
        return try JSONDecoder().decode(T.self, from: Data(value.canonicalJSONString.utf8))
    }

    private func summary(_ key: String) throws -> SessionSummary {
        try decoded(SessionSummary.self, key, minor: 37)
    }

    private func stopping() throws -> SessionSummary {
        guard case .sessions(let list) = try decoded(ServerMessage.self, "sessions", minor: 35) else {
            throw CocoaError(.coderReadCorrupt, userInfo: [NSDebugDescriptionErrorKey: "sessions is not a sessions frame"])
        }
        return try XCTUnwrap(list.first, "minor-35's sessions frame holds no run")
    }

    private func asksForTheFleet(_ sent: [ClientMessage]) -> Bool {
        sent.contains { if case .sessions = $0 { return true } else { return false } }
    }

    private func stops(_ sent: [ClientMessage], _ run: SessionSummary) -> Bool {
        sent.contains { if case .stopRun(let session, _) = $0 { return session == run.sessionUID } else { return false } }
    }

    /// A model connected to a daemon that advertises what minor 37's does, with or
    /// without `stop_run`; every frame it would send is kept, and a `stop_run` is answered
    /// with `answer`.
    private func model(endsRuns: Bool = true, answer: StopRunResult? = nil) throws -> (AppModel, () -> [ClientMessage]) {
        let model = AppModel(
            cache: EventCache(),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        var capabilities = try decoded(Capabilities.self, "hello_ack_capabilities", minor: 37)
        if !endsRuns {
            capabilities = Capabilities(canApproveReliably: true, sendText: true, extra: ["stop_run": .bool(false)])
        }
        model.connection.simulateCapabilitiesForTesting(capabilities, minor: 37)
        var sent: [ClientMessage] = []
        model.connection.sendStub = { [weak connection = model.connection] message in
            sent.append(message)
            if case .stopRun(let session, let requestID) = message, let answer {
                connection?.injectForTesting(.stopRunResult(sessionID: session, requestID: requestID, result: answer))
            }
        }
        return (model, { sent })
    }

    func testEndRunIsOfferedOnlyOnALiveRunCodeConnectStarted() throws {
        let (model, _) = try model()
        XCTAssertTrue(model.canEndRun(try summary("session_hosted_live")))
        XCTAssertFalse(model.canEndRun(try summary("session_hosted_ended")), "it has ended")
        XCTAssertFalse(model.canEndRun(try stopping()), "it is already being ended")
        let (older, _) = try self.model(endsRuns: false)
        XCTAssertFalse(older.canEndRun(try summary("session_hosted_live")), "this daemon cannot end a run")
    }

    func testARunIsRemovableOnlyOnceItHasEnded() throws {
        XCTAssertFalse(try summary("session_hosted_live").isRemovable, "ccd refuses a live run it started")
        XCTAssertFalse(try stopping().isRemovable)
        XCTAssertTrue(try summary("session_hosted_ended").isRemovable)
    }

    func testARunBeingEndedReadsStopping() throws {
        XCTAssertEqual(FleetStatusRule.status(summary: try stopping(), shown: nil, reviewedSeq: 0), .stopping)
        XCTAssertEqual(FleetStatus.stopping.label, "Stopping")
    }

    /// Accepted or already over, nothing to say, and the fleet is asked again at once: ccd
    /// sends nothing else, so the row reads Stopping without waiting for the next poll.
    func testAStopTakenSaysNothingAndAsksForTheFleet() async throws {
        for key in ["stop_run_stopping", "stop_run_already_ended"] {
            guard case .stopRunResult(_, _, let answer) = try decoded(ServerMessage.self, key, minor: 35) else {
                return XCTFail("\(key) is not a stop_run_result")
            }
            let (model, sent) = try model(answer: answer)
            let run = try summary("session_hosted_live")
            let note = await model.endRun(run)
            XCTAssertNil(note, key)
            XCTAssertTrue(stops(sent(), run), "\(key): no stop_run for the run")
            XCTAssertTrue(asksForTheFleet(sent()), "\(key): the fleet is not asked again")
        }
    }

    func testARefusalIsSaidInTheComputersOwnWords() async throws {
        guard case .stopRunResult(_, _, let answer) = try decoded(ServerMessage.self, "stop_run_refused", minor: 35)
        else { return XCTFail("stop_run_refused is not a stop_run_result") }
        let (model, sent) = try model(answer: answer)
        let note = await model.endRun(try summary("session_hosted_live"))
        XCTAssertEqual(note, "There is no such session on this computer.")
        XCTAssertFalse(asksForTheFleet(sent()), "nothing changed, so nothing is asked again")
    }

    func testADaemonThatCannotEndARunIsSaidSoAndSentNothing() async throws {
        let (model, sent) = try model(endsRuns: false)
        let note = await model.endRun(try summary("session_hosted_live"))
        XCTAssertNotNil(note)
        XCTAssertTrue(sent().isEmpty, "a frame left without stop_run")
    }
}
