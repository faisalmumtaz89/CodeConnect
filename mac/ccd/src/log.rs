//! Minimal levelled logging to stderr.
//!
//! launchd captures stderr to `StandardErrorPath`, so a logging crate would buy
//! nothing here beyond dependency surface. `CODECONNECT_LOG=debug` raises the
//! level; the default is `info`.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

const LEVEL_ERROR: u8 = 0;
const LEVEL_WARN: u8 = 1;
const LEVEL_INFO: u8 = 2;
const LEVEL_DEBUG: u8 = 3;

static LEVEL: AtomicU8 = AtomicU8::new(LEVEL_INFO);

pub fn init_from_env() {
    let level = match std::env::var("CODECONNECT_LOG").as_deref() {
        Ok("error") => LEVEL_ERROR,
        Ok("warn") => LEVEL_WARN,
        Ok("debug") | Ok("trace") => LEVEL_DEBUG,
        _ => LEVEL_INFO,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

pub fn emit(tag: &str, message: &str) {
    let line = format!("{} {tag} {message}", protocol::time::now_rfc3339());
    // Test-only mirror. The `#[cfg(test)]` is on the call, the module, the buffer
    // and the installer, so a non-test build compiles the same single `writeln!`
    // to stderr it always has and gains no field, no static and no public item.
    #[cfg(test)]
    capture::record(&line);
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::emit("ERROR", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        if $crate::log::enabled(1) { $crate::log::emit("WARN ", &format!($($arg)*)) }
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::log::enabled(2) { $crate::log::emit("INFO ", &format!($($arg)*)) }
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::log::enabled(3) { $crate::log::emit("DEBUG", &format!($($arg)*)) }
    };
}

/// A test-only mirror of every line [`emit`] writes.
///
/// `emit` writes to `std::io::stderr()`, which the test harness does not capture —
/// it only captures the `print!` family. A gate that has to assert what production
/// *actually logged* (that the STOP-AND-AMEND report carries no frame dump, say)
/// therefore has nothing to read. This gives it the real formatted line, taken at
/// the same point the real one is written, so what is asserted is what was emitted
/// rather than a reconstruction of it.
///
/// The sink is process-global because `emit` is, and the ccd test binary runs its
/// tests on many threads at once: a test that installs it will see lines from
/// whatever else is logging, and should select the line it means by content.
#[cfg(test)]
pub mod capture {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// `None` when nothing is capturing — which is the state a test binary starts
    /// in, so an uninstalled sink costs one relaxed load and no allocation.
    static SINK: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();

    /// The lock, never poisoned into a panic: a test that asserts inside a capture
    /// would otherwise take every other test down with it.
    fn sink() -> MutexGuard<'static, Option<Vec<String>>> {
        SINK.get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start capturing, discarding anything held from before. Idempotent: a second
    /// install is a clear, not an error.
    pub fn install() {
        *sink() = Some(Vec::new());
    }

    /// Take everything captured so far, leaving the sink installed and empty.
    pub fn drain() -> Vec<String> {
        match sink().as_mut() {
            Some(lines) => std::mem::take(lines),
            None => Vec::new(),
        }
    }

    /// Stop capturing and drop whatever is held.
    pub fn uninstall() {
        *sink() = None;
    }

    pub(super) fn record(line: &str) {
        if let Some(lines) = sink().as_mut() {
            lines.push(line.to_string());
        }
    }
}
