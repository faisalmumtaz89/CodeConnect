//! Replacing the installed binaries, all three or none of them.
//!
//! **One commit point, not three.** The obvious implementation renames each new
//! binary over its old one, and that is three atomic operations rather than one
//! atomic installation: a power loss or a `SIGKILL` between the second and the
//! third leaves `codeconnect` and `ccd` from the new release running against a
//! `cc-hook` from the old one. Nothing in the product would notice, and the
//! mixture is not a state anybody tested.
//!
//! So the whole directory is exchanged instead. A complete, verified set is
//! staged in a sibling directory on the same filesystem, and one
//! `renameatx_np(RENAME_SWAP)` makes it live. Every lookup of
//! `~/.codeconnect/bin/ccd` sees either the complete old set or the complete
//! new one — including a lookup that happens the instant the machine loses
//! power.
//!
//! **Why an exchange rather than a plain rename.** The swap leaves the previous
//! set in the staging directory rather than deleting it, so the rollback after
//! a failed smoke test is the same operation run again. And it never unlinks
//! the live path: a rename that removed `bin` first would leave a window in
//! which `codeconnect` does not exist, which is precisely the window a user
//! would hit by pressing Ctrl-C.
//!
//! **Why not overwrite in place.** macOS caches a binary's code signature
//! against its inode. Writing new bytes into the existing inode leaves the
//! cached signature describing bytes that are no longer there, and the kernel
//! then kills every subsequent exec with no diagnostic — see the note at the
//! top of `mac/install.sh`. Exchanging directories installs new inodes and
//! never touches the old ones.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The Apple team whose signature is accepted on a release binary.
///
/// **A constant in the source, not a value baked at build time.** This is a
/// trust policy — whom this program believes — and it belongs where a reader
/// reviewing the source can see it. A build-time value would let a
/// misconfigured or hostile build environment quietly change whom every
/// existing install trusts, which is the opposite of what pinning is for.
pub const TEAM_ID: &str = "2XL2264MC8";

/// What a release binary's signature must satisfy, in Apple's requirement
/// language.
///
/// **Three clauses, and dropping any one of them makes it meaningless.**
///
///   * `anchor apple generic` — the chain ends at Apple. Without it the whole
///     expression can be satisfied by a certificate anybody can mint.
///   * the two OIDs — the issuing CA is Apple's *Developer ID* CA and the leaf
///     is a *Developer ID Application* certificate. Without them any Apple
///     certificate qualifies, including the free Apple Development certificate
///     that every developer account can issue in a minute.
///   * `subject.OU` — that certificate belongs to *this* team.
///
/// A bare `codesign --verify` accepts any internally consistent signature —
/// ad-hoc, Apple Development, anybody's — and only this requirement refuses
/// them. The refusal is proven by a test below against a real ad-hoc
/// signature.
pub fn signature_requirement() -> String {
    format!(
        "anchor apple generic \
         and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
         and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
         and certificate leaf[subject.OU] = \"{TEAM_ID}\""
    )
}

/// Whether this file is signed by CodeConnect and unmodified since.
///
/// `--all-architectures` because a universal binary carries one signature per
/// slice and the host would otherwise only ever check its own: an x86_64 half
/// could be replaced wholesale and an arm64 Mac would never look at it.
pub fn verify_signature(binary: &Path) -> Result<()> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .arg("--verify")
        .arg("--strict")
        .arg("--all-architectures")
        .arg(format!("-R={}", signature_requirement()))
        .arg(binary)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running codesign against {}", binary.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "{} is not signed by CodeConnect (team {TEAM_ID}): {}",
        binary.display(),
        why.trim()
    )
}

/// Fetch one file, refusing anything that is not plain https or is larger than
/// it said it would be.
///
/// `-q` first, always: without it curl reads `~/.curlrc`, which can add URLs,
/// attach headers or redirect output — and an update is not something a dotfile
/// gets to redirect. `--proto '=https'` and `--proto-redir` keep every hop on
/// https, so a redirect cannot walk the download down to plaintext.
pub fn download(url: &str, into: &Path, max_bytes: u64) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/curl")
        .args([
            "-q",
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            "300",
            "--max-filesize",
        ])
        .arg(max_bytes.to_string())
        .arg("-o")
        .arg(into)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("fetching {url}"))?;
    if !status.success() {
        anyhow::bail!("could not download {url}");
    }
    Ok(())
}

/// The SHA-256 of a file, streamed rather than held in memory.
pub fn digest_of(path: &Path) -> Result<[u8; 32]> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to hash it", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("hashing the archive")?;
    Ok(hasher.finalize().into())
}

/// What the archive says it contains, without unpacking any of it.
///
/// The listing pass exists so the whole archive can be judged before a byte is
/// written; see `update_release::validate_members`.
pub fn list_members(archive: &Path) -> Result<Vec<crate::update_release::Member>> {
    let output = std::process::Command::new("/usr/bin/tar")
        .arg("-tvzf")
        .arg(archive)
        .stdin(std::process::Stdio::null())
        .output()
        .context("listing the archive")?;
    if !output.status.success() {
        anyhow::bail!(
            "the archive could not be read: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut members = Vec::new();
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        // `tar -tv` prints: mode links owner group size month day time name.
        // Measured against the system tar rather than assumed — reading the
        // wrong column silently reports every member as zero bytes, which
        // makes a size bound that looks present do nothing at all.
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 9 {
            anyhow::bail!("the archive listing has a line this cannot read: {line}");
        }
        let size = fields[4]
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("the archive listing has no size for: {line}"))?;
        // Everything from the ninth field on, so a name containing spaces is
        // carried whole rather than truncated into a different name. A link
        // prints `name -> target` and is refused by type before its path is
        // ever used.
        let name = fields[8..].join(" ");
        members.push(crate::update_release::Member {
            path: name.trim_end_matches('/').to_string(),
            is_regular_file: fields[0].starts_with('-'),
            size,
        });
    }
    if members.is_empty() {
        anyhow::bail!("the archive is empty");
    }
    Ok(members)
}

/// Unpack an archive that has already been judged, into a directory nothing
/// else can see.
pub fn extract(archive: &Path, into: &Path) -> Result<()> {
    std::fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .stdin(std::process::Stdio::null())
        .status()
        .context("unpacking the archive")?;
    if !status.success() {
        anyhow::bail!("the archive could not be unpacked");
    }
    Ok(())
}

/// What a freshly extracted binary says it is.
pub fn version_of(binary: &Path) -> Result<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {} --version", binary.display()))?;
    if !output.status.success() {
        anyhow::bail!("{} does not run", binary.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A universal file's header. Always big-endian on disk, whatever the host is.
const FAT_MAGIC: u32 = 0xcafe_babe;
/// The same header with 64-bit offsets: wider entries, identical `cputype`.
const FAT_MAGIC_64: u32 = 0xcafe_babf;
/// A single-architecture 64-bit Mach-O, in the host's byte order. Every binary
/// macOS runs today is this or a fat file containing several of them.
const MH_MAGIC_64: u32 = 0xfeed_facf;
/// A fat header names one architecture per entry. Apple ships two; the largest
/// real files carry a handful. A header claiming thousands is malformed or
/// hostile, not something to allocate for.
const MAX_FAT_ARCHS: u32 = 32;

const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// The architectures a Mach-O file carries, as `cputype` values.
///
/// Only the `cputype` of each entry is read. The subtype is what distinguishes
/// `arm64` from `arm64e`, and that distinction is not the question here: both
/// are the same machine.
fn architectures(binary: &Path) -> Result<Vec<u32>> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(binary).with_context(|| format!("opening {}", binary.display()))?;
    let mut head = [0u8; 8];
    file.read_exact(&mut head)
        .with_context(|| format!("{} is too short to be a Mach-O", binary.display()))?;
    let leading: [u8; 4] = head[..4].try_into().expect("four bytes of eight");

    // A fat header is big-endian by definition; a thin one is written in the
    // host's order, which on every Mac this runs on is little-endian.
    let fat = u32::from_be_bytes(leading);
    if fat == FAT_MAGIC || fat == FAT_MAGIC_64 {
        let count = u32::from_be_bytes(head[4..].try_into().expect("the other four"));
        if count == 0 || count > MAX_FAT_ARCHS {
            anyhow::bail!(
                "{} claims {count} architectures, which is not a file this will install",
                binary.display()
            );
        }
        // `fat_arch` is 20 bytes and `fat_arch_64` is 32; in both, `cputype` is
        // the first four.
        let width = if fat == FAT_MAGIC_64 { 32 } else { 20 };
        let mut entries = vec![0u8; width * count as usize];
        file.read_exact(&mut entries).with_context(|| {
            format!(
                "{} names {count} architectures but ends before describing them",
                binary.display()
            )
        })?;
        return Ok(entries
            .chunks_exact(width)
            .map(|entry| u32::from_be_bytes(entry[..4].try_into().expect("four bytes")))
            .collect());
    }

    if u32::from_le_bytes(leading) == MH_MAGIC_64 {
        return Ok(vec![u32::from_le_bytes(
            head[4..].try_into().expect("the other four"),
        )]);
    }

    anyhow::bail!("{} is not a Mach-O this can read", binary.display())
}

fn arch_name(cputype: u32) -> String {
    match cputype {
        CPU_TYPE_ARM64 => "arm64".to_string(),
        CPU_TYPE_X86_64 => "x86_64".to_string(),
        other => format!("cputype {other:#010x}"),
    }
}

/// Whether this file carries both architectures a release ships.
///
/// The release workflow asserts this too, but the updater installs what it
/// downloads rather than what a workflow once produced — and a validly signed
/// thin binary would run on the machine that built it and refuse to launch on
/// the other half of the fleet.
///
/// **Read out of the header rather than asked of `lipo`.** `/usr/bin/lipo` is
/// one of the `xcrun` shims: on a Mac with no developer directory it answers
/// `xcrun: error` and exits non-zero instead of describing the file.
/// `codeconnect update` states that it needs no toolchain, and a check that
/// only works where Xcode is installed would make that false on exactly the
/// machines the promise is for.
pub fn is_universal(binary: &Path) -> Result<()> {
    let archs = architectures(binary)?;
    if archs.contains(&CPU_TYPE_ARM64) && archs.contains(&CPU_TYPE_X86_64) {
        return Ok(());
    }
    anyhow::bail!(
        "{} carries {} rather than both architectures a release ships",
        binary.display(),
        archs
            .iter()
            .map(|arch| arch_name(*arch))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// Tidy up after an update that was interrupted.
///
/// **The exchange is atomic; the tidying after it is not.** A process killed
/// between the swap and the smoke test leaves the new set live and the previous
/// set sitting in `bin.incoming`. Nothing is broken — the live set is complete
/// and verified — but the leftovers would otherwise sit there forever, and the
/// next update would stage on top of them.
///
/// So the next update clears them first — under the update lock, before it
/// reads or stages anything. It never exchanges: a directory left over from an
/// interrupted run is not a rollback anybody asked for, and swapping it back
/// would undo an install that succeeded.
/// **Fail-closed, not best-effort.** A directory that could not be cleared is a
/// directory the next step builds on top of, and whatever survived in it would
/// be exchanged into place as if this run had verified it. Reporting that the
/// tidy-up failed and continuing anyway is how a half-deleted `bin.incoming`
/// becomes an installed set.
pub fn clear_leftovers(root: &Path) -> Result<()> {
    for path in [staging_dir(root), root.join("update.work")] {
        if path.exists() {
            eprintln!("clearing {} left by an interrupted update", path.display());
        }
        remove_tree(&path)?;
    }
    Ok(())
}

/// Remove a directory and everything under it. Absent is success; every other
/// failure is a failure.
pub fn remove_tree(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("clearing {}", path.display())),
    }
}

/// Whether every installed binary is the named release, and the same build of
/// it.
///
/// A binary that cannot be asked counts as not current: an update that skipped
/// itself because a file was missing would leave the gap it was run to close.
///
/// **The build has to agree too, not just the version.** Three binaries can all
/// say `0.4.0` and still come from three different commits — which is precisely
/// what a half-finished hand copy leaves behind, and a set no release ever
/// produced. Equal version numbers are not evidence of a matching set, so a
/// mixture reads as "not current" and is repaired rather than blessed.
pub fn set_is_current(set: &[(String, Option<String>)], version: &str) -> bool {
    if set.is_empty() {
        return false;
    }
    let mut common: Option<&str> = None;
    for (binary, said) in set {
        let Some(build) = said
            .as_deref()
            .and_then(|said| reported_build(said, binary, version))
        else {
            return false;
        };
        match common {
            None => common = Some(build),
            Some(seen) if seen == build => {}
            Some(_) => return false,
        }
    }
    true
}

/// The build this `--version` line names, when the line is exactly the form a
/// release binary prints.
///
/// **Exact, not a substring.** `0.4.0` is contained in `10.4.0`, and a
/// containment test would also accept the right version printed by the wrong
/// binary, or surrounded by anything at all. A build that calls itself
/// `build unknown` or `-dirty` is refused here too: a release is neither.
/// Lowercase because that is what git prints and therefore the only thing
/// `build.rs` can bake in; accepting `ABCDEF` would be accepting a line no
/// binary produces.
pub fn reported_build<'a>(said: &'a str, binary: &str, version: &str) -> Option<&'a str> {
    let (head, build) = said.rsplit_once(" (")?;
    let build = build.strip_suffix(')')?;
    (head == format!("{binary} {version}")
        && build.len() == 12
        && build
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(build)
}

/// Where the incoming set is assembled, beside the set it will replace.
///
/// A sibling of `bin` rather than a temporary directory elsewhere, because an
/// exchange requires both paths on one filesystem.
pub fn staging_dir(root: &Path) -> PathBuf {
    root.join("bin.incoming")
}

/// Exchange two directories in one operation.
///
/// After this returns, `live` holds what `staged` held and `staged` holds what
/// `live` held — which is what makes rollback the same call again.
pub fn exchange(staged: &Path, live: &Path) -> io::Result<()> {
    let staged_c = std::ffi::CString::new(staged.as_os_str().as_bytes())?;
    let live_c = std::ffi::CString::new(live.as_os_str().as_bytes())?;
    // SAFETY: both pointers are NUL-terminated strings that outlive the call,
    // and `AT_FDCWD` makes the absolute paths absolute.
    let rc = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            staged_c.as_ptr(),
            libc::AT_FDCWD,
            live_c.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    // A filesystem that cannot exchange atomically cannot install atomically.
    // Falling back to three renames would silently give up the one property
    // this module exists to provide, so it refuses instead — having changed
    // nothing.
    if err.raw_os_error() == Some(libc::ENOTSUP) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{} cannot exchange directories atomically, so this update \
                 cannot be installed without risking a half-replaced set",
                live.display()
            ),
        ));
    }
    Err(err)
}

/// Push a file's bytes, and then its directory entry, to disk.
///
/// **Both, in that order.** Flushing the file leaves the name still only in the
/// directory's page cache, so a power loss can produce a directory entry
/// pointing at a file that was never written.
pub fn sync_tree(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::File::open(entry.path())?.sync_all()?;
        }
    }
    std::fs::File::open(dir)
        .with_context(|| format!("opening {} to flush it", dir.display()))?
        .sync_all()
        .with_context(|| format!("flushing {}", dir.display()))?;
    Ok(())
}

/// Anything already in `bin` that a release does not ship, carried across so an
/// exchange does not silently delete it.
///
/// `bin` is CodeConnect's directory, but it is inside the user's home and
/// nothing stops them putting a script beside the binaries. Losing that to an
/// update they asked for would be the update taking something that was not
/// offered.
pub fn carry_over_strangers(live: &Path, staged: &Path, shipped: &[&str]) -> Result<Vec<String>> {
    let mut carried = Vec::new();
    if !live.is_dir() {
        return Ok(carried);
    }
    for entry in std::fs::read_dir(live)? {
        let entry = entry?;
        let name = entry.file_name();
        let as_text = name.to_string_lossy().to_string();
        if shipped.contains(&as_text.as_str()) {
            continue;
        }
        let target = staged.join(&name);
        if target.exists() {
            continue;
        }
        // **Refused rather than quietly dropped.** A directory or a symlink
        // cannot be carried across by a file copy, and the exchange deletes
        // whatever is left behind — so an update that met one would silently
        // take something the user put there. Stopping costs them an update;
        // continuing costs them the file.
        if !entry.file_type()?.is_file() {
            anyhow::bail!(
                "{} holds {}, which an update cannot move safely. Move it elsewhere \
                 and run this again; nothing was changed.",
                live.display(),
                as_text
            );
        }
        std::fs::copy(entry.path(), &target)?;
        carried.push(as_text);
    }
    Ok(carried)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-install-{}-{}-{name}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    /// **The whole set changes hands at once**, and the previous set survives
    /// in the staging directory — which is what makes rollback possible.
    #[test]
    fn an_exchange_swaps_both_directories() {
        let root = scratch("swap");
        let live = root.join("bin");
        let staged = staging_dir(&root);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        write(&live, "codeconnect", "old");
        write(&staged, "codeconnect", "new");

        exchange(&staged, &live).expect("the exchange succeeds on a temp filesystem");

        assert_eq!(read(&live, "codeconnect"), "new", "the new set is live");
        assert_eq!(read(&staged, "codeconnect"), "old", "the old set is kept");
    }

    /// Rollback is the same call again, which is why it cannot half-succeed.
    #[test]
    fn exchanging_twice_restores_the_original() {
        let root = scratch("rollback");
        let live = root.join("bin");
        let staged = staging_dir(&root);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        write(&live, "ccd", "old");
        write(&staged, "ccd", "new");

        exchange(&staged, &live).unwrap();
        exchange(&staged, &live).unwrap();

        assert_eq!(read(&live, "ccd"), "old", "the original set is back");
    }

    /// A partial set never becomes live: the three binaries move together
    /// because the directory holding them moves.
    #[test]
    fn every_binary_changes_in_the_same_operation() {
        let root = scratch("all-three");
        let live = root.join("bin");
        let staged = staging_dir(&root);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        for binary in ["codeconnect", "ccd", "cc-hook"] {
            write(&live, binary, "0.1.0");
            write(&staged, binary, "0.2.0");
        }

        exchange(&staged, &live).unwrap();

        for binary in ["codeconnect", "ccd", "cc-hook"] {
            assert_eq!(read(&live, binary), "0.2.0", "{binary} moved with the rest");
        }
    }

    /// An update the user asked for must not take something they did not offer.
    #[test]
    fn a_file_the_release_does_not_ship_survives_the_exchange() {
        let root = scratch("strangers");
        let live = root.join("bin");
        let staged = staging_dir(&root);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        write(&live, "codeconnect", "old");
        write(&live, "my-wrapper.sh", "#!/bin/sh");
        write(&staged, "codeconnect", "new");

        let carried = carry_over_strangers(&live, &staged, &["codeconnect"]).unwrap();
        assert_eq!(carried, vec!["my-wrapper.sh".to_string()]);
        exchange(&staged, &live).unwrap();

        assert_eq!(read(&live, "codeconnect"), "new");
        assert_eq!(read(&live, "my-wrapper.sh"), "#!/bin/sh", "kept");
    }

    /// The requirement is not a string this code merely stores: `codesign` has
    /// to accept it as a requirement, and a typo would otherwise only surface
    /// on a user's machine at the moment an update was refused.
    #[test]
    fn the_pinned_requirement_is_one_codesign_understands() {
        use std::io::Write;
        let mut child = std::process::Command::new("/usr/bin/csreq")
            .args(["-r-", "-b", "/dev/null"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("csreq is part of the OS");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(signature_requirement().as_bytes())
            .unwrap();
        assert!(
            child.wait().unwrap().success(),
            "codesign would reject the pinned requirement: {}",
            signature_requirement()
        );
    }

    #[test]
    fn a_version_line_is_matched_exactly_and_not_by_containment() {
        let build = |said| reported_build(said, "codeconnect", "0.4.0");
        assert_eq!(
            build("codeconnect 0.4.0 (abcdef123456)"),
            Some("abcdef123456")
        );
        // The failure a containment test would wave through.
        assert_eq!(build("codeconnect 10.4.0 (abcdef123456)"), None);
        // The right version, the wrong binary.
        assert_eq!(build("ccd 0.4.0 (abcdef123456)"), None);
        // A release is never one of these.
        assert_eq!(build("codeconnect 0.4.0 (build unknown)"), None);
        assert_eq!(build("codeconnect 0.4.0 (abcdef123456-dirty)"), None);
        assert_eq!(build("noise codeconnect 0.4.0 (abcdef123456)"), None);
        // Git prints lowercase, so `build.rs` can only ever bake lowercase.
        assert_eq!(build("codeconnect 0.4.0 (ABCDEF123456)"), None);
    }

    /// A set is current only when every binary in it is — the skew a check on
    /// the running binary alone would bless and could not repair.
    #[test]
    fn a_skewed_install_is_not_current() {
        let all = |a: &str, b: &str, c: &str| {
            vec![
                ("codeconnect".to_string(), Some(a.to_string())),
                ("ccd".to_string(), Some(b.to_string())),
                ("cc-hook".to_string(), Some(c.to_string())),
            ]
        };
        assert!(set_is_current(
            &all(
                "codeconnect 0.4.0 (abcdef123456)",
                "ccd 0.4.0 (abcdef123456)",
                "cc-hook 0.4.0 (abcdef123456)"
            ),
            "0.4.0"
        ));
        assert!(!set_is_current(
            &all(
                "codeconnect 0.4.0 (abcdef123456)",
                "ccd 0.3.0 (abcdef123456)",
                "cc-hook 0.4.0 (abcdef123456)"
            ),
            "0.4.0"
        ));
        // One that cannot be asked is not current either.
        let mut missing = all(
            "codeconnect 0.4.0 (abcdef123456)",
            "ccd 0.4.0 (abcdef123456)",
            "cc-hook 0.4.0 (abcdef123456)",
        );
        missing[2].1 = None;
        assert!(!set_is_current(&missing, "0.4.0"));
        assert!(!set_is_current(&[], "0.4.0"));
    }

    /// An interrupted update leaves a complete live set and a stale staging
    /// directory. The next run clears it — and must never exchange it back,
    /// because that would undo an install that actually succeeded.
    #[test]
    fn leftovers_from_an_interrupted_update_are_cleared_not_restored() {
        let root = scratch("leftovers");
        let live = root.join("bin");
        let staged = staging_dir(&root);
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        write(&live, "codeconnect", "new");
        write(&staged, "codeconnect", "old");
        std::fs::create_dir_all(root.join("update.work")).unwrap();

        clear_leftovers(&root).expect("there is nothing here that cannot be removed");

        assert!(!staged.exists(), "the stale staging directory is gone");
        assert!(
            !root.join("update.work").exists(),
            "and so is the work directory"
        );
        assert_eq!(
            read(&live, "codeconnect"),
            "new",
            "the set that was installed stays installed"
        );
    }

    /// **The two tar shapes, against the real listing parser.** Naming the
    /// four files writes four regular members; `tar -czf archive dir` also
    /// writes a directory member, and the validator allows only the four.
    /// This proves the member-shape rule and nothing more — the updater's
    /// other refusals (file type, signature, architecture, version) have
    /// their own tests, and the full sequence is proven by the transport
    /// suite in `update_check`.
    #[test]
    fn the_way_a_release_is_packaged_is_the_way_the_updater_reads_it() {
        let root = scratch("packaging");
        let version = "9.9.9";
        let dir = root.join(format!("codeconnect-{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["codeconnect", "ccd", "cc-hook", "LICENSE"] {
            std::fs::write(dir.join(name), b"contents").unwrap();
        }

        let tar = |args: &[&str], out: &str| {
            let ok = std::process::Command::new("/usr/bin/tar")
                .arg("-czf")
                .arg(root.join(out))
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("COPYFILE_DISABLE", "1")
                .status()
                .expect("tar runs");
            assert!(ok.success(), "tar failed for {out}");
        };

        // Exactly how the release workflow packages it.
        tar(
            &[
                "codeconnect-9.9.9/codeconnect",
                "codeconnect-9.9.9/ccd",
                "codeconnect-9.9.9/cc-hook",
                "codeconnect-9.9.9/LICENSE",
            ],
            "good.tar.gz",
        );
        let members = list_members(&root.join("good.tar.gz")).expect("the listing parses");
        assert_eq!(members.len(), 4, "four members: {members:?}");
        assert!(
            members.iter().all(|m| m.is_regular_file && m.size == 8),
            "mode and size are read from the right columns: {members:?}"
        );
        crate::update_release::validate_members(&members, version)
            .expect("the packaged shape is one the updater installs");

        // The obvious way, which writes a directory member the updater refuses.
        tar(&["codeconnect-9.9.9"], "with-dir.tar.gz");
        let members = list_members(&root.join("with-dir.tar.gz")).unwrap();
        assert!(
            crate::update_release::validate_members(&members, version).is_err(),
            "packaging the directory must be refused, not silently installed"
        );
    }

    /// **The check that matters, run against a real signature.** An ad-hoc
    /// signature satisfies a bare `codesign --verify --strict`; the pinned
    /// requirement refuses it. That is the whole claim this test makes, and it
    /// is the negative half: it proves the requirement rejects something
    /// `codesign` alone accepts, so the pin is load-bearing rather than
    /// decorative.
    ///
    /// The positive half — that a genuine Developer ID signature *passes* —
    /// cannot be proven here, because producing one needs a private key that
    /// does not belong in a repository. The release workflow runs this exact
    /// requirement against the real signed binaries, which is where that half
    /// is proven.
    #[test]
    fn a_signature_that_is_not_developer_id_is_refused() {
        let root = scratch("signature");
        let probe = root.join("probe");
        // This test binary: a real Mach-O, produced by the real compiler, and
        // already on disk. Building one with `cc` would make the test depend on
        // the command line tools — the very dependency `is_universal` exists to
        // avoid — and would have to decide what to do when they are absent.
        std::fs::copy(
            std::env::current_exe().expect("a test binary has a path"),
            &probe,
        )
        .expect("copying this test binary");

        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&probe)
            .output()
            .expect("codesign runs");
        assert!(signed.status.success(), "the probe could be ad-hoc signed");

        // Bare verification accepts it; ours must not.
        let bare = std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&probe)
            .output()
            .unwrap();
        assert!(
            bare.status.success(),
            "the premise: it is a valid signature"
        );
        assert!(
            verify_signature(&probe).is_err(),
            "an ad-hoc signature must not pass a Developer ID requirement"
        );
    }

    /// A fat header, byte for byte as one appears on disk: big-endian
    /// throughout, `magic` then `nfat_arch`, then one entry per architecture
    /// whose first four bytes are the `cputype`.
    fn fat_header(magic: u32, cputypes: &[u32], claimed: Option<u32>) -> Vec<u8> {
        let width = if magic == FAT_MAGIC_64 { 32 } else { 20 };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&magic.to_be_bytes());
        bytes.extend_from_slice(&claimed.unwrap_or(cputypes.len() as u32).to_be_bytes());
        for cputype in cputypes {
            let mut entry = vec![0u8; width];
            entry[..4].copy_from_slice(&cputype.to_be_bytes());
            bytes.extend_from_slice(&entry);
        }
        bytes
    }

    /// The rule, against headers laid out exactly as the loader sees them.
    /// Nothing here runs `lipo` — that is the point, and a machine with no
    /// developer directory reaches this code and no other.
    #[test]
    fn both_architectures_are_read_out_of_the_header_and_nothing_else_passes() {
        let root = scratch("universal");
        let check = |name: &str, bytes: Vec<u8>| {
            let path = root.join(name);
            std::fs::write(&path, bytes).unwrap();
            is_universal(&path)
        };

        assert!(
            check(
                "both",
                fat_header(FAT_MAGIC, &[CPU_TYPE_X86_64, CPU_TYPE_ARM64], None)
            )
            .is_ok(),
            "a release carries exactly these two"
        );
        assert!(
            check(
                "both64",
                fat_header(FAT_MAGIC_64, &[CPU_TYPE_ARM64, CPU_TYPE_X86_64], None)
            )
            .is_ok(),
            "the 64-bit header has wider entries and the same cputype"
        );

        // The failure this check exists for: signed, genuine, and dead on half
        // the fleet.
        let one = check("arm64only", fat_header(FAT_MAGIC, &[CPU_TYPE_ARM64], None));
        assert!(one.is_err());
        assert!(
            one.unwrap_err().to_string().contains("arm64"),
            "the message has to name what it actually found"
        );

        // A thin binary, in the little-endian form every Mac binary uses.
        let mut thin = Vec::new();
        thin.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        thin.extend_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        assert!(check("thin", thin).is_err(), "one architecture is not two");

        // **A header that is entirely well-formed, and still refused.** The
        // entries are all present, so nothing downstream would notice anything
        // wrong — the count alone is the defect. Reaching `read_exact` first
        // means sizing a buffer from an attacker's number: at the largest a
        // `u32` allows, that is an 85 GB allocation and the process is gone
        // before it reads a byte.
        let overfull: Vec<u32> = std::iter::repeat_n(CPU_TYPE_ARM64, MAX_FAT_ARCHS as usize + 1)
            .enumerate()
            .map(|(at, arch)| if at == 0 { CPU_TYPE_X86_64 } else { arch })
            .collect();
        assert!(
            check("overfull", fat_header(FAT_MAGIC, &overfull, None)).is_err(),
            "more architectures than a release could carry is refused on the count \
             alone, before the entries are sized or read"
        );
        assert!(
            check(
                "truncated",
                fat_header(FAT_MAGIC, &[CPU_TYPE_ARM64, CPU_TYPE_X86_64], Some(4))
            )
            .is_err(),
            "a header that names more entries than it carries is refused"
        );
        assert!(
            check("garbage", b"not a mach-o at all".to_vec()).is_err(),
            "and so is a file that is not a Mach-O"
        );
        assert!(
            check("short", vec![0xca, 0xfe]).is_err(),
            "or too short to be"
        );
    }

    /// The parser against a file nobody in this test wrote: the test binary
    /// itself, which `rustc` produced for this machine and is therefore thin.
    #[test]
    fn a_real_compiled_binary_is_read_as_the_single_architecture_it_is() {
        let me = std::env::current_exe().expect("a test binary has a path");
        let archs = architectures(&me).expect("this is a Mach-O");
        assert_eq!(archs.len(), 1, "a cargo build produces one architecture");
        assert!(
            archs[0] == CPU_TYPE_ARM64 || archs[0] == CPU_TYPE_X86_64,
            "and it is one of the two a release ships, not {}",
            arch_name(archs[0])
        );
        assert!(
            is_universal(&me).is_err(),
            "which is not both, so it is not a file this would install"
        );
    }

    /// The set is judged on the build as well as the number, so three binaries
    /// that agree on `0.4.0` and disagree on the commit are not "current".
    #[test]
    fn three_binaries_at_one_version_from_three_commits_are_not_a_release() {
        let set = |builds: [&str; 3]| {
            ["codeconnect", "ccd", "cc-hook"]
                .iter()
                .zip(builds)
                .map(|(binary, build)| {
                    (
                        binary.to_string(),
                        Some(format!("{binary} 0.4.0 ({build})")),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert!(set_is_current(&set(["abcdef123456"; 3]), "0.4.0"));
        assert!(
            !set_is_current(
                &set(["abcdef123456", "abcdef123456", "0123456789ab"]),
                "0.4.0"
            ),
            "one binary from another commit makes it a mixture, not a release"
        );
    }

    /// A staging directory that cannot be cleared must stop the update, not be
    /// stepped over — whatever survives in it is what gets exchanged into
    /// place.
    #[test]
    fn a_leftover_that_cannot_be_removed_stops_the_update() {
        let root = scratch("stubborn");
        let staged = staging_dir(&root);
        std::fs::create_dir_all(&staged).unwrap();
        write(&staged, "codeconnect", "stale");

        // A directory with no write permission cannot have its contents
        // unlinked, which is the real shape of this failure.
        let mut mode = std::fs::metadata(&staged).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o500);
        std::fs::set_permissions(&staged, mode).unwrap();

        let refused = clear_leftovers(&root);

        // Restored first, so the assertion cannot leave an unremovable
        // directory behind whichever way it goes.
        let mut mode = std::fs::metadata(&staged).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o700);
        std::fs::set_permissions(&staged, mode).unwrap();

        assert!(
            refused.is_err(),
            "a staging directory that survives must not be built on top of"
        );
        assert_eq!(
            read(&staged, "codeconnect"),
            "stale",
            "the premise: it really was still there"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
