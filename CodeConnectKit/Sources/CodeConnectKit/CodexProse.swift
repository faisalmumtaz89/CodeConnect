import Foundation

/// **Everything a Codex outcome is allowed to say, as pure functions.**
///
/// One place, so the card, the fleet row, the session timeline and the composer
/// cannot describe the same fact three ways — and so every sentence can be
/// asserted by name without standing up a `View`. `AgentProse` does the same job
/// for the Claude surfaces; this is its Codex half.
///
/// Three rules govern every string below, and each of them was earned:
///
///   1. **The daemon's refusal sentences are shown verbatim.** Each names which
///      condition failed and what the operator can do instead; they were written
///      to be read. Nothing here truncates, re-words or prefixes one.
///   2. **Nothing claims an actuation the wire did not report.** An answer whose
///      record names no decision says it does not know, rather than naming a
///      choice.
///   3. **No raw JSON on a product screen.** The status words,
///      the write stage and the option ids are wire vocabulary; what the reader
///      sees is a sentence.
public enum CodexProse {

    /// A banner's three parts. A struct rather than a tuple because five call
    /// sites read it and a positional tuple is where copy drifts.
    public struct Banner: Sendable, Hashable {
        /// The classification, in a word or three. `CCBanner` uppercases it.
        public let title: String
        /// The sentence a human reads and a test looks for by name. Never
        /// uppercased, never abbreviated.
        public let message: String
        public let icon: String
        public let tone: CodexTone
        /// **Whether `message` is the daemon's own sentence, verbatim.**
        ///
        /// The daemon writes its refusals as clauses: they begin lowercase and
        /// carry their own punctuation. That is a fact about the string, so it
        /// is recorded here rather than guessed at each of the three places
        /// that draw it on one line.
        var messageIsVerbatim = false

        /// Title and sentence on one line, for the surfaces that have one line:
        /// the composer note, the fleet's stop note, a row's accessibility
        /// label.
        ///
        /// A colon before the daemon's words and a full stop before the app's.
        /// Both were a full stop, which produced *"Sent, outcome unknown. this
        /// interrupt was already sent…"* — a sentence beginning after a period
        /// with a lowercase letter, on the surface whose whole job is to be
        /// read. The rejection titles had the same seam; it is ruled here once
        /// for every arm.
        public var oneLine: String {
            messageIsVerbatim ? "\(title): \(message)" : "\(title). \(message)"
        }
    }

    /// The tones a Codex outcome uses, kept free of SwiftUI so this file — and
    /// its tests — need no view layer.
    public enum CodexTone: Sendable, Hashable {
        case info, success, warning, danger
    }

    // MARK: What became of a card

    /// The one banner a resolved Codex card shows.
    ///
    /// **Never the app's `.unavailable` copy** ("the run is no longer on your
    /// computer, or its log was reset"). That sentence is about the phone losing sight of
    /// a run; every sentence here is about the *question* ending, which is a
    /// different fact and usually a completely ordinary one.
    public static func resolution(_ resolution: CodexResolution, on host: HostDevice = .current) -> Banner {
        switch resolution {
        case .answered(let by, let decision, let answeredOn):
            return answered(by: by, decision: decision, answeredOn: answeredOn, on: host)

        case .cleared(.turnAborted):
            return Banner(
                title: "The turn was stopped",
                message:
                    "This question went away with it. Nothing was approved and nothing was denied.",
                icon: "stop-circle", tone: .warning)

        case .cleared(.turnCompleted):
            return Banner(
                title: "The turn ended",
                message: "Codex finished before this was answered, and the question went with it.",
                icon: "check-circle", tone: .info)

        case .cleared(.superseded):
            return Banner(
                title: "Replaced by a newer question",
                message:
                    "Codex asked something else instead. Nothing was approved and nothing was denied.",
                icon: "arrows-clockwise", tone: .info)

        // Deliberately **not** the `turn_completed` sentence: the turn is still
        // running, and telling a reader the session finished when it is still
        // working is the exact confusion the named case exists to prevent.
        case .cleared(.itemCompleted):
            return Banner(
                title: "This step finished",
                message: "It was settled at \(host.computer("the computer")). Codex is still working.",
                icon: "check-circle", tone: .info)

        case .cleared(.unknown(let raw)):
            return Banner(
                title: "The question was withdrawn",
                message:
                    "\(host.computer("The computer")) gave a reason this app does not recognise (\(raw)). "
                    + "Nothing was approved and nothing was denied.",
                icon: "question", tone: .warning)

        case .timeout:
            return Banner(
                title: "No answer arrived in time",
                message: "Codex stopped waiting. Nothing was approved and nothing was denied.",
                icon: "clock-countdown", tone: .warning)

        // The daemon attempted a write and never confirmed it. Its `cause` is
        // human text written to be shown, so it is shown — and the stage says
        // how far the attempt got, in words rather than in its wire spelling.
        case .unknown(let attemptedBy, _, let stage, let cause):
            // A paired device's attempt, which on a Mac may be any of them: the iPhone keeps its words.
            let who =
                attemptedBy == .phone
                ? AnswerOutcome.answeringDevice(nil, on: host).map { "The answer from \($0)" }
                    ?? "This \(host.noun)'s answer"
                : "The answer"
            return Banner(
                title: "Sent, outcome unknown",
                message:
                    "\(who) was written and \(host.computer("the computer")) did not learn what became of it. "
                    + "\(writeStage(stage)) It will not be sent again — check \(host.computer("the computer")). (\(cause))",
                icon: "question", tone: .warning)

        case .unrecognisedStatus(let raw):
            return Banner(
                title: "Ending not recognised",
                message:
                    "\(host.computer("The computer")) reported an ending this app has never seen (\(raw)), so it cannot say "
                    + "what became of this question. Check \(host.computer("the computer")).",
                icon: "question", tone: .warning)
        }
    }

    private static func answered(
        by: ResolutionActor, decision: AnswerDecision?, answeredOn: AnsweredOn?, on host: HostDevice
    ) -> Banner {
        switch by {
        case .phone:
            // `phone` is any paired device, named by the kind `answeredOn` records, as a Claude answer's provenance is;
            // where the iPhone has no other words, "a phone".
            return Banner(
                title: "Answered from \(AnswerOutcome.answeringDevice(answeredOn, on: host) ?? "a phone")",
                message: "Codex has the answer.",
                icon: "seal-check", tone: .success)
        case .unknown(let raw):
            return Banner(
                title: "Answered",
                message:
                    "\(host.computer("The computer")) names an answerer this app does not recognise (\(raw)), "
                    + "so it cannot say where this was decided.",
                icon: "question", tone: .warning)
        }
    }

    /// What was chosen, when the wire said. `nil` when nobody may claim one,
    /// which is stated as a sentence rather than left as a blank space.
    ///
    /// An option is named by its label on `card`, the words the reader chose it by, when the card's table is the one
    /// the daemon sent (`renderMatchesDisplayText`) and holds that id. Otherwise it is named by its **id**, what the
    /// wire carries and the daemon validated: a card that is gone, or a table that no longer holds the id, still gets
    /// an exact caption rather than none.
    public static func whatWasChosen(_ resolution: CodexResolution, card: ApprovalCard?) -> String {
        let options = card.map { $0.verification.renderMatchesDisplayText ? CodexCard.options(in: $0.toolInput) : [] } ?? []
        func label(_ id: String) -> String? { options.first { $0.id == id }?.label }
        switch resolution {
        case .answered(_, .some(.optionId(let id)), _):
            return label(id).map { $0 + "." } ?? "The option “\(id)”."
        case .answered(_, .some(let decision), _):
            return decision.label + "."
        case .answered(_, nil, _):
            return "Not known. \(HostDevice.current.computer("The computer")) recorded who answered but not what they chose."
        case .unknown(_, .some(.optionId(let id)), _, _):
            return "The option “\(label(id) ?? id)” was written, and its outcome was never confirmed."
        case .cleared, .timeout:
            return "Nothing. This question ended without an answer."
        // **Not "Nothing".** An unknown write, or a status this build cannot
        // read, is precisely the case where the app does not know whether a
        // choice landed — and the adjacent banner says so. Saying "Nothing"
        // beside it would be the same screen contradicting itself.
        case .unknown, .unrecognisedStatus:
            return "Not known. \(HostDevice.current.computer("The computer")) could not account for what became of this."
        }
    }

    private static func writeStage(_ stage: WriteStage) -> String {
        switch stage {
        case .claimedNotEnqueued:
            return "It was claimed but never handed to the link."
        case .brokerIngressAccepted:
            return "The link took it; nothing confirmed it reached Codex."
        case .upstreamWriteUnconfirmed:
            return "It was written to Codex and never acknowledged."
        case .unknown:
            return "How far it got is not something this app can read."
        }
    }

    // MARK: What became of a stop

    /// The four `InterruptResult` arms. **Refusals verbatim** — a daemon
    /// sentence is the only thing on this screen that knows which of eleven
    /// conditions failed, and paraphrasing it would throw that away.
    public static func interrupt(_ result: InterruptResult, agent: AgentKind) -> Banner {
        switch result {
        case .aborted:
            return Banner(
                title: "Stopped",
                message: "\(agent.displayName) stopped the turn.",
                icon: "stop-circle-fill", tone: .success)
        case .duplicate:
            return Banner(
                title: "Already stopped",
                message: "This exact request already did that. The turn was not stopped twice.",
                icon: "copy", tone: .info)
        // **The title must not restate the sentence.** It read "Nothing was
        // sent" over a daemon sentence that itself ends `…; nothing was sent` —
        // the clause twice, with a full stop running into a lowercase seam.
        // Measured on `codex-compose-rejected--L.png`. The message stays
        // verbatim; the title says who refused, which the sentence does not.
        case .rejected(let reason):
            return Banner(
                title: "\(HostDevice.current.computer("The computer")) refused this",
                message: reason,
                icon: "warning-fill", tone: .warning,
                messageIsVerbatim: true)
        // **Never a retry.** The stop was issued and the daemon did not live to
        // see what it did; offering a resend would risk aborting a turn the
        // reader never meant to touch.
        case .indeterminate(let reason):
            return Banner(
                title: "Sent, outcome unknown",
                message: reason,
                icon: "question", tone: .warning,
                messageIsVerbatim: true)
        case .unknown(let status):
            return Banner(
                title: "Answer not recognised",
                message: Self.unknownStatusSentence(status),
                icon: "question", tone: .warning)
        }
    }

    /// **Claims neither actuation nor non-actuation.** A word this build cannot
    /// interpret says nothing about whether the mutation happened, and the copy
    /// must not decide for it in either direction.
    static func unknownStatusSentence(_ status: String) -> String {
        "\(HostDevice.current.computer("The computer")) answered with a status this app does not know (“\(status)”), so what became "
            + "of this is not something the app can say. Update the app, and check \(HostDevice.current.computer("the computer"))."
    }

    // MARK: What became of a message

    /// What became of a message, or nil when it landed (started, steered, or a replay of one that did): the
    /// message appearing in the conversation says so.
    public static func compose(_ result: ComposeResult) -> Banner? {
        switch result {
        case .started, .steered, .duplicate:
            return nil
        case .rejected(let reason):
            return Banner(
                title: "\(HostDevice.current.computer("The computer")) refused this",
                message: reason,
                icon: "warning-fill", tone: .warning,
                messageIsVerbatim: true)
        // Written, outcome unknown, **never retried automatically** — and the
        // phone must not offer a retry that would resend.
        case .indeterminate(let reason):
            return Banner(
                title: "Sent, outcome unknown",
                message: reason,
                icon: "question", tone: .warning,
                messageIsVerbatim: true)
        case .unknown(let status):
            return Banner(
                title: "Answer not recognised",
                message: Self.unknownStatusSentence(status),
                icon: "question", tone: .warning)
        }
    }

    // MARK: Why a control is not offered

    /// Why Stop is not being offered, or nil when it is.
    ///
    /// The order is the order a reader would ask the questions in, and each
    /// answer is a fact the phone genuinely holds — none of them is a guess
    /// about what the Mac would say if asked. The daemon's own refusal comes
    /// *after* a tap, and says which of its eleven conditions failed.
    /// **This is the gate**, not a description of it.
    ///
    /// `AppModel.stopCodexTurn` calls this before it mints anything, and every
    /// Stop surface calls it to decide what to draw. It used to be one of three
    /// readings of the same rule — the fleet row and the session header each
    /// re-derived their own — and the three disagreed: the views checked the
    /// link and the send path did not, so an `interrupt` left for sessions the
    /// phone already knew were `bound`, `offline` or `none`. Measured at the
    /// send stub. One rule, one reader.
    static func stopUnavailable(
        agent: AgentKind, daemonHonoursStop: Bool, isLive: Bool,
        link: CodexLinkState, runLink: Link, runningTurn: String?
    ) -> String? {
        switch agent {
        case .codex:
            guard daemonHonoursStop else {
                return "\(HostDevice.current.computer("This computer"))'s CodeConnect is too old to stop a Codex turn. Update it."
            }
            // **The one honest hide** is the absent turn: with nothing to name
            // there is nothing to send, so the control is absent rather than dead.
            guard runningTurn != nil else { return "Nothing is running to stop." }
            return link.blockedReason(for: .stop)
        case .claude:
            // A Claude run is stopped when it is live, a turn is actually running,
            // and its own link to the host is attached. No Codex control-link
            // check — its interrupt does not travel the Codex link — but it does
            // ride ccd's link to the host.
            //
            // These reasons are plain, readable sentences rather than the daemon's
            // own uid-prefixed operator rows because of where they surface: the
            // views read `stopUnavailable` only as `== nil` for button visibility
            // and the Stop button's accessibility label is static, so a person
            // never hears these from the control. They reach a person only through
            // the send path — `stopBlocked → stopNotSent` into the Stop outcome
            // banner — in the race where the run's state changes between the button
            // showing and the tap landing (the run ends under a shown button).
            guard daemonHonoursStop else {
                return "\(HostDevice.current.computer("This computer"))'s CodeConnect is too old to stop a Claude turn. Update it."
            }
            guard isLive else { return "This run has ended, so there is no turn to stop." }
            guard runningTurn != nil else { return "Nothing is running to stop." }
            // The interrupt rides ccd's own link to the host; when that link has dropped the daemon
            // buffers for re-attach and cannot deliver the Stop, so it is not offered. A detached run
            // stays live with its turn still running, so "live + a turn" do not catch it — the
            // link does (G-1).
            guard runLink == .attached else {
                return "This run has lost its link to \(HostDevice.current.computer("the computer")), so the Stop cannot reach it."
            }
            return nil
        case .unsupported:
            return "Only a Claude or Codex run can be stopped from here."
        }
    }

    /// Why the composer cannot send, or nil when it can. Same shape, same order.
    static func composeUnavailable(
        agent: AgentKind, daemonUnderstandsCompose: Bool, link: CodexLinkState
    ) -> String? {
        guard agent == .codex else { return "Only a Codex run is spoken to this way." }
        guard daemonUnderstandsCompose else {
            return "\(HostDevice.current.computer("This computer"))'s CodeConnect is too old to carry a message to Codex. Update it."
        }
        return link.blockedReason(for: .compose)
    }
}
