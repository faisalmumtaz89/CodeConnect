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
public final class Machines {
    let pairing: PairingStore
    public let settings: AppSettings
    private let cache: EventCache
    private let relayEnrollment: RelayEnrollment
    private let codexSpentLedger: CodexSpentLedger
    /// Makes each model its own push registrar. Nil: no model registers for push.
    private let makePushRegistrar: (@MainActor () -> any PushRegistrar)?

    /// One per paired machine, oldest first, or the one with no machine.
    public private(set) var models: [AppModel] = []
    /// A pairing with an address no machine has, until its daemon answers it.
    private(set) var candidate: AppModel?
    /// The owner's computers a daemon listed that this device holds no pairing
    /// for, each being joined without a code until its daemon lets this device
    /// in — then it is a machine — or says no, and stays here, refused.
    private(set) var joining: [(computer: PeerComputer, model: AppModel)] = []
    /// Machines joined from a list in this launch, until their daemon says which
    /// it is: one that is a removed computer under a new address leaves again.
    private var joinedFromList: Set<String> = []
    /// What a daemon last said about its computer's Tailscale.
    public private(set) var tailnet: PeerDirectory?
    /// Where a deep link (a `codeconnect://` URL, or a tapped notification)
    /// wants the UI to go. Cleared by whichever surface consumes it.
    public var pendingDeepLink: DeepLink?

    public init(
        pairing: PairingStore = PairingStore(), cache: EventCache = EventCache(),
        settings: AppSettings = AppSettings(),
        relayEnrollment: RelayEnrollment = RelayEnrollment(),
        codexSpentLedger: CodexSpentLedger = CodexSpentLedger(),
        makePushRegistrar: (@MainActor () -> any PushRegistrar)? = nil
    ) {
        // Startup housekeeping, on the one object the app builds exactly once.
        LegacyCredentials.purge()
        self.pairing = pairing
        self.cache = cache
        self.settings = settings
        self.relayEnrollment = relayEnrollment
        self.codexSpentLedger = codexSpentLedger
        self.makePushRegistrar = makePushRegistrar
        // What an earlier build cached for its one Mac is the first machine's.
        if let first = pairing.machines.first { cache.moveUnfiledFiles(toMachine: first.id) }
        models = modelsForPairing()
    }

    #if DEBUG
        /// Test seam: the app holding one model a test has built and staged, so
        /// a screen hosted over it finds the fleet, the Deck and the deep-link
        /// slot where the app keeps them.
        public init(holding model: AppModel) {
            pairing = model.pairing
            cache = EventCache()
            settings = model.settings
            relayEnrollment = RelayEnrollment()
            codexSpentLedger = model.codexSpentLedger
            makePushRegistrar = nil
            models = [model]
        }
    #endif

    private func makeModel(_ machineID: String?) -> AppModel {
        let model = AppModel(
            machineID: machineID, pairing: pairing, cache: cache, settings: settings,
            relayEnrollment: relayEnrollment, codexSpentLedger: codexSpentLedger,
            pushRegistrar: makePushRegistrar?())
        model.onIdentified = { [weak self, weak model] in
            guard let self, let model else { return }
            self.identified(model)
        }
        model.onPeers = { [weak self] directory in self?.join(directory) }
        return model
    }

    private func modelsForPairing() -> [AppModel] {
        pairing.isPaired ? pairing.machines.map { makeModel($0.id) } : [makeModel(nil)]
    }

    public func bootstrap() {
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
        #if DEBUG
            // After the starts: the second machine is one that is offline.
            applyFixtureSecondMachine()
        #endif
    }

    // MARK: - The fleet

    /// The fleet is the root once there is something to show it from.
    public var showsFleet: Bool { models.contains(where: \.showsFleet) }

    /// Whether the fleet may be shown: every machine has caught up with its Mac,
    /// or waited long enough for one that is not answering.
    public var launchSettled: Bool { models.allSatisfy(\.launchSettled) }

    /// Every machine waits out its own launch, all at once: one Mac that is not
    /// answering costs the wait it costs alone. Waited for again while any is
    /// still unsettled, so the machines this ends on are the ones there are —
    /// the loader can start before the launch seams have replaced them.
    public func waitOutLaunch() async {
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
    public var fleet: [FleetRow] { FleetOrdering.sort(models.flatMap(\.fleet)) }

    /// Every machine's open cards in one queue, each ranked by what its own
    /// daemon says about it.
    public var deck: [ApprovalItem] {
        DeckOrdering.sort(models.flatMap(\.deck)) { self.profile(forSession: $0.sessionKey) }
    }

    public var hasLiveFleet: Bool { models.contains(where: \.hasLiveFleet) }
    public var isCatchingUp: Bool { models.contains(where: \.isCatchingUp) }

    /// How many catch-ups have changed the screen, on every machine.
    public var catchUpReleases: Int { models.reduce(0) { $0 + $1.catchUpReleases } }

    /// The Deck a notification opened waits for each machine catching up on a
    /// live link. One that cannot be reached is not waited for, unless none can:
    /// then the Deck waits, and says why, as it does for one machine.
    public var deckAwaitsCatchUp: Bool {
        let waiting = models.filter(\.deckAwaitsCatchUp)
        guard !waiting.isEmpty else { return false }
        return waiting.contains { $0.connection.phase.isConnected }
            || !models.contains { $0.connection.phase.isConnected }
    }

    public func deckClosed() {
        for model in models { model.deckClosed() }
    }

    public func refreshFleet() {
        for model in models { model.refreshFleet() }
    }

    /// Dial every machine whose link is down now, rather than at its next
    /// backoff.
    public func retryNow() {
        for model in models { model.connection.retryNow() }
    }

    public func scenePhaseChanged(to phase: ScenePhase) {
        for model in models + [candidate].compactMap({ $0 }) + joining.map(\.model) {
            model.scenePhaseChanged(to: phase)
        }
    }

    /// The model of the machine this session is on.
    public func model(forSession key: String) -> AppModel? {
        models.first { $0.shownSummary(for: key) != nil || $0.summary(for: key) != nil }
    }

    public func model(for machineID: String?) -> AppModel? {
        models.first { $0.machineID == machineID }
    }

    /// What the daemon a session is on can do.
    public func profile(forSession key: String) -> DaemonProfile {
        (model(forSession: key) ?? linkModel).daemonProfile
    }

    /// The machine paired most recently, whose model Settings speaks for.
    public var current: AppModel {
        pairing.current.flatMap { model(for: $0.id) } ?? models[models.count - 1]
    }

    /// The machine the app-wide surfaces — the one banner, the Deck's wait —
    /// speak for: the best connected, so they report trouble only when every
    /// machine has it. Each machine's own link is on its project cards and its
    /// sessions. With one machine, that machine.
    public var linkModel: AppModel {
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
    public func resolveSessionKey(reference: String) -> String? {
        models.lazy.compactMap { $0.resolveSessionKey(reference: reference) }.first
    }

    /// Removes each session on its own machine, and returns why each one still
    /// in the fleet was not removed, by key. A machine whose link has gone stops
    /// asking for its own sessions only.
    public func removeSessions(keys: [String]) async -> [String: String] {
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
    public func pair(withQR payload: PairingQRPayload) {
        pairing.undecline(host: payload.host)
        pairingModel(for: payload.endpoint).pair(withQR: payload)
    }

    /// Pair from a host and a token typed by hand. See `AppModel.pair(with:)`.
    public func pair(with endpoint: DaemonEndpoint) {
        pairing.undecline(host: endpoint.host)
        pairingModel(for: endpoint).pair(with: endpoint)
    }

    // MARK: - The owner's other computers

    /// The computers a daemon lists, joined without a code: each this device
    /// has no pairing for, has not removed and is not already joining.
    func join(_ directory: PeerDirectory) {
        tailnet = directory
        for computer in directory.computers {
            guard pairing.machine(matching: computer.endpoint) == nil,
                !pairing.isDeclined(host: computer.host), joiningIndex(at: computer.endpoint) == nil
            else { continue }
            let model = makeModel(UUID().uuidString)
            model.onPaired = { [weak self, weak model] in
                guard let self, let model else { return }
                self.adopt(model)
            }
            model.start()
            model.join(computer)
            joining.append((computer, model))
        }
    }

    private func joiningIndex(at endpoint: DaemonEndpoint) -> Int? {
        joining.firstIndex { $0.computer.endpoint.isSameAddress(as: endpoint) }
    }

    /// The listed computers whose daemon would not let this device in without a
    /// code.
    public var computersNeedingCode: [PeerComputer] {
        joining.compactMap { entry in
            guard case .failed = entry.model.connection.phase else { return nil }
            return entry.computer
        }
    }

    public var isPairing: Bool { candidate != nil || models.contains(where: \.isPairing) }

    public var pairingError: String? {
        (candidate ?? models.first(where: \.isPairing))?.pairingError
    }

    public func cancelPairing() {
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
        // A listed computer being paired by code is no longer joined from the
        // list: one link to it, and no "scan its code" left behind.
        if let index = joiningIndex(at: endpoint) { joining.remove(at: index).model.leave() }
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

    /// A new machine's daemon answered its pairing, or let it join from a list:
    /// it joins the fleet, and the model there was with nothing paired goes.
    private func adopt(_ model: AppModel) {
        if model === candidate {
            candidate = nil
        } else if let index = joining.firstIndex(where: { $0.model === model }), let id = model.machineID {
            joining.remove(at: index)
            joinedFromList.insert(id)
        } else {
            return
        }
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
    ///
    /// A machine joined from a list whose daemon is a removed computer, under an
    /// address it was not removed by, leaves again, and that address joins the
    /// removed ones. Any other machine this device holds is wanted, so its daemon
    /// is no longer a removed one.
    private func identified(_ answered: AppModel) {
        guard let id = answered.machineID else { return }
        if let daemonID = pairing.machine(id: id)?.daemonID {
            if joinedFromList.remove(id) != nil, pairing.isDeclined(daemonID: daemonID) {
                removeMachine(id: id)
                return
            }
            pairing.undecline(daemonID: daemonID)
        }
        guard let other = pairing.sameDaemon(as: id),
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
    public func unpair(_ model: AppModel) {
        guard let id = model.machineID else {
            for model in models { model.stopSampleFleet() }
            return
        }
        removeMachine(id: id)
    }

    /// Forget one machine — its pairing, its link and its cache — and no other
    /// machine's. It stays removed: no daemon's list adds it back.
    func removeMachine(id: String) {
        if let machine = pairing.machine(id: id) {
            pairing.decline(daemonID: machine.daemonID, host: machine.endpoint.host)
        }
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
    public func open(url: URL) -> Bool {
        guard let link = DeepLink(url: url) else { return false }
        pendingDeepLink = link
        return true
    }

    public func consumeDeepLink() -> DeepLink? {
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
    public func consumePendingTap() {
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

        /// Test seam: `-CC_FIXTURE_SECOND_MACHINE <name>` stands a second machine
        /// beside a fixture run's, so Settings can be photographed holding two.
        /// Its record lives in its own in-memory store, never the keychain, and
        /// it is never started: it is a machine that is offline.
        private func applyFixtureSecondMachine() {
            guard models[0].fixturesActive,
                let name = UserDefaults.standard.string(forKey: "CC_FIXTURE_SECOND_MACHINE")
            else { return }
            let id = "fixture-second-machine"
            let store = PairingStore(ephemeral: true)
            store.save(
                DaemonEndpoint(host: "100.64.0.2", port: 8787, credential: .token("fixture"), useTLS: false),
                id: id)
            store.noteMachine(name: name, os: "linux", daemonID: nil, id: id)
            let second = AppModel(
                machineID: id, pairing: store, cache: cache, settings: settings,
                relayEnrollment: relayEnrollment, codexSpentLedger: codexSpentLedger,
                pushRegistrar: makePushRegistrar?())
            models.append(second)
            // `-CC_FIXTURE_SECOND_LIVE` puts it online with its own fleet.
            if UserDefaults.standard.bool(forKey: "CC_FIXTURE_SECOND_LIVE") {
                second.applySecondMachineFixtures()
            }
        }
    #endif
}
