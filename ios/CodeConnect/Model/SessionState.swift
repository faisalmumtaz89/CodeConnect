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
                "Resynced. The computer now tells its runs apart, so history cached under the old name was discarded."
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
    /// False for sample sessions: their review marks would persist in
    /// UserDefaults after the fleet itself is gone, and nothing could ever
    /// sweep them (the departed-session sweep only sees keys still in memory).
    let recordsReviewMarks: Bool

    private(set) var events: [Event] = [] { didSet { unpublished = true } }
    private(set) var timeline: [TimelineItem] = [] { didSet { unpublished = true } }
    /// The last event `timeline` was built from. The timeline is rebuilt a
    /// moment after events arrive, so `lastSeq` can be ahead of what is drawn.
    private(set) var drawnThrough: UInt64 = 0 { didSet { unpublished = true } }
    /// The newest prompt handed to the agent, built with `timeline`
    /// (`TimelineBuilder.lastPromptSeq`).
    private(set) var lastPromptSeq: UInt64 = 0 { didSet { unpublished = true } }
    /// When the on-disk copy was written. Non-nil only until live data arrives,
    /// so the UI can say "as of 6 minutes ago" and mean it.
    private(set) var loadedFromCacheAt: Date? { didSet { unpublished = true } }
    /// When *this launch* loaded that cache — the banner's grace clock, as
    /// distinct from `loadedFromCacheAt`, which is how old the data itself is.
    private(set) var cacheRestoredAt: Date? { didSet { unpublished = true } }
    private(set) var hasLiveData = false { didSet { unpublished = true } }
    private(set) var gap: GapNotice? { didSet { unpublished = true } }
    /// True when the top of the timeline is not the start of the session.
    var headTruncated: Bool { windowStart.map { $0 > 1 } ?? truncatedByBackfill }
    /// Set when an older daemon's bounded backfill started mid-log.
    private var truncatedByBackfill = false { didSet { unpublished = true } }
    /// **The first seq of the run of events this chat holds**, on a daemon that
    /// serves earlier pages; `nil` on one that does not. Everything from here to
    /// the tail is held, and earlier pages are asked for below it.
    private(set) var windowStart: UInt64? { didSet { unpublished = true } }
    /// **Every turn this session has seen end**, by the id its terminal carried.
    /// Never trimmed: the window can take a terminal away while keeping a later
    /// event of the same turn, or none at all, and a turn that ended stays ended.
    private(set) var endedTurns: Set<String> = [] { didSet { unpublished = true } }
    /// Counts window applications. A subscription that read the cache across one
    /// compares it, so it never lays its own, older window over a newer one.
    private(set) var windowEpoch = 0
    /// The earlier page asked for and not yet ended. One at a time.
    private(set) var pageRequest: PageRequest? { didSet { unpublished = true } }
    /// That page's events, held back until its `history_end` so it is merged and
    /// drawn once.
    private var pageBuffer: [Event] = []
    /// A page failed, so this window stops paging back on its own. Asking again
    /// is the reader's to do; a page that lands, or a new window, resumes it.
    private var fillStopped = false
    /// **Open cards whose request is older than the window**, by request id:
    /// the request event, fetched alone or kept as a trim took it, and the hold
    /// the Mac last listed for it (`SessionSummary.pending`). Never drawn in the
    /// chat; the Deck, the fleet, links and sheets read them.
    private(set) var pins: [String: Pin] = [:]
    /// The one card asked for and not yet ended. It shares the one request in
    /// flight with `pageRequest`.
    private(set) var pinRequest: PinRequest?
    private var pinBuffer: Event?
    /// Cards a fetch came back without: not asked again on this link and window.
    private var unpinnable: Set<PinRequest.Target> = []
    /// The built rows below the window: the pinned cards.
    private var pinnedItems: [TimelineItem] = [] { didSet { unpublished = true } }

    struct Pin: Sendable, Hashable {
        let event: Event
        var hold: QuestionHold?
        /// The card's resolution, once a window that held it moved past it: an
        /// answer the phone has shown stays shown.
        var resolution: Event?
    }

    struct PinRequest: Sendable, Hashable {
        struct Target: Sendable, Hashable {
            let requestID: String
            let seq: UInt64
        }
        /// Chosen here, echoed by the daemon on the fetch's end.
        let id: String
        let target: Target
        let hold: QuestionHold?
    }

    /// Told after every timeline rebuild, so the owner can ask for the page
    /// below a window that still has nothing to show.
    @ObservationIgnored var onTimelineRebuilt: (() -> Void)?
    /// Told as the rows above the reader are replaced — an earlier page
    /// commits, or the window is set afresh — with the timeline on screen and
    /// the one about to replace it, before the new one is published: the screen
    /// showing this chat reads where the reader is while it still can, and
    /// keeps them there once the new rows are drawn.
    @ObservationIgnored var onWindowChange:
        ((_ shown: [TimelineItem], _ next: [TimelineItem], _ change: WindowChange) -> Void)?

    /// Told as a catch-up's one change is about to replace the rows on screen,
    /// after `onWindowChange` and before it is published, with what is on
    /// screen and what replaces it: the screen lands it without animating and
    /// sets its "N new" once.
    @ObservationIgnored var onCatchUpLanding: ((_ shown: Shown, _ next: Shown) -> Void)?

    enum WindowChange {
        /// An earlier page was drawn above.
        case page
        /// The window was set afresh: trimmed to the newest events, or
        /// fetched again.
        case reset
    }

    /// Of two changes waiting for one publication, the one the screen must
    /// treat as having happened: a reset replaces whatever a page added.
    private static func stronger(_ held: WindowChange?, _ change: WindowChange) -> WindowChange {
        held == .reset ? .reset : change
    }

    /// **The window holds nothing the fleet counts as activity, and there is
    /// earlier history to look in.** The fleet's status and subtitle read the
    /// last substantive item, so a window without one pages back until it holds
    /// one or reaches the start.
    var needsEarlierPage: Bool {
        pagesBackOnItsOwn && FleetStatusRule.lastSubstantiveItem(in: timeline) == nil
    }

    /// **There is earlier history, and this window may still ask for it without
    /// the reader.** A failed page ends that until the reader asks again.
    var pagesBackOnItsOwn: Bool {
        guard let windowStart, windowStart > 1 else { return false }
        return !fillStopped
    }

    struct PageRequest: Sendable, Hashable {
        /// Chosen here, echoed by the daemon on the page's end.
        let id: String
        let beforeSeq: UInt64
        let limit: UInt64
    }
    private(set) var lastEventAt: Date? { didSet { unpublished = true } }
    /// The permission mode this run is operating under, tracked on ingest
    /// because the fleet reads it continuously and it changes about once a
    /// session.
    ///
    /// It is a *log-derived* fact, not a wire field, so it costs no protocol
    /// change and works against a daemon that predates it — which also means it is
    /// `nil` until the transcript says otherwise, and `nil` must never be rendered
    /// as a claim about anything.
    private(set) var permissionMode: String? { didSet { unpublished = true } }
    private(set) var permissionModeSeq: UInt64 = 0 { didSet { unpublished = true } }

    /// The last model this session *confirmed*, and how we know. Seeded by
    /// the SessionStart hook's `model` field; replaced whenever Claude Code's
    /// own local-command stdout says "Set model to X…" or "Kept model as X" —
    /// both are true about the model in force. Which verb it was is a property
    /// of the receipt, not of this fact: it lives on `ModelConfirmation`, and
    /// the sheet reads it from `lastModelCommandSignal`.
    /// Deliberately named "confirmed", never "current": a picker change made
    /// at the Mac's keyboard with `s` (session-only) may be unobservable,
    /// and this app does not present what it cannot know.
    private(set) var lastConfirmedModel: ConfirmedModel? { didSet { unpublished = true } }
    private(set) var lastConfirmedModelSeq: UInt64 = 0 { didSet { unpublished = true } }

    /// The last effort level Claude Code *confirmed* — its own "Set effort
    /// level to X…" or "Kept effort level as X" stdout, nothing else, with
    /// `isChange` recording which. Same honesty rule as the model fact: the
    /// sheet reports what the transcript said, never what a send hoped for.
    private(set) var lastConfirmedEffort: EffortConfirmation?
    /// Exposed so a sheet can fence a receipt against the stream position it
    /// captured when the row was tapped. A fact alone cannot say whether it is
    /// news: a backfill can publish a historical receipt into an empty slot.
    private(set) var lastConfirmedEffortSeq: UInt64 = 0

    /// The last thing `/model` said back — a receipt, or the measured
    /// `Model 'x' not found` — with the sequence that carried it. Kept apart
    /// from `lastConfirmedModel` because an error is not a fact about the
    /// session's model, and because the sheet correlates on the sequence.
    private(set) var lastModelCommandSignal: ModelCommandSignal?

    /// The last compaction outcome Claude Code reported — "Compacted (…)"
    /// or the measured refusal on a near-empty context.
    private(set) var lastCompactSignal: CompactConfirmation?
    /// Exposed for the same reason as `lastConfirmedEffortSeq`: a fact cannot
    /// say whether it is news, and a backfill can publish a historical one.
    private(set) var lastCompactSignalSeq: UInt64 = 0

    /// The newest command receipts as they arrived, for the sheets that wait on
    /// one. Never from `shown`: a receipt answers a send, and the sheet fences
    /// it against the stream position it captured.
    struct Receipts {
        let modelSignal: ModelCommandSignal?
        let effort: EffortConfirmation?
        let effortSeq: UInt64
        let compact: CompactConfirmation?
        let compactSeq: UInt64
    }

    var receipts: Receipts {
        Receipts(
            modelSignal: lastModelCommandSignal, effort: lastConfirmedEffort,
            effortSeq: lastConfirmedEffortSeq, compact: lastCompactSignal,
            compactSeq: lastCompactSignalSeq)
    }


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
    var silentBecauseOfPermissions: String? { Self.silentNotice(permissionMode: permissionMode) }

    static func silentNotice(permissionMode: String?) -> String? {
        permissionMode == "bypassPermissions"
            ? "Permissions bypassed - this run decides for itself and will not ask you"
            : nil
    }

    /// What one event says about the session's model, if anything.
    ///
    /// A `Kept model as X` receipt becomes a fact here just as `Set model to X`
    /// does — both are true statements of the model in force, and the cancelled
    /// one is the only way this app learns the model after somebody dismisses a
    /// confirmation at the Mac. This fact is only ever "the model in force";
    /// the verb, and therefore whether anything changed, is carried separately
    /// by `lastModelCommandSignal`.
    private static func modelFact(of event: Event) -> ConfirmedModel? {
        if event.kind == .sessionStart, let name = event.modelName, !name.isEmpty {
            return ConfirmedModel(name: name, provenance: .sessionStart, at: event.date)
        }
        if case .output(let line)? = event.localCommand,
            let receipt = ModelConfirmation.parse(line)
        {
            return ConfirmedModel(
                name: receipt.name,
                provenance: .command,
                at: event.date)
        }
        return nil
    }

    /// What one event says `/model` answered, if anything.
    private static func modelCommandSignal(of event: Event) -> ModelCommandSignal? {
        guard case .output(let line)? = event.localCommand,
            let outcome = ModelCommandOutcome.parse(line)
        else { return nil }
        return ModelCommandSignal(outcome: outcome, seq: event.seq)
    }

    private static func effortFact(of event: Event) -> EffortConfirmation? {
        guard case .output(let line)? = event.localCommand else { return nil }
        return EffortConfirmation.parse(line)
    }

    private static func compactFact(of event: Event) -> CompactConfirmation? {
        guard case .output(let line)? = event.localCommand else { return nil }
        return CompactConfirmation.parse(line)
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

    init(sessionKey: String, recordsReviewMarks: Bool = true) {
        self.sessionKey = sessionKey
        self.recordsReviewMarks = recordsReviewMarks
    }

    /// The highest `seq` held. Buffered events are by construction at or below
    /// the tail, so a pending merge can never make this understate what we have.
    var lastSeq: UInt64 { events.last?.seq ?? 0 }

    /// Every card in this run still waiting on a human.
    ///
    /// Tracked on rebuild rather than recomputed on read: the fleet and the
    /// Deck read it once a second and it changes about twice a session. As a computed property it walked the whole
    /// timeline per access, and `AppModel.deck` flat-maps it across every
    /// session the app holds — so one `body` pass on a 24-session fleet walked
    /// two dozen event logs, and `body` re-runs on the one-second tick. That is
    /// a standing battery and thermal cost for a value nothing changed, and it
    /// is the shape that defeats XCUITest's quiescence wait.
    private(set) var pendingApprovals: [ApprovalItem] = [] { didSet { unpublished = true } }

    /// The current approval for `id`, exactly as the live timeline holds it now —
    /// pending or resolved. `nil` when this session's log has no such card.
    ///
    /// **The live data source a still-open decision sheet re-derives from on
    /// every render.** Holding the opened `ApprovalItem` *value* freezes it at
    /// open-time, so a crash-recovery rebuild that resolves the card
    /// (`approval_resolved`, often `indeterminate`) is never reflected: the sheet
    /// keeps `outcome == nil`, renders a stale local attempt, and leaves the
    /// action bar live on a card the log has already resolved. Looking the card
    /// up here instead closes that staleness for *any* outcome, because
    /// `timeline` is rebuilt in the one place the log can change.
    func approval(id: String) -> ApprovalItem? {
        Self.approval(id: id, in: pinnedItems + timeline)
    }

    nonisolated private static func approval(id: String, in items: [TimelineItem]) -> ApprovalItem? {
        for item in items {
            if case .approval(let approval) = item.content, approval.id == id { return approval }
        }
        return nil
    }

    // MARK: - Presentation

    /// **Everything a chat, a fleet row, a card or a sheet draws of this run**,
    /// as of its last publication.
    ///
    /// The properties above are the truth: every send gate, validation and
    /// request reads them, and they change as events arrive. What is on screen
    /// changes only when `shown` is published, so a catch-up can bring the
    /// truth level with the Mac and put the result on screen in one change.
    struct Shown: Equatable {
        // The cheap fields come first: comparing two presentations stops at the
        // first difference, and the timeline is the costly one.
        var drawnThrough: UInt64 = 0
        var lastPromptSeq: UInt64 = 0
        var lastEventAt: Date?
        var gap: GapNotice?
        var windowStart: UInt64?
        var headTruncated = false
        /// An earlier page is on its way.
        var loadingPage = false
        var permissionMode: String?
        var permissionModeSeq: UInt64 = 0
        var confirmedModel: ConfirmedModel?
        var confirmedModelSeq: UInt64 = 0
        var hasLiveData = false
        var loadedFromCacheAt: Date?
        var cacheRestoredAt: Date?
        /// The Codex turn Stop would name, or nil.
        var runningTurn: String?
        /// The newest event the reader has seen this run through to.
        var reviewedSeq: UInt64 = 0
        var pendingApprovals: [ApprovalItem] = []
        var pinnedItems: [TimelineItem] = []
        var timeline: [TimelineItem] = []

        /// The card with this id as drawn, pending or resolved — what an open
        /// decision sheet renders from. See `SessionState.approval(id:)`.
        func approval(id: String) -> ApprovalItem? {
            SessionState.approval(id: id, in: pinnedItems + timeline)
        }
    }

    /// What is on screen for this run.
    private(set) var shown = Shown()
    /// While set, the truth changes and `shown` does not, except for the
    /// reader's own acts. `AppModel` holds a session for a catch-up.
    @ObservationIgnored var holdsPresentation = false
    /// The truth has changed since the last publication.
    @ObservationIgnored private var unpublished = false
    /// The strongest change to the rows above the reader since the last
    /// publication, for `onWindowChange`.
    @ObservationIgnored private var unpublishedChange: WindowChange?
    /// The turns a Codex mutation reported begun and ended, which
    /// `runningTurn` reads beside the events. `AppModel` supplies them.
    @ObservationIgnored var codexTurns: () -> (observed: [String], retired: Set<String>) = { ([], []) }

    /// What a publication now would put on screen.
    var current: Shown {
        let turns = codexTurns()
        return Shown(
            drawnThrough: drawnThrough, lastPromptSeq: lastPromptSeq, lastEventAt: lastEventAt,
            gap: gap,
            windowStart: windowStart, headTruncated: headTruncated,
            loadingPage: pageRequest != nil,
            permissionMode: permissionMode, permissionModeSeq: permissionModeSeq,
            confirmedModel: lastConfirmedModel, confirmedModelSeq: lastConfirmedModelSeq,
            hasLiveData: hasLiveData, loadedFromCacheAt: loadedFromCacheAt,
            cacheRestoredAt: cacheRestoredAt,
            runningTurn: CodexTurnTracker.runningTurn(
                in: events, observed: turns.observed, retired: turns.retired,
                knownEnded: endedTurns),
            reviewedSeq: ReviewMarks.reviewedSeq(for: sessionKey),
            pendingApprovals: pendingApprovals, pinnedItems: pinnedItems, timeline: timeline)
    }

    /// Put what changed on screen, unless the presentation is held. Called once
    /// at the end of each public entry point; the helpers they share only mark.
    private func publish() {
        guard unpublished, !holdsPresentation else { return }
        commitPresentation()
    }

    /// Put the truth on screen now, held or not. `asCatchUp` is the end of a
    /// catch-up: the one change that brings the screen level with the Mac.
    func commitPresentation(asCatchUp: Bool = false) {
        unpublished = false
        let next = current
        if let change = unpublishedChange {
            unpublishedChange = nil
            onWindowChange?(shown.timeline, next.timeline, change)
        }
        if asCatchUp, next.timeline != shown.timeline {
            onCatchUpLanding?(shown, next)
        }
        guard next != shown else { return }
        shown = next
        #if DEBUG
            publications.append(asCatchUp ? .release : publishing ?? .live)
        #endif
    }

    #if DEBUG
        /// Why `shown` changed, once per publication. A catch-up is promised to
        /// change the screen once, and a promise about publications with no way
        /// to count them is one that quietly stops holding. Debug builds only.
        enum Publication: Equatable {
            case live
            case release
            /// The page this request asked for committed.
            case page(String)
            case reviewed
            case gapDismissed
        }
        private(set) var publications: [Publication] = []
        @ObservationIgnored var publishing: Publication?
    #endif

    /// A Codex mutation reported a turn begun or ended.
    func codexTurnsChanged() {
        unpublished = true
        publish()
    }

    // MARK: - Ingest

    func loadCached(_ cached: EventCache.CachedEvents) {
        // Once a window is set it is the whole truth: the file is an older copy
        // of what it held, or of what it dropped for having a hole.
        guard events.isEmpty, pendingMerge.isEmpty, windowStart == nil else { return }
        events = cached.events
        noteEndedTurns(cached.events)
        loadedFromCacheAt = cached.cachedAt
        cacheRestoredAt = Date()
        lastEventAt = cached.events.last?.date
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
        if let effort = cached.events.reversed()
            .compactMap({ event in Self.effortFact(of: event).map { (event.seq, $0) } })
            .first
        {
            lastConfirmedEffort = effort.1
            lastConfirmedEffortSeq = effort.0
        }
        if let compact = cached.events.reversed()
            .compactMap({ event in Self.compactFact(of: event).map { (event.seq, $0) } })
            .first
        {
            lastCompactSignal = compact.1
            lastCompactSignalSeq = compact.0
        }
        lastModelCommandSignal = cached.events.reversed()
            .compactMap(Self.modelCommandSignal(of:)).first
        rebuildTimeline()
        publish()
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
    ///
    /// What it changes is published with the rebuild it schedules.
    func ingest(_ event: Event) {
        if !hasLiveData { hasLiveData = true }

        if event.kind == .resync {
            gap = GapNotice(cause: .daemonResync(skipped: event.resyncSkipped), at: Date())
            publish()
            return
        }

        // Below the window only the page or the card asked for is taken, and it
        // waits for its end. Anything else down there is a straggler from a
        // replay that began lower, and holding it would put a hole between it and
        // the window. A terminal is still a fact about its turn, held or not.
        if let windowStart, event.seq < windowStart {
            noteEndedTurns([event])
            if let page = pageRequest, event.seq + page.limit >= page.beforeSeq {
                pageBuffer.append(event)
            }
            if pinRequest?.target.seq == event.seq { pinBuffer = event }
            publish()
            return
        }

        if let last = events.last, event.seq <= last.seq {
            // Already held, or already queued: a replay resends what we have.
            guard !pendingSeqs.contains(event.seq), !holds(seq: event.seq) else {
                publish()
                return
            }
            pendingMerge.append(event)
            pendingSeqs.insert(event.seq)
        } else {
            if let last = events.last, event.seq > last.seq + 1 {
                noteMissing(last.seq + 1...event.seq - 1)
            }
            events.append(event)
            noteEndedTurns([event])
            lastEventAt = event.date
            if let mode = event.permissionModeChange {
                permissionMode = mode
                permissionModeSeq = event.seq
            }
            if let fact = Self.modelFact(of: event) {
                lastConfirmedModel = fact
                lastConfirmedModelSeq = event.seq
            }
            if let fact = Self.effortFact(of: event) {
                lastConfirmedEffort = fact
                lastConfirmedEffortSeq = event.seq
            }
            if let fact = Self.compactFact(of: event) {
                lastCompactSignal = fact
                lastCompactSignalSeq = event.seq
            }
            if let signal = Self.modelCommandSignal(of: event),
                signal.seq > (lastModelCommandSignal?.seq ?? 0)
            {
                lastModelCommandSignal = signal
            }
        }

        loadedFromCacheAt = nil
        cacheRestoredAt = nil
        // Once seq 1 is present the timeline really does start at the start.
        if events.first?.seq == 1 { truncatedByBackfill = false }
        scheduleRebuild()
    }

    /// Called when a bounded backfill deliberately skipped older events.
    func noteTruncatedHead() {
        truncatedByBackfill = true
        publish()
    }

    /// How many of a run's newest events a chat holds at launch and on return.
    nonisolated static let windowSize: UInt64 = 500

    /// Hold the newest `windowSize` of a log whose tail is `daemonLast`, and say
    /// where the subscription resumes.
    ///
    /// What is held is kept only if it covers everything from the window's first
    /// seq to its own tail; anything less would be a window with a hole in it, so
    /// it is all dropped and the window fetched afresh.
    ///
    /// `keepEarlier` is a reconnect while the app is open: the window and every
    /// page the reader scrolled up to stay, unless more than a window was missed,
    /// which is a fresh start like any other.
    ///
    /// An open card in `pending` whose request the window leaves below it stays
    /// a card: its event is kept as a pin, not dropped and fetched back.
    func applyWindow(daemonLast: UInt64, keepEarlier: Bool, pending: [PendingRef] = []) -> UInt64 {
        let floor = daemonLast > Self.windowSize ? daemonLast - Self.windowSize : 0
        windowEpoch += 1
        if keepEarlier, windowStart != nil, lastSeq >= floor { return lastSeq }
        rebuildTask?.cancel()
        rebuildTask = nil
        mergePending()
        let below = events.partitionPoint { $0.seq <= floor }
        let open = Dictionary(pending.map { ($0.seq, $0) }) { first, _ in first }
        for event in events[..<below] {
            if event.kind == .approvalRequest, let ref = open[event.seq] {
                pins[ref.requestID] = Pin(event: event, hold: ref.hold)
            } else if let id = Self.answeredRequest(of: event), pins[id] != nil {
                pins[id]?.resolution = event
            }
        }
        dropPin()
        let kept = events[below...]
        let covers =
            kept.first?.seq == floor + 1 && kept.last.map { $0.seq - floor } == UInt64(kept.count)
        events = covers ? Array(kept) : []
        // A page in flight was asked below the old window. It still fits a
        // window that starts where that one did; any other one it does not.
        if windowStart != floor + 1 { dropPage() }
        windowStart = floor + 1
        fillStopped = false
        // What remains is contiguous, so no hole the banner counted is still open.
        missingRanges.removeAll()
        if case .sequenceJump = gap?.cause { gap = nil }
        rebuildTimeline(changing: .reset)
        publish()
        return covers ? lastSeq : floor
    }

    /// The card an `approval_resolved` answers: Claude's outcome or Codex's.
    private static func answeredRequest(of event: Event) -> String? {
        guard event.kind == .approvalResolved else { return nil }
        return event.approvalOutcome?.requestID ?? event.codexResolvedRequestID
    }

    /// A pinned question's hold is the newest one held for it, or else the one
    /// the Mac listed — never the card's own, which is as old as the card.
    private func withPinnedHold(_ item: TimelineItem) -> TimelineItem {
        guard case .approval(var approval) = item.content,
            let hold = pins[approval.card.requestID]?.hold,
            !events.contains(where: { $0.questionHoldChange?.requestID == approval.card.requestID })
        else { return item }
        approval.questionHold = hold
        return TimelineItem(id: item.id, seq: item.seq, date: item.date, content: .approval(approval))
    }

    private func noteEndedTurns(_ taken: [Event]) {
        for event in taken where event.isTurnComplete {
            if let turn = event.turnID, !turn.isEmpty { endedTurns.insert(turn) }
        }
    }

    /// Ask for the page just below the window, unless one is already on its way
    /// or the window starts at the start.
    func beginPage() -> PageRequest? {
        guard pageRequest == nil, pinRequest == nil, let windowStart, windowStart > 1 else { return nil }
        let page = PageRequest(id: UUID().uuidString, beforeSeq: windowStart, limit: Self.windowSize)
        pageRequest = page
        publish()
        return page
    }

    /// The request `requestID` has ended: the page or the card asked for. A
    /// complete page is merged and drawn once and the window starts at its first
    /// event; a failed one changes nothing. An end for any other request is
    /// neither's, and is ignored.
    func endHistory(requestID: String, firstSeq: UInt64, failed: Bool) {
        if pinRequest?.id == requestID {
            endPin(failed: failed)
            return publish()
        }
        guard pageRequest?.id == requestID else { return }
        let page = pageBuffer
        dropPage()
        fillStopped = failed
        guard !failed else { return publish() }
        rebuildTask?.cancel()
        rebuildTask = nil
        for event in page where pendingSeqs.insert(event.seq).inserted {
            pendingMerge.append(event)
        }
        mergePending()
        windowStart = firstSeq
        // The window holds a card the page reached, so it is no longer pinned.
        pins = pins.filter { $0.value.event.seq < firstSeq }
        rebuildTimeline(changing: .page)
        #if DEBUG
            publishing = .page(requestID)
            defer { publishing = nil }
        #endif
        publish()
    }

    /// Forget the page in flight: the link it was asked on has gone, or the
    /// window it was below has been replaced.
    func cancelPage() {
        dropPage()
        publish()
    }

    private func dropPage() {
        pageRequest = nil
        pageBuffer.removeAll()
    }

    /// Ask for the request event of an open card the Mac lists below the window,
    /// unless a page or a card is already on its way, the card is pinned, or a
    /// fetch for it came back without it.
    func beginPin(_ ref: PendingRef) -> PinRequest? {
        let target = PinRequest.Target(requestID: ref.requestID, seq: ref.seq)
        guard pageRequest == nil, pinRequest == nil, let windowStart, ref.seq < windowStart,
            pins[ref.requestID] == nil, !unpinnable.contains(target)
        else { return nil }
        let request = PinRequest(id: UUID().uuidString, target: target, hold: ref.hold)
        pinRequest = request
        publish()
        return request
    }

    private func endPin(failed: Bool) {
        guard let request = pinRequest else { return }
        let event = pinBuffer
        pinRequest = nil
        pinBuffer = nil
        guard !failed, let event, event.kind == .approvalRequest else {
            unpinnable.insert(request.target)
            return
        }
        pins[request.target.requestID] = Pin(event: event, hold: request.hold)
        rebuildTimeline()
    }

    /// Keep the pinned cards `pending` still lists, with the hold it gives them
    /// now, and those whose answer the phone holds, which a sheet may be
    /// showing; drop the rest: the Mac has resolved them.
    ///
    /// Read against the list the window has caught up with, so an answer the
    /// Mac recorded before it is held first.
    func keepPins(_ pending: [PendingRef]) {
        let open = Dictionary(pending.map { ($0.requestID, $0) }) { first, _ in first }
        let unlisted = pins.contains { open[$0.key]?.seq != $0.value.event.seq }
        let answered = unlisted ? Set(events.compactMap(Self.answeredRequest)) : []
        var kept = pins.filter {
            open[$0.key]?.seq == $0.value.event.seq || $0.value.resolution != nil
                || answered.contains($0.key)
        }
        for id in kept.keys { kept[id]?.hold = open[id]?.hold }
        guard kept != pins else { return }
        pins = kept
        rebuildTimeline()
        publish()
    }

    /// Whether the open card `ref` is held: in the window, pinned, or fetched
    /// and found missing. A window that does not page holds what it holds.
    func holds(card ref: PendingRef) -> Bool {
        guard let windowStart else { return true }
        return ref.seq >= windowStart || pins[ref.requestID] != nil
            || unpinnable.contains(PinRequest.Target(requestID: ref.requestID, seq: ref.seq))
    }

    /// Forget the card in flight, and the fetches that came back empty: the
    /// link they were asked on has gone, or the window has been set afresh, and
    /// the next may answer.
    func cancelPin() {
        dropPin()
        publish()
    }

    private func dropPin() {
        pinRequest = nil
        pinBuffer = nil
        unpinnable.removeAll()
    }

    /// The daemon has just reported a `last_seq` we already hold, so what came
    /// off disk *is* the daemon's current view — not a stale guess about it.
    /// Understating what we know is its own kind of dishonesty.
    func noteConfirmedCurrent() {
        guard lastSeq > 0 else { return }
        if !hasLiveData { hasLiveData = true }
        if loadedFromCacheAt != nil { loadedFromCacheAt = nil }
        if cacheRestoredAt != nil { cacheRestoredAt = nil }
        publish()
    }

    /// The daemon's log no longer contains what we cached; start clean rather
    /// than showing a history that cannot be reconciled.
    func resetForRewoundLog() {
        reset(gap: GapNotice(cause: .logRewound, at: Date()))
        publish()
    }

    /// The cache was filed under a tmux name and this daemon identifies runs
    /// individually, so nothing on disk could be attributed to this run.
    func noteCacheDiscarded() {
        // Only worth saying while the timeline is still empty. Once live events
        // are in, the banner would be describing a state the screen has already
        // moved past.
        guard events.isEmpty, gap == nil else { return }
        gap = GapNotice(cause: .cacheDiscarded, at: Date())
        publish()
    }

    private func reset(gap notice: GapNotice?) {
        events.removeAll()
        timeline.removeAll()
        drawnThrough = 0
        lastPromptSeq = 0
        // Cleared with the timeline it is a projection of. Left behind, a
        // rewound/reset log would keep feeding stale cards to the Deck
        // (`AppModel.deck` flat-maps this) and to any open sheet's live lookup —
        // an actionable card no daemon would accept an answer for.
        pendingApprovals.removeAll()
        pendingMerge.removeAll()
        pendingSeqs.removeAll()
        missingRanges.removeAll()
        loadedFromCacheAt = nil
        cacheRestoredAt = nil
        lastEventAt = nil
        truncatedByBackfill = false
        windowStart = nil
        dropPage()
        dropPin()
        pins.removeAll()
        pinnedItems.removeAll()
        permissionMode = nil
        permissionModeSeq = 0
        lastConfirmedModel = nil
        lastConfirmedModelSeq = 0
        lastConfirmedEffort = nil
        lastConfirmedEffortSeq = 0
        lastCompactSignal = nil
        lastCompactSignalSeq = 0
        lastModelCommandSignal = nil
        gap = notice
    }

    /// Acknowledged: the notice goes, and so does the record of what was missing.
    /// Re-raising a hole the reader has already dismissed would be nagging, not
    /// honesty.
    ///
    /// The reader's own act, so the banner goes at once, held or not.
    func dismissGap() {
        gap = nil
        missingRanges.removeAll()
        guard shown.gap != nil else { return }
        shown.gap = nil
        #if DEBUG
            publications.append(.gapDismissed)
        #endif
    }

    /// The reader has seen this run through what is on screen — never further,
    /// whatever has arrived and not been drawn. Their own act, so the mark is
    /// shown at once, held or not.
    func markReviewed() {
        // Sample sessions must not write marks: the store is UserDefaults, its
        // eviction keeps the highest-sorting 200 keys, and `fx-*` sorts above
        // every ULID — a sample browse would take permanent slots from real
        // runs, announcing work the user already read as new.
        guard recordsReviewMarks else { return }
        ReviewMarks.markReviewed(sessionKey: sessionKey, seq: shown.drawnThrough)
        let reviewed = ReviewMarks.reviewedSeq(for: sessionKey)
        guard shown.reviewedSeq != reviewed else { return }
        shown.reviewedSeq = reviewed
        #if DEBUG
            publications.append(.reviewed)
        #endif
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
        publish()
    }

    /// Draws the timeline now instead of at the end of the coalescing window.
    ///
    /// For the moment the fleet is first shown: a reveal that waited for every
    /// session to catch up must not then paint its rows a beat later.
    func rebuildNow() {
        guard rebuildTask != nil || !pendingMerge.isEmpty else { return }
        rebuildTask?.cancel()
        rebuildTask = nil
        mergePending()
        rebuildTimeline()
        publish()
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

    #if DEBUG
        /// Waits until any scheduled rebuild has actually run. Tests await
        /// this fact instead of sleeping some multiple of the coalesce
        /// window: the rebuild task starts whenever the scheduler gets to
        /// it, so on a starved machine every fixed sleep is a bet the
        /// machine eventually loses.
        func settleForTesting() async {
            while let task = rebuildTask {
                _ = await task.value
            }
        }
    #endif

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
            self.publish()
        }
    }

    /// One sorted merge of everything a replay delivered below the tail.
    private func mergePending() {
        guard !pendingMerge.isEmpty else { return }
        let incoming = pendingMerge.sorted { $0.seq < $1.seq }
        pendingMerge.removeAll(keepingCapacity: true)
        pendingSeqs.removeAll(keepingCapacity: true)
        events = Self.merged(events, incoming)
        noteEndedTurns(incoming)

        // A replayed event can carry the only mode change in the log — the
        // run's first turn is below the backfill window — but must never
        // displace a newer one.
        for event in incoming where event.seq > permissionModeSeq {
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
        for event in incoming where event.seq > lastConfirmedEffortSeq {
            if let fact = Self.effortFact(of: event) {
                lastConfirmedEffort = fact
                lastConfirmedEffortSeq = event.seq
            }
        }
        for event in incoming where event.seq > lastCompactSignalSeq {
            if let fact = Self.compactFact(of: event) {
                lastCompactSignal = fact
                lastCompactSignalSeq = event.seq
            }
        }
        for event in incoming where event.seq > (lastModelCommandSignal?.seq ?? 0) {
            if let signal = Self.modelCommandSignal(of: event) {
                lastModelCommandSignal = signal
            }
        }
        if events.first?.seq == 1 { truncatedByBackfill = false }
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

    private func rebuildTimeline(changing change: WindowChange? = nil) {
        // One build over the pinned cards and the window, so a resolution in the
        // window closes a pinned card and the window's rows know the card is
        // there; the chat draws only the window's rows.
        let built = TimelineBuilder.build(
            pins.values.flatMap { [$0.event] + ($0.resolution.map { [$0] } ?? []) }
                .sorted { $0.seq < $1.seq } + events)
        let start = windowStart ?? 0
        let next = pins.isEmpty ? built : built.filter { $0.seq >= start }
        pinnedItems = pins.isEmpty ? [] : built.filter { $0.seq < start }.map(withPinnedHold)
        if let change { unpublishedChange = Self.stronger(unpublishedChange, change) }
        timeline = next
        drawnThrough = lastSeq
        lastPromptSeq = TimelineBuilder.lastPromptSeq(events)
        // Derived here, in the one place the timeline can change, so the cached
        // copy cannot go stale — a Deck holding a card the log has resolved
        // would invite a second answer to a decision already made.
        pendingApprovals = (pinnedItems + timeline).compactMap(\.pendingApproval)
        #if DEBUG
            rebuildCount += 1
        #endif
        onTimelineRebuilt?()
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
