import CodeConnectKit
import Darwin
import Foundation

/// This Mac's own daemon, asked over its local socket (`ccd.sock` under the CodeConnect folder): one JSON
/// line out, one back, the way `codeconnect` asks it (mac/codeconnect/src/daemon.rs): where it answers now, a
/// fresh pairing code (for this app or an iPhone), and the devices it has paired, which it can block and allow
/// again (Kit's `DeviceRequest`). Which daemon it is, it keeps in its folder.
enum LocalDaemon {
    struct Address: Equatable {
        let host: String
        let port: Int
        let tls: Bool
    }

    /// Where the daemon answers devices now (`daemon_info`): loopback before Tailscale, its tailnet
    /// name after. Nil when nothing answers the socket.
    static func address(root: URL) -> Address? {
        guard let reply = request(#"{"type":"daemon_info"}"#, root: root), reply["type"] as? String == "daemon",
            let host = reply["endpoint_host"] as? String, let port = reply["endpoint_port"] as? Int
        else { return nil }
        return Address(host: host, port: port, tls: reply["tls"] as? Bool ?? false)
    }

    /// A single-use code from the daemon itself (`create_pairing`, five minutes), with the host and port it
    /// gives for it: where it answers devices, the same three `codeconnect pair` puts in its QR (pair.rs).
    static func pairing(root: URL) -> PairingQRPayload? {
        let reply = request(#"{"type":"create_pairing","ttl_secs":300,"allow_ssh":false}"#, root: root)
        guard reply?["type"] as? String == "pairing", let code = reply?["code"] as? String,
            let host = reply?["host"] as? String, let port = reply?["port"] as? Int
        else { return nil }
        return PairingQRPayload(host: host, port: port, code: code)
    }

    /// The devices paired with the daemon (`list_devices`, as `codeconnect devices` lists them), by id: their
    /// names. Nil when nothing answers.
    static func devices(root: URL) -> [String: String]? {
        guard case .devices(let devices)? = ask(.list, root: root) else { return nil }
        return Dictionary(uniqueKeysWithValues: devices.map { ($0.deviceID, $0.name) })
    }

    /// A device request (list, block, allow again), answered in the daemon's words; nil when nothing answers.
    static func ask(_ request: DeviceRequest, root: URL) -> DeviceReply? {
        guard let line = try? JSONEncoder().encode(request), let reply = exchange(String(decoding: line, as: UTF8.self), root: root)
        else { return nil }
        return try? JSONDecoder().decode(DeviceReply.self, from: reply)
    }

    /// Whether the daemon answers its socket at all.
    static func answers(root: URL) -> Bool { address(root: root) != nil }

    /// Which daemon this Mac's is: the id it minted once in its folder (`machine_id`) and sends in every
    /// `hello_ack`, so a computer in the list is this Mac by that id, whatever address reached it. Nil before the
    /// daemon has first run.
    static func machineID(root: URL) -> String? {
        guard let text = try? String(contentsOf: root.appendingPathComponent("machine_id"), encoding: .utf8) else {
            return nil
        }
        let id = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return id.isEmpty ? nil : id
    }

    private static func request(_ line: String, root: URL) -> [String: Any]? {
        guard let reply = exchange(line, root: root) else { return nil }
        return try? JSONSerialization.jsonObject(with: reply) as? [String: Any]
    }

    /// One line out, and the one line the daemon answers, without its newline.
    private static func exchange(_ line: String, root: URL) -> Data? {
        let path = root.appendingPathComponent("ccd.sock").path
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        defer { close(fd) }
        var timeout = timeval(tv_sec: 5, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: address.sun_path) else { return nil }
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            raw.copyBytes(from: bytes)
        }
        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else { return nil }
        let out = Array((line + "\n").utf8)
        guard out.withUnsafeBytes({ write(fd, $0.baseAddress, out.count) }) == out.count else { return nil }
        var received = [UInt8]()
        var chunk = [UInt8](repeating: 0, count: 4096)
        while !received.contains(UInt8(ascii: "\n")) {
            let count = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
            guard count > 0 else { break }
            received.append(contentsOf: chunk[..<count])
        }
        guard let end = received.firstIndex(of: UInt8(ascii: "\n")) else { return nil }
        return Data(received[..<end])
    }
}
