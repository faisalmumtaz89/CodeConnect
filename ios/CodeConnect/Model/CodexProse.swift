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
///   1. **The daemon's refusal sentences are shown verbatim.** All eleven of the
///      interrupt refusals and all ten of the compose ones name which condition
///      failed and what the operator can do instead; they were written to be
///      read. Nothing here truncates, re-words or prefixes one.
///   2. **Nothing claims an actuation the wire did not report.** `answered by
///      local` carries no decision at all — the Codex wire has no provenance for
///      a keyboard answer — so the copy says it does not know, rather than
///      naming a choice.
///   3. **No raw JSON on a product screen** (decision D9). The status words,
///      the write stage and the option ids are wire vocabulary; what the reader
///      sees is a sentence.
enum CodexProse {

    /// A banner's three parts. A struct rather than a tuple because five call
    /// sites read it and a positional tuple is where copy drifts.
    struct Banner: Sendable, Hashable {
        /// The classification, in a word or three. `CCBanner` uppercases it.
        let title: String
        /// The sentence a human reads and a test looks for by name. Never
        /// uppercased, never abbreviated.
        let message: String
        let icon: String
        let tone: CodexTone
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
        /// read. K2 fixed the rejection titles; this is the same seam, ruled
        /// once for every arm.
        var oneLine: String {
            messageIsVerbatim ? "\(title): \(message)" : "\(title). \(message)"
        }
    }

    /// The tones a Codex outcome uses, kept free of SwiftUI so this file — and
    /// its tests — need no view layer.
    enum CodexTone: Sendable, Hashable {
        case info, success, warning, danger
    }

    // MARK: What became of a card

    /// The one banner a resolved Codex card shows.
    ///
    /// **Never the app's `.unavailable` copy** ("the run left the daemon's list
    /// or its log was reset"). That sentence is about the phone losing sight of
    /// a run; every sentence here is about the *question* ending, which is a
    /// different fact and usually a completely ordinary one.
    static func resolution(_ resolution: CodexResolution) -> Banner {
        switch resolution {
        case .answered(let by, let decision):
            return answered(by: by, decision: decision)

        case .cleared(.turnAborted):
            return Banner(
                title: "The turn was stopped",
                message:
                    "This question went away with it. Nothing was approved and nothing was denied.",
                icon: "stop.circle", tone: .warning)

        case .cleared(.turnCompleted):
            return Banner(
                title: "The turn ended",
                message: "Codex finished before this was answered, and the question went with it.",
                icon: "checkmark.circle", tone: .info)

        case .cleared(.superseded):
            return Banner(
                title: "Replaced by a newer question",
                message:
                    "Codex asked something else instead. Nothing was approved and nothing was denied.",
                icon: "arrow.triangle.2.circlepath", tone: .info)

        // Deliberately **not** the `turn_completed` sentence: the turn is still
        // running, and telling a reader the session finished when it is still
        // working is the exact confusion the named case exists to prevent.
        case .cleared(.itemCompleted):
            return Banner(
                title: "This step finished",
                message: "It was settled at the Mac. Codex is still working.",
                icon: "checkmark.circle", tone: .info)

        case .cleared(.unknown(let raw)):
            return Banner(
                title: "The question was withdrawn",
                message:
                    "The Mac gave a reason this app does not recognise (\(raw)). "
                    + "Nothing was approved and nothing was denied.",
                icon: "questionmark.circle", tone: .warning)

        case .timeout:
            return Banner(
                title: "No answer arrived in time",
                message: "Codex stopped waiting. Nothing was approved and nothing was denied.",
                icon: "clock.badge.exclamationmark", tone: .warning)

        // The daemon attempted a write and never confirmed it. Its `cause` is
        // human text written to be shown, so it is shown — and the stage says
        // how far the attempt got, in words rather than in its wire spelling.
        case .unknown(let attemptedBy, _, let stage, let cause):
            let who = attemptedBy == .phone ? "This phone's answer" : "The answer"
            return Banner(
                title: "Sent, outcome unknown",
                message:
                    "\(who) was written and the Mac did not learn what became of it. "
                    + "\(writeStage(stage)) It will not be sent again — check the Mac. (\(cause))",
                icon: "questionmark.circle", tone: .warning)

        case .unrecognisedStatus(let raw):
            return Banner(
                title: "Ending not recognised",
                message:
                    "The Mac reported an ending this app has never seen (\(raw)), so it cannot say "
                    + "what became of this question. Check the Mac.",
                icon: "questionmark.circle", tone: .warning)
        }
    }

    private static func answered(by: ResolutionActor, decision: AnswerDecision?) -> Banner {
        switch by {
        case .phone:
            return Banner(
                title: "Answered from this phone",
                message: "Codex has the answer.",
                icon: "checkmark.seal", tone: .success)
        // **The honesty this arm exists for.** `decision` is absent for a
        // keyboard answer and always will be: upstream carries no provenance,
        // so the daemon knows somebody answered and not what they chose.
        case .local:
            return Banner(
                title: "Answered at the Mac",
                message:
                    "Someone answered at the keyboard before this phone did. "
                    + "Codex does not report which choice they made.",
                icon: "keyboard", tone: .info)
        case .unknown(let raw):
            return Banner(
                title: "Answered",
                message:
                    "The Mac names an answerer this app does not recognise (\(raw)), "
                    + "so it cannot say where this was decided.",
                icon: "questionmark.circle", tone: .warning)
        }
    }

    /// What was chosen, when the wire said. `nil` when nobody may claim one —
    /// which is the common case for a keyboard answer, and is stated as a
    /// sentence rather than left as a blank space.
    ///
    /// The option **id** is what the wire carries and what the daemon validated,
    /// so it is what is shown. Rendering the label instead would mean matching
    /// an id against the card's own table and quietly showing nothing when the
    /// card is gone — a caption that disappears is worse than one that is exact.
    static func whatWasChosen(_ resolution: CodexResolution) -> String {
        switch resolution {
        case .answered(_, .some(.optionId(let id))):
            return "The option “\(id)”."
        case .answered(_, .some(let decision)):
            return decision.label + "."
        case .answered(.local, nil):
            return "Not known. The Codex wire carries no provenance for a keyboard answer."
        case .answered(_, nil):
            return "Not known. The Mac recorded who answered but not what they chose."
        case .unknown(_, .some(.optionId(let id)), _, _):
            return "The option “\(id)” was written, and its outcome was never confirmed."
        case .cleared, .timeout:
            return "Nothing. This question ended without an answer."
        // **Not "Nothing".** An unknown write, or a status this build cannot
        // read, is precisely the case where the app does not know whether a
        // choice landed — and the adjacent banner says so. Saying "Nothing"
        // beside it would be the same screen contradicting itself.
        case .unknown, .unrecognisedStatus:
            return "Not known. The Mac could not account for what became of this."
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
    static func interrupt(_ result: InterruptResult) -> Banner {
        switch result {
        case .aborted:
            return Banner(
                title: "Stopped",
                message: "The turn reached its aborted boundary.",
                icon: "stop.circle.fill", tone: .success)
        case .duplicate:
            return Banner(
                title: "Already stopped",
                message: "This exact request already did that. The turn was not stopped twice.",
                icon: "doc.on.doc", tone: .info)
        // **The title must not restate the sentence.** It read "Nothing was
        // sent" over a daemon sentence that itself ends `…; nothing was sent` —
        // the clause twice, with a full stop running into a lowercase seam.
        // Measured on `codex-compose-rejected--L.png`. The message stays
        // verbatim; the title says who refused, which the sentence does not.
        case .rejected(let reason):
            return Banner(
                title: "The Mac refused this",
                message: reason,
                icon: "exclamationmark.triangle.fill", tone: .warning,
                messageIsVerbatim: true)
        // **Never a retry.** The stop was issued and the daemon did not live to
        // see what it did; offering a resend would risk aborting a turn the
        // reader never meant to touch.
        case .indeterminate(let reason):
            return Banner(
                title: "Sent, outcome unknown",
                message: reason,
                icon: "questionmark.circle", tone: .warning,
                messageIsVerbatim: true)
        case .unknown(let status):
            return Banner(
                title: "Answer not recognised",
                message: Self.unknownStatusSentence(status),
                icon: "questionmark.circle", tone: .warning)
        }
    }

    /// **Claims neither actuation nor non-actuation.** A word this build cannot
    /// interpret says nothing about whether the mutation happened, and the copy
    /// must not decide for it in either direction.
    static func unknownStatusSentence(_ status: String) -> String {
        "The Mac answered with a status this app does not know (“\(status)”), so what became of "
            + "this is not something the app can say. Update the app, and check the Mac."
    }

    // MARK: What became of a message

    /// The five `ComposeResult` arms.
    ///
    /// `started` and `steered` are **never the same sentence**: which one
    /// arrived is the whole answer to "what did my words do", and it was decided
    /// by what the session was doing at the instant the daemon wrote — not by
    /// anything the reader could have predicted when they tapped.
    static func compose(_ result: ComposeResult) -> Banner {
        switch result {
        case .started:
            return Banner(
                title: "Your words started a new turn",
                message: "Codex was idle, so this began the turn it is running now.",
                icon: "play.circle", tone: .success)
        case .steered:
            return Banner(
                title: "Your words joined the running turn",
                message: "Codex was already working, so this went in alongside what it was doing.",
                icon: "arrow.turn.down.right", tone: .success)
        // A replay reports the route **snapshotted at the claim**, so the verb
        // is the one that was true when the words landed, not the one that
        // would be true now.
        case .duplicate(_, let started):
            return Banner(
                title: started
                    ? "Your words started a new turn" : "Your words joined the running turn",
                message:
                    "This was already sent. Nothing was said a second time — "
                    + "this is the same answer you got the first time.",
                icon: "doc.on.doc", tone: .info)
        case .rejected(let reason):
            return Banner(
                title: "The Mac refused this",
                message: reason,
                icon: "exclamationmark.triangle.fill", tone: .warning,
                messageIsVerbatim: true)
        // Written, outcome unknown, **never retried automatically** — and the
        // phone must not offer a retry that would resend.
        case .indeterminate(let reason):
            return Banner(
                title: "Sent, outcome unknown",
                message: reason,
                icon: "questionmark.circle", tone: .warning,
                messageIsVerbatim: true)
        case .unknown(let status):
            return Banner(
                title: "Answer not recognised",
                message: Self.unknownStatusSentence(status),
                icon: "questionmark.circle", tone: .warning)
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
        agent: AgentKind, daemonHonoursStop: Bool, link: CodexLinkState, runningTurn: String?
    ) -> String? {
        guard agent == .codex else { return "Only a Codex session can be stopped from here." }
        guard daemonHonoursStop else {
            return "This Mac's CodeConnect is too old to stop a Codex turn. Update it."
        }
        // **The one honest hide** is the absent turn: with nothing to name there
        // is nothing to send, so the control is absent rather than dead.
        guard runningTurn != nil else { return "Nothing is running to stop." }
        return link.blockedReason
    }

    /// Why the composer cannot send, or nil when it can. Same shape, same order.
    static func composeUnavailable(
        agent: AgentKind, daemonUnderstandsCompose: Bool, link: CodexLinkState
    ) -> String? {
        guard agent == .codex else { return "Only a Codex session is spoken to this way." }
        guard daemonUnderstandsCompose else {
            return "This Mac's CodeConnect is too old to carry a message to Codex. Update it."
        }
        return link.blockedReason
    }
}
