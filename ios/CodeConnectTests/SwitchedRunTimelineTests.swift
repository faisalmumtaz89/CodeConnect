import XCTest

@testable import CodeConnectKit

/// **A run moved to another account looks exactly as before** (decision 306; decision 311 v4, minor 52).
///
/// The person's message moves a quiet run: ccd ends its process and files a `session_end` whose payload carries
/// `switched_to` (`{"account_id", "label"}`, minor 48), then the message starts it again on the new account, which files
/// no start a page could draw (a Claude start names no source; a Codex run files none). The conversation draws nothing
/// for any of it but the message itself;
/// every listing in the window reads live, linked and with its menus, so neither app shows the Ended band, loses the
/// reply box or goes read only.
///
/// Pinned to `fixtures/accounts/switch-claude-wire.json` and `switch-codex-wire.json` (copied byte for byte): the
/// event frames ccd broadcast across a real move on a stand-in rig, through the message, each followed by the
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

    /// From the switch's end on, the conversation gains nothing but the person's message: the end, the relaunch that
    /// message makes and the link coming back draw nothing.
    private func assertNothingDrawn(_ resource: String) async throws {
        let all = events(try frames(resource))
        let end = try XCTUnwrap(all.first { $0.kind == .sessionEnd }, "the fixture carries no end")
        XCTAssertNotNil(end.payload["switched_to"], "the end carries no typed switch")
        XCTAssertTrue(all.contains { $0.seq > end.seq && $0.kind == .userMessage }, "the fixture carries the next message")
        let drawn = await state(all)
        XCTAssertEqual(
            drawn.timeline.filter { $0.seq >= end.seq }.map(\.content).filter {
                if case .userMessage = $0 { false } else { true }
            },
            [],
            "\(resource): the switch draws nothing but the message")
        XCTAssertFalse(
            notices(drawn).contains { $0.kind == .sessionEnded || $0.kind == .sessionStart },
            "\(resource): no Session ended, no Session resumed or Session started")
    }

    func testAClaudeSwitchDrawsNothing() async throws {
        try await assertNothingDrawn("switch-claude-wire")
    }

    func testACodexSwitchAndItsResumeDrawNothing() async throws {
        try await assertNothingDrawn("switch-codex-wire")
    }

    /// **A page that begins after a switch's end draws no start**: opened cold where the first event loaded follows the
    /// switch's end, the conversation draws no "Session resumed" either, so loading the page before it changes nothing drawn.
    func testAPageBeginningAfterTheSwitchsEndDrawsNoStart() async throws {
        for resource in ["switch-codex-wire", "relaunch-plan-default-codex-wire", "switch-failed-then-recovered-claude-wire"] {
            let all = events(try frames(resource))
            let end = try XCTUnwrap(all.first { $0.kind == .sessionEnd }, "\(resource) carries no end")
            let page = await state(all.filter { $0.seq > end.seq })
            XCTAssertFalse(
                notices(page).contains { $0.kind == .sessionStart },
                "\(resource): a page after the switch's end draws no start")
        }
    }

    /// **A background task's start and end are no activity and draw nothing**: the quiet `background_task` facts a
    /// Claude run files (which decide whether a switch of account may move it) leave the conversation without a row and
    /// its last activity at the person's message before them.
    func testABackgroundTaskDrawsNothingAndIsNoActivity() async throws {
        func event(_ json: String) throws -> Event { try JSONDecoder().decode(Event.self, from: Data(json.utf8)) }
        let uid = "01M8BGTASK0000000000000001"
        let said = try event(
            #"{"kind": "user_message", "payload": {"text": "start the server"}, "seq": 1, "session_id": "\#(uid)", "session_uid": "\#(uid)", "source": "daemon", "ts": "2026-11-25T14:00:00.000Z"}"#)
        let started = try event(
            #"{"kind": "background_task", "payload": {"task_id": "b1", "ended": false}, "seq": 2, "session_id": "\#(uid)", "session_uid": "\#(uid)", "source": "sdk", "ts": "2026-11-25T14:00:05.000Z"}"#)
        let ended = try event(
            #"{"kind": "background_task", "payload": {"task_id": "b1", "ended": true}, "seq": 3, "session_id": "\#(uid)", "session_uid": "\#(uid)", "source": "sdk", "ts": "2026-11-25T14:00:09.000Z"}"#)
        let alone = await state([said])
        let drawn = await state([said, started, ended])
        XCTAssertEqual(drawn.timeline.map(\.content), alone.timeline.map(\.content), "no row for a background task")
        XCTAssertEqual(drawn.shown.lastActivityAt, said.date, "a background task is no activity")
    }

    /// Every listing in the window reads live, with what the run had drawn by then: never the Ended band, and the reply
    /// box kept.
    func testEveryListingInTheSwitchReadsLive() async throws {
        for resource in ["switch-claude-wire", "switch-codex-wire"] {
            let read = try await listings(resource)
            XCTAssertFalse(read.isEmpty, "\(resource) carries no listing")
            for (summary, status) in read {
                XCTAssertFalse(status == .ended || status == .failed, "\(resource): \(status)")
                XCTAssertFalse(summary.takesNoReply, "\(resource): no reply box")
            }
        }
    }

    /// The status and reply box each listing gives, with what the run had drawn by then, and the settings it draws.
    private func listings(_ resource: String) async throws -> [(summary: SessionSummary, status: FleetStatus)] {
        var seen: [Event] = []
        var out: [(SessionSummary, FleetStatus)] = []
        for frame in try frames(resource) {
            switch frame {
            case .event(let event):
                seen.append(event)
            case .sessions(let sessions):
                let summary = try XCTUnwrap(sessions.first)
                // What had been drawn by then, which may be nothing yet (a first link row draws none).
                let state = SessionState(sessionKey: summary.sessionKey)
                for event in seen { state.ingest(event) }
                await state.settleForTesting()
                out.append((summary, FleetStatusRule.status(summary: summary, shown: state.shown, reviewedSeq: 0)))
            }
        }
        return out
    }

    /// **A start on the new account that was refused** draws one warning row by the account ccd types, its sentence
    /// beneath, never a title made from the notice's kind; the run reads ended with the reply box kept (its next
    /// message relaunches it), and once a relaunch goes live it reads live with no failed mark.
    func testAFailedSwitchDrawsOneWarningRowAndRecoversLive() async throws {
        let all = events(try frames("switch-failed-then-recovered-claude-wire"))
        let notice = try XCTUnwrap(all.first { $0.kind == .notification && $0.notificationType == "switch_failed" })
        let drawn = await state(all)
        XCTAssertEqual(
            drawn.timeline.filter { $0.seq == notice.seq }.map(\.content),
            [.notice(NoticeItem(
                kind: .other, symbol: "warning", title: "Couldn’t move to B",
                detail: "The session could not be started on the new account. Try again, or choose the account it ran on.",
                severity: .warning))])
        let read = try await listings("switch-failed-then-recovered-claude-wire")
        let failed = try XCTUnwrap(read.first { $0.summary.lifecycle == .exited })
        XCTAssertEqual(failed.status, .ended, "a refused start reads ended")
        XCTAssertFalse(failed.summary.takesNoReply, "its next message relaunches it")
        let last = try XCTUnwrap(read.last)
        XCTAssertEqual(last.summary.lifecycle, .live)
        XCTAssertFalse(last.status == .ended || last.status == .failed, "recovered, it reads live: \(last.status)")
    }

    /// **End run on a run due to move** files an ordinary end: "Session ended"; the ended listing reads ended with no reply
    /// box.
    func testEndRunOnADueRunReadsRunEnded() async throws {
        let drawn = await state(events(try frames("switch-then-end-claude-wire")))
        XCTAssertEqual(notices(drawn).map(\.title), ["Session ended"])
        let read = try await listings("switch-then-end-claude-wire")
        XCTAssertEqual(read.map(\.summary.lifecycle), [.exited])
        XCTAssertEqual(read.last?.status, .ended)
        XCTAssertTrue(read.last?.summary.takesNoReply ?? false, "ended by End run, it takes no reply")
    }

    /// A moving run's listings, from the switch's end on, carry its menus throughout, so both apps draw its live
    /// settings, never the read-only line (`hostedReadOnly`).
    func testAMovingRunKeepsItsSettings() throws {
        for resource in ["switch-claude-wire", "switch-codex-wire", "relaunch-plan-default-codex-wire"] {
            let moving = try frames(resource).compactMap {
                if case .sessions(let sessions) = $0, let summary = sessions.first, summary.switching == .moved {
                    summary
                } else {
                    nil
                }
            }
            XCTAssertFalse(moving.isEmpty, "\(resource) lists the run moving")
            for summary in moving {
                XCTAssertNotNil(summary.menus, "\(resource): a moving run lists its menus")
                XCTAssertNil(summary.modelLabel, "\(resource): named by its menu, not a label")
                if case .hostedReadOnly = RunSettings.of(summary: summary) {
                    XCTFail("\(resource): a moving run draws the read-only settings line")
                }
            }
        }
    }

    /// **A move onto a plan without the run's model** (decision 316) draws one quiet row from the notice's typed fields,
    /// "Now on <to>: <from> isn't in <account>'s plan.", never a title made from its kind; the run reads live.
    func testAMoveOntoAPlanWithoutTheModelDrawsOneQuietRow() async throws {
        let all = events(try frames("relaunch-plan-default-codex-wire"))
        let notice = try XCTUnwrap(all.first { $0.kind == .notification && $0.notificationType == "model_unavailable" })
        let drawn = await state(all)
        XCTAssertEqual(
            drawn.timeline.filter { $0.seq == notice.seq }.map(\.content),
            [.notice(NoticeItem(
                kind: .other, symbol: "arrows-clockwise", title: "Now on GPT-6 Luna: GPT-6.1-Sol isn’t in B’s plan.",
                detail: nil, severity: .info))])
        for (summary, status) in try await listings("relaunch-plan-default-codex-wire") {
            XCTAssertFalse(status == .ended || status == .failed, "\(status)")
            XCTAssertFalse(summary.takesNoReply)
        }
        // On the new plan's default from the relaunch on (the listing's switching mark at the notice is not pinned: the
        // daemon emitted it in a window of a few milliseconds).
        let last = try await listings("relaunch-plan-default-codex-wire").last
        XCTAssertEqual(last?.summary.model?.value, "gpt-6-luna")
    }

    /// **A moving run is answered from its reply box** (decision 306; decision 311 v4): its message is what moves it,
    /// and while it moves it reads linked (`attached`, a Codex run's `subscribed`): its fleet row's badge is control, its
    /// message is not blocked, and no "Observe only". From the daemon's `switch-codex-wire.json`: its listing after the
    /// typed end (live, moving).
    func testAMovingRunIsAnsweredNotObserved() async throws {
        let capabilities = try JSONDecoder().decode(
            Capabilities.self, from: Data(#"{"can_approve_reliably": true, "codex_compose": true, "supported_agents": ["claude", "codex"]}"#.utf8))
        var moving: SessionSummary?
        for frame in try frames("switch-codex-wire") {
            if case .sessions(let sessions) = frame, let run = sessions.first, run.switching == .moved { moving = run }
        }
        let run = try XCTUnwrap(moving, "the recording lists the run as moving")
        XCTAssertEqual(run.link, .attached, "a moving run reads linked")
        XCTAssertEqual(run.codexLink, .subscribed, "a moving Codex run reads subscribed")
        XCTAssertEqual(FleetStatusRule.capability(summary: run, capabilities: capabilities), .control, "the fleet row's badge")

        let model = AppModel(
            cache: EventCache(root: FileManager.default.temporaryDirectory.appendingPathComponent("moving-\(UUID().uuidString)")),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(capabilities, minor: 52)
        model.connection.injectForTesting(.sessions([run]))
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        XCTAssertNil(model.sendBlockedReason(for: run.sessionKey, draft: "go on"), "a moving run's message can be sent")
        let note = model.standingNote(for: run.sessionKey, draft: "")
        XCTAssertFalse(note?.text.hasPrefix("Observe only") ?? false, "the standing note: \(note?.text ?? "none")")
    }
}
