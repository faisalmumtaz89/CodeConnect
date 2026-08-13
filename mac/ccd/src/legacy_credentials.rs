//! Credentials earlier releases installed, taken back at startup and again on
//! every revocation.
//!
//! `ccd` grants a paired phone exactly one thing: a device token, held hashed
//! in its own database and taken away by `codeconnect revoke`. Earlier releases
//! also appended the phone's public key to `~/.ssh/authorized_keys`, and that
//! grant lives entirely outside the database: revoking the token cannot reach
//! it, and sshd honours it either way. Taking it back therefore means editing
//! that file, which is the whole of what this module does — and why it runs
//! from the two callers below rather than from the revocation alone.
//!
//! **Two callers, because one was not enough.** A sweep runs before the daemon
//! serves anything, so the upgrade itself does not hand a grant a window it
//! could be used in — as far as a best-effort sweep reaches, which is the whole
//! of what rule 6 is about and not a guarantee that nothing survives it. That
//! sweep establishes nothing about the rest of the daemon's life, though: an
//! `~/.ssh`
//! that could not be rewritten at boot and can be an hour later is never
//! retried, an `authorized_keys` restored from a backup or resynced by a
//! dotfiles manager puts the grant back under a daemon that has already swept,
//! and a Mac that runs `ccd` for months between restarts gives either of those
//! months to sit. So the sweep runs again on `codeconnect revoke` — the command
//! an operator reaches for meaning *this phone reaches nothing of mine any
//! more*, and the command that, until it swept, took the token and left the
//! shell. Running it there is what stops a revocation leaving a shell behind in
//! the cases anything can — which is not all of them, and the next paragraph is
//! the list of what it cannot do. A revocation is the whole truth about the
//! token; about the shell it is a best effort at a file this daemon does not
//! own.
//!
//! **Both callers sweep the whole file, not one device's entries.** This daemon
//! issues a device token and nothing else, on a branch that removes SSH
//! outright, so there is no device whose tagged entry it is willing to keep —
//! the id in a tag decides nothing about whether the entry should be there.
//! Sweeping per device would also promote that id from a shape this file
//! recognises to a claim about who wrote a line, which rule 1 is careful never
//! to make.
//!
//! **It is best-effort, and every way it removes nothing is a rule here rather
//! than an accident.** Without an absolute `$HOME` the file is never opened; a
//! file that cannot be read is never examined; a replacement that cannot be
//! written leaves every entry exactly where it stands; a file somebody else
//! rewrote while this sweep was working on it is left as they left it, by rule
//! 5; a tagged line that is not a whole pair is kept on purpose, by rule 1.
//! Each of the five is a warning saying only what that path established, and
//! none of them stops the daemon starting or a revocation succeeding. So a
//! sweep having run entitles nobody to believe a particular key is gone — it
//! may only have tried. What settles it is a look at the file itself:
//!
//! ```text
//! grep -n codeconnect: ~/.ssh/authorized_keys
//! ```
//!
//! Nothing printed means nothing there carries the tag; no such file means the
//! same. What it prints is read by shape, per rule 1.
//!
//! The rules, in priority order:
//!
//! 1. **Only the shape earlier releases wrote, and only whole ones.** That
//!    shape is two adjacent lines carrying the same `codeconnect:<device id>`
//!    tag: a marker comment, then a bare three-field `ssh-ed25519` key whose
//!    comment field is the tag. Both lines go together or neither does. A key
//!    line on its own — whatever its comment field says — stays, because
//!    nothing in the file identifies whose it is; a marker on its own stays
//!    too, because removing the label while leaving the key strips a working
//!    grant of the only line that says anything about it. Shape is the whole of
//!    what this file offers: who wrote a line is not recorded in it, and no
//!    match here is evidence of provenance. Key material is never examined.
//! 2. **Everything else is byte-for-byte untouched.** Line endings, blank
//!    lines, comments and ordering all survive, because the file is edited as
//!    raw text rather than reassembled from parsed lines.
//! 3. **Nothing is written when nothing matches.** The common case — every
//!    boot after the first, every revocation after that, and every Mac that
//!    never granted SSH access — does not open the file for writing at all.
//!    This is what makes the second caller affordable.
//! 4. **Never a partial file.** The replacement is written to a sibling
//!    temporary and `rename`d over the original, so an interrupted daemon
//!    cannot leave a truncated `authorized_keys` and lock the operator out of
//!    their own machine.
//! 5. **Never a file other than the one it read.** This daemon is not the only
//!    thing that writes `authorized_keys` — a dotfiles sync, an `ssh-copy-id`,
//!    an editor and a restore all do, and none of them is ours to coordinate
//!    with. So one path is resolved *before* the read, the bytes are read
//!    through that resolution, and the same file holding the same bytes is
//!    confirmed again immediately before the `rename`. Anything else — the link
//!    now points elsewhere, the name is a different inode, the content moved on
//!    — and the replacement is dropped rather than renamed over somebody's
//!    newer edit, because a sweep whose whole job is a few tagged lines may not
//!    pay for them with keys it never saw. The read is cheap and the window is
//!    somebody else's editor, so it is tried a couple of times before it gives
//!    up; giving up is a warning, by rule 6.
//! 6. **Never fatal, to either caller.** A file that cannot be read or
//!    rewritten is a warning naming it, saying only what this sweep established
//!    about it, and handing over the check above. A daemon that refused to
//!    start over this would leave the operator with no CodeConnect *and* the
//!    stale grant; a revocation that reported failure over it would tell an
//!    operator their phone still holds a device token it no longer holds, and
//!    invite them to retry a withdrawal that already happened.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The tag earlier releases wrote into both the marker comment and the key's
/// own comment field, followed by the device id.
const TAG_PREFIX: &str = "codeconnect:";

/// Take back what can be taken back: every whole `authorized_keys` entry
/// matching the shape earlier releases installed, or a warning naming what was
/// not taken back and why.
///
/// Reports how many lines went, which is `0` in six distinct situations: there
/// was nothing to do, `$HOME` could not be resolved, the file could not be read,
/// the replacement could not be written, the file was rewritten by somebody else
/// while this sweep held its snapshot, or what carries the tag is not a whole
/// pair and is therefore left alone. All six are told apart in the log, and none
/// of them is a reason for a caller to fail — not the daemon's startup, and not
/// a revocation. The five that are not "nothing to do" each name the file and
/// hand over `grep -n codeconnect:` on it, because what this function returns is
/// a count of lines removed and never a statement that the file is now clean.
///
/// Blocking. Async callers want [`purge_authorized_keys_off_runtime`].
pub fn purge_authorized_keys() -> usize {
    let Some(path) = authorized_keys_path() else {
        crate::log_warn!(
            "no absolute $HOME, so ~/.ssh/authorized_keys cannot be resolved and nothing was \
             swept; if an earlier release installed lines tagged `{TAG_PREFIX}…` there, they \
             still grant a phone a shell on this Mac. In the home of the account this daemon \
             runs as: `grep -n {TAG_PREFIX} ~/.ssh/authorized_keys`"
        );
        return 0;
    };
    match purge(&path) {
        Ok(sweep) => {
            if sweep.removed > 0 {
                // Loud on purpose. Withdrawing shell access is a fact an
                // operator must be able to find in a log later without having
                // gone looking for it at the time.
                crate::log_warn!(
                    "SSH ACCESS WITHDRAWN: removed {} line(s) tagged `{TAG_PREFIX}…` \
                     from {} — this daemon grants a device token and nothing else",
                    sweep.removed,
                    path.display()
                );
            }
            if sweep.left > 0 {
                // The one way this sweep removes nothing that it can actually
                // see into: lines that carry the tag but are not the adjacent
                // pair earlier releases wrote — a blank line between them, a
                // key whose marker somebody removed. Removing half of such a
                // pair would strand a working grant with nothing left in the
                // file to say anything about it, so the daemon leaves it whole
                // and says so instead. Silence here would be the one case where
                // a shell survives this sweep unannounced.
                crate::log_warn!(
                    "{} line(s) in {} carry `{TAG_PREFIX}…` and are not the adjacent \
                     marker-and-key pair earlier releases wrote, so they are left as they \
                     are. If any of them is an `ssh-ed25519` line, it still grants a phone a \
                     shell on this Mac; delete it and its `{TAG_PREFIX}` comment by hand. \
                     `grep -n {TAG_PREFIX} {}` lists them",
                    sweep.left,
                    path.display(),
                    path.display()
                );
            }
            sweep.removed
        }
        // Three failures, three messages, because the daemon knows different
        // things at each. Telling them apart is the whole point: one saw the
        // file's content, one never did, and one saw content that has since
        // been overtaken — and a single line covering them would have to
        // describe a live grant it confirmed as a maybe, or a file it never
        // opened as if it had read it.
        Err(Stopped::Unread(err)) => {
            crate::log_warn!(
                "could not read {}: {err:#} — this daemon removed nothing and has not seen \
                 what is in that file. If an earlier release installed lines tagged \
                 `{TAG_PREFIX}…` there, they still grant a phone a shell on this Mac. \
                 `grep -n {TAG_PREFIX} {}` settles it: a marker comment and the \
                 `ssh-ed25519` line directly under it carrying the same tag are one entry, \
                 and both lines go by hand",
                path.display(),
                path.display()
            );
            0
        }
        Err(Stopped::Unwritten { err, doomed }) => {
            crate::log_warn!(
                "could not rewrite {}: {err:#} — this sweep read {doomed} line(s) out of it \
                 that grant a phone a shell on this Mac, and removed none of them; delete \
                 them by hand. `grep -n {TAG_PREFIX} {}` lists what is in it now, along \
                 with anything else carrying the tag",
                path.display(),
                path.display()
            );
            0
        }
        // The third thing the daemon can know: it read the file, and by the
        // time its replacement was ready that read was out of date. What it
        // counted is a fact about bytes that have been overtaken, so the count
        // is reported as of that read and nothing is claimed about the file as
        // it now stands — the one thing this path is sure of is that it wrote
        // nothing, which is why somebody else's newer edit is still there.
        Err(Stopped::Changed { reason, doomed }) => {
            crate::log_warn!(
                "{} kept changing while it was being swept, over {ATTEMPTS} attempt(s): \
                 {reason} — this daemon removed nothing rather than put a replacement it had \
                 already computed over an edit it never saw. As of the last read, {doomed} \
                 line(s) tagged `{TAG_PREFIX}…` were in it; if they are still there they \
                 still grant a phone a shell on this Mac. Running `codeconnect revoke` again \
                 sweeps it afresh; `grep -n {TAG_PREFIX} {}` settles what is in it now",
                path.display(),
                path.display()
            );
            0
        }
    }
}

/// [`purge_authorized_keys`], moved off the runtime worker that asked for it.
///
/// The sweep opens a file, and when it removes something it also creates a
/// second one, `fsync`s it, renames it and `fsync`s the directory. Startup is
/// the one caller that can do that inline, because nothing is being served yet.
/// Every other caller is inside an async task — a revocation arrives on the IPC
/// socket — and a runtime worker parked on `~/.ssh` is a worker not carrying the
/// sessions, terminals and pushes it shares that thread with. Handing file work
/// to the blocking pool is what `db`, `logrotate` and `tailer` already do.
///
/// A sweep that panicked is reported and survived, for the same reason a sweep
/// that failed is: the caller is withdrawing access, and this file is the part
/// of that it does not own. Rule 6 is a property of this function too, not only
/// of the one it calls.
pub async fn purge_authorized_keys_off_runtime() -> usize {
    match tokio::task::spawn_blocking(purge_authorized_keys).await {
        Ok(removed) => removed,
        Err(err) => {
            crate::log_warn!(
                "the sweep of ~/.ssh/authorized_keys did not finish: {err} — it removed \
                 nothing this caller can account for, and if an earlier release installed \
                 lines tagged `{TAG_PREFIX}…` there they still grant a phone a shell on this \
                 Mac. In the home of the account this daemon runs as: \
                 `grep -n {TAG_PREFIX} ~/.ssh/authorized_keys`"
            );
            0
        }
    }
}

/// `None` when there is no absolute `$HOME` to resolve against. This file
/// decides who may log in, so "somewhere relative to the current directory" is
/// not an acceptable guess at where it lives — a daemon started without a home
/// leaves the file alone and says so.
fn authorized_keys_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".ssh").join("authorized_keys"))
}

/// What one sweep of `path` came to.
#[derive(Debug)]
struct Sweep {
    /// Lines removed. `0` leaves the file untouched.
    removed: usize,
    /// Lines carrying the tag that were left where they are, because they are
    /// not a whole pair. Reported separately because they are the case the
    /// daemon cannot act on and the operator therefore has to.
    left: usize,
}

/// What stopped a sweep — and, the reason this is an enum rather than one error
/// type, how much the daemon had established about the file when it stopped.
///
/// Every variant comes out of the same `Err`, and two of them used to share one
/// warning that told the operator the file "still contains lines tagged
/// `codeconnect:…`". That sentence is unsupportable after [`Stopped::Unread`],
/// where nothing in the file was ever seen — and softening it to cover that case
/// would report a grant [`Stopped::Unwritten`] did read and count as a maybe.
/// [`Stopped::Changed`] is a third position again: it read the file and then
/// watched that read go out of date, so it owes a count and a hedge at once. So
/// they are carried apart to the one place that speaks to an operator.
#[derive(Debug)]
enum Stopped {
    /// The file could not be read. Its content is unknown: not what it holds,
    /// not whether it holds anything carrying the tag at all.
    Unread(anyhow::Error),
    /// The file was read and the replacement could not be put in place, so the
    /// `doomed` lines that were going are — as of that read — still in it.
    Unwritten { err: anyhow::Error, doomed: usize },
    /// The file was read, and somebody else had replaced it, repointed the link
    /// to it or rewritten its content before the replacement could land — every
    /// time this sweep tried. Nothing was written, so whatever they wrote is
    /// still there, and the `doomed` count describes the last read rather than
    /// the file as it now stands.
    Changed { reason: String, doomed: usize },
}

/// How many times one sweep will read the file, work out the survivors and try
/// to land them before it gives the file up as somebody else's.
///
/// Retrying at all is the difference between an operator's editor save costing
/// them the sweep and costing them nothing: each attempt starts from a fresh
/// resolve and a fresh read, so a retry after a competing write sweeps what that
/// write left — including, when a symlink was repointed, the file it now points
/// at. Three is where the two failure shapes meet. A single competing writer is
/// beaten by the first retry; something writing the file continuously beats any
/// bounded number, and against that the honest move is to stop and say so rather
/// than spin on a blocking-pool thread.
///
/// There is deliberately no pause between attempts. A retry costs one read of a
/// small file, and sleeping cannot make another process's write finish any
/// sooner — it would only park a thread the daemon needs and make the tests here
/// depend on a clock.
const ATTEMPTS: usize = 3;

/// One attempt is the floor, and what it protects is the constant's meaning
/// rather than the sweep. `purge` retries `1..ATTEMPTS` times and then reads the
/// file once more unconditionally, so a zero would still sweep — `1..0` is an
/// empty range, and the trailing read is outside the loop — it would just make a
/// constant named for a number of attempts describe one that never happens.
const _: () = assert!(ATTEMPTS >= 1);

/// What one attempt at a sweep came to.
#[derive(Debug)]
enum Attempt {
    /// The file was read and either wanted nothing done to it or has been
    /// replaced.
    Settled(Sweep),
    /// The bytes the survivors were computed from were out of date by the time
    /// the replacement was ready, so nothing was written. Worth another go.
    Stale { reason: String, doomed: usize },
}

fn purge(path: &Path) -> Result<Sweep, Stopped> {
    // Every attempt but the last, whose stale result is the one an operator
    // hears about. Split this way rather than tracking the last reason through
    // the loop, so there is no "ran out of attempts without a reason" case for
    // a reader to wonder about — there isn't one.
    for _ in 1..ATTEMPTS {
        if let Attempt::Settled(sweep) = sweep_once(path)? {
            return Ok(sweep);
        }
    }
    match sweep_once(path)? {
        Attempt::Settled(sweep) => Ok(sweep),
        Attempt::Stale { reason, doomed } => Err(Stopped::Changed { reason, doomed }),
    }
}

/// One resolve, one read, one replacement — all of them about the same file.
///
/// The resolution happening *here*, before the read, rather than inside the
/// write is the whole of rule 5. Resolving at write time reads one file and
/// renames over whatever the name points at by then, which loses data twice
/// over: a link repointed in between takes a replacement derived from the old
/// target and drops it on the new one, wiping keys this sweep never read while
/// leaving the grant it meant to remove exactly where it was.
fn sweep_once(path: &Path) -> Result<Attempt, Stopped> {
    let Some(snapshot) = read(path).map_err(Stopped::Unread)? else {
        return Ok(Attempt::Settled(Sweep {
            removed: 0,
            left: 0,
        }));
    };
    after_read(path);

    // `split_inclusive` keeps each line's terminator with it, so concatenating
    // the survivors reproduces the original bytes exactly — including a final
    // line with no newline, which reassembling from `lines()` would silently
    // grow one.
    let lines: Vec<&str> = snapshot.text.split_inclusive('\n').collect();
    let doomed = doomed_lines(&lines);
    // Counted before anything is written, over the file as it arrived: a
    // tagged line this sweep is not taking is one somebody has to look at.
    let left = lines
        .iter()
        .zip(&doomed)
        .filter(|(line, gone)| !**gone && (marker_tag(line).is_some() || key_tag(line).is_some()))
        .count();
    let removed = doomed.iter().filter(|gone| **gone).count();
    if removed == 0 {
        return Ok(Attempt::Settled(Sweep { removed, left }));
    }

    let kept: String = lines
        .iter()
        .zip(&doomed)
        .filter(|(_, gone)| !**gone)
        .map(|(line, _)| *line)
        .collect();
    // Written and `fsync`ed before the file is re-checked rather than after, so
    // the slow part of a replacement happens inside what the check covers. What
    // is left between [`moved_on`] and the `rename` is a couple of syscalls.
    let staged = stage(&snapshot.target, &kept).map_err(|err| Stopped::Unwritten {
        err,
        doomed: removed,
    })?;
    if let Some(reason) = moved_on(path, &snapshot) {
        // `staged` is dropped here, and takes its temporary with it.
        return Ok(Attempt::Stale {
            reason,
            doomed: removed,
        });
    }
    staged.commit().map_err(|err| Stopped::Unwritten {
        err,
        doomed: removed,
    })?;
    Ok(Attempt::Settled(Sweep { removed, left }))
}

/// Which lines are an installed entry's, by the shape earlier releases wrote:
/// a marker comment, then — directly under it, tagged with the *same* device
/// id — the key line. Both go, or neither does.
///
/// **A marker is never removed on its own, even though a comment grants
/// nothing.** The marker is the only line in the file that says anything at all
/// about the key beneath it. Take it away and leave the key — which is what one
/// blank line between the two, from an `ssh-copy-id` or a hand edit, used to
/// produce — and what remains is a working grant that nothing in the file
/// identifies, to a phone this daemon does not manage, revoke or list.
/// Withdrawing shell access is the whole purpose here, and a rule that can
/// strand the key while deleting its label works against it.
///
/// A key line reached any other way is still kept, so a key somebody put there
/// themselves ending in something tag-shaped cannot be deleted. The cost of the
/// pairing rule is a stale comment left behind when the key beneath it has
/// already gone by hand; a comment grants nothing, which is exactly why leaving
/// it is the cheap side of this trade.
fn doomed_lines(lines: &[&str]) -> Vec<bool> {
    let mut doomed = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if let Some(device_id) = marker_tag(lines[index]) {
            if index + 1 < lines.len() && key_tag(lines[index + 1]) == Some(device_id) {
                doomed[index] = true;
                doomed[index + 1] = true;
                index += 1;
            }
        }
        index += 1;
    }
    doomed
}

/// One file, the bytes it held, and enough to prove later that both are still
/// the same.
///
/// The identity is `(dev, ino)` off an `fstat` of the very handle the bytes came
/// out of, so it describes the file that was read rather than whatever the name
/// resolves to afterwards. The content check is the bytes themselves rather than
/// a size, a timestamp or a digest of them: `authorized_keys` is a few kilobytes
/// and they are already in hand, so comparing them outright is both cheaper to
/// reason about and strictly stronger than any (size, mtime, hash) triple —
/// there is no timestamp granularity to argue about and no collision to
/// dismiss.
struct Snapshot {
    /// The path with every symlink resolved, which is what gets renamed over.
    /// Resolved once, before the read, and carried from here on.
    target: PathBuf,
    text: String,
    dev: u64,
    ino: u64,
}

/// `Ok(None)` when the file does not exist — distinct from an empty file, so
/// the caller can decline to create one. A symlink that points at nothing
/// resolves to nothing and lands here too, which is what
/// [`std::fs::read_to_string`] used to do with it.
///
/// The failure carries no path context of its own: the one caller names the
/// file in the warning it builds, and a second copy inside the error read as
/// `could not read <path>: reading <path>: Is a directory` — noise in the one
/// line an operator gets about a grant this daemon could not take back. A
/// second caller would have to name the path itself.
fn read(path: &Path) -> Result<Option<Snapshot>> {
    // Resolve the symlinks here, not at write time: a link is followed to its
    // target so `rename` replaces the file rather than the link that a dotfiles
    // setup deliberately points elsewhere, and doing it before the read is what
    // makes the read and the write the same file.
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut file = match std::fs::File::open(&target) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text)?;
    // `fstat`, so this is the file the bytes above came from and cannot be the
    // one that took its name in the meantime.
    let meta = file.metadata()?;
    use std::os::unix::fs::MetadataExt;
    Ok(Some(Snapshot {
        target,
        text,
        dev: meta.dev(),
        ino: meta.ino(),
    }))
}

/// `None` when `path` still resolves to the file [`read`] read, that file is
/// still the same inode, and it still holds the same bytes — the three things a
/// replacement computed from those bytes needs to be true to be an edit rather
/// than a rollback. `Some(reason)` naming the first that is not.
///
/// Called immediately before the `rename` and nowhere else. It cannot make the
/// replacement atomic with respect to other writers — POSIX offers no
/// rename-if-unchanged, and a few microseconds between this check and the
/// `rename` stay unguarded — but it narrows the window from "everything since
/// the read", which includes reading, parsing and rebuilding the file, down to
/// those microseconds. The remaining sliver is worth stating plainly: this
/// detects and declines, it does not serialise other processes, and it never
/// claims to.
fn moved_on(path: &Path, snapshot: &Snapshot) -> Option<String> {
    // The name first, because a repointed symlink leaves the file that was read
    // perfectly intact and would pass every check below while no longer being
    // the file sshd consults.
    match std::fs::canonicalize(path) {
        Ok(target) if target == snapshot.target => {}
        Ok(target) => {
            return Some(format!(
                "{} now resolves to {} rather than the {} that was read",
                path.display(),
                target.display(),
                snapshot.target.display()
            ))
        }
        Err(err) => {
            return Some(format!(
                "{} no longer resolves to anything: {err}",
                path.display()
            ))
        }
    }

    // `symlink_metadata` rather than `metadata`: the resolved target is not a
    // link, so anything that is one now is something that took its name.
    match std::fs::symlink_metadata(&snapshot.target) {
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            if (meta.dev(), meta.ino()) != (snapshot.dev, snapshot.ino) {
                return Some(format!(
                    "{} is a different file from the one that was read",
                    snapshot.target.display()
                ));
            }
        }
        Err(err) => {
            return Some(format!(
                "{} can no longer be examined: {err}",
                snapshot.target.display()
            ))
        }
    }

    // Same name, same file — and possibly a `>>` that appended to it in place,
    // which neither check above can see.
    match std::fs::read(&snapshot.target) {
        Ok(now) if now == snapshot.text.as_bytes() => None,
        Ok(_) => Some(format!(
            "the content of {} changed after it was read",
            snapshot.target.display()
        )),
        Err(err) => Some(format!(
            "{} could not be re-read to confirm it is unchanged: {err}",
            snapshot.target.display()
        )),
    }
}

/// A test's chance to act on the file after the sweep has read it and before it
/// checks and rewrites it — the window this module's rule 5 is about, which no
/// test can otherwise hit deterministically. Nothing in a release build calls
/// it.
#[cfg(not(test))]
fn after_read(_path: &Path) {}

/// See [`AfterRead`]. Held for the duration of the hook, which is why a hook
/// must not itself reach a sweep.
///
/// Fires only for the file the hook was armed against, which narrows a real
/// hazard without closing it. A sweep is reachable from `Daemon::revoke` as well
/// as from this module's own tests, and it runs on a blocking-pool thread that
/// belongs to no test; a revocation test that forgets its
/// [`test_support::FakeHome`] therefore sweeps whichever fake home is installed
/// and fires whatever hook is armed. That is how a sweep from `ipc_server`'s
/// tests once rewrote another test's fixture mid-run, and the tests that failed
/// were the ones it happened to.
///
/// What the path key buys is that a sweep of a *different* file cannot reach an
/// armed hook. What it cannot buy is isolation from a forgetful caller: with
/// `$HOME` process-global, a sweep taken without the lock resolves to the same
/// path as the test that holds it, so the key matches and the hook fires. The
/// [`test_support::FakeHome`] guard is what prevents that, and this is a second
/// wall rather than a replacement for it.
#[cfg(test)]
fn after_read(path: &Path) {
    let mut armed = AFTER_READ
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some((armed_for, hook)) = armed.as_mut() {
        if armed_for == path {
            hook();
        }
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity)]
static AFTER_READ: std::sync::Mutex<Option<(PathBuf, Box<dyn FnMut() + Send>)>> =
    std::sync::Mutex::new(None);

/// Arms the seam above for one file, for the duration of one test, and disarms
/// it on drop, in the house style: a test that panics part-way cannot leave its
/// hook running inside the next test's sweep.
///
/// Two things keep the process-wide static safe. Every test able to reach a
/// sweep holds a [`test_support::FakeHome`], which holds one process-wide lock
/// for its lifetime, so two armed hooks cannot overlap; and the hook is armed
/// against a path, so a sweep of any other file cannot reach it even when a
/// caller skips that lock.
#[cfg(test)]
struct AfterRead;

#[cfg(test)]
impl AfterRead {
    fn runs(path: &Path, hook: impl FnMut() + Send + 'static) -> AfterRead {
        *AFTER_READ
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) =
            Some((path.to_path_buf(), Box::new(hook)));
        AfterRead
    }
}

#[cfg(test)]
impl Drop for AfterRead {
    fn drop(&mut self) {
        *AFTER_READ
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

/// The device id of a marker comment — `# codeconnect:<id> name="…" added <ts>`
/// — where the tag is the first word after the `#`. `None` for every other
/// comment, including one that merely mentions the tag later in the line.
fn marker_tag(line: &str) -> Option<&str> {
    let comment = line
        .trim_end_matches(['\n', '\r'])
        .trim_start()
        .strip_prefix('#')?;
    comment.split_whitespace().next().and_then(tag_device_id)
}

/// The device id of an installed key line — exactly the three fields earlier
/// releases wrote, `ssh-ed25519 <key material> codeconnect:<id>`. A line with
/// an options prefix, another key type, or extra fields is outside that shape,
/// whatever its comment field says. Matching the shape is not evidence that an
/// earlier release wrote the line, and nothing downstream treats it as such:
/// it is the narrowest description of what was written that a file of raw text
/// can be checked against.
fn key_tag(line: &str) -> Option<&str> {
    let fields: Vec<&str> = line
        .trim_end_matches(['\n', '\r'])
        .split_whitespace()
        .collect();
    match fields.as_slice() {
        ["ssh-ed25519", _material, tag] => tag_device_id(tag),
        _ => None,
    }
}

/// A device id is generated hex, and earlier releases narrowed it to this
/// alphabet before writing it, so a field outside the alphabet is outside the
/// shape and no candidate. The narrowing only rules lines out; a field inside
/// the alphabet is a field anyone could have typed.
fn tag_device_id(field: &str) -> Option<&str> {
    let device_id = field.strip_prefix(TAG_PREFIX)?;
    (!device_id.is_empty()
        && device_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some(device_id)
}

/// A replacement written and made durable under a name of its own, one
/// `rename` away from being the file — and, until [`Staged::commit`] says so,
/// one `Drop` away from never having existed.
///
/// Splitting the write from the rename is what lets the confirmation sit
/// between them. Creating the temporary, writing it and `fsync`ing it is the
/// slow part of a replacement by orders of magnitude, and doing it *before* the
/// file is re-checked puts that whole span inside what the check covers instead
/// of after it.
struct Staged {
    temp: PathBuf,
    target: PathBuf,
}

impl Staged {
    fn commit(self) -> Result<()> {
        std::fs::rename(&self.temp, &self.target)
            .with_context(|| format!("replacing {}", self.target.display()))?;

        // `rename` is atomic but the *directory entry* is not durable until the
        // directory itself is synced. Best-effort: a directory that cannot be
        // opened for sync is not a reason to undo a rename that succeeded.
        if let Some(dir) = self.target.parent() {
            if let Ok(handle) = std::fs::File::open(dir) {
                let _ = handle.sync_all();
            }
        }
        Ok(())
    }
}

impl Drop for Staged {
    /// After a `commit` that landed, the temporary's name is already gone and
    /// this does nothing. After anything else — a failed rename, an error on
    /// the way, a check that came back saying the file moved on, a panic — it is
    /// what stops a replacement full of keys sitting in `~/.ssh` under a name
    /// nobody will recognise.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
    }
}

/// Write the replacement beside `target`, `fsync` it, and hand back the one
/// step that has not been taken.
///
/// `target` is the resolution [`read`] made — already followed through any
/// symlink, so the eventual `rename` replaces the file rather than a link a
/// dotfiles setup deliberately points elsewhere. Resolving it here instead would
/// be resolving it a second time, against a name that may have moved since the
/// bytes were read.
fn stage(target: &Path, contents: &str) -> Result<Staged> {
    let dir = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;
    let mode = mode_of(target).unwrap_or(0o600);

    // A predictable name in a directory whose whole purpose is deciding who may
    // log in is a plant waiting to happen: anything able to create a file there
    // first could point that name at a file of its choosing. 128 random bits
    // removes the guess and `create_new` removes the plant — `O_CREAT|O_EXCL`
    // fails outright on an existing path *and* refuses to follow a symlink at
    // the final component.
    let temp = dir.join(format!(
        ".authorized_keys.codeconnect.{}.tmp",
        crate::secret::hex(&crate::secret::random_bytes::<16>()?)
    ));

    use std::os::unix::fs::OpenOptionsExt;
    // Created *at* the final mode rather than created and then narrowed, which
    // would leave a window at whatever the umask allows. `set_mode` afterwards
    // pins the exact bits, since the creation mode is itself masked by the
    // umask.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    // From here on the temporary exists, so every failure has to take it away
    // again — which is `Staged`'s job, and why it is built before anything else
    // can go wrong.
    let staged = Staged {
        temp,
        target: target.to_path_buf(),
    };
    fill(&mut file, &staged.temp, contents, mode)
        .with_context(|| format!("writing {}", staged.temp.display()))?;
    // Closed here, with its data already on the way to disk, rather than left to
    // the end of the caller's scope: nothing should hold a handle to a file that
    // is about to become `authorized_keys`.
    drop(file);
    Ok(staged)
}

/// Everything that can fail once the temporary exists, so one [`Staged`] covers
/// the cleanup for all of it.
fn fill(file: &mut std::fs::File, path: &Path, contents: &str, mode: u32) -> Result<()> {
    set_mode(path, mode)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// What a test needs to reach a sweep safely: a redirected `HOME`, and the file
/// content to point it at.
///
/// Taking the guard is the only supported way to run a test that reaches
/// [`purge_authorized_keys`]: a test that wrote to the developer's real
/// `~/.ssh/authorized_keys` would be a genuinely bad day.
///
/// `pub(crate)` because the second caller of the sweep is `state::Daemon::revoke`,
/// and the tests that hold *it* to sweeping have to say what was in the file and
/// what survived — in the same words as the tests below, so the two cannot drift
/// into disagreeing about what an installed entry looks like.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Two real-shaped ed25519 entries as an earlier release wrote them, around
    /// two keys the user put there themselves — one of which names this project
    /// in its own comment without carrying the tag.
    pub(crate) const MIXED: &str = "\
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gH laptop@home

# a key I added myself for codeconnect work
ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ backup-machine

# codeconnect:d1a2b3c4 name=\"iPhone\" added 2026-07-30T10:00:00.000Z
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gI codeconnect:d1a2b3c4
";

    /// What must survive [`MIXED`] byte for byte: the blank line, the comment
    /// mentioning the project, both user keys, and the order they are in.
    pub(crate) const SURVIVORS: &str = "\
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gH laptop@home

# a key I added myself for codeconnect work
ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ backup-machine

";

    pub(crate) fn write(path: &Path, contents: &str, mode: u32) {
        std::fs::write(path, contents).unwrap();
        super::set_mode(path, mode).unwrap();
    }

    /// `HOME` is process-global while tests run in parallel threads, so every
    /// redirect is serialised and restored. Without this, one test's fake home
    /// would still be installed when an unrelated test resolved a path.
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
            std::fs::create_dir_all(dir.join(".ssh")).unwrap();
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", &dir);
            FakeHome {
                dir,
                previous,
                _guard: guard,
            }
        }

        pub(crate) fn keys(&self) -> PathBuf {
            self.dir.join(".ssh").join("authorized_keys")
        }

        /// The file as it stands, or `None` where there is none — the shape the
        /// assertions want, since "no file" is a distinct outcome from "an
        /// empty one".
        pub(crate) fn read_keys(&self) -> Option<String> {
            std::fs::read_to_string(self.keys()).ok()
        }

        /// Make a replacement impossible. The *directory* is what one needs:
        /// the temporary is created in it and renamed over the original, so a
        /// file that is readable inside an unwritable `~/.ssh` is exactly the
        /// case where a sweep reads, counts, and cannot finish. `Drop` puts the
        /// mode back, so the fake home can still be cleaned up.
        pub(crate) fn seal(&self) {
            super::set_mode(&self.dir.join(".ssh"), 0o500).unwrap();
        }

        /// Names in `~/.ssh` other than `authorized_keys`: the leftovers a
        /// replacement that was written and abandoned would show up as.
        pub(crate) fn strays(&self) -> Vec<std::ffi::OsString> {
            std::fs::read_dir(self.dir.join(".ssh"))
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                .filter(|name| name != "authorized_keys")
                .collect()
        }
    }

    impl Drop for FakeHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::set_permissions(
                self.dir.join(".ssh"),
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            );
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{write, FakeHome, MIXED, SURVIVORS};
    use super::*;

    #[test]
    fn a_tagged_entry_goes_and_the_users_own_keys_do_not() {
        let home = FakeHome::new("purge");
        write(&home.keys(), MIXED, 0o600);

        assert_eq!(
            purge_authorized_keys(),
            2,
            "the marker line and the key line"
        );
        assert_eq!(std::fs::read_to_string(home.keys()).unwrap(), SURVIVORS);
        assert_eq!(
            mode_of(&home.keys()),
            Some(0o600),
            "the replacement keeps owner-only permissions"
        );
    }

    #[test]
    fn a_second_boot_finds_nothing_and_writes_nothing() {
        let home = FakeHome::new("idempotent");
        write(&home.keys(), MIXED, 0o600);
        assert_eq!(purge_authorized_keys(), 2);

        let before = std::fs::metadata(home.keys()).unwrap();
        assert_eq!(purge_authorized_keys(), 0);
        let after = std::fs::metadata(home.keys()).unwrap();

        assert_eq!(std::fs::read_to_string(home.keys()).unwrap(), SURVIVORS);
        // The inode is the proof that no replacement was renamed into place:
        // every write here goes through a fresh temporary, so an untouched
        // file is the same file.
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after),
            "a file with nothing to remove must not be rewritten"
        );
        // No temporary was left behind either.
        let strays = home.strays();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn a_file_of_nothing_but_the_users_own_keys_is_left_alone() {
        let home = FakeHome::new("untouched");
        write(&home.keys(), SURVIVORS, 0o644);

        assert_eq!(purge_authorized_keys(), 0);
        assert_eq!(std::fs::read_to_string(home.keys()).unwrap(), SURVIVORS);
        assert_eq!(mode_of(&home.keys()), Some(0o644), "not even the mode");
    }

    #[test]
    fn a_mac_that_never_granted_ssh_access_has_no_file_and_gets_none() {
        let home = FakeHome::new("absent");
        assert!(!home.keys().exists());

        assert_eq!(purge_authorized_keys(), 0);
        assert!(
            !home.keys().exists(),
            "removing nothing must not create the file"
        );
    }

    #[test]
    fn a_file_that_cannot_be_rewritten_warns_instead_of_stopping_the_daemon() {
        let home = FakeHome::new("readonly");
        write(&home.keys(), MIXED, 0o600);
        home.seal();

        assert_eq!(
            purge_authorized_keys(),
            0,
            "nothing was removed, and the caller is told so"
        );
        assert_eq!(
            std::fs::read_to_string(home.keys()).unwrap(),
            MIXED,
            "a failed replacement leaves the original whole"
        );
    }

    /// The other failure, and the one that has established nothing at all: the
    /// read itself fails, so the daemon never sees a byte of the file.
    ///
    /// A directory where the file should be produces that deterministically and
    /// without a root shell or a permission trick: `read_to_string` opens a
    /// directory happily on macOS and takes `EISDIR` off the first `read(2)`.
    /// `IsADirectory` is not `NotFound`, so it lands in the failure arm rather
    /// than the "no file, nothing to do" one — which is the distinction the
    /// arm's message depends on and nothing else here covers.
    #[test]
    fn a_file_that_cannot_be_read_warns_instead_of_stopping_the_daemon() {
        let home = FakeHome::new("unreadable");
        std::fs::create_dir(home.keys()).unwrap();

        assert!(
            matches!(purge(&home.keys()), Err(Stopped::Unread(_))),
            "a failed read has to reach the arm that knows nothing about the file, \
             not the one that counted lines in it"
        );
        assert_eq!(
            purge_authorized_keys(),
            0,
            "nothing was removed, and the caller is told so rather than unwound at"
        );
        assert!(
            home.keys().is_dir(),
            "a sweep that could not read the path must not have written over it"
        );
    }

    /// The one line of `told` an operator would find by looking for `needle`.
    fn warning<'a>(told: &'a str, needle: &str) -> &'a str {
        told.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("nothing an operator can read said {needle:?}: {told}"))
    }

    /// **A failed sweep says what it established and not one word more.**
    ///
    /// Every other test here reads a count out of the return value, which is a
    /// convenience for tests: a sweep that removed nothing and a sweep that had
    /// nothing to remove both return `0`. What separates them is the line on
    /// stderr, and that line is the entirety of what an operator gets. Delete
    /// any one of these `log_warn!`s and every other test stays green while a
    /// Mac keeps a shell it granted and says nothing about it.
    ///
    /// All four run in one child because the point is that they differ. No
    /// `$HOME` resolved no path, so the check it hands over is the one an
    /// operator resolves themselves. The read failure has not seen the file, so
    /// it may not describe what is in it — this warning used to say the file
    /// "still contains lines tagged `codeconnect:…`", which on that path is an
    /// assertion about bytes the daemon never got. The write failure *did* read
    /// it and counted whole pairs in it, so it may not hedge either: one
    /// message covering both would have to invent content on one path or file a
    /// confirmed live grant as a maybe on the other. The fourth read it too,
    /// and then watched that read go out of date, so it is the one path that
    /// owes a count *and* a hedge — the count is what it saw, and what is there
    /// now belongs to whoever wrote it last.
    ///
    /// Read by re-running this one test in a child process. `cargo test`
    /// captures each test's output for the harness rather than handing it to
    /// the test, and this crate's logger writes to the process's own stderr
    /// with nothing in front of it that a test could stand in.
    #[test]
    fn a_failed_sweep_tells_an_operator_only_what_it_established() {
        const CHILD: &str = "CCD_LEGACY_SWEEP_LOG_CHILD";
        const THIS_TEST: &str =
            "legacy_credentials::tests::a_failed_sweep_tells_an_operator_only_what_it_established";
        if std::env::var_os(CHILD).is_some() {
            // Each home is dropped before the next is built: `FakeHome` holds
            // the `HOME` lock for its lifetime, and `HOME` is one variable. The
            // guard is what puts it back after the middle case takes it away.
            {
                let _home = FakeHome::new("homeless-child");
                std::env::remove_var("HOME");
                assert_eq!(purge_authorized_keys(), 0);
            }
            {
                let home = FakeHome::new("unreadable-child");
                std::fs::create_dir(home.keys()).unwrap();
                assert_eq!(purge_authorized_keys(), 0);
            }
            {
                let home = FakeHome::new("readonly-child");
                write(&home.keys(), MIXED, 0o600);
                home.seal();
                assert_eq!(purge_authorized_keys(), 0);
            }
            {
                let home = FakeHome::new("changed-child");
                write(&home.keys(), MIXED, 0o600);
                let keys = home.keys();
                let _hook = AfterRead::runs(&home.keys(), move || {
                    append(&keys, "ssh-ed25519 AAAA someone-else@desk\n");
                });
                assert_eq!(purge_authorized_keys(), 0);
            }
            return;
        }

        let swept = std::process::Command::new(std::env::current_exe().unwrap())
            .args([THIS_TEST, "--exact", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let told = String::from_utf8_lossy(&swept.stderr);
        assert!(
            swept.status.success(),
            "a sweep it could not finish must not take the daemon down with it: {told}"
        );

        // No path was ever resolved, so the check is the one the operator can
        // resolve themselves, in the account this daemon runs as.
        let homeless = warning(&told, "no absolute $HOME");
        assert!(
            homeless.contains("still grant a phone a shell on this Mac")
                && homeless.contains("`grep -n codeconnect: ~/.ssh/authorized_keys`"),
            "a sweep that never resolved a path still owes the risk and a check: {homeless}"
        );

        // Nothing was read, so nothing may be said about the content. The risk
        // is still named — in the conditional, which is the strongest form the
        // daemon is entitled to here.
        let unread = warning(&told, "could not read ");
        assert!(
            unread.contains("removed nothing") && unread.contains("has not seen what is in"),
            "a sweep that never opened the file has to say so: {unread}"
        );
        assert!(
            unread.contains("If an earlier release installed lines tagged"),
            "the risk is real and unconfirmed, so it is stated as a condition: {unread}"
        );
        assert!(
            !unread.contains("still contains"),
            "this asserts content of a file the daemon never read: {unread}"
        );
        assert!(
            !unread.contains("line(s) this sweep"),
            "a file that was never read has no count to quote: {unread}"
        );

        // This one read the file and counted whole pairs in it. The count is
        // the fact, and hedging it would report a confirmed live grant as a
        // possibility.
        //
        // What it may not do is claim the count still describes the file. Those
        // are different sentences, and only the first is knowable: staging now
        // happens before the change-check, so a write can fail on a file
        // somebody else has already replaced — and this sweep never read what
        // they put there. So the message states what it read and what it did,
        // and hands the present tense to the `grep`, which is the only thing
        // here that looks at the file as it stands.
        let unwritten = warning(&told, "could not rewrite ");
        assert!(
            unwritten.contains("read 2 line(s)") && unwritten.contains("removed none of them"),
            "what the sweep counted and failed to remove is what it has to name: {unwritten}"
        );
        assert!(
            !unwritten.contains("are still there"),
            "the file may have been replaced under a failed write, so its present \
             contents are not this warning's to assert: {unwritten}"
        );
        assert!(
            !unwritten.contains("has not seen what is in")
                && !unwritten.contains("If an earlier release installed"),
            "this read the file; a grant it confirmed may not be reported as a maybe: \
             {unwritten}"
        );

        // And the third: it read the file, so it may quote the count it took
        // off that read — but the read is exactly what went out of date, so
        // every word about the file *now* is conditional, and the thing it is
        // certain of is that it wrote nothing.
        let changed = warning(&told, "kept changing while it was being swept");
        assert!(
            changed.contains("removed nothing rather than put a replacement")
                && changed.contains("As of the last read, 2 line(s)"),
            "a declined sweep owes the operator both facts: it wrote nothing, and what it \
             had counted before it stopped: {changed}"
        );
        assert!(
            changed.contains("if they are still there"),
            "the file moved on after that count, so its content now is not something this \
             path may assert: {changed}"
        );
        assert!(
            !told.contains("SSH ACCESS WITHDRAWN"),
            "no sweep in the child could finish, so none of them may announce a withdrawal: \
             {told}"
        );

        // All three that named a file hand over the check, aimed at the real
        // file rather than a placeholder, so a reader can settle it instead of
        // trusting any of them.
        for line in [unread, unwritten, changed] {
            let check = line
                .split("grep -n codeconnect: ")
                .nth(1)
                .unwrap_or_else(|| panic!("no check a reader can run: {line}"));
            assert!(
                check.starts_with('/') && check.contains("/.ssh/authorized_keys"),
                "the check has to name the file this daemon meant: {line}"
            );
        }
    }

    #[test]
    fn only_the_two_line_shapes_this_project_wrote_are_recognised() {
        for (marker, id) in [
            ("# codeconnect:d1a2b3c4", "d1a2b3c4"),
            ("#codeconnect:d1a2b3c4", "d1a2b3c4"),
            (
                "  #   codeconnect:d1a2b3c4 name=\"iPhone\" added 2026-07-30T10:00:00.000Z",
                "d1a2b3c4",
            ),
        ] {
            assert_eq!(marker_tag(marker), Some(id), "a marker: {marker:?}");
            assert_eq!(key_tag(marker), None);
        }

        for (key, id) in [
            (
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2b3c4",
                "d1a2b3c4",
            ),
            (
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:unknown\n",
                "unknown",
            ),
            (
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2b3c4\r\n",
                "d1a2b3c4",
            ),
        ] {
            assert_eq!(key_tag(key), Some(id), "an installed key: {key:?}");
            assert_eq!(marker_tag(key), None);
        }

        for theirs in [
            "",
            "\n",
            "# codeconnect",
            "# codeconnect:",
            // The tag is the *first* word of a marker, and nowhere else.
            "# my key for codeconnect:d1a2b3c4",
            // An installed key line is exactly three fields: a comment after
            // the tag, an options prefix before the type, another key type, or
            // a missing field puts a line outside the shape, and outside the
            // shape is all this sweep can tell about it.
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2b3c4 laptop@home",
            "command=\"echo hi\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2b3c4",
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ codeconnect:d1a2b3c4",
            "ssh-ed25519 codeconnect:d1a2b3c4",
            // A device id outside the alphabet earlier releases wrote.
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2/b3c4",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 laptop@home",
        ] {
            assert_eq!(marker_tag(theirs), None, "not a marker: {theirs:?}");
            assert_eq!(key_tag(theirs), None, "not an installed key: {theirs:?}");
        }
    }

    /// The half of the contract the classifier alone cannot carry: a key line
    /// is removed only *under its own marker*, and a marker only *above its own
    /// key*. Nothing here is a complete pair, so nothing here goes.
    #[test]
    fn a_key_line_is_only_removed_as_the_second_half_of_its_pair() {
        let home = FakeHome::new("pairing");
        const UNPAIRED: &str = "\
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:handmade
# codeconnect:orphan name=\"iPhone\" added 2026-07-30T10:00:00.000Z
ssh-rsa AAAAB3 not-the-key-below-the-marker
# codeconnect:aaa name=\"iPhone\" added 2026-07-30T10:00:00.000Z
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:bbb
";
        write(&home.keys(), UNPAIRED, 0o600);

        assert_eq!(
            purge_authorized_keys(),
            0,
            "no key line sits directly under a marker carrying its own id"
        );
        assert_eq!(std::fs::read_to_string(home.keys()).unwrap(), UNPAIRED);
    }

    /// **A marker is never taken away from the key it names.**
    ///
    /// An installed pair separated by one blank line — an `ssh-copy-id`, a hand
    /// edit — is not a pair this sweep can remove. Removing the marker anyway
    /// would leave the key working and unlabelled: a live grant to a phone this
    /// daemon does not manage, revoke or list, and one that whoever audits the
    /// file has nothing left to identify by.
    #[test]
    fn a_marker_separated_from_its_key_takes_neither_line() {
        let home = FakeHome::new("separated");
        const SEPARATED: &str = "\
# codeconnect:d1a2b3c4 name=\"iPhone\" added 2026-07-30T10:00:00.000Z

ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2b3c4
";
        write(&home.keys(), SEPARATED, 0o600);

        // Seen, and counted as something the operator has to deal with: the
        // daemon says so rather than passing over a live grant in silence.
        assert_eq!(
            purge(&home.keys()).unwrap().left,
            2,
            "both lines carry the tag and neither can be removed safely"
        );
        assert_eq!(purge_authorized_keys(), 0);
        let left = std::fs::read_to_string(home.keys()).unwrap();
        assert_eq!(left, SEPARATED, "neither line went");
        // The property that matters is not the byte equality above but this:
        // the key is still identifiable as this project's, because the line
        // that says so is still there.
        assert!(
            left.contains("# codeconnect:d1a2b3c4"),
            "the key's only label must survive with it: {left}"
        );
    }

    /// `HOME` unset, and `HOME` relative: the one file this module rewrites
    /// decides who may log in, so an unresolvable home skips the sweep rather
    /// than guessing at a path relative to wherever the daemon happens to be
    /// running.
    ///
    /// Both halves are here because only one of them can see the guard. An
    /// unset `HOME` yields `None` whether or not the path is checked for being
    /// absolute, so that half would pass against a build with the check deleted;
    /// a relative one is the sole input the two answers differ on. This test
    /// used to name the relative case in its own docstring and exercise only the
    /// unset one.
    #[test]
    fn an_unresolvable_home_is_a_no_op() {
        let home = FakeHome::new("homeless");
        write(&home.keys(), MIXED, 0o600);

        std::env::remove_var("HOME");
        assert_eq!(purge_authorized_keys(), 0);
        std::env::set_var("HOME", &home.dir);
        assert_eq!(
            std::fs::read_to_string(home.keys()).unwrap(),
            MIXED,
            "nothing was touched while HOME was unset"
        );

        // A relative `HOME` resolves against the daemon's working directory,
        // which is not a place it may rewrite an `authorized_keys` in. This is
        // the case the `is_absolute` filter exists for, and the only one that
        // fails without it.
        std::env::set_var("HOME", "relative/home");
        assert_eq!(
            authorized_keys_path(),
            None,
            "a relative HOME resolves to no path this module will touch"
        );
        assert_eq!(purge_authorized_keys(), 0);
        std::env::set_var("HOME", &home.dir);
        assert_eq!(
            std::fs::read_to_string(home.keys()).unwrap(),
            MIXED,
            "nothing was touched while HOME was relative either"
        );
    }

    /// A key somebody else adds while the sweep is working — an `ssh-copy-id`,
    /// a dotfiles sync, an editor saving the file.
    const THEIR_NEW_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gJ \
         new-laptop@desk\n";

    fn append(path: &Path, line: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(line.as_bytes()).unwrap();
    }

    /// **The whole reason the file is re-checked: a sweep may not pay for two
    /// tagged lines with a key it never read.**
    ///
    /// Somebody appends between the read and the rewrite — the window an
    /// `ssh-copy-id` or a dotfiles sync lands in, and one that `revoke` made
    /// reachable during ordinary operation rather than only at boot. Renaming
    /// the replacement computed from the older bytes would silently take that
    /// key back out and report success.
    ///
    /// The retry is what turns "declined" into "swept": the second attempt
    /// reads what the writer left and sweeps *that*, so the operator gets both
    /// their key and their revocation.
    #[test]
    fn a_key_added_after_the_read_survives_and_is_swept_around() {
        let home = FakeHome::new("concurrent-edit");
        write(&home.keys(), MIXED, 0o600);

        let keys = home.keys();
        let mut written = false;
        let _hook = AfterRead::runs(&home.keys(), move || {
            if std::mem::replace(&mut written, true) {
                return;
            }
            append(&keys, THEIR_NEW_KEY);
        });

        assert_eq!(
            purge_authorized_keys(),
            2,
            "the retry reads the file as its writer left it and sweeps that"
        );
        assert_eq!(
            std::fs::read_to_string(home.keys()).unwrap(),
            format!("{SURVIVORS}{THEIR_NEW_KEY}"),
            "the key that arrived mid-sweep is still there, and the tagged pair is not"
        );
    }

    /// And when the retries are used up: nothing is written at all. The file
    /// belongs to whoever is writing it, and a stale replacement is not an
    /// improvement on leaving it alone.
    #[test]
    fn a_file_rewritten_under_every_attempt_is_left_as_its_writer_left_it() {
        let home = FakeHome::new("churn");
        write(&home.keys(), MIXED, 0o600);

        let keys = home.keys();
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&writes);
        let _hook = AfterRead::runs(&home.keys(), move || {
            let nth = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            append(&keys, &format!("ssh-ed25519 AAAA{nth} someone-else@desk\n"));
        });

        match purge(&home.keys()) {
            Err(Stopped::Changed { doomed, .. }) => {
                assert_eq!(doomed, 2, "the count belongs to the read that went stale");
            }
            other => panic!("a file that kept moving must not be rewritten: {other:?}"),
        }
        // One competing write must not cost the sweep; only a file nobody stops
        // writing does. The hook runs once per read, so this counts the reads.
        assert_eq!(
            writes.load(std::sync::atomic::Ordering::Relaxed),
            ATTEMPTS,
            "every attempt went back to the file rather than reusing the first read"
        );

        let left = std::fs::read_to_string(home.keys()).unwrap();
        assert!(
            left.starts_with(MIXED),
            "nothing was removed, so what the sweep read is still there: {left}"
        );
        for nth in 0..ATTEMPTS {
            assert!(
                left.contains(&format!("AAAA{nth} someone-else@desk")),
                "every competing write survives whole: {left}"
            );
        }
        // And no temporary left behind. A stale attempt does create one — the
        // replacement is staged and fsynced before the check that rejects it,
        // so the slow part of a write happens inside the window the check
        // covers — so what this pins is the cleanup: an uncommitted `Staged`
        // removes its own file on the way out.
        let strays = home.strays();
        assert!(strays.is_empty(), "{strays:?}");
    }

    /// **Same name, same bytes, different file — and the sweep goes back for
    /// it.**
    ///
    /// This is what the `(dev, ino)` half of the check is for, and the only way
    /// to see it work: somebody replaced the file the way everything careful
    /// replaces it, by renaming a new one over the name, and the bytes they put
    /// there happen to match what was read. Comparing content alone cannot tell
    /// that apart from nothing having happened.
    ///
    /// The file it lands on is the same either way — a `rename` over a name is a
    /// `rename` over a name — so what is asserted is the read: a sweep that
    /// noticed goes back to the file, and one that did not renames a replacement
    /// derived from a file it no longer holds onto a writer's half-finished
    /// work. `ssh-copy-id` is temp-then-rename followed by more; being one read
    /// behind it is where the finding's whole class of bug lives.
    #[test]
    fn a_file_replaced_by_a_rename_is_read_again_even_when_the_bytes_look_the_same() {
        let home = FakeHome::new("replaced");
        write(&home.keys(), MIXED, 0o600);

        let keys = home.keys();
        let understudy = home.dir.join(".ssh").join("their-replacement");
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&reads);
        let _hook = AfterRead::runs(&home.keys(), move || {
            if counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
                return;
            }
            write(&understudy, MIXED, 0o600);
            std::fs::rename(&understudy, &keys).unwrap();
        });

        assert_eq!(purge_authorized_keys(), 2);
        assert_eq!(
            std::fs::read_to_string(home.keys()).unwrap(),
            SURVIVORS,
            "the sweep still finishes — noticing is a retry, not a refusal"
        );
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "the replacement had to be computed from the file that is actually there, \
             which means reading it after somebody swapped it"
        );
    }

    /// **A repointed link must not carry one target's content onto another.**
    ///
    /// `authorized_keys` is a symlink a dotfiles setup manages, and it is
    /// repointed — a `stow`, a profile switch — after the sweep has read the
    /// old target. Resolving the path a second time at write time renames the
    /// replacement computed from the *old* target over the *new* one: the new
    /// target loses keys this daemon never read, the old one keeps the grant the
    /// sweep meant to remove, and the log says two lines were withdrawn.
    ///
    /// Resolving once, before the read, makes that impossible; the retry then
    /// picks up the file the link now names and sweeps that instead.
    #[test]
    fn a_link_repointed_mid_sweep_does_not_carry_one_target_onto_another() {
        let home = FakeHome::new("retarget");
        // Unrelated keys, in the file the link is about to name. Nothing here
        // carries the tag, so a sweep that reaches it honestly does nothing.
        const THEIRS: &str = "\
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0JGKZ3rL5vhF2dxJ8kX9wQqM4tN6pS1aB7cD3eF5gK desktop@studio
ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ nas
";
        let was = home.dir.join("dotfiles-work-authorized_keys");
        let now = home.dir.join("dotfiles-home-authorized_keys");
        write(&was, MIXED, 0o600);
        write(&now, THEIRS, 0o600);
        std::os::unix::fs::symlink(&was, home.keys()).unwrap();

        let link = home.keys();
        let repointed = now.clone();
        let mut moved = false;
        let _hook = AfterRead::runs(&home.keys(), move || {
            if std::mem::replace(&mut moved, true) {
                return;
            }
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&repointed, &link).unwrap();
        });

        assert_eq!(
            purge_authorized_keys(),
            0,
            "the file the link now names holds nothing this sweep may remove"
        );
        assert_eq!(
            std::fs::read_to_string(&now).unwrap(),
            THEIRS,
            "keys in the file the link now names were never read by this sweep and must \
             not be replaced by anything it computed"
        );
        assert_eq!(
            std::fs::read_to_string(&was).unwrap(),
            MIXED,
            "and the target it resolved first is not rewritten from behind either"
        );
    }

    /// A symlinked `authorized_keys` — a dotfiles setup — has its *target*
    /// rewritten; the link itself survives.
    #[test]
    fn a_symlinked_file_is_rewritten_through_the_link() {
        let home = FakeHome::new("symlink");
        let real = home.dir.join("dotfiles-authorized_keys");
        write(&real, MIXED, 0o600);
        std::os::unix::fs::symlink(&real, home.keys()).unwrap();

        assert_eq!(purge_authorized_keys(), 2);
        assert!(
            std::fs::symlink_metadata(home.keys())
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), SURVIVORS);
    }

    #[test]
    fn a_file_whose_last_line_has_no_newline_keeps_it_that_way() {
        let home = FakeHome::new("no-trailing-newline");
        write(
            &home.keys(),
            "# codeconnect:d1a2b3c4\n\
             ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 codeconnect:d1a2b3c4\n\
             ssh-ed25519 AAAA laptop@home",
            0o600,
        );

        assert_eq!(purge_authorized_keys(), 2, "the pair, and only the pair");
        assert_eq!(
            std::fs::read_to_string(home.keys()).unwrap(),
            "ssh-ed25519 AAAA laptop@home",
            "the survivor is reproduced byte for byte, terminator and all"
        );
    }
}
