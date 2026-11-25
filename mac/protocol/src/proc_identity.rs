//! Kernel-backed process **birth identity** and OS **boot identity** — the
//! primitives process signalling and run cleanup stand on.
//!
//! The governing invariant: CodeConnect never signals by name or by bare
//! pid. Every signal and every liveness check binds to a `(pid, birth_identity)`
//! pair. A pid on its own is a liar the instant the kernel reuses it; the birth identity is
//! what makes reuse observable.
//!
//! ## Darwin field choice
//!
//! The birth identity is the process **start time**, read via
//! `proc_pidinfo(PROC_PIDTBSDINFO)` as `proc_bsdinfo.pbi_start_tvsec` /
//! `pbi_start_tvusec` (seconds + microseconds since the epoch). This is the
//! same `proc_pidinfo` family the descendant census uses. It is chosen
//! because:
//!
//!   * It is assigned by the kernel at fork and is **preserved across `execve`**:
//!     `execve` replaces the image, not the proc struct, so `p_starttime` survives.
//!   * Together with the pid it disambiguates pid reuse at microsecond
//!     resolution, which binding every signal to `(pid, birth)` requires.
//!
//! The boot identity is `KERN_BOOTTIME` (also a `timeval`).
//!
//! ## Linux field choice
//!
//! The birth identity is `starttime` from `/proc/<pid>/stat`: clock ticks
//! since boot, assigned at fork and kept across `execve`, the same two
//! properties the Darwin field is chosen for. The same file gives the process
//! group and the zombie state (`Z`). The boot identity is
//! `/proc/sys/kernel/random/boot_id`, a random id the kernel fixes for one
//! boot; the boot time `/proc/stat` reports would not do, because it is derived
//! from the wall clock and moves when the clock is adjusted.

use serde::{Deserialize, Serialize};

/// A process's kernel-assigned birth time: seconds + microseconds of
/// `p_starttime` since the epoch on macOS, of `starttime` since boot on Linux.
/// Compared for **exact** equality — this is an identity, not a clock reading,
/// so "close" is not "same".
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

/// The OS boot identity. On macOS the two words are the boot wall-clock instant
/// in seconds and microseconds; on Linux they are the two 64-bit halves of
/// `/proc/sys/kernel/random/boot_id`. Either way they are only ever compared
/// for equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootIdentity {
    pub boot_sec: i64,
    pub boot_usec: i64,
}

/// Whether a recorded `(pid, birth)` is still the very process that was
/// recorded. Absence and "cannot tell" are **distinct** — cleanup turns on
/// it: `Unavailable` is never treated as absence.
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
pub fn read_birth_identity(pid: i32) -> Option<BirthIdentity> {
    let info = read_bsdinfo(pid)?;
    Some(BirthIdentity {
        start_sec: info.pbi_start_tvsec as i64,
        start_usec: info.pbi_start_tvusec as i64,
    })
}

/// Read the process group id of `pid` off the same `proc_bsdinfo` answer as its
/// birth identity, so a recorded `(pid, birth, pgid)` triple is internally
/// consistent.
#[cfg(target_os = "macos")]
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
/// Three-valued and **fail-closed in both directions**: only a *proven* absence
/// or a *proven* identity change is `Gone`; a transient inability to read is
/// `Unknown`, never `Gone` (so a caller never tears a session down on a hiccup)
/// and never `Alive` (so a caller never treats an unreadable process as proof of
/// life).
///
///   * `kill(pid, 0)` ⇒ `ESRCH`  → the pid does not exist → **Gone**.
///   * `kill(pid, 0)` ⇒ `0`/`EPERM` → the pid exists; but a **zombie** (an
///     unreaped, dead child — `SZOMB`) is **Gone**, not alive: its pid still
///     occupies the table and `kill(0)` succeeds, yet the process is dead.
///     Otherwise confirm identity by start time: equal → **Alive**; different →
///     reuse → **Gone**; a birth read that *fails while the pid exists* is
///     **Unknown**.
///   * any other `kill` errno → **Unknown**.
///
/// **Not monotonic while a process dies.** Around its exit a dying process can read `Gone`,
/// then `Unknown`, then `Gone` again (seen on macOS as Codex's app-server died), so a caller
/// acts on a `Gone` when it reads it and never reads again to confirm it.
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
    // `Unknown`, so a dead-but-unreaped custodian does not block its replacement.
    // On macOS `proc_pidinfo` *short-reads* a zombie, so the only reliable status
    // source is `sysctl(KERN_PROC_PID)` (`p_stat == SZOMB`).
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

/// Whether `pid` is proven to have exited, whatever process it was: no such pid,
/// or a zombie. A pid that exists and cannot be described is not proven exited.
pub fn exited(pid: i32) -> bool {
    let missing = unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    missing || is_zombie(pid) == Some(true)
}

/// Whether `pid` is a **zombie** (exited, awaiting reap). `Some(true)`/`Some(false)`
/// on a successful status read; `None` when the kernel could not be asked.
///
/// Reads `struct kinfo_proc` via `sysctl(KERN_PROC_PID)` — the `kp_proc.p_stat`
/// byte (offset 36 on 64-bit macOS; `SZOMB == 5`), verified against the live
/// binary. `sysctl` is used rather than `proc_pidinfo` precisely because the
/// latter returns a zero-length answer for a zombie.
#[cfg(target_os = "macos")]
pub fn is_zombie(pid: i32) -> Option<bool> {
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
#[cfg(target_os = "macos")]
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

/// The four `/proc/<pid>/stat` fields needed by process identity and tool-group
/// discovery, parsed directly from a stack buffer with zero heap allocations.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatFields {
    state: u8,
    ppid: i32,
    pgid: i32,
    starttime_ticks: i64,
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> i64 {
    static HZ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| {
        // SAFETY: `sysconf` reads a constant of the running system.
        unsafe { libc::sysconf(libc::_SC_CLK_TCK) as i64 }
    })
}

/// Parse the fields of `/proc/<pid>/stat` after the command name — the one field
/// that may itself hold spaces and parentheses, so the split is at the last
/// `)`. Counted from there, `state` is `[0]`, `ppid` `[1]`, `pgid` `[2]`, and
/// `starttime` `[19]`. `None` when the process does not exist or cannot be parsed.
#[cfg(target_os = "linux")]
fn parse_stat_line(stat: &[u8]) -> Option<StatFields> {
    let close = stat.windows(2).rposition(|w| w == b") ")?;
    let rest = &stat[close + 2..];
    let mut state: Option<u8> = None;
    let mut ppid: Option<i32> = None;
    let mut pgid: Option<i32> = None;
    let mut starttime_ticks: Option<i64> = None;

    let mut idx = 0usize;
    let mut i = 0usize;
    let len = rest.len();
    while i < len {
        while i < len && rest[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        let start = i;
        while i < len && !rest[i].is_ascii_whitespace() {
            i += 1;
        }
        let tok = &rest[start..i];
        match idx {
            0 => state = tok.first().copied(),
            1 => ppid = std::str::from_utf8(tok).ok()?.parse().ok(),
            2 => pgid = std::str::from_utf8(tok).ok()?.parse().ok(),
            19 => {
                starttime_ticks = std::str::from_utf8(tok).ok()?.parse().ok();
                break;
            }
            _ => {}
        }
        idx += 1;
    }

    Some(StatFields {
        state: state?,
        ppid: ppid?,
        pgid: pgid?,
        starttime_ticks: starttime_ticks?,
    })
}

/// Read and parse `/proc/<pid>/stat` using stack buffers only (zero heap allocations).
#[cfg(target_os = "linux")]
fn read_stat(pid: i32) -> Option<StatFields> {
    use std::io::{Read, Write};
    if pid <= 0 {
        return None;
    }
    let mut path_buf = [0u8; 32];
    let path_len = {
        let mut cursor = std::io::Cursor::new(&mut path_buf[..]);
        write!(cursor, "/proc/{pid}/stat").ok()?;
        cursor.position() as usize
    };
    let path = std::str::from_utf8(&path_buf[..path_len]).ok()?;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).ok()?;
        if n == 0 {
            break;
        }
        total += n;
    }
    parse_stat_line(&buf[..total])
}

/// Read the birth identity of `pid`: its `starttime`, in clock ticks since
/// boot, as seconds and microseconds. `None` when the process does not exist.
#[cfg(target_os = "linux")]
pub fn read_birth_identity(pid: i32) -> Option<BirthIdentity> {
    stat_birth(read_stat(pid)?)
}

/// The birth identity in a `/proc/<pid>/stat` line's fields ([`read_stat`]).
#[cfg(target_os = "linux")]
fn stat_birth(fields: StatFields) -> Option<BirthIdentity> {
    let ticks = fields.starttime_ticks;
    let hz = clock_ticks_per_sec();
    if hz <= 0 {
        return None;
    }
    Some(BirthIdentity {
        start_sec: ticks / hz,
        start_usec: ticks % hz * 1_000_000 / hz,
    })
}

/// Read the process group id of `pid` off the same `/proc/<pid>/stat` line
/// its birth identity comes from.
#[cfg(target_os = "linux")]
pub fn read_pgid(pid: i32) -> Option<i32> {
    Some(read_stat(pid)?.pgid)
}

/// Whether `pid` is a **zombie** (exited, awaiting reap), from its `state`:
/// `Z`, or `X` in the instant it is being reaped. `None` when it cannot be read.
#[cfg(target_os = "linux")]
pub fn is_zombie(pid: i32) -> Option<bool> {
    Some(matches!(read_stat(pid)?.state, b'Z' | b'X'))
}

/// The OS boot identity: the kernel's random `boot_id`, fixed for one boot,
/// as its two 64-bit halves.
#[cfg(target_os = "linux")]
pub fn boot_identity() -> Option<BootIdentity> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let hex: String = id.trim().chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let id = u128::from_str_radix(&hex, 16).ok()?;
    Some(BootIdentity {
        boot_sec: (id >> 64) as u64 as i64,
        boot_usec: id as u64 as i64,
    })
}

/// Whether a process group still has any member this process could signal.
///
/// `kill(-pgid, 0)` sends nothing; it only asks. `ESRCH` is the proof of
/// extinction. Any other error means the question could not be answered, which is
/// reported as "still has members" — the fail-closed reading, since the caller
/// uses this to decide whether cleanup fell short.
pub fn group_has_members(pgid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing and only probes for the group's existence.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Whether a recorded pgid may still be signalled as a group (the pgid-reuse guard).
#[derive(Debug, PartialEq, Eq)]
pub enum GroupWarrant {
    Warranted,
    Refused(String),
}

/// Bind a process group to the identity that was recorded as leading it.
///
/// The reasoning, which rests on one kernel rule: a process group id can only come
/// into existence in a process whose **pid equals that id** (`setpgid(0, 0)`). So
/// the only way the recorded number can name a group that is not ours is if the
/// recorded leader's pid was freed and handed to somebody else who then led a group
/// with it. Deciding whether that happened is therefore a question about the PID,
/// and it is answerable:
///
///   * the pid is **unoccupied** — nothing can currently be leading a fresh group
///     with that id, so whatever is still in the group inherited it from our leader.
///     This is the case that actually occurs: the app-server is reaped and the
///     descendants it forked keep the group alive (measured);
///   * the pid is still **our recorded process** — the identity matches outright;
///   * anything else — a live pid with a different birth stamp, or one whose birth
///     cannot be read — is the ABA case, or indistinguishable from it. Refused.
///
/// **Residual, stated exactly, and it is bigger than this used to claim.**
///
/// The first part is the gap between the answer and the signal that follows it:
/// Darwin has no atomic check-and-signal for a process group, so that window is
/// narrowed to an instant rather than closed.
///
/// The second part is the **unoccupied-leader arm itself, which is an unbounded
/// ABA** — this doc used to say the opposite, and that was wrong. "Nothing can
/// currently be leading a fresh group with that id" is true and does not imply what
/// the arm concludes from it. Counterexample: our group empties; the pid is reused
/// by a new process that leads a group with it; that process forks and exits; its
/// unrelated descendants keep the group alive while the pid is unoccupied *again*.
/// The arm then SIGKILLs a group that was never ours, and the age of the record
/// does not bound it.
///
/// Measured (`KERN_PROC_PGRP` on the constructed state): the surviving members'
/// only distinguishing fields are pid, ppid and start time. Every member has
/// reparented to `launchd`, so ppid says nothing; and members of a recycled group
/// start *after* the recycled leader's birth, which is after our leader's death,
/// which is after our leader's birth — so start time does not separate them either.
/// Nothing Darwin exposes today tells our own descendants from a stranger's.
///
/// The arm is nevertheless KEPT, deliberately, and the alternative is worse: the
/// host reaps its children by pid (`Child::start_kill`), so the app-server's
/// surviving descendants are exactly what this arm reaches on the ordinary teardown
/// path. Refusing here would trade a narrow within-boot risk for a guaranteed leak of
/// codex subprocesses on every session that ends normally — the leak this arm
/// exists to close.
///
/// **The worst arm of that residual is closed rather than recorded.** The residual
/// above is a within-boot one, and the sentences describing it used to say the
/// unrelated-group risk "requires pid wraparound". Across a REBOOT it
/// requires nothing at all: the recorded pgid is a number from a boot that no longer
/// exists, the new boot allocates pids from the bottom and reaches that number in the
/// ordinary course of starting up, and if the new occupant has exited while its
/// children live, the unoccupied-leader arm authorizes `SIGKILL` on a process group
/// belonging to a machine-lifetime this launch never touched. Nothing about the
/// recorded identity distinguishes it, because every field in it — pid, birth,
/// pgid — is scoped to the boot it was written under.
///
/// So the warrant now takes the record's [`BootIdentity`] and refuses outright when
/// the current boot differs. This costs nothing real: no process recorded before a
/// reboot can still be running after it, so there is never anything of ours on the
/// other side of that comparison to reach. It is the one place in this residual where
/// fail-closed is free. An UNREADABLE current boot also refuses — the same rule
/// `boot_changed` uses in the other direction is inverted here on purpose, because
/// this side is authorizing a `SIGKILL` rather than declining to conclude an absence.
///
/// **What would close it**, and why it is a change of its own rather than a line
/// here: provenance the members carry themselves. The host would stamp the
/// app-server's environment with this launch's nonce, every descendant would inherit
/// it, and the warrant would enumerate `KERN_PROC_PGRP` and require every member to
/// present it (via `KERN_PROCARGS2`, which is readable for same-uid processes). That
/// is new kernel-introspection machinery with its own measurement and failure modes,
/// and until it exists this arm signals on a warrant it cannot corroborate.
///
/// A second, smaller residual: a same-uid process in the same session may join an
/// existing group deliberately with `setpgid(self, pgid)`. Such a process would be
/// killed by the group signal. It is not reachable by accident — it requires
/// guessing the pgid and choosing to join it — and no privilege boundary is crossed.
pub fn group_warrant(identity: &ProcessIdentity, recorded_boot: BootIdentity) -> GroupWarrant {
    // The boot check. Asked FIRST, before the pid is even probed: across a boot
    // change every field of `identity` names a machine-lifetime that is over, so no
    // answer the kernel gives about that pid can be about our process.
    match boot_identity() {
        Some(now) if now == recorded_boot => {}
        Some(_) => {
            return GroupWarrant::Refused(
                "the machine has rebooted since this launch was recorded, so the recorded \
                 pgid names a process group from a boot that no longer exists"
                    .to_string(),
            )
        }
        None => {
            return GroupWarrant::Refused(
                "the current boot identity could not be read, so the recorded pgid cannot \
                 be proven to belong to this boot at all"
                    .to_string(),
            )
        }
    }
    let pid = identity.pid;
    // SAFETY: signal 0 delivers nothing; it only asks whether the pid is occupied.
    if unsafe { libc::kill(pid, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            // Unoccupied: no live leader. This is the UNCORROBORATED arm — see the
            // residual above. It is warranted because the alternative leaks, not
            // because the members are proven ours.
            Some(libc::ESRCH) => GroupWarrant::Warranted,
            // Occupied by a process we may not signal — fall through to the birth
            // compare, which is what decides whether it is still ours.
            Some(libc::EPERM) => birth_matches(identity),
            _ => GroupWarrant::Refused(format!("pid {pid} could not be probed: {err}")),
        };
    }
    birth_matches(identity)
}

fn birth_matches(identity: &ProcessIdentity) -> GroupWarrant {
    let pid = identity.pid;
    match read_birth_identity(pid) {
        Some(birth) if birth == identity.birth => GroupWarrant::Warranted,
        Some(_) => GroupWarrant::Refused(format!(
            "pid {pid} is now a different process, so the group id may have been recycled"
        )),
        // A pid that exists but cannot be described is not evidence of anything.
        None => GroupWarrant::Refused(format!(
            "pid {pid} exists but its birth identity could not be read"
        )),
    }
}

/// One process-table entry: its pid, parent, process group and birth identity.
struct Entry {
    pid: i32,
    ppid: i32,
    pgid: i32,
    birth: BirthIdentity,
}

/// One snapshot of the system's process table, reusable across multiple
/// [`ProcessTable::tool_groups`] queries without re-scanning `/proc` or
/// `proc_listallpids` per session.
pub struct ProcessTable(Vec<Entry>);

impl ProcessTable {
    pub fn snapshot() -> Self {
        Self(process_table())
    }

    pub fn tool_groups(&self, group: i32) -> Vec<ProcessIdentity> {
        led_by_members(group, &self.0)
    }
}

/// The groups a process group's members started and that lead themselves: each process whose parent is a member of
/// `group` and whose own group id is its pid, other than `group` itself. This is how Codex runs a command, a shell
/// snapshot or an MCP server: in a group of its own, as a child of the app-server, which B3's ladder on the
/// app-server's group never reaches. Read from one scan of the process table; a process that cannot be described is
/// left out.
pub fn tool_groups(group: i32) -> Vec<ProcessIdentity> {
    led_by_members(group, &process_table())
}

fn led_by_members(group: i32, table: &[Entry]) -> Vec<ProcessIdentity> {
    // A hosted run's process group typically holds 1–4 processes, so a tiny Vec
    // avoids HashSet hashing/allocation overhead completely.
    let mut members: Vec<i32> = Vec::new();
    for entry in table {
        if entry.pgid == group {
            members.push(entry.pid);
        }
    }
    if members.is_empty() {
        return Vec::new();
    }
    table
        .iter()
        .filter(|entry| {
            entry.pgid == entry.pid && entry.pid != group && members.contains(&entry.ppid)
        })
        .map(|entry| ProcessIdentity {
            pid: entry.pid,
            birth: entry.birth,
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn process_table() -> Vec<Entry> {
    // SAFETY: with no buffer, proc_listallpids only reports how many pids there are.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started between the two calls.
    let mut pids: Vec<libc::pid_t> = vec![0; count as usize + 64];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: the buffer holds `size` bytes, and proc_listallpids writes at most that, returning how many pids.
    let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, size) };
    pids.truncate(listed.max(0) as usize);
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            let info = read_bsdinfo(pid)?;
            Some(Entry {
                pid,
                ppid: info.pbi_ppid as i32,
                pgid: info.pbi_pgid as i32,
                birth: BirthIdentity {
                    start_sec: info.pbi_start_tvsec as i64,
                    start_usec: info.pbi_start_tvusec as i64,
                },
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn process_table() -> Vec<Entry> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(128);
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Some(fields) = read_stat(pid) else {
            continue;
        };
        let Some(birth) = stat_birth(fields) else {
            continue;
        };
        out.push(Entry {
            pid,
            ppid: fields.ppid,
            pgid: fields.pgid,
            birth,
        });
    }
    out
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
        // Spawn, let it exit, reap it, then assert Gone.
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

    /// A pid with nothing behind it — a running process is not; an exited child is,
    /// whether or not it has been reaped.
    #[test]
    fn exited_is_true_only_once_the_process_is_dead() {
        use std::process::{Command, Stdio};
        assert!(
            !exited(std::process::id() as i32),
            "this process is running"
        );
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let pid = child.id() as i32;
        assert!(!exited(pid), "a child blocked on its input is running");
        drop(child.stdin.take());
        let mut zombie = exited(pid);
        for _ in 0..500 {
            if zombie {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            zombie = exited(pid);
        }
        assert!(zombie, "an unreaped zombie has exited");
        child.wait().expect("reap");
        assert!(exited(pid), "a reaped child has exited");
    }

    #[test]
    fn an_unreaped_zombie_reads_as_gone_not_alive() {
        // An exited-but-unreaped child is a zombie — its pid still
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

    /// **X8-2: the groups a run's members started and lead themselves.** The run's group is 100: its leader, and a
    /// member under it (the npm launcher's app-server). Listed: a group led by a child of the leader (the native
    /// app-server's command) and one led by a child of the other member. Not listed: a process in the run's own group,
    /// a group led by a grandchild under a tool, and an unrelated group.
    #[test]
    fn the_groups_led_by_children_of_a_groups_members_are_listed() {
        let birth = BirthIdentity {
            start_sec: 1,
            start_usec: 2,
        };
        let entry = |pid, ppid, pgid| Entry {
            pid,
            ppid,
            pgid,
            birth,
        };
        let table = [
            entry(600, 200, 600),
            entry(100, 1, 100),
            entry(101, 100, 100),
            entry(200, 100, 200),
            entry(300, 101, 300),
            entry(400, 100, 100),
            entry(500, 1, 500),
        ];
        let mut listed: Vec<i32> = led_by_members(100, &table)
            .iter()
            .map(|id| id.pid)
            .collect();
        listed.sort();
        assert_eq!(listed, vec![200, 300]);
    }

    /// **X8-2, live:** a child of this test that leads its own group is listed under this test's group, with its birth
    /// identity, and a child left in this test's group is not.
    #[test]
    fn a_child_leading_its_own_group_is_found_in_the_process_table() {
        use std::os::unix::process::CommandExt;
        let own_group = read_pgid(std::process::id() as i32).expect("this test's group");
        let mut leading = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut staying = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let listed = tool_groups(own_group);
        let leading_id = ProcessIdentity {
            pid: leading.id() as i32,
            birth: read_birth_identity(leading.id() as i32).unwrap(),
        };
        let staying_pid = staying.id() as i32;
        let _ = leading.kill();
        let _ = staying.kill();
        let _ = leading.wait();
        let _ = staying.wait();
        assert!(
            listed.contains(&leading_id),
            "{listed:?} lacks {leading_id:?}"
        );
        assert!(listed.iter().all(|id| id.pid != staying_pid), "{listed:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_line_handles_spaces_and_parens_in_comm() {
        // Real Linux /proc/<pid>/stat shape where comm itself contains spaces and ") ".
        let line = b"4242 (my (weird) proc) Z 100 200 300 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 987654 12345 100\n";
        let fields = parse_stat_line(line).expect("parsed stat fields");
        assert_eq!(fields.state, b'Z');
        assert_eq!(fields.ppid, 100);
        assert_eq!(fields.pgid, 200);
        assert_eq!(fields.starttime_ticks, 987654);
    }
}
