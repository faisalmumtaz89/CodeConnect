import CodeConnectKit
import SwiftUI

/// **A cold launch is the mark's slice** (`LaunchMark`), on the app's ground from
/// the first frame: the system's launch screen is that ground alone
/// (`UILaunchScreen` › `LaunchGround`), and the intro starts from it. The mark
/// holds, breathing, until every computer has caught up or the wait for it ran
/// out (`Machines.waitOutLaunch`); then it leaves on a hold-loop boundary with the
/// first screen already laid out under it, the ground gone so the screen shows
/// between the parts. Nothing changes opacity. Past a second the line under the
/// mark says what the launch waits for, and at the cap which computers never
/// answered. With no computer set up the intro plays and the onboarding is cut to.
/// Reduce Motion: the still mark, then a cut at the same moment.
struct Splash: View {
    /// When the splash appeared, the zero of its time.
    let started: Date
    /// When the fleet caught up, in splash time; nil while it is waiting.
    let settledAt: Double?
    /// No computer is set up: the intro, then a cut.
    let unpaired: Bool
    let onFinished: () -> Void

    @Environment(Machines.self) private var machines
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// When the exit begins, once known.
    private var exitAt: Double? {
        if unpaired { return LaunchMark.introEnd }
        return settledAt.map { LaunchMark.exitStart(settledAt: $0) }
    }

    var body: some View {
        TimelineView(.animation) { context in
            let t = Self.frozenT ?? context.date.timeIntervalSince(started)
            SplashFrame(t: t, exitAt: exitAt, cut: unpaired || reduceMotion, reduceMotion: reduceMotion, line: unpaired ? nil : line)
                .onChange(of: done(t)) { _, done in if done { onFinished() } }
        }
        .ignoresSafeArea()
        .allowsHitTesting(false)
    }

    /// The exit has run, or, cut instead of played, has begun.
    private func done(_ t: Double) -> Bool {
        guard let exitAt else { return false }
        return unpaired || reduceMotion ? t >= exitAt : LaunchMark.finished(t: t, exitAt: exitAt)
    }

    private var line: String {
        let names = machines.models.compactMap(\.machineName)
        if Self.frozenUnreachable { return FleetWords.notAnswering(names) }
        let silent = machines.models.filter { !$0.connection.phase.isConnected }.compactMap(\.machineName)
        if settledAt != nil, !silent.isEmpty { return FleetWords.notAnswering(silent) }
        return FleetWords.connecting(to: names)
    }

    /// Only a cold launch of the app plays it, never a render pass's, whose
    /// screenshots are of the screen under it, unless one asks for a still.
    static var playsAtLaunch: Bool {
        #if DEBUG
            if UserDefaults.standard.bool(forKey: "CC_RENDER_PROBE") { return frozenT != nil }
        #endif
        return true
    }

    #if DEBUG
        /// `-CC_LAUNCH_SETTLED <seconds>`: with `-CC_LAUNCH_T`, when the fleet caught up.
        static var frozenSettledAt: Double? {
            UserDefaults.standard.object(forKey: "CC_LAUNCH_SETTLED") == nil
                ? nil : UserDefaults.standard.double(forKey: "CC_LAUNCH_SETTLED")
        }
        /// `-CC_LAUNCH_T <seconds>`: the splash held at that moment, for a still.
        static var frozenT: Double? {
            UserDefaults.standard.object(forKey: "CC_LAUNCH_T") == nil
                ? nil : UserDefaults.standard.double(forKey: "CC_LAUNCH_T")
        }
        /// `-CC_LAUNCH_UNREACHABLE YES`: the line as at the cap, every computer silent.
        static var frozenUnreachable: Bool { UserDefaults.standard.bool(forKey: "CC_LAUNCH_UNREACHABLE") }
    #else
        static let frozenSettledAt: Double? = nil
        static let frozenT: Double? = nil
        static let frozenUnreachable = false
    #endif
}

/// The splash at one moment: the ground until the exit, the mark, and past a
/// second the line, the same drawing live and in a still.
struct SplashFrame: View {
    let t: Double
    let exitAt: Double?
    /// The exit is a cut, not the mark's own exit.
    let cut: Bool
    let reduceMotion: Bool
    let line: String?

    var body: some View {
        let exiting = exitAt.map { t >= $0 } ?? false
        ZStack {
            if !exiting { CC.color.bg }
            if !(exiting && cut) { LaunchMark(t: t, exitAt: exitAt, reduceMotion: reduceMotion) }
            if !exiting, let line, t >= LaunchMark.lineAfter {
                Text(line)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    .offset(y: 80 + 18 + 9)
            }
        }
    }
}

#if DEBUG
    extension Splash {
        /// `-CC_LAUNCH_FRAMES <folder>`: the splash's frames for the owner's page, 30 a second, each a 402 x 402 pt
        /// crop around the mark on the ground at 3x: a fast launch (caught up at 0.3 s), a slow one (at 4 s), one
        /// whose computer never answered (the cap, 15 s; from 14 s on), and one with no computer set up, as
        /// splash-<case>-<000…>.png.
        @MainActor static func writeFrames(to folder: String, computer: String) {
            let cases: [(String, Double?, Bool, String?, Double)] = [
                ("fast", 0.3, false, FleetWords.connecting(to: [computer]), 0),
                ("slow", 4, false, FleetWords.connecting(to: [computer]), 0),
                ("unreachable", 15, false, nil, 14),
                ("unpaired", nil, true, nil, 0),
            ]
            for (name, settledAt, unpaired, connecting, from) in cases {
                let exitAt = unpaired ? LaunchMark.introEnd : settledAt.map { LaunchMark.exitStart(settledAt: $0) }
                let end = (exitAt ?? 0) + (unpaired ? 0 : LaunchMark.exitLength)
                for index in Int(from * 30)...Int((end * 30).rounded(.up)) {
                    let t = Double(index) / 30
                    let line = unpaired ? nil : (connecting ?? (t >= 15 ? FleetWords.notAnswering([computer]) : FleetWords.connecting(to: [computer])))
                    let renderer = ImageRenderer(
                        content: SplashFrame(t: t, exitAt: exitAt, cut: unpaired, reduceMotion: false, line: line)
                            .frame(width: 402, height: 402)
                            .background(CC.color.bg))
                    renderer.scale = 3
                    guard let data = renderer.uiImage?.pngData() else { continue }
                    try? data.write(
                        to: URL(fileURLWithPath: folder).appendingPathComponent(
                            String(format: "splash-%@-%03d.png", name, index)))
                }
            }
            FileManager.default.createFile(atPath: folder + "/done", contents: Data())
        }
    }
#endif
