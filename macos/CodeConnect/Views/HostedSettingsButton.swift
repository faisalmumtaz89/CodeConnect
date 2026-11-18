import CodeConnectKit
import SwiftUI

/// A Claude or Codex run started from the app: the model button, with the run's agent mark, and its close-on-click
/// list, built from the run's own menus (Kit's `HostedSettings`). The Model rows carry the agent's own detail lines, the
/// Effort rows are the selected model's efforts, and the Mode rows have the agent's own labels; a mode the menu marks
/// `confirm` (one that lets the agent work without asking) ends in "…" and asks first, in the reply box's confirmation.
/// The checks follow the run's facts, never a guess between a click and its answer. While one of its calls is in flight
/// the list is disabled and a small spinner stands in the chevron's place; what became of the call is the reply box's
/// note line. On a Claude run its last rows are the reply box's own: Compact… opens its compact form, and Clear… its
/// confirmation; they type Claude Code's commands, which a Codex run does not take. Closed, the button names the run's
/// mode beside the model whenever it is not the agent's asking default, a `confirm` mode in red.
struct HostedSettingsButton: View {
    let model: AppModel
    let key: String
    let agent: AgentKind
    let settings: HostedSettings
    /// The button's own tooltip: why it is disabled, or what it holds.
    let help: String
    @Binding var open: Bool
    /// A setting was chosen: the `/model` or `/effort` that opened the list has been answered.
    let chose: () -> Void
    let compact: () -> Void
    let clear: () -> Void
    /// A mode the menu marks `confirm` was chosen: nothing is sent until the reply box's confirmation is.
    let confirmMode: (MenuMode) -> Void

    private var name: String { settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay }

    /// The run's mode when it is not the agent's asking default (the menu's first mode), named beside the model in the
    /// agent's own words so that a run left in another mode says so at rest; one the menu marks `confirm` is red. It
    /// follows the run's fact, as the list's check does.
    private var standingMode: MenuMode? {
        settings.selectedMode == settings.modes.first ? nil : settings.selectedMode
    }

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                AgentMark(agent: agent)
                Text(name).foregroundStyle(Theme.text)
                if let standingMode {
                    Text("· \(standingMode.label)").foregroundStyle(standingMode.confirm ? Theme.red : Theme.text)
                }
                if settings.inFlight {
                    ProgressView().controlSize(.small).accessibilityHidden(true)
                } else {
                    Image(systemName: "chevron.down").font(.system(size: 8, weight: .semibold))
                }
            }
            .font(Theme.ui(12))
            .foregroundStyle(Theme.text2)
            .padding(.horizontal, 6)
            .frame(height: 24)
            .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: "session.composer.commands"))
        .help(help)
        .accessibilityLabel(standingMode.map { "\(name) · \($0.label)" } ?? name)
        .accessibilityIdentifier("session.composer.commands")
        .accessibilityValue(open ? "open" : "closed")
        .overlay(alignment: .bottomTrailing) {
            if open {
                list.offset(y: -30)
                    // Esc closes the list first: the window's own Esc stands down while the list is open.
                    .background {
                        Button("Close list") { open = false }
                            .keyboardShortcut(.cancelAction)
                            .accessibilityIdentifier("session.composer.commands.close")
                            .opacity(0)
                            .allowsHitTesting(false)
                    }
            }
        }
    }

    private var list: some View {
        VStack(alignment: .leading, spacing: 0) {
            header("Model")
            ForEach(settings.models, id: \.value) { entry in
                row(entry.label, detail: entry.detail, marked: entry == settings.selectedModel,
                    id: "session.composer.model.\(entry.value)") {
                    _ = await model.setModel(for: key, modelValue: entry.value)
                }
            }
            if !settings.efforts.isEmpty {
                divider
                header("Effort")
                ForEach(settings.efforts, id: \.value) { effort in
                    row(effort.label, detail: nil, marked: effort == settings.selectedEffort,
                        id: "session.composer.effort.\(effort.value)") {
                        _ = await model.setEffort(for: key, effortValue: effort.value)
                    }
                }
            }
            divider
            header("Mode")
            ForEach(settings.modes, id: \.value) { mode in
                row(mode.confirm ? "\(mode.label)…" : mode.label, detail: nil, marked: mode == settings.selectedMode,
                    id: "session.composer.mode.\(mode.value)", asksFirst: mode.confirm) {
                    if mode.confirm {
                        confirmMode(mode)
                    } else {
                        _ = await model.setMode(for: key, modeValue: mode.value)
                    }
                }
            }
            if agent == .claude {
                divider
                Button {
                    open = false
                    compact()
                } label: {
                    rowLabel("Compact…", detail: nil, marked: false)
                }
                .buttonStyle(.hover(radius: 5, id: "session.composer.compact"))
                .padding(.horizontal, 4)
                .accessibilityIdentifier("session.composer.compact")
                Button {
                    open = false
                    clear()
                } label: {
                    rowLabel("Clear…", detail: nil, marked: false)
                }
                .buttonStyle(.hover(radius: 5, id: "session.composer.clear"))
                .padding(.horizontal, 4)
                .accessibilityIdentifier("session.composer.clear")
            }
        }
        .disabled(settings.inFlight)
        .padding(.vertical, 5)
        .frame(width: 320)
        .fixedSize(horizontal: false, vertical: true)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
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

    /// One choice: the list closes and Kit sends it, one call at a time; one that asks first answers the draft that
    /// opened the list only once it is confirmed. The checked one is the run's fact already.
    private func row(
        _ title: String, detail: String?, marked: Bool, id: String, asksFirst: Bool = false,
        set: @escaping () async -> Void
    ) -> some View {
        Button {
            open = false
            if !asksFirst { chose() }
            Task { await set() }
        } label: {
            rowLabel(title, detail: detail, marked: marked)
        }
        .buttonStyle(.hover(radius: 5, id: id))
        .padding(.horizontal, 4)
        .disabled(marked)
        // One element, so a row with a detail line reports its selection as a one-line row does.
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(marked ? .isSelected : [])
    }

    private func rowLabel(_ title: String, detail: String?, marked: Bool) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "checkmark")
                .font(.system(size: 9.5, weight: .bold))
                .opacity(marked ? 1 : 0)
                .frame(width: 12)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(Theme.ui(12.5)).foregroundStyle(settings.inFlight ? Theme.text3 : Theme.text)
                    .lineLimit(1)
                if let detail, !detail.isEmpty {
                    Text(detail).font(Theme.ui(11)).foregroundStyle(Theme.text3)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(marked ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
        .contentShape(Rectangle())
    }
}

/// A Claude or Codex run started from the app that offers no menus (its link is down, it has ended or is starting, or
/// its computer's CodeConnect is older): no list, the run's agent mark and the model as the run's fact names it.
struct ModelFactLabel: View {
    let agent: AgentKind
    let fact: String?

    var body: some View {
        HStack(spacing: 5) {
            AgentMark(agent: agent)
            Text(fact.map { ModelDisplay.from($0).line } ?? "Model").foregroundStyle(Theme.text)
        }
        .font(Theme.ui(12))
        .padding(.horizontal, 6)
        .frame(height: 24)
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
