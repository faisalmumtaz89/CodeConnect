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

    /// The fixture's own `session_uid`, so `ingest` keys its events to the state.
    private static let uid = "01K1B3XQ8ZC0DE5FGH7MAWAKE0"

    private func fixture() throws -> Fixture {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "mail-wake-turn-wire", withExtension: "json"))
        return try JSONDecoder().decode(Fixture.self, from: Data(contentsOf: url))
    }

    private func events(_ run: Run) -> [Event] {
        run.frames.compactMap { if case .event(let event) = $0 { return event } else { return nil } }
    }

    private func liveSummary() -> SessionSummary {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(Self.uid)","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/work/proj","lifecycle":"live","link":"attached","last_seq":5,
                 "created_at":"2026-11-13T00:00:00Z","updated_at":"2026-11-13T00:00:00Z","blocked_on":[]}
                """.utf8))
    }

    private func state(_ events: [Event]) async -> SessionState {
        let state = SessionState(sessionKey: Self.uid)
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
            }.first { $0.title == "Reading CodeConnect mail" },
            "\(label): no mail notice in the timeline")
        XCTAssertEqual(mail.kind, .taskReport, "\(label): the mail notice is activity, so a woken run stays Running")

        // 2. Before any reply (session_start + the wake), the run reads Running:
        // `.taskReport` is substantive, where `.other` would read Idle — a Codex
        // wake is not a prompt.
        let woken = await state(Array(all.prefix(2)))
        XCTAssertEqual(
            FleetStatusRule.status(summary: liveSummary(), shown: woken.shown, reviewedSeq: 0),
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
}
