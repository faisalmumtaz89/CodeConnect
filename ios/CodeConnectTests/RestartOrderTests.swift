import XCTest

@testable import CodeConnectKit

/// **A daemon restart is not activity, on the phone's side.** `restart-link-state-wire.json` is what a daemon emits
/// across a restart (re-derived by `ccd/src/state.rs`):
/// - runs A and B each start a tool call, A first;
/// - C does nothing;
/// - the daemon restarts, and B, C, then A register again, so A's link fact is the newest;
/// - A then starts another tool call.
///
/// Each stage reaches a real `AppModel` as a phone receives it: the `sessions` list, the subscriptions it starts, then
/// the stage's events, drawn before the fleet is read.
@MainActor
final class RestartOrderTests: XCTestCase {
    private let a = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"
    private let b = "01K1B3XQ8ZC0DE5FGH7JKMNPQS"
    private let c = "01K1B3XQ8ZC0DE5FGH7JKMNPQT"

    private final class Sent {
        var subscribed: Set<String> = []
    }

    func testARestartLeavesTheOrderAndTheAgesWhereActivityPutThem() async throws {
        let wire = try XCTUnwrap(WireRecording.json("restart-link-state-wire") as? [String: Any])
        let (model, sent) = model()

        try await deliver(wire, "before_restart", to: model, sent: sent, list: true)
        XCTAssertEqual(order(model), [b, a], "B's tool call is the newer")
        XCTAssertEqual(row(c, model)?.lastActive, stamp(2), "C, which has done nothing, reads its launch")

        try await deliver(wire, "restart", to: model, sent: sent, list: true)
        XCTAssertEqual(order(model), [b, a], "the restart's link facts moved A above B")
        XCTAssertEqual(row(a, model)?.lastActive, stamp(3), "the restart moved A's age")
        XCTAssertEqual(row(b, model)?.lastActive, stamp(4), "the restart moved B's age")
        XCTAssertEqual(row(c, model)?.lastActive, stamp(2), "the restart moved C's age")

        // Before the next list: a real event moves its run at once, not at the next poll.
        try await deliver(wire, "activity", to: model, sent: sent, list: false)
        XCTAssertEqual(order(model), [a, b], "A's tool call after the restart did not move it up")
        XCTAssertEqual(row(a, model)?.lastActive, stamp(8))
    }

    private func model() -> (AppModel, Sent) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("restart-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(canApproveReliably: true, sendText: true, extra: ["session_uid": .bool(true)]), minor: 23)
        let sent = Sent()
        model.connection.sendStub = { message in
            if case .subscribe(let session, _) = message {
                MainActor.assumeIsolated { _ = sent.subscribed.insert(session) }
            }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        for key in [a, b, c] { addTeardownBlock { ReviewMarks.forget(sessionKey: key) } }
        return (model, sent)
    }

    /// One stage, as a phone receives it: its `sessions` list when `list` (waiting for the subscriptions that list
    /// starts, so no subscription lands after the events it would find ahead of the list), then its events, drawn.
    private func deliver(
        _ wire: [String: Any], _ name: String, to model: AppModel, sent: Sent, list: Bool
    ) async throws {
        let stage = try XCTUnwrap(wire[name] as? [String: Any], "no stage \(name)")
        if list {
            let sessions = try JSONSerialization.data(withJSONObject: try XCTUnwrap(stage["sessions"]))
            model.connection.injectForTesting(.sessions(try JSONDecoder().decode([SessionSummary].self, from: sessions)))
            try await until("\(name): every run subscribed") { [a, b, c].allSatisfy(sent.subscribed.contains) }
        }
        let events = try WireRecording.decode(try XCTUnwrap(stage["events"] as? [[String: Any]]))
        XCTAssertFalse(events.isEmpty, "\(name) has no events; the stage proves nothing")
        for event in events { model.connection.injectForTesting(.event(event)) }
        let through = Dictionary(events.map { ($0.sessionKey, $0.seq) }, uniquingKeysWith: max)
        try await until("\(name): every event drawn") {
            through.allSatisfy { model.shown(for: $0.key)?.drawnThrough == $0.value }
        }
    }

    private func until(_ what: String, _ done: () -> Bool) async throws {
        for _ in 0..<300 {
            if done() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("timed out: \(what)")
    }

    /// The running band's order: A and B start tool calls, and C, which never does, is idle.
    private func order(_ model: AppModel) -> [String] {
        model.fleet.map(\.summary.sessionKey).filter { [a, b].contains($0) }
    }

    private func row(_ key: String, _ model: AppModel) -> FleetRow? {
        model.fleet.first { $0.summary.sessionKey == key }
    }

    /// The fixture's stamp for its step `minute`.
    private func stamp(_ minute: Int) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.date(from: String(format: "2026-01-01T00:%02d:00.000Z", minute))
    }
}
