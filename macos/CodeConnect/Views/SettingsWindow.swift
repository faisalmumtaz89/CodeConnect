import CodeConnectKit
import SwiftUI

/// CodeConnect > Settings… (⌘,), drawn by the app rather than the system's toolbar: "Settings" on the traffic lights'
/// line, the panes as the run pane's underlined tabs, a hairline, then the pane, as wide as a pane and as tall as its
/// content. Its asks cover it whole, tabs included (`confirmationHost` on the window's root).
struct SettingsWindow: View {
    @Binding var tab: SettingsTab

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Settings").font(Theme.ui(13, .semibold)).foregroundStyle(Theme.text)
                .frame(maxWidth: .infinity)
                .frame(height: 28)
                .accessibilityAddTraits(.isHeader)
            HStack(spacing: 20) {
                ForEach(SettingsTab.allCases, id: \.self) { each in
                    UnderlineTab(title: each.title, id: each.id, selected: tab == each, count: nil) { tab = each }
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, 8)
            Rectangle().fill(Theme.hairline).frame(height: 1)
            switch tab {
            case .devices: DevicesPane()
            case .folders: FoldersPane()
            }
        }
        .frame(width: 560)
        // The title line is the title bar's: the window's own title and toolbar are gone, its traffic lights stay.
        .ignoresSafeArea(.container, edges: .top)
        .background(Theme.sheet)
        .toolbar(removing: .title)
        .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        .background(WindowChrome())
    }
}
