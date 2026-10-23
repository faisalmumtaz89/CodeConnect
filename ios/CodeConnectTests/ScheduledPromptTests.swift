import XCTest

@testable import CodeConnect

/// A prompt that starts a turn and draws no row.
///
/// `transcript-scheduled-prompt-wire.json` is emitted, not written: Claude Code
/// 2.1.305, recorded against a fake model, scheduling a one-shot prompt that
/// fires with nothing typed — an `isMeta` user line stamped `promptSource:
/// "system"` (seq 30), answered (31) and closed (32) — between two typed
/// prompts, through the daemon's production tailer and store as the events a
/// phone is sent (`ccd/src/fixture_replay.rs` re-derives it).
@MainActor
final class ScheduledPromptTests: XCTestCase {

    /// The recording's events up to and including the one at `seq`.
    private func recorded(through seq: Int) throws -> [Event] {
        try WireRecording.events("transcript-scheduled-prompt-wire", through: seq)
    }

    private func state(_ events: [Event]) async -> SessionState {
        await WireRecording.state(events)
    }

    private func status(_ state: SessionState, reviewedSeq: UInt64 = 0) throws -> FleetStatus {
        try WireRecording.status(state, reviewedSeq: reviewedSeq)
    }

    /// The defect: the prompt fires after a finished turn and draws nothing, so
    /// the previous turn's end was the last row and the fleet said Done while
    /// Claude worked on it.
    func testAScheduledPromptAfterAFinishedTurnIsWork() async throws {
        let fired = await state(try recorded(through: 30))
        XCTAssertFalse(
            fired.timeline.contains { $0.seq == 30 }, "the scheduled prompt draws no row")
        XCTAssertEqual(try status(fired), .running)
        XCTAssertEqual(try status(fired, reviewedSeq: 29), .running, "reviewed, it is still work")
    }

    /// Before it fires the session is done; once its turn closes it is done
    /// again, and that turn is new to the reader.
    func testTheTurnsAroundItAreDone() async throws {
        let before = await state(try recorded(through: 29))
        XCTAssertEqual(try status(before, reviewedSeq: 29), .idle)
        let answered = await state(try recorded(through: 32))
        XCTAssertEqual(try status(answered, reviewedSeq: 29), .doneUnreviewed)
        XCTAssertEqual(try status(answered, reviewedSeq: 32), .idle)
    }

    /// Resuming a session whose last prompt went unanswered, Claude Code writes
    /// its `<synthetic>` placeholder reply: that prompt will never be answered.
    /// The emitted placeholder of the recorded resume, renumbered to follow.
    func testAPromptClaudeAnnulledIsNotWork() async throws {
        let runs = try XCTUnwrap(
            try WireRecording.json("transcript-interrupts-wire") as? [String: Any])
        let resume = try XCTUnwrap(runs["resume-after-esc"] as? [[String: Any]])
        var placeholder = try XCTUnwrap(
            resume.first {
                let payload = $0["payload"] as? [String: Any]
                let message = payload?["message"] as? [String: Any]
                return message?["model"] as? String == "<synthetic>"
            })
        placeholder["seq"] = 31
        let annulled = await state(try recorded(through: 30) + WireRecording.decode([placeholder]))
        XCTAssertTrue(try WireRecording.decode([placeholder])[0].isClaudePlaceholderReply)
        XCTAssertEqual(try status(annulled, reviewedSeq: 25), .idle)
    }

    /// A log that rewound starts clean, and no prompt from before survives it.
    func testARewoundLogForgetsThePrompt() async throws {
        let fired = await state(try recorded(through: 30))
        XCTAssertEqual(fired.shown.lastPromptSeq, 30)
        fired.resetForRewoundLog()
        XCTAssertEqual(fired.shown.lastPromptSeq, 0)
    }

    /// Which recorded user lines Claude Code handed the agent as a prompt: the
    /// typed ones and the report, never the lines it wrote for the model — Stop
    /// hook feedback (seq 49, 59), which belongs to the turn still running, and
    /// the `/rename` reminder (78), written after the turn's Stop hook with no
    /// answer to come.
    func testOnlyWhatClaudeSubmittedIsAPrompt() throws {
        let lines = try WireRecording.decode(
            try XCTUnwrap(try WireRecording.json("transcript-user-lines-wire") as? [[String: Any]]))
        XCTAssertEqual(
            lines.filter { !$0.startsTurn }.map(\.seq), [49, 59, 78])

        let scheduled = try recorded(through: 34).filter(\.startsTurn).map(\.seq)
        XCTAssertEqual(scheduled, [5, 30, 34], "two typed prompts and the scheduled one")
    }
}
