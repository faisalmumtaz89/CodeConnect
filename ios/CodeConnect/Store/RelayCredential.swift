import Foundation

/// The one relay binding this phone holds.
///
/// It is the App Attest key identity and the bearer credential the relay minted
/// for exactly one `(token, environment)`. There is never more than one: a phone
/// has a single current APNs token, and the plan issues a single shared
/// credential for it (§Decision 1). A new token or a new credential replaces the
/// whole record; the daemon and relay revoke the previous binding atomically.
///
/// `keyID` is the durable half — an App Attest key survives app updates and is
/// reused to *assert* a rotation or rebind, so losing the credential does not
/// cost the key. Losing the whole record (reinstall, Keychain loss) does cost
/// the key, which is exactly the case the plan routes to fresh attestation.
struct RelayCredential: Codable, Equatable, Sendable {
    /// The App Attest key id from `generateKey` — base64, ~44 chars. Durable.
    let keyID: String
    /// Lowercase-hex APNs token this credential is bound to. A different token is
    /// a different binding and cannot reuse this credential.
    let token: String
    /// `"sandbox"` or `"production"`. The relay binding is the authority (§4);
    /// this field tracks the daemon's `hello_ack.push_environment` correction so
    /// the app stops resending a stale value.
    let environment: String
    /// The relay bearer — base64url, 43 chars. Sent to the daemon in
    /// `RegisterPush.relay_credential` and to the relay's status endpoint as a
    /// bearer. A secret; never logged.
    let credential: String
    /// The generation floor the bearer was minted under. A relay database restore
    /// bumps the floor and refuses every older bearer, which the app learns as
    /// `reenroll`. Carried so a future check can compare without another round
    /// trip.
    let generation: Int
    /// The install this binding was minted under (`DeviceIdentity.installationID`).
    /// A Keychain item can outlive an app delete while the App Attest private key
    /// and `UserDefaults` do not, so a reinstall regenerates the install id and
    /// this no longer matches — which is how the app detects that a surviving
    /// credential has lost its key and must be freshly attested (§Decision 1).
    let installID: String

    /// The same binding with a corrected environment — the daemon→app
    /// propagation path. Returns `self` unchanged when the environment already
    /// matches, so a caller can persist unconditionally without a needless write.
    func adoptingEnvironment(_ corrected: String) -> RelayCredential {
        guard corrected != environment else { return self }
        return RelayCredential(
            keyID: keyID, token: token, environment: corrected,
            credential: credential, generation: generation, installID: installID)
    }
}

/// The bearer is a secret, so a `RelayCredential` never prints one. Both hooks
/// are overridden because `String(describing:)` uses one and the debugger the
/// other, and the plan requires the credential to be absent from `Debug` output
/// (§Decision 5 stored-secrets table).
extension RelayCredential: CustomStringConvertible, CustomDebugStringConvertible {
    var description: String {
        "RelayCredential(token: …\(token.suffix(4)), environment: \(environment), "
            + "credential: <redacted>, generation: \(generation))"
    }
    var debugDescription: String { description }
}

/// Where the relay binding lives. A protocol so the enrollment flow can be driven
/// in a unit test against an in-memory store — the Keychain is unavailable to an
/// unsigned automation launch, and "Keychain loss" is itself an acceptance case
/// that a fake models directly.
protocol RelayCredentialStoring: Sendable {
    func load() -> RelayCredential?
    func save(_ credential: RelayCredential)
    func clear()
}

/// The production store: one `ThisDeviceOnly` Keychain item, JSON-encoded.
///
/// `ThisDeviceOnly` accessibility (inherited from `Keychain`) keeps the bearer
/// out of encrypted backups and iCloud Keychain, so a restored backup never
/// resurrects a credential the relay may have revoked — the app re-enrolls
/// instead, which is the behaviour the restore procedure depends on.
struct KeychainRelayCredentialStore: RelayCredentialStoring {
    /// Distinct from the pairing account so clearing one never touches the other.
    private static let account = "relay-credential"

    func load() -> RelayCredential? {
        guard let data = Keychain.load(account: Self.account) else { return nil }
        return try? JSONDecoder().decode(RelayCredential.self, from: data)
    }

    func save(_ credential: RelayCredential) {
        guard let data = try? JSONEncoder().encode(credential) else { return }
        try? Keychain.save(data, account: Self.account)
    }

    func clear() {
        Keychain.delete(account: Self.account)
    }
}
