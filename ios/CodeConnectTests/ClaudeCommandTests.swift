import XCTest

@testable import CodeConnect

/// The slash-command policy: native adapters for the eight commands the app
/// answers itself, a static guard for the dialog class whose injection
/// measured as a phone lockout, and untouched passage for everything else —
/// prose and the user's own skills included.
final class ClaudeCommandPolicyTests: XCTestCase {

    /// Most policy questions do not depend on the recovery capability;
    /// this default keeps those tests about what they test.
    private func action(_ text: String) -> CommandAction {
        ClaudeCommandPolicy.action(for: text, recoversComposer: true)
    }

    func testModelIsTheNativeAdapterWithAndWithoutArguments() {
        XCTAssertEqual(action("/model"), .nativeModel(prefillArgs: ""))
        XCTAssertEqual(action("  /model sonnet  "), .nativeModel(prefillArgs: "sonnet"))
    }

    func testDiffOpensTheAppsOwnView() {
        XCTAssertEqual(action("/diff"), .nativeDiff)
    }

    /// Bare `/effort` chooses in the sheet; an argument — valid, invalid,
    /// or one this build has never heard of — passes straight through,
    /// because every measured argument runs inline and Claude Code prints
    /// its own confirmation or error. The valid set drifted once already
    /// (`ultracode`, `auto`): an app-side validity list would refuse
    /// things the Mac accepts.
    func testEffortSplitsOnBareVersusArguments() {
        XCTAssertEqual(action("/effort"), .nativeEffort)
        for typed in ["/effort high", "/effort xhigh", "/effort ultracode", "/effort bananas"] {
            XCTAssertEqual(action(typed), .passThrough, typed)
        }
    }

    func testCompactOpensItsSheetWithTypedInstructionsPrefilled() {
        XCTAssertEqual(action("/compact"), .nativeCompact(prefillInstructions: ""))
        XCTAssertEqual(
            action("/compact preserve the greek letters"),
            .nativeCompact(prefillInstructions: "preserve the greek letters"))
    }

    func testClearIsTheConfirmationAndRefusesArguments() {
        XCTAssertEqual(action("/clear"), .nativeClear)
        guard case .blocked(_, let reason) = action("/clear everything") else {
            return XCTFail("/clear with arguments must be refused")
        }
        XCTAssertTrue(reason.contains("without arguments"), reason)
    }

    /// The three snapshot commands open a Mac view on purpose — offered
    /// only when the daemon has proven it can close that view again.
    func testSnapshotCommandsRequireTheRecoveryCapability() {
        for (typed, command) in [
            ("/status", SnapshotCommand.status),
            ("/usage", .usage),
            ("/cost", .cost),
        ] {
            XCTAssertEqual(action(typed), .nativeSnapshot(command), typed)
            guard
                case .blocked(_, let reason) = ClaudeCommandPolicy.action(
                    for: typed, recoversComposer: false)
            else { return XCTFail("\(typed) must be refused below minor 9") }
            XCTAssertTrue(reason.contains("Restart it after updating"), reason)
        }
        guard case .blocked(_, let reason) = action("/status now") else {
            return XCTFail("/status with arguments must be refused")
        }
        XCTAssertTrue(reason.contains("no arguments"), reason)
    }

    /// Every command in the refuse set was MEASURED to take the Mac's
    /// composer away (needle absent at 50/100/200/400/800/1200ms).
    func testMeasuredDialogCommandsAreRefusedBeforeTyping() {
        for command in ["help", "export", "permissions", "memory", "hooks", "ide", "mcp"] {
            guard case .blocked(let blocked, let reason) = action("/\(command)") else {
                return XCTFail("/\(command) must be refused")
            }
            XCTAssertEqual(blocked, command)
            XCTAssertTrue(reason.contains("take over this composer"), reason)
            XCTAssertTrue(reason.contains("Terminal"), reason)
        }
    }

    /// The two the recovery net cannot save: measured to survive Escape —
    /// `/keybindings` opens vim, where Escape is a mode key.
    func testUnrecoverableCommandsSayWhyTheyAreDifferent() {
        for command in ["config", "keybindings"] {
            guard case .blocked(_, let reason) = action("/\(command)") else {
                return XCTFail("/\(command) must be refused")
            }
            XCTAssertTrue(reason.contains("Esc does not close"), reason)
        }
    }

    /// Measured inline, so no block — a hand-written list once claimed all
    /// three were dialogs.
    func testMeasuredInlineCommandsPassThrough() {
        for command in ["context", "agents", "focus", "effort high"] {
            XCTAssertEqual(action("/\(command)"), .passThrough, "/\(command)")
        }
    }

    /// Refused for a different reason: the app cannot observe the result.
    func testRenameIsRefusedAsUnobservable() {
        guard case .blocked(_, let reason) = action("/rename new-name") else {
            return XCTFail("rename must be refused")
        }
        XCTAssertTrue(reason.contains("cannot yet observe"), reason)
    }

    /// A custom skill is nobody's business but the user's.
    func testCustomSkillsAndProseAlwaysPassThrough() {
        XCTAssertEqual(action("/deep-research pricing"), .passThrough)
        XCTAssertEqual(action("hello"), .passThrough)
        XCTAssertEqual(action("/"), .passThrough)
        XCTAssertEqual(
            action("/tmp/build.log is failing"), .passThrough,
            "a path is not a command")
    }

    func testTokensAreCaseInsensitive() {
        XCTAssertEqual(ClaudeCommandPolicy.firstToken("/MODEL sonnet"), "model")
        XCTAssertEqual(ClaudeCommandPolicy.firstToken("hello"), nil)
        XCTAssertEqual(ClaudeCommandPolicy.firstToken("/"), nil)
    }
}

/// The palette's content rule: bare `/` is discovery (every available row
/// plus the caption), a fragment filters, and everything else — arguments,
/// unknown fragments, paths — shows nothing at all.
@MainActor
final class PaletteContentTests: XCTestCase {

    private func rows(_ typed: String, recovers: Bool = true) -> [String]? {
        CommandPalette.content(for: typed, recoversComposer: recovers)?
            .rows.map(\.command)
    }

    func testBareSlashIsDiscoveryWithEveryRowAndTheCaption() {
        let content = CommandPalette.content(for: "/", recoversComposer: true)
        XCTAssertEqual(
            content?.rows.map(\.command),
            ["model", "effort", "compact", "clear", "diff", "status", "usage", "cost"])
        XCTAssertEqual(content?.showsCaption, true)
        XCTAssertEqual(
            rows("  /"), rows("/"),
            "leading whitespace is forgiven")
        XCTAssertNil(rows("/ "), "trailing whitespace means typing has moved on")
    }

    /// Rows the daemon cannot honour are omitted, never disabled: a
    /// pre-minor-9 daemon shows five rows, not three dead ones.
    func testSnapshotRowsAreOmittedWithoutTheRecoveryCapability() {
        XCTAssertEqual(
            rows("/", recovers: false),
            ["model", "effort", "compact", "clear", "diff"])
        XCTAssertNil(rows("/st", recovers: false), "no row, no palette")
    }

    func testFragmentsFilterCaseInsensitively() {
        XCTAssertEqual(rows("/m"), ["model"])
        XCTAssertEqual(rows("/MODEL"), ["model"])
        XCTAssertEqual(rows("/c"), ["compact", "clear", "cost"])
        XCTAssertEqual(rows("/co"), ["compact", "cost"])
        XCTAssertEqual(rows("/e"), ["effort"])
        XCTAssertEqual(rows("/s"), ["status"])
        for typed in ["/c", "/e", "/s"] {
            XCTAssertEqual(
                CommandPalette.content(for: typed, recoversComposer: true)?.showsCaption,
                false, "the caption belongs to discovery, not filtering")
        }
    }

    func testEverythingElseShowsNoPalette() {
        for typed in ["hello", "/model sonnet", "/tmp/x", "/mx", "", "/model ", "/zebra"] {
            XCTAssertNil(rows(typed), typed)
        }
    }

    /// A tapped row and the typed command resolve to the same action — the
    /// palette can never do something typing would not.
    func testRowActionsMatchTheTypedPolicy() {
        for row in CommandPalette.allRows {
            XCTAssertEqual(
                row.action,
                ClaudeCommandPolicy.action(for: "/\(row.command)", recoversComposer: true),
                row.command)
        }
    }
}

/// Claude Code's measured confirmation grammars — exactly what the rig
/// recorded, nothing invented.
final class CommandConfirmationTests: XCTestCase {

    func testEffortConfirmationsParseForAllFiveValues() {
        // Verbatim measured lines, scope notes and all.
        let measured: [(String, String)] = [
            (
                "Set effort level to medium (saved as your default for new sessions): "
                    + "Balanced approach with standard implementation and testing",
                "medium"
            ),
            ("Set effort level to low (saved as your default for new sessions): Quick", "low"),
            ("Set effort level to xhigh (saved as your default for new sessions): Deeper", "xhigh"),
            ("Set effort level to max (this session only): Maximum capability", "max"),
            ("Set effort level to high (saved as your default for new sessions): Comp", "high"),
        ]
        for (line, value) in measured {
            XCTAssertEqual(EffortConfirmation.parse(line), value, line)
        }
    }

    func testEffortRejectsWhatItNeverMeasured() {
        XCTAssertNil(EffortConfirmation.parse("Invalid argument: bananas. Valid options are: low"))
        XCTAssertNil(EffortConfirmation.parse("Set model to Sonnet 5 and saved"))
        XCTAssertNil(EffortConfirmation.parse("Set effort level to "))
        XCTAssertNil(EffortConfirmation.parse("effort is now high"))
    }

    func testEffortLabelsAreTheHumanWords() {
        XCTAssertEqual(EffortConfirmation.label(for: "xhigh"), "Extra high")
        XCTAssertEqual(EffortConfirmation.label(for: "max"), "Maximum")
        XCTAssertEqual(EffortConfirmation.label(for: "high"), "High")
        XCTAssertEqual(
            EffortConfirmation.label(for: "ultracode"), "ultracode",
            "an unmeasured value stays visibly machine-shaped")
    }

    func testCompactSignalsParseBothMeasuredLines() {
        XCTAssertEqual(
            CompactConfirmation.parse("Compacted (ctrl+o to see full summary)"), .compacted)
        XCTAssertEqual(
            CompactConfirmation.parse("Not enough messages to compact."), .notEnoughMessages)
        XCTAssertNil(CompactConfirmation.parse("compacting now…"))
        XCTAssertNil(CompactConfirmation.parse("Not enough messages"))
    }
}

/// One provenance line, one shape — origin and age together, never three
/// floating pieces.
@MainActor
final class ProvenanceLineTests: XCTestCase {

    func testBothOriginsRenderWithTheirAge() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let start = ConfirmedModel(
            name: "claude-opus-5[1m]", source: "Session start",
            at: now.addingTimeInterval(-120))
        XCTAssertEqual(
            ModelSheet.provenanceLine(start, now: now),
            "Confirmed at session start · 2m ago")
        let command = ConfirmedModel(
            name: "Sonnet 5", source: "Command confirmation",
            at: now.addingTimeInterval(-5))
        XCTAssertEqual(
            ModelSheet.provenanceLine(command, now: now),
            "Confirmed by /model · just now")
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

/// The effort and compact facts ride the same ingest path as the model
/// fact: Claude Code's own stdout lines, ANSI-stripped, nothing else.
@MainActor
final class EffortAndCompactFactTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    private func stdoutEvent(seq: Int, _ line: String) throws -> Event {
        try event(
            """
            {"seq":\(seq),"session_id":"cc-1","ts":"2026-08-05T20:00:0\(seq).000Z",
             "kind":"user_message","source":"transcript",
             "payload":{"type":"user","message":{"role":"user","content":
             "<local-command-stdout>\(line)</local-command-stdout>"}}}
            """)
    }

    func testAnEffortConfirmationBecomesTheFact() throws {
        let state = SessionState(sessionKey: "cc-1")
        XCTAssertNil(state.lastConfirmedEffort)
        state.ingest(
            try stdoutEvent(
                seq: 1,
                "Set effort level to xhigh (saved as your default for new sessions): Deeper"))
        XCTAssertEqual(state.lastConfirmedEffort?.value, "xhigh")
    }

    func testACompactionOutcomeBecomesTheFact() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(try stdoutEvent(seq: 1, "Not enough messages to compact."))
        XCTAssertEqual(state.lastCompactSignal?.outcome, .notEnoughMessages)
        state.ingest(try stdoutEvent(seq: 2, "Compacted (ctrl+o to see full summary)"))
        XCTAssertEqual(state.lastCompactSignal?.outcome, .compacted)
    }

    func testAnInvalidEffortLineIsNotAConfirmation() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try stdoutEvent(
                seq: 1, "Invalid argument: bananas. Valid options are: low, medium, high"))
        XCTAssertNil(state.lastConfirmedEffort)
    }
}

/// `/clear` in the timeline: the invocation entry at the head of the rotated
/// transcript renders as the fact it proves.
@MainActor
final class ClearedNoticeTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    func testAClearInvocationRendersAsTheClearedNotice() async throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-08-05T20:00:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<command-name>/clear</command-name>\\n<command-message>clear</command-message>\\n<command-args></command-args>"}}}
                """))
        // `ingest` schedules the rebuild; read synchronously the timeline is
        // empty and the assertion passes for the wrong reason.
        try? await Task.sleep(for: SessionState.coalesceWindow * 6)
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        guard
            let item = state.timeline.first(where: {
                if case .notice(let notice) = $0.content {
                    return notice.title == "Conversation cleared."
                }
                return false
            })
        else { return XCTFail("the /clear invocation must render as the cleared notice") }
        if case .notice(let notice) = item.content {
            XCTAssertEqual(notice.symbol, "eraser")
        }
    }

    func testOtherInvocationsStillRenderAsCommands() async throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-08-05T20:00:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<command-name>/compact</command-name>\\n<command-message>compact</command-message>\\n<command-args>keep tests</command-args>"}}}
                """))
        try? await Task.sleep(for: SessionState.coalesceWindow * 6)
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        XCTAssertTrue(
            state.timeline.contains(where: {
                if case .userMessage(let text, let isCommand) = $0.content {
                    return text == "/compact keep tests" && isCommand
                }
                return false
            }))
    }
}
