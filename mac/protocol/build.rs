// Bakes this build's identity — the commit it came from, and whether the ship
// source was edited — into every binary, so two builds that both call
// themselves "0.4.0" can still be told apart. It is what `--version` prints,
// and the release workflow compares that printed line against the commit it
// checked out.
//
// The rerun rules are the load-bearing half. The identity lives in the
// `protocol` crate but describes ALL of the Mac ship source, so a change under
// `ccd/` must rerun this script — and thereby rebuild `protocol` and every
// dependent — or the baked dirty flag goes stale. Directory entries watch
// recursively.
//
// Never fails the build: outside a git checkout (a source archive, CI without
// history) every value degrades to empty and the binaries introduce themselves
// as "build unknown".

use std::path::Path;
use std::process::{Command, Stdio};

/// The Mac ship-source set, repo-relative. Exactly what `install.sh` builds: a
/// change outside it (iOS, docs) is not a change to these binaries.
const SHIP_SOURCE_PATHS: &[&str] = &[
    "mac/Cargo.toml",
    "mac/Cargo.lock",
    "mac/protocol",
    "mac/codeconnect",
    "mac/ccd",
    "mac/cc-hook",
    "mac/push-core",
];

/// Git's stdout when it ran and exited zero; `None` for every other outcome —
/// no git, spawn failure, non-zero exit. A build that cannot ask git makes no
/// claim about its identity rather than an invented one.
fn run_git(repo_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// A full git object id: 40 (SHA-1) or 64 (SHA-256 repos) lowercase hex.
/// Anything else — capitalised, short, or decorated — is not an identity.
fn is_valid_commit(id: &str) -> bool {
    (id.len() == 40 || id.len() == 64)
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// HEAD's full object id, validated.
fn checkout_head(repo_root: &Path) -> Option<String> {
    let output = run_git(repo_root, &["rev-parse", "HEAD"])?;
    let head = String::from_utf8_lossy(&output).trim().to_string();
    is_valid_commit(&head).then_some(head)
}

/// `git status --porcelain` over the ship-source set: any line at all —
/// modified, staged, or untracked — means the working state is not a plain
/// checkout of HEAD.
fn ship_source_dirty(repo_root: &Path) -> Option<bool> {
    let mut args = vec!["status", "--porcelain", "--"];
    args.extend_from_slice(SHIP_SOURCE_PATHS);
    let output = run_git(repo_root, &args)?;
    Some(!output.is_empty())
}

fn main() {
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    // mac/protocol -> repo root.
    let repo_root = manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest.clone());

    // The ship-source set, relative to this crate. The whole crate directory is
    // watched — not just `src` — because a `tests/` file added later must
    // invalidate too. (The workspace `target/` lives at `mac/target`, outside
    // every watched directory.)
    for watched in [
        ".",
        "../Cargo.toml",
        "../Cargo.lock",
        "../codeconnect",
        "../ccd",
        "../cc-hook",
        "../push-core",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            manifest.join(watched).display()
        );
    }
    // Git's own state, resolved rather than assumed: in a linked worktree
    // `.git` is a pointer file and HEAD/index live in the real git dir, while
    // refs live in the *common* dir — and a ref-only commit (amend, empty
    // commit) moves only a ref, so refs and packed-refs are watched too.
    // Missing paths are skipped; a repo that cannot be resolved leaves identity
    // degraded, never the build broken.
    let mut git_dirs: Vec<std::path::PathBuf> = Vec::new();
    for query in ["--git-dir", "--git-common-dir"] {
        if let Some(output) = run_git(&repo_root, &["rev-parse", query]) {
            let dir = String::from_utf8_lossy(&output).trim().to_string();
            if dir.is_empty() {
                continue;
            }
            let path = std::path::Path::new(&dir);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                repo_root.join(path)
            };
            if !git_dirs.contains(&absolute) {
                git_dirs.push(absolute);
            }
        }
    }
    for dir in &git_dirs {
        for state in ["HEAD", "index", "refs", "packed-refs"] {
            let path = dir.join(state);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    // Both facts or neither. A commit with an unknowable dirty state is not
    // an identity: baking the commit alone would print it *without* `-dirty`,
    // which is the claim "a plain checkout of this commit" — exactly what
    // could not be established. The full object id is validated here and only
    // the twelve characters everything downstream uses are baked in.
    let identity = checkout_head(&repo_root).zip(ship_source_dirty(&repo_root));
    let (short, dirty) = match identity {
        Some((commit, dirty)) => (commit.chars().take(12).collect::<String>(), dirty),
        None => (String::new(), false),
    };

    println!("cargo:rustc-env=CODECONNECT_BUILD_COMMIT_SHORT={short}");
    println!(
        "cargo:rustc-env=CODECONNECT_BUILD_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
}
