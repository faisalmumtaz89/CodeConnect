import XCTest

@testable import CodeConnect

/// **Tier 2 — the copy decisions, as pure functions.**
///
/// Follows `AgentSeamRenderingTests`, which already tests the Claude half this
/// way: the sentences a reader sees are produced by static functions, so the
/// exact words can be asserted without standing up a SwiftUI `View` and without
/// a simulator. What a render pass adds on top is what those sentences *look
/// like* at AX5; what a render pass cannot do is tell you a sentence is a lie.
@MainActor
final class CodexProseTests: XCTestCase {

    // MARK: Card endings

    /// The copy the `.unavailable` banner uses, which no Codex ending may reuse.
    /// It is about the phone losing sight of a run; every Codex ending is about
    /// the *question* finishing, which is usually completely ordinary.
    private let unavailableCopy =
        "This decision can't be acted on here — the run left the daemon's list or its log was reset."

    private var allEndings: [CodexResolution] {
        [
            .answered(by: .phone, decision: .optionId("accept")),
            .answered(by: .local, decision: nil),
            .cleared(cause: .turnAborted),
            .cleared(cause: .turnCompleted),
            .cleared(cause: .superseded),
            .cleared(cause: .itemCompleted),
            .timeout,
            .unknown(
                attemptedBy: .phone, attemptedDecision: .optionId("accept"),
                writeStage: .upstreamWriteUnconfirmed, cause: "connection reset before ack"),
        ]
    }

    /// Every terminal gets its **own** sentence. Two endings that read the same
    /// are two endings a reader cannot tell apart, and four of these are the
    /// difference between "your file was written" and "nothing happened".
    func testEveryCodexTerminalHasItsOwnSentence() {
        let banners = allEndings.map(CodexProse.resolution)
        XCTAssertEqual(
            Set(banners.map(\.title)).count, banners.count,
            "each ending is titled distinctly")
        XCTAssertEqual(
            Set(banners.map(\.message)).count, banners.count,
            "each ending explains itself distinctly")
        for banner in banners {
            XCTAssertNotEqual(banner.message, unavailableCopy, banner.title)
            XCTAssertFalse(banner.message.isEmpty, banner.title)
        }
    }

    /// **`item_completed` is not `turn_completed`.** The turn is still running,
    /// and a sentence saying the session finished would send a reader looking
    /// for a result that is still being produced.
    func testTheItemCompletedSentenceDoesNotSayTheTurnEnded() {
        let item = CodexProse.resolution(.cleared(cause: .itemCompleted))
        let turn = CodexProse.resolution(.cleared(cause: .turnCompleted))
        XCTAssertNotEqual(item.title, turn.title)
        XCTAssertTrue(
            item.message.contains("still working"),
            "the reader has to be told the session is not finished: \(item.message)")
        XCTAssertFalse(item.message.contains("finished before"), item.message)
    }

    /// **The honesty this whole envelope exists for.** A keyboard answer carries
    /// no decision, because upstream carries no provenance for one — so nothing
    /// may name a choice, and the copy has to say it does not know rather than
    /// leaving a blank the reader fills in themselves.
    func testAnsweredByLocalWithNoDecisionNeverNamesAChoice() {
        let banner = CodexProse.resolution(.answered(by: .local, decision: nil))
        for word in ["accept", "cancel", "Allowed", "Denied", "approved"] {
            XCTAssertFalse(
                banner.message.lowercased().contains(word.lowercased()),
                "\(word) must not appear: \(banner.message)")
        }
        let chosen = CodexProse.whatWasChosen(.answered(by: .local, decision: nil))
        XCTAssertTrue(chosen.hasPrefix("Not known"), chosen)
    }

    /// When the wire *did* say, the option id is shown verbatim — it is the
    /// thing the daemon validated, and the only spelling that is certainly right.
    func testAPhoneAnswerNamesTheOptionItSent() {
        let chosen = CodexProse.whatWasChosen(
            .answered(by: .phone, decision: .optionId("acceptWithExecpolicyAmendment")))
        XCTAssertTrue(chosen.contains("acceptWithExecpolicyAmendment"), chosen)
    }

    /// A cleared card never claims a decision. Four causes, one rule.
    func testAClearedCardNeverClaimsADecision() {
        for cause in [
            ClearCause.turnAborted, .turnCompleted, .superseded, .itemCompleted,
        ] {
            let chosen = CodexProse.whatWasChosen(.cleared(cause: cause))
            XCTAssertEqual(chosen, "Nothing. This question ended without an answer.", "\(cause)")
        }
    }

    // MARK: Refusals, verbatim

    /// A success may name the turn — the phone sent it — but the two refusal
    /// arms carry none, and the type is what enforces it.
    func testOnlySuccessCarriesATurn() {
        XCTAssertEqual(InterruptResult.aborted(turnID: "t-1").turnID, "t-1")
        XCTAssertEqual(InterruptResult.duplicate(turnID: "t-1").turnID, "t-1")
        XCTAssertNil(InterruptResult.rejected(reason: "no").turnID)
        XCTAssertNil(InterruptResult.indeterminate(reason: "?").turnID)
        XCTAssertEqual(ComposeResult.started(turnID: "t-1").turnID, "t-1")
        XCTAssertEqual(ComposeResult.steered(turnID: "t-1").turnID, "t-1")
        XCTAssertEqual(ComposeResult.duplicate(turnID: "t-1", started: false).turnID, "t-1")
        XCTAssertNil(ComposeResult.rejected(reason: "no").turnID)
        XCTAssertNil(ComposeResult.indeterminate(reason: "?").turnID)
    }

    // MARK: started vs steered

    /// **Never the same sentence.** Which of the two arrived is the answer to
    /// "what did my words do", and it was decided at the Mac, at the instant the
    /// daemon wrote — not by anything the reader could predict when they tapped.
    func testStartedAndSteeredAreNeverTheSameSentence() {
        let started = CodexProse.compose(.started(turnID: "t-1"))
        let steered = CodexProse.compose(.steered(turnID: "t-1"))
        XCTAssertNotEqual(started.title, steered.title)
        XCTAssertNotEqual(started.message, steered.message)
        XCTAssertTrue(started.title.contains("started"), started.title)
        XCTAssertTrue(steered.title.contains("joined"), steered.title)
    }

    /// D8: a replay reads with the **verb that was true when the words landed**,
    /// because the route was snapshotted at the claim. `started: false` says
    /// they joined a running turn even if the session is idle now.
    func testADuplicateReadsWithTheVerbTheOriginalEarned() {
        let asSteer = CodexProse.compose(.duplicate(turnID: "t-1", started: false))
        let asStart = CodexProse.compose(.duplicate(turnID: "t-1", started: true))
        XCTAssertEqual(asSteer.title, CodexProse.compose(.steered(turnID: "t-1")).title)
        XCTAssertEqual(asStart.title, CodexProse.compose(.started(turnID: "t-1")).title)
        // And both say plainly that nothing was said twice.
        for banner in [asSteer, asStart] {
            XCTAssertTrue(banner.message.contains("second time"), banner.message)
        }
    }

    // `testIndeterminateIsNeverOfferedARetry` lived here and asserted
    // `CodexProse.mayRetry`, a Boolean **nothing in production ever consulted**.
    // It read as proof of the strongest claim this phase makes and proved only
    // that a pure function returned what it was written to return — while the
    // real send path happily re-sent an indeterminate compose under a fresh id.
    // The rule now lives on the send path and is asserted there, by counting
    // frames: `CodexSendPathTests.testAnUnchangedComposeIsNotResentAfterIndeterminate`.
    // `mayRetry` is deleted.

    // MARK: The answer surface (gap G4)

    private func card(toolInput: String, toolName: String = "command") -> ApprovalCard {
        let display = "\(toolName)\n\(toolInput)"
        return ApprovalCard(
            // The hash is irrelevant here — these tests read the option table,
            // never `payloadHash`. `CodexTestHash` used to compute it by
            // recomputing `CardVerification`'s own expression, which guaranteed
            // the gate it was meant to exercise would pass.
            requestID: "rid", payloadHash: "unread-by-these-tests", toolName: toolName,
            toolInput: try! JSONDecoder().decode(JSONValue.self, from: Data(toolInput.utf8)),
            displayText: display, permissionSuggestions: nil, promptID: nil, permissionMode: nil,
            risk: nil)
    }

    /// **A two-option Codex card is answerable.** Claude's `count > 2` rule
    /// suppresses its redundant yes/no pair; applied to Codex it would leave
    /// Allow and Deny, which are the exact two decisions a Codex card refuses by
    /// name — so the card would have been unanswerable on this build.
    ///
    /// Two options is a real shape: the daemon **withholds** the amendment
    /// option entirely when its argv contains a line break, rather than
    /// shortening a label it cannot shorten honestly.
    func testATwoOptionCodexCardStillRendersItsOptions() {
        let two = card(
            toolInput: #"{"command":"touch x","options":[{"id":"accept","label":"Yes, proceed"},"#
                + #"{"id":"cancel","label":"No, and tell Codex what to do differently"}]}"#)
        guard case .codexOptions(let options) =
            DecisionCardView.answerSurface(card: two, agent: .codex, paneSnapshot: nil)
        else { return XCTFail("a two-option Codex card must still offer its options") }
        XCTAssertEqual(options.map(\.id), ["accept", "cancel"])
    }

    /// And a Codex card never falls through to Allow/Deny, at any count.
    func testACodexCardNeverOffersAllowOrDeny() {
        for count in 1...4 {
            let rows = (0..<count)
                .map { #"{"id":"opt\#($0)","label":"Option \#($0)"}"# }
                .joined(separator: ",")
            let subject = card(toolInput: #"{"command":"x","options":[\#(rows)]}"#)
            guard case .codexOptions(let options) =
                DecisionCardView.answerSurface(card: subject, agent: .codex, paneSnapshot: nil)
            else { return XCTFail("\(count) options must still be a Codex surface") }
            XCTAssertEqual(options.count, count)
        }
    }

    /// **Claude is unchanged.** Same function, same rule, including the `> 2`
    /// suppression that keeps a two-item prompt from drawing Allow and Deny twice.
    func testAClaudeCardKeepsItsExistingSurface() {
        let claude = card(toolInput: #"{"command":"ls"}"#, toolName: "Bash")
        let twoItemPane = "  1. Yes\n  2. No, and tell Claude what to do differently\n"
        guard case .allowDeny(let suppressed) =
            DecisionCardView.answerSurface(card: claude, agent: .claude, paneSnapshot: twoItemPane)
        else { return XCTFail("a Claude card keeps Allow/Deny") }
        XCTAssertTrue(suppressed.isEmpty, "the two-item prompt is Allow and Deny, drawn once")

        let threeItemPane =
            "  1. Yes\n  2. Yes, and don't ask again\n  3. No, and tell Claude what to do\n"
        guard case .allowDeny(let shown) =
            DecisionCardView.answerSurface(card: claude, agent: .claude, paneSnapshot: threeItemPane)
        else { return XCTFail("a Claude card keeps Allow/Deny") }
        XCTAssertEqual(shown.count, 3, "a third outcome Allow cannot express still renders")
    }

    /// **F5 — the surface follows the SESSION, not the card.**
    ///
    /// `tool_input` is content the agent itself authored, so it cannot be the
    /// discriminator: a Claude tool emitting an `options[]` table lost Allow and
    /// Deny and would have transmitted an `option_id` the Mac refuses by name.
    func testAClaudeCardWithAnOptionsKeyKeepsAllowDeny() {
        let looksCodex = card(
            toolInput: #"{"command":"x","options":[{"id":"accept","label":"Yes"}]}"#,
            toolName: "Bash")
        guard case .allowDeny =
            DecisionCardView.answerSurface(card: looksCodex, agent: .claude, paneSnapshot: nil)
        else { return XCTFail("a Claude session keeps Claude's vocabulary") }
    }

    /// And a Codex card with no usable options is **not answerable** — it never
    /// falls back to Allow/Deny, which Codex refuses.
    func testACodexCardWithoutUsableOptionsIsNotAnswerable() {
        for input in [#"{"command":"x"}"#, #"{"command":"x","options":[]}"#,
                      #"{"command":"x","options":[{"label":"no id"}]}"#] {
            guard case .noneAnswerable =
                DecisionCardView.answerSurface(
                    card: card(toolInput: input), agent: .codex,
                    paneSnapshot: "  1. Yes\n  2. No\n  3. Later\n")
            else { return XCTFail("malformed Codex options must not become Allow/Deny: \(input)") }
        }
    }

    /// **A missing summary is not a Claude card.** `?? .claude` was the default
    /// and `.claude` is the vocabulary that transmits — so a card whose run had
    /// left the fleet kept Allow, Deny, and Claude's raw disclosure.
    func testAnUnknownAgentIsNotAnswerable() {
        guard case .noneAnswerable =
            DecisionCardView.answerSurface(
                card: card(toolInput: #"{"command":"x"}"#), agent: nil,
                paneSnapshot: "  1. Yes\n  2. No\n  3. Later\n")
        else { return XCTFail("an unknown agent must not inherit Claude's answer surface") }
    }

    /// **A Codex card the phone cannot vouch for is not answerable either**, and
    /// what it shows is one sentence rather than the daemon's raw
    /// `tool\n{JSON}` — which is what `primaryText` fell back to.
    func testAnUnverifiableCodexCardIsNeitherAnswerableNorRaw() {
        // `displayText` disagrees with the structured fields, so the render
        // cannot be vouched for: the old fallback printed it verbatim.
        let unverifiable = ApprovalCard(
            requestID: "rid", payloadHash: "h", toolName: "command",
            toolInput: try! JSONDecoder().decode(
                JSONValue.self,
                from: Data(#"{"command":"ls","options":[{"id":"accept","label":"Yes"}]}"#.utf8)),
            displayText: #"command\n{"command":"ls","options":[{"id":"accept"}]}"#,
            permissionSuggestions: nil, promptID: nil, permissionMode: nil, risk: nil)
        XCTAssertFalse(unverifiable.verification.renderMatchesDisplayText)

        guard case .noneAnswerable =
            DecisionCardView.answerSurface(
                card: unverifiable, agent: .codex, paneSnapshot: nil)
        else { return XCTFail("a card the phone cannot vouch for must not be answerable") }

        let shown = unverifiable.primaryText(verification: unverifiable.verification, agent: .codex)
        XCTAssertFalse(shown.contains("{"), "no raw JSON on a Codex product screen: \(shown)")
        XCTAssertEqual(shown, ApprovalCard.unverifiableCodexLine)

        // Claude's fallback is untouched: its `display_text` is the tool's own
        // arguments, which is exactly what a Claude reader is checking.
        XCTAssertEqual(
            unverifiable.primaryText(verification: unverifiable.verification, agent: .claude),
            unverifiable.displayText)
    }

    /// An agent this build cannot drive gets no answer surface at all.
    func testAnUnsupportedAgentIsNotAnswerable() {
        guard case .noneAnswerable =
            DecisionCardView.answerSurface(
                card: card(toolInput: #"{"command":"x"}"#), agent: .unsupported("gemini"),
                paneSnapshot: nil)
        else { return XCTFail("guessing a vocabulary for an unknown agent is guessing on the wire") }
    }

    // MARK: G10 — one line, one seam rule

    /// **A title that ends in a period runs into a lowercase sentence.**
    ///
    /// Three surfaces put a banner on one line — the composer note, the fleet
    /// row's stop note, the timeline row's accessibility label — and all three
    /// joined `"\(title). \(message)"`. The daemon's sentences begin
    /// lowercase and carry their own punctuation, so that produced
    /// *"Sent, outcome unknown. this interrupt was already sent…"* — the same
    /// seam K2 fixed for rejections, still there for every other arm. One rule,
    /// stated where the banner is built: a colon before a sentence the daemon
    /// wrote, a period before one the app wrote.
    func testAVerbatimDaemonSentenceIsNotIntroducedByAFullStop() {
        let daemonWritten: [CodexProse.Banner] = [
            CodexProse.interrupt(.indeterminate(reason: "this interrupt was already sent")),
            CodexProse.interrupt(.rejected(reason: "this Mac has lost its control link")),
            CodexProse.compose(.indeterminate(reason: "that message was written")),
            CodexProse.compose(.rejected(reason: "this Mac is connected but not watching")),
        ]
        for banner in daemonWritten {
            XCTAssertTrue(
                banner.oneLine.contains("\(banner.title): "),
                "a daemon sentence follows a colon, not a full stop: \(banner.oneLine)")
            XCTAssertFalse(
                banner.oneLine.contains(". \(banner.message.prefix(1).lowercased())"),
                "no lowercase seam after a period: \(banner.oneLine)")
        }

        // The app's own sentences are whole sentences and keep the full stop.
        let appWritten: [CodexProse.Banner] = [
            CodexProse.interrupt(.aborted(turnID: "t-1")),
            CodexProse.compose(.started(turnID: "t-1")),
            CodexProse.compose(.duplicate(turnID: "t-1", started: true)),
            CodexProse.interrupt(.unknown(status: "quiesced")),
        ]
        for banner in appWritten {
            XCTAssertTrue(
                banner.oneLine.hasPrefix("\(banner.title). "),
                "the app's own sentence starts a new one: \(banner.oneLine)")
        }
    }

    // MARK: Which controls are offered

    /// The order the reader would ask the questions in, and every answer a fact
    /// the phone genuinely holds. The daemon's refusal comes **after** a tap and
    /// says which of its own eleven conditions failed; this is what a *disabled*
    /// control says before anything has been sent.
    func testWhyStopIsNotOffered() {
        XCTAssertNil(
            CodexProse.stopUnavailable(
                agent: .codex, daemonHonoursStop: true, link: .subscribed, runningTurn: "t-1"))

        XCTAssertEqual(
            CodexProse.stopUnavailable(
                agent: .claude, daemonHonoursStop: true, link: .subscribed, runningTurn: "t-1"),
            "Only a Codex session can be stopped from here.")

        XCTAssertTrue(
            CodexProse.stopUnavailable(
                agent: .codex, daemonHonoursStop: false, link: .subscribed, runningTurn: "t-1")!
                .contains("too old"))

        // **The one honest hide**: no turn, nothing to name.
        XCTAssertEqual(
            CodexProse.stopUnavailable(
                agent: .codex, daemonHonoursStop: true, link: .subscribed, runningTurn: nil),
            "Nothing is running to stop.")

        for link in [
            CodexLinkState.bound, .boundNotStarted, .offline, .none, .unknown("teleported"),
        ] {
            XCTAssertNotNil(
                CodexProse.stopUnavailable(
                    agent: .codex, daemonHonoursStop: true, link: link, runningTurn: "t-1"),
                "\(link) cannot be stopped, and the control says why")
        }
    }

    func testWhyComposeIsNotOffered() {
        XCTAssertNil(
            CodexProse.composeUnavailable(
                agent: .codex, daemonUnderstandsCompose: true, link: .subscribed))
        XCTAssertNotNil(
            CodexProse.composeUnavailable(
                agent: .claude, daemonUnderstandsCompose: true, link: .subscribed))
        XCTAssertNotNil(
            CodexProse.composeUnavailable(
                agent: .codex, daemonUnderstandsCompose: false, link: .subscribed))
        XCTAssertNotNil(
            CodexProse.composeUnavailable(
                agent: .codex, daemonUnderstandsCompose: true, link: .offline))
        // **The state that composes without being subscribed.** The Mac has proved
        // this thread has never run a turn, so the first message can be sent.
        XCTAssertNil(
            CodexProse.composeUnavailable(
                agent: .codex, daemonUnderstandsCompose: true, link: .boundNotStarted))
        XCTAssertNotNil(
            CodexProse.composeUnavailable(
                agent: .codex, daemonUnderstandsCompose: true, link: .bound))
    }

    /// **Two states actuate, and each actuates exactly what it can.** An
    /// unrecognised state is not read as either. The gate lives on the enum so
    /// no surface can re-derive it.
    ///
    /// This is the protocol rule, as a table: **`subscribed` actuates both verbs;
    /// `bound_not_started` actuates compose only; everything else neither.** The
    /// fifth word is protocol minor 20 and the other four are minor 19 — which
    /// build 72 shipped, so a daemon can legitimately speak either vocabulary at
    /// this app. A minor-19 daemon simply never says the word, and the last loop
    /// is what makes the reverse — a word this build has never heard — grey both
    /// controls instead of guessing.
    func testOnlyAProvenLinkCanActuateAndOnlyForWhatItProves() {
        XCTAssertTrue(CodexLinkState.subscribed.canActuate(.compose))
        XCTAssertTrue(CodexLinkState.subscribed.canActuate(.stop))
        XCTAssertNil(CodexLinkState.subscribed.blockedReason(for: .compose))
        XCTAssertNil(CodexLinkState.subscribed.blockedReason(for: .stop))

        // **The one un-subscribed state that composes.** The Mac has proved this
        // thread has never run a turn, so a first turn can be started on it —
        // and for the same reason there is nothing to stop.
        XCTAssertTrue(CodexLinkState.boundNotStarted.canActuate(.compose))
        XCTAssertNil(CodexLinkState.boundNotStarted.blockedReason(for: .compose))
        XCTAssertFalse(CodexLinkState.boundNotStarted.canActuate(.stop))
        XCTAssertNotNil(CodexLinkState.boundNotStarted.blockedReason(for: .stop))

        for state in [CodexLinkState.bound, .offline, .none, .unknown("future")] {
            for ask in [CodexLinkState.Ask.compose, .stop] {
                XCTAssertFalse(state.canActuate(ask), "\(state)/\(ask)")
                XCTAssertNotNil(state.blockedReason(for: ask), "\(state)/\(ask) must say why")
            }
        }
    }

    /// **Refusals pass through untouched — proven on an arbitrary string.**
    ///
    /// This used to be two hand-transcribed tables of the daemon's 22 sentences
    /// and three tests over them. That is a stale snapshot of Rust source with
    /// no build-time link to it: `banner.message == reason` where production
    /// assigns `reason` straight through is `f(x) == x`, and copying the
    /// sentences proves nothing about the daemon. Parity belongs to a
    /// daemon-emitted fixture (see `CodexRefusalClassifierTests`); passthrough
    /// belongs here, once.
    func testARefusalIsPassedThroughUntouched() {
        let arbitrary =
            "  Any sentence at all — 4 KiB of it, with «punctuation», a\nnewline, "
            + "and a trailing space "
        XCTAssertEqual(CodexProse.interrupt(.rejected(reason: arbitrary)).message, arbitrary)
        XCTAssertEqual(CodexProse.compose(.rejected(reason: arbitrary)).message, arbitrary)
        XCTAssertEqual(CodexProse.interrupt(.indeterminate(reason: arbitrary)).message, arbitrary)
        XCTAssertEqual(CodexProse.compose(.indeterminate(reason: arbitrary)).message, arbitrary)
    }

    /// **The title must not restate the sentence's own clause.** It read
    /// "Nothing was sent" over a daemon sentence ending `…; nothing was sent`.
    func testTheRefusalTitleDoesNotRestateTheSentence() {
        let sentence =
            "this Mac is connected to the Codex session but is not yet watching its thread, "
            + "so a message cannot be confirmed; nothing was sent"
        let banner = CodexProse.compose(.rejected(reason: sentence))
        XCTAssertEqual(banner.title, "The Mac refused this")
        XCTAssertFalse(
            banner.title.lowercased().contains("nothing was sent"),
            "the clause is already in the message: \(banner.title) / \(banner.message)")
    }

    /// **F8 — an unknown status claims nothing either way.** It used to decode
    /// as `.indeterminate`, which asserts the mutation *was* issued; a future
    /// `cancelled_before_send` would have read "Sent, outcome unknown".
    func testAnUnknownStatusClaimsNeitherActuationNorNonActuation() {
        for banner in [
            CodexProse.interrupt(.unknown(status: "cancelled_before_send")),
            CodexProse.compose(.unknown(status: "cancelled_before_send")),
        ] {
            XCTAssertTrue(banner.message.contains("cancelled_before_send"), banner.message)
            for forbidden in ["Sent,", "nothing was sent", "was written"] {
                XCTAssertFalse(
                    banner.message.contains(forbidden),
                    "\(forbidden) is a claim this build cannot make: \(banner.message)")
            }
        }
    }

    /// And the card's "what was chosen" line says the same — **never
    /// "Nothing"**, which would contradict the banner beside it.
    ///
    /// The arm that already read well is kept: an unknown write that *did* name
    /// an attempted option says which one, and says its outcome was never
    /// confirmed. The defect was the arms with no decision to name, which fell
    /// through to "Nothing. This question ended without an answer" — a
    /// definitive claim on the one screen that cannot make it.
    func testAnUnknownWriteDoesNotSayNothingHappened() {
        for resolution in [
            CodexResolution.unknown(
                attemptedBy: .phone, attemptedDecision: nil,
                writeStage: .upstreamWriteUnconfirmed, cause: "reset"),
            CodexResolution.unrecognisedStatus("teleported"),
        ] {
            let chosen = CodexProse.whatWasChosen(resolution)
            XCTAssertFalse(
                chosen.hasPrefix("Nothing."),
                "an unaccounted-for write did not end with nothing happening: \(chosen)")
            XCTAssertTrue(chosen.hasPrefix("Not known"), chosen)
        }

        // The named-option arm still names it, and still refuses to confirm.
        let named = CodexProse.whatWasChosen(
            .unknown(
                attemptedBy: .phone, attemptedDecision: .optionId("accept"),
                writeStage: .upstreamWriteUnconfirmed, cause: "reset"))
        XCTAssertTrue(named.contains("accept"), named)
        XCTAssertTrue(named.contains("never confirmed"), named)
    }
}
