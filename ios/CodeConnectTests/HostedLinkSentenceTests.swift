import XCTest

@testable import CodeConnectKit

/// **What the apps say when a run's link is down.** A run's link is ccd's connection to its agent host.
/// `minor-38-wire.json` carries a hosted run's summary as the daemon emits it, with that connection up
/// (`session_hosted_live`) and down, the host still running (`session_hosted_detached`).
@MainActor
final class HostedLinkSentenceTests: XCTestCase {
    private let capabilities = Capabilities(canApproveReliably: true, sendText: true)

    func testAHostedRunWhoseLinkIsDownSaysItsLinkIsLost() throws {
        XCTAssertEqual(
            reason(try summary("session_hosted_detached")),
            "\(HostDevice.current.computer("The computer")) has lost its link to this run. "
                + "It can be answered again once the link is back.")
        XCTAssertEqual(
            FleetStatusRule.capability(summary: try summary("session_hosted_live"), capabilities: capabilities),
            .control)
    }

    /// A run whose start failed never started, and ccd counts it as ended. Its link can still read attached until its
    /// host exits. A derived input: the hosted summary with its lifecycle set to `failed`, the value ccd writes for a
    /// refused start (`Lifecycle::Failed`), with each link a failed run carries.
    func testAFailedRunReadsEnded() throws {
        for link in ["attached", "detached"] {
            XCTAssertEqual(
                reason(try summary("session_hosted_detached", setting: ["lifecycle": "failed", "link": link])),
                "This run has ended.", "link \(link)")
        }
    }

    private func reason(_ summary: SessionSummary) -> String? {
        FleetStatusRule.capability(summary: summary, capabilities: capabilities).reason
    }

    private func summary(_ key: String, setting fields: [String: Any] = [:]) throws -> SessionSummary {
        let wire = try XCTUnwrap(WireRecording.json("minor-38-wire") as? [String: Any])
        var object = try XCTUnwrap(wire[key] as? [String: Any], "no \(key) in minor-38-wire.json")
        object.merge(fields) { _, new in new }
        return try JSONDecoder().decode(SessionSummary.self, from: JSONSerialization.data(withJSONObject: object))
    }
}
