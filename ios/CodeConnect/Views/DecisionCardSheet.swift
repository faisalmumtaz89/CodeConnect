import CodeConnectKit
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

    /// What the sheet holds. A question is not a decision to approve, and its
    /// card carries no title of its own, so this is its one heading.
    private var title: String {
        guard
            let questions = DecisionCard.questionCard(
                card: approval.card, agent: model.shownSummary(for: approval.sessionKey)?.agent)
        else { return "Decision" }
        return questions.questions.count == 1 ? "Question" : "\(questions.questions.count) questions"
    }

    var body: some View {
        CCSheetChrome(
            title,
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
struct ResolutionBanner: View {
    let attempt: AnswerAttempt

    var body: some View {
        let banner = DecisionCard.resultBanner(for: attempt)
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCBanner(banner.title, message: banner.message, tone: banner.tone.ccTone, icon: banner.icon)
            if let provenance = banner.provenance {
                Text(provenance)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
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
