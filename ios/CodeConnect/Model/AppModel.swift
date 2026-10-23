import Foundation
import Observation
import SwiftUI

/// What happened to an answer, in terms the UI can be honest about.
enum AnswerAttempt: Sendable {
    case applied(AnswerOutcome)
    /// The daemon accepted the answer but **cannot confirm it landed** —
    /// `AnswerOutcome.indeterminate`. Kept apart from `.applied` so nothing ever
    /// renders an unconfirmed answer as "confirmed by the daemon": a positive
    /// actuation claim the daemon never made.
    case indeterminate(AnswerOutcome)
    /// The ledger already had this request; carries the *original* outcome.
    case duplicate(outcome: AnswerOutcome, staleHash: Bool)
    /// The prompt was gone by the time the keystrokes were about to land —
    /// on this build that means a human answered it at the Mac.
    case answeredAtKeyboard(String)
    case staleCard(String)
    case rejected(String)
    case failed(String)

    var isTerminal: Bool {
        switch self {
        case .applied, .indeterminate, .duplicate, .answeredAtKeyboard: return true
        case .staleCard, .rejected, .failed: return false
        }
    }

    /// Turn a daemon `AnswerResult.applied` outcome into the honest attempt.
    ///
    /// An `indeterminate` outcome means the daemon does not know whether the
    /// answer reached the agent, so it must never become `.applied` — the case
    /// the card renders as "confirmed". Pure and static precisely so this rule
    /// can be tested without a live connection.
    static func classify(applied outcome: AnswerOutcome) -> AnswerAttempt {
        outcome.indeterminate ? .indeterminate(outcome) : .applied(outcome)
    }

    /// Turn a daemon `AnswerResult.duplicate` outcome into the honest attempt.
    ///
    /// **This is the real path an indeterminate outcome arrives on.** The daemon
    /// records the outcome under `(session, request_id)` and *replays* it on any
    /// later answer for the same key (`ccd/src/state.rs`), so a locally-resolved,
    /// never-confirmed answer comes back as a `duplicate` carrying
    /// `indeterminate: true`. Rendering that as "Already answered" with a
    /// checkmark is the same false-confirmation the app forbids, so an
    /// indeterminate duplicate is reported as `.indeterminate`, never `.duplicate`.
    ///
    /// Except an answer sent to Claude: that outcome is an earlier tap's, maybe
    /// another phone's, and nothing from this tap was sent, so it stays a
    /// duplicate.
    static func classify(duplicate outcome: AnswerOutcome, staleHash: Bool) -> AnswerAttempt {
        outcome.indeterminate && !outcome.wasSentToClaude
            ? .indeterminate(outcome) : .duplicate(outcome: outcome, staleHash: staleHash)
    }
}

enum ComposeAttempt: Sendable, Equatable {
    case sent(matched: String)
    case refused(String)
    case failed(String)
    /// This exact text already landed once — the retry was recognised and
    /// replayed, not typed twice. As final as `.sent`.
    case alreadyApplied(appliedAt: String)
    /// Nobody knows whether it was typed. The composer keeps the text and the
    /// same identity, so an explicit re-send is a recognisable retry instead
    /// of a second typing.
    case indeterminate(String)
    /// It landed, it opened a view on the Mac, and CodeConnect closed the
    /// view again. As final as `.sent`.
    case composerRecovered(command: String, paneSnapshot: String?, capturedAt: String)
    /// It landed, it opened a view, and one Escape did not close it.
    case composerLost(command: String)
}

/// Why a diff request ended in nothing — and, decisively, **who said so**.
///
/// The origin is carried, not inferred, because the diff screen prints one of
/// these strings under a caption that promises *the daemon's own reason,
/// verbatim*. A string that never crossed the wire cannot be shown there.
/// Measured failure this exists to make unconstructible: a cold launch through
/// `codeconnect://session/<uid>/diff` raced the socket, `loadDiff` failed with
/// `LinkHealth`'s own sentence, and the sheet rendered
/// `Connecting to the daemon…` as a quotation from the Mac.
enum DiffFailure: Sendable, Equatable {
    /// A string the daemon put on the wire. The only kind that may be quoted.
    case daemon(String)
    /// The app's own account of itself — the link's health, a local timeout, a
    /// transport error. True, and never the daemon's words.
    case app(String)

    var reason: String {
        switch self {
        case .daemon(let reason), .app(let reason): return reason
        }
    }

    /// May this string be printed as a quotation from the Mac?
    var isFromDaemon: Bool {
        if case .daemon = self { return true }
        return false
    }
}

/// The state of one session's on-demand diff.
enum DiffState: Sendable {
    case idle
    case loading
    case loaded(SessionDiff, parsed: UnifiedDiff, fetchedAt: Date)
    case failed(DiffFailure)

    var isLoading: Bool {
        if case .loading = self { return true }
        return false
    }

    /// How many files have changed, where the app has counted them: a clean
    /// tree — a diff with no text at all — is a counted 0. Nil until a diff has
    /// loaded, and for one with no file sections that is not a clean tree: the
    /// daemon's note says it is not a git repository, the capture was cut off,
    /// or the text is something this app does not count, such as `ccd`'s
    /// listing of untracked files.
    var changedFileCount: Int? {
        guard case .loaded(let diff, let parsed, _) = self else { return nil }
        if !parsed.files.isEmpty { return parsed.files.count }
        return parsed.isEmpty && diff.note == nil && !diff.truncated ? 0 : nil
    }

    /// A request that ended in the app's own account of the link, rather than
    /// in anything the daemon said. The screen has a state of its own for this
    /// and must not fall through to the failure state.
    var failedOnTheLink: Bool {
        if case .failed(.app) = self { return true }
        return false
    }
}

/// What the relay enrollment path is doing, folded into the push test-button
/// explanation. Only a relay daemon drives this; direct and off stay `.idle`.
enum RelayPushState: Equatable, Sendable {
    /// No relay work in flight — direct mode, off, or before the first handshake.
    case idle
    /// Attesting and minting the credential.
    case enrolling
    /// A credential is held and registered.
    case ready
    /// This device has no App Attest hardware. No relay push, and — by design —
    /// no insecure fallback; every non-relay affordance stays.
    case unsupported
    /// Enrollment failed for a stated, reader-facing reason. Retried on the next
    /// foreground.
    case failed(String)
}

/// Application state: one daemon, N sessions, one event stream.
@MainActor
@Observable
final class AppModel {
    let pairing: PairingStore
    let connection: DaemonConnection
    let settings: AppSettings
    /// The live terminal, riding the same paired connection as everything else.
    let terminal: TerminalCarrier
    private let cache: EventCache

    /// The Mac's list of runs, as it last said. The truth: what is on screen is
    /// `shownSummaries`.
    private(set) var summaries: [SessionSummary] = []
    /// Per-run state, keyed by `SessionSummary.sessionKey` — the `session_uid`
    /// where the daemon mints them, the tmux name on an older one. Never by the
    /// display name on a uid-capable daemon: `cc-1` is handed to the next
    /// session when this one exits, and keying by it is what spliced two agents'
    /// timelines into one.
    ///
    /// The truth, so no view reads it: screens draw `shownSessions`, each
    /// session's `shown`.
    private var states: [String: SessionState] = [:]
    /// **The fleet on screen**: the list, and the sessions whose `shown` the
    /// screens draw. Published from `summaries` and `states` by `publishList`.
    private var shownSummaries: [SessionSummary] = []
    private var shownSessions: [String: SessionState] = [:]
    /// Runs gone from the Mac whose review marks go when the list without them
    /// is published.
    private var departedMarks: Set<String> = []
    /// One `CodexControls` per session, created on demand by `codexControls(for:)`.
    ///
    /// **`@ObservationIgnored`, and that is load-bearing.** Views read this
    /// during `body` — the fleet asks whether Stop is greyed, the session
    /// header asks what the daemon last said — and the lookup *creates* the
    /// controller on a miss. Observed, that insertion is a mutation during view
    /// update: `body` reads it, the write invalidates, `body` re-runs, and the
    /// app never reports itself idle. Measured as a render pass whose
    /// XCUITest quiescence wait stopped settling, which is the same signature
    /// the read gate's geometry probe produced and the same lesson —
    /// `AppModel.state(for:)` carries the warning in its own doc comment.
    ///
    /// Nothing is lost by ignoring it: each `CodexControls` is itself
    /// `@Observable`, so the *contents* — the in-flight flag, the settled
    /// outcome, the cooldown — still drive redraws. Only the act of minting an
    /// empty one is invisible, and an empty one has nothing to draw.
    @ObservationIgnored private var codexControlsByKey: [String: CodexControls] = [:]
    /// **What this phone has sent and cannot account for, across launches.**
    /// See `CodexSpentLedger`; the per-session controllers read and write it.
    ///
    /// Injectable because it is genuinely durable: a test that shares the
    /// standard defaults with the next test is a test whose second stop is
    /// refused by the first one's uncertainty — which is the interlock working,
    /// and useless as a fixture.
    @ObservationIgnored let codexSpentLedger: CodexSpentLedger
    /// Where the fleet list came from, and when.
    private struct FleetSource: Equatable {
        var isLive = false
        /// Age of the list when it came off disk rather than the wire.
        var cachedAt: Date?
        /// When *this launch* put that cached list on screen. The cached
        /// banner's grace clock — distinct from `cachedAt`, which is how old the
        /// data is, not how long the launch has had to replace it.
        var restoredAt: Date?
    }
    private var fleetSource = FleetSource()
    /// Where the list on screen came from: published with it.
    private var shownFleetSource = FleetSource()
    var hasLiveFleet: Bool { shownFleetSource.isLive }
    var fleetCachedAt: Date? { shownFleetSource.cachedAt }
    var fleetCacheRestoredAt: Date? { shownFleetSource.restoredAt }
    /// Whether the fleet may be shown. Until then the app shows its loader, so
    /// the cached list, the live list and each session's backlog replay are
    /// never painted one after another. True once every session in the first
    /// live list has replayed to the `last_seq` that list reported — or once
    /// the loader has waited long enough — and never false again.
    private(set) var launchSettled = false
    /// What the first live list said each session had, by key.
    private var launchTargets: [String: UInt64]?
    /// Sessions whose subscription has settled what history it keeps — after
    /// the cache read and any rewind — so a cache from a reset log is never
    /// taken for caught up.
    private var preparedForLaunch: Set<String> = []
    /// Ticks once a second so every age on screen stays true without each view
    /// owning a timer.
    private(set) var now = Date()
    /// One diff per session, keyed like `states`, fetched only when asked for.
    private(set) var diffs: [String: DiffState] = [:]
    /// Where a deep link (a `codeconnect://` URL, or a tapped notification)
    /// wants the UI to go. Cleared by whichever surface consumes it.
    var pendingDeepLink: DeepLink?

    /// How far back to backfill a session we have never cached. The daemon pages
    /// at 500; this keeps a cold open bounded without hiding recent work.
    private static let initialBackfill: UInt64 = 400
    private static let cacheDebounce: Duration = .seconds(2)
    private static let fleetRefreshInterval: Duration = .seconds(15)

    private var subscribed: Set<String> = []
    /// Where each run's subscription was last asked from, and on which
    /// connection. See `subscribe(_:after:)`.
    private var subscribedAfter: [String: (generation: Int, afterSeq: UInt64)] = [:]
    /// Keyed by `ApprovalItem.id` — the run *and* the request. A `request_id` is
    /// only unique within one run, so keying by it alone would have one card's
    /// spinner and one card's confirmed outcome appear on another run's card.
    private var answersInFlight: Set<String> = []
    private var answerAttempts: [String: AnswerAttempt] = [:]
    private var loadingEarlier: Set<String> = []
    /// Sessions back in the foreground whose window the next fleet list resets
    /// to the newest 500, since only that list says where the newest 500 start.
    /// During a catch-up only a list that answers it does.
    private var windowResetDue: Set<String> = []

    /// **A catch-up**: the truth replaying what the Mac did while the screen
    /// was not watching — a return, a redial, the launch's first link — with
    /// the screen held as it was until the truth has caught up, then changed
    /// once. See `advanceCatchUp`.
    private struct CatchUp {
        let epoch: Int
        /// The link it is catching up on.
        var generation: Int
        /// Where its own `sessions` request stands among those written on that
        /// link, once one has been.
        var position: Int?
        /// The latest list that answers it: what caught up means.
        var list: [SessionSummary]?
        /// Something owed advanced since the last look.
        var progressed = false
    }
    @ObservationIgnored private var catchUp: CatchUp?
    @ObservationIgnored private var catchUpEpochs = 0
    /// Whether the screen is held for a catch-up. Observed, unlike `catchUp`,
    /// whose progress changes with every event.
    private(set) var isCatchingUp = false
    @ObservationIgnored private var releasing = false
    @ObservationIgnored private var stallWatch: Task<Void, Never>?
    /// `sessions` requests written and replies taken on the link
    /// `sessionsCountGeneration`.
    @ObservationIgnored private var sessionsWritten = 0
    @ObservationIgnored private var sessionsAnswered = 0
    @ObservationIgnored private var sessionsCountGeneration = -1
    /// The link generation whose session list `summaries` is. Open cards are
    /// fetched only from a list the current link sent: one loaded from the
    /// cache, or sent on a link since gone, can name cards the Mac has resolved.
    private var listGeneration: Int?
    /// The app went to the background, so the next `active` is a return. An
    /// `inactive` alone — Control Center, a pulled-down notification — never
    /// left the foreground, and the reader's place is left alone.
    private var wentToBackground = false
    /// Runs the cache migration, at most once per connection. Awaited by every
    /// subscription before it reads the cache, so a file written under the old
    /// keying can never be loaded into a run it may not belong to.
    private var cacheMigration: Task<Set<String>, Never>?
    /// What the cache has been brought into line with. Nil until a daemon has
    /// said which of the two it is.
    private var adoptedKeying: EventCache.Keying?
    /// Keys whose cached history the migration threw away and which have not yet
    /// been told about it. Consumed once each, so the notice lands on the run
    /// that lost the history rather than on every future namesake.
    private var cacheDropNotices: Set<String> = []
    /// The endpoint a scanned QR opened, held only until the device token
    /// arrives. Never persisted — see `pair(withQR:)`.
    private var pendingPairing: DaemonEndpoint?

    /// Why a scan-and-pair did not work. Derived rather than stored: the
    /// connection already knows, and a second copy could disagree with it.
    var pairingError: String? {
        guard pendingPairing != nil, case .failed(let reason) = connection.phase else { return nil }
        return reason
    }
    private var pendingCacheWrites: Set<String> = []
    private var cacheWriteTask: Task<Void, Never>?
    private var tickTask: Task<Void, Never>?
    private var fleetRefreshTask: Task<Void, Never>?

    init(
        pairing: PairingStore = PairingStore(), cache: EventCache = EventCache(),
        settings: AppSettings = AppSettings(),
        relayEnrollment: RelayEnrollment = RelayEnrollment(),
        codexSpentLedger: CodexSpentLedger = CodexSpentLedger()
    ) {
        // Startup housekeeping, on the one object the app builds exactly once.
        LegacyCredentials.purge()
        self.pairing = pairing
        self.connection = DaemonConnection()
        self.settings = settings
        self.cache = cache
        self.relayEnrollment = relayEnrollment
        self.codexSpentLedger = codexSpentLedger
        // Owned here, not by the Terminal tab: the terminal rides the paired
        // connection, and a view that owns it would drop the session every time
        // SwiftUI rebuilt the tab. One carrier per connection is also what makes
        // "one terminal at a time" true on this end, matching the daemon.
        self.terminal = TerminalCarrier(connection: connection)

        connection.onMessage = { [weak self] message in self?.handle(message) }
        connection.onConnected = { [weak self] in
            // Never for a replayed ack: the sample fleet ingests one, and a
            // replay must not register for push or resubscribe — today the
            // fixture's values happen to stop both downstream, but a system
            // permission prompt held back by a data file is not a guarantee.
            guard let self, !self.fixturesActive else { return }
            // Held first, so what the old link's end changes — a page or a card
            // it will never answer — waits for the release with the rest.
            self.openCatchUp(onNewLink: true)
            self.resubscribeAll()
            // After every handshake, not only after a pairing: a phone that
            // paired before push existed must start registering the first
            // time an upgraded daemon advertises it — and the capability is
            // only knowable here, once `hello_ack` has landed. Idempotent:
            // re-registration replaces, the daemon keys on the device.
            self.enablePush()
            // And check the credential's standing now the handshake exists —
            // a cold launch that activated before it connected skipped the
            // foreground check, and this is where its prerequisites arrive.
            // Debounced, so it is at most a daily status GET.
            self.foregroundRelayRefresh()
        }
        connection.onDeviceToken = { [weak self] token in self?.adopt(deviceToken: token) }
        connection.onTransportSettled = { [weak self] useTLS in
            self?.pairing.noteTransport(useTLS: useTLS)
        }
        installPushWiring()
    }

    // MARK: - Lifecycle

    /// Asking iOS for notification permission, and telling the daemon where to
    /// push once Apple issues a token.
    private let pushRegistrar = PushRegistration()
    #if DEBUG
        /// Test seams: observe that registration was *requested* and that a
        /// token delivery was *attempted* — the two separate decisions the
        /// gates control. Attempted, not completed: the seam fires at the
        /// post-recheck decision point, before the socket write, because the
        /// gates are what these tests pin — transport success is the
        /// connection's own tested concern.
        /// The real registrar's answer arrives through async system callbacks
        /// a unit test can neither await nor distinguish from silence —
        /// an assertion on the authorization outcome passed identically with
        /// the gate deleted, which is what these seams exist to prevent.
        var onPushRegistrationRequested: (() -> Void)?
        var onPushDeliveryAttempted: ((String) -> Void)?
        /// The full outbound `RegisterPush` — so a test can observe the exact
        /// environment and credential the app sends, not just the token.
        var onRegisterPushForTesting: ((_ token: String, _ environment: String, _ credential: String?) -> Void)?
        /// Fires **synchronously** the instant `sendRegistration` is called, before
        /// its Task is scheduled — so a test counts registration *intent* the moment
        /// it is decided, and a buggy registration that has not run yet is still
        /// observed. Paired with `onRegisterPushSettledForTesting` this is an
        /// all-registration-work-settled barrier: `scheduled == settled` proves no
        /// registration is still in the air.
        var onRegisterPushScheduledForTesting: ((_ token: String, _ environment: String, _ credential: String?) -> Void)?
        /// Fires when a `sendRegistration` Task has fully finished — whether it sent
        /// or was guarded out — so `scheduled == settled` is a true quiescence check.
        var onRegisterPushSettledForTesting: (() -> Void)?
        /// Fires when a relay outcome has finished being applied — a completion
        /// barrier so a test never sleeps a guess at when a parked flight resumed.
        var onRelayOutcomeApplied: (() -> Void)?
        /// Fires when a foreground status refresh has fully settled (its debounce
        /// window set, its in-flight latch cleared).
        var onRelayRefreshComplete: (() -> Void)?

        /// Inject the token Apple would have issued, through the same path.
        func simulatePushTokenForTesting(_ token: String, environment: String) {
            acceptPushToken(token, environment: environment)
        }
    #endif
    /// What the user actually decided, so the UI can say so rather than imply
    /// it. `nil` until asked.
    private(set) var pushAuthorized: Bool?
    /// Why registration failed, when it did — usually an App ID without the
    /// Push Notifications capability. Reported, never swallowed.
    private(set) var pushFailure: String?

    /// The live iOS permission state, re-read on every ask — the stored
    /// `pushAuthorized` goes stale the moment the user visits Settings.
    func pushAuthorizationStatus() async -> UNAuthorizationStatus {
        await pushRegistrar.authorizationStatus()
    }

    /// Whether this launch has already asked iOS for permission and an APNs
    /// token. Once is enough — Apple re-issues through `onToken` if the token
    /// rotates. **Requesting and delivering are separate states**: conflated
    /// into one latch, a re-pair to another Mac in the same launch got no
    /// token at all (Apple's callback had already come and gone), and a
    /// delivery that failed mid-flap was never retried.
    private var pushRequestedThisLaunch = false
    /// The latest token Apple issued, kept so delivery is repeatable on
    /// demand: Apple's callback fires when Apple pleases, but the device row
    /// that needs the token is whichever daemon is connected *now*.
    private var latestPushToken: (token: String, environment: String)?

    /// Owns the App Attest key, the relay credential, and every operation that
    /// mints or repairs it. Kept apart from `pushRegistrar` (permission + token)
    /// because they are different trust boundaries: one talks to Apple for a
    /// routing token, the other proves this install to the relay for a bearer.
    /// Injected so a test can drive the orchestration against a scripted relay
    /// and a fake attester.
    private let relayEnrollment: RelayEnrollment
    /// What the relay path is doing, for the test-button explanation. Relay mode
    /// only; a direct-key daemon never leaves `.idle`.
    private(set) var relayPushState: RelayPushState = .idle
    /// Earliest the next foreground status check may run. Success caps it to
    /// daily; a failure backs off a few minutes so a flaky relay does not pin the
    /// foreground. `distantPast` so the first foreground after launch checks.
    private var nextRelayRefreshAllowed = Date.distantPast
    /// A status check is in flight. Guards against a second scene activation
    /// firing an overlapping GET before the first has set the debounce.
    private var relayRefreshInFlight = false

    /// The registrar's callbacks and the failure observer, installed exactly
    /// once. This used to live inside `enablePush`, which is called per
    /// handshake — every reconnect stacked another observer and another
    /// pair of closures.
    private func installPushWiring() {
        pushRegistrar.onAuthorization = { [weak self] granted in
            self?.pushAuthorized = granted
        }
        pushRegistrar.onToken = { [weak self] token, environment in
            self?.acceptPushToken(token, environment: environment)
        }
        NotificationCenter.default.addObserver(
            forName: PushWire.failureNotification, object: nil, queue: .main
        ) { [weak self] note in
            // The `String` is lifted out here, on the posting side of the
            // boundary: carrying the `Notification` itself across is what the
            // concurrency checker objects to, and it is right to.
            let reason = note.object as? String
            MainActor.assumeIsolated { self?.pushFailure = reason }
        }
    }

    /// Apple issued (or re-issued) a token: remember it, then hand it to
    /// whichever daemon is connected right now.
    private func acceptPushToken(_ token: String, environment: String) {
        // A new token is a new binding; the daily status debounce is about the
        // *previous* token and must not suppress the first check of this one.
        if latestPushToken?.token != token { nextRelayRefreshAllowed = .distantPast }
        latestPushToken = (token, environment)
        deliverPushToken()
    }

    /// The one push decision the rest of the model reads, normalized from the
    /// wire with direct precedence. Bootstrap/static connections have no
    /// capabilities and resolve to `.none`.
    private var pushMode: PushMode {
        connection.capabilities?.pushMode ?? .none
    }

    /// Whether the *current* connection can accept a push registration: the
    /// daemon sends push in *some* mode, and this session has a device row to
    /// store the token against. `pushMode != .none` replaces the old raw
    /// `capabilities.push` check so a relay daemon (which advertises `push =
    /// false`) is eligible too. A static-token session has no device row and is
    /// refused here, so no prompt or enrollment is ever attempted for it.
    private var pushEligible: Bool {
        pushMode != .none && connection.helloAck?.deviceID != nil
    }

    /// Send the cached token to the current connection's device row. Branches on
    /// the normalized mode: a direct-key daemon takes the legacy token-only
    /// registration and the relay is never contacted; a relay daemon takes the
    /// App Attest credential, enrolling first if one is not already held. Off and
    /// static resolve to `.none` and do nothing. Safe to repeat: the daemon
    /// upserts by device.
    private func deliverPushToken() {
        guard let latest = latestPushToken, pushEligible else { return }
        let generation = connection.generation
        switch pushMode {
        case .direct:
            // Byte-identical to the pre-relay flow: no credential, the wire key
            // omitted entirely. The relay is never reached in direct mode.
            sendRegistration(
                token: latest.token, environment: latest.environment,
                credential: nil, generation: generation)
        case .relay:
            deliverViaRelay(token: latest.token, environment: latest.environment,
                generation: generation)
        case .none:
            return
        }
    }

    /// The socket write, bound to the connection generation it was judged under.
    /// A re-pair or a handshake replacing the socket inside the hop bumps the
    /// generation, and a delivery judged against an older one is dropped rather
    /// than landing on a daemon it was never eligible for.
    private func sendRegistration(
        token: String, environment: String, credential: String?, generation: Int
    ) {
        #if DEBUG
            // Synchronous with the decision to register, before the Task — so a test
            // observes registration intent even when the Task has not yet run.
            onRegisterPushScheduledForTesting?(token, environment, credential)
        #endif
        Task {
            #if DEBUG
                defer { onRegisterPushSettledForTesting?() }
            #endif
            // The token check is not redundant with `applyRelayOutcome`'s: a hop
            // separates them, and an APNs token change inside it would otherwise
            // send a credential bound to a token that is no longer current. The
            // direct path passes `credential == nil` and a token that cannot go
            // stale mid-hop the same way, but the guard is uniform and cheap.
            guard connection.generation == generation, pushEligible,
                latestPushToken?.token == token
            else { return }
            #if DEBUG
                onPushDeliveryAttempted?(token)
                onRegisterPushForTesting?(token, environment, credential)
            #endif
            do {
                try await connection.send(
                    .registerPush(
                        token: token, environment: environment, relayCredential: credential))
            } catch {
                pushFailure = error.localizedDescription
            }
        }
    }

    /// Acquire (reuse, rebind, or freshly enroll) the relay credential for this
    /// token, then register it — but only if it still belongs on the connection
    /// and the token that asked for it. The enrollment await is where a
    /// relay→direct switch or an APNs token change can slip in; the recheck in
    /// `applyRelayOutcome` is what keeps a stale relay result from registering
    /// over the current token.
    private func deliverViaRelay(token: String, environment: String, generation: Int) {
        let daemonEnvironment = connection.helloAck?.pushEnvironment
        if relayPushState != .ready { relayPushState = .enrolling }
        Task {
            let outcome = await relayEnrollment.credential(
                token: token, environment: environment, daemonEnvironment: daemonEnvironment)
            applyRelayOutcome(outcome, forToken: token, generation: generation)
        }
    }

    /// The one place a relay outcome becomes UI state and, when it should, a
    /// registration. The guard is the single-flight's downstream twin: it rejects
    /// a result whose connection moved, whose eligibility lapsed, or whose token
    /// is no longer current, so a late credential is remembered (it is already
    /// stored) but never registered against the wrong tuple.
    private func applyRelayOutcome(
        _ outcome: RelayEnrollment.Outcome, forToken token: String, generation: Int
    ) {
        #if DEBUG
            defer { onRelayOutcomeApplied?() }
        #endif
        guard connection.generation == generation, pushEligible,
            pushMode == .relay, latestPushToken?.token == token
        else {
            if case .ready = outcome { relayPushState = .ready }
            return
        }
        switch outcome {
        case .ready(let credential):
            relayPushState = .ready
            sendRegistration(
                token: credential.token, environment: credential.environment,
                credential: credential.credential, generation: generation)
        case .unsupported:
            relayPushState = .unsupported
        case .tokenInvalid:
            // The relay retired this APNs token. Drop it and ask Apple for a new
            // one; the fresh token enrolls itself when it arrives.
            relayPushState = .idle
            latestPushToken = nil
            pushRegistrar.registerForRemoteNotifications()
        case .failed(let reason):
            relayPushState = .failed(reason)
        case .superseded:
            // A newer request owns the binding now; this result is inert. Leave
            // the state to whatever that newer request sets.
            break
        }
    }

    /// Runs after every handshake via `onConnected`, gated on the daemon
    /// being able to ring **this device**: `capabilities.push` alone is
    /// server-global, and a static-token session has no device row — the
    /// daemon refuses its registration outright (a push stream no revocation
    /// could switch off), so asking the user to authorise it would be a
    /// prompt nothing can honour. The *permission request* happens once per
    /// launch; the *token delivery* repeats on every eligible handshake, so
    /// a same-launch re-pair hands the new Mac the token Apple already
    /// issued, and a delivery that failed while the link flapped is retried
    /// by the next handshake.
    func enablePush() {
        guard pushEligible else { return }
        if !pushRequestedThisLaunch {
            pushRequestedThisLaunch = true
            #if DEBUG
                onPushRegistrationRequested?()
            #endif
            pushRegistrar.requestAndRegister()
        }
        deliverPushToken()
    }

    /// Foreground housekeeping for the relay path, debounced and backed off.
    /// Re-reads notification authorization (so a Settings toggle takes effect
    /// without a re-prompt) and checks the credential's standing with the relay.
    /// Called on scene activation.
    func foregroundPushCheck() {
        reReadNotificationAuthorization()
        foregroundRelayRefresh()
    }

    /// Deny-in-app, enable-in-Settings, return to the app: Settings gives no
    /// callback, so on foreground the app re-reads the live authorization and, if
    /// it is now granted but no token is in hand, registers for one **without**
    /// prompting again. One launch, no second dialog.
    private func reReadNotificationAuthorization() {
        guard pushEligible, latestPushToken == nil else { return }
        Task {
            guard await pushAuthorizationStatus() == .authorized else { return }
            pushRequestedThisLaunch = true
            pushRegistrar.registerForRemoteNotifications()
        }
    }

    /// Ask the relay what became of the stored credential and act on it: keep,
    /// rotate via assertion, re-enroll, or surrender a retired token. At most
    /// daily on success; a short backoff on failure. Runs on foreground *and*
    /// once a relay handshake completes, so a cold launch that connects after
    /// activation still gets its one check this session.
    private func foregroundRelayRefresh() {
        guard pushMode == .relay, pushEligible, let latest = latestPushToken,
            !relayRefreshInFlight, Date() >= nextRelayRefreshAllowed
        else { return }
        // Claim the window before the await, so a second activation that lands
        // mid-check does not fire an overlapping status GET.
        relayRefreshInFlight = true
        nextRelayRefreshAllowed = Date().addingTimeInterval(24 * 60 * 60)
        let generation = connection.generation
        Task {
            let outcome = await relayEnrollment.refresh(
                token: latest.token, environment: latest.environment)
            applyRelayOutcome(outcome, forToken: latest.token, generation: generation)
            // A failure earns a short retry rather than the full day.
            switch outcome {
            case .ready, .unsupported, .superseded: break
            case .failed, .tokenInvalid:
                nextRelayRefreshAllowed = Date().addingTimeInterval(5 * 60)
            }
            relayRefreshInFlight = false
            #if DEBUG
                onRelayRefreshComplete?()
            #endif
        }
    }

    /// "Reset notification registration." Forgets the local relay binding and
    /// re-drives the flow from scratch on the current connection: a fresh key, a
    /// fresh attestation, a fresh credential. The stored App Attest key is
    /// discarded, so this is the in-app equivalent of the plan's `reenroll`.
    func resetNotificationRegistration() {
        Task {
            await relayEnrollment.reset()
            relayPushState = .idle
            nextRelayRefreshAllowed = .distantPast
            deliverPushToken()
        }
    }

    /// Where a tapped notification lands.
    ///
    /// Routed through `pendingDeepLink` rather than by setting a view's state,
    /// because a cold-started tap arrives before any view exists — the same
    /// reason a `codeconnect://` URL takes this path.
    ///
    /// A tapped notification opens the decision list for an approval, and the
    /// fleet for anything else.
    ///
    /// It opens the *list*, never a card: the payload names no decision, so
    /// nothing here can point at something already answered. The other three
    /// kinds have no card in that list and never will — `Finished a turn` has
    /// nothing to decide — so sending them there would be a wrong answer rather
    /// than a stale one.
    ///
    /// Opened while a catch-up holds the screen — and one does from the moment
    /// the app leaves — the list waits for it and says so
    /// (`deckAwaitsCatchUp`), then shows its cards in one go.
    func openFromNotification(kind: String?) {
        pendingDeepLink = kind == "approval" ? .deck(requestID: nil) : .fleet
        if kind == "approval", isCatchingUp { deckAwaitsCatchUp = true }
    }

    /// The Deck a notification opened is waiting for the catch-up to end.
    private(set) var deckAwaitsCatchUp = false

    /// The Deck closed: whatever it was waiting for, the next one opened from
    /// the bar shows the cards on screen.
    func deckClosed() {
        deckAwaitsCatchUp = false
    }

    /// How many catch-ups have changed the screen. A Deck left open tells the
    /// cards one brings from arrivals by it: they were there before the reader
    /// looked.
    private(set) var catchUpReleases = 0

    /// Drain a tap that arrived before anything was listening.
    func consumePendingTap() {
        if let tap = PushWire.consumeTap() { openFromNotification(kind: tap.kind) }
    }

    func bootstrap() {
        var pairedByCode = false
        #if DEBUG
            applyAutomationPairing()
            pairedByCode = applyAutomationPairingCode()
            // Before anything reads from disk: a fixture run has to be hermetic
            // or a previous live session's cache bleeds into it, and a test that
            // is half real data is a test of nothing.
            applyFixtures()
            // After the fixtures, so a link naming one of their cards resolves.
            applyAutomationDeepLink()
        #endif
        startTicking()
        // A fixture run is the whole fleet already; nothing is catching up.
        guard !fixturesActive else {
            launchSettled = true
            return
        }
        startFleetRefresh()
        Task {
            // **The saved fleet is on screen before the Mac is dialled.** The
            // first link's catch-up holds the screen as it is, and the launch
            // falls back to what is on it: dialled first, a local Mac answers
            // before the cache is read, and that fallback is an empty fleet.
            await loadCachedFleet()
            // A pairing exchange is already dialling; starting a second
            // connection would spend the single-use code twice.
            if !pairedByCode, let endpoint = pairing.endpoint {
                connect(to: endpoint)
                // Push registration deliberately does NOT happen here: before
                // the handshake the daemon's capabilities are unknown, and
                // asking for notification permission on behalf of a daemon that
                // may not advertise push is a system prompt nothing can honour.
                // `onConnected` registers once the `hello_ack` says it can ring.
            }
        }
    }

    /// Start the connection. One connection carries everything: the timeline,
    /// approvals, and the live terminal.
    private func connect(to endpoint: DaemonEndpoint) {
        connection.start(endpoint: endpoint)
    }

    /// The daemon answered a pairing code with a durable device token.
    private func adopt(deviceToken: String) {
        guard let endpoint = pendingPairing ?? pairing.endpoint else { return }
        pairing.adopt(deviceToken: deviceToken, from: endpoint)
        pendingPairing = nil
        // No push call here: `onConnected` fires for this same handshake and
        // is the one registration site — a second call from the pairing path
        // was a duplicate workflow, measured as stacked observers.
    }

    #if DEBUG
        /// How many of `-CC_FIXTURE_LATE_EVENT_MS`'s held-back frames have been
        /// delivered, so a test can see an event land behind a screen that
        /// does not draw it. Not observed: reading it redraws nothing.
        static var lateFramesDelivered = 0

        /// Test seam: `-CC_HOST <host> -CC_TOKEN <token>` on the launch command
        /// line pairs the app without anyone typing, which is what lets the UI
        /// test drive a real daemon. Debug builds only — a release build has no
        /// way to be paired except by a person.
        private func applyAutomationPairing() {
            let defaults = UserDefaults.standard
            guard let host = defaults.string(forKey: "CC_HOST"),
                let token = defaults.string(forKey: "CC_TOKEN"),
                let endpoint = DaemonEndpoint.parse(address: host, token: token)
            else { return }
            if defaults.bool(forKey: "CC_RESET_CACHE") {
                Task { [cache] in await cache.clearAll() }
            }
            // Ephemeral on purpose: this unsigned automation build has no
            // Keychain entitlement, so a durable save cannot work — measured
            // as every `-CC_HOST` launch landing on the welcome screen.
            pairing.saveEphemeral(endpoint)
        }

        /// Test seam: `-CC_PAIR_HOST <host> -CC_PAIR_CODE <code>` runs the real
        /// pairing exchange — tokenless `hello{pairing_code}`, `hello_ack`
        /// device token, Keychain — without a camera. It is the same call the
        /// scanner makes; only the source of the payload differs, which is
        /// exactly the part a simulator cannot provide.
        private func applyAutomationPairingCode() -> Bool {
            let defaults = UserDefaults.standard
            guard let host = defaults.string(forKey: "CC_PAIR_HOST"),
                let code = defaults.string(forKey: "CC_PAIR_CODE"), !code.isEmpty
            else { return false }
            pairing.clear()
            let port = defaults.integer(forKey: "CC_PAIR_PORT")
            pair(
                withQR: PairingQRPayload(
                    host: host, port: (1...65535).contains(port) ? port : DaemonEndpoint.defaultPort,
                    code: code))
            return true
        }

        /// Test seam: `-CC_DEEPLINK codeconnect://deck/<request-id>` opens the
        /// app on one named card — something a URL can do and a tapped
        /// notification cannot, since its payload names no decision.
        ///
        /// It exists because the read gate has to be asserted **on a named
        /// card**, and the only other way to reach the second card in the queue
        /// is to postpone the first — which at AX5 means three drags to bring
        /// `Come back to this` above the fold before the tap. That is a lot of
        /// harness standing between a test and a safety assertion, and every
        /// bit of it can fail for reasons that have nothing to do with the gate.
        /// The link goes through `DeepLink.init(url:)` and the real
        /// `pendingDeepLink` path, so it exercises the routing rather than
        /// bypassing it.
        private func applyAutomationDeepLink() {
            guard let raw = UserDefaults.standard.string(forKey: "CC_DEEPLINK"),
                let url = URL(string: raw)
            else { return }
            _ = open(url: url)
        }

        /// Test seam: `-CC_CODEX <state>` stages one Codex state end to end.
        ///
        /// Every frame goes through the real `ServerMessage` decoder and the
        /// real ingest path, exactly as the Claude fixtures do — so a fixture
        /// that stopped matching the wire fails to decode rather than quietly
        /// diverging, and these double as a decoder test.
        ///
        /// The two **mutation** outcomes are stubbed differently, and they have
        /// to be: a stop and a message are request/response, so there is no
        /// frame to replay until something asks. The stub answers the question
        /// the state stages, after the same capability gate a real send meets —
        /// which is why `daemon-minor16` and `daemon-minor17` render a refusal
        /// that never left the phone rather than one the daemon wrote.
        private func applyCodexFixture(_ state: CodexFixtures.State) {
            fixturesActive = true
            connection.fixtureAnswers = true
            connection.simulateConnectedForTesting()
            for message in CodexFixtures.frames(state: state) {
                connection.injectForTesting(message)
            }
            // The staged answer, delivered through the real waiter table by the
            // real correlation on the phone's own `request_id`.
            connection.sendStub = { [weak connection] message in
                switch message {
                case .interrupt(let session, let requestID, _, _):
                    guard let result = CodexFixtures.interruptResult(state) else { return }
                    connection?.injectForTesting(
                        .interruptResult(
                            sessionID: session, requestID: requestID, result: result))
                case .compose(let session, let requestID, _, _):
                    guard let result = CodexFixtures.composeResult(state) else { return }
                    connection?.injectForTesting(
                        .composeResult(
                            sessionID: session, requestID: requestID, result: result))
                default:
                    break
                }
            }
            // A staged outcome is *pressed*, not painted: the render drives the
            // real send path, so what it photographs is what a tap produces —
            // including the presses the gate refuses, which are the only way to
            // photograph the gate itself.
            if CodexFixtures.pressesStop(state) {
                Task { await stopCodexTurn(sessionKey: CodexFixtures.sessionKey) }
            }
            if CodexFixtures.pressesCompose(state) {
                Task {
                    await composeToCodex(
                        sessionKey: CodexFixtures.sessionKey,
                        text: "Also say the word HELLO-STEER when you are done.")
                }
            }
            keepFixtureLinkFresh()
        }

        /// Link health is measured from when a frame last *arrived*, so a
        /// fixture run needs frames to keep arriving or the link correctly goes
        /// stale mid-render and disables every action.
        private func keepFixtureLinkFresh() {
            Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(5))
                    guard let self, self.fixturesActive else { return }
                    self.connection.injectForTesting(.pong)
                }
            }
        }

        /// Test seam: `-CC_FIXTURE deck` replays contract-shaped daemon frames
        /// through the real decoders and the real ingest path, so the Deck can
        /// be driven with several agents blocked at three risk classes at once —
        /// a state that cannot be arranged on demand against a live Mac.
        ///
        /// Debug builds only, and it deliberately does *not* pair: a fixture run
        /// never has a socket, so nothing it shows can be confused with a live
        /// link that has gone quiet.
        private func applyFixtures() {
            // `-CC_CODEX <state>` stages one Codex state, seeded from
            // `fixtures/codex/*`. Its own entry point rather than a `Variant`,
            // because a Codex state is not a *shape of fleet* — it is a shape of
            // one session's history, its daemon's age, and what the daemon
            // answers a mutation with, which is three axes the fleet variants
            // do not have.
            if let state = CodexFixtures.State(
                UserDefaults.standard.string(forKey: "CC_CODEX"))
            {
                applyCodexFixture(state)
                return
            }
            // `-CC_QUESTION <state>` stages one Claude question card, for the
            // same reason: a state of one session and its daemon's age, not a
            // shape of fleet. Answers resolve locally, as under `-CC_FIXTURE`.
            if let state = QuestionFixtures.State(
                UserDefaults.standard.string(forKey: "CC_QUESTION"))
            {
                fixturesActive = true
                connection.fixtureAnswers = true
                connection.simulateConnectedForTesting()
                for message in QuestionFixtures.frames(state: state) {
                    connection.injectForTesting(message)
                }
                keepFixtureLinkFresh()
                return
            }
            // `-CC_FIXTURE stacked` is the same fleet with one agent holding two
            // decisions — the state where "count the agents" and "count the
            // cards" stop agreeing.
            guard let variant = Fixtures.Variant(UserDefaults.standard.string(forKey: "CC_FIXTURE"))
            else { return }
            fixturesActive = true
            connection.fixtureAnswers = true
            // `-cc.debug.sendText` resolves typed sends locally and can inject
            // measured receipts, allowing command-sheet states to be rendered
            // without arranging live Mac interactions on cue.
            if let sendMode = UserDefaults.standard.string(forKey: "cc.debug.sendText") {
                connection.sendTextStub = { [weak connection] session, _ in
                    // A receipt-bearing mode injects Claude Code's own
                    // transcript line *before* answering, so the sheet's
                    // immediate post-send check finds it. The real daemon has
                    // the same two parts in the other order and the sheet
                    // handles both; injecting first is what makes the render
                    // deterministic instead of a race with `onChange`.
                    if let frame = Fixtures.receiptFrame(mode: sendMode, session: session),
                        let message = Fixtures.decode(frame)
                    {
                        connection?.injectForTesting(message)
                    }
                    return Fixtures.sendTextResult(mode: sendMode)
                }
            }
            connection.simulateConnectedForTesting()
            // **`-CC_FIXTURE_LATE_APPROVALS_MS <n>` is a socket still catching
            // up.** A cold launch from a notification tap consumes its deep
            // link before the history that carries the decision has arrived.
            // The stream is split at the first approval and the tail delivered
            // `n` ms later, in order — held back whole rather than approvals
            // alone, because a fixture that skips ahead reads as a gap.
            // `DeepLinkTests` is what drives it.
            let lateApprovalsMS = UserDefaults.standard.integer(
                forKey: "CC_FIXTURE_LATE_APPROVALS_MS")
            var frames = Fixtures.frames(variant: variant)
            // **`-CC_FIXTURE_LATE_EVENT_MS <n>[,<n>…]` is events arriving while
            // the reader is on screen.** The stream's last frames are held back,
            // one per delay, and delivered in stream order — the shortest delay
            // first — each that many ms after the rest, so a UI test can act
            // first and then see how each event is counted.
            // `SessionShowAllUITests` and `FleetRowRenderUITests` drive it.
            let lateEventMS = (UserDefaults.standard.string(forKey: "CC_FIXTURE_LATE_EVENT_MS")
                ?? "").split(separator: ",").compactMap { Int($0) }.filter { $0 > 0 }.sorted()
            if !lateEventMS.isEmpty, frames.count >= lateEventMS.count {
                let late = frames.suffix(lateEventMS.count)
                frames.removeLast(lateEventMS.count)
                for (delay, frame) in zip(lateEventMS, late) {
                    Task { @MainActor [weak connection] in
                        try? await Task.sleep(for: .milliseconds(delay))
                        connection?.injectForTesting(frame)
                        Self.lateFramesDelivered += 1
                    }
                }
            }
            if lateApprovalsMS > 0,
                let first = frames.firstIndex(where: {
                    if case .event(let event) = $0 { return event.kind == .approvalRequest }
                    return false
                })
            {
                let held = Array(frames[first...])
                frames.removeSubrange(first...)
                Task { @MainActor [weak connection] in
                    try? await Task.sleep(for: .milliseconds(lateApprovalsMS))
                    for message in held { connection?.injectForTesting(message) }
                }
            }
            for message in frames {
                connection.injectForTesting(message)
            }
            if let diff = Fixtures.diff() {
                diffs[diff.sessionID] = .loaded(
                    diff, parsed: UnifiedDiff.parse(diff.unified), fetchedAt: Date())
            }
            applyCachedFixture()
            applyPushStateFixture()
            // **`-CC_FIXTURE_LINK_DROP_MS <n>` drops the link once, `n` ms in,**
            // through the connection's own `stop()` — every waiter failed, the
            // handshake and the Mac's capabilities forgotten — and brings it back
            // five seconds later with the handshake the fixture ended up with
            // (after the push seam, which replaces it). It is how a fixture run
            // takes the Mac's removal capability away and gives it back, with
            // the rows still on screen. It is narrower than a real drop: no
            // socket closes, no backoff runs, no redial is counted.
            // **`-CC_FIXTURE_REMOVE_MS <n>` holds a removal's answer `n` ms,**
            // then refuses it the way every fixture removal is refused, with no
            // answer. Without it a fixture refuses at once, so a removal still
            // on its way — "Removing…", and whatever happens around it — is a
            // state no fixture run can show.
            let removeMS = UserDefaults.standard.integer(forKey: "CC_FIXTURE_REMOVE_MS")
            if removeMS > 0 {
                connection.deleteStub = { _ in
                    try await Task.sleep(for: .milliseconds(removeMS))
                    throw ConnectionError.sentButUnanswered
                }
            }
            let linkDropMS = UserDefaults.standard.integer(forKey: "CC_FIXTURE_LINK_DROP_MS")
            if linkDropMS > 0, let ack = connection.helloAck {
                Task { @MainActor [weak connection] in
                    try? await Task.sleep(for: .milliseconds(linkDropMS))
                    connection?.stop()
                    connection?.setPhaseForTesting(.connecting)
                    try? await Task.sleep(for: .seconds(5))
                    connection?.injectForTesting(.helloAck(ack))
                }
            }
            // Link health is measured from when a frame last *arrived*, so a
            // fixture run needs frames to keep arriving or the link correctly
            // goes stale mid-test and disables every action.
            //
            // **`-CC_FIXTURE_LINK stale` withholds them on purpose.** That
            // keep-alive is why `stale` was unreachable under every fixture, and
            // a state nothing can reach is a state nobody has looked at: the
            // accessory bar shipped with its blocked reason cut mid-word at
            // `…but the daem…` and dimmed to 3.16:1, and nothing could render
            // the frame that showed it. `LinkHealth.staleAfter` is 45s, so the
            // state arrives on its own about three quarters of a minute in.
            guard UserDefaults.standard.string(forKey: "CC_FIXTURE_LINK") != "stale" else { return }
            Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(5))
                    guard let self, self.fixturesActive else { return }
                    self.connection.injectForTesting(.pong)
                }
            }
        }

        /// Test seam: **`-CC_FIXTURE_CACHED <age in seconds>`** restages the
        /// fixture's fleet as one read off **disk** that many seconds ago.
        ///
        /// Without it **the cached fleet cannot be rendered or tested at all.**
        /// That is the state where every wait clock on screen keeps ticking off
        /// data that arrived before the app launched — a reader at 2am sees an
        /// amber `5m40s` and cannot tell it from a live one — and the only way
        /// into it was to have a real cache file, no daemon, and the patience to
        /// arrange both. So the one banner in the ladder that exists to say *how
        /// much of this screen to believe* was the one banner nothing could draw.
        ///
        /// Compose it with `-CC_FIXTURE_LINK stale` for the **compound** state
        /// the ladder was rebuilt for: the link's own classification carrying the
        /// cache's age, one banner, both facts.
        ///
        /// Deliberately runs *after* the frames are injected — a `sessions` frame
        /// is what sets `hasLiveFleet` — and latches, so nothing arriving later
        /// can relight the fleet behind the render.
        private func applyCachedFixture() {
            guard let raw = UserDefaults.standard.string(forKey: "CC_FIXTURE_CACHED"),
                let age = TimeInterval(raw), age > 0
            else { return }
            fixtureCachedFleet = true
            // The seam stages "the banner is up", so the grace is staged as
            // already passed — the render harness is photographing the earned
            // state, not the launch that leads to it.
            fleetSource = FleetSource(
                isLive: false, cachedAt: Date().addingTimeInterval(-age), restoredAt: .distantPast)
            publishList()
        }

        /// Test seam: **`-CC_PUSH_STATE enrolling|failed|unsupported|ready`** puts
        /// the connection in relay mode and pins the enrollment state, so the push
        /// test-button ladder and the reset action can be photographed. A relay
        /// enrollment cannot be arranged in a render pass — it needs App Attest
        /// hardware and a live relay — so the state is staged, exactly as the
        /// cached-fleet and snapshot seams stage theirs.
        private func applyPushStateFixture() {
            guard let raw = UserDefaults.standard.string(forKey: "CC_PUSH_STATE") else { return }
            // `bootstrap` stages a relay-capable daemon reached over a static
            // connection: no device row, so enrollment can never begin.
            let deviceID: String? = raw == "bootstrap" ? nil : "render-device"
            connection.injectForTesting(
                .helloAck(
                    HelloAck(
                        protocolVersion: 1, protocolMinor: 14, serverTime: "",
                        capabilities: Capabilities(
                            pushRelay: true, extra: ["test_push": .bool(true)]),
                        deviceToken: nil, deviceID: deviceID, deviceName: "iPhone",
                        pushEnvironment: "production")))
            switch raw {
            case "enrolling": relayPushState = .enrolling
            case "failed": relayPushState = .failed("The relay could not be reached.")
            case "unsupported": relayPushState = .unsupported
            case "ready": relayPushState = .ready
            default: break
            }
        }
    #endif

    /// Latched by `-CC_FIXTURE_CACHED`. Always false in a release build, where
    /// the seam does not exist.
    #if DEBUG
        private var fixtureCachedFleet = false
    #endif

    /// True while the app is showing replayed frames instead of a daemon: the
    /// `-CC_FIXTURE` launch argument in a debug build, or the sample fleet in
    /// any build.
    ///
    /// It is what keeps a replay hermetic — nothing subscribes, nothing
    /// migrates, and nothing reaches the cache — so no part of it can be
    /// mistaken for, or written over, a real Mac's.
    private(set) var fixturesActive = false

    /// True while the sample fleet is on screen.
    ///
    /// Implies `fixturesActive`, and adds the two things only a shipping screen
    /// needs: the banner that says what this is, and an answer path that says
    /// what it would have done instead of reaching for a Mac. The launch
    /// argument is a test seam and never sets it.
    private(set) var sampleFleetActive = false

    /// Show the sample fleet. The only way in, and it takes an explicit tap.
    ///
    /// **Why this ships.** Somebody arriving with no Mac — an App Store reviewer
    /// most of all — can complete no pairing, and every screen in this product
    /// sits behind one. Without this the app can only ever be read about.
    ///
    /// It creates no pairing and opens no socket, so the rule it sits beside
    /// still holds: a release build has no way to be *paired* except by a person
    /// with a Mac. Entry is refused from anywhere but the unpaired state, which
    /// is what keeps sample and live state from ever being on screen together.
    func startSampleFleet() {
        // `pendingPairing == nil` is load-bearing: the replayed ack reaches
        // `commitPendingPairing`, and a pending typed-token endpoint would be
        // saved to the Keychain on the strength of a frame no daemon sent.
        guard !fixturesActive, !pairing.isPaired, pendingPairing == nil else { return }
        // A return holds the screen even with no Mac, and none will catch it up.
        closeCatchUp()
        sampleFleetActive = true
        fixturesActive = true
        for message in Fixtures.sampleFrames() {
            connection.ingest(message)
        }
        // The sample is the whole fleet already; there is no Mac to wait for.
        settleLaunch()
        // Every sample session, not just the diff's own: the replayed ack
        // advertises diff support, so the ± control appears on all of them, and
        // a tap that answers "Not paired with a daemon." inside the sample
        // fleet would be a dead end in the link's vocabulary. One shared diff
        // is what the fixture has; the banner has already said none of it is
        // real.
        if let diff = Fixtures.diff() {
            let parsed = UnifiedDiff.parse(diff.unified)
            for key in states.keys {
                diffs[key] = .loaded(diff, parsed: parsed, fetchedAt: Date())
            }
        }
    }

    /// Leave the sample fleet and go back to pairing.
    ///
    /// Also called on the way *into* a pairing, from both entry points: the
    /// sample fleet is reachable while its own Settings sheet is open, so
    /// "sample and live never mix" has to be enforced where a live link begins,
    /// not only where the sample one ends.
    func stopSampleFleet() {
        guard sampleFleetActive else { return }
        sampleFleetActive = false
        fixturesActive = false
        connection.stop()
        forgetFleet()
        // The real fleet that follows has to be caught up with like any other.
        launchSettled = false
    }

    /// Ask the Mac to forget a run, and forget it here too.
    ///
    /// **Gated on `SessionSummary.isRemovable`, which mirrors the daemon's own
    /// rule.** A hosted run needs `lifecycle == .exited` — the proof, never the
    /// derived Ended status, which can read Ended without the run being proven
    /// exited. An unhosted run (adopted; empty `tmux_session`) is removable at
    /// any lifecycle, because no proof of its end can ever exist — and removing
    /// one also stops observation of that conversation: the Mac drops its
    /// further hooks until a new session start announces itself and re-adopts
    /// it. The daemon enforces the same rule in SQL regardless; the gate here
    /// keeps the phone from offering what would be refused.
    ///
    /// Also gated on the daemon having advertised `delete_session`. `FleetView`
    /// checks that too, to decide whether to offer the swipe at all; it is
    /// repeated here because this is the method that puts a request on the wire,
    /// and an older Mac answers an unknown message with an error rather than a
    /// result — a spinner that ends in nothing.
    ///
    /// Local clean-up happens **after** the daemon says it is gone, through the
    /// ordered cache path — see `enqueueCacheWork` for why order is not free —
    /// and is awaited, so a caller that reports success is reporting something
    /// that has actually been written.
    @discardableResult
    func removeSession(_ summary: SessionSummary) async -> DeleteSessionResult? {
        guard summary.isRemovable, daemonProfile.removesSessions else { return nil }
        let key = summary.sessionKey
        let result = try? await connection.deleteSession(sessionUID: summary.sessionUID)
        // `notFound` counts as gone: the Mac does not have it, so neither should
        // we. Nothing else does — see `meansItIsGone`.
        guard let result, result.meansItIsGone else { return result }

        forgetLocalState(of: key)
        summaries.removeAll { $0.sessionKey == key }
        if catchUp != nil {
            unpublishRemoved(key)
        } else {
            publishList()
        }
        // **The row itself lives here.** `loadCachedFleet` restores this file
        // wholesale on a cold open and is what the app shows before the first
        // live reply, so a removal that skipped it survived only until the app
        // was killed — and offline, indefinitely. Written now rather than left
        // to the next fleet reply, which may never come.
        let remaining = summaries
        enqueueCacheWork { await $0.saveFleet(remaining) }
        await cacheSettled()
        return result
    }

    /// Removes each ended session in turn, and returns why each one still in
    /// the fleet was not removed, by key.
    ///
    /// Each is looked up again when its turn comes: one that left the fleet
    /// while the question was up is not asked about, and one that is no longer
    /// an ended, removable run is kept. A refusal is that session's own answer
    /// and the batch carries on; no answer at all — the link is gone, or the Mac
    /// no longer says it removes sessions — means every later request would
    /// fail the same way, so the rest are not asked and share that reason.
    func removeSessions(keys: [String]) async -> [String: String] {
        var refused: [String: String] = [:]
        var stopped: String?
        for key in keys {
            guard let summary = summary(for: key) else { continue }
            if let stopped {
                refused[key] = stopped
                continue
            }
            let status = FleetStatusRule.status(
                summary: summary, shown: states[key]?.current,
                reviewedSeq: ReviewMarks.reviewedSeq(for: key))
            guard status == .ended, summary.isRemovable else {
                refused[key] = "Changed since you chose it"
                continue
            }
            let result = await removeSession(summary)
            if let reason = result.refusal { refused[key] = reason }
            if result == nil { stopped = result.refusal }
        }
        return refused
    }

    #if DEBUG
        /// Test seam: run the real cold-open restore against this model's cache.
        ///
        /// `loadCachedFleet` is private and scheduled by `start()`, which also
        /// arms timers and dials — none of which a test about restore wants.
        func loadCachedFleetForTesting() async { await loadCachedFleet() }

        /// Test seam: start from a known fleet without a `sessions` frame.
        ///
        /// `summaries` is `private(set)` because the daemon owns it. A removal
        /// test has to begin with the row present, and injecting the frame would
        /// also start subscriptions and cache migrations that have nothing to do
        /// with what is being tested.
        func adoptSummariesForTesting(_ list: [SessionSummary]) {
            summaries = list
            publishList()
        }

        /// Test seam: the truth, keyed by session. Views read `shown(for:)`.
        var statesForTesting: [String: SessionState] { states }
    #endif

    /// The fleet is the root once there is something to show it from.
    var showsFleet: Bool { pairing.isPaired || fixturesActive }

    /// Manual entry: a host and a token typed by hand.
    ///
    /// Held pending, exactly like a scanned code, rather than saved on the way in.
    /// Saving first meant a typo was persisted as *the* pairing before anything
    /// had spoken to a Mac — the app then left onboarding for a Fleet it could
    /// never load, with a credential the daemon would reject forever and no route
    /// back to the screen that could fix it. The endpoint is committed by
    /// `commitPendingPairing()` once a daemon has actually answered.
    func pair(with endpoint: DaemonEndpoint) {
        stopSampleFleet()
        pendingPairing = endpoint
        subscribed.removeAll()
        forgetDaemonKeying()
        // A typed token is still a pairing: nothing is saved until the daemon's
        // `hello_ack` proves the credential, and someone is watching the screen
        // for that verdict — so the dial must be bounded, not endlessly patient.
        connection.start(endpoint: endpoint, forPairing: true)
    }

    /// A daemon answered on a pending endpoint that carries its own token.
    ///
    /// The QR path commits in `adopt(deviceToken:)`, because there the proof is
    /// the durable token arriving. A typed token has no exchange to complete, so
    /// the proof is the `hello_ack` itself: the daemon accepted this credential.
    private func commitPendingPairing() {
        guard let endpoint = pendingPairing, case .token = endpoint.credential else { return }
        pairing.save(endpoint)
        pendingPairing = nil
        // No push call here either — same handshake, same `onConnected`.
    }

    /// A different Mac may identify its runs differently, so nothing about the
    /// last one's keying survives being pointed at a new one.
    private func forgetDaemonKeying() {
        cacheMigration = nil
        adoptedKeying = nil
        cacheDropNotices = []
    }

    /// Pair from a scanned QR code.
    ///
    /// The code is spent on one connection and never written to disk: it is
    /// single-use with a five-minute life, so a stored copy would produce a
    /// pairing that looks valid and can never connect. The durable device token
    /// arrives in `hello_ack` and *that* is what gets saved.
    ///
    func pair(withQR payload: PairingQRPayload) {
        stopSampleFleet()
        let endpoint = payload.endpoint
        pendingPairing = endpoint
        subscribed.removeAll()
        forgetDaemonKeying()
        connection.start(endpoint: endpoint)
    }

    /// True while a scanned code is being exchanged for a device token.
    var isPairing: Bool { pendingPairing != nil }

    func cancelPairing() {
        pendingPairing = nil
        if let endpoint = pairing.endpoint {
            connect(to: endpoint)
        } else {
            connection.stop()
        }
    }

    func unpair() {
        // Unpairing returns the app to the unpaired ground state, and being
        // inside the sample fleet is not part of that state: left set, these
        // flags strand an empty fleet under a Sample banner, and
        // `startSampleFleet`'s own guard then refuses re-entry.
        sampleFleetActive = false
        fixturesActive = false
        connection.stop()
        pairing.clear()
        forgetFleet()
        pendingPairing = nil
        forgetDaemonKeying()
        Task { await cache.clearAll() }
    }

    /// Drop every trace of the fleet on screen, including what feeds the Deck.
    ///
    /// Shared by `unpair` and by leaving the sample fleet, because "completely"
    /// has to mean the same thing both times: a card left in `states` is a card
    /// still counted by the Deck, and after a re-pair it would be counted beside
    /// a real Mac's.
    private func forgetFleet() {
        subscribed.removeAll()
        summaries = []
        states = [:]
        answerAttempts = [:]
        answersInFlight = []
        diffs = [:]
        // **The mutation controllers go with the fleet.** They were left
        // behind, so a reconnect that re-created a session under the same key
        // inherited the previous fleet's in-flight ids and settled banners —
        // one run's uncertainty printed over another's. The durable spent
        // material is a separate store on purpose and is NOT cleared here:
        // "what this phone has already sent" survives losing the list.
        codexControlsByKey.removeAll()
        fleetSource = FleetSource()
        // Nothing held is owed to a screen that no longer has a Mac.
        closeCatchUp()
        commitList()
        #if DEBUG
            listPublications.append(.live)
        #endif
    }

    func scenePhaseChanged(to phase: ScenePhase) {
        switch phase {
        case .active:
            // Every age on screen is true on the first frame back, not a tick
            // later.
            now = Date()
            connection.retryNow()
            if !comeBack() { refreshFleet() }
            startTicking()
            startFleetRefresh()
            foregroundPushCheck()
        case .background, .inactive:
            if phase == .background {
                wentToBackground = true
                holdForReturn()
            } else {
                now = Date()
                comeBack()
            }
            flushCache()
            tickTask?.cancel()
            tickTask = nil
            fleetRefreshTask?.cancel()
            fleetRefreshTask = nil
        @unknown default:
            break
        }
    }

    /// Back from the background: the newest 500 again, and the catch-up aimed
    /// at this return. The way back starts with `inactive`, while the app is
    /// still being brought to the front and already reading its socket, so a
    /// short backlog is caught up by the time it is active. A return on a link
    /// that stayed up asks now; one on a link that is down is aimed by the
    /// redial's handshake. Says whether this return's list was asked for.
    @discardableResult
    private func comeBack() -> Bool {
        guard wentToBackground else { return false }
        wentToBackground = false
        windowResetDue = Set(states.keys)
        guard connection.phase.isConnected else { return false }
        openCatchUp(onNewLink: false)
        return true
    }

    private func startTicking() {
        guard tickTask == nil else { return }
        tickTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                guard let self, !Task.isCancelled else { return }
                self.now = Date()
                for controls in self.codexControlsByKey.values {
                    controls.expireStopGrey(now: self.now)
                }
            }
        }
    }

    /// `link` and `blocked_on` can change without producing an event (a session
    /// going quiet, an approval answered at the Mac), so the fleet is re-asked
    /// rather than inferred.
    private func startFleetRefresh() {
        guard fleetRefreshTask == nil else { return }
        fleetRefreshTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.fleetRefreshInterval)
                guard let self, !Task.isCancelled else { return }
                self.refreshFleet()
            }
        }
    }

    func refreshFleet() {
        requestSessions()
    }

    /// The one way a `sessions` request leaves, so each is counted in the order
    /// it was written: the Mac answers them in that order, and a reply is
    /// matched to its request by that count. The first written on a catch-up's
    /// link after it opened is its own.
    private func requestSessions() {
        guard connection.phase.isConnected else { return }
        syncSessionsCount()
        guard connection.sendInOrder(.sessions) else { return }
        let position = sessionsWritten
        sessionsWritten += 1
        if catchUp?.generation == connection.generation, catchUp?.position == nil {
            catchUp?.position = position
        }
    }

    /// Take the next reply's position on this link — a list, or the Mac saying
    /// it has none — and say whether it answers the open catch-up: written at
    /// or after its own request.
    private func takeSessionsReply() -> Bool {
        syncSessionsCount()
        let position = sessionsAnswered
        sessionsAnswered += 1
        guard let catchUp, catchUp.generation == connection.generation,
            let own = catchUp.position
        else { return false }
        return position >= own
    }

    /// Counting starts again on every link.
    private func syncSessionsCount() {
        guard sessionsCountGeneration != connection.generation else { return }
        sessionsCountGeneration = connection.generation
        sessionsWritten = 0
        sessionsAnswered = 0
    }

    // MARK: - Derived state

    var linkHealth: LinkHealth {
        LinkHealth.evaluate(
            phase: connection.phase, lastContactAt: connection.lastContactAt,
            dialFailure: connection.dialFailure, isRedial: connection.isRedial, now: now)
    }

    /// Why a decision cannot be answered from here, or nil when it can.
    ///
    /// The link's own sentence, except in the sample fleet — which reports no
    /// link precisely because it has none, and would therefore disable every
    /// control on the one screen somebody came here to read. Those controls
    /// answer inline instead; see `DecisionCardView.submit`.
    var actionsBlockedReason: String? {
        sampleFleetActive ? nil : linkHealth.disabledReason
    }

    /// Which daemon build this is, and therefore which of the newer surfaces
    /// are real here.
    var daemonProfile: DaemonProfile { connection.profile }

    /// Every card in the fleet still waiting on a human, **urgency-ranked**:
    /// HIGH → MEDIUM → LOW, then oldest first inside a tier.
    ///
    /// Built from the event log rather than from `blocked_on`, because the Deck
    /// has to render each card's contents, and only the log carries them. The
    /// two agree; `blocked_on` is what makes the fleet band correct before the
    /// stream catches up.
    ///
    /// `SessionState.pendingApprovals` is a stored projection rebuilt on ingest,
    /// so this is O(sessions + cards) rather than O(sessions × events) — which
    /// matters because `FleetView.body` re-runs on the one-second tick.
    var deck: [ApprovalItem] {
        DeckOrdering.sort(
            shownSessions.values.flatMap(\.shown.pendingApprovals), profile: daemonProfile)
    }

    var deckCount: Int { deck.count }

    /// The card with this identity as it is drawn — the single resolver every
    /// decision surface (the open sheet, the Deck cell, the card view) renders
    /// from before offering an action or claiming an outcome.
    ///
    /// **`nil` means nothing on screen backs this card** — the session departed
    /// the fleet (`forgetLocalState`), its log was reset/rewound, or the card
    /// left the timeline. A `nil` here must render as a NON-actionable "no
    /// longer available" state, never a frozen actionable snapshot: acting on,
    /// or claiming an outcome for, a card no daemon would accept an answer for
    /// is exactly the class this resolver exists to close. Routed by
    /// `sessionKey` (the id's own prefix) so it is one timeline scan, not a
    /// fleet-wide one. What may actually be sent is `answer`'s to decide, from
    /// the truth.
    func shownApproval(sessionKey: String, id: String) -> ApprovalItem? {
        shownSessions[sessionKey]?.shown.approval(id: id)
    }

    /// Working directories the daemon has reported, newest session first. The
    /// only evidence the app has for the Mac's account name.
    var sessionPaths: [String] {
        shownSummaries.sorted { $0.updatedDate > $1.updatedDate }.map(\.cwd)
    }

    var fleet: [FleetRow] {
        let labels = RunLabel.labels(for: shownSummaries)
        return FleetOrdering.sort(
            shownSummaries.map { summary in
                let shown = shownSessions[summary.sessionKey]?.shown
                let pending = shown?.pendingApprovals ?? []
                return FleetRow(
                    summary: summary,
                    status: FleetStatusRule.status(
                        summary: summary, shown: shown, reviewedSeq: shown?.reviewedSeq ?? 0),
                    // `displaySafe`: the row's sentence is often the agent's
                    // own last words, or a notice built from someone else's.
                    subtitle: subtitle(for: summary, shown: shown).displaySafe,
                    activity: activity(for: shown),
                    label: labels[summary.sessionKey] ?? .unknown,
                    capability: FleetStatusRule.capability(
                        summary: summary, capabilities: connection.capabilities),
                    blockedCount: max(summary.blockedOn.count, pending.count),
                    lastEventAt: shown?.lastEventAt,
                    cachedAt: (shown?.hasLiveData ?? false) ? nil : shown?.loadedFromCacheAt)
            })
    }

    var blockedCount: Int { fleet.filter { $0.status == .blocked }.count }

    /// Creates on demand. Only ever called from event handling — a view that
    /// called it during `body` would be mutating state mid-render, so views read
    /// `shown(for:)`.
    private func state(for key: String) -> SessionState {
        if let existing = states[key] { return existing }
        let fresh = SessionState(sessionKey: key, recordsReviewMarks: !fixturesActive)
        fresh.onTimelineRebuilt = { [weak self] in
            guard let self else { return }
            self.fillWindow(of: key)
            if !self.launchSettled { self.settleLaunchIfCaughtUp() }
        }
        fresh.codexTurns = { [weak self] in
            guard let controls = self?.codexControlsByKey[key] else { return ([], []) }
            return (controls.observedTurns, controls.retiredTurns)
        }
        states[key] = fresh
        if catchUp == nil {
            shownSessions[key] = fresh
        } else {
            fresh.holdsPresentation = true
        }
        return fresh
    }

    /// Put the list and its sessions on screen as the truth now has them,
    /// unless a catch-up holds the screen.
    private func publishList() {
        guard catchUp == nil else { return }
        commitList()
        #if DEBUG
            listPublications.append(.live)
        #endif
    }

    private func commitList() {
        // A run that left the Mac takes its review mark with it as its row
        // leaves the screen, not before: a row drawn as reviewed stays reviewed
        // until it goes, and one listed again by then keeps its mark.
        for key in departedMarks where states[key] == nil {
            ReviewMarks.forget(sessionKey: key)
        }
        departedMarks.removeAll()
        if diffs.contains(where: { states[$0.key] == nil }) {
            diffs = diffs.filter { states[$0.key] != nil }
        }
        // Each assigned only when it differs: `@Observable` fires on the
        // assignment, and an equal list redraws every row for nothing.
        if shownSummaries != summaries { shownSummaries = summaries }
        if shownSessions.count != states.count
            || states.contains(where: { shownSessions[$0.key] !== $0.value })
        {
            shownSessions = states
        }
        if shownFleetSource != fleetSource { shownFleetSource = fleetSource }
    }

    #if DEBUG
        /// Why the list was published, once per publication. See
        /// `SessionState.publications`.
        enum ListPublication: Equatable {
            case live
            case release
            /// The reader removed a run.
            case removal
        }
        private(set) var listPublications: [ListPublication] = []
    #endif

    /// The run as the Mac last listed it. The truth: send paths and validation
    /// read it, and screens read `shownSummary(for:)`.
    func summary(for key: String) -> SessionSummary? {
        summaries.first { $0.sessionKey == key }
    }

    /// The run as the list on screen has it.
    func shownSummary(for key: String) -> SessionSummary? {
        shownSummaries.first { $0.sessionKey == key }
    }

    /// What is on screen for this run, or nil when the screen has no such run.
    func shown(for key: String) -> SessionState.Shown? {
        shownSessions[key]?.shown
    }

    /// The reader has seen this run through what is drawn.
    func markReviewed(_ key: String) {
        shownSessions[key]?.markReviewed()
    }

    /// The reader acknowledged this run's gap notice.
    func dismissGap(_ key: String) {
        shownSessions[key]?.dismissGap()
    }

    /// The screen showing this chat is told as the rows above its reader are
    /// replaced. See `SessionState.onWindowChange`.
    func onWindowChange(
        of key: String,
        _ observer: ((_ shown: [TimelineItem], _ next: [TimelineItem], _ change: SessionState.WindowChange) -> Void)?
    ) {
        shownSessions[key]?.onWindowChange = observer
    }

    /// The screen showing this chat is told as a catch-up's one change lands.
    /// See `SessionState.onCatchUpLanding`.
    func onCatchUpLanding(
        of key: String, _ observer: ((_ shown: SessionState.Shown, _ next: SessionState.Shown) -> Void)?
    ) {
        shownSessions[key]?.onCatchUpLanding = observer
    }

    /// What the command sheets watch for: the run's newest receipts as they
    /// arrive, never as drawn — a receipt is the answer to a send, and the sheet
    /// fences it against `eventHighWater`, which is the truth too.
    func receipts(for key: String) -> SessionState.Receipts? {
        states[key]?.receipts
    }

    /// What the diagnostics sheet reports of the sessions this phone holds, as
    /// they are now.
    var sessionDiagnostics: SessionDiagnostics {
        SessionDiagnostics(
            known: summaries.count,
            gaps: states.values.filter { $0.gap != nil }.count,
            highestSeq: states.values.map(\.lastSeq).filter { $0 > 0 }.max(),
            fromCache: states.values.filter { $0.loadedFromCacheAt != nil && !$0.hasLiveData }.count)
    }

    struct SessionDiagnostics {
        let known: Int
        /// Sessions whose event log has a discontinuity the app has not seen filled.
        let gaps: Int
        let highestSeq: UInt64?
        let fromCache: Int
    }

    /// The run's permission mode as drawn: what its held events last said, or
    /// the Mac's own account where that rests on a newer event — the event that
    /// set the mode can be older than the window.
    func permissionMode(for key: String) -> String? {
        let shown = shown(for: key)
        if let fact = shownSummary(for: key)?.permissionMode, fact.seq > shown?.permissionModeSeq ?? 0 {
            return fact.value
        }
        return shown?.permissionMode
    }

    /// The run's model, likewise.
    func confirmedModel(for key: String) -> ConfirmedModel? {
        let shown = shown(for: key)
        if let fact = shownSummary(for: key)?.model, fact.seq > shown?.confirmedModelSeq ?? 0 {
            return ConfirmedModel(
                name: fact.value, provenance: ModelProvenance(fact.source),
                at: ISO8601.parse(fact.at) ?? .distantPast)
        }
        return shown?.confirmedModel
    }

    /// How far this run's event stream has got, as far as anything here knows.
    ///
    /// A command sheet captures this the moment a row is tapped and accepts
    /// only receipts above it. Both terms are needed: `SessionState` holds what
    /// has actually arrived, and during a first subscription or a catch-up the
    /// fleet summary can already advertise events the state has not received —
    /// a fence built on the lower of the two would admit exactly the historical
    /// receipt it exists to exclude. The lookup lives here so a sheet never has
    /// to reach into fleet presentation to correlate its own send.
    func eventHighWater(for key: String) -> UInt64 {
        max(states[key]?.lastSeq ?? 0, summary(for: key)?.lastSeq ?? 0)
    }

    /// What to call this run on screen — see `RunLabel`.
    ///
    /// Computed across the whole fleet because the answer depends on it: a
    /// second run in the same project is what earns a qualifier, and no run can
    /// know that alone.
    func runLabel(for key: String) -> RunLabel {
        RunLabel.labels(for: shownSummaries)[key] ?? .unknown
    }

    /// The tmux session name to attach to, or nil when the daemon no longer
    /// lists this run.
    ///
    /// Nil rather than a guess, and taken from the fleet rather than from the
    /// route: `tmux attach -t =<uid>` cannot work — tmux has never heard of a
    /// uid — and the name is only safe to use while the daemon still vouches
    /// that it belongs to this run.
    func tmuxName(for key: String) -> String? {
        guard let summary = shownSummary(for: key) else { return nil }
        // Empty is the daemon saying "no known location": an adopted run whose
        // hooks arrived but which nothing ever put in tmux. Falling back to the
        // session id here was a guess wearing a fact's clothes — the exact
        // thing the comment above forbids — and attaching to a name nothing
        // vouches for could hand the user a different agent's keyboard.
        guard !summary.tmuxSession.isEmpty else { return nil }
        return summary.tmuxSession
    }

    private func subtitle(for summary: SessionSummary, shown: SessionState.Shown?) -> String {
        // **Nothing, rather than the run's own name back.** A run that has said
        // nothing has no sentence, and repeating the title underneath itself
        // reads as a fact and is not one.
        guard let last = shown?.timeline.last else { return "" }
        switch last.content {
        case .userMessage(let text, _): return "you: \(text.firstLine)"
        case .agentMessage(let text, _): return text.firstLine
        case .tool(let tool):
            return "\(tool.name) \(tool.argument?.firstLine ?? "")".trimmingCharacters(
                in: .whitespaces)
        case .approval(let approval):
            // No `needs you:` prefix. The band header, the aggregate and the
            // display line all say it already — three restatements per viewport
            // of one fact — and the width is worth more spent on the argument,
            // which is the only thing on the row that lets a fleet be triaged.
            let argument = ToolSummary.principalArgument(
                tool: approval.card.toolName, input: approval.card.toolInput)?.firstLine
            return "\(approval.card.toolName) \(argument ?? "")".trimmingCharacters(
                in: .whitespaces)
        case .notice(let notice): return notice.title
        }
    }

    /// The same last item as `subtitle(for:shown:)`, kept as **two strings** so
    /// the row can set the tool in prose and its argument in monospace.
    ///
    /// The flat sentence above is still built, and is still what VoiceOver
    /// hears: a screen reader has no faces to distinguish, and "Bash, echo soak"
    /// is the same fact spoken. What the split buys is that `echo soak` — a
    /// shell command — stops being drawn in proportional type on Running and
    /// Idle rows while an identical construction two rows above it is monospace.
    private func activity(for shown: SessionState.Shown?) -> FleetActivity? {
        guard let last = shown?.timeline.last else { return nil }
        switch last.content {
        case .tool(let tool):
            return FleetActivity(tool: tool.name, argument: tool.argument?.firstLine)
        case .approval(let approval):
            return FleetActivity(
                tool: approval.card.toolName,
                argument: ToolSummary.principalArgument(
                    tool: approval.card.toolName, input: approval.card.toolInput)?.firstLine)
        // Prose all the way through. A user message, an agent sentence and a
        // notice have no argument to set as code.
        case .userMessage, .agentMessage, .notice:
            return nil
        }
    }

    // MARK: - Inbound

    private func handle(_ message: ServerMessage) {
        switch message {
        case .deleteSessionResult:
            // Nothing to do here. `removeSession` awaits the reply itself and does
            // the local clean-up, because only it knows which row asked.
            break
        case .testPushResult:
            // Same shape: the sheet's own request awaits the reply by id.
            break
        case .terminalAttached, .terminalOutput, .terminalCredit, .terminalClosed:
            // The terminal is a byte stream with its own flow control, delivered
            // to `TerminalCarrier` on `connection.onTerminal`. Nothing here may
            // read it: bytes off that screen are never facts this model holds.
            break
        case .sessions(let list):
            let answersCatchUp = takeSessionsReply()
            if answersCatchUp {
                // Its first list is progress; the fleet refresh's later ones
                // restate what is owed and pay none of it.
                if catchUp?.list == nil { catchUp?.progressed = true }
                catchUp?.list = list
            }
            markFleetLive()
            // **Assigned only when it differs.** The fleet is re-asked every 15
            // seconds because `link` and `blocked_on` can change without an
            // event, and on a quiet fleet the answer is usually the one we
            // already have — measured against the owner's daemon, four
            // consecutive replies fifteen seconds apart were byte-identical.
            // `@Observable` fires on the *assignment*, not on a change, so
            // storing an equal array rebuilt and re-sorted all 45 rows and
            // re-rendered every visible one to redraw exactly what was already
            // on screen. `SessionSummary` is `Hashable` on every wire field, so
            // this comparison is the whole fact.
            if summaries != list { summaries = list }
            listGeneration = connection.generation
            enqueueCacheWork { await $0.saveFleet(list) }
            // Anything the daemon does not list is not something this app can
            // claim to know about. Dropping those states is what stops a
            // session read from a cold cache — or one left over from the old
            // name keying — contributing a card to the Deck that no daemon
            // would accept an answer for.
            let live = Set(list.map(\.sessionKey))
            // **Every route out of the fleet ends here, not just this phone's
            // swipe.** A session removed from another phone, pruned at the Mac,
            // or lost with the daemon's database simply stops being listed, and
            // that is the only notice this app gets. Cleaning up only in
            // `removeSession` left a cache file and a review mark behind for
            // each one, permanently — nothing else sweeps that directory.
            for departed in states.keys.filter({ !live.contains($0) }) {
                forgetLocalState(of: departed)
            }
            for summary in list {
                let sessionState = state(for: summary.sessionKey)
                if sessionState.lastSeq >= summary.lastSeq {
                    sessionState.noteConfirmedCurrent()
                }
                // A catch-up's own list spends the reset, never an older one.
                let resetDue =
                    (catchUp == nil || answersCatchUp)
                    && windowResetDue.remove(summary.sessionKey) != nil
                if resetDue, daemonProfile.pagesHistory, subscribed.contains(summary.sessionKey) {
                    restartWindow(of: summary)
                }
                subscribeIfNeeded(summary, resetWindow: resetDue)
                if daemonProfile.pagesHistory, sessionState.lastSeq >= summary.lastSeq {
                    sessionState.keepPins(summary.pending)
                }
            }
            if !launchSettled {
                // The first live list sets what "caught up" means; a later one
                // can only take away a session that has left.
                launchTargets =
                    launchTargets?.filter { live.contains($0.key) }
                    ?? Dictionary(list.map { ($0.sessionKey, $0.lastSeq) }) { first, _ in first }
                settleLaunchIfCaughtUp()
            }
            publishList()
            advanceCatchUp()
        case .event(let event):
            let sessionState = state(for: event.sessionKey)
            let heldThrough = sessionState.lastSeq
            sessionState.ingest(event)
            scheduleCacheWrite(event.sessionKey)
            if !launchSettled { settleLaunchIfCaughtUp() }
            // A resolution or a new card changes the fleet's top band.
            if event.kind == .approvalRequest || event.kind == .approvalResolved {
                refreshFleet()
            }
            if catchUp != nil {
                // Only a session still short of what it owes is progress: one
                // already there, or one the list does not name, is busy, not
                // catching up.
                if let target = catchUp?.list?.first(where: { $0.sessionKey == event.sessionKey })?
                    .lastSeq, heldThrough < target, sessionState.lastSeq > heldThrough
                {
                    catchUp?.progressed = true
                }
                advanceCatchUp()
            }
        case .historyEnd(let sessionID, let requestID, _, let firstSeq, let error):
            // Echoed as it was sent, which is the session's key.
            if let sessionState = states[sessionID],
                sessionState.pageRequest?.id == requestID || sessionState.pinRequest?.id == requestID
            {
                catchUp?.progressed = true
            }
            states[sessionID]?.endHistory(requestID: requestID, firstSeq: firstSeq, failed: error != nil)
            // The one request in flight is free: open cards first, then the fill.
            // A reader at the top is woken through `isReadyToPage`.
            fillWindow(of: sessionID)
            // A failed page ends the fill, which may be what the loader waits on.
            if !launchSettled { settleLaunchIfCaughtUp() }
            advanceCatchUp()
        case .helloAck(let ack):
            // The daemon accepted this credential, which is the only proof a
            // hand-typed token ever gets.
            commitPendingPairing()
            // A certificate the pairing's address can never validate is a fact
            // worth stating, not a silent downgrade.
            pairing.noteTLSUnusable(
                ack.capabilities.tls && pairing.endpoint?.hostIsIPLiteral == true)
            startCacheMigration()
        case .interruptResult, .composeResult:
            // Awaited by the control that asked, correlated on the request id it
            // minted. Nothing here may act on one: a stop or a message is a
            // mutation somebody pressed, and its outcome belongs to that press.
            break
        case .error(let code, _) where code == "sessions_unavailable":
            // The Mac's answer to a `sessions` request it could not serve: it
            // takes that request's place, so the replies after it stay matched.
            if takeSessionsReply() { catchUp?.progressed = true }
        case .answerResult, .sendTextResult, .captureResult, .commandCatalog, .diff, .error,
            .pong, .unknown:
            break
        }
    }

    /// Bring the on-disk cache into line with how *this* daemon identifies runs,
    /// before anything reads from it.
    ///
    /// A cache written under tmux names cannot be handed to a uid-keyed run: the
    /// file called `cc-1` holds whatever runs have held that name, spliced, and
    /// nothing in it says which parts belong to the run now offering that uid.
    /// So it is dropped and the timeline is re-fetched — a bounded backfill,
    /// paid once — and the sessions that lost history are told to say so.
    private func startCacheMigration() {
        // A fixture run has no daemon to adopt anything from, and sweeping a
        // real cache on the strength of a replayed frame would be a side effect
        // nobody asked for.
        guard !fixturesActive else { return }
        let keying: EventCache.Keying = daemonProfile.scopesSessionsByUID ? .uid : .name
        // Re-run only when the answer changes: reconnecting to the same daemon
        // has nothing to migrate, but re-pairing to a different one does.
        guard adoptedKeying != keying else { return }
        adoptedKeying = keying
        cacheMigration = Task { [cache] in await cache.adopt(keying: keying) }
    }

    // MARK: - Deep links

    /// Aim the UI at a decision, from a `codeconnect://` URL. A tapped
    /// notification takes a different route in — it names nothing, so it has no
    /// URL — and the two meet at `pendingDeepLink`, which is why the target is a
    /// value the model holds rather than navigation done inline.
    func open(url: URL) -> Bool {
        guard let link = DeepLink(url: url) else { return false }
        pendingDeepLink = link
        return true
    }

    func consumeDeepLink() -> DeepLink? {
        defer { pendingDeepLink = nil }
        return pendingDeepLink
    }

    /// Turn a link's session reference into a key this app can look up.
    ///
    /// A `codeconnect://session/…` URL may carry either: a uid (which survives
    /// a name being reused — a notification never sends one) or a tmux
    /// name (what a person types, and what every link written before uids
    /// existed says). A name is resolved the way the daemon resolves one —
    /// prefer the run with a supervisor attached, else the newest — because that
    /// is the run somebody following the link means, and `mac/README.md` makes
    /// that the contract.
    ///
    /// During a catch-up a name waits — nil — for the list the catch-up ends
    /// on: until then the name may still belong to a run that has gone.
    func resolveSessionKey(reference: String) -> String? {
        if summary(for: reference) != nil { return reference }
        guard !isCatchingUp else { return nil }
        let named = summaries.filter { $0.sessionID == reference }
        guard !named.isEmpty else { return nil }
        let attached = named.filter { $0.link == .attached }
        return (attached.isEmpty ? named : attached)
            .max { $0.createdDate < $1.createdDate }?
            .sessionKey
    }

    private func resubscribeAll() {
        subscribed.removeAll()
        // Asked on a link that has gone: none of them is settled on this one.
        subscribedAfter.removeAll()
        // A page asked for on the old connection ends on that connection or not
        // at all.
        for state in states.values {
            state.cancelPage()
            state.cancelPin()
        }
        // Before the fleet is shown, a new connection starts the catch-up over:
        // its first list says what caught up means, and what was prepared
        // against the old one says nothing about the new.
        if !launchSettled {
            launchTargets = nil
            preparedForLaunch = []
        }
    }

    /// Resume where the cache left off, and be explicit whenever that is not
    /// possible — a gap the client cannot see makes the log worthless.
    ///
    /// The daemon is asked for the run by `sessionKey`, which is the uid where
    /// there is one. Subscribing by name would follow *whichever* run holds that
    /// name — the daemon says so in `ws.rs` — so a dead `cc-1` on screen would
    /// quietly start showing a live `cc-1`'s work.
    ///
    /// On a Mac that serves pages the chat is the newest 500 at launch and on
    /// `resetWindow` (a return to the foreground), and on any other reconnect
    /// keeps what it holds unless it missed more than that.
    private func subscribeIfNeeded(_ summary: SessionSummary, resetWindow: Bool = false) {
        let key = summary.sessionKey
        guard !fixturesActive else { return }
        guard !subscribed.contains(key) else { return }
        subscribed.insert(key)

        // A subscription belongs to the connection that asked for it. Without
        // this, a task suspended across a reconnect can resume holding a stale
        // `summary.lastSeq`, conclude from newer in-memory events that the
        // daemon's log was reset, and wipe a perfectly good timeline while
        // announcing a rewind that never happened.
        let generation = connection.generation
        // Likewise a window: one applied while this task reads the cache — a
        // return to the foreground on this same connection — is the newer
        // decision, and this task must not lay its own over it.
        let epoch = state(for: key).windowEpoch
        // And the turns a compose had reported by now: one reported later began
        // after any events this window skips.
        let observed = codexControlsByKey[key]?.observedTurns ?? []
        Task { [weak self] in
            guard let self else { return }
            guard self.connection.generation == generation else {
                self.subscribed.remove(key)
                return
            }
            // Before any read: the cache has to be keyed the way this daemon
            // identifies runs, or a file could be loaded into the wrong one.
            if let migration = self.cacheMigration {
                self.cacheDropNotices.formUnion(await migration.value)
            }
            guard self.connection.generation == generation else {
                self.subscribed.remove(key)
                return
            }
            let sessionState = self.state(for: key)
            if self.cacheDropNotices.remove(summary.sessionID) != nil
                || self.cacheDropNotices.remove(key) != nil
            {
                sessionState.noteCacheDiscarded()
            }
            let cached = await self.cache.loadEvents(key: key)
            if let cached { sessionState.loadCached(cached) }

            // In-memory wins over disk: the cache write is debounced, so after a
            // brief drop RAM holds events the file does not.
            guard self.connection.generation == generation else {
                self.subscribed.remove(key)
                return
            }
            guard sessionState.windowEpoch == epoch else {
                return self.subscribeUnderNewerWindow(key, sessionState, generation: generation)
            }
            let known = max(sessionState.lastSeq, cached?.lastSeq ?? 0)
            var afterSeq: UInt64
            if known > summary.lastSeq {
                // The daemon's log is shorter than ours: it was reset behind us.
                sessionState.resetForRewoundLog()
                await self.cache.clearEvents(key: key)
                // Checked before the window is touched: a link replaced during
                // the clear has a task of its own for this run, which reads a
                // window change made here as a newer decision and stands down.
                // Its key in `subscribed` is that task's, so it stays.
                guard self.connection.generation == generation else { return }
                guard sessionState.windowEpoch == epoch else {
                    return self.subscribeUnderNewerWindow(key, sessionState, generation: generation)
                }
                afterSeq =
                    self.daemonProfile.pagesHistory
                    ? self.applyWindow(
                        sessionState, daemonLast: summary.lastSeq, keepEarlier: false, observed: observed)
                    : Self.backfillStart(lastSeq: summary.lastSeq, state: sessionState)
            } else if self.daemonProfile.pagesHistory {
                afterSeq = self.applyWindow(
                    sessionState, daemonLast: summary.lastSeq,
                    keepEarlier: sessionState.windowStart != nil && !resetWindow, observed: observed)
            } else if known > 0 {
                afterSeq = known
            } else {
                afterSeq = Self.backfillStart(lastSeq: summary.lastSeq, state: sessionState)
            }

            if !self.launchSettled {
                self.preparedForLaunch.insert(key)
                self.settleLaunchIfCaughtUp()
            }
            guard self.connection.phase.isConnected,
                self.connection.generation == generation
            else {
                self.subscribed.remove(key)
                return
            }
            self.subscribe(key, after: afterSeq)
            self.advanceCatchUp()
        }
    }

    /// Ask for a run's events after `afterSeq`, unless this connection was
    /// already asked from there or earlier.
    ///
    /// The replay of that ask is already bringing everything after it, and the
    /// live stream follows it, so holding fewer events now — a trim, or events
    /// sent but not yet ingested — is local and asks for nothing. Asking again
    /// would restart the daemon's replay. Only a start below the asked one, or a
    /// new connection, needs a new ask.
    private func subscribe(_ key: String, after afterSeq: UInt64) {
        let generation = connection.generation
        if let asked = subscribedAfter[key], asked.generation == generation,
            asked.afterSeq <= afterSeq
        {
            return
        }
        subscribedAfter[key] = (generation, afterSeq)
        connection.sendIgnoringFailure(.subscribe(session: key, afterSeq: afterSeq))
    }

    /// A window applied while this run's subscription read the cache decided what
    /// it holds, and this connection still needs a subscription from its tail —
    /// unless that application already asked for one.
    private func subscribeUnderNewerWindow(
        _ key: String, _ sessionState: SessionState, generation: Int
    ) {
        if !launchSettled {
            preparedForLaunch.insert(key)
            settleLaunchIfCaughtUp()
        }
        guard connection.phase.isConnected, connection.generation == generation else { return }
        let start = sessionState.windowStart.map { $0 - 1 } ?? 0
        subscribe(key, after: max(sessionState.lastSeq, start))
        advanceCatchUp()
    }

    /// Back in the foreground on a link that stayed up: the newest 500 again.
    /// Usually a local trim — the subscription on this link already brings what
    /// follows — and an ask only where this link has none yet, or one that
    /// starts later than the window now needs.
    private func restartWindow(of summary: SessionSummary) {
        let sessionState = state(for: summary.sessionKey)
        let afterSeq = applyWindow(
            sessionState, daemonLast: summary.lastSeq, keepEarlier: false,
            observed: codexControlsByKey[summary.sessionKey]?.observedTurns ?? [])
        subscribe(summary.sessionKey, after: afterSeq)
    }

    /// `SessionState.applyWindow`, then the fill: any application may leave a
    /// window that still has nothing to show, and one that kept everything draws
    /// nothing new, so no rebuild would ask for the page below it. `observed` are
    /// the turns a compose had reported when the list deciding this window came.
    private func applyWindow(
        _ sessionState: SessionState, daemonLast: UInt64, keepEarlier: Bool, observed: [String]
    ) -> UInt64 {
        let heldThrough = sessionState.lastSeq
        let afterSeq = sessionState.applyWindow(
            daemonLast: daemonLast, keepEarlier: keepEarlier,
            pending: summary(for: sessionState.sessionKey)?.pending ?? [])
        if heldThrough < afterSeq, let controls = codexControlsByKey[sessionState.sessionKey] {
            controls.forgetObservedTurns(observed)
            sessionState.codexTurnsChanged()
        }
        fillWindow(of: sessionState.sessionKey)
        return afterSeq
    }

    private func settleLaunchIfCaughtUp() {
        // A catch-up's release is the launch's, so the loader never gives way
        // to a screen the release changes a moment later.
        guard !launchSettled, catchUp == nil, let launchTargets else { return }
        // Caught up means holding what the list reported, and, on a Mac that
        // serves pages, a window that has something to show or nothing earlier
        // to page back to.
        let caughtUp = launchTargets.allSatisfy { key, target in
            guard preparedForLaunch.contains(key), let state = states[key] else { return false }
            return state.lastSeq >= target && !state.needsEarlierPage
        }
        if caughtUp { settleLaunch() }
    }

    /// The loader's patience. A Mac that has not answered since the loader came
    /// up gets `offline`, then the saved fleet under the link banner. One that
    /// has answered is catching up, and showing the fleet then would show the
    /// replay the loader hides, so it gets until it has caught up, through any
    /// blip and redial on the way. A refusal needs the user, so it ends the wait
    /// at once. Nothing waits past `cap`: a spinner that does not end is worse
    /// than any fleet.
    func waitOutLaunch(offline: Duration = .seconds(3), cap: Duration = .seconds(15)) async {
        let started = ContinuousClock.now
        // A handshake bumps the generation, so one that came and went between
        // two looks at the phase still counts.
        let connectedAtStart = connection.phase.isConnected
        let generationAtStart = connection.generation
        while !launchSettled {
            if case .failed = connection.phase { break }
            let answered = connectedAtStart || connection.generation != generationAtStart
            let waited = ContinuousClock.now - started
            if waited >= cap || (waited >= offline && !answered) { break }
            do {
                try await Task.sleep(for: .milliseconds(100))
            } catch {
                return
            }
        }
        settleLaunch()
    }

    /// Shows the fleet, with every timeline already drawn.
    func settleLaunch() {
        guard !launchSettled else { return }
        for state in states.values { state.rebuildNow() }
        launchSettled = true
        launchTargets = nil
        preparedForLaunch = []
    }

    // MARK: - Catch-up

    /// Hold the screen while the truth catches up with the Mac on this link —
    /// or, with a catch-up already open, aim it here: a redial or a return
    /// re-targets it rather than starting a second. Its own `sessions` request
    /// goes out now.
    ///
    /// On a new link every reply answers a request written after the
    /// handshake, so the link's first list is the catch-up's. On a link that
    /// stayed up, an older request may still be answering: only replies from
    /// its own request on count.
    private func openCatchUp(onNewLink: Bool) {
        guard !fixturesActive else { return }
        let epoch = holdScreen()
        catchUp?.generation = connection.generation
        catchUp?.list = nil
        catchUp?.position = onNewLink ? 0 : nil
        watchForStall(epoch: epoch)
        requestSessions()
    }

    /// **Leaving holds the screen at once.** A resuming app reads its socket
    /// while it is still coming back, before it is active, and one the system
    /// leaves running reads it all the while it is away: either way what
    /// arrives waits for the return's catch-up. Whatever an open catch-up was
    /// owed is old news by then, so nothing is released until the return
    /// aims it again.
    ///
    /// Not behind the launch's loader before any link: nothing is on screen to
    /// hold yet, and the saved fleet still being read must reach the screen the
    /// loader gives way to. The first link's catch-up holds what is there.
    private func holdForReturn() {
        guard !fixturesActive, catchUp != nil || launchSettled else { return }
        holdScreen()
        catchUp?.list = nil
        catchUp?.position = nil
        stallWatch?.cancel()
        stallWatch = nil
    }

    /// Hold the screen as it is, unless a catch-up already does. Returns the
    /// catch-up's epoch.
    @discardableResult
    private func holdScreen() -> Int {
        if let open = catchUp { return open.epoch }
        catchUpEpochs += 1
        catchUp = CatchUp(epoch: catchUpEpochs, generation: connection.generation)
        isCatchingUp = true
        for state in states.values { state.holdsPresentation = true }
        return catchUpEpochs
    }

    /// Let go of the screen without publishing.
    private func closeCatchUp() {
        stallWatch?.cancel()
        stallWatch = nil
        catchUp = nil
        isCatchingUp = false
        deckAwaitsCatchUp = false
        for state in states.values { state.holdsPresentation = false }
    }

    /// **Release the catch-up once the truth has caught up, in one change.**
    ///
    /// Caught up means, for every session the catch-up's latest list names: it
    /// holds what that list says it has, its subscription on this link has
    /// settled, its window has something to show or nothing earlier to page
    /// back to, no page or card is on its way, and every open card the list
    /// names is held — in the window, pinned, or fetched and found missing.
    ///
    /// Every session is built first, and any fill or card fetch that starts is
    /// waited for: its end comes back here. Between the last check and the
    /// commit nothing suspends, so what is checked is what is published. Never
    /// while the app is away: the return aims the catch-up again.
    private func advanceCatchUp() {
        guard let open = catchUp, !releasing, !wentToBackground, let list = open.list else { return }
        releasing = true
        defer { releasing = false }
        guard list.allSatisfy({ hasArrived($0, on: open.generation) }) else { return }
        for state in states.values { state.rebuildNow() }
        for summary in list { fillWindow(of: summary.sessionKey) }
        guard catchUp?.epoch == open.epoch,
            list.allSatisfy({ isCaughtUp($0, on: open.generation) })
        else { return }
        releaseCatchUp()
    }

    /// Everything the list says the session has is here, on a subscription this
    /// link has settled.
    private func hasArrived(_ summary: SessionSummary, on generation: Int) -> Bool {
        guard let state = states[summary.sessionKey] else { return false }
        return state.lastSeq >= summary.lastSeq
            && subscribedAfter[summary.sessionKey]?.generation == generation
    }

    private func isCaughtUp(_ summary: SessionSummary, on generation: Int) -> Bool {
        guard hasArrived(summary, on: generation), let state = states[summary.sessionKey] else {
            return false
        }
        return !state.needsEarlierPage && state.pageRequest == nil && state.pinRequest == nil
            && (!daemonProfile.pagesHistory || summary.pending.allSatisfy(state.holds(card:)))
    }

    private func releaseCatchUp() {
        closeCatchUp()
        for state in states.values { state.commitPresentation(asCatchUp: true) }
        commitList()
        catchUpReleases += 1
        #if DEBUG
            listPublications.append(.release)
        #endif
        if !launchSettled { settleLaunch() }
    }

    /// The reader removed a run mid-catch-up: its row and its cards leave the
    /// screen now, every other row stays as it is, and the catch-up stops
    /// waiting for it.
    private func unpublishRemoved(_ key: String) {
        shownSummaries.removeAll { $0.sessionKey == key }
        shownSessions[key] = nil
        if departedMarks.remove(key) != nil { ReviewMarks.forget(sessionKey: key) }
        diffs[key] = nil
        catchUp?.list?.removeAll { $0.sessionKey == key }
        catchUp?.progressed = true
        #if DEBUG
            listPublications.append(.removal)
        #endif
        advanceCatchUp()
    }

    /// **No progress on a link that is up means retry, never release.** Every
    /// fleet refresh interval — long enough for a healthy Mac to answer — a
    /// catch-up that got no list and advanced nothing it is owed redials, which
    /// asks for the list and subscribes everything afresh. A link that is down
    /// is its own supervision's to redial.
    private func watchForStall(epoch: Int) {
        stallWatch?.cancel()
        stallWatch = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.fleetRefreshInterval)
                guard let self, !Task.isCancelled, self.catchUp?.epoch == epoch else { return }
                self.checkCatchUp()
            }
        }
    }

    private func checkCatchUp() {
        // Away, nothing is released, so nothing could count as progress.
        guard let open = catchUp, connection.phase.isConnected, !wentToBackground else { return }
        guard !open.progressed else {
            catchUp?.progressed = false
            return
        }
        connection.redial()
    }

    #if DEBUG
        /// Test seam: what the stall watch does when its interval ends.
        func checkCatchUpForTesting() { checkCatchUp() }
    #endif

    private static func backfillStart(lastSeq: UInt64, state: SessionState) -> UInt64 {
        guard lastSeq > initialBackfill else { return 0 }
        state.noteTruncatedHead()
        return lastSeq - initialBackfill
    }

    /// The events before the ones on screen: one page of 500 from a Mac that
    /// serves pages, the whole log from one that does not. Guarded: one page is in
    /// flight at a time, and a full replay is expensive and repeat taps only
    /// duplicate it.
    ///
    /// The reader's ask waits out a catch-up: the page belongs above rows the
    /// screen is not showing yet. `isReadyToPage` turning true asks again.
    func loadEarlier(key: String) {
        guard connection.phase.isConnected else { return }
        if daemonProfile.pagesHistory {
            guard !isCatchingUp else { return }
            requestPage(of: key)
            return
        }
        guard !loadingEarlier.contains(key) else { return }
        loadingEarlier.insert(key)
        connection.sendIgnoringFailure(.subscribe(session: key, afterSeq: 0))
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(10))
            self?.loadingEarlier.remove(key)
        }
    }

    /// Ask for the page below this window, on the truth.
    private func requestPage(of key: String) {
        guard connection.phase.isConnected, let page = states[key]?.beginPage() else { return }
        connection.sendIgnoringFailure(
            .history(
                session: key, beforeSeq: page.beforeSeq, limit: UInt32(page.limit),
                requestID: page.id))
    }

    /// Asks for the request event of the first open card the Mac lists below the
    /// window and the phone does not hold, one request at a time; the next is
    /// asked when this one ends. Only from a list this link sent, and once the
    /// window holds what that list counts, the pins are reconciled with it.
    private func pinCards(of key: String) {
        guard connection.phase.isConnected, daemonProfile.pagesHistory,
            listGeneration == connection.generation, let state = states[key],
            let summary = summary(for: key)
        else { return }
        if state.lastSeq >= summary.lastSeq { state.keepPins(summary.pending) }
        for ref in summary.pending {
            guard let request = state.beginPin(ref) else { continue }
            connection.sendIgnoringFailure(
                .history(
                    session: key, beforeSeq: ref.seq + 1, limit: 1, requestID: request.id))
            return
        }
    }

    /// A window with nothing to show asks for the page below it once it has
    /// caught up with what the Mac listed — before then the newest events are
    /// still arriving and may be all it needs. An open card the window lacks is
    /// asked for first: one event, where the page is 500.
    private func fillWindow(of key: String) {
        pinCards(of: key)
        guard let state = states[key], state.needsEarlierPage,
            let summary = summary(for: key), state.lastSeq >= summary.lastSeq
        else { return }
        requestPage(of: key)
    }

    /// The reader reached the top of a chat a Mac that serves pages windowed.
    /// Theirs to ask, so it asks again after a page that failed.
    func reachedTop(key: String) {
        guard states[key]?.windowStart != nil, daemonProfile.pagesHistory else { return }
        loadEarlier(key: key)
    }

    /// The open chat needs the page below without the reader asking: it does not
    /// fill its screen, or a page added nothing and the reader is still at its
    /// top. Like the fill, it waits until the window has caught up with what the
    /// Mac listed, and stops at a page that failed.
    func pageBackOnItsOwn(key: String) {
        guard isReadyToPage(key), states[key]?.pagesBackOnItsOwn == true else { return }
        loadEarlier(key: key)
    }

    /// The window can page now: the link is up to a Mac that serves pages, no
    /// catch-up holds the screen, its timeline is drawn through what that Mac
    /// listed, and there is earlier history. The open chat's screen watches
    /// this, so a link coming back or a catch-up ending wakes a chat that still
    /// needs the page below, with nothing on screen having moved — and only
    /// once the caught-up rows are the ones on screen, since whether the chat
    /// is short is read off them.
    func isReadyToPage(_ key: String) -> Bool {
        guard connection.phase.isConnected, !isCatchingUp, daemonProfile.pagesHistory,
            let state = states[key], let windowStart = state.windowStart, windowStart > 1,
            state.pinRequest == nil, let summary = summary(for: key)
        else { return false }
        return state.drawnThrough >= summary.lastSeq
    }

    func isLoadingEarlier(_ key: String) -> Bool {
        daemonProfile.pagesHistory ? states[key]?.pageRequest != nil : loadingEarlier.contains(key)
    }

    // MARK: - Cache

    private func scheduleCacheWrite(_ key: String) {
        pendingCacheWrites.insert(key)
        guard cacheWriteTask == nil else { return }
        cacheWriteTask = Task { [weak self] in
            try? await Task.sleep(for: Self.cacheDebounce)
            guard let self else { return }
            self.cacheWriteTask = nil
            self.flushCache()
        }
    }

    func flushCache() {
        let keys = pendingCacheWrites
        pendingCacheWrites.removeAll()
        guard !keys.isEmpty else { return }
        let snapshots = keys.compactMap { key -> (String, [Event])? in
            guard let state = states[key] else { return nil }
            // A replay in flight is held in a merge buffer for a few
            // milliseconds; persisting mid-window would write a timeline with a
            // hole in it and then trust that file on the next cold open.
            state.settlePendingEvents()
            return (key, state.events)
        }
        enqueueCacheWork { cache in
            for (key, events) in snapshots {
                await cache.saveEvents(events, key: key)
            }
        }
    }

    // MARK: - The cache is written in one order

    /// The tail of the cache-write chain. Every mutation is appended to it.
    private var cacheWork: Task<Void, Never> = Task {}

    /// Run one cache mutation after every mutation asked for before it.
    ///
    /// **Actor isolation is not ordering, and that is the whole reason this
    /// exists.** `EventCache` being an actor guarantees its methods do not
    /// overlap; it promises nothing about *which* of two independently spawned
    /// tasks arrives first. Both hazards that follow from that are real:
    ///
    ///   * `flushCache` snapshots its events and *then* spawns a write. A
    ///     removal that clears the same session's file can land between the two,
    ///     and the write puts the file back. Dropping the key from
    ///     `pendingCacheWrites` cannot help — by then the snapshot is taken.
    ///   * every fleet reply spawns a `saveFleet`. One carrying a row that has
    ///     since been removed can land after the removal's own `saveFleet`, and
    ///     `fleet.json` is what a cold launch restores from.
    ///
    /// Chaining makes the order the order things were asked for, which is the
    /// only order that is ever right here. Nothing is dropped or coalesced: a
    /// stale `saveFleet` still runs, it just cannot run *last*.
    private func enqueueCacheWork(_ work: @escaping @Sendable (EventCache) async -> Void) {
        // A replayed fleet never reaches the disk. The cache is this app's
        // memory of a real Mac, and sample rows restored into a later paired
        // launch would be exactly the mixing the sample fleet is not allowed to
        // do — the one place it could outlive itself.
        guard !fixturesActive else { return }
        let previous = cacheWork
        let cache = self.cache
        cacheWork = Task {
            await previous.value
            await work(cache)
        }
    }

    /// Wait for everything queued so far to have been written.
    private func cacheSettled() async {
        await cacheWork.value
    }

    /// Forget every trace of a session that is no longer on the Mac.
    ///
    /// **Called from both routes, because a session leaves the fleet more ways
    /// than one.** This phone's swipe is the obvious one. The others are the
    /// common ones: another phone removed it, the operator ran
    /// `codeconnect prune` at the Mac, or the daemon's database was replaced.
    /// Reachable only from the swipe, this cleanup would leave a cache file and
    /// a review mark behind for every one of those — and nothing else ever
    /// sweeps that directory, so on a long-lived install it only grows. The
    /// review-mark map is capped at 200 and pruned oldest-first, so marks held
    /// for sessions that no longer exist are slots taken from sessions that do,
    /// and those rows announce work the user has already read as new.
    private func forgetLocalState(of key: String) {
        // **The controller goes with the run.** Keyed by `sessionKey`, which
        // falls back to a reused tmux name on a pre-uid daemon — so a later row
        // under the same key inherited the departed run's banners, its ten-second
        // cooldown, and its spent request material.
        codexControlsByKey.removeValue(forKey: key)
        // The run is gone, so there is nothing left for its material to reach.
        codexSpentLedger.forget(session: key)
        subscribed.remove(key)
        subscribedAfter.removeValue(forKey: key)
        states.removeValue(forKey: key)
        pendingCacheWrites.remove(key)
        // A departed run's unsettled mutations die with it. Left behind, a
        // reused session *name* would collide a brand-new send with a dead
        // mutation's identity and the daemon would refuse it as a conflict.
        pendingSendIdentities = pendingSendIdentities.filter { $0.key.key != key }
        // Its review mark and its diff go when the list without it is
        // published (`commitList`). The mark is gated exactly like the cache
        // line below: the sample fleet's `.sessions` frame lists only sample
        // runs, so an ungated forget would take one tap on "look around" as
        // permission to destroy every real run's review marks — the cached
        // fleet is loaded even unpaired.
        if !fixturesActive { departedMarks.insert(key) }
        enqueueCacheWork { await $0.clearEvents(key: key) }
    }

    /// The fleet on screen came off the wire, so it carries no age.
    ///
    /// Factored out of `handle(_:)` because `-CC_FIXTURE_CACHED` needs one place
    /// to stand: a fixture stages a fleet that came off *disk*, and a later
    /// frame must not be able to quietly relight it and delete the age with it.
    private func markFleetLive() {
        #if DEBUG
            if fixtureCachedFleet { return }
        #endif
        fleetSource = FleetSource(isLive: true)
    }

    /// Cold open: hold the last-known fleet, stamped with its age, for when the
    /// loader gives up waiting on a Mac that is not answering; a live list
    /// replaces it as soon as one arrives.
    private func loadCachedFleet() async {
        // A fixture run is hermetic: it *is* the cache, and reading the real one
        // over it is how half a previous live session ends up in a render.
        guard !fixturesActive else { return }
        guard let cached = await cache.loadFleet(), !fleetSource.isLive else { return }
        summaries = cached.sessions
        fleetSource = FleetSource(isLive: false, cachedAt: cached.cachedAt, restoredAt: Date())
        for summary in cached.sessions {
            _ = state(for: summary.sessionKey)
        }
        publishList()
        for summary in cached.sessions {
            let sessionState = state(for: summary.sessionKey)
            if let events = await cache.loadEvents(key: summary.sessionKey) {
                sessionState.loadCached(events)
            }
        }
    }

    // MARK: - Actions

    /// True while an answer for this card is on the wire. Owned by the model
    /// rather than the sheet: a sheet dismissed mid-flight and re-presented gets
    /// fresh `@State`, which would otherwise let the same card be answered twice
    /// concurrently — and concurrent answers are exactly what makes an outcome
    /// attributable to the wrong tap.
    func isAnswering(_ item: ApprovalItem) -> Bool { answersInFlight.contains(item.id) }

    /// The last thing the daemon said about this card, kept so re-opening it
    /// shows the confirmed outcome instead of an innocent-looking pending state.
    func lastAttempt(for item: ApprovalItem) -> AnswerAttempt? { answerAttempts[item.id] }

    // MARK: - Codex: stop the turn, say something

    /// One controller per session, created on demand. Held here rather than in a
    /// view so an in-flight stop survives the sheet being dismissed — and so the
    /// bounded grey after a link-state refusal is not reset by scrolling the
    /// fleet.
    func codexControls(for key: String) -> CodexControls {
        if let existing = codexControlsByKey[key] { return existing }
        // **A fixture run writes nothing durable.** The render harness presses
        // Stop and Compose for real, and two of the staged answers are
        // `indeterminate` — which is exactly the outcome that spends material
        // for ever. Persisted, the L pass would silently change what the AX5
        // pass photographs, and the second render of the same scenario would
        // show "this was already sent" instead of the state it names. The same
        // rule `SessionState.recordsReviewMarks` follows: a run that is not real
        // leaves no trace a sweep could never reach.
        let fresh = CodexControls(
            sessionKey: key, ledger: fixturesActive ? nil : codexSpentLedger)
        codexControlsByKey[key] = fresh
        return fresh
    }

    /// The turn this session is running, or nil when the phone holds none. The
    /// truth, for the send path; a screen reads `Shown.runningTurn`.
    ///
    /// Derived from the event envelopes, because there is no "a turn began"
    /// fact on this wire at all — see `CodexTurnTracker`. The approval event's
    /// own envelope `turn_id` is preferred when the Mac supplies one, and this is
    /// the fallback; the tracker already reads whichever is present,
    /// because both arrive as `Event.turnID`.
    func runningTurn(for key: String) -> String? {
        let controls = codexControls(for: key)
        // A turn this session saw end is over even where the window no longer
        // holds its terminal: what a compose observed, or a later event of the
        // same turn, never brings it back.
        return CodexTurnTracker.runningTurn(
            in: states[key]?.events ?? [],
            observed: controls.observedTurns,
            retired: controls.retiredTurns,
            knownEnded: states[key]?.endedTurns ?? [])
    }

    /// Why Stop is not offered on this session as drawn, or nil when it is.
    func stopUnavailable(for key: String) -> String? {
        guard let summary = shownSummary(for: key) else { return "This run is not in the fleet." }
        return CodexProse.stopUnavailable(
            agent: summary.agent,
            daemonHonoursStop: daemonProfile.stopsCodexTurns,
            link: summary.codexLink,
            runningTurn: shown(for: key)?.runningTurn)
    }

    /// The same rule against the truth: what decides whether a stop may leave.
    private func stopBlocked(for key: String) -> String? {
        guard let summary = summary(for: key) else { return "This run is not in the fleet." }
        return CodexProse.stopUnavailable(
            agent: summary.agent,
            daemonHonoursStop: daemonProfile.stopsCodexTurns,
            link: summary.codexLink,
            runningTurn: runningTurn(for: key))
    }

    /// Why the Codex composer cannot send as drawn, or nil when it can.
    func composeUnavailable(for key: String) -> String? {
        guard let summary = shownSummary(for: key) else { return "This run is not in the fleet." }
        return Self.composeUnavailable(summary, profile: daemonProfile)
    }

    /// The same rule against the truth: what decides whether a message may leave.
    private func composeBlocked(for key: String) -> String? {
        guard let summary = summary(for: key) else { return "This run is not in the fleet." }
        return Self.composeUnavailable(summary, profile: daemonProfile)
    }

    private static func composeUnavailable(_ summary: SessionSummary, profile: DaemonProfile)
        -> String?
    {
        CodexProse.composeUnavailable(
            agent: summary.agent,
            daemonUnderstandsCompose: profile.composesToCodex,
            link: summary.codexLink)
    }

    /// **Stop this Codex session's running turn.**
    ///
    /// Every value the frame carries is derived here, from facts this model
    /// holds, so a view cannot supply a turn the phone never saw or a session
    /// reference the hash was not computed over.
    ///
    /// **The whole gate lives here**, in one guard chain, and no surface
    /// re-derives any part of it: `stopUnavailable(for:)` answers the same
    /// question for the controls, by the same rule over what is drawn. Two
    /// rules is how the link state came to be enforced when drawing a button
    /// and not when sending a frame.
    @discardableResult
    func stopCodexTurn(sessionKey key: String) async -> InterruptResult? {
        let controls = codexControls(for: key)
        guard let summary = summary(for: key) else {
            controls.stopNotSent("This run is not in the fleet.")
            return nil
        }
        // Agent, daemon capability, running turn and **link state**, in the one
        // place that decides whether a frame may leave.
        if let blocked = stopBlocked(for: key) {
            controls.stopNotSent(blocked)
            return nil
        }
        guard let turn = runningTurn(for: key) else {
            controls.stopNotSent("Nothing is running to stop.")
            return nil
        }
        // **The uid or nothing.** The tmux name is handed to the
        // next run, so hashing and sending it can aim an abort at a session the
        // reader never saw. There is no safe fallback, so there is none.
        guard let reference = Self.sessionReference(summary) else {
            controls.stopNotSent(Self.noUidSentence)
            return nil
        }
        let hash = CodexHash.interrupt(sessionRef: reference, turnID: turn)

        // **Spent material is never re-sent.** A stop whose outcome nobody
        // knows — `indeterminate`, or a reply this phone never heard — was
        // issued, and the daemon's own sentence says it will not be sent again.
        // Neither will this.
        guard !controls.stopIsSpent(material: hash) else {
            controls.stopNotSent(Self.alreadyIssuedSentence(verb: "stop"))
            return nil
        }
        let requestID = controls.stopRequestID(material: hash)

        controls.beginStop()
        do {
            let result = try await connection.interrupt(
                session: reference, requestID: requestID, turnID: turn, payloadHash: hash)
            controls.settleStop(result, material: hash, turnID: turn, now: now)
            states[key]?.codexTurnsChanged()
            refreshFleet()
            return result
        } catch let refusal as DaemonConnection.CodexRefusal {
            controls.stopNotSent(Self.sentence(for: refusal, verb: "stop this turn"))
            return nil
        } catch ConnectionError.sentButUnanswered {
            // The frame left. Saying "nothing was sent" here would be the app
            // inventing a guarantee the transport never gave it.
            controls.stopSentNoAnswer(Self.sentNoAnswerSentence(verb: "stop"), material: hash)
            return nil
        } catch {
            controls.stopNotSent(error.localizedDescription)
            return nil
        }
    }

    /// **Say something to this Codex session.**
    @discardableResult
    func composeToCodex(sessionKey key: String, text: String) async -> ComposeResult? {
        let controls = codexControls(for: key)
        guard let summary = summary(for: key) else {
            controls.composeNotSent("This run is not in the fleet.")
            return nil
        }
        if let blocked = composeBlocked(for: key) {
            controls.composeNotSent(blocked)
            return nil
        }
        // Refused here as well as inside the connection: the reader deserves the
        // byte count in the app's own words rather than after a round trip, and
        // the connection's copy of the check is the one that guarantees no frame
        // leaves regardless of which caller forgot.
        if let blocked = ComposeDraft(text: text).blockedReason {
            controls.composeNotSent(blocked)
            return nil
        }
        guard let reference = Self.sessionReference(summary) else {
            controls.composeNotSent(Self.noUidSentence)
            return nil
        }
        let hash = CodexHash.compose(sessionRef: reference, text: text)

        // **The same words are never said twice** once their outcome is
        // unknown. Editing them makes a different message, which is a different
        // hash, and goes.
        guard !controls.composeIsSpent(material: hash) else {
            controls.composeNotSent(Self.alreadyIssuedSentence(verb: "send"))
            return nil
        }
        let requestID = controls.composeRequestID(material: hash)

        controls.beginCompose()
        do {
            let result = try await connection.compose(
                session: reference, requestID: requestID, text: text, payloadHash: hash)
            controls.settleCompose(result, material: hash)
            states[key]?.codexTurnsChanged()
            return result
        } catch let refusal as DaemonConnection.CodexRefusal {
            controls.composeNotSent(Self.sentence(for: refusal, verb: "carry this message"))
            return nil
        } catch ConnectionError.sentButUnanswered {
            controls.composeSentNoAnswer(
                Self.sentNoAnswerSentence(verb: "send"), material: hash)
            return nil
        } catch {
            controls.composeNotSent(error.localizedDescription)
            return nil
        }
    }

    /// The one string a mutation may name a session by: its uid. `nil` when
    /// there is none, which is a refusal and not a fallback.
    private static func sessionReference(_ summary: SessionSummary) -> String? {
        summary.sessionUID.isEmpty ? nil : summary.sessionUID
    }

    private static let noUidSentence =
        "This Mac has not given this run an id of its own, so nothing was sent."

    private static func alreadyIssuedSentence(verb: String) -> String {
        "This was already sent and what became of it is not known; it will not be \(verb) again. "
            + "Check the Mac."
    }

    private static func sentNoAnswerSentence(verb: String) -> String {
        "This left the phone and the Mac never answered, so what became of it is not known; "
            + "it will not be \(verb) again. Check the Mac."
    }

    /// A typed refusal in the app's own words. **Never dressed as the daemon's**
    /// — nothing was sent, so the daemon has said nothing about it.
    private static func sentence(
        for refusal: DaemonConnection.CodexRefusal, verb: String
    ) -> String {
        switch refusal {
        case .notAdvertised:
            return
                "This Mac's CodeConnect never said it could \(verb), so nothing was sent. Update it."
        case .notACodexSession(let agent):
            return "This is a \(agent) session, so nothing was sent."
        case .nothingToSend(let reason):
            return reason
        }
    }

    /// Answers are idempotent by `(session, request_id)` on the daemon side, so
    /// a retry after any failure here is always safe.
    func answer(item: ApprovalItem, decision: AnswerDecision) async -> AnswerAttempt {
        // **The decision must match the agent, checked on the send path.**
        //
        // The two vocabularies are mutually exclusive and the Mac refuses each
        // one aimed at the other by name. The view already picks the right
        // surface, but a view is not a guarantee: this closes the path itself,
        // so a deep link, a stale sheet or a future caller cannot transmit a
        // decision the daemon will only send back.
        // Unknown agent fails closed. `?? .claude` was the default here, and
        // `.claude` is the one vocabulary that transmits — so a card whose run
        // had left the fleet sent an `allow` for an agent nobody could name.
        guard let agent = summary(for: item.sessionKey)?.agent else {
            return .rejected(
                "This run is no longer on the fleet, so nothing was sent. Answer it at the Mac.")
        }
        if let mismatch = Self.decisionMismatch(
            decision: decision, agent: agent,
            resolvesCodexCards: daemonProfile.resolvesCodexCards,
            isQuestion: item.card.toolName == QuestionCard.toolName,
            answersQuestions: daemonProfile.answersQuestions,
            answersApprovalsByHook: daemonProfile.answersApprovalsByHook)
        {
            return .rejected(mismatch)
        }
        guard !answersInFlight.contains(item.id) else {
            return .failed("An answer for this card is already being sent.")
        }
        if let resolved = resolution(of: item) {
            answerAttempts[item.id] = resolved
            return resolved
        }
        answersInFlight.insert(item.id)
        defer { answersInFlight.remove(item.id) }
        let attempt = await sendAnswer(item: item, decision: decision)
        answerAttempts[item.id] = attempt
        return attempt
    }

    /// What became of this card by now, as the truth has it, or nil while it is
    /// still open there.
    ///
    /// Read after the reader's tap and any face check, never from what was
    /// drawn when they tapped: an answer the Mac recorded meanwhile — arrived
    /// and not yet drawn, or held back by a catch-up — leaves nothing to send.
    /// Rebuilt first, so an event ingested and not yet built still counts.
    private func resolution(of item: ApprovalItem) -> AnswerAttempt? {
        guard let state = states[item.sessionKey] else { return nil }
        state.rebuildNow()
        guard let card = state.approval(id: item.id) else { return nil }
        if let outcome = card.outcome {
            return AnswerAttempt.classify(duplicate: outcome, staleHash: false)
        }
        if card.codexResolution != nil {
            return .failed("This card was resolved on the Mac before your answer left, so nothing was sent.")
        }
        return nil
    }

    /// Why this decision cannot be sent to this agent, or nil when it can.
    ///
    /// Deliberately silent about `.unrecognised`: it is only ever *received*.
    /// `.text` is Claude's denial with a reason, which a Codex card does not
    /// take, and "the control is not drawn" is a view fact, not a guarantee.
    ///
    /// **A Claude question takes only `answers` and `decline`, and only from a
    /// daemon that understands them.** `allow` or an option on Claude's
    /// `AskUserQuestion` would pick answers nobody chose; this refuses them on
    /// the send path whatever the daemon advertises.
    ///
    /// **A Claude approval is answered only by a daemon that returns it through
    /// the card's own hook.** An older one types the answer into whatever prompt
    /// is on screen, which can be another agent's.
    static func decisionMismatch(
        decision: AnswerDecision, agent: AgentKind, resolvesCodexCards: Bool = true,
        isQuestion: Bool = false, answersQuestions: Bool = false,
        answersApprovalsByHook: Bool = false
    ) -> String? {
        switch (agent, decision) {
        case (.claude, .answers), (.claude, .decline):
            guard isQuestion else {
                return "Answers are for Claude's questions; this card is not one, "
                    + "so nothing was sent."
            }
            guard answersQuestions else {
                return "This Mac's CodeConnect is too old to answer Claude's questions from the "
                    + "phone, so nothing was sent. Answer at the Mac."
            }
            return nil
        case (.claude, _) where isQuestion:
            return "Claude is asking a question, which is answered by choosing its answers, "
                + "so nothing was sent."
        case (.claude, _) where !answersApprovalsByHook:
            return "This Mac's CodeConnect is too old to answer Claude's approvals from the "
                + "phone, so nothing was sent. Update it, or answer at the Mac."
        case (.codex, _) where !resolvesCodexCards:
            // **Old daemons.** Below minor 19 the resolution carries no `request_id`, so
            // an answered card can never be retired. A card that cannot be
            // retired must not be answered from here, by any vocabulary.
            return "This Mac's CodeConnect is too old to answer a Codex card from the phone, "
                + "so nothing was sent. Update it, or answer at the Mac."
        case (.codex, .allow), (.codex, .deny), (.codex, .option),
            (.codex, .text), (.codex, .answers), (.codex, .decline):
            return "A Codex card is answered by naming one of the options it offered, "
                + "so nothing was sent."
        case (.claude, .optionId):
            return "An option id is for a Codex session; this is a Claude session, "
                + "so nothing was sent."
        case (.unsupported(let raw), _):
            return "This app does not know how to answer a \(raw) session, so nothing was sent."
        default:
            return nil
        }
    }

    private func sendAnswer(item: ApprovalItem, decision: AnswerDecision) async -> AnswerAttempt {
        let card = item.card
        do {
            // Scoped to the run the card came from. On a uid daemon that ends
            // the ambiguity outright; on an older one it is a tmux name, which
            // the daemon uses to *refuse* rather than guess when the same
            // request id is open in two runs.
            let result = try await connection.answer(
                requestID: card.requestID, payloadHash: card.payloadHash, decision: decision,
                session: item.sessionKey)
            switch result {
            case .applied(let outcome):
                refreshFleet()
                return AnswerAttempt.classify(applied: outcome)
            case .duplicate(let outcome, let stale):
                refreshFleet()
                return AnswerAttempt.classify(duplicate: outcome, staleHash: stale)
            case .rejected(let reason):
                refreshFleet()
                return Self.classify(rejection: reason)
            }
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    /// `key` is a run, not a name. It reaches the daemon as the `session_uid`
    /// wherever there is one, because a name resolves to whichever run holds it
    /// *now* — and typing into the wrong agent's TTY is not a mistake that can
    /// be taken back.
    func send(text: String, to key: String, submit: Bool = true) async -> ComposeAttempt {
        // The policy stands between EVERY caller and the wire — the composer,
        // a denial reason, a diff comment. A deny reason that happens to
        // start with `/config` would otherwise recreate the measured Mac
        // dialog lockout through the side door.
        switch ClaudeCommandPolicy.action(
            for: text, recoversComposer: connection.capabilities?.recoversComposer == true)
        {
        case .nativeModel:
            return .refused("/model has its own control in the app — use the Model sheet.")
        case .nativeDiff:
            return .refused("/diff has its own view in the app — open Changes.")
        case .nativeEffort:
            return .refused("/effort has its own control in the app — use the Effort sheet.")
        case .nativeCompact:
            return .refused("/compact has its own control in the app — use the Compact sheet.")
        case .nativeClear:
            return .refused("/clear has its own confirmation in the app.")
        case .nativeSnapshot(let command):
            return .refused(
                "/\(command.rawValue) has its own view in the app — use the command palette.")
        case .blocked(_, let reason):
            return .refused(reason)
        case .passThrough:
            break
        }
        return await sendUnchecked(text: text, to: key, submit: submit)
    }

    /// The native adapters' own injections — the ONLY paths that may carry
    /// a slash command past the policy, because each sheet *is* the
    /// policy's answer for its command.
    /// The one caller that permits the daemon to *complete* Claude Code's
    /// confirmation rather than dismiss it. The Model sheet states both
    /// consequences — the new default, and the history re-read — above the rows
    /// it is tapped from, so the tap is the consent that key needs. Nothing
    /// else sets it, and the daemon still decides which commands it applies to.
    func sendModelCommand(_ argument: String, to key: String) async -> ComposeAttempt {
        await sendUnchecked(
            text: "/model \(argument)", to: key, submit: true,
            completeNativeConfirmation: true)
    }

    /// The Effort sheet's own injection. Like `sendModelCommand`, it permits
    /// the daemon to complete Claude Code's confirmation, because the sheet
    /// states the cost above the rows it is tapped from. A *typed*
    /// `/effort <value>` passes through instead and never sets this, so it gets
    /// the ordinary rescue — there is no disclosure behind it to point at.
    func sendEffortCommand(_ value: String, to key: String) async -> ComposeAttempt {
        await sendUnchecked(
            text: "/effort \(value)", to: key, submit: true,
            completeNativeConfirmation: true)
    }

    func sendCompactCommand(instructions: String, to key: String) async -> ComposeAttempt {
        let trimmed = instructions.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = trimmed.isEmpty ? "/compact" : "/compact \(trimmed)"
        return await sendUnchecked(text: text, to: key, submit: true)
    }

    func sendClearCommand(to key: String) async -> ComposeAttempt {
        await sendUnchecked(text: "/clear", to: key, submit: true)
    }

    /// `/status`, `/usage`, `/cost` — the send whose *result* is the point:
    /// on `.composerRecovered` the daemon returns the pane it saved while
    /// the Mac view was open, and the snapshot sheet renders exactly that.
    func sendSnapshotCommand(_ command: SnapshotCommand, to key: String) async -> ComposeAttempt {
        await sendUnchecked(text: "/\(command.rawValue)", to: key, submit: true)
    }

    private func sendUnchecked(
        text: String, to key: String, submit: Bool,
        completeNativeConfirmation: Bool = false
    ) async -> ComposeAttempt {
        // Every typed-text route funnels through here — the compose bar, the
        // Model/Effort/Compact sheets, `/clear` — so this is where the sample
        // fleet is answered once, in its own vocabulary, rather than per
        // surface. A surface the sample forgot to gate otherwise falls
        // through to the link's sentence ("Not connected to the daemon"),
        // which is a dead end in a fleet whose banner says nothing is
        // connected.
        if sampleFleetActive {
            return .failed("These agents are not real. Pair with your Mac to talk to your own.")
        }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return .failed("Nothing to send.") }
        guard connection.capabilities?.sendText != false else {
            return .failed("This daemon does not accept typed text.")
        }
        // Every send carries an identity, so a retry is recognisable instead
        // of being typed twice. The id is fresh per mutation — except after
        // an indeterminate outcome, where re-sending the *same mutation*
        // (same session, text, and submit flag) reuses the original id on
        // purpose: that is the retry the ledger exists to recognise, and the
        // daemon replays the settled outcome instead of typing again.
        let mutation = SendMutationKey(key: key, text: trimmed, submit: submit)
        let requestID = pendingSendIdentities[mutation] ?? UUID().uuidString.lowercased()
        let payloadHash = SendTextIdentity.payloadHash(
            session: key, text: trimmed, submit: submit)
        do {
            let result = try await connection.sendText(
                session: key, text: trimmed, require: .inputBox, submit: submit,
                requestID: requestID, payloadHash: payloadHash,
                completeNativeConfirmation: completeNativeConfirmation)
            switch result {
            case .sent(let matched):
                pendingSendIdentities[mutation] = nil
                return .sent(matched: matched)
            case .refused(let reason):
                // A refusal promises nothing was typed, so the next attempt
                // is a fresh mutation, not a retry of this one.
                pendingSendIdentities[mutation] = nil
                return .refused(reason)
            case .duplicate(_, let appliedAt):
                pendingSendIdentities[mutation] = nil
                return .alreadyApplied(appliedAt: appliedAt)
            case .composerRecovered(_, let paneSnapshot, let capturedAt):
                pendingSendIdentities[mutation] = nil
                return .composerRecovered(
                    command: trimmed, paneSnapshot: paneSnapshot, capturedAt: capturedAt)
            case .composerLost:
                // **The identity is kept, and the draft with it.** The keys
                // landed; the daemon has settled this mutation, so pressing
                // send again after recovering the Mac by hand is recognised
                // as the duplicate it is instead of typing the command a
                // second time into whatever is on screen by then. Dropping
                // the identity here while the composer deliberately keeps the
                // text was a way to type it twice.
                retainIdentityIfListed(mutation, requestID)
                return .composerLost(command: trimmed)
            case .indeterminate(let reason):
                retainIdentityIfListed(mutation, requestID)
                return .indeterminate(reason)
            }
        } catch {
            // The transport died before an answer. The mutation may have been
            // claimed, so the identity is kept for the same reason as above.
            retainIdentityIfListed(mutation, requestID)
            return .failed(error.localizedDescription)
        }
    }

    /// Keeps an unsettled mutation's identity — unless its session departed
    /// while the send was in flight. The departure sweep already ran; writing
    /// after it would park a dead identity under a session *name* the next
    /// run may reuse, and that run's first send would be refused as a
    /// conflict with a mutation it never made.
    private func retainIdentityIfListed(_ mutation: SendMutationKey, _ requestID: String) {
        guard summaries.contains(where: { $0.sessionKey == mutation.key }) else { return }
        pendingSendIdentities[mutation] = requestID
    }

    /// Unsettled mutations, each under its full identity key. A dictionary,
    /// not a slot: an indeterminate send in one session must survive settled
    /// sends in every other, or the promised retry recognition silently
    /// stops holding exactly when two sessions are busy.
    private struct SendMutationKey: Hashable {
        let key: String
        let text: String
        let submit: Bool
    }
    private var pendingSendIdentities: [SendMutationKey: String] = [:]

    func capturePane(key: String, lines: UInt32 = 80) async -> String? {
        guard connection.capabilities?.capture != false else { return nil }
        return try? await connection.capture(session: key, lines: lines)
    }

    // MARK: - Diff

    func diffState(for key: String) -> DiffState { diffs[key] ?? .idle }

    /// Ask the daemon for this run's working-tree diff.
    ///
    /// On demand only, and never automatically refreshed: a diff is a thing you
    /// ask for when you are about to read it. The result is stamped with the
    /// daemon's own `captured_at` so the view can say how old what you are
    /// reading is, rather than implying it is live.
    func loadDiff(key: String, force: Bool = false) {
        // The sample fleet's diff is preloaded by `startSampleFleet` and is
        // the only diff its sessions will ever have: a refresh has no daemon
        // to ask, and falling through would overwrite the loaded diff with a
        // failure written in the link's vocabulary.
        if sampleFleetActive { return }
        if !force, diffs[key]?.isLoading == true { return }
        guard connection.phase.isConnected else {
            // The app's own sentence about its own link. Tagged `.app` so the
            // screen renders its link state instead of quoting this to the Mac.
            diffs[key] = .failed(.app(linkHealth.disabledReason ?? "Not connected to the daemon."))
            return
        }
        diffs[key] = .loading
        let askedAt = Date()
        Task { [weak self] in
            guard let self else { return }
            do {
                let diff = try await self.connection.diff(session: key)
                self.diffs[key] = .loaded(
                    diff, parsed: UnifiedDiff.parse(diff.unified), fetchedAt: Date())
            } catch {
                // A daemon that does not know `get_diff` never answers it. If it
                // complained *on the wire* while we were waiting, quote the
                // complaint; otherwise say plainly that nothing came back — in
                // the app's own voice, because `ConnectionError`'s strings are
                // the app's ("The daemon did not answer in time"), as are the
                // connection's own notes about a closed socket, and only what
                // actually arrived from the Mac may be quoted as the Mac's.
                let hint =
                    self.daemonProfile.servesDiff
                    ? ""
                    : " This daemon never advertised diff support, which would explain it."
                if let quoted = self.connection.daemonErrorReported(since: askedAt) {
                    self.diffs[key] = .failed(.daemon(quoted + hint))
                } else {
                    let observed =
                        self.connection.errorReported(since: askedAt)
                        ?? error.localizedDescription
                    self.diffs[key] = .failed(.app(observed + hint))
                }
            }
        }
    }

    func clearDiff(key: String) { diffs[key] = nil }

    /// The daemon reports refusals in prose; these are the two that mean
    /// something specific to the user rather than "try again".
    static func classify(rejection reason: String) -> AnswerAttempt {
        let lowered = reason.lowercased()
        if lowered.contains("not on screen") {
            return .answeredAtKeyboard(reason)
        }
        if lowered.contains("already-resolved") || lowered.contains("already being applied") {
            return .answeredAtKeyboard(reason)
        }
        // A question the Mac answered first: Claude took the keyboard's answer
        // and the phone's was not used.
        if lowered.contains("answered at the mac") {
            return .answeredAtKeyboard(reason)
        }
        if lowered.contains("stale payload_hash") || lowered.contains("out of date") {
            return .staleCard(reason)
        }
        return .rejected(reason)
    }
}

// MARK: - Small helpers

extension String {
    /// Up to the first line break the screen draws — `\n`, `\r\n`, `\r`, form
    /// feed, U+0085, U+2028, U+2029 — without its surrounding spaces.
    ///
    /// Swift reads `\r\n` as one `Character`, so a split on `"\n"` let a
    /// Windows-style command through whole: drawn as its first line, and
    /// spoken as both. `Character.isNewline` also counts a vertical tab, which
    /// `Text` draws as nothing and runs on through (measured: one line, as wide
    /// as without it), so a cut there hid the rest of a command the screen
    /// would have shown — `printf 'a<VT>b' | rm -rf ~/work` as `printf 'a`.
    var firstLine: String {
        String(prefix { !$0.isNewline || $0 == "\u{0B}" })
            .trimmingCharacters(in: .whitespaces)
    }

    /// An agent's text as the screen may draw it: NUL, U+061C and the bidi
    /// embeddings, overrides and isolates (U+202A–202E, U+2066–2069) each
    /// become U+FFFD, and nothing else changes.
    ///
    /// Drawn as they arrive, those characters show something other than what
    /// runs: an override reverses the rest of its line, so `git push origin
    /// <U+202E>niam ecrof--` read `git push origin --force main` under
    /// "Verified", and a NUL is drawn as nothing. Claude Code's own renderer
    /// draws the same bidi characters as U+FFFD (2.1.308). For display only —
    /// the wire, the hash and the answer keep the bytes.
    var displaySafe: String {
        var scalars = String.UnicodeScalarView()
        for scalar in unicodeScalars {
            switch scalar.value {
            case 0x00, 0x061C, 0x202A...0x202E, 0x2066...0x2069: scalars.append("\u{FFFD}")
            default: scalars.append(scalar)
            }
        }
        return String(scalars)
    }
}


extension Optional where Wrapped == DeleteSessionResult {
    /// What to tell the person who swiped, or `nil` when the run is gone.
    ///
    /// **`nil` — the request never got an answer — is a refusal too.** It is the
    /// one this used to lose: `removeSession` reaches the daemon through `try?`,
    /// so a dropped link, a timeout, or a second swipe while the first is still
    /// in flight all arrive here as `nil`, and reading that as success would
    /// close the row over a session the Mac still has.
    ///
    /// Short because it is rendered inside the revealed button, which is as wide
    /// as the word "Remove" and cannot grow.
    var refusal: String? {
        switch self {
        case .deleted, .notFound: nil
        case .stillRunning: "Still running"
        // The Mac's own word, because it has none better: it never established
        // what happened to this run, and saying "still running" here would
        // invent the one fact it is missing.
        case .notExited(let lifecycle): "Mac says “\(lifecycle)”"
        case .failed(let message): message
        case .unknown(let status): "Mac said “\(status)”"
        case .none: "No answer"
        }
    }
}
