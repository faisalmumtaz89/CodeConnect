import XCTest

@testable import CodeConnect

/// A built-in slash command typed after a turn ended.
///
/// `transcript-local-commands-wire.json` is emitted, not written: Claude Code
/// 2.1.308, recorded against a fake model, through the daemon's production
/// tailer and store as the events a phone is sent (`ccd/src/fixture_replay.rs`
/// re-derives it). `model-after-turn`: a turn closed (seq 9), `/model sonnet`
/// — caveat (21), invocation (22), output (23), no reply, no turn end — then a
/// typed prompt (25) answered and closed (30). `plan-goal`: a turn closed (19),
/// `/plan <description>` (22, 23) answered and closed (27), then `/goal
/// <condition>` (30, 31) answered and closed (35). `commands-mid-turn`: three
/// typed prompts, each answered after 15 s, with `/model sonnet`, `/rename`
/// and `/plan <description>` typed during them. No line of any command carries
/// `promptSource`; the `/plan` and `/goal` output lines carry `turnOrigin:
/// "human"`, the `/model` one does not.
@MainActor
final class LocalCommandStatusTests: XCTestCase {

    /// The run's events up to and including the one at `seq`.
    private func recorded(_ name: String, through seq: Int) throws -> [Event] {
        try WireRecording.events("transcript-local-commands-wire", run: name, through: seq)
    }

    private func state(_ events: [Event]) async -> SessionState {
        await WireRecording.state(events)
    }

    private func status(_ state: SessionState, reviewedSeq: UInt64 = 0) throws -> FleetStatus {
        try WireRecording.status(state, reviewedSeq: reviewedSeq)
    }

    /// The defect: `/model` after a finished turn drew its YOU row last, the
    /// row is not a turn's end, and the fleet said Running until the next
    /// prompt. The Mac shows the command's one-line answer and an empty
    /// prompt — no spinner — and Claude never replies.
    func testAModelSwitchAfterAFinishedTurnIsNotWork() async throws {
        let switched = await state(try recorded("model-after-turn", through: 23))
        XCTAssertTrue(
            switched.timeline.contains {
                $0.seq == 22 && $0.content == .userMessage("/model sonnet", isCommand: true)
            }, "the command still draws its row")
        XCTAssertEqual(try status(switched, reviewedSeq: 9), .idle)
        XCTAssertEqual(try status(switched), .doneUnreviewed, "the turn before it is still new")
    }

    /// The prompt typed after it is work, and its end is a new turn to read.
    func testThePromptAfterTheSwitchIsWork() async throws {
        let prompted = await state(try recorded("model-after-turn", through: 25))
        XCTAssertEqual(try status(prompted, reviewedSeq: 9), .running)
        let answered = await state(try recorded("model-after-turn", through: 30))
        XCTAssertEqual(try status(answered, reviewedSeq: 9), .doneUnreviewed)
        XCTAssertEqual(try status(answered, reviewedSeq: 30), .idle)
    }

    /// `/plan <description>` asks the model: Claude replies to it as to a
    /// prompt, and the session is working from the command's output line on.
    func testAPlanCommandStartsATurn() async throws {
        let before = await state(try recorded("plan-goal", through: 19))
        XCTAssertEqual(try status(before, reviewedSeq: 19), .idle)
        let planned = await state(try recorded("plan-goal", through: 23))
        XCTAssertEqual(try status(planned, reviewedSeq: 19), .running)
        let answered = await state(try recorded("plan-goal", through: 27))
        XCTAssertEqual(try status(answered, reviewedSeq: 19), .doneUnreviewed)
        XCTAssertEqual(try status(answered, reviewedSeq: 27), .idle)
    }

    /// `/goal <condition>` does too, and its turn holds through the line
    /// Claude Code writes for the model until the Stop hook's summary ends it.
    func testAGoalCommandStartsATurn() async throws {
        let set = await state(try recorded("plan-goal", through: 31))
        XCTAssertEqual(try status(set, reviewedSeq: 27), .running)
        let told = await state(try recorded("plan-goal", through: 32))
        XCTAssertEqual(try status(told, reviewedSeq: 27), .running)
        let answered = await state(try recorded("plan-goal", through: 35))
        XCTAssertEqual(try status(answered, reviewedSeq: 27), .doneUnreviewed)
        XCTAssertEqual(try status(answered, reviewedSeq: 35), .idle)
    }

    /// Commands typed while a turn runs leave it running to its end:
    /// `/model` writes no line (seq 5–19), `/rename` a reminder for the model
    /// before the end (22–31), and `/plan` waits for the end (38) and then
    /// starts a turn of its own (40–43). Every event from a prompt to its
    /// turn's end reads Running, and every other one Done. Not the `/plan`
    /// invocation alone (40): its output is written in the same millisecond.
    func testACommandTypedMidTurnLeavesItRunning() async throws {
        let working = Set(5...18).union(22...30).union(33...37).union(41...42)
        for seq in 5...43 where seq != 40 {
            let state = await state(try recorded("commands-mid-turn", through: seq))
            XCTAssertEqual(
                try status(state), working.contains(seq) ? .running : .doneUnreviewed,
                "through seq \(seq)")
        }
    }

    /// Which recorded user lines start a turn: the typed prompts and the
    /// output lines of the two commands that ask the model — never a
    /// command's invocation, its caveat, `/model`'s output, or the note
    /// `/goal` writes for the model.
    func testOnlyACommandThatAsksTheModelStartsATurn() throws {
        XCTAssertEqual(
            try recorded("model-after-turn", through: .max).filter(\.startsTurn).map(\.seq),
            [6, 25])
        XCTAssertEqual(
            try recorded("plan-goal", through: .max).filter(\.startsTurn).map(\.seq),
            [5, 23, 31])
        XCTAssertEqual(
            try recorded("commands-mid-turn", through: .max).filter(\.startsTurn).map(\.seq),
            [5, 22, 33, 41])
    }

    // MARK: Skills

    /// A skill invoked after a finished turn is work, though its line carries
    /// neither `promptSource` nor `turnOrigin`.
    ///
    /// `transcript-skill-invocation-wire.json` is emitted through the daemon's
    /// production tailer and store, and `ccd/src/fixture_replay.rs` re-derives
    /// it from `transcript/skill-invocation-unstamped.jsonl`, built on
    /// `plan-goal-2.1.308.jsonl`: a turn closed (19), then a skill's
    /// `<command-message>`-first line with no stamp (21), the skill's `isMeta`
    /// body (22), Claude's reply (23) and the turn's end (24) — the shape
    /// Claude Code 2.1.220 wrote for `/code-review`, the one unstamped line of
    /// the six real skill lines, every one of which was answered.
    func testASkillInvokedAfterAFinishedTurnIsWork() async throws {
        func recorded(through seq: Int) throws -> [Event] {
            try WireRecording.events("transcript-skill-invocation-wire", through: seq)
        }
        let invoked = await state(try recorded(through: 21))
        XCTAssertTrue(
            invoked.timeline.contains {
                $0.seq == 21
                    && $0.content == .userMessage("/code-review synthetic arguments", isCommand: true)
            }, "the skill still draws as its command")
        XCTAssertEqual(try status(invoked, reviewedSeq: 19), .running)
        let meta = await state(try recorded(through: 22))
        XCTAssertEqual(try status(meta, reviewedSeq: 19), .running)
        let answered = await state(try recorded(through: 24))
        XCTAssertEqual(try status(answered, reviewedSeq: 19), .doneUnreviewed)
    }
}
