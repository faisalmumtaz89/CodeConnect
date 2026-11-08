/// One project on the home screen: every run in one working directory on one
/// computer. The folder as the fleet draws it, so one folder on two computers is
/// two projects and a name the daemon later resolves differently is not.
public struct ProjectKey: Hashable, Sendable {
    public let machineID: String?
    public let folder: String

    public init(machineID: String?, folder: String) {
        self.machineID = machineID
        self.folder = folder
    }

    /// How it is kept on the device: the computer, a unit separator, the folder.
    var stored: String { "\(machineID ?? "")\u{1F}\(folder)" }

    init?(stored: String) {
        let parts = stored.split(separator: "\u{1F}", maxSplits: 1, omittingEmptySubsequences: false)
        guard parts.count == 2 else { return nil }
        machineID = parts[0].isEmpty ? nil : String(parts[0])
        folder = String(parts[1])
    }
}

/// **The owner's order of the projects, and nothing else.** A project's place
/// is where it was put: a status, an event or a card arriving changes what a
/// project shows, never where it is. Only a drag moves one.
///
/// A project the order has never held goes on top, so the newest is first; one
/// that has gone is skipped, so the rest close up without changing places, and
/// keeps its place for when it comes back.
public enum ProjectOrder {
    /// `present` in `saved`'s order, with those `saved` does not hold on top,
    /// in the order they are given.
    public static func arrange(_ present: [ProjectKey], saved: [ProjectKey]) -> [ProjectKey] {
        let held = Set(saved)
        let here = Set(present)
        return present.filter { !held.contains($0) } + saved.filter { here.contains($0) }
    }

    /// `saved` holding every one of `present`, the new ones on top as
    /// `arrange` shows them, so each keeps the place it appeared in.
    public static func recording(_ present: [ProjectKey], into saved: [ProjectKey]) -> [ProjectKey] {
        let held = Set(saved)
        let new = present.filter { !held.contains($0) }
        return new.isEmpty ? saved : new + saved
    }

    /// `saved` with `key` put where `target` is in `shown`, the order on
    /// screen: before it when moving up, after it when moving down. Projects
    /// not on screen keep their places.
    public static func moving(
        _ key: ProjectKey, to target: ProjectKey, shown: [ProjectKey], saved: [ProjectKey]
    ) -> [ProjectKey] {
        guard key != target, let from = shown.firstIndex(of: key), let to = shown.firstIndex(of: target)
        else { return saved }
        var order = recording(shown, into: saved).filter { $0 != key }
        guard let anchor = order.firstIndex(of: target) else { return saved }
        order.insert(key, at: from > to ? anchor : anchor + 1)
        return order
    }
}
