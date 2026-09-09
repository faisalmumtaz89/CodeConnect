//! **The lock that gives a freeze an owner, across two real processes.**
//!
//! The immutable bit is one bit on one vnode. It cannot say who set it, and two
//! launches freezing the same `codex` set the identical bit — so ownership has to
//! come from a rule about *when* the bit and the records describing it may be looked
//! at together. That rule is one advisory `flock(LOCK_EX)` on the executable's own
//! descriptor: every freezer holds it across **freeze + record**, and the custodian
//! holds it across **scan + clear**, so those two intervals cannot interleave.
//!
//! Without it, a launch could publish its claim after a custodian's scan had passed
//! its directory and before that custodian's `chflags` landed — and the clear would
//! then revoke a guard that by then existed.
//!
//! `protocol::hash` pins the lock's exclusion in-process, over two descriptors on one
//! file. That is the property `flock` documents, but it is not the situation: the two
//! holders are always different processes, and an in-process test cannot tell an
//! exclusion from a self-conflict the same process would have had anyway. This drives
//! a real second process.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "cc-freeze-lock-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

/// Take the lock the way a custodian does, and say how it went.
fn try_lock(path: &std::path::Path) -> Result<(), String> {
    match protocol::hash::FreezeLock::acquire(path) {
        Ok(_lock) => Ok(()),
        Err(why) => Err(why.to_string()),
    }
}

#[test]
fn a_second_process_holding_the_freeze_lock_excludes_this_one_until_it_lets_go() {
    let dir = scratch("two-processes");
    let target = dir.join("codex");
    let marker = dir.join("locked");
    std::fs::write(&target, b"the bytes a launch would have pinned").unwrap();

    // Uncontended first, so a refusal below is the other process and not the file.
    try_lock(&target).expect("an uncontended lock must be takeable");

    // A hold long enough to outlast this side's whole bounded wait, and short enough
    // that the helper ends on its own if anything here goes wrong.
    let bin = env!("CARGO_BIN_EXE_codeconnect");
    let mut holder = Command::new(bin)
        .arg("internal-freeze-lock-hold")
        .arg(&target)
        .arg(&marker)
        .arg("20")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the lock holder");

    if !wait_until(Duration::from_secs(20), || marker.exists()) {
        let _ = holder.kill();
        let _ = holder.wait();
        let _ = std::fs::remove_dir_all(&dir);
        panic!("the holder never took the lock, so there is nothing to be excluded from");
    }

    // THE GATE. A custodian arriving now is refused, and refused **within its budget**
    // rather than blocking forever — a janitor that waited indefinitely on a live
    // launch would stop doing everything else it owes.
    let started = Instant::now();
    let refused = try_lock(&target);
    let waited = started.elapsed();

    let _ = holder.kill();
    let _ = holder.wait();

    let why = match refused {
        Err(why) => why,
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&dir);
            panic!(
                "a second process held the freeze lock and this one took it anyway — a \
                 custodian's scan and clear can then interleave with a launch's freeze \
                 and record, which is the whole hole the lock exists to close"
            );
        }
    };
    assert!(
        why.contains("held by another process"),
        "the refusal must say what it was waiting for: {why}"
    );
    assert!(
        waited < Duration::from_secs(15),
        "the wait must be bounded, and it took {waited:?}"
    );

    // And it is not a permanent wedge: once the holder is gone the lock is free
    // again, which is what a killed janitor or a killed launch has to leave behind.
    assert!(
        wait_until(Duration::from_secs(10), || try_lock(&target).is_ok()),
        "the lock must be released when its holder dies, or one crash would stop \
         every later launch from ever freezing again"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
