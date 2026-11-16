import XCTest

@testable import CodeConnectKit

/// **A Claude run's mail wake draws as the row a Codex wake draws.**
///
/// When ccd wakes a Claude run CodeConnect started for its mail, it files its own `mail_nudge` notification, whose
/// message is the wake line, and hands the line to the host; claude's echo of the line files nothing. The builder draws
/// the notice as "Reading CodeConnect mail", a task report, so the woken run reads Running until it replies, and never
/// as a row named after the notice's type. No row reads the line as the person's words.
///
/// Pinned to `fixtures/claude/mail-wake-wire.json` (copied byte for byte): a hosted run's events as the wire delivers
/// them (its first turn, ccd's notice, the woken turn) and the run's fleet summary after the woken turn.
@MainActor
final class ClaudeMailWakeTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "mail-wake-wire", withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func decoded<T: Decodable>(_ type: T.Type, _ key: String) throws -> T {
        let value = try XCTUnwrap(try wire()[key], "no \(key) in mail-wake-wire.json")
        return try JSONDecoder().decode(T.self, from: Data(value.canonicalJSONString.utf8))
    }

    private func events() throws -> [Event] { try decoded([Event].self, "session_hosted_live_events") }

    /// The row a Codex wake draws (Timeline.swift); `MailWakeNudgeTests` pins its title and kind for Codex.
    private let mailRow = TimelineItem.Content.notice(
        NoticeItem(
            kind: .taskReport, symbol: "chat-text", title: "Reading CodeConnect mail", detail: nil, severity: .info))

    /// Where ccd's notice is, and the wake line it carries.
    private func wake(in events: [Event]) throws -> (index: Int, line: String) {
        let index = try XCTUnwrap(
            events.firstIndex { $0.kind == .notification && $0.notificationType == EngineNotice.mailNudge },
            "the fixture carries no mail_nudge notice")
        return (index, try XCTUnwrap(events[index].notificationMessage, "the notice carries no wake line"))
    }

    private func state(_ events: [Event]) async -> SessionState {
        let state = SessionState(sessionKey: events.first?.sessionKey ?? "")
        for event in events { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return state
    }

    func testTheWakeDrawsAsTheMailNotice() async throws {
        let all = try events()
        let (index, line) = try wake(in: all)
        let drawn = await state(all)
        XCTAssertEqual(
            drawn.timeline.filter { $0.seq == all[index].seq }.map(\.content), [mailRow],
            "ccd's notice draws the Codex wake's row")
        for item in drawn.timeline {
            if case .userMessage(let text, _) = item.content {
                XCTAssertFalse(text.contains(line), "a row reads the wake line as the person's words: \(text)")
            }
        }
    }

    /// Before the agent replies (every event through the notice), the run reads Running, and the fleet's preview is the
    /// notice: a row that is not activity would leave the run reading done with its first turn.
    func testAWokenRunReadsRunningUntilItReplies() async throws {
        let all = try events()
        let (index, _) = try wake(in: all)
        let woken = await state(Array(all[...index]))
        XCTAssertEqual(
            FleetStatusRule.status(
                summary: try decoded(SessionSummary.self, "session_hosted_live"), shown: woken.shown, reviewedSeq: 0),
            .running, "a mail-woken run reads Running")
        XCTAssertEqual(woken.shown.timeline.last?.content, mailRow, "the fleet's preview is the notice")
    }
}
