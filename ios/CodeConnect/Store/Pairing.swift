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
                    "That pairing code points at \(host), which \(why). Bring Tailscale up on the Mac, run `codeconnect daemon restart`, then `codeconnect pair` again."
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
            return "is the Mac's own loopback name"
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
            if octets.first == 127 { return "only that Mac can reach" }
            if octets == Data([0, 0, 0, 0]) { return "is not an address to connect to" }
            if octets.count == 4, octets[0] == 169, octets[1] == 254 {
                return "is a link-local address that does not route"
            }
            return nil
        }
        if let v6 = IPv6Address(bare) {
            if v6.isLoopback { return "only that Mac can reach" }
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

/// Owns the pairing. The whole record lives in the Keychain rather than only
/// the token: one item to write, one to revoke.
@MainActor
@Observable
final class PairingStore {
    private static let account = "daemon"

    private(set) var endpoint: DaemonEndpoint?
    private(set) var lastError: String?
    /// Set when the daemon advertises TLS but the pairing points at an IP
    /// literal, which no certificate can cover. Surfaced in Settings, because
    /// the fix — re-pair from a QR carrying the MagicDNS name — is the user's.
    private(set) var tlsUnavailableReason: String?

    var isPaired: Bool { endpoint != nil }

    init() {
        guard let data = Keychain.load(account: Self.account) else { return }
        endpoint = try? JSONDecoder().decode(DaemonEndpoint.self, from: data)
    }

    /// Persist a durable pairing. A pairing code is refused on purpose: it is
    /// single-use, so storing one would produce a pairing that looks valid and
    /// can never connect.
    func save(_ endpoint: DaemonEndpoint) {
        guard !endpoint.isPairingCode else {
            lastError = "Internal: a single-use pairing code cannot be stored as a pairing."
            return
        }
        do {
            try Keychain.save(JSONEncoder().encode(endpoint), account: Self.account)
            self.endpoint = endpoint
            lastError = nil
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// The daemon has answered a pairing code with a device token. Swap the
    /// spent code for the durable credential and keep everything else.
    func adopt(deviceToken: String, from endpoint: DaemonEndpoint) {
        var durable = endpoint
        durable.credential = .token(deviceToken)
        save(durable)
    }

    /// Remember the transport that actually completed a handshake, so the next
    /// launch starts on it instead of paying for the fallback ladder again.
    func noteTransport(useTLS: Bool) {
        guard var endpoint, endpoint.useTLS != useTLS, !endpoint.isPairingCode else { return }
        endpoint.useTLS = useTLS
        save(endpoint)
    }

    /// Record that the daemon has a certificate this pairing can never use.
    ///
    /// A `tailscale cert` certificate carries a DNS SAN, so a pairing made
    /// against `100.x.y.z` cannot validate it. The fix is the user's — re-pair
    /// from a QR carrying the MagicDNS name — so it is stated rather than
    /// silently endured.
    func noteTLSUnusable(_ unusable: Bool) {
        guard let endpoint else { return }
        tlsUnavailableReason =
            unusable
            ? "This daemon serves TLS, but the pairing points at the address \(endpoint.host) and a `tailscale cert` certificate only covers the MagicDNS name. Scan a fresh QR code to move onto the encrypted connection."
            : nil
    }

    func clear() {
        Keychain.delete(account: Self.account)
        endpoint = nil
        lastError = nil
        tlsUnavailableReason = nil
    }
}
