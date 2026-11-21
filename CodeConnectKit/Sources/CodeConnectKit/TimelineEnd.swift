/// Where a timeline ends: how many rows it has and the last event it was
/// drawn from. "N new" counts the rows drawn from events after that one at the
/// tail — a timeline's rows are in event order, except a Codex item the Mac read
/// back, which is drawn above inside its turn and is not new at the tail — so an
/// earlier page drawn above adds nothing: not where the old last row folded into a
/// call the page brought, nor where it gives a later event the context that shows it.
public struct TimelineEnd: Equatable {
    public let count: Int
    public let drawnThrough: UInt64

    public init(_ timeline: [TimelineItem], drawnThrough: UInt64) {
        count = timeline.count
        self.drawnThrough = drawnThrough
    }

    /// The rows of `timeline` from events after the last one `old` was drawn from.
    public static func rowsAfter(_ old: TimelineEnd, in timeline: [TimelineItem]) -> Int {
        timeline.reversed().prefix { $0.seq > old.drawnThrough }.count
    }
}
