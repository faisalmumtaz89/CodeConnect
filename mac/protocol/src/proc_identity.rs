//! Kernel-backed process **birth identity**, OS **boot identity**, and a
//! **boot-relative monotonic clock** — the primitives D5/D6/D7 stand on.
//!
//! The governing invariant (D5): CodeConnect never signals by name or by bare
//! pid. Every signal, every "is my coordinator still alive?" check, and every
//! launch-record admission binds to a `(pid, birth_identity)` pair. A pid on
//! its own is a liar the instant the kernel reuses it; the birth identity is
//! what makes reuse observable.
//!
//! ## Darwin field choice — [UNVERIFIED], Phase-0 stop-and-amend
//!
//! The birth identity is the process **start time**, read via
//! `proc_pidinfo(PROC_PIDTBSDINFO)` as `proc_bsdinfo.pbi_start_tvsec` /
//! `pbi_start_tvusec` (seconds + microseconds since the epoch). This is the
//! same `proc_pidinfo` family D5's descendant census uses. It is chosen
//! because:
//!
//!   * It is assigned by the kernel at fork and — the load-bearing claim — is
//!     **preserved across `execve`** (D6 needs the identity recorded before the
//!     gate's `execve` to equal the identity observed after it, with the pid
//!     held constant). `execve` replaces the image, not the proc struct, so
//!     `p_starttime` should survive. **This must be proven live before the
//!     `codex` command is ungated** (the plan marks it [UNVERIFIED] under D6).
//!   * Together with the pid it disambiguates pid reuse at microsecond
//!     resolution, which is the D5 canary requirement.
//!
//! The boot identity is `KERN_BOOTTIME` (also a `timeval`); the monotonic clock
//! is `CLOCK_MONOTONIC`. Both are boot-scoped: a value is only comparable to
//! another value read on the **same** boot, which is exactly why the launch
//! record carries the boot identity alongside its monotonic deadline — a reboot
//! (or a restore-from-image) changes the boot identity, and admission then
//! fails closed rather than trusting a stale deadline. That `CLOCK_MONOTONIC`
//! advances consistently across processes and its behaviour across sleep are
//! the other [UNVERIFIED] Phase-0 items.

use serde::{Deserialize, Serialize};

/// A process's kernel-assigned birth time: seconds + microseconds of
/// `p_starttime`. Compared for **exact** equality — this is an identity, not a
/// clock reading, so "close" is not "same".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BirthIdentity {
    pub start_sec: i64,
    pub start_usec: i64,
}

/// A `(pid, birth_identity)` pair: the only thing CodeConnect will signal or
/// treat as "the process I recorded".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub birth: BirthIdentity,
}

/// The OS boot identity: the boot wall-clock instant. Two monotonic readings
/// are only comparable when they were taken under the **same** boot identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootIdentity {
    pub boot_sec: i64,
    pub boot_usec: i64,
}

/// Whether a recorded `(pid, birth)` is still the very process that was
/// recorded. Absence and "cannot tell" are **distinct** — D7 turns on it:
/// `Unavailable` is never treated as absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// This exact pid is live and its start time matches the recorded birth.
    Alive,
    /// The pid is gone, or it is live but its start time differs (reuse) — in
    /// both cases the *recorded* process no longer exists.
    Gone,
    /// The kernel could not be asked (transient sysctl failure). Not absence.
    Unknown,
}

/// Read the `proc_bsdinfo` of `pid`, or `None` when the process does not exist
/// or the kernel could not be queried. Shared by [`read_birth_identity`] and
/// [`read_pgid`] so both come off one kernel answer.
fn read_bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
    // SAFETY: `proc_pidinfo` writes at most `buffersize` bytes into `info` and
    // returns the number written; we zero it first and demand a full struct.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        // A short or zero-length answer means "no such process" (or no
        // permission): that is `None` — read by the caller as Gone — never a
        // birth identity of (0, 0).
        if written != size {
            return None;
        }
        Some(info)
    }
}

/// Read the birth identity of `pid`. `None` when the process does not exist or
/// the kernel could not be queried.
pub fn read_birth_identity(pid: i32) -> Option<BirthIdentity> {
    let info = read_bsdinfo(pid)?;
    Some(BirthIdentity {
        start_sec: info.pbi_start_tvsec as i64,
        start_usec: info.pbi_start_tvusec as i64,
    })
}

/// Read the process group id of `pid` off the same `proc_bsdinfo` answer as its
/// birth identity, so a recorded `(pid, birth, pgid)` triple is internally
/// consistent (D6 records the pgid alongside the birth identity).
pub fn read_pgid(pid: i32) -> Option<i32> {
    Some(read_bsdinfo(pid)?.pbi_pgid as i32)
}

/// The current process's own identity, read the same way every other
/// process's is — no shortcut through `getpid` alone, because a child records
/// itself with this and the owner must be able to reproduce it byte for byte.
pub fn current_identity() -> Option<ProcessIdentity> {
    let pid = std::process::id() as i32;
    Some(ProcessIdentity {
        pid,
        birth: read_birth_identity(pid)?,
    })
}

/// Is `identity` still exactly the process that was recorded?
///
/// Three-valued and **fail-closed in both directions** (Principle D / finding
/// 6): only a *proven* absence or a *proven* identity change is `Gone`; a
/// transient inability to read is `Unknown`, never `Gone` (so a caller never
/// tears a session down on a hiccup) and never `Alive` (so a caller never treats
/// an unreadable process as proof of life).
///
///   * `kill(pid, 0)` ⇒ `ESRCH`  → the pid does not exist → **Gone**.
///   * `kill(pid, 0)` ⇒ `0`/`EPERM` → the pid exists; but a **zombie** (an
///     unreaped, dead child — `SZOMB`) is **Gone**, not alive: its pid still
///     occupies the table and `kill(0)` succeeds, yet the process is dead (finding
///     6). Otherwise confirm identity by start time: equal → **Alive**;
///     different → reuse → **Gone**; a birth read that *fails while the pid
///     exists* is **Unknown**.
///   * any other `kill` errno → **Unknown**.
pub fn liveness(identity: &ProcessIdentity) -> Liveness {
    let exists = unsafe { libc::kill(identity.pid, 0) };
    if exists != 0 {
        let err = std::io::Error::last_os_error().raw_os_error();
        match err {
            Some(libc::ESRCH) => return Liveness::Gone,
            Some(libc::EPERM) => { /* exists but not ours to signal — fall through to identity */
            }
            _ => return Liveness::Unknown,
        }
    }
    // A zombie (an exited-but-unreaped child) still occupies the pid table and
    // `kill(0)` succeeds, but it is DEAD — it must read `Gone`, not `Alive` and not
    // `Unknown`, so a dead-but-unreaped custodian does not block its replacement
    // (finding 6). On macOS `proc_pidinfo` *short-reads* a zombie, so the only
    // reliable status source is `sysctl(KERN_PROC_PID)` (`p_stat == SZOMB`).
    if is_zombie(identity.pid) == Some(true) {
        return Liveness::Gone;
    }
    match read_birth_identity(identity.pid) {
        Some(birth) if birth == identity.birth => Liveness::Alive,
        Some(_) => Liveness::Gone,
        // The pid exists (kill said so) but we could not read its birth: we can
        // prove neither sameness nor absence ⇒ Unknown.
        None => Liveness::Unknown,
    }
}

/// Whether `pid` is a **zombie** (exited, awaiting reap). `Some(true)`/`Some(false)`
/// on a successful status read; `None` when the kernel could not be asked.
///
/// Reads `struct kinfo_proc` via `sysctl(KERN_PROC_PID)` — the `kp_proc.p_stat`
/// byte (offset 36 on 64-bit macOS; `SZOMB == 5`), verified against the live
/// binary. `sysctl` is used rather than `proc_pidinfo` precisely because the
/// latter returns a zero-length answer for a zombie.
fn is_zombie(pid: i32) -> Option<bool> {
    const P_STAT_OFFSET: usize = 36;
    const SZOMB: u8 = 5;
    // A generous buffer; sysctl reports the real length in `size`. sizeof is 648
    // on 64-bit macOS, but we never depend on the exact value.
    let mut buf = [0u8; 1024];
    let mut size = buf.len();
    let mut mib: [libc::c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    // SAFETY: fixed 4-element MIB; `buf`/`size` are the out-param and its length.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size <= P_STAT_OFFSET {
        return None;
    }
    Some(buf[P_STAT_OFFSET] == SZOMB)
}

/// The OS boot identity, or `None` if it could not be read.
pub fn boot_identity() -> Option<BootIdentity> {
    // SAFETY: fixed MIB, single `timeval` out-param.
    unsafe {
        let mut mib: [libc::c_int; 2] = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let mut tv: libc::timeval = std::mem::zeroed();
        let mut size = std::mem::size_of::<libc::timeval>();
        let rc = libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut tv as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        );
        if rc != 0 || size == 0 {
            return None;
        }
        Some(BootIdentity {
            boot_sec: tv.tv_sec as i64,
            boot_usec: tv.tv_usec as i64,
        })
    }
}

/// Nanoseconds on `CLOCK_MONOTONIC`. Boot-relative: only comparable to another
/// reading taken under the same [`boot_identity`]. Never goes backwards under a
/// wall-clock change, which is why the launch deadline is expressed in it.
pub fn monotonic_now_nanos() -> Option<u64> {
    // SAFETY: single `timespec` out-param.
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) != 0 {
            return None;
        }
        Some((ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_identity_is_readable_and_live() {
        let me = current_identity().expect("this process has a birth identity");
        assert_eq!(me.pid, std::process::id() as i32);
        assert_eq!(liveness(&me), Liveness::Alive);
    }

    #[test]
    fn a_wrong_birth_identity_on_a_live_pid_reads_as_gone() {
        // Same pid (ours, definitely alive), deliberately wrong start time:
        // this is the pid-reuse canary. It must be Gone, never Alive.
        let mut me = current_identity().unwrap();
        me.birth.start_usec ^= 0x5A5A;
        assert_eq!(liveness(&me), Liveness::Gone);
    }

    #[test]
    fn a_pid_that_never_existed_here_reads_as_gone_not_unknown() {
        // A very high pid that is not allocated: kill(0) ⇒ ESRCH ⇒ Gone.
        let ghost = ProcessIdentity {
            pid: 0x3FFF_FFFF,
            birth: BirthIdentity {
                start_sec: 1,
                start_usec: 1,
            },
        };
        assert_eq!(liveness(&ghost), Liveness::Gone);
    }

    #[test]
    fn a_reaped_child_reads_as_gone() {
        // Spawn, let it exit, reap it, then assert Gone — the exact shape the
        // custodian relies on to notice its coordinator died.
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn true");
        let pid = child.id() as i32;
        let birth = loop {
            // Read the birth identity while it is (briefly) alive; retry a few
            // times in case we lost the race to its exit.
            if let Some(b) = read_birth_identity(pid) {
                break b;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let mut child = child;
        child.wait().expect("reap true");
        // Give the kernel a moment to retire the pid table entry.
        let id = ProcessIdentity { pid, birth };
        let mut verdict = liveness(&id);
        for _ in 0..50 {
            if verdict == Liveness::Gone {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            verdict = liveness(&id);
        }
        assert_eq!(verdict, Liveness::Gone, "a reaped child must read Gone");
    }

    #[test]
    fn an_unreaped_zombie_reads_as_gone_not_alive() {
        // Finding 6/8: an exited-but-unreaped child is a zombie — its pid still
        // occupies the table, `kill(pid, 0)` succeeds — yet it is DEAD, and must
        // read `Gone`, never `Alive`/`Unknown`, so a dead-but-unreaped custodian
        // does not block its replacement.
        //
        // The child is **synchronized** (blocks reading stdin) so we read its LIVE
        // birth FIRST — a race that let the child exit first would leave us reading
        // the very zombie `proc_pidinfo` short-reads, and the old loop could hang.
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let pid = child.id() as i32;
        // It is alive (blocked on stdin): read its birth now, while readable.
        let birth = read_birth_identity(pid).expect("a live child has a readable birth");
        let id = ProcessIdentity { pid, birth };
        assert_eq!(
            liveness(&id),
            Liveness::Alive,
            "the child is live before EOF"
        );

        // Close its stdin ⇒ cat sees EOF and exits ⇒ becomes a zombie because we
        // do NOT reap it yet. Bounded wait for the zombie state.
        {
            let mut stdin = child.stdin.take().expect("stdin handle");
            let _ = stdin.write_all(b""); // no-op; the drop below closes it
        }
        let mut verdict = liveness(&id);
        for _ in 0..500 {
            // `kill(0)` still succeeds while it is a zombie (pid occupied), so a
            // correct liveness says Gone *despite* the pid existing.
            if verdict == Liveness::Gone {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            verdict = liveness(&id);
        }
        assert_eq!(
            verdict,
            Liveness::Gone,
            "an unreaped zombie must read Gone, not Alive/Unknown"
        );
        // Reap it now so we don't leak the zombie.
        let _ = child.wait();
    }

    #[test]
    fn boot_identity_is_stable_within_a_run() {
        let a = boot_identity().expect("boot identity readable");
        let b = boot_identity().expect("boot identity readable");
        assert_eq!(a, b, "boot identity does not change under our feet");
    }

    #[test]
    fn the_monotonic_clock_does_not_go_backwards() {
        let a = monotonic_now_nanos().expect("monotonic clock readable");
        let b = monotonic_now_nanos().expect("monotonic clock readable");
        assert!(b >= a, "CLOCK_MONOTONIC is monotonic: {a} then {b}");
    }
}
