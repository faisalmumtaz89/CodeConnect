//! The filesystem boundary for `~/.codeconnect`.
//!
//! SECURITY.md says the daemon's socket and database "are protected by file
//! permissions". That was only true by accident: every directory and file here
//! used to be created under the process umask, so a user running with `umask
//! 022` — the macOS default for a login shell — got a world-readable event log
//! containing every transcript line an agent ever produced, and a
//! group-and-world-readable `~/.codeconnect` to enumerate it through.
//!
//! Two rules, applied at startup and at every creation:
//!
//!   * **Directories are `0700`.** Created that way by [`std::fs::DirBuilder`]
//!     rather than chmod'ed afterwards, so there is no window in which the
//!     directory exists and is readable.
//!   * **Sensitive files are `0600`.** The static token, the database and its
//!     WAL sidecars, the TLS private key, the logs.
//!
//! Repair is as important as creation. An installation made before this module
//! existed still has a `0644` token file, and a boundary that is only enforced
//! on new state protects nobody who already ran the daemon.
//!
//! **`chmod` is never applied through a symlink.** `std::fs::set_permissions`
//! follows them, so repairing a path an attacker replaced with a link to
//! something else would change *that* file's mode instead. Every repair path
//! here stats with [`std::fs::symlink_metadata`] first and refuses rather than
//! guessing.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Every directory under the CodeConnect root. Owner-only, no group, no other.
pub const DIR_MODE: u32 = 0o700;

/// Every file that carries a credential, a key, or terminal content.
pub const FILE_MODE: u32 = 0o600;

/// The permission bits, ignoring the file-type bits `st_mode` also carries.
const PERMISSION_BITS: u32 = 0o7777;

/// Create `path` and its parents `0700`, and repair the mode if it already
/// exists.
///
/// The mode is passed to `mkdir(2)` rather than applied afterwards: a directory
/// created `0755` and chmod'ed a microsecond later is readable for that
/// microsecond, and the whole point of this function is that no such window
/// exists. `mkdir` also applies the umask on top of the mode, but only ever to
/// *remove* bits, so `0700 & ~umask` can never be wider than `0700`.
pub fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)?;
    // `recursive(true)` is a no-op on a directory that already exists, so this
    // is what repairs an installation made before the boundary was enforced.
    harden(path, DIR_MODE)
}

/// Bring an existing file to `0600`. A path that does not exist is not an error
/// — there is nothing there to protect.
pub fn harden_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => harden(path, FILE_MODE),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Open `path` for writing, `0600` from the instant it exists.
///
/// `OpenOptions::mode` only applies when the file is *created*, so an existing
/// loose file would keep its old mode and be silently written to. The repair
/// afterwards is what closes that: the file is owner-only before this function
/// returns, whether it was created here or not.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(path)?;
    harden_file(path)?;
    Ok(file)
}

/// Make sure `path` exists and is `0600`, without disturbing its contents.
///
/// Used ahead of a library that will create the file itself under the umask —
/// SQLite being the case that matters. An empty file is a valid zero-page
/// database, and SQLite copies the main database's mode onto the `-wal` and
/// `-shm` sidecars it creates, so getting there first is what makes the whole
/// set owner-only rather than only the file we can name.
pub fn touch_private(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(FILE_MODE)
        .open(path)?;
    harden_file(path)
}

/// Write `contents` to a file that is owner-only for its entire existence.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = create_private(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// The permission bits currently on `path`, for assertions and diagnostics.
pub fn mode_of(path: &Path) -> io::Result<u32> {
    Ok(fs::symlink_metadata(path)?.permissions().mode() & PERMISSION_BITS)
}

/// `chmod`, but never through a symlink, and never when it is already right.
fn harden(path: &Path, mode: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a symlink; refusing to chmod through it",
                path.display()
            ),
        ));
    }
    if meta.permissions().mode() & PERMISSION_BITS == mode {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cc-fsperm-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_directory_is_owner_only_even_under_a_permissive_umask() {
        // The defect this replaces: `create_dir_all` under the macOS login
        // default of `umask 022` produced a `0755` `~/.codeconnect`, so any
        // other account on the Mac could list the event log and the token file.
        let root = scratch("dir");
        let nested = root.join("a/b/c");
        private_dir(&nested).unwrap();
        for path in [root.join("a"), root.join("a/b"), nested] {
            assert_eq!(
                mode_of(&path).unwrap(),
                0o700,
                "{} is not owner-only",
                path.display()
            );
        }
    }

    #[test]
    fn an_existing_world_readable_directory_is_repaired() {
        // A boundary enforced only on new state protects nobody who already ran
        // the daemon, and every installation made before this module existed
        // has exactly this directory sitting in it.
        let root = scratch("repair-dir");
        let loose = root.join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode_of(&loose).unwrap(), 0o755);

        private_dir(&loose).unwrap();
        assert_eq!(mode_of(&loose).unwrap(), 0o700);
    }

    #[test]
    fn an_existing_world_readable_file_is_repaired() {
        let root = scratch("repair-file");
        let token = root.join("token");
        fs::write(&token, b"secret\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();

        harden_file(&token).unwrap();
        assert_eq!(mode_of(&token).unwrap(), 0o600);
        // Repair must not disturb what is in the file.
        assert_eq!(fs::read_to_string(&token).unwrap(), "secret\n");
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let root = scratch("missing");
        harden_file(&root.join("never-existed")).unwrap();
    }

    #[test]
    fn writing_produces_an_owner_only_file() {
        let root = scratch("write");
        let path = root.join("token");
        write_private(&path, b"abc").unwrap();
        assert_eq!(mode_of(&path).unwrap(), 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "abc");
    }

    #[test]
    fn writing_over_a_loose_file_tightens_it() {
        // `OpenOptions::mode` is ignored when the file already exists, so
        // without the repair a pre-existing `0644` token would stay `0644`
        // through every rewrite.
        let root = scratch("rewrite");
        let path = root.join("token");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        write_private(&path, b"new").unwrap();
        assert_eq!(mode_of(&path).unwrap(), 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn touching_creates_without_truncating() {
        let root = scratch("touch");
        let path = root.join("events.db");
        fs::write(&path, b"payload").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        touch_private(&path).unwrap();
        assert_eq!(mode_of(&path).unwrap(), 0o600);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "payload",
            "touching an existing database must never empty it"
        );
    }

    #[test]
    fn a_symlink_is_never_chmoded_through() {
        // The classic escalation: replace a path the daemon repairs at startup
        // with a link to something else and let it do the chmod for you.
        let root = scratch("symlink");
        let target = root.join("victim");
        fs::write(&target, b"x").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = harden_file(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            mode_of(&target).unwrap(),
            0o644,
            "the link target must be untouched"
        );
    }
}
