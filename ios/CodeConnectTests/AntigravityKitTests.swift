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

    /// Only Claude's card offers a Deny with a reason: agy takes a rejection with no words, and Codex is answered with
    /// its own options, so a reason typed for either would reach no one.
    func testOnlyAClaudeCardTakesADenysReason() {
        XCTAssertTrue(AgentKind.claude.takesDenyReason)
        XCTAssertFalse(AgentKind.antigravity.takesDenyReason)
        XCTAssertFalse(AgentKind.codex.takesDenyReason)
        XCTAssertFalse(AgentKind.unsupported("gemini").takesDenyReason)
    }

    /// A recorded hosted summary (`minor-46-wire.json`) as an Antigravity run's, with `changes` applied: the menus,
    /// facts and label in the shapes ccd's `an_antigravity_runs_settings_are_agys_own` emits for one.
    private func summary(_ key: String, _ changes: [String: JSONValue]) throws -> SessionSummary {
        var frame = try XCTUnwrap(try wire("minor-46-wire")[key]?.objectValue)
        frame["agent"] = .string("antigravity")
        for (field, value) in changes { frame[field] = value }
        return try JSONDecoder().decode(SessionSummary.self, from: Data(JSONValue.object(frame).canonicalJSONString.utf8))
    }

    private func fact(_ value: String) -> JSONValue { .object(["value": .string(value), "seq": .int(4)]) }

    /// A live Antigravity run with menus draws them as a hosted run's: agy's families, the chosen one's efforts, and
    /// agy's modes, each selected from its facts. Ended, its model reads by its family's name.
    func testAnAntigravityRunsSettingsAreDrawnFromItsMenus() throws {
        let model: (String, String, [String]) -> JSONValue = { value, label, efforts in
            .object(["value": .string(value), "label": .string(label), "efforts": .array(efforts.map { .string($0) })])
        }
        let menus: JSONValue = .object([
            "models": .array([
                model("gemini-4-neon", "Gemini 4 Neon", ["high", "xhigh", "medium", "low"]),
                model("gemini-3.8-flash", "Gemini 3.8 Flash", ["high", "medium", "low"]),
            ]),
            "modes": .array(
                [("default", "Default"), ("accept-edits", "Accept edits"), ("auto", "Auto"), ("plan", "Plan")].map {
                    .object(["value": .string($0.0), "label": .string($0.1)])
                }),
            "selected_model": .string("gemini-4-neon"),
        ])
        let live = try summary(
            "session_hosted_live",
            ["menus": menus, "model": fact("gemini-4-neon"), "effort": fact("high"), "permission_mode": fact("plan")])
        guard case .hostedLive(let settings) = RunSettings.of(summary: live) else {
            return XCTFail("an Antigravity run with menus draws no settings: \(RunSettings.of(summary: live))")
        }
        XCTAssertEqual(settings.models.map(\.value), ["gemini-4-neon", "gemini-3.8-flash"])
        XCTAssertEqual(settings.selectedModel?.value, "gemini-4-neon")
        XCTAssertEqual(settings.efforts.map(\.value), ["high", "xhigh", "medium", "low"])
        XCTAssertEqual(settings.selectedEffort?.value, "high")
        XCTAssertEqual(settings.modes.map(\.value), ["default", "accept-edits", "auto", "plan"])
        XCTAssertEqual(settings.selectedMode?.value, "plan")
        XCTAssertFalse(settings.modes.contains(where: \.confirm), "none of agy's modes asks nothing")

        let ended = try summary(
            "session_hosted_ended", ["model": fact("gemini-4-neon"), "model_label": .string("Gemini 4 Neon")])
        XCTAssertEqual(
            RunSettings.of(summary: ended), .hostedReadOnly(model: RunModelName(text: "Gemini 4 Neon", reported: false)))
    }

    /// Antigravity's sign-in as a computer lists it (`fixtures/accounts/antigravity-sign-in-wire.json`, what ccd's
    /// `antigravity_sign_in_frames` emits), read from the source file.
    private func signInBook(_ key: String) throws -> AccountBook {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let data = try Data(contentsOf: repoRoot.appendingPathComponent("fixtures/accounts/antigravity-sign-in-wire.json"))
        let frame = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data)[key], key)
        guard case .accounts(_, let book) = try JSONDecoder().decode(
            ServerMessage.self, from: Data(frame.canonicalJSONString.utf8))
        else { throw XCTSkip("\(key) is not an accounts frame") }
        return book
    }

    /// Antigravity's one row says its plan exactly as agy names it ("Google AI Pro" here), "Signed in" when agy names
    /// none, and nothing of its own while it is signed out or signing in (the row says those); its "…" offers Sign Out…
    /// signed in, Sign In… signed out, and nothing while a sign-in is going.
    func testAntigravitysRowSaysItsSignInAndItsPlanAsAgyNamesIt() throws {
        let line: (AccountBook) -> String? = { book in
            book.agentSignIn(for: .antigravity)?.line(plan: book.defaultFacts(for: .antigravity)?.plan)
        }
        let pro = try signInBook("signed_in_pro")
        XCTAssertEqual(line(pro), "Signed in · Google AI Pro")
        XCTAssertEqual(pro.agentSignIn(for: .antigravity)?.acts, [.signOut])
        XCTAssertEqual(line(try signInBook("signed_in_no_tier")), "Signed in")
        let out = try signInBook("signed_out")
        XCTAssertNil(line(out))
        XCTAssertEqual(out.agentSignIn(for: .antigravity)?.signedOut, true)
        XCTAssertEqual(out.agentSignIn(for: .antigravity)?.acts, [.signIn])
        let going = try signInBook("signing_in")
        XCTAssertNil(line(going))
        XCTAssertEqual(going.agentSignIn(for: .antigravity)?.signingIn, true)
        XCTAssertEqual(going.agentSignIn(for: .antigravity)?.acts, [])
        XCTAssertEqual(AccountRowAct.signIn.title, "Sign In…")
        XCTAssertEqual(AccountRowAct.signOut.title, "Sign Out…")
    }

    /// An answer to an Antigravity card, confirmed through its host, says it was handed to Antigravity, not to Claude;
    /// a Claude card's still says Claude.
    func testAnAnsweredAntigravityCardSaysItWentToAntigravity() {
        let applied = AnswerOutcome(
            requestID: "r", sessionID: "cc-1", decision: .allow, resolvedBy: .phone, appliedVia: .hookReturn,
            resolvedAt: "2026-11-25T12:00:00.000Z", detail: nil, indeterminate: false)
        XCTAssertEqual(
            DecisionCard.resultBanner(for: .applied(applied), agent: .antigravity, on: .iPhone).provenance,
            "handed straight to Antigravity")
        XCTAssertEqual(
            DecisionCard.resultBanner(for: .applied(applied), agent: .claude, on: .iPhone).provenance,
            "handed straight to Claude")
    }

    /// Setup asks for the person only when the service is missing or no agent is ready (the owner's rule): a computer
    /// with only Claude Code, only Codex or only Antigravity is fine; one with none, or no service, is not; an agent
    /// still being checked is not missing.
    func testSetupNeedsThePersonOnlyWhenNoAgentIsReady() {
        let needed = { (service: Bool, agents: [Bool]) in
            SetupAttention.needed(serviceMissing: service, agentsMissing: agents)
        }
        XCTAssertFalse(needed(false, [false, true, true]), "Claude Code only")
        XCTAssertFalse(needed(false, [true, false, true]), "Codex only")
        XCTAssertFalse(needed(false, [true, true, false]), "Antigravity only")
        XCTAssertTrue(needed(false, [true, true, true]), "none")
        XCTAssertTrue(needed(true, [false, false, false]), "no service")
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
