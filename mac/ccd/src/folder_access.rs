//! The folders macOS guards: Desktop, Documents and Downloads.
//!
//! The first time ccd, or a process it started, reads one of them, macOS shows
//! a dialog naming ccd on the Mac's screen, and the read waits for the click.
//! From the phone nobody is there to click, so `codeconnect daemon folders`
//! has the daemon read each one once while someone is at the Mac. macOS keeps
//! each answer for ccd's path, which an update keeps, so every later read is
//! allowed at once or refused at once. A refusal is then the system's words
//! plus where the answer can be changed.
//!
//! External drives are out of scope. iCloud Drive and disk images raise no
//! dialog of their own (measured on macOS 27), and other apps' data is refused
//! without one.

use std::path::{Path, PathBuf};

use protocol::ipc::{FolderAccess, FolderOutcome};

/// Each a folder of the home folder, by the name macOS gives it.
const GUARDED: [&str; 3] = ["Desktop", "Documents", "Downloads"];

/// Reads each guarded folder of `home` once, one after another, each waiting
/// for its dialog if macOS shows one.
pub(crate) fn ask(home: &Path) -> Vec<FolderAccess> {
    GUARDED
        .iter()
        .map(|folder| FolderAccess {
            folder: folder.to_string(),
            outcome: read_once(&home.join(folder)),
        })
        .collect()
}

/// [`ask`], and a note in `root` that every guarded folder has been read once,
/// whatever macOS answered: a start typed at the Mac asks only while the note
/// is missing.
pub(crate) fn ask_and_note(home: &Path, root: &Path) -> Vec<FolderAccess> {
    let folders = ask(home);
    let note: String = folders
        .iter()
        .map(|access| format!("{}\n", access.folder))
        .collect();
    if let Err(err) = std::fs::write(note_path(root), note) {
        crate::log_warn!("folders: could not note that they were asked: {err}");
    }
    folders
}

/// Whether [`ask_and_note`] has run on this computer.
pub(crate) fn asked(root: &Path) -> bool {
    note_path(root).exists()
}

fn note_path(root: &Path) -> PathBuf {
    root.join("folders-asked")
}

/// A folder macOS lets ccd list: opened, and its first entry read. An empty
/// folder is allowed; a first entry that cannot be read is not.
fn read_once(path: &Path) -> FolderOutcome {
    match std::fs::read_dir(path).and_then(|mut entries| entries.next().transpose()) {
        Ok(_) => FolderOutcome::Allowed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => FolderOutcome::Absent,
        Err(err) => FolderOutcome::Refused {
            error: crate::start_session::system_words(&err),
        },
    }
}

/// Where a "no" for `path` can be changed, when `err` may be one: EPERM on a
/// guarded folder of `home` or anything in it. EPERM has other causes too, so
/// the sentence says what to do if access is off, not that it is.
pub(crate) fn hint(err: &std::io::Error, path: &Path, home: &Path) -> Option<String> {
    if !cfg!(target_os = "macos") || err.raw_os_error() != Some(libc::EPERM) {
        return None;
    }
    let folder = GUARDED
        .iter()
        .find(|folder| path.starts_with(home.join(folder)))?;
    Some(format!(
        "If CodeConnect's access to {folder} is off, allow it in System Settings \
         \u{2192} Privacy & Security \u{2192} Files and Folders \u{2192} ccd."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "ccd-folders-{tag}-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn each_folder_is_allowed_refused_or_absent_and_none_is_made() {
        let home = scratch_home("ask");
        std::fs::create_dir(home.join("Desktop")).unwrap();
        std::fs::write(home.join("Desktop").join("notes.txt"), "").unwrap();
        std::fs::create_dir(home.join("Documents")).unwrap();
        std::fs::create_dir(home.join("Downloads")).unwrap();
        std::fs::set_permissions(home.join("Downloads"), PermissionsExt::from_mode(0o000)).unwrap();

        let asked = ask(&home);
        std::fs::set_permissions(home.join("Downloads"), PermissionsExt::from_mode(0o755)).unwrap();
        let outcomes: Vec<(&str, &FolderOutcome)> = asked
            .iter()
            .map(|access| (access.folder.as_str(), &access.outcome))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("Desktop", &FolderOutcome::Allowed),
                ("Documents", &FolderOutcome::Allowed),
                (
                    "Downloads",
                    &FolderOutcome::Refused {
                        error: "Permission denied".into()
                    }
                ),
            ]
        );

        std::fs::remove_dir(home.join("Documents")).unwrap();
        assert_eq!(ask(&home)[1].outcome, FolderOutcome::Absent);
        assert!(
            !home.join("Documents").exists(),
            "an absent folder is not made"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_answered_ask_is_noted_and_only_then() {
        let home = scratch_home("note-home");
        let root = scratch_home("note-root");
        assert!(!asked(&root));
        ask(&home);
        assert!(!asked(&root), "a bare read notes nothing");
        ask_and_note(&home, &root);
        assert!(asked(&root));
        assert_eq!(
            std::fs::read_to_string(root.join("folders-asked")).unwrap(),
            "Desktop\nDocuments\nDownloads\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_hint_is_for_eperm_in_a_guarded_folder_only() {
        let home = Path::new("/Users/me");
        let eperm = std::io::Error::from_raw_os_error(libc::EPERM);
        let eacces = std::io::Error::from_raw_os_error(libc::EACCES);
        let hint_for = |err: &std::io::Error, path: &Path| hint(err, path, home);

        for (path, folder) in [
            (home.join("Desktop"), "Desktop"),
            (home.join("Documents/app/src"), "Documents"),
            (home.join("Downloads/x"), "Downloads"),
        ] {
            assert_eq!(
                hint_for(&eperm, &path),
                Some(format!(
                    "If CodeConnect's access to {folder} is off, allow it in System Settings \
                     \u{2192} Privacy & Security \u{2192} Files and Folders \u{2192} ccd."
                )),
                "{}",
                path.display()
            );
        }
        for path in [
            home.join("Desktopx"),
            home.to_path_buf(),
            Path::new("/Volumes/Desktop").to_path_buf(),
            Path::new("/Users/other").join("Desktop"),
        ] {
            assert_eq!(hint_for(&eperm, &path), None, "{}", path.display());
        }
        assert_eq!(hint_for(&eacces, &home.join("Desktop")), None);
    }
}
