// Bakes the build's identity — commit, dirty flag, and a fingerprint of the
// Mac ship-source tree — into the binaries, so two builds that both call
// themselves "0.3.0" can still be told apart. The measured confusion this
// exists to end: a daemon built yesterday and a checkout carrying two days of
// new work both said "0.2.0", the release advisory compared the equal numbers
// and stayed silent, and the only surface that noticed was the phone.
//
// The rerun rules are the load-bearing half. The identity lives in the
// `protocol` crate, but it describes ALL of the ship source — so a change in
// `ccd/` must rerun this script (and thereby rebuild `protocol` and every
// dependent) or the embedded fingerprint would go stale and the advisory
// would lie in both directions. Directory entries watch recursively.
//
// Never fails the build: outside a git checkout (source archive, CI without
// history) every value degrades to empty and the binaries introduce
// themselves as "build unknown".

// The include is wrapped so the subset of the shared core this script does
// not call (the launch-time budget) lints clean here while staying fully
// linted in the library, which uses all of it.
#[allow(dead_code)]
mod identity_core {
    include!("src/build_identity_core.rs");
}
use identity_core::{checkout_head, fingerprint_ship_source, run_git, ship_source_dirty, Budget};
use std::path::Path;

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

    // The ship-source set, relative to this crate. The whole crate
    // directory is watched — not just `src` — because the fingerprint reads
    // everything git tracks under `mac/protocol`, and a `tests/` file added
    // later must invalidate too. (The workspace `target/` lives at
    // `mac/target`, outside every watched directory.)
    for watched in [
        ".",
        "../Cargo.toml",
        "../Cargo.lock",
        "../codeconnect",
        "../ccd",
        "../cc-hook",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            manifest.join(watched).display()
        );
    }
    // Git's own state, resolved rather than assumed: in a linked worktree
    // `.git` is a pointer file and HEAD/index live in the real git dir,
    // while refs live in the *common* dir — and a ref-only commit (amend,
    // empty commit) moves only a ref, so refs and packed-refs are watched
    // too. Missing paths are skipped; a repo that cannot be resolved leaves
    // identity degraded, never the build broken.
    let budget = Budget::unbounded();
    let mut git_dirs: Vec<std::path::PathBuf> = Vec::new();
    for query in ["--git-dir", "--git-common-dir"] {
        if let Some(output) = run_git(&repo_root, &["rev-parse", query], budget) {
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

    let commit = checkout_head(&repo_root, budget).unwrap_or_default();
    let short: String = commit.chars().take(12).collect();
    let dirty = ship_source_dirty(&repo_root, budget).unwrap_or(false);
    let fingerprint = fingerprint_ship_source(&repo_root, budget).unwrap_or_default();

    println!("cargo:rustc-env=CODECONNECT_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=CODECONNECT_BUILD_COMMIT_SHORT={short}");
    println!(
        "cargo:rustc-env=CODECONNECT_BUILD_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
    println!("cargo:rustc-env=CODECONNECT_BUILD_FINGERPRINT={fingerprint}");
}
