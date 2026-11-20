import CodeConnectKit
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
        case .running, .stopping: return .neutral
        // Seen: as quiet as a run with nothing to show yet.
        case .done, .idle: return .neutral
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
        // On its way to ended: the row's line says Stopping…
        case .stopping: return CC.text.tertiary
        case .done, .idle: return CC.text.tertiary
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
        case .blocked: return .neutral
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
    /// The agent tag: the product the session runs (`AgentKind.productName`),
    /// not the name prose addresses it by (`displayName`). An unsupported agent
    /// is the daemon's word, verbatim.
    init(agent: AgentKind) {
        let name = agent.productName
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

/// **A computer, and whether it is answering** — the one label for it, on the
/// home screen's project cards and in a session's header.
///
/// A green dot is a live link and says nothing more: a healthy link is not news.
/// Anything else is said in a word beside the name — the dot alone would be a
/// colour asked to carry a fact. It takes the link's level rather than reading
/// it, because the level is derived from the clock: each caller reads it in a
/// view of its own, so the second is charged to that view and not to a list.
struct CCMachineLabel: View {
    /// Nil while no daemon has named the computer: the dot and any word stand
    /// alone.
    let name: String?
    let level: LinkHealth.Level
    /// The word beside the name: the link's own (`LinkHealth.machineWord`) where the caller has the link, else
    /// what the level alone says.
    var word: String?
    /// Nil wraps, which a project card has room for; the header's one line
    /// truncates the name instead, and never the word.
    var lineLimit: Int?
    /// Half the footnote's cap height, so the dot sits on the first line's
    /// middle however many lines the name wraps to.
    @ScaledMetric(relativeTo: .footnote) private var capMiddle: CGFloat = 4.5

    var body: some View {
        let mark = level.machineMark
        HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
            CCStatusDot(color: mark.dot, size: CC.size.dotSm, isHollow: mark.hollow)
                .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + capMiddle }
            if let lineLimit {
                // Cut short, the name gives way and the word stays whole: the
                // word is the one fact the dot cannot say alone.
                HStack(alignment: .firstTextBaseline, spacing: 0) {
                    Text(verbatim: name ?? "").foregroundStyle(CC.text.secondary)
                        .lineLimit(lineLimit)
                    Text(Self.suffix(name: name, word: word ?? mark.word))
                        .foregroundStyle(CC.text.primary)
                        .fixedSize()
                }
                .ccType(CC.type.footnote)
            } else {
                // One run of text, so it wraps as a whole: a word wrapped apart
                // from the name it belongs to reads as a second fact.
                (Text(verbatim: name ?? "").foregroundStyle(CC.text.secondary)
                    + Text(Self.suffix(name: name, word: word ?? mark.word)).foregroundStyle(CC.text.primary))
                    .ccType(CC.type.footnote)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private static func suffix(name: String?, word: String?) -> String {
        guard let word else { return "" }
        return name == nil ? word : " · \(word)"
    }
}

extension LinkHealth.Level {
    /// The dot, and the word said beside a computer's name — none while the
    /// link is live or merely lagging, and none for a refusal, whose word is
    /// its cause's (`LinkHealth.machineWord`), which the level alone cannot tell.
    var machineMark: (dot: Color, hollow: Bool, word: String?) {
        switch self {
        case .live: return (CC.color.success, false, nil)
        case .lagging: return (CC.color.warning, false, nil)
        case .stale: return (CC.color.danger, false, "Stale")
        case .connecting: return (CC.color.info, false, "Connecting")
        case .offline: return (CC.text.tertiary, true, "Offline")
        case .rejected: return (CC.color.danger, false, nil)
        case .blocked: return (CC.color.danger, false, "Blocked")
        }
    }
}

extension LinkHealth {
    /// What VoiceOver says for the link: the state, and its age while there is
    /// a link to age, spelled out — "14s" is read as "fourteen ess".
    var spokenValue: String {
        switch level {
        case .live, .lagging, .stale:
            let age = age.map(Format.spokenAge) ?? "unknown"
            return "\(word). Your computer last answered \(age)."
        default:
            return "\(word). \(detail)"
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
                "These agents are not real and nothing here is connected to a computer. Leave to pair with your own.",
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
        // The words and the one action are the Kit's (`LinkHealth.notice`), which the Mac's computer row and list
        // say too; the slot, tone and icon are this screen's.
        guard let notice else { return nil }
        let action: (title: String, run: () -> Void)? =
            switch notice.action {
            case .retry: ("Try again", onRetry)
            case .settings: ("Settings", onSettings)
            case .tailscale:
                (TailscaleAssist.isInstallHintNeeded ? "Set up Tailscale" : "Open Tailscale", onTailscale ?? onRetry)
            case nil: nil
            }
        let (slot, tone, icon): (CCBannerPriority, CCTone, String) =
            switch level {
            case .live, .lagging: (.offline, .neutral, "plugs")
            case .rejected: (.rejected, .danger, "lock-open")
            case .blocked: (.offline, .neutral, "lock")
            case .offline: (.offline, .neutral, "plugs")
            case .stale: (.stale, .danger, "warning-fill")
            case .connecting: (.offline, .info, "arrows-clockwise")
            }
        return CCBannerItem(
            slot, title: notice.title, message: notice.sentence, tone: tone, icon: icon,
            actionTitle: action?.title, action: action?.run)
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
