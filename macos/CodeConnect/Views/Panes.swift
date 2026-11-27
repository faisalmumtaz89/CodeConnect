import SwiftUI

/// The main window's two panes, the run list and the run, each of which hides beside the other while a run is open: by
/// its toggle at the window's top edge, or from the View menu (⌃⌘S for the list). A hidden pane leaves nothing behind;
/// its room goes to the pane beside it. Kept in the Mac's preferences, so the window opens again as it was left.
enum Panes {
    static let listHidden = "panes.listHidden"
    static let runHidden = "panes.runHidden"
}

extension FocusedValues {
    /// A run is open in the main window: only then do the list and the run hide.
    @Entry var runIsOpen: Bool?
    /// Settings is over the panes: none of them hides or shows from the View menu meanwhile.
    @Entry var settingsShown: Bool?
    /// An ask or a sheet (Add, Set Up) covers the main window.
    @Entry var windowCovered: Bool?
}

/// One pane's toggle at the window's top edge, as a Mac toolbar's pane toggles are drawn: the icon alone, the hover
/// plate under the pointer, and its pane's own presence for its state; a hidden pane's icon is a step dimmer.
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
                .foregroundStyle(shown ? Theme.text2 : Theme.text3)
                .frame(width: 28, height: 24)
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

/// View > Hide List (⌃⌘S) and Hide Session, each Show while its pane is hidden.
struct PaneCommands: Commands {
    @AppStorage(Panes.listHidden) private var listHidden = false
    @AppStorage(Panes.runHidden) private var runHidden = false
    @FocusedValue(\.runIsOpen) private var runIsOpen
    @FocusedValue(\.settingsShown) private var settingsShown

    var body: some Commands {
        CommandGroup(replacing: .sidebar) {
            Group {
                Button(listHidden ? "Show List" : "Hide List") {
                    listHidden.toggle()
                    if listHidden { runHidden = false }
                }
                .keyboardShortcut("s", modifiers: [.control, .command])
                .disabled(runIsOpen != true)
                Button(runHidden ? "Show Session" : "Hide Session") {
                    runHidden.toggle()
                    if runHidden { listHidden = false }
                }
                .disabled(runIsOpen != true)
            }
            .disabled(settingsShown == true)
        }
    }
}
