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
/// # The leak, stated exactly — and the three things that undo it
///
/// If the process is `SIGKILL`ed or the machine loses power in the sub-second window
/// the flag is held, it is left set. Three things can take it off again, in the order
/// they are likely to arrive:
///
///   * **The custodian clears it**, which is the ordinary way a leak ends and needs
///     nobody's attention: the record names the holder, the holder is provably dead,
///     no other record claims the vnode, and the whole scan-and-clear runs inside the
///     same lock a freezer takes.
///   * **The next launch adopts it** — which does not take the bit off, and is not
///     meant to. A guard that finds the bit already set records itself as a live
///     holder of the vnode under [`FreezeLock`], so nothing clears the flag while it
///     is using those bytes; the clear falls to a later custodian pass, once no live
///     holder names the vnode. See [`FreezeLock`] for why an adopter's release leaves
///     the bit alone, and what that costs.
///   * **The operator clears it**, which is what a `SIGKILL` during the launcher's
///     probe still needs, because no record exists that early:
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
    /// `Some` when this guard **owns** the freeze, and which kind of owner it is:
    /// [`FreezeOwn::Set`] — it turned `UF_IMMUTABLE` on, and owes that exact flag word
    /// back on release — or [`FreezeOwn::Adopted`], where it found the bit already on
    /// and became a recorded live holder of it, owing the file nothing. `None` only
    /// when there is nothing to own: the flag word could not be read, the flag could
    /// not be set, or the lock could not be taken and the fallback was used.
    ///
    /// The saved word never carries the bit, in any case, which is what makes a saved
    /// word that carries it provably not from this path.
    own: Option<FreezeOwn>,
}

/// **What a guard owns on the pinned file, and therefore what it owes on release.**
///
/// The two are not the same thing, and one field that conflated them is how an
/// adopter came to take the flag off a binary another launch was still running.
#[derive(Debug, Clone, Copy)]
enum FreezeOwn {
    /// **This guard turned the bit on.** It owes the file this exact flag word back:
    /// from its `Drop`, and from the armed signal release for the endings that run no
    /// `Drop`. The setter is the only holder that ever takes the bit off.
    Set(u32),
    /// **This guard found the bit on and became a recorded live holder of it.** Its
    /// release is leave-as-is, on every path — see [`FreezeLock`]. It carries the
    /// same saved word, so the record it produces is indistinguishable from a
    /// setter's, which is exactly what a janitor needs it to be.
    Adopted(u32),
}

impl FreezeOwn {
    /// The flag word as found with the immutable bit removed: evidence for the
    /// record, identical for both kinds of ownership.
    fn saved(self) -> u32 {
        match self {
            FreezeOwn::Set(saved) | FreezeOwn::Adopted(saved) => saved,
        }
    }

    /// Which kind of ownership this is, in the form a record can carry.
    fn kind(self) -> FreezeOwnership {
        match self {
            FreezeOwn::Set(_) => FreezeOwnership::Set,
            FreezeOwn::Adopted(_) => FreezeOwnership::Adopted,
        }
    }
}

/// **Which kind of owner wrote a freeze record** — the one fact that says whether
/// the bit on the file is this launch's to take off.
///
/// [`FreezeOwn`] has always known the difference and the RELEASE paths have always
/// respected it; the record did not carry it, so a janitor reading a record could
/// not. That erasure is not cosmetic: an adopter's record describes a bit somebody
/// else turned on — an operator's `chflags uchg` among them — and a custodian that
/// read it as a setter's took that stranger's flag off the moment the adopter died.
///
/// Carried in the record precisely because the custodian is a **different process**
/// from the one that froze: the guard is gone by the time the question is asked, and
/// the record is all that is left of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreezeOwnership {
    /// **This launch turned the bit on.** Its custodian may take it off again, under
    /// the ordinary warrants — the holder is provably dead, and no other record names
    /// the vnode with a holder that is not.
    Set,
    /// **This launch found the bit already on and adopted it**, so that a stale
    /// record's custodian could see a live holder and defer. It never owned the flag,
    /// so neither does its custodian: what such a record licenses is its own
    /// withdrawal and nothing else.
    ///
    /// The **default**, which is the fail-closed direction: a record that does not say
    /// it set the bit has not established that the bit is anybody's to remove, and the
    /// cost of not clearing is a legible `chflags nouchg` while the cost of clearing
    /// wrongly is a stranger's flag silently removed.
    #[default]
    Adopted,
}

/// What a launch must persist so a **later, unrelated process** can undo a freeze
/// its own guard never got to undo.
///
/// [`FrozenExecutable`] clears the flag in `Drop`, which covers every ending a
/// process can run code for. It does not cover `SIGKILL`, and that is not a
/// theoretical gap: a host killed under load left `UF_IMMUTABLE` set on the real
/// `codex` binary twice, and a frozen `codex` cannot be updated until somebody
/// notices and runs `chflags nouchg` by hand.
///
/// So the guard's three facts are written down where the janitor can read them:
/// the pathname, the `(dev, ino)` the freeze was taken on, and the exact flag word
/// to put back. The pair is what makes the clear safe to perform from a process
/// that was not there — it is a warrant for **one vnode**, not for a name. A name
/// can be made to point somewhere else between the freeze and the clear, and
/// `chflags` on whatever a stale string reaches now is a strictly worse bug than
/// the leak it was meant to fix.
///
/// It is written whenever the guard **owns** the flag: it set the bit, or it found
/// the bit already set and adopted it under [`FreezeLock`]. Adoption is what gives a
/// stranded bit a live holder again — a stale record's custodian can then see that
/// somebody is using this vnode and defer, instead of clearing a guard out from
/// under a running launch. It is written for no other case: a guard that could not
/// freeze at all has nothing to withdraw.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeldFreeze {
    /// The pathname the freeze was taken through, as the exec site spelled it.
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    /// The flag word as it was found — **evidence, not authority.** It says what
    /// the file looked like before the freeze so a reader can tell a record written
    /// by this path from one that was not, and [`clear_held_freeze`] never writes
    /// it: the clear reads the current word and removes one bit from it. Writing
    /// this word back was a way for a corrupt or hand-written record to stamp an
    /// arbitrary flag set onto whatever file it named.
    ///
    /// By construction it does not carry `UF_IMMUTABLE`: the word is the flags as
    /// found with that one bit removed, for a fresh freeze and for an adoption
    /// alike. A saved word that does carry it is therefore impossible, and is
    /// refused rather than acted on.
    ///
    /// It is also the **comparison** the clear makes: `now == original_flags |
    /// UF_IMMUTABLE` or the clear refuses, because a file whose flags have moved
    /// since the freeze is one this record has stopped being true about.
    pub original_flags: u32,
    /// **Which kind of owner wrote this**, and therefore whether the flag is this
    /// record's to take off at all. See [`FreezeOwnership`]: a `Set` record's
    /// custodian may clear, an `Adopted` record's custodian may only withdraw.
    ///
    /// Defaulted on the way in, and the default is [`FreezeOwnership::Adopted`] — a
    /// record that does not say it set the bit has not established that the bit is
    /// anybody's to remove.
    #[serde(default)]
    pub ownership: FreezeOwnership,
    /// **Who took the freeze**, so a janitor can prove the holder is dead before
    /// undoing its guard.
    ///
    /// The flag exists to stop the pinned bytes changing under a launch that is
    /// still using them, so clearing it while its holder lives is precisely the
    /// failure the freeze was put there to prevent — and "the session is gone" is
    /// not that proof: a host whose tmux session has been destroyed handles its
    /// `SIGHUP` asynchronously and is alive for as long as its own teardown takes.
    ///
    /// `None` is a record from a build that did not write one. It is not read as
    /// "no holder"; it is read as "the holder cannot be proven dead", which defers
    /// the clear rather than licensing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<FreezeHolder>,
}

/// The process that took a freeze, named the way every other identity in this
/// codebase is: a `(pid, birth)` pair, plus the boot it was read under so a pid
/// recycled across a reboot cannot answer for the process that is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FreezeHolder {
    pub identity: crate::proc_identity::ProcessIdentity,
    pub boot: crate::proc_identity::BootIdentity,
}

impl FreezeHolder {
    /// Whether this holder is **provably** gone — the only condition under which
    /// undoing its freeze is not revoking a live launch's guard.
    ///
    /// Fail-closed in the D7 sense: `false` means "not proven dead", which covers a
    /// holder that is alive, a holder that is stopped, and a kernel that could not
    /// be asked. Only two things prove death, and both are proofs rather than
    /// inferences:
    ///
    ///   * **A different boot.** Every pid the recorded boot issued died with it, so
    ///     a boot identity that no longer matches settles the question without
    ///     asking about the pid at all. An unreadable boot proves nothing and is not
    ///     read as a change.
    ///   * **[`crate::proc_identity::liveness`] says `Gone`** — the pid does not
    ///     exist, or it exists with a different birth time and is therefore a reuse.
    ///     `Unknown` is not absence; a `SIGSTOP`ped holder reads `Alive` and stays
    ///     alive, which is the case the session-is-destroyed reasoning got wrong.
    pub fn is_provably_gone(&self) -> bool {
        match crate::proc_identity::boot_identity() {
            Some(now) if now != self.boot => return true,
            Some(_) => {}
            None => return false,
        }
        matches!(
            crate::proc_identity::liveness(&self.identity),
            crate::proc_identity::Liveness::Gone
        )
    }
}

/// What [`clear_held_freeze`] did, so the caller can say it out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreezeClear {
    /// The recorded vnode was still there, still immutable, and its original flags
    /// are back.
    Cleared,
    /// Nothing was owed and nothing was touched — the flag was already clear, or
    /// the pathname no longer reaches the vnode the freeze was taken on. Carries
    /// the reason, because "I deliberately did not chflags this" is worth reading.
    NotOurs(String),
    /// The clear was owed and could not be performed. Carries the reason.
    Failed(String),
}

// ------------------------------------------- one lock, over one executable

/// How long either holder waits for the other before giving up.
///
/// The lock is held across an `fchflags` and one record write — tens of
/// milliseconds — so a wait this long is already evidence that something is wrong
/// rather than merely busy. It is bounded because neither holder may block
/// forever: a freezer that waited indefinitely would hang a launch behind a wedged
/// custodian, and a custodian that did would stop doing everything else it owes.
const FREEZE_LOCK_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the wait re-asks: short enough that an ordinary momentary contention
/// costs one sleep, long enough not to spin.
const FREEZE_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// **How long a freeze site needs [`FreezeLock`] held** — a choice with no safe
/// default, which is why it is a parameter rather than a policy inside the primitive.
///
/// The lock is the only thing that stops a custodian's scan-and-clear interleaving
/// with a freeze. What ENDS the need for it is not the freeze: it is the moment a
/// custodian scanning the records can see this holder. For a site that writes a
/// record that moment is the record, and holding the lock any longer would make every
/// other participant wait out a whole-file hash that has nothing to do with them. For
/// a site that writes NO record there is no such moment, and the lock is the only
/// thing standing in for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHold {
    /// **Release at the record.** The site has written a claim a custodian's scan can
    /// read, so the long hash that follows runs unlocked. Every recording site — the
    /// host's two verifies — is this one.
    UntilRecorded,
    /// **Hold until the guard is released.** The site records nothing, so nothing a
    /// custodian scans will ever name it, and the lock is the only thing that makes it
    /// visible: a custodian takes the same lock across its scan and its clear, so
    /// while this is held the clear cannot happen at all.
    ///
    /// The launcher's probe is the site that needs it: it freezes before a uid exists,
    /// then hashes 210 MB and runs five execs against the frozen bytes. With the lock
    /// dropped at the (empty) record, a peer's custodian found no claim naming the
    /// vnode and cleared the flag out from under those execs.
    ///
    /// It costs the other participants a bounded wait — [`FREEZE_LOCK_BUDGET`] — and
    /// a custodian that loses it DEFERS, which is a retry rather than a failure.
    UntilReleased,
}

/// Take `flock(LOCK_EX)` on an open descriptor, waiting at most
/// [`FREEZE_LOCK_BUDGET`].
///
/// `LOCK_NB` in a loop rather than a blocking `LOCK_EX`, because the blocking form
/// has no timeout and both callers have somewhere else to be.
fn lock_exclusive_bounded(file: &std::fs::File) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        // SAFETY: a valid fd for the borrow; `flock` takes an advisory lock and
        // touches nothing else about the file.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => {}
            Some(libc::EINTR) => continue,
            _ => return Err(format!("the freeze lock could not be taken ({err})")),
        }
        if started.elapsed() >= FREEZE_LOCK_BUDGET {
            return Err(format!(
                "the freeze lock was held by another process for longer than \
                 {FREEZE_LOCK_BUDGET:?}"
            ));
        }
        std::thread::sleep(FREEZE_LOCK_POLL);
    }
}

/// Give the advisory lock back. Closing the descriptor does this too, and the two
/// orderings matter: a guard that must give the FLAG back before another process
/// may look at it closes its descriptor last, so the unlock happens after the
/// `fchflags` rather than before it.
fn unlock(file: &std::fs::File) {
    // SAFETY: a valid fd for the borrow; releasing a lock this process holds.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

/// **The one lock that makes a freeze have an owner.**
///
/// # The three holes it closes, which no amount of record-checking could
///
/// The immutable bit is one bit on one vnode. It cannot say who set it, and two
/// launches freezing the same `codex` set the identical bit — so ownership has to
/// live somewhere else, and the only place it can live is a rule about *when* the
/// bit and the records that describe it may be looked at together. Without such a
/// rule three things happen, all of them measured shapes rather than theory:
///
///  * A launch B that arrives on a leaked bit A left behind used to record nothing
///    at all (`own` was `None`, so [`FrozenExecutable::held`] returned `None`),
///    hash and use those bytes, and be invisible to A's custodian — which then
///    cleared the bit out from under it.
///  * A custodian's scan of the other launches' records and its `chflags` were two
///    separate moments. B could publish its record after A's scan had passed it and
///    before A's clear landed, and the clear revoked a guard that by then existed.
///  * The scan itself read a directory it could not enumerate, and a record it
///    could not stat, as *absence* — the one direction that licenses a clear.
///
/// # What it is
///
/// `flock(LOCK_EX)` on the executable's own descriptor. Every freezer holds it
/// across **freeze + record** — the interval in which the bit exists and nothing
/// durable says so — and the custodian holds it across **scan + clear**, so those
/// two never interleave. `flock` conflicts are evaluated on the vnode, so two
/// processes that open the same pathname exclude each other even though each has
/// its own descriptor; and it is *advisory*, so nothing about an `execve`, an
/// installer or a `chflags` by hand is affected by it. It is a convention among the
/// three participants (host, launcher probe, custodian) and it is not a boundary
/// against anything else — the same scope as everything else in this module.
///
/// # Adoption: what it is, and what it deliberately is not
///
/// Under this lock, a launch that finds the bit **already set adopts it**: it
/// records itself as a holder of that vnode, in exactly the record a launch that set
/// the bit writes. A stale custodian's scan then sees a live holder and defers,
/// instead of clearing a bit a running launch is relying on. That visibility is the
/// whole of what adoption is for.
///
/// **An adopter never clears.** Not from its `Drop`, and not from the armed signal
/// release — which is why the adopting path returns before anything is armed at all.
/// Only the launch that turned the bit on gives it back.
///
/// The rule has to be asymmetric, because **adoption is not a refcount and cannot be
/// made into one here**: a release knows a descriptor and a flag word, not a uid or a
/// records directory, and the same path runs from a signal handler, where a file
/// write and a lock acquisition are not allowed. A symmetric rule — everyone who owns
/// it clears it — is what two concurrent sessions on one binary actually met: the
/// session that ended first took the pin off the one still running. Phase 2's
/// guarantee is that the pinned bytes cannot change for the whole life of a session's
/// app-server, and a peer's release must not be able to end it.
///
/// **What it costs, stated plainly:** a bit whose setter was `SIGKILL`ed, met by an
/// adopter, stays on the file until the last live holder is gone. The adopter's
/// release does not take it off, and the custodian's clear defers while any record
/// names the vnode with a holder that is not provably dead — so the clear happens on
/// a later pass, once the adopter's own claim has been withdrawn. That is the
/// fail-closed direction: immutable is the safe state (the binary still runs; only an
/// update is refused, legibly, and `chflags nouchg` is the operator's one-liner),
/// while the other direction is unpinning a session that is still executing those
/// bytes.
///
/// **What it buys back:** an immutable bit an operator set on this pathname by hand
/// is adopted and left alone, rather than adopted and cleared by the next launch to
/// end.
pub struct FreezeLock {
    /// The name it was taken through, for the messages only. The warrant is the
    /// descriptor and the `(dev, ino)` behind it, never this string.
    path: String,
    /// The locked handle. **This is the descriptor [`clear_held_freeze`] checks and
    /// writes through** — one open, so there is no second resolution of the name
    /// between locking it and acting on it.
    file: std::fs::File,
}

/// Why a [`FreezeLock`] could not be taken — and the two answers mean opposite
/// things to a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreezeLockFailure {
    /// The name reaches no file (`ENOENT`) or reaches through something that is not
    /// a directory (`ENOTDIR`). **Nothing is owed**: the vnode a record describes is
    /// not reachable, so there is no bit to clear and nothing to lock.
    Missing(String),
    /// Every other reason — a permission denied, a descriptor shortfall, an I/O
    /// error, or another participant holding the lock past the budget. **A question
    /// that could not be asked**, which is not an answer: the caller must come back,
    /// never treat it as nothing owed.
    Unavailable(String),
}

impl std::fmt::Display for FreezeLockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FreezeLockFailure::Missing(why) | FreezeLockFailure::Unavailable(why) => {
                f.write_str(why)
            }
        }
    }
}

impl FreezeLock {
    /// Open `path` and take the exclusive freeze lock on it.
    ///
    /// The open's errno is classified here rather than at the clear, because this is
    /// now the only open on that path: **only "there is nothing there" discharges the
    /// debt**, and every other errno is a question this process could not ask.
    /// Reading a transient `EACCES` as "nothing owed" withdrew the claim and turned a
    /// failure that a later pass would have succeeded at into a leak nobody would
    /// look at again.
    pub fn acquire(path: &std::path::Path) -> Result<FreezeLock, FreezeLockFailure> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) => {
                let why = format!("{} could not be opened ({err})", path.display());
                return Err(
                    if matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) {
                        FreezeLockFailure::Missing(format!(
                            "{why}, so the vnode the record describes is not reachable and \
                             nothing is touched"
                        ))
                    } else {
                        FreezeLockFailure::Unavailable(format!(
                            "{why}; the claim is kept, because a failure to ask is not an answer"
                        ))
                    },
                );
            }
        };
        if let Err(why) = lock_exclusive_bounded(&file) {
            return Err(FreezeLockFailure::Unavailable(format!(
                "{}: {why}",
                path.display()
            )));
        }
        Ok(FreezeLock {
            path: path.display().to_string(),
            file,
        })
    }
}

impl Drop for FreezeLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

/// Undo a freeze recorded by [`FrozenExecutable::held`], from any process, **under
/// the lock that gives the bit an owner**.
///
/// The `lock` argument is the whole synchronisation, and taking it as a parameter
/// rather than acquiring it here is deliberate: the caller's *scan* of the other
/// launches' records has to happen inside the same hold. A custodian that scanned,
/// released, and then called a function that locked for itself would leave exactly
/// the window this exists to remove — a launch publishing its claim after the scan
/// passed it and before the `chflags` landed. See [`FreezeLock`].
///
/// **The `(dev, ino)` comparison is the whole warrant, not a sanity check.** This
/// function exists to be called on a stored string long after the process that
/// stored it died, and `chflags` on a pathname is `chflags` on whatever that name
/// reaches *now*. A codex update that landed after the leak — exactly the thing the
/// operator does next, once they discover the binary cannot be written — replaces
/// the file, and clearing flags on the replacement would be this janitor mutating a
/// file it has no record of and no business touching. So the identity of the locked
/// handle is compared against the record, and a mismatch reported as
/// [`FreezeClear::NotOurs`] with nothing written.
///
/// The handle is the **lock's own**, not a fresh open. One resolution of the name,
/// used to lock, to compare and to write, so there is no instant between them in
/// which the name could come to mean something else.
///
/// An already-clear flag is the same answer for the same reason: the leak may well
/// have been dealt with by hand, and the record is evidence of what was done, not a
/// standing licence to write.
pub fn clear_held_freeze(held: &HeldFreeze, lock: &FreezeLock) -> FreezeClear {
    let file = &lock.file;
    let Ok(md) = file.metadata() else {
        return FreezeClear::Failed(format!(
            "{} could not be stat'ed, so its identity cannot be matched against the record",
            held.path
        ));
    };
    if md.dev() != held.dev || md.ino() != held.ino {
        return FreezeClear::NotOurs(format!(
            "{} names device {}/inode {} now, and the freeze was taken on device {}/inode {} \
             — refusing to change the flags of a file this record does not describe",
            lock.path,
            md.dev(),
            md.ino(),
            held.dev,
            held.ino,
        ));
    }
    let Some(now) = current_flags(file) else {
        return FreezeClear::Failed(format!("{}'s flags could not be read", held.path));
    };
    if now & libc::UF_IMMUTABLE == 0 {
        return FreezeClear::NotOurs(format!(
            "{} is not immutable, so the freeze is already gone",
            held.path
        ));
    }
    // **A saved word carrying the bit is impossible**, and an impossible record is
    // one this path did not write: [`FrozenExecutable::held`] saves the flag word
    // with the immutable bit removed, whether the guard set that bit or adopted one
    // it found. Refused rather than acted on — the record is the only thing
    // asserting ownership of this vnode, and one that cannot have come from the
    // freeze is not evidence of a freeze. Checked before the comparison below,
    // which such a word could otherwise satisfy (`saved | UF_IMMUTABLE == saved`).
    if held.original_flags & libc::UF_IMMUTABLE != 0 {
        return FreezeClear::Failed(format!(
            "the record for {} saves a flag word ({:#x}) that is impossible — the bit this \
             clear exists to remove is already in it, which no freeze this janitor undoes \
             could have written. Refusing to act on a record the freeze did not produce",
            held.path, held.original_flags
        ));
    }
    // **The file must look exactly like the record says the freeze left it**, which
    // is the saved word plus the one bit the freeze added. Anything else means
    // something changed these flags after the freeze was taken — an operator, an
    // installer, another tool — and this janitor's account of the file has stopped
    // being true of it. A clear performed anyway would be removing a bit whose
    // provenance is no longer the one the record describes, on a file somebody else
    // is evidently managing. Refused, with the difference named, so the operator
    // reading it can see what moved.
    if now != held.original_flags | libc::UF_IMMUTABLE {
        return FreezeClear::Failed(format!(
            "{}'s flags are {now:#x} and the record says the freeze left them {:#x} (the \
             saved word {:#x} plus the immutable bit), so something else has changed this \
             file's flags since. Refusing to clear a bit this record no longer describes",
            held.path,
            held.original_flags | libc::UF_IMMUTABLE,
            held.original_flags
        ));
    }
    // **One bit off the CURRENT word, never the saved word written back.** The saved
    // word is a claim about a moment that has passed; the file's flags now are the
    // only thing the write can be correct about. Restoring the record's word would
    // undo, in one `fchflags`, every flag anyone set on this file since the freeze —
    // and would let a corrupt or fabricated `original_flags` stamp an arbitrary flag
    // set onto the target (`uappnd` on a record naming an unrelated file, measured).
    //
    // With the comparison directly above in place the two spellings can no longer
    // produce different bytes — `now == saved | UF_IMMUTABLE` is precisely the
    // condition under which `now & !UF_IMMUTABLE` and `saved` are equal — so this
    // is no longer a discriminable behaviour, and no test can tell them apart. It
    // stays because it is the form that is correct on its own terms: if the
    // comparison is ever relaxed, this line does not become a way to write an
    // attacker's flag word.
    //
    // SAFETY: valid fd for the borrow; `fchflags` only writes the flag word, and it
    // is written through the handle whose identity was just compared.
    if unsafe { libc::fchflags(file.as_raw_fd(), now & !libc::UF_IMMUTABLE) } == 0 {
        FreezeClear::Cleared
    } else {
        FreezeClear::Failed(format!(
            "restoring {}'s flags failed: {}",
            held.path,
            std::io::Error::last_os_error()
        ))
    }
}

impl FrozenExecutable {
    /// Whether the bytes are frozen right now — `true` when this guard set the flag
    /// or adopted one it found set, `false` in the fallback where the freeze could
    /// not be established (or the lock that makes one ownable could not be taken).
    ///
    /// It reports a fact, not a verdict. `false` does **not** mean the file is safe
    /// (a freeze failure proves nothing about who may write it) and `true` does not
    /// mean it is inviolable (the flag is owner-revocable, and a writer that opened
    /// before the freeze is unaffected by it). See the type's docs for both.
    pub fn is_frozen(&self) -> bool {
        current_flags(&self.file).is_some_and(|f| f & libc::UF_IMMUTABLE != 0)
    }

    /// The record a janitor needs to undo this freeze after a `SIGKILL` — `Some`
    /// whenever **this guard owns the flag**, whether it set that flag or adopted one
    /// it found already set.
    ///
    /// Owning it covers two cases and the record **says which**: the guard set the
    /// bit, or it found the bit already set and **adopted** it under [`FreezeLock`].
    /// The saved word is the flag word with the immutable bit removed either way, and
    /// both records answer "a live holder claims this vnode" identically — which is
    /// what makes another launch's custodian defer to either of them.
    ///
    /// What the two do NOT answer identically is "may this flag be taken off", and a
    /// record that could not tell them apart was answering it wrongly. An adopter owes
    /// the file nothing (see [`FreezeOwn`]) — including through its own custodian,
    /// which inherits exactly the authority the guard had and no more. So
    /// [`HeldFreeze::ownership`] travels with the record: an adopter still owes the
    /// machine the claim, because a custodian that cannot see it clears the vnode it
    /// is using, but the claim now says what it licenses.
    ///
    /// `path` is the name the caller froze through, taken as an argument rather
    /// than remembered, because the guard holds a handle and a handle has no name.
    /// A path that is not valid UTF-8 yields `None`: a name that cannot survive the
    /// round trip through the record is a name the janitor must not reopen.
    ///
    /// See [`HeldFreeze`] for why the `(dev, ino)` travels with it.
    pub fn held(&self, path: &std::path::Path) -> Option<HeldFreeze> {
        let own = self.own?;
        let original_flags = own.saved();
        let md = self.file.metadata().ok()?;
        // The holder is read, not assumed: `current_identity` goes to the kernel for
        // this process's birth time the same way the janitor will go to the kernel
        // for it later, so the two answers are comparable byte for byte. A boot that
        // cannot be read leaves the holder absent — which defers the clear rather
        // than licensing it — because a `(pid, birth)` with no boot beside it cannot
        // be told apart from the same numbers across a reboot.
        let holder = match (
            crate::proc_identity::current_identity(),
            crate::proc_identity::boot_identity(),
        ) {
            (Some(identity), Some(boot)) => Some(FreezeHolder { identity, boot }),
            _ => None,
        };
        Some(HeldFreeze {
            path: path.to_str()?.to_string(),
            dev: md.dev(),
            ino: md.ino(),
            original_flags,
            ownership: own.kind(),
            holder,
        })
    }
}

impl Drop for FrozenExecutable {
    fn drop(&mut self) {
        // **Only the setter clears, and only what it set.** An adopter's release is
        // leave-as-is: the launch that turned the bit on may still be running on
        // these bytes, and adoption is not a refcount, so a guard that gave back a
        // flag it did not set would be unpinning somebody else's live executable.
        // It leaves the armed slot alone too — it never armed one, and a blanket
        // disarm here would take the SETTER's release out of it.
        let Some(FreezeOwn::Set(original)) = self.own else {
            return;
        };
        // A failure here cannot un-run an `execve` that already happened, and the
        // custodian's clear recovers a flag left set, so it is best-effort by
        // necessity rather than by neglect.
        //
        // SAFETY: a valid fd for the borrow; `fchflags` only writes the flag word.
        unsafe { libc::fchflags(self.file.as_raw_fd(), original) };
        // Disarm: the descriptor is about to close, and a handler that fired
        // afterwards would be calling `fchflags` on a number that now means whatever
        // the next `open` in this process made it mean. This guard is the one that
        // armed the slot, so it is the one with something to disarm.
        ARMED_FREEZE.store(NOTHING_ARMED, std::sync::atomic::Ordering::SeqCst);
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

/// Publish `(fd, flags)` to the slot a signal handler may act on.
fn arm_freeze_slot(fd: i32, flags: u32) {
    let packed = ((fd as u32 as u64) << 32) | flags as u64;
    ARMED_FREEZE.store(packed, std::sync::atomic::Ordering::SeqCst);
}

fn disarm_freeze_slot() {
    ARMED_FREEZE.store(NOTHING_ARMED, std::sync::atomic::Ordering::SeqCst);
}

/// **Arm the release, then take the freeze — in that order** — returning the flag
/// word this guard now owes the file.
///
/// # Why the arming comes first
///
/// The slot is what a `SIGINT` reads, and a signal that arrives between the
/// `fchflags` and the arming finds nothing armed and leaves the bit set. That is a
/// keystroke-wide window on the launcher's probe, which has no launch record behind
/// it and no janitor that could ever read one. Arming first inverts which side of
/// the pair can be interrupted, and the inversion costs nothing: what the handler
/// would do in the window before the flag goes on is `fchflags(fd, found_word)` on a
/// file whose flags are already that word — a write of the value it already has.
///
/// # Adoption, and why it arms nothing
///
/// `Some` in two cases, and they are different ownerships (see [`FreezeOwn`]). The
/// bit was clear and this call set it — [`FreezeOwn::Set`], which owes the flag word
/// back on release and arms the slot that gives it back when no `Drop` will run. Or
/// the bit was **already set** and this call adopts it: it becomes the live holder a
/// stale custodian's scan can see, and undertakes **nothing** about the flag. So the
/// adopting branch returns before the arming rather than arming a no-op — the slot is
/// the one release path that cannot reason about who owns what, and the emptiest way
/// to say "this guard clears nothing" is to put nothing in it.
///
/// Adoption is sound only under [`FreezeLock`], which is why the caller takes that
/// lock around this call and the record that follows it.
///
/// `None` means there is nothing to own: the flag word could not be read, or
/// `fchflags` was refused (a read-only volume, a filesystem without flag support, a
/// file this uid may not flag). A `None` is never an error — the caller falls back
/// to hashing and vnode-checking exactly as [`sha256_file`] does, which is the
/// behaviour every site had before this existed.
fn arm_then_freeze(file: &std::fs::File) -> Option<FreezeOwn> {
    let found = current_flags(file)?;
    // The flag word as found, minus the one bit the freeze is about: what a setter
    // owes the file back, and what either kind of owner writes in its record.
    let saved = found & !libc::UF_IMMUTABLE;
    if found & libc::UF_IMMUTABLE != 0 {
        // **Adopted, and nothing is armed.** Nothing was set, so nothing is owed: the
        // bit belongs to whoever put it there, and that launch may still be running
        // on these bytes. Returning ahead of the arming is how "an adopter never
        // clears" survives a `SIGINT` as well as a `Drop`.
        return Some(FreezeOwn::Adopted(saved));
    }
    arm_freeze_slot(file.as_raw_fd(), saved);
    // SAFETY: valid fd for the borrow; `fchflags` only writes the flag word.
    if unsafe { libc::fchflags(file.as_raw_fd(), found | libc::UF_IMMUTABLE) } == 0 {
        Some(FreezeOwn::Set(saved))
    } else {
        // Nothing was changed, so nothing is owed, and an armed slot that owes
        // nothing would have a handler write a flag word it has no business writing.
        disarm_freeze_slot();
        None
    }
}

/// Open `path`, **freeze its bytes immutable, write the freeze down, then hash it**,
/// and refuse unless the name still reaches the frozen file — returning the digest
/// together with a guard that keeps the freeze until it is dropped.
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
///
/// # `on_frozen` runs while the flag is on and the digest has not started
///
/// The digest is a whole-file read of a 210 MB executable — measured at 0.46 s in a
/// release build and about eight seconds unoptimised — and the flag is on for all of
/// it. A caller that recorded the freeze after the digest came back therefore left
/// most of a second (or most of ten) in which the bytes were immutable and nothing
/// durable said so: a `SIGKILL` there, which is exactly the load-induced ending this
/// whole mechanism exists for, leaves a frozen binary no janitor has a claim on and
/// no operator can explain. `on_frozen` runs with the flag already set and the digest
/// not yet started, so what the caller writes down is true from the instant it
/// becomes true.
///
/// There is no no-callback spelling of this function. There used to be, and it was
/// how a freeze site came to exist with nothing recording it; a signature that
/// cannot be called without a recorder is what makes "every freeze is written down"
/// a property of the type rather than of everybody's memory. A caller with genuinely
/// nothing to record — a test — passes `|_| Ok(())` and says so at the call site.
///
/// # A recording failure aborts, and that is not a preference
///
/// `on_frozen` returns a `Result`, and an `Err` ends this call: the guard is dropped
/// (giving the flag straight back if this call is the one that set it, while the lock
/// below is still held) and the error is returned. The record **is** the safety — it is the only thing that can tell a
/// janitor the flag is ours, that tells a peer's custodian to defer, and that
/// explains an unwritable binary days later — so a freeze nobody could write down is
/// not a freeze this function will hand out. Continuing unrecorded was trading a
/// legible refusal for exactly the invisible leak this whole mechanism exists to
/// stop.
///
/// # Under the lock, and what happens when it cannot be had
///
/// The freeze and the recording happen inside [`FreezeLock`] — an advisory
/// `flock(LOCK_EX)` on this very descriptor — so a custodian's scan-and-clear can
/// never interleave with them, and so a bit found already set can be safely adopted.
/// How long it is held after that is the caller's to say, and [`LockHold`] is where
/// the reasoning lives: a site that wrote a record is visible without it and lets it
/// go before the long hash; a site that wrote none is invisible without it and keeps
/// it until the guard is released.
///
/// If the lock cannot be taken within its budget, **no freeze is taken at all** and
/// this degrades to the documented fallback — hash plus vnode check, the behaviour
/// every site had before the freeze existed. That is the safe direction: a freeze
/// taken outside the lock would have no owner, which is the state all of this is
/// here to abolish. [`FrozenExecutable::is_frozen`] reports it, and a caller that
/// cares (the launcher's probe does) can refuse.
pub fn freeze_and_hash_recording<F>(
    path: &std::path::Path,
    hold: LockHold,
    on_frozen: F,
) -> std::io::Result<(String, FrozenExecutable)>
where
    F: FnOnce(&FrozenExecutable) -> Result<(), String>,
{
    let file = std::fs::File::open(path)?;
    let locked = lock_exclusive_bounded(&file).is_ok();
    let own = if locked { arm_then_freeze(&file) } else { None };
    let mut frozen = FrozenExecutable { file, own };
    if let Err(why) = on_frozen(&frozen) {
        // `frozen` drops on the way out of this `return`, which gives the flag back
        // and only THEN closes the descriptor — and closing it is what releases the
        // lock. So the bit a SETTER put on is gone before any other participant is
        // allowed to look at it, and no adopter can inherit a freeze this call is
        // abandoning. An adoption has nothing to give back and abandons nothing: it
        // never set the bit, and what it drops here is only its own claim to be one
        // of the vnode's live holders.
        return Err(std::io::Error::other(format!(
            "the freeze on {} could not be written down, so it is being given back rather \
             than held by a launch no janitor could account for: {why}",
            path.display()
        )));
    }
    // **The hold ends where the caller said, and the two answers are not a
    // preference.** A site that wrote a record is visible to a custodian's scan
    // without the lock, so it lets go before a whole-file hash nobody else cares
    // about. A site that wrote none is visible through NOTHING else, and the
    // custodian's scan-and-clear takes this same lock — so keeping it is the only way
    // that freeze exists as far as any other participant is concerned. Releasing it
    // there was a freeze with an owner for the length of a record write and no owner
    // for the second and a half that mattered.
    //
    // Nothing is unlocked explicitly on the `UntilReleased` path: the guard's `Drop`
    // gives the flag back and THEN closes the descriptor, and closing it is what
    // releases the lock — so the bit is already gone before anyone else is allowed to
    // look, which is the same ordering the recording path relies on.
    if locked && hold == LockHold::UntilRecorded {
        unlock(&frozen.file);
    }
    let digest = sha256_stream(&mut frozen.file)?;
    refuse_unless_path_still_names(path, &frozen.file)?;
    Ok((digest, frozen))
}

// ------------------------------------------------- the freeze a signal must undo

/// The one armed freeze a signal handler may undo, packed into a single word so a
/// handler reads a descriptor and its flags as one indivisible fact.
///
/// `(fd as u32) << 32 | flags`, with [`NOTHING_ARMED`] for "no freeze is armed".
/// One slot rather than a list because the site that needs this holds exactly one
/// freeze at a time, and a list is a heap allocation a signal handler must not make.
static ARMED_FREEZE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(NOTHING_ARMED);
const NOTHING_ARMED: u64 = u64::MAX;

/// Undo the armed freeze, if there is one, and disarm it. Idempotent.
///
/// **Async-signal-safe by construction**: one lock-free atomic swap and one
/// `fchflags`. No allocation, no locks, no formatting, nothing that could deadlock
/// against the interrupted code. Public so the guard's own drop and the handler
/// share one implementation, and so a test can call what the handler calls without
/// the `raise` that would take the test process with it.
pub fn release_armed_freeze() {
    let packed = ARMED_FREEZE.swap(NOTHING_ARMED, std::sync::atomic::Ordering::SeqCst);
    if packed == NOTHING_ARMED {
        return;
    }
    let fd = (packed >> 32) as i32;
    let flags = packed as u32;
    // SAFETY: the fd was valid when armed and the guard that armed it disarms in its
    // own `Drop`, so it is still open here; `fchflags` only writes the flag word.
    unsafe { libc::fchflags(fd, flags) };
}

/// The handler: give the flag back, then die of the signal that was sent.
///
/// Restoring the default disposition and re-raising rather than `_exit`ing keeps the
/// exit status honest — a shell that sees `codeconnect` killed by `SIGINT` is
/// reading the truth — and means this handler adds a cleanup to the ending rather
/// than inventing a different one.
extern "C" fn release_and_reraise(sig: libc::c_int) {
    release_armed_freeze();
    // SAFETY: `sigaction` with `SIG_DFL` and `raise` are both async-signal-safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(sig, &sa, std::ptr::null_mut());
        libc::raise(sig);
    }
}

/// Install the handler that clears an armed freeze on the signals that otherwise
/// end a process without running any `Drop`.
///
/// For a **synchronous** program that takes a freeze outside any durable record —
/// the launcher's probe is the one that exists — this stands in for the record: a
/// `Ctrl-C` during the probe is a keystroke away, `panic = "abort"` means even an
/// unwind would not help, and the flag it leaves is invisible until the next codex
/// update fails with `Operation not permitted`.
///
/// Not installed by [`freeze_and_hash_recording`] itself: a process that already runs its own
/// signal handling (the host does) must not have them replaced by a library call it
/// did not ask for. The site that needs it says so.
pub fn install_freeze_signal_release() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: a plain handler with an empty mask and no flags; the handler body
        // is async-signal-safe (see `release_armed_freeze`).
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = release_and_reraise as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
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

    /// [`freeze_and_hash_recording`] with nothing to record.
    ///
    /// The production signature has no no-callback spelling on purpose — that is how
    /// a freeze site came to exist with no record behind it — so a caller with
    /// genuinely nothing to write says so here, once, where "a test" is the whole
    /// reason.
    fn freeze_and_hash(path: &std::path::Path) -> std::io::Result<(String, FrozenExecutable)> {
        freeze_and_hash_recording(path, LockHold::UntilRecorded, |_| Ok(()))
    }

    /// The same, shaped like the launcher's probe: nothing recorded, so the lock is
    /// the only thing that can stand in for the record.
    fn freeze_and_hash_holding(
        path: &std::path::Path,
    ) -> std::io::Result<(String, FrozenExecutable)> {
        freeze_and_hash_recording(path, LockHold::UntilReleased, |_| Ok(()))
    }

    /// The lock a janitor holds across its scan and its clear, taken here so a test
    /// can call [`clear_held_freeze`] the way the custodian does.
    fn clear_under_lock(held: &HeldFreeze) -> FreezeClear {
        match FreezeLock::acquire(std::path::Path::new(&held.path)) {
            Ok(lock) => clear_held_freeze(held, &lock),
            // The open's errno is classified by the lock now, and the two failure
            // shapes are the two the clear used to return for itself.
            Err(FreezeLockFailure::Missing(why)) => FreezeClear::NotOurs(why),
            Err(FreezeLockFailure::Unavailable(why)) => FreezeClear::Failed(why),
        }
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

    /// **A flag the guard did not set is ADOPTED, and never given back — including
    /// one an operator set deliberately.** Adoption makes this guard a recorded live
    /// holder of the vnode, which is what stops a stale custodian clearing a bit a
    /// running launch is relying on. It does not make this guard the bit's owner.
    ///
    /// The rule this replaces had the adopter clear on release, so that a leak healed
    /// through the next launch. It paid for that with the case the pin is actually
    /// made of: two concurrent sessions on one binary, where the adopter ending first
    /// took the flag off while the launch that set it was still running on those
    /// bytes. Only the setter clears. Immutable is the safe state, so leaving a bit
    /// alone is the fail-closed direction and a leak is dealt with by a later pass.
    ///
    /// What that buys back, for nothing: `chflags uchg` set on this pathname by hand
    /// now survives a launch, which the previous rule spent.
    #[test]
    fn a_freeze_the_guard_did_not_set_is_adopted_and_never_cleared_on_release() {
        let dir = scratch("preheld");
        let target = dir.join("codex");
        let bytes = b"already frozen when we arrived";
        std::fs::write(&target, bytes).unwrap();

        // A freeze somebody else set before we ever looked — a leak from a killed
        // launch, or an operator's own `chflags uchg`. Nothing here can tell them
        // apart, and that is exactly why the rule had to pick one.
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
        assert!(
            guard.is_frozen(),
            "the bytes are frozen — that is what it found"
        );
        assert!(
            guard.held(&target).is_some(),
            "and this guard is the holder now, which is what makes it visible to a \
             peer's custodian instead of invisible to it"
        );
        drop(guard);

        let f = std::fs::File::open(&target).unwrap();
        assert_ne!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "the adopter leaves the bit exactly as it found it: it did not set it, and \
             whoever did may still be running on these bytes"
        );
        drop(f);

        thaw(&target);
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

    // ------------------------------------------- the janitor's clear (H1.1)

    /// Take the flag off `path` whatever it carries, so a scratch dir holding a
    /// deliberately-frozen file can still be torn down.
    fn thaw(path: &std::path::Path) {
        if let Ok(f) = std::fs::File::open(path) {
            if let Some(st) = current_flags(&f) {
                unsafe { libc::fchflags(f.as_raw_fd(), st & !libc::UF_IMMUTABLE) };
            }
        }
    }

    /// **A file whose flags have moved since the freeze is one this record has
    /// stopped being true about.** The record says what the freeze left on the file:
    /// the saved word plus the one bit. If the flags now are anything else, somebody
    /// has changed them — an operator, an installer, another tool — and the bit this
    /// janitor would remove is no longer the bit its record describes. Refused, with
    /// the difference named.
    ///
    /// This is also what a **corrupt** saved word hits, and that is the point: the
    /// earlier clear wrote the saved word verbatim, and `uappnd` in that field turned
    /// an unrelated target append-only. Such a word can no longer even reach the
    /// write.
    #[test]
    fn a_flag_word_that_does_not_match_the_record_is_refused_rather_than_cleared() {
        let dir = scratch("clear-mismatch");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        let mut held = guard.held(&target).expect("this guard set the flag");
        // The guard is forgotten rather than dropped: the janitor's whole job is to
        // clear a freeze whose own process never got to.
        std::mem::forget(guard);
        // The corruption: a flag word this record could never honestly carry.
        held.original_flags = libc::UF_APPEND;

        match clear_under_lock(&held) {
            FreezeClear::Failed(why) => {
                assert!(
                    why.contains("something else has changed this file's flags"),
                    "the refusal must say what stopped matching: {why}"
                );
            }
            other => panic!("a record that does not describe the file must be refused: {other:?}"),
        }
        let f = std::fs::File::open(&target).unwrap();
        let now = current_flags(&f).unwrap();
        assert_eq!(
            now & libc::UF_IMMUTABLE,
            libc::UF_IMMUTABLE,
            "a refused clear must leave the flag exactly as it found it"
        );
        assert_eq!(
            now & libc::UF_APPEND,
            0,
            "and must certainly not have written the corrupt saved word to the file"
        );

        thaw(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The clear removes one bit and leaves everything else exactly as found.** A
    /// flag the file already carried before the freeze is not the janitor's to undo,
    /// and it is still there afterwards.
    ///
    /// With the comparison the test above pins in place, "remove one bit from the
    /// current word" and "write the saved word back" can no longer produce different
    /// bytes — `now == saved | UF_IMMUTABLE` is exactly the condition that makes them
    /// equal — so this test cannot discriminate the two spellings, and does not claim
    /// to. It pins the outcome; the refusal above is what pins the safety.
    #[test]
    fn the_clear_removes_the_immutable_bit_and_preserves_the_flags_it_found() {
        let dir = scratch("clear-bit-only");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();
        // A pre-existing, unrelated flag, so "preserved the rest" has something to
        // be wrong about.
        {
            let f = std::fs::File::open(&target).unwrap();
            let st = current_flags(&f).unwrap();
            assert_eq!(
                unsafe { libc::fchflags(f.as_raw_fd(), st | libc::UF_NODUMP) },
                0
            );
        }

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        let held = guard.held(&target).expect("this guard set the flag");
        assert_eq!(
            held.original_flags & libc::UF_NODUMP,
            libc::UF_NODUMP,
            "the saved word is the flags as found, and they included nodump"
        );
        std::mem::forget(guard);

        assert_eq!(clear_under_lock(&held), FreezeClear::Cleared);

        let f = std::fs::File::open(&target).unwrap();
        let now = current_flags(&f).unwrap();
        assert_eq!(
            now & libc::UF_IMMUTABLE,
            0,
            "the immutable bit is what the clear owes and it must be gone"
        );
        assert_eq!(
            now & libc::UF_NODUMP,
            libc::UF_NODUMP,
            "a flag the freeze found already set must survive the clear"
        );

        unsafe { libc::fchflags(f.as_raw_fd(), now & !libc::UF_NODUMP) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A saved word carrying `UF_IMMUTABLE` is **impossible** by construction — the
    /// record is only ever written when this guard set the bit on a file that did not
    /// have it — so a record carrying one was not written by that path. It is refused
    /// rather than acted on, and the claim is kept so the refusal is visible.
    #[test]
    fn a_saved_word_that_carries_the_immutable_bit_is_refused_as_impossible() {
        let dir = scratch("clear-impossible");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        let mut held = guard.held(&target).unwrap();
        std::mem::forget(guard);
        held.original_flags |= libc::UF_IMMUTABLE;

        match clear_under_lock(&held) {
            FreezeClear::Failed(why) => assert!(
                why.contains("impossible"),
                "the refusal must say what is wrong with the record: {why}"
            ),
            other => panic!("an impossible saved word must be refused, got {other:?}"),
        }
        let f = std::fs::File::open(&target).unwrap();
        assert_eq!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            libc::UF_IMMUTABLE,
            "a refused clear must leave the flag exactly as it found it"
        );

        thaw(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Only a file that is not there is somebody else's business.** Every other
    /// reason an open can fail — a permission denied on the directory, a descriptor
    /// shortfall, an I/O error — is a question this janitor could not ask, not an
    /// answer that it owes nothing. Reading them as `NotOurs` withdrew the claim and
    /// made a transient failure a permanent leak.
    #[test]
    fn only_a_missing_path_is_not_ours_and_every_other_open_failure_keeps_the_claim() {
        let dir = scratch("clear-open-errors");

        // ENOENT: nothing at the name at all.
        let gone = HeldFreeze {
            path: dir.join("never-existed").to_str().unwrap().to_string(),
            dev: 1,
            ino: 1,
            original_flags: 0,
            ownership: FreezeOwnership::Set,
            holder: None,
        };
        assert!(
            matches!(clear_under_lock(&gone), FreezeClear::NotOurs(_)),
            "a name that reaches nothing owes nothing"
        );

        // ENOTDIR: a path whose parent is a regular file.
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let through_a_file = HeldFreeze {
            path: file.join("child").to_str().unwrap().to_string(),
            ..gone.clone()
        };
        assert!(
            matches!(clear_under_lock(&through_a_file), FreezeClear::NotOurs(_)),
            "a path that cannot name a file owes nothing"
        );

        // EACCES: the file is there, and this process may not open it.
        let closed = dir.join("closed");
        std::fs::create_dir(&closed).unwrap();
        let hidden = closed.join("codex");
        std::fs::write(&hidden, b"x").unwrap();
        std::fs::set_permissions(&closed, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        let unreadable = HeldFreeze {
            path: hidden.to_str().unwrap().to_string(),
            ..gone.clone()
        };
        let verdict = clear_under_lock(&unreadable);
        std::fs::set_permissions(&closed, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        assert!(
            matches!(verdict, FreezeClear::Failed(_)),
            "an open this process was refused is a question unanswered, not a debt \
             discharged: {verdict:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **An already-clear flag owes nothing, and saying so is not a formality.** The
    /// leak may well have been dealt with by hand — the operator ran `chflags nouchg`
    /// the moment codex refused to update — and the record is then evidence of what
    /// was done, not a standing licence to write. Without the early return the clear
    /// would `fchflags` a file it owes nothing to and report `Cleared`, which is a
    /// janitor claiming an act it did not perform.
    #[test]
    fn a_flag_that_is_already_clear_is_reported_as_owing_nothing() {
        let dir = scratch("clear-already-gone");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        let held = guard.held(&target).unwrap();
        // The operator got there first: the guard's own drop puts the flag back.
        drop(guard);

        match clear_under_lock(&held) {
            FreezeClear::NotOurs(why) => assert!(
                why.contains("not immutable"),
                "the reason must say the flag was already gone: {why}"
            ),
            other => {
                panic!("a clear flag owes nothing, and must not be reported as a clear: {other:?}")
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record names **who** took the freeze, because a janitor that cannot prove
    /// the holder is dead cannot know whether clearing revokes a live launch's guard.
    #[test]
    fn a_record_names_the_process_and_the_boot_that_took_the_freeze() {
        let dir = scratch("clear-holder");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        let held = guard.held(&target).expect("this guard set the flag");
        drop(guard);

        let holder = held.holder.expect("the record must name its holder");
        assert_eq!(
            holder.identity,
            crate::proc_identity::current_identity().unwrap(),
            "the holder is this process, read the same way every other identity is"
        );
        assert_eq!(
            Some(holder.boot),
            crate::proc_identity::boot_identity(),
            "and the boot it was taken under, so a reused pid is not mistaken for it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The record must land before the hash, not after it.** Hashing a 210 MB
    /// executable is most of a second in a release build and eight in a debug one,
    /// and for the whole of it the old order had the flag set with nothing written
    /// down — a `SIGKILL` there left an immutable binary no janitor had a claim on.
    /// The callback runs with the flag already set and the digest not yet taken.
    #[test]
    fn the_freeze_is_recorded_before_the_hash_is_taken() {
        let dir = scratch("record-first");
        let target = dir.join("codex");
        let bytes = long_bytes(400_000);
        std::fs::write(&target, &bytes).unwrap();

        // **The descriptor's own read offset is the witness, and it is the only one
        // that cannot be satisfied by a callback that merely runs.** The digest is
        // taken by streaming this same handle to EOF, so "nothing has been read yet"
        // is a fact about the file position: zero before the hash, the file's length
        // after it. A callback moved to after the digest still sees a frozen file and
        // still produces the same record — and would still pass an assertion about
        // either — but it cannot see an offset of zero.
        let mut recorded: Option<HeldFreeze> = None;
        let mut offset_when_recorded = -1i64;
        let (digest, guard) =
            freeze_and_hash_recording(&target, LockHold::UntilRecorded, |frozen| {
                assert!(
                    frozen.is_frozen(),
                    "the flag must already be on when the record is written"
                );
                // SAFETY: a valid fd for the borrow; `lseek` with `SEEK_CUR` and 0 only
                // reports the position and moves nothing.
                offset_when_recorded =
                    unsafe { libc::lseek(frozen.file.as_raw_fd(), 0, libc::SEEK_CUR) };
                recorded = frozen.held(&target);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            offset_when_recorded, 0,
            "the record must be written before a single byte has been hashed"
        );
        assert_eq!(
            unsafe { libc::lseek(guard.file.as_raw_fd(), 0, libc::SEEK_CUR) },
            bytes.len() as i64,
            "and the hash really did read the whole file afterwards, or the offset \
             above proves nothing"
        );
        assert_eq!(digest, sha256_hex(&bytes));
        let recorded = recorded.expect("the callback ran and the guard set the flag");
        assert_eq!(
            recorded,
            guard.held(&target).unwrap(),
            "what was written down before the hash describes the same freeze the \
             guard is still holding after it"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A `SIGINT` on a path with no launch record still has to give the flag back.**
    /// The launcher freezes for the length of its probes before any uid exists, so
    /// there is nothing durable for a janitor to read; `panic = "abort"` and the
    /// default disposition mean the guard's `Drop` never runs. The armed release is
    /// what stands in for the record there.
    ///
    /// Nothing arms it — that is the fix. Arming used to be a call the freeze site
    /// had to remember to make from inside the recording callback, which left the
    /// interval between `fchflags` and that call uncovered; the primitive now arms
    /// the slot itself, and *before* it sets the flag.
    #[test]
    fn every_freeze_is_armed_for_the_signal_handler_that_replaces_the_drop() {
        let dir = scratch("signal-release");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let (_digest, guard) = freeze_and_hash(&target).unwrap();
        assert!(guard.is_frozen());

        // What the handler does, called directly: the handler itself ends in
        // `raise`, which would take the test process with it. Nobody armed anything
        // by hand, so a slot that gives the flag back is the primitive's own doing.
        release_armed_freeze();

        let f = std::fs::File::open(&target).unwrap();
        assert_eq!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "the armed release must clear the bit the freeze set, with no site having \
             remembered to arm it"
        );
        // Idempotent: the guard's own drop, arriving afterwards, is a no-op on an
        // already-clear file and must not fail the tear-down.
        release_armed_freeze();
        drop(guard);
        thaw(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The arming happens AFTER the branch that adopts and BEFORE the flag goes on,
    /// and only a source read can see either.** A `SIGINT` landing between `fchflags`
    /// and the arming finds an empty slot and leaves the bit set — a keystroke-wide
    /// window on the one freeze site that has no record behind it. An arming placed
    /// before the adoption branch is the opposite failure: the adopter would undertake
    /// to clear a bit it did not set, from the one release path that cannot reason
    /// about who owns it. The statements are adjacent lines inside one function, so no
    /// test that runs the code can observe their order; what can be observed is that
    /// they are written in it.
    ///
    /// Paired with the two runtime tests above: the slot really is armed by the time a
    /// fresh freeze returns, and really is not for an adoption.
    #[test]
    fn the_freeze_arms_the_release_after_it_adopts_and_before_it_sets_the_flag() {
        let source = include_str!("hash.rs");
        let at = source
            .find("fn arm_then_freeze(file: &std::fs::File) -> Option<FreezeOwn> {")
            .expect("the freeze helper must exist");
        let rest = &source[at..];
        let body = &rest[..rest.find("\n}\n").expect("a closed function body")];

        // **One arming, on the setting path only**, and the count is load-bearing
        // rather than tidiness. An earlier version of this test asked only whether the
        // FIRST `arm_freeze_slot` preceded the `fchflags`, and a mutation that moved
        // the arming into both branches — after the flag on the one that sets it —
        // satisfied that and survived. An arming per branch is also how an adoption
        // comes to arm one at all. `disarm_freeze_slot(` ENDS with the same characters,
        // so a bare substring count reads the disarm as an arming — which it is the
        // opposite of. Only a match that starts a word is a call to the arming.
        let armings: Vec<usize> = body
            .match_indices("arm_freeze_slot(")
            .filter(|(at, _)| {
                body[..*at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
            })
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            armings.len(),
            1,
            "the slot must be armed once, on the path that sets the flag — an arming \
             per branch is an arming that can be placed after the flag on one of them, \
             or on the branch that must not arm at all"
        );
        let armed = armings[0];
        let branched = body
            .find("if found & libc::UF_IMMUTABLE != 0 {")
            .expect("the freeze must decide whether it is adopting");
        let adopted = body
            .find("return Some(FreezeOwn::Adopted(")
            .expect("the adopting branch must leave before anything is armed");
        let set = body
            .find("libc::fchflags(")
            .expect("the freeze must set the flag");
        assert!(
            branched < adopted && adopted < armed,
            "the adopting branch must return BEFORE the arming, or a signal arriving \
             during an adoption gives back a bit this launch never set"
        );
        assert!(
            armed < set,
            "and the release must be armed before the flag goes on, or a signal in \
             between finds nothing armed and leaves the bit set"
        );
    }

    /// **A bit somebody else left behind is adopted — recorded, and left on.**
    ///
    /// A launch arriving on a leaked `UF_IMMUTABLE` used to record nothing at all —
    /// `own` was `None`, so `held()` was `None` — which made it invisible to every
    /// custodian on the machine while it hashed and used those very bytes. It now
    /// takes ownership of the FACT: it is the holder in its own record, so a stale
    /// record's custodian can see a live claim on the vnode and defer.
    ///
    /// It does not take ownership of the BIT. What ends the leak is the record it
    /// writes and then withdraws — a later custodian pass clears the flag once no live
    /// holder names the vnode — and until then the bit stays on, which is the state
    /// that cannot hurt anybody.
    #[test]
    fn a_launch_that_finds_the_bit_already_set_adopts_it_and_leaves_it_set() {
        let dir = scratch("adopt");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        // The leak: a freeze whose guard never got to run its `Drop`.
        let (_digest, leaked) = freeze_and_hash(&target).unwrap();
        std::mem::forget(leaked);
        // Nothing is armed any more either — a `SIGKILL`ed process's slot dies with
        // it — so the bit is genuinely stranded.
        disarm_freeze_slot();
        let f = std::fs::File::open(&target).unwrap();
        assert_ne!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "the fixture must really be a leaked freeze"
        );
        drop(f);

        // The next launch arrives.
        let (_digest, adopter) = freeze_and_hash(&target).unwrap();
        assert!(
            adopter.is_frozen(),
            "the bytes are frozen — that is what it found"
        );
        let held = adopter
            .held(&target)
            .expect("a launch that adopts a stranded bit is its holder and must say so");
        assert_eq!(
            held.original_flags & libc::UF_IMMUTABLE,
            0,
            "the saved word never carries the bit, for an adoption as for a fresh freeze"
        );
        assert!(
            held.holder.is_some(),
            "and it names this process, so a peer's custodian can see a live claim"
        );

        drop(adopter);
        let f = std::fs::File::open(&target).unwrap();
        assert_ne!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "an adopter's release takes nothing off: the launch it inherited the bit \
             from may still be alive, and it is the RECORD, not the release, that ends \
             the leak"
        );
        drop(f);
        thaw(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Two live holders, and only the setter's release clears.** This is the
    /// concurrent-sessions case the Phase 2 pin is actually made of: one binary, two
    /// sessions, each holding the freeze for the whole life of its own app-server.
    ///
    /// Under a rule where an adopter clears on release, session B ending first takes
    /// the bit off while session A is still running on those bytes — the pin removed
    /// from under a live session, which is the precise thing it exists to prevent.
    /// Adoption is not a refcount and cannot be made into one on this path: a `Drop`
    /// knows a descriptor and a flag word, not a uid or a records directory, and the
    /// same path runs from a signal handler where a file write and a lock are not
    /// allowed. So the rule is asymmetric instead — the setter clears, the adopter
    /// never does — and the cost is a leaked bit that outlives its setter, which the
    /// custodian's later pass takes off.
    #[test]
    fn with_two_live_holders_only_the_setter_s_release_clears_the_bit() {
        let dir = scratch("two-holders");
        let target = dir.join("codex");
        std::fs::write(&target, b"one binary, two sessions").unwrap();

        // A sets the bit; B arrives while A is still holding it, and adopts.
        let (_digest, setter) = freeze_and_hash(&target).unwrap();
        assert!(setter.is_frozen(), "A took the freeze");
        let (_digest, adopter) = freeze_and_hash(&target).unwrap();
        assert!(
            adopter.held(&target).is_some(),
            "B is a recorded live holder, which is what makes a stale custodian defer \
             to it rather than clear the vnode it is using"
        );

        // B ends first — the ordering the old rule got wrong.
        drop(adopter);
        let f = std::fs::File::open(&target).unwrap();
        assert_ne!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "the adopter's release must leave the pin standing: A is still running on \
             these bytes and the whole guarantee is that they cannot change while it is"
        );
        drop(f);

        // A ends. It set the bit, so it is the one that gives it back.
        drop(setter);
        let f = std::fs::File::open(&target).unwrap();
        assert_eq!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "and the setter's release does clear it, so two ordinary overlapping \
             sessions leave nothing behind at all"
        );
        drop(f);
        thaw(&target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **An adoption arms no release, and does not disturb the one the setter armed.**
    ///
    /// `Drop` is not the only path that gives a flag back: the primitive arms a signal
    /// slot, so a `SIGINT` — which runs no `Drop` — still clears what this process
    /// set. An adopter that armed that slot would clear somebody else's bit from the
    /// path that has the least ability to reason about it, which is why the adoption
    /// returns before the arming rather than arming a no-op.
    ///
    /// Both halves are here, and the second is the one a plausible mutation reaches:
    /// an adopter must arm nothing of its own, AND must leave the setter's arming in
    /// the slot when it goes.
    #[test]
    fn an_adoption_arms_nothing_and_leaves_the_setters_armed_release_alone() {
        let dir = scratch("adopt-signal");

        // (1) A stranded bit, adopted. The handler's work is a no-op, because the
        // adopter undertook nothing.
        let stranded = dir.join("stranded");
        std::fs::write(&stranded, b"a bit whose setter was SIGKILLed").unwrap();
        let (_digest, leaked) = freeze_and_hash(&stranded).unwrap();
        std::mem::forget(leaked);
        // A killed process's armed slot dies with it.
        disarm_freeze_slot();

        let (_digest, adopter) = freeze_and_hash(&stranded).unwrap();
        assert!(adopter.is_frozen());
        release_armed_freeze();
        {
            let f = std::fs::File::open(&stranded).unwrap();
            assert_ne!(
                current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
                0,
                "an adopter's signal release must clear nothing: it armed nothing, \
                 because the bit is not its to give back"
            );
        }
        drop(adopter);
        {
            let f = std::fs::File::open(&stranded).unwrap();
            assert_ne!(
                current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
                0,
                "and its `Drop` clears nothing either — the two release paths agree"
            );
        }
        thaw(&stranded);

        // (2) A live setter, and an adopter that comes and goes. The slot must still
        // be the SETTER's afterwards, or a `SIGINT` arriving once the adopter has gone
        // finds an empty slot and leaves the setter's flag on the file.
        let shared = dir.join("shared");
        std::fs::write(&shared, b"one binary, two sessions").unwrap();
        let (_digest, setter) = freeze_and_hash(&shared).unwrap();
        let (_digest, adopter) = freeze_and_hash(&shared).unwrap();
        drop(adopter);
        release_armed_freeze();
        {
            let f = std::fs::File::open(&shared).unwrap();
            assert_eq!(
                current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
                0,
                "an adopter must not take the setter's release out of the slot — the \
                 signal that arrives after it has gone still has to give that flag back"
            );
        }
        drop(setter);
        thaw(&shared);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The record IS the safety, so a freeze nobody could write down is not handed
    /// out.** The call used to log a recording failure and continue, which produced
    /// exactly the state this whole mechanism exists to abolish: a flag set by a
    /// launch, on a file no janitor has a claim on, with nothing to explain it when
    /// codex refuses to update a week later. The freeze is given straight back and the
    /// launch aborts, legibly.
    #[test]
    fn a_freeze_that_cannot_be_recorded_is_given_back_and_the_call_fails() {
        let dir = scratch("record-fails");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let err = freeze_and_hash_recording(&target, LockHold::UntilRecorded, |frozen| {
            assert!(
                frozen.is_frozen(),
                "the flag is on when the record is attempted"
            );
            Err("the launch record could not be written".to_string())
        })
        .expect_err("a recording failure must end the call");
        assert!(
            err.to_string().contains("could not be written down"),
            "the refusal must say what was wrong: {err}"
        );

        let f = std::fs::File::open(&target).unwrap();
        assert_eq!(
            current_flags(&f).unwrap() & libc::UF_IMMUTABLE,
            0,
            "and the flag must be back off the file, not left for nobody"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The lock is exclusive, and that is the whole of the synchronisation.** A
    /// freezer holds it across freeze + record; a custodian holds it across scan +
    /// clear. If it did not actually exclude, those two could interleave and a clear
    /// could land on a bit whose record had not been published yet.
    ///
    /// In-process here, over two descriptors on one file, which is the property
    /// `flock` gives (conflicts are evaluated on the vnode, not the description). The
    /// cross-process half — the one that matters, since the two holders are always
    /// different processes — is
    /// `codeconnect/tests/exec_freeze_lock.rs`, which drives a real second process.
    #[test]
    fn the_freeze_lock_excludes_a_second_holder_and_is_released_on_drop() {
        let dir = scratch("lock-excludes");
        let target = dir.join("codex");
        std::fs::write(&target, b"pinned bytes").unwrap();

        let held = FreezeLock::acquire(&target).expect("an uncontended lock is taken");
        let second = std::fs::File::open(&target).unwrap();
        assert!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0,
            "a second exclusive hold on the same vnode must be refused while the \
             first is held"
        );
        drop(held);
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "and must succeed once the holder drops, or a crashed janitor would wedge \
             every later launch"
        );
        unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_UN) };

        // A name that reaches nothing is `Missing` — nothing owed — and every other
        // failure is `Unavailable`, which the caller must come back to rather than
        // read as an answer.
        assert!(matches!(
            FreezeLock::acquire(&dir.join("never-existed")),
            Err(FreezeLockFailure::Missing(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The FREEZER's half of the lock: it is held across the freeze and the record,
    /// and released before the hash.** The custodian's half is easy to see (it takes
    /// the lock explicitly); this one is inside the primitive, and if it were dropped
    /// nothing about a single-process run would look any different — a custodian could
    /// simply scan and clear in the middle of somebody's freeze, which is the whole
    /// hole.
    ///
    /// The recording callback is the witness. It runs inside the hold, so a second
    /// descriptor on the same file cannot take the lock while it is running; and the
    /// hold ends before the digest, which is a whole-file read of a 210 MB executable
    /// and concerns no other participant.
    #[test]
    fn the_freeze_holds_the_lock_across_the_record_and_lets_go_before_the_hash() {
        let dir = scratch("freezer-holds");
        let target = dir.join("codex");
        std::fs::write(&target, long_bytes(200_000)).unwrap();

        let probe = std::fs::File::open(&target).unwrap();
        let mut locked_during_record = None;
        let (_digest, guard) = freeze_and_hash_recording(&target, LockHold::UntilRecorded, |_| {
            // SAFETY: a valid fd for the borrow; a non-blocking advisory lock attempt.
            locked_during_record =
                Some(unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) });
            Ok(())
        })
        .unwrap();
        assert_eq!(
            locked_during_record,
            Some(-1),
            "while the freeze is being written down, nobody else may take the lock —              a custodian that could would scan and clear inside somebody's freeze"
        );
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "and the hold ends with the record, not with the hash — half a second of              whole-file read is nobody else's business"
        );
        unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The other half of the same rule: a freeze that records NOTHING keeps the
    /// lock, because the lock is the only thing standing in for the record.**
    ///
    /// What ends a freeze site's need for the lock is not the freeze — it is the
    /// moment a custodian scanning the launch records can SEE this holder. A
    /// recording site reaches that moment at its record. The launcher's probe never
    /// reaches it at all: it freezes before a uid exists, so there is nothing for any
    /// scan to find, and releasing at the empty record left the whole interval that
    /// follows — a 210 MB hash and five execs against the frozen bytes — as a freeze
    /// no custodian could see and any custodian could undo. A peer's stale record was
    /// the whole of what it took.
    ///
    /// The lock closes it without a record existing, because the custodian's clear
    /// takes this same lock across its scan and its `chflags`: while this is held the
    /// clear cannot happen, and a custodian that cannot take it defers and comes back.
    #[test]
    fn a_freeze_that_records_nothing_holds_the_lock_until_its_guard_is_released() {
        let dir = scratch("probe-holds");
        let target = dir.join("codex");
        std::fs::write(&target, long_bytes(200_000)).unwrap();

        let (_digest, guard) = freeze_and_hash_holding(&target).unwrap();
        assert!(guard.is_frozen(), "the fixture must really be frozen");

        // The probes' interval: the hash is done, the record was never written, and
        // this is where a peer's custodian used to arrive and find nothing.
        let probe = std::fs::File::open(&target).unwrap();
        // SAFETY: a valid fd for the borrow; a non-blocking advisory lock attempt.
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1,
            "the lock must still be held after the hash, or the probe's execs run on \
             bytes any custodian is free to unpin"
        );

        // And a custodian taking it the way a custodian actually takes it is refused —
        // `Unavailable`, which is a deferral and a retry, never a discharged debt.
        let started = std::time::Instant::now();
        match FreezeLock::acquire(&target) {
            Err(FreezeLockFailure::Unavailable(why)) => assert!(
                why.contains("held by another"),
                "the refusal must say what it was waiting for: {why}"
            ),
            other => panic!(
                "a custodian must not be able to take the lock while a probe holds it: \
                 {:?}",
                other.map(|_| "acquired")
            ),
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "and the refusal must be bounded: a custodian that waited on a live probe \
             would stop doing everything else it owes"
        );

        // Released with the guard, so the probe is a bounded hold and not a wedge.
        drop(guard);
        assert!(
            FreezeLock::acquire(&target).is_ok(),
            "the hold must end when the probe does, or one launch would stop every \
             later custodian from ever clearing anything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
