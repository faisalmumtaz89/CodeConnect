import XCTest

@testable import CodeConnect

/// A Claude turn an API error ended.
///
/// `transcript-api-errors-wire.json` is emitted, not written: Claude Code
/// 2.1.305, recorded against a fake model, through the daemon's production
/// tailer and store as the events a phone is sent (`ccd/src/fixture_replay.rs`
/// re-derives it). Three turns end on an error — an HTTP 400 (prompt 22, error
/// 24, end 25), a repeated 529 (34, 36, 37) and a 400 after a tool call (39,
/// 44, 45) — and the Mac draws its "✻ … done" line after each. One turn carries
/// on past an error: the stream cut while a tool was being called (prompt 54,
/// error 58), then the tool's result (59), a reply (61) and the turn's end (62).
@MainActor
final class ApiErrorTurnTests: XCTestCase {

    /// The recording's events up to and including the one at `seq`.
    private func recorded(through seq: Int) throws -> [Event] {
        try WireRecording.events("transcript-api-errors-wire", through: seq)
    }

    private func state(_ events: [Event]) async -> SessionState {
        await WireRecording.state(events)
    }

    private func status(_ state: SessionState, reviewedSeq: UInt64 = 0) throws -> FleetStatus {
        try WireRecording.status(state, reviewedSeq: reviewedSeq)
    }

    /// The defect: no Stop hook runs after an API error, so the phone found no
    /// end and said Running until the next prompt — measured on real sessions,
    /// 409 hours of Running that the Mac showed as done. The end is the Mac's,
    /// right after the error's row, and the turn is new to the reader.
    func testATurnAnApiErrorEndedIsDone() async throws {
        for (name, prompt, error, end) in [
            ("HTTP 400", 22, 24, 25), ("repeated 529", 34, 36, 37), ("400 after a tool", 39, 44, 45),
        ] {
            let ended = await state(try recorded(through: end))
            XCTAssertEqual(
                try status(ended, reviewedSeq: UInt64(prompt)), .doneUnreviewed, "\(name)")
            XCTAssertEqual(try status(ended, reviewedSeq: UInt64(end)), .idle, "\(name): reviewed")

            let row = try XCTUnwrap(
                ended.timeline.firstIndex { $0.seq == UInt64(error) }, "\(name): no error row")
            guard case .agentMessage(let text, _) = ended.timeline[row].content else {
                return XCTFail("\(name): the error is not Claude's reply row")
            }
            XCTAssertTrue(text.hasPrefix("API Error: "), "\(name): \(text)")
            let next = try XCTUnwrap(
                ended.timeline.dropFirst(row + 1).first, "\(name): nothing after the error")
            guard case .notice(let notice) = next.content else {
                return XCTFail("\(name): \(next.content) after the error")
            }
            XCTAssertEqual(notice.kind, .turnComplete, "\(name)")
        }
    }

    /// An error Claude carries on past is not an end: the Mac draws no done line
    /// after it, and Claude is still working until the turn's own end.
    func testAnErrorMidTurnIsStillWork() async throws {
        for seq in [58, 59, 61] {
            let working = await state(try recorded(through: seq))
            XCTAssertEqual(try status(working, reviewedSeq: 54), .running, "through \(seq)")
        }
        let ended = await state(try recorded(through: 62))
        XCTAssertEqual(try status(ended, reviewedSeq: 54), .doneUnreviewed)
        XCTAssertEqual(try status(ended, reviewedSeq: 62), .idle)
    }
}
