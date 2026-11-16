import CoreGraphics

/// The Mac conversation column's geometry, kept by the app: its rows top to bottom (the Earlier row, the timeline's
/// rows by `TimelineItem.places`, the tail), each row's height, the pane's height and where its top stands in the
/// column (`origin`, flipped). Every height and every change of rows goes through `apply`, which keeps the reader's place
/// in the same call: a reader at the end stays at the end, unless they left it themselves; a reader away keeps the
/// first timeline row on screen that the change keeps exactly where its top was; with none kept, the end. The reader's
/// own scroll is the other way the origin moves (`scrolled`). Nothing here reads the screen: the table is told what
/// this decided.
public struct MacColumnLayout: Sendable, Equatable {
    public struct Row: Sendable, Hashable {
        public let place: String
        public let height: CGFloat
        /// A timeline row, which can keep the reader's place; the Earlier row and the tail never do.
        public let isTimeline: Bool

        public init(place: String, height: CGFloat, isTimeline: Bool = true) {
            self.place = place
            self.height = height
            self.isTimeline = isTimeline
        }
    }

    /// A change to the column: its rows set anew (inserted, removed, reordered), rows' new heights by place, or the
    /// pane's new height. A row the change brings takes the height it carries.
    public struct Change: Sendable {
        public var rows: [Row]?
        public var heights: [String: CGFloat]
        public var pane: CGFloat?

        public init(rows: [Row]? = nil, heights: [String: CGFloat] = [:], pane: CGFloat? = nil) {
            self.rows = rows
            self.heights = heights
            self.pane = pane
        }
    }

    /// What `apply` moved: the origin and the column's height. The follow takes them out of its next reading, so it
    /// reads only the reader's own move.
    public struct Moved: Sendable, Equatable {
        public let dy: CGFloat
        public let grown: CGFloat

        public init(dy: CGFloat, grown: CGFloat) {
            self.dy = dy
            self.grown = grown
        }
    }

    /// Within this of the end, the reader is at the end.
    public static let endSlack: CGFloat = 32

    public private(set) var rows: [Row]
    public private(set) var origin: CGFloat
    public private(set) var pane: CGFloat

    public init(rows: [Row], origin: CGFloat, pane: CGFloat) {
        self.rows = rows
        self.pane = pane
        self.origin = 0
        self.origin = clamped(origin)
    }

    /// The column's height: its rows' heights summed.
    public var height: CGFloat { rows.reduce(0) { $0 + $1.height } }

    public var isAtEnd: Bool { height - (origin + pane) <= Self.endSlack }

    public func minY(of place: String) -> CGFloat? {
        var y: CGFloat = 0
        for row in rows {
            if row.place == place { return y }
            y += row.height
        }
        return nil
    }

    /// The reader moved the column themselves (a trackpad, keys, the scroller, VoiceOver).
    public mutating func scrolled(to y: CGFloat) {
        origin = clamped(y)
    }

    /// Makes `change` and keeps the reader's place in the same step; returns what moved. `readerAway`: the reader left
    /// the end themselves (opened a row to read it), so their row is kept even within the end's slack.
    @discardableResult
    public mutating func apply(_ change: Change, readerAway: Bool = false) -> Moved {
        let before = (origin: origin, height: height)
        let anchor = isAtEnd && !readerAway ? nil : firstOnScreen(keptBy: change.rows.map { Set($0.map(\.place)) })
        var next = change.rows ?? rows
        if !change.heights.isEmpty {
            next = next.map { row in
                change.heights[row.place].map { Row(place: row.place, height: $0, isTimeline: row.isTimeline) } ?? row
            }
        }
        rows = next
        if let newPane = change.pane { pane = newPane }
        if let anchor, let newMinY = minY(of: anchor.place) {
            origin = clamped(before.origin + newMinY - anchor.minY)
        } else {
            origin = clamped(height - pane)
        }
        return Moved(dy: origin - before.origin, grown: height - before.height)
    }

    /// The first timeline row on screen (its foot below the pane's top, its top above the pane's foot) whose place
    /// `kept` keeps; any, when the change keeps every row.
    private func firstOnScreen(keptBy kept: Set<String>?) -> (place: String, minY: CGFloat)? {
        var y: CGFloat = 0
        for row in rows {
            defer { y += row.height }
            guard row.isTimeline, y + row.height > origin, y < origin + pane else { continue }
            if kept?.contains(row.place) ?? true { return (row.place, y) }
        }
        return nil
    }

    private func clamped(_ y: CGFloat) -> CGFloat {
        min(max(y, 0), max(0, height - pane))
    }
}
