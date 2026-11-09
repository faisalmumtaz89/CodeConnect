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
}
