import XCTest

@testable import CodeConnectKit

/// **What the Kit offers on a Codex run CodeConnect started, over the run's life as the daemon reports it.**
///
/// `compose-stop-wire.json` (copied byte for byte from `fixtures/codex/`) holds the capabilities a paired phone is
/// offered and `fleet_over_a_run`: a run started on the engine, step by step, each step's summary as the fleet sends it
/// and the events filed since the step before. The capabilities and the summaries are the daemon's own bytes. The
/// events are not: the fixture keeps only their kind, link and turn, so each is built here from those — **derived**:
/// its seq from its order, its session from the step's summary, a fixed time, the source a real one has (`daemon` for a
/// link fact, `codex` otherwise), and a payload of its link or nothing. The rules below read an event's kind and turn,
/// and a link fact's seq only against the summary's `last_seq`. The events go after the app has subscribed to the
/// run, as the daemon sends a run's events only on its subscription.
///
/// A summary a test writes can agree with a rule that the one the daemon sends does not (X8-1: the fleet read a
/// started run's `codex_link` as `none`, so neither app could send to it or stop it).
@MainActor
final class CodexAdmissionWireTests: XCTestCase {

    private struct Offered: Equatable {
        var compose: Bool
        var stop: Bool
        var endRun: Bool
    }

    private final class Sent {
        /// How many times the model asked the daemon for the fleet.
        var fleetAsks = 0
        /// The runs the model subscribed to.
        var subscribed: Set<String> = []
    }

    /// The run's whole life: the composer takes a message while its link is subscribed, Stop is offered while a turn
    /// runs, End run while it is live, and nothing once it is ending or has ended.
    func testWhatEachStepOfACodexRunOffers() async throws {
        let (model, sent) = try model()
        let steps = try fleetOverARun()
        let expected: [(String, Offered)] = [
            ("started", Offered(compose: false, stop: false, endRun: false)),
            ("first message taken", Offered(compose: true, stop: true, endRun: true)),
            ("ccd restarted, before its reconnect", Offered(compose: false, stop: false, endRun: true)),
            ("reconnected", Offered(compose: true, stop: false, endRun: true)),
            ("End run", Offered(compose: false, stop: false, endRun: false)),
            ("ended", Offered(compose: false, stop: false, endRun: false)),
        ]
        XCTAssertEqual(steps.map { $0["step"] as? String }, expected.map(\.0), "the fixture's steps")
        // The first list answers a request, as every list does.
        model.refreshFleet()
        var seq: UInt64 = 0
        for (step, (name, offered)) in zip(steps, expected) {
            let asks = sent.fleetAsks
            try await deliverEvents(step, after: &seq, to: model)
            if name == "first message taken" {
                XCTAssertEqual(sent.fleetAsks, asks + 1, "the attach fact did not ask for the fleet")
            }
            try await answer(step, to: model, sent: sent)
            XCTAssertEqual(try self.offered(model), offered, name)
        }
    }

    /// **The X8-1 state**: the same run with its first message taken, but the fleet reading its link as `none`, as
    /// it did before the daemon filled `codex_link` for an engine run. Neither the composer nor Stop is offered.
    func testARunWhoseLinkTheFleetReadsAsNoneTakesNoMessageAndNoStop() async throws {
        let (model, sent) = try model()
        let steps = try fleetOverARun()
        model.refreshFleet()
        var seq: UInt64 = 0
        try await deliverEvents(steps[0], after: &seq, to: model)
        try await answer(steps[0], to: model, sent: sent)
        try await deliverEvents(steps[1], after: &seq, to: model)
        // Derived: step 1's summary as the daemon sent it, with only its link changed.
        var summary = try XCTUnwrap(steps[1]["summary"] as? [String: Any])
        summary["codex_link"] = "none"
        try await answer(["summary": summary], to: model, sent: sent)
        XCTAssertNotNil(model.shown(for: key)?.runningTurn, "a turn is running")
        XCTAssertEqual(
            model.composeUnavailable(for: key), CodexLinkState.none.blockedReason(for: .compose),
            "the composer is refused for the link")
        XCTAssertEqual(
            model.stopUnavailable(for: key), CodexLinkState.none.blockedReason(for: .stop), "Stop is refused for the link")
    }

    private let key = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private func fixture() throws -> [String: Any] {
        try XCTUnwrap(WireRecording.json("compose-stop-wire") as? [String: Any])
    }

    private func fleetOverARun() throws -> [[String: Any]] {
        try XCTUnwrap(try fixture()["fleet_over_a_run"] as? [[String: Any]])
    }

    /// A model connected to a daemon that advertises the fixture's capabilities (this build's minor, 39), counting
    /// the fleet requests and the subscriptions it sends.
    private func model() throws -> (AppModel, Sent) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("codex-admission-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        let capabilities = try JSONDecoder().decode(
            Capabilities.self,
            from: JSONSerialization.data(withJSONObject: try XCTUnwrap(try fixture()["hello_ack_capabilities"])))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(capabilities, minor: 39)
        let sent = Sent()
        model.connection.sendStub = { message in
            if case .sessions = message { MainActor.assumeIsolated { sent.fleetAsks += 1 } }
            if case .subscribe(let session, _) = message {
                MainActor.assumeIsolated { _ = sent.subscribed.insert(session) }
            }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return (model, sent)
    }

    /// A step's events, built from its records (derived, as the class says), delivered and drawn.
    private func deliverEvents(_ step: [String: Any], after seq: inout UInt64, to model: AppModel) async throws {
        let summary = try XCTUnwrap(step["summary"] as? [String: Any])
        let records = try XCTUnwrap(step["events"] as? [[String: Any]])
        let events = try WireRecording.decode(
            records.map { record in
                seq += 1
                let isLink = record["kind"] as? String == "link_state"
                return [
                    "seq": seq, "session_uid": summary["session_uid"] as Any,
                    "session_id": summary["session_id"] as Any, "ts": "2026-11-15T00:00:00Z",
                    "kind": record["kind"] as Any, "source": isLink ? "daemon" : "codex",
                    "payload": isLink ? ["link": record["link"] as Any] : [:],
                    "source_event_id": record["source_event_id"] as Any, "turn_id": record["turn_id"] as Any,
                ]
            })
        for event in events { model.connection.injectForTesting(.event(event)) }
        guard !events.isEmpty else { return }
        try await until("events through \(seq) drawn") { model.shown(for: key)?.drawnThrough == seq }
    }

    /// A step's summary, as the daemon answers a request for the fleet, and the run's subscription that the first
    /// list starts.
    private func answer(_ step: [String: Any], to model: AppModel, sent: Sent) async throws {
        let summary = try JSONDecoder().decode(
            SessionSummary.self, from: JSONSerialization.data(withJSONObject: try XCTUnwrap(step["summary"])))
        model.connection.injectForTesting(.sessions([summary]))
        try await until("the fleet's summary reads \(summary.lifecycle)") {
            model.shownSummary(for: key) == summary
        }
        try await until("the run subscribed") { sent.subscribed.contains(key) }
    }

    /// What the run offers as drawn.
    private func offered(_ model: AppModel) throws -> Offered {
        let summary = try XCTUnwrap(model.shownSummary(for: key))
        return Offered(
            compose: model.composeUnavailable(for: key) == nil,
            stop: model.stopUnavailable(for: key) == nil,
            endRun: model.canEndRun(summary))
    }

    private func until(_ what: String, _ done: () -> Bool) async throws {
        for _ in 0..<300 {
            if done() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("timed out: \(what)")
    }
}
