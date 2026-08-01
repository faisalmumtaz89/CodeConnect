import Foundation

/// **The resolution an age is printed at, as it grows.**
///
/// Every age on this screen coarsens as it gets older, because that is how ages
/// read: `12s`, then `4m`, then `2h`, then `3d`. So the string a view is showing
/// is constant for a stretch that is knowable in advance — a row reading `21h`
/// cannot say anything else for the next hour — and that is the fact this type
/// exists to make computable.
struct AgeScale: Hashable, Sendable {
    /// While the age is below `below` seconds, the printed value changes every
    /// `step` seconds.
    struct Step: Hashable, Sendable {
        let below: TimeInterval
        let step: TimeInterval
    }

    let steps: [Step]
    /// The step in force once the age is past every threshold.
    let coarsest: TimeInterval

    /// A printed `Format.age` — `12s` · `4m` · `2h` · `3d` — **together with the
    /// `Format.spokenAge` that accompanies it**.
    ///
    /// The two coarsen differently: `3d` will not change for a day, but the
    /// spoken form never coarsens past hours, so it is still moving. The scale
    /// follows the finer of the two, because a row must not tell the eye and
    /// VoiceOver different things about the same fact.
    static let age = AgeScale(
        steps: [
            Step(below: 60, step: 1),
            Step(below: 3600, step: 60),
        ],
        coarsest: 3600)

    /// What `CCWaitClock` prints: `12s` · `4m12s` · `2h04m` · `3d02h`. Two units
    /// rather than one, so the seconds place survives all the way to an hour —
    /// which is why a row that is *waiting on a human* is the one row that
    /// genuinely earns a tick every second.
    static let clock = AgeScale(
        steps: [
            Step(below: 3600, step: 1),
            Step(below: 86400, step: 60),
        ],
        coarsest: 3600)

    /// The finest step this scale can print. The floor on any wait.
    var finest: TimeInterval { steps.first?.step ?? coarsest }

    func step(forAge age: TimeInterval) -> TimeInterval {
        for candidate in steps where age < candidate.below { return candidate.step }
        return coarsest
    }
}

/// **One view's clock**: the timestamp it counts from, and the resolution it
/// prints at.
///
/// Hashable as a unit so `.task(id:)` restarts the wait when *either* changes —
/// a row whose newest event arrives, or one that goes from counting a wait in
/// seconds to reporting an age in hours. Restarting on the timestamp alone
/// would leave a row that had changed bands sleeping against its old deadline.
struct AgeClock: Hashable, Sendable {
    let since: Date
    let scale: AgeScale
}

/// **When a rendered age stops being true.**
///
/// `AppModel.now` ticks once a second so that every age on screen stays honest,
/// and every view that merely *displays* an age was invalidated by it. On the
/// owner's own fleet — 45 sessions, of which 27 draw — that meant the whole list
/// was rebuilt and re-sorted once a second, and roughly fifty row bodies were
/// re-evaluated, to redraw strings like `21h` that change once an hour.
/// Measured on that fleet, sitting untouched: **14.2ms of main-thread work every
/// second**, arriving in one burst. A 120Hz frame is due every 8.3ms, so the
/// burst could not fit in one and the list stuttered under a finger.
///
/// The fix is not a slower clock — that would make the ages less true, which on
/// this app is the one thing worse than a dropped frame. It is to wake each view
/// at the instant *its own* string changes, and not before. A row showing `21h`
/// sleeps for an hour; a row counting a wait still ticks every second, because
/// that one really is changing.
enum AgeTick {
    /// The next instant at which an age of `date`, printed on `scale`, reads
    /// differently.
    ///
    /// Anchored to `date` rather than to the wall clock. Two rows half a second
    /// out of phase tick over at their own half-seconds, and expressing that is
    /// exactly what one shared clock cannot do: a global minute tick would show
    /// `4m` for a thing that had been `5m` old for fifty-nine seconds, which is
    /// the app claiming something is fresher than it is.
    static func nextChange(since date: Date, now: Date, scale: AgeScale) -> Date {
        // Negative when the Mac's clock is ahead of this phone's. `Format.age`
        // clamps that to `0s`, so the next change really is one step after the
        // timestamp itself, and sleeping until then is correct rather than lazy.
        let age = max(0, now.timeIntervalSince(date))
        let step = scale.step(forAge: age)
        let elapsed = (age / step).rounded(.down)
        let candidate = date.addingTimeInterval((elapsed + 1) * step)
        // Belt and braces: `elapsed + 1` always lands past `now`, but a view
        // that busy-waits is a worse failure than one that updates a beat late.
        guard candidate > now else { return now.addingTimeInterval(scale.finest) }
        return candidate
    }

    /// **The time a view renders an age against**, given the last tick it was
    /// handed.
    ///
    /// Every view that follows a clock holds a `lastTick` and renders through
    /// this. Both halves are load-bearing, and both were learned the hard way:
    ///
    /// **The tick must be read during render.** A `@State` a body never looks at
    /// does not invalidate the view — the tick fires on schedule and the age on
    /// screen silently freezes. Calling this in a body is what makes the read
    /// happen, which is why it takes the tick rather than reaching for it.
    ///
    /// **The value returned is the wall clock, not the tick.** A row that has
    /// slept for an hour holds an hour-old stamp; if a refresh then moves the
    /// fact it is measuring, that fact would be measured against the stale
    /// stamp and print `0s` — the app claiming something is newer than it is,
    /// which is the one failure this screen exists to prevent. `max` keeps the
    /// clock monotonic if the device's own time steps backwards.
    static func renderTime(lastTick: Date, wallClock: Date = Date()) -> Date {
        max(lastTick, wallClock)
    }

    /// Keep `update` supplied with a time that is accurate enough to print, and
    /// sleep the rest of the time.
    ///
    /// One suspended task per visible age, waking at the rate the age actually
    /// changes. That is strictly fewer wakeups than one shared 1Hz timer the
    /// moment anything on screen is older than a minute — and on a real fleet
    /// almost everything is.
    @MainActor
    static func follow(_ clock: AgeClock, update: @MainActor (Date) -> Void) async {
        while !Task.isCancelled {
            let now = Date()
            let due = nextChange(since: clock.since, now: now, scale: clock.scale)
            // Floored only to stop a spin, never to round the wait up: a view
            // that sleeps past its own boundary prints an age one step too
            // young, which is the lie this whole mechanism exists to avoid.
            let wait = max(due.timeIntervalSince(now), 0.02)
            do {
                try await Task.sleep(for: .seconds(wait))
            } catch {
                return  // cancelled
            }
            guard !Task.isCancelled else { return }
            update(Date())
        }
    }
}
