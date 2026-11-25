import CodeConnectKit
import Foundation
import Observation

/// First run, and every launch after it: CodeConnect installed and its daemon answering, this Mac paired
/// with its own daemon without a code, and what the checklist shows — the background service, the agents as
/// that daemon finds them, and `codeconnect` on the PATH of a new Terminal.
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
    private(set) var commandLine: Row = .checking
    /// This Mac's daemon's id (`LocalDaemon.machineID`), read from its CodeConnect folder whatever the install did, so
    /// the agents' rows follow the service once it answers again: the computer whose agents the checklist shows, known
    /// as the start form knows it.
    private(set) var localMachineID: String?
    /// True once this run has its answers: the service, with this Mac paired with it when it is up, and, while
    /// the checklist is open, the PATH line.
    private(set) var settled = false
    /// The checklist is open: on its own when the service isn't working, and from the menu.
    var shown = false

    /// This Mac's own computer among those that can start: the one whose daemon said this Mac's daemon's id when it
    /// connected.
    func thisMacContext(in machines: Machines) -> StartContext? {
        guard let localMachineID else { return nil }
        return machines.startContexts.first { machines.model(for: $0.id)?.connection.helloAck?.machineID == localMachineID }
    }

    /// An agent as this Mac's service answers for it (its list of folders and agents), so the checklist and the start
    /// form say the same: Checking… until it has answered, Ready, or why not (the computer's own sentence
    /// for either agent). A later answer replaces it.
    func agent(_ agent: StartAgent, in machines: Machines) -> Row {
        #if DEBUG
            if let forced = RenderWindow.setupRows { return forced[agent.rawValue] ?? .checking }
        #endif
        guard let context = thisMacContext(in: machines) else { return .checking }
        switch context.models(agent) {
        case .checking: return .checking
        case .choices: return .ready("Ready")
        case .missing(let sentence), .signedOut(let sentence): return .missing(sentence)
        }
    }

    /// The checklist needs the person: the service is missing, or no agent can start (`SetupAttention`); one agent
    /// ready is enough. The PATH line is an opt-in with its own Set up, so it never asks for Try Again.
    func needsAttention(_ machines: Machines) -> Bool {
        SetupAttention.needed(
            serviceMissing: service.isMissing,
            agentsMissing: StartAgent.allCases.map { agent($0, in: machines).isMissing })
    }

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
        #if DEBUG
            if let forced = RenderWindow.setupRows {
                service = forced["service"] ?? .checking
                commandLine = forced["path"] ?? .checking
                settled = true
                shown = true
            }
        #endif
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
    /// optional ones only while the checklist is open. The service reads Checking… until it is answered.
    func run(machines: Machines) async {
        settled = false
        service = .checking
        guard let installer else {
            service = .missing("Not installed by this launch.")
            settled = true
            return
        }
        let failure = await Task.detached { installer.install() }.value
        if let failure {
            service = .missing(failure)
        } else if let address = LocalDaemon.address(root: installer.root) {
            let paired = await pairWithThisMac(at: address, machines: machines, root: installer.root)
            service = paired ? .ready("Running") : .missing("Running, but this Mac couldn’t pair with it.")
        } else {
            service = .missing("The background service isn’t answering.")
        }
        localMachineID = LocalDaemon.machineID(root: installer.root)
        let bin = installer.bin
        // Only a service that isn't working opens the checklist by itself. The PATH line is optional, so it is asked
        // only while the checklist is open: it runs a login shell. The agents are the service's answer, asked for
        // again here, so Try Again after installing or signing in shows what is there now.
        shown = shown || service.isMissing
        if shown {
            thisMacContext(in: machines)?.load()
            let codeconnect = bin.appendingPathComponent("codeconnect").path
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
            commandLine = .missing("Couldn’t set this up: \(error.localizedDescription)")
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
