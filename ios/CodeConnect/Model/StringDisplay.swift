import Foundation

extension String {
    /// Up to the first line break the screen draws — `\n`, `\r\n`, `\r`, form
    /// feed, U+0085, U+2028, U+2029 — without its surrounding spaces.
    ///
    /// Swift reads `\r\n` as one `Character`, so a split on `"\n"` let a
    /// Windows-style command through whole: drawn as its first line, and
    /// spoken as both. `Character.isNewline` also counts a vertical tab, which
    /// `Text` draws as nothing and runs on through (measured: one line, as wide
    /// as without it), so a cut there hid the rest of a command the screen
    /// would have shown — `printf 'a<VT>b' | rm -rf ~/work` as `printf 'a`.
    var firstLine: String {
        String(prefix { !$0.isNewline || $0 == "\u{0B}" })
            .trimmingCharacters(in: .whitespaces)
    }

    /// An agent's text as the screen may draw it: NUL, U+061C and the bidi
    /// embeddings, overrides and isolates (U+202A–202E, U+2066–2069) each
    /// become U+FFFD, and nothing else changes.
    ///
    /// Drawn as they arrive, those characters show something other than what
    /// runs: an override reverses the rest of its line, so `git push origin
    /// <U+202E>niam ecrof--` read `git push origin --force main` under
    /// "Verified", and a NUL is drawn as nothing. Claude Code's own renderer
    /// draws the same bidi characters as U+FFFD (2.1.308). For display only —
    /// the wire, the hash and the answer keep the bytes.
    var displaySafe: String {
        var scalars = String.UnicodeScalarView()
        for scalar in unicodeScalars {
            switch scalar.value {
            case 0x00, 0x061C, 0x202A...0x202E, 0x2066...0x2069: scalars.append("\u{FFFD}")
            default: scalars.append(scalar)
            }
        }
        return String(scalars)
    }
}
