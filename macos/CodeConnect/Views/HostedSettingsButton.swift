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
/// type Claude Code's commands; a Codex run takes neither and has no "⋯". One panel is open at a time, above its
/// button; Esc or a click anywhere else closes it. The checks follow the run's facts, never a guess between a click and
/// its answer; while one of its calls is in flight every control is disabled and a small spinner stands in the model
/// button's chevron, and what became of the call is the reply box's note line.
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

    /// Where the controls and the open panel are in the window, for the click that closes it.
    @State private var frames = PanelFrames()
    /// The model row the pointer is on.
    @State private var hovered: String?

    private var name: String { settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay }

    var body: some View {
        HStack(spacing: 2) {
            modelButton
            if !settings.modes.isEmpty { modeButton }
            if agent == .claude { moreButton }
        }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frames.controls = $0 }
        .modifier(ClosesOnClickElsewhere(open: $open, frames: frames))
        // The pointer's row is the open model panel's; a closed panel has none (its rows send no exit as they go).
        .onChange(of: open) { _, now in if now != .model { hovered = nil } }
        // The run's menus went away (it ended, or its link is down): nothing of them stays open.
        .onDisappear { open = nil }
    }

    // MARK: The controls

    private var modelButton: some View {
        control(.model, id: "session.composer.commands", label: name, help: "Model and effort") {
            HStack(spacing: 5) {
                AgentMark(agent: agent)
                Text(name).foregroundStyle(Theme.text)
                if settings.inFlight {
                    ProgressView().controlSize(.small).accessibilityHidden(true)
                } else {
                    chevron
                }
            }
        } panel: {
            modelPanel
        }
    }

    /// The run's mode as its fact names it, "Mode" when the fact names none of the listed modes.
    private var modeButton: some View {
        let label = settings.selectedMode?.label ?? "Mode"
        return control(.mode, id: "session.composer.modes", label: label, help: "Mode") {
            HStack(spacing: 5) {
                Text(label).foregroundStyle(settings.selectedMode?.confirm == true ? Theme.red : Theme.text)
                chevron
            }
        } panel: {
            card(width: 240) { modeRows }
        }
    }

    private var moreButton: some View {
        control(.more, id: "session.composer.more", label: "More", help: "Compact or clear the conversation") {
            Image(systemName: "ellipsis").font(.system(size: 11, weight: .semibold))
                .frame(width: 12)
        } panel: {
            card(width: 180) {
                commandRow("Compact…", id: "session.composer.compact", run: compact)
                commandRow("Clear…", id: "session.composer.clear", run: clear)
            }
        }
    }

    private var chevron: some View {
        Image(systemName: "chevron.down").font(.system(size: 8, weight: .semibold))
    }

    /// One control of the reply row and the panel it opens above itself, its trailing edge on the control's.
    private func control<Label: View, Panel: View>(
        _ which: SettingsPanel, id: String, label: String, help: String,
        @ViewBuilder content: () -> Label, @ViewBuilder panel: () -> Panel
    ) -> some View {
        let isOpen = open == which
        return Button {
            open = isOpen ? nil : which
        } label: {
            content()
                .font(Theme.ui(12))
                .foregroundStyle(Theme.text2)
                .padding(.horizontal, 6)
                .frame(height: 24)
                .background(isOpen ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: id))
        .help(blocked ?? help)
        .accessibilityLabel(label)
        .accessibilityIdentifier(id)
        .accessibilityValue(isOpen ? "open" : "closed")
        .overlay(alignment: .bottomTrailing) {
            if isOpen {
                panel()
                    .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frames.panel = $0 }
                    .offset(y: -30)
            }
        }
    }

    // MARK: The panels

    /// The models, one line each with the selected one's detail under it, then the selected model's efforts.
    private var modelPanel: some View {
        card(width: 420) {
            header("Model")
            ForEach(settings.models, id: \.value) { entry in
                let marked = entry == settings.selectedModel
                let id = "session.composer.model.\(entry.value)"
                row(entry.label, detail: marked || isHovered(entry, id) ? entry.detail : nil, marked: marked, id: id) {
                    _ = await model.setModel(for: key, modelValue: entry.value)
                }
                .onHover { hovered = $0 ? entry.value : (hovered == entry.value ? nil : hovered) }
            }
            if !settings.efforts.isEmpty {
                divider
                header("Effort")
                efforts
            }
        }
    }

    /// The pointer is on this model's row (or, in a render, the row is drawn as if it were: `-CC_HOVER`).
    private func isHovered(_ entry: MenuModel, _ id: String) -> Bool {
        #if DEBUG
            if RenderWindow.hovered.contains(id) { return true }
        #endif
        return hovered == entry.value
    }

    /// The selected model's efforts as one row of segments, the selected one filled.
    private var efforts: some View {
        HStack(spacing: 2) {
            ForEach(settings.efforts, id: \.value) { effort in
                let marked = effort == settings.selectedEffort
                Button {
                    choose { _ = await model.setEffort(for: key, effortValue: effort.value) }
                } label: {
                    Text(effort.label).font(Theme.ui(11.5)).lineLimit(1)
                        .foregroundStyle(marked ? Theme.text : Theme.text2)
                        .frame(maxWidth: .infinity, minHeight: 24)
                        .background(marked ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                        .contentShape(Rectangle())
                }
                .buttonStyle(.hover(radius: 5, id: "session.composer.effort.\(effort.value)"))
                .disabled(marked)
                .accessibilityIdentifier("session.composer.effort.\(effort.value)")
                .accessibilityAddTraits(marked ? .isSelected : [])
            }
        }
        .padding(3)
        .background(Theme.window.opacity(0.6), in: RoundedRectangle(cornerRadius: 7))
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Effort")
        .padding(.horizontal, 8)
        .padding(.bottom, 3)
    }

    private var modeRows: some View {
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
    }

    private func commandRow(_ title: String, id: String, run: @escaping () -> Void) -> some View {
        Button {
            open = nil
            run()
        } label: {
            rowLabel(title, detail: nil, marked: false)
        }
        .buttonStyle(.hover(radius: 5, id: id))
        .padding(.horizontal, 4)
        .accessibilityIdentifier(id)
    }

    /// A panel: the app's raised card, its own Esc closing it first (the window's Esc stands down while it is open).
    private func card<Content: View>(width: CGFloat, @ViewBuilder _ content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 0) { content() }
            .disabled(settings.inFlight)
            .padding(.vertical, 5)
            .frame(width: width)
            .fixedSize(horizontal: false, vertical: true)
            // The card takes every click inside it: one on its checked or disabled rows, a header or a gap does
            // nothing, rather than reaching the reply box or the conversation under it.
            .background {
                RoundedRectangle(cornerRadius: 8).fill(Theme.raised)
                    .contentShape(RoundedRectangle(cornerRadius: 8))
                    .onTapGesture {}
            }
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
            .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
            .background {
                Button("Close list") { open = nil }
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("session.composer.commands.close")
                    .opacity(0)
                    .allowsHitTesting(false)
            }
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

    /// One choice. The checked one is the run's fact already.
    private func row(
        _ title: String, detail: String?, marked: Bool, id: String, asksFirst: Bool = false,
        set: @escaping () async -> Void
    ) -> some View {
        Button {
            choose(asksFirst: asksFirst, set)
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

/// Where a run's settings controls and their open panel are, in the window's coordinates.
private final class PanelFrames {
    var controls: CGRect = .zero
    var panel: CGRect = .zero
}

/// While a panel is open, a click anywhere in the window but on the controls or the panel closes it; the click still
/// does what it does there.
private struct ClosesOnClickElsewhere: ViewModifier {
    @Binding var open: SettingsPanel?
    let frames: PanelFrames
    @State private var monitor: Any?

    func body(content: Content) -> some View {
        content
            .onChange(of: open != nil, initial: true) { _, isOpen in
                stop()
                guard isOpen else { return }
                monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { event in
                    if let view = event.window?.contentView {
                        let point = view.convert(event.locationInWindow, from: nil)
                        let top = CGPoint(x: point.x, y: view.isFlipped ? point.y : view.bounds.height - point.y)
                        if !frames.controls.contains(top), !frames.panel.contains(top) { open = nil }
                    }
                    return event
                }
            }
            .onDisappear(perform: stop)
    }

    private func stop() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
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
            Text(fact ?? "Model").foregroundStyle(Theme.text)
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
