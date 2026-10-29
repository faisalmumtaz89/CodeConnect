import XCTest

@testable import CodeConnect

/// A computer whose link is down has said nothing about itself: its
/// capabilities went with the link. Unknown is not old — every surface that
/// would call such a computer too old says what is known instead, which is the
/// link, and a computer that really is too old still says so.
@MainActor
final class UnknownComputerTests: XCTestCase {

    private let uid = "01K8N0CCCCCCCCCCCCCCCCCCCC"

    private func codexSummary() -> SessionSummary {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-10-29T10:00:00Z",
                 "updated_at":"2026-10-29T10:00:00Z","blocked_on":[],
                 "agent":"codex","codex_link":"subscribed"}
                """.utf8))
    }

    /// A Codex session on a computer whose link has dropped.
    private func offlineCodexModel() -> AppModel {
        let model = AppModel(pairing: PairingStore(ephemeral: true), cache: EventCache())
        model.adoptSummariesForTesting([codexSummary()])
        model.connection.setPhaseForTesting(.waiting(until: Date(), reason: "down"))
        return model
    }

    /// Stop on a session whose computer cannot be reached says why: the link.
    func testStopOnAComputerThatIsNotConnectedSaysTheLink() throws {
        let model = offlineCodexModel()
        let link = try XCTUnwrap(model.actionsBlockedReason)

        let reason = try XCTUnwrap(model.stopUnavailable(for: uid))

        XCTAssertEqual(reason, link)
        XCTAssertFalse(reason.contains("too old"), reason)
    }

    /// The composer on such a session says the same.
    func testComposeOnAComputerThatIsNotConnectedSaysTheLink() throws {
        let model = offlineCodexModel()
        let link = try XCTUnwrap(model.actionsBlockedReason)

        let reason = try XCTUnwrap(model.composeUnavailable(for: uid))

        XCTAssertEqual(reason, link)
        XCTAssertFalse(reason.contains("too old"), reason)
    }

    private let link = "Could not connect to the server. Retrying automatically."

    private func heldQuestion() throws -> (ApprovalItem, QuestionCard) {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-21-wire", withExtension: "json"))
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        let request = try XCTUnwrap(try wire["held_request"]?.decoded(Event.self))
        let item = try XCTUnwrap(
            TimelineBuilder.build([request]).compactMap { item -> ApprovalItem? in
                if case .approval(let approval) = item.content { return approval }
                return nil
            }.first)
        return (item, try XCTUnwrap(QuestionCard(card: item.card)))
    }

    /// A question from such a computer, on its card and on its timeline row,
    /// says the link and offers nothing to answer.
    func testAQuestionOnAComputerThatIsNotConnectedSaysTheLink() throws {
        let (item, card) = try heldQuestion()
        let unknown = DaemonProfile.unknown.knownAnswersQuestions

        let onCard = QuestionCardStatus.resolve(
            outcome: nil, attempt: nil, isBacked: true, hold: .held,
            answersQuestions: unknown, linkReason: link)
        let onTimeline = QuestionCardStatus.recorded(
            item, answersQuestions: unknown, linkReason: link)

        XCTAssertFalse(onCard.isAnswerable)
        XCTAssertEqual(onCard.banner?.message, link)
        XCTAssertEqual(onTimeline.summary(of: card), link)
        XCTAssertFalse(onCard.banner?.message.contains("too old") ?? true)
    }

    /// A Codex card from such a computer says the link: not the caveat for an
    /// old daemon, and not that the card came without its options.
    func testACodexCardOnAComputerThatIsNotConnectedSaysTheLink() {
        let unknown = DaemonProfile.unknown
        let reason = DecisionCardView.codexUnanswerableReason(
            isKnown: unknown.isConnected, caveat: unknown.codexAnswerCaveat, isVerified: true,
            linkReason: link)

        XCTAssertEqual(reason, link)
    }

    /// A computer that is known and really too old still says so, in the
    /// computer's words.
    func testAComputerThatIsTooOldStillSaysSo() {
        let old = DaemonProfile(
            protocolVersion: Wire.protocolVersion, protocolMinor: 18, capabilities: Capabilities())
        XCTAssertEqual(old.knownAnswersQuestions, false)
        let question = QuestionCardStatus.resolve(
            outcome: nil, attempt: nil, isBacked: true, hold: .held,
            answersQuestions: old.knownAnswersQuestions, linkReason: nil)
        XCTAssertEqual(question, .macTooOld)
        XCTAssertTrue(
            question.banner?.message.contains("This computer's CodeConnect is too old") ?? false)
        let codex = DecisionCardView.codexUnanswerableReason(
            isKnown: old.isConnected, caveat: old.codexAnswerCaveat, isVerified: true,
            linkReason: nil)
        XCTAssertTrue(codex.hasPrefix("This computer's CodeConnect is too old"), codex)

        let model = AppModel(pairing: PairingStore(ephemeral: true), cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(Capabilities(), minor: 16)
        model.adoptSummariesForTesting([codexSummary()])
        XCTAssertEqual(
            model.stopUnavailable(for: uid),
            "This computer's CodeConnect is too old to stop a Codex turn. Update it.")
        XCTAssertEqual(
            model.composeUnavailable(for: uid),
            "This computer's CodeConnect is too old to carry a message to Codex. Update it.")
    }
}
