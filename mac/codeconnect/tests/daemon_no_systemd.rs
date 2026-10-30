//! On Linux with no systemd user manager there is no unit for `codeconnect daemon
//! uninstall` to remove: it says so, and what to do instead. With a unit installed,
//! the manager is only out of this shell's reach (`su`, `sudo -u`), and the unit is
//! not reported as absent. The unreachable manager is a `systemctl` on PATH that
//! fails as one with no bus does.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn uninstall(home: &Path) -> Output {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(home.join("cc")).unwrap();
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\necho 'Failed to connect to bus: No medium found' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args(["daemon", "uninstall"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", home)
        .env("CODECONNECT_HOME", home.join("cc"))
        .env("TMUX_TMPDIR", home)
        .output()
        .expect("running codeconnect")
}

fn scratch() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "cc-nosd-{}",
        protocol::uid::new().expect("a unique test directory")
    ))
}

#[test]
fn uninstall_without_systemd_says_there_is_no_service_to_remove() {
    let home = scratch();
    let run = uninstall(&home);
    std::fs::remove_dir_all(&home).unwrap();

    let said = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(1), "{said}");
    assert!(said.contains("no CodeConnect service to remove"), "{said}");
    assert!(said.contains("No medium found"), "{said}");
    assert!(
        said.contains("`codeconnect daemon status` shows its pid"),
        "{said}"
    );
}

#[test]
fn uninstall_out_of_reach_of_an_installed_unit_leaves_it_and_says_so() {
    let home = scratch();
    let unit = home.join(".config/systemd/user/com.codeconnect.ccd.service");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "[Unit]\n").unwrap();
    let run = uninstall(&home);
    let kept = unit.exists();
    std::fs::remove_dir_all(&home).unwrap();

    let said = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(1), "{said}");
    assert!(!said.contains("no CodeConnect service to remove"), "{said}");
    assert!(said.contains("was left in place"), "{said}");
    assert!(kept, "the unit was removed: {said}");
}
