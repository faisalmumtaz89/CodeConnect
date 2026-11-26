import AppKit
import CodeConnectKit
import SwiftTerm
import SwiftUI

/// The run's shell at the foot of its panel: a 28pt bar naming the computer and the folder, and, open, the shell
/// itself below it in 300pt. A click on the bar opens and folds it; folded, or with the panel closed, the shell runs
/// on, and opened again it goes on from where the screen left off. Shown for a run that has not ended, on a computer
/// that serves shells; on a computer that is offline, it says so and offers to try again.
struct TerminalFoot: View {
    let row: FleetRow
    let model: AppModel
    @State private var shells = Shells.shared

    static let barHeight: CGFloat = 28
    static let openHeight: CGFloat = 300

    var body: some View {
        if shown {
            let open = shells.open.contains(row.summary.sessionUID)
            VStack(spacing: 0) {
                Bar(row: row, model: model, open: open, offline: offline) { shells.toggle(row.summary.sessionUID) }
                if open {
                    if offline {
                        Offline(computer: computer) { model.connection.retryNow() }
                    } else {
                        ShellHost(session: row.summary.sessionUID, terminal: model.terminal)
                    }
                }
            }
            .frame(height: open ? Self.openHeight : Self.barHeight)
            .background(Theme.window)
            .overlay(alignment: .top) { Rectangle().fill(Theme.hairline).frame(height: 1) }
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("terminal.foot")
            #if DEBUG
                .task(id: row.summary.sessionUID) {
                    guard RenderWindow.terminalOpen, !shells.open.contains(row.summary.sessionUID) else { return }
                    shells.toggle(row.summary.sessionUID)
                    guard let typed = RenderWindow.terminalTyped else { return }
                    // Once the shell is open and its prompt has had a moment to draw.
                    while !model.terminal.isShowing { try? await Task.sleep(for: .milliseconds(200)) }
                    try? await Task.sleep(for: .seconds(1))
                    model.terminal.type(Array((typed + "\r").utf8))
                }
            #endif
        }
    }

    /// A run that has ended has no shell; a run an account switch moved has not ended. A computer known not to serve
    /// shells shows none; one not heard from yet still shows the bar, so being offline is said.
    private var shown: Bool {
        let ended =
            (row.summary.lifecycle == .exited || row.summary.lifecycle == .failed)
            && row.summary.switching != .moved
        return !ended && model.connection.capabilities?.servesRunTerminal != false
    }

    private var offline: Bool { model.linkHealth.level == .offline }

    private var computer: String { row.machineName ?? "This computer" }
}

/// The bar: what it is, on which computer, in which folder.
private struct Bar: View {
    let row: FleetRow
    let model: AppModel
    let open: Bool
    let offline: Bool
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 8) {
                Image(systemName: "chevron.down")
                    .font(.system(size: 8, weight: .semibold))
                    .rotationEffect(.degrees(open ? 0 : -90))
                    .foregroundStyle(Theme.text3)
                Text("Terminal").font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text)
                Text("·").font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                Image(systemName: model.connection.helloAck?.machineOS == "linux" ? "server.rack" : "desktopcomputer")
                    .font(.system(size: 10))
                    .foregroundStyle(offline ? Theme.text3 : Theme.text2)
                Text(row.machineName ?? "This computer")
                    .font(Theme.ui(11.5, offline ? .regular : .medium))
                    .foregroundStyle(offline ? Theme.text3 : Theme.text)
                    .lineLimit(1)
                    .fixedSize()
                if offline {
                    Text("Offline").font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                } else {
                    Text(folder)
                        .font(Theme.mono(10.5))
                        .foregroundStyle(Theme.text3)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 12)
            .frame(height: TerminalFoot.barHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("terminal.bar")
        .accessibilityLabel(open ? "Hide Terminal" : "Show Terminal")
    }

    /// The shell's folder as the bar names it: a worktree as `worktrees/<name>`, any other folder as the run's own
    /// shortened path.
    private var folder: String {
        var path = row.summary.cwd
        if case .open(let shellFolder) = model.terminal.phase, model.terminal.session == row.summary.sessionUID {
            path = shellFolder
        }
        if let range = path.range(of: "/worktrees/", options: .backwards) {
            return "worktrees/" + path[range.upperBound...]
        }
        return FleetGroup.shellPath(row.summary.shownCwd)
    }
}

/// An offline computer's shell: what happened, and a way to try again.
private struct Offline: View {
    let computer: String
    let retry: () -> Void

    var body: some View {
        VStack(spacing: 8) {
            Text("\(computer) is offline").font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
            Text("The shell opens again when it’s back.").font(Theme.ui(12)).foregroundStyle(Theme.text3)
            ActionButton(title: "Try Again", key: nil, prominent: false, id: "terminal.retry", action: retry)
                .padding(.top, 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.terminal)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("terminal.offline")
    }
}

/// Which runs' shells are open in the panel, and each run's terminal screen, kept while the view is folded or another
/// run is shown, so coming back continues the screen it left.
@MainActor
@Observable
final class Shells {
    static let shared = Shells()

    private(set) var open: Set<String> = []
    /// Whether a shell view holds the keyboard: the window's Esc stands down, so Esc reaches the shell.
    var holdsKeys = false
    @ObservationIgnored private var views: [String: ShellView] = [:]
    @ObservationIgnored private var recent: [String] = []

    /// The most screens kept: the runs shown last.
    static let kept = 8

    func toggle(_ session: String) {
        if open.contains(session) { open.remove(session) } else { open.insert(session) }
    }

    func view(for session: String) -> ShellView {
        recent.removeAll { $0 == session }
        recent.append(session)
        while recent.count > Self.kept { views[recent.removeFirst()] = nil }
        if let view = views[session] { return view }
        let view = ShellView(frame: .zero)
        views[session] = view
        return view
    }
}

/// The shell's screen: SwiftTerm's Mac terminal in the app's terminal colours and Menlo, read out as a text area.
final class ShellView: TerminalView {
    /// Nothing has been fed to this screen yet.
    var isNew = true

    // A stopped screen (its shell exited, or another device took it) is kept for reading and selecting until the
    // person types: what the terminal reports for the program on it (mouse, focus, answers to its queries) goes
    // nowhere, and a click or a scroll selects and scrolls here rather than reporting. Keys and pastes reach the
    // coordinator without passing through `send(source:data:)`, so they still open it again.

    private var stopped: Bool {
        if case .stopped = (terminalDelegate as? ShellHost.Coordinator)?.terminal.phase { return true }
        return false
    }

    override func send(source: Terminal, data: ArraySlice<UInt8>) {
        if !stopped { super.send(source: source, data: data) }
    }

    override func mouseDown(with event: NSEvent) {
        allowMouseReporting = !stopped
        super.mouseDown(with: event)
    }

    // On the alternate screen with reporting off, SwiftTerm turns the wheel into arrow keys, which would be typing.
    override func scrollWheel(with event: NSEvent) {
        allowMouseReporting = !stopped
        if !stopped || !getTerminal().isCurrentBufferAlternate { super.scrollWheel(with: event) }
    }

    override init(frame: CGRect) {
        super.init(frame: frame, font: NSFont(name: Theme.codeFace, size: 11.5))
        getTerminal().options.ansi256PaletteStrategy = .xterm
        getTerminal().changeScrollback(10_000)
        nativeBackgroundColor = NSColor(Theme.terminal)
        nativeForegroundColor = NSColor(Theme.terminalText)
        caretColor = NSColor(Theme.terminalText)
        optionAsMetaKey = false
        setAccessibilityIdentifier("terminal.view")
    }

    required init?(coder: NSCoder) { fatalError("not from a nib") }

    // What the terminal shows, for a screen reader and the accessibility driver: SwiftTerm's Mac view exposes nothing.
    // A text area whose value is the screen's lines, top to bottom, each a whole row, blanks included; whose
    // insertion point is the terminal's cursor; whose label is its grid; and which reads focused while it has the
    // keyboard.

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .textArea }

    override func accessibilityLabel() -> String? {
        let terminal = getTerminal()
        return "Terminal, \(terminal.cols) columns, \(terminal.rows) rows"
    }

    override func accessibilityValue() -> Any? { screenLines().joined(separator: "\n") }

    override func accessibilityInsertionPointLineNumber() -> Int { getTerminal().buffer.y }

    override func accessibilitySelectedTextRange() -> NSRange {
        let cursor = getTerminal().getCursorLocation()
        let lines = screenLines()
        guard cursor.y < lines.count else { return NSRange(location: 0, length: 0) }
        let before = lines.prefix(cursor.y).reduce(0) { $0 + ($1 as NSString).length + 1 }
        return NSRange(location: before + (text(ofRow: cursor.y, upTo: cursor.x) as NSString).length, length: 0)
    }

    override func isAccessibilityFocused() -> Bool { window?.firstResponder === self }

    private func screenLines() -> [String] { (0..<getTerminal().rows).map { text(ofRow: $0) } }

    /// A row as SwiftTerm draws it: a wide character once, though it fills two cells; an emoji whole; and an empty
    /// cell, a NUL to SwiftTerm, as the blank it is. Up to a column, the row before that cell.
    private func text(ofRow row: Int, upTo column: Int = -1) -> String {
        let terminal = getTerminal()
        let line = terminal.getLine(row: row)?.translateToString(
            endCol: column, skipNullCellsFollowingWide: true, characterProvider: { terminal.getCharacter(for: $0) })
        return (line ?? "").replacingOccurrences(of: "\u{0}", with: " ")
    }
}

/// What holds the shown screen: it says, as the window's keyboard moves, whether a shell has it, so the window's Esc
/// stands down while it does.
private final class ShellContainer: NSView {
    private var watching: NSKeyValueObservation?

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        watching = window?.observe(\.firstResponder, options: [.initial, .new]) { window, _ in
            let held = window.firstResponder is ShellView
            MainActor.assumeIsolated { if Shells.shared.holdsKeys != held { Shells.shared.holdsKeys = held } }
        }
        if window == nil { MainActor.assumeIsolated { Shells.shared.holdsKeys = false } }
    }
}

/// The run's kept screen in the panel, joined to the computer's link while it shows: output fed in as it comes,
/// what is typed sent, its size the shell's.
private struct ShellHost: NSViewRepresentable {
    let session: String
    let terminal: RunTerminal

    func makeNSView(context: Context) -> NSView {
        let container = ShellContainer()
        container.wantsLayer = true
        container.layer?.backgroundColor = NSColor(Theme.terminal).cgColor
        mount(in: container, context: context)
        return container
    }

    func updateNSView(_ container: NSView, context: Context) {
        if context.coordinator.session != session { mount(in: container, context: context) }
    }

    static func dismantleNSView(_ container: NSView, coordinator: Coordinator) {
        coordinator.leave()
    }

    func makeCoordinator() -> Coordinator { Coordinator(terminal: terminal) }

    private func mount(in container: NSView, context: Context) {
        let coordinator = context.coordinator
        coordinator.leave()
        let view = Shells.shared.view(for: session)
        view.removeFromSuperview()
        // Pinned, not framed: the container is mounted before it has a size, and a frame inset from an empty one is
        // null, which grows with the container from an origin at infinity.
        view.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 12),
            view.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -12),
            view.topAnchor.constraint(equalTo: container.topAnchor, constant: 10),
            view.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -10),
        ])
        view.terminalDelegate = coordinator
        if view.isNew { terminal.forgetScreen(session: session) }
        view.isNew = false
        coordinator.join(session: session, view: view)
        DispatchQueue.main.async { [weak view] in view?.window?.makeFirstResponder(view) }
    }

    /// SwiftTerm calls its delegate on the main thread (`TerminalView` is an `NSView`), so the callbacks assert that
    /// rather than hopping, which would reorder keystrokes.
    final class Coordinator: NSObject, TerminalViewDelegate, @unchecked Sendable {
        @MainActor let terminal: RunTerminal
        @MainActor private(set) var session: String?
        @MainActor private weak var view: ShellView?

        @MainActor init(terminal: RunTerminal) { self.terminal = terminal }

        @MainActor
        func join(session: String, view: ShellView) {
            self.session = session
            self.view = view
            terminal.output = { [weak view] data, reset in
                guard let view else { return }
                // A fresh screen, as the shell's output begins again from what the computer kept.
                if reset { view.getTerminal().resetToInitialState() }
                view.feed(byteArray: ArraySlice(data))
            }
            let size = view.getTerminal()
            terminal.show(session: session, cols: size.cols, rows: size.rows)
        }

        /// The panel no longer shows this run's shell: it runs on, and its screen is kept.
        @MainActor
        func leave() {
            guard let session else { return }
            if terminal.session == session { terminal.hide() }
            self.session = nil
        }

        // MARK: TerminalViewDelegate

        nonisolated func send(source: TerminalView, data: ArraySlice<UInt8>) {
            MainActor.assumeIsolated {
                // A replay's queries were the program's, long since answered: the terminal's own answers go nowhere.
                guard !terminal.replaying else { return }
                terminal.type(Array(data))
            }
        }

        nonisolated func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
            MainActor.assumeIsolated { terminal.resize(cols: newCols, rows: newRows) }
        }

        nonisolated func setTerminalTitle(source: TerminalView, title: String) {}
        nonisolated func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        nonisolated func scrolled(source: TerminalView, position: Double) {}
        nonisolated func bell(source: TerminalView) {
            MainActor.assumeIsolated { if !terminal.replaying { NSSound.beep() } }
        }
        /// What a program copied (OSC 52), onto this Mac's clipboard.
        nonisolated func clipboardCopy(source: TerminalView, content: Data) {
            let text = String(decoding: content, as: UTF8.self)
            MainActor.assumeIsolated {
                guard !terminal.replaying else { return }
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
            }
        }
        /// Denied: a program that can read this Mac's clipboard can read whatever was last copied, here or anywhere.
        nonisolated func clipboardRead(source: TerminalView) -> Data? { nil }
        nonisolated func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }
}
