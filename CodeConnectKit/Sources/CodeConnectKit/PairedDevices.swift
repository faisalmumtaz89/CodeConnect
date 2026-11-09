import Foundation

// The devices paired with a computer's own daemon, as its local socket lists them (`codeconnect devices`), the
// requests that block one and allow one again, and the words a device list says. The Mac asks its own daemon;
// fixtures/wire/devices.json is the daemon's own frames.

/// A device paired with this computer (`DeviceSummary`): a phone, or another computer, by one pairing.
public struct PairedDevice: Decodable, Hashable, Sendable, Identifiable {
    public let deviceID: String
    public let name: String
    public let createdAt: String
    public let lastSeenAt: String?
    public let revokedAt: String?
    /// The Tailscale device it last connected from. Pairings that share one are one phone or one computer, and
    /// blocking any of them blocks them all.
    public let tailnetNode: String?
    /// Paired before the daemon recorded Tailscale devices.
    public let pairedBeforeNodes: Bool

    public var id: String { deviceID }
    public var isBlocked: Bool { revokedAt != nil }
    /// Blocked with no Tailscale device to name: while it is listed, the daemon asks every new device for a code.
    public var holdsNewDevices: Bool { isBlocked && tailnetNode == nil && pairedBeforeNodes }

    enum CodingKeys: String, CodingKey {
        case deviceID = "device_id"
        case name
        case createdAt = "created_at"
        case lastSeenAt = "last_seen_at"
        case revokedAt = "revoked_at"
        case tailnetNode = "tailnet_node"
        case pairedBeforeNodes = "paired_before_nodes"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        deviceID = try c.decode(String.self, forKey: .deviceID)
        name = try c.decode(String.self, forKey: .name)
        createdAt = try c.decode(String.self, forKey: .createdAt)
        lastSeenAt = try c.decodeIfPresent(String.self, forKey: .lastSeenAt)
        revokedAt = try c.decodeIfPresent(String.self, forKey: .revokedAt)
        tailnetNode = try c.decodeIfPresent(String.self, forKey: .tailnetNode)
        // Sent only when true.
        pairedBeforeNodes = try c.decodeIfPresent(Bool.self, forKey: .pairedBeforeNodes) ?? false
    }
}

/// One phone or one computer: the pairings that share a Tailscale device, as blocking treats them, or one pairing
/// with none, alone. Allowed and blocked pairings are never one group.
public struct DeviceGroup: Hashable, Sendable, Identifiable {
    /// The most recently seen first.
    public let devices: [PairedDevice]

    public var id: String { devices[0].tailnetNode ?? devices[0].deviceID }
    public var name: String { devices[0].name }
    public var isBlocked: Bool { devices[0].isBlocked }
    public var holdsNewDevices: Bool { devices.contains(where: \.holdsNewDevices) }

    /// When this phone or computer last reached the computer.
    public var lastSeen: Date? { devices.compactMap { $0.lastSeenAt.flatMap(ISO8601.parse) }.max() }
    public var blockedAt: Date? { devices.compactMap { $0.revokedAt.flatMap(ISO8601.parse) }.max() }

    /// The allowed groups, then the blocked ones, each most recently seen first.
    public static func groups(_ devices: [PairedDevice]) -> (allowed: [DeviceGroup], blocked: [DeviceGroup]) {
        func grouped(_ devices: [PairedDevice]) -> [DeviceGroup] {
            var order: [String] = []
            var members: [String: [PairedDevice]] = [:]
            for device in devices {
                let key = device.tailnetNode ?? device.deviceID
                if members[key] == nil { order.append(key) }
                members[key, default: []].append(device)
            }
            return order.map { key in
                DeviceGroup(devices: members[key]!.sorted { seen($0) > seen($1) })
            }
            .sorted { ($0.lastSeen ?? .distantPast) > ($1.lastSeen ?? .distantPast) }
        }
        return (grouped(devices.filter { !$0.isBlocked }), grouped(devices.filter(\.isBlocked)))
    }

    private static func seen(_ device: PairedDevice) -> Date {
        device.lastSeenAt.flatMap(ISO8601.parse) ?? .distantPast
    }

    // MARK: Words

    public static let title = "This computer’s devices"
    public static let intro = "The phones and other computers that can reach this computer."
    public static let blockedTitle = "Blocked"
    public static let empty = "No phone or other computer has been added yet."
    public static let reading = "Reading this computer’s devices…"
    public static let unreachable = "This computer’s CodeConnect isn’t answering, so its devices can’t be shown."
    /// Under the app's own entry, which has no Block: blocking it would cut the app off from its own computer.
    public static let thisDevice = "This Mac"
    public static let blockAction = "Block"
    public static let allowAction = "Allow again"

    /// Under an allowed device: when it last reached this computer, as of `now`.
    public func seenLine(now: Date = Date()) -> String {
        guard let lastSeen else { return "Not seen yet" }
        return "Last seen \(Format.age(since: lastSeen, now: now)) ago"
    }

    /// Under a blocked device, as of `now`, and, for one blocked by an older CodeConnect, what that does to new
    /// devices.
    public func blockedLine(now: Date = Date()) -> String {
        holdsNewDevices
            ? "Blocked by an older CodeConnect. While it’s listed, any new device needs a code to join."
            : "Blocked \(Format.age(since: blockedAt, now: now)) ago"
    }

    public var blockQuestion: String { "Block \(name)?" }
    public var blockSentence: String { "Blocks \(name) and its messages to this computer." }
    public var allowQuestion: String { "Allow \(name) again?" }
    public var allowSentence: String { "\(name) can reach this computer again." }
}

/// What a device list asks this computer's daemon, one JSON line each (`ClientFrame`).
public enum DeviceRequest: Encodable, Sendable {
    case list
    /// Blocks this pairing, and every pairing of the same Tailscale device.
    case block(deviceID: String)
    /// Lets a blocked pairing back in, and every blocked one of the same Tailscale device, each with its own token.
    case allow(deviceID: String)

    enum CodingKeys: String, CodingKey {
        case type, device
        case sshOnly = "ssh_only"
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .list:
            try c.encode("list_devices", forKey: .type)
        case .block(let id):
            try c.encode("revoke_device", forKey: .type)
            try c.encode(id, forKey: .device)
            try c.encode(false, forKey: .sshOnly)
        case .allow(let id):
            try c.encode("unblock_device", forKey: .type)
            try c.encode(id, forKey: .device)
        }
    }
}

/// The daemon's answer to a `DeviceRequest`.
public enum DeviceReply: Decodable, Sendable, Equatable {
    case devices([PairedDevice])
    case blocked(PairedDevice)
    case allowed(PairedDevice)
    /// A refusal, in the daemon's words.
    case refused(String)

    enum CodingKeys: String, CodingKey {
        case type, devices, device, message
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "devices": self = .devices(try c.decode([PairedDevice].self, forKey: .devices))
        case "revoked": self = .blocked(try c.decode(PairedDevice.self, forKey: .device))
        case "unblocked": self = .allowed(try c.decode(PairedDevice.self, forKey: .device))
        case "error": self = .refused(try c.decode(String.self, forKey: .message))
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .type, in: c, debugDescription: "not a device answer: \(other)")
        }
    }
}
