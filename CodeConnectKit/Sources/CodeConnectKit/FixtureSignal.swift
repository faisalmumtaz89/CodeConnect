import Foundation

#if DEBUG
    /// A UI test's cue to a held fixture. The runner names a directory in the launch environment
    /// (`CC_FIXTURE_SIGNALS`) and writes a file into it, named for the hold, at the moment its test needs the held
    /// thing to happen; the app waits for that file. A test then owns the moment instead of racing a clock, which a
    /// loaded Mac stretches.
    ///
    /// The wait polls the file system on a sleeping task. It reads and writes no observed state, so a held fixture adds
    /// no body pass for the render probes to count. Once written, a signal stays: every later wait for it passes at
    /// once. The app answers each signal it takes with `<name>.taken`, so the runner knows the hold has let go before
    /// it goes on. With no directory named, nothing is held.
    enum FixtureSignal {
        static func wait(_ name: String) async {
            guard let directory = ProcessInfo.processInfo.environment["CC_FIXTURE_SIGNALS"] else { return }
            let path = (directory as NSString).appendingPathComponent(name)
            while !FileManager.default.fileExists(atPath: path) {
                try? await Task.sleep(for: .milliseconds(50))
            }
            FileManager.default.createFile(atPath: path + ".taken", contents: nil)
        }
    }
#endif
