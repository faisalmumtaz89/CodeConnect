import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Several accounts per agent (minor 47) against the daemon's own bytes.**
///
/// `fixtures/accounts/minor-47-wire.json` is what a ccd built at the engine's slice answered one paired device, read
/// from the source file: an empty list, an account added for each agent, Codex's made active, the same label again
/// (the account already made, no twin), a name too long (refused, naming no request), and Default chosen again.
@MainActor
final class AccountsWireTests: XCTestCase {

    // MARK: Harness

    private func wire() throws -> [String: JSONValue] {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let name = "fixtures/accounts/minor-47-wire.json"
        guard let data = try? Data(contentsOf: repoRoot.appendingPathComponent(name)) else {
            throw XCTSkip("\(name) is not reachable from this bundle")
        }
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
    }

    private func step(_ key: String) throws -> (request: JSONValue, reply: JSONValue) {
        let step = try XCTUnwrap(try wire()[key]?.objectValue, "no \(key) in minor-47-wire.json")
        return (try XCTUnwrap(step["request"]), try XCTUnwrap(step["reply"]))
    }

    private func reply(_ key: String) throws -> ServerMessage {
        try JSONDecoder().decode(ServerMessage.self, from: Data(try step(key).reply.canonicalJSONString.utf8))
    }

    private func book(_ key: String) throws -> AccountBook {
        guard case .accounts(_, let book) = try reply(key) else { throw XCTSkip("\(key) is not an accounts reply") }
        return book
    }

    private func encoded(_ message: ClientMessage) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: JSONEncoder().encode(message))
    }

    private final class Sent {
        var sent: [ClientMessage] = []
    }

    /// A model whose computer advertises `accounts` as the fixture's hello did, its frames captured.
    private func model(accounts: Bool = true) throws -> (AppModel, Sent) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("accounts-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            try JSONDecoder().decode(Capabilities.self, from: Data(#"{"accounts": \#(accounts)}"#.utf8)), minor: 47)
        let sent = Sent()
        model.connection.sendStub = { message in MainActor.assumeIsolated { sent.sent.append(message) } }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return (model, sent)
    }

    private func waitUntil(_ done: () -> Bool, _ what: String) async {
        for _ in 0..<5000 where !done() { await Task.yield() }
        XCTAssertTrue(done(), "timed out waiting for \(what)")
    }

    private func requestID(_ message: ClientMessage?) -> String? {
        switch message {
        case .listAccounts(let id)?, .addAccount(let id, _, _)?, .activateAccount(let id, _, _)?: id
        default: nil
        }
    }

    // MARK: The bytes

    /// Every reply decodes as the daemon sent it: the accounts oldest first, the active one per agent, Default as
    /// none.
    func testEveryReplyDecodesAsTheDaemonSentIt() throws {
        XCTAssertEqual(try book("list_empty"), AccountBook(accounts: [], active: []))

        let added = try book("add_codex_work")
        let work = try XCTUnwrap(added.accounts(for: .codex).first)
        XCTAssertEqual(work.label, "Work")
        XCTAssertNil(added.activeID(for: .codex), "adding does not choose it")

        let both = try book("add_claude_personal")
        XCTAssertEqual(both.accounts.map(\.label), ["Work", "Personal"], "oldest first")
        XCTAssertEqual(both.accounts(for: .claude).map(\.label), ["Personal"])

        let chosen = try book("activate_codex_work")
        XCTAssertEqual(chosen.activeID(for: .codex), work.id)
        XCTAssertNil(chosen.activeID(for: .claude), "Claude Code is on its Default")

        XCTAssertEqual(try book("add_codex_work_again"), chosen, "the same label is the account already made")
        XCTAssertNil(try book("activate_codex_default").activeID(for: .codex), "Default again")
        XCTAssertEqual(try book("list_after").accounts, both.accounts, "Default keeps the accounts")

        guard case .error(let code, let message) = try reply("refused_label_too_long") else {
            return XCTFail("the long name is refused")
        }
        XCTAssertEqual(code, "bad_request")
        XCTAssertEqual(message, "an account's name is 1 to 64 characters")
    }

    /// The three requests encode as the device that recorded the fixture sent them.
    func testEachRequestEncodesAsTheDaemonReadIt() throws {
        let book = try self.book("add_codex_work")
        let work = try XCTUnwrap(book.accounts.first).id
        let cases: [(String, ClientMessage)] = [
            ("list_empty", .listAccounts(requestID: "fx-a1")),
            ("add_codex_work", .addAccount(requestID: "fx-a2", agent: "codex", label: "Work")),
            ("activate_codex_work", .activateAccount(requestID: "fx-a4", agent: "codex", accountID: work)),
            ("activate_codex_default", .activateAccount(requestID: "fx-a7", agent: "codex", accountID: nil)),
        ]
        for (key, message) in cases {
            XCTAssertEqual(try encoded(message), try step(key).request, key)
        }
    }

    /// The capability is read from the hello: a computer that names `accounts` keeps them; one that does not, an
    /// older one, keeps none.
    func testTheCapabilityIsTheComputersOwnWord() throws {
        let decode = { (json: String) in try JSONDecoder().decode(Capabilities.self, from: Data(json.utf8)) }
        XCTAssertTrue(try decode(#"{"accounts": true}"#).keepsAccounts)
        XCTAssertFalse(try decode(#"{"accounts": false}"#).keepsAccounts)
        XCTAssertFalse(try decode(#"{"start_session": true}"#).keepsAccounts, "a computer from before minor 47")
        XCTAssertEqual(try wire()["hello_ack_capabilities_accounts"], .bool(true), "the fixture's own hello")
    }

    // MARK: The model

    /// The model asks, and keeps the computer's answer as its book.
    func testTheModelKeepsTheComputersAnswer() async throws {
        let (model, sent) = try model()
        let chosen = try book("activate_codex_work")
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ requestID(sent.sent.last) != nil }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: chosen))
        await loaded
        XCTAssertEqual(model.accountBook, chosen)
        XCTAssertNil(model.accountsProblem)
    }

    /// An older computer is never asked: no frame leaves, and the book stays nil.
    func testAnOlderComputerIsNeverAsked() async throws {
        let (model, sent) = try model(accounts: false)
        XCTAssertFalse(model.keepsAccounts)
        await model.loadAccounts()
        let added = await model.addAccount(agent: .codex, label: "Work")
        XCTAssertFalse(added)
        XCTAssertTrue(sent.sent.isEmpty, "nothing sent: \(sent.sent)")
        XCTAssertNil(model.accountBook)
    }

    /// One accounts request at a time: a second, while the first is in flight, is not sent.
    func testOneRequestAtATime() async throws {
        let (model, sent) = try model()
        async let first: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the first to be sent")
        let second = await model.activateAccount(agent: .codex, accountID: nil)
        XCTAssertFalse(second, "refused while the first is in flight")
        XCTAssertEqual(sent.sent.count, 1)
        model.connection.injectForTesting(
            .accounts(requestID: try XCTUnwrap(requestID(sent.sent.first)), book: try book("list_empty")))
        await first
    }

    /// The computer's refusal names no request: it is the answer to the one in flight, in the computer's own words,
    /// and the book stays as it was.
    func testARefusalAnswersTheRequestInFlight() async throws {
        let (model, sent) = try model()
        let before = try book("activate_codex_work")
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: before))
        await loaded

        async let added = model.addAccount(agent: .codex, label: String(repeating: "x", count: 65))
        await waitUntil({ sent.sent.count == 2 }, "the add to be sent")
        model.connection.injectForTesting(try reply("refused_label_too_long"))
        let wasAdded = await added
        XCTAssertFalse(wasAdded)
        XCTAssertEqual(model.accountsProblem, "an account's name is 1 to 64 characters")
        XCTAssertEqual(model.accountBook, before, "unchanged by a refusal")
        XCTAssertFalse(model.accountsBusy, "free for the next request")
    }
}
