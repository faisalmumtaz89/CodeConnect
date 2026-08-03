import SwiftUI

/// One open swipe row per screen, and a way to shut it.
///
/// `List`'s `.swipeActions` closes an open row when the list scrolls, when
/// another row opens, and when you navigate away. This hand-built swipe has to
/// say those rules out loud, and this is where they live. The screen owns one
/// coordinator; every removable row reports to it.
///
/// **`FleetView`'s body must never read `signal`.** Rows observe it; the screen
/// only calls methods. That split is what lets a scroll close a row without
/// re-rendering the fleet — the exact scroll-driven rebuild this screen has
/// been burned by before.
@MainActor
@Observable
final class CCSwipeCloseCoordinator {
    /// Bumped to tell rows to close. Only rows read it.
    private(set) var signal = 0
    /// The row exempt from the current pulse: the one that just opened.
    @ObservationIgnored private(set) var except: String?
    /// The row currently open, if any. Bookkeeping, not observable: each row
    /// keeps its own open flag, so the only observable state here is `signal` —
    /// a pulse invalidates every enabled row once, which is bounded, rare (an
    /// exclusivity hand-off or a screen-level close), and cheap next to the
    /// fleet rebuild the split exists to avoid.
    @ObservationIgnored private(set) var openRow: String?
    /// A row slid open. Closes any other open row in the same breath —
    /// exclusivity, the rule `.swipeActions` has natively.
    func noteOpened(_ id: String) {
        if openRow != id, openRow != nil {
            except = id
            signal &+= 1
        }
        openRow = id
    }

    /// A row shut itself (the user swiped it closed, or removal succeeded).
    func noteClosed(_ id: String) {
        if openRow == id { openRow = nil }
    }

    /// Navigation, a sheet, anything that takes the screen over: shut the row.
    func closeAll() {
        guard openRow != nil else { return }
        openRow = nil
        except = nil
        signal &+= 1
    }
}

/// Swipe a row aside to reveal a single destructive action.
///
/// **Hand-built because the fleet is not a `List`.** `.swipeActions` exists only
/// inside `List`, and the fleet is a `ScrollView` over a `LazyVStack` — a choice
/// made for the row measurement and banding this screen needs. Attaching
/// `.swipeActions` here compiles and silently does nothing, which is the worst
/// available outcome.
///
/// **A nested scroll view, not a `DragGesture`, and that is the whole design.**
/// The first version of this used a `DragGesture` with a horizontal-intent gate:
/// ignore the drag until it is clearly sideways, then move the row. It reads as
/// correct and it is not, because a `DragGesture` has no direction — it
/// recognises at `minimumDistance` whichever way the finger went, and the gate
/// runs in `onChanged`, *after* recognition. Declining there stops the row
/// moving but cannot fail an already-recognised gesture or hand the touch back,
/// so the fleet stopped scrolling over exactly the rows that offer removal.
/// Measured on the simulator: a drag beginning on a live row scrolled the list
/// 250pt, the identical drag beginning on a removable row moved nothing.
/// `simultaneousGesture` does not fix it either — that was measured too.
///
/// Two nested scroll views have no such problem: direction locking between a
/// vertical parent and a horizontal child is UIKit's own behaviour, decided
/// below SwiftUI. The reveal, the rubber-banding at the stop, and the
/// swipe-back-to-close all come from the platform rather than from arithmetic
/// here. What is left to write is where it should come to rest.
/// **On tap-highlight latency, examined and accepted.** A nested `UIScrollView`
/// waits out `delaysContentTouches` before delivering a press, so an ended
/// row's highlight *may* light a beat later than a live row's. Three facts
/// closed the question: the delay is the platform's own scroll-intent
/// arbitration, which is load-bearing here (see the header above); no
/// instrument available to this project can demonstrate it (a screenshot
/// mid-press post-dates any plausible delay, and SwiftUI exposes no
/// `delaysContentTouches` switch to A/B); and what actually matters — taps
/// activating, rows navigating — is proven by the UI tests. A UIKit
/// introspection workaround would trade a proven arbitration for an
/// undemonstrated cosmetic.
struct CCSwipeToRemove<Content: View>: View {
    /// Hidden entirely when false, so a daemon that cannot delete offers
    /// nothing rather than something that fails. Also the reason a live row
    /// costs nothing: it is not wrapped in a scroll view at all.
    var isEnabled: Bool
    var title: String
    /// Returns `nil` when it worked, or the reason it did not.
    ///
    /// **A refusal has to come back here, and this signature is why.** The first
    /// version returned `Void`: the Mac's answer — still running, could not,
    /// a request that timed out — was discarded at the call site and the row
    /// simply closed. `DaemonProfile` argues, about the capability gate, that "a
    /// swipe that appeared and then failed would be worse than no swipe, because
    /// the row would stay and the user would not know why". That was exactly the
    /// shipped behaviour for every answer except success.
    var action: () async -> String?
    /// This row's stable identity with the coordinator — the session key.
    var rowID: String = ""
    /// The screen's close rules. Optional so the component stands alone, but
    /// the fleet always passes one.
    var coordinator: CCSwipeCloseCoordinator?
    @ViewBuilder var content: () -> Content

    @State private var busy = false
    /// This row's own account of being open. Row-local so the catcher overlay
    /// invalidates only this row, and so a close pulse can be ignored by rows
    /// that were never open.
    @State private var openLocal = false
    /// When a programmatic close was issued. The settle callback fires for that
    /// animated scroll too, at whatever offset the row started from — reporting
    /// "open" for a row on its way shut. Time-bounded rather than a flag: a
    /// flag that only a later callback could clear jammed permanently when that
    /// callback never came, discarding every future open. Within the animation
    /// window a settle(true) is the stale echo; after it, it is the user.
    @State private var closingSince: Date?
    /// Held rather than flashed. A message that clears itself is one the user
    /// can miss by looking away, and the row staying open is itself the signal
    /// that nothing happened to it.
    @State private var refusal: String?
    @Environment(\.dynamicTypeSize) private var typeSize

    /// How far the row slides. Scaled, because the label inside grows: a fixed
    /// width at AX5 is a button with its word cut in half.
    private var revealWidth: CGFloat { typeSize.isAccessibilitySize ? 168 : 112 }

    /// The closed position, as a scroll target.
    private static var closedID: Int { 0 }

    var body: some View {
        if isEnabled {
            ScrollViewReader { proxy in
                ScrollView(.horizontal) {
                    HStack(spacing: 0) {
                        content()
                            // The row keeps the full width it would have had;
                            // the button lives past the edge until asked for.
                            .containerRelativeFrame(.horizontal)
                            .id(Self.closedID)
                            // While open, a tap on the row shuts it rather than
                            // opening the session — `.swipeActions` behaviour,
                            // and the difference between a destructive control
                            // left armed under a stray tap and one that stands
                            // down.
                            .overlay {
                                if openLocal {
                                    Color.clear
                                        .contentShape(Rectangle())
                                        .onTapGesture { close(proxy) }
                                        .accessibilityHidden(true)
                                }
                            }
                        removeButton(proxy)
                    }
                }
                .onChange(of: coordinator?.signal ?? 0) {
                    // A pulse names at most one exempt row: the one that just
                    // opened. Everyone else that is actually open shuts; rows
                    // that never were have nothing to do and no state to touch.
                    if openLocal, coordinator?.except != rowID { close(proxy) }
                }
                .scrollIndicators(.hidden)
                // Without this the row rests wherever the finger left it. The
                // behavior is also the open-state instrument: it is the one
                // code that knows, at the moment a drag lets go, which of the
                // two positions the row is about to rest at. (An offset
                // preference was tried first and measured dead — a geometry
                // reader inside this scroll view reports its frame once and
                // never again as the content moves.)
                .scrollTargetBehavior(
                    RevealBehavior(reveal: revealWidth) { open in
                        if open {
                            // The stale echo of a programmatic close reports
                            // "open" from the old offset within the close
                            // animation; a report after that window is a real
                            // re-open by the user.
                            if let since = closingSince, Date().timeIntervalSince(since) < 0.4 {
                                return
                            }
                            closingSince = nil
                            openLocal = true
                            coordinator?.noteOpened(rowID)
                        } else {
                            closingSince = nil
                            openLocal = false
                            refusal = nil
                            coordinator?.noteClosed(rowID)
                        }
                    })
                // `revealWidth` is scaled, so changing type size while a row is
                // open moves the open position out from under the offset: the
                // row is left resting at 112 when open is now 168, which is
                // exactly the part-way state `RevealBehavior` exists to make
                // impossible. Shut it and let the user swipe again at the new
                // size.
                .onChange(of: typeSize) { close(proxy) }
            }
        } else {
            content()
        }
    }

    private func close(_ proxy: ScrollViewProxy) {
        closingSince = Date()
        openLocal = false
        refusal = nil
        coordinator?.noteClosed(rowID)
        withAnimation(CC.motion.small) {
            proxy.scrollTo(Self.closedID, anchor: .leading)
        }
    }

    private func removeButton(_ proxy: ScrollViewProxy) -> some View {
        Button {
            guard !busy else { return }
            busy = true
            refusal = nil
            Task {
                let reason = await action()
                busy = false
                refusal = reason
                guard let reason else {
                    // On success the row is already gone and this does nothing.
                    // It matters for the answers that leave the row in place.
                    close(proxy)
                    return
                }
                // The row is open and the label now reads the reason, which a
                // screen reader has no way to notice on its own: nothing moved
                // and focus did not change.
                UIAccessibility.post(notification: .announcement, argument: reason)
            }
        } label: {
            Text(busy ? "Removing…" : (refusal ?? title))
                .ccType(CC.type.footnote.weight(.semibold))
                .foregroundStyle(CC.color.onAccent)
                .multilineTextAlignment(.center)
                // The reason is longer than the word it replaces and the reveal
                // cannot grow — the row beside it owns that width. Shrinking is
                // the only honest option: truncating a refusal hides the half
                // that says what to do about it.
                .minimumScaleFactor(0.6)
                .frame(width: revealWidth)
                .frame(maxHeight: .infinity)
                .contentShape(Rectangle())
        }
        // Red, and this is the one place on this screen that earns it: the row
        // is being destroyed. Every other use of `danger` in the app is a
        // failure or a destructive control, never emphasis.
        .background(CC.color.danger)
        // VoiceOver reaches removal by the row's own named action, which works
        // whether or not the row is open. Left visible, it would be a second
        // route to the same thing, reachable only after a gesture a screen
        // reader user has no reason to perform.
        .accessibilityHidden(true)
    }
}

/// Where a released swipe comes to rest: open, or shut, and nothing between.
///
/// `.viewAligned` is the stock behaviour and is wrong here — it aligns a *child*
/// to the container edge, so opening would scroll the row itself off the screen
/// rather than by the width of one button. The two positions this row has are
/// offsets, not views.
private struct RevealBehavior: ScrollTargetBehavior {
    let reveal: CGFloat
    /// Told, on the main actor, which position the released drag settles at.
    /// `@Sendable` because `updateTarget` gives no isolation promise, so the
    /// hop to the main actor below has to carry the closure across.
    let onSettle: @Sendable @MainActor (Bool) -> Void

    init(reveal: CGFloat, onSettle: @escaping @Sendable @MainActor (Bool) -> Void = { _ in }) {
        self.reveal = reveal
        self.onSettle = onSettle
    }

    func updateTarget(_ target: inout ScrollTarget, context: TargetContext) {
        // Past halfway counts as opening it. `target.rect` is already where the
        // platform's own deceleration would land, so a flick that never reached
        // halfway but was thrown hard still opens.
        let open = target.rect.origin.x > reveal / 2
        target.rect.origin.x = open ? reveal : 0
        let report = onSettle
        Task { @MainActor in report(open) }
    }
}
