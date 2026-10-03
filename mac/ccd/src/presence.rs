//! Whether someone is at the Mac, for a background agent's question.
//!
//! Claude draws a background agent's question only after its
//! `PermissionRequest` hook returns, so while the daemon holds it for the phone
//! the Mac shows nothing. It is therefore held only while nobody is at the Mac:
//! "at the Mac" is a viewer of that session in the front tab **and** keyboard or
//! mouse input in the last [`IDLE_THRESHOLD`]. The viewers report the first
//! (focus reporting); this module reads the second.

use std::time::Duration;

/// How recent the last keyboard or mouse input must be to count as someone at
/// the Mac.
pub const IDLE_THRESHOLD: Duration = Duration::from_secs(10);

/// How often a held background question re-checks presence.
pub const POLL: Duration = Duration::from_millis(100);

/// Every this many polls (a second), the tmux clients of a session holding a
/// question are listed again, for one that attached since.
pub const CLIENT_CHECK_TICKS: u32 = 10;

/// Time since the last keyboard or mouse input on this Mac, as the HID system
/// counts it (`IOHIDSystem`'s `HIDIdleTime`). Needs no permission and prompts
/// for none. `None` when it cannot be read, which callers treat as "someone
/// may be here": a question then goes to the Mac rather than being hidden.
pub fn hid_idle() -> Option<Duration> {
    use std::ffi::{c_char, c_void};
    type CFTypeRef = *const c_void;
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
        fn IORegistryEntryCreateCFProperty(
            entry: u32,
            key: CFTypeRef,
            allocator: CFTypeRef,
            options: u32,
        ) -> CFTypeRef;
        fn IOObjectRelease(object: u32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            allocator: CFTypeRef,
            text: *const c_char,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFNumberGetValue(number: CFTypeRef, kind: i64, value: *mut c_void) -> bool;
        fn CFGetTypeID(value: CFTypeRef) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFRelease(value: CFTypeRef);
    }
    const UTF8: u32 = 0x0800_0100;
    const SINT64: i64 = 4;
    // SAFETY: each Create/Get is paired with its Release, every pointer is
    // checked before use, and the number is read into an i64 of the size asked.
    unsafe {
        let service = IOServiceGetMatchingService(0, IOServiceMatching(c"IOHIDSystem".as_ptr()));
        if service == 0 {
            return None;
        }
        let key = CFStringCreateWithCString(std::ptr::null(), c"HIDIdleTime".as_ptr(), UTF8);
        let value = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
        CFRelease(key);
        IOObjectRelease(service);
        if value.is_null() {
            return None;
        }
        let mut nanos: i64 = -1;
        let read = CFGetTypeID(value) == CFNumberGetTypeID()
            && CFNumberGetValue(value, SINT64, (&mut nanos as *mut i64).cast());
        CFRelease(value);
        (read && nanos >= 0).then(|| Duration::from_nanos(nanos as u64))
    }
}

/// Someone is at the Mac: a viewer of the session is in the front tab and the
/// Mac has had input within the threshold — or its idle time cannot be read,
/// in which case nothing is hidden from it.
pub fn at_the_mac(focused_viewer: bool, idle: Option<Duration>) -> bool {
    focused_viewer && idle.is_none_or(|idle| idle < IDLE_THRESHOLD)
}

/// True when this daemon started `pid` — one of its own tmux clients, the
/// phone's terminal — rather than a person's.
pub fn ours(pid: i32) -> bool {
    protocol::proc_identity::read_ppid(pid) == Some(std::process::id() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off the real HID system, so this is the only test that proves the
    /// FFI is wired: a broken read returns `None`, which `at_the_mac` would
    /// quietly turn into "someone is here" everywhere.
    #[test]
    fn the_mac_s_idle_time_reads_without_any_permission() {
        let idle = hid_idle().expect("HIDIdleTime must read");
        assert!(idle < Duration::from_secs(365 * 24 * 3600), "{idle:?}");
        let again = hid_idle().unwrap();
        assert_ne!(idle, again, "the value is computed at read time");
    }

    /// The presence rule as a table: both halves are needed, and an idle time
    /// that cannot be read counts as someone there.
    #[test]
    fn someone_is_at_the_mac_only_in_the_front_tab_with_recent_input() {
        let recent = Some(Duration::from_secs(2));
        let stale = Some(IDLE_THRESHOLD);
        assert!(at_the_mac(true, recent));
        assert!(at_the_mac(true, None));
        assert!(!at_the_mac(true, stale));
        assert!(!at_the_mac(true, Some(Duration::from_secs(600))));
        assert!(!at_the_mac(false, recent));
        assert!(!at_the_mac(false, None));
    }

    #[test]
    fn a_child_of_this_process_is_ours_and_this_process_is_not() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert!(ours(child.id() as i32));
        assert!(!ours(std::process::id() as i32));
        let _ = child.kill();
        let _ = child.wait();
    }
}
