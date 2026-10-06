import SwiftUI

// =============================================================================
//  Domain adapters — the product's own enums, mapped onto the system.
//
//  This file is the *only* place a `FleetStatus` is allowed to know what colour
//  it is. Screens ask for `status.ccTone`; they never pick a colour, which is
//  what stops "blocked" being amber on one screen and orange on the next.
// =============================================================================

extension FleetStatus {
    /// The **badge** tone.
    ///
    /// `running` is deliberately `neutral`, not `info`: the system-blue
    /// "Running" pill is gone. Running is the healthy default and colour is
    /// reserved for the things that need you.
    var ccTone: CCTone {
        switch self {
        case .blocked: return .warning
        case .failed: return .danger
        case .doneUnreviewed: return .success
        case .running: return .neutral
        case .idle: return .neutral
        case .ended: return .neutral
        }
    }

    /// The **dot** colour. Three neutral steps —
    /// `text`, `textTertiary`, `textDisabled` — separate running from idle from
    /// ended without spending a single hue on any of them.
    var ccDotColor: Color {
        switch self {
        case .blocked: return CC.color.warning
        case .failed: return CC.color.danger
        case .doneUnreviewed: return CC.color.success
        // White. Running is healthy; colour is for things that need you.
        case .running: return CC.text.primary
        case .idle: return CC.text.tertiary
        case .ended: return CC.text.disabled
        }
    }

    /// Only `blocked` pulses. Never `running` — a fleet of eight pulsing dots
    /// is a fairground.
    var ccDotPulses: Bool { self == .blocked }
}

extension ToolStatus {
    var ccTone: CCTone {
        switch self {
        case .running: return .neutral
        case .succeeded: return .success
        case .failed: return .danger
        case .interrupted: return .warning
        case .denied: return .warning
        case .unresolved: return .neutral
        }
    }
}

extension LinkHealth.Level {
    /// The tone flips to `danger` exactly where `actionsEnabled` goes false.
    /// A user should be able to learn the palette once — red means the buttons
    /// below are dead — rather than learning six words.
    ///
    /// `stale` moved from `warning` to `danger`: the visual weight
    /// must match the functional consequence, and the consequence of stale is
    /// that nothing works.
    var ccTone: CCTone {
        switch self {
        case .live: return .success
        case .lagging: return .warning
        case .stale: return .danger
        case .connecting: return .info
        case .offline: return .neutral
        case .rejected: return .danger
        }
    }
}

extension CapabilityBadge {
    var ccTone: CCTone { canAct ? .success : .neutral }
}

extension NoticeSeverity {
    /// `info` is `neutral`, not `.info`: a timeline notice that a session
    /// started is not news, and spending blue on it would put a colour on the
    /// most common row in the product. Colour here is reserved for the notices
    /// that changed something — a gap, a refusal, a failure.
    var ccTone: CCTone {
        switch self {
        case .info: return .neutral
        case .success: return .success
        case .warning: return .warning
        case .failure: return .danger
        }
    }
}

// MARK: - Ready-made components

extension CCBadge {
    /// The fleet status badge. `count` renders as `×3` and is dropped when it
    /// is 1 — "Blocked ×1" is noise.
    init(status: FleetStatus, count: Int = 0) {
        self.init(
            status.label,
            icon: nil,
            tone: status.ccTone,
            count: count > 1 ? count : nil,
            accessibilityText: status.label)
    }

    /// Whether an answer typed here can actually reach the agent — reported,
    /// never assumed.
    init(capability: CapabilityBadge) {
        self.init(
            capability.label,
            tone: capability.ccTone,
            accessibilityText: capability.canAct
                ? "Control: answers from this phone reach the agent"
                : "Observe only. \(capability.reason ?? "")")
        // Callers must not construct one for `.unknown` — the fleet's
        // `showsCapability` refuses unsettled rows — but the type cannot make
        // that unrepresentable without losing the shared initializer, so the
        // guard lives at the render sites.
    }
}

extension CCTag {
    /// The agent tag. The tag's own names, not `AgentKind.displayName`: prose
    /// says "Claude", the tag names the product the session runs. An
    /// unsupported agent is the daemon's word, verbatim.
    init(agent: AgentKind) {
        let name: String
        switch agent {
        case .claude: name = "Claude Code"
        case .codex: name = "Codex"
        case .unsupported(let raw): name = raw
        }
        self.init(name, accessibilityText: "\(name) session")
    }
}

extension CCStatusDot {
    /// The fleet dot. `isCached` is the *only* per-row cached treatment — the
    /// word "cached" belongs in the banner, never on a row.
    init(status: FleetStatus, size: CGFloat = Size.row.rawValue, isCached: Bool = false) {
        self.init(
            color: status.ccDotColor,
            size: size,
            isHollow: isCached,
            pulses: status.ccDotPulses,
            accessibilityText: isCached ? "\(status.label), from cache" : status.label)
    }
}

extension CCFreshnessPill {
    /// Link health, expressed so that no state is ever shown without its age.
    ///
    /// This initialiser is the **only** written-down copy of the freshness
    /// table. Note that the label colour is not simply the tone colour: `live`
    /// prints its age in `textTertiary`, because a healthy link is not news.
    init(health: LinkHealth, action: (() -> Void)? = nil) {
        let dot: Color
        let label: Color
        var hollow = false
        var pulses = false

        switch health.level {
        case .live:
            dot = CC.color.success
            label = CC.text.tertiary
        case .lagging:
            dot = CC.color.warning
            label = CC.color.warning
        case .stale:
            dot = CC.color.danger
            label = CC.color.danger
        case .connecting:
            dot = CC.color.info
            label = CC.text.tertiary
            pulses = true
        case .offline:
            dot = CC.text.tertiary
            label = CC.text.tertiary
            hollow = true
        case .rejected:
            dot = CC.color.danger
            label = CC.color.danger
        }

        let isAge: Bool
        switch health.level {
        case .live, .lagging, .stale: isAge = true
        case .connecting, .offline, .rejected: isAge = false
        }
        self.init(
            age: health.shortText,
            prefix: isAge ? "link" : nil,
            dotColor: dot,
            labelColor: label,
            isHollow: hollow,
            pulses: pulses,
            accessibilityLabelText: "Link health",
            accessibilityValueText: Self.spokenValue(for: health),
            accessibilityHintText: action == nil ? nil : "Opens connection details",
            action: action)
    }

    private static func spokenValue(for health: LinkHealth) -> String {
        let word: String
        switch health.level {
        case .live: word = "Live"
        case .lagging: word = "Lagging"
        case .stale: word = "Stale"
        case .connecting: word = "Connecting"
        case .offline: word = "Offline"
        case .rejected: word = "Token rejected"
        }
        switch health.level {
        case .live, .lagging, .stale:
            // VoiceOver reads "14s" as "fourteen ess"; spell it out.
            let age = health.age.map(Format.spokenAge) ?? "unknown"
            return "\(word). Daemon last spoke \(age)."
        default:
            return "\(word). \(health.detail)"
        }
    }
}

// MARK: - Banner ladder inputs

extension CCBannerItem {
    /// What the sample fleet says about itself, wherever it is on screen.
    ///
    /// **It outranks every other banner**, because every other banner is a
    /// statement about a Mac and here there is none: an `offline` notice would
    /// be describing a link that does not exist. It is also the only banner in
    /// the product that is not a problem, which is why it is `info` and why its
    /// action is a way out rather than a retry.
    ///
    /// One written-down copy, because two screens carry it and a fleet and a
    /// session that described this differently would be two different claims
    /// about the same fact.
    static func sampleFleet(onLeave: @escaping () -> Void) -> CCBannerItem {
        CCBannerItem(
            .rejected,
            title: "Sample fleet",
            message:
                "These agents are not real and nothing here is connected to a Mac. Leave to pair with your own.",
            tone: .info,
            icon: "eye",
            actionTitle: "Leave",
            action: onLeave)
    }
}

extension LinkHealth {
    /// The link's claim on the screen's single banner slot, or `nil` when the
    /// link has nothing to say.
    ///
    /// Returns the *candidate*, never the decision: `CCBannerSlot` picks. That
    /// is what keeps "rejected beats offline beats stale" in one place instead
    /// of in an `if` ladder on every screen.
    @MainActor
    func ccBannerItem(
        onRetry: @escaping () -> Void,
        onSettings: @escaping () -> Void,
        onTailscale: (() -> Void)? = nil
    ) -> CCBannerItem? {
        switch level {
        case .live, .lagging:
            return nil
        case .rejected:
            return CCBannerItem(
                .rejected, title: "Token rejected", message: detail, tone: .danger,
                icon: "lock.slash", actionTitle: "Settings", action: onSettings)
        case .offline:
            switch cause {
            case .tailnetHostUnresolvable(let host):
                // Recovery guidance, not a claim: DNS said only that the name
                // did not answer. On a tailnet name that is almost always
                // Tailscale being off on this phone — but "almost always" is
                // why the title is an instruction rather than a diagnosis.
                return CCBannerItem(
                    .offline, title: "Connect Tailscale",
                    message:
                        "`\(host)` isn't resolving. Use Tailscale to put this iPhone on the same tailnet as your Mac — CodeConnect reconnects on its own.",
                    tone: .neutral,
                    icon: "bolt.horizontal.circle",
                    actionTitle: TailscaleAssist.isInstallHintNeeded
                        ? "Set up Tailscale" : "Open Tailscale",
                    action: onTailscale ?? onRetry)
            case .hostUnresolvable(let host):
                return CCBannerItem(
                    .offline, title: "Host not found",
                    message:
                        "`\(host)` isn't resolving. Check the paired address and this iPhone's network — CodeConnect reconnects on its own.",
                    tone: .neutral,
                    icon: "bolt.horizontal.circle",
                    actionTitle: "Settings", action: onSettings)
            case nil:
                return CCBannerItem(
                    .offline, title: "Offline", message: detail, tone: .neutral,
                    icon: "bolt.horizontal.circle", actionTitle: "Retry", action: onRetry)
            }
        case .stale:
            return CCBannerItem(
                .stale, title: "Link stale",
                message:
                    "\(ageText) since the daemon last spoke. Actions are disabled until it answers.",
                tone: .danger, icon: "exclamationmark.triangle.fill",
                actionTitle: "Retry", action: onRetry)
        case .connecting:
            // Only after 2s of connecting — a banner that flashes on
            // every reconnect is noise. The caller owns that delay.
            return CCBannerItem(
                .offline, title: "Connecting", message: "Connecting to the daemon…",
                tone: .info, icon: "arrow.triangle.2.circlepath")
        }
    }
}

extension CCDisabledReason {
    /// Lifts the model's own `disabledReason` into the kit's type, so a screen
    /// cannot disable a control without carrying the explanation with it.
    init?(_ reason: String?) {
        guard let reason, !reason.isEmpty else { return nil }
        self.init(reason)
    }
}
