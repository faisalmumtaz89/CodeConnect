import CryptoKit
import Foundation
import UIKit

/// Stable per-install identity. Used for the daemon's client log, for the SSH
/// key comment, and for nothing that needs to be secret.
enum DeviceIdentity {
    private static let key = "codeconnect.installationID"

    static let installationID: String = {
        if let existing = UserDefaults.standard.string(forKey: key) { return existing }
        let fresh = UUID().uuidString
        UserDefaults.standard.set(fresh, forKey: key)
        return fresh
    }()

    /// Short, stable, and safe to paste into `authorized_keys`: the comment has
    /// to survive being read by a human on the Mac who is deciding whether to
    /// revoke it, so it names the device and pins the install.
    @MainActor
    static var sshKeyComment: String {
        let device = UIDevice.current.name
            .replacingOccurrences(of: " ", with: "-")
            .filter { $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" }
        let short = installationID.replacingOccurrences(of: "-", with: "").prefix(8).lowercased()
        return "codeconnect-\(device.isEmpty ? "iphone" : device)-\(short)"
    }
}

/// The app's own SSH key pair.
///
/// Generated once, on first use, and never leaves the device: the private half
/// lives in the Keychain with `ThisDeviceOnly` accessibility, so it is excluded
/// from encrypted backups and iCloud Keychain. Only the *public* half is ever
/// transmitted, and even then the daemon files it in `authorized_keys` only when
/// its operator ran `codeconnect pair --ssh` — consent happens at the Mac's terminal, not
/// here.
///
/// Encoding is done by hand rather than borrowed from the SSH library, for two
/// reasons: the public-key wire format is eleven lines of well-specified
/// framing (RFC 4253 section 6.6), and keeping it here means the key can be
/// generated, displayed and unit-tested without the transport stack being
/// involved at all.
struct SSHIdentity: Sendable, Hashable {
    /// 32-byte Ed25519 seed. Never displayed, never logged, never sent.
    let privateKeyRaw: Data
    /// 32-byte Ed25519 public key.
    let publicKeyRaw: Data
    /// The trailing comment written into `authorized_keys`.
    let comment: String

    static let algorithm = "ssh-ed25519"

    var signingKey: Curve25519.Signing.PrivateKey? {
        try? Curve25519.Signing.PrivateKey(rawRepresentation: privateKeyRaw)
    }

    /// `ssh-ed25519 AAAAC3Nz… comment` — exactly what goes in `authorized_keys`.
    var openSSHPublicKey: String {
        "\(Self.algorithm) \(base64Blob) \(comment)"
    }

    /// The base64 body only, without algorithm or comment.
    var base64Blob: String {
        Self.wireBlob(publicKeyRaw).base64EncodedString()
    }

    /// `SHA256:…` as `ssh-keygen -l` prints it: base64 of the SHA-256 of the
    /// wire blob, with padding stripped. Shown next to the key so the value on
    /// the phone can be compared with the value on the Mac by eye.
    var fingerprint: String {
        Self.fingerprint(ofWireBlob: Self.wireBlob(publicKeyRaw))
    }

    // MARK: - Wire format

    /// RFC 4253 section 6.6: `string(algorithm) || string(key)`, where `string`
    /// is a 32-bit big-endian length followed by that many bytes.
    static func wireBlob(_ publicKeyRaw: Data) -> Data {
        var blob = Data()
        blob.append(sshString(Data(algorithm.utf8)))
        blob.append(sshString(publicKeyRaw))
        return blob
    }

    private static func sshString(_ bytes: Data) -> Data {
        var out = Data()
        var length = UInt32(bytes.count).bigEndian
        withUnsafeBytes(of: &length) { out.append(contentsOf: $0) }
        out.append(bytes)
        return out
    }

    static func fingerprint(ofWireBlob blob: Data) -> String {
        let digest = SHA256.hash(data: blob)
        let base64 = Data(digest).base64EncodedString()
        return "SHA256:" + base64.replacingOccurrences(of: "=", with: "")
    }
}

// MARK: - Store

/// Owns the app's SSH key pair, creating it on first use.
@MainActor
enum SSHIdentityStore {
    private static let account = "ssh-ed25519"

    private struct Stored: Codable {
        var privateKey: Data
        var comment: String
    }

    private static var cached: SSHIdentity?

    /// The identity, generating it if this is the first ask.
    ///
    /// Returns nil only when the Keychain refuses to hold the key, which is a
    /// real failure worth surfacing rather than papering over with an
    /// in-memory key that would stop working at the next launch — and would
    /// mean the pubkey shown in Settings is not the one that authenticates.
    static func identity() -> SSHIdentity? {
        if let cached { return cached }
        if let existing = load() {
            cached = existing
            return existing
        }
        return generate()
    }

    /// Present without creating one. Used by anything that must not have the
    /// side effect of minting a key (the pairing hello for a daemon that will
    /// not use it, for instance).
    static func existingIdentity() -> SSHIdentity? {
        if let cached { return cached }
        cached = load()
        return cached
    }

    private static func load() -> SSHIdentity? {
        guard let data = Keychain.load(account: account),
            let stored = try? JSONDecoder().decode(Stored.self, from: data),
            let key = try? Curve25519.Signing.PrivateKey(rawRepresentation: stored.privateKey)
        else { return nil }
        return SSHIdentity(
            privateKeyRaw: stored.privateKey,
            publicKeyRaw: key.publicKey.rawRepresentation,
            comment: stored.comment)
    }

    private static func generate() -> SSHIdentity? {
        let key = Curve25519.Signing.PrivateKey()
        let stored = Stored(
            privateKey: key.rawRepresentation, comment: DeviceIdentity.sshKeyComment)
        guard let data = try? JSONEncoder().encode(stored),
            (try? Keychain.save(data, account: account)) != nil
        else { return nil }
        let identity = SSHIdentity(
            privateKeyRaw: stored.privateKey,
            publicKeyRaw: key.publicKey.rawRepresentation,
            comment: stored.comment)
        cached = identity
        return identity
    }

    /// Forget the key pair. The copy in the Mac's `authorized_keys` is not this
    /// app's to remove — `codeconnect ssh-revoke <device>` is — so the UI that calls this
    /// has to say so.
    static func reset() {
        Keychain.delete(account: account)
        cached = nil
    }
}
