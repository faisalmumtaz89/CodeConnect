import Foundation
import os

/// The app installs CodeConnect itself: the binaries it carries (`Contents/Helpers`) go into the
/// CodeConnect folder's `bin`, and the existing `codeconnect daemon install` starts the daemon and keeps it
/// running at login. One updater, the app's: when the app's binaries differ from those installed, they
/// are copied again and the daemon reinstalled; sessions survive it (they live in tmux). Their notices go
/// beside them, as every release archive carries them (`Contents/Resources`).
struct Installer {
    static let binaries = ["ccd", "codeconnect", "cc-hook", "cc-tmux"]
    static let notices = ["LICENSE", "THIRD_PARTY_NOTICES"]
    /// A copy or a `daemon install` that failed, as the checklist says it. The system's own words go to the log.
    static let failed = "CodeConnect couldn't set up its background service. Choose Try Again."
    static let log = Logger(subsystem: "com.codeconnect.mac", category: "setup")

    /// The CodeConnect folder, as the binaries resolve it (`protocol::root_dir`).
    let root: URL
    /// The home whose `.zprofile` carries the PATH block.
    let home: URL
    let helpers: URL
    let resources: URL

    var bin: URL { root.appendingPathComponent("bin") }

    /// For this launch, or nil when it may not install: a test launch installs only into a rig of its
    /// own — a CodeConnect folder it was given and a test label for its daemon's job — so it can never
    /// write the owner's binaries.
    static func forThisLaunch() -> Installer? {
        let environment = ProcessInfo.processInfo.environment
        #if DEBUG
            if ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("-CC_") }) {
                guard environment["CODECONNECT_HOME"] != nil,
                    environment["CODECONNECT_LAUNCHD_LABEL"]?.hasPrefix("com.codeconnect.ccd.test-") == true
                else { return nil }
            }
        #endif
        let home = environment["HOME"].map { URL(fileURLWithPath: $0) }
            ?? FileManager.default.homeDirectoryForCurrentUser
        let root = environment["CODECONNECT_HOME"].map { URL(fileURLWithPath: $0) }
            ?? home.appendingPathComponent(".codeconnect")
        return Installer(
            root: root, home: home,
            helpers: Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers"),
            resources: Bundle.main.bundleURL.appendingPathComponent("Contents/Resources"))
    }

    /// Whether this build carries all four binaries.
    var bundled: Bool {
        Self.binaries.allSatisfy { FileManager.default.isExecutableFile(atPath: helpers.appendingPathComponent($0).path) }
    }

    /// Whether what is installed is what the app carries, by `codeconnect --version`.
    var current: Bool {
        Self.binaries.allSatisfy { FileManager.default.isExecutableFile(atPath: bin.appendingPathComponent($0).path) }
            && Self.version(of: bin.appendingPathComponent("codeconnect"))
                == Self.version(of: helpers.appendingPathComponent("codeconnect"))
    }

    /// Copies the binaries and their notices, then starts the daemon; nothing more when what is installed is
    /// current and answers. Either way the app owns `bin` from now on: its marker stands `codeconnect update`
    /// down. Returns what went wrong, in a sentence, or nil; the error or the CLI's output behind it is logged.
    func install() -> String? {
        guard bundled else { return "This build of CodeConnect doesn't carry its background service." }
        let wasCurrent = current
        do {
            if !wasCurrent {
                try FileManager.default.createDirectory(at: bin, withIntermediateDirectories: true)
                for name in Self.binaries {
                    let target = bin.appendingPathComponent(name)
                    // Replaced, never written over: macOS caches a binary's signature by its inode, and
                    // an overwritten one is killed on its next start (mac/install.sh).
                    try? FileManager.default.removeItem(at: target)
                    try FileManager.default.copyItem(at: helpers.appendingPathComponent(name), to: target)
                }
                for name in Self.notices {
                    let target = bin.appendingPathComponent(name)
                    try? FileManager.default.removeItem(at: target)
                    try FileManager.default.copyItem(at: resources.appendingPathComponent(name), to: target)
                }
            }
            try Bundle.main.bundlePath.write(
                to: bin.appendingPathComponent(".installed-by-app"), atomically: true, encoding: .utf8)
        } catch {
            Self.log.error("Installing into bin failed: \(String(describing: error), privacy: .public)")
            return Self.failed
        }
        if wasCurrent, LocalDaemon.answers(root: root) { return nil }
        let (status, output) = Self.run(bin.appendingPathComponent("codeconnect"), ["daemon", "install"])
        guard status == 0 else {
            Self.log.error("codeconnect daemon install exited \(status): \(output, privacy: .public)")
            return Self.failed
        }
        return nil
    }

    static func version(of binary: URL) -> String? {
        let (status, output) = run(binary, ["--version"])
        return status == 0 ? output.trimmingCharacters(in: .whitespacesAndNewlines) : nil
    }

    /// Runs a program to its end with this app's environment, returning its exit status and output.
    @discardableResult
    static func run(_ program: URL, _ arguments: [String]) -> (Int32, String) {
        let process = Process()
        process.executableURL = program
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        process.standardInput = FileHandle.nullDevice
        do { try process.run() } catch { return (-1, error.localizedDescription) }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return (process.terminationStatus, String(decoding: data, as: UTF8.self))
    }
}
