import CodeConnectKit
import Foundation
import Observation

/// First run, and every launch after it: CodeConnect installed and its daemon answering, this Mac paired
/// with its own daemon without a code, and what the checklist shows — the background service, tmux, the
/// agents found, and `codeconnect` on the PATH of a new Terminal.
@MainActor @Observable
final class Setup {
    enum Row: Equatable {
        case checking
        case ready(String)
        case missing(String)

        var isMissing: Bool {
            if case .missing = self { true } else { false }
        }
    }

    private(set) var service: Row = .checking
    private(set) var tmux: Row = .checking
    private(set) var claude: Row = .checking
    private(set) var codex: Row = .checking
    private(set) var commandLine: Row = .checking
    /// True once this run has its answers: the service, with this Mac paired with it when it is up, and tmux;
    /// and, while the checklist is open, the agents and the PATH line.
    private(set) var settled = false
    /// The checklist is open: on its own when the service or tmux isn't working, and from the menu.
    var shown = false
    /// A row that needs the person: the service, tmux, or an agent not found. The PATH line is an opt-in with its own
    /// Set up, so it never asks for Try Again.
    var needsAttention: Bool { [service, tmux, claude, codex].contains(where: \.isMissing) }

    let installer: Installer?
    private let pairing: PairingStore

    /// Whether this launch installs and checks: always in a shipped build; in a debug build only when
    /// asked (`-CC_SETUP`), so no debug launch ever installs into the owner's CodeConnect folder.
    static var runsThisLaunch: Bool {
        #if DEBUG
            ProcessInfo.processInfo.arguments.contains("-CC_SETUP")
        #else
            true
        #endif
    }

    init(installer: Installer?, pairing: PairingStore) {
        self.installer = installer
        self.pairing = pairing
    }

    /// The pairing with this Mac's own daemon follows the daemon to where it answers now: paired at
    /// loopback before Tailscale, it moves to the tailnet address with its token. Asked before the app
    /// connects, so it never dials an address the daemon has left.
    static func followThisMac(_ pairing: PairingStore, root: URL) {
        guard let address = LocalDaemon.address(root: root) else { return }
        for machine in pairing.machines where isLoopback(machine.endpoint.host) {
            pairing.readdress(machine.id, host: address.host, port: address.port, useTLS: address.tls)
        }
    }

    /// The id of this Mac's pairing with its own daemon: the one at loopback, or where the daemon answers now.
    func thisMac(at address: LocalDaemon.Address?) -> String? {
        pairing.machines.first {
            isLoopback($0.endpoint.host) || ($0.endpoint.host == address?.host && $0.endpoint.port == address?.port)
        }?.id
    }

    /// Installs or updates, then pairs with this Mac's daemon if it isn't yet, then answers the rows: the
    /// optional ones only while the checklist is open. The service and tmux read Checking… until they are answered.
    func run(machines: Machines) async {
        settled = false
        service = .checking
        tmux = .checking
        guard let installer else {
            service = .missing("Not installed by this launch.")
            settled = true
            return
        }
        let failure = await Task.detached { installer.install() }.value
        if let failure {
            service = .missing(failure)
        } else if let address = LocalDaemon.address(root: installer.root) {
            service = await pairWithThisMac(at: address, machines: machines, root: installer.root)
                ? .ready("Running") : .missing("Running, but this Mac couldn't pair with it.")
        } else {
            service = .missing("The background service isn't answering.")
        }
        let bin = installer.bin
        tmux = FileManager.default.isExecutableFile(atPath: bin.appendingPathComponent("cc-tmux").path)
            ? .ready("Ready") : .missing("Missing. Choose Try Again to install it again.")
        // Only a service or tmux that isn't working opens the checklist by itself. The agents and the PATH line
        // are optional, so they are asked only while it is open: each one runs a login shell.
        shown = shown || service.isMissing || tmux.isMissing
        if shown {
            let codeconnect = bin.appendingPathComponent("codeconnect").path
            claude = await Task.detached { Self.agent("claude", codeconnect: codeconnect) }.value
            codex = await Task.detached { Self.agent("codex", codeconnect: codeconnect) }.value
            let found = await Task.detached { Self.inNewTerminal("command -v codeconnect") }.value
            commandLine = found?.contains(codeconnect) == true
                ? .ready("Ready") : .missing("Not set up yet")
            #if DEBUG
                // `-CC_ADD_PATH YES`: a test run agrees to the PATH line, as a click on Set up would.
                if case .missing = commandLine, UserDefaults.standard.bool(forKey: "CC_ADD_PATH") { addPathBlock() }
            #endif
        }
        settled = true
    }

    /// The checklist from the menu or its Try Again: open, with every row asked again, since the person may have
    /// just installed what it named. Only once this launch's own run has settled: never a second run beside it, and
    /// none in a launch that doesn't run setup.
    func reopen(machines: Machines) {
        shown = true
        if settled { Task { await run(machines: machines) } }
    }

    /// Adds the PATH block to `~/.zprofile` when the person chooses Set up (its help shows the line): only
    /// offered when a new Terminal window doesn't already find this install's `codeconnect`.
    func addPathBlock() {
        guard let installer else { return }
        do {
            try PathBlock.add(home: installer.home, bin: installer.bin)
            commandLine = .ready("Ready")
        } catch {
            commandLine = .missing("Couldn't set this up: \(error.localizedDescription)")
        }
    }

    /// Pairs with this Mac's daemon when it isn't yet, and returns whether it is: true only once the
    /// pairing is stored. The daemon answers before the app has kept the token it was given, so a pairing
    /// that has only connected is not one yet.
    private func pairWithThisMac(at address: LocalDaemon.Address, machines: Machines, root: URL) async -> Bool {
        func paired() -> Bool { thisMac(at: address) != nil }
        if paired() { return true }
        guard let code = LocalDaemon.pairing(root: root) else { return false }
        machines.pair(withQR: code)
        var waited = 0
        while !paired(), machines.pairingError == nil, waited < 150 {
            try? await Task.sleep(for: .milliseconds(100))
            waited += 1
        }
        return paired()
    }

    /// The lines a new Terminal window prints for `command`, or nil when it fails: an interactive login
    /// shell reads ~/.zprofile and ~/.zshrc as Terminal does, so a PATH set in either counts. Its startup
    /// may print lines of its own before the command's.
    nonisolated private static func inNewTerminal(_ command: String) -> [String]? {
        let (status, output) = Installer.run(URL(fileURLWithPath: "/bin/zsh"), ["-ilc", command])
        return status == 0 ? output.split(separator: "\n").map(String.init) : nil
    }

    /// An agent as `codeconnect` starts it from a new Terminal, never bundled: `codeconnect claude --version`
    /// resolves the agent as `codeconnect claude` does and, with no terminal to host a session in, runs only its
    /// `--version`; the row shows the last line printed.
    nonisolated private static func agent(_ name: String, codeconnect: String) -> Row {
        guard let version = inNewTerminal("'\(codeconnect)' \(name) --version")?.last
        else { return .missing("Not found. Install it, then choose Try Again.") }
        return .ready(version)
    }
}

private func isLoopback(_ host: String) -> Bool {
    ["127.0.0.1", "localhost", "::1", "[::1]"].contains(host.lowercased())
}

/// The lines that put CodeConnect's `bin` on the PATH of every new login shell, in `~/.zprofile`.
enum PathBlock {
    static func file(home: URL) -> URL { home.appendingPathComponent(".zprofile") }

    static func line(home: URL, bin: URL) -> String {
        let path = bin.path.hasPrefix(home.path + "/") ? "$HOME" + bin.path.dropFirst(home.path.count) : bin.path
        return "export PATH=\"\(path):$PATH\""
    }

    static func add(home: URL, bin: URL) throws {
        let existing = (try? String(contentsOf: file(home: home), encoding: .utf8)) ?? ""
        let block = "\n# Added by CodeConnect: its command line tools.\n\(line(home: home, bin: bin))\n"
        try (existing + block).write(to: file(home: home), atomically: true, encoding: .utf8)
    }
}
