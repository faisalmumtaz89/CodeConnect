import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Claude Stop eligibility against the daemon's OWN session summaries (minor 37), not a hand-built one.**
///
/// The class this closes (terminal's Codex finding): the Kit decides whether to offer Stop from a
/// `SessionSummary` field, and a summary written by hand in a test can agree with the gate while the
/// daemon's real summary does not — the daemon filled `codex_link` only for tmux-era runs, so every
/// hand-built-summary test passed while a stock app could never offer Stop on an engine Codex run. So the
/// Claude Stop gate is proved here against `fixtures/claude/minor-37-wire.json`: the daemon's captured
/// `session_hosted_live` (empty `tmux_session`, lifecycle live, a real running turn), `session_tmux_live`
/// (a hook run) and `session_hosted_ended`, plus the hello capabilities it advertised.
@MainActor
final class ClaudeStopEligibilityWireTests: XCTestCase {

    /// The daemon's recorded frame set, read from the source fixture (the daemon-emitted bytes).
    private func wire() throws -> [String: JSONValue] {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let url = repoRoot.appendingPathComponent("fixtures/claude/minor-37-wire.json")
        guard let data = try? Data(contentsOf: url) else {
            throw XCTSkip("fixtures/claude/minor-37-wire.json is not reachable from this bundle")
        }
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
    }

    /// Lets the coalesced rebuilds and publications the injections cause run, so `shown(for:)` reflects them.
    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// An app that advertised the fixture's hello capabilities (so `claude_interrupt` is the daemon's own),
    /// with the daemon's `summaryKey` summary answered into the fleet and, when `eventsKey` is given, that
    /// run's real events so a turn reads as running from the wire. The flow is the app's own —
    /// request → answer → events → settle — so the run is arranged onto the screen the gate reads.
    private func model(summaryKey: String, eventsKey: String?) async throws -> (AppModel, String) {
        let fixture = try wire()
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(try XCTUnwrap(fixture["hello_ack_capabilities"]).canonicalJSONString.utf8))
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(try XCTUnwrap(fixture[summaryKey]).canonicalJSONString.utf8))
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("m37-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(caps, minor: 37)
        model.connection.sendStub = { _ in }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.refreshFleet()
        model.connection.injectForTesting(.sessions([summary]))
        if let eventsKey {
            for element in try XCTUnwrap(fixture[eventsKey]?.arrayValue) {
                let event = try JSONDecoder().decode(Event.self, from: Data(element.canonicalJSONString.utf8))
                model.connection.injectForTesting(.event(event))
            }
        }
        await settle(model)
        return (model, summary.sessionKey)
    }

    /// Hosted, live, a running turn, `claude_interrupt`: Stop is offered. If the gate ever reads a field the
    /// daemon fills differently (the Codex class), this is the test that goes red.
    func testAHostedLiveClaudeRunWithARunningTurnIsStoppable() async throws {
        let (model, key) = try await self.model(
            summaryKey: "session_hosted_live", eventsKey: "session_hosted_live_events")
        XCTAssertNotNil(model.shown(for: key)?.runningTurn, "the fixture's events carry a running turn")
        XCTAssertNil(
            model.stopUnavailable(for: key),
            "a hosted, live Claude run with a running turn and claude_interrupt is stoppable")
    }

    /// A tmux (hook-era) Claude run is not hosted, so Stop is not offered — exactly the shape a hand-built
    /// summary could get wrong.
    func testATmuxHookClaudeRunIsNotStoppable() async throws {
        let (model, key) = try await self.model(summaryKey: "session_tmux_live", eventsKey: nil)
        XCTAssertNotNil(
            model.stopUnavailable(for: key), "a tmux (hook) run is not hosted, so Stop is not offered")
    }

    /// A hosted run that has ended has no turn to stop.
    func testAnEndedHostedClaudeRunIsNotStoppable() async throws {
        let (model, key) = try await self.model(summaryKey: "session_hosted_ended", eventsKey: nil)
        XCTAssertNotNil(
            model.stopUnavailable(for: key), "an ended run has no running turn, so Stop is not offered")
    }
}
