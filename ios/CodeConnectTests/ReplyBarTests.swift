import XCTest

@testable import CodeConnectKit

/// **What a session's reply box shows beside its send button** (`ReplyBar`): the context from the run's newest `usage`
/// that names both figures, the session's account's windows with their countdowns, and the account's name; each part
/// absent when nothing is held, never a placeholder. From the daemon's recordings: `sdk-session-events.json` (Claude's
/// usage events), `mail-wake-turn-wire.json` (Codex's), `fixtures/claude/minor-49-wire.json` (a /compact turn's
/// usage), `fixtures/accounts/accounts-answer-then-push-wire.json` (Claude's D with both windows, C with no plan
/// limits, Codex's Default with one).
final class ReplyBarTests: XCTestCase {

    private var repoRoot: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
    }

    private func bundled(_ name: String) throws -> JSONValue {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
    }

    private func repo(_ path: String) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: repoRoot.appendingPathComponent(path)))
    }

    private func decode<T: Decodable>(_ type: T.Type, _ value: JSONValue) throws -> T {
        try JSONDecoder().decode(type, from: Data(value.canonicalJSONString.utf8))
    }

    private func claudeEvents() throws -> [Event] {
        try XCTUnwrap(try bundled("sdk-session-events").arrayValue).map { try decode(Event.self, $0) }
    }

    /// The recorded accounts frame, with `change` applied to the account named `label` (`"Default"`: Codex's Default).
    private func book(changing label: String? = nil, _ change: (inout [String: JSONValue]) -> Void = { _ in })
        throws -> AccountBook
    {
        var frame = try XCTUnwrap(try repo("fixtures/accounts/accounts-answer-then-push-wire.json")[15]?.objectValue)
        if let label {
            let key = label == "Default" ? "defaults" : "accounts"
            var rows = try XCTUnwrap(frame[key]?.arrayValue)
            let index = try XCTUnwrap(rows.firstIndex { label == "Default" || $0["label"]?.stringValue == label })
            var row = try XCTUnwrap(rows[index].objectValue)
            var facts = try XCTUnwrap(row["facts"]?.objectValue)
            change(&facts)
            row["facts"] = .object(facts)
            rows[index] = .object(row)
            frame[key] = .array(rows)
        }
        guard case .accounts(_, let book) = try decode(ServerMessage.self, .object(frame)) else {
            throw XCTSkip("frame 15 is not an accounts frame")
        }
        return book
    }

    /// A recorded live summary of `agent`, on the account `account` names (`nil`: none named).
    private func summary(_ agent: StartAgent, account: JSONValue?) throws -> SessionSummary {
        let (name, key) =
            agent == .claude ? ("minor-46-wire", "session_hosted_live") : ("codex-settings-wire", "session_codex_live")
        var run = try XCTUnwrap(try bundled(name)[key]?.objectValue)
        run["account"] = account
        return try decode(SessionSummary.self, .object(run))
    }

    private func on(_ label: String, in book: AccountBook) throws -> (AgentAccount, SessionSummary) {
        let account = try XCTUnwrap(book.accounts.first { $0.label == label }, label)
        return (account, try summary(.claude, account: .object(["id": .string(account.id), "label": .string(label)])))
    }

    private var onCodexDefault: JSONValue { .object(["id": .null, "label": .string("Default")]) }

    /// 2h 10m before D's 5-hour window resets.
    private let now = Date(timeIntervalSince1970: 1_795_900_000 - (2 * 3600 + 10 * 60))

    // MARK: R4-1 The bar's figures are the Accounts page's

    func testTheBarShowsTheSessionsAccountsWindowsAsThePageDoes() throws {
        let book = try book()
        let (d, run) = try on("D", in: book)
        let bar = ReplyBar.of(events: [], summary: run, book: book, now: now)
        XCTAssertEqual(bar.limits, [
            .init(usedPercent: 12, caption: "5-hour", resets: "resets in 2h 10m"),
            .init(usedPercent: 42, caption: "Week", resets: "resets in 5 days"),
        ])
        XCTAssertEqual(bar.limits, ReplyBar.limits(d.facts, now: now), "the page's row for the same account")
        XCTAssertNil(bar.limitsNote)
        XCTAssertEqual(bar.account, d.facts?.email)

        let codex = ReplyBar.of(events: [], summary: try summary(.codex, account: onCodexDefault), book: book, now: now)
        XCTAssertEqual(codex.limits, ReplyBar.limits(book.defaultFacts(for: .codex), now: now), "the Default's row")
        XCTAssertEqual(codex.account, "default@example.test")
    }

    // MARK: Context

    func testTheContextIsTheNewestUsageThatNamesBothFigures() throws {
        let run = try summary(.claude, account: nil)
        XCTAssertEqual(
            ReplyBar.of(events: try claudeEvents(), summary: run, book: nil, now: now).context,
            .init(used: "32k", window: "1M"))
        let codexFrame = try XCTUnwrap(try bundled("mail-wake-turn-wire")["act"]?["frames"]?[4]?["event"])
        let codexEvent = try decode(Event.self, codexFrame)
        XCTAssertEqual(
            ReplyBar.of(events: [codexEvent], summary: try summary(.codex, account: nil), book: nil, now: now).context,
            .init(used: "17k", window: "258k"))
    }

    /// The run's last event, as a /compact turn's `usage` (minor-49-wire's `usage_compact`): neither figure.
    private func compactTurn(after events: [JSONValue]) throws -> Event {
        var event = try XCTUnwrap(events.last?.objectValue)
        event["seq"] = .int(Int64(try XCTUnwrap(event["seq"]?.intValue) + 1))
        event["kind"] = .string("usage")
        event["payload"] = try XCTUnwrap(try repo("fixtures/claude/minor-49-wire.json")["usage_compact"])
        return try decode(Event.self, .object(event))
    }

    func testACommandTurnLeavesTheContextBeforeIt() throws {
        let raw = try XCTUnwrap(try bundled("sdk-session-events").arrayValue)
        let events = try claudeEvents() + [try compactTurn(after: raw)]
        XCTAssertEqual(
            ReplyBar.of(events: events, summary: try summary(.claude, account: nil), book: nil, now: now).context,
            .init(used: "32k", window: "1M"))
    }

    // MARK: R4-2 Absent draws nothing

    func testNoUsageYetHasNoContext() throws {
        let events = try claudeEvents().filter { $0.kind != .usage }
        let run = try summary(.claude, account: nil)
        XCTAssertNil(ReplyBar.of(events: events, summary: run, book: nil, now: now).context)
    }

    func testOnlyCommandTurnsSoFarHaveNoContext() throws {
        let raw = try XCTUnwrap(try bundled("sdk-session-events").arrayValue)
        let events = try claudeEvents().filter { $0.kind != .usage } + [try compactTurn(after: raw)]
        let run = try summary(.claude, account: nil)
        XCTAssertNil(ReplyBar.of(events: events, summary: run, book: nil, now: now).context)
    }

    func testARunThatNamesNoAccountHasNoLimitsAndNoAccount() throws {
        let bar = ReplyBar.of(events: [], summary: try summary(.claude, account: nil), book: try book(), now: now)
        XCTAssertEqual(bar.limits, [])
        XCTAssertNil(bar.limitsNote)
        XCTAssertNil(bar.account)
    }

    /// Facts not read: no book yet, or an account it does not list. No limit rows and no words; the account by its label.
    func testFactsNotReadHaveNoLimitsAndNoWords() throws {
        let removed = try summary(
            .claude, account: .object(["id": .string("01M8REMOVED0000000000000000"), "label": .string("E")]))
        for book in [nil, try book()] {
            let bar = ReplyBar.of(events: [], summary: removed, book: book, now: now)
            XCTAssertEqual(bar.limits, [])
            XCTAssertNil(bar.limitsNote)
            XCTAssertEqual(bar.account, "E", "named by its label")
        }
    }

    func testAClaudeAccountWithNoFiveHourWindowShowsTheWeekOnly() throws {
        let book = try book(changing: "D") { facts in
            let windows = facts["windows"]?.arrayValue ?? []
            facts["windows"] = .array(windows.filter { $0["window_minutes"]?.intValue != 300 })
        }
        let (_, run) = try on("D", in: book)
        XCTAssertEqual(ReplyBar.of(events: [], summary: run, book: book, now: now).limits.map(\.caption), ["Week"])
    }

    func testACodexAccountWithNoWindowsHasNoLimits() throws {
        let book = try book(changing: "Default") { $0["windows"] = .array([]) }
        let bar = ReplyBar.of(events: [], summary: try summary(.codex, account: onCodexDefault), book: book, now: now)
        XCTAssertEqual(bar.limits, [])
        XCTAssertNil(bar.limitsNote)
    }

    func testNoPlanLimitsSaysSoInTheLimitsPlace() throws {
        let book = try book()
        let (c, run) = try on("C", in: book)
        let bar = ReplyBar.of(events: [], summary: run, book: book, now: now)
        XCTAssertEqual(bar.limits, [])
        XCTAssertEqual(bar.limitsNote, "No plan limits")
        XCTAssertEqual(bar.account, c.facts?.email)
    }

    func testAPassedResetHasNoCountdown() throws {
        let bar = ReplyBar.of(
            events: [], summary: try summary(.codex, account: onCodexDefault), book: try book(),
            now: Date(timeIntervalSince1970: 1_800_000_000))
        XCTAssertEqual(bar.limits.map(\.resets), [nil])
    }

    // MARK: The sentence both apps speak

    /// The bar's whole reading as one sentence, for VoiceOver on the Mac's figures and the iPhone's settings button:
    /// each part in the order the card draws it, a passed reset and an absent part left out.
    func testTheBarIsSpokenAsOneSentence() throws {
        let book = try book()
        let (d, run) = try on("D", in: book)
        let email = try XCTUnwrap(d.facts?.email)
        XCTAssertEqual(
            ReplyBar.of(events: try claudeEvents(), summary: run, book: book, now: now).spoken,
            "Context 32k of 1M; 5-hour 12%, resets in 2h 10m; Week 42%, resets in 5 days; Account \(email)")
        let (c, noLimits) = try on("C", in: book)
        XCTAssertEqual(
            ReplyBar.of(events: [], summary: noLimits, book: book, now: now).spoken,
            "No plan limits; Account \(try XCTUnwrap(c.facts?.email))")
        let passed = ReplyBar.of(
            events: [], summary: try summary(.codex, account: onCodexDefault), book: book,
            now: Date(timeIntervalSince1970: 1_800_000_000))
        XCTAssertEqual(passed.spoken, "5-hour 42%; Account default@example.test")
        XCTAssertEqual(ReplyBar.of(events: [], summary: try summary(.claude, account: nil), book: nil, now: now).spoken, "")
    }

    // MARK: R4-5 Context text

    func testTokensAreWrittenInThousandsAndMillions() {
        let cases: [(Int, String)] = [
            (0, "0"), (999, "999"), (1000, "1k"), (1499, "1k"), (1500, "2k"), (38_660, "39k"), (258_400, "258k"),
            (390_400, "390k"), (999_499, "999k"), (999_500, "1M"), (1_000_000, "1M"), (1_049_999, "1M"),
            (1_050_000, "1.1M"), (1_250_000, "1.3M"), (2_000_000, "2M"),
        ]
        for (count, words) in cases { XCTAssertEqual(ReplyBar.tokens(count), words, "\(count)") }
    }
}
