import XCTest

@testable import CodeConnectKit

/// **A run moved to another account never reads as ended** (decision 306).
///
/// ccd ends the run's process at its idle point and files a `session_end` whose payload carries `switched_to`
/// (`{"account_id", "label"}`, minor 47); the person's next message starts it again on the new account, which for a
/// Codex run files the thread's resume start. The conversation draws one quiet row, "Switched to <label>", in place of
/// "Run ended", and the first start after it draws nothing, however much later it comes; every listing in the window
/// reads live, so neither app shows the Ended band or loses the reply box.
///
/// Pinned to `fixtures/accounts/switch-claude-wire.json` and `switch-codex-wire.json` (copied byte for byte): the
/// event frames ccd broadcast across a real switch on a stand-in rig, through the next message, each followed by the
/// `sessions` frame a device listing at that instant read.
@MainActor
final class SwitchedRunTimelineTests: XCTestCase {

    private enum Frame {
        case event(Event)
        case sessions([SessionSummary])
    }

    private func frames(_ resource: String) throws -> [Frame] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: resource, withExtension: "json"))
        let all = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).arrayValue)
        return try all.map { frame in
            let object = try XCTUnwrap(frame.objectValue)
            if let event = object["event"] {
                return .event(try JSONDecoder().decode(Event.self, from: Data(event.canonicalJSONString.utf8)))
            }
            let sessions = try XCTUnwrap(object["sessions"], "a frame that is neither an event nor a listing")
            return .sessions(try JSONDecoder().decode([SessionSummary].self, from: Data(sessions.canonicalJSONString.utf8)))
        }
    }

    private func events(_ frames: [Frame]) -> [Event] {
        frames.compactMap { if case .event(let event) = $0 { event } else { nil } }
    }

    private func state(_ events: [Event]) async -> SessionState {
        let state = SessionState(sessionKey: events.first?.sessionKey ?? "")
        for event in events { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return state
    }

    private func notices(_ state: SessionState) -> [NoticeItem] {
        state.timeline.compactMap { if case .notice(let notice) = $0.content { notice } else { nil } }
    }

    /// From the switch's end on, the conversation gains exactly one row besides the person's next message: the quiet
    /// "Switched to B". The relaunch that message makes, and the link coming back, draw nothing.
    private func assertOneQuietRow(_ resource: String) async throws {
        let all = events(try frames(resource))
        let end = try XCTUnwrap(all.first { $0.kind == .sessionEnd }, "the fixture carries no end")
        XCTAssertNotNil(end.payload["switched_to"], "the end carries no typed switch")
        XCTAssertTrue(all.contains { $0.seq > end.seq && $0.kind == .userMessage }, "the fixture carries the next message")
        let drawn = await state(all)
        XCTAssertEqual(
            drawn.timeline.filter { $0.seq >= end.seq }.map(\.content).filter {
                if case .userMessage = $0 { false } else { true }
            },
            [.notice(NoticeItem(kind: .other, symbol: "arrows-clockwise", title: "Switched to B", detail: nil, severity: .info))],
            "\(resource): the switch draws one quiet row and nothing after it but the next message")
        XCTAssertFalse(
            notices(drawn).contains { $0.kind == .sessionEnded || $0.kind == .sessionStart },
            "\(resource): no Run ended, no Run resumed or Run started")
    }

    func testAClaudeSwitchDrawsOneQuietRow() async throws {
        try await assertOneQuietRow("switch-claude-wire")
    }

    func testACodexSwitchDrawsOneQuietRowAndItsResumeNothing() async throws {
        try await assertOneQuietRow("switch-codex-wire")
    }

    /// Every listing in the window reads live, with what the run had drawn by then: never the Ended band, and the reply
    /// box kept.
    func testEveryListingInTheSwitchReadsLive() async throws {
        for resource in ["switch-claude-wire", "switch-codex-wire"] {
            var seen: [Event] = []
            var listings = 0
            for frame in try frames(resource) {
                switch frame {
                case .event(let event):
                    seen.append(event)
                case .sessions(let sessions):
                    let summary = try XCTUnwrap(sessions.first)
                    let shown = await state(seen).shown
                    let status = FleetStatusRule.status(summary: summary, shown: shown, reviewedSeq: 0)
                    XCTAssertFalse(
                        status == .ended || status == .failed, "\(resource) after seq \(seen.last?.seq ?? 0): \(status)")
                    XCTAssertFalse(summary.takesNoReply, "\(resource) after seq \(seen.last?.seq ?? 0): no reply box")
                    listings += 1
                }
            }
            XCTAssertGreaterThan(listings, 0, "\(resource) carries no listing")
        }
    }
}
