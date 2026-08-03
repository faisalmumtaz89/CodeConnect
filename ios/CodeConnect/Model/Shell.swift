import Foundation

/// Turning a value into something safe to *show* as part of a shell command.
///
/// This app runs nothing. What it does is print commands for a person to copy
/// into their own terminal, which is the same exposure wearing different
/// clothes: the value still ends up in a shell, just one we do not control and
/// cannot take responsibility for. A daemon-supplied id with a space in it
/// silently resumes the wrong session; one with a `;` or a backtick runs
/// whatever follows.
enum Shell {

    /// POSIX single-quoting: everything inside is literal, and the one character
    /// that cannot appear is the quote itself, which is closed, escaped and
    /// reopened in the standard `'\''` form.
    ///
    /// Applied unconditionally rather than only to values that look dangerous.
    /// "Quote it if it needs quoting" needs a definition of dangerous that is
    /// right for every shell, and quoting a UUID costs two characters.
    static func quoted(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: #"'\''"#) + "'"
    }
}
