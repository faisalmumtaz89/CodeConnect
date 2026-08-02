import Foundation
import Network

/// Is anything listening for SSH on the Mac?
///
/// Asked before a connection attempt so the terminal can tell the difference
/// between "the SSH server is switched off" — which needs a setup card and a
/// decision by the user — and "authentication failed", which needs a completely
/// different explanation. Guessing from a connect error would conflate them.
///
/// This *only* probes. It never enables anything: turning on Remote Login or
/// Tailscale SSH is a system-level change that belongs to the person at the Mac,
/// and an app that flipped it would be doing exactly what this product promises
/// it will not.
enum SSHReachability {
    enum Outcome: Sendable, Equatable {
        /// A TCP connection was accepted.
        case reachable(rtt: TimeInterval)
        /// The host answered and said no — nothing is listening on that port.
        case refused
        /// Nothing answered in time. Host asleep, off the tailnet, or filtered.
        case timedOut
        /// The name did not resolve, or the network refused to try.
        case failed(String)

        var isReachable: Bool {
            if case .reachable = self { return true }
            return false
        }
    }

    /// Probe `host:port`, giving up after `timeout`.
    ///
    /// Deliberately short by default: this runs before showing the terminal, and
    /// a Mac that is awake on the tailnet answers in milliseconds. A long wait
    /// here would be indistinguishable from a hung connect.
    static func probe(host: String, port: Int, timeout: Duration = .seconds(4)) async -> Outcome {
        guard let nwPort = NWEndpoint.Port(rawValue: UInt16(clamping: port)), !host.isEmpty else {
            return .failed("\(host):\(port) is not an address that can be dialled.")
        }

        let parameters = NWParameters.tcp
        // A half-open probe is enough to learn whether anything is listening,
        // and it avoids holding a connection the SSH server would have to time
        // out on its own.
        parameters.prohibitedInterfaceTypes = []
        let connection = NWConnection(
            host: NWEndpoint.Host(host), port: nwPort, using: parameters)
        let started = Date()

        return await withTaskGroup(of: Outcome.self) { group in
            group.addTask {
                await withCheckedContinuation { (continuation: CheckedContinuation<Outcome, Never>) in
                    // `NWConnection` can report several states; only the first
                    // one that settles the question may resume the continuation.
                    let settled = AtomicFlag()
                    connection.stateUpdateHandler = { state in
                        switch state {
                        case .ready:
                            if settled.take() {
                                continuation.resume(
                                    returning: .reachable(rtt: Date().timeIntervalSince(started)))
                            }
                        case .failed(let error):
                            if settled.take() {
                                continuation.resume(returning: classify(error))
                            }
                        case .cancelled:
                            if settled.take() { continuation.resume(returning: .timedOut) }
                        case .waiting(let error):
                            // "Waiting" means the path is not usable right now —
                            // connection refused surfaces here rather than as a
                            // failure, because NWConnection would keep retrying.
                            if settled.take() { continuation.resume(returning: classify(error)) }
                        case .setup, .preparing:
                            break
                        @unknown default:
                            break
                        }
                    }
                    connection.start(queue: .global(qos: .userInitiated))
                }
            }
            group.addTask {
                try? await Task.sleep(for: timeout)
                return .timedOut
            }
            let first = await group.next() ?? .timedOut
            group.cancelAll()
            connection.cancel()
            return first
        }
    }

    private static func classify(_ error: NWError) -> Outcome {
        if case .posix(let code) = error {
            switch code {
            case .ECONNREFUSED: return .refused
            case .ETIMEDOUT, .EHOSTUNREACH, .ENETUNREACH, .EHOSTDOWN: return .timedOut
            default: break
            }
        }
        if case .dns = error {
            return .failed("That hostname did not resolve. Is the Mac on the tailnet?")
        }
        return .failed(error.localizedDescription)
    }

    /// One-shot latch. `NWConnection` delivers state changes on its own queue,
    /// so "resume exactly once" has to hold across threads.
    private final class AtomicFlag: @unchecked Sendable {
        private let lock = NSLock()
        private var taken = false

        func take() -> Bool {
            lock.lock()
            defer { lock.unlock() }
            if taken { return false }
            taken = true
            return true
        }
    }
}

/// What the user has to do at the Mac before the terminal can work.
///
/// Rendered as a setup card with the exact commands. Neither option is
/// performed by this app.
struct SSHSetupGuidance: Sendable, Equatable {
    var title: String
    var detail: String
    var steps: [Step]

    struct Step: Sendable, Equatable, Identifiable {
        var id: String { title }
        var title: String
        var body: String
        /// A command the user can copy. Nil for "click here in System Settings".
        var command: String?
    }

    static func forOutcome(_ outcome: SSHReachability.Outcome, host: String, port: Int)
        -> SSHSetupGuidance?
    {
        switch outcome {
        case .reachable:
            return nil
        case .refused:
            return SSHSetupGuidance(
                title: "No SSH server on \(host)",
                detail:
                    "The Mac answered on port \(port) but nothing is listening. macOS ships with SSH switched off; turn on one of these at the Mac. CodeConnect will not enable a system service for you.",
                steps: standardSteps)
        case .timedOut:
            return SSHSetupGuidance(
                title: "\(host) did not answer on port \(port)",
                detail:
                    "Either the Mac is asleep or off the tailnet, or its SSH server is off. Check the daemon link first, if that is live, the Mac is reachable and this is the SSH server.",
                steps: standardSteps)
        case .failed(let reason):
            return SSHSetupGuidance(
                title: "Could not reach \(host)",
                detail: reason,
                steps: standardSteps)
        }
    }

    private static let standardSteps: [Step] = [
        Step(
            title: "Option 1 - Tailscale SSH",
            body:
                "Recommended: authentication and access control ride your tailnet ACLs, and there is no port exposed anywhere else. Run this on the Mac, then re-open this tab.",
            command: "tailscale up --ssh"),
        Step(
            title: "Option 2 - macOS Remote Login",
            body:
                "System Settings → General → Sharing → Remote Login. Limit access to your own user while you are there.",
            command: nil),
        Step(
            title: "Then authorise this iPhone",
            body:
                "Run this at the Mac and scan the QR it prints. Only `--ssh` lets the daemon add this phone's public key to authorized_keys, and it says so before it does.",
            command: "codeconnect pair --ssh"),
    ]
}
