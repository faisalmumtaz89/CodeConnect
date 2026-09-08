import Foundation

/// **Material this phone has already put on the wire and cannot account for —
/// remembered across launches.**
///
/// `CodexControls` holds the same fact for the life of one process, and that is
/// where the send path reads it. It was not enough: `indeterminate` and
/// `sentNoAnswer` both mean *the mutation may have happened and no id scheme can
/// make a second attempt safe*, and quitting the app is not evidence about what
/// the Mac did. Relaunching forgot every spent material, minted a fresh
/// `request_id`, and the same words went to Codex a second time — the one
/// outcome nobody can undo.
///
/// **Bounded on purpose.** This is a safety interlock, not a history: it keeps
/// the last `capacity` entries in arrival order and drops the oldest, and a run
/// that leaves the fleet takes its entries with it. An unbounded list would grow
/// for the life of the install with nothing able to sweep it — the failure
/// `SessionState.recordsReviewMarks` exists to avoid.
///
/// Entries are `"kind|session|material"`, where material is already a SHA-256
/// hex digest of the preimage (`CodexHash`), so nothing here is a command, a
/// message, or a path — the store holds no content a reader could recover.
@MainActor
final class CodexSpentLedger {

    enum Kind: String, Sendable {
        case stop
        case compose
    }

    /// How many entries survive. Twelve sessions' worth of both mutations is
    /// far past any real fleet's in-flight uncertainty, and the whole store
    /// stays under 4 KB.
    static let capacity = 48

    static let defaultsKey = "cc.codex.spent"

    private let defaults: UserDefaults
    /// Arrival order, oldest first. An array rather than a set because eviction
    /// is by age, and 48 linear scans cost nothing next to a socket round trip.
    private var entries: [String]

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        entries = defaults.stringArray(forKey: Self.defaultsKey) ?? []
        if entries.count > Self.capacity {
            entries = Array(entries.suffix(Self.capacity))
        }
    }

    private static func entry(_ kind: Kind, session: String, material: String) -> String {
        "\(kind.rawValue)|\(session)|\(material)"
    }

    func isSpent(_ kind: Kind, session: String, material: String) -> Bool {
        entries.contains(Self.entry(kind, session: session, material: material))
    }

    func markSpent(_ kind: Kind, session: String, material: String) {
        let entry = Self.entry(kind, session: session, material: material)
        guard !entries.contains(entry) else { return }
        entries.append(entry)
        if entries.count > Self.capacity { entries.removeFirst(entries.count - Self.capacity) }
        flush()
    }

    /// A run that left the fleet takes its uncertainty with it: there is no
    /// longer anything on this Mac the material could be sent to.
    func forget(session: String) {
        let prefix = "|\(session)|"
        let kept = entries.filter { !$0.contains(prefix) }
        guard kept.count != entries.count else { return }
        entries = kept
        flush()
    }

    private func flush() {
        defaults.set(entries, forKey: Self.defaultsKey)
    }
}
