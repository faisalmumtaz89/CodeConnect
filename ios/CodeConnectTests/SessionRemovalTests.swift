import XCTest

@testable import CodeConnect

/// Removing a session is the phone's first destructive verb, so the rules that
/// keep it safe are pinned here.
///
/// The daemon refuses a live run in SQL whatever the phone believes
/// (`store.rs: WHERE session_uid = ? AND lifecycle = 'exited'`). These cover the
/// phone's half: that it does not *offer* what would be refused, that it decodes
/// each answer correctly, and that a removal cleans up locally rather than
/// leaving a row that reappears on the next refresh.
@MainActor
final class SessionRemovalTests: XCTestCase {

    private func summary(
        lifecycle: String, uid: String = "01K1B3XQ8ZC0DE5FGH7JKMNPQR", tmux: String = "cc-1"
    )
        -> SessionSummary
    {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"cc-1",
                 "tmux_session":"\(tmux)","cwd":"/tmp/x","lifecycle":"\(lifecycle)",
                 "link":"attached","last_seq":9,"created_at":"2026-08-02T10:00:00Z",
                 "updated_at":"2026-08-02T10:00:00Z","blocked_on":[]}
                """.utf8))
    }

    private func capabilities(_ json: String) -> Capabilities {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(Capabilities.self, from: Data(json.utf8))
    }

    // MARK: The wire

    func testTheRequestNamesTheRunByUidNotByTmuxName() throws {
        let encoded = try JSONEncoder().encode(
            ClientMessage.deleteSession(sessionUID: "01K1B3XQ8ZC0DE5FGH7JKMNPQR"))
        let text = String(decoding: encoded, as: UTF8.self)
        XCTAssertTrue(text.contains("\"delete_session\""), text)
        XCTAssertTrue(text.contains("\"session_uid\""), text)
        XCTAssertFalse(
            text.contains("\"session_id\""),
            """
            A tmux name is handed to the next run, so a phone holding a stale `cc-1` \
            could name a session it never saw. The request must carry the uid alone.
            """)
    }

    func testEachAnswerDecodesToItsOwnMeaning() throws {
        func decode(_ json: String) throws -> ServerMessage {
            try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
        }
        let deleted = try decode(
            #"{"type":"delete_session_result","session_uid":"U","result":{"status":"deleted","events":5}}"#)
        guard case .deleteSessionResult(_, .deleted(let events)) = deleted else {
            return XCTFail("expected deleted, got \(deleted)")
        }
        XCTAssertEqual(events, 5, "the count is reported because afterwards there is none to take")

        let running = try decode(
            #"{"type":"delete_session_result","session_uid":"U","result":{"status":"still_running"}}"#)
        guard case .deleteSessionResult(_, .stillRunning) = running else {
            return XCTFail("expected stillRunning, got \(running)")
        }

        let gone = try decode(
            #"{"type":"delete_session_result","session_uid":"U","result":{"status":"not_found"}}"#)
        guard case .deleteSessionResult(_, .notFound) = gone else {
            return XCTFail("expected notFound, got \(gone)")
        }
    }

    // MARK: What is offered

    /// An older Mac has no `delete_session`. Unknown is false, so the swipe is
    /// absent rather than present and ineffective.
    func testAnOlderDaemonIsNotOfferedRemoval() {
        let profile = DaemonProfile(
            protocolVersion: 1, protocolMinor: 6,
            capabilities: capabilities(
                #"{"can_approve_reliably":true,"fail_mode":"fail_open","answer_path":"hook_return","hold_secs":25,"send_text":true,"capture":true,"push":true,"tls":false}"#),
            deviceName: nil)
        XCTAssertFalse(profile.removesSessions)
    }

    func testADaemonThatAdvertisesItIsOfferedRemoval() {
        let profile = DaemonProfile(
            protocolVersion: 1, protocolMinor: 7,
            capabilities: capabilities(
                #"{"can_approve_reliably":true,"fail_mode":"fail_open","answer_path":"hook_return","hold_secs":25,"send_text":true,"capture":true,"push":true,"tls":false,"delete_session":true}"#),
            deviceName: nil)
        XCTAssertTrue(profile.removesSessions)
    }

    // MARK: Which answers are allowed to destroy anything

    /// `meansItIsGone` is the single place the destructive reading is decided, so
    /// this is the whole rule rather than a sample of it.
    func testOnlyTheTwoAnswersThatMeanGoneAreDestructive() {
        XCTAssertTrue(DeleteSessionResult.deleted(events: 5).meansItIsGone)
        XCTAssertTrue(DeleteSessionResult.notFound.meansItIsGone, "the Mac does not have it")
        XCTAssertFalse(DeleteSessionResult.stillRunning.meansItIsGone)
        XCTAssertFalse(
            DeleteSessionResult.failed(message: "disk").meansItIsGone,
            "a daemon that could not delete it did not delete it")
        XCTAssertFalse(
            DeleteSessionResult.unknown(status: "queued").meansItIsGone,
            """
            A status this build has never seen says nothing about whether the run \
            is gone, and this used to decode straight to `notFound` — so any status \
            a future daemon invents would have wiped the row and its cache here.
            """)
    }

    /// The daemon answers its own request even when it fails, because an `error`
    /// frame names no session and a phone cannot tell one is meant for it.
    func testAFailureIsAnAnswerRatherThanSilence() throws {
        let decoded = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                #"{"type":"delete_session_result","session_uid":"U","result":{"status":"failed","message":"disk full"}}"#
                    .utf8))
        guard case .deleteSessionResult(_, .failed(let message)) = decoded else {
            return XCTFail("expected failed, got \(decoded)")
        }
        XCTAssertEqual(message, "disk full")
    }

    func testAnUnknownStatusKeepsItsName() throws {
        let decoded = try JSONDecoder().decode(
            DeleteSessionResult.self, from: Data(#"{"status":"queued"}"#.utf8))
        XCTAssertEqual(decoded, .unknown(status: "queued"))
    }

    // MARK: The local half

    /// A model wired to answer without a socket.
    ///
    /// **The seam is what makes the gate tests mean anything.** A bare
    /// `AppModel()` has no connection, so `deleteSession` throws `notConnected`
    /// and `removeSession`'s `try?` turns that into the same `nil` its guards
    /// return. Every one of these tests passed with the guards deleted — proven
    /// by deleting them — because they could only see the answer, and all three
    /// causes produce the same answer. What separates them is whether a request
    /// was *sent*, so that is what these assert.
    private func connectedModel(
        canDelete: Bool = true, answering result: DeleteSessionResult = .deleted(events: 3),
        cache: EventCache = EventCache()
    ) -> AppModel {
        let model = AppModel(cache: cache)
        let caps = capabilities(
            #"{"can_approve_reliably":true,"fail_mode":"fail_open","answer_path":"hook_return","hold_secs":25,"send_text":true,"capture":true,"push":true,"tls":false\#(canDelete ? #","delete_session":true"# : "")}"#
        )
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(caps, minor: canDelete ? 7 : 6)
        model.connection.deleteStub = { _ in result }
        return model
    }

    /// The gate is `lifecycle`, not the derived Ended status: the status rule can
    /// read Ended without the run being proven exited, and asking the daemon to
    /// destroy something still alive is a request that should never leave.
    func testALiveRunIsNeverEvenAskedAbout() async {
        let model = connectedModel()
        let result = await model.removeSession(summary(lifecycle: "live"))
        XCTAssertNil(result)
        XCTAssertEqual(
            model.connection.deleteRequests, [],
            "a live run must not produce a request at all — and this daemon would have said yes")
    }

    /// An ended run on a Mac that never advertised `delete_session` is also never
    /// asked about. `FleetView` hides the swipe, but this is the method that puts
    /// bytes on the wire: an older daemon answers an unknown message with an
    /// error, not a result, so the request would end in a spinner and a timeout.
    func testAnEndedRunOnADaemonThatCannotDeleteIsNeverAskedAboutEither() async {
        let model = connectedModel(canDelete: false)
        XCTAssertFalse(model.daemonProfile.removesSessions, "the premise")
        let result = await model.removeSession(summary(lifecycle: "exited"))
        XCTAssertNil(result)
        XCTAssertEqual(model.connection.deleteRequests, [])
    }

    /// And the case that proves the two above are gates rather than a broken
    /// method: an ended run on a daemon that can delete *is* asked about.
    func testAnEndedRunOnACapableDaemonIsAsked() async {
        let model = connectedModel()
        let target = summary(lifecycle: "exited")
        let result = await model.removeSession(target)
        XCTAssertEqual(result, .deleted(events: 3))
        XCTAssertEqual(model.connection.deleteRequests, [target.sessionUID])
    }

    /// A refusal must leave everything alone — including the caches.
    func testARefusedRemovalChangesNothingLocally() async {
        let root = tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        let cache = EventCache(root: root)
        let target = summary(lifecycle: "exited")
        await cache.saveFleet([target])

        let model = connectedModel(answering: .stillRunning, cache: cache)
        let result = await model.removeSession(target)

        XCTAssertEqual(result, .stillRunning)
        XCTAssertEqual(result.refusal, "Still running", "and the user is told")
        let survived = await cache.loadFleet()?.sessions.map(\.sessionUID)
        XCTAssertEqual(
            survived, [target.sessionUID],
            "a session the Mac refused to delete must still be there on the next cold open")
    }

    // MARK: The removal gate, as a matrix

    /// The phone's gate mirrors the daemon's rule exactly. Hosted rows need the
    /// proof (`lifecycle == .exited`); unhosted rows — empty `tmux_session`,
    /// adopted, nothing ever put them in tmux — are removable at any lifecycle,
    /// because the proof cannot exist and a row waiting on an unobtainable
    /// proof would be immortal.
    func testTheRemovalGateMatrix() {
        XCTAssertFalse(summary(lifecycle: "live").isRemovable)
        XCTAssertFalse(summary(lifecycle: "unknown").isRemovable)
        XCTAssertTrue(summary(lifecycle: "exited").isRemovable)
        for lifecycle in ["live", "unknown", "exited"] {
            XCTAssertTrue(
                summary(lifecycle: lifecycle, tmux: "").isRemovable,
                "an unhosted \(lifecycle) row must be removable")
        }
    }

    /// And the gate is what `removeSession` consults: an unhosted live run IS
    /// asked about, where a hosted live run never is.
    func testAnUnhostedLiveRunIsAskedAbout() async {
        let model = connectedModel()
        let target = summary(lifecycle: "live", tmux: "")
        let result = await model.removeSession(target)
        XCTAssertEqual(result, .deleted(events: 3))
        XCTAssertEqual(model.connection.deleteRequests, [target.sessionUID])
    }

    /// Empty means "no known location", and the answer is nil — never a guess.
    /// The old fallback substituted `session_id`, which for an adopted run is a
    /// name tmux has never heard of, and for a recycled `cc-*` name could be a
    /// different agent's keyboard.
    func testTmuxNameIsNilWhenTheDaemonReportsNoLocation() {
        let model = AppModel()
        let unhosted = summary(lifecycle: "live", tmux: "")
        let hosted = summary(lifecycle: "live", uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS")
        model.adoptSummariesForTesting([unhosted, hosted])
        XCTAssertNil(model.tmuxName(for: unhosted.sessionKey))
        XCTAssertEqual(model.tmuxName(for: hosted.sessionKey), "cc-1")
    }

    // MARK: The cached-banner clocks

    /// The grace clock's lifecycle, through the real restore and the real
    /// frame handler. `fleetCacheRestoredAt` is when *this launch* painted the
    /// cache — the fact the banner's grace runs on — and it must exist exactly
    /// while the cached fleet is what is on screen.
    func testTheRestoreClockIsSetByRestoreAndClearedByTheFirstLiveFrame() async {
        let root = tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        let cache = EventCache(root: root)
        await cache.saveFleet([summary(lifecycle: "exited")])

        let model = connectedModel(cache: cache)
        XCTAssertNil(model.fleetCacheRestoredAt, "no restore has happened yet")

        await model.loadCachedFleetForTesting()
        XCTAssertNotNil(model.fleetCachedAt)
        XCTAssertNotNil(model.fleetCacheRestoredAt, "the restore stamps the grace clock")

        model.connection.injectForTesting(.sessions([]))
        XCTAssertNil(model.fleetCachedAt, "the first live frame retires the cache")
        XCTAssertNil(model.fleetCacheRestoredAt, "and its clock with it")
    }

    // MARK: What has to be cleaned up, and why

    private func tempRoot() -> URL {
        URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("cc-removal-\(UUID().uuidString)", isDirectory: true)
    }

    /// **The row itself lives in `fleet.json`.** `loadCachedFleet` restores that
    /// file wholesale on a cold open, so a removal that cleared only the events
    /// file put the row back on the next launch — and while offline, kept it.
    ///
    /// Driven through `removeSession` rather than by replaying what it does. An
    /// earlier version called `saveFleet` itself and so stayed green with the
    /// `saveFleet` deleted from `removeSession` — testing `EventCache`, which
    /// was never in doubt, instead of the thing that had the defect.
    func testARemovedRowDoesNotComeBackFromTheFleetCache() async {
        let root = tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        let cache = EventCache(root: root)

        let kept = summary(lifecycle: "exited", uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        let removed = summary(lifecycle: "exited", uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQS")
        await cache.saveFleet([kept, removed])

        let model = connectedModel(cache: cache)
        model.adoptSummariesForTesting([kept, removed])
        await model.removeSession(removed)

        let reloaded = await cache.loadFleet()?.sessions.map(\.sessionUID)
        XCTAssertEqual(
            reloaded, [kept.sessionUID],
            "a cold open must not restore a session the Mac no longer has")
    }

    /// The review-mark map is capped at 200 and pruned oldest-first by ULID, so a
    /// mark held for a deleted run is not merely untidy: it is a slot taken from a
    /// run that still exists, and the evicted one's finished work reads as new.
    func testARemovedSessionGivesUpItsReviewMark() {
        let key = "01K1B3XQ8ZC0DE5FGH7JKMNPQR-test-\(UUID().uuidString)"
        ReviewMarks.markReviewed(sessionKey: key, seq: 42)
        XCTAssertEqual(ReviewMarks.reviewedSeq(for: key), 42)

        ReviewMarks.forget(sessionKey: key)
        XCTAssertEqual(ReviewMarks.reviewedSeq(for: key), 0)
    }

    func testForgettingAMarkLeavesEveryOtherMarkAlone() {
        let mine = "cc-forget-\(UUID().uuidString)"
        let theirs = "cc-keep-\(UUID().uuidString)"
        ReviewMarks.markReviewed(sessionKey: mine, seq: 1)
        ReviewMarks.markReviewed(sessionKey: theirs, seq: 2)
        defer { ReviewMarks.forget(sessionKey: theirs) }

        ReviewMarks.forget(sessionKey: mine)
        XCTAssertEqual(ReviewMarks.reviewedSeq(for: theirs), 2)
    }
}
