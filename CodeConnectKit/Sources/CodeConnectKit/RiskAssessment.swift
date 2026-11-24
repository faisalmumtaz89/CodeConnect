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
/// That asymmetry is deliberate. Escalating costs a higher place in the queue
/// and a stronger tag for a command that turned out to be harmless. De-escalating would show
/// `git push --force` as routine because a rule file said so — and the whole
/// product is the promise that that cannot happen.
///
/// The contract's "absent means medium" applies only when the daemon is known
/// to classify: on an older daemon nothing is absent, because nothing was ever
/// offered, and forcing every `Read` to MEDIUM would be a regression dressed up
/// as caution.
public struct RiskAssessment: Sendable, Hashable {
    /// The class the UI gates on.
    public var effective: RiskClass

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

        if let declared {
            return RiskAssessment(effective: max(declared, heuristic))
        }
        if profile.classifiesRisk {
            // Contract: a classifying daemon that omits the field means medium.
            // Still a floor, never a ceiling.
            return RiskAssessment(effective: max(.medium, heuristic))
        }
        return RiskAssessment(effective: heuristic)
    }
}
