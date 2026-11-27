//! What to call a run out loud.
//!
//! A session's name is its uid, a ULID, and the wrong word for a human: on a
//! lock screen it names nothing the reader recognises.
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
//! and the surface above says so. Falling back to the session's name would put
//! its uid on the screen through the one door left open.

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
    //
    // **And nothing that reorders one is drawn as itself.** The label is the
    // direct APNs alert title, which the phone draws as it arrives: U+202E in
    // `abc\u{202E}gpj.exe` draws `abcexe.jpg`. The eleven characters the phone's
    // own `displaySafe` maps — NUL, the Arabic letter mark, the embeddings and
    // overrides, the isolates — are mapped to U+FFFD here too, before the
    // control filter, so NUL is shown as the phone shows it rather than dropped.
    let cleaned: String = last
        .chars()
        .map(|c| match c {
            '\u{0}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => '\u{FFFD}',
            c => c,
        })
        .filter(|c| !c.is_control() && *c != '\u{2028}' && *c != '\u{2029}')
        .collect();
    // Whole, however long: each app cuts a name where it has no room, and the
    // one surface with a fixed bound, the push title, cuts its own
    // ([`crate::apns::alert`]).
    cleaned.trim().to_string()
}

/// Whether `cwd` is a worktree of the run's own: a `.claude/worktrees/<name>` folder, where a start in a new worktree,
/// and claude's own `--worktree`, make one. Lexical, as [`project_label`] is.
pub fn in_worktree(cwd: &str) -> bool {
    let parts: Vec<&str> = cwd.split('/').filter(|part| !part.is_empty()).collect();
    matches!(parts.as_slice(), [.., ".claude", "worktrees", name] if *name != "." && *name != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A run is in a worktree of its own only in a `.claude/worktrees/<name>` folder**: not the repository, not a
    /// folder under a worktree, not one that only names worktrees elsewhere in its path.
    #[test]
    fn a_run_is_in_a_worktree_only_in_a_claude_worktrees_folder() {
        assert!(in_worktree("/Users/dev/app/.claude/worktrees/fix+login"));
        assert!(in_worktree("/Users/dev/app/.claude/worktrees/fix+login/"));
        assert!(!in_worktree("/Users/dev/app"));
        assert!(!in_worktree("/Users/dev/app/.claude/worktrees"));
        assert!(!in_worktree(
            "/Users/dev/app/.claude/worktrees/fix+login/src"
        ));
        assert!(!in_worktree("/Users/dev/worktrees/fix"));
        assert!(!in_worktree(""));
    }

    /// **A direction override cannot reorder a label.** U+202E in a directory
    /// name draws `abc\u{202E}gpj.exe` as `abcexe.jpg`, and the label becomes the
    /// direct APNs alert title, which the phone cannot correct. Each of the
    /// eleven characters the phone's `displaySafe` maps is mapped here too, to
    /// the same U+FFFD.
    ///
    /// **Mutation:** drop the mapping and U+202E reaches the label.
    #[test]
    fn a_direction_control_in_a_directory_name_is_shown_as_a_replacement_character() {
        assert_eq!(
            project_label("/work/abc\u{202E}gpj.exe"),
            "abc\u{FFFD}gpj.exe"
        );
        for c in [
            '\u{0}', '\u{061C}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ]
        .into_iter()
        .chain('\u{202A}'..='\u{202E}')
        {
            assert_eq!(
                project_label(&format!("/work/a{c}b")),
                "a\u{FFFD}b",
                "U+{:04X}",
                c as u32
            );
        }
    }

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

    /// A long folder name reaches the apps whole: they cut it where they have no
    /// room, and keep its end in view.
    #[test]
    fn a_long_name_comes_back_whole() {
        let long = "customer-analytics-platform-q4-2026-data-migration-worktrees";
        assert_eq!(long.chars().count(), 60);
        assert_eq!(project_label(&format!("/srv/dev/{long}")), long);
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
}
