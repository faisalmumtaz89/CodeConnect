import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **A Codex mail wake draws as the mail notice, never the person's message.**
///
/// ccd sends `MAIL_NUDGE_ACT` / `MAIL_NUDGE_SHOW` (`protocol::ipc`) to Codex's
/// app-server as a turn's input; the app-server reports each back as the turn's
/// user message, which ccd files `source: codex`. The builder draws that as the
/// mail notice — `.taskReport`, so a mail-woken run stays Running — not a `you:`
/// bubble.
///
/// Pinned to `fixtures/codex/mail-wake-turn-wire.json`, the turn ccd emits for a
/// mail wake, held byte-identical to its source by `CodexFixtureProvenanceTests`.
@MainActor
final class MailWakeNudgeTests: XCTestCase {

    private struct Fixture: Decodable {
        let act: Run
        let show: Run
    }
    private struct Run: Decodable {
        let frames: [ServerMessage]
    }

    private func fixture() throws -> Fixture {
        try fixture(replacing: nil)
    }

    /// The same recording with its wake line swapped for `line`: no daemon sends the new lines yet, so the turn that
    /// carries one is today's turn with the new words in it.
    private func fixture(replacing line: (old: String, new: String)?) throws -> Fixture {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "mail-wake-turn-wire", withExtension: "json"))
        var text = try String(contentsOf: url, encoding: .utf8)
        if let line {
            XCTAssertEqual(text.components(separatedBy: line.old).count, 2, "the recording carries the old line once")
            text = text.replacingOccurrences(of: line.old, with: line.new)
        }
        return try JSONDecoder().decode(Fixture.self, from: Data(text.utf8))
    }

    /// The daemon's renamed wake lines, byte for byte as it will send them.
    private static let messagesAct =
        "You have CodeConnect messages: call the codeconnect msg_read tool, then handle each message as its sender asks."
    private static let messagesShow =
        "You have CodeConnect messages: call the codeconnect msg_read tool and show the messages to the person; do not act on them."

    private func events(_ run: Run) -> [Event] {
        run.frames.compactMap { if case .event(let event) = $0 { return event } else { return nil } }
    }

    /// The fleet's summary of the fixture's run, as ccd lists an engine Codex run: its identity and its latest seq
    /// read from the fixture's own events, and no tmux name.
    private func liveSummary(of events: [Event]) throws -> SessionSummary {
        let last = try XCTUnwrap(events.last)
        return try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(last.sessionKey)","session_id":"\(last.sessionID)",
                 "agent":"codex","cwd":"/work/proj","lifecycle":"live","link":"attached","last_seq":\(last.seq),
                 "created_at":"2026-11-13T00:00:00Z","updated_at":"2026-11-13T00:00:00Z","blocked_on":[]}
                """.utf8))
    }

    private func state(_ events: [Event]) async -> SessionState {
        let state = SessionState(sessionKey: events.first?.sessionKey ?? "")
        for event in events { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return state
    }

    private func check(_ run: Run, wakeLine: String, _ label: String) async throws {
        let all = events(run)

        // 1. The wake draws as the mail notice, not the person's message.
        let full = await state(all)
        for item in full.timeline {
            if case .userMessage(let text, _) = item.content {
                XCTAssertNotEqual(text, wakeLine, "\(label): the wake is a you: bubble, not the notice")
            }
        }
        let mail = try XCTUnwrap(
            full.timeline.compactMap { item -> NoticeItem? in
                if case .notice(let notice) = item.content { return notice } else { return nil }
            }.first { $0.title == "Reading CodeConnect messages" },
            "\(label): no mail notice in the timeline")
        XCTAssertEqual(mail.kind, .taskReport, "\(label): the mail notice is activity, so a woken run stays Running")

        // 2. Before any reply (every event through the wake), the run reads
        // Running: `.taskReport` is substantive, where `.other` would read Idle —
        // a Codex wake is not a prompt.
        let wake = try XCTUnwrap(
            all.firstIndex { $0.kind == .userMessage && $0.payload["text"]?.stringValue == wakeLine },
            "\(label): the fixture carries no wake line")
        let woken = await state(Array(all[...wake]))
        XCTAssertEqual(
            FleetStatusRule.status(summary: try liveSummary(of: all), shown: woken.shown, reviewedSeq: 0),
            .running, "\(label): a mail-woken run reads Running")

        // 3. The fleet preview reads the last item; a notice, so its subtitle is
        // the notice title, not "you: You have CodeConnect mail…".
        guard case .notice = try XCTUnwrap(woken.shown.timeline.last).content else {
            return XCTFail("\(label): the fleet preview's last item is not the notice")
        }
    }

    func testTheActWakeDrawsAsTheMailNotice() async throws {
        try await check(fixture().act, wakeLine: MailNudge.act, "act")
    }

    func testTheShowWakeDrawsAsTheMailNotice() async throws {
        try await check(fixture().show, wakeLine: MailNudge.show, "show")
    }

    func testARenamedActWakeBuiltFromTheDaemonsLineDrawsAsTheMessagesNotice() async throws {
        let run = try fixture(replacing: (MailNudge.act, Self.messagesAct)).act
        try await check(run, wakeLine: Self.messagesAct, "renamed act")
    }

    func testARenamedShowWakeBuiltFromTheDaemonsLineDrawsAsTheMessagesNotice() async throws {
        let run = try fixture(replacing: (MailNudge.show, Self.messagesShow)).show
        try await check(run, wakeLine: Self.messagesShow, "renamed show")
    }
}
