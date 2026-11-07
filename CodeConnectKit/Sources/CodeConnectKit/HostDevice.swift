/// The device this build of the Kit runs on, as its sentences name it.
///
/// Chosen when the Kit is compiled, which is once per platform, so there is
/// nothing for an app to set and nothing to forget: every sentence that says
/// where it is being read says it through this one value.
public struct HostDevice: Sendable, Equatable {
    /// The device by its product name: "this iPhone", "CodeConnect iPhone".
    public let name: String
    /// The device as a sentence says it: "this phone", "from the phone".
    public let noun: String
    /// What a high-risk approval asks for first.
    public let biometric: String
    /// What it falls back to.
    public let secret: String

    public static let iPhone = HostDevice(
        name: "iPhone", noun: "phone", biometric: "Face ID", secret: "passcode")
    public static let mac = HostDevice(
        name: "Mac", noun: "Mac", biometric: "Touch ID", secret: "password")

    #if os(macOS)
        public static let current = HostDevice.mac
    #else
        public static let current = HostDevice.iPhone
    #endif
}
