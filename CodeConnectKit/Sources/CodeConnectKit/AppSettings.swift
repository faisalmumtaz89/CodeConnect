import Foundation
import Observation

/// User-set preferences that are not secrets and not protocol state.
///
/// Stored properties (not computed `UserDefaults` accessors) so `@Observable`
/// actually sees the writes; the defaults database is written through on each
/// change.
@MainActor
@Observable
public final class AppSettings {
    private enum Key {
        static let terminalFontSize = "codeconnect.terminal.fontSize"
        static let diffFontSize = "codeconnect.diff.fontSize"
        static let projectOrder = "codeconnect.fleet.projectOrder"
    }

    private let defaults: UserDefaults

    /// Terminal type size, in points. Persisted so a pinch survives the tab
    /// being closed.
    public var terminalFontSize: Double {
        didSet { defaults.set(terminalFontSize, forKey: Key.terminalFontSize) }
    }
    public var diffFontSize: Double {
        didSet { defaults.set(diffFontSize, forKey: Key.diffFontSize) }
    }

    /// The owner's order of the home screen's projects (`ProjectOrder`), kept on
    /// this device so a drag survives a relaunch.
    public var projectOrder: [ProjectKey] {
        didSet {
            guard persistsProjectOrder else { return }
            defaults.set(projectOrder.map(\.stored), forKey: Key.projectOrder)
        }
    }

    /// Off for a fixture run or a test host unless `-CC_FIXTURE_KEEP_ORDER YES`
    /// asks for it, so one run's drag cannot reorder the next run's fleet.
    private let persistsProjectOrder: Bool

    public static let terminalFontRange: ClosedRange<Double> = 8...22
    public static let diffFontRange: ClosedRange<Double> = 9...24

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let storedTerminal = defaults.double(forKey: Key.terminalFontSize)
        terminalFontSize =
            Self.terminalFontRange.contains(storedTerminal) ? storedTerminal : 12
        let storedDiff = defaults.double(forKey: Key.diffFontSize)
        diffFontSize = Self.diffFontRange.contains(storedDiff) ? storedDiff : 12
        var persists = true
        #if DEBUG
            let underTest =
                defaults.string(forKey: "CC_FIXTURE") != nil
                || ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
            persists = !underTest || defaults.bool(forKey: "CC_FIXTURE_KEEP_ORDER")
        #endif
        persistsProjectOrder = persists
        projectOrder =
            persists
            ? (defaults.stringArray(forKey: Key.projectOrder) ?? []).compactMap(ProjectKey.init(stored:))
            : []
    }
}
