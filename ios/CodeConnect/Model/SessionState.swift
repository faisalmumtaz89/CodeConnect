import Foundation
import Observation

/// Why the client cannot claim to have shown everything.
struct GapNotice: Sendable, Hashable {
    enum Cause: Sendable, Hashable {
        /// The daemon told us it could not replay gap-free.
        case daemonResync(skipped: Int?)
        /// A `seq` arrived that did not follow the previous one. The daemon
        /// assigns `seq` as `MAX(seq)+1` *after* its dedup check
        /// (`ccd/src/store.rs` `append_in_tx`), so the log is contiguous by
        /// construction and a hole can only mean events we never received.
        case sequenceJump(missing: Int)
        /// The daemon's log is shorter than our cache: it was reset behind us,
        /// so everything cached is about a log that no longer exists.
        case logRewound
        /// The daemon now mints a `session_uid` per run, so the cache — which
        /// was filed under the tmux name, and therefore under whichever runs
        /// held that name — could not be attributed to this run and was
        /// discarded. The timeline below is a fresh read from the daemon.
        case cacheDiscarded
    }

    var cause: Cause
    var at: Date

    var message: String {
        switch cause {
        case .daemonResync(let skipped):
            if let skipped {
                return "Resynced. The daemon fell \(skipped) events behind and replayed the log."
            }
            return "Resynced. The daemon replayed the log."
        case .sequenceJump(let missing):
            return "Resynced, \(missing) event\(missing == 1 ? "" : "s") not shown."
        case .logRewound:
            return "Resynced. The daemon's log was reset; cached history was discarded."
        case .cacheDiscarded:
            return
                "Resynced. This Mac now tells its runs apart, so history cached under the old name was discarded."
        }
    }
}

/// Everything the app knows about one session, and how old it is.
@MainActor
@Observable
final class SessionState {
    /// What this run is filed under: its `session_uid` where the daemon mints
    /// one, its tmux name otherwise. Never the display name — see
    /// `SessionSummary.sessionKey`.
    let sessionKey: String

    private(set) var events: [Event] = []
    private(set) var timeline: [TimelineItem] = []
    /// When the on-disk copy was written. Non-nil only until live data arrives,
    /// so the UI can say "as of 6 minutes ago" and mean it.
    private(set) var loadedFromCacheAt: Date?
    /// When *this launch* loaded that cache — the banner's grace clock, as
    /// distinct from `loadedFromCacheAt`, which is how old the data itself is.
    private(set) var cacheRestoredAt: Date?
    private(set) var hasLiveData = false
    private(set) var gap: GapNotice?
    /// True when the backfill started mid-log, so the top of the timeline is
    /// not the start of the session.
    private(set) var headTruncated = false
    private(set) var lastEventAt: Date?
    /// Claude's own generated title for the run. Tracked on ingest because the
    /// fleet reads it once a second, and rescanning the log for it each time is
    /// a linear cost for a value that changes about twice a session.
    private(set) var aiTitle: String?
    /// Which event `aiTitle` came from, so a *replayed older* event can never
    /// overwrite a newer title. Replay makes that a real ordering, not a
    /// hypothetical one: "load all" re-delivers the whole log after the tail.
    private var aiTitleSeq: UInt64 = 0
    /// The permission mode this run is operating under, tracked for the same
    /// reason as `aiTitle`: the fleet reads it continuously and it changes about
    /// once a session.
    ///
    /// It is a *log-derived* fact, not a wire field, so it costs no protocol
    /// change and works against a daemon that predates it — which also means it is
    /// `nil` until the transcript says otherwise, and `nil` must never be rendered
    /// as a claim about anything.
    private(set) var permissionMode: String?
    private var permissionModeSeq: UInt64 = 0

    /// The last model this session *confirmed*, and how we know. Seeded by
    /// the SessionStart hook's `model` field; replaced whenever Claude Code's
    /// own local-command stdout says "Set model to X…" or "Kept model as X".
    /// Deliberately named "confirmed", never "current": a picker change made
    /// at the Mac's keyboard with `s` (session-only) may be unobservable,
    /// and this app does not present what it cannot know.
    private(set) var lastConfirmedModel: ConfirmedModel?
    private var lastConfirmedModelSeq: UInt64 = 0

    /// What to tell the reader when this run will never ask them anything.
    ///
    /// `nil` for every mode that can still raise a card, including an unknown one:
    /// a run whose transcript has not yet said which mode it is in gets no notice
    /// at all, because "we do not know yet" and "nothing will be sent" are
    /// different facts and only one of them is worth a line on screen.
    ///
    /// **The wording is deliberately about consequence, not configuration.** The
    /// reader does not need to be taught what `bypassPermissions` means; they need
    /// to know that waiting for this session to ask them something is waiting for
    /// nothing. Everything else the app does for this run — the timeline, the
    /// diff, taking the keyboard — still works, so the line says what stops, not
    /// that the session is degraded.
    var silentBecauseOfPermissions: String? {
        permissionMode == "bypassPermissions"
            ? "Permissions bypassed - this run decides for itself and will not ask you"
            : nil
    }

    /// What one event says about the session's model, if anything.
    private static func modelFact(of event: Event) -> ConfirmedModel? {
        if event.kind == .sessionStart, let name = event.modelName, !name.isEmpty {
            return ConfirmedModel(name: name, source: "Session start", at: event.date)
        }
        if case .output(let line)? = event.localCommand,
            let name = ModelConfirmation.parse(line)
        {
            return ConfirmedModel(name: name, source: "Command confirmation", at: event.date)
        }
        return nil
    }

    /// Events that arrived at or below the tail — a replay — held back until the
    /// coalescing window closes. See `ingest`.
    private var pendingMerge: [Event] = []
    private var pendingSeqs: Set<UInt64> = []
    /// Sequence numbers this client knows it never received. Kept so the gap
    /// banner can be *withdrawn* when a later replay fills the hole: a warning
    /// that stays up after the reason for it is gone teaches people to ignore
    /// warnings.
    private var missingRanges: [ClosedRange<UInt64>] = []

    private var rebuildTask: Task<Void, Never>?

    /// How long ingest waits before rebuilding.
    ///
    /// A replay arrives one WebSocket frame at a time, each on its own turn of
    /// the main actor, so "rebuild on the next hop" coalesced almost nothing: a
    /// 400-event backfill did 400 array merges and 400 full timeline builds.
    /// One frame at 60Hz is below the threshold where a person can see the
    /// difference on a single live event, and it turns that replay into a
    /// handful of passes.
    ///
    /// The contract this states, and that `SessionIdentityTests` holds it to: at
    /// most one merge and one timeline build per window, however many events
    /// arrive inside it.
    static let coalesceWindow: Duration = .milliseconds(16)

    init(sessionKey: String) {
        self.sessionKey = sessionKey
    }

    /// The highest `seq` held. Buffered events are by construction at or below
    /// the tail, so a pending merge can never make this understate what we have.
    var lastSeq: UInt64 { events.last?.seq ?? 0 }

    /// Every card in this run still waiting on a human.
    ///
    /// Tracked on rebuild rather than recomputed on read, for the same reason
    /// `aiTitle` is: the fleet and the Deck read it once a second and it changes
    /// about twice a session. As a computed property it walked the whole
    /// timeline per access, and `AppModel.deck` flat-maps it across every
    /// session the app holds — so one `body` pass on a 24-session fleet walked
    /// two dozen event logs, and `body` re-runs on the one-second tick. That is
    /// a standing battery and thermal cost for a value nothing changed, and it
    /// is the shape that defeats XCUITest's quiescence wait.
    private(set) var pendingApprovals: [ApprovalItem] = []

    // MARK: - Ingest

    func loadCached(_ cached: EventCache.CachedEvents) {
        guard events.isEmpty, pendingMerge.isEmpty else { return }
        events = cached.events
        loadedFromCacheAt = cached.cachedAt
        cacheRestoredAt = Date()
        lastEventAt = cached.events.last?.date
        if let titled = cached.events.reversed().first(where: { $0.aiTitle != nil }) {
            aiTitle = titled.aiTitle
            aiTitleSeq = titled.seq
        }
        if let moded = cached.events.reversed().first(where: { $0.permissionModeChange != nil }) {
            permissionMode = moded.permissionModeChange
            permissionModeSeq = moded.seq
        }
        if let confirmed = cached.events.reversed()
            .compactMap({ event in Self.modelFact(of: event).map { (event.seq, $0) } })
            .first
        {
            lastConfirmedModel = confirmed.1
            lastConfirmedModelSeq = confirmed.0
        }
        rebuildTimeline()
    }

    /// The daemon streams per session in ascending `seq`, but a "load earlier"
    /// re-subscribe deliberately resends what we already have, so ingest must be
    /// an idempotent merge rather than an append.
    ///
    /// The two cases are kept apart on purpose. A new event at the tail is an
    /// append — O(1), and the common case by far. Anything at or below the tail
    /// is a replay: it is buffered and merged in one pass when the window
    /// closes, because inserting each one in turn is a memmove of the whole
    /// array per event and a full-log replay would pay that hundreds of times.
    func ingest(_ event: Event) {
        hasLiveData = true

        if event.kind == .resync {
            gap = GapNotice(cause: .daemonResync(skipped: event.resyncSkipped), at: Date())
            return
        }

        if let last = events.last, event.seq <= last.seq {
            // Already held, or already queued: a replay resends what we have.
            guard !pendingSeqs.contains(event.seq), !holds(seq: event.seq) else { return }
            pendingMerge.append(event)
            pendingSeqs.insert(event.seq)
        } else {
            if let last = events.last, event.seq > last.seq + 1 {
                noteMissing(last.seq + 1...event.seq - 1)
            }
            events.append(event)
            lastEventAt = event.date
            if let title = event.aiTitle {
                aiTitle = title
                aiTitleSeq = event.seq
            }
            if let mode = event.permissionModeChange {
                permissionMode = mode
                permissionModeSeq = event.seq
            }
            if let fact = Self.modelFact(of: event) {
                lastConfirmedModel = fact
                lastConfirmedModelSeq = event.seq
            }
        }

        loadedFromCacheAt = nil
        cacheRestoredAt = nil
        // Once seq 1 is present the timeline really does start at the start.
        if events.first?.seq == 1 { headTruncated = false }
        scheduleRebuild()
    }

    /// Called when a bounded backfill deliberately skipped older events.
    func noteTruncatedHead() {
        headTruncated = true
    }

    /// The daemon has just reported a `last_seq` we already hold, so what came
    /// off disk *is* the daemon's current view — not a stale guess about it.
    /// Understating what we know is its own kind of dishonesty.
    func noteConfirmedCurrent() {
        guard lastSeq > 0 else { return }
        hasLiveData = true
        loadedFromCacheAt = nil
        cacheRestoredAt = nil
    }

    /// The daemon's log no longer contains what we cached; start clean rather
    /// than showing a history that cannot be reconciled.
    func resetForRewoundLog() {
        reset(gap: GapNotice(cause: .logRewound, at: Date()))
    }

    /// The cache was filed under a tmux name and this daemon identifies runs
    /// individually, so nothing on disk could be attributed to this run.
    func noteCacheDiscarded() {
        // Only worth saying while the timeline is still empty. Once live events
        // are in, the banner would be describing a state the screen has already
        // moved past.
        guard events.isEmpty, gap == nil else { return }
        gap = GapNotice(cause: .cacheDiscarded, at: Date())
    }

    private func reset(gap notice: GapNotice?) {
        events.removeAll()
        timeline.removeAll()
        pendingMerge.removeAll()
        pendingSeqs.removeAll()
        missingRanges.removeAll()
        loadedFromCacheAt = nil
        cacheRestoredAt = nil
        lastEventAt = nil
        headTruncated = false
        aiTitle = nil
        aiTitleSeq = 0
        permissionMode = nil
        permissionModeSeq = 0
        lastConfirmedModel = nil
        lastConfirmedModelSeq = 0
        gap = notice
    }

    /// Acknowledged: the notice goes, and so does the record of what was missing.
    /// Re-raising a hole the reader has already dismissed would be nagging, not
    /// honesty.
    func dismissGap() {
        gap = nil
        missingRanges.removeAll()
    }

    func markReviewed() {
        ReviewMarks.markReviewed(sessionKey: sessionKey, seq: lastSeq)
    }

    /// Merge and rebuild now rather than at the end of the coalescing window.
    ///
    /// For the two callers that need `events` to be the whole truth at a
    /// particular instant: writing the cache, and going to the background — a
    /// snapshot taken mid-window would persist a timeline missing the replay
    /// that was in flight.
    func settlePendingEvents() {
        // Nothing buffered means `events` is already the whole truth, and the
        // timeline is derived rather than persisted — so there is no reason to
        // pay for a rebuild here. That matters: this runs on every debounced
        // cache write, which is every couple of seconds per busy session.
        guard !pendingMerge.isEmpty else { return }
        rebuildTask?.cancel()
        rebuildTask = nil
        mergePending()
        rebuildTimeline()
    }

    // MARK: - Gaps

    private func noteMissing(_ range: ClosedRange<UInt64>) {
        missingRanges.append(range)
        missingRanges.removeAll { missingCount(in: $0) == 0 }
        gap = GapNotice(cause: .sequenceJump(missing: totalMissing), at: Date())
    }

    /// How many events are still missing, counted from what is actually held
    /// rather than from what was missing when the hole opened.
    private var totalMissing: Int {
        missingRanges.reduce(0) { $0 + Int(missingCount(in: $1)) }
    }

    /// Drop the ranges a replay has filled, and withdraw or restate the banner.
    private func reconcileGap() {
        guard !missingRanges.isEmpty else { return }
        missingRanges.removeAll { missingCount(in: $0) == 0 }
        guard case .sequenceJump = gap?.cause else { return }
        if missingRanges.isEmpty {
            gap = nil
        } else {
            // The same notice, restated: the *detection* time is when the hole
            // opened, not when the last part of it was filled.
            gap = GapNotice(cause: .sequenceJump(missing: totalMissing), at: gap?.at ?? Date())
        }
    }

    private func holds(seq: UInt64) -> Bool {
        let index = events.partitionPoint { $0.seq < seq }
        return index < events.count && events[index].seq == seq
    }

    /// `events` is sorted and unique by `seq`, so how much of a range is held is
    /// the distance between the two ends of it — no scan required.
    private func missingCount(in range: ClosedRange<UInt64>) -> UInt64 {
        let lower = events.partitionPoint { $0.seq < range.lowerBound }
        let upper = events.partitionPoint { $0.seq <= range.upperBound }
        return (range.upperBound - range.lowerBound + 1) - UInt64(upper - lower)
    }

    // MARK: - Timeline

    /// Rebuilding is a pure function of `events`, so a burst — a 500-event
    /// replay page — collapses into a single pass instead of one pass per event.
    private func scheduleRebuild() {
        guard rebuildTask == nil else { return }
        rebuildTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: Self.coalesceWindow)
            guard let self, !Task.isCancelled else { return }
            self.rebuildTask = nil
            self.mergePending()
            self.rebuildTimeline()
        }
    }

    /// One sorted merge of everything a replay delivered below the tail.
    private func mergePending() {
        guard !pendingMerge.isEmpty else { return }
        let incoming = pendingMerge.sorted { $0.seq < $1.seq }
        pendingMerge.removeAll(keepingCapacity: true)
        pendingSeqs.removeAll(keepingCapacity: true)
        events = Self.merged(events, incoming)

        // A replayed event can carry the only title in the log — the run's first
        // turn is below the backfill window — but must never displace a newer
        // one.
        for event in incoming where event.seq > aiTitleSeq {
            if let title = event.aiTitle {
                aiTitle = title
                aiTitleSeq = event.seq
            }
            if let mode = event.permissionModeChange {
                permissionMode = mode
                permissionModeSeq = event.seq
            }
        }
        for event in incoming where event.seq > lastConfirmedModelSeq {
            if let fact = Self.modelFact(of: event) {
                lastConfirmedModel = fact
                lastConfirmedModelSeq = event.seq
            }
        }
        if events.first?.seq == 1 { headTruncated = false }
        reconcileGap()
    }

    /// Merge two `seq`-ascending, `seq`-unique runs into one. Where both hold a
    /// `seq`, the one already ingested wins: it was there first and the daemon's
    /// log is immutable, so a replayed copy is the same fact.
    static func merged(_ existing: [Event], _ incoming: [Event]) -> [Event] {
        var out: [Event] = []
        out.reserveCapacity(existing.count + incoming.count)
        var left = existing.startIndex
        var right = incoming.startIndex
        while left < existing.endIndex, right < incoming.endIndex {
            let lhs = existing[left]
            let rhs = incoming[right]
            if lhs.seq < rhs.seq {
                out.append(lhs)
                left += 1
            } else if lhs.seq > rhs.seq {
                out.append(rhs)
                right += 1
            } else {
                out.append(lhs)
                left += 1
                right += 1
            }
        }
        out.append(contentsOf: existing[left...])
        out.append(contentsOf: incoming[right...])
        return out
    }

    #if DEBUG
        /// How many times the timeline has been rebuilt. The coalescing above is
        /// a *performance* contract — a replay must not cost one full rebuild
        /// per frame — and a contract with no way to observe it is one that
        /// quietly stops holding. Debug builds only.
        private(set) var rebuildCount = 0
    #endif

    private func rebuildTimeline() {
        timeline = TimelineBuilder.build(events)
        // Derived here, in the one place the timeline can change, so the cached
        // copy cannot go stale — a Deck holding a card the log has resolved
        // would invite a second answer to a decision already made.
        pendingApprovals = timeline.compactMap(\.pendingApproval)
        #if DEBUG
            rebuildCount += 1
        #endif
    }
}

extension Array {
    /// Index of the first element for which `predicate` is false, assuming the
    /// array is partitioned by it.
    func partitionPoint(_ predicate: (Element) -> Bool) -> Int {
        var low = 0
        var high = count
        while low < high {
            let mid = low + (high - low) / 2
            if predicate(self[mid]) {
                low = mid + 1
            } else {
                high = mid
            }
        }
        return low
    }
}
