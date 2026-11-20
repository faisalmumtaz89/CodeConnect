import SwiftUI

/// A Settings pane's page, as a Mac Settings window sizes one: as tall as its content, up to `maxHeight`, past which it
/// scrolls. So the window takes each pane's own height, and switching panes resizes it.
struct SettingsScroll<Content: View>: View {
    static var maxHeight: CGFloat { 640 }
    @ViewBuilder let content: Content
    @State private var height: CGFloat = 0

    var body: some View {
        ScrollView {
            content
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(24)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        }
        .frame(width: 560, height: min(height, Self.maxHeight))
        .background(Theme.sheet)
    }
}
