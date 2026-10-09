import XCTest

@testable import CodeConnect

/// Claude's `AskUserQuestion`, against the bytes a minor-21 daemon sends.
///
/// `minor-21-wire.json` is emitted by ccd's own test
/// (`the_minor_21_wire_fixture_is_what_this_build_emits`) and checked in at
/// `fixtures/claude/`; the copy in this bundle must stay byte-identical to it.
/// Its question is a live Claude 2.1.286 session's four: a single choice,
/// several choices, "Other", and previews.
@MainActor
final class QuestionCardTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-21-wire", withExtension: "json"),
            "minor-21-wire.json is not in the test bundle (checked in at "
                + "ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func event(_ name: String) throws -> Event {
        try XCTUnwrap(try wire()[name]?.decoded(Event.self), "\(name) decodes as an Event")
    }

    private func heldCard() throws -> ApprovalCard {
        try XCTUnwrap(try event("held_request").approvalCard)
    }

    private func profile(minor: UInt32, questionCard: Bool) -> DaemonProfile {
        DaemonProfile(
            protocolVersion: 1, protocolMinor: minor,
            capabilities: Capabilities(extra: ["question_card": .bool(questionCard)]))
    }

    // MARK: The daemon's own bytes

    func testTheFiveMinor21EventsDecodeThroughTheAppsTypes() throws {
        let held = try event("held_request")
        XCTAssertEqual(held.kind, .approvalRequest)
        let card = try heldCard()
        XCTAssertTrue(card.verification.hashMatchesDisplayText, "the real card's hash holds")
        XCTAssertTrue(card.verification.renderMatchesDisplayText, "and its questions are the hashed ones")
        XCTAssertEqual(card.questionHold, .held)
        let questions = try XCTUnwrap(QuestionCard(card: card))
        XCTAssertEqual(
            questions.questions.map(\.header), ["Snapshots", "Checks", "Archive", "Layout"])
        XCTAssertEqual(questions.questions.map(\.multiSelect), [false, true, false, false])
        XCTAssertEqual(questions.questions[2].options.map(\.label), ["Keep on disk (Recommended)", "Commit it"])
        XCTAssertEqual(
            questions.questions[3].options.map(\.preview),
            ["+------+\n| A    |\n+------+\n| B    |\n+------+", "+---+---+\n| A | B |\n+---+---+"])

        let phone = try XCTUnwrap(try event("answered_from_phone").approvalOutcome)
        XCTAssertEqual(phone.resolvedBy, .phone)
        XCTAssertEqual(phone.appliedVia, .hookReturn)
        XCTAssertEqual(
            phone.decision,
            .answers([
                QuestionAnswer(selected: [0]), QuestionAnswer(selected: [0, 2]),
                QuestionAnswer(other: "Archive it under docs/plans, café ☕"),
                QuestionAnswer(selected: [1], notes: "wider screens only"),
            ]))

        let ended = try event("hold_ended")
        XCTAssertEqual(ended.kind, .questionHold)
        XCTAssertEqual(ended.questionHoldChange?.requestID, card.requestID)
        XCTAssertEqual(ended.questionHoldChange?.hold, .ended)

        XCTAssertEqual(try event("at_mac_request").approvalCard?.questionHold, .atMac)

        let mac = try XCTUnwrap(try event("answered_at_mac").approvalOutcome)
        XCTAssertEqual(mac.resolvedBy, .local)
        XCTAssertEqual(mac.decision, .answers([]))
    }

    /// The card's own `question_hold`, then the event that changes it — and a
    /// card raised after that event keeps its own.
    func testTheLatestHoldGovernsTheCard() throws {
        let held = try event("held_request")
        let ended = try event("hold_ended")
        func hold(_ events: [Event]) -> [QuestionHold?] {
            TimelineBuilder.build(events).compactMap {
                if case .approval(let item) = $0.content { return item.questionHold }
                return nil
            }
        }
        XCTAssertEqual(hold([held]), [.held])
        XCTAssertEqual(hold([held, ended]), [.ended], "a later question_hold overrides the card")

        var raised = try XCTUnwrap(try wire()["at_mac_request"]?.objectValue)
        raised["seq"] = .int(5)
        let reraised = try XCTUnwrap(JSONValue.object(raised).decoded(Event.self))
        XCTAssertEqual(
            hold([held, ended, reraised]), [.ended, .atMac],
            "a card raised after the event keeps its own hold")

        let answered = try event("answered_from_phone")
        let item = try XCTUnwrap(
            TimelineBuilder.build([held, answered]).compactMap { row -> ApprovalItem? in
                if case .approval(let item) = row.content { return item }
                return nil
            }.first)
        XCTAssertFalse(item.isPending, "an answered question is no longer waiting on anyone")
    }

    /// One status per hold, and every ending the wire can carry.
    func testTheCardsStateForEachHoldAndEnding() throws {
        func status(
            hold: QuestionHold?, outcome: AnswerOutcome? = nil, attempt: AnswerAttempt? = nil,
            backed: Bool = true, capable: Bool = true
        ) -> QuestionCardStatus {
            QuestionCardStatus.resolve(
                outcome: outcome, attempt: attempt, isBacked: backed, hold: hold,
                answersQuestions: capable)
        }
        XCTAssertEqual(status(hold: .held), .answerable)
        XCTAssertEqual(status(hold: .atMac), .askingAtMac)
        XCTAssertEqual(status(hold: .ended), .answerAtMac)
        XCTAssertEqual(status(hold: nil), .answerAtMac, "no stated hold is never answerable")
        XCTAssertEqual(status(hold: .unrecognised("later")), .answerAtMac)
        XCTAssertEqual(status(hold: .held, capable: false), .macTooOld)
        XCTAssertEqual(status(hold: .held, backed: false), .unavailable)

        let phone = try XCTUnwrap(try event("answered_from_phone").approvalOutcome)
        guard case .answers(let sent) = phone.decision else { return XCTFail("answers") }
        XCTAssertEqual(status(hold: .held, outcome: phone), .answeredOnPhone(sent))
        XCTAssertEqual(
            status(hold: .held, attempt: .applied(phone)), .answeredOnPhone(sent),
            "this phone's own confirmed answer")
        let mac = try XCTUnwrap(try event("answered_at_mac").approvalOutcome)
        XCTAssertEqual(status(hold: .held, outcome: mac), .answeredAtMac)
        XCTAssertEqual(
            status(hold: .held, attempt: .answeredAtKeyboard("x")), .answeredAtMac)
        XCTAssertEqual(
            status(hold: .held, attempt: .rejected("refused")), .answerable,
            "a refusal leaves the question answerable")
        XCTAssertFalse(
            [QuestionCardStatus.askingAtMac, .answerAtMac, .macTooOld, .answeredAtMac, .unavailable]
                .contains { $0.isAnswerable })
    }

    /// Every ending the daemon records for a question that nobody answered
    /// from this phone, or that it only inferred, reads as closed: the question
    /// leaving the Mac, and an older phone's Allow, which a question cannot
    /// carry. (No age and no newer prompt ever closes a question card.) Only an observed decline from this phone
    /// says declined: on a current Mac a decline is unconfirmed, and an older
    /// Mac recorded the Escape it typed for an older phone's Deny.
    func testOnlyAnObservedEndingIsStated() throws {
        let mac = try XCTUnwrap(try wire()["answered_at_mac"]?["payload"]?.objectValue)
        func ending(_ changes: [String: JSONValue], attempt: AnswerAttempt? = nil) throws
            -> QuestionCardStatus
        {
            let outcome = try JSONDecoder().decode(
                AnswerOutcome.self,
                from: JSONEncoder().encode(JSONValue.object(mac.merging(changes) { $1 })))
            return QuestionCardStatus.resolve(
                outcome: outcome, attempt: attempt, isBacked: true, hold: .ended,
                answersQuestions: true)
        }
        let deny: JSONValue = .object(["type": .string("deny")])
        let decline: JSONValue = .object(["type": .string("decline")])
        XCTAssertEqual(
            try ending(["decision": decline, "inferred": .bool(true)]), .closed,
            "left the Mac, by Escape or the session ending")
        XCTAssertEqual(
            try ending(["decision": deny, "inferred": .bool(true)]), .closed, "the prompt went")
        XCTAssertEqual(
            try ending(["inferred": .bool(true)]), .closed, "an inferred answer is not stated")
        XCTAssertEqual(
            try ending([
                "resolved_by": .string("phone"), "decision": .object(["type": .string("allow")]),
            ]), .closed, "an older phone's Allow")
        XCTAssertEqual(
            try ending(["resolved_by": .string("phone"), "decision": decline]), .declined)
        XCTAssertEqual(
            try ending(["resolved_by": .string("phone"), "decision": deny]), .declined,
            "an older Mac's record of the Escape it typed for a Deny")
        XCTAssertEqual(try ending([:]), .answeredAtMac)
        XCTAssertEqual(
            try ending(
                ["decision": decline, "inferred": .bool(true)],
                attempt: .answeredAtKeyboard("unknown or already-resolved request")),
            .closed, "a tap refused as already resolved shows what was recorded")
    }

    /// Notes go to the daemon as typed: it trims them as Claude's dialog does,
    /// and any other trim here would change bytes the keyboard keeps.
    func testNotesAreSentAsTyped() throws {
        let card = try XCTUnwrap(QuestionCard(card: try heldCard()))
        var draft = QuestionDraft(card: card)
        draft.tap(option: 1, of: 3, in: card)
        let typed = "\u{0085}wider screens only\u{200B} "
        draft.entries[3].notes = typed
        XCTAssertEqual(draft.answer(for: 3, in: card)?.notes, typed)
        draft.entries[3].notes = ""
        XCTAssertNil(draft.answer(for: 3, in: card)?.notes, "no notes typed, none sent")
    }

    // MARK: The answer

    /// The same taps as the keyboard run produce the exact `decision` the
    /// daemon recorded for the phone.
    func testTheEncodedAnswerIsTheFixturesDecision() throws {
        let card = try XCTUnwrap(QuestionCard(card: try heldCard()))
        var draft = QuestionDraft(card: card)
        draft.tap(option: 0, of: 0, in: card)
        draft.tap(option: 0, of: 1, in: card)
        draft.tap(option: 2, of: 1, in: card)
        draft.tapOther(of: 2, in: card)
        draft.entries[2].otherText = "Archive it under docs/plans, café ☕"
        draft.tap(option: 1, of: 3, in: card)
        draft.entries[3].notes = "wider screens only"
        let answers = try XCTUnwrap(draft.answers(in: card))

        let encoded = try JSONEncoder().encode(AnswerDecision.answers(answers))
        let fixture = try XCTUnwrap(try wire()["answered_from_phone"]?["payload"]?["decision"])
        XCTAssertEqual(try JSONDecoder().decode(JSONValue.self, from: encoded), fixture)
        XCTAssertEqual(
            card.choices(of: answers[1], for: 1), ["Unit tests", "Soak test"],
            "several choices, each on its own, in the order picked")
        XCTAssertEqual(
            card.choices(of: answers[2], for: 2), ["Archive it under docs/plans, café ☕"],
            "an \"Other\" answer whole, its comma included")

        XCTAssertEqual(
            String(decoding: try JSONEncoder().encode(AnswerDecision.decline), as: UTF8.self),
            #"{"type":"decline"}"#)
    }

    func testSubmitIsOfferedOnlyOnceEveryQuestionHasAnAnswer() throws {
        let card = try XCTUnwrap(QuestionCard(card: try heldCard()))
        var draft = QuestionDraft(card: card)
        XCTAssertNil(draft.answers(in: card))
        draft.tap(option: 1, of: 0, in: card)
        draft.tap(option: 1, of: 1, in: card)
        draft.tap(option: 1, of: 3, in: card)
        XCTAssertNil(draft.answers(in: card), "three of four is not an answer")
        draft.tapOther(of: 2, in: card)
        draft.entries[2].otherText = "  \n "
        XCTAssertNil(draft.answers(in: card), "an \"Other\" with nothing typed is not an answer")
        draft.entries[2].otherText = "Neither"
        XCTAssertNotNil(draft.answers(in: card))
        draft.tap(option: 1, of: 1, in: card)
        XCTAssertNil(draft.answers(in: card), "untapping the only choice unanswers the question")
    }

    /// The preview layout has no "Type something" at the Mac, so the card
    /// offers no "Other" there and the daemon would refuse one.
    func testAQuestionWithPreviewsTakesNoOther() throws {
        let card = try XCTUnwrap(QuestionCard(card: try heldCard()))
        XCTAssertEqual(card.questions.map(\.takesOther), [true, true, true, false])
        var draft = QuestionDraft(card: card)
        draft.tapOther(of: 3, in: card)
        XCTAssertFalse(draft.entries[3].otherChosen)
    }

    func testSeveralChoicesKeepTheOrderTheyWereTapped() throws {
        let card = try XCTUnwrap(QuestionCard(card: try heldCard()))
        var draft = QuestionDraft(card: card)
        draft.tap(option: 2, of: 1, in: card)
        draft.tap(option: 0, of: 1, in: card)
        XCTAssertEqual(draft.answer(for: 1, in: card)?.selected, [2, 0])
        draft.tap(option: 2, of: 1, in: card)
        draft.tap(option: 2, of: 1, in: card)
        XCTAssertEqual(draft.answer(for: 1, in: card)?.selected, [0, 2])
        draft.tapOther(of: 1, in: card)
        draft.entries[1].otherText = "Fuzz"
        XCTAssertEqual(
            draft.answer(for: 1, in: card), QuestionAnswer(selected: [0, 2], other: "Fuzz"),
            "several choices take indices plus one \"Other\"")

        // A single choice is exactly one of an index and "Other".
        draft.tapOther(of: 0, in: card)
        draft.entries[0].otherText = "Mine"
        draft.tap(option: 1, of: 0, in: card)
        XCTAssertEqual(draft.answer(for: 0, in: card), QuestionAnswer(selected: [1]))
    }

    func testNotesAreOfferedAndSentOnlyWherePreviewsExist() throws {
        let card = try XCTUnwrap(QuestionCard(card: try heldCard()))
        XCTAssertEqual(card.questions.map(\.takesNotes), [false, false, false, true])
        var draft = QuestionDraft(card: card)
        draft.tap(option: 0, of: 0, in: card)
        draft.entries[0].notes = "not for this one"
        draft.tap(option: 0, of: 3, in: card)
        draft.entries[3].notes = "wider screens only"
        XCTAssertNil(draft.answer(for: 0, in: card)?.notes)
        XCTAssertEqual(draft.answer(for: 3, in: card)?.notes, "wider screens only")
    }

    // MARK: No Allow, ever

    /// Allow and an option would pick answers nobody chose. Under no capability
    /// or hold is either offered or sent for Claude's question.
    func testAQuestionCardNeverOffersOrSendsAllowOrAnOption() throws {
        let card = try heldCard()
        XCTAssertEqual(
            DecisionCardView.answerSurface(
                card: card, agent: .claude, hold: .held, hookOnlyApprovals: true),
            .noneAnswerable, "the fallback layout is read-only for a question")
        XCTAssertNotNil(DecisionCardView.questionCard(card: card, agent: .claude))
        XCTAssertNil(DecisionCardView.questionCard(card: card, agent: nil), "unknown agent")
        XCTAssertNil(DecisionCardView.questionCard(card: card, agent: .codex))

        for (minor, capable) in [(20, false), (20, true), (21, false), (21, true)] {
            let answers = profile(minor: UInt32(minor), questionCard: capable).answersQuestions
            XCTAssertEqual(answers, minor >= 21 && capable, "minor \(minor), question_card \(capable)")
            for decision: AnswerDecision in [
                .allow, .deny, .option(index: 1), .optionId("accept"), .text("yes"),
            ] {
                XCTAssertNotNil(
                    AppModel.decisionMismatch(
                        decision: decision, agent: .claude, isQuestion: true,
                        answersQuestions: answers),
                    "\(decision) refused on a question, minor \(minor), question_card \(capable)")
            }
            for decision: AnswerDecision in [.answers([]), .decline] {
                XCTAssertEqual(
                    AppModel.decisionMismatch(
                        decision: decision, agent: .claude, isQuestion: true,
                        answersQuestions: answers) == nil,
                    answers, "\(decision) sent only to a daemon that takes it")
                XCTAssertNotNil(
                    AppModel.decisionMismatch(
                        decision: decision, agent: .claude, isQuestion: false,
                        answersQuestions: answers),
                    "\(decision) never on a card that is not a question")
                XCTAssertNotNil(
                    AppModel.decisionMismatch(decision: decision, agent: .codex))
            }
        }
        XCTAssertNil(
            AppModel.decisionMismatch(
                decision: .allow, agent: .claude, answersApprovalsByHook: true),
            "an ordinary approval still takes Allow from a daemon that answers it by its hook")
    }

    func testTheMacAnsweringFirstReadsAsAnsweredAtTheMac() {
        let reason =
            "this question was answered at the Mac first, so the phone's answer was not used"
        guard case .answeredAtKeyboard = AppModel.classify(rejection: reason) else {
            return XCTFail("the Mac's answer won; it is not a refusal to retry")
        }
    }

    /// The staged question is the daemon's own events with longer words: every
    /// state's frames decode, its card verifies, the hold is the state's, and
    /// the event after the card is the daemon's.
    func testTheStagedQuestionIsTheDaemonsWithLongerWords() throws {
        let expected: [(QuestionFixtures.State, QuestionHold?, EventKind?)] = [
            (.held, .held, nil), (.atMac, .atMac, nil), (.oldMac, nil, nil),
            (.ended, .held, .questionHold), (.answered, .held, .approvalResolved),
        ]
        for (state, hold, later) in expected {
            let events: [Event] = QuestionFixtures.frames(state: state).compactMap {
                if case .event(let event) = $0 { return event }
                return nil
            }
            XCTAssertEqual(events.count, later == nil ? 1 : 2, "\(state)")
            let card = try XCTUnwrap(events.first?.approvalCard, "\(state)")
            XCTAssertTrue(card.verification.hashMatchesDisplayText, "\(state)")
            XCTAssertEqual(card.questionHold, hold, "\(state)")
            XCTAssertEqual(card.requestID, try heldCard().requestID, "\(state)")
            XCTAssertEqual(
                try XCTUnwrap(QuestionCard(card: card)).questions[2].text,
                "Where should the plan archive go — the café ☕ notes included?", "\(state)")
            XCTAssertEqual(events.dropFirst().first?.kind, later, "\(state)")
        }
    }

    func testTheTimelineRowNamesTheQuestion() throws {
        let card = try heldCard()
        XCTAssertEqual(
            ToolSummary.principalArgument(tool: card.toolName, input: card.toolInput),
            "How should snapshots be stored?")
    }

    // MARK: What the timeline says about a question

    /// The question card the timeline holds after `names`, through the builder.
    private func recorded(_ names: [String], capable: Bool = true) throws
        -> (QuestionCardStatus, String?)
    {
        let items = TimelineBuilder.build(try names.map(event))
        let item = try XCTUnwrap(
            items.compactMap { item -> ApprovalItem? in
                if case .approval(let approval) = item.content { return approval }
                return nil
            }.last)
        let card = try XCTUnwrap(QuestionCard(card: item.card))
        let status = QuestionCardStatus.recorded(item, answersQuestions: capable)
        return (status, status.summary(of: card))
    }

    /// The row and the card read one status: the row says what the card's
    /// banner says, and an answer from the phone is what was chosen.
    func testTheTimelineStatesWhatTheCardStates() throws {
        XCTAssertNil(try recorded(["held_request"]).1, "answerable: the row offers Answer instead")
        XCTAssertEqual(try recorded(["at_mac_request"]).1, "Asking at the Mac")
        XCTAssertEqual(try recorded(["held_request", "hold_ended"]).1, "Answer at the Mac")
        XCTAssertEqual(try recorded(["held_request"], capable: false).1, "Answer at the Mac")
        XCTAssertEqual(try recorded(["at_mac_request", "answered_at_mac"]).1, "Answered at the Mac")
        XCTAssertEqual(
            try recorded(["held_request", "answered_from_phone"]).1,
            "Answered: Dedupe with hardlinks (Recommended) · Unit tests, Soak test · "
                + "Archive it under docs/plans, café ☕ · Side by side")
    }

    /// A question that left the Mac with nobody seen to answer it is closed —
    /// never "answered at the keyboard", which states an answer nobody saw.
    func testAnInferredEndingIsClosedOnTheTimelineToo() throws {
        var resolved = try XCTUnwrap(try wire()["answered_at_mac"]?.objectValue)
        var payload = try XCTUnwrap(resolved["payload"]?.objectValue)
        payload["inferred"] = .bool(true)
        payload["decision"] = .object(["type": .string("decline")])
        resolved["payload"] = .object(payload)
        let ended = try XCTUnwrap(JSONValue.object(resolved).decoded(Event.self))
        let items = TimelineBuilder.build([try event("at_mac_request"), ended])
        let item = try XCTUnwrap(
            items.compactMap { item -> ApprovalItem? in
                if case .approval(let approval) = item.content { return approval }
                return nil
            }.first)
        let status = QuestionCardStatus.recorded(item, answersQuestions: true)
        XCTAssertEqual(status, .closed)
        XCTAssertEqual(status.summary(of: try XCTUnwrap(QuestionCard(card: item.card))), "Closed")
    }

    // MARK: One question, one entry

    /// A PreToolUse or PostToolUse hook event for one call, with the fields the
    /// builder reads from it.
    private func toolEvent(_ kind: String, seq: Int, id: String, tool: String) throws -> Event {
        try XCTUnwrap(
            JSONValue.object([
                "seq": .int(Int64(seq)), "session_uid": .string("01K1B3XQ8ZC0DE5FGH7JKMNPQR"),
                "session_id": .string("cc-1"), "ts": .string("2026-10-03T00:00:0\(seq).000Z"),
                "kind": .string(kind), "source": .string("hook"),
                "payload": .object([
                    "tool_name": .string(tool), "tool_use_id": .string(id),
                    "tool_input": .object([:]), "duration_ms": .int(0),
                ]),
            ]).decoded(Event.self))
    }

    func testAQuestionIsOneEntryNotAToolRowAndACard() throws {
        let id = try heldCard().requestID
        let items = TimelineBuilder.build([
            try toolEvent("tool_call", seq: 1, id: id, tool: "AskUserQuestion"),
            try event("held_request"),
            try event("answered_from_phone"),
            try toolEvent("tool_result", seq: 6, id: id, tool: "AskUserQuestion"),
        ])
        let tools = items.filter {
            if case .tool = $0.content { return true }
            return false
        }
        XCTAssertEqual(tools.count, 0, "no tool row for the question's own call")
        XCTAssertEqual(items.count, 1, "the card alone")
    }

    /// Only a question folds: an ordinary approved call keeps its own row.
    func testAnOrdinaryToolKeepsItsRowBesideItsCard() throws {
        var request = try XCTUnwrap(try wire()["held_request"]?.objectValue)
        var payload = try XCTUnwrap(request["payload"]?.objectValue)
        var card = try XCTUnwrap(payload["card"]?.objectValue)
        card["tool_name"] = .string("Bash")
        payload["card"] = .object(card)
        request["payload"] = .object(payload)
        let bashCard = try XCTUnwrap(JSONValue.object(request).decoded(Event.self))
        let id = try heldCard().requestID
        let items = TimelineBuilder.build([
            try toolEvent("tool_call", seq: 1, id: id, tool: "Bash"), bashCard,
        ])
        XCTAssertEqual(items.count, 2)
    }
}
