import Foundation

/// How a decision card may be answered, and the question it asks: the rules every app's decision
/// card follows, so no surface offers an answer another withholds.
public enum DecisionCard {
    /// **How this card may be answered at all** — one decision, read by the
    /// action bar, the option rows and `submit`, so no answer surface can be
    /// offered that another withholds.
    ///
    /// The two agents do not share an answer vocabulary and the Mac enforces it
    /// in both directions: a Codex card answered with `allow`/`deny`/`option` is
    /// refused by name — *"a Codex card is answered by naming one of the options
    /// it offered; this decision names none of them, so nothing was sent"* — and
    /// an `option_id` aimed at a Claude session is refused just as plainly. So
    /// this is a fork, not an addition.
    public enum AnswerSurface: Sendable, Hashable {
        /// Claude's card, held for the phone: Allow, Deny, and a denial with a
        /// reason. "Always" is chosen at the Mac, where Claude's own dialog says
        /// what it saves.
        case allowDeny
        /// A Codex card: **the option table and nothing else.** Every count,
        /// including two — the `> 2` rule that suppresses Claude's redundant
        /// yes/no pair would leave a two-option Codex card with Allow and Deny,
        /// which are the exact two decisions it refuses, and the card would be
        /// unanswerable.
        case codexOptions([CodexCard.Option])
        /// **No answer this phone can honestly offer.** A Codex card whose
        /// options are missing or malformed, or a card on an agent this build
        /// does not know how to drive. The card is still readable; it is simply
        /// not answerable from here, and it says so rather than offering a
        /// vocabulary the Mac would refuse.
        case noneAnswerable
    }

    /// Decided from the **session's agent**, which is the daemon's own word for
    /// what is running — never from `tool_input`, which is content the agent
    /// itself authored.
    ///
    /// It was the card's shape, and that was a real hole: `tool_input` is
    /// arbitrary tool content, so a Claude tool that happened to emit an
    /// `options[]` table lost Allow and Deny and transmitted an `option_id` the
    /// Mac refuses by name — and a malformed Codex card with no usable options
    /// fell back to Allow/Deny, which Codex refuses. The discriminator has to be
    /// something the agent cannot write.
    ///
    /// For `.codex`, empty or malformed options make the card **non-actionable**
    /// rather than falling through: a Codex card is answered by naming one of
    /// its own options, and if there are none to name there is no honest answer
    /// this phone can offer.
    /// **A missing agent is not Claude.**
    ///
    /// `agent` is nil when the run has no live summary — it left the fleet, the
    /// daemon has not sent one yet, or the card outlived its session. That used
    /// to default to `.claude`, which is the one answer that can transmit: an
    /// `allow` for a run whose agent nobody knows. Unknown fails closed.
    public static func answerSurface(
        card: ApprovalCard, agent: AgentKind?, hold: QuestionHold?,
        hookOnlyApprovals: Bool, resolvesCodexCards: Bool = true
    ) -> AnswerSurface {
        guard let agent else { return .noneAnswerable }
        switch agent {
        case .codex:
            // **Old daemons.** Below minor 19 a Codex resolution carries no `request_id`,
            // so an answered card can never be retired — it would stay live and
            // tappable for ever. A card that cannot be retired must not be
            // answerable.
            guard resolvesCodexCards else { return .noneAnswerable }
            // **And a card whose rendering cannot be vouched for.** The options
            // are read out of `tool_input`; if the structured fields do not
            // reproduce the hashed `display_text`, the table on screen is not
            // provably the table the daemon sent, and answering by index into it
            // is answering a question nobody can check.
            guard card.verification.renderMatchesDisplayText else { return .noneAnswerable }
            let options = CodexCard.options(in: card.toolInput)
            return options.isEmpty ? .noneAnswerable : .codexOptions(options)
        case .claude:
            // **Claude's question is never Allow and never an option.** Either
            // types keys that pick answers nobody chose. A question this phone
            // can read is answered on `QuestionCardView`; one it cannot is
            // read-only here.
            guard card.toolName != QuestionCard.toolName else { return .noneAnswerable }
            // **Only through the hook Claude holds for this card's own call.** A
            // daemon that does not say so types the answer into whatever prompt
            // is on screen, which can be another agent's; and a card no hook is
            // held for is answered at the Mac.
            guard hookOnlyApprovals, hold == .held else { return .noneAnswerable }
            return .allowDeny
        case .unsupported:
            // An agent this build cannot drive gets no answer surface at all.
            // Guessing a vocabulary for it would be guessing on the wire.
            return .noneAnswerable
        }
    }

    /// **The question this card asks, when the phone can show it as asked.**
    ///
    /// Claude's `AskUserQuestion`, on a card whose structured `tool_input`
    /// reproduces the hashed text — the questions drawn are then provably the
    /// ones the daemon sent. Anything less falls back to the approval layout,
    /// read-only (`answerSurface`), showing the hashed text through `displaySafe`.
    public static func questionCard(card: ApprovalCard, agent: AgentKind?) -> QuestionCard? {
        guard agent == .claude, card.verification.hashMatchesDisplayText,
            card.verification.renderMatchesDisplayText
        else { return nil }
        return QuestionCard(card: card)
    }

    /// What a card whose text does not match its hash says, wherever it says it: the card's banner and the reason its
    /// controls cannot act.
    public static let hashMismatchSentence = "The text on this card does not match its hash. It cannot be answered safely."

    /// Whether the answer controls should be offered. Requires **live backing**
    /// (`isBacked`) and withdraws the moment the card carries an authoritative
    /// outcome or the local attempt is terminal. Static and pure so the whole
    /// guard is testable off the real resolver: a card with no live state behind
    /// it can never present an action bar, closing the "act on a departed/reset
    /// card" half of the class, and a resolved card disables it even while a
    /// stale non-terminal `.failed` is still stored locally.
    /// `codex` is the second authoritative ending, and it is checked here rather
    /// than mapped onto `outcome`: a Codex resolution says things `AnswerOutcome`
    /// cannot express — cleared because the turn was aborted, answered at the Mac
    /// with no decision recorded, retired because the item finished — and every
    /// one of them means the same thing about this bar, which is that it must go.
    public static func actionBarAvailable(
        outcome: AnswerOutcome?, codex: CodexResolution?, attempt: AnswerAttempt?, isBacked: Bool
    ) -> Bool {
        isBacked && outcome == nil && codex == nil && attempt?.isTerminal != true
    }

    /// **At HIGH, every answer but deny, deny with a reason and decline takes a second factor.** Allowing, choosing
    /// any option (a Codex card's "No" option included) or submitting a question's answers at HIGH waits for the
    /// device's own check (Face ID on the iPhone, the Mac's own); denying, denying with a reason and declining are the
    /// safe direction, and a product that promises silence never decides anything cannot put friction in front of
    /// them.
    public static func needsSecondFactor(risk: RiskClass, decision: AnswerDecision) -> Bool {
        guard risk == .high else { return false }
        switch decision {
        case .deny, .text, .decline: return false
        default: return true
        }
    }

    /// The read-before-you-decide gate, as a value.
    ///
    /// **This is a viewport test, and it was not one.** The previous implementation
    /// armed on `onAppear` of a 1pt marker nested inside a `LazyVStack` child, on
    /// the premise that laziness would make the callback mean "scrolled into view".
    /// Laziness operates on a stack's *direct children*; `commandBlock` was the
    /// second one, so it was materialised at first layout and the marker inside it
    /// fired then. `onAppear` is a lifecycle callback and never a visibility test.
    ///
    /// What that cost, read off the AX5 renders rather than off the code:
    ///
    ///  * A MEDIUM `Write` outside the worktree, with thirteen characters of the
    ///    path on screen and `Allow` drawn as the enabled white primary. At MEDIUM
    ///    there is no hold and no Face ID, so this gate is the *only* thing between
    ///    one tap and a write to an arbitrary path.
    ///  * A HIGH `git push --force origin main` with **zero characters of the
    ///    command rendered** and `Hold to allow` fully armed: a face authenticating
    ///    a command the product never showed.
    ///
    /// The rule lives here, apart from any view, so every app's card follows it and
    /// it can be tested without a viewport; each app measures the edges it reports.
    public struct ReadGate: Equatable, Sendable {
        /// The bottom edge of the block carrying the command has been inside
        /// the visible region at least once.
        public var hasSeenCommand = false
        /// …and so has the bottom edge of the risk class and its provenance.
        public var hasSeenProvenance = false

        public init() {}

        /// Both edges are measured in the scroll view's own space. `visibleBottom`
        /// is where the readable part of the scroll view ends: its own bottom, or
        /// the pinned action bar's top where that is higher.
        ///
        /// Every way of not knowing answers **no**: an unmeasured edge (`nil`), a
        /// layout that has not happened (`visibleBottom <= 0`), a non-finite value
        /// from a degenerate proposal, or a block that has scrolled off the top.
        /// Only a real measurement can open a gate.
        public static func isVisible(bottom: CGFloat?, visibleBottom: CGFloat) -> Bool {
            guard let bottom, bottom.isFinite, visibleBottom.isFinite else { return false }
            guard visibleBottom > 0, bottom > 0 else { return false }
            return bottom <= visibleBottom
        }

        /// Whether this class may be answered yet.
        ///
        /// **The gate applies at every tier**, and the reason is that risk governs
        /// *friction* — tap, hold, Face ID — and never whether the command has to be
        /// visible. LOW used to return `true` unconditionally on the argument that
        /// "a `Read` that costs a scroll is a gate people learn to defeat". The
        /// argument is sound and the implementation did not follow from it: at
        /// reading sizes a LOW command fits above the bar on the first frame, so the
        /// gate is already open by the time the card is drawn and costs the reader
        /// nothing at all. It is only when the command does *not* fit that the two
        /// behaviours differ, and that is precisely the case an AX5 render of a LOW
        /// card measured: `Allow` enabled, in the thumb zone, over **13 of the 24
        /// characters** of `/Users/dev/app/README.md` — the 11 that were missing
        /// being the ones that tell a README from `~/.aws/credentials`.
        ///
        /// A tier-shaped exception here is not "less friction for a small risk", it
        /// is "the product will let you approve something it never showed you", and
        /// that is not a property any tier gets.
        public func isOpen(at risk: RiskClass) -> Bool {
            switch risk {
            case .low:
                return hasSeenCommand
            case .medium:
                return hasSeenCommand
            // Stated separately rather than left to fall out of the layout: at HIGH
            // a face is about to authorise this, and the two facts that say what it
            // authorises are the command and the class it was given. That the one
            // is drawn above the other is not evidence that both were seen.
            case .high:
                return hasSeenCommand && hasSeenProvenance
            }
        }

        /// What a closed gate says. Drawn as the control's disabled reason, never only hinted.
        public func blockedReason(at risk: RiskClass) -> String? {
            guard !isOpen(at: risk) else { return nil }
            guard hasSeenCommand else { return "Scroll the command into view before deciding." }
            return "Scroll to the end of the command block before deciding."
        }
    }
}

extension AppModel {
    /// Why no control on `approval` can act now, in the order that matters — the
    /// same for every surface that answers a card.
    public func answerBlockedReason(for approval: ApprovalItem, inFlight: Bool) -> String? {
        // A second tap while the first is in flight would be a silent no-op;
        // saying "waiting" is the honest version of the same refusal.
        if inFlight { return "Waiting for your computer to confirm…" }
        if let reason = actionsBlockedReason { return reason }
        if let summary = shownSummary(for: approval.sessionKey) {
            let badge = FleetStatusRule.capability(summary: summary, capabilities: connection.capabilities)
            if let reason = badge.reason { return reason }
        }
        if !approval.card.verification.hashMatchesDisplayText {
            return DecisionCard.hashMismatchSentence
        }
        return nil
    }
}
