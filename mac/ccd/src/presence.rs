//! Whether someone is at the Mac: keyboard or mouse input anywhere on it
//! within a time each caller chooses.
//!
//! Two callers, one check:
//!
//!   * **A background agent's question.** Claude draws it only after its
//!     `PermissionRequest` hook returns, so while the daemon holds it for the
//!     phone the Mac shows nothing. It is held only while nobody has touched
//!     the Mac for [`QUESTION_IDLE`], and input resuming hands it back.
//!   * **A ring.** The phone is for when the owner is away: a ring raised while
//!     someone has used the Mac within [`RING_IDLE`] waits, and rings once the
//!     Mac has been left that long.
//!
//! A Linux machine has no input-idle reading here, so it always counts as
//! left: its questions and its rings go to the phone straight away.

use std::time::Duration;

/// How recent input must be for a background agent's question to stay at the
/// Mac.
pub const QUESTION_IDLE: Duration = Duration::from_secs(10);

/// How long the Mac must be left before a ring reaches the phone.
pub const RING_IDLE: Duration = Duration::from_secs(120);

/// How often a held question or a held ring checks the Mac again.
pub const POLL: Duration = Duration::from_millis(100);

/// Time since the last keyboard or mouse input on this Mac, as the HID system
/// counts it (`IOHIDSystem`'s `HIDIdleTime`). Needs no permission and prompts
/// for none. `None` when it cannot be read, and each caller reads that so that
/// nothing is hidden: a background question counts someone as here and goes to
/// the Mac; a ring counts nobody as here and goes to the phone.
#[cfg(target_os = "macos")]
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

/// Whether the Mac has had keyboard or mouse input within `within`; `None`
/// when its idle time cannot be read. What `None` means is the caller's, and
/// both read it so that nothing is hidden: a question goes to the Mac, a ring
/// goes to the phone.
pub fn at_the_mac(within: Duration) -> Option<bool> {
    used_within(idle(), within)
}

fn used_within(idle: Option<Duration>, within: Duration) -> Option<bool> {
    idle.map(|idle| idle < within)
}

#[cfg(not(test))]
fn idle() -> Option<Duration> {
    machine_idle()
}

#[cfg(target_os = "macos")]
fn machine_idle() -> Option<Duration> {
    hid_idle()
}

/// Longer than any caller's time, so a Linux machine is never someone there.
#[cfg(target_os = "linux")]
fn machine_idle() -> Option<Duration> {
    Some(Duration::MAX)
}

/// In tests the Mac's idle time is the test's, set per thread with
/// [`set_idle`]; unset, it cannot be read, so no test hides a question or
/// holds a ring by accident of how long this machine has been left alone.
#[cfg(test)]
fn idle() -> Option<Duration> {
    IDLE.with(|idle| idle.get())
}

#[cfg(test)]
thread_local! {
    static IDLE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub fn set_idle(idle: Option<Duration>) {
    IDLE.with(|cell| cell.set(idle));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off the real HID system, so this is the only test that proves the
    /// FFI is wired: a broken read returns `None`, which `at_the_mac` would
    /// quietly turn into "someone is here" everywhere.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_s_idle_time_reads_without_any_permission() {
        let idle = machine_idle().expect("HIDIdleTime must read");
        assert!(idle < Duration::from_secs(365 * 24 * 3600), "{idle:?}");
        let again = machine_idle().unwrap();
        assert_ne!(idle, again, "the value is computed at read time");
    }

    /// A Linux machine is away for both callers, so a background question and
    /// a ring both go to the phone at once.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_machine_always_counts_as_left() {
        assert_eq!(used_within(machine_idle(), QUESTION_IDLE), Some(false));
        assert_eq!(used_within(machine_idle(), RING_IDLE), Some(false));
    }

    /// The presence rule as a table: input more recent than the caller's time
    /// is someone there, and an idle time that cannot be read is no answer.
    #[test]
    fn someone_is_at_the_mac_only_with_input_within_the_callers_time() {
        let minute = Duration::from_secs(60);
        assert_eq!(
            used_within(Some(Duration::from_secs(2)), minute),
            Some(true)
        );
        assert_eq!(
            used_within(Some(minute - Duration::from_millis(1)), minute),
            Some(true)
        );
        assert_eq!(used_within(Some(minute), minute), Some(false));
        assert_eq!(
            used_within(Some(Duration::from_secs(600)), minute),
            Some(false)
        );
        assert_eq!(used_within(None, minute), None);
    }

    /// The owner's two times.
    #[test]
    fn a_question_waits_ten_seconds_and_a_ring_two_minutes() {
        assert_eq!(QUESTION_IDLE, Duration::from_secs(10));
        assert_eq!(RING_IDLE, Duration::from_secs(120));
    }
}
