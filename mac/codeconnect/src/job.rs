//! Job control for the agent in a pane, so Ctrl+Z stops it as it does in a shell.
//!
//! A pane's own process leads a session whose parent, the tmux server, is in another
//! one, so its process group is orphaned and the kernel discards the SIGTSTP an agent
//! sends itself. The agent therefore runs as a job of the pane's process: in its own
//! group, in the terminal's foreground. When it stops, the pane's process takes the
//! terminal back, marks the pane stopped (`@codeconnect-stopped`) and writes
//! [`STOP_MARKER`] after everything the agent printed, which tells each viewer to
//! suspend itself in the user's shell. A viewer resumed with `fg` sends SIGUSR1, and the
//! agent gets the terminal back and SIGCONT. When the last viewer goes away while the
//! agent is stopped, or the session does, it gets SIGHUP and SIGCONT, as a shell's
//! stopped job does when its terminal closes. A stop with no viewer at the Mac attached
//! is dropped: the phone's terminal cannot `fg`.
//!
//! Claude's and OpenCode's panes run [`run`] (`codeconnect internal-job`); the Codex
//! host is its TUI's parent already and calls [`watch`] itself.

use std::ffi::{CString, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub const SUBCOMMAND: &str = "internal-job";

/// Written to the pane after the agent stopped; the viewer drops it.
pub const STOP_MARKER: &[u8] = b"\x1bP=codeconnect-stopped\x1b\\";

const STOPPED_OPTION: &str = "@codeconnect-stopped";

/// The row a resuming viewer's cursor is on, where the agent resumes.
pub const CURSOR_OPTION: &str = "@codeconnect-cursor";

/// How often a stopped agent's pane asks whether a viewer is still attached.
const VIEWER_POLL: Duration = Duration::from_secs(1);

static AGENT: AtomicI32 = AtomicI32::new(0);
static RESUME: [AtomicI32; 2] = [AtomicI32::new(-1), AtomicI32::new(-1)];

extern "C" fn resume_requested(_: libc::c_int) {
    let fd = RESUME[1].load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

/// The OpenCode session's files, as paths a signal handler can unlink.
static SESSION_FILES: OnceLock<Vec<CString>> = OnceLock::new();

/// Pass a terminal signal to the agent's group. Before the agent runs, the job ends
/// by the signal instead, removing the session's files first.
extern "C" fn forward(signal: libc::c_int) {
    let agent = AGENT.load(Ordering::SeqCst);
    if agent > 0 {
        unsafe {
            libc::killpg(agent, signal);
            libc::killpg(agent, libc::SIGCONT);
        }
        return;
    }
    for path in SESSION_FILES.get().into_iter().flatten() {
        unsafe { libc::unlink(path.as_ptr()) };
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Hand SIGHUP, SIGINT and SIGTERM to [`forward`].
fn forward_terminal_signals() {
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(signal, forward as *const () as libc::sighandler_t) };
    }
}

/// Ignore SIGTTOU, so this process can hand the terminal back and forth, and hear a
/// viewer's SIGUSR1. Before the agent is spawned.
pub fn prepare() -> Result<()> {
    let mut pipe = [0; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("creating the resume pipe");
    }
    for fd in pipe {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    unsafe { libc::fcntl(pipe[1], libc::F_SETFL, libc::O_NONBLOCK) };
    RESUME[0].store(pipe[0], Ordering::SeqCst);
    RESUME[1].store(pipe[1], Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(
            libc::SIGUSR1,
            resume_requested as *const () as libc::sighandler_t,
        );
    }
    Ok(())
}

/// Spawn `command` as the terminal's foreground job, with the terminal signals a
/// shell gives its jobs. A tmux pane starts with SIGTTIN and SIGTTOU ignored, and a
/// job that ignores SIGTTIN has a read it restarts after being continued fail with EIO.
pub fn in_foreground(command: &mut Command) {
    command.process_group(0);
    unsafe {
        command.pre_exec(|| {
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            libc::signal(libc::SIGTTIN, libc::SIG_DFL);
            Ok(())
        });
    }
}

/// The usage line, options included.
const USAGE: &str = "usage: codeconnect internal-job [--opencode-dir DIR] [--verify PATH SHA256] \
                     PROGRAM [ARG...]";

/// What the options ahead of `PROGRAM` ask of the job.
#[derive(Debug, Default, PartialEq)]
struct Options {
    /// An OpenCode session directory: the agent's identity is published there once
    /// it runs, and the session's files there are removed when it ends.
    opencode_dir: Option<PathBuf>,
    /// The executable the agent must still be, and its sha256, checked right
    /// before it is started.
    verify: Option<(PathBuf, String)>,
}

/// Read the options ahead of `PROGRAM`. An option is recognised only as an exact
/// word at the front; `PROGRAM` is `/usr/bin/env` from the pane's terminal wrapper,
/// so it is never one of them.
fn parse(mut args: &[OsString]) -> Result<(Options, &[OsString])> {
    let mut options = Options::default();
    loop {
        match args {
            [flag, dir, rest @ ..]
                if flag == "--opencode-dir" && options.opencode_dir.is_none() =>
            {
                options.opencode_dir = Some(PathBuf::from(dir));
                args = rest;
            }
            [flag, path, sha256, rest @ ..] if flag == "--verify" && options.verify.is_none() => {
                let path = PathBuf::from(path);
                crate::codex::require_absolute("--verify", &path)?;
                let sha256 = crate::codex::parse_sha256(
                    crate::codex::OPENCODE.name,
                    sha256.to_str().unwrap_or_default(),
                )?;
                options.verify = Some((path, sha256));
                args = rest;
            }
            [flag, ..] if flag == "--opencode-dir" || flag == "--verify" => bail!(USAGE),
            _ => return Ok((options, args)),
        }
    }
}

/// `codeconnect internal-job [--opencode-dir DIR] [--verify PATH SHA256] PROGRAM
/// [ARG...]`: run `PROGRAM` as this pane's job and end as it ends.
///
/// With `--verify`, the executable at `PATH` must still hash to `SHA256` the moment
/// before `PROGRAM` starts. With `--opencode-dir`, the agent's pid and start time
/// are published in `DIR` once it runs, and the OpenCode session's files there are
/// removed however the job ends; when the agent cannot be started, the pane shows
/// why until Return.
pub fn run(args: &[OsString]) -> Result<()> {
    let (options, args) = parse(args)?;
    if let Some(dir) = &options.opencode_dir {
        let files = crate::opencode::SESSION_FILES
            .iter()
            .filter_map(|name| CString::new(dir.join(name).into_os_string().into_vec()).ok())
            .collect();
        let _ = SESSION_FILES.set(files);
        forward_terminal_signals();
    }
    let ended = start(&options, args);
    if let Some(dir) = &options.opencode_dir {
        remove_session_files(dir);
        if let Err(error) = &ended {
            show_until_return(error);
            std::process::exit(1);
        }
    }
    match ended? {
        Ended::Signalled(signal) => {
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
            std::process::exit(128 + signal);
        }
        Ended::Exited(code) => std::process::exit(code),
    }
}

/// Start the agent and wait for it to end.
fn start(options: &Options, args: &[OsString]) -> Result<Ended> {
    let [program, rest @ ..] = args else {
        bail!(USAGE);
    };
    prepare()?;
    let mut command = Command::new(program);
    command.args(rest);
    in_foreground(&mut command);
    if let Some((path, sha256)) = &options.verify {
        crate::codex::verify_binary_identity(
            &crate::codex::OPENCODE,
            path,
            sha256,
            "before the pane starts opencode",
        )?;
    }
    let agent = command
        .spawn()
        .with_context(|| format!("starting {}", program.to_string_lossy()))?
        .id() as libc::pid_t;
    AGENT.store(agent, Ordering::SeqCst);
    forward_terminal_signals();
    unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, agent) };
    if let Some(dir) = &options.opencode_dir {
        publish_agent(dir, agent);
    }
    loop {
        let mut status = 0;
        if unsafe { libc::waitpid(agent, &mut status, libc::WUNTRACED) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting for the agent");
        }
        if libc::WIFSTOPPED(status) {
            hold_while_stopped(agent, &|_| {});
        } else if libc::WIFSIGNALED(status) {
            return Ok(Ended::Signalled(libc::WTERMSIG(status)));
        } else {
            return Ok(Ended::Exited(libc::WEXITSTATUS(status)));
        }
    }
}

/// How the agent ended, which is how the job ends.
enum Ended {
    Exited(i32),
    Signalled(libc::c_int),
}

/// Publish the agent's pid and start time in `dir`, for the plugin inside it to
/// name itself by. The start time is the kernel's, and an exec keeps it, so it is
/// the agent's own after the wrapper has exec'd it. Written whole under another
/// name and renamed into place, so the plugin never reads a partial file. When the
/// start time cannot be read, or something already holds the staging name, nothing
/// is published and the agent runs unobserved.
fn publish_agent(dir: &Path, agent: libc::pid_t) {
    let Some(birth) = protocol::proc_identity::read_birth_identity(agent) else {
        return;
    };
    let record = format!(
        r#"{{"pid":{agent},"start":{{"sec":{},"usec":{}}}}}"#,
        birth.start_sec, birth.start_usec
    );
    let staged = dir.join(format!("{}.tmp", crate::opencode::AGENT_FILE));
    let Ok(mut file) = protocol::fsperm::create_private_new(&staged) else {
        return;
    };
    let published = file
        .write_all(record.as_bytes())
        .and_then(|()| std::fs::rename(&staged, dir.join(crate::opencode::AGENT_FILE)));
    // Only a file this job created is ever removed.
    if published.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
}

/// Remove the OpenCode session's files from `dir`. Best effort and silent: the
/// pane is closing, and a file already gone is the outcome wanted.
fn remove_session_files(dir: &Path) {
    for name in crate::opencode::SESSION_FILES {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Print `error` in the pane and keep the pane until Return or the end of its input,
/// so the reason is read before the session closes.
/// A child that failed to exec may have taken the terminal first, so it is taken back.
fn show_until_return(error: &anyhow::Error) {
    unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) };
    let _ = writeln!(
        std::io::stderr(),
        "codeconnect: {error:#}\nPress Return to close this session."
    );
    let _ = std::io::stdin().read_line(&mut String::new());
}

/// Hold each stop of `agent`, a child of this process leading its own group, until
/// it has exited. `pause` is told when the agent is held stopped and when it is let go,
/// so what runs beside it can stop and continue with it.
pub fn watch(agent: libc::pid_t, pause: impl Fn(bool) + Send + 'static) {
    AGENT.store(agent, Ordering::SeqCst);
    std::thread::spawn(move || loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::waitid(libc::P_PID, agent as libc::id_t, &mut info, libc::WSTOPPED) } != 0
        {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        hold_while_stopped(agent, &pause);
    });
}

/// How a stop ends.
#[derive(Debug, PartialEq)]
enum Release {
    /// No viewer at the Mac was attached to hold it: nobody can `fg`, so the stop is
    /// dropped, as the kernel drops one for a job nobody can resume.
    Unheld,
    /// A viewer came back with `fg`.
    Resumed,
    /// Every viewer at the Mac went away: a hangup, as a shell gives its stopped jobs
    /// when its terminal goes.
    Hangup,
    /// The pane's session or server is gone: a hangup, and nothing more is asked of a
    /// tmux that may now be another one.
    Gone,
    /// The agent ended while stopped.
    Exited,
}

/// Take the terminal from the stopped `agent`, tell the viewers, and give it back once
/// a viewer resumes it, with SIGHUP first if the viewers or the session went away.
fn hold_while_stopped(agent: libc::pid_t, pause: &dyn Fn(bool)) {
    let release = match viewers() {
        Some(0) => Release::Unheld,
        Some(_) => hold(agent, pause),
        None => Release::Gone,
    };
    unsafe {
        match release {
            Release::Exited => {}
            Release::Hangup | Release::Gone => {
                libc::killpg(agent, libc::SIGHUP);
                libc::killpg(agent, libc::SIGCONT);
            }
            Release::Unheld | Release::Resumed => {
                libc::killpg(agent, libc::SIGCONT);
            }
        }
    }
}

fn hold(agent: libc::pid_t, pause: &dyn Fn(bool)) -> Release {
    let mut modes: libc::termios = unsafe { std::mem::zeroed() };
    unsafe {
        libc::tcgetattr(libc::STDIN_FILENO, &mut modes);
        libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
        let mut quiet = modes;
        quiet.c_lflag &= !(libc::ISIG | libc::ICANON | libc::ECHO);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet);
    }
    let resume = RESUME[0].load(Ordering::SeqCst);
    drain(resume);
    pause(true);
    set_stopped(true);
    write_pane(STOP_MARKER);
    let release = loop {
        let mut poll = libc::pollfd {
            fd: resume,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, VIEWER_POLL.as_millis() as i32) } > 0 {
            drain(resume);
            break Release::Resumed;
        }
        if exited(agent) {
            break Release::Exited;
        }
        match viewers() {
            Some(0) => break Release::Hangup,
            None => break Release::Gone,
            Some(_) => {}
        }
    };
    if release != Release::Gone {
        set_stopped(false);
    }
    if release == Release::Resumed {
        // The viewer's cursor, so the agent resumes where the user's shell left it.
        let row = tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane(),
            &format!("#{{{CURSOR_OPTION}}}"),
        ]);
        if let Some(row) =
            row.filter(|row| !row.is_empty() && row.bytes().all(|b| b.is_ascii_digit()))
        {
            write_pane(format!("\x1b[{row};1H").as_bytes());
        }
        tmux(&["set-option", "-p", "-t", &pane(), "-u", CURSOR_OPTION]);
    }
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &modes);
        libc::tcsetpgrp(libc::STDIN_FILENO, agent);
    }
    pause(false);
    release
}

/// Whether the stopped `agent` has ended, without collecting its status: its waiter
/// still reaps it.
fn exited(agent: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let waited = unsafe {
        libc::waitid(
            libc::P_PID,
            agent as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    waited == 0 && unsafe { info.si_pid() } == agent
}

/// How many viewers at the Mac the pane's session has, if its tmux server answers:
/// `None` once the session or the server is gone. The phone's terminal is attached as
/// a client too, flagged `ignore-size`; it is not a viewer, since nobody can `fg` from it.
fn viewers() -> Option<usize> {
    let answer = tmux(&["list-clients", "-t", &pane(), "-F", "#{client_flags}"])?;
    Some(
        answer
            .lines()
            .filter(|flags| !flags.split(',').any(|flag| flag == "ignore-size"))
            .count(),
    )
}

fn write_pane(bytes: &[u8]) {
    unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
}

fn drain(fd: libc::c_int) {
    let mut byte = [0u8; 64];
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    while unsafe { libc::poll(&mut poll, 1, 0) } > 0 {
        unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
    }
}

fn set_stopped(stopped: bool) {
    let pane = pane();
    let mut args = vec!["set-option", "-p", "-t", pane.as_str()];
    args.extend(if stopped {
        [STOPPED_OPTION, "1"].as_slice()
    } else {
        ["-u", STOPPED_OPTION].as_slice()
    });
    tmux(&args);
}

fn pane() -> String {
    std::env::var("TMUX_PANE").unwrap_or_default()
}

fn tmux(args: &[&str]) -> Option<String> {
    let socket = std::env::var("TMUX").ok()?;
    let socket = socket.split(',').next()?;
    let output = Command::new(crate::tmux::tmux_bin().ok()?)
        .arg("-S")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    const SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    /// A private directory under the system temp dir, removed when it drops.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "cc-job-{tag}-{}-{}",
                std::process::id(),
                protocol::uid::new().unwrap()
            ));
            protocol::fsperm::private_dir(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// **Claude's pane has no options**, and its `PROGRAM` — the terminal wrapper's
    /// `/usr/bin/env` — is never read as one; an option word after `PROGRAM` is the
    /// program's argument.
    #[test]
    fn a_pane_without_options_runs_its_command_unchanged() {
        let command = words(&[
            "/usr/bin/env",
            "-u",
            "TMUX",
            "/bin/sh",
            "-c",
            "exec \"$0\" \"$@\"",
            "/usr/local/bin/claude",
            "--verify",
            "--opencode-dir",
        ]);
        let (options, rest) = parse(&command).unwrap();
        assert_eq!(options, Options::default());
        assert_eq!(rest, command.as_slice());
    }

    #[test]
    fn the_leading_options_are_read_in_either_order() {
        let command = words(&["/usr/bin/env", "/bin/sh"]);
        let expected = Options {
            opencode_dir: Some(PathBuf::from("/s/cc-1-UID")),
            verify: Some((PathBuf::from("/bin/opencode"), SHA256.to_string())),
        };
        for options in [
            words(&[
                "--opencode-dir",
                "/s/cc-1-UID",
                "--verify",
                "/bin/opencode",
                SHA256,
            ]),
            words(&[
                "--verify",
                "/bin/opencode",
                SHA256,
                "--opencode-dir",
                "/s/cc-1-UID",
            ]),
        ] {
            let mut args = options.clone();
            args.extend(command.iter().cloned());
            let (parsed, rest) = parse(&args).unwrap();
            assert_eq!(parsed, expected, "{options:?}");
            assert_eq!(rest, command.as_slice());
        }
    }

    /// An option that is repeated, short of its values, or given a relative path or
    /// a malformed digest refuses the pane rather than running anything.
    #[test]
    fn a_malformed_option_refuses_the_pane() {
        for args in [
            &["--opencode-dir"][..],
            &[
                "--opencode-dir",
                "/a",
                "--opencode-dir",
                "/b",
                "/usr/bin/env",
            ],
            &["--verify", "/bin/opencode"],
            &[
                "--verify",
                "/bin/opencode",
                SHA256,
                "--verify",
                "/bin/opencode",
                SHA256,
                "/usr/bin/env",
            ],
            &["--verify", "opencode", SHA256, "/usr/bin/env"],
            &["--verify", "/bin/opencode", "ABC", "/usr/bin/env"],
            &[
                "--verify",
                "/bin/opencode",
                &SHA256.to_uppercase(),
                "/usr/bin/env",
            ],
        ] {
            assert!(parse(&words(args)).is_err(), "{args:?}");
        }
    }

    /// **The agent's identity is published whole, owner-only, and only when it can
    /// be read.** It is the pid and the kernel's start time, in the shape the plugin
    /// reads; nothing is left under the staging name.
    #[test]
    fn the_agent_is_published_with_its_kernel_start_time() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("publish");
        let pid = std::process::id() as libc::pid_t;
        publish_agent(&scratch.0, pid);
        let path = scratch.0.join(crate::opencode::AGENT_FILE);
        let birth = protocol::proc_identity::read_birth_identity(pid).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!(
                r#"{{"pid":{pid},"start":{{"sec":{},"usec":{}}}}}"#,
                birth.start_sec, birth.start_usec
            )
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 1);
    }

    #[test]
    fn an_unreadable_agent_or_an_occupied_staging_name_publishes_nothing() {
        let scratch = Scratch::new("unpublished");
        let agent = scratch.0.join(crate::opencode::AGENT_FILE);
        // No process has this pid.
        publish_agent(&scratch.0, i32::MAX);
        assert!(!agent.exists());

        let staged = scratch
            .0
            .join(format!("{}.tmp", crate::opencode::AGENT_FILE));
        std::fs::write(&staged, "planted").unwrap();
        publish_agent(&scratch.0, std::process::id() as libc::pid_t);
        assert!(!agent.exists());
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "planted");
    }

    /// The OpenCode files go with the job; whatever else the directory holds stays.
    #[test]
    fn the_session_files_are_removed_and_nothing_else() {
        let scratch = Scratch::new("remove");
        for name in crate::opencode::SESSION_FILES
            .iter()
            .chain(&["settings.json"])
        {
            std::fs::write(scratch.0.join(name), "x").unwrap();
        }
        remove_session_files(&scratch.0);
        remove_session_files(&scratch.0);
        let left: Vec<_> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![OsString::from("settings.json")]);
    }
}
