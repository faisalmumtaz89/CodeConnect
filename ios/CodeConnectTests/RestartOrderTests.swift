import XCTest

@testable import CodeConnectKit

/// **A daemon restart is not activity, on the phone's side.** `restart-link-state-wire.json` is what a daemon emits
/// across a restart (frozen, as an earlier daemon emitted it; never regenerated: `fixtures/README.md`):
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
    /// `restart-reattach-wire.json`'s Codex runs: A in proj-alpha, B in proj-notes.
    private let codexA = "01M87AXA2PJ0TKMX3KZT5YKJZY"
    private let codexB = "01M87AXAS9BEYEWMZEH2FFTTP2"

    private final class Sent {
        var subscribed: Set<String> = []
        /// How many times the model asked the daemon for the fleet.
        var fleetAsks = 0
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

    /// **A Codex run's reattach is not activity either.** `restart-reattach-wire.json`, the daemon's own frames: Codex
    /// runs A and B each finish a turn, B last; the daemon restarts and files, for each, its settings again
    /// (`codex_settings`) and a link fact, at the restart's time; then A finishes another turn.
    func testACodexReattachLeavesTheOrderAndTheAgesWhereActivityPutThem() async throws {
        let wire = try XCTUnwrap(WireRecording.json("restart-reattach-wire") as? [String: Any])
        let (model, sent) = model()
        let runs = [codexA, codexB]

        try await deliver(wire, "before_restart", to: model, sent: sent, list: true)
        XCTAssertEqual(order(model, among: runs), [codexB, codexA], "B's turn is the newer")
        let ageA = row(codexA, model)?.lastActive
        let ageB = row(codexB, model)?.lastActive

        try await deliver(wire, "restart", to: model, sent: sent, list: true)
        XCTAssertEqual(row(codexA, model)?.lastActive, ageA, "the reattach moved A's age")
        XCTAssertEqual(row(codexB, model)?.lastActive, ageB, "the reattach moved B's age")
        XCTAssertEqual(order(model, among: runs), [codexB, codexA], "the reattach reordered the runs")

        try await deliver(wire, "activity", to: model, sent: sent, list: false)
        XCTAssertEqual(order(model, among: runs), [codexA, codexB], "A's turn after the restart did not move it up")
    }

    /// **A link fact or an end the list has not seen asks for the fleet at once**, and one the list already holds asks
    /// nothing. The daemon sends a list only when asked, so a run whose link attached a moment after the list was taken
    /// (a start, a restart's re-attach) or that has just ended kept its earlier state in the fleet until the next timed
    /// refresh. A burst costs two requests at most: the first fact asks, the rest wait for its answer, and one more
    /// follows it.
    func testALinkFactOrAnEndTheListHasNotSeenAsksForTheFleet() async throws {
        let wire = try XCTUnwrap(WireRecording.json("restart-link-state-wire") as? [String: Any])
        let (model, sent) = model()
        // The first list answers a request, as every list does.
        model.refreshFleet()

        try await deliver(wire, "before_restart", to: model, sent: sent, list: true)
        XCTAssertEqual(sent.fleetAsks, 1, "a link fact the list already holds asked for the fleet")

        try await deliver(wire, "restart", to: model, sent: sent, list: false)
        XCTAssertEqual(sent.fleetAsks, 2, "the restart's three re-attach facts did not fold into one request")

        try list(wire, "restart", to: model)
        XCTAssertEqual(sent.fleetAsks, 3, "no follow-up went when the answer caught up with the facts that waited")

        try await deliver(wire, "activity", to: model, sent: sent, list: false)
        XCTAssertEqual(sent.fleetAsks, 3, "a tool call asked for the fleet")

        try list(wire, "activity", to: model)
        XCTAssertEqual(sent.fleetAsks, 3, "an answer with no fact waiting asked again")

        try await deliver(wire, "end", to: model, sent: sent, list: false)
        XCTAssertEqual(sent.fleetAsks, 4, "the run's end did not ask for the fleet")
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
            if case .sessions = message { MainActor.assumeIsolated { sent.fleetAsks += 1 } }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        for key in [a, b, c, codexA, codexB] { addTeardownBlock { ReviewMarks.forget(sessionKey: key) } }
        return (model, sent)
    }

    /// One stage, as a phone receives it: its `sessions` list when `list` (waiting for the subscriptions that list
    /// starts, so no subscription lands after the events it would find ahead of the list), then its events, drawn.
    private func deliver(
        _ wire: [String: Any], _ name: String, to model: AppModel, sent: Sent, list: Bool
    ) async throws {
        let stage = try XCTUnwrap(wire[name] as? [String: Any], "no stage \(name)")
        if list {
            try self.list(wire, name, to: model)
            let runs = (stage["sessions"] as? [[String: Any]] ?? []).compactMap { $0["session_uid"] as? String }
            try await until("\(name): every run subscribed") { runs.allSatisfy(sent.subscribed.contains) }
        }
        let events = try WireRecording.decode(try XCTUnwrap(stage["events"] as? [[String: Any]]))
        XCTAssertFalse(events.isEmpty, "\(name) has no events; the stage proves nothing")
        for event in events { model.connection.injectForTesting(.event(event)) }
        let through = Dictionary(events.map { ($0.sessionKey, $0.seq) }, uniquingKeysWith: max)
        try await until("\(name): every event drawn") {
            through.allSatisfy { model.shown(for: $0.key)?.drawnThrough == $0.value }
        }
    }

    /// A stage's `sessions` list, as the daemon answers a request for the fleet.
    private func list(_ wire: [String: Any], _ name: String, to model: AppModel) throws {
        let stage = try XCTUnwrap(wire[name] as? [String: Any], "no stage \(name)")
        let sessions = try JSONSerialization.data(withJSONObject: try XCTUnwrap(stage["sessions"]))
        model.connection.injectForTesting(.sessions(try JSONDecoder().decode([SessionSummary].self, from: sessions)))
    }

    private func until(_ what: String, _ done: () -> Bool) async throws {
        for _ in 0..<300 {
            if done() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("timed out: \(what)")
    }

    /// The fleet's order of `runs`: by default the running band's, where A and B start tool calls and C, which never
    /// does, is idle.
    private func order(_ model: AppModel, among runs: [String]? = nil) -> [String] {
        let runs = runs ?? [a, b]
        return model.fleet.map(\.summary.sessionKey).filter { runs.contains($0) }
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
