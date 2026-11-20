/// Removing a paired computer from this device, in the words both apps ask with.
public enum ComputerRemoval {
    public static let question = "Remove this computer?"
    public static let action = "Remove"

    /// What removing forgets, and how the computer comes back: the iPhone scans a new code, a Mac has it entered.
    public static func sentence(name: String?, on device: HostDevice = .current) -> String {
        let back = device == .mac ? "enter it here" : "scan it"
        return "This \(device.name) forgets \(name ?? "this computer"). To use it again, show a new code on it and "
            + "\(back) (codeconnect.sh/support shows how)."
    }
}
