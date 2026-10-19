import SwiftUI
import XCTest

@testable import CodeConnect

/// Earlier pages (protocol minor 23), against the frames a minor-23 daemon sends.
///
/// `minor-23-wire.json` is captured, not written: a `demo-daemon` built from the
/// minor-23 Mac commit, run in a sandbox and spoken to over its socket — its
/// `hello_ack`, its `sessions` reply (open cards, model and mode on each run),
/// one `history` page of three events and its `history_end`, the first two
/// events of that run (its `session_start` and a `user_message`), and the
/// `history_end` it sends for a run it does not know. The behaviour tests below
/// cut every frame they inject from these.
@MainActor
final class HistoryPagesWireTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-23-wire", withExtension: "json"),
            "minor-23-wire.json is not in the test bundle (checked in at "
                + "ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func frame(_ name: String) throws -> ServerMessage {
        try XCTUnwrap(try wire()[name]?.decoded(ServerMessage.self), "\(name) decodes")
    }

    func testTheDaemonSaysItServesPages() throws {
        guard case .helloAck(let ack) = try frame("hello_ack") else {
            return XCTFail("not a hello_ack")
        }
        XCTAssertTrue(ack.capabilities.historyPages)
        let profile = DaemonProfile(
            protocolVersion: ack.protocolVersion, protocolMinor: ack.protocolMinor,
            capabilities: ack.capabilities)
        XCTAssertTrue(profile.pagesHistory)
        // The flag alone is not enough, and neither is the minor alone.
        XCTAssertFalse(
            DaemonProfile(protocolVersion: 1, protocolMinor: 22, capabilities: ack.capabilities)
                .pagesHistory)
        XCTAssertFalse(
            DaemonProfile(protocolVersion: 1, protocolMinor: 23, capabilities: Capabilities())
                .pagesHistory)
    }

    func testASessionCarriesItsOpenCardsModelAndMode() throws {
        guard case .sessions(let list) = try frame("sessions") else {
            return XCTFail("not a sessions reply")
        }
        let blocked = try XCTUnwrap(list.first { !$0.pending.isEmpty })
        let card = try XCTUnwrap(blocked.pending.first)
        XCTAssertEqual(blocked.blockedOn, [card.requestID], "the same cards as blocked_on")
        XCTAssertGreaterThan(card.seq, 0)
        XCTAssertEqual(card.hold, .held)
        XCTAssertNil(card.turnID, "a Claude card names no turn")
        let model = try XCTUnwrap(blocked.model)
        XCTAssertEqual(model.value, "claude-opus-5[1m]")
        XCTAssertEqual(model.source, .sessionStart)
        XCTAssertEqual(model.seq, 1)
        XCTAssertFalse(model.at.isEmpty)
        let mode = try XCTUnwrap(blocked.permissionMode)
        XCTAssertEqual(mode.value, "default")
        XCTAssertGreaterThan(mode.seq, 0)
        // A run with nothing open and no mode yet omits both.
        let quiet = try XCTUnwrap(list.first { $0.pending.isEmpty && $0.permissionMode == nil })
        XCTAssertTrue(quiet.pending.isEmpty)
    }

    func testAPageEndsWithTheRequestItAnswers() throws {
        let page = try XCTUnwrap(try wire()["page_events"]?.arrayValue).map {
            try XCTUnwrap($0.decoded(ServerMessage.self))
        }
        let seqs = page.compactMap { message -> UInt64? in
            guard case .event(let event) = message else { return nil }
            return event.seq
        }
        guard
            case .historyEnd(let session, let requestID, let before, let first, let error) =
                try frame("history_end")
        else { return XCTFail("not a history_end") }
        XCTAssertEqual(requestID, "b1-page")
        XCTAssertFalse(session.isEmpty)
        XCTAssertNil(error)
        XCTAssertEqual(seqs, Array(first..<before), "oldest first, ending just below before_seq")
    }

    func testAFailedPageSaysWhy() throws {
        guard
            case .historyEnd(_, let requestID, let before, let first, let error) =
                try frame("history_end_error")
        else { return XCTFail("not a history_end") }
        XCTAssertEqual(requestID, "b1-unknown")
        XCTAssertEqual(first, before, "nothing was sent")
        XCTAssertNotNil(error)
    }

    func testHistoryIsAskedForByRequest() throws {
        let data = try JSONEncoder().encode(
            ClientMessage.history(session: "01K1B3XQ8ZC0DE5FGH7JKMNPQR", beforeSeq: 9501, limit: 500,
                requestID: "r-1"))
        let object = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
        XCTAssertEqual(object["type"]?.stringValue, "history")
        XCTAssertEqual(object["session_id"]?.stringValue, "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        XCTAssertEqual(object["before_seq"]?.intValue, 9501)
        XCTAssertEqual(object["limit"]?.intValue, 500)
        XCTAssertEqual(object["request_id"]?.stringValue, "r-1")
        XCTAssertEqual(object.count, 5)
    }
}

/// What every frame this phone sent said, in order.
@MainActor
private final class Outbox {
    var messages: [ClientMessage] = []

    func subscribes(_ key: String) -> [UInt64] {
        messages.compactMap {
            guard case .subscribe(let session, let afterSeq) = $0, session == key else { return nil }
            return afterSeq
        }
    }

    func pages(_ key: String) -> [(before: UInt64, limit: UInt32, id: String)] {
        messages.compactMap {
            guard case .history(let session, let before, let limit, let id) = $0, session == key
            else { return nil }
            return (before, limit, id)
        }
    }
}

/// The phone's window on a run's log against a Mac that serves pages: the newest
/// 500 at launch and on return, earlier pages one at a time, and a window with
/// nothing in it to show paged back until it has.
@MainActor
final class HistoryPagesTests: XCTestCase {

    private let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"
    private let outbox = Outbox()

    /// The captured minor-23 `hello_ack`'s capabilities, so the model is set up
    /// against what a daemon really advertises.
    private func daemonCapabilities() throws -> Capabilities {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-23-wire", withExtension: "json"))
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        guard case .helloAck(let ack)? = wire["hello_ack"]?.decoded(ServerMessage.self) else {
            throw XCTSkip("the fixture carries no hello_ack")
        }
        return ack.capabilities
    }

    private func cacheRoot() -> URL {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("pages-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return root
    }

    private func pagingModel(root: URL? = nil) throws -> AppModel {
        let model = AppModel(cache: EventCache(root: root ?? cacheRoot()))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(try daemonCapabilities(), minor: 23)
        let outbox = self.outbox
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { outbox.messages.append(message) }
        }
        return model
    }

    /// The captured frames every frame below is cut from. Only the run, the seqs
    /// and the request ids are changed; the shapes are the daemon's own.
    private func captured(_ name: String) throws -> JSONValue {
        try XCTUnwrap(Self.capture?[name], "\(name) is in the capture")
    }

    /// Read once: the tests cut thousands of frames from it.
    private static let capture: [String: JSONValue]? = {
        guard
            let url = Bundle(for: HistoryPagesTests.self)
                .url(forResource: "minor-23-wire", withExtension: "json"),
            let data = try? Data(contentsOf: url)
        else { return nil }
        return (try? JSONDecoder().decode(JSONValue.self, from: data))?.objectValue
    }()

    private func setting(_ value: JSONValue, _ changes: [String: JSONValue]) throws -> JSONValue {
        var object = try XCTUnwrap(value.objectValue)
        for (key, change) in changes { object[key] = change }
        return .object(object)
    }

    private func message(_ value: JSONValue) throws -> ServerMessage {
        try XCTUnwrap(value.decoded(ServerMessage.self))
    }

    /// This run, as the captured list carries a run with nothing open, with
    /// `changes` made to its summary.
    private func sessions(lastSeq: UInt64, _ changes: [String: JSONValue] = [:]) throws -> ServerMessage {
        let list = try XCTUnwrap(captured("sessions").objectValue?["sessions"]?.arrayValue)
        let quiet = try XCTUnwrap(list.first { $0.objectValue?["pending"] == nil })
        let run = try setting(
            quiet,
            ["session_uid": .string(uid), "last_seq": .int(Int64(lastSeq))].merging(changes) { $1 })
        return try message(setting(captured("sessions"), ["sessions": .array([run])]))
    }

    /// A summary fact as the captured list carries one, saying `value` as of `seq`.
    private func fact(_ name: String, _ value: String, seq: UInt64, _ changes: [String: JSONValue] = [:])
        throws -> JSONValue
    {
        let list = try XCTUnwrap(captured("sessions").objectValue?["sessions"]?.arrayValue)
        let template = try XCTUnwrap(list.compactMap { $0.objectValue?[name] }.first { $0 != .null })
        return try setting(
            template, ["value": .string(value), "seq": .int(Int64(seq))].merging(changes) { $1 })
    }

    /// Claude's permission-mode transcript line, as the tailer files it.
    private func modeLine(_ seq: UInt64, _ mode: String) throws -> ServerMessage {
        let frame = try eventFrame(seq)
        let event = try setting(
            XCTUnwrap(frame.objectValue?["event"]),
            [
                "kind": .string("transcript_permission-mode"), "source": .string("transcript"),
                "payload": .object(["permissionMode": .string(mode)]),
            ])
        return try message(setting(frame, ["event": event]))
    }

    /// The captured run's `user_message` is something the fleet counts as
    /// activity; its `session_start` is not.
    private func eventFrame(_ seq: UInt64, substantive: Bool = true) throws -> JSONValue {
        let head = try XCTUnwrap(captured("head_events").arrayValue)
        let kind = substantive ? "user_message" : "session_start"
        let frame = try XCTUnwrap(
            head.first { $0.objectValue?["event"]?.objectValue?["kind"]?.stringValue == kind })
        let event = try setting(
            XCTUnwrap(frame.objectValue?["event"]),
            ["seq": .int(Int64(seq)), "session_uid": .string(uid)])
        return try setting(frame, ["event": event])
    }

    private func event(_ seq: UInt64, substantive: Bool = true) throws -> ServerMessage {
        try message(eventFrame(seq, substantive: substantive))
    }

    private func historyEnd(
        _ id: String, before: UInt64, first: UInt64, error: String? = nil
    ) throws -> ServerMessage {
        var changes: [String: JSONValue] = [
            "session_id": .string(uid), "request_id": .string(id),
            "before_seq": .int(Int64(before)), "first_seq": .int(Int64(first)),
        ]
        if let error { changes["error"] = .string(error) }
        return try message(
            setting(captured(error == nil ? "history_end" : "history_end_error"), changes))
    }

    /// Waits, boundedly, for the `n`th subscription to this run, which goes out
    /// after a cache read off the main actor.
    private func subscription(_ n: Int = 1) async throws -> UInt64 {
        for _ in 0..<300 where outbox.subscribes(uid).count < n {
            try? await Task.sleep(for: .milliseconds(10))
        }
        let all = outbox.subscribes(uid)
        XCTAssertEqual(all.count, n, "subscriptions so far: \(all)")
        return try XCTUnwrap(all.last)
    }

    /// Lets the coalesced rebuild and any send it causes run.
    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    // MARK: The window at launch

    func testALaunchFetchesAtMostTheNewest500() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 10_000))
        let after = try await subscription()
        XCTAssertEqual(after, 9_500, "the newest 500, not the old 400-event backfill or more")
        XCTAssertEqual(model.statesForTesting[uid]?.headTruncated, true)
    }

    // MARK: A cache that does not reach the window

    /// A cache as a uid-keyed daemon's phone holds it, so a later handshake
    /// with that daemon has nothing to sweep.
    private func cached(_ seqs: ClosedRange<UInt64>, root: URL) async throws {
        let events = try seqs.map {
            try XCTUnwrap(eventFrame($0).objectValue?["event"]?.decoded(Event.self))
        }
        let cache = EventCache(root: root)
        await cache.adopt(keying: .uid)
        await cache.saveEvents(events, key: uid)
    }

    /// Held events are kept only if they reach all the way down to the window's
    /// first seq: 9,601…10,000 does not cover 9,501, so it is all dropped and the
    /// newest 500 fetched, rather than holding a window with a hole in it.
    func testACacheThatDoesNotReachTheWindowIsDropped() async throws {
        let root = cacheRoot()
        try await cached(9_601...10_000, root: root)
        let model = try pagingModel(root: root)
        model.connection.injectForTesting(try sessions(lastSeq: 10_000))
        let after = try await subscription()
        XCTAssertEqual(after, 9_500)
        XCTAssertEqual(model.statesForTesting[uid]?.events.count, 0, "nothing held from the cache")
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 9_501)
    }

    /// A cache that does reach it is kept, trimmed to the window, and the
    /// subscription resumes after its tail.
    func testACacheThatCoversTheWindowIsKeptAndTrimmed() async throws {
        let root = cacheRoot()
        try await cached(9_401...9_990, root: root)
        let model = try pagingModel(root: root)
        model.connection.injectForTesting(try sessions(lastSeq: 10_000))
        let after = try await subscription()
        XCTAssertEqual(after, 9_990)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 9_501)
        XCTAssertEqual(model.statesForTesting[uid]?.events.last?.seq, 9_990)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 9_501)
    }

    // MARK: The cache keeps the newest 500

    /// A file an earlier build wrote, at its old bound of 3,000 events, in the
    /// cache's own on-disk shape.
    private func writeLegacyCache(_ seqs: ClosedRange<UInt64>, root: URL) throws {
        let events = try seqs.map {
            try JSONSerialization.jsonObject(
                with: JSONEncoder().encode(XCTUnwrap(eventFrame($0).objectValue?["event"])))
        }
        let envelope: [String: Any] = [
            "version": 2, "sessionID": uid, "keying": "name", "cachedAt": 0, "events": events,
        ]
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try JSONSerialization.data(withJSONObject: envelope)
            .write(to: root.appendingPathComponent("session-\(uid).json"))
    }

    private func eventsOnDisk(root: URL) throws -> Int {
        let data = try Data(contentsOf: root.appendingPathComponent("session-\(uid).json"))
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        return try XCTUnwrap(object["events"] as? [Any]).count
    }

    func testAThreeThousandEventCacheLoadsAsTheNewest500() async throws {
        let root = cacheRoot()
        try writeLegacyCache(1...3_000, root: root)
        let read = await EventCache(root: root).loadEvents(key: uid)
        let loaded = try XCTUnwrap(read)
        XCTAssertEqual(loaded.events.count, 500)
        XCTAssertEqual(loaded.events.first?.seq, 2_501)
        XCTAssertEqual(loaded.lastSeq, 3_000)
    }

    func testTheCacheSavesAtMost500() async throws {
        let root = cacheRoot()
        let events = try (UInt64(1)...3_000).map {
            try XCTUnwrap(eventFrame($0).objectValue?["event"]?.decoded(Event.self))
        }
        await EventCache(root: root).saveEvents(events, key: uid)
        XCTAssertEqual(try eventsOnDisk(root: root), 500)
    }

    // MARK: Earlier pages

    /// A model holding `lastSeq - 499 ... lastSeq` of a run, every one of them
    /// something the fleet counts, so nothing pages back on its own.
    private func windowedModel(lastSeq: UInt64) async throws -> AppModel {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: lastSeq))
        let after = try await subscription()
        for seq in (after + 1)...lastSeq { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        return model
    }

    func testAPageCommitsOnceOnItsEnd() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertEqual(state.windowStart, 501)

        model.loadEarlier(key: uid)
        model.loadEarlier(key: uid)
        await settle(model)
        let pages = outbox.pages(uid)
        XCTAssertEqual(pages.count, 1, "one request in flight per session")
        let page = try XCTUnwrap(pages.first)
        XCTAssertEqual(page.before, 501)
        XCTAssertEqual(page.limit, 500)
        XCTAssertTrue(model.isLoadingEarlier(uid))

        let rebuilds = state.rebuildCount
        for seq in UInt64(1)...500 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        XCTAssertEqual(state.events.first?.seq, 501, "a page waits for its end")
        XCTAssertEqual(state.rebuildCount, rebuilds, "and draws nothing until then")

        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(state.events.map(\.seq), Array(UInt64(1)...1_000))
        XCTAssertEqual(state.rebuildCount, rebuilds + 1, "merged and drawn once")
        XCTAssertEqual(state.windowStart, 1)
        XCTAssertFalse(state.headTruncated)
        XCTAssertFalse(model.isLoadingEarlier(uid))
    }

    func testOnlyTheRequestedPageGetsBelowTheWindow() async throws {
        let model = try await windowedModel(lastSeq: 2_000)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertEqual(state.windowStart, 1_501)

        // Nothing asked for: an event below the window is a straggler from a
        // replay that started lower, and is not held.
        model.connection.injectForTesting(try event(1_200))
        await settle(model)
        XCTAssertEqual(state.events.first?.seq, 1_501)

        model.loadEarlier(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        // Below the page's own range: still dropped.
        model.connection.injectForTesting(try event(800))
        for seq in UInt64(1_001)...1_500 { model.connection.injectForTesting(try event(seq)) }
        // An end for some other request is not this page's end.
        model.connection.injectForTesting(try historyEnd("stale", before: 1_501, first: 1_001))
        await settle(model)
        XCTAssertEqual(state.events.first?.seq, 1_501, "a stale end commits nothing")
        XCTAssertTrue(model.isLoadingEarlier(uid))

        model.connection.injectForTesting(try historyEnd(page.id, before: 1_501, first: 1_001))
        await settle(model)
        XCTAssertEqual(state.events.map(\.seq), Array(UInt64(1_001)...2_000))
        XCTAssertEqual(state.windowStart, 1_001)
    }

    // MARK: A new connection

    private func capturedHelloAck() throws -> ServerMessage {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-23-wire", withExtension: "json"))
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        return try XCTUnwrap(wire["hello_ack"]?.decoded(ServerMessage.self))
    }

    /// A page asked for on a link that has since been replaced will never end on
    /// this one, so it stops being in flight, and the next ask goes out.
    func testANewConnectionClearsThePageInFlight() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        model.loadEarlier(key: uid)
        await settle(model)
        XCTAssertTrue(model.isLoadingEarlier(uid))
        let generation = model.connection.generation

        model.connection.injectForTesting(try capturedHelloAck())
        XCTAssertNotEqual(model.connection.generation, generation)
        XCTAssertFalse(model.isLoadingEarlier(uid), "nothing will answer the old request")
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription(2)
        await settle(model)

        model.loadEarlier(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 2, "the next request is allowed")
    }

    // MARK: Filling a window with nothing to show

    /// A window of nothing but `usage` has nothing to show, so once it has caught
    /// up it asks for the page below, and stops when a page brings something.
    func testAnEmptyWindowPagesBackUntilItHoldsSomething() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        let after = try await subscription()
        XCTAssertEqual(after, 500)
        for seq in UInt64(501)...900 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertTrue(outbox.pages(uid).isEmpty, "not before the window has caught up")

        for seq in UInt64(901)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first, "asked for without a tap")
        XCTAssertEqual(page.before, 501)

        for seq in UInt64(1)...500 {
            model.connection.injectForTesting(try event(seq, substantive: seq == 300))
        }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1)
    }

    /// And when no page ever brings anything, it stops at the start of the log.
    func testAnEmptyLogPagesBackToItsStart() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_500))
        let after = try await subscription()
        XCTAssertEqual(after, 1_000)
        for seq in UInt64(1_001)...1_500 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        for (before, first) in [(UInt64(1_001), UInt64(501)), (501, 1)] {
            let page = try XCTUnwrap(outbox.pages(uid).last)
            XCTAssertEqual(page.before, before)
            for seq in first..<before {
                model.connection.injectForTesting(try event(seq, substantive: false))
            }
            model.connection.injectForTesting(try historyEnd(page.id, before: before, first: first))
            await settle(model)
        }
        XCTAssertEqual(outbox.pages(uid).count, 2, "nothing below seq 1 to ask for")
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1)
        XCTAssertFalse(model.isLoadingEarlier(uid))
    }

    // MARK: A failed page

    /// A page that fails stops the automatic paging where it is; asking again
    /// is the reader's to do.
    func testAFailedPageStopsTheFillAndAskingAgainRetries() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        for seq in UInt64(501)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        model.connection.injectForTesting(try event(400, substantive: false))
        model.connection.injectForTesting(
            try historyEnd(page.id, before: 501, first: 501, error: "store failed"))
        await settle(model)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertEqual(state.windowStart, 501, "a failed page moves nothing")
        XCTAssertEqual(state.events.first?.seq, 501, "and merges nothing")
        XCTAssertFalse(model.isLoadingEarlier(uid))

        // Later activity redraws the window; the fill stays stopped.
        model.connection.injectForTesting(try event(1_001, substantive: false))
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1, "the fill stopped on the error")

        model.loadEarlier(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 2, "asking again retries")
        XCTAssertEqual(outbox.pages(uid).last?.before, 501)
    }

    // MARK: The launch loader and the fill

    /// The loader also waits for a window with nothing to show to page back to
    /// something, so the fleet is never revealed with an empty row that fills
    /// in a moment later.
    func testTheLoaderWaitsForTheFill() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        for seq in UInt64(501)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertFalse(model.launchSettled, "caught up, but the window has nothing to show")

        let page = try XCTUnwrap(outbox.pages(uid).first)
        for seq in UInt64(1)...500 {
            model.connection.injectForTesting(try event(seq, substantive: seq == 499))
        }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertTrue(model.launchSettled)
    }

    /// A page that fails is the end of the fill, so it is the end of the wait.
    func testAFailedFillEndsTheWait() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        for seq in UInt64(501)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertFalse(model.launchSettled)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        model.connection.injectForTesting(
            try historyEnd(page.id, before: 501, first: 501, error: "store failed"))
        await settle(model)
        XCTAssertTrue(model.launchSettled)
    }

    // MARK: A reconnect while open

    /// A model holding the whole of 1…1,000: the newest 500, then the page below.
    private func scrolledUpModel() async throws -> AppModel {
        let model = try await windowedModel(lastSeq: 1_000)
        model.loadEarlier(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        for seq in UInt64(1)...500 { model.connection.injectForTesting(try event(seq)) }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1)
        return model
    }

    /// A reconnect while the app is open keeps what the reader scrolled up to,
    /// and resumes after its tail.
    func testAShortReconnectKeepsTheEarlierPages() async throws {
        let model = try await scrolledUpModel()
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_010))
        let after = try await subscription(2)
        XCTAssertEqual(after, 1_000)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 1, "the earlier pages are kept")
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1)
    }

    /// One that missed more than a window is a fresh start: the newest 500.
    func testALongReconnectStartsAFreshWindow() async throws {
        let model = try await scrolledUpModel()
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_601))
        let after = try await subscription(2)
        XCTAssertEqual(after, 1_101)
        XCTAssertEqual(model.statesForTesting[uid]?.events.count, 0)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1_102)
    }

    /// A window dropped for a hole is not refilled from the cache that had the
    /// hole, however soon the link comes back.
    func testAReconnectDoesNotRestoreADroppedCache() async throws {
        let root = cacheRoot()
        try await cached(9_601...10_000, root: root)
        let model = try pagingModel(root: root)
        model.connection.injectForTesting(try sessions(lastSeq: 10_000))
        _ = try await subscription()
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 10_000))
        let after = try await subscription(2)
        XCTAssertEqual(after, 9_500)
        XCTAssertEqual(model.statesForTesting[uid]?.events.count, 0)
    }

    // MARK: Returning to the foreground

    private func returnToForeground(
        _ model: AppModel, lastSeq: UInt64, _ changes: [String: JSONValue] = [:]
    ) async throws {
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try sessions(lastSeq: lastSeq, changes))
        await settle(model)
    }

    /// Back in the foreground the chat is the newest 500 again, whatever the
    /// reader had scrolled up to; what is held and still in it is kept.
    func testReturningKeepsWhatIsHeldTrimmedToTheNewest500() async throws {
        let model = try await scrolledUpModel()
        try await returnToForeground(model, lastSeq: 1_010)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertEqual(state.events.first?.seq, 511)
        XCTAssertEqual(state.events.last?.seq, 1_000)
        XCTAssertEqual(state.windowStart, 511)
        XCTAssertEqual(outbox.subscribes(uid).count, 1, "the subscription runs on from 1,000")
    }

    /// A pull-down of Control Center or a notification makes the app inactive
    /// without leaving the foreground, so the reader's place is left alone.
    func testBeingInactiveIsNotAReturn() async throws {
        let model = try await scrolledUpModel()
        model.scenePhaseChanged(to: .inactive)
        model.scenePhaseChanged(to: .active)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try sessions(lastSeq: 1_010))
        await settle(model)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 1)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1)
    }

    /// More than a window behind: nothing held is kept and any page in flight is
    /// forgotten. The link stayed up, so its subscription is still bringing what
    /// followed 1,000: the trim is local, and of that stream only the newest 500
    /// are taken.
    func testReturningAfterMoreThanAWindowStartsAFreshOne() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        model.loadEarlier(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)

        try await returnToForeground(model, lastSeq: 1_600)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertEqual(outbox.subscribes(uid), [500], "nothing is asked again on this link")
        XCTAssertEqual(state.events.count, 0)
        XCTAssertEqual(state.windowStart, 1_101)
        XCTAssertFalse(model.isLoadingEarlier(uid))

        // The forgotten page's end changes nothing.
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(state.windowStart, 1_101)

        for seq in UInt64(1_001)...1_600 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        XCTAssertEqual(state.events.map(\.seq), Array(UInt64(1_101)...1_600))
    }

    // MARK: A stale subscription task

    /// The reconnect's subscription is still reading the cache when the app
    /// comes back to the foreground with the Mac 600 further on. The reset the
    /// foreground makes is the newer decision; the older task must not lay its
    /// own window over it.
    func testAStaleSubscriptionDoesNotUndoANewerReset() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try capturedHelloAck())
        // All in one turn of the main actor: the reconnect's list starts its
        // subscription, which suspends on the cache read before the foreground's
        // list arrives.
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.connection.injectForTesting(try sessions(lastSeq: 1_600))
        await settle(model)
        for _ in 0..<50 { await Task.yield() }

        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1_101, "the foreground's window stands")
        XCTAssertEqual(outbox.subscribes(uid), [500, 1_100], "and nothing re-asks from 500")
    }

    /// When that newer reset kept what was held, the reconnect still owes this
    /// connection its subscription, from the kept tail.
    func testAStaleSubscriptionStillSubscribesAfterAKeepingReset() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.connection.injectForTesting(try sessions(lastSeq: 1_010))
        await settle(model)
        for _ in 0..<50 { await Task.yield() }

        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 510 + 1)
        XCTAssertEqual(outbox.subscribes(uid), [500, 1_000])
    }

    // MARK: A stale task after its cache clear

    /// Task A, on one connection, finds the log rewound and is clearing the
    /// cache when the link is replaced; task B starts on the new one. A must not
    /// touch the window once its connection is gone, or B reads A's change as a
    /// newer window and never subscribes.
    func testAStaleRewindDoesNotStopTheNewSubscription() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 900))
        // A resets the rewound log just before it awaits the cache clear.
        for _ in 0..<2_000 where state.gap?.cause != .logRewound { await Task.yield() }
        XCTAssertEqual(state.gap?.cause, .logRewound)
        XCTAssertEqual(outbox.subscribes(uid), [500], "A is still at its cache clear")

        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 900))
        await settle(model)
        for _ in 0..<50 { await Task.yield() }
        XCTAssertEqual(outbox.subscribes(uid), [500, 400], "B subscribes on the new link")
        XCTAssertEqual(state.windowStart, 401)
    }

    /// The same race, with the new link's list further on than the one task A
    /// read: the window has to be the new list's newest 500, not A's.
    func testAStaleRewindDoesNotSetTheNewLinksWindow() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 900))
        for _ in 0..<2_000 where state.gap?.cause != .logRewound { await Task.yield() }
        XCTAssertEqual(state.gap?.cause, .logRewound)
        XCTAssertEqual(outbox.subscribes(uid), [500], "A is still at its cache clear")

        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 950))
        await settle(model)
        for _ in 0..<50 { await Task.yield() }
        XCTAssertEqual(outbox.subscribes(uid), [500, 450], "the newest 500 of the new list")
        XCTAssertEqual(state.windowStart, 451)
    }

    // MARK: One subscription per connection

    /// The foreground reset asked from 1,100 and its first events are already in
    /// when the reconnect's stale task stands down: what it holds is what that
    /// ask brought, and asking again would restart the replay.
    func testAStaleTaskDoesNotReaskWhatTheResetAsked() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.connection.injectForTesting(try sessions(lastSeq: 1_600))
        for seq in UInt64(1_101)...1_105 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        for _ in 0..<50 { await Task.yield() }
        XCTAssertEqual(outbox.subscribes(uid), [500, 1_100], "one subscription per connection")
    }

    /// A return to the foreground just after the list, before the replay it
    /// asked for has landed: the window is the one just asked, so nothing is
    /// asked again.
    func testAReturnBeforeTheReplayLandsDoesNotReask() async throws {
        let model = try pagingModel()
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        await settle(model)
        XCTAssertEqual(outbox.subscribes(uid), [500], "one subscription per connection")
    }

    // MARK: Fill after a quiet reconnect

    /// The automatic fill was asked on a link that dropped. Nothing new arrives
    /// after the reconnect, so nothing redraws; the fill still has to be asked
    /// again, on the new link.
    func testAReconnectReissuesTheFill() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        for seq in UInt64(501)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1, "the fill asked once")

        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription(2)
        await settle(model)
        let pages = outbox.pages(uid)
        XCTAssertEqual(pages.count, 2, "asked again on the new link")
        XCTAssertEqual(pages.last?.before, 501)
        XCTAssertTrue(model.isLoadingEarlier(uid))
    }

    // MARK: A window application that changes nothing

    /// "Load earlier" tapped on the new link, before its list arrives, waits for
    /// the link's catch-up: the page belongs above rows not yet on screen.
    /// Asked once it has ended, the page lands.
    func testAPageAskedAfterTheHandshakeWaitsForTheCatchUp() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        model.connection.injectForTesting(try capturedHelloAck())
        model.loadEarlier(key: uid)
        XCTAssertTrue(outbox.pages(uid).isEmpty, "not while catching up")
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription(2)
        await settle(model)
        model.loadEarlier(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        XCTAssertEqual(outbox.pages(uid).count, 1)
        XCTAssertTrue(model.isLoadingEarlier(uid), "still in flight")

        for seq in UInt64(1)...500 { model.connection.injectForTesting(try event(seq)) }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1, "and it lands")
    }

    /// A return to the foreground that lands on the same window leaves the fill
    /// in flight alone rather than cancelling it and asking for it again.
    func testAResetToTheSameWindowDoesNotReaskThePage() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        for seq in UInt64(501)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1)

        try await returnToForeground(model, lastSeq: 1_000)
        XCTAssertEqual(outbox.pages(uid).count, 1, "one request for one page")
        XCTAssertTrue(model.isLoadingEarlier(uid))
    }

    // MARK: Codex Stop after a trim

    /// `phone-turn-stream-0.153.4.json`, the frames ccd sends a phone for a Codex
    /// turn: its `compose_result`, a `reasoning` item and a `turn_complete` are
    /// cut, re-pointed only at a turn and a seq. Its run uid is the one used here.
    private func turnStream() throws -> [JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self))
                .url(forResource: "phone-turn-stream-0.153.4", withExtension: "json"))
        let file = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
        return try XCTUnwrap(file.objectValue?["frames"]?.arrayValue)
    }

    private func codexEvent(_ seq: UInt64, turn: String, ending: Bool) throws -> ServerMessage {
        let kind = ending ? "turn_complete" : "reasoning"
        let frame = try XCTUnwrap(
            try turnStream().first {
                $0.objectValue?["event"]?.objectValue?["kind"]?.stringValue == kind
            })
        let event = try setting(
            XCTUnwrap(frame.objectValue?["event"]),
            ["seq": .int(Int64(seq)), "session_uid": .string(uid), "turn_id": .string(turn)])
        return try message(setting(frame, ["event": event]))
    }

    /// The turn a phone's compose was told it started, settled on the controls
    /// the way a real `compose_result` is.
    private func composed(_ model: AppModel) throws -> String {
        let frame = try XCTUnwrap(
            try turnStream().first { $0.objectValue?["type"]?.stringValue == "compose_result" })
        guard case .composeResult(_, _, let result) = try message(frame),
            case .started(let turn) = result
        else { throw XCTSkip("the stream's compose_result is not a start") }
        model.codexControls(for: uid).settleCompose(result, material: "compose-material")
        return turn
    }

    /// Ends `count` turns of its own after `from`, one item and one terminal each.
    private func endedTurns(_ model: AppModel, from: UInt64, count: UInt64) throws {
        for n in 0..<count {
            let turn = "later-\(n)"
            model.connection.injectForTesting(try codexEvent(from + 2 * n, turn: turn, ending: false))
            model.connection.injectForTesting(try codexEvent(from + 2 * n + 1, turn: turn, ending: true))
        }
    }

    /// A composed turn that ended, then 600 events of later turns, then a return
    /// to the foreground: the trim takes away the terminal that said it ended,
    /// and what the compose said about it is no longer a reason to offer Stop.
    func testATrimDoesNotBringBackStopForAnEndedComposedTurn() async throws {
        let model = try pagingModel()
        // Opened when the run was new; everything after arrives live.
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        let t1 = try composed(model)
        model.connection.injectForTesting(try codexEvent(1, turn: t1, ending: false))
        model.connection.injectForTesting(try codexEvent(2, turn: t1, ending: true))
        try endedTurns(model, from: 3, count: 300)
        await settle(model)
        XCTAssertNil(model.runningTurn(for: uid), "T1's terminal is held")

        try await returnToForeground(model, lastSeq: 602)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 103, "T1's events are gone")
        XCTAssertNil(model.runningTurn(for: uid), "Stop is not offered for an ended turn")
    }

    /// A composed turn still running has its newest events in the window, so a
    /// trim leaves it, and Stop, exactly where they were.
    func testATrimKeepsStopForATurnStillRunning() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        try endedTurns(model, from: 1, count: 300)
        let t1 = try composed(model)
        model.connection.injectForTesting(try codexEvent(601, turn: t1, ending: false))
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t1)

        try await returnToForeground(model, lastSeq: 601)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 102, "the trim happened")
        XCTAssertEqual(model.codexControls(for: uid).observedTurns, [t1], "still observed")
        XCTAssertEqual(model.runningTurn(for: uid), t1, "still offered Stop")
    }

    /// Codex files no event for `turn/started`: a turn the phone composed may
    /// have none yet when the app goes away and comes back. A trim took nothing
    /// of it, so it keeps Stop.
    func testATrimKeepsStopForAComposedTurnWithNoEventsYet() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        try endedTurns(model, from: 1, count: 300)
        let t1 = try composed(model)
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t1)

        try await returnToForeground(model, lastSeq: 600)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 101, "the trim happened")
        XCTAssertEqual(model.runningTurn(for: uid), t1, "still offered Stop")
    }

    // MARK: Stop from a trimmed window

    /// The Mac files an `error` for a turn after its terminal. A trim that takes
    /// the terminal and keeps the later event must not make the turn look alive.
    /// (No captured frame of a Codex `error` is in the phone's fixtures; the
    /// stream's `reasoning` item stands in, and the turn tracker reads any
    /// non-terminal event of a turn the same way.)
    func testATrimThatKeepsAnEventAfterTheTerminalDoesNotBringBackStop() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        let t1 = try composed(model)
        for seq in UInt64(1)...600 {
            switch seq {
            case 99, 101:
                model.connection.injectForTesting(try codexEvent(seq, turn: t1, ending: false))
            case 100:
                model.connection.injectForTesting(try codexEvent(seq, turn: t1, ending: true))
            default:
                model.connection.injectForTesting(try event(seq, substantive: false))
            }
        }
        await settle(model)
        XCTAssertNil(model.runningTurn(for: uid), "T1's terminal is held")

        try await returnToForeground(model, lastSeq: 600)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 101, "the terminal is gone, 101 kept")
        XCTAssertNil(model.runningTurn(for: uid), "an ended turn stays ended")
    }

    /// A running composed turn whose only event 600 later events without a turn
    /// push out of the window is still running.
    func testATrimThatTakesARunningTurnsOnlyEventKeepsStop() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        let t1 = try composed(model)
        model.connection.injectForTesting(try codexEvent(1, turn: t1, ending: false))
        for seq in UInt64(2)...601 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t1)

        try await returnToForeground(model, lastSeq: 601)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 102)
        XCTAssertEqual(model.runningTurn(for: uid), t1, "still running after the trim")
    }

    // MARK: A known-ended straggler and another running turn

    /// Ended T1 keeps a later event in the window after its terminal is trimmed,
    /// and the phone has just composed T2, which has filed nothing yet. T1's
    /// leftover must not stand in front of T2: Stop is T2's.
    func testAnEndedTurnsStragglerDoesNotHideStopForTheComposedTurn() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        let t1 = "turn-that-ended"
        for seq in UInt64(1)...600 {
            switch seq {
            case 99, 101:
                model.connection.injectForTesting(try codexEvent(seq, turn: t1, ending: false))
            case 100:
                model.connection.injectForTesting(try codexEvent(seq, turn: t1, ending: true))
            default:
                model.connection.injectForTesting(try event(seq, substantive: false))
            }
        }
        let t2 = try composed(model)
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t2, "before the trim")

        try await returnToForeground(model, lastSeq: 600)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 101)
        XCTAssertEqual(model.runningTurn(for: uid), t2, "and after it")
    }

    // MARK: A terminal that lands below the window

    /// Composed T1 has its item at 1,000 when the app goes away; back with the
    /// Mac at 1,600 the window starts at 1,101, and T1's terminal at 1,001
    /// arrives in the stream below it. Not held, but heard: T1 is over.
    func testATerminalBelowTheWindowStillEndsTheTurn() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        for seq in UInt64(1)...999 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        let t1 = try composed(model)
        model.connection.injectForTesting(try codexEvent(1_000, turn: t1, ending: false))
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t1)

        try await returnToForeground(model, lastSeq: 1_600)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1_101)
        model.connection.injectForTesting(try codexEvent(1_001, turn: t1, ending: true))
        for seq in UInt64(1_002)...1_600 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertEqual(model.statesForTesting[uid]?.events.first?.seq, 1_101)
        XCTAssertNil(model.runningTurn(for: uid), "T1 ended, below the window or not")
    }

    /// The link drops with composed T1 running at 1,000; T1 ends at 1,001 while
    /// the phone is away, and the Mac is at 1,600 when it comes back. The new
    /// subscription starts at the window, 1,101, so the terminal never arrives.
    func testATurnThatEndedInTheGapAResetSkipsDoesNotKeepStop() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        for seq in UInt64(1)...999 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        let t1 = try composed(model)
        model.connection.injectForTesting(try codexEvent(1_000, turn: t1, ending: false))
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t1)

        model.connection.simulatePhaseForTesting(.connecting)
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_600))
        let after = try await subscription(2)
        XCTAssertEqual(after, 1_100, "the Mac sends from the window on")
        for seq in (after + 1)...1_600 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1_101)
        XCTAssertNil(model.runningTurn(for: uid), "T1's terminal fell in the gap; it is not running")
    }

    /// A compose answered on the new link while the subscription still reads
    /// the cache began after the gap, so the gap says nothing about it.
    func testATurnComposedWhileTheWindowIsPreparedKeepsStop() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        for seq in UInt64(1)...1_000 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)

        model.connection.simulatePhaseForTesting(.connecting)
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_600))
        let fresh = try composed(model)
        let after = try await subscription(2)
        XCTAssertEqual(after, 1_100)
        for seq in (after + 1)...1_600 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), fresh, "the new link's compose keeps Stop")
    }

    /// The same gap with T1 still running: its newest events are in the window,
    /// and they keep Stop.
    func testATurnStillRunningAcrossTheGapAResetSkipsKeepsStop() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 0))
        _ = try await subscription()
        for seq in UInt64(1)...999 {
            model.connection.injectForTesting(try event(seq, substantive: false))
        }
        let t1 = try composed(model)
        model.connection.injectForTesting(try codexEvent(1_000, turn: t1, ending: false))
        await settle(model)

        model.connection.simulatePhaseForTesting(.connecting)
        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_600))
        let after = try await subscription(2)
        for seq in (after + 1)...1_600 {
            model.connection.injectForTesting(try codexEvent(seq, turn: t1, ending: false))
        }
        await settle(model)
        XCTAssertEqual(model.runningTurn(for: uid), t1, "T1's own events in the window keep Stop")
    }

    // MARK: A rewound log

    /// A Mac whose log is now shorter than what is held was reset behind the
    /// phone; the fresh start is the newest 500 of the new log, and it pages.
    func testARewoundLogStartsAPagedWindow() async throws {
        let model = try pagingModel()
        for seq in UInt64(1)...1_000 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        model.connection.injectForTesting(try sessions(lastSeq: 900))
        let after = try await subscription()
        XCTAssertEqual(after, 400)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 401)
        XCTAssertEqual(model.statesForTesting[uid]?.gap?.cause, .logRewound)

        for seq in UInt64(401)...900 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        model.loadEarlier(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).first?.before, 401)
    }

    // MARK: Reaching the top, and paging without the reader

    func testReachingTheTopAsksForThePageBelow() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        model.reachedTop(key: uid)
        model.reachedTop(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [501], "one page, below the window")
        XCTAssertEqual(outbox.subscribes(uid).count, 1, "never the whole log")
    }

    /// After a failed page the reader can ask again by reaching the top; nothing
    /// asks on its own until a page lands.
    func testOnlyTheReaderRetriesAFailedPage() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        model.reachedTop(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        model.connection.injectForTesting(
            try historyEnd(page.id, before: 501, first: 501, error: "store failed"))
        await settle(model)

        model.pageBackOnItsOwn(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1, "paging on its own stops at a failure")

        model.reachedTop(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 2, "the reader retries")
        let retry = try XCTUnwrap(outbox.pages(uid).last)
        model.connection.injectForTesting(try historyEnd(retry.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 1)
    }

    /// Before the window has caught up with the Mac's list its newest events are
    /// still arriving, and a short screen says nothing yet.
    func testPagingOnItsOwnWaitsForTheCatchUp() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        let after = try await subscription()
        for seq in (after + 1)...900 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        model.pageBackOnItsOwn(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 0, "still catching up")

        for seq in UInt64(901)...1_000 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        model.pageBackOnItsOwn(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [501])
    }

    /// A Mac that does not serve pages: the top asks for nothing; the head row's
    /// "Load all" is the only way to the rest.
    func testAnOlderMacsTopAsksForNothing() async throws {
        let model = AppModel(cache: EventCache(root: cacheRoot()))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(try daemonCapabilities(), minor: 22)
        let outbox = self.outbox
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { outbox.messages.append(message) }
        }
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription()
        model.reachedTop(key: uid)
        model.pageBackOnItsOwn(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 0)
        XCTAssertEqual(outbox.subscribes(uid).count, 1, "no full replay either")
        XCTAssertNil(model.statesForTesting[uid]?.windowStart)
    }

    /// The screen reads where the reader is while the old timeline is still the
    /// one on show, and is handed the one about to replace it.
    func testAPageCommitShowsTheScreenBothTimelinesBeforePublishing() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        var seen: [(shownIsOnScreen: Bool, shown: Int, next: Int, change: SessionState.WindowChange)] = []
        state.onWindowChange = { [weak state] shown, next, change in
            seen.append((state?.shown.timeline == shown, shown.count, next.count, change))
        }
        model.reachedTop(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        for seq in UInt64(1)...500 { model.connection.injectForTesting(try event(seq)) }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertEqual(seen.count, 1, "told once, as the page commits")
        XCTAssertEqual(seen.first?.shownIsOnScreen, true)
        XCTAssertEqual(seen.first?.shown, 500)
        XCTAssertEqual(seen.first?.next, 1_000)
        XCTAssertEqual(seen.first?.change, .page)
        XCTAssertEqual(state.timeline.count, 1_000)
    }

    /// A return to the foreground that trims the window tells the screen too, so
    /// a reader whose rows are still there keeps them, and one whose are not
    /// goes to the latest.
    func testAReturnThatTrimsTheWindowTellsTheScreen() async throws {
        let model = try await scrolledUpModel()
        let state = try XCTUnwrap(model.statesForTesting[uid])
        var seen: [(shown: Int, next: Int, change: SessionState.WindowChange)] = []
        state.onWindowChange = { shown, next, change in seen.append((shown.count, next.count, change)) }
        try await returnToForeground(model, lastSeq: 1_010)
        XCTAssertTrue(seen.isEmpty, "the screen is told when the catch-up ends")
        for seq in UInt64(1_001)...1_010 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        XCTAssertEqual(seen.count, 1)
        XCTAssertEqual(seen.first?.shown, 1_000)
        XCTAssertEqual(seen.first?.next, 500)
        XCTAssertEqual(seen.first?.change, .reset)
    }

    /// The open chat's top wakes when this turns true: only once the window has
    /// caught up with what the Mac listed, and only while there is earlier
    /// history to ask for.
    func testAWindowIsReadyToPageOnceCaughtUpAndUntilItsStart() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        let after = try await subscription()
        for seq in (after + 1)...900 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        XCTAssertFalse(model.isReadyToPage(uid), "still catching up")

        for seq in UInt64(901)...1_000 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        XCTAssertTrue(model.isReadyToPage(uid))

        model.reachedTop(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).first)
        for seq in UInt64(1)...500 { model.connection.injectForTesting(try event(seq)) }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        XCTAssertFalse(model.isReadyToPage(uid), "nothing before seq 1")
    }

    /// A live event is in a moment before the timeline is rebuilt from it, and
    /// whether the chat fills its screen is read off what is drawn.
    func testAWindowIsReadyToPageOnlyOnceItsCatchUpIsDrawn() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertTrue(model.isReadyToPage(uid))

        model.connection.injectForTesting(try sessions(lastSeq: 1_001))
        model.connection.injectForTesting(try event(1_001))
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(state.lastSeq, 1_001)
        XCTAssertFalse(model.isReadyToPage(uid), "event 1001 is not drawn yet")

        await settle(model)
        XCTAssertTrue(model.isReadyToPage(uid))
    }

    // MARK: The screen

    /// The reader's page is in flight when the link drops, and they have moved
    /// a little way down, still within a screen of the top, so the top's row is
    /// off screen. The link comes back with nothing moving: the screen asks for
    /// the page again. Driven through the real screen, because SwiftUI holds
    /// back what it tells a row that is off screen.
    func testAPageTheLinkDroppedIsAskedForAgainOnceTheLinkIsBack() async throws {
        let model = try await windowedModel(lastSeq: 1_000)
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let host = Host(
            SessionDetailView(route: SessionRoute(key: uid), surfaceForTesting: .constant(.timeline))
                .environment(model),
            scene: scene, size: CGSize(width: 402, height: 874))
        defer { host.dismantle() }
        await render(model)
        let list = try XCTUnwrap(collectionView(in: host.view), "no timeline on screen")
        let top = -list.adjustedContentInset.top
        // Just below the top first, so the top's row is in the list when the
        // reader arrives.
        list.setContentOffset(CGPoint(x: 0, y: top + 20), animated: false)
        await render(model)
        list.setContentOffset(CGPoint(x: 0, y: top), animated: false)
        await render(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [501], "the reader's ask")
        list.setContentOffset(CGPoint(x: 0, y: top + 400), animated: false)
        await render(model)
        XCTAssertTrue(PageAnchor.isNearTop(list))

        model.connection.simulatePhaseForTesting(.connecting)
        await render(model)
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_000))
        _ = try await subscription(2)
        await render(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [501, 501], "asked again on the new link")
    }

    // MARK: Open cards older than the window

    /// An event of this run as a minor-22 Mac sends it (`minor-22-wire.json`,
    /// emitted by ccd's own test), at `seq`, with `changes`.
    private func approvalEvent(_ name: String, seq: UInt64, _ changes: [String: JSONValue] = [:])
        throws -> ServerMessage
    {
        let url = try XCTUnwrap(
            Bundle(for: HistoryPagesTests.self).url(forResource: "minor-22-wire", withExtension: "json"))
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        let event = try setting(
            XCTUnwrap(wire[name]),
            ["seq": .int(Int64(seq)), "session_uid": .string(uid)].merging(changes) { $1 })
        return try message(setting(eventFrame(seq), ["event": event]))
    }

    /// Open cards as the Mac lists them in `pending`.
    private func pending(_ refs: [(id: String, seq: UInt64)], hold: String = "held") -> [String: JSONValue] {
        [
            "pending": .array(
                refs.map {
                    .object([
                        "request_id": .string($0.id), "seq": .int(Int64($0.seq)), "hold": .string(hold),
                    ])
                })
        ]
    }

    /// A window of 501…1,000 whose Mac lists `toolu_allowed` open, its request at
    /// 100, with `held` standing in for any event of the window.
    private func pinnedModel(
        _ refs: [(id: String, seq: UInt64)] = [("toolu_allowed", 100)],
        hold: String = "held", held: ((UInt64) throws -> ServerMessage)? = nil
    ) async throws -> AppModel {
        let model = try pagingModel()
        model.connection.injectForTesting(
            try sessions(lastSeq: 1_000, pending(refs, hold: hold)))
        let after = try await subscription()
        for seq in (after + 1)...1_000 {
            model.connection.injectForTesting(try held?(seq) ?? event(seq))
        }
        await settle(model)
        return model
    }

    /// The card fetch in flight comes back with the captured request.
    private func landPin(_ model: AppModel) async throws {
        let ask = try XCTUnwrap(outbox.pages(uid).last)
        XCTAssertEqual(ask.limit, 1, "a card is fetched alone")
        model.connection.injectForTesting(try approvalEvent("held_request", seq: ask.before - 1))
        model.connection.injectForTesting(try historyEnd(ask.id, before: ask.before, first: ask.before - 1))
        await settle(model)
    }

    func testAnOpenCardOlderThanTheWindowIsFetchedAloneAndReachesTheDeck() async throws {
        let model = try await pinnedModel()
        XCTAssertEqual(outbox.pages(uid).map(\.before), [101], "the card's event, nothing else")
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertFalse(model.isLoadingEarlier(uid), "a card is not earlier history")

        try await landPin(model)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertEqual(model.deck.map(\.card.requestID), ["toolu_allowed"])
        let card = try XCTUnwrap(model.deck.first)
        XCTAssertNotNil(model.shownApproval(sessionKey: uid, id: card.id), "a sheet finds it")
        XCTAssertEqual(model.deck.first?.requestedAt, ISO8601.parse("2026-10-08T00:00:00.000Z"))
        XCTAssertEqual(state.windowStart, 501, "the window is untouched")
        XCTAssertFalse(state.timeline.contains { $0.seq < 501 }, "never drawn in the chat")
    }

    func testAResolutionInTheWindowClosesAPinnedCard() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        let card = try XCTUnwrap(model.deck.first)
        model.connection.injectForTesting(try approvalEvent("answered_allow", seq: 1_001))
        await settle(model)
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertNotNil(
            model.shownApproval(sessionKey: uid, id: card.id)?.outcome, "a sheet shows the outcome")
    }

    /// The Mac drops a card from its list as it records the answer; the answer
    /// arrived first, so a sheet open on the card keeps showing it.
    func testAPinnedCardAnsweredElsewhereKeepsItsOutcomeAfterTheMacStopsListingIt() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        let card = try XCTUnwrap(model.deck.first)
        model.connection.injectForTesting(try approvalEvent("answered_allow", seq: 1_001))
        model.connection.injectForTesting(try sessions(lastSeq: 1_001, pending([])))
        await settle(model)
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertNotNil(model.shownApproval(sessionKey: uid, id: card.id)?.outcome, "the sheet shows the answer")
    }

    /// A new link's list comes before its replay: a card answered at the Mac
    /// while the link was down keeps its answer once the replay brings it.
    func testACardAnsweredWhileTheLinkWasDownKeepsItsOutcome() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        let card = try XCTUnwrap(model.deck.first)
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_001, pending([])))
        let after = try await subscription(2)
        XCTAssertEqual(after, 1_000)
        model.connection.injectForTesting(try approvalEvent("answered_allow", seq: 1_001))
        await settle(model)
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertNotNil(model.shownApproval(sessionKey: uid, id: card.id)?.outcome, "the sheet shows the answer")
    }

    /// An answer already shown stays shown when a fresh window leaves it below.
    func testAnAnsweredCardStaysAnsweredWhenAFreshWindowLeavesItsAnswerBelow() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        let card = try XCTUnwrap(model.deck.first)
        model.connection.injectForTesting(try approvalEvent("answered_allow", seq: 1_001))
        model.connection.injectForTesting(try sessions(lastSeq: 1_001, pending([])))
        await settle(model)
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(try sessions(lastSeq: 1_601, pending([])))
        let after = try await subscription(2)
        XCTAssertEqual(after, 1_101)
        for seq in (after + 1)...1_601 { model.connection.injectForTesting(try event(seq)) }
        await settle(model)
        XCTAssertTrue(model.deck.isEmpty, "not pending again")
        XCTAssertNotNil(model.shownApproval(sessionKey: uid, id: card.id)?.outcome)
    }

    /// A reconnect past the window asks for an old card once, against the new
    /// window.
    func testAReconnectPastTheWindowAsksForACardOnce() async throws {
        let model = try await pinnedModel()
        XCTAssertEqual(outbox.pages(uid).count, 1)
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(
            try sessions(lastSeq: 1_600, pending([("toolu_allowed", 100)])))
        _ = try await subscription(2)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [101, 101], "one ask per link")
    }

    /// A log the Mac reset backs no card a sheet could still act on.
    func testAResetLogBacksNoPinnedCard() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        let card = try XCTUnwrap(model.deck.first)
        model.statesForTesting[uid]?.resetForRewoundLog()
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertNil(model.shownApproval(sessionKey: uid, id: card.id))
    }

    func testACardTheMacNoLongerListsIsGone() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        let card = try XCTUnwrap(model.deck.first)
        model.connection.injectForTesting(try sessions(lastSeq: 1_000, pending([])))
        await settle(model)
        XCTAssertTrue(model.deck.isEmpty)
        XCTAssertNil(model.shownApproval(sessionKey: uid, id: card.id))
    }

    func testAPageThatReachesAPinnedCardDrawsItOnce() async throws {
        let model = try await pinnedModel()
        try await landPin(model)
        model.loadEarlier(key: uid)
        await settle(model)
        let page = try XCTUnwrap(outbox.pages(uid).last)
        XCTAssertEqual(page.before, 501)
        for seq in UInt64(1)...500 {
            model.connection.injectForTesting(
                seq == 100 ? try approvalEvent("held_request", seq: seq) : try event(seq))
        }
        model.connection.injectForTesting(try historyEnd(page.id, before: 501, first: 1))
        await settle(model)
        let state = try XCTUnwrap(model.statesForTesting[uid])
        XCTAssertTrue(state.pins.isEmpty, "the window holds it")
        XCTAssertEqual(model.deck.count, 1)
        XCTAssertEqual(state.timeline.filter { $0.pendingApproval != nil }.count, 1, "drawn once")
    }

    /// A window with nothing to show pages back by itself; an open card still
    /// comes before the page.
    func testTheNextOpenCardComesBeforeAWindowsFill() async throws {
        let model = try await pinnedModel(
            [("toolu_allowed", 100), ("toolu_second", 200)]) { try self.event($0, substantive: false) }
        XCTAssertEqual(outbox.pages(uid).map(\.before), [101])
        try await landPin(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [101, 201], "the second card, not a page")
    }

    func testAPageWaitsForTheCardInFlightAndIsAskedWhenItEnds() async throws {
        let model = try await pinnedModel()
        model.loadEarlier(key: uid)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1, "one request at a time")
        XCTAssertFalse(model.isReadyToPage(uid), "a reader's ask waits for the card")
        try await landPin(model)
        XCTAssertTrue(model.isReadyToPage(uid), "and the screen is woken when it lands")
    }

    func testAFetchThatBringsNoCardIsNotAskedAgain() async throws {
        let model = try await pinnedModel()
        let ask = try XCTUnwrap(outbox.pages(uid).last)
        model.connection.injectForTesting(try historyEnd(ask.id, before: ask.before, first: ask.before))
        await settle(model)
        model.connection.injectForTesting(
            try sessions(lastSeq: 1_000, pending([("toolu_allowed", 100)])))
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1)
        XCTAssertTrue(model.deck.isEmpty)
    }

    /// Another card landing does not make a failed one worth asking again.
    func testAFailedFetchStaysUnaskedWhenAnotherCardLands() async throws {
        let model = try await pinnedModel([("toolu_allowed", 100), ("toolu_second", 200)])
        let first = try XCTUnwrap(outbox.pages(uid).last)
        model.connection.injectForTesting(
            try historyEnd(first.id, before: first.before, first: first.before, error: "store error"))
        await settle(model)
        try await landPin(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [101, 201])
    }

    /// A fetch the Mac could not serve is not repeated on that link, and a new
    /// link asks again.
    func testAFailedFetchIsAskedAgainOnANewLink() async throws {
        let model = try await pinnedModel()
        let ask = try XCTUnwrap(outbox.pages(uid).last)
        model.connection.injectForTesting(
            try historyEnd(ask.id, before: ask.before, first: ask.before, error: "store error"))
        await settle(model)
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.injectForTesting(try capturedHelloAck())
        model.connection.injectForTesting(
            try sessions(lastSeq: 1_000, pending([("toolu_allowed", 100)])))
        _ = try await subscription(2)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).map(\.before), [101, 101])
    }

    func testALinkDropClearsTheFetchAndTheNextListAsksAgain() async throws {
        let model = try await pinnedModel()
        let first = try XCTUnwrap(outbox.pages(uid).last)
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.injectForTesting(try capturedHelloAck())
        // The old link's end, arriving before the new link's list.
        model.connection.injectForTesting(try historyEnd(first.id, before: 101, first: 100))
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 1, "nothing asked from the old link's list")
        model.connection.injectForTesting(
            try sessions(lastSeq: 1_000, pending([("toolu_allowed", 100)])))
        _ = try await subscription(2)
        await settle(model)
        XCTAssertEqual(outbox.pages(uid).count, 2, "asked again on the new link")
        model.connection.injectForTesting(try historyEnd(first.id, before: 101, first: 100))
        await settle(model)
        XCTAssertNotNil(model.statesForTesting[uid]?.pinRequest, "the old link's end is not this one's")
    }

    /// The card's own hold is as old as the card; the Mac's is now.
    func testAPinnedCardsHoldIsTheMacsUntilANewerOneIsHeld() async throws {
        let model = try await pinnedModel(hold: "ended")
        try await landPin(model)
        XCTAssertEqual(model.deck.first?.questionHold, .ended, "the Mac's, not the card's own held")

        model.connection.injectForTesting(
            try approvalEvent(
                "hold_ended", seq: 1_001,
                ["payload": .object(["request_id": .string("toolu_allowed"), "question_hold": .string("held")])]))
        await settle(model)
        XCTAssertEqual(model.deck.first?.questionHold, .held, "a newer hold in the window wins")
    }

    /// Claude's permission notification names the card's prompt; with the card
    /// pinned, the notice must not say none arrived.
    func testAPermissionNotificationForAPinnedCardDoesNotSayNoCardArrived() async throws {
        let notification = { (seq: UInt64) throws -> ServerMessage in
            guard seq == 900 else { return try self.event(seq) }
            let frame = try self.eventFrame(seq)
            let event = try self.setting(
                XCTUnwrap(frame.objectValue?["event"]),
                [
                    "kind": .string("notification"), "source": .string("hook"),
                    "payload": .object([
                        "hook_event_name": .string("Notification"),
                        "notification_type": .string("permission_prompt"),
                        "message": .string("Claude needs your permission to use Bash"),
                        "prompt_id": .string("p1"),
                    ]),
                ])
            return try self.message(self.setting(frame, ["event": event]))
        }
        let model = try await pinnedModel(held: notification)
        func says(_ state: SessionState?) -> Bool {
            state?.timeline.contains {
                if case .notice(let notice) = $0.content { return notice.detail?.contains("No card arrived") == true }
                return false
            } == true
        }
        XCTAssertTrue(says(model.statesForTesting[uid]), "before the card is here, it says so")
        try await landPin(model)
        XCTAssertFalse(says(model.statesForTesting[uid]), "the card is pinned")
    }

    func testAReturnKeepsAHeldOpenCardWithoutFetchingIt() async throws {
        let model = try pagingModel()
        model.connection.injectForTesting(
            try sessions(lastSeq: 1_000, pending([("toolu_allowed", 600)])))
        let after = try await subscription()
        for seq in (after + 1)...1_000 {
            model.connection.injectForTesting(
                seq == 600 ? try approvalEvent("held_request", seq: seq) : try event(seq))
        }
        await settle(model)
        XCTAssertEqual(model.deck.count, 1)

        try await returnToForeground(model, lastSeq: 1_200, pending([("toolu_allowed", 600)]))
        XCTAssertEqual(model.statesForTesting[uid]?.windowStart, 701)
        XCTAssertEqual(model.deck.map(\.card.requestID), ["toolu_allowed"], "never gone")
        XCTAssertTrue(outbox.pages(uid).isEmpty, "nothing fetched")
    }

    // MARK: Facts older than the window

    /// A window of 500 whose events say nothing about the mode or the model,
    /// with the summary carrying `changes`.
    private func factsModel(_ changes: [String: JSONValue], held: (UInt64) throws -> ServerMessage)
        async throws -> AppModel
    {
        let model = try pagingModel()
        model.connection.injectForTesting(try sessions(lastSeq: 1_000, changes))
        let after = try await subscription()
        for seq in (after + 1)...1_000 { model.connection.injectForTesting(try held(seq)) }
        await settle(model)
        return model
    }

    func testTheMacsModeStandsWhenTheLineThatSetItIsOlderThanTheWindow() async throws {
        let model = try await factsModel(
            ["permission_mode": try fact("permission_mode", "bypassPermissions", seq: 3)]) {
                try self.event($0)
            }
        XCTAssertNil(model.statesForTesting[uid]?.permissionMode, "the window holds no mode line")
        XCTAssertEqual(model.permissionMode(for: uid), "bypassPermissions")
    }

    func testANewerModeLineInTheWindowWinsOverTheMacsOlderFact() async throws {
        let model = try await factsModel(
            ["permission_mode": try fact("permission_mode", "bypassPermissions", seq: 3)]) {
                $0 == 900 ? try self.modeLine($0, "default") : try self.event($0)
            }
        XCTAssertEqual(model.permissionMode(for: uid), "default")
    }

    func testTheMacsNewerModeWinsOverAnOlderLineInTheWindow() async throws {
        let model = try await factsModel(
            ["permission_mode": try fact("permission_mode", "bypassPermissions", seq: 950)]) {
                $0 == 600 ? try self.modeLine($0, "default") : try self.event($0)
            }
        XCTAssertEqual(model.statesForTesting[uid]?.permissionMode, "default", "the window's own line")
        XCTAssertEqual(model.permissionMode(for: uid), "bypassPermissions")
    }

    func testTheMacsModelStandsWithWhereItCameFrom() async throws {
        let model = try await factsModel(
            ["model": try fact("model", "claude-opus-5[1m]", seq: 1)]) { try self.event($0) }
        XCTAssertNil(model.statesForTesting[uid]?.lastConfirmedModel, "the window holds no session start")
        let confirmed = try XCTUnwrap(model.confirmedModel(for: uid))
        XCTAssertEqual(confirmed.name, "claude-opus-5[1m]")
        XCTAssertEqual(confirmed.provenance, .sessionStart)
        XCTAssertNotEqual(confirmed.at, .distantPast, "the Mac's time is read")
    }

    func testAModelFromASourceThisBuildDoesNotKnowSaysTheMacReportedIt() async throws {
        let model = try await factsModel(
            ["model": try fact("model", "claude-opus-5[1m]", seq: 1, ["source": .string("a_new_kind")])]) {
                try self.event($0)
            }
        let confirmed = try XCTUnwrap(model.confirmedModel(for: uid))
        XCTAssertEqual(confirmed.provenance, .reported)
        XCTAssertTrue(
            ModelSheet.provenanceLine(confirmed, now: confirmed.at).hasPrefix("Reported by the Mac"))
    }

    /// A window in a test process is never on a display, so nothing drives
    /// SwiftUI's update pass but a flush.
    private func render(_ model: AppModel) async {
        for _ in 0..<4 {
            await settle(model)
            CATransaction.flush()
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    private func collectionView(in view: UIView) -> UICollectionView? {
        if let list = view as? UICollectionView { return list }
        return view.subviews.lazy.compactMap { self.collectionView(in: $0) }.first
    }
}

/// Which row keeps its place on screen while the rows above it change, and what
/// "N new" counts.
@MainActor
final class PageAnchorTests: XCTestCase {
    private let at = Date(timeIntervalSince1970: 1_800_000_000)

    private func message(_ id: String, seq: UInt64 = 1) -> TimelineItem {
        TimelineItem(id: id, seq: seq, date: at, content: .agentMessage(id))
    }

    private func tool(_ id: String, use: String, seq: UInt64 = 1) -> TimelineItem {
        TimelineItem(
            id: id, seq: seq, date: at,
            content: .tool(
                ToolItem(
                    toolUseID: use, name: "Bash", argument: nil, status: .succeeded,
                    durationMS: nil, input: nil, output: nil, isTruncated: false)))
    }

    private func choose(
        _ visible: [(Int, CGFloat)], _ shown: [TimelineItem], _ next: [TimelineItem],
        below: [Int: CGFloat]? = [:]
    ) -> (index: Int, top: CGFloat)? {
        PageAnchor.choose(
            visible: visible.map { (index: $0.0, top: $0.1) }, shown: shown, next: next,
            below: below.map { below in { below[$0] } })
    }

    func testTheFirstRowOnScreenThatSurvivesIsKept() throws {
        let shown = [message("a"), message("b"), message("c")]
        let next = [message("x"), message("y"), message("a"), message("b"), message("c")]
        let kept = try XCTUnwrap(choose([(0, -12), (1, 40)], shown, next))
        XCTAssertEqual(kept.index, 2)
        XCTAssertEqual(kept.top, -12)
    }

    func testARowThatFoldsAwayIsSkippedForTheNextOnScreen() throws {
        let shown = [tool("result-1", use: "u1"), message("b")]
        let next = [tool("call-1", use: "u1"), message("b")]
        let kept = try XCTUnwrap(choose([(0, 0), (1, 60)], shown, next))
        XCTAssertEqual(next[kept.index].id, "b")
        XCTAssertEqual(kept.top, 60)
    }

    func testWithNothingOnScreenSurvivingTheCallThatAbsorbedAResultStandsForIt() throws {
        let shown = [tool("result-1", use: "u1"), tool("result-2", use: "u2"), message("c")]
        let next = [
            message("x"), tool("call-1", use: "u1"), tool("call-2", use: "u2"), message("c"),
        ]
        let kept = try XCTUnwrap(choose([(0, 8), (1, 50)], shown, next))
        XCTAssertEqual(next[kept.index].id, "call-1")
        XCTAssertEqual(kept.top, 8)
    }

    func testFailingThatTheNearestSurvivorBelowTheScreen() throws {
        let shown = [message("gone-1"), message("gone-2"), message("c"), message("d")]
        let next = [message("x"), message("c"), message("d")]
        let kept = try XCTUnwrap(choose([(0, 0), (1, 300)], shown, next, below: [2: 700, 3: 760]))
        XCTAssertEqual(next[kept.index].id, "c")
        XCTAssertEqual(kept.top, 700)
    }

    /// A reset that left nothing on screen standing does not reach below it:
    /// the reader goes to the latest instead.
    func testWithoutBelowNothingOnScreenSurvivingKeepsNothing() {
        let shown = [message("gone-1"), message("c")]
        let next = [message("c"), message("d")]
        XCTAssertNil(choose([(0, 0)], shown, next, below: nil))
    }

    func testNothingSurvivingKeepsNothing() {
        XCTAssertNil(choose([(0, 0)], [message("a")], [message("b")]))
    }

    func testAnEarlierPageAddsNothingToNNew() {
        let old = [message("a", seq: 10), message("b", seq: 11)]
        let prepended = [message("x", seq: 1), message("y", seq: 2)] + old
        XCTAssertEqual(TimelineEnd.rowsAfter(TimelineEnd(old, drawnThrough: 11), in: prepended), 0)
    }

    func testRowsAfterTheOldLastOneAreNew() {
        let old = [message("a", seq: 10), message("b", seq: 11)]
        let next = [message("x", seq: 1)] + old + [message("c", seq: 12)]
        XCTAssertEqual(TimelineEnd.rowsAfter(TimelineEnd(old, drawnThrough: 11), in: next), 1)
    }

    /// The newest row was a tool's result whose call the page brought: it folds
    /// into that call, which sits earlier, and nothing after it is new.
    func testAPageThatFoldsTheOldLastRowAddsNothing() {
        let old = [message("a", seq: 500), tool("result-1", use: "u1", seq: 501)]
        let next = [message("x", seq: 1), tool("call-1", use: "u1", seq: 300), message("a", seq: 500)]
        let end = TimelineEnd(old, drawnThrough: 501)
        XCTAssertEqual(TimelineEnd.rowsAfter(end, in: next), 0)
        XCTAssertEqual(
            TimelineEnd.rowsAfter(end, in: next + [message("c", seq: 502)]), 1,
            "a real tail row in the same update still counts")
    }

    /// The window held a re-attach after its last row, hidden because nothing
    /// before it said the link was down. The page brings the detach, so the
    /// re-attach now shows: an event the reader's window already held, not a
    /// new one.
    func testAnEventThePageGivesContextIsNotNew() {
        let old = [message("a", seq: 501), message("b", seq: 510)]
        let next = [message("x", seq: 499), message("detached", seq: 500)] + old
            + [message("attached", seq: 511)]
        XCTAssertEqual(TimelineEnd.rowsAfter(TimelineEnd(old, drawnThrough: 520), in: next), 0)
    }
}

/// The anchor against a real collection view in a window: a head row, the
/// timeline's rows and the tail anchor, laid out as the list lays them out.
@MainActor
final class PageAnchorLayoutTests: XCTestCase {
    private let at = Date(timeIntervalSince1970: 1_800_000_000)

    /// Reports a deceleration while `moving`, as a fling does.
    private final class List: UICollectionView {
        var moving = false
        override var isDecelerating: Bool { moving || super.isDecelerating }
    }

    private final class Rows: NSObject, UICollectionViewDataSource, UICollectionViewDelegateFlowLayout {
        var heights: [CGFloat] = []

        func collectionView(_ view: UICollectionView, numberOfItemsInSection section: Int) -> Int {
            heights.count
        }

        func collectionView(_ view: UICollectionView, cellForItemAt path: IndexPath) -> UICollectionViewCell {
            view.dequeueReusableCell(withReuseIdentifier: "row", for: path)
        }

        func collectionView(
            _ view: UICollectionView, layout: UICollectionViewLayout, sizeForItemAt path: IndexPath
        ) -> CGSize {
            CGSize(width: view.bounds.width, height: heights[path.item])
        }
    }

    private static let head: CGFloat = 44
    private static let row: CGFloat = 100

    private var window: UIWindow!
    private var list: List!
    private let rows = Rows()
    private let anchor = PageAnchor()
    private var published: [TimelineItem] = []
    private var asks = 0
    private var reached = 0

    override func setUp() async throws {
        let layout = UICollectionViewFlowLayout()
        layout.minimumLineSpacing = 0
        layout.minimumInteritemSpacing = 0
        window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 800))
        list = List(frame: window.bounds, collectionViewLayout: layout)
        list.contentInsetAdjustmentBehavior = .never
        list.register(UICollectionViewCell.self, forCellWithReuseIdentifier: "row")
        list.dataSource = rows
        list.delegate = rows
        window.addSubview(list)
        window.isHidden = false
        anchor.timeline = { [unowned self] in self.published }
        anchor.pageBackOnItsOwn = { [unowned self] in self.asks += 1 }
        anchor.attach(list)
    }

    override func tearDown() async throws {
        window.isHidden = true
        window = nil
        list = nil
    }

    private func messages(_ range: ClosedRange<Int>) -> [TimelineItem] {
        range.map { TimelineItem(id: "m\($0)", seq: UInt64($0), date: at, content: .agentMessage("m\($0)")) }
    }

    /// Publishes `timeline` and draws it: the head row, its rows, the tail.
    private func draw(_ timeline: [TimelineItem], heights: [String: CGFloat] = [:]) {
        published = timeline
        rows.heights = [Self.head] + timeline.map { heights[$0.id] ?? Self.row } + [1]
        list.reloadData()
        list.layoutIfNeeded()
    }

    /// As the chat does it: the screen is told, then the new timeline is
    /// published, and the list draws it later.
    private func change(
        _ shown: [TimelineItem], to next: [TimelineItem], _ change: SessionState.WindowChange
    ) {
        anchor.willChange(shown: shown, next: next, change: change)
        published = next
    }

    private func turn() async {
        for _ in 0..<10 { await Task.yield() }
    }

    /// The top of the row `id` below the viewport's.
    private func top(of id: String) throws -> CGFloat {
        let index = try XCTUnwrap(published.firstIndex { $0.id == id })
        let frame = try XCTUnwrap(list.layoutAttributesForItem(at: IndexPath(item: 1 + index, section: 0))).frame
        return frame.minY - list.contentOffset.y
    }

    /// The head row's probe, in the list as the head row's background is. Its
    /// first reading is taken here.
    @discardableResult
    private func probe() -> HeadObserver.Probe {
        let probe = HeadObserver.Probe(anchor: anchor)
        probe.reachedTop = { [unowned self] in self.reached += 1 }
        probe.pageBackOnItsOwn = { [unowned self] in self.asks += 1 }
        list.addSubview(probe)
        return probe
    }

    /// A chat a little taller than its screen, the reader at its latest: the
    /// top is within a screen of them.
    private func atTheLatestOfAChatJustTallerThanItsScreen() {
        draw(messages(992...999))
        list.contentOffset.y = list.contentSize.height - list.bounds.height
        list.layoutIfNeeded()
        XCTAssertTrue(PageAnchor.isNearTop(list))
        XCTAssertFalse(PageAnchor.isShort(list))
    }

    /// The reader rests on the second row of the window, near its top.
    private func readingNearTheTop() throws -> [TimelineItem] {
        let shown = messages(500...999)
        draw(shown)
        list.contentOffset.y = Self.head + Self.row + 10
        list.layoutIfNeeded()
        XCTAssertEqual(try top(of: "m501"), -10)
        return shown
    }

    func testAPageKeepsTheRowOnScreenWhereItWas() async throws {
        let shown = try readingNearTheTop()
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        draw(next)
        XCTAssertEqual(try top(of: "m501"), -10)
    }

    /// A tail row published after the page and drawn in the same pass: the row
    /// is found by what it is, not by counting from the timeline it was given.
    func testATailRowDrawnWithThePageDoesNotShiftTheRowKept() async throws {
        let shown = try readingNearTheTop()
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        draw(next + messages(1_000...1_000))
        XCTAssertEqual(try top(of: "m501"), -10)
    }

    /// A page that leaves the reader well below the top asks for nothing more,
    /// however soon the main actor turns: before the page is drawn the offset
    /// still reads as the top.
    func testAPageThatMovesTheReaderDownAsksForNoMore() async throws {
        let shown = try readingNearTheTop()
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        await turn()
        draw(next)
        await turn()
        XCTAssertEqual(asks, 0)
    }

    /// One that adds too little to leave the top asks for the next, once drawn.
    func testAPageThatLeavesTheReaderAtTheTopAsksForTheNext() async throws {
        let shown = try readingNearTheTop()
        let next = messages(498...499) + shown
        change(shown, to: next, .page)
        draw(next)
        await turn()
        XCTAssertEqual(asks, 1)
        XCTAssertEqual(try top(of: "m501"), -10)
    }

    /// A page that adds no rows draws nothing, and a reader at the top gets the
    /// next one.
    func testAPageThatAddsNoRowsAsksForTheNext() async throws {
        let shown = try readingNearTheTop()
        change(shown, to: shown, .page)
        await turn()
        XCTAssertEqual(asks, 1)
    }

    /// Rows above settle to their real heights after the page is placed, during
    /// a fling as at rest: only what they grew by is given back, so the row
    /// stays and the fling keeps its own motion.
    func testRowsSettlingDuringAFlingDoNotMoveTheRowKept() async throws {
        let shown = try readingNearTheTop()
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        draw(next)
        list.moving = true
        defer { list.moving = false }
        draw(next, heights: ["m499": 160, "m498": 130])
        XCTAssertEqual(try top(of: "m501"), -10)
    }

    /// After the turn the row is let go: a row the reader opens later grows
    /// where it is.
    func testTheRowIsLetGoAfterTheTurn() async throws {
        let shown = try readingNearTheTop()
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        draw(next)
        await turn()
        draw(next, heights: ["m499": 160])
        XCTAssertEqual(try top(of: "m501"), 50)
    }

    /// A result the reader opened folded into the call the page brought, which
    /// is shorter: the call is put at the top of the screen, never above it.
    func testAFoldedRowIsNeverLeftAboveTheScreen() async throws {
        let result = TimelineItem(
            id: "result-1", seq: 600, date: at,
            content: .tool(
                ToolItem(
                    toolUseID: "u1", name: "Bash", argument: nil, status: .succeeded,
                    durationMS: nil, input: nil, output: "out", isTruncated: false)))
        let call = TimelineItem(
            id: "call-1", seq: 300, date: at,
            content: .tool(
                ToolItem(
                    toolUseID: "u1", name: "Bash", argument: nil, status: .succeeded,
                    durationMS: nil, input: nil, output: "out", isTruncated: false)))
        let shown = [result] + messages(601...700)
        draw(shown, heights: ["result-1": 3_000])
        list.contentOffset.y = Self.head + 1_500
        list.layoutIfNeeded()
        let next = messages(0...299) + [call] + messages(601...700)
        change(shown, to: next, .page)
        draw(next)
        XCTAssertEqual(try top(of: "call-1"), 0)
    }

    /// The call was placed by its estimated height, which kept its saved top
    /// partly on screen; measured shorter in the same turn, it would be wholly
    /// above. It goes to the top of the screen instead.
    func testARowMeasuredShorterWhileHeldIsNotLeftAboveTheScreen() async throws {
        let result = TimelineItem(
            id: "result-1", seq: 600, date: at,
            content: .tool(
                ToolItem(
                    toolUseID: "u1", name: "Bash", argument: nil, status: .succeeded,
                    durationMS: nil, input: nil, output: "out", isTruncated: false)))
        let call = TimelineItem(
            id: "call-1", seq: 300, date: at,
            content: .tool(
                ToolItem(
                    toolUseID: "u1", name: "Bash", argument: nil, status: .succeeded,
                    durationMS: nil, input: nil, output: "out", isTruncated: false)))
        let shown = [result] + messages(601...700)
        draw(shown, heights: ["result-1": 1_000])
        list.contentOffset.y = Self.head + 80
        list.layoutIfNeeded()
        XCTAssertEqual(try top(of: "result-1"), -80)
        let next = messages(0...299) + [call] + messages(601...700)
        change(shown, to: next, .page)
        draw(next)
        XCTAssertEqual(try top(of: "call-1"), -80, "placed by the estimate")
        draw(next, heights: ["call-1": 52])
        XCTAssertEqual(try top(of: "call-1"), 0)
    }

    /// The chat became able to page in the same turn as a page committed: the
    /// top is judged once the page is drawn, which moved the reader down.
    func testAWakeWithAPageInTheSameTurnAsksForNoMore() async throws {
        let shown = try readingNearTheTop()
        let next = messages(0...499) + shown
        anchor.recheck()
        change(shown, to: next, .page)
        await turn()
        draw(next)
        await turn()
        XCTAssertEqual(asks, 0)
    }

    /// A reader parked at the top when the chat becomes able to page gets the
    /// page once the list has laid out.
    func testAWakeWithTheReaderAtTheTopAsks() async throws {
        let shown = messages(500...999)
        draw(shown)
        anchor.recheck()
        await turn()
        XCTAssertEqual(asks, 0, "not before the layout")
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(asks, 1)
    }

    /// A return to the app that trims the window and makes it able to page in
    /// one turn: the wake survives the reset.
    func testAWakeIsNotLostToAResetInTheSameTurn() async throws {
        let shown = messages(500...999)
        draw(shown)
        anchor.recheck()
        let next = messages(500...1_010)
        change(shown, to: next, .reset)
        draw(next)
        await turn()
        XCTAssertEqual(asks, 1)
        XCTAssertEqual(try top(of: "m500"), Self.head)
    }

    /// Opened at the latest of a chat a little taller than its screen: the
    /// reader never went to the top, so the chat becoming able to page asks for
    /// nothing, as it would not had it been able to page on opening.
    func testAWakeAtTheLatestOfAChatJustTallerThanItsScreenAsksForNothing() async throws {
        atTheLatestOfAChatJustTallerThanItsScreen()
        let probe = probe()
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(asks + reached, 0)
    }

    /// One who scrolled to its top while it could not page gets the page once
    /// it can.
    func testAWakeAfterTheReaderScrolledToTheTopAsks() async throws {
        atTheLatestOfAChatJustTallerThanItsScreen()
        let probe = probe()
        list.contentOffset.y = 0
        await turn()
        XCTAssertEqual(reached, 1, "the reader's own ask")
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(asks, 1)
    }

    /// One who went to the top and then more than a screen down has left it,
    /// though the screen growing taller under them brings it back within one.
    func testAReaderWhoLeftTheTopIsNotAskedForOnAWake() async throws {
        draw(messages(980...999))
        list.contentOffset.y = list.contentSize.height - list.bounds.height
        list.layoutIfNeeded()
        let probe = probe()
        list.contentOffset.y = 0
        await turn()
        XCTAssertEqual(reached, 1, "the reader's own ask")
        list.contentOffset.y = 1_000
        list.frame.size.height = 1_100
        list.layoutIfNeeded()
        XCTAssertTrue(PageAnchor.isNearTop(list))
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(asks, 0)
    }

    /// The chat opened again starts afresh: an earlier visit to the top is not
    /// the reader's now.
    func testAChatOpenedAgainIsNotAskedForOnAWake() async throws {
        atTheLatestOfAChatJustTallerThanItsScreen()
        let probe = probe()
        list.contentOffset.y = 0
        await turn()
        list.contentOffset.y = list.contentSize.height - list.bounds.height
        probe.removeFromSuperview()
        list.addSubview(probe)
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(asks, 0)
    }

    /// The list moves the offset up as a page's rows go in, before the page is
    /// placed. That is not the reader scrolling, and once placed they are well
    /// below the top.
    func testThePageGoingInIsNotTheReaderScrollingUp() async throws {
        let shown = try readingNearTheTop()
        probe()
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        list.contentOffset.y -= 100
        await turn()
        draw(next)
        await turn()
        XCTAssertEqual(asks + reached, 0)
    }

    /// The same under a hand: a hand near the top is the reader asking, but not
    /// while the page goes in under it.
    func testThePageGoingInUnderAHandIsNotTheReaderReachingTheTop() async throws {
        let shown = try readingNearTheTop()
        probe()
        list.moving = true
        defer { list.moving = false }
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        list.contentOffset.y -= 100
        await turn()
        draw(next)
        await turn()
        XCTAssertEqual(asks + reached, 0)
    }

    /// An ask the reader made just before a page committed is judged after the
    /// page is placed, not before.
    func testAnAskMadeAsAPageCommitsIsJudgedOnceItIsPlaced() async throws {
        let shown = try readingNearTheTop()
        probe()
        list.moving = true
        defer { list.moving = false }
        list.contentOffset.y -= 1
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        await turn()
        draw(next)
        await turn()
        XCTAssertEqual(asks + reached, 0)
    }

    /// A return to the app while the reader was in the newest rows trims the
    /// rows above them, and the content is now shorter than their offset. The
    /// list starts its own animated correction; it must not carry on from the
    /// row put back under the reader.
    func testAResetUnderTheReaderInTheNewestRowsKeepsThePlace() async throws {
        let shown = messages(500...999)
        draw(shown)
        list.contentOffset.y = Self.head + 450 * Self.row + 25
        list.layoutIfNeeded()
        XCTAssertEqual(try top(of: "m950"), -25)
        let next = messages(900...1_010)
        change(shown, to: next, .reset)
        draw(next)
        try await Task.sleep(for: .milliseconds(800))
        XCTAssertEqual(try top(of: "m950"), -25)
    }

    /// Rows settle after a reset's placement, in the same turn: those above
    /// shrink, which the hold gives back, and those below shrink so far that
    /// the content no longer reaches the offset, which starts the list's own
    /// correction. The reader ends at the end of the content, with no gap
    /// below it, and the correction does not carry the give-back on with it.
    func testRowsSettlingAfterAResetEndAtTheEndOfTheContent() async throws {
        let shown = messages(500...999)
        draw(shown)
        list.contentOffset.y = Self.head + 490 * Self.row + 25
        list.layoutIfNeeded()
        XCTAssertEqual(try top(of: "m990"), -25)
        let next = messages(900...1_010)
        change(shown, to: next, .reset)
        draw(next)
        XCTAssertEqual(try top(of: "m990"), -25)
        var settled: [String: CGFloat] = [:]
        for row in 980...987 { settled["m\(row)"] = 50 }
        for row in 991...1_010 { settled["m\(row)"] = 10 }
        draw(next, heights: settled)
        try await Task.sleep(for: .milliseconds(800))
        XCTAssertEqual(list.contentOffset.y, list.contentSize.height - list.bounds.height)
    }

    /// A reset that keeps the rows on screen keeps the reader on them.
    func testAResetThatKeepsTheRowsOnScreenKeepsThePlace() async throws {
        let shown = messages(500...999)
        draw(shown)
        list.contentOffset.y = Self.head + 300 * Self.row + 25
        list.layoutIfNeeded()
        let next = messages(700...1_010)
        change(shown, to: next, .reset)
        draw(next)
        XCTAssertEqual(try top(of: "m800"), -25)
    }

    /// A reader at the latest is left to the tail, which follows the newest
    /// rows: the anchor holds nothing that would stop it.
    func testAResetLeavesAReaderAtTheLatestToTheTail() async throws {
        let shown = messages(500...999)
        draw(shown)
        list.contentOffset.y = list.contentSize.height - list.bounds.height
        list.layoutIfNeeded()
        let offset = list.contentOffset.y
        change(shown, to: messages(510...1_010), .reset)
        draw(messages(510...1_010))
        XCTAssertEqual(list.contentOffset.y, offset)
    }

    /// One that took away everything the reader was looking at goes to the
    /// latest, as opening the chat does.
    func testAResetThatTookTheRowsOnScreenGoesToTheLatest() async throws {
        let shown = try readingNearTheTop()
        let next = messages(700...1_010)
        change(shown, to: next, .reset)
        draw(next)
        XCTAssertEqual(list.contentOffset.y, list.contentSize.height - list.bounds.height)
        await turn()
        XCTAssertEqual(asks, 0, "a reset asks for no page")
    }

    /// The list scrolling itself to the top, as the status bar has it, when a
    /// page lands: the row is put back and the scroll stops there, rather than
    /// carrying the reader on through the page to its top.
    func testAPageLandingAsTheListScrollsItselfToTheTopKeepsTheRow() async throws {
        let shown = messages(500...999)
        draw(shown)
        list.contentOffset.y = Self.head + 5 * Self.row
        list.layoutIfNeeded()
        list.setContentOffset(.zero, animated: true)
        try await Task.sleep(for: .milliseconds(60))
        let next = messages(0...499) + shown
        change(shown, to: next, .page)
        draw(next)
        let placed = list.contentOffset.y
        XCTAssertGreaterThan(placed, 500 * Self.row)
        try await Task.sleep(for: .milliseconds(800))
        XCTAssertEqual(list.contentOffset.y, placed)
    }

    /// A scroll without a hand asks once it reaches the top, not on its way
    /// there: the list moves the offset itself as rows above it change.
    func testAScrollWithoutAHandAsksOnlyAtTheTop() async throws {
        draw(messages(500...999))
        list.contentOffset.y = 3_000
        list.layoutIfNeeded()
        probe()
        list.contentOffset.y = 400
        await turn()
        XCTAssertEqual(reached, 0, "within a screen of the top")
        list.contentOffset.y = 0
        await turn()
        XCTAssertEqual(reached, 1)
    }

    /// The status bar's scroll eases into the top a pixel at a time, as
    /// recorded on an iPhone 17 Pro: the step that arrives asks.
    func testAScrollEasingIntoTheTopAPixelAtATimeAsksOnArriving() async throws {
        draw(messages(500...999))
        list.contentOffset.y = 3_000
        list.layoutIfNeeded()
        probe()
        for y in [2, 5.0 / 3, 4.0 / 3, 1, 2.0 / 3, 1.0 / 3, 0] {
            list.contentOffset.y = y
            await turn()
        }
        XCTAssertEqual(reached, 1)
    }

    /// At the latest of a chat a little taller than its screen, a row above it
    /// goes, as the link's banner does, and the list moves the offset up before
    /// the content shrinks. The reader did not go to the top: nothing is asked,
    /// then or when the chat becomes able to page.
    func testTheListMovingTheOffsetAtTheLatestIsNotTheReaderGoingToTheTop() async throws {
        draw(messages(990...999))
        list.contentOffset.y = list.contentSize.height - list.bounds.height
        list.layoutIfNeeded()
        let probe = probe()
        list.contentOffset.y -= 54
        draw(published, heights: ["m990": 46])
        XCTAssertTrue(PageAnchor.isAtLatest(list))
        await turn()
        XCTAssertEqual(reached + asks, 0)
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(reached + asks, 0)
    }

    /// A reset that puts the reader at the top as it puts their row back is not
    /// the reader arriving there: a wake later asks for nothing.
    func testAResetThatPutsTheReaderAtTheTopIsNotTheirArrival() async throws {
        let shown = messages(990...999)
        draw(shown)
        list.contentOffset.y = 20
        list.layoutIfNeeded()
        let probe = probe()
        let next = messages(991...999)
        change(shown, to: next, .reset)
        draw(next)
        XCTAssertEqual(list.contentOffset.y, 0)
        await turn()
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(reached + asks, 0)
    }

    /// A hand on a short chat is the reader at its top. A page that makes the
    /// chat a little taller than its screen leaves them there, so a wake after
    /// the next page was lost to the link asks again.
    func testAReaderWhoAskedOnAShortChatIsStillAtItsTopOnAWake() async throws {
        let shown = messages(993...999)
        draw(shown)
        XCTAssertTrue(PageAnchor.isShort(list))
        list.moving = true
        let probe = probe()
        await turn()
        XCTAssertEqual(reached, 1, "the reader's own ask")
        list.moving = false
        let next = messages(992...992) + shown
        change(shown, to: next, .page)
        draw(next, heights: ["m992": 60])
        await turn()
        XCTAssertFalse(PageAnchor.isShort(list))
        XCTAssertEqual(asks, 1, "the page's own continuation")
        anchor.wake()
        list.layoutIfNeeded()
        await turn()
        XCTAssertEqual(asks, 2)
    }

    /// A reset under a held row that leaves the chat shorter than its screen:
    /// once the row is let go, the chat asks for the page below on its own.
    func testAResetThatLeavesTheChatShortAsksOnceTheRowIsLetGo() async throws {
        let shown = messages(990...999)
        draw(shown)
        probe()
        let next = messages(993...999)
        change(shown, to: next, .reset)
        draw(next)
        XCTAssertTrue(PageAnchor.isShort(list))
        await turn()
        XCTAssertEqual(asks, 1)
    }
}
