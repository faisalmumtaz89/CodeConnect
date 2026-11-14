import Foundation

/// What a computer's row in the sidebar shows of its latest CPU, memory and disk sample: each figure the sample has,
/// as a share of its whole for the row's meters, and the same figures in words for the row's hover. A figure the
/// computer could not read is left out, never shown as 0%.
struct MachineLoad {
    /// The share of all cores busy, from 0 to 1.
    let cpu: Double?
    /// The share of memory in use, and of the disk holding the daemon user's home folder, from 0 to 1.
    let memory: Double?
    let disk: Double?
    /// "CPU 38% · RAM 71/192 GB · Disk 1.4/4 TB", naming only the figures there.
    let line: String

    /// Nil when the sample has no figure at all. Memory counts in binary gigabytes, as macOS states memory, and disk
    /// in decimal ones, as Finder states disks.
    init?(cpu: Double?, memoryUsed: UInt64?, memoryTotal: UInt64?, diskUsed: UInt64?, diskTotal: UInt64?) {
        let memory = Self.figure(memoryUsed, memoryTotal, gigabyte: 1_073_741_824)
        let disk = Self.figure(diskUsed, diskTotal, gigabyte: 1_000_000_000)
        var words: [String] = []
        if let cpu { words.append("CPU \(Self.percent(cpu))") }
        if let memory { words.append("RAM \(memory.words)") }
        if let disk { words.append("Disk \(disk.words)") }
        guard !words.isEmpty else { return nil }
        self.cpu = cpu
        self.memory = memory?.share
        self.disk = disk?.share
        line = words.joined(separator: " · ")
    }

    /// "38%".
    static func percent(_ share: Double) -> String { "\(Int((share * 100).rounded()))%" }

    /// A figure the sample has both halves of: its share, and its words in the total's unit, "71/192 GB" or
    /// "1.4/4 TB", as the approved design writes them.
    private static func figure(_ used: UInt64?, _ total: UInt64?, gigabyte: Double) -> (share: Double, words: String)? {
        guard let used, let total else { return nil }
        let (usedGB, totalGB) = (Double(used) / gigabyte, Double(total) / gigabyte)
        let words =
            totalGB >= 1000
            ? String(format: "%g/%g TB", (usedGB / 100).rounded() / 10, (totalGB / 100).rounded() / 10)
            : "\(Int(usedGB.rounded()))/\(Int(totalGB.rounded())) GB"
        return (Double(used) / Double(total), words)
    }
}
