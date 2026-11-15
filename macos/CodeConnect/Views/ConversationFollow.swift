import AppKit
import CodeConnectKit
import SwiftUI

/// Whether the conversation follows its newest rows, read off the List's own scroll view: the iPhone's `TailWatch` and
/// `TailObserver` (SessionDetailView.swift) in AppKit. Owned by the conversation for as long as it is open, so what it
/// watches outlives the tail row whose probe found the scroll view: the table reuses a row's view once the reader is
/// far from it. Nothing draws from it, so none of its writes updates a view.
///
/// It reads once per turn of the run loop. AppKit tells of the clip view's bounds and the document view's frame
/// separately, so one layout pass read a notification at a time can look like a scroll with nothing changing size;
/// read at the end of the turn, a scroll and the change of size that came with it are one reading.
///
/// The verdicts, in the iPhone's order: nothing moved, none; a column shorter than the pane, at the end; the pane
/// resized under a reader at the end, the newest rows kept (`PageAnchor.keepLatest`); within 32 pt of the end, at the
/// end; a hand (a trackpad, its momentum, the scroller dragged) moving up, away; a move up with no hand while the
/// column and the pane keep exactly their size (a page by VoiceOver, a key, the scroller's page part), away: a row
/// measured anew always changes the column's height, so it is never taken for the reader leaving; a reader away, still
/// away; otherwise the last row grew under a reader who was following, who is brought back to the end once 400 ms pass
/// with it still off screen.
@MainActor
final class Tail {
    let anchor = PageAnchor()
    /// Asks Kit for the page above: the reader came to the top.
    var reachedTop: () -> Void = {}
    /// The reader left the newest rows: set at once, cleared on arriving.
    private var handAway = false
    /// Settled 400 ms after a departure, so the column's end moving away an instant before the follow's scroll has
    /// caught up is not taken for the reader leaving.
    private var following = true
    /// The one pending verdict: an arrival replaces a departure and the other way round.
    private var verdict: Task<Void, Never>?
    private weak var scrollView: NSScrollView?
    private var observers: [NSObjectProtocol] = []
    private var dirty = false
    /// A live scroll is under way: a trackpad, its momentum, the scroller dragged.
    private var hand = false
    private var last: (y: CGFloat, height: CGFloat, pane: CGSize)?
    /// The reader's ask for the page above went out and they have not left the top since.
    private var arrived = false
    /// The one pending ask; a burst of readings replaces it.
    private var ask: Task<Void, Never>?

    /// Away from the newest rows: rows land below without moving the reader.
    var isAway: Bool { handAway || !following }

    /// The reader left the newest rows, or opened a row to read it (a tool's output, a card's full input or the rest
    /// of its files): away before the row grows, so neither the re-pin nor a row landing carries them through it.
    func leave() {
        if !handAway { noteFollow("LEAVE") }
        handAway = true
        depart()
    }

    /// The rows above the reader are about to change from `shown` to `next` (Kit's `onWindowChange`).
    func willChange(shown: [TimelineItem], next: [TimelineItem], change: SessionState.WindowChange) {
        anchor.willChange(shown: shown, next: next, change: change, away: isAway)
    }

    /// Kit became able to page with nothing on screen having moved (the link came back, a catch-up ended): a reader
    /// whose ask went out and who is still at the top, or a column shorter than the pane, gets the page once the table
    /// shows the timeline published now.
    func wake() {
        guard let scrollView, arrived || PageAnchor.isShort(scrollView) else { return }
        anchor.recheck()
    }

    func attach(_ scrollView: NSScrollView) {
        guard scrollView !== self.scrollView else { return }
        detach()
        self.scrollView = scrollView
        last = nil
        // The pane's scroll area by name, as the stack's was: its bar and page parts are what a test drives.
        scrollView.setAccessibilityIdentifier("session.conversation")
        anchor.attach(scrollView)
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
        anchor.detach()
        verdict?.cancel()
        ask?.cancel()
    }

    private func arrive() {
        if isAway { noteFollow("BACK") }
        handAway = false
        verdict?.cancel()
        verdict = Task { [weak self] in
            guard !Task.isCancelled else { return }
            self?.following = true
        }
    }

    /// `repin` is for a reader who was following when the last row grew: they never left, so when the debounce runs
    /// out with the end still off screen it is put back rather than taken for a departure.
    private func depart(repin: (@MainActor () -> Void)? = nil) {
        verdict?.cancel()
        verdict = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled, let self else { return }
            if let repin {
                repin()
            } else if following {
                noteFollow("AWAY")
                following = false
            }
        }
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
        defer { self.last = (y, height, pane) }
        // The first reading, made as the column opens, has nothing to compare with; a column already shorter than the
        // pane pages back on its own.
        guard let last else {
            if PageAnchor.isShort(scrollView) { later(anchor.pageBackOnItsOwn, if: PageAnchor.isShort) }
            return
        }
        let dy = y - last.y, grown = height - last.height, resized = pane != last.pane
        guard abs(dy) > 0.5 || grown != 0 || resized else { return }
        readTop(scrollView, dy: dy, grown: grown, resized: resized)
        let gap = height - (y + pane.height)
        if height <= pane.height {
            arrive()
        } else if resized, !hand, !isAway, abs(gap) > 0.5 {
            anchor.keepLatest()
        } else if gap <= 32 {
            arrive()
        } else if hand {
            if dy < -0.5 { leave() } else { depart() }
        } else if dy < -0.5, grown == 0, !resized {
            leave()
        } else if isAway {
            depart()
        } else {
            depart { [weak self] in self?.repin() }
        }
    }

    /// Back to the end after the last row grew under a reader who was following; nothing if it is there already.
    private func repin() {
        guard let scrollView, let document = scrollView.documentView else { return }
        let end = document.frame.height - scrollView.contentView.bounds.height
        guard end > scrollView.contentView.bounds.origin.y else { return }
        PageAnchor.scroll(scrollView, to: end, as: "REPIN")
    }

    /// The top, as the iPhone's `HeadObserver` reads it. A column shorter than the pane pages back on its own; a hand
    /// within a pane's height of the top asks for the page above, and so does a move up that reaches the top while
    /// nothing changes size.
    private func readTop(_ scrollView: NSScrollView, dy: CGFloat, grown: CGFloat, resized: Bool) {
        if PageAnchor.isShort(scrollView) {
            if hand {
                readerAsks(if: PageAnchor.isShort)
            } else {
                later(anchor.pageBackOnItsOwn, if: PageAnchor.isShort)
            }
            return
        }
        guard PageAnchor.isNearTop(scrollView) else {
            arrived = false
            return
        }
        let reachesTop = dy < -0.5 && grown == 0 && !resized && scrollView.contentView.bounds.origin.y <= 0.5
        guard hand || reachesTop else { return }
        readerAsks(if: PageAnchor.isNearTop)
    }

    /// The reader's own ask: once it goes out, they have come to the top.
    private func readerAsks(if still: @escaping @MainActor (NSScrollView) -> Bool) {
        later({ [weak self] in
            self?.arrived = true
            self?.reachedTop()
        }, if: still)
    }

    /// Deferred past this turn and checked again when it runs, so nothing is asked from inside layout; dropped while a
    /// row is held in place, when the origin moves with the table's update, not the reader.
    private func later(_ action: @escaping @MainActor () -> Void, if still: @escaping @MainActor (NSScrollView) -> Bool) {
        ask?.cancel()
        ask = Task { [weak self] in
            guard !Task.isCancelled, let self, !self.anchor.isPlacing, let scrollView = self.scrollView,
                still(scrollView)
            else { return }
            action()
        }
    }
}

/// Keeps the reader's place while the rows above them are replaced (an earlier page drawn above, or the window set
/// afresh): the iPhone's `PageAnchor` (SessionDetailView.swift) on the List's table. The table keeps its origin when
/// rows are added above the ones on screen, so on its own a page moves what the reader was looking at down by the
/// page's height. Before Kit publishes the change this notes the first row on screen that the change keeps and where
/// its top is, or, for a reader following, the newest rows; once the table shows the rows published, it puts that row's
/// top back there and keeps it there while the rows around it are measured, until the main actor next turns. The move
/// is made from the layout of a view of its own inside the table, which runs after the table's, and never by
/// `scrollTo`, which can only put a row's top on an edge. While a row is held the follow does not scroll: the
/// placement is the one move.
@MainActor
final class PageAnchor {
    private enum Target {
        /// This row (its place, `TimelineItem.places`), its top this far below the pane's.
        case row(place: String, top: CGFloat)
        /// The newest rows.
        case latest
    }

    private struct Hold {
        let target: Target
        /// Which hold this is, so letting go of one never lets go of the next.
        let number: Int
        /// The row's top in the table once placed, and where it was put below the pane's top: later passes give back
        /// only what layout moved it by.
        var placedMinY: CGFloat?
        var placedTop: CGFloat = 0
    }

    /// The published timeline, which the table's rows are matched against: the Earlier row, one row per item, then
    /// the tail.
    var timeline: () -> [TimelineItem] = { [] }
    /// Asks Kit for the page above without the reader; see `continueIfAtTop`.
    var pageBackOnItsOwn: () -> Void = {}
    private weak var scrollView: NSScrollView?
    private var hold: Hold?
    private var holds = 0
    /// Once the table shows the published timeline, the next page is asked for if the reader is near the top or the
    /// column is short: a page landed, or Kit became able to page.
    private var continuing = false
    private var sizeObserver: NSObjectProtocol?
    private lazy var restorer = Restorer(anchor: self)

    /// A row is held while the rows above it change: until it is let go, the table's update and the placement move the
    /// origin, not the reader.
    var isPlacing: Bool { hold != nil }

    private var table: NSTableView? { scrollView?.documentView as? NSTableView }

    func attach(_ scrollView: NSScrollView) {
        guard let table = scrollView.documentView as? NSTableView else { return }
        self.scrollView = scrollView
        table.addSubview(restorer)
        sizeObserver = NotificationCenter.default.addObserver(
            forName: NSView.frameDidChangeNotification, object: table, queue: nil
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.hold != nil || self.continuing else { return }
                self.restorer.needsLayout = true
            }
        }
    }

    func detach() {
        sizeObserver.map(NotificationCenter.default.removeObserver)
        sizeObserver = nil
        restorer.removeFromSuperview()
        hold = nil
        continuing = false
    }

    /// The pane changed size under a reader at the newest rows: they are put back there from the restorer's layout,
    /// later in the pass that resized the table. A row held for a page or a reset keeps its place.
    func keepLatest() {
        guard hold == nil else { return }
        holds += 1
        hold = Hold(target: .latest, number: holds)
        restorer.needsLayout = true
    }

    /// Kit became able to page: the top is read again once the table shows the timeline published now.
    func recheck() {
        continuing = true
        restorer.needsLayout = true
    }

    /// The rows above the reader are about to change from `shown` to `next`. A reader away keeps the first row on
    /// screen that `next` keeps; a reader following, or one a reset keeps none of whose rows, goes to the newest rows.
    func willChange(shown: [TimelineItem], next: [TimelineItem], change: SessionState.WindowChange, away: Bool) {
        hold = nil
        guard let scrollView, let table, scrollView.window != nil, table.numberOfRows == shown.count + 2 else { return }
        guard shown.map(\.id) != next.map(\.id) else {
            // Nothing is drawn, so there is no layout to wait for.
            if change == .page { Task { [weak self] in self?.continueIfAtTop() } }
            return
        }
        if change == .page { continuing = true }
        restorer.needsLayout = true
        holds += 1
        guard away else {
            hold = Hold(target: .latest, number: holds)
            return
        }
        let visible = table.visibleRect
        let rows = table.rows(in: visible)
        let shownPlaces = TimelineItem.places(shown)
        let nextPlaces = Set(TimelineItem.places(next))
        // Table rows are the Earlier row, then one per item: an item's row is its index plus one.
        for row in rows.lowerBound..<rows.upperBound where shownPlaces.indices.contains(row - 1) {
            let place = shownPlaces[row - 1]
            guard nextPlaces.contains(place) else { continue }
            hold = Hold(target: .row(place: place, top: table.rect(ofRow: row).minY - visible.minY), number: holds)
            return
        }
        if change == .reset { hold = Hold(target: .latest, number: holds) }
    }

    /// From the restorer's layout, after the table's, in each pass while there is something to do. It acts once the
    /// table shows the rows of the timeline published now, which may already hold a later row than the change brought.
    fileprivate func restore() {
        guard let scrollView, let table else { return }
        let timeline = timeline()
        guard table.numberOfRows == timeline.count + 2 else { return }
        if var hold {
            let highest = max(0, table.frame.height - scrollView.contentView.bounds.height)
            switch hold.target {
            case .latest:
                Self.scroll(scrollView, to: highest, as: "PLACE latest")
                self.hold = nil
            case .row(let place, let top):
                guard let index = TimelineItem.places(timeline).firstIndex(of: place) else {
                    self.hold = nil
                    break
                }
                let frame = table.rect(ofRow: index + 1)
                // Never above the pane: a row measured shorter than it was would otherwise put the reader past it.
                let rowTop = top + frame.height > 0 ? top : 0
                let origin = scrollView.contentView.bounds.origin.y
                if let placedMinY = hold.placedMinY {
                    let moved = frame.minY - placedMinY - (rowTop - hold.placedTop)
                    Self.scroll(scrollView, to: min(max(origin + moved, 0), highest), as: "PLACE again")
                } else {
                    Self.scroll(scrollView, to: min(max(frame.minY - rowTop, 0), highest), as: "PLACE row")
                    let number = hold.number
                    // Until the main actor next turns: the passes that measure the rows it brings on screen run first.
                    Task { [weak self] in
                        if self?.hold?.number == number { self?.hold = nil }
                    }
                }
                hold.placedMinY = frame.minY
                hold.placedTop = rowTop
                self.hold = hold
            }
        }
        if continuing {
            continuing = false
            Task { [weak self] in self?.continueIfAtTop() }
        }
    }

    /// A page that left the reader near the top, or a column still shorter than the pane, gets the next page without
    /// the reader moving; so does a column Kit became able to page with the reader at its top.
    private func continueIfAtTop() {
        guard let scrollView, scrollView.window != nil, Self.isShort(scrollView) || Self.isNearTop(scrollView)
        else { return }
        pageBackOnItsOwn()
    }

    /// The column is shorter than the pane.
    static func isShort(_ scrollView: NSScrollView) -> Bool {
        (scrollView.documentView?.frame.height ?? 0) < scrollView.contentView.bounds.height
    }

    /// Within a pane's height of the top: the page is asked for before the reader runs out of rows.
    static func isNearTop(_ scrollView: NSScrollView) -> Bool {
        scrollView.contentView.bounds.origin.y <= scrollView.contentView.bounds.height
    }

    /// Puts the pane's top at `y` in the table, if that moves it.
    static func scroll(_ scrollView: NSScrollView, to y: CGFloat, as move: String) {
        let clip = scrollView.contentView
        guard abs(clip.bounds.origin.y - y) > 0.5 else { return }
        noteFollow(move)
        clip.scroll(to: NSPoint(x: clip.bounds.origin.x, y: y))
        scrollView.reflectScrolledClipView(clip)
    }

    /// A zero-size view inside the table whose layout runs after the table's own, in the same pass.
    private final class Restorer: NSView {
        unowned let anchor: PageAnchor

        init(anchor: PageAnchor) {
            self.anchor = anchor
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not from a nib") }

        override func layout() {
            super.layout()
            anchor.restore()
        }
    }
}

/// Rides the tail row, inside the table, to find the scroll view for `Tail`. The observations are `Tail`'s, not this
/// row's: the table reuses the row's view when the reader is far from it.
struct TailProbe: NSViewRepresentable {
    let tail: Tail

    func makeNSView(context: Context) -> Probe { Probe(tail: tail) }
    func updateNSView(_ probe: Probe, context: Context) {}

    final class Probe: NSView {
        let tail: Tail

        init(tail: Tail) {
            self.tail = tail
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not from a nib") }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window != nil, let scrollView = enclosingScrollView { tail.attach(scrollView) }
        }
    }
}

extension EnvironmentValues {
    /// The open conversation's `Tail`, for a row that opens more of itself on a click: opening is choosing to read,
    /// and must reach the follow before the row grows. Nil outside the column (a request at the pane's foot).
    @Entry var conversationTail: Tail?
}

/// `-CC_FOLLOW_LOG YES` (Debug builds): one line on stderr for each move the follow makes (its scroll to the end, a
/// re-pin, a held row placed) and each turn of its verdict (the reader left, the departure settled, the reader came
/// back), so a test reads them from outside the app. Nothing in a Release build.
func noteFollow(_ move: @autoclosure () -> String) {
    #if DEBUG
        guard UserDefaults.standard.bool(forKey: "CC_FOLLOW_LOG") else { return }
        let line = "follow \(String(format: "%.3f", Date().timeIntervalSince1970)) \(move())\n"
        FileHandle.standardError.write(Data(line.utf8))
    #endif
}
