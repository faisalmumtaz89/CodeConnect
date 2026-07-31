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
        static let sshUsername = "codeconnect.ssh.username"
        static let sshHost = "codeconnect.ssh.host"
        static let sshPort = "codeconnect.ssh.port"
        static let terminalFontSize = "codeconnect.terminal.fontSize"
        static let diffFontSize = "codeconnect.diff.fontSize"
    }

    private let defaults: UserDefaults

    /// Blank means "use the name derived from a session's working directory".
    var sshUsername: String {
        didSet { defaults.set(sshUsername, forKey: Key.sshUsername) }
    }
    /// Blank means "use the host this app is paired with".
    var sshHostOverride: String {
        didSet { defaults.set(sshHostOverride, forKey: Key.sshHost) }
    }
    var sshPort: Int {
        didSet { defaults.set(sshPort, forKey: Key.sshPort) }
    }
    /// Terminal type size, in points. Persisted so a pinch survives the tab
    /// being closed.
    var terminalFontSize: Double {
        didSet { defaults.set(terminalFontSize, forKey: Key.terminalFontSize) }
    }
    var diffFontSize: Double {
        didSet { defaults.set(diffFontSize, forKey: Key.diffFontSize) }
    }

    static let defaultSSHPort = 22
    static let terminalFontRange: ClosedRange<Double> = 8...22
    static let diffFontRange: ClosedRange<Double> = 9...24

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        sshUsername = defaults.string(forKey: Key.sshUsername) ?? ""
        sshHostOverride = defaults.string(forKey: Key.sshHost) ?? ""
        let storedPort = defaults.integer(forKey: Key.sshPort)
        sshPort = (1...65535).contains(storedPort) ? storedPort : Self.defaultSSHPort
        let storedTerminal = defaults.double(forKey: Key.terminalFontSize)
        terminalFontSize =
            Self.terminalFontRange.contains(storedTerminal) ? storedTerminal : 12
        let storedDiff = defaults.double(forKey: Key.diffFontSize)
        diffFontSize = Self.diffFontRange.contains(storedDiff) ? storedDiff : 12
    }

    /// The Mac account name, inferred from where the agents are actually
    /// working.
    ///
    /// `cwd` comes from the daemon's own session registry, so `/Users/<name>/…`
    /// is direct evidence of the account that owns the tmux server we need to
    /// attach to — better than asking the user to remember it, and better than
    /// guessing from the device name. Anything not under `/Users` yields no
    /// default at all rather than a wrong one.
    nonisolated static func inferredUsername(fromSessionPaths paths: [String]) -> String? {
        for path in paths {
            let parts = path.split(separator: "/", omittingEmptySubsequences: true)
            guard parts.count >= 2, parts[0] == "Users" else { continue }
            let candidate = String(parts[1])
            guard candidate != "Shared", !candidate.isEmpty else { continue }
            return candidate
        }
        return nil
    }

    /// The username the terminal will actually use, or nil when nothing is
    /// known — in which case the UI must ask rather than invent one.
    func effectiveUsername(sessionPaths: [String]) -> String? {
        let typed = sshUsername.trimmingCharacters(in: .whitespacesAndNewlines)
        if !typed.isEmpty { return typed }
        return Self.inferredUsername(fromSessionPaths: sessionPaths)
    }

    func effectiveHost(pairedHost: String?) -> String? {
        let typed = sshHostOverride.trimmingCharacters(in: .whitespacesAndNewlines)
        if !typed.isEmpty { return typed }
        guard let pairedHost, !pairedHost.isEmpty else { return nil }
        return pairedHost
    }
}

// MARK: - Known host keys

/// Trust-on-first-use for SSH host keys.
///
/// The alternative — accepting whatever key turns up, as NIO's own sample client
/// does and as at least one competitor ships — means a machine that can answer
/// on the tailnet address can read everything typed into the terminal. An
/// unverified host key is forgeable by exactly such a relay, so: the first key
/// is pinned, a changed key is a hard stop with the old and new fingerprints
/// shown, and clearing a pin is a deliberate act in Settings.
///
/// Pins live in the Keychain rather than `UserDefaults` because what matters
/// about them is integrity, not secrecy, and the Keychain is the only store in
/// the sandbox an attacker with file access cannot quietly rewrite.
@MainActor
enum KnownHostKeys {
    private static let account = "ssh-known-hosts"

    struct Pin: Codable, Sendable, Hashable {
        var fingerprint: String
        var addedAt: Date
    }

    private static func load() -> [String: Pin] {
        guard let data = Keychain.load(account: account),
            let map = try? JSONDecoder().decode([String: Pin].self, from: data)
        else { return [:] }
        return map
    }

    private static func store(_ map: [String: Pin]) {
        guard let data = try? JSONEncoder().encode(map) else { return }
        try? Keychain.save(data, account: account)
    }

    /// Keys are host+port, because two daemons behind one name on different
    /// ports are two different servers.
    static func key(host: String, port: Int) -> String { "\(host.lowercased()):\(port)" }

    static func pin(host: String, port: Int) -> Pin? {
        load()[key(host: host, port: port)]
    }

    static func remember(fingerprint: String, host: String, port: Int) {
        var map = load()
        map[key(host: host, port: port)] = Pin(fingerprint: fingerprint, addedAt: Date())
        store(map)
    }

    static func forget(host: String, port: Int) {
        var map = load()
        map.removeValue(forKey: key(host: host, port: port))
        store(map)
    }

    static func all() -> [(host: String, pin: Pin)] {
        load().map { ($0.key, $0.value) }.sorted { $0.host < $1.host }
    }

    static func forgetAll() {
        Keychain.delete(account: account)
    }
}
