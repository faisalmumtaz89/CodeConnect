import Foundation
import Network
import Observation

/// Where the daemon lives and how to prove we may talk to it.
///
/// Two credentials are possible and they behave very differently. A *token* is
/// durable and survives launches. A *pairing code* is single-use with a
/// five-minute life and is never written to the Keychain — it exists only to be
/// exchanged, once, for a device token. Persisting a spent code would leave a
/// pairing that can never connect again and no way to tell that from a network
/// fault.
struct DaemonEndpoint: Codable, Sendable, Hashable {
    var host: String
    var port: Int
    var credential: HelloCredential
    /// Whether to speak `wss://`. Set from `hello_ack.capabilities.tls` rather
    /// than assumed — see `PairingStore.noteTLS`.
    var useTLS: Bool

    static let defaultPort = 8787

    init(host: String, port: Int, credential: HelloCredential, useTLS: Bool) {
        self.host = host
        self.port = port
        self.credential = credential
        self.useTLS = useTLS
    }

    var scheme: String { useTLS ? "wss" : "ws" }

    /// The durable credential, or nil while pairing.
    var token: String? {
        if case .token(let token) = credential { return token }
        return nil
    }

    var isPairingCode: Bool {
        if case .pairingCode = credential { return true }
        return false
    }

    /// Bracketed for IPv6, which a tailnet address may well be.
    var hostForURL: String {
        if host.contains(":") && !host.hasPrefix("[") { return "[\(host)]" }
        return host
    }

    var url: URL? { url(useTLS: useTLS) }

    func url(useTLS: Bool) -> URL? {
        URL(string: "\(useTLS ? "wss" : "ws")://\(hostForURL):\(port)")
    }

    var displayAddress: String { "\(scheme)://\(host):\(port)" }

    /// Whether `other` names the same daemon: the same host, compared without
    /// case, and the same port. Credential and scheme do not enter into it.
    func isSameAddress(as other: DaemonEndpoint) -> Bool {
        port == other.port && host.caseInsensitiveCompare(other.host) == .orderedSame
    }

    /// True when `host` is a literal address rather than a name.
    ///
    /// It matters for exactly one reason: a `tailscale cert` certificate carries
    /// a DNS SAN, so TLS cannot be validated against `100.x.y.z` no matter how
    /// willing both ends are. So when the daemon serves `wss://`, the QR has to
    /// carry the MagicDNS name rather than the tailnet address.
    var hostIsIPLiteral: Bool {
        if host.contains(":") { return true }  // IPv6
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return false }
        return parts.allSatisfy { part in
            !part.isEmpty && part.allSatisfy(\.isNumber) && (UInt8(part) != nil)
        }
    }

    /// Accepts what a person would actually type or paste: a bare host, a
    /// `host:port`, or a full URL with any of the four schemes.
    static func parse(address: String, token: String) -> DaemonEndpoint? {
        let cleanToken = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !cleanToken.isEmpty else { return nil }
        return parse(address: address, credential: .token(cleanToken))
    }

    static func parse(address: String, credential: HelloCredential) -> DaemonEndpoint? {
        let trimmed = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }

        var remainder = trimmed
        var useTLS = false
        for (prefix, tls) in [
            ("wss://", true), ("ws://", false), ("https://", true), ("http://", false),
        ]
        where remainder.lowercased().hasPrefix(prefix) {
            remainder = String(remainder.dropFirst(prefix.count))
            useTLS = tls
            break
        }
        remainder = remainder.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        if let slash = remainder.firstIndex(of: "/") { remainder = String(remainder[..<slash]) }
        guard !remainder.isEmpty else { return nil }

        var host = remainder
        var port = defaultPort

        if remainder.hasPrefix("[") {
            // [::1]:8787
            guard let close = remainder.firstIndex(of: "]") else { return nil }
            host = String(remainder[remainder.index(after: remainder.startIndex)..<close])
            let rest = remainder[remainder.index(after: close)...]
            if rest.hasPrefix(":"), let parsed = Int(rest.dropFirst()) { port = parsed }
        } else if let colon = remainder.lastIndex(of: ":"),
            remainder.filter({ $0 == ":" }).count == 1
        {
            host = String(remainder[..<colon])
            if let parsed = Int(remainder[remainder.index(after: colon)...]) { port = parsed }
        }

        guard !host.isEmpty, (1...65535).contains(port) else { return nil }
        return DaemonEndpoint(host: host, port: port, credential: credential, useTLS: useTLS)
    }

    // MARK: Codable

    /// Hand-written so that a pairing written by an earlier build —
    /// `{"host","port","token","useTLS"}` — still decodes after this update.
    /// An app update that silently unpairs the user would be indistinguishable
    /// from the Keychain item going missing.
    private enum CodingKeys: String, CodingKey {
        case host, port, token, useTLS
        case pairingCode = "pairing_code"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        host = try c.decode(String.self, forKey: .host)
        port = try c.decode(Int.self, forKey: .port)
        useTLS = try c.decodeIfPresent(Bool.self, forKey: .useTLS) ?? false
        if let token = try c.decodeIfPresent(String.self, forKey: .token), !token.isEmpty {
            credential = .token(token)
        } else if let code = try c.decodeIfPresent(String.self, forKey: .pairingCode),
            !code.isEmpty
        {
            credential = .pairingCode(code)
        } else {
            throw DecodingError.dataCorruptedError(
                forKey: .token, in: c, debugDescription: "endpoint has no credential")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(host, forKey: .host)
        try c.encode(port, forKey: .port)
        try c.encode(useTLS, forKey: .useTLS)
        switch credential {
        case .token(let token): try c.encode(token, forKey: .token)
        case .pairingCode(let code): try c.encode(code, forKey: .pairingCode)
        }
    }
}

// MARK: - QR payload

/// The JSON `codeconnect pair` renders as a QR code.
///
/// Locked shape: `{"v":1,"host":"<MagicDNS-or-tailnet-ip>","port":8787,"code":"<8 chars>"}`.
/// Parsing is strict about the version and permissive about nothing else: a
/// payload this build cannot vouch for is refused with a reason rather than
/// half-understood.
struct PairingQRPayload: Sendable, Hashable {
    var host: String
    var port: Int
    var code: String

    static let supportedVersion = 1

    enum Failure: LocalizedError, Equatable {
        case notJSON
        case unsupportedVersion(Int)
        case missingField(String)
        case badPort(Int)
        /// A host no other device could ever reach — see `unreachableHost`.
        case unreachableHost(String, String)

        var errorDescription: String? {
            switch self {
            case .notJSON:
                return "That QR code is not a CodeConnect pairing code."
            case .unsupportedVersion(let version):
                return
                    "That pairing code is version \(version); this app understands version \(PairingQRPayload.supportedVersion). Update the app or the daemon."
            case .missingField(let field):
                return "That pairing code is missing its \(field)."
            case .badPort(let port):
                return "That pairing code has an impossible port (\(port))."
            case .unreachableHost(let host, let why):
                return
                    "That pairing code points at \(host), which \(why). Connect the computer to the network your iPhone reaches it on (Tailscale, by default), run `codeconnect daemon restart`, then `codeconnect pair` again."
            }
        }
    }

    /// Why no other device could reach this host, or `nil` when it might.
    ///
    /// The mirror of `protocol::pairing::unreachable_host` on the Mac, and it is
    /// deliberately duplicated rather than trusted: the daemon that minted a code
    /// may be older than the check, and the phone should refuse a code it can
    /// prove is dead rather than spend five minutes discovering it. This is the
    /// strongest evidence the app ever has before it has spoken to anything — a
    /// fact established offline, from the payload alone.
    ///
    /// It rejects only what is *definitionally* unreachable. It deliberately does
    /// **not** demand a `100.64/10` address or a `.ts.net` name: a custom DNS
    /// name, a tailnet with its own domain, or a deliberately pinned `ws_bind` are
    /// all legitimate, and refusing the unfamiliar would be a guess.
    static func unreachableHost(_ host: String) -> String? {
        let trimmed = host.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty { return "is empty" }
        if trimmed.caseInsensitiveCompare("localhost") == .orderedSame {
            return "is the computer's own loopback name"
        }
        // An IPv6 literal may arrive bracketed, as it does inside a URL.
        var bare = trimmed
        if bare.hasPrefix("["), bare.hasSuffix("]") {
            bare = String(bare.dropFirst().dropLast())
        }
        // A name this app cannot parse as a literal is not a name it may reject.
        if let v4 = IPv4Address(bare) {
            let octets = v4.rawValue
            // **The whole of `127.0.0.0/8`, not just `127.0.0.1`.**
            // `IPv4Address.isLoopback` matches only the canonical literal, which
            // let `127.1.2.3` through — a real gap the Rust side does not have,
            // since `Ipv4Addr::is_loopback` covers the block.
            if octets.first == 127 { return "only that computer can reach" }
            if octets == Data([0, 0, 0, 0]) { return "is not an address to connect to" }
            if octets.count == 4, octets[0] == 169, octets[1] == 254 {
                return "is a link-local address that does not route"
            }
            return nil
        }
        if let v6 = IPv6Address(bare) {
            if v6.isLoopback { return "only that computer can reach" }
            if v6.isLinkLocal { return "is a link-local address that does not route" }
            if v6.rawValue.allSatisfy({ $0 == 0 }) { return "is not an address to connect to" }
            return nil
        }
        return nil
    }

    static func decode(_ scanned: String) -> Result<PairingQRPayload, Failure> {
        guard let data = scanned.data(using: .utf8),
            let value = try? JSONDecoder().decode(JSONValue.self, from: data),
            case .object = value
        else { return .failure(.notJSON) }

        let version = value["v"]?.intValue ?? 0
        guard version == supportedVersion else { return .failure(.unsupportedVersion(version)) }

        guard let host = value["host"]?.stringValue, !host.isEmpty else {
            return .failure(.missingField("host"))
        }
        guard let code = value["code"]?.stringValue, !code.isEmpty else {
            return .failure(.missingField("code"))
        }
        let port = value["port"]?.intValue ?? DaemonEndpoint.defaultPort
        guard (1...65535).contains(port) else { return .failure(.badPort(port)) }
        if let why = PairingQRPayload.unreachableHost(host) {
            return .failure(.unreachableHost(host, why))
        }

        return .success(PairingQRPayload(host: host, port: port, code: code))
    }

    /// The endpoint to open while exchanging the code for a device token.
    ///
    /// Always starts on `ws://`: whether the daemon has a certificate is a fact
    /// it reports in `hello_ack`, and guessing `wss://` at a daemon without one
    /// would fail the handshake with a TLS error that says nothing useful about
    /// what is wrong. `PairingStore.noteTLS` upgrades the moment the daemon says
    /// it can.
    var endpoint: DaemonEndpoint {
        DaemonEndpoint(host: host, port: port, credential: .pairingCode(code), useTLS: false)
    }
}

// MARK: - Typed-in credentials

/// What the user typed into the manual pairing field.
///
/// `codeconnect pair` prints both a QR *and* a human-readable code, and tells the user
/// they may "enter it by hand" — so the manual field has to accept a pairing
/// code as well as a static `codeconnect token`. The two are unmistakable by shape, which
/// is better than a segmented control the user has to get right: a token is 32+
/// hex characters, a code is eight alphanumerics that `codeconnect pair` prints
/// hyphenated for reading.
enum PairingCredentialInput {
    /// Nil when the text is neither shape. Refusing beats guessing: a
    /// misclassified credential fails at the daemon with an authentication
    /// error that says nothing about what was actually wrong.
    static func classify(_ raw: String) -> HelloCredential? {
        let cleaned = raw.filter { !$0.isWhitespace && $0 != "-" }
        guard !cleaned.isEmpty else { return nil }

        let isHex = cleaned.allSatisfy(\.isHexDigit)
        if isHex, cleaned.count >= 32 { return .token(cleaned) }

        if cleaned.count == 8, cleaned.allSatisfy({ $0.isLetter || $0.isNumber }) {
            // Codes are printed uppercase; normalising means a typed lowercase
            // code is not rejected for a reason nobody can see.
            return .pairingCode(cleaned.uppercased())
        }
        // Anything else that is long enough to be a token is treated as one:
        // the token format is the daemon's to change, and refusing a valid
        // credential is worse than letting the daemon refuse an invalid one.
        return cleaned.count >= 16 ? .token(cleaned) : nil
    }

    /// What the app understood, so the user can see it before committing.
    static func describe(_ credential: HelloCredential) -> String {
        switch credential {
        case .token: return "Read as a device token."
        case .pairingCode(let code): return "Read as the single-use pairing code \(code)."
        }
    }
}

// MARK: - Store

/// One computer this phone is paired with.
struct PairedMachine: Codable, Sendable, Hashable, Identifiable {
    /// This phone's own name for the pairing, minted when it was added. Names the
    /// machine's folder in the event cache.
    let id: String
    var endpoint: DaemonEndpoint
    /// What its daemon last said it is (minor 25), kept so a machine that is not
    /// connected still reads as itself.
    var name: String?
    var os: String?
    /// Which daemon it is, whatever address reached it (`hello_ack.machine_id`,
    /// minor 26). Nil until that daemon has said, and from an older one.
    var daemonID: String?
}

/// Owns the pairings. Every machine's whole record lives in one Keychain item
/// rather than only the tokens: one item to write, one to revoke.
///
/// Two machines are the same machine when they share a host and port, compared
/// without case — so re-scanning a Mac replaces only its own credential — or, once
/// their daemons have said, when they share a `daemonID`: the same Mac paired once
/// by address and once by name is merged into one (`merge(_:into:)`). From a
/// daemon too old to say, the two stay two; removing one is the owner's fix.
@MainActor
@Observable
final class PairingStore {
    private let legacyAccount: String
    /// Nil once the store holds a pairing in memory only: from then on nothing it
    /// holds is written, so a test or an automation launch never replaces the
    /// machines the device keeps.
    private var machinesAccount: String?

    /// Oldest first, so the last is the machine paired most recently.
    private(set) var machines: [PairedMachine] = []
    private(set) var lastError: String?
    /// By machine: set when its daemon advertises TLS but the pairing points at
    /// an IP literal, which no certificate can cover. Surfaced in Settings,
    /// because the fix — re-pair from a QR carrying the MagicDNS name — is the
    /// user's.
    private var tlsUnavailableReasons: [String: String] = [:]

    /// The machine paired most recently: the one Settings speaks for.
    var current: PairedMachine? { machines.last }
    var isPaired: Bool { !machines.isEmpty }

    init() {
        legacyAccount = "daemon"
        machinesAccount = "machines"
        load()
    }

    #if DEBUG
        /// Test seam: start empty, without reading the Keychain.
        ///
        /// `init()` adopts whatever pairing the device already holds, so a suite
        /// asserting on the *unpaired* state passes on a clean simulator and fails
        /// on one the app has been paired on — `startSampleFleet` is refused from
        /// a paired app by design, so the fleet never loads. Clearing the real
        /// item in `setUp` would make the suite pass by deleting the developer's
        /// pairing; this lets the suite own its own state instead of destroying
        /// theirs.
        init(ephemeral: Bool) {
            legacyAccount = "daemon"
            machinesAccount = nil
        }

        /// Test seam: the real Keychain, under accounts of the suite's own, so a
        /// suite exercises loading and migration without touching the pairing
        /// the simulator holds.
        init(keychainPrefix prefix: String) {
            legacyAccount = prefix + "daemon"
            machinesAccount = prefix + "machines"
            load()
        }
    #endif

    /// Builds before several machines kept one endpoint under the legacy account.
    /// It becomes the first machine, field for field, and its item is deleted only
    /// once the new one is written — so a migration stopped in between is finished
    /// on the next launch, and one that cannot write leaves the old item in place.
    private func load() {
        guard let machinesAccount else { return }
        if let data = Keychain.load(account: machinesAccount) {
            guard let stored = try? JSONDecoder().decode([PairedMachine].self, from: data) else {
                return
            }
            machines = stored
            Keychain.delete(account: legacyAccount)
        } else if let data = Keychain.load(account: legacyAccount),
            let endpoint = try? JSONDecoder().decode(DaemonEndpoint.self, from: data)
        {
            machines = [PairedMachine(id: UUID().uuidString, endpoint: endpoint)]
            if persist(machines) { Keychain.delete(account: legacyAccount) }
        }
    }

    /// The paired machine at this endpoint's host and port, if there is one.
    func machine(matching endpoint: DaemonEndpoint) -> PairedMachine? {
        machines.first { $0.endpoint.isSameAddress(as: endpoint) }
    }

    func machine(id: String) -> PairedMachine? {
        machines.first { $0.id == id }
    }

    /// Persist a durable pairing: added as a new machine, or — for a host and port
    /// already paired — replacing only that machine's endpoint, keeping its id, its
    /// cache and its name. Either way it becomes the machine paired most recently.
    /// `id` names a new machine; the app chooses it when the pairing starts, so the
    /// machine's cache is its own from the first frame.
    ///
    /// A pairing code is refused on purpose: it is single-use, so storing one would
    /// produce a pairing that looks valid and can never connect.
    func save(_ endpoint: DaemonEndpoint, id: String = UUID().uuidString) {
        guard !endpoint.isPairingCode else {
            lastError = "Internal: a single-use pairing code cannot be stored as a pairing."
            return
        }
        let next = upserting(endpoint, id: id)
        if persist(next) { machines = next }
    }

    #if DEBUG
        /// Test seam: hold a pairing in memory only, skipping the Keychain.
        /// The automation launches (`-CC_HOST`/`-CC_TOKEN`) run unsigned under
        /// `CODE_SIGNING_ALLOWED=NO`, and an unsigned build has no Keychain
        /// entitlement — `save` fails with "A required entitlement isn't
        /// present", measured, which left every automation launch unpaired.
        /// Ephemeral is also the more hermetic behavior for a test seam, and so is
        /// holding this machine alone: every paired machine is dialled, and an
        /// automation launch talks to the daemon it names and no other. One fixed
        /// id, so its cache folder is the same from one automation launch to the
        /// next.
        func saveEphemeral(_ endpoint: DaemonEndpoint) {
            guard !endpoint.isPairingCode else { return }
            machinesAccount = nil
            machines = [PairedMachine(id: "automation", endpoint: endpoint)]
            lastError = nil
        }
    #endif

    /// The daemon has answered a pairing code with a device token. Swap the
    /// spent code for the durable credential and keep everything else.
    func adopt(deviceToken: String, from endpoint: DaemonEndpoint, id: String) {
        var durable = endpoint
        durable.credential = .token(deviceToken)
        save(durable, id: id)
    }

    /// Remember the transport that actually completed a handshake with this
    /// machine, so the next launch starts on it instead of paying for the
    /// fallback ladder again.
    func noteTransport(useTLS: Bool, id: String) {
        guard let endpoint = machine(id: id)?.endpoint, endpoint.useTLS != useTLS,
            !endpoint.isPairingCode
        else { return }
        update(id) { $0.endpoint.useTLS = useTLS }
    }

    /// Keep what this machine's daemon says it is, for when it is not
    /// connected. Written only when it changed.
    func noteMachine(name: String?, os: String?, daemonID: String?, id: String) {
        guard let machine = machine(id: id),
            machine.name != name || machine.os != os || machine.daemonID != daemonID
        else { return }
        update(id) {
            $0.name = name
            $0.os = os
            $0.daemonID = daemonID
        }
    }

    /// The other machine whose daemon is this one's: the same Mac, paired under a
    /// second address.
    func sameDaemon(as id: String) -> PairedMachine? {
        guard let daemonID = machine(id: id)?.daemonID else { return nil }
        return machines.first { $0.id != id && $0.daemonID == daemonID }
    }

    /// One Mac paired under two addresses becomes one machine. `kept` stays — its
    /// id, so its cache folder, and its name — and takes `dropped`'s endpoint and
    /// place in the order, which are the later pairing's: the address and
    /// credential the owner chose last, and the machine Settings speaks for.
    /// `dropped` goes. Persisted before it is published, as `save` is.
    func merge(_ dropped: String, into kept: String) {
        guard var survivor = machine(id: kept), let other = machine(id: dropped) else { return }
        survivor.endpoint = other.endpoint
        let next = machines.compactMap { machine -> PairedMachine? in
            switch machine.id {
            case kept: return nil
            case dropped: return survivor
            default: return machine
            }
        }
        guard persist(next) else { return }
        machines = next
        tlsUnavailableReasons[kept] = tlsUnavailableReasons.removeValue(forKey: dropped)
    }

    /// Forget one machine. Every other machine's record is left as it was.
    func remove(id: String) {
        let next = machines.filter { $0.id != id }
        guard persist(next) else { return }
        machines = next
        tlsUnavailableReasons[id] = nil
    }

    /// Change one machine's record where it stands: no other machine's record,
    /// and no machine's place in the order, moves.
    private func update(_ id: String, _ change: (inout PairedMachine) -> Void) {
        guard let index = machines.firstIndex(where: { $0.id == id }) else { return }
        var next = machines
        change(&next[index])
        machines = next
        persist(next)
    }

    private func upserting(_ endpoint: DaemonEndpoint, id: String) -> [PairedMachine] {
        var next = machines
        var machine = PairedMachine(id: id, endpoint: endpoint)
        if let index = next.firstIndex(where: { $0.endpoint.isSameAddress(as: endpoint) }) {
            machine = next.remove(at: index)
            machine.endpoint = endpoint
        }
        next.append(machine)
        return next
    }

    @discardableResult
    private func persist(_ machines: [PairedMachine]) -> Bool {
        guard let machinesAccount else {
            lastError = nil
            return true
        }
        do {
            try Keychain.save(JSONEncoder().encode(machines), account: machinesAccount)
            lastError = nil
            return true
        } catch {
            lastError = error.localizedDescription
            return false
        }
    }

    /// Record that the daemon has a certificate this pairing can never use.
    ///
    /// A `tailscale cert` certificate carries a DNS SAN, so a pairing made
    /// against `100.x.y.z` cannot validate it. The fix is the user's — re-pair
    /// from a QR carrying the MagicDNS name — so it is stated rather than
    /// silently endured.
    func noteTLSUnusable(_ unusable: Bool, id: String) {
        guard let endpoint = machine(id: id)?.endpoint else { return }
        tlsUnavailableReasons[id] =
            unusable
            ? "This daemon serves TLS, but the pairing points at the address \(endpoint.host) and a `tailscale cert` certificate only covers the MagicDNS name. Scan a fresh QR code to move onto the encrypted connection."
            : nil
    }

    func tlsUnavailableReason(for id: String) -> String? {
        tlsUnavailableReasons[id]
    }
}
