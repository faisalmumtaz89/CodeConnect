//! **The launcher's own freeze, and the ending that runs no `Drop`.**
//!
//! The exec hash-pin freezes the pinned `codex` immutable and gives the flag back
//! in a guard's `Drop`. The launcher takes one of those freezes before a uid exists
//! — the launch probes read version, command surface, both schema bundles and the
//! effective feature value under a single held freeze — so there is no launch record
//! for the custodian to read and nothing but that `Drop` to put the flag back.
//!
//! `SIGINT` runs no `Drop`. With `panic = "abort"` in the release profile neither
//! would an unwind. So `Ctrl-C` during a launch left `UF_IMMUTABLE` on the real
//! binary, on demand, every time — and a frozen `codex` is invisible until the next
//! update fails with `Operation not permitted`. The signal release is what stands in
//! for the record there, and this is the test that it does.

use std::os::macos::fs::MetadataExt;
use std::process::{Command, Stdio};
use std::time::Duration;

fn scratch(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "cc-freeze-signal-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn flags_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).map(|m| m.st_flags()).unwrap_or(0)
}

/// Take the flag off whatever happened, so a failing run cannot leave an
/// undeletable fixture behind.
fn thaw(path: &std::path::Path) {
    if let Ok(f) = std::fs::File::open(path) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::fchflags(f.as_raw_fd(), flags_of(path) & !libc::UF_IMMUTABLE) };
    }
}

fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[test]
fn a_sigint_during_the_launcher_s_probe_freeze_gives_the_flag_back() {
    let dir = scratch("sigint");
    let target = dir.join("codex");
    let marker = dir.join("frozen");
    std::fs::write(&target, b"the bytes a launch would have pinned").unwrap();
    assert_eq!(flags_of(&target), 0, "the fixture starts with no flags set");

    let bin = env!("CARGO_BIN_EXE_codeconnect");
    let mut child = Command::new(bin)
        .arg("internal-freeze-probe")
        .arg(&target)
        .arg(&marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the freeze probe");

    let staged = wait_until(Duration::from_secs(20), || marker.exists());
    let frozen_while_held = flags_of(&target) & libc::UF_IMMUTABLE != 0;
    if !staged || !frozen_while_held {
        let _ = child.kill();
        let _ = child.wait();
        thaw(&target);
        let _ = std::fs::remove_dir_all(&dir);
        panic!("the probe never got the flag on, so there is nothing to interrupt");
    }

    // The interrupt. Not a kill: `SIGINT` is what a `Ctrl-C` at a launch delivers,
    // and it is the one the default disposition would have ended the process on with
    // no `Drop` run and the flag left behind.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(rc, 0, "the probe must still be there to interrupt");
    let status = child.wait().expect("the probe exits on the interrupt");

    let after = flags_of(&target);
    thaw(&target);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        after & libc::UF_IMMUTABLE,
        0,
        "a SIGINT during the launcher's freeze must give the flag back; it was left set"
    );
    // And the ending stays honest: the handler restores the default disposition and
    // re-raises, so the process is reported as killed by the signal that was sent
    // rather than exiting cleanly out from under it.
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGINT),
        "the handler must add a cleanup to the ending, not invent a different one"
    );
}
