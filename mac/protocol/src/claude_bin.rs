//! Where the `claude` a session runs is found: one list, read by the launcher
//! (`codeconnect claude`) and by the daemon's check before a Claude Code start, so
//! the claude a start checks is the claude it runs.

use std::path::{Path, PathBuf};

/// The ordered candidates: config `claude_bin`, then the `CODECONNECT_CLAUDE_BIN`
/// override, then every `claude` on `PATH` in the order the shell tries them — so the
/// claude a session runs is the one typing `claude` runs — and the well-known install
/// locations last, for a `PATH` that has none: the native installer's, the legacy
/// local npm install's, and Homebrew's on Apple silicon and on Intel.
pub fn candidates(
    configured: Option<&str>,
    env_override: Option<PathBuf>,
    home: &Path,
    path_hits: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(configured) = configured {
        candidates.push(PathBuf::from(configured));
    }
    if let Some(env) = env_override {
        candidates.push(env);
    }
    candidates.extend(path_hits);
    candidates.push(home.join(".local/bin/claude"));
    candidates.push(home.join(".claude/local/claude"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
    candidates.push(PathBuf::from("/usr/local/bin/claude"));
    candidates
}

/// The first of `candidates` that is an executable file and not `launcher`, the
/// `codeconnect` binary itself: a shell alias like `alias claude=codeconnect claude`
/// would otherwise resolve to it, and a session would spawn itself in a loop.
pub fn first_runnable(candidates: Vec<PathBuf>, launcher: Option<&Path>) -> Option<PathBuf> {
    candidates.into_iter().find(|candidate| {
        crate::tmux::is_executable_file(candidate) && !is_same_file(candidate, launcher)
    })
}

fn is_same_file(candidate: &Path, current: Option<&Path>) -> bool {
    let Some(current) = current else { return false };
    match (candidate.canonicalize(), current.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_candidate_order_is_config_env_path_wellknown() {
        assert_eq!(
            candidates(
                Some("/from/config/claude"),
                Some(PathBuf::from("/from/env/claude")),
                Path::new("/home/u"),
                vec![
                    PathBuf::from("/first/on/path/claude"),
                    PathBuf::from("/second/on/path/claude"),
                ],
            ),
            vec![
                PathBuf::from("/from/config/claude"),
                PathBuf::from("/from/env/claude"),
                PathBuf::from("/first/on/path/claude"),
                PathBuf::from("/second/on/path/claude"),
                PathBuf::from("/home/u/.local/bin/claude"),
                PathBuf::from("/home/u/.claude/local/claude"),
                PathBuf::from("/opt/homebrew/bin/claude"),
                PathBuf::from("/usr/local/bin/claude"),
            ]
        );
    }

    /// **The first candidate that runs is taken**: one that is missing, one that is
    /// not executable, and the launcher itself (through a link) are passed over, at
    /// any position of the list.
    #[test]
    fn the_first_runnable_candidate_is_taken_and_the_launcher_never() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "protocol-claude-bin-{}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let program = |name: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let (plain, launcher, claude, later) = (
            program("plain", 0o644),
            program("codeconnect", 0o755),
            program("claude", 0o755),
            program("later", 0o755),
        );
        let alias = dir.join("alias-claude");
        std::os::unix::fs::symlink(&launcher, &alias).unwrap();
        let passed_over = vec![dir.join("missing"), plain, alias];
        let list = |last: &[PathBuf]| [passed_over.clone(), last.to_vec()].concat();
        assert_eq!(
            first_runnable(list(&[claude.clone(), later]), Some(&launcher)),
            Some(claude.clone())
        );
        assert_eq!(first_runnable(list(&[]), Some(&launcher)), None);
        assert_eq!(
            first_runnable(list(&[]), None),
            Some(dir.join("alias-claude")),
            "with no launcher to skip, the link is a claude like any other"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_file_detection_handles_symlinks_and_absence() {
        assert!(is_same_file(
            Path::new("/bin/sh"),
            Some(Path::new("/bin/sh"))
        ));
        assert!(!is_same_file(Path::new("/bin/sh"), None));
        assert!(!is_same_file(
            Path::new("/bin/sh"),
            Some(Path::new("/bin/echo"))
        ));
    }
}
