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
    /// three were dialogs. `/effort <arg>` is deliberately not here: its
    /// routing is owned by `testEffortSplitsOnBareVersusArguments`, and it is
    /// not an inline command.
    func testMeasuredInlineCommandsPassThrough() {
        for command in ["context", "agents", "focus"] {
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
    /// `Change effort level?` confirmation. Parsed to nothing it leaves the
    /// Effort sheet waiting, and then saying no confirmation arrived — while
    /// the receipt is on screen in the timeline behind it.
    func testAKeptEffortLineIsParsedAsANonChange() {
        let receipt = EffortConfirmation.parse("Kept effort level as high")
        XCTAssertEqual(receipt, .kept(value: "high"))
        XCTAssertEqual(receipt?.isChange, false)
    }

    /// A receipt shape this build has never seen states the level and no
    /// scope, rather than inventing one.
    func testAScopelessSetReceiptCarriesNoScope() {
        XCTAssertEqual(
            EffortConfirmation.parse("Set effort level to high: Comprehensive"),
            .set(value: "high", scope: nil))
    }

    /// The sentences the sheet shows, asserted here because an honesty
    /// contract that cannot be tested is a comment.
    func testEffortSheetSentencesStateExactlyWhatWasObserved() {
        XCTAssertEqual(
            EffortConfirmation.set(value: "max", scope: "this session only").sheetStatus,
            "Claude Code confirmed Maximum effort (this session only).")
        XCTAssertEqual(
            EffortConfirmation.set(
                value: "xhigh", scope: "saved as your default for new sessions"
            ).sheetStatus,
            "Claude Code confirmed Extra high effort (saved as your default for new sessions).")
        XCTAssertEqual(
            EffortConfirmation.kept(value: "high").sheetStatus,
            "Claude Code kept High effort. The level was not changed.")
        XCTAssertEqual(
            EffortConfirmation.set(value: "high", scope: nil).sheetStatus,
            "Claude Code confirmed High effort.")
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
}

/// One provenance line, one shape — origin and age together, never three
/// floating pieces.
@MainActor
final class ProvenanceLineTests: XCTestCase {

    func testBothOriginsRenderWithTheirAge() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let start = ConfirmedModel(
            name: "claude-opus-5[1m]", provenance: .sessionStart,
            at: now.addingTimeInterval(-120))
        XCTAssertEqual(
            ModelSheet.provenanceLine(start, now: now),
            "Confirmed at session start · 2m ago")
        let command = ConfirmedModel(
            name: "Sonnet 5", provenance: .command,
            at: now.addingTimeInterval(-5))
        XCTAssertEqual(
            ModelSheet.provenanceLine(command, now: now),
            "Confirmed by /model · just now")
    }

    /// A cancelled confirmation keeps the same origin words: it is still
    /// `/model` reporting the model in force, and a third phrase would say
    /// more about the request than about the fact.
    func testAKeptReceiptKeepsTheCommandOrigin() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let kept = ConfirmedModel(
            name: "Fable 5", provenance: .command, at: now.addingTimeInterval(-5))
        XCTAssertEqual(
            ModelSheet.provenanceLine(kept, now: now),
            "Confirmed by /model · just now")
    }
}

/// The sheet's correlation rule, as a pure function: **a fence in the event
/// stream, and a match on the value that was asked for.**
final class ModelChangeWatchTests: XCTestCase {
    private func signal(_ outcome: ModelCommandOutcome, _ seq: UInt64) -> ModelCommandSignal {
        ModelCommandSignal(outcome: outcome, seq: seq)
    }

    func testASignalAboveTheFenceForTheRequestedValueCounts() {
        XCTAssertEqual(
            ModelChangeWatch.outcome(
                after: 10, requested: "sonnet", signal: signal(.receipt(.set("Sonnet 5")), 11)),
            .receipt(.set("Sonnet 5")),
            "the request carries an alias and the receipt a display name")
    }

    /// **The backfill hazard.** `SessionState` fills an empty model slot with
    /// the first receipt it finds at any sequence, so without a fence a "load
    /// earlier" replay landing inside the wait is reported as the answer to
    /// this tap.
    func testASignalAtOrBelowTheFenceDoesNot() {
        for seq in [UInt64(10), 3] {
            XCTAssertNil(
                ModelChangeWatch.outcome(
                    after: 10, requested: "sonnet",
                    signal: signal(.receipt(.set("Sonnet 5")), seq)),
                "seq \(seq)")
        }
    }

    /// **Recency is not identity.** A receipt from somebody at the Mac, or from
    /// a second phone, also lands above the fence. Reporting it as this tap's
    /// outcome is the same phantom state wearing a fresher timestamp.
    func testARecentSignalForADifferentValueDoesNotAnswerThisRequest() {
        XCTAssertNil(
            ModelChangeWatch.outcome(
                after: 1, requested: "sonnet", signal: signal(.receipt(.set("Opus 5")), 2)),
            "somebody else switched to Opus; this sheet asked for Sonnet")
        // A `kept` receipt is exempt, and measurably must be: it names the
        // model still in force, so it can never equal the value asked for.
        XCTAssertEqual(
            ModelChangeWatch.outcome(
                after: 1, requested: "sonnet", signal: signal(.receipt(.kept("Opus 5")), 2)),
            .receipt(.kept("Opus 5")),
            "cancelling /model sonnet prints `Kept model as Opus 5`")
        XCTAssertNil(
            ModelChangeWatch.outcome(
                after: 1, requested: "sonnet",
                signal: signal(.notFound("Model 'bananas' not found"), 2)),
            "an error about a value nobody here typed is not this sheet's failure")
    }

    /// The error for *this* sheet's own value is its failure, quoted verbatim.
    func testANotFoundForTheRequestedValueDoesAnswerIt() {
        XCTAssertEqual(
            ModelChangeWatch.outcome(
                after: 1, requested: "bananas",
                signal: signal(.notFound("Model 'bananas' not found"), 2)),
            .notFound("Model 'bananas' not found"))
    }

    /// A `Set model to X` that re-applies the model already in force is still
    /// an answer: identity here is the event, not a change in value.
    func testAnIdenticalReceiptAtAHigherSequenceStillCounts() {
        XCTAssertEqual(
            ModelChangeWatch.outcome(
                after: 5, requested: "opus", signal: signal(.receipt(.set("Opus 5")), 6)),
            .receipt(.set("Opus 5")))
    }

    func testNoSignalDoesNot() {
        XCTAssertNil(ModelChangeWatch.outcome(after: 0, requested: "opus", signal: nil))
    }

    /// A cancelled confirmation for the requested value is reported as the
    /// non-change it is — never as the answer the sheet hoped for.
    func testAKeptReceiptAboveTheFenceIsAKeptReceipt() {
        let outcome = ModelChangeWatch.outcome(
            after: 1, requested: "fable", signal: signal(.receipt(.kept("Fable 5")), 2))
        XCTAssertEqual(outcome, .receipt(.kept("Fable 5")))
        guard case .receipt(let receipt)? = outcome else {
            return XCTFail("\(String(describing: outcome))")
        }
        XCTAssertFalse(receipt.isChange)
    }
}

/// `/model`'s measured outcomes, receipts and the one error alike.
final class ModelCommandOutcomeTests: XCTestCase {
    func testTheMeasuredNotFoundLineIsParsedVerbatim() {
        XCTAssertEqual(
            ModelCommandOutcome.parse("Model \'bananas\' not found"),
            .notFound("Model \'bananas\' not found"))
    }

    func testReceiptsStillWin() {
        XCTAssertEqual(
            ModelCommandOutcome.parse("Kept model as Fable 5"), .receipt(.kept("Fable 5")))
    }

    /// Matched by its measured shape and nothing looser.
    func testUnrelatedOutputIsNotAnOutcome() {
        XCTAssertNil(ModelCommandOutcome.parse("Model bananas not found"))
        XCTAssertNil(ModelCommandOutcome.parse("Something else not found"))
        XCTAssertNil(ModelCommandOutcome.parse("Compacted (ctrl+o to see full summary)"))
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
        XCTAssertEqual(state.lastConfirmedModel?.provenance, .sessionStart)

        state.ingest(
            try event(
                """
                {"seq":2,"session_id":"cc-1","ts":"2026-08-04T20:01:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Set model to \\u001b[1mSonnet 5\\u001b[22m and saved as your default for new sessions</local-command-stdout>"}}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "Sonnet 5")
        XCTAssertEqual(state.lastConfirmedModel?.provenance, .command)
    }

    /// A `Kept model as X` line is still a fact about the model in force — it
    /// is the only way this app learns the model after somebody cancels a
    /// confirmation at the Mac — but it is recorded as the non-change it is.
    func testAKeptModelLineIsAFactButNotAChange() throws {
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
        XCTAssertEqual(
            state.lastConfirmedModel?.provenance, .command,
            "the verb must survive into the fact, or a cancellation reads as a change")
    }

    /// A session-start seed is not an answer to a request: it produces a model
    /// fact for the current-model card and **no** `/model` signal at all, so it
    /// can never satisfy the sheet however recent it is.
    func testASessionStartSeedRaisesNoCommandSignal() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":9,"session_id":"cc-1","ts":"2026-08-04T20:00:00.000Z",
                 "kind":"session_start","source":"hook",
                 "payload":{"hook_event_name":"SessionStart","model":"claude-fable-5"}}
                """))
        XCTAssertEqual(state.lastConfirmedModel?.name, "claude-fable-5")
        XCTAssertNil(state.lastModelCommandSignal, "a seed is not something /model said")
    }

    /// The signal carries the sequence that delivered it — the fence the sheet
    /// compares against.
    func testACommandSignalCarriesItsSequence() throws {
        let state = SessionState(sessionKey: "cc-1")
        state.ingest(
            try event(
                """
                {"seq":12,"session_id":"cc-1","ts":"2026-08-04T20:01:00.000Z",
                 "kind":"user_message","source":"transcript",
                 "payload":{"type":"user","message":{"role":"user","content":
                 "<local-command-stdout>Kept model as Fable 5</local-command-stdout>"}}}
                """))
        XCTAssertEqual(
            state.lastModelCommandSignal,
            ModelCommandSignal(outcome: .receipt(.kept("Fable 5")), seq: 12))
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
        XCTAssertEqual(state.lastConfirmedModel?.provenance, .command)
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
        XCTAssertEqual(state.lastCompactSignal, .notEnoughMessages)
        state.ingest(try stdoutEvent(seq: 2, "Compacted (ctrl+o to see full summary)"))
        XCTAssertEqual(state.lastCompactSignal, .compacted)
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
