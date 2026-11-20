/// Removing a paired computer from this device, in the words both apps ask with.
public enum ComputerRemoval {
    public static let question = "Remove this computer?"
    public static let action = "Remove"

    /// What removing forgets, and where the computer comes back: the place that holds both routes, the owner's
    /// Tailscale list and a code. On a Mac that is Add a Computer; on the iPhone, Settings, where Add a computer
    /// opens the camera and the Tailscale list is a section of its own.
    public static func sentence(name: String?, on device: HostDevice = .current) -> String {
        let place = device == .mac ? "Add a Computer" : "Settings"
        return "This \(device.name) forgets \(name ?? "this computer"). You can add it again from \(place)."
    }
}
