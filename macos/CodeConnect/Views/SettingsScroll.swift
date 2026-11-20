import AppKit
import SwiftUI

/// A Settings pane's page, as a Mac Settings window sizes one: as tall as its content, up to `maxHeight` and to what the
/// screen's visible frame leaves under the window's own chrome, past which it scrolls. So the window takes each pane's
/// own height, and switching panes resizes it, without ever reaching under the Dock or the menu bar.
struct SettingsScroll<Content: View>: View {
    static var maxHeight: CGFloat { 640 }
    @ViewBuilder let content: Content
    @State private var height: CGFloat = 0
    @State private var room: CGFloat = .infinity

    var body: some View {
        ScrollView {
            content
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(24)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        }
        .frame(width: 560, height: min(height, Self.maxHeight, room))
        .background(Theme.sheet)
        .background(OnScreen { room = $0 })
    }
}

/// Keeps its window inside its screen's visible frame. A window grows from its top-left as its pane fills in, so one
/// placed for a short pane runs under the Dock once the pane is taller: on every resize it is moved up until its bottom
/// is on the visible frame. It reports the pane height the visible frame leaves under the window's chrome.
private struct OnScreen: NSViewRepresentable {
    let room: (CGFloat) -> Void

    func makeNSView(context: Context) -> Probe { Probe(room: room) }
    func updateNSView(_ view: Probe, context: Context) { view.room = room }

    final class Probe: NSView {
        var room: (CGFloat) -> Void
        private var resizing: NSObjectProtocol?

        init(room: @escaping (CGFloat) -> Void) {
            self.room = room
            super.init(frame: .zero)
        }

        required init?(coder: NSCoder) { nil }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            resizing.map(NotificationCenter.default.removeObserver)
            resizing = nil
            guard let window else { return }
            resizing = NotificationCenter.default.addObserver(
                forName: NSWindow.didResizeNotification, object: window, queue: .main
            ) { [weak self] _ in MainActor.assumeIsolated { self?.fit() } }
            fit()
        }

        private func fit() {
            guard let window, let visible = window.screen?.visibleFrame else { return }
            let chrome = window.frame.height - window.contentLayoutRect.height
            room(max(0, visible.height - chrome))
            if window.frame.minY < visible.minY {
                window.setFrameOrigin(NSPoint(x: window.frame.minX, y: visible.minY))
            }
        }
    }
}
