/// Removing a paired computer from this device, in the words both apps ask with.
public enum ComputerRemoval {
    /// Removing computer `name`, whose system is `system` (`AppModel.machineSystem`): what this device forgets, marked
    /// with the computer's own mark, and where it comes back, the place that holds both routes, the owner's Tailscale
    /// list and a code. On a Mac that is Add Computer; on the iPhone, Settings, where Add a computer opens the camera
    /// and the Tailscale list is a section of its own.
    public static func ask(name: String?, system: String?, on device: HostDevice = .current) -> AskWords {
        let computer = name ?? "this computer"
        return AskWords(
            title: "Remove \(computer)?",
            items: [
                .init(mark: system == "Linux" ? "server.rack" : "desktopcomputer",
                      words: "This \(device.name) forgets \(computer)")
            ],
            note: device == .mac ? "You can add it again with Add Computer." : "You can add it again from Settings.",
            cancel: "Cancel", confirm: "Remove")
    }
}
