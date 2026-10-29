//! Which computer this daemon runs on, as its owner names it.

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
