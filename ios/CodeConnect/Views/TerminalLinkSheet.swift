import SwiftUI
import UIKit

/// A web link tapped in the terminal, whole.
struct TerminalLink: Identifiable {
    let url: URL
    var id: URL { url }

    /// Only web links count. A target an agent printed can name any scheme, and
    /// a tap on terminal text must not launch whatever app claims it; SwiftTerm
    /// also matches a scheme inside a word (`file:` in `Makefile:2:`).
    init?(_ text: String) {
        guard let url = URL(string: text), let scheme = url.scheme?.lowercased(),
            scheme == "https" || scheme == "http"
        else { return nil }
        self.url = url
    }
}

/// The link sheet: the full address, readable, with Open and Copy.
///
/// A wrapped URL in a terminal is unreadable and unselectable as one piece;
/// here it is one block, with the host first so the reader can see where it goes.
struct TerminalLinkSheet: View {
    let link: TerminalLink

    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    @State private var copied = false

    var body: some View {
        CCSheetChrome("Link", subtitle: link.url.host(), onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCMonoBlock(link.url.absoluteString, showsCopy: false, wraps: true)
                    CCButton("Open in Safari", icon: "safari", fullWidth: true) {
                        openURL(link.url)
                        dismiss()
                    }
                    CCButton(
                        copied ? "Copied" : "Copy link",
                        icon: copied ? "checkmark" : "doc.on.doc",
                        variant: .secondary, fullWidth: true
                    ) {
                        UIPasteboard.general.string = link.url.absoluteString
                        copied = true
                    }
                }
                .padding(CC.space.md)
            }
        }
        .ccResizableSheet()
    }
}
