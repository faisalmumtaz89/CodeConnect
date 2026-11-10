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

    /// What a card's controls say while its answer is on the way, in the words of the device reading it: on a Mac,
    /// "your computer" would be the Mac itself.
    public static func waitingSentence(on host: HostDevice = .current) -> String {
        "Waiting for \(host.computer("your computer")) to confirm…"
    }

    /// **At HIGH, every answer but a refusal takes a second factor.** Allowing, choosing an option or submitting a
    /// question's answers at HIGH waits for the device's own check (Face ID on the iPhone, the Mac's own). Denying,
    /// denying with a reason, declining and a Codex card's `cancel` option ("No, and tell Codex what to do
    /// differently", the one refusal among the options the daemon offers) are the safe direction, and a product that
    /// promises silence never decides anything cannot put friction in front of them.
    public static func needsSecondFactor(risk: RiskClass, decision: AnswerDecision) -> Bool {
        guard risk == .high else { return false }
        switch decision {
        case .deny, .text, .decline: return false
        case .optionId(let id): return id != "cancel"
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
        if inFlight { return DecisionCard.waitingSentence() }
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

// MARK: - What became of an answer

extension DecisionCard {
    /// One banner on a decision card, in three tiers. The **title** is the classification, short because a view may
    /// uppercase it. The **message** is the sentence a human reads and a test looks for by name — sentence case,
    /// verbatim, never uppercased or abbreviated. The **provenance** underneath carries the original outcome, the
    /// transport and the daemon's own reason, because a duplicate that does not show you what the first answer was
    /// is just a refusal. The mark is an icon name, as `CodexProse.Banner` uses.
    public struct Banner: Sendable, Hashable {
        public let title: String
        public let message: String
        public let provenance: String?
        public let tone: CodexProse.CodexTone
        public let icon: String
    }

    /// What became of this session's own answer, in the words of the device reading it.
    public static func resultBanner(for attempt: AnswerAttempt, on host: HostDevice = .current) -> Banner {
        switch attempt {
        case .applied(let outcome):
            return Banner(
                title: "Confirmed",
                message: "\(outcome.decisionLabel), confirmed by \(host.computer("your computer"))",
                provenance: [actuationPhrase(for: outcome.appliedVia, on: host), outcome.detail]
                    .compactMap { $0 }.joined(separator: " · "),
                tone: .success, icon: "check-circle-fill")
        // The one honesty this case exists to protect: the daemon accepted the
        // answer but could not confirm it reached the agent, so it never reads
        // as confirmed or applied ("Unconfirmed", or "Sent" for an answer handed
        // to Claude's hook). Its provenance never uses the actuation phrase,
        // which would claim the exact actuation the daemon could not confirm.
        case .indeterminate(let outcome) where outcome.wasSentToClaude:
            return Banner(
                title: "Sent",
                message: "\(outcome.decisionLabel) · \(outcome.sentToClaudeLine(on: host))",
                provenance: "Claude takes the first answer, \(host.computer("the computer"))’s or this one, "
                    + "so whether it used this one is not known.",
                tone: .warning, icon: "question-fill")
        case .indeterminate(let outcome):
            return Banner(
                title: "Unconfirmed",
                message: "\(outcome.decisionLabel), but \(host.computer("your computer")) couldn’t confirm it landed",
                provenance: [
                    "\(host.computer("Your computer")) accepted this but never confirmed it reached the agent.",
                    outcome.detail,
                ].compactMap { $0 }.joined(separator: " · "),
                tone: .warning, icon: "question-fill")
        case .duplicate(let outcome, let stale):
            let original =
                "Original outcome: \(outcome.decisionLabel) \(outcome.provenance(on: host)), \(outcome.resolvedAt)."
                + (outcome.inferred
                    ? " \(host.computer("Your computer")) inferred that from the prompt disappearing "
                        + "rather than observing the answer."
                    : "")
            return Banner(
                title: "Duplicate", message: "Already answered",
                provenance: stale ? original + " The card you answered was also out of date." : original,
                tone: .warning, icon: "copy")
        case .answeredAtKeyboard(let reason):
            return Banner(
                title: "Not answered here",
                message: "Answered at \(host.computer("the computer"))",
                provenance: "The prompt was gone before the keystrokes could land, so somebody answered it on "
                    + "\(host.computer("the computer")). Nothing was typed. (\(reason))",
                tone: .warning, icon: "keyboard")
        case .staleCard(let reason):
            return Banner(
                title: "Out of date", message: "This card is out of date", provenance: reason, tone: .danger,
                icon: "clock-countdown")
        case .rejected(let reason):
            return Banner(
                title: "Refused", message: "\(host.computer("Your computer")) refused this answer",
                provenance: reason, tone: .danger, icon: "x-circle")
        case .failed(let reason):
            return Banner(
                title: "Not delivered", message: "The answer did not reach \(host.computer("your computer"))",
                provenance: "\(reason) Answers are idempotent. Retrying is safe.", tone: .danger, icon: "x-circle")
        }
    }

    /// How an answer was *applied*, in plain words — never a claim this build cannot support. An `AnswerPath.unknown`
    /// is a path this build has never heard of, so it is described as unrecognised rather than asserted to be
    /// keystrokes or a hook return nobody can vouch for.
    static func actuationPhrase(for path: AnswerPath, on host: HostDevice = .current) -> String {
        switch path {
        case .sendKeys: return "typed into the session on \(host.computer("your computer"))"
        case .hookReturn: return "handed straight to Claude"
        // A real actuation, and it reads like one. Codex asked, over its own
        // control link, and this is the answer written back to that request —
        // no keyboard, no pane, nothing inferred from a prompt disappearing.
        case .codexResponse: return "sent straight to Codex"
        case .unknown: return "applied in a way this app doesn’t recognise"
        }
    }

    /// A card whose recorded outcome arrived from the log rather than from this session's own answer. A **recorded**
    /// outcome carrying `indeterminate: true` — the shape the daemon replays for a locally resolved, never-confirmed
    /// answer — is "Unconfirmed", never "Already resolved", and never wears the confirming seal: the seal *is* the
    /// visual "confirmed" claim this rule exists to prevent.
    public static func alreadyResolvedBanner(
        for outcome: AnswerOutcome, now: Date, on host: HostDevice = .current
    ) -> Banner {
        let age = Format.ago(since: outcome.resolvedDate, now: now)
        let title: String
        let message: String
        if outcome.wasSentToClaude {
            title = "Sent"
            message = "\(outcome.decisionLabel) · \(outcome.sentToClaudeLine(on: host)) · \(age)"
        } else {
            title = outcome.indeterminate ? "Unconfirmed" : "Already resolved"
            let ending = "\(outcome.decisionLabel) \(outcome.provenance(on: host)) · \(age)"
            message =
                outcome.indeterminate
                ? "\(ending), but \(host.computer("your computer")) couldn’t confirm it reached the agent" : ending
        }
        return Banner(
            title: title, message: message, provenance: nil, tone: outcome.indeterminate ? .warning : .info,
            icon: outcome.indeterminate ? "question" : "seal-check")
    }

    /// No live state backs this card: the session left the fleet, the log was reset, or the card left the timeline.
    /// Non-actionable, and it says so rather than presenting a frozen card the reader could act on.
    public static func unavailableBanner(on host: HostDevice = .current) -> Banner {
        Banner(
            title: "No longer available",
            message: "This decision can't be acted on here — the run is no longer on \(host.computer("your computer")), "
                + "or its log was reset.",
            provenance: nil, tone: .warning, icon: "question")
    }

    /// The governing outcome, from the live lookup OR the snapshot the card was opened from. A recorded outcome from
    /// either source makes the card non-actionable, so a resolved card can never revert to actionable when a replay
    /// re-supplies its id as pending. A card pending in both sources is `nil`, and the `isBacked` gate decides
    /// whether it is live-pending or unavailable.
    public static func effectiveOutcome(live: AnswerOutcome?, snapshot: AnswerOutcome?) -> AnswerOutcome? {
        live ?? snapshot
    }

    /// Which resolved banner a card shows. See `resolvedBanner(attempt:persisted:isBacked:)`.
    public enum ResolvedBanner: Sendable {
        case attempt(AnswerAttempt)
        case persisted(AnswerOutcome)
        /// No live state backs this card, and this session holds no terminal receipt for it. Non-actionable by
        /// construction.
        case unavailable
        case none
    }

    /// Which resolved banner a card shows, given this session's own answer `attempt`, any authoritative outcome the
    /// LIVE card carries (`persisted`), and whether live state still backs the card at all (`isBacked`).
    ///
    /// The rules, in order:
    ///   * a **terminal** local attempt (`.applied`, `.indeterminate`, `.duplicate`, `.answeredAtKeyboard`) is this
    ///     session's own observation and keeps its richer receipt, even if the backing was later dropped;
    ///   * an authoritative persisted outcome outranks a *stale, non-terminal* local attempt — the crash/recovery
    ///     false-negative, where a dead-socket `.failed` would otherwise mask the recovered `approval_resolved`;
    ///   * **no live backing** and no terminal receipt ⇒ `.unavailable`: a departed/reset/left-the-log card is
    ///     neither actionable nor an outcome anyone can claim, so it must never fall back to a frozen actionable
    ///     snapshot or a stale local failure the user could retry;
    ///   * a live pending card with a non-terminal attempt still shows that attempt (a genuine failure the user may
    ///     retry); with none, `.none` (the ordinary pending path, action bar available).
    public static func resolvedBanner(
        attempt: AnswerAttempt?, persisted: AnswerOutcome?, isBacked: Bool
    ) -> ResolvedBanner {
        if let attempt {
            if attempt.isTerminal { return .attempt(attempt) }
            if let persisted { return .persisted(persisted) }
            if !isBacked { return .unavailable }
            return .attempt(attempt)
        }
        if let persisted { return .persisted(persisted) }
        if !isBacked { return .unavailable }
        return .none
    }
}

// MARK: - Why a card offers nothing to tap

extension DecisionCard {
    /// The run is gone, or has not been described yet: there is no vocabulary to offer, and never Claude's sentence
    /// by default.
    public static let notOnTheFleetReason =
        "This run is no longer on the fleet, so this card cannot be answered from here."

    /// Why a Codex card offers nothing to tap. `computer` is the run's computer by name, where the device knows it.
    ///
    /// Unknown is not old: while nothing is known of the computer (`isKnown` false, its link down) the card says what
    /// is known, which is the link — never the caveat, and never that the card came without its options.
    public static func codexUnanswerableReason(
        isKnown: Bool, caveat: String?, isVerified: Bool, linkReason: String?, computer: String? = nil,
        on host: HostDevice = .current
    ) -> String {
        guard isKnown else { return linkReason ?? cannotAnswerHere(computer: computer, on: host) }
        if let caveat { return caveat }
        guard isVerified else { return ApprovalCard.unverifiableCodexLine }
        return "This card arrived without the options Codex answers by, so it can only be answered at "
            + "\(host.computer("the computer", named: computer))."
    }

    /// Why a Claude card offers nothing to tap: the computer's CodeConnect would type the answer, or the card is not
    /// held for this device.
    ///
    /// `hookOnlyApprovals` is nil while nothing is known of the computer — its link is down and took its capabilities
    /// with it. Unknown is not old: the card then says what is known, which is the link.
    public static func claudeUnanswerableReason(
        isQuestion: Bool, hookOnlyApprovals: Bool?, linkReason: String?, hold: QuestionHold?,
        computer: String? = nil, on host: HostDevice = .current
    ) -> String {
        if isQuestion { return "This card cannot be answered from here." }
        guard let hookOnlyApprovals else { return linkReason ?? cannotAnswerHere(computer: computer, on: host) }
        guard hookOnlyApprovals else {
            return "\(host.computer("This computer", named: computer))'s CodeConnect is too old to answer Claude's "
                + "approvals from the \(host.noun). Update it, or answer at \(host.computer("the computer", named: computer))."
        }
        if hold == .atMac {
            return "Claude is asking this at \(host.computer("the computer", named: computer)), so answer it there."
        }
        return cannotAnswerHere(computer: computer, on: host)
    }

    /// A session of an agent this app does not know how to answer.
    public static func unsupportedAgentReason(
        _ raw: String, computer: String? = nil, on host: HostDevice = .current
    ) -> String {
        "This app does not know how to answer a \(raw) session. Answer it at "
            + "\(host.computer("the computer", named: computer))."
    }

    static func cannotAnswerHere(computer: String?, on host: HostDevice) -> String {
        "The \(host.noun) can no longer answer this. Answer it at \(host.computer("the computer", named: computer))."
    }

    /// An answer that left and is not yet confirmed, `elapsed` after it was sent: as text, or `spoken` for a screen
    /// reader.
    public static func stillWaiting(
        after elapsed: TimeInterval, spoken: Bool = false, computer: String? = nil, on host: HostDevice = .current
    ) -> String {
        let place = host.computer("the computer", named: computer)
        return spoken
            ? "Still waiting on \(place), \(Format.spokenDuration(elapsed))."
            : "Still waiting on \(place) - \(Format.age(elapsed))."
    }
}
