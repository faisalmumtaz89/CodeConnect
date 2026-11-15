import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Sending a hosted run's model / effort / mode through the app's own flow** (B8, minor 39): the call
/// goes out, its answer is worded, the menus are disabled while it is in flight, and the notice is
/// dropped once the run's own fact passes it. Driven like `Minor35WireTests`' stop path — a real
/// `AppModel`, the daemon's live summary answered into the fleet, the answer injected on the wire.
@MainActor
final class SettingControlTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-39-wire", withExtension: "json"))
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// An app that has the daemon's live hosted summary (minor 39) in its fleet.
    private func liveModel() async throws -> (AppModel, String) {
        let fixture = try wire()
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(try XCTUnwrap(fixture["hello_ack_capabilities"]).canonicalJSONString.utf8))
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(try XCTUnwrap(fixture["session_hosted_live"]).canonicalJSONString.utf8))
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
        XCTAssertEqual(s.lastOutcome, SettingOutcome(text: "Model set to Sonnet.", tone: .success))
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
}
