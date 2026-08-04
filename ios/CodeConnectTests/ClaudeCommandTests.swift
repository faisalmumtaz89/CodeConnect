import XCTest

@testable import CodeConnect

/// The slash-command policy: one native adapter, a static guard for the
/// dialog class whose injection measured as a phone lockout, fail-closed for
/// discovered-but-unclassified built-ins, and untouched passage for
/// everything else — prose and the user's own skills included.
final class ClaudeCommandPolicyTests: XCTestCase {

    func testModelIsTheNativeAdapterWithAndWithoutArguments() {
        XCTAssertEqual(
            ClaudeCommandPolicy.action(for: "/model", catalog: nil),
            .nativeModel(prefillArgs: ""))
        XCTAssertEqual(
            ClaudeCommandPolicy.action(for: "  /model sonnet  ", catalog: nil),
            .nativeModel(prefillArgs: "sonnet"))
    }

    /// The measured lockout class: these open a dialog that replaces the
    /// Mac's composer, after which every phone send is refused. The guard is
    /// static so it holds even when discovery is unavailable.
    func testDialogCommandsAreBlockedWithTheLockoutSentence() {
        for command in ["permissions", "agents", "config", "context", "memory", "hooks"] {
            guard
                case .blocked(let blocked, let reason) = ClaudeCommandPolicy.action(
                    for: "/\(command)", catalog: nil)
            else { return XCTFail("/\(command) must be blocked") }
            XCTAssertEqual(blocked, command)
            XCTAssertTrue(reason.contains("locks this composer"), reason)
            XCTAssertTrue(reason.contains("Terminal"), reason)
        }
    }

    /// Fail closed: a discovered built-in this app has not classified might
    /// be a dialog under a new name.
    func testACatalogedButUnclassifiedBuiltInIsBlocked() {
        guard
            case .blocked(_, let reason) = ClaudeCommandPolicy.action(
                for: "/insights", catalog: ["model", "clear", "insights"])
        else { return XCTFail("a cataloged built-in must fail closed") }
        XCTAssertTrue(reason.contains("doesn't drive yet"), reason)
    }

    /// A custom skill is not in the binary's own list, and refusing it would
    /// break the user's own commands.
    func testAnUncatalogedCommandPassesThrough() {
        XCTAssertEqual(
            ClaudeCommandPolicy.action(for: "/catchup all of it", catalog: ["model", "clear"]),
            .passThrough)
        XCTAssertEqual(
            ClaudeCommandPolicy.action(for: "/catchup", catalog: nil),
            .passThrough,
            "no catalog means no claim of knowledge — only the static dialog set blocks")
    }

    func testProseAndNonCommandShapesPassThrough() {
        XCTAssertEqual(ClaudeCommandPolicy.action(for: "hello", catalog: nil), .passThrough)
        XCTAssertEqual(ClaudeCommandPolicy.action(for: "/", catalog: nil), .passThrough)
        XCTAssertEqual(
            ClaudeCommandPolicy.action(for: "/tmp/build.log is failing", catalog: nil),
            .passThrough, "a path is not a command")
    }

    func testTokensAreCaseInsensitive() {
        XCTAssertEqual(ClaudeCommandPolicy.firstToken("/MODEL sonnet"), "model")
        XCTAssertEqual(ClaudeCommandPolicy.firstToken("hello"), nil)
        XCTAssertEqual(ClaudeCommandPolicy.firstToken("/"), nil)
    }
}

/// Claude Code's confirmation grammar for model changes — both measured
/// spellings, nothing invented.
final class ModelConfirmationTests: XCTestCase {

    func testTheMeasuredSpellingsParse() {
        XCTAssertEqual(
            ModelConfirmation.parse(
                "Set model to Sonnet 5 and saved as your default for new sessions"),
            "Sonnet 5")
        XCTAssertEqual(ModelConfirmation.parse("Kept model as Fable 5"), "Fable 5")
    }

    func testAFutureSuffixlessSpellingStillYieldsTheName() {
        XCTAssertEqual(ModelConfirmation.parse("Set model to Haiku 4.5"), "Haiku 4.5")
    }

    func testUnrelatedLinesParseAsNothing() {
        XCTAssertNil(ModelConfirmation.parse("Goodbye!"))
        XCTAssertNil(ModelConfirmation.parse("Set effort to high"))
        XCTAssertNil(ModelConfirmation.parse(""))
    }
}

/// The sheet's correlation rule, as a pure function.
final class ModelChangeWatchTests: XCTestCase {
    private let sonnet = ConfirmedModel(
        name: "Sonnet 5", source: "Command confirmation", at: Date(timeIntervalSince1970: 100))

    func testANewCommandConfirmationCounts() {
        XCTAssertEqual(ModelChangeWatch.confirmed(baseline: nil, current: sonnet), "Sonnet 5")
    }

    func testTheUnchangedBaselineDoesNot() {
        XCTAssertNil(ModelChangeWatch.confirmed(baseline: sonnet, current: sonnet))
    }

    func testASessionStartSeedDoesNot() {
        let seed = ConfirmedModel(
            name: "claude-fable-5", source: "Session start",
            at: Date(timeIntervalSince1970: 200))
        XCTAssertNil(ModelChangeWatch.confirmed(baseline: nil, current: seed))
    }

    func testNothingCurrentDoesNot() {
        XCTAssertNil(ModelChangeWatch.confirmed(baseline: sonnet, current: nil))
    }
}

/// `lastConfirmedModel`: seeded by the SessionStart hook, replaced only by
/// Claude Code's own confirmation lines, provenance carried.
@MainActor
final class LastConfirmedModelTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    func testSessionStartSeedsAndConfirmationReplaces() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-08-04T20:00:00.000Z",
                 "kind":"session_start","source":"hook",
                 "payload":{"hook_event_name":"SessionStart","model":"claude-fable-5"}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "claude-fable-5")
        XCTAssertEqual(state.lastConfirmedModel?.source, "Session start")

        state.ingest(
            try event(
                """
                {"seq":2,"session_id":"cc-1","ts":"2026-08-04T20:01:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Set model to \\u001b[1mSonnet 5\\u001b[22m and saved as your default for new sessions</local-command-stdout>"}}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "Sonnet 5")
        XCTAssertEqual(state.lastConfirmedModel?.source, "Command confirmation")
    }

    func testAKeptModelLineAlsoConfirms() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-08-04T20:00:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Kept model as Fable 5</local-command-stdout>"}}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "Fable 5")
    }

    /// The replay path: a backfilled *older* confirmation may fill an empty
    /// slot but must never displace a newer fact — and a reset forgets the
    /// fact entirely.
    func testAReplayedOlderConfirmationCannotDisplaceANewerOne() async throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":8,"session_id":"cc-1","ts":"2026-08-04T20:05:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Set model to Sonnet 5 and saved as your default for new sessions</local-command-stdout>"}}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "Sonnet 5")

        // A replay below the tail: seq 3's confirmation is older news.
        state.ingest(
            try event(
                """
                {"seq":3,"session_id":"cc-1","ts":"2026-08-04T20:01:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Kept model as Fable 5</local-command-stdout>"}}}
                """))
        try await Task.sleep(for: .milliseconds(400))
        XCTAssertEqual(
            state.lastConfirmedModel?.name, "Sonnet 5",
            "a replayed older confirmation is history, not news")
    }

    func testOrdinaryTrafficChangesNothing() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-08-04T20:00:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":"set the model please"}}}
                """))
        XCTAssertNil(state.lastConfirmedModel)
    }
}
