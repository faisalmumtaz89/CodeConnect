import Foundation

/// Whether a run's terminal can open now: the exact run the attach would carry, or why not and the route to the fix.
///
/// **One value, because the two answers have to agree.** Every control that opens a terminal needs both halves: what
/// to dial, and what to say instead. Computed separately they drift, and the drift shows up as a control that does
/// nothing — an attach returning at a guard the screen never stated. Deriving both from here makes "an action the
/// connection would refuse" unconstructible rather than something each call site has to remember.
public enum TerminalConnect: Sendable, Equatable {
    /// Everything the attach needs: the run's uid, which the daemon resolves to the one live session carrying it.
    case ready(sessionUID: String)
    /// Why the terminal cannot open, and the route to the fix — `nil` where there honestly is no route.
    case blocked(reason: String, fix: Fix?)

    /// A route to the fix. Each app runs its own action for it.
    public enum Fix: Sendable, Equatable {
        /// Pair with the computer: on a Mac, pair again with the run's computer, which is already paired.
        case pair
        /// End the terminal another run holds and open this one.
        case takeOver

        public func title(on host: HostDevice = .current) -> String {
            switch self {
            case .pair: return host == .mac ? "Pair again" : "Pair"
            case .takeOver: return "End that terminal and open this one"
            }
        }
    }
}

extension AppModel {
    /// Whether this run's terminal can open now, in the words of the device reading it, naming the computer the model
    /// is paired with when it knows it.
    public func terminalConnect(sessionUID: String, on host: HostDevice = .current) -> TerminalConnect {
        let theComputer = host.computer("the computer", named: machineName)
        guard endpoint?.host.isEmpty == false else {
            return .blocked(
                reason: "Pair with \(host.computer("your computer", named: machineName)) first; the terminal uses the "
                    + "same connection.",
                fix: .pair)
        }
        // The terminal is the same connection as everything else, so a link
        // that is down is the whole story — there is no second thing to try.
        guard connection.phase.isConnected else {
            return .blocked(
                reason: "Not connected to \(theComputer). The terminal uses the same connection.", fix: nil)
        }
        // Shell-equivalent authority is never granted to the bootstrap token,
        // so the daemon says per connection whether it will open one at all.
        guard connection.capabilities?.servesTerminal == true else {
            // A paired device refused anyway is on a connection the daemon
            // cannot vouch for as private: pairing again would change nothing,
            // so it is not offered.
            if connection.helloAck?.deviceID != nil, connection.capabilities?.withholdsTerminal == true {
                return .blocked(
                    reason: "\(host.computer("The computer", named: machineName)) opens the live terminal over "
                        + "Tailscale, not over this connection.",
                    fix: nil)
            }
            let device = host == .mac ? "this \(host.name)" : "this device"
            return .blocked(
                reason: "This connection may not open a terminal. Pair \(device) with \(theComputer), then try again.",
                fix: .pair)
        }
        guard tmuxName(for: sessionUID) != nil else {
            // No route to a fix, and honestly so — but two different truths. An
            // adopted run was never in CodeConnect's tmux, so "no longer lists"
            // would be a lie about a session the fleet is showing.
            let adopted = shownSummary(for: sessionUID)?.tmuxSession.isEmpty == true
            return .blocked(
                reason: adopted
                    ? "CodeConnect did not launch this session, so there is no terminal of its own to attach to."
                    : "\(host.computer("Your computer", named: machineName)) no longer lists this run, so there is "
                        + "no terminal to attach to.",
                fix: nil)
        }
        // The daemon allows one terminal per connection and the carrier is
        // shared, so this is the one block whose route ends something the
        // reader owns. It names the run it would end: a control that closes
        // another agent's keyboard without saying whose is not one to offer.
        if case .heldByAnotherRun(let held) = terminal.standing(forRun: sessionUID) {
            return .blocked(
                reason: "\(runLabel(for: held).spoken) has the terminal open, and this connection allows one at a time.",
                fix: .takeOver)
        }
        return .ready(sessionUID: sessionUID)
    }

    /// Whether this run's terminal reopens by itself when its screen appears.
    ///
    /// Back into *this run's* terminal or none: the carrier is shared, so anything automatic would open a shell for a
    /// run the reader merely navigated to — and, where another run holds the terminal, end that one to do it. The
    /// carrier is this run's only once this run's terminal was opened, so this never opens one: opening one is
    /// shell-equivalent authority, never done merely by looking. And never one the daemon closed for a reason a retry
    /// cannot change: a session that ended does not come back by asking again.
    public func terminalReopensByItself(sessionUID: String) -> Bool {
        guard case .mine = terminal.standing(forRun: sessionUID), terminal.canAttach else { return false }
        if let close = terminal.lastClose, !close.isRetryable { return false }
        return true
    }
}
