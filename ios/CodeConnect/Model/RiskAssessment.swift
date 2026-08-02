import Foundation

/// How risky this decision is, and — just as important — who says so.
///
/// Two classifiers exist and they know different things. The daemon sees the
/// repo, the worktree boundary and the operator's own rules, so `risk_class` is
/// the better answer whenever it arrives. The phone sees only the command, but
/// it has had that text ever since it could answer a tool call at all, and it
/// recognises `rm -rf` without being told.
///
/// One rule reconciles them:
///
/// > **The effective class is the stricter of the two. The daemon can tighten a
/// > gate; it can never loosen one below what this build's own reading of the
/// > command justifies.**
///
/// That asymmetry is deliberate. Escalating costs a hold and a Face ID on a
/// command that turned out to be harmless. De-escalating would turn
/// `git push --force` into a single tap because a rule file said so — and the
/// whole product is the promise that that cannot happen.
///
/// The contract's "absent means medium" applies only when the daemon is known
/// to classify: on an older daemon nothing is absent, because nothing was ever
/// offered, and forcing every `Read` to MEDIUM would be a regression dressed up
/// as caution.
struct RiskAssessment: Sendable, Hashable {
    /// Where the class this build is acting on came from.
    enum Source: Sendable, Hashable {
        /// The daemon named a class and this build agreed, or was less strict.
        case daemon
        /// The daemon classifies but said nothing about this card, so the
        /// contract's medium floor applied.
        case daemonSilent
        /// The daemon does not classify at all; this is the phone's own reading.
        case local
    }

    /// The class the UI gates on.
    var effective: RiskClass
    /// What the daemon said, parsed. Nil when it said nothing.
    var declared: RiskClass?
    /// What this build makes of the command on its own.
    var heuristic: RiskClass
    /// The daemon's rule, when it named one.
    var matchedPattern: String?
    var source: Source

    /// True when the phone's own reading was stricter than the daemon's.
    var escalatedLocally: Bool {
        guard let declared else { return false }
        return effective > declared
    }

    static func parse(_ raw: String?) -> RiskClass? {
        switch raw?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "low": return .low
        case "medium", "med": return .medium
        case "high": return .high
        default: return nil
        }
    }

    static func resolve(
        wire: WireRisk?, tool: String, input: JSONValue?, profile: DaemonProfile
    ) -> RiskAssessment {
        let heuristic = ToolSummary.risk(tool: tool, input: input)
        let declared = parse(wire?.cls)
        let matchedPattern = wire?.matchedPattern

        if let declared {
            return RiskAssessment(
                effective: max(declared, heuristic), declared: declared, heuristic: heuristic,
                matchedPattern: matchedPattern, source: .daemon)
        }
        if profile.classifiesRisk {
            // Contract: a classifying daemon that omits the field means medium.
            // Still a floor, never a ceiling.
            return RiskAssessment(
                effective: max(.medium, heuristic), declared: nil, heuristic: heuristic,
                matchedPattern: matchedPattern, source: .daemonSilent)
        }
        return RiskAssessment(
            effective: heuristic, declared: nil, heuristic: heuristic,
            matchedPattern: matchedPattern, source: .local)
    }

    /// One line saying where this class came from. Shown under the badge,
    /// because a risk class whose provenance is invisible is a risk class the
    /// user has to take on faith.
    var provenance: String {
        switch source {
        case .daemon:
            if escalatedLocally {
                return
                    "The Mac classified this \(declared?.label ?? "-"); this app reads the command as \(heuristic.label) and applied the stricter one."
            }
            return "Classified at the Mac."
        case .daemonSilent:
            return
                heuristic > .medium
                ? "The Mac sent no class for this card. This app reads the command as \(heuristic.label)."
                : "The Mac sent no class for this card, so it is treated as MEDIUM."
        case .local:
            return "This daemon does not classify risk; read from the command by this app."
        }
    }
}
