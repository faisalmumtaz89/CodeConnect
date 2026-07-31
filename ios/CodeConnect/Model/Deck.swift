import Foundation

/// The cross-fleet decision queue.
///
/// The Deck is the product's signature interaction: one ranked stack of every
/// agent waiting on a human, emptied with a thumb. Two rules are load-bearing
/// and are enforced here rather than in the view:
///
///   * **Urgency first, then oldest.** Risk tier decides the tier — HIGH before
///     MEDIUM before LOW — and within a tier the agent that has been blocked
///     longest is the one you owe.
///   * **No gesture ever decides anything.** There is no swipe in this app. An
///     accidental swipe approving `rm -rf` is the one bug that ends the product,
///     so advancing is a tap and answering is a button.
enum DeckOrdering {
    /// Every pending card across the fleet, **urgency-ranked**: HIGH → MEDIUM →
    /// LOW, then oldest first inside each tier.
    ///
    /// The first implementation ranked oldest-first; urgency wins instead, and
    /// the reason is visible on the fleet. Under age-first the accessory bar
    /// advertised `app-3 · Read · waiting 5m03s` — the least consequential card
    /// in the queue — while a `git push --force` sat above it, and the one line of
    /// detail the product volunteers at 2am was its least important item. Age
    /// remains the tiebreak, so nothing inside a tier can starve.
    ///
    /// Below the tier, ties break on time, then session, then request id, so
    /// that two cards created in the same millisecond do not swap places
    /// between renders — a stack that reshuffles under your thumb is a stack
    /// you cannot trust to tap.
    ///
    /// The profile is required rather than optional because the class the queue
    /// ranks by must be the same class the card gates on: `RiskAssessment`
    /// reads "absent means medium" only on a daemon that classifies, and a
    /// ranking that guessed differently would put a card in a tier its own
    /// badge disagrees with.
    static func sort(_ items: [ApprovalItem], profile: DaemonProfile) -> [ApprovalItem] {
        // One assessment per card rather than one per comparison: `resolve`
        // re-reads the tool input, and a sort does O(n log n) comparisons.
        let ranked = items.map { (item: $0, risk: $0.assessment(profile: profile).effective) }
        return ranked.sorted { lhs, rhs in
            if lhs.risk != rhs.risk { return lhs.risk > rhs.risk }
            if lhs.item.requestedAt != rhs.item.requestedAt {
                return lhs.item.requestedAt < rhs.item.requestedAt
            }
            if lhs.item.sessionKey != rhs.item.sessionKey {
                return lhs.item.sessionKey < rhs.item.sessionKey
            }
            return lhs.item.card.requestID < rhs.item.card.requestID
        }.map(\.item)
    }
}

/// One pass through the Deck.
///
/// Owns only what is true *about this pass* — what you have already dealt with,
/// and what you asked to come back to. The set of pending cards itself always
/// comes from the event log, so a card answered at the Mac while the Deck is
/// open disappears from the stack on its own.
/// Cards are tracked by `ApprovalItem.id` — the run *and* the request — rather
/// than by request id alone, because two runs can hold a card with the same
/// `request_id` and settling one of them must not silently settle the other.
struct DeckPass: Sendable, Equatable {
    /// Cards answered (or confirmed already-answered) during this pass.
    private(set) var settled: Set<String> = []
    /// Cards pushed to the back of this pass without being answered.
    /// Postponing is not deciding: they stay pending, and they stay in the
    /// queue.
    private(set) var deferred: [String] = []

    mutating func settle(_ cardID: String) {
        settled.insert(cardID)
        deferred.removeAll { $0 == cardID }
    }

    mutating func postpone(_ cardID: String) {
        guard !deferred.contains(cardID) else { return }
        deferred.append(cardID)
    }

    mutating func reset() {
        settled.removeAll()
        deferred.removeAll()
    }

    /// The queue as this pass sees it: still-pending cards, urgency-ranked, with
    /// anything you asked to revisit moved to the back in the order you deferred
    /// it.
    ///
    /// Settled cards are dropped here rather than waited out. A card the daemon
    /// has confirmed is answered stays `pending` in the log until the matching
    /// `approval_resolved` event arrives, and leaving it on top of the stack for
    /// that beat would invite a second answer to a decision already made.
    func arrange(_ pending: [ApprovalItem], profile: DaemonProfile) -> [ApprovalItem] {
        let ordered = DeckOrdering.sort(
            pending.filter { !settled.contains($0.id) }, profile: profile)
        guard !deferred.isEmpty else { return ordered }
        let deferredOrder = Dictionary(
            uniqueKeysWithValues: deferred.enumerated().map { ($0.element, $0.offset) })
        let front = ordered.filter { deferredOrder[$0.id] == nil }
        let back = ordered
            .filter { deferredOrder[$0.id] != nil }
            .sorted { deferredOrder[$0.id]! < deferredOrder[$1.id]! }
        return front + back
    }

    /// How many decisions this pass has actually resolved. Drives the "you
    /// cleared N" line on the finish state.
    var settledCount: Int { settled.count }
}
