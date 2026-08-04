import XCTest

@testable import CodeConnect

/// A pairing code the phone can prove is dead must be refused before it is used.
///
/// This is the strongest evidence the app ever holds before it has spoken to
/// anything: a fact established offline, from the scanned payload alone. Everything
/// else about the Mac — Tailscale, the daemon, sleep, tailnet policy — is one
/// indistinguishable transport failure, and the app must not pretend otherwise.
///
/// Mirrors `protocol::pairing::unreachable_host` on the Mac. Duplicated on purpose:
/// the daemon that minted a code may predate that check, and the phone should
/// refuse a dead code immediately rather than spend a five-minute code discovering
/// it.
final class PairingReachabilityTests: XCTestCase {

    private func qr(host: String) -> String {
        #"{"v":1,"host":"\#(host)","port":8787,"code":"ABCD2345"}"#
    }

    // MARK: What must be refused

    /// `127.0.0.1` is the one that shipped. `ccd` binds loopback whenever Tailscale
    /// is not up when it starts, and `codeconnect pair` drew a scannable code
    /// around it and exited 0.
    func testAQRPointingSomewhereUnreachableIsRefused() {
        for host in [
            "127.0.0.1", "127.1.2.3", "localhost", "LOCALHOST",
            "::1", "[::1]", "0.0.0.0", "::", "169.254.10.1", "fe80::1",
        ] {
            switch PairingQRPayload.decode(qr(host: host)) {
            case .success:
                XCTFail("\(host) can never be reached from this iPhone and must be refused")
            case .failure(let failure):
                guard case .unreachableHost = failure else {
                    return XCTFail("\(host) failed for the wrong reason: \(failure)")
                }
            }
        }
    }

    /// The message has to name the address and the fix, because the person reading
    /// it is standing at the Mac that produced it.
    func testTheRefusalNamesTheHostAndTheRecovery() {
        guard case .failure(let failure) = PairingQRPayload.decode(qr(host: "127.0.0.1")),
            let message = failure.errorDescription
        else { return XCTFail("expected a described failure") }
        XCTAssertTrue(message.contains("127.0.0.1"), message)
        XCTAssertTrue(message.contains("codeconnect daemon restart"), message)
    }

    // MARK: What must NOT be refused

    /// The half that would do real damage. Refusing a working setup is worse than
    /// the defect being fixed: the defect costs one scan, this would make pairing
    /// impossible with no way for the operator to argue back.
    func testAnythingThatMightWorkIsStillAccepted() {
        for host in [
            // The ordinary cases: a tailnet address and a MagicDNS name.
            "100.101.102.103",
            "some-mac.tailnet-example.ts.net",
            // Unfamiliar is not broken. A tailnet with its own domain, or an
            // operator who pinned `ws_bind` to something reachable on purpose.
            "mac.internal.example.com",
            "192.168.1.20",
            "10.0.0.5",
            "2001:db8::1",
        ] {
            switch PairingQRPayload.decode(qr(host: host)) {
            case .success(let payload):
                XCTAssertEqual(payload.host, host)
            case .failure(let failure):
                XCTFail("\(host) may be reachable; refusing it would be a guess (\(failure))")
            }
        }
    }

    /// The existing refusals must keep their own reasons rather than being
    /// swallowed by the new one.
    func testTheOlderRefusalsAreUnchanged() {
        guard case .failure(let notJSON) = PairingQRPayload.decode("hello") else {
            return XCTFail("expected a failure")
        }
        guard case .notJSON = notJSON else { return XCTFail("wrong reason: \(notJSON)") }

        let badPort = #"{"v":1,"host":"mac.ts.net","port":70000,"code":"ABCD2345"}"#
        guard case .failure(let port) = PairingQRPayload.decode(badPort) else {
            return XCTFail("expected a failure")
        }
        guard case .badPort = port else { return XCTFail("wrong reason: \(port)") }
    }
}
