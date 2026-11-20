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
        content.alert(
            ComputerRemoval.question,
            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
            presenting: removing
        ) { computer in
            Button(ComputerRemoval.action, role: .destructive) { machines.unpair(computer) }
                .accessibilityIdentifier("computer.remove.confirm")
            Button("Cancel", role: .cancel) {}.accessibilityIdentifier("computer.remove.cancel")
        } message: { computer in
            Text(ComputerRemoval.sentence(name: computer.machineName))
        }
    }
}
