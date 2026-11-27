import AppKit
import CodeConnectKit
import SwiftUI

/// The conversation column on an `NSTableView` the app drives. The rows (the Earlier row, one per timeline item by its
/// place, the tail), every row's height and where the pane stands are the app's, in `MacColumnLayout`; the table draws
/// what that holds and decides nothing. Every change of rows, heights or pane is one `apply`: the layout keeps the
/// reader's place, the table is told in the same call with no animation, and the origin is set. Nothing else moves the
/// origin but the reader. Each row is the existing SwiftUI row view in a hosting view, made when the table draws the row
/// and let go when the table does; what a reader opened in a row is kept by the row's place (`RowOpenings`), so a row
/// made again opens as it was. A row far from the screen gets no view: an accessibility client walking the table finds
/// it empty, as a SwiftUI list gives nothing for a row it has not loaded, and nothing is built for it.
struct ConversationTable: NSViewRepresentable {
    let model: AppModel
    let key: String
    let items: [TimelineItem]
    /// The last event `items` were drawn from, from which "N new" counts (`TimelineEnd`).
    let drawnThrough: UInt64
    /// The run's missed-events notice, drawn among the items where `GapNotice.index(in:)` places it.
    let gap: GapNotice?
    let tail: Tail
    let latest: Latest

    func makeCoordinator() -> ConversationColumn {
        ConversationColumn(model: model, key: key, tail: tail, latest: latest)
    }

    func makeNSView(context: Context) -> NSScrollView { context.coordinator.scroll }

    func updateNSView(_ view: NSScrollView, context: Context) {
        noteFollow("UPDATE")
        context.coordinator.show(items, drawnThrough: drawnThrough, gap: gap, model: model)
    }

    static func dismantleNSView(_ view: NSScrollView, coordinator: ConversationColumn) { coordinator.detach() }
}

@MainActor
final class ConversationColumn: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    private struct Entry {
        let place: String
        let item: TimelineItem?
        /// The missed-events notice, on its own entry.
        var notice: GapNotice? = nil
    }

    private static let earlier = "column.earlier", gap = "column.gap", end = "column.tail"
    /// A row's margins: the column's sides and half the gap between rows above and below it.
    private static let insets = EdgeInsets(top: 7, leading: 28, bottom: 7, trailing: 28)

    let scroll = NSScrollView()
    private let table = ColumnTableView()
    private let ring = ColumnRing()
    private var model: AppModel
    private let key: String
    private let tail: Tail
    /// The way back to the end: assigned from `away` and `arrived` when they change, never read here, so nothing this
    /// column does during a view update depends on what it writes.
    private let latest: Latest
    private var away = false
    /// Rows that arrived while the reader was away from the end.
    private var arrived = 0
    /// What `latest.count` was last given.
    private var shownArrived = 0
    /// Where the timeline ended at the last `show`.
    private var end: TimelineEnd?
    private var entries: [Entry] = []
    private var layout = MacColumnLayout(rows: [], origin: 0, pane: 0)
    /// Rows whose height was measured at the present width; the rest hold an estimate until they come near the screen.
    private var measured: Set<String> = []
    /// The rows the table holds, by place.
    private var cells: [String: RowCell] = [:]
    private var openings: [String: RowOpenings] = [:]
    private var width: CGFloat = 0
    private var applying = false
    /// Heights the rows near the screen reported for themselves, applied together on the next turn.
    private var reports: [String: CGFloat] = [:]
    private var observers: [NSObjectProtocol] = []
    private let sizer = NSHostingController(rootView: AnyView(EmptyView()))

    init(model: AppModel, key: String, tail: Tail, latest: Latest) {
        self.model = model
        self.key = key
        self.tail = tail
        self.latest = latest
        super.init()
        // First: the scroll view's background setting lives on its clip view, so a clip swapped in later drew its own.
        scroll.contentView = PageClip()
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.automaticallyAdjustsContentInsets = false
        scroll.contentInsets = NSEdgeInsetsZero
        let column = NSTableColumn(identifier: .init("row"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.headerView = nil
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.usesAutomaticRowHeights = false
        table.intercellSpacing = .zero
        table.style = .plain
        table.selectionHighlightStyle = .none
        table.backgroundColor = .clear
        table.dataSource = self
        table.delegate = self
        scroll.documentView = table
        // One stop for the keyboard: the column itself, which scrolls by its keys (`ColumnTableView`); its scroll bar
        // is never a stop.
        scroll.verticalScroller?.refusesFirstResponder = true
        ring.isHidden = true
        // Under the scroll bar, so its knob stays whole over the ring, as it does over a system ring.
        scroll.addSubview(ring, positioned: .below, relativeTo: scroll.verticalScroller)
        table.ring = ring
        tail.attach(scroll)
        tail.readerLeaves = { [weak self] in self?.left() }
        tail.isFollowing = { [weak self] in self?.layout.following ?? true }
        latest.jump = { [weak self] in self?.jumpToEnd() }
        let clip = scroll.contentView
        clip.postsBoundsChangedNotifications = true
        clip.postsFrameChangedNotifications = true
        let center = NotificationCenter.default
        observers = [
            center.addObserver(forName: NSView.boundsDidChangeNotification, object: clip, queue: nil) { [weak self] _ in
                MainActor.assumeIsolated { self?.scrolled() }
            },
            center.addObserver(forName: NSView.frameDidChangeNotification, object: clip, queue: nil) { [weak self] _ in
                // The ring takes the pane's size here: a scroll view's tiling sizes its clip view and scrollers only,
                // and leaves another subview at the size it joined with, whether by autoresizing or by constraints.
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.ring.frame = self.scroll.bounds
                    self.resized()
                }
            },
        ]
    }

    func detach() {
        observers.forEach(NotificationCenter.default.removeObserver)
        observers = []
        tail.detach()
    }

    // MARK: The rows

    /// Several requests waiting: the conversation says so once, by the first; the card in the reply box's place counts
    /// the rest ("1 more waiting").
    private static func oneWaiting(_ items: [TimelineItem]) -> [TimelineItem] {
        var said = false
        return items.filter { item in
            guard case .approval(let approval) = item.content, approval.isPending else { return true }
            defer { said = true }
            return !said
        }
    }

    /// The published timeline, and its missed-events notice after the row the Kit places it by. New rows and rows
    /// whose item or notice changed are measured (once the column has a width), the rest keep their heights; then one
    /// `apply`: a page above, rows below, rows changed in place, or the rows set afresh.
    func show(_ items: [TimelineItem], drawnThrough: UInt64, gap: GapNotice?, model: AppModel) {
        self.model = model
        let items = Self.oneWaiting(items)
        // Rows that arrived while the reader was away from the end, as the iPhone counts them.
        if let end, !layout.following { arrived += TimelineEnd.rowsAfter(end, in: items) }
        end = TimelineEnd(items, drawnThrough: drawnThrough)
        let places = TimelineItem.places(items)
        var timeline = zip(places, items).map { Entry(place: $0, item: $1) }
        if let gap, !items.isEmpty { timeline.insert(Entry(place: Self.gap, item: nil, notice: gap), at: gap.index(in: items) + 1) }
        let next = [Entry(place: Self.earlier, item: nil)] + timeline + [Entry(place: Self.end, item: nil)]
        var was: [String: (item: TimelineItem?, notice: GapNotice?, height: CGFloat)] = [:]
        for (entry, row) in zip(entries, layout.rows) { was[entry.place] = (entry.item, entry.notice, row.height) }
        var changed: [String] = []
        let rows = next.map { entry -> MacColumnLayout.Row in
            let kept = was[entry.place]
            if let kept, kept.item == entry.item, kept.notice == entry.notice {
                return .init(place: entry.place, height: kept.height, isTimeline: entry.item != nil)
            }
            if kept != nil { changed.append(entry.place) }
            return .init(place: entry.place, height: height(of: entry, fresh: !entries.isEmpty), isTimeline: entry.item != nil)
        }
        guard rows != layout.rows || changed.count > 0 || entries.isEmpty else { return }
        for place in changed { if let entry = next.first(where: { $0.place == place }) { cells[place]?.show(view(entry)) } }
        let kept = Set(next.map(\.place))
        cells = cells.filter { kept.contains($0.key) }
        openings = openings.filter { kept.contains($0.key) }
        measured.formIntersection(kept)
        let old = entries.map(\.place), new = next.map(\.place)
        if old.isEmpty {
            apply("open", .init(rows: rows), entries: next) { $0.reloadData() }
            #if DEBUG
                // A render's `-CC_CONVERSATION_UP <points>`: a moment after opening, the reader scrolls up that far.
                if let up = RenderWindow.conversationUp {
                    Task { [weak self] in
                        try? await Task.sleep(for: .seconds(1))
                        guard let self else { return }
                        let clip = self.scroll.contentView
                        clip.scroll(to: NSPoint(
                            x: clip.bounds.origin.x, y: max(0, self.table.frame.height - clip.bounds.height - up)))
                        self.scroll.reflectScrolledClipView(clip)
                    }
                }
            #endif
        } else if new.count > old.count, Array(new.suffix(old.count - 1)) == Array(old.dropFirst()) {
            let count = new.count - old.count
            apply("page \(count)", .init(rows: rows), entries: next) {
                $0.insertRows(at: IndexSet(1...count), withAnimation: [])
            }
            // A page that left the reader near the top gets the next one, asked past this view update.
            if Tail.isNearTop(scroll) || Tail.isShort(scroll) { Task { [tail] in tail.pageBackOnItsOwn() } }
        } else if new.count > old.count, Array(new.prefix(old.count - 1)) == Array(old.dropLast()) {
            let first = old.count - 1
            apply("append \(new.count - old.count)", .init(rows: rows), entries: next) {
                $0.insertRows(at: IndexSet(first..<first + new.count - old.count), withAnimation: [])
            }
        } else if new == old {
            apply("content \(changed.count)", .init(rows: rows), entries: next) { _ in }
        } else {
            apply("reload", .init(rows: rows), entries: next) { $0.reloadData() }
        }
        measureNearScreen()
    }

    // MARK: Heights, the app's own

    /// The row's view, on the conversation's column (`Theme.conversationColumn`): the table spans the pane, its scroller at the
    /// pane's edge, and the row is measured as it is drawn.
    private func view(_ entry: Entry) -> AnyView {
        AnyView(content(entry).onConversationColumn())
    }

    private func content(_ entry: Entry) -> AnyView {
        guard let item = entry.item else {
            if entry.place == Self.earlier { return AnyView(Earlier(model: model, key: key).padding(Self.insets)) }
            if let notice = entry.notice {
                return AnyView(
                    NoticeLine(gap: notice) { [model, key] in model.dismissGap(key) }
                        .accessibilityElement(children: .contain)
                        .accessibilityIdentifier("conversation.gap")
                        .padding(Self.insets))
            }
            return AnyView(Color.clear.frame(height: 1))
        }
        let opened = openings[entry.place] ?? RowOpenings()
        openings[entry.place] = opened
        return AnyView(
            ConversationRow(item: item, model: model).padding(Self.insets).environment(\.conversationTail, tail)
                .environment(opened))
    }

    /// The row's height at the column's width: measured when the column has one and `fresh` (a row the published
    /// timeline brought or changed), else an estimate by kind until it comes near the screen.
    private func height(of entry: Entry, fresh: Bool) -> CGFloat {
        if fresh, width > 10 { return measure(entry) }
        guard let item = entry.item else { return entry.place == Self.end ? 1 : 30 }
        switch item.content {
        case .tool, .notice: return 43
        default: return 60
        }
    }

    private func measure(_ entry: Entry) -> CGFloat {
        sizer.rootView = AnyView(ThemeRoot(content: view(entry)))
        measured.insert(entry.place)
        return ceil(sizer.sizeThatFits(in: CGSize(width: width, height: .greatestFiniteMagnitude)).height)
    }

    /// Rows still on estimates within a pane of the screen are measured, then one `apply`.
    private func measureNearScreen() {
        guard width > 10 else { return }
        let near = nearScreen().filter { !measured.contains(layout.rows[$0].place) }
        guard !near.isEmpty else { return }
        var heights: [String: CGFloat] = [:]
        for index in near { heights[entries[index].place] = measure(entries[index]) }
        apply("measure \(near.count)", .init(heights: heights))
    }

    /// The rows within a pane above or below the screen, in one pass down the column.
    private func nearScreen() -> [Int] {
        let low = layout.origin - layout.pane, high = layout.origin + 2 * layout.pane
        var near: [Int] = [], top: CGFloat = 0
        for (index, row) in layout.rows.enumerated() {
            if top >= high { break }
            if top + row.height > low { near.append(index) }
            top += row.height
        }
        return near
    }

    /// A row near the screen reports its own height (its content, an expansion, a banner's age); one the table prepared
    /// further out is measured when it comes near. Applied on the next turn, outside any table call; a row the reader
    /// opened to read (the column stopped following: `left()`) grows below its top.
    private func reported(_ place: String, _ height: CGFloat) {
        guard let index = entries.firstIndex(where: { $0.place == place }), nearScreen().contains(index),
            abs(layout.rows[index].height - ceil(height)) > 0.5
        else { return }
        let first = reports.isEmpty
        reports[place] = ceil(height)
        measured.insert(place)
        guard first else { return }
        RunLoop.main.perform { [weak self] in
            MainActor.assumeIsolated {
                guard let self else { return }
                let heights = self.reports
                self.reports = [:]
                self.apply("report \(heights.count)", .init(heights: heights))
            }
        }
    }

    // MARK: The one path that changes geometry

    /// The layout decides (the reader's place kept), then the table is told in the same call, with no animation, and
    /// the origin set. Never re-entered; what it moved is handed to `Tail`.
    private func apply(
        _ why: String, _ change: MacColumnLayout.Change, entries next: [Entry]? = nil,
        table update: (NSTableView) -> Void = { _ in }
    ) {
        guard !applying else { return }
        applying = true
        defer { applying = false }
        let before = (origin: layout.origin, height: layout.height, following: layout.following)
        let moved = layout.apply(change)
        if layout.following != before.following { noteFollow("FOLLOW \(layout.following ? 1 : 0) \(why)") }
        if let next { entries = next }
        NSAnimationContext.beginGrouping()
        NSAnimationContext.current.duration = 0
        update(table)
        table.noteHeightOfRows(withIndexesChanged: IndexSet(integersIn: 0..<entries.count))
        NSAnimationContext.endGrouping()
        // The table keeps its rows' positions in a cache it rebuilds lazily: right after an update the frame it tiles
        // from its last row can be a few points short until the rows are read in order. They are read now, so the frame
        // is this change's.
        for row in 0..<table.numberOfRows { _ = table.rect(ofRow: row) }
        table.tile()
        let clip = scroll.contentView
        if abs(clip.bounds.origin.y - layout.origin) > 0.25 {
            clip.setBoundsOrigin(NSPoint(x: clip.bounds.origin.x, y: layout.origin))
            scroll.reflectScrolledClipView(clip)
        }
        tail.applied(moved)
        noteLatest()
        noteFollow(String(format: "APPLY %@ y %.3f -> %.3f h %.3f -> %.3f frame %.3f", why, before.origin,
            layout.origin, before.height, layout.height, table.frame.height))
    }

    /// The reader moved the column; rows coming near the screen are measured.
    private func scrolled() {
        guard !applying else { return }
        let was = layout.following
        layout.scrolled(to: scroll.contentView.bounds.origin.y)
        if layout.following != was {
            noteFollow(String(format: "FOLLOW %d scroll y %.3f", layout.following ? 1 : 0, layout.origin))
        }
        noteLatest()
        measureNearScreen()
    }

    /// The reader opened a row to read it: the column stops following before the row grows.
    private func left() {
        if layout.following { noteFollow("FOLLOW 0 opened a row") }
        layout.left()
        noteLatest()
    }

    /// The way back to the end shows while the reader is away from it and not already there; back at the end, nothing
    /// is new.
    private func noteLatest() {
        let nowAway = !layout.following && !layout.isAtEnd
        if layout.following { arrived = 0 }
        if nowAway != away {
            away = nowAway
            latest.away = nowAway
        }
        if arrived != shownArrived {
            shownArrived = arrived
            latest.count = arrived
        }
    }

    /// To the end, as End takes the reader there: the column follows again from there.
    private func jumpToEnd() {
        let clip = scroll.contentView
        clip.scroll(to: NSPoint(x: clip.bounds.origin.x, y: max(0, table.frame.height - clip.bounds.height)))
        scroll.reflectScrolledClipView(clip)
    }

    /// The pane changed size. A new width measures the rows near the screen at it (the rest are measured as they
    /// come near); either way one `apply`, which keeps a following reader at the end.
    private func resized() {
        guard !applying else { return }
        let bounds = scroll.contentView.bounds
        guard abs(bounds.width - width) > 0.5 else {
            if abs(bounds.height - layout.pane) > 0.25 { apply("pane", .init(pane: bounds.height)) }
            return
        }
        width = bounds.width
        measured = []
        var heights: [String: CGFloat] = [:]
        for index in nearScreen() { heights[entries[index].place] = measure(entries[index]) }
        apply("width", .init(heights: heights, pane: bounds.height))
        measureNearScreen()
    }

    // MARK: The table

    func numberOfRows(in tableView: NSTableView) -> Int { entries.count }

    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
        guard row < layout.rows.count else { return 1 }
        // A conversation shorter than the pane sits at its foot, just above the reply box: the first row (the place
        // earlier events load into) takes the room above it, so the column fits the pane exactly and nothing scrolls.
        return layout.rows[row].height + (row == 0 ? max(0, layout.pane - layout.height) : 0)
    }

    /// A row the table draws or prepares to, or one within a pane of the screen, gets its view; a row asked for anywhere
    /// else is an accessibility client walking the table, and gets none.
    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let entry = entries[row]
        if let cell = cells[entry.place] { return cell }
        guard isNear(row) else { return nil }
        let cell = RowCell(place: entry.place) { [weak self] height in self?.reported(entry.place, height) }
        cell.show(view(entry))
        cells[entry.place] = cell
        return cell
    }

    /// The table let a row go: its view goes with it.
    func tableView(_ tableView: NSTableView, didRemove rowView: NSTableRowView, forRow row: Int) {
        guard let cell = rowView.view(atColumn: 0) as? RowCell, cells[cell.place] === cell else { return }
        cells[cell.place] = nil
    }

    /// A row the table took to draw without a view (it asked before the place it draws was known) is asked for again on
    /// the next turn. Never the ordinary path: the follow log counts each.
    func tableView(_ tableView: NSTableView, didAdd rowView: NSTableRowView, forRow row: Int) {
        guard rowView.view(atColumn: 0) == nil else { return }
        RunLoop.main.perform { [weak self] in
            MainActor.assumeIsolated {
                guard let self, row < self.table.numberOfRows, self.isNear(row) else { return }
                noteFollow("BACKSTOP row \(row)")
                self.table.reloadData(forRowIndexes: [row], columnIndexes: [0])
            }
        }
    }

    private func isNear(_ row: Int) -> Bool {
        var low = layout.origin - layout.pane, high = layout.origin + 2 * layout.pane
        for drawn in [table.visibleRect, table.preparedContentRect] where !drawn.isEmpty {
            low = min(low, drawn.minY)
            high = max(high, drawn.maxY)
        }
        let rect = table.rect(ofRow: row)
        return rect.maxY > low && rect.minY < high
    }

    func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool { false }
}

/// What a reader opened in one row of the column (a tool's output, a card's full input or all its files), kept by the
/// row's place for as long as the conversation is open: a row's view is made again when it comes back near the
/// screen, and the place survives a page that folds a tool's result into its call. A row at the pane's foot has none
/// and keeps its own.
@MainActor @Observable
final class RowOpenings {
    var tool = false
    var input = false
    var allChanges = false
}

/// One row's SwiftUI view, sized by the table (never by its content), its content at the top; it tells the column the
/// height its content wants.
private final class RowCell: NSView {
    let place: String
    private let host = NSHostingView(rootView: AnyView(EmptyView()))
    private let report: (CGFloat) -> Void

    init(place: String, report: @escaping (CGFloat) -> Void) {
        self.place = place
        self.report = report
        super.init(frame: .zero)
        host.sizingOptions = []
        host.translatesAutoresizingMaskIntoConstraints = true
        host.autoresizingMask = [.width, .height]
        host.frame = bounds
        addSubview(host)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    func show(_ content: AnyView) {
        let report = self.report
        host.rootView = AnyView(
            ThemeRoot(content: content)
                .fixedSize(horizontal: false, vertical: true)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { report($0) }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading))
    }
}

/// The column's table: Page Up and Page Down, the arrows and Home and End scroll it, as they do the pane; there is no
/// selection to move. It tells its ring when it takes or gives up the keyboard.
private final class ColumnTableView: NSTableView {
    weak var ring: ColumnRing?

    override func becomeFirstResponder() -> Bool {
        let took = super.becomeFirstResponder()
        if took { ring?.show(holding: true) }
        return took
    }

    override func resignFirstResponder() -> Bool {
        let gave = super.resignFirstResponder()
        if gave { ring?.show(holding: false) }
        return gave
    }

    override func keyDown(with event: NSEvent) {
        guard let scroll = enclosingScrollView else { return super.keyDown(with: event) }
        let clip = scroll.contentView, page = max(clip.bounds.height - scroll.verticalPageScroll, scroll.verticalLineScroll)
        let highest = max(0, frame.height - clip.bounds.height)
        let y: CGFloat
        switch event.specialKey {
        case .pageUp: y = clip.bounds.origin.y - page
        case .pageDown: y = clip.bounds.origin.y + page
        case .upArrow: y = clip.bounds.origin.y - scroll.verticalLineScroll
        case .downArrow: y = clip.bounds.origin.y + scroll.verticalLineScroll
        case .home: y = 0
        case .end: y = highest
        default: return super.keyDown(with: event)
        }
        clip.scroll(to: NSPoint(x: clip.bounds.origin.x, y: min(max(y, 0), highest)))
        scroll.reflectScrolledClipView(clip)
    }
}

/// The column's focus ring, drawn by the app: AppKit draws none for this table, as first responder of the key window it
/// is never asked for its ring's mask, whatever its `focusRingType`, and AppKit's documentation leaves a view's ring to
/// the view. A view over the whole pane, sized with the scroll view, so scrolling and resizing move nothing; shown while
/// the table holds the keyboard and its window is key, in the system's focus colour, which follows the accent colour.
/// It takes no clicks and is no accessibility element.
private final class ColumnRing: NSView {
    // The system ring's measure, read from a button's ring on macOS 26: a solid 4 pt band whose outer corners round by
    // 10 pt. This one stands 2 pt inside the pane, as the pane has no room outside.
    private static let inset: CGFloat = 2
    private static let width: CGFloat = 4
    private static let outerRadius: CGFloat = 10
    private var holding = false
    private var observers: [NSObjectProtocol] = []

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        observers.forEach(NotificationCenter.default.removeObserver)
        observers = []
        guard let window else { return }
        let center = NotificationCenter.default
        observers = [NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification].map { name in
            center.addObserver(forName: name, object: window, queue: nil) { [weak self] _ in
                MainActor.assumeIsolated { self?.show(holding: nil) }
            }
        }
        show(holding: nil)
    }

    /// `holding` is the table's word while it takes or gives up the keyboard, before the window names its first
    /// responder; nil keeps what it last said.
    func show(holding: Bool?) {
        if let holding { self.holding = holding }
        isHidden = !(self.holding && window?.isKeyWindow == true)
    }

    override func draw(_ dirtyRect: NSRect) {
        let edge = Self.inset + Self.width / 2
        let radius = Self.outerRadius - Self.width / 2
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: edge, dy: edge), xRadius: radius, yRadius: radius)
        path.lineWidth = Self.width
        NSColor.keyboardFocusIndicatorColor.setStroke()
        path.stroke()
    }
}

/// The column's clip view. A move the reader makes by the scroller's page part or track, or by an accessibility page
/// press, lands at once, as the keys do, rather than gliding: AppKit's glide moves the bounds by a path of its own,
/// aimed where the column ended when it began, and a landing that pins the column to its end meanwhile is undone by the
/// glide's next frame. A scroll wheel's and a trackpad's moves (the current event a scroll wheel) stay AppKit's own.
private final class PageClip: NSClipView {
    override func scroll(to newOrigin: NSPoint) {
        guard NSApp.currentEvent?.type != .scrollWheel else { return super.scroll(to: newOrigin) }
        setBoundsOrigin(newOrigin)
        enclosingScrollView?.reflectScrolledClipView(self)
    }
}
