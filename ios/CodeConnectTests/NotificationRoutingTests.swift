import UIKit
import UserNotifications
import XCTest

@testable import CodeConnect

/// Where a tapped notification lands, and how it survives a cold launch.
///
/// Separate from the wire tests because this is navigation and lifecycle, not
/// decoding — the payload's only contribution is one word.
final class NotificationRoutingTests: XCTestCase {

    /// An approval tap opens the decision list.
    @MainActor
    func testAnApprovalTapOpensTheDecisionList() {
        let model = AppModel()
        XCTAssertNil(model.pendingDeepLink, "nothing pending before the tap")
        model.openFromNotification(kind: "approval")
        XCTAssertEqual(
            model.pendingDeepLink, .deck(requestID: nil),
            "the list, never a card — the payload names no decision to open")
    }

    /// **Only an approval has a card there.** `Finished a turn` has nothing to
    /// decide and never will, so sending it to the decision list would be a
    /// wrong answer rather than a stale one. It lands on the fleet — explicitly,
    /// because doing nothing only resembles the fleet on a cold launch.
    @MainActor
    func testEveryOtherKindOpensTheFleetExplicitly() {
        for kind in ["input", "done", "idle", nil] {
            let model = AppModel()
            model.openFromNotification(kind: kind)
            XCTAssertEqual(
                model.pendingDeepLink, .fleet,
                "\(kind ?? "an unknown kind") has no card to open")
        }
    }

    /// **Through `recordTap` itself**, so the store is what is being checked
    /// rather than a copy of it. The broadcast it also performs is harmless
    /// here: the assertion is about what was stored.
    @MainActor
    func testRecordTapStoresBeforeItBroadcastsSoAColdLaunchCannotLoseIt() {
        _ = PushWire.consumeTap()
        PushWire.recordTap(kind: "approval")
        // Read back synchronously: the store happens before the post, so a
        // launch-time callback is drainable even with nothing listening yet.
        let tap = PushWire.consumeTap()
        XCTAssertEqual(tap?.kind, "approval")
        XCTAssertNil(PushWire.consumeTap(), "drained once, not replayed")
    }

    /// A tap that arrived before anything was listening still reaches a model
    /// constructed afterwards.
    @MainActor
    func testATapThatArrivesBeforeAnythingIsListeningStillLands() {
        _ = PushWire.consumeTap()
        // Seeded rather than broadcast: the host app is running during these
        // tests and its own observer would drain the buffer first — which is
        // the production path doing its job, and exactly what a cold launch
        // does not have.
        PushWire.seedTapForTesting(kind: "approval")
        let model = AppModel()
        XCTAssertNil(model.pendingDeepLink, "nothing has drained it yet")

        model.consumePendingTap()
        XCTAssertEqual(model.pendingDeepLink, .deck(requestID: nil))
        XCTAssertNil(PushWire.consumeTap(), "a tap is drained once, not replayed")
    }

    /// **The production payload shape, exactly as the daemon writes it.**
    ///
    /// `UNNotificationResponse` has no public initialiser, so the delegate
    /// itself cannot be driven from a unit test; this covers the one thing the
    /// delegate does that can be wrong — reading the routing word out of a real
    /// `userInfo`.
    ///
    /// The literal below is the daemon's whole payload — the same object, key
    /// for key, though not the same byte order, since `serde_json` writes its
    /// maps sorted. What keeps the two in step is the Rust side, which pins
    /// that object across every kind and count it can produce
    /// (`apns_sender::tests`); a field added there fails that test first.
    func testTheRoutingWordIsReadFromTheDaemonsActualPayloadShape() throws {
        let json = """
            {"aps":{"alert":{"title":"Aion","body":"Waiting on an approval"},\
            "sound":"default","badge":1,"interruption-level":"time-sensitive"},\
            "codeconnect":{"kind":"approval"}}
            """
        let userInfo = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [AnyHashable: Any])
        XCTAssertEqual(PushWire.kind(fromUserInfo: userInfo), "approval")
    }

    /// An older daemon sends no custom object at all. That must read as "no
    /// kind" and land on the fleet, not crash and not be mistaken for one.
    func testAPayloadWithNoRoutingWordReadsAsNone() throws {
        let userInfo = try XCTUnwrap(
            JSONSerialization.jsonObject(
                with: Data(#"{"aps":{"alert":{"title":"Aion","body":"Finished a turn"}}}"#.utf8))
                as? [AnyHashable: Any])
        XCTAssertNil(PushWire.kind(fromUserInfo: userInfo))
    }

    /// **The literal the daemon writes.** Both sides matching on a shared
    /// constant would let a rename pass every test while every approval tap
    /// landed on the fleet, so this pins the word itself.
    @MainActor
    func testTheApprovalWordIsTheOneTheDaemonWrites() {
        let model = AppModel()
        model.openFromNotification(kind: "approval")
        XCTAssertEqual(
            model.pendingDeepLink, .deck(requestID: nil),
            "the daemon writes exactly \"approval\"; anything else routes to the fleet")
    }

    /// **The two ways a tap is silently never delivered.**
    ///
    /// A real tap arrives only if the delegate is installed *and* its callback
    /// matches the Objective-C selector `UNUserNotificationCenter` looks for.
    /// Both can break without a compiler error: an unset delegate is legal, and
    /// a renamed or resignatured Swift method simply stops being that selector.
    /// Either way the app builds, every routing test above still passes, and a
    /// tapped notification does nothing at all.
    ///
    /// **What this cannot reach**, stated rather than implied: the one line
    /// inside that callback. `UNNotificationResponse` has no public
    /// initialiser, so no test can invoke the method Apple invokes. The line
    /// calls `PushWire.deliverTap`, which the UI tests drive end to end with
    /// the daemon's own payload — so emptying `deliverTap` fails a test, and
    /// only deleting the call *from this callback* would not.
    @MainActor
    func testTheTapCallbackIsInstalledAndMatchesApplesSelector() {
        let delegate = PushAppDelegate()
        _ = delegate.application(UIApplication.shared, didFinishLaunchingWithOptions: nil)

        XCTAssertTrue(
            UNUserNotificationCenter.current().delegate === delegate,
            "a delegate installed later than launch can miss the tap that started the app")
        XCTAssertTrue(
            delegate.responds(
                to: #selector(
                    UNUserNotificationCenterDelegate.userNotificationCenter(
                        _:didReceive:withCompletionHandler:))),
            "the tap callback has to be the selector Apple calls, not merely a method")
    }
}
