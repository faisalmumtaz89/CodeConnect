import Foundation

/// What *this* daemon is, distilled from `hello_ack` into the handful of
/// questions the rest of the app actually asks.
///
/// It exists so that "does the daemon do X?" is answered in one place, from
/// evidence the daemon supplied, rather than by scattering
/// `capabilities?.foo == true` through the views. Every question defaults to
/// "no" while nothing is known, which keeps a disconnected app from claiming
/// features it cannot demonstrate.
public struct DaemonProfile: Sendable, Hashable {
    public var protocolVersion: UInt32
    /// The additive feature level. Level `1` only ever added to the protocol, so
    /// the *major* version stayed at 1 and this — not `protocolVersion` — is
    /// what distinguishes a newer daemon from an older one.
    public var protocolMinor: UInt32
    public var capabilities: Capabilities?
    /// What `codeconnect devices` calls this phone, once the daemon has said.
    var deviceName: String?
    /// What the owner calls the computer, once the daemon has said (minor 25).
    public var machineName: String?
    /// `"macos"` or `"linux"`, once the daemon has said (minor 25).
    public var machineOS: String?

    static let unknown = DaemonProfile(
        protocolVersion: 0, protocolMinor: 0, capabilities: nil, deviceName: nil)

    init(
        protocolVersion: UInt32, protocolMinor: UInt32, capabilities: Capabilities?,
        deviceName: String? = nil, machineName: String? = nil, machineOS: String? = nil
    ) {
        self.protocolVersion = protocolVersion
        self.protocolMinor = protocolMinor
        self.capabilities = capabilities
        self.deviceName = deviceName
        self.machineName = machineName
        self.machineOS = machineOS
    }

    public var isConnected: Bool { capabilities != nil }

    /// The computer's system as a person writes it, or nil for one this build
    /// does not know — nothing is shown rather than a raw token.
    var machineSystem: String? { Self.system(forOS: machineOS) }

    /// `hello_ack.machine_os` as a person writes it.
    public static func system(forOS os: String?) -> String? {
        switch os {
        case "macos": "macOS"
        case "linux": "Linux"
        default: nil
        }
    }

    /// The daemon puts a `risk` block on approval cards, which makes an *absent*
    /// class meaningful (contract: treat it as medium). Every connected daemon does.
    public var classifiesRisk: Bool { isConnected }

    /// The daemon accepts `delete_session` (minor 7).
    ///
    /// A **soft gate**: an older Mac simply does not offer the
    /// swipe. Unknown is false, so this can only ever hide the action, never
    /// invent one — and a swipe that appeared and then failed would be worse than
    /// no swipe, because the row would stay and the user would not know why.
    public var removesSessions: Bool { capabilities?.deletesSessions == true }

    /// The daemon ends a run CodeConnect started (`stop_run`, minor 35). Unknown
    /// is false, so End run is only offered where it can be sent.
    public var endsRuns: Bool { capabilities?.endsRuns == true }

    /// The daemon answers `get_diff`: every connected daemon does.
    public var servesDiff: Bool { isConnected }

    /// The daemon mints a `session_uid` per run, puts it on every session and
    /// every event, and accepts one anywhere a session is named.
    ///
    /// Every connected daemon does; while nothing is known of it, the app keeps the
    /// tmux name as the identity, which is all a list it cannot ask carries.
    public var scopesSessionsByUID: Bool { isConnected }

    /// This connection is actually encrypted — not merely "the daemon holds a
    /// certificate", which is what `capabilities.tls` says.
    public var connectionEncrypted: Bool { capabilities?.tlsActive == true }

    /// **This daemon honours a stop for a Codex session** (minor 17).
    ///
    /// A **hard gate**, and the reason is not a hang: a
    /// minor-15/16 daemon decodes `interrupt` perfectly well and answers a
    /// well-formed refusal to *every* ask. So an unflagged daemon would give the
    /// reader a button that is offered and guaranteed broken — a control whose
    /// only behaviour is refusing the tap, which is the one thing the
    /// verification bar names by name.
    public var stopsCodexTurns: Bool { capabilities?.codexInterrupt == true }

    /// **Whether this daemon honours a Stop for a run of `agent`** — `codex_interrupt`
    /// for Codex, `claude_interrupt` for a hosted Claude or Antigravity run (minor 37). The same
    /// hard gate as `stopsCodexTurns`, resolved per agent: an unflagged daemon
    /// answers a well-formed refusal to every ask, so the button must not be offered.
    public func honoursStop(for agent: AgentKind) -> Bool {
        switch agent {
        case .codex: return capabilities?.codexInterrupt == true
        // An Antigravity run is stopped through the same agent host's interrupt, on any computer that hosts one.
        case .claude, .antigravity: return capabilities?.claudeInterrupt == true
        case .unsupported: return false
        }
    }

    /// **This daemon understands `compose` at all** (minor 18).
    ///
    /// The strictest gate in the app. A minor-17 daemon answers
    /// `Error{code:"bad_request"}` rather than a `ComposeResult`, so a phone
    /// that sent one would register a typed waiter for a reply that never comes
    /// and sit on it until the request timeout — every tap.
    public var composesToCodex: Bool { capabilities?.codexCompose == true }

    /// **A Codex card can be answered here**: the daemon puts `request_id` on the
    /// Codex `approval_resolved` payload, so a card answered elsewhere is retired
    /// here. Every connected daemon does; a card is never answerable while nothing
    /// is known of the daemon.
    public var resolvesCodexCards: Bool { isConnected }

    /// **The daemon answers Claude's questions from the phone**: the `question_card`
    /// capability. Without it the question is read-only here: such a daemon files it
    /// as an ordinary approval, and nothing this phone could send it chooses an answer.
    public var answersQuestions: Bool { capabilities?.questionCard == true }

    /// `answersQuestions`, or nil while nothing is known of the daemon — its
    /// link is down and took its capabilities with it. Unknown is not old.
    public var knownAnswersQuestions: Bool? { isConnected ? answersQuestions : nil }

    /// **The daemon answers a Claude approval only through the hook Claude holds
    /// for that card's own call** — the `hook_only_approvals` capability (minor
    /// 22). Without it a Claude approval is read-only here: an older daemon
    /// types the answer into whatever prompt is on screen, which can be another
    /// agent's.
    public var answersApprovalsByHook: Bool { capabilities?.hookOnlyApprovals == true }

    /// **The daemon serves a run's log a page at a time**: the `history_pages`
    /// capability. With it a chat holds the newest 500 events and asks for earlier
    /// ones; without it the app backfills and loads the whole log.
    public var pagesHistory: Bool { capabilities?.historyPages == true }

    /// Why Codex's controls are absent on a daemon that is otherwise working,
    /// or nil when they are not. One sentence, so the two flags cannot produce
    /// two different accounts of one old Mac.
    var codexCaveat: String? {
        // **Explains itself when disconnected**, like the adjacent `diffCaveat`.
        // Returning nil left the Codex surface silent on a dropped link while
        // the diff surface beside it said what was wrong.
        guard isConnected else {
            return "Not connected. \(HostDevice.current.computer("The computer", named: machineName)) has not said what it can do for a Codex run."
        }
        switch (stopsCodexTurns, composesToCodex) {
        case (true, true): return nil
        case (true, false):
            return "\(HostDevice.current.computer("The computer", named: machineName))’s CodeConnect can stop a Codex turn but cannot carry a message to one. Update it."
        case (false, true):
            return "\(HostDevice.current.computer("The computer", named: machineName))’s CodeConnect can carry a message to Codex but cannot stop a turn. Update it."
        case (false, false):
            return "\(HostDevice.current.computer("The computer", named: machineName))’s CodeConnect cannot stop or speak to a Codex run. Update it."
        }
    }

    /// Why the diff button might not work, or nil when it should: it works on every
    /// connected daemon.
    public var diffCaveat: String? {
        isConnected ? nil : "Not connected. \(HostDevice.current.computer("The computer")) can’t be asked for a diff."
    }
}
