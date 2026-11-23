import AppKit
import CodeConnectKit
import SwiftUI

/// Which of a run's settings panels is open: the model's (its models and efforts), the mode's, or "⋯" (Compact and
/// Clear).
enum SettingsPanel {
    case model, mode, more
}

/// A Claude or Codex run started from the app: its settings as the reply box's controls, built from the run's own menus
/// (Kit's `HostedSettings`). The model button, with the run's agent mark and the setting's name, opens the models, one
/// line each, and the selected model's efforts as one row of segments. The mode button names the run's mode at all
/// times, a mode the menu marks `confirm` (one that lets the agent work without asking) in red; its list ends such a
/// mode in "…", which asks first in the reply box's confirmation. On a Claude run "⋯" holds Compact… and Clear…, which
/// type Claude Code's commands; a Codex run takes neither and has no "⋯". One panel is open at a time, above the
/// controls, its trailing edge on the reply box's content edge (the send button's) whichever button opened it, and no
/// wider than that content; Esc or a click anywhere else closes it. The checks follow the run's facts, never a guess
/// between a click and its answer; while one of its calls is in flight every control is disabled and a small spinner
/// stands in the model button's chevron, and what became of the call is the reply box's note line.
struct HostedSettingsButton: View {
    let model: AppModel
    let key: String
    let agent: AgentKind
    let settings: HostedSettings
    /// Why the controls are disabled, for their tooltips; `nil` when they work.
    let blocked: String?
    @Binding var open: SettingsPanel?
    /// A setting was chosen: the `/model` or `/effort` that opened the list has been answered.
    let chose: () -> Void
    let compact: () -> Void
    let clear: () -> Void
    /// A mode the menu marks `confirm` was chosen: nothing is sent until the reply box's confirmation is.
    let confirmMode: (MenuMode) -> Void
    /// How far past these controls' trailing edge the reply box's content ends, where an open panel's trailing edge
    /// stands.
    let reach: CGFloat
    /// The reply box's content width, which no panel is wider than; 0 until measured.
    let room: CGFloat

    private var name: String { settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay }
    /// The effort in use beside the model, by Kit's one rule (`RunSettings.effortName`).
    private var effort: String? { model.shownSummary(for: key).flatMap(RunSettings.effortName) }

    var body: some View {
        HStack(spacing: 2) {
            modelButton
            if !settings.modes.isEmpty { modeButton }
            if agent == .claude { moreButton }
        }
        .overlay(alignment: .bottomTrailing) {
            if let open {
                panel(open)
                    .openPanel()
                    // Clear of the bar's chips by the panels' one gap.
                    .offset(x: reach, y: -(InputBoxChip.height + Theme.panelGap))
            }
        }
        .closesPanel(Binding(get: { open != nil }, set: { if !$0 { open = nil } }))
        // The run's menus went away (it ended, or its link is down): nothing of them stays open.
        .onDisappear { open = nil }
    }

    // MARK: The controls

    private var modelButton: some View {
        control(
            .model, id: "session.composer.commands", label: effort.map { "\(name), \($0) effort" } ?? name,
            help: "Model and effort"
        ) {
            HStack(spacing: 5) {
                AgentMark(agent: agent)
                // The agent's own report of the model (an id) in code type; a menu row's label in the app's.
                Text(name).font(settings.modelDisplayReported ? Theme.mono(11.5) : InputBoxChip.font)
                    .foregroundStyle(Theme.text)
                if let effort { Text("· \(effort)").foregroundStyle(Theme.text2) }
                if settings.inFlight {
                    ProgressView().controlSize(.small).accessibilityHidden(true)
                } else {
                    chevron
                }
            }
        }
    }

    /// The run's mode as its fact names it, "Permission mode" when the fact names none of the listed modes.
    private var modeButton: some View {
        let label = settings.selectedMode?.label ?? RunSettingWords.permissionMode
        return control(.mode, id: "session.composer.modes", label: label, help: RunSettingWords.permissionMode) {
            HStack(spacing: 5) {
                Text(label).foregroundStyle(settings.selectedMode?.confirm == true ? Theme.red : Theme.text)
                chevron
            }
        }
    }

    private var moreButton: some View {
        control(.more, id: "session.composer.more", label: "More", help: RunSettingWords.compactOrClear) {
            Image(systemName: "ellipsis").font(.system(size: 11, weight: .semibold))
                .frame(width: 12)
        }
    }

    private var chevron: some View {
        Image(systemName: "chevron.down").font(.system(size: 8, weight: .semibold))
    }

    /// One control of the reply row, which opens its panel.
    private func control<Label: View>(
        _ which: SettingsPanel, id: String, label: String, help: String, @ViewBuilder content: () -> Label
    ) -> some View {
        let isOpen = open == which
        return Button {
            open = isOpen ? nil : which
        } label: {
            content()
                .font(InputBoxChip.font)
                .foregroundStyle(Theme.text2)
                .inputBoxChip(open: isOpen)
        }
        .buttonStyle(.hover(radius: 8, id: id))
        .help(blocked ?? help)
        .accessibilityLabel(label)
        .accessibilityIdentifier(id)
        .accessibilityValue(isOpen ? "open" : "closed")
    }

    // MARK: The panels

    @ViewBuilder
    private func panel(_ which: SettingsPanel) -> some View {
        switch which {
        case .model: modelPanel
        case .mode: card(width: 240) { modeRows }
        case .more:
            card(width: 180) {
                PanelItem(index: 0, title: "Compact…", symbol: PanelSymbol.compact, id: "session.composer.compact") {
                    open = nil
                    compact()
                }
                PanelItem(
                    index: 1, title: "Clear…", symbol: PanelSymbol.clear, destructive: true, id: "session.composer.clear"
                ) {
                    open = nil
                    clear()
                }
            }
        }
    }

    /// The models, one line each, then the selected model's efforts.
    private var modelPanel: some View {
        card(width: 420) {
            header("Model")
            ForEach(Array(settings.models.enumerated()), id: \.element.value) { index, entry in
                let marked = entry == settings.selectedModel
                let id = "session.composer.model.\(entry.value)"
                row(index, entry.label, detail: entry.detail, marked: marked, id: id) {
                    _ = await model.setModel(for: key, modelValue: entry.value)
                }
            }
            if !settings.efforts.isEmpty {
                divider
                header(settings.selectedEffort == nil || settings.effortIsDefault ? "Effort · Default" : "Effort")
                efforts
            }
        }
    }

    /// The selected model's efforts as one row of segments, the selected one filled; to the keys, the rows after the
    /// models, left to right.
    private var efforts: some View {
        SegmentedTrack(
            options: settings.efforts, chosen: settings.selectedEffort, fill: Theme.window.opacity(0.6),
            firstPanelRow: settings.models.count,
            id: { "session.composer.effort.\($0.value)" },
            offers: settings.offers,
            choose: { effort in choose { _ = await model.setEffort(for: key, effortValue: effort.value) } }
        ) { effort, chosen in
            Text(effort.label).font(Theme.ui(11.5)).lineLimit(1)
                .foregroundStyle(chosen ? Theme.text : Theme.text2)
                .frame(maxWidth: .infinity, minHeight: 24)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Effort")
        .padding(.horizontal, 8)
        .padding(.bottom, 3)
    }

    private var modeRows: some View {
        ForEach(Array(settings.modes.enumerated()), id: \.element.value) { index, mode in
            row(index, mode.confirm ? "\(mode.label)…" : mode.label, marked: mode == settings.selectedMode,
                id: "session.composer.mode.\(mode.value)", asksFirst: mode.confirm) {
                if mode.confirm {
                    confirmMode(mode)
                } else {
                    _ = await model.setMode(for: key, modeValue: mode.value)
                }
            }
        }
    }

    /// A panel: the app's raised card.
    private func card<Content: View>(width: CGFloat, @ViewBuilder _ content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 0) { content() }
            .disabled(settings.inFlight)
            .frame(width: (room > 0 ? min(width, room) : width) - 12)
            .fixedSize(horizontal: false, vertical: true)
            .menuCard()
            // The card takes every click inside it: one on its checked or disabled rows, a header or a gap does
            // nothing, rather than reaching the reply box or the conversation under it.
            .contentShape(RoundedRectangle(cornerRadius: 14))
            .onTapGesture {}
    }

    private func header(_ text: String) -> some View {
        Text(text).font(Theme.ui(11, .semibold)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.top, 5)
            .padding(.bottom, 3)
    }

    private var divider: some View {
        Rectangle().fill(Theme.hairline).frame(height: 1).padding(.vertical, 5)
    }

    /// The panel closes and Kit sends the choice, one call at a time; one that asks first answers the draft that
    /// opened the list only once it is confirmed.
    private func choose(asksFirst: Bool = false, _ set: @escaping () async -> Void) {
        open = nil
        if !asksFirst { chose() }
        Task { await set() }
    }

    /// Choice `index` of its panel. The checked one is the run's fact already, never chosen or lit.
    private func row(
        _ index: Int, _ title: String, detail: String? = nil, marked: Bool, id: String, asksFirst: Bool = false,
        set: @escaping () async -> Void
    ) -> some View {
        Button {
            choose(asksFirst: asksFirst, set)
        } label: {
            rowLabel(title, detail: detail, marked: marked)
        }
        .buttonStyle(.panelRow)
        .panelRow(index, id: id) { choose(asksFirst: asksFirst, set) }
        .padding(.horizontal, 4)
        .disabled(marked)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(marked ? .isSelected : [])
    }

    /// A row's label, and beside it, quieter, the agent's own description as sent, cut at its end.
    private func rowLabel(_ title: String, detail: String? = nil, marked: Bool) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "checkmark")
                .font(.system(size: 9.5, weight: .bold))
                .opacity(marked ? 1 : 0)
                .frame(width: 12)
                .accessibilityHidden(true)
            Text(title).font(Theme.ui(12.5)).foregroundStyle(settings.inFlight ? Theme.text3 : Theme.text)
                .lineLimit(1)
                .layoutPriority(1)
            if let detail {
                ModelDetail(text: detail)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .contentShape(Rectangle())
    }
}

/// A Claude or Codex run started from the app that offers no menus (its link is down, it has ended or is starting, or
/// its computer's CodeConnect is older): no list, the run's agent mark and its model, by its menu's name when the
/// daemon gives one, else as the run's fact names it.
struct ModelFactLabel: View {
    let agent: AgentKind
    let fact: RunModelName?
    /// The effort the agent named, if it named one (`RunSettings.effortName`).
    let effort: String?

    var body: some View {
        HStack(spacing: 5) {
            AgentMark(agent: agent)
            // By its menu's name, or as the run reported it in code type; "Model" while it has reported none.
            Text(fact?.text ?? "Model").font(fact?.reported == true ? Theme.mono(11.5) : InputBoxChip.font)
                .foregroundStyle(Theme.text)
            if let effort { Text("· \(effort)").foregroundStyle(Theme.text2) }
        }
        .font(InputBoxChip.font)
        .inputBoxChip(open: false)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.composer.model")
    }
}

extension SettingOutcome.Tone {
    /// A setting's answer on the reply box's note line, drawn as its other notes are: a change from the next message
    /// with the info mark.
    var composeTone: ComposeProse.Tone {
        switch self {
        case .success: .success
        case .neutral: .info
        case .warning: .warning
        }
    }
}

/// A model's description beside its label in a model menu (the reply box's and Start's): the agent's own words as
/// sent, quieter, on the label's line, cut at the end.
struct ModelDetail: View {
    let text: String

    var body: some View {
        Text(text).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.tail)
    }
}
