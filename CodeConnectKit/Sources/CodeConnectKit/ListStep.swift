/// One arrow-key step through a list, in the order it shows: from nothing, down lands on the first entry and up on the
/// last; at either end the selection stays.
public enum ListStep {
    public static func step<ID: Equatable>(from current: ID?, in ids: [ID], down: Bool) -> ID? {
        guard let first = ids.first, let last = ids.last else { return current }
        guard let current, let index = ids.firstIndex(of: current) else { return down ? first : last }
        return ids[down ? min(index + 1, ids.count - 1) : max(index - 1, 0)]
    }
}
