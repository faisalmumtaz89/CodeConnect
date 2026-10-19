import XCTest

@testable import CodeConnect

/// A notification tapped mid-catch-up opens the Deck waiting for it — the ring
/// and "Catching up…" — and the cards come in one go at the release. A Deck
/// opened from the bar shows the cards on screen.
@MainActor
final class DeckCatchUpTests: XCTestCase {

    private let a = "01K1B3XQ8ZC0DE5FGH7JKMNPA1"

    private final class Sent {
        var sent: [ClientMessage] = []
    }

    private func frame(_ json: String) -> ServerMessage {
        // swiftlint:disable:next force_unwrapping
        Fixtures.decode(json)!
    }

    private func sessions(_ lastSeq: Int) -> ServerMessage {
        frame(
            """
            {"type":"sessions","sessions":[{"session_uid":"\(a)","session_id":"cc-1",\
            "tmux_session":"cc-1","cwd":"/tmp/a","lifecycle":"live","link":"attached",\
            "last_seq":\(lastSeq),"created_at":"2026-10-16T10:00:00Z",\
            "updated_at":"2026-10-16T10:00:00Z","blocked_on":[]}]}
            """)
    }

    private func event(_ seq: Int) -> ServerMessage {
        frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(a)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:00Z","kind":"user_message","source":"transcript",\
            "payload":{"message":{"content":"step \(seq)"}}}}
            """)
    }

    private func card(_ seq: Int) -> ServerMessage {
        frame(
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(a)","session_id":"cc-1",\
            "ts":"2026-10-16T10:00:00Z","kind":"approval_request","source":"hook",\
            "payload":{"card":{"request_id":"toolu_\(seq)","payload_hash":"h","tool_name":"Bash",\
            "tool_input":{"command":"ls"},"display_text":"Bash"}}}}
            """)
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    private func caughtUp() async -> AppModel {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("deck-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(extra: ["history_pages": .bool(true), "session_uid": .bool(true)]),
            minor: 23)
        let sent = Sent()
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { sent.sent.append(message) }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.refreshFleet()
        model.connection.injectForTesting(sessions(3))
        for _ in 0..<300 where !sent.sent.contains(where: {
            if case .subscribe = $0 { return true } else { return false }
        }) {
            try? await Task.sleep(for: .milliseconds(10))
        }
        for seq in 1...3 { model.connection.injectForTesting(event(seq)) }
        await settle(model)
        return model
    }

    /// Tapped during the catch-up: the Deck waits, then has the card the
    /// catch-up brought.
    func testANotificationTapMidCatchUpWaitsForTheRelease() async {
        let model = await caughtUp()
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.openFromNotification(kind: "approval")
        XCTAssertTrue(model.deckAwaitsCatchUp, "the ring, not the held cards")
        XCTAssertEqual(model.pendingDeepLink, .deck(requestID: nil))

        model.connection.injectForTesting(sessions(4))
        model.connection.injectForTesting(card(4))
        await settle(model)
        XCTAssertFalse(model.deckAwaitsCatchUp)
        XCTAssertEqual(model.deck.map(\.card.requestID), ["toolu_4"], "the cards, in one go")
    }

    /// The tap is heard before the return that opens the catch-up: it counts
    /// for that catch-up.
    func testATapHeardBeforeTheReturnWaitsToo() async {
        let model = await caughtUp()
        model.scenePhaseChanged(to: .background)
        model.connection.setPhaseForTesting(.connecting)
        model.openFromNotification(kind: "approval")
        model.scenePhaseChanged(to: .active)
        XCTAssertTrue(model.deckAwaitsCatchUp, "the return's catch-up, on a link coming back")
    }

    /// Closed while it waits, the Deck stops waiting: opened again from the
    /// bar, it shows the cards on screen.
    func testClosingTheDeckEndsItsWait() async {
        let model = await caughtUp()
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.openFromNotification(kind: "approval")
        model.deckClosed()
        XCTAssertFalse(model.deckAwaitsCatchUp)
    }

    /// Each release is counted, so a Deck already open takes it as its
    /// reopening rather than as cards arriving mid-pass.
    func testEachReleaseIsCounted() async {
        let model = await caughtUp()
        let releases = model.catchUpReleases
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.connection.injectForTesting(sessions(4))
        model.connection.injectForTesting(card(4))
        await settle(model)
        XCTAssertEqual(model.catchUpReleases, releases + 1)
    }

    /// Opened from the bar, the Deck shows the cards on screen; and a tap with
    /// nothing catching up has nothing to wait for.
    func testOnlyANotificationTapMidCatchUpWaits() async {
        let model = await caughtUp()
        model.openFromNotification(kind: "approval")
        XCTAssertFalse(model.deckAwaitsCatchUp, "nothing is catching up")
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        XCTAssertFalse(model.deckAwaitsCatchUp, "a tap in the foreground is not one made while away")
        model.openFromNotification(kind: "finished")
        XCTAssertFalse(model.deckAwaitsCatchUp, "only an approval opens the Deck")
    }
}
