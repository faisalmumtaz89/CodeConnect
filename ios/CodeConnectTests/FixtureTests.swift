import XCTest

@testable import CodeConnect

/// The fixtures are JSON decoded by the app's real decoders, so they are a
/// protocol test as well as a test rig. If one of them stops matching the wire
/// it must fail here, loudly, rather than quietly producing an empty Deck.
@MainActor
final class FixtureTests: XCTestCase {

    func testEveryFixtureFrameDecodes() {
        let frames = Fixtures.frames()
        XCTAssertEqual(
            frames.count, 3 + Fixtures.deckCards.count,
            "hello_ack + sessions + one card each + turn_complete")

        guard case .helloAck(let ack) = frames[0] else { return XCTFail("no hello_ack") }
        XCTAssertEqual(ack.protocolMinor, 1)
        XCTAssertTrue(ack.capabilities.canApproveReliably)

        guard case .sessions(let sessions) = frames[1] else { return XCTFail("no sessions") }
        XCTAssertEqual(sessions.count, 4)
        XCTAssertEqual(sessions[0].link, .attached)
    }

    /// A fixture card whose hash did not verify would exercise the *blocked*
    /// path, not the Deck — the buttons would be disabled and the test would be
    /// measuring nothing.
    func testFixtureCardsVerifyAgainstTheirHashes() {
        for frame in Fixtures.frames() {
            guard case .event(let event) = frame, event.kind == .approvalRequest else { continue }
            let card = event.approvalCard
            XCTAssertNotNil(card, "an approval fixture must decode as a card")
            XCTAssertTrue(
                card?.verification.hashMatchesDisplayText == true,
                "\(card?.requestID ?? "?") must verify or the card cannot be answered")
        }
    }

    func testFixtureRiskClassesCoverAllThreeGates() {
        let profile = DaemonProfile(protocolVersion: 1, protocolMinor: 1, capabilities: nil)
        var classes: Set<RiskClass> = []
        for frame in Fixtures.frames() {
            guard case .event(let event) = frame, let card = event.approvalCard else { continue }
            let item = ApprovalItem(
                card: card, requestedAt: event.date, sessionKey: event.sessionKey,
                sessionName: event.sessionID, outcome: nil, paneSnapshot: nil, risk: card.risk)
            classes.insert(item.assessment(profile: profile).effective)
        }
        XCTAssertEqual(classes, [.low, .medium, .high])
    }

    func testFixtureDiffParses() throws {
        let diff = try XCTUnwrap(Fixtures.diff())
        let parsed = UnifiedDiff.parse(diff.unified)
        XCTAssertEqual(parsed.files.count, 2)
        XCTAssertEqual(parsed.files[0].shortName, "Feature.swift")
        XCTAssertTrue(parsed.trailing.contains { $0.contains("Untracked.swift") })
    }

    /// The fixture ingests through the real model, so the Deck it produces is
    /// the Deck the app would show.
    func testFixturesProduceAThreeCardDeck() async {
        let model = AppModel(
            pairing: PairingStore(), cache: EventCache(root: URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent(UUID().uuidString)),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
        model.connection.simulateConnectedForTesting()
        for frame in Fixtures.frames() { model.connection.injectForTesting(frame) }
        // The timeline rebuild is scheduled on the main actor, so let it run.
        try? await Task.sleep(for: .milliseconds(120))
        XCTAssertEqual(model.summaries.count, 4)
        XCTAssertEqual(model.deckCount, 3, "three agents blocked")
        // Urgency-ranked, and the fixture is built to make the difference
        // visible: `fx-1` is the **newest** card (60s) and the HIGH-risk
        // `git push --force`, `fx-3` is the oldest (300s) and a LOW `Read`. Age
        // first would put the `Read` on top of the stack and advertise it on the
        // accessory bar while the force-push waited behind it.
        XCTAssertEqual(
            model.deck.map(\.sessionKey), ["fx-1", "fx-2", "fx-3"],
            "HIGH before MEDIUM before LOW")
        XCTAssertEqual(
            model.deck.map(\.sessionName), ["fx-1", "fx-2", "fx-3"],
            "a daemon that mints no uid is keyed by its tmux name, exactly as before")
    }
}

/// Deep links land on the model, survive a cold start, and are consumed exactly
/// once — so the fleet and the session detail cannot both act on the same link.
@MainActor
final class DeepLinkRoutingTests: XCTestCase {
    private func makeModel() -> AppModel {
        AppModel(
            pairing: PairingStore(),
            cache: EventCache(
                root: URL(fileURLWithPath: NSTemporaryDirectory())
                    .appendingPathComponent(UUID().uuidString)),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
    }

    func testKnownLinksAreAccepted() throws {
        let model = makeModel()
        XCTAssertTrue(model.open(url: try XCTUnwrap(URL(string: "codeconnect://deck"))))
        XCTAssertEqual(model.pendingDeepLink, .deck(requestID: nil))
    }

    func testForeignLinksAreRejectedAndChangeNothing() throws {
        let model = makeModel()
        model.pendingDeepLink = .deck(requestID: "keep-me")
        XCTAssertFalse(model.open(url: try XCTUnwrap(URL(string: "https://example.com"))))
        XCTAssertEqual(
            model.pendingDeepLink, .deck(requestID: "keep-me"),
            "an unrecognised URL must not clear a pending target")
    }

    func testALinkIsConsumedOnlyOnce() throws {
        let model = makeModel()
        _ = model.open(url: try XCTUnwrap(URL(string: "codeconnect://session/cc-1/diff")))
        XCTAssertEqual(model.consumeDeepLink(), .diff(sessionID: "cc-1"))
        XCTAssertNil(model.consumeDeepLink(), "two surfaces must not both act on one link")
        XCTAssertNil(model.pendingDeepLink)
    }
}
