import Foundation
import Observation
import SwiftUI
import UserNotifications

/// What happened to an answer, in terms the UI can be honest about.
public enum AnswerAttempt: Sendable {
    case applied(AnswerOutcome)
    /// The daemon accepted the answer but **cannot confirm it landed** —
    /// `AnswerOutcome.indeterminate`. Kept apart from `.applied` so nothing ever
    /// renders an unconfirmed answer as "confirmed by your computer": a positive
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
    /// The answer left this phone but its outcome never came back — a transport
    /// `sentButUnanswered`: the bytes may have landed and the daemon may still
    /// apply it, so re-answering could collide with the one already sent. The
    /// controls close (terminal), but it does NOT settle the card off the Deck —
    /// the agent may still be blocked — and the computer's own `approval_resolved`
    /// outranks it when it arrives.
    case sentUnconfirmed(String)

    public var isTerminal: Bool {
        switch self {
        case .applied, .indeterminate, .duplicate, .answeredAtKeyboard, .sentUnconfirmed: return true
        case .staleCard, .rejected, .failed: return false
        }
    }

    /// Whether a terminal attempt also settles the card off the Deck. A
    /// `.sentUnconfirmed` closes the controls but keeps the card: the agent may
    /// still be blocked on an answer that never confirmed, so it waits for the
    /// computer's own outcome instead of clearing.
    public var settlesCard: Bool {
        if case .sentUnconfirmed = self { return false }
        return isTerminal
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
    static func classify(duplicate outcome: AnswerOutcome, staleHash: Bool) -> AnswerAttempt {
        outcome.indeterminate
            ? .indeterminate(outcome) : .duplicate(outcome: outcome, staleHash: staleHash)
    }
}

public enum ComposeAttempt: Sendable, Equatable {
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
public enum DiffFailure: Sendable, Equatable {
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

/// Where a run's action from its Changes panel stands (`AppModel.gitActionState(for:)`): one at a time, a commit or a
/// pull request.
public enum GitActionState: Sendable, Equatable {
    case idle
    case working
    /// The computer opened this pull request, or found it open: kept until the run's repository, read again, has it (GitHub
    /// is asked at most every 30 s).
    case opened(number: UInt64, url: String)
    /// The computer did not do `step`, or may not have: why, in the app's words.
    case refused(String, step: GitStep)
}

/// The step a Changes panel action takes.
public enum GitStep: Sendable, Equatable {
    case commit
    case pullRequest
    case archive
}

/// The state of one session's on-demand diff.
public enum DiffState: Sendable {
    case idle
    case loading
    case loaded(SessionDiff, parsed: UnifiedDiff, fetchedAt: Date)
    case failed(DiffFailure)

    public var isLoading: Bool {
        if case .loading = self { return true }
        return false
    }

    /// A request that ended in the app's own account of the link, rather than
    /// in anything the daemon said. The screen has a state of its own for this
    /// and must not fall through to the failure state.
    public var failedOnTheLink: Bool {
        if case .failed(.app) = self { return true }
        return false
    }
}

/// What the relay enrollment path is doing, folded into the push test-button
/// explanation. Only a relay daemon drives this; direct and off stay `.idle`.
public enum RelayPushState: Equatable, Sendable {
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

/// One machine's state: its daemon, its sessions, its event stream. `Machines`
/// holds one per paired machine and makes their fleet.
@MainActor
@Observable
public final class AppModel {
    /// The machine this model is for, by `PairedMachine.id`. Nil only for the one
    /// model there is with nothing paired — the welcome screen's, the sample
    /// fleet's, a fixture run's. A pairing that has not finished has its id
    /// already, so every frame of its link reaches its own cache.
    public let machineID: String?
    public let pairing: PairingStore
    public let connection: DaemonConnection
    /// A run's shell on this computer, as this link shows it (`RunTerminal`).
    public let terminal: RunTerminal
    public let settings: AppSettings
    /// This machine's folder of the event cache.
    private let cache: EventCache
    /// Called once this machine's pairing is saved: its daemon has answered it.
    @ObservationIgnored var onPaired: (() -> Void)?
    /// Called at the end of every `hello_ack`, once what the daemon said about
    /// itself is kept: the last thing the handshake does on this model.
    @ObservationIgnored var onIdentified: (() -> Void)?
    /// Called with each list of the owner's other computers this machine's
    /// daemon sends.
    @ObservationIgnored var onPeers: ((PeerDirectory) -> Void)?
    /// What this machine's daemon last said about its own computer's Tailscale.
    public private(set) var tailnet: PeerDirectory?

    /// The Mac's list of runs, as it last said. The truth: what is on screen is
    /// `shownSummaries`.
    private(set) var summaries: [SessionSummary] = []
    #if DEBUG
        /// Folders made on the fixture computer, by parent.
        private var madeFolders: [String: [String]] = [:]
    #endif
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
    /// **One hosted Claude run's model / effort / mode call state** (B8, minor 39): whether a set call
    /// is in flight — the menus are then disabled — and the last answer still worth showing. Observed
    /// (not `@ObservationIgnored`), so a change redraws the controls. The selection is never kept here:
    /// it follows the summary's facts, read through `RunSettings`.
    private var settingStateByKey: [String: SettingState] = [:]
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
    public var hasLiveFleet: Bool { shownFleetSource.isLive }
    public var fleetCachedAt: Date? { shownFleetSource.cachedAt }
    public var fleetCacheRestoredAt: Date? { shownFleetSource.restoredAt }
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
    public private(set) var now = Date()
    /// One diff per session, keyed like `states`, fetched only when asked for.
    private(set) var diffs: [String: DiffState] = [:]
    /// Each run's repository as its computer last read it (`runGit(for:)`), and the runs a read is on its way for.
    private(set) var runGits: [String: RunGitReply] = [:]
    private var runGitsReading: Set<String> = []
    /// Each run's action from its Changes panel: on its way, or what the computer said when it did not do it.
    private(set) var gitActions: [String: GitActionState] = [:]

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
    public private(set) var isCatchingUp = false
    @ObservationIgnored private var releasing = false
    @ObservationIgnored private var stallWatch: Task<Void, Never>?
    /// `sessions` requests written and replies taken on the link
    /// `sessionsCountGeneration`.
    @ObservationIgnored private var sessionsWritten = 0
    @ObservationIgnored private var sessionsAnswered = 0
    @ObservationIgnored private var sessionsCountGeneration = -1
    /// A fact the fleet had not seen came while a `sessions` request was unanswered (`askFleetForFact`).
    @ObservationIgnored private var factAskWaiting = false
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

    /// `cache` is every machine's; this model reads and writes its own folder of
    /// it.
    init(
        machineID: String? = nil, pairing: PairingStore = PairingStore(),
        cache: EventCache = EventCache(), settings: AppSettings = AppSettings(),
        relayEnrollment: RelayEnrollment = RelayEnrollment(),
        codexSpentLedger: CodexSpentLedger = CodexSpentLedger(),
        pushRegistrar: (any PushRegistrar)? = nil
    ) {
        self.machineID = machineID
        self.pairing = pairing
        self.connection = DaemonConnection()
        self.terminal = RunTerminal(connection: connection)
        self.settings = settings
        self.cache = machineID.map { cache.machine($0) } ?? cache
        self.relayEnrollment = relayEnrollment
        self.codexSpentLedger = codexSpentLedger
        self.pushRegistrar = pushRegistrar
        // A figure from a link that is gone is not current.
        connection.onDisconnected = { [weak self] in
            self?.machineStats = nil
            self?.filesLostWithLink()
            self?.terminal.linkDown()
        }

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
            self.terminal.linkUp()
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
            // The account book, so a switch's rows name an account by its email; kept current by the computer's
            // pushes from then on. Asked only of a computer that keeps accounts.
            Task { await self.loadAccounts() }
        }
        connection.onDeviceToken = { [weak self] token in self?.adopt(deviceToken: token) }
        connection.onTransportSettled = { [weak self] useTLS in
            // A pairing's link is not yet the saved one, whose transport this
            // would rewrite.
            guard let self, self.pendingPairing == nil, let machineID = self.machineID else {
                return
            }
            self.pairing.noteTransport(useTLS: useTLS, id: machineID)
        }
        installPushWiring()
    }

    // MARK: - Lifecycle

    /// Asking iOS for notification permission, and telling the daemon where to
    /// push once Apple issues a token.
    private let pushRegistrar: (any PushRegistrar)?
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
    public func pushAuthorizationStatus() async -> UNAuthorizationStatus {
        await pushRegistrar?.authorizationStatus() ?? .notDetermined
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
    public private(set) var relayPushState: RelayPushState = .idle
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
        pushRegistrar?.onAuthorization = { [weak self] granted in
            self?.pushAuthorized = granted
        }
        pushRegistrar?.onToken = { [weak self] token, environment in
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
            pushRegistrar?.registerForRemoteNotifications()
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
            pushRegistrar?.requestAndRegister()
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
            pushRegistrar?.registerForRemoteNotifications()
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
    public func resetNotificationRegistration() {
        Task {
            await relayEnrollment.reset()
            relayPushState = .idle
            nextRelayRefreshAllowed = .distantPast
            deliverPushToken()
        }
    }

    /// A notification opened the Deck. Opened while this machine's catch-up
    /// holds the screen — and one does from the moment the app leaves — the
    /// Deck waits for it and says so (`deckAwaitsCatchUp`), then shows its cards
    /// in one go.
    func deckOpenedFromNotification() {
        if isCatchingUp { deckAwaitsCatchUp = true }
    }

    /// The Deck a notification opened is waiting for this machine's catch-up to
    /// end.
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

    /// Start this machine's clocks, put its saved fleet on screen, and dial it.
    func start() {
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
            if !isPairing, let endpoint {
                connect(to: endpoint)
                // Push registration deliberately does NOT happen here: before
                // the handshake the daemon's capabilities are unknown, and
                // asking for notification permission on behalf of a daemon that
                // may not advertise push is a system prompt nothing can honour.
                // `onConnected` registers once the `hello_ack` says it can ring.
            }
        }
    }

    /// Start the connection. One connection carries everything: the timeline
    /// and approvals.
    private func connect(to endpoint: DaemonEndpoint) {
        connection.start(endpoint: endpoint)
    }

    /// The daemon answered a pairing code with a durable device token.
    private func adopt(deviceToken: String) {
        guard let machineID, let endpoint = pendingPairing ?? endpoint else { return }
        pairing.adopt(deviceToken: deviceToken, from: endpoint, id: machineID)
        pendingPairing = nil
        onPaired?()
        // No push call here: `onConnected` fires for this same handshake and
        // is the one registration site — a second call from the pairing path
        // was a duplicate workflow, measured as stacked observers.
    }

    #if DEBUG
        /// How many of `-CC_FIXTURE_LATE_EVENTS`'s held-back frames have been
        /// delivered, so a test can see an event land behind a screen that
        /// does not draw it. Not observed: reading it redraws nothing.
        public static var lateFramesDelivered = 0

        /// Test seam: `-CC_RESET_CACHE` starts an automation launch with this
        /// machine's saved history gone.
        func resetCacheForAutomation() {
            Task { [cache] in await cache.clearAll() }
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
                case .putFile(_, let uploadID, _, let size, _, _):
                    // A fixture computer takes every file whole (`-CC_FIXTURE_UPLOADS`).
                    connection?.injectForTesting(.putFileResult(uploadID: uploadID, result: .received(size)))
                case .compose(let session, let requestID, _, _, _):
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

        /// Test seam: the second fixture machine online, with its own
        /// sessions and projects, so a start can choose between two computers.
        func applySecondMachineFixtures() {
            fixturesActive = true
            connection.fixtureAnswers = true
            connection.simulateConnectedForTesting()
            for message in Fixtures.secondMachineFrames() { connection.injectForTesting(message) }
            keepFixtureLinkFresh()
        }

        /// The fixture answer to `start_session`. `-CC_START_OUTCOME` names a
        /// failure to end in, `trust` for the folder question (asked once: the answered resend starts), or `pending` to stay starting; otherwise the start
        /// succeeds and the new session arrives as a real one does: listed,
        /// then its first events. `instant` answers and lists it at once, before the starting screen appears, as a
        /// computer that answers faster than the start sheet closes does.
        func answerFixtureStart(_ requestID: String) {
            guard let start = starter.starts[requestID] else { return }
            let wanted = UserDefaults.standard.string(forKey: "CC_START_OUTCOME")
            if wanted == "pending" { return }
            Task { @MainActor [weak self] in
                if wanted != "instant" { try? await Task.sleep(for: .milliseconds(wanted == nil ? 1500 : 600)) }
                guard let self else { return }
                if wanted == "trust", start.trust == nil {
                    self.starter.resolve(requestID, .needsTrust(path: start.project.path))
                    return
                }
                if let wanted, let reason = StartFailure(rawValue: wanted) {
                    self.starter.resolve(
                        requestID, .failed(reason, detail: Fixtures.startFailureDetail(reason, start)))
                    return
                }
                let uid = "01K1START" + String(requestID.prefix(8)).uppercased()
                self.starter.resolve(requestID, .started(sessionUID: uid))
                if wanted != "instant" { try? await Task.sleep(for: .milliseconds(400)) }
                for message in Fixtures.startedSessionFrames(start, uid: uid, listed: self.summaries) {
                    self.connection.injectForTesting(message)
                }
            }
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
        func applyFixtures() {
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
                applyFixtureDiff()
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
            // `-CC_SWITCH <recording>` replays the daemon's recording of a run moved to another account, to the step
            // `-CC_SWITCH_STEP` names (`SwitchFixtures`).
            if let recording = SwitchFixtures.recording {
                fixturesActive = true
                connection.fixtureAnswers = true
                connection.simulateConnectedForTesting()
                for message in SwitchFixtures.frames(recording: recording) {
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
            // `-cc.debug.sendText` resolves typed sends locally, so a send's
            // outcomes can be rendered without a Mac answering on cue.
            if let sendMode = UserDefaults.standard.string(forKey: "cc.debug.sendText") {
                connection.sendTextStub = { _, _ in Fixtures.sendTextResult(mode: sendMode) }
            }
            connection.simulateConnectedForTesting()
            // **`-CC_FIXTURE_LATE_APPROVALS YES` is a socket still catching
            // up.** A cold launch from a notification tap consumes its deep
            // link before the history that carries the decision has arrived.
            // The stream is split at the first approval and the tail delivered
            // on the test's `late-approvals` signal (`FixtureSignal`), in order —
            // held back whole rather than approvals alone, because a fixture that
            // skips ahead reads as a gap. `DeepLinkTests` and
            // `FleetRowChangeUITests` drive it.
            let lateApprovals = UserDefaults.standard.bool(forKey: "CC_FIXTURE_LATE_APPROVALS")
            var frames = Fixtures.frames(variant: variant)
            // `-CC_FIXTURE_USAGE YES|limits`: each run's account, and with YES its context (`Fixtures.withUsage`); the
            // account's limits are `-CC_ACCOUNTS_WIRE`'s, loaded here so the reply bar has them from the start.
            if let usage = UserDefaults.standard.string(forKey: "CC_FIXTURE_USAGE") {
                frames = Fixtures.withUsage(frames, context: usage != "limits")
            }
            // Read from the arguments as given: defaults would take its JSON for a property list.
            let arguments = ProcessInfo.processInfo.arguments
            if let flag = arguments.firstIndex(of: "-CC_FIXTURE_FAILED"), flag + 1 < arguments.count {
                frames = Fixtures.withFailedTurn(frames, failed: arguments[flag + 1])
            }
            if fixtureAccountBook != nil { Task { await loadAccounts() } }
            // **`-CC_FIXTURE_LATE_EVENTS <n>` is events arriving while the
            // reader is on screen.** The stream's last `n` frames are held back
            // and delivered in stream order, the `i`th on the test's `late-<i>`
            // signal, so a UI test can act first and then see how each event is
            // counted. `SessionShowAllUITests` and `FleetRowRenderSerialUITests`
            // drive it.
            let lateEvents = UserDefaults.standard.integer(forKey: "CC_FIXTURE_LATE_EVENTS")
            if lateEvents > 0, frames.count >= lateEvents {
                let late = Array(frames.suffix(lateEvents))
                frames.removeLast(lateEvents)
                Task { @MainActor [weak connection] in
                    for (index, frame) in late.enumerated() {
                        await FixtureSignal.wait("late-\(index + 1)")
                        connection?.injectForTesting(frame)
                        Self.lateFramesDelivered += 1
                    }
                }
            }
            // **`-CC_FIXTURE_TAIL_EVENTS <n>`** lands n more of `fx-5`'s tool calls, 20 ms apart, from 2.4 s in: a
            // run that keeps working while the reader reads, for a load no fixture's own frames reach.
            let tailEvents = UserDefaults.standard.integer(forKey: "CC_FIXTURE_TAIL_EVENTS")
            if tailEvents > 0 {
                Task { @MainActor [weak connection] in
                    try? await Task.sleep(for: .milliseconds(2400))
                    for index in 0..<tailEvents {
                        if let frame = Fixtures.tailEvent(seq: UInt64(28 + index)) {
                            connection?.injectForTesting(frame)
                        }
                        try? await Task.sleep(for: .milliseconds(20))
                    }
                }
            }
            if lateApprovals,
                let first = frames.firstIndex(where: {
                    if case .event(let event) = $0 { return event.kind == .approvalRequest }
                    return false
                })
            {
                let held = Array(frames[first...])
                frames.removeSubrange(first...)
                Task { @MainActor [weak connection] in
                    await FixtureSignal.wait("late-approvals")
                    for message in held { connection?.injectForTesting(message) }
                }
            }
            for message in frames {
                connection.injectForTesting(message)
            }
            #if DEBUG
                applyGapFixture()
            #endif
            applyFixtureDiff()
            applyCachedFixture()
            applyPushStateFixture()
            // **`-CC_FIXTURE_LINK_DROP YES` drops the link once, on the test's
            // `link-drop` signal,** through the connection's own `stop()` — every
            // waiter failed, the handshake and the Mac's capabilities forgotten —
            // and brings it back on `link-return` with the handshake the fixture
            // ended up with (after the push seam, which replaces it). It is how a fixture run
            // takes the Mac's removal capability away and gives it back, with
            // the rows still on screen. It is narrower than a real drop: no
            // socket closes, no backoff runs, no redial is counted.
            // **`-CC_FIXTURE_REMOVE_HELD YES` holds a removal's answer until the
            // test's `remove` signal,** then refuses it the way every fixture
            // removal is refused, with no answer. Without it a fixture refuses at once, so a removal still
            // on its way — "Removing…", and whatever happens around it — is a
            // state no fixture run can show.
            if UserDefaults.standard.bool(forKey: "CC_FIXTURE_REMOVE_HELD") {
                connection.deleteStub = { _ in
                    await FixtureSignal.wait("remove")
                    throw ConnectionError.sentButUnanswered
                }
            }
            if UserDefaults.standard.bool(forKey: "CC_FIXTURE_LINK_DROP"), let ack = connection.helloAck {
                Task { @MainActor [weak connection] in
                    await FixtureSignal.wait("link-drop")
                    connection?.stop()
                    connection?.setPhaseForTesting(.connecting)
                    await FixtureSignal.wait("link-return")
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
            // the frame that showed it. `LinkHealth.staleAfter` is 30s, so the
            // state arrives on its own about half a minute in.
            //
            // **`-CC_FIXTURE_LINK refused-version` / `refused-pairing`** puts the link where the computer refused it,
            // for a different version or for its pairing, with the fleet still on screen: no fixture reached a refusal,
            // which only a live pairing produces, so the word each refusal says could not be rendered.
            switch UserDefaults.standard.string(forKey: "CC_FIXTURE_LINK") {
            case "stale": return
            // `-CC_FIXTURE_LINK offline`: the computer stopped answering, its runs still listed as it last said them.
            case "offline":
                connection.setPhaseForTesting(
                    .waiting(until: Date().addingTimeInterval(60), reason: "The computer did not answer."))
                return
            case "refused-version":
                connection.setRefusedForTesting(
                    .differentVersion,
                    reason: "That computer runs a different version of CodeConnect. Update CodeConnect on it and on "
                        + "this \(HostDevice.current.name), then try again.")
                return
            case "refused-pairing":
                connection.setRefusedForTesting(.credential, reason: "This \(HostDevice.current.name) is not paired with it.")
                return
            default: break
            }
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
    public private(set) var fixturesActive = false

    /// This computer's projects, and the sessions this phone started on it.
    public let starter = SessionStarter()

    // MARK: Accounts (minor 48)

    /// This computer's accounts per agent and the one each agent's starts use, once asked; nil before.
    /// Held in memory only: no account's email is written to the cache or the store.
    public private(set) var accountBook: AccountBook? {
        didSet {
            guard accountBook != oldValue else { return }
            for state in states.values { state.accountNamesChanged() }
        }
    }

    /// What run `key`'s reply box shows beside its send button (`ReplyBar`), from what this app holds; nil for a run it
    /// does not list.
    public func replyBar(for key: String, now: Date) -> ReplyBar? {
        guard let summary = summary(for: key) else { return nil }
        return ReplyBar.of(events: states[key]?.events ?? [], summary: summary, book: accountBook, now: now)
    }

    /// The band over run `key`'s reply box while it is at a usage limit (`LimitFoot`); nil when it is not, or for a run
    /// this app does not list.
    public func limitFoot(for key: String, now: Date) -> LimitFoot? {
        summary(for: key).flatMap { LimitFoot.of(summary: $0, book: accountBook, now: now) }
    }

    /// An account's email as this computer's account book knows it, for a switch's rows on run `key`: by its id (nil:
    /// the agent's Default), under the run's agent. Nil while the book is not in, or for an account it no longer lists.
    private func accountEmail(run key: String, id: String?) -> String? {
        guard let agent = summary(for: key).flatMap({ StartAgent(rawValue: $0.agent.rawValue) }) else { return nil }
        return accountBook?.facts(for: agent, accountID: id)?.email
    }
    /// What the computer said when it refused, or could not answer, the last accounts request; cleared as the next
    /// one starts.
    public private(set) var accountsProblem: String?
    /// An accounts request is in flight; one at a time.
    public private(set) var accountsBusy = false

    /// This computer keeps accounts per agent: it advertises `accounts`. A computer that does not shows none.
    public var keepsAccounts: Bool {
        #if DEBUG
            if fixtureAccountBook != nil { return true }
        #endif
        return connection.capabilities?.keepsAccounts == true
    }

    /// Asks this computer for its accounts.
    public func loadAccounts() async {
        #if DEBUG
            if let fixture = fixtureAccountBook {
                accountBook = fixture
                // `-CC_ACCOUNTS_PROBLEM <words>`: the page as it stands after the computer refused a change.
                accountsProblem = UserDefaults.standard.string(forKey: "CC_ACCOUNTS_PROBLEM")
                return
            }
        #endif
        _ = await accountsRequest { try await $0.listAccounts() }
    }

    /// Adds an account for `agent` named `label`; true once the computer has it.
    @discardableResult
    public func addAccount(agent: StartAgent, label: String) async -> Bool {
        await accountsRequest { try await $0.addAccount(agent: agent, label: label) }
    }

    /// Makes `accountID` (nil: Default) the account `agent`'s next starts use; true once the computer has made it. Then
    /// its sessions are asked for once, so the switch's sentence counts them now: the computer lists them only when
    /// asked.
    @discardableResult
    public func activateAccount(agent: StartAgent, accountID: String?) async -> Bool {
        guard await accountsRequest({ try await $0.activateAccount(agent: agent, accountID: accountID) }) else { return false }
        refreshFleet()
        return true
    }

    /// Whether an account can be added for `agent` now: not while an accounts request is out, nor while one of its
    /// accounts is signing in (its row's line says so), so a second click never ends that sign-in for another account.
    public func canAddAccount(agent: StartAgent) -> Bool {
        !accountsBusy && !(accountBook?.accounts(for: agent).contains(where: \.signingIn) ?? false)
    }

    /// A click on `account`'s row: a signed-out one is signed in again, as its menu's Sign In Again… does, and never
    /// becomes the account in use, so no session moves onto an account that can't start; any other is chosen.
    @discardableResult
    public func choose(_ account: AgentAccount) async -> Bool {
        if account.signedOut { return await signInAccount(accountID: account.id) }
        guard let agent = StartAgent(rawValue: account.agent) else { return false }
        return await activateAccount(agent: agent, accountID: account.id)
    }

    /// A click on `agent`'s Default row: chosen, unless its own sign-in (Antigravity's) reads signed out, when it is
    /// signed in instead, as an added account's row is ([`choose`]); while that sign-in is going on, nothing.
    @discardableResult
    public func chooseDefault(_ agent: StartAgent) async -> Bool {
        if let signIn = accountBook?.agentSignIn(for: agent), !signIn.choosable {
            return signIn.signedOut ? await signInAgent(agent) : false
        }
        return await activateAccount(agent: agent, accountID: nil)
    }

    /// The switch `agent`'s sessions on this computer are in (their summaries' `switching`), toward the account its
    /// new starts now use; nil while none is moving.
    public func accountSwitch(for agent: StartAgent) -> AccountSwitch? {
        let moving = summaries.filter { summary in
            guard summary.switching != nil else { return false }
            switch (summary.agent, agent) {
            case (.claude, .claude), (.codex, .codex), (.antigravity, .antigravity): return true
            default: return false
            }
        }
        guard !moving.isEmpty else { return nil }
        let active = accountBook?.activeID(for: agent)
        let target = accountBook?.facts(for: agent, accountID: active)?.email
            ?? active.flatMap { id in accountBook?.accounts(for: agent).first { $0.id == id }?.label }
            ?? "Default"
        return AccountSwitch(
            target: target,
            waiting: moving.count { $0.switching == .waiting },
            moved: moving.count { $0.switching == .moved },
            // A failed move is said only while its row reads ended and relaunchable: once a retry starts it, the
            // listing may still carry the mark for an instant, and nothing has failed any more.
            failed: moving.filter { $0.switching == .failed && $0.lifecycle == .exited && $0.relaunchesOnMessage }
                .map(\.projectLabel))
    }

    /// Adds an account for `agent` and starts its sign-in in the browser at once; no name is asked of the person (an
    /// account reads as its email once signed in). The name the computer keeps is composed: the agent's own name and the
    /// next number no account of it bears, "Codex 2", within the computer's rule (1 to 64 characters, one per name).
    @discardableResult
    public func addAndSignIn(agent: StartAgent) async -> Bool {
        let taken = Set(accountBook?.accounts(for: agent).map(\.label) ?? [])
        let label = (2...).lazy.map { "\(agent.displayName) \($0)" }.first { !taken.contains($0) }!
        guard await addAccount(agent: agent, label: label),
            let added = accountBook?.accounts(for: agent).first(where: { $0.label == label })
        else { return false }
        return await signInAccount(accountID: added.id)
    }

    /// Removes `accountID` (its agent signs it out first); true once the computer has removed it.
    @discardableResult
    public func removeAccount(accountID: String) async -> Bool {
        await accountsRequest { try await $0.removeAccount(accountID: accountID) }
    }

    /// Starts `accountID`'s sign-in in a browser on the computer; true once the computer has it going (that account
    /// `signingIn` in the book). Its end arrives as a pushed listing.
    @discardableResult
    public func signInAccount(accountID: String) async -> Bool {
        await accountsRequest { try await $0.signInAccount(accountID: accountID) }
    }

    /// This computer signs an agent in and out with its own sign-in (minor 51: Antigravity's); one before it would
    /// answer those requests as undecodable, so none is offered there.
    public var signsAgentsIn: Bool {
        #if DEBUG
            if fixtureAccountBook != nil { return true }
        #endif
        return keepsAccounts && connection.profile.protocolMinor >= 51
    }

    /// This computer keeps accounts for Antigravity as it does for Claude Code and Codex (minor 57), each its own home of
    /// agy's; one before it refuses them, and offers Antigravity's one sign-in alone ([`signsAgentsIn`]).
    public var keepsAntigravityAccounts: Bool {
        #if DEBUG
            if fixtureAccountBook != nil { return true }
        #endif
        return keepsAccounts && connection.profile.protocolMinor >= 57
    }

    /// Starts `agent`'s own sign-in in a browser on the computer; true once the computer has it going.
    @discardableResult
    public func signInAgent(_ agent: StartAgent) async -> Bool {
        guard signsAgentsIn else { return false }
        return await accountsRequest { try await $0.signInAgent(agent.rawValue) }
    }

    /// Signs `agent` out of the computer; true once it has.
    @discardableResult
    public func signOutAgent(_ agent: StartAgent) async -> Bool {
        guard signsAgentsIn else { return false }
        return await accountsRequest { try await $0.signOutAgent(agent.rawValue) }
    }

    private func accountsRequest(_ ask: (DaemonConnection) async throws -> AccountBook) async -> Bool {
        guard keepsAccounts, !accountsBusy else { return false }
        accountsProblem = nil
        accountsBusy = true
        defer { accountsBusy = false }
        do {
            accountBook = try await ask(connection)
            accountsProblem = nil
            return true
        } catch {
            accountsProblem = error.localizedDescription
            return false
        }
    }

    #if DEBUG
        /// `-CC_ACCOUNTS_WIRE <frame>`: a recorded `accounts` reply (fixtures/accounts/minor-48-wire.json) this computer
        /// is taken to answer with, read from the arguments as given, so a fixture launch draws its accounts.
        private var fixtureAccountBook: AccountBook? {
            guard fixturesActive else { return nil }
            let arguments = ProcessInfo.processInfo.arguments
            guard let flag = arguments.firstIndex(of: "-CC_ACCOUNTS_WIRE"), flag + 1 < arguments.count,
                case .accounts(_, let book)? = try? JSONDecoder().decode(
                    ServerMessage.self, from: Data(arguments[flag + 1].utf8))
            else { return nil }
            return book
        }
    #endif

    /// Ask this computer for its projects. Only a computer that advertises
    /// `start_session` is asked; the answer arrives as `projects`.
    public func listProjects() {
        #if DEBUG
            if fixturesActive {
                if starter.projects == nil {
                    starter.listFixtureProjects(linux: machineSystem == "Linux", computer: machineName ?? "this computer")
                }
                return
            }
        #endif
        guard connection.capabilities?.startsSessions == true else { return }
        Task { try? await connection.send(.listProjects) }
    }

    /// The folders this computer's live sessions run in, so a start can say
    /// it would share one (minor 29 has no such fact of its own).
    public var liveFolders: Set<String> { Set(summaries.filter { $0.lifecycle == .live }.map(\.cwd)) }

    /// Ask this computer about one folder's repository: its checkouts and
    /// free branches (`get_project_git`, minor 29). Only a computer that
    /// advertises `start_worktree` is asked; the answer arrives as
    /// `project_git`.
    public func projectGit(_ path: String) {
        #if DEBUG
            if fixturesActive {
                // `-CC_GIT_PENDING YES`: a render of the start while the computer is still reading the repository.
                if UserDefaults.standard.bool(forKey: "CC_GIT_PENDING") { return }
                starter.receive(git: SessionStarter.fixtureGit(path, projects: starter.projects ?? []), for: path)
                return
            }
        #endif
        // Asked while a redial has no capabilities yet: the next handshake
        // asks again. Only a computer that says it cannot is never asked.
        guard connection.capabilities?.startsInWorktrees != false else { return }
        starter.askedGit(path)
        Task { try? await connection.send(.getProjectGit(path: path)) }
    }

    /// Ask this computer for one folder's subfolders (`list_folder`, minor 30),
    /// under a fresh request id: only the answer to the latest ask is shown.
    public func listFolder(_ request: StartFolderRequest) {
        #if DEBUG
            if fixturesActive {
                // `-CC_BROWSE_UNASKED YES`: a render of either app's browser while the computer is still listing.
                if UserDefaults.standard.bool(forKey: "CC_BROWSE_UNASKED") { return }
                starter.receive(
                    SessionStarter.fixtureListing(request, projects: starter.projects ?? [], made: madeFolders),
                    for: request)
                return
            }
        #endif
        // As with git: asked during a redial, asked again after it.
        guard connection.capabilities?.browsesFolders != false else { return }
        let requestID = UUID().uuidString.lowercased()
        starter.ask(request, as: requestID)
        Task {
            try? await connection.send(
                .listFolder(requestID: requestID, path: request.path, showHidden: request.showHidden))
        }
    }

    /// Make a folder, as `mkdir` would (`create_folder`, minor 30); the answer
    /// arrives as `create_folder_result` under the returned request id. Never
    /// sent again after a reconnect: a repeat is a second `mkdir`.
    public func createFolder(named name: String, in parent: String) -> String {
        let requestID = UUID().uuidString.lowercased()
        #if DEBUG
            if fixturesActive {
                let listed = SessionStarter.fixtureFolders(projects: starter.projects ?? [])[parent] ?? []
                let refusal: String? =
                    name.contains("/")
                    ? "\"\(name)\" is not a single folder name"
                    : ([".", "..", ""] + listed + (madeFolders[parent] ?? [])).contains(name) ? "File exists" : nil
                if refusal == nil { madeFolders[parent, default: []].append(name) }
                starter.resolveCreate(requestID, refusal: refusal)
                return requestID
            }
        #endif
        guard connection.capabilities?.browsesFolders == true else { return requestID }
        Task { try? await connection.send(.createFolder(requestID: requestID, parent: parent, name: name)) }
        return requestID
    }

    /// Start an agent in one of this computer's projects; the answer arrives as
    /// `start_session_result` under the returned request id. `trust` is the
    /// person's answer to "trust this folder?", sent with the start and with every
    /// resend of it.
    /// `files` go with the first message: every send of the start uploads them first, under new ids, for no session.
    public func startSession(
        _ project: StartProject, place: StartPlace = .folder, agent: StartAgent, model: String? = nil, prompt: String,
        trust: FolderTrust? = nil, files: [Attachment] = []
    ) -> String {
        let requestID = starter.start(
            project: project, place: place, agent: agent, model: model, prompt: prompt, trust: trust,
            machineName: machineName ?? HostDevice.current.computer("this computer"), files: files)
        #if DEBUG
            if fixturesActive {
                answerFixtureStart(requestID)
                return requestID
            }
        #endif
        sendStart(requestID)
        return requestID
    }

    /// A start stopped retrying, sent again under its own id (`SessionStarter.resume`).
    public func resumeStart(_ requestID: String) {
        starter.resume(requestID)
        #if DEBUG
            if fixturesActive {
                answerFixtureStart(requestID)
                return
            }
        #endif
        sendStart(requestID)
    }

    /// One send of one start. A send that fails, or a link that drops before
    /// the answer, is sent again by the next handshake under the same id. One
    /// send of a start at a time: a start with files spends a while sending them,
    /// and a handshake in that while leaves it to the send under way.
    private func sendStart(_ requestID: String) {
        Task {
            // The one send-eligibility check, where the send happens. A start
            // answered or retry-stopped between this being scheduled and here is
            // not sent — the first send and every reconnect resend alike.
            guard let start = starter.starts[requestID], start.outcome == nil, !start.retryStopped,
                !startsSending.contains(requestID)
            else { return }
            startsSending.insert(requestID)
            defer { startsSending.remove(requestID) }
            guard let ids = await sendStartFiles(requestID) else { return }
            try? await connection.send(
                .startSession(
                    requestID: requestID, path: start.project.path, agent: start.agent.rawValue,
                    prompt: start.prompt, worktree: start.place, model: start.model, trust: start.trust,
                    attachments: ids))
        }
    }

    /// Starts being sent now (`sendStart`).
    private var startsSending: Set<String> = []

    /// True while the sample fleet is on screen.
    ///
    /// Implies `fixturesActive`, and adds the two things only a shipping screen
    /// needs: the banner that says what this is, and an answer path that says
    /// what it would have done instead of reaching for a Mac. The launch
    /// argument is a test seam and never sets it.
    public private(set) var sampleFleetActive = false

    /// Whether Settings lists this as a computer: one paired (it has a machine id), or a fixture's computer. Never the
    /// sample fleet, which is no computer: with only it, Settings says no computer is there yet.
    public var listsAsComputer: Bool { machineID != nil || (fixturesActive && !sampleFleetActive) }

    /// The fixture's diff on its own run, or with `-CC_FIXTURE_DIFF_ALL YES` on every run, as the sample fleet's.
    private func applyFixtureDiff() {
        guard let diff = Fixtures.diff() else { return }
        let parsed = UnifiedDiff.parse(diff.unified)
        let keys = UserDefaults.standard.bool(forKey: "CC_FIXTURE_DIFF_ALL") ? Array(states.keys) : [diff.sessionID]
        for key in keys { diffs[key] = .loaded(diff, parsed: parsed, fetchedAt: Date()) }
    }

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
    public func startSampleFleet() {
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
        // a tap that answers "Not paired with a computer." inside the sample
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
    /// Also called on the way *into* every pairing (`Machines.pair`): the
    /// sample fleet is reachable while its own Settings sheet is open, so
    /// "sample and live never mix" has to be enforced where a live link begins,
    /// not only where the sample one ends.
    public func stopSampleFleet() {
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
    /// rule.** A run needs `lifecycle` `exited` or `failed` — the proof, never the
    /// derived Ended status, which can read Ended without the run being proven
    /// ended. An adopted run (`claude:`) is removable at any lifecycle, because
    /// no proof of its end can ever exist — and removing
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
    public func removeSession(_ summary: SessionSummary) async -> DeleteSessionResult? {
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

    /// Whether End run is offered on this run: a live run CodeConnect started,
    /// Claude's or Codex's, on a computer whose ccd ends runs (it ends no other).
    public func canEndRun(_ summary: SessionSummary) -> Bool {
        daemonProfile.endsRuns && summary.lifecycle == .live
    }

    /// End a run CodeConnect started (`stop_run`): what to tell the person who
    /// asked, or `nil` when the run is ending or has ended.
    ///
    /// No gate of its own: the connection refuses to send without `stop_run` in
    /// its own words, and ccd answers every other case itself, a refusal written
    /// to be shown, so a confirmation that outlived the row it was asked from
    /// still gets an answer. ccd says nothing more when it accepts the stop, so
    /// the fleet is asked again at once and the row reads Stopping.
    public func endRun(_ summary: SessionSummary) async -> String? {
        let result: StopRunResult
        do {
            result = try await connection.stopRun(session: summary.sessionUID, requestID: UUID().uuidString)
        } catch {
            return error.localizedDescription
        }
        switch result {
        case .stopping, .alreadyEnded:
            refreshFleet()
            return nil
        case .refused(let reason):
            return reason
        case .unknown:
            return FleetWords.refusedPlainly
        }
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

        /// Test seam: wait for every cache write queued so far.
        func cacheSettledForTesting() async { await cacheSettled() }

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
    public var showsFleet: Bool { pairing.isPaired || fixturesActive }

    /// Manual entry: a host and a token typed by hand.
    ///
    /// Held pending, exactly like a scanned code, rather than saved on the way in.
    /// Saving first meant a typo was persisted as *the* pairing before anything
    /// had spoken to a Mac — the app then left onboarding for a Fleet it could
    /// never load, with a credential the daemon would reject forever and no route
    /// back to the screen that could fix it. The endpoint is committed by
    /// `commitPendingPairing()` once a daemon has actually answered.
    func pair(with endpoint: DaemonEndpoint) {
        startPairing(endpoint)
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
        guard let machineID, let endpoint = pendingPairing, case .token = endpoint.credential
        else { return }
        pairing.save(endpoint, id: machineID)
        pendingPairing = nil
        onPaired?()
        // No push call here either — same handshake, same `onConnected`.
    }

    /// Hold `endpoint` as this machine's pairing in progress.
    private func startPairing(_ endpoint: DaemonEndpoint) {
        pendingPairing = endpoint
        subscribed.removeAll()
        forgetDaemonKeying()
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
        let endpoint = payload.endpoint
        startPairing(endpoint)
        connection.start(endpoint: endpoint)
    }

    /// Join a computer another daemon listed, with no code: its daemon lets this
    /// device in by its Tailscale identity, and the device token it answers with
    /// is what gets saved. Dialled as a pairing is: a few tries, then it stops.
    /// Tailscale lists every Mac and Linux node, and one that never answers may
    /// run no CodeConnect, so it is not dialled for ever; the next list naming it
    /// (`Machines.join`) asks again.
    func join(_ computer: PeerComputer) {
        startPairing(computer.endpoint)
        connection.start(endpoint: computer.endpoint, forPairing: true)
    }

    /// True while a scanned code is being exchanged for a device token.
    var isPairing: Bool { pendingPairing != nil }

    /// Give up this machine's pairing in progress, and go back to the link it
    /// had.
    func cancelPairing() {
        pendingPairing = nil
        if let endpoint {
            connect(to: endpoint)
        } else {
            connection.stop()
        }
    }

    /// This machine is gone from the phone, or its pairing never finished: its
    /// link closes, its clocks stop, and everything that came over it goes.
    func leave() {
        connection.stop()
        forgetFleet()
        pendingPairing = nil
        tickTask?.cancel()
        tickTask = nil
        fleetRefreshTask?.cancel()
        fleetRefreshTask = nil
    }

    /// Drop every trace of the fleet on screen, including what feeds the Deck.
    ///
    /// Shared by `leave` and by leaving the sample fleet, because "completely"
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
        runGits = [:]
        gitActions = [:]
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

    public func refreshFleet() {
        requestSessions()
    }

    /// The daemon's answer to a message for a run that was moving to another account ends the move's step, whether the
    /// move went through or not: the list is asked for again at once, so the run shows its account and where its move
    /// stands without waiting for the next refresh.
    private func listAgainAfterAMove(_ moving: Bool) {
        if moving { requestSessions() }
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
        factAskWaiting = false
    }

    /// Asks for the fleet because of a fact it has not seen, once for any burst of them. With no request unanswered,
    /// it asks now. Otherwise the fact waits, and one more request goes with the next answer
    /// (`askFleetForWaitingFact`): an answer to a request written before the fact was filed can predate it. So a
    /// restart's re-attach facts cost two requests at most, however many runs there are.
    private func askFleetForFact() {
        syncSessionsCount()
        if sessionsWritten > sessionsAnswered { factAskWaiting = true } else { refreshFleet() }
    }

    private func askFleetForWaitingFact() {
        guard factAskWaiting else { return }
        factAskWaiting = false
        refreshFleet()
    }

    // MARK: - Derived state

    public var linkHealth: LinkHealth {
        #if DEBUG
            if let forced = forcedLinkHealth { return forced }
        #endif
        return LinkHealth.evaluate(
            phase: connection.phase, lastContactAt: connection.lastContactAt,
            dialFailure: connection.dialFailure, refusal: connection.refusal, isRedial: connection.isRedial, now: now)
    }

    #if DEBUG
        /// `-CC_LINK_STATE unreachable|tailnet|host|blocked|version|pair|stale|connecting`: every computer's link as
        /// that state, built as the live one is (`LinkHealth.evaluate`, with the refusal's own sentence), for a render.
        private var forcedLinkHealth: LinkHealth? {
            guard let state = UserDefaults.standard.string(forKey: "CC_LINK_STATE") else { return nil }
            let failed = { (error: ConnectionError) in
                LinkHealth.evaluate(
                    phase: .failed(reason: error.localizedDescription), lastContactAt: nil,
                    refusal: ConnectionRefusal(error), now: self.now)
            }
            let waiting = { (failure: DaemonConnection.DialFailure) in
                LinkHealth.evaluate(
                    phase: .waiting(until: self.now, reason: ""), lastContactAt: self.now.addingTimeInterval(-90),
                    dialFailure: failure, isRedial: true, now: self.now)
            }
            switch state {
            case "unreachable": return waiting(.unreachable)
            case "tailnet": return waiting(.hostUnresolvable(host: "jordans-mac-studio.tail1a2b3.ts.net"))
            case "host": return waiting(.hostUnresolvable(host: "mac-studio.local"))
            case "blocked": return waiting(.blocked)
            case "version":
                return failed(.incompatible(DaemonConnection.differentVersion(named: machineName, older: true)))
            case "pair": return failed(.unauthorized)
            case "stale":
                return LinkHealth.evaluate(
                    phase: .connected(since: now.addingTimeInterval(-600)),
                    lastContactAt: now.addingTimeInterval(-75), now: now)
            case "connecting": return LinkHealth.evaluate(phase: .connecting, lastContactAt: nil, now: now)
            default: return nil
            }
        }
    #endif

    /// Why a decision cannot be answered from here, or nil when it can.
    ///
    /// The link's own sentence, except in the sample fleet — which reports no
    /// link precisely because it has none, and would therefore disable every
    /// control on the one screen somebody came here to read. Those controls
    /// answer inline instead; see `DecisionCardView.submit`.
    public var actionsBlockedReason: String? {
        sampleFleetActive ? nil : linkHealth.disabledReason
    }

    /// Which daemon build this is, and therefore which of the newer surfaces
    /// are real here.
    public var daemonProfile: DaemonProfile { connection.profile }

    /// This model's machine as the phone keeps it. Nil with nothing paired, and
    /// for a new machine until its pairing is saved.
    var machine: PairedMachine? { machineID.flatMap(pairing.machine(id:)) }

    /// Where this machine is reached.
    public var endpoint: DaemonEndpoint? {
        #if DEBUG
            // A render's forced link (`-CC_LINK_STATE`) is a paired computer's: it has the address a pairing keeps.
            if machine == nil, UserDefaults.standard.string(forKey: "CC_LINK_STATE") != nil {
                return DaemonEndpoint(
                    host: "jordans-mac-studio.tail1a2b3.ts.net", port: DaemonEndpoint.defaultPort,
                    credential: .token("render"), useTLS: false)
            }
        #endif
        return machine?.endpoint
    }

    /// Why this machine's encrypted connection cannot be used, when it cannot.
    public var tlsUnavailableReason: String? { machineID.flatMap(pairing.tlsUnavailableReason(for:)) }

    /// The computer as its owner names it: the name its daemon reports, or what
    /// it last reported while it is not connected, or — from a daemon that has
    /// never said (older than minor 25) — the host this phone paired with. Nil
    /// when nothing is paired.
    public var machineName: String? {
        daemonProfile.machineName ?? machine?.name ?? endpoint?.host
    }

    /// The computer's system as its daemon names it ("macos", "linux"), by the same rule as its name.
    public var machineOS: String? { daemonProfile.machineOS ?? machine?.os }

    /// The computer's system, by the same rule as its name.
    public var machineSystem: String? { DaemonProfile.system(forOS: machineOS) }

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
    public func shownApproval(sessionKey: String, id: String) -> ApprovalItem? {
        shownSessions[sessionKey]?.shown.approval(id: id)
    }

    /// Working directories the daemon has reported, newest session first. The
    /// only evidence the app has for the Mac's account name.
    var sessionPaths: [String] {
        shownSummaries.sorted { $0.updatedDate > $1.updatedDate }.map(\.cwd)
    }

    var fleet: [FleetRow] {
        let labels = RunLabel.labels(for: shownSummaries)
        let name = machineName
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
                    lastActivityAt: shown?.lastActivityAt,
                    cachedAt: (shown?.hasLiveData ?? false) ? nil : shown?.loadedFromCacheAt,
                    machineID: machineID, machineName: name)
            })
    }

    public var blockedCount: Int { fleet.filter { $0.status == .blocked }.count }

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
        fresh.accountName = { [weak self] id in self?.accountEmail(run: key, id: id) }
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
        if runGits.contains(where: { states[$0.key] == nil }) {
            runGits = runGits.filter { states[$0.key] != nil }
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
    public func shownSummary(for key: String) -> SessionSummary? {
        shownSummaries.first { $0.sessionKey == key }
    }

    /// What is on screen for this run, or nil when the screen has no such run.
    public func shown(for key: String) -> SessionState.Shown? {
        shownSessions[key]?.shown
    }

    /// What the reader has typed on this run and not sent, or nil when the screen has no such run. See `RunDrafts`.
    public func drafts(for key: String) -> RunDrafts? {
        shownSessions[key]?.drafts
    }

    /// Every listed run's drafts.
    var shownDrafts: [RunDrafts] { shownSessions.values.map(\.drafts) }

    /// The reader has seen this run through what is drawn.
    public func markReviewed(_ key: String) {
        shownSessions[key]?.markReviewed()
    }

    /// The reader acknowledged this run's gap notice.
    #if DEBUG
        /// `-CC_GAP <cause>` (`GapNotice.forRender`): once the fixture's events have been drawn, as a live hole is seen
        /// after events have arrived, each run of two or more drawn rows gets that notice, dated halfway between its
        /// first row and its last, so it stands mid-conversation.
        private func applyGapFixture() {
            guard let text = UserDefaults.standard.string(forKey: "CC_GAP") else { return }
            Task { @MainActor [weak self] in
                try? await Task.sleep(for: .seconds(1))
                for state in self?.states.values.map({ $0 }) ?? [] {
                    let rows = TimelineBuilder.drawn(state.timeline)
                    guard rows.count > 1, let first = rows.first?.date, let last = rows.last?.date,
                        let notice = GapNotice.forRender(text, at: first.addingTimeInterval(last.timeIntervalSince(first) / 2))
                    else { continue }
                    state.showForRender(notice)
                }
            }
        }
    #endif

    public func dismissGap(_ key: String) {
        shownSessions[key]?.dismissGap()
    }

    /// The screen showing this chat is told as the rows above its reader are
    /// replaced. See `SessionState.onWindowChange`.
    public func onWindowChange(
        of key: String,
        _ observer: ((_ shown: [TimelineItem], _ next: [TimelineItem], _ change: SessionState.WindowChange) -> Void)?
    ) {
        shownSessions[key]?.onWindowChange = observer
    }

    /// The screen showing this chat is told as a catch-up's one change lands.
    /// See `SessionState.onCatchUpLanding`.
    public func onCatchUpLanding(
        of key: String, _ observer: ((_ shown: SessionState.Shown, _ next: SessionState.Shown) -> Void)?
    ) {
        shownSessions[key]?.onCatchUpLanding = observer
    }

    /// What a compact sheet watches for: the run's newest receipts as they
    /// arrive, never as drawn — a receipt is the answer to a send, and the sheet
    /// fences it against `eventHighWater`, which is the truth too.
    public func receipts(for key: String) -> SessionState.Receipts? {
        states[key]?.receipts
    }

    /// What the diagnostics sheet reports of the sessions this phone holds, as
    /// they are now.
    public var sessionDiagnostics: SessionDiagnostics {
        SessionDiagnostics(
            known: summaries.count,
            gaps: states.values.filter { $0.gap != nil }.count,
            highestSeq: states.values.map(\.lastSeq).filter { $0 > 0 }.max(),
            fromCache: states.values.filter { $0.loadedFromCacheAt != nil && !$0.hasLiveData }.count)
    }

    public struct SessionDiagnostics {
        public let known: Int
        /// Sessions whose event log has a discontinuity the app has not seen filled.
        public let gaps: Int
        public let highestSeq: UInt64?
        public let fromCache: Int
    }

    /// The run's permission mode as drawn: what its held events last said, or
    /// the Mac's own account where that rests on a newer event — the event that
    /// set the mode can be older than the window.
    public func permissionMode(for key: String) -> String? {
        let shown = shown(for: key)
        if let fact = shownSummary(for: key)?.permissionMode, fact.seq > shown?.permissionModeSeq ?? 0 {
            return fact.value
        }
        return shown?.permissionMode
    }

    /// The run's model, likewise. A tie is one event read twice, and the Mac's reading of it stands: the Mac
    /// resolved it against the run's start's models (claude's `sonnet (claude-sonnet-5-5)` is the model id
    /// `claude-sonnet-5-5`), which an ended run no longer sends.
    public func confirmedModel(for key: String) -> ConfirmedModel? {
        let shown = shown(for: key)
        if let fact = shownSummary(for: key)?.model, fact.seq >= shown?.confirmedModelSeq ?? 0 {
            return ConfirmedModel(name: ModelConfirmation.unwrapped(fact.value))
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
    public func eventHighWater(for key: String) -> UInt64 {
        max(states[key]?.lastSeq ?? 0, summary(for: key)?.lastSeq ?? 0)
    }

    /// What to call this run on screen — see `RunLabel`.
    ///
    /// Computed across the whole fleet because the answer depends on it: a
    /// second run in the same project is what earns a qualifier, and no run can
    /// know that alone.
    public func runLabel(for key: String) -> RunLabel {
        RunLabel.labels(for: shownSummaries)[key] ?? .unknown
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
        case .terminalOpened, .terminalOutput, .terminalCredit, .terminalClosed:
            terminal.receive(message)
        case .deleteSessionResult, .putFileResult:
            // Nothing to do here. `removeSession` awaits the reply itself and does
            // the local clean-up, because only it knows which row asked.
            break
        case .testPushResult, .runGit, .conflicts, .conflict, .gitActionResult:
            // Same shape: the sheet's own request awaits the reply by id, and a run's
            // repository is awaited by the Changes panel that asked for it.
            break
        case .accounts(nil, let book):
            // Pushed when the list changes on the computer (from any device, or a sign-in's end): the latest is kept.
            accountBook = book
        case .folderTrust, .accounts, .accountsRefused:
            // The same again: `listFolderTrust`, `setFolderTrust` and
            // `forgetFolderTrust`, and the accounts requests (answered or refused), each await
            // their reply by id.
            break
        case .projects(let projects, let agents):
            starter.receive(projects: projects, agents: agents)
        case .projectGit(let path, let git):
            starter.receive(git: git, for: path)
        case .folderListing(let requestID, let listing):
            starter.receive(listing, answering: requestID)
        case .createFolderResult(let requestID, let refusal):
            starter.resolveCreate(requestID, refusal: refusal)
        case .machineStats(let stats):
            // Only while a screen is watching (see `keepMachineStatsFresh`): a late
            // answer after the view left, or one in flight when the link drops, is
            // ignored. The drop clears the sample through `onDisconnected`,
            // and the view leaving clears it through the poll's own `defer`.
            if pollingStats { machineStats = stats }
        case .startSessionResult(let requestID, let outcome):
            starter.resolve(requestID, outcome)
            // The new session reaches the fleet on its own; asking now shortens
            // the Starting… it waits behind.
            if case .started = outcome { refreshFleet() }
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
            askFleetForWaitingFact()
        case .event(let event):
            let sessionState = state(for: event.sessionKey)
            let heldThrough = sessionState.lastSeq
            // Whether this event arrives live, captured BEFORE `ingest`/`settleLaunchIfCaughtUp`:
            // the event that completes a catch-up flips `launchSettled` true, so reading it afterward
            // would read a replay's last event as live. Used by the "Not sent: stopped" hand-back.
            let wasLiveArrival = launchSettled && catchUp == nil
            // The run's setting-fact seqs before ingest, to learn whether this event moves one: the
            // sheet's model/effort/mode selection is the daemon's resolved value, carried only by the
            // list, so a fact an event changes is asked for below rather than waited for at the next poll.
            let modeSeqBefore = sessionState.permissionModeSeq
            let modelSeqBefore = sessionState.lastConfirmedModelSeq
            let effortSeqBefore = sessionState.lastConfirmedEffortSeq
            sessionState.ingest(event)
            scheduleCacheWrite(event.sessionKey)
            if !launchSettled { settleLaunchIfCaughtUp() }
            // A resolution or a new card changes the fleet's top band; a run's end
            // changes its lifecycle, which only the list carries.
            if event.kind == .approvalRequest || event.kind == .approvalResolved || event.kind == .sessionEnd {
                refreshFleet()
            }
            // A run's link attaching or dropping, the run ending, or a setting fact the sheet's selection
            // follows (its permission mode, a /model receipt's model, or an effort notice) moving on this
            // event — each changes what the fleet says of the run, and the daemon sends a list only when
            // asked: a fact newer than the list holds is asked about now, not at the next timed refresh. A
            // replayed older fact is in the list already. For a setting fact the event is the trigger, so
            // the fact already exists when the pull goes out — no race with a setting's reply. The model
            // fact is also read off session_start, which carries the current model at every turn init, not
            // a change; it is excluded so a streaming turn is not a poll storm, leaving the /model receipt
            // (a `.userMessage`). Mode and effort never advance on session_start.
            let settingFactMoved = sessionState.permissionModeSeq != modeSeqBefore
                || sessionState.lastConfirmedEffortSeq != effortSeqBefore
                || (event.kind != .sessionStart && sessionState.lastConfirmedModelSeq != modelSeqBefore)
            if event.kind == .linkState || event.isSessionExit || settingFactMoved,
                let summary = summaries.first(where: { $0.sessionKey == event.sessionKey }),
                event.seq > summary.lastSeq
            {
                askFleetForFact()
            }
            // A send a Stop cancelled ("Not sent: stopped"): drop its retry identity so the
            // text handed back to the composer re-sends fresh, not as the duplicate the
            // daemon would replay as "already applied". Only this notice drops one.
            if let stoppedRequestID = event.sendNotSentStoppedRequestID {
                dropSendIdentity(requestID: stoppedRequestID)
                // Offer the cancelled text to the composer on a LIVE arrival (`wasLiveArrival`, captured
                // before the settle). The real stopped send answered `.sent` before the Stop, so its
                // identity is already cleared by then — liveness, not a held identity, is what separates
                // a live stop from a replay. The view takes the text only when its composer is empty.
                if wasLiveArrival,
                    let text = sessionState.events
                        .first(where: { $0.sendRequestID == stoppedRequestID })?.userText
                {
                    composerRestore[event.sessionKey] = text
                }
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
            if let machineID {
                pairing.noteMachine(
                    name: ack.machineName, os: ack.machineOS, daemonID: ack.machineID,
                    id: machineID)
                // A certificate the pairing's address can never validate is a
                // fact worth stating, not a silent downgrade.
                pairing.noteTLSUnusable(
                    ack.capabilities.tls && endpoint?.hostIsIPLiteral == true, id: machineID)
            }
            startCacheMigration()
            // A start whose answer the link lost is asked again, unchanged: the
            // daemon answers a repeated request id with its first outcome.
            if ack.capabilities.startsSessions {
                for start in starter.unanswered { sendStart(start.requestID) }
            }
            // A listing or a repository the link lost is asked for again.
            if ack.capabilities.browsesFolders {
                for request in starter.unansweredListings { listFolder(request) }
            }
            if ack.capabilities.startsInWorktrees {
                for path in starter.gitAsked { projectGit(path) }
            }
            // Last: it may find this model is another's machine, and leave it.
            onIdentified?()
        case .interruptResult, .stopRunResult, .composeResult, .settingResult:
            // Awaited by the control that asked, correlated on the request id it
            // minted. Nothing here may act on one: a stop, a message or a setting
            // is a mutation somebody pressed, and its outcome belongs to that press.
            break
        case .error(let code, _) where code == "sessions_unavailable":
            // The Mac's answer to a `sessions` request it could not serve: it
            // takes that request's place, so the replies after it stay matched.
            if takeSessionsReply() { catchUp?.progressed = true }
        case .peers(let directory):
            tailnet = directory
            onPeers?(directory)
        case .answerResult, .sendTextResult, .commandCatalog, .diff, .error,
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

    /// Turn a link's session reference into a key this machine can look up.
    ///
    /// A `codeconnect://session/…` URL may carry either: a uid (which survives
    /// a name being reused — a notification never sends one) or a tmux
    /// name (what a person types, and what every link written before uids
    /// existed says). A name is resolved to the run whose link is attached, else
    /// the newest, because that is the run somebody following the link means.
    ///
    /// During a catch-up a name waits — nil — for the list the catch-up ends
    /// on: until then the name may still belong to a run that has gone.
    public func resolveSessionKey(reference: String) -> String? {
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
        runGits[key] = nil
        gitActions[key] = nil
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
    public func loadEarlier(key: String) {
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
    public func reachedTop(key: String) {
        guard states[key]?.windowStart != nil, daemonProfile.pagesHistory else { return }
        loadEarlier(key: key)
    }

    /// The open chat needs the page below without the reader asking: it does not
    /// fill its screen, or a page added nothing and the reader is still at its
    /// top. Like the fill, it waits until the window has caught up with what the
    /// Mac listed, and stops at a page that failed.
    public func pageBackOnItsOwn(key: String) {
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
    public func isReadyToPage(_ key: String) -> Bool {
        guard connection.phase.isConnected, !isCatchingUp, daemonProfile.pagesHistory,
            let state = states[key], let windowStart = state.windowStart, windowStart > 1,
            state.pinRequest == nil, let summary = summary(for: key)
        else { return false }
        return state.drawnThrough >= summary.lastSeq
    }

    public func isLoadingEarlier(_ key: String) -> Bool {
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
    public func isAnswering(_ item: ApprovalItem) -> Bool { answersInFlight.contains(item.id) }

    /// The last thing the daemon said about this card, kept so re-opening it
    /// shows the confirmed outcome instead of an innocent-looking pending state.
    public func lastAttempt(for item: ApprovalItem) -> AnswerAttempt? { answerAttempts[item.id] }

    // MARK: - Codex: stop the turn, say something

    /// One controller per session, created on demand. Held here rather than in a
    /// view so an in-flight stop survives the sheet being dismissed — and so the
    /// bounded grey after a link-state refusal is not reset by scrolling the
    /// fleet.
    public func codexControls(for key: String) -> CodexControls {
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

    /// Why a Codex control cannot act while nothing is known of this daemon —
    /// its link is down and took its capabilities with it — or nil when the
    /// daemon is known. Unknown is not old: the control says what is known, the
    /// link, rather than that the daemon predates it.
    private func unknownDaemonReason(_ summary: SessionSummary) -> String? {
        guard summary.agent == .codex, !daemonProfile.isConnected else { return nil }
        return actionsBlockedReason ?? "Not connected to \(HostDevice.current.computer("your computer"))."
    }

    /// Why Stop is not offered on this session as drawn, or nil when it is.
    public func stopUnavailable(for key: String) -> String? {
        guard let summary = shownSummary(for: key) else { return "This run is not in the fleet." }
        if let reason = unknownDaemonReason(summary) { return reason }
        return CodexProse.stopUnavailable(
            agent: summary.agent,
            daemonHonoursStop: daemonProfile.honoursStop(for: summary.agent),
            isLive: summary.lifecycle == .live,
            link: summary.codexLink,
            runLink: summary.link,
            runningTurn: shown(for: key)?.runningTurn)
    }

    /// The same rule against the truth: what decides whether a stop may leave.
    private func stopBlocked(for key: String) -> String? {
        guard let summary = summary(for: key) else { return "This run is not in the fleet." }
        if let reason = unknownDaemonReason(summary) { return reason }
        return CodexProse.stopUnavailable(
            agent: summary.agent,
            daemonHonoursStop: daemonProfile.honoursStop(for: summary.agent),
            isLive: summary.lifecycle == .live,
            link: summary.codexLink,
            runLink: summary.link,
            runningTurn: runningTurn(for: key))
    }

    /// Why the Codex composer cannot send as drawn, or nil when it can.
    public func composeUnavailable(for key: String) -> String? {
        guard let summary = shownSummary(for: key) else { return "This run is not in the fleet." }
        if let reason = unknownDaemonReason(summary) { return reason }
        return Self.composeUnavailable(summary, profile: daemonProfile, cardShown: cardShown(for: key))
    }

    /// What decides whether a message may leave — the truth summary (`summary(for:)`) for the pending
    /// ref, and the shown card for `cardShown`, which the submit path's `rebuildNow` makes current first.
    public func composeBlocked(for key: String) -> String? {
        guard let summary = summary(for: key) else { return "This run is not in the fleet." }
        if let reason = unknownDaemonReason(summary) { return reason }
        return Self.composeUnavailable(summary, profile: daemonProfile, cardShown: cardShown(for: key))
    }

    /// A card the reader can see on this run — built into the shown `pendingApprovals` from an
    /// ingested approval_request, which leads `summary.pending` by a fleet round trip.
    private func cardShown(for key: String) -> Bool {
        !(shown(for: key)?.pendingApprovals.isEmpty ?? true)
    }

    private static func composeUnavailable(
        _ summary: SessionSummary, profile: DaemonProfile, cardShown: Bool
    ) -> String? {
        // While a card waits on a Codex run, a message must not be sent: it would
        // steer the turn the card's answer then aborts, and the words would be
        // lost with no row (R2T3-F1). The draft is kept; the reader answers the
        // card first. A card names itself on either of two clocks: `summary.pending`,
        // which the fleet refresh fills, and the shown `pendingApprovals`, which an
        // ingested approval_request builds a fleet round trip earlier. Blocking on
        // EITHER closes both orders — the summary-first one and the live-event one
        // app-qa first saw, where the card is on screen before the summary catches
        // up. Codex-only — Claude's typed text is not a steer, and its sends keep
        // their words.
        if summary.isCodex, !summary.pending.isEmpty || cardShown {
            return "Answer the card first. Your draft will stay here."
        }
        // A Codex run still starting has no link yet, so the link-based reason would read "no live
        // link" — a fault for what is ordinary start-up. Say it is starting, greying the composer as
        // Claude's starting run does, until the run is live.
        if summary.isCodex, summary.lifecycle == .starting {
            return "Codex is still starting."
        }
        return CodexProse.composeUnavailable(
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
    public func stopCodexTurn(sessionKey key: String) async -> InterruptResult? {
        let controls = codexControls(for: key)
        guard let summary = summary(for: key) else {
            controls.stopNotSent("This run is not in the fleet.")
            return nil
        }
        // Agent, daemon capability, running turn and **link state**, in the one
        // place that decides whether a frame may leave.
        if let blocked = stopBlocked(for: key) {
            controls.stopNotSent(blocked, turn: runningTurn(for: key))
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
            controls.stopNotSent(Self.noUidSentence, turn: turn)
            return nil
        }
        let hash = CodexHash.interrupt(sessionRef: reference, turnID: turn)

        // **Spent material is never re-sent.** A stop whose outcome nobody
        // knows — `indeterminate`, or a reply this phone never heard — was
        // issued, and the daemon's own sentence says it will not be sent again.
        // Neither will this.
        guard !controls.stopIsSpent(material: hash) else {
            controls.stopNotSent(Self.alreadyIssuedSentence, turn: turn)
            return nil
        }
        let requestID = controls.stopRequestID(material: hash)

        controls.beginStop(turn: turn)
        do {
            let result = try await connection.interrupt(
                session: reference, requestID: requestID, turnID: turn, payloadHash: hash,
                requires: summary.agent)
            controls.settleStop(result, material: hash, turnID: turn, now: now)
            states[key]?.codexTurnsChanged()
            refreshFleet()
            return result
        } catch let refusal as DaemonConnection.CodexRefusal {
            controls.stopNotSent(Self.sentence(for: refusal, verb: "stop this turn"), turn: turn)
            return nil
        } catch ConnectionError.sentButUnanswered {
            // The frame left. Saying "nothing was sent" here would be the app
            // inventing a guarantee the transport never gave it.
            controls.stopSentNoAnswer(Self.sentNoAnswerSentence, material: hash)
            return nil
        } catch {
            controls.stopNotSent(error.localizedDescription, turn: turn)
            return nil
        }
    }

    /// A Codex run's stop as every screen shows it: the last outcome while it
    /// is about the turn running now (`CodexControls.stop(forRunningTurn:)`),
    /// read against the same running turn Stop is offered for.
    public func codexStop(for key: String) -> CodexControls.Activity<InterruptResult> {
        codexControls(for: key).stop(forRunningTurn: shown(for: key)?.runningTurn)
    }

    /// **Say something to this Codex session.**
    @discardableResult
    /// `files`: the run's tray goes with it, every file once all have arrived (the reply box's send); no other caller
    /// carries files.
    public func composeToCodex(sessionKey key: String, text: String, files withFiles: Bool = false) async -> ComposeResult? {
        let files = withFiles ? arrivedFileIDs(key) : []
        let controls = codexControls(for: key)
        guard let summary = summary(for: key) else {
            controls.composeNotSent("This run is not in the fleet.")
            return nil
        }
        // A card may have arrived as an event and not yet been built into the shown
        // state (ingest coalesces rebuilds), so rebuild before the send gate — the
        // same re-check the approval-answer path does — so `composeBlocked`'s
        // `cardShown` sees the card the reader already sees even when `summary.pending`
        // has not caught up. One re-check on submit, no third guard.
        states[key]?.rebuildNow()
        if let blocked = composeBlocked(for: key) {
            controls.composeNotSent(blocked)
            return nil
        }
        // Refused here as well as inside the connection: the reader deserves the
        // byte count in the app's own words rather than after a round trip, and
        // the connection's copy of the check is the one that guarantees no frame
        // leaves regardless of which caller forgot.
        if let blocked = ComposeDraft(text: text, files: files.count).blockedReason {
            controls.composeNotSent(blocked)
            return nil
        }
        guard let reference = Self.sessionReference(summary) else {
            controls.composeNotSent(Self.noUidSentence)
            return nil
        }
        let hash = CodexHash.compose(sessionRef: reference, text: text, attachments: files)

        // **The same words are never said twice** once their outcome is
        // unknown. Editing them makes a different message, which is a different
        // hash, and goes.
        guard !controls.composeIsSpent(material: hash) else {
            controls.composeNotSent(Self.alreadyIssuedSentence)
            return nil
        }
        let requestID = controls.composeRequestID(material: hash)

        retireSettingNoticeForSend(key)
        controls.beginCompose()
        let moving = summary.switching != nil
        do {
            let result = try await connection.compose(
                session: reference, requestID: requestID, text: text, payloadHash: hash, attachments: files)
            controls.settleCompose(result, material: hash)
            listAgainAfterAMove(moving)
            switch result {
            case .started, .steered, .duplicate:
                filesSent(files, from: key)
            default: break
            }
            states[key]?.codexTurnsChanged()
            return result
        } catch let refusal as DaemonConnection.CodexRefusal {
            listAgainAfterAMove(moving)
            controls.composeNotSent(Self.sentence(for: refusal, verb: "carry this message"))
            return nil
        } catch ConnectionError.sentButUnanswered {
            controls.composeSentNoAnswer(
                Self.sentNoAnswerSentence, material: hash)
            return nil
        } catch {
            controls.composeNotSent(error.localizedDescription)
            return nil
        }
    }

    /// **A message to a Codex run from a `send(text:to:)` caller that owns its own outcome** — the diff
    /// comment, a denial reason — reached here instead of the Claude `send_text` path the daemon refuses by
    /// name. Unlike `composeToCodex`, which drives the composer's one note, this returns a `ComposeAttempt`
    /// for the caller's own banner and keeps the diff comment's own retry: the request id is the compose
    /// hash of the words, so a re-send of the same words is recognised by the daemon and replayed, never
    /// typed twice. A refusal comes back in the app's own words, never ccd's raw sentence.
    private func composeComment(text: String, to key: String) async -> ComposeAttempt {
        guard let summary = summary(for: key) else { return .failed("This run is not in the fleet.") }
        states[key]?.rebuildNow()
        if let blocked = composeBlocked(for: key) { return .refused(blocked) }
        if let blocked = ComposeDraft(text: text).blockedReason { return .refused(blocked) }
        guard let reference = Self.sessionReference(summary) else { return .refused(Self.noUidSentence) }
        let hash = CodexHash.compose(sessionRef: reference, text: text)
        retireSettingNoticeForSend(key)
        let moving = summary.switching != nil
        do {
            let result = try await connection.compose(
                session: reference, requestID: "say-" + hash, text: text, payloadHash: hash)
            listAgainAfterAMove(moving)
            states[key]?.codexTurnsChanged()
            return Self.commentAttempt(for: result, matched: runLabel(for: key).project)
        } catch let refusal as DaemonConnection.CodexRefusal {
            listAgainAfterAMove(moving)
            return .refused(Self.sentence(for: refusal, verb: "carry this comment"))
        } catch ConnectionError.sentButUnanswered {
            return .indeterminate(Self.sentNoAnswerSentence)
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    /// A Codex compose outcome in the diff comment's banner vocabulary. A duplicate is as final as sent —
    /// the same words were recognised and replayed, not carried twice.
    private static func commentAttempt(for result: ComposeResult, matched: String) -> ComposeAttempt {
        switch result {
        case .started, .steered: return .sent(matched: matched)
        case .duplicate(let turnID, _): return .alreadyApplied(appliedAt: turnID)
        case .rejected(let reason): return .refused(reason)
        case .indeterminate(let reason): return .indeterminate(reason)
        case .unknown(let status): return .indeterminate(status)
        }
    }

    // MARK: - Hosted Claude settings (model, effort, mode)

    /// **What one run's model / effort / mode controls draw**, computed in the one place both apps read.
    /// The selection follows the summary's facts; the only client state folded in is whether a call is
    /// in flight and the last answer — and a retained answer is dropped once that setting's own fact has
    /// moved past the one the call started from: the notice bridged the wait, and the wait is over. An
    /// answer that applies from the next turn is the exception: its fact can move at the change itself
    /// (Codex reports the thread's new settings at once; ccd files claude's taken effort before it
    /// answers), so it stands until this device sends the run's next message
    /// (`retireSettingNoticeForSend`). A message sent from another device leaves it standing until this
    /// device's own next send or call: an accepted miss.
    public func runSettings(for key: String) -> RunSettings {
        guard let summary = summary(for: key) else { return .deferToOtherPath }
        let state = settingStateByKey[key] ?? SettingState()
        var outcome = state.outcome
        if let setting = state.outcomeSetting, factSeq(summary, setting) > state.outcomeBaselineSeq {
            outcome = nil
        }
        return RunSettings.of(
            summary: summary, confirmed: confirmedModel(for: key)?.name, inFlight: state.inFlight, lastOutcome: outcome)
    }

    /// A message this device sends to the run ends its settings notice: a change that applies from the
    /// next message now has, and the send's own outcome takes the line.
    private func retireSettingNoticeForSend(_ key: String) {
        settingStateByKey[key]?.outcome = nil
    }

    /// Set this run's model. The label the outcome names is the menu's own.
    @discardableResult
    public func setModel(for key: String, modelValue: String) async -> SettingResult? {
        await applySetting(
            key: key, setting: .model, label: menuLabel(key: key, setting: .model, value: modelValue)
        ) { try await self.connection.setModel(session: $0, requestID: $1, model: modelValue, effort: nil) }
    }

    /// Set this run's effort — it rides `set_model` with no model.
    @discardableResult
    public func setEffort(for key: String, effortValue: String) async -> SettingResult? {
        await applySetting(
            key: key, setting: .effort, label: menuLabel(key: key, setting: .effort, value: effortValue)
        ) { try await self.connection.setModel(session: $0, requestID: $1, model: nil, effort: effortValue) }
    }

    /// Set this run's permission mode. The label the outcome names is the menu's own.
    @discardableResult
    public func setMode(for key: String, modeValue: String, confirmed: Bool = false) async -> SettingResult? {
        await applySetting(
            key: key, setting: .mode, label: menuLabel(key: key, setting: .mode, value: modeValue)
        ) { try await self.connection.setMode(session: $0, requestID: $1, mode: modeValue, confirmed: confirmed) }
    }

    /// **One call in flight per run.** A second tap while one is in flight is dropped — the menus are
    /// disabled for exactly that reason, and queuing a change whose state nobody can still see is worse
    /// than losing it. Beginning a call clears the last notice; the answer records the next. The send
    /// path itself is where the frame is gated and the daemon validates the value.
    @discardableResult
    private func applySetting(
        key: String, setting: SettingKind, label: String,
        send: (String, String) async throws -> SettingResult
    ) async -> SettingResult? {
        var state = settingStateByKey[key] ?? SettingState()
        guard !state.inFlight else { return nil }
        guard let summary = summary(for: key), let reference = Self.sessionReference(summary) else {
            return nil
        }
        let baseline = factSeq(summary, setting)
        state.inFlight = true
        state.outcome = nil
        settingStateByKey[key] = state
        func record(_ outcome: SettingOutcome?, fromNextTurn: Bool = false) {
            settingStateByKey[key] = SettingState(
                inFlight: false, outcome: outcome, outcomeSetting: fromNextTurn ? nil : setting,
                outcomeBaselineSeq: baseline)
        }
        do {
            let result = try await send(reference, UUID().uuidString)
            let fromNextTurn: Bool = switch result {
            case .applied(.now): false
            case .applied: true
            case .refused, .indeterminate, .unknown: false
            }
            record(SettingOutcome.of(setting: setting, label: label, result: result), fromNextTurn: fromNextTurn)
            return result
        } catch ConnectionError.sentButUnanswered {
            // The frame left and no answer came — the same promise as the wire's `indeterminate`.
            record(SettingOutcome(text: Self.sentNoAnswerSentence, tone: .warning))
            return nil
        } catch {
            record(
                SettingOutcome(
                    text: "The change could not reach \(HostDevice.current.computer("the computer")).",
                    tone: .warning))
            return nil
        }
    }

    /// The display name for a setting's value: the menu's own label for a model or mode, and
    /// `EffortConfirmation`'s for an effort. The raw value when the menu does not list it.
    private func menuLabel(key: String, setting: SettingKind, value: String) -> String {
        let menus = summary(for: key)?.menus
        switch setting {
        case .model:
            return menus?.models.first { $0.value == value }
                .map { RunSettings.modelName(label: $0.label, detail: $0.detail) } ?? value
        case .mode: return menus?.modes.first { $0.value == value }?.label ?? value
        case .effort: return EffortConfirmation.label(for: value)
        }
    }

    private func factSeq(_ summary: SessionSummary, _ setting: SettingKind) -> UInt64 {
        switch setting {
        case .model: return summary.model?.seq ?? 0
        case .effort: return summary.effort?.seq ?? 0
        case .mode: return summary.permissionMode?.seq ?? 0
        }
    }

    /// One run's model / effort / mode call state. The selection is never kept here — it follows the
    /// summary's facts, read through `RunSettings`.
    private struct SettingState: Sendable {
        var inFlight = false
        var outcome: SettingOutcome?
        /// The setting whose fact ends the notice; `nil` for an answer that applies from the next turn,
        /// which no fact ends (`runSettings`).
        var outcomeSetting: SettingKind?
        /// The seq of `outcomeSetting`'s fact when the call began; the notice drops once the run's fact
        /// passes it.
        var outcomeBaselineSeq: UInt64 = 0
    }

    /// The one string a mutation may name a session by: its uid. `nil` when
    /// there is none, which is a refusal and not a fallback.
    private static func sessionReference(_ summary: SessionSummary) -> String? {
        summary.sessionUID.isEmpty ? nil : summary.sessionUID
    }

    private static let noUidSentence =
        "\(HostDevice.current.computer("The computer")) has not given this run an id of its own, so nothing was sent."

    /// A stop or a message whose outcome nobody knows, refused again. "Sent" for both, as the computer's own sentence
    /// for the same case says: a stop that was "stopped" would claim the turn ended, which nobody knows.
    private static let alreadyIssuedSentence =
        "This was already sent and what became of it is not known; it will not be sent again. "
            + "Check \(HostDevice.current.computer("the computer"))."

    static let sentNoAnswerSentence =
        "This left the \(HostDevice.current.noun) and \(HostDevice.current.computer("the computer")) never answered, so what became of it is not known; "
            + "it will not be sent again. Check \(HostDevice.current.computer("the computer"))."

    /// A typed refusal in the app's own words. **Never dressed as the daemon's**
    /// — nothing was sent, so the daemon has said nothing about it.
    private static func sentence(
        for refusal: DaemonConnection.CodexRefusal, verb: String
    ) -> String {
        switch refusal {
        case .notAdvertised:
            return
                "\(HostDevice.current.computer("The computer"))’s CodeConnect never said it could \(verb), so nothing was sent. Update it."
        case .notACodexSession(let agent):
            return "This is a \(agent) run, so nothing was sent."
        case .nothingToSend(let reason):
            return reason
        }
    }

    /// Answers are idempotent by `(session, request_id)` on the daemon side, so
    /// a retry after any failure here is always safe.
    public func answer(item: ApprovalItem, decision: AnswerDecision) async -> AnswerAttempt {
        // **The decision must match the agent, checked on the send path.**
        //
        // The two vocabularies are mutually exclusive and the Mac refuses each
        // one aimed at the other by name, but for a Claude card's
        // `acceptForSession` where the card offers it (`allow_for_session`,
        // minor 36). The view already picks the right
        // surface, but a view is not a guarantee: this closes the path itself,
        // so a deep link, a stale sheet or a future caller cannot transmit a
        // decision the daemon will only send back.
        // Unknown agent fails closed. `?? .claude` was the default here, and
        // `.claude` is the one vocabulary that transmits — so a card whose run
        // had left the fleet sent an `allow` for an agent nobody could name.
        guard let agent = summary(for: item.sessionKey)?.agent else {
            return .rejected(
                "This run is no longer on the fleet, so nothing was sent. Answer it at \(HostDevice.current.computer("the computer", named: machineName)).")
        }
        if let mismatch = Self.decisionMismatch(
            decision: decision, agent: agent,
            resolvesCodexCards: daemonProfile.resolvesCodexCards,
            isQuestion: QuestionCard.isQuestionToolName(item.card.toolName),
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
    /// Read after the reader's tap, never from what was
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
            return .failed("This card was resolved on \(HostDevice.current.computer("the computer", named: machineName)) before your answer left, so nothing was sent.")
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
                return "Answers are for Claude’s questions; this card is not one, "
                    + "so nothing was sent."
            }
            guard answersQuestions else {
                return "\(HostDevice.current.computer("The computer"))’s CodeConnect is too old to answer Claude’s questions from the "
                    + "\(HostDevice.current.noun), so nothing was sent. Answer at \(HostDevice.current.computer("the computer"))."
            }
            return nil
        case (.claude, _) where isQuestion:
            return "Claude is asking a question, which is answered by choosing its answers, "
                + "so nothing was sent."
        case (.claude, _) where !answersApprovalsByHook:
            return "\(HostDevice.current.computer("The computer"))’s CodeConnect is too old to answer Claude’s approvals from the "
                + "\(HostDevice.current.noun), so nothing was sent. Update it, or answer at \(HostDevice.current.computer("the computer"))."
        case (.codex, _) where !resolvesCodexCards:
            // **Nothing known of the daemon.** A card is retired by the `request_id`
            // on its resolution, which every connected daemon sends. A card that
            // cannot be retired must not be answered from here, by any vocabulary.
            return "\(HostDevice.current.computer("The computer"))’s CodeConnect is too old to answer a Codex card from the \(HostDevice.current.noun), "
                + "so nothing was sent. Update it, or answer at \(HostDevice.current.computer("the computer"))."
        case (.codex, .answers), (.codex, .decline):
            // A Codex question card is answered with the question family, exactly as
            // a Claude one is; an approval card is not, and says so. `resolvesCodexCards`
            // is already guaranteed — the `!resolvesCodexCards` case above caught its absence.
            guard isQuestion else {
                return "A Codex card is answered by naming one of the options it offered, "
                    + "so nothing was sent."
            }
            return nil
        case (.codex, .allow), (.codex, .deny), (.codex, .option), (.codex, .text):
            return "A Codex card is answered by naming one of the options it offered, "
                + "so nothing was sent."
        case (.claude, .optionId("acceptForSession")):
            // The one option id a Claude card accepts: "Allow for this session",
            // shown only when the card offered it (`allowForSession`) and refused
            // by the computer on a card that did not. Every other option id is a
            // Codex vocabulary the Mac refuses by name.
            return nil
        case (.claude, .optionId):
            return "An option id is for a Codex run; this is a Claude run, "
                + "so nothing was sent."
        case (.antigravity, .allow), (.antigravity, .deny):
            // Answered through the agent host that holds agy's request, as a Claude card is.
            guard answersApprovalsByHook else {
                return "\(HostDevice.current.computer("The computer"))’s CodeConnect is too old to answer Antigravity’s approvals from the "
                    + "\(HostDevice.current.noun), so nothing was sent. Update it, or answer at \(HostDevice.current.computer("the computer"))."
            }
            return nil
        case (.antigravity, _):
            return "An Antigravity card is answered Allow or Deny, so nothing was sent."
        case (.unsupported(let raw), _):
            return "This app does not know how to answer a \(raw) run, so nothing was sent."
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
        } catch ConnectionError.sentButUnanswered {
            // The answer left the phone and nothing came back. It may have
            // landed, so re-answering could collide with it — the controls stay
            // closed and the card waits for the computer's own outcome.
            return .sentUnconfirmed(ConnectionError.sentButUnanswered.localizedDescription)
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    /// `key` is a run, not a name. It reaches the daemon as the `session_uid`
    /// wherever there is one, because a name resolves to whichever run holds it
    /// *now* — and sending to the wrong agent is not a mistake that can be
    /// taken back.
    /// `files`: the run's tray goes with it (the reply box's send); no other caller carries files.
    public func send(text: String, to key: String, submit: Bool = true, files: Bool = false) async -> ComposeAttempt {
        // A Codex run has no Claude slash vocabulary and refuses `send_text` by name; a message to it — a
        // diff comment, or any other caller here — goes through the Codex compose path instead, its outcome
        // in the caller's own banner and any refusal in the app's own words, never ccd's raw sentence.
        if summary(for: key)?.isCodex == true {
            return await composeComment(text: text, to: key)
        }
        // The policy stands between EVERY caller and the wire — the composer,
        // a denial reason, a diff comment — so a command this app answers
        // itself is never typed through a side door.
        switch ClaudeCommandPolicy.action(for: text) {
        case .nativeModel:
            return .refused("Use the run’s settings to change the model.")
        case .nativeDiff:
            return .refused("Open Changes to see the diff.")
        case .nativeEffort:
            return .refused("Use the run’s settings to change the effort.")
        case .nativeCompact:
            return .refused("Type /compact in the reply box to compact the context.")
        case .nativeClear:
            return .refused("/clear has its own confirmation in the app.")
        case .blocked(_, let reason):
            return .refused(reason)
        case .passThrough:
            break
        }
        return await sendUnchecked(text: text, to: key, submit: submit, files: files ? arrivedFileIDs(key) : [])
    }

    /// The native adapters' own injections — the ONLY paths that may carry
    /// a slash command past the policy, because each sheet *is* the
    /// policy's answer for its command.
    public func sendCompactCommand(instructions: String, to key: String) async -> ComposeAttempt {
        let trimmed = instructions.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = trimmed.isEmpty ? "/compact" : "/compact \(trimmed)"
        return await sendUnchecked(text: text, to: key, submit: true)
    }

    public func sendClearCommand(to key: String) async -> ComposeAttempt {
        await sendUnchecked(text: "/clear", to: key, submit: true)
    }

    private func sendUnchecked(text: String, to key: String, submit: Bool, files: [String] = []) async -> ComposeAttempt {
        // Every typed-text route funnels through here — the compose bar, the
        // Compact sheet, `/clear` — so this is where the sample
        // fleet is answered once, in its own vocabulary, rather than per
        // surface. A surface the sample forgot to gate otherwise falls
        // through to the link's sentence ("Not connected to your computer"),
        // which is a dead end in a fleet whose banner says nothing is
        // connected.
        if sampleFleetActive {
            return .failed("These agents are not real. Pair with \(HostDevice.current.computer("your computer")) to talk to your own.")
        }
        let trimmed = Self.normalizedSendText(text)
        guard !trimmed.isEmpty || !files.isEmpty else { return .failed("Nothing to send.") }
        guard connection.capabilities?.sendText != false else {
            return .failed("\(HostDevice.current.computer("The computer"))’s CodeConnect can’t take typed text.")
        }
        // A committed send retires the last Stop's note, so the shared outcome slot shows what
        // became of THIS send rather than a stale "stopped" from the turn just ended. Codex does the
        // same inside `beginCompose`; this is the Claude send path's own retirement.
        codexControls(for: key).retireStopForSend()
        retireSettingNoticeForSend(key)
        // Every send carries an identity, so a retry is recognisable instead
        // of being typed twice. The id is fresh per mutation — except after
        // an indeterminate outcome, where re-sending the *same mutation*
        // (same session, text, and submit flag) reuses the original id on
        // purpose: that is the retry the ledger exists to recognise, and the
        // daemon replays the settled outcome instead of typing again.
        let mutation = SendMutationKey(key: key, text: trimmed, submit: submit, files: files)
        let requestID = pendingSendIdentities[mutation] ?? UUID().uuidString.lowercased()
        let payloadHash = SendTextIdentity.payloadHash(
            session: key, text: trimmed, submit: submit, attachments: files)
        let moving = summary(for: key)?.switching != nil
        do {
            let result = try await connection.sendText(
                session: key, text: trimmed, submit: submit,
                requestID: requestID, payloadHash: payloadHash, attachments: files)
            listAgainAfterAMove(moving)
            switch result {
            case .sent(let matched):
                pendingSendIdentities[mutation] = nil
                filesSent(files, from: key)
                return .sent(matched: matched)
            case .refused(let reason):
                // A refusal promises nothing was typed, so the next attempt
                // is a fresh mutation, not a retry of this one.
                pendingSendIdentities[mutation] = nil
                return .refused(reason)
            case .duplicate(_, let appliedAt):
                pendingSendIdentities[mutation] = nil
                filesSent(files, from: key)
                return .alreadyApplied(appliedAt: appliedAt)
            case .indeterminate(let reason):
                retainIdentityIfListed(mutation, requestID)
                return .indeterminate(reason)
            }
        } catch ConnectionError.sentButUnanswered {
            // It left the phone and no answer came back: nobody knows whether
            // it was typed, which is what indeterminate says. The identity is
            // kept, so the next attempt is the recognisable retry.
            retainIdentityIfListed(mutation, requestID)
            return .indeterminate(ConnectionError.sentButUnanswered.localizedDescription)
        } catch {
            // The transport died before an answer. The mutation may have been
            // claimed, so the identity is kept for the same reason as above.
            retainIdentityIfListed(mutation, requestID)
            return .failed(error.localizedDescription)
        }
    }

    /// How send text becomes a mutation identity: leading and trailing whitespace is
    /// not part of the message. The send's mutation key, `holdsSendIdentity`,
    /// `dropSendIdentity`, and the composer's abandon-on-edit guard all compare through
    /// this one function, so a whitespace-only edit never drops or keeps the wrong one.
    static func normalizedSendText(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Whether this exact text to `key` still has the identity of an attempt whose
    /// outcome is unknown, so sending it again is a retry and not a second typing.
    public func holdsSendIdentity(text: String, to key: String) -> Bool {
        let trimmed = Self.normalizedSendText(text)
        return pendingSendIdentities[SendMutationKey(key: key, text: trimmed, submit: true, files: arrivedFileIDs(key))] != nil
    }

    /// Drops one unanswered send's identity (its exact session, trimmed text and
    /// submit flag), so the same words sent again are a new send and not the retry
    /// the daemon would replay as "already applied". The composer decides when,
    /// through `abandonHeldSend(whenDraftChangesFrom:to:in:)`, which drops the words the
    /// draft just left.
    func dropSendIdentity(text: String, to key: String) {
        let trimmed = Self.normalizedSendText(text)
        pendingSendIdentities[SendMutationKey(key: key, text: trimmed, submit: true, files: arrivedFileIDs(key))] = nil
    }

    /// Drops the retry identity of the send a Stop cancelled, found by its `request_id`
    /// (the value the mint recorded), so the text handed back to the composer re-sends as
    /// a new send and not the retry the daemon would answer "already applied". Only a
    /// `send_not_sent` notice drops one; an ordinary unanswered send keeps its identity
    /// (`retainIdentityIfListed`), so an unconfirmed send still retries under its first id.
    func dropSendIdentity(requestID: String) {
        pendingSendIdentities = pendingSendIdentities.filter { $0.value != requestID }
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
        /// The upload ids it carried: the same words with other files are another send.
        var files: [String] = []
    }
    private var pendingSendIdentities: [SendMutationKey: String] = [:]

    /// The text of a send a **live** Stop cancelled, by session key, for the composer to take back. Set
    /// only when a `send_not_sent {stopped}` arrives on a LIVE event (`wasLiveArrival`): the real stopped
    /// send answered `.sent` before the Stop, so its identity is already gone by notice time — liveness,
    /// not a held identity, is the signal. Consumed by the view, which fills its composer only when it is
    /// empty, so a draft in progress is never overwritten.
    public private(set) var composerRestore: [String: String] = [:]

    /// The view took the handed-back text, or declined it because its composer was busy; either way
    /// clear it, so a later notice is its own one-shot event and the same text is never re-offered.
    public func consumeComposerRestore(for key: String) {
        composerRestore[key] = nil
    }

    // MARK: - Machine stats

    /// This computer's latest CPU/memory/disk sample, or nil when it is unknown:
    /// not asked yet, this computer's CodeConnect does not report stats, the link
    /// is down, or no screen is showing them. The Kit exposes the value or nil;
    /// what nil draws is the view's business.
    public private(set) var machineStats: MachineStats?

    private var pollingStats = false
    static let machineStatsInterval: Duration = .seconds(5)

    /// Keep `machineStats` fresh while the calling view's task lives. The run list's
    /// computer menu is the one caller; a second concurrent call returns at once — one poll per model,
    /// never a second stream. While the link is up and the computer reports stats it
    /// asks every `machineStatsInterval`; it abstains otherwise, so an old daemon is
    /// never asked and a view that appears before the handshake still polls once it
    /// lands. The view's task cancellation ends the loop, and the sample clears with it.
    public func keepMachineStatsFresh() async {
        guard !pollingStats else { return }
        pollingStats = true
        defer { pollingStats = false; machineStats = nil }
        while !Task.isCancelled {
            if connection.phase.isConnected, connection.capabilities?.reportsMachineStats == true {
                try? await connection.send(.getMachineStats)
            }
            try? await Task.sleep(for: Self.machineStatsInterval)
        }
    }

    // MARK: - Diff

    public func diffState(for key: String) -> DiffState { diffs[key] ?? .idle }

    /// The run's repository as its computer last read it for its Changes panel (minor 56): nil before a read, from a
    /// computer that does not read repositories, and in the sample fleet and fixtures, which have none.
    public func runGit(for key: String) -> RunGitReply? { runGits[key] }

    /// The computer commits a run's changes from its Changes panel (minor 59).
    public var commitsChanges: Bool { connection.capabilities?.commitsChanges == true }

    /// The computer opens a pull request from a run's branch (minor 61).
    public var opensPullRequests: Bool { connection.capabilities?.opensPullRequests == true }

    /// Where the run's action from its Changes panel stands.
    public func gitActionState(for key: String) -> GitActionState { gitActions[key] ?? .idle }

    /// Commits every change in the run's folder with `message`. Each is its own request: once a commit has landed,
    /// another finds nothing to commit.
    public func commit(key: String, message: String) {
        perform(key: key, step: .commit) { [connection] in
            try await connection.commit(sessionUID: key, message: message, requestID: UUID().uuidString)
        }
    }

    /// Opens a pull request from the run's branch, pushed first, into the repository its `run_git` names. Each press is
    /// its own request: once one is open, the computer answers with it.
    public func createPullRequest(key: String) {
        perform(key: key, step: .pullRequest) { [connection] in
            try await connection.createPullRequest(sessionUID: key, requestID: UUID().uuidString)
        }
    }

    /// One action of the run's Changes panel, unless one is on its way. Done, the panel's diff and repository are read
    /// again, and they show what the action made; refused, the computer's reason is kept, in the app's words, until the
    /// step it was for has passed.
    private func perform(key: String, step: GitStep, _ request: @escaping () async throws -> GitActionResult) {
        guard gitActions[key] != .working else { return }
        gitActions[key] = .working
        Task { [weak self] in
            guard let self else { return }
            let after: GitActionState
            do {
                switch try await request() {
                case .done: after = .idle
                case .pullRequest(let number, let url): after = .opened(number: number, url: url)
                case .refused(let code, let detail):
                    after = .refused(ChangesWords.actionRefused(code: code, detail: detail, step: step), step: step)
                case .unknown: after = .refused(ChangesWords.actionUnknown, step: step)
                }
            } catch {
                after = .refused(ChangesWords.actionUnknown, step: step)
            }
            self.gitActions[key] = after
            // A read already on its way waits behind the action on the computer, so it shows the action's result.
            if self.diffs[key]?.isLoading != true { self.loadDiff(key: key, force: true) }
            self.loadRunGit(key: key)
        }
    }

    /// Reads the run's repository again, unless a read is on its way. A failed read keeps the last one: the panel's
    /// diff says what became of the link, in its own words, so this read adds no second account of it.
    public func loadRunGit(key: String) {
        guard !sampleFleetActive, !fixturesActive, connection.capabilities?.readsRunGit == true,
            connection.phase.isConnected, !runGitsReading.contains(key)
        else { return }
        runGitsReading.insert(key)
        Task { [weak self] in
            guard let self else { return }
            defer { self.runGitsReading.remove(key) }
            guard let reply = try? await self.connection.runGit(session: key) else { return }
            self.runGits[key] = reply
            // A clean repository ends what a refused commit said: the next changes are another commit.
            // What the panel holds has passed once the repository shows it: a whole clean repository ends a commit's
            // refusal; a pull request ends a pull request's refusal, and the one just opened.
            if let git = reply.git {
                let passed =
                    switch self.gitActions[key] {
                    case .refused(_, .commit): git.changed.files == 0 && git.changed.complete
                    case .refused(_, .pullRequest), .opened: git.pullRequest != nil
                    default: false
                    }
                if passed { self.gitActions[key] = nil }
            }
        }
    }

    /// Ask the daemon for this run's working-tree diff.
    ///
    /// On demand only: a diff is a thing you ask for when you are about to read
    /// it, or again while a panel that shows it follows its run. The result is stamped with the
    /// daemon's own `captured_at` so the view can say how old what you are
    /// reading is, rather than implying it is live.
    public func loadDiff(key: String, force: Bool = false) {
        // The sample fleet's diff is preloaded by `startSampleFleet` and is
        // the only diff its sessions will ever have: a refresh has no daemon
        // to ask, and falling through would overwrite the loaded diff with a
        // failure written in the link's vocabulary.
        if sampleFleetActive { return }
        // A fixture's preloaded diff likewise has no daemon to read it again.
        if fixturesActive, case .loaded = diffs[key] { return }
        if !force, diffs[key]?.isLoading == true { return }
        guard connection.phase.isConnected else {
            // The app's own sentence about its own link. Tagged `.app` so the
            // screen renders its link state instead of quoting this to the Mac.
            diffs[key] = .failed(.app(linkHealth.disabledReason ?? "Not connected to \(HostDevice.current.computer("your computer"))."))
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
                    : " \(HostDevice.current.computer("The computer"))’s CodeConnect never said it can show diffs, which would explain it."
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

    public func clearDiff(key: String) { diffs[key] = nil }

    /// The daemon reports refusals in prose; these are the two that mean
    /// something specific to the user rather than "try again".
    static func classify(rejection reason: String) -> AnswerAttempt {
        let lowered = reason.lowercased()
        if lowered.contains("already-resolved") {
            return .answeredAtKeyboard(reason)
        }
        if lowered.contains("stale payload_hash") || lowered.contains("out of date") {
            return .staleCard(reason)
        }
        return .rejected(reason)
    }
}

// MARK: - Small helpers

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
    public var refusal: String? {
        switch self {
        case .deleted, .notFound: nil
        case .stillRunning: "Still running"
        // The Mac's own word, because it has none better: it never established
        // what happened to this run, and saying "still running" here would
        // invent the one fact it is missing.
        case .notExited(let lifecycle): "Computer says “\(lifecycle)”"
        case .failed(let message): message
        case .unknown(let status): "Computer said “\(status)”"
        case .none: "No answer"
        }
    }
}
