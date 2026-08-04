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
        .presentationDetents([.large])
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
            line("Your reason was typed into the session.", tone: .success, glyph: "checkmark")
        case .refused(let reason):
            line(
                "Denied, but the reason was not typed: \(reason)", tone: .warning,
                glyph: "exclamationmark.triangle.fill")
        case .failed(let reason):
            line(
                "Denied, but the reason failed to send: \(reason)", tone: .warning,
                glyph: "xmark.octagon.fill")
        case .alreadyApplied:
            line(
                "Your reason was already typed by an earlier attempt.", tone: .success,
                glyph: "checkmark")
        case .indeterminate(let reason):
            line(
                "Denied; whether the reason was typed is unknown: \(reason)", tone: .warning,
                glyph: "questionmark.circle.fill")
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
        case .duplicate, .answeredAtKeyboard: return .warning
        case .staleCard, .rejected, .failed: return .danger
        }
    }

    private var symbol: String {
        switch attempt {
        case .applied: return "checkmark.circle.fill"
        case .duplicate: return "doc.on.doc"
        case .answeredAtKeyboard: return "keyboard"
        case .staleCard: return "clock.badge.exclamationmark"
        case .rejected, .failed: return "xmark.octagon"
        }
    }

    /// The `micro` label. Short, because the component uppercases it.
    private var classification: String {
        switch attempt {
        case .applied: return "Confirmed"
        case .duplicate: return "Duplicate"
        case .answeredAtKeyboard: return "Not answered here"
        case .staleCard: return "Out of date"
        case .rejected: return "Refused"
        case .failed: return "Not delivered"
        }
    }

    /// The sentence. Sentence case, verbatim, and never uppercased — an
    /// outcome a reader has to decode from `ALLOWED — CONFIRMED BY THE DAEMON`
    /// is an outcome they skim.
    private var headline: String {
        switch attempt {
        case .applied(let outcome): return "\(outcome.decisionLabel), confirmed by the daemon"
        case .duplicate: return "Already answered"
        case .answeredAtKeyboard: return "Answered at the keyboard"
        case .staleCard: return "This card is out of date"
        case .rejected: return "The daemon refused this answer"
        case .failed: return "The answer did not reach the daemon"
        }
    }

    private var provenance: String? {
        switch attempt {
        case .applied(let outcome):
            let via = outcome.appliedVia == .sendKeys ? "typed at the TTY" : "returned to the hook"
            return [via, outcome.detail].compactMap { $0 }.joined(separator: " · ")
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
