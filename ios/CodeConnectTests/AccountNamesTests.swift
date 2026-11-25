import XCTest

@testable import CodeConnectKit

/// **A switch's rows name an account the way the Accounts page does** (decision 306, ruled 2026-11-23): by its email
/// when this app's account book knows it, held in memory; else by the label the event carries (no book yet, or an
/// account since removed). The email never reaches an event, the store or a cache. A move itself draws no row (decision
/// 311 v4): only a move that could not start, and one onto a plan without the run's model, name the account.
///
/// From the daemon's own recordings: `switch-codex-wire.json`, `switch-failed-then-recovered-claude-wire.json` and
/// `relaunch-plan-default-codex-wire.json` (the label "B"), and `accounts-facts-wire.json` (a listing with facts).
@MainActor
final class AccountNamesTests: XCTestCase {

    private func json(_ resource: String) throws -> JSONValue {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: resource, withExtension: "json"))
        return try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
    }

    private func events(_ resource: String) throws -> [Event] {
        try XCTUnwrap(try json(resource).arrayValue).compactMap { $0["event"] }.map {
            try JSONDecoder().decode(Event.self, from: Data($0.canonicalJSONString.utf8))
        }
    }

    private func titles(_ items: [TimelineItem]) -> [String] {
        items.compactMap { if case .notice(let notice) = $0.content { notice.title } else { nil } }
    }

    // MARK: 1. The rows read a name from the book, else the label

    func testTheRowsNameTheAccountByTheBooksEmailElseByItsLabel() throws {
        let byEmail: (String?) -> String? = { _ in "work@example.test" }
        let switched = try events("switch-codex-wire")
        XCTAssertFalse(
            titles(TimelineBuilder.build(switched, accountName: byEmail)).contains { $0.hasPrefix("Switched to") },
            "a move draws no row")

        let failed = try events("switch-failed-then-recovered-claude-wire")
        XCTAssertTrue(titles(TimelineBuilder.build(failed, accountName: byEmail)).contains("Couldn’t move to work@example.test"))
        XCTAssertTrue(titles(TimelineBuilder.build(failed)).contains("Couldn’t move to B"), "no book: the label")
        XCTAssertTrue(
            titles(TimelineBuilder.build(failed, accountName: { _ in nil })).contains("Couldn’t move to B"),
            "an account the book does not know (removed): the label")

        let plan = try events("relaunch-plan-default-codex-wire")
        XCTAssertTrue(
            titles(TimelineBuilder.build(plan, accountName: byEmail))
                .contains("Now on GPT-6 Luna: GPT-6.1-Sol isn’t in work@example.test’s plan."))
        XCTAssertTrue(
            titles(TimelineBuilder.build(plan)).contains("Now on GPT-6 Luna: GPT-6.1-Sol isn’t in B’s plan."))
    }

    /// The lookup is asked with the event's own account id: the recorded failed move's `switched_to.account_id`.
    func testTheLookupIsAskedWithTheEventsAccountID() throws {
        var asked: [String?] = []
        let failed = try events("switch-failed-then-recovered-claude-wire")
        _ = TimelineBuilder.build(failed, accountName: { asked.append($0); return nil })
        let notice = try XCTUnwrap(failed.first { $0.notificationType == "switch_failed" })
        XCTAssertEqual(asked, [notice.payload["switched_to"]?["account_id"]?.stringValue])
    }

    // MARK: 2, 3. The model: its book answers the lookup, is loaded after a handshake, and redraws on a change

    private final class Sent {
        var sent: [ClientMessage] = []
    }

    private func model() -> (AppModel, Sent, URL) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("names-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            try! JSONDecoder().decode(Capabilities.self, from: Data(#"{"accounts": true}"#.utf8)), minor: 48)
        let sent = Sent()
        model.connection.sendStub = { message in MainActor.assumeIsolated { sent.sent.append(message) } }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return (model, sent, root)
    }

    private func factsBook() throws -> AccountBook {
        let frame = try XCTUnwrap(try json("accounts-facts-wire").arrayValue?.first)
        guard case .accounts(_, let book) = try JSONDecoder().decode(ServerMessage.self, from: Data(frame.canonicalJSONString.utf8))
        else { throw XCTSkip("accounts-facts-wire.json holds no accounts frame") }
        return book
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    private func waitUntil(_ done: () -> Bool, _ what: String) async {
        for _ in 0..<5000 where !done() { await Task.yield() }
        XCTAssertTrue(done(), "timed out waiting for \(what)")
    }

    func testAHandshakeLoadsTheBook() async throws {
        let (model, sent, _) = model()
        model.connection.onConnected?()
        await waitUntil({ sent.sent.contains { if case .listAccounts = $0 { true } else { false } } }, "the list to be asked")
    }

    /// The model's lookup for a run answers from its book: the email of the account the id names, under the run's
    /// agent; nil before the book is in, and for an id the book doesn't list (a removed account).
    func testTheModelAnswersTheLookupFromItsBook() async throws {
        let (model, _, _) = model()
        let book = try factsBook()
        let account = try XCTUnwrap(book.accounts.first)
        let frames = try XCTUnwrap(try json("switch-codex-wire").arrayValue)
        let list = try XCTUnwrap(frames.compactMap { $0["sessions"] }.first)
        model.connection.injectForTesting(.sessions(try JSONDecoder().decode([SessionSummary].self, from: Data(list.canonicalJSONString.utf8))))
        let key = try XCTUnwrap(model.summaries.first?.sessionKey)
        let lookup = try XCTUnwrap(model.statesForTesting[key]).accountName
        XCTAssertNil(lookup(account.id), "no book yet")
        model.connection.injectForTesting(.accounts(requestID: nil, book: book))
        XCTAssertEqual(lookup(account.id), account.facts?.email)
        XCTAssertEqual(lookup(nil), book.defaultFacts(for: .codex)?.email, "nil: the Default")
        XCTAssertNil(lookup("01M8REMOVED0000000000000000"), "an account the book no longer lists")
    }

    /// A change of names redraws a run's rows: the label until the book names the account, then its email.
    func testANewBookRedrawsTheRows() async throws {
        let all = try events("switch-failed-then-recovered-claude-wire")
        let state = SessionState(sessionKey: try XCTUnwrap(all.first).sessionKey)
        for event in all { state.ingest(event) }
        await state.settleForTesting()
        XCTAssertTrue(titles(state.timeline).contains("Couldn’t move to B"))
        state.accountName = { _ in "work@example.test" }
        state.accountNamesChanged()
        XCTAssertTrue(titles(state.timeline).contains("Couldn’t move to work@example.test"))
    }

    // MARK: 4. Memory only

    /// The book's emails reach no file: the fleet cache written after a book is held carries none. (The per-run event
    /// files hold only the daemon's events, which carry no email: no recorded switch, notice or listing puts one in an
    /// event.)
    func testNoEmailFromTheBookReachesTheCache() async throws {
        let (model, _, root) = model()
        let book = try factsBook()
        model.connection.injectForTesting(.accounts(requestID: nil, book: book))
        XCTAssertEqual(model.accountBook, book)
        let frames = try XCTUnwrap(try json("switch-codex-wire").arrayValue)
        for frame in frames {
            if let list = frame["sessions"] {
                model.connection.injectForTesting(.sessions(try JSONDecoder().decode([SessionSummary].self, from: Data(list.canonicalJSONString.utf8))))
            } else if let event = frame["event"] {
                model.connection.injectForTesting(.event(try JSONDecoder().decode(Event.self, from: Data(event.canonicalJSONString.utf8))))
            }
        }
        model.flushCache()
        func written() -> [URL] {
            (FileManager.default.enumerator(at: root, includingPropertiesForKeys: nil)?.allObjects ?? [])
                .compactMap { $0 as? URL }.filter { !$0.hasDirectoryPath }
        }
        await waitUntil({ written().contains { $0.lastPathComponent == "fleet.json" } }, "the fleet to be written")
        let files = written()
        let emails = Set((book.accounts.compactMap(\.facts?.email)) + StartAgent.allCases.compactMap { book.defaultFacts(for: $0)?.email })
        XCTAssertFalse(emails.isEmpty)
        for file in files {
            let text = try String(contentsOf: file, encoding: .utf8)
            for email in emails { XCTAssertFalse(text.contains(email), "\(file.lastPathComponent) holds \(email)") }
            XCTAssertFalse(text.contains("@example.test"), file.lastPathComponent)
        }
    }
}
