import SwiftUI

// =============================================================================
//  Slash commands, from a phone.
//
//  Typing `/` in the composer summons a palette of what the Mac's Claude Code
//  actually has — read from the binary itself over `get_command_catalog`,
//  never from a hand-kept list. Exactly one command gets native UX in this
//  release: `/model`, whose picker form measured as a lockout (the dialog
//  replaces the Mac's composer and every phone send is refused until someone
//  presses Esc there). Everything else is labeled honestly and handed to the
//  Terminal tab rather than injected — a discovered-but-unclassified built-in
//  might be a dialog under a new name, and "send and warn" is not a
//  safeguard when the failure is a measured lockout.
// =============================================================================

/// The palette above the keyboard while the composer starts with `/`.
struct CommandPalette: View {
    let typed: String
    let catalog: [String]?
    let failure: String?
    let onModel: () -> Void
    let onBlocked: (String) -> Void

    /// The prefix being completed: `/mo` → `mo`.
    private var fragment: String {
        let trimmed = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("/") else { return "" }
        return String(trimmed.dropFirst().prefix { !$0.isWhitespace }).lowercased()
    }

    private var matches: [String] {
        guard let catalog else { return [] }
        let names = catalog.filter { fragment.isEmpty || $0.hasPrefix(fragment) }
        // `/model` first — it is the one row that opens app UX — then the
        // binary's own order, which is Claude Code's to choose.
        return names.sorted { a, b in
            if a == "model" { return true }
            if b == "model" { return false }
            return false
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if catalog == nil, failure == nil {
                statusLine("Reading the Mac's command list…")
            } else if let failure, catalog == nil {
                // Static knowledge still stands when discovery does not: the
                // model row works, and the guard on dialog commands holds.
                row(command: "model")
                CCHairline()
                statusLine(failure)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(matches, id: \.self) { command in
                            row(command: command)
                            if command != matches.last { CCHairline() }
                        }
                        if matches.isEmpty {
                            statusLine("No built-in matches — sent as typed.")
                        }
                    }
                }
                .frame(maxHeight: 224)
                .scrollBounceBehavior(.basedOnSize)
            }
        }
        .background(CC.color.surfaceOverlay)
        .clipShape(RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
        }
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xs)
    }

    private func row(command: String) -> some View {
        let native = command == "model"
        return Button {
            if native {
                onModel()
            } else {
                switch ClaudeCommandPolicy.action(for: "/\(command)", catalog: catalog) {
                case .blocked(_, let reason): onBlocked(reason)
                case .nativeModel, .passThrough: break
                }
            }
        } label: {
            HStack(spacing: CC.space.xs) {
                Text("/\(command)")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.primary)
                Spacer(minLength: CC.space.sm)
                Text(native ? "App control" : "Mac only")
                    .ccType(CC.type.micro)
                    .foregroundStyle(native ? CCTone.success.color : CC.text.tertiary)
            }
            .padding(.horizontal, CC.space.sm)
            .padding(.vertical, CC.space.xs)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(
            "/\(command), \(native ? "opens app control" : "runs on the Mac only")")
    }

    private func statusLine(_ text: String) -> some View {
        Text(text)
            .ccType(CC.type.micro)
            .foregroundStyle(CC.text.secondary)
            .padding(.horizontal, CC.space.sm)
            .padding(.vertical, CC.space.xs)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// The native `/model` control.
///
/// Injects the *argument form* (`/model sonnet`), which executes inline and
/// keeps the Mac's composer on screen — and, measured, also saves the choice
/// as the default for new sessions. There is no session-only argument form,
/// so the sheet says exactly that instead of pretending; session-only remains
/// a Terminal handoff.
struct ModelSheet: View {
    let sessionKey: String
    var prefill: String = ""
    let onOpenTerminal: () -> Void

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    /// Claude Code's shipping aliases. Free text below covers everything
    /// else, so a new alias is typeable the day it exists.
    private static let aliases = ["fable", "opus", "sonnet", "haiku"]

    private enum Phase: Equatable {
        case choosing
        case sending(String)
        /// Keystrokes landed; Claude Code has not yet said so in the
        /// transcript. `since` drives the honesty timeout.
        case waiting(String, since: Date)
        case confirmed(String)
        case failed(String)
    }

    @State private var phase: Phase = .choosing
    @State private var custom: String = ""
    @State private var chosen: String?
    @State private var timeoutTask: Task<Void, Never>?
    /// The model fact as it stood when send was tapped. The confirmation
    /// rule is "a NEW command-confirmation fact", and new is relative to
    /// this — never to whatever happens to be current when the transcript
    /// event lands.
    @State private var baseline: ConfirmedModel?

    private var state: SessionState? { model.states[sessionKey] }

    private var selection: String? {
        if let chosen { return chosen }
        let typed = custom.trimmingCharacters(in: .whitespacesAndNewlines)
        return typed.isEmpty ? nil : typed
    }

    var body: some View {
        CCSheetChrome("Model", onClose: { dismiss() }) {
            ScrollView {
                sheetBody
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .onAppear {
            // "/model sonnet" typed into the composer arrives here with its
            // argument; the sheet is the confirmation step, not a detour.
            if custom.isEmpty, chosen == nil, !prefill.isEmpty { custom = prefill }
        }
        .onChange(of: custom) { _, typed in
            if !typed.isEmpty { chosen = nil }
        }
        .onChange(of: state?.lastConfirmedModel) { _, current in
            guard case .waiting = phase,
                let name = ModelChangeWatch.confirmed(baseline: baseline, current: current)
            else { return }
            timeoutTask?.cancel()
            withAnimation(CC.motion.small) { phase = .confirmed("Model set to \(name).") }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    private var sheetBody: some View {
        VStack(alignment: .leading, spacing: CC.space.md) {
                if let confirmed = state?.lastConfirmedModel {
                    CCFactRow(
                        "Last confirmed", age: RelativeAge.text(since: confirmed.at),
                        separator: false
                    ) {
                        Text(confirmed.name).ccType(CC.type.body)
                            .foregroundStyle(CC.text.primary)
                    } detail: {
                        Text(confirmed.source).ccType(CC.type.micro)
                            .foregroundStyle(CC.text.tertiary)
                    }
                } else {
                    CCFactRow("Last confirmed", separator: false) {
                        Text("Unknown").ccType(CC.type.body)
                            .foregroundStyle(CC.text.secondary)
                    } detail: {
                        Text("This session has not said which model it runs.")
                            .ccType(CC.type.micro)
                            .foregroundStyle(CC.text.tertiary)
                    }
                }

                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Self.aliases, id: \.self) { alias in
                        aliasRow(alias)
                        if alias != Self.aliases.last { CCHairline() }
                    }
                }
                .ccSurface(.raised, radius: CC.radius.md)

                CCField(
                    label: "Other model or alias", text: $custom,
                    placeholder: "e.g. claude-sonnet-5",
                    autocapitalization: .never,
                    disableAutocorrection: true)

                CCBanner(
                    "Also becomes your default",
                    message:
                        "Claude Code's argument form changes this session and saves the "
                        + "choice as the default for new sessions. A session-only change "
                        + "exists only in the Mac's own picker — use Terminal for that.",
                    tone: .warning, icon: "exclamationmark.triangle")

                statusLines

                CCButton(
                    primaryTitle,
                    variant: .primary,
                    disabledReason: disabledReason
                ) {
                    if let selection { send(selection) }
                }
                CCButton("Use Terminal for session only", variant: .ghost) {
                    dismiss()
                    onOpenTerminal()
                }
            }
        .padding(CC.space.md)
    }

    private var disabledReason: CCDisabledReason? {
        if selection == nil { return CCDisabledReason("Pick a model or type one") }
        if !phaseAllowsSending { return CCDisabledReason("Waiting on the Mac") }
        return nil
    }

    private var phaseAllowsSending: Bool {
        switch phase {
        case .choosing, .confirmed, .failed: return true
        case .sending, .waiting: return false
        }
    }

    private var primaryTitle: String {
        if let selection { return "Set \(selection) & make default" }
        return "Set model & make default"
    }

    private func aliasRow(_ alias: String) -> some View {
        Button {
            chosen = alias
            custom = ""
        } label: {
            HStack(spacing: CC.space.xs) {
                Text(alias.capitalized).ccType(CC.type.body)
                    .foregroundStyle(CC.text.primary)
                Spacer(minLength: 0)
                if chosen == alias {
                    CCIcon("checkmark", size: 13, weight: .semibold, relativeTo: .body)
                        .foregroundStyle(CCTone.success.color)
                } else if let confirmed = state?.lastConfirmedModel,
                    confirmed.name.lowercased().contains(alias)
                {
                    Text("last confirmed").ccType(CC.type.micro)
                        .foregroundStyle(CC.text.tertiary)
                }
            }
            .padding(.horizontal, CC.space.sm)
            .padding(.vertical, CC.space.sm)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    @ViewBuilder
    private var statusLines: some View {
        switch phase {
        case .choosing:
            EmptyView()
        case .sending(let alias):
            statusLine("Typing /model \(alias) on the Mac…", tone: .neutral)
        case .waiting:
            statusLine(
                "Typed on the Mac. Waiting for Claude Code to confirm.", tone: .neutral)
        case .confirmed(let message):
            statusLine(message, tone: .success)
        case .failed(let reason):
            statusLine(reason, tone: .warning)
        }
    }

    private func statusLine(_ text: String, tone: CCTone) -> some View {
        Text(text)
            .ccType(CC.type.footnote)
            .foregroundStyle(tone == .neutral ? CC.text.secondary : tone.color)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func send(_ alias: String) {
        baseline = state?.lastConfirmedModel
        phase = .sending(alias)
        Task {
            let attempt = await model.sendModelCommand(alias, to: sessionKey)
            switch attempt {
            case .sent:
                // Keystrokes landed — which proves typing, not execution. The
                // transcript's own confirmation is what flips this to done —
                // and it may already have landed while we were `.sending`,
                // so the current fact is checked before waiting on a change.
                if let name = ModelChangeWatch.confirmed(
                    baseline: baseline, current: state?.lastConfirmedModel)
                {
                    phase = .confirmed("Model set to \(name).")
                } else {
                    phase = .waiting(alias, since: Date())
                    armTimeout()
                }
            case .alreadyApplied:
                // The daemon replayed an earlier attempt's outcome without
                // typing again — no new transcript line is coming, and
                // waiting for one would be a guaranteed timeout.
                phase = .confirmed("Already applied by an earlier attempt.")
            case .refused(let reason):
                phase = .failed("Nothing was typed: \(reason)")
            case .failed(let reason):
                phase = .failed(reason)
            case .indeterminate(let reason):
                phase = .failed(
                    "CodeConnect can't tell whether this landed: \(reason) "
                        + "Sending the same choice again is safe — a retry is "
                        + "recognised, not retyped.")
            }
        }
    }

    private func armTimeout() {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            if case .waiting = phase {
                withAnimation(CC.motion.small) {
                    phase = .failed(
                        "Claude Code did not confirm the change. It may still have run — "
                            + "check the session timeline or Terminal.")
                }
            }
        }
    }
}

/// "12m ago" — one place, so every age in this feature reads the same.
enum RelativeAge {
    static func text(since date: Date, now: Date = Date()) -> String {
        let seconds = max(0, now.timeIntervalSince(date))
        if seconds < 60 { return "just now" }
        if seconds < 3600 { return "\(Int(seconds / 60))m ago" }
        if seconds < 86_400 { return "\(Int(seconds / 3600))h ago" }
        return "\(Int(seconds / 86_400))d ago"
    }
}
