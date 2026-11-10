import AppKit
import CodeConnectKit
import SwiftTerm
import SwiftUI

/// One run's terminal: SwiftTerm's Mac view, holding the tmux pane's own size once the stream states it. The daemon
/// puts xterm's window resize, `CSI 8 ; rows ; cols t`, ahead of every paint, so the grid is the pane's, and a window
/// resize scrolls the pane rather than re-wrapping what the agent drew.
final class TerminalPaneView: TerminalView {
    /// The pane's size once the daemon has stated it; until then the emulator takes its size from the view.
    private(set) var paneSize: PaneSize?
    /// The pane's size was stated.
    var onPaneSize: ((PaneSize) -> Void)?

    /// Take the pane's size. The grid changes at once, so every byte after the size is parsed on it, and the view
    /// becomes exactly that grid, which is the grid SwiftTerm derives back from the view's frame.
    func takePaneSize(_ size: PaneSize) {
        paneSize = size
        getTerminal().resize(cols: size.cols, rows: size.rows)
        setFrameSize(getOptimalFrameSize().size)
        onPaneSize?(size)
    }

    /// SwiftTerm hands the two numbers on in the order they arrive, rows first, under the labels `cols` and `rows`.
    override func windowCommand(source: Terminal, command: Terminal.WindowManipulationCommand) -> [UInt8]? {
        guard case .resizeTerminal(let rows, let cols) = command else {
            return super.windowCommand(source: source, command: command)
        }
        takePaneSize(PaneSize(cols: cols, rows: rows))
        return nil
    }

    /// A sideways swipe pans the window across a pane wider than it. The window also gets every event with no
    /// vertical movement, which SwiftTerm drops: a gesture begins and ends with one, and a window that never saw its
    /// end would stay pulled past the pane's edge. Every other swipe stays the terminal's, as SwiftTerm has it: its
    /// scrollback, or the agent's own mouse wheel.
    override func scrollWheel(with event: NSEvent) {
        if event.scrollingDeltaY == 0 || abs(event.scrollingDeltaX) > abs(event.scrollingDeltaY) {
            enclosingScrollView?.scrollWheel(with: event)
        } else {
            super.scrollWheel(with: event)
        }
    }

    // What the terminal shows, for a screen reader and the accessibility driver: SwiftTerm's Mac view exposes nothing.
    // A text area whose value is the screen's lines, top to bottom, each a whole row, blanks included; whose
    // insertion point is the terminal's cursor; whose label is the real grid; and which reads focused while it has
    // the keyboard.

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

    /// One cell in points, from SwiftTerm's own pixel-snapped cell.
    var cellSize: CGSize? {
        guard let pixels = cellSizeInPixels(source: getTerminal()) else { return nil }
        let scale = window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2
        return CGSize(width: CGFloat(pixels.width) / scale, height: CGFloat(pixels.height) / scale)
    }
}

/// The pane in a scroll view. Once the pane's size is known the pane keeps it and the window scrolls over it; until
/// then the pane fills what is visible and SwiftTerm sizes the emulator from there, as it always has.
final class TerminalPaneScroller: NSScrollView {
    let pane = TerminalPaneView(frame: .zero)
    /// What fits the visible area, in cells: the size this Mac shows, for the daemon, once the emulator holds the
    /// pane's.
    var onViewport: ((_ cols: Int, _ rows: Int) -> Void)?
    private var reportedViewport: (cols: Int, rows: Int)?

    init() {
        super.init(frame: .zero)
        hasVerticalScroller = true
        hasHorizontalScroller = true
        autohidesScrollers = true
        // Bars that stay while the pane is larger than the window, so what lies past its edge is never out of sight.
        scrollerStyle = .legacy
        drawsBackground = true
        documentView = pane
        pane.onPaneSize = { [weak self] _ in self?.needsLayout = true }
    }

    required init?(coder: NSCoder) { fatalError("not from a nib") }

    override func layout() {
        super.layout()
        let visible = contentView.bounds.size
        guard pane.paneSize != nil else {
            if pane.frame.size != visible { pane.setFrameSize(visible) }
            return
        }
        guard let cell = pane.cellSize, cell.width > 0, cell.height > 0 else { return }
        let viewport = (cols: Int(visible.width / cell.width), rows: Int(visible.height / cell.height))
        guard viewport.cols > 0, viewport.rows > 0,
            reportedViewport.map({ $0 != viewport }) ?? true
        else { return }
        reportedViewport = viewport
        onViewport?(viewport.cols, viewport.rows)
    }
}

/// The live terminal of one run, fed by its computer's terminal carrier. Typing goes to the carrier; the carrier's
/// output comes here, and only here, while this view is in the pane, shown or hidden.
struct TerminalSurface: NSViewRepresentable {
    let carrier: TerminalCarrier
    let sessionUID: String
    /// Whether its tab is shown. Hidden, the terminal draws nothing, takes no clicks, is not read out, and gives up
    /// the keyboard, so nothing typed elsewhere reaches the run; shown again, it takes the keyboard back.
    let onScreen: Bool

    func makeNSView(context: Context) -> TerminalPaneScroller {
        let scroller = TerminalPaneScroller()
        let pane = scroller.pane
        pane.terminalDelegate = context.coordinator
        pane.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        let background = NSColor(CC.ansi.background)
        scroller.backgroundColor = background
        pane.nativeBackgroundColor = background
        pane.nativeForegroundColor = NSColor(CC.ansi.foreground)
        pane.caretColor = NSColor(CC.ansi.foreground)
        // The app's own 16 colours: SwiftTerm's defaults are tuned for a light background.
        pane.installColors(CC.ansi.table.map { SwiftTerm.Color(red: $0.red, green: $0.green, blue: $0.blue) })
        pane.setAccessibilityIdentifier("terminal.view")
        context.coordinator.attach(scroller: scroller, carrier: carrier, sessionUID: sessionUID)
        scroller.isHidden = !onScreen
        // The terminal takes the keyboard when it appears, as a terminal window does: what is typed next is the run's.
        if onScreen {
            DispatchQueue.main.async { [weak scroller] in scroller?.window?.makeFirstResponder(scroller?.pane) }
        }
        return scroller
    }

    func updateNSView(_ scroller: TerminalPaneScroller, context: Context) {
        context.coordinator.attach(scroller: scroller, carrier: carrier, sessionUID: sessionUID)
        guard scroller.isHidden == onScreen else { return }
        if !onScreen, scroller.window?.firstResponder === scroller.pane { scroller.window?.makeFirstResponder(nil) }
        scroller.isHidden = !onScreen
        if onScreen { scroller.window?.makeFirstResponder(scroller.pane) }
    }

    static func dismantleNSView(_ scroller: TerminalPaneScroller, coordinator: Coordinator) {
        coordinator.detach()
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    /// SwiftTerm calls the delegate from the main thread (`TerminalView` is an `NSView`), so the callbacks assert
    /// that rather than hopping, which would reorder keystrokes. Every stored property is main-actor.
    final class Coordinator: NSObject, TerminalViewDelegate, @unchecked Sendable {
        @MainActor private weak var scroller: TerminalPaneScroller?
        @MainActor private var carrier: TerminalCarrier?
        /// The run whose transcript this emulator was seeded with, so an update replays nothing and another run
        /// replays its own.
        @MainActor private var replayedRun: String?

        @MainActor
        func attach(scroller: TerminalPaneScroller, carrier: TerminalCarrier, sessionUID: String) {
            self.scroller = scroller
            self.carrier = carrier
            scroller.onViewport = { [weak self] cols, rows in self?.carrier?.resize(cols: cols, rows: rows) }
            scroller.pane.onPaneSize = { [weak self, weak scroller] size in
                self?.carrier?.paneSize = size
                scroller?.needsLayout = true
            }
            guard replayedRun != sessionUID else { return }
            replayedRun = sessionUID
            // The pane's size as the stream last stated it, ahead of the replay drawn on it.
            if let size = carrier.paneSize { scroller.pane.takePaneSize(size) }
            let transcript = carrier.transcript(forRun: sessionUID)
            if !transcript.isEmpty { scroller.pane.feed(byteArray: transcript) }
            carrier.deliverOutput(to: self) { [weak scroller] bytes in scroller?.pane.feed(byteArray: bytes) }
        }

        @MainActor
        func detach() {
            // Stamped with this coordinator, so a teardown after the next view is receiving cannot silence it.
            carrier?.stopDeliveringOutput(to: self)
        }

        // MARK: TerminalViewDelegate

        nonisolated func send(source: TerminalView, data: ArraySlice<UInt8>) {
            MainActor.assumeIsolated { carrier?.send(data) }
        }

        /// The emulator's own size, which is the Mac's only until the pane's size is known; from then on the
        /// viewport is reported instead.
        nonisolated func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
            MainActor.assumeIsolated {
                guard (source as? TerminalPaneView)?.paneSize == nil else { return }
                carrier?.resize(cols: newCols, rows: newRows)
            }
        }

        nonisolated func setTerminalTitle(source: TerminalView, title: String) {}
        nonisolated func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        nonisolated func scrolled(source: TerminalView, position: Double) {}
        nonisolated func bell(source: TerminalView) {
            MainActor.assumeIsolated { NSSound.beep() }
        }
        /// What the agent copied (OSC 52), onto this Mac's clipboard, as the iPhone puts it on the phone's.
        nonisolated func clipboardCopy(source: TerminalView, content: Data) {
            let text = String(decoding: content, as: UTF8.self)
            MainActor.assumeIsolated {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
            }
        }
        /// Denied: an agent that can read this Mac's clipboard can read whatever was last copied, here or anywhere.
        nonisolated func clipboardRead(source: TerminalView) -> Data? { nil }
        nonisolated func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }
}
