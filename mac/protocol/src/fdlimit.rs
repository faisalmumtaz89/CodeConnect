//! The open-file limit CodeConnect's processes raise as they start.
//!
//! macOS hands a launchd job, and everything started from one, a soft
//! `RLIMIT_NOFILE` of 256 under an unlimited hard limit. That soft limit is a
//! fleet size: ccd holds a socket for every live session's supervisor and
//! another for every waiting mail waiter, and the tmux server holds one per
//! pane, so at 256 one computer stops admitting sessions somewhere past a
//! hundred. Both are raised where they start: ccd in its own `main`, and the
//! tmux server through the launcher that forks it — `codeconnect`, for a typed
//! start and a phone start alike. Every child inherits what its parent raised.

/// The soft limit asked for: macOS's `OPEN_MAX`, the most a process may ask for
/// portably. The pseudo-terminals a session takes (`kern.tty.ptmx_max` is 511)
/// run out long before it does.
pub const TARGET: libc::rlim_t = 10_240;

/// Raise the soft open-file limit to [`TARGET`], or to the hard limit when that
/// is lower. A higher soft limit is left as it is. Returns the soft limit before
/// and after.
pub fn raise() -> std::io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes one `rlimit` through a pointer to a live one.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let before = limit.rlim_cur;
    let wanted = TARGET.min(limit.rlim_max);
    if before >= wanted {
        return Ok((before, before));
    }
    limit.rlim_cur = wanted;
    // SAFETY: `setrlimit` reads one `rlimit` through a pointer to a live one.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((before, wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set only in the child [`started_at`] runs: the soft limit it began with.
    const CHILD: &str = "CODECONNECT_FDLIMIT_CHILD";

    fn soft_and_hard() -> (libc::rlim_t, libc::rlim_t) {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: as in `raise`.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        (limit.rlim_cur, limit.rlim_max)
    }

    /// Run [`raise_in_a_child`] in a fresh process whose soft limit starts at
    /// `soft`, the way launchd starts a job at 256. A raise inside this test
    /// process would prove nothing: the runner's limit is whatever started it.
    fn started_at(soft: libc::rlim_t) {
        let (_, hard) = soft_and_hard();
        assert!(
            hard > soft,
            "hard limit {hard}: nothing above {soft} to raise to"
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(r#"ulimit -S -n {soft} && exec "$0" "$@""#))
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "fdlimit::tests::raise_in_a_child", "--nocapture"])
            .env(CHILD, soft.to_string())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn raise_in_a_child() {
        let Some(started) = std::env::var(CHILD).ok() else {
            return;
        };
        let started: libc::rlim_t = started.parse().unwrap();
        let (soft, hard) = soft_and_hard();
        assert_eq!(soft, started, "the child starts at the limit it was given");
        let expected = started.max(TARGET.min(hard));
        assert_eq!(raise().unwrap(), (started, expected));
        assert_eq!(soft_and_hard().0, expected, "the limit the process now has");
    }

    #[test]
    fn a_process_started_at_launchds_256_is_raised() {
        started_at(256);
    }

    #[test]
    fn a_higher_soft_limit_is_left_as_it_is() {
        started_at(TARGET + 1);
    }
}
