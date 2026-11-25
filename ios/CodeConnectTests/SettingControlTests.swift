import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Sending a hosted run's model / effort / mode through the app's own flow** (B8, minor 39): the call
/// goes out, its answer is worded, the menus are disabled while it is in flight, and the notice is
/// dropped once the run's own fact passes it — or, for a change that applies from the next turn, once
/// this app sends the run's next message. Driven like `Minor35WireTests`' stop path — a real
/// `AppModel`, the daemon's live summary answered into the fleet, the answer injected on the wire.
@MainActor
final class SettingControlTests: XCTestCase {

    private func wire(_ name: String = "minor-39-wire") throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// An app that has one of the daemon's live run summaries in its fleet: by default the hosted Claude
    /// run (minor 39).
    private func liveModel(
        _ run: String = "session_hosted_live", in name: String = "minor-39-wire"
    ) async throws -> (AppModel, String) {
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(try XCTUnwrap(try wire()["hello_ack_capabilities"]).canonicalJSONString.utf8))
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(try XCTUnwrap(try wire(name)[run]).canonicalJSONString.utf8))
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("b8-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(caps, minor: 39)
        model.connection.sendStub = { _ in }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.refreshFleet()
        model.connection.injectForTesting(.sessions([summary]))
        await settle(model)
        return (model, summary.sessionKey)
    }

    private func live(_ model: AppModel, _ key: String) -> HostedSettings? {
        if case .hostedLive(let s) = model.runSettings(for: key) { return s }
        return nil
    }

    /// An applied model change: the frame leaves, the menus are disabled while the answer is awaited,
    /// and the outcome names the setting and the menu's label.
    func testAnAppliedModelChangeIsWordedAndDisablesTheMenusWhileAwaited() async throws {
        let (model, key) = try await liveModel()
        var inFlightDuringSend = false
        model.connection.sendStub = { [weak model] message in
            guard case .setModel(let session, let request, let modelValue, _) = message else { return }
            XCTAssertEqual(modelValue, "sonnet")
            if let model, case .hostedLive(let s) = model.runSettings(for: key) { inFlightDuringSend = s.inFlight }
            model?.connection.injectForTesting(
                .settingResult(sessionID: session, requestID: request, result: .applied(applies: .now)))
        }
        let result = await model.setModel(for: key, modelValue: "sonnet")
        XCTAssertEqual(result, .applied(applies: .now))
        XCTAssertTrue(inFlightDuringSend, "the menus are disabled while the call is in flight")
        let s = try XCTUnwrap(live(model, key))
        XCTAssertFalse(s.inFlight)
        XCTAssertEqual(s.lastOutcome, SettingOutcome(text: "Model set to Sonnet 5.5.", tone: .success))
    }

    /// A refused change shows the agent's own sentence, verbatim, as a warning — and effort rides
    /// `set_model`, so a bare effort change is a `set_model` with no model.
    func testARefusedEffortShowsTheAgentsSentence() async throws {
        let (model, key) = try await liveModel()
        model.connection.sendStub = { [weak model] message in
            guard case .setModel(let session, let request, let modelValue, let effort) = message else { return }
            XCTAssertNil(modelValue, "a bare effort change carries no model")
            XCTAssertEqual(effort, "turbo")
            model?.connection.injectForTesting(
                .settingResult(
                    sessionID: session, requestID: request,
                    result: .refused(reason: "Claude doesn't offer that effort in this session; nothing was changed.")))
        }
        _ = await model.setEffort(for: key, effortValue: "turbo")
        let s = try XCTUnwrap(live(model, key))
        XCTAssertEqual(
            s.lastOutcome,
            SettingOutcome(
                text: "Claude doesn't offer that effort in this session; nothing was changed.",
                tone: .warning))
    }

    /// The applied notice bridges the wait, then drops once the run's model fact moves past the one it
    /// was recorded against — the fact caught up, so the notice has said its piece.
    func testTheOutcomeDropsOnceTheModelFactPassesIt() async throws {
        let (model, key) = try await liveModel()
        model.connection.sendStub = { [weak model] message in
            guard case .setModel(let session, let request, _, _) = message else { return }
            model?.connection.injectForTesting(
                .settingResult(sessionID: session, requestID: request, result: .applied(applies: .now)))
        }
        _ = await model.setModel(for: key, modelValue: "sonnet")
        XCTAssertNotNil(try XCTUnwrap(live(model, key)).lastOutcome, "the notice shows while the fact lags")

        // The daemon's later summary: the model fact's seq has advanced past the baseline.
        var object = try JSONSerialization.jsonObject(
            with: Data(try XCTUnwrap(try wire()["session_hosted_live"]).canonicalJSONString.utf8))
            as! [String: Any]
        var modelFact = object["model"] as! [String: Any]
        modelFact["seq"] = (modelFact["seq"] as! Int) + 1
        object["model"] = modelFact
        let advanced = try JSONDecoder().decode(
            SessionSummary.self, from: JSONSerialization.data(withJSONObject: object))
        model.connection.injectForTesting(.sessions([advanced]))
        await settle(model)
        XCTAssertNil(try XCTUnwrap(live(model, key)).lastOutcome, "the notice drops once the fact passes it")
    }

    /// The run's later summary once its effort is filed as `effort` at `seq`, with each of `refiled`
    /// filed again at the same seq: a Codex settings report names all three at once, Claude's
    /// `model_set` notice only the effort.
    private func reported(
        _ run: String, in name: String, effort: String, seq: Int, refiled: [String]
    ) throws -> SessionSummary {
        var object = try JSONSerialization.jsonObject(
            with: Data(try XCTUnwrap(try wire(name)[run]).canonicalJSONString.utf8)) as! [String: Any]
        for fact in refiled {
            var filed = object[fact] as! [String: Any]
            filed["seq"] = seq
            object[fact] = filed
        }
        object["effort"] = ["seq": seq, "value": effort]
        return try JSONDecoder().decode(SessionSummary.self, from: JSONSerialization.data(withJSONObject: object))
    }

    /// Answers every settings call `.applied(applies)`.
    private func answerSettings(_ model: AppModel, _ applies: SettingApplies) {
        model.connection.sendStub = { [weak model] message in
            switch message {
            case .setModel(let session, let request, _, _), .setMode(let session, let request, _, _):
                model?.connection.injectForTesting(
                    .settingResult(sessionID: session, requestID: request, result: .applied(applies: applies)))
            default: break
            }
        }
    }

    private static let lowFromTheNextMessage = SettingOutcome(
        text: "Effort changes to Low from your next message.", tone: .neutral)

    /// A Codex run's effort set during a turn applies from the next message, and Codex reports the
    /// thread's new settings at the change itself: the note still says so after that report has moved
    /// the effort fact past the one the call started from.
    func testACodexNextTurnNoteOutlivesCodexsOwnReportOfTheChange() async throws {
        let (model, key) = try await liveModel("session_codex_live", in: "codex-settings-wire")
        answerSettings(model, .nextTurn)
        _ = await model.setEffort(for: key, effortValue: "low")
        model.connection.injectForTesting(
            .sessions([
                try reported(
                    "session_codex_live", in: "codex-settings-wire", effort: "low", seq: 2,
                    refiled: ["model", "permission_mode"])
            ]))
        await settle(model)
        XCTAssertEqual(try XCTUnwrap(live(model, key)).lastOutcome, Self.lowFromTheNextMessage)
    }

    /// A Claude run's effort set during a turn: ccd files claude's `model_set` notice for it before the
    /// answer goes, so the effort fact moves at the change too. The note still says when it applies.
    func testAClaudeNextTurnEffortNoteOutlivesTheTakenEffortsNotice() async throws {
        let (model, key) = try await liveModel()
        answerSettings(model, .nextTurn)
        _ = await model.setEffort(for: key, effortValue: "low")
        model.connection.injectForTesting(
            .sessions([try reported("session_hosted_live", in: "minor-39-wire", effort: "low", seq: 8, refiled: [])]))
        await settle(model)
        XCTAssertEqual(try XCTUnwrap(live(model, key)).lastOutcome, Self.lowFromTheNextMessage)
    }

    /// The next message this app sends to the Claude run is the one the change applies from: the note
    /// has said its piece, and the send's own outcome takes the line.
    func testAClaudeNextTurnNoteClearsWhenThisAppSendsTheNextMessage() async throws {
        let (model, key) = try await liveModel()
        answerSettings(model, .nextTurn)
        _ = await model.setEffort(for: key, effortValue: "low")
        XCTAssertEqual(try XCTUnwrap(live(model, key)).lastOutcome, Self.lowFromTheNextMessage)
        model.connection.sendTextStub = { _, _ in .sent(matched: "❯") }
        let attempt = await model.send(text: "go on", to: key)
        XCTAssertEqual(attempt, .sent(matched: "❯"))
        XCTAssertNil(try XCTUnwrap(live(model, key)).lastOutcome)
    }

    /// The same on a Codex run, by both of its routes: the reply box's compose and a `send` caller's
    /// (a diff comment).
    func testACodexNextTurnNoteClearsWhenThisAppSendsTheNextMessage() async throws {
        let (model, key) = try await liveModel("session_codex_live", in: "codex-settings-wire")
        let composed = { [weak model] (message: ClientMessage) in
            guard case .compose(let session, let request, _, _, _) = message else { return }
            model?.connection.injectForTesting(
                .composeResult(sessionID: session, requestID: request, result: .started(turnID: "t-\(request)")))
        }

        answerSettings(model, .nextTurn)
        _ = await model.setEffort(for: key, effortValue: "low")
        XCTAssertEqual(try XCTUnwrap(live(model, key)).lastOutcome, Self.lowFromTheNextMessage)
        model.connection.sendStub = composed
        let result = await model.composeToCodex(sessionKey: key, text: "go on")
        guard case .started? = result else { return XCTFail("the message did not go: \(String(describing: result))") }
        XCTAssertNil(try XCTUnwrap(live(model, key)).lastOutcome, "the reply box's message")

        answerSettings(model, .nextTurn)
        _ = await model.setEffort(for: key, effortValue: "low")
        XCTAssertEqual(try XCTUnwrap(live(model, key)).lastOutcome, Self.lowFromTheNextMessage)
        model.connection.sendStub = composed
        let attempt = await model.send(text: "src/a.swift:3: and here", to: key)
        guard case .sent = attempt else { return XCTFail("the comment did not go: \(attempt)") }
        XCTAssertNil(try XCTUnwrap(live(model, key)).lastOutcome, "a diff comment")
    }
}
