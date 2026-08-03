import SwiftUI

// =============================================================================
//  Domain adapters — the product's own enums, mapped onto the system.
//
//  This file is the *only* place a `FleetStatus` is allowed to know what colour
//  it is. Screens ask for `status.ccTone`; they never pick a colour, which is
//  what stops "blocked" being amber on one screen and orange on the next.
// =============================================================================

extension RiskClass {
    /// `low` carries **no colour at all**. A three-colour
    /// risk scale trains the eye to see green as "fine", and there is no such
    /// thing as a risk-free approval — only a boring one.
    var ccTone: CCTone {
        switch self {
        case .low: return .neutral
        case .medium: return .warning
        case .high: return .danger
        }
    }

    /// **HIGH is the one risk badge that carries a glyph**, and that is how it
    /// out-ranks MEDIUM now that all three share one construction.
    ///
    /// It used to out-rank it by mass — a 100%-saturation `#FF4D4F` fill with a
    /// `#000` label, which is the primary button's own recipe in another hue,
    /// so the loudest object on the root screen was a label nobody can press.
    /// Measured, the replacement is *not* automatically louder: `warning` on
    /// its 12% fill is 8.27:1 and `danger` on its own is 5.41:1, so red alone
    /// would have read as the quieter of the two. Three things restore the
    /// order without touching the fill — the glyph, a 1.5pt border
    /// (`CC.stroke.emphasis`), and the word itself, all of which survive
    /// greyscale and every form of colour blindness. Mass does none of that.
    var ccGlyph: String? {
        self == .high ? "exclamationmark.triangle.fill" : nil
    }
}

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

    init(risk: RiskClass) {
        self.init(
            risk.label,
            icon: risk.ccGlyph,
            tone: risk.ccTone,
            accessibilityText: "Risk \(risk.label). \(risk.rationale)")
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

        self.init(
            age: health.shortText,
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

extension LinkHealth {
    /// The link's claim on the screen's single banner slot, or `nil` when the
    /// link has nothing to say.
    ///
    /// Returns the *candidate*, never the decision: `CCBannerSlot` picks. That
    /// is what keeps "rejected beats offline beats stale" in one place instead
    /// of in an `if` ladder on every screen.
    func ccBannerItem(onRetry: @escaping () -> Void, onSettings: @escaping () -> Void)
        -> CCBannerItem?
    {
        switch level {
        case .live, .lagging:
            return nil
        case .rejected:
            return CCBannerItem(
                .rejected, title: "Token rejected", message: detail, tone: .danger,
                icon: "lock.slash", actionTitle: "Settings", action: onSettings)
        case .offline:
            return CCBannerItem(
                .offline, title: "Offline", message: detail, tone: .neutral,
                icon: "bolt.horizontal.circle", actionTitle: "Retry", action: onRetry)
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
