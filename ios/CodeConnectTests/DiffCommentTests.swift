import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Item 3: a diff comment on a Codex run is composed as a message, not typed.** ccd refuses a `send_text`
/// aimed at a Codex session by name, so a comment there goes through the compose path instead — its outcome
/// in the sheet's own banner, refusals in the app's own words (never ccd's raw sentence), and the comment's
/// own retry (the request id is the compose hash of the words, so a re-send is recognised, not typed twice).
/// Driven through the public `send(text:to:)` — the diff comment's own call (DiffView:1427) — on ccd's
/// `compose-stop-wire.json` Codex summaries. Each test must-fails on today's code, where a Codex comment
/// takes the Claude `send_text` path and the daemon refuses it by name.
@MainActor
final class DiffCommentTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "compose-stop-wire", withExtension: "json"),
            "compose-stop-wire.json is not in the test bundle (ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// A live app holding one of compose-stop-wire's Codex summaries; the caps advertise codex_compose.
    private func liveCodex(_ summaryKey: String, switching: String? = nil) async throws -> (AppModel, String) {
        var fixture = try wire()
        if let switching, case .object(var row)? = fixture[summaryKey] {
            row["switching"] = .string(switching)
            fixture[summaryKey] = .object(row)
        }
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(try XCTUnwrap(fixture["hello_ack_capabilities"]).canonicalJSONString.utf8))
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(try XCTUnwrap(fixture[summaryKey]).canonicalJSONString.utf8))
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("dc-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(caps, minor: 41)
        // The listing's own task subscribes to the run once it has read the run's cache: waited for, so no frame of
        // its lands inside a comment's.
        let subscribed = expectation(description: "the run subscribed")
        subscribed.assertForOverFulfill = false
        model.connection.sendStub = { message in
            if case .subscribe(summary.sessionKey, _) = message { subscribed.fulfill() }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.refreshFleet()
        model.connection.injectForTesting(.sessions([summary]))
        await settle(model)
        await fulfillment(of: [subscribed], timeout: 10)
        return (model, summary.sessionKey)
    }

    private func frameType(_ message: ClientMessage) -> String? { field(message, "type") }
    private func field(_ message: ClientMessage, _ key: String) -> String? {
        guard let data = try? JSONEncoder().encode(message),
            let object = try? JSONDecoder().decode(JSONValue.self, from: data).objectValue
        else { return nil }
        return object[key]?.stringValue
    }

    /// Sends a comment and returns the first frame's type, its request id, and — only when it composed — the
    /// resolved attempt. The attempt is awaited only on a compose frame: on today's Claude `send_text` path
    /// the send would await a daemon response the stub never gives, so gating the await on a seen compose
    /// frame lets a must-fail record the wrong frame type and still finish, never hang.
    private func comment(_ model: AppModel, _ text: String, to key: String, reply: ComposeResult)
        async -> (type: String?, requestID: String?, attempt: ComposeAttempt?)
    {
        var firstType: String?
        var requestID: String?
        let composed = expectation(description: "the comment composed")
        model.connection.sendStub = { [weak model] message in
            if firstType == nil { firstType = self.frameType(message) }
            guard self.frameType(message) == "compose" else { return }
            requestID = self.field(message, "request_id")
            if let session = self.field(message, "session_id"), let request = requestID {
                model?.connection.injectForTesting(
                    .composeResult(sessionID: session, requestID: request, result: reply))
            }
            composed.fulfill()
        }
        let send = Task { @MainActor in await model.send(text: text, to: key) }
        await fulfillment(of: [composed], timeout: 2)
        guard firstType == "compose" else {
            send.cancel()
            return (firstType, requestID, nil)
        }
        return (firstType, requestID, await send.value)
    }

    // MARK: Composed, not typed (core)

    func testACodexDiffCommentIsComposedNotTyped() async throws {
        let (model, key) = try await liveCodex("session_summary")  // a live Codex run
        let (frameType, _, attempt) = await comment(
            model, "In `Foo.swift` line 9: this is wrong", to: key, reply: .started(turnID: "t-1"))
        XCTAssertEqual(frameType, "compose", "a Codex comment is composed, never typed — send_text is refused by name")
        guard case .sent? = attempt else {
            return XCTFail("the comment is sent, got \(String(describing: attempt))")
        }
    }

    /// **A comment that moves a Codex run asks for the list again, once**, as the composer's own message does: the run
    /// shows its new account without waiting for the next refresh.
    func testACommentToAMovingCodexRunAsksForTheListOnce() async throws {
        let (model, key) = try await liveCodex("session_summary", switching: "waiting")
        var asked = 0
        let composed = expectation(description: "the comment composed")
        model.connection.sendStub = { [weak model] message in
            if self.frameType(message) == "sessions" { asked += 1 }
            guard self.frameType(message) == "compose", let session = self.field(message, "session_id"),
                let request = self.field(message, "request_id")
            else { return }
            model?.connection.injectForTesting(
                .composeResult(sessionID: session, requestID: request, result: .started(turnID: "t-1")))
            composed.fulfill()
        }
        let attempt = await model.send(text: "In `Foo.swift` line 9: this is wrong", to: key)
        await fulfillment(of: [composed], timeout: 2)
        guard case .sent = attempt else { return XCTFail("the comment is sent, got \(attempt)") }
        XCTAssertEqual(asked, 1, "the comment that moved the run asks for the list once")
    }

    // MARK: Relaunchable run

    func testACommentToARelaunchableCodexRunRelaunchesIt() async throws {
        let (model, key) = try await liveCodex("session_summary_relaunchable")  // exited, bound_not_started
        XCTAssertNil(
            model.composeBlocked(for: key), "a relaunchable run admits a comment — the composer's own gate")
        // The compose path is the one decision 210 reopens an exited Codex run through, so a comment
        // relaunches it exactly as a composed message does — no separate relaunch path.
        let (frameType, _, attempt) = await comment(
            model, "In `Bar.swift` line 2: fix this", to: key, reply: .started(turnID: "t-1"))
        XCTAssertEqual(frameType, "compose", "a comment to a relaunchable run composes (relaunches), never types")
        guard case .sent? = attempt else { return XCTFail("the relaunching comment is sent") }
    }

    // MARK: Own retry

    func testARetriedCodexCommentIsRecognisedNotDoubleSent() async throws {
        let (model, key) = try await liveCodex("session_summary")
        let text = "In `Baz.swift` line 5: same words, sent twice"
        let (_, id1, first) = await comment(model, text, to: key, reply: .started(turnID: "t-1"))
        let (_, id2, second) = await comment(
            model, text, to: key, reply: .duplicate(turnID: "t-1", started: false))
        XCTAssertNotNil(id1, "the comment composed with a request id")
        XCTAssertEqual(id1, id2, "the retry carries the same request id — keyed on the words, the compose hash")
        guard case .sent? = first else { return XCTFail("first comment is sent, got \(String(describing: first))") }
        guard case .alreadyApplied? = second else {
            return XCTFail("the retry is recognised and replayed, got \(String(describing: second))")
        }
    }
}
