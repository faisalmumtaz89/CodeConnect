//! **A test run observes the machine; it does not change it.**
//!
//! Test-only. Several production writers in this crate address
//! [`protocol::root_dir`] — the tmux server conf, the supervisor's per-run log, the
//! coordinator's stdout capture — and a suite that drives them through their real
//! code paths writes into the operator's own `~/.codeconnect`. That is not a
//! hypothetical: 292 of the 380 files in the real logs directory were supervisor
//! logs from 71 test processes that had long since exited, and every `cargo test`
//! rewrote the real `tmux.conf` with whatever history limit some test picked.
//!
//! Each of those writers is now split at compile time, so the test branch reaches a
//! throwaway directory. This is the fence behind those splits: a recursive
//! before-and-after of the whole home, so a future caller that reaches the real root
//! by a route nobody anticipated is caught here rather than discovered in somebody's
//! logs directory a month later.
//!
//! # This module is the per-test half. The suite-level half is a script.
//!
//! What is bracketed here is one test at a time — the ones that drive a writer
//! somebody thought to bracket. The claim being made is wider than that: "`cargo test`
//! does not write into the operator's home", a sentence about the WHOLE suite, whose
//! counterexample will be a writer nobody suspected, reached from a test nobody
//! bracketed. A per-test fence cannot say it.
//!
//! `tests/real-home-unchanged.sh` says it: inventory the real home, run
//! `cargo test --workspace`, inventory it again, diff. That is the shape that found
//! the finding this module exists for, at a time when a one-filename in-process guard
//! was passing. Run it, and cite it, whenever the claim is made.
//!
//! # What is deliberately not watched, and why
//!
//! A **live `ccd`** writes into this same directory while the suite runs — its
//! event database and the write-ahead files beside it, its own stdout and stderr
//! logs, and the socket it listens on. Those are the daemon's, not the suite's, and
//! a guard that failed because a daemon did its job would be noise that gets muted.
//! They are named exactly, so the exclusion is a short readable list rather than a
//! pattern that could quietly swallow a real finding — and everything else in the
//! home, at every depth, is watched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One entry per file: its size and modification time, or the reason it could not be
/// read. Enough to catch a file created, removed, appended to or rewritten, without
/// reading a 800 KB database on every test that wants the fence.
///
/// "Unreadable" is a value rather than a fallback pair, because a fallback compares
/// equal to itself: a file that became unreadable during the suite would have read as
/// `(0, None)` before and after, and been reported as unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileEntry {
    Seen(u64, Option<std::time::SystemTime>),
    Unreadable(String),
}

pub(crate) type Snapshot = BTreeMap<PathBuf, FileEntry>;

/// Paths, relative to the home, that a **running daemon** owns and rewrites on its
/// own schedule — matched by their **exact spelling and nothing else**.
///
/// The list used to mix exact names with a prefix (`starts_with("events.db")`) and a
/// suffix (`ends_with(".DS_Store")`), and both are wider than what they were written
/// to cover. `starts_with("events.db")` excuses `events.db.stolen`, and any file
/// somebody names with that prefix, from a fence whose entire value is that it is not
/// selective. `ends_with(".DS_Store")` excuses a file called
/// `everything-i-wrote.DS_Store` at any depth. Neither hole would ever be noticed,
/// because the failure mode of an over-wide exclusion is silence.
///
/// So: the daemon's database files by name, its socket, its two logs, and `.DS_Store`
/// as a whole FILE NAME at any depth (Finder's, and it appears whenever somebody opens
/// the folder). Everything else in the home, at every depth, is watched.
fn is_the_daemons_own(relative: &Path) -> bool {
    let Some(name) = relative.to_str() else {
        return false;
    };
    if matches!(
        name,
        "ccd.sock"
            | "logs/ccd.out.log"
            | "logs/ccd.err.log"
            | "events.db"
            | "events.db-wal"
            | "events.db-shm"
            | "events.db-journal"
    ) {
        return true;
    }
    relative.file_name().is_some_and(|f| f == ".DS_Store")
}

/// Every file under `dir`, recorded relative to `root` — or the reason this walk
/// **could not say** what is under it.
///
/// **An unreadable directory is an error, not a skip.** This is a fence whose whole
/// product is the sentence "the suite changed nothing", and a walk that silently
/// stepped over a directory it could not open would be asserting that sentence about
/// a home it had only partly seen — with the unseen part being, by construction, the
/// part something had just done something to. The same goes for an entry the kernel
/// refuses to produce halfway through an enumeration: `flatten()` dropped those, and a
/// listing that ends early looks exactly like a directory with fewer files in it.
///
/// A file whose metadata cannot be read is recorded as unreadable rather than as
/// `(0, None)`. The old spelling gave every unreadable file the same entry, so a file
/// that became unreadable *during* the suite compared equal to itself and the change
/// went unreported.
fn walk(dir: &Path, root: &Path, into: &mut Snapshot) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("{} could not be listed ({err})", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "{} could not be enumerated to the end ({err})",
                dir.display()
            )
        })?;
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if is_the_daemons_own(relative) {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, root, into)?,
            Ok(_) => {
                let seen = match std::fs::symlink_metadata(&path) {
                    Ok(m) => FileEntry::Seen(m.len(), m.modified().ok()),
                    Err(err) => FileEntry::Unreadable(err.to_string()),
                };
                into.insert(relative.to_path_buf(), seen);
            }
            Err(err) => {
                return Err(format!(
                    "{}'s kind could not be read ({err}), so this walk cannot say what is \
                     under it",
                    path.display()
                ))
            }
        }
    }
    Ok(())
}

/// Every file under the operator's real home right now, at every depth.
///
/// **Panics rather than returning a partial answer.** A snapshot that quietly omitted
/// what it could not read would make the comparison that follows it a comparison
/// between two equally partial views — which agree with each other for exactly the
/// reason that should have failed the test. A home this process cannot fully read is a
/// fence that cannot do its job, and saying so is the job.
pub fn snapshot_real_home() -> Snapshot {
    snapshot_home_at(&protocol::root_dir())
}

/// [`snapshot_real_home`] over a named root.
///
/// **A home that does not exist is an empty home, not an unreadable one.** A machine
/// that has never run the daemon — every CI runner, a fresh account — has no
/// `~/.codeconnect` at all, and "nothing is under it" is a complete answer the
/// comparison can use: a test that goes on to create the directory is then reported
/// as having created every file it put there. Only the root is allowed to be absent;
/// a directory that vanishes deeper in the walk is still the walk failing to see
/// what it set out to see.
fn snapshot_home_at(root: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    match std::fs::symlink_metadata(root) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return out,
        _ => {}
    }
    if let Err(why) = walk(root, root, &mut out) {
        panic!(
            "the operator's home at {} could not be fully read, so no test can claim to \
             have left it unchanged: {why}",
            root.display()
        );
    }
    out
}

/// Fail with the exact list of what appeared, vanished or changed.
///
/// The difference is spelled out rather than left to a `BTreeMap` comparison's
/// output, because the whole value of this fence is that somebody reading the
/// failure can see which writer to go and split.
pub fn assert_real_home_unchanged(before: &Snapshot, what: &str) {
    let after = snapshot_real_home();
    let mut complaints = Vec::new();
    for (path, entry) in &after {
        match before.get(path) {
            None => complaints.push(format!("created {}", path.display())),
            Some(was) if was != entry => complaints.push(format!("changed {}", path.display())),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            complaints.push(format!("removed {}", path.display()));
        }
    }
    assert!(
        complaints.is_empty(),
        "{what} changed the operator's own {}: {}",
        protocol::root_dir().display(),
        complaints.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-home-guard-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// **A directory this process cannot open is a hole in the fence, not a hole in
    /// the home.** The walk used to `return` on an unreadable directory and carry on
    /// as if it had seen everything, which makes "the suite changed nothing" a
    /// sentence about the part of the home the walk happened to reach — and the part
    /// it did not reach is, by construction, the part something had just done
    /// something to.
    #[test]
    fn a_directory_the_walk_cannot_read_is_an_error_rather_than_a_skip() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("unreadable");
        std::fs::write(root.join("visible"), b"x").unwrap();
        let closed = root.join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::write(closed.join("hidden"), b"x").unwrap();

        let mut open = Snapshot::new();
        walk(&root, &root, &mut open).expect("a readable home walks cleanly");
        assert_eq!(open.len(), 2, "both files are seen while both are readable");

        std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o000)).unwrap();
        let mut partial = Snapshot::new();
        let err = walk(&root, &root, &mut partial);
        std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o755)).unwrap();

        let why = err.expect_err("an unreadable directory must fail the walk, not be skipped");
        assert!(
            why.contains("closed"),
            "the failure must name the directory it could not read: {why}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A home that is not there is an empty home.** A CI runner has never run the
    /// daemon and has no `~/.codeconnect`; the fence must still stand there, and
    /// stand in the direction that matters: a test that brings the directory into
    /// existence is reported for every file it put in it.
    #[test]
    fn an_absent_home_snapshots_as_empty_and_its_creation_is_reported() {
        let root = scratch("absent");
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!root.exists());

        let before = snapshot_home_at(&root);
        assert!(
            before.is_empty(),
            "nothing is under a home that does not exist"
        );

        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs/run.log"), b"x").unwrap();
        let after = snapshot_home_at(&root);
        assert_eq!(
            after.len(),
            1,
            "the file the test created is seen: {after:?}"
        );
        assert!(
            after.contains_key(Path::new("logs/run.log")),
            "and it is named relative to the home: {after:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Exclusions match a whole name, never a prefix or a suffix.** The failure mode
    /// of an over-wide exclusion is silence: a file the fence quietly stops watching is
    /// a file nothing will ever report, and the two patterns that were here excused
    /// `events.db.stolen` and `everything-i-wrote.DS_Store`.
    #[test]
    fn the_exclusions_are_exact_names_and_do_not_swallow_their_neighbours() {
        let excluded = [
            "ccd.sock",
            "logs/ccd.out.log",
            "logs/ccd.err.log",
            "events.db",
            "events.db-wal",
            "events.db-shm",
            ".DS_Store",
            "logs/.DS_Store",
        ];
        for name in excluded {
            assert!(
                is_the_daemons_own(Path::new(name)),
                "{name} is the daemon's own (or Finder's) and must not fail a suite"
            );
        }
        let watched = [
            "events.db.stolen",
            "events.dbx",
            "everything-i-wrote.DS_Store",
            "logs/ccd.out.log.1",
            "ccd.sock.old",
            "logs/supervisor-cc-1.log",
        ];
        for name in watched {
            assert!(
                !is_the_daemons_own(Path::new(name)),
                "{name} is nobody's but a test's, and a fence that excuses it is not a fence"
            );
        }
    }

    /// A file that becomes unreadable during a run is a CHANGE, and the old fallback
    /// pair hid it: `(0, None)` before and `(0, None)` after compare equal.
    #[test]
    fn a_file_that_becomes_unreadable_is_reported_as_changed() {
        assert_ne!(
            FileEntry::Seen(0, None),
            FileEntry::Unreadable("permission denied".into()),
            "an unreadable file must not compare equal to an empty one"
        );
    }
}
