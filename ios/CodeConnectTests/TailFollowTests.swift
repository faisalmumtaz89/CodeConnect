import XCTest

@testable import CodeConnect

/// The tail-following verdict's state machine, at the seams a UI test cannot
/// reach: the *synchronous* hand-away guard against the debounced pill flag.
///
/// The defect this pins: `following` flips through a deliberate 400ms
/// debounce so the pill cannot flicker, and an event arriving inside that
/// window used to pass the follow guard and yank a reader who was actively
/// scrolling up the history back to the tail.
@MainActor
final class TailFollowTests: XCTestCase {

    func testLeaveRaisesTheYankGuardThisInstantNotAfterTheDebounce() {
        let watch = TailWatch()
        watch.leave()
        XCTAssertTrue(
            watch.handAway,
            "the yank guard must not wait out the pill's 400ms debounce")
        XCTAssertTrue(
            watch.following,
            "the pill's own flag still owes the debounce — it must not flicker")
    }

    func testArriveClearsTheGuardAndRestoresFollowingImmediatelyOnItsTask() async {
        let watch = TailWatch()
        watch.leave()
        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertFalse(watch.following, "a real departure settles after the debounce")

        watch.arrive()
        XCTAssertFalse(watch.handAway, "arrival lowers the yank guard synchronously")
        // The write itself is deferred one hop off the KVO stream.
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertTrue(watch.following, "back at the tail, following resumes")
        XCTAssertEqual(watch.newSinceLeaving, 0, "the pill's count resets on return")
    }

    /// Content sliding away with no nameable act — a rotation, a keyboard
    /// reshape — departs *without* raising the yank guard: for the debounce
    /// window an arriving event may still follow, and after it the pill
    /// takes over. (Expanding a message is nameable and goes through
    /// `leave()`.)
    func testDepartAloneLeavesTheYankGuardDown() async {
        let watch = TailWatch()
        watch.depart()
        XCTAssertFalse(watch.handAway, "no hand moved; the guard stays down")
        XCTAssertTrue(watch.following, "inside the debounce the follow persists")
        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertFalse(watch.following, "then the departure lands and the pill shows")
    }

    /// `isAway` is what the follow guard and the "N new" counter share, and
    /// its whole point is the debounce window: hand away but not yet
    /// settled must already count as away — events there used to both yank
    /// the reader and go uncounted.
    func testIsAwayCoversTheDebounceWindowNotJustTheSettledFlag() async {
        let watch = TailWatch()
        XCTAssertFalse(watch.isAway, "at the tail, following, guard down")

        watch.leave()
        XCTAssertTrue(
            watch.isAway,
            "inside the debounce — following still true — the reader is away")

        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertTrue(watch.isAway, "and settled away stays away")

        watch.arrive()
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(watch.isAway, "back at the tail on every axis")
    }

    /// The transient a fresh append causes — depart, then the follow-scroll's
    /// arrive — must resolve to following, with no pill flicker in between.
    func testAnAppendTransientNeverShowsThePill() async {
        let watch = TailWatch()
        watch.depart()
        try? await Task.sleep(for: .milliseconds(100))
        watch.arrive()
        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertTrue(
            watch.following,
            "an arrive inside the debounce cancels the departure outright")
    }
}
