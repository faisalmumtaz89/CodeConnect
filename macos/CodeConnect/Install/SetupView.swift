import CodeConnectKit
import SwiftUI

/// The setup checklist: what CodeConnect needs on this Mac, each row answered as it is found. The
/// agents are never bundled. Plain words only, no command: the line Set up adds to the Terminal's start-up file
/// is in that button's help. Once the checklist has its answers, if a row needs the person, Try Again runs it again.
struct SetupView: View {
    @Environment(Setup.self) private var setup
    @Environment(Machines.self) private var machines
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Set up CodeConnect").font(Theme.ui(17, .semibold)).foregroundStyle(Theme.text)
                Text("Everything this Mac needs to run agents you can follow from anywhere.")
                    .font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
            }
            .padding(.horizontal, 24)
            .padding(.top, 22)
            .padding(.bottom, 16)
            VStack(spacing: 0) {
                row("Background service", setup.service, id: "setup.service")
                row("Claude Code", setup.agent(.claude, in: machines), id: "setup.claude")
                row("Codex", setup.agent(.codex, in: machines), id: "setup.codex")
                commandLine
            }
            .background(Theme.raised.opacity(0.6), in: RoundedRectangle(cornerRadius: Theme.boxRadius))
            .overlay(RoundedRectangle(cornerRadius: Theme.boxRadius).strokeBorder(Theme.hairline))
            .padding(.horizontal, 24)
            Spacer(minLength: 16)
            VStack(spacing: 0) {
                Rectangle().fill(Theme.hairline).frame(height: 1)
                HStack(spacing: 8) {
                    Spacer()
                    if setup.settled, setup.needsAttention(machines) {
                        ActionButton(title: "Try Again", key: nil, prominent: false, id: "setup.tryAgain") {
                            setup.reopen(machines: machines)
                        }
                    }
                    ActionButton(title: "Done", key: nil, prominent: true, id: "setup.done") { dismiss() }
                        .keyboardShortcut(.defaultAction)
                }
                .padding(.horizontal, 14)
                .frame(height: 52)
            }
        }
        .frame(width: 560, height: 470)
        .background(Theme.sheet)
        // The agents' rows are this Mac's service's answer: asked for once its link is up, while the checklist is open.
        .task(id: machines.startContexts.map(\.id)) { setup.thisMacContext(in: machines)?.load() }
    }

    /// A row's answer: a word beside its name, or, for a row that isn't ready, its sentence under it.
    private func row(_ title: String, _ state: Setup.Row, id: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                mark(state, id: id)
                Text(title).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                Spacer(minLength: 12)
                if !state.isMissing {
                    Text(detail(state)).font(Theme.ui(12)).foregroundStyle(Theme.text2).lineLimit(1)
                }
            }
            if state.isMissing {
                Text(detail(state))
                    .font(Theme.ui(11.5))
                    .foregroundStyle(Theme.text2)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 24)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    /// CodeConnect in a new Terminal window: set up only when the person chooses Set up.
    @ViewBuilder private var commandLine: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                mark(setup.commandLine, id: "setup.commandLine")
                Text("Use CodeConnect from Terminal").font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                Spacer(minLength: 12)
                Text(detail(setup.commandLine)).font(Theme.ui(12)).foregroundStyle(Theme.text2)
            }
            if case .missing = setup.commandLine, let installer = setup.installer {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text("Set up puts the codeconnect command on the PATH of new Terminal windows.")
                        .font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 0)
                    ActionButton(title: "Set up", key: nil, prominent: false, id: "setup.setUp") { setup.addPathBlock() }
                        .help("Adds to ~/.zprofile: \(PathBlock.line(home: installer.home, bin: installer.bin))")
                }
                .padding(.leading, 24)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("setup.commandLine")
    }

    private func mark(_ state: Setup.Row, id: String) -> some View {
        Group {
            switch state {
            case .checking: Image(systemName: "circle.dotted").foregroundStyle(Theme.text3)
            case .ready: Image(systemName: "checkmark.circle.fill").foregroundStyle(Theme.text2)
            case .missing: Image(systemName: "exclamationmark.circle.fill").foregroundStyle(Theme.amber)
            }
        }
        .font(.system(size: 12))
        .frame(width: 14)
        // The row's state, in the same words on every row.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(word(state))
        .accessibilityIdentifier("\(id).state")
    }

    private func word(_ state: Setup.Row) -> String {
        switch state {
        case .checking: "Checking"
        case .ready: "Ready"
        case .missing: "Not ready"
        }
    }

    private func detail(_ state: Setup.Row) -> String {
        switch state {
        case .checking: "Checking…"
        case .ready(let text), .missing(let text): text
        }
    }
}
