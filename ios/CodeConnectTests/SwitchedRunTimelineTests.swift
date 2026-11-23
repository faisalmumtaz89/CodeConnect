import XCTest

@testable import CodeConnectKit

/// **A run moved to another account never reads as ended** (decision 306).
///
/// ccd ends the run's process at its idle point and files a `session_end` whose payload carries `switched_to`
/// (`{"account_id", "label"}`, minor 48); the person's next message starts it again on the new account, which for a
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
                detail: "Couldn’t start on B: B is signed out", severity: .warning))])
        let read = try await listings("switch-failed-then-recovered-claude-wire")
        let failed = try XCTUnwrap(read.first { $0.summary.lifecycle == .exited })
        XCTAssertEqual(failed.status, .ended, "a refused start reads ended")
        XCTAssertFalse(failed.summary.takesNoReply, "its next message relaunches it")
        let last = try XCTUnwrap(read.last)
        XCTAssertEqual(last.summary.lifecycle, .live)
        XCTAssertFalse(last.status == .ended || last.status == .failed, "recovered, it reads live: \(last.status)")
    }

    /// **End run on a moved run** files an ordinary end after the switch's typed one: "Switched to B", then "Run
    /// ended"; the moved listing reads live, and the ended one reads ended with no reply box.
    func testEndRunOnAMovedRunReadsRunEnded() async throws {
        let drawn = await state(events(try frames("switch-then-end-claude-wire")))
        XCTAssertEqual(notices(drawn).map(\.title), ["Switched to B", "Run ended"])
        let read = try await listings("switch-then-end-claude-wire")
        XCTAssertEqual(read.map(\.summary.lifecycle), [.live, .exited])
        XCTAssertFalse(read.first.map { $0.status == .ended || $0.status == .failed } ?? true, "moved, it reads live")
        XCTAssertEqual(read.last?.status, .ended)
        XCTAssertTrue(read.last?.summary.takesNoReply ?? false, "ended by End run, it takes no reply")
    }

    /// A moved run's listing, the one right after the typed end, carries no menus until its next message starts it,
    /// so both apps draw the read-only settings line (`hostedReadOnly`), never a live-looking menu.
    func testAMovedRunsSettingsAreReadOnly() throws {
        for resource in ["switch-claude-wire", "switch-codex-wire"] {
            let all = try frames(resource)
            let end = try XCTUnwrap(all.firstIndex {
                if case .event(let event) = $0 { event.kind == .sessionEnd } else { false }
            })
            guard case .sessions(let sessions) = all[end + 1], let moved = sessions.first else {
                return XCTFail("\(resource): no listing follows the end")
            }
            XCTAssertNil(moved.menus, "\(resource): a moved run lists no menus")
            guard case .hostedReadOnly = RunSettings.of(summary: moved) else {
                return XCTFail("\(resource): a moved run draws a live settings line")
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
                kind: .other, symbol: "arrows-clockwise", title: "Now on gpt-6-luna: GPT-6.1-Sol isn’t in B’s plan.",
                detail: nil, severity: .info))])
        for (summary, status) in try await listings("relaunch-plan-default-codex-wire") {
            XCTAssertFalse(status == .ended || status == .failed, "\(status)")
            XCTAssertFalse(summary.takesNoReply)
        }
    }

    /// **A moved run is answered from its reply box** (decision 306): its next message is what starts it on the new
    /// account. A moved Codex run's link is detached from its idle end until then, and that is not a lost link: no
    /// "Observe only" on its reply box, and its fleet row's badge is control. A run still waiting keeps the link's own
    /// word. From the daemon's `switch-codex-wire.json`: its listing after the typed end (live, detached, moved).
    func testAMovedRunIsAnsweredNotObserved() async throws {
        let capabilities = try JSONDecoder().decode(
            Capabilities.self, from: Data(#"{"can_approve_reliably": true, "codex_compose": true, "supported_agents": ["claude", "codex"]}"#.utf8))
        var moved: SessionSummary?
        for frame in try frames("switch-codex-wire") {
            if case .sessions(let sessions) = frame, let run = sessions.first, run.switching == .moved { moved = run }
        }
        let run = try XCTUnwrap(moved, "the recording lists the run as moved")
        XCTAssertEqual(run.link, .detached, "precondition: a moved Codex run's link is detached")
        XCTAssertEqual(FleetStatusRule.capability(summary: run, capabilities: capabilities), .control, "the fleet row's badge")

        let model = AppModel(
            cache: EventCache(root: FileManager.default.temporaryDirectory.appendingPathComponent("moved-\(UUID().uuidString)")),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(capabilities, minor: 48)
        model.connection.injectForTesting(.sessions([run]))
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        // The send was never blocked: its message is what restarts the run.
        XCTAssertNil(model.sendBlockedReason(for: run.sessionKey, draft: "go on"), "a moved run's message can be sent")
        let note = model.standingNote(for: run.sessionKey, draft: "")
        XCTAssertFalse(note?.text.hasPrefix("Observe only") ?? false, "the standing note: \(note?.text ?? "none")")
    }
}
