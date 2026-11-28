import CodeConnectKit
import SwiftUI

/// **The reply box's one bar**, 48 pt under the field: `+` (where the computer takes files), the model chip, space, the
/// run's usage figures, and the circle; every control a 44 pt target.
///
/// The chip is the agent's mark, the model's name and ▾, and opens the run's settings sheet (model, effort,
/// permission); with a mode that lets the agent work without asking (`MenuMode.confirm`) the chip is in the alert's
/// red. A run with no menus shows its mark and model as a fact; a run these controls do not own shows its mark alone.
/// The chip is dead while a card waits on the run or it is starting (`isHeld`), as the sheet's calls are refused then.
///
/// **One line, never a second**: "61k / 1M" and its hairline give way first, then the model's name is cut at its end,
/// then the share; `+`, the mark, the ring and the circle never go. At accessibility sizes the chip is the mark and ▾,
/// the figures the ring alone. The figures open the run's usage.
struct ReplyBoxBar<Leading: View, Trailing: View>: View {
    let model: AppModel
    let key: String
    let agent: AgentKind?
    let onSettings: () -> Void
    let onUsage: () -> Void
    @ViewBuilder let leading: Leading
    @ViewBuilder let trailing: Trailing

    @Environment(\.dynamicTypeSize) private var typeSize

    /// How much of the model's name the chip draws.
    private enum Name {
        /// All of it.
        case whole
        /// As much as the line leaves, cut at its end.
        case cut
        /// None: the mark and ▾ alone.
        case none
    }

    var body: some View {
        Group {
            if typeSize.isAccessibilitySize {
                row(.none, figures: .ring)
            } else if usage != nil {
                // The first that fits: everything; the share without the size; the name cut; the ring alone.
                ViewThatFits(in: .horizontal) {
                    row(.whole, figures: .full)
                    row(.whole, figures: .ringAndShare)
                    row(.cut, figures: .ringAndShare)
                    row(.cut, figures: .ring)
                }
            } else {
                ViewThatFits(in: .horizontal) {
                    row(.whole, figures: nil)
                    row(.cut, figures: nil)
                }
            }
        }
        .frame(height: 48)
        .padding(.horizontal, 2)
    }

    private func row(_ name: Name, figures: UsageFigures.Form?) -> some View {
        HStack(spacing: 0) {
            leading
            chip(name)
            if name != .cut { Spacer(minLength: 0) }
            if let figures, let bar = usage {
                Button(action: onUsage) {
                    UsageFigures(bar: bar, form: figures, atLimit: model.shownSummary(for: key)?.atLimit == true)
                        .padding(.horizontal, 6)
                        .frame(minHeight: CC.size.hitTarget)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .fixedSize()
                .accessibilityIdentifier("session.composer.usage")
                .accessibilityLabel("Usage")
                .accessibilityValue(bar.spoken)
            }
            trailing
        }
    }

    // MARK: The chip

    /// The effort in use, by Kit's one rule.
    private var effort: String? { model.shownSummary(for: key).flatMap(RunSettings.effortName) }

    @ViewBuilder
    private func chip(_ name: Name) -> some View {
        switch model.runSettings(for: key) {
        case .deferToOtherPath:
            mark(warn: false).frame(width: CC.size.hitTarget, height: CC.size.hitTarget)
        case .hostedReadOnly(let fact):
            chipLabel(name, model: fact.flatMap { $0.text.isEmpty ? nil : $0.text }, mono: fact?.reported == true,
                      opens: false, warn: false, nameID: "session.composer.model")
                .foregroundStyle(CC.text.secondary)
        case .hostedLive(let settings):
            let modelName = settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay
            let warn = settings.standingMode?.confirm == true
            Button(action: onSettings) {
                chipLabel(name, model: modelName, mono: settings.modelDisplayReported && !settings.modelDisplay.isEmpty,
                          opens: true, warn: warn)
            }
            .buttonStyle(.plain)
            .foregroundStyle(warn ? CC.color.danger : CC.text.secondary)
            .disabled(model.settingsHeld(key))
            .accessibilityIdentifier("session.composer.settings")
            .accessibilityLabel(
                [modelName, effort.map { "\($0) effort" }, settings.standingMode?.label]
                    .compactMap { $0 }.joined(separator: ", "))
        }
    }

    /// The mark, the name as `name` allows, and ▾ where the chip opens the sheet.
    private func chipLabel(
        _ name: Name, model modelName: String?, mono: Bool, opens: Bool, warn: Bool, nameID: String = ""
    ) -> some View {
        HStack(spacing: 6) {
            mark(warn: warn)
            if name != .none, let modelName {
                let text = Text(modelName)
                    .ccType(mono ? CC.type.footnote.monospaced() : CC.type.footnote)
                    .fontWeight(.medium)
                    .foregroundStyle(warn ? CC.color.danger : CC.text.primary)
                    .lineLimit(1)
                    .accessibilityIdentifier(nameID)
                if name == .whole {
                    text.fixedSize()
                } else {
                    // A small ideal width, so the line with the name cut is chosen only once the whole name cannot fit;
                    // laid out, it takes what the line leaves.
                    text.truncationMode(.tail)
                        .frame(minWidth: 24, idealWidth: 24, maxWidth: .infinity, alignment: .leading)
                }
            }
            if opens {
                Image(systemName: "chevron.down").font(.system(size: 9, weight: .semibold))
                    .accessibilityHidden(true)
            }
        }
        .padding(.horizontal, 8)
        .frame(minHeight: CC.size.hitTarget)
        .contentShape(Rectangle())
    }

    private func mark(warn: Bool) -> some View {
        Group {
            if let agent {
                CCAgentMark(agent: agent, warn: warn)
            }
        }
    }

    // MARK: Usage

    /// The run's usage (`ReplyBar`), when the app holds its context or a window of its account.
    private var usage: ReplyBar? {
        guard let bar = model.replyBar(for: key, now: Date()), bar.context != nil || bar.ring != nil
        else { return nil }
        return bar
    }
}

extension AppModel {
    /// A session's settings are held while a card waits or it is starting, as the sheet's calls are refused then; the
    /// empty draft keeps a long Codex draft's own ceiling — a send concern, not a settings one — out of this check.
    /// Files on their way, or one that failed, hold the message, not the session: its settings stay open, as on the
    /// Mac.
    func settingsHeld(_ key: String) -> Bool {
        sendBlockedReason(for: key, draft: "", files: false) != nil
    }
}
