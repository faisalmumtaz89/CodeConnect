//! Which computer this daemon runs on, as its owner names it, and how loaded it
//! is.

use std::path::Path;
use std::time::Duration;

/// The Mac's computer name — what `scutil --get ComputerName` prints. Read each
/// time it is asked for, so a rename reaches a phone on its next connect. `None`
/// when it cannot be read or is empty; the phone then names the machine by the
/// host it paired with.
#[cfg(target_os = "macos")]
pub fn computer_name() -> Option<String> {
    use std::ffi::{c_char, c_void};
    type CFTypeRef = *const c_void;
    #[link(name = "SystemConfiguration", kind = "framework")]
    extern "C" {
        fn SCDynamicStoreCopyComputerName(store: CFTypeRef, encoding: *mut u32) -> CFTypeRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetLength(text: CFTypeRef) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            text: CFTypeRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> bool;
        fn CFRelease(value: CFTypeRef);
    }
    const UTF8: u32 = 0x0800_0100;
    // SAFETY: the copied string is checked before use and released once; the
    // buffer is sized for the string's longest UTF-8 form plus its terminator,
    // and is read only up to the terminator CFStringGetCString wrote.
    unsafe {
        let name = SCDynamicStoreCopyComputerName(std::ptr::null(), std::ptr::null_mut());
        if name.is_null() {
            return None;
        }
        let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(name), UTF8) + 1;
        let mut buffer = vec![0u8; size as usize];
        let read = CFStringGetCString(name, buffer.as_mut_ptr().cast(), size, UTF8);
        CFRelease(name);
        if !read {
            return None;
        }
        let name = std::ffi::CStr::from_bytes_until_nul(&buffer)
            .ok()?
            .to_str()
            .ok()?;
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// The Linux hostname: the kernel's own name for this machine, the one
/// `hostname` prints — not `/etc/hostname`, which says what it was set to at
/// boot. Read each time, as on macOS; `None` when it cannot be read or is empty.
#[cfg(target_os = "linux")]
pub fn computer_name() -> Option<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let name = name.trim_end_matches('\n');
    (!name.is_empty()).then(|| name.to_string())
}

/// One sample of this computer's load, as `machine_stats` carries it. Each value
/// is `None` when it could not be read.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Stats {
    /// The share of all cores busy over [`CPU_WINDOW`], from 0 to 1.
    pub cpu: Option<f64>,
    /// Bytes not available for new work: total less available.
    pub memory_used: Option<u64>,
    pub memory_total: Option<u64>,
    /// Bytes in use on the volume holding `home`: its size less what this user
    /// can still write.
    pub disk_used: Option<u64>,
    pub disk_total: Option<u64>,
}

/// How long the CPU is watched for one sample.
pub const CPU_WINDOW: Duration = Duration::from_millis(500);

/// Take one sample. Waits [`CPU_WINDOW`] between two readings of the CPU
/// counters, asleep, holding nothing.
pub async fn stats(home: Option<&Path>) -> Stats {
    let before = cpu_ticks();
    tokio::time::sleep(CPU_WINDOW).await;
    let cpu = busy_share(before, cpu_ticks());
    let (memory_used, memory_total) = memory().unzip();
    let (disk_used, disk_total) = home.and_then(disk).unzip();
    Stats {
        cpu,
        memory_used,
        memory_total,
        disk_used,
        disk_total,
    }
}

/// The CPU counters since boot, all cores together: idle time and all time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CpuTicks {
    idle: u64,
    total: u64,
}

fn busy_share(before: Option<CpuTicks>, after: Option<CpuTicks>) -> Option<f64> {
    let (before, after) = (before?, after?);
    let total = after.total.checked_sub(before.total)?;
    let idle = after.idle.checked_sub(before.idle)?.min(total);
    (total > 0).then(|| (total - idle) as f64 / total as f64)
}

/// `/proc/stat`'s `cpu` line: user, nice, system, idle, iowait, irq, softirq,
/// steal, then guest time already counted in user and nice. Idle is idle plus
/// iowait.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(text: &str) -> Option<CpuTicks> {
    let fields: Vec<u64> = text
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .map(|field| field.parse().ok())
        .collect::<Option<_>>()?;
    Some(CpuTicks {
        idle: fields.get(3)? + fields.get(4).copied().unwrap_or(0),
        total: fields.iter().take(8).sum(),
    })
}

/// `/proc/meminfo`'s MemTotal and MemAvailable, as (used, total) bytes.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let kib = |key: &str| {
        text.lines().find_map(|line| {
            line.strip_prefix(key)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    let total = kib("MemTotal:")? * 1024;
    let available = kib("MemAvailable:")? * 1024;
    Some((total.saturating_sub(available), total))
}

#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<CpuTicks> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(target_os = "linux")]
fn memory() -> Option<(u64, u64)> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// (used, total) bytes of the filesystem holding `path`.
#[cfg(target_os = "linux")]
fn disk(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut fs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `fs` is a properly sized out-parameter.
    if unsafe { libc::statvfs(path.as_ptr(), &mut fs) } != 0 {
        return None;
    }
    let block = fs.f_frsize as u64;
    let total = fs.f_blocks as u64 * block;
    Some((total.saturating_sub(fs.f_bavail as u64 * block), total))
}

#[cfg(target_os = "macos")]
extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn mach_host_self() -> libc::mach_port_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

/// The host port, released after `read` has used it.
#[cfg(target_os = "macos")]
fn with_host<T>(read: impl FnOnce(libc::mach_port_t) -> Option<T>) -> Option<T> {
    // SAFETY: plain Mach calls; the send right `mach_host_self` hands out is
    // released once `read` returns.
    unsafe {
        let host = mach_host_self();
        let value = read(host);
        mach_port_deallocate(mach_task_self_, host);
        value
    }
}

/// Every core's counters, summed. `host_processor_info` rather than
/// `host_statistics(HOST_CPU_LOAD_INFO)`: the kernel rate-limits the latter per
/// process and answers a call past the limit with the previous answer, so two
/// samples close together read no time passing at all.
#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<CpuTicks> {
    with_host(|host| {
        let mut cpus: libc::natural_t = 0;
        let mut info: libc::processor_info_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        // SAFETY: the kernel allocates `info`, `count` words long, holding
        // CPU_STATE_MAX counters per core; it is read within those bounds and
        // given back with `vm_deallocate`.
        unsafe {
            if libc::host_processor_info(
                host,
                libc::PROCESSOR_CPU_LOAD_INFO,
                &mut cpus,
                &mut info,
                &mut count,
            ) != libc::KERN_SUCCESS
            {
                return None;
            }
            let words = std::slice::from_raw_parts(info, count as usize);
            let mut ticks = CpuTicks { idle: 0, total: 0 };
            for core in words.chunks_exact(libc::CPU_STATE_MAX as usize) {
                ticks.idle += core[libc::CPU_STATE_IDLE as usize] as u32 as u64;
                ticks.total += core.iter().map(|&n| n as u32 as u64).sum::<u64>();
            }
            libc::vm_deallocate(
                mach_task_self_,
                info as libc::vm_address_t,
                count as libc::vm_size_t * std::mem::size_of::<libc::integer_t>(),
            );
            Some(ticks)
        }
    })
}

/// Activity Monitor's Memory Used (app memory, wired and compressed) and the
/// physical memory, in bytes.
#[cfg(target_os = "macos")]
fn memory() -> Option<(u64, u64)> {
    let mut total: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer and `total` is one.
    let read = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    // SAFETY: no arguments; it answers -1 when it cannot.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let pages = with_host(|host| {
        let mut info: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `info` is the struct HOST_VM_INFO64 fills, and `count` says
        // how many words it holds.
        let status = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&mut info as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        };
        (status == libc::KERN_SUCCESS).then(|| {
            (info.internal_page_count as u64).saturating_sub(info.purgeable_count as u64)
                + info.wire_count as u64
                + info.compressor_page_count as u64
        })
    })?;
    Some(((pages * page).min(total), total))
}

/// (used, total) bytes of the volume holding `path`. `statfs` rather than
/// `statvfs`, whose block counts are 32 bits on macOS.
#[cfg(target_os = "macos")]
fn disk(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `fs` is a properly sized out-parameter.
    if unsafe { libc::statfs(path.as_ptr(), &mut fs) } != 0 {
        return None;
    }
    let block = fs.f_bsize as u64;
    let total = fs.f_blocks * block;
    Some((total.saturating_sub(fs.f_bavail * block), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_counts_iowait_as_idle_and_guest_time_once() {
        let text = "cpu  100 20 30 400 50 6 7 8 90 10\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
        assert_eq!(
            parse_proc_stat(text),
            Some(CpuTicks {
                idle: 450,
                total: 621
            })
        );
        assert_eq!(parse_proc_stat("intr 1 2 3\n"), None);
        assert_eq!(parse_proc_stat("cpu  1 2 x 4\n"), None);
    }

    #[test]
    fn meminfo_used_is_total_less_available() {
        let text = "MemTotal:       16303428 kB\nMemFree:          500000 kB\n\
                    MemAvailable:   12000000 kB\nBuffers:            1 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some(((16_303_428 - 12_000_000) * 1024, 16_303_428 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 16 kB\n"), None, "no MemAvailable");
    }

    #[test]
    fn the_busy_share_is_the_non_idle_part_of_the_window() {
        let at = |idle, total| Some(CpuTicks { idle, total });
        assert_eq!(busy_share(at(100, 400), at(160, 600)), Some(0.7));
        assert_eq!(
            busy_share(at(100, 400), at(100, 400)),
            None,
            "no time passed"
        );
        assert_eq!(
            busy_share(at(100, 400), at(90, 500)),
            None,
            "a counter went back"
        );
        assert_eq!(busy_share(None, at(1, 2)), None);
    }

    /// One real sample on this computer: every value read, and in range.
    #[tokio::test]
    async fn a_sample_of_this_computer_is_complete_and_in_range() {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let sample = stats(home.as_deref()).await;
        let cpu = sample.cpu.expect("cpu");
        assert!((0.0..=1.0).contains(&cpu), "{sample:?}");
        let (used, total) = (sample.memory_used.unwrap(), sample.memory_total.unwrap());
        assert!(total > 0 && used > 0 && used <= total, "{sample:?}");
        let (used, total) = (sample.disk_used.unwrap(), sample.disk_total.unwrap());
        assert!(total > 0 && used > 0 && used <= total, "{sample:?}");
    }
}
