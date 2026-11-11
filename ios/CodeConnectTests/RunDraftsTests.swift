import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// What the reader types on a run stays with the run, not with the view that shows it: the reply, a Deny's reason
/// and a question's answers outlive the list being sent again and the run moving on, and a request's go with it.
@MainActor
final class RunDraftsTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    private let command = """
        {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:00.000Z",
         "kind":"approval_request","source":"hook","payload":{"card":{
           "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
           "tool_input":{"command":"ls"},"display_text":"Bash"}}}
        """
    private let question = """
        {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:01.000Z",
         "kind":"approval_request","source":"hook","payload":{"card":{
           "request_id":"toolu_2","payload_hash":"def","tool_name":"AskUserQuestion",
           "tool_input":{"questions":[{"question":"Which branch?","header":"Branch","multiSelect":false,
             "options":[{"label":"main"},{"label":"qa"}]}]},"display_text":"AskUserQuestion"}}}
        """
    private let message = """
        {"seq":3,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:02.000Z",
         "kind":"user_message","source":"transcript","payload":{"message":{"content":"step 3"}}}
        """
    private let commandResolved = """
        {"seq":3,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:02.000Z",
         "kind":"approval_resolved","source":"daemon","payload":{
           "request_id":"toolu_1","session_id":"cc-1","decision":{"type":"deny"},
           "resolved_by":"local","applied_via":"send_keys",
           "resolved_at":"2026-08-18T09:00:02.000Z"}}
        """

    private func list(lastSeq: Int) throws -> ServerMessage {
        .sessions([
            try JSONDecoder().decode(
                SessionSummary.self,
                from: Data(
                    """
                    {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1",
                     "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                     "last_seq":\(lastSeq),"created_at":"2026-08-18T09:00:00.000Z",
                     "updated_at":"2026-08-18T09:00:00.000Z","blocked_on":[],"agent":"claude"}
                    """.utf8))
        ])
    }

    private func settle(_ model: AppModel) async throws {
        try await XCTUnwrap(model.statesForTesting["u-1"]).settleForTesting()
    }

    /// A run with a command and a question waiting, as the screen shows it, and words typed for each and for the
    /// reply.
    private func typedOnARunWaiting() async throws -> (AppModel, command: ApprovalItem, question: ApprovalItem,
        answers: QuestionDraft)
    {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.injectForTesting(.event(try event(command)))
        model.connection.injectForTesting(.event(try event(question)))
        model.connection.injectForTesting(try list(lastSeq: 2))
        try await settle(model)
        let waiting = try XCTUnwrap(model.shown(for: "u-1")?.pendingApprovals)
        let command = try XCTUnwrap(waiting.first { $0.card.requestID == "toolu_1" })
        let question = try XCTUnwrap(waiting.first { $0.card.requestID == "toolu_2" })
        let card = try XCTUnwrap(QuestionCard(card: question.card))

        let drafts = try XCTUnwrap(model.drafts(for: "u-1"))
        drafts.reply = "keep me ✓ 日本"
        drafts.unanswered = UnansweredSend(text: "sent once", sentAt: Date(timeIntervalSince1970: 1))
        drafts.reasons[command.id] = "Nein — use the qa branch"
        var answers = QuestionDraft(card: card)
        answers.tap(option: 1, of: 0, in: card)
        drafts.answers[question.id] = answers
        return (model, command, question, answers)
    }

    /// The list sent again and the run moving on change nothing of what was typed: whatever shows the run next reads
    /// the same words.
    func testWhatWasTypedOutlivesTheRunMovingOn() async throws {
        let (model, command, question, answers) = try await typedOnARunWaiting()
        let first = try XCTUnwrap(model.drafts(for: "u-1"))

        model.connection.injectForTesting(try list(lastSeq: 2))
        model.connection.injectForTesting(.event(try event(message)))
        model.connection.injectForTesting(try list(lastSeq: 3))
        try await settle(model)

        let again = try XCTUnwrap(model.drafts(for: "u-1"))
        XCTAssertTrue(again === first, "one run, one place for its words")
        XCTAssertEqual(again.reply, "keep me ✓ 日本")
        XCTAssertEqual(again.unanswered?.text, "sent once")
        XCTAssertEqual(again.reasons[command.id], "Nein — use the qa branch", "the command still waits")
        XCTAssertEqual(again.answers[question.id], answers, "the question still waits")
    }

    /// Once a request no longer waits, what was typed for it goes; what was typed for the rest stays.
    func testARequestsWordsGoWithIt() async throws {
        let (model, command, question, answers) = try await typedOnARunWaiting()

        model.connection.injectForTesting(.event(try event(commandResolved)))
        try await settle(model)

        XCTAssertEqual(model.shown(for: "u-1")?.pendingApprovals.map(\.id), [question.id])
        let drafts = try XCTUnwrap(model.drafts(for: "u-1"))
        XCTAssertNil(drafts.reasons[command.id], "the command was answered")
        XCTAssertEqual(drafts.answers[question.id], answers)
        XCTAssertEqual(drafts.reply, "keep me ✓ 日本")
    }
}
