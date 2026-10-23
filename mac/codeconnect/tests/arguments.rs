//! What a command does with an argument it does not take is only visible by running
//! the binary. Every command here is one whose action is harmless in a scratch home
//! even if it were taken: the commands that install, update or restart are left out,
//! because a regression would reach the real launchd or download a release.

use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args(args)
        .env("HOME", home)
        .env("CODECONNECT_HOME", home.join("cc"))
        .env("TMUX_TMPDIR", home)
        .output()
        .expect("running codeconnect")
}

#[test]
fn an_argument_a_command_does_not_take_is_refused_before_it_acts() {
    let home = std::env::temp_dir().join(format!(
        "cc-args-{}",
        protocol::uid::new().expect("a unique test directory")
    ));
    std::fs::create_dir_all(home.join("cc")).unwrap();
    std::fs::write(home.join("cc/token"), "SECRET-TOKEN\n").unwrap();

    let asked = run(&home, &["token"]);
    let refused: Vec<_> = [
        &["token", "--help"][..],
        &["token", "-h"],
        &["token", "help"],
        &["ls", "--help"],
        &["sessions", "list", "--help"],
        &["devices", "--help"],
        &["pair", "--help"],
        &["revoke", "phone", "--dry-run"],
        &["revoke", "--help"],
        &["attach", "cc-1", "--help"],
    ]
    .into_iter()
    .map(|args| (args.join(" "), run(&home, args)))
    .collect();
    std::fs::remove_dir_all(&home).unwrap();

    assert_eq!(asked.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&asked.stdout), "SECRET-TOKEN\n");
    for (args, run) in refused {
        let said = String::from_utf8_lossy(&run.stderr);
        assert_eq!(run.status.code(), Some(1), "{args}: {said}");
        assert!(run.stdout.is_empty(), "{args} printed {:?}", run.stdout);
        assert!(said.contains("usage:"), "{args}: {said}");
        assert!(!said.contains("SECRET-TOKEN"), "{args} leaked the token");
    }
}
