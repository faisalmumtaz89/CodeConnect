//! What to call a run out loud.
//!
//! A session's name is `cc-<n>` — the lowest free integer, reused the moment
//! its holder exits. It is the right handle for `tmux attach` and the wrong
//! word for a human: `cc-1 needs you` on a lock screen names nothing the reader
//! recognises, and two different projects wear the same name a week apart.
//!
//! The one fact a run carries that a human chose is where it is working. So the
//! label is the last component of `cwd` and nothing else.
//!
//! **Lexical, and deliberately so.** This touches no filesystem: no `stat`, no
//! `git`, no subprocess. It is called for every session in every fleet
//! snapshot, and the fleet is read on a fifteen-second poll — a probe here
//! would be process spawns on the hottest read in the daemon, and a probe
//! against a hung network mount would be those spawns blocking. A path's last
//! component needs none of that.
//!
//! **There is no fallback.** When `cwd` names nothing, this returns nothing,
//! and the surface above says so. Falling back to the tmux name would put the
//! counter back on the screen through the one door left open, which is exactly
//! how it got there in the first place.

/// Longest label any surface will be handed.
///
/// A directory name is normally a word. This exists for the one that is not —
/// a timestamped export directory, a generated worktree — so it cannot run off
/// the end of a notification title.
const MAX_CHARS: usize = 40;

/// The project a run is working in, or empty when its `cwd` does not name one.
///
/// Empty covers every way that can happen — no cwd recorded, the filesystem
/// root, a path ending in `.` or `..` that only a filesystem could resolve —
/// because a caller's answer to all of them is the same, and inventing a
/// different word for each would be claiming to know which one occurred.
pub fn project_label(cwd: &str) -> String {
    let last = cwd.split('/').rfind(|part| !part.is_empty()).unwrap_or("");
    // `.` and `..` are positions, not names. A trailing `.` could be dropped
    // lexically, but `..` could not — that needs the filesystem this function
    // deliberately never touches — and a rule that resolves one of them and not
    // the other is a rule nobody can predict. Neither names a project, so
    // neither produces a label.
    if last == "." || last == ".." {
        return String::new();
    }
    // Nothing that breaks a line survives to a label. A newline in a directory
    // name is legal on macOS, and one in a push title would let a path forge a
    // second line of alert copy. `is_control` alone is not enough: U+2028 and
    // U+2029 are line and paragraph separators and are *not* control
    // characters, so they would pass a control-only filter and still break the
    // line wherever the platform honours them.
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{2028}' && *c != '\u{2029}')
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return String::new();
    }
    if cleaned.chars().count() <= MAX_CHARS {
        return cleaned.to_string();
    }
    // Counted in `char`s rather than sliced by byte, so a multi-byte name is
    // cut between characters instead of through one.
    let kept: String = cleaned.chars().take(MAX_CHARS - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_is_the_last_component_of_the_working_directory() {
        assert_eq!(project_label("/srv/dev/code/Aion"), "Aion");
        assert_eq!(project_label("/srv/dev/Aion/"), "Aion");
        assert_eq!(project_label("/srv/dev/Aion//"), "Aion");
    }

    /// A nested directory names *itself*, not the repository above it. Someone
    /// working in `packages/api` is working on `api`, and finding the repo root
    /// would mean asking git — see the module note on why this never does.
    #[test]
    fn a_nested_directory_names_itself() {
        assert_eq!(project_label("/srv/dev/Aion/packages/api"), "api");
    }

    /// The counter must not come back through the door marked "fallback".
    #[test]
    fn a_path_that_names_no_project_yields_no_label() {
        assert_eq!(project_label(""), "");
        assert_eq!(project_label("/"), "");
        assert_eq!(project_label("///"), "");
        assert_eq!(project_label("/srv/dev/."), "");
        assert_eq!(project_label("/srv/dev/.."), "");
        assert_eq!(project_label("   "), "");
    }

    /// Names that merely *look* like something else are still names. A denylist
    /// on the shape of the string would rename a real directory, and `cc-tools`
    /// is a real directory.
    #[test]
    fn a_directory_is_never_rejected_for_resembling_a_handle() {
        assert_eq!(project_label("/srv/dev/cc-tools"), "cc-tools");
        assert_eq!(project_label("/srv/dev/highline"), "highline");
        assert_eq!(
            project_label("/srv/dev/claude-experiments"),
            "claude-experiments"
        );
    }

    #[test]
    fn spaces_dots_and_unicode_survive() {
        assert_eq!(project_label("/srv/dev/My Project"), "My Project");
        assert_eq!(project_label("/srv/dev/.dotfiles"), ".dotfiles");
        assert_eq!(project_label("/srv/dev/Ærø-planner"), "Ærø-planner");
        assert_eq!(project_label("/srv/dev/日本語"), "日本語");
    }

    /// A newline in a directory name is legal, and a newline in an alert title
    /// is a second line of copy the daemon did not write.
    #[test]
    fn control_characters_never_reach_a_label() {
        assert_eq!(project_label("/srv/dev/Ai\non"), "Aion");
        assert_eq!(project_label("/srv/dev/A\u{7}i\u{1b}on"), "Aion");
        assert_eq!(project_label("/srv/dev/\n\t"), "");
    }

    #[test]
    fn an_overlong_name_is_cut_between_characters() {
        let long = "é".repeat(60);
        let label = project_label(&format!("/srv/dev/{long}"));
        assert_eq!(label.chars().count(), MAX_CHARS);
        assert!(label.ends_with('…'));
        // The point of counting chars: a byte slice at 39 would have split a
        // two-byte character and produced invalid UTF-8.
        assert!(label.chars().take(MAX_CHARS - 1).all(|c| c == 'é'));
    }

    /// Provenance, not shape. A directory really can be called `cc-1`, and a
    /// guard that rejected names *resembling* a handle would rename it. What
    /// keeps a handle off the screen is where callers get their argument: the
    /// only production call passes a session's `cwd`.
    #[test]
    fn a_directory_named_like_a_handle_is_still_a_directory() {
        assert_eq!(project_label("/srv/dev/cc-1"), "cc-1");
        assert_eq!(project_label("/srv/dev/ccx-2"), "ccx-2");
        assert_eq!(project_label("/srv/dev/unknown"), "unknown");
        assert_eq!(
            project_label("/srv/dev/01K76F46YQ2S9J8Z0P0K3D0V4T"),
            "01K76F46YQ2S9J8Z0P0K3D0V4T"
        );
    }

    /// U+2028 and U+2029 are not control characters, so a control-only filter
    /// would pass them — and a notification title that can be made to contain a
    /// line break is a title a directory name can add a second line to.
    #[test]
    fn unicode_line_separators_never_reach_a_label() {
        assert_eq!(project_label("/srv/dev/Ai\u{2028}on"), "Aion");
        assert_eq!(project_label("/srv/dev/Ai\u{2029}on"), "Aion");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(project_label("/srv/dev/  Aion  "), "Aion");
    }

    #[test]
    fn a_name_exactly_at_the_bound_is_left_alone() {
        let exact = "a".repeat(MAX_CHARS);
        assert_eq!(project_label(&format!("/srv/dev/{exact}")), exact);
    }
}
