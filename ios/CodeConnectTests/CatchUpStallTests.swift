import XCTest

@testable import CodeConnect

/// **No progress on a connected link means retry, never release.** A catch-up
/// that stops advancing asks for the list again if it has none, and otherwise
/// redials; the screen holds still throughout. Driven by the same check the
/// fleet refresh interval runs, called directly.
@MainActor
final class CatchUpStallTests: XCTestCase {

    private let a = "01K1B3XQ8ZC0DE5FGH7JKMNPA1"
    private let b = "01K1B3XQ8ZC0DE5FGH7JKMNPB2"
    private let c = "01K1B3XQ8ZC0DE5FGH7JKMNPC3"

    private final class Sent {
        var sent: [ClientMessage] = []

        var sessionsAsked: Int {
            sent.filter { if case .sessions = $0 { return true } else { return false } }.count
        }

        func subscribes(_ key: String) -> Int {
            sent.filter {
                if case .subscribe(let session, _) = $0 { return session == key }
                return false
            }.count
        }
    }

    private static let capabilities = Capabilities(
        canApproveReliably: true, sendText: true,
        extra: ["history_pages": .bool(true), "session_uid": .bool(true)])

    private func model() -> (AppModel, Sent) {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("stall-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(Self.capabilities, minor: 23)
        let sent = Sent()
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { sent.sent.append(message) }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return (model, sent)
    }

    private func frame(_ json: String) -> ServerMessage {
        // swiftlint:disable:next force_unwrapping
        Fixtures.decode(json)!
    }

    private func list(_ rows: [(uid: String, lastSeq: Int)]) -> ServerMessage {
        let body = rows.enumerated().map { index, row in
            """
            {"session_uid":"\(row.uid)","session_id":"cc-\(index)","tmux_session":"cc-\(index)",\
            "cwd":"/tmp/\(index)","lifecycle":"live","link":"attached","last_seq":\(row.lastSeq),\
            "created_at":"2026-10-16T10:00:00Z","updated_at":"2026-10-16T10:00:00Z","blocked_on":[]}
            """
        }
        return frame(#"{"type":"sessions","sessions":["# + body.joined(separator: ",") + "]}")
    }

    private func event(_ uid: String, _ seq: Int) -> ServerMessage {
        frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(uid)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:00Z","kind":"user_message","source":"transcript",\
            "payload":{"message":{"content":"step \(seq)"}}}}
            """)
    }

    private func inject(_ model: AppModel, _ uid: String, _ seqs: ClosedRange<Int>) {
        for seq in seqs { model.connection.injectForTesting(event(uid, seq)) }
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    private func subscribed(_ sent: Sent, _ uid: String) async {
        for _ in 0..<300 where sent.subscribes(uid) == 0 {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    private func caughtUp(_ rows: [(uid: String, lastSeq: Int)]) async -> (AppModel, Sent) {
        let (model, sent) = model()
        model.refreshFleet()
        model.connection.injectForTesting(list(rows))
        for row in rows { await subscribed(sent, row.uid) }
        for row in rows where row.lastSeq > 0 { inject(model, row.uid, 1...row.lastSeq) }
        await settle(model)
        return (model, sent)
    }

    private func returnToForeground(_ model: AppModel) {
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
    }

    /// The Mac goes silent on a live link: no list since the catch-up opened,
    /// or a list and none of what it owes since the last look, and the link is
    /// redialled. The screen never changes.
    func testASilentMacIsRedialledNeverReleased() async {
        let (model, _) = await caughtUp([(a, 10)])
        let before = model.shown(for: a)
        returnToForeground(model)

        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 1, "no list has come: redialled")

        model.connection.injectForTesting(list([(a, 20)]))
        inject(model, a, 11...15)
        await settle(model)
        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 1, "the list and the events were progress")

        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 2, "nothing since: the link is redialled")
        XCTAssertEqual(model.shown(for: a), before, "and nothing is released")
    }

    /// A handshake while the app is away catches nothing up until it is back,
    /// so it is no stall: a healthy link is never redialled for it.
    func testALinkThatComesBackWhileAwayIsNotRedialled() async {
        let (model, _) = await caughtUp([(a, 10)])
        model.scenePhaseChanged(to: .background)
        model.connection.injectForTesting(
            .helloAck(
                HelloAck(
                    protocolVersion: Wire.protocolVersion, protocolMinor: 23, serverTime: "",
                    capabilities: Self.capabilities, deviceToken: nil, deviceID: nil,
                    deviceName: nil)))
        model.connection.injectForTesting(list([(a, 10)]))
        await settle(model)
        model.checkCatchUpForTesting()
        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 0)
    }

    /// The Mac keeps answering the fleet refresh while the events owed never
    /// come: a list that repeats what is owed is not progress.
    func testListsThatRepeatWhatIsOwedAreNotProgress() async {
        let (model, _) = await caughtUp([(a, 10)])
        let before = model.shown(for: a)
        returnToForeground(model)
        model.connection.injectForTesting(list([(a, 20)]))
        await settle(model)
        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 0, "its first list was progress")

        model.refreshFleet()
        model.connection.injectForTesting(list([(a, 20)]))
        await settle(model)
        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 1, "the same list again is not")
        XCTAssertEqual(model.shown(for: a), before)
    }

    /// Events from a session that is already caught up, or from one the list
    /// does not name, advance nothing owed.
    func testABusySessionElsewhereIsNotProgress() async {
        let (model, _) = await caughtUp([(a, 10), (b, 10)])
        returnToForeground(model)
        model.connection.injectForTesting(list([(a, 20), (b, 10)]))
        await settle(model)
        model.checkCatchUpForTesting()

        inject(model, b, 11...40)
        inject(model, c, 1...40)
        await settle(model)
        model.checkCatchUpForTesting()
        XCTAssertEqual(model.connection.redialsForTesting, 1, "busy elsewhere, stalled here")
    }

    /// Twenty sessions replaying five hundred events each, checked between
    /// every batch: each check sees progress, nothing is redialled, and the
    /// screen changes once.
    func testALongReplayIsNeverInterrupted() async {
        let keys = (0..<20).map { String(format: "01K1B3XQ8ZC0DE5FGH7JKMN%03d", $0) }
        let (model, _) = await caughtUp(keys.map { ($0, 1) })
        let published = keys.map { model.statesForTesting[$0]?.publications.count ?? 0 }
        returnToForeground(model)
        model.connection.injectForTesting(list(keys.map { ($0, 500) }))
        await settle(model)
        for batch in stride(from: 2, through: 500, by: 50) {
            for key in keys { inject(model, key, batch...min(batch + 49, 500)) }
            await settle(model)
            model.checkCatchUpForTesting()
        }
        XCTAssertEqual(model.connection.redialsForTesting, 0)
        for (index, key) in keys.enumerated() {
            XCTAssertEqual(
                model.statesForTesting[key]?.publications.dropFirst(published[index]).map { $0 },
                [.release], key)
        }
    }

    /// A link that fails mid-catch-up keeps the old screen, asks nothing while
    /// it is down, and updates once when the redial has caught up.
    func testAFailedLinkHoldsUntilCaughtUpAfterTheRedial() async {
        let (model, sent) = await caughtUp([(a, 10)])
        let before = model.shown(for: a)
        let published = model.statesForTesting[a]?.publications.count ?? 0
        returnToForeground(model)
        model.connection.injectForTesting(list([(a, 30)]))
        inject(model, a, 11...20)
        model.connection.setPhaseForTesting(.connecting)
        let asked = sent.sessionsAsked
        model.checkCatchUpForTesting()
        model.checkCatchUpForTesting()
        XCTAssertEqual(sent.sessionsAsked, asked, "nothing is asked of a link that is down")
        XCTAssertEqual(model.connection.redialsForTesting, 0, "its own supervision redials")
        await settle(model)
        XCTAssertEqual(model.shown(for: a), before)

        model.connection.injectForTesting(
            .helloAck(
                HelloAck(
                    protocolVersion: Wire.protocolVersion, protocolMinor: 23, serverTime: "",
                    capabilities: Self.capabilities, deviceToken: nil, deviceID: nil,
                    deviceName: nil)))
        model.connection.injectForTesting(list([(a, 30)]))
        for _ in 0..<300 where sent.subscribes(a) < 2 {
            try? await Task.sleep(for: .milliseconds(10))
        }
        inject(model, a, 21...30)
        await settle(model)
        XCTAssertEqual(
            model.statesForTesting[a]?.publications.dropFirst(published).map { $0 }, [.release])
    }
}
