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
//! `CODECONNECT_HOME`, over a launch record whose recorded coordinator is a real
//! process that has been killed and reaped — and asserts the repair is made, the pass
//! exits zero, and it said which record it was about.
//!
//! No tmux, no codex, no daemon: the subject is the dispatcher and the record.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A process that is **provably** dead: spawned, killed, and reaped, so the kernel
/// has no `(pid, birth)` left to answer for it. The sweep's liveness checks turn on
/// exactly this, so the identity is read from the live process rather than invented.
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
