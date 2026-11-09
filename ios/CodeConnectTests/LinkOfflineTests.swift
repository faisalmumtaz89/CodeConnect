import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The offline link's classification, copy and cadence — the value layer of
/// the fix for the flickering "OFFLINE … retrying in 5s" banner.
///
/// The defect, filmed on a device with Tailscale off: DNS fails instantly,
/// every retry blanked the banner (each dial re-armed the connect grace), the
/// countdown re-rendered each second, and the reason text alternated with
/// scheme-switch narration that could never fix DNS.
@MainActor
final class LinkOfflineTests: XCTestCase {

    // MARK: Classification

    func testUnresolvableIsRecognisedDirectlyAndDownTheUnderlyingChain() {
        let direct = URLError(.cannotFindHost)
        XCTAssertTrue(DaemonConnection.isUnresolvableHost(direct))
        XCTAssertTrue(DaemonConnection.isUnresolvableHost(URLError(.dnsLookupFailed)))

        // URLSession routinely wraps the interesting error one level down.
        let wrapped = NSError(
            domain: "com.apple.something", code: 1,
            userInfo: [NSUnderlyingErrorKey: direct as NSError])
        XCTAssertTrue(DaemonConnection.isUnresolvableHost(wrapped))

        XCTAssertFalse(DaemonConnection.isUnresolvableHost(URLError(.timedOut)))
        XCTAssertFalse(
            DaemonConnection.isUnresolvableHost(URLError(.cannotConnectToHost)),
            "refused is not unresolved: the name answered, the port did not")
    }

    func testTheTailnetSuffixRuleIsBoundariedCaseInsensitiveAndDotTolerant() {
        XCTAssertTrue(LinkHealth.isTailnetHost("my-mac.tailnet-abcd.ts.net"))
        XCTAssertTrue(LinkHealth.isTailnetHost("My-Mac.Tailnet.TS.NET"))
        XCTAssertTrue(LinkHealth.isTailnetHost("my-mac.tailnet.ts.net."))
        XCTAssertFalse(
            LinkHealth.isTailnetHost("evilts.net"),
            "the suffix is a label boundary, not a substring")
        XCTAssertFalse(LinkHealth.isTailnetHost("ts.net"))
        XCTAssertFalse(LinkHealth.isTailnetHost("mac.example.com"))
    }

    // MARK: Evaluate — calm copy, sticky redial

    private let waiting = DaemonConnection.Phase.waiting(
        until: Date().addingTimeInterval(7), reason: "Could not connect to the server.")

    func testAWaitingLinkNamesTheFailureWithoutACountdown() {
        let health = LinkHealth.evaluate(
            phase: waiting, lastContactAt: nil,
            dialFailure: .hostUnresolvable(host: "mac.tail1234.ts.net"))
        XCTAssertEqual(health.level, .offline)
        XCTAssertEqual(health.cause, .tailnetHostUnresolvable(host: "mac.tail1234.ts.net"))
        XCTAssertTrue(health.detail.contains("mac.tail1234.ts.net"))
        XCTAssertTrue(health.detail.contains("Retrying automatically"))
        XCTAssertFalse(
            health.detail.contains("retrying in"),
            "a per-second countdown re-renders the banner and informs nothing")
    }

    /// The app's own words, never the system's: the phase's reason here is
    /// URLSession's sentence, and it stays off the screen.
    func testAnOrdinaryFailureAlsoLosesTheCountdown() {
        let health = LinkHealth.evaluate(
            phase: waiting, lastContactAt: nil, dialFailure: .unreachable)
        XCTAssertEqual(health.level, .offline)
        XCTAssertEqual(health.detail, "Can't reach your computer. Retrying automatically.")
        XCTAssertNil(health.cause)
    }

    // MARK: A computer that goes quiet

    /// Quiet reads stale from the silence deadline on: while the socket is
    /// still up, and through the cut and the redial after it. On a real
    /// computer the cut came 30 to 40 s in, before stale's old 45 s, and the
    /// banner read the system's text for the cut: "The operation couldn't be
    /// completed. Socket is not connected. Retrying automatically."
    func testAQuietComputerReadsStaleFromTheDeadlineThroughTheRedial() {
        let now = Date()
        let heard = now.addingTimeInterval(-DaemonConnection.silenceDeadline)

        let connected = LinkHealth.evaluate(
            phase: .connected(since: heard), lastContactAt: heard, now: now)
        XCTAssertEqual(connected.level, .stale, "stale at the deadline, before the cut")

        let failure = DaemonConnection.dropped(wasUp: true, lastContactAt: heard, now: now)
        XCTAssertEqual(failure, .quiet)
        for phase in [DaemonConnection.Phase.connecting, waiting] {
            let health = LinkHealth.evaluate(
                phase: phase, lastContactAt: heard, dialFailure: failure, isRedial: true, now: now)
            XCTAssertEqual(health.level, .stale, "\(phase)")
            XCTAssertEqual(health.ageText, "30s", "the age keeps counting from the last answer")
            XCTAssertEqual(health.detail, "Your computer has gone quiet. Retrying automatically.")
            let banner = health.ccBannerItem(onRetry: {}, onSettings: {}, onTailscale: {})
            XCTAssertEqual(banner?.title, "Link stale")
            XCTAssertNil(banner?.actionTitle, "the app is already dialling again")
        }
    }

    /// Only a link that was up and heard nothing for the deadline went quiet.
    /// A link dropped sooner, and a dial that never got its handshake, could
    /// not reach the computer.
    func testOnlyALinkThatWasUpAndSilentWentQuiet() {
        let now = Date()
        let deadline = DaemonConnection.silenceDeadline
        XCTAssertEqual(
            DaemonConnection.dropped(
                wasUp: true, lastContactAt: now.addingTimeInterval(-(deadline - 1)), now: now),
            .unreachable)
        XCTAssertEqual(
            DaemonConnection.dropped(
                wasUp: false, lastContactAt: now.addingTimeInterval(-600), now: now),
            .unreachable, "a redial that fails is a fresh failure, however long ago the last answer")
        XCTAssertEqual(
            DaemonConnection.dropped(wasUp: true, lastContactAt: nil, now: now), .unreachable)
    }

    func testARedialKeepsTheFailureOnScreenInsteadOfBlanking() {
        // The flicker: every retry entered .connecting, which used to evaluate
        // as a fresh "Opening the connection" and drop the banner for the
        // second the dial took.
        let health = LinkHealth.evaluate(
            phase: .connecting, lastContactAt: nil,
            dialFailure: .hostUnresolvable(host: "mac.tail1234.ts.net"), isRedial: true)
        XCTAssertEqual(health.level, .offline, "a redial shows the standing failure")
        XCTAssertEqual(health.cause, .tailnetHostUnresolvable(host: "mac.tail1234.ts.net"))

        let launch = LinkHealth.evaluate(
            phase: .connecting, lastContactAt: nil, dialFailure: nil, isRedial: false)
        XCTAssertEqual(launch.level, .connecting, "the launch dial keeps its quiet grace")
    }

    func testANonTailnetHostGetsTheGenericCause() {
        let health = LinkHealth.evaluate(
            phase: waiting, lastContactAt: nil,
            dialFailure: .hostUnresolvable(host: "mac.example.com"))
        XCTAssertEqual(health.cause, .hostUnresolvable(host: "mac.example.com"))
    }

    // MARK: Banner mapping

    func testTheTailnetBannerInstructsWithoutClaimingTailscaleIsOff() {
        let health = LinkHealth.evaluate(
            phase: waiting, lastContactAt: nil,
            dialFailure: .hostUnresolvable(host: "mac.tail1234.ts.net"))
        let item = health.ccBannerItem(onRetry: {}, onSettings: {}, onTailscale: {})
        XCTAssertEqual(item?.title, "Connect Tailscale")
        XCTAssertTrue(item?.message?.contains("mac.tail1234.ts.net") == true)
        XCTAssertTrue(
            item?.actionTitle == "Open Tailscale" || item?.actionTitle == "Set up Tailscale",
            "the action adapts to whether the Tailscale app answers canOpenURL")
    }

    func testTheGenericUnresolvableBannerPointsAtSettings() {
        let health = LinkHealth.evaluate(
            phase: waiting, lastContactAt: nil,
            dialFailure: .hostUnresolvable(host: "mac.example.com"))
        let item = health.ccBannerItem(onRetry: {}, onSettings: {}, onTailscale: {})
        XCTAssertEqual(item?.title, "Host not found")
        XCTAssertEqual(item?.actionTitle, "Settings")
    }

    func testAPlainOfflineBannerIsUnchanged() {
        let health = LinkHealth.evaluate(
            phase: waiting, lastContactAt: nil,
            dialFailure: .unreachable)
        let item = health.ccBannerItem(onRetry: {}, onSettings: {}, onTailscale: {})
        XCTAssertEqual(item?.title, "Offline")
        XCTAssertEqual(item?.actionTitle, "Retry")
    }

    // MARK: Plain words

    /// Everything the link says about itself, in the fleet's banner, a disabled
    /// control's reason and VoiceOver, names the computer, never the helper on
    /// it or the network under it.
    func testTheLinkSpeaksOfTheComputer() {
        let now = Date()
        let healths = [
            LinkHealth.evaluate(phase: .idle, lastContactAt: nil),
            LinkHealth.evaluate(phase: .connecting, lastContactAt: nil),
            LinkHealth.evaluate(
                phase: .connected(since: now), lastContactAt: now.addingTimeInterval(-60), now: now),
            LinkHealth.evaluate(
                phase: waiting, lastContactAt: nil,
                dialFailure: .hostUnresolvable(host: "mac.tail1234.ts.net")),
            LinkHealth.evaluate(phase: waiting, lastContactAt: nil, dialFailure: .unreachable),
            LinkHealth.evaluate(
                phase: waiting, lastContactAt: now.addingTimeInterval(-60), dialFailure: .quiet,
                now: now),
        ]
        for health in healths {
            let banner = health.ccBannerItem(onRetry: {}, onSettings: {}, onTailscale: {})
            for sentence in [health.detail, health.disabledReason, health.spokenValue, banner?.message] {
                guard let sentence else { continue }
                XCTAssertFalse(sentence.localizedCaseInsensitiveContains("daemon"), sentence)
                XCTAssertFalse(sentence.localizedCaseInsensitiveContains("tailnet"), sentence)
            }
        }
    }

    /// A computer this phone has not heard from since launch last answered
    /// "never", which takes no "ago".
    func testAnAgeThatHasNotHappenedSaysNever() {
        let now = Date()
        XCTAssertEqual(Format.ago(since: nil, now: now), "never")
        XCTAssertEqual(Format.ago(since: now.addingTimeInterval(-120), now: now), "2m ago")
    }

    /// The system alert asking for the local network is the first thing many
    /// people read, and it says the same.
    func testTheLocalNetworkPromptSpeaksOfTheComputer() throws {
        let prompt = try XCTUnwrap(
            Bundle.main.object(forInfoDictionaryKey: "NSLocalNetworkUsageDescription") as? String)
        XCTAssertFalse(prompt.localizedCaseInsensitiveContains("daemon"), prompt)
    }

    // MARK: Cadence

    func testTheDNSCadenceIsFastTwiceThenCalm() {
        // Two fast attempts absorb a transient blip…
        XCTAssertLessThanOrEqual(DaemonConnection.dnsBackoff(streak: 1, unit: 1), 1.0)
        XCTAssertLessThanOrEqual(DaemonConnection.dnsBackoff(streak: 2, unit: 1), 1.0)
        // …then the pace drops to one a screen can be calm over.
        XCTAssertEqual(DaemonConnection.dnsBackoff(streak: 3, unit: 0), 10)
        XCTAssertEqual(DaemonConnection.dnsBackoff(streak: 3, unit: 1), 15)
        XCTAssertEqual(DaemonConnection.dnsBackoff(streak: 4, unit: 0), 20)
        XCTAssertEqual(DaemonConnection.dnsBackoff(streak: 9, unit: 1), 30)
    }

    func testTheGeneralBackoffKeepsItsJitteredBounds() {
        XCTAssertEqual(DaemonConnection.backoff(attempt: 0, unit: 1), 0.5)
        XCTAssertEqual(DaemonConnection.backoff(attempt: 3, unit: 0), 1.0)
        XCTAssertEqual(DaemonConnection.backoff(attempt: 3, unit: 1), 2.0)
        XCTAssertEqual(
            DaemonConnection.backoff(attempt: 40, unit: 1), 30,
            "the ceiling holds however long the outage")
    }

    // MARK: Retry ledger lifecycle

    /// The counters are about the *current unbroken run of failures*, and a
    /// handshake ends the run. Kept loose, one eligible failure before a
    /// successful connect plus one unrelated drop after it made "two
    /// failures" and switched away from a scheme that demonstrably worked —
    /// and a fresh outage inherited the old one's calm cadence.
    func testAHandshakeEndsTheFailureRun() {
        var ledger = DaemonConnection.RetryLedger()

        ledger.failure()
        XCTAssertFalse(ledger.takeAlternation(), "one failure alone never switches scheme")
        ledger.reset()  // hello_ack

        ledger.failure()
        XCTAssertFalse(
            ledger.takeAlternation(),
            "the first failure after a working connection is failure ONE — "
                + "the pre-ack failure must not count toward the pair")
        ledger.failure()
        XCTAssertTrue(ledger.takeAlternation(), "two consecutive failures switch")
        XCTAssertFalse(ledger.takeAlternation(), "and the pair is consumed")
    }

    func testAFreshOutageStartsAtTheFastCadence() {
        var ledger = DaemonConnection.RetryLedger()
        for _ in 0..<6 { ledger.dnsFailure() }
        XCTAssertEqual(ledger.delay(unit: 0), 20, "deep in an outage the cadence is calm")

        ledger.reset()  // hello_ack or a fresh start
        ledger.dnsFailure()
        XCTAssertLessThanOrEqual(
            ledger.delay(unit: 1), 1.0,
            "a new outage earns the fast first tries, not the old outage's pace")
    }

    func testDNSFailuresNeverAdvanceTheSchemePair() {
        var ledger = DaemonConnection.RetryLedger()
        for _ in 0..<5 { ledger.dnsFailure() }
        XCTAssertFalse(ledger.takeAlternation(), "no TCP happened; the scheme is not implicated")
        ledger.failure()
        XCTAssertFalse(
            ledger.takeAlternation(),
            "the first real failure after DNS recovers is failure ONE of a fresh pair")
    }

    // MARK: Path monitor guard

    func testAPathChangeNeverRestartsATerminalFailure() {
        let connection = DaemonConnection()
        connection.simulatePhaseForTesting(.failed(reason: "The daemon rejected this token"))
        connection.pathDidChange()
        XCTAssertEqual(
            connection.phase, .failed(reason: "The daemon rejected this token"),
            "a rejected token retried on every network blip would spin forever")

        connection.simulatePhaseForTesting(.idle)
        connection.pathDidChange()
        XCTAssertEqual(connection.phase, .idle, "idle has nothing to wake")
    }

    // MARK: Silence

    /// A handshaken link that has sent nothing for the deadline is dead: a
    /// vanished network leaves a socket that takes writes and delivers nothing
    /// (B19, reproduced with a half-open proxy: pings went out for 77 s).
    func testALinkSilentForTheDeadlineIsDeadAndOneHeardFromIsNot() {
        let heard = Date(timeIntervalSinceReferenceDate: 1_000)
        let deadline = DaemonConnection.silenceDeadline
        XCTAssertFalse(DaemonConnection.silenceExpired(since: nil, now: heard))
        XCTAssertFalse(
            DaemonConnection.silenceExpired(since: heard, now: heard.addingTimeInterval(deadline - 1)),
            "a slow answer holds pongs back for up to the daemon's own budget")
        XCTAssertTrue(
            DaemonConnection.silenceExpired(since: heard, now: heard.addingTimeInterval(deadline)))
        XCTAssertEqual(deadline, 30, "the app's 20s request wait plus one 10s ping")
    }
}
