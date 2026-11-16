import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Decisions 214/215 follow-up: the Settings sheet's selection follows the open run's events, not the
/// periodic fleet poll.** A fact-bearing event — a permission-mode change, a model receipt, an effort
/// notice — newer than the list triggers a `sessions` ask the moment the fact exists, so the daemon's
/// resolved selection is pulled and the sheet's check moves at once, whether the event lands before or
/// after the setting's reply, with no race. The model's checked row stays the daemon's `menus.selected_model`
/// — never the receipt's full-id text in `summary.model` ("sonnet (claude-sonnet-5-5)"), the merges-108/109
/// escape. Today's code reads only the poll's summary, so the check lags several seconds (app-qa, 7599ead8);
/// each test here must-fails on that code.
@MainActor
final class ModeFollowTests: XCTestCase {

    private let key = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    // MARK: Fixtures

    private func wire(_ name: String) throws -> [String: JSONValue] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func liveObject() throws -> [String: Any] {
        try JSONSerialization.jsonObject(
            with: Data(try XCTUnwrap(try wire("minor-39-wire")["session_hosted_live"]).canonicalJSONString.utf8))
            as! [String: Any]
    }

    private func summary(_ object: [String: Any]) throws -> SessionSummary {
        try JSONDecoder().decode(SessionSummary.self, from: JSONSerialization.data(withJSONObject: object))
    }

    private func decoded(_ value: JSONValue) throws -> SessionSummary {
        try JSONDecoder().decode(SessionSummary.self, from: Data(value.canonicalJSONString.utf8))
    }

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    // MARK: Per-fact shapes (daemon-emitted)

    // Mode: plan -> bypassPermissions, via a transcript_permission-mode event; the after-summary is the
    // daemon's own bypass summary (minor-40), same run key.
    private func modeInitial() throws -> SessionSummary {
        try decoded(try XCTUnwrap(try wire("minor-39-wire")["session_hosted_live"]))
    }
    private func modeAfter() throws -> SessionSummary {
        try decoded(try XCTUnwrap(try wire("minor-40-wire")["session_hosted_started_in_bypass"]))
    }
    private let modeEventJSON = #"{"seq":200,"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","ts":"2026-11-13T12:30:00.000Z","kind":"transcript_permission-mode","payload":{"permissionMode":"bypassPermissions"},"source":"sdk"}"#

    // Model: menus.selected_model default -> sonnet, via a model receipt. summary.model.value stays the
    // receipt's full id throughout, so a pass proves the checked row is the menu's selected_model, never
    // that text.
    private func modelInitial() throws -> SessionSummary {
        var o = try liveObject()
        var menus = o["menus"] as! [String: Any]
        menus["selected_model"] = "default"
        o["menus"] = menus
        return try summary(o)
    }
    private func modelAfter() throws -> SessionSummary {
        var o = try liveObject()
        var menus = o["menus"] as! [String: Any]
        menus["selected_model"] = "sonnet"
        o["menus"] = menus
        return try summary(o)
    }
    private let modelEventJSON = #"{"seq":200,"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","ts":"2026-11-13T12:30:00.000Z","kind":"user_message","payload":{"message":{"content":"<local-command-stdout>Set model to `claude-sonnet-5-5`</local-command-stdout>"}},"source":"sdk"}"#

    // Effort: high -> low, via an effort notice.
    private func effortInitial() throws -> SessionSummary {
        try decoded(try XCTUnwrap(try wire("minor-39-wire")["session_hosted_live"]))
    }
    private func effortAfter() throws -> SessionSummary {
        var o = try liveObject()
        var fact = o["effort"] as! [String: Any]
        fact["value"] = "low"
        fact["seq"] = (fact["seq"] as! Int) + 10
        o["effort"] = fact
        return try summary(o)
    }
    private let effortEventJSON = #"{"seq":200,"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","ts":"2026-11-13T12:30:00.000Z","kind":"user_message","payload":{"message":{"content":"<local-command-stdout>Set effort level to low (this session only): Quick</local-command-stdout>"}},"source":"sdk"}"#

    // MARK: Harness

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// A model holding `initial`, whose stub — armed only after the initial list has landed — serves
    /// `after` on ANY `sessions` ask (the event-triggered pull) and applies ANY set*. The sheet's new
    /// selection can therefore come only from a fresh ask, never from the setting reply alone.
    private func live(initial: SessionSummary, after: SessionSummary) async throws -> AppModel {
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(try XCTUnwrap(try wire("minor-39-wire")["hello_ack_capabilities"]).canonicalJSONString.utf8))
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("mf-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(caps, minor: 39)
        model.connection.sendStub = { _ in }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.refreshFleet()
        model.connection.injectForTesting(.sessions([initial]))
        await settle(model)
        model.connection.sendStub = { [weak model] message in
            switch message {
            case .sessions:
                model?.connection.injectForTesting(.sessions([after]))
            case .setMode(let session, let request, _, _):
                model?.connection.injectForTesting(
                    .settingResult(sessionID: session, requestID: request, result: .applied(applies: .now)))
            case .setModel(let session, let request, _, _):
                model?.connection.injectForTesting(
                    .settingResult(sessionID: session, requestID: request, result: .applied(applies: .now)))
            default:
                break
            }
        }
        return model
    }

    private func selectedMode(_ model: AppModel) -> String? {
        if case .hostedLive(let s) = model.runSettings(for: key) { return s.selectedMode?.value }
        return nil
    }
    private func selectedModel(_ model: AppModel) -> String? {
        if case .hostedLive(let s) = model.runSettings(for: key) { return s.selectedModel?.value }
        return nil
    }
    private func selectedEffort(_ model: AppModel) -> String? {
        if case .hostedLive(let s) = model.runSettings(for: key) { return s.selectedEffort?.value }
        return nil
    }

    // MARK: Mode

    func testModeCheckFollowsAFactEventBeforeTheReply() async throws {
        let model = try await live(initial: try modeInitial(), after: try modeAfter())
        model.connection.injectForTesting(.event(try event(modeEventJSON)))
        await settle(model)
        XCTAssertEqual(selectedMode(model), "bypassPermissions", "the mode event moves the check")
        _ = await model.setMode(for: key, modeValue: "bypassPermissions", confirmed: true)
        XCTAssertEqual(selectedMode(model), "bypassPermissions")
    }

    func testModeCheckFollowsAFactEventAfterTheReply() async throws {
        let model = try await live(initial: try modeInitial(), after: try modeAfter())
        _ = await model.setMode(for: key, modeValue: "bypassPermissions", confirmed: true)
        XCTAssertEqual(selectedMode(model), "plan", "the reply alone does not move the check")
        model.connection.injectForTesting(.event(try event(modeEventJSON)))
        await settle(model)
        XCTAssertEqual(selectedMode(model), "bypassPermissions", "the later mode event moves it")
    }

    // MARK: Model (receipt-shaped; merges 108/109)

    func testModelCheckFollowsTheDaemonSelectionBeforeTheReply() async throws {
        let model = try await live(initial: try modelInitial(), after: try modelAfter())
        model.connection.injectForTesting(.event(try event(modelEventJSON)))
        await settle(model)
        XCTAssertEqual(
            selectedModel(model), "sonnet",
            "the check follows the daemon's selected_model, not the receipt's full-id text")
        _ = await model.setModel(for: key, modelValue: "sonnet")
        XCTAssertEqual(selectedModel(model), "sonnet")
    }

    func testModelCheckFollowsTheDaemonSelectionAfterTheReply() async throws {
        let model = try await live(initial: try modelInitial(), after: try modelAfter())
        _ = await model.setModel(for: key, modelValue: "sonnet")
        XCTAssertEqual(selectedModel(model), "default", "the reply alone does not move the check")
        model.connection.injectForTesting(.event(try event(modelEventJSON)))
        await settle(model)
        XCTAssertEqual(selectedModel(model), "sonnet", "the model receipt moves it to the daemon's row")
    }

    // MARK: Effort

    func testEffortCheckFollowsAFactEventBeforeTheReply() async throws {
        let model = try await live(initial: try effortInitial(), after: try effortAfter())
        model.connection.injectForTesting(.event(try event(effortEventJSON)))
        await settle(model)
        XCTAssertEqual(selectedEffort(model), "low", "the effort notice moves the check")
        _ = await model.setEffort(for: key, effortValue: "low")
        XCTAssertEqual(selectedEffort(model), "low")
    }

    func testEffortCheckFollowsAFactEventAfterTheReply() async throws {
        let model = try await live(initial: try effortInitial(), after: try effortAfter())
        _ = await model.setEffort(for: key, effortValue: "low")
        XCTAssertEqual(selectedEffort(model), "high", "the reply alone does not move the check")
        model.connection.injectForTesting(.event(try event(effortEventJSON)))
        await settle(model)
        XCTAssertEqual(selectedEffort(model), "low", "the later effort notice moves it")
    }

    // MARK: No poll storm on ordinary turn inits

    /// A run's ordinary turn inits — each a `session_start` carrying the current model — must trigger NO
    /// fleet ask. The model fact is read off session_start too, but that is the turn's init, not a change;
    /// asking on each would be a poll storm during a streaming turn. The model arm of the trigger excludes
    /// session_start, so only a /model receipt (a `.userMessage`) pulls. Must-fails the un-narrowed trigger.
    func testOrdinaryTurnInitsCauseNoFleetAsk() async throws {
        let model = try await live(initial: try modeInitial(), after: try modeAfter())
        var asks = 0
        model.connection.sendStub = { message in if case .sessions = message { asks += 1 } }
        for seq in [300, 400, 500] {
            let json = #"{"seq":\#(seq),"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","ts":"2026-11-13T12:30:00.000Z","kind":"session_start","payload":{"model":"claude-sonnet-5-5","permissionMode":"plan"},"source":"sdk"}"#
            model.connection.injectForTesting(.event(try event(json)))
        }
        await settle(model)
        XCTAssertEqual(asks, 0, "ordinary turn inits cause no fleet ask (no poll storm)")
    }
}
