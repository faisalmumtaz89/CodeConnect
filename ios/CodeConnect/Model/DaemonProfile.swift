import Foundation

/// What *this* daemon is, distilled from `hello_ack` into the handful of
/// questions the rest of the app actually asks.
///
/// It exists so that "does the daemon do X?" is answered in one place, from
/// evidence the daemon supplied, rather than by scattering
/// `capabilities?.foo == true` through the views. Every question defaults to
/// "no" while nothing is known, which keeps a disconnected app from claiming
/// features it cannot demonstrate.
struct DaemonProfile: Sendable, Hashable {
    var protocolVersion: UInt32
    /// The additive feature level. Level `1` only ever added to the protocol, so
    /// the *major* version stayed at 1 and this — not `protocolVersion` — is
    /// what distinguishes a newer daemon from an older one.
    var protocolMinor: UInt32
    var capabilities: Capabilities?
    /// What `codeconnect devices` calls this phone, once the daemon has said.
    var deviceName: String?

    static let unknown = DaemonProfile(
        protocolVersion: 0, protocolMinor: 0, capabilities: nil, deviceName: nil)

    init(
        protocolVersion: UInt32, protocolMinor: UInt32, capabilities: Capabilities?,
        deviceName: String? = nil
    ) {
        self.protocolVersion = protocolVersion
        self.protocolMinor = protocolMinor
        self.capabilities = capabilities
        self.deviceName = deviceName
    }

    var isConnected: Bool { capabilities != nil }

    /// The daemon reports protocol minor 1 or newer, which is where
    /// `turn_complete`, `get_diff`, pairing codes, per-device tokens, `risk`
    /// and `ResolvedBy::Local` arrived. A newer major implies all of them.
    var speaksMinor1OrLater: Bool { protocolVersion > Wire.protocolVersion || protocolMinor >= 1 }

    /// The daemon files the Stop hook as `EventKind::TurnComplete`, so the
    /// `session_end` + `hook_event_name: "Stop"` workaround is no longer needed.
    ///
    /// There is no separate capability flag for this — `protocol_minor` is the
    /// contract's own answer, and the workaround is inert against such a daemon
    /// anyway, since one that emits `turn_complete` for `Stop` never emits a
    /// `session_end` carrying that hook name.
    var trustsTurnCompleteKind: Bool { speaksMinor1OrLater }

    /// The daemon puts a `risk` block on approval cards, which makes an *absent*
    /// class meaningful (contract: treat it as medium) rather than merely old.
    var classifiesRisk: Bool { capabilities?.classifiesRisk == true || speaksMinor1OrLater }

    /// The daemon accepts `delete_session` (minor 7).
    ///
    /// A **soft gate**, like `servesDiff`: an older Mac simply does not offer the
    /// swipe. Unknown is false, so this can only ever hide the action, never
    /// invent one — and a swipe that appeared and then failed would be worse than
    /// no swipe, because the row would stay and the user would not know why.
    var removesSessions: Bool { capabilities?.deletesSessions == true }

    /// The daemon answers `get_diff`.
    var servesDiff: Bool { capabilities?.servesDiff == true || speaksMinor1OrLater }

    /// The daemon mints a `session_uid` per run, puts it on every session and
    /// every event, and accepts one anywhere a session is named.
    ///
    /// Unlike `servesDiff` this is a **hard gate**, and deliberately so: the
    /// consequence of guessing wrong is not a missing affordance but a
    /// mis-attributed timeline, or an answer typed into a different agent's TTY.
    /// Either the daemon has said it does this — by the capability flag or by
    /// its own feature level — or the app keeps treating the tmux name as the
    /// identity, which is what a pre-minor-2 daemon means by sending no uid.
    var scopesSessionsByUID: Bool {
        capabilities?.scopesSessionsByUID == true || protocolMinor >= 2
    }

    /// The daemon takes `hello{pairing_code}` and hands back a device token.
    var supportsPairingCode: Bool { speaksMinor1OrLater }

    /// This connection is actually encrypted — not merely "the daemon holds a
    /// certificate", which is what `capabilities.tls` says.
    var connectionEncrypted: Bool { capabilities?.tlsActive == true }

    /// Why the diff button might not work, or nil when it should.
    ///
    /// Deliberately not a hard gate. A capability key named differently on the
    /// Mac side would otherwise hide a feature that is really there, and a
    /// diff request costs one frame — so the app asks, and says plainly that it
    /// is asking a daemon which never advertised the ability.
    var diffCaveat: String? {
        guard isConnected else { return "Not connected. The daemon cannot be asked for a diff." }
        return servesDiff
            ? nil
            : "This daemon has not advertised diff support. Asking anyway; if it cannot answer, this will say so."
    }
}
