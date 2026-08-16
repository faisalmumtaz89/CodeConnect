import XCTest

@testable import CodeConnect

// =============================================================================
//  Minor 14 on the app side: the normalized push mode, the new wire fields, and
//  the compatibility matrix from docs/push-gateway.md §5.
// =============================================================================

final class PushModeWireTests: XCTestCase {

    // MARK: PushMode normalization

    func testDirectTakesPrecedenceOverRelay() {
        // A daemon that somehow advertised both is a direct-key daemon; the
        // direct path needs no relay credential.
        XCTAssertEqual(Capabilities(push: true, pushRelay: true).pushMode, .direct)
    }

    func testRelayWhenOnlyRelayFlag() {
        XCTAssertEqual(Capabilities(push: false, pushRelay: true).pushMode, .relay)
    }

    func testDirectWhenOnlyPushFlag() {
        XCTAssertEqual(Capabilities(push: true, pushRelay: false).pushMode, .direct)
    }

    func testNoneWhenNeitherFlag() {
        XCTAssertEqual(Capabilities(push: false, pushRelay: false).pushMode, .none)
    }

    // MARK: New wire fields, decoded

    private func capabilities(fromAck json: String) throws -> Capabilities {
        let message = try JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
        guard case .helloAck(let ack) = message else {
            throw XCTSkip("not a hello_ack")
        }
        return ack.capabilities
    }

    func testPushRelayDecodesFromTheAck() throws {
        let caps = try capabilities(
            fromAck: #"{"type":"hello_ack","protocol_version":1,"protocol_minor":14,"capabilities":{"push":false,"push_relay":true}}"#)
        XCTAssertTrue(caps.pushRelay)
        XCTAssertFalse(caps.push)
        XCTAssertEqual(caps.pushMode, .relay)
    }

    func testPushRelayAbsentDefaultsFalse() throws {
        let caps = try capabilities(
            fromAck: #"{"type":"hello_ack","protocol_version":1,"protocol_minor":13,"capabilities":{"push":true}}"#)
        XCTAssertFalse(caps.pushRelay, "an old daemon that never heard of the key reads as false")
        XCTAssertEqual(caps.pushMode, .direct)
    }

    func testHelloAckPushEnvironmentDecodes() throws {
        let message = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                #"{"type":"hello_ack","protocol_version":1,"protocol_minor":14,"push_environment":"production","capabilities":{}}"#
                    .utf8))
        guard case .helloAck(let ack) = message else { return XCTFail("no hello_ack") }
        XCTAssertEqual(ack.pushEnvironment, "production")
    }

    func testHelloAckPushEnvironmentAbsentIsNil() throws {
        let message = try JSONDecoder().decode(
            ServerMessage.self,
            from: Data(
                #"{"type":"hello_ack","protocol_version":1,"protocol_minor":14,"capabilities":{}}"#
                    .utf8))
        guard case .helloAck(let ack) = message else { return XCTFail("no hello_ack") }
        XCTAssertNil(ack.pushEnvironment)
    }

    func testCredentialInvalidDecodes() throws {
        let result = try JSONDecoder().decode(
            TestPushResult.self, from: Data(#"{"status":"credential_invalid"}"#.utf8))
        XCTAssertEqual(result, .credentialInvalid)
    }

    // MARK: RegisterPush encoding

    private func encodeRegister(credential: String?) throws -> [String: Any] {
        let message = ClientMessage.registerPush(
            token: "abc", environment: "production", relayCredential: credential)
        let data = try JSONEncoder().encode(message)
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
    }

    func testRegisterPushCarriesRelayCredentialWhenPresent() throws {
        let object = try encodeRegister(credential: "BEARER-xyz")
        XCTAssertEqual(object["type"] as? String, "register_push")
        XCTAssertEqual(object["token"] as? String, "abc")
        XCTAssertEqual(object["environment"] as? String, "production")
        XCTAssertEqual(object["relay_credential"] as? String, "BEARER-xyz")
    }

    func testRegisterPushOmitsRelayCredentialWhenAbsent() throws {
        let object = try encodeRegister(credential: nil)
        XCTAssertNil(
            object["relay_credential"],
            "direct mode omits the key entirely — a present null would read as a lost credential")
        // The legacy fields are byte-for-byte the old direct registration.
        XCTAssertEqual(object["type"] as? String, "register_push")
        XCTAssertEqual(object["token"] as? String, "abc")
        XCTAssertEqual(object["environment"] as? String, "production")
    }

    // MARK: Compatibility matrix (§5)

    /// Each row is the mode the *new app* computes from what the daemon
    /// advertises. The eligibility half (a device row for non-bootstrap) is an
    /// AppModel concern; this pins the capability half the matrix turns on.
    func testCompatibilityMatrix() {
        // New app + old direct-key daemon: sees push=true → direct.
        XCTAssertEqual(Capabilities(push: true).pushMode, .direct)
        // New app + old unconfigured daemon: neither flag → none, no prompt.
        XCTAssertEqual(Capabilities().pushMode, .none)
        // New app + new relay daemon: push_relay=true → relay.
        XCTAssertEqual(Capabilities(pushRelay: true).pushMode, .relay)
        // A relay daemon advertising push=false does NOT read as "no push".
        XCTAssertNotEqual(Capabilities(push: false, pushRelay: true).pushMode, PushMode.none)
    }
}
