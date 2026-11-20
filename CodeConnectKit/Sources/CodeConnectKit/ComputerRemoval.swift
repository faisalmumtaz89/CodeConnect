/// Removing a paired computer from this device, in the words both apps ask with.
public enum ComputerRemoval {
    public static let question = "Remove this computer?"
    public static let action = "Remove"

    /// What removing forgets, and how the computer comes back: from Add a computer while another computer on the
    /// same Tailscale account lists it, otherwise by a new code, scanned on the iPhone and entered on a Mac.
    public static func sentence(name: String?, on device: HostDevice = .current) -> String {
        let add = device == .mac ? "Add a Computer" : "Add a computer"
        let back = device == .mac ? "enter it here" : "scan it"
        return "This \(device.name) forgets \(name ?? "this computer"). To use it again, open \(add), which lists it "
            + "while this \(device.name) is connected to another computer on your Tailscale account, or show a new "
            + "code on it and \(back) (codeconnect.sh/support shows how)."
    }
}
