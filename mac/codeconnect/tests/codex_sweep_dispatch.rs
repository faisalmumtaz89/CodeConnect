//! **The one proof that the shipped `codeconnect` actually performs the recovery
//! pass** — across the real dispatcher, in the real subprocess the daemon spawns.
//!
//! Everything else about this path is tested on one side of a seam or the other:
//! `ccd`'s own tests run a shell stub and prove the daemon sends the intended argv,
//! and the launcher's unit tests call the sweep body directly inside the test
//! process. Neither says the dispatch arm between them reaches that body. A
//! `CODEX_SWEEP_SUBCOMMAND` arm changed to a no-op that exits zero passes both.
//!
//! So this runs the BUILT binary the way the daemon runs it, under a throwaway
//! `CODECONNECT_HOME`, over a real `UF_IMMUTABLE` flag on a real file whose recorded
//! holder is a real process that has been killed and reaped — and asserts the flag
//! comes off, the claim is withdrawn, the pass exits zero, and it said which record
//! it was about.
//!
//! No tmux, no codex, no daemon: the subject is the dispatcher and the flag.

use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Thaws the fixture and removes the whole scratch tree whatever happens —
/// **including the path where an assertion fails**, which is the path that matters. A
/// `UF_IMMUTABLE` file left in the temp dir cannot be deleted without `chflags
/// nouchg`: the exact wound this whole item exists to heal, left behind by the test
/// that proves it healed. The thaw comes first, because the flag is what would stop
/// the removal.
struct Fixture {
    base: PathBuf,
    frozen: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let flags = std::fs::metadata(&self.frozen)
            .map(|m| m.st_flags())
            .unwrap_or(0);
        if flags & libc::UF_IMMUTABLE != 0 {
            chflags(&self.frozen, flags & !libc::UF_IMMUTABLE);
        }
        std::fs::remove_dir_all(&self.base).ok();
    }
}

fn chflags(path: &Path, flags: u32) {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path this test owns; `chflags` touches nothing else.
    let rc = unsafe { libc::chflags(c.as_ptr(), flags) };
    assert_eq!(rc, 0, "chflags on {}", path.display());
}

fn st_flags(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("stat the fixture")
        .st_flags()
}

/// A process that is **provably** dead: spawned, killed, and reaped, so the kernel
/// has no `(pid, birth)` left to answer for it. The freeze warrants turn on exactly
/// this, so the identity is read from the live process rather than invented.
fn a_dead_holder() -> protocol::proc_identity::ProcessIdentity {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a stand-in holder");
    let pid = child.id() as i32;
    let birth = protocol::proc_identity::read_birth_identity(pid).expect("its birth identity");
    child.kill().ok();
    child.wait().ok();
    protocol::proc_identity::ProcessIdentity { pid, birth }
}

#[test]
fn the_shipped_launcher_clears_a_leaked_freeze_through_its_own_sweep_subcommand() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base =
        std::env::temp_dir().join(format!("cc-sweep-dispatch-{}-{nanos}", std::process::id()));
    let home = base.join("home");
    let uid = "01JSWEEPDISPATCH00000000AB";
    let session = home.join("sessions").join(uid);
    std::fs::create_dir_all(&session).expect("the scratch home");

    // A real file with a real `UF_IMMUTABLE` on it, exactly as a launch's probe
    // would have left it.
    let bin = base.join("codex-fixture");
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("the fixture binary");
    let _fixture = Fixture {
        base: base.clone(),
        frozen: bin.clone(),
    };
    let meta = std::fs::metadata(&bin).expect("stat the fixture");
    let (dev, ino, original_flags) = (meta.dev(), meta.ino(), meta.st_flags());
    chflags(&bin, original_flags | libc::UF_IMMUTABLE);
    assert_ne!(
        st_flags(&bin) & libc::UF_IMMUTABLE,
        0,
        "the fixture must start immutable, or this proves nothing"
    );

    let holder = a_dead_holder();
    let boot = protocol::proc_identity::boot_identity().expect("this machine's boot identity");
    let me = protocol::proc_identity::current_identity().expect("this process's identity");
    // A committed, torn-down launch — so the pass has no lifecycle work to do and no
    // custodian to rearm, and the only thing it is owed is the freeze claim.
    let record = serde_json::json!({
        "schema": 1,
        "launch_nonce": "sweep-dispatch-nonce",
        "uid": uid,
        "session_name": "cc-1",
        "coordinator": me,
        "custodian": null,
        "boot": boot,
        "deadline_monotonic_nanos": 1u64,
        "state": "Ready",
        "cleanup": "Complete",
        "host_lease": null,
        "children": [],
        "created_ms": 1,
        "exec_freeze": {
            "path": bin.display().to_string(),
            "dev": dev,
            "ino": ino,
            "original_flags": original_flags,
            "ownership": "set",
            "holder": { "identity": holder, "boot": boot },
        },
    });
    std::fs::write(
        session.join("launch.json"),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .expect("stage the launch record");

    // The daemon's own invocation, verbatim: the shared subcommand constant, the
    // socket flag, and `CODECONNECT_HOME` inherited by the child.
    let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .arg(protocol::CODEX_SWEEP_SUBCOMMAND)
        .args(["--socket", "cc-sweep-dispatch"])
        .env("CODECONNECT_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run the sweep subcommand");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();

    let flags_after = st_flags(&bin);
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session.join("launch.json")).unwrap()).unwrap();

    assert!(
        out.status.success(),
        "a pass with nothing left owed must exit 0; it exited {:?} saying {said}",
        out.status.code()
    );
    assert_eq!(
        flags_after & libc::UF_IMMUTABLE,
        0,
        "the flag must come off the real file without anybody typing chflags; it \
         said {said}"
    );
    assert!(
        after
            .get("exec_freeze")
            .is_none_or(serde_json::Value::is_null),
        "and the claim must be withdrawn, or the next pass goes back to a file this \
         one has already dealt with: {after}"
    );
    assert!(
        said.contains(uid),
        "a flag coming off a real binary is worth reading, and the line must name the \
         record it was about: {said}"
    );
}

/// **A repair the pass makes is said, not only recorded.**
///
/// A stale `pending` CAS'd to `failed{cleanup:pending}` is the sweep's most consequential
/// act on a record's lifecycle — a launch somebody may still have open becomes a failed
/// one — and it had no line at all. The durable state remained inspectable, which is not
/// the same as legible: ccd's log is where an operator looks to find out what happened to
/// a session, and a repair that only exists in a JSON file two directories deep is a
/// repair nobody will connect to the launch that stopped working.
///
/// Staged so the CAS is the ONLY thing owed: the coordinator is provably dead and the
/// deadline is past, so the record is stale — but the custodian is this live test
/// process, so no replacement is called for and nothing is spawned.
#[test]
fn the_pass_says_the_lifecycle_repair_it_makes() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("cc-sweep-repair-{}-{nanos}", std::process::id()));
    let home = base.join("home");
    let uid = "01JSWEEPREPAIR0000000000AB";
    let session = home.join("sessions").join(uid);
    std::fs::create_dir_all(&session).expect("the scratch home");
    struct Tree(PathBuf);
    impl Drop for Tree {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
    let _tree = Tree(base.clone());

    let boot = protocol::proc_identity::boot_identity().expect("this machine's boot identity");
    let me = protocol::proc_identity::current_identity().expect("this process's identity");
    let record = serde_json::json!({
        "schema": 1,
        "launch_nonce": "sweep-repair-nonce",
        "uid": uid,
        "session_name": "cc-1",
        // Gone, so the launch has nobody driving it.
        "coordinator": a_dead_holder(),
        // Alive, so the pass owes a transition and NOT a replacement custodian.
        "custodian": me,
        "boot": boot,
        "deadline_monotonic_nanos": 1u64,
        "state": "Pending",
        "cleanup": "Pending",
        "host_lease": null,
        "children": [],
        "created_ms": 1,
    });
    std::fs::write(
        session.join("launch.json"),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .expect("stage the launch record");

    let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .arg(protocol::CODEX_SWEEP_SUBCOMMAND)
        .args(["--socket", "cc-sweep-repair"])
        .env("CODECONNECT_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run the sweep subcommand");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session.join("launch.json")).unwrap()).unwrap();

    assert!(
        said.contains(uid) && said.contains("recorded as failed"),
        "the repair must be said, and the line must name the record it was about: {said}"
    );
    assert!(
        after["state"].get("Failed").is_some(),
        "and the repair must actually have been made: {after}"
    );
    assert!(
        out.status.success(),
        "a pass that made its repair and owes nothing else exits 0; it exited {:?} saying \
         {said}",
        out.status.code()
    );
}
