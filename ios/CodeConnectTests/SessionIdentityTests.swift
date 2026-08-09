import XCTest

@testable import CodeConnect

/// Protocol minor 2: a session has an identity (`session_uid`) as well as a name
/// (`session_id`), and the app keys everything by the identity.
///
/// The bug all of this exists to prevent, stated once: `codeconnect claude` takes the
/// lowest free tmux name, so when a run exits the next one is called `cc-1`
/// again. Keying by that name made one `cc-1` timeline out of two unrelated
/// agent runs — and made an answer to one of them addressable by the other.
///
/// Frames here are hand-written JSON decoded by the app's real decoders, for the
/// same reason as `ProtocolTests`: a round trip through this app's own encoder
/// passes just as happily when both ends are wrong together.
@MainActor
final class SessionIdentityTests: XCTestCase {

    // MARK: - Wire

    func testAnEventCarriesBothIdentitiesAndIsKeyedByTheUID() throws {
        let event = try decodeEvent(
            """
            {"seq":7,"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
             "ts":"2026-07-30T16:36:58.412Z","kind":"tool_call","payload":{"tool_name":"Bash"},
             "source":"hook"}
            """)
        XCTAssertEqual(event.sessionUID, "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        XCTAssertEqual(event.sessionID, "cc-1", "the tmux name is still there, for display")
        XCTAssertEqual(event.sessionKey, "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
    }

    /// `protocol/src/event.rs` marks `session_uid` `#[serde(default)]`, so an
    /// event logged before uids existed must still decode — and must be keyed by
    /// the only identity it has.
    func testAPreUIDEventStillDecodesAndFallsBackToTheName() throws {
        let event = try decodeEvent(
            """
            {"seq":3,"session_id":"cc-1","ts":"t","kind":"tool_call","payload":{},"source":"hook"}
            """)
        XCTAssertEqual(event.sessionUID, "")
        XCTAssertEqual(event.sessionKey, "cc-1")
    }

    func testTwoRunsOfOneNameGetDifferentEventIdentities() throws {
        let first = try decodeEvent(
            """
            {"seq":1,"session_uid":"01AAAAAAAAAAAAAAAAAAAAAAAA","session_id":"cc-1","ts":"t",
             "kind":"session_start","payload":{},"source":"daemon"}
            """)
        let second = try decodeEvent(
            """
            {"seq":1,"session_uid":"01BBBBBBBBBBBBBBBBBBBBBBBB","session_id":"cc-1","ts":"t",
             "kind":"session_start","payload":{},"source":"daemon"}
            """)
        XCTAssertNotEqual(
            first.id, second.id,
            "seq 1 of two runs is two facts; one id would render one of them")
    }

    func testASessionSummaryIsIdentifiedByItsUID() throws {
        let sessions = try decodeSessions(
            twoRunsSharingTheName(liveSeq: 4, deadSeq: 9))
        XCTAssertEqual(sessions.map(\.sessionID), ["cc-1", "cc-1"])
        XCTAssertEqual(Set(sessions.map(\.sessionKey)).count, 2)
        XCTAssertEqual(sessions[0].sessionID, "cc-1", "the handle is still there for attach")
    }

    func testCapabilityAndProfileAgreeOnUIDScoping() throws {
        let modern = try profile(minor: 2, sessionUID: true)
        XCTAssertTrue(modern.scopesSessionsByUID)
        // The flag alone is enough — a daemon that says it does this does it.
        XCTAssertTrue(try profile(minor: 1, sessionUID: true).scopesSessionsByUID)
        // And the feature level alone is enough, for a daemon that renamed the key.
        XCTAssertTrue(try profile(minor: 2, sessionUID: false).scopesSessionsByUID)
        // A minor-1 daemon does neither, and must keep being treated as name-keyed.
        XCTAssertFalse(try profile(minor: 1, sessionUID: false).scopesSessionsByUID)
        XCTAssertFalse(DaemonProfile.unknown.scopesSessionsByUID)
    }

    // MARK: - Answering by uid

    func testAnAnswerNamesTheRunItCameFrom() throws {
        let fields = try encodeClient(
            .answer(
                requestID: "toolu_1", payloadHash: "h", decision: .allow,
                session: "01K1B3XQ8ZC0DE5FGH7JKMNPQR"))
        XCTAssertEqual(fields["type"]?.stringValue, "answer")
        XCTAssertEqual(fields["session_id"]?.stringValue, "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
    }

    func testAnUnscopedAnswerOmitsTheFieldRatherThanSendingNull() throws {
        let fields = try encodeClient(
            .answer(requestID: "toolu_1", payloadHash: "h", decision: .deny, session: nil))
        XCTAssertNil(fields["session_id"])
    }

    func testSubscribeAndTheOtherSessionMessagesTakeAReference() throws {
        XCTAssertEqual(
            try encodeClient(.subscribe(session: "01AAAAAAAAAAAAAAAAAAAAAAAA", afterSeq: 12))[
                "session_id"]?.stringValue,
            "01AAAAAAAAAAAAAAAAAAAAAAAA")
        XCTAssertEqual(
            try encodeClient(.capture(session: "cc-1", lines: 40))["session_id"]?.stringValue,
            "cc-1", "a legacy daemon still gets the only reference there is")
    }

    /// A `request_id` is unique within a run and not across them, which is why
    /// the daemon refuses an unscoped answer when two runs hold the same id.
    func testACardIsIdentifiedByItsRunAndItsRequest() {
        let first = approval(session: "01AAAAAAAAAAAAAAAAAAAAAAAA", request: "toolu_1")
        let second = approval(session: "01BBBBBBBBBBBBBBBBBBBBBBBB", request: "toolu_1")
        XCTAssertNotEqual(first.id, second.id)

        var pass = DeckPass()
        pass.settle(first.id)
        XCTAssertEqual(
            pass.arrange(
                [first, second],
                profile: DaemonProfile(protocolVersion: 1, protocolMinor: 1, capabilities: nil)
            ).map(\.sessionKey), [second.sessionKey],
            "answering one run's card must not clear another run's")
    }

    // MARK: - The fleet

    /// The splice, end to end: two runs called `cc-1`, each with its own log,
    /// must land in two stores and render as two rows.
    func testTwoRunsSharingANameAreTwoSessions() async throws {
        let model = makeModel()
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(try decode(helloAckJSON(minor: 2, sessionUID: true)))
        model.connection.injectForTesting(
            try decode(twoRunsSharingTheName(liveSeq: 2, deadSeq: 2)))
        model.connection.injectForTesting(
            try decode(
                eventFrame(uid: liveUID, name: "cc-1", seq: 1, text: "the live run said this")))
        model.connection.injectForTesting(
            try decode(
                eventFrame(uid: deadUID, name: "cc-1", seq: 1, text: "the dead run said this")))
        await settle()

        XCTAssertEqual(model.states.count, 2, "one store per run, not one per name")
        XCTAssertEqual(model.fleet.count, 2)
        XCTAssertEqual(
            model.states[liveUID]?.events.count, 1,
            "each run holds only its own events — this is the splice")
        XCTAssertEqual(model.states[deadUID]?.events.count, 1)
        // Neither row says `cc-1`: the counter is reused by the next run and
        // names nothing. This daemon named no project either, so both rows say
        // so — and are still told apart, by the one honest thing left.
        let labels = model.fleet.map(\.label)
        XCTAssertEqual(Set(labels).count, 2, "two identical rows would be its own kind of lie")
        // The single-key accessor every detail view calls agrees with the row.
        XCTAssertEqual(
            model.runLabel(for: liveUID), model.fleet.first { $0.id == liveUID }?.label)
        XCTAssertEqual(
            model.runLabel(for: "a run nobody has heard of"), .unknown,
            "an unknown key is not a crash and not a made-up name")
        XCTAssertTrue(labels.allSatisfy { $0.project == "Unknown project" })
        XCTAssertTrue(labels.allSatisfy { $0.inline.contains("started") })
        XCTAssertFalse(labels.contains { $0.inline.contains("cc-1") })
    }

    /// The same frames from a daemon that mints no uids: one session, keyed by
    /// name, exactly as before. Legacy behaviour is a feature, not a leftover.
    func testALegacyDaemonKeepsKeyingByName() async throws {
        let model = makeModel()
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(try decode(helloAckJSON(minor: 1, sessionUID: false)))
        model.connection.injectForTesting(
            try decode(
                """
                {"type":"sessions","sessions":[{"session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/tmp/one","lifecycle":"live","link":"attached","last_seq":2,
                 "created_at":"2026-07-31T09:00:00.000Z","updated_at":"2026-07-31T09:00:00.000Z"}]}
                """))
        model.connection.injectForTesting(
            try decode(eventFrame(uid: nil, name: "cc-1", seq: 1, text: "one")))
        model.connection.injectForTesting(
            try decode(eventFrame(uid: nil, name: "cc-1", seq: 2, text: "two")))
        await settle()

        XCTAssertEqual(model.states.count, 1)
        XCTAssertEqual(model.states["cc-1"]?.events.count, 2)
        XCTAssertEqual(
            model.fleet.first?.label, .unknown,
            "one run, no project named, and nothing to tell it from")
    }

    /// A run the daemon has stopped listing cannot contribute a decision to the
    /// Deck: nobody could answer it, and the card would be a request from a
    /// session that no longer exists.
    func testStatesTheDaemonNoLongerListsAreDropped() async throws {
        let model = makeModel()
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(try decode(helloAckJSON(minor: 2, sessionUID: true)))
        model.connection.injectForTesting(
            try decode(twoRunsSharingTheName(liveSeq: 1, deadSeq: 1)))
        model.connection.injectForTesting(
            try decode(eventFrame(uid: deadUID, name: "cc-1", seq: 1, text: "gone")))
        await settle()
        XCTAssertNotNil(model.states[deadUID])

        model.connection.injectForTesting(
            try decode(
                """
                {"type":"sessions","sessions":[{"session_uid":"\(liveUID)","session_id":"cc-1",
                 "tmux_session":"cc-1","cwd":"/tmp/live","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-07-31T09:05:00.000Z",
                 "updated_at":"2026-07-31T09:05:00.000Z"}]}
                """))
        await settle()
        XCTAssertNil(model.states[deadUID], "the daemon stopped vouching for it")
        XCTAssertNotNil(model.states[liveUID])
    }

    func testADeepLinkByNameResolvesToTheRunTheDaemonWouldPick() async throws {
        let model = makeModel()
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(try decode(helloAckJSON(minor: 2, sessionUID: true)))
        model.connection.injectForTesting(
            try decode(twoRunsSharingTheName(liveSeq: 1, deadSeq: 1)))
        await settle()

        XCTAssertEqual(
            model.resolveSessionKey(reference: "cc-1"), liveUID,
            "a bare name means the run with a supervisor attached (mac/README.md)")
        XCTAssertEqual(
            model.resolveSessionKey(reference: deadUID), deadUID, "a uid is exact")
        XCTAssertNil(model.resolveSessionKey(reference: "cc-9"))
    }

    /// **The uid keys the run; it never names it.** Two runs really can share a
    /// tmux name, and the rows still have to be told apart — but by what a
    /// reader recognises, not by a slice of an identifier. Every route,
    /// subscription and answer below still travels on the uid.
    func testTwoRunsSharingATmuxNameAreToldApartWithoutShowingTheUID() throws {
        let sessions = try decodeSessions(twoRunsSharingTheName(liveSeq: 1, deadSeq: 1))
        XCTAssertEqual(liveUID.suffix(6), deadUID.suffix(6), "the case this test exists for")

        let labels = RunLabel.labels(for: sessions)
        XCTAssertEqual(Set(labels.values).count, 2, "two identical rows would be their own lie")
        for label in labels.values {
            XCTAssertFalse(label.inline.contains("cc-1"), "the tmux counter names nothing")
            XCTAssertFalse(
                label.inline.contains(liveUID.suffix(6)), "and a uid tail is not a name")
        }

        // A single run has nothing to be told apart from, so it wears nothing.
        let alone = try decodeSessions(
            """
            {"type":"sessions","sessions":[{"session_uid":"\(liveUID)","session_id":"cc-1",
             "tmux_session":"cc-1","cwd":"/tmp/live","project_label":"live","lifecycle":"live",
             "link":"attached","last_seq":1,"created_at":"t","updated_at":"t"}]}
            """)
        XCTAssertEqual(
            RunLabel.labels(for: alone)[liveUID], RunLabel(project: "live", qualifier: nil))
    }

    // MARK: - Cache migration

    func testAdoptingUIDKeyingDropsNameKeyedHistoryAndSaysWhichSessions() async throws {
        let cache = EventCache(root: temporaryDirectory())
        await cache.saveEvents([try decodeEvent(eventJSON(uid: nil, name: "cc-1", seq: 1))], key: "cc-1")
        await cache.saveEvents([try decodeEvent(eventJSON(uid: nil, name: "cc-2", seq: 1))], key: "cc-2")
        let before = await cache.loadEvents(key: "cc-1")
        XCTAssertNotNil(before)

        let dropped = await cache.adopt(keying: .uid)
        XCTAssertEqual(dropped, ["cc-1", "cc-2"], "each one has to be able to say so")
        let reloaded = await cache.loadEvents(key: "cc-1")
        XCTAssertNil(reloaded, "a file keyed by a reused name cannot be given to a run")

        let again = await cache.adopt(keying: .uid)
        XCTAssertTrue(again.isEmpty, "the migration is not a thing that happens twice")
    }

    func testUIDKeyedHistorySurvivesAReconnectToTheSameDaemon() async throws {
        let root = temporaryDirectory()
        let cache = EventCache(root: root)
        _ = await cache.adopt(keying: .uid)
        await cache.saveEvents(
            [try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 1))], key: liveUID)

        // A second process — a relaunch — reads the same directory.
        let reopened = EventCache(root: root)
        let droppedOnReconnect = await reopened.adopt(keying: .uid)
        XCTAssertTrue(droppedOnReconnect.isEmpty)
        let kept = await reopened.loadEvents(key: liveUID)
        XCTAssertEqual(kept?.events.count, 1)
    }

    func testTheDiscardIsAnnouncedOnTheSessionThatLostItsHistory() {
        let state = SessionState(sessionKey: liveUID)
        state.noteCacheDiscarded()
        XCTAssertEqual(state.gap?.cause, .cacheDiscarded)
        XCTAssertTrue(state.gap?.message.contains("discarded") == true)
    }

    // MARK: - Gaps close

    /// A gap banner is a claim about *now*. When the missing events arrive it
    /// stops being true, and a warning left up after its reason is gone teaches
    /// people to ignore warnings.
    func testAGapBannerIsWithdrawnWhenTheMissingEventsArrive() async throws {
        let state = SessionState(sessionKey: liveUID)
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 1)))
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 5)))
        XCTAssertEqual(state.gap?.cause, .sequenceJump(missing: 3))

        // The replay a "Load all" produces: everything, in order, including what
        // we already hold.
        for seq in UInt64(1)...5 {
            state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: seq)))
        }
        await settle()
        XCTAssertNil(state.gap, "the hole is closed, so the banner is not true any more")
        XCTAssertEqual(state.events.map(\.seq), [1, 2, 3, 4, 5])
    }

    func testAPartiallyFilledGapRestatesWhatIsStillMissing() async throws {
        let state = SessionState(sessionKey: liveUID)
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 1)))
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 5)))
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 3)))
        await settle()
        XCTAssertEqual(
            state.gap?.cause, .sequenceJump(missing: 2),
            "two of the three arrived; the banner must not claim all three are still missing")
    }

    func testADismissedGapStaysDismissed() async throws {
        let state = SessionState(sessionKey: liveUID)
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 1)))
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 4)))
        state.dismissGap()
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 5)))
        await settle()
        XCTAssertNil(state.gap, "re-raising a hole the reader has acknowledged is nagging")
    }

    // MARK: - Replay is batched

    /// The replay case, arriving the way it really arrives: one frame per turn of
    /// the main actor. Rebuilding per frame is what made a 400-event backfill
    /// hundreds of full timeline passes and hundreds of whole-array inserts.
    func testAFullLogReplayIsMergedAndRebuiltInOnePass() async throws {
        // Decoded up front: this measures how ingest batches, not how fast JSON
        // parses.
        let log = try (UInt64(1)...260).map {
            try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: $0))
        }
        let state = SessionState(sessionKey: liveUID)
        // The tail we already hold, and the head we know we skipped.
        for event in log.suffix(60) { state.ingest(event) }
        state.noteTruncatedHead()
        await settle()
        let rebuildsBeforeReplay = state.rebuildCount
        XCTAssertTrue(state.headTruncated)

        // "Load all": the whole log from the beginning, one frame at a time —
        // each one on its own turn of the main actor, which is how a WebSocket
        // delivers them.
        let clock = ContinuousClock()
        let started = clock.now
        for event in log {
            state.ingest(event)
            await Task.yield()
        }
        let elapsed = clock.now - started
        await settle()

        XCTAssertEqual(state.events.map(\.seq), Array(UInt64(1)...260), "sorted and deduplicated")
        // Measured against the window rather than against a fixed number: the
        // guarantee is a *rate* — at most one pass per window — and hard-coding
        // a count would only be measuring how fast this machine runs a loop.
        // The behaviour this replaces rebuilt once per event, which is 260.
        let allowed = Int(elapsed / SessionState.coalesceWindow) + 2
        XCTAssertLessThanOrEqual(
            state.rebuildCount - rebuildsBeforeReplay, allowed,
            "260 replayed events must not cost 260 merges and 260 timeline builds")
        XCTAssertLessThan(state.rebuildCount - rebuildsBeforeReplay, log.count / 4)
        XCTAssertFalse(state.headTruncated, "the head arrived, so the banner is no longer true")
    }

    func testMergingKeepsTheLogSortedAndUnique() throws {
        let existing = try [1, 3, 5, 7].map {
            try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: UInt64($0)))
        }
        let incoming = try [2, 3, 4, 8].map {
            try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: UInt64($0)))
        }
        XCTAssertEqual(
            SessionState.merged(existing, incoming).map(\.seq), [1, 2, 3, 4, 5, 7, 8])
        XCTAssertEqual(SessionState.merged([], incoming).map(\.seq), [2, 3, 4, 8])
        XCTAssertEqual(SessionState.merged(existing, []).map(\.seq), [1, 3, 5, 7])
    }

    /// A replay in flight is held for a few milliseconds; a cache write that
    /// landed mid-window would persist a log with a hole in it and then trust
    /// that file on the next cold open.
    func testPendingEventsAreSettledBeforeTheyCanBePersisted() throws {
        let state = SessionState(sessionKey: liveUID)
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 2)))
        state.ingest(try decodeEvent(eventJSON(uid: liveUID, name: "cc-1", seq: 1)))
        XCTAssertEqual(state.events.map(\.seq), [2], "still buffered")
        state.settlePendingEvents()
        XCTAssertEqual(state.events.map(\.seq), [1, 2])
    }

    // MARK: - Review marks stay bounded

    /// Uids are never reused, so this map gains an entry per agent run rather
    /// than per tmux name and has to be bounded or it grows forever.
    func testReviewMarksKeepTheNewestRunsAndDropTheOldest() {
        var map: [String: NSNumber] = [:]
        for index in 0..<(ReviewMarks.maxEntries + 50) {
            map[String(format: "01%024d", index)] = NSNumber(value: index)
        }
        let pruned = ReviewMarks.pruned(map)
        XCTAssertEqual(pruned.count, ReviewMarks.maxEntries)
        XCTAssertNil(pruned[String(format: "01%024d", 0)], "the oldest ULID goes first")
        XCTAssertNotNil(
            pruned[String(format: "01%024d", ReviewMarks.maxEntries + 49)],
            "a ULID sorts by the millisecond it was minted, so the newest survive")
    }

    func testReviewMarksLeaveASmallMapAlone() {
        let map = ["cc-1": NSNumber(value: 4), "cc-2": NSNumber(value: 9)]
        XCTAssertEqual(ReviewMarks.pruned(map), map)
    }

    // MARK: - Helpers

    private let liveUID = "01K1B3XZZZC0DE5FGH7JKMNPQR"
    private let deadUID = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private func makeModel() -> AppModel {
        AppModel(
            pairing: PairingStore(),
            cache: EventCache(root: temporaryDirectory()),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
    }

    private func temporaryDirectory() -> URL {
        URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent(UUID().uuidString)
    }

    /// Let the ingest coalescing window close and any cache task run.
    private func settle() async {
        try? await Task.sleep(for: .milliseconds(80))
    }

    private func decode(_ json: String) throws -> ServerMessage {
        try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
    }

    private func decodeEvent(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    private func decodeSessions(_ json: String) throws -> [SessionSummary] {
        guard case .sessions(let list) = try decode(json) else {
            throw XCTSkip("not a sessions frame")
        }
        return list
    }

    private func profile(minor: UInt32, sessionUID: Bool) throws -> DaemonProfile {
        guard case .helloAck(let ack) = try decode(helloAckJSON(minor: minor, sessionUID: sessionUID))
        else { throw XCTSkip("not a hello_ack") }
        return DaemonProfile(
            protocolVersion: ack.protocolVersion, protocolMinor: ack.protocolMinor,
            capabilities: ack.capabilities)
    }

    private func encodeClient(_ message: ClientMessage) throws -> [String: JSONValue] {
        let data = try JSONEncoder().encode(message)
        let value = try JSONDecoder().decode(JSONValue.self, from: data)
        return try XCTUnwrap(value.objectValue)
    }

    private func approval(session: String, request: String) -> ApprovalItem {
        ApprovalItem(
            card: ApprovalCard(
                requestID: request, payloadHash: "h", toolName: "Bash", toolInput: .object([:]),
                displayText: "d", permissionSuggestions: nil, promptID: nil, permissionMode: nil,
                risk: nil),
            requestedAt: Date(timeIntervalSince1970: 1_000_000), sessionKey: session,
            outcome: nil, paneSnapshot: nil, risk: nil)
    }

    private func helloAckJSON(minor: UInt32, sessionUID: Bool) -> String {
        """
        {"type":"hello_ack","protocol_version":1,"protocol_minor":\(minor),
         "server_time":"2026-07-31T09:14:00.000Z",
         "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",
           "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,
           "push":false,"tls":false,"tls_active":false,"diff":true,"risk_class":true,
           "session_uid":\(sessionUID)}}
        """
    }

    /// A dead `cc-1` and a live `cc-1`, which is what the daemon really reports
    /// after one run exits and the next takes the name.
    private func twoRunsSharingTheName(liveSeq: UInt64, deadSeq: UInt64) -> String {
        """
        {"type":"sessions","sessions":[
          {"session_uid":"\(deadUID)","session_id":"cc-1","tmux_session":"cc-1",
           "cwd":"/tmp/first","lifecycle":"exited","link":"detached","last_seq":\(deadSeq),
           "created_at":"2026-07-31T09:00:00.000Z","updated_at":"2026-07-31T09:01:00.000Z"},
          {"session_uid":"\(liveUID)","session_id":"cc-1","tmux_session":"cc-1",
           "cwd":"/tmp/second","lifecycle":"live","link":"attached","last_seq":\(liveSeq),
           "created_at":"2026-07-31T09:05:00.000Z","updated_at":"2026-07-31T09:06:00.000Z"}]}
        """
    }

    private func eventJSON(uid: String?, name: String, seq: UInt64, text: String = "hi") -> String {
        let identity = uid.map { "\"session_uid\":\"\($0)\"," } ?? ""
        return """
            {\(identity)"seq":\(seq),"session_id":"\(name)",
             "ts":"2026-07-31T09:0\(seq % 10):00.000Z","kind":"agent_message",
             "payload":{"text":"\(text)"},"source":"transcript"}
            """
    }

    /// The same event wrapped as the frame the socket delivers.
    private func eventFrame(uid: String?, name: String, seq: UInt64, text: String = "hi") -> String
    {
        "{\"type\":\"event\",\"event\":\(eventJSON(uid: uid, name: name, seq: seq, text: text))}"
    }
}
