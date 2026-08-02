import Foundation
import NIOCore
import NIOPosix
import NIOSSH

/// The live terminal half of the product: a real SSH session to the Mac,
/// attached to the private tmux server the agent is actually running in.
///
/// This is deliberately *not* the semantic surface. Everything the app knows
/// about state comes from the daemon's event log; this is the escape hatch where
/// the phone becomes the Mac's keyboard. Bytes are never parsed for meaning —
/// they go to the terminal emulator and nowhere else, so nothing scraped off
/// this screen can ever become a fact the rest of the app believes.
///
/// Three properties are worth not breaking:
///
///   * **Host keys are pinned on first use.** Accepting any key, as NIO's own
///     sample client does, would let anything that can answer on the tailnet
///     address read every keystroke. A changed key is a hard stop, not a
///     warning.
///   * **The attach command is built from validated input only.** The session id
///     comes from the daemon, and a session id is not a place to find a shell
///     metacharacter. It is checked against an allowlist anyway.
///   * **Disconnected is always visible.** A terminal showing the last thing it
///     received is indistinguishable from a live one, which is the exact lie
///     this rule exists to forbid — so the view gets an explicit not-live state
///     and this class never pretends a dead channel is open.
@MainActor
@Observable
final class SSHTerminalSession {
    enum Phase: Sendable, Equatable {
        case idle
        /// Checking whether anything is listening before dialling.
        case probing
        /// Nothing is listening; the user has to switch something on at the Mac.
        case needsSetup(SSHSetupGuidance)
        case connecting
        case authenticating
        case attached(since: Date)
        /// The server's key is not the one pinned for this host. Hard stop.
        case hostKeyChanged(HostKeyChange)
        /// The connection ended. Carries why, because "it stopped" is not a
        /// reason anyone can act on.
        case ended(reason: String, wasAttached: Bool)

        var isAttached: Bool {
            if case .attached = self { return true }
            return false
        }

        var isBusy: Bool {
            switch self {
            case .probing, .connecting, .authenticating: return true
            default: return false
            }
        }
    }

    struct HostKeyChange: Sendable, Equatable {
        var host: String
        var port: Int
        var pinned: String
        var offered: String
        var pinnedAt: Date
    }

    // MARK: Observable state

    private(set) var phase: Phase = .idle
    /// Rows and columns the emulator last reported, so a reconnect starts at the
    /// right size instead of at 80×24.
    private(set) var lastSize: (cols: Int, rows: Int) = (80, 24)
    /// Everything received, so a reconnect or a view rebuild can replay what is
    /// already on screen instead of showing an empty terminal.
    private(set) var transcript: [UInt8] = []
    /// Set once per connection when the host key is pinned for the first time.
    private(set) var firstUseFingerprint: String?

    /// Bytes arriving from the Mac. Set by the terminal view.
    var onOutput: ((ArraySlice<UInt8>) -> Void)?

    // MARK: Configuration

    struct Target: Sendable, Equatable {
        var host: String
        var port: Int
        var username: String
        var sessionID: String
    }

    private(set) var target: Target?

    // MARK: Private

    private var group: MultiThreadedEventLoopGroup?
    private var connection: Channel?
    private var child: Channel?
    private var connectTask: Task<Void, Never>?

    /// Bounded so a chatty agent cannot grow the transcript without limit. The
    /// emulator holds the scrollback that matters; this is only what a rebuilt
    /// view needs to look continuous.
    private static let maxTranscriptBytes = 256 * 1024
    private static let terminalType = "xterm-256color"

    // MARK: - Lifecycle

    /// Whether `connect` would do anything: used by foreground-reconnect so it
    /// cannot stack connection attempts.
    var canConnect: Bool {
        switch phase {
        case .idle, .ended, .needsSetup: return connectTask == nil
        case .hostKeyChanged: return false
        case .probing, .connecting, .authenticating, .attached: return false
        }
    }

    func connect(to target: Target) {
        guard connectTask == nil else { return }
        guard let command = Self.attachCommand(sessionID: target.sessionID) else {
            phase = .ended(
                reason:
                    "The session name \"\(target.sessionID)\" is not one this app will put in a command line.",
                wasAttached: false)
            return
        }
        guard let identity = SSHIdentityStore.identity() else {
            phase = .ended(
                reason:
                    "This iPhone could not create or read its SSH key. Without it there is nothing to authenticate with.",
                wasAttached: false)
            return
        }
        self.target = target
        firstUseFingerprint = nil

        connectTask = Task { [weak self] in
            await self?.run(target: target, command: command, identity: identity)
            self?.connectTask = nil
        }
    }

    /// Close everything and say why. Safe to call from any state.
    func disconnect(reason: String = "Disconnected.") {
        connectTask?.cancel()
        connectTask = nil
        let wasAttached = phase.isAttached
        teardown()
        if case .hostKeyChanged = phase { return }
        if case .needsSetup = phase { return }
        phase = .ended(reason: reason, wasAttached: wasAttached)
    }

    /// Drop the pin for this host so the next connection can pin afresh.
    /// Deliberately explicit and deliberately destructive-sounding: the only
    /// legitimate reason is that the *user* knows the Mac's key changed.
    func trustNewHostKey() {
        guard case .hostKeyChanged(let change) = phase else { return }
        KnownHostKeys.forget(host: change.host, port: change.port)
        phase = .idle
    }

    // MARK: - Terminal I/O

    /// Keystrokes from the emulator.
    ///
    /// A bare `ByteBuffer` goes down the pipeline and `TerminalDataHandler`
    /// wraps it in `SSHChannelData` on the event loop. `SSHChannelData` is
    /// explicitly *not* `Sendable` — it holds `IOData` — so keeping it entirely
    /// inside the loop is what lets this send from the main actor without
    /// smuggling a non-Sendable value across an isolation boundary.
    func send(_ bytes: ArraySlice<UInt8>) {
        guard let child, phase.isAttached, !bytes.isEmpty else { return }
        var buffer = child.allocator.buffer(capacity: bytes.count)
        buffer.writeBytes(bytes)
        child.writeAndFlush(buffer, promise: nil)
    }

    func send(text: String) {
        send(ArraySlice(Array(text.utf8)))
    }

    /// The emulator resized. Tell the far end, or the agent's TUI will draw for
    /// the wrong window.
    func resize(cols: Int, rows: Int) {
        guard cols > 0, rows > 0 else { return }
        guard lastSize.cols != cols || lastSize.rows != rows else { return }
        lastSize = (cols, rows)
        guard let child, phase.isAttached else { return }
        let request = SSHChannelRequestEvent.WindowChangeRequest(
            terminalCharacterWidth: cols, terminalRowHeight: rows,
            terminalPixelWidth: 0, terminalPixelHeight: 0)
        child.triggerUserOutboundEvent(request, promise: nil)
    }

    // MARK: - Connection

    private func run(target: Target, command: String, identity: SSHIdentity) async {
        phase = .probing
        let reachability = await SSHReachability.probe(host: target.host, port: target.port)
        if Task.isCancelled { return }
        if !reachability.isReachable {
            phase = .needsSetup(
                SSHSetupGuidance.forOutcome(reachability, host: target.host, port: target.port)
                    ?? SSHSetupGuidance(
                        title: "Cannot reach \(target.host)", detail: "", steps: []))
            return
        }

        guard let signingKey = identity.signingKey else {
            phase = .ended(reason: "This iPhone's SSH key could not be read.", wasAttached: false)
            return
        }

        phase = .connecting
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        self.group = group

        // Everything crossing into the event loop below is either a value type
        // or a `@Sendable` closure. Nothing that is confined to the main actor
        // is captured.
        let host = target.host
        let port = target.port
        let username = target.username
        let pinned = KnownHostKeys.pin(host: host, port: port)
        let verdicts = HostKeyVerdictBox()
        let auth = PrivateKeyAuthDelegate(
            username: username, privateKey: NIOSSHPrivateKey(ed25519Key: signingKey))
        let hostKeys = PinningHostKeyDelegate(pinnedFingerprint: pinned?.fingerprint, box: verdicts)
        let output = OutputBox()

        // The session channel is requested inside the pipeline initialiser and
        // its promise is fulfilled once authentication completes — `NIOSSHHandler`
        // queues pending channel creations until then. Doing it here rather than
        // fishing the handler back out after `connect` means neither
        // non-`Sendable` participant (the SSH handler, the terminal handler) ever
        // crosses an isolation boundary; only the resulting `Channel`, which is
        // `Sendable`, comes back out.
        let size = lastSize
        let childPromise = group.next().makePromise(of: Channel.self)

        do {
            let bootstrap = ClientBootstrap(group: group)
                .channelInitializer { channel in
                    channel.eventLoop.makeCompletedFuture {
                        let ssh = NIOSSHHandler(
                            role: .client(
                                .init(userAuthDelegate: auth, serverAuthDelegate: hostKeys)),
                            allocator: channel.allocator,
                            inboundChildChannelInitializer: nil)
                        try channel.pipeline.syncOperations.addHandler(ssh)
                        ssh.createChannel(childPromise, channelType: .session) { child, type in
                            guard type == .session else {
                                return child.eventLoop.makeFailedFuture(
                                    SSHTerminalError.wrongChannel)
                            }
                            return child.eventLoop.makeCompletedFuture {
                                try child.pipeline.syncOperations.addHandler(
                                    TerminalDataHandler(output: output))
                            }
                        }
                    }
                }
                .channelOption(ChannelOptions.socketOption(.so_reuseaddr), value: 1)
                .channelOption(ChannelOptions.socketOption(.tcp_nodelay), value: 1)
                .connectTimeout(.seconds(15))

            let connection = try await bootstrap.connect(host: host, port: port).get()
            if Task.isCancelled {
                try? await connection.close()
                teardown()
                return
            }
            self.connection = connection
            phase = .authenticating

            // Race the session against the connection dying. Without this, a
            // rejected key closes the TCP channel and the promise above is
            // simply never fulfilled — the UI would sit on "authenticating"
            // forever with the real reason already known and thrown away.
            let child = try await withThrowingTaskGroup(of: Channel.self) { racers in
                racers.addTask { try await childPromise.futureResult.get() }
                racers.addTask {
                    try await connection.closeFuture.get()
                    throw SSHTerminalError.noAcceptableAuthMethod
                }
                guard let first = try await racers.next() else {
                    throw SSHTerminalError.wrongChannel
                }
                racers.cancelAll()
                return first
            }
            self.child = child

            try await child.triggerUserOutboundEvent(
                SSHChannelRequestEvent.PseudoTerminalRequest(
                    wantReply: true,
                    term: Self.terminalType,
                    terminalCharacterWidth: size.cols,
                    terminalRowHeight: size.rows,
                    terminalPixelWidth: 0,
                    terminalPixelHeight: 0,
                    terminalModes: SSHTerminalModes([:]))
            ).get()

            try await child.triggerUserOutboundEvent(
                SSHChannelRequestEvent.ExecRequest(command: command, wantReply: true)
            ).get()

            if let firstUse = verdicts.firstUseFingerprint {
                KnownHostKeys.remember(fingerprint: firstUse, host: host, port: port)
                firstUseFingerprint = firstUse
            }

            phase = .attached(since: Date())
            await pump(output: output, child: child)
        } catch {
            handle(failure: error, verdicts: verdicts, host: host, port: port, pinned: pinned)
        }
    }

    /// Deliver bytes to the emulator until the channel closes.
    ///
    /// A stream rather than a callback into the main actor from the event loop:
    /// the buffering lives in one place, and cancelling the task is the same
    /// thing as stopping delivery.
    private func pump(output: OutputBox, child: Channel) async {
        for await chunk in output.stream {
            if Task.isCancelled { break }
            append(chunk)
            onOutput?(ArraySlice(chunk))
        }
        // The stream finishes when the channel closes.
        let reason = output.closeReason ?? "The Mac closed the session."
        if !Task.isCancelled {
            let wasAttached = phase.isAttached
            teardown()
            phase = .ended(reason: reason, wasAttached: wasAttached)
        }
    }

    private func append(_ chunk: [UInt8]) {
        transcript.append(contentsOf: chunk)
        if transcript.count > Self.maxTranscriptBytes {
            transcript.removeFirst(transcript.count - Self.maxTranscriptBytes)
        }
    }

    private func handle(
        failure: Error, verdicts: HostKeyVerdictBox, host: String, port: Int,
        pinned: KnownHostKeys.Pin?
    ) {
        teardown()
        if let offered = verdicts.rejectedFingerprint, let pinned {
            phase = .hostKeyChanged(
                HostKeyChange(
                    host: host, port: port, pinned: pinned.fingerprint, offered: offered,
                    pinnedAt: pinned.addedAt))
            return
        }
        phase = .ended(reason: Self.describe(failure), wasAttached: false)
    }

    private func teardown() {
        child?.close(promise: nil)
        child = nil
        connection?.close(promise: nil)
        connection = nil
        if let group {
            // Shutting the loop down asynchronously: the channels above have
            // already been asked to close, and a synchronous shutdown from the
            // main actor would block the UI on network teardown.
            group.shutdownGracefully { _ in }
        }
        group = nil
    }

    // MARK: - Command construction

    /// `tmux -L codeconnect attach -t =<session>`.
    ///
    /// `=` pins the target to an exact name; without it tmux prefix-matches and
    /// `cc-1` can route to `cc-12`. `attach-session` is one of the subcommands
    /// that takes a bare `=name` rather than the `=name:` pane form.
    ///
    /// Returns nil for any session id that is not a plain identifier — the value
    /// arrives from the daemon and there is no version of "session name" that
    /// legitimately contains a shell metacharacter.
    nonisolated static func attachCommand(sessionID: String) -> String? {
        guard !sessionID.isEmpty, sessionID.count <= 64 else { return nil }
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_."))
        guard sessionID.unicodeScalars.allSatisfy({ allowed.contains($0) }) else { return nil }
        guard sessionID != ".", sessionID != ".." else { return nil }
        return "tmux -L codeconnect attach -t =\(sessionID)"
    }

    private static func describe(_ error: Error) -> String {
        if error is SSHTerminalError { return (error as! SSHTerminalError).description }
        let text = String(describing: error)
        if text.contains("authenticationFailed") || text.contains("allAuthenticationOptionsFailed")
        {
            return
                "The Mac refused this iPhone's key. Run `codeconnect pair --ssh` at the Mac and scan the QR it prints, only that adds this phone to authorized_keys."
        }
        if text.contains("connectTimeout") || text.contains("connectionTimeout") {
            return "The Mac did not answer in time."
        }
        return (error as NSError).localizedDescription
    }
}

// MARK: - Errors

enum SSHTerminalError: Error, CustomStringConvertible {
    case wrongChannel
    case hostKeyRejected
    case noAcceptableAuthMethod

    var description: String {
        switch self {
        case .wrongChannel:
            return "The Mac opened the wrong kind of SSH channel."
        case .hostKeyRejected:
            return "The Mac's SSH host key is not the one this iPhone pinned."
        case .noAcceptableAuthMethod:
            return
                "The Mac would not accept this iPhone's key. Run `codeconnect pair --ssh` at the Mac and scan the QR it prints, only that adds this phone to authorized_keys."
        }
    }
}

// MARK: - Event-loop side

/// Buffers bytes from the event loop and hands them to the main actor as an
/// `AsyncStream`.
///
/// `@unchecked Sendable` with an explicit lock: NIO delivers on its own loop and
/// the consumer is the main actor, so the two really do touch this from
/// different threads. The lock covers every mutable field.
private final class OutputBox: @unchecked Sendable {
    private let lock = NSLock()
    private var _closeReason: String?
    private let continuation: AsyncStream<[UInt8]>.Continuation
    let stream: AsyncStream<[UInt8]>

    init() {
        var escaped: AsyncStream<[UInt8]>.Continuation!
        stream = AsyncStream(bufferingPolicy: .unbounded) { escaped = $0 }
        continuation = escaped
    }

    var closeReason: String? {
        lock.lock()
        defer { lock.unlock() }
        return _closeReason
    }

    func deliver(_ bytes: [UInt8]) {
        continuation.yield(bytes)
    }

    func finish(reason: String?) {
        lock.lock()
        if _closeReason == nil { _closeReason = reason }
        lock.unlock()
        continuation.finish()
    }
}

/// Moves bytes between the SSH child channel and the emulator. Never interprets
/// them: parsing terminal bytes for semantics is banned by the architecture, and
/// this is the one place it would be tempting.
private final class TerminalDataHandler: ChannelDuplexHandler {
    typealias InboundIn = SSHChannelData
    /// Keystrokes arrive as a plain buffer and are wrapped here, on the event
    /// loop, so the non-`Sendable` `SSHChannelData` never leaves it.
    typealias OutboundIn = ByteBuffer
    typealias OutboundOut = SSHChannelData

    private let output: OutputBox
    private var exitStatus: Int?

    init(output: OutputBox) {
        self.output = output
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let channelData = unwrapInboundIn(data)
        guard case .byteBuffer(let buffer) = channelData.data else { return }
        // stdout and stderr both belong on the screen: this is a terminal, and
        // tmux writes diagnostics to stderr.
        output.deliver(Array(buffer.readableBytesView))
    }

    func write(context: ChannelHandlerContext, data: NIOAny, promise: EventLoopPromise<Void>?) {
        let buffer = unwrapOutboundIn(data)
        context.write(
            wrapOutboundOut(SSHChannelData(type: .channel, data: .byteBuffer(buffer))),
            promise: promise)
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if let status = event as? SSHChannelRequestEvent.ExitStatus {
            exitStatus = status.exitStatus
        }
        context.fireUserInboundEventTriggered(event)
    }

    func channelInactive(context: ChannelHandlerContext) {
        output.finish(reason: closeReason)
        context.fireChannelInactive()
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        output.finish(reason: (error as NSError).localizedDescription)
        context.close(promise: nil)
    }

    private var closeReason: String {
        switch exitStatus {
        case .none:
            return "The Mac closed the session."
        case .some(0):
            return "tmux detached. The agent is still running on the Mac."
        case .some(let code):
            return
                "tmux exited with status \(code). If the session has ended, `codeconnect attach` on the Mac will say so."
        }
    }
}

/// Offers exactly one credential: this app's Ed25519 key.
///
/// No password fallback. There is nothing to type a password into on this
/// screen, and offering one would invite a phishing prompt from anything that
/// can answer on the address.
private final class PrivateKeyAuthDelegate: NIOSSHClientUserAuthenticationDelegate, @unchecked
    Sendable
{
    private let username: String
    private let privateKey: NIOSSHPrivateKey
    private var offered = false

    init(username: String, privateKey: NIOSSHPrivateKey) {
        self.username = username
        self.privateKey = privateKey
    }

    func nextAuthenticationType(
        availableMethods: NIOSSHAvailableUserAuthenticationMethods,
        nextChallengePromise: EventLoopPromise<NIOSSHUserAuthenticationOffer?>
    ) {
        guard availableMethods.contains(.publicKey), !offered else {
            // Failing the promise is how NIOSSH is told there is nothing left to
            // try; it turns into an authentication failure the UI can explain.
            nextChallengePromise.fail(SSHTerminalError.noAcceptableAuthMethod)
            return
        }
        offered = true
        nextChallengePromise.succeed(
            NIOSSHUserAuthenticationOffer(
                username: username, serviceName: "", offer: .privateKey(.init(privateKey: privateKey))
            ))
    }
}

/// Records what the host-key check decided, for the main actor to read after the
/// connection succeeds or fails.
private final class HostKeyVerdictBox: @unchecked Sendable {
    private let lock = NSLock()
    private var _firstUse: String?
    private var _rejected: String?

    var firstUseFingerprint: String? {
        lock.lock()
        defer { lock.unlock() }
        return _firstUse
    }

    var rejectedFingerprint: String? {
        lock.lock()
        defer { lock.unlock() }
        return _rejected
    }

    func noteFirstUse(_ fingerprint: String) {
        lock.lock()
        _firstUse = fingerprint
        lock.unlock()
    }

    func noteRejected(_ fingerprint: String) {
        lock.lock()
        _rejected = fingerprint
        lock.unlock()
    }
}

/// Trust-on-first-use host key validation.
private final class PinningHostKeyDelegate: NIOSSHClientServerAuthenticationDelegate, @unchecked
    Sendable
{
    private let pinnedFingerprint: String?
    private let box: HostKeyVerdictBox

    init(pinnedFingerprint: String?, box: HostKeyVerdictBox) {
        self.pinnedFingerprint = pinnedFingerprint
        self.box = box
    }

    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>)
    {
        let fingerprint = Self.fingerprint(of: hostKey)
        guard let pinnedFingerprint else {
            box.noteFirstUse(fingerprint)
            validationCompletePromise.succeed(())
            return
        }
        if pinnedFingerprint == fingerprint {
            validationCompletePromise.succeed(())
        } else {
            box.noteRejected(fingerprint)
            validationCompletePromise.fail(SSHTerminalError.hostKeyRejected)
        }
    }

    /// `SHA256:…`, matching what `ssh-keygen -lf` prints for the same key, so
    /// the value on the phone can be compared with the value on the Mac by eye.
    ///
    /// `String(openSSHPublicKey:)` renders `"<algorithm> <base64 blob>"`; the
    /// fingerprint is the SHA-256 of that blob's bytes.
    static func fingerprint(of key: NIOSSHPublicKey) -> String {
        let openSSH = String(openSSHPublicKey: key)
        let parts = openSSH.split(separator: " ")
        guard parts.count >= 2, let blob = Data(base64Encoded: String(parts[1])) else {
            // Cannot happen for a key NIOSSH just parsed, but a fingerprint that
            // silently became a constant would pin nothing at all — so make it
            // impossible to match instead.
            return "SHA256:unreadable-\(UUID().uuidString)"
        }
        return SSHIdentity.fingerprint(ofWireBlob: blob)
    }
}
