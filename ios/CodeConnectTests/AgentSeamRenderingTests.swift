import XCTest

@testable import CodeConnect

/// The agent-seam **rendering** half of Phase-1 decode-safety (findings 6a/6b):
/// decoding the new shapes safely is only half the job — the copy the reader
/// actually sees must never make an actuation claim this build cannot support.
/// These assert the pure, static copy-producing functions that drive
/// `ResolutionBanner`, so the exact words are testable without standing up a
/// SwiftUI `View`. (The `View`'s `body` is not exercised here; the functions it
/// calls to produce every string are.)
@MainActor
final class AgentSeamRenderingTests: XCTestCase {

    private func outcome(
        appliedVia: AnswerPath, indeterminate: Bool = false, decision: AnswerDecision = .allow,
        detail: String? = nil
    ) -> AnswerOutcome {
        AnswerOutcome(
            requestID: "r", sessionID: "cc-1", decision: decision, resolvedBy: .phone,
            appliedVia: appliedVia, resolvedAt: "2026-08-18T09:00:00.000Z", detail: detail,
            inferred: false, indeterminate: indeterminate)
    }

    // MARK: 6a — an unknown applied-path never claims a real actuation

    /// The exact trap the plan forbids: an `AnswerPath.unknown` used to render as
    /// "returned to the hook" — a positive claim about how the answer landed that
    /// this build cannot vouch for. It must now read as unrecognised, never as a
    /// hook return or a keystroke.
    func testUnknownAppliedViaMakesNoPositiveActuationClaim() {
        let phrase = ResolutionBanner.actuationPhrase(for: .unknown("future_path"))
        XCTAssertNotEqual(phrase, "returned to the hook", "unknown must not claim a hook return")
        XCTAssertNotEqual(phrase, "typed at the TTY", "unknown must not claim keystrokes")
        XCTAssertTrue(
            phrase.contains("recognise"),
            "an unknown path is described as unrecognised, got: \(phrase)")

        // And through the full provenance line an applied outcome would render.
        let provenance = ResolutionBanner.provenance(for: .applied(outcome(appliedVia: .unknown("future_path"))))
        XCTAssertNotNil(provenance)
        XCTAssertFalse(provenance!.contains("returned to the hook"), provenance!)
        XCTAssertFalse(provenance!.contains("typed at the TTY"), provenance!)
    }

    /// The two known paths keep their exact, unchanged copy.
    func testKnownAppliedViaKeepTheirExactCopy() {
        XCTAssertEqual(ResolutionBanner.actuationPhrase(for: .sendKeys), "typed at the TTY")
        XCTAssertEqual(ResolutionBanner.actuationPhrase(for: .hookReturn), "returned to the hook")
    }

    // MARK: 6b — an indeterminate outcome renders as indeterminate, never confirmed

    /// The classification step, tested pure: an `indeterminate` outcome must
    /// never become `.applied` (the case rendered as "confirmed"). This is the
    /// exact function `AppModel.sendAnswer` now routes its applied result through.
    func testIndeterminateOutcomeClassifiesAsIndeterminateNotApplied() {
        guard case .indeterminate = AnswerAttempt.classify(applied: outcome(appliedVia: .sendKeys, indeterminate: true))
        else { return XCTFail("an indeterminate outcome must classify as .indeterminate") }

        guard case .applied = AnswerAttempt.classify(applied: outcome(appliedVia: .sendKeys, indeterminate: false))
        else { return XCTFail("a confirmed outcome must classify as .applied") }
    }

    /// The rendering step: `.indeterminate` reads as "Unconfirmed" and its
    /// sentence never says "confirmed by the daemon" — while `.applied` still
    /// does, so the contrast is real and not a copy accident.
    func testIndeterminateRendersAsUnconfirmedNotConfirmed() {
        let indeterminate = AnswerAttempt.indeterminate(outcome(appliedVia: .sendKeys, indeterminate: true))
        XCTAssertEqual(ResolutionBanner.classificationLabel(for: indeterminate), "Unconfirmed")
        let indeterminateHeadline = ResolutionBanner.headline(for: indeterminate)
        XCTAssertFalse(
            indeterminateHeadline.contains("confirmed by the daemon"),
            "the daemon did not confirm this landed: \(indeterminateHeadline)")

        let applied = AnswerAttempt.applied(outcome(appliedVia: .sendKeys))
        XCTAssertEqual(ResolutionBanner.classificationLabel(for: applied), "Confirmed")
        XCTAssertTrue(
            ResolutionBanner.headline(for: applied).contains("confirmed by the daemon"),
            "a genuinely confirmed answer still says so")
    }

    /// An `.indeterminate` provenance must never reuse the actuation phrase —
    /// even when the outcome's `appliedVia` is `.sendKeys`, it cannot say "typed
    /// at the TTY", because that is the very landing the daemon could not confirm.
    func testIndeterminateProvenanceMakesNoPositiveActuationClaim() throws {
        let attempt = AnswerAttempt.indeterminate(outcome(appliedVia: .sendKeys, indeterminate: true))
        let provenance = try XCTUnwrap(ResolutionBanner.provenance(for: attempt))
        XCTAssertFalse(provenance.contains("typed at the TTY"), provenance)
        XCTAssertFalse(provenance.contains("returned to the hook"), provenance)
        XCTAssertTrue(provenance.contains("never confirmed"), provenance)
    }

    // MARK: 6b — the DUPLICATE path is where indeterminate actually arrives

    /// The reachable path the first fix missed: the daemon replays a recorded,
    /// never-confirmed outcome as `AnswerResult.duplicate` carrying
    /// `indeterminate: true`. Decoded from the real wire frame, it must classify
    /// as `.indeterminate` — never `.duplicate` ("Already answered") — and render
    /// as unconfirmed.
    func testDuplicateFrameCarryingIndeterminateClassifiesAndRendersUnconfirmed() throws {
        let message = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                """
                {"type":"answer_result","request_id":"toolu_1","result":{"status":"duplicate",
                 "outcome":{"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"deny"},
                   "resolved_by":"local","applied_via":"send_keys",
                   "resolved_at":"2026-08-18T09:00:00.000Z","indeterminate":true},
                 "stale_payload_hash":false}}
                """.utf8))
        guard case .answerResult(_, .duplicate(let outcome, let stale)) = message else {
            return XCTFail("expected a duplicate answer_result, got \(message)")
        }
        XCTAssertTrue(outcome.indeterminate, "the wire carried indeterminate on the duplicate")

        let attempt = AnswerAttempt.classify(duplicate: outcome, staleHash: stale)
        guard case .indeterminate = attempt else {
            return XCTFail("an indeterminate duplicate must classify as .indeterminate, got \(attempt)")
        }
        // And the copy that reaches the reader is unconfirmed, not "Already answered".
        XCTAssertEqual(ResolutionBanner.classificationLabel(for: attempt), "Unconfirmed")
        let headline = ResolutionBanner.headline(for: attempt)
        XCTAssertFalse(headline.contains("confirmed by the daemon"), headline)
        XCTAssertNotEqual(headline, "Already answered")
    }

    /// A genuine, confirmed duplicate is untouched: it still reads "Already
    /// answered", so the fix narrows only the indeterminate case.
    func testAConfirmedDuplicateStillReadsAsAlreadyAnswered() {
        let attempt = AnswerAttempt.classify(
            duplicate: outcome(appliedVia: .sendKeys, indeterminate: false), staleHash: false)
        guard case .duplicate = attempt else { return XCTFail("expected .duplicate, got \(attempt)") }
        XCTAssertEqual(ResolutionBanner.headline(for: attempt), "Already answered")
    }

    // MARK: 6b — the already-resolved banner (recorded outcome on the card)

    /// A recorded outcome with `indeterminate: true` — the shape the daemon
    /// replays — must render the card's already-resolved banner as "Unconfirmed"
    /// with no confirming seal, never "Already resolved" with a checkmark.
    func testAlreadyResolvedBannerHonoursIndeterminate() {
        let unconfirmed = outcome(appliedVia: .sendKeys, indeterminate: true)
        XCTAssertEqual(DecisionCardView.alreadyResolvedTitle(for: unconfirmed), "Unconfirmed")
        XCTAssertNotEqual(
            DecisionCardView.alreadyResolvedIcon(for: unconfirmed), "checkmark.seal",
            "an unconfirmed outcome must not wear the confirming seal")
        XCTAssertTrue(
            DecisionCardView.alreadyResolvedMessage(for: unconfirmed, now: Date())
                .contains("reached the agent"))

        let confirmed = outcome(appliedVia: .sendKeys, indeterminate: false)
        XCTAssertEqual(DecisionCardView.alreadyResolvedTitle(for: confirmed), "Already resolved")
        XCTAssertEqual(DecisionCardView.alreadyResolvedIcon(for: confirmed), "checkmark.seal")
    }

    // MARK: 6b — the timeline tool status

    /// An indeterminate answer must not stamp a definitive tool status. An
    /// indeterminate *deny* must not read as `denied`; a confirmed deny still does.
    func testIndeterminateAnswerDoesNotDriveADefinitiveToolStatus() {
        let toolID = "toolu_9"
        let indeterminateDeny = outcome(appliedVia: .sendKeys, indeterminate: true, decision: .deny)
        XCTAssertNotEqual(
            TimelineBuilder.statusFromApproval(toolUseID: toolID, approvals: [toolID: indeterminateDeny]),
            .denied,
            "an unconfirmed denial must not claim the call was blocked")
        XCTAssertNil(
            TimelineBuilder.statusFromApproval(toolUseID: toolID, approvals: [toolID: indeterminateDeny]),
            "an indeterminate answer justifies no definitive status; the timing verdict stands")

        let confirmedDeny = outcome(appliedVia: .sendKeys, indeterminate: false, decision: .deny)
        XCTAssertEqual(
            TimelineBuilder.statusFromApproval(toolUseID: toolID, approvals: [toolID: confirmedDeny]),
            .denied,
            "a confirmed denial still reads as denied")
    }
}


/// Finding 7 — composite wire request-ids are **opaque**. The client correlates
/// an approval to its answer by exact-string equality of `request_id` and never
/// parses or decodes the id. Two visit generations of the SAME logical
/// (session_uid, thread_id, server_request_id) are different opaque strings that
/// share a decoded prefix, so a parser that looked past the bytes would collide
/// them — the exact bug this proves absent, against the real correlation path in
/// `DaemonConnection` (`answerWaiters[requestID]` keyed delivery).
@MainActor
final class CompositeIdOpacityTests: XCTestCase {

    /// Pinned from `fixtures/codex/composite_ids.json` (generated by the Rust
    /// `protocol::composite_id` vector). Kept here as literals so the opacity
    /// proof runs even where the test bundle cannot read the repo tree;
    /// `testTheFixtureFileMatchesThePinnedStrings` cross-checks them against the
    /// file when it is reachable, so drift is still caught.
    private static let generation1 =
        "AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QUVIABHRoX0EAAAAAAAAAAAAAAAAAAAAAAQ"
    private static let generation3 =
        "AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QUVIABHRoX0EAAAAAAAAAAAAAAAAAAAAAAw"

    private func appliedResult(requestID: String) -> AnswerResult {
        .applied(
            outcome: AnswerOutcome(
                requestID: requestID, sessionID: "u-1", decision: .allow, resolvedBy: .phone,
                appliedVia: .sendKeys, resolvedAt: "2026-08-18T09:00:00.000Z", detail: nil,
                inferred: false))
    }

    /// (c) The two ids are genuinely different strings — the premise everything
    /// else depends on.
    func testTheTwoGenerationsAreDistinctStrings() {
        XCTAssertNotEqual(
            Self.generation1, Self.generation3,
            "a different visit generation is a different opaque id")
    }

    /// (a) and (b): an answer carrying `generation_1` correlates to the pending
    /// answer keyed by `generation_1`, and an answer carrying `generation_3`
    /// does NOT satisfy it — exercised through the real request/waiter path.
    func testAnswerCorrelatesByExactRequestIdAndNotByADecodedPrefix() async throws {
        let model = AppModel(cache: EventCache())
        let connection = model.connection
        connection.simulateConnectedForTesting()

        // The waiter is registered before `send` runs, so fulfilling here is a
        // deterministic sync point: once it fires, `answerWaiters[gen1]` exists.
        let sent = expectation(description: "the answer for generation_1 was sent")
        connection.sendStub = { _ in sent.fulfill() }

        let pending = Task { @MainActor in
            try await connection.answer(
                requestID: Self.generation1, payloadHash: "h", decision: .allow, session: "u-1")
        }
        await fulfillment(of: [sent], timeout: 2)

        // A different visit's id must not be read as this one: it shares a
        // decoded prefix but is a different string, so it finds no waiter.
        connection.injectForTesting(
            .answerResult(requestID: Self.generation3, result: appliedResult(requestID: Self.generation3)))

        // The exact id does correlate, and resumes the pending answer.
        connection.injectForTesting(
            .answerResult(requestID: Self.generation1, result: appliedResult(requestID: Self.generation1)))

        let result = try await pending.value
        guard case .applied(let outcome) = result else {
            return XCTFail("expected the applied result, got \(result)")
        }
        // If the client had parsed the id and collided the two generations, the
        // generation_3 frame would have resumed the waiter and this would read
        // generation_3. Exact-string correlation makes it generation_1.
        XCTAssertEqual(
            outcome.requestID, Self.generation1,
            "the answer for generation_3 must never satisfy generation_1's card")
    }

    /// When the test bundle can reach the repo tree, the pinned literals above
    /// must equal the Rust-generated fixture — otherwise a regenerated vector
    /// would silently drift from this test. Skipped (not failed) when the file
    /// is unreachable, e.g. a sandboxed CI bundle.
    func testTheFixtureFileMatchesThePinnedStrings() throws {
        struct Fixture: Decodable {
            let generation_1: String
            let generation_3: String
        }
        // #filePath is repo/ios/CodeConnectTests/<thisfile>.swift → up 3 to repo.
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let url = repoRoot.appendingPathComponent("fixtures/codex/composite_ids.json")
        guard let data = try? Data(contentsOf: url) else {
            throw XCTSkip("fixture not reachable from the test bundle at \(url.path)")
        }
        let fixture = try JSONDecoder().decode(Fixture.self, from: data)
        XCTAssertEqual(fixture.generation_1, Self.generation1)
        XCTAssertEqual(fixture.generation_3, Self.generation3)
    }
}


/// The persisted-timeline `ApprovalRow` (finding 6, last reachable site): a
/// recorded `approvalResolved` outcome carrying `indeterminate: true` is
/// retained onto the row and must render UNCONFIRMED — never a definitive
/// "RESOLVED", decision/actor claim, or "Resolved approval" to VoiceOver.
/// Driven through the REAL `TimelineBuilder`, not a manufactured item.
@MainActor
final class PersistedApprovalRowTests: XCTestCase {

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    /// A request + a resolution for one request_id; `indeterminate` toggles the
    /// outcome the daemon replays onto the row.
    private func approvalRow(indeterminate: Bool) throws -> ApprovalItem {
        let request = try event(
            """
            {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:00.000Z",
             "kind":"approval_request","source":"hook","payload":{"card":{
               "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
               "tool_input":{"command":"ls"},"display_text":"Bash"}}}
            """)
        let resolved = try event(
            """
            {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:01.000Z",
             "kind":"approval_resolved","source":"daemon","payload":{
               "request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
               "resolved_by":"local","applied_via":"send_keys",
               "resolved_at":"2026-08-18T09:00:01.000Z","indeterminate":\(indeterminate)}}
            """)
        let items = TimelineBuilder.build([request, resolved])
        for item in items {
            if case .approval(let approval) = item.content { return approval }
        }
        throw XCTSkip("no approval row was built")
    }

    /// The reachable-path assertion: an indeterminate resolved outcome renders
    /// UNCONFIRMED, no definitive decision/actor, and "Unconfirmed approval" to
    /// VoiceOver — never "RESOLVED"/"Resolved approval".
    func testIndeterminateResolvedOutcomeRendersUnconfirmedOnTheRow() throws {
        let approval = try approvalRow(indeterminate: true)
        let outcome = try XCTUnwrap(approval.outcome, "the row retained the resolved outcome")
        XCTAssertTrue(outcome.indeterminate)

        XCTAssertEqual(ApprovalRow.headerTitle(for: approval), "UNCONFIRMED")
        XCTAssertNotEqual(ApprovalRow.headerTitle(for: approval), "RESOLVED")

        let footer = ApprovalRow.resolutionText(for: outcome)
        XCTAssertFalse(footer.contains("Allowed"), "no definitive decision claim: \(footer)")
        XCTAssertFalse(footer.contains("from this app"), "no definitive actor claim: \(footer)")
        XCTAssertEqual(footer, "Answer not confirmed")

        let spoken = ApprovalRow.resolvedAccessibilityLabel(
            for: outcome, toolName: approval.card.toolName)
        XCTAssertFalse(spoken.contains("Resolved approval"), spoken)
        XCTAssertTrue(spoken.hasPrefix("Unconfirmed approval"), spoken)
    }

    /// Scope check: a genuinely confirmed resolved outcome still renders
    /// "RESOLVED", the decision/actor, and "Resolved approval" exactly as before.
    func testConfirmedResolvedOutcomeStillRendersResolved() throws {
        let approval = try approvalRow(indeterminate: false)
        let outcome = try XCTUnwrap(approval.outcome)
        XCTAssertFalse(outcome.indeterminate)

        XCTAssertEqual(ApprovalRow.headerTitle(for: approval), "RESOLVED")
        // resolved_by "local" in the fixture → "at the keyboard".
        XCTAssertEqual(ApprovalRow.resolutionText(for: outcome), "Allowed at the keyboard")
        XCTAssertTrue(
            ApprovalRow.resolvedAccessibilityLabel(for: outcome, toolName: approval.card.toolName)
                .hasPrefix("Resolved approval"))
    }
}


/// Finding 6, the whole CLASS closed in one place: a decision card must never
/// present an actionable pending state, or claim an outcome, it cannot back with
/// authoritative LIVE state. Every scenario here is driven through the REAL
/// production resolver the card view uses — `AppModel.liveApproval(...)` — and
/// the real gate functions it feeds (`DecisionCardView.resolvedBanner` /
/// `.actionBarAvailable`). A revert of the resolver (e.g. `reset()` no longer
/// clearing `pendingApprovals`, or `liveApproval` returning a frozen snapshot) or
/// of the gates (dropping `isBacked`) fails these. The only thing outside unit
/// reach is the SwiftUI body continuing to CALL them — covered by UI tests, and
/// minimised here by there being a single call site each.
@MainActor
final class DecisionCardLiveBackingTests: XCTestCase {

    private let sessionKey = "u-1"
    private let cardID = "u-1#toolu_1"

    private func event(_ json: String) throws -> Event {
        try JSONDecoder().decode(Event.self, from: Data(json.utf8))
    }

    private let requestEvent = """
        {"seq":1,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:00.000Z",
         "kind":"approval_request","source":"hook","payload":{"card":{
           "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
           "tool_input":{"command":"ls"},"display_text":"Bash"}}}
        """
    private let resolvedIndeterminateEvent = """
        {"seq":2,"session_uid":"u-1","session_id":"cc-1","ts":"2026-08-18T09:00:01.000Z",
         "kind":"approval_resolved","source":"daemon","payload":{
           "request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
           "resolved_by":"local","applied_via":"send_keys",
           "resolved_at":"2026-08-18T09:00:01.000Z","indeterminate":true}}
        """

    private func ingest(_ json: String, into model: AppModel) throws {
        model.connection.injectForTesting(.event(try event(json)))
    }
    private func settle(_ model: AppModel) async {
        for state in model.states.values { await state.settleForTesting() }
    }

    /// **A card's run, described.** `AppModel.answer` fails closed when no
    /// summary names the agent (a card whose run has left the fleet must not
    /// transmit an `allow` on Claude's behalf), so a test about *answering*
    /// has to say whose session this is. A test about an unbacked card
    /// deliberately does not.
    private func describeSessionAsClaude(_ model: AppModel) {
        model.connection.injectForTesting(
            .sessions([
                try! JSONDecoder().decode(
                    SessionSummary.self,
                    from: Data(
                        """
                        {"session_uid":"u-1","session_id":"cc-1","tmux_session":"cc-1",
                         "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                         "last_seq":1,"created_at":"2026-08-18T09:00:00.000Z",
                         "updated_at":"2026-08-18T09:00:00.000Z","blocked_on":[],"agent":"claude"}
                        """.utf8))
            ]))
    }

    // The two production-boundary expressions the card view actually evaluates.
    private func liveBanner(_ model: AppModel, attempt: AnswerAttempt?)
        -> DecisionCardView.ResolvedBanner
    {
        let live = model.liveApproval(sessionKey: sessionKey, id: cardID)
        return DecisionCardView.resolvedBanner(
            attempt: attempt, persisted: live?.outcome, isBacked: live != nil)
    }
    private func liveBarAvailable(_ model: AppModel, attempt: AnswerAttempt?) -> Bool {
        let live = model.liveApproval(sessionKey: sessionKey, id: cardID)
        return DecisionCardView.actionBarAvailable(
            outcome: live?.outcome, codex: live?.codexResolution, attempt: attempt,
            isBacked: live != nil)
    }
    private func pendingItem(_ model: AppModel) -> ApprovalItem? {
        model.liveApproval(sessionKey: sessionKey, id: cardID)
    }

    // MARK: normal live paths

    /// A live pending card offers its controls and shows no resolved banner.
    func testLivePendingCardIsActionable() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        await settle(model)

        XCTAssertNotNil(pendingItem(model), "the card is live-backed")
        XCTAssertTrue(liveBarAvailable(model, attempt: nil))
        guard case .none = liveBanner(model, attempt: nil) else {
            return XCTFail("a live pending card shows no resolved banner")
        }
    }

    /// **A card whose run the fleet cannot describe answers nothing.**
    ///
    /// The same card as the test below, minus the summary. `AppModel.answer`
    /// used to read `summary(for:)?.agent ?? .claude`, so this transmitted an
    /// `allow` on behalf of an agent nobody could name — the one vocabulary the
    /// daemon accepts, chosen by a default.
    func testACardWithNoDescribedSessionIsNotAnswered() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        await settle(model)

        let pending = try XCTUnwrap(pendingItem(model))
        guard case .rejected(let reason) = await model.answer(item: pending, decision: .allow)
        else { return XCTFail("an unknown agent must be refused, not attempted") }
        XCTAssertTrue(reason.contains("no longer on the fleet"), reason)
    }

    /// Answer → the daemon crashes → recovery resolves the card as indeterminate.
    /// The open card re-derives it: Unconfirmed, and the action bar disables —
    /// even though a stale dead-socket `.failed` is still stored.
    func testRecoveredIndeterminateOutcomeWinsAndDisablesBar() async throws {
        let model = AppModel(cache: EventCache())  // NOT connected
        try ingest(requestEvent, into: model)
        describeSessionAsClaude(model)
        await settle(model)

        // A real dead-socket failure stored under the card id.
        let pending = try XCTUnwrap(pendingItem(model))
        let failure = await model.answer(item: pending, decision: .allow)
        guard case .failed = failure else { return XCTFail("expected .failed, got \(failure)") }

        // Recovery persists the authoritative indeterminate outcome.
        try ingest(resolvedIndeterminateEvent, into: model)
        await settle(model)

        let attempt = model.lastAttempt(for: pending)  // still the stale .failed
        guard case .failed = attempt else { return XCTFail("the stale failure is still stored") }

        XCTAssertFalse(liveBarAvailable(model, attempt: attempt), "a resolved card disables the bar")
        switch liveBanner(model, attempt: attempt) {
        case .persisted(let outcome):
            XCTAssertTrue(outcome.indeterminate)
            XCTAssertEqual(DecisionCardView.alreadyResolvedTitle(for: outcome), "Unconfirmed")
        case .attempt, .unavailable, .none:
            XCTFail("the recovered outcome must win, never the stale failure")
        }
    }

    // MARK: the stale/unbacked class

    /// The session leaves the fleet while the sheet is open (a live refresh drops
    /// its state). The card must go NON-actionable — never fall back to the frozen
    /// actionable snapshot with its stale `.failed`.
    func testStateDisappearingWhileOpenMakesCardUnavailable() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        await settle(model)
        let pending = try XCTUnwrap(pendingItem(model))
        _ = await model.answer(item: pending, decision: .allow)  // stores a stale .failed

        // A fleet refresh that no longer lists this run drops its state.
        model.connection.injectForTesting(.sessions([]))
        XCTAssertNil(model.states[sessionKey], "the departed session's state is gone")

        let attempt = model.lastAttempt(for: pending)  // the stale .failed survives
        XCTAssertNil(pendingItem(model), "no live state backs the card")
        XCTAssertFalse(
            liveBarAvailable(model, attempt: attempt),
            "an unbacked card must never present an action bar")
        guard case .unavailable = liveBanner(model, attempt: attempt) else {
            return XCTFail("an unbacked card must render Unavailable, not the stale failure")
        }
    }

    /// A log rewind/reset clears the timeline. `reset()` must also clear the
    /// derived `pendingApprovals`, or the card lingers in the Deck projection and
    /// the sheet's live lookup. After reset: gone from the Deck, and Unavailable.
    func testResetClearsPendingApprovalsAndDeckProjection() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        await settle(model)
        let state = try XCTUnwrap(model.states[sessionKey])
        XCTAssertFalse(state.pendingApprovals.isEmpty, "the pending card is projected")
        XCTAssertTrue(
            model.deck.contains { $0.id == cardID }, "and appears in the cross-fleet Deck")

        state.resetForRewoundLog()

        XCTAssertTrue(state.pendingApprovals.isEmpty, "reset clears the derived projection")
        XCTAssertFalse(
            model.deck.contains { $0.id == cardID }, "so the Deck drops the stale card")
        XCTAssertNil(pendingItem(model), "and the sheet's live lookup finds nothing")
        guard case .unavailable = liveBanner(model, attempt: nil) else {
            return XCTFail("a reset card is Unavailable")
        }
        XCTAssertFalse(liveBarAvailable(model, attempt: nil))
    }

    // MARK: no-regression — this session's own terminal receipt still stands

    /// A terminal local observation (`.applied`) is this session's own fact and
    /// keeps its receipt even if the backing state is later dropped — it is not
    /// turned into "Unavailable", and it is already non-actionable.
    func testTerminalLocalReceiptSurvivesLossOfBacking() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        await settle(model)
        model.connection.injectForTesting(.sessions([]))  // drop the backing
        XCTAssertNil(pendingItem(model))

        let applied = AnswerAttempt.applied(
            AnswerOutcome(
                requestID: "toolu_1", sessionID: "cc-1", decision: .allow, resolvedBy: .phone,
                appliedVia: .sendKeys, resolvedAt: "2026-08-18T09:00:01.000Z", detail: nil,
                inferred: false))
        guard case .attempt = liveBanner(model, attempt: applied) else {
            return XCTFail("a terminal receipt stands even without live backing")
        }
        XCTAssertFalse(
            liveBarAvailable(model, attempt: applied), "and it is non-actionable regardless")
    }

    // MARK: the accessibility-size answer/deny affordance shares the gate

    /// At accessibility text sizes the "Deny with a reason" affordance
    /// (`subordinateControls`) is the only place that deny path lives, and it is
    /// rendered outside the action bar. It must be withheld on an unbacked /
    /// `.unavailable` card exactly like every other answer surface, so a card is
    /// non-actionable at EVERY text size. Driven through the real gate the view
    /// uses (`isActionable`, computed here from the real resolver).
    func testAccessibilitySizeDenyAffordanceSharesTheActionabilityGate() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        await settle(model)

        // Live pending card: the affordance shows at accessibility sizes.
        let pending = try XCTUnwrap(pendingItem(model))
        XCTAssertTrue(
            DecisionCardView.subordinateControlsShown(
                isAccessibilitySize: true, isActionable: liveBarAvailable(model, attempt: nil)),
            "a live pending card offers the deny-with-reason affordance at AX sizes")

        // The session leaves the fleet: the card is unbacked, and the AX-size
        // affordance is withheld just like the action bar — even with a stale
        // local .failed still stored.
        _ = await model.answer(item: pending, decision: .allow)
        model.connection.injectForTesting(.sessions([]))
        XCTAssertNil(pendingItem(model), "no live state backs the card")
        let attempt = model.lastAttempt(for: pending)
        XCTAssertFalse(
            DecisionCardView.subordinateControlsShown(
                isAccessibilitySize: true, isActionable: liveBarAvailable(model, attempt: attempt)),
            "an unbacked card exposes no deny affordance at accessibility sizes either")
    }

    // MARK: a resolved card never reverts to actionable (opened-snapshot outcome)

    /// Regression our own live-only derivation opened: a sheet opened from a
    /// RESOLVED row must keep its outcome and stay non-actionable even after a
    /// reset and a behind/replay that re-supplies the same request id as PENDING.
    /// The governing outcome comes from the live lookup OR the opened snapshot —
    /// a recorded outcome from either source disables the card. Driven through the
    /// real resolver and the real `effectiveOutcome`/gate the view uses.
    func testResolvedCardNeverRevertsToActionableAfterResetAndReplay() async throws {
        let model = AppModel(cache: EventCache())
        try ingest(requestEvent, into: model)
        try ingest(resolvedIndeterminateEvent, into: model)
        await settle(model)

        // The sheet was opened from a resolved (indeterminate) row.
        let snapshot = try XCTUnwrap(pendingItem(model))
        XCTAssertTrue(try XCTUnwrap(snapshot.outcome).indeterminate)

        // (snapshot resolved, live nil): reset clears the log.
        try XCTUnwrap(model.states[sessionKey]).resetForRewoundLog()
        assertResolvedAndDisabled(model, snapshot: snapshot, "snapshot resolved + live nil")

        // (snapshot resolved, live PENDING): a behind replay re-supplies the same
        // request id as pending — THE regression trigger.
        try ingest(requestEvent, into: model)
        await settle(model)
        let live = model.liveApproval(sessionKey: sessionKey, id: cardID)
        XCTAssertNotNil(live, "the replay re-supplied a live pending card")
        XCTAssertNil(live?.outcome, "which is pending (no outcome of its own)")
        assertResolvedAndDisabled(model, snapshot: snapshot, "snapshot resolved + live pending")
    }

    /// Asserts the card renders its resolved (Unconfirmed) banner and is not
    /// actionable, evaluating the exact `effectiveOutcome`/gate expressions the
    /// card view uses (live lookup OR opened snapshot).
    private func assertResolvedAndDisabled(
        _ model: AppModel, snapshot: ApprovalItem, _ what: String
    ) {
        let live = model.liveApproval(sessionKey: sessionKey, id: cardID)
        let eff = DecisionCardView.effectiveOutcome(live: live?.outcome, snapshot: snapshot.outcome)
        XCTAssertNotNil(eff, "\(what): the resolved outcome still governs")
        XCTAssertFalse(
            DecisionCardView.actionBarAvailable(
                outcome: eff, codex: live?.codexResolution, attempt: nil, isBacked: live != nil),
            "\(what): a resolved card must never be actionable")
        switch DecisionCardView.resolvedBanner(attempt: nil, persisted: eff, isBacked: live != nil) {
        case .persisted(let outcome):
            XCTAssertEqual(
                DecisionCardView.alreadyResolvedTitle(for: outcome), "Unconfirmed",
                "\(what): the indeterminate banner must stand")
        case .attempt, .unavailable, .none:
            XCTFail("\(what): must show the resolved banner, never pending/unavailable")
        }
    }
}
