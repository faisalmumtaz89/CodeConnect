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
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{} {tag} {message}", protocol::time::now_rfc3339());
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
