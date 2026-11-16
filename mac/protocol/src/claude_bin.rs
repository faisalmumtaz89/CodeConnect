//! Where an agent's program is found: the ordered `claude` candidates the daemon's check
//! before a Claude Code start reads ([`candidates`]), so the claude a start checks is the
//! claude it runs; the pick from such a list that the checks for both agents make
//! ([`first_runnable`]); and the `PATH` lookups beside them ([`search_path`],
//! [`hits_on`], [`is_executable_file`]).

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
    candidates
        .into_iter()
        .find(|candidate| is_executable_file(candidate) && !is_same_file(candidate, launcher))
}

fn is_same_file(candidate: &Path, current: Option<&Path>) -> bool {
    let Some(current) = current else { return false };
    match (candidate.canonicalize(), current.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// [`crate::tmux::path_hits`] over a given `PATH` value and current directory: another
/// process's, such as a login shell's.
pub fn hits_on(path: &std::ffi::OsStr, cwd: Option<&Path>, name: &str) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter_map(|dir| match dir.is_absolute() {
            true => Some(dir),
            false => Some(cwd?.join(dir)),
        })
        .map(|dir| dir.join(name))
        .filter(|candidate| is_executable_file(candidate))
        .collect()
}

/// Whether `path` reaches a regular file someone may execute — what a shell's lookup
/// requires of a `PATH` hit.
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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

    /// What a shell's lookup would try, in its order: an executable file in each
    /// directory, a relative directory (an empty entry included) taken against the
    /// current directory, and nothing a shell would pass over.
    #[test]
    fn path_hits_are_the_programs_a_shell_would_try_in_its_order() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cc-path-hits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let make = |sub: &str, mode: u32| {
            let path = dir.join(sub).join("agent");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let first = make("first", 0o755);
        make("unrunnable", 0o644);
        std::fs::create_dir_all(dir.join("dir-named/agent")).unwrap();
        let relative = make("cwd/rel", 0o700);
        let in_cwd = make("cwd", 0o755);
        let path = std::env::join_paths([
            dir.join("first"),
            dir.join("unrunnable"),
            dir.join("dir-named"),
            dir.join("missing"),
            PathBuf::from("rel"),
            PathBuf::new(),
        ])
        .unwrap();

        assert_eq!(
            hits_on(&path, Some(&dir.join("cwd")), "agent"),
            vec![first.clone(), relative, in_cwd]
        );
        // With no current directory, the relative entries cannot be taken.
        assert_eq!(hits_on(&path, None, "agent"), vec![first]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
