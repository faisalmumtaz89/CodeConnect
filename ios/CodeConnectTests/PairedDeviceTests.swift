import XCTest

@testable import CodeConnectKit

/// **A computer's devices, blocked and allowed again, against the daemon's own bytes.**
///
/// `devices.json` is what ccd's local socket sends and reads (`fixtures/wire/devices.json`, emitted by the protocol
/// crate's test and copied here byte for byte): the list, a block and an allow, and the answer to each.
final class PairedDeviceTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: "devices", withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func reply(_ key: String) throws -> DeviceReply {
        let value = try XCTUnwrap(try wire()[key], "no \(key) in devices.json")
        return try JSONDecoder().decode(DeviceReply.self, from: Data(value.canonicalJSONString.utf8))
    }

    private func listed() throws -> [PairedDevice] {
        guard case .devices(let devices) = try reply("devices") else {
            XCTFail("devices is not a device list")
            throw CancellationError()
        }
        return devices
    }

    func testTheRequestsEncodeAsTheDaemonReadsThem() throws {
        let root = try wire()
        let cases: [(DeviceRequest, String)] = [
            (.list, "list_devices_request"),
            (.block(deviceID: "a1b2c3d4e5f6"), "revoke_device_request"),
            (.allow(deviceID: "b7c8d9e0f1a2"), "unblock_device_request"),
        ]
        for (request, key) in cases {
            XCTAssertEqual(
                try JSONDecoder().decode(JSONValue.self, from: JSONEncoder().encode(request)), root[key], key)
        }
    }

    func testEveryAnswerDecodesInTheDaemonsOwnWords() throws {
        let devices = try listed()
        XCTAssertEqual(devices.count, 6)
        XCTAssertEqual(devices.map(\.name).first, "Jordan’s iPhone")
        let legacy = try XCTUnwrap(devices.first { $0.deviceID == "f9a0b1c2d3e4" })
        XCTAssertTrue(legacy.pairedBeforeNodes, "sent only when true, and read when sent")
        XCTAssertTrue(legacy.holdsNewDevices)
        XCTAssertFalse(devices.first { $0.deviceID == "b7c8d9e0f1a2" }!.holdsNewDevices, "blocked, with its device")
        guard case .blocked(let blocked) = try reply("revoked") else { return XCTFail("not a block") }
        XCTAssertEqual(blocked.deviceID, "a1b2c3d4e5f6")
        XCTAssertTrue(blocked.isBlocked)
        guard case .allowed(let allowed) = try reply("unblocked") else { return XCTFail("not an allow") }
        XCTAssertEqual(allowed.deviceID, "b7c8d9e0f1a2")
        let refusal = try JSONDecoder().decode(
            DeviceReply.self, from: Data(#"{"type":"error","message":"that device is not blocked"}"#.utf8))
        XCTAssertEqual(refusal, .refused("that device is not blocked"))
    }

    /// One entry a phone or computer: the two pairings of one Tailscale device are one, allowed and blocked are
    /// apart, and each list runs most recently seen first.
    func testDevicesGroupByTheirTailscaleDevice() throws {
        let (allowed, blocked) = DeviceGroup.groups(try listed())
        XCTAssertEqual(allowed.map(\.name), ["build-server-eu-west-1", "Jordan’s iPhone", "Jordan’s MacBook Pro"])
        let laptop = try XCTUnwrap(allowed.last)
        XCTAssertEqual(laptop.devices.map(\.deviceID), ["e7f8a9b0c1d2", "d5e6f7a8b9c0"], "newest pairing first")
        XCTAssertEqual(blocked.map(\.name), ["Jordan’s iPad", "Jordan’s old iPhone"])
        XCTAssertEqual(blocked.map(\.holdsNewDevices), [false, true])
    }

    func testTheWordsNameTheDevice() throws {
        let (allowed, blocked) = DeviceGroup.groups(try listed())
        let phone = try XCTUnwrap(allowed.first { $0.name == "Jordan’s iPhone" })
        XCTAssertEqual(phone.blockQuestion, "Block Jordan’s iPhone?")
        XCTAssertEqual(phone.blockSentence, "Blocks Jordan’s iPhone and its messages to this computer.")
        XCTAssertTrue(phone.seenLine().hasPrefix("Last seen "))
        let ipad = try XCTUnwrap(blocked.first)
        XCTAssertEqual(ipad.allowQuestion, "Allow Jordan’s iPad again?")
        XCTAssertEqual(ipad.allowSentence, "Jordan’s iPad can reach this computer again.")
        XCTAssertTrue(ipad.blockedLine().hasPrefix("Blocked "))
        XCTAssertEqual(
            blocked.last?.blockedLine(),
            "Blocked by an older CodeConnect. While it’s listed, any new device needs a code to join.")
    }
}
