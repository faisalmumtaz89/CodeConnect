import AppKit
import CodeConnectKit
import SwiftUI

/// A cold launch's splash, inside the main window over the app from the window's first frame: the Kit's mark (the
/// slice) on the window's ground, the app laid out under it. The intro plays, the mark breathes until the fleet has
/// caught up (the Kit's `waitOutLaunch`: 3 s for a silent computer, 15 s at most), and on the next loop boundary the
/// bands slide away with the ground already gone, so the app shows between them. Past a second, one quiet line under
/// the mark says where it is connecting, or, when the wait ran out, which computers didn't answer. With no computer
/// set up there is nothing to wait for: the intro, then a cut (to Set Up). Reduce Motion: the still mark, then a cut.
/// Nothing changes opacity. Only a cold launch plays it: a window reopened later opens on the app.
struct LaunchSurface: View {
    /// Whether the splash is over for this process, for what waits on it (Set Up's sheet).
    @Observable @MainActor final class State {
        var over = false
    }
    @MainActor static let state = State()

    @Environment(Machines.self) private var machines
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @SwiftUI.State private var start = Date()
    /// When the fleet caught up, in the splash's own time.
    @SwiftUI.State private var settledAt: Double?
    /// The computers still silent when the wait ran out.
    @SwiftUI.State private var silent: [String] = []

    var body: some View {
        if !Self.state.over, shows {
            TimelineView(.animation(paused: frozen)) { context in
                frame(at: time(context.date))
            }
            .ignoresSafeArea()
            .allowsHitTesting(exitAt == nil)
            .onChange(of: machines.launchSettled, initial: true) { _, settled in
                if settled, settledAt == nil { settle(at: time(Date())) }
                #if DEBUG
                    // A render's given settle time stands for the fleet's.
                    if RenderWindow.launchSettle != nil, settledAt == nil { settle(at: 0) }
                #endif
            }
            .task(id: exitAt) {
                guard let exitAt else { return }
                let end = cuts ? exitAt : exitAt + LaunchMark.exitLength
                // A still's clock doesn't run: it is over only if drawn past the end.
                if frozen {
                    if time(Date()) >= end { Self.state.over = true }
                    return
                }
                try? await Task.sleep(for: .seconds(max(0, end - time(Date()))))
                Self.state.over = true
            }
            #if DEBUG
                .task { if RenderWindow.launchRecord != nil { await record() } }
            #endif
        }
    }

    @ViewBuilder
    private func frame(at t: Double) -> some View {
        let exiting = exitAt.map { t >= $0 } ?? false
        ZStack {
            if !exiting { Theme.window }
            if !(cuts && exiting) {
                LaunchMark(t: t, exitAt: exitAt, reduceMotion: reduceMotion)
            }
            if !exiting, t >= LaunchMark.lineAfter, let line {
                Text(line).font(Theme.ui(12.5)).foregroundStyle(Theme.text3)
                    // Under the 160 pt mark, 18 pt clear of it.
                    .offset(y: 80 + 18 + 8)
            }
        }
    }

    /// No computer set up to wait for, or Reduce Motion: the splash ends by a cut, not the exit's slide.
    private var cuts: Bool { reduceMotion || !machines.showsFleet }

    /// When the exit begins: with no computer, at the intro's end; else on the loop boundary after the fleet caught up.
    private var exitAt: Double? {
        if !machines.showsFleet { return LaunchMark.introEnd }
        return settledAt.map { LaunchMark.exitStart(settledAt: $0) }
    }

    private var line: String? {
        let names = machines.models.compactMap(\.machineName)
        guard !names.isEmpty else { return nil }
        return silent.isEmpty ? FleetWords.connecting(to: names) : FleetWords.notAnswering(silent)
    }

    private func settle(at t: Double) {
        settledAt = t
        silent = machines.models.filter { !$0.connection.phase.isConnected }.compactMap(\.machineName)
        #if DEBUG
            if let given = RenderWindow.launchSettle {
                settledAt = given
                silent = RenderWindow.launchUnreachable ? Array(machines.models.compactMap(\.machineName).prefix(1)) : []
            }
        #endif
    }

    private func time(_ date: Date) -> Double {
        #if DEBUG
            if let given = RenderWindow.launchT ?? recorded { return given }
        #endif
        return date.timeIntervalSince(start)
    }

    // MARK: Renders

    /// Whether this launch shows the splash: every launch but a render that didn't ask for it.
    private var shows: Bool {
        #if DEBUG
            if RenderWindow.isRendering || RenderWindow.launchRecord != nil { return RenderWindow.showsLaunch }
        #endif
        return true
    }

    /// A still at `-CC_LAUNCH_T`, or a recording stepping the time itself: the clock doesn't run.
    private var frozen: Bool {
        #if DEBUG
            return RenderWindow.launchT != nil || RenderWindow.launchRecord != nil
        #else
            return false
        #endif
    }

    #if DEBUG
        @SwiftUI.State private var recorded: Double?

        /// Every frame, 30 a second, from `-CC_LAUNCH_RECORD_FROM` until the splash is gone, the window photographed
        /// into launch-<000…>.png in `-CC_LAUNCH_RECORD`'s folder; then the app quits.
        private func record() async {
            guard let folder = RenderWindow.launchRecord else { return }
            var index = 0
            var t = RenderWindow.launchRecordFrom
            while true {
                recorded = t
                try? await Task.sleep(for: .milliseconds(60))
                guard let window = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil }),
                    let view = window.contentView, let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
                else { return }
                view.cacheDisplay(in: view.bounds, to: rep)
                let name = String(format: "launch-%03d.png", index)
                try? rep.representation(using: .png, properties: [:])?
                    .write(to: URL(fileURLWithPath: folder).appendingPathComponent(name))
                if LaunchMark.finished(t: t, exitAt: exitAt) || (cuts && exitAt.map { t >= $0 } == true) { break }
                index += 1
                t = RenderWindow.launchRecordFrom + Double(index) / 30
            }
            NSApp.terminate(nil)
        }
    #endif
}
