import SwiftUI

/// The main window's three panes, each of which hides on its own: by its toggle at the window's top edge, or from the
/// View menu (⌃⌘S for the sidebar). A hidden pane leaves nothing behind; its room goes to the panes beside it. Kept in
/// the Mac's preferences, so the window opens again as it was left.
enum Panes {
    static let sidebarHidden = "panes.sidebarHidden"
    static let listHidden = "panes.listHidden"
    static let runHidden = "panes.runHidden"
}

extension FocusedValues {
    /// A run is open in the main window: only then do the list and the run hide.
    @Entry var runIsOpen: Bool?
}

/// One pane's toggle at the window's top edge, as the window's own controls draw one: lit while its pane shows.
struct PaneToggle: View {
    let symbol: String
    let shown: Bool
    let title: String
    let id: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 13))
                .foregroundStyle(shown ? Theme.text : Theme.text2)
                .frame(width: 28, height: 24)
                .background(shown ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: id))
        // The window's controls, as a toolbar's: reached by the pointer, the View menu and VoiceOver, not by Tab.
        .focusable(false)
        .help(title)
        .accessibilityLabel(title)
        .accessibilityIdentifier(id)
    }
}

/// View > Hide Sidebar (⌃⌘S), Hide List and Hide Run, each Show while its pane is hidden.
struct PaneCommands: Commands {
    @AppStorage(Panes.sidebarHidden) private var sidebarHidden = false
    @AppStorage(Panes.listHidden) private var listHidden = false
    @AppStorage(Panes.runHidden) private var runHidden = false
    @FocusedValue(\.runIsOpen) private var runIsOpen

    var body: some Commands {
        CommandGroup(replacing: .sidebar) {
            Button(sidebarHidden ? "Show Sidebar" : "Hide Sidebar") { sidebarHidden.toggle() }
                .keyboardShortcut("s", modifiers: [.control, .command])
            Button(listHidden ? "Show List" : "Hide List") {
                listHidden.toggle()
                if listHidden { runHidden = false }
            }
            .disabled(runIsOpen != true)
            Button(runHidden ? "Show Run" : "Hide Run") {
                runHidden.toggle()
                if runHidden { listHidden = false }
            }
            .disabled(runIsOpen != true)
        }
    }
}
