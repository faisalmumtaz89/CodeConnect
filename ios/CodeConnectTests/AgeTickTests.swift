import SwiftUI
import UIKit
import XCTest

@testable import CodeConnect

/// **The clock the whole app reads ages from.**
///
/// `AgeTick` exists so that a view showing `21h` sleeps for an hour instead of
/// being rebuilt every second by `AppModel.now`. That trade is only safe while
/// two things hold, and neither is visible from a screenshot:
///
///   * **the deadline is exact** — a view that sleeps past its own boundary
///     prints an age one step too young, which is the app claiming something is
///     fresher than it is; and
///   * **the wake actually redraws** — a `@State` mutated on a timer that the
///     body never reads does not invalidate the view at all, so the ticks fire
///     on schedule while the ages on screen quietly freeze.
///
/// The second one shipped. It was found by measurement rather than by a test,
/// which is why `testAWokenClockOnlyRedrawsAnAgeThatItsBodyActuallyReads` is
/// here: it is the regression test for a bug that has already been paid for
/// once.
@MainActor
final class AgeTickTests: XCTestCase {

    /// Exactly representable as a `Double`, so a boundary computed to the
    /// millisecond is not a floating-point argument.
    private let epoch = Date(timeIntervalSince1970: 1_700_000_000)

    /// The whole range an age passes through, with both sides of every
    /// threshold either scale cares about: the second, the minute, the hour and
    /// the day.
    private static let ages: [TimeInterval] = [
        0, 0.5, 1, 29.5, 30, 59, 59.999, 60, 60.001, 61, 119, 120,
        3599, 3599.999, 3600, 3600.001, 3661, 7199, 7200,
        86_399, 86_400, 86_400.001, 90_000, 200_000, 604_800,
    ]

    // MARK: - What the two scales actually print

    /// What a reader *and* VoiceOver get from `Format.age`, together.
    ///
    /// The two coarsen differently — `3d` will not change for a day, but the
    /// spoken form never coarsens past hours — and a row must not tell the eye
    /// and the screen reader different things about the same fact. So the pair
    /// is the unit of truth, and the pair is what the scale has to track.
    private func agePair(since: Date, now: Date) -> String {
        Format.age(since: since, now: now)
            + " / " + Format.spokenAge(now.timeIntervalSince(since))
    }

    /// The same, for `CCWaitClock`: its text and its accessibility label.
    private func clockPair(since: Date, now: Date) -> String {
        CCWaitClock.clock(since: since, now: now)
            + " / " + Format.spokenAge(now.timeIntervalSince(since))
    }

    /// **The promise `nextChange` makes**, asserted against the formatter that
    /// has to keep it: the rendered string is unchanged for every instant before
    /// the deadline, and different at it.
    private func assertDeadlineIsExact(
        age: TimeInterval,
        scale: AgeScale,
        render: (Date, Date) -> String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let now = epoch.addingTimeInterval(age)
        let due = AgeTick.nextChange(since: epoch, now: now, scale: scale)

        XCTAssertGreaterThan(
            due, now,
            "a deadline at or before now is a spin, at age \(age)", file: file, line: line)

        let current = render(epoch, now)
        XCTAssertEqual(
            render(epoch, due.addingTimeInterval(-0.001)), current,
            "woke early at age \(age): the string was still \(current) a millisecond before "
                + "the deadline, so the view was rebuilt for nothing",
            file: file, line: line)
        XCTAssertNotEqual(
            render(epoch, due), current,
            "overslept at age \(age): the view is still printing \(current) at its own "
                + "deadline, which is the app claiming something is fresher than it is",
            file: file, line: line)
    }

    func testAgeScaleWakesExactlyWhenAPrintedAgeStopsBeingTrue() {
        for age in Self.ages {
            assertDeadlineIsExact(age: age, scale: .age, render: agePair(since:now:))
        }
    }

    func testClockScaleWakesExactlyWhenAWaitClockStopsBeingTrue() {
        for age in Self.ages {
            assertDeadlineIsExact(age: age, scale: .clock, render: clockPair(since:now:))
        }
    }

    /// The seconds place survives all the way to an hour on `CCWaitClock`, which
    /// is why a row *waiting on a human* is the one row that earns a tick every
    /// second — and why the same row, once answered, must not.
    func testAWaitingRowTicksEverySecondAndAReportingRowDoesNot() {
        let waiting = AgeTick.nextChange(
            since: epoch, now: epoch.addingTimeInterval(1800), scale: .clock)
        XCTAssertEqual(waiting.timeIntervalSince(epoch.addingTimeInterval(1800)), 1, accuracy: 1e-6)

        let reporting = AgeTick.nextChange(
            since: epoch, now: epoch.addingTimeInterval(1800), scale: .age)
        XCTAssertEqual(
            reporting.timeIntervalSince(epoch.addingTimeInterval(1800)), 60, accuracy: 1e-6,
            "a row printing `30m` has nothing to say for another minute")
    }

    // MARK: - The invariants that stop a view spinning or oversleeping

    /// The wait is always in `(0, step]`: never zero — which would spin — and
    /// never longer than the resolution the view is printing at, which would
    /// leave a stale age on screen.
    func testTheWaitIsNeverZeroAndNeverLongerThanTheStepInForce() {
        for scale in [AgeScale.age, AgeScale.clock] {
            for age in Self.ages {
                let now = epoch.addingTimeInterval(age)
                let wait = AgeTick.nextChange(since: epoch, now: now, scale: scale)
                    .timeIntervalSince(now)
                let step = scale.step(forAge: age)
                XCTAssertGreaterThan(wait, 0, "spin at age \(age)")
                XCTAssertLessThanOrEqual(
                    wait, step + 1e-9,
                    "slept \(wait)s at age \(age) while printing at \(step)s resolution")
            }
        }
    }

    func testStepsChangeOnTheThresholdsTheFormattersUse() {
        XCTAssertEqual(AgeScale.age.step(forAge: 59.999), 1)
        XCTAssertEqual(AgeScale.age.step(forAge: 60), 60)
        XCTAssertEqual(AgeScale.age.step(forAge: 3599.999), 60)
        XCTAssertEqual(AgeScale.age.step(forAge: 3600), 3600)
        XCTAssertEqual(AgeScale.age.step(forAge: 86_400), 3600)
        XCTAssertEqual(AgeScale.age.finest, 1)

        XCTAssertEqual(AgeScale.clock.step(forAge: 3599.999), 1)
        XCTAssertEqual(AgeScale.clock.step(forAge: 3600), 60)
        XCTAssertEqual(AgeScale.clock.step(forAge: 86_399.999), 60)
        XCTAssertEqual(AgeScale.clock.step(forAge: 86_400), 3600)
        XCTAssertEqual(AgeScale.clock.finest, 1)
    }

    /// **Anchored to the age's own timestamp, never to the wall clock.**
    ///
    /// Two rows half a second out of phase tick over at their own half-seconds.
    /// One shared minute tick would show `4m` for a thing that had been `5m` old
    /// for fifty-nine seconds, which is the app claiming something is fresher
    /// than it is.
    func testTwoRowsOutOfPhaseTickOnTheirOwnBoundaries() {
        let other = epoch.addingTimeInterval(0.5)
        let now = epoch.addingTimeInterval(130)

        let first = AgeTick.nextChange(since: epoch, now: now, scale: .age)
        let second = AgeTick.nextChange(since: other, now: now, scale: .age)

        // Each is three whole minutes after *its own* timestamp, so the two land
        // half a second apart — which is the phase difference between the rows,
        // and exactly what one shared clock cannot express.
        XCTAssertEqual(first, epoch.addingTimeInterval(180))
        XCTAssertEqual(second, other.addingTimeInterval(180))
        XCTAssertEqual(second.timeIntervalSince(first), 0.5, accuracy: 1e-6)
    }

    /// The general form of the same rule: every deadline is a whole number of
    /// steps after the timestamp it is measuring, at every age and both scales.
    ///
    /// A deadline anchored to the wall clock instead would drift by whatever the
    /// row's phase happens to be, and the drift is only ever in one direction —
    /// showing an age *younger* than the truth.
    func testEveryDeadlineIsAWholeNumberOfStepsAfterItsOwnTimestamp() {
        for scale in [AgeScale.age, AgeScale.clock] {
            for age in Self.ages {
                let now = epoch.addingTimeInterval(age)
                let step = scale.step(forAge: age)
                let offset = AgeTick.nextChange(since: epoch, now: now, scale: scale)
                    .timeIntervalSince(epoch)
                XCTAssertEqual(
                    offset.truncatingRemainder(dividingBy: step), 0, accuracy: 1e-6,
                    "deadline at age \(age) is not on this timestamp's own \(step)s grid")
            }
        }
    }

    /// `.task(id:)` restarts the wait when *either* half of the clock changes.
    /// Restarting on the timestamp alone would leave a row that had changed
    /// bands — from counting a wait in seconds to reporting an age in hours —
    /// sleeping against its old deadline.
    func testAClockIsIdentifiedByBothItsTimestampAndItsScale() {
        XCTAssertEqual(
            AgeClock(since: epoch, scale: .age), AgeClock(since: epoch, scale: .age))
        XCTAssertNotEqual(
            AgeClock(since: epoch, scale: .age), AgeClock(since: epoch, scale: .clock))
        XCTAssertNotEqual(
            AgeClock(since: epoch, scale: .age),
            AgeClock(since: epoch.addingTimeInterval(1), scale: .age))
    }

    // MARK: - The Mac's clock is ahead of this phone's

    /// A future timestamp is not a bug to be guarded against, it is a Mac whose
    /// clock is a few seconds ahead. `Format.age` clamps it to `0s`, so the next
    /// change really is one step after the timestamp itself — and sleeping until
    /// then is correct rather than lazy.
    func testAFutureTimestampStillYieldsAFutureDeadlineAtEveryScale() {
        for scale in [AgeScale.age, AgeScale.clock] {
            for ahead in [0.25, 5.0, 90.0, 86_400.0, 315_360_000.0] {
                let since = epoch.addingTimeInterval(ahead)
                let due = AgeTick.nextChange(since: since, now: epoch, scale: scale)
                XCTAssertGreaterThan(
                    due, epoch, "a deadline in the past is an immediate spin, \(ahead)s ahead")
                XCTAssertEqual(
                    due, since.addingTimeInterval(scale.finest),
                    "one step after the timestamp, \(ahead)s ahead")
            }
        }
    }

    func testAFutureTimestampReadsAsZeroUntilItsOwnFirstStep() {
        let since = epoch.addingTimeInterval(5)
        let due = AgeTick.nextChange(since: since, now: epoch, scale: .age)

        XCTAssertEqual(Format.age(since: since, now: epoch), "0s")
        XCTAssertEqual(Format.age(since: since, now: since), "0s")
        XCTAssertEqual(Format.age(since: since, now: due.addingTimeInterval(-0.001)), "0s")
        XCTAssertEqual(Format.age(since: since, now: due), "1s")
    }

    // MARK: - The clock a view renders with

    /// **A row waking from an hour's sleep must never print a stale age.**
    ///
    /// The stamp it fell asleep holding is an hour old. If a refresh then moves
    /// the fact it is measuring, measuring that fact against the stale stamp
    /// prints `0s` — the app claiming something is newer than it is, which is
    /// the one failure this screen exists to prevent.
    func testRenderTimeDiscardsAStaleTickSoAWokenRowCannotPrintAFalseAge() {
        let wall = epoch
        let stale = wall.addingTimeInterval(-3600)
        let fact = wall.addingTimeInterval(-180)

        XCTAssertEqual(AgeTick.renderTime(lastTick: stale, wallClock: wall), wall)
        XCTAssertEqual(
            Format.age(since: fact, now: AgeTick.renderTime(lastTick: stale, wallClock: wall)),
            "3m")

        // What the same row would print if it rendered against the stamp it
        // woke holding. This is the failure the `max` is there to prevent, and
        // it is stated here so that removing the `max` fails a test rather than
        // shipping a lie.
        XCTAssertEqual(Format.age(since: fact, now: stale), "0s")
    }

    func testRenderTimeIsMonotonicWhenTheDeviceClockStepsBackwards() {
        let tick = epoch
        XCTAssertEqual(
            AgeTick.renderTime(lastTick: tick, wallClock: epoch.addingTimeInterval(-30)), tick,
            "a clock that steps backwards must not make ages younger")
        XCTAssertEqual(
            AgeTick.renderTime(lastTick: tick, wallClock: epoch.addingTimeInterval(30)),
            epoch.addingTimeInterval(30))
    }

    // MARK: - The bug that shipped

    /// **A `@State` the body never reads does not invalidate the view.**
    ///
    /// This is the defect the previous change shipped and found by measurement:
    /// every tick task fired exactly on schedule, and not one age on screen
    /// advanced, because the state they were writing was never read during
    /// render. The whole mechanism looked like it was working.
    ///
    /// Both views below follow a real `AgeTick` clock. They differ in one line —
    /// whether the body reads the tick — and that line is the difference between
    /// an age that stays true and an age that is frozen while a timer runs
    /// behind it. The reading view is the control: it proves this harness
    /// renders at all, so the other view's silence means what it says.
    func testAWokenClockOnlyRedrawsAnAgeThatItsBodyActuallyReads() async throws {
        BodyLog.reset()

        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first,
            "no window scene: this test has to actually render to mean anything")
        let previousKeyWindow = scene.windows.first { $0.isKeyWindow }
        let window = UIWindow(windowScene: scene)
        let host = UIHostingController(rootView: TickHarness(since: Date()))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previousKeyWindow?.makeKeyAndVisible()
        }
        host.view.layoutIfNeeded()

        // The first pass is not the measurement: SwiftUI is entitled to evaluate
        // a body more than once on the way to a first layout.
        let readsBaseline = BodyLog.count(of: .reads)
        let ignoresBaseline = BodyLog.count(of: .ignores)

        // Both views drew at least once. Without this the "did not redraw"
        // assertion below could be satisfied by a view that never rendered at
        // all, which would make the whole test vacuous.
        XCTAssertGreaterThan(readsBaseline, 0, "the reading view never rendered")
        XCTAssertGreaterThan(ignoresBaseline, 0, "the blind view never rendered")

        // Three seconds at `AgeScale.age`, whose finest step is one second, so
        // three deadlines pass while nothing at all is touched.
        try await Task.sleep(for: .seconds(3.2))
        host.view.layoutIfNeeded()

        // Both clocks really ran. Without this the rest of the test could pass
        // by nothing having happened, which is the failure mode it exists to
        // rule out.
        XCTAssertGreaterThanOrEqual(
            BodyLog.count(of: .readsTick), 2, "the reading view's clock never fired")
        XCTAssertGreaterThanOrEqual(
            BodyLog.count(of: .ignoresTick), 2, "the blind view's clock never fired")

        XCTAssertGreaterThan(
            BodyLog.count(of: .reads), readsBaseline,
            "a view that reads its tick must be rebuilt when the tick moves — "
                + "if this fails, no age in this app is advancing")
        // The consequence, in the terms the user sees: this view is still drawing
        // the string it drew at launch, while its timer has fired three times
        // behind it. That is what shipped.
        XCTAssertEqual(
            BodyLog.count(of: .ignores), ignoresBaseline,
            "a view whose body never reads its tick is not invalidated by writing it — "
                + "the age on screen freezes while the clock keeps firing. If this ever "
                + "starts failing, the mechanism has changed shape and every `lastTick` "
                + "read in the app needs re-checking")
    }
}

// MARK: - Harness

/// Body evaluations and clock ticks, counted. `@MainActor` because SwiftUI
/// bodies are, so no lock is needed and none is pretended.
@MainActor
private enum BodyLog {
    enum Event: Hashable {
        case reads, readsTick, ignores, ignoresTick
    }

    private static var events: [Event: Int] = [:]

    static func record(_ event: Event) { events[event, default: 0] += 1 }
    static func count(of event: Event) -> Int { events[event] ?? 0 }
    static func reset() { events = [:] }
}

/// Two views, one difference: `ReadsItsTick` renders through
/// `AgeTick.renderTime`, which reads the tick; `IgnoresItsTick` writes the same
/// tick on the same schedule and never looks at it.
private struct TickHarness: View {
    let since: Date

    var body: some View {
        VStack {
            ReadsItsTick(since: since)
            IgnoresItsTick(since: since)
        }
    }
}

private struct ReadsItsTick: View {
    let since: Date
    @State private var lastTick = Date()

    private var clock: AgeClock { AgeClock(since: since, scale: .age) }

    var body: some View {
        BodyLog.record(.reads)
        // The read. This is the whole difference.
        return Text(Format.age(since: since, now: AgeTick.renderTime(lastTick: lastTick)))
            .task(id: clock) {
                await AgeTick.follow(clock) {
                    lastTick = $0
                    BodyLog.record(.readsTick)
                }
            }
    }
}

private struct IgnoresItsTick: View {
    let since: Date
    @State private var lastTick = Date()

    private var clock: AgeClock { AgeClock(since: since, scale: .age) }

    var body: some View {
        BodyLog.record(.ignores)
        // The shipped bug, in one line: the age is computed without consulting
        // the state the clock below is writing, so nothing ever invalidates this
        // view and the string it drew at launch is the string it keeps.
        return Text(Format.age(since: since, now: since))
            .task(id: clock) {
                await AgeTick.follow(clock) {
                    lastTick = $0
                    BodyLog.record(.ignoresTick)
                }
            }
    }
}
