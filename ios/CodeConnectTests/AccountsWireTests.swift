import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Several accounts per agent (minor 48) against the daemon's own bytes.**
///
/// `fixtures/accounts/minor-48-wire.json` is what a ccd built at the engine's slice answered one paired device, read
/// from the source file: an empty list, an account added for each agent, Codex's made active, the same label again
/// (the account already made, no twin), a name too long (refused, naming its request), and Default chosen again.
@MainActor
final class AccountsWireTests: XCTestCase {

    // MARK: Harness

    private func wire() throws -> [String: JSONValue] {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let name = "fixtures/accounts/minor-48-wire.json"
        guard let data = try? Data(contentsOf: repoRoot.appendingPathComponent(name)) else {
            throw XCTSkip("\(name) is not reachable from this bundle")
        }
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
    }

    /// Frame `index` of the daemon's recording of three adds answered at once and filled in by pushes (machine's
    /// fixtures/accounts/accounts-answer-then-push-wire.json), decoded as the app reads it.
    private func answerThenPush(_ index: Int) throws -> AccountBook {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let data = try Data(contentsOf: repoRoot.appendingPathComponent("fixtures/accounts/accounts-answer-then-push-wire.json"))
        let frames = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).arrayValue)
        guard case .accounts(_, let book) = try JSONDecoder().decode(
            ServerMessage.self, from: Data(frames[index].canonicalJSONString.utf8))
        else { throw XCTSkip("frame \(index) is not an accounts frame") }
        return book
    }

    private func account(_ label: String, in book: AccountBook) throws -> AgentAccount {
        try XCTUnwrap(book.accounts.first { $0.label == label }, label)
    }

    private func step(_ key: String) throws -> (request: JSONValue, reply: JSONValue) {
        let step = try XCTUnwrap(try wire()[key]?.objectValue, "no \(key) in minor-48-wire.json")
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
            try JSONDecoder().decode(Capabilities.self, from: Data(#"{"accounts": \#(accounts)}"#.utf8)), minor: 48)
        let sent = Sent()
        model.connection.sendStub = { message in MainActor.assumeIsolated { sent.sent.append(message) } }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return (model, sent)
    }

    private func waitUntil(_ done: () -> Bool, _ what: String) async {
        for _ in 0..<5000 where !done() { await Task.yield() }
        XCTAssertTrue(done(), "timed out waiting for \(what)")
    }

    /// The fixture's refusal, in its own code and words, naming `requestID`.
    private func refusal(naming requestID: String) throws -> ServerMessage {
        guard case .accountsRefused(_, let code, let message) = try reply("refused_label_too_long") else {
            throw XCTSkip("refused_label_too_long is not a refusal")
        }
        return .accountsRefused(requestID: requestID, code: code, message: message)
    }

    private func requestID(_ message: ClientMessage?) -> String? {
        switch message {
        case .listAccounts(let id)?, .addAccount(let id, _, _)?, .activateAccount(let id, _, _)?,
            .signInAccount(let id, _)?, .removeAccount(let id, _)?:
            id
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

        guard case .accountsRefused(let requestID, let code, let message) = try reply("refused_label_too_long") else {
            return XCTFail("the long name is refused")
        }
        XCTAssertEqual(requestID, try step("refused_label_too_long").request.objectValue?["request_id"]?.stringValue)
        XCTAssertEqual(code, "bad_request")
        XCTAssertEqual(message, "An account's name is 1 to 64 characters.")
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
        XCTAssertFalse(try decode(#"{"start_session": true}"#).keepsAccounts, "a computer from before minor 48")
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

    /// A refusal answers the request it names, in the computer's own words, and the book stays as it was; one naming
    /// another request leaves this one waiting.
    func testARefusalAnswersTheRequestItNames() async throws {
        let (model, sent) = try model()
        let before = try book("activate_codex_work")
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: before))
        await loaded

        async let added = model.addAccount(agent: .codex, label: String(repeating: "x", count: 65))
        await waitUntil({ sent.sent.count == 2 }, "the add to be sent")
        model.connection.injectForTesting(try refusal(naming: "ac-another"))
        for _ in 0..<200 { await Task.yield() }
        XCTAssertTrue(model.accountsBusy, "still waiting for its own answer")
        XCTAssertNil(model.accountsProblem)
        model.connection.injectForTesting(try refusal(naming: try XCTUnwrap(requestID(sent.sent.last))))
        let wasAdded = await added
        XCTAssertFalse(wasAdded)
        XCTAssertEqual(model.accountsProblem, "An account's name is 1 to 64 characters.")
        XCTAssertEqual(model.accountBook, before, "unchanged by a refusal")
        XCTAssertFalse(model.accountsBusy, "free for the next request")
    }

    /// A refusal is shown until the next accounts request starts, never longer: the page asks again when the link
    /// comes back, so a refusal from before a reconnect is gone as soon as it asks.
    func testARefusalLastsOnlyUntilTheNextRequest() async throws {
        let (model, sent) = try model()
        async let added = model.addAccount(agent: .codex, label: String(repeating: "x", count: 65))
        await waitUntil({ sent.sent.count == 1 }, "the add to be sent")
        model.connection.injectForTesting(try refusal(naming: try XCTUnwrap(requestID(sent.sent.last))))
        _ = await added
        XCTAssertEqual(model.accountsProblem, "An account's name is 1 to 64 characters.")

        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 2 }, "the list to be sent")
        XCTAssertNil(model.accountsProblem, "gone once the next request starts, before its answer")
        model.connection.injectForTesting(
            .accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: try book("list_after")))
        await loaded
        XCTAssertNil(model.accountsProblem)
    }

    // MARK: Signing in and removing (machine/accounts-306, minor 48)

    /// What ccd's pinned wire test (ws.rs, the accounts round trip, at 3db0be74) holds: an account says whether it is
    /// signed in when that is known and whether a sign-in is going; a sign-in and a removal name the account; a pushed
    /// listing names no request.
    private static let pinnedAccountID = "01M80QGTWR9RCHBSJWR169GXCN"

    /// `signed_in` is read when the computer says it, and is not known when it does not.
    func testAnAccountSaysWhetherItIsSignedIn() throws {
        let decode = { (json: String) in try JSONDecoder().decode(AgentAccount.self, from: Data(json.utf8)) }
        let id = Self.pinnedAccountID
        XCTAssertEqual(try decode(#"{"id": "\#(id)", "agent": "codex", "label": "Work", "signed_in": true}"#).signedIn, true)
        XCTAssertEqual(try decode(#"{"id": "\#(id)", "agent": "codex", "label": "Work", "signed_in": false}"#).signedIn, false)
        XCTAssertNil(try decode(#"{"id": "\#(id)", "agent": "codex", "label": "Work"}"#).signedIn, "not known")
        XCTAssertTrue(try decode(#"{"id": "\#(id)", "agent": "codex", "label": "Work", "signing_in": true}"#).signingIn)
        XCTAssertFalse(try decode(#"{"id": "\#(id)", "agent": "codex", "label": "Work"}"#).signingIn, "omitted when false")
    }

    /// A sign-in and a removal name the account, as ccd reads them.
    func testSignInAndRemoveEncodeAsCcdReadsThem() throws {
        let id = Self.pinnedAccountID
        XCTAssertEqual(
            try encoded(.signInAccount(requestID: "a7", accountID: id)),
            .object(["type": .string("sign_in_account"), "request_id": .string("a7"), "account_id": .string(id)]))
        XCTAssertEqual(
            try encoded(.removeAccount(requestID: "a8", accountID: id)),
            .object(["type": .string("remove_account"), "request_id": .string("a8"), "account_id": .string(id)]))
    }

    /// A removal the computer refuses (a session runs on the account) is shown in its words, and nothing changes.
    func testARefusedRemovalIsShownInTheComputersWords() async throws {
        let (model, sent) = try model()
        let before = try book("activate_codex_work")
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: before))
        await loaded

        let work = try XCTUnwrap(before.accounts(for: .codex).first).id
        async let removed = model.removeAccount(accountID: work)
        await waitUntil({ sent.sent.count == 2 }, "the removal to be sent")
        guard case .removeAccount(_, let named)? = sent.sent.last else { return XCTFail("sent \(String(describing: sent.sent.last))") }
        XCTAssertEqual(named, work)
        let words = "Sessions are running on this account. Choose another account first."
        model.connection.injectForTesting(
            .accountsRefused(requestID: try XCTUnwrap(requestID(sent.sent.last)), code: "accounts_in_use", message: words))
        let wasRemoved = await removed
        XCTAssertFalse(wasRemoved)
        XCTAssertEqual(model.accountsProblem, words)
        XCTAssertEqual(model.accountBook, before, "nothing removed")
    }

    /// A sign-in answers at once with the account signing in; its end comes as a listing the computer pushes, naming
    /// no request, which the model keeps as the latest.
    func testASignInAnswersAtOnceAndItsEndIsPushed() async throws {
        let (model, sent) = try model()
        let id = Self.pinnedAccountID
        let account = { (rest: String) in
            #"{"type": "accounts", "accounts": [{"id": "\#(id)", "agent": "codex", "label": "Work"\#(rest)}], "active": []}"#
        }
        let frame = { (json: String) in try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8)) }

        async let started = model.signInAccount(accountID: id)
        await waitUntil({ sent.sent.count == 1 }, "the sign-in to be sent")
        guard case .accounts(_, let signing) = try frame(account(#", "signing_in": true"#)) else { return XCTFail() }
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: signing))
        let wasStarted = await started
        XCTAssertTrue(wasStarted)
        XCTAssertEqual(model.accountBook?.accounts.first?.signingIn, true)
        XCTAssertFalse(model.accountsBusy, "the page is free while the browser waits")

        let pushed = try frame(account(#", "signed_in": true"#))
        guard case .accounts(nil, _) = pushed else { return XCTFail("a pushed listing names no request") }
        model.connection.injectForTesting(pushed)
        await waitUntil({ model.accountBook?.accounts.first?.signedIn == true }, "the pushed listing to be kept")
        XCTAssertEqual(model.accountBook?.accounts.first?.signingIn, false, "signing_in gone reads not signing in")
    }

    // MARK: What each account's agent says of it (machine/accounts-306, minor 48)

    /// `fixtures/accounts/accounts-facts-wire.json`: a listing ccd sent with each Codex account's facts, made-up values:
    /// two accounts signed in as one address (each marked the same as another), and the Default's facts.
    private func factsBook() throws -> AccountBook {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let name = "fixtures/accounts/accounts-facts-wire.json"
        guard let data = try? Data(contentsOf: repoRoot.appendingPathComponent(name)) else {
            throw XCTSkip("\(name) is not reachable from this bundle")
        }
        let frame = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).arrayValue?.first)
        guard case .accounts(nil, let book) = try JSONDecoder().decode(ServerMessage.self, from: Data(frame.canonicalJSONString.utf8))
        else { throw XCTSkip("not a pushed accounts listing") }
        return book
    }

    /// Each account's email, plan and the windows its agent names are read as sent; so are the Default's.
    func testEachAccountsFactsAreReadAsSent() throws {
        let book = try factsBook()
        let first = try XCTUnwrap(book.accounts(for: .codex).first)
        let facts = try XCTUnwrap(first.facts)
        XCTAssertEqual(facts.email, "same@example.test")
        XCTAssertEqual(facts.plan, "plus")
        XCTAssertEqual(facts.windows, [UsageWindow(usedPercent: 42, windowMinutes: 300, resetsAt: 1_800_000_000)])
        XCTAssertEqual(facts.readAt, "2026-11-22T20:43:28.243Z")
        XCTAssertTrue(first.sameAsAnother)
        XCTAssertFalse(first.notSeparate)
        XCTAssertEqual(book.defaultFacts(for: .codex)?.email, "default@example.test")
        XCTAssertNil(book.defaultFacts(for: .claude), "Claude Code's Default has no facts yet")
    }

    /// The facts of the account a run is on: an added account by its id, the Default by none.
    func testARunsAccountFactsAreFoundByItsId() throws {
        let book = try factsBook()
        let first = try XCTUnwrap(book.accounts(for: .codex).first)
        XCTAssertEqual(book.facts(for: .codex, accountID: first.id)?.email, "same@example.test")
        XCTAssertEqual(book.facts(for: .codex, accountID: nil)?.email, "default@example.test")
        XCTAssertNil(book.facts(for: .codex, accountID: "01M8NOSUCHACCOUNT00000000"), "an account since removed")
    }

    /// An account without facts, an older listing's, reads none.
    func testAListingWithoutFactsReadsNone() throws {
        let account = try JSONDecoder().decode(
            AgentAccount.self, from: Data(#"{"id": "\#(Self.pinnedAccountID)", "agent": "codex", "label": "Work"}"#.utf8))
        XCTAssertNil(account.facts)
        XCTAssertFalse(account.sameAsAnother)
        XCTAssertEqual(try book("list_empty").defaults, [])
    }

    // MARK: The words of a switch in progress (design A)

    /// While any session is due to move, one static line: open sessions move with their next message.
    func testASwitchSaysOpenSessionsMoveWithTheirNextMessage() {
        for (waiting, moved) in [(2, 1), (0, 4), (1, 0), (0, 1)] {
            XCTAssertEqual(
                AccountSwitchWords.sentence(to: "ben@example.com", waiting: waiting, moved: moved),
                "Open sessions move to ben@example.com with their next message.", "\(waiting) due, \(moved) moving")
        }
        XCTAssertNil(AccountSwitchWords.sentence(to: "ben@example.com", waiting: 0, moved: 0), "no live session: no sentence")
    }

    /// A session that couldn't switch is named; several are counted.
    func testAFailedMoveIsNamed() {
        XCTAssertEqual(
            AccountSwitchWords.failure(sessions: ["query-parser"]),
            "query-parser couldn't switch accounts. Its conversation says what to do.")
        XCTAssertEqual(
            AccountSwitchWords.failure(sessions: ["query-parser", "docs-site"]),
            "2 sessions couldn't switch accounts. Their conversations say what to do.")
        XCTAssertNil(AccountSwitchWords.failure(sessions: []))
    }

    /// Two accounts signed in as one address, and an account that isn't its own, each say so in one line.
    func testTheRowsQuietLinesSayWhatTheyMean() {
        XCTAssertEqual(AccountSwitchWords.sameSignIn, "The same sign-in as another account here", "never the row's own email again")
        XCTAssertEqual(AccountSwitchWords.notSeparate, "Uses this computer's own Codex sign-in, so it isn't a separate account")
    }

    /// The page's switch reads the sessions' own `switching`, listing by listing, across the daemon's recorded move:
    /// moving while the person's message ends the run's process and starts the next, gone once that one is linked.
    func testTheSwitchIsCountedFromTheSessionsOwnWord() async throws {
        let (model, _) = try model()
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: "switch-codex-wire", withExtension: "json"))
        let frames = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).arrayValue)
        func listing(_ index: Int) throws -> ServerMessage {
            try JSONDecoder().decode(ServerMessage.self, from: Data(frames[index].canonicalJSONString.utf8))
        }
        model.connection.injectForTesting(try listing(1))
        await waitUntil({ model.accountSwitch(for: .codex) != nil }, "the moving listing to be read")
        XCTAssertEqual(model.accountSwitch(for: .codex), AccountSwitch(target: "Default", waiting: 0, moved: 1, failed: []))
        XCTAssertEqual(model.accountSwitch(for: .codex)?.sentence, "Open sessions move to Default with their next message.")
        model.connection.injectForTesting(try listing(5))
        await waitUntil({ model.accountSwitch(for: .codex) == nil }, "the move to end")
        XCTAssertNil(model.accountSwitch(for: .claude), "another agent's sessions aren't counted")
    }

    /// An Antigravity run moving to another account is counted for Antigravity, as Claude Code's and Codex's are (minor
    /// 57), from the daemon's recorded move, and for no other agent.
    func testAnAntigravityRunsMoveIsCountedForAntigravity() async throws {
        let (model, _) = try model()
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: "switch-antigravity-wire", withExtension: "json"))
        let frames = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).arrayValue)
        model.connection.injectForTesting(
            try JSONDecoder().decode(ServerMessage.self, from: Data(frames[1].canonicalJSONString.utf8)))
        await waitUntil({ model.accountSwitch(for: .antigravity) != nil }, "the moving listing to be read")
        XCTAssertEqual(model.accountSwitch(for: .antigravity)?.moved, 1)
        XCTAssertNil(model.accountSwitch(for: .codex), "another agent's sessions aren't counted")
    }

    /// A failed move is about the session: its line names the session and points to its conversation, never an account.
    /// The account chosen since (here B, after the daemon's recorded failure) is not the one it failed on.
    func testAFailedMoveNamesNoAccount() async throws {
        let (model, _) = try model()
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "switch-failed-then-recovered-claude-wire", withExtension: "json"))
        let frames = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).arrayValue)
        let book = #"{"type": "accounts", "accounts": [{"agent": "claude", "id": "B1", "label": "B", "signed_in": true, "facts": {"email": "b@example.test", "read_at": "2026-11-23T03:00:00Z"}}], "active": [{"agent": "claude", "account_id": "B1"}], "defaults": []}"#
        model.connection.injectForTesting(try JSONDecoder().decode(ServerMessage.self, from: Data(book.utf8)))
        model.connection.injectForTesting(
            try JSONDecoder().decode(ServerMessage.self, from: Data(frames[5].canonicalJSONString.utf8)))
        await waitUntil({ model.accountSwitch(for: .claude)?.failure != nil }, "the failed listing to be read")
        XCTAssertEqual(
            model.accountSwitch(for: .claude)?.failure,
            "cch2.3J9739PW4Y couldn't switch accounts. Its conversation says what to do.")
    }

    /// A failed move's line goes once a retry starts its session: listings of it starting and then live, the failed
    /// mark not yet cleared (the daemon's recording carries both), say nothing failed.
    func testAFailedMoveIsNotShownOnceItsRetryStarts() async throws {
        let (model, _) = try model()
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "switch-failed-then-recovered-claude-wire", withExtension: "json"))
        let frames = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).arrayValue)
        func listing(_ index: Int) throws -> ServerMessage {
            try JSONDecoder().decode(ServerMessage.self, from: Data(frames[index].canonicalJSONString.utf8))
        }
        model.connection.injectForTesting(try listing(5))
        await waitUntil({ model.accountSwitch(for: .claude)?.failure != nil }, "the failed listing to be read")
        for index in [7, 9] {
            model.connection.injectForTesting(try listing(index))
            await waitUntil({ model.accountSwitch(for: .claude)?.failure == nil }, "listing \(index) to be read")
            XCTAssertNil(model.accountSwitch(for: .claude)?.failure, "listing \(index)")
        }
    }

    /// An account signing in is not signed out: the service says `signed_in: false` with `signing_in: true` while the
    /// person is in the browser (terminal's probe, push 3 of one Add), and the row says the sign-in, not "Signed out".
    /// Signed out is false only, never unread, never not separate.
    func testAnAccountSigningInIsNotSignedOut() throws {
        XCTAssertFalse(try account("C", in: answerThenPush(9)).signedOut, "signed_in false while signing in")
        XCTAssertTrue(try account("C", in: answerThenPush(9)).signingIn)
        XCTAssertTrue(try account("C", in: answerThenPush(8)).signedOut, "signed_in false, no sign-in going")
        XCTAssertFalse(try account("B", in: answerThenPush(1)).signedOut, "not read yet")
        let notSeparate = try JSONDecoder().decode(
            AgentAccount.self,
            from: Data(#"{"id": "C8", "agent": "codex", "label": "Codex 8", "signed_in": false, "not_separate": true}"#.utf8))
        XCTAssertFalse(notSeparate.signedOut)
    }

    /// A row's "…" offers what can be done to that account and nothing else: Sign In Again… where it is signed out, and
    /// Remove… for every account added here. The Default is the agent as set up on the computer: its row has no "…".
    func testARowsMenuOffersOnlyWhatItsAccountTakes() throws {
        XCTAssertEqual(AccountRowAct.acts(for: try account("C", in: answerThenPush(8))), [.signInAgain, .remove], "signed out")
        XCTAssertEqual(AccountRowAct.acts(for: try account("C", in: answerThenPush(9))), [.remove], "signing in")
        XCTAssertEqual(AccountRowAct.acts(for: try account("B", in: answerThenPush(15))), [.remove], "signed in")
        XCTAssertEqual(AccountRowAct.acts(for: try account("B", in: answerThenPush(1))), [.remove], "not read yet")
        XCTAssertEqual(AccountRowAct.acts(for: nil), [], "the Default")
        XCTAssertEqual(AccountRowAct.signInAgain.title, "Sign In Again…")
        XCTAssertEqual(AccountRowAct.remove.title, "Remove…")
    }

    /// Under each figure, how long until its window resets, from the window's own `resets_at` (account D's 5-hour window
    /// on the recording): whole days from two days on, hours and minutes under that ("0m" dropped), minutes under an
    /// hour (at least 1), and nothing once the time has come.
    func testAWindowSaysWhenItResets() throws {
        let window = try XCTUnwrap(account("D", in: answerThenPush(15)).facts?.windows.first)
        XCTAssertEqual(window.resetsAt, 1_795_900_000)
        func words(_ secondsLeft: Int) -> String? {
            AccountSwitchWords.resets(window, now: Date(timeIntervalSince1970: TimeInterval(window.resetsAt - secondsLeft)))
        }
        XCTAssertEqual(words(59), "resets in 1 min")
        XCTAssertEqual(words(60), "resets in 1 min")
        XCTAssertEqual(words(59 * 60), "resets in 59 min")
        XCTAssertEqual(words(60 * 60), "resets in 1h")
        XCTAssertEqual(words(2 * 3600 + 10 * 60 + 30), "resets in 2h 10m")
        XCTAssertEqual(words(47 * 3600 + 59 * 60), "resets in 47h 59m")
        XCTAssertEqual(words(48 * 3600), "resets in 2 days")
        XCTAssertEqual(words(3 * 86400 + 23 * 3600), "resets in 3 days")
        XCTAssertNil(words(0), "at the reset time")
        XCTAssertNil(words(-60), "past it")
    }

    /// A window's caption is its own length, as the service names it: never a guess at "5-hour" or "Week".
    func testAWindowIsCaptionedByItsOwnLength() {
        XCTAssertEqual(AccountSwitchWords.caption(minutes: 300), "5-hour")
        XCTAssertEqual(AccountSwitchWords.caption(minutes: 10080), "Week")
        XCTAssertEqual(AccountSwitchWords.caption(minutes: 1440), "1-day")
        XCTAssertEqual(AccountSwitchWords.caption(minutes: 120), "2-hour")
    }

    /// What a row says where it has no figures: a plan with no limits, a figure not yet reported, a sign-in waiting on
    /// the browser.
    func testARowWithoutFiguresStillSaysWhy() {
        XCTAssertEqual(AccountSwitchWords.noPlanLimits, "No plan limits")
        XCTAssertEqual(AccountSwitchWords.notReported, "Not reported yet")
        XCTAssertEqual(
            AccountSwitchWords.signingIn, "Finish signing in in your browser. This stops by itself after 10 minutes.")
    }

    /// Add account… asks no name: the account is added under a composed name, the agent's own and the next number no
    /// account of it bears, and its sign-in starts at once.
    func testAddingAnAccountNamesItAndStartsItsSignIn() async throws {
        let (model, sent) = try model()
        let before = try book("list_after")
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: before))
        await loaded

        async let added = model.addAndSignIn(agent: .codex)
        await waitUntil({ sent.sent.count == 2 }, "the add to be sent")
        guard case .addAccount(let addID, "codex", let label)? = sent.sent.last else { return XCTFail("\(String(describing: sent.sent.last))") }
        XCTAssertEqual(label, "Codex 2", "Codex's only account is named Work, so 2 is free")
        let id = "01M8NEWACCOUNT0000000000AB"
        let after = try JSONDecoder().decode(ServerMessage.self, from: Data(#"""
            {"type": "accounts", "request_id": "\#(addID)", "accounts": [
              {"id": "01M80T9Q0VFB9FYGMSDGHYJ45K", "agent": "codex", "label": "Work"},
              {"id": "\#(id)", "agent": "codex", "label": "Codex 2"}], "active": []}
            """#.utf8))
        model.connection.injectForTesting(after)
        await waitUntil({ sent.sent.count == 3 }, "the sign-in to be sent")
        guard case .signInAccount(_, let signing)? = sent.sent.last else { return XCTFail("\(String(describing: sent.sent.last))") }
        XCTAssertEqual(signing, id)
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: try book("list_after")))
        let wasAdded = await added
        XCTAssertTrue(wasAdded)
    }

    /// Choosing an account asks the computer for its sessions once its answer is in: the computer sends no listing of
    /// its own when a session is marked to move, and the person is reading the switch sentence at that moment.
    func testChoosingAnAccountAsksForTheSessionsOnce() async throws {
        let (model, sent) = try model()
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: try book("list_after")))
        await loaded
        let before = sent.sent.count
        async let chosen = model.activateAccount(agent: .codex, accountID: nil)
        await waitUntil({ sent.sent.count == before + 1 }, "the choice to be sent")
        guard case .activateAccount? = sent.sent.last else { return XCTFail("\(String(describing: sent.sent.last))") }
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: try book("list_after")))
        let made = await chosen
        XCTAssertTrue(made)
        let after = sent.sent.dropFirst(before + 1)
        XCTAssertEqual(after.count { if case .sessions = $0 { true } else { false } }, 1, "\(after)")
    }

    /// Add account… waits while one of its agent's accounts is signing in (the daemon's recording: B from the sign-in's
    /// answer to its end), so a second click can't end the first sign-in for another account.
    func testAddingWaitsWhileASignInIsGoing() async throws {
        let (model, sent) = try model()
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: try answerThenPush(7)))
        await loaded
        XCTAssertFalse(model.canAddAccount(agent: .codex), "B is signing in")
        XCTAssertTrue(model.canAddAccount(agent: .claude), "another agent's sign-in doesn't hold this one")
        model.connection.injectForTesting(.accounts(requestID: nil, book: try answerThenPush(8)))
        await waitUntil({ model.canAddAccount(agent: .codex) }, "B's sign-in to end")
    }

    /// A click on a signed-out row signs it in again, as its menu's Sign In Again… does: it never becomes the account in
    /// use, so no session moves onto an account that can't start. A row not read yet is chosen as any other.
    func testChoosingASignedOutAccountSignsItInAgain() async throws {
        let (model, sent) = try model()
        async let loaded: Void = model.loadAccounts()
        await waitUntil({ sent.sent.count == 1 }, "the list to be sent")
        let book = try answerThenPush(8)
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: book))
        await loaded
        let signedOut = try account("C", in: book)
        XCTAssertTrue(signedOut.signedOut)
        async let chosen = model.choose(signedOut)
        await waitUntil({ sent.sent.count == 2 }, "the click to send")
        guard case .signInAccount(_, let id)? = sent.sent.last else { return XCTFail("\(String(describing: sent.sent.last))") }
        XCTAssertEqual(id, signedOut.id)
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: try answerThenPush(9)))
        _ = await chosen

        let unread = try account("B", in: try answerThenPush(1))
        async let used = model.choose(unread)
        await waitUntil({ sent.sent.count == 3 }, "the second click to send")
        guard case .activateAccount(_, _, let picked)? = sent.sent.last else { return XCTFail("\(String(describing: sent.sent.last))") }
        XCTAssertEqual(picked, unread.id)
        model.connection.injectForTesting(.accounts(requestID: try XCTUnwrap(requestID(sent.sent.last)), book: book))
        _ = await used
    }

    /// Whether plan limits apply, as the agent says (`limits_available`, machine's 39685ca2): false is a sign-in with
    /// none, "No plan limits"; absent is not reported yet. Never inferred from an empty list of windows.
    func testWhetherPlanLimitsApplyIsTheAgentsOwnWord() throws {
        XCTAssertNil(try account("B", in: answerThenPush(1)).facts, "the add's answer: nothing read yet")
        let filled = try answerThenPush(15)
        XCTAssertEqual(try account("C", in: filled).facts?.limitsAvailable, false, "a sign-in with no plan limits")
        XCTAssertEqual(try account("C", in: filled).facts?.windows, [])
        XCTAssertEqual(try account("D", in: filled).facts?.limitsAvailable, true)
        XCTAssertEqual(try account("D", in: filled).facts?.windows.count, 2)
        XCTAssertNil(try account("B", in: filled).facts?.limitsAvailable, "not reported: one window, no word on limits")
    }
}
