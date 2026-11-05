import Foundation

enum ToolStatus: Sendable, Hashable {
    case running
    case succeeded
    case failed
    case interrupted
    /// The call was never approved, so it never ran.
    case denied
    /// No result was ever recorded and the turn has moved on. Saying "running"
    /// here would be a lie with a very long tail.
    case unresolved

    var label: String {
        switch self {
        case .running: return "running"
        case .succeeded: return "done"
        case .failed: return "failed"
        case .interrupted: return "interrupted"
        case .denied: return "denied"
        case .unresolved: return "no result recorded"
        }
    }

    var symbol: String {
        switch self {
        case .running: return "circle-dashed"
        case .succeeded: return "check-circle-fill"
        case .failed: return "x-circle-fill"
        case .interrupted: return "stop-circle-fill"
        case .denied: return "hand-palm-fill"
        case .unresolved: return "question"
        }
    }
}
