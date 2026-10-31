import XCTest

@testable import CodeConnect

/// Other agents' messages reaching a Claude session.
///
/// `transcript-agent-messages-wire.json` is emitted, not written: Claude Code 2.1.316
/// hosted by CodeConnect took two messages at the end of a turn that ran a tool, through
/// its Stop hook, and then one typed into its idle box, through the daemon's production
/// tailer and store as the events a phone is sent (`ccd/src/fixture_replay.rs`
/// re-derives it).
@MainActor
final class AgentMailTests: XCTestCase {

    /// The drawn rows, less tool rows: the recording carries no hook events, so its
    /// tool call draws as the transcript alone draws it, which is not this test's subject.
    private func rows(through seq: Int = .max) throws -> [String] {
        let events = try WireRecording.events("transcript-agent-messages-wire", through: seq)
        return TimelineBuilder.drawn(TimelineBuilder.build(events)).compactMap { item in
            switch item.content {
            case .userMessage(let text, _): return "you: \(text)"
            case .agentMessage(let text, _): return "claude: \(text)"
            case .tool, .approval: return nil
            case .notice(let notice):
                return "notice: \(notice.title)" + (notice.detail.map { " | \($0)" } ?? "")
            }
        }
    }

    /// The turn's own answer, then each message it was handed at its end under who sent
    /// it, then Claude's answer to them; no "Turn complete" between, because the turn goes
    /// on into that answer. A message typed into the idle box is the same row, never the
    /// person's words.
    func testEachMessageIsARowUnderItsSenderInTheOrderClaudeTookThem() throws {
        XCTAssertEqual(
            try rows(),
            [
                "you: Run the Bash command `sleep 8`, then reply with exactly DONE.",
                "claude: DONE",
                "notice: Message from cc-2@studio | Please reply with the word ALPHA, use no tools.",
                "notice: Message from cc-2@studio | Then also reply with the word BETA, use no tools.",
                "claude: ALPHA\n\nBETA",
                "notice: Message from cc-2@studio | Reply with the word GAMMA, use no tools.",
                "claude: GAMMA",
            ])
    }

    /// Handed a message, the session is at work on it until its answer ends the turn.
    func testAHandedMessageIsActivityAndItsAnsweredTurnIsDone() async throws {
        let handed = try WireRecording.events("transcript-agent-messages-wire", through: 6)
        let timeline = TimelineBuilder.build(handed)
        guard case .notice(let last)? = FleetStatusRule.lastSubstantiveItem(in: timeline)?.content
        else { return XCTFail("the last substantive item is not the message") }
        XCTAssertEqual(last.kind, .messageFromAgent)
        // The Stop hook's summary is on disk before Claude's answer to them.
        let answering = try WireRecording.events("transcript-agent-messages-wire", through: 7)
        let working = await WireRecording.state(answering)
        XCTAssertEqual(try WireRecording.status(working), .running)
        let done = try WireRecording.events("transcript-agent-messages-wire", through: 15)
        let state = await WireRecording.state(done)
        XCTAssertEqual(try WireRecording.status(state), .doneUnreviewed)
    }

    /// The daemon's header, one message or several in one block; anything else is not
    /// CodeConnect's and reads as nothing.
    func testTheHeaderIsReadAndNothingElseIs() {
        let header = "[CodeConnect] Message from cc-2@studio (Codex · project api). Reply with "
            + "send_message to cc-2@studio.\n\n"
        XCTAssertEqual(
            AgentMail.parse(header + "first\n\nstill first"),
            [AgentMail(from: "cc-2@studio", text: "first\n\nstill first")])
        XCTAssertEqual(
            AgentMail.parse(header + "one\n\n" + header + "two"),
            [AgentMail(from: "cc-2@studio", text: "one"), AgentMail(from: "cc-2@studio", text: "two")])
        XCTAssertEqual(AgentMail.parse("please quote [CodeConnect] Message from x"), [])
        XCTAssertEqual(AgentMail.parse("[CodeConnect] Message from cc-2@studio with no body"), [])
    }
}
