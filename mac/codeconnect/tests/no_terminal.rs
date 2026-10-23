//! `codeconnect claude` with no terminal to show a session in — run from a script,
//! a pipe or `ssh host command` — runs claude as claude runs directly. Whether a
//! launch hosts anything is only visible from outside the binary, with its
//! terminal taken away one way at a time, which is why this runs the built binary.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// How the launch is denied its terminal, if it is.
enum Missing {
    /// Nothing: a usable terminal, so the launch hosts a session.
    Nothing,
    /// Keys and output on a terminal, but none controlling: `/dev/tty` cannot be
    /// opened, so there is no size to show a session at.
    Controlling,
    /// A controlling terminal, but the keys come from nowhere.
    Input,
    /// A controlling terminal and keys, but the output goes into a pipe.
    Output,
}

/// A pseudo-terminal of a usable size, both ends.
fn pty() -> (OwnedFd, OwnedFd) {
    let (mut primary, mut secondary) = (0, 0);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let opened = unsafe {
        libc::openpty(
            &mut primary,
            &mut secondary,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe {
        (
            OwnedFd::from_raw_fd(primary),
            OwnedFd::from_raw_fd(secondary),
        )
    }
}

/// The launch's exit status and what it printed, wherever it printed it.
fn launch(home: &Path, missing: Missing) -> (Option<i32>, String) {
    let (primary, secondary) = pty();
    let mut command = Command::new(env!("CARGO_BIN_EXE_codeconnect"));
    command
        .args(["claude", "--version"])
        .env("HOME", home)
        .env("CODECONNECT_HOME", home.join("cc"))
        .env("TMUX_TMPDIR", home);
    let terminal = secondary.as_raw_fd();
    let controlling = !matches!(missing, Missing::Controlling);
    match missing {
        Missing::Input => command.stdin(Stdio::null()),
        Missing::Nothing | Missing::Controlling | Missing::Output => {
            command.stdin(Stdio::from(secondary.try_clone().unwrap()))
        }
    };
    let onto_terminal = !matches!(missing, Missing::Output);
    if onto_terminal {
        command.stdout(Stdio::from(secondary.try_clone().unwrap()));
    }
    // A new session with no terminal, or with the pty as its controlling terminal.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0
                || (controlling && libc::ioctl(terminal, libc::TIOCSCTTY as _, 0) < 0)
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The terminal is read for as long as the launch runs, as a real one is: a
    // session leader's exit waits for its terminal's output to drain. It answers
    // what an attach asks a terminal — its colours, cursor and attributes.
    let reader = std::thread::spawn(move || {
        let mut terminal = std::fs::File::from(primary);
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        // The other end closing reads as an error, not an end: that is the end.
        while let Ok(read @ 1..) = std::io::Read::read(&mut terminal, &mut chunk) {
            let asked = &chunk[..read];
            if asked.windows(3).any(|w| w == b"\x1b[c") || asked.windows(4).any(|w| w == b"\x1b[6n")
            {
                let _ = std::io::Write::write_all(
                    &mut terminal,
                    b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[1;1R\x1b[?62;22c",
                );
            }
            bytes.extend_from_slice(asked);
        }
        bytes
    });
    let run = command.output().expect("running codeconnect claude");
    // Every copy of the terminal's other end closed — the command holds some too —
    // so the reader sees the end.
    drop(command);
    drop(secondary);
    let on_terminal = reader.join().expect("reading the terminal");
    let printed = if onto_terminal {
        String::from_utf8_lossy(&on_terminal).replace("\r\n", "\n")
    } else {
        String::from_utf8_lossy(&run.stdout).into_owned()
    };
    (run.status.code(), printed)
}

#[test]
fn with_no_terminal_to_show_a_session_claude_runs_as_it_runs_directly() {
    // Short and under `/tmp`: tmux's socket lives in `TMUX_TMPDIR`, a unix socket
    // path must fit `sun_path`, and macOS's temp dir is long enough to overflow it.
    let home = std::path::PathBuf::from(format!(
        "/tmp/cc-noterm-{}",
        protocol::uid::new().expect("a unique test directory")
    ));
    std::fs::create_dir_all(home.join("cc")).unwrap();
    let claude = home.join("claude");
    std::fs::write(&claude, "#!/bin/sh\necho \"STAND-IN $*\"\nexit 3\n").unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("cc/config.json"),
        format!(r#"{{"claude_bin": {:?}, "update_check": false}}"#, claude),
    )
    .unwrap();

    let mut verdicts = Vec::new();
    for (name, missing) in [
        ("no controlling terminal", Missing::Controlling),
        ("input not a terminal", Missing::Input),
        ("output not a terminal", Missing::Output),
    ] {
        let (status, printed) = launch(&home, missing);
        let hosted = home.join("cc/sessions").exists();
        verdicts.push((name, status, printed, hosted));
    }
    // And with nothing missing it hosts — where there is a tmux to host on — so
    // the three above are about the terminal and not about a launch that could
    // never host: claude is handed its `--settings`, and the command exits 0.
    let control = protocol::tmux::tmux_bin().map(|_| {
        let (status, printed) = launch(&home, Missing::Nothing);
        (status, printed, home.join("cc/sessions").exists())
    });
    std::fs::remove_dir_all(&home).unwrap();
    if let Some((status, printed, hosted)) = control {
        assert!(hosted, "a usable terminal hosted nothing: {printed}");
        assert_eq!(status, Some(0), "{printed}");
        assert!(printed.contains("STAND-IN --settings "), "{printed}");
    }
    for (name, status, printed, hosted) in verdicts {
        // Claude's own status and exactly its argv: a hosted run exits 0 and hands
        // claude its `--settings`.
        assert_eq!(status, Some(3), "{name}: {printed}");
        assert_eq!(printed, "STAND-IN --version\n", "{name}");
        assert!(
            !hosted,
            "{name}: a session was created that nothing could show"
        );
    }
}
