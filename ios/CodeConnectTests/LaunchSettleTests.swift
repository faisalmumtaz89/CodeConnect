import XCTest

@testable import CodeConnect

/// The fleet is shown once it is caught up with the Mac, not while it fills in.
///
/// "Caught up" is the daemon's own number: every session in the first live
/// `sessions` frame has replayed to the `last_seq` that frame reported. Until
/// then the app shows its loader, so the cached list, the live list and the
/// replay of each session's backlog are never painted one after another.
@MainActor
final class LaunchSettleTests: XCTestCase {

    private let uidA = "01K1B3XQ8ZC0DE5FGH7JKMNPLA"
    private let uidB = "01K1B3XQ8ZC0DE5FGH7JKMNPLB"

    private func connectedModel() -> AppModel {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("launch-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        return model
    }

    private func frame(_ json: String) -> ServerMessage {
        // swiftlint:disable:next force_unwrapping
        Fixtures.decode(json)!
    }

    private func sessions(_ entries: [(uid: String, lastSeq: Int)]) -> ServerMessage {
        let rows = entries.map { entry in
            """
            {"session_uid":"\(entry.uid)","session_id":"cc-\(entry.uid.suffix(2))",\
            "tmux_session":"cc-\(entry.uid.suffix(2))","cwd":"/tmp/x","lifecycle":"live",\
            "link":"attached","last_seq":\(entry.lastSeq),\
            "created_at":"2026-10-14T10:00:00Z","updated_at":"2026-10-14T10:00:00Z",\
            "blocked_on":[]}
            """
        }
        return frame(#"{"type":"sessions","sessions":["# + rows.joined(separator: ",") + "]}")
    }

    private func event(_ uid: String, seq: Int) -> ServerMessage {
        frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(uid)","session_id":"cc-x",\
            "ts":"2026-10-14T10:00:0\(seq % 10)Z","kind":"user_message","source":"transcript",\
            "payload":{"message":{"content":"step \(seq)"}}}}
            """)
    }

    /// Subscribing reads the cache first, off the main actor; give it its turn.
    /// Enough for what has already happened to have happened — used before an
    /// assertion that something has *not* happened yet.
    private func settle(_ model: AppModel) async {
        for _ in 0..<50 { await Task.yield() }
    }

    /// Waits, boundedly, for the fleet to be shown.
    private func settled(_ model: AppModel) async -> Bool {
        for _ in 0..<200 where !model.launchSettled {
            try? await Task.sleep(for: .milliseconds(10))
        }
        return model.launchSettled
    }

    func testTheFleetWaitsUntilEverySessionHasCaughtUp() async {
        let model = connectedModel()
        XCTAssertFalse(model.launchSettled, "nothing has been heard yet")

        model.connection.injectForTesting(sessions([(uidA, 3), (uidB, 2)]))
        await settle(model)
        XCTAssertFalse(model.launchSettled, "the list arrived, the sessions have not caught up")

        for seq in 1...3 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        await settle(model)
        XCTAssertFalse(model.launchSettled, "B is still behind")

        model.connection.injectForTesting(event(uidB, seq: 1))
        await settle(model)
        XCTAssertFalse(model.launchSettled, "B is one short")

        model.connection.injectForTesting(event(uidB, seq: 2))
        let shown = await settled(model)
        XCTAssertTrue(shown, "every session reached the number the Mac reported")
    }

    /// Revealed with every timeline already drawn: the reveal is one step, not
    /// the list followed by its rows filling in a moment later.
    func testTheRevealCarriesFinishedTimelines() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        await settle(model)
        for seq in 1...2 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        let shown = await settled(model)
        XCTAssertTrue(shown)
        XCTAssertEqual(
            model.statesForTesting[uidA]?.timeline.count, 2, "the timeline is built by the time it shows")
    }

    func testAnEmptyFleetIsCaughtUpAtOnce() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([]))
        let shown = await settled(model)
        XCTAssertTrue(shown)
    }

    /// A cache ahead of the Mac means the Mac's log was reset: that history is
    /// thrown away and replayed, and having had it is not being caught up.
    func testARewoundLogIsNotTakenForCaughtUp() async {
        let model = connectedModel()
        for seq in 1...5 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        // The replay only starts once the subscription has thrown the old
        // history away, which happens after the cache read.
        for _ in 0..<200 where model.statesForTesting[uidA]?.lastSeq != 0 {
            try? await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.statesForTesting[uidA]?.lastSeq, 0, "the rewound history was dropped")
        XCTAssertFalse(model.launchSettled, "the old history does not count")

        for seq in 1...2 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        let shown = await settled(model)
        XCTAssertTrue(shown)
    }

    /// Once shown, the fleet stays shown: a later list asking for more catch-up
    /// never brings the loader back.
    func testOnceShownTheFleetStaysShown() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([]))
        let shown = await settled(model)
        XCTAssertTrue(shown)
        model.connection.injectForTesting(sessions([(uidA, 9)]))
        await settle(model)
        XCTAssertTrue(model.launchSettled)
    }

    // MARK: A reconnect during the launch

    /// A reconnect starts the catch-up over: what the new list says is what
    /// "caught up" means, and history kept from before it counts for nothing
    /// until its subscription has decided what to keep.
    func testAReconnectStartsTheCatchUpOver() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([(uidA, 3), (uidB, 3)]))
        await settle(model)
        for seq in 1...3 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        await settle(model)
        XCTAssertFalse(model.launchSettled, "B is behind")

        model.connection.onConnected?()
        // The Mac's log for A was reset, and B has gone.
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        for _ in 0..<200 where model.statesForTesting[uidA]?.lastSeq != 0 {
            try? await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(model.launchSettled, "A's old history is not its new one")

        for seq in 1...2 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        let shown = await settled(model)
        XCTAssertTrue(shown, "caught up with the list the reconnect brought")
    }

    // MARK: How long the loader waits

    /// A Mac that is not answering gets the short wait, then the saved fleet.
    func testAMacThatIsNotAnsweringGetsTheShortWait() async {
        let model = AppModel(cache: EventCache(root: FileManager.default.temporaryDirectory
            .appendingPathComponent("launch-\(UUID().uuidString)")))
        let started = ContinuousClock.now
        await model.waitOutLaunch(offline: .milliseconds(100), cap: .seconds(5))
        XCTAssertTrue(model.launchSettled)
        XCTAssertLessThan(ContinuousClock.now - started, .seconds(2), "not the long cap")
    }

    /// A Mac that is answering and still catching up keeps the loader past the
    /// short wait: showing the fleet then would show the replay it hides.
    func testAMacThatIsCatchingUpKeepsTheLoader() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        await settle(model)
        let waiting = Task { await model.waitOutLaunch(offline: .milliseconds(50), cap: .seconds(5)) }
        try? await Task.sleep(for: .milliseconds(400))
        XCTAssertFalse(model.launchSettled, "past the short wait, still catching up")

        for seq in 1...2 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        let shown = await settled(model)
        XCTAssertTrue(shown)
        await waiting.value
    }

    /// A Mac that answered and then dropped is mid catch-up, not away: a blip
    /// past the short wait keeps the loader, so the redial's replay stays hidden.
    func testABlipDuringTheCatchUpKeepsTheLoader() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        await settle(model)
        let waiting = Task { await model.waitOutLaunch(offline: .milliseconds(50), cap: .seconds(5)) }
        try? await Task.sleep(for: .milliseconds(200))
        model.connection.setPhaseForTesting(.connecting)
        try? await Task.sleep(for: .milliseconds(400))
        XCTAssertFalse(model.launchSettled, "a Mac that answered is not taken for away")

        model.connection.simulateConnectedForTesting()
        for seq in 1...2 { model.connection.injectForTesting(event(uidA, seq: seq)) }
        let shown = await settled(model)
        XCTAssertTrue(shown)
        await waiting.value
    }

    /// An answer is the handshake having happened, however briefly the link
    /// then lasted: one that comes and goes between two looks still counts.
    func testAHandshakeThatDropsAtOnceStillCountsAsAnswered() async {
        let model = AppModel(cache: EventCache(root: FileManager.default.temporaryDirectory
            .appendingPathComponent("launch-\(UUID().uuidString)")))
        let waiting = Task { await model.waitOutLaunch(offline: .milliseconds(300), cap: .seconds(5)) }
        await settle(model)
        model.connection.injectForTesting(frame(
            #"{"type":"hello_ack","protocol_version":1,"protocol_minor":6,"capabilities":{}}"#))
        model.connection.setPhaseForTesting(.connecting)
        try? await Task.sleep(for: .milliseconds(600))
        XCTAssertFalse(model.launchSettled, "the Mac answered; this is a blip, not an absence")

        model.connection.injectForTesting(sessions([]))
        let shown = await settled(model)
        XCTAssertTrue(shown)
        await waiting.value
    }

    /// A refusal is an answer the user has to see, so it ends the wait at
    /// once rather than hiding behind the loader until the cap.
    func testARefusalEndsTheWait() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        await settle(model)
        let started = ContinuousClock.now
        let waiting = Task { await model.waitOutLaunch(offline: .milliseconds(50), cap: .seconds(5)) }
        try? await Task.sleep(for: .milliseconds(200))
        model.connection.setPhaseForTesting(.failed(reason: "Token rejected"))
        let shown = await settled(model)
        XCTAssertTrue(shown)
        XCTAssertLessThan(ContinuousClock.now - started, .seconds(2), "not held until the cap")
        await waiting.value
    }

    /// The loader never outlasts its cap, caught up or not.
    func testTheCapEndsTheWait() async {
        let model = connectedModel()
        model.connection.injectForTesting(sessions([(uidA, 2)]))
        await settle(model)
        await model.waitOutLaunch(offline: .milliseconds(50), cap: .milliseconds(300))
        XCTAssertTrue(model.launchSettled)
    }
}
