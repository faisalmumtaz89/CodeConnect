import XCTest

@testable import CodeConnect

/// A daemon-emitted wire recording (`ios/CodeConnectTests/Resources/<name>.json`,
/// re-derived by `ccd/src/fixture_replay.rs`), replayed into one session the way
/// the phone builds it, and the fleet status a live, attached session reads
/// from it.
@MainActor
enum WireRecording {
    /// The session every recording is emitted under.
    static let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private final class InTheTestBundle {}

    /// The recording as it is stored: its events, or its runs by name.
    static func json(_ name: String) throws -> Any {
        let url = try XCTUnwrap(
            Bundle(for: InTheTestBundle.self).url(forResource: name, withExtension: "json"),
            "\(name).json is not in the test bundle (checked in at "
                + "ios/CodeConnectTests/Resources/)")
        return try JSONSerialization.jsonObject(with: Data(contentsOf: url))
    }

    static func decode(_ objects: [[String: Any]]) throws -> [Event] {
        try objects.map {
            try JSONDecoder().decode(
                Event.self, from: JSONSerialization.data(withJSONObject: $0))
        }
    }

    /// The recording's events up to and including the one at `seq`, as stored;
    /// `run` names one run of a recording that holds several.
    static func objects(
        _ name: String, run: String? = nil, through seq: Int = .max
    ) throws -> [[String: Any]] {
        let stored = try json(name)
        let events = try XCTUnwrap(
            run.map { (stored as? [String: Any])?[$0] } ?? stored, "no run named \(run ?? "")")
        return try XCTUnwrap(events as? [[String: Any]])
            .filter { ($0["seq"] as? Int ?? .max) <= seq }
    }

    /// The same, decoded as the phone decodes them.
    static func events(_ name: String, run: String? = nil, through seq: Int) throws -> [Event] {
        try decode(try objects(name, run: run, through: seq))
    }

    static func state(_ events: [Event]) async -> SessionState {
        let state = SessionState(sessionKey: uid)
        for event in events { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return state
    }

    static func status(_ state: SessionState, reviewedSeq: UInt64 = 0) throws -> FleetStatus {
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                #"""
                {"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "tmux_session":"cc-1","cwd":"/scratch/work","lifecycle":"live",
                 "link":"attached","last_seq":1,"created_at":"2026-10-22T00:00:00Z",
                 "updated_at":"2026-10-22T00:00:00Z","blocked_on":[]}
                """#.utf8))
        return FleetStatusRule.status(summary: summary, shown: state.shown, reviewedSeq: reviewedSeq)
    }
}
