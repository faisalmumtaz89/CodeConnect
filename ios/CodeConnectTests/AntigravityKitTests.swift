import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **An Antigravity run, as the Kit reads it.** A computer that hosts Antigravity lists it among the agents a start can
/// use; ccd's entry for a found agy is `{"agent": "antigravity", "available": true}` (ccd's
/// `antigravity_is_listed_when_an_agy_is_found`), its models and sign-in left out as ccd leaves them out. Its runs are
/// agent-host runs, as a hosted Claude run is: stopped through `claude_interrupt`, sent plain text, and their cards
/// answered Allow or Deny through the host. The recorded frames are ccd's own (`minor-33-wire.json`,
/// `minor-36-wire.json`, `minor-28-wire.json`).
@MainActor
final class AntigravityKitTests: XCTestCase {

    private func wire(_ name: String) throws -> [String: JSONValue] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    /// The recorded `projects` frame, with ccd's Antigravity entry added after Claude's and Codex's when `listed`.
    private func projects(listingAntigravity listed: Bool) throws -> ServerMessage {
        var frame = try XCTUnwrap(try wire("minor-33-wire")["projects"]?.objectValue)
        var agents = try XCTUnwrap(frame["agents"]?.arrayValue)
        if listed { agents.append(.object(["agent": .string("antigravity"), "available": .bool(true)])) }
        frame["agents"] = .array(agents)
        return try JSONDecoder().decode(ServerMessage.self, from: Data(JSONValue.object(frame).canonicalJSONString.utf8))
    }

    /// A computer that has answered with `frame`.
    private func context(_ frame: ServerMessage) throws -> StartContext {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("antigravity-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(
            cache: EventCache(root: root),
            codexSpentLedger: CodexSpentLedger(defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        let advertised = try XCTUnwrap(try wire("minor-28-wire")["hello_ack_capabilities"])
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            try JSONDecoder().decode(Capabilities.self, from: Data(advertised.canonicalJSONString.utf8)), minor: 33)
        model.connection.sendStub = { _ in }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        model.connection.injectForTesting(frame)
        return StartContext(model: model)
    }

    private func card(_ key: String) throws -> ApprovalCard {
        try XCTUnwrap(try wire("minor-36-wire")[key]?.decoded(ApprovalCard.self), "no \(key) in minor-36-wire.json")
    }

    func testTheAgentAndItsEventsSourceDecodeByName() throws {
        let agent = try JSONDecoder().decode(AgentKind.self, from: Data("\"antigravity\"".utf8))
        XCTAssertEqual(agent, .antigravity)
        XCTAssertEqual(agent.displayName, "Antigravity")
        XCTAssertEqual(agent.productName, "Antigravity")
        XCTAssertEqual(AgentKind(.antigravity), .antigravity)
        XCTAssertEqual(try JSONDecoder().decode(EventSource.self, from: Data("\"acp\"".utf8)), .acp)
    }

    /// A computer that lists Antigravity offers it, its one choice the agent's Default; one that does not (every
    /// computer before it) offers Claude Code and Codex as ever, and no Antigravity section.
    func testAStartOffersAntigravityOnlyWhereTheComputerListsIt() throws {
        let listing = try context(try projects(listingAntigravity: true))
        XCTAssertEqual(listing.startAgents, [.claude, .codex, .antigravity])
        XCTAssertEqual(
            listing.models(.antigravity),
            .choices([StartModelChoice(agent: .antigravity, model: nil, label: "Default", detail: nil)]))
        XCTAssertTrue(listing.modelChoices.contains { $0.agent == .antigravity })
        let older = try context(try projects(listingAntigravity: false))
        XCTAssertEqual(older.startAgents, [.claude, .codex])
        XCTAssertFalse(older.modelChoices.contains { $0.agent == .antigravity })
        XCTAssertEqual(StartAgent.withAccounts, [.claude, .codex])
    }

    /// A card on a held request is answered Allow or Deny, never "for this session", even on a card that offers it; one
    /// not held is not answerable; and it is never a question card.
    func testAnAntigravityCardIsAnsweredAllowOrDenyThroughItsHost() throws {
        for key in ["card_offering", "card_not_offering"] {
            XCTAssertEqual(
                DecisionCard.answerSurface(
                    card: try card(key), agent: .antigravity, hold: .held, hookOnlyApprovals: true),
                .allowDeny(allowForSession: false), key)
        }
        XCTAssertEqual(
            DecisionCard.answerSurface(
                card: try card("card_not_offering"), agent: .antigravity, hold: nil, hookOnlyApprovals: true),
            .noneAnswerable)
        XCTAssertNil(DecisionCard.questionCard(card: try card("card_not_offering"), agent: .antigravity))
        XCTAssertNil(AppModel.decisionMismatch(decision: .allow, agent: .antigravity, answersApprovalsByHook: true))
        XCTAssertNil(AppModel.decisionMismatch(decision: .deny, agent: .antigravity, answersApprovalsByHook: true))
        XCTAssertNotNil(
            AppModel.decisionMismatch(
                decision: .optionId("acceptForSession"), agent: .antigravity, answersApprovalsByHook: true))
        XCTAssertNotNil(AppModel.decisionMismatch(decision: .allow, agent: .antigravity, answersApprovalsByHook: false))
    }

    /// Stop follows the hosted rule, and its sentences name Antigravity.
    func testAnAntigravityTurnIsStoppedAsAHostedOne() {
        XCTAssertNil(
            CodexProse.stopUnavailable(
                agent: .antigravity, daemonHonoursStop: true, isLive: true, link: .subscribed, runLink: .attached,
                runningTurn: "turn-1"))
        let tooOld = CodexProse.stopUnavailable(
            agent: .antigravity, daemonHonoursStop: false, isLive: true, link: .subscribed, runLink: .attached,
            runningTurn: "turn-1")
        XCTAssertTrue(tooOld?.contains("too old to stop an Antigravity turn") == true, tooOld ?? "nil")
        XCTAssertNotNil(
            CodexProse.stopUnavailable(
                agent: .antigravity, daemonHonoursStop: true, isLive: true, link: .subscribed, runLink: .attached,
                runningTurn: nil))
    }
}
