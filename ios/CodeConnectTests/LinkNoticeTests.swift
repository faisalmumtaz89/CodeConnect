import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **What a link that is not working says, word for word** (`LinkHealth.notice`, the Kit's, which the iPhone's banner
/// and the Mac's computer row and list read): the iPhone's banner keeps the title, sentence and action it had when it
/// wrote them itself, one state at a time.
@MainActor
final class LinkNoticeTests: XCTestCase {

    private func banner(_ health: LinkHealth) -> (title: String, message: String?, action: String?)? {
        health.ccBannerItem(onRetry: {}, onSettings: {}, onTailscale: {}).map { ($0.title, $0.message, $0.actionTitle) }
    }

    func testALiveOrLaggingLinkSaysNothing() {
        XCTAssertNil(LinkHealth(level: .live, age: 1, detail: "Live.").notice)
        XCTAssertNil(LinkHealth(level: .lagging, age: 20, detail: "Link lagging.").notice)
        XCTAssertNil(banner(LinkHealth(level: .live, age: 1, detail: "Live.")))
    }

    func testAnUnreachableComputerIsOfflineWithTryAgain() throws {
        let health = LinkHealth(
            level: .offline, age: nil, detail: "Can’t reach your computer. Retrying automatically.")
        XCTAssertEqual(health.notice?.action, .retry)
        let shown = try XCTUnwrap(banner(health))
        XCTAssertEqual(shown.title, "Offline")
        XCTAssertEqual(shown.message, "Can’t reach your computer. Retrying automatically.")
        XCTAssertEqual(shown.action, "Try again")
    }

    func testATailnetNameNotResolvingAsksForTailscale() throws {
        let health = LinkHealth(
            level: .offline, age: nil, detail: "`mac.tail1.ts.net` is not resolving. Retrying automatically.",
            cause: .tailnetHostUnresolvable(host: "mac.tail1.ts.net"))
        XCTAssertEqual(health.notice?.action, .tailscale)
        let shown = try XCTUnwrap(banner(health))
        XCTAssertEqual(shown.title, "Connect Tailscale")
        XCTAssertEqual(
            shown.message,
            "Can’t find your computer at `mac.tail1.ts.net`. Turn on Tailscale on this iPhone, signed in to the same account as your computer. CodeConnect reconnects on its own.")
        XCTAssertEqual(shown.action, TailscaleAssist.isInstallHintNeeded ? "Set up Tailscale" : "Open Tailscale")
    }

    func testAnotherNameNotResolvingIsHostNotFoundWithSettings() throws {
        let health = LinkHealth(
            level: .offline, age: nil, detail: "`mac.lan` is not resolving. Retrying automatically.",
            cause: .hostUnresolvable(host: "mac.lan"))
        let shown = try XCTUnwrap(banner(health))
        XCTAssertEqual(shown.title, "Host not found")
        XCTAssertEqual(
            shown.message,
            "`mac.lan` isn’t resolving. Check the paired address and this iPhone’s network. CodeConnect reconnects on its own.")
        XCTAssertEqual(shown.action, "Settings")
    }

    func testABlockedDeviceSaysSoWithTryAgain() throws {
        let sentence = "Your computer has blocked this device. It reconnects when the computer allows it again."
        let shown = try XCTUnwrap(banner(LinkHealth(level: .blocked, age: nil, detail: sentence)))
        XCTAssertEqual(shown.title, "Blocked")
        XCTAssertEqual(shown.message, sentence)
        XCTAssertEqual(shown.action, "Try again")
    }

    func testARefusalSaysItsOwnWordWithSettings() throws {
        let version = "Your computer has an older CodeConnect. Update CodeConnect on it, then try again."
        let different = try XCTUnwrap(banner(LinkHealth(
            level: .rejected, age: nil, detail: version, cause: .refused(.differentVersion))))
        XCTAssertEqual(different.title, "Different version")
        XCTAssertEqual(different.message, version)
        XCTAssertEqual(different.action, "Settings")
        let pair = try XCTUnwrap(banner(LinkHealth(
            level: .rejected, age: nil, detail: "The computer did not accept this device.", cause: .refused(.credential))))
        XCTAssertEqual(pair.title, "Pair again")
        XCTAssertEqual(pair.message, "The computer did not accept this device.")
        XCTAssertEqual(pair.action, "Settings")
    }

    func testAStaleLinkSaysItsAgeWithNoAction() throws {
        let shown = try XCTUnwrap(banner(LinkHealth(
            level: .stale, age: 75, detail: "Your computer has gone quiet. Retrying automatically.")))
        XCTAssertEqual(shown.title, "Link stale")
        XCTAssertEqual(shown.message, "1m since your computer last answered. Actions are disabled until it answers.")
        XCTAssertNil(shown.action)
    }

    func testAConnectingLinkSaysSoWithNoAction() throws {
        let shown = try XCTUnwrap(banner(LinkHealth(level: .connecting, age: nil, detail: "Opening the connection.")))
        XCTAssertEqual(shown.title, "Connecting")
        XCTAssertEqual(shown.message, "Connecting to your computer…")
        XCTAssertNil(shown.action)
    }

    /// The word beside a computer's name, now the Kit's, as the iPhone's card label said it.
    func testTheWordBesideAComputersName() {
        XCTAssertNil(LinkHealth(level: .live, age: 1, detail: "").machineWord)
        XCTAssertNil(LinkHealth(level: .lagging, age: 20, detail: "").machineWord)
        XCTAssertEqual(LinkHealth(level: .stale, age: 75, detail: "").machineWord, "Stale")
        XCTAssertEqual(LinkHealth(level: .connecting, age: nil, detail: "").machineWord, "Connecting")
        XCTAssertEqual(LinkHealth(level: .offline, age: nil, detail: "").machineWord, "Offline")
        XCTAssertEqual(LinkHealth(level: .blocked, age: nil, detail: "").machineWord, "Blocked")
        XCTAssertEqual(
            LinkHealth(level: .rejected, age: nil, detail: "", cause: .refused(.differentVersion)).machineWord,
            "Different version")
        XCTAssertEqual(
            LinkHealth(level: .rejected, age: nil, detail: "", cause: .refused(.credential)).machineWord, "Pair again")
    }

    /// A row's words that say what its action says are not shown beside it; any other words are.
    func testWordsThatSayTheActionAreShownOnce() {
        XCTAssertNil(LinkNotice.words("Pair again", beside: "Pair Again"))
        XCTAssertEqual(LinkNotice.words("Offline", beside: "Try Again"), "Offline")
        XCTAssertEqual(LinkNotice.words("Pair again", beside: nil), "Pair again")
    }
}
