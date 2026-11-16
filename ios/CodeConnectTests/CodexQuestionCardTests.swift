import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **X7b (decision 211): a Codex "Plan" question is answered from the phone.** A `request_user_input` card is
/// recognised as a question, answerable without a Mac-side hold, sent with the question family, its recorded
/// answer shown, and named as Codex — each against the daemon-emitted `question-card-wire.json`. The card's
/// request_id is read from the fixture, never hard-coded (terminal's X5-2 re-derives it).
@MainActor
final class CodexQuestionCardTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "question-card-wire", withExtension: "json"),
            "question-card-wire.json is not in the test bundle (ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    /// The scenario's events in seq order, decoded by the app's own `Event` type.
    private func events(_ scenario: String) throws -> [Event] {
        let frames = try XCTUnwrap(try wire()[scenario]?["frames"]?.arrayValue, "\(scenario).frames")
        return try frames.compactMap { try $0["event"]?.decoded(Event.self) }
    }

    private func card(_ scenario: String = "answered") throws -> ApprovalCard {
        try XCTUnwrap(
            try events(scenario).first { $0.kind == .approvalRequest }?.approvalCard, "the request's card")
    }

    private func resolution(_ scenario: String) throws -> CodexResolution {
        try XCTUnwrap(
            try events(scenario).first { $0.kind == .approvalResolved }?.codexResolution, "the resolution")
    }

    /// A backed, connected Codex card, as the live card resolves it.
    private func codexStatus(codex: CodexResolution?) -> QuestionCardStatus {
        QuestionCardStatus.resolve(
            outcome: nil, attempt: nil, isBacked: true, hold: nil, answersQuestions: nil, linkReason: nil,
            codex: codex, resolvesCodexCards: true, agent: .codex)
    }

    // MARK: 1 — recognise

    func testTheCodexCardIsRecognisedAsAQuestion() throws {
        let card = try card()
        XCTAssertEqual(card.toolName, "request_user_input")
        let q = try XCTUnwrap(DecisionCard.questionCard(card: card, agent: .codex))
        XCTAssertEqual(q.questions.map(\.text), ["Which colour?"])
        XCTAssertEqual(q.questions[0].header, "Colour")
        XCTAssertEqual(q.questions[0].options.map(\.label), ["Red", "Blue"])
        XCTAssertFalse(q.questions[0].multiSelect)
        // No preview on a Codex option → Other, not notes.
        XCTAssertTrue(q.questions[0].takesOther)
        XCTAssertFalse(q.questions[0].takesNotes)
        // Agent and tool name must match: a Claude run never claims it, and an unknown agent is never a question.
        XCTAssertNil(DecisionCard.questionCard(card: card, agent: .claude))
        XCTAssertNil(DecisionCard.questionCard(card: card, agent: nil))
    }

    // MARK: 2 — answerable without a hold, under the Codex capability

    func testABackedCodexQuestionIsAnswerableWithoutAHold() throws {
        XCTAssertEqual(codexStatus(codex: nil), .answerable)
        XCTAssertNotEqual(
            QuestionCardStatus.resolve(
                outcome: nil, attempt: nil, isBacked: true, hold: nil, answersQuestions: nil, linkReason: nil,
                codex: nil, resolvesCodexCards: false, agent: .codex),
            .answerable, "not answerable when the daemon cannot retire the card")
        XCTAssertEqual(
            QuestionCardStatus.resolve(
                outcome: nil, attempt: nil, isBacked: false, hold: nil, answersQuestions: nil, linkReason: nil,
                codex: nil, resolvesCodexCards: true, agent: .codex),
            .unavailable, "never answerable with nothing backing it")
    }

    // MARK: 3 — the question family is allowed to send; approvals are not

    func testACodexQuestionAdmitsTheQuestionFamily() throws {
        XCTAssertNil(
            AppModel.decisionMismatch(
                decision: .answers([]), agent: .codex, resolvesCodexCards: true, isQuestion: true))
        XCTAssertNil(
            AppModel.decisionMismatch(
                decision: .decline, agent: .codex, resolvesCodexCards: true, isQuestion: true))
        XCTAssertNotNil(
            AppModel.decisionMismatch(
                decision: .answers([]), agent: .codex, resolvesCodexCards: true, isQuestion: false),
            "a Codex approval card is not answered with the question family")
    }

    // MARK: 4 — the recorded answer shows

    func testTheRecordedAnswerShows() throws {
        guard case .answeredOnPhone(let answers, _) = codexStatus(codex: try resolution("answered")) else {
            return XCTFail("an answered codex question shows its answer")
        }
        XCTAssertEqual(answers.map(\.selected), [[1]], "the chosen option, Blue")
        XCTAssertEqual(codexStatus(codex: try resolution("declined")), .declined)
    }

    // MARK: 4b — a late resolution outranks an unconfirmed send

    func testALateCodexResolutionOutranksAnUnconfirmedSend() throws {
        // A phone answer whose ack was lost records `.sentUnconfirmed`; when the
        // daemon's authoritative resolution arrives it outranks that optimistic
        // attempt — as a Claude outcome does — so the card shows the answer, not "Sent".
        let status = QuestionCardStatus.resolve(
            outcome: nil, attempt: .sentUnconfirmed("ack lost"), isBacked: true, hold: nil,
            answersQuestions: nil, linkReason: nil,
            codex: try resolution("answered"), resolvesCodexCards: true, agent: .codex)
        guard case .answeredOnPhone = status else {
            return XCTFail("the resolution outranks the unconfirmed send, got \(status)")
        }
    }

    // MARK: 5 — the agent is named

    func testTheCodexCardNamesCodex() throws {
        let banner = try XCTUnwrap(
            codexStatus(codex: try resolution("answered")).banner(on: .current, agent: .codex))
        XCTAssertTrue(banner.message.contains("Codex has the answers"), banner.message)
        XCTAssertFalse(banner.message.contains("Claude"), banner.message)
        XCTAssertEqual(QuestionCard.heading(agent: .codex, count: 1), "Codex asks")
        XCTAssertEqual(QuestionCard.heading(agent: .codex, count: 2), "Codex asks 2 questions")
        XCTAssertEqual(QuestionCard.heading(agent: .claude, count: 1), "Claude asks")
    }

    // MARK: 5b — a recorded outcome names the resolving device, not the viewer's

    func testAnAnsweredCodexCardNamesTheResolverNotTheViewer() throws {
        let status = codexStatus(codex: try resolution("answered"))
        let onMac = try XCTUnwrap(status.banner(on: .mac, agent: .codex))
        XCTAssertTrue(onMac.message.contains("from an iPhone"), onMac.message)
        XCTAssertFalse(onMac.message.contains("the Mac"), onMac.message)
        let onPhone = try XCTUnwrap(status.banner(on: .iPhone, agent: .codex))
        XCTAssertTrue(onPhone.message.contains("from the phone"), onPhone.message)
    }

    /// The companion to the above (answered_on, minor 41): a Mac answer is the same
    /// resolution with `answered_on` set to "mac", derived here by changing that one
    /// value. Either device reading it is told the Mac sent the answer — the resolver
    /// is named, not the reader. Must-fail: a read that ignores `answered_on` says a phone.
    func testAMacAnsweredCodexCardNamesTheMacOnEitherView() throws {
        let frames = try XCTUnwrap(try wire()["answered"]?["frames"]?.arrayValue, "answered.frames")
        let eventJSON = try XCTUnwrap(
            frames.lazy.compactMap { frame -> String? in
                guard let event = frame["event"],
                    (try? event.decoded(Event.self))?.kind == .approvalResolved
                else { return nil }
                return event.canonicalJSONString
            }.first, "the answered scenario's resolution frame")
        let macJSON = eventJSON.replacingOccurrences(
            of: #""answered_on":"iphone""#, with: #""answered_on":"mac""#)
        XCTAssertNotEqual(macJSON, eventJSON, "the fixture answers on an iPhone, derived here to a Mac")
        let macEvent = try JSONDecoder().decode(Event.self, from: Data(macJSON.utf8))
        let status = codexStatus(codex: try XCTUnwrap(macEvent.codexResolution))
        for host in [HostDevice.iPhone, HostDevice.mac] {
            let banner = try XCTUnwrap(status.banner(on: host, agent: .codex))
            XCTAssertTrue(
                banner.message.contains("from a Mac"),
                "read on \(host.noun): a Mac answer names the Mac — \(banner.message)")
        }
    }

    func testADeclinedCodexCardDoesNotNameTheViewer() throws {
        let status = codexStatus(codex: try resolution("declined"))
        for host in [HostDevice.mac, HostDevice.iPhone] {
            let banner = try XCTUnwrap(status.banner(on: host, agent: .codex))
            XCTAssertFalse(banner.message.contains("Declined from"), "\(host): \(banner.message)")
        }
    }

    // MARK: 97's constraint — "Other" sends `other` only, no notes beside a chosen option

    func testOtherSendsOtherWithNoNotes() throws {
        let q = try XCTUnwrap(DecisionCard.questionCard(card: try card(), agent: .codex))
        var draft = QuestionDraft(card: q)
        draft.tapOther(of: 0, in: q)
        draft.entries[0].otherText = "Teal"
        let answer = try XCTUnwrap(draft.answer(for: 0, in: q))
        XCTAssertEqual(answer.other, "Teal")
        XCTAssertTrue(answer.selected.isEmpty)
        XCTAssertNil(answer.notes, "a Codex question has no previews, so no notes are offered or sent")
    }
}
