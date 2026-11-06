import UserNotifications

/// What a model needs from whatever gets this device a push token: ask for
/// permission, ask for a token, re-read the permission, and hear both answers.
///
/// **One per model.** Each model sets the single `onToken` and
/// `onAuthorization` of its own registrar, so a registrar shared by two
/// models would hand every token to the last one wired. The app supplies a
/// factory, and `Machines` makes one registrar from it for each model.
@MainActor
public protocol PushRegistrar: AnyObject {
    /// Called with the token, lowercase hex, and its APNs environment.
    var onToken: ((String, String) -> Void)? { get set }
    /// Called with what the user actually decided.
    var onAuthorization: ((Bool) -> Void)? { get set }
    /// Ask for permission, then register for a token if it was granted.
    func requestAndRegister()
    /// Register for a token without asking.
    func registerForRemoteNotifications()
    /// The current permission, re-read.
    func authorizationStatus() async -> UNAuthorizationStatus
}
