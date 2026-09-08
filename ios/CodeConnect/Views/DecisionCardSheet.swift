import SwiftUI

/// The per-session home of `DecisionCardView`: a sheet with a label and a way
/// out. The card itself — and every risk gate on it — lives in `DecisionCard`,
/// so this sheet and the Deck can never drift apart on what it takes to approve
/// something.
///
/// **`.large` detent only.** A `.medium` detent would let you approve a
/// command you cannot fully see, which is the exact failure MEDIUM's scroll gate
/// exists to prevent. Swipe-to-dismiss is allowed — dismissing is not deciding;
/// swipe-to-*answer* does not exist anywhere in this app.
struct DecisionCardSheet: View {
    /// The card carries its own run, so the sheet does not need to be told
    /// which session it belongs to — and cannot be told a different one.
    let approval: ApprovalItem

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    private var attempt: AnswerAttempt? { model.lastAttempt(for: approval) }

    var body: some View {
        CCSheetChrome(
            "Decision",
            onClose: { dismiss() },
            closeLabel: attempt?.isTerminal == true ? "Done" : "Close"
        ) {
            DecisionCardView(approval: approval)
        }
        .ccTallSheet()
    }
}

// MARK: - Resolution

/// What happened to an answer, in terms the app can defend.
///
/// One `CCBanner`, never a stack: the card has exactly one banner slot and this
/// is what fills it once an answer has been sent.
///
/// Three tiers, and the split is not cosmetic. The banner's **title** is the
/// classification (`micro`, uppercased by the component). Its **message** is the
/// sentence a human reads and a test looks for by name — so it is never
/// uppercased and never abbreviated. The **provenance** underneath carries the
/// original outcome, the transport and the daemon's own reason, because a
/// duplicate that does not show you what the first answer was is just a refusal.
/// The compose result rides last: "denied" and "your sentence reached the
/// session" succeed and fail independently, and collapsing them would let one
/// claim the other's success.
struct ResolutionBanner: View {
    let attempt: AnswerAttempt
    let compose: ComposeAttempt?

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCBanner(classification, message: headline, tone: tone, icon: symbol)
            if let provenance {
                Text(provenance)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if let compose {
                composeLine(compose)
            }
        }
    }

    @ViewBuilder
    private func composeLine(_ compose: ComposeAttempt) -> some View {
        switch compose {
        case .sent:
            line("Reason typed into the session.", tone: .success, glyph: "checkmark")
        case .refused(let reason):
            line(
                "Reason not typed: \(reason)", tone: .warning,
                glyph: "exclamationmark.triangle.fill")
        case .failed(let reason):
            line(
                "Reason could not be sent: \(reason)", tone: .warning,
                glyph: "xmark.octagon.fill")
        case .alreadyApplied:
            line(
                "Reason was already typed — not repeated.", tone: .success,
                glyph: "checkmark")
        case .indeterminate(let reason):
            line(
                "Couldn’t confirm whether the reason was typed: \(reason)", tone: .warning,
                glyph: "questionmark.circle.fill")
        case .composerRecovered:
            // Unreachable from a denial reason (never a slash command), but
            // the compiler is right to ask and silence would be a lie.
            line("Reason typed into the session.", tone: .success, glyph: "checkmark")
        case .composerLost:
            line(
                "The Mac's composer did not come back. Open Terminal to recover.",
                tone: .warning, glyph: "exclamationmark.triangle.fill")
        }
    }

    private func line(_ text: String, tone: CCTone, glyph: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
            CCIcon(glyph, size: 11, weight: .semibold, relativeTo: .caption)
                .foregroundStyle(tone.color)
            Text(text)
                .ccType(CC.type.footnote)
                .foregroundStyle(tone.color)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var tone: CCTone {
        switch attempt {
        case .applied: return .success
        case .indeterminate, .duplicate, .answeredAtKeyboard: return .warning
        case .staleCard, .rejected, .failed: return .danger
        }
    }

    private var symbol: String {
        switch attempt {
        case .applied: return "checkmark.circle.fill"
        case .indeterminate: return "questionmark.circle.fill"
        case .duplicate: return "doc.on.doc"
        case .answeredAtKeyboard: return "keyboard"
        case .staleCard: return "clock.badge.exclamationmark"
        case .rejected, .failed: return "xmark.octagon"
        }
    }

    private var classification: String { Self.classificationLabel(for: attempt) }
    private var headline: String { Self.headline(for: attempt) }
    private var provenance: String? { Self.provenance(for: attempt) }

    /// The `micro` label. Short, because the component uppercases it. Static
    /// and pure so the copy can be asserted without standing up a `View`.
    static func classificationLabel(for attempt: AnswerAttempt) -> String {
        switch attempt {
        case .applied: return "Confirmed"
        // Never "Confirmed": the daemon did not confirm this one landed.
        case .indeterminate: return "Unconfirmed"
        case .duplicate: return "Duplicate"
        case .answeredAtKeyboard: return "Not answered here"
        case .staleCard: return "Out of date"
        case .rejected: return "Refused"
        case .failed: return "Not delivered"
        }
    }

    /// The sentence. Sentence case, verbatim, and never uppercased — an
    /// outcome a reader has to decode from `ALLOWED — CONFIRMED BY THE DAEMON`
    /// is an outcome they skim. Static and pure for the same reason as above.
    static func headline(for attempt: AnswerAttempt) -> String {
        switch attempt {
        case .applied(let outcome): return "\(outcome.decisionLabel), confirmed by the daemon"
        // The one honesty this case exists to protect: the daemon accepted the
        // answer but could not confirm it reached the agent, so this must never
        // read as confirmed/applied.
        case .indeterminate(let outcome):
            return "\(outcome.decisionLabel), but the daemon couldn’t confirm it landed"
        case .duplicate: return "Already answered"
        case .answeredAtKeyboard: return "Answered at the keyboard"
        case .staleCard: return "This card is out of date"
        case .rejected: return "The daemon refused this answer"
        case .failed: return "The answer did not reach the daemon"
        }
    }

    /// How an answer was *applied*, in the daemon's terms — never a claim this
    /// build cannot support. An `AnswerPath.unknown` is a path this build has
    /// never heard of, so it is described as unrecognised rather than asserted
    /// to be keystrokes or a hook return we cannot vouch for.
    static func actuationPhrase(for path: AnswerPath) -> String {
        switch path {
        case .sendKeys: return "typed at the TTY"
        case .hookReturn: return "returned to the hook"
        // A real actuation, and it reads like one. Codex asked, over its own
        // control link, and this is the answer written back to that request —
        // no keyboard, no pane, nothing inferred from a prompt disappearing.
        case .codexResponse: return "answered on the Codex link"
        case .unknown: return "applied in a way this app doesn’t recognise"
        }
    }

    static func provenance(for attempt: AnswerAttempt) -> String? {
        switch attempt {
        case .applied(let outcome):
            return [actuationPhrase(for: outcome.appliedVia), outcome.detail]
                .compactMap { $0 }.joined(separator: " · ")
        // Never the `actuationPhrase` — that would claim the answer was "typed at
        // the TTY" or "returned to the hook", the exact positive actuation the
        // daemon could not confirm. The provenance states the uncertainty instead.
        case .indeterminate(let outcome):
            return [
                "The daemon accepted this but never confirmed it reached the agent.",
                outcome.detail,
            ].compactMap { $0 }.joined(separator: " · ")
        case .duplicate(let outcome, let stale):
            let original =
                "Original outcome: \(outcome.decisionLabel) \(outcome.resolvedBy == .phone ? "from a phone" : "at the Mac"), \(outcome.resolvedAt)."
                + (outcome.inferred
                    ? " The daemon inferred that from the prompt disappearing rather than observing the answer."
                    : "")
            return stale
                ? original + " The card you answered was also out of date." : original
        case .answeredAtKeyboard(let reason):
            return
                "The prompt was gone before the keystrokes could land, so somebody answered it on the Mac. Nothing was typed. (\(reason))"
        case .staleCard(let reason): return reason
        case .rejected(let reason): return reason
        case .failed(let reason): return "\(reason) Answers are idempotent. Retrying is safe."
        }
    }
}

/// The bridge between `CodexProse`'s view-free tone and the kit's.
///
/// `CodexProse` is deliberately free of SwiftUI so its sentences can be
/// asserted without a view layer; this is the one line that costs.
extension CodexProse.CodexTone {
    var ccTone: CCTone {
        switch self {
        case .info: return .info
        case .success: return .success
        case .warning: return .warning
        case .danger: return .danger
        }
    }
}
