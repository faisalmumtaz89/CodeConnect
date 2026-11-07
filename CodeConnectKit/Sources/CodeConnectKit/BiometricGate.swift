import Foundation
import LocalAuthentication

/// The second factor on a HIGH-risk approval.
///
/// The trust brand made physical: a destructive command gets a deliberate hold
/// *and* proof that the person holding the phone is the person who owns it.
///
/// The check is built on a `SecAccessControl` with
/// `[.biometryCurrentSet, .or, .devicePasscode]` rather than the simpler
/// `LAPolicy.deviceOwnerAuthentication`, because `biometryCurrentSet` is the
/// flag that *invalidates when the enrolled set changes*. Adding a face or a
/// finger after the phone is taken must not silently inherit the ability to
/// approve `rm -rf`; with this flag it falls through to the passcode instead.
public enum BiometricGate {
    public enum Outcome: Sendable, Equatable {
        case authenticated
        /// The user dismissed the prompt. Not a failure, and never an approval.
        case cancelled
        /// Authentication ran and said no.
        case failed(String)
        /// This device cannot perform the check at all.
        case unavailable(String)

        public var isAuthenticated: Bool { self == .authenticated }

        /// What to put on screen. Nil when there is nothing to explain.
        public var message: String? {
            switch self {
            case .authenticated: return nil
            case .cancelled: return "Not approved - the check was cancelled."
            case .failed(let reason): return "Not approved - \(reason)"
            case .unavailable(let reason): return reason
            }
        }
    }

    /// The description shown in the system sheet. Deliberately names the command
    /// class: an authentication prompt that does not say what it authorises is a
    /// prompt people learn to clear reflexively.
    public static func reason(for toolName: String) -> String {
        "Approve a high-risk \(toolName) on \(HostDevice.current.computer("your computer"))"
    }

    public static func confirm(reason: String) async -> Outcome {
        #if DEBUG
            if let forced = forcedOutcome() { return forced }
        #endif

        var accessError: Unmanaged<CFError>?
        guard
            let access = SecAccessControlCreateWithFlags(
                nil,
                kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly,
                [.biometryCurrentSet, .or, .devicePasscode],
                &accessError)
        else {
            let detail = accessError?.takeRetainedValue().localizedDescription
                ?? "the access policy could not be created"
            return .unavailable(
                "High-risk approvals need \(HostDevice.current.biometric) or a \(HostDevice.current.secret), and this \(HostDevice.current.name) could not set that up (\(detail)). Answer this one at \(HostDevice.current.computer("the computer"))."
            )
        }

        // A fresh context every time. A reused one can satisfy a later check
        // from an earlier unlock, which would turn the second high-risk approval
        // of a session into no check at all.
        let context = LAContext()
        context.localizedCancelTitle = "Cancel"

        var policyError: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &policyError) else {
            return .unavailable(unavailableReason(policyError))
        }

        do {
            let ok = try await context.evaluateAccessControl(
                access, operation: .useItem, localizedReason: reason)
            return ok ? .authenticated : .failed("the check did not pass")
        } catch let error as LAError {
            switch error.code {
            case .userCancel, .appCancel, .systemCancel:
                return .cancelled
            case .userFallback:
                // The user asked for the passcode and then backed out of it.
                return .cancelled
            case .passcodeNotSet, .biometryNotAvailable, .biometryNotEnrolled:
                return .unavailable(unavailableReason(error as NSError))
            default:
                return .failed(error.localizedDescription)
            }
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    private static func unavailableReason(_ error: NSError?) -> String {
        guard let error, let code = LAError.Code(rawValue: error.code) else {
            return
                "High-risk approvals need \(HostDevice.current.biometric) or a \(HostDevice.current.secret) and this \(HostDevice.current.name) cannot run that check. Answer this one at \(HostDevice.current.computer("the computer"))."
        }
        switch code {
        case .passcodeNotSet:
            return
                "This \(HostDevice.current.name) has no \(HostDevice.current.secret), so it cannot prove who is holding it. High-risk approvals have to be answered at \(HostDevice.current.computer("the computer"))."
        case .biometryNotEnrolled:
            return
                "\(HostDevice.current.biometric) is not set up on this \(HostDevice.current.name). Set it up, or answer high-risk approvals at \(HostDevice.current.computer("the computer")), the \(HostDevice.current.secret) alone is offered when \(HostDevice.current.biometric) is available but unrecognised."
        case .biometryNotAvailable:
            return
                "\(HostDevice.current.biometric) is unavailable on this \(HostDevice.current.name). High-risk approvals need it, or the \(HostDevice.current.secret); answer this one at \(HostDevice.current.computer("the computer"))."
        default:
            return
                "High-risk approvals need \(HostDevice.current.biometric) or a \(HostDevice.current.secret): \(error.localizedDescription). Answer this one at \(HostDevice.current.computer("the computer"))."
        }
    }

    #if DEBUG
        /// Test seam. A UI test cannot drive the system biometric sheet, so the
        /// *outcome* is injectable — but only in a debug build, and only from
        /// the launch command line. A release build has no path to this at all.
        private static func forcedOutcome() -> Outcome? {
            switch UserDefaults.standard.string(forKey: "CC_BIOMETRICS") {
            case "allow": return .authenticated
            case "deny": return .failed("denied by the test harness")
            case "cancel": return .cancelled
            case "unavailable":
                return .unavailable("Biometrics disabled by the test harness.")
            default: return nil
            }
        }
    #endif
}
