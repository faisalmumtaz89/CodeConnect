//! Lives in its own test binary on purpose: it counts the whole process's
//! descriptor table, so any test running beside it in the same process —
//! the tmux tests open sockets and pipes — moves the count and fails it.

use protocol::proc::run_deadlined;
use std::process::Command;
use std::time::Duration;

/// Timeouts must not accumulate resources: after many of them, the
/// process holds no more descriptors than it started with. This is what
/// rules out the parked-thread/leaked-pipe design this module replaced.
#[test]
fn repeated_timeouts_accumulate_no_descriptors() {
    let open_fds = || std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0);
    // One warm-up so lazy allocations settle before the baseline.
    let _ = run_deadlined(
        Command::new("/bin/sleep").arg("30"),
        Duration::from_millis(50),
    );
    let baseline = open_fds();
    for _ in 0..20 {
        let _ = run_deadlined(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(50),
        );
    }
    let after = open_fds();
    assert!(
        after <= baseline + 2,
        "descriptors grew from {baseline} to {after} across 20 timeouts"
    );
}
