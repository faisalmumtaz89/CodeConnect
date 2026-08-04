import Foundation
import Observation
import SwiftUI

/// What happened to an answer, in terms the UI can be honest about.
enum AnswerAttempt: Sendable {
    case applied(AnswerOutcome)
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
        case .applied, .duplicate, .answeredAtKeyboard: return true
        case .staleCard, .rejected, .failed: return false
        }
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

/// Application state: one daemon, N sessions, one event stream.
@MainActor
@Observable
final class AppModel {
    let pairing: PairingStore
    let connection: DaemonConnection
    let settings: AppSettings
    private let cache: EventCache

    private(set) var summaries: [SessionSummary] = []
    /// Per-run state, keyed by `SessionSummary.sessionKey` — the `session_uid`
    /// where the daemon mints them, the tmux name on an older one. Never by the
    /// display name on a uid-capable daemon: `cc-1` is handed to the next
    /// session when this one exits, and keying by it is what spliced two agents'
    /// timelines into one.
    private(set) var states: [String: SessionState] = [:]
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
    /// Where a deep link (today: a URL; once push notifications land: a push)
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
        settings: AppSettings = AppSettings()
    ) {
        self.pairing = pairing
        self.connection = DaemonConnection()
        self.settings = settings
        self.cache = cache

        connection.onMessage = { [weak self] message in self?.handle(message) }
        connection.onConnected = { [weak self] in
            self?.resubscribeAll()
            // After every handshake, not only after a pairing: a phone that
            // paired before push existed must start registering the first
            // time an upgraded daemon advertises it — and the capability is
            // only knowable here, once `hello_ack` has landed. Idempotent:
            // re-registration replaces, the daemon keys on the device.
            self?.enablePush()
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
        latestPushToken = (token, environment)
        deliverPushToken()
    }

    /// Whether the *current* connection can accept a push registration: the
    /// daemon advertises push, and this session has a device row to store
    /// the token against. One definition, consulted by both the ask and the
    /// delivery — Apple's callback arrives whenever Apple pleases, including
    /// after a switch to a Mac this predicate says no to.
    private var pushEligible: Bool {
        connection.capabilities?.push == true && connection.helloAck?.deviceID != nil
    }

    /// Send the cached token to the current connection's device row. Safe to
    /// repeat: the daemon upserts by device, so re-delivery replaces rather
    /// than accumulates — which is exactly what a re-pair or a retried flap
    /// needs. Gated here, at the single choke point, not only at the
    /// callers: the APNs callback path used to deliver unconditionally, and
    /// a token that began its journey against an eligible Mac would land on
    /// whatever ineligible daemon was connected by the time Apple answered.
    private func deliverPushToken() {
        guard let latest = latestPushToken, pushEligible else { return }
        // The send happens a hop later, and the connection can change inside
        // that hop — a re-pair completing, a handshake replacing the socket.
        // The eligibility that mattered at the guard is re-established at the
        // moment of sending, bound to this connection's generation so a
        // *newer* connection is never handed a delivery that was judged
        // against an older one. The observation seam sits at this recheck —
        // the real decision point — not at the guard above.
        let generation = connection.generation
        Task {
            guard connection.generation == generation, pushEligible else { return }
            #if DEBUG
                onPushDeliveryAttempted?(latest.token)
            #endif
            do {
                try await connection.send(
                    .registerPush(token: latest.token, environment: latest.environment))
            } catch {
                pushFailure = error.localizedDescription
            }
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

    /// Start the connection, offering the SSH public key.
    ///
    /// The key is only *offered*; a daemon acts on it only when its operator ran
    /// `codeconnect pair --ssh`. It is not minted here — `existingIdentity` deliberately
    /// does not create one — so a user who never opens the terminal never
    /// generates a key they did not ask for.
    private func connect(to endpoint: DaemonEndpoint) {
        connection.start(
            endpoint: endpoint, sshPublicKey: SSHIdentityStore.existingIdentity()?.openSSHPublicKey)
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
        /// app exactly where a push would put it.
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

        /// Test seam: `-CC_FIXTURE deck` replays contract-shaped daemon frames
        /// through the real decoders and the real ingest path, so the Deck can
        /// be driven with several agents blocked at three risk classes at once —
        /// a state that cannot be arranged on demand against a live Mac.
        ///
        /// Debug builds only, and it deliberately does *not* pair: a fixture run
        /// never has a socket, so nothing it shows can be confused with a live
        /// link that has gone quiet.
        private func applyFixtures() {
            // `-CC_FIXTURE stacked` is the same fleet with one agent holding two
            // decisions — the state where "count the agents" and "count the
            // cards" stop agreeing.
            guard let variant = Fixtures.Variant(UserDefaults.standard.string(forKey: "CC_FIXTURE"))
            else { return }
            fixturesActive = true
            connection.fixtureAnswers = true
            connection.simulateConnectedForTesting()
            for message in Fixtures.frames(variant: variant) {
                connection.injectForTesting(message)
            }
            if let diff = Fixtures.diff() {
                diffs[diff.sessionID] = .loaded(
                    diff, parsed: UnifiedDiff.parse(diff.unified), fetchedAt: Date())
            }
            applyCachedFixture()
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
    #endif

    /// Latched by `-CC_FIXTURE_CACHED`. Always false in a release build, where
    /// the seam does not exist.
    #if DEBUG
        private var fixtureCachedFleet = false
    #endif

    /// True while the app is showing replayed fixture frames instead of a
    /// daemon. Always false in a release build.
    private(set) var fixturesActive = false

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
        pendingPairing = endpoint
        subscribed.removeAll()
        forgetDaemonKeying()
        connect(to: endpoint)
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
    /// The SSH public key is minted here, because scanning a QR is the moment
    /// the user has decided to trust this Mac — and `codeconnect pair --ssh` on the other
    /// end can only file a key that was offered.
    func pair(withQR payload: PairingQRPayload) {
        let endpoint = payload.endpoint
        pendingPairing = endpoint
        subscribed.removeAll()
        forgetDaemonKeying()
        connection.start(
            endpoint: endpoint, sshPublicKey: SSHIdentityStore.identity()?.openSSHPublicKey)
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
        connection.stop()
        pairing.clear()
        subscribed.removeAll()
        summaries = []
        states = [:]
        answerAttempts = [:]
        answersInFlight = []
        diffs = [:]
        pendingPairing = nil
        hasLiveFleet = false
        fleetCachedAt = nil
        fleetCacheRestoredAt = nil
        forgetDaemonKeying()
        Task { await cache.clearAll() }
    }

    func scenePhaseChanged(to phase: ScenePhase) {
        switch phase {
        case .active:
            connection.retryNow()
            refreshFleet()
            startTicking()
            startFleetRefresh()
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

    /// Working directories the daemon has reported, newest session first. The
    /// only evidence the app has for the Mac's account name.
    var sessionPaths: [String] {
        summaries.sorted { $0.updatedDate > $1.updatedDate }.map(\.cwd)
    }

    var fleet: [FleetRow] {
        let identities = Self.identityLabels(for: summaries)
        return FleetOrdering.sort(
            summaries.map { summary in
                let state = states[summary.sessionKey]
                let pending = state?.pendingApprovals ?? []
                return FleetRow(
                    summary: summary,
                    status: FleetStatusRule.status(
                        summary: summary, state: state,
                        reviewedSeq: ReviewMarks.reviewedSeq(for: summary.sessionKey)),
                    title: title(for: summary, state: state),
                    subtitle: subtitle(for: summary, state: state),
                    activity: activity(for: state),
                    identity: identities[summary.sessionKey] ?? summary.displayName,
                    capability: FleetStatusRule.capability(
                        summary: summary, capabilities: connection.capabilities),
                    blockedCount: max(summary.blockedOn.count, pending.count),
                    lastEventAt: state?.lastEventAt,
                    cachedAt: (state?.hasLiveData ?? false) ? nil : state?.loadedFromCacheAt)
            })
    }

    /// What to print on each row's id line, keyed by run.
    ///
    /// `cc-1` normally. Two runs really can be called `cc-1` at once — one
    /// exited, one live — and a list with two identical-looking rows is its own
    /// kind of lie, so those get `cc-1 · <tail>`: the shortest tail of the uid
    /// that actually tells them apart. Shortest *and verified*, rather than a
    /// fixed slice, because a fixed slice can tie — and a discriminator that
    /// does not discriminate is worse than none, since it looks like it does.
    static func identityLabels(for summaries: [SessionSummary]) -> [String: String] {
        var labels: [String: String] = [:]
        for (name, group) in Dictionary(grouping: summaries, by: \.sessionID) {
            guard group.count > 1 else {
                for summary in group { labels[summary.sessionKey] = name }
                continue
            }
            let uids = group.map(\.sessionUID)
            // A daemon that mints no uids has nothing to tell them apart with,
            // and inventing something would be worse than admitting it.
            guard uids.allSatisfy({ !$0.isEmpty }) else {
                for summary in group { labels[summary.sessionKey] = name }
                continue
            }
            let length = Self.shortestDistinguishingSuffix(uids)
            for summary in group {
                labels[summary.sessionKey] = "\(name) · \(summary.sessionUID.suffix(length))"
            }
        }
        return labels
    }

    /// The shortest suffix length at which every one of `uids` differs. Six is
    /// the floor because a shorter one reads as noise rather than as an
    /// identifier; the full length is the ceiling, and it always works, because
    /// the uids themselves are distinct.
    static func shortestDistinguishingSuffix(_ uids: [String], floor: Int = 6) -> Int {
        let longest = uids.map(\.count).max() ?? floor
        var length = min(floor, longest)
        while length < longest {
            if Set(uids.map { $0.suffix(length) }).count == uids.count { return length }
            length += 1
        }
        return longest
    }

    var blockedCount: Int { fleet.filter { $0.status == .blocked }.count }

    /// Creates on demand. Only ever called from event handling — a view that
    /// called it during `body` would be mutating state mid-render, so views read
    /// `states[key]`, which is always populated for a session in `summaries`.
    private func state(for key: String) -> SessionState {
        if let existing = states[key] { return existing }
        let fresh = SessionState(sessionKey: key)
        states[key] = fresh
        return fresh
    }

    func summary(for key: String) -> SessionSummary? {
        summaries.first { $0.sessionKey == key }
    }

    /// What to call this run on screen. The tmux name, which is what `codeconnect attach`
    /// takes and what the Mac's terminal is showing — never the uid, which is an
    /// identifier and not a name anybody uses.
    func displayName(for key: String) -> String {
        summary(for: key)?.displayName ?? key
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

    private func title(for summary: SessionSummary, state: SessionState?) -> String {
        // Claude's own generated title beats a tmux name every time.
        if let aiTitle = state?.aiTitle { return aiTitle }
        return summary.folderName.isEmpty ? summary.displayName : summary.folderName
    }

    private func subtitle(for summary: SessionSummary, state: SessionState?) -> String {
        guard let last = state?.timeline.last else { return summary.displayName }
        switch last.content {
        case .userMessage(let text): return "you: \(text.firstLine)"
        case .agentMessage(let text): return text.firstLine
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
            // A failure recorded against the old connection says nothing
            // about this one — the daemon may have been upgraded under us.
            commandCatalogFailures.removeAll()
            // A certificate the pairing's address can never validate is a fact
            // worth stating, not a silent downgrade.
            pairing.noteTLSUnusable(
                ack.capabilities.tls && pairing.endpoint?.hostIsIPLiteral == true)
            startCacheMigration()
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

    /// Aim the UI at a decision. Today this comes from a `codeconnect://` URL;
    /// once push notifications land the same entry point serves those too, which
    /// is why the target is a value the model holds rather than navigation done
    /// inline.
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
    /// A `codeconnect://session/…` URL may carry either: a uid (what a push
    /// notification will send, and what survives a name being reused) or a tmux
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
        subscribed.remove(key)
        states.removeValue(forKey: key)
        diffs.removeValue(forKey: key)
        pendingCacheWrites.remove(key)
        commandCatalogs.removeValue(forKey: key)
        commandCatalogFailures.removeValue(forKey: key)
        // A departed run's unsettled mutations die with it. Left behind, a
        // reused session *name* would collide a brand-new send with a dead
        // mutation's identity and the daemon would refuse it as a conflict.
        pendingSendIdentities = pendingSendIdentities.filter { $0.key.key != key }
        ReviewMarks.forget(sessionKey: key)
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

    /// Answers are idempotent by `(session, request_id)` on the daemon side, so
    /// a retry after any failure here is always safe.
    func answer(item: ApprovalItem, decision: AnswerDecision) async -> AnswerAttempt {
        guard !answersInFlight.contains(item.id) else {
            return .failed("An answer for this card is already being sent.")
        }
        answersInFlight.insert(item.id)
        defer { answersInFlight.remove(item.id) }
        let attempt = await sendAnswer(item: item, decision: decision)
        answerAttempts[item.id] = attempt
        return attempt
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
                return .applied(outcome)
            case .duplicate(let outcome, let stale):
                refreshFleet()
                return .duplicate(outcome: outcome, staleHash: stale)
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
        switch ClaudeCommandPolicy.action(for: text, catalog: commandCatalogs[key]) {
        case .nativeModel:
            return .refused("/model has its own control in the app — use the Model sheet.")
        case .blocked(_, let reason):
            return .refused(reason)
        case .passThrough:
            break
        }
        return await sendUnchecked(text: text, to: key, submit: submit)
    }

    /// The native `/model` adapter's own injection — the ONE path that may
    /// carry a slash command past the policy, because the sheet *is* the
    /// policy's answer for it.
    func sendModelCommand(_ argument: String, to key: String) async -> ComposeAttempt {
        await sendUnchecked(text: "/model \(argument)", to: key, submit: true)
    }

    private func sendUnchecked(
        text: String, to key: String, submit: Bool
    ) async -> ComposeAttempt {
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
                requestID: requestID, payloadHash: payloadHash)
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

    /// The installed Claude Code's slash-command inventory for one session,
    /// fetched once and kept. `nil` while never asked; a failure records its
    /// reason so the palette can say why instead of guessing.
    private(set) var commandCatalogs: [String: [String]] = [:]
    private(set) var commandCatalogFailures: [String: String] = [:]
    private var catalogFetchesInFlight: Set<String> = []

    func fetchCommandCatalog(for key: String) async {
        if commandCatalogs[key] != nil { return }
        // One fetch at a time per session: the palette can appear and
        // disappear faster than a probe answers.
        guard !catalogFetchesInFlight.contains(key) else { return }
        catalogFetchesInFlight.insert(key)
        defer { catalogFetchesInFlight.remove(key) }
        guard let capabilities = connection.capabilities else {
            // Handshake still in flight: no claim can be made either way.
            // The palette's task re-fires when capabilities arrive, so this
            // is a wait, not a failure — recording one here left the palette
            // stuck on a lie after reconnects.
            return
        }
        guard capabilities.servesCommandCatalog else {
            commandCatalogFailures[key] =
                "This Mac's daemon predates command discovery. Update it with codeconnect update."
            return
        }
        do {
            let result = try await connection.commandCatalog(session: key)
            // The session may have departed while the probe ran; writing the
            // answer back would resurrect state the sweep just removed.
            guard summaries.contains(where: { $0.sessionKey == key }) else { return }
            switch result {
            case .available(let commands, _, _):
                commandCatalogs[key] = commands
                commandCatalogFailures[key] = nil
            case .unavailable(let reason):
                commandCatalogFailures[key] = reason
            }
        } catch {
            guard summaries.contains(where: { $0.sessionKey == key }) else { return }
            commandCatalogFailures[key] = error.localizedDescription
        }
    }

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
