import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Which kind of device sent an answer**, read from the outcomes a minor-34
/// daemon records (`fixtures/claude/minor-34-wire.json`, emitted by the daemon).
final class AnsweredOnTests: XCTestCase {

    /// The outcome the fixture records under `key`, as an older daemon would have recorded it when `named` is false.
    private func outcome(_ key: String, named: Bool = true) throws -> AnswerOutcome {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-34-wire", withExtension: "json"))
        let root = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let event = try XCTUnwrap(root[key] as? [String: Any])
        var fields = try XCTUnwrap(event["payload"] as? [String: Any])
        if !named { fields["answered_on"] = nil }
        let payload = try JSONSerialization.data(withJSONObject: fields)
        return try JSONDecoder().decode(AnswerOutcome.self, from: payload)
    }

    /// Each kind of device's answer reads as its own, and both as sent to Claude.
    func testAnAnswerSaysWhichKindOfDeviceSentIt() throws {
        let iPhone = try outcome("answered_on_iphone")
        XCTAssertEqual(iPhone.answeredOn, .iphone)
        XCTAssertEqual(iPhone.resolvedBy, .phone)
        XCTAssertTrue(iPhone.wasSentToClaude)

        let mac = try outcome("answered_on_mac")
        XCTAssertEqual(mac.answeredOn, .mac)
        XCTAssertEqual(mac.resolvedBy, .phone)
        XCTAssertEqual(mac.detail, AnswerOutcome.sentToClaude)
        XCTAssertTrue(mac.wasSentToClaude)
    }

    /// An answer from an older daemon names no device, and a kind this build does
    /// not know names none either: neither is an error.
    func testAnAbsentOrUnknownKindIsNone() throws {
        for answeredOn in [nil, "ipad"] as [String?] {
            var payload: [String: Any] = [
                "request_id": "toolu_a", "session_id": "cc-1", "decision": ["type": "allow"],
                "resolved_by": "phone", "applied_via": "hook_return",
                "resolved_at": "2026-11-10T00:00:00.000Z",
            ]
            payload["answered_on"] = answeredOn
            let outcome = try JSONDecoder().decode(
                AnswerOutcome.self, from: JSONSerialization.data(withJSONObject: payload))
            XCTAssertNil(outcome.answeredOn, "\(answeredOn ?? "absent")")
        }
    }

    /// What a card says of a paired device's answer, in each device's words: the kind of device that sent it, never the
    /// daemon's detail, which reads the same for every device. The iPhone's words are unchanged wherever an iPhone
    /// answered or the record does not say; no sentence claims "this Mac" or "your iPhone".
    func testACardNamesTheKindOfDeviceThatAnswered() throws {
        let iPhone = try outcome("answered_on_iphone"), mac = try outcome("answered_on_mac")
        let older = try outcome("answered_on_mac", named: false)
        XCTAssertEqual(iPhone.detail, mac.detail, "the detail cannot tell them apart")
        func said(_ outcome: AnswerOutcome) -> [String] {
            [HostDevice.iPhone, .mac].flatMap { host in
                [outcome.sentToClaudeLine(on: host), outcome.provenance(on: host)]
            }
        }
        XCTAssertEqual(
            said(iPhone),
            ["Sent to Claude from a phone", "from a phone", "Sent to Claude from an iPhone", "from an iPhone"])
        XCTAssertEqual(
            said(mac), ["Sent to Claude from a Mac", "from a Mac", "Sent to Claude from a Mac", "from a Mac"])
        XCTAssertEqual(
            said(older), ["Sent to Claude from a phone", "from a phone", "Sent to Claude from a phone", "from a phone"])
        XCTAssertEqual(
            DecisionCard.resultBanner(for: .indeterminate(mac), on: .mac).message,
            "Allowed · Sent to Claude from a Mac")
        XCTAssertEqual(
            DecisionCard.resultBanner(for: .indeterminate(iPhone), on: .iPhone).message,
            "Allowed · Sent to Claude from a phone")
        let now = try XCTUnwrap(mac.resolvedDate).addingTimeInterval(30)
        XCTAssertEqual(
            DecisionCard.alreadyResolvedBanner(for: iPhone, now: now, on: .mac).message,
            "Allowed · Sent to Claude from an iPhone · \(Format.ago(since: iPhone.resolvedDate, now: now))")

        XCTAssertEqual(ApprovalRow.resolutionText(for: mac), "Sent to Claude from a Mac")
        XCTAssertEqual(ApprovalRow.resolutionText(for: iPhone), "Sent to Claude from a phone")

        func answered(_ device: AnsweredOn?, on host: HostDevice) -> String? {
            QuestionCardStatus.answeredOnPhone([], from: device).banner(on: host)?.message
        }
        XCTAssertEqual(answered(nil, on: .iPhone), "Claude has the answers sent from the phone.")
        XCTAssertEqual(answered(.iphone, on: .iPhone), "Claude has the answers sent from the phone.")
        XCTAssertEqual(answered(.mac, on: .iPhone), "Claude has the answers sent from a Mac.")
        XCTAssertEqual(answered(nil, on: .mac), "Claude has the answers sent from the Mac.")
        XCTAssertEqual(answered(.iphone, on: .mac), "Claude has the answers sent from an iPhone.")
        XCTAssertEqual(answered(.mac, on: .mac), "Claude has the answers sent from a Mac.")
    }

    /// The daemon records an answer as the Mac's by the name the Mac app says
    /// hello with, so that name is pinned here as well as there (`AnsweredOn` in
    /// ccd's protocol).
    func testTheMacAppSaysHelloWithTheNameTheDaemonReads() {
        XCTAssertEqual(DaemonConnection.clientName, "CodeConnect \(HostDevice.current.name)")
        XCTAssertEqual("CodeConnect \(HostDevice.mac.name)", "CodeConnect Mac")
    }
}
