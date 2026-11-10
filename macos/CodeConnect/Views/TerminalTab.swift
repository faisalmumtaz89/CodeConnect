import CodeConnectKit
import SwiftUI

/// A run's terminal: why it cannot open, the control that opens it, or the live terminal. Showing the tab attaches
/// nothing. A terminal is shell-equivalent authority, so it opens on a click, comes back by itself only into this
/// run's own terminal that was already open, and is taken from another run only through the block's own button,
/// which names that run.
struct TerminalTab: View {
    let row: FleetRow
    let model: AppModel
    /// Whether the tab is the one shown; once shown, the pane keeps it, hidden behind the conversation.
    let onScreen: Bool
    /// The computer's own re-pair, for a terminal this connection may not open.
    let pairAgain: () -> Void

    private var carrier: TerminalCarrier { model.terminal }
    private var sessionUID: String { row.id }

    var body: some View {
        Group {
            switch model.terminalConnect(sessionUID: sessionUID) {
            case .blocked(let reason, let fix):
                Blocked(reason: reason, fix: fix) { run(fix) }
            case .ready(let uid):
                Live(carrier: carrier, sessionUID: uid, onScreen: onScreen) { open(uid) }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.window)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("terminal.tab")
        // Coming to the tab, the first time or again, is when this run's own terminal comes back.
        .onChange(of: onScreen, initial: true) { reopen() }
        // So is the link coming back while the tab shows, after a sleep or a drop, as on the iPhone. The daemon then
        // repaints the pane whole. Only once connected: an attach before that would only be refused.
        .onChange(of: model.connection.phase.isConnected) { _, connected in
            if connected { reopen() }
        }
    }

    private func reopen() {
        if onScreen, model.terminalReopensByItself(sessionUID: sessionUID) { open(sessionUID) }
    }

    private func open(_ uid: String) {
        carrier.attach(sessionUID: uid, cols: carrier.lastSize.cols, rows: carrier.lastSize.rows)
    }

    private func run(_ fix: TerminalConnect.Fix?) {
        switch fix {
        case .pair: pairAgain()
        case .takeOver: carrier.takeOver(sessionUID: sessionUID, cols: carrier.lastSize.cols, rows: carrier.lastSize.rows)
        case nil: break
        }
    }
}

/// Why the terminal cannot open, in Kit's words, with the one route that changes it when there is one.
private struct Blocked: View {
    let reason: String
    let fix: TerminalConnect.Fix?
    let action: () -> Void

    var body: some View {
        VStack(spacing: 14) {
            Text(reason)
                .font(Theme.ui(13))
                .foregroundStyle(Theme.text2)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
                .accessibilityIdentifier("terminal.blocked")
            if let fix {
                ActionButton(title: fix.title(), key: nil, prominent: false, id: "terminal.blocked.fix", action: action)
            }
        }
        .padding(28)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// This run's terminal: the control that opens it, and once opened, the terminal itself with how it stands. As on the
/// iPhone, an open terminal stays open until its connection ends or another run takes it.
private struct Live: View {
    let carrier: TerminalCarrier
    let sessionUID: String
    let onScreen: Bool
    let open: () -> Void
    /// Whether this view's terminal began from a transcript that had lost its head to Kit's cap, so its screen may be
    /// partial; as on the iPhone, it says so above the terminal. A fresh attach repaints the screen whole and clears it.
    @State private var seededFromTrimmedBuffer = false

    var body: some View {
        let phase = carrier.phase(forRun: sessionUID)
        let shown = !carrier.transcript(forRun: sessionUID).isEmpty
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                if let status = status(phase) {
                    Text(status)
                        .font(Theme.ui(12))
                        .foregroundStyle(Theme.text2)
                        .lineLimit(2)
                        .accessibilityIdentifier("terminal.status")
                }
                Spacer(minLength: 0)
                if !isOpen(phase), mayOpen {
                    ActionButton(title: "Open terminal", key: nil, prominent: true, id: "terminal.open", action: open)
                }
            }
            .padding(.horizontal, 20)
            .frame(height: 44)
            .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
            if isOpen(phase) || shown {
                // A sibling, so showing it never rebuilds the terminal it describes.
                if seededFromTrimmedBuffer {
                    Text(TerminalCarrier.trimmedBufferNotice)
                        .font(Theme.ui(11.5))
                        .foregroundStyle(Theme.text3)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 20)
                        .padding(.vertical, 6)
                        .accessibilityIdentifier("terminal.trimmed")
                }
                TerminalSurface(carrier: carrier, sessionUID: sessionUID, onScreen: onScreen) {
                    seededFromTrimmedBuffer = $0
                }
            } else {
                Text("Open the terminal to see this run on its computer and type into it.")
                    .font(Theme.ui(12.5))
                    .foregroundStyle(Theme.text3)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 420)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .onChange(of: isAttached(phase)) { _, attached in
            if attached { seededFromTrimmedBuffer = false }
        }
    }

    private func isAttached(_ phase: TerminalCarrier.Phase) -> Bool {
        if case .attached = phase { return true }
        return false
    }

    /// Not after a close of this run's terminal that asking again cannot change (the session ended, or is not
    /// CodeConnect's): Open would only be refused the same way. Another run's close says nothing about this one.
    private var mayOpen: Bool {
        guard carrier.standing(forRun: sessionUID).isMine, let close = carrier.lastClose else { return true }
        return close.isRetryable
    }

    private func isOpen(_ phase: TerminalCarrier.Phase) -> Bool {
        switch phase {
        case .attaching, .attached: return true
        case .idle, .ended: return false
        }
    }

    private func status(_ phase: TerminalCarrier.Phase) -> String? {
        switch phase {
        case .attaching: return "Connecting…"
        case .ended(let reason, _): return reason
        case .idle, .attached: return nil
        }
    }
}
