//! Where the `codex` a session runs is found: one ordered list of candidates, read by
//! the launcher (`codeconnect codex`) and by the daemon's check of Codex. The launcher
//! puts its `CODECONNECT_CODEX_BIN` override in it, which the check leaves out.
//! [`crate::claude_bin::first_runnable`] picks the check's codex from it.

use std::path::{Path, PathBuf};

/// The ordered candidates: config `codex_bin`, then the `CODECONNECT_CODEX_BIN`
/// override, then every `codex` on `PATH` in the order the shell tries them — so the
/// codex a session runs is the one typing `codex` runs — and the well-known install
/// locations last, for a `PATH` that has none: the standalone installer's entry point,
/// then the two generic bin dirs a package manager would link a `codex` into.
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
    candidates.push(home.join(".local/bin/codex"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    candidates
}
