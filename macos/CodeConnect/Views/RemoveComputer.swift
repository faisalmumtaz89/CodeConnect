import CodeConnectKit
import SwiftUI

extension View {
    /// Asks, in both apps' words (`ComputerRemoval`), whether to remove the computer `removing` holds, and on Remove
    /// forgets it (`Machines.unpair`). The main window lets go of what it showed of it once it is gone.
    func removesComputer(_ removing: Binding<AppModel?>) -> some View {
        modifier(RemovesComputer(removing: removing))
    }
}

private struct RemovesComputer: ViewModifier {
    @Environment(Machines.self) private var machines
    @Binding var removing: AppModel?

    func body(content: Content) -> some View {
        content.confirmation($removing) { computer in
            Confirmation(
                title: ComputerRemoval.question, message: ComputerRemoval.sentence(name: computer.machineName),
                answers: [
                    .init(title: "Cancel", kind: .cancel, id: "computer.remove.cancel"),
                    .init(title: ComputerRemoval.action, kind: .destructive, id: "computer.remove.confirm") {
                        machines.unpair(computer)
                    },
                ])
        }
    }
}
