//! Files a person sends a session's agent (minor 50, `put_file`).
//!
//! An upload lands under `~/.codeconnect/uploads/<session_uid>/<upload_id>/<name>`, never in
//! the run's folder. It is bound to the connection and the run of its first chunk, written to
//! [`PART`] in its own folder as its chunks arrive in order, and renamed to `<name>` at exactly the size
//! its first chunk named. A message on that connection attaches it by id: from then it is the
//! message's ([`Uploads::claim`]), and the agent reads it at its path, which the message
//! carries as a line of its text ([`attached`]). An upload no message attached is removed
//! when its connection closes; a run's whole folder goes with the run.
//!
//! An upload sent before its run exists, for a start in the start box, names no session: it is
//! held under [`PENDING`], its connection's alone, and the start that attaches it moves it into
//! the new run's folder by rename, under the same id, before the run's first message is written
//! ([`Uploads::adopt`]).
//!
//! The daemon enforces every bound here; a client's own checks are a courtesy to the person.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::ws::{MAX_ATTACHMENTS, MAX_UPLOAD_BYTES, MAX_UPLOAD_CHUNK_BYTES};

/// The longest file name an upload may have, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// What an upload is written to in its own folder until it is whole: one name, so a person's
/// file name of any allowed length fits beside it, and no name an upload may have, since none
/// starts with a dot.
const PART: &str = ".part";

/// The folder, in place of a run's uid, of uploads sent before their run exists: no run's uid
/// starts with a dot.
pub const PENDING: &str = ".pending";

/// The uploads a start attaches: sent from `connection` before the run was named, in order.
#[derive(Clone, Copy, Default)]
pub struct Pending<'a> {
    pub connection: u64,
    pub ids: &'a [String],
}

/// Where uploads live: `~/.codeconnect/uploads`.
pub fn root() -> PathBuf {
    protocol::root_dir().join("uploads")
}

/// `session_uid`'s folder under `root`, made `0700` when it is not there yet: the one folder its
/// Claude is given to read beside its own (`additionalDirectories`).
pub fn session_folder(root: &Path, session_uid: &str) -> std::io::Result<PathBuf> {
    let folder = root.join(session_uid);
    protocol::fsperm::private_dir(&folder)?;
    Ok(folder)
}

/// Remove `session_uid`'s uploads, with its run.
pub fn remove_session(root: &Path, session_uid: &str) {
    match std::fs::remove_dir_all(root.join(session_uid)) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => crate::log_error!("could not remove {session_uid}'s uploads: {err}"),
    }
}

/// Remove the start box's uploads no start took, when ccd starts: each was its connection's,
/// and no connection outlives the process that held it.
pub fn remove_pending(root: &Path) {
    remove_session(root, PENDING);
}

/// The line before a message's attached files' paths ([`attached`]).
pub const ATTACHED: &str = "Attached:";

/// `text` with each attached file's path on a line after [`ATTACHED`], as the agent is sent it.
pub fn attached(text: String, paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return text;
    }
    let mut lines = String::from(ATTACHED);
    for path in paths {
        lines.push('\n');
        lines.push_str(&path.to_string_lossy());
    }
    if text.is_empty() {
        lines
    } else {
        format!("{text}\n\n{lines}")
    }
}

/// One path component a person's device chose: not empty, not starting with a dot, no
/// separator, no control character (a name is a line of the agent's message), at most
/// [`MAX_NAME_BYTES`].
fn is_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && !value.starts_with('.')
        && !value
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

struct Upload {
    connection: u64,
    session_uid: String,
    name: String,
    size: u64,
    received: u64,
    /// Its [`PART`] file, until the upload is complete.
    file: Option<std::fs::File>,
    /// A message attached it: no connection's close removes it.
    claimed: bool,
}

/// Every upload this daemon holds, by id.
#[derive(Default)]
pub struct Uploads {
    held: Mutex<HashMap<String, Upload>>,
}

impl Uploads {
    /// Write one chunk of `upload_id` for `session_uid`, from `connection`: the bytes held after
    /// it, or why it was refused, the upload then removed.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &self,
        root: &Path,
        connection: u64,
        session_uid: &str,
        upload_id: &str,
        name: &str,
        size: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, String> {
        if !is_component(upload_id) || !is_component(name) {
            return Err("an upload's id and name are each one file name".into());
        }
        let mut held = self.held.lock().unwrap();
        let refused = match held.get_mut(upload_id) {
            None => {
                if offset != 0 {
                    return Err("an upload starts at offset 0".into());
                }
                if size > MAX_UPLOAD_BYTES {
                    return Err(format!("a file is at most {MAX_UPLOAD_BYTES} bytes"));
                }
                if data.len() > MAX_UPLOAD_CHUNK_BYTES || data.len() as u64 > size {
                    return Err(format!("a chunk is at most {MAX_UPLOAD_CHUNK_BYTES} bytes"));
                }
                let file = create(root, session_uid, upload_id)
                    .map_err(|err| format!("could not keep the file: {err}"))?;
                held.insert(
                    upload_id.to_string(),
                    Upload {
                        connection,
                        session_uid: session_uid.to_string(),
                        name: name.to_string(),
                        size,
                        received: 0,
                        file: Some(file),
                        claimed: false,
                    },
                );
                None
            }
            Some(upload)
                if upload.connection != connection || upload.session_uid != session_uid =>
            {
                // Another connection's or run's upload under this id: refused, and left alone.
                return Err("that upload is not this connection's".into());
            }
            Some(upload) if upload.file.is_none() => {
                return Err("that upload is already complete".into())
            }
            Some(upload) if upload.name != name || upload.size != size => {
                Some("an upload's name and size are its first chunk's".to_string())
            }
            Some(upload) if offset != upload.received => Some(format!(
                "the next chunk starts at {}, not {offset}",
                upload.received
            )),
            Some(upload)
                if data.len() > MAX_UPLOAD_CHUNK_BYTES
                    || upload.received + data.len() as u64 > upload.size =>
            {
                Some(format!(
                    "a chunk is at most {MAX_UPLOAD_CHUNK_BYTES} bytes and ends within the file"
                ))
            }
            Some(_) => None,
        };
        if let Some(reason) = refused {
            discard(root, &mut held, upload_id);
            return Err(reason);
        }
        let upload = held.get_mut(upload_id).expect("held above");
        let written = upload
            .file
            .as_mut()
            .expect("an incomplete upload holds its file")
            .write_all(data);
        if let Err(err) = written {
            discard(root, &mut held, upload_id);
            return Err(format!("could not keep the file: {err}"));
        }
        upload.received += data.len() as u64;
        if upload.received == upload.size {
            upload.file = None;
            let folder = root.join(&upload.session_uid).join(upload_id);
            if let Err(err) = std::fs::rename(folder.join(PART), folder.join(&upload.name)) {
                discard(root, &mut held, upload_id);
                return Err(format!("could not keep the file: {err}"));
            }
        }
        Ok(held[upload_id].received)
    }

    /// Attach `ids`, in order, to a message from `connection` to `session_uid`'s run: their
    /// paths, or why none was attached. Each must be complete and this run's, and either this
    /// connection's own or already attached by a message (a retry). From here no connection's
    /// close removes them.
    pub fn claim(
        &self,
        root: &Path,
        connection: u64,
        session_uid: &str,
        ids: &[String],
    ) -> Result<Vec<PathBuf>, String> {
        if ids.len() > MAX_ATTACHMENTS {
            return Err(format!(
                "a message attaches at most {MAX_ATTACHMENTS} files"
            ));
        }
        let mut held = self.held.lock().unwrap();
        let mut paths = Vec::with_capacity(ids.len());
        for (index, id) in ids.iter().enumerate() {
            if ids[..index].contains(id) {
                return Err(format!("{id} is attached twice"));
            }
            let Some(upload) = held.get(id) else {
                return Err(format!("{id} is not an upload this computer holds"));
            };
            if upload.session_uid != session_uid {
                return Err(format!("{id} was sent for another session"));
            }
            if upload.file.is_some() {
                return Err(format!("{id} is not complete"));
            }
            if !upload.claimed && upload.connection != connection {
                return Err(format!("{id} is another connection's"));
            }
            paths.push(root.join(session_uid).join(id).join(&upload.name));
        }
        for id in ids {
            held.get_mut(id).expect("checked above").claimed = true;
        }
        Ok(paths)
    }

    /// Give the start's `pending` uploads to the run `session_uid`, which is not yet recorded: each
    /// moved by rename from [`PENDING`] into the run's folder, and attached. Their new paths, or
    /// why none was given, every one of them then where it was. Each must be complete,
    /// unattached, pending and `pending`'s connection's.
    pub fn adopt(
        &self,
        root: &Path,
        pending: Pending<'_>,
        session_uid: &str,
    ) -> Result<Vec<PathBuf>, String> {
        let ids = pending.ids;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if ids.len() > MAX_ATTACHMENTS {
            return Err(format!("a start attaches at most {MAX_ATTACHMENTS} files"));
        }
        let mut held = self.held.lock().unwrap();
        for (index, id) in ids.iter().enumerate() {
            if ids[..index].contains(id) {
                return Err(format!("{id} is attached twice"));
            }
            match held.get(id) {
                Some(upload)
                    if upload.session_uid == PENDING
                        && upload.connection == pending.connection
                        && upload.file.is_none()
                        && !upload.claimed => {}
                _ => {
                    return Err(format!(
                        "{id} is not a finished upload this start may attach"
                    ))
                }
            }
        }
        let folder = session_folder(root, session_uid)
            .map_err(|err| format!("could not make the session's uploads folder: {err}"))?;
        for (index, id) in ids.iter().enumerate() {
            if let Err(err) = std::fs::rename(root.join(PENDING).join(id), folder.join(id)) {
                // Every one moved so far goes back, so a refused start leaves them all pending.
                for moved in &ids[..index] {
                    let _ = std::fs::rename(folder.join(moved), root.join(PENDING).join(moved));
                }
                return Err(format!("could not move {id} to the session: {err}"));
            }
        }
        let mut paths = Vec::with_capacity(ids.len());
        for id in ids {
            let upload = held.get_mut(id).expect("checked above");
            upload.session_uid = session_uid.to_string();
            upload.claimed = true;
            paths.push(folder.join(id).join(&upload.name));
        }
        Ok(paths)
    }

    /// `connection` closed: remove every upload of its that no message attached.
    pub fn close_connection(&self, root: &Path, connection: u64) {
        let mut held = self.held.lock().unwrap();
        let open: Vec<String> = held
            .iter()
            .filter(|(_, upload)| upload.connection == connection && !upload.claimed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in open {
            discard(root, &mut held, &id);
        }
    }

    /// `session_uid`'s run was removed: forget its uploads and remove their folder.
    pub fn remove_session(&self, root: &Path, session_uid: &str) {
        self.held
            .lock()
            .unwrap()
            .retain(|_, upload| upload.session_uid != session_uid);
        remove_session(root, session_uid);
    }
}

/// The upload's folder, made exclusively, and its [`PART`] file in it, created `0600` and never
/// through a link.
fn create(root: &Path, session_uid: &str, upload_id: &str) -> std::io::Result<std::fs::File> {
    let folder = session_folder(root, session_uid)?.join(upload_id);
    std::fs::DirBuilder::new()
        .mode(protocol::fsperm::DIR_MODE)
        .create(&folder)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(protocol::fsperm::FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(folder.join(PART))
}

/// Forget `upload_id` and remove its folder.
fn discard(root: &Path, held: &mut HashMap<String, Upload>, upload_id: &str) {
    if let Some(upload) = held.remove(upload_id) {
        let folder = root.join(&upload.session_uid).join(upload_id);
        if let Err(err) = std::fs::remove_dir_all(&folder) {
            if err.kind() != std::io::ErrorKind::NotFound {
                crate::log_error!("could not remove upload {upload_id}: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const UID: &str = "01UPLOADTESTRUN000000000000";

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccd-uploads-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// **A file sent in two chunks lands whole, owner-only, at its own name, and only at exactly
    /// its size.** Until the last byte it is [`PART`]: nothing a message could attach.
    #[test]
    fn a_file_in_two_chunks_lands_owner_only_at_its_name_only_at_its_size() {
        let root = root("whole");
        let uploads = Uploads::default();
        assert_eq!(
            uploads.put(&root, 1, UID, "u1", "a.png", 5, 0, b"abc"),
            Ok(3)
        );
        let folder = root.join(UID).join("u1");
        assert!(!folder.join("a.png").exists());
        assert_eq!(
            uploads.claim(&root, 1, UID, &["u1".into()]),
            Err("u1 is not complete".into())
        );
        assert_eq!(
            uploads.put(&root, 1, UID, "u1", "a.png", 5, 3, b"de"),
            Ok(5)
        );
        assert_eq!(std::fs::read(folder.join("a.png")).unwrap(), b"abcde");
        assert!(!folder.join(PART).exists());
        assert_eq!(mode(&folder.join("a.png")), 0o600);
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&root.join(UID)), 0o700);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// **The start box's uploads no start took go when ccd starts again**: what the next process
    /// finds is the files, and no connection or upload in memory. A run's own files stay.
    #[test]
    fn a_restart_removes_the_pending_uploads_no_start_took() {
        let root = root("restart");
        let before = Uploads::default();
        assert_eq!(before.put(&root, 1, PENDING, "p1", "a", 1, 0, b"a"), Ok(1));
        assert_eq!(before.put(&root, 1, UID, "u1", "b", 1, 0, b"b"), Ok(1));

        remove_pending(&root);
        assert!(!root.join(PENDING).exists());
        assert_eq!(
            std::fs::read(root.join(UID).join("u1").join("b")).unwrap(),
            b"b"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// **A file whose name is as long as a name may be lands under it.** Its partial file has a
    /// name of its own, so the person's name never grows past what the filesystem takes.
    #[test]
    fn a_file_named_at_the_longest_a_name_may_be_lands() {
        let root = root("longest");
        let uploads = Uploads::default();
        let name = format!("{}.txt", "a".repeat(MAX_NAME_BYTES - 4));
        assert_eq!(uploads.put(&root, 1, UID, "u1", &name, 1, 0, b"x"), Ok(1));
        assert_eq!(
            std::fs::read(root.join(UID).join("u1").join(&name)).unwrap(),
            b"x"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// **Every bound is the daemon's**: the size, the chunk, the order, the name, and the first
    /// chunk's size and name; a refused chunk takes its upload with it.
    #[test]
    fn the_daemon_refuses_what_breaks_a_bound_and_drops_the_upload() {
        let root = root("bounds");
        let uploads = Uploads::default();
        let big = vec![0u8; MAX_UPLOAD_CHUNK_BYTES + 1];
        for (id, name, size, offset, data) in [
            ("u1", "a", MAX_UPLOAD_BYTES + 1, 0, &b"x"[..]),
            ("u2", "a", MAX_UPLOAD_BYTES, 0, &big[..]),
            ("u3", "a", 5, 1, &b"x"[..]),
            ("u4", "../a", 5, 0, &b"x"[..]),
            (".u5", "a", 5, 0, &b"x"[..]),
            ("u6", ".a", 5, 0, &b"x"[..]),
            ("u7", "a\nb", 5, 0, &b"x"[..]),
            ("u8", "a", 1, 0, &b"xy"[..]),
        ] {
            assert!(
                uploads
                    .put(&root, 1, UID, id, name, size, offset, data)
                    .is_err(),
                "{id} {name} {size} {offset}"
            );
        }
        assert_eq!(uploads.put(&root, 1, UID, "u9", "a", 5, 0, b"ab"), Ok(2));
        assert!(uploads.put(&root, 1, UID, "u9", "a", 5, 1, b"c").is_err());
        assert!(!root.join(UID).join("u9").exists());
        assert_eq!(uploads.put(&root, 1, UID, "u10", "a", 5, 0, b"ab"), Ok(2));
        assert!(uploads.put(&root, 1, UID, "u10", "a", 6, 2, b"c").is_err());
        assert!(!root.join(UID).join("u10").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **An upload is its connection's and its run's**, and a link where its file would be is
    /// never written through.
    #[test]
    fn an_upload_is_its_connections_and_runs_and_no_link_is_written_through() {
        let root = root("bound");
        let uploads = Uploads::default();
        assert_eq!(uploads.put(&root, 1, UID, "u1", "a", 4, 0, b"ab"), Ok(2));
        assert!(uploads.put(&root, 2, UID, "u1", "a", 4, 2, b"cd").is_err());
        assert!(uploads
            .put(
                &root,
                1,
                "01OTHERRUN0000000000000000",
                "u1",
                "a",
                4,
                2,
                b"cd"
            )
            .is_err());
        assert_eq!(uploads.put(&root, 1, UID, "u1", "a", 4, 2, b"cd"), Ok(4));
        assert_eq!(
            uploads.claim(&root, 2, UID, &["u1".into()]),
            Err("u1 is another connection's".into())
        );
        assert_eq!(
            uploads.claim(&root, 1, "01OTHERRUN0000000000000000", &["u1".into()]),
            Err("u1 was sent for another session".into())
        );

        let target = root.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let folder = root.join(UID).join("u2");
        std::fs::create_dir_all(&folder).unwrap();
        std::os::unix::fs::symlink(&target, folder.join(PART)).unwrap();
        assert!(uploads
            .put(&root, 1, UID, "u2", "b", 4, 0, b"evil")
            .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A message owns what it attached**: a closing connection removes only uploads no message
    /// attached, and an attached one can be attached again (a retry) from another connection.
    #[test]
    fn a_closing_connection_removes_only_what_no_message_attached() {
        let root = root("claim");
        let uploads = Uploads::default();
        assert_eq!(uploads.put(&root, 1, UID, "kept", "a", 1, 0, b"a"), Ok(1));
        assert_eq!(uploads.put(&root, 1, UID, "left", "b", 1, 0, b"b"), Ok(1));
        assert_eq!(uploads.put(&root, 1, UID, "half", "c", 2, 0, b"c"), Ok(1));
        let paths = uploads.claim(&root, 1, UID, &["kept".into()]).unwrap();
        assert_eq!(paths, vec![root.join(UID).join("kept").join("a")]);
        uploads.close_connection(&root, 1);
        assert!(root.join(UID).join("kept").join("a").exists());
        assert!(!root.join(UID).join("left").exists());
        assert!(!root.join(UID).join("half").exists());
        assert_eq!(uploads.claim(&root, 2, UID, &["kept".into()]), Ok(paths));
        assert!(uploads.claim(&root, 2, UID, &["left".into()]).is_err());
        uploads.remove_session(&root, UID);
        assert!(!root.join(UID).exists());
        assert!(uploads.claim(&root, 2, UID, &["kept".into()]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A file sent before its run exists waits pending, its connection's alone, and a start
    /// moves it into the run's folder.** A message cannot attach it; another connection's start
    /// cannot take it; a start naming one file it may not take moves none; the moved file is the
    /// run's, and a closing connection no longer removes it.
    #[test]
    fn a_pending_upload_moves_whole_into_the_run_its_start_names() {
        let root = root("pending");
        let uploads = Uploads::default();
        assert_eq!(
            uploads.put(&root, 1, PENDING, "p1", "a.png", 2, 0, b"ab"),
            Ok(2)
        );
        assert_eq!(
            uploads.put(&root, 1, PENDING, "p2", "b.txt", 1, 0, b"b"),
            Ok(1)
        );
        assert_eq!(
            uploads.put(&root, 1, PENDING, "half", "c", 2, 0, b"c"),
            Ok(1)
        );
        assert_eq!(mode(&root.join(PENDING).join("p1").join("a.png")), 0o600);
        assert_eq!(
            uploads.claim(&root, 1, UID, &["p1".into()]),
            Err("p1 was sent for another session".into())
        );
        let both = ["p1".to_string(), "p2".to_string()];
        let theirs = Pending {
            connection: 2,
            ids: &both,
        };
        assert!(uploads.adopt(&root, theirs, UID).is_err());
        let with_half = ["p1".to_string(), "half".to_string()];
        let mixed = Pending {
            connection: 1,
            ids: &with_half,
        };
        assert!(uploads.adopt(&root, mixed, UID).is_err());
        assert!(root.join(PENDING).join("p1").join("a.png").exists());
        let mine = Pending {
            connection: 1,
            ids: &both,
        };
        // The second cannot move (something is already where it would go): the first goes back.
        let blocker = root.join(UID).join("p2").join("x");
        std::fs::create_dir_all(&blocker).unwrap();
        assert!(uploads.adopt(&root, mine, UID).is_err());
        assert!(root.join(PENDING).join("p1").join("a.png").exists());
        assert!(!root.join(UID).join("p1").exists());
        std::fs::remove_dir_all(root.join(UID).join("p2")).unwrap();
        let paths = uploads.adopt(&root, mine, UID).unwrap();
        assert_eq!(
            paths,
            vec![
                root.join(UID).join("p1").join("a.png"),
                root.join(UID).join("p2").join("b.txt")
            ]
        );
        assert!(!root.join(PENDING).join("p1").exists());
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"ab");
        uploads.close_connection(&root, 1);
        assert!(paths[1].exists());
        assert!(!root.join(PENDING).join("half").exists());
        assert!(uploads
            .adopt(&root, mine, "01OTHERRUN0000000000000000")
            .is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The attachments reach the agent as path lines, and only then.** No attachment leaves the
    /// text as it was; attachments alone are the whole text.
    #[test]
    fn attachments_are_path_lines_after_the_text() {
        let path = PathBuf::from("/h/.codeconnect/uploads/r/u1/a.png");
        assert_eq!(attached("hi".into(), &[]), "hi");
        assert_eq!(
            attached("hi".into(), std::slice::from_ref(&path)),
            "hi\n\nAttached:\n/h/.codeconnect/uploads/r/u1/a.png"
        );
        assert_eq!(
            attached(String::new(), &[path.clone(), path]),
            "Attached:\n/h/.codeconnect/uploads/r/u1/a.png\n/h/.codeconnect/uploads/r/u1/a.png"
        );
    }
}
