//! Payload hashing for the stale-approval race, and file hashing for executable
//! identity.
//!
//! `payload_hash` is a hash of the *exact text the phone displayed*. The daemon
//! recomputes it from its own record and rejects a mismatch, so an approval
//! tapped against a stale card can never be applied to a different command.
//!
//! [`sha256_stream_head`] and [`sha256_file`] serve the other kind of identity:
//! *which bytes are about to run*. A pathname is not an executable — it is a name
//! that can be made to point at different bytes between the moment one is
//! inspected and the moment one is `execve`d — so `codeconnect::codex` pins the
//! resolved `codex` by its digest and re-derives that digest immediately before
//! each spawn (A7.1).
//!
//! Hashing alone does not finish that job, because a hash is taken through an
//! **open file** and an exec is performed on a **pathname**, and those are two
//! different things the instant somebody renames over the name.
//! [`refuse_unless_path_still_names`] is the second arm, and every whole-file
//! digest this module hands out for an executable identity is taken under it.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;

/// Raw digest, for callers that need the bytes rather than the hex text.
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_of(sha256_bytes(bytes))
}

/// The one hex encoder. Lowercase, fixed width, no separators — the single
/// spelling every digest in this codebase has, so two equal digests can never
/// compare unequal as text.
fn hex_of(digest: [u8; 32]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Hand-rolled hex keeps the dependency list at one crate.
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// How much is read per `read` call by the streaming hashers.
///
/// 64 KiB, measured against the file this exists for — the ~220 MB standalone
/// `codex` executable. At that size the syscall count is negligible beside the
/// hashing itself, and the buffer still lives comfortably on a worker thread's
/// stack (Rust's default is 2 MiB).
const STREAM_CHUNK: usize = 64 * 1024;

/// SHA-256 over everything `reader` yields, **together with a copy of the first
/// `head.len()` bytes it yielded** and how many of those there actually were.
///
/// The head is not a convenience. The caller this exists for has to decide *what
/// kind of file it has* — a native Mach-O executable or a `#!`-script wrapper —
/// from the same bytes it is pinning. Reading a magic number and then hashing is
/// two reads, and a file that is rewritten in place between them makes the verdict
/// describe bytes the digest does not cover. Handing the head back from inside the
/// hashing pass makes the verdict and the digest properties of one read, by
/// construction rather than by assumption.
///
/// Streaming, not `read_to_end`: producing 32 bytes must not require holding 220 MB
/// resident.
///
/// **The returned count is the contract, not the buffer.** Bytes of `head` past
/// that count are left exactly as the caller had them — this never zeroes a tail.
/// A caller that reuses a head buffer, or reads past the count, is reading its own
/// stale bytes and calling them file content; check the count first.
pub fn sha256_stream_head(
    reader: &mut impl Read,
    head: &mut [u8],
) -> std::io::Result<(String, usize)> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; STREAM_CHUNK];
    let mut head_filled = 0usize;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if head_filled < head.len() {
                    let take = n.min(head.len() - head_filled);
                    head[head_filled..head_filled + take].copy_from_slice(&buf[..take]);
                    head_filled += take;
                }
                hasher.update(&buf[..n]);
            }
            // A signal arriving mid-read is not a failure to read the file. Every
            // other error is, and is returned rather than silently truncating the
            // digest — a short hash of a long file would be a *different* identity
            // that still looks like a valid one.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok((hex_of(hasher.finalize().into()), head_filled))
}

/// SHA-256 over everything `reader` yields, for callers that need no head.
pub fn sha256_stream(reader: &mut impl Read) -> std::io::Result<String> {
    sha256_stream_head(reader, &mut []).map(|(digest, _)| digest)
}

/// Refuse unless `path` **still names the very file** `file` is open on.
///
/// # The hole this closes
///
/// A digest is taken through an open file; an `execve` is performed on a
/// *pathname*. Those are the same thing only for as long as nobody moves the name.
/// Every whole-file hash in this codebase exists to answer "which bytes will run",
/// and it answers it by reading a vnode the kernel pinned at `open` time — so an
/// atomic replacement (`rename(2)` of a new file over the name, which is how every
/// installer, `npm` overwrite and `standalone/current` flip lands) that arrives
/// **while the read is in flight** does not disturb the read at all. The open fd
/// keeps referring to the old vnode to its last byte, the digest comes out equal to
/// what was pinned, the check *passes* — and then `Command::new(path)` opens the
/// name afresh and runs the replacement. The digest was true; it was true about a
/// file that no longer answers to that name.
///
/// This is not a theoretical sliver, and it is not reasoned — it was staged against
/// the real 220 MB standalone `codex` 0.147 (219,997,536 bytes): open, stream-hash
/// to EOF, compare, `execve` the pathname, with an atomic `rename` landing 50 ms
/// into the hash. Three of three runs, the digest matched the pin **exactly** and
/// the replacement then ran (it exited 42 to prove it). A whole-file read of that
/// binary is about half a second warm — 0.478 / 0.459 / 0.460 s through this exact
/// chunked hasher built `--release`, 0.52 s through `shasum -a 256` — and ~8 s
/// unoptimised, and *all* of it used to be inside the window. That makes the
/// attacker's job "land a rename some time during a half-second read", which is less
/// a race than an appointment.
///
/// # What it does
///
/// `fstat` the open handle, `stat` the pathname, and refuse unless `(st_dev,
/// st_ino)` match. `stat`, not `lstat`: it must resolve symlinks exactly the way
/// `execve` will, or the two would disagree about which file the name reaches and
/// the comparison would be checking the wrong thing.
///
/// # The caller must still hold the fd open — this is load-bearing
///
/// The `file` argument is a live handle rather than a `Metadata` snapshot, and that
/// is the signature's job: borrowing the `File` makes it impossible to have dropped
/// it before the comparison, which is exactly the mistake that would quietly hollow
/// this check out. An open fd holds a reference on the vnode, and that reference is
/// what stops the inode number being recycled. Close it first and the original can
/// be unlinked, its inode freed, and a *new* file created that the filesystem hands
/// the very same `st_ino` — at which point `(dev, ino)` equality is an alias rather
/// than an identity, and this function would say "unchanged" about a swap.
///
/// The converse was measured on this filesystem, not assumed: with the fd held open,
/// unlinking the name and then creating 20,000 files to force reuse never once
/// handed the held inode number back out. So while the handle is open, equal
/// `(dev, ino)` means the same file.
///
/// # What it does NOT prove (measured on this platform, not reasoned)
///
/// It does not make the exec use the handle that was hashed. That is what `fexecve`
/// is for, and macOS does not have it — nor any of its substitutes:
///
/// * `fexecve` is not declared anywhere in the macOS SDK. Compiling a call to it
///   fails with "call to undeclared function 'fexecve'", and `grep -rl fexecve` over
///   `$(xcrun --show-sdk-path)/usr/include` matches no file at all.
/// * `posix_spawn` has no fd-based variant: `sys/spawn.h` defines no flag that takes
///   an executable by descriptor.
/// * `execve("/dev/fd/N", …)` fails with `EACCES`. macOS devfs materialises an fd
///   node with no execute permission for a read handle (`cr--r--r--`), and even for
///   a handle opened `O_EXEC` — whose `/dev/fd` node *does* report mode `0111` —
///   the exec is still refused with `EACCES`. `O_EXEC` is no way out regardless:
///   `read(2)` on such a descriptor returns `EBADF`, so the one handle that could be
///   exec'd is the one handle that cannot be hashed.
///
/// So the residual after this check is the interval between the `stat` here and the
/// kernel's own open inside `execve` — microseconds, and genuinely irreducible on
/// this platform. What is no longer in the window is the whole-file read: hundreds
/// of milliseconds instead of microseconds, and the only part of it an attacker could
/// realistically aim at.
///
/// It also does not detect a replacement that is **reverted** before the check, which
/// no verify-by-content scheme can see.
pub fn refuse_unless_path_still_names(
    path: &std::path::Path,
    file: &std::fs::File,
) -> std::io::Result<()> {
    let opened = file.metadata()?;
    let named = std::fs::metadata(path)?;
    if opened.dev() == named.dev() && opened.ino() == named.ino() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "the file was replaced while it was being read: {} named device {}/inode {} when it \
         was opened and names device {}/inode {} now, so what was just read describes bytes \
         this pathname no longer reaches",
        path.display(),
        opened.dev(),
        opened.ino(),
        named.dev(),
        named.ino(),
    )))
}

/// SHA-256 of the file at `path`, **opened now, and still that same file when the
/// last byte has been read**.
///
/// Opening by path is the point at the verification sites: the question they ask
/// is "are the bytes reachable through this name at this instant still the ones we
/// pinned?", and only a fresh open can answer it. The resolution site deliberately
/// does *not* use this — it hashes a handle it already holds (see
/// [`sha256_stream_head`]) — but it applies the same vnode check to that handle, so
/// there is one rule here and not two.
///
/// The `refuse_unless_path_still_names` call is what makes the returned digest a
/// statement about `path` rather than only about a vnode. It happens **before**
/// `file` is dropped, deliberately; see that function for why the live handle is
/// what gives the comparison its meaning, and for what remains open afterwards.
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let digest = sha256_stream(&mut file)?;
    refuse_unless_path_still_names(path, &file)?;
    Ok(digest)
}

/// An open file whose bytes are **held immutable** for the lifetime of this value.
///
/// # Why this exists — closing what [`refuse_unless_path_still_names`] cannot
///
/// The vnode check bounds the swap window to `stat`→`execve`, but it does two
/// things it fundamentally cannot: it cannot stop a **same-inode content
/// mutation** — an attacker overwriting bytes the sequential hasher has already
/// read, or writing through a second hard link — because `(dev, ino)` is unchanged
/// by a write; and it cannot make the `execve` use the very bytes that were hashed,
/// because a digest is taken through a handle and `execve` opens a pathname. The
/// clean answer to the second is to exec the hashed descriptor — and macOS has no
/// way to do it: `fexecve` is not even a symbol in libSystem (`dlsym` returns
/// null), and `execve("/dev/fd/N", …)` is `EACCES` for a read handle and for an
/// `O_EXEC` handle alike, while an `O_EXEC` handle cannot be `read` at all. Both
/// measured on this platform.
///
/// So this takes the other road available here: it **freezes the vnode** with
/// `fchflags(fd, UF_IMMUTABLE)` *before* hashing, and keeps it frozen across the
/// `execve`. While the flag is set, every way of mutating or replacing the file
/// **from a fresh start** is refused by the kernel — measured, all
/// `EPERM`/`EEXIST`/`EINVAL` on this platform: `open(O_WRONLY)` and `open(O_RDWR)`
/// (so no new `MAP_SHARED` write), `ftruncate`, a write through any other hard link
/// to the inode (the flag is per-inode), `rename`-over the name,
/// `renamex_np(RENAME_SWAP)`, `clonefile`-over, and `unlink`. An `execve` of the
/// frozen file still runs — including the real signed, hardened-runtime `codex`
/// 0.147, measured.
///
/// # What that is worth, stated exactly — and what it is NOT
///
/// **It closes the update race.** The vector every comment in this file was written
/// about — an installer, an `npm` overwrite, a `standalone/current` flip landing
/// mid-launch — replaces the file by `rename`, or writes it through a *fresh* open.
/// Both are refused outright while the freeze is held, so the bytes hashed here are
/// the bytes `execve` loads. That is the whole of the benign, and by far the most
/// likely, case, and it is now closed rather than merely narrow.
///
/// **It is NOT a boundary against a hostile process running as this uid**, and must
/// never be described as one. Three holes, each measured on this platform, not
/// reasoned:
///
/// * `UF_IMMUTABLE` is **owner-revocable**. A same-uid peer runs `chflags nouchg` on
///   the pathname (or any hard link), mutates, and may restore the flag. Holding the
///   read fd changes nothing — measured: the peer's `chflags` succeeded and the
///   following `open(O_WRONLY)` then succeeded.
/// * A writable descriptor or `MAP_SHARED` mapping opened **before** the freeze
///   survives it. Measured: a `pwrite` through a pre-existing `O_RDWR` fd wrote, its
///   `msync` succeeded, and the new bytes were then visible through the very handle
///   this guard had frozen and hashed. So a pre-positioned writer defeats the freeze
///   completely.
/// * The flag freezes one vnode, not the path to it. An ancestor directory can be
///   renamed aside and rebuilt around a different `bin/codex` — measured, it
///   succeeds. [`refuse_unless_path_still_names`] catches that at verify time (the
///   name stops reaching the frozen handle), so it is refused rather than run; but
///   the freeze itself does not prevent it.
///
/// None of that is a regression, and none of it is news to this codebase: a hostile
/// same-uid process is **already out of scope by construction** — see the module doc
/// of `codeconnect::codex_host`, which says plainly that such a process "can already
/// `ptrace`, signal, or replace the binaries this host execs, so no filesystem check
/// here would be a boundary against it". Nothing on macOS changes that: there is no
/// exec-by-descriptor, and a private `0700` staging copy is equally readable and
/// writable by that same uid. What this buys is the benign race, in full, plus a
/// materially harder hostile case — not a new security boundary.
///
/// # Hold duration, and the residual after the flag is cleared
///
/// The freeze is needed from before the hash until the child is past `execve`;
/// after that the caller drops this guard and the flag is cleared (its original
/// value restored). macOS still demand-pages a Mach-O's text from the vnode over
/// the process's life, and an in-place write to the same vnode *can* be seen by a
/// later fault (`execve` takes no snapshot). For the standalone `codex` that is
/// bounded by code signing rather than by this flag: it is a signed, hardened-runtime
/// binary whose CodeDirectory carries a hash per page, and the process was measured
/// running with `CS_VALID | CS_HARD | CS_KILL | CS_RUNTIME | CS_SIGNED` — `CS_HARD`
/// refuses an invalid page and `CS_KILL` kills a process that becomes invalid — so a
/// substituted text page produces a code-signing **kill**, not attacker-controlled
/// execution. That makes the post-clear residual denial-of-service for a signed,
/// hardened build. It is *not* a guarantee this module can make for an arbitrary
/// executable, and it is not checked here. Holding the flag for the whole (long-lived)
/// session would not repair it either — the flag is revocable by the same uid that
/// would be doing the writing — while blocking legitimate `codex` updates for the
/// session's length and widening the leak window, so that trade is not taken.
///
/// # When the freeze cannot be set
///
/// `fchflags` fails on a read-only volume, on a filesystem without flag support, and
/// for a file this uid may not flag. The guard records that it did not freeze and
/// falls back to hashing and vnode-checking exactly as [`sha256_file`] does — the
/// behaviour every site had before this existed, which still refuses the atomic
/// replacement that is the realistic vector.
///
/// **The fallback is not evidence that the file is unwritable.** An earlier version
/// of this note argued that a uid which cannot set the flag cannot write the file
/// either. That is wrong: flag-change rights, data-write rights and directory-entry
/// rights are separate layers on macOS. A root-owned but group-writable executable, a
/// non-owned file in a user-owned directory (rename-over succeeds), an ACL granting
/// `write-data` without attribute-change, and an ownership-disabled external volume
/// are all cases where `fchflags` fails and a write or replacement still lands. So a
/// freeze failure means the F2 content-mutation hole is simply **not closed** on that
/// path, and this type says so rather than implying otherwise. What stays true is the
/// direction of the trade: the fallback is exactly the prior guarantee, never below
/// it.
///
/// # The leak, stated exactly — it does NOT self-heal
///
/// If the process is `SIGKILL`ed or the machine loses power in the sub-second window
/// the flag is held, it is left set. It is **not** cleared by a later launch, and the
/// reason is the rule directly above: a guard that finds the file already immutable
/// leaves it alone, because a flag we did not set is indistinguishable from one the
/// user set deliberately, and silently clearing the latter would be worse than
/// leaving the former. So a leaked freeze persists until somebody clears it:
///
/// ```text
/// chflags nouchg /path/to/codex
/// ```
///
/// What that costs while it persists is bounded and visible: the binary still
/// **runs** (an `execve` of an immutable file is fine — measured), so no session
/// breaks. Only writing it fails, which surfaces as `Operation not permitted` the
/// next time an installer or `npm` tries to update `codex`. That is the whole of it:
/// a recoverable, self-announcing annoyance, traded for closing a
/// bytes-verified-are-not-bytes-executed hole on the path that decides which code
/// runs.
#[derive(Debug)]
pub struct FrozenExecutable {
    /// The handle the freeze is on and the hash was taken through. Held open so the
    /// `(dev, ino)` the vnode check compared cannot be recycled, and so the flag can
    /// be cleared through the same descriptor on drop.
    file: std::fs::File,
    /// `Some(original_flags)` when *this* guard set `UF_IMMUTABLE` and must restore
    /// that exact value on drop; `None` when it changed nothing — the file was
    /// already immutable, or the flag could not be set and the fallback was taken.
    restore: Option<u32>,
}

impl FrozenExecutable {
    /// Whether the bytes are frozen right now — `true` when this guard set the flag
    /// or found it already set, `false` in the fallback where the freeze could not be
    /// established.
    ///
    /// It reports a fact, not a verdict. `false` does **not** mean the file is safe
    /// (a freeze failure proves nothing about who may write it) and `true` does not
    /// mean it is inviolable (the flag is owner-revocable, and a writer that opened
    /// before the freeze is unaffected by it). See the type's docs for both.
    pub fn is_frozen(&self) -> bool {
        current_flags(&self.file).is_some_and(|f| f & libc::UF_IMMUTABLE != 0)
    }
}

impl Drop for FrozenExecutable {
    fn drop(&mut self) {
        // Only ever undo what we did: restore the exact flags this guard changed
        // from. A failure here cannot un-run an `execve` that already happened, and
        // the next launch's set-then-clear recovers a flag left set, so it is
        // best-effort by necessity rather than by neglect.
        if let Some(original) = self.restore {
            unsafe { libc::fchflags(self.file.as_raw_fd(), original) };
        }
    }
}

/// Read the current BSD `st_flags` of an open file, or `None` if it cannot be read.
fn current_flags(file: &std::fs::File) -> Option<u32> {
    // SAFETY: `fstat` writes a `struct stat` into a zeroed, owned buffer; the fd is
    // valid for the borrow of `file`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut st) } == 0 {
        Some(st.st_flags)
    } else {
        None
    }
}

/// Set `UF_IMMUTABLE` on the open file, returning the flags to restore.
///
/// `Some(original)` when this call set the bit and the caller's drop must put the
/// original flags back. `None` when nothing was changed — the file was already
/// immutable (leave it; it is not ours to clear), or the flag could not be set
/// (read-only volume, unsupported filesystem, not the owner), in which case the
/// caller falls back to hash + vnode verification. A `None` is never an error: the
/// type's docs explain why every non-freezable case is one the attack cannot reach.
fn try_freeze(file: &std::fs::File) -> Option<u32> {
    let original = current_flags(file)?;
    if original & libc::UF_IMMUTABLE != 0 {
        return None;
    }
    // SAFETY: valid fd for the borrow; `fchflags` only reads the flag word.
    if unsafe { libc::fchflags(file.as_raw_fd(), original | libc::UF_IMMUTABLE) } == 0 {
        Some(original)
    } else {
        None
    }
}

/// Open `path`, **freeze its bytes immutable**, hash it, and refuse unless the name
/// still reaches the frozen file — returning the digest together with a guard that
/// keeps the freeze until it is dropped.
///
/// This is the executable-identity counterpart to [`sha256_file`], and the one an
/// exec site uses: the caller holds the returned [`FrozenExecutable`] across its
/// `execve` and drops it once the child is past `execve`, so the bytes hashed here
/// are provably the bytes that ran. See [`FrozenExecutable`] for the whole argument,
/// the demand-paging residual, and what happens when the freeze cannot be set.
///
/// The freeze is taken **before** the hash so the read cannot be raced, and the
/// vnode check runs **after** it, so a replacement that landed between the `open`
/// and the freeze — the only remaining instant the name could move — is still
/// caught (the name no longer reaches the handle the freeze and hash are on).
pub fn freeze_and_hash(path: &std::path::Path) -> std::io::Result<(String, FrozenExecutable)> {
    let mut file = std::fs::File::open(path)?;
    let restore = try_freeze(&file);
    let digest = sha256_stream(&mut file)?;
    refuse_unless_path_still_names(path, &file)?;
    Ok((digest, FrozenExecutable { file, restore }))
}

/// Canonical text for an approval card: the tool name plus its input rendered
/// as sorted-key JSON. `serde_json::Value` maps are BTreeMap-backed only with
/// the `preserve_order` feature *off*, which is the default — so key order is
/// already deterministic. We still round-trip through `to_string` on the Value
/// rather than the raw stdin bytes, because whitespace in the hook's stdin is
/// not guaranteed stable across Claude Code versions.
pub fn approval_payload_text(tool_name: &str, tool_input: &serde_json::Value) -> String {
    format!("{tool_name}\n{tool_input}")
}

pub fn approval_payload_hash(tool_name: &str, tool_input: &serde_json::Value) -> String {
    sha256_hex(approval_payload_text(tool_name, tool_input).as_bytes())
}

/// Identity of one `send_text` mutation: what is typed, where, and whether it
/// is submitted.
///
/// Not the text alone. A `request_id` makes a retry idempotent, but only if the
/// daemon can also tell a *retry* from a *different* mutation reusing the id —
/// otherwise a captured frame could be replayed with new text under an id the
/// ledger already trusts. Binding the target and the submit flag as well means
/// the ledger's answer to "is this the same mutation?" cannot be forged by
/// changing any part of what would actually be typed.
///
/// The session is hashed exactly as the client named it (a uid or a tmux name),
/// because that is the only string both sides can agree on before the daemon
/// has resolved it.
///
/// Fields are length-prefixed rather than joined by a separator. A plain
/// `"{session}\n{submit}\n{text}"` is ambiguous the moment `text` contains a
/// newline — `("cc-1", "true\nx")` and `("cc-1\ntrue", "x")` produce identical
/// material — and an ambiguity in a hash that authorises typing is a way to
/// make one mutation answer for another.
pub fn send_text_hash(session_ref: &str, text: &str, submit: bool) -> String {
    let mut material = String::from("codeconnect.send_text.v1");
    for field in [session_ref, if submit { "submit" } else { "stage" }, text] {
        material.push('\n');
        material.push_str(&field.len().to_string());
        material.push(':');
        material.push_str(field);
    }
    sha256_hex(material.as_bytes())
}

/// **Identity of one phone answer to a Codex approval.**
///
/// The whole authorization surface of the decision, in the same length-prefixed,
/// domain-tagged shape as [`send_text_hash`] and for the same reason: the ledger
/// treats a retry under the same request id with *different* material as a conflict
/// rather than a replay, so anything that could make this answer a different answer
/// has to be inside the preimage. The card's own `payload_hash` carries the question
/// and the exact option table it was displayed with; `option_id` is what the phone
/// chose; `decision` is the body that will actually be written, which for an
/// amendment is a structure the option id alone does not determine; and `thread_id`
/// is which conversation the request belongs to.
///
/// The decision is hashed as its serialized JSON, because that is precisely what
/// goes on the wire — hashing a rendering of it would let two decisions that
/// serialize differently share an identity.
pub fn answer_hash(
    request_id: &str,
    payload_hash: &str,
    option_id: &str,
    thread_id: &str,
    decision: &serde_json::Value,
) -> String {
    let wire = decision.to_string();
    let mut material = String::from("codeconnect.answer.v1");
    for field in [request_id, payload_hash, option_id, thread_id, &wire] {
        material.push('\n');
        material.push_str(&field.len().to_string());
        material.push(':');
        material.push_str(field);
    }
    sha256_hex(material.as_bytes())
}

/// Identity of one `interrupt` mutation: which session's which turn is aborted.
///
/// The same length-prefixed, domain-tagged shape as [`send_text_hash`], for the
/// same reason: a retry with the same `request_id` but a different target turn
/// must be recognised as a *different* mutation and refused, not silently
/// treated as a replay that aborts the wrong turn. There is no submit flag —
/// aborting is not staged.
pub fn interrupt_hash(session_ref: &str, turn_id: &str) -> String {
    let mut material = String::from("codeconnect.interrupt.v1");
    for field in [session_ref, turn_id] {
        material.push('\n');
        material.push_str(&field.len().to_string());
        material.push(':');
        material.push_str(field);
    }
    sha256_hex(material.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_vector() {
        // NIST/RFC-6234 canonical vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn payload_hash_is_key_order_independent() {
        let a = json!({"command": "touch /tmp/x", "description": "d"});
        let b: serde_json::Value =
            serde_json::from_str(r#"{"description":"d","command":"touch /tmp/x"}"#).unwrap();
        assert_eq!(
            approval_payload_hash("Bash", &a),
            approval_payload_hash("Bash", &b)
        );
    }

    #[test]
    fn payload_hash_detects_command_change() {
        let a = json!({"command": "touch /tmp/x"});
        let b = json!({"command": "rm -rf /tmp/x"});
        assert_ne!(
            approval_payload_hash("Bash", &a),
            approval_payload_hash("Bash", &b)
        );
    }

    #[test]
    fn payload_hash_detects_tool_change() {
        let input = json!({"command": "ls"});
        assert_ne!(
            approval_payload_hash("Bash", &input),
            approval_payload_hash("Write", &input)
        );
    }

    /// The exact vector the iOS client pins too (SendTextIdentityTests): one
    /// literal on each side is what proves the two implementations are the
    /// same function rather than two functions that agree on easy inputs.
    #[test]
    fn send_text_hash_matches_the_cross_language_vector() {
        assert_eq!(
            send_text_hash("cc-1", "hi", true),
            "832d56d28203c01645209f9b61d192de468301d1c2bdb6090a607b15ab8026a9"
        );
    }

    #[test]
    fn send_text_hash_covers_every_part_of_the_mutation() {
        let base = send_text_hash("cc-1", "deploy to prod", true);
        assert_eq!(base, send_text_hash("cc-1", "deploy to prod", true));
        // Changing the text, the target or the submit flag is a *different*
        // mutation and must not be able to ride an already-trusted request id.
        assert_ne!(base, send_text_hash("cc-1", "rm -rf /", true));
        assert_ne!(base, send_text_hash("cc-2", "deploy to prod", true));
        assert_ne!(base, send_text_hash("cc-1", "deploy to prod", false));
    }

    #[test]
    fn interrupt_hash_binds_session_and_turn() {
        let base = interrupt_hash("cc-1", "turn-7");
        assert_eq!(base, interrupt_hash("cc-1", "turn-7"));
        // A different turn or session is a different mutation, never a replay.
        assert_ne!(base, interrupt_hash("cc-1", "turn-8"));
        assert_ne!(base, interrupt_hash("cc-2", "turn-7"));
        // Distinct domain tag: it can never equal a send_text hash.
        assert_ne!(base, send_text_hash("cc-1", "turn-7", true));
        // Separator-safe: a turn id containing a newline cannot impersonate a
        // different (session, turn).
        assert_ne!(
            interrupt_hash("cc-1", "a\nb"),
            interrupt_hash("cc-1\na", "b")
        );
    }

    // ------------------------------------------------- streaming / file hashing

    /// A deterministic byte stream several `STREAM_CHUNK`s long, so the chunked
    /// path is actually exercised at its boundaries.
    fn long_bytes(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut x: u32 = 0x9e37_79b9;
        for _ in 0..len {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push((x >> 24) as u8);
        }
        out
    }

    #[test]
    fn streaming_agrees_with_the_one_shot_hash_across_chunk_boundaries() {
        // The property that makes the streaming hasher usable at all: chunking
        // must not be visible in the digest. Sizes chosen to straddle the chunk
        // size in both directions and to land exactly on it.
        for len in [
            0,
            1,
            STREAM_CHUNK - 1,
            STREAM_CHUNK,
            STREAM_CHUNK + 1,
            3 * STREAM_CHUNK + 7,
        ] {
            let bytes = long_bytes(len);
            assert_eq!(
                sha256_stream(&mut bytes.as_slice()).expect("in-memory read cannot fail"),
                sha256_hex(&bytes),
                "streaming and one-shot disagree at {len} bytes"
            );
        }
    }

    #[test]
    fn the_known_vector_survives_the_streaming_path() {
        assert_eq!(
            sha256_stream(&mut &b"abc"[..]).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_stream(&mut &b""[..]).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_head_comes_from_the_hashed_bytes_and_reports_a_short_file_honestly() {
        // A file longer than the head: the head is the first four bytes of
        // exactly the stream that was hashed.
        let bytes = long_bytes(STREAM_CHUNK + 5);
        let mut head = [0u8; 4];
        let (digest, filled) = sha256_stream_head(&mut bytes.as_slice(), &mut head).unwrap();
        assert_eq!(filled, 4);
        assert_eq!(head, bytes[..4]);
        assert_eq!(digest, sha256_hex(&bytes));

        // A file SHORTER than the head must report how much it really got, not
        // leave the caller reading zeroed padding as if it were file content —
        // that is how a two-byte file would be mistaken for one whose magic
        // happens to end in zeroes.
        let short = [0xCFu8, 0xFA];
        let mut head = [0u8; 4];
        let (_, filled) = sha256_stream_head(&mut &short[..], &mut head).unwrap();
        assert_eq!(filled, 2);
        assert_eq!(head, [0xCF, 0xFA, 0x00, 0x00]);
    }

    #[test]
    fn hashing_a_file_by_path_matches_hashing_its_contents() {
        let bytes = long_bytes(200_000);
        let path = std::env::temp_dir().join(format!(
            "cc-hash-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, &bytes).expect("write the probe file");
        assert_eq!(
            sha256_file(&path).expect("hash it back"),
            sha256_hex(&bytes)
        );

        // Rewriting the file changes the digest — the whole property the
        // executable pin rests on.
        std::fs::write(&path, b"replaced").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(b"replaced"));

        // A path that is not there is an error, never a digest of nothing: an
        // absent file must not hash equal to an empty one.
        std::fs::remove_file(&path).unwrap();
        assert!(sha256_file(&path).is_err());
    }

    // ------------------------------------------------- the vnode check (A7.1)

    /// A private scratch directory, named so two tests (or two `cargo test`
    /// processes) cannot collide over one pathname — these tests rename files over
    /// each other and a shared name would make them each other's attacker.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-vnode-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        dir
    }

    /// The two halves of finding 1, separated so each is visible on its own, with no
    /// race to stage: **a digest taken through a handle keeps describing the file the
    /// handle was opened on, and a pathname does not.**
    ///
    /// This is the whole exploit in four lines. The digest below comes out equal to
    /// the *original* bytes even though the name has been atomically replaced — which
    /// is exactly why comparing it against a pin used to pass, and why the pathname
    /// that gets `execve`d afterwards is a different file. Only the vnode comparison
    /// can tell them apart, because only it looks at anything other than bytes.
    #[test]
    fn a_digest_read_through_a_handle_outlives_the_name_it_was_opened_by() {
        let dir = scratch("static");
        let (target, replacement) = (dir.join("codex"), dir.join("codex.new"));
        std::fs::write(&target, b"the bytes that were inspected").unwrap();
        std::fs::write(&replacement, b"the bytes that would run").unwrap();

        let mut file = std::fs::File::open(&target).unwrap();
        // Unmoved, the name and the handle agree, and the digest is attributable.
        assert!(refuse_unless_path_still_names(&target, &file).is_ok());

        std::fs::rename(&replacement, &target).unwrap();
        let digest = sha256_stream(&mut file).unwrap();
        assert_eq!(
            digest,
            sha256_hex(b"the bytes that were inspected"),
            "the open handle never sees the replacement — this is the trap"
        );
        assert_ne!(digest, sha256_hex(&std::fs::read(&target).unwrap()));

        let err = refuse_unless_path_still_names(&target, &file)
            .expect_err("a moved name must be refused");
        let text = err.to_string();
        assert!(
            text.contains("replaced while it was being read"),
            "says what happened: {text}"
        );
        assert!(
            text.contains("inode"),
            "names the identity that moved: {text}"
        );

        // And a name with nothing behind it is an error, never a pass: "I could not
        // look" and "it is the same file" must not share an answer.
        std::fs::remove_file(&target).unwrap();
        assert!(refuse_unless_path_still_names(&target, &file).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// How large the raced file is. Big enough that a swap landing a millisecond or
    /// two after the open is still hundreds of milliseconds short of EOF, so the race
    /// below is staged **by construction** rather than by luck.
    const RACE_BYTES: usize = 32 * 1024 * 1024;

    /// Block until some descriptor in *this* process is open on `ino`.
    ///
    /// The synchronisation for the race test, and it is an observation rather than a
    /// sleep: `/dev/fd` is this process's own descriptor table, so an entry whose
    /// `stat` reports the target's inode is proof that the hashing thread has already
    /// opened the file. (macOS reports a devfs `st_dev` for these nodes but the real
    /// `st_ino` — measured — so the inode is what is compared, with the size as a
    /// second discriminator.) Panicking on the deadline is deliberate: a race that was
    /// never staged must fail loudly, not quietly pass as if the guard had held.
    fn wait_until_open_in_this_process(ino: u64, size: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if let Ok(entries) = std::fs::read_dir("/dev/fd") {
                for entry in entries.flatten() {
                    if let Ok(meta) = std::fs::metadata(entry.path()) {
                        if meta.ino() == ino && meta.len() == size {
                            return;
                        }
                    }
                }
            }
            std::thread::yield_now();
        }
        panic!("the hashing thread never opened the target: the race was not staged");
    }

    /// **Finding 1, staged as the real thing.** An atomic `rename` lands over the
    /// pathname *after* the hash has demonstrably opened the file and long before it
    /// reaches EOF — the shape every installer, `npm` overwrite and
    /// `standalone/current` flip actually has.
    ///
    /// Without the vnode check this returns `Ok` with the digest of the original
    /// bytes: the pin matches, the caller proceeds, and the `execve` that follows
    /// opens the name afresh and runs the replacement. That is not a narrower window
    /// than the old comments claimed, it is the whole read — 0.52 s warm for the real
    /// 220 MB codex, ~8 s unoptimised.
    ///
    /// The swap is not timed with a sleep. It waits on an observable fact — a
    /// descriptor in this process standing open on the target's inode — so the
    /// ordering is established rather than hoped for.
    #[test]
    fn a_rename_landing_mid_hash_is_refused_rather_than_hashed_clean() {
        let dir = scratch("race");
        let (target, replacement) = (dir.join("codex"), dir.join("codex.new"));
        let original = long_bytes(RACE_BYTES);
        std::fs::write(&target, &original).unwrap();
        std::fs::write(&replacement, b"the attacker's bytes").unwrap();
        let (ino, size) = {
            let meta = std::fs::metadata(&target).unwrap();
            (meta.ino(), meta.len())
        };

        let hashed_path = target.clone();
        let hasher = std::thread::spawn(move || sha256_file(&hashed_path));

        wait_until_open_in_this_process(ino, size);
        std::fs::rename(&replacement, &target).expect("the atomic replacement");

        let outcome = hasher.join().expect("the hashing thread must not panic");
        let err = match outcome {
            // The digest of the ORIGINAL bytes is precisely the dangerous answer:
            // it matches the pin while the name reaches something else entirely.
            Ok(digest) => panic!(
                "the mid-hash swap was not caught; sha256_file returned {digest} \
                 (the original hashes {})",
                sha256_hex(&original)
            ),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("replaced while it was being read"),
            "the refusal must say the file moved under the read: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------- the freeze (A7.1, findings 1 & 2)

    /// **Findings 1 and 2, defeated at the kernel rather than merely detected.**
    /// While the guard lives the bytes are immutable, so the two attacks a bare
    /// `(dev, ino)` comparison cannot see — a same-inode content overwrite behind the
    /// reader (finding 2), and a `rename` over the name before `execve` (finding 1) —
    /// are refused `EPERM` by the OS. Dropping the guard restores the file.
    #[test]
    fn freezing_refuses_every_write_and_swap_until_the_guard_is_dropped() {
        let dir = scratch("frozen");
        let (target, replacement) = (dir.join("codex"), dir.join("codex.new"));
        let bytes = long_bytes(200_000);
        std::fs::write(&target, &bytes).unwrap();
        std::fs::write(&replacement, b"the attacker's bytes").unwrap();
        // A second hard link to the SAME inode, made before the freeze (linking an
        // immutable file is itself refused): the flag is per-inode, so a write
        // through this alternate name must be refused too.
        let other_link = dir.join("codex.alias");
        std::fs::hard_link(&target, &other_link).unwrap();

        let (digest, guard) = freeze_and_hash(&target).expect("freeze + hash");
        assert_eq!(
            digest,
            sha256_hex(&bytes),
            "the digest is of the frozen bytes"
        );
        assert!(
            guard.is_frozen(),
            "the file must be frozen while the guard lives"
        );

        // Finding 2: there is no writable handle to be had — not for the name, and
        // not for any other hard link to the inode.
        for name in [&target, &other_link] {
            let err = std::fs::OpenOptions::new()
                .write(true)
                .open(name)
                .expect_err("a frozen file must refuse a writable open");
            assert_eq!(
                err.raw_os_error(),
                Some(libc::EPERM),
                "a write to {} must be EPERM while frozen",
                name.display()
            );
        }

        // Finding 1: the swap cannot even land while frozen — it is not a narrow
        // race, it is refused.
        let err = std::fs::rename(&replacement, &target)
            .expect_err("a rename over a frozen name must be refused");
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));

        // The freeze is transient: dropping the guard makes the file writable again.
        drop(guard);
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .is_ok(),
            "the guard's drop must clear the freeze it set"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard restores the **exact** prior flags on drop — it clears only the bit
    /// it set, never a flag that was already there.
    #[test]
    fn drop_restores_exactly_the_prior_flags() {
        let dir = scratch("restore");
        let target = dir.join("codex");
        std::fs::write(&target, b"x").unwrap();

        // Put a benign, unrelated flag on first, so "restore" has something to get
        // wrong.
        let original = {
            let f = std::fs::File::open(&target).unwrap();
            let st = current_flags(&f).unwrap();
            assert_eq!(st & libc::UF_IMMUTABLE, 0);
            assert_eq!(
                unsafe { libc::fchflags(f.as_raw_fd(), st | libc::UF_NODUMP) },
                0
            );
            current_flags(&f).unwrap()
        };
        assert_eq!(original & libc::UF_NODUMP, libc::UF_NODUMP);

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        // While held, immutable is added ON TOP of the pre-existing flag.
        {
            let f = std::fs::File::open(&target).unwrap();
            let held = current_flags(&f).unwrap();
            assert_eq!(held & libc::UF_IMMUTABLE, libc::UF_IMMUTABLE);
            assert_eq!(held & libc::UF_NODUMP, libc::UF_NODUMP);
        }
        drop(guard);

        let f = std::fs::File::open(&target).unwrap();
        assert_eq!(
            current_flags(&f).unwrap(),
            original,
            "drop must restore the exact prior flags, clearing only what it set"
        );
        // Clean up the flag we planted so the scratch dir can be removed.
        let st = current_flags(&f).unwrap();
        unsafe { libc::fchflags(f.as_raw_fd(), st & !libc::UF_NODUMP) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A flag the guard did **not** set — one the user froze themselves — is left set
    /// on drop. We only ever undo our own change, so a deliberate freeze survives us.
    #[test]
    fn a_freeze_we_did_not_set_outlives_our_guard() {
        let dir = scratch("preheld");
        let target = dir.join("codex");
        let bytes = b"already the user's to freeze";
        std::fs::write(&target, bytes).unwrap();

        // Simulate a freeze the user set before we ever looked.
        {
            let f = std::fs::File::open(&target).unwrap();
            let st = current_flags(&f).unwrap();
            assert_eq!(
                unsafe { libc::fchflags(f.as_raw_fd(), st | libc::UF_IMMUTABLE) },
                0
            );
        }

        let (digest, guard) = freeze_and_hash(&target).unwrap();
        assert_eq!(digest, sha256_hex(bytes));
        assert!(guard.is_frozen());
        drop(guard);

        // We did not set it, so we did not clear it.
        let err = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .expect_err("a user-set freeze must outlive our guard");
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));

        // Clear it ourselves so the scratch dir can be torn down.
        let f = std::fs::File::open(&target).unwrap();
        let st = current_flags(&f).unwrap();
        unsafe { libc::fchflags(f.as_raw_fd(), st & !libc::UF_IMMUTABLE) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The limits of the freeze, pinned so the docs cannot drift into claiming
    /// more.** Both of these are why [`FrozenExecutable`] is documented as closing
    /// the *update race* and explicitly NOT as a boundary against a hostile process
    /// running as this uid.
    ///
    /// If macOS ever made `UF_IMMUTABLE` non-revocable by its owner, or made setting
    /// it revoke pre-existing writable descriptors, this test would fail — and that
    /// failure is the signal to go strengthen the claims above, not to delete it.
    #[test]
    fn the_freeze_is_owner_revocable_and_blind_to_a_pre_opened_writer() {
        let dir = scratch("limits");

        // (1) A same-uid peer can simply take the flag off again.
        let revocable = dir.join("codex");
        std::fs::write(&revocable, b"pinned bytes").unwrap();
        let (_digest, guard) = freeze_and_hash(&revocable).unwrap();
        assert!(guard.is_frozen());
        {
            // The "peer": same uid, working from the pathname, exactly as
            // `chflags nouchg` does.
            let peer = std::fs::File::open(&revocable).unwrap();
            let flags = current_flags(&peer).unwrap();
            assert_eq!(
                unsafe { libc::fchflags(peer.as_raw_fd(), flags & !libc::UF_IMMUTABLE) },
                0,
                "the owner can always clear UF_IMMUTABLE — this is the documented hole"
            );
        }
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&revocable)
                .is_ok(),
            "after a peer revokes the flag the file is writable again: the freeze is \
             not a boundary against a hostile same-uid process"
        );
        drop(guard);

        // (2) A writable handle opened BEFORE the freeze is unaffected by it, and its
        // writes are visible through the very handle that was frozen and hashed.
        let pre_opened = dir.join("codex2");
        std::fs::write(&pre_opened, b"AAAAoriginal").unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&pre_opened)
            .expect("the writer opens first");
        let (digest, guard) = freeze_and_hash(&pre_opened).unwrap();
        assert!(guard.is_frozen(), "we did freeze it");
        assert_eq!(digest, sha256_hex(b"AAAAoriginal"));
        use std::io::{Seek, Write};
        writer.rewind().unwrap();
        writer
            .write_all(b"ZZZZ")
            .expect("a pre-existing writable fd still writes through a frozen vnode");
        writer.flush().unwrap();
        assert_eq!(
            std::fs::read(&pre_opened).unwrap(),
            b"ZZZZoriginal",
            "the frozen file's contents changed under the freeze — a pre-positioned \
             writer defeats it, which is exactly what the type documents"
        );
        drop(guard);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The frozen digest is the digest of the bytes, and a missing file is an error
    /// rather than a digest of nothing — the same contract as [`sha256_file`], now
    /// with the freeze in front of it.
    #[test]
    fn freeze_and_hash_matches_contents_and_refuses_an_absent_path() {
        let dir = scratch("frozen-digest");
        let target = dir.join("codex");
        let bytes = long_bytes(150_000);
        std::fs::write(&target, &bytes).unwrap();
        let (digest, guard) = freeze_and_hash(&target).unwrap();
        assert_eq!(digest, sha256_hex(&bytes));
        drop(guard);

        std::fs::remove_file(&target).unwrap();
        assert!(freeze_and_hash(&target).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn send_text_hash_is_not_confusable_by_moving_the_separator() {
        // The fields are newline-joined, so a text that *contains* newlines
        // must not be able to impersonate a different (session, submit, text).
        assert_ne!(
            send_text_hash("cc-1", "true\nx", true),
            send_text_hash("cc-1\ntrue", "x", true)
        );
    }
}
