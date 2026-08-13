import Foundation
import Observation

/// User-set preferences that are not secrets and not protocol state.
///
/// Stored properties (not computed `UserDefaults` accessors) so `@Observable`
/// actually sees the writes; the defaults database is written through on each
/// change.
@MainActor
@Observable
final class AppSettings {
    private enum Key {
        static let terminalFontSize = "codeconnect.terminal.fontSize"
        static let diffFontSize = "codeconnect.diff.fontSize"
    }

    private let defaults: UserDefaults

    /// Terminal type size, in points. Persisted so a pinch survives the tab
    /// being closed.
    var terminalFontSize: Double {
        didSet { defaults.set(terminalFontSize, forKey: Key.terminalFontSize) }
    }
    var diffFontSize: Double {
        didSet { defaults.set(diffFontSize, forKey: Key.diffFontSize) }
    }

    static let terminalFontRange: ClosedRange<Double> = 8...22
    static let diffFontRange: ClosedRange<Double> = 9...24

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let storedTerminal = defaults.double(forKey: Key.terminalFontSize)
        terminalFontSize =
            Self.terminalFontRange.contains(storedTerminal) ? storedTerminal : 12
        let storedDiff = defaults.double(forKey: Key.diffFontSize)
        diffFontSize = Self.diffFontRange.contains(storedDiff) ? storedDiff : 12
    }
}
