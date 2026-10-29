import SwiftUI

/// Every paired machine, each with a model of its own, and the one fleet they
/// make together.
///
/// **A model is one machine's, whole.** Its connection, its cache folder, its
/// catch-up and every session it holds are its own, so nothing one Mac says can
/// land in another Mac's state, and each machine's link is judged by itself.
/// Whatever acts on a session goes to the model of the machine the session is
/// on: the screens under a session are handed that model. What belongs to the
/// phone rather than to a machine — the pairings, the settings, the relay
/// credential, what has been sent and cannot be accounted for — is one
/// instance, shared by every model.
///
/// With nothing paired there is exactly one model, with no machine: the welcome
/// screen's, the sample fleet's and a fixture run's.
@MainActor
@Observable
final class Machines {
    let pairing: PairingStore
    let settings: AppSettings
    private let cache: EventCache
    private let relayEnrollment: RelayEnrollment
    private let codexSpentLedger: CodexSpentLedger

    /// One per paired machine, oldest first, or the one with no machine.
    private(set) var models: [AppModel] = []
    /// A pairing with an address no machine has, until its daemon answers it.
    private(set) var candidate: AppModel?
    /// Where a deep link (a `codeconnect://` URL, or a tapped notification)
    /// wants the UI to go. Cleared by whichever surface consumes it.
    var pendingDeepLink: DeepLink?

    init(
        pairing: PairingStore = PairingStore(), cache: EventCache = EventCache(),
        settings: AppSettings = AppSettings(),
        relayEnrollment: RelayEnrollment = RelayEnrollment(),
        codexSpentLedger: CodexSpentLedger = CodexSpentLedger()
    ) {
        // Startup housekeeping, on the one object the app builds exactly once.
        LegacyCredentials.purge()
        self.pairing = pairing
        self.cache = cache
        self.settings = settings
        self.relayEnrollment = relayEnrollment
        self.codexSpentLedger = codexSpentLedger
        // What an earlier build cached for its one Mac is the first machine's.
        if let first = pairing.machines.first { cache.moveUnfiledFiles(toMachine: first.id) }
        models = modelsForPairing()
    }

    #if DEBUG
        /// Test seam: the app holding one model a test has built and staged, so
        /// a screen hosted over it finds the fleet, the Deck and the deep-link
        /// slot where the app keeps them.
        init(holding model: AppModel) {
            pairing = model.pairing
            cache = EventCache()
            settings = model.settings
            relayEnrollment = RelayEnrollment()
            codexSpentLedger = model.codexSpentLedger
            models = [model]
        }
    #endif

    private func makeModel(_ machineID: String?) -> AppModel {
        let model = AppModel(
            machineID: machineID, pairing: pairing, cache: cache, settings: settings,
            relayEnrollment: relayEnrollment, codexSpentLedger: codexSpentLedger)
        model.onIdentified = { [weak self, weak model] in
            guard let self, let model else { return }
            self.identified(model)
        }
        return model
    }

    private func modelsForPairing() -> [AppModel] {
        pairing.isPaired ? pairing.machines.map { makeModel($0.id) } : [makeModel(nil)]
    }

    func bootstrap() {
        #if DEBUG
            applyAutomationPairing()
            applyAutomationPairingCode()
            // Before anything reads from disk: a fixture run has to be hermetic
            // or a previous live session's cache bleeds into it, and a test that
            // is half real data is a test of nothing. One model, and no other
            // machine dialled.
            models[0].applyFixtures()
            if models[0].fixturesActive { models = [models[0]] }
            // After the fixtures, so a link naming one of their cards resolves.
            applyAutomationDeepLink()
        #endif
        for model in models { model.start() }
    }

    // MARK: - The fleet

    /// The fleet is the root once there is something to show it from.
    var showsFleet: Bool { models.contains(where: \.showsFleet) }

    /// Whether the fleet may be shown: every machine has caught up with its Mac,
    /// or waited long enough for one that is not answering.
    var launchSettled: Bool { models.allSatisfy(\.launchSettled) }

    /// Every machine waits out its own launch, all at once: one Mac that is not
    /// answering costs the wait it costs alone. Waited for again while any is
    /// still unsettled, so the machines this ends on are the ones there are —
    /// the loader can start before the launch seams have replaced them.
    func waitOutLaunch() async {
        while !launchSettled, !Task.isCancelled {
            await withTaskGroup(of: Void.self) { group in
                for model in models where !model.launchSettled {
                    group.addTask { await model.waitOutLaunch() }
                }
            }
        }
    }

    /// Every machine's sessions, in the fleet's one order. Each row names its
    /// machine (`FleetRow.machineID`, `machineName`); that machine's link is
    /// `model(for: row.machineID)?.linkHealth`.
    var fleet: [FleetRow] { FleetOrdering.sort(models.flatMap(\.fleet)) }

    /// Every machine's open cards in one queue, each ranked by what its own
    /// daemon says about it.
    var deck: [ApprovalItem] {
        DeckOrdering.sort(models.flatMap(\.deck)) { self.profile(forSession: $0.sessionKey) }
    }

    var hasLiveFleet: Bool { models.contains(where: \.hasLiveFleet) }
    var isCatchingUp: Bool { models.contains(where: \.isCatchingUp) }

    /// How many catch-ups have changed the screen, on every machine.
    var catchUpReleases: Int { models.reduce(0) { $0 + $1.catchUpReleases } }

    /// The Deck a notification opened waits for each machine catching up on a
    /// live link. One that cannot be reached is not waited for, unless none can:
    /// then the Deck waits, and says why, as it does for one machine.
    var deckAwaitsCatchUp: Bool {
        let waiting = models.filter(\.deckAwaitsCatchUp)
        guard !waiting.isEmpty else { return false }
        return waiting.contains { $0.connection.phase.isConnected }
            || !models.contains { $0.connection.phase.isConnected }
    }

    func deckClosed() {
        for model in models { model.deckClosed() }
    }

    func refreshFleet() {
        for model in models { model.refreshFleet() }
    }

    /// Dial every machine whose link is down now, rather than at its next
    /// backoff.
    func retryNow() {
        for model in models { model.connection.retryNow() }
    }

    func scenePhaseChanged(to phase: ScenePhase) {
        for model in models + [candidate].compactMap({ $0 }) { model.scenePhaseChanged(to: phase) }
    }

    /// The model of the machine this session is on.
    func model(forSession key: String) -> AppModel? {
        models.first { $0.shownSummary(for: key) != nil || $0.summary(for: key) != nil }
    }

    func model(for machineID: String?) -> AppModel? {
        models.first { $0.machineID == machineID }
    }

    /// What the daemon a session is on can do.
    func profile(forSession key: String) -> DaemonProfile {
        (model(forSession: key) ?? linkModel).daemonProfile
    }

    /// The machine paired most recently, whose model Settings speaks for.
    var current: AppModel {
        pairing.current.flatMap { model(for: $0.id) } ?? models[models.count - 1]
    }

    /// The machine the app-wide surfaces — the link pill, the one banner, the
    /// Deck's wait — speak for: the best connected, so they report trouble only
    /// when every machine has it. Each machine's own link is on its rows and its
    /// sessions. With one machine, that machine.
    var linkModel: AppModel {
        models.min { Self.reach($0.linkHealth.level) < Self.reach($1.linkHealth.level) }
            ?? models[0]
    }

    private static func reach(_ level: LinkHealth.Level) -> Int {
        switch level {
        case .live: return 0
        case .lagging: return 1
        case .stale: return 2
        case .connecting: return 3
        case .rejected: return 4
        case .offline: return 5
        }
    }

    /// A link's session reference, resolved on whichever machine holds it.
    func resolveSessionKey(reference: String) -> String? {
        models.lazy.compactMap { $0.resolveSessionKey(reference: reference) }.first
    }

    /// Removes each session on its own machine, and returns why each one still
    /// in the fleet was not removed, by key. A machine whose link has gone stops
    /// asking for its own sessions only.
    func removeSessions(keys: [String]) async -> [String: String] {
        var refused: [String: String] = [:]
        for model in models {
            let own = keys.filter { self.model(forSession: $0) === model }
            guard !own.isEmpty else { continue }
            refused.merge(await model.removeSessions(keys: own)) { $1 }
        }
        return refused
    }

    // MARK: - Pairing

    /// Pair from a scanned QR code. See `AppModel.pair(withQR:)`.
    func pair(withQR payload: PairingQRPayload) {
        pairingModel(for: payload.endpoint).pair(withQR: payload)
    }

    /// Pair from a host and a token typed by hand. See `AppModel.pair(with:)`.
    func pair(with endpoint: DaemonEndpoint) {
        pairingModel(for: endpoint).pair(with: endpoint)
    }

    var isPairing: Bool { candidate != nil || models.contains(where: \.isPairing) }

    var pairingError: String? {
        (candidate ?? models.first(where: \.isPairing))?.pairingError
    }

    func cancelPairing() {
        dropCandidate()
        for model in models where model.isPairing { model.cancelPairing() }
    }

    /// The model a pairing with this address runs on: the paired machine's own,
    /// so a re-scan replaces only its credential, on its one link — or a new
    /// machine's, which joins the fleet once its daemon answers.
    private func pairingModel(for endpoint: DaemonEndpoint) -> AppModel {
        // Sample and live never mix, so the sample fleet goes where a live link
        // begins.
        for model in models { model.stopSampleFleet() }
        dropCandidate()
        if let machine = pairing.machine(matching: endpoint), let model = model(for: machine.id) {
            return model
        }
        let model = makeModel(UUID().uuidString)
        model.onPaired = { [weak self, weak model] in
            guard let self, let model else { return }
            self.adopt(model)
        }
        model.start()
        candidate = model
        return model
    }

    /// A new machine's daemon answered its pairing: it joins the fleet, and the
    /// model there was with nothing paired goes.
    private func adopt(_ model: AppModel) {
        guard model === candidate else { return }
        candidate = nil
        // Added to a fleet already on screen, it joins it as its sessions
        // arrive, rather than sending the app back to the loader.
        if models.contains(where: \.launchSettled) { model.settleLaunch() }
        for unpaired in models where unpaired.machineID == nil { unpaired.leave() }
        models.removeAll { $0.machineID == nil }
        models.append(model)
    }

    /// A machine's daemon has said which daemon it is. Another machine that is the
    /// same daemon, paired under another address, is the same Mac: the one paired
    /// first keeps its model, its cache and its name, and takes the later
    /// pairing's address, credential and place (`PairingStore.merge`); the later
    /// one's link closes and its folder goes. A daemon too old to say is never
    /// merged.
    private func identified(_ answered: AppModel) {
        guard let id = answered.machineID, let other = pairing.sameDaemon(as: id),
            let mine = pairing.machines.firstIndex(where: { $0.id == id }),
            let theirs = pairing.machines.firstIndex(where: { $0.id == other.id })
        else { return }
        let (kept, dropped) = mine < theirs ? (id, other.id) : (other.id, id)
        pairing.merge(dropped, into: kept)
        guard pairing.machine(id: dropped) == nil else { return }
        if let index = models.firstIndex(where: { $0.machineID == dropped }) {
            models.remove(at: index).leave()
        }
        cache.removeMachine(dropped)
        if let survivor = model(for: kept), let endpoint = survivor.endpoint {
            survivor.connection.start(endpoint: endpoint)
        }
    }

    /// A pairing that never finished keeps no link and no folder behind it.
    private func dropCandidate() {
        guard let candidate, let id = candidate.machineID else { return }
        self.candidate = nil
        candidate.leave()
        cache.removeMachine(id)
    }

    /// Remove the machine this model is for: the one the screen asking shows.
    /// With nothing paired, that is leaving the sample fleet.
    func unpair(_ model: AppModel) {
        guard let id = model.machineID else {
            for model in models { model.stopSampleFleet() }
            return
        }
        removeMachine(id: id)
    }

    /// Forget one machine — its pairing, its link and its cache — and no other
    /// machine's.
    func removeMachine(id: String) {
        pairing.remove(id: id)
        guard pairing.machine(id: id) == nil else { return }
        if let index = models.firstIndex(where: { $0.machineID == id }) {
            models.remove(at: index).leave()
        }
        cache.removeMachine(id)
        guard models.isEmpty else { return }
        let unpaired = makeModel(nil)
        models = [unpaired]
        unpaired.start()
    }

    // MARK: - Deep links

    /// Aim the UI at a decision, from a `codeconnect://` URL. A tapped
    /// notification takes a different route in — it names nothing, so it has no
    /// URL — and the two meet at `pendingDeepLink`, which is why the target is a
    /// value held here rather than navigation done inline.
    func open(url: URL) -> Bool {
        guard let link = DeepLink(url: url) else { return false }
        pendingDeepLink = link
        return true
    }

    func consumeDeepLink() -> DeepLink? {
        defer { pendingDeepLink = nil }
        return pendingDeepLink
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
    /// than a stale one. Opened mid-catch-up, the list waits for it
    /// (`deckAwaitsCatchUp`).
    func openFromNotification(kind: String?) {
        pendingDeepLink = kind == "approval" ? .deck(requestID: nil) : .fleet
        guard kind == "approval" else { return }
        for model in models { model.deckOpenedFromNotification() }
    }

    /// Drain a tap that arrived before anything was listening.
    func consumePendingTap() {
        if let tap = PushWire.consumeTap() { openFromNotification(kind: tap.kind) }
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
            // Ephemeral on purpose: this unsigned automation build has no
            // Keychain entitlement, so a durable save cannot work — measured
            // as every `-CC_HOST` launch landing on the welcome screen.
            pairing.saveEphemeral(endpoint)
            models = modelsForPairing()
            if defaults.bool(forKey: "CC_RESET_CACHE") { models[0].resetCacheForAutomation() }
        }

        /// Test seam: `-CC_PAIR_HOST <host> -CC_PAIR_CODE <code>` runs the real
        /// pairing exchange — tokenless `hello{pairing_code}`, `hello_ack`
        /// device token, Keychain — without a camera. It is the same call the
        /// scanner makes; only the source of the payload differs, which is
        /// exactly the part a simulator cannot provide — so, like a scan, it adds
        /// a machine and replaces none.
        private func applyAutomationPairingCode() {
            let defaults = UserDefaults.standard
            guard let host = defaults.string(forKey: "CC_PAIR_HOST"),
                let code = defaults.string(forKey: "CC_PAIR_CODE"), !code.isEmpty
            else { return }
            let port = defaults.integer(forKey: "CC_PAIR_PORT")
            pair(
                withQR: PairingQRPayload(
                    host: host, port: (1...65535).contains(port) ? port : DaemonEndpoint.defaultPort,
                    code: code))
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
    #endif
}
