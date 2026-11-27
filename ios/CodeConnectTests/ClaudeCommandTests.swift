import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The slash-command policy: native adapters for the five commands the app
/// answers itself, and untouched passage for everything else — Claude's other
/// commands, prose and the user's own skills included.
final class ClaudeCommandPolicyTests: XCTestCase {

    private func action(_ text: String) -> CommandAction {
        ClaudeCommandPolicy.action(for: text)
    }

    func testModelIsTheNativeAdapterWithAndWithoutArguments() {
        XCTAssertEqual(action("/model"), .nativeModel)
        XCTAssertEqual(action("  /model sonnet  "), .nativeModel)
    }

    func testDiffOpensTheAppsOwnView() {
        XCTAssertEqual(action("/diff"), .nativeDiff)
    }

    /// Bare `/effort` chooses in the sheet; an argument — valid, invalid, or
    /// one this build has never heard of — passes straight through, because an
    /// app-side validity list would refuse things the Mac accepts (the valid
    /// set drifted once already: `ultracode`, `auto`).
    ///
    /// It is NOT because every argument runs inline: measured on 2.1.223, a
    /// *different* value on a cache-warm conversation opens the same
    /// confirmation `/model` does.
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

    /// Every command without a control of its own is typed: Claude Code answers it on a run CodeConnect started.
    func testEveryOtherCommandIsTyped() {
        for typed in [
            "/status", "/usage", "/cost", "/help", "/config autoCompact=false", "/keybindings", "/mcp",
            "/rename new-name", "/context", "/agents",
        ] {
            XCTAssertEqual(action(typed), .passThrough, typed)
        }
    }

    /// Neither app has a terminal, so no refusal sends the reader to one: each ends at why the command is not
    /// available here.
    func testNoRefusalSendsTheReaderToATerminal() {
        for typed in ["/clear everything"] {
            guard case .blocked(_, let reason) = action(typed) else { return XCTFail("\(typed) must be refused") }
            XCTAssertFalse(reason.localizedCaseInsensitiveContains("terminal"), "\(typed): \(reason)")
        }
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

    private func rows(_ typed: String) -> [String]? {
        SlashPalette.content(for: typed)?.rows.map(\.command)
    }

    func testBareSlashIsDiscoveryWithEveryRowAndTheCaption() {
        let content = SlashPalette.content(for: "/")
        XCTAssertEqual(content?.rows.map(\.command), ["model", "effort", "compact", "clear", "diff"])
        XCTAssertEqual(content?.caption, "Any other command goes to Claude Code as typed")
        XCTAssertEqual(
            rows("  /"), rows("/"),
            "leading whitespace is forgiven")
        XCTAssertNil(rows("/ "), "trailing whitespace means typing has moved on")
    }

    func testFragmentsFilterCaseInsensitively() {
        XCTAssertEqual(rows("/m"), ["model"])
        XCTAssertEqual(rows("/MODEL"), ["model"])
        XCTAssertEqual(rows("/c"), ["compact", "clear"])
        XCTAssertEqual(rows("/co"), ["compact"])
        XCTAssertEqual(rows("/e"), ["effort"])
        XCTAssertNil(rows("/s"), "no row, no palette")
        for typed in ["/c", "/e"] {
            XCTAssertEqual(
                SlashPalette.content(for: typed)?.caption,
                nil, "the caption belongs to discovery, not filtering")
        }
    }

    func testEverythingElseShowsNoPalette() {
        for typed in ["hello", "/model sonnet", "/tmp/x", "/mx", "", "/model ", "/zebra"] {
            XCTAssertNil(rows(typed), typed)
        }
    }

    /// The palette closes when a row runs, on Esc and on a click outside, and comes back on the next edit: after
    /// /model ran from "/", clearing the box and typing "/" again shows it again. Must-fail: a close that outlasts the
    /// edit (the latch kept the draft it closed on, and "/" typed again matched it), so Return sent a bare "/".
    func testThePaletteComesBackOnTheNextEdit() {
        var latch = SlashPalette.Latch()
        func palette(_ draft: String, _ agent: AgentKind = .claude, focused: Bool = true) -> SlashPalette.Content? {
            latch.content(for: draft, agent: agent, changes: nil, focused: focused)
        }
        XCTAssertNotNil(palette("/"))
        latch.close()  // /model ran from the palette
        XCTAssertNil(palette("/"), "closed on the draft it ran from")
        latch.edited()  // ⌘A Delete
        latch.edited()  // "/"
        XCTAssertNotNil(palette("/"), "the same words typed again show it")
        latch.close()  // Esc on "/c"
        XCTAssertNil(palette("/c"), "Esc keeps it closed on the words")
        latch.edited()  // "/cl"
        XCTAssertEqual(palette("/cl")?.rows.map(\.command), ["clear"])
        XCTAssertNil(palette("/", .codex), "a Codex run has no palette")
        XCTAssertNil(palette("/", focused: false), "nor a box without the keyboard")
    }

    /// The palette's keys: the first row lit, ↑ and ↓ move the light and stop at the ends, Return runs the lit row,
    /// ⇧/⌥Return add a line, Esc closes it; with no palette every key is the box's but ⇧/⌥Return. Must-fail: Return
    /// with a row lit falls through to the box and sends the fragment as typed.
    func testThePalettesKeys() {
        func outcome(_ key: SlashPalette.Key, rows: Int = 3, lit: Int = 0) -> SlashPalette.KeyOutcome {
            SlashPalette.outcome(of: key, rows: Array(SlashPalette.allRows.prefix(rows)), highlighted: lit)
        }
        XCTAssertEqual(outcome(.return(addsLine: false)), .run(0), "Return runs the first row, lit when it shows")
        XCTAssertEqual(outcome(.return(addsLine: false), lit: 2), .run(2))
        XCTAssertEqual(outcome(.down), .highlight(1))
        XCTAssertEqual(outcome(.down, lit: 2), .highlight(2), "↓ stops at the last row")
        XCTAssertEqual(outcome(.up, lit: 1), .highlight(0))
        XCTAssertEqual(outcome(.up), .highlight(0), "↑ stops at the first row")
        XCTAssertEqual(outcome(.escape), .close)
        XCTAssertEqual(outcome(.return(addsLine: true)), .addLine, "⇧/⌥Return adds a line over the palette")
        for key: SlashPalette.Key in [.up, .down, .escape, .return(addsLine: false)] {
            XCTAssertEqual(outcome(key, rows: 0), .toBox, "no palette: \(key)")
        }
        XCTAssertEqual(outcome(.return(addsLine: true), rows: 0), .addLine)
    }

    /// `/diff` is dimmed, saying "No changes yet", while the run's computer counts no changed lines, and only then: a
    /// run whose count is not known yet, or one with changes, keeps it. ↓ passes a dimmed row, and Return on one does
    /// nothing: no fragment is sent as a message.
    func testDiffIsDimmedWhileTheRunHasNoChanges() {
        func diff(_ changes: DiffStat?) -> SlashRow? {
            SlashPalette.content(for: "/", changes: changes)?.rows.first { $0.command == "diff" }
        }
        XCTAssertEqual(diff(DiffStat(added: 0, removed: 0))?.unavailable, "No changes yet")
        XCTAssertNil(diff(nil)?.unavailable, "a count not known yet dims nothing")
        XCTAssertNil(diff(DiffStat(added: 3, removed: 0))?.unavailable)
        XCTAssertNil(diff(DiffStat(added: 0, removed: 1))?.unavailable)
        let rows = SlashPalette.content(for: "/", changes: DiffStat(added: 0, removed: 0))!.rows
        XCTAssertEqual(rows.filter { $0.unavailable != nil }.map(\.command), ["diff"], "only /diff")
        let last = rows.count - 1
        XCTAssertEqual(
            SlashPalette.outcome(of: .down, rows: rows, highlighted: last - 1), .highlight(last - 1), "↓ passes it")
        XCTAssertEqual(
            SlashPalette.outcome(of: .return(addsLine: false), rows: rows, highlighted: last), .highlight(last))
        let alone = SlashPalette.content(for: "/di", changes: DiffStat(added: 0, removed: 0))!.rows
        XCTAssertEqual(SlashPalette.outcome(of: .return(addsLine: false), rows: alone, highlighted: 0), .highlight(0))
    }

    /// A tapped row and the typed command resolve to the same action — the
    /// palette can never do something typing would not.
    func testRowActionsMatchTheTypedPolicy() {
        for row in SlashPalette.allRows {
            XCTAssertEqual(
                row.action,
                ClaudeCommandPolicy.action(for: "/\(row.command)"),
                row.command)
        }
    }
}

/// Claude Code's measured confirmation grammars — exactly what the rig
/// recorded, nothing invented.
final class CommandConfirmationTests: XCTestCase {

    /// Measured lines, scope notes and all, re-measured on 2.1.223 — the trailing
    /// descriptions elided, since nothing here parses them.
    /// **The scope differs per value** and is therefore read, never assumed:
    /// four of the five save as the default, `max` is this-session-only.
    func testEffortConfirmationsParseForAllFiveValuesWithTheirScope() {
        let saved = "saved as your default for new sessions"
        let measured: [(String, String, String)] = [
            (
                "Set effort level to medium (saved as your default for new sessions): "
                    + "Balanced approach with standard implementation and testing",
                "medium", saved
            ),
            ("Set effort level to low (saved as your default for new sessions): Quick",
             "low", saved),
            ("Set effort level to xhigh (saved as your default for new sessions): Deeper",
             "xhigh", saved),
            ("Set effort level to max (this session only): Maximum capability",
             "max", "this session only"),
            ("Set effort level to high (saved as your default for new sessions): Comp",
             "high", saved),
        ]
        for (line, value, scope) in measured {
            XCTAssertEqual(EffortConfirmation.parse(line), .set(value: value, scope: scope), line)
        }
    }

    /// `Kept effort level as X` is Claude Code's no-change receipt for the
    /// `Change effort level?` confirmation, read as the level it kept.
    func testAKeptEffortLineIsParsedAsANonChange() {
        XCTAssertEqual(EffortConfirmation.parse("Kept effort level as high"), .kept(value: "high"))
    }

    /// A receipt shape this build has never seen states the level and no
    /// scope, rather than inventing one.
    func testAScopelessSetReceiptCarriesNoScope() {
        XCTAssertEqual(
            EffortConfirmation.parse("Set effort level to high: Comprehensive"),
            .set(value: "high", scope: nil))
    }

    /// The scope must follow the value immediately. `xhigh`'s description ends
    /// `(Fable 5, Opus 4.7+, Sonnet 5)`, so a parser that searched the whole
    /// line would read a description as a scope the moment either is reworded.
    func testAParentheticalInTheDescriptionIsNotReadAsAScope() {
        XCTAssertEqual(
            EffortConfirmation.parse(
                "Set effort level to high: Comprehensive (saved as your default for new sessions)"),
            .set(value: "high", scope: nil))
    }

    func testEffortRejectsWhatItNeverMeasured() {
        XCTAssertNil(EffortConfirmation.parse("Invalid argument: bananas. Valid options are: low"))
        XCTAssertNil(EffortConfirmation.parse("Set model to Sonnet 5 and saved"))
        XCTAssertNil(EffortConfirmation.parse("Set effort level to "))
        XCTAssertNil(EffortConfirmation.parse("Kept effort level as "))
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

    /// The rule the phone and the daemon share, case by case, from the
    /// transcript line as it arrives: the name out of one code span around it,
    /// and nothing else.
    func testTheReceiptRuleThePhoneAndTheDaemonShare() {
        let cases: [(String, ModelConfirmation?)] = [
            ("Set model to `Opus 5.5` and saved as your default for new sessions", .set("Opus 5.5")),
            ("Kept model as `Opus 5.5`", .kept("Opus 5.5")),
            ("Set model to ``", .set("``")),
            ("Set model to ` `", .set(" ")),
            ("Set model to `opus` (claude-opus-5[1m])", .set("`opus` (claude-opus-5[1m])")),
        ]
        for (line, expected) in cases {
            guard case .output(let text)? = LocalCommandLine.parse("<local-command-stdout>\(line)</local-command-stdout>")
            else { XCTFail(line); continue }
            XCTAssertEqual(ModelConfirmation.parse(text), expected, line)
        }
    }
}

/// The run's model reads the same whichever side named it. A daemon of 0.17.2
/// sends a 2.1.319 receipt's name as it was written, a code span, and its fact
/// wins over the phone's own parse when it is the newer.
@MainActor
final class DaemonModelFactTests: XCTestCase {
    func testABacktickedDaemonFactReadsAsTheModelItNames() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("model-fact-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        let summary = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                #"""
                {"session_uid":"01K8N0AAAAAAAAAAAAAAAAAAAA","session_id":"cc-1",
                 "cwd":"/tmp/alpha","project_label":"alpha","agent":"claude","lifecycle":"live","link":"attached",
                 "last_seq":9,"created_at":"2026-11-09T09:00:00Z","updated_at":"2026-11-09T10:00:00Z","blocked_on":[],
                 "model":{"value":"`Opus 5.5`","seq":9,"source":"command","at":"2026-11-09T10:00:00Z"}}
                """#.utf8))
        model.adoptSummariesForTesting([summary])
        let name = try XCTUnwrap(model.confirmedModel(for: summary.sessionKey)).name
        XCTAssertEqual(name, "Opus 5.5")
    }
}

/// `lastConfirmedModel`: seeded by the SessionStart hook, replaced only by
/// Claude Code's own confirmation lines.
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

        state.ingest(
            try event(
                """
                {"seq":2,"session_id":"cc-1","ts":"2026-08-04T20:01:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Set model to \\u001b[1mSonnet 5\\u001b[22m and saved as your default for new sessions</local-command-stdout>"}}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "Sonnet 5")
    }

    /// A `Kept model as X` line is still a fact about the model in force — it
    /// is the only way this app learns the model after somebody cancels a
    /// confirmation at the Mac.
    func testAKeptModelLineIsAFact() throws {
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

    /// **The bytes a real transcript actually holds.** Claude Code bolds the
    /// model name, so the line arrives wrapped in SGR codes. A parser exercised
    /// only against tidied strings passes here and fails on every real
    /// receipt.
    func testTheKeptLineParsesWithItsRealEscapeCodes() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-08-04T20:00:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Kept model as \\u001b[1mFable 5\\u001b[22m</local-command-stdout>"}}}
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
        await state.settleForTesting()
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
        XCTAssertEqual(state.lastConfirmedEffortSeq, 0)
        state.ingest(
            try stdoutEvent(
                seq: 1,
                "Set effort level to xhigh (saved as your default for new sessions): Deeper"))
        XCTAssertEqual(state.lastConfirmedEffortSeq, 1)
    }

    func testACompactionOutcomeBecomesTheFact() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(try stdoutEvent(seq: 1, "Not enough messages to compact."))
        XCTAssertEqual(state.lastCompactSignal, .notEnoughMessages)
        state.ingest(try stdoutEvent(seq: 2, "Compacted (ctrl+o to see full summary)"))
        XCTAssertEqual(state.lastCompactSignal, .compacted)
    }

    func testAnInvalidEffortLineIsNotAConfirmation() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try stdoutEvent(
                seq: 1, "Invalid argument: bananas. Valid options are: low, medium, high"))
        XCTAssertEqual(state.lastConfirmedEffortSeq, 0)
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
        await state.settleForTesting()
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
        await state.settleForTesting()
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
