import XCTest

@testable import CodeConnect

/// The fixtures are JSON decoded by the app's real decoders, so they are a
/// protocol test as well as a test rig. If one of them stops matching the wire
/// it must fail here, loudly, rather than quietly producing an empty Deck.
@MainActor
final class FixtureTests: XCTestCase {

    /// `protocol::PROTOCOL_MINOR` on the Mac. Pinned rather than derived: the
    /// fixture's `hello_ack` is a literal, so this is the one place that says
    /// out loud which daemon it is pretending to be, and raising the Mac's
    /// minor without raising this one is what the assertion below catches.
    private let currentProtocolMinor: UInt32 = 14

    func testEveryFixtureFrameDecodes() {
        let frames = Fixtures.frames()
        XCTAssertEqual(
            frames.count, 5 + Fixtures.deckCards.count,
            "hello_ack + sessions + one card each + turn_complete + the two confirmed facts")

        guard case .helloAck(let ack) = frames[0] else { return XCTFail("no hello_ack") }
        XCTAssertEqual(
            ack.protocolMinor, currentProtocolMinor,
            "the fixture daemon speaks this build's protocol, or capability-gated UI is unreachable")

        guard case .sessions(let sessions) = frames[1] else { return XCTFail("no sessions") }
        XCTAssertEqual(sessions.count, 4)
        XCTAssertEqual(sessions[0].link, .attached)
    }

    /// Every gate the app puts in front of a screen, asked of the fixture.
    ///
    /// A capability the fixture withholds is a screen the fixture cannot reach:
    /// the UI is correctly omitted, nothing fails, and the render harness
    /// photographs an app with a hole in it. So the whole set is asserted by
    /// name rather than a chosen two, and the two that are honestly false are
    /// asserted false for the same reason — a fixture daemon holds no APNs key
    /// and terminates no TLS.
    func testTheFixtureDaemonAdvertisesEveryCapabilityTheAppGatesOn() {
        guard case .helloAck(let ack) = Fixtures.frames()[0] else { return XCTFail("no hello_ack") }
        let capabilities = ack.capabilities

        XCTAssertTrue(capabilities.canApproveReliably)
        XCTAssertTrue(capabilities.sendText)
        XCTAssertTrue(capabilities.capture)
        XCTAssertTrue(capabilities.servesDiff)
        XCTAssertTrue(capabilities.sendTextIdempotent)
        XCTAssertTrue(capabilities.servesCommandCatalog)
        XCTAssertTrue(
            capabilities.recoversComposer,
            "without this the snapshot rows are correctly omitted and cannot be rendered")
        XCTAssertTrue(capabilities.classifiesRisk)
        XCTAssertTrue(
            capabilities.servesTerminal,
            "without this the Terminal tab draws its update-the-Mac state and nothing else")
        XCTAssertTrue(capabilities.deletesSessions)
        XCTAssertTrue(capabilities.scopesSessionsByUID)

        XCTAssertFalse(capabilities.push)
        XCTAssertFalse(capabilities.testsPush)
        XCTAssertFalse(capabilities.tls)
        XCTAssertFalse(capabilities.tlsActive)
    }

    /// The sample fleet's ack, held to the opposite rule: it advertises
    /// exactly what the sample serves. A capability advertised here whose
    /// request rides the connection is a control that dead-ends in front of
    /// a reviewer, because the sample has no connection.
    func testTheSampleAckWithholdsWhatTheSampleCannotServe() {
        guard case .helloAck(let ack) = Fixtures.sampleFrames()[0] else {
            return XCTFail("no hello_ack")
        }
        let capabilities = ack.capabilities

        XCTAssertFalse(
            capabilities.servesTerminal,
            "the terminal rides the connection; offered in the sample it asks the reviewer to pair")
        XCTAssertFalse(capabilities.capture)
        XCTAssertFalse(capabilities.deletesSessions)
        XCTAssertFalse(capabilities.servesCommandCatalog)

        XCTAssertTrue(
            capabilities.sendText,
            "typing is answered in sample vocabulary, not hidden")
        XCTAssertTrue(
            capabilities.servesDiff,
            "the sample preloads its own diff")
        XCTAssertTrue(capabilities.canApproveReliably)
        XCTAssertTrue(capabilities.classifiesRisk)
    }

    /// Everything but the ack is the same fleet the harness photographs —
    /// the sample must never drift into a second, unphotographed deck.
    ///
    /// Compared by `Equatable`, not by rendered description: the frames are
    /// decoded from JSON twice, and a dictionary's key order is not a fact
    /// about its contents.
    func testSampleFramesAreTheDeckUnderADifferentAck() {
        let now = Date(timeIntervalSince1970: 1_753_950_000)
        let deck = Fixtures.frames(now: now, variant: .deck)
        let sample = Fixtures.sampleFrames(now: now)
        XCTAssertEqual(deck.count, sample.count)
        for (index, (a, b)) in zip(deck, sample).enumerated() where index > 0 {
            switch (a, b) {
            case (.sessions(let deckSessions), .sessions(let sampleSessions)):
                XCTAssertEqual(deckSessions, sampleSessions, "frame \(index)")
            case (.event(let deckEvent), .event(let sampleEvent)):
                XCTAssertEqual(deckEvent, sampleEvent, "frame \(index)")
            default:
                XCTFail("frame \(index) changed shape between the deck and the sample")
            }
        }
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
                outcome: nil, paneSnapshot: nil, risk: card.risk)
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
        // The timeline rebuild is scheduled on the main actor; await the
        // fact rather than sleeping a guess at it.
        for state in model.states.values { await state.settleForTesting() }
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

/// The sample fleet, at the model, where its two safety rules live: it claims no
/// link, and it never shares the app with a real one.
///
/// The screens are covered by `SampleModeUITests`, which drives the shipping
/// build with no launch arguments at all. These are the invariants underneath
/// them, which a screenshot cannot see.
@MainActor
final class SampleFleetTests: XCTestCase {
    private func makeModel() -> AppModel {
        AppModel(
            // Ephemeral: these tests assert on the unpaired state, and a real
            // simulator the app has been paired on would otherwise hand every
            // model a pairing that refuses the sample fleet outright.
            pairing: PairingStore(ephemeral: true),
            cache: EventCache(
                root: URL(fileURLWithPath: NSTemporaryDirectory())
                    .appendingPathComponent(UUID().uuidString)),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
    }

    /// A fleet with agents on it, and a link that is still reported as absent —
    /// the whole claim the banner makes, in the state the app is actually in.
    func testTheSampleFleetLoadsWithoutClaimingALink() async {
        let model = makeModel()
        model.startSampleFleet()
        // The timeline rebuild is scheduled on the main actor; await the fact
        // rather than sleeping a guess at it.
        for state in model.states.values { await state.settleForTesting() }

        XCTAssertTrue(model.showsFleet, "the sample fleet is a fleet, or it shows nothing")
        XCTAssertEqual(model.summaries.count, 4)
        XCTAssertEqual(model.deckCount, 3, "three agents blocked")
        XCTAssertFalse(model.pairing.isPaired, "nothing here may create a pairing")

        XCTAssertFalse(
            model.connection.phase.isConnected,
            "a replayed `hello_ack` must not promote the app to connected")
        XCTAssertNil(model.connection.lastContactAt, "no daemon has spoken, ever")
        XCTAssertNotEqual(
            model.linkHealth.level, .live,
            "the one claim this feature could mislead somebody with")
        XCTAssertNil(
            model.actionsBlockedReason,
            "the cards still answer — inline, and only in the sample fleet")
    }

    /// Entry is refused from a paired app, which is what keeps the two kinds of
    /// state from ever being on screen together.
    func testAPairedAppCannotEnterTheSampleFleet() {
        let model = makeModel()
        model.pairing.saveEphemeral(
            DaemonEndpoint(host: "mac.example", port: 8787, credential: .token("t"), useTLS: false))
        model.startSampleFleet()

        XCTAssertFalse(model.sampleFleetActive)
        XCTAssertTrue(model.summaries.isEmpty)
    }

    /// Pairing from inside the sample fleet — its Settings sheet reaches the
    /// pairing form — takes the sample state down first, entirely.
    func testPairingTearsTheSampleFleetDown() async {
        let model = makeModel()
        model.startSampleFleet()
        for state in model.states.values { await state.settleForTesting() }
        XCTAssertEqual(model.deckCount, 3)

        model.pair(
            with: DaemonEndpoint(
                host: "mac.example", port: 8787, credential: .token("t"), useTLS: false))
        defer { model.connection.stop() }

        XCTAssertFalse(model.sampleFleetActive)
        XCTAssertFalse(model.fixturesActive, "the network side effects come back with the pairing")
        XCTAssertTrue(model.summaries.isEmpty)
        XCTAssertEqual(model.deckCount, 0, "a sample card must never be counted beside a real one")
    }

    /// Leaving returns the app to onboarding rather than to an empty fleet.
    func testLeavingTheSampleFleetReturnsToOnboarding() async {
        let model = makeModel()
        model.startSampleFleet()
        for state in model.states.values { await state.settleForTesting() }
        model.stopSampleFleet()

        XCTAssertFalse(model.showsFleet)
        XCTAssertTrue(model.summaries.isEmpty)
        XCTAssertEqual(model.deckCount, 0)
        XCTAssertNil(model.connection.capabilities, "the sample daemon's claims go with it")
    }
}
