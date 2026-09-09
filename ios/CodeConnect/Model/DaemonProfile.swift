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

    /// **This daemon honours a stop for a Codex session** (minor 17).
    ///
    /// A **hard gate**, unlike `servesDiff`, and the reason is not a hang: a
    /// minor-15/16 daemon decodes `interrupt` perfectly well and answers a
    /// well-formed refusal to *every* ask. So an unflagged daemon would give the
    /// reader a button that is offered and guaranteed broken — a control whose
    /// only behaviour is refusing the tap, which is the one thing the
    /// verification bar names by name.
    var stopsCodexTurns: Bool { capabilities?.codexInterrupt == true }

    /// **This daemon understands `compose` at all** (minor 18).
    ///
    /// The strictest gate in the app. A minor-17 daemon answers
    /// `Error{code:"bad_request"}` rather than a `ComposeResult`, so a phone
    /// that sent one would register a typed waiter for a reply that never comes
    /// and sit on it until the request timeout — every tap.
    var composesToCodex: Bool { capabilities?.codexCompose == true }

    /// **The minimum daemon for an ANSWERABLE Codex card** (minor 19).
    ///
    /// Minor 19 is where `request_id` arrived on the Codex `approval_resolved`
    /// payload. Below it a resolution cannot be correlated at all — D1 forbids
    /// prefix-parsing `source_event_id`, and there is nothing else to key on —
    /// so a card answered at the Mac stays live and tappable on the phone for
    /// ever, with nothing on screen to explain it.
    ///
    /// A card that can be *shown* but never *retired* must not be answerable:
    /// the reader would tap a decision the daemon has already made. So on an
    /// older daemon the Codex card is read-only and says why, the same shape the
    /// compose bar already uses for a minor-16 Mac.
    var resolvesCodexCards: Bool {
        protocolVersion > Wire.protocolVersion || protocolMinor >= 19
    }

    /// Why a Codex card cannot be answered from this phone, or nil when it can.
    ///
    /// **What is wrong, what to do now, what to do about it** — in that order,
    /// because the reader's next action is at the Mac and the update is the fix
    /// rather than the workaround. It replaces a longer sentence that explained
    /// the daemon's bookkeeping ("too old to report when a Codex question has
    /// been answered") and then left the reader with nowhere to go; measured at
    /// AX5 on a 6.9" phone, that one ran to four lines in the pinned action bar
    /// and clipped its own last two words off the bottom of the screen.
    var codexAnswerCaveat: String? {
        guard !resolvesCodexCards else { return nil }
        return "This Mac's CodeConnect is too old for answers from the phone; answer it at "
            + "the Mac. Update the Mac to answer here."
    }

    /// Why Codex's controls are absent on a daemon that is otherwise working,
    /// or nil when they are not. One sentence, so the two flags cannot produce
    /// two different accounts of one old Mac.
    var codexCaveat: String? {
        // **Explains itself when disconnected**, like the adjacent `diffCaveat`.
        // Returning nil left the Codex surface silent on a dropped link while
        // the diff surface beside it said what was wrong.
        guard isConnected else {
            return "Not connected. This Mac has not said what it can do for a Codex session."
        }
        switch (stopsCodexTurns, composesToCodex) {
        case (true, true): return nil
        case (true, false):
            return "This Mac's CodeConnect can stop a Codex turn but cannot carry a message to one. Update it."
        case (false, true):
            return "This Mac's CodeConnect can carry a message to Codex but cannot stop a turn. Update it."
        case (false, false):
            return "This Mac's CodeConnect cannot stop or speak to a Codex session. Update it."
        }
    }

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
