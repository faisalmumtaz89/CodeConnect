import Foundation

/// Claude's `AskUserQuestion`, read from the card's verified `tool_input`.
///
/// Claude Code asks every such question through a `PermissionRequest`, so it
/// arrives as an approval card — and an Allow on it typed keys that picked
/// options nobody chose. From protocol minor 21 the daemon answers it through
/// Claude's own hook instead: the phone sends the choices as option indices and
/// the daemon builds Claude's answer from the question it stored
/// (`mac/ccd/src/question.rs`). Nothing is typed, and this card has no Allow.
struct QuestionCard: Sendable, Hashable {
    static let toolName = "AskUserQuestion"

    struct Option: Sendable, Hashable {
        let label: String
        let description: String?
        /// What the option would look like, drawn monospace under it. Nil when
        /// absent or blank, which is how the daemon reads it too.
        let preview: String?
    }

    struct Question: Sendable, Hashable {
        /// The short chip Claude puts above the question.
        let header: String?
        let text: String
        let multiSelect: Bool
        let options: [Option]

        /// Notes are taken on a single choice whose options carry a preview,
        /// and nowhere else — the terminal's `n to add notes`, and the only
        /// questions the daemon accepts notes on.
        var takesNotes: Bool { !multiSelect && options.contains { $0.preview != nil } }

        /// "Other" is offered wherever the terminal offers "Type something":
        /// everywhere but the preview layout, which has none, so the daemon
        /// refuses an "Other" there.
        var takesOther: Bool { !takesNotes }
    }

    let questions: [Question]

    /// Nil for any other card, and for a question whose shape this build cannot
    /// read — which is then shown read-only, never guessed at.
    init?(card: ApprovalCard) {
        guard card.toolName == Self.toolName,
            let list = card.toolInput["questions"]?.arrayValue, !list.isEmpty
        else { return nil }
        var questions: [Question] = []
        for item in list {
            guard let text = item["question"]?.stringValue,
                let rawOptions = item["options"]?.arrayValue, !rawOptions.isEmpty
            else { return nil }
            var options: [Option] = []
            for option in rawOptions {
                guard let label = option["label"]?.stringValue else { return nil }
                let preview = option["preview"]?.stringValue
                options.append(
                    Option(
                        label: label, description: option["description"]?.stringValue,
                        preview: preview?.trimmingCharacters(in: .whitespacesAndNewlines)
                            .isEmpty == false ? preview : nil))
            }
            questions.append(
                Question(
                    header: item["header"]?.stringValue, text: text,
                    multiSelect: item["multiSelect"]?.boolValue ?? false, options: options))
        }
        self.questions = questions
    }

    /// What one question's answer reads as — the chosen labels in the order
    /// they were chosen, then the "Other" text, joined with `", "` as the
    /// terminal shows several choices. Nil when nothing is chosen.
    func summary(of answer: QuestionAnswer, for index: Int) -> String? {
        let options = questions[index].options
        var items = answer.selected.compactMap { selected in
            Int(selected) < options.count ? options[Int(selected)].label : nil
        }
        if let other = answer.other { items.append(other) }
        return items.isEmpty ? nil : items.joined(separator: ", ")
    }
}

/// The reader's choices on a question card, before they are sent.
///
/// The rules are the daemon's (`question.rs`): a single choice is exactly one
/// option or "Other"; several choices keep the order they were tapped in, plus
/// at most one "Other"; an "Other" with nothing typed is not an answer; notes
/// only where the question takes them.
struct QuestionDraft: Sendable, Hashable {
    struct Entry: Sendable, Hashable {
        /// Option indices, in the order they were tapped.
        var selected: [Int] = []
        var otherChosen = false
        var otherText = ""
        var notes = ""
    }

    var entries: [Entry]

    init(card: QuestionCard) {
        entries = Array(repeating: Entry(), count: card.questions.count)
    }

    /// A tap on option `option` of question `question`.
    mutating func tap(option: Int, of question: Int, in card: QuestionCard) {
        if card.questions[question].multiSelect {
            if let at = entries[question].selected.firstIndex(of: option) {
                entries[question].selected.remove(at: at)
            } else {
                entries[question].selected.append(option)
            }
        } else {
            entries[question].selected = [option]
            entries[question].otherChosen = false
        }
    }

    /// A tap on the "Other" row of question `question`.
    mutating func tapOther(of question: Int, in card: QuestionCard) {
        guard card.questions[question].takesOther else { return }
        if card.questions[question].multiSelect {
            entries[question].otherChosen.toggle()
        } else {
            entries[question].selected = []
            entries[question].otherChosen = true
        }
    }

    /// This question's answer as the wire carries it, or nil while it has none.
    func answer(for question: Int, in card: QuestionCard) -> QuestionAnswer? {
        let entry = entries[question]
        if entry.otherChosen,
            entry.otherText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            return nil
        }
        guard !entry.selected.isEmpty || entry.otherChosen else { return nil }
        let notes = entry.notes.trimmingCharacters(in: .whitespacesAndNewlines)
        return QuestionAnswer(
            selected: entry.selected.map { UInt32($0) },
            other: entry.otherChosen ? entry.otherText : nil,
            notes: card.questions[question].takesNotes && !notes.isEmpty ? notes : nil)
    }

    /// Every question's answer, in card order — or nil until all of them have
    /// one. Submit is offered on exactly this.
    func answers(in card: QuestionCard) -> [QuestionAnswer]? {
        var all: [QuestionAnswer] = []
        for index in card.questions.indices {
            guard let answer = answer(for: index, in: card) else { return nil }
            all.append(answer)
        }
        return all
    }
}

/// Where a question card stands, and so what it may offer.
enum QuestionCardStatus: Sendable, Hashable {
    /// Claude is waiting on the daemon, and the phone may answer.
    case answerable
    /// The question is on the Mac and only the Mac can answer it.
    case askingAtMac
    /// The phone's hold ended, or was never stated; the question may still be
    /// open at the Mac.
    case answerAtMac
    /// The Mac's CodeConnect predates answering questions from the phone.
    case macTooOld
    /// Answered from this phone; carries what was sent.
    case answeredOnPhone([QuestionAnswer])
    case answeredAtMac
    case declined(byPhone: Bool, withReply: Bool)
    /// The daemon took the answer and could not confirm Claude received it.
    case unconfirmed
    /// Retired without anybody answering it here or at the Mac.
    case closed
    /// No live state backs this card any more.
    case unavailable

    var isAnswerable: Bool { self == .answerable }

    /// One decision for the whole card, so no control can be offered that the
    /// status withholds. A recorded ending outranks everything; a card nothing
    /// backs is never answerable; then the daemon's age, then the hold.
    static func resolve(
        outcome: AnswerOutcome?, attempt: AnswerAttempt?, isBacked: Bool,
        hold: QuestionHold?, answersQuestions: Bool
    ) -> QuestionCardStatus {
        switch attempt {
        case .applied(let outcome), .indeterminate(let outcome), .duplicate(let outcome, _):
            return ended(by: outcome)
        case .answeredAtKeyboard:
            return .answeredAtMac
        case .staleCard, .rejected, .failed, nil:
            break
        }
        if let outcome { return ended(by: outcome) }
        guard isBacked else { return .unavailable }
        guard answersQuestions else { return .macTooOld }
        switch hold {
        case .held: return .answerable
        case .atMac: return .askingAtMac
        case .ended, .unrecognised, nil: return .answerAtMac
        }
    }

    private static func ended(by outcome: AnswerOutcome) -> QuestionCardStatus {
        if outcome.indeterminate { return .unconfirmed }
        let byPhone = outcome.resolvedBy == .phone
        switch outcome.decision {
        case .decline(let message): return .declined(byPhone: byPhone, withReply: message != nil)
        case .deny: return .declined(byPhone: byPhone, withReply: false)
        default: break
        }
        switch outcome.resolvedBy {
        case .phone:
            if case .answers(let answers) = outcome.decision { return .answeredOnPhone(answers) }
            return .answeredOnPhone([])
        case .local: return .answeredAtMac
        case .timeout, .superseded: return .closed
        }
    }

    /// The banner a card in this status carries, or nil when it needs none.
    var banner: (title: String, message: String, tone: CodexProse.CodexTone, icon: String)? {
        switch self {
        case .answerable:
            return nil
        case .askingAtMac:
            return (
                "Asking at the Mac",
                "Claude is asking this at the Mac, so it can only be answered there.",
                .info, "desktopcomputer"
            )
        case .answerAtMac:
            return (
                "Answer at the Mac",
                "This question can no longer be answered from the phone. It may still be open "
                    + "at the Mac.",
                .warning, "desktopcomputer"
            )
        case .macTooOld:
            return (
                "Answer at the Mac",
                "This Mac's CodeConnect is too old to answer Claude's questions from the phone. "
                    + "Answer it at the Mac, or update the Mac to answer here.",
                .warning, "desktopcomputer"
            )
        case .answeredOnPhone:
            return (
                "Answered on iPhone", "Claude has your answers.", .success,
                "checkmark.circle.fill"
            )
        case .answeredAtMac:
            return (
                "Answered at the Mac",
                "This question was answered at the Mac. Nothing from this phone was used.",
                .info, "desktopcomputer"
            )
        case .declined(let byPhone, let withReply):
            let message =
                withReply
                ? "Claude got your reply instead of an answer and carried on."
                : (byPhone
                    ? "You declined this question, as Escape does at the Mac."
                    : "This question was declined at the Mac.")
            return ("Declined", message, .info, "xmark.circle")
        case .unconfirmed:
            return (
                "Unconfirmed",
                "The Mac took this answer but could not confirm Claude received it. Check the Mac.",
                .warning, "questionmark.circle"
            )
        case .closed:
            return ("Closed", "This question is no longer open.", .info, "questionmark.circle")
        case .unavailable:
            return (
                "No longer available",
                "This question can't be answered here — the run left the daemon's list or its "
                    + "log was reset.",
                .warning, "questionmark.circle"
            )
        }
    }
}
