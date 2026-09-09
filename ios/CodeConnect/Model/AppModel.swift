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
    /// checkmark is the same false-confirmation this phase forbids, so an
    /// indeterminate duplicate is reported as `.indeterminate`, never `.duplicate`.
    static func classify(duplicate outcome: AnswerOutcome, staleHash: Bool) -> AnswerAttempt {
        outcome.indeterminate
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

    private(set) var summaries: [SessionSummary] = []
    /// Per-run state, keyed by `SessionSummary.sessionKey` — the `session_uid`
    /// where the daemon mints them, the tmux name on an older one. Never by the
    /// display name on a uid-capable daemon: `cc-1` is handed to the next
    /// session when this one exits, and keying by it is what spliced two agents'
    /// timelines into one.
    private(set) var states: [String: SessionState] = [:]
    /// One `CodexControls` per session, created on demand by `codexControls(for:)`.
    ///
    /// **`@ObservationIgnored`, and that is load-bearing.** Views read this
    /// during `body` — the fleet row asks whether Stop is greyed, the session
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
    /// Age of the fleet list when it came off disk rather than the wire.
    private(set) var fleetCachedAt: Date?
    /// When *this launch* put that cached list on screen. The cached banner's
    /// grace clock — distinct from `fleetCachedAt`, which is how old the data
    /// is, not how long the launch has had to replace it.
    private(set) var fleetCacheRestoredAt: Date?
    private(set) var hasLiveFleet = false
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
    /// Keyed by `ApprovalItem.id` — the run *and* the request. A `request_id` is
    /// only unique within one run, so keying by it alone would have one card's
    /// spinner and one card's confirmed outcome appear on another run's card.
    private var answersInFlight: Set<String> = []
    private var answerAttempts: [String: AnswerAttempt] = [:]
    private var loadingEarlier: Set<String> = []
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
    func openFromNotification(kind: String?) {
        pendingDeepLink = kind == "approval" ? .deck(requestID: nil) : .fleet
    }

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
        guard !fixturesActive else { return }
        startFleetRefresh()
        Task { await loadCachedFleet() }
        // A pairing exchange is already dialling; starting a second connection
        // would spend the single-use code twice.
        if !pairedByCode, let endpoint = pairing.endpoint {
            connect(to: endpoint)
            // Push registration deliberately does NOT happen here: before the
            // handshake the daemon's capabilities are unknown, and asking for
            // notification permission on behalf of a daemon that may not
            // advertise push is a system prompt nothing can honour.
            // `onConnected` registers once the `hello_ack` says it can ring.
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
            for message in Fixtures.frames(variant: variant) {
                connection.injectForTesting(message)
            }
            if let diff = Fixtures.diff() {
                diffs[diff.sessionID] = .loaded(
                    diff, parsed: UnifiedDiff.parse(diff.unified), fetchedAt: Date())
            }
            applyCachedFixture()
            applyPushStateFixture()
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
            hasLiveFleet = false
            fleetCachedAt = Date().addingTimeInterval(-age)
            // The seam stages "the banner is up", so the grace is staged as
            // already passed — the render harness is photographing the earned
            // state, not the launch that leads to it.
            fleetCacheRestoredAt = .distantPast
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
        sampleFleetActive = true
        fixturesActive = true
        for message in Fixtures.sampleFrames() {
            connection.ingest(message)
        }
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
        func adoptSummariesForTesting(_ list: [SessionSummary]) { summaries = list }
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
        hasLiveFleet = false
        fleetCachedAt = nil
        fleetCacheRestoredAt = nil
    }

    func scenePhaseChanged(to phase: ScenePhase) {
        switch phase {
        case .active:
            connection.retryNow()
            refreshFleet()
            startTicking()
            startFleetRefresh()
            foregroundPushCheck()
        case .background, .inactive:
            flushCache()
            tickTask?.cancel()
            tickTask = nil
            fleetRefreshTask?.cancel()
            fleetRefreshTask = nil
        @unknown default:
            break
        }
    }

    private func startTicking() {
        guard tickTask == nil else { return }
        tickTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                guard let self, !Task.isCancelled else { return }
                self.now = Date()
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
        guard connection.phase.isConnected else { return }
        connection.sendIgnoringFailure(.sessions)
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
        DeckOrdering.sort(states.values.flatMap(\.pendingApprovals), profile: daemonProfile)
    }

    var deckCount: Int { deck.count }

    /// The authoritative, LIVE approval for a card identity — the single resolver
    /// every decision surface (the open sheet, the Deck cell, the card view)
    /// derives from before offering an action or claiming an outcome.
    ///
    /// **`nil` means nothing live backs this card** — the session departed the
    /// fleet (`forgetLocalState`), its log was reset/rewound, or the card left
    /// the timeline. A `nil` here must render as a NON-actionable "no longer
    /// available" state, never a frozen actionable snapshot: acting on, or
    /// claiming an outcome for, a card no daemon would accept an answer for is
    /// exactly the class this resolver exists to close. Routed by `sessionKey`
    /// (the id's own prefix) so it is one timeline scan, not a fleet-wide one.
    func liveApproval(sessionKey: String, id: String) -> ApprovalItem? {
        states[sessionKey]?.approval(id: id)
    }

    /// Working directories the daemon has reported, newest session first. The
    /// only evidence the app has for the Mac's account name.
    var sessionPaths: [String] {
        summaries.sorted { $0.updatedDate > $1.updatedDate }.map(\.cwd)
    }

    var fleet: [FleetRow] {
        let labels = RunLabel.labels(for: summaries)
        return FleetOrdering.sort(
            summaries.map { summary in
                let state = states[summary.sessionKey]
                let pending = state?.pendingApprovals ?? []
                return FleetRow(
                    summary: summary,
                    status: FleetStatusRule.status(
                        summary: summary, state: state,
                        reviewedSeq: ReviewMarks.reviewedSeq(for: summary.sessionKey)),
                    subtitle: subtitle(for: summary, state: state),
                    activity: activity(for: state),
                    label: labels[summary.sessionKey] ?? .unknown,
                    capability: FleetStatusRule.capability(
                        summary: summary, capabilities: connection.capabilities),
                    blockedCount: max(summary.blockedOn.count, pending.count),
                    lastEventAt: state?.lastEventAt,
                    cachedAt: (state?.hasLiveData ?? false) ? nil : state?.loadedFromCacheAt)
            })
    }

    var blockedCount: Int { fleet.filter { $0.status == .blocked }.count }

    /// Creates on demand. Only ever called from event handling — a view that
    /// called it during `body` would be mutating state mid-render, so views read
    /// `states[key]`, which is always populated for a session in `summaries`.
    private func state(for key: String) -> SessionState {
        if let existing = states[key] { return existing }
        let fresh = SessionState(sessionKey: key, recordsReviewMarks: !fixturesActive)
        states[key] = fresh
        return fresh
    }

    func summary(for key: String) -> SessionSummary? {
        summaries.first { $0.sessionKey == key }
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
        RunLabel.labels(for: summaries)[key] ?? .unknown
    }

    /// The tmux session name to attach to, or nil when the daemon no longer
    /// lists this run.
    ///
    /// Nil rather than a guess, and taken from the fleet rather than from the
    /// route: `tmux attach -t =<uid>` cannot work — tmux has never heard of a
    /// uid — and the name is only safe to use while the daemon still vouches
    /// that it belongs to this run.
    func tmuxName(for key: String) -> String? {
        guard let summary = summary(for: key) else { return nil }
        // Empty is the daemon saying "no known location": an adopted run whose
        // hooks arrived but which nothing ever put in tmux. Falling back to the
        // session id here was a guess wearing a fact's clothes — the exact
        // thing the comment above forbids — and attaching to a name nothing
        // vouches for could hand the user a different agent's keyboard.
        guard !summary.tmuxSession.isEmpty else { return nil }
        return summary.tmuxSession
    }

    private func subtitle(for summary: SessionSummary, state: SessionState?) -> String {
        // **Nothing, rather than the run's own name back.** A run that has said
        // nothing has no sentence, and repeating the title underneath itself
        // reads as a fact and is not one.
        guard let last = state?.timeline.last else { return "" }
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
                tool: approval.card.toolName, input: approval.card.toolInput)
            return "\(approval.card.toolName) \(argument ?? "")".trimmingCharacters(
                in: .whitespaces)
        case .notice(let notice): return notice.title
        }
    }

    /// The same last item as `subtitle(for:state:)`, kept as **two strings** so
    /// the row can set the tool in prose and its argument in monospace.
    ///
    /// The flat sentence above is still built, and is still what VoiceOver
    /// hears: a screen reader has no faces to distinguish, and "Bash, echo soak"
    /// is the same fact spoken. What the split buys is that `echo soak` — a
    /// shell command — stops being drawn in proportional type on Running and
    /// Idle rows while an identical construction two rows above it is monospace.
    private func activity(for state: SessionState?) -> FleetActivity? {
        guard let last = state?.timeline.last else { return nil }
        switch last.content {
        case .tool(let tool):
            return FleetActivity(tool: tool.name, argument: tool.argument?.firstLine)
        case .approval(let approval):
            return FleetActivity(
                tool: approval.card.toolName,
                argument: ToolSummary.principalArgument(
                    tool: approval.card.toolName, input: approval.card.toolInput))
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
            // Guarded for the same reason the assignment above is: `filter`
            // always returns a new dictionary, and assigning one that dropped
            // nothing is an invalidation carrying no news.
            if diffs.contains(where: { !live.contains($0.key) }) {
                diffs = diffs.filter { live.contains($0.key) }
            }
            for summary in list {
                let sessionState = state(for: summary.sessionKey)
                if sessionState.lastSeq >= summary.lastSeq {
                    sessionState.noteConfirmedCurrent()
                }
                subscribeIfNeeded(summary)
            }
        case .event(let event):
            state(for: event.sessionKey).ingest(event)
            scheduleCacheWrite(event.sessionKey)
            // A resolution or a new card changes the fleet's top band.
            if event.kind == .approvalRequest || event.kind == .approvalResolved {
                refreshFleet()
            }
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
    func resolveSessionKey(reference: String) -> String? {
        if summary(for: reference) != nil { return reference }
        let named = summaries.filter { $0.sessionID == reference }
        guard !named.isEmpty else { return nil }
        let attached = named.filter { $0.link == .attached }
        return (attached.isEmpty ? named : attached)
            .max { $0.createdDate < $1.createdDate }?
            .sessionKey
    }

    private func resubscribeAll() {
        subscribed.removeAll()
        refreshFleet()
    }

    /// Resume where the cache left off, and be explicit whenever that is not
    /// possible — a gap the client cannot see makes the log worthless.
    ///
    /// The daemon is asked for the run by `sessionKey`, which is the uid where
    /// there is one. Subscribing by name would follow *whichever* run holds that
    /// name — the daemon says so in `ws.rs` — so a dead `cc-1` on screen would
    /// quietly start showing a live `cc-1`'s work.
    private func subscribeIfNeeded(_ summary: SessionSummary) {
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
            let known = max(sessionState.lastSeq, cached?.lastSeq ?? 0)
            var afterSeq: UInt64
            if known > summary.lastSeq {
                // The daemon's log is shorter than ours: it was reset behind us.
                sessionState.resetForRewoundLog()
                await self.cache.clearEvents(key: key)
                afterSeq = Self.backfillStart(lastSeq: summary.lastSeq, state: sessionState)
            } else if known > 0 {
                afterSeq = known
            } else {
                afterSeq = Self.backfillStart(lastSeq: summary.lastSeq, state: sessionState)
            }

            guard self.connection.phase.isConnected,
                self.connection.generation == generation
            else {
                self.subscribed.remove(key)
                return
            }
            self.connection.sendIgnoringFailure(.subscribe(session: key, afterSeq: afterSeq))
        }
    }

    private static func backfillStart(lastSeq: UInt64, state: SessionState) -> UInt64 {
        guard lastSeq > initialBackfill else { return 0 }
        state.noteTruncatedHead()
        return lastSeq - initialBackfill
    }

    /// Pull the whole log for one session, for when the truncated head matters.
    /// Guarded: a full replay is expensive and repeat taps only duplicate it.
    func loadEarlier(key: String) {
        guard !loadingEarlier.contains(key), connection.phase.isConnected else { return }
        loadingEarlier.insert(key)
        connection.sendIgnoringFailure(.subscribe(session: key, afterSeq: 0))
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(10))
            self?.loadingEarlier.remove(key)
        }
    }

    func isLoadingEarlier(_ key: String) -> Bool { loadingEarlier.contains(key) }

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
        states.removeValue(forKey: key)
        diffs.removeValue(forKey: key)
        pendingCacheWrites.remove(key)
        // A departed run's unsettled mutations die with it. Left behind, a
        // reused session *name* would collide a brand-new send with a dead
        // mutation's identity and the daemon would refuse it as a conflict.
        pendingSendIdentities = pendingSendIdentities.filter { $0.key.key != key }
        // Gated exactly like the cache line below it: the sample fleet's
        // `.sessions` frame lists only sample runs, so an ungated forget here
        // would take one tap on "look around" as permission to destroy every
        // real run's review marks — the cached fleet is loaded even unpaired.
        if !fixturesActive { ReviewMarks.forget(sessionKey: key) }
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
        hasLiveFleet = true
        fleetCachedAt = nil
        fleetCacheRestoredAt = nil
    }

    /// Cold open: show the last-known fleet immediately, stamped with its age,
    /// and let the live list replace it when it arrives.
    private func loadCachedFleet() async {
        // A fixture run is hermetic: it *is* the cache, and reading the real one
        // over it is how half a previous live session ends up in a render.
        guard !fixturesActive else { return }
        guard let cached = await cache.loadFleet(), !hasLiveFleet else { return }
        summaries = cached.sessions
        fleetCachedAt = cached.cachedAt
        fleetCacheRestoredAt = Date()
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

    /// The turn this session is running, or nil when the phone holds none.
    ///
    /// Derived from the event envelopes, because there is no "a turn began"
    /// fact on this wire at all — see `CodexTurnTracker`. Decision D2 prefers
    /// the approval event's own envelope `turn_id` when the Mac supplies one and
    /// falls back to this; the tracker already reads whichever is present,
    /// because both arrive as `Event.turnID`.
    func runningTurn(for key: String) -> String? {
        let controls = codexControls(for: key)
        return CodexTurnTracker.runningTurn(
            in: states[key]?.events ?? [],
            observed: controls.observedTurns,
            retired: controls.retiredTurns)
    }

    /// Why Stop is not offered on this session, or nil when it is.
    func stopUnavailable(for key: String) -> String? {
        guard let summary = summary(for: key) else { return "This run is not in the fleet." }
        return CodexProse.stopUnavailable(
            agent: summary.agent,
            daemonHonoursStop: daemonProfile.stopsCodexTurns,
            link: summary.codexLink,
            runningTurn: runningTurn(for: key))
    }

    /// Why the Codex composer cannot send, or nil when it can.
    func composeUnavailable(for key: String) -> String? {
        guard let summary = summary(for: key) else { return "This run is not in the fleet." }
        return CodexProse.composeUnavailable(
            agent: summary.agent,
            daemonUnderstandsCompose: daemonProfile.composesToCodex,
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
    /// question for the controls, off the same inputs. Two readers of one rule
    /// is how the link state came to be enforced when drawing a button and not
    /// when sending a frame.
    @discardableResult
    func stopCodexTurn(sessionKey key: String) async -> InterruptResult? {
        let controls = codexControls(for: key)
        guard let summary = summary(for: key) else {
            controls.stopNotSent("This run is not in the fleet.")
            return nil
        }
        // Agent, daemon capability, running turn and **link state**, in the one
        // place that decides whether a frame may leave.
        if let blocked = stopUnavailable(for: key) {
            controls.stopNotSent(blocked)
            return nil
        }
        guard let turn = runningTurn(for: key) else {
            controls.stopNotSent("Nothing is running to stop.")
            return nil
        }
        // **The uid or nothing** (decision D4). The tmux name is handed to the
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
        if let blocked = composeUnavailable(for: key) {
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
    /// there is none, which is a refusal and not a fallback (D4).
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
            resolvesCodexCards: daemonProfile.resolvesCodexCards)
        {
            return .rejected(mismatch)
        }
        guard !answersInFlight.contains(item.id) else {
            return .failed("An answer for this card is already being sent.")
        }
        answersInFlight.insert(item.id)
        defer { answersInFlight.remove(item.id) }
        let attempt = await sendAnswer(item: item, decision: decision)
        answerAttempts[item.id] = attempt
        return attempt
    }

    /// Why this decision cannot be sent to this agent, or nil when it can.
    ///
    /// Deliberately silent about `.unrecognised`: it is only ever *received*.
    /// `.text` is **not** silent any more — it is Claude's deny-with-a-reason
    /// path, which types free text into a composer a Codex session does not
    /// have, and "the control is not drawn" is a view fact, not a guarantee.
    static func decisionMismatch(
        decision: AnswerDecision, agent: AgentKind, resolvesCodexCards: Bool = true
    ) -> String? {
        switch (agent, decision) {
        case (.codex, _) where !resolvesCodexCards:
            // **F4.** Below minor 19 the resolution carries no `request_id`, so
            // an answered card can never be retired. A card that cannot be
            // retired must not be answered from here, by any vocabulary.
            return "This Mac's CodeConnect is too old to answer a Codex card from the phone, "
                + "so nothing was sent. Update it, or answer at the Mac."
        case (.codex, .allow), (.codex, .deny), (.codex, .option), (.codex, .text):
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

    /// Deny, then type the reason. Two steps because `Deny` is Escape — it
    /// dismisses the prompt and returns Claude to its composer, which is the
    /// only place free text can land.
    func denyWithReason(item: ApprovalItem, reason: String) async -> (
        AnswerAttempt, ComposeAttempt?
    ) {
        let denial = await answer(item: item, decision: .deny)
        let trimmed = reason.trimmingCharacters(in: .whitespacesAndNewlines)
        guard case .applied = denial, !trimmed.isEmpty else { return (denial, nil) }

        // The composer needs a beat to come back after Escape; a refusal here
        // means "not ready yet", so it is worth a few short retries.
        var last: ComposeAttempt = .failed("The composer never came back after the denial.")
        for attempt in 0..<4 {
            if attempt > 0 { try? await Task.sleep(for: .milliseconds(500)) }
            last = await send(text: trimmed, to: item.sessionKey, submit: true)
            if case .refused = last { continue }
            return (denial, last)
        }
        return (denial, last)
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
    private static func classify(rejection reason: String) -> AnswerAttempt {
        let lowered = reason.lowercased()
        if lowered.contains("not on screen") {
            return .answeredAtKeyboard(reason)
        }
        if lowered.contains("already-resolved") || lowered.contains("already being applied") {
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
    var firstLine: String {
        let line = split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false).first ?? ""
        return String(line).trimmingCharacters(in: .whitespaces)
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
