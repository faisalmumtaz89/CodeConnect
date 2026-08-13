//! The retired SSH commands, run as the operator runs them.
//!
//! The unit tests beside `retirement_notice` can assert what the notice says,
//! but not what the process leaves behind: `RETIRED_EXIT_CODE` is a constant,
//! and a test that reads it proves the constant is `1` however `main` exits.
//! The status is the half a script reads, and the only way to assert it is to
//! run the binary — `CARGO_BIN_EXE_codeconnect` is defined for an integration
//! test and not for a unit test inside the binary itself, which is why this
//! file exists at all.

use std::process::Command;

/// Every spelling an older app still hands its owner. All of them are retired,
/// and none of them performs anything.
const RETIRED: &[&[&str]] = &[
    &["pair", "--ssh"],
    &["pair", "--qr", "--ssh"],
    &["pair", "--ssh", "--qr"],
    &["ssh-revoke"],
    &["ssh-revoke", "iPhone"],
    &["revoke", "--ssh"],
    &["revoke", "iPhone", "--ssh"],
];

#[test]
fn a_retired_command_leaves_with_a_status_no_script_can_read_as_success() {
    for words in RETIRED {
        let run = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(*words)
            .output()
            .unwrap_or_else(|err| panic!("running `codeconnect {}`: {err}", words.join(" ")));

        // The status, which is the whole point of running the binary: a script
        // that reads success here would be reading a pairing that never
        // happened, or a revocation nothing performed.
        assert_eq!(
            run.status.code(),
            Some(1),
            "`codeconnect {}` exited {:?}",
            words.join(" "),
            run.status.code()
        );

        // Nothing on stdout, because stdout is where a QR code or a machine
        // readable answer would go and there is neither.
        assert!(
            run.stdout.is_empty(),
            "`codeconnect {}` wrote {} byte(s) to stdout",
            words.join(" "),
            run.stdout.len()
        );

        // And the reader is told why, rather than left at a bare failure.
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("is retired"),
            "`codeconnect {}` explained nothing: {stderr}",
            words.join(" ")
        );
    }
}

/// The neighbours the diversion must not catch. A mistyped option is still an
/// error naming the usage, and an unknown command is still unknown — neither is
/// a retirement notice, and neither reports success either.
#[test]
fn a_command_that_is_not_retired_is_not_diverted_into_the_notice() {
    for words in [
        vec!["pair", "--sshh"],
        vec!["pair", "--nope"],
        vec!["ssh-install"],
        vec!["sshrevoke"],
    ] {
        let run = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(&words)
            .output()
            .unwrap_or_else(|err| panic!("running `codeconnect {}`: {err}", words.join(" ")));
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            !stderr.contains("is retired"),
            "`codeconnect {}` was diverted into the retirement notice: {stderr}",
            words.join(" ")
        );
        assert_ne!(
            run.status.code(),
            Some(0),
            "`codeconnect {}` reported success",
            words.join(" ")
        );
    }
}
