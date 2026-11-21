/// A menu panel's one lit row, which the pointer and the keys move alike: the pointer entering a row lights it, ↓ and
/// ↑ light the next or the previous row that can be chosen (from none, the first or the last) and stop at the ends, and
/// Return chooses the lit row. Return with none lit, or on a lit row that can no longer be chosen, closes the panel, as
/// a native menu's does. A row that cannot be chosen never lights; a heading or a note is no row.
public struct MenuLight: Equatable, Sendable {
    /// A key the panel takes.
    public enum Key: Sendable {
        case up, down, `return`
    }

    /// What a key did.
    public enum Outcome: Equatable, Sendable {
        /// The light moved, or stayed at an end.
        case moved
        /// Choose this row.
        case choose(Int)
        /// Close the panel.
        case close
    }

    /// The key a Mac key press is, if a panel takes it: ↓, ↑, Return or the keypad's Enter, with none of ⌘, ⌃, ⇧ or ⌥
    /// held. A held one leaves the key to what has the keyboard: ⇧Return and ⌥Return add a line in the reply box.
    public static func key(code: UInt16, command: Bool, control: Bool, shift: Bool, option: Bool) -> Key? {
        guard !(command || control || shift || option) else { return nil }
        switch code {
        case 125: return .down
        case 126: return .up
        case 36, 76: return .return
        default: return nil
        }
    }

    /// The lit row, if one is.
    public private(set) var lit: Int?

    public init(lit: Int? = nil) {
        self.lit = lit
    }

    /// `key` pressed over rows of which `enabled` says which can be chosen.
    public mutating func press(_ key: Key, enabled: [Bool]) -> Outcome {
        let choosable = enabled.indices.filter { enabled[$0] }
        switch key {
        case .down:
            if let next = choosable.first(where: { $0 > lit ?? -1 }) { lit = next }
            return .moved
        case .up:
            if let previous = choosable.last(where: { $0 < lit ?? enabled.count }) { lit = previous }
            return .moved
        case .return:
            guard let lit, choosable.contains(lit) else { return .close }
            return .choose(lit)
        }
    }

    /// The pointer entered `row`: it lights when it can be chosen.
    public mutating func enter(_ row: Int, enabled: [Bool]) {
        if enabled.indices.contains(row), enabled[row] { lit = row }
    }
}
