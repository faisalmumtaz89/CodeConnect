//! A command that prints and exits, piped into a reader that has already gone
//! (`codeconnect daemon status | head -3`), ends quietly instead of panicking.
//! Only commands harmless in a scratch home are run here.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

#[test]
fn a_printing_command_whose_reader_is_gone_ends_quietly() {
    let home = std::env::temp_dir().join(format!(
        "cc-pipe-{}",
        protocol::uid::new().expect("a unique test directory")
    ));
    std::fs::create_dir_all(home.join("cc")).unwrap();
    std::fs::write(home.join("cc/token"), "SECRET-TOKEN\n").unwrap();

    for args in [&["--version"][..], &["token"]] {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe(2) just returned both descriptors, owned by nothing else.
        let (reader, writer) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        drop(reader);

        let run = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(args)
            .env("HOME", &home)
            .env("CODECONNECT_HOME", home.join("cc"))
            .env("TMUX_TMPDIR", &home)
            .stdout(Stdio::from(writer))
            .output()
            .expect("running codeconnect");

        let said = String::from_utf8_lossy(&run.stderr);
        assert!(said.is_empty(), "{args:?} said {said:?}");
        assert_eq!(
            run.status.signal(),
            Some(libc::SIGPIPE),
            "{args:?}: {:?}",
            run.status
        );
    }
    std::fs::remove_dir_all(&home).unwrap();
}
