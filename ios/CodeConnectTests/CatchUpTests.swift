import XCTest

@testable import CodeConnect

/// **A catch-up changes the screen once.** While the phone replays what the Mac
/// did while it was away — a return, a redial, the launch's first link — what
/// is on screen stays as it was, and the caught-up state replaces it in one
/// publication per session and one of the list.
@MainActor
final class CatchUpTests: XCTestCase {

    private let a = "01K1B3XQ8ZC0DE5FGH7JKMNPA1"
    private let b = "01K1B3XQ8ZC0DE5FGH7JKMNPB2"
    private let c = "01K1B3XQ8ZC0DE5FGH7JKMNPC3"

    // MARK: Harness

    private final class Sent {
        var sent: [ClientMessage] = []

        var sessionsAsked: Int {
            sent.filter { if case .sessions = $0 { return true } else { return false } }.count
        }

        func subscribes(_ key: String) -> [UInt64] {
            sent.compactMap {
                guard case .subscribe(let session, let after) = $0, session == key else { return nil }
                return after
            }
        }

        func histories(_ key: String) -> [(before: UInt64, limit: UInt32, id: String)] {
            sent.compactMap {
                guard case .history(let session, let before, let limit, let id) = $0, session == key
                else { return nil }
                return (before, limit, id)
            }
        }

        var interrupts: Int {
            sent.filter { if case .interrupt = $0 { return true } else { return false } }.count
        }
    }

    private static let capabilities = Capabilities(
        canApproveReliably: true, sendText: true,
        extra: [
            "history_pages": .bool(true), "session_uid": .bool(true),
            "hook_only_approvals": .bool(true), "codex_interrupt": .bool(true),
            "delete_session": .bool(true),
        ])

    private func model() -> (AppModel, Sent) {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("catchup-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(Self.capabilities, minor: 23)
        let wire = Sent()
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { wire.sent.append(message) }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return (model, wire)
    }

    private func frame(_ json: String) -> ServerMessage {
        // swiftlint:disable:next force_unwrapping
        Fixtures.decode(json)!
    }

    private struct Row {
        var uid: String
        var lastSeq: Int
        var name = "cc-1"
        var agent = "claude"
        var lifecycle = "live"
        var pending: [(id: String, seq: Int)] = []
        var codexLink = "none"
        var created = "2026-10-16T10:00:00Z"
    }

    private func list(_ rows: [Row]) -> ServerMessage {
        let body = rows.map { row in
            let pending = row.pending.map { #"{"request_id":"\#($0.id)","seq":\#($0.seq)}"# }
            return """
                {"session_uid":"\(row.uid)","session_id":"\(row.name)","tmux_session":"\(row.name)",\
                "cwd":"/tmp/\(row.uid.suffix(2))","lifecycle":"\(row.lifecycle)","link":"attached",\
                "last_seq":\(row.lastSeq),"created_at":"\(row.created)",\
                "updated_at":"2026-10-16T10:00:00Z","blocked_on":[\(row.pending.map { "\"\($0.id)\"" }.joined(separator: ","))],\
                "agent":"\(row.agent)","codex_link":"\(row.codexLink)","pending":[\(pending.joined(separator: ","))]}
                """
        }
        return frame(#"{"type":"sessions","sessions":["# + body.joined(separator: ",") + "]}")
    }

    private func event(_ uid: String, _ seq: Int, substantive: Bool = true) -> ServerMessage {
        let kind = substantive ? "user_message" : "session_start"
        return frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(uid)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:00Z","kind":"\(kind)","source":"transcript",\
            "payload":{"message":{"content":"step \(seq)"}}}}
            """)
    }

    private func card(_ uid: String, _ seq: Int, id: String) -> ServerMessage {
        frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(uid)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:00Z","kind":"approval_request","source":"hook",\
            "payload":{"card":{"request_id":"\(id)","payload_hash":"h","tool_name":"Bash",\
            "tool_input":{"command":"ls"},"display_text":"Bash"}}}}
            """)
    }

    private func turnEvent(_ uid: String, _ seq: Int, turn: String, ends: Bool) -> ServerMessage {
        frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(uid)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:00Z","kind":"\(ends ? "turn_complete" : "tool_call")",\
            "source":"codex","turn_id":"\(turn)","payload":{}}}
            """)
    }

    private func historyEnd(_ uid: String, _ id: String, before: Int, first: Int, error: String? = nil)
        -> ServerMessage
    {
        .historyEnd(
            sessionID: uid, requestID: id, beforeSeq: UInt64(before), firstSeq: UInt64(first),
            error: error)
    }

    private func helloAck() -> ServerMessage {
        .helloAck(
            HelloAck(
                protocolVersion: Wire.protocolVersion, protocolMinor: 23, serverTime: "",
                capabilities: Self.capabilities, deviceToken: nil, deviceID: nil, deviceName: nil))
    }

    private func inject(_ model: AppModel, _ message: ServerMessage) {
        model.connection.injectForTesting(message)
    }

    private func inject(_ model: AppModel, _ uid: String, _ seqs: ClosedRange<Int>, substantive: Bool = true) {
        for seq in seqs { inject(model, event(uid, seq, substantive: substantive)) }
    }

    /// Lets every coalesced rebuild, cache read and send it causes run.
    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// Waits, boundedly, for the `n`th subscription to `uid`: it goes out after
    /// a cache read off the main actor.
    private func subscribed(_ wire: Sent, _ uid: String, _ n: Int = 1) async {
        for _ in 0..<300 where wire.subscribes(uid).count < n {
            try? await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertGreaterThanOrEqual(wire.subscribes(uid).count, n, "subscriptions to \(uid)")
    }

    /// Every list the tests inject answers a request that went out for it, as
    /// the Mac's do.
    private func answer(_ model: AppModel, _ rows: [Row]) {
        inject(model, list(rows))
    }

    /// A fleet caught up with the Mac, outside any catch-up.
    private func caughtUp(_ rows: [Row]) async -> (AppModel, Sent) {
        let (model, wire) = model()
        model.refreshFleet()
        answer(model, rows)
        for row in rows { await subscribed(wire, row.uid) }
        for row in rows where row.lastSeq > 0 {
            let start = max(1, row.lastSeq - 499)
            inject(model, row.uid, start...row.lastSeq)
        }
        await settle(model)
        return (model, wire)
    }

    private func returnToForeground(_ model: AppModel) {
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
    }

    /// Everything the fleet, the Deck and the open chats draw.
    private struct Screen: Equatable {
        let fleet: [FleetRow]
        let deck: [ApprovalItem]
        let sessions: [String: SessionState.Shown]
    }

    private func screen(_ model: AppModel, _ keys: [String]) -> Screen {
        Screen(
            fleet: model.fleet, deck: model.deck,
            sessions: Dictionary(uniqueKeysWithValues: keys.compactMap { key in
                model.shown(for: key).map { (key, $0) }
            }))
    }

    private func publications(_ model: AppModel, _ key: String) -> [SessionState.Publication] {
        model.statesForTesting[key]?.publications ?? []
    }

    // MARK: G1/G2. One publication, at the release

    /// A 700-event backlog on a link that stayed up: nothing on screen moves
    /// until the window has caught up, then each session and the list publish
    /// once.
    func testABacklogOnAStayingLinkIsPublishedOnce() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 10), Row(uid: b, lastSeq: 5)])
        let before = screen(model, [a, b])
        let aPublished = publications(model, a).count
        let bPublished = publications(model, b).count
        let listed = model.listPublications.count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 710), Row(uid: b, lastSeq: 5)])
        inject(model, a, 11...709)
        await settle(model)
        XCTAssertEqual(screen(model, [a, b]), before, "nothing on screen moves while catching up")
        XCTAssertEqual(publications(model, a).count, aPublished)
        XCTAssertEqual(model.listPublications.count, listed)

        inject(model, event(a, 710))
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(aPublished)), [.release])
        XCTAssertEqual(Array(publications(model, b).dropFirst(bPublished)), [], "b did not change")
        XCTAssertEqual(Array(model.listPublications.dropFirst(listed)), [.release])
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 710)
        XCTAssertEqual(model.shown(for: a)?.windowStart, 211, "the newest 500, in the same change")
    }

    /// The link drops and redials part-way through: the redial's own list
    /// takes over, and the screen still changes once.
    func testADroppedLinkAndItsRedialStillPublishOnce() async {
        let (model, wire) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let before = screen(model, [a])
        let published = publications(model, a).count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 40)])
        inject(model, a, 11...20)
        model.connection.setPhaseForTesting(.connecting)
        inject(model, helloAck())
        answer(model, [Row(uid: a, lastSeq: 40)])
        await subscribed(wire, a, 2)
        await settle(model)
        XCTAssertEqual(screen(model, [a]), before)

        inject(model, a, 21...40)
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 40)
    }

    /// A list asked for before the return answers first. It is not the return's
    /// list: it neither resets the window nor releases the catch-up.
    func testAnOlderListInFlightIsNotTheReturns() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 600)])
        let before = screen(model, [a])
        let published = publications(model, a).count
        model.refreshFleet()

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 650)])
        inject(model, a, 601...650)
        await settle(model)
        XCTAssertEqual(model.statesForTesting[a]?.windowStart, 101, "the older list spends no reset")
        XCTAssertEqual(screen(model, [a]), before, "and releases nothing")

        answer(model, [Row(uid: a, lastSeq: 1_200)])
        await settle(model)
        XCTAssertEqual(model.statesForTesting[a]?.windowStart, 701, "the return's list resets it")
        inject(model, a, 651...1_200)
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(model.shown(for: a)?.windowStart, 701)
    }

    /// `sessions_unavailable` answers a request as a list does, so the list
    /// after it is matched to the right one.
    func testAnUnavailableListTakesItsTurn() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let published = publications(model, a).count
        model.refreshFleet()

        returnToForeground(model)
        inject(model, .error(code: "sessions_unavailable", message: "the store is busy"))
        answer(model, [Row(uid: a, lastSeq: 30)])
        inject(model, a, 11...30)
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
    }

    /// A later list drops a session and adds one: the dropped one is no longer
    /// waited for, the added one is, and both changes land in the release.
    func testALaterListAddsAndDropsSessions() async {
        let (model, wire) = await caughtUp([Row(uid: a, lastSeq: 10), Row(uid: b, lastSeq: 10)])
        let before = screen(model, [a, b])
        let listed = model.listPublications.count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 20), Row(uid: b, lastSeq: 30)])
        inject(model, a, 11...20)
        model.refreshFleet()
        answer(model, [Row(uid: a, lastSeq: 20), Row(uid: c, lastSeq: 5, name: "cc-3")])
        await subscribed(wire, c)
        await settle(model)
        XCTAssertEqual(screen(model, [a, b]), before, "b's row is still drawn and c's is not yet")

        inject(model, c, 1...5)
        await settle(model)
        XCTAssertEqual(Array(model.listPublications.dropFirst(listed)), [.release])
        XCTAssertEqual(Set(model.fleet.map(\.summary.sessionKey)), [a, c])
    }

    /// A window with nothing to show pages back while the screen holds still,
    /// and the release waits for the page.
    func testTheReleaseWaitsForTheFillPage() async {
        let (model, wire) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let before = screen(model, [a])
        let published = publications(model, a).count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 1_000)])
        inject(model, a, 501...1_000, substantive: false)
        await settle(model)
        let page = wire.histories(a).last
        XCTAssertEqual(page?.before, 501, "the fill runs on the truth")
        XCTAssertEqual(screen(model, [a]), before, "and the screen waits for it")

        inject(model, a, 1...500, substantive: true)
        inject(model, historyEnd(a, page?.id ?? "", before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(model.shown(for: a)?.windowStart, 1)
    }

    /// A fill page that fails ends the fill, and with it the wait.
    func testAFailedFillReleases() async {
        let (model, wire) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let published = publications(model, a).count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 1_000)])
        inject(model, a, 501...1_000, substantive: false)
        await settle(model)
        let page = wire.histories(a).last
        XCTAssertEqual(publications(model, a).count, published)

        inject(model, historyEnd(a, page?.id ?? "", before: 501, first: 501, error: "store failed"))
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
    }

    /// An open card older than the window is fetched while the screen holds
    /// still; an event landing meanwhile is in the same release, and so is the
    /// card.
    func testTheReleaseWaitsForAPinAndTakesAnEventThatArrivedMeanwhile() async {
        let (model, wire) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let before = screen(model, [a])
        let published = publications(model, a).count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 1_000, pending: [("toolu_p", 300)])])
        inject(model, a, 501...1_000)
        await settle(model)
        let pin = wire.histories(a).last
        XCTAssertEqual(pin?.limit, 1, "the card is asked for alone")
        inject(model, event(a, 1_001))
        await settle(model)
        XCTAssertEqual(screen(model, [a]), before)

        inject(model, card(a, 300, id: "toolu_p"))
        inject(model, historyEnd(a, pin?.id ?? "", before: 301, first: 300))
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 1_001)
        XCTAssertEqual(model.deck.map(\.card.requestID), ["toolu_p"])
    }

    // MARK: G5. Launch

    /// A Mac that answered and then went quiet: the loader gives up and shows
    /// the saved fleet; the catch-up that follows updates it once.
    func testALaunchRevealLeavesTheCatchUpOpen() async {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("catchup-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let cache = EventCache(root: root)
        let saved = try? JSONDecoder().decode(
            [SessionSummary].self,
            from: Data(
                """
                [{"session_uid":"\(a)","session_id":"cc-1","tmux_session":"cc-1","cwd":"/tmp/a",\
                "lifecycle":"live","link":"attached","last_seq":3,"created_at":"2026-10-16T10:00:00Z",\
                "updated_at":"2026-10-16T10:00:00Z","blocked_on":[]}]
                """.utf8))
        await cache.saveFleet(saved ?? [])
        let model = AppModel(cache: cache)
        let wire = Sent()
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { wire.sent.append(message) }
        }
        model.connection.simulateCapabilitiesForTesting(Self.capabilities, minor: 23)
        await model.loadCachedFleetForTesting()
        let cached = screen(model, [a])
        XCTAssertEqual(cached.fleet.count, 1, "the saved fleet is what the reveal would show")

        model.connection.simulateConnectedForTesting()
        inject(model, helloAck())
        await model.waitOutLaunch(offline: .milliseconds(50), cap: .milliseconds(200))
        XCTAssertTrue(model.launchSettled)
        XCTAssertEqual(screen(model, [a]), cached, "the saved fleet, as saved")

        let published = publications(model, a).count
        answer(model, [Row(uid: a, lastSeq: 9)])
        await subscribed(wire, a)
        inject(model, a, 1...8)
        await settle(model)
        XCTAssertEqual(screen(model, [a]), cached, "still the saved fleet while catching up")

        inject(model, event(a, 9))
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 9)
    }

    // MARK: G6. The truth decides

    /// A name in a link means the run the catch-up's own list names, so it
    /// waits for that list rather than resolving to the run that held the name
    /// before the return.
    func testANameLinkWaitsForTheCatchUpsList() async {
        let old = Row(uid: a, lastSeq: 5, created: "2026-10-16T09:00:00Z")
        let (model, wire) = await caughtUp([old])
        XCTAssertEqual(model.resolveSessionKey(reference: "cc-1"), a)

        returnToForeground(model)
        XCTAssertNil(model.resolveSessionKey(reference: "cc-1"), "waits for the return's list")

        answer(model, [Row(uid: b, lastSeq: 2)])
        await subscribed(wire, b)
        inject(model, b, 1...2)
        await settle(model)
        XCTAssertEqual(model.resolveSessionKey(reference: "cc-1"), b, "the run the Mac names now")
    }

    /// The turn ended on the Mac and the screen still shows it running: Stop is
    /// drawn, and pressing it sends nothing, because the truth says there is
    /// nothing to stop.
    func testStopIsDecidedOnTheTruth() async {
        let codex = Row(uid: a, lastSeq: 1, agent: "codex", codexLink: "subscribed")
        let (model, wire) = model()
        model.refreshFleet()
        answer(model, [codex])
        await subscribed(wire, a)
        inject(model, turnEvent(a, 1, turn: "turn-9", ends: false))
        await settle(model)
        XCTAssertNil(model.stopUnavailable(for: a), "Stop is offered")

        returnToForeground(model)
        var ended = codex
        ended.lastSeq = 2
        answer(model, [ended, Row(uid: b, lastSeq: 1, name: "cc-2")])
        inject(model, turnEvent(a, 2, turn: "turn-9", ends: true))
        await settle(model)
        XCTAssertNil(model.stopUnavailable(for: a), "the screen still shows the turn running")

        await model.stopCodexTurn(sessionKey: a)
        XCTAssertEqual(wire.interrupts, 0, "the turn has ended, so nothing is sent")
    }

    // MARK: G7. The reader's own acts

    /// Removing a run mid-catch-up takes its row off at once, leaves every
    /// other row as it was, and does not hold the release.
    func testARemovalTakesOnlyItsRowAndDoesNotHoldTheRelease() async {
        let ended = Row(uid: a, lastSeq: 5, lifecycle: "exited")
        let (model, _) = await caughtUp([ended, Row(uid: b, lastSeq: 5, name: "cc-2")])
        model.connection.deleteStub = { _ in .deleted(events: 5) }
        let before = screen(model, [b])
        let listed = model.listPublications.count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 50, lifecycle: "exited"), Row(uid: b, lastSeq: 6, name: "cc-2")])
        await settle(model)
        await model.removeSession(model.fleet.first { $0.summary.sessionKey == a }!.summary)
        XCTAssertEqual(model.fleet.map(\.summary.sessionKey), [b], "its row goes at once")
        XCTAssertEqual(model.shown(for: b), before.sessions[b], "and nothing else moves")

        inject(model, event(b, 6))
        await settle(model)
        XCTAssertEqual(Array(model.listPublications.dropFirst(listed)), [.removal, .release])
        XCTAssertEqual(model.shown(for: b)?.drawnThrough, 6, "a's missing events held nothing back")
    }

    /// Dismissing a gap notice mid-catch-up hides it at once and changes
    /// nothing else on screen.
    func testDismissingAGapIsImmediate() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 3)])
        inject(model, event(a, 6))
        await settle(model)
        XCTAssertNotNil(model.shown(for: a)?.gap)

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 20)])
        inject(model, a, 7...10)
        await settle(model)
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 6, "the screen is held")
        var expected = model.shown(for: a)
        model.dismissGap(a)
        expected?.gap = nil
        XCTAssertEqual(model.shown(for: a), expected, "the banner goes, and only the banner")
        XCTAssertEqual(publications(model, a).last, .gapDismissed)
    }

    // MARK: G9, G10

    /// Unpairing mid-catch-up shows nothing it was holding.
    func testUnpairingMidCatchUpShowsNoHeldData() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 3)])
        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 20)])
        inject(model, a, 4...10)
        await settle(model)
        model.unpair()
        XCTAssertTrue(model.fleet.isEmpty)
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertNil(model.shown(for: a))
    }

    /// Back in the background before the catch-up ends, and back again: the
    /// first return's list is old news, and the second return's releases.
    func testASecondReturnRetargetsTheCatchUp() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let before = screen(model, [a])
        let published = publications(model, a).count

        returnToForeground(model)
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        answer(model, [Row(uid: a, lastSeq: 10)])
        await settle(model)
        XCTAssertEqual(screen(model, [a]), before, "the first return's list releases nothing")

        answer(model, [Row(uid: a, lastSeq: 15)])
        inject(model, a, 11...15)
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
    }

    /// What arrives once the app has left — in the background, or while it is
    /// coming back and not yet active — waits for the return's catch-up.
    func testWhatArrivesBeforeTheAppIsActiveWaitsForTheReturn() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 10), Row(uid: b, lastSeq: 5)])
        let before = screen(model, [a, b])
        let published = publications(model, a).count
        let listed = model.listPublications.count

        model.scenePhaseChanged(to: .background)
        inject(model, a, 11...15)
        await settle(model)
        model.scenePhaseChanged(to: .inactive)
        inject(model, a, 16...20)
        await settle(model)
        XCTAssertEqual(screen(model, [a, b]), before, "nothing is drawn before the app is active")

        model.scenePhaseChanged(to: .active)
        answer(model, [Row(uid: a, lastSeq: 20), Row(uid: b, lastSeq: 5)])
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(Array(model.listPublications.dropFirst(listed)), [.release])
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 20)
    }

    /// The way back starts with `inactive`, while the app is still being
    /// brought to the front: the return is aimed then, so a short backlog is
    /// caught up by the time the app is active.
    func testAReturnIsAimedWhileTheAppIsStillComingBack() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let published = publications(model, a).count

        model.scenePhaseChanged(to: .background)
        inject(model, a, 11...20)
        model.scenePhaseChanged(to: .inactive)
        answer(model, [Row(uid: a, lastSeq: 20)])
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertFalse(model.isCatchingUp)

        model.scenePhaseChanged(to: .active)
        XCTAssertFalse(model.isCatchingUp, "already back")
    }

    /// Leaving while the launch is still reading the saved fleet holds
    /// nothing, since nothing is on screen yet: offline, the reveal shows that
    /// fleet.
    func testLeavingDuringTheLaunchHoldsNothing() async {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("catchup-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let cache = EventCache(root: root)
        let saved = try? JSONDecoder().decode(
            [SessionSummary].self,
            from: Data(
                """
                [{"session_uid":"\(a)","session_id":"cc-1","tmux_session":"cc-1","cwd":"/tmp/a",\
                "lifecycle":"live","link":"attached","last_seq":3,"created_at":"2026-10-16T10:00:00Z",\
                "updated_at":"2026-10-16T10:00:00Z","blocked_on":[]}]
                """.utf8))
        await cache.saveFleet(saved ?? [])
        let model = AppModel(cache: cache)
        model.scenePhaseChanged(to: .background)
        await model.loadCachedFleetForTesting()
        model.scenePhaseChanged(to: .active)
        await model.waitOutLaunch(offline: .milliseconds(50), cap: .milliseconds(200))
        XCTAssertTrue(model.launchSettled)
        XCTAssertEqual(model.fleet.map(\.summary.sessionKey), [a], "the saved fleet")
    }

    /// With no Mac, a return holds a screen nothing will catch up: the sample
    /// fleet, opened after it, shows at once.
    func testTheSampleFleetShowsAfterAReturnWithNoMac() async {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("catchup-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        returnToForeground(model)
        model.startSampleFleet()
        await settle(model)
        XCTAssertFalse(model.fleet.isEmpty)
    }

    /// Leaving again mid-catch-up makes its list old news: everything it was
    /// owed can arrive while away or on the way back, and nothing is released
    /// until the next return's catch-up, in one change.
    func testLeavingAgainMidCatchUpReleasesNothingUntilTheNextReturn() async {
        let (model, _) = await caughtUp([Row(uid: a, lastSeq: 10)])
        let before = screen(model, [a])
        let published = publications(model, a).count

        returnToForeground(model)
        answer(model, [Row(uid: a, lastSeq: 40)])
        inject(model, a, 11...20)
        await settle(model)
        model.scenePhaseChanged(to: .background)
        inject(model, a, 21...50)
        await settle(model)
        model.scenePhaseChanged(to: .inactive)
        await settle(model)
        XCTAssertEqual(screen(model, [a]), before, "the first return's list is never released")

        model.scenePhaseChanged(to: .active)
        answer(model, [Row(uid: a, lastSeq: 60)])
        inject(model, a, 51...60)
        await settle(model)
        XCTAssertEqual(Array(publications(model, a).dropFirst(published)), [.release])
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 60)
    }
}
