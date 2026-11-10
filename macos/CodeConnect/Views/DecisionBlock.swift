import CodeConnectKit
import SwiftUI

/// A decision where it happened: what the agent asks to do, word for word as it was hashed, how risky Kit rates it
/// and why, the edit a Codex card carries, and the answers this Mac may give, or why it can give none. Every rule is
/// Kit's (`DecisionCard`, `AppModel.answer`); this view draws them and measures that the command and its risk were on
/// screen before an answer that says yes.
struct DecisionBlock: View {
    let approval: ApprovalItem
    let model: AppModel
    @State private var gate = DecisionCard.ReadGate()
    /// The control whose answer is on its way, so a second click waits for the first.
    @State private var sending: String?
    @State private var sentAt: Date?
    /// The reason a deny with a reason sends, as it is typed.
    @State private var reason = ""
    @State private var askingReason = false
    @State private var showAllChanges = false
    @State private var showInput = false

    /// The card as the fleet holds it now; nil once nothing backs it.
    private var live: ApprovalItem? { model.shownApproval(sessionKey: approval.sessionKey, id: approval.id) }
    private var summary: SessionSummary? { model.shownSummary(for: approval.sessionKey) }
    private var assessment: RiskAssessment { approval.assessment(profile: model.daemonProfile) }
    private var risk: RiskClass { assessment.effective }
    private var attempt: AnswerAttempt? { model.lastAttempt(for: approval) }
    private var outcome: AnswerOutcome? { DecisionCard.effectiveOutcome(live: live?.outcome, snapshot: approval.outcome) }
    private var codexEnding: CodexResolution? { live?.codexResolution ?? approval.codexResolution }
    private var inFlight: Bool { sending != nil || model.isAnswering(approval) }
    private var id: String { "decision.\(approval.card.requestID)" }

    private var surface: DecisionCard.AnswerSurface {
        DecisionCard.answerSurface(
            card: approval.card, agent: summary?.agent, hold: (live ?? approval).questionHold,
            hookOnlyApprovals: model.daemonProfile.answersApprovalsByHook,
            resolvesCodexCards: model.daemonProfile.resolvesCodexCards)
    }

    private var actionable: Bool {
        DecisionCard.actionBarAvailable(outcome: outcome, codex: codexEnding, attempt: attempt, isBacked: live != nil)
    }

    /// Why no answer can go now, in Kit's order: refusals stop here.
    private var refusalBlocked: String? { model.answerBlockedReason(for: approval, inFlight: inFlight) }
    /// …and an answer that says yes also waits for the command, and at HIGH its risk, to have been on screen.
    private var assentBlocked: String? { refusalBlocked ?? gate.blockedReason(at: risk) }

    var body: some View {
        HStack(spacing: 0) {
            Rectangle().fill(actionable ? Theme.amber : Theme.text3).frame(width: 2)
            VStack(alignment: .leading, spacing: 10) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text("\(summary?.agent.displayName ?? "The agent") asks to use \(approval.card.toolName)")
                        .font(Theme.ui(12.5, .medium))
                        .foregroundStyle(actionable ? Theme.amber : Theme.text2)
                    Spacer(minLength: 8)
                    if let cwd = CodexCard.cwd(in: approval.card.toolInput) {
                        Text(FleetGroup.shellPath(cwd)).font(Theme.mono(11)).foregroundStyle(Theme.text3).lineLimit(1)
                            .truncationMode(.head)
                    }
                }
                VStack(spacing: 0) {
                    Text(approval.card.primaryText(verification: approval.card.verification, agent: summary?.agent))
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.text)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                    // The command block's bottom edge: on screen once, and the command has been.
                    Color.clear.frame(height: 1)
                        .onScrollVisibilityChange(threshold: 1) { if $0 { gate.hasSeenCommand = true } }
                }
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("\(id).command")
                if summary?.agent == .claude { fullInput }
                if let why = CodexCard.reason(in: approval.card.toolInput) {
                    Text(why).font(Theme.ui(12)).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
                }
                changes
                VStack(alignment: .leading, spacing: 0) {
                    Text("\(risk.label.capitalized) risk · \(assessment.provenance)")
                        .font(Theme.ui(11.5))
                        .foregroundStyle(risk == .high ? Theme.red : Theme.text3)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("\(id).risk")
                    Color.clear.frame(height: 1)
                        .onScrollVisibilityChange(threshold: 1) { if $0 { gate.hasSeenProvenance = true } }
                }
                if let banner { banner.accessibilityIdentifier("\(id).banner") }
                if actionable { controls } else if case .noneAnswerable = surface, banner == nil { readOnly }
                if let sentAt, inFlight, model.now.timeIntervalSince(sentAt) >= 3 {
                    Text(
                        DecisionCard.stillWaiting(
                            after: model.now.timeIntervalSince(sentAt), computer: model.machineName)
                    )
                    .font(Theme.ui(11.5)).foregroundStyle(Theme.amber)
                }
            }
            .padding(12)
        }
        .background(Theme.raised.opacity(0.45))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    // MARK: What the card says

    /// The card's one banner: a text that does not match its hash, then how a Codex question ended, then what became
    /// of an answer (this Mac's, or one recorded).
    private var banner: BannerLine? {
        if !approval.card.verification.hashMatchesDisplayText {
            return BannerLine(title: "Can't be verified", message: DecisionCard.hashMismatchSentence, tone: .danger)
        }
        if let codexEnding {
            let ending = CodexProse.resolution(codexEnding)
            return BannerLine(title: ending.title, message: ending.message, tone: ending.tone)
        }
        switch DecisionCard.resolvedBanner(attempt: attempt, persisted: outcome, isBacked: live != nil) {
        case .attempt(let attempt): return BannerLine(DecisionCard.resultBanner(for: attempt))
        case .persisted(let outcome): return BannerLine(DecisionCard.alreadyResolvedBanner(for: outcome, now: model.now))
        case .unavailable: return BannerLine(DecisionCard.unavailableBanner())
        case .none: break
        }
        return nil
    }

    /// Every field a Claude tool was called with, as the iPhone offers it under "Full tool input", and the ids the
    /// answer is checked against. Codex cards show no wire. Display-safe: JSON leaves direction marks as they are.
    @ViewBuilder private var fullInput: some View {
        Button { showInput.toggle() } label: {
            HStack(spacing: 6) {
                Image(systemName: showInput ? "chevron.down" : "chevron.right").font(.system(size: 9, weight: .semibold))
                    .accessibilityHidden(true)
                Text("Full tool input").font(Theme.ui(12))
            }
            .foregroundStyle(Theme.text2)
        }
        .buttonStyle(.hover(outset: 4, id: "\(id).input"))
        .accessibilityIdentifier("\(id).input")
        .accessibilityValue(showInput ? "open" : "closed")
        if showInput {
            VStack(alignment: .leading, spacing: 6) {
                Text(approval.card.shownToolInput)
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.text)
                    .fixedSize(horizontal: false, vertical: true)
                Text("request_id \(approval.card.requestID)\npayload_hash \(approval.card.payloadHash)")
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.text3)
            }
            .textSelection(.enabled)
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
            .accessibilityIdentifier("\(id).input.text")
        }
    }

    /// Why this Mac offers nothing to answer with, in Kit's words.
    private var readOnly: some View {
        let words: String
        switch summary?.agent {
        case nil:
            words = DecisionCard.notOnTheFleetReason
        case .codex:
            words = DecisionCard.codexUnanswerableReason(
                isKnown: model.daemonProfile.isConnected, caveat: model.daemonProfile.codexAnswerCaveat,
                isVerified: approval.card.verification.renderMatchesDisplayText,
                linkReason: model.actionsBlockedReason, computer: model.machineName)
        case .unsupported(let raw):
            words = DecisionCard.unsupportedAgentReason(raw, computer: model.machineName)
        case .claude:
            words = DecisionCard.claudeUnanswerableReason(
                isQuestion: approval.card.toolName == QuestionCard.toolName,
                hookOnlyApprovals: model.daemonProfile.capabilities?.hookOnlyApprovals,
                linkReason: model.actionsBlockedReason, hold: (live ?? approval).questionHold,
                computer: model.machineName)
        }
        return Text(words).font(Theme.ui(12)).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
            .accessibilityIdentifier("\(id).readOnly")
    }

    // MARK: A Codex card's edit

    /// The files a Codex edit would change, each as a diff, the first few open and the rest a click away; Kit
    /// bounds how many rows the card draws.
    @ViewBuilder private var changes: some View {
        let all = CodexCard.changes(in: approval.card.toolInput)
        if !all.isEmpty {
            let shown = showAllChanges ? all : Array(all.prefix(Self.foldedChanges))
            let plan = CodexCard.diffPlan(for: shown)
            VStack(alignment: .leading, spacing: 8) {
                ForEach(Array(shown.enumerated()), id: \.element.id) { index, change in
                    if index < plan.allowances.count, plan.allowances[index] > 0,
                        let file = CodexCard.parsedDiff(for: change)
                    {
                        FileDiffView(
                            file: file, path: change.path, status: Self.status(of: change.kind),
                            lineLimit: plan.allowances[index])
                    } else {
                        Text(change.path).font(Theme.mono(11.5)).foregroundStyle(Theme.text2)
                    }
                }
                if plan.heldBack > 0 {
                    Text("\(plan.heldBack) more lines not drawn here").font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                }
                if all.count > shown.count {
                    Button("Show \(all.count - shown.count) more files") { showAllChanges = true }
                        .buttonStyle(.hover(outset: 4, id: "\(id).moreChanges"))
                        .font(Theme.ui(12)).foregroundStyle(Theme.text2)
                        .accessibilityIdentifier("\(id).moreChanges")
                }
                if let omitted = CodexCard.changesOmitted(in: approval.card.toolInput) {
                    Text("\(omitted.count) more files were too many to carry on the card")
                        .font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                }
            }
        }
    }

    /// A Codex change's kind as a file status: the wire's word, not the header the parser was handed.
    private static func status(of kind: String) -> UnifiedDiff.FileDiff.Status? {
        switch kind {
        case "add": .added
        case "delete": .deleted
        case "rename": .renamed
        case "update": .modified
        default: nil
        }
    }

    /// How many files a Codex card opens before "Show more", as the iPhone folds them.
    private static let foldedChanges = 4

    // MARK: Answering

    @ViewBuilder private var controls: some View {
        switch surface {
        case .allowDeny:
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 8) {
                    Button("Deny with a reason…") { askingReason.toggle() }
                        .buttonStyle(.hover(outset: 4, id: "\(id).reason"))
                        .font(Theme.ui(12)).foregroundStyle(Theme.text2)
                        .disabled(refusalBlocked != nil)
                        .accessibilityIdentifier("\(id).reason")
                    Spacer()
                    ActionButton(title: "Deny", key: nil, prominent: false, id: "\(id).deny") { send(.deny, from: "deny") }
                        .disabled(refusalBlocked != nil)
                    ActionButton(title: "Allow", key: nil, prominent: true, id: "\(id).allow") { send(.allow, from: "allow") }
                        .disabled(assentBlocked != nil)
                        .opacity(assentBlocked != nil ? 0.45 : 1)
                }
                if askingReason {
                    HStack(spacing: 8) {
                        TextField("What should Claude do instead?", text: $reason)
                            .textFieldStyle(.plain)
                            .font(Theme.ui(12.5))
                            .padding(.horizontal, 8)
                            .frame(height: 26)
                            .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                            .accessibilityLabel("What should Claude do instead?")
                            .accessibilityIdentifier("\(id).reason.text")
                        ActionButton(title: "Deny", key: nil, prominent: false, id: "\(id).reason.send") {
                            let words = reason.trimmingCharacters(in: .whitespacesAndNewlines)
                            send(words.isEmpty ? .deny : .text(words), from: "reason")
                        }
                        .disabled(refusalBlocked != nil)
                    }
                }
                blockedLine(assentBlocked)
            }
        case .codexOptions(let options):
            VStack(alignment: .leading, spacing: 4) {
                ForEach(options, id: \.id) { option in
                    let refusal = !DecisionCard.needsSecondFactor(risk: .high, decision: .optionId(option.id))
                    let blocked = refusal ? refusalBlocked : assentBlocked
                    Button { send(.optionId(option.id), from: option.id) } label: {
                        HStack(spacing: 8) {
                            Image(systemName: refusal ? "xmark" : "checkmark").font(.system(size: 10, weight: .semibold))
                                .frame(width: 14).accessibilityHidden(true)
                            Text(option.label).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                                .fixedSize(horizontal: false, vertical: true)
                            Spacer(minLength: 0)
                        }
                        .padding(.horizontal, 8)
                        .padding(.vertical, 6)
                    }
                    .buttonStyle(.hover(id: "\(id).option.\(option.id)"))
                    .disabled(blocked != nil)
                    .opacity(blocked != nil ? 0.45 : 1)
                    .accessibilityIdentifier("\(id).option.\(option.id)")
                }
                blockedLine(assentBlocked)
            }
        case .noneAnswerable:
            readOnly
        }
    }

    @ViewBuilder private func blockedLine(_ reason: String?) -> some View {
        if let reason {
            Text(reason).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).accessibilityIdentifier("\(id).blocked")
        }
    }

    /// Sends one answer as Kit's rules have it: never on a card that cannot be answered or while one is on its way.
    private func send(_ decision: AnswerDecision, from control: String) {
        guard actionable, !inFlight else { return }
        sending = control
        sentAt = Date()
        Task {
            _ = await model.answer(item: approval, decision: decision)
            sending = nil
        }
    }
}

/// A banner on the Mac, Kit's or the card's own: the tone's symbol and colour, the title, the sentence, and where
/// it came from.
struct BannerLine: View {
    let title: String
    let message: String
    var provenance: String?
    let tone: CodexProse.CodexTone

    init(title: String, message: String, provenance: String? = nil, tone: CodexProse.CodexTone) {
        self.title = title
        self.message = message
        self.provenance = provenance
        self.tone = tone
    }

    init(_ banner: DecisionCard.Banner) {
        self.init(title: banner.title, message: banner.message, provenance: banner.provenance, tone: banner.tone)
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            Image(systemName: tone.symbol).foregroundStyle(tone.color).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(Theme.ui(12, .semibold)).foregroundStyle(Theme.text)
                Text(message).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                    .fixedSize(horizontal: false, vertical: true)
                if let provenance {
                    Text(provenance).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .accessibilityElement(children: .combine)
    }
}
