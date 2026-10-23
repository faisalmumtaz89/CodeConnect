import XCTest

@testable import CodeConnect

/// A typed prompt whose pasted image takes it over the daemon's payload limit.
///
/// `transcript-big-image-prompt-wire.json` is emitted, not written: Claude Code
/// 2.1.308, two typed prompts with a pasted image each (seq 5 and 12), each
/// answered (9, 16) and closed (10, 17), through the daemon's production tailer
/// and ingest with its payload limit at 4 KiB, as the events a phone is sent
/// (`ccd/src/state.rs` re-derives it). The second prompt is over the limit and
/// arrives with its fields and words, its image reduced to `{"type": "image"}`.
@MainActor
final class BigImagePromptTests: XCTestCase {

    /// The recording's events up to and including the one at `seq`.
    private func recorded(through seq: Int) throws -> [Event] {
        try WireRecording.events("transcript-big-image-prompt-wire", through: seq)
    }

    private func state(_ events: [Event]) async -> SessionState {
        await WireRecording.state(events)
    }

    private func status(_ state: SessionState, reviewedSeq: UInt64 = 0) throws -> FleetStatus {
        try WireRecording.status(state, reviewedSeq: reviewedSeq)
    }

    private func prompts(_ state: SessionState) -> [UInt64: String] {
        var rows: [UInt64: String] = [:]
        for item in state.timeline {
            if case .userMessage(let text, _) = item.content { rows[item.seq] = text }
        }
        return rows
    }

    /// The defect: the daemon replaced the prompt whole, so it drew no YOU row,
    /// the previous turn's end was the last row, and the fleet said Done while
    /// Claude worked on it.
    func testAPromptOverTheLimitDrawsItsRowAndIsWork() async throws {
        let sent = await state(try recorded(through: 12))
        XCTAssertEqual(
            prompts(sent),
            [
                5: "[Image #1] SLOW what is in this picture",
                12: "[Image #2] SLOW and this bigger one",
            ])
        XCTAssertEqual(try status(sent), .running)
        XCTAssertEqual(try status(sent, reviewedSeq: 11), .running, "reviewed, it is still work")
    }

    /// Answered and closed, its turn is done like any other.
    func testItsTurnEndsLikeAnyOther() async throws {
        let answered = await state(try recorded(through: 17))
        XCTAssertEqual(try status(answered, reviewedSeq: 11), .doneUnreviewed)
        XCTAssertEqual(try status(answered, reviewedSeq: 17), .idle)
    }
}
