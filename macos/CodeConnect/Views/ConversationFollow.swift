import AppKit
import CodeConnectKit
import SwiftUI

/// The conversation's top, read off the column's scroll view: the iPhone's `HeadObserver` (SessionDetailView.swift) in
/// AppKit. It asks Kit for the page above when the reader comes to the top, and passes on a reader opening a row to read
/// it. Whether the reader follows the newest rows is the column's layout's (`MacColumnLayout.following`), which this
/// reads. Owned by the conversation for as long as it is open. Nothing draws from it, so none of its writes updates a
/// view.
///
/// It reads once per turn of the run loop. AppKit tells of the clip view's bounds and the document view's frame
/// separately, so one layout pass read a notification at a time can look like a scroll with nothing changing size;
/// read at the end of the turn, a scroll and the change of size that came with it are one reading. What the column's
/// own `apply` moved in that turn (`applied(_:)`) is taken out first: a reading holds only the reader's own move.
@MainActor
final class Tail {
    /// Asks Kit for the page above: the reader came to the top.
    var reachedTop: () -> Void = {}
    /// Asks Kit for the page above without the reader: a column shorter than the pane, or a page that left the reader
    /// near the top.
    var pageBackOnItsOwn: () -> Void = {}
    /// The reader opened a row to read it: the column stops following.
    var readerLeaves: () -> Void = {}
    /// The column follows its newest rows.
    var isFollowing: () -> Bool = { true }
    private weak var scrollView: NSScrollView?
    private var observers: [NSObjectProtocol] = []
    private var dirty = false
    /// A live scroll is under way: a trackpad, its momentum, the scroller dragged.
    private var hand = false
    private var last: (y: CGFloat, height: CGFloat, pane: CGSize)?
    /// What the column's `apply` moved since the last reading.
    private var ownMoves = MacColumnLayout.Moved(dy: 0, grown: 0)
    /// The reader's ask for the page above went out and they have not left the top since.
    private var arrived = false
    /// The one pending ask; a burst of readings replaces it.
    private var ask: Task<Void, Never>?

    /// The reader opened a row to read it (a tool's output, a card's full input or the rest of its files): the column
    /// stops following before the row grows.
    func leave() {
        readerLeaves()
    }

    /// The column's `apply` moved the origin and the column's height by these: the next reading takes them out.
    func applied(_ moved: MacColumnLayout.Moved) {
        ownMoves = .init(dy: ownMoves.dy + moved.dy, grown: ownMoves.grown + moved.grown)
    }

    /// Kit became able to page with nothing on screen having moved (the link came back, a catch-up ended): a reader
    /// whose ask went out and who is still at the top, or a column shorter than the pane, gets the page.
    func wake() {
        guard let scrollView, arrived || Self.isShort(scrollView) else { return }
        later(pageBackOnItsOwn) { Self.isShort($0) || Self.isNearTop($0) }
    }

    func attach(_ scrollView: NSScrollView) {
        guard scrollView !== self.scrollView else { return }
        detach()
        self.scrollView = scrollView
        last = nil
        // The pane's scroll area by name: its bar and page parts are what a test drives.
        scrollView.setAccessibilityIdentifier("session.conversation")
        let clip = scrollView.contentView
        clip.postsBoundsChangedNotifications = true
        clip.postsFrameChangedNotifications = true
        scrollView.documentView?.postsFrameChangedNotifications = true
        let center = NotificationCenter.default
        let mark: @Sendable (Notification) -> Void = { [weak self] _ in MainActor.assumeIsolated { self?.markDirty() } }
        observers = [
            center.addObserver(forName: NSView.boundsDidChangeNotification, object: clip, queue: nil, using: mark),
            center.addObserver(forName: NSView.frameDidChangeNotification, object: clip, queue: nil, using: mark),
            center.addObserver(
                forName: NSView.frameDidChangeNotification, object: scrollView.documentView, queue: nil, using: mark),
            center.addObserver(forName: NSScrollView.willStartLiveScrollNotification, object: scrollView, queue: nil) {
                [weak self] _ in MainActor.assumeIsolated { self?.hand = true }
            },
            center.addObserver(forName: NSScrollView.didEndLiveScrollNotification, object: scrollView, queue: nil) {
                [weak self] _ in MainActor.assumeIsolated { self?.hand = false }
            },
        ]
        markDirty()
    }

    func detach() {
        observers.forEach(NotificationCenter.default.removeObserver)
        observers = []
        ask?.cancel()
    }

    private func markDirty() {
        guard !dirty else { return }
        dirty = true
        RunLoop.main.perform { [weak self] in MainActor.assumeIsolated { self?.read() } }
    }

    private func read() {
        dirty = false
        guard let scrollView, let document = scrollView.documentView else { return }
        let y = scrollView.contentView.bounds.origin.y, height = document.frame.height
        let pane = scrollView.contentView.bounds.size
        let own = ownMoves
        ownMoves = .init(dy: 0, grown: 0)
        defer { self.last = (y, height, pane) }
        // The first reading, made as the column opens, has nothing to compare with; a column already shorter than the
        // pane pages back on its own.
        guard let last else {
            if Self.isShort(scrollView) { later(pageBackOnItsOwn, if: Self.isShort) }
            return
        }
        let dy = y - last.y - own.dy, grown = height - last.height - own.grown, resized = pane != last.pane
        noteFollow(String(format: "READ y %.3f dy %.3f h %.3f grown %.3f pane %.3f following %d hand %d",
            y, dy, height, grown, pane.height, isFollowing() ? 1 : 0, hand ? 1 : 0))
        guard abs(dy) > 0.5 || abs(grown) > 0.01 || resized else { return }
        readTop(scrollView, dy: dy)
    }

    /// The top, as the iPhone's `HeadObserver` reads it. A column shorter than the pane pages back on its own; a hand
    /// within a pane's height of the top asks for the page above, and so does any reading that finds the reader at the
    /// top, by a move up or while not following the newest rows.
    private func readTop(_ scrollView: NSScrollView, dy: CGFloat) {
        if Self.isShort(scrollView) {
            if hand {
                readerAsks(if: Self.isShort)
            } else {
                later(pageBackOnItsOwn, if: Self.isShort)
            }
            return
        }
        guard Self.isNearTop(scrollView) else {
            arrived = false
            return
        }
        let atTop = scrollView.contentView.bounds.origin.y <= 0.5 && (dy < -0.5 || !isFollowing())
        guard hand || atTop else { return }
        readerAsks(if: Self.isNearTop)
    }

    /// The reader's own ask: once it goes out, they have come to the top.
    private func readerAsks(if still: @escaping @MainActor (NSScrollView) -> Bool) {
        later({ [weak self] in
            self?.arrived = true
            self?.reachedTop()
        }, if: still)
    }

    /// Deferred past this turn and checked again when it runs, so nothing is asked from inside layout.
    private func later(_ action: @escaping @MainActor () -> Void, if still: @escaping @MainActor (NSScrollView) -> Bool) {
        ask?.cancel()
        ask = Task { [weak self] in
            guard !Task.isCancelled, let self, let scrollView = self.scrollView, still(scrollView) else { return }
            action()
        }
    }

    /// The column is shorter than the pane.
    static func isShort(_ scrollView: NSScrollView) -> Bool {
        (scrollView.documentView?.frame.height ?? 0) < scrollView.contentView.bounds.height
    }

    /// Within a pane's height of the top: the page is asked for before the reader runs out of rows.
    static func isNearTop(_ scrollView: NSScrollView) -> Bool {
        scrollView.contentView.bounds.origin.y <= scrollView.contentView.bounds.height
    }
}

extension EnvironmentValues {
    /// The open conversation's `Tail`, for a row that opens more of itself on a click: opening is choosing to read,
    /// and must reach the follow before the row grows. Nil outside the column (a request at the pane's foot).
    @Entry var conversationTail: Tail?
}

/// `-CC_FOLLOW_LOG YES` (Debug builds): one line on stderr for each change the column makes (APPLY: what changed, the
/// origin and the column's height before and after), each reading of the top (READ, with the column's own moves taken
/// out) and each turn of the follow (FOLLOW 1 or 0, and why), so a test reads them from outside the app. Nothing in a
/// Release build.
func noteFollow(_ move: @autoclosure () -> String) {
    #if DEBUG
        guard UserDefaults.standard.bool(forKey: "CC_FOLLOW_LOG") else { return }
        let line = "follow \(String(format: "%.3f", Date().timeIntervalSince1970)) \(move())\n"
        FileHandle.standardError.write(Data(line.utf8))
    #endif
}
