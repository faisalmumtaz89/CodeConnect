//! What this build actually is, and how the current checkout relates to it.
//!
//! A version number moves at releases and capability bumps, never on
//! ordinary commits — so between those moments it cannot answer "am I
//! running current code?", and two different builds happily share one
//! number. The commit and the ship-source fingerprint baked in by
//! `build.rs` can answer it, and this module is both halves of that
//! comparison: the embedded identity, and the launch-time reading of the
//! recorded checkout. `None` always means "make no claim" — an advisory that
//! can be wrong is worse than none.

include!("build_identity_core.rs");

/// The identity `build.rs` embedded when this `protocol` crate was compiled.
/// Empty strings mean the build ran outside a usable git checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildIdentity {
    /// Full object id, for comparisons.
    pub commit: Option<&'static str>,
    /// Twelve characters, for humans.
    pub short: Option<&'static str>,
    /// The ship-source tree had uncommitted changes when built.
    pub dirty: bool,
    /// Ship-source working-state fingerprint — the exact-match key.
    pub fingerprint: Option<&'static str>,
}

fn non_empty(value: &'static str) -> Option<&'static str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

pub fn installed() -> BuildIdentity {
    BuildIdentity {
        commit: non_empty(env!("CODECONNECT_BUILD_COMMIT")),
        short: non_empty(env!("CODECONNECT_BUILD_COMMIT_SHORT")),
        dirty: env!("CODECONNECT_BUILD_DIRTY") == "1",
        fingerprint: non_empty(env!("CODECONNECT_BUILD_FINGERPRINT")),
    }
}

/// `227f6d4e1791`, `227f6d4e1791-dirty`, or `build unknown`.
pub fn build_tag() -> String {
    tag_of(installed())
}

fn tag_of(identity: BuildIdentity) -> String {
    match identity.short {
        Some(short) if identity.dirty => format!("{short}-dirty"),
        Some(short) => short.to_string(),
        None => "build unknown".to_string(),
    }
}

/// `codeconnect 0.3.0 (227f6d4e1791)` — every shipped binary's one-line
/// introduction. The version says which release lineage; the tag says which
/// exact code.
pub fn version_line(binary: &str, version: &str) -> String {
    format!("{binary} {version} ({})", build_tag())
}

/// How the checkout at `repo_root` relates to the running build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutRelation {
    /// The working tree is byte-for-byte what this binary was built from.
    /// Includes the dirty-build case: a dirty tree that still matches the
    /// dirty build is current, and says nothing.
    Matches,
    /// Clean checkout, strictly ahead: this many commits touching the ship
    /// source are not in the installed build.
    Newer(u64),
    /// The checkout has uncommitted ship-source changes that the build does
    /// not have.
    DirtyCheckout,
    /// Some other divergence — checkout behind, different branch, a build
    /// whose commit this clone does not know. True, so say it; but nothing
    /// more specific is provable.
    Differs,
}

/// The launch-time comparison, under one 750ms budget. `None` means no claim
/// can be made (no embedded identity, no git, deadline breached, repo
/// unreadable) and the caller must stay silent.
pub fn checkout_relation(repo_root: &Path) -> Option<CheckoutRelation> {
    checkout_relation_of(installed(), repo_root, Budget::launch())
}

fn checkout_relation_of(
    identity: BuildIdentity,
    repo_root: &Path,
    budget: Budget,
) -> Option<CheckoutRelation> {
    let installed_fingerprint = identity.fingerprint?;

    let current = fingerprint_ship_source(repo_root, budget)?;
    if current == installed_fingerprint {
        return Some(CheckoutRelation::Matches);
    }
    if ship_source_dirty(repo_root, budget)? {
        return Some(CheckoutRelation::DirtyCheckout);
    }

    let Some(commit) = identity.commit.filter(|commit| is_valid_commit(commit)) else {
        // Fingerprints differ and the build carries no commit to reason
        // from: a true, unspecific difference.
        return Some(CheckoutRelation::Differs);
    };
    // Three-way ancestry, because git *answering no* and git *giving no
    // answer* are different facts: exit 1 from `--is-ancestor` is evidence
    // of divergence; a timeout or spawn failure is evidence of nothing, and
    // the contract for "cannot know" is silence, not an advisory. `?` on the
    // runner is what propagates that silence.
    let probe = format!("{commit}^{{commit}}");
    let (exists, _) = run_git_exit(repo_root, &["cat-file", "-e", &probe], budget)?;
    if exists != 0 {
        // Git answered: this clone has never heard of the build's commit —
        // a different clone, or pruned history.
        return Some(CheckoutRelation::Differs);
    }
    let (ancestry, _) = run_git_exit(
        repo_root,
        &["merge-base", "--is-ancestor", commit, "HEAD"],
        budget,
    )?;
    if ancestry != 0 {
        // Git answered: genuinely not an ancestor — checkout behind, or a
        // diverged branch. `B..HEAD` on that shape would count the far
        // side's commits and dress divergence up as plain "newer".
        return Some(CheckoutRelation::Differs);
    }
    let range = format!("{commit}..HEAD");
    let mut args = vec!["rev-list", "--count", range.as_str(), "--"];
    args.extend_from_slice(SHIP_SOURCE_PATHS);
    let (counted, output) = run_git_exit(repo_root, &args, budget)?;
    if counted != 0 {
        return None;
    }
    let count = String::from_utf8_lossy(&output)
        .trim()
        .parse::<u64>()
        .ok()?;
    if count >= 1 {
        return Some(CheckoutRelation::Newer(count));
    }
    Some(CheckoutRelation::Differs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A miniature repo with the ship-source shape, for exercising the real
    /// git enumeration end to end.
    struct Repo {
        root: std::path::PathBuf,
    }

    impl Repo {
        fn new() -> Repo {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "cc-buildid-{}-{}-{}",
                std::process::id(),
                n,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("mac/protocol/src")).unwrap();
            std::fs::write(root.join("mac/Cargo.toml"), "[workspace]\n").unwrap();
            std::fs::write(root.join("mac/Cargo.lock"), "# lock\n").unwrap();
            std::fs::write(root.join("mac/protocol/src/lib.rs"), "// lib\n").unwrap();
            // Outside the ship set on purpose.
            std::fs::create_dir_all(root.join("ios")).unwrap();
            std::fs::write(root.join("ios/App.swift"), "// ios\n").unwrap();
            let repo = Repo { root };
            repo.git(&["init", "--quiet"]);
            repo.git(&["config", "user.email", "test@example.com"]);
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["add", "-A"]);
            repo.git(&["commit", "--quiet", "-m", "init"]);
            repo
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(args)
                .status()
                .expect("git runs in tests");
            assert!(status.success(), "git {args:?} failed");
        }

        fn head(&self) -> String {
            checkout_head(&self.root, Budget::unbounded()).expect("head exists")
        }

        fn fingerprint(&self) -> String {
            fingerprint_ship_source(&self.root, Budget::unbounded()).expect("fingerprint")
        }

        fn identity(&self) -> BuildIdentity {
            // Leaked: BuildIdentity carries &'static because the real one
            // comes from compile-time env; tests mint theirs the same shape.
            BuildIdentity {
                commit: Some(Box::leak(self.head().into_boxed_str())),
                short: Some(Box::leak(
                    self.head()
                        .chars()
                        .take(12)
                        .collect::<String>()
                        .into_boxed_str(),
                )),
                dirty: false,
                fingerprint: Some(Box::leak(self.fingerprint().into_boxed_str())),
            }
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn commit_validation_is_strict() {
        assert!(is_valid_commit(&"a".repeat(40)));
        assert!(is_valid_commit(&"0".repeat(64)));
        assert!(!is_valid_commit(&"A".repeat(40)), "no uppercase");
        assert!(!is_valid_commit("227f6d4e1791"), "no short ids");
        assert!(!is_valid_commit(&"g".repeat(40)), "hex only");
        assert!(!is_valid_commit(""));
    }

    #[test]
    fn tags_render_clean_dirty_and_unknown() {
        let clean = BuildIdentity {
            commit: Some("aaaa"),
            short: Some("227f6d4e1791"),
            dirty: false,
            fingerprint: Some("ff"),
        };
        assert_eq!(tag_of(clean), "227f6d4e1791");
        let dirty = BuildIdentity {
            dirty: true,
            ..clean
        };
        assert_eq!(tag_of(dirty), "227f6d4e1791-dirty");
        let unknown = BuildIdentity {
            commit: None,
            short: None,
            dirty: false,
            fingerprint: None,
        };
        assert_eq!(tag_of(unknown), "build unknown");
    }

    #[test]
    fn a_matching_checkout_is_matches() {
        let repo = Repo::new();
        let identity = repo.identity();
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Matches)
        );
    }

    #[test]
    fn a_ship_source_commit_ahead_is_newer_with_its_count() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "change"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Newer(1))
        );
        std::fs::write(repo.root.join("mac/Cargo.lock"), "# lock v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "second"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Newer(2))
        );
    }

    /// The ordering pin: fingerprint equality outranks dirtiness. A dirty
    /// build whose tree still matches is *current* — hoisting the dirty
    /// check above equality would nag on every launch of a deliberately
    /// dirty install.
    #[test]
    fn a_dirty_build_matching_the_dirty_tree_is_matches() {
        let repo = Repo::new();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// edited\n").unwrap();
        let identity = BuildIdentity {
            commit: Some(Box::leak(repo.head().into_boxed_str())),
            short: Some("aaaaaaaaaaaa"),
            dirty: true,
            fingerprint: Some(Box::leak(repo.fingerprint().into_boxed_str())),
        };
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Matches),
            "a dirty install of exactly this tree is current, not nagging"
        );
    }

    /// Divergence never reads as Newer: with common ancestor A, build B on
    /// one branch and checkout C on another, `B..HEAD` would count C — the
    /// ancestry gate is what keeps that honest.
    #[test]
    fn a_diverged_checkout_is_differs_not_newer() {
        let repo = Repo::new();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// build\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "build side"]);
        let identity = repo.identity();
        repo.git(&["checkout", "--quiet", "-b", "other", "HEAD~1"]);
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// other\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "diverged ship-source commit"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Differs),
            "a different branch is a difference, not an update count"
        );
    }

    /// The count is pathspec-filtered: a mixed history advertises only the
    /// commits that actually touch what `install.sh` builds.
    #[test]
    fn a_mixed_history_counts_only_ship_source_commits() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("ios/App.swift"), "// ios v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "ios"]);
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "mac"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Newer(1)),
            "the ios commit is not news about the Mac build"
        );
    }

    /// The set boundary: history can move without the Mac build going stale.
    #[test]
    fn an_ios_only_commit_is_not_newer() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("ios/App.swift"), "// ios v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "ios only"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Matches),
            "the fingerprint never saw ios/, so nothing changed"
        );
    }

    #[test]
    fn uncommitted_ship_source_changes_are_dirty_checkout() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// edited\n").unwrap();
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::DirtyCheckout)
        );
    }

    /// An untracked, non-ignored file under the ship set is a real
    /// difference — and must read as dirty, not as some commit distance.
    #[test]
    fn an_untracked_ship_source_file_is_dirty_checkout() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("mac/protocol/src/new.rs"), "// new\n").unwrap();
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::DirtyCheckout)
        );
    }

    /// Commits added and exactly reverted: the fingerprint is equal again,
    /// and equality outranks history.
    #[test]
    fn an_exact_revert_is_matches_again() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "change"]);
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// lib\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "revert"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Matches)
        );
    }

    /// A build this clone has never heard of (or a checkout behind it):
    /// true-but-unspecific.
    #[test]
    fn an_unknown_build_commit_is_differs() {
        let repo = Repo::new();
        let foreign = BuildIdentity {
            commit: Some(Box::leak("b".repeat(40).into_boxed_str())),
            short: Some("bbbbbbbbbbbb"),
            dirty: false,
            fingerprint: Some("not-the-fingerprint-of-this-tree"),
        };
        assert_eq!(
            checkout_relation_of(foreign, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Differs)
        );
    }

    #[test]
    fn a_checkout_behind_the_build_is_differs() {
        let repo = Repo::new();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "ahead"]);
        let identity = repo.identity();
        repo.git(&["reset", "--hard", "--quiet", "HEAD~1"]);
        assert_eq!(
            checkout_relation_of(identity, &repo.root, Budget::unbounded()),
            Some(CheckoutRelation::Differs)
        );
    }

    /// The silence contract: a budget that has already expired means git
    /// can give no answer — and no answer is `None`, never a `Differs`
    /// advisory invented from the failure.
    #[test]
    fn an_expired_budget_makes_no_claim_even_on_a_real_repo() {
        let repo = Repo::new();
        let identity = repo.identity();
        std::fs::write(repo.root.join("mac/protocol/src/lib.rs"), "// v2\n").unwrap();
        repo.git(&["commit", "--quiet", "-am", "ahead"]);
        let expired = Budget {
            deadline: Some(std::time::Instant::now() - Duration::from_millis(1)),
        };
        assert_eq!(
            checkout_relation_of(identity, &repo.root, expired),
            None,
            "cannot-know must render as silence, not as a difference"
        );
    }

    #[test]
    fn no_embedded_fingerprint_makes_no_claim() {
        let repo = Repo::new();
        let anonymous = BuildIdentity {
            commit: None,
            short: None,
            dirty: false,
            fingerprint: None,
        };
        assert_eq!(
            checkout_relation_of(anonymous, &repo.root, Budget::unbounded()),
            None
        );
    }

    #[test]
    fn a_missing_repo_makes_no_claim() {
        let repo = Repo::new();
        let identity = repo.identity();
        let gone = repo.root.join("never-existed");
        assert_eq!(
            checkout_relation_of(identity, &gone, Budget::unbounded()),
            None
        );
    }

    #[test]
    fn the_fingerprint_sees_mode_symlink_and_deletion() {
        let repo = Repo::new();
        let base = repo.fingerprint();

        // Mode flip.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = repo.root.join("mac/protocol/src/lib.rs");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_ne!(repo.fingerprint(), base, "the executable bit is identity");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(repo.fingerprint(), base);
        }

        // Deletion of a tracked file.
        std::fs::remove_file(repo.root.join("mac/Cargo.lock")).unwrap();
        let deleted = repo.fingerprint();
        assert_ne!(deleted, base, "absence is a state, not a skip");

        // Restore; symlink where a file was.
        std::fs::write(repo.root.join("mac/Cargo.lock"), "# lock\n").unwrap();
        assert_eq!(repo.fingerprint(), base);
        #[cfg(unix)]
        {
            std::fs::remove_file(repo.root.join("mac/Cargo.lock")).unwrap();
            std::os::unix::fs::symlink("Cargo.toml", repo.root.join("mac/Cargo.lock")).unwrap();
            assert_ne!(
                repo.fingerprint(),
                base,
                "a symlink is not its target's file"
            );
        }
    }

    #[test]
    fn ignored_files_are_invisible_to_the_fingerprint() {
        let repo = Repo::new();
        std::fs::write(repo.root.join(".gitignore"), "mac/ccd/\n").unwrap();
        repo.git(&["add", ".gitignore"]);
        repo.git(&["commit", "--quiet", "-m", "ignore"]);
        let base = repo.fingerprint();
        std::fs::create_dir_all(repo.root.join("mac/ccd")).unwrap();
        std::fs::write(repo.root.join("mac/ccd/scratch.log"), "noise\n").unwrap();
        assert_eq!(repo.fingerprint(), base, "ignored means ignored");
    }
}
