import CodeConnectKit
import SwiftUI

/// **This session: its effort, permission and model** (B8, minor 39), opened by the reply box's model chip.
///
/// One full-dark sheet, as the Decision sheet: the title with the current choice under it, then Effort (for the
/// model in use, one segmented row), Permission (the agent's own names, a mode that lets the agent work without
/// asking in red, confirmed first), and Model: this session's agent's models, then every other agent's on this
/// computer, each group under its agent's mark, its heading pinned while the list scrolls. Choosing another agent's
/// model starts a new session: the Start sheet opens on this computer and folder with that agent and model, ready to
/// send (`onStartNew`). One outcome line — reached from `/model` and `/effort` too, because a run
/// CodeConnect started offers all three as structured menus the daemon validates, not as typed
/// commands. `SessionDetailView` sends every run's `/model` and `/effort` here.
///
/// Everything drawn comes from `AppModel.runSettings(for:)` — the one place both apps read — so the
/// selection, the labels and the in-flight state are the Kit's, never re-derived here. The view
/// switches on its three cases with **no `else`**: a run that is not offering menus (detached, ended,
/// an older daemon) shows its model read-only and no chooser, rather than a static list it cannot act
/// on.
struct HostedSettingsSheet: View {
    let sessionKey: String
    /// The `/model` or `/effort` draft that opened this sheet is consumed when a change is applied.
    var onLanded: () -> Void = {}
    /// Start a new session with another agent's model, from the Start sheet seeded with it.
    var onStartNew: (StartSeed) -> Void = { _ in }

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    /// A mode the menu marks `confirm`, tapped and awaiting the person's confirmation before it is sent.
    @State private var confirmingMode: MenuMode?

    private var settings: RunSettings { model.runSettings(for: sessionKey) }

    /// The run's agent — the confirmation words are the agent's. Claude for a hosted run, which is all
    /// this sheet is reached for.
    private var agent: AgentKind { model.shownSummary(for: sessionKey)?.agent ?? .claude }

    /// The current choice, under the title: the model, its effort, the mode.
    private var current: String? {
        guard case .hostedLive(let live) = settings else { return nil }
        return [live.modelDisplay.isEmpty ? nil : live.modelDisplay, live.selectedEffort?.label, live.selectedMode?.label]
            .compactMap { $0 }.joined(separator: " · ")
    }

    var body: some View {
        CCSheetChrome("This session", subtitle: current, onClose: { dismiss() }) {
            VStack(spacing: 0) {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: CC.space.md, pinnedViews: [.sectionHeaders]) {
                        switch settings {
                        case .hostedLive(let live):
                            liveSections(live)
                        case .hostedReadOnly(let modelFact):
                            readOnly(modelFact)
                        case .deferToOtherPath:
                            // An agent these menus do not speak for: it says so.
                            // Handled, not defaulted, so the picker is never drawn for such a run.
                            unavailable
                        }
                    }
                    .padding(CC.space.md)
                }
                .scrollBounceBehavior(.basedOnSize)
                outcomeBar
            }
            .ccConfirmation($confirmingMode) { mode in
                let words = ModeConfirmation.words(for: agent)
                return CCConfirmation(
                    title: words.title, message: words.message,
                    answers: [
                        .init(title: words.cancel, kind: .cancel, id: "settings.mode.cancel"),
                        .init(title: words.confirm, kind: .destructive, id: "settings.mode.confirm") {
                            run { await model.setMode(for: sessionKey, modeValue: mode.value, confirmed: true) }
                        },
                    ])
            }
        }
    }

    // MARK: Live

    @ViewBuilder
    private func liveSections(_ live: HostedSettings) -> some View {
        // The effort menu is the selected model's; with nothing selected there is none to draw.
        if !live.efforts.isEmpty {
            CCSectionHeader("Effort · for \(live.modelDisplay.isEmpty ? "this model" : live.modelDisplay)")
            effortRow(live)
        }
        CCSectionHeader("Permission")
        card {
            ForEach(Array(live.modes.enumerated()), id: \.element.value) { index, entry in
                choiceRow(
                    title: entry.confirm ? "\(entry.label)…" : entry.label, warn: entry.confirm,
                    isSelected: live.selectedMode?.value == entry.value,
                    isLast: index == live.modes.count - 1, disabled: live.inFlight
                ) {
                    // A mode the daemon marks `confirm` asks first; nothing is sent until the alert's
                    // confirm button, which sends it with `confirmed`.
                    if entry.confirm {
                        confirmingMode = entry
                        return nil
                    }
                    return await model.setMode(for: sessionKey, modeValue: entry.value)
                }
            }
        }
        CCSectionHeader("Model")
        Section {
            card {
                ForEach(Array(live.models.enumerated()), id: \.element.value) { index, entry in
                    choiceRow(
                        title: entry.label, detail: entry.detail,
                        isSelected: live.selectedModel?.value == entry.value,
                        isLast: index == live.models.count - 1, disabled: live.inFlight
                    ) { await model.setModel(for: sessionKey, modelValue: entry.value) }
                }
            }
        } header: {
            agentHeading(agent, note: "This session")
        }
        ForEach(otherAgents) { other in
            Section {
                otherModels(other)
            } header: {
                agentHeading(Self.kind(other), note: "Starts a new session")
            }
        }
    }

    /// The effort as one segmented row, 44 pt tall; the model's own default, while no effort is chosen, is the
    /// selected segment and says so to VoiceOver (decision 318).
    private func effortRow(_ live: HostedSettings) -> some View {
        HStack(spacing: 0) {
            ForEach(live.efforts, id: \.value) { entry in
                let isSelected = live.selectedEffort?.value == entry.value
                Button {
                    guard live.offers(entry) || !isSelected else { return }
                    run { await model.setEffort(for: sessionKey, effortValue: entry.value) }
                } label: {
                    Text(entry.label)
                        .ccType(CC.type.footnote).fontWeight(.medium)
                        .lineLimit(1).minimumScaleFactor(0.8)
                        .foregroundStyle(isSelected ? CC.text.primary : CC.text.tertiary)
                        .frame(maxWidth: .infinity, minHeight: CC.size.hitTarget)
                        .background(
                            isSelected ? Color.white.opacity(0.1) : .clear,
                            in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(live.inFlight)
                .accessibilityAddTraits(isSelected ? [.isSelected] : [])
                .accessibilityValue(isSelected && live.effortIsDefault ? "Default" : "")
            }
        }
        .padding(2)
        .background(CC.color.surface, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.white.opacity(0.08)))
    }

    /// An agent's group heading, pinned while its models scroll under it: its mark, its name, and what choosing does.
    private func agentHeading(_ agent: AgentKind, note: String) -> some View {
        HStack(spacing: 7) {
            CCAgentMark(agent: agent)
            Text(agent.productName.uppercased()).ccType(CC.type.micro).foregroundStyle(CC.text.secondary)
            Spacer(minLength: CC.space.sm)
            Text(note).ccType(CC.type.footnote).foregroundStyle(CC.text.tertiary)
        }
        .padding(.top, CC.space.sm)
        .padding(.bottom, CC.space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(CC.color.bg)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }

    // MARK: Other agents

    /// This computer, as a start sees it.
    private var startHere: StartContext { StartContext(model: model) }

    /// Every agent a start here offers but this session's.
    private var otherAgents: [StartAgent] { startHere.startAgents.filter { Self.kind($0) != agent } }

    /// Another agent's models on this computer; choosing one opens the Start sheet with it, on this session's folder.
    @ViewBuilder
    private func otherModels(_ other: StartAgent) -> some View {
        switch startHere.models(other) {
        case .checking:
            card { note("Checking \(other.displayName) on \(startHere.machineName)…") }
        case .missing(let reason), .signedOut(let reason):
            card { note(reason) }
        case .choices(let choices):
            card {
                ForEach(Array(choices.enumerated()), id: \.offset) { index, choice in
                    choiceRow(
                        title: choice.label, detail: choice.detail, isSelected: false,
                        isLast: index == choices.count - 1, disabled: folder == nil
                    ) {
                        guard let folder else { return nil }
                        dismiss()
                        onStartNew(StartSeed(
                            context: startHere, project: startHere.project(at: folder), place: .folder,
                            agent: other, model: choice.model, trustPath: nil))
                        return nil
                    }
                }
            }
        }
    }

    /// The folder this session runs in, which a new session starts in too.
    private var folder: String? { model.shownSummary(for: sessionKey)?.cwd }

    private static func kind(_ agent: StartAgent) -> AgentKind {
        switch agent {
        case .claude: return .claude
        case .codex: return .codex
        case .antigravity: return .antigravity
        }
    }

    private func note(_ text: String) -> some View {
        Text(text).ccType(CC.type.footnote).foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, minHeight: 48, alignment: .leading)
            .padding(.horizontal, 14)
    }

    private func card<Content: View>(@ViewBuilder _ content: @escaping () -> Content) -> some View {
        CCCard(padding: 0) { VStack(spacing: 0) { content() } }
    }

    /// One selectable row: its label (in the alert's red for a mode that lets the agent work without asking), the
    /// agent's description beneath, a check at its end when it is the current value, and its tap — disabled while any
    /// call is in flight (the menus are one unit), and doing nothing on the row already in force (there is nothing to
    /// apply) unless the row says it is still on offer (`offered`). That row stays enabled: iOS fades a disabled one.
    private func choiceRow(
        title: String, detail: String? = nil, warn: Bool = false, isSelected: Bool, offered: Bool? = nil, isLast: Bool,
        disabled: Bool, apply: @escaping () async -> SettingResult?
    ) -> some View {
        Button {
            guard offered ?? !isSelected else { return }
            run(apply)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(title).ccType(CC.type.callout)
                        .foregroundStyle(warn ? CC.color.danger : CC.text.primary)
                        .lineLimit(1)
                    if let detail {
                        Text(detail).ccType(CC.type.footnote).foregroundStyle(CC.text.tertiary).lineLimit(1)
                    }
                }
                Spacer(minLength: 0)
                if isSelected {
                    Image(systemName: "checkmark").font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(CC.text.primary)
                        .accessibilityHidden(true)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .frame(minHeight: 48)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(disabled)
        .overlay(alignment: .bottom) {
            if !isLast { Rectangle().fill(Color.white.opacity(0.06)).frame(height: 1).padding(.leading, 14) }
        }
        // The check is drawn with an icon, which VoiceOver hides, so the selection is announced as a trait instead.
        .accessibilityAddTraits(isSelected ? [.isSelected] : [])
    }

    private func run(_ apply: @escaping () async -> SettingResult?) {
        Task {
            // The /model or /effort draft that opened the sheet is consumed only when a change is
            // actually applied; a refusal, an indeterminate answer or a lost link leaves it to retry.
            if case .applied? = await apply() { onLanded() }
        }
    }

    // MARK: Read-only

    @ViewBuilder
    private func readOnly(_ modelFact: RunModelName?) -> some View {
        CCSectionHeader("Model")
        card {
            if let modelFact, !modelFact.text.isEmpty {
                // The model by its menu's name, or as the run reported it: an id, in code type.
                CCRow(modelFact.text, titleIsMono: modelFact.reported, showsChevron: false, separator: false) {
                    CCIcon("check-circle-fill", size: 18, relativeTo: .body)
                        .foregroundStyle(CC.text.primary)
                } trailing: { EmptyView() } meta: { EmptyView() }
            } else {
                CCRow(
                    "Not reported", meta: "This session has not reported a model.",
                    showsChevron: false, separator: false
                ) {
                    CCIcon("circle-dashed", size: 16, weight: .regular, relativeTo: .body)
                        .foregroundStyle(CC.text.tertiary)
                } trailing: { EmptyView() }
            }
        }
        Text("This session isn’t offering model controls right now. Its model is shown as last reported.")
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }

    @ViewBuilder
    private var unavailable: some View {
        CCCard {
            Text("This session’s settings aren’t available here.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
        }
    }

    // MARK: Outcome

    /// **Pinned, not scrolled** — the one line the reader has to see, below the sections. While a
    /// call is in flight it says so once (the menus above are disabled); then the Kit's own wording for
    /// what the daemon answered, for as long as the Kit keeps it (`HostedSettings.lastOutcome`).
    @ViewBuilder
    private var outcomeBar: some View {
        if case .hostedLive(let live) = settings {
            if live.inFlight {
                CCActionBar { statusLine("Applying…", tone: .neutral) }
            } else if let outcome = live.lastOutcome {
                CCActionBar { statusLine(outcome.text, tone: Self.ccTone(outcome.tone)) }
            }
        }
    }

    private func statusLine(_ text: String, tone: CCTone) -> some View {
        CCProse(
            text, style: CC.type.footnote,
            color: tone == .neutral ? CC.text.secondary : tone.color
        )
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private static func ccTone(_ tone: SettingOutcome.Tone) -> CCTone {
        switch tone {
        case .success: return .success
        case .neutral: return .neutral
        case .warning: return .warning
        }
    }
}
