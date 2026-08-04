import XCTest

@testable import CodeConnect

/// The two client-side rules that make an App-Store phone honest against a
/// Mac helper of any age.
///
/// The phone updates itself; the Mac updates only when a human runs the
/// installer — so a newer app against an older daemon is not an edge case,
/// it is the steady state the week after every release. Feature *minors*
/// degrade per surface by capability; these tests pin the two places that
/// used to overclaim instead.
@MainActor
final class UpdateCompatibilityTests: XCTestCase {

    /// Decoded from wire-shaped JSON, the way every real one arrives.
    private func capabilities(push: Bool) -> Capabilities {
        let json = """
            {"can_approve_reliably":true,"fail_mode":"fail_open",\
            "answer_path":"send_keys","hold_secs":0,"send_text":true,\
            "capture":true,"push":\(push),"tls":false}
            """
        return try! JSONDecoder().decode(Capabilities.self, from: Data(json.utf8))
    }

    private func ack(version: UInt32) -> HelloAck {
        HelloAck(
            protocolVersion: version,
            protocolMinor: 0,
            serverTime: "2026-08-04T00:00:00Z",
            capabilities: capabilities(push: false),
            deviceToken: nil,
            deviceID: nil,
            deviceName: nil,
            sshKeyInstalled: nil)
    }

    /// A legacy daemon that predates the server's own `protocol_mismatch`
    /// frame answers a mismatched `hello_ack` as if nothing were wrong. The
    /// client must refuse it as terminal — the app used to mark itself
    /// *connected* and merely record the mismatch as an error string, a
    /// broken link presented as a working one.
    func testAMismatchedMajorIsRefusedNotRecorded() {
        let refusal = DaemonConnection.incompatibility(of: ack(version: Wire.protocolVersion + 1))
        XCTAssertNotNil(refusal, "a newer major must be refused")
        XCTAssertTrue(
            refusal?.contains("Update CodeConnect on the Mac") == true,
            "the refusal names the fix: \(refusal ?? "nil")")

        XCTAssertNil(
            DaemonConnection.incompatibility(of: ack(version: Wire.protocolVersion)),
            "the matching major is never refused — minors degrade per surface, "
                + "they do not disconnect")
    }

    /// `enablePush` against a daemon that does not advertise push used to ask
    /// the user to authorise notifications nothing could ever send, then post
    /// a `register_push` the daemon does not understand. The capability is
    /// known by the time any caller runs (all run after `hello_ack`), so the
    /// gate is a fact, not a guess.
    func testPushRegistrationWaitsForTheCapabilityAndTheDeviceRow() {
        let model = AppModel()
        var requests = 0
        model.onPushRegistrationRequested = { requests += 1 }

        // No handshake yet: capabilities unknown, enablePush must do nothing.
        XCTAssertNil(model.connection.capabilities)
        model.enablePush()
        XCTAssertEqual(
            requests, 0,
            "no permission prompt may fire before a daemon advertises push")

        // A daemon without push: still nothing.
        model.connection.simulateCapabilitiesForTesting(capabilities(push: false), minor: 7)
        model.enablePush()
        XCTAssertEqual(requests, 0)

        // Push advertised but no device row — a static-token session. The
        // daemon refuses that registration outright (no revocation could
        // ever switch the stream off), so the prompt must not fire:
        // `capabilities.push` is server-global, not per-connection.
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: nil)
        model.enablePush()
        XCTAssertEqual(requests, 0, "no device row, no registration")

        // A paired device against a push-capable daemon: now, and only now.
        model.connection.simulateCapabilitiesForTesting(capabilities(push: true), minor: 7)
        model.enablePush()
        XCTAssertEqual(requests, 1, "an advertised capability is acted on")
    }

    /// `onConnected` fires per handshake, and during a pairing the token
    /// handler used to fire a second registration for the same handshake —
    /// stacked observers and duplicate workflows. One launch, one ask.
    func testThePermissionRequestHappensOncePerLaunchHoweverManyHandshakes() {
        let model = AppModel()
        var requests = 0
        model.onPushRegistrationRequested = { requests += 1 }

        model.connection.simulateCapabilitiesForTesting(capabilities(push: true), minor: 7)
        model.enablePush()  // first handshake
        model.enablePush()  // reconnect
        model.enablePush()  // pairing's own paths, historically
        XCTAssertEqual(requests, 1, "a flapping link must not re-ask per dial")
    }

    /// Requesting once is not delivering once. Apple's callback fires when
    /// Apple pleases; the device row that needs the token is whichever daemon
    /// is connected *now* — so a same-launch re-pair to another Mac must be
    /// handed the token Apple already issued, and a delivery that failed
    /// while the link flapped must go out again on the next handshake.
    /// Delivery decides inside its own task — the seam fires there — so
    /// tests yield to let queued attempts land before asserting.
    private func drainDeliveries() async {
        for _ in 0..<4 { await Task.yield() }
    }

    func testTheCachedTokenIsRedeliveredToEveryEligibleHandshake() async {
        let model = AppModel()
        var requests = 0
        var attempts = [String]()
        model.onPushRegistrationRequested = { requests += 1 }
        model.onPushDeliveryAttempted = { attempts.append($0) }

        // First Mac: handshake, then Apple issues the token.
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: "dev-a")
        model.enablePush()
        model.simulatePushTokenForTesting("tok-1", environment: "prod")
        await drainDeliveries()
        XCTAssertEqual(attempts, ["tok-1"], "the fresh token goes to the current Mac")

        // Same launch, re-paired to a different Mac: Apple will not call
        // again, so the cached token must be delivered to the new row.
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: "dev-b")
        model.enablePush()
        await drainDeliveries()
        XCTAssertEqual(
            attempts, ["tok-1", "tok-1"],
            "the second Mac gets the token Apple already issued")
        XCTAssertEqual(requests, 1, "no second permission prompt for the second Mac")
    }

    /// The check and the send are separated by a task hop, and the
    /// connection can change inside it. The delivery must re-judge at the
    /// send, not coast on a verdict from the old world.
    func testEligibilityIsRejudgedAtTheSendNotOnlyAtTheGuard() async {
        let model = AppModel()
        var attempts = [String]()
        model.onPushDeliveryAttempted = { attempts.append($0) }

        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: "dev-a")
        model.simulatePushTokenForTesting("tok-1", environment: "prod")
        // The delivery task is queued and judged eligible — and before it
        // runs, the connection becomes a static-token session.
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: nil)
        await drainDeliveries()
        XCTAssertEqual(
            attempts, [],
            "a delivery judged against the old connection must not land on the new one")
    }

    /// Apple's callback fires whenever Apple pleases — including after the
    /// user has switched to a Mac that cannot take the token. Delivery is
    /// gated at the choke point, so the late token waits in the cache for
    /// the next eligible handshake instead of landing as an unsupported
    /// message on the wrong daemon.
    func testALateTokenWaitsOutAnIneligibleConnection() async {
        let model = AppModel()
        var attempts = [String]()
        model.onPushDeliveryAttempted = { attempts.append($0) }

        // Registration began against an eligible Mac…
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: "dev-a")
        model.enablePush()
        // …but by the time Apple answers, the user is on a static-token
        // session with no device row.
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: nil)
        model.simulatePushTokenForTesting("tok-late", environment: "prod")
        await drainDeliveries()
        XCTAssertEqual(attempts, [], "no delivery to a daemon that must refuse it")

        // The next eligible handshake collects the cached token.
        model.connection.simulateCapabilitiesForTesting(
            capabilities(push: true), minor: 7, deviceID: "dev-c")
        model.enablePush()
        await drainDeliveries()
        XCTAssertEqual(attempts, ["tok-late"], "the cache outlives the bad window")
    }
}
